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

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
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
    assert!(matches!(err, StoreError::UnknownRecipient(_)), "{err:?}");
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
        .insert_held(
            note(&a, &[], "For frontend", "hi"),
            1,
            repo,
            &names(&["frontend"]),
        )
        .unwrap();
    assert!(m.to.is_empty());
    // A message already delivered to an existing recipient can also be held
    // for a name that is not registered yet.
    let other = agent(&s, "docs", Some("/repo"));
    let m2 = s
        .insert_message(note(&a, &[&other], "Also for frontend", "hey"), 2)
        .unwrap();
    s.hold_for_name(repo, "frontend", &m2.id).unwrap();
    // Not released to a different name or repo.
    assert!(s.release_held(repo, "docs", &other).unwrap().is_empty());
    let elsewhere = agent(&s, "frontend", Some("/other"));
    assert!(
        s.release_held(Path::new("/other"), "frontend", &elsewhere)
            .unwrap()
            .is_empty()
    );
    let f = agent(&s, "frontend", Some("/repo"));

    let released = s.release_held(repo, "frontend", &f).unwrap();
    assert_eq!(ids(&released), vec![m.id.as_str(), m2.id.as_str()]);
    assert_eq!(released[0].to, vec![f.clone()]);
    assert!(released[1].to.contains(&f));
    assert!(released[1].to.contains(&other));
    let inbox = s.inbox(&f, false).unwrap();
    let mut inbox_ids = ids(&inbox);
    inbox_ids.sort_unstable();
    assert_eq!(inbox_ids, vec![m.id.as_str(), m2.id.as_str()]);
    assert!(inbox.iter().all(|msg| msg.to.contains(&f)));
    assert_eq!(s.get_message(&m.id).unwrap().unwrap().to, vec![f.clone()]);
    let stored2 = s.get_message(&m2.id).unwrap().unwrap().to;
    assert!(stored2.contains(&f) && stored2.contains(&other));
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
    assert_eq!(by_human[0].thread_id, t2.id);

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
    let held = s
        .insert_held(
            note(&a, &[], "Held", "5"),
            100,
            Path::new("/repo"),
            &names(&["docs"]),
        )
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
    // What this guards: the mailbox row written for the human's own reply.
    // It is already read, so it stays out of the unread inbox but shows
    // with `include_read`, which keeps the thread in the human's mail.
    assert!(s.inbox(HUMAN, false).unwrap().is_empty());
    assert_eq!(ids(&s.inbox(HUMAN, true).unwrap()), vec![h.id.as_str()]);
    assert_eq!(s.unread_counts(HUMAN).unwrap(), (0, 0));
    // The thread listing does not depend on that row (it finds the thread
    // because the human sent in it); this only pins participant order.
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
    let m = s
        .insert_held(note(&a, &[], "Hi", "x"), 1, repo, &names(&["frontend"]))
        .unwrap();
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
    assert!(matches!(err, StoreError::InvalidRecipient(_)), "{err:?}");
    let err = s
        .insert_held(
            note(&a, &[&a], "Me", "x"),
            1,
            Path::new("/repo"),
            &names(&["docs"]),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidRecipient(_)), "{err:?}");
}

#[test]
fn insert_message_without_recipients_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", Some("/repo"));
    let err = s
        .insert_message(note(&a, &[], "Nobody", "x"), 1)
        .unwrap_err();
    assert_eq!(err, StoreError::InvalidInput("no recipients".into()));
    assert!(s.search("Nobody").unwrap().is_empty());
}

