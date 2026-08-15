// The rail and the list — where am I, where can I go, what is there.

const view = {
  coll: null,          // the collection being shown
  items: [],
  watcher: null,
  revision: null,
  rechecker: null,     // see startRecheck
};

// How many objects each collection holds and how many of them are not settled.
// Swept once after sign-in, so the rail answers "where is the problem" before
// anybody clicks: an operator should not have to open ten screens to find out
// that nothing is wrong.
const census = {};

function groups() {
  const out = [];
  for (const c of collections()) {
    let g = out.find((x) => x.name === c.group);
    if (!g) { g = { name: c.group, items: [] }; out.push(g); }
    g.items.push(c);
  }
  return out;
}

function renderRail() {
  const rail = $("rail");
  clear(rail);
  for (const g of groups()) {
    rail.appendChild(el("div.railgroup", g.name));
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
          : el("span.n", seen ? String(seen.total) : "")));
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
  box.appendChild(el("span.muted", total + " objects"));
  box.appendChild(unsettled
    ? el("span.state.drifting", mark("drifting"), unsettled + " not settled")
    : el("span.state.settled", mark("settled"), "all settled"));
  if (failed) box.appendChild(el("span.state.failing", mark("failing"), failed + " unreadable"));
}

async function sweep() {
  await Promise.all(collections().map(async (c) => {
    try {
      const r = await list(c);
      census[c.id] = {
        ok: true,
        total: r.items.length,
        unsettled: r.items.filter((x) => verdict(x, c.condition).kind !== "settled").length,
      };
    } catch (e) {
      census[c.id] = { ok: false, total: 0, unsettled: 0, why: e.message };
    }
  }));
  renderRail();
  renderFleet();
}

/// Recount one collection from what is on screen, so the rail agrees with the
/// board without a second round trip.
function recount() {
  if (!view.coll) return;
  census[view.coll.id] = {
    ok: true,
    total: view.items.length,
    unsettled: view.items.filter((x) => verdict(x, view.coll.condition).kind !== "settled").length,
  };
  renderRail();
  renderFleet();
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
  fill($("boardhead"),
    el("th", { style: "width:220px" }, coll.singular === "project" ? "Project" : "Name"),
    el("th", { style: "width:150px" }, "Convergence"),
    coll.columns.map((c) => el("th", { style: "width:" + c.width + "px" }, c.label)));

  const body = $("boardbody");
  clear(body);
  const rows = view.items.slice().sort((a, b) => nameOf(a).localeCompare(nameOf(b)));
  for (const r of rows) {
    const tr = el("tr", { tabindex: "0", "data-name": nameOf(r) });
    const open = () => openSheet(coll, r);
    tr.addEventListener("click", open);
    tr.addEventListener("keydown", (e) => { if (e.key === "Enter") open(); });
    tr.appendChild(el("td.name", el("span.id", { title: nameOf(r) }, idOf(r))));
    tr.appendChild(el("td", stateOf(r, coll.condition)));
    for (const c of coll.columns) tr.appendChild(cell(r, c));
    body.appendChild(tr);
  }

  const empty = $("listempty");
  empty.classList.toggle("hidden", rows.length > 0);
  empty.textContent = rows.length ? "" :
    "No " + coll.title.toLowerCase() + " here yet." +
    (coll.creatable ? " Create the first one above." : "");
  $("board").classList.toggle("hidden", rows.length === 0);
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
function applyEvent(coll, event) {
  if (!view.coll || view.coll.id !== coll.id) return;
  const name = event.name || (event.resource ? nameOf(event.resource) : null);
  if (!name) return;
  const i = view.items.findIndex((r) => nameOf(r) === name);
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

async function show(id) {
  const coll = collection(id);
  if (!coll) return;
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  view.coll = coll;
  location.hash = "#" + id;
  renderRail();
  renderListHead();
  $("listerr").classList.add("hidden");
  try {
    const r = await list(coll);
    view.items = r.items;
    view.revision = r.revision;
  } catch (e) {
    view.items = [];
    fill($("listerr"), e.message).classList.remove("hidden");
  }
  renderBoard();
  recount();
  view.watcher = watch(coll, view.revision, (ev) => applyEvent(coll, ev), setWatchState);
  startRecheck(coll);
}
