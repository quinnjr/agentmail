//! Task 10: the stdio MCP server, driven through rmcp's client.

use std::path::PathBuf;
use std::process::{Child, Command as StdCommand, Stdio};

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, RequestMetaObject};
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
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("KIRO_SESSION_ID")
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
        self.mcp_with(&[]).await
    }

    /// [`Env::mcp`] with extra variables for the server.
    async fn mcp_with(&self, vars: &[(&str, &str)]) -> RunningService<RoleClient, ClientConfig> {
        let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|c| {
            // A private kiro config, so the user's never sets the default.
            c.env_remove("AGENTMAIL_KIRO_WAKE")
                .env("KIRO_HOME", self.dir.path().join("kiro"))
                .envs(vars.iter().copied())
                .arg("mcp")
                .env("AGENTMAIL_DATA_DIR", self.data_dir())
                .env("AGENTMAIL_SOCKET", self.socket())
                .env_remove("AGENTMAIL_ADDRESS")
                .env_remove("AGENTMAIL_HUMAN")
                .env_remove("AGENTMAIL_BIN")
                .env_remove("OPENCODE_SESSION_ID")
                .env_remove("KIRO_SESSION_ID")
                .current_dir(&self.work);
        }))
        .unwrap();
        ClientConfig::default().serve(transport).await.unwrap()
    }
}

impl Env {
    /// `agentmail mcp` run by a fake harness process named `harness` (a
    /// copy of `/bin/sh`) through `script`, a shell snippet in which `$MCP`
    /// starts the server. The harness writes its pid to a file; returns the
    /// client and that pid.
    async fn mcp_under(
        &self,
        harness: &str,
        script: &str,
    ) -> (RunningService<RoleClient, ClientConfig>, u32) {
        let fake = self.dir.path().join(harness);
        std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
        let pid_file = self.dir.path().join(format!("{harness}.pid"));
        let script = format!(
            "echo $$ > '{}'; {}",
            pid_file.display(),
            script.replace("$MCP", &format!("{BIN} mcp"))
        );
        let transport =
            TokioChildProcess::new(tokio::process::Command::new(&fake).configure(|c| {
                c.arg("-c")
                    .arg(&script)
                    .env("AGENTMAIL_DATA_DIR", self.data_dir())
                    .env("AGENTMAIL_SOCKET", self.socket())
                    .env_remove("AGENTMAIL_ADDRESS")
                    .env_remove("AGENTMAIL_HUMAN")
                    .env_remove("AGENTMAIL_BIN")
                    .env_remove("OPENCODE_SESSION_ID")
                    .env_remove("KIRO_SESSION_ID")
                    .current_dir(&self.work);
            }))
            .unwrap();
        let client = ClientConfig::default().serve(transport).await.unwrap();
        // The server answered, so the harness wrote its pid before starting it.
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        (client, pid.trim().parse().unwrap())
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

/// `call` as opencode makes it: with `_meta["ai.opencode/sessionID"]`.
async fn call_in_session(
    c: &RunningService<RoleClient, ClientConfig>,
    session: &str,
    tool: &str,
    args: Value,
) -> CallToolResult {
    let args = args.as_object().cloned().unwrap_or_default();
    let mut meta = RequestMetaObject::new();
    meta.insert("ai.opencode/sessionID".into(), json!(session));
    let mut params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    params.meta = Some(meta);
    c.call_tool(params).await.unwrap()
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

/// Two opencode sessions share one MCP server: each call is resolved by the
/// session in its `_meta`, never by a cached address.
#[tokio::test(flavor = "multi_thread")]
async fn opencode_meta_session_isolates_callers() {
    let mut env = Env::new();
    // opencode -> agentmail mcp; the trailing `true` stops the shell from
    // exec'ing the server.
    let (client, opencode) = env.mcp_under("opencode", "$MCP; true").await;

    // A call without a session cannot register on the shared server, and
    // leaves no identity behind for other sessionless calls.
    let r = call(&client, "register", json!({"name": "nosession"})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("no opencode session id"), "{}", text(&r));
    // Other sessionless calls say why they have no identity, rather than
    // suggesting a `register` this server would refuse.
    let r = call(&client, "whoami", json!({})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));
    assert!(text(&r).contains("no opencode session id"), "{}", text(&r));
    assert!(!text(&r).contains("agentmail register"), "{}", text(&r));
    // A session's call that is not registered keeps the daemon's advice.
    let r = call_in_session(&client, "s0", "whoami", json!({})).await;
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));
    assert!(!text(&r).contains("no opencode session id"), "{}", text(&r));

