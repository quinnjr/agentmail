//! kiro-cli (V3 engine only): the MCP server in `settings/mcp.json`, the
//! hooks in a standalone hooks file, `hooks/agentmail.json`.
//!
//! User scope: `$KIRO_HOME`, else `~/.kiro`. Project scope (`--project`):
//! `.kiro/` in the current directory. kiro-cli V3 loads every `*.json`
//! file in the user's `hooks/` (and a trusted workspace's `.kiro/hooks/`)
//! for whichever agent runs, so no agent file is written and none is made
//! the default (see docs/harness-notes.md, "kiro-cli V3 hooks").
//!
//! The hooks: reminders on `SessionStart` and `UserPromptSubmit`, urgent
//! mail on `PreToolUse` (kiro ignores `PostToolUse` output), and the
//! open-question check on `Stop`. With `--kiro-wake` the Stop hook also
//! waits for mail (`--wake`, no timeout) and the MCP server gets
//! `AGENTMAIL_KIRO_WAKE=1`, so kiro agents register with `wake` on: the
//! MCP server reads the variable, and the CLI, whose environment kiro
//! does not give it, reads the installed entry ([`wake_installed`]).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};

use super::{
    ByHand, Change, Env, InstallOpts, JsonFile, bin_str, merge_keys, object_at, owns_hook,
    shell_quote, shell_words,
};
use crate::identity::ENV_KIRO_WAKE;

/// Timeout for the quick hooks, in seconds.
const HOOK_TIMEOUT: u64 = 10;
/// kiro's timeout for no timeout at all: the waiting Stop hook.
const NO_TIMEOUT: u64 = 0;
/// The hooks file format kiro-cli V3 reads.
const VERSION: &str = "v1";

/// The line `install` prints for kiro.
pub const V3_WARNING: &str =
    "kiro-cli: agentmail supports the V3 engine only — run kiro-cli chat --v3";

/// kiro's config directory: the workspace's (`project`) or the user's.
fn dir(project: bool, env: &Env) -> PathBuf {
    if project {
        return env.cwd.join(".kiro");
    }
    env.dir_var("KIRO_HOME")
        .unwrap_or_else(|| env.home.join(".kiro"))
}

/// The files: (hooks file, file with `mcpServers`).
fn files(o: &InstallOpts, env: &Env) -> (PathBuf, PathBuf) {
    let d = dir(o.project, env);
    (d.join("hooks").join("agentmail.json"), mcp_file(&d))
}

/// The file with `mcpServers` in kiro config directory `d`.
fn mcp_file(d: &Path) -> PathBuf {
    d.join("settings").join("mcp.json")
}

/// agentmail's hooks, in the order they are written.
fn wanted_hooks(bin: &str, wake: bool) -> Vec<Value> {
    let cmd = |event: &str| format!("{} hook {event} --harness kiro", shell_quote(bin));
    let hook = |trigger: &str, event: &str, matcher: Option<&str>, command: String, t: u64| {
        let mut h = Map::new();
        h.insert("name".into(), json!(format!("agentmail-{event}")));
        h.insert("trigger".into(), json!(trigger));
        if let Some(m) = matcher {
            h.insert("matcher".into(), json!(m));
        }
        h.insert(
            "action".into(),
            json!({"type": "command", "command": command}),
        );
        h.insert("timeout".into(), json!(t));
        Value::Object(h)
    };
    let (stop, stop_timeout) = match wake {
        true => (format!("{} --wake", cmd("stop")), NO_TIMEOUT),
        false => (cmd("stop"), HOOK_TIMEOUT),
    };
    vec![
        hook(
            "SessionStart",
            "session-start",
            None,
            cmd("session-start"),
            HOOK_TIMEOUT,
        ),
        hook(
            "UserPromptSubmit",
            "prompt",
            None,
            cmd("prompt"),
            HOOK_TIMEOUT,
        ),
        // Every tool: without a matcher kiro runs it for every call too,
        // but logs a warning each time it loads the file.
        hook(
            "PreToolUse",
            "pre-tool",
            Some(".*"),
            cmd("pre-tool"),
            HOOK_TIMEOUT,
        ),
        hook("Stop", "stop", None, stop, stop_timeout),
    ]
}

