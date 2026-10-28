//! Harness Task 8: `agentmail hook` with kiro-cli V3's events.
//!
//! kiro runs a hook command through `sh -c` under its engine's `node`,
//! below `kiro-cli`; the hook resolves its agent by that ancestry, as
//! Claude Code's hooks do. Each test registers its agent bound to this
//! test process's parent, which is in the ancestry of `hook::run_with`
//! (called in process), and has its own daemon.

mod common;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use agentmail::hook::{self, HookArgs, HookEvent, HookOutput};
use agentmail::model::{Harness, ProcId};
use agentmail::paths::Paths;
use agentmail::procinfo;
use agentmail::protocol::{Caller, Request};
use common::{TestDaemon, agent, send_req};
use tempfile::TempDir;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks/kiro")
        .join(format!("{name}.json"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The environment a hook sees: this daemon's paths, plus `extra`.
#[derive(Clone)]
struct Env(HashMap<String, OsString>);

impl Env {
    fn of(paths: &Paths) -> Env {
        let mut m = HashMap::new();
        m.insert("AGENTMAIL_DATA_DIR".into(), paths.data_dir.clone().into());
        m.insert("AGENTMAIL_SOCKET".into(), paths.socket.clone().into());
        // kiro sets it for every hook command.
        m.insert(
            "KIRO_SESSION_ID".into(),
            "sess_8b70a346-1388-49b5-8819-90066699d42d".into(),
        );
        Env(m)
    }

    fn get(&self, k: &str) -> Option<OsString> {
        self.0.get(k).cloned()
    }
}

/// Run `event` as `agentmail hook <event> --harness kiro [--wake]`.
async fn run_wake(env: &Env, event: HookEvent, payload: &str, wake: bool) -> HookOutput {
    let args = HookArgs {
        wake,
        ..HookArgs::default()
    };
    hook::run_with(Harness::Kiro, event, &args, &fixture(payload), &|k| {
        env.get(k)
    })
    .await
}

async fn run(env: &Env, event: HookEvent, payload: &str) -> HookOutput {
    run_wake(env, event, payload, false).await
}

/// The Stop hook installed with `--kiro-wake`, on its own task.
fn spawn_stop(env: &Env) -> tokio::task::JoinHandle<HookOutput> {
    let env = env.clone();
    tokio::spawn(async move { run_wake(&env, HookEvent::Stop, "stop", true).await })
}

fn parent() -> ProcId {
    procinfo::ancestors(std::process::id())[0]
}

/// Register `name` bound to this test process's parent (the "harness").
async fn register_here(d: &TestDaemon, name: &str, wake: bool) -> String {
    let mut c = d.client().await;
    let data = c
        .call(
            &Caller::Human,
            Request::Register {
                name: name.into(),
                role: None,
                working_on: None,
                harness: Harness::Kiro,
                cwd: d.dir.path().to_path_buf(),
                proc_id: Some(parent()),
                fresh: true,
                wake,
                session: None,
            },
        )
        .await
        .unwrap();
    data["address"].as_str().unwrap().to_string()
}

async fn send(d: &TestDaemon, to: &str, subject: &str, body: &str, urgent: bool) {
    let mut c = d.client().await;
    let mut req = send_req(&[to], subject, body);
    if let Request::Send { urgent: u, .. } = &mut req {
        *u = urgent;
    }
    c.call(&Caller::Human, req).await.unwrap();
}

async fn ask(d: &TestDaemon, to: &str, subject: &str) -> String {
    let mut c = d.client().await;
    let data = c
        .call(
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
    data["question_id"].as_str().unwrap().to_string()
}

fn silent(out: &HookOutput) -> bool {
    out.stdout.is_empty() && out.stderr.is_empty() && out.exit == 0
}

/// Whether `handle` is still running after `secs`.
async fn still_waiting<T>(handle: &tokio::task::JoinHandle<T>, secs: f64) -> bool {
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    !handle.is_finished()
}

async fn finish(handle: tokio::task::JoinHandle<HookOutput>) -> HookOutput {
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("hook did not finish")
        .unwrap()
}

const ALL_EVENTS: [(HookEvent, &str); 5] = [
    (HookEvent::SessionStart, "session-start"),
    (HookEvent::Prompt, "prompt"),
    (HookEvent::PreTool, "pre-tool"),
    (HookEvent::PostTool, "post-tool"),
    (HookEvent::Stop, "stop"),
];

#[tokio::test]
async fn unregistered_silent() {
    let d = TestDaemon::start().await;
    let env = Env::of(&d.paths);
    let other = common::register(&mut d.client().await, "other", d.dir.path()).await;
    send(&d, &other, "hi", "x", true).await;
    ask(&d, &other, "which?").await;
    for (event, payload) in ALL_EVENTS {
        for wake in [false, true] {
            let out = run_wake(&env, event, payload, wake).await;
            assert!(silent(&out), "{event:?} wake {wake}: {out:?}");
        }
    }
}

#[tokio::test]
async fn daemon_down_is_silent_and_spawns_nothing() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    // In process, a spawn would not show here (it would start this test
    // binary, which makes no socket): `cli_hook_daemon_down_is_silent`
    // runs the real binary and is the test that catches one.
    let env = Env::of(&paths);
    for (event, payload) in ALL_EVENTS {
        let started = Instant::now();
        let out = run_wake(&env, event, payload, true).await;
        assert!(silent(&out), "{event:?}: {out:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{event:?}");
    }
    assert!(!paths.socket.exists(), "a daemon socket appeared");
    assert!(!paths.db.exists(), "a database appeared");
}

#[tokio::test]
async fn prompt_stdout_reminder() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);

    // Without mail: who the agent is, nothing more.
    for (event, payload) in [
        (HookEvent::SessionStart, "session-start"),
        (HookEvent::Prompt, "prompt"),
    ] {
        let out = run(&env, event, payload).await;
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        assert!(out.stdout.contains(&a), "{out:?}");
        assert!(!out.stdout.contains("unread"), "{out:?}");
    }

    send(&d, &a, "schema", "see the PR", false).await;
    for (event, payload) in [
        (HookEvent::SessionStart, "session-start"),
        (HookEvent::Prompt, "prompt"),
    ] {
        let out = run(&env, event, payload).await;
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        // Plain text, not Claude Code's JSON: kiro adds stdout as is.
        assert!(!out.stdout.starts_with('{'), "{out:?}");
        assert!(out.stdout.contains(&a), "{out:?}");
        assert!(out.stdout.contains("1 unread"), "{out:?}");
    }
}

