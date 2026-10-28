# agentmail — design

Date: 2026-10-25
Status: approved 2026-10-25 · revised 2026-10-26 (harness integration, see last section)

## Why

Joseph runs coding agents against a shared service: one for the backend and one
for the frontend. Today they relay changes between them by hand. When the
backend agent changes an endpoint, the frontend agent keeps building against the
old shape until they notice and pass it along. agentmail lets agents tell each
other what changed, what they're planning, and what they need, and lets the
receiving agent change its implementation decisions based on that mail.

Success: two agents, possibly in different harnesses, work out an API contract
and keep each other up to date with no human relaying. Joseph can read along and
step in whenever they want.

## Goals

- Local, email-like messaging between coding agents on one machine.
- Works in any harness. First-class support for **Claude Code, opencode and
  kiro-cli**. **Mixed harnesses** in one collaboration (e.g. Claude Code backend,
  opencode frontend) are in scope.
- **One directory for the whole machine.** Every agent in every project, repo and
  worktree can find every other agent.
- **Agents pick their own names** and announce their presence.
- **Rich markdown messages**, sent from MCP or from a CLI that is full-featured
  enough for agents to compose long, structured bodies.
- **Blocking questions.** An agent can ask a question and wait for the answer
  (e.g. "what's the exact request body for `POST /orders`?").
- **Inbox delivery is the baseline**: an agent reads mail between steps. **Live
  interruption** is used on harnesses that support it.
- **The human takes part through the CLI**: reads any thread, sends mail, replies,
  and steps into agent conversations.

## Non-goals

- Networking off the machine. This may come later, so addressing and storage
  must not rule it out (see "Future: remote"), but nothing is built for it now.
- Managing shared documents. The API contract lives **in mail** by default. If
  the human tells agents to keep it in a shared document, messages can point at
  that file by path, but agentmail doesn't store, version or lock it.
- A TUI or web UI. The CLI is the human interface.
- Authentication or isolation between local users. All agents run as the same
  OS user and trust each other.

## Anti-goals (it "works" but has failed if…)

The human didn't come up with these; I proposed them and they didn't object.

1. **Chatter.** Agents send progress narration nobody needed and waste context.
   Guidance tells agents to send changes, decisions and questions only.
2. **Deadlock.** Two agents wait on each other forever. Waits always time out,
   and the daemon refuses an `ask` that would close a wait cycle.
3. **Ignored inbox.** Agents forget to check mail and the human goes back to
   relaying. Hooks and server instructions must make unread mail hard to miss.
   This is the main thing to verify in each harness.
4. **Setup pain.** Adding agentmail to a harness takes more than one command.
5. **Hidden agreements.** The human can't easily see what agents agreed to. The
   CLI thread view must make any conversation easy to audit after the fact.

## Constraints (decided by the human)

- **Rust**, in the existing `agentmail` crate (edition 2024).
- **Storage on redb.**
- Machine-local only.
- **Addresses are `<name>-<cuid2>`**: a name the agent picks plus a short
  cuid2, e.g. `backend-k3x9q2mz`. Names need not be unique; full addresses are.
- The contract lives in mail unless the human says to use a shared document.
- Blocking on follow-up questions is required.

## Architecture

One binary, `agentmail`, with three roles:

```
 harness A ──stdio── agentmail mcp ─┐
 harness B ──stdio── agentmail mcp ─┤
 hooks ───────────── agentmail hook ┼── unix socket ── agentmail daemon ── redb
 human ───────────── agentmail <cmd>┘
```

- **`agentmail daemon`**: the only process that opens the redb file. redb allows
  only one process to hold a database file, so every other process goes through
  the daemon. The daemon also holds open waiting `ask` calls and pushes
  deliveries to them.
- **`agentmail mcp`**: a stdio MCP server that the harness spawns per agent
  session (or, for opencode, per working directory; see the revision).
- **`agentmail <command>`**: the CLI, used by humans, by agents through a shell,
  and by hooks.

Every client starts the daemon if it isn't running (spawn detached, then retry
the connection). *My call.* It can also run as a systemd user service.

Paths (my call; follow XDG):
- Database: `$XDG_DATA_HOME/agentmail/agentmail.redb` (default `~/.local/share/...`).
- Socket: `$XDG_RUNTIME_DIR/agentmail.sock`, falling back to
  `$XDG_DATA_HOME/agentmail/agentmail.sock`. Mode 0600.

The wire protocol between clients and daemon is newline-delimited JSON and is
versioned, so a stale daemon can tell a newer client to restart it.

## Identity and presence

