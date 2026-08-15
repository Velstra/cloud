// What the console has to actually do, checked in a browser against a running
// API.
//
// Read a failure here as "an operator cannot do X", not as unit noise. Most of
// these exist because the thing they check is invisible when it breaks: the
// page still loads, the layout is still right, and the answer on screen is
// quietly wrong or the button quietly does nothing.

import { browser, signIn, open, openRow, sheetText, sleep, waitFor, test, check, equal, skip, summary } from "./harness.mjs";

const URL = process.env.CONSOLE_URL || "http://127.0.0.1:18100/";
const TOKEN = process.env.CONSOLE_TOKEN || "testtoken";
// The scaffolding endpoints only the in-memory API has. Against a real API
// these tests are skipped rather than faked.
const SCAFFOLDED = process.env.CONSOLE_SCAFFOLD !== "0";

const api = (path, init) => fetch(new globalThis.URL(path, URL), {
  ...init,
  headers: { Authorization: "Bearer " + TOKEN, "Content-Type": "application/json", ...(init || {}).headers },
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
    return r ? { id: idOf(r), name: nameOf(r) } : null;
  })()`);
  if (!found) skip(`this API's seed holds no ${what}`);
  return found;
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

await signIn(page, TOKEN);

await test("signing in lands on a collection, with the rail beside it", async () => {
  const seen = await page.evaluate(`({
    rail: [...document.querySelectorAll(".railitem")].map((b) => b.dataset.collection),
    title: document.getElementById("listtitle").textContent,
    rows: document.querySelectorAll("#boardbody tr").length,
  })`);
  equal(seen.rail.length, 11, "the rail does not list every collection");
  check(seen.title === "Instances", `landed on ${seen.title}`);
  check(seen.rows >= 1, "the board showed no instances at all");
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
    if (seen.head[1] !== "Convergence") wrong.push(`${id}: no convergence column`);
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
  const found = await page.evaluate(`(async () => {
    const coll = collection("nodes");
    const was = scopeFound.nodes;
    delete scopeFound.nodes;
    const original = coll.scope;
    coll.scope = "project";               // deliberately the wrong one
    try {
      const r = await list(coll);
      return { items: r.items.length, resolved: scopeFound.nodes };
    } finally { coll.scope = original; scopeFound.nodes = was; }
  })()`);
  check(found.items >= 1, "the fallback found no nodes");
  equal(found.resolved, "global", "the fallback did not remember where they really are");
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
  await sleep(600);
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
    } catch (e) { return null; }
    const r = view.items.find((x) => at(status(x), "node") && !busy.has(nameOf(x)));
    return r ? { id: idOf(r), name: nameOf(r), node: at(status(r), "node") } : null;
  })()`);
  if (!found) skip("this API's seed holds no running guest that is not already moving");
  return found;
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
  await open(page, "migrations");
  const it = await pick("migration in flight to time out",
    `(x) => !deletedAt(x) && condition(x, "Moved")?.status === "Unknown"`);
  // The interval, not its length: fifteen seconds is right for an operator and
  // wrong for a test, and what is being checked is that it asks at all.
  await page.evaluate(`(async () => {
    collection("migrations").recheck = 2;
    await show("migrations");
  })()`);
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
  await page.evaluate(`(async () => {
    closeSheet();
    collection("migrations").recheck = 15;
  })()`);
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

await test("signing out leaves nothing behind", async () => {
  await page.evaluate(`document.getElementById("signout").click()`);
  const left = await page.evaluate(`({
    inside: !document.getElementById("app").classList.contains("hidden"),
    token: sessionStorage.getItem("velstra-cloud-token"),
  })`);
  check(!left.inside, "signing out left the console up");
  check(!left.token, "the token survived signing out");
});

page.close();
process.exit(summary());
