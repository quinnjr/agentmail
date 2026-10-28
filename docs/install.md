# Installing

[← README](../README.md)

## Installing the binary

```sh
curl -fsSL https://raw.githubusercontent.com/quinnjr/agentmail/main/install.sh | sh -s -- [options]
```

`install.sh` runs on Linux and WSL. It finds the latest release (or the
one given with `--version`) and downloads that release's archive for the
machine (`x86_64` or `aarch64`, statically linked with musl) from GitHub.
It checks the archive against the release's `.sha256` file and installs
`agentmail` into `~/.local/bin`. It never installs an archive whose
checksum is missing or wrong, or a binary that does not run. It replaces
an existing binary by renaming the new one over it, so a running daemon
is never left with a half-written file. Afterwards it stops the running
daemon, so the next command starts the new version (under systemd, the
unit restarts it). It also warns if another `agentmail`, such as one from
`cargo install`, comes earlier on your `PATH`.

On a CPU with no prebuilt binary, or with `--from-source`, it builds that
release with `cargo install --tag`. If no release has been published yet,
`--from-source` builds the development branch instead. A failed download
stops the install. It never falls back to building from source. Finding
the latest release needs curl or GNU wget. Where it cannot be found (for
example with busybox wget), `--from-source` asks for `--version <tag>`.

| option | env | effect |
|---|---|---|
| `--version <tag>` | `AGENTMAIL_VERSION` | Release to install, e.g. `v0.1.0` (default: latest). |
| `--bin-dir <dir>` | `AGENTMAIL_INSTALL_DIR` | Where to put the binary (default: `~/.local/bin`). |
| `--harness <name>` | | Afterwards run `agentmail install --harness <name>`. Takes `claude-code`, `opencode` or `kiro`, and can be repeated. `auto` picks every harness whose CLI (`claude`, `opencode`, `kiro-cli`) is on `PATH`, `~/.local/bin` or `~/.cargo/bin` (not Windows tools seen from WSL under `/mnt`). |
| `--service` | | Afterwards run `agentmail install --service`. |
| `--from-source` | | Build with cargo instead of downloading. |
| | `AGENTMAIL_DOWNLOAD_URL` | Download from a mirror instead of `https://github.com/quinnjr/agentmail/releases`. |

### Windows

agentmail needs Linux, so on Windows it runs inside WSL, together with the
agents that use it. Run Claude Code, opencode or kiro-cli inside WSL too.
The Windows scripts check that WSL and a distribution are available, then
run `install.sh` in your default distribution and report its exit code:

```powershell
irm https://raw.githubusercontent.com/quinnjr/agentmail/main/install.ps1 | iex

# with options
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/quinnjr/agentmail/main/install.ps1))) -Harness auto -Distro Ubuntu
```

`install.ps1` takes `-Version`, `-BinDir`, `-Harness`, `-Service`,
`-FromSource` and `-Distro`. `install.cmd` passes its arguments straight
on to `install.sh`, and uses `AGENTMAIL_WSL_DISTRO` to pick the
distribution:

```bat
curl -fsSLo "%TEMP%\agentmail-install.cmd" https://raw.githubusercontent.com/quinnjr/agentmail/main/install.cmd && "%TEMP%\agentmail-install.cmd" --harness auto
```

## Wiring agentmail into a harness

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
