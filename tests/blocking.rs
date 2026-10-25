//! Task 8: blocking ask, answers, waits and presence.

mod common;

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use agentmail::client::{Client, ClientError};
use agentmail::model::{HUMAN, Harness, ProcId};
use agentmail::procinfo;
use agentmail::protocol::{Caller, ErrorCode, Request};
use common::{TestDaemon, agent, register, send_req};
use serde_json::Value;

/// A live stand-in for a harness process, killed on drop.
struct FakeHarness {
    child: Child,
    id: ProcId,
}

impl FakeHarness {
    fn spawn() -> FakeHarness {
        let child = Command::new("sleep").arg("300").spawn().unwrap();
        let id = procinfo::proc_id(child.id()).expect("proc id of fake harness");
        FakeHarness { child, id }
    }
}

impl Drop for FakeHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The id of a process that has exited and been reaped.
fn dead_proc() -> ProcId {
    let mut h = FakeHarness::spawn();
    let id = h.id;
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    assert!(!procinfo::is_alive(&id));
    id
}

/// Register `name` in `cwd` bound to `proc_id`.
async fn register_bound(c: &mut Client, name: &str, cwd: &Path, proc_id: ProcId) -> String {
    let req = Request::Register {
        name: name.to_string(),
        role: None,
        working_on: Some(format!("{name} work")),
        harness: Harness::Other,
        cwd: cwd.to_path_buf(),
        proc_id: Some(proc_id),
        fresh: true,
        wake: true,
    };
    let data = c.call(&Caller::Human, req).await.unwrap();
    data["address"].as_str().unwrap().to_string()
}

/// An agent bound to a new live fake harness, returned with it.
async fn active(c: &mut Client, name: &str, cwd: &Path) -> (String, FakeHarness) {
    let h = FakeHarness::spawn();
    let addr = register_bound(c, name, cwd, h.id).await;
    (addr, h)
}

fn ask_req(to: &[&str], body: &str, timeout_secs: u64) -> Request {
    Request::Ask {
        to: to.iter().map(|s| s.to_string()).collect(),
        subject: "Schema".to_string(),
        body: body.to_string(),
        refs: Vec::new(),
        urgent: false,
        timeout_secs,
    }
}

fn reply_req(message_id: &str, body: &str, answer: bool) -> Request {
    Request::Reply {
        message_id: message_id.to_string(),
        body: body.to_string(),
        urgent: false,
        refs: Vec::new(),
        answer,
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("no {key:?} in {v}"))
}

/// The caller for `addr`, which may be the human.
fn as_caller(addr: &str) -> Caller {
    if addr == HUMAN {
        Caller::Human
    } else {
        agent(addr)
    }
}

/// Poll `addr`'s inbox until an open question shows up, and return its id.
async fn question_for(c: &mut Client, addr: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inbox = c
            .call(&as_caller(addr), Request::Inbox { all: true })
            .await
            .unwrap();
        if let Some(q) = inbox
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "question")
        {
            return str_of(q, "id").to_string();
        }
        assert!(Instant::now() < deadline, "no question reached {addr}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Run `req` as `caller` on a new connection in the background.
fn spawn_call(
    d: &TestDaemon,
    caller: Caller,
    req: Request,
) -> tokio::task::JoinHandle<Result<Value, ClientError>> {
    let paths = d.paths.clone();
    tokio::spawn(async move {
        let mut c = Client::connect(&paths).await.unwrap();
        c.call(&caller, req).await
    })
}

async fn finish(
    h: tokio::task::JoinHandle<Result<Value, ClientError>>,
    within: Duration,
) -> Result<Value, ClientError> {
    tokio::time::timeout(within, h)
        .await
        .expect("blocking call did not return in time")
        .unwrap()
}

/// Whether the call in `h` is still blocked after `wait`.
async fn still_blocked<T>(h: &tokio::task::JoinHandle<T>, wait: Duration) -> bool {
    tokio::time::sleep(wait).await;
    !h.is_finished()
}

#[tokio::test]
async fn ask_returns_answer() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let ask = spawn_call(&d, agent(&a), ask_req(&[&b], "which schema?", 5));
    let qid = question_for(&mut c, &b).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ask.is_finished(), "ask returned before the answer");
    let answer = c
        .call(&agent(&b), reply_req(&qid, "use v2", true))
        .await
        .unwrap();
    assert_eq!(answer["kind"], "answer");
    assert_eq!(answer["to"], serde_json::json!([a]));

    let out = finish(ask, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "answered", "{out}");
    assert_eq!(out["question_id"], qid.as_str());
    assert_eq!(out["answer"]["body"], "use v2");
    assert_eq!(out["answer"]["id"], answer["id"]);
    assert_eq!(out["answer"]["from"], b.as_str());
}

