//! Questions: the open/answered record of every [`Kind::Question`], answer
//! closing, and the wait graph behind the deadlock guard.
//!
//! An open question from A to B is an edge "A waits on B" until it is
//! answered, whether or not A is still blocked on it.

use std::collections::{HashMap, HashSet, VecDeque};

use redb::{ReadableTable, WriteTransaction};

use super::mail::{questions_closed_by, thread_messages};
use super::{
    MESSAGES, OPEN_QUESTIONS, QUESTIONS, Store, StoreError, THREADS, all_json, encode, get_json,
};
use crate::model::{Kind, Message, Question};

/// The error for an answer that closes no open question.
fn not_open() -> StoreError {
    StoreError::InvalidInput("not an open question".into())
}

/// Every open question, in id order. Reads only the rows the
/// [`OPEN_QUESTIONS`] index names, so answered history costs nothing.
fn open_questions(
    index: &impl ReadableTable<&'static str, ()>,
    questions: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<Question>, StoreError> {
    let mut out = Vec::new();
    for row in index.iter()? {
        let (id, _) = row?;
        if let Some(q) = get_json::<Question>(questions, id.value())?
            && q.answered_by.is_none()
        {
            out.push(q);
        }
    }
    Ok(out)
}

/// Whether question `id` is open, by the [`OPEN_QUESTIONS`] index. The
/// one source of open-question state for inbox, counts and thread
/// listings, as for the deadlock guard.
pub(super) fn is_open(
    index: &impl ReadableTable<&'static str, ()>,
    id: &str,
) -> Result<bool, StoreError> {
    Ok(index.get(id)?.is_some())
}

/// Recreate [`OPEN_QUESTIONS`] from the rows of [`QUESTIONS`], so the index
/// is right even for a database written before it existed. Run by
/// [`Store::open`].
pub(super) fn rebuild_open_index(txn: &WriteTransaction) -> Result<(), StoreError> {
    let all: Vec<Question> = all_json(&txn.open_table(QUESTIONS)?)?;
    let mut index = txn.open_table(OPEN_QUESTIONS)?;
    index.retain(|_, _| false)?;
    for q in all.iter().filter(|q| q.answered_by.is_none()) {
        index.insert(q.id.as_str(), ())?;
    }
    Ok(())
}

/// The search behind [`Store::would_deadlock`], over the open questions in
/// `open`.
fn find_cycle(open: &[Question], asker: &str, to: &[String]) -> Option<Question> {
    let mut edges: HashMap<&str, Vec<(&str, &Question)>> = HashMap::new();
    for q in open {
        for t in &q.to {
            edges
                .entry(q.asker.as_str())
                .or_default()
                .push((t.as_str(), q));
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for t in to {
        if t != asker && seen.insert(t.as_str()) {
            queue.push_back(t.as_str());
        }
    }
    while let Some(node) = queue.pop_front() {
        for &(next, q) in edges.get(node).into_iter().flatten() {
            if next == asker {
                return Some(q.clone());
            }
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    None
}

/// Store question row `q` and keep [`OPEN_QUESTIONS`] in step with it.
fn put_question(txn: &WriteTransaction, q: &Question) -> Result<(), StoreError> {
    txn.open_table(QUESTIONS)?
        .insert(q.id.as_str(), encode(q)?.as_slice())?;
    let mut index = txn.open_table(OPEN_QUESTIONS)?;
    if q.answered_by.is_none() {
        index.insert(q.id.as_str(), ())?;
    } else {
        index.remove(q.id.as_str())?;
    }
    Ok(())
}

impl Store {
    /// The question asked by message `id`, open or answered; `None` when
    /// `id` is not a question.
    pub fn question(&self, id: &str) -> Result<Option<Question>, StoreError> {
        let txn = self.read()?;
        get_json(&txn.open_table(QUESTIONS)?, id)
    }

    /// Open questions with `addr` among their recipients, in id order.
    pub fn open_questions_to(&self, addr: &str) -> Result<Vec<Question>, StoreError> {
        let txn = self.read()?;
        let open = open_questions(
            &txn.open_table(OPEN_QUESTIONS)?,
            &txn.open_table(QUESTIONS)?,
        )?;
        Ok(open
            .into_iter()
            .filter(|q| q.to.iter().any(|a| a == addr))
            .collect())
    }

    /// Whether `asker` asking `to` would close a wait cycle.
    ///
    /// Each open question is an edge from its asker to each of its
    /// recipients. Searching breadth-first from every address in `to`, if
    /// some path of edges reaches `asker`, returns the question on the last
    /// edge of a shortest such path: an open question addressed to `asker`
    /// that it should answer (or reply to in that thread) instead of asking.
    /// `None` when no recipient waits, directly or through others, on
    /// `asker`.
    ///
    /// This is a read-only query for building a helpful message. Storing a
    /// question ([`Store::insert_message`], [`Store::insert_held`]) runs the
    /// same search inside its write transaction and refuses with
    /// [`StoreError::Deadlock`], so two agents asking each other at once can
    /// never both succeed; callers need not serialize check and insert.
    pub fn would_deadlock(
        &self,
        asker: &str,
        to: &[String],
    ) -> Result<Option<Question>, StoreError> {
        let txn = self.read()?;
        let open = open_questions(
            &txn.open_table(OPEN_QUESTIONS)?,
            &txn.open_table(QUESTIONS)?,
        )?;
        Ok(find_cycle(&open, asker, to))
    }

    /// Called inside the write transaction before question `asker -> to` is
    /// stored: [`StoreError::Deadlock`] when it would close a wait cycle
    /// (see [`Store::would_deadlock`]), which aborts the transaction.
    pub(super) fn refuse_deadlock(
        &self,
        txn: &WriteTransaction,
        asker: &str,
        to: &[String],
    ) -> Result<(), StoreError> {
        let open = open_questions(
            &txn.open_table(OPEN_QUESTIONS)?,
            &txn.open_table(QUESTIONS)?,
        )?;
        match find_cycle(&open, asker, to) {
            Some(q) => Err(StoreError::Deadlock(Box::new(q))),
            None => Ok(()),
        }
    }

    /// Called inside the write transaction exactly once per question, when
    /// the message is stored (by [`Store::insert_message`] or
    /// [`Store::insert_held`]). Creates the question row, open, with the
    /// message's current recipients. Releasing held mail never calls it
    /// again; see [`Store::on_question_recipient_added`].
    pub(super) fn on_question_inserted(
        &self,
        txn: &WriteTransaction,
        msg: &Message,
    ) -> Result<(), StoreError> {
        put_question(
            txn,
            &Question {
                id: msg.id.clone(),
                thread_id: msg.thread_id.clone(),
                asker: msg.from.clone(),
                to: msg.to.clone(),
                answered_by: None,
            },
        )
    }

    /// Called inside the release transaction when held question `msg` is
    /// delivered to `addr` and `addr` was newly appended to `msg.to`. Adds
    /// `addr` to the existing row's recipients.
    ///
    /// Every stored question has a row (made by
    /// [`Store::on_question_inserted`] in the same transaction). Should one
    /// be missing anyway, e.g. in a database written before the questions
    /// table existed, it is rebuilt from `msg` (whose `to` already has
    /// `addr`), answered by the first answer in its thread that closes it,
    /// so the released recipient still gets its wait-graph edge.
    pub(super) fn on_question_recipient_added(
        &self,
        txn: &WriteTransaction,
        msg: &Message,
        addr: &str,
    ) -> Result<(), StoreError> {
        debug_assert_eq!(msg.kind, Kind::Question);
        let existing: Option<Question> = get_json(&txn.open_table(QUESTIONS)?, &msg.id)?;
        let q = match existing {
            Some(mut q) => {
                if q.to.iter().any(|a| a == addr) {
                    return Ok(());
                }
                q.to.push(addr.to_string());
                q
            }
            None => {
                let thread = thread_messages(
                    &txn.open_table(THREADS)?,
                    &txn.open_table(MESSAGES)?,
                    &msg.thread_id,
                )?;
                let answered_by = thread
                    .iter()
                    .find(|m| {
                        questions_closed_by(m, &thread)
                            .iter()
                            .any(|c| c.id == msg.id)
                    })
                    .map(|m| m.from.clone());
                let mut to = msg.to.clone();
                if !to.iter().any(|a| a == addr) {
                    to.push(addr.to_string());
                }
                Question {
                    id: msg.id.clone(),
                    thread_id: msg.thread_id.clone(),
                    asker: msg.from.clone(),
                    to,
                    answered_by,
                }
            }
        };
        put_question(txn, &q)
    }

    /// Called inside the write transaction after answer `msg` is stored:
    /// closes every open question it closes under [`questions_closed_by`]
    /// (the same rule inbox and thread listings use), setting
    /// `answered_by`. An answer that closes no open question is refused
    /// with `InvalidInput("not an open question")`, which aborts the whole
    /// transaction, so the answer is not stored either.
    pub(super) fn on_answer_inserted(
        &self,
        txn: &WriteTransaction,
        msg: &Message,
    ) -> Result<(), StoreError> {
        debug_assert_eq!(msg.kind, Kind::Answer);
        let thread = thread_messages(
            &txn.open_table(THREADS)?,
            &txn.open_table(MESSAGES)?,
            &msg.thread_id,
        )?;
        let mut closed = 0;
        for m in questions_closed_by(msg, &thread) {
            let Some(mut q): Option<Question> = get_json(&txn.open_table(QUESTIONS)?, &m.id)?
            else {
                continue;
            };
            if q.answered_by.is_some() {
                continue;
            }
            q.answered_by = Some(msg.from.clone());
            put_question(txn, &q)?;
            closed += 1;
        }
        if closed == 0 {
            return Err(not_open());
        }
        Ok(())
    }

    /// Called inside the prune transaction with the ids of the deleted
    /// messages, after every other table handle is closed. Deletes their
    /// question rows and open-index entries.
    pub(super) fn on_messages_pruned(
        &self,
        txn: &WriteTransaction,
        ids: &HashSet<String>,
    ) -> Result<(), StoreError> {
        txn.open_table(QUESTIONS)?
            .retain(|id, _| !ids.contains(id))?;
        txn.open_table(OPEN_QUESTIONS)?
            .retain(|id, _| !ids.contains(id))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::model::Harness;
    use crate::store::{NewMessage, RegisterReq, ThreadFilter};

    fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
        (dir, store)
    }

    fn agent(s: &Store, name: &str) -> String {
        let req = RegisterReq {
            name: name.into(),
            role: None,
            working_on: None,
            harness: Harness::Other,
            cwd: PathBuf::from("/repo"),
            repo: Some(PathBuf::from("/repo")),
            proc_id: None,
            fresh: true,
            wake: false,
        };
        s.register(req, 1, &|_| false).unwrap().0.address
    }

    fn msg(from: &str, to: &[&str], kind: Kind, parent: Option<&str>) -> NewMessage {
        NewMessage {
            from: from.into(),
            to: to.iter().map(|s| s.to_string()).collect(),
            subject: Some("S".into()),
            body: "b".into(),
            kind,
            urgent: false,
            refs: vec![],
            in_reply_to: parent.map(str::to_string),
        }
    }

    /// The ids in the open-question index.
    fn index_ids(s: &Store) -> Vec<String> {
        let txn = s.read().unwrap();
        let index = txn.open_table(OPEN_QUESTIONS).unwrap();
        index
            .iter()
            .unwrap()
            .map(|r| r.unwrap().0.value().to_string())
            .collect()
    }

    /// Simulate a database whose question row (and index entry) is missing.
    fn drop_row(s: &Store, id: &str) {
        let txn = s.db.begin_write().unwrap();
        txn.open_table(QUESTIONS).unwrap().remove(id).unwrap();
        txn.open_table(OPEN_QUESTIONS).unwrap().remove(id).unwrap();
        txn.commit().unwrap();
    }

    /// Hold question `from -> to` for `docs`, then register docs.
    fn held_question(s: &Store, from: &str, to: &[&str]) -> (Message, String) {
        let q = s
            .insert_held(
                msg(from, to, Kind::Question, None),
                1,
                Path::new("/repo"),
                &["docs".to_string()],
                &|_| false,
            )
            .unwrap();
        let d = agent(s, "docs");
        (q, d)
    }

    #[test]
    fn missing_row_is_rebuilt_open_on_release() {
        let (_d, s) = store();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let (q, d) = held_question(&s, &a, &[&b]);
        drop_row(&s, &q.id);
        s.release_held(Path::new("/repo"), "docs", &d).unwrap();
        assert_eq!(
            s.question(&q.id).unwrap(),
            Some(Question {
                id: q.id.clone(),
                thread_id: q.thread_id.clone(),
                asker: a.clone(),
                to: vec![b.clone(), d.clone()],
                answered_by: None,
            })
        );
        assert_eq!(s.open_questions_to(&d).unwrap().len(), 1);
        assert_eq!(index_ids(&s), vec![q.id.clone()]);
        assert!(
            s.would_deadlock(&d, std::slice::from_ref(&a))
                .unwrap()
                .is_some()
        );
        assert_eq!(s.unread_counts(&d).unwrap(), (1, 1));
    }

    #[test]
    fn missing_row_is_rebuilt_answered_on_release() {
        let (_d, s) = store();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let (q, d) = held_question(&s, &a, &[&b]);
        s.insert_message(msg(&b, &[&a], Kind::Answer, Some(&q.id)), 2)
            .unwrap();
        drop_row(&s, &q.id);
        s.release_held(Path::new("/repo"), "docs", &d).unwrap();
        let row = s.question(&q.id).unwrap().unwrap();
        assert_eq!(row.answered_by, Some(b.clone()));
        assert_eq!(row.to, vec![b, d.clone()]);
        assert!(index_ids(&s).is_empty());
        assert!(s.open_questions_to(&d).unwrap().is_empty());
    }

    #[test]
    fn open_index_tracks_insert_answer_and_prune() {
        let (_d, s) = store();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let q1 = s
            .insert_message(msg(&a, &[&b], Kind::Question, None), 1)
            .unwrap();
        let q2 = s
            .insert_message(msg(&a, &[&b], Kind::Question, None), 200)
            .unwrap();
        s.insert_message(msg(&a, &[&b], Kind::Note, None), 1)
            .unwrap();
        assert_eq!(index_ids(&s), vec![q1.id.clone(), q2.id.clone()]);
        s.insert_message(msg(&b, &[&a], Kind::Answer, Some(&q2.id)), 201)
            .unwrap();
        assert_eq!(index_ids(&s), vec![q1.id.clone()]);
        s.prune(100).unwrap();
        assert!(index_ids(&s).is_empty());
        assert!(s.question(&q2.id).unwrap().is_some());
    }

    #[test]
    fn open_index_is_rebuilt_on_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("agentmail.redb");
        let (q1, q2) = {
            let s = Store::open(&path).unwrap();
            let a = agent(&s, "backend");
            let b = agent(&s, "frontend");
            let q1 = s
                .insert_message(msg(&a, &[&b], Kind::Question, None), 1)
                .unwrap();
            let q2 = s
                .insert_message(msg(&a, &[&b], Kind::Question, None), 2)
                .unwrap();
            s.insert_message(msg(&b, &[&a], Kind::Answer, Some(&q2.id)), 3)
                .unwrap();
            // Corrupt the index: lose an open entry, gain an answered one.
            let txn = s.db.begin_write().unwrap();
            {
                let mut index = txn.open_table(OPEN_QUESTIONS).unwrap();
                index.remove(q1.id.as_str()).unwrap();
                index.insert(q2.id.as_str(), ()).unwrap();
            }
            txn.commit().unwrap();
            (q1, q2)
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(index_ids(&s), vec![q1.id]);
        assert!(s.question(&q2.id).unwrap().unwrap().answered_by.is_some());
    }

    #[test]
    fn every_view_reads_open_state_from_the_questions_table() {
        let (_d, s) = store();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let n = s
            .insert_message(msg(&a, &[&b], Kind::Note, None), 1)
            .unwrap();
        let q = s
            .insert_message(msg(&a, &[&b], Kind::Question, None), 2)
            .unwrap();
        assert_eq!(s.inbox(&b, true).unwrap()[0].id, q.id);
        // Close the question in the table only; the thread still has no
        // answer. Every view must follow the table.
        let txn = s.db.begin_write().unwrap();
        let mut row: Question = get_json(&txn.open_table(QUESTIONS).unwrap(), &q.id)
            .unwrap()
            .unwrap();
        row.answered_by = Some(b.clone());
        put_question(&txn, &row).unwrap();
        txn.commit().unwrap();
        assert_eq!(s.unread_counts(&b).unwrap(), (2, 0));
        let inbox: Vec<String> = s
            .inbox(&b, true)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(inbox, vec![n.id, q.id.clone()]);
        let open = s
            .threads(&ThreadFilter {
                open_questions: true,
                ..ThreadFilter::default()
            })
            .unwrap();
        assert!(open.is_empty());
        assert!(s.open_questions_to(&b).unwrap().is_empty());
        assert_eq!(
            s.would_deadlock(&b, std::slice::from_ref(&a)).unwrap(),
            None
        );
    }
}
