//! Task 4: messages, mailboxes, threads and held mail in the redb store.

use std::path::{Path, PathBuf};

use agentmail::model::{HUMAN, Harness, Kind, Message};
use agentmail::store::{MAX_BODY, NewMessage, Store, StoreError, ThreadFilter};
use tempfile::TempDir;

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
    (dir, store)
}

/// Register an agent (unbound, so never reclaimed by another test call) and
/// return its address.
fn agent(store: &Store, name: &str, repo: Option<&str>) -> String {
    let req = agentmail::store::RegisterReq {
        name: name.into(),
        role: None,
        working_on: None,
        harness: Harness::Other,
        cwd: PathBuf::from(repo.unwrap_or("/w")),
        repo: repo.map(PathBuf::from),
        proc_id: None,
        fresh: true,
        wake: false,
    };
    store.register(req, 1, &|_| false).unwrap().0.address
}

fn note(from: &str, to: &[&str], subject: &str, body: &str) -> NewMessage {
    NewMessage {
        from: from.into(),
        to: to.iter().map(|s| s.to_string()).collect(),
        subject: Some(subject.into()),
        body: body.into(),
        kind: Kind::Note,
        urgent: false,
        refs: vec![],
        in_reply_to: None,
    }
}

fn reply(from: &str, to: &[&str], parent: &str, body: &str) -> NewMessage {
    NewMessage {
        subject: None,
        in_reply_to: Some(parent.into()),
        ..note(from, to, "", body)
    }
}

fn ids(msgs: &[Message]) -> Vec<&str> {
    msgs.iter().map(|m| m.id.as_str()).collect()
}

#[test]
fn send_and_inbox() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let m = s
        .insert_message(note(&a, &[&b], "Orders", "hi"), 10)
        .unwrap();
    assert_eq!(m.thread_id, m.id);
    assert_eq!(m.subject, "Orders");
    assert_eq!(m.from, a);
    assert_eq!(m.to, vec![b.clone()]);
    assert_eq!(m.created_at, 10);
    assert!(!m.from_human);
    assert_eq!(ids(&s.inbox(&b, false).unwrap()), vec![m.id.as_str()]);
    assert!(s.inbox(&a, false).unwrap().is_empty());
    assert_eq!(s.get_message(&m.id).unwrap(), Some(m));
    assert_eq!(s.unread_counts(&b).unwrap(), (1, 0));
    assert_eq!(s.unread_counts(&a).unwrap(), (0, 0));
}

#[test]
fn send_to_several_recipients_dedupes() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let m = s
        .insert_message(note(&a, &[&b, &c, &b], "Orders", "hi"), 10)
        .unwrap();
    assert_eq!(m.to, vec![b.clone(), c.clone()]);
    assert_eq!(s.inbox(&b, false).unwrap().len(), 1);
    assert_eq!(s.inbox(&c, false).unwrap().len(), 1);
}

#[test]
fn unknown_sender_or_recipient_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let err = s
        .insert_message(note(&a, &["frontend-zzzzzzzz"], "Orders", "hi"), 1)
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    let err = s
        .insert_message(note("ghost-zzzzzzzz", &[&a], "Orders", "hi"), 1)
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    let err = s
        .insert_message(reply(&a, &[HUMAN], "01ARZ3NDEKTSV4RRFFQ69G5FAV", "x"), 1)
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
}

#[test]
fn new_thread_requires_subject() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    for subject in [None, Some("   ".to_string())] {
        let m = NewMessage {
            subject,
            ..note(&a, &[HUMAN], "", "hi")
        };
        let err = s.insert_message(m, 1).unwrap_err();
        assert_eq!(err, StoreError::InvalidInput("subject required".into()));
    }
}

#[test]
fn mark_read_hides_from_unread_inbox() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let m1 = s.insert_message(note(&a, &[&b], "One", "1"), 1).unwrap();
    let m2 = s.insert_message(note(&a, &[&b], "Two", "2"), 2).unwrap();
    s.mark_read(&b, std::slice::from_ref(&m1.id)).unwrap();
    assert_eq!(ids(&s.inbox(&b, false).unwrap()), vec![m2.id.as_str()]);
    assert_eq!(
        ids(&s.inbox(&b, true).unwrap()),
        vec![m1.id.as_str(), m2.id.as_str()]
    );
    assert_eq!(s.unread_counts(&b).unwrap(), (1, 0));
    // Ids not in the caller's mailbox are ignored.
    s.mark_read(&a, &[m2.id.clone(), "nope".into()]).unwrap();
    assert_eq!(s.unread_counts(&b).unwrap(), (1, 0));
}