#[tokio::test]
async fn ask_returns_clarification_then_await_gets_answer() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let ask = spawn_call(&d, agent(&a), ask_req(&[&b], "which schema?", 5));
    let qid = question_for(&mut c, &b).await;
    let note = c
        .call(&agent(&b), reply_req(&qid, "for which endpoint?", false))
        .await
        .unwrap();
    assert_eq!(note["kind"], "note");
    assert_eq!(note["thread_id"], qid.as_str());

    let out = finish(ask, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "clarification", "{out}");
    assert_eq!(out["question_id"], qid.as_str());
    assert_eq!(out["message"]["id"], note["id"]);
    assert_eq!(out["message"]["body"], "for which endpoint?");

    // The question stays open.
    let counts = c.call(&agent(&b), Request::Counts).await.unwrap();
    assert_eq!(counts["open_questions"], 1);

    // A clarifies; the earlier clarification does not wake the await again.
    c.call(&agent(&a), reply_req(str_of(&note, "id"), "orders", false))
        .await
        .unwrap();
    let wait = spawn_call(
        &d,
        agent(&a),
        Request::AwaitAnswer {
            question_id: qid.clone(),
            timeout_secs: 5,
        },
    );
    assert!(still_blocked(&wait, Duration::from_millis(200)).await);
    let answer = c
        .call(&agent(&b), reply_req(&qid, "v2", true))
        .await
        .unwrap();
    let out = finish(wait, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "answered", "{out}");
    assert_eq!(out["answer"]["id"], answer["id"]);

    // Once answered, await returns at once.
    let started = Instant::now();
    let again = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: qid.clone(),
                timeout_secs: 30,
            },
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(again["status"], "answered");
    assert_eq!(again["answer"]["id"], answer["id"]);
}

#[tokio::test]
async fn answering_a_clarification_closes_the_question() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let ask = spawn_call(&d, agent(&a), ask_req(&[&b], "q", 5));
    let qid = question_for(&mut c, &b).await;
    let note = c
        .call(&agent(&b), reply_req(&qid, "which?", false))
        .await
        .unwrap();
    finish(ask, Duration::from_secs(5)).await.unwrap();
    let back = c
        .call(&agent(&a), reply_req(str_of(&note, "id"), "orders", false))
        .await
        .unwrap();
    let wait = spawn_call(
        &d,
        agent(&a),
        Request::AwaitAnswer {
            question_id: qid.clone(),
            timeout_secs: 5,
        },
    );
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);
    let answer = c
        .call(&agent(&b), reply_req(str_of(&back, "id"), "v2", true))
        .await
        .unwrap();
    let out = finish(wait, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "answered", "{out}");
    assert_eq!(out["answer"]["id"], answer["id"]);
}

#[tokio::test]
async fn human_answer_wakes_the_ask() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let ask = spawn_call(&d, agent(&a), ask_req(&[&b], "q", 5));
    let qid = question_for(&mut c, &b).await;
    let answer = c
        .call(&Caller::Human, reply_req(&qid, "do v3", true))
        .await
        .unwrap();
    assert_eq!(answer["from_human"], true);
    let out = finish(ask, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "answered", "{out}");
    assert_eq!(out["answer"]["from"], HUMAN);
}

#[tokio::test]
async fn ask_times_out_pending() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let started = Instant::now();
    let out = c.call(&agent(&a), ask_req(&[&b], "q", 1)).await.unwrap();
    let took = started.elapsed();
    assert_eq!(out["status"], "pending", "{out}");
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took <= Duration::from_secs(2), "{took:?}");
    let qid = str_of(&out, "question_id");
    let counts = c.call(&agent(&b), Request::Counts).await.unwrap();
    assert_eq!(counts["open_questions"], 1);
    assert_eq!(question_for(&mut c, &b).await, qid);

    // A note to the asker outside the thread does not wake an await.
    let wait = spawn_call(
        &d,
        agent(&a),
        Request::AwaitAnswer {
            question_id: qid.to_string(),
            timeout_secs: 1,
        },
    );
    c.call(&agent(&b), send_req(&[&a], "other", "unrelated"))
        .await
        .unwrap();
    let out = finish(wait, Duration::from_secs(3)).await.unwrap();
    assert_eq!(out["status"], "pending", "{out}");
}

