//! opencode: the MCP server in `opencode.json`, the plugin in `plugins/`.
//!
//! User scope: the config directory is `$XDG_CONFIG_HOME/opencode`, else
//! `~/.config/opencode`; project scope (`--project`) is the current
//! directory, with the plugin in `.opencode/plugins/`. The config file is
//! the first of `opencode.json` and `opencode.jsonc` there, else a new
//! `opencode.json`. The MCP server goes in `mcp.servers.agentmail`, also
//! in a file that keeps its other servers in the V1 shape (`mcp.<name>`):
//! opencode 2.0.27 reads both, but drops `codemode` from a V1 entry, and
//! its own `opencode mcp add` writes `servers` into such a file too (see
//! `docs/harness-notes.md`). A V1 entry named `agentmail` is agentmail's:
//! install moves it to `servers`, uninstall removes it. A config with
//! comments is never rewritten (they would be lost): install and uninstall
//! refuse with the entry to add or remove by hand, unless the file is
//! already as they would leave it. The plugin is `plugin/agentmail.js`,
//! embedded, with the binary's path in place of its placeholder; a
//! `plugins/agentmail.js` that is not agentmail's is never replaced or
//! removed.

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

/// The files: (config, plugin).
fn files(o: &InstallOpts, env: &Env) -> (PathBuf, PathBuf) {
    let (dir, plugins) = if o.project {
        (env.cwd.clone(), env.cwd.join(".opencode").join("plugins"))
    } else {
        let dir = match env.dir_var("XDG_CONFIG_HOME") {
            Some(d) => d.join("opencode"),
            None => env.home.join(".config").join("opencode"),
        };
        let plugins = dir.join("plugins");
        (dir, plugins)
    };
    let config = ["opencode.json", "opencode.jsonc"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| std::fs::symlink_metadata(p).is_ok())
        .unwrap_or_else(|| dir.join("opencode.json"));
    (config, plugins.join("agentmail.js"))
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
    match servers.get_mut("agentmail") {
        // Keep what the user added to the entry, such as `environment`.
        Some(Value::Object(cur)) => merge_keys(cur, want, &SERVER_KEYS),
        _ => {
            let mut entry = want.clone();
            if let Some(Value::Object(old)) = legacy {
                for k in SHARED_KEYS {
                    if let Some(x) = old.get(k) {
                        entry.insert(k.to_string(), x.clone());
                    }
                }
                if old.get("enabled") == Some(&Value::Bool(false)) {
                    entry.insert("disabled".into(), Value::Bool(true));
                }
            }
            servers.insert("agentmail".into(), Value::Object(entry));
        }
    }
    Ok(())
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
    let text = read_opt(&path)?;
    if let Some(bare) = text.as_deref().and_then(super::json::strip_comments) {
        // Check whether the file is already as it should be; never write it.
        let value = match super::json::parse(&bare) {
            Ok(Value::Object(m)) => m,
            _ => bail!(
                "cannot parse {}; agentmail left it unchanged. Fix it, or add this by hand:\n{}",
                path.display(),
                snippet(bin)
            ),
        };
        let mut new = value.clone();
        edit(&mut new, install, bin, &path)?;
        if new == value {
            return Ok(None);
        }
        if install {
            bail!(
                "{} has comments, which agentmail cannot keep when it edits the file; nothing was changed. \
                 Add this by hand (merging it into the \"mcp\" there), then run the install again to add the plugin:\n{}",
                path.display(),
                snippet(bin)
            );
        }
        bail!(
            "{} has comments, which agentmail cannot keep when it edits the file; nothing was changed. \
             Remove the \"agentmail\" entry from \"mcp\" → \"servers\" (and any \"agentmail\" directly \
             under \"mcp\") by hand, then run the uninstall again to remove the plugin.",
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
    let (config, plugin) = files(o, env);
    // Both are planned before either is written: a refusal changes nothing.
    let plugin = plan_plugin(plugin, install, bin)?;
    let config = plan_config(config, install, bin)?;
    Ok([config, plugin].into_iter().flatten().collect())
}
