//! Harness Tasks 5 and 7: `agentmail install` / `uninstall` for Claude Code
//! and opencode.
//!
//! Each test works in its own temporary home and working directory and
//! drives the planner and `apply` directly, except the CLI tests, which run
//! the built binary with `HOME` pointed at the temporary home.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentmail::install::{self, Change, Env, InstallOpts};
use agentmail::model::Harness;
use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = "/opt/agentmail/bin/agentmail";

/// A temporary home and a project directory inside it.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Home {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("proj")).unwrap();
        Home { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn proj(&self) -> PathBuf {
        self.dir.path().join("proj")
    }

    fn env(&self, vars: &[(&str, &str)]) -> Env {
        Env {
            home: self.home(),
            cwd: self.proj(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn settings(&self) -> PathBuf {
        self.home().join(".claude/settings.json")
    }

    fn user_mcp(&self) -> PathBuf {
        self.home().join(".claude.json")
    }
}

fn opts(project: bool, bin: &str) -> InstallOpts {
    InstallOpts {
        harness: Harness::ClaudeCode,
        project,
        dry_run: false,
        set_default: false,
        bin: PathBuf::from(bin),
    }
}

fn install(o: &InstallOpts, env: &Env) -> Vec<Change> {
    let changes = install::plan_install(o, env).unwrap();
    install::apply(&changes).unwrap();
    changes
}

fn uninstall(o: &InstallOpts, env: &Env) -> Vec<Change> {
    let changes = install::plan_uninstall(o, env).unwrap();
    install::apply(&changes).unwrap();
    changes
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&read(p)).unwrap()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn bak(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".agentmail.bak");
    PathBuf::from(s)
}

fn marker(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".agentmail.created");
    PathBuf::from(s)
}

fn hook(bin: &str, event: &str) -> Value {
    json!({"type": "command", "command": format!("{bin} hook {event} --harness claude-code"), "timeout": 10})
}

/// The `hooks` object agentmail installs, for `bin` (already shell-quoted).
fn expected_hooks(bin: &str) -> Value {
    json!({
        "SessionStart": [{"hooks": [hook(bin, "session-start")]}],
        "UserPromptSubmit": [{"hooks": [hook(bin, "prompt")]}],
        "PostToolUse": [{"matcher": "*", "hooks": [hook(bin, "post-tool")]}],
        "PostToolUseFailure": [{"matcher": "*", "hooks": [hook(bin, "post-tool")]}],
        "Stop": [{"hooks": [
            hook(bin, "stop"),
            {"type": "command", "command": format!("{bin} hook wait --harness claude-code"), "asyncRewake": true, "timeout": 86400}
        ]}]
    })
}

fn expected_server(bin: &str) -> Value {
    json!({"type": "stdio", "command": bin, "args": ["mcp"]})
}

/// A user settings file as Claude Code writes it, with someone else's
/// PostToolUse hook, a hook on an event agentmail does not use, and keys
/// agentmail does not touch (one a number whose spelling serde would change).
const FOREIGN_SETTINGS: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": [
      "Bash(git status)"
    ]
  },
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "prettier --write \"$CLAUDE_FILE\""
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "notify-send claude"
          }
        ]
      }
    ]
  },
  "cleanupPeriodDays": 30.50,
  "statusLine": {"type": "command", "command": "~/bin/status é"}
}
"#;

const FOREIGN_USER_MCP: &str = r#"{
  "numStartups": 12,
  "mcpServers": {
    "github": {
      "type": "stdio",
      "command": "gh-mcp",
      "args": []
    }
  },
  "projects": {
    "/work/x": {
      "mcpServers": {}
    }
  },
  "userID": "abc"
}
"#;

#[test]
fn claude_install_into_empty() {
    let h = Home::new();
    let env = h.env(&[]);
    let changes = install(&opts(false, BIN), &env);
    assert_eq!(changes.len(), 2);
    assert!(
        changes
            .iter()
            .all(|c| c.before.is_none() && c.after.is_some())
    );

    let settings = read_json(&h.settings());
    assert_eq!(settings, json!({"hooks": expected_hooks(BIN)}));
    let mcp = read_json(&h.user_mcp());
    assert_eq!(
        mcp,
        json!({"mcpServers": {"agentmail": expected_server(BIN)}})
    );

    // 2-space indent, our key order, trailing newline.
    let text = read(&h.settings());
    assert!(
        text.starts_with("{\n  \"hooks\": {\n    \"SessionStart\": ["),
        "{text}"
    );
    assert!(text.ends_with("}\n"));
    assert!(
        read(&h.user_mcp()).contains("\"type\": \"stdio\",\n      \"command\""),
        "{}",
        read(&h.user_mcp())
    );
    // No backups for files that did not exist.
    assert!(!bak(&h.settings()).exists());
    assert!(!bak(&h.user_mcp()).exists());
}

#[test]
fn claude_install_preserves_foreign_hooks_and_keys() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    write(&h.user_mcp(), FOREIGN_USER_MCP);
    install(&opts(false, BIN), &h.env(&[]));

    let text = read(&h.settings());
    let v: Value = serde_json::from_str(&text).unwrap();
    let orig: Value = serde_json::from_str(FOREIGN_SETTINGS).unwrap();
    for k in ["model", "permissions", "cleanupPeriodDays", "statusLine"] {
        assert_eq!(v[k], orig[k], "{k}");
    }
    // The foreign hooks stay, first; ours are appended.
    let post = v["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post.len(), 2);
    assert_eq!(post[0], orig["hooks"]["PostToolUse"][0]);
    assert_eq!(post[1], expected_hooks(BIN)["PostToolUse"][0]);
    assert_eq!(v["hooks"]["Notification"], orig["hooks"]["Notification"]);
    for ev in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUseFailure",
        "Stop",
    ] {
        assert_eq!(v["hooks"][ev], expected_hooks(BIN)[ev], "{ev}");
    }
    // Untouched text keeps its exact spelling, key order and layout.
    assert!(text.starts_with("{\n  \"model\": \"opus\",\n  \"permissions\": {\n"));
    assert!(text.contains("\"cleanupPeriodDays\": 30.50,"), "{text}");
    assert!(
        text.contains(r#""statusLine": {"type": "command", "command": "~/bin/status é"}"#),
        "{text}"
    );
    assert!(text.contains("\"command\": \"prettier --write \\\"$CLAUDE_FILE\\\"\""));
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "permissions",
            "hooks",
            "cleanupPeriodDays",
            "statusLine"
        ]
    );

    let mcp = read_json(&h.user_mcp());
    let orig_mcp: Value = serde_json::from_str(FOREIGN_USER_MCP).unwrap();
    assert_eq!(
        mcp["mcpServers"]["github"],
        orig_mcp["mcpServers"]["github"]
    );
    assert_eq!(mcp["mcpServers"]["agentmail"], expected_server(BIN));
    assert_eq!(mcp["projects"], orig_mcp["projects"]);
    assert_eq!(mcp["numStartups"], 12);
    assert_eq!(mcp["userID"], "abc");
}

#[test]
fn claude_install_keeps_other_indentation() {
    let h = Home::new();
    let orig = "{\n    \"model\": \"opus\",\n    \"hooks\": {}\n}\n";
    write(&h.settings(), orig);
    install(&opts(false, BIN), &h.env(&[]));
    let text = read(&h.settings());
    assert!(text.starts_with("{\n    \"model\": \"opus\",\n    \"hooks\": {\n        \"SessionStart\": [\n            {\n"), "{text}");
    assert_eq!(read_json(&h.settings())["hooks"], expected_hooks(BIN));
    // The `hooks` object was there before: it stays, empty.
    uninstall(&opts(false, BIN), &h.env(&[]));
    assert_eq!(read(&h.settings()), orig);
}

