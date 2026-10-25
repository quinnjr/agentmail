//! Task 7: caller identity and name/contact recipient resolution.

mod common;

use std::path::Path;
use std::process::{Child, Command};

use agentmail::client::Client;
use agentmail::model::{HUMAN, Harness, Presence, ProcId};
use agentmail::procinfo;
use agentmail::protocol::{Caller, ErrorCode, Request};
use common::{TestDaemon, agent, send_req};
use serde_json::Value;
use tempfile::TempDir;

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

/// A fresh git repository in a temporary directory.
fn git_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let ok = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(dir.path())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init failed");
    dir
}

fn register_as(name: &str, cwd: &Path, proc_id: Option<ProcId>, working_on: &str) -> Request {
    Request::Register {
        name: name.to_string(),
        role: None,
        working_on: Some(working_on.to_string()),
        harness: Harness::Other,
        cwd: cwd.to_path_buf(),
        proc_id,
        fresh: true,
        wake: true,
    }
}

/// Register `name` in `cwd`, bound to `proc_id` when given (so it is active).
async fn register_bound(
    c: &mut Client,
    name: &str,
    cwd: &Path,
    proc_id: Option<ProcId>,
    working_on: &str,
) -> String {
    let data = c
        .call(&Caller::Human, register_as(name, cwd, proc_id, working_on))
        .await
        .unwrap();
    data["address"].as_str().unwrap().to_string()
}

/// Register an active agent bound to a new fake harness, returned with it.
async fn active(c: &mut Client, name: &str, cwd: &Path, working_on: &str) -> (String, FakeHarness) {
    let h = FakeHarness::spawn();
    let addr = register_bound(c, name, cwd, Some(h.id), working_on).await;
    (addr, h)
}

async fn inbox(c: &mut Client, addr: &str) -> Vec<Value> {
    c.call(&agent(addr), Request::Inbox { all: true })
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

async fn send(
    c: &mut Client,
    from: &Caller,
    to: &[&str],
) -> Result<Value, agentmail::client::ClientError> {
    c.call(from, send_req(to, "hello", "body")).await
}

fn own_proc() -> ProcId {
    procinfo::proc_id(std::process::id()).unwrap()
}

#[tokio::test]
async fn procs_caller_resolves_to_registered_agent() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let me = own_proc();
    let addr = register_bound(&mut c, "backend", d.dir.path(), Some(me), "api").await;
    // The nearest ancestors come first; ours is somewhere in the chain.
    let mut chain = vec![ProcId {
        pid: me.pid,
        start_time: me.start_time + 1,
    }];
    chain.push(me);
    let data = c
        .call(&Caller::Procs { procs: chain }, Request::Whoami)
        .await
        .unwrap();
    assert_eq!(data["address"], addr.as_str());
}

#[tokio::test]
async fn unregistered_procs_is_no_identity() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    for procs in [vec![own_proc()], Vec::new()] {
        let err = c
            .call(&Caller::Procs { procs }, Request::Whoami)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NoIdentity);
        assert!(
            err.message.contains("agentmail register"),
            "{}",
            err.message
        );
    }
}

#[tokio::test]
async fn procs_caller_ignores_signed_off_agent() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let me = own_proc();
    let addr = register_bound(&mut c, "backend", d.dir.path(), Some(me), "api").await;
    c.call(&agent(&addr), Request::Signoff).await.unwrap();
    let err = c
        .call(&Caller::Procs { procs: vec![me] }, Request::Whoami)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoIdentity);
}

#[tokio::test]
async fn name_resolves_to_contact_first() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (f1, _h1) = active(&mut c, "frontend", repo.path(), "checkout page").await;
    let (f2, _h2) = active(&mut c, "frontend", repo.path(), "admin panel").await;
    c.call(
        &agent(&a),
        Request::ContactAdd {
            address: f2.clone(),
        },
    )
    .await
    .unwrap();
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([f2.clone()]));
    assert_eq!(inbox(&mut c, &f2).await.len(), 1);
    assert!(inbox(&mut c, &f1).await.is_empty());
}

#[tokio::test]
async fn contact_wins_even_when_gone_or_in_another_repo() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let other = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (_local, _hl) = active(&mut c, "frontend", repo.path(), "local").await;
    // A contact in another repo, not bound to any process (gone).
    let far = register_bound(&mut c, "frontend", other.path(), None, "far").await;
    c.call(
        &agent(&a),
        Request::ContactAdd {
            address: far.clone(),
        },
    )
    .await
    .unwrap();
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([far]));
}

#[tokio::test]
async fn several_contacts_with_the_name_are_ambiguous() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (f1, _h1) = active(&mut c, "frontend", repo.path(), "one").await;
    let (f2, _h2) = active(&mut c, "frontend", repo.path(), "two").await;
    for f in [&f1, &f2] {
        c.call(&agent(&a), Request::ContactAdd { address: f.clone() })
            .await
            .unwrap();
    }
    let err = send(&mut c, &agent(&a), &["frontend"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousRecipient);
    let mut got: Vec<&str> = err
        .candidates
        .iter()
        .map(|v| v.agent.address.as_str())
        .collect();
    got.sort();
    let mut want = vec![f1.as_str(), f2.as_str()];
    want.sort();
    assert_eq!(got, want);
    assert!(err.candidates.iter().all(|v| v.is_contact));
}

