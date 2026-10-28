//! Harness Tasks 5, 7, 8 and 9: `agentmail install` / `uninstall` for Claude Code
//! opencode, kiro, and the systemd user service.
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
        kiro_wake: false,
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
    // A reinstall keeps no backup of the file agentmail itself created (it
    // had no original); uninstall still sees that agentmail created it.
    let h = Home::new();
    let env = h.env(&[]);
    install(&opts(false, "/old/agentmail"), &env);
    install(&opts(false, BIN), &env);
    assert!(!bak(&h.settings()).exists());
    assert!(!bak(&h.user_mcp()).exists());
    assert!(marker(&h.settings()).exists());
    uninstall(&opts(false, BIN), &env);
    assert!(!h.settings().exists());
    assert!(!h.user_mcp().exists());
    // Nothing is left behind.
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
            {"path": settings, "action": "update", "backup": bak(&h.settings()).display().to_string(), "saved": null},
            {"path": mcp, "action": "create", "backup": null, "saved": null},
        ], "restart": "Restart running Claude Code sessions to load it.", "notes": []})
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
            {"path": settings, "action": "update", "backup": null, "saved": null},
            {"path": mcp, "action": "delete", "backup": null, "saved": null},
        ], "restart": null, "notes": []})
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

/// The plugin as shipped, with `bin` in place of the placeholder, signed.
fn oc_plugin_text(bin: &str) -> String {
    let src = include_str!("../plugin/agentmail.js");
    oc_sign(src.replace(
        "\"__AGENTMAIL_BIN__\"",
        &serde_json::to_string(bin).unwrap(),
    ))
}

/// `body` with the digest line agentmail ends its plugin with: FNV-1a (64
/// bits) of the text above it.
fn oc_sign(mut body: String) -> String {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let digest = body.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    body.push_str(&format!("// agentmail-digest: fnv1a64:{digest:016x}\n"));
    body
}

/// The plugin an older agentmail installed for `bin`: other text, signed
/// as every version signs it.
fn oc_older_plugin(bin: &str, version: u32) -> String {
    let text = oc_plugin_text(bin);
    let body = &text[..text.trim_end_matches('\n').rfind('\n').unwrap() + 1];
    oc_sign(body.replacen('\n', &format!("\n// shipped by agentmail 0.{version}\n"), 1))
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
fn opencode_symlinked_plugin_in_singular_dir_is_refused() {
    // A dev link at plugin/agentmail.js shadows the installed plugin, but
    // it is the user's setup: install refuses rather than removing it.
    let h = Home::new();
    let env = h.env(&[]);
    let src = h.dir.path().join("src/agentmail.js");
    let text = oc_plugin_text("/home/dev/agentmail");
    write(&src, &text);
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&src, &legacy).unwrap();

    let err = install::plan_install(&oc_opts(false, BIN), &env).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("symbolic link"), "{msg}");
    assert!(msg.contains("shadows"), "{msg}");
    assert!(msg.contains("Remove the link"), "{msg}");
    assert!(msg.contains("plugin/agentmail.js"), "{msg}");
    assert!(
        std::fs::symlink_metadata(&legacy)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!h.oc_config().exists());
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&src), text);

    // Uninstall still removes only the link, never its target.
    let changes = uninstall(&oc_opts(false, BIN), &env);
    assert!(paths(&changes).contains(&legacy.as_path()));
    assert!(std::fs::symlink_metadata(&legacy).is_err());
    assert_eq!(read(&src), text);

    // A link to a file that is not agentmail's plugin is left alone.
    let other = h.dir.path().join("src/other.js");
    write(&other, "export default {}\n");
    std::os::unix::fs::symlink(&other, &legacy).unwrap();
    install(&oc_opts(false, BIN), &env);
    assert!(std::fs::symlink_metadata(&legacy).is_ok());
}

#[test]
fn opencode_plugin_agentmail_created_then_symlinked_keeps_link_target() {
    // agentmail created the plugin (its marker says so), then the user
    // swapped it for a link to their own copy: uninstall removes only the
    // link and the marker, never the file the link points to.
    let h = Home::new();
    let env = h.env(&[]);
    install(&oc_opts(false, BIN), &env);
    assert!(marker(&h.oc_plugin()).exists());
    let src = h.dir.path().join("src/agentmail.js");
    let text = oc_plugin_text("/home/dev/agentmail");
    write(&src, &text);
    std::fs::remove_file(h.oc_plugin()).unwrap();
    std::os::unix::fs::symlink(&src, h.oc_plugin()).unwrap();

    uninstall(&oc_opts(false, BIN), &env);
    assert!(std::fs::symlink_metadata(h.oc_plugin()).is_err());
    assert!(!marker(&h.oc_plugin()).exists());
    assert_eq!(read(&src), text);
}

#[test]
fn opencode_unreadable_plugin_in_other_scope_is_no_plugin() {
    // The notes are advisory: a path in the other scope that cannot be
    // read (a directory, a file that is not UTF-8) is no agentmail plugin
    // there, and never stops the install or uninstall.
    let h = Home::new();
    let env = h.env(&[]);
    std::fs::create_dir_all(h.proj().join(".opencode/plugins/agentmail.js")).unwrap();
    let odd = h.proj().join(".opencode/plugin/agentmail.js");
    std::fs::create_dir_all(odd.parent().unwrap()).unwrap();
    std::fs::write(&odd, b"// agentmail for opencode\n\xff\xfe\n").unwrap();
    assert!(
        install::notes(&oc_opts(false, BIN), &env, true)
            .unwrap()
            .is_empty()
    );
    install(&oc_opts(false, BIN), &env);
    assert!(h.oc_plugin().exists());
    assert!(
        install::notes(&oc_opts(false, BIN), &env, false)
            .unwrap()
            .is_empty()
    );
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());

    // Likewise the user scope's, for a project install.
    std::fs::create_dir_all(h.oc_plugin()).unwrap();
    assert!(
        install::notes(&oc_opts(true, BIN), &env, true)
            .unwrap()
            .is_empty()
    );
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
        set(&h.oc_plugin(), 0o600).unwrap();
        set(&bak(&h.oc_plugin()), 0o644).unwrap();
    }
    uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(read(&h.oc_plugin()), mine);
    // The backup is used up, and the file is as it was, mode and all.
    assert!(!bak(&h.oc_plugin()).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(h.oc_plugin())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }
    // Later runs find nothing of agentmail's there.
    let err = install::plan_install(&oc_opts(false, BIN), &env).unwrap_err();
    assert!(err.to_string().contains("not agentmail's plugin"), "{err}");
    assert!(uninstall(&oc_opts(false, BIN), &env).is_empty());

    // An edited agentmail plugin takes the place of the backup put back.
    let edited = oc_edited_plugin("/old/agentmail");
    write(&bak(&h.oc_plugin()), mine);
    write(&h.oc_plugin(), &edited);
    let changes = uninstall(&oc_opts(false, BIN), &env);
    let c = changes.iter().find(|c| c.path == h.oc_plugin()).unwrap();
    assert_eq!(c.save.as_deref(), Some(bak(&h.oc_plugin()).as_path()));
    assert_eq!(read(&h.oc_plugin()), mine);
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
}

#[test]
fn opencode_plugin_in_singular_plugin_dir_is_removed() {
    // opencode also loads `plugin/`, before `plugins/`, and runs only the
    // first plugin of each id: a copy there would shadow the installed one,
    // which would never run.
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

/// An agentmail plugin the user edited by hand.
fn oc_edited_plugin(bin: &str) -> String {
    oc_plugin_text(bin).replacen('\n', "\n// my own tweak\n", 1)
}

#[test]
fn opencode_edited_plugin_agentmail_did_not_create_is_kept_on_install() {
    // A hand-placed `plugin/agentmail.js` with the user's own edits: install
    // removes it (it would shadow the installed plugin) but keeps its text
    // as its backup, and says so.
    let h = Home::new();
    let env = h.env(&[]);
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    let edited = oc_edited_plugin("/old/agentmail");
    write(&legacy, &edited);
    let changes = install(&oc_opts(false, BIN), &env);
    assert!(paths(&changes).contains(&legacy.as_path()));
    assert!(!legacy.exists());
    assert_eq!(read(&bak(&legacy)), edited);
    // Nothing more to do, and uninstall keeps the copy.
    assert!(
        install::plan_install(&oc_opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
    uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(read(&bak(&legacy)), edited);

    // A plugin agentmail would write goes with no copy.
    let h = Home::new();
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    write(&legacy, &oc_plugin_text("/old/agentmail"));
    install(&oc_opts(false, BIN), &h.env(&[]));
    assert!(!legacy.exists());
    assert!(!bak(&legacy).exists());
}

#[test]
fn opencode_edited_plugin_is_kept_on_uninstall() {
    let h = Home::new();
    let env = h.env(&[]);
    // Copied by hand and edited: uninstall keeps its text as the backup.
    let edited = oc_edited_plugin("/old/agentmail");
    write(&h.oc_plugin(), &edited);
    let changes = uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(
        changes[0].save.as_deref(),
        Some(bak(&h.oc_plugin()).as_path())
    );
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&bak(&h.oc_plugin())), edited);

    // A new install keeps that copy (not the record of an earlier file),
    // and its uninstall too.
    install(&oc_opts(false, BIN), &env);
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text(BIN));
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&bak(&h.oc_plugin())), edited);

    // Edited again with that copy there: saved beside it.
    let again = oc_edited_plugin("/other/agentmail");
    write(&h.oc_plugin(), &again);
    let changes = uninstall(&oc_opts(false, BIN), &env);
    let spare = PathBuf::from(format!("{}.1", bak(&h.oc_plugin()).display()));
    assert_eq!(changes[0].save.as_deref(), Some(spare.as_path()));
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
    assert_eq!(read(&spare), again);

    // Installed by agentmail, then edited: kept too.
    let h = Home::new();
    let env = h.env(&[]);
    install(&oc_opts(false, BIN), &env);
    let edited = oc_edited_plugin(BIN);
    write(&h.oc_plugin(), &edited);
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
    assert!(!marker(&h.oc_plugin()).exists());
}

#[test]
fn opencode_upgrade_of_a_plugin_agentmail_created_leaves_no_backup() {
    // Each agentmail version ships its own plugin text: replacing an older
    // version's (untouched) plugin keeps no copy of it, and uninstall
    // leaves nothing behind, not even the `plugins/` directory agentmail
    // created.
    let h = Home::new();
    let env = h.env(&[]);
    let plugins = h.oc_plugin().parent().unwrap().to_path_buf();
    install(&oc_opts(false, BIN), &env);
    assert!(marker(&h.oc_plugin()).exists());
    for version in [1, 2] {
        write(&h.oc_plugin(), &oc_older_plugin(BIN, version));
        let changes = install(&oc_opts(false, BIN), &env);
        let plugin = changes.iter().find(|c| c.path == h.oc_plugin()).unwrap();
        assert_eq!(plugin.save, None);
        assert_eq!(read(&h.oc_plugin()), oc_plugin_text(BIN));
        assert!(!bak(&h.oc_plugin()).exists());
    }
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());
    assert!(!bak(&h.oc_plugin()).exists());
    assert!(!plugins.exists(), "{}", plugins.display());

    // An older version's plugin agentmail did not create: its backup holds
    // that plugin, no edits are kept beside it on upgrade, and uninstall
    // removes both.
    let h = Home::new();
    let env = h.env(&[]);
    write(&h.oc_plugin(), &oc_older_plugin("/old/agentmail", 1));
    install(&oc_opts(false, BIN), &env);
    write(&h.oc_plugin(), &oc_older_plugin(BIN, 2));
    let changes = install(&oc_opts(false, BIN), &env);
    let plugin = changes.iter().find(|c| c.path == h.oc_plugin()).unwrap();
    assert_eq!(plugin.save, None);
    let spare = PathBuf::from(format!("{}.1", bak(&h.oc_plugin()).display()));
    assert!(!spare.exists());
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());
    assert!(!bak(&h.oc_plugin()).exists());
}