#[test]
fn claude_uninstall_keeps_empty_containers_that_were_there() {
    let h = Home::new();
    let env = h.env(&[]);
    let settings = "{\n  \"hooks\": {\n    \"Stop\": []\n  }\n}\n";
    let mcp = "{\n  \"mcpServers\": {}\n}\n";
    write(&h.settings(), settings);
    write(&h.user_mcp(), mcp);
    install(&opts(false, BIN), &env);
    assert_eq!(read_json(&h.settings())["hooks"], expected_hooks(BIN));
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read(&h.settings()), settings);
    assert_eq!(read(&h.user_mcp()), mcp);
    assert!(
        install::plan_uninstall(&opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn claude_reinstall_after_uninstall_records_the_file_anew() {
    // The record of the file before agentmail is taken again by an install
    // into a file without agentmail's entries: containers the user added
    // since the first install stay, empty, through the next uninstall.
    let h = Home::new();
    let env = h.env(&[]);
    write(&h.settings(), "{\"model\":\"opus\"}");
    write(&h.user_mcp(), "{\"x\":1}");
    install(&opts(false, BIN), &env);
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read_json(&h.settings()), json!({"model": "opus"}));
    assert_eq!(read_json(&h.user_mcp()), json!({"x": 1}));
    let settings = "{\"model\":\"opus\",\"hooks\":{}}";
    let mcp = "{\"x\":1,\"mcpServers\":{}}";
    write(&h.settings(), settings);
    write(&h.user_mcp(), mcp);
    install(&opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.settings())), settings);
    assert_eq!(read(&bak(&h.user_mcp())), mcp);
    uninstall(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.settings()),
        json!({"model": "opus", "hooks": {}})
    );
    assert_eq!(read_json(&h.user_mcp()), json!({"x": 1, "mcpServers": {}}));
}

#[test]
fn claude_file_agentmail_created_then_kept_is_the_users_after_reinstall() {
    // Agentmail created the files; the user added keys of their own, so an
    // uninstall kept them. Once agentmail's entries are gone, the next
    // install sees files the user had, not files agentmail created.
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, BIN), &env);
    let mut s = read_json(&h.settings());
    s["model"] = json!("opus");
    write(&h.settings(), &s.to_string());
    let mut m = read_json(&h.user_mcp());
    m["x"] = json!(1);
    write(&h.user_mcp(), &m.to_string());
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read_json(&h.settings()), json!({"model": "opus"}));
    assert_eq!(read_json(&h.user_mcp()), json!({"x": 1}));
    let settings = "{\"model\":\"opus\",\"hooks\":{}}";
    let mcp = "{\"x\":1,\"mcpServers\":{}}";
    write(&h.settings(), settings);
    write(&h.user_mcp(), mcp);
    install(&opts(false, BIN), &env);
    assert!(!marker(&h.settings()).exists());
    assert!(!marker(&h.user_mcp()).exists());
    assert_eq!(read(&bak(&h.settings())), settings);
    uninstall(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.settings()),
        json!({"model": "opus", "hooks": {}})
    );
    assert_eq!(read_json(&h.user_mcp()), json!({"x": 1, "mcpServers": {}}));
}

#[test]
fn claude_install_after_file_removed_forgets_its_old_backup() {
    // A backup left from a file the user has since deleted does not make
    // the file agentmail now creates count as theirs.
    let h = Home::new();
    let env = h.env(&[]);
    write(&h.settings(), "{\"model\":\"opus\"}");
    install(&opts(false, BIN), &env);
    assert!(bak(&h.settings()).exists());
    std::fs::remove_file(h.settings()).unwrap();
    // A stale marker listing a directory this install does not create.
    write(&marker(&h.settings()), "/stale\n");
    install(&opts(false, BIN), &env);
    assert!(!bak(&h.settings()).exists());
    assert_eq!(read(&marker(&h.settings())), "");
    uninstall(&opts(false, BIN), &env);
    assert!(!h.settings().exists());
    assert!(!marker(&h.settings()).exists());
}

#[test]
fn claude_uninstall_keeps_files_that_were_empty_objects() {
    let h = Home::new();
    let env = h.env(&[]);
    write(&h.settings(), "{}\n");
    write(&h.user_mcp(), "{}");
    install(&opts(false, BIN), &env);
    let removed = uninstall(&opts(false, BIN), &env);
    assert!(removed.iter().all(|c| c.after.is_some()));
    assert_eq!(read_json(&h.settings()), json!({}));
    assert_eq!(read_json(&h.user_mcp()), json!({}));
    assert!(
        install::plan_uninstall(&opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn claude_files_agentmail_created_go_even_after_reinstall() {
    // A reinstall backs up the file agentmail itself created; uninstall
    // still sees that agentmail created it.
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, "/old/agentmail"), &env);
    install(&opts(false, BIN), &env);
    assert!(bak(&h.settings()).exists());
    uninstall(&opts(false, BIN), &env);
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    // The backups held only agentmail's entries: nothing is left behind.
    assert!(!bak(&h.settings()).exists());
    assert!(!bak(&h.user_mcp()).exists());
    assert!(!marker(&h.settings()).exists());
    assert!(!marker(&h.user_mcp()).exists());
    assert!(!h.home().join(".claude").exists());
}

#[test]
fn claude_pre_existing_file_with_only_agentmail_survives_uninstall() {
    // Added by hand (say with `claude mcp add`), with the user's own `env`.
    let h = Home::new();
    let env = h.env(&[]);
    let mine = json!({"mcpServers": {"agentmail": {
        "type": "stdio", "command": "/old/agentmail", "args": ["mcp"], "env": {"AGENTMAIL_X": "1"}
    }}});
    let text = format!("{}\n", serde_json::to_string_pretty(&mine).unwrap());
    write(&h.user_mcp(), &text);
    install(&opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.user_mcp())), text);
    assert!(!marker(&h.user_mcp()).exists());
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read_json(&h.user_mcp()), json!({}));
    assert_eq!(read(&bak(&h.user_mcp())), text);

    // Without a backup (agentmail never changed it) the same holds.
    let h = Home::new();
    write(&h.user_mcp(), &text);
    uninstall(&opts(false, BIN), &h.env(&[]));
    assert_eq!(read_json(&h.user_mcp()), json!({}));
}

#[test]
fn claude_install_keeps_fields_the_user_added() {
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, "/old/agentmail"), &env);
    let mut v = read_json(&h.user_mcp());
    v["mcpServers"]["agentmail"]["env"] = json!({"AGENTMAIL_X": "1"});
    v["mcpServers"]["agentmail"]["timeout"] = json!(5000);
    write(&h.user_mcp(), &serde_json::to_string_pretty(&v).unwrap());
    let mut s = read_json(&h.settings());
    s["hooks"]["Stop"][0]["hooks"][1]["statusMessage"] = json!("waiting for mail");
    s["hooks"]["PostToolUse"][0]["matcher"] = json!("Bash");
    write(&h.settings(), &serde_json::to_string_pretty(&s).unwrap());

    install(&opts(false, BIN), &env);
    let server = &read_json(&h.user_mcp())["mcpServers"]["agentmail"];
    assert_eq!(
        *server,
        json!({"type": "stdio", "command": BIN, "args": ["mcp"], "env": {"AGENTMAIL_X": "1"}, "timeout": 5000})
    );
    let mut want = expected_hooks(BIN);
    want["Stop"][0]["hooks"][1]["statusMessage"] = json!("waiting for mail");
    want["PostToolUse"][0]["matcher"] = json!("Bash");
    assert_eq!(read_json(&h.settings())["hooks"], want);
    assert!(
        install::plan_install(&opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );

    // A missing hook goes back into the group of its sibling.
    let mut s = read_json(&h.settings());
    s["hooks"]["Stop"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    write(&h.settings(), &serde_json::to_string_pretty(&s).unwrap());
    install(&opts(false, BIN), &env);
    let stop = &read_json(&h.settings())["hooks"]["Stop"];
    assert_eq!(stop.as_array().unwrap().len(), 1);
    assert_eq!(stop[0]["hooks"][1], hook(BIN, "stop"));
}

#[test]
fn claude_crlf_file_round_trips() {
    let h = Home::new();
    let env = h.env(&[]);
    let crlf = FOREIGN_SETTINGS.replace('\n', "\r\n");
    write(&h.settings(), &crlf);
    install(&opts(false, BIN), &env);
    let text = read(&h.settings());
    assert_eq!(
        read_json(&h.settings())["hooks"]["Stop"],
        expected_hooks(BIN)["Stop"]
    );
    assert_eq!(
        text.matches('\n').count(),
        text.matches("\r\n").count(),
        "a bare LF in {text:?}"
    );
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read(&h.settings()), crlf);
}

