//! Harness Task 1: agents bound to (harness, session id).

mod common;

use std::path::Path;
use std::process::{Child, Command};

use agentmail::client::Client;
use agentmail::model::{Harness, ProcId, SessionKey};
use agentmail::procinfo;
use agentmail::protocol::{Caller, Envelope, ErrorCode, Request, Response};
use common::{TestDaemon, send_req};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A live stand-in for the shared opencode service process, killed on drop.
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

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Opencode,
        id: id.to_string(),
    }
}

fn session(id: &str) -> Caller {
    Caller::Session { session: key(id) }
}

fn register_req(
    name: &str,
    cwd: &Path,
    proc_id: Option<ProcId>,
    session: &str,
    role: &str,
) -> Request {
    Request::Register {
        name: name.to_string(),
        role: Some(role.to_string()),
        working_on: None,
        harness: Harness::Opencode,
        cwd: cwd.to_path_buf(),
        proc_id,
        fresh: false,
        wake: true,
        session: Some(key(session)),
    }
}

async fn register(
    c: &mut Client,
    name: &str,
    cwd: &Path,
    proc_id: Option<ProcId>,
    session: &str,
    role: &str,
) -> Value {
    c.call(
        &Caller::Human,
        register_req(name, cwd, proc_id, session, role),
    )
    .await
    .unwrap()
}

fn address(data: &Value) -> String {
    data["address"].as_str().unwrap().to_string()
}

async fn whoami(c: &mut Client, caller: &Caller) -> String {
    let data = c.call(caller, Request::Whoami).await.unwrap();
    data["address"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn two_sessions_same_proc_get_distinct_addresses() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let b = address(&register(&mut c, "frontend", cwd, Some(h.id), "s2", "ui").await);
    assert_ne!(a, b);
    assert_eq!(whoami(&mut c, &session("s1")).await, a);
    assert_eq!(whoami(&mut c, &session("s2")).await, b);
    // Both stay active: sharing a process does not unbind the other.
    let who = c.call(&session("s1"), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["presence"], "active");
}

#[tokio::test]
async fn two_sessions_same_name_same_proc_do_not_share_an_address() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let b = address(&register(&mut c, "backend", cwd, Some(h.id), "s2", "api").await);
    assert_ne!(a, b);
    assert_eq!(whoami(&mut c, &session("s1")).await, a);
    assert_eq!(whoami(&mut c, &session("s2")).await, b);
}

#[tokio::test]
async fn register_same_session_is_idempotent() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let first = register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await;
    assert_eq!(first["existing"], false);
    let second = register(&mut c, "backend", cwd, Some(h.id), "s1", "orders").await;
    assert_eq!(address(&first), address(&second));
    assert_eq!(second["reclaimed"], false);
    assert_eq!(second["existing"], true);
    assert_eq!(second["agent"]["role"], "orders");
    let who = c.call(&session("s1"), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["role"], "orders");
}

#[tokio::test]
async fn same_session_different_name_rejected() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let err = c
        .call(
            &Caller::Human,
            register_req("frontend", cwd, Some(h.id), "s1", "ui"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert!(err.message.contains(&a), "{}", err.message);
}

#[tokio::test]
async fn invalid_session_id_rejected() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    for id in [String::new(), "x".repeat(129), "a\nb".to_string()] {
        let err = c
            .call(
                &Caller::Human,
                register_req("backend", d.dir.path(), None, &id, "api"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{id:?}");
    }
    // The session's harness must match the registration's.
    let mut mismatched = register_req("backend", d.dir.path(), None, "s1", "api");
    if let Request::Register { harness, .. } = &mut mismatched {
        *harness = Harness::Kiro;
    }
    let err = c.call(&Caller::Human, mismatched).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert!(err.message.contains("does not match"), "{}", err.message);
    let ok = register(
        &mut c,
        "backend",
        d.dir.path(),
        None,
        &"y".repeat(128),
        "api",
    )
    .await;
    assert!(ok["address"].is_string());
}

#[tokio::test]
async fn session_caller_unregistered_is_no_identity() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    // Even with an agent bound to this process, a session caller never
    // falls back to process ancestry.
    let me = procinfo::proc_id(std::process::id()).unwrap();
    c.call(
        &Caller::Human,
        Request::Register {
            name: "backend".into(),
            role: None,
            working_on: None,
            harness: Harness::Other,
            cwd: d.dir.path().to_path_buf(),
            proc_id: Some(me),
            fresh: true,
            wake: false,
            session: None,
        },
    )
    .await
    .unwrap();
    let err = c
        .call(&session("nobody"), Request::Whoami)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
}

#[tokio::test]
async fn signoff_frees_session() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await;
    c.call(&session("s1"), Request::Signoff).await.unwrap();
    let err = c.call(&session("s1"), Request::Whoami).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);

    // A different name is no longer refused: the binding is gone.
    let again = register(&mut c, "frontend", cwd, Some(h.id), "s1", "ui").await;
    assert_eq!(again["existing"], false);
    assert_eq!(again["agent"]["signed_off"], false);
    assert_eq!(again["agent"]["name"], "frontend");
    assert_eq!(whoami(&mut c, &session("s1")).await, address(&again));
}

