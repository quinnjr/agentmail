//! Task 3: agents, reclaim and contacts in the redb store.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use agentmail::model::{Agent, Harness, Presence, ProcId, SessionKey};
use agentmail::store::{RegisterReq, Store, StoreError};
use tempfile::TempDir;

fn p(pid: u32) -> ProcId {
    ProcId {
        pid,
        start_time: 1000 + u64::from(pid),
    }
}

/// A fake process table: exactly the procs in the set are alive.
struct Procs(HashSet<ProcId>);

impl Procs {
    fn new(alive: &[ProcId]) -> Procs {
        Procs(alive.iter().copied().collect())
    }
    fn alive(&self) -> impl Fn(&ProcId) -> bool + '_ {
        |p| self.0.contains(p)
    }
    fn kill(&mut self, p: ProcId) {
        self.0.remove(&p);
    }
    fn spawn(&mut self, p: ProcId) {
        self.0.insert(p);
    }
}

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
    (dir, store)
}

fn req(name: &str, cwd: &str, proc_id: Option<ProcId>) -> RegisterReq {
    RegisterReq {
        name: name.into(),
        role: None,
        working_on: None,
        harness: Harness::ClaudeCode,
        cwd: PathBuf::from(cwd),
        repo: None,
        proc_id,
        fresh: false,
        wake: true,
        session: None,
    }
}

fn register(store: &Store, procs: &Procs, r: RegisterReq, now: i64) -> (Agent, bool) {
    store.register(r, now, &procs.alive()).unwrap()
}

#[test]
fn register_assigns_address() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let mut r = req("backend", "/w", Some(p(1)));
    r.role = Some("orders API".into());
    r.working_on = Some("Building orders".into());
    r.repo = Some(PathBuf::from("/w"));
    let (a, reclaimed) = register(&s, &procs, r, 10);
    assert!(!reclaimed);
    let suffix = a.address.strip_prefix("backend-").expect("prefix");
    assert_eq!(suffix.len(), 8, "{}", a.address);
    assert_eq!(a.name, "backend");
    assert_eq!(a.role.as_deref(), Some("orders API"));
    assert_eq!(a.working_on.as_deref(), Some("Building orders"));
    assert_eq!(a.harness, Harness::ClaudeCode);
    assert_eq!(a.cwd, PathBuf::from("/w"));
    assert_eq!(a.repo, Some(PathBuf::from("/w")));
    assert_eq!(a.proc_id, Some(p(1)));
    assert!(a.wake);
    assert_eq!(a.registered_at, 10);
    assert_eq!(a.last_seen, 10);
    assert!(!a.signed_off);
    assert_eq!(s.get_agent(&a.address).unwrap(), Some(a));
}

#[test]
fn register_rejects_bad_name() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    for name in ["Back End", "human", ""] {
        let err = s
            .register(req(name, "/w", None), 1, &procs.alive())
            .unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidName(_)),
            "{name:?}: {err:?}"
        );
    }
    assert!(s.list_agents().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn register_rejects_non_utf8_cwd() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));

    let mut r = req("backend", "/w", None);
    r.cwd = bad.clone();
    let err = s.register(r, 1, &procs.alive()).unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidInput(m) if m.contains("UTF-8") && m.contains("cwd")),
        "{err:?}"
    );

    let mut r = req("backend", "/w", None);
    r.repo = Some(bad);
    let err = s.register(r, 1, &procs.alive()).unwrap_err();
    assert!(
        matches!(&err, StoreError::InvalidInput(m) if m.contains("UTF-8") && m.contains("repo")),
        "{err:?}"
    );
    assert!(s.list_agents().unwrap().is_empty());
}

#[test]
fn register_rejects_long_working_on() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let mut r = req("backend", "/w", None);
    r.working_on = Some("x".repeat(281));
    let err = s.register(r, 1, &procs.alive()).unwrap_err();
    assert!(matches!(err, StoreError::TooLong(_)), "{err:?}");
}