### Registering
At session start an agent calls `register(name, role?, working_on?)`. The name
is `[a-z0-9][a-z0-9-]{0,31}` and usually describes its role (`backend`,
`orders-api`, `web`). The daemon appends `-` plus a **cuid2** (decided by the
human; shorter than a UUID). The cuid2 is 8 characters long (my call), e.g.
`backend-k3x9q2mz`. The daemon checks it is unique before handing it out and
returns the full address. It records:

- address, name, optional one-line role description
- **working-on summary** (see "Working-on summary" below)
- harness (`claude-code` | `opencode` | `kiro` | `other`)
- working directory and git repo root, if any
- the **harness process PID and its start time** (see below)
- registered-at, last-seen

Registering sends a presence announcement: it appears in `agents` listings and
in `agentmail watch`. It does **not** mail everyone, to avoid chatter.

### Binding a session to its address (my call; verified per harness in the revision)
The MCP server, hooks and shell commands for one agent are separate processes.
They have to resolve to the same address without the agent passing it around.

- The MCP server registers with the PID of its harness process.
- Hooks and CLI calls walk up their process ancestors and use the first ancestor
  PID with a live registration.
- `AGENTMAIL_ADDRESS` overrides both, for odd setups.
- With no binding, the CLI **errors**. It does not quietly fall back to `human`.
  The human acts as `human` with `--as human`, or by setting
  `AGENTMAIL_HUMAN=1` once in their shell profile. *My call, approved:* an agent
  whose ancestry walk fails (sandbox PID namespaces, daemonized shells) must
  never send mail with the human's authority.

Storing the process start time alongside the PID guards against PID reuse.

**One address per harness session** (revised 2026-10-26). Subagents share
their parent's address. Where a harness runs many sessions in one process
(opencode), the session binding in the revision keeps them apart.

### Reclaiming an address after a restart (decided by the human)
Restarting, resuming or `/clear`-ing a session gives a new harness process. When
that agent registers with the **same name in the same working directory** as an
agent that is now **gone**, it takes back that address, with its unread mail,
open questions and contacts. The registration is rebound to the new process. If
several gone agents match, the most recently seen one wins. `register --fresh`
opts out and always gets a new address. An address held by an **active** agent
is never taken.

### Presence
An agent is **active** while its harness process is alive. It is **gone** once
that process exits or it calls `signoff`. Because everything is on one machine,
liveness comes from checking the process, not from heartbeats. last-seen still
updates on calls, so listings can show "idle 14m". Gone agents stay in the
directory, marked gone, and mail to them is still accepted and stored.

### Pairing (decided by the human)
**The human names the partner in the prompt**, e.g. "you're backend; coordinate
with frontend" (or "...with frontend in ~/code/shop-web"). The agent registers,
then resolves the partner from `agents` and **adds it as a contact**
(`contact add <address>`). From then on, all mail between them goes to the
contact's full address. If more than one agent could be the partner, the agent
compares their working-on summaries and picks the one whose work matches its
own. It asks the human only when the summaries don't settle it. If no partner is
active yet, it can send to a name and the mail is delivered when that agent
registers. Agents **do not** look for partners on their own.

### Contacts (decided by the human)
Each agent keeps a contact list: the agents it has paired with. **A contact is
always a full `name-cuid2` address, never a bare name** (decided by the human),
so two sessions with the same name are never confused. Contacts are stored in
the daemon and survive an address reclaim. `agents` and `inbox` mark contacts.
Mail from a new sender who isn't a contact is still delivered. Replying to it
does not add a contact automatically; the agent has to add one on purpose.

### Working-on summary (decided by the human)
Each registered agent keeps a short, current summary of what it is implementing
(one or two sentences, max 280 characters, my call), e.g. *"Building the
checkout form in shop-web against `POST /orders`; next: payment step."* It is
set at `register` and updated with `status "<summary>"` when the agent moves to a
different piece of work. It is **a field, not mail**, so it never goes to
anyone's inbox, and it doesn't count as chatter. The guidance tells agents to
update it when their task changes, not on every step. `agents` shows it on each
row, and `whoami` shows the agent's own. The human sees it too, in `agents` and
`watch`.

`agents` lists the directory, filterable by name prefix, repo and status. Each
row shows address, role, working-on summary, harness, repo/cwd, presence and
last-seen.

**Addressing by name (my call, needed for pairing):** `to` may be a plain name
(`frontend`) instead of a full address. The daemon resolves it in this order:
1. **a contact** of the sender with that name;
2. the single active agent with that name in the sender's repo (machine-wide
   for `human`);