#[test]
fn opencode_edited_plugin_agentmail_created_is_saved_on_upgrade() {
    // With no backup (agentmail created the file), an edited plugin that
    // install replaces is saved as the backup, which uninstall keeps.
    let h = Home::new();
    let env = h.env(&[]);
    install(&oc_opts(false, "/old/agentmail"), &env);
    let edited = oc_edited_plugin("/old/agentmail");
    write(&h.oc_plugin(), &edited);
    let changes = install(&oc_opts(false, BIN), &env);
    let plugin = changes.iter().find(|c| c.path == h.oc_plugin()).unwrap();
    assert_eq!(plugin.save.as_deref(), Some(bak(&h.oc_plugin()).as_path()));
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
    uninstall(&oc_opts(false, BIN), &env);
    assert!(!h.oc_plugin().exists());
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
}

#[test]
fn opencode_edited_plugin_is_kept_when_its_backup_is_restored() {
    let h = Home::new();
    let env = h.env(&[]);
    let mine = "export default { id: \"mine\", setup() {} };\n";
    let edited = oc_edited_plugin("/old/agentmail");
    write(&h.oc_plugin(), &edited);
    write(&bak(&h.oc_plugin()), mine);
    uninstall(&oc_opts(false, BIN), &env);
    assert_eq!(read(&h.oc_plugin()), mine);
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
}

#[test]
fn opencode_edited_plugin_is_kept_on_reinstall() {
    // A backup is written only where there is none: with one there, an
    // edited plugin that install replaces is saved beside it.
    let h = Home::new();
    let env = h.env(&[]);
    let first = oc_edited_plugin("/first/agentmail");
    write(&h.oc_plugin(), &first);
    install(&oc_opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.oc_plugin())), first);
    let second = oc_edited_plugin("/second/agentmail");
    write(&h.oc_plugin(), &second);
    install(&oc_opts(false, BIN), &env);
    assert_eq!(read(&h.oc_plugin()), oc_plugin_text(BIN));
    assert_eq!(read(&bak(&h.oc_plugin())), first);
    let spare = PathBuf::from(format!("{}.1", bak(&h.oc_plugin()).display()));
    assert_eq!(read(&spare), second);

    // An edited plugin over a backup agentmail would write replaces it.
    let h = Home::new();
    let env = h.env(&[]);
    write(&bak(&h.oc_plugin()), &oc_plugin_text("/old/agentmail"));
    let edited = oc_edited_plugin("/old/agentmail");
    write(&h.oc_plugin(), &edited);
    install(&oc_opts(false, BIN), &env);
    assert_eq!(read(&bak(&h.oc_plugin())), edited);
}

#[test]
fn cli_opencode_upgrade_keeps_no_original_it_did_not_find() {
    // agentmail created the plugin: replacing an older version's keeps no
    // "original", and says none.
    let h = Home::new();
    let plugin = h.oc_plugin();
    let run = |args: &[&str]| {
        let out = cli(&h)
            .env_remove("XDG_CONFIG_HOME")
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out).unwrap()
    };
    run(&["install", "--harness", "opencode"]);
    write(&plugin, &oc_older_plugin("/old/agentmail", 1));
    let out = run(&["install", "--harness", "opencode"]);
    assert!(
        out.contains(&format!("updated {}\n", plugin.display())),
        "{out}"
    );
    assert!(!out.contains("kept as"), "{out}");
    write(&plugin, &oc_older_plugin("/old/agentmail", 2));
    let out = run(&["--json", "install", "--harness", "opencode"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    let entry = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == plugin.display().to_string())
        .unwrap();
    assert_eq!(entry["backup"], Value::Null, "{out}");
    assert_eq!(entry["saved"], Value::Null, "{out}");
    run(&["uninstall", "--harness", "opencode"]);
    assert!(!plugin.exists());
    assert!(!bak(&plugin).exists());
    assert!(!plugin.parent().unwrap().exists());
}

#[test]
fn cli_opencode_says_a_plugin_was_restored() {
    // Uninstall puts back a backup that is not agentmail's plugin: it says
    // so, not that an original was kept.
    let h = Home::new();
    let plugin = h.oc_plugin();
    let mine = "export default { id: \"mine\", setup() {} };\n";
    let run = |args: &[&str]| {
        let out = cli(&h)
            .env_remove("XDG_CONFIG_HOME")
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out).unwrap()
    };
    let setup = || {
        run(&["install", "--harness", "opencode"]);
        write(&bak(&plugin), mine);
        std::fs::remove_file(marker(&plugin)).unwrap();
    };
    setup();
    let out = run(&["uninstall", "--harness", "opencode"]);
    assert!(
        out.contains(&format!("restored {} from its backup\n", plugin.display())),
        "{out}"
    );
    assert!(!out.contains("kept as"), "{out}");
    assert_eq!(read(&plugin), mine);
    assert!(!bak(&plugin).exists());

    std::fs::remove_file(&plugin).unwrap();
    setup();
    let out = run(&["--json", "uninstall", "--harness", "opencode"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    let entry = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == plugin.display().to_string())
        .unwrap();
    assert_eq!(entry["action"], "restore", "{out}");
    assert_eq!(entry["backup"], Value::Null, "{out}");
    assert_eq!(entry["saved"], Value::Null, "{out}");
    assert!(!bak(&plugin).exists());
}

#[test]
fn cli_opencode_says_where_an_edited_plugin_went() {
    let h = Home::new();
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    write(&legacy, &oc_edited_plugin("/old/agentmail"));
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "--harness", "opencode", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let kept = bak(&legacy).display().to_string();
    assert!(out.contains(&format!("would be kept as {kept}")), "{out}");
    assert!(legacy.exists());
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains(&format!(
            "removed {} (edited copy kept as {kept})",
            legacy.display()
        )),
        "{out}"
    );
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["--json", "uninstall", "--harness", "opencode"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["saved"].is_null()),
        "{v}"
    );
    assert!(bak(&legacy).exists());
}

