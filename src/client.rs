//! Client side of the wire protocol, with daemon autostart.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use futures_util::Stream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::paths::{LockRecord, Paths};
pub use crate::protocol::ClientError;
use crate::protocol::{Caller, Envelope, ErrorCode, Event, PROTOCOL_VERSION, Request, Response};
use crate::store::lexical;

/// How often to retry connecting while a daemon starts.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);
/// How long to wait for a spawned daemon.
const SPAWN_WAIT: Duration = Duration::from_secs(3);
/// How long a running daemon may take to accept and answer `Hello` (or
/// `Shutdown`) before the client gives up on it.
const ANSWER_WAIT: Duration = Duration::from_secs(3);
/// How long a client that finds a supervised daemon gone (its lock record
/// says so, see [`LockRecord`]) keeps trying to reach a successor, once the
/// lock is free, before starting a daemon itself: long enough for a
/// supervisor such as systemd (`RestartSec=500ms`) to start a new one, so
/// the client does not race it with an unsupervised daemon.
const SUCCESSOR_WAIT: Duration = Duration::from_secs(2);
/// How long a client waits for a daemon that is going away (it holds the
/// lock but no longer listens) to finish its in-flight requests before
/// going ahead anyway. Requests run commands under a 2s timeout.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// How often to spawn again while waiting. A spawned daemon exits quietly
/// when another holds the lock, e.g. one that is still shutting down.
const RESPAWN_INTERVAL: Duration = Duration::from_millis(500);

/// How much older than the client's first spawn a recorded start-up
/// failure (see [`Paths::startup_error`]) may look and still count as one
/// of the daemons it spawned: file times may be coarser than the clock.
const STARTUP_ERROR_SLACK: Duration = Duration::from_secs(1);

/// How many times to read a lock record that does not decode while the
/// lock is held, [`RECORD_REREAD_INTERVAL`] apart.
const RECORD_READS: u32 = 10;
/// The pause between those reads.
const RECORD_REREAD_INTERVAL: Duration = Duration::from_millis(10);

/// The record in the lock file (see [`Paths::lock_record`]). A daemon that
/// has just taken the lock may be caught rewriting it, the new record
/// followed by the tail of a longer old one; so while the lock is held, a
/// record that does not decode is read again a few times before it counts
/// as none. An empty file counts as none at once: a daemon never empties
/// it, and one from before records were written holds the lock without
/// writing any.
pub async fn settled_lock_record(paths: &Paths) -> Option<LockRecord> {
    let read = || std::fs::read(&paths.lock).ok();
    settled_record(read, || paths.lock_held(), true).await
}

/// The record of a supervised daemon waiting for the lock (see
/// [`Paths::waiter`]), if one waits; read like [`settled_lock_record`].
async fn settled_waiter_record(paths: &Paths) -> Option<LockRecord> {
    if !paths.waiter_held() {
        return None;
    }
    let read = || std::fs::read(paths.waiter()).ok();
    settled_record(read, || paths.waiter_held(), false).await
}

/// The record that `read` returns the bytes of, read again while `held`
/// and it does not decode; see [`settled_lock_record`]. With `empty_is_none`
/// an empty file is none at once. (A waiter file is empty for an instant
/// after a daemon first creates and locks it, so it is read again.)
async fn settled_record(
    read: impl Fn() -> Option<Vec<u8>>,
    held: impl Fn() -> bool,
    empty_is_none: bool,
) -> Option<LockRecord> {
    for _ in 1..RECORD_READS {
        let bytes = read()?;
        if empty_is_none && bytes.is_empty() {
            return None;
        }
        if let Some(rec) = LockRecord::decode(&bytes) {
            return Some(rec);
        }
        if !held() {
            return None;
        }
        tokio::time::sleep(RECORD_REREAD_INTERVAL).await;
    }
    LockRecord::decode(&read()?)
}

/// Whether `rec` is the record of a daemon serving the socket of `paths`.
///
/// The socket itself may be gone (a draining daemon removes it first), so
/// spellings that differ are compared by file name and resolved parent
/// directory, which outlives the socket.
fn serves_our_socket(paths: &Paths, rec: &LockRecord) -> bool {
    let (ours, theirs) = (lexical(&paths.socket), lexical(&rec.socket));
    if ours == theirs {
        return true;
    }
    match (
        ours.parent().zip(ours.file_name()),
        theirs.parent().zip(theirs.file_name()),
    ) {
        (Some((a, name_a)), Some((b, name_b))) => name_a == name_b && same_dir(a, b),
        _ => false,
    }
}

