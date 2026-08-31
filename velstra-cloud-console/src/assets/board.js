// The rail and the list — where am I, where can I go, what is there.

const view = {
  coll: null,          // the collection being shown
  /// Whether the overview is what is on screen. Distinct from `coll === null`,
  /// which is also true of a signed-out console with nothing shown at all.
  home: false,
  /// The same, for the network map. Two flags rather than one enum because the
  /// rail asks each of them separately and a third screen would add a third
  /// question, not a third value to compare against.
  map: false,
  items: [],
  // Whether `items` is the whole collection. False only when a paged walk had to
  // be abandoned — see `walk` in api.js. It is here rather than implied by the
  // row count because the whole hazard is that a short list looks exactly like a
  // small collection.
  complete: true,
  watcher: null,
  revision: null,
  rechecker: null,     // see startRecheck
  /// Which rows are selected, by name. Names rather than indices: the board
  /// re-sorts and re-renders under a watch event, and a selection of positions
  /// would quietly come to mean different objects.
  picked: new Set(),
};

// How many objects each collection holds and how many of them are not settled.
// Swept once after sign-in, so the rail answers "where is the problem" before
// anybody clicks: an operator should not have to open ten screens to find out
// that nothing is wrong.
const census = {};

/// What the cell keeps for itself.
///
/// A tenant was shown the whole rail — nodes, pools, **Ceph**, device classes,
/// maintenance windows, the other projects — and every one of those boards
/// answered them 403 or empty. It is not a leak, but it is worse than useless:
/// the point of a tenant's console is the things they have, and being offered
/// the machine room and refused at the door teaches them the platform is broken.
///
/// The scope is not the test, because images are cell-wide *and* everybody's.
/// What these have in common is that they are the estate, not the tenancy.
// Project collections that are nonetheless the operator's business. A
// migration names the machines a guest moves between, and machines are not
// part of a tenant's view — the API refuses them the whole object. Ports exist
// so an address can outlive its machine; they are made and removed by the
// platform, and a board of them is plumbing a customer never asked to see.
const OPERATOR_ONLY = ["migrations", "ports"];

/// The columns a board shows this account.
///
/// One filter, one reason: machine names are not part of a tenant's view. The
/// API already takes them off the objects, so for a tenant these columns could
/// only ever be a header over blanks — a question the page keeps asking with
/// no answer coming.
function columnsFor(coll) {
  if (session.who && session.who.cellAdmin) return coll.columns;
  return coll.columns.filter((c) => !/(^|\.)node$/.test(c.path));
}

const CELL_ONLY = [
  // Kept in step with `authz::belongs_to_the_cell`, which is what the API
  // refuses — a guard checks the two agree. `ceph-clusters` is the collection's
  // id; `ceph` is only its title.
  "nodes", "pools", "ceph-clusters", "device-classes", "maintenance-windows",
  "image-sources", "projects", "users", "audit", "backup-targets",
];

function groups() {
  const out = [];
  for (const c of collections()) {
    const admin = session.who && session.who.cellAdmin;
    if ((CELL_ONLY.includes(c.id) || OPERATOR_ONLY.includes(c.id)) && !admin) continue;
    let g = out.find((x) => x.name === c.group);
    if (!g) { g = { name: c.group, items: [] }; out.push(g); }
    g.items.push(c);
  }
  return out;
}

function renderRail() {
  const rail = $("rail");
  clear(rail);
  // Deliberately not a `.railitem`: it is not a collection, and everything that
  // walks the rail expecting one — the wayfinding checks, the census — would be
  // walking one thing that has no board, no columns and no condition.
  const attention = unsettledEverywhere().length;
  rail.appendChild(el("button.railhome" + (view.coll === null && view.home ? ".on" : ""),
    { type: "button", id: "railhome", onclick: () => showOverview() },
    el("span", "Overview"),
    attention
      ? el("span.state.drifting", mark("drifting"), String(attention))
      : el("span.n", "")));
  for (const g of groups()) {
    rail.appendChild(el("div.railgroup", g.name));
    // The network map sits at the top of its own group, above the collections
    // it is drawn from. Like the overview it is not a `.railitem`: it has no
    // board, no columns and no condition, and everything that walks the rail
    // expecting one would trip over it.
    if (g.name === "Network") {
      rail.appendChild(el("button.railhome" + (view.map ? ".on" : ""),
        { type: "button", id: "railmap", onclick: () => showTopology() },
        el("span", "Map"), el("span.n", "")));
    }
    for (const c of g.items) {
      const seen = census[c.id];
      const unsettled = seen ? seen.unsettled : 0;
      rail.appendChild(el("button.railitem" + (view.coll && view.coll.id === c.id ? ".on" : ""),
        { type: "button", "data-collection": c.id, onclick: () => show(c.id) },
        el("span", c.title),
        // A count is shown when it is known and a drift count when there is
        // one. Amber here means exactly what it means everywhere else.
        unsettled
          ? el("span.state.drifting", mark("drifting"), String(unsettled))
          : el("span.n", seen ? String(seen.total) + (seen.complete === false ? "+" : "") : "")));
    }
  }
}

function renderFleet() {
  const box = $("fleet");
  clear(box);
  const known = Object.values(census).filter((c) => c.ok);
  if (!known.length) return;
  const total = known.reduce((n, c) => n + c.total, 0);
  const unsettled = known.reduce((n, c) => n + c.unsettled, 0);
  const failed = Object.values(census).filter((c) => !c.ok).length;
  const partial = known.some((c) => c.complete === false);
  box.appendChild(el("span.muted", (partial ? "at least " : "") + total + " objects"));
  box.appendChild(unsettled
    ? el("span.state.drifting", mark("drifting"), unsettled + " not settled")
    : el("span.state.settled", mark("settled"), "all settled"));
  if (failed) box.appendChild(el("span.state.failing", mark("failing"), failed + " unreadable"));
}

