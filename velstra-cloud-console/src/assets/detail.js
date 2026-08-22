// One object, in full: what was asked for, what is, and whether they agree.
//
// The three are never shown apart. A status value alone cannot be judged —
// `Stopped` is a fault or exactly right depending on the spec beside it — and a
// spec alone is a wish. So the sheet leads with the verdict, then puts the two
// halves in the same table, and only then lists the object's own detail.

const sheet = { open: false, name: null, coll: null, timer: null };

/// A sheet that asks something again on a timer owns that timer, so it stops
/// when the sheet does and there is never a second one running behind it.
function sheetTimer(every, tick) {
  if (sheet.timer) { clearInterval(sheet.timer); sheet.timer = null; }
  if (every) sheet.timer = setInterval(tick, every * 1000);
}

function closeSheet() {
  sheet.open = false; sheet.name = null;
  sheetTimer(0, null);
  const s = $("sheet"), sc = $("scrim");
  if (s) s.remove();
  if (sc) sc.remove();
}

function openSheet(coll, r) {
  closeSheet();
  const scrim = el("div", { id: "scrim", onclick: closeSheet });
  const panel = el("aside", { id: "sheet", role: "dialog", "aria-label": nameOf(r) });
  document.body.appendChild(scrim);
  document.body.appendChild(panel);
  sheet.open = true; sheet.coll = coll; sheet.name = nameOf(r);
  renderSheet(coll, r);
}

/// A second reading where the unit is one people mis-key. `65536` and `64 GiB`
/// are the same number and only one of them is read correctly at a glance.
function alsoIn(key, value) {
  if (typeof value !== "number") return null;
  const larger = /mib$/i.test(key) ? mibAlso(value) : /bytes$/i.test(key) ? bytes(value) : "";
  return larger ? el("span.faint", " (" + larger + ")") : null;
}

/// A resource name is not a string, it is somewhere to go. An attachment names
/// its volume, an operation names its target, a port names its subnet — and
/// following one by hand means reading it, choosing the right collection in the
/// rail, and finding the row.
function nameLink(value) {
  const s = String(value);
  const segs = s.split("/");
  if (segs.length < 2 || segs.length % 2 !== 0) return null;
  const coll = collection(segs[segs.length - 2]);
  return coll ? el("button.link.mono", { type: "button", title: s, onclick: () => goTo(s) }, shortName(s)) : null;
}

/// A reference on a form field is followable even when it arrives as a bare id,
/// because the schema says which collection it points at. That is not a guess:
/// `nameLink` cannot resolve `node-a` on its own and correctly refuses to try.
function refValue(f, v) {
  if (v === null || v === undefined || v === "") return el("span.blank.faint", "—");
  const coll = collection(f.collection);
  const raw = String(v);
  const name = raw.includes("/") || !coll
    ? raw
    : coll.scope === "global"
      ? coll.id + "/" + raw
      : "projects/" + session.project + "/" + coll.id + "/" + raw;
  return coll
    ? el("button.link.mono", { type: "button", title: name, onclick: () => goTo(name) }, shortName(name))
    : el("span.mono", { title: raw }, shortName(raw));
}

async function goTo(name) {
  const segs = name.split("/");
  const coll = collection(segs[segs.length - 2]);
  if (!coll) return;
  // A name carries its project. Following one out of the project on screen
  // changes the project rather than showing an empty board.
  const project = segs[0] === "projects" ? segs[1] : null;
  if (project && project !== session.project) {
    session.project = project;
    sessionStorage.setItem(PROJECT_KEY, project);
    $("project").value = project;
    forgetOptions();
  }
  closeSheet();
  await show(coll.id);
  const here = view.items.find((r) => nameOf(r) === name);
  const found = here || await get(coll, segs[segs.length - 1]).catch(() => null);
  if (found) openSheet(coll, found);
  else toast("There is no " + coll.singular + " called " + shortName(name) + " any more.", "bad");
}

