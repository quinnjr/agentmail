# Harness notes

Observed harness behaviour that agentmail depends on, with how it was checked.

## Session ids (Task 3)

Checked by reading the shipped bundles, not a live model run (a live run needs
model credentials that an isolated config does not have).

- **opencode 2.0.27.** The MCP client sends the session on every tool call:
  `client.callTool({name, arguments, _meta: {"ai.opencode/sessionID": sessionID}})`,
  omitting `_meta` when the call has no session. Shell commands get
  `OPENCODE_SESSION_ID=<sessionID>` in their environment. agentmail's MCP
  server resolves each call by that `_meta` key, and the CLI by the variable.
- **kiro-cli 2.32.0, V3 engine (KAS).** The engine is a Node bundle,
  `~/.local/share/kiro-cli/kas/<version>/node_modules/@kiro/agent/dist/server/acp-server.js`.
  - Where `KIRO_SESSION_ID` is set: the bundle defines it once
    (`dta="KIRO_SESSION_ID"`, `Are(id) => {[dta]: id}`) and adds it to the
    environment of shell tool commands (the terminal's `sessionEnv`, passed
    to every command the shell tool runs) and of hook and confirmation
    commands (`env: Are(rootConversationId)`).
  - Where it is not: MCP servers. Their `StdioClientTransport` gets the
    default environment plus the server's configured `env`, nothing more,
    so agentmail's MCP server under kiro registers by process alone, with
    no session.
  - Its value is the **root** conversation id (`rootConversationId`), which
    every subagent conversation of that root shares. So a subagent's shell
    and hook commands carry the root's session, and run under the same
    `kiro-cli` process: identity cannot tell a kiro subagent from its root
    agent; both act as the agent the root registered.
  - Consequence: the CLI does not resolve kiro callers by session. With
    `KIRO_SESSION_ID` it still calls by process ancestry (`Caller::Procs`),
    which finds the agent the MCP server bound to `kiro-cli`. `register`
    from a kiro shell still records the session on that same agent.
  - `kiro-cli chat` runs the separate `kiro-cli-chat` binary as a child,
    and `kiro-cli` stays its parent (it does not exec). Checked with a stub
    `kiro-cli-chat` first on `PATH` in an isolated `HOME`/`KIRO_HOME`:
    `kiro-cli chat --help` ran the stub with its PPID set to the live
    `kiro-cli` process. So the chain is `kiro-cli` -> `kiro-cli-chat` ->
    ... -> the engine's `node` -> MCP server.
  - MCP servers are started by the engine's `node` process, not by
    `kiro-cli` itself, so when the MCP server's parent is not a harness and
    a `kiro-cli` sits above it, `register` binds that `kiro-cli`. The
    `kiro-cli-chat` process in between is not a harness name (it is
    detected as other), so the binding skips it too and reaches `kiro-cli`
    (test `bind_skips_kiro_cli_chat`). An unknown MCP client run from a
    Claude Code shell binds its own parent instead, and so does one run
    from a kiro or opencode shell (see "MCP clients run from a harness
    shell" below). What sits between `kiro-cli-chat` and `node` was
    not observed live: in an isolated `HOME`/`KIRO_HOME` the real
    `kiro-cli chat` stops at the browser login prompt before starting the
    engine (no credentials in isolation). The binding does not depend on
    that depth.
- **Inherited session variables.** A harness started from another
  harness's shell inherits the outer variable (`claude` run from an opencode
  bash tool sees `OPENCODE_SESSION_ID`). The CLI only uses the variable of
  the harness it actually runs under: the nearest harness ancestor's
  (`opencode` → `OPENCODE_SESSION_ID`, kiro → `KIRO_SESSION_ID`, Claude Code
  → none). With no harness ancestor the variables are ignored too: a
  process detached from opencode (a tmux server, `setsid` or `nohup` job
  started from its bash tool, reparented to init) keeps
  `OPENCODE_SESSION_ID` but is no longer part of that session, and a real
  opencode session always has an `opencode` process (`opencode serve
  --stdio` under `run --standalone`, 2.0.27) above its shell and MCP
  children. A value the harness process itself was started with does not
  count either: opencode B started from opencode A's bash tool carries A's
  `OPENCODE_SESSION_ID` and passes it on to everything it starts (its
  plugin's `Bun.spawn`, an LSP), not only its own shell commands, which
  carry B's session id instead. The MCP server applies the same rule when
  deciding whether the harness started it.
- **Shared opencode MCP server.** A tool call without an opencode
  session in `_meta` (opencode omits it when the call has no session)
  cannot `register` on a server started by opencode, so the shared server
  never caches an address. Likewise `agentmail register` bound to an
  opencode process without an opencode session is refused: no
  `OPENCODE_SESSION_ID`, or only another harness's session (a kiro shell
  running `register --pid <opencode pid>`). Only `--pid` naming a process
  outside any opencode gets past it, so no agent becomes the identity of
  the shared opencode process. This holds for opencode at PID 1 (a
  container's entry point) too: it cannot be bound, but its harness is
  still read, so the server counts as shared and the refusal applies. A
  server that is not opencode's own (another MCP client run from its bash
  tool, below) is not bound to opencode and not shared. The refusal names
  the cause: the server was started by opencode, which shares it between
  its sessions.
