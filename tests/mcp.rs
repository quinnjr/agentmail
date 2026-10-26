//! Task 10: the stdio MCP server, driven through rmcp's client.

use std::path::PathBuf;
use std::process::{Child, Command as StdCommand, Stdio};

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_agentmail");

/// The tools the server must offer, sorted.
const TOOLS: [&str; 15] = [
    "agents",
    "answer",
    "ask",
    "await_answer",
    "contacts",
    "inbox",
    "read",
    "register",
    "reply",
    "search",
    "send",
    "signoff",
    "status",
    "wait_for_mail",
    "whoami",
];

/// A private data directory and socket. Dropping it stops the daemon and
/// the fake harness processes.
struct Env {
    dir: TempDir,
    work: PathBuf,
    sleeps: Vec<Child>,
}

impl Env {
    fn new() -> Env {
        let dir = TempDir::new().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        Env {
            dir,
            work,
            sleeps: Vec::new(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("agentmail.sock")
    }

    fn cmd(&self) -> StdCommand {
        let mut c = StdCommand::new(BIN);
        c.env("AGENTMAIL_DATA_DIR", self.data_dir())
            .env("AGENTMAIL_SOCKET", self.socket())
            .env_remove("AGENTMAIL_ADDRESS")
            .env_remove("AGENTMAIL_HUMAN")
            .env_remove("AGENTMAIL_BIN")
            .current_dir(&self.work);
        c
    }

    /// Register a CLI agent `name` bound to a fresh `sleep`; its address.
    fn cli_agent(&mut self, name: &str) -> String {
        let sleep = StdCommand::new("sleep").arg("300").spawn().unwrap();
        let out = self
            .cmd()
            .args(["register", name, "--json", "--fresh", "--pid"])
            .arg(sleep.id().to_string())
            .output()
            .unwrap();
        self.sleeps.push(sleep);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["address"].as_str().unwrap().to_string()
    }

    /// `agentmail mcp` as a child process, with an initialized client.
    async fn mcp(&self) -> RunningService<RoleClient, ClientConfig> {
        let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|c| {
            c.arg("mcp")
                .env("AGENTMAIL_DATA_DIR", self.data_dir())
                .env("AGENTMAIL_SOCKET", self.socket())
                .env_remove("AGENTMAIL_ADDRESS")
                .env_remove("AGENTMAIL_HUMAN")
                .env_remove("AGENTMAIL_BIN")
                .current_dir(&self.work);
        }))
        .unwrap();
        ClientConfig::default().serve(transport).await.unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self
            .cmd()
            .args(["daemon", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for s in &mut self.sleeps {
            let _ = s.kill();
            let _ = s.wait();
        }
    }
}

async fn call(
    c: &RunningService<RoleClient, ClientConfig>,
    tool: &str,
    args: Value,
) -> CallToolResult {
    let args = args.as_object().cloned().unwrap_or_default();
    c.call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool's JSON result; panics on a tool error.
fn ok(r: &CallToolResult) -> Value {
    assert_ne!(r.is_error, Some(true), "tool error: {}", text(r));
    serde_json::from_str(&text(r)).unwrap_or_else(|e| panic!("bad json ({e}): {}", text(r)))
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_server_end_to_end() {
    let mut env = Env::new();
    let client = env.mcp().await;

    let info = client.peer_info().expect("initialized");
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(
        instructions.contains("Only use it when the human asks"),
        "{instructions}"
    );
    assert_eq!(
        info.server_info.as_ref().map(|i| i.name.as_str()),
        Some("agentmail")
    );

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, TOOLS);

    // Before registering there is no identity.
    let r = call(&client, "whoami", json!({})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));

    let reg = ok(&call(
        &client,
        "register",
        json!({"name": "backend", "working_on": "the API"}),
    )
    .await);
    let backend = reg["address"].as_str().unwrap().to_string();
    assert!(backend.starts_with("backend-"), "{backend}");
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["address"], backend.as_str());

    // A CLI agent mails the MCP agent by full address.
    let frontend = env.cli_agent("frontend");
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &frontend)
        .args([
            "send",
            "--to",
            &backend,
            "--subject",
            "hello there",
            "--body",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inbox = ok(&call(&client, "inbox", json!({})).await);
    let msgs = inbox.as_array().unwrap();
    assert_eq!(msgs.len(), 1, "{inbox}");
    assert_eq!(msgs[0]["subject"], "hello there");
    assert_eq!(msgs[0]["from"], frontend.as_str());
    // Summaries only: the body comes from `read`, which marks it read.
    assert!(msgs[0].get("body").is_none(), "{inbox}");
    assert_eq!(msgs[0]["preview"], "hi", "{inbox}");
    assert_eq!(msgs[0]["body_len"], 2, "{inbox}");

    // Read and reply through MCP; the CLI agent sees the reply.
    let id = msgs[0]["id"].as_str().unwrap().to_string();
    let read = ok(&call(&client, "read", json!({"id": id})).await);
    assert!(read.to_string().contains("\"body\":\"hi\""), "{read}");
    ok(&call(
        &client,
        "reply",
        json!({"message_id": id, "body": "got it"}),
    )
    .await);
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &frontend)
        .args(["inbox", "--json"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("got it"));

    // Contacts. A call with no data says so instead of returning a bare `null`.
    let added = ok(&call(
        &client,
        "contacts",
        json!({"action": "add", "address": frontend}),
    )
    .await);
    assert_eq!(added, json!({"ok": true}), "{added}");
    let contacts = ok(&call(&client, "contacts", json!({"action": "list"})).await);
    assert!(contacts.to_string().contains(&frontend), "{contacts}");

    // Two active agents named `api` in this repo: the name is ambiguous.
    let mut apis = vec![env.cli_agent("api"), env.cli_agent("api")];
    apis.sort();
    let r = call(
        &client,
        "send",
        json!({"to": ["api"], "subject": "s", "body": "b"}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    let t = text(&r);
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("bad json ({e}): {t}"));
    assert_eq!(v["error"]["code"], "AmbiguousRecipient", "{t}");
    let candidates = v["error"]["candidates"]
        .as_array()
        .expect("candidates array");
    assert_eq!(candidates.len(), 2, "{t}");
    let mut got: Vec<String> = candidates
        .iter()
        .map(|c| c["address"].as_str().unwrap().to_string())
        .collect();
    got.sort();
    assert_eq!(got, apis, "{t}");

    // ask → pending, answered from the CLI, then await_answer.
    let a = ok(&call(
        &client,
        "ask",
        json!({"to": [frontend], "subject": "q", "body": "which port?", "timeout_secs": 1}),
    )
    .await);
    assert_eq!(a["status"], "pending", "{a}");
    let qid = a["question_id"].as_str().unwrap().to_string();
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &frontend)
        .args(["answer", &qid, "--body", "8080"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let a = ok(&call(&client, "await_answer", json!({"question_id": qid})).await);
    assert_eq!(a["status"], "answered", "{a}");
    assert_eq!(a["answer"]["body"], "8080");
    let answer_id = a["answer"]["id"].as_str().unwrap();
    ok(&call(&client, "read", json!({"id": answer_id})).await);

    // A short wait with no mail times out.
    let w = ok(&call(&client, "wait_for_mail", json!({"timeout_secs": 1})).await);
    assert_eq!(w["status"], "timeout");

    client.cancel().await.unwrap();
}

/// The tools the end-to-end test does not reach, and the argument checks.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_server_status_agents_search_signoff() {
    let mut env = Env::new();
    let client = env.mcp().await;

    // An unknown harness is refused before anything is registered.
    let r = call(
        &client,
        "register",
        json!({"name": "backend", "harness": "bogus"}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    let t = text(&r);
    assert!(t.contains("InvalidInput"), "{t}");
    assert!(t.contains("bogus"), "{t}");

    // A valid override is recorded, and `wake` defaults to true.
    let reg = ok(&call(
        &client,
        "register",
        json!({"name": "backend", "working_on": "the API", "harness": "opencode"}),
    )
    .await);
    let backend = reg["address"].as_str().unwrap().to_string();
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["address"], backend.as_str(), "{me}");
    assert_eq!(me["agent"]["harness"], "opencode", "{me}");
    assert_eq!(me["agent"]["wake"], true, "{me}");

    // `status` updates the working-on summary.
    let st = ok(&call(&client, "status", json!({"working_on": "the schema"})).await);
    assert_eq!(st["working_on"], "the schema", "{st}");
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["working_on"], "the schema", "{me}");

    // `agents` filters by name prefix and repo.
    let frontend = env.cli_agent("frontend");
    let list = ok(&call(&client, "agents", json!({"name": "back"})).await);
    let found = list.as_array().unwrap();
    assert_eq!(found.len(), 1, "{list}");
    assert_eq!(found[0]["address"], backend.as_str());
    assert_eq!(found[0]["working_on"], "the schema");
    let work = env.work.to_string_lossy().to_string();
    let list = ok(&call(&client, "agents", json!({"repo": work})).await);
    assert!(list.to_string().contains(&backend), "{list}");
    assert!(list.to_string().contains(&frontend), "{list}");
    // A relative `repo` resolves against the server's working directory, as the CLI flag does.
    let list = ok(&call(&client, "agents", json!({"repo": "."})).await);
    assert!(list.to_string().contains(&backend), "{list}");
    assert!(list.to_string().contains(&frontend), "{list}");
    let elsewhere = env.dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let list = ok(&call(
        &client,
        "agents",
        json!({"repo": elsewhere.to_string_lossy()}),
    )
    .await);
    assert_eq!(list.as_array().unwrap().len(), 0, "{list}");

    // `search` finds a message by its subject.
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &frontend)
        .args([
            "send",
            "--to",
            &backend,
            "--subject",
            "hello there",
            "--body",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hits = ok(&call(&client, "search", json!({"query": "hello there"})).await);
    assert!(hits.to_string().contains("hello there"), "{hits}");
    let none = ok(&call(&client, "search", json!({"query": "zzz-no-such"})).await);
    assert!(!none.to_string().contains("hello there"), "{none}");

    // Adding or removing a contact needs an address.
    for action in ["add", "remove"] {
        let r = call(&client, "contacts", json!({"action": action})).await;
        assert_eq!(r.is_error, Some(true), "{action}");
        assert!(text(&r).contains("InvalidInput"), "{}", text(&r));
    }
    ok(&call(
        &client,
        "contacts",
        json!({"action": "add", "address": frontend}),
    )
    .await);
    ok(&call(
        &client,
        "contacts",
        json!({"action": "remove", "address": frontend}),
    )
    .await);
    let contacts = ok(&call(&client, "contacts", json!({})).await);
    assert!(!contacts.to_string().contains(&frontend), "{contacts}");

    // After `signoff` the agent is listed only with `all`.
    ok(&call(&client, "signoff", json!({})).await);
    let list = ok(&call(&client, "agents", json!({"name": "backend"})).await);
    assert_eq!(list.as_array().unwrap().len(), 0, "{list}");
    let list = ok(&call(&client, "agents", json!({"name": "backend", "all": true})).await);
    assert!(list.to_string().contains(&backend), "{list}");

    client.cancel().await.unwrap();
}

/// `wake: false` and `fresh: true` reach the daemon as given.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_register_wake_and_fresh() {
    let env = Env::new();
    let client = env.mcp().await;

    let first = ok(&call(&client, "register", json!({"name": "x", "wake": false})).await);
    let first = first["address"].as_str().unwrap().to_string();
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["address"], first.as_str(), "{me}");
    assert_eq!(me["agent"]["wake"], false, "{me}");

    let second = ok(&call(&client, "register", json!({"name": "x", "fresh": true})).await);
    let second = second["address"].as_str().unwrap().to_string();
    assert!(second.starts_with("x-"), "{second}");
    assert_ne!(first, second);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["address"], second.as_str(), "{me}");
    assert_eq!(me["agent"]["wake"], true, "{me}");

    client.cancel().await.unwrap();
}

/// A blocking tool does not hold up the others on the same session.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_blocking_tools_do_not_block_others() {
    let mut env = Env::new();
    let client = env.mcp().await;
    let reg = ok(&call(&client, "register", json!({"name": "backend"})).await);
    let backend = reg["address"].as_str().unwrap().to_string();
    let frontend = env.cli_agent("frontend");

    let peer = client.peer().clone();
    let started = std::time::Instant::now();
    let wait = tokio::spawn(async move {
        let args = json!({"timeout_secs": 5}).as_object().cloned().unwrap();
        let r = peer
            .call_tool(CallToolRequestParams::new("wait_for_mail").with_arguments(args))
            .await
            .unwrap();
        (r, started.elapsed())
    });
    // Let the wait reach the daemon first.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let t = std::time::Instant::now();
    let inbox = ok(&call(&client, "inbox", json!({})).await);
    assert_eq!(inbox, json!([]), "{inbox}");
    ok(&call(&client, "whoami", json!({})).await);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "other tools waited {:?}",
        t.elapsed()
    );
    assert!(!wait.is_finished(), "wait_for_mail returned early");

    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &frontend)
        .args(["send", "--to", &backend, "--subject", "ping", "--body", "x"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (r, elapsed) = wait.await.unwrap();
    let w = ok(&r);
    assert_eq!(w["status"], "mail", "{w}");
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");

    client.cancel().await.unwrap();
}
