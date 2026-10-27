// agentmail for opencode (V2 plugin API: `export default { id, setup }`).
//
// Installed by `agentmail install --harness opencode`, which writes this
// file with the agentmail binary's absolute path in place of the
// placeholder below. It keeps an opencode agent in touch with its mail:
//
// - After each tool call: urgent mail is pushed into the running turn once
//   (`session.synthetic`, delivery "steer"; appended to the tool's result
//   when it cannot be steered, or kept for the next tool call or the turn's
//   end when the tool failed), else a one-line reminder of unread mail is
//   appended to the tool's result.
// - When a turn ends with questions to the agent still open, they are
//   queued as the next turn (once; not again after that turn).
// - While a session sits idle, a waiter runs `agentmail hook wait` and,
//   when mail comes, queues a reminder that starts a turn (`resume`); a
//   reminder that cannot be queued is tried again, and the waiter waits on.
// - When opencode deletes a session, its agent signs off (`agentmail hook
//   signoff`: only the session's own agent, and never starting the daemon).
//
// The agent of a session is the one registered for it (by agentmail's MCP
// server, from the `_meta` session id). Sessions without one, and a
// stopped daemon, get nothing. Every agentmail call is a short-lived
// `agentmail hook ...` process; any failure counts as "nothing to say".
// No dependencies: `Bun.spawn` only.
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

/**
 * How many of its own sessions one instance keeps state for. Past this,
 * the least recently seen that need nothing more (idle, no waiter, nothing
 * left to deliver) are forgotten, and looked up again when next seen.
 */
const SESSIONS_MAX = 256;

/** The first wait after a failed delivery, in seconds; doubled per failure. */
const RETRY_SECS = 5;

/** The agentmail binary: the installed path, else `agentmail` on PATH. */
function bin() {
  return BIN === PLACEHOLDER ? "agentmail" : BIN;
}

/**
 * The environment for agentmail commands: opencode's, without the
 * variables that say who is calling. opencode's process may have inherited
 * them from the shell it was started in, and they name no session of this
 * opencode: with `AGENTMAIL_ADDRESS` the CLI would act as that address
 * instead of the session, and with `AGENTMAIL_HUMAN=1` (the human's own
 * shell) every hook would act as the human and say nothing, which would
 * silently turn the plugin off for every session.
 */
function childEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  delete env.AGENTMAIL_ADDRESS;
  delete env.AGENTMAIL_HUMAN;
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
 * Run `agentmail <args>` to its end: its exit code and stdout. Its stdin
 * is empty, never opencode's: every `agentmail hook` reads its stdin to the
 * end, and under `opencode run --standalone` the server's stdin carries
 * the CLI's requests (`serve --stdio`), which an inherited stdin would take
 * and so hang the session. Bun's `$` inherits it, hence `Bun.spawn`.
 */
async function run(args) {
  const proc = Bun.spawn([bin(), ...args], { stdin: "ignore", stdout: "pipe", stderr: "ignore", env: childEnv() });
  const [stdout, code] = await Promise.all([new Response(proc.stdout).text(), proc.exited]);
  return { code, stdout };
}

/**
 * `agentmail hook status` for `sessionID`: the agent's status with the
 * words to show under `text`, or null when the session has no agent, the
 * daemon is down, or anything fails.
 */
async function status(sessionID, surfaceUrgent) {
  const flags = surfaceUrgent ? ["--surface-urgent"] : [];
  try {
    const out = await run(["hook", "status", "--harness", "opencode", "--session", sessionID, ...flags]);
    if (out.code !== 0) return null;
    const v = parse(out.stdout);
    return v && typeof v.address === "string" ? v : null;
  } catch {
    return null;
  }
}

/**
 * One `agentmail hook wait` for `sessionID`, of at most `secs` seconds,
 * killed when `signal` aborts. `from` says which unread mail does not
 * count as news: `continued`, all there is now (only mail that arrives
 * from now on counts); `afterWake`, the mail agentmail last woke the
 * session for (the turn that just ended was started by a wake);
 * `known`, exactly these ids (the unread mail when the plugin queued open
 * questions as a turn, the unread mail when a turn was interrupted or
 * failed, or the `known` ids of the timed-out
 * wait before this one, so mail that came between counts); none of them,
 * mail already woken for (a plain end of turn). The result is agentmail's
 * answer (`{mail}`, `{busy}`, `{skip}`, `{gone}`), or null when it failed
 * or was killed. agentmail catches the kill: one that comes before it
 * recorded a wake ends it with nothing recorded, one that comes after
 * lets it print the wake first. A mail answer is returned however the
 * process ended, since agentmail has recorded it as a wake and the caller
 * must not lose it.
 */
