//! Harness Task 6: `agentmail hook status|wait --harness opencode`, the
//! JSON the opencode plugin reads.
//!
//! opencode runs every session in one shared process, so the plugin names
//! the session (`--session`) and the hook acts as the agent bound to it.
//! Each test has its own daemon; the agents are bound to this test
//! process's parent, standing in for the opencode process.

mod common;

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

use agentmail::hook::{self, HookArgs, HookEvent, HookOutput};
use agentmail::model::{Harness, ProcId, SessionKey};
use agentmail::paths::Paths;
use agentmail::procinfo;
use agentmail::protocol::{Caller, Request};
use common::{TestDaemon, send_req};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The environment a hook sees: this daemon's paths, plus `extra`.
#[derive(Clone)]
struct Env(HashMap<String, OsString>);

impl Env {
    fn of(paths: &Paths) -> Env {
        let mut m = HashMap::new();
        m.insert("AGENTMAIL_DATA_DIR".into(), paths.data_dir.clone().into());
        m.insert("AGENTMAIL_SOCKET".into(), paths.socket.clone().into());
        Env(m)
    }

    fn with(mut self, k: &str, v: &str) -> Env {
        self.0.insert(k.into(), v.into());
        self
    }

    fn get(&self, k: &str) -> Option<OsString> {
        self.0.get(k).cloned()
    }
}

fn parent() -> ProcId {
    procinfo::ancestors(std::process::id())[0]
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Opencode,
        id: id.to_string(),
    }
}

/// Register `name` for the opencode session `session`.
async fn register(d: &TestDaemon, name: &str, session: &str, wake: bool) -> String {
    let mut c = d.client().await;
    let data = c
        .call(
            &Caller::Human,
            Request::Register {
                name: name.into(),
                role: None,
                working_on: None,
                harness: Harness::Opencode,
                cwd: d.dir.path().to_path_buf(),
                proc_id: Some(parent()),
                fresh: false,
                wake,
                session: Some(key(session)),
            },
        )
        .await
        .unwrap();
    data["address"].as_str().unwrap().to_string()
}

async fn send(d: &TestDaemon, to: &str, subject: &str, urgent: bool) {
    let mut c = d.client().await;
    let mut req = send_req(&[to], subject, "see the PR");
    if let Request::Send { urgent: u, .. } = &mut req {
        *u = urgent;
    }
    c.call(&Caller::Human, req).await.unwrap();
}

async fn ask(d: &TestDaemon, to: &str, subject: &str) {
    let mut c = d.client().await;
    c.call(
        &Caller::Human,
        Request::Ask {
            to: vec![to.into()],
            subject: subject.into(),
            body: "which fields?".into(),
            refs: Vec::new(),
            urgent: false,
            timeout_secs: 0,
        },
    )
    .await
    .unwrap();
}

async fn run(
    env: &Env,
    event: HookEvent,
    session: Option<&str>,
    surface_urgent: bool,
    stdin: &[u8],
) -> HookOutput {
    let args = HookArgs {
        session,
        surface_urgent,
        timeout: None,
    };
    hook::run_with(Harness::Opencode, event, &args, stdin, &|k| env.get(k)).await
}