#[test]
fn opencode_plugin_in_other_scope_is_noted() {
    // opencode runs one plugin per id, the first loaded, and loads the user
    // scope before the project: the user's agentmail plugin shadows the
    // project's.
    let h = Home::new();
    let env = h.env(&[]);
    let dev = "/home/dev/agentmail/target/debug/agentmail";
    let project = h.proj().join(".opencode/plugins/agentmail.js");
    let user = h.oc_plugin().display().to_string();
    // Nothing in the other scope: no note.
    assert!(
        install::notes(&oc_opts(true, dev), &env, true)
            .unwrap()
            .is_empty()
    );
    install(&oc_opts(false, BIN), &env);
    // The same binary in both scopes: no note.
    assert!(
        install::notes(&oc_opts(true, BIN), &env, true)
            .unwrap()
            .is_empty()
    );
    // A project install for another binary: the user's plugin wins.
    let notes = install::notes(&oc_opts(true, dev), &env, true).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].starts_with("note: "), "{notes:?}");
    assert!(!notes[0].contains('\n'), "{notes:?}");
    assert!(notes[0].contains(&user), "{notes:?}");
    assert!(notes[0].contains(BIN), "{notes:?}");
    install(&oc_opts(true, dev), &env);
    // A user install for another binary than the project's: it wins there.
    let notes = install::notes(&oc_opts(false, "/usr/bin/agentmail"), &env, true).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&project.display().to_string()),
        "{notes:?}"
    );
    assert!(notes[0].contains(dev), "{notes:?}");
    // Uninstalling the user scope hands over to the project's plugin.
    let notes = install::notes(&oc_opts(false, BIN), &env, false).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&project.display().to_string()),
        "{notes:?}"
    );
    assert!(notes[0].contains(dev), "{notes:?}");
    // Uninstalling the project scope changes nothing that runs.
    assert!(
        install::notes(&oc_opts(true, dev), &env, false)
            .unwrap()
            .is_empty()
    );
    uninstall(&oc_opts(false, BIN), &env);
    // With no user plugin left, uninstalling the user scope notes nothing.
    assert!(
        install::notes(&oc_opts(false, BIN), &env, false)
            .unwrap()
            .is_empty()
    );
    // The other scope's `plugin/` counts too, and a plugin there whose
    // binary cannot be read differs from any.
    let legacy = h.oc_dir().join("plugin/agentmail.js");
    write(
        &legacy,
        "// agentmail for opencode (an old one)\nexport default {};\n",
    );
    let notes = install::notes(&oc_opts(true, dev), &env, true).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&legacy.display().to_string()),
        "{notes:?}"
    );
    // Not for Claude Code.
    assert!(
        install::notes(&opts(true, BIN), &env, true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cli_opencode_notes_plugin_in_other_scope() {
    let h = Home::new();
    let user = h.oc_plugin();
    write(&user, &oc_plugin_text("/somewhere/else/agentmail"));
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "--harness", "opencode", "--project"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let note = out
        .lines()
        .find(|l| l.starts_with("note: "))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(note.contains(&user.display().to_string()), "{out}");
    assert!(note.contains("/somewhere/else/agentmail"), "{out}");
    let out = cli(&h)
        .env_remove("XDG_CONFIG_HOME")
        .args(["--json", "install", "--harness", "opencode", "--project"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let notes = v["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{v}");
    // The "note: " prefix is the text form's.
    let note = notes[0].as_str().unwrap();
    assert!(
        !note.starts_with("note: ") && note.contains("opencode"),
        "{v}"
    );
}

// kiro-cli (V3).

fn kiro_opts(project: bool, bin: &str) -> InstallOpts {
    InstallOpts {
        harness: Harness::Kiro,
        ..opts(project, bin)
    }
}

impl Home {
    fn kiro_dir(&self) -> PathBuf {
        self.home().join(".kiro")
    }

    fn kiro_mcp(&self) -> PathBuf {
        self.kiro_dir().join("settings/mcp.json")
    }

    fn kiro_hooks(&self) -> PathBuf {
        self.kiro_dir().join("hooks/agentmail.json")
    }
}

fn kiro_hook(bin: &str, trigger: &str, event: &str) -> Value {
    json!({
        "name": format!("agentmail-{event}"),
        "trigger": trigger,
        "action": {"type": "command", "command": format!("{bin} hook {event} --harness kiro")},
        "timeout": 10
    })
}

/// The hooks file agentmail installs for kiro, with `--kiro-wake` or not.
fn kiro_expected_hooks(bin: &str, wake: bool) -> Value {
    let pre = json!({
        "name": "agentmail-pre-tool",
        "trigger": "PreToolUse",
        "matcher": ".*",
        "action": {"type": "command", "command": format!("{bin} hook pre-tool --harness kiro")},
        "timeout": 10
    });
    let stop = match wake {
        false => kiro_hook(bin, "Stop", "stop"),
        true => json!({
            "name": "agentmail-stop",
            "trigger": "Stop",
            "action": {"type": "command", "command": format!("{bin} hook stop --harness kiro --wake")},
            "timeout": 0
        }),
    };
    json!({
        "version": "v1",
        "hooks": [
            kiro_hook(bin, "SessionStart", "session-start"),
            kiro_hook(bin, "UserPromptSubmit", "prompt"),
            pre,
            stop,
        ]
    })
}

fn kiro_server(bin: &str) -> Value {
    json!({"command": bin, "args": ["mcp"]})
}

/// A kiro `settings/mcp.json` with someone else's servers.
const FOREIGN_KIRO_MCP: &str = r#"{
    "mcpServers": {
      "gmail": {
        "type": "http",
        "url": "http://127.0.0.1:3016/mcp",
        "disabled": false
      }
    }
  }
"#;

#[test]
fn kiro_install_mcp_merge() {
    let h = Home::new();
    write(&h.kiro_mcp(), FOREIGN_KIRO_MCP);
    let env = h.env(&[]);
    install(&kiro_opts(false, BIN), &env);
    let v = read_json(&h.kiro_mcp());
    assert_eq!(v["mcpServers"]["agentmail"], kiro_server(BIN), "{v}");
    assert_eq!(v["mcpServers"]["gmail"]["url"], "http://127.0.0.1:3016/mcp");
    // Foreign bytes are kept as written: only agentmail's entry is new.
    let text = read(&h.kiro_mcp());
    assert!(
        text.starts_with("{\n    \"mcpServers\": {\n      \"gmail\": {"),
        "{text}"
    );
    assert_eq!(read(&bak(&h.kiro_mcp())), FOREIGN_KIRO_MCP);
}

#[test]
fn kiro_install_hooks_file() {
    let h = Home::new();
    let env = h.env(&[]);
    let changes = install(&kiro_opts(false, BIN), &env);
    assert_eq!(paths(&changes), [h.kiro_hooks().as_path(), &h.kiro_mcp()]);
    assert_eq!(read_json(&h.kiro_hooks()), kiro_expected_hooks(BIN, false));
    assert!(marker(&h.kiro_hooks()).exists());
    assert!(!bak(&h.kiro_hooks()).exists());
}

#[test]
fn kiro_wake_installs_the_waiting_stop_hook() {
    let h = Home::new();
    let env = h.env(&[]);
    let wake = InstallOpts {
        kiro_wake: true,
        ..kiro_opts(false, BIN)
    };
    install(&wake, &env);
    assert_eq!(read_json(&h.kiro_hooks()), kiro_expected_hooks(BIN, true));
    // The MCP server registers kiro agents with `wake` on by default.
    let mut server = kiro_server(BIN);
    server["env"] = json!({"AGENTMAIL_KIRO_WAKE": "1"});
    assert_eq!(read_json(&h.kiro_mcp())["mcpServers"]["agentmail"], server);
    assert!(install::plan_install(&wake, &env).unwrap().is_empty());

    // Installing again without it takes waking out, keeping the user's env.
    let mut v = read_json(&h.kiro_mcp());
    v["mcpServers"]["agentmail"]["env"]["RUST_LOG"] = json!("debug");
    write(&h.kiro_mcp(), &serde_json::to_string_pretty(&v).unwrap());
    install(&kiro_opts(false, BIN), &env);
    assert_eq!(read_json(&h.kiro_hooks()), kiro_expected_hooks(BIN, false));
    assert_eq!(
        read_json(&h.kiro_mcp())["mcpServers"]["agentmail"]["env"],
        json!({"RUST_LOG": "debug"})
    );
    // Without anything else in it, `env` goes with the variable.
    install(&wake, &env);
    let mut v = read_json(&h.kiro_mcp());
    v["mcpServers"]["agentmail"]["env"]
        .as_object_mut()
        .unwrap()
        .remove("RUST_LOG");
    write(&h.kiro_mcp(), &serde_json::to_string_pretty(&v).unwrap());
    install(&kiro_opts(false, BIN), &env);
    assert_eq!(
        read_json(&h.kiro_mcp())["mcpServers"]["agentmail"],
        kiro_server(BIN)
    );
}

#[test]
fn kiro_hooks_file_keeps_foreign_hooks_and_fields() {
    let h = Home::new();
    let foreign = json!({
        "version": "v1",
        "hooks": [
            {"name": "fmt", "trigger": "PostToolUse", "matcher": "fs_write",
             "action": {"type": "command", "command": "cargo fmt"}},
            {"name": "mine", "trigger": "Stop", "description": "kept",
             "action": {"type": "command", "command": format!("{BIN} hook stop --harness kiro")},
             "enabled": true, "timeout": 99},
        ]
    });
    write(
        &h.kiro_hooks(),
        &serde_json::to_string_pretty(&foreign).unwrap(),
    );
    let env = h.env(&[]);
    install(&kiro_opts(false, BIN), &env);
    let v = read_json(&h.kiro_hooks());
    let hooks = v["hooks"].as_array().unwrap();
    assert_eq!(hooks[0], foreign["hooks"][0]);
    // agentmail's stop hook is updated in place, keeping the user's keys.
    assert_eq!(hooks[1]["description"], "kept", "{v}");
    assert_eq!(hooks[1]["enabled"], true, "{v}");
    assert_eq!(hooks[1]["timeout"], 10, "{v}");
    assert_eq!(hooks[1]["name"], "agentmail-stop", "{v}");
    assert_eq!(hooks.len(), 5, "{v}");
    uninstall(&kiro_opts(false, BIN), &env);
    let v = read_json(&h.kiro_hooks());
    assert_eq!(
        v["hooks"].as_array().unwrap(),
        &[foreign["hooks"][0].clone()]
    );
}

#[test]
fn kiro_respects_kiro_home() {
    let h = Home::new();
    let kh = h.dir.path().join("kh");
    let env = h.env(&[("KIRO_HOME", kh.to_str().unwrap())]);
    install(&kiro_opts(false, BIN), &env);
    assert_eq!(
        read_json(&kh.join("settings/mcp.json"))["mcpServers"]["agentmail"],
        kiro_server(BIN)
    );
    assert_eq!(
        read_json(&kh.join("hooks/agentmail.json")),
        kiro_expected_hooks(BIN, false)
    );
    assert!(!h.kiro_dir().exists());
    uninstall(&kiro_opts(false, BIN), &env);
    assert!(!kh.exists(), "agentmail created it and removes it again");
}

#[test]
fn kiro_project_scope() {
    let h = Home::new();
    let env = h.env(&[]);
    install(&kiro_opts(true, BIN), &env);
    let mcp = h.proj().join(".kiro/settings/mcp.json");
    let hooks = h.proj().join(".kiro/hooks/agentmail.json");
    assert_eq!(read_json(&mcp)["mcpServers"]["agentmail"], kiro_server(BIN));
    assert_eq!(read_json(&hooks), kiro_expected_hooks(BIN, false));
    assert!(!h.kiro_dir().exists());
    uninstall(&kiro_opts(true, BIN), &env);
    assert!(!h.proj().join(".kiro").exists());
}

#[test]
fn kiro_idempotent() {
    let h = Home::new();
    write(&h.kiro_mcp(), FOREIGN_KIRO_MCP);
    let env = h.env(&[]);
    install(&kiro_opts(false, BIN), &env);
    assert!(
        install::plan_install(&kiro_opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
    // A reinstall from another path replaces the entries, never adds.
    install(&kiro_opts(false, "/usr/local/bin/agentmail"), &env);
    assert_eq!(
        read_json(&h.kiro_hooks()),
        kiro_expected_hooks("/usr/local/bin/agentmail", false)
    );
}

#[test]
fn kiro_uninstall() {
    let h = Home::new();
    write(&h.kiro_mcp(), FOREIGN_KIRO_MCP);
    let env = h.env(&[]);
    install(&kiro_opts(false, BIN), &env);
    let changes = uninstall(&kiro_opts(false, BIN), &env);
    assert_eq!(changes.len(), 2);
    assert!(!h.kiro_hooks().exists());
    assert!(!marker(&h.kiro_hooks()).exists());
    assert!(!h.kiro_dir().join("hooks").exists());
    assert_eq!(
        read_json(&h.kiro_mcp()),
        serde_json::from_str::<Value>(FOREIGN_KIRO_MCP).unwrap()
    );
    assert!(
        install::plan_uninstall(&kiro_opts(false, BIN), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn kiro_hooks_file_left_with_no_hooks_is_removed() {
    // A hooks file agentmail did not create (its hooks pasted by hand)
    // that held nothing else: kiro rejects a hooks file without hooks
    // (`{}` too), so the file goes, its text kept in the backup.
    let h = Home::new();
    let env = h.env(&[]);
    let text = serde_json::to_string_pretty(&kiro_expected_hooks(BIN, false)).unwrap();
    write(&h.kiro_hooks(), &text);
    uninstall(&kiro_opts(false, BIN), &env);
    assert!(!h.kiro_hooks().exists());
    assert_eq!(std::fs::read_to_string(bak(&h.kiro_hooks())).unwrap(), text);

    // Installed over a file of the user's: the backup of the original is
    // kept as it was.
    let h = Home::new();
    let env = h.env(&[]);
    let original = r#"{"version": "v1", "hooks": []}"#;
    write(&h.kiro_hooks(), original);
    install(&kiro_opts(false, BIN), &env);
    uninstall(&kiro_opts(false, BIN), &env);
    assert!(!h.kiro_hooks().exists());
    assert_eq!(
        std::fs::read_to_string(bak(&h.kiro_hooks())).unwrap(),
        original
    );
}

#[test]
fn kiro_uninstall_leaves_hooks_files_without_agentmail_alone() {
    // A hooks file with no agentmail hooks (empty, or only the user's) is
    // not agentmail's to touch: no change, the file byte-identical.
    let foreign = r#"{
  "version": "v1",
  "hooks": [{"name": "mine", "trigger": "Stop", "action": {"type": "command", "command": "echo hi"}}]
}"#;
    for text in [r#"{"version":"v1","hooks":[]}"#, foreign] {
        let h = Home::new();
        let env = h.env(&[]);
        write(&h.kiro_hooks(), text);
        assert!(
            install::plan_uninstall(&kiro_opts(false, BIN), &env)
                .unwrap()
                .is_empty(),
            "{text}"
        );
        uninstall(&kiro_opts(false, BIN), &env);
        assert_eq!(read(&h.kiro_hooks()), text);
        assert!(!bak(&h.kiro_hooks()).exists());
    }
}

#[test]
fn kiro_install_notes_the_other_scope() {
    // Hooks installed at both scopes all run: kiro loads the workspace's
    // hooks alongside the user's.
    let both = |notes: &[String]| notes.iter().any(|n| n.contains("both"));
    let h = Home::new();
    let env = h.env(&[]);
    let notes = install::notes(&kiro_opts(true, BIN), &env, true).unwrap();
    assert!(!both(&notes), "{notes:?}");
    install(&kiro_opts(false, BIN), &env);
    let notes = install::notes(&kiro_opts(true, BIN), &env, true).unwrap();
    assert!(both(&notes), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|n| n.contains(&h.kiro_hooks().display().to_string())),
        "{notes:?}"
    );
    // The user's scope, with the project's installed.
    install(&kiro_opts(true, BIN), &env);
    let notes = install::notes(&kiro_opts(false, BIN), &env, true).unwrap();
    assert!(both(&notes), "{notes:?}");
    // Not on uninstall, nor once the other scope is gone.
    assert!(
        install::notes(&kiro_opts(false, BIN), &env, false)
            .unwrap()
            .is_empty()
    );
    uninstall(&kiro_opts(true, BIN), &env);
    let notes = install::notes(&kiro_opts(false, BIN), &env, true).unwrap();
    assert!(!both(&notes), "{notes:?}");
}

#[test]
fn kiro_v3_warning_printed() {
    let h = Home::new();
    let out = cli(&h)
        .env_remove("KIRO_HOME")
        .args(["install", "--harness", "kiro"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.lines().any(
            |l| l == "kiro-cli: agentmail supports the V3 engine only — run kiro-cli chat --v3"
        ),
        "{out}"
    );
    assert!(h.kiro_hooks().exists());
    let out = cli(&h)
        .env_remove("KIRO_HOME")
        .args(["--json", "install", "--harness", "kiro", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("V3 engine only")),
        "{v}"
    );
    cli(&h)
        .env_remove("KIRO_HOME")
        .args(["uninstall", "--harness", "kiro"])
        .assert()
        .success();
    assert!(!h.kiro_dir().exists());
}

#[test]
fn cli_kiro_wake_is_kiro_only() {
    let h = Home::new();
    let out = cli(&h)
        .args(["install", "--harness", "claude-code", "--kiro-wake"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("--kiro-wake"));
    assert!(!h.settings().exists());
    let out = cli(&h)
        .env_remove("KIRO_HOME")
        .args(["install", "--harness", "kiro", "--kiro-wake"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    // Waking holds what the human types until Esc: the install says so.
    assert!(out.contains("Esc"), "{out}");
    assert!(read(&h.kiro_hooks()).contains("--wake"));
}

// ---- Task 9: `install --service` -------------------------------------------

/// The systemd unit in the README's "Running the daemon under systemd"
/// section.
fn readme_unit() -> String {
    let readme = read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"));
    let section = readme
        .split("### Running the daemon under systemd")
        .nth(1)
        .expect("README has the systemd section");
    let block = section
        .split("```ini\n")
        .nth(1)
        .expect("the section has an ini block");
    block.split("```").next().unwrap().to_string()
}

#[test]
fn service_unit_content_matches_readme() {
    let bin = Path::new("/opt/agentmail/bin/agentmail");
    let want: String = readme_unit()
        .lines()
        .map(|l| {
            if l.starts_with("ExecStart=") {
                "ExecStart=/opt/agentmail/bin/agentmail daemon run --wait-for-lock\n".to_string()
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    assert!(readme_unit().contains("\nExecStart="), "{}", readme_unit());
    assert_eq!(install::service::unit(bin).unwrap(), want);
}

#[test]
fn service_unit_quotes_the_binary_for_systemd() {
    let exec = |bin: &str| {
        install::service::unit(Path::new(bin))
            .unwrap()
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .unwrap()
            .to_string()
    };
    assert_eq!(
        exec("/a b/agentmail"),
        "ExecStart=\"/a b/agentmail\" daemon run --wait-for-lock"
    );
    assert_eq!(
        exec("/a%b$c/agent\"mail"),
        "ExecStart=\"/a%%b$c/agent\\\"mail\" daemon run --wait-for-lock"
    );
    // systemd expands no `$` in the executable path, so it stays single.
    assert_eq!(
        exec("/a$b/agentmail"),
        "ExecStart=/a$b/agentmail daemon run --wait-for-lock"
    );
    assert!(install::service::unit(Path::new("relative/agentmail")).is_err());
    assert!(install::service::unit(Path::new("/a\nb/agentmail")).is_err());
}

/// A temporary home for `install --service`: a fake `systemctl` that logs
/// each call (with whether the test's daemon at the default location was
/// up then) but the read-only `is-active`, which succeeds with
/// `FAKE_ACTIVE=active`, `show-environment`, which prints `FAKE_ENV`
/// (by default, the manager's `XDG_CONFIG_HOME` the same as the CLI's),
/// `show -p UnitPath`, which prints the manager's unit search path
/// `FAKE_UNITPATH` (by default, nothing), `show -p Environment`, which
/// prints the unit's `FAKE_UNITENV` (by default, nothing), and `show`, which prints
/// `FAKE_SHOW` (by default, no unit file), starts a daemon on demand at
/// the default location during `enable --now` with `FAKE_START_ON_ENABLE`,
/// and fails a call holding `FAKE_FAIL`; and the daemon's default data
/// directory and socket inside it.
struct ServiceHome {
    h: Home,
}

impl ServiceHome {
    fn new() -> ServiceHome {
        let h = Home::new();
        let fake = h.dir.path().join("systemctl");
        write(
            &fake,
            r#"#!/bin/sh
case "$*" in
  *show-environment*) printf '%s' "$FAKE_ENV"; exit 0 ;;
  *is-active*) [ "$FAKE_ACTIVE" = active ]; exit $? ;;
  *"show -p UnitPath"*)
    if [ -n "${FAKE_UNITPATH+x}" ]; then printf 'UnitPath=%s\n' "$FAKE_UNITPATH"; fi
    exit 0 ;;
  *"show -p Environment"*) printf '%s' "$FAKE_UNITENV"; exit 0 ;;
  "--user show "*) printf '%s' "${FAKE_SHOW:-LoadState=not-found
FragmentPath=
}"; exit 0 ;;
esac
if env -u AGENTMAIL_DATA_DIR -u AGENTMAIL_SOCKET "$AGENTMAIL_TEST_BIN" daemon status >/dev/null 2>&1; then up=up; else up=down; fi
echo "$* [$up]" >> "$FAKE_LOG"
case "$*" in
  *disable*) if [ -n "$FAKE_RO" ]; then chmod 555 "$FAKE_RO"; fi ;;
  *"enable --now"*)
    if [ -n "$FAKE_START_ON_ENABLE" ]; then
      env -u AGENTMAIL_DATA_DIR -u AGENTMAIL_SOCKET "$AGENTMAIL_TEST_BIN" daemon start >/dev/null 2>&1
    fi ;;
esac
if [ -n "$FAKE_FAIL" ]; then
  case "$*" in *"$FAKE_FAIL"*) echo "Failed to $FAKE_FAIL" >&2; exit 1 ;; esac
fi
case "$*" in
  *is-system-running*) echo "${FAKE_STATE:-running}"; exit "${FAKE_RC:-0}" ;;
esac
exit 0
"#,
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            // Another test's child, forked while the script was open for
            // writing here, holds it open until it execs: running the
            // script fails with ETXTBSY until then.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::process::Command::new(&fake)
                .args(["--user", "is-active"])
                .env("FAKE_ACTIVE", "")
                .output()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ExecutableFileBusy)
            {
                assert!(std::time::Instant::now() < deadline, "fake systemctl busy");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        ServiceHome { h }
    }

    fn config(&self) -> PathBuf {
        self.h.dir.path().join("cfg")
    }

    fn unit(&self) -> PathBuf {
        self.config().join("systemd/user/agentmail.service")
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.h.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn cli(&self) -> Command {
        self.cli_as(Path::new(env!("CARGO_BIN_EXE_agentmail")))
    }

    /// The CLI, run as `prog`. The daemon's location is the default one
    /// under the temporary home.
    fn cli_as(&self, prog: &Path) -> Command {
        let d = self.h.dir.path();
        let mut c = Command::new(prog);
        c.env("HOME", self.h.home())
            .env_remove("CLAUDE_CONFIG_DIR")
            .current_dir(self.h.proj())
            .timeout(Duration::from_secs(30))
            .env("XDG_CONFIG_HOME", self.config())
            .env("AGENTMAIL_SYSTEMCTL", d.join("systemctl"))
            .env("AGENTMAIL_TEST_BIN", env!("CARGO_BIN_EXE_agentmail"))
            .env("FAKE_LOG", d.join("log"))
            .env_remove("AGENTMAIL_DATA_DIR")
            .env_remove("AGENTMAIL_SOCKET")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("FAKE_STATE")
            .env_remove("FAKE_RC")
            .env_remove("FAKE_ACTIVE")
            .env(
                "FAKE_ENV",
                format!("XDG_CONFIG_HOME={}\n", self.config().display()),
            )
            .env_remove("FAKE_SHOW")
            .env_remove("FAKE_UNITPATH")
            .env_remove("FAKE_UNITENV")
            .env_remove("FAKE_FAIL")
            .env_remove("FAKE_START_ON_ENABLE")
            .env_remove("FAKE_RO");
        c
    }

    /// What `systemctl --user show` says of the service running on
    /// agentmail's unit as installed for this binary, reloaded since.
    fn loaded(&self) -> String {
        format!(
            "LoadState=loaded\nFragmentPath={}\nNeedDaemonReload=no\nExecStart={{ path={} ; \
             argv[]={} daemon run --wait-for-lock ; ignore_errors=no ; start_time=[n/a] ; \
             stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}\n",
            self.unit().display(),
            bin_path(),
            bin_path()
        )
    }

    /// The CLI with a private daemon location, as `AGENTMAIL_DATA_DIR` and
    /// `AGENTMAIL_SOCKET` set it.
    fn private_cli(&self) -> Command {
        let d = self.h.dir.path();
        let mut c = self.cli();
        c.env("AGENTMAIL_DATA_DIR", d.join("data"))
            .env("AGENTMAIL_SOCKET", d.join("sock"));
        c
    }

    fn daemon_up(&self, mut c: Command) -> bool {
        c.args(["daemon", "status"]).ok().is_ok()
    }

    /// The default location's lock record.
    fn lock_record(&self) -> Option<agentmail::paths::LockRecord> {
        agentmail::paths::Paths::from_vars(|k| {
            (k == "HOME").then(|| self.h.home().into_os_string())
        })
        .unwrap()
        .lock_record()
    }

    /// A supervised daemon at the default location, as the service runs
    /// (`daemon run --wait-for-lock`), once it serves.
    fn supervised(&self) -> Supervised {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_agentmail"))
            .args(["daemon", "run", "--wait-for-lock"])
            .env("HOME", self.h.home())
            .env_remove("AGENTMAIL_DATA_DIR")
            .env_remove("AGENTMAIL_SOCKET")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let child = Supervised(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !(self.daemon_up(self.cli()) && self.lock_record().is_some_and(|r| r.supervised)) {
            assert!(std::time::Instant::now() < deadline, "no supervised daemon");
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }
}

/// A supervised daemon, killed when dropped.
struct Supervised(std::process::Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ServiceHome {
    fn drop(&mut self) {
        let _ = self.cli().args(["daemon", "stop"]).ok();
        let _ = self.private_cli().args(["daemon", "stop"]).ok();
    }
}

fn bin_path() -> String {
    env!("CARGO_BIN_EXE_agentmail").to_string()
}

#[test]
fn service_calls_systemctl_in_order() {
    let s = ServiceHome::new();
    // A daemon started on demand, which the service replaces, and one at a
    // private location, which is not the service's business.
    s.cli().args(["daemon", "start"]).assert().success();
    s.private_cli().args(["daemon", "start"]).assert().success();
    let out = s
        .private_cli()
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(&s.unit().display().to_string()), "{out}");
    // The command run is shown as run: this binary, at the default location.
    assert!(
        out.contains(&format!(
            "ran env -u AGENTMAIL_DATA_DIR -u AGENTMAIL_SOCKET {} daemon stop",
            bin_path()
        )),
        "{out}"
    );
    assert_eq!(
        read(&s.unit()),
        install::service::unit(Path::new(&bin_path())).unwrap()
    );
    // systemd is checked first, and the on-demand daemon is stopped only
    // once the service is started, so that the service's daemon, waiting
    // for the lock, takes over at once.
    assert_eq!(
        s.log(),
        [
            "--user is-system-running [up]",
            "--user daemon-reload [up]",
            "--user enable --now agentmail [up]",
        ]
    );
    let ran: Vec<&str> = out.lines().filter(|l| l.starts_with("ran ")).collect();
    assert!(ran.last().unwrap().ends_with("daemon stop"), "{out}");
    assert!(ran[0].ends_with("daemon-reload"), "{out}");
    assert!(!s.daemon_up(s.cli()));
    assert!(
        s.daemon_up(s.private_cli()),
        "the private daemon was stopped"
    );

    // Again, with the service running its daemon (supervised): the unit
    // is unchanged, the service (re)enabled, and its daemon left alone.
    let sup = s.supervised();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .env("FAKE_SHOW", s.loaded())
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("daemon stop"), "{out}");
    assert_eq!(
        s.log()[3..],
        [
            "--user is-system-running [up]",
            "--user daemon-reload [up]",
            "--user enable --now agentmail [up]",
        ]
    );
    assert!(s.daemon_up(s.cli()));
    assert!(s.lock_record().is_some_and(|r| r.supervised));
    drop(sup);
    // No daemon at all: none to stop.
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("daemon stop"), "{out}");
    // A daemon on demand is stopped, the service running or not.
    s.cli().args(["daemon", "start"]).assert().success();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("daemon stop"), "{out}");
    assert!(!s.daemon_up(s.cli()));
    assert!(!bak(&s.unit()).exists());
}

#[test]
fn service_unit_runs_the_binary_by_the_path_invoked() {
    // A symlink to the binary (say, to a versioned install) stays in the
    // unit, so that it follows the link when it is updated.
    let s = ServiceHome::new();
    let link = s.h.dir.path().join("bin/agentmail");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentmail"), &link).unwrap();
    s.cli().args(["daemon", "start"]).assert().success();
    let out = s
        .cli_as(&link)
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains(&format!(
            "+ExecStart={} daemon run --wait-for-lock",
            link.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("  {} daemon stop", link.display())),
        "{out}"
    );
}

#[test]
fn service_notes_the_location_overrides() {
    let s = ServiceHome::new();
    let out = s
        .private_cli()
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("note: AGENTMAIL_DATA_DIR and AGENTMAIL_SOCKET are set here"),
        "{out}"
    );
    let out = s
        .cli()
        .env("AGENTMAIL_SOCKET", s.h.dir.path().join("sock"))
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let note = v["notes"][0].as_str().unwrap();
    assert!(note.starts_with("AGENTMAIL_SOCKET is set here"), "{v}");
    // It says how to give the service the same, in a drop-in that install
    // and uninstall leave alone, with the value as a unit takes it.
    assert!(note.contains("systemctl --user edit agentmail"), "{v}");
    assert!(
        note.contains(&format!(
            "Environment=\"AGENTMAIL_SOCKET={}\"",
            s.h.dir.path().join("sock").display()
        )),
        "{v}"
    );
    assert!(note.contains("agentmail.service.d/override.conf"), "{v}");
    assert!(!note.contains("AGENTMAIL_DATA_DIR"), "{v}");
}

#[test]
fn service_dry_run_changes_and_runs_nothing() {
    let s = ServiceHome::new();
    let out = s
        .cli()
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains(&format!("+++ {}", s.unit().display())),
        "{out}"
    );
    assert!(
        out.contains("systemctl --user enable --now agentmail"),
        "{out}"
    );
    assert!(!s.unit().exists());
    // Only the read-only check that systemd is usable runs.
    assert_eq!(s.log(), ["--user is-system-running [down]"]);
    assert!(!out.contains("note:"), "{out}");
    // No daemon runs: there is none to stop.
    assert!(!out.contains("daemon stop"), "{out}");

    // Once installed, a dry run shows `daemon stop` only where a real run
    // would run it: for a daemon on demand, not the service's (supervised).
    s.cli().args(["install", "--service"]).assert().success();
    let dry = |on_demand: bool| {
        let _sup = (!on_demand).then(|| s.supervised());
        if on_demand {
            s.cli().args(["daemon", "start"]).assert().success();
        }
        let out = s
            .cli()
            .env("FAKE_ACTIVE", "active")
            .env("FAKE_SHOW", s.loaded())
            .args(["install", "--service", "--dry-run"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out).unwrap()
    };
    let out = dry(false);
    assert!(out.contains("nothing to change"), "{out}");
    assert!(!out.contains("try-restart"), "{out}");
    assert!(!out.contains("daemon stop"), "{out}");
    assert!(
        out.contains("systemctl --user enable --now agentmail"),
        "{out}"
    );
    let out = dry(true);
    assert!(out.contains("daemon stop"), "{out}");
}

#[test]
fn service_notes_a_manager_with_other_default_locations() {
    // The systemd user manager's XDG_RUNTIME_DIR puts the service's socket
    // elsewhere than the default here (none set): said so.
    let s = ServiceHome::new();
    let out = s
        .cli()
        .env(
            "FAKE_ENV",
            format!(
                "LANG=C\nXDG_RUNTIME_DIR=/run/user/4242\nXDG_CONFIG_HOME={}\n",
                s.config().display()
            ),
        )
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let note = v["notes"][0].as_str().unwrap();
    assert!(note.contains("socket /run/user/4242/agentmail.sock"), "{v}");
    assert!(!note.contains("data directory"), "{v}");
    assert!(note.contains("gets XDG_RUNTIME_DIR differently"), "{v}");
    // An AGENTMAIL_SOCKET the manager has (imported, say) is named as the
    // cause, not the XDG variables.
    let out = s
        .cli()
        .env(
            "FAKE_ENV",
            format!(
                "AGENTMAIL_SOCKET=/run/elsewhere.sock\nXDG_CONFIG_HOME={}\n",
                s.config().display()
            ),
        )
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let note = v["notes"][0].as_str().unwrap();
    assert!(note.contains("socket /run/elsewhere.sock"), "{v}");
    assert!(note.contains("gets AGENTMAIL_SOCKET differently"), "{v}");
    assert!(!note.contains("XDG"), "{v}");
    // Matching values: no note.
    let run = s.h.dir.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let out = s
        .cli()
        .env("XDG_RUNTIME_DIR", &run)
        .env(
            "FAKE_ENV",
            format!(
                "XDG_RUNTIME_DIR={}\nXDG_CONFIG_HOME={}\n",
                run.display(),
                s.config().display()
            ),
        )
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["notes"], json!([]), "{v}");
    // A quoted value of some other variable (a multi-line one, say) does
    // not hide the difference.
    let out = s
        .cli()
        .env(
            "FAKE_ENV",
            format!(
                "MOTD=$'a\\nb'\nXDG_RUNTIME_DIR=/run/user/4242\nXDG_CONFIG_HOME={}\n",
                s.config().display()
            ),
        )
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["notes"][0]
            .as_str()
            .unwrap()
            .contains("socket /run/user/4242/agentmail.sock"),
        "{v}"
    );
    // One of a variable the location depends on cannot be read: no note.
    let out = s
        .cli()
        .env(
            "FAKE_ENV",
            format!(
                "XDG_RUNTIME_DIR=$'/run/user/42\\n42'\nXDG_CONFIG_HOME={}\n",
                s.config().display()
            ),
        )
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["notes"], json!([]), "{v}");
}