#[test]
fn insert_held_delivers_and_holds_atomically() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let b = agent(&s, "frontend", Some("/repo"));
    let m = s
        .insert_held(
            note(&a, &[&b], "Mixed", "x"),
            1,
            repo,
            &names(&["docs", "qa"]),
        )
        .unwrap();
    assert_eq!(m.to, vec![b.clone()]);
    assert_eq!(ids(&s.inbox(&b, false).unwrap()), vec![m.id.as_str()]);
    let d = agent(&s, "docs", Some("/repo"));
    assert_eq!(
        ids(&s.release_held(repo, "docs", &d).unwrap()),
        vec![m.id.as_str()]
    );
    let q = agent(&s, "qa", Some("/repo"));
    let released = s.release_held(repo, "qa", &q).unwrap();
    assert_eq!(released[0].to, vec![b, d, q]);

    // Nothing is stored when any part fails.
    let err = s
        .insert_held(note(&a, &[], "Bad", "x"), 1, repo, &names(&["Not A Name"]))
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidName(_)), "{err:?}");
    let err = s
        .insert_held(
            note(&a, &[], "Bad", "x"),
            1,
            Path::new("rel"),
            &names(&["docs"]),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    let err = s
        .insert_held(note(&a, &[], "Bad", "x"), 1, repo, &[])
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(s.search("Bad").unwrap().is_empty());
}

#[test]
fn held_repo_path_is_normalized() {
    let (_d, s) = store();
    let a = agent(&s, "backend", Some("/repo"));
    let m = s
        .insert_held(
            note(&a, &[], "Slash", "x"),
            1,
            Path::new("/repo/"),
            &names(&["frontend"]),
        )
        .unwrap();
    let m2 = s
        .insert_held(
            note(&a, &[], "Dot", "y"),
            2,
            Path::new("/repo/./"),
            &names(&["frontend"]),
        )
        .unwrap();
    let m3 = s
        .insert_held(
            note(&a, &[], "Parent", "z"),
            3,
            Path::new("/repo/sub/.."),
            &names(&["frontend"]),
        )
        .unwrap();
    let m4 = s
        .insert_held(
            note(&a, &[], "Above root", "w"),
            4,
            Path::new("/../repo/x/../"),
            &names(&["frontend"]),
        )
        .unwrap();
    // The agent's own repo may be spelled with `..` as well.
    let f = agent(&s, "frontend", Some("/other/../repo"));
    assert_eq!(
        ids(&s.release_held(Path::new("/repo"), "frontend", &f).unwrap()),
        vec![
            m.id.as_str(),
            m2.id.as_str(),
            m3.id.as_str(),
            m4.id.as_str()
        ]
    );
    let err = s
        .hold_for_name(Path::new("repo"), "frontend", &m.id)
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    let err = s
        .release_held(Path::new("repo"), "frontend", &f)
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    // hold_for_name normalizes too.
    s.hold_for_name(Path::new("/repo//"), "docs", &m.id)
        .unwrap();
    let d = agent(&s, "docs", Some("/repo"));
    assert_eq!(
        ids(&s.release_held(Path::new("/repo"), "docs", &d).unwrap()),
        vec![m.id.as_str()]
    );
}

#[test]
fn release_held_checks_agent_matches_name_and_repo() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let m = s
        .insert_held(
            note(&a, &[], "For frontend", "hi"),
            1,
            repo,
            &names(&["frontend"]),
        )
        .unwrap();
    // Wrong name.
    let docs = agent(&s, "docs", Some("/repo"));
    let err = s.release_held(repo, "frontend", &docs).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    // Right name, wrong repo.
    let other = agent(&s, "frontend", Some("/other"));
    let err = s.release_held(repo, "frontend", &other).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    // No repo: the cwd stands in for it.
    let no_repo = agent(&s, "frontend", None);
    let err = s.release_held(repo, "frontend", &no_repo).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(
        s.release_held(Path::new("/w"), "frontend", &no_repo)
            .unwrap()
            .is_empty()
    );
    // Still held for the right agent.
    let f = agent(&s, "frontend", Some("/repo"));
    assert_eq!(
        ids(&s.release_held(repo, "frontend", &f).unwrap()),
        vec![m.id.as_str()]
    );
}

fn question(from: &str, to: &[&str], subject: &str) -> NewMessage {
    NewMessage {
        kind: Kind::Question,
        ..note(from, to, subject, "?")
    }
}

