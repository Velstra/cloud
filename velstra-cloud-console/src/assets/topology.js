// The project's network, drawn.
//
// Not a board. Every other screen in this console answers "what is in this
// collection"; this one answers the question somebody actually has when they
// open a cloud console for the first time — *what does my network look like* —
// and the answer is not in any one collection. It is in seven: routers,
// networks, subnets, ports, instances, floating IPs and load balancers, plus
// one fact about the cell (which machines carry external traffic).
//
// Three rules it is built on, and each is there because the alternative was
// tried somewhere in this codebase and read as a lie:
//
// 1. **Every line is an object.** Nothing is drawn because it is usually true.
//    A guest hangs under a subnet because a port says so; the internet is drawn
//    reachable because a node in this cell says `gateway`. Where there is no
//    object there is no line, and the page says why rather than leaving a gap.
// 2. **What names nothing is shown, not hidden.** A subnet whose network was
//    never created, a router pointing at a name nobody made — these are
//    accepted by the API on purpose (a reference may be written before the
//    thing it names) and they are invisible everywhere else. The map is where
//    they stop being invisible: they are drawn adrift, with what they name.
// 3. **The text carries it.** The lines and the indentation are decoration; a
//    reader with a screen reader, and the check that drives this page, both get
//    the same sentences.

/// What the map is built out of, fetched once.
///
/// Seven lists and one report, in parallel: the page is a picture of a moment,
/// and seven sequential round trips would draw one that never existed all at
/// once. A list that fails is `[]` *and* named in `trouble` — a map missing a
/// collection must not look like a project that has none of it.
async function topologyFacts() {
  const trouble = [];
  const want = async (id) => {
    const coll = collection(id);
    if (!coll) return [];
    try {
      return (await list(coll)).items;
    } catch (e) {
      trouble.push(coll.title + ": " + String((e && e.message) || e));
      return [];
    }
  };
  const [routers, networks, subnets, ports, instances, floating, balancers, nodes] =
    await Promise.all([
      want("routers"), want("networks"), want("subnets"), want("ports"),
      want("instances"), want("floatingips"), want("load-balancers"),
      // The one cell-wide fact on the page. A tenant may not read `nodes`, and
      // that is not an error here: it means this page cannot say how the
      // project reaches the internet, which it says instead of guessing.
      options("nodes").catch(() => null),
    ]);
  return { routers, networks, subnets, ports, instances, floating, balancers, nodes, trouble };
}

/// Everything a port knows about itself, gathered once.
///
/// A port is the join the whole picture hangs on — this guest, that subnet,
/// this address — so the guest it belongs to is found here rather than by
/// searching the instance list once per port.
function portIndex(f) {
  const byName = new Map();
  for (const p of f.ports) byName.set(nameOf(p), { port: p, guest: null, floating: [] });
  for (const i of f.instances) {
    for (const named of at(spec(i), "ports") || []) {
      const entry = byName.get(String(named));
      if (entry) entry.guest = i;
    }
  }
  for (const fip of f.floating) {
    const named = at(spec(fip), "port");
    const entry = named ? byName.get(String(named)) : null;
    if (entry) entry.floating.push(fip);
  }
  return byName;
}

/// One line of the map: a mark, a name, and what is true of it.
function mapRow(depth, kind, name, said, opts = {}) {
  const row = el("div.maprow", { "data-depth": String(depth), "data-kind": kind });
  row.appendChild(el("span.mapwire", { "aria-hidden": "true" }, "  ".repeat(depth)));
  row.appendChild(el("span.mapkind", kind));
  row.appendChild(opts.goes
    ? el("button.linky", { type: "button", "data-goes": opts.goes, onclick: opts.onclick }, name)
    : el("span.mapname", name));
  if (said) row.appendChild(el("span.muted.mapsaid", said));
  if (opts.trouble) row.appendChild(el("span.state.failing", mark("failing"), opts.trouble));
  return row;
}

/// Open the object a row is about, on its own board.
///
/// Not `goTo`, which detail.js already has and which takes a resource name. The
/// whole console is one script in one scope: a second function by that name
/// would replace the first, and the sheet's own links would start doing
/// something else — which is exactly what happened, and read as "following a
/// name does not change the board".
function openFromMap(collId, item) {
  const coll = collection(collId);
  if (coll) openFrom(coll, item);
}

