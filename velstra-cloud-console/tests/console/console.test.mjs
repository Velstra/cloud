// What the console has to actually do, checked in a browser against a running
// API.
//
// Read a failure here as "an operator cannot do X", not as unit noise. Most of
// these exist because the thing they check is invisible when it breaks: the
// page still loads, the layout is still right, and the answer on screen is
// quietly wrong or the button quietly does nothing.

import { browser, signIn, open, openRow, sheetText, sleep, waitFor, test, check, equal, skip, summary } from "./harness.mjs";
import { checkShapes } from "./shapes.mjs";

const URL = process.env.CONSOLE_URL || "http://127.0.0.1:18100/";
const TOKEN = process.env.CONSOLE_TOKEN || "testtoken";
const USERNAME = process.env.CONSOLE_USER || "operator";
const PASSWORD = process.env.CONSOLE_PASSWORD || "a test operator passphrase";
// The scaffolding endpoints only the in-memory API has. Against a real API
// these tests are skipped rather than faked.
const SCAFFOLDED = process.env.CONSOLE_SCAFFOLD !== "0";

const api = (path, init) => fetch(new globalThis.URL(path, URL), {
  ...init,
  headers: { Authorization: "Bearer " + TOKEN, "Content-Type": "application/json", ...(init || {}).headers },
});

// Before a browser is started, because it is not about the browser: it asks
// whether the fixture below still answers what the real API answers. A console
// tested against a fixture that has drifted is a console tested against
// nothing.
await test("the fixture answers the shapes the real API was recorded answering", async () => {
  const outcome = await checkShapes(URL, TOKEN);
  if (outcome.skipped) return skip(outcome.skipped);
  check(outcome.checked > 0, "the recording is empty, so this checked nothing");
  check(outcome.gaps.length === 0,
    "the fixture and the API no longer answer the same shape:\n  " + outcome.gaps.join("\n  ") +
    "\n  Re-record with UPDATE_SHAPES=1 cargo test -p velstra-cloud-api --test contract_shapes " +
    "if the API is what changed.");
});

const page = await browser();
await page.goto(URL);

const noThrows = (why) => check(page.thrown.length === 0, why + "\n  " + page.thrown.join("\n  "));

// These tests name objects by what is true of them, never by id. The suite has
// to mean the same thing against the in-memory contract server and against a
// real API with its own seed — and a check that cannot find its subject says so
// rather than passing.
const pick = async (what, predicate) => {
  const found = await page.evaluate(`(() => {
    const r = view.items.find((x) => (${predicate})(x));
    if (r) return { id: idOf(r), name: nameOf(r) };
    // Why there is nothing, not just that there is nothing. A board whose list
    // failed holds no items either, and reporting that as "the seed has none"
    // is the lie this suite is least able to afford: it turns a run in which
    // the API stopped answering into a run that says everything is fine. It has
    // already happened — one dropped fetch produced six skips and a "36/37
    // passed" over a migration screen nothing had looked at.
    const err = document.getElementById("listerr");
    const broke = err && !err.classList.contains("hidden") ? err.textContent : "";
    return { trouble: broke, board: view.coll ? view.coll.id : "(none)" };
  })()`);
  if (found.id) return found;
  if (found.trouble) {
    throw new Error(`the ${found.board} board is not listing at all, so nothing could be ` +
      `chosen to check: ${found.trouble}`);
  }
  skip(`this API's seed holds no ${what}`);
};

const ofKind = (kind) => `(x) => verdict(x).kind === ${JSON.stringify(kind)}`;
const gens = (id) => page.evaluate(`(() => {
  const r = view.items.find((x) => idOf(x) === ${JSON.stringify(id)});
  return { generation: generation(r), observed: observed(r), ready: condition(r, "Ready") };
})()`);

await test("the page loads without the script throwing", () => {
  noThrows("the console threw while loading");
});

await test("a bad token is refused, and says so", async () => {
  const said = await page.evaluate(`(async () => {
    document.getElementById("token").value = "not-the-token";
    document.getElementById("tokenform").dispatchEvent(new Event("submit", { cancelable: true }));
    await new Promise((r) => setTimeout(r, 700));
    return {
      inside: !document.getElementById("app").classList.contains("hidden"),
      said: document.getElementById("loginerr").textContent,
    };
  })()`);
  check(!said.inside, "a wrong token got in");
  check(/refused/i.test(said.said), `the refusal said "${said.said}"`);
});

await test("token-only sign-in survives native form validation", async () => {
  // The username and password inputs carry `required` for the password path. A
  // service account signs in with a token alone and leaves both blank, so the
  // form must not let native constraint validation reject that submit before
  // the handler can route it to the token.
  //
  // This drives the form through `requestSubmit()`, which runs native
  // validation exactly as a real click does — unlike the synthetic
  // `dispatchEvent(new Event("submit"))` the rest of the suite uses, which
  // bypasses validation and so cannot see this regression. A bad token keeps us
  // on the sign-in screen; the tell is whether the submit fired at all: if the
  // form validated the empty required fields it would never reach the API and
  // no refusal would appear.
  const said = await page.evaluate(`(async () => {
    const form = document.getElementById("tokenform");
    document.getElementById("username").value = "";
    document.getElementById("password").value = "";
    document.getElementById("token").value = "not-the-token";
    const novalidate = form.noValidate;
    form.requestSubmit();
    await new Promise((r) => setTimeout(r, 700));
    return {
      novalidate,
      inside: !document.getElementById("app").classList.contains("hidden"),
      said: document.getElementById("loginerr").textContent,
    };
  })()`);
  check(said.novalidate, "the sign-in form does not carry novalidate, so native "
    + "validation of the empty username/password blocks token-only sign-in");
  check(!said.inside, "a wrong token got in");
  check(/refused/i.test(said.said),
    "the token submit never reached the API — native validation swallowed it "
    + `before the handler ran (loginerr said "${said.said}")`);
});

await test("a wrong password is refused in the API's own words", async () => {
  const said = await page.evaluate(`(async () => {
    document.getElementById("token").value = "";
    document.getElementById("username").value = ${JSON.stringify(USERNAME)};
    document.getElementById("password").value = "not the password";
    document.getElementById("tokenform").dispatchEvent(new Event("submit", { cancelable: true }));
    await new Promise((r) => setTimeout(r, 700));
    return {
      inside: !document.getElementById("app").classList.contains("hidden"),
      said: document.getElementById("loginerr").textContent,
      leftBehind: document.getElementById("password").value,
    };
  })()`);
  check(!said.inside, "a wrong password got in");
  // The API refuses every cause in one sentence so the response is not a
  // username oracle. A console that translated it into something friendlier —
  // "no such user", "wrong password" — would undo that from the outside.
  check(/not accepted/i.test(said.said), `the refusal said "${said.said}"`);
  check(said.leftBehind === "not the password",
    "a failed sign-in cleared the field, so a typo means retyping the whole thing");
});

await signIn(page, { username: USERNAME, password: PASSWORD });

await test("the console says who is signed in", async () => {
  const chip = await page.evaluate(`document.getElementById("whoami").textContent`);
  // Name and standing both: an operator who cannot tell which account they are
  // using eventually does something as the wrong one.
  check(/Test Operator/.test(chip), `the header said "${chip}"`);
  check(/operator/.test(chip), `the header did not say this account holds the cell: "${chip}"`);
});

await test("the password field does not keep the password", async () => {
  // It sat in a DOM node for the life of the tab, where a screen-share or a
  // stray extension reads it back. Asserted after a *successful* sign-in,
  // because that is the path where clearing it is easy to forget.
  const left = await page.evaluate(`document.getElementById("password").value`);
  check(left === "", `the password field still held ${left.length} characters`);
});

// --- setting a password -----------------------------------------------------
//
// The "Set password" action on a user opens a dialog. For the operator's *own*
// account it must ask for and send the current password, because the API
// refuses a self-change that does not — a stolen session that could set a new
// password would otherwise be a permanent takeover. These drive that real
// dialog against the (fake) API's own password route.

await test("the password dialog sends current + new for a self-service change", async () => {
  // Mutating, and only meaningful against the in-memory API, whose password
  // route accepts the current one without changing anything real.
  if (!SCAFFOLDED) return skip("this API's password route is real, so a self-change would alter the account");
  const outcome = await page.evaluate(`(async () => {
    // The operator's own subject, so the dialog takes the "prove the current
    // one" shape and shows the current-password field it must send.
    openPasswordDialog(session.who.subject);
    const cur = document.getElementById("currentpassword");
    if (!document.getElementById("dialog") || !cur || !document.getElementById("newpassword")) {
      return { built: false, subject: (session.who || {}).subject };
    }
    cur.value = ${JSON.stringify(PASSWORD)};
    document.getElementById("newpassword").value = "a freshly chosen passphrase";
    document.getElementById("newpasswordagain").value = "a freshly chosen passphrase";
    document.getElementById("submitpassword").click();
    await new Promise((r) => setTimeout(r, 500));
    const err = document.querySelector("#dialog .err");
    return {
      built: true,
      // Success closes the dialog; the API refusing keeps it open with the reason.
      closed: !document.getElementById("dialog"),
      said: err ? err.textContent : "",
    };
  })()`);
  check(outcome.built,
    `the self-change dialog did not show the current-password field (own not detected; subject=${JSON.stringify(outcome.subject)})`);
  check(outcome.closed,
    `a self-change with the right current password did not go through: "${outcome.said}"`);
});

await test("the password dialog shows the API's refusal for a wrong current password", async () => {
  const outcome = await page.evaluate(`(async () => {
    openPasswordDialog(session.who.subject);
    const cur = document.getElementById("currentpassword");
    if (!cur) return { built: false, subject: (session.who || {}).subject };
    cur.value = "not the current one";
    document.getElementById("newpassword").value = "a freshly chosen passphrase";
    document.getElementById("newpasswordagain").value = "a freshly chosen passphrase";
    document.getElementById("submitpassword").click();
    await new Promise((r) => setTimeout(r, 500));
    const err = document.querySelector("#dialog .err");
    return {
      built: true,
      open: !!document.getElementById("dialog"),
      shown: err && !err.classList.contains("hidden"),
      said: err ? err.textContent : "",
    };
  })()`);
  check(outcome.built,
    `the self-change dialog did not ask for the current password (own not detected; subject=${JSON.stringify(outcome.subject)})`);
  check(outcome.open, "a refused password change closed the dialog instead of showing why");
  check(outcome.shown && /current password/i.test(outcome.said),
    `the refusal was not shown honestly: "${outcome.said}"`);
  await page.evaluate(`closeDialog()`);
});

await test("signing in lands on the overview, with the rail beside it", async () => {
  const seen = await page.evaluate(`({
    rail: [...document.querySelectorAll(".railitem")].map((b) => b.dataset.collection),
    schema: SCHEMA.map((c) => c.id),
    title: document.getElementById("listtitle").textContent,
    rows: document.querySelectorAll("#boardbody tr").length,
  })`);
  // Compared against the schema rather than against a number.
  //
  // It used to be a hand-kept count — 12, then 13, then 15, 16, 17, 18 as
  // screens arrived — and it sat at 13 through two of those. The comment here
  // said so: "a hand-kept number is only a guard if somebody runs it, and this
  // suite is not in `cargo test`. What it caught in the end was itself." It
  // then went stale a third time, which is enough evidence.
  //
  // The schema is in scope in the page, so the check can ask what *should* be
  // there instead of being told. Now a collection added without a rail entry
  // fails by name, and nobody has to remember a number.
  const missing = seen.schema.filter((id) => !seen.rail.includes(id));
  const extra = seen.rail.filter((id) => !seen.schema.includes(id));
  check(
    missing.length === 0 && extra.length === 0,
    `the rail does not match the schema — missing ${JSON.stringify(missing)}, ` +
      `unexpected ${JSON.stringify(extra)}`,
  );
  // The overview, not a board. Landing on one collection was landing on one
  // answer to a question nobody had asked yet — "is anything wrong" crosses
  // every collection there is.
  check(seen.title === "Overview", `landed on ${seen.title}`);
  const home = await page.evaluate(`({
    shown: !document.getElementById("overviewbox").classList.contains("hidden"),
    boardHidden: document.querySelector(".boardwrap").classList.contains("hidden"),
    text: document.getElementById("overviewbox").textContent,
    rail: !!document.getElementById("railhome"),
  })`);
  check(home.shown && home.boardHidden, "the overview is not what is on screen");
  check(home.rail, "there is no way back to the overview from the rail");
  // Whatever the cell's state, the page says which it is — an empty panel
  // reads as a page that failed to load.
  check(/Everything has settled/.test(home.text) ||
    (await page.evaluate(`document.querySelectorAll("#overviewbox .overrow").length`)) > 0,
    `the attention panel says nothing at all: ${home.text}`);
  // And the answers no listing carries: room, and what this tenant may start.
  await waitFor(page, `document.getElementById("overviewreports").textContent.includes("Largest")`);

  // A board is still one click away, and still full.
  await open(page, "instances");
  const rows = await page.evaluate(`document.querySelectorAll("#boardbody tr").length`);
  check(rows >= 1, "the board showed no instances at all");
});

// --- wayfinding -------------------------------------------------------------

await test("every section opens and shows its own board", async () => {
  const ids = await page.evaluate(`[...document.querySelectorAll(".railitem")].map((b) => b.dataset.collection)`);
  const empty = [], wrong = [];
  for (const id of ids) {
    await open(page, id);
    const seen = await page.evaluate(`({
      title: document.getElementById("listtitle").textContent,
      blurb: document.getElementById("listblurb").textContent,
      head: [...document.querySelectorAll("#boardhead th")].map((t) => t.textContent),
      rows: document.querySelectorAll("#boardbody tr").length,
      err: document.getElementById("listerr").classList.contains("hidden") ? "" :
           document.getElementById("listerr").textContent,
    })`);
    if (seen.err) wrong.push(`${id}: ${seen.err}`);
    if (!seen.blurb) wrong.push(`${id}: no blurb`);
    // The name column, then convergence. On a board that offers a bulk action
    // there is a picker column in front of both, and it carries no label.
    const named = seen.head.filter((t) => t !== "");
    if (named[1] !== "Convergence") wrong.push(`${id}: no convergence column`);
    if (!seen.rows) empty.push(id);
  }
  check(!wrong.length, "sections that did not render:\n  " + wrong.join("\n  "));
  // A collection may legitimately be empty. All ten empty means the console is
  // listing from the wrong place entirely, which is the failure this catches.
  check(empty.length < ids.length, "every collection listed nothing: " + empty.join(", "));
  noThrows("opening the sections threw");
});

await test("a node is found even though it is not under a project", async () => {
  await open(page, "nodes");
  const rows = await page.evaluate(`document.querySelectorAll("#boardbody tr").length`);
  check(rows >= 1, "the fleet showed no nodes");
  const scope = await page.evaluate(`scopeFound.nodes || collection("nodes").scope`);
  equal(scope, "global", "nodes were not addressed globally");
});

await test("a collection whose scope the contract spells differently is still found", async () => {
  // The contract does not say where a node is addressed from. The console
  // declares one and falls back to the other once, so being wrong about it
  // costs a round trip rather than showing an empty fleet.
  //
  // **In one direction only.** A collection declared global whose objects turn
  // out to live under a project is found. The reverse is not probed: an empty
  // answer for a project collection is what a project looks like on its first
  // day, and asking the cell-wide path instead returns every other tenant's
  // rows to anybody who may read the cell — which is how a new customer's first
  // guest came to be created outside their project entirely.
  const found = await page.evaluate(`(async () => {
    const coll = collection("instances");
    const was = scopeFound.instances;
    delete scopeFound.instances;
    const original = coll.scope;
    coll.scope = "global";                // deliberately the wrong one
    try {
      const r = await list(coll);
      return { items: r.items.length, resolved: scopeFound.instances };
    } finally { coll.scope = original; scopeFound.instances = was; }
  })()`);
  check(found.items >= 1, "the fallback found no instances");
  equal(found.resolved, "project", "the fallback did not remember where they really are");
});

// --- convergence, the whole point -------------------------------------------