#[tokio::test]
async fn pre_tool_blocks_once_for_urgent() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);

    // Plain mail never blocks a tool call, nor is it shown there.
    send(&d, &a, "note", "fyi", false).await;
    assert!(silent(&run(&env, HookEvent::PreTool, "pre-tool").await));

    send(&d, &a, "STOP", "the schema changed: do not migrate", true).await;
    let first = run(&env, HookEvent::PreTool, "pre-tool").await;
    assert_eq!(first.exit, 2, "{first:?}");
    assert!(first.stdout.is_empty(), "{first:?}");
    assert!(
        first.stderr.contains("the schema changed: do not migrate"),
        "{first:?}"
    );
    assert!(first.stderr.contains("urgent"), "{first:?}");
    // The model is told why its call did not run and that it may retry.
    assert!(first.stderr.contains("again"), "{first:?}");

    // Once: the next tool call goes ahead.
    let second = run(&env, HookEvent::PreTool, "pre-tool").await;
    assert!(silent(&second), "{second:?}");
}

#[tokio::test]
async fn subagent_pre_tool_acts_as_the_root() {
    // kiro gives a subagent's tool hooks the root's payload `session_id`
    // and `KIRO_SESSION_ID`, and nothing else marks them (recorded live,
    // `pre-tool-subagent*.json`): urgent mail is pushed at whichever tool
    // call comes first, a subagent's included, and only once. The root
    // agent still counts it unread in its next reminder.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "STOP", "the schema changed", true).await;
    let out = run(&env, HookEvent::PreTool, "pre-tool-subagent").await;
    assert_eq!(out.exit, 2, "{out:?}");
    assert!(silent(&run(&env, HookEvent::PreTool, "pre-tool").await));
    assert!(silent(
        &run(&env, HookEvent::PreTool, "pre-tool-subagent-response").await
    ));
    let out = run(&env, HookEvent::Prompt, "prompt").await;
    assert!(out.stdout.contains("1 unread"), "{out:?}");
}