/// A connection to the daemon that has completed the `Hello` handshake.
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Set once [`Client::stream`] has put the connection in watch mode;
    /// the daemon then only pushes events on it, so no further request can
    /// be answered.
    streaming: bool,
}

/// The binary to run as `daemon run`: `AGENTMAIL_BIN` if set and non-empty,
/// else this executable.
pub fn daemon_bin() -> Result<PathBuf> {
    daemon_bin_from(std::env::var_os("AGENTMAIL_BIN"))
}

fn daemon_bin_from(env: Option<OsString>) -> Result<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        Some(bin) => Ok(PathBuf::from(bin)),
        None => std::env::current_exe().context("locating the agentmail executable"),
    }
}

/// Why a daemon spawned at or after `since` failed to start, as it recorded
/// in [`Paths::startup_error`]. An older record is from an earlier attempt
/// and says nothing about this one.
fn fresh_startup_error(paths: &Paths, since: SystemTime) -> Option<String> {
    let path = paths.startup_error();
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    if modified < since.checked_sub(STARTUP_ERROR_SLACK).unwrap_or(since) {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Start `bin daemon run` for `paths`, detached: its own process group, no
/// stdio, working directory `/` (so it never pins the caller's directory).
/// The paths are passed explicitly so the daemon serves exactly the socket
/// this client will connect to.
fn spawn_daemon(paths: &Paths, bin: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(bin)
        .args(["daemon", "run"])
        .env("AGENTMAIL_DATA_DIR", &paths.data_dir)
        .env("AGENTMAIL_SOCKET", &paths.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/")
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {} daemon run", bin.display()))?;
    // Reap it when it exits (at once, if another daemon holds the lock), so
    // a long-lived client does not collect zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Why opening a connection with the `Hello` handshake failed.
enum Handshake {
    /// Nothing accepted the connection.
    Open(std::io::Error),
    /// The peer answered `Hello` with an error.
    Refused(Box<Client>, ClientError),
    /// The peer did not answer in time.
    TimedOut(anyhow::Error),
    /// The daemon at the socket serves another data directory.
    WrongDataDir(anyhow::Error),
}

/// What one attempt to reach a successor found.
enum Successor {
    /// A daemon of our protocol, connected.
    Reached(Client),
    /// Nothing answered, or only a daemon going away did.
    Absent,
    /// A daemon of another protocol version answered `Hello` with this.
    OtherVersion(ClientError),
}

/// The daemon at the socket serves another data directory than the
/// client's (or did not say which one it serves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirMismatch {
    /// The socket both resolve.
    pub socket: PathBuf,
    /// The client's data directory, lexically normalized.
    pub ours: PathBuf,
    /// The daemon's data directory, lexically normalized; `None` when the
    /// daemon did not report one.
    pub theirs: Option<PathBuf>,
}

impl std::fmt::Display for DataDirMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.theirs {
            Some(theirs) => write!(
                f,
                "the daemon at {} serves data directory {}, not {}; set \
                 AGENTMAIL_SOCKET so each data directory has its own socket",
                self.socket.display(),
                theirs.display(),
                self.ours.display()
            ),
            None => write!(
                f,
                "the daemon at {} did not report its data directory (expected {}); \
                 stop it with `agentmail daemon stop`",
                self.socket.display(),
                self.ours.display()
            ),
        }
    }
}

impl std::error::Error for DataDirMismatch {}

/// Whether `a` and `b` name the same directory: first lexically, then, if
/// the spellings differ, after resolving symlinks on both. When either path
/// cannot be resolved the lexical answer stands.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a.to_string_lossy() == b.to_string_lossy() {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Check that the daemon that answered `Hello` with `hello` serves the data
/// directory of `paths`. Both sides are compared after lexical
/// normalization, so different spellings of one directory match; if they
/// still differ, symlinks are resolved before reporting a mismatch.
fn check_data_dir(paths: &Paths, hello: &Value) -> Result<(), DataDirMismatch> {
    let ours = lexical(&paths.data_dir);
    let theirs = hello
        .get("data_dir")
        .and_then(Value::as_str)
        .map(|d| lexical(Path::new(d)));
    match theirs {
        Some(t) if same_dir(&t, &ours) => Ok(()),
        theirs => Err(DataDirMismatch {
            socket: paths.socket.clone(),
            ours,
            theirs,
        }),
    }
}

impl Client {
    /// Open the socket without the handshake.
    async fn open(paths: &Paths) -> std::io::Result<Client> {
        let stream = UnixStream::connect(&paths.socket).await?;
        let (r, w) = stream.into_split();
        Ok(Client {
            reader: BufReader::new(r),
            writer: w,
            streaming: false,
        })
    }

    async fn hello(&mut self) -> Result<Value, ClientError> {
        self.call(
            &Caller::Human,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .await
    }

    /// The error for a peer at the socket that did not answer in time.
    fn no_answer(paths: &Paths, limit: Duration) -> anyhow::Error {
        anyhow!(
            "daemon at {} did not answer within {}ms (is it stopped or wedged, \
             or is another program listening there?)",
            paths.socket.display(),
            limit.as_millis()
        )
    }

    /// Open the socket and complete the handshake, all within `limit`.
    async fn open_and_hello(paths: &Paths, limit: Duration) -> Result<Client, Handshake> {
        let attempt = async {
            let mut c = Client::open(paths).await.map_err(Handshake::Open)?;
            match c.hello().await {
                Ok(hello) => match check_data_dir(paths, &hello) {
                    Ok(()) => Ok(c),
                    Err(e) => Err(Handshake::WrongDataDir(e.into())),
                },
                Err(e) => Err(Handshake::Refused(Box::new(c), e)),
            }
        };
        match tokio::time::timeout(limit, attempt).await {
            Ok(res) => res,
            Err(_) => Err(Handshake::TimedOut(Client::no_answer(paths, limit))),
        }
    }

    /// Connect to a running daemon. Never starts one.
    pub async fn connect(paths: &Paths) -> Result<Client> {
        match Client::open_and_hello(paths, ANSWER_WAIT).await {
            Ok(c) => Ok(c),
            Err(Handshake::Open(e)) => {
                Err(anyhow::Error::new(e)
                    .context(format!("connecting to {}", paths.socket.display())))
            }
            Err(Handshake::Refused(_, e)) => Err(e.into()),
            Err(Handshake::TimedOut(e) | Handshake::WrongDataDir(e)) => Err(e),
        }
    }

    /// Ask the daemon for `paths` to stop. Never starts one.
    ///
    /// Sends `Hello` first, so a daemon serving another data directory
    /// through the same socket is left running and reported as
    /// [`DataDirMismatch`]. A daemon of another protocol version (it answers
    /// `Hello` with `VersionMismatch`), or one that does not report its data
    /// directory, is sent a bare `Shutdown` on the same connection, which
    /// every protocol version honours.
    ///
    /// Returns false when no daemon is listening (no socket file, or a stale
    /// one nobody accepts on), true once the daemon has acknowledged or has
    /// closed the connection in answer to `Shutdown`. A daemon of this
    /// version acknowledges only once it has stopped listening, so a
    /// connection made after this returns never reaches it; it may still be
    /// releasing its lock, though. A peer that does neither
    /// within a few seconds is an error.
    pub async fn shutdown(paths: &Paths) -> Result<bool> {
        let attempt = async {
            let mut c = match Client::open(paths).await {
                Ok(c) => c,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(false);
                }
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("connecting to {}", paths.socket.display())));
                }
            };
            match c.hello().await {
                Ok(hello) => match check_data_dir(paths, &hello) {
                    Ok(()) => {}
                    // A daemon that does not say which directory it serves
                    // cannot be told apart from ours; stop it, as the
                    // mismatch message advises.
                    Err(e) if e.theirs.is_none() => {}
                    Err(e) => return Err(e.into()),
                },
                Err(e) if e.code == ErrorCode::VersionMismatch => {}
                Err(e) => return Err(e.into()),
            }
            match c.call(&Caller::Human, Request::Shutdown).await {
                Ok(_) => Ok(true),
                // Every version honours `Shutdown`: a peer that hangs up
                // instead of replying is going away, as asked.
                Err(e) if e.code == ErrorCode::ConnectionLost => Ok(true),
                Err(e) => Err(e.into()),
            }
        };
        tokio::time::timeout(ANSWER_WAIT, attempt)
            .await
            .unwrap_or_else(|_| Err(Client::no_answer(paths, ANSWER_WAIT)))
    }

    /// Connect, starting the daemon (see [`daemon_bin`]) if none is running.
    pub async fn connect_or_spawn(paths: &Paths) -> Result<Client> {
        Client::connect_or_spawn_with(paths, &daemon_bin()?).await
    }

    /// [`Client::connect_or_spawn`] with an explicit daemon binary.
    ///
    /// A daemon speaking another protocol version is sent `Shutdown` and
    /// replaced, once. A daemon serving another data directory is an error
    /// ([`DataDirMismatch`]): a new daemon would take over its socket. So is
    /// a peer that accepts but does not answer: starting another daemon would
    /// not help, as the silent one either holds the lock or is not a daemon
    /// at all.
    ///
    /// Before starting a daemon, whether because none is listening, it
    /// stopped an old-protocol one, or it found one shutting down, the
    /// client waits for a successor: see [`Client::await_successor`].
    pub async fn connect_or_spawn_with(paths: &Paths, bin: &Path) -> Result<Client> {
        match Client::open_and_hello(paths, ANSWER_WAIT).await {
            Ok(c) => return Ok(c),
            Err(Handshake::Refused(mut c, e)) if e.code == ErrorCode::VersionMismatch => {
                let _ =
                    tokio::time::timeout(ANSWER_WAIT, c.call(&Caller::Human, Request::Shutdown))
                        .await;
                drop(c);
                if let Some(c) = Client::await_successor(paths, true).await? {
                    return Ok(c);
                }
            }
            // The daemon is going away; wait for a successor below.
            Err(Handshake::Refused(_, e)) if e.code == ErrorCode::ConnectionLost => {
                if let Some(c) = Client::await_successor(paths, false).await? {
                    return Ok(c);
                }
            }
            Err(Handshake::Refused(_, e)) => return Err(e.into()),
            Err(Handshake::TimedOut(e) | Handshake::WrongDataDir(e)) => return Err(e),
            // Nothing listening, but a daemon may be on its way.
            Err(Handshake::Open(_)) => {
                if let Some(c) = Client::await_successor(paths, false).await? {
                    return Ok(c);
                }
            }
        }
        Client::spawn_and_connect(paths, bin).await
    }

    /// Wait for a daemon that replaces one going away, so as not to race a
    /// supervisor with an unsupervised daemon. `None` when the client
    /// should start one itself; an error when the successor speaks another
    /// protocol version.
    ///
    /// While a daemon holds the lock for our socket without answering (it
    /// is finishing in-flight requests after removing its socket), wait up
    /// to [`DRAIN_WAIT`] for it to release the lock. Then, only if the last
    /// daemon to hold the lock was supervised, keep trying for up to
    /// [`SUCCESSOR_WAIT`] to reach the one its supervisor starts next. The
    /// same holds when a supervised daemon for our socket is waiting for the
    /// lock (see [`Paths::waiter`]): it takes over from an on-demand daemon.
    /// Without a supervisor (the record says `on-demand`, or there is none,
    /// and no supervised daemon waits), return at once. A lock held for
    /// another socket is a misconfiguration that
    /// [`Client::spawn_and_connect`] reports, so it is not waited for.
    ///
    /// `stopped_other` says the client has just stopped a daemon of another
    /// protocol version. That daemon may answer for a while as it drains
    /// (an older one keeps accepting until it notices), so while the lock
    /// is held its `VersionMismatch` is waited out, up to [`DRAIN_WAIT`],
    /// rather than taken for a successor's.
    async fn await_successor(paths: &Paths, stopped_other: bool) -> Result<Option<Client>> {
        let drain_deadline = Instant::now() + DRAIN_WAIT;
        while paths.lock_held() {
            if !settled_lock_record(paths)
                .await
                .is_some_and(|r| serves_our_socket(paths, &r))
            {
                return Ok(None);
            }
            let left = drain_deadline.saturating_duration_since(Instant::now());
            let other = match Client::try_successor(paths, left.max(RETRY_INTERVAL)).await? {
                Successor::Reached(c) => return Ok(Some(c)),
                Successor::OtherVersion(e) if !stopped_other => {
                    return Err(Client::other_version_error(paths, e));
                }
                Successor::OtherVersion(e) => Some(e),
                Successor::Absent => None,
            };
            if Instant::now() >= drain_deadline {
                return match other {
                    // It never went away: it is no daemon we stopped.
                    Some(e) => Err(Client::other_version_error(paths, e)),
                    None => Ok(None),
                };
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
        // A supervised daemon waiting for the lock lets go of the waiter
        // file only once its record is in the lock file, so look there
        // first.
        let waiter = settled_waiter_record(paths)
            .await
            .is_some_and(|r| serves_our_socket(paths, &r));
        if !waiter
            && !settled_lock_record(paths)
                .await
                .is_some_and(|r| r.supervised && serves_our_socket(paths, &r))
        {
            return Ok(None);
        }
        // The daemon we stopped, if any, has released the lock and with it
        // its socket: a daemon of another version now is a successor.
        let deadline = Instant::now() + SUCCESSOR_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match Client::try_successor(paths, left.max(RETRY_INTERVAL)).await? {
                Successor::Reached(c) => return Ok(Some(c)),
                Successor::OtherVersion(e) => return Err(Client::other_version_error(paths, e)),
                Successor::Absent => {}
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    /// One attempt to reach a daemon within `limit`. A daemon serving
    /// another data directory is an error.
    async fn try_successor(paths: &Paths, limit: Duration) -> Result<Successor> {
        match Client::open_and_hello(paths, limit).await {
            Ok(c) => Ok(Successor::Reached(c)),
            Err(Handshake::WrongDataDir(e)) => Err(e),
            Err(Handshake::Refused(_, e)) if e.code == ErrorCode::VersionMismatch => {
                Ok(Successor::OtherVersion(e))
            }
            Err(Handshake::Open(_) | Handshake::Refused(..) | Handshake::TimedOut(_)) => {
                Ok(Successor::Absent)
            }
        }
    }

    /// The error for a daemon at our socket that answered `Hello` with `e`
    /// (`VersionMismatch`) and stays.
    fn other_version_error(paths: &Paths, e: ClientError) -> anyhow::Error {
        anyhow::Error::new(e).context(Client::other_version(paths))
    }

    /// The message for a daemon at our socket that speaks another protocol
    /// version and stays.
    fn other_version(paths: &Paths) -> String {
        format!(
            "the daemon at {} speaks another protocol version; is another \
             agentmail binary serving it (e.g. a systemd unit's ExecStart, or \
             AGENTMAIL_BIN)? Run the same agentmail everywhere",
            paths.socket.display()
        )
    }

    async fn spawn_and_connect(paths: &Paths, bin: &Path) -> Result<Client> {
        crate::daemon::check_socket_path(&paths.socket)?;
        let deadline = Instant::now() + SPAWN_WAIT;
        let mut last_spawn = Instant::now();
        let first_spawn = SystemTime::now();
        spawn_daemon(paths, bin)?;
        let mut last_err = anyhow!("no daemon answered");
        loop {
            // Never an attempt too short to be answered: its time-out would
            // hide what the daemon there said before.
            let left = deadline.saturating_duration_since(Instant::now());
            match Client::open_and_hello(paths, left.max(RETRY_INTERVAL)).await {
                Ok(c) => return Ok(c),
                // An old daemon still shutting down, or a new one dying.
                Err(Handshake::Refused(_, e)) => last_err = e.into(),
                Err(Handshake::Open(e)) => last_err = e.into(),
                Err(Handshake::TimedOut(e)) => last_err = e,
                Err(Handshake::WrongDataDir(e)) => return Err(e),
            }
            if Instant::now() >= deadline {
                let held = paths.lock_held();
                let ours = held
                    && settled_lock_record(paths)
                        .await
                        .is_some_and(|r| serves_our_socket(paths, &r));
                let other_version = last_err
                    .downcast_ref::<ClientError>()
                    .is_some_and(|e| e.code == ErrorCode::VersionMismatch);
                let msg = if other_version {
                    Client::other_version(paths)
                } else if let Some(why) = fresh_startup_error(paths, first_spawn) {
                    // A daemon we spawned recorded why it could not start
                    // (it has no stderr); one respawned since may still
                    // hold the lock as it fails the same way. That reason
                    // stands alone: the failed connect adds only noise.
                    return Err(anyhow!("the agentmail daemon failed to start: {why}"));
                } else if ours {
                    // It serves our socket but no longer listens: it is
                    // still finishing in-flight requests.
                    format!(
                        "the daemon holding {} is still shutting down (finishing \
                         in-flight requests); try again once it has exited",
                        paths.lock.display()
                    )
                } else if held {
                    // A daemon that holds the lock but is not at our socket
                    // is serving another socket path for the same data
                    // directory.
                    format!(
                        "a daemon holds {} but is not listening on {}; check \
                         AGENTMAIL_SOCKET/XDG_RUNTIME_DIR (every client sharing a \
                         data directory must resolve the same socket)",
                        paths.lock.display(),
                        paths.socket.display()
                    )
                } else {
                    format!(
                        "the agentmail daemon did not start (socket {})",
                        paths.socket.display()
                    )
                };
                return Err(last_err.context(msg));
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
            if last_spawn.elapsed() >= RESPAWN_INTERVAL {
                last_spawn = Instant::now();
                spawn_daemon(paths, bin)?;
            }
        }
    }

    async fn send(&mut self, caller: &Caller, req: Request) -> Result<(), ClientError> {
        if self.streaming {
            return Err(ClientError::new(
                ErrorCode::Internal,
                "connection is in watch mode; open a new client",
            ));
        }
        let env = Envelope {
            caller: caller.clone(),
            req,
        };
        let mut line = serde_json::to_vec(&env)
            .map_err(|e| ClientError::new(ErrorCode::Internal, e.to_string()))?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|_| ClientError::connection_lost())?;
        self.writer
            .flush()
            .await
            .map_err(|_| ClientError::connection_lost())
    }

    async fn recv(&mut self) -> Result<Response, ClientError> {
        let mut line = Vec::new();
        let n = self
            .reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| ClientError::connection_lost())?;
        if n == 0 || line.last() != Some(&b'\n') {
            return Err(ClientError::connection_lost());
        }
        serde_json::from_slice(&line).map_err(|e| {
            ClientError::new(
                ErrorCode::Internal,
                format!("bad response from daemon: {e}"),
            )
        })
    }

    /// Send one request and wait for its result.
    pub async fn call(&mut self, caller: &Caller, req: Request) -> Result<Value, ClientError> {
        self.send(caller, req).await?;
        self.reply().await
    }

    /// Read the reply to the request just sent.
    async fn reply(&mut self) -> Result<Value, ClientError> {
        match self.recv().await? {
            Response::Ok { data } => Ok(data),
            Response::Err {
                code,
                message,
                candidates,
            } => Err(ClientError {
                code,
                message,
                candidates,
            }),
            Response::Event { .. } => Err(ClientError::new(
                ErrorCode::Internal,
                "unexpected event from daemon",
            )),
        }
    }

    /// Send a streaming request (`Watch`) and return its events. The stream
    /// ends after the first error, e.g. [`ErrorCode::ConnectionLost`].
    ///
    /// This puts the connection in watch mode for good: once `stream` has
    /// been called, every later [`Client::call`] or `stream` on this client
    /// fails with [`ErrorCode::Internal`] ("connection is in watch mode"),
    /// even after the returned stream is dropped. Open a new client for
    /// further requests.
    pub async fn stream(
        &mut self,
        caller: &Caller,
        req: Request,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>>, ClientError> {
        self.send(caller, req).await?;
        // From here on the daemon may push events at any time, even if this
        // future is dropped before the acknowledgement arrives.
        self.streaming = true;
        self.reply().await?;
        Ok(futures_util::stream::unfold(
            Some(self),
            |state| async move {
                let c = state?;
                match c.recv().await {
                    Ok(Response::Event { event }) => Some((Ok(event), Some(c))),
                    Ok(Response::Err {
                        code,
                        message,
                        candidates,
                    }) => Some((
                        Err(ClientError {
                            code,
                            message,
                            candidates,
                        }),
                        None,
                    )),
                    Ok(Response::Ok { .. }) => Some((
                        Err(ClientError::new(
                            ErrorCode::Internal,
                            "unexpected response in event stream",
                        )),
                        None,
                    )),
                    Err(e) => Some((Err(e), None)),
                }
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record caught mid-rewrite (a shorter one written over a longer
    /// one) is read again while the lock is held, rather than taken for a
    /// daemon that is not ours.
    #[tokio::test]
    async fn a_record_caught_mid_rewrite_is_read_again() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        let rec = LockRecord {
            supervised: false,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        let mut torn = rec.encode();
        torn.extend_from_slice(b"k\n");
        std::fs::write(&paths.lock, &torn).unwrap();
        // Nobody holds the lock: what is there is all there will be.
        assert_eq!(settled_lock_record(&paths).await, None);

        let lock = std::fs::File::open(&paths.lock).unwrap();
        lock.try_lock().unwrap();
        let fix = std::thread::spawn({
            let path = paths.lock.clone();
            let rec = rec.clone();
            move || {
                std::thread::sleep(Duration::from_millis(5));
                std::fs::write(path, rec.encode()).unwrap();
            }
        });
        assert_eq!(settled_lock_record(&paths).await, Some(rec));
        fix.join().unwrap();
    }

    /// A held lock with an empty lock file is a daemon from before lock
    /// records; there is nothing to wait for, so it is not read again.
    #[tokio::test]
    async fn an_empty_lock_file_is_no_record_at_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::write(&paths.lock, b"").unwrap();
        let lock = std::fs::File::open(&paths.lock).unwrap();
        lock.try_lock().unwrap();
        let began = Instant::now();
        assert_eq!(settled_lock_record(&paths).await, None);
        assert!(
            began.elapsed() < RECORD_REREAD_INTERVAL,
            "read again for {:?}",
            began.elapsed()
        );
    }

    /// A daemon that has removed its socket (it is draining) still counts
    /// as serving ours when the two spell the socket's directory through
    /// different symlinks.
    #[test]
    fn serves_our_socket_resolves_a_symlinked_directory_without_the_socket() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("run");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut paths = Paths::in_dir(tmp.path());
        paths.socket = link.join("agentmail.sock");
        let rec = |socket: PathBuf| LockRecord {
            supervised: false,
            pid: std::process::id(),
            socket,
        };
        assert!(!paths.socket.exists());
        assert!(serves_our_socket(&paths, &rec(real.join("agentmail.sock"))));
        assert!(serves_our_socket(
            &paths,
            &rec(link.join("./agentmail.sock"))
        ));
        assert!(!serves_our_socket(&paths, &rec(real.join("other.sock"))));
        assert!(!serves_our_socket(
            &paths,
            &rec(tmp.path().join("agentmail.sock"))
        ));
    }

    /// A listener at `paths.socket` that answers every `Hello` with
    /// `VersionMismatch` and ignores everything else, like a daemon of
    /// another protocol version that will not go away.
    fn other_version_daemon(paths: &Paths) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(_)) = lines.next_line().await {
                        let resp = Response::Err {
                            code: ErrorCode::VersionMismatch,
                            message: "other version".into(),
                            candidates: Vec::new(),
                        };
                        let mut out = serde_json::to_vec(&resp).unwrap();
                        out.push(b'\n');
                        if w.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        })
    }

    /// When the daemon holding the lock for our socket answers, but speaks
    /// another protocol version, autostart says so rather than that it is
    /// still shutting down.
    #[tokio::test]
    async fn autostart_reports_a_daemon_of_another_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(tmp.path());
        let rec = LockRecord {
            supervised: true,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        std::fs::write(&paths.lock, rec.encode()).unwrap();
        let lock = std::fs::File::open(&paths.lock).unwrap();
        crate::test_util::eventually(|| lock.try_lock()).unwrap();
        let fake = other_version_daemon(&paths);
        // `true daemon run` exits at once, as a daemon finding the lock held
        // would.
        let err = Client::spawn_and_connect(&paths, Path::new("/bin/true"))
            .await
            .err()
            .expect("connected to a daemon of another version");
        let msg = format!("{err:#}");
        assert!(msg.contains("another protocol version"), "{msg}");
        assert!(!msg.contains("still shutting down"), "{msg}");
        fake.abort();
    }

    /// A successor that speaks another protocol version ends the wait for
    /// one at once, with an error that says so.
    #[tokio::test]
    async fn a_successor_of_another_version_is_an_error_at_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(tmp.path());
        let rec = LockRecord {
            supervised: true,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        std::fs::write(&paths.lock, rec.encode()).unwrap();
        let fake = other_version_daemon(&paths);
        let began = Instant::now();
        let err = Client::await_successor(&paths, false)
            .await
            .err()
            .expect("a successor of another version is not ours");
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "waited {:?}",
            began.elapsed()
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("another protocol version"), "{msg}");
        fake.abort();
    }

    /// Answer every request line on `stream` with `reply`, until the peer
    /// hangs up or sends `Shutdown`; whether it sent `Shutdown`.
    async fn answer_until_shutdown(stream: UnixStream, reply: impl Fn() -> Response) -> bool {
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let env: Envelope = serde_json::from_str(&l).unwrap();
            let resp = match env.req {
                Request::Shutdown => Response::Ok { data: Value::Null },
                _ => reply(),
            };
            let mut out = serde_json::to_vec(&resp).unwrap();
            out.push(b'\n');
            if w.write_all(&out).await.is_err() || matches!(env.req, Request::Shutdown) {
                return matches!(env.req, Request::Shutdown);
            }
        }
        false
    }

    /// A daemon of another protocol version that the client stops may
    /// still answer for a moment while it drains (an older daemon keeps
    /// accepting until it notices). Its answers are not taken for a
    /// successor of another version: the client waits for the real one.
    #[tokio::test]
    async fn a_stopped_daemon_of_another_version_is_waited_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(tmp.path());
        let rec = LockRecord {
            supervised: true,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        std::fs::write(&paths.lock, rec.encode()).unwrap();
        let lock = std::fs::File::open(&paths.lock).unwrap();
        crate::test_util::eventually(|| lock.try_lock()).unwrap();
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let old = |_: ()| Response::Err {
            code: ErrorCode::VersionMismatch,
            message: "other version".into(),
            candidates: Vec::new(),
        };
        let old_and_new = tokio::spawn({
            let paths = paths.clone();
            async move {
                // The old daemon: serve until stopped, then keep answering
                // for a while before going away.
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    if answer_until_shutdown(stream, || old(())).await {
                        break;
                    }
                }
                let lingering = Instant::now() + Duration::from_millis(300);
                while let Ok(Ok((stream, _))) =
                    tokio::time::timeout_at(lingering.into(), listener.accept()).await
                {
                    tokio::spawn(answer_until_shutdown(stream, move || old(())));
                }
                drop(listener);
                std::fs::remove_file(&paths.socket).unwrap();
                drop(lock);
                // Its successor, of our protocol.
                let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                let hello = serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "pid": 1,
                    "data_dir": paths.data_dir,
                });
                answer_until_shutdown(stream, move || Response::Ok {
                    data: hello.clone(),
                })
                .await;
            }
        });
        // Starting a daemon would fail: there is no such binary.
        let c = Client::connect_or_spawn_with(&paths, Path::new("/nonexistent/agentmail"))
            .await
            .expect("took the stopped daemon for a successor of another version");
        drop(c);
        old_and_new.await.unwrap();
    }

    #[test]
    fn client_daemon_bin_prefers_env_override() {
        assert_eq!(
            daemon_bin_from(Some("/opt/agentmail".into())).unwrap(),
            PathBuf::from("/opt/agentmail")
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(daemon_bin_from(Some("".into())).unwrap(), exe);
        assert_eq!(daemon_bin_from(None).unwrap(), exe);
    }

    #[test]
    fn check_data_dir_compares_lexically_and_requires_the_field() {
        let paths = Paths::in_dir(Path::new("/data/a"));
        let hello = |d: &str| serde_json::json!({ "version": 1, "pid": 1, "data_dir": d });
        assert_eq!(check_data_dir(&paths, &hello("/data/a")), Ok(()));
        assert_eq!(check_data_dir(&paths, &hello("/data/./b/../a/")), Ok(()));

        let err = check_data_dir(&paths, &hello("/data/b")).unwrap_err();
        assert_eq!(err.theirs, Some(PathBuf::from("/data/b")));
        assert_eq!(err.ours, PathBuf::from("/data/a"));
        let msg = err.to_string();
        assert!(msg.contains("/data/b") && msg.contains("/data/a"), "{msg}");
        assert!(msg.contains("AGENTMAIL_SOCKET"), "{msg}");

        let err = check_data_dir(&paths, &serde_json::json!({ "version": 1 })).unwrap_err();
        assert_eq!(err.theirs, None);
        assert!(err.to_string().contains("did not report"), "{err}");
    }

    #[tokio::test]
    async fn client_refuses_to_spawn_for_an_unbindable_socket() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut paths = Paths::in_dir(tmp.path());
        paths.socket = tmp.path().join("s".repeat(120));
        let bin = Path::new("/nonexistent/agentmail");
        let err = Client::connect_or_spawn_with(&paths, bin)
            .await
            .err()
            .expect("a too-long socket path must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("too long"), "{msg}");
        assert!(msg.contains("AGENTMAIL_SOCKET"), "{msg}");
    }

    #[test]
    fn check_data_dir_resolves_symlinks_when_spellings_differ() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let hello = |d: &Path| serde_json::json!({ "version": 1, "pid": 1, "data_dir": d.to_str().unwrap() });

        // Either side may be the symlink.
        assert_eq!(check_data_dir(&Paths::in_dir(&link), &hello(&real)), Ok(()));
        assert_eq!(check_data_dir(&Paths::in_dir(&real), &hello(&link)), Ok(()));

        // A different real directory still mismatches.
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let err = check_data_dir(&Paths::in_dir(&link), &hello(&other)).unwrap_err();
        assert_eq!(err.theirs, Some(other.clone()));
        assert_eq!(err.ours, link.clone());

        // An unresolvable path falls back to the lexical answer.
        let missing = tmp.path().join("missing");
        let err = check_data_dir(&Paths::in_dir(&missing), &hello(&real)).unwrap_err();
        assert_eq!(err.ours, missing);
    }
}