/// Everything the last sweep found that has not settled, newest first.
///
/// Read off the census rather than fetched again: the sweep already lists every
/// collection on the way in, and asking a second time would double the cost of
/// signing in to show the same objects.
function unsettledEverywhere() {
  const out = [];
  for (const c of collections()) {
    const seen = census[c.id];
    if (!seen || !seen.ok) continue;
    for (const item of seen.unsettledItems || []) out.push({ coll: c, item });
  }
  return out;
}

async function sweep() {
  await Promise.all(collections().map(async (c) => {
    // The same rule the rail draws by. Sweeping a collection the rail hides
    // from this account is ten requests whose answers are known — and, until
    // the 403 handling below existed, ten red rows on a tenant's overview.
    if ((CELL_ONLY.includes(c.id) || OPERATOR_ONLY.includes(c.id))
        && !(session.who && session.who.cellAdmin)) {
      census[c.id] = { ok: true, denied: true, total: 0, unsettled: 0 };
      return;
    }
    try {
      const r = await list(c);
      const unsettled = r.items.filter((x) => verdict(x, c.condition).kind !== "settled");
      census[c.id] = {
        ok: true,
        total: r.items.length,
        // The objects themselves, not just how many. A count tells somebody
        // that something is wrong and then makes them go looking for it, one
        // board at a time — which is the hunt an overview exists to end.
        //
        // Capped: a cell where four hundred guests are drifting has one
        // problem, not four hundred, and a page that listed them all would
        // bury the sentence that says so.
        unsettledItems: unsettled.slice(0, ATTENTION_SHOWN),
        // Names only, so an operator can be sent to an object by name rather
        // than made to guess which board it is on. The listing is already in
        // hand, so this costs one map and some strings; asking again per
        // keystroke would cost the cell.
        names: r.items.map(nameOf),
        // A count that is the length of what arrived, when what arrived is not
        // the collection, is the failure worth being loud about: it reads as an
        // answer. `complete` comes back false only when a walk had to be given
        // up on, and everywhere the number is shown it is marked as a floor.
        complete: r.complete !== false,
        unsettled: unsettled.length,
      };
    } catch (e) {
      // "You may not look" is not "something is broken". A tenant's sweep runs
      // into the cell's own collections — nodes, pools, users — and the API
      // refuses each with a sentence, which is right. Rendering those as eight
      // red `unreadable` rows taught a customer's first overview that the
      // console was on fire. A refusal is recorded as not-mine and stays off
      // the attention list; every *other* failure is still the alarm it was.
      census[c.id] = e && e.status === 403
        ? { ok: true, denied: true, total: 0, unsettled: 0 }
        : { ok: false, total: 0, unsettled: 0, why: e.message };
    }
  }));
  renderRail();
  renderFleet();
  // The overview is a view *of* the sweep, so it is redrawn when the sweep
  // finishes rather than fetching the same lists a second time.
  if (view.home) renderOverviewBody();
}

/// How many drifting objects the overview names before it stops naming them.
///
/// A cell where four hundred guests are drifting has one problem, not four
/// hundred. Six is enough to see a pattern — "all of them are on node-c" — and
/// short enough that the line underneath, which says how many more there are,
/// is still on the screen.
const ATTENTION_SHOWN = 6;

/// Recount one collection from what is on screen, so the rail agrees with the
/// board without a second round trip.
function recount() {
  if (!view.coll) return;
  // A narrowed board is not the collection, and counting it as one makes the
  // rail say a tenant has one guest because they typed `env=prod`. The sweep's
  // numbers are about the whole collection and stay until the next sweep —
  // which is the honest answer to "how many are there".
  if (session.labels) return;
  const unsettled = view.items.filter((x) => verdict(x, view.coll.condition).kind !== "settled");
  census[view.coll.id] = {
    ok: true,
    total: view.items.length,
    // Recounted from the board, so it inherits whatever the listing that filled
    // the board managed to read.
    complete: view.complete !== false,
    unsettledItems: unsettled.slice(0, ATTENTION_SHOWN),
    // Recomputed rather than left behind. This replaces the census entry for
    // the collection on screen, and an entry rebuilt without them would empty
    // the palette's index of exactly the collection somebody is looking at.
    names: view.items.map(nameOf),
    unsettled: unsettled.length,
  };
  renderRail();
  renderFleet();
}

/// Everything the board remembers, and everything it has put on screen.
///
/// The counts are the part worth being explicit about: `census` feeds the rail
/// and the fleet bar, so leaving it means the next person to sign in is told how
/// many objects the *previous* one had — for as long as it takes the new
/// session's sweep to come back, which is a round trip per collection and is not
/// awaited.
function forgetBoard() {
  for (const key of Object.keys(census)) delete census[key];
  view.coll = null;
  view.home = false;
  view.picked.clear();
  bulkOutcome = null;
  clear($("picked"));
  $("picked").classList.add("hidden");
  view.items = [];
  view.revision = null;
  view.complete = true;
  clear($("rail"));
  clear($("fleet"));
  clear($("boardbody"));
  clear($("boardhead"));
  clear($("overviewbox"));
  $("overviewbox").classList.add("hidden");
  $("topologybox").classList.add("hidden");
  closeScreen();
  view.map = false;
}