await test("a settled object reads as settled", async () => {
  await open(page, "instances");
  const it = await pick("settled instance", ofKind("settled"));
  await openRow(page, it.id);
  const g = await gens(it.id);
  const text = await sheetText(page);
  check(/Settled/.test(text), `${it.id} does not read as settled:\n` + text.slice(0, 400));
  check(new RegExp(`Asked at\\s*${g.generation}`, "i").test(text) &&
        new RegExp(`Observed at\\s*${g.observed}`, "i").test(text),
    `the two generations (${g.generation}/${g.observed}) are not both shown`);
});

await test("a drifting object shows the gap, and the reason for it", async () => {
  await open(page, "instances");
  const it = await pick("drifting instance", ofKind("drifting"));
  await openRow(page, it.id);
  const g = await gens(it.id);
  const text = await sheetText(page);
  check(/Drifting/.test(text), `${it.id} does not read as drifting`);
  check(text.includes(`generation ${g.generation}`) && text.includes(`reported at ${g.observed}`),
    "the gap is not stated in generations:\n" + text.slice(0, 500));
  // A drifting object need not carry a Ready condition — but if it does, the
  // reason, the sentence and the staleness all have to be on the page. That is
  // the whole difference between this console and a list of names.
  if (g.ready) {
    check(text.includes(g.ready.reason), "the Ready reason is missing");
    if (g.ready.message) check(text.includes(g.ready.message), "the sentence the agent wrote is missing");
    const at = g.ready.observedGeneration ?? g.ready.observed_generation;
    if (at < g.generation) {
      check(text.includes(`recorded at generation ${at}`),
        "a condition written about an older generation is not marked stale");
    }
  }
});

await test("a failing object leads with the reason, not with a spinner", async () => {
  await open(page, "instances");
  const it = await pick("failing instance", ofKind("failing"));
  await openRow(page, it.id);
  const g = await gens(it.id);
  const text = await sheetText(page);
  check(/Failing/.test(text), `${it.id} does not read as failing`);
  check(text.includes(g.ready.reason), "the machine reason is missing");
  check(!g.ready.message || text.includes(g.ready.message), "the operator's sentence is missing");
});

await test("a failed placement shows the rejection chain per node", async () => {
  await open(page, "instances");
  // Whatever is not settled — an operator looking at one of these is looking at
  // it precisely because it did not come up.
  const it = await pick("unsettled instance", `(x) => verdict(x).kind !== "settled"`);
  const answer = await (await api("/api/v1/" + it.name + ":explainPlacement")).json().catch(() => null);
  if (!answer || answer.error) skip("this API does not answer :explainPlacement");
  await openRow(page, it.id);
  await sleep(800);
  const text = await sheetText(page);
  check(answer.placed ? /Placed on/.test(text) : /Not placed/.test(text),
    "the sheet does not say whether it was placed:\n" + text.slice(0, 700));
  for (const r of answer.rejected || []) {
    check(text.includes(String(r.node)) && text.includes(String(r.why)),
      `the rejection of ${r.node} (${r.why}) is not on the page`);
    if (r.detail) check(text.includes(String(r.detail)), `the detail behind ${r.why} is missing`);
  }
});

await test("asked and is are shown side by side, and the disagreement is named", async () => {
  await open(page, "volumes");
  const it = await pick("volume whose size has not been provisioned yet",
    `(x) => pick(spec(x), "sizeGib") !== pick(status(x), "actualSizeGib")`);
  await openRow(page, it.id);
  const seen = await page.evaluate(`(() => {
    const rows = [...document.querySelectorAll("#sheet table.pairs tbody tr")];
    return {
      differs: rows.filter((r) => r.classList.contains("differs")).length,
      text: document.getElementById("sheet").innerText,
    };
  })()`);
  check(seen.differs === 1, `${seen.differs} pairs were marked as differing`);
  check(/pool has not finished growing/.test(seen.text),
    "a disagreement is shown without saying what it means");
});

await test("an object that agrees says so rather than staying silent", async () => {
  await open(page, "volumes");
  const it = await pick("volume that is the size it was asked to be",
    `(x) => pick(spec(x), "sizeGib") === pick(status(x), "actualSizeGib")`);
  await openRow(page, it.id);
  check(/agrees/.test(await sheetText(page)), "a pair that agrees is not marked");
});

await test("a name on an object is somewhere you can go", async () => {
  await open(page, "attachments");
  const it = await pick("attachment", `(x) => !!pick(spec(x), "volume")`);
  const volume = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(it.id)});
    return pick(spec(r), "volume");
  })()`);
  await openRow(page, it.id);
  const went = await page.evaluate(`(async () => {
    const link = [...document.querySelectorAll("#sheet button.link")]
      .find((b) => b.title === ${JSON.stringify(volume)});
    if (!link) return { found: false };
    link.click();
    await new Promise((r) => setTimeout(r, 900));
    return {
      found: true,
      list: document.getElementById("listtitle").textContent,
      sheet: document.getElementById("sheet") ? document.getElementById("sheet").innerText.slice(0, 40) : "",
    };
  })()`);
  check(went.found, "the volume an attachment names is not a link");
  equal(went.list, "Volumes", "following the name did not change the board");
  check(went.sheet.includes(volume.split("/").pop()), `it opened "${went.sheet}"`);
  await page.evaluate(`closeSheet()`);
});

await test("the rail says where the trouble is without opening anything", async () => {
  const counts = await page.evaluate(`(() => {
    const item = [...document.querySelectorAll(".railitem")].find((b) => b.dataset.collection === "instances");
    return { text: item.innerText, fleet: document.getElementById("fleet").innerText,
             unsettled: Object.values(census).reduce((n, c) => n + c.unsettled, 0) };
  })()`);
  check(/\d/.test(counts.text), `the instances rail entry shows no count: "${counts.text}"`);
  check(counts.unsettled ? /not settled/.test(counts.fleet) : /all settled/.test(counts.fleet),
    `${counts.unsettled} objects are not settled and the header says "${counts.fleet}"`);
});

// --- forms ------------------------------------------------------------------

await test("a constrained value is chosen, never typed", async () => {
  await open(page, "instances");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(500);
  const shape = await page.evaluate(`({
    image: document.getElementById("f-image").tagName,
    vcpus: document.querySelector("#f-vcpus").type,
    stepper: !!document.getElementById("f-vcpus").closest(".stepper"),
    unit: document.getElementById("f-memoryMib").closest(".stepper").querySelector(".unit").textContent,
    power: [...document.querySelectorAll("#f-desiredState button")].map((b) => b.textContent),
    imageOptions: [...document.querySelectorAll("#f-image option")].map((o) => o.value).filter(Boolean).length,
    more: !!document.getElementById("moresettings"),
  })`);
  equal(shape.image, "SELECT", "the image is not picked");
  check(shape.stepper && shape.vcpus === "number", "vCPUs is not a stepper");
  equal(shape.unit, "MiB", "the memory field does not carry its unit");
  equal(shape.power, ["Running", "Stopped"], "power is not a segmented choice");
  check(shape.imageOptions >= 1, "the image picker offers nothing");
  check(shape.more, "there is no second level for the advanced settings");
  noThrows("opening the create form threw");
});

await test("a boolean is a switch, and it flips", async () => {
  await page.evaluate(`document.getElementById("cancelform").click()`);
  await open(page, "attachments");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(400);
  const flipped = await page.evaluate(`(() => {
    const s = document.getElementById("f-readOnly");
    const before = s.getAttribute("aria-checked");
    s.click();
    return { tag: s.tagName, role: s.getAttribute("role"), before, after: s.getAttribute("aria-checked") };
  })()`);
  equal(flipped.role, "switch", "read-only is not a switch");
  check(flipped.before !== flipped.after, "the switch did not flip");
});

await test("choosing an instance fills in the node it is on", async () => {
  // Only an instance that has been placed can hand a node over; on a cell whose
  // scheduler has not run yet there is nothing to derive.
  const placed = await page.evaluate(`(async () => {
    const all = await options("instances");
    const r = all.find((x) => at(status(x), "node") || at(spec(x), "node"));
    return r ? { name: nameOf(r), node: at(status(r), "node") || at(spec(r), "node") } : null;
  })()`);
  if (!placed) skip("this API's seed holds no instance that has been placed on a node");
  const got = await page.evaluate(`(async () => {
    const s = document.getElementById("f-instance");
    s.value = ${JSON.stringify(placed.name)};
    s.dispatchEvent(new Event("change"));
    await new Promise((r) => setTimeout(r, 300));
    return document.getElementById("f-node").value;
  })()`);
  equal(got, placed.node, "the node was not taken from the instance");
});

await test("attaching to an instance that was never placed says why, at the choice", async () => {
  // An attachment needs a node — that is what opens the volume — and an
  // instance the scheduler could not place has none to give. The form has to
  // say that where the choice was made, not leave a required field the
  // operator is staring at with nothing legal to put in it.
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "instances");
  const unplaced = await pick("instance that was never placed",
    `(x) => !at(status(x), "node") && !at(spec(x), "node")`);
  await open(page, "attachments");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  const said = await page.evaluate(`(async () => {
    const s = document.getElementById("f-instance");
    if (!s || ![...s.options].some((o) => o.value === ${JSON.stringify(unplaced.name)})) return null;
    s.value = ${JSON.stringify(unplaced.name)};
    s.dispatchEvent(new Event("change"));
    await new Promise((r) => setTimeout(r, 400));
    const box = document.getElementById("f-node").closest(".field").querySelector(".err");
    return { node: document.getElementById("f-node").value, why: box.textContent };
  })()`);
  if (said === null) skip("the attachment form does not offer that instance");
  equal(said.node, "", "a node was invented for an instance that has none");
  check(/not been placed/.test(said.why),
    `the form said "${said.why}" instead of why there is no node`);
  await page.evaluate(`document.getElementById("cancelform").click()`);
});

await test("a reference is sent the way the platform spells it", async () => {
  // Opens its own form rather than inheriting the one a previous test left up:
  // against a seed where an earlier check skipped, the inherited form is not
  // there, and a test that assumes it fails as a crash instead of a skip.
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "attachments");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(600);

  const offered = await page.evaluate(`(() => {
    const values = (id) => {
      const s = document.getElementById(id);
      return s ? [...s.options].map((o) => o.value).filter(Boolean) : null;
    };
    return { node: values("f-node"), volume: values("f-volume"), instance: values("f-instance") };
  })()`);
  for (const [what, values] of Object.entries(offered)) {
    if (values === null) skip(`the attachment form has no ${what} control`);
    if (!values.length) skip(`this API's seed holds no ${what} to attach`);
  }

  // A node is a bare id and everything else is a full resource name — the
  // scheduler writes `node-a` into `spec.node` and ownership is decided by
  // string equality against it, so a picker offering `nodes/node-a` builds an
  // object assigned to a node that does not answer to it. The API refuses the
  // wrong spelling outright, which is why this is checked on the way out.
  check(offered.node.every((v) => !v.includes("/")),
    "a node is offered as a full name: " + offered.node.join(", "));
  for (const what of ["volume", "instance"]) {
    check(offered[what].every((v) => v.includes("/") && v.split("/").length % 2 === 0),
      `a ${what} is offered as a bare id: ` + offered[what].join(", "));
  }

  // And it is really accepted: both servers refuse the wrong spelling by field,
  // so a create that goes through is the strongest form this can be checked in.
  // Only an instance that has been placed can be attached to anything: the node
  // is what opens the volume, and an unplaced instance has none to give.
  const placed = await page.evaluate(`(async () => {
    const all = await options("instances");
    const r = all.find((x) => at(status(x), "node") || at(spec(x), "node"));
    return r ? nameOf(r) : null;
  })()`);
  if (!placed) skip("this API's seed holds no instance that has been placed on a node");

  const id = "consoletest-" + Math.random().toString(36).slice(2, 8);
  await page.evaluate(`(async () => {
    const set = (el, v) => { el.value = v; el.dispatchEvent(new Event(el.tagName === "SELECT" ? "change" : "input")); };
    set(document.getElementById("f-id"), ${JSON.stringify(id)});
    set(document.getElementById("f-volume"), ${JSON.stringify(offered.volume[0])});
    set(document.getElementById("f-instance"), ${JSON.stringify(placed)});
    await new Promise((r) => setTimeout(r, 400));
    document.getElementById("submitform").click();
  })()`);
  const made = await waitFor(page, `(() => {
    if (document.getElementById("dialog")) return null;
    return [...document.querySelectorAll("#boardbody tr")]
      .some((r) => r.dataset.name.endsWith("/" + ${JSON.stringify(id)})) || null;
  })()`);
  if (!made) {
    const said = await page.evaluate(`document.querySelector("#dialog p.err")?.textContent || ""`);
    check(false, said ? `the attachment was refused: ${said}` : "the attachment never appeared on the board");
  }
  await page.evaluate(`closeSheet()`);
});

await test("a picker offers only what belongs", async () => {
  await page.evaluate(`document.getElementById("cancelform")?.click()`);
  await open(page, "ports");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(500);
  const before = await page.evaluate(`(() => {
    const s = document.getElementById("f-subnet");
    return { placeholder: s.options[0].textContent, count: s.options.length };
  })()`);
  check(/network first/.test(before.placeholder),
    `the subnet picker said "${before.placeholder}" before a network was chosen`);
  equal(before.count, 1, "the subnet picker offered subnets of no network at all");

  const network = await page.evaluate(`(() => {
    const n = [...document.getElementById("f-network").options].map((o) => o.value).filter(Boolean)[0];
    return n || null;
  })()`);
  if (!network) skip("this API's seed holds no network to filter by");
  const seen = await page.evaluate(`(async () => {
    const n = document.getElementById("f-network");
    n.value = ${JSON.stringify(network)};
    n.dispatchEvent(new Event("change"));
    await new Promise((r) => setTimeout(r, 500));
    const all = await options("subnets");
    return {
      offered: [...document.getElementById("f-subnet").options].map((o) => o.value).filter(Boolean),
      belong: all.filter((x) => at(spec(x), "network") === ${JSON.stringify(network)}).map(nameOf),
    };
  })()`);
  equal(seen.offered, seen.belong, "the subnet picker does not follow the network");
});

await test("validation arrives while typing, not on submit", async () => {
  const said = await page.evaluate(`(() => {
    const more = document.getElementById("moresettings");
    if (more) more.click();
    const a = document.getElementById("f-address");
    a.value = "10.20.0.999";
    a.dispatchEvent(new Event("input"));
    const box = a.closest(".field").querySelector(".err");
    return { message: box.textContent, hidden: box.classList.contains("hidden"), marked: a.classList.contains("bad") };
  })()`);
  check(!said.hidden && /not an address/.test(said.message),
    `a bad address said "${said.message}" before anything was submitted`);
  check(said.marked, "the offending control is not marked");
});

await test("an id that would be mis-split downstream is refused before it is sent", async () => {
  const said = await page.evaluate(`(() => {
    const i = document.getElementById("f-id");
    i.value = "Web/1";
    i.dispatchEvent(new Event("input"));
    return document.getElementById("f-id-err").textContent;
  })()`);
  check(/lowercase/.test(said), `a bad id said "${said}"`);
});

await test("a gateway outside its own range is caught, and only once both exist", async () => {
  await page.evaluate(`document.getElementById("cancelform").click()`);
  await open(page, "subnets");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(300);
  const said = await page.evaluate(`(() => {
    const g = document.getElementById("f-gateway");
    g.value = "10.20.0.1"; g.dispatchEvent(new Event("input"));
    const alone = g.closest(".field").querySelector(".err").textContent;
    const c = document.getElementById("f-cidr");
    c.value = "10.99.0.0/24"; c.dispatchEvent(new Event("input"));
    return { alone, together: g.closest(".field").querySelector(".err").textContent };
  })()`);
  equal(said.alone, "", "a gateway was judged before there was a range to judge it against");
  check(/outside 10.99.0.0\/24/.test(said.together), `the pair said "${said.together}"`);
});

const MADE = "scratch-" + Math.random().toString(36).slice(2, 8);

