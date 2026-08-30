// One object, in full: what was asked for, what is, and whether they agree.
//
// The three are never shown apart. A status value alone cannot be judged —
// `Stopped` is a fault or exactly right depending on the spec beside it — and a
// spec alone is a wish. So the sheet leads with the verdict, then puts the two
// halves in the same table, and only then lists the object's own detail.

const sheet = { open: false, name: null, coll: null, timer: null, closers: [] };

/// Something to undo when the sheet closes.
///
/// A timer is not the only thing a sheet can leave running: a console holds a
/// socket, and a socket left open holds a guest's serial line against a session
/// that cannot be reused. Registered rather than remembered by name, because the
/// sheet does not need to know what it is closing.
function onSheetClose(undo) {
  sheet.closers.push(undo);
}

/// A sheet that asks something again on a timer owns that timer, so it stops
/// when the sheet does and there is never a second one running behind it.
function sheetTimer(every, tick) {
  if (sheet.timer) { clearInterval(sheet.timer); sheet.timer = null; }
  if (every) sheet.timer = setInterval(tick, every * 1000);
}

function closeSheet() {
  sheet.open = false; sheet.name = null;
  sheetTimer(0, null);
  for (const undo of sheet.closers) {
    try { undo(); } catch (e) { /* one that throws must not strand the others */ }
  }
  sheet.closers = [];
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
  return coll
    ? nameTheImage(s, el("button.link.mono", { type: "button", title: s, onclick: () => goTo(s) }, shortName(s)))
    : null;
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
    ? nameTheImage(name, el("button.link.mono", { type: "button", title: name, onclick: () => goTo(name) }, shortName(name)))
    : el("span.mono", { title: raw }, shortName(raw));
}

/// Put a readable name on a link to an image, once the object is to hand.
///
/// A reference is rendered from the name alone, and an image's name is its
/// digest — so every link to one read `images/sha256-cbf3e1f5…`, on the guest's
/// own screen, where the one thing somebody wants to know is which operating
/// system it runs. The lookup is asynchronous and the link is already on screen,
/// so the text is replaced when the answer arrives; the digest stays as the
/// hover title, because that is what identifies the bytes.
function nameTheImage(name, node) {
  const segs = String(name).split("/");
  if (segs[segs.length - 2] !== "images") return node;
  const coll = collection("images");
  if (!coll) return node;
  listBoth(coll)
    .then((r) => {
      const found = (r.items || []).find((o) => nameOf(o) === name);
      if (found) node.textContent = imageTitle(found);
    })
    .catch(() => {});
  return node;
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
    // An entry is usually a resource name, but the bespoke list controls
    // (listeners, security-group rules, ceph disks and pools) hold objects.
    // Stringifying one of those printed "[object Object]" — the configuration
    // the sheet exists to show, hidden behind a JavaScript default.
    return el("div", v.map((x) =>
      el("div", x !== null && typeof x === "object"
        ? valueNode(x)
        : nameLink(x) || el("span.mono", { title: String(x) }, shortName(x)))));
  }
  if (typeof v === "object") {
    return el("div", Object.entries(v).map(([k, sub]) =>
      el("div", el("span.faint", label(k) + " "), valueNode(sub), alsoIn(k, sub))));
  }
  const s = String(v);
  const link = nameLink(s);
  if (link) return link;
  // Anything with a line break in it is a document, not a value: console
  // output, cloud-init, a public key blob. Rendered as one truncated line with
  // the rest in a tooltip — which is what every other string gets — it is
  // unreadable exactly when somebody needs to read it.
  if (s.includes("\n")) {
    return el("pre.logblock", { title: "" }, s);
  }
  const machine = kindHint === "mono" || s.includes("/") || s.includes(":") || /^[0-9a-f]{16,}$/.test(s);
  return el(machine ? "span.mono" : "span", { title: s }, machine ? shortName(s) : s);
}

