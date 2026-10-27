//! Harness Task 4: `agentmail hook` with Claude Code's events.
//!
//! The hooks resolve their agent by process ancestry, like a hook command
//! Claude Code runs. Each test registers its agent bound to this test
//! process's parent, which is in the ancestry of `hook::run` (called in
//! process) and of a spawned `agentmail hook`. Every test has its own
//! daemon, so the shared parent never names two agents.

mod common;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agentmail::hook::{self, HookEvent, HookOutput};
use agentmail::model::{Harness, ProcId};
use agentmail::paths::Paths;
use agentmail::procinfo;
use agentmail::protocol::{Caller, Request};
use common::{TestDaemon, agent, send_req};
use serde_json::Value;
use tempfile::TempDir;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hooks/claude-code")
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

async fn run(env: &Env, event: HookEvent, payload: &str) -> HookOutput {
    hook::run(Harness::ClaudeCode, event, &fixture(payload), &|k| {
        env.get(k)
    })
    .await
}

/// `name`'s Stop payload with a `prompt_id` of its own, as each stop has.
fn stop_payload(name: &str) -> Vec<u8> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut v: Value = serde_json::from_slice(&fixture(name)).unwrap();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    v["prompt_id"] = Value::from(format!("prompt-{}-{n}", std::process::id()));
    serde_json::to_vec(&v).unwrap()
}

/// Run a hook on its own task, so the test can act while it waits.
fn spawn_raw(env: &Env, event: HookEvent, stdin: Vec<u8>) -> tokio::task::JoinHandle<HookOutput> {
    let env = env.clone();
    tokio::spawn(
        async move { hook::run(Harness::ClaudeCode, event, &stdin, &|k| env.get(k)).await },
    )
}

/// What Claude Code does on a stop: start the wait hook and, beside it,
/// run the Stop hook, both with the stop's payload (`stop` or
/// `stop-active`). The wait goes by the Stop hook's decision.
async fn stop_and_wait(env: &Env, name: &str) -> tokio::task::JoinHandle<HookOutput> {
    stop_and_wait_payload(env, stop_payload(name)).await
}

/// [`stop_and_wait`] with the stop's whole payload.
async fn stop_and_wait_payload(env: &Env, payload: Vec<u8>) -> tokio::task::JoinHandle<HookOutput> {
    let wait = spawn_raw(env, HookEvent::Wait, payload.clone());
    spawn_raw(env, HookEvent::Stop, payload).await.unwrap();
    wait
}

fn parent() -> ProcId {
    procinfo::ancestors(std::process::id())[0]
}

/// Register `name` bound to this test process's parent (the "harness").
async fn register_here(d: &TestDaemon, name: &str, wake: bool) -> String {
    register_with(d, name, wake, true).await
}

/// [`register_here`], reclaiming the process's own agent unless `fresh`.
async fn register_with(d: &TestDaemon, name: &str, wake: bool, fresh: bool) -> String {
    let mut c = d.client().await;
    let data = c
        .call(
            &Caller::Human,
            Request::Register {
                name: name.into(),
                role: None,
                working_on: None,
                harness: Harness::ClaudeCode,
                cwd: d.dir.path().to_path_buf(),
                proc_id: Some(parent()),
                fresh,
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

fn context(out: &HookOutput, event_name: &str) -> String {
    assert_eq!(out.exit, 0, "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let v: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    let hso = &v["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], event_name, "{v}");
    assert_eq!(v.as_object().unwrap().len(), 1, "{v}");
    hso["additionalContext"].as_str().unwrap().to_string()
}

const ALL_EVENTS: [(HookEvent, &str); 5] = [
    (HookEvent::SessionStart, "session-start"),
    (HookEvent::Prompt, "prompt"),
    (HookEvent::PostTool, "post-tool"),
    (HookEvent::Stop, "stop"),
    (HookEvent::Wait, "stop"),
];

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

#[tokio::test]
async fn unregistered_is_silent() {
    let d = TestDaemon::start().await;
    let env = Env::of(&d.paths);
    // Mail for someone else changes nothing.
    let other = common::register(&mut d.client().await, "other", d.dir.path()).await;
    send(&d, &other, "hi", "x", true).await;
    for (event, payload) in ALL_EVENTS {
        let out = run(&env, event, payload).await;
        assert!(silent(&out), "{event:?}: {out:?}");
    }
}

#[tokio::test]
async fn human_is_silent() {
    let d = TestDaemon::start().await;
    register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths).with("AGENTMAIL_HUMAN", "1");
    for (event, payload) in ALL_EVENTS {
        let out = run(&env, event, payload).await;
        assert!(silent(&out), "{event:?}: {out:?}");
    }
}

#[tokio::test]
async fn daemon_down_is_silent_and_spawns_nothing() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let env = Env::of(&paths).with("AGENTMAIL_BIN", "/nonexistent/agentmail");
    for (event, payload) in ALL_EVENTS {
        let started = Instant::now();
        let out = run(&env, event, payload).await;
        assert!(silent(&out), "{event:?}: {out:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{event:?}");
    }
    assert!(!paths.socket.exists(), "a daemon socket appeared");
    assert!(!paths.lock.exists(), "a daemon lock appeared");
    assert!(!paths.db.exists(), "a database appeared");
}