/// The hook's stdout as JSON, checking it exited 0 with nothing on stderr.
fn json_of(out: &HookOutput) -> Value {
    assert_eq!(out.exit, 0, "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

async fn status(env: &Env, session: &str, surface_urgent: bool) -> Value {
    json_of(&run(env, HookEvent::Status, Some(session), surface_urgent, b"").await)
}

/// Start `hook wait` for `session` on its own task; `continued` is the
/// plugin's `stop_hook_active` (the turn followed one of its own wakes).
fn spawn_wait(
    env: &Env,
    session: &str,
    continued: bool,
    timeout: Option<Duration>,
) -> tokio::task::JoinHandle<Value> {
    spawn_wait_with(
        env,
        session,
        json!({ "stop_hook_active": continued }),
        timeout,
    )
}

/// [`spawn_wait`] with the whole stdin payload.
fn spawn_wait_with(
    env: &Env,
    session: &str,
    payload: Value,
    timeout: Option<Duration>,
) -> tokio::task::JoinHandle<Value> {
    let env = env.clone();
    let session = session.to_string();
    tokio::spawn(async move {
        let stdin = payload.to_string();
        let args = HookArgs {
            session: Some(&session),
            surface_urgent: false,
            timeout,
        };
        let out = hook::run_with(
            Harness::Opencode,
            HookEvent::Wait,
            &args,
            stdin.as_bytes(),
            &|k| env.get(k),
        )
        .await;
        json_of(&out)
    })
}

/// Give a spawned wait time to subscribe before acting.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

async fn joined(h: tokio::task::JoinHandle<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(15), h)
        .await
        .expect("wait did not end")
        .unwrap()
}

#[tokio::test]
async fn status_json_shape() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "schema", false).await;
    send(&d, &a, "prod is down", true).await;
    ask(&d, &a, "POST /orders body?").await;
    let env = Env::of(&d.paths);

    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["address"], a.as_str());
    assert_eq!(v["name"], "backend");
    assert_eq!(v["wake"], true);
    // Two messages and the question's own message.
    assert_eq!(v["unread"], 3, "{v}");
    assert_eq!(v["unread_ids"].as_array().unwrap().len(), 3, "{v}");
    let questions = v["open_questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1, "{v}");
    assert_eq!(questions[0]["subject"], "POST /orders body?");
    let urgent = v["urgent"].as_array().unwrap();
    assert_eq!(urgent.len(), 1, "{v}");
    assert_eq!(urgent[0]["subject"], "prod is down");

    // The words to show, rendered as the other harnesses render them.
    let text = &v["text"];
    let guidance = text["guidance"].as_str().unwrap();
    assert!(guidance.contains(&a), "{guidance}");
    let reminder = text["reminder"].as_str().unwrap();
    assert!(
        reminder.contains(&a) && reminder.contains("3 unread"),
        "{reminder}"
    );
    let urgent_text = text["urgent"].as_str().unwrap();
    assert!(urgent_text.contains("prod is down"), "{urgent_text}");
    assert!(urgent_text.contains("see the PR"), "{urgent_text}");
    let questions_text = text["questions"].as_str().unwrap();
    assert!(
        questions_text.contains("POST /orders body?"),
        "{questions_text}"
    );

    // Urgent mail is surfaced once: the next status has none.
    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["urgent"], json!([]), "{v}");
    assert_eq!(v["text"]["urgent"], Value::Null, "{v}");
    assert_eq!(v["unread"], 3, "surfacing is not reading: {v}");

    // Without `--surface-urgent` urgent mail stays unsurfaced.
    send(&d, &a, "again", true).await;
    let v = status(&env, "ses_a", false).await;
    assert_eq!(v["urgent"].as_array().unwrap().len(), 1, "{v}");
    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["urgent"].as_array().unwrap().len(), 1, "{v}");
    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["urgent"], json!([]), "{v}");
}

#[tokio::test]
async fn status_nothing_to_say_is_null_text() {
    let d = TestDaemon::start().await;
    register(&d, "backend", "ses_a", false).await;
    let env = Env::of(&d.paths);
    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["unread"], 0);
    assert_eq!(v["wake"], false);
    assert_eq!(v["text"]["reminder"], Value::Null, "{v}");
    assert_eq!(v["text"]["urgent"], Value::Null, "{v}");
    assert_eq!(v["text"]["questions"], Value::Null, "{v}");
    assert!(v["text"]["guidance"].is_string(), "{v}");
}

#[tokio::test]
async fn status_is_per_session() {
    // Two sessions of one opencode process: each sees only its own mail.
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    let b = register(&d, "frontend", "ses_b", true).await;
    assert_ne!(a, b);
    send(&d, &a, "for a", true).await;
    let env = Env::of(&d.paths);
    let vb = status(&env, "ses_b", true).await;
    assert_eq!(vb["address"], b.as_str());
    assert_eq!(vb["unread"], 0, "{vb}");
    assert_eq!(vb["urgent"], json!([]), "{vb}");
    let va = status(&env, "ses_a", true).await;
    assert_eq!(va["unread"], 1, "{va}");
    assert_eq!(va["urgent"].as_array().unwrap().len(), 1, "{va}");
}