function cell(r, col) {
  const raw = at(r, col.path);
  const blank = raw === null || raw === undefined || raw === "";
  const td = el("td");
  if (blank && col.cell !== "yes" && col.cell !== "count") {
    td.className = "blank";
    td.appendChild(document.createTextNode("—"));
    return td;
  }
  switch (col.cell) {
    case "mono":
      td.className = "mono";
      td.title = String(raw);
      td.appendChild(document.createTextNode(shortName(raw)));
      break;
    case "number":
      td.className = "num";
      td.appendChild(document.createTextNode(
        Number(raw).toLocaleString() + (col.unit ? " " + col.unit : "")));
      break;
    case "bytes":
      td.className = "num";
      td.appendChild(document.createTextNode(bytes(raw)));
      break;
    case "count":
      td.className = "num";
      td.appendChild(document.createTextNode(String((raw || []).length)));
      break;
    case "yes":
      // Neutral on purpose: being attached or encrypted is what a thing is,
      // not a verdict on it, and the four signal colours are not spent here.
      td.appendChild(document.createTextNode(
        raw === null || raw === undefined || raw === "" || raw === false ? col.no : col.yes));
      if (!raw) td.className = "faint";
      break;
    case "ago":
      td.title = stamp(raw);
      td.appendChild(document.createTextNode(ago(raw)));
      break;
    default:
      td.appendChild(document.createTextNode(String(raw)));
  }
  return td;
}

function renderBoard() {
  const coll = view.coll;
  const many = bulkActions(coll).length > 0;
  fill($("boardhead"),
    // A picker column only where there is something to do with a selection.
    // A column of checkboxes above a board with no bulk action is a control
    // that does nothing, which is worse than no control at all.
    many ? el("th.pickcol", el("input", {
      type: "checkbox", id: "pickall", "aria-label": "Select every row",
      onclick: (e) => { e.stopPropagation(); pickEveryRow(e.target.checked); },
    })) : null,
    el("th", { style: "width:220px" }, coll.singular === "project" ? "Project" : "Name"),
    el("th", { style: "width:150px" }, "Convergence"),
    columnsFor(coll).map((c) => el("th", { style: "width:" + c.width + "px" }, c.label)));

  const body = $("boardbody");
  clear(body);
  const rows = view.items.slice().sort((a, b) => nameOf(a).localeCompare(nameOf(b)));
  for (const r of rows) {
    const tr = el("tr", { tabindex: "0", "data-name": nameOf(r) });
    const open = () => openSheet(coll, r);
    tr.addEventListener("click", open);
    tr.addEventListener("keydown", (e) => { if (e.key === "Enter") open(); });
    if (many) {
      // The click is stopped here: a row opens its sheet, and picking a row is
      // not opening it. Without this every tick would also open a panel over
      // the board somebody is ticking through.
      tr.appendChild(el("td.pickcol", el("input", {
        type: "checkbox",
        "data-picks": nameOf(r),
        "aria-label": "Select " + idOf(r),
        checked: view.picked.has(nameOf(r)) ? "" : null,
        onclick: (e) => { e.stopPropagation(); pickRow(nameOf(r), e.target.checked); },
      })));
    }
    tr.appendChild(el("td.name", el("span.id", { title: nameOf(r) }, idOf(r))));
    tr.appendChild(el("td", stateOf(r, coll.condition)));
    for (const c of columnsFor(coll)) tr.appendChild(cell(r, c));
    body.appendChild(tr);
  }

  const empty = $("listempty");
  empty.classList.toggle("hidden", rows.length > 0);
  empty.textContent = rows.length ? "" :
    "No " + coll.title.toLowerCase() + " here yet." +
    (coll.creatable ? " Create the first one above." : "");
  $("board").classList.toggle("hidden", rows.length === 0);
  renderPicked();
}

// ---- doing one thing to several ---------------------------------------------

/// What can be done to a selection, on this board.
///
/// Deliberately few. Everything here is a change somebody can already make one
/// object at a time; what this adds is not new power but not having to do it
/// forty times. Anything that needs a *decision* per object — a migration's
/// destination, a resize — is not on this list and should not be: a control
/// that applies one answer to forty questions is a control that gets one of
/// them badly wrong.
function bulkActions(coll) {
  const out = [];
  if (coll.id === "instances") {
    out.push({ id: "start", label: "Start", body: { spec: { desiredState: "Running" } } });
    out.push({ id: "stop", label: "Stop", body: { spec: { desiredState: "Stopped" } } });
  }
  if (coll.deletable) out.push({ id: "delete", label: "Delete", destroys: true });
  return out;
}

function pickRow(name, on) {
  if (on) view.picked.add(name); else view.picked.delete(name);
  renderPicked();
}

function pickEveryRow(on) {
  view.picked.clear();
  if (on) for (const r of view.items) view.picked.add(nameOf(r));
  renderBoard();
}

/// What the last bulk run reported, kept until the board changes.
///
/// Held rather than left on screen by accident: the run empties the selection,
/// and a bar that vanished with it would take the answer — "two refused, and
/// here is what the API said" — off the screen at the moment it was written.
let bulkOutcome = null;

