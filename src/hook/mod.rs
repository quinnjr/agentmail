//! `agentmail hook <event> --harness <h>`: turn the daemon's state into what
//! a harness hook prints, so the agent hears about its mail between steps,
//! on urgent mail, before it stops, and when idle.
//!
//! Hooks run on every tool call of every session on the machine, registered
//! or not, so they must be cheap and must never get in the way:
//!
//! - They never start the daemon ([`Client::connect`] only). With no daemon,
//!   or no agent for the caller, a hook prints nothing and exits 0.
//! - Every daemon call is bounded by [`CALL_LIMIT`]; a hook that cannot get
//!   an answer in time also stays silent.
//!
//! Identity: the harness session given with `--session` (opencode), else
//! the hook's process ancestry up to its harness process (Claude Code,
//! kiro). Unlike the CLI, a hook never acts as `AGENTMAIL_ADDRESS`: that
//! variable is inherited, so a harness started from a registered agent's
//! shell would otherwise act as the outer agent. A hook run as the human
//! (`AGENTMAIL_HUMAN=1`, or the CLI's `--as human`) has nothing to say.
//!
//! Entry points for the per-harness modules (Tasks 6 and 8 build on these,
//! not on the plan's sketch):
//!
//! - [`run`]`(harness, event, stdin, env)`, [`run_as`]`(.., session, ..)`
//!   and [`run_with`]`(.., &HookArgs, ..)` (the whole command line), with
//!   `env: &(dyn Fn(&str) -> Option<OsString> + Sync)` (the `Sync` lets a
//!   waiting hook run on a spawned task).
//! - [`Daemon::status`], [`Daemon::mark_busy`], [`Daemon::record_stop`]
//!   and [`Daemon::wait`]. The wait is not a `WaitForMail { timeout_secs }`
//!   loop: it watches the daemon's event stream ([`Request::Watch`]) for
//!   deliveries and the agent's sign-off, going away or turning `wake`
//!   off, and coordinates with other hooks of the same agent through files
//!   under `<data dir>/hook-wait/` (see [`Daemon::wait`]). It ends with a
//!   [`WaitOutcome`], which the harness module maps to its exit code.
//! - [`clear_wait_state`]: the daemon removes an agent's files there once
//!   it finds the agent gone (signed off, unbound, or its process dead).

pub mod claude;
pub mod kiro;
pub mod opencode;
pub mod text;

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::identity::ENV_HUMAN;
use crate::model::{Harness, Message, Presence, ProcId, SessionKey};
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{Caller, ErrorCode, Event, Request};

/// The longest a hook waits for any one daemon answer (connecting
/// included) before giving up silently.
pub const CALL_LIMIT: Duration = Duration::from_secs(5);

/// How often a waiting hook checks that the processes it ran under are
/// still alive and that its session is still idle.
const CHECK_EVERY: Duration = Duration::from_millis(500);

/// How long a waiting hook whose turn claim could not be written waits
/// for the one it replaces to let go. With its claim written it waits for
/// as long as the claim stands: the wait it replaces lets go once it sees
/// the claim, which can take a status call ([`CALL_LIMIT`]) and a tick.
const TAKEOVER_LIMIT: Duration = Duration::from_secs(5);

/// How long a waiting hook looks for its stop's decision
/// ([`Daemon::record_stop`]) before deciding by itself: the Stop hook runs
/// alongside it and may take a status call ([`CALL_LIMIT`]) to write it.
const STOP_LIMIT: Duration = Duration::from_secs(6);

/// How long before a waiting hook started its stop's record may be dated
/// and still be its own: the Stop hook and the wait hook are started side
/// by side, and the Stop hook may write before the wait hook has started.
/// A record with the same key dated earlier is an earlier stop's (one that
/// shared the key: see [`Daemon::wait`]); a continuation runs a model call
/// first, so it stops well after this.
const STOP_SLACK: Duration = Duration::from_millis(500);

/// How often a waiting hook looks for its stop's decision.
const STOP_POLL: Duration = Duration::from_millis(20);

/// The kinds of per-agent files under `<data dir>/hook-wait/`.
const STATE_KINDS: [&str; 6] = ["turn", "lock", "woken", "busy", "stop", "shown"];

/// Claude Code sets this in its hooks' environment to its own pid.
const ENV_CLAUDE_PID: &str = "CLAUDE_PID";

/// The harness event a hook runs for.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// The session started (or was cleared or compacted).
    SessionStart,
    /// The human submitted a prompt.
    Prompt,
    /// A tool call finished.
    PostTool,
    /// A tool call is about to run.
    PreTool,
    /// The agent is about to stop.
    Stop,
    /// Block until the idle agent has mail, to wake it.
    Wait,
    /// The agent's status as JSON (for plugins).
    Status,
    /// Sign the session's agent off (opencode's plugin, when opencode
    /// deletes the session). Never starts the daemon and acts only as the
    /// agent registered for `--session`, never by process ancestry.
    Signoff,
}

/// What the harness sent the hook on stdin.
#[derive(Debug, Clone, PartialEq)]
pub struct HookInput {
    /// The payload as JSON; `Null` when it was empty or not JSON.
    pub raw: Value,
    /// The harness's session id (Claude Code's `session_id`).
    pub session_id: Option<String>,
    /// Claude Code's `stop_hook_active`: this stop follows a continuation a
    /// hook caused (a Stop block, or a wake).
    pub stop_hook_active: bool,
    /// Claude Code's `agent_id`: set (non-empty) when the hook runs for a
    /// subagent's tool call, not the main agent's.
    pub agent_id: Option<String>,
    /// Claude Code's `hook_event_name` (`PostToolUse` or
    /// `PostToolUseFailure` for `post-tool`).
    pub hook_event_name: Option<String>,
    /// Claude Code's `prompt_id`: the Stop and the wait hook it starts get
    /// the same one, which (with `stop_hook_active`) ties the wait to the
    /// Stop hook's decision. It is the same for every stop until the next
    /// human prompt, a blocked stop's continuation included.
    pub prompt_id: Option<String>,
    /// opencode's plugin: the unread mail ids a timed-out `wait` did not
    /// count as news (its `known`), passed back to the wait that follows,
    /// so mail that came between the two is news; or the unread mail
    /// before a turn the plugin started with open questions. `None` when
    /// absent.
    pub known: Option<Vec<String>>,
    /// opencode's plugin: the turn that just ended was started by a wake
    /// (or failed or was interrupted), so the wait counts mail not in the
    /// `woken` file as news ([`Daemon::wait_from`]'s `after_wake`).
    pub after_wake: bool,
}

