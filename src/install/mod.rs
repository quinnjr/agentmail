//! `agentmail install` / `uninstall`: wire agentmail into a harness's own
//! config files.
//!
//! Planning reads the files and works out each one's new text
//! ([`plan_install`], [`plan_uninstall`]); nothing is written until
//! [`apply`]. Only agentmail's own entries are touched (hook commands that
//! run `agentmail hook`, the MCP server named `agentmail`), everything else
//! in a file is kept as it was written ([`json::render`]), and planning
//! twice in a row finds nothing more to do. Before a file is first changed
//! its original is saved next to it as `<file>.agentmail.bak`; a file
//! agentmail creates is marked as such by `<file>.agentmail.created`, and
//! only such a file is ever deleted again, or one holding nothing but
//! agentmail's own content: an emptied config, or agentmail's opencode
//! plugin, whose text is first saved ([`Change::save`]) when the user
//! edited it. An install into a file holding
//! none of agentmail's entries takes that record anew, so an earlier
//! install's record never outlives an uninstall.

mod claude;
pub mod json;
mod kiro;
mod opencode;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::model::Harness;

/// What to install, and where.
#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub harness: Harness,
    /// Install into the current project instead of the user's config.
    pub project: bool,
    /// Show the changes without making them.
    pub dry_run: bool,
    /// Make agentmail's agent the harness's default (kiro only). kiro-cli
    /// V3 loads agentmail's hooks file for every agent, so this changes
    /// nothing; install says so.
    pub set_default: bool,
    /// kiro only: install the Stop hook that waits for mail to wake an
    /// idle agent, and have kiro agents register with `wake` on.
    pub kiro_wake: bool,
    /// The absolute path of the `agentmail` binary the harness should run.
    pub bin: PathBuf,
}

/// One file's change: its text before (`None`: it does not exist) and
/// after (`None`: delete it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    /// The file holds none of agentmail's entries (an install): `before` is
    /// the file before agentmail, replacing any backup or marker an earlier
    /// install left, which no longer describe it.
    pub record_anew: bool,
    /// Deleting `path`, a symlink, removes only the link (and its marker),
    /// never the file it points to, even one agentmail created.
    pub unlink: bool,
    /// Before the change, `before` is written here: it is agentmail's file
    /// with edits of the user's (the opencode plugin), never discarded.
    pub save: Option<PathBuf>,
    /// Deleting `path` leaves its backup, which holds something to keep
    /// (such as an edited copy of agentmail's file).
    pub keep_backup: bool,
    /// `after` is the file's backup, which is put back in place of `path`
    /// (the opencode plugin, on uninstall): moved there, so it keeps its
    /// own permissions and is no longer left beside the file.
    pub restore: bool,
}

/// The environment the config locations depend on.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub vars: HashMap<String, String>,
}

impl Env {
    /// This process's home (`HOME`), working directory and variables.
    pub fn from_process() -> Result<Env> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
            .ok_or_else(|| anyhow!("cannot locate the home directory: set HOME"))?;
        Ok(Env {
            home,
            cwd: std::env::current_dir().context("reading the current directory")?,
            vars: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        })
    }

    /// A variable's value, unless unset or empty.
    pub fn var(&self, k: &str) -> Option<&str> {
        self.vars
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// A directory named by variable `k`, relative ones taken from `cwd`.
    pub fn dir_var(&self, k: &str) -> Option<PathBuf> {
        self.var(k).map(|v| self.cwd.join(v))
    }
}

/// The changes that install agentmail for `o.harness`.
pub fn plan_install(o: &InstallOpts, env: &Env) -> Result<Vec<Change>> {
    match o.harness {
        Harness::ClaudeCode => claude::plan(o, env, true),
        Harness::Opencode => opencode::plan(o, env, true),
        Harness::Kiro => kiro::plan(o, env, true),
        h => bail!("agentmail cannot install into {} yet", h.as_str()),
    }
}

