// A browser, driven directly over the DevTools protocol.
//
// The pattern is the appliance console's (`sentinel/tests/console/harness.mjs`)
// and it is here for the same reason: the console is one generated document,
// and the failures that hurt it are the ones a parser cannot see. A function
// that was never written, an element a redesign removed, a field the API
// answers with a 400. All of them load, parse and render — and then do nothing.
// Only a real browser clicking real buttons catches that.
//
// It speaks CDP over a WebSocket rather than using a driver library: one file
// with no dependencies is something that still runs in five years.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const CHROME = process.env.CHROMIUM || "chromium";

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/// A port nothing else is on. A fixed debugging port is the worst kind of test
/// dependency: a browser left behind by an interrupted run keeps listening on
/// it, the browser this run starts cannot bind and dies, and the harness then
/// attaches to *the old page* — an old build against a dead API. Everything
/// after that is a lie that reads as the console being broken.
function freePort() {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

export async function browser({ width = 1600, height = 1000 } = {}) {
  const port = await freePort();
  const profile = mkdtempSync(join(tmpdir(), "cloud-console-"));
  const proc = spawn(CHROME, [
    "--headless=new",
    `--remote-debugging-port=${port}`,
    "--no-first-run",
    "--no-default-browser-check",
    // There is no user namespace to sandbox into in a build sandbox, and the
    // page we load is one we generated ourselves.
    "--no-sandbox",
    "--disable-dev-shm-usage",
    "--disable-gpu",
    `--window-size=${width},${height}`,
    `--user-data-dir=${profile}`,
    "about:blank",
  ], { stdio: ["ignore", "pipe", "pipe"] });

  // The browser's own last words. When it dies mid-run the only evidence is on
  // its stderr, and a harness that discards it can only report that nothing
  // answered.
  const said = [];
  const keep = (chunk) => { said.push(String(chunk)); if (said.length > 40) said.shift(); };
  proc.stdout.on("data", keep);
  proc.stderr.on("data", keep);

  let socket = null, died = null;
  proc.on("exit", (code, signal) => { died = signal ? `signal ${signal}` : `code ${code}`; });
  for (let i = 0; i < 120 && !socket; i++) {
    if (died !== null) throw new Error(`the browser exited before it listened (${died})`);
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const target = list.find((t) => t.type === "page");
      if (target) socket = target.webSocketDebuggerUrl;
    } catch (e) { /* not listening yet */ }
    if (!socket) await sleep(250);
  }
  if (!socket) throw new Error("the browser never came up");

  const ws = new WebSocket(socket);
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = () => reject(new Error("could not attach to the browser"));
  });

  let id = 0;
  const pending = new Map();
  const thrown = [];
  const requests = [];
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); return; }
    // An uncaught exception is the failure this harness exists for: the page
    // keeps rendering and every button wired after it is dead.
    if (m.method === "Runtime.exceptionThrown") {
      const d = m.params.exceptionDetails;
      thrown.push(`${d.exception?.description || d.text} (line ${d.lineNumber})`);
    }
    if (m.method === "Runtime.consoleAPICalled" && m.params.type === "error") {
      thrown.push("console.error: " + m.params.args.map((a) => a.value ?? a.description).join(" "));
    }
    // Every byte this page asks for, so "it fetches nothing from outside
    // itself" is a checked claim rather than an intention.
    if (m.method === "Network.requestWillBeSent") requests.push(m.params.request.url);
  };

  const CALL_TIMEOUT = Number(process.env.CONSOLE_CALL_TIMEOUT || 60000);

  // A browser that has died must not be waited out: every pending call fails
  // the moment the process exits, with the reason and its last words.
  const giveUp = (why) => {
    const tail = said.join("").trim().split("\n").slice(-8).join("\n");
    const e = new Error(why + (tail ? `\n  the browser said:\n  ${tail.split("\n").join("\n  ")}` : ""));
    for (const [, settle] of pending) settle({ __dead: e });
    pending.clear();
  };
  proc.on("exit", (code, signal) =>
    giveUp(`the browser exited mid-run (${signal ? `signal ${signal}` : `code ${code}`})`));
  ws.addEventListener("close", () => giveUp("the browser closed the debugging socket mid-run"));

  const send = (method, params = {}) =>
    new Promise((res, rej) => {
      const callId = ++id;
      const timer = setTimeout(() => {
        pending.delete(callId);
        rej(new Error(`the page stopped answering (${method} after ${CALL_TIMEOUT}ms)`));
      }, CALL_TIMEOUT);
      pending.set(callId, (m) => {
        clearTimeout(timer);
        if (m && m.__dead) rej(m.__dead); else res(m);
      });
      ws.send(JSON.stringify({ id: callId, method, params }));
    });

  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    const failed = r.result?.exceptionDetails;
    if (failed) throw new Error(failed.exception?.description || failed.text);
    return r.result?.result?.value;
  };

  await send("Runtime.enable");
  await send("Page.enable");
  await send("Network.enable");

  return {
    evaluate, thrown, requests,
    async goto(url) { await send("Page.navigate", { url }); await sleep(900); },
    async screenshot() { return (await send("Page.captureScreenshot", { format: "png" })).result.data; },
    close() {
      try { ws.close(); } catch (e) {}
      proc.kill();
      try { rmSync(profile, { recursive: true, force: true }); } catch (e) {}
    },
  };
}

