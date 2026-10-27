//! opencode: the MCP server in `opencode.json`, the plugin in `plugins/`.
//!
//! User scope: the config directory is `$XDG_CONFIG_HOME/opencode`, else
//! `~/.config/opencode`; project scope (`--project`) is the current
//! directory, with the plugin in `.opencode/plugins/`. opencode loads and
//! merges both `opencode.json` and `opencode.jsonc` there (the `.jsonc`
//! last), and at project scope also `.opencode/opencode.json(c)` after
//! them, so install updates agentmail's entry in each that has one, else
//! adds it to the first of the direct two that exists (a new
//! `opencode.json` when neither does), and uninstall removes it from
//! all. The MCP server goes in `mcp.servers.agentmail`, also in a file
//! that keeps its other servers in the V1 shape (`mcp.<name>`): opencode
//! 2.0.27 reads both, but drops `codemode` from a V1 entry, and its own
//! `opencode mcp add` writes `servers` into such a file too (see
//! `docs/harness-notes.md`). A V1 entry named `agentmail` is agentmail's:
//! install moves it to `servers`, uninstall removes it. A config with
//! JSONC syntax (comments, trailing commas) is never rewritten (it would
//! be lost): install and uninstall refuse with the entry to add or remove
//! by hand, unless the file is already as they would leave it. The plugin
//! is `plugin/agentmail.js`, embedded, with the binary's path in place of
//! its placeholder; a `plugins/agentmail.js` that is not agentmail's, or
//! is a symlink, is never replaced, and of a symlink uninstall removes
//! only the link, even when agentmail created the file it replaced. A
//! plugin the user edited (one without the digest line that ends every
//! plugin agentmail writes) is saved as its backup (or beside it) before it
//! is replaced or removed, whether or not agentmail created it.
//!
//! opencode loads the user scope's `plugin/` and `plugins/`, then the
//! project's, but runs only the first plugin of each `id`: an agentmail
//! plugin left in `plugin/` would shadow the installed one, which would
//! never run, so install and uninstall alike remove it. Likewise the user
//! scope's plugin shadows the project's; [`notes`] says when the one that
//! runs is for another binary (see `docs/harness-notes.md`).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::{
    ByHand, Change, Env, InstallOpts, JsonFile, backup_path, bin_str, created_marker_path,
    merge_keys, object_at, read_opt,
};

/// The plugin as shipped.
const PLUGIN: &str = include_str!("../../plugin/agentmail.js");
/// The plugin's binary path, a JS string literal, as shipped.
const PLACEHOLDER: &str = "\"__AGENTMAIL_BIN__\"";
/// The plugin's first line, which marks a file as agentmail's plugin.
const PLUGIN_HEAD: &str = "// agentmail for opencode";
/// The start of the plugin's last line as installed, which signs the text
/// above it (see [`sign`]).
const DIGEST_PREFIX: &str = "// agentmail-digest: fnv1a64:";

/// The keys of the MCP server entry that agentmail sets; others (such as
/// `environment` or `disabled`) are kept.
const SERVER_KEYS: [&str; 3] = ["type", "command", "codemode"];
/// The keys a V1 entry shares with a V2 one, carried over when agentmail's
/// V1 entry is moved to `servers`.
const SHARED_KEYS: [&str; 4] = ["environment", "cwd", "timeout", "protocol"];

/// The files: (direct configs, overriding configs, plugin). The direct
/// configs are those of `opencode.json` and `opencode.jsonc` that exist,
/// in that order, else a new `opencode.json` alone. At project scope the
/// overriding ones are those of `.opencode/opencode.json` and
/// `.opencode/opencode.jsonc` that exist: opencode merges them after the
/// direct ones, so an agentmail entry there wins.
fn files(o: &InstallOpts, env: &Env) -> (Vec<PathBuf>, Vec<PathBuf>, PathBuf) {
    let existing = |dir: &Path| -> Vec<PathBuf> {
        ["opencode.json", "opencode.jsonc"]
            .iter()
            .map(|n| dir.join(n))
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect()
    };
    let (dir, overrides) = if o.project {
        (env.cwd.clone(), existing(&env.cwd.join(".opencode")))
    } else {
        (user_dir(env), Vec::new())
    };
    let mut configs = existing(&dir);
    if configs.is_empty() {
        configs.push(dir.join("opencode.json"));
    }
    (
        configs,
        overrides,
        plugin_dir(o.project, env).join("agentmail.js"),
    )
}