#[tokio::test]
async fn post_tool_is_silent() {
    // kiro ignores PostToolUse output; nothing is installed for it, and
    // the hook says nothing even when run.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "STOP", "now", true).await;
    assert!(silent(&run(&env, HookEvent::PostTool, "post-tool").await));
    // The urgent mail is still pushed at the next tool call.
    assert_eq!(run(&env, HookEvent::PreTool, "pre-tool").await.exit, 2);
}

#[tokio::test]
async fn stop_exit1_on_open_question() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);

    // No question: kiro may stop.
    send(&d, &a, "note", "fyi", false).await;
    assert!(silent(&run(&env, HookEvent::Stop, "stop").await));

    for wake in [false, true] {
        let subject = format!("POST /orders body, wake {wake}?");
        let q = ask(&d, &a, &subject).await;
        let out = run_wake(&env, HookEvent::Stop, "stop", wake).await;
        assert_eq!(out.exit, 1, "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(out.stderr.contains(&q), "{out:?}");
        assert!(out.stderr.contains(&subject), "{out:?}");
    }
}

#[tokio::test]
async fn stop_shows_open_questions_at_every_stop_without_wake() {
    // Without waking no stop waits, so a question the model left open is
    // shown again at every stop (kiro continues once per human prompt).
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    for _ in 0..2 {
        let out = run(&env, HookEvent::Stop, "stop").await;
        assert_eq!(out.exit, 1, "{out:?}");
        assert!(out.stderr.contains(&q), "{out:?}");
    }

    // A new question is shown with the one still open.
    let q2 = ask(&d, &a, "GET /orders filters?").await;
    let out = run(&env, HookEvent::Stop, "stop").await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(
        out.stderr.contains(&q) && out.stderr.contains(&q2),
        "{out:?}"
    );

    // Stops without `--wake` record nothing: a `--wake` stop still shows
    // the questions once.
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stderr.contains(&q2), "{out:?}");
}

#[tokio::test]
async fn stop_with_wake_shows_an_open_question_once() {
    // A question shown at a `--wake` stop is not shown at the next one
    // (which waits instead); a new question is, with the one still open.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stderr.contains(&q), "{out:?}");
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await, "showed the question again");
    let q2 = ask(&d, &a, "GET /orders filters?").await;
    assert_eq!(finish(h).await.exit, 1);
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(
        out.stderr.contains(&q) && out.stderr.contains(&q2),
        "{out:?}"
    );
    let out = run(&env, HookEvent::Prompt, "prompt").await;
    assert!(out.stdout.contains("2 question"), "{out:?}");
}

#[tokio::test]
async fn stop_waits_past_an_open_question_already_shown() {
    // kiro runs Stop once per human prompt: a question left open must
    // not keep every later stop from waiting.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stderr.contains(&q), "{out:?}");

    // The question, still open and unread, does not wake the agent.
    let h = spawn_stop(&env);
    assert!(
        still_waiting(&h, 0.5).await,
        "did not wait past the question"
    );
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stderr.contains("2 unread"), "{out:?}");

    // A question that comes while it waits wakes it, and the next stop
    // shows it.
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await, "woke twice for the same mail");
    let q2 = ask(&d, &a, "GET /orders filters?").await;
    assert_eq!(finish(h).await.exit, 1);
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stderr.contains(&q2), "{out:?}");
}