#[tokio::test]
async fn signoff_then_same_name_reregisters_via_normal_rules() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let first = register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await;
    c.call(&session("s1"), Request::Signoff).await.unwrap();
    let again = register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await;
    assert_eq!(again["existing"], false);
    assert_eq!(again["agent"]["signed_off"], false);
    // The signed-off agent of the same session is reclaimed.
    assert_eq!(address(&again), address(&first));
    let who = c.call(&session("s1"), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["presence"], "active");
}

#[tokio::test]
async fn dead_session_agent_is_not_taken_by_another_session() {
    // s1's agent is bound to a dead process but s1 never signed off: it
    // still belongs to s1, so s2 gets a fresh address of its own.
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let cwd = d.dir.path();
    let dead = {
        let h = FakeHarness::spawn();
        h.id
    };
    let a = address(&register(&mut c, "backend", cwd, Some(dead), "s1", "api").await);
    let h = FakeHarness::spawn();
    let b = register(&mut c, "backend", cwd, Some(h.id), "s2", "api").await;
    assert_ne!(address(&b), a);
    assert_eq!(b["reclaimed"], false);
    assert_eq!(whoami(&mut c, &session("s2")).await, address(&b));
    assert_eq!(whoami(&mut c, &session("s1")).await, a);
}

#[tokio::test]
async fn opencode_restart_keeps_sessions_apart() {
    // opencode restarts (new service process); s2 registers before s1 does.
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let cwd = d.dir.path();
    let p1 = FakeHarness::spawn();
    let a = address(&register(&mut c, "backend", cwd, Some(p1.id), "s1", "api").await);
    c.call(&Caller::Human, send_req(&[&a], "for s1", "hi"))
        .await
        .unwrap();
    drop(p1);
    let p2 = FakeHarness::spawn();
    let b = register(&mut c, "backend", cwd, Some(p2.id), "s2", "api").await;
    assert_ne!(address(&b), a);
    assert_eq!(b["reclaimed"], false);
    assert_eq!(b["existing"], false);
    let again = register(&mut c, "backend", cwd, Some(p2.id), "s1", "api").await;
    assert_eq!(address(&again), a);
    assert_eq!(again["existing"], true);
    for (s, addr) in [("s1", a.clone()), ("s2", address(&b))] {
        let who = c.call(&session(s), Request::Whoami).await.unwrap();
        assert_eq!(who["address"], addr.as_str());
        assert_eq!(who["agent"]["presence"], "active", "{s}");
    }
    // s1's mail stayed with s1.
    let mine = c
        .call(&session("s1"), Request::Inbox { all: true })
        .await
        .unwrap();
    assert_eq!(mine.as_array().unwrap().len(), 1);
    let theirs = c
        .call(&session("s2"), Request::Inbox { all: true })
        .await
        .unwrap();
    assert!(theirs.as_array().unwrap().is_empty(), "{theirs}");
}

#[tokio::test]
async fn procs_caller_never_resolves_to_a_shared_session_agent() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let procs = Caller::Procs { procs: vec![h.id] };
    register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await;
    let err = c.call(&procs, Request::Whoami).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
    register(&mut c, "frontend", cwd, Some(h.id), "s2", "ui").await;
    let err = c.call(&procs, Request::Whoami).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
}