#[tokio::test]
async fn status_empty_when_unregistered() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "schema", false).await;
    let env = Env::of(&d.paths);
    assert_eq!(status(&env, "ses_unknown", true).await, json!({}));
    // `AGENTMAIL_ADDRESS` (inherited by the opencode process) never names
    // the session's agent.
    let addr = env.clone().with("AGENTMAIL_ADDRESS", &a);
    assert_eq!(status(&addr, "ses_unknown", true).await, json!({}));
    // The human has nothing to say.
    let human = env.clone().with("AGENTMAIL_HUMAN", "1");
    assert_eq!(status(&human, "ses_a", true).await, json!({}));
    // No session: opencode's shared process is nobody's.
    let out = run(&env, HookEvent::Status, None, true, b"").await;
    assert_eq!(json_of(&out), json!({}));
}

#[tokio::test]
async fn status_empty_when_daemon_down_and_spawns_nothing() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let env = Env::of(&paths).with("AGENTMAIL_BIN", "/nonexistent/agentmail");
    assert_eq!(status(&env, "ses_a", true).await, json!({}));
    let wait = joined(spawn_wait(&env, "ses_a", false, None)).await;
    assert_eq!(wait, json!({ "gone": true }));
    let off = run(&env, HookEvent::Signoff, Some("ses_a"), false, b"").await;
    assert_eq!(json_of(&off), json!({}));
    assert!(!paths.socket.exists(), "a daemon socket appeared");
    assert!(!paths.lock.exists(), "a daemon lock appeared");
    assert!(!paths.db.exists(), "a database appeared");
}

async fn whoami(d: &TestDaemon, caller: &Caller) -> Value {
    let mut c = d.client().await;
    c.call(caller, Request::Whoami).await.unwrap()
}

#[tokio::test]
async fn signoff_acts_only_as_the_session_agent() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    let b = register(&d, "frontend", "ses_b", true).await;
    // A process-level agent bound to the same (opencode) process: a
    // session-less sign-off would find it by process ancestry.
    let mut c = d.client().await;
    let docs = c
        .call(
            &Caller::Human,
            Request::Register {
                name: "docs".into(),
                role: None,
                working_on: None,
                harness: Harness::Opencode,
                cwd: d.dir.path().to_path_buf(),
                proc_id: Some(parent()),
                fresh: false,
                wake: true,
                session: None,
            },
        )
        .await
        .unwrap()["address"]
        .as_str()
        .unwrap()
        .to_string();
    let env = Env::of(&d.paths).with("AGENTMAIL_ADDRESS", &docs);
    let procs = Caller::Procs {
        procs: procinfo::ancestors(std::process::id()),
    };
    assert_eq!(whoami(&d, &procs).await["address"], docs.as_str());

    // An unregistered session signs nobody off.
    let out = run(&env, HookEvent::Signoff, Some("ses_unknown"), false, b"").await;
    assert_eq!(json_of(&out), json!({}));
    // No session: nobody's.
    let out = run(&env, HookEvent::Signoff, None, false, b"").await;
    assert_eq!(json_of(&out), json!({}));
    let v = whoami(&d, &procs).await;
    assert_eq!(v["address"], docs.as_str());
    assert_eq!(v["agent"]["signed_off"], false, "{v}");

    // The session's own agent, and only it.
    let out = run(&env, HookEvent::Signoff, Some("ses_a"), false, b"").await;
    assert_eq!(
        json_of(&out),
        json!({ "signed_off": true, "address": a.as_str() })
    );
    assert_eq!(status(&env, "ses_a", false).await, json!({}));
    assert_eq!(status(&env, "ses_b", false).await["address"], b.as_str());
    let v = whoami(&d, &procs).await;
    assert_eq!(v["address"], docs.as_str());
    assert_eq!(v["agent"]["signed_off"], false, "{v}");
    // Again: the session has no agent any more.
    let out = run(&env, HookEvent::Signoff, Some("ses_a"), false, b"").await;
    assert_eq!(json_of(&out), json!({}));
}

#[tokio::test]
async fn other_events_are_silent() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "schema", true).await;
    let env = Env::of(&d.paths);
    for event in [
        HookEvent::SessionStart,
        HookEvent::Prompt,
        HookEvent::PostTool,
        HookEvent::PreTool,
        HookEvent::Stop,
    ] {
        let out = run(&env, event, Some("ses_a"), true, b"").await;
        assert_eq!(out, HookOutput::default(), "{event:?}");
    }
    // Nothing was surfaced by them.
    let v = status(&env, "ses_a", true).await;
    assert_eq!(v["urgent"].as_array().unwrap().len(), 1, "{v}");
}