#[test]
fn reclaim_same_name_same_cwd_when_gone() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    procs.kill(p(1));
    procs.spawn(p(2));
    let mut r = req("backend", "/w", Some(p(2)));
    r.harness = Harness::Kiro;
    r.wake = false;
    r.role = Some("new role".into());
    r.working_on = Some("new work".into());
    let (b, reclaimed) = register(&s, &procs, r, 20);
    assert!(reclaimed);
    assert_eq!(b.address, a.address);
    assert_eq!(b.proc_id, Some(p(2)));
    assert_eq!(b.harness, Harness::Kiro);
    assert!(!b.wake);
    assert_eq!(b.role.as_deref(), Some("new role"));
    assert_eq!(b.working_on.as_deref(), Some("new work"));
    assert_eq!(b.registered_at, 10);
    assert_eq!(b.last_seen, 20);
    assert_eq!(s.get_agent(&a.address).unwrap(), Some(b));
    assert_eq!(s.list_agents().unwrap().len(), 1);
}

#[test]
fn reclaim_after_signoff() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    s.signoff(&a.address).unwrap();
    procs.kill(p(1));
    procs.spawn(p(2));
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(2))), 20);
    assert!(reclaimed);
    assert_eq!(b.address, a.address);
    assert!(!b.signed_off);
}

#[test]
fn no_reclaim_when_active() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1), p(2)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(2))), 20);
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
    assert_eq!(
        s.get_agent(&a.address).unwrap().unwrap().proc_id,
        Some(p(1))
    );
}

#[test]
fn no_reclaim_when_signed_off_but_process_alive() {
    // Gone because it signed off, but still bound to a live process: not taken.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1), p(2)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    s.signoff(&a.address).unwrap();
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(2))), 20);
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
}

#[test]
fn no_reclaim_with_fresh() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    procs.kill(p(1));
    procs.spawn(p(2));
    let mut r = req("backend", "/w", Some(p(2)));
    r.fresh = true;
    let (b, reclaimed) = register(&s, &procs, r, 20);
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
}

#[test]
fn no_reclaim_different_cwd() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    procs.kill(p(1));
    procs.spawn(p(2));
    let (b, reclaimed) = register(&s, &procs, req("backend", "/other", Some(p(2))), 20);
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
}

#[test]
fn no_reclaim_different_name() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    procs.kill(p(1));
    procs.spawn(p(2));
    let (b, reclaimed) = register(&s, &procs, req("frontend", "/w", Some(p(2))), 20);
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
}

#[test]
fn reclaim_picks_most_recent() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1), p(2)]);
    let (a1, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (a2, _) = register(&s, &procs, req("backend", "/w", Some(p(2))), 11);
    assert_ne!(a1.address, a2.address);
    // a1 was seen more recently than a2.
    s.touch(&a1.address, 50).unwrap();
    s.touch(&a2.address, 40).unwrap();
    procs.kill(p(1));
    procs.kill(p(2));
    procs.spawn(p(3));
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(3))), 60);
    assert!(reclaimed);
    assert_eq!(b.address, a1.address);
}

#[test]
fn reregister_from_same_process_keeps_address() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(1))), 20);
    // Keeping the caller's own address is not a reclaim: nothing was taken back.
    assert!(!reclaimed);
    assert_eq!(b.address, a.address);
    assert_eq!(b.last_seen, 20);
    assert_eq!(s.list_agents().unwrap().len(), 1);
}

#[test]
fn reregister_after_signoff_from_same_process_is_not_a_reclaim() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    s.signoff(&a.address).unwrap();
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(1))), 20);
    assert!(!reclaimed);
    assert_eq!(b.address, a.address);
    assert!(!b.signed_off);
}

#[test]
fn reregister_prefers_own_binding_over_newer_gone_agent() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(0), p(1)]);
    let (y, _) = register(&s, &procs, req("backend", "/w", Some(p(0))), 5);
    let mut fresh = req("backend", "/w", Some(p(1)));
    fresh.fresh = true;
    let (x, _) = register(&s, &procs, fresh, 10);
    assert_ne!(x.address, y.address);
    s.touch(&y.address, 50).unwrap();
    procs.kill(p(0));

    let mut again = req("backend", "/w", Some(p(1)));
    again.role = Some("orders API".into());
    let (b, reclaimed) = register(&s, &procs, again, 60);
    assert_eq!(b.address, x.address);
    assert!(!reclaimed);
    assert_eq!(b.role.as_deref(), Some("orders API"));
    // Y is untouched and still unbound from p1's session.
    let y_now = s.get_agent(&y.address).unwrap().unwrap();
    assert_eq!(y_now.proc_id, Some(p(0)));
    assert_eq!(y_now.last_seen, 50);
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        x.address
    );
}

