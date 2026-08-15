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

  // Wait for the browser to really be gone, without the event loop.
  //
  // The cleanup below runs from `process.on("exit")`, where nothing async will
  // ever run again, so `await` and timers are not available. Removing the
  // profile while chromium is still shutting down half works: the directory is
  // emptied and the dying browser writes a stub back over it. So the wait is
  // done by asking the kernel whether the pid is still there.
  const gone = (within) => {
    const until = Date.now() + within;
    const idle = new Int32Array(new SharedArrayBuffer(4));
    for (;;) {
      try { process.kill(proc.pid, 0); } catch (e) { return true; }
      if (Date.now() > until) return false;
      Atomics.wait(idle, 0, 0, 25);
    }
  };

  let shut = false;
  const shutDown = () => {
    if (shut) return;
    shut = true;
    try { ws.close(); } catch (e) {}
    // Asked first, so chromium takes its own children with it — a killed parent
    // leaves the zygote and every renderer behind, and those are what fill a
    // machine after a handful of interrupted runs.
    try { proc.kill(); } catch (e) {}
    if (!gone(3000)) { try { proc.kill("SIGKILL"); } catch (e) {} gone(1000); }
    try { rmSync(profile, { recursive: true, force: true }); } catch (e) {}
  };

  // A run that dies takes the browser and its profile with it.
  //
  // The suite calls `close()` on its last line, so it only ever ran on the happy
  // path: any throw above it — an assertion the harness itself raises, a browser
  // that went away mid-run — left a live chromium behind and a ~147 MB profile
  // in the temp directory. That is not tidiness. A handful of abandoned runs
  // fill the tmpfs, and the next run dies with "No space left on device" while
  // the suite is entirely fine. A failure that makes *later* runs fail for an
  // unrelated reason is the thing that teaches people to re-run instead of look.
  //
  // `exit` is sync-only, which is why the cleanup is sync. The signals are here
  // because an interrupted run is the common way this happens.
  const onExit = () => shutDown();
  process.once("exit", onExit);
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.once(signal, () => { shutDown(); process.exit(130); });
  }

  return {
    evaluate, thrown, requests,
    async goto(url) { await send("Page.navigate", { url }); await sleep(900); },
    async screenshot() { return (await send("Page.captureScreenshot", { format: "png" })).result.data; },
    close() { process.removeListener("exit", onExit); shutDown(); },
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
  const inside = await waitFor(page,
    `!document.getElementById("app").classList.contains("hidden") || null`);
  if (!inside) {
    const why = await page.evaluate(`document.getElementById("loginerr").textContent`);
    throw new Error(`signing in left the sign-in screen up: ${why}`);
  }
  // Signed in is not the same as ready to be asked a question. The console shows
  // the app as soon as the token is accepted and lists the first collection
  // after that, so a check made in between reads a board that has not been
  // filled and reports "no instances at all" about a cell that has plenty. That
  // was a fixed 900 ms, which is a guess about a round trip.
  const board = await waitFor(page, `(() => {
    if (!view.coll) return null;
    if (document.querySelectorAll("#boardbody tr").length) return "rows";
    for (const id of ["listempty", "listerr"]) {
      const box = document.getElementById(id);
      if (box && !box.classList.contains("hidden")) return id;
    }
    return null;
  })()`, { timeout: 20000 });
  if (!board) throw new Error("signing in never produced a board to look at");
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

/// Open a collection and wait for its board — for the board, not for a while.
///
/// `show()` renders the new collection only once its list has come back, so
/// until then `view.items` and the rows on screen are still the *previous*
/// collection's. This used to click and sleep half a second, which is enough on
/// an idle machine and is not a guarantee: with the API answering a few hundred
/// milliseconds slower — a rebuild running beside the suite is enough — every
/// check that names its subject out of `view.items` picks it out of the wrong
/// world, and reports whatever that produces. Sleeping longer only moves the
/// line, so what is waited for is the thing itself: the promise the rail's own
/// handler made.
export async function open(page, collectionId) {
  const opened = await page.evaluate(`(async () => {
    const rail = document.querySelector('[data-collection=${JSON.stringify(collectionId)}]');
    if (!rail) return "no rail item";
    // Clicked, because that is how an operator gets there and the rail is worth
    // exercising — and the show it starts is taken in passing so this can wait
    // on it rather than on a guess. \`show\` is a top-level declaration, so the
    // rail's handler reaches it through the global object.
    let started = null;
    const real = window.show;
    window.show = (id) => (started = real(id));
    try { rail.click(); } finally { window.show = real; }
    if (!started) return "the rail did not open anything";
    await started;
    return "";
  })()`);
  if (opened) throw new Error(`could not open ${collectionId}: ${opened}`);
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