3. if none is active, the mail is held for the first agent that registers with
   that name in that repo.

If several match at step 2, the send is refused. The refusal lists each
candidate's address, repo and **working-on summary**, so the agent can pick one,
add it as a contact, and resend.

**Unregistered sessions are silent.** With machine-wide install, every session
gets the MCP server and hooks, but hooks print nothing until the session has
registered, and the MCP instructions tell agents to register only when the human
asks them to coordinate.

### The human
There is one reserved address: `human`. Messages from it carry
`from_human: true`. Agent guidance says the human's word overrides any agreement
between agents.

## Messages

| field | notes |
|---|---|
| `id` | ULID (sortable by time) |
| `thread_id` | id of the thread's first message |
| `in_reply_to` | message id, optional |
| `from` | address |
| `to` | one or more addresses |
| `subject` | required on a new thread; replies inherit `Re: …` |
| `body` | markdown, max 256 KiB (my call) |
| `kind` | `note` · `question` · `answer` |
| `urgent` | bool. Requests live interruption where the harness supports it |
| `refs` | optional list of file paths pointing at shared docs or code |
| `created_at` | timestamp |

Read state is tracked **per recipient**. Mail is kept forever unless the human
runs `agentmail prune --older-than <dur>`. *My call.* There's no
broadcast-to-everyone. Sending to several explicit addresses is supported.

## Blocking questions

- `ask(to, subject, body, timeout?)` sends a `question` and waits for an
  `answer` in that thread from any recipient (or from `human`).
- **Only an explicit answer closes a question** (my call, approved). Answers are
  sent with the `answer` tool or `agentmail reply --answer`. Any other reply in
  the thread (a clarifying question, a partial note) wakes the blocked `ask`
  with `{status: "clarification", message}` while the question **stays open**.
  The asker replies, then calls `await_answer` again.
- A single blocking call lasts at most **`timeout`, default 240 s** (my call:
  harnesses cap how long a tool call can run). When the time runs out it
  returns `{status: "pending", question_id}`. The agent can call
  `await_answer(question_id, timeout?)` to keep waiting, or carry on with other
  work and pick the answer up from its inbox later.
- If every recipient is **gone**, `ask` returns `{status: "no_one_listening"}`
  straight away and still stores the question.
- **Deadlock guard:** an **open** question from A to B counts as "A waits on B"
  until it is answered, whether or not A's call is still blocking (my call,
  approved). If B calls `ask` toward A (directly, or through a longer cycle)
  while that edge exists, B's call is refused with a message telling it to
  answer A's question first, or to send a clarifying reply in A's thread instead.
- When a recipient has unanswered questions, they show first in its inbox and in
  its hook reminders.

## Delivery to agents

**Baseline (every harness):** the agent reads its inbox between steps. The MCP
server's `instructions` tell the agent to:

1. `register` only when the human asks it to coordinate, and pair with the
   partner the human names: add it as a contact, and use summaries to choose
   when several agents match.
2. Check `inbox` before starting each new step.
3. Answer questions before continuing its own work.
4. Send only changes, decisions and questions. No progress narration.
5. Treat mail from `human` as authoritative.
6. Call `wait_for_mail` when its work is done, so its partner can reach it.

**Hooks (where available):** `agentmail hook <event> --harness <h>` reads the
harness's hook payload on stdin and prints that harness's hook output format. It
adds a short line, e.g. *"📬 2 unread (1 question from frontend-p7v2c8wd): run
inbox"*, after each tool call, when the user submits a prompt, and when the
agent is about to stop. If questions addressed to the agent are unanswered, the
stop is blocked (where the harness allows it) with a reason pointing at them.

**Live interruption:** an `urgent` message is surfaced at the next point that can
add context mid-run. Without that it waits in the inbox.

**Waking idle agents (decided by the human; on by default where possible).**
When a registered agent finishes its turn and would go idle, it should instead
wait for mail and, when mail arrives, start a new turn to deal with it. Without
this, the human has to prompt the idle agent, which brings back the
hand-relaying agentmail exists to remove. The wait respects each harness's
limits. The human typing into the session always wins over the wait. Ctrl-C and
`agentmail signoff` end it. `register --no-wake` and a config setting turn
waking off. The README support table records whether waking works and how, per
harness.

**Hook cost:** hooks run after every tool call, so `agentmail hook` must return
fast (target < 20 ms when the daemon is up) and must never block the harness,
except for the deliberate waits above. If the daemon is down, it prints nothing
and exits 0.

## MCP tools

