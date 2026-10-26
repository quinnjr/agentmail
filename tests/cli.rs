//! Task 9: the command-line interface, driven through the built binary.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

/// The Task 4 body: code fences, a table, `\r\n`, non-ASCII and a trailing
/// newline.
const MARKDOWN: &str = "```rust\nfn x(){}\n```\r\n| a | b |\n| - | - |\nnaïve ✓\n";

const HUMAN_ADDR: &str = "human";

const BIN: &str = env!("CARGO_BIN_EXE_agentmail");

/// A private data directory and socket, with a daemon the CLI autostarts.
/// Dropping it stops the daemon and the fake harness processes.
struct Env {
    dir: TempDir,
    work: PathBuf,
    sleeps: Vec<Child>,
}

impl Env {
    fn new() -> Env {
        let dir = TempDir::new().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        Env {
            dir,
            work,
            sleeps: Vec::new(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("agentmail.sock")
    }

    /// `agentmail` with this environment and no identity.
    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("AGENTMAIL_DATA_DIR", self.data_dir())
            .env("AGENTMAIL_SOCKET", self.socket())
            .env_remove("AGENTMAIL_ADDRESS")
            .env_remove("AGENTMAIL_HUMAN")
            .env_remove("AGENTMAIL_BIN")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("KIRO_SESSION_ID")
            .current_dir(&self.work)
            .timeout(Duration::from_secs(30));
        c
    }

    /// `agentmail` run by a fake harness process named `harness` (a copy of
    /// `/bin/sh`, which stays the CLI's parent), with this environment and
    /// no identity. Arguments added to it go to `agentmail`.
    fn under(&self, harness: &str) -> Command {
        // The trailing `exit` stops the shell from exec'ing the CLI.
        self.under_script(harness, r#""$0" "$@"; exit $?"#)
    }

    /// `agentmail` run as a shell command of opencode session `id`: under a
    /// fake `opencode` process, with OPENCODE_SESSION_ID set for the CLI
    /// only, not for the opencode process itself (opencode sets it for the
    /// commands its bash tool runs).
    fn in_session_of(&self, id: &str) -> Command {
        let mut c = self.under_script(
            "opencode",
            r#"OPENCODE_SESSION_ID="$1"; export OPENCODE_SESSION_ID; shift; "$0" "$@"; exit $?"#,
        );
        c.arg(id);
        c
    }

    /// The fake harness `harness` (a shell) running `script`, with the
    /// agentmail binary as `$0`, this environment and no identity.
    fn under_script(&self, harness: &str, script: &str) -> Command {
        let fake = self.dir.path().join(harness);
        if !fake.exists() {
            std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
        }
        let mut c = Command::new(&fake);
        c.args(["-c", script, BIN])
            .env("AGENTMAIL_DATA_DIR", self.data_dir())
            .env("AGENTMAIL_SOCKET", self.socket())
            .env_remove("AGENTMAIL_ADDRESS")
            .env_remove("AGENTMAIL_HUMAN")
            .env_remove("AGENTMAIL_BIN")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("KIRO_SESSION_ID")
            .current_dir(&self.work)
            .timeout(Duration::from_secs(30));
        c
    }

    /// `agentmail` acting as agent `addr`.
    fn as_agent(&self, addr: &str) -> Command {
        let mut c = self.cmd();
        c.env("AGENTMAIL_ADDRESS", addr);
        c
    }

    /// `agentmail --as human`.
    fn as_human(&self) -> Command {
        let mut c = self.cmd();
        c.arg("--as").arg("human");
        c
    }

    /// Register `name` bound to a fresh `sleep` process; return its address.
    fn agent(&mut self, name: &str) -> String {
        let sleep = StdCommand::new("sleep").arg("300").spawn().unwrap();
        let out = self
            .cmd()
            .args(["register", name, "--json", "--fresh", "--pid"])
            .arg(sleep.id().to_string())
            .output()
            .unwrap();
        self.sleeps.push(sleep);
        assert!(
            out.status.success(),
            "register failed: {}",
            text(&out.stderr)
        );
        let v = json(&out);
        let addr = v["address"].as_str().unwrap().to_string();
        assert!(addr.starts_with(&format!("{name}-")), "{addr}");
        addr
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = StdCommand::new(BIN)
            .args(["daemon", "stop"])
            .env("AGENTMAIL_DATA_DIR", self.data_dir())
            .env("AGENTMAIL_SOCKET", self.socket())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for s in &mut self.sleeps {
            let _ = s.kill();
            let _ = s.wait();
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad json ({e}): {}", text(&out.stdout)))
}

/// Run and require exit 0; return stdout parsed as JSON.
fn ok_json(c: &mut Command) -> Value {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        text(&out.stderr)
    );
    json(&out)
}

/// Run and require exit 0; return stdout.
fn ok_text(c: &mut Command) -> String {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        text(&out.stderr)
    );
    text(&out.stdout)
}

/// Send `body` from `from` to `to` by stdin; return the message id.
fn send_stdin(env: &Env, from: &str, to: &str, body: &[u8]) -> String {
    let v = ok_json(
        env.as_agent(from)
            .args(["send", "--json", "--to", to, "--subject", "s", "-"])
            .write_stdin(body.to_vec()),
    );
    v["id"].as_str().unwrap().to_string()
}

#[test]
fn stdin_markdown_round_trip() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let id = send_stdin(&env, &a, &b, MARKDOWN.as_bytes());
    let v = ok_json(env.as_agent(&b).args(["read", &id, "--json"]));
    assert_eq!(v["body"].as_str().unwrap().as_bytes(), MARKDOWN.as_bytes());
    // The human-readable form prints the body byte for byte too.
    let out = env.as_agent(&b).args(["read", &id]).output().unwrap();
    assert!(out.status.success());
    let shown = out.stdout;
    assert!(
        shown
            .windows(MARKDOWN.len())
            .any(|w| w == MARKDOWN.as_bytes()),
        "{}",
        text(&shown)
    );
}

#[test]
fn body_file_and_inline_equivalent() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let file = env.dir.path().join("body.md");
    std::fs::write(&file, MARKDOWN).unwrap();
    let from_file = ok_json(env.as_agent(&a).args([
        "send",
        "--json",
        "--to",
        &b,
        "--subject",
        "f",
        "--body-file",
        file.to_str().unwrap(),
    ]));
    let inline = ok_json(env.as_agent(&a).args([
        "send",
        "--json",
        "--to",
        &b,
        "--subject",
        "i",
        "--body",
        MARKDOWN,
    ]));
    assert_eq!(from_file["body"], MARKDOWN);
    assert_eq!(inline["body"], MARKDOWN);
    // A relative --body-file is read from the working directory.
    std::fs::write(env.work.join("rel.md"), "rel body").unwrap();
    let rel = ok_json(env.as_agent(&a).args([
        "send",
        "--json",
        "--to",
        &b,
        "--subject",
        "r",
        "--body-file",
        "rel.md",
    ]));
    assert_eq!(rel["body"], "rel body");
}