/// What has been asked for that this guest will only get when it next starts.
///
/// Computed from the two numbers already on the object — the spec's and the
/// running machine's — rather than from a flag somebody sets. A flag can be
/// stale; a comparison cannot.
///
/// This exists because the platform used to accept a resize of a running guest,
/// do nothing, and show the object as settled. A badge is not the point: the
/// two numbers are, because "pending" on its own is a thing people dismiss.
function pendingChanges(r) {
  // Read, not recomputed. This used to do the comparison itself, which made
  // three copies of one rule — the model's `pending_changes`, this, and
  // nothing in between — and the API was the one that did not have it, so the
  // board could not show what the sheet knew. The API answers it now, on every
  // read, and this renders the answer.
  const answered = at(status(r), "pendingChanges");
  if (!Array.isArray(answered)) return [];
  const labels = { vcpus: "vCPU", memoryMib: "Memory", rootDiskGib: "Root disk" };
  return answered.map((c) => ({
    label: labels[pick(c, "field")] || pick(c, "field"),
    from: pick(c, "from"),
    to: pick(c, "to"),
  }));
}

/// A node's PCI devices, each with what it drags along.
function passableBlock(r) {
  const devices = at(status(r), "pciDevices");
  if (!Array.isArray(devices) || !devices.length) return null;
  return el("div.pending",
    el("div.why.muted",
      "Passing one of these to a guest takes everything on its line: a device " +
      "shares an isolation group with its neighbours and the hardware cannot " +
      "separate them."),
    el("div", devices.map((d) => {
      const address = pick(d, "address");
      const withIt = at(d, "groupWith");
      const others = Array.isArray(withIt) ? withIt.filter((a) => a !== address) : [];
      const group = pick(d, "iommuGroup");
      return el("div.cpuline",
        el("span.cpukey", pick(d, "description") || address),
        el("span.cpuval.mono",
          group === undefined || group === null
            ? address + " — no isolation group, cannot be passed through"
            : others.length
              ? address + " + " + others.join(", ")
              : address + " — on its own"));
    })));
}