await test("a resource can be created, and appears", async () => {
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "volumes");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(300);
  await page.evaluate(`(() => {
    const id = document.getElementById("f-id");
    id.value = ${JSON.stringify(MADE)}; id.dispatchEvent(new Event("input"));
    const size = document.getElementById("f-sizeGib");
    size.value = "25"; size.dispatchEvent(new Event("input"));
    const pool = document.getElementById("f-pool");
    pool.value = "nvme"; pool.dispatchEvent(new Event("input"));
    document.getElementById("submitform").click();
  })()`);
  await sleep(1500);
  const seen = await page.evaluate(`({
    dialog: !!document.getElementById("dialog"),
    rows: [...document.querySelectorAll("#boardbody tr")].map((r) => r.dataset.name.split("/").pop()),
    sheet: document.getElementById("sheet") ? document.getElementById("sheet").innerText : "",
  })`);
  check(!seen.dialog, "the form stayed open after a successful create");
  check(seen.rows.includes(MADE), "the new volume is not on the board: " + seen.rows.join(", "));
  // Created and honest about it. Whatever else it says, it may not claim an
  // agent has looked at something no agent can have looked at yet.
  check(!/Settled/.test(seen.sheet),
    "a brand new object claims to be settled before anything reported on it");
  noThrows("creating threw");
});

await test("what the API refuses lands on the control that caused it", async () => {
  await page.evaluate(`document.getElementById("closesheet")?.click()`);
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(300);
  const said = await page.evaluate(`(async () => {
    const id = document.getElementById("f-id");
    id.value = ${JSON.stringify(MADE)}; id.dispatchEvent(new Event("input"));   // taken
    const pool = document.getElementById("f-pool");
    pool.value = "nvme"; pool.dispatchEvent(new Event("input"));
    document.getElementById("submitform").click();
    await new Promise((r) => setTimeout(r, 900));
    return {
      open: !!document.getElementById("dialog"),
      onField: document.getElementById("f-id-err").textContent,
      banner: document.querySelector("#dialog p.err")?.textContent || "",
    };
  })()`);
  check(said.open, "a refused create closed the form and lost what was typed");
  // The API names the offending field; the message is its own words, and this
  // checks only that they arrived on the control rather than in a banner.
  check(said.onField.length > 0,
    `the refusal landed in the banner ("${said.banner}") instead of on the id`);
  await page.evaluate(`document.getElementById("cancelform").click()`);
});

// --- live --------------------------------------------------------------------

await test("the watch delivers a change made elsewhere, without a reload", async () => {
  await open(page, "instances");
  const it = await pick("instance", `(x) => true`);
  const was = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(it.id)});
    return Number(pick(spec(r), "vcpus") || 1);
  })()`);
  const wanted = was + 1;
  const patched = await api("/api/v1/" + it.name, {
    method: "PATCH",
    body: JSON.stringify({ spec: { vcpus: wanted } }),
  });
  check(patched.ok, `the API refused the outside change (${patched.status})`);

  // What this test is about, and all it may assert: a change nobody made in
  // this tab arrives in the board without a reload. It deliberately says
  // nothing about the verdict afterwards — against a live cell the node agent
  // converges the change while the assertion is being written, so "it now reads
  // Drifting" is a claim about a system where nothing reconciles. The drift
  // rendering is checked on an object that is actually drifting, above.
  const arrived = await waitFor(page, `(() => {
    const row = [...document.querySelectorAll("#boardbody tr")]
      .find((r) => r.dataset.name === ${JSON.stringify(it.name)});
    return row && row.innerText.includes(${JSON.stringify(String(wanted))}) ? row.innerText : null;
  })()`);
  check(arrived, `the new vCPU count never arrived on the board`);
  const live = await page.evaluate(`document.getElementById("watchstate").textContent`);
  check(/live/.test(live), `the watch reports "${live}"`);
});

await test("an open sheet follows a change all the way to settled", async () => {
  // The subject has to be an object that *can* settle. Picking any instance
  // grabs whichever the store lists first, and a cell seeded with a deliberate
  // failure lists it first — so the check reported "nothing ever settles" about
  // the one object in the cell designed never to. Anything not permanently
  // failing will do: it need not be settled right now, only able to become so.
  await open(page, "instances");
  const it = await pick("instance that is not permanently failing",
    `(x) => verdict(x).kind !== "failing"`);
  const before = await gens(it.id);
  await openRow(page, it.id);

  const vcpus = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(it.id)});
    return Number(pick(spec(r), "vcpus") || 1);
  })()`);
  const patched = await api("/api/v1/" + it.name, {
    method: "PATCH", body: JSON.stringify({ spec: { vcpus: vcpus + 1 } }),
  });
  check(patched.ok, `the API refused the change (${patched.status})`);
  // Nothing reconciles against the in-memory contract server, so the report an
  // agent would write is asked for. In a live cell one arrives on its own.
  if (SCAFFOLDED) await api("/__test/converge?name=" + it.name);

  const settled = await waitFor(page, `(() => {
    const r = view.items.find((x) => nameOf(x) === ${JSON.stringify(it.name)});
    if (!r || generation(r) <= ${before.generation} || verdict(r).kind !== "settled") return null;
    const text = document.getElementById("sheet") ? document.getElementById("sheet").innerText : "";
    return text.includes("Settled") ? { text, generation: generation(r), observed: observed(r) } : null;
  })()`, { timeout: 20000 });
  if (!settled) skip("nothing in this cell reports on an object, so a change never settles");
  check(settled.generation > before.generation, "the generation never moved");
  equal(settled.observed, settled.generation, "settled with the two generations still apart");
  check(new RegExp(`Observed at\\s*${settled.generation}`, "i").test(settled.text),
    "the open sheet still shows an older observed generation");
  noThrows("following a change to settled threw");
});

await test("deleting asks once, and then says what is holding the object", async () => {
  await page.evaluate(`closeSheet()`);
  await open(page, "attachments");
  // Only an object the platform holds a finalizer on can show the two-phase
  // delete, and this really does delete it.
  const it = await pick("attachment to delete",
    `(x) => (pick(meta(x), "finalizers") || []).length && !deletedAt(x)`);
  await openRow(page, it.id);
  const asked = await page.evaluate(`(() => {
    document.getElementById("deletebtn").click();
    return !!document.getElementById("confirmdelete");
  })()`);
  check(asked, "delete went ahead without asking");
  await page.evaluate(`document.getElementById("confirmdelete").click()`);
  await sleep(1200);
  await openRow(page, it.id);
  const text = await sheetText(page);
  check(/Deleting/.test(text), "an object with a finalizer did not read as deleting after a delete");
  check(/velstra.io/.test(text), "the finalizer holding it is not named:\n" + text.slice(0, 400));
});

// --- moving a guest ----------------------------------------------------------
//
// The failure these are written against is a screen that invents a state: an
// instance that reads "MIGRATING" and stays that way, a spinner that outlives
// the receiver behind it, a percentage against a total nobody promised. Each
// check below is one of those, phrased as what an operator would be told.

/// A running guest that is not already moving — the one a migration can be
/// started from. Named by what is true of it, so it means the same thing
/// against another seed.
const migratable = async () => {
  await page.evaluate(`closeSheet(); document.getElementById("cancelform")?.click();`);
  await open(page, "instances");
  const found = await page.evaluate(`(async () => {
    let busy = new Set();
    try {
      // A migration that has arrived is history, not something in flight: the
      // guest it names can be moved again.
      const ms = (await list(collection("migrations"))).items;
      busy = new Set(ms
        .filter((m) => !deletedAt(m) && condition(m, "Moved")?.status !== "True")
        .map((m) => String(pick(spec(m), "instance"))));
    } catch (e) {
      // Not "no guest": the question was never asked. Swallowing this into a
      // skip is how a run in which the API stopped answering reports itself as
      // a run whose cell simply had nothing to move.
      return { trouble: String(e.message || e) };
    }
    const r = view.items.find((x) => at(status(x), "node") && !busy.has(nameOf(x)));
    return r ? { id: idOf(r), name: nameOf(r), node: at(status(r), "node") } : {};
  })()`);
  if (found.trouble) {
    throw new Error("the migrations could not be read, so no guest could be chosen to move: "
      + found.trouble);
  }
  if (!found.id) skip("this API's seed holds no running guest that is not already moving");
  return found;
};

/// A migration of this check's own, started through the API rather than the
/// dialog, for the checks that have to change one.
///
/// A check that mutates its subject must own it. Choosing one out of the seed by
/// what is true of it is right for reading — it is what makes these mean the
/// same thing against another API — and wrong the moment the check writes,
/// because the predicate can land on the object another check is in the middle
/// of, or on one a check above already removed. Made here, named after this
/// suite, and removed by the caller.
const ownMigration = async () => {
  await page.evaluate(`closeSheet()`);
  await open(page, "instances");
  const guest = await page.evaluate(`(() => {
    // Placed *and running*. A guest that is merely placed can be one that
    // failed, and a failed guest is refused by every destination — which
    // reads as "this cell can receive nothing" and skips the whole test.
    const r = view.items.find(
      (x) => at(status(x), "node") && at(status(x), "state") === "Running",
    );
    return r ? { name: nameOf(r), node: at(status(r), "node") } : null;
  })()`);
  if (!guest) skip("this cell holds no placed guest to move");
  // The platform's own answer about this guest, so the destination is one it has
  // said yes to rather than one this file assumed.
  const asked = await api("/api/v1/" + guest.name + ":explainMigration");
  check(asked.ok, `the API would not say where ${guest.name} could go (${asked.status})`);
  const to = ((await asked.json()).destinations || []).find((d) => d.allowed);
  if (!to) skip("no node in this cell can receive that guest");
  const project = guest.name.split("/").slice(0, 2).join("/");
  const id = "consoletest-expiring-" + Math.random().toString(36).slice(2, 6);
  const made = await api("/api/v1/" + project + "/migrations", {
    method: "POST",
    body: JSON.stringify({ id, spec: { instance: guest.name, toNode: to.node,
      mode: "Live", downtimeMs: 300, timeoutS: 3600, connections: 1 } }),
  });
  if (!made.ok) {
    check(false, `the API would not start a migration to time out (${made.status}): ` +
      JSON.stringify(await made.json()));
  }
  await open(page, "migrations");
  return { id, name: project + "/migrations/" + id, node: guest.node };
};

/// Open the migrate dialog on that guest.
const openMigrateOn = async (it) => {
  await openRow(page, it.id);
  const there = await waitFor(page, `!!document.getElementById("migratebtn")`);
  if (!there) skip("this console offers no migration for " + it.id);
  await page.evaluate(`document.getElementById("migratebtn").click()`);
  const ready = await waitFor(page, `(() => {
    const s = document.getElementById("f-toNode");
    return s && s.options.length > 1 ? 1 : null;
  })()`);
  if (!ready) {
    const said = await page.evaluate(`document.querySelector("#dialog .candidates")?.innerText || ""`);
    skip("the destination picker was never filled: " + said);
  }
};

await test("a migration is started where the guest is, not from a page of its own", async () => {
  await open(page, "migrations");
  const seen = await page.evaluate(`({
    create: !!document.getElementById("newbtn"),
    blurb: document.getElementById("listblurb").textContent,
    rows: document.querySelectorAll("#boardbody tr").length,
  })`);
  check(!seen.create, "migrations are offered as a blank form, with no guest to answer about");
  check(/from the instance/i.test(seen.blurb), `the list says "${seen.blurb}"`);
  check(seen.rows >= 0, "the migrations board did not render");
  noThrows("opening the migrations board threw");
});

await test("the destination list greys out a node that cannot receive, and says why", async () => {
  const it = await migratable();
  await openMigrateOn(it);
  const seen = await page.evaluate(`(() => {
    const s = document.getElementById("f-toNode");
    const opts = [...s.options].filter((o) => o.value);
    return {
      usable: opts.filter((o) => !o.disabled).map((o) => o.value),
      refused: opts.filter((o) => o.disabled).map((o) => o.value),
      reasons: document.querySelector("#dialog .candidates")?.innerText || "",
      free: !!document.querySelector("#dialog input[id='f-toNode']"),
    };
  })()`);
  check(!seen.free, "the destination is a text box");
  check(!seen.usable.includes(it.node),
    "the node the guest is already on is offered as a destination: " + seen.usable.join(", "));
  if (!seen.refused.length) skip("every node in this cell can receive that guest");
  // Out, and never silently: a destination missing without a reason sends
  // somebody to a log file to find out why they cannot pick it.
  for (const node of seen.refused) {
    check(seen.reasons.includes(node), `${node} is greyed out with no reason given`);
  }
  check(seen.reasons.split("\n").filter(Boolean).length >= seen.refused.length,
    "the refusals are listed without saying anything:\n" + seen.reasons);
});

/// The mode is not a detail behind the destination — it decides which
/// destinations there are.
///
/// A live move carries the guest's memory across while it runs, so a processor
/// it has already been told about has to keep working; a cold move stops it and
/// starts it, and the guest reads the CPUID it is given there like any freshly
/// booted machine. Found on two real machines whose CPUs differed by a handful
/// of flags: every destination was greyed out, and every one of them could have
/// taken the guest with a restart.
///
/// So the picker is asked again when the mode changes. A picker filled once,
/// live, is a fleet of unlike machines told its guests cannot move at all.
await test("changing the mode asks the platform again, and a cold move reaches further", async () => {
  const it = await migratable();
  await openMigrateOn(it);
  const live = await page.evaluate(`(() => {
    const s = document.getElementById("f-toNode");
    return [...s.options].filter((o) => o.value && !o.disabled).map((o) => o.value);
  })()`);

  const cold = await page.evaluate(`(async () => {
    // Three modes, so the control is a segmented row of buttons and not a
    // select: assigning to its value would do nothing at all, quietly.
    document.querySelector('#f-mode [data-value=Reboot]').click();
    // The picker is refilled from an answer that has to be fetched. Waited for
    // by the thing that is supposed to change, not by a clock.
    const open = () => [...document.getElementById("f-toNode").options]
      .filter((o) => o.value && !o.disabled).map((o) => o.value);
    for (let i = 0; i < 100; i++) {
      await new Promise((r) => setTimeout(r, 100));
      if (open().includes("node-d")) break;
    }
    return open();
  })()`);

  // node-d is a generation behind on its processor and holds the image: nothing
  // but the CPU stands between it and this guest, and only a live move cares.
  check(!live.includes("node-d"),
    `a live move was offered a machine that cannot present the guest's cpu: ${live.join(", ")}`);
  check(cold.includes("node-d"),
    `a cold move was still refused the machine only a live one cannot use: ${cold.join(", ")}`);

  // And back, so the answer follows the question rather than only widening.
  const again = await page.evaluate(`(async () => {
    document.querySelector('#f-mode [data-value=Live]').click();
    for (let i = 0; i < 100; i++) {
      await new Promise((r) => setTimeout(r, 100));
      const d = [...document.getElementById("f-toNode").options]
        .find((o) => o.value === "node-d");
      if (d && d.disabled) return true;
    }
    return false;
  })()`);
  check(again, "switching back to a live move kept offering the machine it cannot use");

  const said = await page.evaluate(`document.querySelector("#dialog .candidates")?.innerText || ""`);
  check(/cpu/i.test(said), `the refusal does not say it is about the processor: ${said}`);
});

await test("a destination that stops being possible is refused at the control, in the API's words", async () => {
  // The dialog from the previous test is still up. This is the race the answer
  // cannot close on its own: the node was fine when it was asked about and is
  // not fine when Migrate is pressed, so the refusal has to land where the
  // choice was made.
  const dest = await page.evaluate(`(() => {
    const s = document.getElementById("f-toNode");
    if (!s) return null;
    const o = [...s.options].find((x) => x.value && !x.disabled);
    return o ? o.value : null;
  })()`);
  if (!dest) skip("no destination could be chosen to begin with");
  const drained = await api("/api/v1/nodes/" + dest, {
    method: "PATCH", body: JSON.stringify({ spec: { schedulable: false } }),
  });
  if (!drained.ok) skip(`this API would not drain ${dest} (${drained.status})`);
  try {
    const said = await page.evaluate(`(async () => {
      const s = document.getElementById("f-toNode");
      s.value = ${JSON.stringify(dest)};
      s.dispatchEvent(new Event("change"));
      document.getElementById("f-id").value = "consoletest-refused";
      document.getElementById("f-id").dispatchEvent(new Event("input"));
      await new Promise((r) => setTimeout(r, 200));
      document.getElementById("submitform").click();
      await new Promise((r) => setTimeout(r, 1200));
      const field = document.getElementById("f-toNode").closest(".field");
      return {
        open: !!document.getElementById("dialog"),
        onField: field.querySelector(".err").textContent,
        banner: document.querySelector("#dialog > p.err")?.textContent || "",
      };
    })()`);
    check(said.open, "a refused migration closed the form and lost what was chosen");
    check(said.onField.length > 0,
      `the refusal landed in the banner ("${said.banner}") instead of on the destination`);
    check(/not accepting work|drain/i.test(said.onField),
      `the control says "${said.onField}" rather than the API's own sentence`);
  } finally {
    await api("/api/v1/nodes/" + dest, {
      method: "PATCH", body: JSON.stringify({ spec: { schedulable: true } }),
    });
    await page.evaluate(`document.getElementById("cancelform")?.click()`);
  }
});

