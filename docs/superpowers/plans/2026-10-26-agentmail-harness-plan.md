# agentmail harness integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make agentmail work end to end in Claude Code, opencode 2.x and kiro-cli V3. That means session-aware identity, hooks and plugins that remind, interrupt and wake agents, one-command install and uninstall, and real-harness e2e scripts.

**Architecture:** The core (branch `feature/core`) gains a second identity binding, (harness, session id), and a per-recipient "surfaced" flag. A new `agentmail hook <event> --harness <h>` subcommand turns daemon state into each harness's hook output format. opencode has no shell hooks, so it gets an embedded JS plugin (V2 API) that calls the same subcommand. `agentmail install` edits each harness's config files directly, with backups.

**Tech Stack:** Rust (the existing crate), JS for the opencode plugin (Bun runtime inside opencode; `bun test` for its unit tests, no package manager, no dependencies), shell for e2e scripts.

**Spec:** `docs/superpowers/specs/2026-10-25-agentmail-design.md`, especially "Harness integration (revision 2026-10-26)". Both the spec and this plan are git-excluded and must never be committed.

## Global Constraints

- Work in the git worktree `.worktrees/harness` on branch `feature/harness`, created from `feature/core`. Never touch `main`. Never use bare `git stash`.
- Commits are conventional and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never commit `docs/superpowers/`.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before every commit, plus `bun test plugin/` once the plugin exists.
- Supported harnesses: **Claude Code**; **opencode ≥ 2.0** with the V2 plugin API `export default { id, setup }`; **kiro-cli V3 only** (`--v3`). kiro's V2 engine is not supported. Install and the README must say so.
- Identity resolution order: explicit `--as human`, then `AGENTMAIL_ADDRESS`, then `AGENTMAIL_HUMAN=1`, then **session**, then process ancestry.
- Session id sources: opencode `_meta["ai.opencode/sessionID"]` (MCP), `OPENCODE_SESSION_ID` (shell), `sessionID` (plugin). Claude Code `session_id` (hook stdin). kiro `KIRO_SESSION_ID` (hook env).
- Hooks never start the daemon. If it is down, or the session is unregistered, a hook prints nothing and exits 0. Non-waiting hook paths must finish in < 20 ms with the daemon up (measured in a test).
- Urgent mail is pushed mid-run once per recipient (the surfaced flag), separate from read state.
- Install honours `CLAUDE_CONFIG_DIR`, `XDG_CONFIG_HOME` and `KIRO_HOME`. It writes `<file>.agentmail.bak` before modifying any existing file, touches only entries it owns, is idempotent, and supports `--dry-run` and `uninstall`. It never shells out to `claude`, which may be a shell function.
- Unverified harness behaviour must be confirmed by the spike step that opens its task. Record the result in `docs/harness-notes.md`, which **is** committed because it's product documentation. Where a spike contradicts this plan, follow the decision rule given in that task.

## Review Focus

1. **A hook during daemon downtime or on an unregistered session** must print nothing, exit 0, and never spawn a daemon. Otherwise every Claude Code session on the machine starts a daemon or shows errors. Pinned in Task 4.
2. **Config files with user content.** Install into a `settings.json` or `opencode.json` that already has unrelated hooks, servers and comments must preserve them byte-for-byte except our entries. Uninstall must restore an equivalent file. Running install twice must not duplicate anything. Pinned in Tasks 5, 7 and 8.
3. **Two opencode sessions in one directory**, sharing one MCP server process, must get different addresses and see only their own mail. Pinned in Tasks 1 and 3.
4. **A wake loop that never ends.** The wait hook must exit when the agent signs off, when its session or process is gone, or when the daemon stops. It must never spin the CPU, and it must not wake for mail the agent already read. Pinned in Tasks 4, 6 and 8.
5. **Old databases.** A schema-1 database written by the core build must open and migrate to schema 2 with read state preserved. A schema-2 database must be refused by an old binary with a clear message (already handled by the existing version check). Pinned in Task 2.

---

## File Structure

