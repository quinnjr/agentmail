//! Request dispatch: resolves the caller, runs the store operation and
//! publishes the resulting events.
//!
//! Every method here is synchronous and may block on the database; the
//! connection loop runs it inside `spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::hub::{Hub, Wait};
use crate::ids::{is_address_shape, now_ms, validate_name};
use crate::model::{Agent, AgentView, HUMAN, Kind, Message, Presence, Question};
use crate::procinfo;
use crate::protocol::{Caller, ClientError, ErrorCode, Event, Request};
use crate::store::{
    NewMessage, RegisterReq, Store, StoreError, agent_repo_key, lexical, questions_closed_by,
};

/// How stale an agent's `last_seen` may get before a request refreshes it.
pub const TOUCH_INTERVAL_MS: i64 = 1000;

/// Shared state behind every connection.
pub struct Handler {
    store: Arc<Store>,
    hub: Hub,
}

/// Map a store error to the code clients see.
pub fn store_err(e: StoreError) -> ClientError {
    let code = match &e {
        StoreError::InvalidName(_) => ErrorCode::InvalidName,
        StoreError::InvalidInput(_) | StoreError::TooLong(_) => ErrorCode::InvalidInput,
        StoreError::TooLarge => ErrorCode::TooLarge,
        StoreError::NotFound(_) => ErrorCode::NotFound,
        StoreError::UnknownRecipient(_) => ErrorCode::UnknownRecipient,
        StoreError::InvalidRecipient(_) => ErrorCode::InvalidRecipient,
        StoreError::Deadlock(q) => return deadlock(q),
        StoreError::RecipientRegistered(_) | StoreError::Db(_) => ErrorCode::Internal,
    };
    ClientError::new(code, e.to_string())
}

/// The refusal of a question that would close a wait cycle: `q` is the
/// open question addressed to the would-be asker that it should deal with
/// instead.
fn deadlock(q: &Question) -> ClientError {
    ClientError::new(
        ErrorCode::Deadlock,
        format!(
            "asking would deadlock: {} is already waiting on you for question {}. \
             Answer it first (`agentmail answer {}`), or send a clarifying reply \
             in that thread (`agentmail reply {}`)",
            q.asker, q.id, q.id, q.id
        ),
    )
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, ClientError> {
    serde_json::to_value(value).map_err(|e| ClientError::new(ErrorCode::Internal, e.to_string()))
}

fn human_has_no(what: &str) -> ClientError {
    ClientError::new(
        ErrorCode::InvalidInput,
        format!("{HUMAN} has no {what}; only registered agents do"),
    )
}

/// What a [`Caller::Procs`] caller is told when none of its processes is
/// bound to an agent.
pub const NOT_REGISTERED: &str =
    "this session is not registered: run `agentmail register <name>` (or use --as human)";

/// How many times a send resolves its recipients again when an agent
/// registers under a held name between resolving and storing.
const RESOLVE_ATTEMPTS: usize = 8;

/// The longest a single blocking call waits; longer timeouts are cut to
/// this. A week is far beyond any harness's tool-call limit.
pub const MAX_WAIT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What a blocking request turned into once its immediate part ran.
pub enum Begun {
    /// The request is answered already.
    Done(Value),
    /// Wait for events on `rx` (subscribed before anything the wait
    /// depends on was read or stored, so nothing is missed), checking
    /// `wait` with [`Handler::check`] on each relevant one, for at most
    /// `timeout`; then answer [`Handler::timed_out`].
    Wait {
        rx: broadcast::Receiver<Event>,
        wait: Wait,
        timeout: Duration,
    },
}

fn answered(question_id: &str, answer: Option<&Message>) -> Result<Value, ClientError> {
    Ok(json!({
        "status": "answered",
        "question_id": question_id,
        "answer": to_json(&answer)?,
    }))
}

/// The recipients of a send, resolved from its `to` entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Full addresses (or `human`) to deliver to now, without duplicates.
    pub addresses: Vec<String>,
    /// `(repo, name)` pairs to hold the mail for until an agent registers
    /// under `name` in `repo`, without duplicates. Every repo is the
    /// sender's.
    pub held_names: Vec<(PathBuf, String)>,
}

/// Where a plain name is looked up.
enum Scope<'a> {
    /// Machine-wide, and nowhere to hold mail (the human).
    Machine,
    /// The sender's repo (its cwd when it has none), as a held-mail key;
    /// `None` when it has no usable one, so nothing matches.
    Repo(Option<&'a str>),
}

/// What one `to` entry resolved to.
enum Target {
    Address(String),
    Held(String),
}

