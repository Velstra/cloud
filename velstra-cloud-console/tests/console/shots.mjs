// Every view the console has, in both appearances, as PNGs.
//
// Not a test — a camera. The design loop is screenshot, critique, adjust,
// re-screenshot, and doing that by hand against a live browser is how a
// redesign ends up better in the two places somebody happened to look and
// worse everywhere else. This visits all of them, every pass, in a fixed
// order, with fixed names, so two passes can be put side by side.
//
//     CONSOLE_SHOTS=/path/to/dir tests/console/shots.mjs   (via run-shots.sh)

import { writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { browser, signIn, open, openRow, sleep, waitFor } from "./harness.mjs";

const URL = process.env.CONSOLE_URL || "http://127.0.0.1:18100/";
const TOKEN = process.env.CONSOLE_TOKEN || "testtoken";
const OUT = process.env.CONSOLE_SHOTS || "/tmp/console-shots";
mkdirSync(OUT, { recursive: true });

const page = await browser({ width: 1600, height: 1000 });
await page.goto(URL);

let n = 0;
async function shot(name) {
  const file = join(OUT, `${String(++n).padStart(2, "0")}-${name}.png`);
  writeFileSync(file, Buffer.from(await page.screenshot(), "base64"));
  console.log("  " + file);
}

/// Both appearances of the same view, back to back, so a token that only
/// resolves in one of them is visible rather than a thing to remember to check.
async function both(name, before) {
  if (before) await before();
  await setTheme("dark");
  await shot(name + "-dark");
  await setTheme("light");
  await shot(name + "-light");
  await setTheme("dark");
}

/// Switch appearance, and wait for it to finish switching.
///
/// The wait is not politeness. Colours transition, so a screenshot taken the
/// instant after the attribute flips catches every surface part way between the
/// two ramps — which looks exactly like a token that resolves wrongly in one
/// appearance. It cost one round of "fixing" a light-mode bug that did not
/// exist; the transition is 140ms and this waits well past it.
async function setTheme(theme) {
  await page.evaluate(`(() => {
    localStorage.setItem("velstra-cloud-theme", ${JSON.stringify(theme)});
    applyTheme(${JSON.stringify(theme)});
    return 1;
  })()`);
  await sleep(500);
}

// ---- signed out -------------------------------------------------------------

await both("signin");

// ---- the board --------------------------------------------------------------

await signIn(page, { username: process.env.CONSOLE_USER || "operator", password: process.env.CONSOLE_PASSWORD || "a test operator passphrase" });
await both("board-instances", () => open(page, "instances"));
await both("board-nodes", () => open(page, "nodes"));
await both("board-subnets", () => open(page, "subnets"));

// ---- one object -------------------------------------------------------------

await both("sheet-instance", async () => {
  await open(page, "instances");
  await openRow(page, "web-1");
  await sleep(400);
});
await page.evaluate(`closeSheet()`);

// A guest that is failing, which is what the console exists for — the settled
// one above says nothing about how a problem reads.
await both("sheet-failing", async () => {
  await open(page, "instances");
  const id = await page.evaluate(`(() => {
    const r = view.items.find((x) => verdict(x, view.coll.condition).kind === "failing")
      || view.items.find((x) => verdict(x, view.coll.condition).kind !== "settled");
    return r ? idOf(r) : null;
  })()`);
  if (id) { await openRow(page, id); await sleep(400); }
});
await page.evaluate(`closeSheet()`);

// ---- creating ---------------------------------------------------------------
//
// The one the user's complaint is about: "crowded with input boxes".

await both("create-instance", async () => {
  await open(page, "instances");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(400);
});

await both("create-instance-more", async () => {
  await page.evaluate(`(() => {
    const d = document.querySelector("#dialog .disclose");
    if (d) d.click();
    return 1;
  })()`);
  await sleep(300);
});
await page.evaluate(`closeDialog()`);

await both("create-volume", async () => {
  await open(page, "volumes");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(400);
});
await page.evaluate(`closeDialog()`);

// A form with the most in it, so the busiest case is looked at rather than
// inferred from the smallest.
await both("create-securitygroup", async () => {
  await open(page, "security-groups");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(400);
});
await page.evaluate(`closeDialog()`);

// ---- moving a guest ---------------------------------------------------------

await both("migrate", async () => {
  await open(page, "instances");
  await openRow(page, "web-1");
  await sleep(300);
  await page.evaluate(`(() => {
    const b = [...document.querySelectorAll("#sheet button")].find((x) => /move|migrat/i.test(x.textContent));
    if (b) b.click();
    return 1;
  })()`);
  await sleep(500);
});

if (page.thrown.length) {
  console.log("\n  the page threw while being photographed:\n   " + page.thrown.join("\n   "));
}
page.close();
console.log(`\n${n} shots in ${OUT}`);