```
src/model.rs              + SessionKey
src/store/agents.rs       session binding, find_by_session, register idempotence per session
src/store/mail.rs         mailbox flags (read | surfaced), unsurfaced_urgent(), mark_surfaced()
src/store/mod.rs          SESSIONS table, schema 2 + migration from 1
src/protocol.rs           Caller::Session, Register.session, Request::HookStatus, PROTOCOL_VERSION = 2
src/daemon/handler.rs     session resolution, HookStatus, wait-for-unread semantics
src/identity.rs           session from env; MCP bind prefers harness ancestor
src/mcp.rs                per-request caller from _meta session id
src/hook/mod.rs           `agentmail hook` entry: read stdin, resolve, dispatch
src/hook/text.rs          reminder / urgent / question text (shared)
src/hook/claude.rs        Claude Code output formats
src/hook/kiro.rs          kiro V3 output formats (exit codes + stdio)
src/hook/opencode.rs      JSON status for the plugin
src/install/mod.rs        install/uninstall entry, backup, dry-run diff, JSON merge helpers
src/install/claude.rs
src/install/opencode.rs
src/install/kiro.rs
src/install/service.rs    systemd user unit
plugin/agentmail.js       opencode V2 plugin (embedded via include_str!)
plugin/agentmail.test.js  bun tests with a mocked ctx
tests/fixtures/hooks/<harness>/<event>.json   recorded stdin payloads
tests/session.rs, tests/hook_claude.rs, tests/hook_kiro.rs, tests/hook_opencode.rs, tests/install.rs
scripts/e2e.sh            real-harness e2e driver
docs/harness-notes.md     spike results + support table source
```

---

### Task 0: Worktree

- [ ] **Step 1:** From the repo root: `git worktree add .worktrees/harness -b feature/harness feature/core`. Then copy `docs/superpowers/` from `.worktrees/core` into it. It's git-excluded, so this is a plain copy. Run `cargo test` and expect everything to pass.

### Task 1: Session binding in the core

**Files:** `src/model.rs`, `src/store/mod.rs`, `src/store/agents.rs`, `src/protocol.rs`, `src/daemon/handler.rs`. Test: `tests/session.rs`.

**Interfaces (Produces):**
```rust
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct SessionKey { pub harness: Harness, pub id: String }   // id: 1..=128 chars, no control chars
// Agent gains:            pub session: Option<SessionKey>   (#[serde(default)])
// RegisterReq gains:      pub session: Option<SessionKey>
// Request::Register gains: #[serde(default)] session: Option<SessionKey>
// Caller gains:           Session { session: SessionKey }
impl Store {
  pub fn find_by_session(&self, key: &SessionKey) -> Result<Option<Agent>, StoreError>; // non-signed-off only
}
pub const PROTOCOL_VERSION: u32 = 2;
```
Table: `SESSIONS: (&str harness, &str id) → &str address`.

Rules:
- **Register with `session = Some(k)`.** If a non-signed-off agent is bound to `k`, return it, updating role, working_on, wake, proc_id and last_seen, and report `reclaimed: false, existing: true`. The name must match, otherwise `InvalidInput("session already registered as <address>")`. Otherwise apply the normal reclaim and new-address rules, then bind `k`.
- **Signoff** removes the SESSIONS row.
- **`Caller::Session`** resolves via `find_by_session`. If there's no match → `NoIdentity`. It never falls through to procs, because the daemon can't see the caller's ancestry.
- **Presence** is unchanged: proc alive and not signed off. A session-bound opencode agent's `proc_id` is the opencode service process.

- [ ] **Step 1: Failing tests** in `tests/session.rs` (TestDaemon + Client):
  - `two_sessions_same_proc_get_distinct_addresses`: same `proc_id`, sessions `s1` and `s2`, names `backend`/`frontend` → different addresses. `Whoami` with `Caller::Session{s1}` → the first address.
  - `register_same_session_is_idempotent`: same address both times, and the role update is applied.
  - `same_session_different_name_rejected` → `InvalidInput`.
  - `session_caller_unregistered_is_no_identity`.
  - `signoff_frees_session`: after signoff, registering `s1` again gets a **new** or reclaimed address via the normal rules, not the signed-off binding.
  - `session_mail_isolated`: mail to s1's address isn't in s2's inbox.
  - `old_protocol_client_rejected`: `Hello{version:1}` → `VersionMismatch`.
