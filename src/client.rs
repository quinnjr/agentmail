//! Client side of the wire protocol, with daemon autostart.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures_util::Stream;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::paths::Paths;
pub use crate::protocol::ClientError;
use crate::protocol::{Caller, Envelope, ErrorCode, Event, PROTOCOL_VERSION, Request, Response};

/// How often to retry connecting while a daemon starts.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);
/// How long to wait for a spawned daemon.
const SPAWN_WAIT: Duration = Duration::from_secs(3);
/// How long a running daemon may take to accept and answer `Hello` (or
/// `Shutdown`) before the client gives up on it.
const ANSWER_WAIT: Duration = Duration::from_secs(3);
/// How often to spawn again while waiting. A spawned daemon exits quietly
/// when another holds the lock, e.g. one that is still shutting down.
const RESPAWN_INTERVAL: Duration = Duration::from_millis(500);

/// A connection to the daemon that has completed the `Hello` handshake.
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
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
}

impl Client {
    /// Open the socket without the handshake.
    async fn open(paths: &Paths) -> std::io::Result<Client> {
        let stream = UnixStream::connect(&paths.socket).await?;
        let (r, w) = stream.into_split();
        Ok(Client {
            reader: BufReader::new(r),
            writer: w,
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
                Ok(_) => Ok(c),
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
            Err(Handshake::TimedOut(e)) => Err(e),
        }
    }

    /// Ask the daemon for `paths` to stop, without the `Hello` handshake, so
    /// it works on a daemon of any protocol version. Never starts one.
    ///
    /// Returns false when no daemon is listening (no socket file, or a stale
    /// one nobody accepts on), true once the daemon has acknowledged. The
    /// daemon may still be releasing its lock when this returns. A peer that
    /// does not acknowledge within a few seconds is an error.
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
            c.call(&Caller::Human, Request::Shutdown).await?;
            Ok(true)
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
    /// replaced, once. A peer that accepts but does not answer is an error:
    /// starting another daemon would not help, as the silent one either
    /// holds the lock or is not a daemon at all.
    pub async fn connect_or_spawn_with(paths: &Paths, bin: &Path) -> Result<Client> {
        match Client::open_and_hello(paths, ANSWER_WAIT).await {
            Ok(c) => return Ok(c),
            Err(Handshake::Refused(mut c, e)) if e.code == ErrorCode::VersionMismatch => {
                let _ =
                    tokio::time::timeout(ANSWER_WAIT, c.call(&Caller::Human, Request::Shutdown))
                        .await;
            }
            // The daemon is going away; start a new one below.
            Err(Handshake::Refused(_, e)) if e.code == ErrorCode::ConnectionLost => {}
            Err(Handshake::Refused(_, e)) => return Err(e.into()),
            Err(Handshake::TimedOut(e)) => return Err(e),
            // Nothing listening: start a daemon below.
            Err(Handshake::Open(_)) => {}
        }
        Client::spawn_and_connect(paths, bin).await
    }

    async fn spawn_and_connect(paths: &Paths, bin: &Path) -> Result<Client> {
        let deadline = Instant::now() + SPAWN_WAIT;
        let mut last_spawn = Instant::now();
        spawn_daemon(paths, bin)?;
        let mut last_err = anyhow!("no daemon answered");
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match Client::open_and_hello(paths, left).await {
                Ok(c) => return Ok(c),
                // An old daemon still shutting down, or a new one dying.
                Err(Handshake::Refused(_, e)) => last_err = e.into(),
                Err(Handshake::Open(e)) => last_err = e.into(),
                Err(Handshake::TimedOut(e)) => last_err = e,
            }
            if Instant::now() >= deadline {
                // A daemon that holds the lock but is not at our socket is
                // serving another socket path for the same data directory.
                let msg = if paths.lock_held() {
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
    pub async fn stream(
        &mut self,
        caller: &Caller,
        req: Request,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>>, ClientError> {
        self.call(caller, req).await?;
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
}