#[test]
fn claude_binary_not_named_agentmail_is_still_recognised() {
    let h = Home::new();
    let env = h.env(&[]);
    write(&h.settings(), FOREIGN_SETTINGS);
    let bin = "/x/am";
    install(&opts(false, bin), &env);
    assert_eq!(
        read_json(&h.settings())["hooks"]["Stop"],
        expected_hooks(bin)["Stop"]
    );
    let again = install::plan_install(&opts(false, bin), &env).unwrap();
    assert!(again.is_empty(), "{:?}", paths(&again));
    uninstall(&opts(false, bin), &env);
    assert_eq!(read(&h.settings()), FOREIGN_SETTINGS);
    assert!(!h.user_mcp().exists());
}

#[cfg(unix)]
#[test]
fn claude_hooks_written_through_a_symlink_are_recognised() {
    let h = Home::new();
    let env = h.env(&[]);
    let real = h.dir.path().join("bin/am-real");
    write(&real, "");
    let link = h.dir.path().join("bin/am");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    install(&opts(false, link.to_str().unwrap()), &env);
    // Uninstalling from the resolved path still finds the hooks.
    uninstall(&opts(false, real.to_str().unwrap()), &env);
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
}

#[test]
fn claude_install_idempotent() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    write(&h.user_mcp(), FOREIGN_USER_MCP);
    let env = h.env(&[]);
    install(&opts(false, BIN), &env);
    let again = install::plan_install(&opts(false, BIN), &env).unwrap();
    assert!(again.is_empty(), "{:?}", paths(&again));

    // A hook the user adds after ours does not make ours move.
    let mut v = read_json(&h.settings());
    v["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .push(json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "late"}]}));
    write(&h.settings(), &serde_json::to_string_pretty(&v).unwrap());
    let again = install::plan_install(&opts(false, BIN), &env).unwrap();
    assert!(again.is_empty(), "{:?}", paths(&again));
}

#[test]
fn claude_reinstall_replaces_old_entries() {
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, "/old/agentmail"), &env);
    // An entry from an older agentmail on an event this one no longer uses.
    let mut v = read_json(&h.settings());
    v["hooks"]["PreToolUse"] = json!([{"hooks": [hook("/old/agentmail", "pre-tool")]}]);
    write(&h.settings(), &serde_json::to_string_pretty(&v).unwrap());

    install(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.settings()),
        json!({"hooks": expected_hooks(BIN)})
    );
    assert_eq!(
        read_json(&h.user_mcp()),
        json!({"mcpServers": {"agentmail": expected_server(BIN)}})
    );
}

#[test]
fn claude_bin_with_space_is_quoted() {
    let h = Home::new();
    let env = h.env(&[]);
    let bin = "/opt/my tools/it's/agentmail";
    install(&opts(false, bin), &env);
    let quoted = r#"'/opt/my tools/it'\''s/agentmail'"#;
    assert_eq!(read_json(&h.settings())["hooks"], expected_hooks(quoted));
    assert_eq!(
        read_json(&h.user_mcp())["mcpServers"]["agentmail"],
        expected_server(bin)
    );
    // Quoted commands are still recognised as ours.
    assert!(
        install::plan_install(&opts(false, bin), &env)
            .unwrap()
            .is_empty()
    );
    uninstall(&opts(false, bin), &env);
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
}

#[test]
fn claude_uninstall_restores_equivalent() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    write(&h.user_mcp(), FOREIGN_USER_MCP);
    let env = h.env(&[]);
    install(&opts(false, BIN), &env);
    uninstall(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.settings()),
        serde_json::from_str::<Value>(FOREIGN_SETTINGS).unwrap()
    );
    assert_eq!(
        read_json(&h.user_mcp()),
        serde_json::from_str::<Value>(FOREIGN_USER_MCP).unwrap()
    );
    // Byte for byte, in fact.
    assert_eq!(read(&h.settings()), FOREIGN_SETTINGS);
    assert_eq!(read(&h.user_mcp()), FOREIGN_USER_MCP);
    assert!(
        install::plan_uninstall(&opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );

    // Files agentmail created are removed again.
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, BIN), &env);
    let removed = uninstall(&opts(false, BIN), &env);
    assert!(removed.iter().all(|c| c.after.is_none()));
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    assert!(!bak(&h.settings()).exists());
    assert!(!bak(&h.user_mcp()).exists());
}

#[test]
fn claude_uninstall_keeps_user_hooks_in_shared_group() {
    let h = Home::new();
    let env = h.env(&[]);
    let mixed = json!({"hooks": {"Stop": [{"hooks": [
        {"type": "command", "command": "say done"},
        hook(BIN, "stop")
    ]}]}});
    write(
        &h.settings(),
        &format!("{}\n", serde_json::to_string_pretty(&mixed).unwrap()),
    );
    uninstall(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.settings()),
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
    );
}

#[test]
fn claude_respects_config_dir() {
    let h = Home::new();
    let cfg = h.dir.path().join("cfg");
    let env = h.env(&[("CLAUDE_CONFIG_DIR", cfg.to_str().unwrap())]);
    install(&opts(false, BIN), &env);
    assert_eq!(
        read_json(&cfg.join("settings.json")),
        json!({"hooks": expected_hooks(BIN)})
    );
    assert_eq!(
        read_json(&cfg.join(".claude.json")),
        json!({"mcpServers": {"agentmail": expected_server(BIN)}})
    );
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    assert!(!h.home().join(".claude").exists());
    // The directory agentmail created goes again on uninstall.
    uninstall(&opts(false, BIN), &env);
    assert!(!cfg.exists());
}

#[test]
fn claude_project_scope() {
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(true, BIN), &env);
    assert_eq!(
        read_json(&h.proj().join(".claude/settings.json")),
        json!({"hooks": expected_hooks(BIN)})
    );
    assert_eq!(
        read_json(&h.proj().join(".mcp.json")),
        json!({"mcpServers": {"agentmail": expected_server(BIN)}})
    );
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    uninstall(&opts(true, BIN), &env);
    assert!(!h.proj().join(".claude/settings.json").exists());
    assert!(!h.proj().join(".mcp.json").exists());
    assert!(!h.proj().join(".claude").exists());
    assert_eq!(std::fs::read_dir(h.proj()).unwrap().count(), 0);

    // A `.claude/` that was there stays, even when emptied.
    std::fs::create_dir(h.proj().join(".claude")).unwrap();
    install(&opts(true, BIN), &env);
    uninstall(&opts(true, BIN), &env);
    assert!(h.proj().join(".claude").is_dir());
    assert_eq!(
        std::fs::read_dir(h.proj().join(".claude")).unwrap().count(),
        0
    );
}