#[test]
fn reply_threads_and_prefixes_subject() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let root = s.insert_message(note(&a, &[&b], "Orders", "1"), 1).unwrap();
    let r1 = s
        .insert_message(reply(&b, &[&a], &root.id, "2"), 2)
        .unwrap();
    let r2 = s.insert_message(reply(&a, &[&b], &r1.id, "3"), 3).unwrap();
    assert_eq!(r1.subject, "Re: Orders");
    assert_eq!(r2.subject, "Re: Orders");
    assert_eq!(r1.thread_id, root.id);
    assert_eq!(r2.thread_id, root.id);
    assert_eq!(r2.in_reply_to.as_deref(), Some(r1.id.as_str()));
    assert_eq!(
        ids(&s.thread(&root.id).unwrap()),
        vec![root.id.as_str(), r1.id.as_str(), r2.id.as_str()]
    );
    assert!(s.thread("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap().is_empty());
}

#[test]
fn reply_to_re_subject_root_does_not_double_prefix() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let root = s
        .insert_message(note(&a, &[&b], "Re: Orders", "1"), 1)
        .unwrap();
    let r = s
        .insert_message(reply(&b, &[&a], &root.id, "2"), 2)
        .unwrap();
    assert_eq!(r.subject, "Re: Orders");
}

#[test]
fn body_too_large() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let big = "x".repeat(MAX_BODY + 1);
    assert_eq!(
        s.insert_message(note(&a, &[&b], "Big", &big), 1)
            .unwrap_err(),
        StoreError::TooLarge
    );
    assert!(s.inbox(&b, true).unwrap().is_empty());
    let exact = "x".repeat(MAX_BODY);
    let m = s.insert_message(note(&a, &[&b], "Big", &exact), 1).unwrap();
    assert_eq!(m.body.len(), MAX_BODY);
    // Multi-byte characters count in bytes, not chars.
    let wide = "é".repeat(MAX_BODY / 2 + 1);
    assert_eq!(
        s.insert_message(note(&a, &[&b], "Big", &wide), 1)
            .unwrap_err(),
        StoreError::TooLarge
    );
}

#[test]
fn body_round_trip_exact() {
    let (dir, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let body = "```rust\nfn x(){}\n```\r\n| a | b |\n| - | - |\nnaïve ✓\n";
    let m = s.insert_message(note(&a, &[&b], "Body", body), 1).unwrap();
    assert_eq!(m.body, body);
    drop(s);
    let s = Store::open(&dir.path().join("agentmail.redb")).unwrap();
    assert_eq!(s.get_message(&m.id).unwrap().unwrap().body, body);
    assert_eq!(s.inbox(&b, false).unwrap()[0].body, body);
}

#[test]
fn held_mail_released_on_register() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let m = s
        .insert_message(note(&a, &[], "For frontend", "hi"), 1)
        .unwrap();
    s.hold_for_name(repo, "frontend", &m.id).unwrap();
    // Not released to a different name or repo.
    let other = agent(&s, "docs", Some("/repo"));
    assert!(s.release_held(repo, "docs", &other).unwrap().is_empty());
    let f = agent(&s, "frontend", Some("/repo"));
    assert!(
        s.release_held(Path::new("/other"), "frontend", &f)
            .unwrap()
            .is_empty()
    );

    let released = s.release_held(repo, "frontend", &f).unwrap();
    assert_eq!(ids(&released), vec![m.id.as_str()]);
    assert_eq!(released[0].to, vec![f.clone()]);
    let inbox = s.inbox(&f, false).unwrap();
    assert_eq!(ids(&inbox), vec![m.id.as_str()]);
    assert!(inbox[0].to.contains(&f));
    assert_eq!(s.get_message(&m.id).unwrap().unwrap().to, vec![f.clone()]);
    // Released once only.
    let g = agent(&s, "frontend", Some("/repo"));
    assert!(s.release_held(repo, "frontend", &g).unwrap().is_empty());
}