/// The changes that remove agentmail from `o.harness`.
pub fn plan_uninstall(o: &InstallOpts, env: &Env) -> Result<Vec<Change>> {
    match o.harness {
        Harness::ClaudeCode => claude::plan(o, env, false),
        Harness::Opencode => opencode::plan(o, env, false),
        Harness::Kiro => kiro::plan(o, env, false),
        h => bail!("agentmail cannot uninstall from {} yet", h.as_str()),
    }
}

/// One-line notes for the user about installing (`install`) or
/// uninstalling agentmail for `o.harness`, from the files as they are
/// before the change: for opencode, when the agentmail plugin that runs is
/// not the one this command installs or removes; for kiro, that only its V3
/// engine is supported, and what `--kiro-wake` and `--set-default` do.
pub fn notes(o: &InstallOpts, env: &Env, install: bool) -> Result<Vec<String>> {
    match o.harness {
        Harness::Opencode => opencode::notes(o, env, install),
        Harness::Kiro => Ok(kiro::notes(o, env, install)),
        _ => Ok(Vec::new()),
    }
}

/// Whether `install --kiro-wake` is in effect for kiro agents where `env`
/// runs (see [`kiro::wake_installed`]).
pub fn kiro_wake_installed(env: &Env) -> bool {
    kiro::wake_installed(env)
}

/// Where the original of `path` is kept: `<path>.agentmail.bak`.
pub fn backup_path(path: &Path) -> PathBuf {
    with_suffix(path, ".agentmail.bak")
}

/// The marker saying agentmail created `path`: `<path>.agentmail.created`.
/// It lists the directories agentmail created for the file, one per line.
pub fn created_marker_path(path: &Path) -> PathBuf {
    with_suffix(path, ".agentmail.created")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Make `changes`. First checks that every file still has the text it was
/// planned from (else nothing is written: run the command again). Then, per
/// file: saves the original as `<path>.agentmail.bak` unless a backup is
/// already there or agentmail created the file (its marker is there: there
/// is no original), or, for a file that does not exist yet, records that
/// agentmail creates it (and the directories it creates for it) in
/// `<path>.agentmail.created`. With [`Change::record_anew`] the earlier
/// record is dropped first: the backup is replaced (or removed, for a file
/// that does not exist) and any marker removed (and, for a file that does
/// not exist, written again). Where [`Change::save`] says, `before` is
/// first saved. The file is replaced atomically (a temporary file in the
/// same directory, renamed over it), keeping its permissions; a new file is
/// private to its owner (mode 0600). A symlink is followed, even one whose
/// target does not exist yet: the file it points to changes, the link
/// stays. A file to delete (one agentmail created, emptied by an
/// uninstall, or agentmail's opencode plugin) goes with its marker and,
/// unless [`Change::keep_backup`] or [`Change::save`] keeps it, its backup
/// (a symlink agentmail did not create the target of, or one
/// [`Change::unlink`] marks: only the link and its marker), and so do the
/// directories agentmail created for it once they are empty.
pub fn apply(changes: &[Change]) -> Result<()> {
    for c in changes {
        if read_opt(&c.path)? != c.before {
            bail!(
                "{} changed while agentmail was planning its edit; nothing was written, run the command again",
                c.path.display()
            );
        }
    }
    let mut dirs = Vec::new();
    for c in changes {
        dirs.extend(apply_one(c).with_context(|| format!("writing {}", c.path.display()))?);
    }
    remove_empty_dirs(dirs)
}

/// A file's text, or `None` when it does not exist.
fn read_opt(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b)
            .map(Some)
            .map_err(|_| anyhow!("{} is not UTF-8 text", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

/// The file `path` names once symlinks are followed, whether or not it
/// exists: a dangling link resolves to the file it would point to.
fn resolve(path: &Path) -> Result<PathBuf> {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                let t = std::fs::read_link(&p)?;
                p = match p.parent() {
                    Some(dir) => dir.join(t),
                    None => t,
                };
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(p),
        }
    }
    bail!("too many levels of symbolic links at {}", path.display())
}

/// Remove `path`, which may already be gone.
fn remove_if_there(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(anyhow::Error::new(e).context(format!("removing {}", path.display())))
        }
        _ => Ok(()),
    }
}