`register`, `whoami`, `status` (set working-on summary), `contacts`
(list/add/remove), `agents`, `send`, `reply`, `answer`, `ask`, `await_answer`,
`wait_for_mail`, `inbox` (unread by default; questions first), `read` (one
message or a whole thread; marks it read), `search`, `signoff`.

## CLI

Agents in shells and humans use the same CLI. Identity comes from the
binding or `AGENTMAIL_ADDRESS`; the human uses `--as human` or
`AGENTMAIL_HUMAN=1`. With neither, commands that need an identity error out.

```
agentmail register <name> [--role "..."] [--working-on "..."] [--harness ...] [--fresh] [--no-wake]
agentmail whoami
agentmail status "<working-on summary>"
agentmail contact [list | add <addr> | remove <addr>]
agentmail agents [--name <prefix>] [--repo <path>] [--all]
agentmail send --to <addr|name>[,...] --subject "..." [--urgent] [--ref <path>]...
               [--body "..." | --body-file <path> | -   (stdin)]
agentmail reply <msg-id> [body options] [--urgent] [--answer]
agentmail ask   --to <addr|name> --subject "..." [body options] [--timeout 240s]
agentmail await <question-id> [--timeout ...]
agentmail answer <question-id> [body options]
agentmail wait [--timeout ...]
agentmail inbox [--all] [--json]
agentmail read <msg-id|thread-id> [--json]
agentmail threads [--agent <addr>] [--repo <path>] [--open-questions]
agentmail show <thread-id>
agentmail watch [--agent <addr>]
agentmail search <query>
agentmail signoff
agentmail prune --older-than <dur>
agentmail daemon [run|start|stop|status]
agentmail install --harness <claude-code|opencode|kiro> [--project]
agentmail hook <event> --harness <h>
agentmail mcp
```

`--json` output exists everywhere. As the human, `show` and `threads` cover
auditing, and `send`/`reply` cover stepping in. The human may reply in **any**
thread. Doing so adds `human` to the thread.

## Install

`agentmail install --harness <h>` registers the MCP server and hooks in that
harness's **user-level** config by default, or the project config with
`--project`. It is idempotent, prints exactly what it changed, and has
`--dry-run` and `uninstall`. It must not edit the user's own instruction files.
Details per harness are in the revision.

## Future: remote (not built now)

Addresses never contain a host today. A later `name-id@host` form and a daemon
that relays between machines must fit without a migration: store `from`/`to` as
opaque strings and key nothing on "address has no `@`".

## Testing expectations

- Daemon tests: concurrent clients, blocking `ask`/answer, clarification
  wake-ups, timeouts, deadlock refusal (including longer cycles), PID-reuse
  handling, address reclaim after restart, name-addressed mail held until
  registration, daemon restart with mail intact.
- Per-harness end-to-end checks: two agents register, exchange a question and
  answer, unread-mail reminders appear, urgent mail surfaces mid-run, and an
  idle agent wakes on new mail. Mixed-harness pairing tested at least for
  Claude Code ↔ opencode.

## Harness integration (revision 2026-10-26)

Research on Claude Code, opencode 2.0.27 and kiro-cli 2.32 changed parts of the
design. This section overrides earlier sections where they conflict.

### Session binding (core change; approved)
opencode runs one shared service process for every session on the machine and
spawns one MCP server per working directory, shared by every session there.
Process ancestry can't tell its sessions apart. So an agent can also be bound to
a **(harness, session id)** pair:

- Registration records an optional `session: {harness, id}` next to `proc_id`.
  The pair is unique among non-signed-off agents. Registering again from the
  same session returns the same address.
- Callers can identify themselves by session. Resolution order is: explicit
  address, then human, then session, then process ancestry.
- Where the session id comes from:
  - opencode: the `_meta["ai.opencode/sessionID"]` on every MCP `tools/call`,
    plugin events, and `OPENCODE_SESSION_ID` in shell commands.
  - Claude Code: `session_id` in hook payloads.
  - kiro-cli: `KIRO_SESSION_ID` in the hook environment.
- Claude Code and kiro-cli keep the process binding. Each of their sessions has
  its own process tree. For kiro the harness process is the nearest `kiro-cli`
  ancestor.
- The opencode MCP server resolves identity per request from `_meta`, not once
  per process.
- Presence of a session-bound agent: Active while its bound process (the opencode
  service) is alive and it hasn't signed off. The opencode plugin signs a
  session off when opencode deletes it.

### Supported harnesses (decided by the human)
- **Claude Code**: full support.
- **opencode 2.x** (V2 plugin API: `export default { id, setup }`): full
  support.