fn answer(from: &str, to: &[&str], parent: &str) -> NewMessage {
    NewMessage {
        kind: Kind::Answer,
        ..reply(from, to, parent, "!")
    }
}

fn open_question_threads(s: &Store) -> usize {
    s.threads(&ThreadFilter {
        open_questions: true,
        ..ThreadFilter::default()
    })
    .unwrap()
    .len()
}

#[test]
fn answer_replying_inside_the_question_thread_closes_it() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = s.insert_message(question(&a, &[&b], "Q"), 1).unwrap();
    // b asks for clarification, a clarifies, b answers the clarification.
    let c = s
        .insert_message(reply(&b, &[&a], &q.id, "which endpoint?"), 2)
        .unwrap();
    let r = s
        .insert_message(reply(&a, &[&b], &c.id, "POST /orders"), 3)
        .unwrap();
    assert_eq!(s.unread_counts(&b).unwrap().1, 1);
    s.insert_message(answer(&b, &[&a], &r.id), 4).unwrap();
    assert_eq!(s.unread_counts(&b).unwrap().1, 0);
    assert_eq!(open_question_threads(&s), 0);
}

#[test]
fn only_a_recipient_or_human_answer_closes_a_question() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let q = s.insert_message(question(&a, &[&b], "Q"), 1).unwrap();
    // c joins the thread through a later note, but was never asked.
    s.insert_message(reply(&a, &[&b, &c], &q.id, "cc docs"), 2)
        .unwrap();
    // Whether such answers are accepted is not pinned here; only that they
    // close nothing.
    let _ = s.insert_message(answer(&c, &[&a], &q.id), 3);
    assert_eq!(s.unread_counts(&b).unwrap().1, 1);
    assert_eq!(open_question_threads(&s), 1);
    // The asker cannot answer its own question either.
    let _ = s.insert_message(answer(&a, &[&b], &q.id), 4);
    assert_eq!(s.unread_counts(&b).unwrap().1, 1);
    assert_eq!(open_question_threads(&s), 1);
    // human may answer any question.
    s.insert_message(answer(HUMAN, &[&a], &q.id), 5).unwrap();
    assert_eq!(s.unread_counts(&b).unwrap().1, 0);
    assert_eq!(open_question_threads(&s), 0);
}

#[test]
fn outside_agent_cannot_reply_into_a_thread() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let root = s.insert_message(note(&a, &[&b], "Orders", "1"), 1).unwrap();
    let err = s
        .insert_message(reply(&c, &[&a], &root.id, "me too"), 2)
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert_eq!(ids(&s.thread(&root.id).unwrap()), vec![root.id.as_str()]);
    assert!(s.inbox(&a, true).unwrap().is_empty());
    // Once addressed in the thread, c may reply.
    let cc = s
        .insert_message(reply(&a, &[&b, &c], &root.id, "cc docs"), 3)
        .unwrap();
    s.insert_message(reply(&c, &[&a], &cc.id, "ok"), 4).unwrap();
    assert_eq!(s.thread(&root.id).unwrap().len(), 3);
}

#[test]
fn released_held_question_is_open_to_its_new_recipient() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let q = s
        .insert_held(question(&a, &[], "Q"), 1, repo, &names(&["docs"]))
        .unwrap();
    assert_eq!(open_question_threads(&s), 1);
    let d = agent(&s, "docs", Some("/repo"));
    s.release_held(repo, "docs", &d).unwrap();
    assert_eq!(ids(&s.inbox(&d, false).unwrap()), vec![q.id.as_str()]);
    assert_eq!(s.unread_counts(&d).unwrap(), (1, 1));
    // Released, docs is now a recipient: it may reply and its answer closes.
    s.insert_message(answer(&d, &[&a], &q.id), 2).unwrap();
    assert_eq!(s.unread_counts(&d).unwrap(), (1, 0));
    assert_eq!(open_question_threads(&s), 0);
}