#[test]
fn missing_body_is_usage_error() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    env.as_agent(&a)
        .args(["send", "--to", &b, "--subject", "s"])
        .assert()
        .code(2);
    // Two body sources are as wrong as none.
    env.as_agent(&a)
        .args(["send", "--to", &b, "--subject", "s", "--body", "x", "-"])
        .write_stdin("y")
        .assert()
        .code(2);
    env.as_agent(&a).args(["answer", "x"]).assert().code(2);
    env.as_agent(&a).args(["reply", "x"]).assert().code(2);
    // A positional other than `-` is not a body source.
    env.as_agent(&a)
        .args(["send", "--to", &b, "--subject", "s", "body-text"])
        .assert()
        .code(2);
    // Only `human` may follow --as.
    env.cmd().args(["--as", "bob", "inbox"]).assert().code(2);
}

#[test]
fn oversize_body_rejected() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let big = vec![b'x'; 262_145];
    let out = env
        .as_agent(&a)
        .args(["send", "--to", &b, "--subject", "s", "-"])
        .write_stdin(big)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("too large"),
        "{}",
        text(&out.stderr)
    );
    // Nothing was delivered, not even a truncated copy.
    let inbox = ok_json(env.as_agent(&b).args(["inbox", "--json"]));
    assert_eq!(inbox.as_array().unwrap().len(), 0);
    // Exactly the limit is fine.
    let id = send_stdin(&env, &a, &b, &vec![b'x'; 262_144]);
    let v = ok_json(env.as_agent(&b).args(["read", &id, "--json"]));
    assert_eq!(v["body"].as_str().unwrap().len(), 262_144);
}

#[test]
fn non_utf8_body_rejected() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let out = env
        .as_agent(&a)
        .args(["send", "--to", &b, "--subject", "s", "-"])
        .write_stdin(vec![b'a', 0xff, b'b'])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("UTF-8"), "{}", text(&out.stderr));
}

#[test]
fn no_identity_errors() {
    let env = Env::new();
    let out = env.cmd().arg("inbox").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains("not registered"), "{err}");
}

#[test]
fn human_can_join_any_thread() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let id = send_stdin(&env, &a, &b, b"hello");
    ok_text(env.as_human().args(["reply", &id, "--body", "x"]));
    let shown = ok_text(env.as_human().args(["show", &id]));
    assert!(shown.contains("human"), "{shown}");
    assert!(shown.contains("[from human]"), "{shown}");
    assert!(shown.contains(&format!("{a} → {b}")), "{shown}");
    assert!(shown.contains("hello") && shown.contains('x'), "{shown}");
    // `show` also takes a message id inside the thread.
    let thread = ok_json(env.as_agent(&a).args(["read", &id, "--json"]));
    let reply_id = thread["thread"][1]["id"].as_str().unwrap().to_string();
    let again = ok_text(env.as_human().args(["show", &reply_id]));
    assert_eq!(again, shown);
    // The agents got the human's reply, marked as from the human (`read`
    // above marked it read for `a`).
    let inbox = ok_json(env.as_agent(&a).args(["inbox", "--json", "--all"]));
    let from_human: Vec<&Value> = inbox
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["from_human"] == true)
        .collect();
    assert_eq!(from_human.len(), 1, "{inbox}");
    let inbox = ok_json(env.as_agent(&b).args(["inbox", "--json"]));
    assert!(
        inbox
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["from"] == HUMAN_ADDR),
        "{inbox}"
    );
}