#[tokio::test]
async fn ask_gone_recipient_no_one_listening() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register_bound(&mut c, "backend", d.dir.path(), dead_proc()).await;

    let started = Instant::now();
    let out = c.call(&agent(&a), ask_req(&[&b], "q", 30)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(out["status"], "no_one_listening", "{out}");
    let qid = str_of(&out, "question_id");
    assert_eq!(question_for(&mut c, &b).await, qid);
}

#[tokio::test]
async fn ask_held_name_no_one_listening() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;

    let out = c
        .call(&agent(&a), ask_req(&["backend"], "q", 30))
        .await
        .unwrap();
    assert_eq!(out["status"], "no_one_listening", "{out}");
    let qid = str_of(&out, "question_id").to_string();

    // The held question reaches backend when it registers, still open.
    let b = register(&mut c, "backend", d.dir.path()).await;
    assert_eq!(question_for(&mut c, &b).await, qid);
}

#[tokio::test]
async fn ask_with_one_live_recipient_waits() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let gone = register_bound(&mut c, "docs", d.dir.path(), dead_proc()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let out = c
        .call(&agent(&a), ask_req(&[&gone, &b], "q", 1))
        .await
        .unwrap();
    assert_eq!(out["status"], "pending", "{out}");
}

#[tokio::test]
async fn ask_to_human_waits_for_the_human() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;

    let ask = spawn_call(&d, agent(&a), ask_req(&[HUMAN], "q", 5));
    let qid = question_for(&mut c, HUMAN).await;
    c.call(&Caller::Human, reply_req(&qid, "yes", true))
        .await
        .unwrap();
    let out = finish(ask, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "answered", "{out}");
}

#[tokio::test]
async fn ask_deadlock_refused() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let first = c.call(&agent(&a), ask_req(&[&b], "q", 0)).await.unwrap();
    assert_eq!(first["status"], "pending", "{first}");
    let qid = str_of(&first, "question_id");

    let err = c
        .call(&agent(&b), ask_req(&[&a], "back at you", 5))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Deadlock, "{err:?}");
    assert!(err.message.contains(qid), "{}", err.message);
    assert!(err.message.contains(&a), "{}", err.message);
    assert!(err.message.contains("answer"), "{}", err.message);
    assert!(err.message.contains("reply"), "{}", err.message);
    // Nothing was stored for the refused question.
    let counts = c.call(&agent(&a), Request::Counts).await.unwrap();
    assert_eq!(counts["unread"], 0);

    // Once answered, the edge is gone and B may ask.
    c.call(&agent(&b), reply_req(qid, "done", true))
        .await
        .unwrap();
    let out = c.call(&agent(&b), ask_req(&[&a], "now?", 0)).await.unwrap();
    assert_eq!(out["status"], "pending", "{out}");
}

#[tokio::test]
async fn await_answer_errors() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let err = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                timeout_secs: 1,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");

    let note = c
        .call(&agent(&a), send_req(&[&b], "s", "a note"))
        .await
        .unwrap();
    let err = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: str_of(&note, "id").into(),
                timeout_secs: 1,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");

    let out = c.call(&agent(&a), ask_req(&[&b], "q", 0)).await.unwrap();
    let qid = str_of(&out, "question_id");
    // Only the asker waits on its question.
    let err = c
        .call(
            &agent(&b),
            Request::AwaitAnswer {
                question_id: qid.into(),
                timeout_secs: 1,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
    assert!(
        err.message.contains("only its asker waits for the answer"),
        "{err:?}"
    );
    assert!(!err.message.contains("  "), "{err:?}");
}

/// A clarification that came while no one waited (after the ask returned
/// `pending`) wakes the next await at once, and only that once.
#[tokio::test]
async fn clarification_before_await_returns_at_once() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;

    let out = c
        .call(&agent(&a), ask_req(&[&b], "which schema?", 0))
        .await
        .unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    let qid = str_of(&out, "question_id").to_string();
    let note = c
        .call(&agent(&b), reply_req(&qid, "for which endpoint?", false))
        .await
        .unwrap();

    let started = Instant::now();
    let out = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: qid.clone(),
                timeout_secs: 30,
            },
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(out["status"], "clarification", "{out}");
    assert_eq!(out["question_id"], qid.as_str());
    assert_eq!(out["message"]["id"], note["id"]);

    // Returned once, the clarification is read and does not fire again.
    let out = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: qid.clone(),
                timeout_secs: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "pending", "{out}");
}