const MOVED = "consoletest-" + Math.random().toString(36).slice(2, 6);

await test("a migration can be started, and lands on the object that follows it", async () => {
  const it = await migratable();
  await openMigrateOn(it);
  const chosen = await page.evaluate(`(async () => {
    const s = document.getElementById("f-toNode");
    const o = [...s.options].find((x) => x.value && !x.disabled);
    if (!o) return null;
    s.value = o.value; s.dispatchEvent(new Event("change"));
    const id = document.getElementById("f-id");
    id.value = ${JSON.stringify(MOVED)}; id.dispatchEvent(new Event("input"));
    await new Promise((r) => setTimeout(r, 200));
    document.getElementById("submitform").click();
    return o.value;
  })()`);
  if (!chosen) skip("no node in this cell can receive that guest");
  const made = await waitFor(page, `(() => {
    if (document.getElementById("dialog")) return null;
    return [...document.querySelectorAll("#boardbody tr")]
      .some((r) => r.dataset.name.endsWith("/" + ${JSON.stringify(MOVED)})) || null;
  })()`);
  if (!made) {
    const said = await page.evaluate(`document.querySelector("#dialog p.err")?.textContent || ""`);
    check(false, said ? `the migration was refused: ${said}` : "the migration never appeared on the board");
  }
  const text = await sheetText(page);
  check(text.includes(chosen), `the sheet does not name the destination ${chosen}`);
  // Created and honest: nothing has started a receiver yet, and the page may
  // not imply one is listening.
  check(/Not listening yet/.test(text),
    "a migration nobody has prepared yet does not say so:\n" + text.slice(0, 600));

  // Where those words come from, which is the part worth pinning.
  //
  // `Moved` is computed by the API on every read, so it is always there — this
  // check used to assert the opposite, and passed only because the fake applied
  // the computation on one path out of four. Against the real API it was false.
  // So: the condition is present and agrees with the screen…
  const moved = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(MOVED)});
    return (pick(status(r), "conditions") || []).find((c) => c.kind === "Moved") || null;
  })()`);
  check(moved && moved.reason === "PreparingReceiver",
    "the API did not compute a Moved condition for a migration nobody has prepared: "
      + JSON.stringify(moved));

  // …and the screen does not depend on it. Read from the object with its
  // conditions stripped: a cell whose API is answering but whose destination has
  // said nothing yet must still get a truthful line, and "not listening" is a
  // fact about `receiverReady`, not a reading of somebody's condition.
  const withoutCondition = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(MOVED)});
    const bare = JSON.parse(JSON.stringify(r));
    (status(bare) || {}).conditions = [];
    return movedWords(bare, migrationFacts(bare)).word;
  })()`);
  check(withoutCondition === "Not listening yet",
    "with no Moved condition the screen says " + JSON.stringify(withoutCondition)
      + " instead of reading receiverReady");

  check(!/%/.test(text), "the migration screen shows a percentage");

  // And no age beside it. `Moved` is built during the request, so its
  // `lastTransition` is the moment of *this read* — an age there would read as
  // movement on a transfer that may have stalled hours ago. The message is
  // where the information is, and it is shown.
  const agedRow = await page.evaluate(`(() => {
    const row = [...document.querySelectorAll("#sheet tr")]
      .find((tr) => /^Moved/.test(tr.innerText));
    return row ? row.innerText : "no Moved row in the conditions table";
  })()`);
  check(!/ago/.test(agedRow),
    "the conditions table dates a computed condition, which is the moment of the read: " + agedRow);
  // The source is derived, not asked — and it is on the object all the same.
  check(text.includes(it.node), `the source node ${it.node} is not on the migration`);
  noThrows("starting a migration threw");
});

await test("the guest says where it is and where it is going, without a second list", async () => {
  await page.evaluate(`closeSheet()`);
  await open(page, "instances");
  const it = await page.evaluate(`(async () => {
    const ms = (await list(collection("migrations"))).items
      .filter((m) => !deletedAt(m) && condition(m, "Moved")?.status !== "True");
    if (!ms.length) return null;
    const name = String(pick(spec(ms[0]), "instance"));
    const r = view.items.find((x) => nameOf(x) === name);
    return r ? { id: idOf(r), node: at(status(r), "node"), to: String(pick(spec(ms[0]), "toNode")),
                 migration: idOf(ms[0]) } : null;
  })()`);
  if (!it) skip("nothing in this cell is being migrated");
  await openRow(page, it.id);
  await waitFor(page, `!document.getElementById("instancemigration").innerText.includes("Looking for")`);
  const seen = await page.evaluate(`({
    section: document.getElementById("instancemigration").innerText,
    migrate: !!document.getElementById("migratebtn"),
  })`);
  if (it.node) {
    check(seen.section.includes(it.node), `the guest's own node is not stated: "${seen.section}"`);
  } else {
    check(/No node has this guest/.test(seen.section),
      `a guest nobody holds is described as "${seen.section}"`);
  }
  check(seen.section.includes(it.to), `where it is going is not stated: "${seen.section}"`);
  check(seen.section.includes(it.migration), "the migration itself is not reachable from the guest");
  check(!seen.migrate, "a second migration is offered for a guest already moving");
  // The word the platform has, and not one it does not.
  check(!/MIGRATING/i.test(await sheetText(page)),
    "the instance is given a state the platform does not have");
});

await test("a receiver that is not listening is not called progress", async () => {
  await page.evaluate(`closeSheet()`);
  await open(page, "migrations");
  const it = await pick("migration whose receiver is not up",
    `(x) => pick(status(x), "receiverReady") !== true && !deletedAt(x)`);
  await openRow(page, it.id);
  const text = await sheetText(page);
  check(/Not listening yet/.test(text),
    "a migration with no receiver does not say so:\n" + text.slice(0, 700));
  check(/not listening/.test(text), "the receiver's own state is not shown");
  check(!/Copying memory/.test(text), "a migration nothing is listening for claims to be copying");
});

await test("a transfer shows what was copied, never a percentage of a promise", async () => {
  await open(page, "migrations");
  const it = await pick("migration that is really sending",
    `(x) => pick(status(x), "receiverReady") === true && Number(pick(status(x), "transferredMib") || 0) > 0`);
  const copied = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(it.id)});
    return Number(pick(status(r), "transferredMib"));
  })()`);
  await openRow(page, it.id);
  const text = await sheetText(page);
  check(/Copying memory/.test(text), "a migration that is sending does not say so:\n" + text.slice(0, 700));
  check(text.includes(copied.toLocaleString()), `${copied} MiB copied is not on the page`);
  check(!/%/.test(text), "the page shows a percentage of a total nobody promised");
  // That it may stall is said in words. There is deliberately no age beside the
  // number: `Moved` is computed on read, so its `lastTransition` is the moment
  // of the read, and an age from it would say "just now" over a transfer that
  // stopped an hour ago. Nothing else on the object carries a time for it.
  check(/what the source last reported/.test(text),
    "the copied number is presented as motion rather than as an observation");
  // Read the movement block itself rather than slicing the sheet: the margin
  // labels are set in small caps, so splitting the text on a section name finds
  // nothing and quietly checks the whole page.
  const movement = await page.evaluate(`document.querySelector("#sheet .verdict").innerText`);
  check(/Copying memory/.test(movement), "the first block on a migration is not the movement");
  check(!/ago/.test(movement),
    "the movement block claims an age for a number nothing timestamps:\n" + movement);

  // Computed, never stored — which is what lets it be right about a migration
  // whose destination agent has died, and why a timeout arrives with no event.
  // Reading twice is the proof: the condition is there both times and asking
  // for it is not a write.
  const first = await (await api("/api/v1/" + it.name)).json();
  const again = await (await api("/api/v1/" + it.name)).json();
  const moved = (first.status.conditions || []).find((c) => c.kind === "Moved");
  check(moved, "a migration carries no Moved condition at all: " +
    JSON.stringify(first.status.conditions));
  equal(again.meta.revision, first.meta.revision,
    "reading a migration twice moved its revision, so something is being written on read");
});

await test("a migration that has arrived says so, and removing it is not abandoning it", async () => {
  await page.evaluate(`closeSheet()`);
  await open(page, "migrations");
  const it = await pick("migration that has arrived",
    `(x) => condition(x, "Moved")?.status === "True" && !deletedAt(x)`);
  await openRow(page, it.id);
  const seen = await page.evaluate(`(() => {
    const verb = document.getElementById("deletebtn").textContent;
    document.getElementById("deletebtn").click();
    return { verb, said: document.getElementById("deletewarning").textContent,
             text: document.getElementById("sheet").innerText };
  })()`);
  check(/Arrived/.test(seen.text), "a finished migration does not say it arrived:\n" + seen.text.slice(0, 500));
  // A receiver is torn down once the guest is there, so "arrived" may never be
  // read off whether something is still listening.
  check(!/Not listening yet/.test(seen.text),
    "an arrived migration is described by its dead receiver rather than by where the guest is");
  equal(seen.verb, "Remove", "removing a finished record is offered as abandoning a transfer");
  check(/only removes the record/.test(seen.said),
    `removing an arrived migration warns "${seen.said}"`);
  check(!/not safe|loses it/i.test(seen.said),
    "removing a finished migration is described as dangerous");
  await page.evaluate(`closeSheet()`);
});

await test("abandoning a post-copy migration warns differently from a live one", async () => {
  const words = {};
  for (const mode of ["Live", "PostCopy"]) {
    await page.evaluate(`closeSheet()`);
    await open(page, "migrations");
    const it = await page.evaluate(`(() => {
      const r = view.items.find((x) => pick(spec(x), "mode") === ${JSON.stringify(mode)} &&
        !deletedAt(x) && condition(x, "Moved")?.status !== "True");
      return r ? idOf(r) : null;
    })()`);
    if (!it) continue;
    await openRow(page, it);
    words[mode] = await page.evaluate(`(() => {
      const b = document.getElementById("deletebtn");
      const verb = b.textContent;
      b.click();
      return { verb, said: document.getElementById("deletewarning")?.textContent || "",
               id: ${JSON.stringify(it)} };
    })()`);
  }
  if (!words.Live || !words.PostCopy) skip("this cell holds no live and post-copy pair to compare");
  equal(words.Live.verb, "Abandon", "abandoning a migration is offered as a delete");
  check(/keeps running on/.test(words.Live.said),
    `abandoning a live migration says "${words.Live.said}"`);
  check(!/not safe/i.test(words.Live.said),
    "abandoning a live migration is described as unsafe, and it is not");
  check(/not safe/i.test(words.PostCopy.said) && /loses it|split across both/i.test(words.PostCopy.said),
    `abandoning a post-copy migration says "${words.PostCopy.said}"`);
  check(words.Live.said !== words.PostCopy.said, "both modes are warned about identically");

  // And it really abandons: the one this suite created, so nothing seeded is
  // destroyed by running the tests.
  await page.evaluate(`closeSheet()`);
  await open(page, "migrations");
  const mine = await page.evaluate(`[...document.querySelectorAll("#boardbody tr")]
    .some((r) => r.dataset.name.endsWith("/" + ${JSON.stringify(MOVED)}))`);
  if (!mine) skip("the migration this suite started is not on the board to abandon");
  await openRow(page, MOVED);
  await page.evaluate(`document.getElementById("deletebtn").click()`);
  const said = await page.evaluate(`document.getElementById("deletewarning").textContent`);
  check(said.length > 20, `abandoning asked "${said}"`);
  await page.evaluate(`document.getElementById("confirmdelete").click()`);
  const gone = await waitFor(page, `(() => {
    const still = [...document.querySelectorAll("#boardbody tr")]
      .some((r) => r.dataset.name.endsWith("/" + ${JSON.stringify(MOVED)}));
    return still ? null : 1;
  })()`);
  check(gone, "an abandoned migration is still on the board");
});

await test("only what the platform said `allowed` can be chosen", async () => {
  // The answer carries every node with its own verdict, so nothing here has to
  // infer anything — and must not. The dangerous inference is "not refused,
  // therefore fine": it is exactly how a destination the API is about to turn
  // down ends up in front of somebody.
  const read = await page.evaluate(`(() => {
    const nodes = [{ meta: { name: "nodes/node-a" }, spec: {}, status: {} },
                   { meta: { name: "nodes/node-b" }, spec: {}, status: {} },
                   { meta: { name: "nodes/node-z" }, spec: {}, status: {} }];
    const said = readCandidates({ from: "node-a", destinations: [
      { node: "node-a", allowed: false, why: "AlreadyThere", detail: "it is already on node-a" },
      { node: "node-b", allowed: true, why: "", detail: "" },
    ] }, nodes);
    // A node the answer does not mention does not exist — it is not a
    // candidate this console invents from the fleet list.
    const unmentioned = said.candidates.map((c) => c.id).includes("node-z");
    const unreadable = readCandidates({ something: "else" }, nodes);
    const ok = (a) => a.candidates.filter((c) => c.ok).map((c) => c.id);
    const no = (a) => a.candidates.filter((c) => !c.ok).map((c) => c.id + ":" + c.why + ":" + c.detail);
    return { ok: ok(said), no: no(said), unmentioned,
             unreadableOk: ok(unreadable), trouble: unreadable.trouble || "" };
  })()`);
  equal(read.ok, ["node-b"], "the destinations the platform allowed are not the ones offered");
  equal(read.no, ["node-a:AlreadyThere:it is already on node-a"],
    "a refusal lost its token or its sentence");
  check(!read.unmentioned, "a node the answer never mentioned was offered as a destination");
  // An answer in a shape this console cannot read is not read as "all fine":
  // it says so, and nothing claims the platform vouched for anything.
  check(/does not know/.test(read.trouble), `an unreadable answer said "${read.trouble}"`);
  check(read.unreadableOk.length === 3, "an unreadable answer left nothing to try at all");
});

await test("a migration that ran out of time stops reading as one in flight, with no event to tell it", async () => {
  // The one change in the contract a watch cannot deliver. `Moved` is computed
  // when the object is read — out of the migration, the instance and the clock
  // — so a migration passing its timeout involves no write, and the store has
  // nothing to announce. A screen that only listens shows it as transferring
  // for ever.
  //
  // Nothing here writes a condition: the object is made older than its own
  // budget and the timeout follows from the clock, which is the only way it
  // ever really happens.
  if (!SCAFFOLDED) skip("only the in-memory contract server can make a migration older than it is");
  await page.evaluate(`closeSheet()`);

  // Its own subject, made here and taken away again.
  //
  // This used to choose a migration out of the seed by predicate and then
  // backdate it by 99999 seconds — a shared object left permanently expired for
  // everything that ran afterwards, and a name another check may already have
  // abandoned, in which case `/__test/age` answers 404 about an object this one
  // never touched. Neither is a hazard a subject nothing else can name has.
  const it = await ownMigration();
  try {
    // The interval, not its length: fifteen seconds is right for an operator and
    // wrong for a test, and what is being checked is that it asks at all.
    await page.evaluate(`(async () => {
      collection("migrations").recheck = 2;
      await show("migrations");
    })()`);
    const there = await waitFor(page, `(() => {
      const row = [...document.querySelectorAll("#boardbody tr")]
        .find((r) => r.dataset.name === ${JSON.stringify(it.name)});
      return row ? 1 : null;
    })()`);
    check(there, "the migration this check started never reached the board");
    const before = await (await api("/api/v1/" + it.name)).json();
    const aged = await api("/__test/age?name=" + it.name + "&seconds=99999");
    check(aged.ok, `the API would not age that migration (${aged.status})`);
    const now = await aged.json();
    const moved = (now.status.conditions || []).find((c) => c.kind === "Moved");
    check(moved && moved.reason === "Timeout",
      "ageing the object past its budget did not produce a timeout: " + JSON.stringify(moved));
    const said = moved.message;
    // Nothing was written: same revision, so there was nothing for the store to
    // announce and the watch could not have carried this.
    equal(now.meta.revision, before.meta.revision,
      "the timeout came with a write, which means a watch would have delivered it " +
      "and this test is no longer checking what it says it checks");

    const caught = await waitFor(page, `(() => {
      const row = [...document.querySelectorAll("#boardbody tr")]
        .find((r) => r.dataset.name === ${JSON.stringify(it.name)});
      return row && /Failing/.test(row.innerText) ? row.innerText : null;
    })()`, { timeout: 15000 });
    check(caught, "an expired migration still reads as in flight; nothing asked again");

    await openRow(page, it.id);
    const text = await sheetText(page);
    check(/Gave up/.test(text), "a migration that ran out of time does not say so:\n" + text.slice(0, 600));
    // Verbatim: the sentence names where the guest ended up, which is the whole
    // question at that moment, and no paraphrase can be trusted with it.
    check(text.includes(said), `the platform's own sentence ("${said}") is not on the page`);

    // The one age this screen shows, and the only reason that has earned one: a
    // timeout happened at `createdAt + timeoutS`, so the API stamps that moment
    // and two reads agree on it. Every other reason's `lastTransition` is the
    // moment of the read, which is why nothing else here carries an age.
    const movement = await page.evaluate(`document.querySelector("#sheet .verdict").innerText`);
    check(/gave up/i.test(movement) && /ago/.test(movement),
      "a timed-out migration does not say how long ago it gave up:\n" + movement);
    const twice = await Promise.all([
      (await api("/api/v1/" + it.name)).json(),
      (await api("/api/v1/" + it.name)).json(),
    ]);
    const stamps = twice.map((r) =>
      (r.status.conditions || []).find((c) => c.kind === "Moved").lastTransition);
    equal(stamps[0], stamps[1],
      "the timeout's moment moves between reads, so the age beside it is the age of the read");
    check(!/Copying memory|Not listening yet/.test(text),
      "an expired migration is still described as if it were moving");
    // A new migration is a new migration: there is no retry here.
    check(!/retry|try again/i.test(text), "the screen offers to retry a migration");
  } finally {
    // Both of these used to sit on the happy path, where the first failed check
    // skipped them: a two-second recheck left running for every check after
    // this one, and a subject left behind. A test that changes something global
    // has to put it back when it fails, which is the only time it matters.
    await page.evaluate(`(async () => {
      closeSheet();
      collection("migrations").recheck = 15;
    })()`);
    await api("/api/v1/" + it.name, { method: "DELETE" }).catch(() => {});
  }
});

