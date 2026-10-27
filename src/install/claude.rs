//! Claude Code: hooks in `settings.json`, the MCP server in `.claude.json`.
//!
//! User scope: `$CLAUDE_CONFIG_DIR/settings.json` and
//! `$CLAUDE_CONFIG_DIR/.claude.json`, or without `CLAUDE_CONFIG_DIR`
//! `~/.claude/settings.json` and `~/.claude.json`. Project scope
//! (`--project`): `.claude/settings.json` and `.mcp.json` in the current
//! directory. The hook set is the one `docs/harness-notes.md` describes
//! (Task 4): reminders on SessionStart, UserPromptSubmit and every tool
//! call (PostToolUse and PostToolUseFailure), the open-question check on
//! Stop, and the `asyncRewake` wait hook that wakes an idle session.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};

use super::{
    Change, Env, InstallOpts, JsonFile, bin_str, merge_keys, object_at, owns_hook, shell_quote,
    shell_words,
};

/// Timeout for the quick hooks, in seconds.
const HOOK_TIMEOUT: u64 = 10;
/// Timeout for the wait hook: a day (Claude Code's timers overflow above
/// about 24.8 days; see the harness notes).
const WAIT_TIMEOUT: u64 = 86_400;

/// The files: (settings with hooks, file with `mcpServers`).
fn files(o: &InstallOpts, env: &Env) -> (PathBuf, PathBuf) {
    if o.project {
        return (
            env.cwd.join(".claude").join("settings.json"),
            env.cwd.join(".mcp.json"),
        );
    }
    match env.dir_var("CLAUDE_CONFIG_DIR") {
        Some(dir) => (dir.join("settings.json"), dir.join(".claude.json")),
        None => (
            env.home.join(".claude").join("settings.json"),
            env.home.join(".claude.json"),
        ),
    }
}

/// agentmail's hook groups per event, in the order they are written.
fn hook_groups(bin: &str) -> Vec<(&'static str, Vec<Value>)> {
    let cmd = |event: &str| format!("{} hook {event} --harness claude-code", shell_quote(bin));
    let quick =
        |event: &str| json!({"type": "command", "command": cmd(event), "timeout": HOOK_TIMEOUT});
    vec![
        (
            "SessionStart",
            vec![json!({"hooks": [quick("session-start")]})],
        ),
        (
            "UserPromptSubmit",
            vec![json!({"hooks": [quick("prompt")]})],
        ),
        (
            "PostToolUse",
            vec![json!({"matcher": "*", "hooks": [quick("post-tool")]})],
        ),
        // Failed tool calls run PostToolUseFailure instead of PostToolUse.
        (
            "PostToolUseFailure",
            vec![json!({"matcher": "*", "hooks": [quick("post-tool")]})],
        ),
        (
            "Stop",
            vec![json!({"hooks": [
                quick("stop"),
                {"type": "command", "command": cmd("wait"), "asyncRewake": true, "timeout": WAIT_TIMEOUT}
            ]})],
        ),
    ]
}

fn server(bin: &str) -> Value {
    json!({"type": "stdio", "command": bin, "args": ["mcp"]})
}

fn is_owned_hook(h: &Value, bin: &Path) -> bool {
    h.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| owns_hook(c, bin))
}

/// Remove agentmail's hooks from an event's groups, and the groups that
/// held nothing else.
fn strip(groups: &mut Vec<Value>, bin: &Path) {
    groups.retain_mut(|g| {
        let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let n = hs.len();
        hs.retain(|h| !is_owned_hook(h, bin));
        hs.len() == n || !hs.is_empty()
    });
}

/// The keys of a hook entry that agentmail sets; others (such as a
/// `statusMessage` the user added) are kept.
const HOOK_KEYS: [&str; 5] = ["type", "command", "timeout", "async", "asyncRewake"];
/// The keys of the MCP server entry that agentmail sets; others (such as
/// `env`) are kept.
const SERVER_KEYS: [&str; 3] = ["type", "command", "args"];

/// Which of agentmail's hooks a command runs: the word after `hook`.
fn hook_kind(h: &Value) -> Option<String> {
    let cmd = h.get("command")?.as_str()?;
    shell_words(cmd, 3).into_iter().nth(2)
}

/// `hooks` with agentmail's entries made `want` (none to uninstall).
/// Events left empty stay; [`JsonFile::change`] removes the ones agentmail
/// emptied.
fn edit_hooks(
    hooks: &mut Map<String, Value>,
    want: &[(&str, Vec<Value>)],
    bin: &Path,
    path: &Path,
) -> Result<()> {
    for (ev, groups) in hooks.iter_mut() {
        let wanted = want
            .iter()
            .find(|(e, _)| e == ev)
            .map_or(&[][..], |(_, g)| g.as_slice());
        let Some(groups) = groups.as_array_mut() else {
            // Not ours to fix; only refuse when we have to add to it.
            if !wanted.is_empty() {
                bail!(
                    "`hooks.{ev}` in {} is not a JSON array; agentmail left the file unchanged",
                    path.display()
                );
            }
            continue;
        };
        merge_event(groups, wanted, bin);
    }
    for (ev, groups) in want {
        if !hooks.contains_key(*ev) {
            hooks.insert(ev.to_string(), Value::Array(groups.clone()));
        }
    }
    Ok(())
}

