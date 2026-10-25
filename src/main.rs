use std::process::ExitCode;
use std::time::{Duration, Instant};

use agentmail::client::{Client, ClientError, DataDirMismatch};
use agentmail::daemon::{DaemonConfig, serve};
use agentmail::paths::Paths;
use agentmail::protocol::{Caller, ErrorCode, PROTOCOL_VERSION, Request};
use clap::{Parser, Subcommand};

/// `daemon status` exit code when a daemon answers but speaks another
/// protocol version (1 means not running).
const STATUS_OTHER_PROTOCOL: u8 = 3;

#[derive(Parser)]
#[command(
    name = "agentmail",
    version,
    about = "Mail between coding agents and a human"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the background daemon (default: status).
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum DaemonAction {
    /// Serve in the foreground (what autostart runs).
    Run,
    /// Start the daemon if it is not running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Report whether the daemon is running. Exits 1 when it is not, 3 when
    /// it speaks another protocol version (stop it to replace it).
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let paths = Paths::from_env()?;
    match cli.command {
        Command::Daemon { action } => daemon(&paths, action.unwrap_or(DaemonAction::Status)).await,
    }
}

async fn daemon_pid(c: &mut Client) -> anyhow::Result<u64> {
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

async fn daemon(paths: &Paths, action: DaemonAction) -> anyhow::Result<ExitCode> {
    match action {
        DaemonAction::Run => {
            serve(DaemonConfig {
                paths: paths.clone(),
            })
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonAction::Start => {
            let mut c = Client::connect_or_spawn(paths).await?;
            let pid = daemon_pid(&mut c).await?;
            println!("daemon running (pid {pid})");
            Ok(ExitCode::SUCCESS)
        }
        DaemonAction::Status => match Client::connect(paths).await {
            Ok(mut c) => {
                let pid = daemon_pid(&mut c).await?;
                println!(
                    "daemon running (pid {pid}, protocol {PROTOCOL_VERSION}, socket {})",
                    paths.socket.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(e) if e.downcast_ref::<DataDirMismatch>().is_some() => {
                println!("daemon running, but not for this data directory: {e:#}");
                Ok(ExitCode::FAILURE)
            }
            Err(e) => match e.downcast_ref::<ClientError>() {
                Some(ce) if ce.code == ErrorCode::VersionMismatch => {
                    println!(
                        "daemon running, but it speaks a different protocol (this client \
                         speaks {PROTOCOL_VERSION}: {}); run `agentmail daemon stop`",
                        ce.message
                    );
                    Ok(ExitCode::from(STATUS_OTHER_PROTOCOL))
                }
                _ => {
                    println!("daemon not running ({e:#})");
                    Ok(ExitCode::FAILURE)
                }
            },
        },
        DaemonAction::Stop => {
            // No Hello, so this also stops a daemon of another protocol.
            if !Client::shutdown(paths).await? {
                println!("daemon not running");
                return Ok(ExitCode::SUCCESS);
            }
            // Wait until it has released the database and lock, so a
            // following start gets a fresh daemon.
            let deadline = Instant::now() + Duration::from_secs(5);
            while paths.lock_held() {
                if Instant::now() >= deadline {
                    anyhow::bail!("daemon did not stop within 5s");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            println!("daemon stopped");
            Ok(ExitCode::SUCCESS)
        }
    }
}
