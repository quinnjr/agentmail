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
  process ancestry. Hooks differ: see "Hook identity" below.

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
- **Limitation: idle wake works only in the terminal UI.** The binary
  knows other entry points (`claude-vscode`, `claude-desktop`,
  `claude-desktop-3p`, `local-agent`, `remote*`, `sdk-ts`, `sdk-py`, the
  GitHub action), and an `asyncRewake` hook is backgrounded when the
  session is interactive or has streaming input
  (`async || asyncRewake && (!nonInteractive || hasStreamingInput)`), so
  the IDE and desktop hosts probably background it. That was not checked
  live (it needs those hosts), and a wrong guess would hold a turn open
  for up to a day, so the wait hook stays silent under every entry point
  but `cli`. Under the IDE and desktop integrations the other hooks work
  (reminders, urgent mail, the Stop block for open questions), but an
  idle session is not woken for new mail. The README's support table
  (Task 10) must say so.
- **Timeouts.** `timeout` is in seconds, any positive number; the schema
  sets no maximum. It applies to `asyncRewake` hooks too: with `timeout: 5`
  a rewake hook sleeping 15 s was killed and never woke the session.
  Defaults when `timeout` is unset, read from the 2.1.311 binary: a
  command hook gets 600 s (`e.timeout ? e.timeout*1000 : 600000`, the same
  constant for synchronous runs and for the `asyncTimeout` of `async` /
  `asyncRewake` hooks). The event runners pass the same 600 s for
  `SessionStart`, `PostToolUse`, `PostToolUseFailure` and `Stop`.
  `UserPromptSubmit` alone runs under a 30 s limit (constant `30000`),
  which matches what was observed: a hook with no `timeout` there was
  killed after about 30 s (both interactive and `-p`), and the prompt went
  ahead. So for the events agentmail uses: 600 s for all but
  `UserPromptSubmit` (30 s), whatever the hook's own `timeout`. agentmail sets
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
  `tool_response`, `tool_use_id`, `duration_ms`),
  `post-tool-failure.json` (`PostToolUseFailure`: `tool_name`,
  `tool_input`, `tool_use_id`, `error`, `is_interrupt`, `duration_ms`),
  `post-tool-subagent.json` (a subagent's `PostToolUse`, with `agent_id`
  and `agent_type`), `stop.json` and `stop-active.json`
  (`stop_hook_active`, `last_assistant_message`). All carry `session_id`,
  `transcript_path`, `cwd` and `hook_event_name`. Claude Code writes its
  base fields (`session_id` ... `agent_id`, `agent_type`, `effort`) before
  `hook_event_name` and the event's own fields.
- **Failed tool calls run `PostToolUseFailure`, not `PostToolUse`.**
  Checked live with `-p` (a `false` Bash call): only `PostToolUseFailure`
  fired for it. Its hook output takes `hookSpecificOutput.hookEventName:
  "PostToolUseFailure"` with `additionalContext` (bundled schema). Without
  it, a run of failing tool calls would get no reminder and no urgent
  push, and would not mark the session busy. agentmail runs the same
  `hook post-tool` for both events and names the event in its output after
  the payload's `hook_event_name`.
