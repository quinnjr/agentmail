# agentmail

**Local mail for coding agents.** Give every Claude Code, opencode and
kiro-cli session on your machine an address and an inbox, so your agents
can coordinate with each other directly instead of through you.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust 2024](https://img.shields.io/badge/rust-2024%20edition-orange.svg)
![Platform: Linux | WSL](https://img.shields.io/badge/platform-Linux%20%7C%20WSL-lightgrey.svg)
![MCP server](https://img.shields.io/badge/MCP-server-8A2BE2.svg)

You run one agent on the backend and another on the frontend. When the
backend agent changes an endpoint, the frontend agent should hear about it
from the backend agent, not from you copying and pasting between terminals.
With agentmail the two agents mail each other, ask blocking questions and
wait for answers, and wake each other up when there's news. You can read
every thread and step into any of them from your shell.

```text
frontend-x7p2m4qa → backend-k3v9x2qa   [question]  Orders API
  What does POST /orders take, and is amount in cents?

backend-k3v9x2qa → frontend-x7p2m4qa   [answer]
  `POST /orders` takes `{ "items": [...], "amount_cents": 1234 }`.
  Amounts are integer cents everywhere. Returns 201 with the order id.
```

## Features

- **Mail between agents.** Markdown messages up to 256 KiB, with threads,
  replies, read state for each recipient, search, and urgent mail that
  reaches a busy agent at its next step.
- **Blocking questions.** `ask` waits for an answer, with timeouts, a
  `clarification` state when the reply is another question, and a guard
  against two agents waiting on each other.
- **Idle agents wake up.** When an agent finishes, it just stops. New mail
  starts its next turn, with no polling and no human nudge.
- **Mixed harnesses.** For example, a Claude Code backend can work with an
  opencode frontend and a kiro-cli reviewer, all on one machine-wide
  directory of agents across every repo and worktree.
- **The human is a first-class participant.** Watch every thread live,
  reply in any of them, and answer questions yourself. Mail from `human`
  overrides any agreement between the agents.
- **Local only.** One small Rust binary and a redb database. No network,
  no accounts, no cloud.

## Quick start

1. Install the binary.

   **Linux or WSL:**

   ```sh
   curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh
   ```

   **Windows** (PowerShell) installs into WSL, where your agents run:

   ```powershell
   irm https://raw.githubusercontent.com/quinnjr/agentmail/main/install.ps1 | iex
   ```

   or from `cmd.exe`:

   ```bat
   curl -fsSLo %TEMP%\agentmail-install.cmd https://raw.githubusercontent.com/quinnjr/agentmail/main/install.cmd && %TEMP%\agentmail-install.cmd
   ```

   The script downloads the static binary for your CPU (x86_64 or
   aarch64), checks its SHA-256 and puts it in `~/.local/bin`. With Rust
   installed you can build it instead:
   `cargo install --locked --git https://github.com/quinnjr/agentmail`.

2. Wire it into your harness (the MCP server and hooks, or the opencode
   plugin):

   ```sh
   agentmail install --harness claude-code --dry-run   # see what it would change
   agentmail install --harness claude-code             # or: opencode, kiro
   ```

   To do steps 1 and 2 in one go, for every harness on your `PATH`, run
   `curl -fsSL .../install.sh | sh -s -- --harness auto`. The installer
   options are in [docs/install.md](docs/install.md#installing-the-binary).

3. Start two sessions and tell each one who it is and who its partner is:

   > You're **backend**. Coordinate with **frontend** over agentmail.

   > You're **frontend**. Coordinate with **backend** over agentmail.

That's all. Each agent registers under a short role name, finds its
partner, and from then on sends only changes, decisions and questions,
never progress narration. Agents stay silent on agentmail until you ask
them to coordinate.

`install` edits only agentmail's entries in the harness config, saves each
file's original as `<file>.agentmail.bak`, does nothing the second time
you run it, and reverses cleanly with `agentmail uninstall`. See
[docs/install.md](docs/install.md) for project-scoped installs and manual
MCP setup.

## How it works

```text
 Claude Code ──stdio── agentmail mcp ─┐   + hooks
 opencode ────stdio── agentmail mcp ─┼── unix socket ── agentmail daemon ── redb
 kiro-cli ────stdio── agentmail mcp ─┤   + hooks / plugin
 you ──────────────── agentmail <cmd>┘
```

One binary does everything:

| role | what it is |
|---|---|
| `agentmail daemon` | The only process that opens the database. Every other command starts it on demand, or you can run it under systemd with `agentmail install --service`. |
| `agentmail mcp` | The stdio MCP server each harness spawns. It gives agents 15 tools: `register`, `whoami`, `status`, `contacts`, `agents`, `send`, `reply`, `answer`, `ask`, `await_answer`, `wait_for_mail`, `inbox`, `read`, `search`, `signoff`. |
| `agentmail hook` | The harness hooks, or the opencode plugin. They remind the agent who it is and what's unread, surface urgent mail mid-run, keep open questions in front of the agent, and wake it when it's idle. |
| `agentmail <command>` | The CLI, for you and for agents working in a shell. |

**Addresses** look like `backend-k3v9x2qa`: the name the agent chose plus 8
random characters. Send to a full address, or just to `backend`. A plain
name resolves to your contact with that name, otherwise to the one active
`backend` in your repo, otherwise the mail is held until an agent
registers under that name. When a session restarts and registers with the
same name in the same directory, it gets its old address back, along with
its unread mail and open questions. Details are in
[docs/identity.md](docs/identity.md).

## You, the human

You use the same CLI, acting as the reserved address `human`. Pass
`--as human`, or set it once in your shell profile:

```sh
export AGENTMAIL_HUMAN=1
```

Read along:

```sh
agentmail agents                    # who is registered, and what they're working on
agentmail threads --open-questions  # threads waiting on an answer
agentmail show <thread-id>          # a whole thread, rendered for reading
agentmail watch                     # live tail of mail and presence
```

Step in:

```sh
# Reply in any thread, even one you weren't addressed in
agentmail reply <msg-id> --body "Use cents, not floats, for amount."

# Long markdown bodies: heredoc on stdin
agentmail send --to backend --subject "Orders API" - <<'EOF'
## Decision
`POST /orders` takes `{ "items": [...], "amount_cents": 1234 }`.
EOF

# Answer an agent's question yourself (closes it)
agentmail answer <question-id> --body "Yes, ship it."

# Clean up old mail
agentmail prune --older-than 30d
```

Every command takes `--json`. `ask`, `await` and `wait` exit with status 3
on `pending`, a timeout or `no_one_listening`, so scripts can branch on the
result. Without `--as human` or `AGENTMAIL_HUMAN=1`, the CLI acts as the
agent whose session it runs in, and it errors rather than falling back to
acting as you.

## Harness support

Each row was checked live against Claude Code 2.1.311–2.1.312, opencode
2.0.27 and kiro-cli 2.32.0, and the headless parts are covered by
[`scripts/e2e.sh`](docs/testing.md). The full findings are in
[docs/harness-notes.md](docs/harness-notes.md).

| | Claude Code | opencode ≥ 2.0 | kiro-cli (V3 engine) |
|---|---|---|---|
| `agentmail install` / `uninstall` | ✅ | ✅ | ✅ |
| MCP tools and agent instructions | ✅ | ✅ | ✅ (first 2048 chars) |
| Identity | one address per `claude` process | one address per session | one address per `kiro-cli` process |
| Who-you-are and unread reminder on each prompt | ✅ | after the first tool call | ✅ |
| Unread reminder between steps | ✅ | ✅ | at the next prompt |
| Urgent mail surfaced mid-run | ✅ next tool call | ✅ steered into the turn | ✅ next tool call |
| Open questions kept in front of the agent | ✅ | ✅ | ✅ |
| Idle agent woken by new mail | ✅ terminal UI only | ✅ while the server runs | opt-in: `install --kiro-wake` |
| CLI from the agent's shell | ✅ | ✅ | ✅ |

**Waking** is on by default. Turn it off for one agent with
`agentmail register --no-wake` (MCP: `wake: false`), or for every agent
with `AGENTMAIL_WAKE=0` in the environment you start the harness from. To
take a waiting session back, just type to it. In kiro, press Esc.

### Known limitations

- Claude Code wakes idle sessions only in its terminal UI, not under
  `claude -p`, the IDE or desktop integrations, or the SDKs.
- kiro-cli needs the V3 engine (`kiro-cli chat --v3`). Its subagents act
  as their root agent, and it never wakes in a `--no-interactive` run.
- opencode 2.0.27 has no context hook, so an agent learns its address when
  it registers rather than at session start. Waking stops when its server
  does.
- Linux and WSL only for now. Process inspection on macOS isn't
  implemented yet.

## Configuration

| variable | effect |
|---|---|
| `AGENTMAIL_HUMAN=1` | The CLI acts as `human` (same as `--as human`). |
| `AGENTMAIL_ADDRESS` | Act as this address, overriding the session lookup. |
| `AGENTMAIL_WAKE=0` | Never wake idle agents. |
| `AGENTMAIL_DATA_DIR` | Data directory. Default: `$XDG_DATA_HOME/agentmail`. |
| `AGENTMAIL_SOCKET` | Daemon socket. Default: `$XDG_RUNTIME_DIR/agentmail.sock`. |

## Documentation

- [Installing](docs/install.md): the install scripts, what `agentmail install` changes,
  `--project`, `--dry-run`, `uninstall`, and manual MCP setup.
- [Identity](docs/identity.md): addresses, name resolution, how sessions
  are bound, and reclaiming an address.
- [Operating the daemon](docs/daemon.md): files, systemd, shutdown and
  schema upgrades.
- [Testing](docs/testing.md): the test suite and end-to-end runs against
  real harnesses.
- [Harness notes](docs/harness-notes.md): what was verified in each
  harness, and how.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
bun test plugin/        # the opencode plugin
```

This project uses git-flow. Branch from `develop` as `feature/<name>` and
open pull requests against `develop`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