#[test]
fn ask_pending_exit_code_3() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let started = Instant::now();
    let out = env
        .as_agent(&a)
        .args([
            "ask",
            "--json",
            "--to",
            &b,
            "--subject",
            "q",
            "--timeout",
            "1s",
            "--body",
            "?",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(10));
    let v = json(&out);
    assert_eq!(v["status"], "pending");
    assert!(v["question_id"].is_string());
    // Human-readable form: same exit code, says how to keep waiting.
    let out = env
        .as_agent(&a)
        .args(["ask", "--to", &b, "--subject", "q2", "--timeout", "1s", "-"])
        .write_stdin("?")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        text(&out.stdout).contains("agentmail await"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn answer_then_await() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let out = env
        .as_agent(&a)
        .args([
            "ask",
            "--json",
            "--to",
            &b,
            "--subject",
            "q",
            "--timeout",
            "1s",
            "--body",
            "?",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let qid = json(&out)["question_id"].as_str().unwrap().to_string();
    // Still pending before the answer.
    env.as_agent(&a)
        .args(["await", &qid, "--timeout", "1s"])
        .assert()
        .code(3);
    let ans = ok_json(
        env.as_agent(&b)
            .args(["answer", &qid, "--json", "--body", "ok"]),
    );
    assert_eq!(ans["kind"], "answer");
    let v = ok_json(env.as_agent(&a).args(["await", &qid, "--json"]));
    assert_eq!(v["status"], "answered");
    assert_eq!(v["answer"]["body"], "ok");
    let shown = ok_text(env.as_agent(&a).args(["await", &qid]));
    assert!(shown.contains("ok"), "{shown}");
    // The asker's short id works too, though the question is not in its
    // inbox, in either case.
    let short = &qid[qid.len() - 8..];
    for id in [short.to_string(), short.to_ascii_lowercase()] {
        let v = ok_json(env.as_agent(&a).args(["await", &id, "--json"]));
        assert_eq!(v["status"], "answered", "{id}");
        assert_eq!(v["question_id"], qid.as_str(), "{id}");
    }
    // So does the full id in lower case.
    let v = ok_json(
        env.as_agent(&a)
            .args(["await", &qid.to_ascii_lowercase(), "--json"]),
    );
    assert_eq!(v["status"], "answered");
    // The human can show a thread it is not addressed in by short id.
    let shown = ok_json(env.as_human().args(["show", short, "--json"]));
    assert_eq!(shown["thread"].as_array().unwrap().len(), 2);
}

#[test]
fn ids_are_case_insensitive() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let id = send_stdin(&env, &a, &b, b"hi");
    let v = ok_json(
        env.as_agent(&b)
            .args(["read", &id.to_ascii_lowercase(), "--json"]),
    );
    assert_eq!(v["id"], id.as_str());
    let short = id[id.len() - 8..].to_ascii_lowercase();
    let v = ok_json(env.as_agent(&b).args(["read", &short, "--json"]));
    assert_eq!(v["id"], id.as_str());
    // Four characters are enough; fewer match too much to be useful.
    let v = ok_json(
        env.as_agent(&b)
            .args(["read", &id[id.len() - 4..], "--json"]),
    );
    assert_eq!(v["id"], id.as_str());
    for tiny in [&id[id.len() - 1..], &id[id.len() - 3..]] {
        let out = env
            .as_agent(&b)
            .args(["read", tiny, "--json"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
        let v = json(&out);
        assert_eq!(v["error"]["code"], "InvalidInput", "{v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("at least 4 characters"), "{msg}");
    }
}

#[test]
fn register_refuses_the_human() {
    let env = Env::new();
    let out = env.as_human().args(["register", "foo"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    assert!(text(&out.stderr).contains("human"), "{}", text(&out.stderr));
    let out = env
        .cmd()
        .env("AGENTMAIL_HUMAN", "1")
        .args(["register", "foo"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stdout));
    // Nothing was registered.
    let v = ok_json(env.as_human().args(["agents", "--json", "--all"]));
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");
}

#[test]
fn wait_returns_mail_or_times_out() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    env.as_agent(&b)
        .args(["wait", "--timeout", "1s"])
        .assert()
        .code(3);
    send_stdin(&env, &a, &b, b"ping");
    let v = ok_json(env.as_agent(&b).args(["wait", "--json", "--timeout", "1s"]));
    assert_eq!(v["status"], "mail");
    assert_eq!(v["messages"][0]["body"], "ping");
}

#[test]
fn contact_add_and_agents_marks_contact() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let c = env.agent("docs");
    ok_text(env.as_agent(&a).args(["contact", "add", &b]));
    let listed = ok_json(env.as_agent(&a).args(["contact", "--json"]));
    assert_eq!(listed[0]["address"], b.as_str());
    let agents = ok_json(env.as_agent(&a).args(["agents", "--json"]));
    for row in agents.as_array().unwrap() {
        assert_eq!(row["is_contact"], row["address"] == b.as_str(), "{row}");
    }
    let table = ok_text(env.as_agent(&a).arg("agents"));
    let line_of = |addr: &str| {
        table
            .lines()
            .find(|l| l.contains(addr))
            .unwrap_or_else(|| panic!("{addr} missing: {table}"))
            .to_string()
    };
    assert!(line_of(&b).starts_with('*'), "{table}");
    assert!(!line_of(&c).starts_with('*'), "{table}");
    assert!(line_of(&b).contains("active"), "{table}");
    assert!(line_of(&b).contains("idle"), "{table}");
    // --name filters by prefix.
    let only = ok_json(env.as_agent(&a).args(["agents", "--json", "--name", "doc"]));
    assert_eq!(only.as_array().unwrap().len(), 1);
    ok_text(env.as_agent(&a).args(["contact", "remove", &b]));
    let listed = ok_json(env.as_agent(&a).args(["contact", "list", "--json"]));
    assert_eq!(listed.as_array().unwrap().len(), 0);
}

#[test]
fn status_and_whoami_show_summary() {
    let mut env = Env::new();
    let a = env.agent("backend");
    ok_text(env.as_agent(&a).args(["status", "building the orders API"]));
    let who = ok_text(env.as_agent(&a).arg("whoami"));
    assert!(who.contains(&a), "{who}");
    assert!(who.contains("building the orders API"), "{who}");
    let v = ok_json(env.as_agent(&a).args(["whoami", "--json"]));
    assert_eq!(v["agent"]["working_on"], "building the orders API");
    let human = ok_text(env.as_human().arg("whoami"));
    assert!(human.contains("human"), "{human}");
}

#[test]
fn prune_older_than() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    send_stdin(&env, &a, &b, b"old");
    // Only the human may prune.
    env.as_agent(&a)
        .args(["prune", "--older-than", "0s"])
        .assert()
        .code(1);
    // Nothing is a week old yet.
    let v = ok_json(
        env.as_human()
            .args(["prune", "--json", "--older-than", "7d"]),
    );
    assert_eq!(v["pruned"], 0);
    std::thread::sleep(Duration::from_millis(20));
    let v = ok_json(
        env.as_human()
            .args(["prune", "--json", "--older-than", "0s"]),
    );
    assert_eq!(v["pruned"], 1);
    let inbox = ok_json(env.as_agent(&b).args(["inbox", "--json", "--all"]));
    assert_eq!(inbox.as_array().unwrap().len(), 0);
    // A bad duration is a usage error.
    env.as_human()
        .args(["prune", "--older-than", "soon"])
        .assert()
        .code(2);
}

#[test]
fn inbox_lines_and_short_ids() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let id = ok_json(env.as_agent(&a).args([
        "send",
        "--json",
        "--urgent",
        "--to",
        &b,
        "--subject",
        "Schema change",
        "--body",
        "x",
    ]))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let inbox = ok_text(env.as_agent(&b).arg("inbox"));
    let line = inbox.lines().find(|l| l.contains("Schema change")).unwrap();
    let short = &id[id.len() - 8..];
    assert!(line.contains(short), "{line}");
    assert!(!line.contains(&id), "{line}");
    assert!(line.contains('!'), "{line}");
    assert!(line.contains(&a), "{line}");
    // A short id from the inbox works wherever an id is taken.
    let v = ok_json(env.as_agent(&b).args(["read", short, "--json"]));
    assert_eq!(v["id"], id.as_str());
    ok_text(env.as_agent(&b).args(["reply", short, "--body", "thanks"]));
    let thread = ok_json(env.as_agent(&a).args(["read", &id, "--json"]));
    assert_eq!(thread["thread"].as_array().unwrap().len(), 2);
    // An id nobody has is an error.
    env.as_agent(&b).args(["read", "zzzzzzzz"]).assert().code(1);
}

#[test]
fn send_to_comma_separated_and_repeated() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let c = env.agent("docs");
    let d = env.agent("ops");
    let v = ok_json(env.as_agent(&a).args([
        "send",
        "--json",
        "--to",
        &format!("{b},{c}"),
        "--to",
        &d,
        "--subject",
        "all",
        "--ref",
        "docs/api.md",
        "--ref",
        "src/lib.rs",
        "--body",
        "x",
    ]));
    assert_eq!(v["to"], serde_json::json!([b, c, d]));
    assert_eq!(v["refs"], serde_json::json!(["docs/api.md", "src/lib.rs"]));
}

#[test]
fn threads_and_search() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let id = send_stdin(&env, &a, &b, b"the needle is here");
    let threads = ok_json(env.as_human().args(["threads", "--json", "--agent", &a]));
    assert_eq!(threads[0]["thread_id"], id.as_str());
    let listed = ok_text(env.as_human().arg("threads"));
    assert!(listed.contains(&id), "{listed}");
    let none = ok_json(
        env.as_human()
            .args(["threads", "--json", "--open-questions"]),
    );
    assert_eq!(none.as_array().unwrap().len(), 0);
    let found = ok_json(env.as_human().args(["search", "--json", "needle"]));
    assert_eq!(found[0]["id"], id.as_str());
}

#[test]
fn ambiguous_name_prints_candidates() {
    let mut env = Env::new();
    // Agents in a git repo, so plain names resolve within it.
    let ok = StdCommand::new("git")
        .args(["init", "-q"])
        .current_dir(&env.work)
        .status()
        .unwrap();
    assert!(ok.success());
    let a = env.agent("backend");
    let f1 = env.agent("frontend");
    let f2 = env.agent("frontend");
    for f in [&f1, &f2] {
        ok_text(env.as_agent(f).args(["status", &format!("work of {f}")]));
    }
    let out = env
        .as_agent(&a)
        .args(["send", "--to", "frontend", "--subject", "s", "--body", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.starts_with("error: "), "{err}");
    for f in [&f1, &f2] {
        assert!(err.contains(f.as_str()), "{err}");
        assert!(err.contains(&format!("work of {f}")), "{err}");
    }
    // With --json the error is also printed as JSON on stdout.
    let out = env
        .as_agent(&a)
        .args([
            "send",
            "--json",
            "--to",
            "frontend",
            "--subject",
            "s",
            "--body",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["error"]["code"], "AmbiguousRecipient");
    assert_eq!(v["error"]["candidates"].as_array().unwrap().len(), 2);
}

#[test]
fn signoff_marks_gone() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    ok_text(env.as_agent(&a).arg("signoff"));
    let active = ok_json(env.as_agent(&b).args(["agents", "--json"]));
    assert!(
        active
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["address"] != a.as_str())
    );
    let all = ok_json(env.as_agent(&b).args(["agents", "--json", "--all"]));
    let row = all
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["address"] == a.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["presence"], "gone");
}