/// Remove the directories in `dirs` that are empty, deepest first.
fn remove_empty_dirs(mut dirs: Vec<PathBuf>) -> Result<()> {
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for d in dirs {
        match std::fs::remove_dir(&d) {
            Err(e)
                if !matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                return Err(anyhow::Error::new(e).context(format!("removing {}", d.display())));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Make one change; returns the directories to remove when they are empty.
fn apply_one(c: &Change) -> Result<Vec<PathBuf>> {
    let perms = std::fs::metadata(&c.path).ok().map(|m| m.permissions());
    let bak = backup_path(&c.path);
    let marker = created_marker_path(&c.path);
    // Write through a symlink to its target, so the link stays a link.
    let target = resolve(&c.path)?;
    // The backup becomes the file again: renamed into place, it keeps its
    // permissions and is used up, before any edited text is saved (perhaps
    // as that backup). Never through a link (none is planned): the link
    // itself would be replaced.
    if c.restore {
        std::fs::rename(&bak, &c.path)
            .with_context(|| format!("restoring {} from {}", c.path.display(), bak.display()))?;
        remove_if_there(&marker)?;
    }
    if let (Some(save), Some(before)) = (&c.save, &c.before) {
        replace_file(save, perms.as_ref(), before.as_bytes())
            .with_context(|| format!("writing {}", save.display()))?;
    }
    if c.restore {
        return Ok(Vec::new());
    }
    let Some(after) = &c.after else {
        // Only agentmail's own file is deleted ([`JsonFile::change`], the
        // opencode plugin): any backup of it holds nothing but agentmail's
        // entries, unless [`Change::keep_backup`] says it holds more (and
        // [`Change::save`] has already put the file's edited text away). A
        // file agentmail created (its marker says so) goes even behind a
        // symlink, which stays, unless [`Change::unlink`] says
        // only the link goes. Otherwise of a symlink only the link goes,
        // never the file it points to, and a backup beside it, which may
        // hold that file's original (an older install wrote through
        // links), stays.
        let (dirs, created) = match std::fs::read_to_string(&marker) {
            Ok(text) => (text.lines().map(PathBuf::from).collect(), true),
            Err(_) => (Vec::new(), false),
        };
        let link = std::fs::symlink_metadata(&c.path).is_ok_and(|m| m.file_type().is_symlink());
        std::fs::remove_file(if created && !(link && c.unlink) {
            &target
        } else {
            &c.path
        })?;
        if (created || !link) && !c.keep_backup && c.save.as_ref() != Some(&bak) {
            remove_if_there(&bak)?;
        }
        remove_if_there(&marker)?;
        return Ok(dirs);
    };
    if c.record_anew {
        match &c.before {
            Some(before) => {
                replace_file(&bak, perms.as_ref(), before.as_bytes())
                    .with_context(|| format!("writing {}", bak.display()))?;
                remove_if_there(&marker)?;
            }
            // Written again below, listing the directories this install
            // creates.
            None => {
                remove_if_there(&bak)?;
                remove_if_there(&marker)?;
            }
        }
    } else if let Some(before) = &c.before
        // A file agentmail created had no original to keep: a backup of
        // agentmail's own earlier text would only be left behind.
        && std::fs::symlink_metadata(&marker).is_err()
    {
        write_backup(&bak, perms.as_ref(), |f| f.write_all(before.as_bytes()))
            .with_context(|| format!("writing {}", bak.display()))?;
    }
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    // The directories this creates, deepest first.
    let new_dirs: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| !d.as_os_str().is_empty() && std::fs::symlink_metadata(d).is_err())
        .collect();
    std::fs::create_dir_all(dir)?;
    if c.before.is_none() && std::fs::symlink_metadata(&bak).is_err() {
        // Recorded before the file is created: without it an uninstall
        // keeps the file (as `{}`) rather than delete what it cannot tell
        // agentmail created.
        let list: String = new_dirs
            .iter()
            .filter_map(|d| d.to_str().filter(|s| !s.contains('\n')))
            .map(|s| format!("{s}\n"))
            .collect();
        write_backup(&marker, None, |f| f.write_all(list.as_bytes()))
            .with_context(|| format!("writing {}", marker.display()))?;
    }
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", target.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.agentmail-{}.tmp", std::process::id()));
    let write = || -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        match &perms {
            Some(p) => f.set_permissions(p.clone())?,
            None => set_private(&f)?,
        }
        f.write_all(after.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(Vec::new())
}

/// Make a new file readable and writable by its owner only: config files
/// such as `.claude.json` hold account data.
fn set_private(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        Ok(())
    }
}