/// Free text is free, but a value that is a resource name or a digest is read
/// down a column of others like it, so it gets the mono face.
function valueNode(v, kindHint) {
  if (v === null || v === undefined || v === "") return el("span.blank.faint", "—");
  if (typeof v === "boolean") return el("span", v ? "yes" : "no");
  if (Array.isArray(v)) {
    if (!v.length) return el("span.blank.faint", "none");
    return el("div", v.map((x) =>
      el("div", nameLink(x) || el("span.mono", { title: String(x) }, shortName(x)))));
  }
  if (typeof v === "object") {
    return el("div", Object.entries(v).map(([k, sub]) =>
      el("div", el("span.faint", label(k) + " "), valueNode(sub), alsoIn(k, sub))));
  }
  const s = String(v);
  const link = nameLink(s);
  if (link) return link;
  const machine = kindHint === "mono" || s.includes("/") || s.includes(":") || /^[0-9a-f]{16,}$/.test(s);
  return el(machine ? "span.mono" : "span", { title: s }, machine ? shortName(s) : s);
}

function verdictBlock(coll, r) {
  const v = verdict(r, coll.condition);
  const box = el("div.verdict." + v.kind,
    el("div.head", mark(v.kind), v.word),
    el("div.why.muted", v.why));

  const gen = generation(r), obs = observed(r);
  box.appendChild(el("div.gens",
    el("div", el("span.k", "Asked at"), String(gen)),
    el("div" + (obs < gen ? ".behind" : ""), el("span.k", "Observed at"), obs ? String(obs) : "—"),
    v.since ? el("div", el("span.k", "Since"), el("span", { title: stamp(v.since) }, ago(v.since))) : null));

  // The reason and the sentence, on the object rather than in a log file on
  // whichever machine happened to run the controller. When the verdict already
  // *is* the agent's sentence, only the machine token is added — a page that
  // says the same thing twice is a page that gets skimmed.
  if (v.ready && v.ready.reason && v.kind !== "settled") {
    const said = v.ready.message && v.ready.message !== v.why;
    box.appendChild(el("div.why",
      el("span.mono", v.ready.reason),
      said ? el("span.muted", " — " + v.ready.message) : null));
  }
  return box;
}

function agreementTable(coll, r) {
  if (!coll.agreements.length) return null;
  const table = el("table.pairs",
    el("thead", el("tr",
      el("th", { style: "width:140px" }, ""),
      el("th", "Asked for"),
      el("th", "Is"),
      el("th", { style: "width:120px" }, ""))));
  const body = el("tbody");
  let any = false;
  for (const a of coll.agreements) {
    const asked = at(spec(r), a.asked);
    const is = at(status(r), a.is);
    const empty = (x) => x === null || x === undefined || x === "";
    if (empty(asked) && empty(is)) continue;
    any = true;
    const differs = String(asked ?? "") !== String(is ?? "");
    const row = el("tr" + (differs ? ".differs" : ""),
      el("td.muted", a.label),
      el("td", valueNode(asked)),
      el("td", valueNode(is)),
      el("td", differs
        ? el("span.state.drifting", mark("drifting"), "differs")
        : el("span.state.settled", mark("settled"), "agrees")));
    body.appendChild(row);
    if (differs) {
      body.appendChild(el("tr", el("td", { colspan: "4" }, el("div.note", a.note))));
    }
  }
  table.appendChild(body);
  return any ? table : null;
}

function fieldValue(r, f) {
  const v = at(spec(r), f.key);
  switch (f.kind) {
    case "number": {
      if (v === null || v === undefined) return valueNode(v);
      const also = f.scale === "mib" ? mibAlso(v) : f.scale === "bytes" ? bytes(v) : "";
      return el("span", el("span.num", Number(v).toLocaleString()),
        f.unit ? el("span.faint", " " + f.unit) : null,
        also ? el("span.faint", "  (" + also + ")") : null);
    }
    case "switch":
      return el("span", v ? "yes" : "no");
    case "choice": {
      const opt = (f.options || []).find((o) => o.value === v);
      return el("span", opt ? opt.label : (v === undefined ? "—" : String(v)));
    }
    case "ref":
      return refValue(f, v);
    case "refList":
      return !v || !v.length
        ? el("span.blank.faint", "none")
        : el("div", v.map((x) => el("div", refValue(f, x))));
    default:
      return valueNode(v);
  }
}