#[tokio::test]
async fn bad_paths_are_silent() {
    // A relative override cannot be honoured: still no output.
    let env = Env(HashMap::new()).with("AGENTMAIL_DATA_DIR", "relative/dir");
    let out = run(&env, HookEvent::PostTool, "post-tool").await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn session_start_and_prompt_carry_guidance() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);

    let ctx = context(
        &run(&env, HookEvent::SessionStart, "session-start").await,
        "SessionStart",
    );
    assert!(ctx.contains(&a), "{ctx}");
    assert!(ctx.contains("inbox"), "{ctx}");
    assert!(!ctx.contains("unread"), "no mail yet: {ctx}");

    send(&d, &a, "schema", "see the PR", false).await;
    let ctx = context(
        &run(&env, HookEvent::Prompt, "prompt").await,
        "UserPromptSubmit",
    );
    assert!(ctx.contains(&a), "{ctx}");
    assert!(ctx.contains("1 unread"), "{ctx}");
}

#[tokio::test]
async fn post_tool_reminder_json_shape() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);

    // Nothing to say: no output at all.
    let out = run(&env, HookEvent::PostTool, "post-tool").await;
    assert!(silent(&out), "{out:?}");

    send(&d, &a, "schema", "see the PR", false).await;
    send(&d, &a, "deploy", "tomorrow", false).await;
    let q = ask(&d, &a, "POST /orders body?").await;
    let ctx = context(
        &run(&env, HookEvent::PostTool, "post-tool").await,
        "PostToolUse",
    );
    assert!(ctx.contains(&format!("you are {a}")), "{ctx}");
    assert!(ctx.contains("3 unread"), "{ctx}");
    assert!(
        ctx.contains("1 question from human: \"POST /orders body?\""),
        "{ctx}"
    );
    assert!(!ctx.contains(&q), "the reminder names, not lists: {ctx}");
}

#[tokio::test]
async fn post_tool_urgent_once() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "STOP", "the schema changed: do not migrate", true).await;

    let first = context(
        &run(&env, HookEvent::PostTool, "post-tool").await,
        "PostToolUse",
    );
    assert!(
        first.contains("the schema changed: do not migrate"),
        "{first}"
    );
    assert!(first.contains("urgent"), "{first}");

    // Surfaced once; afterwards only the plain reminder (it is still unread).
    let second = context(
        &run(&env, HookEvent::PostTool, "post-tool").await,
        "PostToolUse",
    );
    assert!(!second.contains("do not migrate"), "{second}");
    assert!(second.contains("1 unread"), "{second}");
}

#[tokio::test]
async fn subagent_post_tool_leaves_urgent_mail_to_the_main_agent() {
    // Claude Code runs PostToolUse for a subagent's tool calls too, with
    // `agent_id` set. Urgent mail is the main agent's to see: a subagent
    // must neither use up its one push nor be told who the agent is.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "STOP", "the schema changed: do not migrate", true).await;

    let out = run(&env, HookEvent::PostTool, "post-tool-subagent").await;
    assert!(silent(&out), "{out:?}");

    let main = context(
        &run(&env, HookEvent::PostTool, "post-tool").await,
        "PostToolUse",
    );
    assert!(main.contains("do not migrate"), "{main}");
}

#[tokio::test]
async fn subagent_hooks_say_nothing() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    ask(&d, &a, "POST /orders body?").await;
    let payload = |event: &str| {
        let mut v: Value = serde_json::from_slice(&fixture(event)).unwrap();
        v["agent_id"] = "aeb4c58b205863493".into();
        serde_json::to_vec(&v).unwrap()
    };
    for (event, name) in ALL_EVENTS {
        let stdin = payload(name);
        let out = hook::run(Harness::ClaudeCode, event, &stdin, &|k| env.get(k)).await;
        assert!(silent(&out), "{event:?}: {out:?}");
    }
}

