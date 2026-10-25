//! Task 5: open questions, answer closing and the deadlock guard.

use std::path::{Path, PathBuf};

use agentmail::model::{HUMAN, Harness, Kind, Message, Question};
use agentmail::store::{Alive, NewMessage, RegisterReq, Store, StoreError};
use tempfile::TempDir;

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
    (dir, store)
}

/// Register an unbound agent and return its address.
fn agent(store: &Store, name: &str, repo: Option<&str>) -> String {
    let req = RegisterReq {
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

const NOT_ALIVE: Alive<'static> = &|_| false;

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

fn ask(s: &Store, from: &str, to: &[&str]) -> Message {
    s.insert_message(question(from, to, "Q"), 1).unwrap()
}

fn qids(qs: &[Question]) -> Vec<&str> {
    qs.iter().map(|q| q.id.as_str()).collect()
}

fn addrs(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn question_row_is_created() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let q = ask(&s, &a, &[&b, &c]);
    assert_eq!(
        s.question(&q.id).unwrap(),
        Some(Question {
            id: q.id.clone(),
            thread_id: q.thread_id.clone(),
            asker: a.clone(),
            to: addrs(&[&b, &c]),
            answered_by: None,
        })
    );
    assert_eq!(qids(&s.open_questions_to(&b).unwrap()), vec![q.id.as_str()]);
    assert_eq!(qids(&s.open_questions_to(&c).unwrap()), vec![q.id.as_str()]);
    assert!(s.open_questions_to(&a).unwrap().is_empty());
    // A note makes no question row.
    let n = s.insert_message(note(&a, &[&b], "N", "x"), 2).unwrap();
    assert_eq!(s.question(&n.id).unwrap(), None);
    assert_eq!(s.question("nope").unwrap(), None);
}

#[test]
fn answer_closes_question() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    s.insert_message(answer(&b, &[&a], &q.id), 2).unwrap();
    assert_eq!(
        s.question(&q.id).unwrap().unwrap().answered_by,
        Some(b.clone())
    );
    assert!(s.open_questions_to(&b).unwrap().is_empty());
}

#[test]
fn second_answer_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    s.insert_message(answer(&b, &[&a], &q.id), 2).unwrap();
    let err = s.insert_message(answer(&b, &[&a], &q.id), 3).unwrap_err();
    assert_eq!(err, StoreError::InvalidInput("not an open question".into()));
    assert_eq!(s.thread(&q.thread_id).unwrap().len(), 2);
}

#[test]
fn answer_to_a_note_is_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let n = s.insert_message(note(&a, &[&b], "N", "x"), 1).unwrap();
    let err = s.insert_message(answer(&b, &[&a], &n.id), 2).unwrap_err();
    assert_eq!(err, StoreError::InvalidInput("not an open question".into()));
    assert!(s.inbox(&a, true).unwrap().is_empty());
}

#[test]
fn note_reply_does_not_close() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    s.insert_message(reply(&b, &[&a], &q.id, "which one?"), 2)
        .unwrap();
    assert_eq!(s.question(&q.id).unwrap().unwrap().answered_by, None);
    assert_eq!(qids(&s.open_questions_to(&b).unwrap()), vec![q.id.as_str()]);
}

#[test]
fn direct_cycle_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    let hit = s.would_deadlock(&b, &addrs(&[&a])).unwrap();
    assert_eq!(hit.map(|h| h.id), Some(q.id));
    // The other direction waits on nobody.
    assert_eq!(s.would_deadlock(&a, &addrs(&[&b])).unwrap(), None);
}

#[test]
fn long_cycle_refused() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let _ab = ask(&s, &a, &[&b]);
    let bc = ask(&s, &b, &[&c]);
    // c asking a closes a -> b -> c -> a; c should answer b's question.
    let hit = s.would_deadlock(&c, &addrs(&[&a])).unwrap();
    assert_eq!(hit.map(|h| h.id), Some(bc.id));
    // Among several recipients, one on a cycle is enough.
    let d = agent(&s, "ops", None);
    assert!(s.would_deadlock(&c, &addrs(&[&d, &a])).unwrap().is_some());
    assert_eq!(s.would_deadlock(&c, &addrs(&[&d])).unwrap(), None);
}