#[test]
fn backup_written_once() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    write(&h.user_mcp(), FOREIGN_USER_MCP);
    let env = h.env(&[]);
    install(&opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.settings())), FOREIGN_SETTINGS);
    assert_eq!(read(&bak(&h.user_mcp())), FOREIGN_USER_MCP);
    // A second change (a new binary path) keeps the first backup.
    let changes = install(&opts(false, "/elsewhere/agentmail"), &env);
    assert_eq!(changes.len(), 2);
    assert_eq!(read(&bak(&h.settings())), FOREIGN_SETTINGS);
    uninstall(&opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.settings())), FOREIGN_SETTINGS);
    assert_eq!(read(&bak(&h.user_mcp())), FOREIGN_USER_MCP);
}

#[test]
fn invalid_json_is_refused_unchanged() {
    let h = Home::new();
    let text = "{\n  // a comment\n  \"model\": \"opus\"\n}\n";
    write(&h.settings(), text);
    let err = install::plan_install(&opts(false, BIN), &h.env(&[])).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("settings.json"), "{msg}");
    assert_eq!(read(&h.settings()), text);
    assert!(!h.user_mcp().exists());

    // A `hooks` that is not an object is refused too.
    write(&h.settings(), "{\"hooks\": []}\n");
    assert!(install::plan_install(&opts(false, BIN), &h.env(&[])).is_err());
}

#[test]
fn unparsable_config_on_uninstall_says_remove_not_add() {
    let h = Home::new();
    for (path, text) in [
        (h.settings(), "{\"hooks\": "),
        (h.settings(), "[]\n"),
        (h.user_mcp(), "{\"mcpServers\": "),
        (h.user_mcp(), "[]\n"),
    ] {
        let _ = std::fs::remove_file(h.settings());
        let _ = std::fs::remove_file(h.user_mcp());
        write(&path, text);
        let err = install::plan_uninstall(&opts(false, BIN), &h.env(&[])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("remove agentmail's entry"), "{msg}");
        assert!(msg.contains("run the uninstall again"), "{msg}");
        assert!(!msg.contains("add this by hand"), "{msg}");
        assert!(!msg.contains("Add this by hand"), "{msg}");
        assert!(!msg.contains("PreToolUse"), "no hook snippet: {msg}");
        assert!(!msg.contains("\"command\""), "no snippet: {msg}");
        assert_eq!(read(&path), text);
    }
}

#[test]
fn apply_refuses_file_changed_since_plan() {
    let h = Home::new();
    write(&h.settings(), "{}\n");
    let changes = install::plan_install(&opts(false, BIN), &h.env(&[])).unwrap();
    write(&h.settings(), "{\"model\": \"opus\"}\n");
    assert!(install::apply(&changes).is_err());
    assert_eq!(read(&h.settings()), "{\"model\": \"opus\"}\n");
    // Nothing was written: not even the file checked before it.
    assert!(!h.user_mcp().exists());
}

#[cfg(unix)]
#[test]
fn symlinked_settings_stay_a_symlink() {
    let h = Home::new();
    let real = h.dir.path().join("dotfiles/settings.json");
    write(&real, "{\"model\": \"opus\"}\n");
    std::fs::create_dir_all(h.settings().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, h.settings()).unwrap();
    install(&opts(false, BIN), &h.env(&[]));
    assert!(
        std::fs::symlink_metadata(h.settings())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read_json(&real)["hooks"], expected_hooks(BIN));
    assert_eq!(read_json(&real)["model"], "opus");
}

#[cfg(unix)]
#[test]
fn file_mode_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    write(&h.user_mcp(), FOREIGN_USER_MCP);
    std::fs::set_permissions(h.user_mcp(), std::fs::Permissions::from_mode(0o600)).unwrap();
    install(&opts(false, BIN), &h.env(&[]));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&h.user_mcp()), 0o600);
    assert_eq!(mode(&bak(&h.user_mcp())), 0o600);
}

#[cfg(unix)]
#[test]
fn new_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    install(&opts(false, BIN), &h.env(&[]));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&h.user_mcp()), 0o600);
    assert_eq!(mode(&h.settings()), 0o600);
}