#[tokio::test]
async fn wait_returns_mail_and_gone() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    let env = Env::of(&d.paths);

    let wait = spawn_wait(&env, "ses_a", false, None);
    settle().await;
    assert!(!wait.is_finished(), "no mail yet");
    send(&d, &a, "schema", false).await;
    let v = joined(wait).await;
    assert_eq!(v["mail"], true, "{v}");
    let text = v["text"].as_str().unwrap();
    assert!(text.contains(&a) && text.contains("1 unread"), "{text}");

    // After a wake the same unread mail does not wake again; signing off
    // ends the wait.
    let wait = spawn_wait(&env, "ses_a", true, None);
    settle().await;
    assert!(!wait.is_finished(), "woke again for the same mail");
    let mut c = d.client().await;
    c.call(
        &Caller::Session {
            session: key("ses_a"),
        },
        Request::Signoff,
    )
    .await
    .unwrap();
    assert_eq!(joined(wait).await, json!({ "gone": true }));

    // An unknown session has nothing to wait for.
    let v = joined(spawn_wait(&env, "ses_unknown", false, None)).await;
    assert_eq!(v, json!({ "gone": true }));
}

#[tokio::test]
async fn wait_wakes_at_once_for_unread_mail_once() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "schema", false).await;
    let env = Env::of(&d.paths);
    // A plain end of turn with unread mail wakes at once...
    let v = joined(spawn_wait(&env, "ses_a", false, None)).await;
    assert_eq!(v["mail"], true, "{v}");
    // ...but only once for that mail, even after a plain turn.
    let wait = spawn_wait(&env, "ses_a", false, Some(Duration::from_secs(1)));
    let v = joined(wait).await;
    assert_eq!(v["mail"], false, "{v}");
    assert_eq!(v["known"].as_array().unwrap().len(), 1, "{v}");
}

#[tokio::test]
async fn wait_times_out_without_mail() {
    let d = TestDaemon::start().await;
    register(&d, "backend", "ses_a", true).await;
    let env = Env::of(&d.paths);
    let started = std::time::Instant::now();
    let v = joined(spawn_wait(
        &env,
        "ses_a",
        false,
        Some(Duration::from_secs(1)),
    ))
    .await;
    assert_eq!(v, json!({ "mail": false, "known": [] }));
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[tokio::test]
async fn wait_after_timeout_counts_mail_that_came_between() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "schema", false).await;
    let env = Env::of(&d.paths);
    let short = Some(Duration::from_secs(1));
    // Woken for the first message; the woken turn ends without reading it.
    assert_eq!(
        joined(spawn_wait(&env, "ses_a", false, None)).await["mail"],
        true
    );
    let v = joined(spawn_wait(&env, "ses_a", true, short)).await;
    let known = v["known"].clone();
    assert_eq!(v["mail"], false, "{v}");
    assert_eq!(known.as_array().unwrap().len(), 1, "{v}");

    // Mail delivered after that wait timed out, before the next one looks.
    send(&d, &a, "between", false).await;

    // Without `known` the next wait counts it as already there...
    let v = joined(spawn_wait(&env, "ses_a", true, short)).await;
    assert_eq!(v["mail"], false, "{v}");
    assert_eq!(v["known"].as_array().unwrap().len(), 2, "{v}");
    // ...with the timed-out wait's `known` it is news, at once.
    let payload = json!({ "stop_hook_active": true, "known": known });
    let v = joined(spawn_wait_with(&env, "ses_a", payload, short)).await;
    assert_eq!(v["mail"], true, "{v}");
    assert!(v["text"].as_str().unwrap().contains("2 unread"), "{v}");
}

#[tokio::test]
async fn wait_respects_no_wake() {
    let d = TestDaemon::start().await;
    register(&d, "backend", "ses_a", false).await;
    let env = Env::of(&d.paths);
    let v = joined(spawn_wait(&env, "ses_a", false, None)).await;
    assert_eq!(v, json!({ "skip": true }));
}

