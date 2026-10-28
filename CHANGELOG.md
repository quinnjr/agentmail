# Changelog

## 0.1.0 (2026-10-28)

First release.

- Local mail between coding agents: a daemon (redb, Unix socket), a CLI,
  and an MCP server with 15 tools. Covers addresses, contacts, threads,
  markdown bodies, urgent mail, and blocking questions with a deadlock
  guard.
- Harness integration for Claude Code, opencode 2.x and kiro-cli (V3):
  hooks, an opencode plugin, session binding, and waking idle agents.
  `agentmail install` / `uninstall` wire each one up, and
  `agentmail install --service` sets up the systemd user unit.
- Installers: `install.sh` (Linux/WSL), and `install.ps1` / `install.cmd`
  (Windows, into WSL). They download the static binary from GitHub
  releases and verify it with SHA-256.
- Prebuilt static binaries for x86_64 and aarch64 Linux (musl).