#[test]
fn register_binds_the_given_process() {
    let env = Env::new();
    let sleep = StdCommand::new("sleep").arg("300").spawn().unwrap();
    let v = ok_json(env.cmd().args([
        "register",
        "backend",
        "--json",
        "--role",
        "API",
        "--working-on",
        "orders",
        "--harness",
        "kiro",
        "--no-wake",
        "--pid",
        &sleep.id().to_string(),
    ]));
    let agent = &v["agent"];
    assert_eq!(agent["proc_id"]["pid"], sleep.id());
    assert_eq!(agent["harness"], "kiro");
    assert_eq!(agent["role"], "API");
    assert_eq!(agent["working_on"], "orders");
    assert_eq!(agent["wake"], false);
    assert_eq!(
        Path::new(agent["cwd"].as_str().unwrap()),
        env.work.canonicalize().unwrap()
    );
    let mut sleep = sleep;
    let _ = sleep.kill();
    let _ = sleep.wait();
}

#[test]
fn watch_prints_one_line_per_event() {
    let mut env = Env::new();
    // Start the daemon first, so the watcher connects at once.
    ok_text(env.cmd().args(["daemon", "start"]));
    let mut watcher = StdCommand::new(BIN)
        .args(["watch", "--json"])
        .env("AGENTMAIL_DATA_DIR", env.data_dir())
        .env("AGENTMAIL_SOCKET", env.socket())
        .env("AGENTMAIL_HUMAN", "1")
        .env_remove("AGENTMAIL_ADDRESS")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watcher.stdout.take().unwrap()).lines();
    // Register until the watcher has subscribed and reports the event.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Some(Ok(l)) = lines.next() {
            let _ = tx.send(l);
        }
    });
    let line = loop {
        env.agent("backend");
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            break l;
        }
        assert!(Instant::now() < deadline, "watch printed nothing");
    };
    let _ = watcher.kill();
    let _ = watcher.wait();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "presence", "{line}");
    assert!(
        v["agent"]["address"]
            .as_str()
            .unwrap()
            .starts_with("backend-")
    );
}