- [ ] **Step 2: Run** `cargo test --test session` → FAIL.
- [ ] **Step 3: Implement.** Bump `PROTOCOL_VERSION` to 2, and update any test that hard-codes 1.
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: bind agents to harness sessions`.

### Task 2: Surfaced flag, hook status and schema 2

**Files:** `src/store/mod.rs`, `src/store/mail.rs`, `src/protocol.rs`, `src/daemon/handler.rs`. Tests: `tests/store_mail.rs`, `tests/session.rs`, and a unit test in `src/store/mod.rs`.

**Interfaces (Produces):**
```rust
// MAILBOX value changes from bool to u8 flags: READ = 0b01, SURFACED = 0b10
pub const SCHEMA_VERSION: &str = "2";
impl Store {
  pub fn unsurfaced_urgent(&self, addr: &str) -> Result<Vec<Message>, StoreError>; // urgent, unread, not surfaced, oldest first
  pub fn mark_surfaced(&self, addr: &str, ids: &[String]) -> Result<(), StoreError>;
}
Request::HookStatus { surface_urgent: bool }
// → {"address": String, "name": String, "wake": bool, "unread": usize,
//    "open_questions": [{"id","from","subject"}],   // to this agent, oldest first
//    "urgent": [Message]}                           // unsurfaced; marked surfaced iff surface_urgent
Request::WaitForMail { timeout_secs: u64 }   // unchanged shape; semantics pinned below
```
Migration: opening a schema-1 DB rewrites every MAILBOX row (`true → READ`, `false → 0`) in one write transaction, then sets the version to 2. Use a temporary table and rename, or read all rows, delete and reinsert. Either is fine at local scale.

`WaitForMail` semantics, made explicit because the wake hooks depend on them: return right away if the caller has **unread** mail. Otherwise block until a `Delivered` event to the caller, or time out. A message that was surfaced but not read still counts as unread.

- [ ] **Step 1: Failing tests:**
  - `migrates_schema_1_mailbox`: build a v1 database with the redb API directly (a bool MAILBOX, `schema_version = "1"`, one read row and one unread row). `Store::open` → version "2", and `inbox(addr, false)` returns only the unread one.
  - `urgent_surfaced_once`: an urgent message arrives. `HookStatus{surface_urgent:true}` → `urgent.len()==1`. The second call → 0. `inbox` still shows it as unread.
  - `hook_status_lists_open_questions_and_counts`.
  - `hook_status_unregistered_is_no_identity`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: surfaced flag, hook status request, schema 2 migration`.

### Task 3: Session-aware CLI and MCP identity

**Files:** `src/identity.rs`, `src/mcp.rs`, `src/cli/mod.rs`. Tests: unit tests in `identity.rs`, `tests/mcp.rs`, `tests/cli.rs`.

**Interfaces (Produces):**
```rust
pub fn session_from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<SessionKey>;
// OPENCODE_SESSION_ID → (Opencode, id); KIRO_SESSION_ID → (Kiro, id); first non-empty wins in that order
// cli_caller order: as_human → AGENTMAIL_ADDRESS → AGENTMAIL_HUMAN=1 → session_from_env → Procs
pub fn register_proc(explicit_pid: Option<u32>) -> Option<(ProcId, Harness)>;
// CHANGED: with explicit_pid, prefer find_harness_ancestor(explicit_pid's child chain, i.e. starting at
// our own pid); fall back to (explicit pid, detect_harness(comm)) only if no harness ancestor exists.
```
- CLI `register` also sends `session: session_from_env(...)`.
- **MCP:** keep the process-level caller for Claude Code and kiro. For opencode, read `_meta["ai.opencode/sessionID"]` from `RequestContext.meta` on each tool call. If it's present, the caller for that call is `Caller::Session{Opencode, id}`, and `register` sends that session. Remove the single "registered address" cache for session callers. Each call resolves through the daemon.
- **Why the bind change:** under kiro V3 the MCP server's parent is `node`, not `kiro-cli`. Binding the parent would record `Harness::Other`.

