// One object, in full: what was asked for, what is, and whether they agree.
//
// The three are never shown apart. A status value alone cannot be judged —
// `Stopped` is a fault or exactly right depending on the spec beside it — and a
// spec alone is a wish. So the sheet leads with the verdict, then puts the two
// halves in the same table, and only then lists the object's own detail.

const sheet = { open: false, name: null, coll: null, timer: null, closers: [], opener: null };

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
  // The keyboard goes back where it came from. A panel that takes focus and
  // then drops it on the body leaves whoever closed it at the top of the page:
  // somebody who opened row forty with the keyboard had to tab past thirty-nine
  // rows to get back to it, every time.
  const back = sheet.opener;
  sheet.opener = null;
  if (back && back.isConnected) { try { back.focus(); } catch (e) {} }
}

/// What the tab key can land on inside one element, in the order it would.
///
/// A control inside a folded disclosure is left out: focus that lands on
/// something nobody can see is the same trap one level down.
const REACHABLE = "a[href], button:not([disabled]), input:not([disabled]), " +
  "select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex=\"-1\"])";
function reachable(root) {
  return [...root.querySelectorAll(REACHABLE)].filter((n) => n.offsetParent !== null);
}

/// The keyboard while the sheet is up.
///
/// The sheet has always covered the page and taken the pointer, and until now
/// it took nothing else: tab walked straight out of it and into the board
/// behind the scrim, where every row is focusable and none of it can be seen or
/// clicked. There was no way back but the mouse, and nothing on screen said
/// where the focus had gone.
function sheetKey(e, panel) {
  // A form opened *from* the sheet sits on top of it and owns the keyboard.
  // Escape is handled once, innermost first, in `app.js`.
  if ($("dialog")) return;
  if (e.key === "Escape") { e.stopPropagation(); closeSheet(); return; }
  if (e.key !== "Tab") return;
  const stops = reachable(panel);
  if (!stops.length) { e.preventDefault(); return; }
  // `-1` is the heading the sheet opens on, which is focusable but not a tab
  // stop: forwards from there the browser lands on the first control by itself,
  // and backwards it would leave, so that is the case the wrap has to catch.
  const here = stops.indexOf(document.activeElement);
  if (e.shiftKey && here <= 0) { e.preventDefault(); stops[stops.length - 1].focus(); }
  else if (!e.shiftKey && here === stops.length - 1) { e.preventDefault(); stops[0].focus(); }
}