/// The keys of a hook entry that agentmail sets; others (such as
/// `description` or `enabled`) are kept.
const HOOK_KEYS: [&str; 5] = ["name", "trigger", "matcher", "action", "timeout"];
/// The keys of the MCP server entry that agentmail sets; others (such as
/// the user's `env` variables) are kept. agentmail's own variable in `env`
/// is handled apart ([`set_wake_env`]).
const SERVER_KEYS: [&str; 2] = ["command", "args"];

fn hook_command(h: &Value) -> Option<&str> {
    h.get("action")?.get("command")?.as_str()
}

fn is_owned_hook(h: &Value, bin: &Path) -> bool {
    hook_command(h).is_some_and(|c| owns_hook(c, bin))
}

/// Which of agentmail's hooks an entry runs: the word after `hook`.
fn hook_kind(h: &Value) -> Option<String> {
    shell_words(hook_command(h)?, 3).into_iter().nth(2)
}

/// `hooks` with agentmail's entries made `wanted` (none to uninstall). An
/// entry of agentmail's for the same kind is updated in place, keeping
/// its position and the keys agentmail does not set; agentmail's other
/// entries are removed; missing ones are added at the end.
fn merge_hooks(hooks: &mut Vec<Value>, wanted: &[Value], bin: &Path) {
    let mut found = vec![false; wanted.len()];
    hooks.retain_mut(|h| {
        if !is_owned_hook(h, bin) {
            return true;
        }
        let kind = hook_kind(h);
        let slot = (0..wanted.len()).find(|&i| !found[i] && hook_kind(&wanted[i]) == kind);
        match (slot, h.as_object_mut()) {
            (Some(i), Some(cur)) => {
                merge_keys(cur, wanted[i].as_object().expect("an object"), &HOOK_KEYS);
                found[i] = true;
                true
            }
            _ => false,
        }
    });
    hooks.extend(
        wanted
            .iter()
            .zip(&found)
            .filter(|(_, f)| !**f)
            .map(|(w, _)| w.clone()),
    );
}

/// Remove agentmail's hooks from a hooks file, for [`JsonFile::change`].
/// A file left with no hooks loses its `hooks` list (kiro rejects an
/// empty one) and its `version`.
fn strip_hooks_file(v: &mut Map<String, Value>, bin: &Path) {
    if let Some(Value::Array(hooks)) = v.get_mut("hooks") {
        hooks.retain(|h| !is_owned_hook(h, bin));
        if hooks.is_empty() {
            v.shift_remove("hooks");
            if v.get("version").and_then(Value::as_str) == Some(VERSION) {
                v.shift_remove("version");
            }
        }
    }
}

fn server(bin: &str, wake: bool) -> Value {
    let mut s = json!({"command": bin, "args": ["mcp"]});
    if wake {
        s["env"] = json!({ ENV_KIRO_WAKE: "1" });
    }
    s
}

/// Set (`wake`) or remove agentmail's variable in a server entry's `env`,
/// keeping the user's; an `env` left empty by removing it goes too.
fn set_wake_env(entry: &mut Map<String, Value>, wake: bool) {
    if wake {
        let env = entry
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()));
        if !env.is_object() {
            *env = Value::Object(Map::new());
        }
        let env = env.as_object_mut().expect("an object");
        if env.get(ENV_KIRO_WAKE) != Some(&json!("1")) {
            env.insert(ENV_KIRO_WAKE.into(), json!("1"));
        }
        return;
    }
    if let Some(Value::Object(env)) = entry.get_mut("env")
        && env.shift_remove(ENV_KIRO_WAKE).is_some()
        && env.is_empty()
    {
        entry.shift_remove("env");
    }
}