function pendingBlock(r) {
  const changes = pendingChanges(r);
  if (!changes.length) return null;
  return el("div.pending",
    el("div.why.muted",
      "The guest is running with these. It gets what was asked for when it next starts — " +
      "nothing here changes a machine that is already up."),
    el("div", changes.map((c) => el("div.cpuline",
      el("span.cpukey", c.label),
      el("span.cpuval.mono", c.from + " \u2192 " + c.to)))));
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

// The read view folds the way the create form does. The form splits its fields
// into a common path and an advanced level behind "More settings (n)"; this
// sheet honoured neither and laid every field flat, so a projects sheet was
// seven rows where the form asked one question and hid six. It folds them the
// same way now — with one honesty rule the form does not need but a read view
// does: a field the object actually set is a value the operator can see on the
// object, and a console that hid it here while showing it in the form would be
// lying about what is set. "Set" is the spec carrying the key; only advanced
// fields the object left unset fold away.
function carriesKey(r, f) {
  const v = at(spec(r), f.key);
  return v !== undefined && v !== null;
}

function specTable(coll, r) {
  const table = el("table.kv");
  const body = el("tbody");
  const deep = el("tbody");   // unset advanced fields, revealed on demand
  const shown = new Set();
  const row = (f) => el("tr",
    el("td", f.label, f.derived ? el("span.faint", " · set by the platform") : null),
    el("td", fieldValue(r, f)));
  for (const f of coll.fields) {
    shown.add(f.key.split(".")[0]);
    (f.advanced && !carriesKey(r, f) ? deep : body).appendChild(row(f));
  }
  // Anything the API sends that this console was never told about is still
  // shown. A field a new release adds is visible the day it ships, rather than
  // silently dropped until somebody notices it is missing.
  for (const [k, v] of Object.entries(spec(r))) {
    if (shown.has(k) || shown.has(k.replace(/_([a-z])/g, (m, c) => c.toUpperCase()))) continue;
    body.appendChild(el("tr", el("td", label(k)), el("td", valueNode(v))));
  }
  table.appendChild(body);
  if (!deep.childElementCount) return table;
  // The unset advanced fields sit one level deeper, behind the same disclosure
  // the form uses — same wording, same aria wiring, its own inset surface — so
  // the two views open the deeper level identically.
  const n = deep.childElementCount;
  const deeper = el("div.deeper.hidden", { id: "specmorefields" }, el("table.kv", deep));
  const toggle = el("button.disclose", { type: "button", id: "specmore",
    "aria-expanded": "false", "aria-controls": "specmorefields" },
    "More settings (" + n + ")");
  toggle.addEventListener("click", () => {
    const open = !deeper.classList.toggle("hidden");
    toggle.setAttribute("aria-expanded", String(open));
    toggle.classList.toggle("open", open);
    toggle.textContent = open ? "Fewer settings" : "More settings (" + n + ")";
  });
  return el("div.specfold", table, toggle, deeper);
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

/// Everything that has happened to one object, newest first.
///
/// Two sources, deliberately in one list: the changes that were **accepted**
/// (operations) and the ones that were **refused** (audit). Reading only the
/// first is how somebody concludes their click did nothing — the refusal is
/// the answer, and it lives in a collection they would otherwise never open.
async function historyInto(host, name) {
  fill(host, el("p.faint", "Asking…"));
  try {
    const { operations, refusals } = await historyOf(name);
    const lines = [];
    for (const o of operations) {
      const s = status(o);
      const at = pick(s, "finishedAt") || pick(meta(o), "createdAt");
      lines.push({
        at: Number(at || 0),
        kind: pick(s, "error") ? "failing" : pick(s, "done") ? "settled" : "drifting",
        what: String(pick(spec(o), "verb") || "change"),
        who: String(pick(spec(o), "requestedBy") || "—"),
        detail: String(pick(s, "error") || (pick(s, "done") ? "" : "still running")),
      });
    }
    for (const a of refusals) {
      lines.push({
        at: Number(pick(meta(a), "createdAt") || 0),
        kind: "failing",
        what: String(pick(spec(a), "verb") || "?") + " refused",
        who: String(pick(spec(a), "subject") || "—"),
        // `detail`, which is the field an audit record actually has — and it
        // holds the *same sentence* the person was given, not a paraphrase of
        // it. Spelled `reason` here, every refusal in this panel was blank.
        detail: String(pick(spec(a), "detail") || ""),
      });
    }
    if (!lines.length) {
      // Said, not left blank: an empty panel reads as one that failed to load.
      fill(host, el("p.faint", "Nothing has been asked of this object yet."));
      return;
    }
    lines.sort((a, b) => b.at - a.at);
    fill(host, el("table.kv", el("tbody", lines.map((l) => el("tr",
      el("td", { title: stamp(l.at) }, ago(l.at)),
      el("td",
        el("span.state." + l.kind, mark(l.kind), l.what),
        el("span.muted", " by " + l.who + (l.detail ? " — " + l.detail : ""))))))));
  } catch (e) {
    fill(host, el("p.err", e.message));
  }
}

/// What a project has left, and what it could actually start with it.
///
/// Both halves, and which of the two is in the way. Quota alone is what a
/// tenant reads before creating a guest that will never be placed; "no valid
/// host" is what they get afterwards, several minutes and one support ticket
/// later.
async function allowanceInto(host, project) {
  fill(host, el("p.faint", "Asking…"));
  try {
    const answer = await explainQuota(project);
    const most = answer.largestStartable || {};
    const gib = (mib) => Math.round(Number(mib || 0) / 1024);
    const because = { quota: "your quota", cell: "the machines", both: "both" };
    const parts = [];

    parts.push(most.none
      ? el("p", el("span.state.failing", mark("failing"), "Nothing can start right now"),
          el("span.muted", " — " + (because[most.vcpusLimitedBy] || "the cell") + " is in the way."))
      : el("p", el("span.state.settled", mark("settled"), "Largest guest that would start: "),
          el("span.mono", most.vcpus + " vCPU · " + gib(most.memoryMib) + " GiB"),
          el("span.muted", " — limited by " +
            (because[most.vcpusLimitedBy] || "?") +
            (most.vcpusLimitedBy === most.memoryLimitedBy
              ? ""
              : " and " + (because[most.memoryLimitedBy] || "?")) + ".")));

    parts.push(el("table.kv", el("tbody", (answer.dimensions || []).map((d) => el("tr",
      el("td", d.name),
      el("td",
        // An unset limit is not a limit of nothing, and must not render as
        // one: a project created without a quota would otherwise read as a
        // project that may not start a single guest.
        d.unlimited
          ? el("span.muted", String(d.used) + " used · no limit")
          : el("span" + (d.exhausted ? ".err" : ""),
              String(d.used) + " of " + String(d.limit) +
              " · " + String(d.left) + " left")))))));
    fill(host, parts);
  } catch (e) {
    fill(host, el("p.err", e.status === 404
      ? "This API does not answer :explainQuota."
      : e.message));
  }
}

/// What is scheduled for this machine, and which guests cannot leave it.
///
/// `cannotMove` is the half that decides whether tonight goes well: a guest
/// that cannot move is stopped when the machine is, and finding that out while
/// the machine is on a trolley is finding it out too late.
async function maintenanceInto(host, r) {
  fill(host, el("p.faint", "Asking…"));
  try {
    const answer = await explainMaintenance(idOf(r));
    const parts = [];
    const when = (w) => {
      const mins = Math.max(1, Math.ceil((Number(w.endsAt || 0) - Date.now()) / 60_000));
      return w.opensInMinutes === null || w.opensInMinutes === undefined
        ? "for another " + minutesAsWords(mins)
        : "in " + minutesAsWords(Number(w.opensInMinutes));
    };
    if (answer.open) {
      parts.push(el("p",
        el("span.state.failing", mark("failing"), "Out of service "),
        el("span", when(answer.open) +
          (answer.open.note ? " — " + answer.open.note : ""))));
    } else if (answer.next) {
      parts.push(el("p",
        el("span.state.waiting", mark("waiting"), "Scheduled "),
        el("span", when(answer.next) + ", for " +
          minutesAsWords(Number(answer.next.minutes || 0)) +
          (answer.next.note ? " — " + answer.next.note : ""))));
    } else {
      parts.push(el("p.faint", "Nothing is scheduled for this machine."));
    }

    const going = answer.willMove || [];
    const stuck = answer.cannotMove || [];
    if (going.length) {
      parts.push(el("p.muted", going.length + " will move: " +
        going.map((g) => shortName(g.instance) + " → " + g.to).join(", ")));
    }
    if (stuck.length) {
      // Named one per line with every node's verdict, not counted: the remedy
      // for "a generation too old" and the remedy for "it holds a GPU" are
      // nothing like each other.
      parts.push(el("p.err", stuck.length === 1
        ? "1 guest cannot move, and will be stopped when the machine is:"
        : stuck.length + " guests cannot move, and will be stopped when the machine is:"));
      parts.push(el("table.reject",
        el("thead", el("tr",
          el("th", { style: "width:220px" }, "Guest"),
          el("th", "Why not"))),
        el("tbody", stuck.map((g) => el("tr",
          el("td.mono", shortName(g.instance)),
          el("td.muted", (g.why || [])
            .map((v) => v.node + ": " + v.detail).join("  ·  ")))))));
    }
    fill(host, parts);
  } catch (e) {
    fill(host, el("p.err", e.status === 404
      ? "This API does not answer :explainMaintenance."
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

/// Ask before minting: a token is a secret, and a button that hands one out on
/// a mis-click is a secret in a screenshot.
///
/// It is asked *and* explained, because the thing an operator most needs to know
/// is what it does not do: the old token keeps working. Somebody who read this
/// as a rotation would leave a machine authenticating on a credential they
/// believe they revoked.
function credentialControl(coll, r) {
  const id = idOf(r);
  const host = el("span.confirm");
  const ask = () => fill(host,
    el("span.muted", { id: "issuecredwarning" },
      "Mint a new token for " + id + "? The one it has now keeps working until " +
      "this " + coll.singular + " is deleted — this issues, it does not revoke. "),
    el("span.btns",
      el("button.btn.quiet", { type: "button", id: "confirmissuecred", onclick: go }, "Issue"),
      el("button.btn", { type: "button", onclick: rest }, "Keep the old one")));
  const rest = () => fill(host,
    el("button.btn.quiet", { type: "button", id: "issuecredbtn", onclick: ask },
      "New agent token"));
  const go = async () => {
    try {
      const answer = await issueCredential(coll, id);
      const token = answer.nodeToken || answer.poolToken;
      if (!token) throw new Error("the platform issued no token");
      rest();
      showAgentToken(id, token, coll.id === "nodes" ? "node" : "pool");
    } catch (e) {
      rest();
      toast(String((e && e.message) || e));
    }
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
  // A machine that already exists, and a credential it needs now. Registration
  // mints one and the platform keeps only a hash, which is right for a secret
  // and wrong for the only way to get one: a box whose token was lost, or a
  // pool registered before pools had credentials at all, could otherwise only
  // be given one by deleting it and making it again — and for a pool that means
  // deleting the thing every volume in it is written against.
  if (coll.id === "nodes" || coll.id === "pools") {
    acts.appendChild(credentialControl(coll, r));
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

  // Above the fields, because it is about the fields below it: an operator who
  // typed 8 vCPU into a running guest and saw nothing happen is owed this
  // before they type it again.
  const pending = coll.id === "instances" ? pendingBlock(r) : null;
  if (pending) {
    panel.appendChild(spread("Waiting for a restart", pending, "asked for, not yet running"));
  }

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

  // Who may do what in this project. Above the fields for the same reason the
  // console is: somebody opening a project's sheet is usually here to add
  // somebody to it, not to read its quota.
  if (coll.id === "projects") {
    const host = el("div", { id: "projectgrants" });
    panel.appendChild(spread("Access", host, "who may do what in this project"));
    // No redraw of the whole sheet on save: the panel keeps its own idea of
    // what is stored, and rebuilding the sheet around it was what left a second
    // save carrying the revision the first one had already moved past.
    grantsInto(host, coll, r);
  }

  // A way in, for when the network is not one. Above the fields, because
  // somebody opening a guest's sheet because it will not come up is here for
  // this and not for its vCPU count.
  if (coll.id === "instances") {
    const host = el("div", { id: "instanceconsole" });
    panel.appendChild(spread("Console", host, "the guest's serial line"));
    consoleSection(host, coll, idOf(r));
    // The display goes to its own page rather than into this column: a
    // framebuffer at sheet width is a postage stamp, and unlike the serial
    // console — whose last lines are useful at any size — a screen you cannot
    // read is not a smaller version of the feature.
    host.appendChild(el("button.btn.quiet", { type: "button", id: "screenbtn",
      onclick: () => { closeSheet(); showScreen(nameOf(r)); } }, "Open screen"));
  }

  const pairs = agreementTable(coll, r);
  if (pairs) panel.appendChild(spread("Asked vs is", pairs, "the two halves side by side"));

  panel.appendChild(spread("Specification", specTable(coll, r), "what was asked for"));
  panel.appendChild(spread("Observation", statusTable(r), "what the owner reports"));
  panel.appendChild(spread("Conditions", conditionsTable(r)));

  // What this tenant has left, on the object the allowance belongs to.
  if (coll.id === "projects") {
    const host = el("div", { id: "allowance" });
    panel.appendChild(spread("Allowance", host, "what is left, and what could actually start"));
    allowanceInto(host, idOf(r));
  }

  // What a maintenance window will cost, on the machine it is about. Fetched
  // rather than offered behind a button: the answer is only useful *before*
  // somebody commits to the window, and a control they have to find is one
  // they find afterwards.
  if (coll.id === "nodes") {
    const host = el("div", { id: "maintenance" });
    panel.appendChild(spread("Maintenance", host, "what is scheduled, and what it will cost"));
    maintenanceInto(host, r);

    // What hardware this machine has that can be passed to a guest — and what
    // comes with each piece. Passing one device through takes its whole IOMMU
    // group, because the hardware cannot isolate less than that, and somebody
    // who learns that afterwards learns it from an outage.
    //
    // `groupWith` is the API's answer, not a grouping done here. A filter on
    // equal group numbers would get the interesting case backwards: a device
    // with no group is not grouped *with* the other ungrouped ones, it is in no
    // group at all and can never be passed through.
    const devices = passableBlock(r);
    if (devices) {
      panel.appendChild(spread("Hardware", devices, "what can be passed to a guest"));
    }
  }

  if (coll.explainable) {
    const host = el("div", { id: "explain" });
    panel.appendChild(spread("Placement", host));
    // An object that is not settled is the one somebody is looking at because
    // it went wrong, so the answer is fetched rather than offered behind a
    // button they have to find.
    if (verdict(r, coll.condition).kind !== "settled") explainInto(host, coll, r);
    else fill(host, el("p.faint", "Placed. Ask for the chain if you want to see what was rejected."));
  }

  // What has happened to this thing. Last, because it is the question asked
  // second — after "what is it doing now", which is everything above.
  const history = el("div", { id: "history" });
  panel.appendChild(spread("History", history, "what was asked of it, and by whom"));
  historyInto(history, nameOf(r));

  panel.appendChild(spread("Object", metaTable(r)));
}