/// Replace `path` with `text` atomically (a temporary file next to it,
/// renamed over it), with permissions `perms` when given.
fn replace_file(path: &Path, perms: Option<&std::fs::Permissions>, text: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        if let Some(p) = perms {
            f.set_permissions(p.clone())?;
        }
        f.write_all(text)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Save a backup at `bak` unless one is there already, never leaving a
/// partial one behind: `fill` writes a temporary file next to it, which is
/// then linked into place (which fails, harmlessly, when `bak` exists).
/// Where hard links are not supported the file is created in place and
/// removed again when filling it fails.
fn write_backup(
    bak: &Path,
    perms: Option<&std::fs::Permissions>,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<()> {
    if std::fs::symlink_metadata(bak).is_ok() {
        return Ok(());
    }
    let dir = bak
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", bak.display()))?;
    let name = bak
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", bak.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let filled = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        if let Some(p) = perms {
            f.set_permissions(p.clone())?;
        }
        fill(&mut f)?;
        f.sync_all()
    })();
    let saved = filled.and_then(|()| match std::fs::hard_link(&tmp, bak) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => copy_new(&tmp, bak),
        r => r,
    });
    let _ = std::fs::remove_file(&tmp);
    match saved {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        r => Ok(r?),
    }
}

/// Copy `from` to a new file `to` (refused when `to` exists), removing
/// `to` again when the copy fails.
fn copy_new(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let copied = (|| {
        f.set_permissions(std::fs::metadata(from)?.permissions())?;
        std::io::copy(&mut std::fs::File::open(from)?, &mut f)?;
        f.sync_all()
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(to);
    }
    copied
}

/// What to do by hand with a config agentmail cannot read.
enum ByHand<'a> {
    /// Install: the entry to add (a snippet), made only when needed.
    Add(&'a dyn Fn() -> String),
    /// Uninstall: where agentmail's entry is, to remove.
    Remove(&'a str),
}

impl ByHand<'_> {
    /// The advice, for a file that cannot be parsed (`unparsable`) or is
    /// not a JSON object.
    fn advice(&self, unparsable: bool) -> String {
        match self {
            ByHand::Add(entry) if unparsable => {
                format!("Fix it, or add this by hand:\n{}", entry())
            }
            ByHand::Add(entry) => format!("Add this by hand:\n{}", entry()),
            ByHand::Remove(entry) => format!(
                "Fix it, or remove agentmail's entry ({entry}) by hand, then run the uninstall again."
            ),
        }
    }
}

/// A JSON config file as read for planning.
///
/// Later install tasks build on it: [`JsonFile::read`] the file, edit a
/// copy of [`JsonFile::value`] (only agentmail's entries), then
/// [`JsonFile::change`] with a `strip` that removes agentmail's entries.
struct JsonFile {
    path: PathBuf,
    before: Option<String>,
    value: Map<String, Value>,
    /// The file before agentmail first changed it, as far as known: its
    /// backup when there is one, else the file as it is now; `None` when
    /// agentmail created it (its marker is there) or it does not exist.
    original: Option<Map<String, Value>>,
}

impl JsonFile {
    /// Read `path`; a missing or blank file is an empty object. Text that is
    /// not a JSON object is refused with `hand`'s advice: the entry to add
    /// by hand on install, the one to remove on uninstall.
    fn read(path: PathBuf, hand: ByHand) -> Result<JsonFile> {
        let before = read_opt(&path)?;
        let value = match json::parse(before.as_deref().unwrap_or("")) {
            Ok(Value::Object(m)) => m,
            Ok(_) => bail!(
                "{} is not a JSON object; agentmail left it unchanged. {}",
                path.display(),
                hand.advice(false)
            ),
            Err(e) => bail!(
                "cannot parse {}: {e}; agentmail left it unchanged. {}",
                path.display(),
                hand.advice(true)
            ),
        };
        // Only the marker says agentmail created the file. A backup, even
        // one that cannot be read as an object, shows it existed before.
        let original = if std::fs::symlink_metadata(created_marker_path(&path)).is_ok() {
            None
        } else {
            match read_opt(&backup_path(&path))? {
                Some(text) => Some(match json::parse(&text) {
                    Ok(Value::Object(m)) => m,
                    _ => Map::new(),
                }),
                None => before.is_some().then(|| value.clone()),
            }
        };
        Ok(JsonFile {
            path,
            before,
            value,
            original,
        })
    }

    /// The change to `new`, if any. `strip` removes agentmail's entries
    /// from an object (and nothing else); with it the file as it was
    /// before agentmail is worked out, so that containers agentmail's
    /// entries emptied are removed only when they were not there before.
    /// A file emptied by an uninstall is deleted only when agentmail
    /// created it (its marker says so); else it is left as `{}`.
    fn change(
        self,
        mut new: Map<String, Value>,
        uninstall: bool,
        strip: &dyn Fn(&mut Map<String, Value>),
    ) -> Option<Change> {
        let original = self.original.as_ref().map(|raw| {
            let mut o = raw.clone();
            strip(&mut o);
            prune_emptied(raw, &mut o, None);
            o
        });
        prune_emptied(&self.value, &mut new, original.as_ref());
        if new == self.value {
            return None;
        }
        // Without agentmail's entries the file is as it was before
        // agentmail: an older backup or marker no longer describes it.
        let record_anew = !uninstall && {
            let mut s = self.value.clone();
            strip(&mut s);
            s == self.value
        };
        let after = if uninstall && new.is_empty() && original.is_none() {
            None
        } else {
            Some(json::render(self.before.as_deref(), &Value::Object(new)))
        };
        Some(Change {
            path: self.path,
            before: self.before,
            after,
            record_anew,
            unlink: false,
            save: None,
            keep_backup: false,
            restore: false,
        })
    }
}

/// Remove from `after` the containers (objects and arrays, at any depth of
/// objects) that are empty there but held something in `before`, unless
/// `orig` has them: agentmail emptied them and they were not there before
/// it.
fn prune_emptied(
    before: &Map<String, Value>,
    after: &mut Map<String, Value>,
    orig: Option<&Map<String, Value>>,
) {
    let is_empty = |v: &Value| match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    };
    after.retain(|k, v| {
        let Some(b) = before.get(k) else {
            return true;
        };
        let o = orig.and_then(|o| o.get(k));
        if let (Value::Object(b), Value::Object(a)) = (b, &mut *v) {
            prune_emptied(b, a, o.and_then(Value::as_object));
        }
        !(is_empty(v) && !is_empty(b) && o.is_none())
    });
}