/// `name: working on ...` for each candidate, for an ambiguity message.
fn describe(views: &[AgentView]) -> String {
    views
        .iter()
        .map(|v| {
            let repo = v.agent.repo.as_deref().unwrap_or(&v.agent.cwd);
            match &v.agent.working_on {
                Some(w) => format!(
                    "{} in {} (working on: {w})",
                    v.agent.address,
                    repo.display()
                ),
                None => format!("{} in {}", v.agent.address, repo.display()),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn ambiguous(name: &str, views: Vec<AgentView>, among: &str) -> ClientError {
    let mut err = ClientError::new(
        ErrorCode::AmbiguousRecipient,
        format!(
            "{name:?} matches several {among}: {}. Pick one, add it as a contact \
             (`agentmail contact add <address>`) and resend to its full address",
            describe(&views)
        ),
    );
    err.candidates = views;
    err
}

fn self_send(entry: &str) -> ClientError {
    ClientError::new(
        ErrorCode::InvalidRecipient,
        format!("{entry:?} is the sender itself; a message cannot be sent to its own sender"),
    )
}

/// How long `git rev-parse` may take before the repo is treated as unknown.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How often to check whether a bounded command has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Run `cmd` and return its stdout if it exits successfully within
/// `limit`. A command still running at the deadline is killed and yields
/// `None`, as does one that cannot be started or fails.
///
/// Stdout is drained on a separate thread, so a chatty command cannot
/// block on a full pipe. The command runs in its own process group, and
/// whenever we give up on it the whole group is killed: a grandchild that
/// keeps the pipe open then dies with it, so neither we nor the reader
/// thread are held past the deadline.
fn output_within(cmd: Command, limit: Duration) -> Option<Vec<u8>> {
    output_within_with(cmd, limit, kill_group).ok()
}

/// Why [`output_within_with`] gave up on a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GiveUp {
    /// The command could not be started.
    Spawn,
    /// The command was still running at the deadline.
    TimedOut,
    /// The command exited unsuccessfully.
    Failed,
    /// The command succeeded, but something it left behind held stdout
    /// open past the deadline.
    StdoutHeld,
    /// Reading stdout failed.
    Read,
    /// `waitid` could not tell whether the leader had exited.
    LeaderUnknown,
}

/// [`output_within`] with the group kill injectable, so tests can observe
/// when it happens, and the reason for giving up reported.
///
/// The group id is the leader's pid, and a pid (hence the group id) stays
/// reserved for exactly as long as the leader is unreaped, alive or a
/// zombie. So the leader's exit is detected without reaping it
/// ([`leader_exited`]), `kill_group` is only ever called before the
/// leader is reaped, and the leader is reaped only after the last
/// possible group kill. Signalling `-pgid` therefore can never reach an
/// unrelated process that reused the id.
fn output_within_with(
    mut cmd: Command,
    limit: Duration,
    kill_group: impl Fn(u32),
) -> Result<Vec<u8>, GiveUp> {
    use std::os::unix::process::CommandExt;

    let deadline = Instant::now() + limit;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| GiveUp::Spawn)?;
    // With `process_group(0)` the group id is the child's pid.
    let pgid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        kill_group(pgid);
        let _ = child.wait();
        return Err(GiveUp::Read);
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });
    let success = loop {
        match leader_exited(pgid) {
            Ok(Some(success)) => break success,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                // Timed out: the leader is still running, so unreaped.
                kill_group(pgid);
                let _ = child.wait();
                return Err(GiveUp::TimedOut);
            }
            Err(_) => {
                // Cannot tell whether the leader is still ours to wait
                // for, so the group id may already be free: do not signal
                // the group, only the child std still tracks.
                let _ = child.kill();
                let _ = child.wait();
                return Err(GiveUp::LeaderUnknown);
            }
        }
    };
    // The leader has exited but is not reaped yet, so its zombie keeps
    // the group id reserved for any kill below.
    let out = if success {
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .max(POLL_INTERVAL);
        // A timeout means something the command left behind still holds
        // stdout open past the deadline.
        match rx.recv_timeout(wait) {
            Ok(Ok(buf)) => Ok(buf),
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => Err(GiveUp::Read),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(GiveUp::StdoutHeld),
        }
    } else {
        Err(GiveUp::Failed)
    };
    if out.is_err() {
        kill_group(pgid);
    }
    let _ = child.wait();
    out
}

