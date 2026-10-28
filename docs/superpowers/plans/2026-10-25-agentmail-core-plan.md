# agentmail core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the harness-agnostic core of agentmail: redb-backed daemon, Unix-socket protocol, full CLI and stdio MCP server. Agents and the human can register, pair, mail, ask blocking questions and wait for mail.

**Architecture:** One binary. `agentmail daemon run` is the only process that opens redb. It serves newline-delimited JSON over a Unix socket and pushes events to blocked callers through a tokio broadcast hub. The CLI and `agentmail mcp` are thin clients. Each starts the daemon on demand.

**Tech Stack:** Rust 2024 · tokio 1.54 · redb 4.4 · rmcp 3.7 (server, stdio) · clap 4.6 (derive) · serde/serde_json · cuid2 0.1 · ulid 3 · anyhow/thiserror · schemars 1 · tempfile 3 (dev) · assert_cmd (dev). Check exact APIs on docs.rs for the pinned versions. redb 4 and rmcp 3 differ from older tutorials.

**Spec:** `/tmp/claude-1000/-home-joseph-agentmail/2336ae47-2dc2-4171-a8e0-56d76c8d88bf/scratchpad/2026-10-25-agentmail-design.md`

**Out of scope for this plan (Plan 2, "harness integration"):** `agentmail hook`, `agentmail install`/`uninstall`, stop-hook waking wiring, urgent mid-run surfacing, the per-harness support table and per-harness e2e checks. This plan gives Plan 2 everything it calls: `wait`, inbox counts, `--json`, identity by process ancestry.

## Global Constraints

- Package manager: cargo (Rust). No JS in this plan.
- Name regex `^[a-z0-9][a-z0-9-]{0,31}$`. Address = `<name>-<cuid2>`, cuid2 length **8**, unique (re-roll on collision).
- Reserved address `human`. Messages from it carry `from_human: true`.
- Message body max **256 KiB** (262144 bytes). Working-on summary max **280** chars.
- Message ids are ULIDs. `thread_id` is the first message's id.
- `ask` default timeout **240 s**.
- Paths: DB `$XDG_DATA_HOME/agentmail/agentmail.redb` (default `~/.local/share/agentmail/`). Socket `$XDG_RUNTIME_DIR/agentmail.sock`, falling back to `$XDG_DATA_HOME/agentmail/agentmail.sock`, mode 0600. Test overrides: `AGENTMAIL_DATA_DIR`, `AGENTMAIL_SOCKET`.
- Only the daemon opens redb. No broadcast-to-all. A contact is always a full address, never a bare name.
- No identity means an error. Acting as human requires `--as human` or `AGENTMAIL_HUMAN=1`.
- Linux/WSL is the primary target. macOS process code may `unimplemented!()` behind `cfg`, but it must compile.
- Commits: conventional style (`feat:`, `test:`, `chore:`), ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Do not commit spec/plan markdown.

## Review Focus

1. **Markdown body round-trip.** A body piped on stdin containing code fences, tables, a trailing newline, non-ASCII and `\r\n` must come back byte-for-byte from `read --json`. A body over 256 KiB is refused with `TooLarge`, never truncated. Pinned in Task 9.
2. **Stale socket after a daemon crash.** A leftover socket file with no listener must not stop autostart. The client spawns a daemon, the daemon removes the stale file and binds. Pinned in Task 6.
3. **Two clients autostarting at once.** Two agents starting together must end with exactly one daemon, and both calls succeed. Pinned in Task 6.
4. **Daemon dies while a client is blocked in `ask`.** The client gets a clear `daemon connection lost` error. It does not hang, and the question survives a daemon restart. Pinned in Task 8.
5. **Unknown full address or self-send.** Sending to `backend-zzzzzzzz` that doesn't exist returns `UnknownRecipient`. Mail is never silently dropped. Sending to yourself is refused (`InvalidRecipient`). Pinned in Task 7.

---

## File Structure

```
Cargo.toml
src/main.rs            clap entry; dispatch to cli / daemon / mcp
src/lib.rs             module declarations, re-exports for tests
src/paths.rs           data dir, db path, socket path, lock path
src/model.rs           Agent, Message, Kind, Harness, ProcId, Question, Presence, views
src/ids.rs             validate_name, new_address, new_message_id
src/procinfo.rs        start time, liveness, ancestry, harness detection
src/store/mod.rs       Store::open, table defs, shared helpers
src/store/agents.rs    register/reclaim, status, signoff, contacts, lookups
src/store/mail.rs      insert, mailbox, read state, threads, held-by-name, search, prune
src/store/questions.rs questions, answer closing, wait graph / deadlock check
src/protocol.rs        PROTOCOL_VERSION, Caller, Request, Response, ErrorCode, Event
src/daemon/mod.rs      serve(), lock + stale socket, accept loop
src/daemon/handler.rs  request dispatch → store; identity + recipient resolution
src/daemon/hub.rs      broadcast Event hub; wait helpers
src/client.rs          Client::connect_or_spawn, call, stream
src/identity.rs        caller resolution for CLI/MCP
src/cli/mod.rs         clap command tree, body input
src/cli/render.rs      human-readable output
src/mcp.rs             rmcp server, tools, instructions text
tests/common/mod.rs    TestDaemon helper
tests/*.rs             integration tests per task
```