function specTable(coll, r) {
  const table = el("table.kv");
  const body = el("tbody");
  const shown = new Set();
  for (const f of coll.fields) {
    shown.add(f.key.split(".")[0]);
    body.appendChild(el("tr",
      el("td", f.label, f.derived ? el("span.faint", " · set by the platform") : null),
      el("td", fieldValue(r, f))));
  }
  // Anything the API sends that this console was never told about is still
  // shown. A field a new release adds is visible the day it ships, rather than
  // silently dropped until somebody notices it is missing.
  for (const [k, v] of Object.entries(spec(r))) {
    if (shown.has(k) || shown.has(k.replace(/_([a-z])/g, (m, c) => c.toUpperCase()))) continue;
    body.appendChild(el("tr", el("td", label(k)), el("td", valueNode(v))));
  }
  table.appendChild(body);
  return table;
}

function statusTable(r) {
  const table = el("table.kv");
  const body = el("tbody");
  for (const [k, v] of Object.entries(status(r))) {
    if (k === "conditions" || k === "observedGeneration" || k === "observed_generation") continue;
    const isTime = /(at|heartbeat|transition)$/i.test(k) && typeof v === "number" && v > 1e12;
    body.appendChild(el("tr",
      el("td", label(k)),
      el("td", isTime ? el("span", { title: stamp(v) }, ago(v)) : valueNode(v), isTime ? null : alsoIn(k, v))));
  }
  if (!body.childElementCount) {
    body.appendChild(el("tr", el("td", { colspan: "2" },
      el("span.faint", "Nothing has been reported about this object yet."))));
  }
  table.appendChild(body);
  return table;
}

/// Conditions the API computes on every read instead of storing them. See
/// `docs/rest-contract.md`, "Computed fields".
const COMPUTED_CONDITIONS = new Set(["Moved"]);

/// When a condition last changed — rendered only where that is a real moment.
///
/// A computed condition is built fresh on every read, so its `lastTransition` is
/// the moment of *this request*. Showing that as an age would put "just now" on
/// a transfer that stalled an hour ago, which is worse than showing nothing: an
/// operator reads a fresh timestamp as movement. The one case the API can anchor
/// is a timeout — its moment really is knowable, `createdAt + timeoutS` — and
/// that one is worth the minute it is accurate to.
///
/// The message is where the information is either way, and it is always shown.
function conditionAge(c) {
  const at = pick(c, "lastTransition");
  if (!at) return null;
  if (COMPUTED_CONDITIONS.has(c.kind) && c.reason !== "Timeout") return null;
  return el("div.faint", { title: stamp(at) }, ago(at));
}

function conditionsTable(r) {
  const cs = pick(status(r), "conditions") || [];
  if (!cs.length) return el("p.faint", "No conditions have been written yet.");
  const table = el("table.conds",
    el("thead", el("tr",
      el("th", { style: "width:150px" }, "Condition"),
      el("th", { style: "width:80px" }, ""),
      el("th", { style: "width:150px" }, "Reason"),
      el("th", "Message"))));
  const body = el("tbody");
  for (const c of cs) {
    const kind = c.status === "True" ? "settled" : c.status === "False" ? "failing" : "unreported";
    const stale = conditionStale(r, c);
    body.appendChild(el("tr",
      el("td", c.kind,
        stale ? el("div.stale", "recorded at generation " + (pick(c, "observedGeneration") || 0)) : null),
      el("td", el("span.state." + kind, mark(kind), c.status)),
      el("td.mono", c.reason || "—"),
      el("td.msg", c.message || "—", conditionAge(c))));
  }
  table.appendChild(body);
  return table;
}