/// The user scope's config directory.
fn user_dir(env: &Env) -> PathBuf {
    match env.dir_var("XDG_CONFIG_HOME") {
        Some(d) => d.join("opencode"),
        None => env.home.join(".config").join("opencode"),
    }
}

/// The directory a scope's plugin is installed in (`plugins/`).
fn plugin_dir(project: bool, env: &Env) -> PathBuf {
    if project {
        env.cwd.join(".opencode").join("plugins")
    } else {
        user_dir(env).join("plugins")
    }
}

/// The agentmail plugin opencode would run from a scope: the first that is
/// agentmail's of `plugin/agentmail.js` and `plugins/agentmail.js`, the
/// order opencode loads them in, with the binary it runs (`None`: not
/// readable from the file). A path that cannot be read (a directory, a
/// file that is not UTF-8) is no agentmail plugin: this only feeds the
/// advisory [`notes`], which never stop a command.
fn scope_plugin(project: bool, env: &Env) -> Option<(PathBuf, Option<String>)> {
    let plugins = plugin_dir(project, env);
    for dir in [plugins.with_file_name("plugin"), plugins] {
        let path = dir.join("agentmail.js");
        if let Ok(Some(text)) = read_opt(&path)
            && is_plugin(&text)
        {
            return Some((path, plugin_bin(&text)));
        }
    }
    None
}

/// The binary an agentmail plugin's text runs: the string literal where
/// the shipped plugin has its placeholder.
fn plugin_bin(text: &str) -> Option<String> {
    let line = PLUGIN.lines().find(|l| l.contains(PLACEHOLDER))?;
    let prefix = &line[..line.find(PLACEHOLDER)?];
    let rest = text.lines().find_map(|l| l.strip_prefix(prefix))?;
    serde_json::Deserializer::from_str(rest)
        .into_iter::<String>()
        .next()?
        .ok()
}

/// What to tell the user about the other scope's agentmail plugin: opencode
/// runs only one, the user scope's when both are there. Install notes
/// when the one that runs (in the current project) is for another binary
/// than `o.bin`; uninstall at user scope, when it hands over to the
/// project's.
pub(super) fn notes(o: &InstallOpts, env: &Env, install: bool) -> Result<Vec<String>> {
    let bin = bin_str(o)?;
    let Some((other, other_bin)) = scope_plugin(!o.project, env) else {
        return Ok(Vec::new());
    };
    let runs = match &other_bin {
        Some(b) => format!("runs {b}"),
        None => "runs a binary agentmail cannot read from it".to_string(),
    };
    let other = other.display();
    let note = match (install, o.project) {
        _ if install && other_bin.as_deref() == Some(bin) => None,
        (true, true) => Some(format!(
            "note: opencode runs only one agentmail plugin, the user scope's when both are there: \
             in this project the user-scope {other} ({runs}) runs, not the one installed here \
             for {bin}."
        )),
        (true, false) => Some(format!(
            "note: opencode runs only one agentmail plugin, the user scope's when both are there: \
             in this project the one installed now for {bin} runs, not the project's {other} \
             ({runs})."
        )),
        (false, false) if scope_plugin(false, env).is_some() => Some(format!(
            "note: in this project opencode now runs the project's agentmail plugin {other} \
             ({runs})."
        )),
        (false, _) => None,
    };
    Ok(note.into_iter().collect())
}

fn server(bin: &str) -> Value {
    json!({"type": "local", "command": [bin, "mcp"], "codemode": false})
}

/// The plugin with `bin` in place of its placeholder, signed ([`sign`]).
fn plugin_text(bin: &str) -> Result<String> {
    if PLUGIN.matches(PLACEHOLDER).count() != 1 {
        bail!("the embedded opencode plugin has no single binary placeholder");
    }
    Ok(sign(PLUGIN.replacen(
        PLACEHOLDER,
        &serde_json::to_string(bin)?,
        1,
    )))
}