#[test]
fn daemon_status_and_stop() {
    let env = Env::new();
    env.cmd().args(["daemon", "status"]).assert().code(1);
    ok_text(env.cmd().args(["daemon", "start"]));
    ok_text(env.cmd().args(["daemon", "status"]));
    let out = ok_text(env.cmd().args(["daemon", "stop"]));
    assert!(out.contains("stopped"), "{out}");
}

#[test]
fn daemon_actions_print_json() {
    let env = Env::new();
    let socket = env.socket().display().to_string();
    let out = env
        .cmd()
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["running"], false, "{v}");
    assert!(v["pid"].is_null(), "{v}");
    assert_eq!(v["socket"], socket.as_str());
    assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()), "{v}");

    let v = ok_json(env.cmd().args(["daemon", "start", "--json"]));
    assert_eq!(v["running"], true, "{v}");
    let pid = v["pid"].as_u64().unwrap();
    assert!(pid > 0);

    let v = ok_json(env.cmd().args(["--json", "daemon", "status"]));
    assert_eq!(v["running"], true, "{v}");
    assert_eq!(v["pid"].as_u64(), Some(pid));
    assert!(v["protocol"].as_u64().is_some(), "{v}");
    assert_eq!(v["socket"], socket.as_str());
    assert!(v["reason"].is_null(), "{v}");

    let v = ok_json(env.cmd().args(["daemon", "stop", "--json"]));
    assert_eq!(v["stopped"], true, "{v}");
    let v = ok_json(env.cmd().args(["daemon", "stop", "--json"]));
    assert_eq!(v["stopped"], false, "{v}");
}

