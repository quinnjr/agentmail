//! The daemon: the only process that opens the database. Serves the wire
//! protocol (see [`crate::protocol`]) on a Unix socket.

pub mod handler;
pub mod hub;

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{self, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    ReadBuf,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;

use self::handler::{Begun, Handler};
use self::hub::{Hub, Wait, involves};
use crate::paths::{LockRecord, Paths};
use crate::protocol::{
    ClientError, Envelope, ErrorCode, Event, MAX_REQUEST_BYTES, PROTOCOL_VERSION, Request, Response,
};
use crate::store::Store;

/// How often the daemon checks that its socket file is still its own.
const SOCKET_CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// How many times a starting daemon tries to take the lock before deciding
/// another daemon holds it: about half a second in all. A probe holds the
/// lock for an instant, but under load the prober can be descheduled while
/// holding it, and a child that a multi-threaded prober forks shares the
/// lock until it execs; the window is wide enough to outlast both. A daemon
/// that loses to a live one only lingers this long before exiting.
const LOCK_ATTEMPTS: u32 = 25;
/// The pause between those tries.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);
/// How often a daemon run with `wait_for_lock` checks whether the lock it
/// waits for is free.
const LOCK_WAIT_POLL: Duration = Duration::from_millis(100);
/// How long to keep retrying to open the database (a previous daemon in
/// this process may still be releasing it).
const STORE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// How often to log while shutdown waits for in-flight database work.
const STORE_RELEASE_WARN: Duration = Duration::from_secs(5);
/// How long the daemon tries to deliver its `Shutdown` acknowledgements,
/// all of them together, to clients that do not read them before it goes
/// on shutting down.
const SHUTDOWN_ACK_WAIT: Duration = Duration::from_secs(1);

/// The longest path a Unix socket can be bound at: `sun_path` holds 108
/// bytes, including the terminating NUL.
const SUN_PATH_MAX: usize = 107;
/// What staging adds to the socket path: `<parent>/.<name>.d/s`.
const STAGING_EXTRA_BYTES: usize = ".".len() + ".d".len() + "/s".len();
/// The longest socket path the daemon can serve, in bytes.
pub const MAX_SOCKET_PATH_BYTES: usize = SUN_PATH_MAX - STAGING_EXTRA_BYTES;

fn socket_too_long(socket: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "socket path {} is too long ({} bytes; at most {MAX_SOCKET_PATH_BYTES}): \
         set AGENTMAIL_SOCKET or XDG_RUNTIME_DIR to a shorter path",
        socket.display(),
        socket.as_os_str().len(),
    )
}

/// Refuse a socket path the daemon could not bind, so a client can say why
/// before it starts a daemon that would only exit.
pub fn check_socket_path(socket: &Path) -> Result<()> {
    if socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(socket_too_long(socket));
    }
    Ok(())
}

pub struct DaemonConfig {
    pub paths: Paths,
    /// When another daemon holds the lock, wait for it to be released and
    /// then serve, instead of returning at once. For a supervised daemon
    /// (`daemon run --wait-for-lock`), which must not exit just because an
    /// on-demand daemon was already running. The daemon's [`LockRecord`]
    /// then marks it supervised.
    pub wait_for_lock: bool,
}

/// Create `dir` (and missing parents) with mode 0700. An existing
/// directory is left as it is.
fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Identity of a file, to tell our socket from one that replaced it.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

/// Open the store at `path`, retrying for up to [`STORE_OPEN_WAIT`] (a
/// previous daemon in this process may still be releasing it). `None` when
/// `stop` resolves first; it is checked between attempts, so an attempt
/// already running finishes first. Never polls `stop` again once it has
/// resolved.
async fn open_store<F: Future<Output = ()>>(
    path: &Path,
    mut stop: Pin<&mut F>,
) -> Result<Option<Store>> {
    let deadline = Instant::now() + STORE_OPEN_WAIT;
    loop {
        let p = path.to_path_buf();
        let res = tokio::task::spawn_blocking(move || Store::open(&p)).await?;
        match res {
            Ok(store) => return Ok(Some(store)),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::select! {
                biased;
                () = stop.as_mut() => return Ok(None),
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            },
        }
    }
}

/// Write `record` to the lock file we hold, for clients to read once we
/// are gone (see [`LockRecord`]). Best effort: without it clients just do
/// not wait for a supervisor.
///
/// The record is written over the old one before the file is cut to its
/// length, so a client reading meanwhile never finds the file empty. Over
/// a longer old record it may briefly find the new one followed by the
/// old one's tail, which does not decode; clients read again in that case
/// while the lock is held.
fn write_lock_record(lock: &File, path: &Path, record: &LockRecord) {
    use std::os::unix::fs::FileExt;
    let bytes = record.encode();
    let res = lock
        .write_all_at(&bytes, 0)
        .and_then(|()| lock.set_len(bytes.len() as u64));
    if let Err(e) = res {
        eprintln!("agentmail daemon: writing {}: {e}", path.display());
    }
}