#[test]
fn cycle_through_a_multi_recipient_question() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let q = ask(&s, &a, &[&b, &c]);
    let hit = s.would_deadlock(&c, &addrs(&[&a])).unwrap();
    assert_eq!(hit.map(|h| h.id), Some(q.id));
}

#[test]
fn no_cycle_after_answer() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    s.insert_message(answer(&b, &[&a], &q.id), 2).unwrap();
    assert_eq!(s.would_deadlock(&b, &addrs(&[&a])).unwrap(), None);
}

#[test]
fn deadlock_check_terminates_on_existing_cycles() {
    // The store refuses a cyclic question, but releasing held mail can
    // still close a cycle (delivery is never refused), so the search must
    // survive cycles it did not start from.
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let c = agent(&s, "frontend", Some("/repo"));
    // a asks the not-yet-registered docs agent.
    s.insert_held(
        question(&a, &[], "Q"),
        1,
        repo,
        &addrs(&["docs"]),
        NOT_ALIVE,
    )
    .unwrap();
    let b = agent(&s, "docs", Some("/repo"));
    // b asks a before the held question is released: no cycle yet.
    ask(&s, &b, &[&a]);
    // Releasing adds the edge a -> b, closing a -> b -> a.
    s.release_held(repo, "docs", &b).unwrap();
    assert_eq!(s.would_deadlock(&c, &addrs(&[&a])).unwrap(), None);
    assert!(s.would_deadlock(&b, &addrs(&[&a])).unwrap().is_some());
}

#[test]
fn cyclic_question_is_refused_atomically() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    let err = s.insert_message(question(&b, &[&a], "Q2"), 2).unwrap_err();
    let StoreError::Deadlock(hit) = err else {
        panic!("expected Deadlock, got {err:?}");
    };
    assert_eq!(hit.id, q.id);
    // Nothing was stored.
    assert!(s.inbox(&a, true).unwrap().is_empty());
    assert!(s.open_questions_to(&a).unwrap().is_empty());
    assert_eq!(s.threads(&Default::default()).unwrap().len(), 1);
    // A note in the other direction is fine.
    s.insert_message(note(&b, &[&a], "N", "x"), 3).unwrap();
    // A clarifying question in the same thread would also wait on a.
    let err = s
        .insert_message(
            NewMessage {
                kind: Kind::Question,
                ..reply(&b, &[&a], &q.id, "which?")
            },
            4,
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Deadlock(_)), "{err:?}");
}

#[test]
fn cyclic_held_question_is_refused() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let b = agent(&s, "frontend", Some("/repo"));
    let q = ask(&s, &a, &[&b]);
    let err = s
        .insert_held(
            question(&b, &[&a], "Q2"),
            2,
            repo,
            &addrs(&["docs"]),
            NOT_ALIVE,
        )
        .unwrap_err();
    assert_eq!(
        err,
        StoreError::Deadlock(Box::new(s.question(&q.id).unwrap().unwrap()))
    );
    let d = agent(&s, "docs", Some("/repo"));
    assert!(s.release_held(repo, "docs", &d).unwrap().is_empty());
}

#[test]
fn simultaneous_opposite_questions_never_both_land() {
    for _ in 0..20 {
        let (_d, s) = store();
        let a = agent(&s, "backend", None);
        let b = agent(&s, "frontend", None);
        let barrier = std::sync::Barrier::new(2);
        let (ra, rb) = std::thread::scope(|scope| {
            let ha = scope.spawn(|| {
                barrier.wait();
                s.insert_message(question(&a, &[&b], "A"), 1)
            });
            let hb = scope.spawn(|| {
                barrier.wait();
                s.insert_message(question(&b, &[&a], "B"), 1)
            });
            (ha.join().unwrap(), hb.join().unwrap())
        });
        assert!(ra.is_ok() != rb.is_ok(), "{ra:?} / {rb:?}");
        let refused = ra.err().or(rb.err()).unwrap();
        assert!(matches!(refused, StoreError::Deadlock(_)), "{refused:?}");
    }
}

