//! The `agentmail` command line: the clap command tree, body input,
//! dispatch to the daemon, and exit codes.
//!
//! Exit codes: 0 success; 1 error; 2 usage error (clap); 3 when `ask`,
//! `await` or `wait` end without an answer or mail (`pending`,
//! `no_one_listening`, `timeout`), so scripts and hooks can branch.

pub mod render;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::client::{Client, ClientError, DataDirMismatch, settled_lock_record};
use crate::daemon::handler::MAX_WAIT;
use crate::daemon::{DaemonConfig, serve_until, shutdown_signal};
use crate::hook::HookEvent;
use crate::identity;
use crate::ids::now_ms;
use crate::model::{AgentView, HUMAN, Harness, MAX_BODY_BYTES, Message, ProcId, SessionKey};
use crate::paths::Paths;
use crate::protocol::{Caller, ErrorCode, PROTOCOL_VERSION, Request};
use crate::store::{ThreadFilter, ThreadSummary};

/// Exit code for `ask`/`await`/`wait` ending without an answer or mail.
pub const EXIT_PENDING: u8 = 3;
/// `daemon status` exit code when a daemon answers but speaks another
/// protocol version (1 means not running).
const STATUS_OTHER_PROTOCOL: u8 = 3;
/// Length of a full message id (a ULID).
const ULID_LEN: usize = 26;
/// The fewest characters a short id may have: fewer match too many
/// messages to be useful, and each lookup scans every message.
const MIN_SHORT_ID: usize = 4;
/// How many matching ids an ambiguous short id's error lists.
const AMBIGUOUS_SHOWN: usize = 5;
/// The default for every `--timeout`.
const DEFAULT_TIMEOUT: &str = "240s";

#[derive(Parser, Debug)]
#[command(
    name = "agentmail",
    version,
    about = "Mail between coding agents and a human"
)]
pub struct Cli {
    /// Act as WHO instead of the registered agent. Only `human` is accepted
    /// (or set AGENTMAIL_HUMAN=1).
    #[arg(long = "as", global = true, value_name = "WHO", value_parser = [HUMAN])]
    pub as_who: Option<String>,
    /// Print the daemon's JSON instead of human-readable output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// Where a message body comes from. Exactly one is required.
#[derive(Args, Debug, Clone)]
#[group(required = true, multiple = false)]
pub struct BodyArgs {
    /// The body as markdown text (may start with `-`, e.g. a bullet list).
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    body: Option<String>,
    /// Read the body from a file.
    #[arg(long, value_name = "PATH")]
    body_file: Option<PathBuf>,
    /// `-` reads the body from standard input (e.g. a heredoc).
    #[arg(value_name = "-", value_parser = ["-"])]
    stdin: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Register this session as an agent and print its address.
    Register {
        /// Short role name, e.g. `backend`.
        name: String,
        /// One-line role description.
        #[arg(long, allow_hyphen_values = true)]
        role: Option<String>,
        /// What you are working on (at most 280 characters).
        #[arg(long, allow_hyphen_values = true)]
        working_on: Option<String>,
        /// Override the detected harness. In an opencode or kiro session the
        /// harness is fixed by the session: any other value is refused.
        #[arg(long, value_parser = ["claude-code", "opencode", "kiro", "other"])]
        harness: Option<String>,
        /// Always take a new address instead of reclaiming a gone one.
        #[arg(long)]
        fresh: bool,
        /// Do not wake this agent for new mail.
        #[arg(long)]
        no_wake: bool,
        /// Bind to this process instead of the detected harness process. A
        /// harness process, or a process that is not an ancestor of this
        /// command, is bound exactly; an ancestor that is not a harness
        /// process (say the shell, `$$`) under a kiro or opencode process is
        /// replaced by that kiro or opencode process.
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Show who you are acting as.
    Whoami,
    /// Set your working-on summary.
    Status {
        /// The new summary (at most 280 characters).
        #[arg(allow_hyphen_values = true)]
        working_on: String,
    },
    /// List, add or remove contacts (default: list).
    Contact {
        #[command(subcommand)]
        action: Option<ContactAction>,
    },
    /// List registered agents (active ones unless --all).
    Agents {
        /// Only agents whose name starts with this.
        #[arg(long, value_name = "PREFIX")]
        name: Option<String>,
        /// Only agents in this repo or directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Include gone agents.
        #[arg(long)]
        all: bool,
    },
    /// Start a new thread.
    Send {
        /// Recipients: full addresses or names, comma-separated or repeated.
        #[arg(
            long,
            required = true,
            value_delimiter = ',',
            value_name = "ADDR",
            value_parser = parse_recipient
        )]
        to: Vec<String>,
        /// The thread's subject line.
        #[arg(long, allow_hyphen_values = true)]
        subject: String,
        /// Ask for live interruption where the harness supports it.
        #[arg(long)]
        urgent: bool,
        /// A file path pointing at shared docs or code (repeatable).
        #[arg(long = "ref", value_name = "PATH")]
        refs: Vec<String>,
        #[command(flatten)]
        body: BodyArgs,
    },
    /// Reply in a message's thread, to its sender and other recipients.
    Reply {
        /// The message to reply to (full id or the inbox's short id).
        id: String,
        #[command(flatten)]
        body: BodyArgs,
        /// Ask for live interruption where the harness supports it.
        #[arg(long)]
        urgent: bool,
        /// A file path pointing at shared docs or code (repeatable).
        #[arg(long = "ref", value_name = "PATH")]
        refs: Vec<String>,
        /// Close the question this replies to (same as `answer`).
        #[arg(long)]
        answer: bool,
    },
    /// Ask a question and wait for its answer. Exits 3 if none comes in time.
    Ask {
        /// Recipients: full addresses or names, comma-separated or repeated.
        #[arg(
            long,
            required = true,
            value_delimiter = ',',
            value_name = "ADDR",
            value_parser = parse_recipient
        )]
        to: Vec<String>,
        /// The thread's subject line.
        #[arg(long, allow_hyphen_values = true)]
        subject: String,
        #[command(flatten)]
        body: BodyArgs,
        /// A file path pointing at shared docs or code (repeatable).
        #[arg(long = "ref", value_name = "PATH")]
        refs: Vec<String>,
        /// Ask for live interruption where the harness supports it.
        #[arg(long)]
        urgent: bool,
        /// How long to wait, e.g. `240s`, `10m`.
        #[arg(long, default_value = DEFAULT_TIMEOUT, value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Keep waiting for the answer to your question. Exits 3 if none comes.
    Await {
        /// The question's id (full, or the inbox's short id).
        question_id: String,
        /// How long to wait, e.g. `240s`, `10m`.
        #[arg(long, default_value = DEFAULT_TIMEOUT, value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Answer a question (same as `reply --answer`).
    Answer {
        /// The question's id (full, or the inbox's short id).
        question_id: String,
        #[command(flatten)]
        body: BodyArgs,
        /// Ask for live interruption where the harness supports it.
        #[arg(long)]
        urgent: bool,
        /// A file path pointing at shared docs or code (repeatable).
        #[arg(long = "ref", value_name = "PATH")]
        refs: Vec<String>,
    },
    /// Block until new mail arrives. Exits 3 on timeout.
    Wait {
        /// How long to wait, e.g. `240s`, `10m`.
        #[arg(long, default_value = DEFAULT_TIMEOUT, value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Your mail: open questions first, then unread messages.
    Inbox {
        /// Include messages already read.
        #[arg(long)]
        all: bool,
    },
    /// Show a message, or a whole thread, and mark it read.
    Read {
        /// Message or thread id (full, or the inbox's short id).
        id: String,
    },
    /// List threads.
    Threads {
        /// Only threads this address took part in.
        #[arg(long, value_name = "ADDR")]
        agent: Option<String>,
        /// Only threads with a participant in this repo or directory.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Only threads with an open question.
        #[arg(long)]
        open_questions: bool,
    },
    /// Show a whole thread, for humans.
    Show {
        /// A thread id, or the id of any message in it.
        id: String,
    },
    /// Live tail of mail and presence.
    Watch {
        /// Only events involving this address.
        #[arg(long, value_name = "ADDR")]
        agent: Option<String>,
    },
    /// Search subjects, bodies and senders.
    Search {
        /// The query; it may start with `-`. Quote a query of several words
        /// when a later word starts with `-`, or put it after `--`.
        #[arg(allow_hyphen_values = true)]
        query: String,
        /// Further words of the query, joined with spaces.
        #[arg(value_name = "MORE")]
        more: Vec<String>,
    },
    /// Mark yourself gone.
    Signoff,
    /// Delete threads with no message newer than DUR (human only).
    Prune {
        /// e.g. `7d`, `12h`, `30m`.
        #[arg(long, value_parser = parse_duration, value_name = "DUR")]
        older_than: Duration,
    },
    /// Serve MCP on stdin/stdout (run by harnesses, not by people).
    Mcp,
    /// Run a harness hook: read its payload on stdin and print what the
    /// harness shows the agent (run by harnesses, not by people). Never
    /// starts the daemon; prints nothing when it is down or the session
    /// has no agent.
    Hook {
        /// The harness event the hook runs for.
        #[arg(value_enum)]
        event: HookEvent,
        /// The harness running the hook.
        #[arg(long, value_parser = ["claude-code", "opencode", "kiro"])]
        harness: String,
        /// The harness session the hook is for (opencode's plugin).
        #[arg(long)]
        session: Option<String>,
        /// `status`: mark the urgent mail reported surfaced, so it is
        /// pushed once (opencode's plugin).
        #[arg(long)]
        surface_urgent: bool,
        /// `wait`: report no mail after this many seconds (opencode's
        /// plugin; default 300).
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// Wire agentmail into a harness: its hooks (opencode: its plugin) and
    /// MCP server. Edits the harness's config files, keeping everything else
    /// in them; each file's original is saved once as `<file>.agentmail.bak`.
    Install {
        /// The harness to install into.
        #[arg(long, value_parser = ["claude-code", "opencode"])]
        harness: String,
        /// Install into the current project instead of your user config.
        #[arg(long)]
        project: bool,
        /// Show the changes as a diff without making them.
        #[arg(long)]
        dry_run: bool,
        /// Make agentmail's agent the harness's default (kiro only).
        #[arg(long)]
        set_default: bool,
    },
    /// Remove what `install` added to a harness's config files.
    Uninstall {
        /// The harness to remove agentmail from.
        #[arg(long, value_parser = ["claude-code", "opencode"])]
        harness: String,
        /// Remove it from the current project instead of your user config.
        #[arg(long)]
        project: bool,
        /// Show the changes as a diff without making them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage the background daemon (default: status).
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ContactAction {
    /// List your contacts.
    List,
    /// Add a contact by full address.
    Add {
        /// The contact's full address, e.g. `backend-k3v9x2qa`.
        address: String,
    },
    /// Remove a contact.
    Remove {
        /// The contact's full address.
        address: String,
    },
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub enum DaemonAction {
    /// Serve in the foreground (what autostart runs).
    Run {
        /// If another daemon is running, wait for it to exit and then
        /// serve, instead of exiting at once (for a systemd unit). Also
        /// marks the daemon supervised, so clients that find it gone wait
        /// briefly for its restart rather than starting their own.
        #[arg(long)]
        wait_for_lock: bool,
    },
    /// Start the daemon if it is not running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Report whether the daemon is running. Exits 1 when it is not, 3 when
    /// it speaks another protocol version (stop it to replace it).
    Status,
}

/// One `--to` recipient: refused when empty or only whitespace (as from
/// `--to a,,b` or `--to a,`), so it is a usage error, not a daemon one.
fn parse_recipient(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        return Err("empty recipient: remove the extra comma or give an address".to_string());
    }
    Ok(s.to_string())
}

/// Parse a duration such as `240s`, `10m`, `7d` or `1h30m`. A bare number
/// is seconds. Units: `s`, `m`, `h`, `d`, `w`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration {s:?}: use e.g. 240s, 10m, 12h, 7d");
    let s = s.trim();
    if s.is_empty() {
        return Err(bad());
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().map(Duration::from_secs).map_err(|_| bad());
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit: u64 = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(bad()),
        };
        let n: u64 = num.parse().map_err(|_| bad())?;
        num.clear();
        total = n
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(bad)?;
    }
    if !num.is_empty() {
        return Err(bad());
    }
    Ok(Duration::from_secs(total))
}

/// How much of a body was read: all of it, or more than a stopping point
/// before the end was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Size {
    Exact(u64),
    MoreThan(u64),
}

impl Size {
    fn over_limit(self) -> bool {
        match self {
            Size::Exact(n) => n > MAX_BODY_BYTES as u64,
            Size::MoreThan(n) => n >= MAX_BODY_BYTES as u64,
        }
    }
}

impl std::fmt::Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Size::Exact(n) => write!(f, "{n} bytes"),
            Size::MoreThan(n) => write!(f, "more than {n} bytes"),
        }
    }
}