function openSheet(coll, r) {
  // Whoever pressed, read before the old sheet is torn down: closing one hands
  // focus back itself, so asking afterwards would name the previous sheet's
  // opener rather than the row that was just clicked.
  //
  // A sheet drawn again over itself keeps the opener it already has. By the
  // time a power press, a saved edit or an operation that ended re-opens one,
  // the button that started it has been disabled or has gone with the panel it
  // was in — so what this reads is `document.body`, and the keyboard would go
  // back to the top of the page, which is the walk back through forty rows
  // that remembering the opener exists to save.
  const pressed = document.activeElement;
  const opener = sheet.open || !pressed || pressed === document.body
    ? sheet.opener
    : pressed;
  closeSheet();
  const scrim = el("div", { id: "scrim", onclick: closeSheet });
  // A modal, and said so. `role="dialog"` on its own tells a screen reader what
  // this is and not that everything behind it is out of reach — so the board
  // under the scrim, which cannot be clicked and cannot be seen, was still
  // there to be read line by line. `aria-modal` is what closes it off, and the
  // label names the object rather than leaving "dialog" to stand for it.
  const panel = el("aside", {
    id: "sheet", role: "dialog", "aria-modal": "true", tabindex: "-1",
    "aria-label": coll.singular + " " + idOf(r),
    onkeydown: (e) => sheetKey(e, panel),
  });
  document.body.appendChild(scrim);
  document.body.appendChild(panel);
  sheet.open = true; sheet.coll = coll; sheet.name = nameOf(r); sheet.opener = opener;
  renderSheet(coll, r);
  // Into the sheet, at the heading: it is the one line that says which object
  // this is about, and starting at the first control skips it.
  const head = panel.querySelector("h2") || panel;
  try { head.focus(); } catch (e) {}
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

/// The disclosure the create form uses, around anything.
///
/// One level deeper, one click away, with its own inset surface — the same
/// wording and the same aria wiring wherever it is used, so a reader who has
/// opened one has opened all of them. `shut` carries the count, always: a fold
/// that does not say how much is behind it is a fold nobody opens.
///
/// `id` where something needs to address it; otherwise one is minted, because
/// the button and the region it controls have to be tied together by id and
/// two of these can be on screen at once.
let foldsMade = 0;
function folded(id, shut, open, body) {
  const key = id || "fold" + ++foldsMade;
  const deeper = el("div.deeper.hidden", { id: key + "fields" }, body);
  const toggle = el("button.disclose", { type: "button", id: key,
    "aria-expanded": "false", "aria-controls": key + "fields" }, shut);
  toggle.addEventListener("click", () => {
    const shown = !deeper.classList.toggle("hidden");
    toggle.setAttribute("aria-expanded", String(shown));
    toggle.classList.toggle("open", shown);
    toggle.textContent = shown ? open : shut;
  });
  return el("div.specfold", toggle, deeper);
}

/// How many entries a list may have before it is folded.
///
/// A node reports about a hundred CPU flags, and printed in full they *are* the
/// Observation panel: everything the machine actually said about itself sits
/// below them, off the bottom of the sheet. Twelve is about a screen of a
/// narrow column, which is the most a value should cost the panel around it.
const LONG_LIST = 12;

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
    const list = el("div", v.map((x) =>
      el("div", x !== null && typeof x === "object"
        ? valueNode(x)
        : nameLink(x) || el("span.mono", { title: String(x) }, shortName(x)))));
    // Nothing is dropped, only folded — with the count on the button, so the
    // length is readable without reading the list.
    return v.length > LONG_LIST
      ? folded(null, "Show all " + v.length, "Show fewer", list)
      : list;
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
  const answered = at(statusOf(r), "pendingChanges");
  if (!Array.isArray(answered)) return [];
  // The unit travels with the label. `4096 → 8192` and `5 → 20` are read off
  // the same column one under the other, and the wire spells the field
  // `memoryMib` — which is the unit, in a name nobody reads as one.
  const named = {
    vcpus: { label: "vCPUs", unit: "" },
    memoryMib: { label: "Memory", unit: "MiB" },
    rootDiskGib: { label: "Root disk", unit: "GiB" },
  };
  return answered.map((c) => {
    const field = pick(c, "field");
    const said = named[field] || { label: label(String(field)), unit: "" };
    return { label: said.label, unit: said.unit, from: pick(c, "from"), to: pick(c, "to") };
  });
}

/// One side of a pending change, with its unit where there is one.
const withUnit = (c, v) => String(v) + (c.unit ? " " + c.unit : "");

/// A node's PCI devices, each with what it drags along.
function passableBlock(r) {
  const devices = at(statusOf(r), "pciDevices");
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
      el("span.cpuval.mono", withUnit(c, c.from) + " \u2192 " + withUnit(c, c.to))))));
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
    const is = at(statusOf(r), a.is);
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
  // And the sizes, which are the same question and were not in this table.
  //
  // The schema pairs a spec field with a status field, and a guest's size has
  // no status field to pair with: what it is running on is `status.runningSize`
  // as a whole, and the difference is `status.pendingChanges`. So a guest
  // resized from one vCPU to two while it ran showed two rows — power and node
  // — both saying "agrees", on an object that was carrying the disagreement in
  // a field this panel never read.
  //
  // Not called a disagreement and not tinted like one: nothing has failed here,
  // and the platform is never going to resize a machine that is up. The mark
  // says something is outstanding; the word says what it is waiting for.
  const waiting = pendingChanges(r);
  for (const c of waiting) {
    any = true;
    body.appendChild(el("tr",
      el("td.muted", c.label),
      el("td", withUnit(c, c.to)),
      el("td", withUnit(c, c.from)),
      el("td", el("span.state.drifting", mark("drifting"), "at next start"))));
  }
  if (waiting.length) {
    body.appendChild(el("tr", el("td", { colspan: "4" }, el("div.note",
      "The guest is running on what is under “Is”. It gets what was asked for when it next " +
      "starts — nothing here changes a machine that is already up."))));
  }
  table.appendChild(body);
  return any ? table : null;
}

