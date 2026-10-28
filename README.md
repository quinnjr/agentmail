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
- `agentmail mcp` is a stdio MCP server the harness spawns: once per session
  under Claude Code and kiro, and once per working directory under opencode,
  where every session in that directory shares it.
- `agentmail <command>` is the CLI, for you and for agents working in a shell.

Files:

| what | where |
|---|---|
| database | `$XDG_DATA_HOME/agentmail/agentmail.redb` (default `~/.local/share/agentmail/`) |
| socket | `$XDG_RUNTIME_DIR/agentmail.sock`, else `$XDG_DATA_HOME/agentmail/agentmail.sock` (mode 0600) |
| daemon lock | `agentmail.lock` in the data directory: held by the running daemon, and records whether the last daemon was supervised (`--wait-for-lock`), its pid and its socket |
| waiter lock | `agentmail.waiter` in the data directory: held by a `--wait-for-lock` daemon while it waits for the daemon lock, with the same record |
| start-up error | `daemon.err` in the data directory: why the last spawned daemon failed to start, for the CLI to report; removed once a daemon starts |

`AGENTMAIL_DATA_DIR` (the data directory) and `AGENTMAIL_SOCKET` override
these, e.g. to run an isolated instance for tests.

To run the daemon under systemd instead of on demand, see
[Operating the daemon](#operating-the-daemon).

## Quick start

Install the binary:

```sh
cargo install --path .
```

Wire it into your harness:

```sh
agentmail install --harness claude-code   # or opencode, or kiro
```

That adds the MCP server and agentmail's hooks (opencode: its plugin) to the
harness's user config. See [Installing into a harness](#installing-into-a-harness)
for what it changes, `--project`, `--dry-run` and `uninstall`.

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

## Installing into a harness

```sh
agentmail install --harness claude-code|opencode|kiro [--project] [--dry-run] [--json]
                  [--kiro-wake] [--set-default]
agentmail uninstall --harness claude-code|opencode|kiro [--project] [--dry-run] [--json]
agentmail install|uninstall --service [--dry-run] [--json]
```

`--kiro-wake` and `--set-default` are for kiro only (see the notes below);
`--service` installs (or removes) the daemon's systemd user unit instead of
a harness and takes none of `--harness`, `--project`, `--kiro-wake` or
`--set-default` (see
[Running the daemon under systemd](#running-the-daemon-under-systemd)).
`--json` prints the result as JSON.

`install` edits the harness's own config files directly (it never runs the
harness's CLI). It touches only agentmail's entries, keeping every other
byte of a file as it was, saves each file's original once as
`<file>.agentmail.bak` before first changing it, and finds nothing to do
when run twice. `--dry-run` prints the changes as a diff and writes
nothing. `uninstall` removes agentmail's entries again and deletes a file
only if `install` created it, with one exception: kiro's schema rejects an
empty hooks list, so a kiro `hooks/agentmail.json` left with no hooks is
removed even if it was yours, and its text is kept in
`hooks/agentmail.json.agentmail.bak`. `--project` installs into the current
directory instead of your user config. Run the harness from the same
`agentmail` binary you installed with: the config records its absolute
path.

| harness | user config (default) | `--project` |
|---|---|---|
| Claude Code | hooks in `settings.json`, MCP server in `.claude.json`, both under `$CLAUDE_CONFIG_DIR` when set, else `~/.claude/settings.json` and `~/.claude.json` | `.claude/settings.json`, `.mcp.json` (Claude Code asks you to approve the server the first time) |
| opencode ≥ 2.0 | `opencode.json` (MCP server, `codemode: false`) and `plugins/agentmail.js` in `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode` | `opencode.json`, `.opencode/plugins/agentmail.js` |
| kiro-cli (V3) | `settings/mcp.json` and `hooks/agentmail.json` in `$KIRO_HOME`, else `~/.kiro` | `.kiro/settings/mcp.json`, `.kiro/hooks/agentmail.json` |

Notes:

- **kiro-cli: V3 engine only.** Run agents with `kiro-cli chat --v3`. kiro's
  default (V2) engine is not supported; `install` says so. Waking an idle
  kiro agent is off unless you install with `--kiro-wake` (see
  [Harness support](#harness-support)). Install at one scope only: kiro
  loads the user's and a trusted workspace's hooks together, so both would
  run every hook twice. `--set-default` is accepted and changes nothing:
  kiro V3 loads agentmail's hooks for every agent.
- **opencode `codemode`.** agentmail's server is installed with
  `codemode: false`. opencode only shows a server's `instructions` (how
  agents should use agentmail) while one of its tools is directly
  reachable, which code mode hides behind its `execute` tool. A
  `opencode.jsonc` with comments is never rewritten: install prints the
  entry to add by hand instead. opencode picks up the change without a
  restart.

To run the daemon as a systemd user service instead of on demand:
`agentmail install --service` (see
[Running the daemon under systemd](#running-the-daemon-under-systemd)).

To register the MCP server by hand instead (no hooks, so no reminders,
urgent pushes or waking): `claude mcp add --scope user agentmail --
agentmail mcp`; in `opencode.json`, `"mcp": {"servers": {"agentmail":
{"type": "local", "command": ["agentmail", "mcp"], "codemode": false}}}`;
in kiro's `settings/mcp.json`, `"mcpServers": {"agentmail": {"command":
"agentmail", "args": ["mcp"]}}`.

## Operating the daemon

### Running the daemon under systemd

On-demand start is enough, but a daemon started by an agent outlives that
agent's session outside any supervisor. To have systemd own it instead, run:

```sh
agentmail install --service            # --dry-run shows what it would do
loginctl enable-linger "$USER"         # optional: keep it running while logged out
```

What it does:

- Writes the unit below to `~/.config/systemd/user/agentmail.service` (under
  `$XDG_CONFIG_HOME` when that is set to an absolute path), running the
  `agentmail` you invoked.
- Runs `systemctl --user daemon-reload` and `enable --now agentmail`,
  restarts the service if it was already running on an older unit, and
  stops a daemon an agent started on demand, so the service's daemon takes
  over.
- Never loses a unit of yours: one already there (say, written by hand from
  this README) is saved as `agentmail.service.agentmail.bak` first, and a
  unit agentmail wrote that you have since edited is not replaced at all.
- Notes when your shell sets `AGENTMAIL_DATA_DIR` or `AGENTMAIL_SOCKET`
  differently from the service, with the lines to give the service the same
  (see the drop-in below).

Put your own settings for the service in a drop-in
(`systemctl --user edit agentmail`), not in `agentmail.service`: install and
uninstall never touch drop-ins.

`agentmail uninstall --service` disables and stops the service and removes
the unit (cleaning up after one you already removed by hand). It leaves
drop-ins and a saved `.agentmail.bak` unit in place, and says how to restore
the latter.

Both refuse, changing nothing and printing the steps to do by hand, when the
systemd user manager is not available, or when it would not see the unit
(its own `XDG_CONFIG_HOME` or `HOME` differs from your shell's). If a step
fails part-way, the error says what is left and how to finish; running the
same command again finishes it. Add `--dry-run` to see what either would do.

To set it up by hand instead, add a user unit at
`~/.config/systemd/user/agentmail.service`:

```ini
[Unit]
Description=agentmail local agent mail daemon
# Tolerate bursts of stops (e.g. several clients upgrading at once) but
# still give up on a daemon that fails at every start; see below.
StartLimitIntervalSec=60s
StartLimitBurst=30

[Service]
ExecStart=%h/.cargo/bin/agentmail daemon run --wait-for-lock
Restart=always
RestartSec=500ms
# Signal only the daemon on stop; see below.
KillMode=mixed

[Install]
WantedBy=default.target
```

`ExecStart` assumes `cargo install` put the binary in `~/.cargo/bin`; point it
at wherever yours lives. Enable the unit, then stop any daemon already
running on demand, which the service's daemon, waiting for the lock, then
replaces:

```sh
systemctl --user daemon-reload
systemctl --user enable --now agentmail
agentmail daemon stop
loginctl enable-linger "$USER"   # optional: keep it running while logged out
```

`daemon run` stays in the foreground and shuts down cleanly on SIGTERM or
SIGINT: it logs `signal received; finishing in-flight requests`, removes
its socket, drops open connections (blocked clients get
`daemon connection lost`) and releases the lock once in-flight requests
finish. Every acknowledged write is already durable, so `systemctl stop` loses
nothing. A second SIGTERM or SIGINT during that wait exits at once with
status 1, first killing any `git` commands those requests still run.

`KillMode=mixed` is what makes that drain work under systemd. With the
default (`control-group`), `systemctl stop` and `restart` send SIGTERM to
every process in the unit at once, including the `git` commands in-flight
requests run to find an agent's repository. Those requests would then
finish without one, and `register` would record the agent with no repo.
With `mixed`, only the daemon gets SIGTERM; any process left in the unit is
sent SIGKILL once the daemon exits, or at `TimeoutStopSec` if it has not. The
daemon reaps its `git` commands before it exits, so normally nothing is left.

With `--wait-for-lock`, a unit started while another daemon (say, one an
agent started on demand) holds the lock waits for that daemon to exit and
then takes over, instead of exiting. While it waits it holds a lock on
`agentmail.waiter` in the data directory, so a client that stops the other
daemon (`agentmail daemon stop`, or a newer client replacing an older daemon)
waits for the unit's daemon to take over rather than starting another
on-demand daemon it would have to wait behind. Without the flag, `daemon run`
exits 0 at once and leaves the other daemon serving, and a unit doing that
shows as `inactive (dead)`: systemd supervises nothing until it is started
again.

`Restart=always` brings the daemon back whenever it exits, including on
`agentmail daemon stop` and when a client of a newer version stops an older
daemon after an upgrade. systemd's default start rate limit (5 starts in
10s) would leave the unit `failed` after a burst of such stops, so the unit
raises it to 30 starts in 60s. That still catches a daemon that fails at
every start (say, its database cannot be opened, its data directory cannot be
created, or its socket path is too long): restarted every 500ms, it uses up
the 30 starts in about 15s and the unit goes to `failed`, which
`systemctl --user status agentmail` shows, with the daemon's errors in
`journalctl --user -u agentmail`. Fix the cause, then
`systemctl --user reset-failed agentmail` and start it again; do the same if
an unusually long burst of stops ever exhausts the limit. Avoid
`StartLimitIntervalSec=0`, which removes the limit: a daemon that cannot
start is then restarted twice a second forever, flooding the journal, and the
unit never shows as failed. So with the unit enabled:

- stop the daemon with `systemctl --user stop agentmail` (and `disable` it to
  keep it stopped across logins); `agentmail daemon stop` only restarts it;
- restart it, e.g. after `cargo install`, with `systemctl --user restart
  agentmail`.

Clients still start a daemon on demand whenever none is reachable, e.g.
while the unit is stopped. To avoid racing systemd, a daemon run with
`--wait-for-lock` notes in its lock file that it is supervised. A client
that finds no daemon answering first waits for one that is shutting down to
finish its in-flight requests (up to 5s), then, if the last daemon was
supervised or a supervised one is waiting for the lock, gives it up to 2s to
take over before starting a daemon of its own. Without a supervisor the
client waits only for a draining daemon, so on-demand start stays immediate.
The first client after `systemctl --user stop` therefore takes about 2s
longer to start its daemon. Every daemon from a build that has
`--wait-for-lock` records itself in the lock file (supervised or on-demand,
with its process id); a daemon from an older build leaves whatever an
earlier daemon wrote there. So right after upgrading from such a build,
while its daemon still runs, a client may wait those 2s needlessly (if a
unit daemon once recorded itself and has since been disabled), or not wait
for a supervisor; restart the daemon once after upgrading
(`systemctl --user restart agentmail`, or `agentmail daemon stop`) to be
done with it. If the daemon that takes over speaks another protocol
version (the unit's `ExecStart` runs a different `agentmail` than the
client), the client says so at once instead of waiting.

A window remains: if systemd takes longer than that to bring the daemon
back (a slow start, or a shutdown that outlasts the 5s wait), a client
starts an on-demand daemon, and the unit's daemon waits behind it for as
long as it runs, which is until it is stopped. The unit then still shows
`active (running)`, but its log (`journalctl --user -u agentmail`) ends with
`another daemon holds ... waiting for it to exit`. To recover, run
`agentmail daemon stop`: the on-demand daemon exits and the unit's takes
over.

The systemd user manager does not read your shell profile. If you set
`XDG_DATA_HOME`, `XDG_RUNTIME_DIR` (systemd sets this one itself),
`AGENTMAIL_DATA_DIR` or `AGENTMAIL_SOCKET` there, the unit would serve a
different data directory or socket than your clients use (they then report
the data directory mismatch, or start a daemon of their own). Either leave
them unset, or give the service the same values, with `Environment=` lines
in a drop-in. Run `systemctl --user edit agentmail`, which saves what you
add to `~/.config/systemd/user/agentmail.service.d/override.conf`:

```ini
[Service]
Environment=AGENTMAIL_DATA_DIR=%h/agentmail-data
```

then `systemctl --user restart agentmail`. Do not add them to
`agentmail.service` itself: that file is agentmail's, and `agentmail install
--service` refuses to replace it once you have edited it (it would drop your
lines), while it never touches the drop-in, nor does uninstall.

Alternatively, import them from your shell before starting it (this lasts
only until the user manager restarts, so the drop-in is the durable choice):

```sh
systemctl --user import-environment XDG_DATA_HOME AGENTMAIL_DATA_DIR AGENTMAIL_SOCKET
systemctl --user restart agentmail
```

### Upgrading the database

A newer agentmail may change the database schema. The first daemon of the
new version to open the database migrates it in place, in one transaction,
and keeps all mail and read state. The migration is one-way: there is no
downgrade, and an older agentmail refuses the migrated database.

Schema 2 (harness integration) is such a change. An agentmail built before
it, run against a schema-2 database, gives a clear reason only from
`agentmail daemon run`:

```
.../agentmail.redb has schema version 2, this build supports 1
```

Its CLI commands and MCP server, which start the daemon on demand,
instead report a misleading error such as `the daemon holding
.../agentmail.lock is still shutting down (finishing in-flight requests); try
again once it has exited: No such file or directory`, and never mention the
schema. That older build cannot be fixed after the fact. If you see that
error, run `agentmail daemon run` with the same binary to see the real
reason, and use a single agentmail binary everywhere (the CLI, every
harness's MCP and hook configuration, and the systemd unit) so that none of
them is older than the database.

## The human

You use the same CLI as the agents, acting as the reserved address `human`.
Pass `--as human`, or set it once in your shell profile:

```sh
export AGENTMAIL_HUMAN=1
```

Without one of those, the CLI resolves an agent identity (see
[Binding a session to its address](#binding-a-session-to-its-address)): an
explicit `AGENTMAIL_ADDRESS`, then the opencode session it runs in, then an
agent registration in its process ancestry. It errors if it finds none. It
never falls back to acting as you, so an agent whose lookup fails can't send
mail with your authority.

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

### Reclaiming an address after a restart

Restarting, resuming or `/clear`-ing a session starts a new harness process.
When it registers with the **same name in the same working directory** as an
agent that is now gone, it takes that address back, with its unread mail, open
questions and contacts, rebound to the new process. If several gone agents
match, the most recently seen one wins. An address bound to a live process is
never taken. `register --fresh` (MCP: `fresh: true`) always gets a new address.

## Known limitations

- **Waking an idle agent depends on how the harness runs.** Claude Code
  wakes an idle session only in its terminal UI (`claude`); under `claude
  -p`, the IDE and desktop integrations and the SDKs the wait hook stays
  silent, since a headless run would otherwise be held open until mail
  came. kiro-cli wakes only when installed with `--kiro-wake`, and never in
  a `--no-interactive` run. opencode wakes an idle session for as long as
  its server runs (the TUI, `opencode serve`); `opencode run --standalone`
  stops its server when the turn ends.
- **kiro-cli subagents act as their root agent.** kiro gives a subagent's
  hooks and shell the root conversation's session and process, so urgent
  mail that arrives while a subagent runs is pushed to the subagent, which
  may or may not pass it on (the root still sees it counted unread at the
  next prompt). Claude Code subagents share the parent's address but its
  hooks skip them, so the parent gets the push. opencode subagents run in
  their own sessions, so each is a separate identity that must `register`
  (or use `AGENTMAIL_ADDRESS`) before it can act.
- **kiro-cli's V2 engine is not supported**; run `kiro-cli chat --v3`.
- macOS process inspection is not implemented yet; Linux and WSL are the
  supported platforms.

## Harness support

Supported: Claude Code, opencode 2.x (V2 plugin API) and kiro-cli with the
V3 engine. Each row was checked live against Claude Code 2.1.311–2.1.312,
opencode 2.0.27 and kiro-cli 2.32.0 (details in
[`docs/harness-notes.md`](docs/harness-notes.md)), and the headless parts
by [`scripts/e2e.sh`](#end-to-end-tests-against-real-harnesses).

| | Claude Code | opencode | kiro-cli (V3) |
|---|---|---|---|
| `agentmail install` / `uninstall` | yes (`settings.json`, `.claude.json`) | yes (`opencode.json`, plugin) | yes (`settings/mcp.json`, `hooks/agentmail.json`) |
| MCP server and tools | yes | yes (installed with `codemode: false`) | yes |
| Agent guidance via MCP `instructions` | yes | yes, while not in code mode | yes |
| Agent identity | by process (one address per `claude` process) | by session (one address per session) | by process (the nearest `kiro-cli`) |
| Who-you-are and unread reminder at session start and on each prompt | yes (`SessionStart`, `UserPromptSubmit`) | no prompt hook in opencode 2.0.27: the agent learns its address from `register`, reminders start after its first tool call | yes (`SessionStart`, `UserPromptSubmit`) |
| Unread-mail reminder between steps | yes (`PostToolUse`, also for failed calls) | yes (appended to each tool result) | no (kiro ignores `PostToolUse` output); at the next prompt instead |
| Urgent mail surfaced mid-run, once | yes, at the next tool call | yes, steered into the running turn | yes: the next tool call is held once with the mail, then can run again |
| Open questions kept in front of the agent at stop | yes (Stop hook blocks, not twice in a row) | yes (queued as one more turn) | yes (Stop hook continues the turn once per prompt) |
| Waking an idle agent for new mail | terminal UI only (`asyncRewake` Stop hook) | yes, while the server runs (plugin) | opt-in with `install --kiro-wake`; interactive only |
| CLI from the agent's shell | yes | yes (by `OPENCODE_SESSION_ID`) | yes |
| Daemon down or session unregistered | hooks print nothing | plugin does nothing | hooks print nothing |

## End-to-end tests against real harnesses

`scripts/e2e.sh <claude-code|opencode|kiro|mixed>` builds agentmail,
installs it into a throwaway home (private `HOME`, `XDG_*`,
`CLAUDE_CONFIG_DIR`, `KIRO_HOME`, data directory and socket), and drives two
real agents headlessly: `backend` registers and answers questions,
`frontend` registers and asks it what `POST /orders` takes. It checks that a
question/answer thread exists between them, that urgent mail sent to
`backend` mid-run reaches it, for kiro that the agent is bound to the
`kiro-cli` process, and for opencode that an idle `backend` is woken by new
mail (it replies, or opencode's session log shows a turn the wake started
that saw the mail). Claude Code and kiro do not wake idle agents in a
headless run, so their waking is checked only by hand (see
`docs/harness-notes.md`). In `opencode` mode both agents run on one
`opencode serve`, so their sessions share one MCP server. `mixed` runs
`backend` under Claude Code and `frontend` under opencode. It exits
non-zero and names the failed step on any failure.

It needs real model access, so `cargo test` does not run it:

- Claude Code: `AGENTMAIL_E2E_CLAUDE=/path/to/claude` (the binary, not a
  shell function), and a login (`~/.claude/.credentials.json`, or
  `$CLAUDE_CONFIG_DIR`'s, or `AGENTMAIL_E2E_CLAUDE_CREDENTIALS`) or
  `ANTHROPIC_API_KEY`.
- opencode: on `PATH` or `AGENTMAIL_E2E_OPENCODE`; uses a free opencode
  model by default (`AGENTMAIL_E2E_OPENCODE_MODEL`).
- kiro-cli: on `PATH` or `AGENTMAIL_E2E_KIRO`, logged in
  (`AGENTMAIL_E2E_KIRO_DATA` for a non-default data directory). Only the
  runtimes kiro-cli never writes are linked from that directory; the login
  and feed files are copied, the rest left out.
- `jq`, `git`, `cargo` and `setsid` (util-linux) in every mode, plus
  `python3` for kiro and `opencode` mode, and `curl` for `opencode` mode.

Logins are copied into the throwaway home without their refresh token, so
a run can never rotate your real login, and deleted afterwards. A copy
cannot outlive its access token, so a login that expires before the
longest possible run ends is refused: by default 6 ×
`AGENTMAIL_E2E_TIMEOUT` (600 s per wait) + 300 seconds, or
`AGENTMAIL_E2E_MIN_LOGIN_SECS` to run on a login that expires sooner. The
API key and opencode's server password reach the harnesses through their
environment, never a command line. Each harness runs in its own process
group, killed with everything it started on a timeout and on exit.
`AGENTMAIL_E2E_KEEP=1` keeps the throwaway directory (logs and
transcripts) for inspection.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
bun test plugin/        # the opencode plugin
```

`scripts/e2e.sh` runs real harnesses; see
[End-to-end tests against real harnesses](#end-to-end-tests-against-real-harnesses).
