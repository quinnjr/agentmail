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
const line = { args, stdin, session_env: process.env.OPENCODE_SESSION_ID ?? "", address: process.env.AGENTMAIL_ADDRESS ?? "", human: process.env.AGENTMAIL_HUMAN ?? "" };
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
    if (v && v._print_on_term) {
      // Answer only when killed, then die by the signal anyway, as the real
      // agentmail does when the kill lands after it recorded and printed a
      // wake: the answer was on its way when the plugin killed it.
      delete v._print_on_term;
      process.on("SIGTERM", () => {
        fs.writeSync(1, JSON.stringify(v) + "\\n");
        process.kill(process.pid, "SIGKILL");
      });
      setTimeout(() => {}, 30000);
    } else if (v && v._delay_ms) {
      // Answer late.
      const ms = v._delay_ms;
      delete v._delay_ms;
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
  // Inherited from the shell opencode was started in: the human's own
  // identity there, which must not reach the plugin's agentmail calls.
  process.env.AGENTMAIL_ADDRESS = "outer-agent";
  process.env.AGENTMAIL_HUMAN = "1";
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
  // The identity opencode inherited never reaches agentmail.
  expect(c.address).toBe("");
  expect(c.human).toBe("");

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
  expect(first.human).toBe("");

  // The woken turn runs, then ends: the waiter re-arms, for mail other
  // than what it was woken for (mail that came during the woken turn
  // counts).
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: false, after_wake: true });
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
  queueWaits("ses_a", { mail: false }, { busy: true }, { busy: true });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 3, 5000);
  const started = Date.now();
  // A busy answer while the session is idle, with no questions to queue,
  // waits again once, as the turn's end would; a second busy ends the
  // waiter: no fourth wait, and no wake.
  expect(JSON.parse(waits("ses_a")[2].stdin)).toEqual({ stop_hook_active: false });
  await Bun.sleep(1500);
  expect(waits("ses_a")).toHaveLength(3);
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
  setStatus("ses_a", registered({ unread: 1, unread_ids: ["q1"], text: { questions: "❓ q" } }));
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  // The questions cannot continue the turn: wait for mail other than
  // what is unread now, and the open questions do not end the wait.
  expect(JSON.parse(waits("ses_a")[0].stdin)).toEqual({ stop_hook_active: false, known: ["q1"] });
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
  queueWaits("ses_a", { mail: true, text: "📬 1 unread", _print_on_term: true });
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
  // Once; the turn it starts is a wake's.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: false, after_wake: true });
  expect(ctx.synthetic).toHaveLength(1);
});

test("wake_dropped_while_busy_is_not_repeated_after_a_tool_reminder", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: true, text: "📬 1 unread", _print_on_term: true });
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

test("busy_while_idle_queues_questions_that_opened_since", async () => {
  // A question arrives after the turn end's status, before the wait looks:
  // agentmail answers busy (the questions would continue the turn). The
  // session is still idle, so the plugin queues them itself.
  const { ctx } = await start();
  setStatus("ses_a", registered());
  queueWaits("ses_a", { busy: true, _delay_ms: 300 });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  setStatus(
    "ses_a",
    registered({ unread: 1, unread_ids: ["q1"], open_questions: [{ id: "q1" }], text: { questions: "❓ q" } }),
  );
  await until(() => ctx.synthetic.length === 1);
  expect(ctx.synthetic[0]).toEqual({ sessionID: "ses_a", text: "❓ q", delivery: "queue", resume: true });
  // The turn they start is the plugin's: its end does not queue them
  // again, and its wait counts mail other than what was unread then.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 2);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: false, known: ["q1"] });
  expect(ctx.synthetic).toHaveLength(1);
});

test("busy_while_idle_with_unqueueable_questions_waits_for_new_mail", async () => {
  let fail = true;
  const { ctx } = await start({
    synthetic: () => {
      if (fail) throw new Error("no");
    },
  });
  setStatus("ses_a", registered());
  queueWaits("ses_a", { busy: true, _delay_ms: 300 });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  setStatus("ses_a", registered({ unread: 1, text: { questions: "❓ q" } }));
  await until(() => waits("ses_a").length === 2, 5000);
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: true });
  expect(ctx.synthetic).toEqual([]);
  fail = false;
});

