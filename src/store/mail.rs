//! Messages: insert, mailboxes and read state, threads, held mail, search
//! and prune.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use redb::{ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::{
    AGENTS, HELD, MAILBOX, MESSAGES, Store, StoreError, THREADS, all_json, encode, get_json,
};
use crate::ids::{new_message_id, validate_name};
use crate::model::{Agent, HUMAN, Kind, MAX_BODY_BYTES, Message};

/// Longest message body, in bytes (256 KiB).
pub const MAX_BODY: usize = MAX_BODY_BYTES;

/// A message to store. Recipients are already-resolved full addresses (or
/// `human`); name resolution is the caller's job. Mail for names not yet
/// registered goes through [`Store::insert_held`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewMessage {
    pub from: String,
    /// Must be non-empty for [`Store::insert_message`]; may be empty for
    /// [`Store::insert_held`].
    pub to: Vec<String>,
    /// Required on a new thread; ignored on a reply, which takes
    /// `Re: <root subject>`.
    pub subject: Option<String>,
    pub body: String,
    pub kind: Kind,
    pub urgent: bool,
    pub refs: Vec<String>,
    pub in_reply_to: Option<String>,
}

/// Which threads [`Store::threads`] returns. Every set field must match.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadFilter {
    /// Threads this address (an agent or `human`) sent or received in.
    pub agent: Option<String>,
    /// Threads with a participant whose repo (or cwd) is this path or
    /// inside it. Must be absolute; compared after lexical normalization.
    pub repo: Option<PathBuf>,
    /// Only threads with at least one open question.
    pub open_questions: bool,
}

/// One row of a thread listing.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ThreadSummary {
    pub thread_id: String,
    /// The root message's subject.
    pub subject: String,
    /// Every sender and recipient, in order of first appearance.
    pub participants: Vec<String>,
    pub messages: usize,
    /// `created_at` of the newest message.
    pub last_at: i64,
    pub open_questions: usize,
}

fn missing_message(id: &str) -> StoreError {
    StoreError::NotFound(format!("no message with id {id:?}"))
}

/// Lexical normalization only (never touches the filesystem): drop `.`,
/// repeated and trailing slashes, and resolve `..` against the previous
/// component, so every spelling of one path compares equal. `..` at the
/// root stays at the root, as the OS does.
fn lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// A caller-supplied repo path, lexically normalized: it must be absolute
/// ([`StoreError::InvalidInput`] otherwise), so `/repo`, `/repo/`,
/// `/repo/.` and `/x/../repo` are one path.
fn normalize(repo: &Path) -> Result<PathBuf, StoreError> {
    if !repo.is_absolute() {
        return Err(StoreError::InvalidInput(format!(
            "repo {} is not an absolute path",
            repo.display()
        )));
    }
    Ok(lexical(repo))
}

/// The held-mail key for a repo path: [`normalize`]d, as UTF-8.
fn repo_key(repo: &Path) -> Result<String, StoreError> {
    normalize(repo)?
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| {
            StoreError::InvalidInput(format!("repo {} is not valid UTF-8", repo.display()))
        })
}

/// `Re: <subject>`, without stacking a second `Re:`.
fn reply_subject(root: &str) -> String {
    let has_re = root.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:"));
    if has_re {
        root.to_string()
    } else {
        format!("Re: {root}")
    }
}

/// Whether `from` may answer question `q`: one of its recipients, or
/// `human`, but never the asker.
pub(crate) fn may_answer(q: &Message, from: &str) -> bool {
    from != q.from && (from == HUMAN || q.to.iter().any(|a| a == from))
}

/// The questions in `thread` that `answer` closes, nearest first.
///
/// This is the one closing rule (the questions table in
/// `store::questions` uses it too): an `answer` closes every question on
/// its reply chain (its `in_reply_to`, that message's parent, and so on up
/// to the root) that its sender [`may_answer`]. So `reply --answer` to a
/// clarification inside a question's thread still closes the question.
/// Anything other than an `answer` closes nothing.
pub(crate) fn questions_closed_by<'a>(answer: &Message, thread: &'a [Message]) -> Vec<&'a Message> {
    if answer.kind != Kind::Answer {
        return Vec::new();
    }
    let by_id: HashMap<&str, &Message> = thread.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut next = answer.in_reply_to.as_deref();
    while let Some(id) = next {
        if !seen.insert(id) {
            break;
        }
        let Some(m) = by_id.get(id) else {
            break;
        };
        if m.kind == Kind::Question && may_answer(m, &answer.from) {
            out.push(*m);
        }
        next = m.in_reply_to.as_deref();
    }
    out
}