/// `body` with a last line holding its digest: whatever text a version of
/// agentmail installs, the file says itself that agentmail wrote it as it
/// is, so a plugin an older agentmail installed is told from one the user
/// edited ([`pristine`]) without a list of every plugin ever shipped. The
/// digest (FNV-1a, 64 bits, of the text above the line) only tells edits
/// apart; it must never change, or every plugin installed before would
/// count as edited.
fn sign(mut body: String) -> String {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let digest = fnv1a64(body.as_bytes());
    body.push_str(&format!("{DIGEST_PREFIX}{digest:016x}\n"));
    body
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn is_plugin(text: &str) -> bool {
    text.starts_with(PLUGIN_HEAD)
}

/// Remove agentmail's server entries (V2 and V1), for [`JsonFile::change`].
fn strip(v: &mut Map<String, Value>) {
    if let Some(Value::Object(mcp)) = v.get_mut("mcp") {
        mcp.shift_remove("agentmail");
        if let Some(Value::Object(servers)) = mcp.get_mut("servers") {
            servers.shift_remove("agentmail");
        }
    }
}

/// Make `v`'s agentmail entry the one for `bin` (install), or remove it.
fn edit(v: &mut Map<String, Value>, install: bool, bin: &str, path: &Path) -> Result<()> {
    if !install {
        strip(v);
        return Ok(());
    }
    let mcp = object_at(v, "mcp", path)?;
    let legacy = mcp.shift_remove("agentmail");
    let servers = object_at(mcp, "servers", path)?;
    let want = server(bin);
    let want = want.as_object().expect("an object");
    let entry = match servers.get_mut("agentmail") {
        // Keep what the user added to the entry, such as `environment`.
        Some(Value::Object(cur)) => {
            merge_keys(cur, want, &SERVER_KEYS);
            cur
        }
        _ => {
            servers.insert("agentmail".into(), Value::Object(want.clone()));
            servers["agentmail"].as_object_mut().expect("an object")
        }
    };
    if let Some(Value::Object(old)) = legacy {
        carry(entry, &old);
    }
    Ok(())
}

/// Carry over to `entry` what agentmail's V1 entry `old` set and `entry`
/// lacks: the keys both shapes share, and `enabled: false` as `disabled:
/// true`.
fn carry(entry: &mut Map<String, Value>, old: &Map<String, Value>) {
    for k in SHARED_KEYS {
        if let Some(x) = old.get(k)
            && !entry.contains_key(k)
        {
            entry.insert(k.to_string(), x.clone());
        }
    }
    if old.get("enabled") == Some(&Value::Bool(false)) && !entry.contains_key("disabled") {
        entry.insert("disabled".into(), Value::Bool(true));
    }
}

/// Whether `v` has an agentmail entry (V2 or V1).
fn has_entry(v: &Map<String, Value>) -> bool {
    let mut s = v.clone();
    strip(&mut s);
    s != *v
}

/// The config at `path` parsed as opencode reads it (JSONC), and whether
/// it has JSONC syntax; `None` when it does not exist.
fn load(path: &Path, install: bool, bin: &str) -> Result<Option<(Map<String, Value>, bool)>> {
    let Some(text) = read_opt(path)? else {
        return Ok(None);
    };
    let bare = super::json::strip_jsonc(&text);
    let entry = || snippet(bin);
    let hand = by_hand(install, &entry);
    match super::json::parse(bare.as_deref().unwrap_or(&text)) {
        Ok(Value::Object(m)) => Ok(Some((m, bare.is_some()))),
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
    }
}

/// What to do by hand with a config that cannot be read: add `entry` on
/// install, remove agentmail's entries on uninstall.
fn by_hand<'a>(install: bool, entry: &'a dyn Fn() -> String) -> ByHand<'a> {
    if install {
        ByHand::Add(entry)
    } else {
        ByHand::Remove("`mcp.servers.agentmail` / `mcp.agentmail`")
    }
}