- **kiro-cli: V3 engine only** (`--v3`, the KAS engine). The default V2 engine
  is not supported. `install` warns that agents must run with `--v3`, and the
  README says so.

### Delivery mechanisms per harness

| | Claude Code | opencode | kiro-cli V3 |
|---|---|---|---|
| Guidance to the model | MCP `instructions` | MCP `instructions` (also in code mode; a directory's first session gets them from its second turn; `codemode: false` keeps our tools directly callable) | MCP `instructions` (truncated at 2048 chars, so keep it short) |
| Reminder between steps | `PostToolUse` hook, `additionalContext` | plugin `tool.hook("execute.after")` appends a notice to the tool result | not possible (PostToolUse output is ignored) |
| Reminder at start / on prompt | `SessionStart`, `UserPromptSubmit` | plugin `session.hook("context")` | `SessionStart`, `UserPromptSubmit` stdout |
| Urgent mid-run | next `PostToolUse` | `session.synthetic({delivery:"steer"})` | `PreToolUse` exits 2 with the mail on stderr: blocks one tool call, once per urgent message |
| Unanswered question at stop | `Stop` hook `decision: block` (respect `stop_hook_active` and the cap of 8 consecutive blocks) | `synthetic` after `session.execution.succeeded` | `Stop` hook exits 1 with the reason on stderr |
| **Wake idle agent** | async hook with `asyncRewake`: `agentmail hook wait` blocks until mail arrives, then exits 2 so Claude wakes | plugin `session.synthetic({resume:true, delivery:"queue"})` when mail arrives | `Stop` hook blocks until mail arrives, then exits 1 so the turn continues; **only if a spike shows the user can still type while it waits**, otherwise waking is off by default on kiro |

Items marked unverified in the research must be confirmed by a spike at the
start of their plan task, and the result recorded in the README support table:
the `asyncRewake` config schema and behaviour; kiro V3 global `~/.kiro/hooks/`;
kiro V3 Stop-hook behaviour while blocking (can the user type, any loop limit);
opencode `synthetic` steer/queue in a live run.

Hooks are silent for unregistered sessions and when the daemon is down. They
must not start the daemon.

Urgent mail is pushed mid-run **once**. The mailbox tracks a "surfaced" state
per recipient, separate from "read".

### Install (calls)
- `agentmail install --harness claude-code|opencode|kiro [--project] [--dry-run]`
  and `uninstall`. Each writes a `.agentmail.bak` backup before changing a file,
  touches only agentmail's own entries, and is idempotent.
- **Claude Code:** honour `CLAUDE_CONFIG_DIR`, writing `$CLAUDE_CONFIG_DIR/settings.json`
  (hooks) and `$CLAUDE_CONFIG_DIR/.claude.json` (user MCP). Without it, use `~/.claude/`
  and `~/.claude.json`. With `--project`: `.claude/settings.json` and `.mcp.json`.
  Edit the JSON directly. `claude` may be a shell function, so don't shell out to it.
- **opencode:** honour `XDG_CONFIG_HOME`. Merge `mcp.servers.agentmail`
  (`type: local`, absolute command, `codemode: false`) into `opencode.json`, and
  write the plugin to `plugins/agentmail.js` (embedded in the binary). If the
  config is a `.jsonc` file with comments, refuse to edit it and print the snippet
  instead. With `--project`: `./opencode.json` and `./.opencode/plugins/`.
- **kiro:** honour `KIRO_HOME`. Add the MCP server to the global `settings/mcp.json`.
  Hooks go in a standalone V3 hooks file (`hooks/agentmail.json`) if the spike
  confirms it works. Otherwise use an `agents/agentmail.json` agent, upgraded to
  the universal format in a temp directory, and change the default agent only
  with `--set-default`.
- `agentmail install --service` writes and enables the systemd user unit from
  the README. My call: setup pain is an anti-goal.

### Testing (calls)
- Hook output formats are unit-tested against recorded payload fixtures for each
  harness and event.
- Real-harness e2e tests are `#[ignore]`d, run by `scripts/e2e.sh <harness>`
  in isolated config homes (`claude -p` with `--settings`/`--mcp-config`,
  `opencode run --standalone` with private `XDG_*`, `kiro-cli chat --v3
  --no-interactive` with `KIRO_HOME`). Binary paths come from env vars
  (`AGENTMAIL_E2E_CLAUDE`, and so on), because `claude` is a shell function here.

## Left to the builder

Async runtime and crates; redb table layout; wire protocol details; CLI output
formatting; exact hook wiring per harness, within the behaviour above; macOS
support details (Linux/WSL is the primary target).