#[test]
fn service_failure_after_writing_the_unit_says_so() {
    let s = ServiceHome::new();
    let out = s
        .cli()
        .env("FAKE_FAIL", "enable")
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&format!("the unit {} is written", s.unit().display())),
        "{err}"
    );
    assert!(
        err.contains("run `agentmail install --service` again"),
        "{err}"
    );
    assert!(err.contains("Failed to enable"), "{err}");
    assert!(s.unit().exists());
    // Running it again finishes it.
    s.cli().args(["install", "--service"]).assert().success();
    assert!(s.log().last().unwrap().contains("enable --now agentmail"));

    // Uninstall: a failed disable keeps the unit.
    let out = s
        .cli()
        .env("FAKE_FAIL", "disable")
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&format!("the unit {} is kept", s.unit().display())),
        "{err}"
    );
    assert!(s.unit().exists());
    // A failed reload, once the unit is removed, says what is left to do.
    let out = s
        .cli()
        .env("FAKE_FAIL", "daemon-reload")
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("systemctl --user daemon-reload"), "{err}");
    assert!(err.contains("is removed"), "{err}");
    assert!(!s.unit().exists());
}

#[cfg(unix)]
#[test]
fn service_failure_to_write_or_remove_the_unit_says_so() {
    let s = ServiceHome::new();
    let dir = s.unit().parent().unwrap().to_path_buf();
    let mode = |m: u32| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m)).unwrap();
    };
    std::fs::create_dir_all(&dir).unwrap();
    mode(0o555);
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    mode(0o755);
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&format!(
            "the unit {} could not be written",
            s.unit().display()
        )),
        "{err}"
    );
    assert!(err.contains("the service is unchanged"), "{err}");
    assert_eq!(s.log(), ["--user is-system-running [down]"]);

    // Uninstall: the service is disabled, then the unit cannot be removed.
    s.cli().args(["install", "--service"]).assert().success();
    let out = s
        .cli()
        .env("FAKE_RO", &dir)
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    mode(0o755);
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&format!(
            "the service is disabled and stopped, but the unit {} could not be removed",
            s.unit().display()
        )),
        "{err}"
    );
    assert!(err.contains("systemctl --user daemon-reload"), "{err}");
    assert!(s.unit().exists());
}