#[cfg(unix)]
#[test]
fn dangling_symlink_is_written_through() {
    let h = Home::new();
    let env = h.env(&[]);
    std::fs::create_dir_all(h.settings().parent().unwrap()).unwrap();
    // A relative link to a dotfiles directory that does not exist yet.
    std::os::unix::fs::symlink("../dotfiles/claude/settings.json", h.settings()).unwrap();
    let real = h.home().join("dotfiles/claude/settings.json");
    install(&opts(false, BIN), &env);
    let is_link = |p: &Path| {
        std::fs::symlink_metadata(p)
            .unwrap()
            .file_type()
            .is_symlink()
    };
    assert!(is_link(&h.settings()));
    assert_eq!(read_json(&real), json!({"hooks": expected_hooks(BIN)}));
    assert!(
        install::plan_install(&opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
    // Uninstall removes what it created, and leaves the link as it was.
    uninstall(&opts(false, BIN), &env);
    assert!(is_link(&h.settings()));
    assert!(!real.exists());
    assert!(!h.home().join("dotfiles").exists());
}

fn paths(changes: &[Change]) -> Vec<&Path> {
    changes.iter().map(|c| c.path.as_path()).collect()
}

fn cli(h: &Home) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_agentmail"));
    c.env("HOME", h.home())
        .env_remove("CLAUDE_CONFIG_DIR")
        .current_dir(h.proj())
        .timeout(Duration::from_secs(30));
    c
}

#[test]
fn dry_run_writes_nothing() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    let out = cli(&h)
        .args(["install", "--harness", "claude-code", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let settings = h.settings().display().to_string();
    let mcp = h.user_mcp().display().to_string();
    assert!(
        out.contains(&format!("--- {settings}\n+++ {settings}\n")),
        "{out}"
    );
    assert!(
        out.contains(&format!("--- /dev/null\n+++ {mcp}\n")),
        "{out}"
    );
    assert!(out.contains("+    \"SessionStart\": ["), "{out}");
    assert!(out.contains("@@ "), "{out}");
    // Unchanged lines far from the change are not shown.
    assert!(!out.contains("\"model\": \"opus\""), "{out}");
    assert_eq!(read(&h.settings()), FOREIGN_SETTINGS);
    assert!(!h.user_mcp().exists());
    assert!(!bak(&h.settings()).exists());
}

#[test]
fn cli_install_and_uninstall() {
    let h = Home::new();
    let out = cli(&h)
        .args(["install", "--harness", "claude-code"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(&h.settings().display().to_string()), "{out}");
    let bin = std::fs::canonicalize(env!("CARGO_BIN_EXE_agentmail")).unwrap();
    let bin = bin.to_str().unwrap();
    assert_eq!(
        read_json(&h.user_mcp())["mcpServers"]["agentmail"],
        expected_server(bin)
    );
    let out = cli(&h)
        .args(["install", "--harness", "claude-code"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("already installed"),
        "second install"
    );
    cli(&h)
        .args(["uninstall", "--harness", "claude-code"])
        .assert()
        .success();
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    assert!(!bak(&h.settings()).exists());
    assert!(!bak(&h.user_mcp()).exists());
}

#[test]
fn cli_set_default_is_kiro_only() {
    let h = Home::new();
    let out = cli(&h)
        .args(["install", "--harness", "claude-code", "--set-default"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("--set-default"), "{err}");
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
}

#[test]
fn cli_json_reports_changes() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    let out = cli(&h)
        .args(["--json", "install", "--harness", "claude-code"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let settings = h.settings().display().to_string();
    let mcp = h.user_mcp().display().to_string();
    assert_eq!(
        v,
        json!({"dry_run": false, "changes": [
            {"path": settings, "action": "update", "backup": bak(&h.settings()).display().to_string()},
            {"path": mcp, "action": "create", "backup": null},
        ], "restart": "Restart running Claude Code sessions to load it."})
    );
    let out = cli(&h)
        .args([
            "uninstall",
            "--harness",
            "claude-code",
            "--json",
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v,
        json!({"dry_run": true, "changes": [
            {"path": settings, "action": "update", "backup": null},
            {"path": mcp, "action": "delete", "backup": null},
        ], "restart": null})
    );
}

#[test]
fn cli_uninstall_dry_run_writes_nothing() {
    let h = Home::new();
    write(&h.settings(), FOREIGN_SETTINGS);
    cli(&h)
        .args(["install", "--harness", "claude-code"])
        .assert()
        .success();
    let installed = read(&h.settings());
    let out = cli(&h)
        .args(["uninstall", "--harness", "claude-code", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let settings = h.settings().display().to_string();
    let mcp = h.user_mcp().display().to_string();
    assert!(
        out.contains(&format!("--- {settings}\n+++ {settings}\n")),
        "{out}"
    );
    assert!(out.contains("-    \"SessionStart\": ["), "{out}");
    assert!(
        out.contains(&format!("--- {mcp}\n+++ /dev/null\n")),
        "{out}"
    );
    assert_eq!(read(&h.settings()), installed);
    assert!(h.user_mcp().exists());
}

#[test]
fn cli_refuses_unparsable_settings() {
    let h = Home::new();
    let text = "{\n  // a comment\n  \"model\": \"opus\"\n}\n";
    write(&h.settings(), text);
    let out = cli(&h)
        .args(["install", "--harness", "claude-code"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains("settings.json"), "{err}");
    assert!(
        err.contains("agentmail hook"),
        "the snippet to add by hand: {err}"
    );
    assert_eq!(read(&h.settings()), text);
    assert!(!h.user_mcp().exists());
    assert!(!bak(&h.settings()).exists());
}

// ---- opencode (Task 7) ----

fn oc_opts(project: bool, bin: &str) -> InstallOpts {
    InstallOpts {
        harness: Harness::Opencode,
        ..opts(project, bin)
    }
}

impl Home {
    fn oc_dir(&self) -> PathBuf {
        self.home().join(".config/opencode")
    }

    fn oc_config(&self) -> PathBuf {
        self.oc_dir().join("opencode.json")
    }

    fn oc_plugin(&self) -> PathBuf {
        self.oc_dir().join("plugins/agentmail.js")
    }
}

fn oc_server(bin: &str) -> Value {
    json!({"type": "local", "command": [bin, "mcp"], "codemode": false})
}

/// The plugin as shipped, with `bin` in place of the placeholder.
fn oc_plugin_text(bin: &str) -> String {
    let src = include_str!("../plugin/agentmail.js");
    src.replace(
        "\"__AGENTMAIL_BIN__\"",
        &serde_json::to_string(bin).unwrap(),
    )
}

/// A user's opencode.json in the V2 shape, with another server, a plugin
/// package and keys agentmail does not touch.
const OC_V2: &str = r#"{
  "$schema": "https://opencode.ai/config.json",
  "model": "anthropic/claude-sonnet",
  "mcp": {
    "timeout": 10000,
    "servers": {
      "github": {
        "type": "remote",
        "url": "https://api.githubcopilot.com/mcp/"
      }
    }
  },
  "plugins": ["opencode-foo"]
}
"#;

/// The V1 shape: servers directly under `mcp`.
const OC_V1: &str = r#"{
  "mcp": {
    "github": {
      "type": "local",
      "command": ["gh-mcp"],
      "enabled": true
    }
  },
  "theme": "dark"
}
"#;

#[test]
fn opencode_install_into_empty() {
    let h = Home::new();
    let env = h.env(&[]);
    let changes = install(&oc_opts(false, BIN), &env);
    assert_eq!(
        paths(&changes),
        [h.oc_config().as_path(), h.oc_plugin().as_path()]
    );
    assert_eq!(
        read_json(&h.oc_config()),
        json!({"mcp": {"servers": {"agentmail": oc_server(BIN)}}})
    );
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text(BIN));
}

#[test]
fn opencode_install_v2_shape() {
    let h = Home::new();
    write(&h.oc_config(), OC_V2);
    install(&oc_opts(false, BIN), &h.env(&[]));
    let text = read(&h.oc_config());
    let mut want: Value = serde_json::from_str(OC_V2).unwrap();
    want["mcp"]["servers"]["agentmail"] = oc_server(BIN);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), want);
    // Everything else is kept as written, ours added after the others.
    assert!(
        text.starts_with("{\n  \"$schema\": \"https://opencode.ai/config.json\",\n"),
        "{text}"
    );
    assert!(text.contains("\"timeout\": 10000,"), "{text}");
    assert!(
        text.ends_with("  \"plugins\": [\"opencode-foo\"]\n}\n"),
        "{text}"
    );
    assert_eq!(read(&bak(&h.oc_config())), OC_V2);
}

#[test]
fn opencode_install_v1_shape_preserved() {
    // opencode 2.0.27 reads V1 entries but drops `codemode` from them, so
    // agentmail's entry goes under `servers` (as `opencode mcp add` does
    // in a V1 file) and the user's V1 entries stay exactly as written.
    let h = Home::new();
    write(&h.oc_config(), OC_V1);
    install(&oc_opts(false, BIN), &h.env(&[]));
    let text = read(&h.oc_config());
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["mcp"]["servers"]["agentmail"], oc_server(BIN));
    assert!(v["mcp"].get("agentmail").is_none());
    assert!(
        text.contains("    \"github\": {\n      \"type\": \"local\",\n      \"command\": [\"gh-mcp\"],\n      \"enabled\": true\n    },\n"),
        "{text}"
    );
    assert_eq!(v["theme"], "dark");
    uninstall(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(read(&h.oc_config()), OC_V1);
}

#[test]
fn opencode_legacy_agentmail_entry_moves_to_servers() {
    let h = Home::new();
    write(
        &h.oc_config(),
        r#"{"mcp": {"agentmail": {"type": "local", "command": ["/old/agentmail", "mcp"], "environment": {"AGENTMAIL_DATA_DIR": "/d"}, "enabled": false}}}"#,
    );
    install(&oc_opts(false, BIN), &h.env(&[]));
    let v = read_json(&h.oc_config());
    assert!(v["mcp"].get("agentmail").is_none(), "{v}");
    assert_eq!(
        v["mcp"]["servers"]["agentmail"],
        json!({"type": "local", "command": [BIN, "mcp"], "codemode": false, "environment": {"AGENTMAIL_DATA_DIR": "/d"}, "disabled": true})
    );
    // It was agentmail's: uninstall takes it out with the `mcp` it emptied.
    uninstall(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(read_json(&h.oc_config()), json!({}));
}

#[test]
fn opencode_jsonc_with_comments_refused_unchanged() {
    let h = Home::new();
    let path = h.oc_dir().join("opencode.jsonc");
    let text =
        "{\n  // my servers\n  \"mcp\": {\"servers\": {}},\n  /* theme */ \"theme\": \"dark\"\n}\n";
    write(&path, text);
    let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("opencode.jsonc"), "{msg}");
    assert!(msg.contains("comments"), "{msg}");
    // The exact entry to paste.
    assert!(msg.contains("\"agentmail\": {"), "{msg}");
    assert!(msg.contains(&format!("\"{BIN}\"")), "{msg}");
    assert!(msg.contains("\"codemode\": false"), "{msg}");
    // opencode reloads its config by itself: no service restart, which
    // would interrupt every opencode session.
    assert!(msg.contains("run the install again"), "{msg}");
    assert!(!msg.contains("restart"), "{msg}");
    assert_eq!(read(&path), text);
    assert!(!h.oc_plugin().exists());
    assert!(!h.oc_config().exists());
    assert!(!bak(&path).exists());

    // Once the entry is pasted, install leaves the file alone and adds the
    // plugin; uninstall asks for the entry to be removed by hand.
    let pasted = text.replace(
        "{\"servers\": {}}",
        &format!("{{\"servers\": {{\"agentmail\": {{\"type\": \"local\", \"command\": [\"{BIN}\", \"mcp\"], \"codemode\": false}}}}}}"),
    );
    write(&path, &pasted);
    let changes = install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(paths(&changes), [h.oc_plugin().as_path()]);
    assert_eq!(read(&path), pasted);
    let err = install::plan_uninstall(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("by hand"), "{msg}");
    assert!(msg.contains("run the uninstall again"), "{msg}");
    assert!(!msg.contains("restart"), "{msg}");
    assert!(h.oc_plugin().exists());
    // Without the entry there is nothing to edit: the plugin goes.
    write(&path, text);
    uninstall(&oc_opts(false, BIN), &h.env(&[]));
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&path), text);
}

#[test]
fn opencode_jsonc_without_comments_is_edited() {
    let h = Home::new();
    let path = h.oc_dir().join("opencode.jsonc");
    write(
        &path,
        "{\"theme\": \"dark\", \"note\": \"see http://x // not a comment\"}\n",
    );
    install(&oc_opts(false, BIN), &h.env(&[]));
    assert!(!h.oc_config().exists(), "the existing .jsonc is the config");
    assert_eq!(
        read_json(&path)["mcp"]["servers"]["agentmail"],
        oc_server(BIN)
    );
}

#[test]
fn opencode_json_preferred_over_jsonc() {
    let h = Home::new();
    let jsonc = h.oc_dir().join("opencode.jsonc");
    write(&jsonc, "{}\n");
    write(&h.oc_config(), "{}\n");
    install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(read(&jsonc), "{}\n");
    assert_eq!(
        read_json(&h.oc_config())["mcp"]["servers"]["agentmail"],
        oc_server(BIN)
    );
}

#[test]
fn opencode_entry_in_jsonc_beside_json_is_updated_and_removed() {
    // opencode loads both files, the .jsonc last: a stale entry there
    // would override the one in opencode.json.
    let h = Home::new();
    let jsonc = h.oc_dir().join("opencode.jsonc");
    write(&h.oc_config(), "{\"theme\": \"dark\"}\n");
    write(
        &jsonc,
        "{\"mcp\": {\"servers\": {\"agentmail\": {\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"], \"environment\": {\"A\": \"1\"}}}}}\n",
    );
    let changes = install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(paths(&changes), [jsonc.as_path(), h.oc_plugin().as_path()]);
    assert_eq!(read(&h.oc_config()), "{\"theme\": \"dark\"}\n");
    let mut want = oc_server(BIN);
    want["environment"] = json!({"A": "1"});
    assert_eq!(read_json(&jsonc)["mcp"]["servers"]["agentmail"], want);

    // An entry in both files: both are updated, and uninstall removes both.
    write(
        &h.oc_config(),
        "{\"theme\": \"dark\", \"mcp\": {\"agentmail\": {\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"]}}}\n",
    );
    let changes = install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(paths(&changes), [h.oc_config().as_path()]);
    assert_eq!(
        read_json(&h.oc_config())["mcp"]["servers"]["agentmail"],
        oc_server(BIN)
    );
    let changes = uninstall(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(
        paths(&changes),
        [
            h.oc_config().as_path(),
            jsonc.as_path(),
            h.oc_plugin().as_path()
        ]
    );
    assert_eq!(read_json(&h.oc_config()), json!({"theme": "dark"}));
    assert_eq!(read_json(&jsonc), json!({}));
}

