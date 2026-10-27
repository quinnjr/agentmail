#!/usr/bin/env bash
# Real-harness end-to-end test for agentmail.
#
#   scripts/e2e.sh <claude-code|opencode|kiro|mixed>
#
# Builds agentmail, creates a throwaway HOME (with private XDG_*,
# CLAUDE_CONFIG_DIR and KIRO_HOME inside it) and a private
# AGENTMAIL_DATA_DIR / AGENTMAIL_SOCKET, runs `agentmail install` into that
# home, then drives two real agents headlessly:
#
#   A ("backend") registers, waits for mail and answers questions;
#   B ("frontend") registers and asks backend what fields POST /orders takes.
#
# Steps (the failing one is printed, and the script exits non-zero):
#   1. A registers.
#   2. B registers and asks.
#   3. `agentmail --as human threads --json` shows a question/answer thread
#      between the two addresses.
#   4. Urgent mail from the human to A, sent while A is still working,
#      shows up in A's output or its own session's transcript.
#   5. kiro only: A's agent is recorded with harness `kiro`, bound to the
#      `kiro-cli` process the script started.
#   6. opencode A only (the one harness that wakes in a headless setup): A's
#      turn ends, the human mails it, and a new turn starts with no prompt:
#      A replies, or opencode's session log shows a turn the plugin's wake
#      started that saw the new mail (A runs on `opencode serve`, which
#      outlives the turn). Claude Code runs its wait hook only in the
#      interactive UI and kiro never waits in a `--no-interactive` run, so
#      their waking is not checked here (see docs/harness-notes.md).
#
# In `opencode` mode B runs in its own session on A's `opencode serve`, so
# both sessions share one agentmail MCP server. `mixed` runs A under Claude
# Code and B under opencode (`run --standalone`).
#
# Needs real model access, so `cargo test` never runs it. Inputs:
#
#   AGENTMAIL_E2E_CLAUDE    Claude Code binary (required for claude-code and
#                           mixed; `claude` is often a shell function). It
#                           is run through a symlink named `claude`.
#   AGENTMAIL_E2E_CLAUDE_CREDENTIALS
#                           Claude Code login to copy in (default
#                           $CLAUDE_CONFIG_DIR/.credentials.json, else
#                           ~/.claude/.credentials.json). Not needed when
#                           ANTHROPIC_API_KEY is set, which is passed through.
#   AGENTMAIL_E2E_CLAUDE_MODEL     default: haiku
#   AGENTMAIL_E2E_OPENCODE  opencode binary (default: `opencode` on PATH)
#   AGENTMAIL_E2E_OPENCODE_MODEL   default: opencode/big-pickle
#                           (free at the time of writing, no login; `opencode
#                           models --standalone` lists the current ones)
#   AGENTMAIL_E2E_KIRO      kiro-cli binary (default: `kiro-cli` on PATH)
#   AGENTMAIL_E2E_KIRO_DATA kiro-cli's data dir holding its login
#                           (default: ${XDG_DATA_HOME:-~/.local/share}/kiro-cli)
#   AGENTMAIL_E2E_TIMEOUT   seconds allowed per wait and agent run (default: 600)
#   AGENTMAIL_E2E_MIN_LOGIN_SECS
#                           how long a copied login must still be valid
#                           (default: 6 x AGENTMAIL_E2E_TIMEOUT + 300, the
#                           longest a run can take: six waits plus setup).
#                           Lower it, or the timeout, to run on a login
#                           that expires sooner.
#   AGENTMAIL_E2E_KEEP=1    keep the temporary directory (logs, transcripts)
#
# Logins are copied, never linked, and with their refresh token removed, so
# the copy cannot rotate (and so invalidate) the real login. Without that
# token the copy cannot outlive its access token, so the script refuses a
# login that expires within AGENTMAIL_E2E_MIN_LOGIN_SECS. Copies are deleted
# on exit, even with AGENTMAIL_E2E_KEEP=1. Of kiro-cli's data dir only the
# runtimes it never writes (bun, node, tui.js, the versioned engine under
# kas/) are linked in; its feed files are copied and the rest left out.
# Secrets (the API key, opencode's server password) reach the harnesses
# through their environment, never a command line. Each harness runs in a
# process group of its own (setsid), which is killed (TERM, then KILL) on a
# timeout and on exit, before the temporary directory is removed, so none
# of its descendants (MCP servers, hooks, kiro's engine) outlives the run.
# Nothing outside the temporary directory is written, apart from cargo's
# target directory.