/// The snippet to add by hand.
fn snippet(bin: &str) -> String {
    super::json::pretty(
        &json!({"mcp": {"servers": {"agentmail": server(bin)}}}),
        "  ",
        "",
    )
}

/// The config's change, if any.
fn plan_config(path: PathBuf, install: bool, bin: &str) -> Result<Option<Change>> {
    if let Some((value, true)) = load(&path, install, bin)? {
        // JSONC: check whether the file is already as it should be; never
        // write it.
        let mut new = value.clone();
        edit(&mut new, install, bin, &path)?;
        if new == value {
            return Ok(None);
        }
        let jsonc = "uses JSONC syntax (comments or trailing commas), which agentmail cannot keep \
                     when it edits the file; nothing was changed";
        if install {
            let v1 = match value.get("mcp") {
                Some(Value::Object(mcp)) if mcp.contains_key("agentmail") => {
                    " Also delete the \"agentmail\" entry directly under \"mcp\" (an old shape), \
                     first copying any \"environment\", \"cwd\", \"timeout\" or \"protocol\" \
                     it has into the new entry."
                }
                _ => "",
            };
            bail!(
                "{} {jsonc}. Add this by hand, merging it into the \"mcp\" there in place of any \
                 \"agentmail\" entry in \"mcp\" → \"servers\".{v1} Then run the install again \
                 to add the plugin:\n{}",
                path.display(),
                snippet(bin)
            );
        }
        bail!(
            "{} {jsonc}. Remove the \"agentmail\" entry from \"mcp\" → \"servers\" (and any \
             \"agentmail\" directly under \"mcp\") by hand, then run the uninstall again to \
             remove the plugin.",
            path.display()
        );
    }
    let entry = || snippet(bin);
    let file = JsonFile::read(path, by_hand(install, &entry))?;
    let mut new = file.value.clone();
    edit(&mut new, install, bin, &file.path)?;
    Ok(file.change(new, !install, &strip))
}

/// Whether `text` is a plugin some version of agentmail wrote, as it
/// wrote it (for any binary): its last line is the digest of the text
/// above ([`sign`]). A copy the user edited is not.
fn pristine(text: &str) -> bool {
    let Some(body) = text.strip_suffix('\n') else {
        return false;
    };
    let cut = body.rfind('\n').map_or(0, |i| i + 1);
    let (body, last) = text.split_at(cut);
    is_plugin(body) && last.starts_with(DIGEST_PREFIX) && sign(body.to_string()) == text
}

/// Where to save `before`, agentmail's plugin at `path`, before it is
/// replaced or deleted, so that edits of the user's are never lost: nowhere
/// when it is a plugin some agentmail version wrote, as it wrote it
/// ([`pristine`]), or its backup `bak` already holds it; else that backup,
/// unless it holds something else to keep (`bak_kept`), then the first free
/// `<backup>.<n>`.
fn save_edits(path: &Path, before: &str, bak: Option<&str>, bak_kept: bool) -> Option<PathBuf> {
    if pristine(before) || bak == Some(before) {
        return None;
    }
    let backup = backup_path(path);
    if !bak_kept {
        return Some(backup);
    }
    (1..)
        .map(|n| {
            let mut s = backup.as_os_str().to_owned();
            s.push(format!(".{n}"));
            PathBuf::from(s)
        })
        .find(|p| std::fs::symlink_metadata(p).is_err())
}