/// The ids of the questions in `thread` that are still open: every
/// question that no `answer` in the thread closes (see
/// [`questions_closed_by`]).
fn open_questions(thread: &[Message]) -> HashSet<&str> {
    let closed: HashSet<&str> = thread
        .iter()
        .flat_map(|m| questions_closed_by(m, thread))
        .map(|q| q.id.as_str())
        .collect();
    thread
        .iter()
        .filter(|m| m.kind == Kind::Question && !closed.contains(m.id.as_str()))
        .map(|m| m.id.as_str())
        .collect()
}

/// A thread's messages, in id order.
fn thread_messages(
    threads: &impl ReadableTable<(&'static str, &'static str), ()>,
    messages: &impl ReadableTable<&'static str, &'static [u8]>,
    thread_id: &str,
) -> Result<Vec<Message>, StoreError> {
    let mut out = Vec::new();
    for row in threads.range((thread_id, "")..)? {
        let (key, _) = row?;
        let (t, id) = key.value();
        if t != thread_id {
            break;
        }
        if let Some(m) = get_json(messages, id)? {
            out.push(m);
        }
    }
    Ok(out)
}

/// Every thread's message ids, grouped by thread id.
fn all_threads(
    threads: &impl ReadableTable<(&'static str, &'static str), ()>,
) -> Result<BTreeMap<String, Vec<String>>, StoreError> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in threads.iter()? {
        let (key, _) = row?;
        let (t, id) = key.value();
        out.entry(t.to_string()).or_default().push(id.to_string());
    }
    Ok(out)
}

/// `addr`'s mailbox rows: `(message id, read)`, in id order.
fn mailbox_rows(
    mailbox: &impl ReadableTable<(&'static str, &'static str), bool>,
    addr: &str,
) -> Result<Vec<(String, bool)>, StoreError> {
    let mut out = Vec::new();
    for row in mailbox.range((addr, "")..)? {
        let (key, read) = row?;
        let (a, id) = key.value();
        if a != addr {
            break;
        }
        out.push((id.to_string(), read.value()));
    }
    Ok(out)
}

fn is_known(
    agents: &impl ReadableTable<&'static str, &'static [u8]>,
    addr: &str,
) -> Result<bool, StoreError> {
    Ok(addr == HUMAN || agents.get(addr)?.is_some())
}

/// The sender must be `human` or a registered agent.
fn require_known_sender(
    agents: &impl ReadableTable<&'static str, &'static [u8]>,
    addr: &str,
) -> Result<(), StoreError> {
    if is_known(agents, addr)? {
        Ok(())
    } else {
        Err(StoreError::NotFound(format!(
            "unknown sender {addr:?}; use a full <name>-<id> address from `agents`"
        )))
    }
}

/// A recipient must be `human` or a registered agent.
fn require_known_recipient(
    agents: &impl ReadableTable<&'static str, &'static [u8]>,
    addr: &str,
) -> Result<(), StoreError> {
    if is_known(agents, addr)? {
        Ok(())
    } else {
        Err(StoreError::UnknownRecipient(format!(
            "unknown recipient {addr:?}; use a full <name>-<id> address from `agents`"
        )))
    }
}

fn push_unique(list: &mut Vec<String>, addr: &str) {
    if !list.iter().any(|a| a == addr) {
        list.push(addr.to_string());
    }
}

/// A mailbox listing for `addr`: its messages paired with read state, plus
/// which of them are open questions addressed to `addr`.
struct MailboxView {
    entries: Vec<(Message, bool)>,
    open: HashSet<String>,
}