test("wake_that_cannot_be_queued_keeps_waiting_and_retries", async () => {
  let failures = 1;
  const { ctx } = await start({
    synthetic: () => {
      if (failures > 0) {
        failures--;
        throw new Error("busy elsewhere");
      }
    },
  });
  setStatus("ses_a", registered());
  queueWaits("ses_a", { mail: true, text: "📬 1 unread" }, { mail: false, known: ["m1"] });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  // Not queued: a new wait starts at once, for new mail only, and short.
  await until(() => waits("ses_a").length === 2, 5000);
  const second = waits("ses_a")[1];
  expect(JSON.parse(second.stdin)).toEqual({ stop_hook_active: true });
  expect(second.args.at(-1)).toBe("5");
  // That wait times out: the reminder is tried again, from a fresh status.
  setStatus("ses_a", registered({ unread: 1, text: { reminder: "📬 1 unread (again)" } }));
  await until(() => ctx.synthetic.length === 1, 5000);
  expect(ctx.synthetic[0]).toEqual({ sessionID: "ses_a", text: "📬 1 unread (again)", delivery: "queue", resume: true });
  // Delivered: the waiter ends.
  await Bun.sleep(1200);
  expect(waits("ses_a")).toHaveLength(2);
});

test("wake_without_text_keeps_waiting_and_retries", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ unread: 1, text: { reminder: "📬 1 unread" } }));
  queueWaits("ses_a", { mail: true }, { mail: false });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1, 5000);
  expect(ctx.synthetic[0].text).toBe("📬 1 unread");
  expect(JSON.parse(waits("ses_a")[1].stdin)).toEqual({ stop_hook_active: true });
});

