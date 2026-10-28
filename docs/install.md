# Installing into a harness

[← README](../README.md)

```sh
agentmail install --harness claude-code|opencode|kiro [--project] [--dry-run] [--json]
                  [--kiro-wake]
agentmail uninstall --harness claude-code|opencode|kiro [--project] [--dry-run] [--json]
agentmail install|uninstall --service [--dry-run] [--json]
```

`--kiro-wake` is for kiro only (see the notes below); `--service`
installs (or removes) the daemon's systemd user unit instead of a harness
and takes none of `--harness`, `--project` or `--kiro-wake` (see
[Running the daemon under systemd](daemon.md#running-the-daemon-under-systemd)).
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

A file `install` creates (rather than edits) gets a marker next to it,
`<file>.agentmail.created`, listing the directories `install` created for
it. Later installs and uninstalls trust the marker: a file with one is
agentmail's, so `uninstall` deletes it (and those directories once empty)
and `install` never backs it up. Leave the markers alone while agentmail
is installed. If you delete such a file by hand, delete its marker too:
otherwise a file you later write at that path is taken for agentmail's,
and a later `uninstall` may delete it. `uninstall` removes a marker
together with the file it marks.

`--project` puts these markers and backups in your working tree beside the
config files, for example `.mcp.json.agentmail.created`,
`.claude/settings.json.agentmail.created`,
`opencode.json.agentmail.created`,
`.opencode/plugins/agentmail.js.agentmail.created`, the ones under
`.kiro/`, and `<file>.agentmail.bak` for each file you already had. They
show up as untracked files. Do not commit them. Add them to `.gitignore`
(or `.git/info/exclude` to keep it to your clone):

```gitignore
*.agentmail.created
*.agentmail.bak
```

| harness | user config (default) | `--project` |
|---|---|---|
| Claude Code | hooks in `settings.json`, MCP server in `.claude.json`, both under `$CLAUDE_CONFIG_DIR` when set, else `~/.claude/settings.json` and `~/.claude.json` | `.claude/settings.json`, `.mcp.json` (Claude Code asks you to approve the server the first time) |
| opencode ≥ 2.0 | `opencode.json` (MCP server, `codemode: false`) and `plugins/agentmail.js` in `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode` | `opencode.json`, `.opencode/plugins/agentmail.js` |
| kiro-cli (V3) | `settings/mcp.json` and `hooks/agentmail.json` in `$KIRO_HOME`, else `~/.kiro` | `.kiro/settings/mcp.json`, `.kiro/hooks/agentmail.json` |

Notes:

- **kiro-cli: V3 engine only.** Run agents with `kiro-cli chat --v3`. kiro's
  default (V2) engine is not supported; `install` says so. Waking an idle
  kiro agent is off unless you install with `--kiro-wake` (see
  [Harness support](../README.md#harness-support)). Install at one scope only: kiro
  loads the user's and a trusted workspace's hooks together, so both would
  run every hook twice. There is no agentmail agent to make kiro's
  default: kiro V3 loads agentmail's hooks for every agent.
- **opencode `codemode`.** agentmail's server is installed with
  `codemode: false`, so the agent calls its tools directly rather than
  from code passed to opencode's `execute` tool. opencode adds a server's
  `instructions` (how agents should use agentmail) to the agent's context
  either way: in code mode unless `execute` is denied, otherwise unless
  all of the server's tools are denied. opencode connects MCP servers
  only when a directory's first session starts, so that session's first
  turn runs without the instructions; they are added at its next turn.
  An `opencode.jsonc` with comments is never rewritten: install prints the
  entry to add by hand instead. opencode picks up the change without a
  restart.

To run the daemon as a systemd user service instead of on demand:
`agentmail install --service` (see
[Running the daemon under systemd](daemon.md#running-the-daemon-under-systemd)).

To register the MCP server by hand instead (no hooks, so no reminders,
urgent pushes or waking): `claude mcp add --scope user agentmail --
agentmail mcp`; in `opencode.json`, `"mcp": {"servers": {"agentmail":
{"type": "local", "command": ["agentmail", "mcp"], "codemode": false}}}`;
in kiro's `settings/mcp.json`, `"mcpServers": {"agentmail": {"command":
"agentmail", "args": ["mcp"]}}`.
