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
//! its placeholder; a `plugins/agentmail.js` that is not agentmail's is
//! never replaced or removed.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::{Change, Env, InstallOpts, JsonFile, bin_str, merge_keys, object_at, read_opt};

/// The plugin as shipped.
const PLUGIN: &str = include_str!("../../plugin/agentmail.js");
/// The plugin's binary path, a JS string literal, as shipped.
const PLACEHOLDER: &str = "\"__AGENTMAIL_BIN__\"";
/// The plugin's first line, which marks a file as agentmail's plugin.
const PLUGIN_HEAD: &str = "// agentmail for opencode";

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
    let (dir, overrides, plugins) = if o.project {
        let dot = env.cwd.join(".opencode");
        (env.cwd.clone(), existing(&dot), dot.join("plugins"))
    } else {
        let dir = match env.dir_var("XDG_CONFIG_HOME") {
            Some(d) => d.join("opencode"),
            None => env.home.join(".config").join("opencode"),
        };
        let plugins = dir.join("plugins");
        (dir, Vec::new(), plugins)
    };
    let mut configs = existing(&dir);
    if configs.is_empty() {
        configs.push(dir.join("opencode.json"));
    }
    (configs, overrides, plugins.join("agentmail.js"))
}

fn server(bin: &str) -> Value {
    json!({"type": "local", "command": [bin, "mcp"], "codemode": false})
}

/// The plugin with `bin` in place of its placeholder.
fn plugin_text(bin: &str) -> Result<String> {
    if PLUGIN.matches(PLACEHOLDER).count() != 1 {
        bail!("the embedded opencode plugin has no single binary placeholder");
    }
    Ok(PLUGIN.replacen(PLACEHOLDER, &serde_json::to_string(bin)?, 1))
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
fn load(path: &Path, bin: &str) -> Result<Option<(Map<String, Value>, bool)>> {
    let Some(text) = read_opt(path)? else {
        return Ok(None);
    };
    let bare = super::json::strip_jsonc(&text);
    match super::json::parse(bare.as_deref().unwrap_or(&text)) {
        Ok(Value::Object(m)) => Ok(Some((m, bare.is_some()))),
        Ok(_) => bail!(
            "{} is not a JSON object; agentmail left it unchanged. Add this by hand:\n{}",
            path.display(),
            snippet(bin)
        ),
        Err(e) => bail!(
            "cannot parse {}: {e}; agentmail left it unchanged. Fix it, or add this by hand:\n{}",
            path.display(),
            snippet(bin)
        ),
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
    if let Some((value, true)) = load(&path, bin)? {
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
                 to add the plugin, and `opencode service restart` to load the server (opencode's \
                 background service reads its config only at start):\n{}",
                path.display(),
                snippet(bin)
            );
        }
        bail!(
            "{} {jsonc}. Remove the \"agentmail\" entry from \"mcp\" → \"servers\" (and any \
             \"agentmail\" directly under \"mcp\") by hand, then run the uninstall again to \
             remove the plugin, and `opencode service restart` to unload the server (opencode's \
             background service reads its config only at start).",
            path.display()
        );
    }
    let file = JsonFile::read(path, &|| snippet(bin))?;
    let mut new = file.value.clone();
    edit(&mut new, install, bin, &file.path)?;
    Ok(file.change(new, !install, &strip))
}

/// The plugin's change, if any.
fn plan_plugin(path: PathBuf, install: bool, bin: &str) -> Result<Option<Change>> {
    let before = read_opt(&path)?;
    let ours = before.as_deref().is_none_or(is_plugin);
    if !install {
        return Ok((before.is_some() && ours).then_some(Change {
            path,
            before,
            after: None,
            record_anew: false,
        }));
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
    Ok(Some(Change {
        path,
        // A plugin that is not there yet starts a new record: any backup or
        // marker of an earlier one no longer describes it.
        record_anew: before.is_none(),
        before,
        after: Some(after),
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
        if !install || load(path, bin)?.is_some_and(|(v, _)| has_entry(&v)) {
            targets.push(path.clone());
        }
    }
    if targets.is_empty() {
        targets.push(configs[0].clone());
    }
    // All are planned before any is written: a refusal changes nothing.
    let plugin = plan_plugin(plugin, install, bin)?;
    let mut changes = Vec::new();
    for path in targets {
        changes.extend(plan_config(path, install, bin)?);
    }
    changes.extend(plugin);
    Ok(changes)
}
