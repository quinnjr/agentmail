// agentmail for opencode (V2 plugin API: `export default { id, setup }`).
//
// Installed by `agentmail install --harness opencode`, which writes this
// file with the agentmail binary's absolute path in place of the
// placeholder below. It keeps an opencode agent in touch with its mail:
//
// - After each tool call: urgent mail is pushed into the running turn once
//   (`session.synthetic`, delivery "steer"), else a one-line reminder of
//   unread mail is appended to the tool's result.
// - When a turn ends with questions to the agent still open, they are
//   queued as the next turn (once; not again after that turn).
// - While a session sits idle, a waiter runs `agentmail hook wait` and,
//   when mail comes, queues a reminder that starts a turn (`resume`).
// - When opencode deletes a session, its agent signs off.
//
// The agent of a session is the one registered for it (by agentmail's MCP
// server, from the `_meta` session id). Sessions without one, and a
// stopped daemon, get nothing. Every agentmail call is a short-lived
// `agentmail hook ...` process; any failure counts as "nothing to say".
// No dependencies: Bun's `$` and `Bun.spawn` only.
//
// See docs/harness-notes.md, "opencode plugin".

const BIN = "__AGENTMAIL_BIN__";
// Split so that install's substitution leaves this one alone.
const PLACEHOLDER = "__AGENTMAIL" + "_BIN__";

/** How long one `hook wait` runs before reporting no mail, in seconds. */
const WAIT_SECS = 300;

/** The shortest time between two `hook wait` runs of one waiter, in ms. */
const MIN_WAIT_GAP_MS = 1000;

/** The agentmail binary: the installed path, else `agentmail` on PATH. */
function bin() {
  return BIN === PLACEHOLDER ? "agentmail" : BIN;
}

/**
 * The environment for agentmail commands: opencode's, without
 * `AGENTMAIL_ADDRESS`. opencode's process may have inherited one from the
 * shell it was started in; it names no session of this opencode, and the
 * CLI would act as that address instead of the session.
 */
function childEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  delete env.AGENTMAIL_ADDRESS;
  return env;
}

/** Parse one JSON object printed by agentmail; null if there is none. */
function parse(text) {
  try {
    const v = JSON.parse(String(text).trim());
    return v && typeof v === "object" && !Array.isArray(v) ? v : null;
  } catch {
    return null;
  }
}

/**
 * `agentmail hook status` for `sessionID`: the agent's status with the
 * words to show under `text`, or null when the session has no agent, the
 * daemon is down, or anything fails.
 */
async function status(sessionID, surfaceUrgent) {
  const flags = surfaceUrgent ? ["--surface-urgent"] : [];
  try {
    const out = await Bun.$`${bin()} hook status --harness opencode --session ${sessionID} ${flags}`
      .env(childEnv())
      .nothrow()
      .quiet();
    if (out.exitCode !== 0) return null;
    const v = parse(out.stdout.toString());
    return v && typeof v.address === "string" ? v : null;
  } catch {
    return null;
  }
}

/**
 * One `agentmail hook wait` for `sessionID`, killed when `signal` aborts.
 * `continued`: the turn that just ended was one this plugin started. The
 * result is agentmail's answer (`{mail}`, `{busy}`, `{skip}`, `{gone}`),
 * or null when it failed or was aborted. Bun's `$` cannot be killed,
 * hence `Bun.spawn`.
 */
async function waitOnce(sessionID, continued, signal) {
  if (signal.aborted) return null;
  let proc;
  try {
    proc = Bun.spawn(
      [bin(), "hook", "wait", "--harness", "opencode", "--session", sessionID, "--timeout", String(WAIT_SECS)],
      {
        stdin: new TextEncoder().encode(JSON.stringify({ stop_hook_active: continued })),
        stdout: "pipe",
        stderr: "ignore",
        env: childEnv(),
      },
    );
  } catch {
    return null;
  }
  const kill = () => {
    try {
      proc.kill();
    } catch {}
  };
  signal.addEventListener("abort", kill, { once: true });
  try {
    const [text, code] = await Promise.all([new Response(proc.stdout).text(), proc.exited]);
    if (signal.aborted || code !== 0) return null;
    return parse(text);
  } catch {
    return null;
  } finally {
    signal.removeEventListener("abort", kill);
  }
}

/** `agentmail signoff` as the agent of `sessionID`. Best effort. */
async function signoff(sessionID) {
  try {
    await Bun.$`${bin()} signoff`.env(childEnv({ OPENCODE_SESSION_ID: sessionID })).nothrow().quiet();
  } catch {}
}

/** Resolve after `ms`, or as soon as `signal` aborts. */
function sleep(ms, signal) {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const done = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    signal.addEventListener("abort", done, { once: true });
  });
}