#[test]
fn one_address_per_process() {
    // Registering a second name from the same process moves the binding.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (b, _) = register(&s, &procs, req("frontend", "/w", Some(p(1))), 20);
    assert_ne!(a.address, b.address);
    let old = s.get_agent(&a.address).unwrap().unwrap();
    assert_eq!(old.proc_id, None);
    assert_eq!(Store::presence(&old, &procs.alive()), Presence::Gone);
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        b.address
    );
}

#[test]
fn presence_rules() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    assert_eq!(Store::presence(&a, &procs.alive()), Presence::Active);
    let (unbound, _) = register(&s, &procs, req("web", "/w", None), 10);
    assert_eq!(Store::presence(&unbound, &procs.alive()), Presence::Gone);
    s.signoff(&a.address).unwrap();
    let signed = s.get_agent(&a.address).unwrap().unwrap();
    assert!(signed.signed_off);
    assert_eq!(Store::presence(&signed, &procs.alive()), Presence::Gone);
    procs.kill(p(1));
    assert_eq!(Store::presence(&a, &procs.alive()), Presence::Gone);
}

#[test]
fn contacts_survive_reclaim() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1), p(9)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (f, _) = register(&s, &procs, req("frontend", "/web", Some(p(9))), 10);
    s.add_contact(&a.address, &f.address).unwrap();
    procs.kill(p(1));
    procs.spawn(p(2));
    let (b, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(2))), 20);
    assert!(reclaimed);
    assert_eq!(s.contacts(&b.address).unwrap(), vec![f.address.clone()]);
}

#[test]
fn add_contact_requires_full_existing_address() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    register(&s, &procs, req("frontend", "/w", None), 10);
    for bad in ["frontend", "frontend-zzzzzzzz"] {
        let err = s.add_contact(&a.address, bad).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{bad}: {err:?}");
    }
    assert!(s.contacts(&a.address).unwrap().is_empty());
}

#[test]
fn add_contact_rejects_unknown_owner_and_self() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    let err = s.add_contact("ghost-zzzzzzzz", &a.address).unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    let err = s.add_contact(&a.address, &a.address).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
}

#[test]
fn contacts_add_list_remove() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    let (f, _) = register(&s, &procs, req("frontend", "/w", None), 10);
    let (g, _) = register(&s, &procs, req("docs", "/w", None), 10);
    s.add_contact(&a.address, &g.address).unwrap();
    s.add_contact(&a.address, &f.address).unwrap();
    // Adding twice is a no-op.
    s.add_contact(&a.address, &f.address).unwrap();
    let mut expected = vec![f.address.clone(), g.address.clone()];
    expected.sort();
    assert_eq!(s.contacts(&a.address).unwrap(), expected);
    // Contacts are one-way.
    assert!(s.contacts(&f.address).unwrap().is_empty());

    s.remove_contact(&a.address, &f.address).unwrap();
    assert_eq!(s.contacts(&a.address).unwrap(), vec![g.address.clone()]);
    let err = s.remove_contact(&a.address, &f.address).unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
}

#[test]
fn human_can_keep_contacts() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    s.add_contact("human", &a.address).unwrap();
    assert_eq!(s.contacts("human").unwrap(), vec![a.address]);
}

#[test]
fn status_limit() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    // 280 characters, multi-byte: the limit counts characters, not bytes.
    let ok = "é".repeat(280);
    s.set_status(&a.address, &ok).unwrap();
    assert_eq!(
        s.get_agent(&a.address)
            .unwrap()
            .unwrap()
            .working_on
            .as_deref(),
        Some(ok.as_str())
    );
    let err = s.set_status(&a.address, &"x".repeat(281)).unwrap_err();
    assert!(matches!(err, StoreError::TooLong(_)), "{err:?}");
    assert_eq!(
        s.get_agent(&a.address)
            .unwrap()
            .unwrap()
            .working_on
            .as_deref(),
        Some(ok.as_str())
    );
}

