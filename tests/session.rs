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