- [ ] **Step 1: Failing tests:**
  - `session_from_env` table tests: opencode var, kiro var, both set (opencode wins), empty → None.
  - `cli_caller_prefers_session_over_procs`.
  - `tests/mcp.rs::opencode_meta_session_isolates_callers`: one MCP child. `register` via a call with `_meta {"ai.opencode/sessionID":"s1"}` as backend, then with `s2` as frontend. `whoami` with s1 meta → the backend address, with s2 meta → the frontend address. Use the rmcp client's request-meta support. If the test client can't set `_meta`, send raw JSON-RPC lines over the child's stdio.
  - `tests/mcp.rs::bind_prefers_harness_ancestor`: spawn the MCP server under a wrapper process named `kiro-cli` (copy `/bin/sh` or a tiny binary into a temp dir as `kiro-cli` and exec through it), with an intermediate process. `register` → harness `kiro`.
  - `tests/cli.rs::opencode_env_session_register_and_whoami`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: session-aware identity in CLI and MCP`.

### Task 4: `agentmail hook` framework and Claude Code hooks

**Files:** Create `src/hook/mod.rs`, `src/hook/text.rs`, `src/hook/claude.rs`, `tests/fixtures/hooks/claude-code/*.json`, `tests/hook_claude.rs`, `docs/harness-notes.md`. Modify `src/cli/mod.rs` (subcommand) and `src/main.rs`.

**Spike first (Step 1):** confirm against current Claude Code (binary `~/.local/share/claude/versions/<latest>`, passed as `AGENTMAIL_E2E_CLAUDE`) in a throwaway `CLAUDE_CONFIG_DIR`:
- (a) the exact config for an async hook that wakes the session (`"async": true` plus the `asyncRewake` key/shape), and that exit code 2 from it wakes an idle interactive session;
- (b) the command-hook default and maximum `timeout`;
- (c) that hook commands are descendants of the `claude` process;
- (d) real stdin payloads for SessionStart, UserPromptSubmit, PostToolUse and Stop.

Save (d) as fixtures, and write (a)–(c) up in `docs/harness-notes.md`. **Decision rule:** if `asyncRewake` doesn't exist or doesn't wake an idle session, waking falls back to a synchronous Stop hook that waits up to 540 s, then allows the stop. That holds the turn open while the human can still press Esc. Record which path was taken.

**Interfaces (Produces):**
```rust
#[derive(clap::ValueEnum, Clone, Copy)] pub enum HookEvent { SessionStart, Prompt, PostTool, PreTool, Stop, Wait, Status }
pub struct HookInput { pub raw: serde_json::Value, pub session_id: Option<String>, pub stop_hook_active: bool }
pub async fn run(harness: Harness, event: HookEvent, stdin: &[u8], env: &dyn Fn(&str) -> Option<OsString>) -> HookOutput;
pub struct HookOutput { pub stdout: String, pub stderr: String, pub exit: u8 }
// text.rs
pub fn reminder(status: &HookStatus) -> Option<String>;   // None when unread==0 && no open questions
// e.g. "📬 agentmail — you are backend-k3x9q2mz: 2 unread (1 question from frontend-p7v2c8wd: \"POST /orders body?\"). Check your inbox before continuing."
pub fn urgent(msgs: &[Message]) -> Option<String>;         // full bodies, each capped at 4 KiB with "… (truncated; read <id>)"
pub fn open_questions(status: &HookStatus) -> Option<String>;
pub fn guidance_reminder(status: &HookStatus) -> String;   // one line: address + "check inbox between steps; answer questions first"
```
Identity for hooks: Claude Code and kiro use `Caller::Procs(ancestors(self))`. opencode uses `--session <id>`. Connect with `Client::connect` (never spawn). Any connection error or `NoIdentity` → `HookOutput::default()` (empty, exit 0).

Claude Code output, per event:
- `session-start`, `prompt`: `{"hookSpecificOutput":{"hookEventName":"SessionStart"|"UserPromptSubmit","additionalContext": guidance_reminder + reminder}}`. Empty if unregistered.
- `post-tool`: `HookStatus{surface_urgent:true}`. `additionalContext` = urgent text if any, else reminder. Empty when there's nothing.
- `stop`: if there are open questions to the agent and `!stop_hook_active` → `{"decision":"block","reason": open_questions text}`. Otherwise empty.
- `wait`: if `!wake` → exit 0. Otherwise loop `WaitForMail{timeout_secs: 300}` until mail arrives, the agent is gone or signed off, or the connection is lost. On mail → stderr = reminder, exit 2. Otherwise exit 0. (Or the fallback per the decision rule.)

- [ ] **Step 2: Failing tests** in `tests/hook_claude.rs`. Drive `run` directly with fixture payloads and a TestDaemon. Supply identity through a registration bound to the test process's own `ProcId`, which is in `ancestors(self)`'s parent chain: register with the test process's parent pid, or add a test-only `AGENTMAIL_ADDRESS` path. Pick one and use it consistently.
  - `unregistered_is_silent`, `daemon_down_is_silent_and_spawns_nothing` (assert no socket file appears), `post_tool_reminder_json_shape`, `post_tool_urgent_once`, `stop_blocks_on_open_question`, `stop_respects_stop_hook_active`, `wait_exits_2_on_mail`, `wait_exits_0_on_signoff`, `wait_respects_no_wake`, `hook_latency_under_20ms` (median of 20 `post-tool` runs against a warm daemon).
- [ ] **Step 3: Run** → FAIL.
- [ ] **Step 4: Implement**, plus `agentmail hook <event> --harness <h> [--session <id>]`, which writes `HookOutput` to stdout and stderr and exits with its code.
- [ ] **Step 5: Run** `cargo test` → PASS.
- [ ] **Step 6: Commit** `feat: agentmail hook with Claude Code events`.

### Task 5: Install / uninstall for Claude Code

**Files:** Create `src/install/mod.rs`, `src/install/claude.rs`, `tests/install.rs`. Modify `src/cli/mod.rs`.

**Interfaces (Produces):**
```rust
pub struct InstallOpts { pub harness: Harness, pub project: bool, pub dry_run: bool, pub set_default: bool, pub bin: PathBuf /* absolute current_exe */ }
pub struct Change { pub path: PathBuf, pub before: Option<String>, pub after: Option<String> }  // after None = delete
pub fn plan_install(o: &InstallOpts, env: &Env) -> anyhow::Result<Vec<Change>>;
pub fn plan_uninstall(o: &InstallOpts, env: &Env) -> anyhow::Result<Vec<Change>>;
pub fn apply(changes: &[Change]) -> anyhow::Result<()>;   // writes <path>.agentmail.bak once per path (only if it didn't exist), then atomic write (temp + rename)
pub struct Env { pub home: PathBuf, pub cwd: PathBuf, pub vars: HashMap<String, String> }
// helpers in mod.rs: json_merge_owned(...) — owned entries are recognised by command containing "agentmail hook" / server key "agentmail"
```
CLI: `agentmail install --harness <h> [--project] [--dry-run] [--set-default]`, `agentmail uninstall --harness <h> [--project]`. `--dry-run` prints a unified-style diff per file and writes nothing.

Claude Code files: the user scope is `$CLAUDE_CONFIG_DIR` or `~/.claude`.
- `settings.json` gets hooks `SessionStart`, `UserPromptSubmit`, `PostToolUse` (matcher `*`) and `Stop` → `"<bin> hook <event> --harness claude-code"` with timeout 10, plus the wait hook, configured per Task 4's spike result.
- The MCP entry `mcpServers.agentmail = {"type":"stdio","command":"<bin>","args":["mcp"]}` goes in `$CLAUDE_CONFIG_DIR/.claude.json`, or `~/.claude.json` when `CLAUDE_CONFIG_DIR` is unset.
- `--project`: `.claude/settings.json` and `.mcp.json` in cwd.
- Preserve key order and all foreign content. Use `serde_json` with the `preserve_order` feature. Re-serialize with 2-space indent and a trailing newline.

- [ ] **Step 1: Failing tests** in `tests/install.rs`, using temp homes:
  - `claude_install_into_empty`.
  - `claude_install_preserves_foreign_hooks_and_keys`: the fixture `settings.json` has another PostToolUse hook and unrelated keys.
  - `claude_install_idempotent`: the second plan has no changes.
  - `claude_uninstall_restores_equivalent`: JSON-equal to the original.
  - `claude_respects_config_dir`.
  - `dry_run_writes_nothing`.
  - `backup_written_once`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: install and uninstall for Claude Code`.