#[test]
fn service_uninstall() {
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    assert!(s.unit().exists());
    let n = s.log().len();
    s.cli().args(["uninstall", "--service"]).assert().success();
    assert_eq!(
        s.log()[n..],
        [
            "--user is-system-running [down]",
            "--user disable --now agentmail [down]",
            "--user daemon-reload [down]",
        ]
    );
    assert!(!s.unit().exists());
    assert!(!marker(&s.unit()).exists());
    // The directories install created for the unit are gone again.
    assert!(!s.config().exists());
    // Nothing left to remove: systemd is not touched.
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(out).unwrap().contains("not installed"),
        "second uninstall"
    );
    // Only asked whether it runs (not logged).
    assert_eq!(s.log().len(), n + 3, "{:?}", s.log());
}

#[test]
fn service_uninstall_cleans_up_after_a_unit_removed_by_hand() {
    // The unit file was deleted while the service was enabled and running:
    // systemd still has its `default.target.wants` link and runs it.
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    let link = s
        .unit()
        .parent()
        .unwrap()
        .join("default.target.wants/agentmail.service");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(s.unit(), &link).unwrap();
    std::fs::remove_file(s.unit()).unwrap();
    let n = s.log().len();
    // A dry run shows what it would do, and does nothing.
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .args(["uninstall", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("systemctl --user stop agentmail"), "{out}");
    assert!(out.contains(&format!("rm {}", link.display())), "{out}");
    assert!(std::fs::symlink_metadata(&link).is_ok());
    // Only the read-only check that systemd is usable runs.
    assert_eq!(s.log()[n..], ["--user is-system-running [down]"]);
    let n = s.log().len();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("already removed"), "{out}");
    assert!(out.contains("ran systemctl --user daemon-reload"), "{out}");
    assert_eq!(
        s.log()[n..],
        [
            "--user is-system-running [down]",
            "--user stop agentmail [down]",
            "--user daemon-reload [down]",
        ]
    );
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert!(!link.parent().unwrap().exists());
    // Only the link left (the service not running): removed, no stop.
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(s.unit(), &link).unwrap();
    let n = s.log().len();
    s.cli().args(["uninstall", "--service"]).assert().success();
    assert_eq!(
        s.log()[n..],
        [
            "--user is-system-running [down]",
            "--user daemon-reload [down]",
        ]
    );
    assert!(std::fs::symlink_metadata(&link).is_err());
    // Nothing left: nothing is touched.
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("not installed"));
}