#[tokio::test]
async fn stop_waits_then_exit1_on_mail() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await, "returned with no mail");
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 1, "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(out.stderr.contains(&a), "{out:?}");
    assert!(out.stderr.contains("1 unread"), "{out:?}");
}

#[tokio::test]
async fn stop_wakes_once_for_mail_already_there() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(spawn_stop(&env)).await;
    assert_eq!(out.exit, 1, "{out:?}");
    // Left unread after that wake: the next stop waits for new mail.
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await, "woke twice for the same mail");
    send(&d, &a, "more", "x", false).await;
    assert_eq!(finish(h).await.exit, 1);
}

#[tokio::test]
async fn stop_silent_without_wake() {
    // Installed without `--kiro-wake`: the Stop hook never waits.
    let d = TestDaemon::start().await;
    register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let started = Instant::now();
    let out = run(&env, HookEvent::Stop, "stop").await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));

    // Installed with it, but the agent registered without `wake`.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    let started = Instant::now();
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    // Even with mail there.
    send(&d, &a, "schema", "see the PR", false).await;
    let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn stop_shows_open_questions_at_every_stop_when_wake_is_off() {
    // Installed with `--kiro-wake`, but the agent registered without
    // `wake`: no stop waits, so a question left open is shown at every
    // stop, not at one only.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    for _ in 0..2 {
        let out = run_wake(&env, HookEvent::Stop, "stop", true).await;
        assert_eq!(out.exit, 1, "{out:?}");
        assert!(out.stderr.contains(&q), "{out:?}");
    }
}

#[tokio::test]
async fn stop_wait_ends_on_signoff() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await);
    d.client()
        .await
        .call(&agent(&a), Request::Signoff)
        .await
        .unwrap();
    assert!(silent(&finish(h).await));
}

#[tokio::test]
async fn stop_wait_ends_when_daemon_stops() {
    let mut d = TestDaemon::start().await;
    register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await);
    d.shutdown().await;
    assert!(silent(&finish(h).await));
}

#[tokio::test]
async fn stop_wait_ends_on_the_next_prompt() {
    // Esc while the Stop hook waits ends kiro's turn but leaves the hook
    // running (seen live). The human's next prompt ends it, so it never
    // wakes a session that is busy again, and only one waits.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await);
    let out = run(&env, HookEvent::Prompt, "prompt").await;
    assert!(out.stdout.contains(&a), "{out:?}");
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");

    // A tool call does the same.
    let h = spawn_stop(&env);
    assert!(still_waiting(&h, 0.5).await);
    assert!(silent(&run(&env, HookEvent::PreTool, "pre-tool").await));
    assert!(silent(&finish(h).await));
}

#[tokio::test]
async fn stop_wait_replaced_by_a_later_stop() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let first = spawn_stop(&env);
    assert!(still_waiting(&first, 0.5).await);
    let second = spawn_stop(&env);
    // The older wait lets go; only the newer one wakes the agent.
    assert!(silent(&finish(first).await));
    assert!(still_waiting(&second, 0.3).await);
    send(&d, &a, "schema", "see the PR", false).await;
    assert_eq!(finish(second).await.exit, 1);
}

/// The median time of 20 runs of `event` (after one warm-up run), each
/// checked by `check`.
async fn median_time(
    env: &Env,
    event: HookEvent,
    payload: &str,
    wake: bool,
    check: impl Fn(&HookOutput),
) -> Duration {
    run_wake(env, event, payload, wake).await;
    let mut times = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let out = run_wake(env, event, payload, wake).await;
        times.push(started.elapsed());
        check(&out);
    }
    times.sort();
    times[times.len() / 2]
}