### Task 6: opencode plugin

**Files:** Create `plugin/agentmail.js`, `plugin/agentmail.test.js`, `src/hook/opencode.rs`. Modify `src/hook/mod.rs`. Tests: `tests/hook_opencode.rs`.

**Spike first (Step 1):** with `opencode run --standalone` and private `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME`, plus a probe plugin, confirm:
- (a) `ctx.session.synthetic({sessionID, text, delivery, resume:true})` starts a turn in an idle session, and `steer` lands mid-run;
- (b) `ctx.tool.hook("execute.after")` can append to the result text;
- (c) the event names `session.execution.succeeded` and `session.deleted` (or the current equivalents in `packages/schema/src/session-event.ts`).

Record the results in `docs/harness-notes.md`. **Decision rule:** if `synthetic` isn't available on the plugin ctx, use `ctx.session.prompt` with the same arguments. If neither starts a turn, waking for opencode is reported as unsupported in the notes, and the rest still ships.

**Interfaces:**
- Rust: `agentmail hook status --harness opencode --session <id> [--surface-urgent]` prints the `HookStatus` JSON, or `{}` when unregistered or the daemon is down, exit 0. `agentmail hook wait --harness opencode --session <id> [--timeout 300]` prints `{"mail":true}`, `{"mail":false}` or `{"gone":true}`.
- JS: `export default { id: "agentmail", setup }`. The binary path is a constant the installer substitutes at install time: the placeholder `__AGENTMAIL_BIN__` is replaced with the absolute path.
  - On `execute.after`: run `hook status --surface-urgent` for `event.sessionID`. If there's urgent mail → `ctx.session.synthetic({delivery:"steer", resume:false, text})`. Else if `unread > 0` → append the reminder line to the tool result.
  - On `session.execution.succeeded`: if there are open questions → `synthetic({delivery:"queue", resume:true, text: questions})`.
  - Waking: keep one waiter per registered session with `wake: true` (`Map<sessionID, AbortController>`). The waiter loops `hook wait`. On `{"mail":true}` → `synthetic({delivery:"queue", resume:true, text: reminder})`, then it re-arms after the session is idle again. On `{"gone":true}` → it stops. Start the waiter lazily the first time any event for that session shows it registered.
  - On `session.deleted` → `agentmail signoff` for that session (CLI with `OPENCODE_SESSION_ID` set), and stop its waiter.
  - The cleanup function aborts all waiters.
  - Run shell calls through `Bun.$` with `.nothrow().quiet()`, and treat failures as "no mail". No dependencies.