/// `p` quoted for `sh` when it needs to be.
pub fn shell_quote(p: &str) -> String {
    let plain = !p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%,=".contains(&b));
    if plain {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', r"'\''"))
    }
}

/// The first `n` words of a shell command line, unquoted (single and
/// double quotes, backslash escapes); enough to recognise a command.
fn shell_words(cmd: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(w) = cur.take() {
                    words.push(w);
                    if words.len() == n {
                        return words;
                    }
                }
            }
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    w.push(c);
                }
            }
            '"' => {
                let w = cur.get_or_insert_with(String::new);
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => w.extend(chars.next()),
                        c => w.push(c),
                    }
                }
            }
            '\\' => cur.get_or_insert_with(String::new).extend(chars.next()),
            c => cur.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(cur);
    words
}

/// Whether a hook command is agentmail's: it runs a program named
/// `agentmail…` with `hook` as its first argument.
pub fn is_agentmail_hook(cmd: &str) -> bool {
    match shell_words(cmd, 2).as_slice() {
        [prog, sub] => {
            sub == "hook"
                && Path::new(prog)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agentmail"))
        }
        _ => false,
    }
}

/// Whether a hook command is agentmail's when the binary is `bin`: as
/// [`is_agentmail_hook`], or a program at `bin` itself (the same path, or
/// the same file once resolved) run with `hook`, whatever its file name.
pub fn owns_hook(cmd: &str, bin: &Path) -> bool {
    if is_agentmail_hook(cmd) {
        return true;
    }
    match shell_words(cmd, 2).as_slice() {
        [prog, sub] if sub == "hook" => {
            let prog = Path::new(prog);
            prog == bin
                || matches!(
                    (std::fs::canonicalize(prog), std::fs::canonicalize(bin)),
                    (Ok(a), Ok(b)) if a == b
                )
        }
        _ => false,
    }
}