/// One event's groups with agentmail's hooks made `wanted`. A hook of
/// agentmail's already there for the same kind (`stop`, `wait`, ...) is
/// updated in place, keeping its group (and so its matcher) and the keys
/// agentmail does not set; the rest of agentmail's hooks are removed, with
/// the groups that held nothing else; missing ones are added to the group
/// holding their siblings, else as a new group.
fn merge_event(groups: &mut Vec<Value>, wanted: &[Value], bin: &Path) {
    // (wanted group, kind, hook) for every hook agentmail wants here.
    let want: Vec<(usize, Option<String>, &Value)> = wanted
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| {
            g["hooks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |h| (gi, hook_kind(h), h))
        })
        .collect();
    // The group each wanted hook was found in.
    let mut found: Vec<Option<usize>> = vec![None; want.len()];
    let mut emptied = Vec::new();
    for (gi, g) in groups.iter_mut().enumerate() {
        let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let n = hs.len();
        hs.retain_mut(|h| {
            if !is_owned_hook(h, bin) {
                return true;
            }
            let kind = hook_kind(h);
            let slot = (0..want.len()).find(|&i| found[i].is_none() && want[i].1 == kind);
            match (
                slot,
                h.as_object_mut(),
                slot.and_then(|i| want[i].2.as_object()),
            ) {
                (Some(i), Some(cur), Some(w)) => {
                    merge_keys(cur, w, &HOOK_KEYS);
                    found[i] = Some(gi);
                    true
                }
                _ => false,
            }
        });
        if n > 0 && hs.is_empty() {
            emptied.push(gi);
        }
    }
    for (wg, group) in wanted.iter().enumerate() {
        let missing: Vec<Value> = (0..want.len())
            .filter(|&i| want[i].0 == wg && found[i].is_none())
            .map(|i| want[i].2.clone())
            .collect();
        if missing.is_empty() {
            continue;
        }
        let host = (0..want.len()).find_map(|i| (want[i].0 == wg).then_some(found[i]).flatten());
        match host.and_then(|gi| groups[gi].get_mut("hooks")?.as_array_mut()) {
            Some(hs) => hs.extend(missing),
            None => {
                let mut g = group.clone();
                g["hooks"] = Value::Array(missing);
                groups.push(g);
            }
        }
    }
    let mut i = 0;
    groups.retain(|_| {
        i += 1;
        !emptied.contains(&(i - 1))
    });
}

/// Remove agentmail's hooks from a settings object, for
/// [`JsonFile::change`].
fn strip_settings(v: &mut Map<String, Value>, bin: &Path) {
    if let Some(Value::Object(hooks)) = v.get_mut("hooks") {
        for groups in hooks.values_mut() {
            if let Some(groups) = groups.as_array_mut() {
                strip(groups, bin);
            }
        }
    }
}

/// Remove agentmail's MCP server, for [`JsonFile::change`].
fn strip_mcp(v: &mut Map<String, Value>) {
    if let Some(Value::Object(servers)) = v.get_mut("mcpServers") {
        servers.shift_remove("agentmail");
    }
}

pub(super) fn plan(o: &InstallOpts, env: &Env, install: bool) -> Result<Vec<Change>> {
    let bin = bin_str(o)?;
    let (settings_path, mcp_path) = files(o, env);
    let groups = hook_groups(bin);
    // Whatever the binary is called, the hooks written are recognised as
    // agentmail's: else a second install would add them again and
    // uninstall would leave them.
    for (ev, gs) in &groups {
        for g in gs {
            ensure!(
                g["hooks"]
                    .as_array()
                    .is_some_and(|hs| hs.iter().all(|h| is_owned_hook(h, &o.bin))),
                "agentmail would not recognise its own {ev} hook; nothing was changed"
            );
        }
    }

    let snippet = || {
        let hooks: Map<String, Value> = groups
            .iter()
            .map(|(e, g)| (e.to_string(), Value::Array(g.clone())))
            .collect();
        super::json::pretty(&json!({"hooks": hooks}), "  ", "")
    };
    let settings = JsonFile::read(settings_path, &snippet)?;
    let mut new = settings.value.clone();
    if install {
        let hooks = object_at(&mut new, "hooks", &settings.path)?;
        edit_hooks(hooks, &groups, &o.bin, &settings.path)?;
    } else if let Some(Value::Object(hooks)) = new.get_mut("hooks") {
        edit_hooks(hooks, &[], &o.bin, &settings.path)?;
    }

    let mcp_snippet =
        || super::json::pretty(&json!({"mcpServers": {"agentmail": server(bin)}}), "  ", "");
    let mcp = JsonFile::read(mcp_path, &mcp_snippet)?;
    let mut new_mcp = mcp.value.clone();
    if install {
        let servers = object_at(&mut new_mcp, "mcpServers", &mcp.path)?;
        let want = server(bin);
        match servers.get_mut("agentmail") {
            // Keep what the user added to the entry, such as `env`.
            Some(Value::Object(cur)) => {
                merge_keys(cur, want.as_object().expect("an object"), &SERVER_KEYS)
            }
            _ => {
                servers.insert("agentmail".into(), want);
            }
        }
    } else {
        strip_mcp(&mut new_mcp);
    }

    Ok([
        settings.change(new, !install, &|v| strip_settings(v, &o.bin)),
        mcp.change(new_mcp, !install, &strip_mcp),
    ]
    .into_iter()
    .flatten()
    .collect())
}