#[tokio::test]
async fn session_reregistration_without_proc_keeps_binding() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let again = register(&mut c, "backend", cwd, None, "s1", "orders").await;
    assert_eq!(address(&again), a);
    assert_eq!(again["existing"], true);
    let who = c.call(&session("s1"), Request::Whoami).await.unwrap();
    assert_eq!(who["agent"]["presence"], "active");
    assert_eq!(who["agent"]["role"], "orders");
}

#[tokio::test]
async fn session_mail_isolated() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    address(&register(&mut c, "frontend", cwd, Some(h.id), "s2", "ui").await);
    c.call(&Caller::Human, send_req(&[&a], "for backend", "hi"))
        .await
        .unwrap();
    let mine = c
        .call(&session("s1"), Request::Inbox { all: true })
        .await
        .unwrap();
    assert_eq!(mine.as_array().unwrap().len(), 1);
    let theirs = c
        .call(&session("s2"), Request::Inbox { all: true })
        .await
        .unwrap();
    assert!(theirs.as_array().unwrap().is_empty(), "{theirs}");
}

#[tokio::test]
async fn old_protocol_client_rejected() {
    let d = TestDaemon::start().await;
    let stream = UnixStream::connect(&d.paths.socket).await.unwrap();
    let (r, mut w) = stream.into_split();
    let line = serde_json::to_string(&Envelope {
        caller: Caller::Human,
        req: Request::Hello { version: 1 },
    })
    .unwrap();
    w.write_all(line.as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let mut buf = String::new();
    BufReader::new(r).read_line(&mut buf).await.unwrap();
    let resp: Response = serde_json::from_str(&buf).unwrap();
    match resp {
        Response::Err { code, .. } => assert_eq!(code, ErrorCode::VersionMismatch),
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

// Harness Task 2: hook status and the surfaced flag.

fn hook_status(surface_urgent: bool) -> Request {
    Request::HookStatus { surface_urgent }
}

fn urgent_req(to: &str, subject: &str) -> Request {
    Request::Send {
        to: vec![to.to_string()],
        subject: subject.to_string(),
        body: "now".to_string(),
        urgent: true,
        refs: Vec::new(),
    }
}

#[tokio::test]
async fn urgent_surfaced_once() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    c.call(&Caller::Human, urgent_req(&a, "fire"))
        .await
        .unwrap();
    // Not urgent: never surfaced as urgent.
    c.call(&Caller::Human, send_req(&[&a], "calm", "later"))
        .await
        .unwrap();

    // Peeking does not surface.
    let peek = c.call(&session("s1"), hook_status(false)).await.unwrap();
    assert_eq!(peek["urgent"].as_array().unwrap().len(), 1, "{peek}");

    let first = c.call(&session("s1"), hook_status(true)).await.unwrap();
    let urgent = first["urgent"].as_array().unwrap();
    assert_eq!(urgent.len(), 1, "{first}");
    assert_eq!(urgent[0]["subject"], "fire");
    assert_eq!(urgent[0]["urgent"], true);

    let second = c.call(&session("s1"), hook_status(true)).await.unwrap();
    assert!(second["urgent"].as_array().unwrap().is_empty(), "{second}");
    assert_eq!(second["unread"], 2);

    // Surfaced is not read.
    let inbox = c
        .call(&session("s1"), Request::Inbox { all: false })
        .await
        .unwrap();
    assert_eq!(inbox.as_array().unwrap().len(), 2, "{inbox}");

    // A wait still returns at once: surfaced-but-unread mail is unread.
    let waited = c
        .call(&session("s1"), Request::WaitForMail { timeout_secs: 5 })
        .await
        .unwrap();
    assert_eq!(waited["status"], "mail");
    assert_eq!(waited["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn read_urgent_mail_is_not_surfaced() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let msg = c
        .call(&Caller::Human, urgent_req(&a, "fire"))
        .await
        .unwrap();
    c.call(
        &session("s1"),
        Request::Read {
            id: msg["id"].as_str().unwrap().to_string(),
        },
    )
    .await
    .unwrap();
    let status = c.call(&session("s1"), hook_status(true)).await.unwrap();
    assert!(status["urgent"].as_array().unwrap().is_empty(), "{status}");
    assert_eq!(status["unread"], 0);
    assert_eq!(status["unread_ids"], serde_json::json!([]), "{status}");
}

#[tokio::test]
async fn hook_status_lists_open_questions_and_counts() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let h = FakeHarness::spawn();
    let cwd = d.dir.path();
    let a = address(&register(&mut c, "backend", cwd, Some(h.id), "s1", "api").await);
    let b = address(&register(&mut c, "frontend", cwd, Some(h.id), "s2", "ui").await);
    let ask = |to: &str, subject: &str| Request::Ask {
        to: vec![to.to_string()],
        subject: subject.to_string(),
        body: "?".to_string(),
        refs: Vec::new(),
        urgent: false,
        timeout_secs: 0,
    };
    let q1 = c.call(&Caller::Human, ask(&a, "first")).await.unwrap();
    let q2 = c.call(&session("s2"), ask(&a, "second")).await.unwrap();
    // A question a asks is not one a must answer.
    let docs = address(&register(&mut c, "docs", cwd, Some(h.id), "s3", "docs").await);
    c.call(&session("s1"), ask(&docs, "mine")).await.unwrap();
    c.call(&Caller::Human, send_req(&[&a], "note", "hi"))
        .await
        .unwrap();

    let status = c.call(&session("s1"), hook_status(false)).await.unwrap();
    assert_eq!(status["address"], a.as_str());
    assert_eq!(status["name"], "backend");
    assert_eq!(status["wake"], true);
    assert_eq!(status["unread"], 3, "{status}");
    let open = status["open_questions"].as_array().unwrap();
    assert_eq!(open.len(), 2, "{status}");
    assert_eq!(open[0]["id"], q1["question_id"]);
    assert_eq!(open[0]["from"], "human");
    assert_eq!(open[0]["subject"], "first");
    assert_eq!(open[1]["id"], q2["question_id"]);
    assert_eq!(open[1]["from"], b.as_str());
    assert_eq!(open[1]["subject"], "second");
    assert!(status["urgent"].as_array().unwrap().is_empty());

    // Answering closes it; reading does not.
    let q1_id = q1["question_id"].as_str().unwrap().to_string();
    c.call(&session("s1"), Request::Read { id: q1_id.clone() })
        .await
        .unwrap();
    let status = c.call(&session("s1"), hook_status(false)).await.unwrap();
    assert_eq!(status["open_questions"].as_array().unwrap().len(), 2);
    assert_eq!(status["unread"], 2);
    // The unread ids name exactly the unread mail, so a hook can tell new
    // mail from mail it already knew about.
    let ids: Vec<&str> = status["unread_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{status}");
    assert!(!ids.contains(&q1_id.as_str()), "{status}");
    c.call(
        &session("s1"),
        Request::Reply {
            message_id: q1_id,
            body: "yes".into(),
            urgent: false,
            refs: Vec::new(),
            answer: true,
        },
    )
    .await
    .unwrap();
    let status = c.call(&session("s1"), hook_status(false)).await.unwrap();
    let open = status["open_questions"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{status}");
    assert_eq!(open[0]["id"], q2["question_id"]);
}

#[tokio::test]
async fn hook_status_unregistered_is_no_identity() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let err = c
        .call(&session("nobody"), hook_status(true))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
    // A signed-off session is unregistered too.
    let h = FakeHarness::spawn();
    let reg = register(&mut c, "backend", d.dir.path(), Some(h.id), "s1", "api").await;
    let a = address(&reg);
    c.call(&session("s1"), Request::Signoff).await.unwrap();
    let err = c.call(&session("s1"), hook_status(true)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
    // Even when addressed directly (AGENTMAIL_ADDRESS), so the hook stays
    // silent and its wake loop ends.
    let err = c
        .call(&Caller::Address { address: a }, hook_status(true))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
    // The human has no hook status.
    let err = c
        .call(&Caller::Human, hook_status(false))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
}