- **Harness at PID 1: session-only agents.** A harness ancestor at PID 1
  (opencode as a container's entry point) cannot be bound to, and the CLI
  then binds nothing rather than falling back to the grandparent, so
  `agentmail register` (however deep its chain: opencode -> bash -> make ->
  sh -> agentmail) and the MCP server (whose parent is the harness) agree:
  the session's agent carries no process. Falling back would bind a
  transient process in between, which would mark the agent Gone when it
  exits and be replaced by a different one on the next register. Presence
  is unchanged: an agent without a bound process is never Active, so mail
  addressed to it by name is held until a bound registration claims it;
  callers in that session act as it through its session (MCP `_meta`,
  `OPENCODE_SESSION_ID`) or `AGENTMAIL_ADDRESS`. Pinned by
  `identity_cli_session_register_under_pid_1_opencode_is_unbound`.
- **opencode MCP server behind a wrapper.** When opencode starts the
  server through a wrapper (opencode -> `sh -c` -> agentmail mcp), the
  server's parent is the wrapper, but a session `register` over MCP binds
  the opencode process above it, the same process `agentmail register`
  from that session's shell binds. Otherwise the session's bound process
  would flip between the wrapper and opencode depending on which path
  registered last, and presence would follow the wrapper's lifetime. This
  cannot leak an identity: sessionless registration on the shared server
  is refused.
- **Claude Code: no harness redirect (a deliberate trade-off).** The plan
  says an explicit pid (the MCP server's parent) should prefer the nearest
  harness ancestor. agentmail applies that only for kiro and opencode.
  Under Claude Code the parent is bound exactly. This was not driven by a
  spike: it guards a case that follows from the process model, an unknown
  MCP client (an inspector, another agent tool) run from Claude Code's bash
  tool, whose `agentmail mcp` would otherwise bind Claude Code itself and
  take over the agent Claude Code registered. The cost is the wrapper case:
  a Claude Code config that starts the server through a wrapper that stays
  its parent (`sh -c 'agentmail mcp'`, no `exec`) binds the wrapper, so
  Claude Code's shell commands and hooks, which resolve by ancestry (bash ->
  claude), do not find that agent. Claude Code runs a configured stdio
  command directly, so a plain `agentmail mcp` config (or a wrapper that
  `exec`s it) is unaffected. Pinned by the test
  `claude_code_wrapper_binds_the_wrapper`.
- **MCP clients run from a harness shell (kiro, opencode).** Another MCP
  client with agentmail configured (codex, gemini-cli, an MCP inspector)
  can be started from kiro's shell tool or opencode's bash tool. Its
  `agentmail mcp` has a non-harness parent with kiro-cli or opencode above
  it, exactly like the server kiro's engine or an opencode wrapper starts.
  Redirected to the harness, it would do harm: under kiro it binds
  kiro-cli and, on `register`, displaces the agent kiro's own server bound
  there (that agent loses its process, and the newcomer, sessionless and of
  the same harness, inherits its session), so kiro's shell commands and
  hooks then resolve to the foreign agent; under opencode it counts as
  opencode's shared server and every sessionless `register` is refused
  with an opencode session-id error, though the client is not opencode.
  Decision: the MCP server redirects to the harness only when the harness
  started it. kiro and opencode set their session variable
  (`KIRO_SESSION_ID`, `OPENCODE_SESSION_ID`) for shell commands but not
  for MCP servers (see above), so when any process from the server up to
  (not including) the harness was started with that harness's variable,
  the server came from the harness's shell: it binds its parent exactly,
  as under Claude Code, and is not treated as shared. The environment
  checked is each process's start environment (`/proc/<pid>/environ`), so
  a client that hands its server a cleaned environment (the MCP SDKs pass
  only a few default variables) is still caught through the client process
  itself. A value the harness process itself carries is ignored: it was
  inherited from an outer session (kiro run from a kiro shell) and passed
  on to everything the harness starts. Known limit: a client that clears
  its own start environment is not detected and binds the harness. The
  CLI's `register --pid $$` in such a shell keeps the redirect (it is the
  harness's own session registering). Pinned by
  `client_from_kiro_shell_binds_itself`,
  `client_from_opencode_shell_binds_itself` and
  `identity_mcp_bind_skips_harness_redirect_from_its_shell`. Like the
  Claude Code decision, this follows from the process model and the
  bundle reading above, not a live model run.
  - **Assumption (kiro, unverified live):** the check covers every process
    between the server and `kiro-cli`, so under kiro it includes
    `kiro-cli-chat` and the engine's `node`. The rule holds only if
    neither of them is started with a `KIRO_SESSION_ID` that differs from
    the one `kiro-cli` itself carries (normally none). If one were, kiro's
    own MCP server would be taken for a foreign client: it would bind its
    parent (`node`, harness `other`) and kiro identity would break
    entirely. What was checked: the KAS bundle adds `KIRO_SESSION_ID` only
    to the environment of the shell commands it runs (see above), not to
    the MCP transport environment. The `kiro-cli-chat` binary contains the
    literal four times: once next to `SHELL` and `.kiro/steering` in the
    V2 agent code (the shell tool's environment), once in the list of
    environment variables the CLI reads, and twice as search keywords in
    its bundled help pages (hooks, session management). None of these
    shows it being set for the `kiro-cli-chat` or `node` children, but that
    is a static reading. The process environments themselves were not
    observed, because in an isolated `HOME`/`KIRO_HOME` the real
    `kiro-cli chat` stops at the browser login before it starts either
    process. The Task 10 kiro e2e (`scripts/e2e.sh kiro`) closes this: it
    asserts that the V3 MCP `register` records harness `kiro`, bound to the
    `kiro-cli` pid.
- **`register --pid` under opencode.** A `--pid` naming a non-harness
  ancestor (the shell, `$$`) is redirected to the opencode process above it,
  and so refused without a session like any other sessionless opencode
  registration. Only a process outside that opencode gets past it.
- **Resolution order** in the CLI: `--as human`, `AGENTMAIL_ADDRESS`,
  `AGENTMAIL_HUMAN=1`, `OPENCODE_SESSION_ID` (checked as above), then
  process ancestry.

## Claude Code hooks (Task 4)

Checked live against Claude Code 2.1.311 (the newest binary under
`~/.local/share/claude/versions/`), interactive in a detached tmux pane and
with `-p`, in a throwaway `CLAUDE_CONFIG_DIR` and `HOME` (only the login
copied in, removed afterwards), with `--model haiku`. The schema details
come from the binary's bundled settings schema.

- **Waking an idle session: `asyncRewake` works, so the fallback (a
  synchronous Stop hook holding the turn open for up to 540 s) is not
  used.** The key is a boolean on a command hook: `{"type": "command",
  "command": "...", "asyncRewake": true, "timeout": N}` ("runs in
  background and wakes the model on exit code 2 (blocking error). Implies
  async."). Observed: on a Stop hook, after the turn ended and the prompt
  sat idle, the hook exited 2 25 s later and Claude Code started a new turn
  by itself. The model sees a user turn carrying
  `<task-notification><summary>Stop hook feedback</summary>` and a
  system reminder `Stop hook blocking error from command "Stop": <stderr>`;
  the terminal shows "Stop hook feedback". The Stop hook of that woken turn
  gets `"stop_hook_active": true`. The `rewakeMessage`/`rewakeSummary` keys
  exist but are marked `@internal`; agentmail does not use them.
- **`claude -p` runs `asyncRewake` hooks synchronously** (the binary gates
  backgrounding on an interactive session or streaming input). Observed: a
  `-p` run waited the hook's 12 s out, then took a second turn for its exit
  2. So `agentmail hook wait` does nothing unless `CLAUDE_CODE_ENTRYPOINT`
  is unset or `cli` (the interactive UI; `-p` sets `sdk-cli`): waiting
  would hold every headless run open until mail came.
- **Timeouts.** `timeout` is in seconds, any positive number; the schema
  sets no maximum. It applies to `asyncRewake` hooks too: with `timeout: 5`
  a rewake hook sleeping 15 s was killed and never woke the session. A hook
  with no `timeout` on `UserPromptSubmit` was killed after about 30 s
  (both interactive and `-p`), and the prompt went ahead. agentmail sets
  `timeout` on every hook: 10 for the quick ones, 86400 (a day) for the
  wait hook, observed to be accepted and to stay alive past the shorter
  waits. Node timers overflow above 2^31-1 ms (about 24.8 days), so
  larger values must not be used. When the wait hook times out, the next
  Stop starts a new one.
- **Hook processes are descendants of the `claude` process**: hook command
  -> `sh` -> `claude`, for SessionStart, UserPromptSubmit, PostToolUse and
  Stop alike (spawned detached, in their own session, but the parent stays
  `claude`). So `Caller::Procs` (ancestry) finds the agent bound to the
  Claude Code process. Hooks also get `CLAUDE_PID` (that process),
  `CLAUDE_CODE_SESSION_ID` (= the payload's `session_id`),
  `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_PROJECT_DIR`. A `claude` started
  through the versioned binary path directly has command name `2.1.311`,
  not `claude`; through the usual `~/.local/bin/claude` symlink it is
  `claude`. The hooks cut their ancestry at `CLAUDE_PID` (else at the
  nearest process with a harness name), so a Claude Code started from a
  registered agent's shell never acts as that outer agent. The e2e script
  should run Claude Code through a symlink named `claude`, so that the
  CLI's own harness detection (by command name) sees it.
- **Payloads** (stdin) are recorded in `tests/fixtures/hooks/claude-code/`
  (paths anonymised): `session-start.json` (`source`, `model`),
  `prompt.json` (`prompt`), `post-tool.json` (`tool_name`, `tool_input`,
  `tool_response`, `tool_use_id`, `duration_ms`), `stop.json` and
  `stop-active.json` (`stop_hook_active`, `last_assistant_message`). All
  carry `session_id`, `transcript_path`, `cwd` and `hook_event_name`.
- **End to end with agentmail itself** (debug build, private data dir):
  hooks installed as below, the agent registered with `register --pid
  <claude pid>`, the session left idle; `agentmail --as human send` woke it
  within seconds with the reminder. The woken model did not read its mail
  and stopped again; the next wait hook (`stop_hook_active: true`) kept
  waiting instead of waking it again for the same mail. After `/exit` no
  wait hook was left running.

- **A rewake during a busy turn is injected into that turn.** Checked
  with a stub `asyncRewake` Stop hook that exits 2 on a trigger file: the
  hook started by one Stop kept running through the next human prompt, and
  the trigger was fired while that turn ran a 20 s `sleep` tool call. No
  extra turn started and nothing was dropped: when the tool call finished,
  Claude Code ran `UserPromptSubmit` (its `prompt` is the
  `<task-notification>` text) and sent the next model request with the
  stderr in a block headed "[SYSTEM NOTIFICATION - NOT USER INPUT] ... an
  automated background-task event", in the same turn; the terminal showed
  "Stop hook feedback" above the turn's answer, which the model was free
  to ignore (haiku did). That turn's Stop got `stop_hook_active: false`,
  so the wake did not count as a hook continuation. Fired between turns
  (idle), the same hook started a new turn, as above, and that turn also
  ran `UserPromptSubmit` with the notification as its prompt. Harmful for
  agentmail: the busy turn's one wake is spent as a confusing "Stop hook
  blocking error" mid-turn, and a long-lived wait keeps the lock so later
  Stops' waits skip. Hence the busy marking below. Rerun end to end with
  the agentmail hooks (debug build, isolated daemon): the wait exited 0
  about 0.1 s after the next prompt; mail sent during that turn's tool
  call caused no injection; the turn's Stop started a plain wait that woke
  the session at once for the unread mail; the woken turn's wait
  (`stop_hook_active: true`) kept waiting, and woke it again only when a
  new message came after the first was read outside the session.

Hook configuration (`settings.json`), as Task 5 installs it:

```json
{"hooks": {
  "SessionStart":     [{"hooks": [{"type": "command", "command": "<bin> hook session-start --harness claude-code", "timeout": 10}]}],
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "<bin> hook prompt --harness claude-code", "timeout": 10}]}],
  "PostToolUse":      [{"matcher": "*", "hooks": [{"type": "command", "command": "<bin> hook post-tool --harness claude-code", "timeout": 10}]}],
  "Stop":             [{"hooks": [
    {"type": "command", "command": "<bin> hook stop --harness claude-code", "timeout": 10},
    {"type": "command", "command": "<bin> hook wait --harness claude-code", "asyncRewake": true, "timeout": 86400}
  ]}]
}}
```

How `agentmail hook wait` behaves under it:

- It waits only for an agent registered with `wake`, and only for an idle
  session. Every Stop starts one, and the newest replaces the one before
  (a turn file under `<data dir>/hook-wait/`, plus a lock so only one waits
  per agent): the earlier one exits 0, so the agent is never woken twice
  for the same mail.
- The `session-start`, `prompt` and `post-tool` hooks mark the session
  busy, and a wait exits 0 within half a second of that (it also checks
  just before waking). So a wait started by one Stop never outlives the
  idle period: mail that arrives during a later turn is that turn's
  (the `post-tool` reminder tells it), and the turn's own Stop starts a
  fresh wait.
- On a plain stop, unread mail already there wakes at once, unless the
  agent also has open questions (the Stop hook's `decision: block` handles
  those). This includes mail that came mid-turn and was left unread: the
  agent gets one wake for it. After a wake or block (`stop_hook_active:
  true`) only mail that arrives from then on counts, so mail the agent
  leaves unread cannot wake it in a loop. What is new is decided per
  message id, not by the unread count: the agent reading old mail while
  the wait runs does not hide a later message. Mail already read by the
  time the wait looks (just after its delivery) does not count.
- It exits 0 when the agent signs off, when the daemon stops, or when a
  process it runs under (Claude Code) exits; it checks the processes and
  the turn file every 0.5 s and otherwise sleeps on the daemon's event
  stream (no polling of mail).
- A `hook` command line this binary cannot parse (a newer config, an older
  binary) prints the usage error and exits 0, so it never blocks a stop.