/// How much of an over-long stdin body is read (and discarded) before
/// giving up: enough that a writer of a modestly oversized body never sees
/// a broken pipe, bounded so an endless one (`yes | agentmail send ...`)
/// is refused instead of hanging.
const STDIN_DRAIN_BYTES: u64 = 8 * 1024 * 1024;

/// The error for a body over the limit.
fn too_large(size: Size) -> anyhow::Error {
    ClientError::new(
        ErrorCode::TooLarge,
        format!(
            "message body is too large: {size}, the limit is {MAX_BODY_BYTES} bytes \
             (256 KiB); nothing was sent"
        ),
    )
    .into()
}

/// Read `r` to its end or until more than `stop_after` bytes have been
/// seen, keeping at most `MAX_BODY_BYTES + 1` bytes. Bytes past the cap are
/// only counted.
fn read_capped(mut r: impl Read, stop_after: u64) -> Result<(Vec<u8>, Size)> {
    let cap = MAX_BODY_BYTES + 1;
    let mut kept = Vec::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Never ask for more than one byte past the stop.
        let want = (stop_after + 1 - total).min(buf.len() as u64) as usize;
        let n = match r.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        total += n as u64;
        let room = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
        if total > stop_after {
            return Ok((kept, Size::MoreThan(stop_after)));
        }
    }
    Ok((kept, Size::Exact(total)))
}

/// A body's bytes as text: refused, never truncated, when it is over the
/// limit; refused when it is not UTF-8.
fn body_text(bytes: Vec<u8>, size: Size) -> Result<String> {
    if size.over_limit() {
        return Err(too_large(size));
    }
    String::from_utf8(bytes).map_err(|e| {
        anyhow!(
            "message body is not valid UTF-8 (at byte {}); nothing was sent",
            e.utf8_error().valid_up_to()
        )
    })
}

/// The body from whichever source was given, byte for byte.
fn read_body(b: &BodyArgs) -> Result<String> {
    if let Some(text) = &b.body {
        return body_text(text.clone().into_bytes(), Size::Exact(text.len() as u64));
    }
    if let Some(path) = &b.body_file {
        let context = || format!("reading body file {}", path.display());
        let file = std::fs::File::open(path).with_context(context)?;
        // A regular file's length is known up front; anything else (a
        // device, a FIFO) is read no further than one byte past the limit.
        let meta = file.metadata().with_context(context)?;
        if meta.is_file() && meta.len() > MAX_BODY_BYTES as u64 {
            return Err(too_large(Size::Exact(meta.len())));
        }
        let (bytes, size) = read_capped(file, MAX_BODY_BYTES as u64).with_context(context)?;
        return body_text(bytes, size);
    }
    let (bytes, size) = read_capped(std::io::stdin().lock(), STDIN_DRAIN_BYTES)
        .context("reading body from stdin")?;
    body_text(bytes, size)
}