#[tokio::test]
async fn background_subagent_does_not_end_the_idle_wait() {
    // A background subagent (`run_in_background`) keeps making tool calls
    // after the main agent's Stop, while the main session sits idle. Its
    // hooks must not count as the session getting busy: mail that comes
    // meanwhile still wakes the idle main agent. (A foreground subagent
    // runs inside a turn whose prompt already marked the session busy.)
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    for (event, payload) in [
        (HookEvent::PostTool, "post-tool-subagent"),
        (HookEvent::PreTool, "post-tool-subagent"),
    ] {
        run(&env, event, payload).await;
    }
    assert!(
        still_waiting(&h, 1.0).await,
        "a subagent tool call ended it"
    );
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn post_tool_failure_names_its_event() {
    // Failed tool calls run PostToolUseFailure instead of PostToolUse; the
    // output must name the event it answers.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "STOP", "the schema changed: do not migrate", true).await;
    let ctx = context(
        &run(&env, HookEvent::PostTool, "post-tool-failure").await,
        "PostToolUseFailure",
    );
    assert!(ctx.contains("do not migrate"), "{ctx}");
    let ctx = context(
        &run(&env, HookEvent::PostTool, "post-tool-failure").await,
        "PostToolUseFailure",
    );
    assert!(ctx.contains("1 unread"), "{ctx}");
}

#[tokio::test]
async fn address_variable_does_not_name_the_hook_agent() {
    // A hook resolves only by its harness process (or `--session`): an
    // `AGENTMAIL_ADDRESS` inherited from an outer agent's shell must not
    // make an inner Claude Code's hooks act as that agent.
    let d = TestDaemon::start().await;
    let outer = common::register(&mut d.client().await, "outer", d.dir.path()).await;
    send(&d, &outer, "STOP", "not yours", true).await;
    let env = Env::of(&d.paths).with("AGENTMAIL_ADDRESS", &outer);
    for (event, payload) in ALL_EVENTS {
        let out = run(&env, event, payload).await;
        assert!(silent(&out), "{event:?}: {out:?}");
    }
}

#[tokio::test]
async fn urgent_body_is_capped() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let body = "é".repeat(10_000);
    send(&d, &a, "big", &body, true).await;
    let ctx = context(
        &run(&env, HookEvent::PostTool, "post-tool").await,
        "PostToolUse",
    );
    assert!(ctx.contains("(truncated; read "), "{}", &ctx[..200]);
    assert!(ctx.len() < 4096 + 1024, "{}", ctx.len());
}

#[tokio::test]
async fn stop_blocks_on_open_question() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);

    // No question: Claude may stop.
    send(&d, &a, "note", "fyi", false).await;
    let out = run(&env, HookEvent::Stop, "stop").await;
    assert!(silent(&out), "{out:?}");

    let q = ask(&d, &a, "POST /orders body?").await;
    let out = run(&env, HookEvent::Stop, "stop").await;
    assert_eq!(out.exit, 0, "{out:?}");
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["decision"], "block", "{v}");
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.contains(&q), "{reason}");
    assert!(reason.contains("POST /orders body?"), "{reason}");
}

#[tokio::test]
async fn stop_respects_stop_hook_active() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    ask(&d, &a, "POST /orders body?").await;
    let out = run(&env, HookEvent::Stop, "stop-active").await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn wait_exits_2_on_mail() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await, "returned with no mail");
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(out.stderr.contains(&a), "{out:?}");
    assert!(out.stderr.contains("1 unread"), "{out:?}");
}

#[tokio::test]
async fn wait_wakes_at_once_for_unread_mail() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(stop_and_wait(&env, "stop").await).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn wait_after_a_wake_waits_for_new_mail() {
    // Woken (stop_hook_active) and still not reading its mail: waking it
    // again for the same mail would loop. Only new mail counts.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    let h = stop_and_wait(&env, "stop-active").await;
    assert!(still_waiting(&h, 0.5).await, "woke for old mail");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
    assert!(out.stderr.contains("2 unread"), "{out:?}");
}