// --- the promises the page itself makes -------------------------------------

await test("nothing is fetched from outside the console's own origin", async () => {
  const origin = new globalThis.URL(URL).origin;
  const outside = page.requests.filter((u) => !u.startsWith(origin) && !u.startsWith("data:") && u !== "about:blank");
  check(!outside.length, "the page reached outside itself:\n  " + outside.join("\n  "));
});

await test("escape leaves, rather than trapping", async () => {
  await page.evaluate(`document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }))`);
  await sleep(300);
  const gone = await page.evaluate(`!document.getElementById("sheet")`);
  check(gone, "escape did not close the sheet");
});

await test("the appearance can be changed and is remembered", async () => {
  const after = await page.evaluate(`(() => {
    document.getElementById("theme").click();
    return { attr: document.documentElement.getAttribute("data-theme"),
             stored: localStorage.getItem("velstra-cloud-theme"),
             bg: getComputedStyle(document.body).backgroundColor };
  })()`);
  equal(after.attr, "light", "the light appearance was not applied");
  equal(after.stored, "light", "the choice was not remembered");
  check(after.bg !== "rgba(0, 0, 0, 0)", "the body has no background of its own");
  await page.evaluate(`document.getElementById("theme").click()`);
});

const GROUP = "sg-" + Math.random().toString(36).slice(2, 6);

await test("a security group can be built out of rules, not just named", async () => {
  // The half of this feature that was missing for a while: a form that could
  // only produce an *empty* group would make something the platform accepts and
  // that allows nothing.
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "security-groups");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(300);
  await page.evaluate(`(() => {
    const id = document.getElementById("f-id");
    id.value = ${JSON.stringify(GROUP)}; id.dispatchEvent(new Event("input"));
    document.getElementById("addrule").click();
  })()`);
  await sleep(200);
  const built = await page.evaluate(`(() => {
    const host = document.getElementById("f-rules");
    const row = host.querySelector(".row");
    const selects = [...row.querySelectorAll("select")];
    const numbers = [...row.querySelectorAll("input[type=number]")];
    const text = row.querySelector("input[type=text]");
    return {
      // A rule offered blank would be a rule somebody has to fill in four times
      // before it means anything; it starts as the commonest one instead.
      direction: selects[0].value,
      protocol: selects[1].value,
      ports: numbers.length,
      remote: text ? text.value : null,
    };
  })()`);
  equal(built.direction, "ingress", "a new rule does not start as ingress");
  equal(built.protocol, "tcp", "a new rule does not start as tcp");
  equal(built.ports, 2, "a tcp rule has no port range");
  equal(built.remote, "0.0.0.0/0", "a new rule has no remote");

  // Switching to a protocol with no ports drops the range rather than hiding
  // it: sent along, it would be refused, and the operator would be looking at a
  // form with no range in it wondering what the API meant.
  const gone = await page.evaluate(`(() => {
    const row = document.getElementById("f-rules").querySelector(".row");
    const protocol = [...row.querySelectorAll("select")][1];
    protocol.value = "icmp"; protocol.dispatchEvent(new Event("change"));
    const now = document.getElementById("f-rules").querySelector(".row");
    return now.querySelectorAll("input[type=number]").length;
  })()`);
  equal(gone, 0, "a port range survived a switch to icmp");

  await page.evaluate(`(() => {
    const row = document.getElementById("f-rules").querySelector(".row");
    const protocol = [...row.querySelectorAll("select")][1];
    protocol.value = "tcp"; protocol.dispatchEvent(new Event("change"));
    const back = document.getElementById("f-rules").querySelector(".row");
    const numbers = [...back.querySelectorAll("input[type=number]")];
    numbers[0].value = "443"; numbers[0].dispatchEvent(new Event("input"));
    numbers[1].value = "443"; numbers[1].dispatchEvent(new Event("input"));
    document.getElementById("submitform").click();
  })()`);
  await sleep(1500);

  const seen = await page.evaluate(`({
    dialog: !!document.getElementById("dialog"),
    rows: [...document.querySelectorAll("#boardbody tr")].map((r) => r.dataset.name.split("/").pop()),
  })`);
  check(!seen.dialog, "the form stayed open after a successful create");
  check(seen.rows.includes(GROUP), "the new group is not on the board: " + seen.rows.join(", "));

  // Read back through the API rather than off the screen: what matters is that
  // the shape the platform stored is the shape it documents, not that the form
  // remembers what was typed into it.
  const stored = await page.evaluate(`(async () => {
    const row = [...document.querySelectorAll("#boardbody tr")]
      .find((r) => r.dataset.name.endsWith("/" + ${JSON.stringify(GROUP)}));
    const r = await fetch("/api/v1/" + row.dataset.name,
      { headers: { authorization: "Bearer " + sessionStorage.getItem("velstra-cloud-token") } });
    const d = await r.json();
    return d.spec.rules;
  })()`);
  equal(stored.length, 1, "the group was created without its rule");
  equal(stored[0].protocol, "tcp", "the rule reached the API as something else");
  equal(stored[0].ports.from, 443, "the port range did not survive");
  equal(stored[0].remote.cidr, "0.0.0.0/0", "the remote did not survive");
});

await test("a load balancer can be built with its listeners, not just named", async () => {
  // The same half that was once missing from security groups: a form that
  // could only produce a load balancer with no listeners would make something
  // the platform accepts and that answers on nothing.
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "load-balancers");
  const seeded = await page.evaluate(`({
    rows: document.querySelectorAll("#boardbody tr").length,
    text: document.querySelector("#boardbody tr").innerText,
  })`);
  check(seeded.rows >= 1, "the seeded load balancer is not on the board");
  check(/10\.20\.0\.20/.test(seeded.text), "the VIP is not in the row: " + seeded.text);

  await page.evaluate(`document.getElementById("newbtn").click()`);
  await sleep(400);
  await page.evaluate(`(() => {
    const id = document.getElementById("f-id");
    id.value = "api"; id.dispatchEvent(new Event("input"));
    const network = document.getElementById("f-network");
    network.value = "projects/p1/networks/prod"; network.dispatchEvent(new Event("change"));
  })()`);
  await sleep(300);
  await page.evaluate(`(() => {
    const subnet = document.getElementById("f-subnet");
    subnet.value = "projects/p1/subnets/prod-a"; subnet.dispatchEvent(new Event("change"));
    document.getElementById("addlistener").click();
  })()`);
  await sleep(200);
  const built = await page.evaluate(`(() => {
    const row = document.getElementById("f-listeners").querySelector(".row");
    const protocol = row.querySelector("select");
    const numbers = [...row.querySelectorAll("input[type=number]")];
    return {
      // A listener offered blank would be one somebody fills in three times
      // before it means anything; it starts as the commonest one instead.
      protocol: protocol.value,
      port: numbers[0].value,
      member: numbers[1].value,
      memberSays: numbers[1].placeholder,
    };
  })()`);
  equal(built.protocol, "Tcp", "a new listener does not start as TCP");
  equal(built.port, "443", "a new listener does not start on 443");
  check(built.member === "" && /same/.test(built.memberSays),
    "an empty member port does not say it keeps the client's own");

  await page.evaluate(`document.getElementById("submitform").click()`);
  await sleep(1500);
  const seen = await page.evaluate(`({
    dialog: !!document.getElementById("dialog"),
    rows: [...document.querySelectorAll("#boardbody tr")].map((r) => r.dataset.name.split("/").pop()),
  })`);
  check(!seen.dialog, "the form stayed open after a successful create");
  check(seen.rows.includes("api"), "the new load balancer is not on the board: " + seen.rows.join(", "));

  // Read back through the API rather than off the screen: what matters is the
  // shape the platform stored, in the contract's spelling.
  const stored = await page.evaluate(`(async () => {
    const r = await fetch("/api/v1/projects/p1/load-balancers/api",
      { headers: { authorization: "Bearer " + sessionStorage.getItem("velstra-cloud-token") } });
    return (await r.json()).spec;
  })()`);
  equal(stored.listeners.length, 1, "the load balancer was created without its listener");
  equal(stored.listeners[0].protocol, "Tcp", "the protocol reached the API as something else");
  equal(stored.listeners[0].port, 443, "the port did not survive");
  equal(stored.network, "projects/p1/networks/prod", "the network did not survive");
});

// ---- picking disks ---------------------------------------------------------
//
// The one control on the page that destroys something. Everything below is
// about the half of it that is easy to build wrong and invisible when it is: a
// disk that cannot be taken has to say *why*, in the model's own words, rather
// than being greyed out or quietly left off the list. A picker that silently
// omitted them would look perfectly correct and would answer "why can I not
// select this disk" with nothing at all.

const CLUSTER = "ceph-" + Math.random().toString(36).slice(2, 6);

const diskRows = (page) => page.evaluate(`(() => {
  const host = document.getElementById("f-osds");
  if (!host) return { trouble: "the Ceph form has no disk picker at all" };
  return {
    warning: (host.querySelector("p.warn") || {}).textContent || "",
    hosts: [...host.querySelectorAll(".diskhost")].map((h) => h.textContent),
    rows: [...host.querySelectorAll(".disk")].map((r) => ({
      node: r.dataset.node,
      device: r.dataset.device,
      why: (r.querySelector(".why") || {}).textContent || "",
      button: (r.querySelector("button") || {}).textContent || "",
      note: (r.querySelector(".note") || {}).textContent || "",
    })),
  };
})()`);

await test("a disk that cannot be taken says why in words, not by being greyed out", async () => {
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  await open(page, "ceph-clusters");
  await page.evaluate(`document.getElementById("newbtn").click()`);
  // The picker is filled after the dialog is on screen, from a real list of
  // nodes — the control is drawn once before that fetch lands.
  await sleep(500);
  const seen = await diskRows(page);
  check(!seen.trouble, seen.trouble || "");
  check(seen.rows.length >= 6,
    `the picker offered ${seen.rows.length} disks, so this seed cannot exercise a refusal`);
  // Grouped by the machine the disk is plugged into: a bare list of paths is a
  // list in which /dev/sda appears three times and means three things.
  check(seen.hosts.some((h) => h.startsWith("node-a")) && seen.hosts.some((h) => h.startsWith("node-c")),
    "the disks are not grouped by node: " + seen.hosts.join(", "));

  const find = (suffix) => seen.rows.find((r) => r.device.endsWith(suffix)) || {};
  const ext4 = find("0002"), small = find("0003"), system = find("0004"),
    mounted = find("0005"), partitioned = find("0006"), osd = find("0008");

  // Word for word what `may_consume` returns — the schema carries the sentences
  // and a test in the API crate holds them to the model. What is checked here
  // is that they reach the screen at all.
  check(/it holds a ext4 filesystem/.test(ext4.why), `the ext4 disk said "${ext4.why}"`);
  check(/erases that/.test(ext4.why), "the reason stops before saying what would happen");
  check(/at least 20/.test(small.why), `the 16 GiB disk said "${small.why}"`);
  check(/takes this node down/.test(system.why), `the system disk said "${system.why}"`);
  check(/mounted at \/var\/lib\/velstra/.test(mounted.why), `the mounted disk said "${mounted.why}"`);
  check(/partition table with 3 partition/.test(partitioned.why),
    `the partitioned disk said "${partitioned.why}"`);
  check(/already OSD 7/.test(osd.why), `the disk that is already an OSD said "${osd.why}"`);

  // Not greyed out — refused. A row that carried a disabled button would be
  // saying "not now" where the platform means "not until you wipe it".
  for (const r of seen.rows.filter((r) => r.why)) {
    check(!r.button, `${r.device} is refused and still offers a "${r.button}" button`);
  }
  // And the erase warning is on the control, not only in the blurb at the top
  // of the dialog: by the time somebody is reading a list of disks they are
  // past the blurb.
  check(/erases it/.test(seen.warning), `the disk picker warned: "${seen.warning}"`);

  // What is offered still has to be legible as a disk: a path and nothing else
  // is not a choice anybody can make.
  const free = find("0001");
  equal(free.button, "Add", "the empty NVMe was not offered");
  check(/931 GiB/.test(free.note) && /solid state/.test(free.note),
    `the offered disk read as "${free.note}"`);
});

