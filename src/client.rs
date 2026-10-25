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

    /// Connect to a running daemon. Never starts one.
    pub async fn connect(paths: &Paths) -> Result<Client> {
        let mut c = Client::open(paths)
            .await
            .with_context(|| format!("connecting to {}", paths.socket.display()))?;
        c.hello().await?;
        Ok(c)
    }

    /// Ask the daemon for `paths` to stop, without the `Hello` handshake, so
    /// it works on a daemon of any protocol version. Never starts one.
    ///
    /// Returns false when no daemon is listening (no socket file, or a stale
    /// one nobody accepts on), true once the daemon has acknowledged. The
    /// daemon may still be releasing its lock when this returns.
    pub async fn shutdown(paths: &Paths) -> Result<bool> {
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
    }

    /// Connect, starting the daemon (see [`daemon_bin`]) if none is running.
    pub async fn connect_or_spawn(paths: &Paths) -> Result<Client> {
        Client::connect_or_spawn_with(paths, &daemon_bin()?).await
    }

    /// [`Client::connect_or_spawn`] with an explicit daemon binary.
    ///
    /// A daemon speaking another protocol version is sent `Shutdown` and
    /// replaced, once.
    pub async fn connect_or_spawn_with(paths: &Paths, bin: &Path) -> Result<Client> {
        if let Ok(mut c) = Client::open(paths).await {
            match c.hello().await {
                Ok(_) => return Ok(c),
                Err(e) if e.code == ErrorCode::VersionMismatch => {
                    let _ = c.call(&Caller::Human, Request::Shutdown).await;
                }
                // The daemon is going away; start a new one below.
                Err(e) if e.code == ErrorCode::ConnectionLost => {}
                Err(e) => return Err(e.into()),
            }
        }
        Client::spawn_and_connect(paths, bin).await
    }

    async fn spawn_and_connect(paths: &Paths, bin: &Path) -> Result<Client> {
        let start = Instant::now();
        let mut last_spawn = start;
        spawn_daemon(paths, bin)?;
        let mut last_err = anyhow!("no daemon answered");
        loop {
            match Client::open(paths).await {
                Ok(mut c) => match c.hello().await {
                    Ok(_) => return Ok(c),
                    // An old daemon still shutting down, or a new one dying.
                    Err(e) => last_err = e.into(),
                },
                Err(e) => last_err = e.into(),
            }
            if start.elapsed() >= SPAWN_WAIT {
                return Err(last_err.context(format!(
                    "the agentmail daemon did not start (socket {})",
                    paths.socket.display()
                )));
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