/// Every open-question view agrees with the questions table.
fn assert_views_agree(s: &Store, addrs_: &[&str]) {
    let mut table_open: Vec<String> = Vec::new();
    for addr in addrs_ {
        let to_me: Vec<String> = s
            .open_questions_to(addr)
            .unwrap()
            .into_iter()
            .map(|q| q.id)
            .collect();
        let (_, open_count) = s.unread_counts(addr).unwrap();
        assert_eq!(open_count, to_me.len(), "unread_counts for {addr}");
        let inbox = s.inbox(addr, true).unwrap();
        let first: Vec<String> = inbox
            .iter()
            .take(to_me.len())
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(first, to_me, "inbox head for {addr}");
        for id in to_me {
            if !table_open.contains(&id) {
                table_open.push(id);
            }
        }
    }
    let threads = s.threads(&Default::default()).unwrap();
    let mut listed = 0;
    for t in &threads {
        let in_thread = s
            .thread(&t.thread_id)
            .unwrap()
            .into_iter()
            .filter(|m| {
                s.question(&m.id)
                    .unwrap()
                    .is_some_and(|q| q.answered_by.is_none())
            })
            .count();
        assert_eq!(t.open_questions, in_thread, "thread {}", t.thread_id);
        listed += t.open_questions;
    }
    assert!(listed >= table_open.len());
}

#[test]
fn open_question_views_agree_after_each_closing_scenario() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let all = [a.as_str(), b.as_str(), c.as_str(), HUMAN];
    let q1 = ask(&s, &a, &[&b, &c]);
    assert_views_agree(&s, &all);
    let cl = s
        .insert_message(reply(&b, &[&a], &q1.id, "which?"), 2)
        .unwrap();
    assert_views_agree(&s, &all);
    let r = s
        .insert_message(reply(&a, &[&b], &cl.id, "this"), 3)
        .unwrap();
    s.insert_message(answer(&b, &[&a], &r.id), 4).unwrap();
    assert_views_agree(&s, &all);
    let q2 = ask(&s, &c, &[&b]);
    let q3 = s
        .insert_message(
            NewMessage {
                kind: Kind::Question,
                ..reply(&c, &[&b], &q2.id, "and?")
            },
            5,
        )
        .unwrap();
    assert_views_agree(&s, &all);
    s.insert_message(answer(HUMAN, &[&c], &q3.id), 6).unwrap();
    assert_views_agree(&s, &all);
    assert!(s.open_questions_to(&b).unwrap().is_empty());
    s.prune(100).unwrap();
    assert_views_agree(&s, &all);
}

#[test]
fn asker_cannot_answer_own_question() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    let err = s.insert_message(answer(&a, &[&b], &q.id), 2).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert_eq!(s.question(&q.id).unwrap().unwrap().answered_by, None);
}

#[test]
fn inbox_orders_open_questions_first() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let n1 = s.insert_message(note(&a, &[&b], "N1", "x"), 1).unwrap();
    let q = ask(&s, &a, &[&b]);
    let n2 = s.insert_message(note(&a, &[&b], "N2", "x"), 3).unwrap();
    let inbox: Vec<String> = s
        .inbox(&b, false)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(inbox, vec![q.id.clone(), n1.id.clone(), n2.id.clone()]);
    s.insert_message(answer(&b, &[&a], &q.id), 4).unwrap();
    let inbox: Vec<String> = s
        .inbox(&b, false)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(inbox, vec![n1.id, q.id, n2.id]);
}

#[test]
fn answer_to_clarification_closes_question() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    let c = s
        .insert_message(reply(&b, &[&a], &q.id, "which endpoint?"), 2)
        .unwrap();
    let r = s
        .insert_message(reply(&a, &[&b], &c.id, "POST /orders"), 3)
        .unwrap();
    s.insert_message(answer(&b, &[&a], &r.id), 4).unwrap();
    assert_eq!(
        s.question(&q.id).unwrap().unwrap().answered_by,
        Some(b.clone())
    );
    assert!(s.open_questions_to(&b).unwrap().is_empty());
}

