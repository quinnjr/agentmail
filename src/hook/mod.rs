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
//! Identity: `AGENTMAIL_ADDRESS` when set (as for the CLI), then the
//! harness session given with `--session` (opencode), then the hook's
//! process ancestry up to its harness process (Claude Code, kiro). A hook
//! run as the human (`AGENTMAIL_HUMAN=1`) has nothing to say.

pub mod claude;
pub mod text;

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::identity::{ENV_ADDRESS, ENV_HUMAN};
use crate::model::{Harness, Message, ProcId, SessionKey};
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{Caller, ErrorCode, Event, Request};

/// The longest a hook waits for any one daemon answer (connecting
/// included) before giving up silently.
pub const CALL_LIMIT: Duration = Duration::from_secs(5);

/// How often a waiting hook checks that the processes it ran under are
/// still alive and that its session is still idle.
const CHECK_EVERY: Duration = Duration::from_millis(500);

/// How long a waiting hook waits for the one it replaces to let go.
const TAKEOVER_LIMIT: Duration = Duration::from_secs(5);

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
}

impl HookInput {
    /// Parse a payload. Anything unreadable counts as an empty one.
    pub fn parse(stdin: &[u8]) -> HookInput {
        let raw: Value = serde_json::from_slice(stdin).unwrap_or(Value::Null);
        HookInput {
            session_id: raw["session_id"].as_str().map(str::to_string),
            stop_hook_active: raw["stop_hook_active"].as_bool().unwrap_or(false),
            raw,
        }
    }
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
    /// The agent signed off, the processes the hook runs under are gone,
    /// or the daemon went away.
    Gone,
    /// The session is busy again (a prompt or tool call since the wait
    /// began), or a later wait took over.
    Busy,
    /// Nothing to wait for: the agent does not want waking, or the wait it
    /// would replace did not let go in time.
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
    let input = HookInput::parse(stdin);
    let Some(mut daemon) = Daemon::connect(harness, session, env).await else {
        return HookOutput::default();
    };
    match harness {
        Harness::ClaudeCode => claude::run(&mut daemon, event, &input, env).await,
        Harness::Opencode | Harness::Kiro | Harness::Other => HookOutput::default(),
    }
}

/// A connection to a running daemon, as the hook's agent.
pub struct Daemon {
    client: Client,
    caller: Caller,
    paths: Paths,
    /// The processes the hook runs under, nearest first, up to its harness.
    procs: Vec<ProcId>,
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

    /// Note that the agent's session is busy: a prompt was submitted, or a
    /// tool ran. A wait started by an earlier stop then ends (see
    /// [`Daemon::wait`]). Best effort: a marker that cannot be written
    /// leaves the wait running.
    pub fn mark_busy(&self, address: &str) {
        let _ = write_turn(&self.paths, address, &format!("busy {}", unique()));
    }