impl Store {
    /// Hook for the questions table (Task 5): called inside the write
    /// transaction exactly once per question, when the message is stored
    /// (by [`Store::insert_message`] or [`Store::insert_held`]). It creates
    /// the question row. Releasing held mail never calls it again; see
    /// [`Store::on_question_recipient_added`].
    fn on_question_inserted(
        &self,
        _txn: &WriteTransaction,
        _msg: &Message,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Hook for the questions table (Task 5): called inside the release
    /// transaction when held question `msg` is delivered to `addr`, after
    /// `addr` was appended to `msg.to`. It adds `addr` to the existing
    /// question row's recipients and must never create a second row.
    fn on_question_recipient_added(
        &self,
        _txn: &WriteTransaction,
        _msg: &Message,
        _addr: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Hook for the questions table (Task 5): called inside the prune
    /// transaction with the ids of the deleted messages, after every other
    /// table handle is closed, so it may open any table.
    fn on_messages_pruned(
        &self,
        _txn: &WriteTransaction,
        _ids: &HashSet<String>,
    ) -> Result<(), StoreError> {
        Ok(())
    }

    /// Store a message and deliver it to each recipient's mailbox.
    ///
    /// A reply (`in_reply_to` set) joins the parent's thread and takes
    /// `Re: <root subject>`; a new thread needs a non-blank subject. An
    /// [`Kind::Answer`] must be a reply ([`StoreError::InvalidInput`]
    /// otherwise), since only a reply can close a question. `to`
    /// must not be empty (mail for unregistered names goes through
    /// [`Store::insert_held`]). The sender must be `human` or a registered
    /// agent ([`StoreError::NotFound`] otherwise), every recipient too
    /// ([`StoreError::UnknownRecipient`]), and the sender may not be a
    /// recipient ([`StoreError::InvalidRecipient`]). Duplicate recipients are
    /// dropped. Only a participant of the parent's thread (the sender or a
    /// recipient of some message in it) may reply there
    /// ([`StoreError::InvalidInput`] otherwise); `human` may reply in any
    /// thread. When `human` replies in a thread it had no part in, the reply
    /// is also put in the human's mailbox, already read, so the thread lists
    /// under `human`.
    pub fn insert_message(&self, m: NewMessage, now: i64) -> Result<Message, StoreError> {
        if m.to.is_empty() {
            return Err(StoreError::InvalidInput("no recipients".into()));
        }
        let txn = self.db.begin_write()?;
        let msg = self.insert_in_txn(&txn, m, now)?;
        txn.commit()?;
        Ok(msg)
    }

    /// Store a message, deliver it to `m.to` (which may be empty), and hold
    /// it for each of `names` in `repo` until an agent registers under that
    /// name (see [`Store::release_held`]), all in one transaction, so held
    /// mail can never be stored without its hold. `names` must not be empty;
    /// otherwise the rules of [`Store::insert_message`] apply.
    pub fn insert_held(
        &self,
        m: NewMessage,
        now: i64,
        repo: &Path,
        names: &[String],
    ) -> Result<Message, StoreError> {
        let repo = repo_key(repo)?;
        if names.is_empty() {
            return Err(StoreError::InvalidInput("no names to hold mail for".into()));
        }
        for name in names {
            validate_name(name).map_err(StoreError::InvalidName)?;
        }
        let txn = self.db.begin_write()?;
        let msg = self.insert_in_txn(&txn, m, now)?;
        {
            let mut held = txn.open_table(HELD)?;
            for name in names {
                held.insert((repo.as_str(), name.as_str(), msg.id.as_str()), ())?;
            }
        }
        txn.commit()?;
        Ok(msg)
    }

    /// The body of [`Store::insert_message`], minus the empty-`to` check and
    /// the commit.
    fn insert_in_txn(
        &self,
        txn: &WriteTransaction,
        m: NewMessage,
        now: i64,
    ) -> Result<Message, StoreError> {
        if m.body.len() > MAX_BODY {
            return Err(StoreError::TooLarge);
        }
        if m.kind == Kind::Answer && m.in_reply_to.is_none() {
            return Err(StoreError::InvalidInput(
                "an answer must reply to a message".into(),
            ));
        }
        let mut to: Vec<String> = Vec::with_capacity(m.to.len());
        for addr in &m.to {
            push_unique(&mut to, addr);
        }
        if to.contains(&m.from) {
            return Err(StoreError::InvalidRecipient(
                "a message cannot be sent to its own sender".into(),
            ));
        }

        let msg = {
            let agents = txn.open_table(AGENTS)?;
            require_known_sender(&agents, &m.from)?;
            for addr in &to {
                require_known_recipient(&agents, addr)?;
            }

            let mut messages = txn.open_table(MESSAGES)?;
            let mut threads = txn.open_table(THREADS)?;
            let mut mailbox = txn.open_table(MAILBOX)?;
            let id = new_message_id();

            let (thread_id, subject, human_joins) = match &m.in_reply_to {
                Some(parent_id) => {
                    let parent: Message = get_json(&messages, parent_id)?
                        .ok_or_else(|| missing_message(parent_id))?;
                    let thread = thread_messages(&threads, &messages, &parent.thread_id)?;
                    let takes_part = |addr: &str| {
                        thread
                            .iter()
                            .any(|t| t.from == addr || t.to.iter().any(|a| a == addr))
                    };
                    if m.from != HUMAN && !takes_part(&m.from) {
                        return Err(StoreError::InvalidInput(format!(
                            "{} is not part of thread {}; only human can reply in a \
                             thread it was not addressed in",
                            m.from, parent.thread_id
                        )));
                    }
                    let root_subject = thread
                        .iter()
                        .find(|t| t.id == parent.thread_id)
                        .map_or(parent.subject.as_str(), |root| root.subject.as_str());
                    let human_joins = m.from == HUMAN && !takes_part(HUMAN);
                    (
                        parent.thread_id.clone(),
                        reply_subject(root_subject),
                        human_joins,
                    )
                }
                None => {
                    let subject = m
                        .subject
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| StoreError::InvalidInput("subject required".into()))?;
                    (id.clone(), subject.to_string(), false)
                }
            };

            let msg = Message {
                id,
                thread_id,
                in_reply_to: m.in_reply_to,
                from_human: m.from == HUMAN,
                from: m.from,
                to,
                subject,
                body: m.body,
                kind: m.kind,
                urgent: m.urgent,
                refs: m.refs,
                created_at: now,
            };
            messages.insert(msg.id.as_str(), encode(&msg)?.as_slice())?;
            threads.insert((msg.thread_id.as_str(), msg.id.as_str()), ())?;
            for addr in &msg.to {
                mailbox.insert((addr.as_str(), msg.id.as_str()), false)?;
            }
            if human_joins {
                mailbox.insert((HUMAN, msg.id.as_str()), true)?;
            }
            msg
        };
        if msg.kind == Kind::Question {
            self.on_question_inserted(txn, &msg)?;
        }
        Ok(msg)
    }

    /// Hold message `msg_id` for the first agent that registers as `name` in
    /// `repo` (see [`Store::release_held`]). `repo` must be absolute. New
    /// mail should use [`Store::insert_held`], which stores and holds in one
    /// transaction.
    pub fn hold_for_name(&self, repo: &Path, name: &str, msg_id: &str) -> Result<(), StoreError> {
        let repo = repo_key(repo)?;
        validate_name(name).map_err(StoreError::InvalidName)?;
        let txn = self.db.begin_write()?;
        {
            let messages = txn.open_table(MESSAGES)?;
            if messages.get(msg_id)?.is_none() {
                return Err(missing_message(msg_id));
            }
            let mut held = txn.open_table(HELD)?;
            held.insert((repo.as_str(), name, msg_id), ())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Deliver every message held for `name` in `repo` to `addr`: each is
    /// added to `addr`'s mailbox (unread), gets `addr` appended to its `to`,
    /// and stops being held. Returns the released messages in id order.
    ///
    /// The agent at `addr` must exist ([`StoreError::NotFound`]) and be
    /// named `name` in `repo` (its repo, or its cwd when it has none);
    /// otherwise [`StoreError::InvalidInput`]. `repo` must be absolute.
    pub fn release_held(
        &self,
        repo: &Path,
        name: &str,
        addr: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let repo = repo_key(repo)?;
        let txn = self.db.begin_write()?;
        let released = {
            let agents = txn.open_table(AGENTS)?;
            let agent: Agent = get_json(&agents, addr)?
                .ok_or_else(|| StoreError::NotFound(format!("no agent with address {addr:?}")))?;
            if agent.name != name {
                return Err(StoreError::InvalidInput(format!(
                    "agent {addr:?} is named {:?}, not {name:?}",
                    agent.name
                )));
            }
            let agent_repo = agent.repo.as_deref().unwrap_or(&agent.cwd);
            if repo_key(agent_repo)? != repo {
                return Err(StoreError::InvalidInput(format!(
                    "agent {addr:?} is in {}, not {repo}",
                    agent_repo.display()
                )));
            }
            let mut held = txn.open_table(HELD)?;
            let mut ids = Vec::new();
            for row in held.range((repo.as_str(), name, "")..)? {
                let (key, _) = row?;
                let (r, n, id) = key.value();
                if r != repo || n != name {
                    break;
                }
                ids.push(id.to_string());
            }

            let mut messages = txn.open_table(MESSAGES)?;
            let mut mailbox = txn.open_table(MAILBOX)?;
            let mut released = Vec::with_capacity(ids.len());
            for id in &ids {
                let Some(mut msg): Option<Message> = get_json(&messages, id)? else {
                    held.remove((repo.as_str(), name, id.as_str()))?;
                    continue;
                };
                if msg.from == addr {
                    // Never deliver mail to its own sender; it stays held
                    // for the next agent with this name.
                    continue;
                }
                held.remove((repo.as_str(), name, id.as_str()))?;
                push_unique(&mut msg.to, addr);
                messages.insert(id.as_str(), encode(&msg)?.as_slice())?;
                if mailbox.get((addr, id.as_str()))?.is_none() {
                    mailbox.insert((addr, id.as_str()), false)?;
                }
                released.push(msg);
            }
            released
        };
        for msg in released.iter().filter(|m| m.kind == Kind::Question) {
            self.on_question_recipient_added(&txn, msg, addr)?;
        }
        txn.commit()?;
        Ok(released)
    }

    fn mailbox_view(&self, addr: &str) -> Result<MailboxView, StoreError> {
        let txn = self.read()?;
        let mailbox = txn.open_table(MAILBOX)?;
        let messages = txn.open_table(MESSAGES)?;
        let threads = txn.open_table(THREADS)?;
        let mut entries = Vec::new();
        let mut open = HashSet::new();
        for (id, read) in mailbox_rows(&mailbox, addr)? {
            let Some(msg): Option<Message> = get_json(&messages, &id)? else {
                continue;
            };
            if msg.kind == Kind::Question && msg.to.iter().any(|a| a == addr) {
                let thread = thread_messages(&threads, &messages, &msg.thread_id)?;
                if open_questions(&thread).contains(msg.id.as_str()) {
                    open.insert(msg.id.clone());
                }
            }
            entries.push((msg, read));
        }
        Ok(MailboxView { entries, open })
    }

    /// `addr`'s mail: open questions addressed to `addr` first (read or not),
    /// then the rest by id. Without `include_read`, read messages other than
    /// open questions are left out.
    pub fn inbox(&self, addr: &str, include_read: bool) -> Result<Vec<Message>, StoreError> {
        let MailboxView { entries, open } = self.mailbox_view(addr)?;
        let (questions, rest): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|(m, _)| open.contains(&m.id));
        Ok(questions
            .into_iter()
            .map(|(m, _)| m)
            .chain(
                rest.into_iter()
                    .filter(|(_, read)| include_read || !read)
                    .map(|(m, _)| m),
            )
            .collect())
    }

    /// `(unread messages, open questions addressed to addr)`.
    pub fn unread_counts(&self, addr: &str) -> Result<(usize, usize), StoreError> {
        let MailboxView { entries, open } = self.mailbox_view(addr)?;
        let unread = entries.iter().filter(|(_, read)| !read).count();
        Ok((unread, open.len()))
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        let txn = self.read()?;
        let messages = txn.open_table(MESSAGES)?;
        get_json(&messages, id)
    }

    /// Every message in a thread, ordered by id; empty if there is no such
    /// thread.
    pub fn thread(&self, thread_id: &str) -> Result<Vec<Message>, StoreError> {
        let txn = self.read()?;
        let threads = txn.open_table(THREADS)?;
        let messages = txn.open_table(MESSAGES)?;
        thread_messages(&threads, &messages, thread_id)
    }

    /// Mark messages read for `addr`. Ids not in `addr`'s mailbox are
    /// ignored, so a whole thread can be marked read at once.
    pub fn mark_read(&self, addr: &str, ids: &[String]) -> Result<(), StoreError> {
        let txn = self.db.begin_write()?;
        {
            let mut mailbox = txn.open_table(MAILBOX)?;
            for id in ids {
                let unread = mailbox
                    .get((addr, id.as_str()))?
                    .is_some_and(|read| !read.value());
                if unread {
                    mailbox.insert((addr, id.as_str()), true)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Thread summaries matching `f`, most recently active first.
    pub fn threads(&self, f: &ThreadFilter) -> Result<Vec<ThreadSummary>, StoreError> {
        let txn = self.read()?;
        let threads = txn.open_table(THREADS)?;
        let messages = txn.open_table(MESSAGES)?;
        let repo_members: Option<HashSet<String>> = match &f.repo {
            None => None,
            Some(repo) => {
                let repo = normalize(repo)?;
                let agents: Vec<Agent> = all_json(&txn.open_table(AGENTS)?)?;
                Some(
                    agents
                        .into_iter()
                        .filter(|a| {
                            a.repo
                                .as_deref()
                                .is_some_and(|r| lexical(r).starts_with(&repo))
                                || lexical(&a.cwd).starts_with(&repo)
                        })
                        .map(|a| a.address)
                        .collect(),
                )
            }
        };

        let mut out = Vec::new();
        for thread_id in all_threads(&threads)?.into_keys() {
            let msgs = thread_messages(&threads, &messages, &thread_id)?;
            let Some(first) = msgs.first() else {
                continue;
            };
            let mut participants = Vec::new();
            for m in &msgs {
                push_unique(&mut participants, &m.from);
                for a in &m.to {
                    push_unique(&mut participants, a);
                }
            }
            if let Some(agent) = &f.agent
                && !participants.contains(agent)
            {
                continue;
            }
            if let Some(members) = &repo_members
                && !participants.iter().any(|p| members.contains(p))
            {
                continue;
            }
            let open_questions = open_questions(&msgs).len();
            if f.open_questions && open_questions == 0 {
                continue;
            }
            let subject = msgs
                .iter()
                .find(|m| m.id == thread_id)
                .unwrap_or(first)
                .subject
                .clone();
            out.push(ThreadSummary {
                subject,
                participants,
                messages: msgs.len(),
                last_at: msgs.iter().map(|m| m.created_at).max().unwrap_or(0),
                open_questions,
                thread_id,
            });
        }
        out.sort_by(|x, y| {
            y.last_at
                .cmp(&x.last_at)
                .then_with(|| y.thread_id.cmp(&x.thread_id))
        });
        Ok(out)
    }

    /// Messages whose subject, body or sender contains `q`, ignoring case,
    /// in id order. A blank query is refused.
    pub fn search(&self, q: &str) -> Result<Vec<Message>, StoreError> {
        if q.trim().is_empty() {
            return Err(StoreError::InvalidInput("search query is empty".into()));
        }
        let needle = q.to_lowercase();
        let txn = self.read()?;
        let all: Vec<Message> = all_json(&txn.open_table(MESSAGES)?)?;
        Ok(all
            .into_iter()
            .filter(|m| {
                [&m.subject, &m.body, &m.from]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&needle))
            })
            .collect())
    }

    /// Delete every thread whose newest message is older than `before`
    /// (Unix ms), with its mailbox and held rows. Returns how many threads
    /// were deleted.
    pub fn prune(&self, before: i64) -> Result<usize, StoreError> {
        let txn = self.db.begin_write()?;
        // Every table handle is dropped before the hook runs, so it may open
        // any table itself (redb refuses to open one table twice).
        let (doomed, pruned) = {
            let mut threads = txn.open_table(THREADS)?;
            let mut messages = txn.open_table(MESSAGES)?;
            let mut doomed: HashSet<String> = HashSet::new();
            let mut doomed_threads = Vec::new();
            for (thread_id, ids) in all_threads(&threads)? {
                let mut newest = i64::MIN;
                for id in &ids {
                    if let Some(m) = get_json::<Message>(&messages, id)? {
                        newest = newest.max(m.created_at);
                    }
                }
                if newest < before {
                    doomed.extend(ids.iter().cloned());
                    doomed_threads.push((thread_id, ids));
                }
            }
            for (thread_id, ids) in &doomed_threads {
                for id in ids {
                    threads.remove((thread_id.as_str(), id.as_str()))?;
                    messages.remove(id.as_str())?;
                }
            }
            txn.open_table(MAILBOX)?
                .retain(|(_, id), _| !doomed.contains(id))?;
            txn.open_table(HELD)?
                .retain(|(_, _, id), _| !doomed.contains(id))?;
            (doomed, doomed_threads.len())
        };
        self.on_messages_pruned(&txn, &doomed)?;
        txn.commit()?;
        Ok(pruned)
    }
}