#[test]
fn hyphen_leading_body_and_subject_round_trip() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    let body = "- a\n- b";
    let sent = ok_json(env.as_agent(&a).args([
        "send",
        "--to",
        &b,
        "--subject",
        "-x",
        "--body",
        body,
        "--json",
    ]));
    let id = sent["id"].as_str().unwrap();
    let v = ok_json(env.as_agent(&b).args(["read", id, "--json"]));
    assert_eq!(v["body"].as_str().unwrap().as_bytes(), body.as_bytes());
    assert_eq!(v["subject"], "-x");
    // A hyphen-leading working-on summary and search query parse too.
    ok_json(env.as_agent(&a).args(["status", "- reviewing", "--json"]));
    let who = ok_json(env.as_agent(&a).args(["whoami", "--json"]));
    assert_eq!(who["agent"]["working_on"], "- reviewing");
    let found = ok_json(env.as_agent(&b).args(["search", "-x", "--json"]));
    let ids: Vec<&str> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id), "{found}");
}

#[test]
fn empty_recipient_is_usage_error() {
    let mut env = Env::new();
    let a = env.agent("backend");
    let b = env.agent("frontend");
    for to in [format!("{b},,{b}"), format!("{b},")] {
        let out = env
            .as_agent(&a)
            .args(["send", "--to", &to, "--subject", "s", "--body", "x"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
        assert!(text(&out.stderr).contains("empty"), "{}", text(&out.stderr));
    }
}

/// Run `args` with stdout already closed; it must not panic (exit 101).
fn closed_stdout(env: &Env, args: &[&str]) -> std::process::ExitStatus {
    let mut child = StdCommand::new(BIN)
        .args(args)
        .env("AGENTMAIL_DATA_DIR", env.data_dir())
        .env("AGENTMAIL_SOCKET", env.socket())
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .current_dir(&env.work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_ne!(
        out.status.code(),
        Some(101),
        "{args:?} panicked: {}",
        text(&out.stderr)
    );
    assert!(
        !text(&out.stderr).contains("panicked"),
        "{args:?}: {}",
        text(&out.stderr)
    );
    out.status
}

#[test]
fn closed_stdout_does_not_panic() {
    let mut env = Env::new();
    for i in 0..5 {
        env.agent(&format!("agent{i}"));
    }
    for args in [
        &["--as", "human", "agents", "--all", "--json"][..],
        &["--as", "human", "agents", "--all"],
        &["daemon", "status"],
        &["--as", "human", "--help"],
    ] {
        let status = closed_stdout(&env, args);
        assert!(status.success(), "{args:?}: {status:?}");
    }
    // An error with --json prints JSON to the closed stdout too.
    closed_stdout(&env, &["--as", "human", "read", "NOSUCHID", "--json"]);
}

/// Shell commands in two opencode sessions run in the same process tree;
/// `OPENCODE_SESSION_ID` tells them apart.
#[test]
fn opencode_env_session_register_and_whoami() {
    let mut env = Env::new();
    let r1 = ok_json(
        env.in_session_of("s1")
            .args(["register", "backend", "--json"]),
    );
    let backend = r1["address"].as_str().unwrap().to_string();
    let r2 = ok_json(
        env.in_session_of("s2")
            .args(["register", "frontend", "--json"]),
    );
    let frontend = r2["address"].as_str().unwrap().to_string();
    assert_ne!(backend, frontend);

    let me = ok_json(env.in_session_of("s1").args(["whoami", "--json"]));
    assert_eq!(me["address"], backend.as_str(), "{me}");
    assert_eq!(me["agent"]["session"]["harness"], "opencode", "{me}");
    assert_eq!(me["agent"]["session"]["id"], "s1", "{me}");
    let me = ok_json(env.in_session_of("s2").args(["whoami", "--json"]));
    assert_eq!(me["address"], frontend.as_str(), "{me}");

    // The same session registering again keeps its address.
    let again = ok_json(
        env.in_session_of("s1")
            .args(["register", "backend", "--json"]),
    );
    assert_eq!(again["address"], backend.as_str(), "{again}");

    // Without the session, a call under opencode resolves to neither
    // agent through its process tree, and neither does one outside it.
    for mut c in [env.under("opencode"), env.cmd()] {
        let out = c.args(["whoami", "--json"]).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        assert!(
            text(&out.stderr).contains("not registered"),
            "{}",
            text(&out.stderr)
        );
    }

    // Mail to s1's agent is in s1's inbox only.
    let sender = env.agent("sender");
    ok_text(env.as_agent(&sender).args([
        "send",
        "--to",
        &backend,
        "--subject",
        "for s1",
        "--body",
        "x",
    ]));
    let inbox = ok_json(env.in_session_of("s1").args(["inbox", "--json"]));
    assert!(inbox.to_string().contains("for s1"), "{inbox}");
    let inbox = ok_json(env.in_session_of("s2").args(["inbox", "--json"]));
    assert!(!inbox.to_string().contains("for s1"), "{inbox}");
}

/// A session variable inherited from an outer harness does not count: a
/// harness started from an opencode shell is not that opencode session.
#[test]
fn inherited_opencode_session_is_ignored() {
    let env = Env::new();
    let r = ok_json(
        env.in_session_of("s1")
            .args(["register", "backend", "--json"]),
    );
    let backend = r["address"].as_str().unwrap().to_string();
    // `claude` run from that opencode's shell: its commands inherit
    // OPENCODE_SESSION_ID, but run under the claude process.
    let out = env
        .under("claude")
        .env("OPENCODE_SESSION_ID", "s1")
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("not registered"),
        "{}",
        text(&out.stderr)
    );
    // Registering there binds the claude process, not the opencode session.
    let inner = ok_json(
        env.under("claude")
            .env("OPENCODE_SESSION_ID", "s1")
            .args(["register", "inner", "--json"]),
    );
    assert_ne!(inner["address"], backend.as_str(), "{inner}");
    let me = ok_json(env.in_session_of("s1").args(["whoami", "--json"]));
    assert_eq!(me["address"], backend.as_str(), "{me}");
}

/// opencode B started from opencode A's bash tool carries A's
/// OPENCODE_SESSION_ID itself and passes it on to everything it starts,
/// not only its own bash tool's commands. A child of B carrying that same
/// value (its plugin's `Bun.spawn`, an LSP) is not A's session; one of B's
/// own shell commands, with B's session id, still is that session.
#[test]
fn opencode_session_carried_by_the_opencode_itself_is_ignored() {
    let env = Env::new();
    let outer = ok_json(env.in_session_of("outer").args(["register", "a", "--json"]));
    let outer = outer["address"].as_str().unwrap().to_string();
    // B's non-shell child: the value B itself was started with.
    let nested = || {
        let mut c = env.under("opencode");
        c.env("OPENCODE_SESSION_ID", "outer");
        c
    };
    let out = nested().args(["whoami", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("not registered"),
        "{}",
        text(&out.stderr)
    );
    // Registering there is refused as sessionless (B's process is shared),
    // and does not touch A's session.
    let out = nested().args(["register", "b", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let err = json(&out);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("shared between sessions"),
        "{err}"
    );
    let message = err["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("OPENCODE_SESSION_ID is set but"),
        "{message}"
    );
    assert!(!message.contains("is not set"), "{message}");
    let me = ok_json(env.in_session_of("outer").args(["whoami", "--json"]));
    assert_eq!(me["address"], outer.as_str(), "{me}");
    // B's own bash tool command, in B's session `inner`: that session.
    let in_b = || {
        let mut c = env.in_session_of("inner");
        c.env("OPENCODE_SESSION_ID", "outer");
        c
    };
    let inner = ok_json(in_b().args(["register", "b", "--json"]));
    let inner = inner["address"].as_str().unwrap().to_string();
    assert_ne!(inner, outer);
    let me = ok_json(in_b().args(["whoami", "--json"]));
    assert_eq!(me["address"], inner.as_str(), "{me}");
    assert_eq!(me["agent"]["session"]["id"], "inner", "{me}");
}

/// A process detached from opencode (a tmux server or `setsid` job started
/// from its bash tool) keeps OPENCODE_SESSION_ID but has no opencode
/// ancestor, so it is not that session.
#[test]
fn opencode_session_without_opencode_ancestor_is_ignored() {
    let env = Env::new();
    ok_json(
        env.in_session_of("s1")
            .args(["register", "backend", "--json"]),
    );
    let out = env
        .cmd()
        .env("OPENCODE_SESSION_ID", "s1")
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("not registered"),
        "{}",
        text(&out.stderr)
    );
}

/// kiro's MCP server registers by process alone (kiro passes it no session
/// id), while kiro's shell commands carry KIRO_SESSION_ID. The CLI in that
/// shell still resolves to the agent the MCP server registered.
#[test]
fn kiro_cli_after_mcp_register_resolves_by_process() {
    let env = Env::new();
    // One kiro-cli process: register as its MCP server does (bound to
    // kiro-cli, no session), then run `whoami` and `register` as its shell
    // commands do, with KIRO_SESSION_ID set. One JSON document per line.
    let script = r#"
        "$0" register kiroagent --json --pid $$ || exit 1
        KIRO_SESSION_ID=k1 "$0" whoami --json || exit 2
        KIRO_SESSION_ID=k1 "$0" register kiroagent --json || exit 3
        echo $$
    "#;
    let out = env.under_script("kiro-cli", script).output().unwrap();
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status.code(),
        text(&out.stderr)
    );
    let stdout = text(&out.stdout);
    let docs: Vec<Value> = serde_json::Deserializer::from_str(&stdout)
        .into_iter()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("bad json ({e}): {stdout}"));
    let [registered, me, again, pid] = &docs[..] else {
        panic!("expected 4 documents: {stdout}");
    };
    let address = registered["address"].as_str().unwrap();
    assert_eq!(me["address"], address, "{me}");
    assert_eq!(me["agent"]["harness"], "kiro", "{me}");
    assert_eq!(me["agent"]["proc_id"]["pid"], *pid, "{me}");
    // The shell's register is the same agent, now with kiro's session.
    assert_eq!(again["address"], address, "{again}");
    let me = ok_json(env.as_agent(address).args(["whoami", "--json"]));
    assert_eq!(me["agent"]["session"]["harness"], "kiro", "{me}");
    assert_eq!(me["agent"]["session"]["id"], "k1", "{me}");
}

/// opencode runs every session in one process, so a `register` under it
/// without OPENCODE_SESSION_ID would make the agent the identity of the
/// whole shared process. It is refused, whatever `--harness` says, and no
/// agent is created.
#[test]
fn opencode_register_without_session_is_refused() {
    let env = Env::new();
    for extra in [&[][..], &["--harness", "other"][..]] {
        let out = env
            .under("opencode")
            .args(["register", "plain", "--json"])
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        let err = json(&out);
        assert_eq!(err["error"]["code"], "InvalidInput", "{err}");
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("OPENCODE_SESSION_ID"), "{message}");
        assert!(message.contains("shared between sessions"), "{message}");
        assert!(message.contains("outside this opencode"), "{message}");
    }
    // `--pid` naming the opencode process itself, or a shell below it (a
    // non-harness ancestor, redirected to the opencode above it), is
    // refused the same way.
    for script in [
        r#""$0" register plain --json --pid $$; exit $?"#,
        r#"/bin/sh -c '"$0" register plain --json --pid $$; exit $?' "$0"; exit $?"#,
    ] {
        let out = env.under_script("opencode", script).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "{script}: {}",
            text(&out.stdout)
        );
        let err = json(&out);
        assert_eq!(err["error"]["code"], "InvalidInput", "{err}");
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("shared between sessions"), "{message}");
    }
    let v = ok_json(env.as_human().args(["agents", "--json", "--all"]));
    assert_eq!(v, serde_json::json!([]), "{v}");
}