async function setup(ctx) {
  const directory = ctx.location?.directory;
  /** Per-session state, by session id. */
  const sessions = new Map();
  /** Whether a session belongs to this plugin instance, by session id. */
  const owned = new Map();
  const stopped = new AbortController();
  const disposers = [];

  function state(sessionID) {
    let s = sessions.get(sessionID);
    if (!s) {
      // idle: no turn is running. continued: the running (or last) turn
      // was started by this plugin. waiter: the idle waiter's controller.
      s = { idle: false, continued: false, waiter: null };
      sessions.set(sessionID, s);
    }
    return s;
  }

  // opencode loads one instance of this plugin per directory it serves,
  // and every instance sees every session's events. A session is this
  // instance's when it lives in this instance's directory.
  function owns(sessionID) {
    if (directory === undefined) return Promise.resolve(true);
    let p = owned.get(sessionID);
    if (!p) {
      p = Promise.resolve()
        .then(() => ctx.session.get({ sessionID }))
        .then((info) => info?.location?.directory === directory)
        .catch(() => false);
      owned.set(sessionID, p);
    }
    return p;
  }

  async function synthetic(sessionID, text, delivery, resume) {
    try {
      await ctx.session.synthetic({ sessionID, text, delivery, resume });
      return true;
    } catch {
      return false;
    }
  }

  function stopWaiter(s) {
    if (s.waiter) {
      s.waiter.abort();
      s.waiter = null;
    }
  }

  // Wait for mail while the session is idle; when it comes, queue the
  // reminder as a new turn. One waiter per session. It ends when the
  // session gets busy (stopWaiter), and on anything but a timeout
  // (`{"mail":false}`); the session's next idle period starts a new one.
  async function runWaiter(sessionID, continued) {
    const s = state(sessionID);
    if (s.waiter || stopped.signal.aborted) return;
    const ac = new AbortController();
    s.waiter = ac;
    const signal = AbortSignal.any([ac.signal, stopped.signal]);
    try {
      while (!signal.aborted) {
        const started = Date.now();
        const r = await waitOnce(sessionID, continued, signal);
        if (signal.aborted || !r) break;
        if (r.mail === true) {
          // A turn that started meanwhile hears of it after its tool calls.
          if (s.idle && typeof r.text === "string" && r.text) {
            s.continued = true;
            if (!(await synthetic(sessionID, r.text, "queue", true))) s.continued = false;
          }
          break;
        }
        if (r.mail !== false) break;
        const gap = MIN_WAIT_GAP_MS - (Date.now() - started);
        if (gap > 0) await sleep(gap, signal);
      }
    } finally {
      if (s.waiter === ac) s.waiter = null;
    }
  }

  async function turnEnded(sessionID, succeeded) {
    if (!(await owns(sessionID))) return;
    const s = state(sessionID);
    s.idle = true;
    const continued = s.continued;
    s.continued = false;
    const st = await status(sessionID, false);
    if (!st || !s.idle) return;
    // Open questions are queued once: not after the turn they started.
    const questions = st.text?.questions;
    if (succeeded && !continued && typeof questions === "string" && questions) {
      s.continued = true;
      if (await synthetic(sessionID, questions, "queue", true)) return;
      s.continued = false;
    }
    // After a turn this plugin started, or one that did not succeed, only
    // new mail wakes the session.
    if (st.wake === true) runWaiter(sessionID, continued || !succeeded);
  }

  async function deleted(sessionID) {
    const s = sessions.get(sessionID);
    if (s) stopWaiter(s);
    sessions.delete(sessionID);
    // A session another instance owns is that instance's to sign off. An
    // unknown one (deleted before this instance saw it) is signed off by
    // every instance: signing off twice is harmless.
    const mine = owned.has(sessionID) ? await owned.get(sessionID) : true;
    owned.delete(sessionID);
    if (mine) await signoff(sessionID);
  }

  async function onEvent(e) {
    const sessionID = e?.data?.sessionID;
    if (typeof sessionID !== "string" || !sessionID) return;
    switch (e.type) {
      case "session.execution.started": {
        const s = state(sessionID);
        s.idle = false;
        stopWaiter(s);
        return;
      }
      case "session.inbox.enqueued":
        // A human prompt: the next turn is not this plugin's.
        if (e.data.item?.type === "user") state(sessionID).continued = false;
        return;
      case "session.execution.succeeded":
        return turnEnded(sessionID, true);
      case "session.execution.failed":
      case "session.execution.interrupted":
        return turnEnded(sessionID, false);
      case "session.deleted":
        return deleted(sessionID);
    }
  }

  // After each tool call: urgent mail into the running turn, else a
  // reminder on the tool's result.
  disposers.push(
    await ctx.tool.hook("execute.after", async (ev) => {
      const sessionID = ev?.sessionID;
      if (typeof sessionID !== "string" || !sessionID) return;
      const st = await status(sessionID, true);
      if (!st) return;
      const urgent = st.text?.urgent;
      if (typeof urgent === "string" && urgent) {
        await synthetic(sessionID, urgent, "steer", false);
        return;
      }
      const reminder = st.text?.reminder;
      if (ev.status !== "completed" || st.unread <= 0) return;
      if (typeof reminder !== "string" || !reminder) return;
      if (!ev.result || typeof ev.result !== "object") return;
      if (!Array.isArray(ev.result.content)) ev.result.content = [];
      ev.result.content.push({ type: "text", text: reminder });
    }),
  );

  (async () => {
    for await (const e of ctx.event.subscribe({ signal: stopped.signal })) {
      // Handlers run side by side, so a slow one never holds up the rest.
      onEvent(e).catch(() => {});
    }
  })().catch(() => {});

  return async () => {
    stopped.abort();
    for (const s of sessions.values()) stopWaiter(s);
    for (const d of disposers) {
      try {
        await d?.dispose?.();
      } catch {}
    }
  };
}

export default { id: "agentmail", setup };