/// `p` made absolute against the working directory.
pub(crate) fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// Why a registration binding a process of `sharing` (a harness whose
/// process is shared between sessions) carries none of its sessions, given
/// the session found (`session`) and the environment (`var`). The session
/// variable may be set but not counted: this command does not run under
/// that harness process (a detached job, `--pid` naming an unrelated one)
/// or the harness process carries the value itself, inherited from an
/// outer session. Otherwise it is unset, possibly with a session of another
/// harness (kiro's) instead.
fn no_session_cause(
    sharing: Harness,
    session: Option<&SessionKey>,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> String {
    let h = sharing.as_str();
    let Some(name) = identity::session_var(sharing) else {
        return format!("{h} has no session variable");
    };
    if var(name).is_some_and(|v| !v.is_empty()) {
        return format!(
            "{name} is set but does not name a session of this {h} process: \
             this command is not running in one of its sessions, or the value \
             was inherited from the {h} process's own environment"
        );
    }
    let other = session
        .cloned()
        .or_else(|| identity::session_from_env(&var))
        .filter(|k| k.harness != sharing);
    match other {
        Some(k) => format!(
            "{name} is not set; it carries a {} session, not an {h} one",
            k.harness.as_str()
        ),
        None => format!("{name} is not set"),
    }
}

/// Whether `register` should tell the user to act as the new agent through
/// `AGENTMAIL_ADDRESS`: the agent is bound to no process and no session the
/// CLI resolves callers by. Only sessions of a harness that shares its
/// process (opencode) are used to find the caller; a kiro session is not
/// (see [`identity::cli_caller`]).
fn unbound_hint(proc_id: Option<ProcId>, session: Option<&SessionKey>) -> bool {
    proc_id.is_none() && !session.is_some_and(|k| k.harness.shares_process())
}

/// The harness of a registration in session `k` given an explicit
/// `--harness` of `h`: a session fixes its harness, so any other value is
/// refused here, before the daemon sees it.
fn session_harness(h: Harness, k: &SessionKey) -> Result<Harness> {
    if h != k.harness {
        return Err(ClientError::new(
            ErrorCode::InvalidInput,
            format!(
                "--harness {} conflicts with this {k} session: the harness is \
                 fixed by the session",
                h.as_str(),
                k = k.harness.as_str()
            ),
        )
        .into());
    }
    Ok(h)
}

fn harness_from(s: &str) -> Result<Harness> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| anyhow!("unknown harness {s:?}"))
}

fn parse<T: DeserializeOwned>(v: &Value) -> Result<T> {
    serde_json::from_value(v.clone()).context("unexpected data from daemon")
}

/// Write `text` to stdout as is (it carries its own newlines). A closed
/// stdout (e.g. `| head -c1`) is not an error: the rest is dropped quietly.
fn print_text(text: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(anyhow::Error::new(e).context("writing to stdout"))
        }
        _ => Ok(()),
    }
}

/// Print `line` and a newline, like `println!` but without its panic on a
/// closed stdout.
fn print_line(line: &str) -> Result<()> {
    print_text(&format!("{line}\n"))
}

/// Print `v` as pretty JSON.
fn print_json(v: &Value) -> Result<()> {
    print_line(&serde_json::to_string_pretty(v)?)
}

/// Write `text` to stderr, ignoring a failure: there is nowhere left to
/// report it (and `eprint!` would panic).
fn print_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

/// Print a clap error or help/version text and return its exit code,
/// without `clap::Error::exit`'s panic on a closed stream.
fn clap_exit(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2))
}

/// Parse and run the CLI. Never returns an error: it is printed here.
pub async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // A hook must never fail its harness: Claude Code reads exit 2 as
        // "block", kiro reads 1 as "continue the turn". A hook this binary
        // does not know (a newer config, an older binary) stays silent:
        // it runs on every tool call, and a harness may show hook stderr.
        // Only a person running it by hand sees the usage error.
        Err(e) if e.use_stderr() && is_hook(std::env::args_os().skip(1)) => {
            use std::io::IsTerminal;
            if std::io::stderr().is_terminal() {
                let _ = e.print();
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => return clap_exit(&e),
    };
    let json = cli.json;
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            report(&e, json);
            ExitCode::FAILURE
        }
    }
}

/// Whether the command line (without the program name) runs the `hook`
/// subcommand: its first positional argument, after any global options
/// (`--json`, `--as WHO`) and before a `--`, is `hook`.
fn is_hook(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    use clap::CommandFactory;
    let cmd = Cli::command();
    // Global options that take their value as the next argument.
    let takes_value = |flag: &str| {
        cmd.get_arguments().any(|a| {
            a.get_action().takes_values()
                && (flag
                    .strip_prefix("--")
                    .is_some_and(|l| a.get_long() == Some(l))
                    || (flag.len() == 2
                        && flag.starts_with('-')
                        && flag.chars().nth(1) == a.get_short()))
        })
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(a) = arg.to_str() else {
            return false;
        };
        if a == "--" {
            return false;
        }
        if a.starts_with('-') && a != "-" {
            if !a.contains('=') && takes_value(a) {
                args.next();
            }
            continue;
        }
        return a == "hook";
    }
    false
}

/// Longest hook payload read from stdin, in bytes. Claude Code's tool
/// results can be large; a hook needs only a few fields, and a payload cut
/// short reads as an empty one.
const MAX_HOOK_INPUT: u64 = 16 * 1024 * 1024;

/// `agentmail hook`: read the payload (unless stdin is a terminal), run
/// the hook and print its output. Never fails: see [`crate::hook`].
async fn hook(event: HookEvent, harness: &str, args: &crate::hook::HookArgs<'_>) -> ExitCode {
    use std::io::IsTerminal;
    let mut stdin = Vec::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin()
            .lock()
            .take(MAX_HOOK_INPUT)
            .read_to_end(&mut stdin);
    }
    let harness = harness_from(harness).unwrap_or(Harness::Other);
    let run = crate::hook::run_with(harness, event, args, &stdin, &|k| std::env::var_os(k));
    // opencode's plugin stops a waiting `hook wait` with SIGTERM when its
    // session gets busy. A wake records its mail (the `woken` file) and
    // then prints it; killed in between, the plugin would never hear of a
    // wake agentmail recorded. So SIGTERM is caught for the whole command:
    // while the wait is still waiting it ends the command at once, with
    // nothing recorded or printed, and once the wait has recorded a wake
    // (in the same poll that returns it, with no await in between) the
    // answer is printed first and the signal no longer matters.
    let term = (harness == Harness::Opencode && event == HookEvent::Wait)
        .then(|| tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok())
        .flatten();
    let out = match term {
        Some(mut term) => tokio::select! {
            biased;
            out = run => out,
            _ = term.recv() => return ExitCode::from(143),
        },
        None => run.await,
    };
    if !out.stdout.is_empty() {
        let _ = print_text(&out.stdout);
    }
    if !out.stderr.is_empty() {
        print_err(&out.stderr);
    }
    ExitCode::from(out.exit)
}

/// The name people know a harness by.
fn harness_title(h: Harness) -> &'static str {
    match h {
        Harness::ClaudeCode => "Claude Code",
        Harness::Opencode => "opencode",
        Harness::Kiro => "kiro-cli",
        Harness::Other => "this harness",
    }
}

/// What the user must restart for the harness to pick up `changes`, if
/// anything. opencode needs nothing: its background service watches its
/// config and plugin files and reloads them by itself (a config change
/// emits `config.updated`, reloads the plugins and connects new MCP
/// servers), and restarting that service would interrupt every opencode
/// session on the machine.
fn restart_hint(
    harness: Harness,
    uninstall: bool,
    changes: &[crate::install::Change],
) -> Option<String> {
    if changes.is_empty() || harness == Harness::Opencode {
        return None;
    }
    let what = if uninstall { "unload" } else { "load" };
    let name = harness_title(harness);
    Some(format!("Restart running {name} sessions to {what} it."))
}