/// Bind a listener at `socket` that is never reachable with looser
/// permissions than 0600.
///
/// The socket is bound inside a fresh private (0700) directory next to
/// `socket`, restricted to 0600 there, and only then renamed onto `socket`
/// (replacing any leftover file). Binding directly would create the socket
/// with the process umask, leaving it connectable by others until the
/// chmod. The umask itself is process-wide, so changing it is not an
/// option in a multi-threaded process.
fn bind_private(socket: &Path) -> Result<UnixListener> {
    let parent = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = socket
        .file_name()
        .with_context(|| format!("{} has no file name", socket.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".d");
    let tmp_dir = parent.join(tmp_name);
    let tmp_sock = tmp_dir.join("s");
    if tmp_sock.as_os_str().len() > SUN_PATH_MAX {
        return Err(socket_too_long(socket));
    }

    // A crashed predecessor may have left the staging directory behind.
    // We hold the lock, so nothing else is using it. Never remove more
    // than what we put there.
    match std::fs::remove_file(&tmp_sock) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", tmp_sock.display())),
    }
    match std::fs::remove_dir(&tmp_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", tmp_dir.display())),
    }
    DirBuilder::new()
        .mode(0o700)
        .create(&tmp_dir)
        .with_context(|| format!("creating {}", tmp_dir.display()))?;

    let staged = (|| -> Result<UnixListener> {
        let listener = UnixListener::bind(&tmp_sock)
            .with_context(|| format!("binding {}", tmp_sock.display()))?;
        std::fs::set_permissions(&tmp_sock, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", tmp_sock.display()))?;
        std::fs::rename(&tmp_sock, socket)
            .with_context(|| format!("moving {} to {}", tmp_sock.display(), socket.display()))?;
        Ok(listener)
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp_sock);
    }
    let _ = std::fs::remove_dir(&tmp_dir);
    staged
}

/// Take the exclusive lock on `lock` (the file at `path`). Returns false when
/// another process keeps holding it for [`LOCK_ATTEMPTS`] tries
/// [`LOCK_RETRY_INTERVAL`] apart. The retries let a daemon start while a
/// momentary probe (`Paths::lock_held`, run by `daemon stop` while it waits)
/// holds the lock for an instant.
async fn take_lock(lock: &File, path: &Path) -> Result<bool> {
    for attempt in 1..=LOCK_ATTEMPTS {
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) if attempt < LOCK_ATTEMPTS => {
                tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
    Ok(false)
}

/// Like [`take_lock`], but when another daemon keeps the lock, wait (polling
/// every [`LOCK_WAIT_POLL`]) until it is free instead of giving up. Notes
/// once that it waits.
async fn wait_for_lock(lock: &File, path: &Path) -> Result<bool> {
    if take_lock(lock, path).await? {
        return Ok(true);
    }
    eprintln!(
        "agentmail daemon: another daemon holds {}; waiting for it to exit",
        path.display()
    );
    loop {
        tokio::time::sleep(LOCK_WAIT_POLL).await;
        match lock.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// Take the lock on the waiter file ([`Paths::waiter`]) and write `record`
/// to it, so clients know a supervised daemon waits for the daemon lock.
/// The lock lasts as long as the returned file is open. Best effort: `None`
/// when the file cannot be locked (say, another supervised daemon waits
/// already), and clients then just do not know about us.
async fn announce_waiter(paths: &Paths, record: &LockRecord) -> Option<File> {
    let path = paths.waiter();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path);
    let file = match file {
        Ok(file) => file,
        Err(e) => {
            eprintln!("agentmail daemon: opening {}: {e}", path.display());
            return None;
        }
    };
    match take_lock(&file, &path).await {
        Ok(true) => {
            write_lock_record(&file, &path, record);
            Some(file)
        }
        Ok(false) => None,
        Err(e) => {
            eprintln!("agentmail daemon: {e:#}");
            None
        }
    }
}

/// Whether `stop` has already resolved. Polls it once; only call it again
/// while it returned false.
fn fired<F: Future<Output = ()>>(stop: Pin<&mut F>) -> bool {
    use futures_util::FutureExt;
    stop.now_or_never().is_some()
}

/// Run the daemon until a client sends `Shutdown` or the socket file is
/// removed or replaced. See [`serve_until`].
pub async fn serve(cfg: DaemonConfig) -> Result<()> {
    serve_until(cfg, std::future::pending()).await
}

/// Resolve on the first SIGTERM or SIGINT. Both handlers are installed
/// before this returns, so a signal sent once it has returned is never
/// missed.
///
/// On resolving it logs that a signal arrived and in-flight requests are
/// finishing, so the journal shows why the daemon stops. Once it has
/// resolved, a second SIGTERM or SIGINT exits the process at
/// once with status 1, without waiting for in-flight requests: the way out
/// of a shutdown that is stuck. The commands those requests run (`git`)
/// are killed first rather than orphaned. Acknowledged writes are durable
/// either way, and the lock goes with the process.
pub fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        eprintln!("agentmail daemon: signal received; finishing in-flight requests");
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            eprintln!(
                "agentmail daemon: second signal; exiting without waiting for \
                 in-flight requests"
            );
            handler::kill_running_commands();
            std::process::exit(1);
        });
    })
}