function metaTable(r) {
  const m = meta(r);
  const p = pick(m, "placement") || {};
  const labels = pick(m, "labels") || {};
  const finalizers = pick(m, "finalizers") || [];
  const rows = [
    ["Name", el("span.mono", nameOf(r))],
    ["UID", el("span.mono", String(pick(m, "uid") || "—"))],
    ["Placement", el("span.mono", (p.region || "?") + " · " + (p.cell || "?"))],
    ["Generation", el("span.num", String(generation(r)))],
    ["Revision", el("span.mono", revision(r) === null ? "—" : revision(r))],
    ["Created", el("span", { title: stamp(pick(m, "createdAt")) }, ago(pick(m, "createdAt")))],
  ];
  if (deletedAt(r)) rows.push(["Deletion asked",
    el("span", { title: stamp(deletedAt(r)) }, ago(deletedAt(r)))]);
  if (finalizers.length) rows.push(["Held by", valueNode(finalizers)]);
  if (Object.keys(labels).length) rows.push(["Labels", valueNode(labels)]);
  const body = el("tbody", rows.map(([k, v]) => el("tr", el("td", k), el("td", v))));
  return el("table.kv", body);
}

/// Why a thing was not placed, as the answer rather than as a spinner.
async function explainInto(host, coll, r) {
  fill(host, el("p.faint", "Asking the scheduler…"));
  try {
    const answer = await explainPlacement(coll, idOf(r));
    const rejected = answer.rejected || [];
    const placed = answer.placed;
    const parts = [];
    parts.push(placed
      ? el("p", el("span.state.settled", mark("settled"), "Placed on "), el("span.mono", String(placed)))
      : el("p", el("span.state.failing", mark("failing"), "Not placed"),
          el("span.muted", rejected.length
            ? " — every node was rejected, in order:"
            : " — the scheduler named no candidates at all.")));
    if (rejected.length) {
      parts.push(el("table.reject",
        el("thead", el("tr",
          el("th", { style: "width:160px" }, "Node"),
          el("th", { style: "width:180px" }, "Rejected because"),
          el("th", "Detail"))),
        el("tbody", rejected.map((x) => el("tr",
          el("td.mono", String(x.node ?? "—")),
          el("td.mono", String(x.why ?? "—")),
          el("td.muted", String(x.detail ?? "")))))));
    }
    fill(host, parts);
  } catch (e) {
    fill(host, el("p.err", e.status === 404
      ? "This API does not answer :explainPlacement for " + coll.singular + "s."
      : e.message));
  }
}

/// `opts.verb` renames the action where "delete" is the wrong word for it, and
/// `opts.warning` is what the operator is actually deciding — used where that
/// differs from object to object, which is exactly one place: abandoning a
/// migration means something different under every mode.
function deleteControl(coll, r, opts = {}) {
  const verb = opts.verb || "Delete";
  const host = el("span.confirm");
  const ask = () => {
    // Deleting a guest is destructive and cannot be undone, so it asks — once,
    // in place, naming what it is about to delete. Everything else on this page
    // is done without a confirmation, which is what keeps this one meaningful.
    fill(host,
      opts.warning
        ? el("p" + (opts.grave ? ".err" : ".muted"), { id: "deletewarning" }, opts.warning)
        : el("span.muted", verb + " " + idOf(r) + "? "),
      el("span.btns",
        el("button.btn.quiet", { type: "button", id: "confirmdelete", onclick: go }, verb),
        el("button.btn", { type: "button", onclick: rest }, "Keep")));
  };
  const rest = () => fill(host,
    el("button.btn.quiet", { type: "button", id: "deletebtn", onclick: ask }, verb));
  const go = async () => {
    try {
      await remove(coll, idOf(r), revision(r));
      toast(opts.done || "Deletion asked for. It stays visible until its finalizers let go.");
      forgetOptions(coll.id);
      show(coll.id);
      closeSheet();
    } catch (e) { toast(e.message, "bad"); rest(); }
  };
  rest();
  return host;
}