---

### Task 0: Branch, dependencies, skeleton

**Files:** Modify `Cargo.toml`, `src/main.rs`. Create `src/lib.rs`, `src/paths.rs`, `tests/common/mod.rs` (stub).

**Interfaces:**
- Produces: `paths::Paths { data_dir: PathBuf, db: PathBuf, socket: PathBuf, lock: PathBuf }`, `Paths::from_env() -> anyhow::Result<Paths>`, `Paths::in_dir(dir: &Path) -> Paths` (socket `dir/agentmail.sock`, used by tests).

- [ ] **Step 1: Git setup.** The repo has no commits. Once the user approves (see handoff), make the initial commit of the existing `Cargo.toml`, `.gitignore` and `src/main.rs` on `main`. Add `.worktrees/` to `.git/info/exclude`, then `git worktree add .worktrees/core -b feature/core main`. All further work happens in that worktree.
- [ ] **Step 2: Write failing test** `paths_from_env_honours_overrides` in `src/paths.rs`: with `AGENTMAIL_DATA_DIR=/x` and `AGENTMAIL_SOCKET=/y.sock`, assert `db == /x/agentmail.redb`, `socket == /y.sock`, `lock == /x/agentmail.lock`. Also test the XDG fallback: unset `XDG_RUNTIME_DIR` gives socket `data_dir/agentmail.sock`.
- [ ] **Step 3: Run** `cargo test paths` → FAIL (does not compile).
- [ ] **Step 4: Implement.** Add the dependencies listed in Tech Stack (`tokio` features `full`, `rmcp` features `server, transport-io, macros`, `clap` feature `derive`, `serde` feature `derive`, `ulid` feature `serde`). Implement `Paths`. Use `$HOME/.local/share` when `XDG_DATA_HOME` is unset.
- [ ] **Step 5: Run** `cargo test paths` → PASS. `cargo clippy -- -D warnings` clean.
- [ ] **Step 6: Commit** `chore: scaffold crate, dependencies and paths`.

### Task 1: Model and ids

**Files:** Create `src/model.rs`, `src/ids.rs`.

**Interfaces (Produces):**
```rust
pub const HUMAN: &str = "human";
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)] #[serde(rename_all="kebab-case")]
pub enum Harness { ClaudeCode, Opencode, Kiro, Other }
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProcId { pub pid: u32, pub start_time: u64 }
pub struct Agent { pub address: String, pub name: String, pub role: Option<String>,
  pub working_on: Option<String>, pub harness: Harness, pub cwd: PathBuf, pub repo: Option<PathBuf>,
  pub proc_id: Option<ProcId>, pub wake: bool, pub registered_at: i64, pub last_seen: i64, pub signed_off: bool }
#[serde(rename_all="lowercase")] pub enum Presence { Active, Gone }
#[serde(rename_all="lowercase")] pub enum Kind { Note, Question, Answer }
pub struct Message { pub id: String, pub thread_id: String, pub in_reply_to: Option<String>,
  pub from: String, pub to: Vec<String>, pub subject: String, pub body: String, pub kind: Kind,
  pub urgent: bool, pub refs: Vec<String>, pub created_at: i64, pub from_human: bool }
pub struct Question { pub id: String, pub thread_id: String, pub asker: String,
  pub to: Vec<String>, pub answered_by: Option<String> }
pub struct AgentView { #[serde(flatten)] pub agent: Agent, pub presence: Presence, pub is_contact: bool }
// all derive Serialize, Deserialize, Clone, Debug, PartialEq
// ids.rs
pub fn validate_name(name: &str) -> Result<(), String>;
pub fn new_address(name: &str) -> String;           // name + "-" + cuid2 len 8
pub fn new_message_id() -> String;                  // Ulid::new().to_string()
pub fn now_ms() -> i64;
```
Times are Unix milliseconds. `repo` is the git toplevel of `cwd`, or `None`.

- [ ] **Step 1: Failing tests** in `src/ids.rs`:
  - `validate_name("backend")`, `("orders-api")`, `("a")` and a 32-char name are Ok. `("")`, `("-x")`, `("Backend")`, `("a_b")`, a 33-char name and `("human")` are Err. `human` is reserved.
  - `new_address("backend")` matches `^backend-[a-z0-9]{8}$`. 1000 calls give 1000 distinct values.
  - `new_message_id()` called twice in order: second > first as strings.
- [ ] **Step 2: Run** `cargo test ids` → FAIL.
- [ ] **Step 3: Implement** `model.rs` (types only) and `ids.rs`. Build cuid2 with the crate's length option (`cuid2::CuidConstructor::new().with_length(8)` or the 0.1.6 equivalent).
- [ ] **Step 4: Run** `cargo test ids` → PASS.
- [ ] **Step 5: Commit** `feat: core model types and id generation`.

### Task 2: Process info

**Files:** Create `src/procinfo.rs`.