#[test]
fn opencode_commented_jsonc_beside_json_is_refused() {
    let h = Home::new();
    let jsonc = h.oc_dir().join("opencode.jsonc");
    write(&h.oc_config(), "{}\n");
    let text = "{\n  // pasted by hand\n  \"mcp\": {\"servers\": {\"agentmail\": {\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"], \"codemode\": false}}}\n}\n";
    write(&jsonc, text);
    for err in [
        install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err(),
        install::plan_uninstall(&oc_opts(false, BIN), &h.env(&[])).unwrap_err(),
    ] {
        let msg = format!("{err:#}");
        assert!(msg.contains("opencode.jsonc"), "{msg}");
        assert!(msg.contains("by hand"), "{msg}");
    }
    assert_eq!(read(&jsonc), text);
    assert_eq!(read(&h.oc_config()), "{}\n");
    assert!(!h.oc_plugin().exists());
}

#[test]
fn opencode_commented_config_with_legacy_entry_says_to_delete_it() {
    let h = Home::new();
    let path = h.oc_dir().join("opencode.jsonc");
    // The V1 entry, plus the servers entry pasted as told.
    let text = format!(
        "{{\n  // mine\n  \"mcp\": {{\n    \"agentmail\": {{\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"]}},\n    \"servers\": {{\"agentmail\": {{\"type\": \"local\", \"command\": [\"{BIN}\", \"mcp\"], \"codemode\": false}}}}\n  }}\n}}\n"
    );
    write(&path, &text);
    let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("delete the \"agentmail\" entry directly under \"mcp\""),
        "{msg}"
    );
    assert_eq!(read(&path), text);
    // With it deleted, the file is as install would leave it.
    let fixed = text.replace(
        "    \"agentmail\": {\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"]},\n",
        "",
    );
    assert_ne!(fixed, text);
    write(&path, &fixed);
    let changes = install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(paths(&changes), [h.oc_plugin().as_path()]);
    // Without a V1 entry the message does not mention one.
    let unpasted = fixed.replace(&format!("\"{BIN}\""), "\"/old/agentmail\"");
    write(&path, &unpasted);
    let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    assert!(!format!("{err:#}").contains("directly under"), "{err:#}");
}

#[test]
fn opencode_legacy_entry_beside_servers_entry_is_carried_over() {
    let h = Home::new();
    write(
        &h.oc_config(),
        r#"{"mcp": {
  "agentmail": {"type": "local", "command": ["/old/agentmail", "mcp"], "environment": {"A": "1"}, "cwd": "/w", "timeout": 5, "protocol": "x", "enabled": false},
  "servers": {"agentmail": {"type": "local", "command": ["/old/agentmail", "mcp"], "timeout": 9}}
}}"#,
    );
    install(&oc_opts(false, BIN), &h.env(&[]));
    let v = read_json(&h.oc_config());
    assert!(v["mcp"].get("agentmail").is_none(), "{v}");
    // The servers entry's own keys win; the rest come from the V1 entry.
    assert_eq!(
        v["mcp"]["servers"]["agentmail"],
        json!({"type": "local", "command": [BIN, "mcp"], "timeout": 9, "codemode": false, "environment": {"A": "1"}, "cwd": "/w", "protocol": "x", "disabled": true})
    );
}

#[test]
fn opencode_unparsable_config_on_uninstall_says_remove_not_add() {
    let h = Home::new();
    for text in ["{\"mcp\": ", "[]\n"] {
        write(&h.oc_config(), text);
        let err = install::plan_uninstall(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("opencode.json"), "{msg}");
        assert!(msg.contains("remove agentmail's entry"), "{msg}");
        assert!(msg.contains("mcp.servers.agentmail"), "{msg}");
        assert!(msg.contains("run the uninstall again"), "{msg}");
        assert!(!msg.contains("add this by hand"), "{msg}");
        assert!(!msg.contains("Add this by hand"), "{msg}");
        assert!(!msg.contains("\"codemode\""), "no snippet: {msg}");
        assert_eq!(read(&h.oc_config()), text);
        // Install still offers the entry to add.
        let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
        assert!(
            format!("{err:#}").contains("\"codemode\": false"),
            "{err:#}"
        );
    }
}

