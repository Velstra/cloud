// The REST contract, in memory, so the console can be driven before — and
// independently of — the API that will really serve it.
//
// This is not a mock of the console's own calls: it is `docs/rest-contract.md`
// implemented, including the parts that are easy to get wrong and that the
// console is built on. `generation` moves only when `spec` really changed, a
// PATCH carrying `status` is refused by field name, `If-Match` is honoured,
// DELETE is two-phase and leaves the object listable behind its finalizers, and
// the watch replays from a revision so nothing between a list and a watch is
// lost. A console that passes against this is a console that read the contract
// the same way the API is being written to.
//
// Everything under `/__test/` is scaffolding — it is what lets a test make an
// agent report, which is the one thing no client of a level-triggered system
// can do for itself.

import { createServer } from "node:http";
import { readFileSync } from "node:fs";

const PAGE = process.env.CONSOLE_PAGE;
const TOKEN = process.env.CONSOLE_TOKEN || "testtoken";
const page = PAGE ? readFileSync(PAGE, "utf8") : "<!doctype html><p>no page";

let clock = 400;
const nextRevision = () => String(++clock);
const now = () => Date.now();

// name -> resource. One flat map: the name carries the project, exactly as the
// store's keys do.
const store = new Map();
const watchers = new Set();

function put(name, spec, status, extra = {}) {
  const r = {
    meta: {
      name,
      uid: "uid-" + name.replace(/\//g, "-"),
      generation: extra.generation ?? 1,
      revision: nextRevision(),
      placement: { region: "eu-central", cell: "cell-1" },
      // Settable, because a migration's timeout is computed from how old the
      // object is: a seed an hour old would time out the moment it is read.
      createdAt: extra.createdAt ?? now() - 3_600_000,
      deletedAt: null,
      finalizers: extra.finalizers || [],
      labels: extra.labels || {},
    },
    spec,
    status,
  };
  store.set(name, r);
  return r;
}

const condition = (kind, status, reason, message, observedGeneration, lastTransition) => ({
  kind, status, reason, message, observedGeneration,
  lastTransition: lastTransition ?? now() - 120_000,
});

const ready = (gen) => [condition("Ready", "True", "Ready", "", gen)];

// ---- computed on read ------------------------------------------------------
//
// A migration's `Moved` is not written by anybody and is not in the store. It is
// a judgement over the whole dance — the migration, the instance it names, and
// the clock — so the API works it out every time the object is read, exactly
// like an operation's `done`. Two things follow, and both are what the console
// is built against:
//
//   * it can never disagree with the world, because there is no stored copy to
//     go stale, and a migration whose destination agent died still reports;
//   * a migration that runs past its timeout becomes failed with *nothing
//     written* — no write, no revision, no watch event. A client that only
//     listens never hears about it.
//
// Reading twice must therefore return the same object at the same revision.

function movedCondition(m) {
  const at = m.meta.generation;
  const to = m.spec.toNode;
  const instance = store.get(m.spec.instance);
  if (!instance) {
    return condition("Moved", "False", "NoSuchInstance",
      "the instance this migration names does not exist", at);
  }
  if (instance.status.node === to && instance.status.state === "Running") {
    return condition("Moved", "True", "Arrived", "running on " + to, at);
  }
  // Decided by time passing, which is why nothing announces it.
  const budget = m.spec.timeoutS || 3600;
  const age = Math.floor((now() - m.meta.createdAt) / 1000);
  if (age > budget) {
    const on = instance.status.node;
    // The one computed condition with a real moment behind it: it happened at
    // `createdAt + timeoutS`, which is arithmetic. Stamped, so two reads agree
    // and a client may show an age for this reason and no other — every other
    // `lastTransition` here is the moment of the read.
    return condition("Moved", "False", "Timeout",
      on ? "gave up after " + budget + "s; the guest is on " + on
        : "gave up after " + budget +
          "s; the handover was interrupted and no node holds the guest",
      at, m.meta.createdAt + budget * 1000);
  }
  if (!m.status.receiverReady) {
    return condition("Moved", "Unknown", "PreparingReceiver", to + " is not listening yet", at);
  }
  if (instance.status.node === null || instance.status.node === undefined) {
    return condition("Moved", "Unknown", "HandingOver",
      "the source has let go and the destination has not claimed it yet", at);
  }
  return condition("Moved", "Unknown", "Transferring",
    (m.status.transferredMib || 0) + " MiB copied to " + to, at);
}

/// The object as a client sees it. Never what is stored: the stored conditions
/// are only the ones somebody wrote.
///
/// This must be applied on **every** path an object leaves by — a read, a list,
/// the answer to a write, and a watch event — because the real API applies it on
/// all four, for the stated reason that a console which learns about an object
/// through a watch must see what a `GET` would have told it. A fake that
/// computed it on one path only would let the console be written against a world
/// where a freshly created migration has no `Moved` condition, which is a world
/// the real API never produces. That is exactly the drift this fake exists to
/// prevent, so it went wrong here once already.
function decorate(r) {
  if (!r || !/\/migrations\//.test(r.meta.name)) return r;
  return {
    ...r,
    status: {
      ...r.status,
      conditions: [
        ...(r.status.conditions || []).filter((c) => c.kind !== "Moved"),
        movedCondition(r),
      ],
    },
  };
}

// ---- the seed --------------------------------------------------------------
//
// Deliberately not all healthy. A console checked only against settled objects
// is a console whose whole reason for existing was never exercised.

function seed() {
  store.clear();
  put("projects/p1", { displayName: "Platform", parent: "organizations/o1",
    quota: { instances: 20, vcpus: 200, memoryMib: 524288, volumeGib: 4096 } },
    { observedGeneration: 1, conditions: ready(1),
      used: { instances: 3, vcpus: 10, memoryMib: 20480, volumeGib: 60 } });
  put("projects/p2", { displayName: "Sandbox", parent: "organizations/o1",
    quota: { instances: 5, vcpus: 20, memoryMib: 65536, volumeGib: 512 } },
    { observedGeneration: 1, conditions: ready(1),
      used: { instances: 0, vcpus: 0, memoryMib: 0, volumeGib: 0 } });

  put("nodes/node-a", { schedulable: true, labels: ["nvme", "gen4"] },
    { observedGeneration: 1, conditions: ready(1),
      capacity: { vcpus: 64, memoryMib: 262144, diskGib: 4096, numaFreeMib: [65536, 65536], hugepages1Gi: 32 },
      allocated: { vcpus: 10, memoryMib: 20480, diskGib: 200, numaFreeMib: [], hugepages1Gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 4000 });
  put("nodes/node-b", { schedulable: false, labels: ["nvme"] },
    { observedGeneration: 1, conditions: ready(1),
      capacity: { vcpus: 32, memoryMib: 65536, diskGib: 2048, numaFreeMib: [16384, 16384], hugepages1Gi: 0 },
      allocated: { vcpus: 30, memoryMib: 61440, diskGib: 1900, numaFreeMib: [], hugepages1Gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 900_000 });
  // Somewhere a guest can actually go. Without one, every migration answer is
  // "no", and a console checked only against that is a console whose picker was
  // never seen with anything in it.
  put("nodes/node-c", { schedulable: true, labels: ["nvme", "gen4"] },
    { observedGeneration: 1, conditions: ready(1),
      capacity: { vcpus: 64, memoryMib: 262144, diskGib: 4096, numaFreeMib: [65536, 65536], hugepages1Gi: 32 },
      allocated: { vcpus: 4, memoryMib: 8192, diskGib: 80, numaFreeMib: [], hugepages1Gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 3000 });

  put("projects/p1/images/debian-13", {
    digest: "sha256:" + "a".repeat(64), format: "Qcow2", sizeBytes: 1_181_116_006,
    sourceUrl: "https://images.invalid/debian-13.qcow2", signature: "MEUCIQ…" },
    { observedGeneration: 1, conditions: ready(1), cachedOn: ["node-a", "node-c"] });
  put("projects/p1/images/alpine-3", {
    digest: "sha256:" + "b".repeat(64), format: "Raw", sizeBytes: 62_914_560,
    sourceUrl: "https://images.invalid/alpine-3.raw", signature: null },
    { observedGeneration: 1, conditions: ready(1), cachedOn: [] });

  put("projects/p1/networks/prod", { vni: 4711, mtu: 9000 },
    { observedGeneration: 1, conditions: ready(1), programmedOn: ["node-a", "node-b"] });
  put("projects/p1/subnets/prod-a", { network: "projects/p1/networks/prod", cidr: "10.20.0.0/24",
    gateway: "10.20.0.1", dns: ["10.20.0.2"], reserved: ["10.20.0.5"] },
    { observedGeneration: 1, conditions: ready(1), allocated: 12, available: 241 });
  put("projects/p1/ports/web-1-eth0", { network: "projects/p1/networks/prod",
    subnet: "projects/p1/subnets/prod-a", address: "10.20.0.11", mac: "02:1a:4b:00:11:22",
    securityGroups: ["web"], rateLimitMbit: 1000 },
    { observedGeneration: 1, conditions: ready(1), node: "node-a", programmed: true, tapDevice: "tap0" });

  // Settled.
  put("projects/p1/instances/web-1", {
    vcpus: 4, memoryMib: 8192, image: "projects/p1/images/debian-13", rootDiskGib: 40,
    desiredState: "Running", ports: ["projects/p1/ports/web-1-eth0"], sshKeys: ["ssh-ed25519 AAAAC3…"],
    userData: null, node: "node-a", placementPolicy: { antiAffinityGroup: "web", requiredLabels: [] } },
    { observedGeneration: 2, conditions: ready(2), state: "Running", node: "node-a",
      addresses: ["10.20.0.11"], vmmPid: 4711, startedAt: now() - 7_200_000 },
    { generation: 2 });

  // Drifting: the ask moved and nothing has reported on it yet. The Ready
  // condition it carries was written about the older generation.
  put("projects/p1/instances/web-2", {
    vcpus: 8, memoryMib: 16384, image: "projects/p1/images/debian-13", rootDiskGib: 40,
    desiredState: "Running", ports: [], sshKeys: [], userData: null, node: "node-a",
    placementPolicy: { antiAffinityGroup: "web", requiredLabels: [] } },
    { observedGeneration: 2,
      conditions: [condition("Ready", "Unknown", "Converging",
        "the node has not reported on this change yet", 2)],
      state: "Running", node: "node-a", addresses: ["10.20.0.12"],
      vmmPid: 5120, startedAt: now() - 86_400_000 },
    { generation: 3 });

  // Converged on a spec it cannot satisfy: nothing is in flight, it is simply
  // not placed, and the reason is on the object.
  put("projects/p1/instances/db-1", {
    vcpus: 32, memoryMib: 131072, image: "projects/p1/images/debian-13", rootDiskGib: 500,
    desiredState: "Running", ports: [], sshKeys: [], userData: null, node: null,
    placementPolicy: { antiAffinityGroup: null, requiredLabels: ["nvme"] } },
    { observedGeneration: 1,
      conditions: [condition("Ready", "False", "NoCapacity",
        "no node in cell-1 has 131072 MiB free on one NUMA node", 1)],
      state: "Unknown", node: null, addresses: [], vmmPid: null, startedAt: null });

  // Settled, and nothing is moving it: the guest a migration can be started
  // from.
  put("projects/p1/instances/web-3", {
    vcpus: 2, memoryMib: 4096, image: "projects/p1/images/debian-13", rootDiskGib: 20,
    desiredState: "Running", ports: [], sshKeys: [], userData: null, node: "node-a",
    placementPolicy: { antiAffinityGroup: "web", requiredLabels: [] } },
    { observedGeneration: 1, conditions: ready(1), state: "Running", node: "node-a",
      addresses: ["10.20.0.13"], vmmPid: 6144, startedAt: now() - 3_600_000 });

  // Two migrations, deliberately at different moments of the same dance. One
  // where the destination has claimed the object and has not got a receiver up
  // yet — the moment a console is most tempted to show a spinner and call it
  // "migrating" — and one that is really sending.
  //
  // Neither stores a `Moved` condition, because nothing does: what each one
  // reports follows from `receiverReady`, where the instance is, and how old
  // the object is, and is worked out when it is read.
  put("projects/p1/migrations/web-2-to-node-c", {
    instance: "projects/p1/instances/web-2", fromNode: "node-a", toNode: "node-c",
    mode: "PostCopy", downtimeMs: 300, timeoutS: 3600, connections: 1 },
    { observedGeneration: 1, conditions: [],
      node: "node-c", receiverUrl: null, receiverReady: false, transferredMib: 0 },
    { createdAt: now() - 120_000 });
  put("projects/p1/migrations/web-3-to-node-c", {
    instance: "projects/p1/instances/web-3", fromNode: "node-a", toNode: "node-c",
    mode: "Live", downtimeMs: 300, timeoutS: 3600, connections: 1 },
    { observedGeneration: 1, conditions: [],
      node: "node-c", receiverUrl: "tcp:10.20.0.13:4711", receiverReady: true,
      transferredMib: 3072 },
    { createdAt: now() - 120_000 });

  // Finished, and still there: whether a migration is done is read from where
  // the instance actually runs, so an arrived one keeps saying so until somebody
  // removes the record. web-1 came from node-b and is on node-a.
  put("projects/p1/migrations/web-1-from-node-b", {
    instance: "projects/p1/instances/web-1", fromNode: "node-b", toNode: "node-a",
    mode: "Live", downtimeMs: 300, timeoutS: 3600, connections: 1 },
    { observedGeneration: 1, conditions: [],
      // The receiver was torn down when it arrived, which is why "arrived" can
      // never be read off `receiverReady` — and why `Arrived` has to beat the
      // timeout: this object is older than its own budget and the guest is
      // exactly where it was asked to be.
      node: "node-a", receiverUrl: null, receiverReady: false, transferredMib: 8192 });

  put("projects/p1/volumes/data-1", { sizeGib: 100, pool: "nvme",
    encryptionKey: "projects/p1/keys/data", sourceImage: null },
    { observedGeneration: 1, conditions: ready(1), provisioned: true, actualSizeGib: 100 });
  // Asked to grow, and the pool has not finished.
  put("projects/p1/volumes/data-2", { sizeGib: 200, pool: "nvme", encryptionKey: null, sourceImage: null },
    { observedGeneration: 2, conditions: ready(2), provisioned: true, actualSizeGib: 100 },
    { generation: 2 });

  put("projects/p1/attachments/data-1-web-1", { volume: "projects/p1/volumes/data-1",
    instance: "projects/p1/instances/web-1", node: "node-a", readOnly: false },
    { observedGeneration: 1, conditions: ready(1), attached: true, device: "/dev/vdb", node: "node-a" },
    { finalizers: ["node.velstra.io/release"] });

  put("projects/p1/operations/op-7", { target: "projects/p1/instances/web-2",
    targetGeneration: 3, verb: "update", requestedBy: "operator" },
    { observedGeneration: 1, conditions: ready(1), done: false, error: null, finishedAt: null });
}

// ---- the wire --------------------------------------------------------------

const fail = (res, status, code, message, field) => {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ error: { code, message, field: field || null } }));
};

const json = (res, status, body, headers = {}) => {
  res.writeHead(status, { "Content-Type": "application/json", ...headers });
  res.end(JSON.stringify(body));
};

/// `/api/v1/projects/p1/instances/i1` → the resource name it addresses.
function nameFrom(path) {
  const rest = path.replace(/^\/api\/v1\//, "");
  return rest.replace(/\/+$/, "");
}

const isCollectionName = (name) => name.split("/").length % 2 === 1;

function listUnder(name) {
  // `projects/p1/instances` selects every resource whose name is that plus one
  // id, which is what the store's prefix scan does.
  const prefix = name + "/";
  return [...store.values()].filter((r) =>
    r.meta.name.startsWith(prefix) && r.meta.name.slice(prefix.length).indexOf("/") < 0);
}

function announce(type, resource, name) {
  const frame = type === "DELETE"
    ? { type, name, revision: nextRevision() }
    : { type, resource: decorate(resource) };
  for (const w of watchers) {
    if (!w.prefix || (name || resource.meta.name).startsWith(w.prefix + "/")) {
      w.res.write("data: " + JSON.stringify(frame) + "\n\n");
    }
  }
}

function sameSpec(a, b) { return JSON.stringify(a) === JSON.stringify(b); }

// How each reference is spelled on the wire. A node is a bare id — that is what
// the scheduler writes into `spec.node` and what ownership is decided by, so a
// full name there assigns the object to a node that does not answer to it and
// nothing ever starts. Everything else is a full resource name, because
// something has to follow it. Refused at the door rather than normalised: a
// server that rewrites a field changes what an object says without being asked.
const SPELLING = {
  instances: { node: "id", image: "name", ports: "name" },
  attachments: { node: "id", volume: "name", instance: "name" },
  volumes: { sourceImage: "name" },
  ports: { network: "name", subnet: "name" },
  migrations: { instance: "name", fromNode: "id", toNode: "id" },
  subnets: { network: "name" },
  projects: { parent: "name" },
};

const isName = (v) => String(v).split("/").length % 2 === 0 && String(v).split("/").every(Boolean);

/// The offending field, and what to say about it, or null.
function misspelled(kind, spec) {
  for (const [key, want] of Object.entries(SPELLING[kind] || {})) {
    const value = spec[key];
    if (value === undefined || value === null || value === "") continue;   // unset is not wrong
    const each = Array.isArray(value) ? value : [value];
    for (let i = 0; i < each.length; i++) {
      const v = each[i];
      if (!v) continue;
      const at = "spec." + key + (Array.isArray(value) ? `[${i}]` : "");
      if (want === "id" && isName(v)) {
        return { field: at, message: "a node is named by its id — `" + String(v).split("/").pop() +
          "`, not `" + v + "`: that is the name the scheduler writes and the name an agent answers to" };
      }
      if (want === "name" && !isName(v)) {
        return { field: at, message: "`" + v + "` is not a resource name: give the whole thing, " +
          "because something has to follow it" };
      }
    }
  }
  return null;
}

const plain = (v) => v && typeof v === "object" && !Array.isArray(v);

function merge(into, patch) {
  const out = { ...into };
  for (const [k, v] of Object.entries(patch)) {
    out[k] = plain(v) && plain(out[k]) ? merge(out[k], v) : v;
  }
  return out;
}

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  if (!chunks.length) return null;
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); } catch (e) { return undefined; }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  const path = url.pathname;

  if (path === "/" || path === "/index.html") {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end(page);
    return;
  }

  // Scaffolding: an agent reporting, and a reset between tests.
  if (path === "/__test/converge") {
    const r = store.get(url.searchParams.get("name"));
    if (!r) return fail(res, 404, "NOT_FOUND", "no such object");
    r.status.observedGeneration = r.meta.generation;
    r.status.conditions = ready(r.meta.generation);
    if (r.status.state !== undefined) r.status.state = r.spec.desiredState || "Running";
    if (r.status.actualSizeGib !== undefined) r.status.actualSizeGib = r.spec.sizeGib;
    r.meta.revision = nextRevision();
    announce("PUT", r);
    return json(res, 200, decorate(r));
  }
  // Make an object older than it is, so a timeout arrives the way it really
  // does: out of the clock.
  //
  // Nothing is written to `status` and nothing is announced. The condition is
  // computed on read, so a migration passing its budget produces no write, no
  // new revision and no event — the one change in this contract a client can
  // only see by asking again. This endpoint is how a test can prove the console
  // asks.
  if (path === "/__test/age") {
    const r = store.get(url.searchParams.get("name"));
    if (!r) return fail(res, 404, "NOT_FOUND", "no such object");
    r.meta.createdAt -= Number(url.searchParams.get("seconds") || 0) * 1000;
    return json(res, 200, decorate(r));
  }
  if (path === "/__test/reset") { seed(); return json(res, 200, { ok: true }); }

  if (!path.startsWith("/api/v1/")) return fail(res, 404, "NOT_FOUND", "no such path");

  const auth = req.headers.authorization || "";
  if (auth !== "Bearer " + TOKEN) {
    return fail(res, 401, "UNAUTHENTICATED", "a bearer token is required");
  }

  // `…/i1:explainPlacement`
  if (path.includes(":explainPlacement")) {
    const name = nameFrom(path.replace(":explainPlacement", ""));
    const r = store.get(name);
    if (!r) return fail(res, 404, "NOT_FOUND", "no such object");
    if (r.spec.node) return json(res, 200, { placed: r.spec.node, rejected: [] });
    return json(res, 200, {
      placed: null,
      rejected: [
        { node: "node-a", why: "InsufficientMemory", detail: "45056 free, 131072 wanted" },
        { node: "node-b", why: "NotSchedulable", detail: "the node is draining" },
      ],
    });
  }

  // `…/i1:explainMigration` — every node in the cell with its own verdict.
  //
  // A GET, like its sibling `:explainPlacement`: it reads and creates nothing.
  // And *not* that verb's shape — placement answers with the one node the
  // scheduler picked and throws the rest away, so a candidate set cannot be
  // recovered from it. `may_migrate` is per destination, so this enumerates,
  // and a node absent from `destinations` does not exist rather than being
  // undecided.
  if (path.includes(":explainMigration")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const instance = store.get(nameFrom(path.replace(":explainMigration", "")));
    if (!instance) return fail(res, 404, "NOT_FOUND", "no such object");
    const destinations = [...store.values()]
      .filter((r) => r.meta.name.startsWith("nodes/"))
      .map((node) => {
        const id = node.meta.name.split("/").pop();
        const no = whyNot(instance, id);
        return no
          ? { node: id, allowed: false, why: no.why, detail: no.detail }
          : { node: id, allowed: true, why: "", detail: "" };
      });
    return json(res, 200, {
      from: instance.status.node || instance.spec.node || null,
      destinations,
    });
  }

  const name = nameFrom(path);

  if (req.method === "GET" && isCollectionName(name)) {
    const items = listUnder(name);
    if (url.searchParams.get("watch") === "true") {
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
      });
      const from = Number(url.searchParams.get("fromRevision") || 0);
      // Nothing between the list and the watch is lost: anything already newer
      // than the revision the client came with is replayed at once.
      for (const r of items) {
        if (Number(r.meta.revision) > from) {
          res.write("data: " + JSON.stringify({ type: "PUT", resource: decorate(r) }) + "\n\n");
        }
      }
      const w = { res, prefix: name };
      watchers.add(w);
      req.on("close", () => watchers.delete(w));
      return;
    }
    const newest = items.reduce((n, r) => Math.max(n, Number(r.meta.revision)), 0);
    return json(res, 200, { items: items.map(decorate) },
      { "X-Velstra-Revision": String(newest || clock) });
  }

  if (req.method === "GET") {
    const r = store.get(name);
    return r ? json(res, 200, decorate(r)) : fail(res, 404, "NOT_FOUND", "no such object");
  }

  if (req.method === "POST" && isCollectionName(name)) {
    const body = await readBody(req);
    if (!body || !body.id) return fail(res, 400, "INVALID_ARGUMENT", "an id is required", "id");
    if (!/^[a-z0-9][a-z0-9.-]*$/.test(body.id)) {
      return fail(res, 400, "INVALID_ARGUMENT", "an id may hold only a-z, 0-9, '-' and '.'", "id");
    }
    const full = name + "/" + body.id;
    if (store.get(full)) return fail(res, 409, "ALREADY_EXISTS", "that name is taken", "id");
    if (body.status) return fail(res, 400, "INVALID_ARGUMENT", "status is not writable", "status");
    const wrong = misspelled(name.split("/").pop(), body.spec || {});
    if (wrong) return fail(res, 400, "INVALID_ARGUMENT", wrong.message, wrong.field);
    const derived = derive(name.split("/").pop(), body.spec || {});
    if (derived.error) return fail(res, derived.status, derived.code, derived.error, derived.field);
    // Created unconverged, which is the honest state: nothing has looked at it.
    const r = put(full, derived.spec, {
      observedGeneration: 0,
      conditions: [condition("Ready", "Unknown", "Converging", "nothing has reported on this object yet", 0)],
      ...blankStatus(name),
    });
    announce("PUT", r);
    const op = put(name.split("/").slice(0, 2).join("/") + "/operations/op-" + clock,
      { target: full, targetGeneration: 1, verb: "create", requestedBy: "console" },
      { observedGeneration: 1, conditions: ready(1), done: false, error: null, finishedAt: null });
    announce("PUT", op);
    return json(res, 202, { operation: op.meta.name, target: full });
  }

  if (req.method === "PATCH") {
    const r = store.get(name);
    if (!r) return fail(res, 404, "NOT_FOUND", "no such object");
    const body = await readBody(req);
    if (body === undefined) return fail(res, 400, "INVALID_ARGUMENT", "the body is not JSON");
    if (body && body.status) return fail(res, 400, "INVALID_ARGUMENT", "status is not writable", "status");
    const ifMatch = req.headers["if-match"];
    if (ifMatch && ifMatch !== r.meta.revision) {
      return json(res, 409, { error: { code: "ABORTED", message: "the object moved since it was read" },
        revision: r.meta.revision });
    }
    // A patch names spec or meta. A bare spec object is refused: a body that is
    // sometimes the spec and sometimes the wrapper is a body nobody can
    // validate.
    if (!body || (body.spec === undefined && body.meta === undefined)) {
      return fail(res, 400, "INVALID_ARGUMENT", "a patch names spec or meta", "spec");
    }
    if (body.meta && Object.keys(body.meta).some((k) => k !== "labels")) {
      return fail(res, 400, "INVALID_ARGUMENT", "only meta.labels is writable", "meta");
    }
    // Objects merge, arrays replace whole (a port list has an order), null
    // clears an optional field.
    const next = body.spec === undefined ? r.spec : merge(r.spec, body.spec);
    const wrong = misspelled(name.split("/").slice(-2)[0], next);
    if (wrong) return fail(res, 400, "INVALID_ARGUMENT", wrong.message, wrong.field);
    const moved = !sameSpec(next, r.spec);
    if (body.meta) r.meta.labels = { ...r.meta.labels, ...body.meta.labels };
    // The generation moves if and only if the spec really changed, and an
    // identical PATCH is not a write at all — same generation, same revision.
    if (!moved && !body.meta) return json(res, 200, decorate(r));
    if (moved) { r.spec = next; r.meta.generation += 1; }
    r.meta.revision = nextRevision();
    announce("PUT", r);
    return json(res, 200, decorate(r));
  }

  if (req.method === "DELETE") {
    const r = store.get(name);
    if (!r) return fail(res, 404, "NOT_FOUND", "no such object");
    const ifMatch = req.headers["if-match"];
    if (ifMatch && ifMatch !== r.meta.revision) {
      return json(res, 409, { error: { code: "ABORTED", message: "the object moved since it was read" },
        revision: r.meta.revision });
    }
    // Two-phase and visible: it stays listable, with its finalizers, until they
    // are released. A client that wants "gone" waits for a 404.
    if (r.meta.finalizers.length) {
      r.meta.deletedAt = now();
      r.meta.revision = nextRevision();
      announce("PUT", r);
      return json(res, 202, decorate(r));
    }
    store.delete(name);
    announce("DELETE", null, name);
    return json(res, 202, decorate({ ...r, meta: { ...r.meta, deletedAt: now() } }));
  }

  return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
});

