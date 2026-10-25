//! The daemon: the only process that opens the database. Serves the wire
//! protocol (see [`crate::protocol`]) on a Unix socket.

pub mod handler;
pub mod hub;

use std::fs::{DirBuilder, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

use self::handler::Handler;
use self::hub::{Hub, involves};
use crate::paths::Paths;
use crate::protocol::{
    ClientError, Envelope, ErrorCode, MAX_REQUEST_BYTES, PROTOCOL_VERSION, Request, Response,
};
use crate::store::Store;

/// How often the daemon checks that its socket file is still its own.
const SOCKET_CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// How long to keep retrying to open the database (a previous daemon in
/// this process may still be releasing it).
const STORE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// How long to wait for in-flight database work after shutdown.
const STORE_RELEASE_WAIT: Duration = Duration::from_secs(5);

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

/// Run the daemon until a client sends `Shutdown` or the socket file is
/// removed or replaced.
///
/// Startup: create the data directory (mode 0700) and take an exclusive
/// lock on `paths.lock`. If another daemon holds it, return `Ok` at once
/// without touching anything: that daemon serves. Otherwise remove any
/// leftover socket file (no live daemon can own it while we hold the lock),
/// bind, restrict the socket to mode 0600, open the store and accept.
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
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", paths.lock.display()));
        }
    }

    if let Some(parent) = paths.socket.parent() {
        create_private_dir(parent)?;
    }
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing {}", paths.socket.display()));
        }
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
    let socket_id = file_id(&paths.socket).context("socket vanished after bind")?;

    let store = Arc::new(open_store(&paths.db).await?);
    let store_ref = Arc::downgrade(&store);
    let handler = Arc::new(Handler::new(store, Hub::new()));

    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut conns = JoinSet::new();
    let mut check = tokio::time::interval(SOCKET_CHECK_INTERVAL);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    conns.spawn(connection(stream, handler.clone(), stop_tx.clone()));
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
    // Close the database before releasing the lock, so the successor that
    // takes the lock can open it.
    let deadline = Instant::now() + STORE_RELEASE_WAIT;
    while store_ref.strong_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(lock);
    Ok(())
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

async fn write_response(w: &mut OwnedWriteHalf, resp: &Response) -> io::Result<()> {
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

async fn connection(stream: UnixStream, handler: Arc<Handler>, stop: watch::Sender<bool>) {
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
                Ok(json!({ "version": PROTOCOL_VERSION, "pid": std::process::id() }))
            }
            Request::Hello { version } => Err(ClientError::new(
                ErrorCode::VersionMismatch,
                format!(
                    "client speaks protocol {version}, daemon speaks {PROTOCOL_VERSION}; \
                     restart the daemon"
                ),
            )),
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

/// Stream hub events to the client until it disconnects.
async fn watch_events(
    reader: &mut BufReader<OwnedReadHalf>,
    w: &mut OwnedWriteHalf,
    hub: &Hub,
    agent: Option<&str>,
) {
    // Subscribe before acknowledging, so nothing published after the
    // client sees `Ok` is missed.
    let mut rx = hub.subscribe();
    if write_response(w, &Response::Ok { data: Value::Null })
        .await
        .is_err()
    {
        return;
    }
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
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