set -uo pipefail

usage() {
    echo "usage: $0 <claude-code|opencode|kiro|mixed>" >&2
    exit 2
}

[ $# -eq 1 ] || usage
MODE=$1
case $MODE in
claude-code) HA=claude-code HB=claude-code ;;
opencode) HA=opencode HB=opencode ;;
kiro) HA=kiro HB=kiro ;;
mixed) HA=claude-code HB=opencode ;;
*) usage ;;
esac

STEP=setup
fail() {
    echo "e2e $MODE: FAILED at step $STEP: $*" >&2
    exit 1
}
note() { echo "e2e $MODE: [$STEP] $*" >&2; }

for tool in jq git cargo setsid; do
    command -v "$tool" >/dev/null || fail "$tool is required"
done

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TIMEOUT=${AGENTMAIL_E2E_TIMEOUT:-600}
[[ $TIMEOUT =~ ^[1-9][0-9]*$ ]] || fail "AGENTMAIL_E2E_TIMEOUT=$TIMEOUT is not a positive number of seconds"
# A run waits at most six times for TIMEOUT (A registers, B registers, the
# answered question, B finishes, A finishes, A is woken), plus setup.
MIN_LOGIN_SECS=${AGENTMAIL_E2E_MIN_LOGIN_SECS:-$((6 * TIMEOUT + 300))}
[[ $MIN_LOGIN_SECS =~ ^[0-9]+$ ]] ||
    fail "AGENTMAIL_E2E_MIN_LOGIN_SECS=$MIN_LOGIN_SECS is not a number of seconds"
MIN_LOGIN_MS=$((MIN_LOGIN_SECS * 1000))
MIN_LOGIN_TEXT="$((MIN_LOGIN_SECS / 60)) minutes ($MIN_LOGIN_SECS s, AGENTMAIL_E2E_MIN_LOGIN_SECS)"

needs() { [ "$HA" = "$1" ] || [ "$HB" = "$1" ]; }

# Resolve the harness binaries before anything is built or copied.
if needs claude-code; then
    CLAUDE_BIN=${AGENTMAIL_E2E_CLAUDE:-}
    [ -n "$CLAUDE_BIN" ] || fail "set AGENTMAIL_E2E_CLAUDE to the Claude Code binary"
    [ -x "$CLAUDE_BIN" ] || fail "AGENTMAIL_E2E_CLAUDE=$CLAUDE_BIN is not executable"
fi
if needs opencode; then
    OPENCODE_BIN=${AGENTMAIL_E2E_OPENCODE:-$(command -v opencode || true)}
    [ -n "$OPENCODE_BIN" ] && [ -x "$OPENCODE_BIN" ] ||
        fail "opencode not found; set AGENTMAIL_E2E_OPENCODE"
    OPENCODE_MODEL=${AGENTMAIL_E2E_OPENCODE_MODEL:-opencode/big-pickle}
    if [ "$HA" = opencode ]; then
        for tool in curl python3; do
            command -v "$tool" >/dev/null || fail "$tool is required"
        done
    fi
fi
if needs kiro; then
    KIRO_BIN=${AGENTMAIL_E2E_KIRO:-$(command -v kiro-cli || true)}
    [ -n "$KIRO_BIN" ] && [ -x "$KIRO_BIN" ] ||
        fail "kiro-cli not found; set AGENTMAIL_E2E_KIRO"
    command -v python3 >/dev/null || fail "python3 is required to copy kiro's login"
fi

STEP=build
(cd "$ROOT" && cargo build --release --quiet) || fail "cargo build failed"
BIN=$ROOT/target/release/agentmail

STEP=setup
TMP=$(mktemp -d "${TMPDIR:-/tmp}/agentmail-e2e.XXXXXX") || fail "mktemp failed"
SECRETS=()
# Each harness is started under setsid, so its pid is also the id of a
# process group holding it and every descendant that stays in the group.
PIDS=()

# kill_groups <pid...>: TERMs each process group, gives them 5 seconds to
# exit, then KILLs whatever is left.
kill_groups() {
    local p i alive
    for p in "$@"; do
        kill -TERM -- "-$p" 2>/dev/null
    done
    for i in $(seq 50); do
        alive=
        for p in "$@"; do
            kill -0 -- "-$p" 2>/dev/null && alive=1
        done
        [ -n "$alive" ] || return 0
        sleep 0.1
    done
    for p in "$@"; do
        kill -KILL -- "-$p" 2>/dev/null
    done
}

