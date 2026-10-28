# Operating the daemon

[← README](../README.md)

One binary, three roles: `agentmail daemon run` is the only process that opens the
database; every other command starts it on demand, so you never have to.

`agentmail daemon status|start|stop|run`. `status` exits 1 when the daemon is not
running and 3 when it speaks another protocol version (stop it and the next
command starts the new one).

## Files

| what | where |
|---|---|
| database | `$XDG_DATA_HOME/agentmail/agentmail.redb` (default `~/.local/share/agentmail/`) |
| socket | `$XDG_RUNTIME_DIR/agentmail.sock`, else `$XDG_DATA_HOME/agentmail/agentmail.sock` (mode 0600) |
| daemon lock | `agentmail.lock` in the data directory: held by the running daemon, and records whether the last daemon was supervised (`--wait-for-lock`), its pid and its socket |
| waiter lock | `agentmail.waiter` in the data directory: held by a `--wait-for-lock` daemon while it waits for the daemon lock, with the same record |
| start-up error | `daemon.err` in the data directory: why the last spawned daemon failed to start, for the CLI to report; removed once a daemon starts |
| hook state | `hook-wait/` in the data directory: per-agent files of the wake and Stop hooks, named `<address>.<kind>`: `lock` (the waiter lock), `turn` (the newest wait's claim on the agent's turn), `woken` (the unread mail ids of the last wake), `busy` (when the session was last marked busy), `stop` (the Stop hook's last decision) and `shown` (the open questions kiro's Stop hook has shown). An agent's files are removed when it signs off or is gone |

`AGENTMAIL_DATA_DIR` (the data directory) and `AGENTMAIL_SOCKET` override
these, e.g. to run an isolated instance for tests.

## Running the daemon under systemd

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
  this page) is saved as `agentmail.service.agentmail.bak` first, and a
  unit agentmail wrote that you have since edited is not replaced at all.
- Marks a unit it created with `agentmail.service.agentmail.created` next
  to it (see
  [Installing into a harness](install.md)). Later installs
  and uninstalls trust it: with the marker there, the unit is agentmail's
  to replace or remove.
- Notes when your shell sets `AGENTMAIL_DATA_DIR` or `AGENTMAIL_SOCKET`
  differently from the service, with the lines to give the service the same
  (see the drop-in below).

Put your own settings for the service in a drop-in
(`systemctl --user edit agentmail`), not in `agentmail.service`: install and
uninstall never touch drop-ins.

`agentmail uninstall --service` disables and stops the service and removes
the unit and its marker. If you already removed the unit by hand, it still
cleans up what systemd has left (the running service and the `enable`
link). It leaves drop-ins and a saved `.agentmail.bak` unit in place, and
says how to restore the latter.

If you remove the unit by hand, also remove its marker:

```sh
rm -f ~/.config/systemd/user/agentmail.service.agentmail.created
```

Otherwise a unit you later write there is taken for agentmail's: a later
install may replace it, and uninstall may remove it.

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

## Upgrading the database

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