/// A session fixes its harness: `--harness` naming another is refused up
/// front, with no agent created, and naming the session's own is accepted.
#[test]
fn register_harness_override_conflicting_with_session_is_refused() {
    let env = Env::new();
    for other in ["other", "claude-code", "kiro"] {
        let out = env
            .in_session_of("s1")
            .args(["register", "plain", "--json", "--harness", other])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
        let err = json(&out);
        assert_eq!(err["error"]["code"], "InvalidInput", "{err}");
        let message = err["error"]["message"].as_str().unwrap();
        assert!(message.contains("fixed by the session"), "{message}");
        assert!(message.contains(other), "{message}");
    }
    let v = ok_json(env.as_human().args(["agents", "--json", "--all"]));
    assert_eq!(v, serde_json::json!([]), "{v}");
    let r = ok_json(env.in_session_of("s1").args([
        "register",
        "plain",
        "--json",
        "--harness",
        "opencode",
    ]));
    let me = ok_json(
        env.as_agent(r["address"].as_str().unwrap())
            .args(["whoami", "--json"]),
    );
    assert_eq!(me["agent"]["harness"], "opencode", "{me}");
}

/// A process shared by every opencode session cannot be taken over by a
/// registration carrying another harness's session: in a kiro shell
/// (KIRO_SESSION_ID set), `--pid` naming an unrelated opencode process is
/// refused, or the agent would become the caller of every sessionless call
/// under that opencode. No agent is created.
#[test]
fn register_other_session_bound_to_opencode_is_refused() {
    let mut env = Env::new();
    let fake = env.dir.path().join("opencode");
    std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    // The trailing `exit` keeps the shell (named opencode) from exec'ing.
    let opencode = StdCommand::new(&fake)
        .args(["-c", "sleep 300; exit"])
        .spawn()
        .unwrap();
    let pid = opencode.id().to_string();
    env.sleeps.push(opencode);
    let out = env
        .under("kiro-cli")
        .env("KIRO_SESSION_ID", "k1")
        .args(["register", "intruder", "--json", "--pid", &pid])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let err = json(&out);
    assert_eq!(err["error"]["code"], "InvalidInput", "{err}");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains("shared between sessions"), "{message}");
    assert!(
        message.contains("carries a kiro session, not an opencode one"),
        "{message}"
    );
    let v = ok_json(env.as_human().args(["agents", "--json", "--all"]));
    assert_eq!(v, serde_json::json!([]), "{v}");
}

