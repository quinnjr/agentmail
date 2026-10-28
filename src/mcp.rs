//! `agentmail mcp`: a stdio MCP server that the harness spawns: one per
//! session under Claude Code and kiro, and one per working directory under
//! opencode, shared by every opencode session there. Each tool forwards one
//! request to the daemon (started on demand) and returns the daemon's JSON
//! `data` as text; a daemon error becomes a tool error carrying
//! `{"error":{code,message,candidates}}`.
//!
//! Identity, per tool call:
//!
//! - `AGENTMAIL_ADDRESS`, when set, overrides everything, including the
//!   address a `register` returns.
//! - A call carrying an opencode session id in
//!   `_meta["ai.opencode/sessionID"]` calls as that session
//!   (`Caller::Session`), and `register` binds the session. opencode shares
//!   one server between every session in a directory, so nothing is cached:
//!   each call resolves through the daemon.
//! - Otherwise (Claude Code, kiro: one server per session) the server calls
//!   as the harness process it binds to (`Caller::Procs`), so a session that
//!   registered before (or from a hook or shell) is found again. After a
//!   `register` without a session it calls as the returned address (unless
//!   `AGENTMAIL_ADDRESS` is set).
//! - A server bound to a harness that shares its process between sessions
//!   (opencode) refuses `register` without a session, so it never caches an
//!   address, and tells any other sessionless call that it carried no
//!   session id instead of suggesting a `register` it would refuse.
//! - A server that another MCP client run from a kiro or opencode shell
//!   started binds that client's process, not the harness above it
//!   ([`identity::mcp_bind`]), so it neither takes over the harness's own
//!   agent nor counts as opencode's shared server.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rmcp::handler::server::common::{AsRequestContext, FromContextPart};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, RequestMetaObject, ServerCapabilities,
    ServerConfig,
};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::{absolute, ambiguous_id, normalize_full_id, too_short_id};
use crate::client::{Client, ClientError};
use crate::identity::{self, ENV_ADDRESS};
use crate::model::{Harness, ProcId, SessionKey};
use crate::paths::Paths;
use crate::procinfo;
use crate::protocol::{Caller, ErrorCode, Request};