#[tokio::test]
async fn wait_leaves_open_questions_to_the_stop_hook() {
    // The Stop hook blocks a plain stop for an open question, so the
    // session goes on with a continuation and is not idle. The wait that
    // stop started ends at once, claiming nothing: mail coming while the
    // continuation runs must not be injected into it as a wake.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    ask(&d, &a, "POST /orders body?").await;
    let started = Instant::now();
    let out = finish(stop_and_wait(&env, "stop").await).await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    // Only the Stop hook's record.
    assert_eq!(wait_files(&d.paths), vec![format!("{a}.stop")]);
    // Mail during the continuation wakes nothing.
    send(&d, &a, "schema", "see the PR", false).await;
    // The continuation's own stop starts the real wait, which wakes for
    // mail that comes after it.
    let h = stop_and_wait(&env, "stop-active").await;
    assert!(still_waiting(&h, 0.5).await, "woke for old mail");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn wait_goes_by_the_stop_hooks_decision() {
    // The Stop hook and the wait hook of one stop run side by side. Each
    // reading the agent's questions for itself, they could decide apart
    // when a question opens or closes between the two reads. The wait goes
    // by the Stop hook's record for the stop instead.

    // The Stop hook let the session stop; a question opens before the
    // wait looks. The session is idle with unread mail: the wait wakes it.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let payload = stop_payload("stop");
    let out = spawn_raw(&env, HookEvent::Stop, payload.clone())
        .await
        .unwrap();
    assert!(silent(&out), "{out:?}");
    ask(&d, &a, "POST /orders body?").await;
    let out = finish(spawn_raw(&env, HookEvent::Wait, payload)).await;
    assert_eq!(out.exit, 2, "{out:?}");

    // The Stop hook blocked the stop; the question is answered before the
    // wait looks. The session goes on with the continuation: the wait
    // ends at once, though mail is unread.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    let payload = stop_payload("stop");
    let out = spawn_raw(&env, HookEvent::Stop, payload.clone())
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["decision"], "block", "{v}");
    let reply = Request::Reply {
        message_id: q,
        body: "id and qty".into(),
        urgent: false,
        refs: Vec::new(),
        answer: true,
    };
    d.client().await.call(&agent(&a), reply).await.unwrap();
    send(&d, &a, "schema", "see the PR", false).await;
    let started = Instant::now();
    let out = finish(spawn_raw(&env, HookEvent::Wait, payload)).await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn wait_with_an_unusable_stop_key_decides_at_once() {
    // A `prompt_id` with whitespace cannot be recorded, so the Stop hook
    // writes no record for it. The wait must not look for one for the
    // whole of its limit: it decides alone, at once.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    ask(&d, &a, "POST /orders body?").await;
    let started = Instant::now();
    let payload = stop_payload_with("stop", "prompt with space");
    let out = finish(stop_and_wait_payload(&env, payload).await).await;
    assert!(silent(&out), "{out:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert!(
        wait_files(&d.paths).is_empty(),
        "{:?}",
        wait_files(&d.paths)
    );
}

/// `name`'s Stop payload with the given `prompt_id`.
fn stop_payload_with(name: &str, prompt_id: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(&fixture(name)).unwrap();
    v["prompt_id"] = Value::from(prompt_id);
    serde_json::to_vec(&v).unwrap()
}

/// Start the wait hook for `payload`, and run its Stop hook `lag` later,
/// so the wait looks for the stop's record before it is written.
async fn wait_then_stop(
    env: &Env,
    payload: Vec<u8>,
    lag: Duration,
) -> tokio::task::JoinHandle<HookOutput> {
    let wait = spawn_raw(env, HookEvent::Wait, payload.clone());
    tokio::time::sleep(lag).await;
    spawn_raw(env, HookEvent::Stop, payload).await.unwrap();
    wait
}

#[tokio::test]
async fn continuation_wait_ignores_the_blocked_stops_record() {
    // Claude Code keeps one `prompt_id` until the next human prompt, so a
    // Stop the Stop hook blocks and the continuation's Stop
    // (`stop_hook_active`) share it. The continuation's wait, started
    // before its Stop hook writes, must not take the blocked stop's record
    // for its own: dated before the continuation's busy marks, it would
    // end the wait at once and the idle agent would never be woken.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let q = ask(&d, &a, "POST /orders body?").await;
    let prompt_id = format!("prompt-shared-{}", std::process::id());
    let out =
        finish(stop_and_wait_payload(&env, stop_payload_with("stop", &prompt_id)).await).await;
    assert!(silent(&out), "{out:?}");
    // The continuation answers the question with a tool call.
    let reply = Request::Reply {
        message_id: q,
        body: "id and qty".into(),
        urgent: false,
        refs: Vec::new(),
        answer: true,
    };
    d.client().await.call(&agent(&a), reply).await.unwrap();
    run(&env, HookEvent::PostTool, "post-tool").await;
    let h = wait_then_stop(
        &env,
        stop_payload_with("stop-active", &prompt_id),
        Duration::from_millis(150),
    )
    .await;
    assert!(still_waiting(&h, 0.5).await, "the wait ended at once");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn wait_after_a_wake_ignores_the_earlier_stops_record() {
    // A wake's continuation stops with `stop_hook_active` and the same
    // `prompt_id` as the stop whose wait woke it, so both stops' records
    // carry one key. The later wait, started before its Stop hook writes,
    // must not go by the earlier record.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let prompt_id = format!("prompt-woken-{}", std::process::id());
    let h = stop_and_wait_payload(&env, stop_payload_with("stop-active", &prompt_id)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
    // The woken turn reads its mail with a tool call, then stops again.
    run(&env, HookEvent::PostTool, "post-tool").await;
    let h = wait_then_stop(
        &env,
        stop_payload_with("stop-active", &prompt_id),
        Duration::from_millis(150),
    )
    .await;
    assert!(still_waiting(&h, 0.5).await, "the wait ended at once");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn busy_before_the_wait_claims_is_not_lost() {
    // Claude Code submits a queued prompt right after the Stop; its prompt
    // hook can mark the session busy before the stop's wait has claimed
    // the turn. The wait must still see the session busy: waking now would
    // inject the wake into the new turn, and record the mail as woken for.
    for wait_first in [false, true] {
        let d = TestDaemon::start().await;
        let a = register_here(&d, "backend", true).await;
        let env = Env::of(&d.paths);
        send(&d, &a, "schema", "see the PR", false).await;
        let payload = stop_payload("stop");
        let wait = if wait_first {
            // The wait started before the Stop hook ran, as Claude Code
            // starts them side by side; the prompt comes as soon as the
            // Stop hook is done, whether or not the wait has claimed yet.
            let wait = spawn_raw(&env, HookEvent::Wait, payload.clone());
            spawn_raw(&env, HookEvent::Stop, payload).await.unwrap();
            run(&env, HookEvent::Prompt, "prompt").await;
            wait
        } else {
            spawn_raw(&env, HookEvent::Stop, payload.clone())
                .await
                .unwrap();
            run(&env, HookEvent::Prompt, "prompt").await;
            spawn_raw(&env, HookEvent::Wait, payload)
        };
        let out = finish(wait).await;
        assert!(silent(&out), "wait first {wait_first}: {out:?}");
        // The mail was not woken for: the busy turn's own stop wakes for it.
        let out = finish(stop_and_wait(&env, "stop").await).await;
        assert_eq!(out.exit, 2, "wait first {wait_first}: {out:?}");
    }
}

#[tokio::test]
async fn wait_ignores_mail_already_read() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let mut c = d.client().await;
    let sent = c
        .call(&Caller::Human, send_req(&[&a], "x", "y"))
        .await
        .unwrap();
    let id = sent["id"].as_str().unwrap().to_string();
    c.call(&agent(&a), Request::Read { id }).await.unwrap();
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await, "woke for read mail");
    h.abort();
}

#[tokio::test]
async fn wait_exits_0_on_signoff() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    d.client()
        .await
        .call(&agent(&a), Request::Signoff)
        .await
        .unwrap();
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn wait_exits_0_when_daemon_stops() {
    let mut d = TestDaemon::start().await;
    register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    d.shutdown().await;
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn wait_respects_no_wake() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    let started = Instant::now();
    let out = run(&env, HookEvent::Wait, "stop").await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));

    // `wake` turned off by registering again while a wait runs: the wait
    // ends without waking, with or without mail it already knew.
    for mail_first in [false, true] {
        let d = TestDaemon::start().await;
        let a = register_here(&d, "backend", true).await;
        let env = Env::of(&d.paths);
        if mail_first {
            // Not news for a wait after a wake: it keeps waiting.
            send(&d, &a, "old", "x", false).await;
        }
        let h = stop_and_wait(&env, "stop-active").await;
        assert!(still_waiting(&h, 0.5).await);
        assert_eq!(register_with(&d, "backend", false, false).await, a);
        send(&d, &a, "schema", "see the PR", false).await;
        let out = finish(h).await;
        assert!(silent(&out), "mail first {mail_first}: {out:?}");
    }
}