    // A session fixes the harness: an override naming another is refused
    // up front, and naming opencode itself is accepted.
    for other in ["other", "claude-code"] {
        let r = call_in_session(
            &client,
            "s1",
            "register",
            json!({"name": "backend", "harness": other}),
        )
        .await;
        assert_eq!(r.is_error, Some(true), "{}", text(&r));
        assert!(text(&r).contains("InvalidInput"), "{}", text(&r));
        assert!(
            text(&r).contains("fixed by the opencode session"),
            "{}",
            text(&r)
        );
    }

    let r1 = ok(&call_in_session(
        &client,
        "s1",
        "register",
        json!({"name": "backend", "harness": "opencode"}),
    )
    .await);
    let backend = r1["address"].as_str().unwrap().to_string();
    let r2 = ok(&call_in_session(&client, "s2", "register", json!({"name": "frontend"})).await);
    let frontend = r2["address"].as_str().unwrap().to_string();
    assert!(backend.starts_with("backend-"), "{backend}");
    assert!(frontend.starts_with("frontend-"), "{frontend}");

    for _ in 0..2 {
        let me = ok(&call_in_session(&client, "s1", "whoami", json!({})).await);
        assert_eq!(me["address"], backend.as_str(), "{me}");
        assert_eq!(me["agent"]["harness"], "opencode", "{me}");
        assert_eq!(me["agent"]["session"]["id"], "s1", "{me}");
        assert_eq!(me["agent"]["proc_id"]["pid"], opencode, "{me}");
        let me = ok(&call_in_session(&client, "s2", "whoami", json!({})).await);
        assert_eq!(me["address"], frontend.as_str(), "{me}");
    }

    // Registering again from s1 returns the same address.
    let again = ok(&call_in_session(&client, "s1", "register", json!({"name": "backend"})).await);
    assert_eq!(again["address"], backend.as_str(), "{again}");

    // Neither registration became the process-level identity.
    let r = call(&client, "whoami", json!({})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));
    // An unregistered session has no identity either.
    let r = call_in_session(&client, "s3", "whoami", json!({})).await;
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));

    // Mail to s1 reaches s1 only.
    let sender = env.cli_agent("sender");
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &sender)
        .args([
            "send",
            "--to",
            &backend,
            "--subject",
            "for s1",
            "--body",
            "x",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inbox = ok(&call_in_session(&client, "s1", "inbox", json!({})).await);
    assert_eq!(inbox.as_array().unwrap().len(), 1, "{inbox}");
    let inbox = ok(&call_in_session(&client, "s2", "inbox", json!({})).await);
    assert_eq!(inbox, json!([]), "{inbox}");

    client.cancel().await.unwrap();
}

/// opencode may start the server through a wrapper (`sh -c`): the server
/// binds the opencode process above the wrapper, and is shared between
/// opencode's sessions, so a sessionless `register` is refused and leaves
/// no identity behind.
#[tokio::test(flavor = "multi_thread")]
async fn opencode_via_wrapper_refuses_sessionless_register() {
    let env = Env::new();
    // opencode -> sh (the wrapper) -> agentmail mcp.
    let (client, opencode) = env
        .mcp_under("opencode", "/bin/sh -c '$MCP; true'; true")
        .await;

    let r = call(&client, "register", json!({"name": "nosession"})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("no opencode session id"), "{}", text(&r));
    assert!(text(&r).contains("started by opencode"), "{}", text(&r));
    let r = call(&client, "whoami", json!({})).await;
    assert_eq!(r.is_error, Some(true), "{}", text(&r));
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));

    // A session still registers.
    let r = ok(&call_in_session(&client, "s1", "register", json!({"name": "backend"})).await);
    assert!(
        r["address"].as_str().unwrap().starts_with("backend-"),
        "{r}"
    );
    // Bound to the opencode process, not the wrapper: the same process
    // `agentmail register` in that session's shell binds.
    let me = ok(&call_in_session(&client, "s1", "whoami", json!({})).await);
    assert_eq!(me["agent"]["proc_id"]["pid"], opencode, "{me}");
    let r = call(&client, "whoami", json!({})).await;
    assert!(text(&r).contains("NoIdentity"), "{}", text(&r));

    client.cancel().await.unwrap();
}