/// How this project reaches the internet, said in objects.
///
/// Two of them, and they answer different halves. A **gateway node** is a
/// machine in this cell that carries external traffic — without one, nothing in
/// any project leaves the cell, however the tenant's own networks are wired. A
/// **floating IP** is an address in front of one port; a project can have none
/// and still reach out, and can have one that points at nothing, which is
/// exactly what an address held while its machine is replaced looks like.
function internetBlock(f) {
  const box = el("div.mapblock");
  // Each gateway with how long ago it was heard from, and that second half is
  // not decoration. On a live cell this page said "carried by peter" about a
  // machine that had not reported in fourteen hours: a way out drawn as a fact
  // when it was a hope. The platform is right not to declare a node dead — a
  // node that does not fence has no deadline — but a page that names it as the
  // way out has to say when it last spoke.
  const gateways = f.nodes === null
    ? null
    : f.nodes
        .filter((n) => at(spec(n), "gateway") === true)
        .map((n) => {
          const heard = at(status(n), "lastHeartbeat");
          return idOf(n) + (heard ? " (heard from " + ago(heard) + ")" : " (never heard from)");
        });

  box.appendChild(mapRow(0, "internet", "Internet",
    gateways === null
      ? "this account may not read the cell's machines, so this page cannot say whether any of them carries external traffic"
      : gateways.length
        ? "carried by " + gateways.join(", ")
        : "no machine in this cell is a gateway, so nothing in any project reaches out",
    gateways !== null && !gateways.length
      ? { trouble: "no way out" }
      : {}));

  for (const fip of f.floating) {
    const port = at(spec(fip), "port");
    const address = at(status(fip), "address") || at(spec(fip), "address") || "no address yet";
    box.appendChild(mapRow(1, "floating ip", idOf(fip),
      port ? address + " → " + shortName(String(port)) : address + " — in front of nothing yet",
      { goes: nameOf(fip), onclick: () => openFromMap("floatingips", fip) }));
  }
  if (!f.floating.length) {
    box.appendChild(mapRow(1, "", "No floating IPs",
      "nothing in this project has an address of its own from outside"));
  }
  return box;
}

/// The guests, addresses and load balancers on one subnet.
function subnetRows(box, f, index, subnet, depth) {
  const name = nameOf(subnet);
  const on = [...index.values()].filter((e) => String(at(spec(e.port), "subnet")) === name);
  for (const entry of on) {
    const address = at(status(entry.port), "address") || at(spec(entry.port), "address") || "no address yet";
    if (entry.guest) {
      const state = at(status(entry.guest), "state") || "unknown";
      box.appendChild(mapRow(depth, "guest", idOf(entry.guest),
        address + " · " + state + (entry.floating.length
          ? " · reachable from outside"
          : ""),
        { goes: nameOf(entry.guest), onclick: () => openFromMap("instances", entry.guest) }));
    } else {
      // An address with nothing behind it. Not a fault: it is what a port
      // outliving its machine is for, and it is the reason ports exist as
      // objects at all.
      box.appendChild(mapRow(depth, "port", idOf(entry.port),
        address + " — held, with no guest on it",
        { goes: nameOf(entry.port), onclick: () => openFromMap("ports", entry.port) }));
    }
  }
  for (const lb of f.balancers) {
    if (String(at(spec(lb), "subnet")) !== name) continue;
    const vip = at(status(lb), "vip") || at(spec(lb), "vip") || "no address yet";
    const members = (at(spec(lb), "members") || []).length;
    box.appendChild(mapRow(depth, "balancer", idOf(lb),
      vip + " · " + members + (members === 1 ? " member" : " members"),
      { goes: nameOf(lb), onclick: () => openFromMap("load-balancers", lb) }));
  }
  if (!on.length && !f.balancers.some((lb) => String(at(spec(lb), "subnet")) === name)) {
    box.appendChild(mapRow(depth, "", "nothing on it yet", ""));
  }
}

/// One network and everything under it.
function networkRows(box, f, index, network, depth) {
  const name = nameOf(network);
  const mine = f.subnets.filter((s) => String(at(spec(s), "network")) === name);
  box.appendChild(mapRow(depth, "network", idOf(network),
    "VNI " + (at(status(network), "vni") || at(spec(network), "vni") || "—") +
      " · MTU " + (at(spec(network), "mtu") || "—"),
    { goes: name, onclick: () => openFromMap("networks", network) }));
  if (!mine.length) {
    // A network without a subnet can hold a port that never gets an address —
    // a guest that boots with a dead NIC and no sign of why. The create refuses
    // to put a guest here; this is where somebody sees that before they try.
    box.appendChild(mapRow(depth + 1, "", "no subnet",
      "a guest cannot be put on this network until it has one — there is no range to take an address from",
      { trouble: "unusable" }));
    return;
  }
  for (const subnet of mine) {
    const used = at(status(subnet), "allocated");
    const free = at(status(subnet), "available");
    box.appendChild(mapRow(depth + 1, "subnet", idOf(subnet),
      (at(spec(subnet), "cidr") || "no range") +
        " · gateway " + (at(spec(subnet), "gateway") || "none") +
        (typeof used === "number" && typeof free === "number"
          ? " · " + used + " of " + (used + free) + " in use"
          : ""),
      { goes: nameOf(subnet), onclick: () => openFromMap("subnets", subnet) }));
    subnetRows(box, f, index, subnet, depth + 2);
  }
  if (mine.length > 1) {
    // The same sentence the API answers with. Somebody looking at this picture
    // is one click from a create that will refuse, and this is where the reason
    // is already on screen.
    box.appendChild(mapRow(depth + 1, "", "two ranges on one network",
      "naming this network does not say which range a new guest's address comes out of — name the subnet"));
  }
}

