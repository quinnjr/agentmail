//! `agentmail install` / `uninstall`: wire agentmail into a harness's own
//! config files.
//!
//! Planning reads the files and works out each one's new text
//! ([`plan_install`], [`plan_uninstall`]); nothing is written until
//! [`apply`]. Only agentmail's own entries are touched (hook commands that
//! run `agentmail hook`, the MCP server named `agentmail`), everything else
//! in a file is kept as it was written ([`json::render`]), and planning
//! twice in a row finds nothing more to do. Before a file is first changed
//! its original is saved next to it as `<file>.agentmail.bak`.

mod claude;
pub mod json;

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
    /// Make agentmail's agent the harness's default (kiro only).
    pub set_default: bool,
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
        h => bail!("agentmail cannot install into {} yet", h.as_str()),
    }
}

/// The changes that remove agentmail from `o.harness`.
pub fn plan_uninstall(o: &InstallOpts, env: &Env) -> Result<Vec<Change>> {
    match o.harness {
        Harness::ClaudeCode => claude::plan(o, env, false),
        h => bail!("agentmail cannot uninstall from {} yet", h.as_str()),
    }
}

/// Where the original of `path` is kept: `<path>.agentmail.bak`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".agentmail.bak");
    PathBuf::from(s)
}

/// Make `changes`. First checks that every file still has the text it was
/// planned from (else nothing is written: run the command again). Then, per
/// file: saves the original as `<path>.agentmail.bak` unless a backup is
/// already there, and replaces the file atomically (a temporary file in the
/// same directory, renamed over it), keeping its permissions. A symlink is
/// followed: the file it points to changes, the link stays.
pub fn apply(changes: &[Change]) -> Result<()> {
    for c in changes {
        if read_opt(&c.path)? != c.before {
            bail!(
                "{} changed while agentmail was planning its edit; nothing was written, run the command again",
                c.path.display()
            );
        }
    }
    for c in changes {
        apply_one(c).with_context(|| format!("writing {}", c.path.display()))?;
    }
    Ok(())
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

fn apply_one(c: &Change) -> Result<()> {
    let perms = std::fs::metadata(&c.path).ok().map(|m| m.permissions());
    if let Some(before) = &c.before {
        let bak = backup_path(&c.path);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&bak)
        {
            Ok(mut f) => {
                if let Some(p) = &perms {
                    f.set_permissions(p.clone())?;
                }
                f.write_all(before.as_bytes())?;
                f.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("writing {}", bak.display())));
            }
        }
    }
    let Some(after) = &c.after else {
        return Ok(std::fs::remove_file(&c.path)?);
    };
    // Write through a symlink to its target, so the link stays a link.
    let target = if c.before.is_some() {
        std::fs::canonicalize(&c.path)?
    } else {
        c.path.clone()
    };
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", target.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.agentmail-{}.tmp", std::process::id()));
    let write = || -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        if let Some(p) = &perms {
            f.set_permissions(p.clone())?;
        }
        f.write_all(after.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A JSON config file as read for planning.
struct JsonFile {
    path: PathBuf,
    before: Option<String>,
    value: Map<String, Value>,
}

impl JsonFile {
    /// Read `path`; a missing or blank file is an empty object. Text that is
    /// not a JSON object is refused, naming `entry` (what agentmail would
    /// add) so the user can add it by hand.
    fn read(path: PathBuf, entry: &dyn Fn() -> String) -> Result<JsonFile> {
        let before = read_opt(&path)?;
        let value = match json::parse(before.as_deref().unwrap_or("")) {
            Ok(Value::Object(m)) => m,
            Ok(_) => bail!(
                "{} is not a JSON object; agentmail left it unchanged. Add this by hand:\n{}",
                path.display(),
                entry()
            ),
            Err(e) => bail!(
                "cannot parse {}: {e}; agentmail left it unchanged. Fix it, or add this by hand:\n{}",
                path.display(),
                entry()
            ),
        };
        Ok(JsonFile {
            path,
            before,
            value,
        })
    }

    /// The change to `new`, if any. An object left empty by an uninstall
    /// held only agentmail's entries: the file is removed.
    fn change(self, new: Map<String, Value>, uninstall: bool) -> Option<Change> {
        if new == self.value {
            return None;
        }
        let after = if uninstall && new.is_empty() {
            None
        } else {
            Some(json::render(self.before.as_deref(), &Value::Object(new)))
        };
        Some(Change {
            path: self.path,
            before: self.before,
            after,
        })
    }
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
    fn diff_hunks() {
        let before: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let after = before.replace("10\n", "ten\n").replace("20\n", "20\n21");
        let d = diff(&Change {
            path: "/f".into(),
            before: Some(before),
            after: Some(after),
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
        });
        assert_eq!(d, "--- /dev/null\n+++ /f\n@@ -0,0 +1,1 @@\n+a\n");
    }
}