/// A field the platform already knows is not asked for twice.
///
/// An attachment's node comes from the instance: an attachment whose node is not
/// the instance's is a meaningless object, because the node it names does not
/// have the guest and the node that does is not watching for it. Derived at
/// create, that state cannot be written down at all.
/// Why this guest cannot be received there, or null.
///
/// A pure function of what has already been reported — capacity from the
/// destination's own report, the image from where it is cached — so it can be
/// answered at the moment somebody clicks rather than after a transfer starts.
function whyNot(instance, toId) {
  const to = store.get("nodes/" + toId);
  const from = instance.status.node || instance.spec.node || null;
  if (!to) return { why: "NoSuchNode", detail: "there is no node called " + toId };
  if (toId === from) return { why: "AlreadyThere", detail: "it is already on " + toId };
  if (instance.spec.desiredState === "Running" && instance.status.state !== "Running") {
    return { why: "NotRunning", detail: "it is not running: " + instance.status.state };
  }
  if (!to.spec.schedulable) return { why: "DestinationDraining", detail: toId + " is not accepting work" };
  const free = to.status.capacity.memoryMib - to.status.allocated.memoryMib;
  if (free < instance.spec.memoryMib) {
    // The model's own sentence, word for word — this server exists to be the
    // contract, not an approximation of it.
    return { why: "DestinationTooSmall",
      detail: toId + " has " + free + " MiB free, it needs " + instance.spec.memoryMib + " MiB" };
  }
  const image = store.get(instance.spec.image);
  if (image && !(image.status.cachedOn || []).includes(toId)) {
    return { why: "DestinationLacksImage",
      detail: toId + " does not have " + instance.spec.image };
  }
  return null;
}