- **Subagents.** Checked live with `-p` (the main agent started one
  general-purpose subagent that ran a Bash call): the subagent's tool call
  ran `PostToolUse` in the same session (`session_id` is the parent's,
  under the same `claude` process) with `"agent_id"` and `"agent_type"`
  added; the main agent's own `PostToolUse` for the `Agent` tool call had
  neither. The subagent fired no `UserPromptSubmit` or `Stop`; its end
  runs `SubagentStop` (with `agent_id`), which agentmail does not hook.
  The binary adds `agent_id` to every hook's base input when the hook runs
  in a subagent's context. So by ancestry alone a subagent's hooks act as
  the agent. agentmail's hooks check `agent_id`: when it is non-empty,
  every hook does nothing and prints nothing. Urgent mail is then not
  marked surfaced, so the main agent's next tool call still gets it in
  full, and the subagent is never told "you are <address>". If a payload
  was cut short (over the CLI's 16 MiB read limit) and is not JSON,
  `agent_id` is still read from the base fields before `hook_event_name`.
- **Background subagents** (`run_in_background`), checked live
  interactively (2.1.311, haiku, a logging hook on every event): the main
  agent launched one and ended its turn; its `Stop` fired, then the
  subagent's `PreToolUse`/`PostToolUse` kept firing (with `agent_id`) for
  the next 30 s while the main session sat idle ("Waiting for 1
  background agent to finish"). When it finished, `SubagentStop` fired and
  Claude Code started a main turn by itself for the completion
  notification (`UserPromptSubmit`, then `Stop`). A stub `asyncRewake`
  Stop hook fired while the subagent was still running started a main
  turn at once (`UserPromptSubmit`, the model answered the wake, `Stop`),
  and the subagent ran on undisturbed. So the main session is idle while
  a background subagent works, and subagent hooks must not mark it busy:
  if they did, the subagent's first tool call would end the idle wait and
  mail arriving meanwhile would wait for the subagent to finish. agentmail
  marks the session busy only from main-agent events (`session-start`,
  `prompt`, `post-tool` without `agent_id`). A foreground subagent runs
  inside a turn whose `UserPromptSubmit` already marked it busy, so it
  needs no mark of its own.
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
  "PostToolUseFailure": [{"matcher": "*", "hooks": [{"type": "command", "command": "<bin> hook post-tool --harness claude-code", "timeout": 10}]}],
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
  for the same mail. A new wait keeps trying for the lock for as long as
  its claim on the turn stands (the one it replaces lets go once it sees
  the claim, after at most a status call and a tick), and exits 0 if a
  later claim replaces it or a busy mark comes meanwhile. With a lock file that
  cannot be opened or locked it exits 0 at once: two waits never both run.
- The `session-start`, `prompt` and `post-tool` hooks of the main agent
  (not of a subagent, see above) mark the session busy: they write the
  time to `<address>.busy` (agents with `wake` only), a file waits only
  read. A wait counts a mark newer than its stop (the Stop hook's record
  below, else when the wait hook started) and exits 0 within half a
  second of it (it also checks before claiming the turn and just before
  waking). So a mark is not lost when it comes before the wait has
  claimed the turn: Claude Code submits a queued prompt right after the
  Stop hook returns, and its `prompt` hook can run before the wait hook
  has connected. A wait started by one Stop never outlives the idle
  period: mail that arrives during a later turn is that turn's (the
  `post-tool` reminder tells it), and the turn's own Stop starts a fresh
  wait.
- The Stop hook and the wait hook of one stop run side by side, and they
  decide together: the Stop hook writes its decision (block or not) with
  the stop's key and the time to `<address>.stop` before it prints, and
  the wait with the same key goes by that record (looking for it up to
  6 s) instead of reading the agent's questions for itself, so a question
  opened or answered between two reads cannot make them disagree. The key
  is the payload's `prompt_id` with its `stop_hook_active`
  (`<prompt_id>:plain` or `<prompt_id>:active`): Claude Code 2.1.311's
  schema describes `prompt_id` as correlating a user prompt with all
  later events until the next prompt, so a stop the Stop hook blocks and
  the continuation's stop share it, and the continuation runs no
  UserPromptSubmit to change it. Keyed by `prompt_id` alone, the
  continuation's wait could read the blocked stop's record before its own
  Stop hook rewrote it, date the stop from it, count the continuation's
  own busy marks (the `answer` tool call) as later and exit 0, leaving
  the idle agent unwoken. Stops that share the whole key too (a wake's
  continuation and the next continuation of the same prompt) are told
  apart by time: a record with the wait's key dated more than 0.5 s
  before the wait hook started is an earlier stop's, and the wait keeps
  looking for its own (a continuation makes a model call first, so it
  stops well after its predecessor's record). Only with no record (no
  `prompt_id`, or no Stop hook answer in time) does the wait decide alone
  from the open questions. A record with another key written after the
  wait started means a later stop: the wait exits 0.
- On a plain stop with open questions, the Stop hook's `decision: block`
  continues the turn, so the session is not idle: the wait that stop
  started exits 0 at once and claims nothing (were it to wait, mail coming
  during a text-only continuation, which runs no busy-marking hook, would
  be injected into it). The continuation's own Stop (`stop_hook_active:
  true`) starts the real wait.
- On a plain stop, unread mail already there wakes at once, unless a
  wait already woke it for that mail. This includes mail that
  came mid-turn and was left unread: the agent gets one wake for it, not
  one after every human turn (a wake records the unread ids it was for in
  `<address>.woken`, and later plain stops do not count those). After a
  wake or block (`stop_hook_active: true`) only mail that arrives from
  then on counts, so mail the agent leaves unread cannot wake it in a
  loop. What is new is decided per
  message id, not by the unread count: the agent reading old mail while
  the wait runs does not hide a later message. Mail already read by the
  time the wait looks (just after its delivery) does not count.
- It exits 0 when the agent turns `wake` off (registers again with
  `--no-wake`; the registration publishes a presence event, and every
  status re-read checks it), when the agent signs off, when the daemon stops, when a
  process it runs under (Claude Code) exits, or when the agent is gone
  because its Claude Code process was registered as another agent
  (`agentmail register --pid`): the daemon then publishes the unbound
  agent as gone, and a status re-read that names another agent also ends
  the wait, so it never wakes the session with the other agent's mail; it checks the processes and
  the turn file every 0.5 s and otherwise sleeps on the daemon's event
  stream (no polling of mail).
- A `hook` command line this binary cannot parse (a newer config, an older
  binary) exits 0 and prints nothing (the usage error only when stderr is
  a terminal, for a person running it by hand), so it never blocks a stop
  and never shows up in a harness. A hook run with `--as human` is the
  human, like `AGENTMAIL_HUMAN=1`: it exits 0 with nothing to say, which
  is no output, except for opencode's plugin, which gets the same answer
  as for a session without an agent (`{}` for `status` and `signoff`,
  `{"gone":true}` for `wait`) (`--as` accepts no other value; any other is
  a usage error, silent as above).
  It counts as a `hook` command line when its first positional argument
  (after global options such as `--json` or `--as human`, before any
  `--`) is `hook`.
- Its files under `<data dir>/hook-wait/` (`<address>.turn`,
  `<address>.lock`, `<address>.woken`, `<address>.busy`,
  `<address>.stop`) exist only for agents registered with `wake` (busy
  marks and stop records are written only for those). The daemon removes them, with any temporary
  `<address>.<kind>.<pid>-<nanos>` file a crashed write left, when the
  agent becomes gone: on sign-off, on a registration that unbinds it, and
  the first time a listing or lookup finds its process dead. It remembers
  which gone agents it cleared (until it restarts), so later listings do
  not scan the directory again; an agent seen active again (reclaimed) is
  forgotten, and cleared anew when it next goes. A running wait also
  removes them when it ends on sign-off, on the agent going away, or on
  its harness exiting. So a Claude Code closed mid-turn, with no wait
  running, leaves its files only until the daemon next looks at the
  agent. A wait still holding a
  lock whose file was removed does not block a new wait: a lock counts
  only while the file at the path is the one locked.

### Install (Task 5)

`agentmail install --harness claude-code` writes the hook set above into
`settings.json` and the MCP server into the user's `.claude.json`.
Checked live against Claude Code 2.1.311 in a throwaway
`CLAUDE_CONFIG_DIR` and `HOME`, with `claude mcp list` / `claude mcp get`
(no model run needed):

- **User-scope MCP servers live in `$CLAUDE_CONFIG_DIR/.claude.json`**
  when `CLAUDE_CONFIG_DIR` is set, else in `~/.claude.json`: with both a
  `$CLAUDE_CONFIG_DIR/.claude.json` and a `~/.claude.json`, only the
  first one's server was listed, and without the variable only the
  second's. After `agentmail install` the server showed as `agentmail:
  <bin> mcp - Connected`, scope "User config".
- **Project-scope servers (`.mcp.json`) wait for approval**: `claude mcp
  list` shows them as "Pending approval (run `claude` to approve)" until
  the user approves them in an interactive session. `install --project`
  says so.
- Claude Code rewrites `.claude.json` itself, often, with a two-space
  indent. Install therefore changes only agentmail's entries and keeps
  every other byte of the file (layout, key order, number spellings), and
  checks just before writing that the file still has the text it planned
  from; if Claude Code wrote it in between, nothing is written and the
  command asks to be run again.
- Hooks are recognised as agentmail's by their command: a program whose
  file name starts with `agentmail`, or the installing binary itself
  (same path, or the same file once symlinks are resolved) whatever it is
  called, run with `hook` as its first argument (the binary path is
  shell-quoted when it needs to be). Install refuses to write a hook it
  would not recognise. A renamed binary's entries are found only by that
  same binary (or one named `agentmail…`). Reinstalling from
  another path replaces the old entries, including ones on events this
  version no longer uses, and uninstall removes every one of them, keeping
  other hooks that share a group with them.
- Install updates agentmail's entries in place rather than replacing
  them: on the MCP server it sets only `type`, `command` and `args`
  (keeping, say, the user's `env`), on each of its hooks only `type`,
  `command`, `timeout`, `async` and `asyncRewake`, and a hook stays in its
  group, so a matcher the user changed or a `statusMessage` they added
  survives a reinstall. A missing hook joins the group of its sibling (the
  Stop group holds `stop` and `wait`), else gets a group of its own.
- Uninstall restores an equivalent file. A container (`hooks`, an event's
  list, `mcpServers`) that agentmail's entries emptied is removed only
  when the file had no such key before agentmail (a `"hooks": {}` that was
  there stays). "Before agentmail" is the backup when there is one (with
  agentmail's entries taken out), else the file as it is.
- That record is taken anew by an install into a file holding none of
  agentmail's entries (after an uninstall, say): the backup is replaced by
  the file as it is now and a `.agentmail.created` marker left from an
  earlier install is removed (for a missing file, a stale backup is removed
  and the marker written afresh). Without this, a second uninstall was
  judged against the first-ever original and removed a `"hooks": {}` or
  `"mcpServers": {}` the user added between the two installs.
- Creation is recorded, never inferred: when install creates a file it
  first writes `<file>.agentmail.created`, listing the directories it
  creates for the file (`.claude/` in a project, `~/.claude/` or
  `$CLAUDE_CONFIG_DIR`). An uninstall that empties such a file deletes it
  with its backup and marker, then those directories once empty. Any other
  file is kept, even one that held only agentmail's entries (one added by
  hand with `claude mcp add`, say): it is left as `{}` and its backup
  stays.
- New files are created with mode 0600: Claude Code keeps `.claude.json`
  (account and project data) private, and keeps the mode it finds. An
  existing file keeps its mode, and its backup gets the same.
- A symlinked config file is written through: the file it points to
  changes and the link stays, also when the target does not exist yet (a
  dotfiles link made before the file); a relative link is resolved from
  its own directory.
- A file using CRLF line endings keeps them: new lines use the file's own
  ending, so install then uninstall gives back the same bytes.
- A backup is written to a temporary file and hard-linked into place, so
  a failed write never leaves a partial `<file>.agentmail.bak` (which
  would stop the real original from ever being saved).
- `--set-default` is kiro-only; with another harness it is a usage error
  (exit 2).

### Hook identity

Hooks resolve their agent by `--session <id>` (opencode's plugin) or else
by the hook's process ancestry, cut at the harness process (`CLAUDE_PID`
for Claude Code, else the nearest harness ancestor). Unlike the CLI they
never use `AGENTMAIL_ADDRESS`: a registered agent's shell may export it,
and a `claude` started from that shell would inherit it, so its hooks
would wake for and surface the outer agent's mail, the very case the
`CLAUDE_PID` cut exists for. On opencode the variable on the server
process would override `--session` for every session. `AGENTMAIL_HUMAN=1`
still silences a hook, and so does `--as human`. Tests bind their agent to the test process's parent
(in the hook's ancestry) instead.

### Interfaces for later hook tasks (Tasks 6 and 8)

The code differs from the plan's sketch; build on the code:

- `hook::run(harness, event, stdin, env)`, `hook::run_as(harness,
  event, session: Option<&str>, stdin, env)` and `hook::run_with(harness,
  event, &HookArgs { session, surface_urgent, timeout }, stdin, env)`
  (the whole `hook` command line), where `env: &(dyn Fn(&str) ->
  Option<OsString> + Sync)` (the `Sync` lets a waiting hook run on a
  spawned task). `run_with` dispatches on the harness: Claude Code and
  opencode (Task 6) are implemented; kiro currently returns empty
  output there.
- `HookInput` also carries `agent_id` (Claude Code subagents), `prompt_id` and
  `hook_event_name`.
- `hook::Daemon` (`connect`, `status(surface_urgent)`, `mark_busy(&status)`,
  `record_stop(&status, key, block)`, `wait(wake_for_unread, stop_key) ->
  WaitOutcome { Mail(HookStatus), Gone, Busy, Skip }`; `stop_key` is the
  stop's key (Claude Code: `<prompt_id>:plain|active`), `None` where a
  harness has none, and then the wait
  decides alone). The wait is not a loop over `WaitForMail { timeout_secs: 300
  }`: it subscribes with `Request::Watch { agent }` and wakes on
  `Delivered` events (re-reading `HookStatus` to decide per message id
  whether the mail is new, and ending when the re-read names another
  agent), ends on a `Presence` event showing the agent signed off or
  gone, `wake` turned off, a lost connection, a dead harness process
  (checked every 0.5 s) or a busy mark, and keeps one waiter per agent with the turn file and lock
  above. It has no timeout of its own; the harness's hook `timeout` bounds
  it. kiro's Stop (Task 8) maps `Mail` to its exit code and the rest to 0.

### Interfaces for later install tasks (Tasks 7 and 8)

The plan's `json_merge_owned(...)` does not exist; `src/install/mod.rs`
has these instead (Claude Code's `claude.rs` shows the pattern):

- `JsonFile::read(path, entry)`: the file as a JSON object (missing or
  blank: empty), refusing other text with a message that shows `entry`
  (what to add by hand). It also records the file as it was before
  agentmail (its backup, else its current value).
- Edit a copy of `.value`, touching only agentmail's entries;
  `object_at(root, key, path)` gets or creates a nested object and
  refuses a non-object.
- `JsonFile::change(new, uninstall, strip)` turns it into a `Change`
  (`None` when nothing changes). `strip` removes agentmail's entries from
  an object and nothing else; with it, containers agentmail emptied are
  removed unless they were there before agentmail, and an uninstall
  deletes the file only when agentmail created it, which `apply` records
  in `<file>.agentmail.created` (`created_marker_path`).
- Ownership: `is_agentmail_hook(cmd)` (program named `agentmail…` run
  with `hook`) and `owns_hook(cmd, bin)` (that, or the program is `bin`
  itself); MCP servers are owned by their key `agentmail`.
- `json::render(orig, new)` writes `new` over the original text, keeping
  every untouched byte (layout, key order, number spellings);
  `json::pretty` for snippets. `bin_str(o)` is the absolute UTF-8 binary
  path, `shell_quote` quotes it for a command line. `plan_install` /
  `plan_uninstall` dispatch per harness, `apply` writes (with backups) and
  `diff` renders `--dry-run`.

## opencode plugin (Task 6)

Checked live against opencode 2.0.27 (`~/.opencode/bin/opencode`) with
private `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
`XDG_CACHE_HOME` and `HOME`, a probe plugin in
`$XDG_CONFIG_HOME/opencode/plugins/`, and opencode's free
`opencode/deepseek-v4.1-flash-free` model (no login needed). Single runs
used `opencode run --standalone`; anything that happens after a turn ends
needs a server that stays up, so those used `opencode serve` (with
`OPENCODE_PASSWORD`) and `opencode run --server`. `run --standalone` shuts
its server down as soon as the turn ends, so nothing that waits for an idle
session can be seen with it. The plugin API was read from the shipped
bundle (the `fromPromise` adapter that wraps a `{ id, setup }` module).

- **Plugin context.** `setup(ctx)` gets `app`, `location`, `options`,
  `agent`, `aisdk`, `command`, `event`, `experimental`, `generate`,
  `model`, `provider`, `integration`, `mcp`, `permission`, `plugin`,
  `reference`, `rpc`, `skill`, `storage`, `tool`, `vcs`, `websearch`,
  `worktree`, `session` and `shell`. `ctx.session` has `hook`, `create`,
  `get`, `remove`, `switchAgent`, `switchModel`, `prompt`, `generate`,
  `command`, `compact`, `synthetic`, `interrupt`, `update`, `move`, `wait`
  and `context`. `ctx.tool.hook(name, fn)` resolves to `{ dispose }`. The
  function `setup` returns is called when the plugin is unloaded (server
  shutdown, or the plugin file changing: opencode watches it).
- **(a) `ctx.session.synthetic({sessionID, text, delivery, resume})`
  works; the `prompt` fallback is not needed.** `delivery` is `"steer"` or
  `"queue"`. With `delivery: "queue", resume: true` on an idle session it
  starts a turn at once (`session.inbox.enqueued` with a `synthetic` item,
  then `session.execution.started` 16 ms later); the model sees the text
  as a message of its own. With `delivery: "steer", resume: false` during a
  turn, the text is delivered after the current step's tool calls
  (`session.inbox.delivered` right after `session.step.ended`) and the
  model acted on it in that same turn. Both were also seen end to end with
  the real plugin below.
- **(b) `ctx.tool.hook("execute.after", fn)` can add to the result.** `fn`
  gets `{tool, sessionID, agent, messageID, id, input, status, result}`;
  for `status: "completed"`, `result` is `{output?, content, metadata?}`,
  and after the hook opencode builds the tool's result from
  `result.content` (an array of `{type: "text", text}` parts), so pushing a
  text part there reaches the model (it repeated the appended line
  verbatim). `output` is tool-specific (an object for `shell`), so
  agentmail appends to `content` only. A failed call has `status:
  "error"` and `error`, no `result`: nothing can be appended, and the
  plugin only steers urgent mail for it. The hook is awaited before the
  result goes back to the model, so its cost (one `agentmail hook status`,
  a few ms) is added to every tool call. With opencode's "code mode" the
  model's tools run inside an `execute` tool: `execute.after` fires for
  each tool called from the code and for `execute` itself.
- **(c) Event names.** `ctx.event.subscribe({signal})` is an async iterable
  of `{id, type, data, ...}`. A turn is `session.execution.started` and then
  one of `session.execution.succeeded`, `session.execution.failed` (with
  `error`) or `session.execution.interrupted` (with `reason`: `user`,
  `shutdown`, `superseded`, `inactivity`), all with `data.sessionID`.
  Deleting a session (`opencode session delete`, or `ctx.session.remove`)
  publishes `session.deleted` with `data.sessionID`. A prompt is
  `session.inbox.enqueued` with `data.item.type` `user` (`synthetic` for
  the plugin's own).
- **One plugin instance per directory, one event stream for all.**
  opencode loads a separate instance of each plugin for every directory
  (location) it serves, in the same process. `ctx.tool.hook` sees only its
  own directory's tool calls, but `ctx.event.subscribe` delivers every
  session's events to every instance (seen with two `run --server` in two
  directories: both instances got both sessions' `execution.*` events). The
  plugin therefore acts on a session only when `ctx.session.get` places it
  in its own `ctx.location.directory`; otherwise two instances would each
  queue the same question or run two waiters. Answers are cached, both in
  lists bounded so a long-running `opencode serve` that never deletes
  sessions does not grow without bound: another instance's sessions, the
  256 most recently seen (state for those is dropped); its own sessions,
  with their state, until deleted or until more than 256 are known, when
  the least recently seen that need nothing more (idle, no waiter or
  turn-end handler running, nothing left to deliver, no turn of the
  plugin's pending) are forgotten and looked up again when next seen. A failed lookup is not cached; the
  session's next event asks again. A deleted session cannot be looked up
  any more: an instance that never saw it signs it off too, which is
  harmless twice.
- **No `session.hook("context")`.** The spec's table names it for the
  reminder at start and on a prompt; 2.0.27 has no such hook (the session
  hooks are `prompt`, `title`, `retry`, `model.request`, `http.request`,
  `http.response` and three `experimental.ws.*`). `prompt` could append to
  the human's prompt text, but that text is stored and shown as the human's
  own message, so agentmail does not touch it. On opencode the agent learns
  who it is from the MCP server's instructions and from `register`, and is
  reminded of mail after its first tool call.

How the plugin (`plugin/agentmail.js`) behaves, built on that:

- It runs `agentmail hook status --harness opencode --session <id>
  [--surface-urgent]`, which prints the agent's `HookStatus` plus a
  `text` object (`guidance`, `reminder`, `urgent`, `questions`; `null`
  when there is nothing), or `{}` for a session without an agent or with
  the daemon down (and never starts the daemon). Rendering stays in Rust,
  shared with the other harnesses.
- After each tool call (`--surface-urgent`): urgent mail goes into the
  running turn with `synthetic` `steer` (once: agentmail marks it surfaced),
  else, with unread mail, the reminder is appended to the tool result. When
  the steer fails (the session ended meanwhile), the urgent text is
  appended to the tool result instead, with the reminder, since agentmail
  will not report that mail as urgent again. A failed tool call (status
  `error`) has no result to append to: its urgent text is kept by the
  plugin and pushed after the next tool call, or, if the turn ends first,
  queued at its end (`resume: true`, with any open questions).
- At the end of a successful turn, open questions are queued as the next
  turn (`queue`, `resume: true`), unless that turn was itself one the
  plugin started (a wake or the questions): like Claude Code's
  `stop_hook_active`, it is not held in a loop by questions the model does
  not answer. A human prompt (`inbox.enqueued` with a `user` item) clears
  that mark. When the questions cannot be queued (`synthetic` fails), the
  waiter starts as after a continuation (`stop_hook_active: true`), so the
  open questions do not end the wait at once (`{"busy":true}`).
- Turn starts and ends are recorded on the session's state as the event
  arrives, before any lookup, with a counter; every step after an await
  checks the session is still in the same idle period. Events are handled
  side by side, so without this a turn that started during the end's
  lookup could be overwritten as idle, and a waiter would run in a busy
  session.
- While a session is idle (after `execution.succeeded`, `failed` or
  `interrupted`) and its agent has `wake` on, one waiter per session runs
  `agentmail hook wait --harness opencode --session <id> --timeout 300`
  with `{"stop_hook_active": <the turn was the plugin's, or did not
  succeed>}` on stdin, the same `Daemon::wait` Claude Code's wait hook
  uses (one waiter per agent, wake once per message, new mail only after
  a wake). It prints `{"mail":true,"text":...}`, `{"mail":false,
  "known":[<ids>]}` on the timeout (the plugin waits again, at most once a
  second, passing `known` back on the next wait's stdin, so mail delivered
  between the two waits is news rather than counted as already there by
  the next wait's first look), `{"busy":true}`
  (open questions continue the turn, or another wait took over),
  `{"skip":true}` (`wake` off, or the lock unusable) or `{"gone":true}`
  (signed off, gone, no agent, daemon down, opencode exited). On mail the
  plugin queues the reminder with `resume: true`, which starts a turn.
  When that fails (or the answer has no text) the waiter does not stop:
  agentmail has recorded the mail as woken, so a later plain wait would
  not wake for it. It waits on for new mail only (`stop_hook_active:
  true`), with a short timeout (5 s, doubling per failure up to 300 s),
  and after each timeout queues the reminder of a fresh status again
  while the mail is still unread; new mail meanwhile is another try. A
  re-wait after a timeout never answers `{"busy":true}` for open
  questions (`Daemon::wait_from` with `known`: the stop was already found
  not blocked), so a question delivered between two waits is mail like
  any other. A first wait can still answer it, for a question that came
  after the turn end's status: if the session is still idle, the plugin
  re-runs the turn's end once (status, then the questions queued; if they
  cannot be, or there are none, it waits again, for new mail only when
  they could not be queued), and a second busy answer ends the waiter. On
  `{"skip":true}`, `{"gone":true}` or a failure the waiter stops until
  the session is idle again. `execution.started` kills the waiter's process, so a wake never
  lands in a busy turn (`Bun.spawn`, since Bun's `$` cannot be killed).
  A mail answer the wait printed just before the kill (or while the turn
  was starting) has already been recorded by agentmail as woken, so the
  plugin does not drop it, even when the process then ended by the signal
  (its stdout is parsed whatever the exit status, and a mail answer is
  kept): it marks the session, and if the busy turn ends
  without a tool call having appended the reminder, the reminder is queued
  then (`resume: true`), once.
  There are no busy-mark files for opencode: the plugin sees the session's
  state directly.
- The waiter starts lazily, at the first end of turn whose status shows the
  agent registered with `wake`. After opencode restarts, a session that
  was idle when it went down is not waited on until its next turn ends.
- `session.deleted` stops the waiter and runs `agentmail hook signoff
  --harness opencode --session <id>`. Like every hook it never starts the
  daemon (plain `agentmail signoff` would, through the CLI's
  connect-or-spawn), and it acts only as the agent registered for that
  session (`Caller::Session`), never by process ancestry, so it cannot
  sign off an outer agent (the Claude Code session whose shell started
  opencode) when the session has none. It prints `{"signed_off":true,
  "address":...}`, or `{}` for a session without an agent or with the
  daemon down.
- `AGENTMAIL_ADDRESS` is removed from the environment of every agentmail
  command the plugin runs: opencode may have inherited it from the shell
  that started it. The hooks never read it, but it is removed anyway so no
  agentmail command the plugin runs can act as that address.
- The plugin's cleanup kills every waiter and stops its event stream.
- The installed file has the binary's absolute path in place of the
  `__AGENTMAIL_BIN__` placeholder; uninstalled (the placeholder left as is,
  as in the unit tests) it runs `agentmail` from `PATH`.

End to end with the real plugin and agentmail (debug build, private data
dir, socket under `/run/user/1000` because the scratch path exceeded the
socket path limit), `opencode serve` plus `opencode run --server`:

- The model registered over MCP; when the turn ended the plugin started a
  `hook wait` for the session. `agentmail --as human send` woke the idle
  session within the same second (the reminder was enqueued, and the turn
  started), and the model read the mail and
  replied with the `reply` tool.
- Urgent mail sent during a turn's `sleep 6` tool call was steered into
  that turn; the model followed it in its final answer.
- `opencode session delete` signed the session's agent off (`signed_off:
  true`) and left no `hook wait` process behind.
- Killing the server left no `hook wait` process behind either.