#[tokio::test]
async fn hook_latency_under_20ms() {
    // Every hook path that does not wait, with the daemon up.
    let limit = Duration::from_millis(20);
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    send(&d, &a, "schema", "see the PR", false).await;
    let env = Env::of(&d.paths);
    for (event, payload) in [
        (HookEvent::SessionStart, "session-start"),
        (HookEvent::Prompt, "prompt"),
    ] {
        let median = median_time(&env, event, payload, false, |out| {
            assert_eq!(out.exit, 0, "{out:?}");
            assert!(out.stdout.contains("agentmail"), "{out:?}");
        })
        .await;
        assert!(median < limit, "{event:?}: median {median:?}");
    }
    let median = median_time(&env, HookEvent::PreTool, "pre-tool", false, |out| {
        assert!(silent(out), "{out:?}");
    })
    .await;
    assert!(median < limit, "PreTool: median {median:?}");
    // Stop without `--wake`, and with it for an agent whose `wake` is off.
    for wake in [false, true] {
        let median = median_time(&env, HookEvent::Stop, "stop", wake, |out| {
            assert!(silent(out), "{out:?}");
        })
        .await;
        assert!(median < limit, "Stop, wake {wake}: median {median:?}");
    }
}

// Through the binary.

/// `agentmail hook <args>` with `stdin`, in this daemon's environment.
fn hook_cmd(paths: &Paths, args: &[&str], stdin: &[u8]) -> std::process::Output {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agentmail"))
        .arg("hook")
        .args(args)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .env_remove("AGENTMAIL_BIN")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_exit_codes_and_streams() {
    // The spawned hook's parent is this test process, whose parent the
    // agent is bound to: in its ancestry, as in process.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    send(&d, &a, "STOP", "the schema changed", true).await;
    let paths = d.paths.clone();
    let out = tokio::task::spawn_blocking(move || {
        hook_cmd(
            &paths,
            &["pre-tool", "--harness", "kiro"],
            &fixture("pre-tool"),
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("the schema changed"));

    let q = ask(&d, &a, "which?").await;
    let paths = d.paths.clone();
    let out = tokio::task::spawn_blocking(move || {
        hook_cmd(
            &paths,
            &["stop", "--harness", "kiro", "--wake"],
            &fixture("stop"),
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains(&q));
}

#[test]
fn cli_hook_daemon_down_is_silent() {
    // Through the binary: a hook that spawned a daemon would run this
    // `agentmail`, whose daemon would make the socket and lock.
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    for args in [
        &["session-start", "--harness", "kiro"][..],
        &["prompt", "--harness", "kiro"][..],
        &["pre-tool", "--harness", "kiro"][..],
        &["post-tool", "--harness", "kiro"][..],
        &["stop", "--harness", "kiro"][..],
        &["stop", "--harness", "kiro", "--wake"][..],
    ] {
        let started = Instant::now();
        let out = hook_cmd(&paths, args, &fixture(args[0]));
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "{args:?}");
    }
    assert!(!paths.socket.exists(), "a daemon socket appeared");
    assert!(!paths.lock.exists(), "a daemon lock appeared");
    assert!(!paths.db.exists(), "a database appeared");
}

#[test]
fn wake_flag_is_kiro_stop_only() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    for args in [
        &["stop", "--harness", "claude-code", "--wake"][..],
        &["prompt", "--harness", "kiro", "--wake"][..],
    ] {
        let out = hook_cmd(&paths, args, b"{}");
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--wake"),
            "{out:?}"
        );
    }
}

/// `agentmail hook stop --harness kiro --wake` run by a fake `kiro-cli`
/// (a copy of `/bin/sh` by that name, which stays the hook's parent and
/// is its harness process) started with `extra` arguments. Returns the
/// running fake and its pid; the hook starts once `go` exists.
fn fake_kiro(dir: &Path, paths: &Paths, extra: &[&str]) -> (std::process::Child, u32) {
    fake_kiro_stops(dir, paths, extra, 1)
}

/// [`fake_kiro`] running the Stop hook `stops` times in a row: each run
/// before the last ends with a line `exit <code>` on stderr; the fake
/// exits with the last run's code.
fn fake_kiro_stops(
    dir: &Path,
    paths: &Paths,
    extra: &[&str],
    stops: usize,
) -> (std::process::Child, u32) {
    let fake = dir.join("kiro-cli");
    if !fake.exists() {
        std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    }
    static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid_file = dir.join(format!("kiro-{n}.pid"));
    let go = dir.join("go");
    let stop = "\"$0\" hook stop --harness kiro --wake < /dev/null";
    let earlier = format!("{stop}; echo \"exit $?\" >&2; ").repeat(stops - 1);
    let script = format!(
        "echo $$ > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; \
         {earlier}{stop}; exit $?",
        pid_file.display(),
        go.display()
    );
    let child = std::process::Command::new(&fake)
        .arg("-c")
        .arg(&script)
        .arg(env!("CARGO_BIN_EXE_agentmail"))
        .args(extra)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the fake kiro-cli did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    (child, pid)
}

async fn register_proc(d: &TestDaemon, name: &str, pid: u32) -> String {
    let mut c = d.client().await;
    let data = c
        .call(
            &Caller::Human,
            Request::Register {
                name: name.into(),
                role: None,
                working_on: None,
                harness: Harness::Kiro,
                cwd: d.dir.path().to_path_buf(),
                proc_id: procinfo::proc_id(pid),
                fresh: true,
                wake: true,
                session: None,
            },
        )
        .await
        .unwrap();
    data["address"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_never_waits_in_a_headless_run() {
    // `kiro-cli chat --no-interactive` ends the run only after the Stop
    // hook: waiting there would hold every headless run open until mail
    // came (seen live). `--output-format stream-json` implies
    // `--no-interactive`. Its interactive chat is waited in as usual.
    let d = TestDaemon::start().await;
    let dir = TempDir::new().unwrap();
    for (i, args) in [
        &["chat", "--v3", "--no-interactive"][..],
        &["chat", "--v3", "--output-format", "stream-json", "hi"],
        &["chat", "--v3", "--output-format=stream-json"],
    ]
    .into_iter()
    .enumerate()
    {
        let _ = std::fs::remove_file(dir.path().join("go"));
        let (headless, pid) = fake_kiro(dir.path(), &d.paths, args);
        register_proc(&d, &format!("headless{i}"), pid).await;
        std::fs::write(dir.path().join("go"), "").unwrap();
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || headless.wait_with_output().unwrap()),
        )
        .await
        .unwrap_or_else(|_| panic!("the headless stop waited: {args:?}"))
        .unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    }

    std::fs::remove_file(dir.path().join("go")).unwrap();
    let (mut chat, pid) = fake_kiro(dir.path(), &d.paths, &["chat", "--v3"]);
    let a = register_proc(&d, "chat", pid).await;
    std::fs::write(dir.path().join("go"), "").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        chat.try_wait().unwrap().is_none(),
        "the chat's stop did not wait"
    );
    send(&d, &a, "schema", "see the PR", false).await;
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || chat.wait_with_output().unwrap()),
    )
    .await
    .expect("the chat's stop did not wake")
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn headless_stops_show_open_questions_at_every_stop() {
    // A headless run never waits, so a question left open is shown at
    // every stop there too, not at one only.
    let d = TestDaemon::start().await;
    let dir = TempDir::new().unwrap();
    let (headless, pid) = fake_kiro_stops(
        dir.path(),
        &d.paths,
        &["chat", "--v3", "--no-interactive"],
        2,
    );
    let a = register_proc(&d, "headless", pid).await;
    let q = ask(&d, &a, "POST /orders body?").await;
    std::fs::write(dir.path().join("go"), "").unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || headless.wait_with_output().unwrap()),
    )
    .await
    .expect("a headless stop waited")
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (first, second) = stderr.split_once("exit 1\n").expect("first stop exit 1");
    assert!(first.contains(&q), "{stderr}");
    assert!(second.contains(&q), "{stderr}");
}
