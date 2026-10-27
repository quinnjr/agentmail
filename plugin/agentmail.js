// agentmail for opencode (V2 plugin API: `export default { id, setup }`).
//
// Installed by `agentmail install --harness opencode`, which writes this
// file with the agentmail binary's absolute path in place of the
// placeholder below. It keeps an opencode agent in touch with its mail:
//
// - After each tool call: urgent mail is pushed into the running turn once
//   (`session.synthetic`, delivery "steer"; appended to the tool's result
//   when it cannot be steered), else a one-line reminder of unread mail is
//   appended to the tool's result.
// - When a turn ends with questions to the agent still open, they are
//   queued as the next turn (once; not again after that turn).
// - While a session sits idle, a waiter runs `agentmail hook wait` and,
//   when mail comes, queues a reminder that starts a turn (`resume`).
// - When opencode deletes a session, its agent signs off (`agentmail hook
//   signoff`: only the session's own agent, and never starting the daemon).
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

/** How many other instances' sessions one instance remembers as theirs. */
const FOREIGN_MAX = 256;

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
 * `continued`: the turn that just ended was one this plugin started.
 * `known`: the `known` ids of the timed-out wait before this one, if any,
 * so mail that came between the two counts. The result is agentmail's
 * answer (`{mail}`, `{busy}`, `{skip}`, `{gone}`), or null when it failed
 * or was killed. An answer printed before the kill took effect is still
 * returned: agentmail has recorded a mail answer as a wake, so the caller
 * must not lose it. Bun's `$` cannot be killed, hence `Bun.spawn`.
 */