#[test]
fn non_recipient_answer_does_not_close() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let c = agent(&s, "docs", None);
    let q = ask(&s, &a, &[&b]);
    // c is cc'd later but was never asked.
    s.insert_message(reply(&a, &[&b, &c], &q.id, "cc docs"), 2)
        .unwrap();
    let err = s.insert_message(answer(&c, &[&a], &q.id), 3).unwrap_err();
    assert_eq!(err, StoreError::InvalidInput("not an open question".into()));
    assert_eq!(s.question(&q.id).unwrap().unwrap().answered_by, None);
    assert_eq!(qids(&s.open_questions_to(&b).unwrap()), vec![q.id.as_str()]);
    assert!(s.open_questions_to(&c).unwrap().is_empty());
    // human may answer any question.
    s.insert_message(answer(HUMAN, &[&a], &q.id), 4).unwrap();
    assert_eq!(
        s.question(&q.id).unwrap().unwrap().answered_by,
        Some(HUMAN.to_string())
    );
    assert!(s.open_questions_to(&b).unwrap().is_empty());
}

#[test]
fn answer_closes_every_question_on_its_chain() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q1 = ask(&s, &a, &[&b]);
    let q2 = s
        .insert_message(
            NewMessage {
                kind: Kind::Question,
                ..reply(&a, &[&b], &q1.id, "and also?")
            },
            2,
        )
        .unwrap();
    assert_eq!(s.open_questions_to(&b).unwrap().len(), 2);
    s.insert_message(answer(&b, &[&a], &q2.id), 3).unwrap();
    assert!(s.open_questions_to(&b).unwrap().is_empty());
    assert_eq!(
        s.question(&q1.id).unwrap().unwrap().answered_by,
        Some(b.clone())
    );
    assert_eq!(s.question(&q2.id).unwrap().unwrap().answered_by, Some(b));
}

#[test]
fn released_held_question_is_not_duplicated() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let b = agent(&s, "frontend", Some("/repo"));
    let q = s
        .insert_held(
            question(&a, &[&b], "Q"),
            1,
            repo,
            &addrs(&["docs"]),
            NOT_ALIVE,
        )
        .unwrap();
    assert!(s.question(&q.id).unwrap().is_some());
    let d = agent(&s, "docs", Some("/repo"));
    s.release_held(repo, "docs", &d).unwrap();
    let row = s.question(&q.id).unwrap().unwrap();
    assert_eq!(row.to, addrs(&[&b, &d]));
    assert_eq!(qids(&s.open_questions_to(&d).unwrap()), vec![q.id.as_str()]);
    assert_eq!(qids(&s.open_questions_to(&b).unwrap()), vec![q.id.as_str()]);
    // Exactly one row: the asker's single question is the only edge.
    let hit = s.would_deadlock(&d, &addrs(&[&a])).unwrap();
    assert_eq!(hit.map(|h| h.id), Some(q.id.clone()));
    // The released recipient may answer, and that closes the one row.
    s.insert_message(answer(&d, &[&a], &q.id), 2).unwrap();
    assert!(s.open_questions_to(&b).unwrap().is_empty());
    assert!(s.open_questions_to(&d).unwrap().is_empty());
}

#[test]
fn held_question_for_no_one_yet_is_open() {
    let (_d, s) = store();
    let repo = Path::new("/repo");
    let a = agent(&s, "backend", Some("/repo"));
    let q = s
        .insert_held(
            question(&a, &[], "Q"),
            1,
            repo,
            &addrs(&["docs"]),
            NOT_ALIVE,
        )
        .unwrap();
    let row = s.question(&q.id).unwrap().unwrap();
    assert!(row.to.is_empty());
    assert_eq!(row.answered_by, None);
}

#[test]
fn prune_removes_question_rows() {
    let (_d, s) = store();
    let a = agent(&s, "backend", None);
    let b = agent(&s, "frontend", None);
    let q = ask(&s, &a, &[&b]);
    assert_eq!(s.prune(100).unwrap(), 1);
    assert_eq!(s.question(&q.id).unwrap(), None);
    assert!(s.open_questions_to(&b).unwrap().is_empty());
    assert_eq!(s.would_deadlock(&b, &addrs(&[&a])).unwrap(), None);
}

#[test]
fn questions_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("agentmail.redb");
    let (a, b, q) = {
        let s = Store::open(&path).unwrap();
        let a = agent(&s, "backend", None);
        let b = agent(&s, "frontend", None);
        let q = ask(&s, &a, &[&b]);
        (a, b, q)
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(qids(&s.open_questions_to(&b).unwrap()), vec![q.id.as_str()]);
    assert!(s.would_deadlock(&b, &addrs(&[&a])).unwrap().is_some());
}