async function waitOnce(sessionID, from, signal, secs = WAIT_SECS) {
  if (signal.aborted) return null;
  const payload = { stop_hook_active: Boolean(from.continued) };
  if (from.afterWake) payload.after_wake = true;
  if (from.known) payload.known = from.known;
  let proc;
  try {
    proc = Bun.spawn(
      [bin(), "hook", "wait", "--harness", "opencode", "--session", sessionID, "--timeout", String(secs)],
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
    const v = parse(text);
    if (code === 0) return v;
    // Killed (or failed) after printing: only a wake is worth keeping.
    return v && v.mail === true ? v : null;
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
    await run(["hook", "signoff", "--harness", "opencode", "--session", sessionID]);
  } catch {}
}

/** The unread mail ids in a status, or null when it has none listed. */
function unreadIds(st) {
  return Array.isArray(st?.unread_ids) ? st.unread_ids.filter((id) => typeof id === "string") : null;
}

/**
 * The unread mail in `st` as already there for a wait: what a turn the
 * plugin queued with `st` (a status from just before) leaves for the wait
 * after it, and what an interrupted or failed turn leaves (`st` from its
 * end). Without listed ids, all mail there now (only new mail counts).
 */
function before(st) {
  const known = unreadIds(st);
  return known ? { known } : { continued: true };
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
  /**
   * Urgent text agentmail marked surfaced that could not be delivered
   * (steering failed after a tool call with no result to append to), by
   * session: delivered after the next tool call, or at the turn's end.
   */
  const urgentPending = new Map();
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
      // started: the running (or last) turn was started by this plugin,
      // and what the wait after it counts as already there (`waitOnce`'s
      // `from`: `{afterWake}` after a wake or urgent mail, `{known}` after
      // open questions); null for a human's turn. waiter: the idle
      // waiter's controller. missed: a wake was dropped (the session was
      // busy, or queueing it failed), and the agent has not heard of its
      // mail since. handling: turn-end handlers running.
      s = { idle: false, gen: 0, started: null, waiter: null, missed: false, handling: 0 };
      sessions.set(sessionID, s);
      evict();
    } else {
      // Seen again: now the most recent.
      sessions.delete(sessionID);
      sessions.set(sessionID, s);
    }
    return s;
  }

  // Forget the least recently seen sessions past SESSIONS_MAX that need
  // nothing more: idle, no waiter or handler, nothing to deliver, and no
  // turn of this plugin's pending (whose end must not queue its questions
  // again). A forgotten session is looked up again when next seen.
  function evict() {
    if (sessions.size <= SESSIONS_MAX) return;
    for (const [id, s] of sessions) {
      if (sessions.size <= SESSIONS_MAX) break;
      if (!s.idle || s.waiter || s.missed || s.started || s.handling || urgentPending.has(id)) continue;
      sessions.delete(id);
      mine.delete(id);
    }
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
    urgentPending.delete(sessionID);
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
  // reminder as a new turn. One waiter per session; `from` is the first
  // wait's (`waitOnce`). It ends when the session gets busy (stopWaiter),
  // and when agentmail says the agent is gone or wants no waking; the
  // session's next idle period starts a new one. A timeout
  // (`{"mail":false}`) waits again. A wake that cannot be queued keeps
  // waiting (for new mail only), and is tried again after each shorter
  // wait. A `{"busy":true}` while the session is still idle (questions
  // opened since the turn ended) re-runs the turn's end once: the
  // questions are queued, else the waiter waits on.
  async function runWaiter(sessionID, from) {
    const s = state(sessionID);
    if (s.waiter || stopped.signal.aborted) return;
    const ac = new AbortController();
    s.waiter = ac;
    const signal = AbortSignal.any([ac.signal, stopped.signal]);
    const ours = () => !signal.aborted && sessions.get(sessionID) === s && s.idle;
    // Queue `text` as a new turn; whether it was.
    const deliver = async (text) => {
      if (!text || !ours()) return false;
      s.started = { afterWake: true };
      if (await synthetic(sessionID, text, "queue", true)) {
        s.missed = false;
        return true;
      }
      s.started = null;
      return false;
    };
    let continued = Boolean(from.continued);
    let afterWake = Boolean(from.afterWake);
    // The next wait's `known` ids: the timed-out wait's, or `from`'s.
    let known = from.known ?? null;
    // Deliveries failed in a row; the busy answer was re-checked.
    let failed = 0;
    let rechecked = false;
    try {
      while (!signal.aborted) {
        const started = Date.now();
        const secs = failed > 0 ? Math.min(WAIT_SECS, RETRY_SECS * 2 ** (failed - 1)) : WAIT_SECS;
        const r = await waitOnce(sessionID, { continued, afterWake, known }, signal, secs);
        if (!r) break;
        if (r.mail === true) {
          // agentmail recorded this mail as woken: it is this plugin's to
          // deliver now, or when the session is next idle.
          if (stopped.signal.aborted || sessions.get(sessionID) !== s) break;
          s.missed = true;
          if (!ours()) break;
          const text = typeof r.text === "string" && r.text ? r.text : null;
          if (await deliver(text)) break;
          // Not queued: wait on, for new mail only, and try again.
          continued = true;
          afterWake = false;
          known = null;
          failed++;
        } else if (r.mail === false) {
          known = Array.isArray(r.known) ? r.known.filter((id) => typeof id === "string") : null;
          if (!s.missed) failed = 0;
          if (failed > 0) {
            const st = await status(sessionID, false);
            if (!ours()) break;
            const reminder = st?.text?.reminder;
            if (st && !(st.unread > 0)) {
              // Read meanwhile: nothing left to deliver.
              s.missed = false;
              failed = 0;
            } else if (st && typeof reminder === "string" && reminder) {
              if (await deliver(reminder)) break;
              failed++;
            }
          }
        } else if (r.busy === true && !rechecked && ours()) {
          rechecked = true;
          const st = await status(sessionID, false);
          if (!st || st.wake !== true || !ours()) break;
          const questions = st.text?.questions;
          if (!continued && !afterWake && typeof questions === "string" && questions) {
            s.started = before(st);
            if (await synthetic(sessionID, questions, "queue", true)) break;
            s.started = null;
            // They cannot continue the turn: only new mail counts.
            continued = true;
          }
          known = null;
        } else {
          break;
        }
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
    const started = s.started;
    s.started = null;
    return afterTurn(sessionID, s, gen, started, succeeded);
  }

  async function afterTurn(sessionID, s, gen, started, succeeded) {
    s.handling++;
    try {
      await handleTurnEnd(sessionID, s, gen, started, succeeded);
    } finally {
      s.handling--;
      evict();
    }
  }

  // `started`: the turn that ended was this plugin's (`s.started`).
  async function handleTurnEnd(sessionID, s, gen, started, succeeded) {
    // Still idle since this turn ended, and not deleted or unloaded.
    const current = () => sessions.get(sessionID) === s && s.gen === gen && s.idle && !stopped.signal.aborted;
    if (!(await owns(sessionID)) || !current()) return;
    const st = await status(sessionID, false);
    if (!st || !current()) return;
    // Urgent mail that never reached the agent, and open questions, are
    // queued as the next turn; questions once: not after the turn they
    // started.
    // A turn that did not succeed (the human interrupted it, or it
    // failed) queues nothing, and its waiter counts all mail unread now as
    // already there: mail waiting at the stop must not start a turn the
    // human just stopped, nor one more failing turn. Urgent text and a
    // dropped wake stay pending for the next turn.
    if (!succeeded) {
      if (st.wake === true) runWaiter(sessionID, before(st));
      return;
    }
    let questionsFailed = false;
    const questions = st.text?.questions;
    const ask = !started && typeof questions === "string" && questions;
    const urgent = urgentPending.get(sessionID);
    const parts = [urgent, ask && questions].filter(Boolean);
    if (parts.length) {
      s.started = ask ? before(st) : { afterWake: true };
      if (await synthetic(sessionID, parts.join("\n\n"), "queue", true)) {
        if (urgentPending.get(sessionID) === urgent) urgentPending.delete(sessionID);
        return;
      }
      s.started = null;
      if (ask) questionsFailed = true;
      if (!current()) return;
    }
    // A wake dropped while the session was busy, whose mail the turn
    // never heard of (no tool call reminded it): deliver it now.
    const reminder = st.text?.reminder;
    if (s.missed && st.wake === true && st.unread > 0 && typeof reminder === "string" && reminder) {
      s.started = { afterWake: true };
      if (await synthetic(sessionID, reminder, "queue", true)) {
        s.missed = false;
        return;
      }
      s.started = null;
      if (!current()) return;
    }
    if (st.wake !== true) return;
    // Which unread mail the wait counts as already there. After a turn of
    // the open questions: the mail unread when they were queued. After a
    // wake (or urgent mail) this plugin queued: the mail agentmail last
    // woke the session for, so mail that came during the turn (after its
    // last tool call) still wakes it. When
    // the questions could not be queued: the mail unread now (they would
    // otherwise hold the wait as a continuation). Else (a human's turn):
    // unread mail not yet woken for wakes the session at once.
    let from = {};
    if (started?.known) from = { known: started.known };
    else if (questionsFailed) from = before(st);
    else if (started) from = { afterWake: true };
    runWaiter(sessionID, from);
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
    urgentPending.delete(sessionID);
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
        if (s && e.data.item?.type === "user") s.started = null;
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
  // A failed tool call has no result to append to: its urgent text is
  // kept, for the next tool call or the turn's end.
  disposers.push(
    await ctx.tool.hook("execute.after", async (ev) => {
      const sessionID = ev?.sessionID;
      if (typeof sessionID !== "string" || !sessionID) return;
      const st = await status(sessionID, true);
      if (!st) return;
      const pending = urgentPending.get(sessionID);
      urgentPending.delete(sessionID);
      const fresh = typeof st.text?.urgent === "string" && st.text.urgent ? st.text.urgent : null;
      const urgent = [pending, fresh].filter(Boolean).join("\n\n");
      const hasUrgent = Boolean(urgent);
      if (hasUrgent && (await synthetic(sessionID, urgent, "steer", false))) return;
      if (ev.status !== "completed" || !ev.result || typeof ev.result !== "object") {
        if (hasUrgent) urgentPending.set(sessionID, [urgentPending.get(sessionID), urgent].filter(Boolean).join("\n\n"));
        return;
      }
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
    urgentPending.clear();
    for (const d of disposers) {
      try {
        await d?.dispose?.();
      } catch {}
    }
  };
}

export default { id: "agentmail", setup };