#[test]
fn answer_without_parent_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let err = s
        .insert_message(
            NewMessage {
                kind: Kind::Answer,
                ..note(&a, &[&b], "Orphan", "!")
            },
            1,
        )
        .unwrap_err();
    assert_eq!(
        err,
        StoreError::InvalidInput("an answer must reply to a message".into())
    );
    assert!(s.inbox(&b, true).unwrap().is_empty());
    assert!(s.threads(&ThreadFilter::default()).unwrap().is_empty());
    assert!(s.search("Orphan").unwrap().is_empty());
    // Held mail goes through the same check.
    let err = s
        .insert_held(
            NewMessage {
                kind: Kind::Answer,
                ..note(&a, &[], "Orphan", "!")
            },
            1,
            Path::new("/repo"),
            &names(&["docs"]),
        )
        .unwrap_err();
    assert_eq!(
        err,
        StoreError::InvalidInput("an answer must reply to a message".into())
    );
    assert!(s.search("Orphan").unwrap().is_empty());
}

fn by_repo(s: &Store, repo: &str) -> Result<Vec<String>, StoreError> {
    Ok(s.threads(&ThreadFilter {
        repo: Some(PathBuf::from(repo)),
        ..ThreadFilter::default()
    })?
    .into_iter()
    .map(|t| t.thread_id)
    .collect())
}

#[test]
fn threads_repo_filter_is_normalized() {
    let (_d, s) = store();
    let a = agent(&s, "backend", Some("/other/../r1"));
    let b = agent(&s, "frontend", Some("/r2"));
    let t1 = s.insert_message(note(&a, &[HUMAN], "One", "1"), 1).unwrap();
    s.insert_message(note(&b, &[HUMAN], "Two", "2"), 2).unwrap();
    for filter in ["/r1", "/r1/", "/r1/./", "/x/../r1", "/r1/sub/.."] {
        assert_eq!(
            by_repo(&s, filter).unwrap(),
            vec![t1.id.clone()],
            "{filter}"
        );
    }
    // A sibling whose name merely shares a prefix is not inside /r1.
    assert!(by_repo(&s, "/r").unwrap().is_empty());
    assert!(matches!(
        by_repo(&s, "r1").unwrap_err(),
        StoreError::InvalidInput(_)
    ));
}

#[test]
fn threads_repo_filter_matches_cwd_without_repo() {
    let (_d, s) = store();
    let req = agentmail::store::RegisterReq {
        name: "scratch".into(),
        role: None,
        working_on: None,
        harness: Harness::Other,
        cwd: PathBuf::from("/r3/./sub"),
        repo: None,
        proc_id: None,
        fresh: true,
        wake: false,
    };
    let a = s.register(req, 1, &|_| false).unwrap().0.address;
    let t = s.insert_message(note(&a, &[HUMAN], "Cwd", "1"), 1).unwrap();
    assert_eq!(by_repo(&s, "/r3").unwrap(), vec![t.id.clone()]);
    assert_eq!(by_repo(&s, "/r3/sub/").unwrap(), vec![t.id]);
    assert!(by_repo(&s, "/r3/other").unwrap().is_empty());
}

#[test]
fn release_held_to_agent_with_relative_cwd_is_a_repo_mismatch() {
    let (_d, s) = store();
    let req = agentmail::store::RegisterReq {
        name: "frontend".into(),
        role: None,
        working_on: None,
        harness: Harness::Other,
        cwd: PathBuf::from("relative/dir"),
        repo: None,
        proc_id: None,
        fresh: true,
        wake: false,
    };
    let f = s.register(req, 1, &|_| false).unwrap().0.address;
    let err = s
        .release_held(Path::new("/repo"), "frontend", &f)
        .unwrap_err();
    match err {
        StoreError::InvalidInput(msg) => {
            assert!(msg.contains("relative/dir"), "{msg}");
            assert!(msg.contains("not /repo"), "{msg}");
            assert!(!msg.contains("not an absolute path"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
}