- [ ] **Step 2: Failing tests:**
  - `plugin/agentmail.test.js` (`bun test`): a mocked ctx records `synthetic` calls and hook registrations, and a fake `agentmail` script on `PATH` emits canned JSON. Tests: `appends_reminder_after_tool`, `urgent_steers_once`, `open_question_queues_after_turn`, `waiter_wakes_on_mail_and_rearms`, `deleted_session_signs_off_and_stops_waiter`, `cleanup_aborts_waiters`, `no_output_when_unregistered`.
  - `tests/hook_opencode.rs`: `status_json_shape`, `status_empty_when_unregistered`, `wait_returns_mail_and_gone`.
- [ ] **Step 3: Run** `cargo test --test hook_opencode` and `bun test plugin/` → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat: opencode plugin`.

### Task 7: Install / uninstall for opencode

**Files:** Create `src/install/opencode.rs`. Modify `tests/install.rs`.

Files written:
- The config dir is `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`. The config file is the first of `opencode.json`, `opencode.jsonc` that exists, else a new `opencode.json`. Merge `mcp.servers.agentmail = {"type":"local","command":["<bin>","mcp"],"codemode":false}`. If the file uses the V1 shape `mcp.<name>`, add ours in that same shape without `servers`, so the user's file stays consistent. If the file contains comments (JSONC) → error with the exact snippet to paste, and change nothing.
- The plugin goes in `plugins/agentmail.js`: `include_str!("../../plugin/agentmail.js")` with `__AGENTMAIL_BIN__` replaced.
- `--project`: `./opencode.json` and `./.opencode/plugins/agentmail.js`.

- [ ] **Step 1: Failing tests:** `opencode_install_v2_shape`, `opencode_install_v1_shape_preserved`, `opencode_jsonc_with_comments_refused_unchanged`, `opencode_plugin_bin_substituted`, `opencode_idempotent`, `opencode_uninstall_removes_plugin_and_entry`, `opencode_respects_xdg`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: install and uninstall for opencode`.