/// OPENCODE_SESSION_ID set in a command that does not run under the
/// opencode process `--pid` names (a detached job, or any shell outside
/// it) is ignored: the refusal says the variable is set but does not count,
/// not that it is unset. No agent is created.
#[test]
fn register_ignored_opencode_session_bound_to_opencode_is_refused() {
    let mut env = Env::new();
    let fake = env.dir.path().join("opencode");
    std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    // The trailing `exit` keeps the shell (named opencode) from exec'ing.
    let opencode = StdCommand::new(&fake)
        .args(["-c", "sleep 300; exit"])
        .spawn()
        .unwrap();
    let pid = opencode.id().to_string();
    env.sleeps.push(opencode);
    let out = env
        .cmd()
        .env("OPENCODE_SESSION_ID", "s9")
        .args(["register", "x", "--json", "--pid", &pid])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let err = json(&out);
    assert_eq!(err["error"]["code"], "InvalidInput", "{err}");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains("shared between sessions"), "{message}");
    assert!(
        message.contains(
            "OPENCODE_SESSION_ID is set but does not name a session of this opencode process"
        ),
        "{message}"
    );
    assert!(!message.contains("is not set"), "{message}");
    let v = ok_json(env.as_human().args(["agents", "--json", "--all"]));
    assert_eq!(v, serde_json::json!([]), "{v}");
}
