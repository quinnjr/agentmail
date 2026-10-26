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
    Claude Code shell binds its own parent instead (see opencode below for
    the other exception). What sits between `kiro-cli-chat` and `node` was
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
  children.
- **Shared opencode MCP server.** A tool call without `_meta` (opencode
  omits it when the call has no session) cannot `register` on a server
  bound to opencode, so the shared server never caches an address.
  Likewise `agentmail register` under an opencode process without
  `OPENCODE_SESSION_ID` is refused (only `--pid` naming another process gets
  past it), so no agent becomes the identity of the shared opencode process.
- **opencode MCP server behind a wrapper.** When opencode starts the
  server through a wrapper (opencode -> `sh -c` -> agentmail mcp), the
  server's parent is the wrapper, but a session `register` over MCP binds
  the opencode process above it, the same process `agentmail register`
  from that session's shell binds. Otherwise the session's bound process
  would flip between the wrapper and opencode depending on which path
  registered last, and presence would follow the wrapper's lifetime. This
  cannot leak an identity: sessionless registration on the shared server
  is refused. Claude Code gets no such rule, so an unknown MCP client run
  from a Claude Code shell does not take over Claude's binding.
- **Resolution order** in the CLI: `--as human`, `AGENTMAIL_ADDRESS`,
  `AGENTMAIL_HUMAN=1`, `OPENCODE_SESSION_ID` (checked as above), then
  process ancestry.