    /// Block until the idle agent has mail to be woken for.
    ///
    /// With `wake_for_unread`, unread mail already there counts, unless the
    /// agent also has open questions (a Stop hook deals with those).
    /// Otherwise (the agent was just woken, or blocked, and stopped again
    /// without reading) only mail that arrives from now on counts, so
    /// mail it chose to leave unread never wakes it twice. Which mail is
    /// new is decided per message (by id), so reading old mail meanwhile
    /// never hides a new one. Mail already read by the time the wait looks
    /// never counts.
    ///
    /// The wait is for an idle session only. It ends with
    /// [`WaitOutcome::Busy`] once the session is busy again
    /// ([`Daemon::mark_busy`]) or a later wait takes over: each stop
    /// starts one, the newest replaces the one before, and only one waits
    /// per agent, so the agent is never woken twice for the same mail.
    ///
    /// Ends with [`WaitOutcome::Gone`] when the agent signs off, the daemon
    /// goes away, or a process the hook runs under (its harness) exits; and
    /// with [`WaitOutcome::Skip`] at once when the agent has `wake` off.
    pub async fn wait(&mut self, wake_for_unread: bool) -> WaitOutcome {
        let Some(status) = self.status(false).await else {
            return WaitOutcome::Gone;
        };
        if !status.wake {
            return WaitOutcome::Skip;
        }
        let address = status.address.clone();
        // Claim the turn first, so the wait holding the lock lets go.
        let token = format!("wait {}", unique());
        let claimed = write_turn(&self.paths, &address, &token).is_ok();
        let Some(_lock) = self.take_waiter_lock(&address).await else {
            return WaitOutcome::Skip;
        };
        let busy = |paths: &Paths| claimed && read_turn(paths, &address) != Some(token.clone());
        if busy(&self.paths) {
            return WaitOutcome::Busy;
        }
        let caller = self.caller.clone();
        // Unread mail that does not count as news; `None` until the first
        // look sets it.
        let mut known: Option<HashSet<String>> = None;
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
            let Some(now) = self.status(false).await else {
                return WaitOutcome::Gone;
            };
            let known = known.get_or_insert_with(|| {
                if wake_for_unread && now.open_questions.is_empty() {
                    HashSet::new()
                } else {
                    now.unread_ids.iter().cloned().collect()
                }
            });
            if has_news(known, &now) {
                return if busy(&self.paths) {
                    WaitOutcome::Busy
                } else {
                    WaitOutcome::Mail(now)
                };
            }
            let mut tick = tokio::time::interval(CHECK_EVERY);
            tick.tick().await;
            loop {
                tokio::select! {
                    ev = events.next() => match ev {
                        Some(Ok(Event::Delivered { to, .. })) if to == address => {
                            let Some(now) = self.status(false).await else {
                                return WaitOutcome::Gone;
                            };
                            if has_news(known, &now) {
                                return if busy(&self.paths) {
                                    WaitOutcome::Busy
                                } else {
                                    WaitOutcome::Mail(now)
                                };
                            }
                        }
                        Some(Ok(Event::Presence { agent }))
                            if agent.agent.address == address && agent.agent.signed_off =>
                        {
                            return WaitOutcome::Gone;
                        }
                        Some(Ok(_)) => {}
                        // The watch fell behind: watch again and compare
                        // the store with what was known.
                        Some(Err(e)) if e.code != ErrorCode::ConnectionLost => break,
                        Some(Err(_)) | None => return WaitOutcome::Gone,
                    },
                    _ = tick.tick() => {
                        if !self.procs.iter().all(procinfo::is_alive) {
                            return WaitOutcome::Gone;
                        }
                        if busy(&self.paths) {
                            return WaitOutcome::Busy;
                        }
                    }
                }
            }
        }
    }

    /// The waiter lock for `address`, waiting up to [`TAKEOVER_LIMIT`] for
    /// the wait it replaces to let go. `None` when it never did.
    async fn take_waiter_lock(&self, address: &str) -> Option<Option<File>> {
        let started = Instant::now();
        loop {
            if let Some(lock) = waiter_lock(&self.paths, address) {
                return Some(lock);
            }
            if started.elapsed() >= TAKEOVER_LIMIT {
                return None;
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

/// A token no other hook process writes.
fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{} {nanos}", std::process::id())
}

/// The file that says who last claimed the agent's turn: the newest wait,
/// or a busy mark. A wait whose token is no longer there ends.
fn turn_path(paths: &Paths, address: &str) -> PathBuf {
    paths
        .data_dir
        .join("hook-wait")
        .join(format!("{address}.turn"))
}

fn write_turn(paths: &Paths, address: &str, token: &str) -> std::io::Result<()> {
    let path = turn_path(paths, address);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole and renamed into place, so a reader never sees half.
    let tmp = path.with_extension(format!("turn.{}", unique().replace(' ', "-")));
    std::fs::write(&tmp, token)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_turn(paths: &Paths, address: &str) -> Option<String> {
    std::fs::read_to_string(turn_path(paths, address)).ok()
}

/// Take the lock only one waiting hook per agent may hold: `None` when
/// another holds it. A lock file that cannot be opened does not stop the
/// wait.
fn waiter_lock(paths: &Paths, address: &str) -> Option<Option<File>> {
    let dir = paths.data_dir.join("hook-wait");
    let path: PathBuf = dir.join(format!("{address}.lock"));
    let file = std::fs::create_dir_all(&dir).and_then(|()| {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
    });
    let Ok(file) = file else {
        return Some(None);
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(_)) => Some(None),
    }
}

/// Who the hook acts as; `None` for the human, who has no hook output.
fn hook_caller(
    harness: Harness,
    session: Option<&str>,
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
    procs: &[ProcId],
) -> Option<Caller> {
    if let Some(address) = env(ENV_ADDRESS).filter(|a| !a.is_empty()) {
        return Some(Caller::Address {
            address: address.to_string_lossy().into_owned(),
        });
    }
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
        let input = HookInput::parse(br#"{"session_id":"s-1","stop_hook_active":true}"#);
        assert_eq!(input.session_id.as_deref(), Some("s-1"));
        assert!(input.stop_hook_active);
        for junk in [
            &b""[..],
            b"not json",
            b"[1,2]",
            b"{\"stop_hook_active\":\"yes\"}",
        ] {
            let input = HookInput::parse(junk);
            assert_eq!(input.session_id, None);
            assert!(!input.stop_hook_active);
        }
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
        assert_eq!(read_turn(&paths, "a-1"), None);
        write_turn(&paths, "a-1", "wait 1").unwrap();
        assert_eq!(read_turn(&paths, "a-1").as_deref(), Some("wait 1"));
        write_turn(&paths, "a-1", "busy 2").unwrap();
        assert_eq!(read_turn(&paths, "a-1").as_deref(), Some("busy 2"));
        // No temporary files are left behind.
        let names: Vec<_> = std::fs::read_dir(paths.data_dir.join("hook-wait"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("a-1.turn")]);
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
        let addr = env(&[("AGENTMAIL_ADDRESS", "a-1"), ("AGENTMAIL_HUMAN", "1")]);
        assert_eq!(
            hook_caller(Harness::Opencode, Some("s"), &addr, &procs),
            Some(Caller::Address {
                address: "a-1".into()
            })
        );
        let human = env(&[("AGENTMAIL_ADDRESS", ""), ("AGENTMAIL_HUMAN", "1")]);
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
