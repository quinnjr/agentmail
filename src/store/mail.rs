//! Messages: insert, mailboxes and read state, threads, held mail, search
//! and prune.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use redb::{ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::questions::is_open;
use super::{
    AGENTS, Alive, HELD, MAILBOX, MESSAGES, OPEN_QUESTIONS, READ, RegisterReq, Registered,
    SURFACED, Store, StoreError, THREADS, URGENT, all_json, encode, get_json,
};
use crate::ids::{new_message_id, validate_name};
use crate::model::{Agent, HUMAN, Kind, MAX_BODY_BYTES, Message, Presence};

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
///
/// Missing fields deserialize to their defaults, so a client may send a
/// partial filter such as `{}` or `{"agent":"x"}`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
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
pub(crate) fn lexical(path: &Path) -> PathBuf {
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
/// This is the one closing rule, applied once when the answer is stored
/// (by `store::questions`, which records it in the questions table that
/// every open-question view reads): an `answer` closes every question on
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

/// A thread's messages, in id order.
pub(super) fn thread_messages(
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

/// `addr`'s mailbox rows: `(message id, flags)`, in id order.
fn mailbox_rows(
    mailbox: &impl ReadableTable<(&'static str, &'static str), u8>,
    addr: &str,
) -> Result<Vec<(String, u8)>, StoreError> {
    let mut out = Vec::new();
    for row in mailbox.range((addr, "")..)? {
        let (key, flags) = row?;
        let (a, id) = key.value();
        if a != addr {
            break;
        }
        out.push((id.to_string(), flags.value()));
    }
    Ok(out)
}

/// The [`URGENT`] mailbox flag for `msg`: set when it is urgent.
fn urgent_flag(msg: &Message) -> u8 {
    if msg.urgent { URGENT } else { 0 }
}

/// `addr`'s urgent mail that is neither read nor surfaced, in id order.
/// Filters on the mailbox flags, so it decodes only the matching messages.
fn unsurfaced_urgent_in(
    mailbox: &impl ReadableTable<(&'static str, &'static str), u8>,
    messages: &impl ReadableTable<&'static str, &'static [u8]>,
    addr: &str,
) -> Result<Vec<Message>, StoreError> {
    let mut out = Vec::new();
    for (id, flags) in mailbox_rows(mailbox, addr)? {
        if flags & (READ | SURFACED | URGENT) != URGENT {
            continue;
        }
        if let Some(msg) = get_json::<Message>(messages, &id)? {
            out.push(msg);
        }
    }
    Ok(out)
}

/// What a harness hook shows `addr`, read from one snapshot of `mailbox`,
/// `messages` and `index` (see [`Store::hook_snapshot`]). Decodes only the
/// unread, unsurfaced urgent messages (found from the [`URGENT`] mailbox
/// flag, without decoding the rest) and the open questions in `addr`'s
/// mailbox.
fn hook_snapshot_in(
    mailbox: &impl ReadableTable<(&'static str, &'static str), u8>,
    messages: &impl ReadableTable<&'static str, &'static [u8]>,
    index: &impl ReadableTable<&'static str, ()>,
    addr: &str,
) -> Result<HookSnapshot, StoreError> {
    let unread = mailbox_rows(mailbox, addr)?
        .iter()
        .filter(|(_, flags)| flags & READ == 0)
        .count();
    let mut open_questions = Vec::new();
    for row in index.iter()? {
        let (id, _) = row?;
        let id = id.value();
        // A mailbox key lookup first: questions to others cost no decode.
        if mailbox.get((addr, id))?.is_none() {
            continue;
        }
        if let Some(msg) = get_json::<Message>(messages, id)?
            && msg.kind == Kind::Question
            && msg.to.iter().any(|a| a == addr)
        {
            open_questions.push(msg);
        }
    }
    let urgent = unsurfaced_urgent_in(mailbox, messages, addr)?;
    Ok(HookSnapshot {
        unread,
        open_questions,
        urgent,
    })
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

/// Append `addr` unless `list` already has it. Returns whether it appended.
fn push_unique(list: &mut Vec<String>, addr: &str) -> bool {
    if list.iter().any(|a| a == addr) {
        false
    } else {
        list.push(addr.to_string());
        true
    }
}

/// The repo an agent is in, as a held-mail key: its repo, or its cwd when it
/// has none. `None` when that path is not absolute (register does not
/// require one), so it can never match a held key.
pub(crate) fn agent_repo_key(agent: &Agent) -> Option<String> {
    repo_key(agent.repo.as_deref().unwrap_or(&agent.cwd)).ok()
}

/// What a harness hook shows an agent, all from one store snapshot: so
/// `unread` always counts every message in `urgent`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookSnapshot {
    /// Messages in the mailbox not yet read.
    pub unread: usize,
    /// Open questions addressed to the agent (read or not), in id order.
    pub open_questions: Vec<Message>,
    /// Unread urgent mail not yet surfaced, oldest first.
    pub urgent: Vec<Message>,
}

/// A mailbox listing for `addr`: its messages paired with their mailbox
/// flags, plus which of them are open questions addressed to `addr`.
struct MailboxView {
    entries: Vec<(Message, u8)>,
    open: HashSet<String>,
}

impl Store {
    /// Store a message and deliver it to each recipient's mailbox.
    ///
    /// A reply (`in_reply_to` set) joins the parent's thread and takes
    /// `Re: <root subject>`; a new thread needs a non-blank subject. An
    /// [`Kind::Answer`] must be a reply that closes at least one open
    /// question on its reply chain (`InvalidInput("not an open question")`
    /// otherwise). A [`Kind::Question`] whose recipients already wait,
    /// directly or through others, on its sender is refused with
    /// [`StoreError::Deadlock`] (see [`Store::would_deadlock`]); the check
    /// runs in the same transaction as the insert. `to`
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
    ///
    /// The caller resolves names before calling, so an agent may register
    /// under one of `names` in between. To keep that agent from missing its
    /// mail, the transaction re-checks: if an agent that is active under
    /// `alive` (see [`Store::presence`]) is named one of `names` in `repo`
    /// (its repo, or its cwd when it has none), nothing is stored and the
    /// result is [`StoreError::RecipientRegistered`]; the caller resolves
    /// again. Gone agents with the name do not count, since resolution holds
    /// mail for them too.
    ///
    /// A question is refused with [`StoreError::Deadlock`] when it would
    /// close a wait cycle through its resolved recipients or through any
    /// existing agent (of any presence) named one of `names` in `repo`,
    /// since such an agent can receive the held question later.
    pub fn insert_held(
        &self,
        m: NewMessage,
        now: i64,
        repo: &Path,
        names: &[String],
        alive: Alive,
    ) -> Result<Message, StoreError> {
        let repo = repo_key(repo)?;
        if names.is_empty() {
            return Err(StoreError::InvalidInput("no names to hold mail for".into()));
        }
        for name in names {
            validate_name(name).map_err(StoreError::InvalidName)?;
        }
        let txn = self.db.begin_write()?;
        let agents: Vec<Agent> = all_json(&txn.open_table(AGENTS)?)?;
        if let Some(agent) = agents.iter().find(|a| {
            names.contains(&a.name)
                && Store::presence(a, alive) == Presence::Active
                && agent_repo_key(a).as_deref() == Some(repo.as_str())
        }) {
            return Err(StoreError::RecipientRegistered(format!(
                "{} is now active as {:?} in {repo}",
                agent.name, agent.address
            )));
        }
        if m.kind == Kind::Question {
            // An existing agent named one of `names`, of any presence, may
            // take the held question later (a gone one by reclaiming its
            // address), so its open questions count as edges too. A newly
            // registered agent asks nothing yet, so cannot close a cycle.
            let mut to = m.to.clone();
            for a in &agents {
                if names.contains(&a.name)
                    && a.address != m.from
                    && agent_repo_key(a).as_deref() == Some(repo.as_str())
                {
                    push_unique(&mut to, &a.address);
                }
            }
            self.refuse_deadlock(&txn, &m.from, &to)?;
        }
        let msg = self.insert_in_txn(&txn, m, now)?;
        for name in names {
            hold_in(&txn, &repo, name, &msg.id)?;
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
            if m.kind == Kind::Question {
                // Checked in this write transaction, so two agents asking
                // each other at once cannot both pass.
                self.refuse_deadlock(txn, &m.from, &to)?;
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
            let urgent = urgent_flag(&msg);
            for addr in &msg.to {
                mailbox.insert((addr.as_str(), msg.id.as_str()), urgent)?;
            }
            if human_joins {
                mailbox.insert((HUMAN, msg.id.as_str()), READ | urgent)?;
            }
            msg
        };
        match msg.kind {
            Kind::Question => self.on_question_inserted(txn, &msg)?,
            Kind::Answer => self.on_answer_inserted(txn, &msg)?,
            Kind::Note => {}
        }
        Ok(msg)
    }

    /// Hold message `msg_id` for the first agent that registers as `name` in
    /// `repo` (see [`Store::release_held`]). `repo` must be absolute. New
    /// mail should use [`Store::insert_held`], which stores and holds in one
    /// transaction and refuses when an agent with the name is already
    /// active; this method does neither check.
    pub fn hold_for_name(&self, repo: &Path, name: &str, msg_id: &str) -> Result<(), StoreError> {
        let repo = repo_key(repo)?;
        validate_name(name).map_err(StoreError::InvalidName)?;
        let txn = self.db.begin_write()?;
        if txn.open_table(MESSAGES)?.get(msg_id)?.is_none() {
            return Err(missing_message(msg_id));
        }
        hold_in(&txn, &repo, name, msg_id)?;
        txn.commit()?;
        Ok(())
    }

    /// Deliver every message held for `name` in `repo` to `addr`: each is
    /// added to `addr`'s mailbox (unread), gets `addr` appended to its `to`,
    /// and stops being held. Returns the newly delivered messages in id
    /// order. A held message that `addr` already had (already in its `to`
    /// and its mailbox) stops being held but is not returned, so callers
    /// never announce it twice.
    ///
    /// The agent at `addr` must exist ([`StoreError::NotFound`]) and be
    /// named `name` in `repo` (its repo, or its cwd when it has none; a
    /// non-absolute stored path never matches); otherwise
    /// [`StoreError::InvalidInput`]. `repo` must be absolute.
    pub fn release_held(
        &self,
        repo: &Path,
        name: &str,
        addr: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let txn = self.db.begin_write()?;
        let released = self.release_held_in(&txn, repo, name, addr)?;
        txn.commit()?;
        Ok(released)
    }

    /// Register (see [`Store::register`]) and release the mail held for the
    /// agent's name in its repo (its cwd when it has none; see
    /// [`Store::release_held`]) in one transaction, so a registration is
    /// never committed without its held mail. Nothing is released when that
    /// path is not absolute, since held mail is only kept for absolute repos.
    ///
    /// Returns the registration and the released messages.
    pub fn register_and_release(
        &self,
        req: RegisterReq,
        now: i64,
        alive: Alive,
    ) -> Result<(Registered, Vec<Message>), StoreError> {
        let txn = self.db.begin_write()?;
        let registered = self.register_in(&txn, req, now, alive)?;
        let agent = &registered.agent;
        let released = if agent_repo_key(agent).is_some() {
            let home = agent.repo.as_deref().unwrap_or(&agent.cwd);
            self.release_held_in(&txn, home, &agent.name, &agent.address)?
        } else {
            Vec::new()
        };
        txn.commit()?;
        Ok((registered, released))
    }

    /// [`Store::release_held`] inside the caller's write transaction, which
    /// the caller commits.
    fn release_held_in(
        &self,
        txn: &WriteTransaction,
        repo: &Path,
        name: &str,
        addr: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let repo = repo_key(repo)?;
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
            // A non-absolute stored path (register does not require one)
            // can never match `repo`; report it as a mismatch, not as a
            // fault in the caller's `repo`.
            if agent_repo_key(&agent).as_deref() != Some(repo.as_str()) {
                return Err(StoreError::InvalidInput(format!(
                    "agent {addr:?} is in {}, not {repo}",
                    agent.repo.as_deref().unwrap_or(&agent.cwd).display()
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
            // (message, whether addr was newly appended to its `to`)
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
                let appended = push_unique(&mut msg.to, addr);
                if appended {
                    messages.insert(id.as_str(), encode(&msg)?.as_slice())?;
                }
                let boxed = mailbox.get((addr, id.as_str()))?.is_none();
                if boxed {
                    mailbox.insert((addr, id.as_str()), urgent_flag(&msg))?;
                }
                if appended || boxed {
                    released.push((msg, appended));
                }
            }
            released
        };
        for (msg, _) in released
            .iter()
            .filter(|(m, appended)| *appended && m.kind == Kind::Question)
        {
            self.on_question_recipient_added(txn, msg, addr)?;
        }
        Ok(released.into_iter().map(|(m, _)| m).collect())
    }

    fn mailbox_view(&self, addr: &str) -> Result<MailboxView, StoreError> {
        let txn = self.read()?;
        let mailbox = txn.open_table(MAILBOX)?;
        let messages = txn.open_table(MESSAGES)?;
        let index = txn.open_table(OPEN_QUESTIONS)?;
        let mut entries = Vec::new();
        let mut open = HashSet::new();
        for (id, flags) in mailbox_rows(&mailbox, addr)? {
            let Some(msg): Option<Message> = get_json(&messages, &id)? else {
                continue;
            };
            if msg.kind == Kind::Question
                && msg.to.iter().any(|a| a == addr)
                && is_open(&index, &msg.id)?
            {
                open.insert(msg.id.clone());
            }
            entries.push((msg, flags));
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
                    .filter(|(_, flags)| include_read || flags & READ == 0)
                    .map(|(m, _)| m),
            )
            .collect())
    }

    /// `addr`'s unread messages, in id order. Unlike [`Store::inbox`], a
    /// read open question is left out: only mail `addr` has not read yet.
    pub fn unread(&self, addr: &str) -> Result<Vec<Message>, StoreError> {
        let MailboxView { entries, .. } = self.mailbox_view(addr)?;
        Ok(entries
            .into_iter()
            .filter(|(_, flags)| flags & READ == 0)
            .map(|(m, _)| m)
            .collect())
    }

    /// `addr`'s urgent mail that is neither read nor yet surfaced (see
    /// [`Store::mark_surfaced`]), oldest first.
    pub fn unsurfaced_urgent(&self, addr: &str) -> Result<Vec<Message>, StoreError> {
        let txn = self.read()?;
        unsurfaced_urgent_in(&txn.open_table(MAILBOX)?, &txn.open_table(MESSAGES)?, addr)
    }

    /// `addr`'s [`HookSnapshot`], read in one transaction. With `surface`,
    /// the urgent mail it returns is also marked surfaced in that same
    /// (write) transaction, so of concurrent callers only one gets each
    /// message; no write transaction is taken when there is none.
    pub fn hook_snapshot(&self, addr: &str, surface: bool) -> Result<HookSnapshot, StoreError> {
        let snap = {
            let txn = self.read()?;
            hook_snapshot_in(
                &txn.open_table(MAILBOX)?,
                &txn.open_table(MESSAGES)?,
                &txn.open_table(OPEN_QUESTIONS)?,
                addr,
            )?
        };
        if !surface || snap.urgent.is_empty() {
            return Ok(snap);
        }
        let txn = self.db.begin_write()?;
        let snap = {
            let mut mailbox = txn.open_table(MAILBOX)?;
            let snap = hook_snapshot_in(
                &mailbox,
                &txn.open_table(MESSAGES)?,
                &txn.open_table(OPEN_QUESTIONS)?,
                addr,
            )?;
            for m in &snap.urgent {
                let key = (addr, m.id.as_str());
                let flags = mailbox.get(key)?.map_or(0, |f| f.value());
                mailbox.insert(key, flags | SURFACED)?;
            }
            snap
        };
        txn.commit()?;
        Ok(snap)
    }

    /// `(unread messages, open questions addressed to addr)`.
    pub fn unread_counts(&self, addr: &str) -> Result<(usize, usize), StoreError> {
        let MailboxView { entries, open } = self.mailbox_view(addr)?;
        let unread = entries
            .iter()
            .filter(|(_, flags)| flags & READ == 0)
            .count();
        Ok((unread, open.len()))
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        let txn = self.read()?;
        let messages = txn.open_table(MESSAGES)?;
        get_json(&messages, id)
    }

    /// The ids of every stored message (thread ids included, as a thread's
    /// id is its root message's) ending with `suffix`, compared without
    /// regard to case. In id order.
    pub fn ids_ending_with(&self, suffix: &str) -> Result<Vec<String>, StoreError> {
        let want = suffix.to_ascii_uppercase();
        let txn = self.read()?;
        let messages = txn.open_table(MESSAGES)?;
        let mut out = Vec::new();
        for row in messages.iter()? {
            let (k, _) = row?;
            let id = k.value();
            if id.to_ascii_uppercase().ends_with(&want) {
                out.push(id.to_string());
            }
        }
        Ok(out)
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
        self.set_flag(addr, ids, READ)
    }

    /// Mark messages surfaced for `addr`: an urgent message was pushed to
    /// it mid-run, so [`Store::unsurfaced_urgent`] leaves it out from now
    /// on. Read state is unchanged. Ids not in `addr`'s mailbox are ignored.
    pub fn mark_surfaced(&self, addr: &str, ids: &[String]) -> Result<(), StoreError> {
        self.set_flag(addr, ids, SURFACED)
    }

    /// Set `flag` on `addr`'s mailbox rows for `ids`, keeping their other
    /// flags. Ids not in the mailbox are ignored.
    fn set_flag(&self, addr: &str, ids: &[String], flag: u8) -> Result<(), StoreError> {
        // Look first under a read transaction: setting a flag that is
        // already set (re-reading read mail, say) is common and must not
        // cost a durable commit or hold the writer lock.
        let missing = |flags: Option<u8>| flags.is_some_and(|f| f & flag == 0);
        let todo: Vec<&str> = {
            let txn = self.read()?;
            let mailbox = txn.open_table(MAILBOX)?;
            let mut todo = Vec::new();
            for id in ids {
                if missing(mailbox.get((addr, id.as_str()))?.map(|f| f.value())) {
                    todo.push(id.as_str());
                }
            }
            todo
        };
        if todo.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut mailbox = txn.open_table(MAILBOX)?;
            for id in todo {
                // Re-read under the writer lock: the entry may have gone
                // (pruned) or changed since the read above.
                let current = mailbox.get((addr, id))?.map(|f| f.value());
                if let Some(flags) = current
                    && flags & flag == 0
                {
                    mailbox.insert((addr, id), flags | flag)?;
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
        let index = txn.open_table(OPEN_QUESTIONS)?;
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
            let mut open_questions = 0;
            for m in msgs.iter().filter(|m| m.kind == Kind::Question) {
                if is_open(&index, &m.id)? {
                    open_questions += 1;
                }
            }
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

/// Hold message `msg_id` for `name` in `repo` (an already normalized repo
/// key) inside the caller's write transaction: the one place a held row is
/// written, shared by [`Store::hold_for_name`] and [`Store::insert_held`].
fn hold_in(txn: &WriteTransaction, repo: &str, name: &str, msg_id: &str) -> Result<(), StoreError> {
    txn.open_table(HELD)?.insert((repo, name, msg_id), ())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::model::Harness;

    fn agent(store: &Store, name: &str) -> String {
        let req = RegisterReq {
            name: name.into(),
            role: None,
            working_on: None,
            harness: Harness::Other,
            cwd: PathBuf::from("/w"),
            repo: None,
            proc_id: None,
            fresh: true,
            wake: false,
            session: None,
        };
        store.register(req, 1, &|_| false).unwrap().0.address
    }

    fn new_message(from: &str, to: &[&str], urgent: bool) -> NewMessage {
        NewMessage {
            from: from.into(),
            to: to.iter().map(|t| t.to_string()).collect(),
            subject: Some("s".into()),
            body: "b".into(),
            kind: Kind::Note,
            urgent,
            refs: Vec::new(),
            in_reply_to: None,
        }
    }

    fn flags(store: &Store, addr: &str, id: &str) -> Option<u8> {
        let txn = store.read().unwrap();
        let mailbox = txn.open_table(MAILBOX).unwrap();
        mailbox.get((addr, id)).unwrap().map(|f| f.value())
    }

    /// Urgency is recorded as the [`URGENT`] mailbox flag at delivery, both
    /// direct and on release of held mail, and [`Store::unsurfaced_urgent`]
    /// goes by that flag alone.
    #[test]
    fn delivery_records_urgency_as_a_mailbox_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = Store::open(&dir.path().join("agentmail.redb")).unwrap();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let u = s.insert_message(new_message(&a, &[&b], true), 1).unwrap();
        let n = s.insert_message(new_message(&a, &[&b], false), 2).unwrap();
        assert_eq!(flags(&s, &b, &u.id), Some(URGENT));
        assert_eq!(flags(&s, &b, &n.id), Some(0));

        // Held urgent mail gets the flag when it is released.
        let held = s
            .insert_held(
                new_message(&a, &[], true),
                3,
                Path::new("/w"),
                &["docs".to_string()],
                &|_| false,
            )
            .unwrap();
        let d = agent(&s, "docs");
        let released = s.release_held(Path::new("/w"), "docs", &d).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(flags(&s, &d, &held.id), Some(URGENT));
        let ids = |v: Vec<Message>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(s.unsurfaced_urgent(&d).unwrap()), vec![held.id.clone()]);

        // Read and surfaced keep the flag.
        s.mark_surfaced(&b, std::slice::from_ref(&u.id)).unwrap();
        s.mark_read(&b, std::slice::from_ref(&u.id)).unwrap();
        assert_eq!(flags(&s, &b, &u.id), Some(URGENT | SURFACED | READ));

        // Only the flag is consulted: with it cleared, the urgent message
        // is no longer listed.
        let n2 = s.insert_message(new_message(&a, &[&b], true), 4).unwrap();
        let txn = s.db.begin_write().unwrap();
        txn.open_table(MAILBOX)
            .unwrap()
            .insert((b.as_str(), n2.id.as_str()), 0)
            .unwrap();
        txn.commit().unwrap();
        assert!(s.unsurfaced_urgent(&b).unwrap().is_empty());
    }

    /// Marking ids that are already read (or not in the caller's mailbox)
    /// succeeds without taking the writer lock: it completes while another
    /// write transaction is open.
    #[test]
    fn mark_read_of_read_or_foreign_ids_takes_no_write_transaction() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = Store::open(&dir.path().join("agentmail.redb")).unwrap();
        let a = agent(&s, "backend");
        let b = agent(&s, "frontend");
        let m = s
            .insert_message(
                NewMessage {
                    from: a.clone(),
                    to: vec![b.clone()],
                    subject: Some("One".into()),
                    body: "1".into(),
                    kind: Kind::Note,
                    urgent: false,
                    refs: Vec::new(),
                    in_reply_to: None,
                },
                1,
            )
            .unwrap();
        s.mark_read(&b, std::slice::from_ref(&m.id)).unwrap();
        assert_eq!(s.unread_counts(&b).unwrap(), (0, 0));

        let writer = s.db.begin_write().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let again = s.mark_read(&b, std::slice::from_ref(&m.id));
                let foreign = s.mark_read(&a, &[m.id.clone(), "nope".into()]);
                let empty = s.mark_read(&b, &[]);
                let _ = tx.send((again, foreign, empty));
            });
            let got = rx.recv_timeout(Duration::from_secs(5));
            // Release the writer before asserting so a blocked call ends.
            drop(writer);
            let (again, foreign, empty) =
                got.expect("mark_read of nothing unread must not wait for the writer lock");
            assert_eq!(again, Ok(()));
            assert_eq!(foreign, Ok(()));
            assert_eq!(empty, Ok(()));
        });
        assert_eq!(s.unread_counts(&b).unwrap(), (0, 0));
    }
}