**Interfaces (Produces):**
```rust
pub fn proc_id(pid: u32) -> Option<ProcId>;          // reads /proc/<pid>/stat field 22
pub fn is_alive(p: &ProcId) -> bool;                 // proc_id(p.pid) == Some(*p)
pub fn parent_pid(pid: u32) -> Option<u32>;          // stat field 4
pub fn ancestors(pid: u32) -> Vec<ProcId>;           // parent-first chain, excludes pid itself, stops at 1
pub fn comm(pid: u32) -> Option<String>;             // /proc/<pid>/comm trimmed
pub fn detect_harness(comm: &str) -> Harness;        // "claude"→ClaudeCode, "opencode"→Opencode, "kiro-cli"|"kiro"→Kiro, else Other
pub fn find_harness_ancestor(pid: u32) -> Option<(ProcId, Harness)>; // first ancestor whose detect_harness != Other
```
Parse `stat` after the last `)`, because comm may contain spaces and parentheses.

- [ ] **Step 1: Failing tests:**
  - `proc_id(std::process::id())` is Some, and `is_alive` on it is true.
  - Spawn `sleep 30`, take its `ProcId`, kill and wait, then `is_alive` is false.
  - `ProcId { pid: me, start_time: real+1 }` is not alive (PID-reuse guard).
  - `ancestors(me)` is non-empty, and its first element's pid equals `parent_pid(me)`.
  - `detect_harness` table: `claude`, `opencode`, `kiro-cli`, `bash` → expected variants.
  - A stat parser unit test on the literal `"123 (we (ird) x) S 45 ..."` gives ppid 45.
- [ ] **Step 2: Run** `cargo test procinfo` → FAIL.
- [ ] **Step 3: Implement.** Gate on `#[cfg(target_os = "linux")]`. Other OSes return `None`/`false`/empty.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: linux process identity and ancestry`.

### Task 3: Store — agents and contacts

**Files:** Create `src/store/mod.rs`, `src/store/agents.rs`.

**Interfaces (Produces):**
```rust
pub struct Store { db: redb::Database }
impl Store { pub fn open(path: &Path) -> anyhow::Result<Store>; }
pub struct RegisterReq { pub name: String, pub role: Option<String>, pub working_on: Option<String>,
  pub harness: Harness, pub cwd: PathBuf, pub repo: Option<PathBuf>, pub proc_id: Option<ProcId>,
  pub fresh: bool, pub wake: bool }
pub type Alive<'a> = &'a dyn Fn(&ProcId) -> bool;
impl Store {
  pub fn register(&self, req: RegisterReq, now: i64, alive: Alive) -> Result<(Agent, bool /*reclaimed*/), StoreError>;
  pub fn presence(agent: &Agent, alive: Alive) -> Presence;  // signed_off || proc None || !alive → Gone
  pub fn get_agent(&self, addr: &str) -> Result<Option<Agent>, StoreError>;
  pub fn list_agents(&self) -> Result<Vec<Agent>, StoreError>;
  pub fn find_by_procs(&self, procs: &[ProcId]) -> Result<Option<Agent>, StoreError>; // first proc (in given order) bound to a non-signed-off agent
  pub fn touch(&self, addr: &str, now: i64) -> Result<(), StoreError>;
  pub fn set_status(&self, addr: &str, working_on: &str) -> Result<(), StoreError>;  // >280 chars → StoreError::TooLong
  pub fn signoff(&self, addr: &str) -> Result<(), StoreError>;
  pub fn add_contact(&self, owner: &str, contact: &str) -> Result<(), StoreError>;    // contact must exist; must be a full address
  pub fn remove_contact(&self, owner: &str, contact: &str) -> Result<(), StoreError>;
  pub fn contacts(&self, owner: &str) -> Result<Vec<String>, StoreError>;
}
#[derive(thiserror::Error)] pub enum StoreError { InvalidName(String), TooLong(&'static str), NotFound(String), Db(String) }
```
Tables (JSON values via serde_json): `AGENTS: &str → &[u8]`, `CONTACTS: (&str, &str) → ()`, `META: &str → &str` (`schema_version = "1"`). Other tasks add their own tables in `mod.rs`.

**Reclaim rule (from spec):** if `!fresh` and some agent has the same `name` and `cwd`, is Gone, and is not itself bound to a live process, then rebind the newest by `last_seen`. Rebinding sets the new `proc_id`, `signed_off=false`, updates `role`/`working_on`/`harness`/`wake`/`last_seen`, and keeps the address and contacts. An Active agent is never reclaimed.

- [ ] **Step 1: Failing tests** in `tests/store_agents.rs`, with a tempdir store and a fake `alive` closure built from a `HashSet<ProcId>`:
  - `register_assigns_address`: address starts with `backend-` and `reclaimed == false`.
  - `register_rejects_bad_name`: `"Back End"` → `InvalidName`.
  - `reclaim_same_name_same_cwd_when_gone`: register A (proc p1), mark p1 dead, register again with p2 → same address, `reclaimed == true`, `proc_id == p2`.
  - `no_reclaim_when_active`: p1 still alive → new address.
  - `no_reclaim_with_fresh` and `no_reclaim_different_cwd` → new address.
  - `reclaim_picks_most_recent`: two gone agents match, and the one with the larger `last_seen` is chosen.
  - `contacts_survive_reclaim`: add a contact, reclaim, `contacts()` unchanged.
  - `add_contact_requires_full_existing_address`: `"frontend"` → `NotFound`.
  - `status_limit`: 280 chars Ok, 281 → `TooLong`.
  - `find_by_procs_skips_signed_off`.
  - `reopen_persists`: drop the store, reopen the same file, agent still present.
- [ ] **Step 2: Run** `cargo test --test store_agents` → FAIL.
- [ ] **Step 3: Implement.** The register scan is a linear pass over AGENTS. That's fine at local scale.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: redb store for agents, reclaim and contacts`.

