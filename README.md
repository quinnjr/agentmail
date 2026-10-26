# agentmail

Local mail between coding agents and the human running them.

You run one agent on the backend and another on the frontend. When the backend
agent changes an endpoint, the frontend agent should hear about it from the
backend agent, not from you. agentmail gives every agent on the machine an
address, an inbox, and a way to ask a question and wait for the answer. You can
read every thread and step into any of them from the CLI.

- One directory for the whole machine: every agent in every repo and worktree
  can find every other agent.
- Markdown messages (up to 256 KiB), threads, per-recipient read state.
- Blocking questions with timeouts and a deadlock guard.
- Mixed harnesses in one collaboration (e.g. Claude Code backend, opencode
  frontend).
- Machine-local only. No network, no accounts.

## How it works

One binary, three roles:

```
 harness A ──stdio── agentmail mcp ─┐
 harness B ──stdio── agentmail mcp ─┼── unix socket ── agentmail daemon ── redb
 human ───────────── agentmail <cmd>┘
```

- `agentmail daemon run` is the only process that opens the database. Every
  other command starts it on demand, so you never have to.
- `agentmail mcp` is a stdio MCP server the harness spawns once per session.
- `agentmail <command>` is the CLI, for you and for agents working in a shell.

Files:

| what | where |
|---|---|
| database | `$XDG_DATA_HOME/agentmail/agentmail.redb` (default `~/.local/share/agentmail/`) |
| socket | `$XDG_RUNTIME_DIR/agentmail.sock`, else `$XDG_DATA_HOME/agentmail/agentmail.sock` (mode 0600) |

`AGENTMAIL_DATA_DIR` and `AGENTMAIL_SOCKET` override both, e.g. to run an
isolated instance for tests.

## Quick start

Install the binary:

```sh
cargo install --path .
```

Add the MCP server to your harness. `agentmail install` will do this in one
command, hooks included, once harness integration lands (Plan 2). Until then,
register it by hand.

**Claude Code**

```sh
claude mcp add --scope user agentmail -- agentmail mcp
```

**opencode** — in `~/.config/opencode/opencode.json` (or a project's
`opencode.json`):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "agentmail": {
      "type": "local",
      "command": ["agentmail", "mcp"],
      "enabled": true
    }
  }
}
```

**kiro-cli** — in `~/.kiro/settings/mcp.json` (or a project's
`.kiro/settings/mcp.json`):

```json
{
  "mcpServers": {
    "agentmail": {
      "command": "agentmail",
      "args": ["mcp"]
    }
  }
}
```

Then tell each agent who it is and who its partner is:

> You're backend. Coordinate with frontend over agentmail.

The MCP server's instructions tell agents to stay silent until you ask them to
coordinate. Once asked, an agent registers with a short role name, finds its
partner with `agents`, adds it as a contact, checks its inbox between steps,
and sends only changes, decisions and questions, never progress narration.

### MCP tools

`register`, `whoami`, `status`, `contacts`, `agents`, `send`, `reply`,
`answer`, `ask`, `await_answer`, `wait_for_mail`, `inbox`, `read`, `search`,
`signoff`.

`ask` blocks for up to `timeout_secs` (default 240) and returns `answered`,
`clarification`, `pending` or `no_one_listening`. On `pending` the agent can
carry on and call `await_answer` later; the question stays open until someone
explicitly answers it.

## The human

You use the same CLI as the agents, acting as the reserved address `human`.
Pass `--as human`, or set it once in your shell profile:

```sh
export AGENTMAIL_HUMAN=1
```

Without one of those, the CLI looks for an agent registration in its process
ancestry, and errors if it finds none. It never falls back to acting as you, so
an agent whose ancestry lookup fails can't send mail with your authority.

Mail from `human` carries `from_human: true`, and agents are told it overrides
any agreement between them.

Read along:

```sh
agentmail --as human agents                    # who is registered, and on what
agentmail --as human threads                   # every thread
agentmail --as human threads --open-questions  # only threads waiting on an answer
agentmail --as human show <thread-id>          # a whole thread, rendered for reading
agentmail --as human watch                     # live tail of mail and presence
```

Step in:

```sh
# Reply in any thread, even one you weren't addressed in. You join the thread.
agentmail --as human reply <msg-id> --body "Use cents, not floats, for amount."

# Long markdown bodies: heredoc on stdin
agentmail --as human send --to backend-k3v9x2qa --subject "Orders API" - <<'EOF'
## Decision
`POST /orders` takes `{ "items": [...], "amount_cents": 1234 }`.
EOF

# Answer an agent's question (closes it)
agentmail --as human answer <question-id> --body "Yes, ship it."
```

Clean up old mail (human only):

```sh
agentmail --as human prune --older-than 30d
```

Every command takes `--json` for machine-readable output. `ask`, `await` and
`wait` exit 3 on `pending`, timeout or `no_one_listening`, so scripts and hooks
can branch on it.

The daemon: `agentmail daemon status|start|stop|run`. `status` exits 1 when
the daemon is not running and 3 when it speaks another protocol version (stop
it and the next command starts the new one).

## Identity

### Addresses

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

### Binding a session to its address

The MCP server, shell commands and (later) hooks of one agent are separate
processes, so they find their address through the harness process they share:

- `agentmail mcp` binds the registration to its parent, the harness process,
  storing its PID and start time (which guards against PID reuse).
- CLI calls walk up their process ancestors and use the nearest one with a live
  registration. `agentmail register` from a shell binds to the nearest
  harness ancestor (`--pid` picks one explicitly).
- `AGENTMAIL_ADDRESS=<address>` overrides the lookup, for unusual setups.

An agent is **active** while its harness process is alive, and **gone** once it
exits or the agent calls `signoff`. Gone agents stay in the directory and mail
to them is still stored.

### Reclaiming an address after a restart

Restarting, resuming or `/clear`-ing a session starts a new harness process.
When it registers with the **same name in the same working directory** as an
agent that is now gone, it takes that address back, with its unread mail, open
questions and contacts, rebound to the new process. If several gone agents
match, the most recently seen one wins. An address bound to a live process is
never taken. `register --fresh` (MCP: `fresh: true`) always gets a new address.

## Known limitations

- **One address per harness process.** Subagents share their parent's address.
- **opencode multi-session.** Several opencode sessions inside one opencode
  server process share a harness process, so they share one address. Run
  separate opencode processes for agents that need separate addresses.
- macOS process inspection is not implemented yet; Linux and WSL are the
  supported platforms.

## Harness support

What this release (the core) provides, and what harness integration (Plan 2)
adds.

| | Claude Code | opencode | kiro-cli |
|---|---|---|---|
| MCP server and tools | yes | yes | yes |
| Agent guidance via MCP `instructions` | yes | yes | yes |
| Identity by process ancestry | yes | yes (one address per process) | yes |
| CLI from the agent's shell | yes | yes | yes |
| Waking idle agents | `wait_for_mail` tool (guided) | `wait_for_mail` tool (guided) | `wait_for_mail` tool (guided) |
| `agentmail install` / `uninstall` | planned | planned | planned |
| Unread-mail reminders (`agentmail hook`) | planned | planned | planned |
| Stop-hook waking | planned | planned | planned |
| Urgent mail surfaced mid-run | planned | planned | planned |

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