/// `bin` as text, for config files (which are JSON).
fn bin_str(o: &InstallOpts) -> Result<&str> {
    if !o.bin.is_absolute() {
        bail!(
            "the agentmail binary path {} is not absolute",
            o.bin.display()
        );
    }
    o.bin
        .to_str()
        .ok_or_else(|| anyhow!("the agentmail binary path {} is not UTF-8", o.bin.display()))
}

/// The object at `root[key]`, created when missing; an error when it is
/// something else.
fn object_at<'a>(
    root: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    root.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow!(
                "`{key}` in {} is not a JSON object; agentmail left the file unchanged",
                path.display()
            )
        })
}

/// Set the `keys` of `cur` to their values in `want`, removing those
/// `want` lacks, and keep every other key.
fn merge_keys(cur: &mut Map<String, Value>, want: &Map<String, Value>, keys: &[&str]) {
    for k in keys {
        match want.get(*k) {
            Some(v) if cur.get(*k) != Some(v) => {
                cur.insert(k.to_string(), v.clone());
            }
            Some(_) => {}
            None => {
                cur.shift_remove(*k);
            }
        }
    }
}

/// A unified diff of one change, three lines of context, for `--dry-run`.
pub fn diff(c: &Change) -> String {
    let name = c.path.display().to_string();
    let a = c.before.as_deref().map(lines).unwrap_or_default();
    let b = c.after.as_deref().map(lines).unwrap_or_default();
    let mut out = format!(
        "--- {}\n+++ {}\n",
        if c.before.is_some() {
            &name
        } else {
            "/dev/null"
        },
        if c.after.is_some() {
            &name
        } else {
            "/dev/null"
        },
    );
    let ops = diff_ops(&a, &b);
    const CTX: usize = 3;
    // Positions (in `ops`) of changed lines, grouped into hunks.
    let changed: Vec<usize> = (0..ops.len()).filter(|&i| ops[i] != Op::Same).collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        match groups.last_mut() {
            Some(g) if i <= g.1 + 2 * CTX + 1 => g.1 = i,
            _ => groups.push((i, i)),
        }
    }
    // Line numbers before each op.
    let mut pos = Vec::with_capacity(ops.len() + 1);
    let (mut ai, mut bi) = (0, 0);
    for op in &ops {
        pos.push((ai, bi));
        match op {
            Op::Same => (ai, bi) = (ai + 1, bi + 1),
            Op::Del => ai += 1,
            Op::Ins => bi += 1,
        }
    }
    pos.push((ai, bi));
    for (first, last) in groups {
        let start = first.saturating_sub(CTX);
        let end = (last + CTX + 1).min(ops.len());
        let (a0, b0) = pos[start];
        let (a1, b1) = pos[end];
        let range = |s: usize, n: usize| {
            if n == 0 {
                format!("{s},0")
            } else {
                format!("{},{n}", s + 1)
            }
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a0, a1 - a0),
            range(b0, b1 - b0)
        ));
        for (i, op) in ops.iter().enumerate().take(end).skip(start) {
            let (ai, bi) = pos[i];
            let (mark, line) = match op {
                Op::Same => (' ', a[ai]),
                Op::Del => ('-', a[ai]),
                Op::Ins => ('+', b[bi]),
            };
            out.push(mark);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Del,
    Ins,
}