### Task 4: Store — messages, mailboxes, held mail

**Files:** Create `src/store/mail.rs`. Modify `src/store/mod.rs` (tables).

**Interfaces (Produces):**
```rust
pub struct NewMessage { pub from: String, pub to: Vec<String>, pub subject: Option<String>,
  pub body: String, pub kind: Kind, pub urgent: bool, pub refs: Vec<String>, pub in_reply_to: Option<String> }
pub const MAX_BODY: usize = 262_144;
impl Store {
  pub fn insert_message(&self, m: NewMessage, now: i64) -> Result<Message, StoreError>;
  pub fn hold_for_name(&self, repo: &Path, name: &str, msg_id: &str) -> Result<(), StoreError>;
  pub fn release_held(&self, repo: &Path, name: &str, addr: &str) -> Result<Vec<Message>, StoreError>; // delivers into addr's mailbox, appends addr to message.to
  pub fn inbox(&self, addr: &str, include_read: bool) -> Result<Vec<Message>, StoreError>;
  pub fn unread_counts(&self, addr: &str) -> Result<(usize /*unread*/, usize /*open questions to addr*/), StoreError>;
  pub fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError>;
  pub fn thread(&self, thread_id: &str) -> Result<Vec<Message>, StoreError>;     // ordered by id
  pub fn mark_read(&self, addr: &str, ids: &[String]) -> Result<(), StoreError>;
  pub fn threads(&self, f: &ThreadFilter) -> Result<Vec<ThreadSummary>, StoreError>;
  pub fn search(&self, q: &str) -> Result<Vec<Message>, StoreError>;           // case-insensitive substring over subject, body, from
  pub fn prune(&self, before: i64) -> Result<usize, StoreError>;               // deletes whole threads whose newest message < before
}
pub struct ThreadFilter { pub agent: Option<String>, pub repo: Option<PathBuf>, pub open_questions: bool }
pub struct ThreadSummary { pub thread_id: String, pub subject: String, pub participants: Vec<String>,
  pub messages: usize, pub last_at: i64, pub open_questions: usize }
```
Tables: `MESSAGES: &str → &[u8]`, `MAILBOX: (&str addr, &str msg_id) → bool read`, `THREADS: (&str thread, &str msg) → ()`, `HELD: (&str repo, &str name, &str msg) → ()`.

Rules:
- A body over `MAX_BODY` gives `TooLarge`. A new thread with no `subject` gives `InvalidInput("subject required")`.
- A reply's subject is `Re: <root subject>`, with no double `Re:`. Its `thread_id` is the parent's.
- `from_human = (from == HUMAN)`.
- Recipients are already resolved full addresses. A mailbox row is written per recipient, and **also for `human` when it replies in a thread it wasn't addressed in**. Nothing else is needed for that: `human` reads via `thread`.
- Inbox order: open questions addressed to `addr` first, then the rest by id.
- `insert_message` with `Kind::Question` creates the question row (Task 5 table; leave a hook call `self.on_question_inserted(&msg)` that Task 5 fills in).

- [ ] **Step 1: Failing tests** in `tests/store_mail.rs`:
  - `send_and_inbox`: A→B note. B's inbox has 1 message, and A's inbox has 0.
  - `mark_read_hides_from_unread_inbox`, and `include_read` shows it again.
  - `reply_threads_and_prefixes_subject`: reply to a reply has subject `Re: Orders`, same `thread_id`.
  - `body_too_large`: `MAX_BODY + 1` → `TooLarge`; exactly `MAX_BODY` → Ok.
  - `body_round_trip_exact`: body `"```rust\nfn x(){}\n```\r\n| a | b |\n| - | - |\nnaïve ✓\n"` comes back identical.
  - `held_mail_released_on_register`: `hold_for_name(repo,"frontend",id)` then `release_held` gives the message, it's in the new address's inbox, and its `to` contains the address.
  - `search_case_insensitive`, `threads_filter_by_agent`, `prune_removes_old_threads_only`.
  - `human_flag`: from `human` → `from_human == true`.