/// Run the daemon like [`serve`], but also stop when `stop` resolves (e.g.
/// [`shutdown_signal`]), taking the same clean path as a `Shutdown` request.
///
/// Startup: create the data directory (mode 0700) and take an exclusive
/// lock on `paths.lock`, retrying briefly so a momentary probe of the lock
/// cannot make the daemon give up. If another daemon keeps holding it,
/// return `Ok` without touching anything: that daemon serves (with
/// `wait_for_lock`, wait until it is free instead). Otherwise open the store
/// (so a database that cannot be opened never leaves a socket behind),
/// then put a mode-0600 socket in place of any leftover one (no live daemon
/// can own it while we hold the lock) and accept. Every fallible startup
/// step runs before the socket appears, so a daemon that fails to start
/// never leaves a socket without a listener behind. A `stop` that resolves
/// during startup, including while the store open is retried, ends it
/// cleanly before the socket is bound.
///
/// Once it holds the lock the daemon records in the lock file whether it
/// is supervised (`wait_for_lock`) and which socket it serves (see
/// [`LockRecord`]), so clients that find it gone know whether to wait for
/// a supervisor to start the next one. While it waits for the lock, a
/// daemon with `wait_for_lock` holds [`Paths::waiter`] and records itself
/// there, so clients that stop the daemon it waits behind wait for it.
///
/// However the serve loop ends, `stop` keeps being polled while in-flight
/// requests finish, so a [`shutdown_signal`] during that wait arms its
/// second-signal exit as it does when a signal started the shutdown.
pub async fn serve_until(cfg: DaemonConfig, stop: impl Future<Output = ()>) -> Result<()> {
    let paths = cfg.paths;
    let mut stop = std::pin::pin!(stop);
    create_private_dir(&paths.data_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock)
        .with_context(|| format!("opening {}", paths.lock.display()))?;
    let record = LockRecord {
        supervised: cfg.wait_for_lock,
        pid: std::process::id(),
        socket: paths.socket.clone(),
    };
    let locked = async {
        if cfg.wait_for_lock {
            // Announce ourselves while we wait, and until the lock record
            // says we hold the lock.
            let waiter = announce_waiter(&paths, &record).await;
            (wait_for_lock(&lock, &paths.lock).await, waiter)
        } else {
            (take_lock(&lock, &paths.lock).await, None)
        }
    };
    let (locked, waiter) = tokio::select! {
        biased;
        () = &mut stop => return Ok(()),
        locked = locked => locked,
    };
    if !locked? {
        return Ok(());
    }
    write_lock_record(&lock, &paths.lock, &record);
    drop(waiter);

    let Some(store) = open_store(&paths.db, stop.as_mut()).await? else {
        return Ok(());
    };
    // Stopped while starting: leave before publishing a socket that would
    // only close again. The store is ours alone, so the lock can go with it.
    if fired(stop.as_mut()) {
        drop(store);
        drop(lock);
        return Ok(());
    }
    let store = Arc::new(store);

    if let Some(parent) = paths.socket.parent() {
        create_private_dir(parent)?;
    }
    let listener = bind_private(&paths.socket)?;
    let Some(socket_id) = file_id(&paths.socket) else {
        // Someone removed it already; there is nothing of ours to clean up.
        anyhow::bail!("{} vanished after bind", paths.socket.display());
    };

    let store_ref = Arc::downgrade(&store);
    let handler = Arc::new(Handler::new(store, Hub::new()));
    let data_dir: Arc<Path> = Arc::from(paths.data_dir.as_path());

    // Connections that were asked to `Shutdown` hand over the writer for
    // the acknowledgement, which is sent only once the daemon has stopped
    // listening: a client that connects after it reaches a successor.
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel::<OwnedWriteHalf>();
    let mut acks = Vec::new();
    // Whether `stop` has resolved, after which it must not be polled again.
    let mut stopped = false;
    let mut conns = JoinSet::new();
    let mut check = tokio::time::interval(SOCKET_CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Stops come first, so no connection is accepted once one is due.
        tokio::select! {
            biased;
            Some(w) = stop_rx.recv() => {
                acks.push(w);
                break;
            }
            () = &mut stop => {
                stopped = true;
                break;
            }
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    conns.spawn(connection(
                        stream,
                        handler.clone(),
                        data_dir.clone(),
                        stop_tx.clone(),
                    ));
                }
                Err(e) => {
                    eprintln!("agentmail daemon: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = check.tick() => {
                if file_id(&paths.socket) != Some(socket_id) {
                    break;
                }
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    // Stop taking connections first, so new clients start a successor.
    // Connections still in the listen backlog are reset with the listener.
    if file_id(&paths.socket) == Some(socket_id) {
        let _ = std::fs::remove_file(&paths.socket);
    }
    drop(listener);
    // Now acknowledge every `Shutdown` asked for so far. Later ones are
    // answered by the connection closing at once, which clients count as
    // the same.
    while let Ok(w) = stop_rx.try_recv() {
        acks.push(w);
    }
    drop(stop_rx);
    acknowledge_shutdowns(acks, SHUTDOWN_ACK_WAIT).await;
    let mut drain = std::pin::pin!(async move {
        conns.shutdown().await;
        drop(handler);
        release_store(store_ref, lock, STORE_RELEASE_WARN).await;
    });
    if !stopped {
        tokio::select! {
            () = drain.as_mut() => return Ok(()),
            () = stop.as_mut() => {}
        }
    }
    drain.await;
    Ok(())
}

/// Acknowledge each `Shutdown` on its writer in `acks`, all at once, and
/// give up on those still unsent after `limit`: however many clients do
/// not read their reply, they hold up the shutdown only that long.
async fn acknowledge_shutdowns<W: AsyncWrite + Unpin>(acks: Vec<W>, limit: Duration) {
    let ok = Response::Ok { data: Value::Null };
    let writes = acks.into_iter().map(|mut w| {
        let ok = &ok;
        async move {
            let _ = write_response(&mut w, ok).await;
        }
    });
    let _ = tokio::time::timeout(limit, futures_util::future::join_all(writes)).await;
}

/// Wait until the store is closed, then release the lock.
///
/// Requests still running on blocking threads hold the store after the
/// connections are gone. The lock is kept until the last of them drops
/// it, however long that takes: a successor takes the lock only when it
/// can open the database. Every request is bounded (child processes run
/// under a timeout), so this ends. A note is logged every `warn_every`
/// while it waits.
async fn release_store(store: Weak<Store>, lock: File, warn_every: Duration) {
    let started = Instant::now();
    let mut next_warn = started + warn_every;
    while store.strong_count() > 0 {
        if Instant::now() >= next_warn {
            eprintln!(
                "agentmail daemon: waiting for in-flight requests to finish ({:.1}s)",
                started.elapsed().as_secs_f64()
            );
            next_warn += warn_every;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(lock);
}

/// Why a request line could not be read.
enum Frame {
    Line(Vec<u8>),
    Eof,
    TooLong,
}

async fn read_frame(reader: &mut Input<BufReader<OwnedReadHalf>>) -> io::Result<Frame> {
    let mut buf = Vec::new();
    let limit = (MAX_REQUEST_BYTES + 1) as u64;
    let n = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(Frame::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_REQUEST_BYTES {
        return Ok(Frame::TooLong);
    }
    Ok(Frame::Line(buf))
}

async fn write_response<W: AsyncWrite + Unpin>(w: &mut W, resp: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(resp).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

fn to_response(res: Result<Value, ClientError>) -> Response {
    match res {
        Ok(data) => Response::Ok { data },
        Err(e) => e.into_response(),
    }
}

/// Serve one connection. `data_dir` is reported in the `Hello` answer, so a
/// client can tell a daemon for another data directory that shares its
/// socket path.
async fn connection(
    stream: UnixStream,
    handler: Arc<Handler>,
    data_dir: Arc<Path>,
    stop: mpsc::UnboundedSender<OwnedWriteHalf>,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = Input::new(BufReader::new(r));
    let mut greeted = false;
    loop {
        let line = match read_frame(&mut reader).await {
            Ok(Frame::Line(line)) => line,
            Ok(Frame::Eof) | Err(_) => return,
            Ok(Frame::TooLong) => {
                let err = ClientError::new(
                    ErrorCode::TooLarge,
                    format!("request is larger than {MAX_REQUEST_BYTES} bytes"),
                );
                let _ = write_response(&mut w, &err.into_response()).await;
                return;
            }
        };
        let env: Envelope = match serde_json::from_slice(&line) {
            Ok(env) => env,
            Err(e) => {
                let err = ClientError::new(ErrorCode::InvalidInput, format!("bad request: {e}"));
                if write_response(&mut w, &err.into_response()).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let res = match env.req {
            Request::Hello { version } if version == PROTOCOL_VERSION => {
                greeted = true;
                Ok(json!({
                    "version": PROTOCOL_VERSION,
                    "pid": std::process::id(),
                    "data_dir": data_dir.to_string_lossy(),
                }))
            }
            Request::Hello { version } => {
                // A failed handshake revokes an earlier successful one.
                greeted = false;
                Err(ClientError::new(
                    ErrorCode::VersionMismatch,
                    format!(
                        "client speaks protocol {version}, daemon speaks {PROTOCOL_VERSION}; \
                     restart the daemon"
                    ),
                ))
            }
            Request::Shutdown => {
                // The serve loop acknowledges once it has stopped listening.
                let _ = stop.send(w);
                return;
            }
            _ if !greeted => Err(ClientError::new(
                ErrorCode::VersionMismatch,
                format!("send hello (protocol {PROTOCOL_VERSION}) first"),
            )),
            Request::Watch { agent } => {
                watch_events(&mut reader, &mut w, handler.hub(), agent.as_deref()).await;
                return;
            }
            req @ (Request::Ask { .. }
            | Request::AwaitAnswer { .. }
            | Request::WaitForMail { .. }) => {
                match blocking_call(&mut reader, &handler, env.caller, req).await {
                    Some(res) => res,
                    // The client hung up while waiting.
                    None => return,
                }
            }
            req => {
                let h = handler.clone();
                let caller = env.caller;
                tokio::task::spawn_blocking(move || h.handle(&caller, req))
                    .await
                    .unwrap_or_else(|e| Err(ClientError::new(ErrorCode::Internal, e.to_string())))
            }
        };
        if write_response(&mut w, &to_response(res)).await.is_err() {
            return;
        }
    }
}

/// Run `f` on the handler on a blocking thread.
async fn on_blocking_thread<T: Send + 'static>(
    handler: &Arc<Handler>,
    f: impl FnOnce(&Handler) -> Result<T, ClientError> + Send + 'static,
) -> Result<T, ClientError> {
    let h = handler.clone();
    tokio::task::spawn_blocking(move || f(&h))
        .await
        .unwrap_or_else(|e| Err(ClientError::new(ErrorCode::Internal, e.to_string())))
}

/// Serve a blocking request (`Ask`, `AwaitAnswer`, `WaitForMail`): run its
/// immediate part, then, if it has to wait, wait on the hub without holding
/// a blocking thread. `None` when the client hung up meanwhile; there is
/// then no one to answer.
async fn blocking_call<R>(
    reader: &mut Input<R>,
    handler: &Arc<Handler>,
    caller: crate::protocol::Caller,
    req: Request,
) -> Option<Result<Value, ClientError>>
where
    R: AsyncBufRead + Unpin,
{
    let begun = on_blocking_thread(handler, move |h| h.begin(&caller, req)).await;
    match begun {
        Ok(Begun::Done(data)) => Some(Ok(data)),
        Ok(Begun::Wait { rx, wait, timeout }) => wait_for(reader, handler, rx, wait, timeout).await,
        Err(e) => Some(Err(e)),
    }
}

/// How much input a client may send ahead while a call of its waits: more
/// than any one request can be. Past this the wait ends with an error.
const MAX_READ_AHEAD: usize = MAX_REQUEST_BYTES + 1;

/// A connection's input: whatever a client sent ahead while a blocking call
/// of its waited (read early, to notice a hangup behind it), served first,
/// then the rest of the stream.
struct Input<R> {
    inner: R,
    ahead: Vec<u8>,
    /// How much of `ahead` has been served.
    pos: usize,
    /// The most `ahead` may hold unserved before a wait gives up.
    limit: usize,
}

impl<R> Input<R> {
    fn new(inner: R) -> Input<R> {
        Input::with_limit(inner, MAX_READ_AHEAD)
    }

    fn with_limit(inner: R, limit: usize) -> Input<R> {
        Input {
            inner,
            ahead: Vec::new(),
            pos: 0,
            limit,
        }
    }

    fn unserved(&self) -> &[u8] {
        &self.ahead[self.pos..]
    }

    fn serve(&mut self, n: usize) {
        self.pos += n;
        if self.pos >= self.ahead.len() {
            self.ahead.clear();
            self.pos = 0;
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Input<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let ahead = this.unserved();
        if ahead.is_empty() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let n = ahead.len().min(buf.remaining());
        buf.put_slice(&ahead[..n]);
        this.serve(n);
        Poll::Ready(Ok(()))
    }
}

impl<R: AsyncBufRead + Unpin> AsyncBufRead for Input<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.pos < this.ahead.len() {
            return Poll::Ready(Ok(&this.ahead[this.pos..]));
        }
        Pin::new(&mut this.inner).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        if this.pos < this.ahead.len() {
            this.serve(amt);
        } else {
            Pin::new(&mut this.inner).consume(amt);
        }
    }
}

/// Why a wait stopped watching its client's input.
enum Interrupt {
    /// The client hung up (or the connection failed).
    Closed,
    /// The client sent more than its input's read-ahead limit.
    Overflow,
}

/// Read what the client sends while a call of its waits into the read-ahead
/// buffer, kept for its next requests, until it hangs up or sends too much.
/// Reading on past early input is what lets a hangup behind it end the wait.
async fn watch_input<R: AsyncBufRead + Unpin>(input: &mut Input<R>) -> Interrupt {
    loop {
        // `fill_buf` is cancel safe, and nothing below awaits: a wait that
        // ends meanwhile loses no input.
        let n = match input.inner.fill_buf().await {
            Ok([]) | Err(_) => return Interrupt::Closed,
            Ok(buf) => {
                input.ahead.extend_from_slice(buf);
                buf.len()
            }
        };
        input.inner.consume(n);
        if input.unserved().len() > input.limit {
            return Interrupt::Overflow;
        }
    }
}

/// Wait until `wait` is over (checked against the store on every relevant
/// event, and after any events were dropped for lagging), `timeout` runs
/// out, or the client hangs up (`None`).
async fn wait_for<R>(
    reader: &mut Input<R>,
    handler: &Arc<Handler>,
    mut rx: broadcast::Receiver<Event>,
    mut wait: Wait,
    timeout: Duration,
) -> Option<Result<Value, ClientError>>
where
    R: AsyncBufRead + Unpin,
{
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let input = watch_input(reader);
    tokio::pin!(input);
    loop {
        tokio::select! {
            biased;
            interrupt = &mut input => return match interrupt {
                Interrupt::Closed => None,
                Interrupt::Overflow => Some(Err(ClientError::new(
                    ErrorCode::TooLarge,
                    format!(
                        "more than {MAX_REQUEST_BYTES} bytes of input arrived while a call \
                         waited; send the next request after the answer"
                    ),
                ))),
            },
            () = &mut deadline => return Some(Ok(Handler::timed_out(&wait))),
            ev = rx.recv() => match ev {
                Ok(event) if !wait.wakes(&event) => continue,
                // A lag may have dropped the very event we wait for.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Some(Err(ClientError::new(
                        ErrorCode::Internal,
                        "the daemon is shutting down",
                    )));
                }
            },
        }
        let checked = on_blocking_thread(handler, move |h| {
            let done = h.check(&mut wait)?;
            Ok((wait, done))
        })
        .await;
        match checked {
            Ok((_, Some(data))) => return Some(Ok(data)),
            Ok((w, None)) => wait = w,
            Err(e) => return Some(Err(e)),
        }
    }
}

/// Acknowledge a watch and stream hub events to the client until it
/// disconnects.
async fn watch_events(
    reader: &mut Input<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    hub: &Hub,
    agent: Option<&str>,
) {
    // Subscribe before acknowledging, so nothing published after the
    // client sees `Ok` is missed.
    let rx = hub.subscribe();
    if write_response(w, &Response::Ok { data: Value::Null })
        .await
        .is_err()
    {
        return;
    }
    stream_events(reader, w, rx, agent).await;
}

/// Forward events from `rx` (those involving `agent`, or all) to `w` until
/// the client disconnects or the hub closes.
///
/// A watcher that falls so far behind that the hub drops events for it is
/// told so with an `Internal` error and the stream ends: a silent gap would
/// look like nothing happened. The client can watch again and re-read the
/// store for what it missed.
async fn stream_events<R, W>(
    reader: &mut R,
    w: &mut W,
    mut rx: broadcast::Receiver<Event>,
    agent: Option<&str>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut scratch = [0u8; 1024];
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(event) => {
                    if agent.is_some_and(|a| !involves(&event, a)) {
                        continue;
                    }
                    if write_response(w, &Response::Event { event }).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let err = ClientError::new(
                        ErrorCode::Internal,
                        format!("watch fell behind; {n} events dropped"),
                    );
                    let _ = write_response(w, &err.into_response()).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            n = reader.read(&mut scratch) => match n {
                // Input during a watch is ignored; EOF or an error ends it.
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, AgentView, Harness, Presence};
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, duplex};

    /// Clients that asked for `Shutdown` but do not read the reply hold up
    /// the acknowledgements, together, for no longer than the limit, and
    /// do not keep the others from theirs.
    #[tokio::test]
    async fn shutdown_acks_share_one_deadline() {
        let limit = Duration::from_millis(300);
        // Writers whose peers never read: nothing past one byte gets out.
        let mut stuck_peers = Vec::new();
        let mut acks = Vec::new();
        for _ in 0..5 {
            let (w, peer) = duplex(1);
            acks.push(w);
            stuck_peers.push(peer);
        }
        let (w, reader) = duplex(4096);
        acks.push(w);

        let began = Instant::now();
        acknowledge_shutdowns(acks, limit).await;
        let took = began.elapsed();
        assert!(took < limit * 3, "took {took:?}");

        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).await.unwrap();
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert!(matches!(resp, Response::Ok { .. }), "{line}");
        drop(stuck_peers);
    }

    /// Rewriting the lock record, over one of the same length or another,
    /// never leaves the file empty for a client reading it meanwhile. (Over
    /// a longer record a reader may see a torn one; clients read again in
    /// that case.)
    #[test]
    fn rewriting_the_lock_record_never_leaves_the_file_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        let short = LockRecord {
            supervised: false,
            pid: std::process::id(),
            socket: paths.socket.clone(),
        };
        let long = LockRecord {
            supervised: true,
            pid: std::process::id(),
            socket: dir
                .path()
                .join("a-much-longer-socket-name-than-the-other.sock"),
        };
        assert_ne!(short.encode().len(), long.encode().len());
        std::fs::write(&paths.lock, short.encode()).unwrap();
        let lock = OpenOptions::new().write(true).open(&paths.lock).unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = std::thread::spawn({
            let done = done.clone();
            let path = paths.lock.clone();
            move || {
                for rec in [&short, &long].into_iter().cycle() {
                    if done.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    write_lock_record(&lock, &path, rec);
                }
            }
        });
        let until = Instant::now() + Duration::from_millis(300);
        let mut empty = false;
        while !empty && Instant::now() < until {
            empty = std::fs::read(&paths.lock).unwrap().is_empty();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(!empty, "read an empty lock file");
    }

    fn presence(address: &str) -> Event {
        Event::Presence {
            agent: AgentView {
                agent: Agent {
                    address: address.into(),
                    name: "a".into(),
                    role: None,
                    working_on: None,
                    harness: Harness::Other,
                    cwd: PathBuf::from("/w"),
                    repo: None,
                    proc_id: None,
                    wake: false,
                    session: None,
                    registered_at: 0,
                    last_seen: 0,
                    signed_off: false,
                },
                presence: Presence::Active,
                is_contact: false,
            },
        }
    }

    /// A slow request still holding the store when the daemon shuts down
    /// must keep the lock held until it finishes, however long that takes,
    /// so the successor that takes the lock can always open the database.
    #[tokio::test]
    async fn lock_is_held_until_a_slow_request_releases_the_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let lock = std::fs::File::create(&paths.lock).unwrap();
        lock.lock().unwrap();
        let store = Arc::new(Store::open(&paths.db).unwrap());
        let weak = Arc::downgrade(&store);

        // An in-flight request that outlives shutdown, and the warning
        // interval, several times over.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let slow = tokio::task::spawn_blocking(move || {
            let _held = store;
            done_rx.recv().ok();
        });
        let release = tokio::spawn(release_store(weak, lock, Duration::from_millis(20)));

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !release.is_finished(),
            "released while the store was in use"
        );
        let probe = std::fs::File::open(&paths.lock).unwrap();
        assert!(
            matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
            "the lock was given up while the store was still open"
        );

        done_tx.send(()).unwrap();
        slow.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), release)
            .await
            .expect("release did not finish once the store was dropped")
            .unwrap();
        // The successor takes the lock and opens the store. Both checks
        // retry briefly: a child another test thread is forking can share
        // our lock (and database) file descriptions until it execs, so
        // they may look held for a moment after release.
        crate::test_util::eventually(|| probe.try_lock()).expect("lock still held after release");
        crate::test_util::eventually(|| Store::open(&paths.db))
            .expect("successor could not open the store");
    }

    /// A future that is pending when first polled (waking at once) and
    /// ready from then on: a stop that fires just after the lock is taken.
    struct ReadyOnSecondPoll(bool);

    impl Future for ReadyOnSecondPoll {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<()> {
            if self.0 {
                return Poll::Ready(());
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn config(paths: &Paths, wait_for_lock: bool) -> DaemonConfig {
        DaemonConfig {
            paths: paths.clone(),
            wait_for_lock,
        }
    }

    /// A stop that fires while the daemon is starting ends it before it
    /// binds: the (stale) file at the socket path is never replaced, and
    /// the lock is released.
    #[tokio::test]
    async fn stop_during_startup_never_binds_the_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::write(&paths.socket, b"stale").unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            serve_until(config(&paths, false), ReadyOnSecondPoll(false)),
        )
        .await
        .expect("serve_until hung")
        .unwrap();
        assert_eq!(std::fs::read(&paths.socket).unwrap(), b"stale");
        let probe = std::fs::File::open(&paths.lock).unwrap();
        crate::test_util::eventually(|| probe.try_lock()).expect("lock still held");
    }

    /// With `wait_for_lock`, a daemon that finds the lock held waits for it
    /// instead of exiting, and serves once it is free.
    #[tokio::test]
    async fn wait_for_lock_serves_once_the_lock_is_released() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let holder = std::fs::File::create(&paths.lock).unwrap();
        holder.lock().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve_until(config(&paths, true), async move {
            let _ = stop_rx.await;
        }));
        // Well past the attempts a daemon makes without waiting.
        tokio::time::sleep(LOCK_RETRY_INTERVAL * LOCK_ATTEMPTS * 3).await;
        assert!(!task.is_finished(), "gave up on a held lock");
        assert!(!paths.socket.exists(), "bound without the lock");

        holder.unlock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !paths.socket.exists() {
            assert!(!task.is_finished(), "{:?}", task.await.unwrap());
            assert!(
                Instant::now() < deadline,
                "never served after the lock was freed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("did not stop")
            .unwrap()
            .unwrap();
        assert!(!paths.socket.exists());
    }

    /// A stop while waiting for the lock ends the wait at once, without
    /// serving.
    #[tokio::test]
    async fn stop_while_waiting_for_the_lock_returns_without_serving() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let holder = std::fs::File::create(&paths.lock).unwrap();
        holder.lock().unwrap();
        let wait = LOCK_RETRY_INTERVAL * LOCK_ATTEMPTS * 3;
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            serve_until(config(&paths, true), tokio::time::sleep(wait)),
        )
        .await
        .expect("stop did not end the wait for the lock")
        .unwrap();
        assert!(started.elapsed() >= wait);
        assert!(!paths.socket.exists());
    }

    /// A handler over a fresh store, with one agent that has sent `human`
    /// a message when `with_mail`.
    fn handler_with(dir: &tempfile::TempDir, with_mail: bool) -> Arc<Handler> {
        use crate::model::Kind;
        use crate::store::{NewMessage, RegisterReq};
        let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
        if with_mail {
            let req = RegisterReq {
                name: "a".into(),
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
            let (agent, _) = store.register(req, 1, &|_| false).unwrap();
            let msg = NewMessage {
                from: agent.address,
                to: vec![crate::model::HUMAN.into()],
                subject: Some("s".into()),
                body: "b".into(),
                kind: Kind::Note,
                urgent: false,
                refs: vec![],
                in_reply_to: None,
            };
            store.insert_message(msg, 2).unwrap();
        }
        Arc::new(Handler::new(Arc::new(store), Hub::new()))
    }

    fn human_mail() -> Wait {
        Wait::Mail {
            addr: crate::model::HUMAN.into(),
        }
    }

    #[tokio::test]
    async fn wait_ends_without_an_answer_when_the_client_hangs_up() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, false);
        let (client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        let (_tx, rx) = broadcast::channel(4);
        drop(client);
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(
                &mut reader,
                &handler,
                rx,
                human_mail(),
                Duration::from_secs(60),
            ),
        )
        .await
        .expect("wait outlived its client");
        assert!(res.is_none(), "{res:?}");
    }

    #[tokio::test]
    async fn input_during_a_wait_is_kept_for_the_next_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, false);
        let (mut client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        let (_tx, rx) = broadcast::channel(4);
        client.write_all(b"next\n").await.unwrap();
        let res = wait_for(
            &mut reader,
            &handler,
            rx,
            human_mail(),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(res, Some(Ok(json!({ "status": "timeout" }))));
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "next\n");
    }

    /// Input sent during a wait does not hide a hangup that follows it:
    /// the wait still ends at once.
    #[tokio::test]
    async fn hangup_after_input_during_a_wait_ends_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, false);
        let (mut client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        let (_tx, rx) = broadcast::channel(4);
        let hangup = tokio::spawn(async move {
            client.write_all(b"next\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            client.write_all(b"junk").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(client);
        });
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(
                &mut reader,
                &handler,
                rx,
                human_mail(),
                Duration::from_secs(60),
            ),
        )
        .await
        .expect("wait outlived its client");
        assert!(res.is_none(), "{res:?}");
        hangup.await.unwrap();
    }

    /// Input read ahead during a wait is served in order, before anything
    /// sent later, across waits.
    #[tokio::test]
    async fn input_read_ahead_keeps_its_order_across_waits() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, false);
        let (mut client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        for line in [&b"one\n"[..], b"two\n"] {
            let (_tx, rx) = broadcast::channel(4);
            client.write_all(line).await.unwrap();
            let res = wait_for(
                &mut reader,
                &handler,
                rx,
                human_mail(),
                Duration::from_millis(50),
            )
            .await;
            assert_eq!(res, Some(Ok(json!({ "status": "timeout" }))));
        }
        client.write_all(b"three\n").await.unwrap();
        let mut got = Vec::new();
        for _ in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            got.push(line);
        }
        assert_eq!(got, ["one\n", "two\n", "three\n"]);
    }

    /// A client that sends more during a wait than could be one request
    /// ends the wait with an error rather than growing the buffer without
    /// bound; what it sent is still served next.
    #[tokio::test]
    async fn too_much_input_during_a_wait_ends_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, false);
        let (mut client, daemon_in) = duplex(64);
        let mut reader = Input::with_limit(BufReader::new(daemon_in), 8);
        let (_tx, rx) = broadcast::channel(4);
        client.write_all(b"0123456789\n").await.unwrap();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(
                &mut reader,
                &handler,
                rx,
                human_mail(),
                Duration::from_secs(60),
            ),
        )
        .await
        .expect("wait did not end on too much input");
        let err = res.unwrap().unwrap_err();
        assert_eq!(err.code, ErrorCode::TooLarge, "{err:?}");
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "0123456789\n");
    }

    /// Events dropped for lagging may include the one waited for, so a
    /// lag makes the wait check the store.
    #[tokio::test]
    async fn lagged_wait_checks_the_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, true);
        let (_client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        let (tx, rx) = broadcast::channel(1);
        for i in 0..3 {
            tx.send(presence(&format!("x-{i}"))).unwrap();
        }
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for(
                &mut reader,
                &handler,
                rx,
                human_mail(),
                Duration::from_secs(60),
            ),
        )
        .await
        .expect("lag did not end the wait");
        let data = res.unwrap().unwrap();
        assert_eq!(data["status"], "mail");
        assert_eq!(data["messages"].as_array().unwrap().len(), 1);
    }

    /// Irrelevant events do not end a wait, even when the store would.
    #[tokio::test]
    async fn irrelevant_events_do_not_check_the_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, true);
        let (_client, daemon_in) = duplex(64);
        let mut reader = Input::new(BufReader::new(daemon_in));
        let (tx, rx) = broadcast::channel(8);
        tx.send(presence("x-1")).unwrap();
        let res = wait_for(
            &mut reader,
            &handler,
            rx,
            human_mail(),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(res, Some(Ok(json!({ "status": "timeout" }))));
    }

    #[tokio::test]
    async fn lagged_watch_reports_dropped_events_and_ends() {
        let (tx, rx) = broadcast::channel(2);
        for i in 0..5 {
            tx.send(presence(&format!("a-{i}"))).unwrap();
        }
        // The client end stays open and silent; only the lag ends the stream.
        let (_client_in, mut daemon_in) = duplex(64);
        let (mut daemon_out, client_out) = duplex(64 * 1024);
        tokio::time::timeout(
            Duration::from_secs(5),
            stream_events(&mut daemon_in, &mut daemon_out, rx, None),
        )
        .await
        .expect("stream did not end after lagging");
        drop(daemon_out);

        let mut lines = BufReader::new(client_out).lines();
        let first: Response =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        match first {
            Response::Err { code, message, .. } => {
                assert_eq!(code, ErrorCode::Internal);
                assert!(message.contains("3 events dropped"), "{message}");
            }
            other => panic!("expected a lag error, got {other:?}"),
        }
        assert!(lines.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stream_forwards_filtered_events_until_eof() {
        let (tx, rx) = broadcast::channel(16);
        tx.send(presence("x-1")).unwrap();
        tx.send(presence("y-1")).unwrap();
        let (client_in, mut daemon_in) = duplex(64);
        let (mut daemon_out, client_out) = duplex(64 * 1024);
        let task = tokio::spawn(async move {
            stream_events(&mut daemon_in, &mut daemon_out, rx, Some("y-1")).await;
        });
        let mut lines = BufReader::new(client_out).lines();
        let first: Response =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            first,
            Response::Event {
                event: presence("y-1")
            }
        );
        drop(client_in);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("stream did not end at EOF")
            .unwrap();
        drop(tx);
    }
}