/// A line diff: common head and tail, then a longest common subsequence
/// of the rest (or all of it replaced, when that would be too big).
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (ma, mb) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let mut ops = vec![Op::Same; head];
    let (n, m) = (ma.len(), mb.len());
    if n * m > 4_000_000 {
        ops.extend(std::iter::repeat_n(Op::Del, n));
        ops.extend(std::iter::repeat_n(Op::Ins, m));
    } else {
        // lcs[i][j]: LCS length of ma[i..] and mb[j..].
        let w = m + 1;
        let mut lcs = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * w + j] = if ma[i] == mb[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && ma[i] == mb[j] {
                ops.push(Op::Same);
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
                ops.push(Op::Del);
                i += 1;
            } else {
                ops.push(Op::Ins);
                j += 1;
            }
        }
    }
    ops.extend(std::iter::repeat_n(Op::Same, tail));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_recognising_commands() {
        assert_eq!(shell_quote("/usr/bin/agentmail"), "/usr/bin/agentmail");
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
        for cmd in [
            "/usr/bin/agentmail hook stop --harness claude-code",
            "agentmail hook wait",
            r"'/a b/it'\''s/agentmail' hook prompt",
            "\"/a b/agentmail\" hook x",
            "/x/agentmail-dev hook x",
        ] {
            assert!(is_agentmail_hook(cmd), "{cmd}");
        }
        for cmd in [
            "agentmail send --to x",
            "echo agentmail hook",
            "/x/notagentmail hook",
            "agentmail",
            "",
        ] {
            assert!(!is_agentmail_hook(cmd), "{cmd}");
        }
    }

    #[test]
    fn owning_hooks_by_binary_path() {
        let bin = Path::new("/x/am");
        assert!(owns_hook("/x/am hook stop --harness claude-code", bin));
        assert!(owns_hook("'/x/am' hook wait", bin));
        assert!(owns_hook("/usr/bin/agentmail hook stop", bin));
        assert!(!owns_hook("/x/am send", bin));
        assert!(!owns_hook("/y/am hook stop", bin));
    }

    #[test]
    fn failed_backup_leaves_nothing_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let bak = dir.path().join("settings.json.agentmail.bak");
        let err = write_backup(&bak, None, |f| {
            f.write_all(b"{\"partial")?;
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        // A later backup is then written in full, and never replaced.
        write_backup(&bak, None, |f| f.write_all(b"{}")).unwrap();
        write_backup(&bak, None, |f| f.write_all(b"other")).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "{}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn diff_hunks() {
        let before: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let after = before.replace("10\n", "ten\n").replace("20\n", "20\n21");
        let d = diff(&Change {
            path: "/f".into(),
            before: Some(before),
            after: Some(after),
            record_anew: false,
            unlink: false,
            save: None,
            keep_backup: false,
            restore: false,
        });
        assert_eq!(
            d,
            "--- /f\n+++ /f\n@@ -7,7 +7,7 @@\n 7\n 8\n 9\n-10\n+ten\n 11\n 12\n 13\n\
             @@ -18,3 +18,4 @@\n 18\n 19\n 20\n+21\n\\ No newline at end of file\n"
        );
        let d = diff(&Change {
            path: "/f".into(),
            before: None,
            after: Some("a\n".into()),
            record_anew: false,
            unlink: false,
            save: None,
            keep_backup: false,
            restore: false,
        });
        assert_eq!(d, "--- /dev/null\n+++ /f\n@@ -0,0 +1,1 @@\n+a\n");
    }
}