impl HookInput {
    /// Parse a payload. Anything unreadable counts as an empty one.
    ///
    /// A payload cut short (the CLI reads at most 16 MiB) is not JSON, but
    /// its `agent_id` is still looked for among the leading fields Claude
    /// Code writes before the event's own (see `agent_id_prefix`), so a
    /// subagent's huge tool result never passes for the main agent's.
    pub fn parse(stdin: &[u8]) -> HookInput {
        let raw: Value = serde_json::from_slice(stdin).unwrap_or(Value::Null);
        let str_field = |k: &str| {
            raw[k]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let agent_id = if raw.is_null() {
            agent_id_prefix(stdin)
        } else {
            str_field("agent_id")
        };
        HookInput {
            session_id: str_field("session_id"),
            stop_hook_active: raw["stop_hook_active"].as_bool().unwrap_or(false),
            agent_id,
            hook_event_name: str_field("hook_event_name"),
            prompt_id: str_field("prompt_id"),
            known: raw["known"].as_array().map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str().map(str::to_string))
                    .collect()
            }),
            after_wake: raw["after_wake"].as_bool().unwrap_or(false),
            raw,
        }
    }

    /// Whether the hook runs for a subagent rather than the agent itself.
    pub fn is_subagent(&self) -> bool {
        self.agent_id.is_some()
    }
}

/// `agent_id` from a payload that is not valid JSON (cut short), read from
/// the part before `"hook_event_name"`: Claude Code writes its base fields
/// (`session_id` .. `agent_id`, `agent_type`) first, then the event name,
/// then the event's own fields (tool input and output), so text there
/// cannot come from a tool.
fn agent_id_prefix(stdin: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&stdin[..stdin.len().min(64 * 1024)]);
    let head = &text[..text.find("\"hook_event_name\"")?];
    let rest = head.split("\"agent_id\"").nth(1)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let id = &rest[..rest.find('"')?];
    (!id.is_empty()).then(|| id.to_string())
}

/// What the hook process prints and its exit code.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HookOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit: u8,
}

/// The daemon's [`Request::HookStatus`] answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HookStatus {
    pub address: String,
    pub name: String,
    pub wake: bool,
    pub unread: usize,
    /// The unread messages' ids, in id order.
    #[serde(default)]
    pub unread_ids: Vec<String>,
    /// Open questions addressed to the agent, oldest first.
    pub open_questions: Vec<OpenQuestion>,
    /// Urgent mail neither read nor surfaced before this call.
    pub urgent: Vec<Message>,
}

/// An open question in [`HookStatus::open_questions`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenQuestion {
    pub id: String,
    pub from: String,
    pub subject: String,
}

/// How a [`Daemon::wait`] ended.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitOutcome {
    /// The agent has mail to wake up for; its status after it came.
    Mail(HookStatus),
    /// The agent signed off or is gone (its process now names another
    /// agent, or no process names it), the processes the hook runs under
    /// are gone, or the daemon went away.
    Gone,
    /// The session is not idle: the Stop hook blocked this stop, the
    /// session is busy again (a prompt or tool call since the stop), or a
    /// later stop or wait took over.
    Busy,
    /// Nothing to wait for: the agent does not want waking (or turned
    /// `wake` off while the wait ran), the wait it
    /// would replace did not let go in time (only when this wait could not
    /// claim the turn), or the waiter lock is unusable.
    Skip,
}

