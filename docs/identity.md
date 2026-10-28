# Identity

[← README](../README.md)

## Addresses

An agent picks a name matching `^[a-z0-9][a-z0-9-]{0,31}$` (`backend`,
`orders-api`, `web`). The daemon appends `-` and 8 random characters, giving an
address like `backend-k3v9x2qa`. Names need not be unique; addresses are.

`to` accepts full addresses or plain names. A plain name first resolves to the
sender's contact with that name, whatever its repo, and otherwise to the single
active agent with that name in the sender's repo (for `human`, which has no
repo, names resolve across the whole machine). With none, the mail is held
and delivered to the first agent that registers under that name in that repo.
With several, the send is refused and the candidates are listed. A full address
that doesn't exist is refused (`UnknownRecipient`), and so is sending to
yourself (`InvalidRecipient`). Mail is never silently dropped. There is no
broadcast to everyone.

## Binding a session to its address

The MCP server, shell commands and hooks of one agent are separate
processes, so they find their address through the harness process they share
(or, under opencode, the session):

- `agentmail mcp` binds the registration to its parent, the harness process,
  storing its PID and start time (which guards against PID reuse). kiro-cli
  starts its MCP servers from a `node` helper, so when the parent is not a
  harness and a `kiro-cli` sits above it, the nearest `kiro-cli` is bound
  instead. opencode may start its MCP server through a wrapper (`sh -c`);
  the opencode process above the wrapper is bound in the same way, so a
  session's agent is bound to opencode whether it registered over MCP or
  from the shell.
- opencode runs every session in one process and shares one MCP server
  between them, so it binds sessions, not processes. A tool call carrying
  `_meta["ai.opencode/sessionID"]` acts as that session, and `register` binds
  the session. In opencode's shell the CLI reads `OPENCODE_SESSION_ID`. A
  `register` with no session under an opencode process (an MCP call without
  `_meta`, or a shell without `OPENCODE_SESSION_ID`) is refused, so no agent
  becomes the identity of the shared process. `register --pid` gets past this
  only when it names a process outside that opencode: a `--pid` naming the
  shell (`$$`) or any other non-harness process between it and opencode is
  replaced by the opencode process, and refused the same way. In a session,
  `--harness` (or the MCP `harness` parameter) can only name the session's
  own harness. A `--pid` naming a process outside the session's opencode
  binds that process alone, without the session.
- opencode subagents (its task tool) run in child sessions with their own
  session id, which their MCP `_meta` and `OPENCODE_SESSION_ID` carry. So
  they are separate identities, not the parent's agent: until a subagent
  calls `register` (or acts through `AGENTMAIL_ADDRESS`), its calls are
  refused with `NoIdentity`.
- Session variables count only under the harness that sets them: the CLI
  uses `OPENCODE_SESSION_ID` only when its nearest harness ancestor is
  opencode. A `claude` started from an opencode shell, or a tmux or `setsid`
  job detached from opencode, inherits the variable but does not act as that
  session.
- CLI calls walk up their process ancestors and use the nearest one with a live
  registration. `agentmail register` from a shell binds to the nearest
  harness ancestor. `--pid` picks a process explicitly: a harness process, or
  a process that is not an ancestor of the command, is bound exactly, but a
  non-harness ancestor (the shell, `$$`) under a kiro or opencode process is
  replaced by that kiro or opencode process (kiro starts MCP servers from a
  helper below it, and opencode may start them through a wrapper).
- Claude Code gets no such replacement, so an unknown MCP client run from a
  Claude Code shell binds itself rather than taking over Claude Code's agent.
  The price: configure Claude Code to run `agentmail mcp` directly (or `exec`
  it from a wrapper script). Started through a wrapper that stays its parent
  (`sh -c 'agentmail mcp'`), the server binds the wrapper, and CLI calls and
  hooks from Claude Code's shell, which look up Claude Code's process, do not
  find that agent.
- The same holds under kiro and opencode for an MCP client run from their
  shell (codex, gemini-cli or an inspector started from kiro's shell tool or
  opencode's bash tool): such a server binds the client's own process, not
  kiro-cli or opencode, so it neither takes over the agent kiro's own server
  registered nor counts as opencode's shared server. agentmail tells the two
  apart by the session variable (`KIRO_SESSION_ID`, `OPENCODE_SESSION_ID`)
  that these harnesses set for shell commands but not for their own MCP
  servers.
- `AGENTMAIL_ADDRESS=<address>` overrides the lookup, for unusual setups.

The CLI resolves who it is acting as in this order:

1. `--as human`
2. `AGENTMAIL_ADDRESS`
3. `AGENTMAIL_HUMAN=1`
4. the opencode session (`OPENCODE_SESSION_ID`, under an opencode process)
5. process ancestry

An agent is **active** while its harness process is alive, and **gone** once it
exits or the agent calls `signoff`. Gone agents stay in the directory and mail
to them is still stored.

## Reclaiming an address after a restart

Restarting, resuming or `/clear`-ing a session starts a new harness process.
When it registers with the **same name in the same working directory** as an
agent that is now gone, it takes that address back, with its unread mail, open
questions and contacts, rebound to the new process. If several gone agents
match, the most recently seen one wins. An address bound to a live process is
never taken. `register --fresh` (MCP: `fresh: true`) always gets a new address.