/// `agentmail install` / `uninstall`: plan the changes, then show them
/// (`--dry-run`) or make them. Needs no daemon.
fn install(
    harness: &str,
    project: bool,
    dry_run: bool,
    set_default: bool,
    uninstall: bool,
    json: bool,
) -> Result<ExitCode> {
    use crate::install::{self as inst, Env, InstallOpts};
    let harness = harness_from(harness)?;
    let bin = std::env::current_exe().context("locating the agentmail binary")?;
    let o = InstallOpts {
        harness,
        project,
        dry_run,
        set_default,
        bin,
    };
    let env = Env::from_process()?;
    let changes = if uninstall {
        inst::plan_uninstall(&o, &env)?
    } else {
        inst::plan_install(&o, &env)?
    };
    let notes = inst::notes(&o, &env, !uninstall)?;
    if !dry_run {
        inst::apply(&changes)?;
    }
    let action = |c: &inst::Change| match (&c.before, &c.after) {
        (None, _) => "create",
        (Some(_), Some(_)) => "update",
        (Some(_), None) => "delete",
    };
    if json {
        let list: Vec<Value> = changes
            .iter()
            .map(|c| {
                let backup = (c.before.is_some() && c.after.is_some() && !dry_run)
                    .then(|| inst::backup_path(&c.path).display().to_string());
                json!({"path": c.path.display().to_string(), "action": action(c), "backup": backup})
            })
            .collect();
        let restart = (!dry_run)
            .then(|| restart_hint(harness, uninstall, &changes))
            .flatten();
        print_json(
            &json!({"dry_run": dry_run, "changes": list, "restart": restart, "notes": notes}),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let name = harness_title(harness);
    if dry_run {
        for c in &changes {
            print_text(&inst::diff(c))?;
        }
        if changes.is_empty() {
            print_line("nothing to change")?;
        }
        for n in &notes {
            print_line(n)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    for c in &changes {
        let verb = match action(c) {
            "create" => "created",
            "update" => "updated",
            _ => "removed",
        };
        let backup = if c.before.is_some() && c.after.is_some() {
            format!(
                " (original kept as {})",
                inst::backup_path(&c.path).display()
            )
        } else {
            String::new()
        };
        print_line(&format!("{verb} {}{backup}", c.path.display()))?;
    }
    let summary = match (uninstall, changes.is_empty()) {
        (false, true) => format!("agentmail is already installed for {name}."),
        (false, false) => {
            let mut s = format!("agentmail is installed for {name}.");
            if project && harness == Harness::ClaudeCode {
                s.push_str(
                    "\nClaude Code asks you to approve the project's MCP servers (.mcp.json) when it next starts.",
                );
            }
            s
        }
        (true, true) => format!("agentmail is not installed for {name}; nothing to remove."),
        (true, false) => format!("agentmail is removed from {name}."),
    };
    print_line(&summary)?;
    if let Some(hint) = restart_hint(harness, uninstall, &changes) {
        print_line(&hint)?;
    }
    for n in &notes {
        print_line(n)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Print an error as `error: <message>` on stderr, with the candidate table
/// for an ambiguous recipient. With `--json`, also print it as
/// `{"error":{code,message,candidates}}` on stdout.
fn report(e: &anyhow::Error, json: bool) {
    match e.downcast_ref::<ClientError>() {
        Some(ce) => {
            print_err(&format!("error: {}\n", ce.message));
            if !ce.candidates.is_empty() {
                print_err(&render::agents(&ce.candidates, now_ms()));
            }
            if json {
                let _ = print_json(&json!({ "error": ce }));
            }
        }
        None => {
            print_err(&format!("error: {e:#}\n"));
            if json {
                let _ = print_json(&json!({
                    "error": { "code": ErrorCode::Internal, "message": format!("{e:#}"), "candidates": [] }
                }));
            }
        }
    }
}

/// A connection to the daemon (started if needed) and who is calling.
struct Session {
    client: Client,
    caller: Caller,
    json: bool,
}

impl Session {
    async fn call(&mut self, req: Request) -> Result<Value> {
        Ok(self.client.call(&self.caller, req).await?)
    }

    /// Print `data` as JSON, or as `text(data)`.
    fn show(&self, data: &Value, text: impl FnOnce(&Value) -> Result<String>) -> Result<()> {
        if self.json {
            print_json(data)
        } else {
            print_text(&text(data)?)
        }
    }

    /// The full id for `id`: a full id upper-cased (ids are Crockford
    /// base32, so case does not matter); otherwise the one stored message
    /// or thread id ending with it, compared without regard to case. No
    /// match passes `id` through, so the daemon reports it.
    async fn full_id(&mut self, id: &str) -> Result<String> {
        if let Some(full) = normalize_full_id(id) {
            return Ok(full);
        }
        if let Some(e) = too_short_id(id) {
            return Err(e.into());
        }
        let d = self.call(Request::ResolveId { id: id.to_string() }).await?;
        let mut found: Vec<String> = parse(&d)?;
        match found.len() {
            0 => Ok(id.to_string()),
            1 => Ok(found.remove(0)),
            _ => Err(ambiguous_id(id, &found).into()),
        }
    }
}

/// The error for a short id with fewer than `MIN_SHORT_ID` characters.
pub(crate) fn too_short_id(id: &str) -> Option<ClientError> {
    (id.chars().count() < MIN_SHORT_ID).then(|| {
        ClientError::new(
            ErrorCode::InvalidInput,
            format!("id {id:?} is too short; give at least {MIN_SHORT_ID} characters of the id"),
        )
    })
}

/// The error for a short id that ends several stored ids, listing the
/// first `AMBIGUOUS_SHOWN` of them.
pub(crate) fn ambiguous_id(id: &str, found: &[String]) -> ClientError {
    let shown = found[..found.len().min(AMBIGUOUS_SHOWN)].join(", ");
    let more = match found.len().saturating_sub(AMBIGUOUS_SHOWN) {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    ClientError::new(
        ErrorCode::InvalidInput,
        format!(
            "id {id:?} matches {} messages ({shown}{more}); give more of it",
            found.len()
        ),
    )
}

/// What `wait` prints when no mail came within `timeout_secs`, cut to the
/// longest wait the daemon allows, as it cuts the wait itself.
fn wait_timeout_text(timeout_secs: u64) -> String {
    format!(
        "no new mail within {}\n",
        humanize(timeout_secs.min(MAX_WAIT.as_secs()))
    )
}

/// `id` as a full id when it is one (upper-cased), or empty; `None` for a
/// short id that needs looking up.
pub(crate) fn normalize_full_id(id: &str) -> Option<String> {
    (id.len() == ULID_LEN || id.is_empty()).then(|| id.to_ascii_uppercase())
}

async fn run(cli: Cli) -> Result<ExitCode> {
    if let Command::Hook {
        event,
        harness,
        session,
        surface_urgent,
        timeout,
    } = cli.command
    {
        // `--as human` is the human, like `AGENTMAIL_HUMAN=1`: a hook run
        // as the human has nothing to say (`--as` takes no other value),
        // which for opencode's plugin is `{}` / `{"gone":true}`.
        if cli.as_who.is_some() {
            if matches!(harness_from(&harness), Ok(Harness::Opencode)) {
                let out = crate::hook::opencode::unavailable(event);
                if !out.stdout.is_empty() {
                    let _ = print_text(&out.stdout);
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        let args = crate::hook::HookArgs {
            session: session.as_deref(),
            surface_urgent,
            timeout: timeout.map(Duration::from_secs),
        };
        return Ok(hook(event, &harness, &args).await);
    }
    match cli.command {
        Command::Install {
            harness,
            project,
            dry_run,
            set_default,
        } => {
            if set_default && harness != "kiro" {
                use clap::CommandFactory;
                let e = Cli::command().error(
                    clap::error::ErrorKind::ArgumentConflict,
                    format!(
                        "--set-default applies only to --harness kiro; {harness} has no \
                         default agent to set, drop --set-default"
                    ),
                );
                return Ok(clap_exit(&e));
            }
            return install(&harness, project, dry_run, set_default, false, cli.json);
        }
        Command::Uninstall {
            harness,
            project,
            dry_run,
        } => return install(&harness, project, dry_run, false, true, cli.json),
        _ => {}
    }
    let paths = Paths::from_env()?;
    if let Command::Daemon { action } = cli.command {
        return daemon(&paths, action.unwrap_or(DaemonAction::Status), cli.json).await;
    }
    if let Command::Mcp = cli.command {
        crate::mcp::serve(paths).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let caller = identity::cli_caller(cli.as_who.is_some());
    if matches!(cli.command, Command::Register { .. }) && caller == Caller::Human {
        use clap::CommandFactory;
        let e = Cli::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!(
                "register cannot act as {HUMAN}: {HUMAN} is the reserved operator \
                     address and never registers; drop --as {HUMAN} (and unset \
                     AGENTMAIL_HUMAN) to register an agent"
            ),
        );
        return Ok(clap_exit(&e));
    }
    let client = Client::connect_or_spawn(&paths).await?;
    let mut s = Session {
        client,
        caller,
        json: cli.json,
    };
    command(&mut s, cli.command).await
}

/// Exit code for a blocking call's `status`.
fn blocking_exit(data: &Value) -> ExitCode {
    match data["status"].as_str() {
        Some("pending" | "no_one_listening" | "timeout") => ExitCode::from(EXIT_PENDING),
        _ => ExitCode::SUCCESS,
    }
}

/// Human-readable result of `ask`/`await`.
fn render_ask(d: &Value) -> Result<String> {
    let qid = d["question_id"].as_str().unwrap_or_default();
    Ok(match d["status"].as_str().unwrap_or_default() {
        "answered" => {
            let m: Message = parse(&d["answer"])?;
            format!(
                "answered by {}:\n{}",
                render::cell(&m.from),
                render::message(&m)
            )
        }
        "clarification" => {
            let m: Message = parse(&d["message"])?;
            format!(
                "clarification from {} (question {qid} is still open: reply, then \
                 `agentmail await {qid}`):\n{}",
                render::cell(&m.from),
                render::message(&m)
            )
        }
        "pending" => format!(
            "pending: no answer to question {qid} yet. Keep waiting with \
             `agentmail await {qid}`, or carry on and check `agentmail inbox` later.\n"
        ),
        "no_one_listening" => format!(
            "no one listening: every recipient is gone. Question {qid} is stored; \
             check later with `agentmail await {qid}`.\n"
        ),
        other => format!("{other}\n"),
    })
}

fn sent(d: &Value) -> Result<String> {
    let m: Message = parse(d)?;
    let mut out = format!(
        "sent {} to {}\n",
        render::cell(&m.id),
        render::cell(&m.to.join(", "))
    );
    let held: Vec<String> = d
        .get("held_for")
        .map(parse)
        .transpose()?
        .unwrap_or_default();
    if !held.is_empty() {
        out.push_str(&format!(
            "held for {} until an agent with that name registers in this repo\n",
            render::cell(&held.join(", "))
        ));
    }
    Ok(out)
}

async fn command(s: &mut Session, cmd: Command) -> Result<ExitCode> {
    let now = now_ms();
    match cmd {
        Command::Register {
            name,
            role,
            working_on,
            harness,
            fresh,
            no_wake,
            pid,
        } => {
            let bound = identity::register_proc(pid);
            if let (Some(pid), None) = (pid, bound) {
                bail!("process {pid} does not exist or cannot be bound to");
            }
            let (proc_id, detected) = match bound {
                Some((p, h)) => (Some(p), h),
                None => (None, Harness::Other),
            };
            // A session from the environment names its harness: the
            // process found may be another harness further up the tree.
            // Only the session of the harness this process runs under
            // counts, not one inherited from an outer harness, and only
            // when the process bound is that harness: `--pid` naming a
            // process outside it binds that process without the session.
            let session = identity::register_session(pid);
            // A harness that runs every session in one process (opencode)
            // has no identity of its own: binding its process without one
            // of its sessions would make the agent the caller of every
            // session's sessionless calls. A session of another harness
            // (kiro's, with `--pid` naming an opencode process) does not
            // count. The harness is the one of the process picked, PID 1
            // included: opencode as a container's entry point cannot be
            // bound, but is still shared. `--harness` does not change the
            // binding, and `--pid` naming a non-harness ancestor (the
            // shell) is redirected to the opencode process, so only
            // `--pid` naming a process outside any opencode gets past this.
            let sharing = identity::register_harness(pid);
            if sharing.shares_process() && session.as_ref().is_none_or(|k| k.harness != sharing) {
                return Err(ClientError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "this {h} process is shared between sessions and the \
                         registration carries no {h} session ({cause}), so it \
                         cannot register; run register from an {h} session's \
                         shell, or bind a process outside this {h} with --pid",
                        h = sharing.as_str(),
                        cause = no_session_cause(sharing, session.as_ref(), |k| {
                            std::env::var_os(k)
                        }),
                    ),
                )
                .into());
            }
            let hint = unbound_hint(proc_id, session.as_ref());
            let harness = match (harness, &session) {
                (Some(h), Some(k)) => session_harness(harness_from(&h)?, k)?,
                (Some(h), None) => harness_from(&h)?,
                (None, Some(k)) => k.harness,
                (None, None) => detected,
            };
            let req = Request::Register {
                name,
                role,
                working_on,
                harness,
                cwd: std::env::current_dir().context("reading the working directory")?,
                proc_id,
                fresh,
                wake: !no_wake,
                session,
            };
            let d = s.call(req).await?;
            s.show(&d, |d| {
                let address = d["address"].as_str().unwrap_or_default();
                let mut out = format!("registered {address}");
                if d["reclaimed"] == true {
                    out.push_str(" (reclaimed)");
                }
                out.push('\n');
                if let Some(n) = d["released"].as_u64().filter(|n| *n > 0) {
                    out.push_str(&format!("{n} held message(s) delivered to you\n"));
                }
                if hint {
                    out.push_str(&format!(
                        "not bound to a process: set AGENTMAIL_ADDRESS={address} to act as it\n"
                    ));
                }
                Ok(out)
            })?;
        }
        Command::Whoami => {
            let d = s.call(Request::Whoami).await?;
            s.show(&d, |d| {
                if d["agent"].is_null() {
                    return Ok(format!(
                        "{} (acting as the human operator)\n",
                        d["address"].as_str().unwrap_or(HUMAN)
                    ));
                }
                Ok(render::agent(&parse::<AgentView>(&d["agent"])?, now))
            })?;
        }
        Command::Status { working_on } => {
            let d = s.call(Request::Status { working_on }).await?;
            s.show(&d, |d| Ok(render::agent(&parse::<AgentView>(d)?, now)))?;
        }
        Command::Contact { action } => match action.unwrap_or(ContactAction::List) {
            ContactAction::List => {
                let d = s.call(Request::Contacts).await?;
                s.show(&d, |d| {
                    let views: Vec<AgentView> = parse(d)?;
                    Ok(if views.is_empty() {
                        "no contacts\n".to_string()
                    } else {
                        render::agents(&views, now)
                    })
                })?;
            }
            ContactAction::Add { address } => {
                let d = s
                    .call(Request::ContactAdd {
                        address: address.clone(),
                    })
                    .await?;
                s.show(&d, |_| Ok(format!("added contact {address}\n")))?;
            }
            ContactAction::Remove { address } => {
                let d = s
                    .call(Request::ContactRemove {
                        address: address.clone(),
                    })
                    .await?;
                s.show(&d, |_| Ok(format!("removed contact {address}\n")))?;
            }
        },
        Command::Agents { name, repo, all } => {
            let repo = repo.as_deref().map(absolute).transpose()?;
            let d = s
                .call(Request::Agents {
                    name_prefix: name,
                    repo,
                    all,
                })
                .await?;
            s.show(&d, |d| {
                Ok(render::agents(&parse::<Vec<AgentView>>(d)?, now))
            })?;
        }
        Command::Send {
            to,
            subject,
            urgent,
            refs,
            body,
        } => {
            let body = read_body(&body)?;
            let d = s
                .call(Request::Send {
                    to,
                    subject,
                    body,
                    urgent,
                    refs,
                })
                .await?;
            s.show(&d, sent)?;
        }
        Command::Reply {
            id,
            body,
            urgent,
            refs,
            answer,
        } => return reply(s, &id, &body, urgent, refs, answer).await,
        Command::Answer {
            question_id,
            body,
            urgent,
            refs,
        } => return reply(s, &question_id, &body, urgent, refs, true).await,
        Command::Ask {
            to,
            subject,
            body,
            refs,
            urgent,
            timeout,
        } => {
            let body = read_body(&body)?;
            let d = s
                .call(Request::Ask {
                    to,
                    subject,
                    body,
                    refs,
                    urgent,
                    timeout_secs: timeout.as_secs(),
                })
                .await?;
            s.show(&d, render_ask)?;
            return Ok(blocking_exit(&d));
        }
        Command::Await {
            question_id,
            timeout,
        } => {
            let question_id = s.full_id(&question_id).await?;
            let d = s
                .call(Request::AwaitAnswer {
                    question_id,
                    timeout_secs: timeout.as_secs(),
                })
                .await?;
            s.show(&d, render_ask)?;
            return Ok(blocking_exit(&d));
        }
        Command::Wait { timeout } => {
            let d = s
                .call(Request::WaitForMail {
                    timeout_secs: timeout.as_secs(),
                })
                .await?;
            s.show(&d, |d| {
                if d["status"] == "mail" {
                    let msgs: Vec<Message> = parse(&d["messages"])?;
                    Ok(render::thread(&msgs))
                } else {
                    Ok(wait_timeout_text(timeout.as_secs()))
                }
            })?;
            return Ok(blocking_exit(&d));
        }
        Command::Inbox { all } => {
            let d = s.call(Request::Inbox { all }).await?;
            s.show(&d, |d| {
                let rows = d
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|v| Ok((parse::<Message>(v)?, v["from_contact"] == true)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(render::inbox(&rows, now))
            })?;
        }
        Command::Read { id } => {
            let id = s.full_id(&id).await?;
            let d = s.call(Request::Read { id }).await?;
            s.show(&d, |d| match d.get("thread") {
                Some(t) => Ok(render::thread(&parse::<Vec<Message>>(t)?)),
                None => Ok(render::message(&parse::<Message>(d)?)),
            })?;
        }
        Command::Show { id } => {
            let id = s.full_id(&id).await?;
            let mut d = s.call(Request::Read { id }).await?;
            if d.get("thread").is_none() {
                let root = d["thread_id"].as_str().unwrap_or_default().to_string();
                d = s.call(Request::Read { id: root }).await?;
            }
            s.show(&d, |d| {
                Ok(render::thread(&parse::<Vec<Message>>(&d["thread"])?))
            })?;
        }
        Command::Threads {
            agent,
            repo,
            open_questions,
        } => {
            let filter = ThreadFilter {
                agent,
                repo: repo.as_deref().map(absolute).transpose()?,
                open_questions,
            };
            let d = s.call(Request::Threads { filter }).await?;
            s.show(&d, |d| {
                Ok(render::threads(&parse::<Vec<ThreadSummary>>(d)?, now))
            })?;
        }
        Command::Search { query, more } => {
            let query = std::iter::once(query)
                .chain(more)
                .collect::<Vec<_>>()
                .join(" ");
            let d = s.call(Request::Search { query }).await?;
            s.show(&d, |d| {
                Ok(render::message_list(&parse::<Vec<Message>>(d)?, now))
            })?;
        }
        Command::Watch { agent } => return watch(s, agent).await,
        Command::Signoff => {
            let d = s.call(Request::Signoff).await?;
            s.show(&d, |d| {
                Ok(format!(
                    "signed off {}\n",
                    d["address"].as_str().unwrap_or_default()
                ))
            })?;
        }
        Command::Prune { older_than } => {
            let d = s
                .call(Request::Prune {
                    older_than_secs: older_than.as_secs(),
                })
                .await?;
            s.show(&d, |d| {
                Ok(format!(
                    "pruned {} thread(s)\n",
                    d["pruned"].as_u64().unwrap_or(0)
                ))
            })?;
        }
        Command::Daemon { .. }
        | Command::Mcp
        | Command::Hook { .. }
        | Command::Install { .. }
        | Command::Uninstall { .. } => {
            unreachable!("handled in run")
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `reply`, or `answer` (`answer = true`).
async fn reply(
    s: &mut Session,
    id: &str,
    body: &BodyArgs,
    urgent: bool,
    refs: Vec<String>,
    answer: bool,
) -> Result<ExitCode> {
    let body = read_body(body)?;
    let message_id = s.full_id(id).await?;
    let d = s
        .call(Request::Reply {
            message_id,
            body,
            urgent,
            refs,
            answer,
        })
        .await?;
    s.show(&d, sent)?;
    Ok(ExitCode::SUCCESS)
}

/// Print every event until the daemon goes away or stdout closes.
async fn watch(s: &mut Session, agent: Option<String>) -> Result<ExitCode> {
    let json = s.json;
    let caller = s.caller.clone();
    let mut events = Box::pin(s.client.stream(&caller, Request::Watch { agent }).await?);
    let mut out = std::io::stdout();
    while let Some(ev) = events.next().await {
        let ev = ev?;
        let line = if json {
            serde_json::to_string(&ev)?
        } else {
            render::event(&ev, now_ms())
        };
        // A closed stdout (e.g. `| head`) ends the watch quietly.
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            return Ok(ExitCode::SUCCESS);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Seconds as the largest whole unit, e.g. `4m`.
fn humanize(secs: u64) -> String {
    render::age(secs as i64 * 1000, 0)
}

async fn daemon_pid(c: &mut Client) -> Result<u64> {
    let hello = c
        .call(
            &Caller::Human,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await?;
    Ok(hello["pid"].as_u64().unwrap_or(0))
}

/// Print `v` as JSON when `json` is set, else the line `text`.
fn say(json: bool, v: Value, text: &str) -> Result<()> {
    if json {
        print_json(&v)
    } else {
        print_line(text)
    }
}

/// `daemon status` as JSON: whether a daemon answers on the socket, its pid
/// and protocol when it is ours, and why not otherwise.
fn daemon_status_json(
    paths: &Paths,
    running: bool,
    pid: Option<u64>,
    reason: Option<String>,
) -> Value {
    json!({
        "running": running,
        "pid": pid,
        "protocol": pid.map(|_| PROTOCOL_VERSION),
        "socket": paths.socket.display().to_string(),
        "reason": reason,
    })
}

async fn daemon(paths: &Paths, action: DaemonAction, json: bool) -> Result<ExitCode> {
    match action {
        DaemonAction::Run { wait_for_lock } => {
            serve_until(
                DaemonConfig {
                    paths: paths.clone(),
                    wait_for_lock,
                },
                shutdown_signal()?,
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonAction::Start => {
            let mut c = Client::connect_or_spawn(paths).await?;
            let pid = daemon_pid(&mut c).await?;
            say(
                json,
                json!({ "running": true, "pid": pid }),
                &format!("daemon running (pid {pid})"),
            )?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonAction::Status => match Client::connect(paths).await {
            Ok(mut c) => {
                let pid = daemon_pid(&mut c).await?;
                say(
                    json,
                    daemon_status_json(paths, true, Some(pid), None),
                    &format!(
                        "daemon running (pid {pid}, protocol {PROTOCOL_VERSION}, socket {})",
                        paths.socket.display()
                    ),
                )?;
                Ok(ExitCode::SUCCESS)
            }
            Err(e) if e.downcast_ref::<DataDirMismatch>().is_some() => {
                let reason = format!("not for this data directory: {e:#}");
                say(
                    json,
                    daemon_status_json(paths, true, None, Some(reason.clone())),
                    &format!("daemon running, but {reason}"),
                )?;
                Ok(ExitCode::FAILURE)
            }
            Err(e) => match e.downcast_ref::<ClientError>() {
                Some(ce) if ce.code == ErrorCode::VersionMismatch => {
                    let reason = format!(
                        "it speaks a different protocol (this client speaks \
                         {PROTOCOL_VERSION}: {}); run `agentmail daemon stop`",
                        ce.message
                    );
                    say(
                        json,
                        daemon_status_json(paths, true, None, Some(reason.clone())),
                        &format!("daemon running, but {reason}"),
                    )?;
                    Ok(ExitCode::from(STATUS_OTHER_PROTOCOL))
                }
                _ => {
                    let reason = format!("{e:#}");
                    say(
                        json,
                        daemon_status_json(paths, false, None, Some(reason.clone())),
                        &format!("daemon not running ({reason})"),
                    )?;
                    Ok(ExitCode::FAILURE)
                }
            },
        },
        DaemonAction::Stop => {
            // The daemon about to be stopped, by the pid it recorded (see
            // `LockRecord`), if it holds the lock and recorded one.
            let stopping = if paths.lock_held() {
                settled_lock_record(paths).await.map(|r| r.pid)
            } else {
                None
            };
            // Also stops a daemon of another protocol, but never one that
            // serves another data directory through the same socket.
            match Client::shutdown(paths).await {
                Ok(true) => {}
                Ok(false) => {
                    say(
                        json,
                        json!({ "stopped": false, "reason": "daemon not running" }),
                        "daemon not running",
                    )?;
                    return Ok(ExitCode::SUCCESS);
                }
                Err(e) if e.downcast_ref::<DataDirMismatch>().is_some() => {
                    let reason = format!("not for this data directory; not stopped: {e:#}");
                    say(
                        json,
                        json!({ "stopped": false, "reason": format!("daemon running, but {reason}") }),
                        &format!("daemon running, but {reason}"),
                    )?;
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => return Err(e),
            }
            // Wait until it has released the database and lock, so a
            // following start gets a fresh daemon. A successor may take
            // the lock before it is ever seen free (a supervised daemon
            // waiting for it, or a supervisor restarting the daemon at
            // once); it then records itself, so a record of another pid
            // means the daemon we stopped is gone too.
            let deadline = Instant::now() + Duration::from_secs(5);
            while paths.lock_held()
                && !stopping.is_some_and(|pid| paths.lock_record().is_some_and(|r| r.pid != pid))
            {
                if Instant::now() >= deadline {
                    bail!("daemon did not stop within 5s");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            say(
                json,
                json!({ "stopped": true, "reason": null }),
                "daemon stopped",
            )?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hook_finds_the_subcommand_after_global_options() {
        let hook = |args: &[&str]| is_hook(args.iter().map(std::ffi::OsString::from));
        assert!(hook(&["hook", "no-such-event"]));
        assert!(hook(&["--json", "hook", "no-such-event"]));
        assert!(hook(&["--as", "human", "hook", "x"]));
        assert!(hook(&["--as=human", "--json", "hook"]));
        assert!(!hook(&["send", "hook"]));
        assert!(!hook(&["--as", "hook", "send"]));
        assert!(!hook(&["--", "hook"]));
        assert!(!hook(&["--json"]));
        assert!(!hook(&[]));
    }

    #[test]
    fn no_session_cause_names_what_happened() {
        let unset = |_: &str| None;
        let set = |k: &str| (k == identity::ENV_OPENCODE_SESSION).then(|| "s9".into());
        let empty = |k: &str| (k == identity::ENV_OPENCODE_SESSION).then(|| "".into());
        let o = Harness::Opencode;
        assert_eq!(
            no_session_cause(o, None, unset),
            "OPENCODE_SESSION_ID is not set"
        );
        assert_eq!(
            no_session_cause(o, None, empty),
            "OPENCODE_SESSION_ID is not set"
        );
        let ignored = no_session_cause(o, None, set);
        assert!(
            ignored.starts_with("OPENCODE_SESSION_ID is set but"),
            "{ignored}"
        );
        assert!(
            ignored.contains("not running in one of its sessions"),
            "{ignored}"
        );
        assert!(ignored.contains("inherited"), "{ignored}");
        let kiro = SessionKey {
            harness: Harness::Kiro,
            id: "k1".into(),
        };
        let kiro_only = "OPENCODE_SESSION_ID is not set; it carries a kiro session, not an \
                         opencode one";
        assert_eq!(no_session_cause(o, Some(&kiro), unset), kiro_only);
        // A kiro session variable that did not count still explains it.
        let kiro_var = |k: &str| (k == identity::ENV_KIRO_SESSION).then(|| "k1".into());
        assert_eq!(no_session_cause(o, None, kiro_var), kiro_only);
    }

    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_unbound_hint_unless_resolvable() {
        let p = ProcId {
            pid: 42,
            start_time: 7,
        };
        let key = |harness| SessionKey {
            harness,
            id: "s1".into(),
        };
        assert!(unbound_hint(None, None));
        assert!(!unbound_hint(Some(p), None));
        // An opencode session resolves the CLI caller by itself.
        assert!(!unbound_hint(None, Some(&key(Harness::Opencode))));
        // A kiro session does not: CLI calls resolve by process, so an agent
        // with no process is reachable only through AGENTMAIL_ADDRESS.
        assert!(unbound_hint(None, Some(&key(Harness::Kiro))));
        assert!(!unbound_hint(Some(p), Some(&key(Harness::Kiro))));
    }

    #[test]
    fn cli_parse_duration_units() {
        let secs = |s: &str| parse_duration(s).map(|d| d.as_secs());
        assert_eq!(secs("240s"), Ok(240));
        assert_eq!(secs("240"), Ok(240));
        assert_eq!(secs("10m"), Ok(600));
        assert_eq!(secs("12h"), Ok(43_200));
        assert_eq!(secs("7d"), Ok(604_800));
        assert_eq!(secs("2w"), Ok(1_209_600));
        assert_eq!(secs("1h30m"), Ok(5400));
        assert_eq!(secs("0s"), Ok(0));
        assert_eq!(
            secs(DEFAULT_TIMEOUT),
            Ok(crate::model::DEFAULT_ASK_TIMEOUT_SECS)
        );
        for bad in [
            "",
            "s",
            "10x",
            "m10",
            "1.5h",
            "-1s",
            "10m5",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cli_body_limits_are_exact() {
        let at = vec![b'x'; MAX_BODY_BYTES];
        assert_eq!(
            body_text(at.clone(), Size::Exact(at.len() as u64))
                .unwrap()
                .len(),
            MAX_BODY_BYTES
        );
        let err = body_text(
            vec![b'x'; MAX_BODY_BYTES + 1],
            Size::Exact(MAX_BODY_BYTES as u64 + 1),
        )
        .unwrap_err();
        let ce = err.downcast_ref::<ClientError>().unwrap();
        assert_eq!(ce.code, ErrorCode::TooLarge);
        assert!(
            ce.message.contains("too large: 262145 bytes"),
            "{}",
            ce.message
        );
        let err = body_text(vec![], Size::MoreThan(MAX_BODY_BYTES as u64)).unwrap_err();
        let ce = err.downcast_ref::<ClientError>().unwrap();
        assert_eq!(ce.code, ErrorCode::TooLarge);
        assert!(
            ce.message.contains("too large: more than 262144 bytes"),
            "{}",
            ce.message
        );
        assert!(
            body_text(vec![0xff], Size::Exact(1))
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }

    #[test]
    fn cli_read_capped_counts_up_to_the_stop_but_keeps_the_cap() {
        let data = vec![b'a'; MAX_BODY_BYTES * 2 + 7];
        let (kept, size) = read_capped(&data[..], STDIN_DRAIN_BYTES).unwrap();
        assert_eq!(kept.len(), MAX_BODY_BYTES + 1);
        assert_eq!(size, Size::Exact(data.len() as u64));
        let (kept, size) = read_capped(&b"hi\r\n"[..], STDIN_DRAIN_BYTES).unwrap();
        assert_eq!((kept.as_slice(), size), (&b"hi\r\n"[..], Size::Exact(4)));
        // Exactly at the stop is still exact.
        let (_, size) = read_capped(&b"abcd"[..], 4).unwrap();
        assert_eq!(size, Size::Exact(4));
        let (kept, size) = read_capped(&b"abcde"[..], 4).unwrap();
        assert_eq!(size, Size::MoreThan(4));
        assert!(kept.len() <= MAX_BODY_BYTES + 1);
    }

    #[test]
    fn cli_read_capped_stops_on_an_endless_reader() {
        let (kept, size) = read_capped(std::io::repeat(b'y'), MAX_BODY_BYTES as u64).unwrap();
        assert_eq!(kept.len(), MAX_BODY_BYTES + 1);
        assert_eq!(size, Size::MoreThan(MAX_BODY_BYTES as u64));
        let (_, size) = read_capped(std::io::repeat(b'y'), STDIN_DRAIN_BYTES).unwrap();
        assert_eq!(size, Size::MoreThan(STDIN_DRAIN_BYTES));
        assert!(body_text(kept, size).is_err());
    }

    #[test]
    fn cli_body_file_endless_is_refused() {
        let b = BodyArgs {
            body: None,
            body_file: Some(PathBuf::from("/dev/zero")),
            stdin: None,
        };
        let err = read_body(&b).unwrap_err();
        let ce = err.downcast_ref::<ClientError>().unwrap();
        assert_eq!(ce.code, ErrorCode::TooLarge);
        assert!(
            ce.message.contains("more than 262144 bytes"),
            "{}",
            ce.message
        );
        // A large regular file is refused with its exact size.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("big.md");
        std::fs::write(&path, vec![b'x'; MAX_BODY_BYTES + 10]).unwrap();
        let b = BodyArgs {
            body: None,
            body_file: Some(path),
            stdin: None,
        };
        let err = read_body(&b).unwrap_err();
        let ce = err.downcast_ref::<ClientError>().unwrap();
        assert!(
            ce.message.contains("too large: 262154 bytes"),
            "{}",
            ce.message
        );
    }

    #[test]
    fn cli_short_ids_need_four_characters() {
        for id in ["0", "ab", "xyz"] {
            let e = too_short_id(id).unwrap();
            assert_eq!(e.code, ErrorCode::InvalidInput);
            assert!(e.message.contains("at least 4 characters"), "{}", e.message);
        }
        assert!(too_short_id("wxyz").is_none());
        assert!(too_short_id("8wgwzt7p").is_none());
    }

    #[test]
    fn cli_ambiguous_id_lists_a_few_candidates() {
        let found: Vec<String> = (0..1000).map(|i| format!("ID{i:04}")).collect();
        let e = ambiguous_id("0", &found);
        assert_eq!(e.code, ErrorCode::InvalidInput);
        assert!(
            e.message
                .contains("ID0000, ID0001, ID0002, ID0003, ID0004 and 995 more"),
            "{}",
            e.message
        );
        assert!(!e.message.contains("ID0005"), "{}", e.message);
        let e = ambiguous_id("abcd", &found[..2]);
        assert!(e.message.contains("(ID0000, ID0001)"), "{}", e.message);
        assert!(!e.message.contains(" and "), "{}", e.message);
    }

    #[test]
    fn cli_every_argument_has_help_text() {
        fn check(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str();
                if id == "help" || id == "version" {
                    continue;
                }
                if arg.get_help().is_none() && arg.get_long_help().is_none() {
                    missing.push(format!("{path} {id}"));
                }
            }
            for sub in cmd.get_subcommands() {
                check(sub, &format!("{path} {}", sub.get_name()), missing);
            }
        }
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        cmd.build();
        let mut missing = Vec::new();
        check(&cmd, "agentmail", &mut missing);
        assert!(missing.is_empty(), "arguments without help: {missing:?}");
    }

    #[test]
    fn cli_wait_timeout_text_uses_the_effective_wait() {
        assert_eq!(wait_timeout_text(90), "no new mail within 1m\n");
        // The daemon cuts every wait to MAX_WAIT (a week).
        assert_eq!(
            wait_timeout_text(100 * 7 * 24 * 3600),
            "no new mail within 7d\n"
        );
    }

    #[test]
    fn cli_full_ulids_are_upper_cased() {
        assert_eq!(
            normalize_full_id("01m5te0000abcdefghjkmnpqrs"),
            Some("01M5TE0000ABCDEFGHJKMNPQRS".to_string())
        );
        assert_eq!(normalize_full_id("8wgwzt7p"), None);
        assert_eq!(normalize_full_id(""), Some(String::new()));
    }

    #[test]
    fn cli_body_sources_are_exclusive_and_required() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ()).map_err(|e| e.kind());
        use clap::error::ErrorKind;
        assert_eq!(
            parse(&["agentmail", "send", "--to", "a", "--subject", "s"]),
            Err(ErrorKind::MissingRequiredArgument)
        );
        assert_eq!(
            parse(&[
                "agentmail",
                "send",
                "--to",
                "a",
                "--subject",
                "s",
                "--body",
                "x",
                "-"
            ]),
            Err(ErrorKind::ArgumentConflict)
        );
        assert_eq!(parse(&["agentmail", "reply", "id", "-"]), Ok(()));
        assert_eq!(
            parse(&["agentmail", "answer", "id", "--body-file", "f"]),
            Ok(())
        );
    }

    #[test]
    fn cli_to_splits_commas_and_repeats() {
        let cli = Cli::try_parse_from([
            "agentmail",
            "send",
            "--to",
            "a,b",
            "--to",
            "c",
            "--subject",
            "s",
            "--body",
            "x",
        ])
        .unwrap();
        match cli.command {
            Command::Send { to, .. } => assert_eq!(to, ["a", "b", "c"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cli_free_text_may_start_with_a_hyphen() {
        let cli = Cli::try_parse_from([
            "agentmail",
            "send",
            "--to",
            "a",
            "--subject",
            "-x",
            "--body",
            "- a\n- b",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Send { subject, body, .. } => {
                assert_eq!(subject, "-x");
                assert_eq!(body.body.as_deref(), Some("- a\n- b"));
                assert_eq!(body.stdin, None);
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["agentmail", "status", "- foo"]).unwrap();
        match cli.command {
            Command::Status { working_on } => assert_eq!(working_on, "- foo"),
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["agentmail", "status", "--json", "-- bar"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Status { working_on } => assert_eq!(working_on, "-- bar"),
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from([
            "agentmail",
            "register",
            "x",
            "--role",
            "-r",
            "--working-on",
            "- w",
        ])
        .unwrap();
        match cli.command {
            Command::Register {
                role, working_on, ..
            } => {
                assert_eq!(role.as_deref(), Some("-r"));
                assert_eq!(working_on.as_deref(), Some("- w"));
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["agentmail", "search", "-foo", "bar"]).unwrap();
        match cli.command {
            Command::Search { query, more } => assert_eq!(
                (query.as_str(), &more[..]),
                ("-foo", &["bar".to_string()][..])
            ),
            other => panic!("{other:?}"),
        }
        // Known flags after a hyphen-leading value are still flags.
        let cli = Cli::try_parse_from(["agentmail", "search", "-foo", "--json"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Search { query, more } => {
                assert_eq!(query, "-foo");
                assert!(more.is_empty());
            }
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["agentmail", "search", "foo", "bar", "--json"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Search { query, more } => {
                assert_eq!(query, "foo");
                assert_eq!(more, ["bar"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["agentmail", "search"]).is_err());
        let cli = Cli::try_parse_from(["agentmail", "status", "- foo", "--json"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Command::Status { working_on } => assert_eq!(working_on, "- foo"),
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from([
            "agentmail",
            "ask",
            "--to",
            "a",
            "--subject",
            "-q",
            "--body",
            "-",
        ])
        .unwrap();
        match cli.command {
            Command::Ask { subject, body, .. } => {
                assert_eq!(subject, "-q");
                assert_eq!(body.body.as_deref(), Some("-"));
            }
            other => panic!("{other:?}"),
        }
        // The positional `-` still means stdin.
        let cli =
            Cli::try_parse_from(["agentmail", "send", "--to", "a", "--subject", "s", "-"]).unwrap();
        match cli.command {
            Command::Send { body, .. } => assert_eq!(body.stdin.as_deref(), Some("-")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cli_to_rejects_empty_recipients() {
        use clap::error::ErrorKind;
        for to in ["a,,b", "a,", ",a", "", " ", "a, ,b"] {
            let err = Cli::try_parse_from([
                "agentmail",
                "send",
                "--to",
                to,
                "--subject",
                "s",
                "--body",
                "x",
            ])
            .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{to:?}");
            assert_eq!(err.exit_code(), 2, "{to:?}");
        }
        let err = Cli::try_parse_from([
            "agentmail",
            "ask",
            "--to",
            "a,",
            "--subject",
            "s",
            "--body",
            "x",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn cli_global_flags_go_anywhere() {
        let cli = Cli::try_parse_from(["agentmail", "inbox", "--json", "--as", "human"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.as_who.as_deref(), Some("human"));
        assert!(Cli::try_parse_from(["agentmail", "--as", "bob", "inbox"]).is_err());
    }

    #[test]
    fn cli_blocking_exit_codes() {
        let code = |s: &str| blocking_exit(&json!({ "status": s }));
        for s in ["pending", "no_one_listening", "timeout"] {
            assert_eq!(code(s), ExitCode::from(EXIT_PENDING), "{s}");
        }
        for s in ["answered", "clarification", "mail"] {
            assert_eq!(code(s), ExitCode::SUCCESS, "{s}");
        }
    }
}
