//! Harness Task 5: `agentmail install` / `uninstall` for Claude Code.
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
        ]})
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
        ]})
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