- [ ] **Step 2: Run** `cargo test --test store_mail` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: message storage, threads and held mail`.

### Task 5: Store — questions and deadlock guard

**Files:** Create `src/store/questions.rs`.

**Interfaces (Produces):**
```rust
impl Store {
  pub fn question(&self, id: &str) -> Result<Option<Question>, StoreError>;
  pub fn open_questions_to(&self, addr: &str) -> Result<Vec<Question>, StoreError>;
  pub fn would_deadlock(&self, asker: &str, to: &[String]) -> Result<Option<Question>, StoreError>;
  // Some(q) = an open question q such that some recipient in `to` reaches `asker` through open-question edges;
  // q is the first edge on that path (the question the new asker should answer)
}
```
Table `QUESTIONS: &str → &[u8]`. Edges: for each open question, `asker → each of to`. Cycle check: BFS from each `t in to` over the edges, and refuse if it reaches `asker`.

Closing: inserting a message with `Kind::Answer` whose `in_reply_to` is a question id, from someone other than the asker, sets `answered_by`. An `Answer` whose `in_reply_to` isn't an open question → `InvalidInput("not an open question")`. Other replies don't close.

- [ ] **Step 1: Failing tests** in `tests/store_questions.rs`:
  - `answer_closes_question`: A asks B, B answers → `answered_by == Some(B)`, and `open_questions_to(B)` is empty.
  - `note_reply_does_not_close`.
  - `direct_cycle_refused`: A asks B (open). `would_deadlock(B, [A])` → Some(A's question).
  - `long_cycle_refused`: A→B, B→C open. `would_deadlock(C, [A])` → Some.
  - `no_cycle_after_answer`: A→B answered. `would_deadlock(B,[A])` → None.
  - `asker_cannot_answer_own_question` → `InvalidInput`.
  - `inbox_orders_open_questions_first`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** (fill in `on_question_inserted` and the answer-closing logic).
- [ ] **Step 4: Run** `cargo test` (all store tests) → PASS.
- [ ] **Step 5: Commit** `feat: open questions and deadlock detection`.

### Task 6: Protocol, daemon, client, autostart

**Files:** Create `src/protocol.rs`, `src/daemon/mod.rs`, `src/daemon/handler.rs`, `src/daemon/hub.rs`, `src/client.rs`, `tests/common/mod.rs`. Modify `src/main.rs` (`daemon run|start|stop|status`).

**Interfaces (Produces):**
```rust
pub const PROTOCOL_VERSION: u32 = 1;
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Caller { Human, Address { address: String }, Procs { procs: Vec<ProcId> } }
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
  Hello { version: u32 },
  Register { name: String, role: Option<String>, working_on: Option<String>, harness: Harness,
             cwd: PathBuf, proc_id: Option<ProcId>, fresh: bool, wake: bool },
  Whoami, Status { working_on: String }, Signoff,
  Agents { name_prefix: Option<String>, repo: Option<PathBuf>, all: bool },
  ContactAdd { address: String }, ContactRemove { address: String }, Contacts,
  Send { to: Vec<String>, subject: String, body: String, urgent: bool, refs: Vec<String> },
  Reply { message_id: String, body: String, urgent: bool, refs: Vec<String>, answer: bool },
  Ask { to: Vec<String>, subject: String, body: String, refs: Vec<String>, urgent: bool, timeout_secs: u64 },
  AwaitAnswer { question_id: String, timeout_secs: u64 },
  WaitForMail { timeout_secs: u64 },
  Inbox { all: bool }, Counts, Read { id: String }, Threads { filter: ThreadFilter },
  Search { query: String }, Prune { older_than_secs: u64 }, Watch { agent: Option<String> },
  Shutdown,
}
pub struct Envelope { pub caller: Caller, pub req: Request }
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response { Ok { data: serde_json::Value }, Err { code: ErrorCode, message: String, candidates: Vec<AgentView> }, Event { event: Event } }
pub enum ErrorCode { VersionMismatch, NoIdentity, InvalidName, InvalidInput, InvalidRecipient,
  UnknownRecipient, AmbiguousRecipient, Deadlock, TooLarge, NotFound, ConnectionLost, Internal }
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event { Delivered { to: String, message: Message }, Presence { agent: AgentView } }