await test("picking a disk stages the node and the device together", async () => {
  const chosen = await page.evaluate(`(() => {
    const row = document.querySelector('#f-osds .disk[data-device$="0001"]');
    row.querySelector("button[data-disk=add]").click();
    const now = document.querySelector('#f-osds .disk[data-device$="0001"]');
    return { chosen: now.classList.contains("chosen"),
             button: now.querySelector("button").textContent };
  })()`);
  check(chosen.chosen, "adding a disk did not mark it as taken");
  equal(chosen.button, "Remove", "a disk that was added cannot be given back");

  // Everything else the form needs, and a pool — which lives one level deeper,
  // because three copies with a floor of two is the answer almost everybody
  // wants and the model already fills it in.
  await page.evaluate(`(() => {
    const id = document.getElementById("f-id");
    id.value = ${JSON.stringify(CLUSTER)}; id.dispatchEvent(new Event("input"));
    const net = document.getElementById("f-publicNetwork");
    net.value = "10.20.0.0/24"; net.dispatchEvent(new Event("input"));
    document.querySelector("#f-monitors > button.btn").click();
    document.getElementById("moresettings").click();
    document.getElementById("addpool").click();
  })()`);
  await sleep(200);
  const pool = await page.evaluate(`(() => {
    const row = document.querySelector("#f-pools .row");
    const numbers = [...row.querySelectorAll("input[type=number]")];
    const name = row.querySelector("input[type=text]");
    name.value = "volumes"; name.dispatchEvent(new Event("input"));
    return { copies: numbers[0].value, floor: numbers[1].value };
  })()`);
  // The blank row starts where `CephPoolSpec` starts. A form proposing 2/1
  // while the API means 3/2 would be quietly offering a weaker pool than the
  // one it is copying.
  equal(pool.copies, "3", "a blank pool does not start at three copies");
  equal(pool.floor, "2", "a blank pool does not start at a floor of two");

  await page.evaluate(`document.getElementById("submitform").click()`);
  await sleep(1500);

  // Read back through the API rather than off the screen: what matters is the
  // shape the platform stored, not that the form remembers what was clicked.
  const stored = await page.evaluate(`(async () => {
    const r = await fetch("/api/v1/ceph-clusters/" + ${JSON.stringify(CLUSTER)},
      { headers: { authorization: "Bearer " + sessionStorage.getItem("velstra-cloud-token") } });
    if (!r.ok) return { status: r.status };
    return { spec: (await r.json()).spec };
  })()`);
  check(!stored.status, "the cluster was never created: the API answered " + stored.status);
  equal((stored.spec.osds || []).length, 1, "the OSD did not reach the API");
  // A bare id, like every other reference to a node in this platform. A full
  // resource name here is an OSD assigned to a node that does not answer to it.
  equal(stored.spec.osds[0].node, "node-a", "the OSD names its node the wrong way");
  equal(stored.spec.osds[0].device, "/dev/disk/by-id/nvme-eui.0001",
    "the OSD lost the disk it was made from");
  equal((stored.spec.pools || []).length, 1, "the pool did not reach the API");
  equal(stored.spec.pools[0].pool, "volumes", "the pool lost its name");
  equal(stored.spec.pools[0].size, 3, "the pool lost its copies");
  equal(stored.spec.pools[0].minSize, 2, "the pool lost its write floor");
});

await test("a disk already given to this cluster can still be taken back", async () => {
  // The trap in an edit form: once Ceph owns a disk the node reports it as an
  // OSD, which `may_consume` refuses — so a picker that believed the refusal
  // would render every disk of a working cluster as unavailable, with no way
  // left to remove one.
  await page.evaluate(`document.getElementById("cancelform")?.click(); closeSheet();`);
  // The picker cache is per collection and a write to a Ceph cluster does not
  // touch `nodes`, so it is dropped here by hand. This is not the test working
  // around the console — it is what the operator sees after any node event, a
  // project switch or a reload, which is to say the ordinary case.
  await page.evaluate(`forgetOptions("nodes")`);
  await open(page, "ceph-clusters");
  await openRow(page, CLUSTER);
  await page.evaluate(`document.getElementById("editbtn").click()`);
  await sleep(500);
  const seen = await page.evaluate(`(async () => {
    const nodes = await options("nodes");
    const disk = (at(status(nodes.find((n) => idOf(n) === "node-a")), "devices") || [])
      .find((d) => d.path.endsWith("0001"));
    const row = document.querySelector('#f-osds .disk[data-device$="0001"]');
    return { reported: at(disk, "state.kind"),
             chosen: row.classList.contains("chosen"),
             why: (row.querySelector(".why") || {}).textContent || "",
             button: (row.querySelector("button") || {}).textContent || "" };
  })()`);
  check(seen.reported === "Osd",
    `the node is reporting this disk as ${seen.reported}, so this test is not exercising the trap`);
  check(seen.chosen, "the disk this cluster already holds is not shown as held");
  check(!seen.why, `the form refuses a disk it already asked for: "${seen.why}"`);
  equal(seen.button, "Remove", "a disk already in the spec cannot be removed");
  await page.evaluate(`document.getElementById("cancelform").click(); closeSheet();`);
});

// ---- paging ----------------------------------------------------------------
//
// The API answers a list a page at a time, and the failure this guards against
// is the silent one: a client that reads the first page and renders it as the
// whole collection. Nothing about that looks wrong — the board is populated, the
// rail shows a count, the watch says live — and an operator is simply missing
// machines.
//
// The first three of these are about the *fake*, and they are here rather than
// nowhere because a fake that always answers with a whole collection is a fake
// no client can fail against. Every console test would pass against a server
// that cannot behave like the real one, and the console's paging would be
// untested by construction.

/// Every page of a walk, fetched by hand. Returns what each page said, so the
/// checks below can be about the shape of the walk and not only its result.
const walkByHand = async (path, pageSize) => {
  const pages = [];
  let token = null;
  for (let i = 0; i < 50; i++) {
    const q = path + "?pageSize=" + pageSize + (token ? "&pageToken=" + encodeURIComponent(token) : "");
    const r = await api(q);
    if (!r.ok) throw new Error(`page ${i + 1} answered ${r.status}`);
    const body = await r.json();
    pages.push(body);
    token = body.nextPageToken || null;
    if (!token) return pages;
  }
  throw new Error("the walk never ended");
};

const setCeiling = (n) => api("/__test/pagesize?n=" + n);

await test("a paged walk hands back the whole collection, once each", async () => {
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  const whole = await (await api("/api/v1/projects/p1/instances")).json();
  const expected = whole.items.map((r) => r.meta.name).sort();
  check(expected.length >= 3,
    `this seed has ${expected.length} instances, which is too few to page over`);

  await setCeiling(2);
  const pages = await walkByHand("/api/v1/projects/p1/instances", 1000);
  await setCeiling(0);

  check(pages.length > 1, `the server never paged: one page of ${pages[0].items.length}`);
  const walked = pages.flatMap((p) => p.items.map((r) => r.meta.name));
  equal([...walked].sort(), expected, "the walk lost, repeated or reordered objects");
  equal(walked.length, new Set(walked).size, "the walk delivered the same object twice");
});

await test("a walk that ends on a page boundary does not offer another page", async () => {
  // A token after the last full page is one round trip per walk, for ever, on
  // every client — and no count of objects would ever reveal it, because the
  // extra page is correctly empty.
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  const whole = await (await api("/api/v1/projects/p1/instances")).json();
  const total = whole.items.length;

  await setCeiling(total);
  const exact = await (await api("/api/v1/projects/p1/instances?pageSize=1000")).json();
  await setCeiling(0);

  equal(exact.items.length, total, "the exact-size page was not the whole collection");
  check(exact.nextPageToken === undefined,
    "a collection that ended exactly on a page boundary still offered a token");
});

await test("every page of a walk reports the revision the first page was read at", async () => {
  // The one that decides whether list-then-watch is correct across a paged
  // list. A client pages to the end and then watches from what it was given; if
  // each page reported its own revision, the client would watch from the end of
  // the walk and silently miss everything that landed during it. Nothing about
  // that is visible — the board is complete, the watch is live, and it is simply
  // never told about the change.
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  await setCeiling(2);
  const pages = await walkByHand("/api/v1/projects/p1/instances", 1000);
  await setCeiling(0);
  check(pages.length > 1, "the server never paged, so there is nothing to compare");
  const first = pages[0].revision;
  check(first, "the first page reported no revision at all");
  for (let i = 1; i < pages.length; i++) {
    equal(pages[i].revision, first,
      `page ${i + 1} reported its own revision instead of the walk's`);
  }
});

await test("a page token is refused where it does not belong, and so is a bad size", async () => {
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  await setCeiling(2);
  const first = await (await api("/api/v1/projects/p1/instances?pageSize=1000")).json();
  const token = first.nextPageToken;
  check(token, "the first page offered no token to misuse");

  // Presented against another collection it does not mean "start there" — it
  // means two walks have been confused, and answering would hand back objects
  // nobody asked for in an order that looks deliberate.
  const wrong = await api("/api/v1/projects/p1/volumes?pageSize=1000&pageToken=" + encodeURIComponent(token));
  equal(wrong.status, 400, "a token minted for instances was accepted for volumes");

  const forged = await api("/api/v1/projects/p1/instances?pageSize=1000&pageToken=obviously-not-one");
  equal(forged.status, 400, "a forged page token was accepted");

  // Ignoring an unparseable size hands the whole cell to a client that believes
  // it asked for twenty.
  const size = await api("/api/v1/projects/p1/instances?pageSize=twenty");
  equal(size.status, 400, "pageSize=twenty was not refused");
  const said = await size.json();
  check(/pageSize/.test(said.error?.message || ""),
    `the refusal did not name the parameter: ${said.error?.message}`);
  await setCeiling(0);
});

await test("the board shows a whole collection even when the API pages it small", async () => {
  // The console's own half. It never sent `pageSize` and never read
  // `nextPageToken`, so it was safe only because an unpaged list happened to
  // return everything — a property of today's API and not of the contract. The
  // moment it asked for a page, or the API grew a default cap, it would have
  // rendered the first page and looked complete.
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  const whole = await (await api("/api/v1/projects/p1/instances")).json();
  const expected = whole.items.map((r) => r.meta.name.split("/").pop()).sort();

  await setCeiling(2);
  await open(page, "instances");
  const seen = await page.evaluate(`({
    items: view.items.map((r) => idOf(r)).sort(),
    rows: [...document.querySelectorAll("#boardbody tr")].map((r) => r.dataset.name.split("/").pop()).sort(),
    complete: view.complete,
    railCount: (document.querySelector('[data-collection="instances"] .n')
      || document.querySelector('[data-collection="instances"] .state')).textContent,
  })`);
  await setCeiling(0);

  equal(seen.items, expected, "the console kept only part of a paged collection");
  equal(seen.rows, expected, "the board rendered only part of a paged collection");
  check(seen.complete !== false, "a walk that finished was reported as incomplete");
  check(!/\+$/.test(seen.railCount),
    `the rail marked a complete count as a floor: ${seen.railCount}`);
  noThrows("walking a paged list threw");
});

await test("a paged list is watched from where the walk started, not where it ended", async () => {
  // The console's side of the revision rule above: it keeps the *first* page's
  // revision and watches from that. Taking the last page's would compile, run,
  // and lose every change that landed during the walk.
  //
  // Asked against a server that gets the rule **wrong** on purpose, and that is
  // the whole point of the test. Against a faithful server every page carries
  // the same revision, so a client that keeps the first and one that keeps the
  // last read the same number and no assertion can separate them — which is
  // exactly how a client-side mistake here would survive a green suite for ever.
  // The first version of this test did that, and a mutation that took the last
  // page's revision passed it.
  //
  // Keeping the first is also the only safe rule for a client, which cannot know
  // which kind of server it is talking to: an earlier revision replays events it
  // already has, and a later one skips events it never will.
  if (!SCAFFOLDED) skip("only the in-memory contract server can be told to page small");
  await api("/__test/pagerevision?own=1");
  await setCeiling(2);
  await open(page, "instances");
  const held = await page.evaluate(`view.revision`);
  const pages = await walkByHand("/api/v1/projects/p1/instances", 1000);
  await setCeiling(0);
  await api("/__test/pagerevision?own=0");

  check(pages.length > 1, "the server never paged, so there is nothing to compare");
  const last = pages[pages.length - 1].revision;
  check(last !== pages[0].revision,
    `the server was asked to report each page's own revision and did not: ${pages.map((p) => p.revision).join(", ")}`);
  equal(String(held), String(pages[0].revision),
    "the console watches from a revision that is not the one its walk began at");
});

await test("a subnet's occupancy follows the ports, with no event to announce it", async () => {
  // The hardest thing on this contract for a client to get right, because
  // nothing goes wrong visibly. `status.allocated` and `status.available` are
  // counted by the API from the *ports*, on the way out — so taking an address
  // moves both numbers with **nothing written to the subnet**: no revision, no
  // watch event, nothing for a listening console to react to. A console that
  // only listens shows the occupancy as of the moment the board was opened, for
  // as long as it stays open, and the number looks perfectly plausible the whole
  // time.
  //
  // The schema said `recheck: 0` for subnets, and the test guarding that list
  // asserted it — so the gap was not merely unnoticed, it was pinned in place.
  //
  // It was invisible for a second reason too: the fake stored whatever the seed
  // put in those fields and never recomputed them, so no test could have moved
  // the number even if the console had asked.
  await open(page, "subnets");
  const subnet = await pick("subnet with a range", `(x) => !!at(x, "spec.cidr")`);
  const before = await page.evaluate(`(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(subnet.id)});
    return { allocated: at(r, "status.allocated"), available: at(r, "status.available") };
  })()`);
  check(Number.isFinite(Number(before.allocated)),
    `the subnet reports no occupancy at all: ${JSON.stringify(before)}`);

  // A port takes an address on that subnet. Nothing about the subnet is written.
  const made = "probe-" + Date.now().toString(36);
  const created = await api("/api/v1/projects/p1/ports", {
    method: "POST",
    body: JSON.stringify({
      id: made,
      spec: {
        network: (await (await api("/api/v1/projects/p1/subnets/" + subnet.id)).json()).spec.network,
        subnet: subnet.name,
        address: "10.20.0.201",
      },
    }),
  });
  check(created.ok, `creating the probe port answered ${created.status}`);

  const moved = await waitFor(page, `(() => {
    const r = view.items.find((x) => idOf(x) === ${JSON.stringify(subnet.id)});
    if (!r) return null;
    return Number(at(r, "status.allocated")) === ${Number(before.allocated) + 1} ? "moved" : null;
  })()`, { timeout: 25000 });

  await api("/api/v1/projects/p1/ports/" + made, { method: "DELETE" });

  check(moved,
    "the subnet's occupancy never moved after a port took an address from it — " +
    "the board is showing a count from whenever it was opened");
});

// The read sheet folds its advanced settings the way the create form does, and
// keeps one honesty rule the form does not need: a field the object actually
// set stays in view, only the unset advanced ones fold.
await test("the sheet folds unset advanced settings but keeps the set ones", async () => {
  await open(page, "projects");
  // A project that sets an advanced field (its folder) and leaves another
  // advanced field (cell) unset — so one of each is on screen at once.
  const proj = await pick("project with a set parent and an unset cell",
    `(x) => spec(x).parent && !spec(x).cell`);
  await openRow(page, proj.id);
  const r = await page.evaluate(`(() => {
    const sheet = document.getElementById("sheet");
    const spread = [...sheet.querySelectorAll(".spread")]
      .find((s) => /Specification/.test((s.querySelector(".margin") || {}).textContent || ""));
    if (!spread) return { error: "no Specification section" };
    const fold = spread.querySelector("#specmorefields");
    const toggle = spread.querySelector("#specmore");
    const label = (td) => td.textContent.replace(/·.*/, "").trim();
    const first = (root) => [...root.querySelectorAll("td:first-child")].map(label);
    const foldLabels = fold ? first(fold) : [];
    const mainLabels = [...spread.querySelectorAll("td:first-child")]
      .filter((td) => !fold || !fold.contains(td)).map(label);
    const hiddenBefore = fold ? fold.classList.contains("hidden") : null;
    const expandedBefore = toggle ? toggle.getAttribute("aria-expanded") : null;
    const toggleText = toggle ? toggle.textContent : "";
    if (toggle) toggle.click();
    const hiddenAfter = fold ? fold.classList.contains("hidden") : null;
    const expandedAfter = toggle ? toggle.getAttribute("aria-expanded") : null;
    return { hasToggle: !!toggle, toggleText,
             foldLabels, mainLabels, hiddenBefore, hiddenAfter, expandedBefore, expandedAfter };
  })()`);
  check(!r.error, r.error || "");
  check(r.hasToggle, "the sheet offers no \"More settings\" disclosure");
  check(/More settings \(\d+\)/.test(r.toggleText), `the disclosure is not labelled like the form: "${r.toggleText}"`);
  check(r.foldLabels.includes("Cell"), `an unset advanced field is not folded: fold has ${JSON.stringify(r.foldLabels)}`);
  check(!r.mainLabels.includes("Cell"), "an unset advanced field is also shown in the main list");
  // "Folder", not "Parent": the field names what it holds now that something
  // walks it, and the label is what somebody looks for on the sheet.
  check(r.mainLabels.includes("Folder"), `a set advanced field was folded away: main has ${JSON.stringify(r.mainLabels)}`);
  check(r.hiddenBefore === true && r.hiddenAfter === false, "the disclosure does not reveal the folded fields");
  check(r.expandedBefore === "false" && r.expandedAfter === "true", "aria-expanded does not track the disclosure");
});