async function waitOnce(sessionID, continued, known, signal) {
  if (signal.aborted) return null;
  const payload = { stop_hook_active: continued };
  if (known) payload.known = known;
  let proc;
  try {
    proc = Bun.spawn(
      [bin(), "hook", "wait", "--harness", "opencode", "--session", sessionID, "--timeout", String(WAIT_SECS)],
      {
        stdin: new TextEncoder().encode(JSON.stringify(payload)),
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
    if (code !== 0) return null;
    return parse(text);
  } catch {
    return null;
  } finally {
    signal.removeEventListener("abort", kill);
  }
}

/**
 * `agentmail hook signoff` for `sessionID`: signs off the agent registered
 * for that session, and no other. Never starts the daemon. Best effort.
 */
async function signoff(sessionID) {
  try {
    await Bun.$`${bin()} hook signoff --harness opencode --session ${sessionID}`.env(childEnv()).nothrow().quiet();
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
  /** Per-session state, by session id; only for sessions not known to be another instance's. */
  const sessions = new Map();
  /** Sessions known to be this instance's. */
  const mine = new Set();
  /** Sessions known to be another instance's, least recently seen first. */
  const foreign = new Map();
  /** `ctx.session.get` lookups under way, by session id. */
  const lookups = new Map();
  const stopped = new AbortController();
  const disposers = [];

  function state(sessionID) {
    let s = sessions.get(sessionID);
    if (!s) {
      // idle: no turn is running. gen: counts turn starts and ends, so a
      // handler that awaited can tell whether the session moved on.
      // continued: the running (or last) turn was started by this plugin.
      // waiter: the idle waiter's controller. missed: a wake was dropped
      // because the session was busy, and the agent has not heard of its
      // mail since.
      s = { idle: false, gen: 0, continued: false, waiter: null, missed: false };
      sessions.set(sessionID, s);
    }
    return s;
  }

  function isForeign(sessionID) {
    if (!foreign.has(sessionID)) return false;
    // Seen again: now the most recent.
    foreign.delete(sessionID);
    foreign.set(sessionID, true);
    return true;
  }

  function rememberForeign(sessionID) {
    foreign.delete(sessionID);
    foreign.set(sessionID, true);
    while (foreign.size > FOREIGN_MAX) foreign.delete(foreign.keys().next().value);
    const s = sessions.get(sessionID);
    if (s) {
      stopWaiter(s);
      sessions.delete(sessionID);
    }
  }

  // opencode loads one instance of this plugin per directory it serves,
  // and every instance sees every session's events. A session is this
  // instance's when it lives in this instance's directory. Answers are
  // kept (another instance's sessions in a bounded list); a failed lookup
  // is not, so the next event asks again.
  function owns(sessionID) {
    if (directory === undefined || mine.has(sessionID)) return Promise.resolve(true);
    if (isForeign(sessionID)) return Promise.resolve(false);
    let p = lookups.get(sessionID);
    if (!p) {
      p = Promise.resolve()
        .then(() => ctx.session.get({ sessionID }))
        .then(
          (info) => {
            const where = info?.location?.directory;
            if (typeof where !== "string") return false;
            if (where === directory) {
              mine.add(sessionID);
              return true;
            }
            rememberForeign(sessionID);
            return false;
          },
          () => false,
        )
        .finally(() => lookups.delete(sessionID));
      lookups.set(sessionID, p);
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
    // The timed-out wait's `known` ids, for the next wait.
    let known = null;
    try {
      while (!signal.aborted) {
        const started = Date.now();
        const r = await waitOnce(sessionID, continued, known, signal);
        if (!r) break;
        if (r.mail === true) {
          // agentmail recorded this mail as woken: it is this plugin's to
          // deliver now, or when the session is next idle.
          if (stopped.signal.aborted || sessions.get(sessionID) !== s) break;
          const text = typeof r.text === "string" && r.text ? r.text : null;
          if (s.idle && !signal.aborted && text) {
            s.continued = true;
            if (await synthetic(sessionID, text, "queue", true)) break;
            s.continued = false;
          }
          s.missed = true;
          break;
        }
        if (signal.aborted || r.mail !== false) break;
        known = Array.isArray(r.known) ? r.known.filter((id) => typeof id === "string") : null;
        const gap = MIN_WAIT_GAP_MS - (Date.now() - started);
        if (gap > 0) await sleep(gap, signal);
      }
    } finally {
      if (s.waiter === ac) s.waiter = null;
    }
  }

  // A turn ended. The transition is recorded at once, before any await,
  // so a turn that starts while this looks things up wins.
  function turnEnded(sessionID, succeeded) {
    if (isForeign(sessionID)) return;
    const s = state(sessionID);
    s.idle = true;
    const gen = ++s.gen;
    const continued = s.continued;
    s.continued = false;
    return afterTurn(sessionID, s, gen, continued, succeeded);
  }

  async function afterTurn(sessionID, s, gen, continued, succeeded) {
    // Still idle since this turn ended, and not deleted or unloaded.
    const current = () => sessions.get(sessionID) === s && s.gen === gen && s.idle && !stopped.signal.aborted;
    if (!(await owns(sessionID)) || !current()) return;
    const st = await status(sessionID, false);
    if (!st || !current()) return;
    // Open questions are queued once: not after the turn they started.
    let questionsFailed = false;
    const questions = st.text?.questions;
    if (succeeded && !continued && typeof questions === "string" && questions) {
      s.continued = true;
      if (await synthetic(sessionID, questions, "queue", true)) return;
      s.continued = false;
      questionsFailed = true;
      if (!current()) return;
    }
    // A wake dropped while the session was busy, whose mail the turn
    // never heard of (no tool call reminded it): deliver it now.
    const reminder = st.text?.reminder;
    if (s.missed && st.wake === true && st.unread > 0 && typeof reminder === "string" && reminder) {
      s.continued = true;
      if (await synthetic(sessionID, reminder, "queue", true)) {
        s.missed = false;
        return;
      }
      s.continued = false;
      if (!current()) return;
    }
    // After a turn this plugin started, or one that did not succeed, only
    // new mail wakes the session; so too when the open questions could not
    // be queued (they would otherwise hold the wait as a continuation).
    if (st.wake === true) runWaiter(sessionID, continued || !succeeded || questionsFailed);
  }

  // A turn started: record it at once, then forget the session if it is
  // another instance's.
  async function turnStarted(sessionID) {
    if (isForeign(sessionID)) return;
    const s = state(sessionID);
    s.idle = false;
    s.gen++;
    stopWaiter(s);
    await owns(sessionID);
  }

  async function deleted(sessionID) {
    const s = sessions.get(sessionID);
    if (s) stopWaiter(s);
    sessions.delete(sessionID);
    // A session another instance owns is that instance's to sign off. An
    // unknown one (deleted before this instance saw it, or whose lookup
    // failed) is signed off by every instance: signing off twice is
    // harmless, and `hook signoff` acts only on the session's own agent.
    const pending = lookups.get(sessionID);
    if (pending) await pending;
    const ours = !foreign.has(sessionID);
    mine.delete(sessionID);
    foreign.delete(sessionID);
    if (ours) await signoff(sessionID);
  }

  async function onEvent(e) {
    const sessionID = e?.data?.sessionID;
    if (typeof sessionID !== "string" || !sessionID) return;
    switch (e.type) {
      case "session.execution.started":
        return turnStarted(sessionID);
      case "session.inbox.enqueued": {
        // A human prompt: the next turn is not this plugin's.
        const s = sessions.get(sessionID);
        if (s && e.data.item?.type === "user") s.continued = false;
        return;
      }
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
  // reminder on the tool's result. Urgent mail that cannot be steered (the
  // turn ended meanwhile, or `synthetic` failed) is appended to the result
  // instead: agentmail has marked it surfaced, so it is not pushed again.
  disposers.push(
    await ctx.tool.hook("execute.after", async (ev) => {
      const sessionID = ev?.sessionID;
      if (typeof sessionID !== "string" || !sessionID) return;
      const st = await status(sessionID, true);
      if (!st) return;
      const urgent = st.text?.urgent;
      const hasUrgent = typeof urgent === "string" && urgent;
      if (hasUrgent && (await synthetic(sessionID, urgent, "steer", false))) return;
      if (ev.status !== "completed" || !ev.result || typeof ev.result !== "object") return;
      const reminder = st.text?.reminder;
      const hasReminder = st.unread > 0 && typeof reminder === "string" && reminder;
      if (!hasUrgent && !hasReminder) return;
      if (!Array.isArray(ev.result.content)) ev.result.content = [];
      if (hasUrgent) ev.result.content.push({ type: "text", text: urgent });
      if (hasReminder) {
        ev.result.content.push({ type: "text", text: reminder });
        // The turn has heard of its mail: a dropped wake is delivered.
        const s = sessions.get(sessionID);
        if (s) s.missed = false;
      }
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