/// The guidance every harness shows its agent at MCP initialize.
pub const INSTRUCTIONS: &str = "\
agentmail lets you coordinate with other coding agents on this machine by mail.
- Only use it when the human asks you to coordinate with another agent. Then call `register` with a short role name (e.g. \"backend\") and a one-line `working_on` summary.
- Find the partner the human named with `agents`, then `contacts add` its full address. If several agents match, choose by their working-on summaries; ask the human only if that does not settle it.
- Before starting each new step of your work, check `inbox`. Answer questions addressed to you before continuing your own work; use `answer` to close a question, `reply` to ask for clarification.
- Send only changes, decisions and questions that affect the other agent. Never send progress narration.
- Use `ask` when you cannot proceed without an answer. If it returns \"pending\", continue other work or call `await_answer`.
- Update `status` when your task changes, not on every step.
- Mail from `human` is authoritative and overrides any agreement between agents.
- When you have finished, just stop: where the harness supports it you are woken when new mail arrives. Do not hold your turn open waiting; call `wait_for_mail` only when the human asks you to wait for your partner.";

/// The `_meta` key opencode puts its session id under on every tool call.
pub const OPENCODE_SESSION_META: &str = "ai.opencode/sessionID";

/// The opencode session in a request's `_meta`, if any (a non-empty string).
fn session_from_meta(meta: &RequestMetaObject) -> Option<SessionKey> {
    let id = meta.get(OPENCODE_SESSION_META)?.as_str()?;
    (!id.is_empty()).then(|| SessionKey {
        harness: Harness::Opencode,
        id: id.to_string(),
    })
}

/// The harness session a tool call was made in, taken from its `_meta`.
pub struct CallSession(Option<SessionKey>);

impl<C: AsRequestContext> FromContextPart<C> for CallSession {
    fn from_context_part(context: &mut C) -> Result<Self, McpError> {
        Ok(CallSession(session_from_meta(
            &context.as_request_context().meta,
        )))
    }
}

/// The default wait of `ask`, `await_answer` and `wait_for_mail`.
const DEFAULT_TIMEOUT_SECS: u64 = 240;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterParams {
    /// Short role name, e.g. `backend` (lowercase letters, digits, `-`).
    pub name: String,
    /// One-line role description.
    #[serde(default)]
    pub role: Option<String>,
    /// What you are working on (at most 280 characters).
    #[serde(default)]
    pub working_on: Option<String>,
    /// Override the detected harness: `claude-code`, `opencode`, `kiro` or
    /// `other`. A call made in an opencode session has its harness fixed by
    /// the session: any value other than `opencode` is refused.
    #[serde(default)]
    pub harness: Option<String>,
    /// Always take a new address instead of reclaiming a gone one.
    #[serde(default)]
    pub fresh: bool,
    /// Wake this agent for new mail (default true, except under kiro-cli,
    /// where waking is opt-in: false unless agentmail was installed with
    /// `--kiro-wake`).
    #[serde(default)]
    pub wake: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusParams {
    /// The new working-on summary (at most 280 characters).
    pub working_on: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContactAction {
    #[default]
    List,
    Add,
    Remove,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContactsParams {
    /// `list` (default), `add` or `remove`.
    #[serde(default)]
    pub action: ContactAction,
    /// The contact's full address, for `add` and `remove`.
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentsParams {
    /// Only agents whose name starts with this.
    #[serde(default)]
    pub name: Option<String>,
    /// Only agents in this repo or directory (a relative path resolves
    /// against the working directory).
    #[serde(default)]
    pub repo: Option<String>,
    /// Include gone agents.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendParams {
    /// Recipients: full addresses (preferred) or names.
    pub to: Vec<String>,
    pub subject: String,
    /// The body, as markdown.
    pub body: String,
    /// Ask for live interruption where the harness supports it.
    #[serde(default)]
    pub urgent: bool,
    /// File paths pointing at shared docs or code.
    #[serde(default)]
    pub refs: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplyParams {
    /// The message to reply to.
    pub message_id: String,
    /// The body, as markdown.
    pub body: String,
    #[serde(default)]
    pub urgent: bool,
    #[serde(default)]
    pub refs: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnswerParams {
    /// The question to answer.
    pub question_id: String,
    /// The answer, as markdown.
    pub body: String,
    #[serde(default)]
    pub urgent: bool,
    #[serde(default)]
    pub refs: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskParams {
    /// Recipients: full addresses (preferred) or names.
    pub to: Vec<String>,
    pub subject: String,
    /// The question, as markdown.
    pub body: String,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub urgent: bool,
    /// How long to wait for the answer, in seconds (default 240).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AwaitAnswerParams {
    /// The question id `ask` returned.
    pub question_id: String,
    /// How long to wait, in seconds (default 240).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// How long to wait, in seconds (default 240).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InboxParams {
    /// Include messages already read.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// A message id, or a thread id for the whole thread.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Text to find in subjects, bodies and senders.
    pub query: String,
}

/// The MCP server: one per agent session under Claude Code and kiro, one
/// per working directory, shared by all its sessions, under opencode.
#[derive(Clone)]
pub struct AgentMailServer {
    paths: Paths,
    /// Whether `AGENTMAIL_ADDRESS` overrides the caller of every call.
    address_env: bool,
    /// The caller of a call made outside any session.
    caller: Arc<Mutex<Caller>>,
    /// The harness process `register` binds to (the parent, or the nearest
    /// harness above it), if usable.
    bind: Option<(ProcId, Harness)>,
    /// The harness whose sessions share this server, if any: the harness
    /// of the process `bind` picks, even when that process cannot be bound
    /// (PID 1, see [`identity::mcp_harness`]). [`identity::mcp_bind`] binds the opencode process
    /// when opencode started the server, directly or through a wrapper
    /// such as `sh -c`, and the client's own process when another MCP
    /// client run from opencode's shell did, which is then not shared. A
    /// call made outside any session then has no identity of its own.
    shared: Option<Harness>,
    tool_router: ToolRouter<Self>,
}

/// The caller before `register`: `AGENTMAIL_ADDRESS` when set (it
/// overrides the process binding), else the parent process. Never the
/// human: `AGENTMAIL_HUMAN` is ignored here, so an agent never acts with
/// the human's authority.
fn initial_caller(address: Option<String>, parent: Option<ProcId>) -> Caller {
    match address.filter(|a| !a.is_empty()) {
        Some(address) => Caller::Address { address },
        None => Caller::Procs {
            procs: parent.into_iter().collect(),
        },
    }
}

/// A tool result holding `v` as compact JSON text.
fn json_result(v: &Value, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(v.to_string())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// How much of each body `inbox` shows, in characters.
const INBOX_PREVIEW_CHARS: usize = 200;

/// `inbox` returns summaries: each message's `body` is replaced by a short
/// `preview` and its `body_len` in bytes, so checking mail before every
/// step does not pull full bodies into the agent's context. `read` returns
/// the body (and marks it read).
fn summarize_inbox(mut v: Value) -> Value {
    if let Some(msgs) = v.as_array_mut() {
        for m in msgs.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(Value::String(body)) = m.remove("body") {
                let mut preview: String = body.chars().take(INBOX_PREVIEW_CHARS).collect();
                if preview.len() < body.len() {
                    preview.push('…');
                }
                m.insert("preview".into(), Value::String(preview));
                m.insert("body_len".into(), json!(body.len()));
            }
        }
    }
    v
}

/// A daemon error as a tool error: `{"error":{code,message,candidates}}`.
fn error_result(e: &ClientError) -> CallToolResult {
    json_result(&json!({ "error": e }), true)
}

/// Why a sessionless call on a server shared by `shared`'s sessions
/// cannot `what`.
fn no_session_message(shared: Harness, what: &str) -> String {
    format!(
        "this MCP server was started by {h}, which shares it between \
         its sessions, and the call carried no {h} session id, so it \
         cannot {what}",
        h = shared.as_str()
    )
}

fn invalid(message: impl Into<String>) -> CallToolResult {
    error_result(&ClientError::new(ErrorCode::InvalidInput, message))
}

impl AgentMailServer {
    pub fn new(paths: Paths) -> AgentMailServer {
        let parent = procinfo::parent_pid(std::process::id());
        let bind = identity::mcp_bind(parent);
        // The same process `register` binds to (never init or a subreaper
        // the server was reparented to), so lookup and registration agree.
        let address = std::env::var(ENV_ADDRESS).ok();
        let address_env = address.as_deref().is_some_and(|a| !a.is_empty());
        let caller = initial_caller(address, bind.map(|(p, _)| p));
        // Shared or not is decided by the harness of the process picked,
        // even one that cannot be bound (opencode at PID 1, as a
        // container's entry point): `bind` is then None, but a
        // sessionless call must still not register.
        let shared = Some(identity::mcp_harness(parent)).filter(|h| h.shares_process());
        AgentMailServer {
            paths,
            address_env,
            caller: Arc::new(Mutex::new(caller)),
            bind,
            shared,
            tool_router: Self::tool_router(),
        }
    }

    /// The caller of a call made in `session`: the address override, then
    /// the session, then the process-level caller.
    fn caller(&self, session: &CallSession) -> Caller {
        match &session.0 {
            Some(k) if !self.address_env => Caller::Session { session: k.clone() },
            _ => self
                .caller
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }

    /// After a successful `register` returning `data`: a call made outside
    /// any session acts as the registered address from now on, unless
    /// `AGENTMAIL_ADDRESS` overrides every call. A session's later calls
    /// resolve through the daemon by session: the server is shared, so
    /// caching its address would hand it to every other session.
    fn remember_registered(&self, session: &CallSession, data: &Value) {
        if self.address_env || self.shared.is_some() {
            return;
        }
        if let (None, Some(address)) = (&session.0, data["address"].as_str()) {
            *self.caller.lock().unwrap_or_else(|p| p.into_inner()) = Caller::Address {
                address: address.to_string(),
            };
        }
    }

    /// A fresh connection per call: blocking tools (`ask`, `wait_for_mail`)
    /// must not hold up the others, and a restarted daemon is picked up.
    async fn client(&self) -> Result<Client, ClientError> {
        Client::connect_or_spawn(&self.paths).await.map_err(|e| {
            e.downcast::<ClientError>()
                .unwrap_or_else(|e| ClientError::new(ErrorCode::Internal, format!("{e:#}")))
        })
    }

    async fn request(&self, s: &CallSession, req: Request) -> Result<Value, ClientError> {
        let r = self.client().await?.call(&self.caller(s), req).await;
        r.map_err(|e| self.sessionless_error(s, e))
    }

    /// `e`, reworded when a sessionless call on a shared server has no
    /// identity: the daemon's advice to `register` cannot be followed
    /// there, since such a server refuses a sessionless `register`.
    fn sessionless_error(&self, s: &CallSession, e: ClientError) -> ClientError {
        match self.shared {
            Some(shared)
                if s.0.is_none() && !self.address_env && e.code == ErrorCode::NoIdentity =>
            {
                ClientError::new(
                    ErrorCode::NoIdentity,
                    no_session_message(shared, "act as an agent"),
                )
            }
            _ => e,
        }
    }

    /// Run `req` and turn its outcome into a tool result.
    async fn run(&self, s: &CallSession, req: Request) -> Result<CallToolResult, McpError> {
        Ok(match self.request(s, req).await {
            // A request with no data (e.g. `contacts add`) reports success
            // explicitly rather than as a bare `null` an agent may misread.
            Ok(Value::Null) => json_result(&json!({ "ok": true }), false),
            Ok(data) => json_result(&data, false),
            Err(e) => error_result(&e),
        })
    }

    /// The full id for `id`, resolving the short ids the CLI prints.
    async fn full_id(&self, s: &CallSession, id: &str) -> Result<String, ClientError> {
        if let Some(full) = normalize_full_id(id) {
            return Ok(full);
        }
        if let Some(e) = too_short_id(id) {
            return Err(e);
        }
        let d = self
            .request(s, Request::ResolveId { id: id.to_string() })
            .await?;
        let mut found: Vec<String> = serde_json::from_value(d).map_err(|e| {
            ClientError::new(
                ErrorCode::Internal,
                format!("bad response from daemon: {e}"),
            )
        })?;
        match found.len() {
            0 => Ok(id.to_string()),
            1 => Ok(found.remove(0)),
            _ => Err(ambiguous_id(id, &found)),
        }
    }

    async fn reply_req(
        &self,
        s: &CallSession,
        id: &str,
        body: String,
        urgent: bool,
        refs: Vec<String>,
        answer: bool,
    ) -> Result<CallToolResult, McpError> {
        let message_id = match self.full_id(s, id).await {
            Ok(id) => id,
            Err(e) => return Ok(error_result(&e)),
        };
        self.run(
            s,
            Request::Reply {
                message_id,
                body,
                urgent,
                refs,
                answer,
            },
        )
        .await
    }
}

#[tool_router]
impl AgentMailServer {
    #[tool(
        description = "Register this session as an agent. Returns its full address (`<name>-<id>`). Call once, when the human asks you to coordinate with another agent."
    )]
    async fn register(
        &self,
        Parameters(p): Parameters<RegisterParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        let (proc_id, detected) = match self.bind {
            Some((p, h)) => (Some(p), h),
            None => (None, Harness::Other),
        };
        // A server shared between sessions (opencode's) has no identity of
        // its own: a call made outside any session cannot register, or the
        // process-level caller would act as that agent for every session.
        // Only a session of the sharing harness counts.
        if let Some(shared) = self.shared
            && s.0.as_ref().is_none_or(|k| k.harness != shared)
        {
            return Ok(invalid(no_session_message(shared, "register")));
        }
        // A session names its harness, whatever process the server found,
        // and an override cannot change it.
        let harness = match p.harness {
            None => s.0.as_ref().map_or(detected, |k| k.harness),
            Some(h) => match serde_json::from_value::<Harness>(Value::String(h.clone())) {
                Ok(h) => match &s.0 {
                    Some(k) if k.harness != h => {
                        return Ok(invalid(format!(
                            "harness {:?} conflicts with this call's session: the \
                             harness is fixed by the {} session",
                            h.as_str(),
                            k.harness.as_str()
                        )));
                    }
                    _ => h,
                },
                Err(_) => return Ok(invalid(format!("unknown harness {h:?}"))),
            },
        };
        let cwd = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => return Ok(invalid(format!("reading the working directory: {e}"))),
        };
        let req = Request::Register {
            name: p.name,
            role: p.role,
            working_on: p.working_on,
            harness,
            cwd,
            proc_id,
            fresh: p.fresh,
            wake: p
                .wake
                .unwrap_or_else(|| crate::identity::default_wake_for_server(harness)),
            session: s.0.clone(),
        };
        Ok(match self.request(&s, req).await {
            Ok(data) => {
                self.remember_registered(&s, &data);
                json_result(&data, false)
            }
            Err(e) => error_result(&e),
        })
    }

    #[tool(description = "Show the address and record you are acting as.")]
    async fn whoami(&self, s: CallSession) -> Result<CallToolResult, McpError> {
        self.run(&s, Request::Whoami).await
    }

    #[tool(
        description = "Set your one-line working-on summary. Update it when your task changes, not on every step."
    )]
    async fn status(
        &self,
        Parameters(p): Parameters<StatusParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            &s,
            Request::Status {
                working_on: p.working_on,
            },
        )
        .await
    }

    #[tool(
        description = "List, add or remove contacts. A contact is always a full address; names you send to resolve through your contacts first."
    )]
    async fn contacts(
        &self,
        Parameters(p): Parameters<ContactsParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        let req = match (p.action, p.address) {
            (ContactAction::List, _) => Request::Contacts,
            (ContactAction::Add, Some(address)) => Request::ContactAdd { address },
            (ContactAction::Remove, Some(address)) => Request::ContactRemove { address },
            (_, None) => return Ok(invalid("`address` is required to add or remove a contact")),
        };
        self.run(&s, req).await
    }

    #[tool(
        description = "List registered agents (active ones unless `all`), with their addresses and working-on summaries."
    )]
    async fn agents(
        &self,
        Parameters(p): Parameters<AgentsParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        // A relative repo resolves against the working directory, as the CLI's `--repo` does.
        let repo = match p
            .repo
            .as_deref()
            .map(|r| absolute(Path::new(r)))
            .transpose()
        {
            Ok(repo) => repo,
            Err(e) => return Ok(invalid(format!("cannot resolve repo: {e}"))),
        };
        self.run(
            &s,
            Request::Agents {
                name_prefix: p.name,
                repo,
                all: p.all,
            },
        )
        .await
    }

    #[tool(
        description = "Start a new thread. Send only changes, decisions and questions that affect the recipient."
    )]
    async fn send(
        &self,
        Parameters(p): Parameters<SendParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            &s,
            Request::Send {
                to: p.to,
                subject: p.subject,
                body: p.body,
                urgent: p.urgent,
                refs: p.refs,
            },
        )
        .await
    }

    #[tool(
        description = "Reply in a message's thread, to its sender and other recipients. Use it for clarifications; use `answer` to close a question."
    )]
    async fn reply(
        &self,
        Parameters(p): Parameters<ReplyParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.reply_req(&s, &p.message_id, p.body, p.urgent, p.refs, false)
            .await
    }

    #[tool(description = "Answer a question addressed to you, closing it.")]
    async fn answer(
        &self,
        Parameters(p): Parameters<AnswerParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.reply_req(&s, &p.question_id, p.body, p.urgent, p.refs, true)
            .await
    }

    #[tool(
        description = "Ask a question and wait for its answer (default 240 s). Returns status `answered`, `clarification` (reply, then `await_answer`), `pending` (continue other work or `await_answer`) or `no_one_listening`."
    )]
    async fn ask(
        &self,
        Parameters(p): Parameters<AskParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            &s,
            Request::Ask {
                to: p.to,
                subject: p.subject,
                body: p.body,
                refs: p.refs,
                urgent: p.urgent,
                timeout_secs: p.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            },
        )
        .await
    }

    #[tool(description = "Keep waiting for the answer to your question (default 240 s).")]
    async fn await_answer(
        &self,
        Parameters(p): Parameters<AwaitAnswerParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        let question_id = match self.full_id(&s, &p.question_id).await {
            Ok(id) => id,
            Err(e) => return Ok(error_result(&e)),
        };
        self.run(
            &s,
            Request::AwaitAnswer {
                question_id,
                timeout_secs: p.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            },
        )
        .await
    }

    #[tool(
        description = "Wait for new mail (default 240 s), holding your turn open meanwhile. Call it only when the human asks you to wait for your partner: when you finish, just stop, and you are woken for new mail where the harness supports it."
    )]
    async fn wait_for_mail(
        &self,
        Parameters(p): Parameters<WaitParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            &s,
            Request::WaitForMail {
                timeout_secs: p.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            },
        )
        .await
    }

    #[tool(
        description = "Your mail as summaries: open questions to you first, then unread messages (all messages with `all`). Each entry has a short `preview` and `body_len`; use `read` for the body, which also marks it read. Check it before each new step of your work."
    )]
    async fn inbox(
        &self,
        Parameters(p): Parameters<InboxParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match self.request(&s, Request::Inbox { all: p.all }).await {
                Ok(data) => json_result(&summarize_inbox(data), false),
                Err(e) => error_result(&e),
            },
        )
    }

    #[tool(description = "Read one message, or a whole thread by its thread id, and mark it read.")]
    async fn read(
        &self,
        Parameters(p): Parameters<ReadParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        let id = match self.full_id(&s, &p.id).await {
            Ok(id) => id,
            Err(e) => return Ok(error_result(&e)),
        };
        self.run(&s, Request::Read { id }).await
    }

    #[tool(description = "Search message subjects, bodies and senders.")]
    async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
        s: CallSession,
    ) -> Result<CallToolResult, McpError> {
        self.run(&s, Request::Search { query: p.query }).await
    }

    #[tool(description = "Mark yourself gone.")]
    async fn signoff(&self, s: CallSession) -> Result<CallToolResult, McpError> {
        self.run(&s, Request::Signoff).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentMailServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentmail", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve MCP on stdin/stdout until the client goes away.
pub async fn serve(paths: Paths) -> Result<()> {
    let service = AgentMailServer::new(paths)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_instructions_carry_the_guidance() {
        assert!(INSTRUCTIONS.starts_with(
            "agentmail lets you coordinate with other coding agents on this machine by mail.\n"
        ));
        assert!(INSTRUCTIONS.contains("Only use it when the human asks"));
        assert!(INSTRUCTIONS.ends_with(
            "call `wait_for_mail` only when the human asks you to wait for your partner."
        ));
        // Harness hooks wake an idle agent; waiting in a tool call at every
        // stop would hold the human's input instead.
        assert!(!INSTRUCTIONS.contains("about to stop, call `wait_for_mail`"));
        assert_eq!(INSTRUCTIONS.lines().count(), 9);
        // kiro-cli V3 cuts a server's instructions after 2048 characters
        // (JS string length; INSTRUCTIONS is ASCII, so the same count) and
        // appends "…[truncated]": past that, the guidance's end is lost.
        assert!(INSTRUCTIONS.is_ascii());
        assert!(INSTRUCTIONS.chars().count() <= 2048);
    }

    #[test]
    fn mcp_initial_caller_is_the_parent_or_the_address_env() {
        let p = ProcId {
            pid: 42,
            start_time: 7,
        };
        assert_eq!(
            initial_caller(None, Some(p)),
            Caller::Procs { procs: vec![p] }
        );
        assert_eq!(
            initial_caller(Some(String::new()), None),
            Caller::Procs { procs: vec![] }
        );
        assert_eq!(
            initial_caller(Some("a-12345678".into()), Some(p)),
            Caller::Address {
                address: "a-12345678".into()
            }
        );
    }

    #[test]
    fn mcp_session_from_meta() {
        let meta = |v: Value| {
            let mut m = RequestMetaObject::new();
            m.insert(OPENCODE_SESSION_META.into(), v);
            m
        };
        assert_eq!(
            session_from_meta(&meta(json!("ses_1"))),
            Some(SessionKey {
                harness: Harness::Opencode,
                id: "ses_1".into()
            })
        );
        assert_eq!(session_from_meta(&meta(json!(""))), None);
        assert_eq!(session_from_meta(&meta(json!(7))), None);
        assert_eq!(session_from_meta(&RequestMetaObject::new()), None);
    }

    #[test]
    fn mcp_caller_prefers_address_env_then_session() {
        let paths = Paths::from_vars(|k| {
            (k == "AGENTMAIL_DATA_DIR" || k == "AGENTMAIL_SOCKET")
                .then(|| std::ffi::OsString::from("/nonexistent"))
        })
        .unwrap();
        let procs = Caller::Procs { procs: vec![] };
        let server = |address_env: bool, caller: Caller| AgentMailServer {
            paths: paths.clone(),
            address_env,
            caller: Arc::new(Mutex::new(caller)),
            bind: None,
            shared: None,
            tool_router: AgentMailServer::tool_router(),
        };
        let key = SessionKey {
            harness: Harness::Opencode,
            id: "s1".into(),
        };
        let in_session = CallSession(Some(key.clone()));
        let s = server(false, procs.clone());
        assert_eq!(s.caller(&in_session), Caller::Session { session: key });
        assert_eq!(s.caller(&CallSession(None)), procs);
        let addr = Caller::Address {
            address: "a-12345678".into(),
        };
        let s = server(true, addr.clone());
        assert_eq!(s.caller(&in_session), addr);
    }

    #[test]
    fn mcp_register_caches_the_address_unless_overridden() {
        let paths = Paths::from_vars(|k| {
            (k == "AGENTMAIL_DATA_DIR" || k == "AGENTMAIL_SOCKET")
                .then(|| std::ffi::OsString::from("/nonexistent"))
        })
        .unwrap();
        let server = |address_env: bool, caller: Caller| AgentMailServer {
            paths: paths.clone(),
            address_env,
            caller: Arc::new(Mutex::new(caller)),
            bind: None,
            shared: None,
            tool_router: AgentMailServer::tool_router(),
        };
        let registered = json!({"address": "new-87654321"});
        let new = Caller::Address {
            address: "new-87654321".into(),
        };
        let none = CallSession(None);
        // Without the override, a sessionless register sets the caller.
        let s = server(false, Caller::Procs { procs: vec![] });
        s.remember_registered(&none, &registered);
        assert_eq!(s.caller(&none), new);
        // A register in a session caches nothing.
        let s = server(false, Caller::Procs { procs: vec![] });
        let in_session = CallSession(Some(SessionKey {
            harness: Harness::Opencode,
            id: "s1".into(),
        }));
        s.remember_registered(&in_session, &registered);
        assert_eq!(s.caller(&none), Caller::Procs { procs: vec![] });
        // AGENTMAIL_ADDRESS keeps overriding after a register.
        let env = Caller::Address {
            address: "env-12345678".into(),
        };
        let s = server(true, env.clone());
        s.remember_registered(&none, &registered);
        assert_eq!(s.caller(&none), env);
        // A server shared between sessions caches nothing either.
        let s = AgentMailServer {
            shared: Some(Harness::Opencode),
            ..server(false, Caller::Procs { procs: vec![] })
        };
        s.remember_registered(&none, &registered);
        assert_eq!(s.caller(&none), Caller::Procs { procs: vec![] });
    }

    #[tokio::test]
    async fn mcp_shared_server_without_binding_refuses_sessionless_register() {
        // opencode at PID 1 cannot be bound (`bind` is None) but still
        // shares the server: a call without an opencode session (none, or
        // another harness's) cannot register, and nothing is cached.
        let paths = Paths::from_vars(|k| {
            (k == "AGENTMAIL_DATA_DIR" || k == "AGENTMAIL_SOCKET")
                .then(|| std::ffi::OsString::from("/nonexistent"))
        })
        .unwrap();
        let s = AgentMailServer {
            paths,
            address_env: false,
            caller: Arc::new(Mutex::new(Caller::Procs { procs: vec![] })),
            bind: None,
            shared: Some(Harness::Opencode),
            tool_router: AgentMailServer::tool_router(),
        };
        let params = || {
            Parameters(RegisterParams {
                name: "intruder".into(),
                role: None,
                working_on: None,
                harness: None,
                fresh: false,
                wake: Some(true),
            })
        };
        let kiro = Some(SessionKey {
            harness: Harness::Kiro,
            id: "k1".into(),
        });
        for session in [None, kiro] {
            let r = s.register(params(), CallSession(session)).await.unwrap();
            assert_eq!(r.is_error, Some(true));
            let t = &r.content[0].as_text().unwrap().text;
            let v: Value = serde_json::from_str(t).unwrap();
            assert_eq!(v["error"]["code"], "InvalidInput", "{v}");
            let message = v["error"]["message"].as_str().unwrap();
            assert!(message.contains("shares it between"), "{message}");
            assert_eq!(
                s.caller(&CallSession(None)),
                Caller::Procs { procs: vec![] }
            );
        }
    }

    #[test]
    fn mcp_errors_carry_code_and_candidates() {
        let r = error_result(&ClientError::new(ErrorCode::AmbiguousRecipient, "pick"));
        assert_eq!(r.is_error, Some(true));
        let t = &r.content[0].as_text().unwrap().text;
        let v: Value = serde_json::from_str(t).unwrap();
        assert_eq!(v["error"]["code"], "AmbiguousRecipient");
        assert_eq!(v["error"]["message"], "pick");
        assert!(v["error"]["candidates"].is_array());
    }

    #[test]
    fn mcp_inbox_summaries_drop_the_body() {
        let long = "é".repeat(INBOX_PREVIEW_CHARS + 50);
        let inbox = json!([
            {"id": "a", "subject": "s", "body": "short", "from_contact": true},
            {"id": "b", "subject": "t", "body": long},
        ]);
        let v = summarize_inbox(inbox);
        let msgs = v.as_array().unwrap();
        assert!(msgs.iter().all(|m| m.get("body").is_none()), "{v}");
        assert_eq!(msgs[0]["preview"], "short");
        assert_eq!(msgs[0]["body_len"], 5);
        assert_eq!(msgs[0]["subject"], "s");
        assert_eq!(msgs[0]["from_contact"], true);
        let preview = msgs[1]["preview"].as_str().unwrap();
        assert_eq!(preview.chars().count(), INBOX_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'), "{preview}");
        assert_eq!(msgs[1]["body_len"], long.len());
        // Anything that is not a list of messages passes through.
        assert_eq!(summarize_inbox(json!({"ok": true})), json!({"ok": true}));
    }

    #[test]
    fn mcp_lists_every_tool() {
        let mut names: Vec<String> = AgentMailServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
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
            ]
        );
    }
}