### Task 8: kiro-cli V3 hooks and install

**Files:** Create `src/hook/kiro.rs`, `src/install/kiro.rs`, `tests/fixtures/hooks/kiro/*.json`, `tests/hook_kiro.rs`. Modify `tests/install.rs`.

**Spike first (Step 1):** with `KIRO_HOME` set to a temp copy that keeps the login (see the research notes in `docs/harness-notes.md`), on `kiro-cli chat --v3`, confirm:
- (a) a standalone `$KIRO_HOME/hooks/agentmail.json` (`{"version":"v1","hooks":[...]}`) is loaded for the default agent;
- (b) the V3 Stop hook: exit 1 with stderr continues the turn, and the number of consecutive continuations allowed;
- (c) **whether the human can type and submit while a Stop hook is blocking** (interactive TUI under `script`);
- (d) real stdin payloads and the `KIRO_SESSION_ID` env for SessionStart, UserPromptSubmit, PreToolUse and Stop.

**Decision rules:**
- If (a) fails, write an agent file `agents/agentmail.json`. Convert it to universal format by running `kiro-cli agent upgrade` on a temp directory and copying the result. Make it the default only with `--set-default`.
- If (c) shows input is blocked, the kiro wait hook is not installed unless `install --kiro-wake`. Waking on kiro is documented as opt-in, and `register` defaults `wake` to false when the harness is kiro.

Record everything in `docs/harness-notes.md`.

kiro output, per event (exit code + stdio):
- `session-start`, `prompt`: stdout = guidance_reminder + reminder, exit 0.
- `pre-tool`: `HookStatus{surface_urgent:true}`. If there's urgent mail → stderr = urgent text, exit 2, which blocks this one tool call. Else exit 0, silent.
- `stop`: if there are open questions → stderr = questions, exit 1. Else, if the agent has `wake` and waking is enabled → wait as in Task 4 (no timeout, with the same exit conditions). On mail → stderr = reminder, exit 1. Else exit 0.
- `wait`: not used on kiro. Stop does the waiting.

Install:
- MCP: merge `mcpServers.agentmail = {"command":"<bin>","args":["mcp"]}` into `$KIRO_HOME/settings/mcp.json`, or `~/.kiro/settings/mcp.json` when `KIRO_HOME` is unset.
- Hooks: per spike (a), with `timeout: 0` on stop (no timeout) and `timeout: 10` on the others.
- Print a warning line: `kiro-cli: agentmail supports the V3 engine only — run kiro-cli chat --v3`.

- [ ] **Step 2: Failing tests:**
  - `tests/hook_kiro.rs`: `prompt_stdout_reminder`, `pre_tool_blocks_once_for_urgent`, `stop_exit1_on_open_question`, `stop_waits_then_exit1_on_mail`, `stop_silent_without_wake`, `unregistered_silent`.
  - `tests/install.rs`: `kiro_install_mcp_merge`, `kiro_install_hooks_file`, the fallback variant behind a flag set by the spike outcome, `kiro_respects_kiro_home`, `kiro_idempotent`, `kiro_uninstall`, `kiro_v3_warning_printed`.