#[tokio::test]
async fn wait_is_busy_while_questions_continue_the_turn() {
    // On a plain end of turn with open questions the plugin queues them,
    // which continues the session: the wait does not hold the turn.
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    ask(&d, &a, "POST /orders body?").await;
    let env = Env::of(&d.paths);
    let v = joined(spawn_wait(&env, "ses_a", false, None)).await;
    assert_eq!(v, json!({ "busy": true }));
    // The continuation's own end of turn waits for new mail only.
    let wait = spawn_wait(&env, "ses_a", true, None);
    settle().await;
    assert!(!wait.is_finished(), "woke for the question already shown");
    send(&d, &a, "schema", false).await;
    assert_eq!(joined(wait).await["mail"], true);
}

const BIN: &str = env!("CARGO_BIN_EXE_agentmail");

fn cli(paths: &Paths, args: &[&str], stdin: &str) -> std::process::Output {
    assert_cmd::Command::new(BIN)
        .args(args)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("KIRO_SESSION_ID")
        .env_remove("CLAUDE_PID")
        .write_stdin(stdin)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap()
}

fn stdout_json(out: &std::process::Output) -> Value {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

// The daemon runs in this process: the blocking CLI call needs another
// worker thread to serve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_status_and_wait() {
    let d = TestDaemon::start().await;
    let a = register(&d, "backend", "ses_a", true).await;
    send(&d, &a, "prod is down", true).await;
    let base = [
        "hook",
        "status",
        "--harness",
        "opencode",
        "--session",
        "ses_a",
    ];

    let v = stdout_json(&cli(
        &d.paths,
        &[&base[..], &["--surface-urgent"]].concat(),
        "",
    ));
    assert_eq!(v["address"], a.as_str());
    assert_eq!(v["urgent"].as_array().unwrap().len(), 1, "{v}");
    let v = stdout_json(&cli(
        &d.paths,
        &[&base[..], &["--surface-urgent"]].concat(),
        "",
    ));
    assert_eq!(v["urgent"], json!([]), "{v}");

    let wait = [
        "hook",
        "wait",
        "--harness",
        "opencode",
        "--session",
        "ses_a",
        "--timeout",
        "1",
    ];
    // The plain end of a turn with unread mail wakes at once.
    let v = stdout_json(&cli(&d.paths, &wait, r#"{"stop_hook_active":false}"#));
    assert_eq!(v["mail"], true, "{v}");
    // After that wake, the same mail does not wake it again: timeout.
    let v = stdout_json(&cli(&d.paths, &wait, r#"{"stop_hook_active":true}"#));
    assert_eq!(v["mail"], false, "{v}");
    assert_eq!(v["known"].as_array().unwrap().len(), 1, "{v}");

    // `hook signoff` signs the session's agent off.
    let off = [
        "hook",
        "signoff",
        "--harness",
        "opencode",
        "--session",
        "ses_a",
    ];
    let v = stdout_json(&cli(&d.paths, &off, ""));
    assert_eq!(v, json!({ "signed_off": true, "address": a.as_str() }));
    assert_eq!(stdout_json(&cli(&d.paths, &off, "")), json!({}));

    let unknown = [
        "hook",
        "status",
        "--harness",
        "opencode",
        "--session",
        "nope",
    ];
    assert_eq!(stdout_json(&cli(&d.paths, &unknown, "")), json!({}));
}

#[test]
fn cli_daemon_down_prints_empty_status_and_gone() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let status = ["hook", "status", "--harness", "opencode", "--session", "s"];
    assert_eq!(stdout_json(&cli(&paths, &status, "")), json!({}));
    let wait = ["hook", "wait", "--harness", "opencode", "--session", "s"];
    assert_eq!(
        stdout_json(&cli(&paths, &wait, "")),
        json!({ "gone": true })
    );
    // The plugin's sign-off of a deleted session, with an
    // `OPENCODE_SESSION_ID` set as well: still nothing started.
    let off = ["hook", "signoff", "--harness", "opencode", "--session", "s"];
    let out = assert_cmd::Command::new(BIN)
        .args(off)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .env("OPENCODE_SESSION_ID", "s")
        .env_remove("AGENTMAIL_ADDRESS")
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out), json!({}));
    assert!(!paths.socket.exists(), "a daemon socket appeared");
    assert!(!paths.lock.exists(), "a daemon lock appeared");
    assert!(!paths.db.exists(), "a database appeared");
}