// daemon
pub struct DaemonConfig { pub paths: Paths }
pub async fn serve(cfg: DaemonConfig) -> anyhow::Result<()>;
// hub
pub struct Hub { tx: tokio::sync::broadcast::Sender<Event> }  // capacity 1024
// client
pub struct Client { /* framed UnixStream */ }
impl Client {
  pub async fn connect_or_spawn(paths: &Paths) -> anyhow::Result<Client>;
  pub async fn connect(paths: &Paths) -> anyhow::Result<Client>;
  pub async fn call(&mut self, caller: &Caller, req: Request) -> Result<serde_json::Value, ClientError>;
  pub async fn stream(&mut self, caller: &Caller, req: Request) -> Result<impl Stream<Item = Result<Event, ClientError>>, ClientError>;
}
pub struct ClientError { pub code: ErrorCode, pub message: String, pub candidates: Vec<AgentView> }
// tests/common
pub struct TestDaemon { pub dir: TempDir, pub paths: Paths, handle: JoinHandle<()> } // TestDaemon::start().await; drop aborts
```
Wire format: one JSON object per line. On connect the client sends `Hello` and expects `Ok`, or `Err{VersionMismatch}`. On `VersionMismatch` the client sends `Shutdown` and respawns once. Each connection then carries one request at a time.

Daemon startup: create `data_dir` with mode 0700, then `File::try_lock` (std) on `paths.lock`. If the lock is held, exit 0 quietly, because another daemon is starting. Next, remove any existing socket file, bind it, chmod 0600, open the Store, and accept. Store calls run in `tokio::task::spawn_blocking` with `Arc<Store>`. The handler resolves the caller, then `touch`es it.

Autostart: `connect` fails, so spawn `current_exe() daemon run` with null stdio and `process_group(0)`. Retry connecting every 25 ms for up to 3 s. `daemon start|stop|status` are thin wrappers: `stop` sends `Shutdown`.

This task implements the handler for `Hello`, `Register`, `Whoami`, `Agents`, `Status`, `Signoff`, `Contact*`, `Send` (full addresses only; names come in Task 7), `Inbox`, `Counts`, `Read` (marks read for the caller), `Threads`, `Search`, `Prune` and `Shutdown`. The `Caller::Address` and `Caller::Human` paths only. The other ops return `Internal("unimplemented")` until their task. `Register` runs `release_held` for (repo, name) and publishes `Event::Presence`. `Send` publishes `Event::Delivered` per recipient. The daemon computes `repo` with `git -C <cwd> rev-parse --show-toplevel` (None on failure).

- [ ] **Step 1: Failing tests** in `tests/daemon_basic.rs`:
  - `register_send_inbox_over_socket`: two registered addresses, A sends to B, B's `Inbox` has it, and after `Read` it's no longer unread.
  - `version_mismatch_rejected`: a raw socket `Hello{version:999}` → `Err VersionMismatch`.
  - `stale_socket_is_replaced`: create a plain file at `paths.socket`, then `Client::connect_or_spawn` (with `AGENTMAIL_*` pointing at the tempdir and the built binary via `env!("CARGO_BIN_EXE_agentmail")` as the spawn target) succeeds.
  - `concurrent_autostart_single_daemon`: two tasks call `connect_or_spawn` at once. Both succeed, and `pgrep -f "agentmail daemon run"` scoped to the tempdir env (or the lock file holder) shows exactly one daemon.
  - `mail_survives_restart`: send, `Shutdown`, start again, message still in the inbox.
  - `registered_presence_event`: a `Watch` stream receives `Presence` after a `Register`. Implement just enough of `Watch` for this test: forward hub events.
- [ ] **Step 2: Run** `cargo test --test daemon_basic` → FAIL.
- [ ] **Step 3: Implement.** Let the client find the binary path to spawn through an `AGENTMAIL_BIN` env override (tests) or `current_exe()`.
- [ ] **Step 4: Run** → PASS. Then `cargo test` → all PASS.
- [ ] **Step 5: Commit** `feat: daemon, wire protocol, client autostart`.

### Task 7: Identity and recipient resolution

**Files:** Create `src/identity.rs`. Modify `src/daemon/handler.rs`.

**Interfaces:**
- Consumes: `Store::find_by_procs`, `contacts`, `list_agents`, `hold_for_name`, `procinfo::*`.
- Produces:
```rust
// identity.rs (client side)
pub fn cli_caller(as_human: bool) -> Caller;
// AGENTMAIL_ADDRESS set → Address; else as_human || AGENTMAIL_HUMAN=="1" → Human;
// else Procs(procinfo::ancestors(std::process::id()))
pub fn register_proc(explicit_pid: Option<u32>) -> Option<(ProcId, Harness)>;
// explicit pid → (proc_id(pid), detect_harness(comm)); else find_harness_ancestor(self);
// else the grandparent (shell's parent) with Harness::Other
// handler.rs (daemon side)
fn resolve_caller(&self, c: &Caller) -> Result<String /*address or "human"*/, ClientError>;
// Procs with no match → NoIdentity, message: "this session is not registered: run `agentmail register <name>` (or use --as human)"
fn resolve_recipients(&self, sender: &str, to: &[String]) -> Result<Resolved, ClientError>;
pub struct Resolved { pub addresses: Vec<String>, pub held_names: Vec<(PathBuf, String)> }
```
Resolution per `to` entry:
1. It contains `-` and matches an existing agent address, or it is `human` → use it.
2. It matches the full-address shape (`name-[a-z0-9]{8}`) but is unknown → `UnknownRecipient`.
3. Otherwise it's a plain name. Use the sender's contact with that name if there's exactly one. Next, Active agents with that name in the sender's repo (for `human`: machine-wide). One → use it. Several → `AmbiguousRecipient` with `candidates`, each an `AgentView` carrying `working_on`, so the agent can choose. None and the sender has a repo → held for (repo, name). None and no repo → `UnknownRecipient`.
4. If a resolved recipient equals the sender → `InvalidRecipient`.

A name with a valid address shape could also be a plain name (`frontend-a1b2c3d4` as a chosen name is impossible, since names are at most 32 chars and the suffix is generated). Rule 2 takes priority.

- [ ] **Step 1: Failing tests** in `tests/resolution.rs` (using TestDaemon + Client):
  - `procs_caller_resolves_to_registered_agent`: register with `proc_id = proc_id(own pid)`, call `Whoami` with `Caller::Procs([own])` → the address.
  - `unregistered_procs_is_no_identity`.
  - `name_resolves_to_contact_first`: two `frontend` agents active in the same repo, and A has one as a contact → that one.
  - `ambiguous_name_lists_candidates_with_summaries`: no contact → `AmbiguousRecipient`, 2 candidates, `working_on` present.
  - `name_in_same_repo_only`: a `frontend` in another repo is ignored.
  - `name_held_until_registration`: send to `frontend` with none registered → Ok. Register `frontend` in the same repo → its inbox has the message.
  - `unknown_full_address_rejected` (`frontend-zzzzzzzz`) → `UnknownRecipient`.
  - `self_send_rejected` → `InvalidRecipient`.
  - Unit test of `cli_caller`: with env `AGENTMAIL_ADDRESS=x` → `Address`. With `AGENTMAIL_HUMAN=1` → `Human`. With neither → `Procs` (non-empty).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Make the test repos with `git init` in tempdirs.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: caller identity and name/contact recipient resolution`.