- [ ] **Step 3: Run** → FAIL. **Step 4: Implement.** **Step 5: Run** → PASS.
- [ ] **Step 6: Commit** `feat: kiro-cli V3 hooks and install`.

### Task 9: `install --service`

**Files:** Create `src/install/service.rs`. Modify `tests/install.rs` and `src/cli/mod.rs`.

- `agentmail install --service [--dry-run]` writes `$XDG_CONFIG_HOME/systemd/user/agentmail.service` (else `~/.config/...`). The content is the README unit verbatim, with `ExecStart` set to the absolute current binary path.
- It then runs `agentmail daemon stop`, `systemctl --user daemon-reload` and `systemctl --user enable --now agentmail`. When it's run with `AGENTMAIL_SYSTEMCTL` set (tests), that path replaces `systemctl`.
- `uninstall --service` does `disable --now`, removes the unit, and runs daemon-reload.
- If `systemctl --user is-system-running` fails, it errors out with the manual steps.
- [ ] **Step 1: Failing tests:** `service_unit_content_matches_readme` (extract the README's ini block and compare it with the generated unit after substituting `ExecStart`), `service_calls_systemctl_in_order` (with a fake `systemctl` script that logs its args), `service_uninstall`, `service_no_systemd_errors`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat: install the daemon as a systemd user service`.

### Task 10: Real-harness e2e, README support table

**Files:** Create `scripts/e2e.sh`. Modify `README.md` and `docs/harness-notes.md`.

- `scripts/e2e.sh <claude-code|opencode|kiro|mixed>` builds the binary and creates isolated config homes plus a private `AGENTMAIL_DATA_DIR`/`AGENTMAIL_SOCKET`. It installs with `--project` (or into the isolated home), then drives the scenario below headlessly:
  - Claude Code: `"$AGENTMAIL_E2E_CLAUDE" -p --settings … --mcp-config …`.
  - opencode: `opencode run --standalone --format json`.
  - kiro: `kiro-cli chat --v3 --no-interactive --trust-all-tools`.
- Scenario:
  1. Agent A is told "you are backend; register; coordinate with frontend; answer questions".
  2. Agent B is told "you are frontend; register; ask backend what fields POST /orders takes; report the answer".
  3. Assert via `agentmail --as human threads --json` that a question/answer thread exists between the two addresses.
  4. Send urgent mail to A and assert that A's transcript or output mentions it.
  5. kiro only: assert that the V3 MCP `register` recorded harness `kiro` bound to the `kiro-cli` pid (`agentmail --as human agents --json --all`), which verifies the docs/harness-notes.md assumption that `kiro-cli-chat` and the KAS `node` do not start with a `KIRO_SESSION_ID` that differs from kiro-cli's.
  6. For harnesses where waking is supported: let A finish, send it mail, and assert a new turn ran (from the transcript/session log, or the thread getting a reply).
- `mixed` pairs Claude Code (A) with opencode (B).
- The script exits non-zero on any failed assertion and prints which step failed. It requires real auth and model access, so it isn't run by `cargo test`.
- README:
  - Replace "planned" in the support table with the verified results from `docs/harness-notes.md`: reminders, urgent, wake, guidance, install, plus notes such as kiro being V3-only and opencode `codemode`.
  - Document `install`/`uninstall`/`--service` and `scripts/e2e.sh`.
  - Remove the "one address per harness process; opencode multi-session" limitation.
- [ ] **Step 1:** Write the script. Run it for each harness available on this machine and record the outcomes in `docs/harness-notes.md`. A harness without auth is recorded as "not run" with the reason, never as passed.
- [ ] **Step 2:** Update the README. Run `cargo test`, clippy, fmt and `bun test plugin/` → all clean.
- [ ] **Step 3: Commit** `docs: harness support table and e2e driver`, then superpowers:finishing-a-development-branch.