/// The bar that appears when something is selected, and what it offers.
function renderPicked() {
  const bar = $("picked");
  if (!bar) return;
  clear(bar);
  const coll = view.coll;
  const n = view.picked.size;
  bar.classList.toggle("hidden", !coll || (n === 0 && !bulkOutcome));
  if (!coll || (!n && !bulkOutcome)) return;

  if (n) {
    bar.appendChild(el("span.pickcount", n + " selected"));
    for (const a of bulkActions(coll)) {
      bar.appendChild(el("button.btn" + (a.destroys ? ".danger" : ""), {
        type: "button", "data-bulk": a.id, onclick: () => askBulk(coll, a),
      }, a.label));
    }
  }
  bar.appendChild(el("button.btn", { type: "button", id: "pickclear",
    onclick: () => { view.picked.clear(); bulkOutcome = null; renderBoard(); } },
    n ? "Clear" : "Dismiss"));
  const host = el("span.bulkresult", { id: "bulkresult" });
  if (bulkOutcome) for (const node of bulkOutcome) host.appendChild(node);
  bar.appendChild(host);
}

/// Deleting several things asks once, and names what it is about to delete.
///
/// The same rule as the single delete: destructive and irreversible asks, and
/// everything else does not — which is what keeps the question meaningful.
/// Naming the objects rather than counting them is the difference between
/// "delete 12?" and seeing `db-1` in the list and stopping.
function askBulk(coll, action) {
  if (!action.destroys) return runBulk(coll, action);
  const host = $("bulkresult");
  const names = [...view.picked].map(shortName);
  fill(host,
    el("span.err", "Delete " + names.slice(0, 5).join(", ") +
      (names.length > 5 ? " and " + (names.length - 5) + " more" : "") + "? "),
    el("button.btn.danger", { type: "button", id: "bulkyes",
      onclick: () => runBulk(coll, action) }, "Delete " + names.length),
    el("button.btn", { type: "button", id: "bulkno",
      onclick: () => clear(host) }, "Keep them"));
}

/// Do it, one at a time, and report every outcome.
///
/// Sequential on purpose. Each of these is a compare-and-swap against an object
/// a controller may also be writing, and forty at once turns ordinary
/// contention into a wall of retries — while the operator watches a spinner
/// that says nothing about which ones landed.
///
/// And **partial success is the normal case**, not an error path: a tenant
/// stopping twelve guests may hold a lease on eleven of them. So the answer is
/// per object, in the API's own words, and the selection keeps exactly what
/// did not work — so a second press retries the failures and nothing else.
async function runBulk(coll, action) {
  bulkOutcome = null;
  const host = $("bulkresult");
  const names = [...view.picked];
  fill(host, el("span.muted", "Working… 0 of " + names.length));
  const failed = [];
  let done = 0;
  for (const name of names) {
    // The bare id, which is what every write path takes. `shortName` is for
    // reading — it keeps the collection in front of the id — and sending that
    // asks the API about an object called `instances/db-1`.
    const id = name.split("/").pop();
    try {
      if (action.destroys) await remove(coll, id);
      else await patch(coll, id, action.body);
      done++;
      view.picked.delete(name);
    } catch (e) {
      failed.push({ id, why: e.message });
    }
    fill(host, el("span.muted", "Working… " + done + " of " + names.length));
  }

  bulkOutcome = failed.length
    // Every refusal, in the API's own words. A count of failures is a message
    // that sends somebody to try them one at a time to find out why.
    ? [
        el("span.state.failing", mark("failing"),
          done + " done, " + failed.length + " refused"),
        el("div.bulkfails", failed.map((f) =>
          el("div", el("span.mono", f.id), el("span.muted", " — " + f.why)))),
      ]
    : [el("span.state.settled", mark("settled"), done + " done")];
  renderBoard();
}

function renderListHead() {
  const coll = view.coll;
  $("listtitle").textContent = coll.title;
  $("listblurb").textContent = coll.blurb;
  const acts = fill($("listacts"));
  if (coll.creatable) {
    acts.appendChild(el("button.btn.primary", { type: "button", id: "newbtn",
      onclick: () => openCreate(coll) }, "New " + coll.singular));
  }
  acts.appendChild(el("button.btn", { type: "button", id: "refreshbtn",
    onclick: () => show(coll.id) }, "Refresh"));
  renderFilter(coll);
}

/// The label filter, on the boards long enough to need one.
///
/// Applied by the API, not here: filtering client-side would mean fetching the
/// whole cell to show six rows of it, which is the cost a filter exists to
/// avoid on exactly the boards where anybody wants one.
///
/// Cleared when the board changes. A filter typed on the instances board that
/// silently followed somebody to volumes would be a short list with no visible
/// reason for it.
function renderFilter(coll) {
  const box = $("listfilter");
  if (!box) return;
  clear(box);
  box.classList.toggle("hidden", !FILTERABLE.has(coll.id));
  if (!FILTERABLE.has(coll.id)) return;

  const input = el("input.filterbox", {
    id: "labelfilter",
    type: "search",
    value: session.labels,
    placeholder: "env=prod, tier=web",
    "aria-label": "Narrow by label",
    onkeydown: (e) => {
      if (e.key === "Enter") applyFilter(coll, e.target.value);
      // Escape clears rather than reverting: somebody reaching for it wants
      // the whole list back, and a filter that needed two gestures to undo is
      // one people leave on by accident.
      if (e.key === "Escape") applyFilter(coll, "");
    },
  });
  box.appendChild(el("label.filterlabel", { for: "labelfilter" }, "Labels"));
  box.appendChild(input);
  box.appendChild(el("button.btn", { type: "button", id: "applyfilter",
    onclick: () => applyFilter(coll, input.value) }, "Narrow"));
  if (session.labels) {
    box.appendChild(el("button.btn", { type: "button", id: "clearfilter",
      onclick: () => applyFilter(coll, "") }, "Show all"));
    box.appendChild(el("span.filternote.muted",
      "Showing only what carries " + session.labels + "."));
  }
}