/// The plugin's change, if any.
///
/// Install never writes through a symlink (the file it points to, perhaps
/// a source checkout, is not agentmail's to change): it refuses. Uninstall
/// removes agentmail's plugin, and of a symlink only the link: when
/// agentmail did not create the file (no marker) and its backup is not
/// agentmail's plugin, that backup is put back instead. A plugin the user
/// edited (one agentmail would not write) is saved before it is replaced
/// or deleted ([`save_edits`]), and a backup holding such a copy (or one
/// that is not agentmail's plugin and is not put back) is kept; a new
/// install keeps it too.
fn plan_plugin(path: PathBuf, install: bool, bin: &str) -> Result<Option<Change>> {
    let link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
    let before = read_opt(&path)?;
    let ours = before.as_deref().is_none_or(is_plugin);
    let bak = read_opt(&backup_path(&path))?;
    let bak_worth = bak.as_deref().is_some_and(|b| !pristine(b));
    if !install {
        let Some(text) = before.as_deref().filter(|_| ours) else {
            return Ok(None);
        };
        let created = std::fs::symlink_metadata(created_marker_path(&path)).is_ok();
        let restore = match &bak {
            Some(b) if !link && !created && !is_plugin(b) => Some(b.clone()),
            _ => None,
        };
        // A restored backup is used up; any other worth keeping stays.
        let keep_backup = restore.is_none() && bak_worth;
        // The file a link points to is never touched: nothing to save.
        let save = (!link)
            .then(|| save_edits(&path, text, bak.as_deref(), keep_backup))
            .flatten();
        return Ok(Some(Change {
            keep_backup: keep_backup || save.as_ref() == Some(&backup_path(&path)),
            save,
            path,
            before,
            after: restore,
            record_anew: false,
            // The file a link points to is never agentmail's to delete,
            // even one it created before the user swapped in the link.
            unlink: link,
        }));
    }
    if link {
        bail!(
            "{} is a symbolic link; agentmail does not write through it and left it unchanged. \
             Remove the link and run the install again.",
            path.display()
        );
    }
    if !ours {
        bail!(
            "{} exists and is not agentmail's plugin; agentmail left it unchanged. Move it away and run the install again.",
            path.display()
        );
    }
    let after = plugin_text(bin)?;
    if before.as_deref() == Some(after.as_str()) {
        return Ok(None);
    }
    // A backup of the file before agentmail is written only where there is
    // none and agentmail did not create the file (an earlier version's
    // plugin is no original to keep: [`apply`] writes none). Otherwise an
    // edited plugin is saved, as the backup when there is none, else
    // beside it.
    let created = std::fs::symlink_metadata(created_marker_path(&path)).is_ok();
    let save = match (&before, &bak) {
        (Some(text), Some(_)) => save_edits(&path, text, bak.as_deref(), bak_worth),
        (Some(text), None) if created => save_edits(&path, text, None, false),
        _ => None,
    };
    // A plugin that is not there yet starts a new record: any backup or
    // marker of an earlier one no longer describes it, except a backup
    // holding an edited agentmail plugin, which is kept.
    let record_anew =
        before.is_none() && !bak.as_deref().is_some_and(|b| is_plugin(b) && bak_worth);
    Ok(Some(Change {
        path,
        record_anew,
        before,
        after: Some(after),
        unlink: false,
        save,
        keep_backup: false,
    }))
}

pub(super) fn plan(o: &InstallOpts, env: &Env, install: bool) -> Result<Vec<Change>> {
    let bin = bin_str(o)?;
    let (configs, overrides, plugin) = files(o, env);
    // Install updates the entry where there is one (in every file, since
    // opencode merges them), else adds it to the first direct file;
    // uninstall removes it from all.
    let mut targets = Vec::new();
    for path in configs.iter().chain(&overrides) {
        if !install || load(path, install, bin)?.is_some_and(|(v, _)| has_entry(&v)) {
            targets.push(path.clone());
        }
    }
    if targets.is_empty() {
        targets.push(configs[0].clone());
    }
    // All are planned before any is written: a refusal changes nothing.
    let plugin_dir = plugin.parent().expect("a plugins directory");
    let legacy = plugin_dir.with_file_name("plugin").join("agentmail.js");
    let plugin = plan_plugin(plugin, install, bin)?;
    // opencode also loads `plugin/`, before `plugins/`, and runs only the
    // first plugin of each id: an agentmail plugin there would shadow the
    // installed one, so both install and uninstall remove it.
    let legacy = plan_plugin(legacy, false, bin)?;
    let mut changes = Vec::new();
    for path in targets {
        changes.extend(plan_config(path, install, bin)?);
    }
    changes.extend(plugin);
    changes.extend(legacy);
    Ok(changes)
}