#[tokio::test]
async fn wait_skips_headless_claude() {
    // `claude -p` runs async hooks synchronously: waiting would hold the
    // run open until mail came.
    let d = TestDaemon::start().await;
    register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths).with("CLAUDE_CODE_ENTRYPOINT", "sdk-cli");
    let started = Instant::now();
    let out = run(&env, HookEvent::Wait, "stop").await;
    assert!(silent(&out), "{out:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn one_waiter_per_agent() {
    // Every Stop starts a wait hook; two waiting would wake the agent twice
    // for the same mail. The later stop's wait takes over: the earlier one
    // ends silently, and only the later one wakes.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths).with("CLAUDE_CODE_ENTRYPOINT", "cli");
    let first = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&first, 0.5).await);
    let second = stop_and_wait(&env, "stop").await;
    let out = finish(first).await;
    assert!(silent(&out), "{out:?}");
    assert!(still_waiting(&second, 0.5).await, "the later wait ended");
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(second).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn wait_wakes_for_new_mail_after_old_mail_is_read() {
    // The wait knew one unread message when it started. The agent read it
    // later; a new message must still wake it, though the unread count is
    // back where it started.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let mut c = d.client().await;
    let sent = c
        .call(&Caller::Human, send_req(&[&a], "x", "y"))
        .await
        .unwrap();
    let h = stop_and_wait(&env, "stop-active").await;
    assert!(still_waiting(&h, 0.5).await, "woke for old mail");
    let id = sent["id"].as_str().unwrap().to_string();
    c.call(&agent(&a), Request::Read { id }).await.unwrap();
    assert!(still_waiting(&h, 0.3).await, "woke for a read");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
    assert!(out.stderr.contains("1 unread"), "{out:?}");
}

#[tokio::test]
async fn wait_ends_when_the_session_is_busy_again() {
    // A wait wakes an idle session. Once a new turn starts (a prompt) or
    // the session runs a tool, the session is busy: mail arriving now must
    // not wake it (Claude Code would inject the wake into the busy turn),
    // and the turn's own Stop starts a fresh wait.
    for (event, payload) in [
        (HookEvent::Prompt, "prompt"),
        (HookEvent::PostTool, "post-tool"),
        (HookEvent::SessionStart, "session-start"),
    ] {
        let d = TestDaemon::start().await;
        let a = register_here(&d, "backend", true).await;
        let env = Env::of(&d.paths);
        let h = stop_and_wait(&env, "stop").await;
        assert!(still_waiting(&h, 0.5).await);
        run(&env, event, payload).await;
        let out = finish(h).await;
        assert!(silent(&out), "{event:?}: {out:?}");
        // The next stop waits again, and wakes for mail.
        let h = stop_and_wait(&env, "stop").await;
        assert!(still_waiting(&h, 0.5).await, "{event:?}");
        send(&d, &a, "schema", "see the PR", false).await;
        let out = finish(h).await;
        assert_eq!(out.exit, 2, "{event:?}: {out:?}");
    }
}