function applyFilter(coll, text) {
  session.labels = String(text || "").trim();
  show(coll.id);
}

/// Where a filter is worth offering.
///
/// The long collections, not every one. A filter box above a list of four
/// routers is a control that costs a glance and answers nothing.
const FILTERABLE = new Set(["instances", "volumes", "ports", "networks", "nodes", "images"]);


/// The cell's CPU picture, shown above the node list.
///
/// Only on the nodes board, and only when there is something to say. A cell of
/// identical machines gets one quiet line; a mixed one gets the domains and
/// the recommendation, each with its cost.
///
/// Asked separately from the list because it is a different question with a
/// different answer shape — and because a fleet-wide report should not delay
/// the rows. A failure here hides the strip and leaves the board intact: not
/// knowing what could be baselined is not a reason to stop showing the nodes.
async function renderCpuAdvisory() {
  const box = $("cpuadvisory");
  if (!box) return;
  clear(box);
  box.classList.add("hidden");
  if (view.coll.id !== "nodes") return;

  // Asked together, and neither is allowed to lose the other: a cell whose
  // CPU report fails still has capacity worth showing, and the other way
  // round.
  const [report, room] = await Promise.all([
    explainCpu().catch(() => null),
    explainCapacity().catch(() => null),
  ]);
  const lines = [];
  if (room) lines.push(capacityLine(room));
  // Before the capacity report is read, not after: "no room for a 32 GiB
  // guest" and "two machines are out until 03:00" are the same sentence, and
  // reading the first without the second sends somebody hunting for a fault.
  for (const line of maintenanceLines(await windowsNow())) lines.unshift(line);

  // A failure on either side is silent by design: the strip is an aid, and an
  // error banner about an aid is worse than the aid being absent. What is
  // *not* silent is a cell with something to say — those lines still render
  // from whichever half answered.
  const domains = (report && report.domains) || [];
  if (domains.length > 1) {
    lines.push(el("div.cpuline",
      el("span.cpukey", domains.length + " migration domains"),
      el("span.cpuval", domains
        .map((d) => (d.nodes || []).join(", ") + " — " + (d.level || "unknown"))
        .join("  ·  "))));
  }

  for (const a of (report && report.advice) || []) lines.push(adviceLine(a));

  const pending = (report && report.pendingAdoption) || [];
  if (pending.length) {
    lines.push(el("div.cpuline",
      el("span.cpukey", pending.length + " awaiting restart"),
      el("span.cpuval",
        pending.map((p) => shortName(p.instance)).join(", ") +
        " still run " + (pending[0].running || "another CPU") +
        " and adopt " + (pending[0].wouldGet || "the current baseline") +
        " when next restarted.")));
  }

  // Nodes nobody has heard from are named, not counted: "2 of 5" with no list
  // reads as a broken report rather than as two quiet machines.
  const unreported = (report && report.unreported) || [];
  if (unreported.length) {
    lines.push(el("div.cpuline",
      el("span.cpukey", "No CPU reported"),
      el("span.cpuval", unreported.join(", ") +
        " — these are in no domain and will not be offered as a destination.")));
  }

  if (!lines.length) return;
  for (const line of lines) box.appendChild(line);
  box.classList.remove("hidden");
}

/// The cell's maintenance windows, as the board needs them.
///
/// Read straight from the collection rather than through a report: whether a
/// window is open is arithmetic on the clock, and a browser has one of those.
/// Failure is silence — the strip is an aid, and an error banner about an aid
/// is worse than the aid being absent.
async function windowsNow() {
  try {
    return await options("maintenance-windows");
  } catch {
    return [];
  }
}

/// "node-b is out for another 40 minutes" and "node-c goes out in 3 hours".
///
/// Both, and in that order. The open one explains what an operator is looking
/// at right now; the next one is the one they would otherwise be surprised by.
function maintenanceLines(windows) {
  const now = Date.now();
  const lines = [];
  const at = (w) => Number(pick(spec(w), "startsAt") || 0);
  const ends = (w) => at(w) + Number(pick(spec(w), "minutes") || 0) * 60_000;
  const of = (w) => String(pick(spec(w), "node") || "");
  const why = (w) => {
    const note = String(pick(spec(w), "note") || "");
    return note ? ": " + note : "";
  };

  const open = windows.filter((w) => at(w) <= now && now < ends(w));
  if (open.length) {
    lines.push(el("div.cpuline",
      el("span.cpukey.warn", open.length === 1 ? "1 node out of service" : open.length + " nodes out of service"),
      el("span.cpuval", open
        .map((w) => of(w) + ", for another " +
          minutesAsWords(Math.max(1, Math.ceil((ends(w) - now) / 60_000))) +
          (pick(spec(w), "drain") ? ", guests moving off" : "") + why(w))
        .join("  ·  "))));
  }

  // Only what is near enough to act on. A window three weeks out on a strip
  // above today's fleet is noise, and noise is what makes people stop reading
  // the line that matters.
  const soon = windows
    .filter((w) => at(w) > now && at(w) - now < 24 * 60 * 60_000)
    .sort((a, b) => at(a) - at(b));
  if (soon.length) {
    lines.push(el("div.cpuline",
      el("span.cpukey", "Scheduled"),
      el("span.cpuval", soon
        .map((w) => of(w) + " in " + minutesAsWords(Math.round((at(w) - now) / 60_000)) +
          ", for " + minutesAsWords(Number(pick(spec(w), "minutes") || 0)) + why(w))
        .join("  ·  "))));
  }
  return lines;
}