#[test]
fn unknown_address_is_not_found() {
    let (_d, s) = store();
    let ghost = "ghost-zzzzzzzz";
    assert_eq!(s.get_agent(ghost).unwrap(), None);
    for err in [
        s.touch(ghost, 1).unwrap_err(),
        s.set_status(ghost, "x").unwrap_err(),
        s.signoff(ghost).unwrap_err(),
    ] {
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }
}

#[test]
fn touch_updates_last_seen() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    s.touch(&a.address, 99).unwrap();
    assert_eq!(s.get_agent(&a.address).unwrap().unwrap().last_seen, 99);
}

#[test]
fn find_by_procs_skips_signed_off() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1), p(2)]);
    let (a, _) = register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let (b, _) = register(&s, &procs, req("frontend", "/w", Some(p(2))), 10);
    // Ancestry order: p1 is nearer than p2.
    assert_eq!(
        s.find_by_procs(&[p(1), p(2)]).unwrap().unwrap().address,
        a.address
    );
    s.signoff(&a.address).unwrap();
    assert_eq!(
        s.find_by_procs(&[p(1), p(2)]).unwrap().unwrap().address,
        b.address
    );
    s.signoff(&b.address).unwrap();
    assert_eq!(s.find_by_procs(&[p(1), p(2)]).unwrap(), None);
    assert_eq!(s.find_by_procs(&[]).unwrap(), None);
}

#[test]
fn find_by_procs_requires_matching_start_time() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    register(&s, &procs, req("backend", "/w", Some(p(1))), 10);
    let reused = ProcId {
        pid: 1,
        start_time: 5,
    };
    assert_eq!(s.find_by_procs(&[reused]).unwrap(), None);
}

#[test]
fn list_agents_returns_all() {
    let (_d, s) = store();
    let procs = Procs::new(&[]);
    let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
    let (b, _) = register(&s, &procs, req("frontend", "/w", None), 10);
    let mut got: Vec<String> = s
        .list_agents()
        .unwrap()
        .into_iter()
        .map(|a| a.address)
        .collect();
    got.sort();
    let mut want = vec![a.address, b.address];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn reopen_persists() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("agentmail.redb");
    let procs = Procs::new(&[]);
    let (a, f) = {
        let s = Store::open(&path).unwrap();
        let (a, _) = register(&s, &procs, req("backend", "/w", None), 10);
        let (f, _) = register(&s, &procs, req("frontend", "/w", None), 10);
        s.add_contact(&a.address, &f.address).unwrap();
        (a, f)
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(s.get_agent(&a.address).unwrap(), Some(a.clone()));
    assert_eq!(s.contacts(&a.address).unwrap(), vec![f.address]);
}

#[test]
fn open_creates_missing_parent_dir() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested").join("agentmail.redb");
    Store::open(&path).unwrap();
    assert!(Path::new(&path).exists());
}

fn session_req(name: &str, cwd: &str, proc_id: Option<ProcId>, id: &str) -> RegisterReq {
    let mut r = req(name, cwd, proc_id);
    r.harness = Harness::Opencode;
    r.session = Some(SessionKey {
        harness: Harness::Opencode,
        id: id.into(),
    });
    r
}

#[test]
fn sessions_sharing_a_proc_keep_their_bindings() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(1)), "s1"),
        1,
    );
    let (b, _) = register(
        &s,
        &procs,
        session_req("frontend", "/w", Some(p(1)), "s2"),
        2,
    );
    // Process ancestry never resolves to a session agent of the shared
    // process: every session there has the same ancestry.
    assert_eq!(s.find_by_procs(&[p(1)]).unwrap(), None);
    // A process-only registration from the same process does not unbind
    // the session agents either.
    let (c, _) = register(&s, &procs, req("docs", "/w", Some(p(1))), 3);
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        c.address
    );
    for addr in [&a.address, &b.address, &c.address] {
        let agent = s.get_agent(addr).unwrap().unwrap();
        assert_eq!(agent.proc_id, Some(p(1)), "{addr}");
    }
    let s1 = session_req("x", "/", None, "s1").session.unwrap();
    assert_eq!(s.find_by_session(&s1).unwrap().unwrap().address, a.address);
}