#[tokio::test]
async fn busy_session_mail_is_not_a_wake() {
    // Mail that comes during a turn is the turn's to see (the post-tool
    // reminder); it never fires the wait started by an earlier stop.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    run(&env, HookEvent::Prompt, "prompt").await;
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
}

/// The files under the daemon's `hook-wait/` directory.
fn wait_files(paths: &Paths) -> Vec<String> {
    match std::fs::read_dir(paths.data_dir.join("hook-wait")) {
        Ok(dir) => dir
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn hooks_without_a_wait_leave_no_files() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", false).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "schema", "see the PR", false).await;
    for (event, payload) in ALL_EVENTS {
        run(&env, event, payload).await;
    }
    assert_eq!(wait_files(&d.paths), Vec::<String>::new());
}

#[tokio::test]
async fn signoff_during_a_wait_leaves_no_files() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    assert!(!wait_files(&d.paths).is_empty(), "the wait wrote nothing");
    d.client()
        .await
        .call(&agent(&a), Request::Signoff)
        .await
        .unwrap();
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
    assert_eq!(wait_files(&d.paths), Vec::<String>::new());
}

#[tokio::test]
async fn signoff_after_a_wait_leaves_no_files() {
    // The wait ended (the session got busy) and left its files for the
    // next one; signing off removes them.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    run(&env, HookEvent::Prompt, "prompt").await;
    assert!(silent(&finish(h).await));
    assert!(!wait_files(&d.paths).is_empty());
    d.client()
        .await
        .call(&agent(&a), Request::Signoff)
        .await
        .unwrap();
    assert_eq!(wait_files(&d.paths), Vec::<String>::new());
}

#[tokio::test]
async fn gone_agents_are_cleared_once() {
    // Listings and lookups view every agent, gone ones included; the
    // daemon removes a gone agent's files when it first finds it gone, not
    // with a directory scan on every later view.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let mut c = d.client().await;
    c.call(&agent(&a), Request::Signoff).await.unwrap();
    let dir = d.paths.data_dir.join("hook-wait");
    std::fs::create_dir_all(&dir).unwrap();
    // Not something a hook writes for a signed-off agent: a marker that
    // shows whether a later view scanned the directory.
    std::fs::write(dir.join(format!("{a}.turn")), "x").unwrap();
    let list = Request::Agents {
        name_prefix: None,
        repo: None,
        all: true,
    };
    c.call(&Caller::Human, list).await.unwrap();
    assert_eq!(wait_files(&d.paths), vec![format!("{a}.turn")]);
}

#[tokio::test]
async fn hook_latency_under_20ms() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    send(&d, &a, "schema", "see the PR", false).await;
    let env = Env::of(&d.paths);
    // Warm up.
    run(&env, HookEvent::PostTool, "post-tool").await;
    let mut times = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let out = run(&env, HookEvent::PostTool, "post-tool").await;
        times.push(started.elapsed());
        assert!(!out.stdout.is_empty());
    }
    times.sort();
    let median = times[times.len() / 2];
    assert!(median < Duration::from_millis(20), "median {median:?}");
}

// Through the binary.

const BIN: &str = env!("CARGO_BIN_EXE_agentmail");

fn cli(paths: &Paths, args: &[&str], payload: &str) -> std::process::Output {
    assert_cmd::Command::new(BIN)
        .args(args)
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .env_remove("OPENCODE_SESSION_ID")
        .env_remove("KIRO_SESSION_ID")
        .env_remove("CLAUDE_PID")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .write_stdin(fixture(payload))
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap()
}

// The daemon runs in this process: the blocking CLI call needs another
// worker thread to serve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_hook_post_tool_and_wait() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    send(&d, &a, "schema", "see the PR", false).await;

    let out = cli(
        &d.paths,
        &["hook", "post-tool", "--harness", "claude-code"],
        "post-tool",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains(&a), "{ctx}");

    // Claude Code runs the Stop hook beside the wait; here before it.
    let out = cli(
        &d.paths,
        &["hook", "stop", "--harness", "claude-code"],
        "stop",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let out = cli(
        &d.paths,
        &["hook", "wait", "--harness", "claude-code"],
        "stop",
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 unread"));
}

#[test]
fn cli_hook_daemon_down_is_silent() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    for event in ["session-start", "prompt", "post-tool", "stop", "wait"] {
        let payload = match event {
            "wait" => "stop",
            e => e,
        };
        let out = cli(
            &paths,
            &["hook", event, "--harness", "claude-code"],
            payload,
        );
        assert_eq!(out.status.code(), Some(0), "{event}: {out:?}");
        assert!(out.stdout.is_empty(), "{event}: {out:?}");
        assert!(out.stderr.is_empty(), "{event}: {out:?}");
    }
    assert!(!paths.socket.exists());
    assert!(!paths.lock.exists());
}