// The jump palette: a flat search over every collection, so one an operator can
// name is a keystroke away rather than a hunt through the rail's groups.
await test("the jump palette reaches a collection by name", async () => {
  await open(page, "instances");
  const r = await page.evaluate(`(async () => {
    const wait = (ms) => new Promise((r) => setTimeout(r, ms || 200));
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
    await wait(150);
    const opened = !!document.getElementById("palette");
    const q = document.getElementById("paletteq");
    q.value = "project";
    q.dispatchEvent(new Event("input"));
    await wait(120);
    const rows = [...document.querySelectorAll("#palettelist .palitem .palt")].map((n) => n.textContent.trim());
    q.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await wait(350);
    const closed = !document.getElementById("palette");
    return { opened, rows, closed, coll: view.coll ? view.coll.id : null };
  })()`);
  check(r.opened, "Ctrl/Cmd-K did not open the palette");
  check(r.rows.some((t) => /project/i.test(t)), "the palette did not find Projects: " + JSON.stringify(r.rows));
  check(r.closed, "the palette stayed open after Enter");
  equal(r.coll, "projects", "Enter did not navigate to the named collection (coll=" + r.coll + ")");
});

/// "Where is db-1" — the question an operator actually has, whose only answer
/// used to be guessing which board it was on and looking.
await test("the palette finds an object by name and opens it where it lives", async () => {
  await open(page, "nodes");
  await page.evaluate(`closeSheet()`);
  const r = await page.evaluate(`(async () => {
    const wait = (ms) => new Promise((r) => setTimeout(r, ms || 200));
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true }));
    await wait(150);
    const q = document.getElementById("paletteq");
    q.value = "db-1";
    q.dispatchEvent(new Event("input"));
    await wait(150);
    const rows = [...document.querySelectorAll("#palettelist .palitem")]
      .map((n) => n.textContent.trim());
    // Down once: the collections that match come first, and here none do, so
    // the first row is already the object. Enter opens it.
    q.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await wait(600);
    return { rows, coll: view.coll ? view.coll.id : null, sheet: sheet.name || "" };
  })()`);
  check(r.rows.some((t) => t.includes("db-1")),
    `the palette did not find the guest: ${JSON.stringify(r.rows)}`);
  // The row says which collection it is in, because "db-1" alone does not say
  // whether it is a guest, a volume or a port.
  check(r.rows.some((t) => t.includes("db-1") && /instance/i.test(t)),
    `the palette does not say what kind of thing it found: ${JSON.stringify(r.rows)}`);
  equal(r.coll, "instances", "the palette did not go to the board the object is on");
  check(r.sheet.endsWith("/db-1"), `the object was not opened: ${r.sheet}`);
  await page.evaluate(`closeSheet()`);
});

/// Sharing a processor is a trade an operator makes on purpose — and a
/// capacity line that reported only one of the two numbers would say the cell
/// had grown a processor.
await test("the cell says how much processor it is promising over what it has", async () => {
  await open(page, "nodes");
  const strip = await waitFor(page, `(() => {
    const box = document.getElementById("cpuadvisory");
    return box && box.textContent.includes("Largest guest") ? box.textContent : null;
  })()`);
  check(/\d+ cores, offered as \d+/.test(strip),
    `the strip does not say what is promised over the silicon: ${strip}`);
  check(strip.includes("sharing theirs"), `it does not say why the two differ: ${strip}`);
});

await test("the nodes board says the cell is split and what a baseline would cost", async () => {
  // The fixture's cell is deliberately mixed: node-a and node-c are a
  // generation ahead of node-b. That is the case the strip exists for, and a
  // console only ever checked against identical machines is one whose strip
  // was never seen with anything in it.
  await page.evaluate(`show("nodes")`);
  const strip = await waitFor(page, `(() => {
    const box = document.getElementById("cpuadvisory");
    if (!box || box.classList.contains("hidden")) return null;
    return {
      text: box.textContent,
      lines: box.querySelectorAll(".cpuline").length,
    };
  })()`);
  check(strip, "the nodes board showed no CPU strip for a cell with two domains");
  check(strip.lines >= 2, `the strip carried ${strip.lines} lines`);
  check(
    /2 migration domains/.test(strip.text),
    "the strip does not say how many domains the cell has: " + strip.text,
  );
  // The price, named. A recommendation carrying only the benefit is the one
  // thing this surface must never be.
  check(
    /Cost:/.test(strip.text) && /avx2/.test(strip.text),
    "the baseline recommendation does not say what it costs: " + strip.text,
  );

  // And it is only on the nodes board: a strip about processors above a list
  // of volumes is chrome for a question nobody asked.
  await page.evaluate(`show("volumes")`);
  const elsewhere = await waitFor(page, `(() => {
    const box = document.getElementById("cpuadvisory");
    return box && box.classList.contains("hidden") ? 1 : null;
  })()`);
  check(elsewhere, "the CPU strip followed the operator off the nodes board");
});

await test("a guest that will not boot shows its last words, readably", async () => {
  // The whole point of capturing a console: a failed guest is the one with the
  // most to say. A console that showed only "Failed" is the reason somebody
  // ssh's into a hypervisor.
  await page.evaluate(`show("instances")`);
  const opened = await waitFor(page, `(() => {
    const row = [...document.querySelectorAll("#boardbody tr")]
      .find((r) => (r.dataset.name || "").endsWith("/web-9"));
    if (!row) return null;
    row.click();
    return 1;
  })()`);
  check(opened, "the failed guest never reached the board");

  const seen = await waitFor(page, `(() => {
    const pre = document.querySelector("#sheet .logblock");
    if (!pre) return null;
    return {
      text: pre.textContent,
      lines: pre.textContent.split("\\n").length,
      // A document is preformatted, not a truncated line with a tooltip.
      tag: pre.tagName,
      sheet: document.getElementById("sheet").textContent,
    };
  })()`);
  check(seen, "the console output is not shown on the sheet at all");
  equal(seen.tag, "PRE", "the console output is not preformatted");
  check(
    /Kernel panic/.test(seen.text),
    "the panic is not in what was rendered: " + seen.text,
  );
  check(seen.lines >= 5, `the output collapsed to ${seen.lines} lines`);
  // And the size beside it, so nobody takes a tail for the whole log.
  check(
    /Console written/.test(seen.sheet),
    "the sheet does not say how much the guest wrote, so a tail reads as everything",
  );
});

await test("the nodes board leads with what fits, not with what is free", async () => {
  // Free memory does not add up into a guest. A strip that showed only the
  // sum would tell somebody a large machine fits a cell that has no room for
  // one anywhere — which is the mistake this line exists to prevent.
  await page.evaluate(`show("nodes")`);
  const strip = await waitFor(page, `(() => {
    const box = document.getElementById("cpuadvisory");
    if (!box || box.classList.contains("hidden")) return null;
    const room = [...box.querySelectorAll(".cpuline")]
      .find((l) => l.textContent.startsWith("Room"));
    return room ? room.textContent : null;
  })()`);
  check(strip, "the nodes board shows nothing about what would fit");
  check(
    /Largest guest that fits anywhere/.test(strip),
    "the strip does not lead with what fits: " + strip,
  );
  check(
    /does not add up into one guest/.test(strip),
    "the strip shows a free total without saying it cannot be used as one: " + strip,
  );
});

/// Narrowing a long board by label, and the two things that make it honest:
/// the API does the narrowing, and the filter does not follow you elsewhere.
/// Doing one thing to several — and the half that decides whether the control
/// is trustworthy: what happens when some of them are refused.
await test("several guests can be stopped at once, and every refusal is named", async () => {
  await open(page, "instances");
  await page.evaluate(`closeSheet()`);
  // Clicked, not assigned: a checkbox that is set in script and never clicked
  // proves nothing about the control an operator uses.
  await page.evaluate(`[...document.querySelectorAll("[data-picks]")].slice(0, 2).forEach((b) => b.click())`);
  const picked = await page.evaluate(`JSON.stringify({
    count: document.querySelector(".pickcount") ? document.querySelector(".pickcount").textContent : "",
    names: [...view.picked],
    sheet: !!document.getElementById("sheet"),
  })`).then(JSON.parse);
  equal(picked.names.length, 2, `ticking two rows selected ${picked.names.length}`);
  check(picked.count.includes("2 selected"), `the bar does not say what is selected: ${picked.count}`);
  check(!picked.sheet, "ticking a row opened its sheet over the board");

  await page.evaluate(`document.querySelector('[data-bulk="stop"]').click()`);
  const outcome = await waitFor(page, `(() => {
    const box = document.getElementById("bulkresult");
    const t = box ? box.textContent : "";
    return t && !t.includes("Working") ? t : null;
  })()`, { timeout: 15000 });
  check(/\d+ done/.test(outcome), `the result does not say what happened: ${outcome}`);
  // Partial success is the normal case, so the panel must say which is which —
  // but for two guests nobody else is touching, both should land.
  check(!/refused/.test(outcome), `the API refused the change: ${outcome}`);

  // What landed, landed: the guests really were asked to stop.
  const asked = await page.evaluate(`(async () => {
    const r = await request("GET", "/api/v1/projects/p1/instances?pageSize=1000");
    const want = ${JSON.stringify(picked.names)};
    return (r.body.items || [])
      .filter((i) => want.includes(i.meta.name))
      .map((i) => i.spec.desiredState);
  })()`);
  check(asked.every((s) => s === "Stopped"),
    `the change did not reach the API: ${JSON.stringify(asked)}`);

  // And the selection is emptied of what worked, so a second press retries the
  // failures and nothing else.
  const leftOver = await page.evaluate(`view.picked.size`);
  check(leftOver < 2, `everything stayed selected after it was done: ${leftOver}`);

  // A board without a bulk action offers no picker column at all: a column of
  // checkboxes over a board with nothing to do with them is a control that
  // does nothing.
  // Nodes are neither created nor deleted from here — a machine joins a cell
  // by being registered — so there is nothing a selection of them could do.
  await open(page, "nodes");
  const boxes = await page.evaluate(`document.querySelectorAll("[data-picks]").length`);
  equal(boxes, 0, "a board with no bulk action still offered a selection column");
});

/// "What has happened to this thing" — including the half that lives where
/// nobody looks.
await test("an object's sheet shows what was asked of it, refusals included", async () => {
  await open(page, "instances");
  await openRow(page, "web-1");
  const said = await waitFor(page, `(() => {
    const box = document.getElementById("history");
    return box && !box.textContent.includes("Asking") ? box.textContent : null;
  })()`);

  check(said.includes("create"), `the change that made it is not there: ${said}`);
  check(said.includes("alice"), `who asked is not there: ${said}`);
  // A change that was accepted and then failed is not a change that happened.
  check(said.includes("the node refused the change"), `a failure is not shown: ${said}`);
  // The refusal, which is the answer to "I clicked delete and nothing
  // happened" and lives in a collection a tenant never opens.
  check(said.includes("refused") && said.includes("viewer"),
    `a refused request is not in the history: ${said}`);
  // Somebody else's history is not in it.
  check(!said.includes("web-2"), `another object's records leaked in: ${said}`);
  await page.evaluate(`closeSheet()`);
});

/// The hunt an overview exists to end: something is wrong somewhere, and the
/// answer used to be visiting eleven boards in turn.
await test("the overview names what is drifting and goes there in one click", async () => {
  await page.evaluate(`document.getElementById("railhome").click()`);
  const shown = await waitFor(page, `(() => {
    const box = document.getElementById("overviewbox");
    return view.home && box.textContent ? box.textContent : null;
  })()`);

  // The fixture has a guest whose ask moved and one that cannot be placed, so
  // the panel has something to say and must say it by name.
  const rows = await page.evaluate(`[...document.querySelectorAll(".overrow .linky")]
    .map((b) => b.dataset.goes)`);
  check(rows.length > 0, `nothing was named as needing attention: ${shown}`);
  check(rows.some((n) => n.includes("/instances/")),
    `a drifting guest is not on the overview: ${JSON.stringify(rows)}`);

  // Clicking one goes to the board it lives on *and* opens it. Opening a sheet
  // over a page the object is not on would leave whoever closes it looking at
  // the overview again, wondering whether they imagined it.
  const target = rows.find((n) => n.includes("/instances/"));
  await page.evaluate(`document.querySelector('.overrow .linky[data-goes=' + JSON.stringify(${JSON.stringify(target)}) + ']').click()`);
  const landed = await waitFor(page, `(() => {
    if (!sheet.open || !view.coll) return null;
    return { coll: view.coll.id, name: sheet.name, board: !document.querySelector(".boardwrap").classList.contains("hidden") };
  })()`);
  equal(landed.coll, "instances", "the click did not open the board the object is on");
  equal(landed.name, target, "the wrong object was opened");
  check(landed.board, "the board is still hidden behind the overview");
  await page.evaluate(`closeSheet()`);
});

/// Adding a machine to the cell from the console — and the one moment that
/// cannot be repeated.
await test("adding a node shows its registration token once, with what to do with it", async () => {
  await open(page, "nodes");
  const opened = await page.evaluate(`(() => {
    const button = document.getElementById("newbtn");
    if (!button) return "the nodes board offers no way to add one";
    button.click();
    return "";
  })()`);
  check(!opened, opened);

  await page.evaluate(`(async () => {
    const id = document.getElementById("f-id");
    id.value = "node-new";
    id.dispatchEvent(new Event("input"));
    document.getElementById("submitform").click();
  })()`);

  const shown = await waitFor(page, `(() => {
    const box = document.getElementById("agenttoken");
    return box ? document.getElementById("dialog").textContent : null;
  })()`, { timeout: 15000 });

  // The token itself, and the sentence that stops somebody expecting to find
  // it again later.
  check(shown.includes("b".repeat(64)), "the token is not on screen");
  check(/cannot show it again/.test(shown), `it does not say the token is once-only: ${shown}`);
  // The command it is for, with this node's id — the moment it is needed is
  // the moment it is on screen.
  check(shown.includes("velstra-cloud-node setup"), `no command to run: ${shown}`);
  check(shown.includes("node-new"), `the command does not name the node: ${shown}`);
  // And what the machine cannot decide about itself.
  check(/external traffic/.test(shown), `it does not say where the role is set: ${shown}`);

  // It has to be dismissed deliberately: a credential that scrolls away is a
  // node that has to be deleted and made again.
  await page.evaluate(`document.getElementById("tokendone").click()`);
  const gone = await page.evaluate(`!document.getElementById("agenttoken")`);
  check(gone, "the token panel would not close");
});

/// A node says what its hardware drags along, before anybody claims it.
///
/// Passing one device to a guest takes its whole isolation group, because the
/// hardware cannot separate less than that. The platform already refuses an
/// unsafe assignment; what did not exist was the sentence *before* the
/// decision, and an operator who learns it afterwards learns it from an outage.
await test("a node's hardware says what comes with each piece", async () => {
  await open(page, "nodes");
  await openRow(page, "node-a");
  const said = await waitFor(page, `(() => {
    const box = [...document.querySelectorAll(".pending")]
      .find((b) => /isolation group/.test(b.textContent));
    return box ? box.textContent : null;
  })()`);

  // The card and its audio function are one line: claiming either takes both.
  check(/0000:41:00\.0 \+ 0000:41:00\.1/.test(said), `the group is not shown: ${said}`);
  // A device alone in its group says so rather than saying nothing.
  check(/0000:42:00\.0 — on its own/.test(said), `a lone device is unclear: ${said}`);
  // And the case a client-side grouping gets backwards: no group at all is not
  // "grouped with the other ungrouped ones", it is "cannot be passed through".
  check(/0000:43:00\.0 — no isolation group/.test(said), `an un-isolatable device is mislabelled: ${said}`);
  check(!/0000:43:00\.0 \+/.test(said), `it lumped ungrouped devices together: ${said}`);
});