cleanup() {
    local p
    [ ${#PIDS[@]} -gt 0 ] && kill_groups "${PIDS[@]}"
    "$BIN" daemon stop >/dev/null 2>&1
    for p in "${SECRETS[@]}"; do
        rm -f "$p"
    done
    rm -rf "${SOCKDIR:-}"
    if [ "${AGENTMAIL_E2E_KEEP:-}" = 1 ]; then
        echo "e2e $MODE: kept $TMP" >&2
    else
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

H=$TMP/home
mkdir -p "$H/.config" "$H/.local/share" "$H/.local/state" "$H/.cache" \
    "$TMP/run" "$TMP/bin" "$TMP/data" "$TMP/logs"
chmod 700 "$TMP/run"
export AGENTMAIL_DATA_DIR=$TMP/data
# A unix socket path is limited to about 100 bytes, so the socket lives in
# a short directory of its own.
SOCKDIR=$(mktemp -d /tmp/am-e2e.XXXXXX) || fail "mktemp failed"
export AGENTMAIL_SOCKET=$SOCKDIR/agentmail.sock
ln -s "$BIN" "$TMP/bin/agentmail"

# The environment every harness (and install) runs with: nothing inherited
# but PATH, the locale and an API key, so no outer session, login or
# agentmail identity leaks in.
HENV=(
    "PATH=$TMP/bin:$PATH"
    "HOME=$H"
    "USER=${USER:-e2e}"
    "LANG=${LANG:-C.UTF-8}"
    "TERM=dumb"
    "XDG_CONFIG_HOME=$H/.config"
    "XDG_DATA_HOME=$H/.local/share"
    "XDG_STATE_HOME=$H/.local/state"
    "XDG_CACHE_HOME=$H/.cache"
    "XDG_RUNTIME_DIR=$TMP/run"
    "CLAUDE_CONFIG_DIR=$H/.claude"
    "KIRO_HOME=$H/.kiro"
    "AGENTMAIL_DATA_DIR=$AGENTMAIL_DATA_DIR"
    "AGENTMAIL_SOCKET=$AGENTMAIL_SOCKET"
)
# Secrets are kept in unexported shell variables and exported only inside
# henv's subshell, so they never appear on a command line (`env -i
# KEY=...` would put them in /proc/<pid>/cmdline for any local user).
E2E_API_KEY=${ANTHROPIC_API_KEY:-}
export -n ANTHROPIC_API_KEY 2>/dev/null

# henv [--opencode-password] <command...>: replaces the current shell (call
# it in a subshell) with the command, in the clean environment above plus
# the API key, and opencode's server password when asked for.
henv() {
    local pw= v
    if [ "${1:-}" = --opencode-password ]; then
        pw=1
        shift
    fi
    for v in $(compgen -e); do
        unset "$v" 2>/dev/null
    done
    for v in "${HENV[@]}"; do
        export "${v?}"
    done
    [ -n "$E2E_API_KEY" ] && export ANTHROPIC_API_KEY="$E2E_API_KEY"
    [ -n "$pw" ] && export OPENCODE_PASSWORD="$OPENCODE_PASSWORD"
    exec "$@"
}

# The human's CLI, against the private daemon.
human() { "$BIN" --as human "$@"; }

now_ms() { date +%s%3N; }

setup_claude() {
    ln -s "$CLAUDE_BIN" "$TMP/bin/claude"
    mkdir -p "$H/.claude"
    if [ -z "$E2E_API_KEY" ]; then
        local src=${AGENTMAIL_E2E_CLAUDE_CREDENTIALS:-${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json}
        [ -r "$src" ] || fail "no Claude Code login at $src (set AGENTMAIL_E2E_CLAUDE_CREDENTIALS or ANTHROPIC_API_KEY)"
        local exp
        exp=$(jq -r '.claudeAiOauth.expiresAt // 0' "$src") || fail "cannot read $src"
        [ "$exp" -gt $(($(now_ms) + MIN_LOGIN_MS)) ] ||
            fail "the Claude Code login in $src expires within $MIN_LOGIN_TEXT; refresh it (run claude once) and retry"
        local dst=$H/.claude/.credentials.json
        SECRETS+=("$dst")
        (umask 077 && jq 'del(.claudeAiOauth.refreshToken)' "$src" >"$dst") ||
            fail "cannot copy the Claude Code login"
    fi
    (henv "$BIN" install --harness claude-code) >"$TMP/logs/install-claude.log" 2>&1 ||
        fail "install --harness claude-code: $(cat "$TMP/logs/install-claude.log")"
}

setup_opencode() {
    (henv "$BIN" install --harness opencode) >"$TMP/logs/install-opencode.log" 2>&1 ||
        fail "install --harness opencode: $(cat "$TMP/logs/install-opencode.log")"
}

setup_kiro() {
    local src=${AGENTMAIL_E2E_KIRO_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/kiro-cli}
    [ -r "$src/data.sqlite3" ] || fail "no kiro-cli login in $src (set AGENTMAIL_E2E_KIRO_DATA)"
    local dst=$H/.local/share/kiro-cli
    mkdir -p "$dst"
    # Only what kiro-cli never writes is linked in: its runtimes and the
    # versioned engine directories under kas/ (the kas/ directory itself,
    # where it keeps lock files, is private). The small feed files are
    # copied; everything else (knowledge bases, run state, logs) is left
    # out, so nothing kiro-cli writes reaches the real data dir.
    local f
    for f in "$src"/*; do
        case $(basename "$f") in
        bun | node | tui.js | *.sha256) ln -s "$f" "$dst/" ;;
        feed*.json | feed-cache.*) cp -p "$f" "$dst/" || fail "cannot copy $f" ;;
        kas)
            mkdir -p "$dst/kas"
            local k
            for k in "$f"/*; do
                [ -d "$k" ] && ln -s "$k" "$dst/kas/"
            done
            ;;
        esac
    done
    SECRETS+=("$dst/data.sqlite3" "$dst/data.sqlite3-wal" "$dst/data.sqlite3-shm" "$dst/data.sqlite3-journal")
    (umask 077 && python3 -I - "$src/data.sqlite3" "$dst/data.sqlite3" "$MIN_LOGIN_MS" "$MIN_LOGIN_TEXT" <<'EOF'
import datetime, json, re, sqlite3, sys

def parse_utc(s):
    # RFC 3339 as kiro-cli writes it, with any number of fraction digits
    # (none, 3, 9, ...): fromisoformat wants 0, 3 or 6 before Python 3.11.
    m = re.fullmatch(
        r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)?", s.strip()
    )
    if m is None:
        raise ValueError(f"bad timestamp {s!r}")
    frac = (m[2] or "")[:6].ljust(6, "0")
    tz = "+00:00" if m[3] in (None, "Z") else m[3]
    return datetime.datetime.fromisoformat(f"{m[1]}.{frac}{tz}")

src, dst, min_ms, min_text = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
s = sqlite3.connect(f"file:{src}?mode=ro", uri=True)
d = sqlite3.connect(dst)
s.backup(d)
s.close()
key = "kirocli:odic:token"
row = d.execute("select value from auth_kv where key = ?", (key,)).fetchone()
if row is None:
    sys.exit("kiro-cli is not logged in")
tok = json.loads(row[0])
exp = parse_utc(tok["expires_at"])
left = (exp - datetime.datetime.now(datetime.timezone.utc)).total_seconds() * 1000
if left < min_ms:
    sys.exit(f"the kiro-cli login expires within {min_text}; refresh it (run kiro-cli once) and retry")
tok.pop("refresh_token", None)
d.execute("update auth_kv set value = ? where key = ?", (json.dumps(tok), key))
d.commit()
d.close()
EOF
    ) || fail "cannot copy the kiro-cli login"
    (henv "$BIN" install --harness kiro) >"$TMP/logs/install-kiro.log" 2>&1 ||
        fail "install --harness kiro: $(cat "$TMP/logs/install-kiro.log")"
}

needs claude-code && setup_claude
needs opencode && setup_opencode
needs kiro && setup_kiro

# The two agents work in one git repository, so plain names resolve.
REPO=$TMP/repo
mkdir -p "$REPO"
git -C "$REPO" init -q || fail "git init failed"

# opencode A runs on a server that outlives its turn, so it can be woken.
# `opencode serve` and `run --server` both take its password from
# OPENCODE_PASSWORD.
OPENCODE_URL=
OPENCODE_PASSWORD=e2e-$RANDOM$RANDOM$RANDOM
start_opencode_server() {
    local port
    port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])' 2>/dev/null ||
        echo $((20000 + RANDOM % 20000)))
    (cd "$REPO" && henv --opencode-password setsid \
        "$OPENCODE_BIN" serve --hostname 127.0.0.1 --port "$port" \
        >"$TMP/logs/opencode-serve.log" 2>&1) &
    PIDS+=($!)
    OPENCODE_URL=http://127.0.0.1:$port
    local i
    for i in $(seq 60); do
        curl -s -o /dev/null "$OPENCODE_URL" 2>/dev/null && return 0
        sleep 1
    done
    fail "opencode serve did not come up (see $TMP/logs/opencode-serve.log)"
}

# start_agent <A|B> <harness> <prompt>: runs the agent in the background;
# sets AGENT_PID (the harness process) and writes $TMP/logs/<A|B>.out.
start_agent() {
    local who=$1 harness=$2 prompt=$3 out=$TMP/logs/$1.out
    case $harness in
    claude-code)
        (cd "$REPO" && henv setsid claude -p --model "${AGENTMAIL_E2E_CLAUDE_MODEL:-haiku}" \
            --output-format json --allowedTools "mcp__agentmail,Bash" -- "$prompt" \
            >"$out" 2>"$out.err") &
        ;;
    opencode)
        # A, and B when A is opencode too, run on A's server (so both
        # sessions share its MCP server); B beside Claude Code A runs alone.
        if [ -n "$OPENCODE_URL" ]; then
            (cd "$REPO" && henv --opencode-password setsid \
                "$OPENCODE_BIN" run --server "$OPENCODE_URL" \
                --format json --auto -m "$OPENCODE_MODEL" -- "$prompt" >"$out" 2>"$out.err") &
        else
            (cd "$REPO" && henv setsid "$OPENCODE_BIN" run --standalone \
                --format json --auto -m "$OPENCODE_MODEL" -- "$prompt" >"$out" 2>"$out.err") &
        fi
        ;;
    kiro)
        (cd "$REPO" && henv setsid "$KIRO_BIN" chat --v3 --no-interactive --trust-all-tools \
            "$prompt" >"$out" 2>"$out.err") &
        ;;
    esac
    AGENT_PID=$!
    PIDS+=("$AGENT_PID")
}

# wait_until <seconds> <description> <command...>
wait_until() {
    local secs=$1 what=$2
    shift 2
    local end=$(($(date +%s) + secs))
    while [ "$(date +%s)" -lt "$end" ]; do
        "$@" && return 0
        sleep 1
    done
    fail "timed out after ${secs}s waiting for $what"
}

# finish <pid> <who>: waits for an agent run to end, killing it on timeout.
finish() {
    local pid=$1 who=$2 end=$(($(date +%s) + TIMEOUT))
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$end" ]; then
            kill_groups "$pid"
            fail "agent $who ran over ${TIMEOUT}s (output in $TMP/logs/$who.out)"
        fi
        sleep 1
    done
    wait "$pid" 2>/dev/null
}

address_of() {
    human agents --json --all 2>/dev/null |
        jq -r --arg n "$1" '[.[] | select(.name == $n)] | sort_by(.registered_at) | last | .address // empty'
}
# registered <name> <pid> <who>: the agent registered; fails the step at
# once if its harness exited first.
registered() {
    [ -n "$(address_of "$1")" ] && return 0
    kill -0 "$2" 2>/dev/null ||
        fail "agent $3 exited before registering as $1 (output in $TMP/logs/$3.out*)"
    return 1
}

# A thread between $ADDR_A and $ADDR_B that holds an answered question.
qa_thread() {
    local t
    for t in $(human threads --json 2>/dev/null | jq -r --arg a "$ADDR_A" --arg b "$ADDR_B" \
        '.[] | select((.participants | index($a)) and (.participants | index($b))) | .thread_id'); do
        if human show "$t" --json 2>/dev/null | jq -e '
            [.. | objects | select(has("kind")) | .kind] as $k
            | ($k | index("question")) and ($k | index("answer"))' >/dev/null; then
            QA_THREAD=$t
            return 0
        fi
    done
    return 1
}

# session_of <address>: the harness session agentmail recorded for it.
session_of() {
    human agents --json --all 2>/dev/null |
        jq -r --arg a "$1" '.[] | select(.address == $a) | .session.id // empty'
}

# mentions <who> <token> <harness> <address>: the agent's own output, or
# its own session's transcript, mentions the token. B's transcript sits in
# the same throwaway home, so the search is tied to the agent's session.
mentions() {
    local who=$1 token=$2 harness=$3 addr=$4 sess
    grep -q -- "$token" "$TMP/logs/$who.out" "$TMP/logs/$who.out.err" 2>/dev/null && return 0
    case $harness in
    claude-code)
        # `-p --output-format json` names the session; agentmail's record
        # of it is the fallback.
        sess=$(jq -r '.session_id // empty' "$TMP/logs/$who.out" 2>/dev/null)
        [ -n "$sess" ] || sess=$(session_of "$addr")
        [ -n "$sess" ] || return 1
        find "$H/.claude/projects" -name "$sess.jsonl" -exec grep -qs -- "$token" {} + 2>/dev/null
        ;;
    opencode)
        sess=$(session_of "$addr")
        [ -n "$sess" ] || return 1
        session_mentions_opencode "$sess" "$token"
        ;;
    kiro)
        # Only A is checked; its session is the one older than B's start.
        [ "$who" = A ] || return 1
        session_mentions_kiro "$B_STARTED" "$token"
        ;;
    *) return 1 ;;
    esac
}

# session_mentions_opencode <session id> <token>: a message or part of that
# session in opencode's database mentions the token.
session_mentions_opencode() {
    python3 -I - "$H/.local/share/opencode/opencode.db" "$1" "$2" <<'PY' 2>/dev/null
import sqlite3, sys
db, session, token = sys.argv[1:4]
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
for (data,) in c.execute("select data from session_message where session_id = ?", (session,)):
    if token in (data if isinstance(data, str) else bytes(data).decode("utf-8", "replace")):
        sys.exit(0)
sys.exit(1)
PY
}

# session_mentions_kiro <before, ms> <token>: a kiro session that began
# before the given time (A's: B starts only once A has registered) mentions
# the token. agentmail records no session for kiro agents, and kiro V3
# keeps each session in KIRO_HOME/sessions/<workspace>/sess_<id>/, its
# session.json giving createdAt.
session_mentions_kiro() {
    python3 -I - "$H/.kiro/sessions" "$1" "$2" <<'PY' 2>/dev/null
import calendar, glob, json, os, re, sys
root, before, token = sys.argv[1], int(sys.argv[2]), sys.argv[3]

def ms(s):
    # kiro writes UTC as 2026-10-27T23:11:56.492Z.
    m = re.fullmatch(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?Z", s)
    secs = calendar.timegm(tuple(map(int, re.split("[-T:]", m[1]))) + (0, 0, 0))
    return secs * 1000 + int((m[2] or "0")[:3].ljust(3, "0"))

for meta in glob.glob(os.path.join(root, "*", "sess_*", "session.json")):
    with open(meta) as f:
        if ms(json.load(f)["createdAt"]) >= before:
            continue
    for name in os.listdir(os.path.dirname(meta)):
        path = os.path.join(os.path.dirname(meta), name)
        if os.path.isfile(path):
            with open(path, errors="replace") as f:
                if token in f.read():
                    sys.exit(0)
sys.exit(1)
PY
}

replied_in() {
    human show "$1" --json 2>/dev/null |
        jq -e --arg a "$ADDR_A" '[.. | objects | select(.from? == $a)] | length > 0' >/dev/null
}

# opencode_woke <session id> <since, ms> <token>: opencode's own record
# (its database in the throwaway home) shows a turn the plugin started in
# that session after the given time, for the wake mail: a synthetic message
# (the plugin's wake, which names only an unread count) answered by the
# model in a turn that mentions the token (the mail's subject, as its inbox
# or read showed it, or the model's own words). A wake for older mail does
# not count unless that turn took in the new mail.
opencode_woke() {
    python3 -I - "$H/.local/share/opencode/opencode.db" "$1" "$2" "$3" <<'PY' 2>/dev/null
import sqlite3, sys
db, session, since, token = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
rows = c.execute(
    "select type, data from session_message where session_id = ? and time_created > ? order by seq",
    (session, since),
).fetchall()
woken = False  # inside a turn a synthetic message started
for kind, data in rows:
    if kind == "synthetic":
        woken = True
    elif kind == "user":
        woken = False
    elif kind == "assistant" and woken and token in data:
        sys.exit(0)
sys.exit(1)
PY
}

# The wake's evidence: a reply in the thread, else a turn opencode records.
WOKE_BY=
woken() {
    if replied_in "$WAKE_THREAD"; then
        WOKE_BY="replied in thread $WAKE_THREAD"
    elif opencode_woke "$SESSION_A" "$WAKE_SENT" "$WAKE_TOKEN"; then
        WOKE_BY="opencode ran a turn for the wake mail (the model did not reply)"
    else
        return 1
    fi
}

TOKEN=urgent-$RANDOM$RANDOM
WAKE_TOKEN=woken-$RANDOM$RANDOM

PROMPT_A="You are backend. Use your agentmail MCP tools (named like register, \
agentmail_register, mcp__agentmail__register or @agentmail/register, depending \
on the harness): register with the name backend, \
then coordinate with frontend and answer its questions. Call wait_for_mail \
(timeout_secs 120) until frontend asks you something, then answer it with: \
POST /orders takes items (an array of {sku, qty}) and amount_cents (an integer). \
After answering, run the shell command \`sleep 20\`, then the shell command \
\`echo finished\`. End with a one-line summary that quotes the subject of every \
agentmail message you saw, including urgent ones. Whenever you are told of \
new agentmail mail later, read it and do what it asks."

PROMPT_B="You are frontend. Use your agentmail MCP tools (named like register, \
agentmail_register, mcp__agentmail__register or @agentmail/register, depending \
on the harness): register with the name \
frontend, then ask backend (with the ask tool, to backend) what fields \
POST /orders takes, and wait for the answer. Report the answer in one line."

STEP="1 (A registers)"
[ "$HA" = opencode ] && start_opencode_server
start_agent A "$HA" "$PROMPT_A"
PID_A=$AGENT_PID
wait_until "$TIMEOUT" "backend to register (output in $TMP/logs/A.out*)" registered backend "$PID_A" A
ADDR_A=$(address_of backend)
note "backend is $ADDR_A"

STEP="2 (B registers and asks)"
B_STARTED=$(now_ms)
start_agent B "$HB" "$PROMPT_B"
PID_B=$AGENT_PID
wait_until "$TIMEOUT" "frontend to register (output in $TMP/logs/B.out*)" registered frontend "$PID_B" B
ADDR_B=$(address_of frontend)
note "frontend is $ADDR_B"

STEP="3 (question/answer thread)"
wait_until "$TIMEOUT" "an answered question between $ADDR_B and $ADDR_A" qa_thread
note "thread $QA_THREAD"

STEP="4 (urgent mail reaches A)"
human send --to "$ADDR_A" --urgent --subject "$TOKEN" \
    --body "Urgent from the human: put the word $TOKEN in your final summary." >/dev/null ||
    fail "sending urgent mail failed"
finish "$PID_B" B
finish "$PID_A" A
mentions A "$TOKEN" "$HA" "$ADDR_A" || fail "A's output and its session's transcript never mention $TOKEN (see $TMP/logs/A.out)"
note "A saw the urgent mail"

if [ "$HA" = kiro ]; then
    STEP="5 (kiro binding)"
    human agents --json --all | jq -e --arg a "$ADDR_A" --argjson p "$PID_A" \
        '.[] | select(.address == $a) | .harness == "kiro" and .proc_id.pid == $p' >/dev/null ||
        fail "$ADDR_A is not harness kiro bound to kiro-cli pid $PID_A: $(human agents --json --all | jq -c --arg a "$ADDR_A" '.[] | select(.address == $a) | {harness, proc_id}')"
    note "bound to kiro-cli pid $PID_A"
fi

if [ "$HA" = opencode ]; then
    STEP="6 (idle A is woken)"
    SESSION_A=$(session_of "$ADDR_A")
    [ -n "$SESSION_A" ] || fail "$ADDR_A has no opencode session"
    # Let a wake for mail A left unread (it may not read every message)
    # run its course first.
    sleep 10
    WAKE_SENT=$(now_ms)
    WAKE_THREAD=$(human send --to "$ADDR_A" --subject "ping $WAKE_TOKEN" \
        --body "Reply to this message (agentmail reply) with the body: pong $WAKE_TOKEN" --json |
        jq -r '.thread_id // .id // empty')
    [ -n "$WAKE_THREAD" ] || fail "sending the wake mail failed"
    wait_until "$TIMEOUT" "idle A to be woken for thread $WAKE_THREAD" woken
    note "A was woken: $WOKE_BY"
fi

STEP=done
echo "e2e $MODE: PASSED"