#[tokio::test]
async fn reply_goes_to_the_other_participants() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let x = register(&mut c, "docs", d.dir.path()).await;

    let first = c
        .call(&agent(&a), send_req(&[&b, &x], "Orders", "hi"))
        .await
        .unwrap();
    let id = str_of(&first, "id");
    let r = c
        .call(&agent(&b), reply_req(id, "ack", false))
        .await
        .unwrap();
    assert_eq!(r["to"], serde_json::json!([a, x]));
    assert_eq!(r["subject"], "Re: Orders");
    assert_eq!(r["thread_id"], id);
    assert_eq!(r["in_reply_to"], id);
    assert_eq!(r["kind"], "note");
    // Replying to one's own message goes to its recipients.
    let r = c
        .call(&agent(&a), reply_req(id, "more", false))
        .await
        .unwrap();
    assert_eq!(r["to"], serde_json::json!([b, x]));
    // The human joins any thread and reaches everyone in the message.
    let r = c
        .call(&Caller::Human, reply_req(id, "note from human", false))
        .await
        .unwrap();
    assert_eq!(r["to"], serde_json::json!([a, b, x]));
    assert_eq!(r["from_human"], true);
    // Delivered: b has a's first message and the human's note unread.
    let inbox = c
        .call(&agent(&b), Request::Inbox { all: false })
        .await
        .unwrap();
    let bodies: Vec<&str> = inbox
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, vec!["hi", "more", "note from human"]);
}

#[tokio::test]
async fn reply_errors() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let x = register(&mut c, "docs", d.dir.path()).await;

    let err = c
        .call(
            &agent(&a),
            reply_req("01ARZ3NDEKTSV4RRFFQ69G5FAV", "x", false),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");

    let note = c
        .call(&agent(&a), send_req(&[&b], "s", "a note"))
        .await
        .unwrap();
    let id = str_of(&note, "id");
    // An answer must close an open question.
    let err = c
        .call(&agent(&b), reply_req(id, "x", true))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
    // An outsider cannot reply in the thread.
    let err = c
        .call(&agent(&x), reply_req(id, "x", false))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
    // Body limit applies.
    let err = c
        .call(&agent(&b), reply_req(id, &"x".repeat(262_145), false))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::TooLarge, "{err:?}");
    // The asker cannot answer its own question.
    let out = c.call(&agent(&a), ask_req(&[&b], "q", 0)).await.unwrap();
    let err = c
        .call(
            &agent(&a),
            reply_req(str_of(&out, "question_id"), "x", true),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput, "{err:?}");
}

#[tokio::test]
async fn wait_for_mail_wakes_on_delivery() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let x = register(&mut c, "docs", d.dir.path()).await;

    let wait = spawn_call(&d, agent(&b), Request::WaitForMail { timeout_secs: 5 });
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);
    // Mail for someone else does not wake it.
    c.call(&agent(&a), send_req(&[&x], "s", "not for b"))
        .await
        .unwrap();
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);
    let sent = c
        .call(&agent(&a), send_req(&[&b], "s", "for b"))
        .await
        .unwrap();
    let out = finish(wait, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "mail", "{out}");
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["id"], sent["id"]);
}

#[tokio::test]
async fn wait_for_mail_wakes_on_released_held_mail() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    // A gone `docs`: mail to the name is held, not delivered to it.
    let docs = register_bound(&mut c, "docs", d.dir.path(), dead_proc()).await;

    let wait = spawn_call(&d, agent(&docs), Request::WaitForMail { timeout_secs: 5 });
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);
    let sent = c
        .call(&agent(&a), send_req(&["docs"], "s", "held"))
        .await
        .unwrap();
    assert_eq!(sent["held_for"], serde_json::json!(["docs"]));
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);

    // docs comes back, reclaims its address and gets the held mail.
    let h = FakeHarness::spawn();
    let back = c
        .call(
            &Caller::Human,
            Request::Register {
                name: "docs".into(),
                role: None,
                working_on: None,
                harness: Harness::Other,
                cwd: d.dir.path().to_path_buf(),
                proc_id: Some(h.id),
                fresh: false,
                wake: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(back["address"], docs.as_str());
    let out = finish(wait, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "mail", "{out}");
    assert_eq!(out["messages"][0]["id"], sent["id"]);
}

#[tokio::test]
async fn wait_for_mail_returns_existing_unread_immediately() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let one = c
        .call(&agent(&a), send_req(&[&b], "s", "one"))
        .await
        .unwrap();
    let two = c
        .call(&agent(&a), send_req(&[&b], "s", "two"))
        .await
        .unwrap();
    c.call(
        &agent(&b),
        Request::Read {
            id: str_of(&one, "id").into(),
        },
    )
    .await
    .unwrap();

    let started = Instant::now();
    let out = c
        .call(&agent(&b), Request::WaitForMail { timeout_secs: 30 })
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(out["status"], "mail", "{out}");
    let ids: Vec<&Value> = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["id"])
        .collect();
    assert_eq!(ids, vec![&two["id"]]);
}