#[test]
fn opencode_trailing_commas_are_jsonc_not_a_parse_error() {
    let h = Home::new();
    let path = h.oc_dir().join("opencode.jsonc");
    let text = "{\n  \"theme\": \"dark\",\n  \"mcp\": {\"servers\": {},},\n}\n";
    write(&path, text);
    let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    let msg = format!("{err:#}");
    assert!(!msg.contains("cannot parse"), "{msg}");
    assert!(msg.contains("trailing commas"), "{msg}");
    assert!(msg.contains("\"codemode\": false"), "{msg}");
    assert_eq!(read(&path), text);
    // Pasted: left alone, the plugin installed; uninstall asks again.
    let pasted = text.replace(
        "{\"servers\": {},}",
        &format!("{{\"servers\": {{\"agentmail\": {{\"type\": \"local\", \"command\": [\"{BIN}\", \"mcp\"], \"codemode\": false,}},}},}}"),
    );
    write(&path, &pasted);
    let changes = install(&oc_opts(false, BIN), &h.env(&[]));
    assert_eq!(paths(&changes), [h.oc_plugin().as_path()]);
    assert_eq!(read(&path), pasted);
    let err = install::plan_uninstall(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    assert!(format!("{err:#}").contains("trailing commas"), "{err:#}");
}

#[test]
fn opencode_plugin_bin_substituted() {
    let h = Home::new();
    let bin = "/opt/my \"agent\" mail\\bin/agentmail";
    install(&oc_opts(false, bin), &h.env(&[]));
    let text = read(&h.oc_plugin());
    assert!(!text.contains("\"__AGENTMAIL_BIN__\""), "placeholder left");
    assert!(
        text.contains(r#"const BIN = "/opt/my \"agent\" mail\\bin/agentmail";"#),
        "{}",
        &text[..text.len().min(2000)]
    );
    // The plugin's own check of its placeholder survives.
    assert!(text.contains("\"__AGENTMAIL\" + \"_BIN__\""));
    assert_eq!(text, oc_plugin_text(bin));
    assert_eq!(
        read_json(&h.oc_config())["mcp"]["servers"]["agentmail"]["command"],
        json!([bin, "mcp"])
    );
}

#[test]
fn opencode_idempotent() {
    let h = Home::new();
    write(&h.oc_config(), OC_V2);
    let env = h.env(&[]);
    install(&oc_opts(false, BIN), &env);
    assert!(
        install::plan_install(&oc_opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
    // A new binary path updates both, keeping what the user added.
    let mut v = read_json(&h.oc_config());
    v["mcp"]["servers"]["agentmail"]["environment"] = json!({"A": "1"});
    write(&h.oc_config(), &serde_json::to_string_pretty(&v).unwrap());
    let changes = install(&oc_opts(false, "/new/agentmail"), &env);
    assert_eq!(changes.len(), 2);
    assert_eq!(
        read_json(&h.oc_config())["mcp"]["servers"]["agentmail"],
        json!({"type": "local", "command": ["/new/agentmail", "mcp"], "codemode": false, "environment": {"A": "1"}})
    );
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text("/new/agentmail"));
    assert!(
        install::plan_install(&oc_opts(false, "/new/agentmail"), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn opencode_uninstall_removes_plugin_and_entry() {
    let h = Home::new();
    write(&h.oc_config(), OC_V2);
    let env = h.env(&[]);
    install(&oc_opts(false, BIN), &env);
    let changes = uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(
        paths(&changes),
        [h.oc_config().as_path(), h.oc_plugin().as_path()]
    );
    assert_eq!(read(&h.oc_config()), OC_V2);
    assert!(!h.oc_plugin().exists());
    assert!(!marker(&h.oc_plugin()).exists());
    assert!(
        !h.oc_dir().join("plugins").exists(),
        "the plugins dir agentmail made"
    );
    assert!(
        install::plan_uninstall(&oc_opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );

    // Into an empty home and back out leaves nothing.
    let h = Home::new();
    let env = h.env(&[]);
    install(&oc_opts(false, BIN), &env);
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.home().join(".config").exists());
}

#[test]
fn opencode_foreign_plugin_file_is_refused() {
    let h = Home::new();
    let mine = "export default { id: \"mine\", setup() {} };\n";
    write(&h.oc_plugin(), mine);
    let err = install::plan_install(&oc_opts(false, BIN), &h.env(&[])).unwrap_err();
    assert!(format!("{err:#}").contains("agentmail.js"), "{err:#}");
    assert!(!h.oc_config().exists());
    // Uninstall leaves it alone.
    assert!(
        install::plan_uninstall(&oc_opts(false, BIN), &h.env(&[]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(read(&h.oc_plugin()), mine);
}

#[test]
fn opencode_respects_xdg() {
    let h = Home::new();
    let xdg = h.dir.path().join("xdg");
    let env = h.env(&[("XDG_CONFIG_HOME", xdg.to_str().unwrap())]);
    let changes = install(&oc_opts(false, BIN), &env);
    assert_eq!(
        paths(&changes),
        [
            xdg.join("opencode/opencode.json").as_path(),
            xdg.join("opencode/plugins/agentmail.js").as_path()
        ]
    );
    assert!(!h.oc_dir().exists());
    // A relative value is taken from the current directory, as opencode
    // does.
    let changes =
        install::plan_install(&oc_opts(false, BIN), &h.env(&[("XDG_CONFIG_HOME", "rel")])).unwrap();
    let rel = h.env(&[]).cwd.join("rel");
    assert_eq!(
        paths(&changes),
        [
            rel.join("opencode/opencode.json").as_path(),
            rel.join("opencode/plugins/agentmail.js").as_path()
        ]
    );
    // An empty variable is unset.
    let changes =
        install::plan_install(&oc_opts(false, BIN), &h.env(&[("XDG_CONFIG_HOME", "")])).unwrap();
    assert_eq!(
        paths(&changes),
        [h.oc_config().as_path(), h.oc_plugin().as_path()]
    );
}

#[test]
fn opencode_project_scope() {
    let h = Home::new();
    let env = h.env(&[]);
    let changes = install(&oc_opts(true, BIN), &env);
    let cfg = h.proj().join("opencode.json");
    let plugin = h.proj().join(".opencode/plugins/agentmail.js");
    assert_eq!(paths(&changes), [cfg.as_path(), plugin.as_path()]);
    assert_eq!(
        read_json(&cfg)["mcp"]["servers"]["agentmail"],
        oc_server(BIN)
    );
    assert_eq!(read(&plugin), oc_plugin_text(BIN));
    assert!(!h.oc_dir().exists());
    uninstall(&oc_opts(true, BIN), &env);
    assert!(!cfg.exists());
    assert!(!h.proj().join(".opencode").exists());
}

#[test]
fn opencode_project_dot_opencode_entry_updated_and_removed() {
    // opencode merges ./.opencode/opencode.json(c) after ./opencode.json:
    // a stale agentmail entry there would override the one install writes.
    let h = Home::new();
    let env = h.env(&[]);
    let direct = h.proj().join("opencode.json");
    let dot = h.proj().join(".opencode/opencode.json");
    let dotc = h.proj().join(".opencode/opencode.jsonc");
    write(&direct, "{\n  \"theme\": \"dark\"\n}\n");
    write(&dotc, "{\n  // mine\n  \"model\": \"x\"\n}\n");
    // No entry anywhere: the new one goes to the direct config, even with
    // .opencode configs there.
    write(&dot, "{\n  \"theme\": \"light\"\n}\n");
    let changes = install(&oc_opts(true, BIN), &env);
    let plugin = h.proj().join(".opencode/plugins/agentmail.js");
    assert_eq!(paths(&changes), [direct.as_path(), plugin.as_path()]);
    assert_eq!(read_json(&dot), json!({"theme": "light"}));
    uninstall(&oc_opts(true, BIN), &env);
    assert_eq!(read_json(&direct), json!({"theme": "dark"}));

    // A stale entry (old shape, old binary) in .opencode/opencode.json is
    // updated there.
    write(
        &dot,
        "{\n  \"mcp\": {\n    \"agentmail\": {\"type\": \"local\", \"command\": [\"/old/agentmail\", \"mcp\"]}\n  }\n}\n",
    );
    let changes = install(&oc_opts(true, BIN), &env);
    assert_eq!(paths(&changes), [dot.as_path(), plugin.as_path()]);
    assert_eq!(
        read_json(&dot),
        json!({"mcp": {"servers": {"agentmail": oc_server(BIN)}}})
    );
    assert_eq!(read_json(&direct), json!({"theme": "dark"}));
    // Uninstall removes it from every config, the .opencode ones included.
    write(
        &direct,
        &format!(
            "{{\"theme\": \"dark\", \"mcp\": {{\"servers\": {{\"agentmail\": {}}}}}}}\n",
            oc_server(BIN)
        ),
    );
    uninstall(&oc_opts(true, BIN), &env);
    assert!(!read_json(&dot).to_string().contains("agentmail"));
    assert_eq!(read_json(&direct), json!({"theme": "dark"}));
    assert!(!plugin.exists());

    // The same JSONC rule: a commented .opencode/opencode.jsonc with an
    // entry is refused and nothing changes.
    let commented = format!(
        "{{\n  // mine\n  \"mcp\": {{\"servers\": {{\"agentmail\": {}}}}}\n}}\n",
        oc_server("/old/agentmail")
    );
    write(&dotc, &commented);
    let before_direct = read(&direct);
    let err = install::plan_install(&oc_opts(true, BIN), &env).unwrap_err();
    assert!(format!("{err:#}").contains("opencode.jsonc"), "{err:#}");
    let err = install::plan_uninstall(&oc_opts(true, BIN), &env).unwrap_err();
    assert!(format!("{err:#}").contains("by hand"), "{err:#}");
    assert_eq!(read(&dotc), commented);
    assert_eq!(read(&direct), before_direct);
    assert!(!plugin.exists());
}

#[test]
fn cli_opencode_install_and_uninstall() {
    let h = Home::new();
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(&h.oc_plugin().display().to_string()), "{out}");
    assert!(
        out.contains("agentmail is installed for opencode."),
        "{out}"
    );
    // opencode's background service watches its config and plugin files
    // and reloads them by itself, so nothing needs restarting (and
    // restarting the service would interrupt every opencode session).
    assert!(!out.contains("restart"), "{out}");
    assert!(!out.contains("Restart"), "{out}");
    let bin = std::fs::canonicalize(env!("CARGO_BIN_EXE_agentmail")).unwrap();
    let bin = bin.to_str().unwrap();
    assert_eq!(
        read_json(&h.oc_config())["mcp"]["servers"]["agentmail"],
        oc_server(bin)
    );
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text(bin));
    // Only the plugin file changes: still no restart line.
    std::fs::remove_file(h.oc_plugin()).unwrap();
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(&h.oc_plugin().display().to_string()), "{out}");
    assert!(!out.contains(&h.oc_config().display().to_string()), "{out}");
    assert!(!out.contains("restart"), "{out}");
    assert!(!out.contains("Restart"), "{out}");
    std::fs::remove_file(h.oc_plugin()).unwrap();
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["--json", "install", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["restart"], Value::Null, "{v}");
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["--json", "uninstall", "--harness", "opencode", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["restart"], Value::Null, "{v}");
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["uninstall", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("agentmail is removed from opencode."), "{out}");
    assert!(!out.contains("restart"), "{out}");
    assert!(!out.contains("Restart"), "{out}");
    assert!(!h.oc_config().exists());
    assert!(!h.oc_plugin().exists());
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["--json", "install", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    // A config change still needs no restart.
    assert!(
        v["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == h.oc_config().display().to_string()),
        "{v}"
    );
    assert_eq!(v["restart"], Value::Null, "{v}");
}

#[test]
fn opencode_symlinked_plugin_is_never_written_through() {
    let h = Home::new();
    let env = h.env(&[]);
    // A dev or dotfiles setup: the plugin is a link to a copy elsewhere.
    let src = h.dir.path().join("src/agentmail.js");
    let shipped = include_str!("../plugin/agentmail.js");
    write(&src, shipped);
    std::fs::create_dir_all(h.oc_plugin().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&src, h.oc_plugin()).unwrap();

    let err = install::plan_install(&oc_opts(false, BIN), &env).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("symbolic link"), "{msg}");
    assert!(msg.contains("agentmail.js"), "{msg}");
    assert!(!h.oc_config().exists());
    assert_eq!(read(&src), shipped);
    assert!(!bak(&h.oc_plugin()).exists());

    // An older install wrote through the link and saved its original:
    // uninstall removes only the link, keeping its target and that backup.
    write(&bak(&h.oc_plugin()), shipped);
    let changes = uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(paths(&changes), [h.oc_plugin().as_path()]);
    assert!(std::fs::symlink_metadata(h.oc_plugin()).is_err());
    assert_eq!(read(&src), shipped);
    assert_eq!(read(&bak(&h.oc_plugin())), shipped);
}

#[test]
fn opencode_plugin_agentmail_did_not_create_is_restored_or_removed() {
    let h = Home::new();
    let env = h.env(&[]);
    // Copied by hand: no marker, no backup. Uninstall removes the file.
    write(&h.oc_plugin(), &oc_plugin_text("/old/agentmail"));
    let changes = uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(paths(&changes), [h.oc_plugin().as_path()]);
    assert!(!h.oc_plugin().exists());

    // A backup of a file that is not agentmail's is put back.
    let mine = "export default { id: \"mine\", setup() {} };\n";
    write(&h.oc_plugin(), &oc_plugin_text("/old/agentmail"));
    write(&bak(&h.oc_plugin()), mine);
    uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(read(&h.oc_plugin()), mine);
}

#[test]
fn opencode_plugin_in_singular_plugin_dir_is_removed() {
    // opencode also loads `plugin/`: a copy there would run beside the
    // installed one and surface every message twice.
    let h = Home::new();
    let env = h.env(&[]);
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    write(&legacy, &oc_plugin_text("/old/agentmail"));
    let changes = install(&oc_opts(false, BIN), &env);
    assert_eq!(
        paths(&changes),
        [
            h.oc_config().as_path(),
            h.oc_plugin().as_path(),
            legacy.as_path()
        ]
    );
    assert!(!legacy.exists());
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text(BIN));

    write(&legacy, &oc_plugin_text("/old/agentmail"));
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!legacy.exists());
    assert!(!h.oc_plugin().exists());

    // One that is not agentmail's is left alone.
    let mine = "export default { id: \"mine\", setup() {} };\n";
    write(&legacy, mine);
    let changes = install(&oc_opts(false, BIN), &env);
    assert!(!paths(&changes).contains(&legacy.as_path()));
    uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(read(&legacy), mine);

    // At project scope too.
    let legacy = h.proj().join(".opencode/plugin/agentmail.js");
    write(&legacy, &oc_plugin_text("/old/agentmail"));
    install(&oc_opts(true, BIN), &env);
    assert!(!legacy.exists());
}