/// Sign in through the form, not by writing the token into storage, so the
/// sign-in path is covered too.
export async function signIn(page, token) {
  await page.evaluate(`(() => {
    document.getElementById("token").value = ${JSON.stringify(token)};
    document.getElementById("tokenform").dispatchEvent(new Event("submit", { cancelable: true }));
    return 1;
  })()`);
  await sleep(900);
  const inside = await page.evaluate(`!document.getElementById("app").classList.contains("hidden")`);
  if (!inside) {
    const why = await page.evaluate(`document.getElementById("loginerr").textContent`);
    throw new Error(`signing in left the sign-in screen up: ${why}`);
  }
}

/// Wait for something to become true, rather than sleeping and hoping.
///
/// A live cell reconciles while the test is running: an instance created a
/// moment ago is scheduled, claimed and running within a turn or two. Any
/// assertion made after a fixed sleep is therefore a race — it passes on a
/// system where nothing moves and fails on the one the product is for. Polling
/// for the condition and giving up loudly is the only shape that means the same
/// thing on both.
export async function waitFor(page, expression, { timeout = 10000, every = 200 } = {}) {
  const until = Date.now() + timeout;
  let last;
  for (;;) {
    last = await page.evaluate(`(() => { try { return (${expression}); } catch (e) { return null; } })()`);
    if (last) return last;
    if (Date.now() > until) return null;
    await sleep(every);
  }
}

/// Open a collection and wait for its board.
export async function open(page, collectionId) {
  await page.evaluate(`(async () => {
    document.querySelector('[data-collection="${collectionId}"]').click();
    await new Promise((r) => setTimeout(r, 500));
    return 1;
  })()`);
}

/// Open one object's sheet by id.
export async function openRow(page, id) {
  const found = await page.evaluate(`(async () => {
    const row = [...document.querySelectorAll("#boardbody tr")]
      .find((r) => r.dataset.name.split("/").pop() === ${JSON.stringify(id)});
    if (!row) return 0;
    row.click();
    await new Promise((r) => setTimeout(r, 400));
    return 1;
  })()`);
  if (!found) throw new Error(`no row for ${id} on the board`);
}

export const sheetText = (page) => page.evaluate(`document.getElementById("sheet").innerText`);

// ---- the smallest test runner that reports usefully -------------------------

const results = [];

/// Thrown when the API under test simply has nothing to check this against —
/// no drifting object, no attachment. Reported as a skip and never as a pass:
/// a check that silently turns into a no-op against a different seed is worse
/// than no check, because the number at the bottom keeps saying everything is
/// fine.
export class Skipped extends Error {}

export function skip(why) {
  throw new Skipped(why);
}

export async function test(name, body) {
  try {
    await body();
    results.push({ name, ok: true });
    console.log(`  ok   ${name}`);
  } catch (e) {
    if (e instanceof Skipped) {
      results.push({ name, skipped: true, why: e.message });
      console.log(`  skip ${name}\n       ${e.message}`);
      return;
    }
    results.push({ name, ok: false, why: String(e.message || e) });
    console.log(`  FAIL ${name}\n       ${String(e.message || e).split("\n").join("\n       ")}`);
  }
}

export function check(condition, message) {
  if (!condition) throw new Error(message);
}

export function equal(actual, expected, message) {
  const a = JSON.stringify(actual), b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${message}\n  expected ${b}\n  got      ${a}`);
}

export function summary() {
  const bad = results.filter((r) => !r.ok && !r.skipped);
  const skipped = results.filter((r) => r.skipped);
  const ran = results.length - skipped.length;
  console.log(`\n${ran - bad.length}/${ran} passed` +
    (skipped.length ? `, ${skipped.length} skipped — this API's seed holds nothing to check them against` : ""));
  return bad.length;
}