### Task 8: Blocking ask, answers, waits, presence

**Files:** Modify `src/daemon/handler.rs`, `src/daemon/hub.rs`.

**Interfaces (Produces): response `data` shapes**
```json
ask / await_answer → {"status":"answered","question_id":..,"answer":Message}
                   | {"status":"clarification","question_id":..,"message":Message}
                   | {"status":"pending","question_id":..}
                   | {"status":"no_one_listening","question_id":..}
wait_for_mail      → {"status":"mail","messages":[Message..]} | {"status":"timeout"}
reply(answer=true) → Message   (kind=answer; in_reply_to must be an open question)
```
Behavior:
- `Ask`: run `would_deadlock` first → `Deadlock`, with a message naming the question to answer, or suggesting a clarifying reply in that thread. Insert the question and publish. If every resolved recipient is Gone (`Store::presence` with `procinfo::is_alive`) → `no_one_listening` right away. Held-by-name recipients count as not listening. Otherwise subscribe to the hub **before** inserting, so no event is missed. Wait up to `timeout_secs` for a `Delivered` event to the asker in the same thread from someone else: an `answer` to this question → `answered`, anything else → `clarification`. On timeout → `pending`.
- `AwaitAnswer`: if already answered → `answered` right away. Otherwise wait as above.
- `WaitForMail`: if the caller has unread mail → return it right away. Otherwise wait for the first `Delivered` to the caller, or time out.
- `Watch`: stream every hub event (filtered by `agent` when given) as `Response::Event` lines until the client disconnects.
- Presence in `Agents`/`Whoami` uses `is_alive`. Agents whose process died are reported Gone with no write needed.
- Client side: if the socket closes during any call → `ClientError{code: ConnectionLost, message: "daemon connection lost"}`.

- [ ] **Step 1: Failing tests** in `tests/blocking.rs`. Agent processes are faked by registering with the test process's own `ProcId` (alive), or a dead `ProcId` from a reaped `sleep` (gone).
  - `ask_returns_answer`: A asks B (timeout 5). A concurrent task as B answers after 100 ms → `answered`, with the body matching.
  - `ask_returns_clarification_then_await_gets_answer`: B replies with a plain note → `clarification`. Then B answers, and A's `AwaitAnswer` → `answered`.
  - `ask_times_out_pending`: timeout 1 → `pending` in ≤ 2 s, and the question is still open.
  - `ask_gone_recipient_no_one_listening`: B's proc dead → immediate `no_one_listening`, and the question is stored.
  - `ask_deadlock_refused`: A→B open, then B asks A → `Deadlock`.
  - `wait_for_mail_wakes_on_delivery` and `wait_for_mail_returns_existing_unread_immediately`.
  - `daemon_death_during_ask_is_connection_lost`: A asks with timeout 30, the daemon handle is aborted → the client gets `ConnectionLost` within 1 s. After restart, `AwaitAnswer` on the same id → `pending` with timeout 1 (the question persisted).
