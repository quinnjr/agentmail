// Unit tests for the opencode plugin: `bun test plugin/`.
//
// opencode's plugin context is mocked (it records hook registrations and
// `session.synthetic` calls, and feeds events), and a fake `agentmail`
// script on PATH answers with canned JSON and logs how it was called.

import { afterAll, afterEach, beforeAll, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import plugin from "./agentmail.js";

let root;
let fake;

// The fake agentmail. `hook status` prints `status.<session>.json` (else
// `{}`). `hook wait` prints the next line of `wait.<session>` and removes
// it, or, with none left, sleeps until killed (writing its pid to
// `waiting.<session>.pid`). Every call is logged to `calls.log` as one
// JSON line with its arguments, stdin and the variables that matter.
const FAKE = `#!/usr/bin/env bun
const fs = require("node:fs");
const dir = process.env.AGENTMAIL_FAKE_DIR;
const args = process.argv.slice(2);
const i = args.indexOf("--session");
const session = i >= 0 ? args[i + 1] : "";
const cmd = args.slice(0, 2).join(" ");
const stdin = cmd === "hook wait" ? fs.readFileSync(0, "utf8") : "";
const line = { args, stdin, session_env: process.env.OPENCODE_SESSION_ID ?? "", address: process.env.AGENTMAIL_ADDRESS ?? "" };
fs.appendFileSync(dir + "/calls.log", JSON.stringify(line) + "\\n");
if (cmd === "hook status") {
  const f = dir + "/status." + session + ".json";
  process.stdout.write(fs.existsSync(f) ? fs.readFileSync(f, "utf8") : "{}");
} else if (cmd === "hook wait") {
  const q = dir + "/wait." + session;
  const lines = fs.existsSync(q) ? fs.readFileSync(q, "utf8").split("\\n").filter(Boolean) : [];
  if (lines.length) {
    fs.writeFileSync(q, lines.slice(1).map((l) => l + "\\n").join(""));
    let answer = lines[0];
    let v = null;
    try { v = JSON.parse(answer); } catch {}
    if (v && v._delay_ms) {
      // Answer late, and (with _ignore_term) after a kill: the answer was
      // already on its way when the plugin killed it.
      if (v._ignore_term) process.on("SIGTERM", () => {});
      const ms = v._delay_ms;
      delete v._delay_ms;
      delete v._ignore_term;
      setTimeout(() => process.stdout.write(JSON.stringify(v) + "\\n"), ms);
    } else {
      process.stdout.write(answer + "\\n");
    }
  } else {
    fs.writeFileSync(dir + "/waiting." + session + ".pid", String(process.pid));
    setTimeout(() => {}, 30000);
  }
} else if (cmd === "hook signoff") {
  process.stdout.write('{"signed_off":true}');
}
`;

beforeAll(() => {
  root = mkdtempSync(join(tmpdir(), "agentmail-plugin-"));
  mkdirSync(join(root, "bin"));
  writeFileSync(join(root, "bin", "agentmail"), FAKE);
  chmodSync(join(root, "bin", "agentmail"), 0o755);
  process.env.PATH = `${join(root, "bin")}:${process.env.PATH}`;
  process.env.AGENTMAIL_ADDRESS = "outer-agent";
});

afterAll(() => rmSync(root, { recursive: true, force: true }));

let cleanups = [];

afterEach(async () => {
  for (const c of cleanups) await c();
  cleanups = [];
});

/** A fresh directory of canned answers for one test. */
function fakeDir() {
  fake = mkdtempSync(join(root, "case-"));
  process.env.AGENTMAIL_FAKE_DIR = fake;
  return fake;
}

function setStatus(session, v) {
  writeFileSync(join(fake, `status.${session}.json`), JSON.stringify(v));
}

function queueWaits(session, ...answers) {
  writeFileSync(join(fake, `wait.${session}`), answers.map((a) => JSON.stringify(a) + "\n").join(""));
}

function calls() {
  const p = join(fake, "calls.log");
  if (!existsSync(p)) return [];
  return readFileSync(p, "utf8")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l));
}

const waits = (session) => calls().filter((c) => c.args[1] === "wait" && c.args.includes(session));