#[test]
fn session_binding_is_per_harness() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(1)), "s1"),
        1,
    );
    let kiro = SessionKey {
        harness: Harness::Kiro,
        id: "s1".into(),
    };
    assert_eq!(s.find_by_session(&kiro).unwrap(), None);
    let mut r = req("frontend", "/w", Some(p(1)));
    r.session = Some(kiro.clone());
    let (b, _) = register(&s, &procs, r, 2);
    assert_ne!(a.address, b.address);
    assert_eq!(
        s.find_by_session(&kiro).unwrap().unwrap().address,
        b.address
    );
}

#[test]
fn signoff_unbinds_session_and_reopen_keeps_bindings() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("agentmail.redb");
    let procs = Procs::new(&[p(1)]);
    let s1 = session_req("x", "/", None, "s1").session.unwrap();
    let s2 = session_req("x", "/", None, "s2").session.unwrap();
    {
        let s = Store::open(&path).unwrap();
        let (a, _) = register(
            &s,
            &procs,
            session_req("backend", "/w", Some(p(1)), "s1"),
            1,
        );
        register(
            &s,
            &procs,
            session_req("frontend", "/w", Some(p(1)), "s2"),
            2,
        );
        s.signoff(&a.address).unwrap();
        assert_eq!(s.find_by_session(&s1).unwrap(), None);
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.find_by_session(&s1).unwrap(), None);
    assert_eq!(
        s.find_by_session(&s2).unwrap().unwrap().name,
        "frontend".to_string()
    );
}

#[test]
fn plain_reregistration_reports_existing() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (first, _) = s
        .register_and_release(req("backend", "/w", Some(p(1))), 1, &procs.alive())
        .unwrap();
    assert!(!first.existing && !first.reclaimed);
    let (again, _) = s
        .register_and_release(req("backend", "/w", Some(p(1))), 2, &procs.alive())
        .unwrap();
    assert_eq!(again.agent.address, first.agent.address);
    assert!(again.existing && !again.reclaimed);
}

fn kiro_req(name: &str, proc_id: Option<ProcId>, session: Option<&str>) -> RegisterReq {
    let mut r = req(name, "/w", proc_id);
    r.harness = Harness::Kiro;
    r.session = session.map(|id| SessionKey {
        harness: Harness::Kiro,
        id: id.into(),
    });
    r
}

fn kiro_key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Kiro,
        id: id.into(),
    }
}

#[test]
fn kiro_process_then_session_registration_keeps_one_address() {
    // kiro's MCP server registers without a session, its CLI with
    // KIRO_SESSION_ID, both from the same kiro-cli process.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = s
        .register_and_release(kiro_req("backend", Some(p(1)), None), 1, &procs.alive())
        .unwrap();
    let (b, _) = s
        .register_and_release(
            kiro_req("backend", Some(p(1)), Some("k1")),
            2,
            &procs.alive(),
        )
        .unwrap();
    assert_eq!(b.agent.address, a.agent.address);
    assert!(b.existing && !b.reclaimed);
    assert_eq!(s.list_agents().unwrap().len(), 1);
    assert_eq!(
        s.find_by_session(&kiro_key("k1")).unwrap().unwrap().address,
        a.agent.address
    );
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        a.agent.address
    );
}

#[test]
fn kiro_session_then_process_registration_keeps_one_address() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, kiro_req("backend", Some(p(1)), Some("k1")), 1);
    let (b, reclaimed) = register(&s, &procs, kiro_req("backend", Some(p(1)), None), 2);
    assert_eq!(b.address, a.address);
    assert!(!reclaimed);
    // The process's own session stays bound.
    assert_eq!(b.session, Some(kiro_key("k1")));
    assert_eq!(s.list_agents().unwrap().len(), 1);
    assert_eq!(
        s.find_by_session(&kiro_key("k1")).unwrap().unwrap().address,
        a.address
    );
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        a.address
    );
}

