//! The daemon: the only process that opens the database. Serves the wire
//! protocol (see [`crate::protocol`]) on a Unix socket.

pub mod handler;
pub mod hub;

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

use self::handler::{Begun, Handler};
use self::hub::{Hub, Wait, involves};
use crate::paths::Paths;
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
/// How long to keep retrying to open the database (a previous daemon in
/// this process may still be releasing it).
const STORE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// How often to log while shutdown waits for in-flight database work.
const STORE_RELEASE_WARN: Duration = Duration::from_secs(5);

/// The longest path a Unix socket can be bound at: `sun_path` holds 108
/// bytes, including the terminating NUL.
const SUN_PATH_MAX: usize = 107;
/// What staging adds to the socket path: `<parent>/.<name>.d/s`.
const STAGING_EXTRA_BYTES: usize = ".".len() + ".d".len() + "/s".len();
/// The longest socket path the daemon can serve, in bytes.
pub const MAX_SOCKET_PATH_BYTES: usize = SUN_PATH_MAX - STAGING_EXTRA_BYTES;

pub struct DaemonConfig {
    pub paths: Paths,
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

async fn open_store(path: &Path) -> Result<Store> {
    let deadline = Instant::now() + STORE_OPEN_WAIT;
    loop {
        let p = path.to_path_buf();
        let res = tokio::task::spawn_blocking(move || Store::open(&p)).await?;
        match res {
            Ok(store) => return Ok(store),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
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
        anyhow::bail!(
            "socket path {} is too long ({} bytes; at most {MAX_SOCKET_PATH_BYTES}): \
             set AGENTMAIL_SOCKET or XDG_RUNTIME_DIR to a shorter path",
            socket.display(),
            socket.as_os_str().len(),
        );
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

/// Run the daemon until a client sends `Shutdown` or the socket file is
/// removed or replaced.
///
/// Startup: create the data directory (mode 0700) and take an exclusive
/// lock on `paths.lock`, retrying briefly so a momentary probe of the lock
/// cannot make the daemon give up. If another daemon keeps holding it,
/// return `Ok` without touching anything: that daemon serves. Otherwise open the store
/// (so a database that cannot be opened never leaves a socket behind),
/// then put a mode-0600 socket in place of any leftover one (no live daemon
/// can own it while we hold the lock) and accept. Every fallible startup
/// step runs before the socket appears, so a daemon that fails to start
/// never leaves a socket without a listener behind.
pub async fn serve(cfg: DaemonConfig) -> Result<()> {
    let paths = cfg.paths;
    create_private_dir(&paths.data_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock)
        .with_context(|| format!("opening {}", paths.lock.display()))?;
    if !take_lock(&lock, &paths.lock).await? {
        return Ok(());
    }

    let store = Arc::new(open_store(&paths.db).await?);

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

    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut conns = JoinSet::new();
    let mut check = tokio::time::interval(SOCKET_CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
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
            _ = stop_rx.changed() => break,
            _ = check.tick() => {
                if file_id(&paths.socket) != Some(socket_id) {
                    break;
                }
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }

    // Stop taking connections first, so new clients start a successor.
    if file_id(&paths.socket) == Some(socket_id) {
        let _ = std::fs::remove_file(&paths.socket);
    }
    drop(listener);
    conns.shutdown().await;
    drop(handler);
    release_store(store_ref, lock, STORE_RELEASE_WARN).await;
    Ok(())
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

async fn read_frame(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Frame> {
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
    stop: watch::Sender<bool>,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
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
                let _ = write_response(&mut w, &Response::Ok { data: Value::Null }).await;
                let _ = stop.send(true);
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
    reader: &mut R,
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

/// Resolve once the peer has closed its end (or the connection failed).
/// Input that arrives instead is left buffered for the next request, and
/// this then never resolves.
async fn peer_closed<R: AsyncBufRead + Unpin>(reader: &mut R) {
    match reader.fill_buf().await {
        Ok([]) | Err(_) => {}
        Ok(_) => std::future::pending().await,
    }
}

/// Wait until `wait` is over (checked against the store on every relevant
/// event, and after any events were dropped for lagging), `timeout` runs
/// out, or the client hangs up (`None`).
async fn wait_for<R>(
    reader: &mut R,
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
    let closed = peer_closed(reader);
    tokio::pin!(closed);
    loop {
        tokio::select! {
            biased;
            () = &mut closed => return None,
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
    reader: &mut BufReader<OwnedReadHalf>,
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
        let mut reader = BufReader::new(daemon_in);
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
        let mut reader = BufReader::new(daemon_in);
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

    /// Events dropped for lagging may include the one waited for, so a
    /// lag makes the wait check the store.
    #[tokio::test]
    async fn lagged_wait_checks_the_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let handler = handler_with(&dir, true);
        let (_client, daemon_in) = duplex(64);
        let mut reader = BufReader::new(daemon_in);
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
        let mut reader = BufReader::new(daemon_in);
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