/// A guest resized while it runs says so, instead of reading as converged.
///
/// This is the failure that hid best: the spec said four vCPUs, the agent had
/// genuinely handled the change so the generation caught up, Ready was true —
/// and the guest went on running on two. Every screen agreed. The board now
/// carries the one column that disagrees, next to the numbers it contradicts.
await test("a guest running on old numbers says so on the board", async () => {
  await open(page, "instances");
  const row = await waitFor(page, `(() => {
    const cell = [...document.querySelectorAll("#boardbody td")]
      .find((c) => c.textContent.trim() === "web-1");
    return cell ? cell.closest("tr").textContent : null;
  })()`);

  // The row text is concatenated, so the vCPU column's "4" runs into the next
  // cell's "8,192 MiB" — asserting on a number here would be asserting on
  // where the columns happen to sit. What matters is that the row says the
  // resize has not taken effect, right beside the number that claims it has.
  check(/vcpus/i.test(row), `nothing says the resize is still pending: ${row}`);

  // And a guest running on what was asked for says nothing — a column that
  // always had something in it would be one nobody reads.
  const other = await page.evaluate(`(() => {
    const cell = [...document.querySelectorAll("#boardbody td")]
      .find((c) => c.textContent.trim() === "web-2");
    return cell ? cell.closest("tr").textContent : null;
  })()`);
  check(other !== null && !/vcpus/i.test(other), `it claims a settled guest is pending: ${other}`);
});

/// A volume's pool is where its bytes are, and re-pointing it moves none.
///
/// Before this, editing it was accepted by the API and moved nothing: the old
/// pool's agent stopped matching its watch filter and let go, the new one saw a
/// volume another pool still had claimed and declined it, and the volume simply
/// stopped converging with nothing anywhere saying why. The form must not offer
/// the control, and the size next to it must still work — a lock that seized
/// the whole dialog would be a worse bug than the one it fixes.
await test("a volume's pool cannot be edited, and the rest of the form still can", async () => {
  await open(page, "volumes");
  await openRow(page, "data-1");
  await page.evaluate(`document.getElementById("editbtn").click()`);

  const state = await waitFor(page, `(() => {
    const pool = document.getElementById("f-pool");
    if (!pool) return null;
    const size = document.getElementById("f-sizeGib");
    return {
      poolLocked: pool.hasAttribute("disabled"),
      sizeLocked: size ? size.hasAttribute("disabled") : "no size field",
    };
  })()`, { timeout: 15000 });

  check(state.poolLocked, "the edit form still offers to move a volume between pools");
  check(state.sizeLocked === false, `locking the pool disabled the size too: ${state.sizeLocked}`);
});

/// A pool is declared here, then claimed by an agent — the same two halves as
/// a node, in the same order. The id is the whole point: every volume is
/// written against it, and a seed naming a pool nobody created is a pool that
/// claims nothing and volumes that are never provisioned, quietly.
///
/// The second half of this test is the part worth having. A pool mints a
/// credential of its own — that is the only thing that lets its agent run on a
/// machine with no store of its own — and a console that swallowed it would
/// leave an operator with a pool object and no way to make the machine speak.
await test("a pool can be declared before its agent exists, and is handed its token", async () => {
  await open(page, "pools");
  const opened = await page.evaluate(`(() => {
    const button = document.getElementById("newbtn");
    if (!button) return "the pools board offers no way to add one";
    button.click();
    return "";
  })()`);
  check(!opened, opened);

  await page.evaluate(`(async () => {
    const id = document.getElementById("f-id");
    id.value = "nvme-2";
    id.dispatchEvent(new Event("input"));
    document.getElementById("submitform").click();
  })()`);

  const shown = await waitFor(page, `(() => {
    const box = document.getElementById("agenttoken");
    if (!box) return null;
    return { token: box.textContent.trim(), panel: box.closest("#dialog").textContent };
  })()`, { timeout: 15000 });

  check(shown.token.length === 64, `the pool's token came through mangled: ${shown.token}`);
  // The command has to be the pool's, not the node wizard's — a token pasted
  // into `velstra-cloud-node setup` is a token in the wrong file.
  check(shown.panel.includes("/etc/velstra/pool-token"),
    `the panel does not say where a pool token goes: ${shown.panel}`);
  await page.evaluate(`document.getElementById("tokendone").click()`);

  const row = await waitFor(page, `(() => {
    const cell = [...document.querySelectorAll("#rows td")]
      .find((c) => c.textContent.trim() === "nvme-2");
    return cell ? cell.closest("tr").textContent : null;
  })()`, { timeout: 15000 });

  // It exists and nothing has reported on it — which is the honest state for a
  // pool whose machine has not been installed yet, not an error.
  check(!/error/i.test(row), `a freshly declared pool reads as broken: ${row}`);
});

/// A tenant's two questions in one answer: what am I allowed, and what would
/// actually start.
await test("a project's sheet says what is left and which limit is in the way", async () => {
  await open(page, "projects");
  await openRow(page, "p1");
  const said = await waitFor(page, `(() => {
    const box = document.getElementById("allowance");
    return box && !box.textContent.includes("Asking") ? box.textContent : null;
  })()`);

  // The shape somebody could start, not a sum of free memory across the cell —
  // a hundred nodes with 2 GiB each cannot run a 4 GiB machine.
  check(said.includes("Largest guest that would start"), `no answer about what fits: ${said}`);
  // And which of the two is in the way, because "your quota" and "the
  // machines" are two entirely different afternoons.
  check(/limited by (your quota|the machines|both)/.test(said), `it does not say why: ${said}`);

  // An unset limit is not a limit of nothing.
  check(said.includes("no limit"), `an unset dimension did not read as unlimited: ${said}`);
  // And a dimension that was set reads as used-of-limit.
  check(/10 of 200/.test(said), `the vCPU line is not there: ${said}`);
});

/// The question an operator asks before the machine goes on a trolley: what is
/// scheduled, and which guests cannot leave.
await test("a node's sheet says what its maintenance window will cost", async () => {
  await open(page, "nodes");
  await openRow(page, "node-b");
  const said = await waitFor(page, `(() => {
    const box = document.getElementById("maintenance");
    return box && !box.textContent.includes("Asking") ? box.textContent : null;
  })()`);
  check(said.includes("Out of service"), `the sheet does not say it is out: ${said}`);
  check(said.includes("DIMM"), `it does not say what for: ${said}`);

  // node-c is not out yet, and the sheet says when it will be rather than
  // saying nothing until the hour arrives.
  await openRow(page, "node-c");
  const soon = await waitFor(page, `(() => {
    const box = document.getElementById("maintenance");
    const t = box ? box.textContent : "";
    return t && !t.includes("Asking") && !t.includes("Out of service") ? t : null;
  })()`);
  check(soon.includes("Scheduled"), `an upcoming window is not shown: ${soon}`);
  check(soon.includes("rack 4"), `it does not say what for: ${soon}`);
});

/// A node that is deliberately out of service, said on the board where its
/// The way back for a machine that already exists.
///
/// Registration mints a credential and the platform keeps only its hash, which
/// is right for a secret and wrong for the only way to get one. On a real
/// two-machine cell, the second box's pool had been registered before pools had
/// credentials at all: its agent could not start, and the only way to hand it a
/// token would have been to delete the pool every volume in it is written
/// against.
///
/// It asks first, and what it says while asking is the part worth testing: an
/// operator who reads this as a rotation leaves a machine authenticating on a
/// credential they believe they revoked.
await test("an existing machine can be handed a fresh agent token, and told what that does not do", async () => {
  // The pool the earlier test declared, which is the shape this exists for: a
  // machine that already exists and needs a token now.
  await open(page, "pools");
  await openRow(page, "nvme-2");

  const asked = await waitFor(page, `(() => {
    const button = document.getElementById("issuecredbtn");
    if (!button) return null;
    button.click();
    const warn = document.getElementById("issuecredwarning");
    return warn ? warn.textContent : "the control asked nothing";
  })()`, { timeout: 15000 });
  check(/does not revoke/i.test(asked), `the warning does not say what it leaves alone: ${asked}`);

  await page.evaluate(`document.getElementById("confirmissuecred").click()`);
  const shown = await waitFor(page, `(() => {
    const box = document.getElementById("agenttoken");
    return box ? { token: box.textContent.trim(), panel: box.closest("#dialog").textContent } : null;
  })()`, { timeout: 15000 });
  check(shown.token.length === 64, `the token came through mangled: ${shown.token}`);
  check(shown.panel.includes("/etc/velstra/pool-token"),
    `the panel does not say where a pool token goes: ${shown.panel}`);
  await page.evaluate(`document.getElementById("tokendone").click()`);

  // And back to a button, so a second one can be issued without a reload.
  const again = await waitFor(page, `!!document.getElementById("issuecredbtn")`, { timeout: 15000 });
  check(again, "the control did not come back after issuing");
});

/// absence would otherwise read as a fault.
await test("the nodes board says which machines are out and when they come back", async () => {
  await open(page, "nodes");
  const strip = await waitFor(page, `(() => {
    const box = document.getElementById("cpuadvisory");
    return box && !box.classList.contains("hidden") && box.textContent.includes("out of service")
      ? box.textContent : null;
  })()`);

  check(strip.includes("node-b"), `the open window does not name the node: ${strip}`);
  // Relative, not a clock time: "back at 03:00" does not say whether that has
  // happened, and the operator is deciding about the next forty minutes.
  check(/for another \d+ minutes/.test(strip), `it does not say for how much longer: ${strip}`);
  check(strip.includes("DIMM"), `it does not say what for: ${strip}`);

  // And the one nobody has been surprised by yet.
  check(strip.includes("Scheduled") && strip.includes("node-c"),
    `the upcoming window is not on the board: ${strip}`);
  check(strip.includes("in 3 hours"), `it does not say when: ${strip}`);
});

await test("a label filter narrows the board and says so", async () => {
  await open(page, "instances");
  const all = await page.evaluate(`view.items.length`);
  check(all > 1, `there is nothing to narrow: ${all} rows`);

  // The buffer fills up over a long run, and a full one records nothing new.
  await page.evaluate(`performance.clearResourceTimings()`);
  await page.evaluate(`
    document.getElementById("labelfilter").value = "env=prod";
    document.getElementById("applyfilter").click();
  `);
  await sleep(600);
  await waitFor(page, `view.items.length < ${all}`);

  const narrowed = await page.evaluate(`({
    rows: view.items.length,
    names: view.items.map((i) => i.meta.name),
    note: document.querySelector(".filternote") ? document.querySelector(".filternote").textContent : "",
    // What the console actually asked the network for. If no request carries
    // the selector, the narrowing happened here — which means the whole cell
    // was fetched to show two rows of it, the cost a filter exists to avoid.
    asked: performance.getEntriesByType("resource").map((e) => e.name).join(" "),
  })`);
  check(narrowed.rows < all, `the filter kept every row: ${narrowed.rows} of ${all}`);
  check(narrowed.names.every((n) => n.endsWith("/web-1")),
    `something without env=prod survived: ${narrowed.names.join(", ")}`);
  check(narrowed.asked.includes("labels=env"), "the API was never asked to narrow — the console fetched the whole cell");
  // A short list with no visible reason is how somebody concludes their guests
  // are gone.
  check(narrowed.note.includes("env=prod"), `the board does not say why it is short: ${narrowed.note}`);

  // The rail still counts the collection, not the board. A filter is a way of
  // looking at a list; it does not change how many guests a tenant has, and a
  // rail that said otherwise would be reporting the filter as an outage.
  const railSays = await page.evaluate(`census.instances.total`);
  equal(railSays, all, "the rail counted the filtered board instead of the collection");

  // A picker is not that board. One that quietly offered two of the five
  // guests in the project — because of something typed on a list elsewhere —
  // would be a wrong answer with no visible cause.
  const offered = await page.evaluate(`(async () => {
    forgetOptions("instances");
    return (await options("instances")).length;
  })()`);
  equal(offered, all, "the filter narrowed a picker as well as the board");

  // Leaving the board drops it. A filter typed about guests that silently
  // shortened the volumes board would be the same confusion, one screen later.
  await open(page, "volumes");
  const carried = await page.evaluate(`session.labels`);
  equal(carried, "", "the filter followed us to another board");

});

await test("a reload with a live token comes back signed in", async () => {
  // The console kept its token in `sessionStorage` and had a path to resume from
  // it — which called a function that does not exist. The ReferenceError was
  // thrown *synchronously*, so the `.catch` meant to fall back to the sign-in
  // screen never ran, and the only evidence was one line in a console nobody has
  // open. Every reload asked the operator to sign in again, with a perfectly good
  // session in the tab.
  await open(page, "instances");
  const token = await page.evaluate(`sessionStorage.getItem("velstra-cloud-token")`);
  check(!!token, "the session left no token to come back with");

  await page.goto(URL);
  const back = await page.evaluate(`({
    inside: !document.getElementById("app").classList.contains("hidden"),
    token: sessionStorage.getItem("velstra-cloud-token"),
  })`);
  check(back.inside, "a reload with a live token landed on the sign-in screen");
  check(back.token === token, "the token did not survive the reload");
});

await test("signing out leaves nothing behind", async () => {
  // "Nothing behind" used to mean the token and the panel, and that is what this
  // checked — an assertion that held while everything the session had read was
  // still sitting in memory. Sign out on a shared machine, sign in as somebody
  // else, and the rail still counted the previous tenant's objects, the board
  // still held their rows, and the picker cache still offered their ports and
  // volumes by name. The picker was the worst of the three: `optionCache` holds
  // resolved promises and is only ever invalidated by a write or a watch event,
  // so it survived not just the sign-out but the next session entirely.
  //
  // So the check is now over everything the console accumulates, by name. A new
  // cache added later and not cleared is a line here that has to be added — and
  // if it is not, this test is the thing that was supposed to notice.
  await open(page, "instances");
  // Make the caches real first: a picker fills `optionCache`, a board fills
  // `view`, and signing in filled `census` and `scopeFound`.
  await page.evaluate(`options("volumes")`);
  const before = await page.evaluate(`({
    options: optionCache.size,
    census: Object.keys(census).length,
    items: view.items.length,
    scopes: Object.keys(scopeFound).length,
  })`);
  // And a filter, which is the newest thing a session accumulates.
  await page.evaluate(`session.labels = "env=prod"`);
  check(before.options > 0 && before.census > 0 && before.items > 0,
    `nothing was cached, so this test would pass without checking anything: ${JSON.stringify(before)}`);

  await page.evaluate(`document.getElementById("signout").click()`);
  const left = await page.evaluate(`({
    inside: !document.getElementById("app").classList.contains("hidden"),
    token: sessionStorage.getItem("velstra-cloud-token"),
    project: sessionStorage.getItem("velstra-cloud-project"),
    options: optionCache.size,
    census: Object.keys(census).length,
    items: view.items.length,
    coll: view.coll ? view.coll.id : null,
    revision: view.revision,
    scopes: Object.keys(scopeFound).length,
    labels: session.labels,
    home: view.home,
    overview: document.getElementById("overviewbox").textContent.trim(),
    rail: document.getElementById("rail").textContent.trim(),
    board: document.querySelectorAll("#boardbody tr").length,
  })`);
  check(!left.inside, "signing out left the console up");
  check(!left.token, "the token survived signing out");
  check(!left.project, "the project this session was looking at survived signing out");
  equal(left.options, 0, "the picker cache survived signing out, names and all");
  equal(left.census, 0, "the rail's counts survived signing out");
  equal(left.items, 0, "the board's rows survived signing out");
  equal(left.coll, null, "the console still believes it is showing a collection");
  check(!left.revision, "the watch revision survived signing out");
  equal(left.scopes, 0, "where each collection was found survived signing out");
  equal(left.labels, "", "the previous session's label filter survived signing out");
  equal(left.home, false, "the console still believes it is showing the overview");
  equal(left.overview, "", "the previous session's overview is still on screen");
  equal(left.rail, "", "the rail still lists the previous session's collections");
  equal(left.board, 0, "the previous session's rows are still on screen");
});

page.close();



process.exit(summary());