/// Which network a guest is on, by way of the ports it holds.
///
/// `spec.networks` is *consumed* on create: the API mints a port per network —
/// or one on the project's default network when nothing was named — stores the
/// ports and empties the field, because two fields describing one set of
/// interfaces are two fields that drift. So the field is empty on every guest
/// that exists, and the row that renders it verbatim said "Networks: none"
/// about a running machine with an address on a network.
///
/// The ports are named and followable straight away, so the row is right before
/// anything has been asked. The network is a fact about the port and takes a
/// read per port to get; the names replace the sentence when they arrive, the
/// way an image's do, and what is on screen is true either way.
function throughPorts(r) {
  const ports = at(spec(r), "ports");
  if (!Array.isArray(ports) || !ports.length) return null;
  const mono = (v) => el("span.mono", { title: String(v) }, shortName(String(v)));
  // The answer when there is one, and the ports underneath either way — so the
  // line still says how the guest is attached once the network has replaced the
  // sentence above it.
  const found = el("div", el("span.faint", "through its ports"));
  const box = el("div", found,
    el("div", el("span.faint", "by way of "),
      ports.map((p, i) => el("span", i ? ", " : "", nameLink(p) || mono(p)))));
  const coll = collection("ports");
  if (!coll) return box;
  Promise.all(ports.map((p) => get(coll, String(p)).catch(() => null)))
    .then((answers) => {
      const lines = [];
      const seen = new Set();
      for (const port of answers) {
        const network = port ? at(spec(port), "network") : null;
        if (!network) continue;
        const subnet = at(spec(port), "subnet");
        // Two ports on one network are one answer. A guest with a second NIC on
        // the same network is ordinary, and a row that named it twice would
        // read as two networks.
        const key = String(network) + "|" + String(subnet || "");
        if (seen.has(key)) continue;
        seen.add(key);
        lines.push(el("div",
          nameLink(network) || mono(network),
          subnet ? el("span.faint", " · ") : null,
          subnet ? (nameLink(subnet) || mono(subnet)) : null));
      }
      if (lines.length) fill(found, lines);
    })
    .catch(() => {});
  return box;
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
    case "refList": {
      // "none" is the wrong answer for a guest's networks — see `throughPorts`.
      const through = f.key === "networks" && (!v || !v.length) ? throughPorts(r) : null;
      if (through) return through;
      return !v || !v.length
        ? el("span.blank.faint", "none")
        : el("div", v.map((x) => el("div", refValue(f, x))));
    }
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
//
// A spread/affinity *mode* without its group is the model's default, not a
// choice anybody made — and "Keeping them apart is: a rule" on a guest in no
// group reads as a rule that exists. The mode folds away with its group.
const MODE_OF = {
  "placementPolicy.spread": "placementPolicy.antiAffinityGroup",
  "placementPolicy.affinity": "placementPolicy.affinityGroup",
};
function carriesKey(r, f) {
  const partner = MODE_OF[f.key];
  if (partner && !at(spec(r), partner)) return false;
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
  return el("div.specfold", table,
    folded("specmore", "More settings (" + n + ")", "Fewer settings", el("table.kv", deep)));
}

/// Whoever is signed in runs this cell, so nothing on these pages is an
/// implementation detail to them.
const runsTheCell = () => !!(session.who && session.who.cellAdmin);

/// Reported fields that are about how the platform is built rather than about
/// the customer's machine.
///
/// The VMM's process id is the case: the pid of a process on a host the tenant
/// cannot log into, on a machine the API does not even tell them the name of,
/// sitting in the middle of the facts about their own guest. It is genuinely
/// useful to whoever operates the cell, so it is not removed — it is one level
/// down for everybody else.
const HOST_SIDE = new Set(["vmmPid", "vmm_pid"]);

function statusTable(r) {
  const table = el("table.kv");
  const body = el("tbody");
  const deep = el("tbody");   // host-side detail, for an account that is not the cell's
  for (const [k, v] of Object.entries(statusOf(r))) {
    if (k === "conditions" || k === "observedGeneration" || k === "observed_generation") continue;
    // A millisecond timestamp reads as one wherever the name says "when":
    // `startedAt`, `lastHeartbeat`, and a user's `lastLogin` — which showed as
    // a thirteen-digit number until "login" was on this list.
    const isTime = /(at|heartbeat|transition|login|seen|expires|since|until)$/i.test(k)
      && typeof v === "number" && v > 1e12;
    (HOST_SIDE.has(k) && !runsTheCell() ? deep : body).appendChild(el("tr",
      el("td", label(k)),
      el("td", isTime ? el("span", { title: stamp(v) }, ago(v)) : valueNode(v), isTime ? null : alsoIn(k, v))));
  }
  if (!body.childElementCount && !deep.childElementCount) {
    body.appendChild(el("tr", el("td", { colspan: "2" },
      el("span.faint", "Nothing has been reported about this object yet."))));
  }
  table.appendChild(body);
  if (!deep.childElementCount) return table;
  const n = deep.childElementCount;
  return el("div.specfold", table,
    folded("hostside", "Host-side detail (" + n + ")", "Fewer details", el("table.kv", deep)));
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
  const cs = pick(statusOf(r), "conditions") || [];
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

/// The object itself: what it is called, when it arrived — and, for whoever
/// operates the cell, how the platform holds it.
///
/// The third element of a row marks it as the platform's own bookkeeping: a uid
/// nobody addresses anything by, the revision an `If-Match` carries, the
/// generation counter the Convergence panel above already gives twice in words,
/// which region and cell hold the row, and the names of the controllers keeping
/// a deletion open. An operator reads all of it — it is how a support call gets
/// answered — and a customer got five rows of somebody else's implementation
/// above the two facts they came for. Folded, not dropped: it is one click away
/// for them too, and the order an operator sees is unchanged.
function metaTable(r) {
  const m = meta(r);
  const p = pick(m, "placement") || {};
  const labels = pick(m, "labels") || {};
  const finalizers = pick(m, "finalizers") || [];
  const rows = [
    ["Name", el("span.mono", nameOf(r))],
    ["UID", el("span.mono", String(pick(m, "uid") || "—")), true],
    ["Placement", el("span.mono", (p.region || "?") + " · " + (p.cell || "?")), true],
    ["Generation", el("span.num", String(generation(r))), true],
    ["Revision", el("span.mono", revision(r) === null ? "—" : revision(r)), true],
    ["Created", el("span", { title: stamp(pick(m, "createdAt")) }, ago(pick(m, "createdAt")))],
  ];
  if (deletedAt(r)) rows.push(["Deletion asked",
    el("span", { title: stamp(deletedAt(r)) }, ago(deletedAt(r)))]);
  if (finalizers.length) rows.push(["Held by", valueNode(finalizers), true]);
  if (Object.keys(labels).length) rows.push(["Labels", valueNode(labels)]);
  const line = ([k, v]) => el("tr", el("td", k), el("td", v));
  const cell = runsTheCell();
  const table = el("table.kv", el("tbody", rows.filter((x) => cell || !x[2]).map(line)));
  const deep = cell ? [] : rows.filter((x) => x[2]);
  if (!deep.length) return table;
  return el("div.specfold", table,
    folded("objectmore", "Platform detail (" + deep.length + ")", "Fewer details",
      el("table.kv", el("tbody", deep.map(line)))));
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
/// Two sources, deliberately in one list: the operations, which are the
/// receipts for what the platform was asked to converge, and the audit records,
/// which are what the API itself did — every change it accepted and everybody
/// it told no. Reading only the first is how somebody concludes their click did
/// nothing — the refusal is the answer, and it lives in a collection they would
/// otherwise never open.
async function historyInto(host, name) {
  fill(host, el("p.faint", "Asking…"));
  try {
    // `refusals` as the API hands it over, `audit` here: the collection holds
    // every kind of record, and reading it as a list of refusals is exactly the
    // mistake the loop below used to make.
    const { operations, refusals: audit } = await historyOf(name);
    const lines = [];
    for (const o of operations) {
      const s = statusOf(o);
      const at = pick(s, "finishedAt") || pick(meta(o), "createdAt");
      lines.push({
        at: Number(at || 0),
        kind: pick(s, "error") ? "failing" : pick(s, "done") ? "settled" : "drifting",
        what: String(pick(spec(o), "verb") || "change"),
        who: String(pick(spec(o), "requestedBy") || "—"),
        detail: String(pick(s, "error") || (pick(s, "done") ? "" : "still running")),
      });
    }
    for (const a of audit) {
      // `spec.kind` says which of the two this is, and until it was read every
      // record in this panel was painted red and had " refused" put after its
      // verb — including the `changed` ones, which are the record of a write
      // that *worked*. Creating a user read "create refused by admin — created
      // it" on the account sitting there, made; editing a project read "update
      // refused by admin — changed it". A change gets the verb alone and the
      // settled mark; only `refused` keeps the word and the failing one.
      const refused = String(pick(spec(a), "kind") || "").toLowerCase() === "refused";
      const verb = String(pick(spec(a), "verb") || "?");
      lines.push({
        at: Number(pick(meta(a), "createdAt") || 0),
        kind: refused ? "failing" : "settled",
        what: refused ? verb + " refused" : verb,
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
/// One month, as metric-hours. The gap between `hours` and the hours the
/// month has held is shown rather than smoothed over: a cell that was down
/// took no readings, and a bill with invented hours is worse than one with a
/// stated gap.
async function consumptionInto(host, project) {
  fill(host, el("p.faint", "Asking…"));
  try {
    const u = await explainUsage(project);
    const rows = [
      ["vCPU-hours", u.vcpuHours],
      ["Memory GiB-hours", u.memoryGibHours],
      ["Storage GiB-hours", u.volumeGibHours],
      ["Instance-hours", u.instanceHours],
      ["Public-address-hours", u.floatingIpHours],
    ].filter(([, v]) => Number(v) > 0);
    const gap = Number(u.hoursInMonthSoFar || 0) - Number(u.hours || 0);
    fill(host,
      el("p.muted", u.month + " · " + u.hours + " hourly readings" +
        (gap > 0 ? " — " + gap + " hours of the month have no reading and are not counted" : "")),
      rows.length
        ? el("table.kv", el("tbody",
            rows.map(([label, v]) => el("tr", el("td", label), el("td", String(v))))))
        : el("p.muted", "Nothing was in use this month."));
  } catch (e) {
    fill(host, el("p.err", String((e && e.message) || e)));
  }
}

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

/// What goes with the object, per collection, in one sentence.
///
/// The question was "Delete <id>?" for everything — the same words over a
/// throwaway port and over the volume somebody's database is on. What a person
/// is actually deciding is not whether to delete a row, it is whether they are
/// ready to lose what the row stands for, and that differs enough between these
/// four that saying it is the difference between a question and a formality.
///
/// Only the four where the answer is bytes or an address. Everything else is a
/// declaration the platform can be told again, and a sentence on each of those
/// would train people to click through this one.
const DELETE_COSTS = {
  instances: "Its root disk goes with it and the addresses it holds are released.",
  volumes: "The data on it goes with it, and the platform keeps no copy to restore from.",
  backups: "This is the copy itself, not a reference to one: the bytes go with it.",
  captures: "This is the image itself, not a reference to one: the bytes go with it, " +
    "and nothing made from it afterwards.",
};

/// `opts.verb` renames the action where "delete" is the wrong word for it, and
/// `opts.warning` is what the operator is actually deciding — used where that
/// differs from object to object, which is exactly one place: abandoning a
/// migration means something different under every mode.
function deleteControl(coll, r, opts = {}) {
  const verb = opts.verb || "Delete";
  const host = el("span.confirm");
  const ask = () => {
    // Deleting a guest is destructive and cannot be undone, so it asks — once,
    // in place, naming what it is about to delete and what goes with it. This
    // and the two power presses that take a machine away are the only things on
    // this page that ask, which is what keeps the question meaningful.
    const costs = DELETE_COSTS[coll.id];
    fill(host,
      opts.warning
        ? el("p" + (opts.grave ? ".err" : ".muted"), { id: "deletewarning" }, opts.warning)
        : el("span.muted", { id: costs ? "deletewarning" : null },
            verb + " " + idOf(r) + "? " + (costs ? costs + " " : "")),
      el("span.btns",
        btn(verb, { quiet: true, id: "confirmdelete", onclick: go }),
        btn("Keep", { onclick: rest })));
  };
  const rest = () => fill(host,
    btn(verb, { quiet: true, id: "deletebtn", onclick: ask }));
  const go = async () => {
    // Say so while it runs: a delete that answers in a second reads as one
    // that did nothing until the row is gone.
    working(host.querySelector("#confirmdelete"));
    try {
      // Its own name, which `pathFor` takes as it is. `idOf` rebuilt the path
      // out of the project currently selected, and on the one board that merges
      // two scopes — the catalogue's cell-wide images beside the project's —
      // that addressed an object nobody had named: retiring a published image
      // answered "projects/p1/images/debian-13 does not exist".
      await remove(coll, nameOf(r), revision(r));
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
      btn("Issue", { quiet: true, id: "confirmissuecred", onclick: go }),
      btn("Keep the old one", { onclick: rest })));
  const rest = () => fill(host,
    btn("New agent token", { quiet: true, id: "issuecredbtn", onclick: ask }));
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

/// Ask again, a few seconds apart, until there is an answer or the time is up.
///
/// Bounded, and deliberately not by much: this is a courtesy on top of the
/// object, which is still where the truth is written, and a console that kept
/// asking all afternoon would be a tab that never goes quiet on a screen people
/// leave open for days.
const ASK_AGAIN_MS = 3000;
const ASK_FOR_MS = 30000;

/// `{ answer }`, `{ timedOut }` or `{ gone }`.
///
/// `about` ties the asking to a sheet: when that sheet is closed, or has moved
/// to another object, there is nobody left to tell and the asking stops. A
/// caller in the middle of a change of its own passes nothing and is left to
/// finish it — a guest stopped for a restart has to be started again whether or
/// not anybody is still watching the panel that asked.
async function askAgainUntil(ask, about) {
  const until = Date.now() + ASK_FOR_MS;
  for (;;) {
    await new Promise((go) => setTimeout(go, ASK_AGAIN_MS));
    if (about && (!sheet.open || sheet.name !== about)) return { gone: true };
    const answer = await ask().catch(() => null);
    if (answer !== null && answer !== undefined) return { answer };
    if (Date.now() >= until) return { timedOut: true };
  }
}

/// Follow the operation a write minted, and say so if it ends badly.
///
/// A write answers the moment the API has *recorded* it, which is not the
/// moment it happened: the object is accepted, an operation is minted, and
/// anything that goes wrong afterwards is written there. Nothing on this page
/// was looking, so a change a controller refused a second later reported
/// nothing at all and the sheet went on showing the ask.
///
/// Only for an answer that names one. A create answers `202` with
/// `{operation, target}`; a change and a delete answer with the object and name
/// the operation in a header, which `request` does not hand on — so those are
/// followed the day it does, and nothing is inferred here from the shape of a
/// body.
async function followOperation(answer, coll, r) {
  const operations = collection("operations");
  const name = answer && typeof answer === "object" ? pick(answer, "operation") : null;
  if (!operations || !name || typeof name !== "string") return;
  const about = nameOf(r);
  const ended = await askAgainUntil(
    () => get(operations, name).then((op) => (pick(statusOf(op), "done") === true ? op : null)),
    about);
  const failed = ended.answer ? pick(statusOf(ended.answer), "error") : null;
  if (!failed) return;
  toast(String(failed), "bad");
  // And the object as it is now, because the sentence is about it: a guest
  // whose change was refused is not the guest this sheet was drawn from.
  const fresh = await get(coll, about).catch(() => null);
  if (fresh && sheet.open && sheet.name === about) openSheet(coll, fresh);
}

/// Power, on the machine itself.
///
/// Start and Stop lived on the board's bulk bar and nowhere else, so stopping
/// one guest meant leaving its sheet, finding its row among forty, ticking a
/// box and using a control written for doing one thing to many. Restart did not
/// exist in the product at all.
///
/// There is no restart verb, and there must not be one: a spec says what a
/// guest should be doing, and "off, then on" is not a state a machine can be
/// in. So it is two asks with a wait between them, done here — where a failure
/// at either step is said out loud, and the guest is left somewhere this sheet
/// can describe truthfully rather than half way through something invisible.
function powerControl(coll, r) {
  const name = nameOf(r);
  const guest = idOf(r);
  const host = el("span.confirm");
  // What was *asked* for, not what is reported. This control changes the ask,
  // and a guest already asked to stop must not be offered Stop a second time
  // while the node works on it. Nothing set means Running: a guest somebody
  // asked to exist runs.
  const wants = String(pick(spec(r), "desiredState") || "Running");

  // The object as it is now, drawn again. Whatever happened, the sheet is what
  // says which state the guest is in, so it is what is brought up to date.
  const again = async () => {
    const fresh = await get(coll, name).catch(() => null);
    if (fresh && sheet.open && sheet.name === name) openSheet(coll, fresh);
    // Only the board this guest is on. A restart waits for the machine to go
    // down before it asks for it to come back, which is half a minute somebody
    // spends elsewhere — and a list that replaces itself with instances because
    // a press finished behind them is a console that navigates on its own.
    if (view.coll && view.coll.id === coll.id) show(coll.id);
  };

  // One ask, either way: what changes between Start and Stop is the word in the
  // spec, and a refusal lands the same way for both.
  const goPower = async (state) => {
    try {
      const answer = await patch(coll, name, { spec: { desiredState: state } }, revision(r));
      toast("Asked for. The node reports the state; watch the observation catch up.");
      followOperation(answer, coll, r);
      await again();
    } catch (e) { toast(e.message, "bad"); rest(); }
  };

  const goRestart = async () => {
    try {
      const stopping = await patch(coll, name, { spec: { desiredState: "Stopped" } }, revision(r));
      followOperation(stopping, coll, r);
      // Read back until the node says it is off. Asking for Running while the
      // guest is still up is the spec it already has — not a write, and not a
      // restart: the platform would accept it, change nothing, and this control
      // would have reported a bounce that never happened.
      const off = await askAgainUntil(() =>
        get(coll, name).then((fresh) => (at(statusOf(fresh), "state") === "Stopped" ? fresh : null)));
      if (!off.answer) {
        toast(guest + " has not stopped, so it was not started again. It is asked to be stopped; " +
          "starting it is one press once it is.", "bad");
        await again();
        return;
      }
      // The revision the wait ended on, not the one this sheet was drawn with:
      // the stop moved it, and an If-Match carrying the old one is refused.
      await patch(coll, name, { spec: { desiredState: "Running" } }, revision(off.answer));
      toast("Stopped, and asked to start again.");
      await again();
    } catch (e) {
      toast(e.message, "bad");
      await again();
    }
  };

  // Stopping a machine is the machine going away for a while, so it asks and
  // names what goes with it — the same rule the bulk bar and the delete follow.
  // Starting one asks nothing: it is the press that undoes the other two.
  const askFirst = (which) => {
    const stopping = which === "stop";
    fill(host,
      el("p.muted", { id: which + "warning" }, stopping
        ? "Stop " + guest + "? Whatever it is serving stops answering until it is started again."
        : "Restart " + guest + "? It is stopped and started again, and whatever it is serving " +
          "stops answering until it is back up."),
      el("span.btns",
        // The word while it runs is spelled out rather than derived: a restart
        // waits for the guest to go down before it asks for it to come back, so
        // this is the press on this sheet that most needs to say it is working.
        btn(stopping ? "Stop" : "Restart",
          { quiet: true, id: "confirm" + which,
            busy: stopping ? "Stopping…" : "Restarting…",
            onclick: stopping ? () => goPower("Stopped") : goRestart }),
        btn(stopping ? "Leave it running" : "Leave it as it is", { onclick: rest })));
  };

  function rest() {
    fill(host,
      wants === "Stopped"
        ? btn("Start", { id: "startbtn", busy: "Starting…", onclick: () => goPower("Running") })
        : btn("Stop", { id: "stopbtn", onclick: () => askFirst("stop") }),
      // Offered whichever way the guest is asked to be: the machine somebody
      // most wants to bounce is the one that is up and wrong, and a restart of
      // a stopped guest is a start that does not need talking out of.
      btn("Restart", { id: "restartbtn", onclick: () => askFirst("restart") }));
  }

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
      // `tabindex="-1"` so the sheet can put the keyboard here when it opens.
      // Not reachable by tab: a heading in the tab order is a stop that does
      // nothing on every pass after the first.
      el("h2", { tabindex: "-1" }, idOf(r)),
      el("p.faint.mono", { title: nameOf(r) }, coll.singular + " · " + nameOf(r))),
    btn("Close", { id: "closesheet", onclick: closeSheet })));

  const acts = el("div.sheetacts");
  // Objects whose fields are the cell's even when a tenant may open the sheet:
  // a project's quota, policy and parent are set by a cell operator, and the
  // API refuses a patch that carries them from anybody else. A project admin
  // reaches this sheet for the Access panel — who may do what — and gets no
  // Edit that would send the quota back and be refused for it.
  const cellsPen = ["flavors", "projects"];
  const holdsThePen = !cellsPen.includes(coll.id) || (session.who && session.who.cellAdmin);
  // And what this account's rung admits: an operator gets Edit (resize,
  // power, attach — the API refuses the rest of the form for them), a viewer
  // gets neither. Drawn from `whoami`, so the button that appears is one
  // that will be accepted.
  if (coll.editable && holdsThePen && allows("edit")) {
    acts.appendChild(btn("Edit", { primary: true, id: "editbtn", onclick: () => openEdit(coll, r) }));
  }
  // Power, beside the rest, on the machine it is about. The same rung as Edit,
  // and for the same reason: the API treats a change of desired state as one of
  // the things an operator may do, so the button that appears is one that will
  // be accepted.
  if (coll.id === "instances" && allows("edit")) {
    acts.appendChild(powerControl(coll, r));
  }
  // Placement is a statement about the machine room, and the API refuses the
  // verb to anybody who cannot see machines — so the button only exists where
  // pressing it answers.
  if (coll.explainable && session.who && session.who.cellAdmin) {
    acts.appendChild(btn("Explain placement", {
    id: "explainbtn",
    onclick: () => explainInto($("explain"), coll, r),
  }));
  }
  // A password is not a field on this sheet and cannot be: the platform stores
  // a hash and cannot show one. Setting it is therefore an *action*, next to the
  // others, rather than a control that would have to render a value it has no
  // way to read.
  if (coll.id === "users") {
    acts.appendChild(btn("Set password", { id: "setpasswordbtn", onclick: () => openPasswordDialog(idOf(r)) }));
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
  if (coll.deletable && holdsThePen && allows("delete")
      && (coll.id !== "audit" || (session.who && session.who.cellAdmin))) {
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
  // Which machine runs the guest, and the way to move it — the operator's
  // half of the sheet. A tenant's answer has no node in it (the API takes the
  // name off the object) and no migrations to read, so for them this section
  // could only be a heading over an error.
  if (coll.id === "instances" && session.who && session.who.cellAdmin) {
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
    host.appendChild(btn("Open screen", {
    quiet: true,
    id: "screenbtn",
    onclick: () => { closeSheet(); showScreen(nameOf(r)); },
  }));
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
    // And what this month has cost so far — the hourly readings, added up the
    // way a bill is. Beside the allowance because the two are the same
    // conversation: what may I use, and what have I used.
    const used = el("div", { id: "consumption" });
    panel.appendChild(spread("Consumption", used, "this month, summed from the hourly readings"));
    consumptionInto(used, nameOf(r));
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

  if (coll.explainable && session.who && session.who.cellAdmin) {
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
