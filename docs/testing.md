# Testing

[← README](../README.md)

## Unit and integration tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
bun test plugin/        # the opencode plugin
```

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
  model by default (`AGENTMAIL_E2E_OPENCODE_MODEL`). Free models have a
  usage limit, so its OpenCode account login, when there is one, is copied
  in from opencode's data directory (`AGENTMAIL_E2E_OPENCODE_DATA`) for
  any other model.
- kiro-cli: on `PATH` or `AGENTMAIL_E2E_KIRO`, logged in
  (`AGENTMAIL_E2E_KIRO_DATA` for a non-default data directory). Only the
  runtimes kiro-cli never writes are linked from that directory; the login
  and feed files are copied, the rest left out.
- `jq`, `git`, `cargo` and `setsid` (util-linux) in every mode, plus
  `python3` whenever kiro or opencode runs, and `curl` for `opencode` mode.

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