#[test]
fn service_uninstall_leaves_a_service_agentmail_did_not_install() {
    // No unit of agentmail's, but a user service named agentmail runs from
    // elsewhere (a package's, say), enabled with a link in the config
    // directory: neither is touched.
    let s = ServiceHome::new();
    let theirs = s.h.dir.path().join("pkg/agentmail.service");
    write(
        &theirs,
        "[Service]\nExecStart=/usr/bin/agentmail daemon run\n",
    );
    let link = s
        .unit()
        .parent()
        .unwrap()
        .join("default.target.wants/agentmail.service");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&theirs, &link).unwrap();
    let show = format!("LoadState=loaded\nFragmentPath={}\n", theirs.display());
    for dry in [true, false] {
        let mut c = s.cli();
        c.env("FAKE_ACTIVE", "active")
            .env("FAKE_SHOW", &show)
            .args(["uninstall", "--service"]);
        if dry {
            c.arg("--dry-run");
        }
        let out = c.assert().success().get_output().stdout.clone();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("stop agentmail"), "{out}");
        assert!(!out.contains("rm "), "{out}");
        assert!(out.contains("left alone"), "{out}");
        assert!(out.contains(&theirs.display().to_string()), "{out}");
    }
    assert_eq!(std::fs::read_link(&link).unwrap(), theirs);
    assert!(s.log().is_empty(), "{:?}", s.log());

    // A service still loaded from agentmail's (removed) unit is agentmail's.
    std::fs::remove_file(&link).unwrap();
    let n = s.log().len();
    s.cli()
        .env("FAKE_ACTIVE", "active")
        .env(
            "FAKE_SHOW",
            format!("LoadState=loaded\nFragmentPath={}\n", s.unit().display()),
        )
        .args(["uninstall", "--service"])
        .assert()
        .success();
    assert_eq!(
        s.log()[n..],
        [
            "--user is-system-running [down]",
            "--user stop agentmail [down]",
            "--user daemon-reload [down]",
        ]
    );
}

#[test]
fn service_refuses_a_manager_loading_units_elsewhere() {
    // The manager's unit search path (from its own XDG_CONFIG_HOME, not
    // this shell's) does not hold the unit's directory: it would never see
    // the unit, so nothing is written.
    let s = ServiceHome::new();
    let other = s.h.dir.path().join("other-cfg");
    let ours = s.config().join("systemd/user");
    let search = format!(
        "{}/systemd/user.control /run/user/1/systemd/transient {} /etc/systemd/user \
         /usr/lib/systemd/user",
        other.display(),
        other.join("systemd/user").display()
    );
    let out = s
        .cli()
        .env("FAKE_UNITPATH", &search)
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&format!("does not load user units from {}", ours.display())),
        "{err}"
    );
    assert!(err.contains("Nothing was changed"), "{err}");
    // `set-environment` changes only what the manager passes to units,
    // never where it looks for them: not suggested.
    assert!(!err.contains("set-environment"), "{err}");
    assert!(!s.unit().exists());
    assert!(s.log().is_empty(), "{:?}", s.log());
    // A dry run says so.
    let out = s
        .cli()
        .env("FAKE_UNITPATH", &search)
        .args(["--json", "install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let notes: Vec<&str> = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("does not load user units from")),
        "{v}"
    );
    assert!(notes.iter().all(|n| !n.contains("set-environment")), "{v}");
    // Uninstall too refuses, keeping the unit.
    s.cli().args(["install", "--service"]).assert().success();
    s.cli()
        .env("FAKE_UNITPATH", &search)
        .args(["uninstall", "--service"])
        .assert()
        .failure();
    assert!(s.unit().exists());
}

#[test]
fn service_trusts_the_manager_search_path_over_its_unit_environment() {
    // The environment the manager passes to units (`show-environment`,
    // say after `import-environment`) names another XDG_CONFIG_HOME, but
    // the manager's own search path holds the unit's directory: installed.
    let s = ServiceHome::new();
    let other = s.h.dir.path().join("other-cfg");
    let search = format!(
        "/run/user/1/systemd/transient {} /etc/systemd/user /usr/lib/systemd/user",
        s.config().join("systemd/user").display()
    );
    s.cli()
        .env("FAKE_ENV", format!("XDG_CONFIG_HOME={}\n", other.display()))
        .env("FAKE_UNITPATH", &search)
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(s.unit().exists());
    // So too with only show-environment differing, the search path unknown.
    let s = ServiceHome::new();
    s.cli()
        .env("FAKE_ENV", format!("XDG_CONFIG_HOME={}\n", other.display()))
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(s.unit().exists());
}

#[test]
fn service_restarts_a_running_service_whose_unit_was_removed() {
    // The unit was removed by hand while the service still runs: install
    // writes it anew and restarts the service onto it, since enabling a
    // running service does not.
    let s = ServiceHome::new();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(s.unit().exists());
    assert!(
        s.log()
            .iter()
            .any(|l| l.starts_with("--user try-restart agentmail")),
        "{:?}",
        s.log()
    );
    assert!(
        out.contains("ran systemctl --user try-restart agentmail"),
        "{out}"
    );
    // A dry run lists it too.
    std::fs::remove_file(s.unit()).unwrap();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("try-restart agentmail"), "{out}");
    // Not running: no restart.
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    assert!(
        !s.log().iter().any(|l| l.contains("try-restart")),
        "{:?}",
        s.log()
    );
}

#[test]
fn service_failure_to_stop_the_on_demand_daemon_says_so() {
    // A daemon on demand holds the default lock (serving another socket),
    // and another data directory's daemon answers on the default socket,
    // so `daemon stop` refuses: the service is running all the same.
    let s = ServiceHome::new();
    let d = s.h.dir.path();
    let default_sock = s.h.home().join(".local/share/agentmail/agentmail.sock");
    let a = || {
        let mut c = s.cli();
        c.env("AGENTMAIL_SOCKET", d.join("sock-a"));
        c
    };
    let b = || {
        let mut c = s.cli();
        c.env("AGENTMAIL_DATA_DIR", d.join("data-b"))
            .env("AGENTMAIL_SOCKET", &default_sock);
        c
    };
    a().args(["daemon", "start"]).assert().success();
    b().args(["daemon", "start"]).assert().success();
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let _ = b().args(["daemon", "stop"]).ok();
    let _ = a().args(["daemon", "stop"]).ok();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains("the service is enabled and running, but stopping the daemon"),
        "{err}"
    );
    assert!(err.contains("agentmail daemon stop"), "{err}");
    assert!(err.contains("not for this data directory"), "{err}");
    assert!(!err.contains("may not be running"), "{err}");
    assert!(s.unit().exists());
}

#[test]
fn service_replaces_a_hand_written_unit_keeping_it() {
    let s = ServiceHome::new();
    write(
        &s.unit(),
        "[Service]\nExecStart=/old/agentmail daemon run\n",
    );
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(
        read(&bak(&s.unit())),
        "[Service]\nExecStart=/old/agentmail daemon run\n"
    );
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!s.unit().exists());
    // The user's own unit is not thrown away, nor put back: the output
    // says how to.
    assert!(bak(&s.unit()).exists());
    assert!(
        out.contains(&format!(
            "note: uninstall does not put the unit kept as {} back",
            bak(&s.unit()).display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "mv {} {} && systemctl --user daemon-reload",
            bak(&s.unit()).display(),
            s.unit().display()
        )),
        "{out}"
    );
}

#[test]
fn service_no_systemd_errors() {
    let s = ServiceHome::new();
    let out = s
        .cli()
        .env("FAKE_STATE", "offline")
        .env("FAKE_RC", "1")
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("offline"), "{err}");
    // The manual steps: the unit, where it goes, and the commands.
    assert!(err.contains(&s.unit().display().to_string()), "{err}");
    assert!(err.contains("ExecStart="), "{err}");
    assert!(
        err.contains(&format!("{} daemon stop", bin_path())),
        "{err}"
    );
    assert!(
        err.contains("systemctl --user enable --now agentmail"),
        "{err}"
    );
    assert!(!s.unit().exists());
    assert_eq!(s.log(), ["--user is-system-running [down]"]);

    // Uninstall says how to remove it by hand.
    write(
        &s.unit(),
        &install::service::unit(Path::new("/x/agentmail")).unwrap(),
    );
    let out = s
        .cli()
        .env("FAKE_STATE", "offline")
        .env("FAKE_RC", "1")
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains("systemctl --user disable --now agentmail"),
        "{err}"
    );
    // The marker goes too, and the backup is explained.
    assert!(
        err.contains(&format!("rm -f {}", marker(&s.unit()).display())),
        "{err}"
    );
    assert!(err.contains(&bak(&s.unit()).display().to_string()), "{err}");
    assert!(s.unit().exists());
}

#[test]
fn service_dry_run_notes_no_systemd() {
    // A dry run where a real run would refuse says so.
    let s = ServiceHome::new();
    for args in [
        &["install", "--service", "--dry-run"][..],
        &["--json", "install", "--service", "--dry-run"][..],
    ] {
        let out = s
            .cli()
            .env("FAKE_STATE", "offline")
            .env("FAKE_RC", "1")
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("the systemd user manager is not available"),
            "{out}"
        );
        assert!(out.contains("offline"), "{out}");
        assert!(out.contains("a real run would change nothing"), "{out}");
    }
    assert!(!s.unit().exists());
    // A usable one: no note.
    let out = s
        .cli()
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("not available"), "{out}");
}