function derive(kind, spec) {
  // A migration's source is where the guest is. Stated it must agree; omitted
  // it is copied, exactly as an attachment's node is — a migration that claims
  // to start somewhere the guest is not would have the source watching for work
  // it will never see.
  if (kind === "migrations") {
    const instance = store.get(spec.instance);
    if (!instance) {
      return { status: 400, code: "INVALID_ARGUMENT", field: "spec.instance",
        error: "there is no instance called " + (spec.instance || "(none)") };
    }
    const from = instance.status.node || instance.spec.node;
    if (!from) {
      return { status: 400, code: "FAILED_PRECONDITION", field: "spec.instance",
        error: instance.meta.name.split("/").pop() +
          " is not on a node, so there is nothing to move" };
    }
    if (spec.fromNode && spec.fromNode !== from) {
      return { status: 400, code: "INVALID_ARGUMENT", field: "spec.fromNode",
        error: "the guest is on " + from + ", not " + spec.fromNode +
          " — the source is taken from the instance and cannot disagree with it" };
    }
    // Two different problems, two different controls: a destination that
    // cannot receive is the destination's fault, a guest that is not running
    // or is not where you said is the guest's.
    const no = whyNot(instance, spec.toNode);
    if (no) {
      const guest = no.why === "NotRunning" || no.why === "NotFromThere";
      return { status: 400, code: "FAILED_PRECONDITION",
        field: guest ? "spec.instance" : "spec.toNode", error: no.detail };
    }
    return { spec: { ...spec, fromNode: from } };
  }
  if (kind !== "attachments") return { spec };
  const instance = store.get(spec.instance);
  if (!instance) {
    return { status: 400, code: "INVALID_ARGUMENT", field: "spec.instance",
      error: "there is no instance called " + (spec.instance || "(none)") };
  }
  const node = instance.spec.node;
  if (!node) {
    return { status: 400, code: "FAILED_PRECONDITION", field: "spec.node",
      error: instance.meta.name.split("/").pop() +
        " has not been placed on a node yet, so there is no node to open the volume on" };
  }
  if (spec.node && spec.node !== node) {
    return { status: 400, code: "INVALID_ARGUMENT", field: "spec.node",
      error: "the instance is on " + node + ", not " + spec.node +
        " — the node is taken from the instance and cannot disagree with it" };
  }
  return { spec: { ...spec, node } };
}