- [ ] **Step 2: Run** `cargo test --test blocking` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: blocking ask/answer, wait for mail, watch`.

### Task 9: CLI

**Files:** Create `src/cli/mod.rs`, `src/cli/render.rs`. Modify `src/main.rs`.

**Interfaces:**
- Consumes: `Client`, `identity::cli_caller`, `identity::register_proc`, every `Request` op.
- Produces: the command surface, exactly as in the spec's CLI section, plus `daemon run|start|stop|status`. Global flags: `--as <who>` (only `human` accepted), `--json`. Body options on `send`/`reply`/`ask`/`answer`: `--body <text>` | `--body-file <path>` | `-` (stdin). Exactly one is required, otherwise exit 2 with a usage error. `--to` accepts comma-separated values and repeats. Durations use humantime-style `240s`/`10m`/`7d` (one small parser, no new crate needed).
- `register` sends `cwd = std::env::current_dir()` and `proc_id`/`harness` from `register_proc(--pid)`, with `--harness` overriding detection.
- Exit codes: 0 OK. 1 error. 3 `pending`/`timeout`/`no_one_listening` from `ask`/`await`/`wait`, so scripts and Plan 2 hooks can branch.
- `render.rs`: `inbox` shows one line per message: `[Q]`/`[A]`/blank marker, `!` for urgent, id short (last 8), from, subject, age. `show <thread>` shows the full thread with a header per message (`── from → to · time · kind`) followed by the raw markdown body. `agents` is a table: address, presence, harness, repo, working-on, last-seen ("idle 14m"), with a `*` marking contacts. `watch` prints one line per event. `--json` prints the raw `data` value (or one event per line for `watch`).
- Errors print `error: <message>`. `AmbiguousRecipient` also prints the candidate table.

- [ ] **Step 1: Failing tests** in `tests/cli.rs` using `assert_cmd` against `CARGO_BIN_EXE_agentmail`, with `AGENTMAIL_DATA_DIR`/`AGENTMAIL_SOCKET` pointing at a tempdir. Agents are simulated with `AGENTMAIL_ADDRESS` taken from `register --json --pid <pid of a spawned sleep>`. Stop the daemon in teardown.
  - `stdin_markdown_round_trip`: `send --to <B> --subject s -` with stdin = the Task 4 body → `read <id> --json` `.body` is byte-identical.
  - `body_file_and_inline_equivalent`.
  - `missing_body_is_usage_error` → exit 2.
  - `oversize_body_rejected`: 262145-byte stdin → exit 1, stderr contains `too large`.
  - `no_identity_errors`: no env, `inbox` → exit 1, stderr contains `not registered`.
  - `human_can_join_any_thread`: A→B thread. `--as human reply <id> --body x` → `show <thread>` lists `human` with the `from_human` marker.
  - `ask_pending_exit_code_3`: `ask --timeout 1s` → exit 3, and the JSON status is `pending`.
  - `answer_then_await`: `answer <qid> --body ok`, then `await <qid> --json` → `answered`.
  - `contact_add_and_agents_marks_contact`.
  - `status_and_whoami_show_summary`.
  - `prune_older_than`.
- [ ] **Step 2: Run** `cargo test --test cli` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test` → PASS. `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `feat: full CLI with markdown body input and renderers`.

### Task 10: MCP server

**Files:** Create `src/mcp.rs`. Modify `src/main.rs` (`agentmail mcp`).

**Interfaces:**
- Consumes: `Client`, `procinfo::{proc_id, comm, detect_harness, parent_pid}`, every `Request`.
- Produces: an rmcp stdio server named `agentmail` with tools `register`, `whoami`, `status`, `contacts` (`action: list|add|remove`, `address?`), `agents`, `send`, `reply`, `answer`, `ask`, `await_answer`, `wait_for_mail`, `inbox`, `read`, `search`, `signoff`. Parameters match the CLI flags (schemars-derived input structs; `to: Vec<String>`; `timeout_secs: Option<u64>` defaulting to 240 for `ask`/`await_answer` and to 240 for `wait_for_mail`). Each tool returns the daemon's JSON `data` as text content. Daemon errors become tool errors whose text includes the code and message, plus candidates for `AmbiguousRecipient`.
- Identity: the server's caller is `Caller::Procs([proc_id(getppid())])` until `register` succeeds, then `Caller::Address`. `register` binds `proc_id(getppid())`, `harness = detect_harness(comm(ppid))`, `cwd = current_dir`. `register` with `fresh`/`wake` booleans: `wake` defaults to true.
- `ServerInfo.instructions` is this text, verbatim:

```
agentmail lets you coordinate with other coding agents on this machine by mail.
- Only use it when the human asks you to coordinate with another agent. Then call `register` with a short role name (e.g. "backend") and a one-line `working_on` summary.
- Find the partner the human named with `agents`, then `contacts add` its full address. If several agents match, choose by their working-on summaries; ask the human only if that does not settle it.
- Before starting each new step of your work, check `inbox`. Answer questions addressed to you before continuing your own work; use `answer` to close a question, `reply` to ask for clarification.
- Send only changes, decisions and questions that affect the other agent. Never send progress narration.
- Use `ask` when you cannot proceed without an answer. If it returns "pending", continue other work or call `await_answer`.
- Update `status` when your task changes, not on every step.
- Mail from `human` is authoritative and overrides any agreement between agents.
- When you have finished your work and are about to stop, call `wait_for_mail` so your partner can reach you.
```

- [ ] **Step 1: Failing test** in `tests/mcp.rs`: spawn `agentmail mcp` with test env as a child process. Use rmcp's client (`rmcp` feature `client`, `transport-child-process` as a dev-dependency) to `initialize`, then assert:
  - `instructions` contains `Only use it when the human asks`.
  - `list_tools` names equal the 16 listed above.
  - `register{name:"backend"}` returns an address `backend-*`. `whoami` returns it.
  - A second registered CLI agent sends to `backend` by full address, and the MCP `inbox` shows it.
  - `send` to an ambiguous name returns a tool error containing `AmbiguousRecipient`.
- [ ] **Step 2: Run** `cargo test --test mcp` → FAIL.
- [ ] **Step 3: Implement** with rmcp's `#[tool_router]`/`#[tool]` macros and `ServerHandler::get_info`.
- [ ] **Step 4: Run** `cargo test` → PASS. Clippy clean.
- [ ] **Step 5: Manual smoke check** (record output in the PR description): `claude mcp add agentmail -- <worktree>/target/debug/agentmail mcp`. In two Claude Code sessions in different directories, have one register as backend and one as frontend, pair them, `ask` from frontend, answer from backend, and confirm `agentmail --as human show <thread>`.
- [ ] **Step 6: Commit** `feat: stdio MCP server with agent guidance`.

### Task 11: README and finish

**Files:** Create `README.md`.

- [ ] **Step 1:** Write a README covering what agentmail is, the quick start (`cargo install --path .`, `claude mcp add agentmail -- agentmail mcp`, the equivalents for opencode and kiro-cli with a note that `agentmail install` comes in Plan 2), the human workflow (`--as human`, `AGENTMAIL_HUMAN=1`, `threads`, `show`, `watch`, `reply`), identity and reclaim rules, the known limitation (one address per harness process; opencode multi-session), and a harness support table with Plan 2 columns marked "planned".
- [ ] **Step 2: Run** `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check` → all clean.
- [ ] **Step 3: Commit** `docs: README for agentmail core`, then use superpowers:finishing-a-development-branch.