#[test]
fn cli_hook_unknown_event_is_silent() {
    // A config newer than the binary must not block Claude Code (exit 2).
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    let out = cli(
        &paths,
        &["hook", "no-such-event", "--harness", "claude-code"],
        "stop",
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_exits_when_its_harness_exits() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let d = TestDaemon::start().await;
    let payload =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/claude-code/stop.json");
    // A stand-in harness: a shell that stays the hook's parent. It starts
    // the hook once its agent is registered (a line on its stdin).
    let mut harness = Command::new("sh")
        .arg("-c")
        .arg(r#"read go; "$0" hook stop --harness claude-code < "$1" > /dev/null; "$0" hook wait --harness claude-code < "$1"; exit $?"#)
        .arg(BIN)
        .arg(&payload)
        .env("AGENTMAIL_DATA_DIR", &d.paths.data_dir)
        .env("AGENTMAIL_SOCKET", &d.paths.socket)
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .env_remove("CLAUDE_PID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let harness_id = procinfo::proc_id(harness.id()).unwrap();
    let mut c = d.client().await;
    c.call(
        &Caller::Human,
        Request::Register {
            name: "backend".into(),
            role: None,
            working_on: None,
            harness: Harness::ClaudeCode,
            cwd: d.dir.path().to_path_buf(),
            proc_id: Some(harness_id),
            fresh: true,
            wake: true,
            session: None,
        },
    )
    .await
    .unwrap();
    harness.stdin.take().unwrap().write_all(b"go\n").unwrap();
    // The hook holds the stdout pipe open for as long as it runs.
    let mut stdout = harness.stdout.take().unwrap();
    let done = tokio::task::spawn_blocking(move || {
        let mut rest = Vec::new();
        let _ = stdout.read_to_end(&mut rest);
        Instant::now()
    });
    assert!(still_waiting(&done, 1.0).await, "the hook did not wait");
    harness.kill().unwrap();
    harness.wait().unwrap();
    let killed = Instant::now();
    let ended = tokio::time::timeout(Duration::from_secs(10), done)
        .await
        .expect("the hook outlived its harness")
        .unwrap();
    assert!(ended.duration_since(killed) < Duration::from_secs(5));
    // A wait whose harness is gone cleans up after itself.
    assert_eq!(wait_files(&d.paths), Vec::<String>::new());
}

#[tokio::test]
async fn wait_ends_when_its_process_is_registered_as_another_agent() {
    // The harness process was registered as `a`, then as `b`: the wait for
    // `a` must neither wake for `a`'s mail with `b`'s reminder nor for `b`'s
    // mail. It ends silently.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    let b = register_here(&d, "frontend", true).await;
    assert_ne!(a, b);
    send(&d, &a, "for a", "x", false).await;
    send(&d, &b, "for b", "y", false).await;
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
}

#[tokio::test]
async fn plain_stops_wake_once_for_mail_left_unread() {
    // Mail the agent was woken for and chose to leave unread does not wake
    // it again after every human turn; new mail still does.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    send(&d, &a, "fyi", "no action needed", false).await;
    let out = finish(stop_and_wait(&env, "stop").await).await;
    assert_eq!(out.exit, 2, "{out:?}");
    // A human turn, then a plain stop with the same mail still unread.
    run(&env, HookEvent::Prompt, "prompt").await;
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await, "woke again for the same mail");
    send(&d, &a, "deploy", "tomorrow", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
    assert!(out.stderr.contains("2 unread"), "{out:?}");
}

#[test]
fn cli_hook_unknown_event_after_a_global_flag_is_silent() {
    let dir = TempDir::new().unwrap();
    let paths = Paths::in_dir(dir.path());
    for args in [
        &[
            "--json",
            "hook",
            "no-such-event",
            "--harness",
            "claude-code",
        ][..],
        &["--as", "human", "hook", "no-such-event"][..],
        &["--as", "nobody", "hook", "stop", "--harness", "claude-code"][..],
    ] {
        let out = cli(&paths, args, "stop");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
    }
    // Not a hook: a usage error still fails.
    let out = cli(&paths, &["--json", "no-such-command"], "stop");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

// The daemon runs in this process: the blocking CLI call needs another
// worker thread to serve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_hook_as_human_is_silent() {
    // `--as human` is the human, as with AGENTMAIL_HUMAN=1: the hook says
    // nothing and uses up no urgent mail.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    send(&d, &a, "prod down", "roll back", true).await;
    for (event, payload) in [
        ("session-start", "session-start"),
        ("prompt", "prompt"),
        ("post-tool", "post-tool"),
        ("stop", "stop"),
        ("wait", "stop"),
    ] {
        let out = cli(
            &d.paths,
            &["--as", "human", "hook", event, "--harness", "claude-code"],
            payload,
        );
        assert_eq!(out.status.code(), Some(0), "{event}: {out:?}");
        assert!(out.stdout.is_empty(), "{event}: {out:?}");
        assert!(out.stderr.is_empty(), "{event}: {out:?}");
    }
    // The urgent mail is still unsurfaced: the agent's own hook gets it.
    let out = cli(
        &d.paths,
        &["hook", "post-tool", "--harness", "claude-code"],
        "post-tool",
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("roll back"), "{ctx}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_exit_between_waits_leaves_no_files() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let d = TestDaemon::start().await;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/claude-code");
    // A stand-in harness that starts a wait, then (on the next line) runs
    // a prompt hook, which ends the wait, and says so once the wait exited.
    // It is then closed mid-turn, with no wait running.
    let script = r#"
        read go
        "$0" hook stop --harness claude-code < "$1/stop.json" > /dev/null 2>&1
        "$0" hook wait --harness claude-code < "$1/stop.json" > /dev/null 2>&1 &
        read go
        "$0" hook prompt --harness claude-code < "$1/prompt.json" > /dev/null 2>&1
        wait
        echo done
        read go
    "#;
    let mut harness = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg(BIN)
        .arg(&fixtures)
        .env("AGENTMAIL_DATA_DIR", &d.paths.data_dir)
        .env("AGENTMAIL_SOCKET", &d.paths.socket)
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env_remove("AGENTMAIL_ADDRESS")
        .env_remove("AGENTMAIL_HUMAN")
        .env_remove("CLAUDE_PID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let harness_id = procinfo::proc_id(harness.id()).unwrap();
    let mut c = d.client().await;
    c.call(
        &Caller::Human,
        Request::Register {
            name: "backend".into(),
            role: None,
            working_on: None,
            harness: Harness::ClaudeCode,
            cwd: d.dir.path().to_path_buf(),
            proc_id: Some(harness_id),
            fresh: true,
            wake: true,
            session: None,
        },
    )
    .await
    .unwrap();
    let mut stdin = harness.stdin.take().unwrap();
    stdin.write_all(b"go\n").unwrap();
    let started = Instant::now();
    while !wait_files(&d.paths).iter().any(|f| f.ends_with(".turn")) {
        assert!(started.elapsed() < Duration::from_secs(10), "no wait ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stdin.write_all(b"go\n").unwrap();
    let stdout = harness.stdout.take().unwrap();
    let line = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        line
    });
    let line = tokio::time::timeout(Duration::from_secs(10), line)
        .await
        .expect("the wait did not end")
        .unwrap();
    assert_eq!(line.trim(), "done");
    assert!(!wait_files(&d.paths).is_empty(), "kept for the next wait");
    harness.kill().unwrap();
    harness.wait().unwrap();
    // The daemon finds the agent gone (here, listing agents) and removes
    // its files.
    c.call(
        &Caller::Human,
        Request::Agents {
            name_prefix: None,
            repo: None,
            all: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(wait_files(&d.paths), Vec::<String>::new());
}

#[tokio::test]
async fn wait_outlasts_a_slow_takeover() {
    // The wait it replaces may take a while to let go of the lock (a
    // status call, then a tick). While its claim on the turn stands, the
    // new wait keeps trying instead of giving up, so the idle agent is
    // still woken for mail.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let dir = d.paths.data_dir.join("hook-wait");
    std::fs::create_dir_all(&dir).unwrap();
    let stuck = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{a}.lock")))
        .unwrap();
    stuck.try_lock().unwrap();
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 5.5).await, "gave up on the lock");
    drop(stuck);
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(h).await;
    assert_eq!(out.exit, 2, "{out:?}");
}

#[tokio::test]
async fn wait_for_the_lock_ends_when_the_session_is_busy() {
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    let dir = d.paths.data_dir.join("hook-wait");
    std::fs::create_dir_all(&dir).unwrap();
    let stuck = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{a}.lock")))
        .unwrap();
    stuck.try_lock().unwrap();
    let h = stop_and_wait(&env, "stop").await;
    assert!(still_waiting(&h, 0.5).await);
    run(&env, HookEvent::Prompt, "prompt").await;
    let out = finish(h).await;
    assert!(silent(&out), "{out:?}");
    drop(stuck);
}

#[tokio::test]
async fn unusable_wait_lock_skips() {
    // Two waits must never both run: without a usable lock, none does.
    let d = TestDaemon::start().await;
    let a = register_here(&d, "backend", true).await;
    let env = Env::of(&d.paths);
    std::fs::create_dir_all(d.paths.data_dir.join(format!("hook-wait/{a}.lock"))).unwrap();
    send(&d, &a, "schema", "see the PR", false).await;
    let out = finish(stop_and_wait(&env, "stop").await).await;
    assert!(silent(&out), "{out:?}");
}
