use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    agentmail::cli::main().await
}