#[test]
fn service_degraded_systemd_is_usable() {
    // `is-system-running` exits non-zero when some unit failed, but the
    // manager is up and can run agentmail's.
    let s = ServiceHome::new();
    s.cli()
        .env("FAKE_STATE", "degraded")
        .env("FAKE_RC", "1")
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(s.unit().exists());
}

#[test]
fn service_respects_home_without_xdg_config() {
    let s = ServiceHome::new();
    s.cli()
        .env_remove("XDG_CONFIG_HOME")
        .env("FAKE_ENV", "")
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(
        s.h.home()
            .join(".config/systemd/user/agentmail.service")
            .exists()
    );
    // A relative XDG_CONFIG_HOME, which systemd ignores, is ignored too.
    let s = ServiceHome::new();
    s.cli()
        .env("XDG_CONFIG_HOME", "rel")
        .env("FAKE_ENV", "")
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(
        s.h.home()
            .join(".config/systemd/user/agentmail.service")
            .exists()
    );
    assert!(!s.h.proj().join("rel").exists());
}

#[test]
fn service_excludes_harness_flags() {
    let s = ServiceHome::new();
    s.cli()
        .args(["install", "--service", "--harness", "claude-code"])
        .assert()
        .code(2);
    s.cli().args(["install"]).assert().code(2);
    assert!(s.log().is_empty());
    assert!(!s.unit().exists());
}

#[test]
fn service_restarts_a_replaced_unit() {
    // enable --now leaves a running service on its old unit: a unit that
    // changed is restarted.
    let s = ServiceHome::new();
    write(
        &s.unit(),
        "[Service]\nExecStart=/old/agentmail daemon run\n",
    );
    s.cli()
        .env("FAKE_ACTIVE", "active")
        .args(["install", "--service"])
        .assert()
        .success();
    assert_eq!(
        s.log(),
        [
            "--user is-system-running [down]",
            "--user daemon-reload [down]",
            "--user enable --now agentmail [down]",
            "--user try-restart agentmail [down]",
        ]
    );
    // A service that is not running is started on the new unit by
    // enable --now: restarting it would only drop its new daemon's clients.
    let s = ServiceHome::new();
    write(
        &s.unit(),
        "[Service]\nExecStart=/old/agentmail daemon run\n",
    );
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(
        s.log(),
        [
            "--user is-system-running [down]",
            "--user daemon-reload [down]",
            "--user enable --now agentmail [down]",
        ]
    );
}

#[test]
fn service_rerun_after_a_failed_install_restarts_the_service() {
    // Install replaces the README's unit while the service runs on it, and
    // fails before the service is restarted onto agentmail's unit: running
    // it again, as the error says, finishes it with the restart, though
    // the unit file is already as it should be.
    let tail = |s: &ServiceHome, n: usize| s.log()[n..].to_vec();
    // With `daemon-reload` failed, systemd has not reloaded the unit since
    // it changed; with a later step failed, it has, but the process
    // started before the unit was written.
    let show = |s: &ServiceHome, fail: &str| {
        if fail == "daemon-reload" {
            s.loaded()
                .replace("NeedDaemonReload=no", "NeedDaemonReload=yes")
        } else {
            format!("{}ExecMainStartTimestamp=@1\n", s.loaded())
        }
    };
    for fail in ["daemon-reload", "enable", "try-restart"] {
        let s = ServiceHome::new();
        write(&s.unit(), &readme_unit());
        let err = s
            .cli()
            .env("FAKE_ACTIVE", "active")
            .env("FAKE_FAIL", fail)
            .args(["install", "--service"])
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone();
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("again to finish"), "{fail}: {err}");
        assert_eq!(
            read(&s.unit()),
            install::service::unit(Path::new(&bin_path())).unwrap()
        );
        let n = s.log().len();
        s.cli()
            .env("FAKE_ACTIVE", "active")
            .env("FAKE_SHOW", show(&s, fail))
            .args(["install", "--service"])
            .assert()
            .success();
        assert_eq!(
            tail(&s, n),
            [
                "--user is-system-running [down]",
                "--user daemon-reload [down]",
                "--user enable --now agentmail [down]",
                "--user try-restart agentmail [down]",
            ],
            "{fail}"
        );
    }

    // The running service is on something else than the unit as it stands
    // here: restarted too.
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    let restarts = |show: String| {
        let n = s.log().len();
        s.cli()
            .env("FAKE_ACTIVE", "active")
            .env("FAKE_SHOW", show)
            .args(["install", "--service"])
            .assert()
            .success();
        s.log()[n..].iter().any(|l| l.contains("try-restart"))
    };
    let other = s.h.dir.path().join("pkg/agentmail.service");
    assert!(restarts(s.loaded().replace(
        &format!("FragmentPath={}", s.unit().display()),
        &format!("FragmentPath={}", other.display())
    )));
    assert!(restarts(s.loaded().replace(
        &format!("path={}", bin_path()),
        "path=/old/agentmail"
    )));
    assert!(restarts("LoadState=not-found\nFragmentPath=\n".to_string()));
    // On agentmail's unit, loaded, and started since it was written: left
    // running, its clients undisturbed.
    assert!(!restarts(s.loaded()));
    assert!(!restarts(format!(
        "{}ExecMainStartTimestamp=@99999999999\n",
        s.loaded()
    )));
    // Through a symlinked config directory, it is the same unit.
    #[cfg(unix)]
    {
        let alias = s.h.dir.path().join("alias-cfg");
        std::os::unix::fs::symlink(s.config(), &alias).unwrap();
        assert!(!restarts(s.loaded().replace(
            &format!("FragmentPath={}", s.unit().display()),
            &format!(
                "FragmentPath={}",
                alias.join("systemd/user/agentmail.service").display()
            )
        )));
    }
    // A service that is not running is not restarted, whatever systemd
    // says of it.
    let n = s.log().len();
    s.cli()
        .env("FAKE_SHOW", "LoadState=not-found\nFragmentPath=\n")
        .args(["install", "--service"])
        .assert()
        .success();
    assert!(!s.log()[n..].iter().any(|l| l.contains("try-restart")));
}

#[cfg(unix)]
#[test]
fn service_uninstall_cleans_up_through_a_symlinked_config_directory() {
    // The config directory reaches systemd through a symlink (HOME, say,
    // a link to another disk): the enable link and the FragmentPath name
    // agentmail's removed unit by the other path, and are agentmail's all
    // the same.
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    let alias = s.h.dir.path().join("alias-cfg");
    std::os::unix::fs::symlink(s.config(), &alias).unwrap();
    let aliased = alias.join("systemd/user/agentmail.service");
    let link = s
        .unit()
        .parent()
        .unwrap()
        .join("default.target.wants/agentmail.service");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&aliased, &link).unwrap();
    std::fs::remove_file(s.unit()).unwrap();
    let n = s.log().len();
    let out = s
        .cli()
        .env("FAKE_ACTIVE", "active")
        .env(
            "FAKE_SHOW",
            format!("LoadState=loaded\nFragmentPath={}\n", aliased.display()),
        )
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("left alone"), "{out}");
    assert_eq!(
        s.log()[n..],
        [
            "--user is-system-running [down]",
            "--user stop agentmail [down]",
            "--user daemon-reload [down]",
        ]
    );
    assert!(std::fs::symlink_metadata(&link).is_err());
}

#[test]
fn service_uninstall_leaves_a_unit_agentmail_did_not_write() {
    let s = ServiceHome::new();
    let mine = "[Service]\nExecStart=/old/agentmail daemon run\nEnvironment=FOO=1\n";
    write(&s.unit(), mine);
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("not the unit agentmail installs"), "{err}");
    assert!(
        err.contains(&format!("rm -f {}", marker(&s.unit()).display())),
        "{err}"
    );
    assert_eq!(read(&s.unit()), mine);
    assert!(!bak(&s.unit()).exists());
    assert!(s.log().is_empty());
}

#[test]
fn service_uninstall_removes_the_readme_unit() {
    // The README's unit, written by hand (whatever binary it runs), is
    // removed, but kept as the backup first, with how to put it back.
    for text in [
        readme_unit(),
        install::service::unit(Path::new("/x/agentmail")).unwrap(),
    ] {
        let s = ServiceHome::new();
        write(&s.unit(), &text);
        let out = s
            .cli()
            .args(["uninstall", "--service"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let out = String::from_utf8(out).unwrap();
        assert!(!s.unit().exists());
        assert_eq!(read(&bak(&s.unit())), text);
        assert!(
            out.contains(&format!("(backup kept as {})", bak(&s.unit()).display())),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "note: uninstall does not put the unit kept as {} back",
                bak(&s.unit()).display()
            )),
            "{out}"
        );
    }
}

#[test]
fn service_install_keeps_a_readme_unit_written_by_hand() {
    // The README's unit, written by hand (its ExecStart `%h/...`, no
    // marker): install keeps it as the backup, and uninstall says how to
    // put it back.
    let s = ServiceHome::new();
    let readme = readme_unit();
    assert!(readme.contains("ExecStart=%h/"), "{readme}");
    write(&s.unit(), &readme);
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(read(&bak(&s.unit())), readme);
    assert!(!marker(&s.unit()).exists());
    assert!(
        out.contains(&format!(
            "updated {} (original kept as {})",
            s.unit().display(),
            bak(&s.unit()).display()
        )),
        "{out}"
    );
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!s.unit().exists());
    assert_eq!(read(&bak(&s.unit())), readme);
    assert!(
        out.contains(&format!(
            "note: uninstall does not put the unit kept as {} back",
            bak(&s.unit()).display()
        )),
        "{out}"
    );
}

#[test]
fn service_never_drops_a_readme_unit_over_an_earlier_kept_unit() {
    // An earlier unit X is kept as the backup, and the README's unit was
    // written by hand in place of agentmail's: neither install nor
    // uninstall discards either.
    let s = ServiceHome::new();
    let x = "[Service]\nExecStart=/x/agentmail daemon run\n";
    let readme = readme_unit();
    write(&bak(&s.unit()), x);
    write(&s.unit(), &readme);
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("different earlier unit"), "{err}");
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("not the unit agentmail installs"), "{err}");
    assert_eq!(read(&s.unit()), readme);
    assert_eq!(read(&bak(&s.unit())), x);
    assert!(s.log().is_empty());

    // agentmail's own unit written over X (by an older binary, say) is
    // replaced and removed as agentmail's, X kept.
    let s = ServiceHome::new();
    write(&s.unit(), x);
    s.cli().args(["install", "--service"]).assert().success();
    write(
        &s.unit(),
        &install::service::unit(Path::new("/old/agentmail")).unwrap(),
    );
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(read(&bak(&s.unit())), x);
    write(
        &s.unit(),
        &install::service::unit(Path::new("/old/agentmail")).unwrap(),
    );
    s.cli().args(["uninstall", "--service"]).assert().success();
    assert!(!s.unit().exists());
    assert_eq!(read(&bak(&s.unit())), x);
}