/// Run hook `event` for `harness`, identified by its environment and
/// process ancestry. See the module docs.
pub async fn run(
    harness: Harness,
    event: HookEvent,
    stdin: &[u8],
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> HookOutput {
    run_as(harness, event, None, stdin, env).await
}

/// [`run`], for the harness session `session` when given (`--session`).
pub async fn run_as(
    harness: Harness,
    event: HookEvent,
    session: Option<&str>,
    stdin: &[u8],
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> HookOutput {
    let args = HookArgs {
        session,
        ..HookArgs::default()
    };
    run_with(harness, event, &args, stdin, env).await
}

/// What a `hook` command line carries besides the harness and the event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HookArgs<'a> {
    /// The harness session the hook is for (`--session`; opencode's plugin).
    pub session: Option<&'a str>,
    /// `--surface-urgent`: a `status` hook marks the urgent mail it reports
    /// surfaced, so it is pushed once (opencode's plugin, after a tool call).
    pub surface_urgent: bool,
    /// `--timeout`: the longest a `wait` hook waits before reporting no
    /// mail (opencode's plugin; its default is
    /// [`opencode::DEFAULT_WAIT`]). Claude Code bounds its hooks itself.
    pub timeout: Option<Duration>,
    /// `--wake`: kiro's `stop` waits for mail while the agent is idle
    /// (installed by `install --kiro-wake`; see [`kiro`]).
    pub wake: bool,
}

/// [`run`] with everything the command line gave ([`HookArgs`]).
pub async fn run_with(
    harness: Harness,
    event: HookEvent,
    args: &HookArgs<'_>,
    stdin: &[u8],
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> HookOutput {
    let input = HookInput::parse(stdin);
    // opencode's process is shared by all its sessions: without a session
    // the hook is nobody's.
    let session_needed = harness == Harness::Opencode && args.session.is_none();
    let daemon = match session_needed {
        true => None,
        false => Daemon::connect(harness, args.session, env).await,
    };
    let Some(mut daemon) = daemon else {
        return match harness {
            Harness::Opencode => opencode::unavailable(event),
            _ => HookOutput::default(),
        };
    };
    match harness {
        Harness::ClaudeCode => claude::run(&mut daemon, event, &input, env).await,
        Harness::Opencode => opencode::run(&mut daemon, event, args, &input).await,
        Harness::Kiro => kiro::run(&mut daemon, event, args).await,
        Harness::Other => HookOutput::default(),
    }
}

/// A connection to a running daemon, as the hook's agent.
pub struct Daemon {
    client: Client,
    caller: Caller,
    paths: Paths,
    /// The processes the hook runs under, nearest first, up to its harness.
    procs: Vec<ProcId>,
    /// When the hook started ([`now_nanos`]), before it connected: a busy
    /// mark newer than this came after the hook was started.
    started: u128,
}

impl Daemon {
    /// Connect as the caller the environment names. `None` (stay silent)
    /// when the paths are unusable, the daemon is not running or does not
    /// answer in time, or the caller is the human.
    pub async fn connect(
        harness: Harness,
        session: Option<&str>,
        env: &(dyn Fn(&str) -> Option<OsString> + Sync),
    ) -> Option<Daemon> {
        let started = now_nanos();
        let procs = hook_procs(harness, env);
        let caller = hook_caller(harness, session, env, &procs)?;
        let paths = Paths::from_vars(env).ok()?;
        let client = tokio::time::timeout(CALL_LIMIT, Client::connect(&paths))
            .await
            .ok()?
            .ok()?;
        Some(Daemon {
            client,
            caller,
            paths,
            procs,
            started,
        })
    }

    /// The agent's [`HookStatus`], marking its urgent mail surfaced when
    /// `surface_urgent`. `None` for no agent (or a signed-off one), or any
    /// failure.
    pub async fn status(&mut self, surface_urgent: bool) -> Option<HookStatus> {
        let call = self
            .client
            .call(&self.caller, Request::HookStatus { surface_urgent });
        let data = tokio::time::timeout(CALL_LIMIT, call).await.ok()?.ok()?;
        serde_json::from_value(data).ok()
    }

    /// Sign the agent off. `None` for no agent (the session has none), or
    /// any failure; otherwise the agent's view after signing off.
    pub async fn signoff(&mut self) -> Option<Value> {
        let call = self.client.call(&self.caller, Request::Signoff);
        tokio::time::timeout(CALL_LIMIT, call).await.ok()?.ok()
    }

    /// Note that the agent's session is busy: a prompt was submitted, or a
    /// tool ran. A wait started by an earlier stop then ends, and one that
    /// has not claimed the turn yet never does (see [`Daemon::wait`]). The
    /// mark is the time it was made, in the `busy` file, which waits only
    /// read, so a mark made before a wait claims the turn is not lost.
    /// Best effort: a mark that cannot be written leaves the wait running.
    ///
    /// Only an agent with `wake` on is marked: no wait runs for one
    /// without, and nothing is written.
    pub fn mark_busy(&self, status: &HookStatus) {
        if status.wake {
            let path = state_path(&self.paths.data_dir, &status.address, "busy");
            let _ = write_state(&path, &now_nanos().to_string());
        }
    }

    /// The open questions (by id) a Stop hook has already shown the agent
    /// ([`Daemon::record_shown_questions`]); empty when none.
    pub fn shown_questions(&self, address: &str) -> HashSet<String> {
        read_ids(&self.paths.data_dir, address, "shown")
    }

    /// Record that a Stop hook showed the agent the open questions `ids`
    /// (all of them still open, so closed ones drop out). Best effort.
    pub fn record_shown_questions(&self, address: &str, ids: &[String]) {
        let path = state_path(&self.paths.data_dir, address, "shown");
        let _ = write_state(&path, &ids.join("\n"));
    }

    /// The mail ids the agent's last wake was for (the `woken` file).
    pub fn woken(&self, address: &str) -> HashSet<String> {
        read_woken(&self.paths.data_dir, address)
    }

    /// Record the Stop hook's decision for the stop `key` (for Claude
    /// Code, the payload's `prompt_id` with its `stop_hook_active`): whether
    /// it blocks the stop. The wait hook started by
    /// the same stop reads it ([`Daemon::wait`]), so the two never decide
    /// apart from different status reads. The record also dates the stop:
    /// a busy mark newer than it came after the stop. Written before the
    /// Stop hook prints, and only for an agent with `wake` on (no wait runs
    /// for one without) and a stop with a key. Best effort.
    pub fn record_stop(&self, status: &HookStatus, key: Option<&str>, block: bool) {
        let Some(key) = key.filter(|k| usable_stop_key(k)) else {
            return;
        };
        if status.wake {
            let path = state_path(&self.paths.data_dir, &status.address, "stop");
            let record = StopRecord {
                at: now_nanos(),
                block,
                key: key.to_string(),
            };
            let _ = write_state(&path, &record.to_string());
        }
    }

    /// Block until the idle agent has mail to be woken for. `stop_key` is
    /// the stop's key (for Claude Code, its `prompt_id` with its
    /// `stop_hook_active`), shared with the Stop hook of the same stop. A
    /// key can repeat: Claude Code keeps one `prompt_id` until the next
    /// human prompt, so a wake's continuation stops with the key of the
    /// stop whose wait woke it. A record with this key dated more than
    /// [`STOP_SLACK`] before this hook started is that earlier stop's, and
    /// the wait goes on looking for its own.
    ///
    /// With `wake_for_unread` (a plain stop), the wait ends at once with
    /// [`WaitOutcome::Busy`] when the Stop hook blocked this stop (for open
    /// questions): the session goes on with a continuation, so it is not
    /// idle, and the continuation's own stop starts the wait. That
    /// decision is the Stop hook's, read from its record
    /// ([`Daemon::record_stop`], looked for up to 6 s); only without one
    /// (no key, or no record in time) does the wait decide by itself from
    /// the agent's open questions. Otherwise unread mail already there
    /// counts, unless a wait already woke it for that mail (the `woken`
    /// file): mail it chose to leave unread wakes it once, not after every
    /// human turn.
    /// Otherwise (the agent was just woken, or blocked, and stopped again
    /// without reading) only mail that arrives from now on counts, so
    /// mail it chose to leave unread never wakes it twice. Which mail is
    /// new is decided per message (by id), so reading old mail meanwhile
    /// never hides a new one. Mail already read by the time the wait looks
    /// never counts.
    ///
    /// The wait is for an idle session only. It ends with
    /// [`WaitOutcome::Busy`] once the session is busy again: a busy mark
    /// ([`Daemon::mark_busy`]) newer than the stop (its record's time, or
    /// else when this hook started), whether made before or after the wait
    /// claims the turn. It also ends so when a later wait takes over: each
    /// stop starts one, the newest replaces the one before (the `turn`
    /// file), and only one waits per agent (the `lock` file), so the agent
    /// is never woken twice for the same mail.
    ///
    /// Ends with [`WaitOutcome::Gone`] when the agent signs off or is
    /// otherwise gone (its harness process was rebound to another agent:
    /// a re-read that names another agent, or a `Presence` event that
    /// shows it gone), the daemon goes away, or a process the hook runs
    /// under (its harness) exits; and with [`WaitOutcome::Skip`] when the
    /// agent has `wake` off, at once or as soon as a re-read shows it
    /// turned off (a registration that changed it publishes a `Presence`
    /// event). On sign-off, on going away and on harness exit the wait
    /// removes the agent's files ([`clear_wait_state`]); they are kept
    /// while the agent lives, for the next wait, and the daemon removes
    /// them once it finds the agent gone.
    pub async fn wait(&mut self, wake_for_unread: bool, stop_key: Option<&str>) -> WaitOutcome {
        self.wait_from(wake_for_unread, stop_key, &mut None, false)
            .await
    }

    /// [`Daemon::wait`], with the unread mail that does not count as news
    /// in `known`. `None` lets the first look decide (as `wait` does);
    /// `Some` (a timed-out wait's set, or the unread mail before a turn,
    /// passed back by opencode's plugin; or, from kiro's Stop hook, the
    /// mail the last wake was for and the open questions it has already
    /// shown) is used as is, and the wait never ends as `Busy` for open
    /// questions (the stop they would block was the timed-out wait's, not
    /// blocked, or the turn was not one to continue; kiro's Stop hook
    /// continues the stop itself for questions it has not shown). With `after_wake` and `known` `None`, `known` is the
    /// mail the last wake was for (the `woken` file), whatever the open
    /// questions, and the wait never ends as `Busy` for them either: the
    /// turn that just ended was started by a wake (or failed), so mail
    /// that came during it is news, as after a woken turn's plain stop.
    /// The wait keeps `known` up to date (mail read meanwhile drops out),
    /// so a caller that stops it (a timeout) can hand it to the wait that
    /// follows: mail that came in between is then news, not counted as
    /// already there.
    pub async fn wait_from(
        &mut self,
        wake_for_unread: bool,
        stop_key: Option<&str>,
        known: &mut Option<HashSet<String>>,
        after_wake: bool,
    ) -> WaitOutcome {
        let Some(status) = self.status(false).await else {
            return WaitOutcome::Gone;
        };
        if !status.wake {
            return WaitOutcome::Skip;
        }
        let address = status.address.clone();
        if after_wake && known.is_none() {
            *known = Some(read_woken(&self.paths.data_dir, &address));
        }
        let stop = match self.stop_record(&address, stop_key).await {
            Ok(stop) => stop,
            Err(NoLock::Busy) => return WaitOutcome::Busy,
            Err(NoLock::Gone) => return WaitOutcome::Gone,
            Err(NoLock::Skip) => return WaitOutcome::Skip,
        };
        // A busy mark newer than this counts.
        let since = stop.as_ref().map_or(self.started, |r| r.at);
        let blocked = match &stop {
            Some(r) => r.block,
            None => !status.open_questions.is_empty(),
        };
        // A wait passed a timed-out wait's `known` (a re-wait) follows a
        // stop that was already found not blocked, and one after a wake
        // (or with the unread mail before the turn) follows a turn whose
        // questions are not queued: questions opened since are mail like
        // any other, so they do not end it as `Busy`.
        let rewait = known.is_some();
        if wake_for_unread && blocked && !rewait {
            // The Stop hook blocks this stop, so the session goes on with
            // a continuation and is not idle: a wake now would be injected
            // into that turn. The continuation's own stop
            // (`stop_hook_active`) starts the real wait. Nothing is
            // written, so no earlier claim or wake record is disturbed.
            return WaitOutcome::Busy;
        }
        if busy_since(&self.paths.data_dir, &address, since) {
            return WaitOutcome::Busy;
        }
        let turn = state_path(&self.paths.data_dir, &address, "turn");
        // Claim the turn first, so the wait holding the lock lets go.
        let token = format!("wait {}", unique());
        let claimed = write_state(&turn, &token).is_ok();
        let _lock = match self
            .take_waiter_lock(&address, claimed.then_some(token.as_str()), since)
            .await
        {
            Ok(lock) => lock,
            Err(NoLock::Busy) => return WaitOutcome::Busy,
            Err(NoLock::Gone) => return WaitOutcome::Gone,
            Err(NoLock::Skip) => return WaitOutcome::Skip,
        };
        let data_dir = self.paths.data_dir.clone();
        let busy = || {
            (claimed && read_state(&turn).as_deref() != Some(token.as_str()))
                || busy_since(&data_dir, &address, since)
        };
        if busy() {
            return WaitOutcome::Busy;
        }
        let caller = self.caller.clone();
        // `known`: unread mail that does not count as news; `None` until
        // the first look sets it.
        loop {
            // Subscribe before looking, so no delivery slips between.
            let Some(mut watcher) = self.watcher().await else {
                return WaitOutcome::Gone;
            };
            let request = Request::Watch {
                agent: Some(address.clone()),
            };
            let Ok(Ok(events)) =
                tokio::time::timeout(CALL_LIMIT, watcher.stream(&caller, request)).await
            else {
                return WaitOutcome::Gone;
            };
            let mut events = Box::pin(events);
            let Some(now) = self.status_of(&address).await else {
                return WaitOutcome::Gone;
            };
            if !now.wake {
                return WaitOutcome::Skip;
            }
            // Without the Stop hook's record, questions that opened since
            // the first look may have made it block after all: then only
            // new mail counts.
            let known = known.get_or_insert_with(|| {
                if wake_for_unread && (stop.is_some() || now.open_questions.is_empty()) {
                    read_woken(&self.paths.data_dir, &address)
                } else {
                    now.unread_ids.iter().cloned().collect()
                }
            });
            if has_news(known, &now) {
                return self.wake(busy(), now);
            }
            let mut tick = tokio::time::interval(CHECK_EVERY);
            tick.tick().await;
            loop {
                tokio::select! {
                    ev = events.next() => match ev {
                        Some(Ok(Event::Delivered { to, .. })) if to == address => {
                            let Some(now) = self.status_of(&address).await else {
                                return WaitOutcome::Gone;
                            };
                            if !now.wake {
                                return WaitOutcome::Skip;
                            }
                            if has_news(known, &now) {
                                return self.wake(busy(), now);
                            }
                        }
                        Some(Ok(Event::Presence { agent }))
                            if agent.agent.address == address
                                && (agent.agent.signed_off
                                    || agent.presence == Presence::Gone) =>
                        {
                            clear_wait_state(&self.paths.data_dir, &address);
                            return WaitOutcome::Gone;
                        }
                        // A registration may have turned `wake` off.
                        Some(Ok(Event::Presence { agent }))
                            if agent.agent.address == address =>
                        {
                            let Some(now) = self.status_of(&address).await else {
                                return WaitOutcome::Gone;
                            };
                            if !now.wake {
                                return WaitOutcome::Skip;
                            }
                        }
                        Some(Ok(_)) => {}
                        // The watch fell behind: watch again and compare
                        // the store with what was known.
                        Some(Err(e)) if e.code != ErrorCode::ConnectionLost => break,
                        Some(Err(_)) | None => return WaitOutcome::Gone,
                    },
                    _ = tick.tick() => {
                        if !self.procs_alive() {
                            clear_wait_state(&self.paths.data_dir, &address);
                            return WaitOutcome::Gone;
                        }
                        if busy() {
                            return WaitOutcome::Busy;
                        }
                    }
                }
            }
        }
    }

    /// The status of `address`, the agent the wait started for. `None`
    /// also when the hook's caller now resolves to another agent (its
    /// harness process was registered as someone else): that agent's mail
    /// is not this wait's to wake for.
    async fn status_of(&mut self, address: &str) -> Option<HookStatus> {
        self.status(false).await.filter(|s| s.address == address)
    }

    /// The outcome for mail to wake for, seen in `now`: [`WaitOutcome::Busy`]
    /// when the session got busy meanwhile. A wake records the unread mail
    /// it is for (the `woken` file), so a later plain stop does not wake
    /// for the same mail again.
    fn wake(&self, busy: bool, now: HookStatus) -> WaitOutcome {
        if busy {
            return WaitOutcome::Busy;
        }
        let woken = state_path(&self.paths.data_dir, &now.address, "woken");
        let _ = write_state(&woken, &now.unread_ids.join("\n"));
        WaitOutcome::Mail(now)
    }

    /// Whether every process the hook runs under is still alive.
    fn procs_alive(&self) -> bool {
        self.procs.iter().all(procinfo::is_alive)
    }

    /// The Stop hook's record for the stop `key`, looked for up to
    /// [`STOP_LIMIT`] (the Stop hook runs alongside this one). A record
    /// with the key dated more than [`STOP_SLACK`] before this hook started
    /// is an earlier stop's and is passed over. `Ok(None)`
    /// without a key, or when no record came in time. Fails with
    /// [`NoLock::Busy`] when the record is a later stop's, or the
    /// session was marked busy since this hook started, and with
    /// [`NoLock::Gone`] when the hook's harness exits.
    async fn stop_record(
        &self,
        address: &str,
        key: Option<&str>,
    ) -> Result<Option<StopRecord>, NoLock> {
        let Some(key) = key.filter(|k| usable_stop_key(k)) else {
            return Ok(None);
        };
        let path = state_path(&self.paths.data_dir, address, "stop");
        let deadline = Instant::now() + STOP_LIMIT;
        let oldest = self.started.saturating_sub(STOP_SLACK.as_nanos());
        loop {
            match read_state(&path).and_then(|s| s.parse::<StopRecord>().ok()) {
                Some(r) if r.key == key && r.at >= oldest => return Ok(Some(r)),
                Some(r) if r.key == key => {}
                Some(r) if r.at > self.started => return Err(NoLock::Busy),
                _ => {}
            }
            if busy_since(&self.paths.data_dir, address, self.started) {
                return Err(NoLock::Busy);
            }
            if !self.procs_alive() {
                return Err(NoLock::Gone);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(STOP_POLL).await;
        }
    }

    /// The waiter lock for `address`, waiting for the wait it replaces to
    /// let go. With this wait's turn claim `token` written, it waits for as
    /// long as the claim stands (the old wait ends once it sees it), and
    /// ends with [`WaitOutcome::Busy`] when a later claim replaces it.
    /// Without one it waits up to [`TAKEOVER_LIMIT`], then
    /// [`WaitOutcome::Skip`]. A lock file that cannot be opened or locked
    /// is [`WaitOutcome::Skip`] too: two waits must never both run. Ends
    /// with [`WaitOutcome::Busy`] when the session is marked busy after
    /// `since`, and with [`WaitOutcome::Gone`] when the hook's harness
    /// exits.
    async fn take_waiter_lock(
        &self,
        address: &str,
        token: Option<&str>,
        since: u128,
    ) -> Result<File, NoLock> {
        let started = Instant::now();
        let turn = state_path(&self.paths.data_dir, address, "turn");
        loop {
            match waiter_lock(&self.paths, address) {
                Lock::Held(file) => return Ok(file),
                Lock::Unusable => return Err(NoLock::Skip),
                Lock::Taken => {}
            }
            match token {
                Some(token) if read_state(&turn).as_deref() != Some(token) => {
                    return Err(NoLock::Busy);
                }
                Some(_) => {}
                None if started.elapsed() >= TAKEOVER_LIMIT => return Err(NoLock::Skip),
                None => {}
            }
            if busy_since(&self.paths.data_dir, address, since) {
                return Err(NoLock::Busy);
            }
            if !self.procs_alive() {
                return Err(NoLock::Gone);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A second connection, for watching events.
    async fn watcher(&self) -> Option<Client> {
        tokio::time::timeout(CALL_LIMIT, Client::connect(&self.paths))
            .await
            .ok()?
            .ok()
    }
}

/// Whether `now` has unread mail not in `known`, and forget known mail
/// that is no longer unread.
fn has_news(known: &mut HashSet<String>, now: &HookStatus) -> bool {
    let unread: HashSet<&str> = now.unread_ids.iter().map(String::as_str).collect();
    known.retain(|id| unread.contains(id.as_str()));
    now.unread_ids.iter().any(|id| !known.contains(id))
}

/// The wall-clock time in nanoseconds since the epoch, which hook
/// processes compare across processes.
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// A token no other hook process writes.
fn unique() -> String {
    format!("{} {}", std::process::id(), now_nanos())
}

/// The Stop hook's decision for one stop ([`Daemon::record_stop`]), in the
/// `stop` file as `<at> <block|idle> <key>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StopRecord {
    /// When the Stop hook decided ([`now_nanos`]).
    at: u128,
    /// Whether it blocked the stop.
    block: bool,
    /// The stop's key (for Claude Code, its `prompt_id` with its
    /// `stop_hook_active`).
    key: String,
}

impl std::fmt::Display for StopRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let decision = if self.block { "block" } else { "idle" };
        write!(f, "{} {decision} {}", self.at, self.key)
    }
}

impl std::str::FromStr for StopRecord {
    type Err = ();

    fn from_str(s: &str) -> Result<StopRecord, ()> {
        let mut parts = s.split(' ');
        let at = parts.next().ok_or(())?.parse().map_err(|_| ())?;
        let block = match parts.next() {
            Some("block") => true,
            Some("idle") => false,
            _ => return Err(()),
        };
        let key = parts.next().filter(|k| !k.is_empty()).ok_or(())?;
        if parts.next().is_some() {
            return Err(());
        }
        Ok(StopRecord {
            at,
            block,
            key: key.to_string(),
        })
    }
}

/// Whether a stop key can be recorded: the stop record is one
/// whitespace-separated line, so a key with whitespace is never written,
/// and neither hook may look for it ([`Daemon::record_stop`],
/// [`Daemon::wait`]).
pub(crate) fn usable_stop_key(key: &str) -> bool {
    !key.contains(char::is_whitespace)
}

/// Where waiting hooks keep their per-agent files.
fn wait_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("hook-wait")
}

/// `address`'s file of `kind` (one of [`STATE_KINDS`]):
///
/// - `turn`: the newest wait's claim on the agent's turn. A wait whose
///   token is no longer there ends.
/// - `lock`: the waiter lock ([`waiter_lock`]).
/// - `woken`: the unread mail ids the last wake was for, one per line.
/// - `busy`: when the session was last marked busy ([`Daemon::mark_busy`]).
/// - `stop`: the Stop hook's decision for the latest stop
///   ([`Daemon::record_stop`]).
/// - `shown`: the open questions a kiro Stop hook has shown the agent, one
///   id per line ([`Daemon::shown_questions`]).
fn state_path(data_dir: &Path, address: &str, kind: &str) -> PathBuf {
    wait_dir(data_dir).join(format!("{address}.{kind}"))
}

/// Whether `name` is one of `address`'s files under the wait directory:
/// `<address>.<kind>`, or a temporary `<address>.<kind>.<pid>-<nanos>` a
/// write left behind ([`write_state`]).
fn is_state_of(name: &str, address: &str) -> bool {
    let Some(rest) = name.strip_prefix(address).and_then(|r| r.strip_prefix('.')) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    STATE_KINDS
        .iter()
        .any(|kind| match rest.strip_prefix(kind) {
            Some("") => true,
            Some(tmp) => tmp
                .strip_prefix('.')
                .and_then(|t| t.split_once('-'))
                .is_some_and(|(pid, nanos)| digits(pid) && digits(nanos)),
            None => false,
        })
}

/// Remove all of `address`'s files under `data_dir` (turn, lock, woken,
/// busy, stop, and temporary files a crashed write left), for an agent
/// that signed off, is gone, or whose harness exited. Best effort. A wait
/// still holding the removed lock keeps it; one that opens the path
/// afterwards gets a new file, and `waiter_lock` makes sure the two never
/// both count as held.
pub fn clear_wait_state(data_dir: &Path, address: &str) {
    let Ok(entries) = std::fs::read_dir(wait_dir(data_dir)) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| is_state_of(n, address))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `content` to `path`, whole: written to a temporary file next to
/// it and renamed into place, so a reader never sees half.
fn write_state(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}", unique().replace(' ', "-")));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_state(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Whether `address` was marked busy after `since` ([`now_nanos`]).
fn busy_since(data_dir: &Path, address: &str, since: u128) -> bool {
    read_state(&state_path(data_dir, address, "busy"))
        .and_then(|s| s.trim().parse::<u128>().ok())
        .is_some_and(|at| at > since)
}

/// The mail ids the agent's last wake was for (empty when none).
fn read_woken(data_dir: &Path, address: &str) -> HashSet<String> {
    read_ids(data_dir, address, "woken")
}

/// The ids, one per line, in `address`'s file of `kind` (empty when none).
fn read_ids(data_dir: &Path, address: &str, kind: &str) -> HashSet<String> {
    read_state(&state_path(data_dir, address, kind))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Why [`Daemon::take_waiter_lock`] got no lock, or
/// [`Daemon::stop_record`] no record; the wait ends with the
/// [`WaitOutcome`] of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoLock {
    Busy,
    Gone,
    Skip,
}

/// What [`waiter_lock`] got.
#[derive(Debug)]
enum Lock {
    /// This wait holds the lock.
    Held(File),
    /// Another wait holds it.
    Taken,
    /// The lock file cannot be opened or locked.
    Unusable,
}

/// Take the lock only one waiting hook per agent may hold. A lock taken on
/// a file that was removed meanwhile ([`clear_wait_state`]) does not
/// count: another wait may already hold the new file at that path, so
/// this one tries again.
fn waiter_lock(paths: &Paths, address: &str) -> Lock {
    use std::os::unix::fs::MetadataExt;
    let dir = wait_dir(&paths.data_dir);
    let path = state_path(&paths.data_dir, address, "lock");
    let file = std::fs::create_dir_all(&dir).and_then(|()| {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
    });
    let Ok(file) = file else {
        return Lock::Unusable;
    };
    match file.try_lock() {
        Ok(()) => {
            let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| {
                a.dev() == b.dev() && a.ino() == b.ino()
            };
            match (file.metadata(), std::fs::metadata(&path)) {
                (Ok(held), Ok(now)) if same(&held, &now) => Lock::Held(file),
                (Ok(_), _) => Lock::Taken,
                (Err(_), _) => Lock::Held(file),
            }
        }
        Err(std::fs::TryLockError::WouldBlock) => Lock::Taken,
        Err(std::fs::TryLockError::Error(_)) => Lock::Unusable,
    }
}

/// Who the hook acts as; `None` for the human, who has no hook output.
/// `AGENTMAIL_ADDRESS` is deliberately not read (see the module docs).
fn hook_caller(
    harness: Harness,
    session: Option<&str>,
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
    procs: &[ProcId],
) -> Option<Caller> {
    if env(ENV_HUMAN).is_some_and(|v| v == "1") {
        return None;
    }
    if let Some(id) = session {
        return Some(Caller::Session {
            session: SessionKey {
                harness,
                id: id.to_string(),
            },
        });
    }
    Some(Caller::Procs {
        procs: procs.to_vec(),
    })
}

/// This process's ancestry, nearest first, cut after its harness process:
/// for Claude Code the process `CLAUDE_PID` names (Claude Code sets it for
/// its hooks, and its command name need not be `claude`), otherwise the
/// nearest ancestor with a harness's command name. With neither, the whole
/// ancestry.
///
/// Cutting keeps a harness run from another harness's shell (`claude`
/// started from a registered Claude Code's Bash tool) from acting as the
/// outer harness's agent: its hooks would remind it of, and wake it for,
/// someone else's mail.
fn hook_procs(harness: Harness, env: &(dyn Fn(&str) -> Option<OsString> + Sync)) -> Vec<ProcId> {
    let mut chain = procinfo::ancestors(std::process::id());
    let claude_pid = (harness == Harness::ClaudeCode)
        .then(|| env(ENV_CLAUDE_PID))
        .flatten()
        .and_then(|v| v.to_str()?.parse::<u32>().ok());
    let cut = claude_pid
        .and_then(|pid| chain.iter().position(|p| p.pid == pid))
        .or_else(|| {
            chain.iter().position(|p| {
                procinfo::comm(p.pid)
                    .is_some_and(|c| procinfo::detect_harness(&c) != Harness::Other)
            })
        });
    if let Some(i) = cut {
        chain.truncate(i + 1);
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_input_reads_claude_fields() {
        let input =
            HookInput::parse(br#"{"session_id":"s-1","prompt_id":"p-1","stop_hook_active":true}"#);
        assert_eq!(input.session_id.as_deref(), Some("s-1"));
        assert_eq!(input.prompt_id.as_deref(), Some("p-1"));
        assert!(input.stop_hook_active);
        assert_eq!(input.agent_id, None);
        assert!(!input.is_subagent());
        assert!(!input.after_wake);
        let input = HookInput::parse(br#"{"stop_hook_active":false,"after_wake":true}"#);
        assert!(input.after_wake);
        assert_eq!(input.known, None);
        for junk in [
            &b""[..],
            b"not json",
            b"[1,2]",
            b"{\"stop_hook_active\":\"yes\",\"agent_id\":\"\"}",
        ] {
            let input = HookInput::parse(junk);
            assert_eq!(input.session_id, None);
            assert!(!input.stop_hook_active);
            assert_eq!(input.agent_id, None);
            assert_eq!(input.hook_event_name, None);
            assert_eq!(input.prompt_id, None);
            assert!(!input.after_wake);
        }
    }

    #[test]
    fn stop_record_round_trip() {
        for block in [false, true] {
            let r = StopRecord {
                at: 1_700_000_000_123_456_789,
                block,
                key: "8745ca22-c22c".into(),
            };
            assert_eq!(r.to_string().parse::<StopRecord>(), Ok(r));
        }
        for junk in ["", "1 idle", "x idle k", "1 maybe k", "1 block k extra"] {
            assert!(junk.parse::<StopRecord>().is_err(), "{junk:?}");
        }
    }

    #[test]
    fn hook_input_reads_subagent_and_event() {
        let input = HookInput::parse(
            br#"{"session_id":"s","agent_id":"a7","hook_event_name":"PostToolUseFailure"}"#,
        );
        assert_eq!(input.agent_id.as_deref(), Some("a7"));
        assert!(input.is_subagent());
        assert_eq!(input.hook_event_name.as_deref(), Some("PostToolUseFailure"));
    }

    #[test]
    fn hook_input_cut_short_keeps_agent_id() {
        // A payload cut off inside a huge tool result is not JSON; the
        // subagent's id comes before the tool fields.
        let cut = br#"{"session_id":"s","agent_id": "a7","agent_type":"x","hook_event_name":"PostToolUse","tool_response":{"stdout":"aaa"#;
        assert_eq!(HookInput::parse(cut).agent_id.as_deref(), Some("a7"));
        // An `agent_id` inside the tool fields is the tool's, not Claude
        // Code's.
        let main = br#"{"session_id":"s","hook_event_name":"PostToolUse","tool_input":{"command":"echo '\"agent_id\":\"z\"'"#;
        assert_eq!(HookInput::parse(main).agent_id, None);
        assert_eq!(HookInput::parse(br#"{"agent_id":"#).agent_id, None);
    }

    fn held(lock: Lock) -> File {
        match lock {
            Lock::Held(f) => f,
            other => panic!("not held: {other:?}"),
        }
    }

    fn names_in(data_dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(wait_dir(data_dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn clear_wait_state_removes_the_agent_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(&dir.path().join("data"));
        let path = |a: &str, k: &str| state_path(&paths.data_dir, a, k);
        write_state(&path("a-1", "turn"), "wait 1").unwrap();
        write_state(&path("a-1", "woken"), "m1").unwrap();
        write_state(&path("a-1", "shown"), "q1").unwrap();
        write_state(&path("a-1", "busy"), "1").unwrap();
        write_state(&path("a-1", "stop"), "1 idle k").unwrap();
        write_state(&path("b-2", "turn"), "wait 1").unwrap();
        let lock = held(waiter_lock(&paths, "a-1"));
        // Temporary files a crashed write left behind go too; files of
        // other agents whose address starts the same way stay.
        let wd = wait_dir(&paths.data_dir);
        for name in [
            "a-1.turn.123-456",
            "a-1.woken.7-8",
            "a-10.turn",
            "a-1.turn-x.turn",
            "a-1.turn.x-1.turn",
        ] {
            std::fs::write(wd.join(name), "").unwrap();
        }
        clear_wait_state(&paths.data_dir, "a-1");
        assert_eq!(
            names_in(&paths.data_dir),
            vec![
                "a-1.turn-x.turn",
                "a-1.turn.x-1.turn",
                "a-10.turn",
                "b-2.turn"
            ]
        );
        // The old holder's lock does not block a new wait, and the new
        // wait's file is a fresh one.
        let again = held(waiter_lock(&paths, "a-1"));
        drop(lock);
        assert!(
            matches!(waiter_lock(&paths, "a-1"), Lock::Taken),
            "{again:?}"
        );
    }

    #[test]
    fn unopenable_lock_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(&dir.path().join("data"));
        // A directory where the lock file should be.
        std::fs::create_dir_all(state_path(&paths.data_dir, "a-1", "lock")).unwrap();
        assert!(matches!(waiter_lock(&paths, "a-1"), Lock::Unusable));
    }

    #[test]
    fn woken_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(&dir.path().join("data"));
        assert!(read_woken(&paths.data_dir, "a-1").is_empty());
        write_state(&state_path(&paths.data_dir, "a-1", "woken"), "m1\nm2").unwrap();
        let ids = read_woken(&paths.data_dir, "a-1");
        assert_eq!(ids, ["m1".to_string(), "m2".to_string()].into());
        write_state(&state_path(&paths.data_dir, "a-1", "woken"), "").unwrap();
        assert!(read_woken(&paths.data_dir, "a-1").is_empty());
    }

    #[test]
    fn stale_lock_on_a_removed_file_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(&dir.path().join("data"));
        // Opened before the removal, locked after: that lock is stale.
        let path = state_path(&paths.data_dir, "a-1", "lock");
        std::fs::create_dir_all(wait_dir(&paths.data_dir)).unwrap();
        std::fs::write(&path, "").unwrap();
        let stale = OpenOptions::new().write(true).open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        stale.try_lock().unwrap();
        let lock = held(waiter_lock(&paths, "a-1"));
        assert!(matches!(waiter_lock(&paths, "a-1"), Lock::Taken));
        drop(lock);
    }

    #[test]
    fn has_news_is_per_message() {
        let status = |ids: &[&str]| HookStatus {
            address: "a".into(),
            name: "a".into(),
            wake: true,
            unread: ids.len(),
            unread_ids: ids.iter().map(|s| s.to_string()).collect(),
            open_questions: Vec::new(),
            urgent: Vec::new(),
        };
        let mut known: HashSet<String> = ["m1".to_string()].into();
        assert!(!has_news(&mut known, &status(&["m1"])));
        // m1 read, m2 new: the count is the same, but m2 is news.
        assert!(has_news(&mut known.clone(), &status(&["m2"])));
        // Reading m1 forgets it.
        assert!(!has_news(&mut known, &status(&[])));
        assert!(known.is_empty());
        assert!(has_news(&mut known, &status(&["m1"])));
    }

    #[test]
    fn turn_marker_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(&dir.path().join("data"));
        let turn = state_path(&paths.data_dir, "a-1", "turn");
        assert_eq!(read_state(&turn), None);
        write_state(&turn, "wait 1").unwrap();
        assert_eq!(read_state(&turn).as_deref(), Some("wait 1"));
        write_state(&turn, "busy 2").unwrap();
        assert_eq!(read_state(&turn).as_deref(), Some("busy 2"));
        // No temporary files are left behind.
        assert_eq!(names_in(&paths.data_dir), vec!["a-1.turn"]);
    }

    #[test]
    fn hook_caller_order() {
        let procs = vec![ProcId {
            pid: 10,
            start_time: 1,
        }];
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        // `AGENTMAIL_ADDRESS` never names the hook's agent: not over
        // `--session`, and not over the cut ancestry.
        let addr = env(&[("AGENTMAIL_ADDRESS", "a-1")]);
        assert_eq!(
            hook_caller(Harness::Opencode, Some("s"), &addr, &procs),
            Some(Caller::Session {
                session: SessionKey {
                    harness: Harness::Opencode,
                    id: "s".into()
                }
            })
        );
        assert_eq!(
            hook_caller(Harness::ClaudeCode, None, &addr, &procs),
            Some(Caller::Procs {
                procs: procs.clone()
            })
        );
        let human = env(&[("AGENTMAIL_ADDRESS", "a-1"), ("AGENTMAIL_HUMAN", "1")]);
        assert_eq!(
            hook_caller(Harness::Opencode, Some("s"), &human, &procs),
            None
        );
        let none = env(&[]);
        assert_eq!(
            hook_caller(Harness::Opencode, Some("s"), &none, &procs),
            Some(Caller::Session {
                session: SessionKey {
                    harness: Harness::Opencode,
                    id: "s".into()
                }
            })
        );
        assert_eq!(
            hook_caller(Harness::ClaudeCode, None, &none, &procs),
            Some(Caller::Procs {
                procs: procs.clone()
            })
        );
    }

    #[test]
    fn hook_procs_cut_at_claude_pid() {
        let all = procinfo::ancestors(std::process::id());
        let none = |_: &str| None;
        let full = hook_procs(Harness::Other, &none);
        assert!(!full.is_empty());
        assert!(all.starts_with(&full));
        // CLAUDE_PID naming our parent: only the parent is left.
        let parent = all[0].pid.to_string();
        let env = move |k: &str| (k == ENV_CLAUDE_PID).then(|| OsString::from(&parent));
        assert_eq!(hook_procs(Harness::ClaudeCode, &env), vec![all[0]]);
        // Other harnesses do not read it.
        assert_eq!(hook_procs(Harness::Kiro, &env), full);
        // A pid that is not an ancestor is ignored.
        let stranger = |k: &str| (k == ENV_CLAUDE_PID).then(|| OsString::from("999999999"));
        assert_eq!(hook_procs(Harness::ClaudeCode, &stranger), full);
    }
}
