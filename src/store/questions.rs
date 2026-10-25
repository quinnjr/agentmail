//! Questions: the open/answered record of every [`Kind::Question`], answer
//! closing, and the wait graph behind the deadlock guard.
//!
//! An open question from A to B is an edge "A waits on B" until it is
//! answered, whether or not A is still blocked on it.

use std::collections::{HashMap, HashSet, VecDeque};

use redb::{ReadableTable, WriteTransaction};

use super::mail::{questions_closed_by, thread_messages};
use super::{MESSAGES, QUESTIONS, Store, StoreError, THREADS, all_json, encode, get_json};
use crate::model::{Kind, Message, Question};

/// The error for an answer that closes no open question.
fn not_open() -> StoreError {
    StoreError::InvalidInput("not an open question".into())
}

/// Every open question, in id order.
fn open_questions(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<Question>, StoreError> {
    let all: Vec<Question> = all_json(table)?;
    Ok(all
        .into_iter()
        .filter(|q| q.answered_by.is_none())
        .collect())
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
        let open = open_questions(&txn.open_table(QUESTIONS)?)?;
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
    pub fn would_deadlock(
        &self,
        asker: &str,
        to: &[String],
    ) -> Result<Option<Question>, StoreError> {
        let txn = self.read()?;
        let open = open_questions(&txn.open_table(QUESTIONS)?)?;
        let mut edges: HashMap<&str, Vec<(&str, &Question)>> = HashMap::new();
        for q in &open {
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
                    return Ok(Some(q.clone()));
                }
                if seen.insert(next) {
                    queue.push_back(next);
                }
            }
        }
        Ok(None)
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
        let q = Question {
            id: msg.id.clone(),
            thread_id: msg.thread_id.clone(),
            asker: msg.from.clone(),
            to: msg.to.clone(),
            answered_by: None,
        };
        txn.open_table(QUESTIONS)?
            .insert(q.id.as_str(), encode(&q)?.as_slice())?;
        Ok(())
    }

    /// Called inside the release transaction when held question `msg` is
    /// delivered to `addr` and `addr` was newly appended to `msg.to`. Adds
    /// `addr` to the existing row's recipients; never creates a row.
    pub(super) fn on_question_recipient_added(
        &self,
        txn: &WriteTransaction,
        msg: &Message,
        addr: &str,
    ) -> Result<(), StoreError> {
        let mut table = txn.open_table(QUESTIONS)?;
        let Some(mut q): Option<Question> = get_json(&table, &msg.id)? else {
            return Ok(());
        };
        if !q.to.iter().any(|a| a == addr) {
            q.to.push(addr.to_string());
            table.insert(q.id.as_str(), encode(&q)?.as_slice())?;
        }
        Ok(())
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
        let mut table = txn.open_table(QUESTIONS)?;
        let mut closed = 0;
        for m in questions_closed_by(msg, &thread) {
            let Some(mut q): Option<Question> = get_json(&table, &m.id)? else {
                continue;
            };
            if q.answered_by.is_some() {
                continue;
            }
            q.answered_by = Some(msg.from.clone());
            table.insert(q.id.as_str(), encode(&q)?.as_slice())?;
            closed += 1;
        }
        if closed == 0 {
            return Err(not_open());
        }
        Ok(())
    }

    /// Called inside the prune transaction with the ids of the deleted
    /// messages, after every other table handle is closed. Deletes their
    /// question rows.
    pub(super) fn on_messages_pruned(
        &self,
        txn: &WriteTransaction,
        ids: &HashSet<String>,
    ) -> Result<(), StoreError> {
        txn.open_table(QUESTIONS)?
            .retain(|id, _| !ids.contains(id))?;
        Ok(())
    }
}