/// What a freshly created object's status looks like before anybody reports:
/// the fields exist and say nothing, which is what `Unknown` means.
function blankStatus(collectionName) {
  const kind = collectionName.split("/").pop();
  if (kind === "instances") return { state: "Unknown", node: null, addresses: [], vmmPid: null, startedAt: null };
  if (kind === "volumes") return { provisioned: false, actualSizeGib: 0 };
  if (kind === "attachments") return { attached: false, device: null, node: null };
  if (kind === "networks") return { programmedOn: [] };
  if (kind === "subnets") return { allocated: 0, available: 0 };
  if (kind === "ports") return { node: null, programmed: false, tapDevice: null };
  if (kind === "images") return { cachedOn: [] };
  // Nothing is listening and nothing has been sent — which is what a migration
  // looks like before the destination has done anything about it.
  if (kind === "migrations") {
    return { node: null, receiverUrl: null, receiverReady: false, transferredMib: 0 };
  }
  if (kind === "projects") return { used: { instances: 0, vcpus: 0, memoryMib: 0, volumeGib: 0 } };
  return {};
}

seed();
const port = Number(process.env.FAKE_PORT || 0);
server.listen(port, "127.0.0.1", () => {
  // The port is announced rather than fixed: a run left behind by an
  // interrupted test must not make the next one attach to a ghost.
  process.stdout.write("listening " + server.address().port + "\n");
});
