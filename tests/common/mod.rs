//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentmail::client::Client;
use agentmail::daemon::{DaemonConfig, serve};
use agentmail::model::Harness;
use agentmail::paths::Paths;
use agentmail::protocol::{Caller, Request};
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// The built `agentmail` binary, for tests that spawn a real daemon.
pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentmail"))
}

/// A daemon served in-process from a fresh temporary directory. Dropping it
/// aborts the daemon.
pub struct TestDaemon {
    pub dir: TempDir,
    pub paths: Paths,
    handle: Option<JoinHandle<()>>,
}

impl TestDaemon {
    /// Start a daemon and wait until it accepts connections.
    pub async fn start() -> TestDaemon {
        TestDaemon::start_in(TempDir::new().unwrap()).await
    }

    /// Start a daemon serving from `dir`, which may already hold files.
    pub async fn start_in(dir: TempDir) -> TestDaemon {
        let paths = Paths::in_dir(dir.path());
        let mut daemon = TestDaemon {
            dir,
            paths,
            handle: None,
        };
        daemon.restart().await;
        daemon
    }

    /// Serve again from the same directory (after [`TestDaemon::shutdown`]
    /// or [`TestDaemon::kill`]).
    pub async fn restart(&mut self) {
        assert!(self.handle.is_none(), "daemon is still running");
        let spawn = |paths: Paths| {
            tokio::spawn(async move {
                serve(DaemonConfig {
                    paths,
                    wait_for_lock: false,
                })
                .await
                .expect("daemon failed");
            })
        };
        let mut handle = spawn(self.paths.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Client::connect(&self.paths).await.is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not start");
            if handle.is_finished() {
                // `serve` returns at once when the lock is held. Right after
                // a shutdown that can be a child process another test is
                // spawning: between fork and exec it shares our lock file
                // description. Try again once it has exec'd.
                handle.await.unwrap();
                handle = spawn(self.paths.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.handle = Some(handle);
    }

    /// A connected client.
    pub async fn client(&self) -> Client {
        Client::connect(&self.paths).await.unwrap()
    }

    /// Send `Shutdown` and wait for the daemon to finish.
    pub async fn shutdown(&mut self) {
        let mut c = self.client().await;
        c.call(&Caller::Human, Request::Shutdown).await.unwrap();
        let handle = self.handle.take().expect("daemon not running");
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("daemon did not stop")
            .unwrap();
    }

    /// Abort the daemon task, as if the daemon crashed.
    pub async fn kill(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// The caller for an agent address.
pub fn agent(address: &str) -> Caller {
    Caller::Address {
        address: address.to_string(),
    }
}

/// A `Register` request for `name` in `cwd`, bound to no process.
pub fn register_req(name: &str, cwd: &Path) -> Request {
    Request::Register {
        name: name.to_string(),
        role: None,
        working_on: None,
        harness: Harness::Other,
        cwd: cwd.to_path_buf(),
        proc_id: None,
        fresh: true,
        wake: true,
        session: None,
    }
}

/// Register `name` in `cwd` and return its address.
pub async fn register(c: &mut Client, name: &str, cwd: &Path) -> String {
    let data = c
        .call(&Caller::Human, register_req(name, cwd))
        .await
        .unwrap();
    data["address"].as_str().unwrap().to_string()
}

/// A `Send` request.
pub fn send_req(to: &[&str], subject: &str, body: &str) -> Request {
    Request::Send {
        to: to.iter().map(|s| s.to_string()).collect(),
        subject: subject.to_string(),
        body: body.to_string(),
        urgent: false,
        refs: Vec::new(),
    }
}