/// What the cell has room for, in the one form that does not mislead.
///
/// `largestFit` leads, and `free` follows it in the same sentence. That order
/// is the whole point: free memory does not add up into a guest, and a strip
/// that showed the sum on its own would tell somebody a 32 GiB machine fits a
/// cell that has no room for one anywhere.
function capacityLine(room) {
  const gib = (mib) => Math.round(Number(mib || 0) / 1024);
  const fit = room.largestFit || {};
  const free = room.free || {};
  const nodes = Number(room.usableNodes || 0);
  const cores = Number((room.total || {}).vcpus || 0);
  const offered = Number(room.offeredVcpus || 0);
  const idle = Number(room.unusableNodes || 0);
  return el("div.cpuline",
    el("span.cpukey", "Room"),
    el("span.cpuval",
      "Largest guest that fits anywhere: " + Number(fit.vcpus || 0) + " vCPU, " +
      gib(fit.memoryMib) + " GiB. " +
      gib(free.memoryMib) + " GiB free across " + nodes + " node" + (nodes === 1 ? "" : "s") +
      (idle ? ", and " + idle + " not taking work." : ".") +
      (Number(free.memoryMib || 0) > Number(fit.memoryMib || 0)
        ? " Free memory does not add up into one guest — the first number is the one to plan with."
        : "") +
      // Silicon and promise, in one sentence. Either number alone reads as
      // though the cell had grown a processor, or as though it were smaller
      // than what it is placing on.
      (offered > cores
        ? " " + cores + " cores, offered as " + offered + ": some machines are sharing theirs."
        : "")));
}

/// One recommendation, as a sentence that says what it costs.
function adviceLine(a) {
  const line = (key, text) => el("div.cpuline", el("span.cpukey", key), el("span.cpuval", text));
  switch (a.kind) {
    case "AlreadyUniform": {
      // The machines by name, not a count. `nodes` means the same thing in
      // every one of these — the machines the advice is about — and printing
      // its length where the others print names was the shape of a screen
      // saying "3" when it meant to say which three.
      const all = a.nodes || [];
      return line("One domain",
        all.length + " node" + (all.length === 1 ? "" : "s") +
        (all.length ? " — " + all.join(", ") : "") +
        ", all " + (a.level || "the same") + ". Guests migrate freely.");
    }
    case "BaselineWouldMerge": {
      // The price, per node. A recommendation naming only the benefit arrives
      // wearing the platform's authority.
      const lost = (a.featuresLost || [])
        .map((f) => f.node + " loses " + (f.flags || []).join(", "));
      return line("Could be baselined",
        "Setting " + a.level + " on " + (a.nodes || []).join(", ") +
        " would put them in one domain. " +
        (lost.length ? "Cost: " + lost.join("; ") + "." : "No node loses anything."));
    }
    case "CannotMerge": {
      const r = a.reason || {};
      if (r.kind === "VmmCannotMask") {
        // No product is named here, because the platform was not told one. The
        // fact it has is that these machines cannot present a CPU other than
        // their own — true of Cloud Hypervisor, which masks nothing, and just as
        // true of a QEMU built without the `x86-64-vN` models, which is what
        // Debian 13 ships. Naming the wrong one sent an operator looking for a
        // hypervisor they were already running.
        return line("Cannot be merged",
          (r.nodes || []).join(", ") + " cannot present a CPU other than their own, " +
          "so a live move works only between identical machines. A cold move " +
          "still works: it stops the guest and starts it on the destination, " +
          "where it reads the processor it is given.");
      }
      return line("Cannot be merged",
        "A common baseline would fall below " + (r.level || "what guests need") + ".");
    }
    case "SplitByArch":
      return line("Different architectures",
        (a.groups || []).map((g) => g.arch + ": " + (g.nodes || []).join(", ")).join("  ·  ") +
        " — nothing bridges this.");
    case "NodeOutsideTheAggregate":
      return (a.missing || []).length
        ? line("Outside the aggregate",
            a.node + " presents " + a.presents + " and cannot reach " + a.aggregate +
            " — it lacks " + a.missing.join(", ") + ". It needs an aggregate of its own.")
        : line("Outside the aggregate",
            a.node + " presents " + a.presents + " while " + a.aggregateNodes +
            " nodes present " + a.aggregate + ". It could join: set the baseline on it.");
    case "AdoptionPending":
      // Rendered from `pendingAdoption` above, which carries the names.
      return el("span");
    default:
      return el("span");
  }
}

function setWatchState(state) {
  const box = $("watchstate");
  clear(box);
  box.classList.toggle("live", state === "live");
  if (state === "live") fill(box, mark("settled"), "live");
  else if (state === "unsupported") fill(box, mark("unreported"), "no live updates");
  else fill(box, mark("drifting"), "reconnecting");
}

/// One event from the watch, folded into what is on screen.
/// Whether a resource passes the filter in force, by the same rule the API uses.
///
/// Every term must match; a bare key asks whether the label is there at all.
function admitted(resource) {
  if (!session.labels) return true;
  const labels = (resource.meta && resource.meta.labels) || {};
  return session.labels.split(",").map((t) => t.trim()).filter(Boolean).every((term) => {
    const at = term.indexOf("=");
    if (at < 0) return Object.prototype.hasOwnProperty.call(labels, term);
    return labels[term.slice(0, at).trim()] === term.slice(at + 1).trim();
  });
}