#[test]
fn service_keeps_edits_to_its_unit() {
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    assert!(marker(&s.unit()).exists());
    let edited = read(&s.unit()).replace("[Service]\n", "[Service]\nEnvironment=FOO=1\n");
    write(&s.unit(), &edited);
    // Uninstall keeps the edited unit as the backup.
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("backup kept as"), "{out}");
    assert!(!s.unit().exists());
    assert_eq!(read(&bak(&s.unit())), edited);

    // Install over an edited unit agentmail created refuses: replacing it
    // would drop the edits from the running service. It shows them and
    // says how to keep them (a drop-in) and go on.
    std::fs::remove_file(bak(&s.unit())).unwrap();
    s.cli().args(["install", "--service"]).assert().success();
    write(&s.unit(), &edited);
    let n = s.log().len();
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("edited since"), "{err}");
    assert!(err.contains("-Environment=FOO=1"), "{err}");
    assert!(err.contains("systemctl --user edit agentmail"), "{err}");
    assert!(
        err.contains(
            &s.unit()
                .with_file_name("agentmail.service.d/override.conf")
                .display()
                .to_string()
        ),
        "{err}"
    );
    assert!(err.contains(&format!("rm {}", s.unit().display())), "{err}");
    assert_eq!(read(&s.unit()), edited);
    assert!(!bak(&s.unit()).exists());
    assert!(marker(&s.unit()).exists());
    assert_eq!(s.log().len(), n, "{:?}", s.log());
    // A dry run refuses alike.
    s.cli()
        .args(["install", "--service", "--dry-run"])
        .assert()
        .failure();
    assert_eq!(read(&s.unit()), edited);
    // An edit only to `ExecStart` is agentmail's to replace.
    let exec_only = install::service::unit(Path::new("/elsewhere/agentmail")).unwrap();
    write(&s.unit(), &exec_only);
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(
        read(&s.unit()),
        install::service::unit(Path::new(&bin_path())).unwrap()
    );
    assert!(!bak(&s.unit()).exists());
    write(&s.unit(), &edited);

    // `--json` names the saved unit.
    s.cli().args(["uninstall", "--service"]).assert().success();
    std::fs::remove_file(bak(&s.unit())).unwrap();
    s.cli().args(["install", "--service"]).assert().success();
    write(&s.unit(), &edited);
    let out = s
        .cli()
        .args(["--json", "uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["changes"][0]["saved"],
        json!(bak(&s.unit()).display().to_string()),
        "{v}"
    );
    assert_eq!(read(&bak(&s.unit())), edited);
}

#[test]
fn service_install_never_drops_an_earlier_kept_unit() {
    let s = ServiceHome::new();
    let a = "[Service]\nExecStart=/a/agentmail daemon run\n";
    let b = "[Service]\nExecStart=/b/agentmail daemon run\n";
    write(&s.unit(), a);
    s.cli().args(["install", "--service"]).assert().success();
    s.cli().args(["uninstall", "--service"]).assert().success();
    assert_eq!(read(&bak(&s.unit())), a);
    write(&s.unit(), b);
    let n = s.log().len();
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("different earlier unit"), "{err}");
    assert_eq!(read(&s.unit()), b);
    assert_eq!(read(&bak(&s.unit())), a);
    assert_eq!(s.log().len(), n);
    // With the old one moved away, B is kept in its place.
    std::fs::remove_file(bak(&s.unit())).unwrap();
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(read(&bak(&s.unit())), b);
    s.cli().args(["uninstall", "--service"]).assert().success();
    assert_eq!(read(&bak(&s.unit())), b);
}

#[test]
fn service_install_keeps_a_unit_already_kept() {
    // The unit there is the one already kept: installing again is fine.
    let s = ServiceHome::new();
    let a = "[Service]\nExecStart=/a/agentmail daemon run\n";
    write(&s.unit(), a);
    s.cli().args(["install", "--service"]).assert().success();
    s.cli().args(["uninstall", "--service"]).assert().success();
    std::fs::rename(bak(&s.unit()), s.unit()).unwrap();
    write(&bak(&s.unit()), a);
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(read(&bak(&s.unit())), a);
}

#[test]
fn service_environment_in_a_drop_in_survives_install_and_uninstall() {
    // The README's way to give the service a data directory: a drop-in,
    // which install and uninstall never touch, so a second install (here,
    // run by another path, which changes `ExecStart`) keeps it applying.
    let s = ServiceHome::new();
    s.cli().args(["install", "--service"]).assert().success();
    let dropin = s.unit().with_file_name("agentmail.service.d/override.conf");
    let conf = "[Service]\nEnvironment=AGENTMAIL_DATA_DIR=%h/agentmail-data\n";
    write(&dropin, conf);
    let link = s.h.dir.path().join("bin/agentmail");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentmail"), &link).unwrap();
    s.cli_as(&link)
        .args(["install", "--service"])
        .assert()
        .success();
    assert_eq!(read(&s.unit()), install::service::unit(&link).unwrap());
    assert_eq!(read(&dropin), conf);
    let out = s
        .cli()
        .args(["uninstall", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!s.unit().exists());
    assert_eq!(read(&dropin), conf);
    assert!(
        out.contains(&format!(
            "note: the service's drop-ins in {} are left in place",
            dropin.parent().unwrap().display()
        )),
        "{out}"
    );
    s.cli().args(["install", "--service"]).assert().success();
    assert_eq!(read(&dropin), conf);
}

#[test]
fn service_notes_settings_of_a_replaced_unit() {
    // A unit of the user's (the README's, written by hand) with an
    // `Environment=` line: install keeps it as the backup and says the
    // service no longer gets that line, and where to put it.
    let s = ServiceHome::new();
    let mine = readme_unit().replace(
        "[Service]\n",
        "[Service]\nEnvironment=AGENTMAIL_DATA_DIR=%h/agentmail-data\n",
    );
    write(&s.unit(), &mine);
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(read(&bak(&s.unit())), mine);
    assert!(
        out.contains(&format!(
            "note: the unit replaced (kept as {}) also set \
             `Environment=AGENTMAIL_DATA_DIR=%h/agentmail-data`, which the service no longer gets",
            bak(&s.unit()).display()
        )),
        "{out}"
    );
    assert!(out.contains("systemctl --user edit agentmail"), "{out}");
    // The README's unit as is sets nothing more: no note.
    let s = ServiceHome::new();
    write(&s.unit(), &readme_unit());
    let out = s
        .cli()
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(!out.contains("also set"), "{out}");
}

#[test]
fn service_stops_a_daemon_started_on_demand_while_it_starts() {
    // No daemon runs when install looks, but a client starts one on
    // demand before the service's takes the lock: install stops it, so
    // the service's does not wait behind it.
    let s = ServiceHome::new();
    let out = s
        .cli()
        .env("FAKE_START_ON_ENABLE", "1")
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        s.log()
            .iter()
            .any(|l| l == "--user enable --now agentmail [down]"),
        "{:?}",
        s.log()
    );
    assert!(
        out.contains(&format!("ran {} daemon stop", bin_path())),
        "{out}"
    );
    assert!(!s.daemon_up(s.cli()));
    // The JSON lists it too.
    let out = s
        .cli()
        .env("FAKE_START_ON_ENABLE", "1")
        .args(["--json", "install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap().ends_with("daemon stop")),
        "{v}"
    );
    assert!(!s.daemon_up(s.cli()));
}

/// The service's location as its environment sets it: by a drop-in, or by
/// the manager's environment (imported, say), the same as or unlike this
/// shell's. Install stops the on-demand daemon there, not at the default
/// location, and notes overrides only where they differ.
#[test]
fn service_stops_the_on_demand_daemon_where_the_service_runs() {
    let s = ServiceHome::new();
    let d = s.h.dir.path();
    let data = d.join("svc-data");
    let at_data = || {
        let mut c = s.cli();
        c.env("AGENTMAIL_DATA_DIR", &data);
        c
    };
    let dropin = format!("Environment=AGENTMAIL_DATA_DIR={}\n", data.display());
    let imported = format!(
        "AGENTMAIL_DATA_DIR={}\nXDG_CONFIG_HOME={}\n",
        data.display(),
        s.config().display()
    );
    for (var, value) in [("FAKE_UNITENV", &dropin), ("FAKE_ENV", &imported)] {
        // The shell has the service's value: `daemon stop` as is, and no
        // note to give the service what it has.
        at_data().args(["daemon", "start"]).assert().success();
        s.cli().args(["daemon", "start"]).assert().success();
        let out = at_data()
            .env(var, value)
            .args(["install", "--service"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains(&format!("ran {} daemon stop", bin_path())),
            "{var}: {out}"
        );
        assert!(!out.contains("note:"), "{var}: {out}");
        assert!(!s.daemon_up(at_data()), "{var}: {out}");
        assert!(
            s.daemon_up(s.cli()),
            "{var}: the default daemon was stopped"
        );

        // The shell has none: `daemon stop` is told the service's
        // location, and the note names the variable.
        at_data().args(["daemon", "start"]).assert().success();
        let out = s
            .cli()
            .env(var, value)
            .args(["install", "--service"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains(&format!(
                "ran env AGENTMAIL_DATA_DIR={} AGENTMAIL_SOCKET={} {} daemon stop",
                data.display(),
                data.join("agentmail.sock").display(),
                bin_path()
            )),
            "{var}: {out}"
        );
        assert!(
            out.contains(&format!("data directory {} (here: ", data.display())),
            "{var}: {out}"
        );
        assert!(
            out.contains("gets AGENTMAIL_DATA_DIR differently"),
            "{var}: {out}"
        );
        assert!(!s.daemon_up(at_data()), "{var}: {out}");
        assert!(
            s.daemon_up(s.cli()),
            "{var}: the default daemon was stopped"
        );
        s.cli().args(["daemon", "stop"]).assert().success();
    }
    // The shell's value unlike the service's: noted, and the daemon at the
    // service's location is the one stopped.
    at_data().args(["daemon", "start"]).assert().success();
    let out = s
        .cli()
        .env("AGENTMAIL_DATA_DIR", d.join("mine"))
        .env("FAKE_UNITENV", &dropin)
        .args(["install", "--service"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("note: AGENTMAIL_DATA_DIR is set here but the service does not get"),
        "{out}"
    );
    assert!(!s.daemon_up(at_data()), "{out}");
    // An environment file the unit names counts too, over its
    // `Environment=` lines; a missing one is skipped.
    let file = d.join("svc.env");
    write(
        &file,
        &format!("AGENTMAIL_DATA_DIR='{}'\n", d.join("from-file").display()),
    );
    let out = s
        .cli()
        .env(
            "FAKE_UNITENV",
            format!(
                "{dropin}EnvironmentFiles={} (ignore_errors=no)\nEnvironmentFiles={} \
                 (ignore_errors=yes)\n",
                file.display(),
                d.join("missing.env").display()
            ),
        )
        .args(["install", "--service", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains(&format!(
            "data directory {} (here: ",
            d.join("from-file").display()
        )),
        "{out}"
    );
}