#[tokio::test]
async fn wait_for_mail_ignores_a_read_open_question() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let out = c.call(&agent(&a), ask_req(&[&b], "q", 0)).await.unwrap();
    c.call(
        &agent(&b),
        Request::Read {
            id: str_of(&out, "question_id").into(),
        },
    )
    .await
    .unwrap();

    let started = Instant::now();
    let out = c
        .call(&agent(&b), Request::WaitForMail { timeout_secs: 1 })
        .await
        .unwrap();
    assert_eq!(out["status"], "timeout", "{out}");
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() <= Duration::from_secs(2));
}

#[tokio::test]
async fn blocking_calls_need_an_identity() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let nobody = Caller::Procs {
        procs: vec![dead_proc()],
    };
    for req in [
        ask_req(&["backend"], "q", 1),
        Request::WaitForMail { timeout_secs: 1 },
        Request::AwaitAnswer {
            question_id: "x".into(),
            timeout_secs: 1,
        },
        reply_req("x", "b", false),
    ] {
        let err = c.call(&nobody, req).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NoIdentity, "{err:?}");
    }
}

#[tokio::test]
async fn connection_survives_a_blocking_call() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let out = c
        .call(&agent(&a), Request::WaitForMail { timeout_secs: 0 })
        .await
        .unwrap();
    assert_eq!(out["status"], "timeout");
    // The same connection carries the next request.
    c.call(&agent(&a), Request::Counts).await.unwrap();
}

#[tokio::test]
async fn huge_timeouts_are_accepted() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let b = register(&mut c, "backend", d.dir.path()).await;
    let wait = spawn_call(
        &d,
        agent(&b),
        Request::WaitForMail {
            timeout_secs: u64::MAX,
        },
    );
    assert!(still_blocked(&wait, Duration::from_millis(100)).await);
    c.call(&agent(&a), send_req(&[&b], "s", "x")).await.unwrap();
    let out = finish(wait, Duration::from_secs(5)).await.unwrap();
    assert_eq!(out["status"], "mail", "{out}");
}

#[tokio::test]
async fn presence_reports_dead_processes_as_gone() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let (a, mut ha) = active(&mut c, "frontend", d.dir.path()).await;
    let who = c.call(&agent(&a), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["presence"], "active");

    ha.child.kill().unwrap();
    ha.child.wait().unwrap();
    let who = c.call(&agent(&a), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["presence"], "gone");
    let listed = c
        .call(
            &Caller::Human,
            Request::Agents {
                name_prefix: None,
                repo: None,
                all: false,
            },
        )
        .await
        .unwrap();
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let listed = c
        .call(
            &Caller::Human,
            Request::Agents {
                name_prefix: None,
                repo: None,
                all: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed[0]["presence"], "gone");
}

#[tokio::test]
async fn daemon_death_during_ask_is_connection_lost() {
    let mut d = TestDaemon::start().await;
    let mut c = d.client().await;
    let a = register(&mut c, "frontend", d.dir.path()).await;
    let (b, _hb) = active(&mut c, "backend", d.dir.path()).await;
    drop(c);

    let ask = spawn_call(&d, agent(&a), ask_req(&[&b], "q", 30));
    // Wait until the question is stored, so the ask is blocked.
    let mut c = d.client().await;
    let qid = question_for(&mut c, &b).await;
    drop(c);
    assert!(!ask.is_finished());

    let killed = Instant::now();
    d.kill().await;
    let err = finish(ask, Duration::from_secs(1)).await.unwrap_err();
    assert!(killed.elapsed() < Duration::from_secs(1));
    assert_eq!(err.code, ErrorCode::ConnectionLost, "{err:?}");
    assert_eq!(err.message, "daemon connection lost");

    d.restart().await;
    let mut c = d.client().await;
    let out = c
        .call(
            &agent(&a),
            Request::AwaitAnswer {
                question_id: qid.clone(),
                timeout_secs: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "pending", "{out}");
    assert_eq!(out["question_id"], qid.as_str());
}