function applyEvent(coll, event) {
  if (!view.coll || view.coll.id !== coll.id) return;
  const name = event.name || (event.resource ? nameOf(event.resource) : null);
  if (!name) return;
  const i = view.items.findIndex((r) => nameOf(r) === name);
  // A filtered board watches everything and keeps what matches.
  //
  // The stream is deliberately *not* narrowed by the server. If it were, an
  // object that had the label when the board was drawn and then lost it would
  // simply stop producing events, and the row would sit there for ever saying
  // something that stopped being true. Judged here, that same change arrives as
  // an event that no longer matches — and the row goes, which is the answer.
  if (event.resource && !admitted(event.resource)) {
    if (i >= 0) {
      view.items.splice(i, 1);
      renderBoard();
      recount();
      if (sheet.open && sheet.name === name) closeSheet();
    }
    return;
  }
  if (event.type === "DELETE") {
    if (i >= 0) view.items.splice(i, 1);
  } else if (event.resource) {
    if (i >= 0) view.items[i] = event.resource; else view.items.push(event.resource);
  }
  forgetOptions(coll.id);
  renderBoard();
  recount();
  // A detail that is open is the thing somebody is watching converge. It must
  // move with the object, not wait for a reload.
  if (sheet.open && sheet.name === name) {
    if (event.type === "DELETE") closeSheet();
    else renderSheet(coll, event.resource);
  }
}

/// Ask again, on a collection whose status is not entirely written down.
///
/// The watch is enough for anything that changes because somebody wrote it: a
/// write is an event. It is not enough for a condition the API computes when
/// the object is read, out of the passing of time — a migration that has run
/// past its timeout becomes failed with nothing written anywhere, so the store
/// has nothing to announce and the watch will never deliver it. Without this,
/// that migration reads as transferring until somebody reloads the page.
///
/// Which is why this is not a polling console with a live update bolted on: it
/// is one collection, named in the schema, for one reason.
function startRecheck(coll) {
  if (!coll.recheck) return;
  view.rechecker = setInterval(async () => {
    if (!view.coll || view.coll.id !== coll.id) return;
    let fresh;
    try { fresh = await list(coll); } catch (e) { return; }   // the watch reports being down
    view.items = fresh.items;
    view.complete = fresh.complete !== false;
    renderBoard();
    recount();
    // The object somebody has open is the one they are watching for exactly
    // this.
    if (sheet.open && sheet.coll && sheet.coll.id === coll.id) {
      const same = view.items.find((r) => nameOf(r) === sheet.name);
      if (same) renderSheet(coll, same);
    }
  }, coll.recheck * 1000);
}

function stopRecheck() {
  if (view.rechecker) { clearInterval(view.rechecker); view.rechecker = null; }
}


// ---- the overview ----------------------------------------------------------

/// The cell at a glance: what needs attention, what the machines look like,
/// and what this project has left.
///
/// It is not a board. Nothing on it is a row of one collection, and that is the
/// point — the questions an operator opens a console with ("is anything wrong",
/// "have I got room", "is a machine out tonight") cross every collection there
/// is, and answering them by visiting eleven boards in turn is the work this
/// page exists to remove.
async function showOverview() {
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  session.labels = "";
  view.coll = null;
  view.home = true;
  view.map = false;
  view.items = [];
  view.picked.clear();
  bulkOutcome = null;
  $("picked").classList.add("hidden");
  location.hash = "#overview";

  $("listtitle").textContent = "Overview";
  fill($("listblurb"), "What needs attention, what the machines look like, and " +
    "what this project has left.");
  clear($("listacts"));
  $("listfilter").classList.add("hidden");
  $("listerr").classList.add("hidden");
  $("cpuadvisory").classList.add("hidden");
  $("listempty").classList.add("hidden");
  closeScreen();
  document.querySelector(".boardwrap").classList.add("hidden");
  $("topologybox").classList.add("hidden");
  $("overviewbox").classList.remove("hidden");

  renderRail();
  renderOverviewBody();
  // The lists are already being swept on the way in; these three are the
  // answers no listing carries.
  renderOverviewReports();
}

/// The half that comes off the sweep: what is not settled, and where.
function renderOverviewBody() {
  const box = $("overviewbox");
  const reports = $("overviewreports");
  clear(box);

  const attention = unsettledEverywhere();
  const total = collections().reduce((n, c) => n + ((census[c.id] || {}).unsettled || 0), 0);
  const unreadable = collections().filter((c) => census[c.id] && !census[c.id].ok);

  const panel = el("div.overpanel");
  panel.appendChild(el("h2", "Attention"));
  if (!attention.length && !unreadable.length) {
    // Said plainly rather than by an empty space. A page that shows nothing
    // when nothing is wrong reads as a page that failed to load.
    panel.appendChild(el("p", el("span.state.settled", mark("settled"), "Everything has settled."),
      el("span.muted", " Nothing in this cell is drifting or failing.")));
  } else {
    for (const { coll, item } of attention) {
      const v = verdict(item, coll.condition);
      panel.appendChild(el("div.overrow",
        el("button.linky", { type: "button", "data-goes": nameOf(item),
          onclick: () => openFrom(coll, item) }, shortName(nameOf(item))),
        el("span.state." + v.kind, mark(v.kind), v.label || v.kind),
        el("span.muted", coll.singular + (v.detail ? " — " + v.detail : ""))));
    }
    if (total > attention.length) {
      panel.appendChild(el("p.muted",
        (total - attention.length) + " more are not settled. " +
        "A cell where everything is drifting has one problem, not four hundred — " +
        "the rail says which collections they are in."));
    }
    for (const c of unreadable) {
      panel.appendChild(el("div.overrow",
        el("span.state.failing", mark("failing"), "unreadable"),
        el("span.muted", c.title + " — " + (census[c.id].why || "the list did not answer"))));
    }
  }
  box.appendChild(panel);
  // Rebuilt above, so the reports panel is re-attached rather than lost.
  box.appendChild(reports || el("div.overpanel", { id: "overviewreports" },
    el("h2", "Cell"), el("p.faint", "Asking…")));
}