#[test]
fn hold_requires_existing_message() {
    let (_d, s) = store();
    let err = s
        .hold_for_name(Path::new("/repo"), "frontend", "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
}

#[test]
fn search_case_insensitive() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let m1 = s
        .insert_message(note(&a, &[&b], "Orders API", "body one"), 1)
        .unwrap();
    let m2 = s
        .insert_message(note(&b, &[&a], "Other", "the ORDERS table"), 2)
        .unwrap();
    let m3 = s
        .insert_message(note(HUMAN, &[&a], "Hello", "x"), 3)
        .unwrap();
    assert_eq!(
        ids(&s.search("orders").unwrap()),
        vec![m1.id.as_str(), m2.id.as_str()]
    );
    assert_eq!(ids(&s.search("HUM").unwrap()), vec![m3.id.as_str()]);
    assert_eq!(ids(&s.search("FRONTEND").unwrap()), vec![m2.id.as_str()]);
    assert!(s.search("nothing here").unwrap().is_empty());
    assert!(matches!(
        s.search("  ").unwrap_err(),
        StoreError::InvalidInput(_)
    ));
}

#[test]
fn threads_filter_by_agent() {
    let (_d, s) = store();
    let a = agent(&s, "backend", Some("/r1"));
    let b = agent(&s, "frontend", Some("/r1"));
    let c = agent(&s, "docs", Some("/r2"));
    let t1 = s.insert_message(note(&a, &[&b], "Orders", "1"), 1).unwrap();
    s.insert_message(reply(&b, &[&a], &t1.id, "2"), 5).unwrap();
    let t2 = s
        .insert_message(note(&c, &[HUMAN], "Docs", "3"), 3)
        .unwrap();

    let all = s.threads(&ThreadFilter::default()).unwrap();
    // Most recent activity first.
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].thread_id, t1.id);
    assert_eq!(all[0].subject, "Orders");
    assert_eq!(all[0].participants, vec![a.clone(), b.clone()]);
    assert_eq!(all[0].messages, 2);
    assert_eq!(all[0].last_at, 5);
    assert_eq!(all[0].open_questions, 0);
    assert_eq!(all[1].thread_id, t2.id);

    let by_c = s
        .threads(&ThreadFilter {
            agent: Some(c.clone()),
            ..ThreadFilter::default()
        })
        .unwrap();
    assert_eq!(by_c.len(), 1);
    assert_eq!(by_c[0].thread_id, t2.id);

    let by_human = s
        .threads(&ThreadFilter {
            agent: Some(HUMAN.into()),
            ..ThreadFilter::default()
        })
        .unwrap();
    assert_eq!(by_human.len(), 1);

    let by_repo = s
        .threads(&ThreadFilter {
            repo: Some(PathBuf::from("/r1")),
            ..ThreadFilter::default()
        })
        .unwrap();
    assert_eq!(by_repo.len(), 1);
    assert_eq!(by_repo[0].thread_id, t1.id);
}