function renderSheet(coll, r) {
  const panel = $("sheet");
  if (!panel) return;
  sheet.name = nameOf(r);
  clear(panel);

  panel.appendChild(el("div", { id: "sheethead" },
    el("div.grow",
      el("h2", idOf(r)),
      el("p.faint.mono", { title: nameOf(r) }, coll.singular + " · " + nameOf(r))),
    el("button.btn", { type: "button", id: "closesheet", onclick: closeSheet }, "Close")));

  const acts = el("div.sheetacts");
  if (coll.editable) {
    acts.appendChild(el("button.btn.primary", { type: "button", id: "editbtn",
      onclick: () => openEdit(coll, r) }, "Edit"));
  }
  if (coll.explainable) {
    acts.appendChild(el("button.btn", { type: "button", id: "explainbtn",
      onclick: () => explainInto($("explain"), coll, r) }, "Explain placement"));
  }
  // A password is not a field on this sheet and cannot be: the platform stores
  // a hash and cannot show one. Setting it is therefore an *action*, next to the
  // others, rather than a control that would have to render a value it has no
  // way to read.
  if (coll.id === "users") {
    acts.appendChild(el("button.btn", { type: "button", id: "setpasswordbtn",
      onclick: () => openPasswordDialog(idOf(r)) }, "Set password"));
  }
  // Abandoning a migration is not deleting a row: what it costs depends on the
  // mode, and the sentence is different enough that it is written where the
  // modes are.
  if (coll.deletable) {
    acts.appendChild(coll.id === "migrations"
      ? deleteControl(coll, r, abandonAsk(r))
      : deleteControl(coll, r));
  }
  panel.appendChild(acts);

  panel.appendChild(el("div", { style: "height:var(--space-6)" }));

  // What is happening to this guest, above everything else: an operator who
  // opened a migration opened it to watch one thing.
  if (coll.id === "migrations") {
    panel.appendChild(spread("Movement", movementBlock(r), "where the guest is in the move"));
  }

  panel.appendChild(spread("Convergence", verdictBlock(coll, r)));

  // Where this guest is, and where it is going. Rendered from the migrations
  // that exist rather than from anything on the instance, because there is no
  // field on an instance that says "moving" — and there must not be one.
  if (coll.id === "instances") {
    const host = el("div", { id: "instancemigration" });
    panel.appendChild(spread("Node", host, "where the guest runs"));
    migrationInto(host, r);
    // The migrations behind this section are not the collection being watched,
    // and one of them can turn into a failure with nothing written — a timeout
    // is decided by the clock, so there is no event for it, on this board or
    // any other. Asked again for as long as this sheet is open.
    const migrations = collection("migrations");
    sheetTimer(migrations ? migrations.recheck : 0, () => {
      if (sheet.open && sheet.name === nameOf(r)) migrationInto(host, r);
    });
  }

  const pairs = agreementTable(coll, r);
  if (pairs) panel.appendChild(spread("Asked vs is", pairs, "the two halves side by side"));

  panel.appendChild(spread("Specification", specTable(coll, r), "what was asked for"));
  panel.appendChild(spread("Observation", statusTable(r), "what the owner reports"));
  panel.appendChild(spread("Conditions", conditionsTable(r)));

  if (coll.explainable) {
    const host = el("div", { id: "explain" });
    panel.appendChild(spread("Placement", host));
    // An object that is not settled is the one somebody is looking at because
    // it went wrong, so the answer is fetched rather than offered behind a
    // button they have to find.
    if (verdict(r, coll.condition).kind !== "settled") explainInto(host, coll, r);
    else fill(host, el("p.faint", "Placed. Ask for the chain if you want to see what was rejected."));
  }

  panel.appendChild(spread("Object", metaTable(r)));
}