#[tokio::test]
async fn ambiguous_name_lists_candidates_with_summaries() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (f1, _h1) = active(&mut c, "frontend", repo.path(), "checkout page").await;
    let (f2, _h2) = active(&mut c, "frontend", repo.path(), "admin panel").await;
    let err = send(&mut c, &agent(&a), &["frontend"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousRecipient);
    assert_eq!(err.candidates.len(), 2);
    let mut got: Vec<(String, Option<String>)> = err
        .candidates
        .iter()
        .map(|v| (v.agent.address.clone(), v.agent.working_on.clone()))
        .collect();
    got.sort();
    let mut want = vec![
        (f1, Some("checkout page".to_string())),
        (f2, Some("admin panel".to_string())),
    ];
    want.sort();
    assert_eq!(got, want);
    assert!(
        err.candidates
            .iter()
            .all(|v| v.presence == Presence::Active && !v.is_contact)
    );
    // Nothing was sent.
    for f in &err.candidates {
        assert!(inbox(&mut c, &f.agent.address).await.is_empty());
    }
}

#[tokio::test]
async fn name_in_same_repo_only() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let other = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (here, _h1) = active(&mut c, "frontend", repo.path(), "here").await;
    let (there, _h2) = active(&mut c, "frontend", other.path(), "there").await;
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([here]));
    assert!(inbox(&mut c, &there).await.is_empty());
}

#[tokio::test]
async fn name_matches_from_a_subdirectory_of_the_repo() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let sub = repo.path().join("web");
    std::fs::create_dir(&sub).unwrap();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (f, _hf) = active(&mut c, "frontend", &sub, "web").await;
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([f]));
}

#[tokio::test]
async fn name_held_until_registration() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let other = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    // A frontend elsewhere does not count.
    let (_there, _ht) = active(&mut c, "frontend", other.path(), "there").await;
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([]));
    assert_eq!(msg["held_for"], serde_json::json!(["frontend"]));
    let (f, _hf) = active(&mut c, "frontend", repo.path(), "web").await;
    let got = inbox(&mut c, &f).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], msg["id"]);
}

#[tokio::test]
async fn gone_agent_with_the_name_does_not_take_the_mail() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let gone = register_bound(&mut c, "frontend", repo.path(), None, "old").await;
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([]));
    assert!(inbox(&mut c, &gone).await.is_empty());
    // The next `frontend` to register in the repo gets it.
    let (f, _hf) = active(&mut c, "frontend", repo.path(), "new").await;
    assert_eq!(inbox(&mut c, &f).await.len(), 1);
}

#[tokio::test]
async fn mixed_addresses_and_held_names_in_one_send() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (db, _hd) = active(&mut c, "db", repo.path(), "schema").await;
    let msg = send(&mut c, &agent(&a), &[&db, "frontend", "db", "frontend"])
        .await
        .unwrap();
    assert_eq!(msg["to"], serde_json::json!([db]));
    assert_eq!(msg["held_for"], serde_json::json!(["frontend"]));
}

#[tokio::test]
async fn unknown_full_address_rejected() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let err = send(&mut c, &agent(&a), &["frontend-zzzzzzzz"])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownRecipient);
    // Nothing was held for it either: a later `frontend` gets nothing.
    let (f, _hf) = active(&mut c, "frontend", repo.path(), "web").await;
    assert!(inbox(&mut c, &f).await.is_empty());
}

#[tokio::test]
async fn invalid_name_rejected() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    for bad in ["Frontend", "", "-x", "a b"] {
        let err = send(&mut c, &agent(&a), &[bad]).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownRecipient, "{bad:?}");
    }
    let err = send(&mut c, &agent(&a), &[]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
}

#[tokio::test]
async fn self_send_rejected() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let err = send(&mut c, &agent(&a), &[&a]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRecipient);
    // By name, when the sender is the only active `backend` in its repo.
    let err = send(&mut c, &agent(&a), &["backend"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRecipient);
    // The human cannot mail itself.
    let err = send(&mut c, &Caller::Human, &[HUMAN]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRecipient);
}

#[tokio::test]
async fn own_name_resolves_to_the_other_agent() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let (b, _hb) = active(&mut c, "backend", repo.path(), "worker").await;
    let msg = send(&mut c, &agent(&a), &["backend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([b]));
}

#[tokio::test]
async fn human_resolves_names_machine_wide() {
    let d = TestDaemon::start().await;
    let r1 = git_repo();
    let r2 = git_repo();
    let mut c = d.client().await;
    let (f, _hf) = active(&mut c, "frontend", r1.path(), "web").await;
    let msg = send(&mut c, &Caller::Human, &["frontend"]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([f]));
    let (_g, _hg) = active(&mut c, "frontend", r2.path(), "other").await;
    let err = send(&mut c, &Caller::Human, &["frontend"])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousRecipient);
    assert_eq!(err.candidates.len(), 2);
    // No active agent and no repo to hold the mail in.
    let err = send(&mut c, &Caller::Human, &["nobody"]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownRecipient);
}

#[tokio::test]
async fn agent_outside_a_repo_holds_mail_for_its_directory() {
    let d = TestDaemon::start().await;
    let dir = TempDir::new().unwrap();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", dir.path(), "api").await;
    let msg = send(&mut c, &agent(&a), &["frontend"]).await.unwrap();
    assert_eq!(msg["held_for"], serde_json::json!(["frontend"]));
    let (f, _hf) = active(&mut c, "frontend", dir.path(), "web").await;
    assert_eq!(inbox(&mut c, &f).await.len(), 1);
}

#[tokio::test]
async fn agent_can_mail_human_by_name() {
    let d = TestDaemon::start().await;
    let repo = git_repo();
    let mut c = d.client().await;
    let (a, _ha) = active(&mut c, "backend", repo.path(), "api").await;
    let msg = send(&mut c, &agent(&a), &[HUMAN]).await.unwrap();
    assert_eq!(msg["to"], serde_json::json!([HUMAN]));
    let got = c
        .call(&Caller::Human, Request::Inbox { all: true })
        .await
        .unwrap();
    assert_eq!(got.as_array().unwrap().len(), 1);
}