/// Open one object's sheet from the overview, board and all.
///
/// It goes to the board first rather than opening a sheet over a page the
/// object is not on: somebody who closes that sheet should be left looking at
/// the thing they were sent to, not back at the overview wondering whether
/// they imagined it.
async function openFrom(coll, item) {
  await show(coll.id);
  openSheet(coll, item);
}

/// The half nothing lists: capacity, processors, maintenance, allowance.
async function renderOverviewReports() {
  const host = $("overviewreports");
  if (!host) return;
  const [room, cpu, windows, allowance] = await Promise.all([
    explainCapacity().catch(() => null),
    explainCpu().catch(() => null),
    windowsNow().catch(() => []),
    session.project ? explainQuota(session.project).catch(() => null) : Promise.resolve(null),
  ]);
  clear(host);

  // The cell's own heading only when there is something of the cell's to say.
  // For a tenant, `explainCapacity` and `explainCpu` are refused — the fleet's
  // machine names and domains are not theirs — and an empty "THE CELL" heading
  // over nothing would be a report that failed to load.
  if (room || (cpu && (cpu.domains || []).length)) {
    host.appendChild(el("h2", "The cell"));
  }
  if (room) host.appendChild(capacityLine(room));
  for (const line of maintenanceLines(windows)) host.appendChild(line);
  const domains = (cpu && cpu.domains) || [];
  if (domains.length > 1) {
    host.appendChild(el("div.cpuline",
      el("span.cpukey", domains.length + " migration domains"),
      el("span.cpuval", domains
        .map((d) => (d.nodes || []).join(", ") + " — " + (d.level || "unknown"))
        .join("  ·  "))));
  }
  for (const a of (cpu && cpu.advice) || []) host.appendChild(adviceLine(a));

  if (allowance) {
    const most = allowance.largestStartable || {};
    const gib = (mib) => Math.round(Number(mib || 0) / 1024);
    const because = { quota: "your quota", cell: "the machines", both: "both" };
    host.appendChild(el("h2", session.project));
    host.appendChild(most.none
      ? el("div.cpuline",
          el("span.cpukey.warn", "Nothing can start"),
          el("span.cpuval", (because[most.vcpusLimitedBy] || "the cell") + " is in the way."))
      : el("div.cpuline",
          el("span.cpukey", "Largest guest"),
          el("span.cpuval", most.vcpus + " vCPU · " + gib(most.memoryMib) + " GiB, limited by " +
            (because[most.vcpusLimitedBy] || "?") + ".")));
    // Only the dimensions that are close to their limit. A tenant with room to
    // spare is told so by the line above; a list of eight numbers they are
    // nowhere near is chrome.
    const tight = (allowance.dimensions || [])
      .filter((d) => !d.unlimited && d.limit > 0 && d.used / d.limit >= 0.8);
    if (tight.length) {
      host.appendChild(el("div.cpuline",
        el("span.cpukey.warn", tight.length === 1 ? "1 limit nearly reached" : tight.length + " limits nearly reached"),
        el("span.cpuval", tight
          .map((d) => d.name + " " + d.used + "/" + d.limit)
          .join("  ·  "))));
    }
  }
}

async function show(id) {
  const coll = collection(id);
  if (!coll) return;
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  // A filter belongs to the board it was typed on. Carrying it across would
  // show somebody a short list of volumes because of something they typed
  // about guests.
  if (view.coll && view.coll.id !== coll.id) session.labels = "";
  view.coll = coll;
  view.home = false;
  view.map = false;
  // A selection belongs to the board it was made on. Carrying it across would
  // arm a Delete over names from another collection entirely — and an answer
  // about guests, read over a list of volumes, is worse than no answer.
  view.picked.clear();
  bulkOutcome = null;
  closeScreen();
  $("overviewbox").classList.add("hidden");
  $("topologybox").classList.add("hidden");
  document.querySelector(".boardwrap").classList.remove("hidden");
  location.hash = "#" + id;
  renderRail();
  renderListHead();
  // Not awaited: the rows are what the operator came for, and a fleet-wide
  // report should never be what they are waiting on.
  renderCpuAdvisory();
  $("listerr").classList.add("hidden");
  try {
    const r = await list(coll, session.labels);
    view.items = r.items;
    view.revision = r.revision;
    view.complete = r.complete !== false;
    if (!view.complete) {
      // The rows are still shown — a truncated fleet plus a sentence is more use
      // to an operator than an empty error page — but the board must not read as
      // the whole collection.
      fill($("listerr"),
        "This list did not finish: the API kept offering more pages. " +
        view.items.length + " shown, and there are more.")
        .classList.remove("hidden");
    }
  } catch (e) {
    view.items = [];
    view.complete = true;
    fill($("listerr"), e.message).classList.remove("hidden");
  }
  renderBoard();
  recount();
  view.watcher = watch(coll, view.revision, (ev) => applyEvent(coll, ev), setWatchState);
  startRecheck(coll);
}