#[test]
fn kiro_rename_from_process_takes_its_session() {
    // A second name from the same kiro-cli process moves the process
    // binding and, since that session runs in this process, the session.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, kiro_req("backend", Some(p(1)), Some("k1")), 1);
    let (b, _) = register(&s, &procs, kiro_req("frontend", Some(p(1)), None), 2);
    assert_ne!(a.address, b.address);
    assert_eq!(b.session, Some(kiro_key("k1")));
    let old = s.get_agent(&a.address).unwrap().unwrap();
    assert_eq!(old.proc_id, None);
    assert_eq!(old.session, None);
    assert_eq!(
        s.find_by_session(&kiro_key("k1")).unwrap().unwrap().address,
        b.address
    );
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        b.address
    );
}

#[test]
fn session_registration_unbinds_process_level_agent() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(&s, &procs, kiro_req("docs", Some(p(1)), None), 1);
    let (b, _) = register(&s, &procs, kiro_req("backend", Some(p(1)), Some("k1")), 2);
    assert_ne!(a.address, b.address);
    assert_eq!(s.get_agent(&a.address).unwrap().unwrap().proc_id, None);
    assert_eq!(
        s.find_by_procs(&[p(1)]).unwrap().unwrap().address,
        b.address
    );
}

#[test]
fn existing_session_moving_process_claims_it() {
    // The existing-session path applies the one-address-per-process rule
    // on the new process too.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1), p(2)]);
    let (a, _) = register(&s, &procs, kiro_req("backend", Some(p(1)), Some("k1")), 1);
    let (x, _) = register(&s, &procs, req("docs", "/w", Some(p(2))), 2);
    let (b, _) = register(&s, &procs, kiro_req("backend", Some(p(2)), Some("k1")), 3);
    assert_eq!(b.address, a.address);
    assert_eq!(b.proc_id, Some(p(2)));
    assert_eq!(s.get_agent(&x.address).unwrap().unwrap().proc_id, None);
    assert_eq!(
        s.find_by_procs(&[p(2)]).unwrap().unwrap().address,
        a.address
    );
}

#[test]
fn existing_session_without_proc_keeps_binding() {
    let (_d, s) = store();
    let procs = Procs::new(&[p(1)]);
    let (a, _) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(1)), "s1"),
        1,
    );
    let (b, _) = register(&s, &procs, session_req("backend", "/w", None, "s1"), 2);
    assert_eq!(b.address, a.address);
    assert_eq!(b.proc_id, Some(p(1)));
    assert_eq!(Store::presence(&b, &procs.alive()), Presence::Active);
}

#[test]
fn unsigned_session_agent_on_dead_process_is_not_reclaimed() {
    let (_d, s) = store();
    let mut procs = Procs::new(&[p(1), p(2)]);
    let (a, _) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(1)), "s1"),
        1,
    );
    procs.kill(p(1));
    // Neither another session nor a session-less registration takes it.
    let (b, reclaimed) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(2)), "s2"),
        2,
    );
    assert!(!reclaimed);
    assert_ne!(b.address, a.address);
    let (c, reclaimed) = register(&s, &procs, req("backend", "/w", Some(p(2))), 3);
    assert!(!reclaimed);
    assert_ne!(c.address, a.address);
    let s1 = session_req("x", "/", None, "s1").session.unwrap();
    assert_eq!(s.find_by_session(&s1).unwrap().unwrap().address, a.address);
    // Once s1 signs off, the address is reclaimable again.
    s.signoff(&a.address).unwrap();
    let (d, reclaimed) = register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(2)), "s3"),
        4,
    );
    assert!(reclaimed);
    assert_eq!(d.address, a.address);
}

#[test]
fn find_by_procs_stops_at_a_shared_process() {
    // A caller under the opencode service never falls through to an outer
    // ancestor's agent: the service's sessions are the nearer identities.
    let (_d, s) = store();
    let procs = Procs::new(&[p(1), p(2)]);
    register(
        &s,
        &procs,
        session_req("backend", "/w", Some(p(1)), "s1"),
        1,
    );
    let (outer, _) = register(&s, &procs, req("outer", "/w", Some(p(2))), 2);
    assert_eq!(s.find_by_procs(&[p(1), p(2)]).unwrap(), None);
    assert_eq!(
        s.find_by_procs(&[p(2)]).unwrap().unwrap().address,
        outer.address
    );
}