/// The whole picture.
function renderTopology(f) {
  const box = $("topologybox");
  clear(box);
  const index = portIndex(f);

  for (const why of f.trouble) {
    box.appendChild(el("p.err", "This map is missing something: " + why));
  }

  box.appendChild(internetBlock(f));

  const routed = new Set();
  for (const router of f.routers) {
    const joins = (at(spec(router), "networks") || []).map(String);
    const block = el("div.mapblock");
    block.appendChild(mapRow(0, "router", idOf(router),
      joins.length
        ? "every subnet on " + joins.map(shortName).join(", ") + " reaches every other"
        : "joins nothing, so it routes nothing",
      { goes: nameOf(router), onclick: () => openFromMap("routers", router) }));
    for (const named of joins) {
      const network = f.networks.find((n) => nameOf(n) === named);
      if (!network) {
        // Accepted by the API on purpose — a router may name a network before
        // it is made — and invisible everywhere but here.
        block.appendChild(mapRow(1, "network", shortName(named),
          "named by this router, and there is no such network in this project",
          { trouble: "not there" }));
        continue;
      }
      routed.add(named);
      networkRows(block, f, index, network, 1);
    }
    box.appendChild(block);
  }

  const alone = f.networks.filter((n) => !routed.has(nameOf(n)));
  if (alone.length) {
    const block = el("div.mapblock");
    block.appendChild(el("h3", f.routers.length ? "Not routed" : "Networks"));
    block.appendChild(el("p.prose.muted", f.routers.length
      ? "No router joins these, so a guest on one cannot reach a guest on another. Guests on the same network can already talk."
      : "This project has no router. Guests on the same network can talk; nothing crosses between networks."));
    for (const network of alone) networkRows(block, f, index, network, 0);
    box.appendChild(block);
  }

  // Everything that names something nobody made. The API accepts these — a
  // reference may be written before its target — so nothing else in the console
  // ever mentions them, and an operator finds out when the thing silently never
  // works.
  const names = new Set(f.networks.map(nameOf));
  const subnetNames = new Set(f.subnets.map(nameOf));
  const adrift = [];
  for (const s of f.subnets) {
    const named = String(at(spec(s), "network") || "");
    if (!names.has(named)) {
      adrift.push(mapRow(0, "subnet", idOf(s),
        "on " + (named ? shortName(named) : "no network") + ", which does not exist — nothing on it can ever be reached",
        { goes: nameOf(s), onclick: () => openFromMap("subnets", s), trouble: "adrift" }));
    }
  }
  for (const { port } of index.values()) {
    const named = String(at(spec(port), "subnet") || "");
    if (named && !subnetNames.has(named)) {
      adrift.push(mapRow(0, "port", idOf(port),
        "on " + shortName(named) + ", which does not exist — this NIC can never be given an address",
        { goes: nameOf(port), onclick: () => openFromMap("ports", port), trouble: "adrift" }));
    }
  }
  if (adrift.length) {
    const block = el("div.mapblock");
    block.appendChild(el("h3", "Adrift"));
    block.appendChild(el("p.prose.muted",
      "These name something this project does not have. The platform accepts a name " +
      "written before the thing it names, so nothing refused them — and nothing else " +
      "shows them. They will not start working on their own."));
    for (const row of adrift) block.appendChild(row);
    box.appendChild(block);
  }

  if (!f.networks.length && !f.routers.length && !adrift.length) {
    box.appendChild(el("p.prose",
      "This project has no networks yet. It does not need any: the first guest " +
      "made here gets a default network, made for it, and a second guest joins " +
      "the same one."));
  }
}

/// The map, as a page.
///
/// Its own screen rather than a panel on the overview: the overview answers
/// "is anything wrong" across the whole cell, and this answers "how is my
/// project wired", which is a question about one project and has no bad news in
/// it most of the time.
async function showTopology() {
  if (view.watcher) { view.watcher.stop(); view.watcher = null; }
  stopRecheck();
  session.labels = "";
  view.coll = null;
  view.home = false;
  view.map = true;
  view.items = [];
  view.picked.clear();
  $("picked").classList.add("hidden");
  location.hash = "#map";

  $("listtitle").textContent = "Network map";
  fill($("listblurb"), "How this project is wired: what reaches the internet, " +
    "which networks route to each other, and which guest holds which address.");
  clear($("listacts"));
  $("listfilter").classList.add("hidden");
  $("listerr").classList.add("hidden");
  $("cpuadvisory").classList.add("hidden");
  $("listempty").classList.add("hidden");
  $("overviewbox").classList.add("hidden");
  document.querySelector(".boardwrap").classList.add("hidden");
  $("topologybox").classList.remove("hidden");
  fill($("topologybox"), el("p.faint", "Asking…"));

  renderRail();
  renderTopology(await topologyFacts());
}