/// Whether `install --kiro-wake` is in effect where `env` runs: the
/// agentmail MCP server entry of the workspace's `.kiro/settings/mcp.json`
/// (in `env.cwd`), else of the user's (`$KIRO_HOME`, else `~/.kiro`), sets
/// `AGENTMAIL_KIRO_WAKE` to `1`. kiro gives that variable only to the MCP
/// server, not to its shell tool's commands, so the CLI's `register` reads
/// it here. A file that is missing or cannot be read counts as no entry.
pub fn wake_installed(env: &Env) -> bool {
    let entry = |project| -> Option<Value> {
        let text = std::fs::read_to_string(mcp_file(&dir(project, env))).ok()?;
        let text = super::json::strip_jsonc(&text).unwrap_or(text);
        let mut v = super::json::parse(&text).ok()?;
        Some(v.get_mut("mcpServers")?.get_mut("agentmail")?.take())
    };
    entry(true)
        .or_else(|| entry(false))
        .is_some_and(|s| s.get("env").and_then(|e| e.get(ENV_KIRO_WAKE)) == Some(&json!("1")))
}

/// Remove agentmail's MCP server, for [`JsonFile::change`].
fn strip_mcp(v: &mut Map<String, Value>) {
    if let Some(Value::Object(servers)) = v.get_mut("mcpServers") {
        servers.shift_remove("agentmail");
    }
}

pub(super) fn plan(o: &InstallOpts, env: &Env, install: bool) -> Result<Vec<Change>> {
    let bin = bin_str(o)?;
    let (hooks_path, mcp_path) = files(o, env);
    let wanted = wanted_hooks(bin, o.kiro_wake);
    ensure!(
        wanted.iter().all(|h| is_owned_hook(h, &o.bin)),
        "agentmail would not recognise its own kiro hooks; nothing was changed"
    );

    let snippet = || super::json::pretty(&json!({"version": VERSION, "hooks": wanted}), "  ", "");
    let file = JsonFile::read(
        hooks_path,
        if install {
            ByHand::Add(&snippet)
        } else {
            ByHand::Remove("the `agentmail hook` commands under `hooks`")
        },
    )?;
    let mut new = file.value.clone();
    if install {
        if !new.contains_key("version") {
            new.insert("version".into(), json!(VERSION));
        }
        let hooks = new
            .entry("hooks")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(hooks) = hooks.as_array_mut() else {
            bail!(
                "`hooks` in {} is not a JSON array; agentmail left the file unchanged",
                file.path.display()
            );
        };
        merge_hooks(hooks, &wanted, &o.bin);
    } else {
        strip_hooks_file(&mut new, &o.bin);
    }

    let mcp_snippet = || {
        super::json::pretty(
            &json!({"mcpServers": {"agentmail": server(bin, o.kiro_wake)}}),
            "  ",
            "",
        )
    };
    let mcp = JsonFile::read(
        mcp_path,
        if install {
            ByHand::Add(&mcp_snippet)
        } else {
            ByHand::Remove("`mcpServers.agentmail`")
        },
    )?;
    let mut new_mcp = mcp.value.clone();
    if install {
        let servers = object_at(&mut new_mcp, "mcpServers", &mcp.path)?;
        let want = server(bin, o.kiro_wake);
        match servers.get_mut("agentmail") {
            // Keep what the user added to the entry, such as `env`.
            Some(Value::Object(cur)) => {
                merge_keys(cur, want.as_object().expect("an object"), &SERVER_KEYS);
                set_wake_env(cur, o.kiro_wake);
            }
            _ => {
                servers.insert("agentmail".into(), want);
            }
        }
    } else {
        strip_mcp(&mut new_mcp);
    }

    Ok([
        file.change(new, !install, &|v| strip_hooks_file(v, &o.bin)),
        mcp.change(new_mcp, !install, &strip_mcp),
    ]
    .into_iter()
    .flatten()
    .collect())
}

/// What `install` tells the user about kiro: the V3-only warning, what
/// waking costs (`--kiro-wake`), and that `--set-default` has nothing to
/// do.
pub(super) fn notes(o: &InstallOpts, install: bool) -> Vec<String> {
    if !install {
        return Vec::new();
    }
    let mut notes = vec![V3_WARNING.to_string()];
    if o.kiro_wake {
        notes.push(
            "note: kiro-cli waking is on: after each turn the Stop hook waits for mail, \
             and kiro holds what you type until mail comes or you press Esc"
                .to_string(),
        );
    }
    if o.set_default {
        notes.push(
            "note: --set-default changes nothing: kiro-cli V3 loads hooks/agentmail.json \
             for every agent, so there is no agentmail agent to make the default"
                .to_string(),
        );
    }
    notes
}