#[test]
fn threads_and_counts_track_open_questions() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let n = s.insert_message(note(&a, &[&b], "Note", "n"), 1).unwrap();
    let q = s
        .insert_message(
            NewMessage {
                kind: Kind::Question,
                ..note(&a, &[&b], "Q", "?")
            },
            2,
        )
        .unwrap();
    // Open questions to b come first, even when older notes are unread.
    assert_eq!(
        ids(&s.inbox(&b, false).unwrap()),
        vec![q.id.as_str(), n.id.as_str()]
    );
    // A read open question still shows, first.
    s.mark_read(&b, std::slice::from_ref(&q.id)).unwrap();
    assert_eq!(
        ids(&s.inbox(&b, false).unwrap()),
        vec![q.id.as_str(), n.id.as_str()]
    );
    assert_eq!(s.unread_counts(&b).unwrap(), (1, 1));
    let open = s
        .threads(&ThreadFilter {
            open_questions: true,
            ..ThreadFilter::default()
        })
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].thread_id, q.id);
    assert_eq!(open[0].open_questions, 1);

    // An answer from a recipient closes it.
    s.insert_message(
        NewMessage {
            kind: Kind::Answer,
            ..reply(&b, &[&a], &q.id, "!")
        },
        3,
    )
    .unwrap();
    assert_eq!(s.unread_counts(&b).unwrap(), (1, 0));
    assert_eq!(ids(&s.inbox(&b, false).unwrap()), vec![n.id.as_str()]);
    assert!(
        s.threads(&ThreadFilter {
            open_questions: true,
            ..ThreadFilter::default()
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn prune_removes_old_threads_only() {
    let (_d, s) = store();
    let a = agent(&s, "backend", Some("/repo"));
    let b = agent(&s, "frontend", None);
    let old = s.insert_message(note(&a, &[&b], "Old", "1"), 100).unwrap();
    let old_r = s
        .insert_message(reply(&b, &[&a], &old.id, "2"), 150)
        .unwrap();
    // A thread with an old root but a recent reply stays whole.
    let mixed = s
        .insert_message(note(&a, &[&b], "Mixed", "3"), 100)
        .unwrap();
    let mixed_r = s
        .insert_message(reply(&b, &[&a], &mixed.id, "4"), 300)
        .unwrap();
    let held = s.insert_message(note(&a, &[], "Held", "5"), 100).unwrap();
    s.hold_for_name(Path::new("/repo"), "docs", &held.id)
        .unwrap();

    assert_eq!(s.prune(200).unwrap(), 2);
    assert_eq!(s.get_message(&old.id).unwrap(), None);
    assert_eq!(s.get_message(&old_r.id).unwrap(), None);
    assert_eq!(s.get_message(&held.id).unwrap(), None);
    assert!(s.thread(&old.id).unwrap().is_empty());
    assert_eq!(
        ids(&s.thread(&mixed.id).unwrap()),
        vec![mixed.id.as_str(), mixed_r.id.as_str()]
    );
    assert_eq!(ids(&s.inbox(&b, true).unwrap()), vec![mixed.id.as_str()]);
    assert_eq!(ids(&s.inbox(&a, true).unwrap()), vec![mixed_r.id.as_str()]);
    let d = agent(&s, "docs", Some("/repo"));
    assert!(
        s.release_held(Path::new("/repo"), "docs", &d)
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.threads(&ThreadFilter::default()).unwrap().len(), 1);
    assert_eq!(s.prune(200).unwrap(), 0);
}

#[test]
fn human_flag() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let m = s.insert_message(note(HUMAN, &[&a], "Hi", "x"), 1).unwrap();
    assert!(m.from_human);
    let r = s
        .insert_message(reply(&a, &[HUMAN], &m.id, "y"), 2)
        .unwrap();
    assert!(!r.from_human);
    assert_eq!(ids(&s.inbox(HUMAN, false).unwrap()), vec![r.id.as_str()]);
}

#[test]
fn human_reply_in_unaddressed_thread_joins_it() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let root = s.insert_message(note(&a, &[&b], "Orders", "1"), 1).unwrap();
    let h = s
        .insert_message(reply(HUMAN, &[&a, &b], &root.id, "use v2"), 2)
        .unwrap();
    assert!(h.from_human);
    // The human's own reply lands in its mailbox, already read.
    assert!(s.inbox(HUMAN, false).unwrap().is_empty());
    assert_eq!(ids(&s.inbox(HUMAN, true).unwrap()), vec![h.id.as_str()]);
    let joined = s
        .threads(&ThreadFilter {
            agent: Some(HUMAN.into()),
            ..ThreadFilter::default()
        })
        .unwrap();
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].participants, vec![a, b, HUMAN.to_string()]);
}

#[test]
fn held_mail_is_never_released_to_its_sender() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "frontend", Some("/repo"));
    let m = s.insert_message(note(&a, &[], "Hi", "x"), 1).unwrap();
    s.hold_for_name(repo, "frontend", &m.id).unwrap();
    assert!(s.release_held(repo, "frontend", &a).unwrap().is_empty());
    let b = agent(&s, "frontend", Some("/repo"));
    assert_eq!(
        ids(&s.release_held(repo, "frontend", &b).unwrap()),
        vec![m.id.as_str()]
    );
    let err = s
        .release_held(repo, "frontend", "frontend-zzzzzzzz")
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
}

#[test]
fn self_send_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let err = s.insert_message(note(&a, &[&a], "Me", "x"), 1).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
}