/// Claude Code gets no harness redirect (see docs/harness-notes.md): a
/// server it starts through a wrapper that stays its parent (claude -> sh
/// -> agentmail mcp) binds the wrapper, not Claude Code.
#[tokio::test(flavor = "multi_thread")]
async fn claude_code_wrapper_binds_the_wrapper() {
    let env = Env::new();
    let wrapper_pid = env.dir.path().join("wrapper.pid");
    let script = format!(
        "/bin/sh -c \"echo \\$\\$ > {}; $MCP; true\"; true",
        wrapper_pid.display()
    );
    let (client, claude) = env.mcp_under("claude", &script).await;
    let wrapper: u32 = std::fs::read_to_string(&wrapper_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(wrapper, claude);

    ok(&call(&client, "register", json!({"name": "wrapped"})).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["proc_id"]["pid"], wrapper, "{me}");
    assert_eq!(me["agent"]["harness"], "other", "{me}");

    client.cancel().await.unwrap();
}

/// Under kiro V3 the MCP server's parent is a helper (`node`), not
/// `kiro-cli`: `register` binds the nearest harness ancestor instead.
#[tokio::test(flavor = "multi_thread")]
async fn bind_prefers_harness_ancestor() {
    let env = Env::new();
    // kiro-cli -> sh (the intermediate) -> agentmail mcp. The trailing
    // `true`s stop either shell from exec'ing its last command.
    let (client, kiro) = env
        .mcp_under("kiro-cli", "/bin/sh -c '$MCP; true'; true")
        .await;

    ok(&call(&client, "register", json!({"name": "kiroagent"})).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["harness"], "kiro", "{me}");
    // Bound to the kiro-cli process itself, not the intermediate shell.
    assert_eq!(me["agent"]["proc_id"]["pid"], kiro, "{me}");

    client.cancel().await.unwrap();
}

/// The real kiro chain: `kiro-cli` -> `kiro-cli-chat` -> the engine's
/// `node` -> agentmail mcp. `kiro-cli-chat` is not a harness name, so the
/// binding skips it and reaches `kiro-cli`.
#[tokio::test(flavor = "multi_thread")]
async fn bind_skips_kiro_cli_chat() {
    let env = Env::new();
    let chat = env.dir.path().join("kiro-cli-chat");
    std::os::unix::fs::symlink("/bin/sh", &chat).unwrap();
    let node = env.dir.path().join("node");
    std::os::unix::fs::symlink("/bin/sh", &node).unwrap();
    // kiro-cli -> kiro-cli-chat -> node -> agentmail mcp.
    let script = format!(
        "'{}' -c \"'{}' -c '$MCP; true'; true\"; true",
        chat.display(),
        node.display()
    );
    let (client, kiro) = env.mcp_under("kiro-cli", &script).await;

    ok(&call(&client, "register", json!({"name": "kiroagent"})).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["harness"], "kiro", "{me}");
    assert_eq!(me["agent"]["proc_id"]["pid"], kiro, "{me}");

    client.cancel().await.unwrap();
}

/// Run `$MCP` as another MCP client (a copy of `/bin/sh` named `client`)
/// would from a harness shell that sets `var=value`: the shell snippet
/// for [`Env::mcp_under`], and the file the client writes its pid to.
fn from_harness_shell(env: &Env, client: &str, var: &str, value: &str) -> (String, PathBuf) {
    let fake = env.dir.path().join(client);
    std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
    let pid_file = env.dir.path().join(format!("{client}.pid"));
    let script = format!(
        "{var}={value} /bin/sh -c \"'{}' -c 'echo \\$\\$ > {}; $MCP; true'; true\"; true",
        fake.display(),
        pid_file.display()
    );
    (script, pid_file)
}

fn read_pid(path: &std::path::Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Another MCP client run from kiro's shell tool (kiro-cli -> sh with
/// `KIRO_SESSION_ID` -> the client -> agentmail mcp) binds its own process.
/// Redirected to kiro-cli, it would take over the agent kiro's own server
/// registered there.
#[tokio::test(flavor = "multi_thread")]
async fn client_from_kiro_shell_binds_itself() {
    let env = Env::new();
    let (script, client_pid) = from_harness_shell(&env, "codex", "KIRO_SESSION_ID", "k1");
    let (client, kiro) = env.mcp_under("kiro-cli", &script).await;
    let foreign = read_pid(&client_pid);

    // The agent kiro's own server registered, bound to kiro-cli.
    let out = env
        .cmd()
        .args(["register", "kiroagent", "--json", "--pid"])
        .arg(kiro.to_string())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let kiro_agent: Value = serde_json::from_slice(&out.stdout).unwrap();
    let kiro_address = kiro_agent["address"].as_str().unwrap().to_string();

    let r = ok(&call(&client, "register", json!({"name": "foreign"})).await);
    assert!(
        r["address"].as_str().unwrap().starts_with("foreign-"),
        "{r}"
    );
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["proc_id"]["pid"], foreign, "{me}");
    assert_eq!(me["agent"]["harness"], "other", "{me}");

    // kiro's agent keeps its binding.
    let out = env
        .cmd()
        .env("AGENTMAIL_ADDRESS", &kiro_address)
        .args(["whoami", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let who: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(who["address"], kiro_address.as_str(), "{who}");
    assert_eq!(who["agent"]["proc_id"]["pid"], kiro, "{who}");

    client.cancel().await.unwrap();
}

/// Another MCP client run from opencode's bash tool (opencode -> sh with
/// `OPENCODE_SESSION_ID` -> the client -> agentmail mcp) is not opencode's
/// shared server: it binds its own process and registers without a
/// session.
#[tokio::test(flavor = "multi_thread")]
async fn client_from_opencode_shell_binds_itself() {
    let env = Env::new();
    let (script, client_pid) = from_harness_shell(&env, "gemini", "OPENCODE_SESSION_ID", "o1");
    let (client, _opencode) = env.mcp_under("opencode", &script).await;
    let foreign = read_pid(&client_pid);

    let r = ok(&call(&client, "register", json!({"name": "foreign"})).await);
    assert!(
        r["address"].as_str().unwrap().starts_with("foreign-"),
        "{r}"
    );
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["proc_id"]["pid"], foreign, "{me}");
    assert_eq!(me["agent"]["harness"], "other", "{me}");

    client.cancel().await.unwrap();
}

/// Harness Task 8: a kiro agent registers with `wake` off unless asked,
/// or unless `install --kiro-wake` gave the server `AGENTMAIL_KIRO_WAKE=1`.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_register_wake_defaults_off_for_kiro() {
    let env = Env::new();
    let client = env.mcp().await;
    ok(&call(&client, "register", json!({"name": "k", "harness": "kiro"})).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["wake"], false, "{me}");
    let args = json!({"name": "k", "harness": "kiro", "wake": true});
    ok(&call(&client, "register", args).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["wake"], true, "{me}");
    client.cancel().await.unwrap();

    let client = env.mcp_with(&[("AGENTMAIL_KIRO_WAKE", "1")]).await;
    let args = json!({"name": "k2", "harness": "kiro", "fresh": true});
    ok(&call(&client, "register", args).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["wake"], true, "{me}");
    client.cancel().await.unwrap();
}

/// The MCP server is the process kiro started with the `env` it settled
/// on, so without `AGENTMAIL_KIRO_WAKE=1` in its own environment
/// `--kiro-wake` is off, whatever the installed `mcp.json` files say (an
/// untrusted workspace's file, say, which kiro did not load).
#[tokio::test(flavor = "multi_thread")]
async fn mcp_register_wake_ignores_installed_files() {
    let env = Env::new();
    let entry = json!({"mcpServers": {"agentmail": {
        "command": "agentmail",
        "args": ["mcp"],
        "env": {"AGENTMAIL_KIRO_WAKE": "1"},
    }}})
    .to_string();
    for dir in [env.dir.path().join("kiro"), env.work.join(".kiro")] {
        let settings = dir.join("settings");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(settings.join("mcp.json"), &entry).unwrap();
    }
    let client = env.mcp().await;
    ok(&call(&client, "register", json!({"name": "k", "harness": "kiro"})).await);
    let me = ok(&call(&client, "whoami", json!({})).await);
    assert_eq!(me["agent"]["wake"], false, "{me}");
    client.cancel().await.unwrap();
}