/** Poll `cond` until it holds, or fail after `ms`. */
async function until(cond, ms = 3000) {
  const deadline = Date.now() + ms;
  while (!(await cond())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${cond}`);
    await Bun.sleep(10);
  }
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** opencode's plugin context, mocked: records and feeds. */
function mockCtx({ directory = "/work", sessionDirs = {}, get = null, synthetic = null } = {}) {
  const queue = [];
  let wake = null;
  const ctx = {
    location: { directory },
    hooks: {},
    synthetic: [],
    disposed: 0,
    subscribed: null,
    tool: {
      hook: async (name, fn) => {
        ctx.hooks[name] = fn;
        return { dispose: () => void ctx.disposed++ };
      },
    },
    gets: [],
    session: {
      // `synthetic(input)`, when given, may throw to fail the call.
      synthetic: async (input) => {
        if (synthetic) await synthetic(input);
        ctx.synthetic.push(input);
        return { id: `msg_${ctx.synthetic.length}`, type: "synthetic" };
      },
      get: async ({ sessionID }) => {
        ctx.gets.push(sessionID);
        if (get) await get(sessionID);
        return { id: sessionID, location: { directory: sessionDirs[sessionID] ?? directory } };
      },
    },
    event: {
      subscribe: ({ signal } = {}) => {
        ctx.subscribed = signal;
        return {
          async *[Symbol.asyncIterator]() {
            while (!signal?.aborted) {
              if (queue.length) {
                yield queue.shift();
                continue;
              }
              await new Promise((r) => {
                wake = r;
                signal?.addEventListener("abort", r, { once: true });
              });
            }
          },
        };
      },
    },
    emit(type, data) {
      queue.push({ id: `evt_${Math.random()}`, type, data });
      wake?.();
    },
  };
  return ctx;
}

async function start(opts) {
  fakeDir();
  const ctx = mockCtx(opts);
  const cleanup = await plugin.setup(ctx);
  cleanups.push(cleanup);
  return { ctx, cleanup };
}

/** A completed tool call's `execute.after` event, as opencode sends it. */
function toolEvent(sessionID, status = "completed") {
  return {
    tool: "shell",
    sessionID,
    status,
    result: { output: { exit: 0, output: "hi\n" }, content: [{ type: "text", text: "hi\n" }] },
  };
}

const registered = (extra = {}) => ({
  address: "backend-k3x9q2mz",
  name: "backend",
  wake: true,
  unread: 0,
  unread_ids: [],
  open_questions: [],
  urgent: [],
  text: { guidance: "you are backend-k3x9q2mz", reminder: null, urgent: null, questions: null },
  ...extra,
});

test("plugin_shape", () => {
  expect(plugin.id).toBe("agentmail");
  expect(typeof plugin.setup).toBe("function");
});

test("appends_reminder_after_tool", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ unread: 2, text: { reminder: "📬 2 unread" } }));
  const ev = toolEvent("ses_a");
  await ctx.hooks["execute.after"](ev);
  expect(ev.result.content).toEqual([
    { type: "text", text: "hi\n" },
    { type: "text", text: "📬 2 unread" },
  ]);
  expect(ctx.synthetic).toEqual([]);
  const [c] = calls();
  expect(c.args).toEqual(["hook", "status", "--harness", "opencode", "--session", "ses_a", "--surface-urgent"]);
  // The address opencode inherited never reaches agentmail.
  expect(c.address).toBe("");

  // A failed tool call has no result to append to.
  const failed = { tool: "shell", sessionID: "ses_a", status: "error", error: new Error("x") };
  await ctx.hooks["execute.after"](failed);
  expect(failed.result).toBeUndefined();

  // Nothing unread: nothing appended.
  setStatus("ses_a", registered());
  const quiet = toolEvent("ses_a");
  await ctx.hooks["execute.after"](quiet);
  expect(quiet.result.content).toHaveLength(1);
});

test("urgent_steers_once", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ unread: 1, urgent: [{ id: "m1" }], text: { urgent: "🚨 prod", reminder: "📬 1" } }));
  const ev = toolEvent("ses_a");
  await ctx.hooks["execute.after"](ev);
  expect(ctx.synthetic).toEqual([{ sessionID: "ses_a", text: "🚨 prod", delivery: "steer", resume: false }]);
  // Steered, not also appended.
  expect(ev.result.content).toHaveLength(1);
  // agentmail marked it surfaced: the next status has no urgent mail, and
  // the plugin only reminds.
  setStatus("ses_a", registered({ unread: 1, text: { urgent: null, reminder: "📬 1" } }));
  const next = toolEvent("ses_a");
  await ctx.hooks["execute.after"](next);
  expect(ctx.synthetic).toHaveLength(1);
  expect(next.result.content.at(-1)).toEqual({ type: "text", text: "📬 1" });
});

test("open_question_queues_after_turn", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ wake: false, unread: 1, text: { questions: "❓ POST /orders body?" } }));
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1);
  expect(ctx.synthetic[0]).toEqual({
    sessionID: "ses_a",
    text: "❓ POST /orders body?",
    delivery: "queue",
    resume: true,
  });
  // The turn it started ends with the question still open: not queued again.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => calls().filter((c) => c.args[1] === "status").length === 2);
  await Bun.sleep(100);
  expect(ctx.synthetic).toHaveLength(1);
  // A human prompt's turn may queue them again.
  ctx.emit("session.inbox.enqueued", { sessionID: "ses_a", item: { type: "user" } });
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 2);
  // A failed or interrupted turn does not.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.interrupted", { sessionID: "ses_a", reason: "user" });
  await until(() => calls().filter((c) => c.args[1] === "status").length === 4);
  await Bun.sleep(100);
  expect(ctx.synthetic).toHaveLength(2);
});

test("waiter_wakes_on_mail_and_rearms", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: true, text: "📬 1 unread" });
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1);
  expect(ctx.synthetic[0]).toEqual({ sessionID: "ses_a", text: "📬 1 unread", delivery: "queue", resume: true });
  const [first] = waits("ses_a");
  expect(first.args).toEqual(["hook", "wait", "--harness", "opencode", "--session", "ses_a", "--timeout", "300"]);
  expect(JSON.parse(first.stdin)).toEqual({ stop_hook_active: false });
  expect(first.address).toBe("");

  // The woken turn runs, then ends: the waiter re-arms, for new mail only.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: true });
  await until(() => existsSync(join(fake, "waiting.ses_a.pid")));
  const pid = Number(readFileSync(join(fake, "waiting.ses_a.pid"), "utf8"));
  expect(alive(pid)).toBe(true);

  // A new turn (the human typed) stops the waiter.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await until(() => !alive(pid));
  expect(ctx.synthetic).toHaveLength(1);
});

test("waiter_waits_again_after_a_timeout_and_stops_otherwise", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: false }, { busy: true });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  const started = Date.now();
  await Bun.sleep(1500);
  // Busy ends the waiter: no third wait, and no wake.
  expect(waits("ses_a")).toHaveLength(2);
  expect(ctx.synthetic).toEqual([]);
  // Gone, skip and failures end it too.
  for (const answer of [{ gone: true }, { skip: true }, "not json"]) {
    writeFileSync(join(fake, "wait.ses_a"), (typeof answer === "string" ? answer : JSON.stringify(answer)) + "\n");
    const before = waits("ses_a").length;
    ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
    await until(() => waits("ses_a").length === before + 1);
    await Bun.sleep(1200);
    expect(waits("ses_a")).toHaveLength(before + 1);
  }
  expect(ctx.synthetic).toEqual([]);
  expect(Date.now() - started).toBeGreaterThan(1000);
}, 15000);

test("waiter_does_not_spin_on_quick_timeouts", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: false }, { mail: false }, { mail: false }, { mail: false });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await Bun.sleep(1500);
  // Answers that come at once are at least a second apart.
  expect(waits("ses_a").length).toBeLessThanOrEqual(2);
});

test("deleted_session_signs_off_and_stops_waiter", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => existsSync(join(fake, "waiting.ses_a.pid")));
  const pid = Number(readFileSync(join(fake, "waiting.ses_a.pid"), "utf8"));
  ctx.emit("session.deleted", { sessionID: "ses_a" });
  await until(() => calls().some((c) => c.args[1] === "signoff"));
  const off = calls().find((c) => c.args[1] === "signoff");
  // The session-strict hook, never the CLI's `signoff` (which would start
  // the daemon, and fall back to process ancestry).
  expect(off.args).toEqual(["hook", "signoff", "--harness", "opencode", "--session", "ses_a"]);
  expect(off.address).toBe("");
  await until(() => !alive(pid));
});

test("deleted_unknown_session_is_signed_off_by_session", async () => {
  // Deleted before this instance saw it: it cannot be looked up any more,
  // so it is signed off, by its session only.
  const { ctx } = await start();
  ctx.emit("session.deleted", { sessionID: "ses_new" });
  await until(() => calls().length === 1);
  expect(calls()[0].args).toEqual(["hook", "signoff", "--harness", "opencode", "--session", "ses_new"]);
});

test("deleted_session_of_another_directory_is_left_alone", async () => {
  const { ctx } = await start({ sessionDirs: { ses_b: "/elsewhere" } });
  setStatus("ses_b", registered({ text: { questions: "❓ q" } }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_b" });
  ctx.emit("session.deleted", { sessionID: "ses_b" });
  await Bun.sleep(300);
  // Not this instance's session: no status, no waiter, no sign-off.
  expect(calls()).toEqual([]);
  expect(ctx.synthetic).toEqual([]);
});

test("cleanup_aborts_waiters", async () => {
  const { ctx, cleanup } = await start();
  setStatus("ses_a", registered());
  setStatus("ses_b", registered({ address: "frontend-p7v2c8wd" }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_b" });
  await until(() => existsSync(join(fake, "waiting.ses_a.pid")) && existsSync(join(fake, "waiting.ses_b.pid")));
  const pids = ["ses_a", "ses_b"].map((s) => Number(readFileSync(join(fake, `waiting.${s}.pid`), "utf8")));
  await cleanup();
  await until(() => pids.every((p) => !alive(p)));
  expect(ctx.subscribed.aborted).toBe(true);
  expect(ctx.disposed).toBe(1);
  // Events after cleanup start nothing.
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await Bun.sleep(200);
  expect(waits("ses_a")).toHaveLength(1);
});

test("no_output_when_unregistered", async () => {
  const { ctx } = await start();
  // No status file: the fake prints `{}`, as agentmail does for a session
  // without an agent or with the daemon down.
  const ev = toolEvent("ses_x");
  await ctx.hooks["execute.after"](ev);
  expect(ev.result.content).toHaveLength(1);
  ctx.emit("session.execution.succeeded", { sessionID: "ses_x" });
  await until(() => calls().length === 2);
  await Bun.sleep(200);
  expect(ctx.synthetic).toEqual([]);
  expect(waits("ses_x")).toEqual([]);
});

test("missing_binary_is_silent", async () => {
  const { ctx } = await start();
  const path = process.env.PATH;
  process.env.PATH = "/nonexistent";
  try {
    const ev = toolEvent("ses_a");
    await ctx.hooks["execute.after"](ev);
    expect(ev.result.content).toHaveLength(1);
    expect(ctx.synthetic).toEqual([]);
  } finally {
    process.env.PATH = path;
  }
});

test("no_waiter_without_wake", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ wake: false }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => calls().length === 1);
  await Bun.sleep(200);
  expect(waits("ses_a")).toEqual([]);
});

test("installed_path_is_substituted_whole", () => {
  // install replaces every `__AGENTMAIL_BIN__` with the binary's path; the
  // fallback check must survive that.
  const src = readFileSync(join(import.meta.dir, "agentmail.js"), "utf8");
  const installed = src.replaceAll("__AGENTMAIL_BIN__", "/opt/agentmail/bin/agentmail");
  expect(installed).toContain('const BIN = "/opt/agentmail/bin/agentmail";');
  expect(installed).toContain('"__AGENTMAIL" + "_BIN__"');
  expect(src.split("__AGENTMAIL_BIN__").length - 1).toBe(1);
});

test("turn_start_during_lookup_wins", async () => {
  // The turn end's session lookup is slow; the next turn starts meanwhile.
  let release;
  const gate = new Promise((r) => (release = r));
  const { ctx } = await start({ get: () => gate });
  setStatus("ses_a", registered());
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await until(() => ctx.gets.length >= 1);
  await Bun.sleep(50);
  release();
  await Bun.sleep(300);
  // The session is busy: no status for the ended turn, no waiter.
  expect(waits("ses_a")).toEqual([]);
  expect(calls()).toEqual([]);
  // Its own end starts the waiter.
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
});

test("turn_start_during_status_wins", async () => {
  // The turn started after the lookup, while `hook status` ran.
  const { ctx } = await start();
  setStatus("ses_a", registered());
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await Bun.sleep(500);
  expect(waits("ses_a")).toEqual([]);
});

test("failed_lookup_is_asked_again", async () => {
  let fail = true;
  const { ctx } = await start({
    get: () => {
      if (fail) throw new Error("transient");
    },
  });
  setStatus("ses_a", registered());
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.gets.length === 1);
  await Bun.sleep(100);
  expect(calls()).toEqual([]);
  fail = false;
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  // A positive answer is kept: no more lookups.
  const n = ctx.gets.length;
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => calls().filter((c) => c.args[1] === "status").length === 2);
  expect(ctx.gets.length).toBe(n);
});

test("foreign_session_is_looked_up_once", async () => {
  const { ctx } = await start({ sessionDirs: { ses_b: "/elsewhere" } });
  setStatus("ses_b", registered());
  for (let i = 0; i < 3; i++) {
    ctx.emit("session.execution.started", { sessionID: "ses_b" });
    ctx.emit("session.execution.succeeded", { sessionID: "ses_b" });
  }
  await Bun.sleep(300);
  expect(ctx.gets).toEqual(["ses_b"]);
  expect(calls()).toEqual([]);
});

test("foreign_sessions_are_bounded", async () => {
  const sessionDirs = {};
  for (let i = 0; i < 300; i++) sessionDirs[`ses_${i}`] = "/elsewhere";
  const { ctx } = await start({ sessionDirs });
  for (let i = 0; i < 300; i++) ctx.emit("session.execution.succeeded", { sessionID: `ses_${i}` });
  await until(() => ctx.gets.length === 300);
  await Bun.sleep(100);
  // The oldest were forgotten (looked up again), the newest kept.
  ctx.emit("session.execution.succeeded", { sessionID: "ses_0" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_299" });
  await until(() => ctx.gets.length === 301);
  await Bun.sleep(100);
  expect(ctx.gets.slice(300)).toEqual(["ses_0"]);
});

test("urgent_is_appended_when_steering_fails", async () => {
  const { ctx } = await start({
    synthetic: () => {
      throw new Error("session ended");
    },
  });
  setStatus("ses_a", registered({ unread: 1, urgent: [{ id: "m1" }], text: { urgent: "🚨 prod", reminder: "📬 1" } }));
  const ev = toolEvent("ses_a");
  await ctx.hooks["execute.after"](ev);
  expect(ev.result.content).toEqual([
    { type: "text", text: "hi\n" },
    { type: "text", text: "🚨 prod" },
    { type: "text", text: "📬 1" },
  ]);
  expect(ctx.synthetic).toEqual([]);
});

test("questions_queue_failure_still_waits_for_new_mail", async () => {
  const { ctx } = await start({
    synthetic: () => {
      throw new Error("no");
    },
  });
  setStatus("ses_a", registered({ unread: 1, text: { questions: "❓ q" } }));
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  // The questions cannot continue the turn: wait as after a continuation,
  // for new mail only, so the open questions do not end the wait.
  expect(JSON.parse(waits("ses_a")[0].stdin)).toEqual({ stop_hook_active: true });
});

test("waiter_passes_known_after_a_timeout", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: false, known: ["m1", "m2"] }, { mail: false });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 3, 5000);
  const [w1, w2, w3] = waits("ses_a").map((w) => JSON.parse(w.stdin));
  expect(w1).toEqual({ stop_hook_active: false });
  expect(w2).toEqual({ stop_hook_active: false, known: ["m1", "m2"] });
  // A timeout without `known` leaves the next wait to look for itself.
  expect(w3).toEqual({ stop_hook_active: false });
});

test("wake_dropped_while_busy_is_delivered_at_turn_end", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  // The wait's mail answer is on its way when the next turn starts.
  queueWaits("ses_a", { mail: true, text: "📬 1 unread", _delay_ms: 300, _ignore_term: true });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  await Bun.sleep(50);
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await Bun.sleep(600);
  // Not queued into the busy turn.
  expect(ctx.synthetic).toEqual([]);
  // That turn makes no tool call, then ends: the agent hears of it now.
  setStatus("ses_a", registered({ unread: 1, text: { reminder: "📬 1 unread" } }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1);
  expect(ctx.synthetic[0]).toEqual({ sessionID: "ses_a", text: "📬 1 unread", delivery: "queue", resume: true });
  // Once.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: true });
  expect(ctx.synthetic).toHaveLength(1);
});

test("wake_dropped_while_busy_is_not_repeated_after_a_tool_reminder", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: true, text: "📬 1 unread", _delay_ms: 300, _ignore_term: true });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  await Bun.sleep(50);
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await Bun.sleep(600);
  // The busy turn's tool call reminds it of the mail.
  setStatus("ses_a", registered({ unread: 1, text: { reminder: "📬 1 unread" } }));
  const ev = toolEvent("ses_a");
  await ctx.hooks["execute.after"](ev);
  expect(ev.result.content.at(-1)).toEqual({ type: "text", text: "📬 1 unread" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(ctx.synthetic).toEqual([]);
});