test("urgent_for_a_failed_tool_is_kept_when_steering_fails", async () => {
  let fail = true;
  const { ctx } = await start({
    synthetic: () => {
      if (fail) throw new Error("no");
    },
  });
  // A failed tool call (nothing to append to), and the steer fails: the
  // urgent text agentmail marked surfaced is kept...
  setStatus("ses_a", registered({ unread: 1, urgent: [{ id: "m1" }], text: { urgent: "🚨 prod" } }));
  const failed = { tool: "shell", sessionID: "ses_a", status: "error", error: new Error("x") };
  await ctx.hooks["execute.after"](failed);
  expect(ctx.synthetic).toEqual([]);
  // ...and pushed after the next tool call, though no longer reported.
  fail = false;
  setStatus("ses_a", registered({ unread: 1 }));
  const next = toolEvent("ses_a");
  await ctx.hooks["execute.after"](next);
  expect(ctx.synthetic).toEqual([{ sessionID: "ses_a", text: "🚨 prod", delivery: "steer", resume: false }]);
  // Once.
  await ctx.hooks["execute.after"](toolEvent("ses_a"));
  expect(ctx.synthetic).toHaveLength(1);

  // Without a next tool call it is queued at the turn's end.
  fail = true;
  setStatus("ses_a", registered({ unread: 1, urgent: [{ id: "m2" }], text: { urgent: "🚨 db" } }));
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await ctx.hooks["execute.after"]({ ...failed });
  fail = false;
  setStatus("ses_a", registered({ unread: 2 }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 2);
  expect(ctx.synthetic[1]).toEqual({ sessionID: "ses_a", text: "🚨 db", delivery: "queue", resume: true });
});

test("own_sessions_are_bounded", async () => {
  const { ctx } = await start();
  // Sessions without an agent: idle, no waiter, nothing to deliver.
  for (let i = 0; i < 300; i++) ctx.emit("session.execution.succeeded", { sessionID: `ses_${i}` });
  await until(() => ctx.gets.length === 300 && calls().length === 300, 10000);
  await Bun.sleep(100);
  // The oldest were forgotten (looked up again), the newest kept.
  ctx.emit("session.execution.succeeded", { sessionID: "ses_0" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_299" });
  await until(() => ctx.gets.length === 301);
  await until(() => calls().length === 302);
  expect(ctx.gets.slice(300)).toEqual(["ses_0"]);
}, 20000);

test("sessions_with_a_waiter_are_not_forgotten", async () => {
  const { ctx } = await start();
  setStatus("ses_w", registered());
  ctx.emit("session.execution.succeeded", { sessionID: "ses_w" });
  await until(() => existsSync(join(fake, "waiting.ses_w.pid")));
  const pid = Number(readFileSync(join(fake, "waiting.ses_w.pid"), "utf8"));
  for (let i = 0; i < 300; i++) ctx.emit("session.execution.succeeded", { sessionID: `ses_${i}` });
  await until(() => ctx.gets.length === 301 && calls().length === 302, 10000);
  await Bun.sleep(100);
  expect(alive(pid)).toBe(true);
  // Still known: a new turn stops its waiter without a lookup.
  ctx.emit("session.execution.started", { sessionID: "ses_w" });
  await until(() => !alive(pid));
  expect(ctx.gets.filter((id) => id === "ses_w")).toHaveLength(1);
}, 20000);

test("questions_turn_waits_for_mail_that_came_during_it", async () => {
  // The questions are queued at a human turn's end; the wait after their
  // turn counts as already there only the mail unread when they were
  // queued, so mail that came during that turn wakes the session.
  const { ctx } = await start();
  setStatus("ses_a", registered({ unread: 2, unread_ids: ["m1", "q1"], text: { questions: "❓ q" } }));
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1);
  expect(waits("ses_a")).toEqual([]);
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  setStatus("ses_a", registered({ unread: 3, unread_ids: ["m1", "m2", "q1"], text: { questions: "❓ q" } }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  expect(JSON.parse(waits("ses_a")[0].stdin)).toEqual({ stop_hook_active: false, known: ["m1", "q1"] });
  expect(ctx.synthetic).toHaveLength(1);
});

test("waiter_after_a_failed_or_interrupted_turn_counts_mail_since_the_last_wake", async () => {
  const { ctx } = await start();
  setStatus("ses_a", registered({ unread: 1, unread_ids: ["m1"], text: { questions: "❓ q" } }));
  for (const [type, extra] of [
    ["session.execution.failed", { error: "x" }],
    ["session.execution.interrupted", { reason: "user" }],
  ]) {
    const n = waits("ses_a").length;
    ctx.emit("session.execution.started", { sessionID: "ses_a" });
    ctx.emit(type, { sessionID: "ses_a", ...extra });
    await until(() => waits("ses_a").length === n + 1);
    // Not the questions (the turn did not succeed), and the wait counts
    // mail that came during the turn.
    expect(JSON.parse(waits("ses_a")[n].stdin)).toEqual({ stop_hook_active: false, after_wake: true });
  }
  expect(ctx.synthetic).toEqual([]);
});

test("waiter_after_queued_urgent_mail_counts_mail_since_the_last_wake", async () => {
  let fail = true;
  const { ctx } = await start({
    synthetic: () => {
      if (fail) throw new Error("no");
    },
  });
  setStatus("ses_a", registered({ unread: 1, urgent: [{ id: "m1" }], text: { urgent: "🚨 prod" } }));
  // A failed tool call, and the steer fails: kept for the turn's end.
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  await ctx.hooks["execute.after"]({ tool: "shell", sessionID: "ses_a", status: "error", error: new Error("x") });
  fail = false;
  setStatus("ses_a", registered({ unread: 1 }));
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => ctx.synthetic.length === 1);
  expect(ctx.synthetic[0].text).toBe("🚨 prod");
  ctx.emit("session.execution.started", { sessionID: "ses_a" });
  ctx.emit("session.execution.succeeded", { sessionID: "ses_a" });
  await until(() => waits("ses_a").length === 1);
  expect(JSON.parse(waits("ses_a")[0].stdin)).toEqual({ stop_hook_active: false, after_wake: true });
});