/// Whether child `pid` has exited, without reaping it: `Ok(None)` while
/// it runs, `Ok(Some(success))` once it has exited, where `success` means
/// it exited with status 0. The child stays a zombie until waited for.
fn leader_exited(pid: u32) -> std::io::Result<Option<bool>> {
    let id = libc::id_t::from(pid);
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value (plain integers
        // and padding), and `waitid` only writes into `info`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, exclusively borrowed `siginfo_t`.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: after a successful `waitid` the child fields are set
        // (or left zero when, under WNOHANG, nothing has exited yet).
        let (si_pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if si_pid == 0 {
            return Ok(None);
        }
        return Ok(Some(info.si_code == libc::CLD_EXITED && status == 0));
    }
}

/// SIGKILL every process left in group `pgid` (best effort).
///
/// Callers must only use this while the group's leader (whose pid is
/// `pgid`) is unreaped. Until it is reaped, no new process or group can
/// take that id, so this only ever hits the command and what it spawned.
/// Once the leader is reaped the id may be reused, even if other members
/// are still alive, and signalling it could kill an unrelated process.
fn kill_group(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    if pgid > 0 {
        // SAFETY: kill(2) takes plain integers and touches no memory.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// The git toplevel containing `cwd`, if any, or `None` when git does not
/// answer within [`GIT_TIMEOUT`]. The caller's own git environment is not
/// inherited, so only `cwd` decides; git never prompts and never takes
/// optional locks.
fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    let mut raw = output_within(cmd, GIT_TIMEOUT)?;
    while raw.last() == Some(&b'\n') || raw.last() == Some(&b'\r') {
        raw.pop();
    }
    if raw.is_empty() {
        return None;
    }
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
    path.is_absolute().then_some(path)
}

impl Handler {
    pub fn new(store: Arc<Store>, hub: Hub) -> Handler {
        Handler { store, hub }
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Who `c` is: an agent address or [`HUMAN`]. Records activity for an
    /// agent, at most once per [`TOUCH_INTERVAL_MS`]: `touch` is a durable
    /// write, and most requests (counts, inbox polls) are read-only.
    pub fn resolve_caller(&self, c: &Caller) -> Result<String, ClientError> {
        let agent = match c {
            Caller::Human => return Ok(HUMAN.to_string()),
            Caller::Address { address } => {
                let found = if address == HUMAN {
                    None
                } else {
                    self.store.get_agent(address).map_err(store_err)?
                };
                found.ok_or_else(|| {
                    ClientError::new(
                        ErrorCode::NoIdentity,
                        format!(
                            "{address:?} is not a registered address: run \
                             `agentmail register <name>` (or use --as human)"
                        ),
                    )
                })?
            }
            Caller::Procs { procs } => self
                .store
                .find_by_procs(procs)
                .map_err(store_err)?
                .ok_or_else(|| ClientError::new(ErrorCode::NoIdentity, NOT_REGISTERED))?,
        };
        let now = now_ms();
        if now.saturating_sub(agent.last_seen) >= TOUCH_INTERVAL_MS {
            // Best effort: a failed touch never fails the request.
            let _ = self.store.touch(&agent.address, now);
        }
        Ok(agent.address)
    }

    /// Resolve the `to` entries of a send from `sender` (an address or
    /// [`HUMAN`]). Per entry, in order:
    ///
    /// 1. `human`, or the address of an existing agent: used as is.
    /// 2. Anything else shaped like a full address (`<name>-<8 id chars>`):
    ///    [`ErrorCode::UnknownRecipient`]. A plain name of that shape cannot
    ///    be addressed by name.
    /// 3. A plain name (anything else that is not a valid name is
    ///    [`ErrorCode::UnknownRecipient`]): the sender's contact with that
    ///    name, if exactly one, whatever its repo or presence; several are
    ///    [`ErrorCode::AmbiguousRecipient`]. Otherwise the active agents
    ///    with that name in the sender's repo (its cwd when it has none;
    ///    machine-wide for `human`), other than the sender: one is used,
    ///    several are [`ErrorCode::AmbiguousRecipient`] with the candidates
    ///    and their working-on summaries. With none the mail is held for
    ///    the name in the sender's repo; `human`, having no repo, gets
    ///    [`ErrorCode::UnknownRecipient`].
    /// 4. A recipient that is the sender: [`ErrorCode::InvalidRecipient`].
    pub fn resolve_recipients(&self, sender: &str, to: &[String]) -> Result<Resolved, ClientError> {
        if to.is_empty() {
            return Err(ClientError::new(ErrorCode::InvalidInput, "no recipients"));
        }
        let agents = self.store.list_agents().map_err(store_err)?;
        let by_address: HashMap<&str, &Agent> =
            agents.iter().map(|a| (a.address.as_str(), a)).collect();
        let contacts: Vec<&Agent> = self
            .store
            .contacts(sender)
            .map_err(store_err)?
            .iter()
            .filter_map(|c| by_address.get(c.as_str()).copied())
            .collect();
        let home = if sender == HUMAN {
            None
        } else {
            by_address.get(sender).copied().and_then(agent_repo_key)
        };
        let scope = if sender == HUMAN {
            Scope::Machine
        } else {
            Scope::Repo(home.as_deref())
        };

        let mut out = Resolved::default();
        for entry in to {
            match self.resolve_one(sender, entry, &agents, &by_address, &contacts, &scope)? {
                Target::Address(address) => {
                    if address == sender {
                        return Err(self_send(entry));
                    }
                    if !out.addresses.contains(&address) {
                        out.addresses.push(address);
                    }
                }
                Target::Held(name) => {
                    let Scope::Repo(Some(repo)) = scope else {
                        unreachable!("mail is only held in a repo");
                    };
                    let held = (PathBuf::from(repo), name);
                    if !out.held_names.contains(&held) {
                        out.held_names.push(held);
                    }
                }
            }
        }
        Ok(out)
    }

    /// One step of [`Handler::resolve_recipients`].
    fn resolve_one(
        &self,
        sender: &str,
        entry: &str,
        agents: &[Agent],
        by_address: &HashMap<&str, &Agent>,
        contacts: &[&Agent],
        scope: &Scope,
    ) -> Result<Target, ClientError> {
        if entry == HUMAN || (entry.contains('-') && by_address.contains_key(entry)) {
            return Ok(Target::Address(entry.to_string()));
        }
        if is_address_shape(entry) {
            return Err(ClientError::new(
                ErrorCode::UnknownRecipient,
                format!(
                    "unknown recipient {entry:?}: no agent has that address (see `agentmail agents --all`)"
                ),
            ));
        }
        if validate_name(entry).is_err() {
            return Err(ClientError::new(
                ErrorCode::UnknownRecipient,
                format!(
                    "unknown recipient {entry:?}: neither a full <name>-<id> address \
                     nor a valid agent name"
                ),
            ));
        }

        let named: Vec<&Agent> = contacts
            .iter()
            .copied()
            .filter(|a| a.name == entry)
            .collect();
        match named.as_slice() {
            [one] => return Ok(Target::Address(one.address.clone())),
            [] => {}
            several => {
                let views = several
                    .iter()
                    .map(|a| self.view((*a).clone(), true))
                    .collect();
                return Err(ambiguous(entry, views, "of your contacts"));
            }
        }

        let in_scope = |a: &Agent| match scope {
            Scope::Machine => true,
            Scope::Repo(Some(home)) => agent_repo_key(a).as_deref() == Some(*home),
            Scope::Repo(None) => false,
        };
        let mut is_sender = false;
        let mut matches = Vec::new();
        for a in agents.iter().filter(|a| a.name == entry && in_scope(a)) {
            let view = self.view(a.clone(), false);
            if view.presence != Presence::Active {
                continue;
            }
            if a.address == sender {
                is_sender = true;
            } else {
                matches.push(view);
            }
        }
        match matches.len() {
            1 => {
                let view = matches.pop().expect("one match");
                return Ok(Target::Address(view.agent.address));
            }
            0 => {}
            _ => {
                let among = match scope {
                    Scope::Machine => "active agents",
                    Scope::Repo(_) => "active agents in your repo",
                };
                return Err(ambiguous(entry, matches, among));
            }
        }
        if is_sender {
            return Err(self_send(entry));
        }
        match scope {
            Scope::Repo(Some(_)) => Ok(Target::Held(entry.to_string())),
            Scope::Machine | Scope::Repo(None) => Err(ClientError::new(
                ErrorCode::UnknownRecipient,
                format!(
                    "unknown recipient {entry:?}: no active agent has that name \
                     (see `agentmail agents --all`)"
                ),
            )),
        }
    }

    /// Store a new message from `me` to the resolved `to` entries, holding it
    /// for names nobody active has yet, and announce the delivery. Returns
    /// the message with the names it is held for.
    ///
    /// A question that would close a wait cycle is refused with
    /// [`ErrorCode::Deadlock`] before anything is written (the store checks
    /// again when it writes, against a concurrent question).
    fn deliver(
        &self,
        me: &str,
        to: &[String],
        mut new: NewMessage,
    ) -> Result<(Message, Vec<String>), ClientError> {
        for _ in 0..RESOLVE_ATTEMPTS {
            let resolved = self.resolve_recipients(me, to)?;
            if new.kind == Kind::Question
                && let Some(q) = self
                    .store
                    .would_deadlock(me, &resolved.addresses)
                    .map_err(store_err)?
            {
                return Err(deadlock(&q));
            }
            let Some((repo, _)) = resolved.held_names.first() else {
                new.to = resolved.addresses;
                let msg = self
                    .store
                    .insert_message(new, now_ms())
                    .map_err(store_err)?;
                self.publish_delivered(&msg);
                return Ok((msg, Vec::new()));
            };
            debug_assert!(resolved.held_names.iter().all(|(r, _)| r == repo));
            let names: Vec<String> = resolved.held_names.iter().map(|(_, n)| n.clone()).collect();
            // Kept for a retry: on a conflict the store hands nothing back.
            let mut m = new.clone();
            m.to = resolved.addresses;
            match self
                .store
                .insert_held(m, now_ms(), repo, &names, &procinfo::is_alive)
            {
                Ok(msg) => {
                    self.publish_delivered(&msg);
                    return Ok((msg, names));
                }
                // An agent registered under a held name meanwhile: resolve
                // again so it gets the mail directly.
                Err(StoreError::RecipientRegistered(_)) => continue,
                Err(e) => return Err(store_err(e)),
            }
        }
        Err(ClientError::new(
            ErrorCode::Internal,
            "recipients kept registering while the message was being sent; try again",
        ))
    }

    /// Reply to message `parent_id` from `me`: to the parent's sender and
    /// recipients other than `me`, in that order, as a note or (with
    /// `answer`) an answer that must close an open question on the reply
    /// chain. The store enforces who may reply in the thread.
    fn reply(
        &self,
        me: String,
        parent_id: String,
        body: String,
        urgent: bool,
        refs: Vec<String>,
        answer: bool,
    ) -> Result<Value, ClientError> {
        let parent = self
            .store
            .get_message(&parent_id)
            .map_err(store_err)?
            .ok_or_else(|| {
                ClientError::new(ErrorCode::NotFound, format!("no message {parent_id:?}"))
            })?;
        let mut to: Vec<String> = Vec::new();
        for addr in std::iter::once(&parent.from).chain(&parent.to) {
            if *addr != me && !to.contains(addr) {
                to.push(addr.clone());
            }
        }
        if to.is_empty() {
            return Err(ClientError::new(
                ErrorCode::InvalidRecipient,
                format!("message {parent_id:?} has no one besides you to reply to"),
            ));
        }
        let new = NewMessage {
            from: me,
            to,
            subject: None,
            body,
            kind: if answer { Kind::Answer } else { Kind::Note },
            urgent,
            refs,
            in_reply_to: Some(parent_id),
        };
        let msg = self
            .store
            .insert_message(new, now_ms())
            .map_err(store_err)?;
        self.publish_delivered(&msg);
        to_json(&msg)
    }

    /// Whether anyone among `to` can answer now: the human, or an active
    /// agent.
    fn anyone_listening(&self, to: &[String]) -> Result<bool, ClientError> {
        for addr in to {
            if addr == HUMAN {
                return Ok(true);
            }
            if let Some(agent) = self.store.get_agent(addr).map_err(store_err)?
                && Store::presence(&agent, &procinfo::is_alive) == Presence::Active
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The first message in `thread` that answers (closes) question `id`.
    fn answer_in<'a>(thread: &'a [Message], id: &str) -> Option<&'a Message> {
        thread.iter().find(|m| {
            questions_closed_by(m, thread)
                .iter()
                .any(|closed| closed.id == id)
        })
    }

    /// Run the immediate part of a blocking request (`Ask`, `AwaitAnswer`,
    /// `WaitForMail`): either its answer, or what to wait for. Any other
    /// request is an internal error.
    pub fn begin(&self, caller: &Caller, req: Request) -> Result<Begun, ClientError> {
        match req {
            Request::Ask {
                to,
                subject,
                body,
                refs,
                urgent,
                timeout_secs,
            } => {
                let me = self.resolve_caller(caller)?;
                // Subscribe before storing, so no reply can slip by.
                let rx = self.hub.subscribe();
                let new = NewMessage {
                    from: me.clone(),
                    to: Vec::new(),
                    subject: Some(subject),
                    body,
                    kind: Kind::Question,
                    urgent,
                    refs,
                    in_reply_to: None,
                };
                let (msg, _held) = self.deliver(&me, &to, new)?;
                // Names the mail is held for have no one to answer yet.
                if !self.anyone_listening(&msg.to)? {
                    return Ok(Begun::Done(json!({
                        "status": "no_one_listening",
                        "question_id": msg.id,
                    })));
                }
                Ok(Begun::Wait {
                    rx,
                    wait: Wait::Answer {
                        question_id: msg.id.clone(),
                        thread_id: msg.thread_id,
                        asker: me,
                        seen: HashSet::from([msg.id]),
                    },
                    timeout: Duration::from_secs(timeout_secs).min(MAX_WAIT),
                })
            }
            Request::AwaitAnswer {
                question_id,
                timeout_secs,
            } => {
                let me = self.resolve_caller(caller)?;
                let rx = self.hub.subscribe();
                // The thread before the question row: see `check_answer`.
                let q = self.question(&question_id)?;
                if q.asker != me {
                    return Err(ClientError::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "question {question_id:?} was asked by {}; only its asker                              waits for the answer",
                            q.asker
                        ),
                    ));
                }
                let thread = self.store.thread(&q.thread_id).map_err(store_err)?;
                let q = self.question(&question_id)?;
                if q.answered_by.is_some() {
                    let thread = self.store.thread(&q.thread_id).map_err(store_err)?;
                    return answered(&q.id, Self::answer_in(&thread, &q.id)).map(Begun::Done);
                }
                Ok(Begun::Wait {
                    rx,
                    wait: Wait::Answer {
                        question_id: q.id,
                        thread_id: q.thread_id,
                        asker: me,
                        seen: thread.into_iter().map(|m| m.id).collect(),
                    },
                    timeout: Duration::from_secs(timeout_secs).min(MAX_WAIT),
                })
            }
            Request::WaitForMail { timeout_secs } => {
                let me = self.resolve_caller(caller)?;
                let rx = self.hub.subscribe();
                let mut wait = Wait::Mail { addr: me };
                if let Some(done) = self.check(&mut wait)? {
                    return Ok(Begun::Done(done));
                }
                Ok(Begun::Wait {
                    rx,
                    wait,
                    timeout: Duration::from_secs(timeout_secs).min(MAX_WAIT),
                })
            }
            _ => Err(ClientError::new(
                ErrorCode::Internal,
                "not a blocking request",
            )),
        }
    }

    /// Question `id`, or [`ErrorCode::NotFound`] when there is no such
    /// question.
    fn question(&self, id: &str) -> Result<Question, ClientError> {
        self.store
            .question(id)
            .map_err(store_err)?
            .ok_or_else(|| ClientError::new(ErrorCode::NotFound, format!("no question {id:?}")))
    }

    /// Whether `wait` is over, judged from the store alone: its answer if
    /// so, `None` to keep waiting. Updates what the wait has seen.
    pub fn check(&self, wait: &mut Wait) -> Result<Option<Value>, ClientError> {
        match wait {
            Wait::Answer {
                question_id,
                thread_id,
                asker,
                seen,
            } => {
                // Read the thread before the question row. An answer and
                // the question's closing are stored together, so an answer
                // in this snapshot always shows as closed below and is
                // never taken for a clarification.
                let thread = self.store.thread(thread_id).map_err(store_err)?;
                let q = self.question(question_id)?;
                if q.answered_by.is_some() {
                    let thread = self.store.thread(thread_id).map_err(store_err)?;
                    return answered(question_id, Self::answer_in(&thread, question_id)).map(Some);
                }
                for m in thread {
                    if !seen.insert(m.id.clone()) {
                        continue;
                    }
                    if m.from != *asker && m.to.iter().any(|a| a == asker) {
                        return Ok(Some(json!({
                            "status": "clarification",
                            "question_id": question_id,
                            "message": to_json(&m)?,
                        })));
                    }
                }
                Ok(None)
            }
            Wait::Mail { addr } => {
                let unread = self.store.unread(addr).map_err(store_err)?;
                if unread.is_empty() {
                    return Ok(None);
                }
                Ok(Some(
                    json!({ "status": "mail", "messages": to_json(&unread)? }),
                ))
            }
        }
    }

    /// The answer to a wait that ran out of time.
    pub fn timed_out(wait: &Wait) -> Value {
        match wait {
            Wait::Answer { question_id, .. } => {
                json!({ "status": "pending", "question_id": question_id })
            }
            Wait::Mail { .. } => json!({ "status": "timeout" }),
        }
    }

    fn view(&self, agent: Agent, is_contact: bool) -> AgentView {
        AgentView {
            presence: Store::presence(&agent, &procinfo::is_alive),
            is_contact,
            agent,
        }
    }

    fn contact_set(&self, owner: &str) -> Result<HashSet<String>, ClientError> {
        Ok(self
            .store
            .contacts(owner)
            .map_err(store_err)?
            .into_iter()
            .collect())
    }

    fn agent_view(&self, address: &str) -> Result<AgentView, ClientError> {
        let agent = self
            .store
            .get_agent(address)
            .map_err(store_err)?
            .ok_or_else(|| {
                ClientError::new(ErrorCode::NotFound, format!("no agent {address:?}"))
            })?;
        Ok(self.view(agent, false))
    }

    fn publish_presence(&self, view: &AgentView) {
        self.hub.publish(Event::Presence {
            agent: view.clone(),
        });
    }

    fn publish_delivered(&self, msg: &Message) {
        for to in &msg.to {
            self.hub.publish(Event::Delivered {
                to: to.clone(),
                message: msg.clone(),
            });
        }
    }

    /// Run one non-streaming request. `Hello`, `Shutdown` and `Watch` belong
    /// to the connection loop.
    pub fn handle(&self, caller: &Caller, req: Request) -> Result<Value, ClientError> {
        match req {
            Request::Register {
                name,
                role,
                working_on,
                harness,
                cwd,
                proc_id,
                fresh,
                wake,
            } => self.register(RegisterReq {
                name,
                role,
                working_on,
                harness,
                repo: None,
                cwd,
                proc_id,
                fresh,
                wake,
            }),
            Request::Whoami => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Ok(json!({ "address": HUMAN, "agent": null }));
                }
                let view = self.agent_view(&me)?;
                Ok(json!({ "address": me, "agent": to_json(&view)? }))
            }
            Request::Status { working_on } => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Err(human_has_no("working-on summary"));
                }
                self.store.set_status(&me, &working_on).map_err(store_err)?;
                let view = self.agent_view(&me)?;
                self.publish_presence(&view);
                to_json(&view)
            }
            Request::Signoff => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Err(human_has_no("session to sign off"));
                }
                self.store.signoff(&me).map_err(store_err)?;
                let view = self.agent_view(&me)?;
                self.publish_presence(&view);
                to_json(&view)
            }
            Request::Agents {
                name_prefix,
                repo,
                all,
            } => self.agents(caller, name_prefix, repo, all),
            Request::ContactAdd { address } => {
                let me = self.resolve_caller(caller)?;
                self.store.add_contact(&me, &address).map_err(store_err)?;
                Ok(Value::Null)
            }
            Request::ContactRemove { address } => {
                let me = self.resolve_caller(caller)?;
                self.store
                    .remove_contact(&me, &address)
                    .map_err(store_err)?;
                Ok(Value::Null)
            }
            Request::Contacts => {
                let me = self.resolve_caller(caller)?;
                let mut views = Vec::new();
                for address in self.store.contacts(&me).map_err(store_err)? {
                    if let Some(agent) = self.store.get_agent(&address).map_err(store_err)? {
                        // Every listed agent is a contact.
                        views.push(self.view(agent, true));
                    }
                }
                to_json(&views)
            }
            Request::Send {
                to,
                subject,
                body,
                urgent,
                refs,
            } => {
                let me = self.resolve_caller(caller)?;
                let new = NewMessage {
                    from: me.clone(),
                    to: Vec::new(),
                    subject: Some(subject),
                    body,
                    kind: Kind::Note,
                    urgent,
                    refs,
                    in_reply_to: None,
                };
                let (msg, held_for) = self.deliver(&me, &to, new)?;
                let mut out = to_json(&msg)?;
                out["held_for"] = to_json(&held_for)?;
                Ok(out)
            }
            Request::Inbox { all } => {
                let me = self.resolve_caller(caller)?;
                let contacts = self.contact_set(&me)?;
                let mut out = Vec::new();
                for msg in self.store.inbox(&me, all).map_err(store_err)? {
                    let from_contact = contacts.contains(&msg.from);
                    let mut v = to_json(&msg)?;
                    v["from_contact"] = Value::Bool(from_contact);
                    out.push(v);
                }
                Ok(Value::Array(out))
            }
            Request::Counts => {
                let me = self.resolve_caller(caller)?;
                let (unread, open_questions) = self.store.unread_counts(&me).map_err(store_err)?;
                Ok(json!({ "unread": unread, "open_questions": open_questions }))
            }
            Request::Read { id } => self.read(caller, &id),
            Request::Threads { filter } => {
                self.resolve_caller(caller)?;
                to_json(&self.store.threads(&filter).map_err(store_err)?)
            }
            Request::Search { query } => {
                self.resolve_caller(caller)?;
                to_json(&self.store.search(&query).map_err(store_err)?)
            }
            Request::Prune { older_than_secs } => {
                // Mail is kept unless the human prunes it.
                if self.resolve_caller(caller)? != HUMAN {
                    return Err(ClientError::new(
                        ErrorCode::InvalidInput,
                        format!("only {HUMAN} may prune mail (use --as human)"),
                    ));
                }
                let age_ms =
                    i64::try_from(older_than_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
                let before = now_ms().saturating_sub(age_ms);
                let pruned = self.store.prune(before).map_err(store_err)?;
                Ok(json!({ "pruned": pruned }))
            }
            Request::Reply {
                message_id,
                body,
                urgent,
                refs,
                answer,
            } => {
                let me = self.resolve_caller(caller)?;
                self.reply(me, message_id, body, urgent, refs, answer)
            }
            Request::Ask { .. }
            | Request::AwaitAnswer { .. }
            | Request::WaitForMail { .. }
            | Request::Hello { .. }
            | Request::Watch { .. }
            | Request::Shutdown => Err(ClientError::new(
                ErrorCode::Internal,
                "handled by the connection loop",
            )),
        }
    }

    fn register(&self, mut req: RegisterReq) -> Result<Value, ClientError> {
        if !req.cwd.is_absolute() {
            return Err(ClientError::new(
                ErrorCode::InvalidInput,
                format!("cwd {} is not an absolute path", req.cwd.display()),
            ));
        }
        req.cwd = lexical(&req.cwd);
        req.repo = git_toplevel(&req.cwd);
        // One transaction: an error never follows a committed registration,
        // so a retry cannot mint an address the caller never learned.
        let (agent, reclaimed, released) = self
            .store
            .register_and_release(req, now_ms(), &procinfo::is_alive)
            .map_err(store_err)?;
        let view = self.view(agent, false);
        self.publish_presence(&view);
        for msg in &released {
            self.hub.publish(Event::Delivered {
                to: view.agent.address.clone(),
                message: msg.clone(),
            });
        }
        Ok(json!({
            "address": view.agent.address,
            "reclaimed": reclaimed,
            "released": released.len(),
            "agent": to_json(&view)?,
        }))
    }

    fn agents(
        &self,
        caller: &Caller,
        name_prefix: Option<String>,
        repo: Option<PathBuf>,
        all: bool,
    ) -> Result<Value, ClientError> {
        let me = self.resolve_caller(caller)?;
        let repo = match repo {
            Some(r) if !r.is_absolute() => {
                return Err(ClientError::new(
                    ErrorCode::InvalidInput,
                    format!("repo {} is not an absolute path", r.display()),
                ));
            }
            r => r.map(|r| lexical(&r)),
        };
        let contacts = self.contact_set(&me)?;
        let views: Vec<AgentView> = self
            .store
            .list_agents()
            .map_err(store_err)?
            .into_iter()
            .filter(|a| name_prefix.as_deref().is_none_or(|p| a.name.starts_with(p)))
            .filter(|a| {
                repo.as_deref().is_none_or(|r| {
                    a.repo
                        .as_deref()
                        .is_some_and(|ar| lexical(ar).starts_with(r))
                        || lexical(&a.cwd).starts_with(r)
                })
            })
            .map(|a| {
                let is_contact = contacts.contains(&a.address);
                self.view(a, is_contact)
            })
            .filter(|v| all || v.presence == crate::model::Presence::Active)
            .collect();
        to_json(&views)
    }

    fn read(&self, caller: &Caller, id: &str) -> Result<Value, ClientError> {
        let me = self.resolve_caller(caller)?;
        let msg = self
            .store
            .get_message(id)
            .map_err(store_err)?
            .ok_or_else(|| ClientError::new(ErrorCode::NotFound, format!("no message {id:?}")))?;
        let mut out = to_json(&msg)?;
        if msg.thread_id == msg.id {
            let thread = self.store.thread(id).map_err(store_err)?;
            let ids: Vec<String> = thread.iter().map(|m| m.id.clone()).collect();
            self.store.mark_read(&me, &ids).map_err(store_err)?;
            out["thread"] = to_json(&thread)?;
        } else {
            self.store
                .mark_read(&me, std::slice::from_ref(&msg.id))
                .map_err(store_err)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_returns_stdout_of_a_quick_command() {
        let mut cmd = Command::new("printf");
        cmd.arg("/repo\n");
        assert_eq!(
            output_within(cmd, Duration::from_secs(5)),
            Some(b"/repo\n".to_vec())
        );
    }

    /// Limits to try, in turn, for a case that needs the command to get
    /// somewhere (exit, or record a pid) before the deadline. Under heavy
    /// load a shell can take longer than a short limit just to start; that
    /// run then times out without exercising the case, and is retried with
    /// a longer limit.
    const GROWING_LIMITS: [Duration; 4] = [
        Duration::from_millis(300),
        Duration::from_secs(1),
        Duration::from_secs(3),
        Duration::from_secs(10),
    ];

    /// Assert the process `pid` dies soon, failing with `context` and its
    /// `/proc` status otherwise.
    fn assert_dies(pid: u32, context: &str) {
        let stat = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        // Dead already: a zombie awaiting its (new) parent (`Z`), or a
        // process its parent is reaping (`X`, EXIT_DEAD). Under heavy load
        // the reaper (init, for an orphan) can leave a reaped process in
        // `X` for seconds before its `/proc` entry goes: stress runs showed
        // such a grandchild, killed by SIGKILL (exit code 9) and still in
        // the killed group, outlast a check that only accepted `Z`.
        let gone = |pid: u32| match stat(pid) {
            None => true,
            Some(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with(['Z', 'X', 'x'])),
        };
        // SIGKILL is not instant either: a process in uninterruptible sleep
        // only dies once it leaves it, which on a heavily loaded machine
        // (WSL in particular) can take seconds. The wait is generous, as
        // only a process that was never signalled should fail this.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !gone(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            gone(pid),
            "{context}: process {pid} survived (/proc/{pid}/stat: {:?})",
            stat(pid)
        );
    }

    /// Run `script` (with `$1` a file to write a grandchild's pid to) and
    /// assert it gives up for `why` and the grandchild dies.
    fn assert_grandchild_killed(script: &str, why: GiveUp) {
        for limit in GROWING_LIMITS {
            let dir = tempfile::TempDir::new().unwrap();
            let pidfile = dir.path().join("grandchild.pid");
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(script).arg("sh").arg(&pidfile);
            let started = Instant::now();
            let kills = std::sync::Mutex::new(Vec::new());
            let out = output_within_with(cmd, limit, |pgid| {
                kills.lock().unwrap().push(pgid);
                kill_group(pgid);
            });
            let kills = kills.into_inner().unwrap();
            assert!(started.elapsed() < limit + Duration::from_secs(10));
            // Whatever happened, a grandchild that was started must die.
            let pid = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if let Some(pid) = pid {
                assert_dies(
                    pid,
                    &format!(
                        "grandchild of {script:?} outlived output_within \
                         (limit {limit:?}, gave up with {out:?}, group kills {kills:?})"
                    ),
                );
            }
            if out == Err(why) && pid.is_some() {
                return;
            }
            // Only a run that was too slow to get anywhere is retried.
            assert_eq!(
                out,
                Err(GiveUp::TimedOut),
                "{script:?} gave up for the wrong reason"
            );
        }
        panic!("{script:?} never gave up for {why:?}, even with a long limit");
    }

    /// Whenever the command is given up on, its whole process group is
    /// killed, so a grandchild holding stdout open dies too and the reader
    /// thread can finish.
    #[test]
    fn bounded_output_kills_grandchildren_when_giving_up() {
        // The command itself outlives the deadline.
        assert_grandchild_killed("sleep 30 & echo $! > \"$1\"; wait", GiveUp::TimedOut);
        // The command succeeds, but a grandchild keeps stdout open.
        assert_grandchild_killed("sleep 30 & echo $! > \"$1\"; exit 0", GiveUp::StdoutHeld);
        // The command fails, leaving a grandchild behind.
        assert_grandchild_killed("sleep 30 & echo $! > \"$1\"; exit 1", GiveUp::Failed);
    }

    /// Run `cmd` with a kill hook that records, for each group kill,
    /// whether the leader was still unreaped (alive or a zombie) at that
    /// moment, then performs the real kill. Returns the outcome and those
    /// records.
    fn run_with_kill_probe(cmd: Command, limit: Duration) -> (Result<Vec<u8>, GiveUp>, Vec<bool>) {
        let seen = std::sync::Mutex::new(Vec::new());
        let out = output_within_with(cmd, limit, |pgid| {
            let pid = libc::pid_t::try_from(pgid).unwrap();
            // SAFETY: kill(2) with signal 0 only checks that `pid` exists;
            // an exited but unreaped child still does.
            let unreaped = unsafe { libc::kill(pid, 0) } == 0;
            seen.lock().unwrap().push(unreaped);
            kill_group(pgid);
        });
        (out, seen.into_inner().unwrap())
    }

    /// The group is only ever killed while its leader is unreaped: the
    /// zombie leader keeps the group id reserved, so the kill cannot hit
    /// an unrelated process that reused it.
    #[test]
    fn bounded_output_kills_the_group_only_before_reaping_the_leader() {
        let sh = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(script);
            cmd
        };
        // Exits non-zero with nothing left in the group.
        let (out, kills) = run_with_kill_probe(sh("exit 3"), Duration::from_secs(5));
        assert_eq!(out, Err(GiveUp::Failed));
        assert_eq!(kills, vec![true]);
        // Outlives the deadline.
        let (out, kills) = run_with_kill_probe(sh("exec sleep 30"), Duration::from_millis(200));
        assert_eq!(out, Err(GiveUp::TimedOut));
        assert_eq!(kills, vec![true]);
        // Succeeds, but a grandchild holds stdout open past the deadline.
        // A run too slow to even exit before the deadline times out
        // instead; retry it with a longer limit.
        let mut held = false;
        for limit in GROWING_LIMITS {
            let (out, kills) = run_with_kill_probe(sh("sleep 30 & exit 0"), limit);
            assert_eq!(kills, vec![true]);
            if out == Err(GiveUp::StdoutHeld) {
                held = true;
                break;
            }
            assert_eq!(out, Err(GiveUp::TimedOut));
        }
        assert!(held, "the grandchild case never got past the deadline");
        // Succeeds cleanly: nothing to kill.
        let (out, kills) = run_with_kill_probe(sh("echo hi"), Duration::from_secs(5));
        assert_eq!(out, Ok(b"hi\n".to_vec()));
        assert!(kills.is_empty());
    }

    /// The leader is always reaped before `output_within` returns.
    #[test]
    fn bounded_output_reaps_the_leader() {
        let gone = |pid: libc::pid_t, script: &str| {
            // SAFETY: kill(2) with signal 0 only checks that `pid` exists;
            // an unreaped zombie still would.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            assert!(!exists, "leader of {script:?} left unreaped");
        };
        // Paths that give up learn the leader's pid from the kill hook.
        for (script, limit) in [
            ("exit 3", Duration::from_secs(5)),
            ("exec sleep 30", Duration::from_millis(200)),
            ("sleep 30 & exit 0", Duration::from_millis(300)),
        ] {
            let leader = std::sync::Mutex::new(None);
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(script);
            let out = output_within_with(cmd, limit, |pgid| {
                *leader.lock().unwrap() = Some(pgid);
                kill_group(pgid);
            });
            assert!(out.is_err(), "{script:?} did not give up");
            let pgid = leader.into_inner().unwrap().expect("group was killed");
            gone(libc::pid_t::try_from(pgid).unwrap(), script);
        }
        // A clean success reports its own pid on stdout.
        let script = "echo $$";
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let out = output_within_with(cmd, Duration::from_secs(5), |_| {
            panic!("a clean success killed its group")
        })
        .unwrap();
        let pid = String::from_utf8(out).unwrap().trim().parse().unwrap();
        gone(pid, script);
    }

    #[test]
    fn bounded_output_is_none_on_failure() {
        assert_eq!(
            output_within(Command::new("false"), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            output_within(
                Command::new("/nonexistent/agentmail-test"),
                Duration::from_secs(5)
            ),
            None
        );
    }

    #[test]
    fn bounded_output_kills_a_hung_command() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        assert_eq!(output_within(cmd, Duration::from_millis(200)), None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn git_toplevel_finds_the_repo_and_none_outside_one() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(git_toplevel(dir.path()), None);
        let ok = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let top = git_toplevel(&sub).unwrap();
        assert_eq!(
            top.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }
}
