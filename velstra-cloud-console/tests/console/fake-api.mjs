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
const USERNAME = process.env.CONSOLE_USER || "operator";
const PASSWORD = process.env.CONSOLE_PASSWORD || "a test operator passphrase";
const page = PAGE ? readFileSync(PAGE, "utf8") : "<!doctype html><p>no page";

let clock = 400;
const nextRevision = () => String(++clock);
const now = () => Date.now();

// name -> resource. One flat map: the name carries the project, exactly as the
// store's keys do.
const store = new Map();
const watchers = new Set();

/// What every object of a kind carries before anybody sets anything.
///
/// The real API builds a document the same way — a default spec, with the
/// caller's fields merged onto it — so a fixture that omitted the untouched
/// fields answered a shape the API never answers, and the console was tested
/// against objects thinner than the ones it will meet. Kept here rather than
/// spelled into every literal below, because the failure mode is a field added
/// to the model and to seven fixtures out of eight.
const DEFAULTS = {
  instances: {
    vcpus: 0, memoryMib: 0, image: "", rootDiskGib: 0, desiredState: "Running",
    // No `devices` and no `affinityGroup`: the API leaves out what is empty,
    // and a fixture that answers a key the API omits is the same drift one
    // direction over.
    ports: [], sshKeys: [], userData: null, node: null,
    console: false, onNodeLoss: "leave", startOrder: 0, startDelayS: 0,
    placementPolicy: {
      antiAffinityGroup: null, requiredLabels: [],
      spread: "Required", affinity: "Required",
    },
  },
  nodes: {
    schedulable: true, evacuate: false, labels: [], fenceAfterS: 0,
    vcpuOvercommit: 0, gateway: false,
  },
  networks: { vni: 0, mtu: 1450, external: false, announce: "FromGateway" },
  floatingips: { subnet: "", port: "", delivery: "Nat" },
};

const STATUS_DEFAULTS = {
  instances: { consoleBytes: 0, addresses: [] },
  nodes: { images: [], devices: [] },
};

/// Which collection a name is in: `projects/p1/instances/web-1` → `instances`.
const kindOf = (name) => name.split("/").slice(-2)[0];

function withDefaults(table, name, given) {
  const base = table[kindOf(name)];
  if (!base) return given;
  const out = { ...base, ...given };
  // One level in, so a nested object with one field set does not lose the rest
  // — `placementPolicy` was exactly that.
  for (const [key, value] of Object.entries(base)) {
    if (value && typeof value === "object" && !Array.isArray(value) && given[key]) {
      out[key] = { ...value, ...given[key] };
    }
  }
  return out;
}

function put(name, spec, status, extra = {}) {
  spec = withDefaults(DEFAULTS, name, spec);
  status = withDefaults(STATUS_DEFAULTS, name, status);
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
/// Every port in the store, which is what the three computed fields below are
/// computed from.
const allPorts = () => [...store.values()].filter((r) => /\/ports\//.test(r.meta.name));

/// How many addresses this subnet has given out, and how many are left —
/// `velstra_cloud_model::ipam::counts`, which the API runs on the way out.
///
/// Counted from the ports, never stored. A port created or deleted changes these
/// two numbers with **nothing written to the subnet**, which is the whole point
/// of computing them on read and is also what makes them the hardest thing on
/// this contract for a client to get right: there is no event to tell anybody.
function subnetCounts(subnet) {
  const cidr = String(subnet.spec.cidr || "");
  const [, bits] = cidr.split("/");
  const prefix = Number(bits);
  if (!cidr.includes("/") || !Number.isFinite(prefix)) return [0, 0];
  const v6 = cidr.includes(":");
  const usable = v6
    ? (prefix === 128 ? 0 : prefix >= 65 ? 2 ** (128 - prefix) - 1 : Number.MAX_SAFE_INTEGER)
    : (prefix >= 31 ? 0 : 2 ** (32 - prefix) - 2);

  const name = subnet.meta.name;
  const allocated = allPorts()
    .filter((p) => p.spec.subnet === name && p.spec.address).length;
  // The gateway and anything explicitly reserved are not available even though
  // nothing holds them. A set, because a gateway that is also listed as reserved
  // is one address, not two.
  const reserved = new Set([...(subnet.spec.reserved || []), subnet.spec.gateway].filter(Boolean));
  return [allocated, Math.max(0, usable - allocated - reserved.size)];
}

/// The addresses in a security group, as the ports say it —
/// `velstra_cloud_model::security::members_in`.
const groupMembers = (group) => allPorts()
  .filter((p) => (p.spec.securityGroups || []).some((g) => g === group.meta.name))
  .map((p) => p.spec.address)
  .filter(Boolean);

/// What the API adds on the way out, and only on the way out.
///
/// Three fields, and every one of them is an aggregate over *other* objects that
/// no writer owns — so none of them is ever written, and none of them produces a
/// watch event on the object carrying it. This used to compute only the
/// migration's condition, which made the other two static: a subnet's occupancy
/// and a group's membership never moved however many ports a test created, so a
/// console that read them once and never asked again passed, and a console that
/// re-read them was doing work no test could see the point of. That is the fake
/// deciding, by omission, that a whole class of contract behaviour does not
/// exist.
function decorate(r) {
  if (!r) return r;
  if (/\/migrations\//.test(r.meta.name)) {
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
  if (/\/subnets\//.test(r.meta.name)) {
    const [allocated, available] = subnetCounts(r);
    return { ...r, status: { ...r.status, allocated, available } };
  }
  if (/\/security-groups\//.test(r.meta.name)) {
    return { ...r, status: { ...r.status, members: groupMembers(r) } };
  }
  // A disk a Ceph cluster was given stops reporting as free — it is an OSD now,
  // which is what the node would say and what `may_consume` refuses. Modelled
  // here because it is the trap the console's disk picker has to survive: a
  // control that believed the refusal would render every disk of a working
  // cluster as unavailable, with no way left to take one back.
  if (/^nodes\//.test(r.meta.name)) {
    const node = r.meta.name.split("/").pop();
    const claimed = new Map();
    for (const c of store.values()) {
      if (!/^ceph-clusters\//.test(c.meta.name)) continue;
      (c.spec.osds || []).forEach((o, i) => claimed.set(o.node + " " + o.device, String(i)));
    }
    if (!claimed.size) return r;
    return { ...r, status: { ...r.status, devices: (r.status.devices || []).map((d) => {
      const osd = claimed.get(node + " " + d.path);
      return osd === undefined ? d : { ...d, state: { kind: "Osd", id: osd } };
    }) } };
  }
  return r;
}

// ---- the seed --------------------------------------------------------------
//
// Deliberately not all healthy. A console checked only against settled objects
// is a console whose whole reason for existing was never exercised.

// What a node reports about its own disks, and deliberately not all of it
// usable. The console's disk picker exists to say *why* a disk cannot be had, so
// a seed of nothing but empty disks would exercise the one half of it that never
// goes wrong.
const disk = (path, kernelName, sizeGib, rotational, model, state) =>
  ({ path, kernelName, sizeGib, rotational, model, serial: "S" + kernelName, state });


/// A processor, as a node reports one.
///
/// The fixture's machines are deliberately *not* identical: node-b is a
/// generation behind. A console checked only against a uniform cell is a
/// console whose CPU strip was never seen with anything in it, and the whole
/// point of that strip is the mixed case.
function cpu(level, extra = []) {
  const v2 = ["cx16", "lahf_lm", "popcnt", "sse3", "sse4_1", "sse4_2", "ssse3"];
  const v3 = [...v2, "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe"];
  const flags = [...(level === "v3" ? v3 : v2), ...extra].sort();
  return {
    arch: "x86_64",
    vendor: "GenuineIntel",
    modelName: level === "v3" ? "Intel(R) Xeon(R) Gold 6248R" : "Intel(R) Xeon(R) E5-2670",
    family: 6, model: 85, stepping: 7,
    flags,
    presents: "host",
    presentedFlags: flags,
    canMask: true,
  };
}

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
      capacity: { vcpus: 64, memoryMib: 262144, diskGib: 4096, numaFreeMib: [65536, 65536], hugepages1gi: 32 },
      allocated: { vcpus: 10, memoryMib: 20480, diskGib: 200, numaFreeMib: [], hugepages1gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 4000,
      cpu: cpu("v3"),
      devices: [
        disk("/dev/disk/by-id/nvme-eui.0001", "nvme0n1", 931, false, "Samsung SSD 990", { kind: "Free" }),
        disk("/dev/disk/by-id/ata-WDC-0002", "sda", 3726, true, "WDC WD40EFRX", { kind: "Filesystem", fstype: "ext4" }),
        disk("/dev/disk/by-id/nvme-eui.0003", "nvme1n1", 16, false, "Kingston NV2", { kind: "Free" }),
        disk("/dev/disk/by-id/ata-INTEL-0004", "sdb", 240, false, "INTEL SSDSC2", { kind: "System" }),
        disk("/dev/disk/by-id/ata-SEAGATE-0005", "sdc", 1863, true, "ST2000DM008", { kind: "Mounted", at: "/var/lib/velstra" }),
      ] });
  put("nodes/node-b", { schedulable: false, labels: ["nvme"] },
    { observedGeneration: 1, conditions: ready(1),
      capacity: { vcpus: 32, memoryMib: 65536, diskGib: 2048, numaFreeMib: [16384, 16384], hugepages1gi: 0 },
      allocated: { vcpus: 30, memoryMib: 61440, diskGib: 1900, numaFreeMib: [], hugepages1gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 900_000,
      // A generation behind the other two: this is what makes the cell mixed.
      cpu: cpu("v2"),
      devices: [
        disk("/dev/disk/by-id/ata-CRUCIAL-0006", "sda", 894, false, "CT1000MX500", { kind: "Partitioned", partitions: 3 }),
      ] });
  // Somewhere a guest can actually go. Without one, every migration answer is
  // "no", and a console checked only against that is a console whose picker was
  // never seen with anything in it.
  put("nodes/node-c", { schedulable: true, labels: ["nvme", "gen4"], vcpuOvercommit: 4 },
    { observedGeneration: 1, conditions: ready(1),
      capacity: { vcpus: 64, memoryMib: 262144, diskGib: 4096, numaFreeMib: [65536, 65536], hugepages1gi: 32 },
      allocated: { vcpus: 4, memoryMib: 8192, diskGib: 80, numaFreeMib: [], hugepages1gi: 0 },
      agentVersion: "0.1.0", lastHeartbeat: now() - 3000,
      cpu: cpu("v3"),
      devices: [
        disk("/dev/disk/by-id/nvme-eui.0007", "nvme0n1", 1863, false, "WD Black SN850X", { kind: "Free" }),
        disk("/dev/disk/by-id/ata-HGST-0008", "sda", 7452, true, "HUH721010ALE600", { kind: "Osd", id: "7" }),
      ] });

  // Two windows: one open over node-b right now, one still to come on node-c.
  // Both are needed — the open one is what an operator is looking at, and the
  // upcoming one is the thing they would otherwise be surprised by.
  put("maintenance-windows/dimm-swap", {
    node: "node-b", startsAt: now() - 20 * 60_000, minutes: 60, drain: false,
    note: "swapping the failed DIMM in slot 3" },
    { observedGeneration: 1, conditions: [] });
  put("maintenance-windows/rack-move", {
    node: "node-c", startsAt: now() + 3 * 60 * 60_000, minutes: 120, drain: true,
    note: "moving it to rack 4" },
    { observedGeneration: 1, conditions: [] });

  // No `signature` on either: the API refuses one, because nothing in the
  // platform verifies it. A fixture carrying a field the real API would reject
  // is the drift this file exists to prevent — the console would be built
  // against a shape that cannot exist.
  put("projects/p1/images/debian-13", {
    digest: "sha256:" + "a".repeat(64), format: "Qcow2", sizeBytes: 1_181_116_006,
    sourceUrl: "https://images.invalid/debian-13.qcow2" },
    { observedGeneration: 1, conditions: ready(1), cachedOn: ["node-a", "node-c"] });
  put("projects/p1/images/alpine-3", {
    digest: "sha256:" + "b".repeat(64), format: "Raw", sizeBytes: 62_914_560,
    sourceUrl: "https://images.invalid/alpine-3.raw" },
    { observedGeneration: 1, conditions: ready(1), cachedOn: [] });

  put("projects/p1/networks/prod", { vni: 4711, mtu: 9000 },
    { observedGeneration: 1, conditions: ready(1), programmedOn: ["node-a", "node-b"] });
  put("projects/p1/subnets/prod-a", { network: "projects/p1/networks/prod", cidr: "10.20.0.0/24",
    gateway: "10.20.0.1", dns: ["10.20.0.2"], reserved: ["10.20.0.5"] },
    { observedGeneration: 1, conditions: ready(1), allocated: 12, available: 241 });
  // One address in front of the web pool. Settled: the fabric serves the
  // address that was asked for, and the observed listeners carry the pool.
  put("projects/p1/load-balancers/web", {
    network: "projects/p1/networks/prod", subnet: "projects/p1/subnets/prod-a",
    vip: "10.20.0.20",
    listeners: [{ protocol: "Tcp", port: 443, memberPort: 8080 }],
    members: ["projects/p1/ports/web-1-eth0"] },
    { observedGeneration: 1, conditions: ready(1), vip: "10.20.0.20",
      listeners: [{ protocol: "Tcp", port: 443, members: 1 }] });

  put("projects/p1/ports/web-1-eth0", { network: "projects/p1/networks/prod",
    subnet: "projects/p1/subnets/prod-a", address: "10.20.0.11", mac: "02:1a:4b:00:11:22",
    // A full resource name, which is how the platform spells a reference to
    // anything but a node — the API refuses a bare id at the door. The group it
    // names does not exist, and that is a real state rather than an oversight: a
    // port whose group has been deleted keeps working with fewer allowances.
    securityGroups: ["projects/p1/security-groups/web"], rateLimitMbit: 1000 },
    { observedGeneration: 1, conditions: ready(1), node: "node-a", programmed: true, tapDevice: "tap0" });

  // Settled.
  put("projects/p1/instances/web-1", {
    vcpus: 4, memoryMib: 8192, image: "projects/p1/images/debian-13", rootDiskGib: 40,
    desiredState: "Running", ports: ["projects/p1/ports/web-1-eth0"], sshKeys: ["ssh-ed25519 AAAAC3…"],
    userData: null, node: "node-a", placementPolicy: { antiAffinityGroup: "web", requiredLabels: [] } },
    { observedGeneration: 2, conditions: ready(2), state: "Running", node: "node-a",
      addresses: ["10.20.0.11"], vmmPid: 4711, startedAt: now() - 7_200_000 },
    { generation: 2, labels: { env: "prod", tier: "web" } });

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
    { generation: 3, labels: { env: "staging", tier: "web" } });

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

  // A guest that will not boot, with its last words attached. This is the case
  // the console capture exists for: a failed guest is the one with the most to
  // say and the least chance of being heard, and a console that showed only
  // "Failed" would be the reason somebody ssh'd into a hypervisor.
  //
  // Named to sort *after* the healthy guests, and that is not cosmetic. Several
  // migration checks take "the first placed guest" off the board; a broken one
  // at the front of the list is refused by every destination, which reads to
  // those checks as "this cell can receive nothing" and skips them silently.
  // One of them now asks for a *running* guest instead, which is the real fix;
  // the name keeps the others honest until they do the same.
  put("projects/p1/instances/web-9", {
    vcpus: 4, memoryMib: 8192, image: "projects/p1/images/debian-13", rootDiskGib: 40,
    desiredState: "Running", ports: [], sshKeys: [], userData: null, node: "node-a",
    console: false,
    placementPolicy: { antiAffinityGroup: null, requiredLabels: [] } },
    { observedGeneration: 1,
      conditions: [condition("Ready", "False", "HostActions",
        "the guest exited without being asked to", 1)],
      state: "Failed", node: "node-a", addresses: [], vmmPid: null,
      startedAt: now() - 120_000,
      // Not switched on, and shown anyway — the second half of the rule.
      consoleTail:
        "[    0.000000] Linux version 6.12.63 (velstra@build)\n" +
        "[    1.204411] EXT4-fs (vda1): mounted filesystem\n" +
        "[    2.881900] systemd[1]: Starting Journal Service...\n" +
        "[    3.104222] EXT4-fs error (device vda1): ext4_lookup:1855: inode #131074\n" +
        "[    3.104980] Kernel panic - not syncing: Attempted to kill init!\n",
      consoleBytes: 240_128 });

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
  // Two more about web-1, so one object has a history worth reading: a change
  // that landed, and one that did not.
  put("projects/p1/operations/op-3", { target: "projects/p1/instances/web-1",
    targetGeneration: 1, verb: "create", requestedBy: "alice" },
    { observedGeneration: 1, conditions: ready(1), done: true, error: null,
      finishedAt: now() - 7_100_000 });
  put("projects/p1/operations/op-5", { target: "projects/p1/instances/web-1",
    targetGeneration: 2, verb: "update", requestedBy: "alice" },
    { observedGeneration: 1, conditions: ready(1), done: true,
      error: "the node refused the change", finishedAt: now() - 3_000_000 });
  // And a refusal, which lives where a tenant never looks — reading only the
  // accepted half is how somebody concludes their click did nothing.
  put("audit/rec-1", { kind: "Refused", subject: "bob", verb: "delete",
    target: "projects/p1/instances/web-1",
    // `detail`, and the same sentence the person was given: a line that
    // paraphrases the refusal is one an operator has to correlate by hand
    // against what they actually saw.
    detail: "bob is a viewer on projects/p1", at: now() - 600_000 },
    { observedGeneration: 1, conditions: [] });
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

/// Apply a label selector, exactly as the model's `labels_match` does.
function narrow(items, selector) {
  const terms = String(selector || "")
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean)
    .map((t) => {
      const at = t.indexOf("=");
      return at < 0
        ? { key: t, value: null }
        : { key: t.slice(0, at).trim(), value: t.slice(at + 1).trim() };
    });
  if (!terms.length) return items;
  return items.filter((r) => {
    const labels = (r.meta && r.meta.labels) || {};
    return terms.every((t) =>
      t.value === null
        ? Object.prototype.hasOwnProperty.call(labels, t.key)
        : labels[t.key] === t.value);
  });
}

function listUnder(name) {
  // `projects/p1/instances` selects every resource whose name is that plus one
  // id, which is what the store's prefix scan does.
  const prefix = name + "/";
  return [...store.values()]
    .filter((r) =>
      r.meta.name.startsWith(prefix) && r.meta.name.slice(prefix.length).indexOf("/") < 0)
    // Name order, because that is what the store's key order *is* and it is what
    // makes a page token work at all: a token carries the last name delivered
    // and the next page resumes strictly after it. Served in insertion order —
    // which is what a `Map` gives and what this returned before — resuming
    // "after" a name means nothing, and a walk would repeat some objects and
    // skip others while looking perfectly well-behaved.
    .sort((a, b) => (a.meta.name < b.meta.name ? -1 : a.meta.name > b.meta.name ? 1 : 0));
}

// ---- paging ----------------------------------------------------------------
//
// `velstra-cloud-api/src/paging.rs`, in the shape a client sees it. This is the
// half the console cannot be tested without: a fake that always answers with a
// whole collection is a fake that no client can fail against, so the console's
// paging would be untested by construction and every test would pass against a
// server that cannot behave like the real one.
//
// Three things here are the contract rather than convenience, and each is a way
// a plausible implementation goes wrong:
//
//  * **`nextPageToken` is absent, not empty, when the walk is done** — and it is
//    absent when a collection ends exactly on a page boundary. Emitting one
//    because the last page was full costs every client one pointless round trip
//    per walk, for ever, and no test that only counts objects would notice.
//  * **Every page reports the revision the *first* page was read at**, carried
//    in the token. Report each page's own and a client that lists to the end and
//    then watches from what it was given silently misses everything that
//    happened during the walk — the exact failure the list-then-watch order
//    exists to prevent.
//  * **A token is checked against the collection and parent it was minted for.**
//    Presented against another it does not mean "start there"; it means the
//    caller has two walks confused.

const DEFAULT_PAGE_SIZE = 100;
const MAX_PAGE_SIZE = 1000;

/// This server's ceiling on a page, lowered by `/__test/pagesize` so a suite can
/// prove a client walks a multi-page list without seeding a thousand objects.
///
/// A ceiling and not a forced page size: an unpaged request is still answered
/// whole, exactly as the real API answers one. What this models is a server
/// whose maximum is smaller than what the client asked for — which is what the
/// real API is to any client asking for more than a thousand, and the case a
/// client is most likely to get wrong because it believes its own number.
let pageCeiling = MAX_PAGE_SIZE;

/// Report each page's *own* revision instead of the walk's — a server getting
/// the rule wrong, on purpose.
///
/// It exists because a client's own correctness here is otherwise untestable. A
/// faithful server repeats the first page's revision on every page, so a client
/// that keeps the first and one that keeps the last read the same value and no
/// test can tell them apart. Pointing the client at a server that gets it wrong
/// is the only way to ask whether the client is doing the conservative thing —
/// and the conservative thing is what it must do, because it cannot know which
/// kind of server it is talking to.
let ownRevisionPerPage = false;

const SEP = "\u001f";

function encodeToken(t) {
  return Buffer.from([t.kind, t.parent, t.revision, t.after].join(SEP), "utf8")
    .toString("base64url");
}

function decodeToken(raw) {
  let plain;
  try { plain = Buffer.from(raw, "base64url").toString("utf8"); } catch (e) { return null; }
  const parts = plain.split(SEP);
  if (parts.length !== 4) return null;
  const [kind, parent, revision, after] = parts;
  if (!/^\d+$/.test(revision)) return null;
  return { kind, parent, revision, after };
}

/// Split a collection name into the parent and the collection, the way a token
/// records them: `projects/p1/instances` → `["projects/p1", "instances"]`, and
/// `nodes` → `["", "nodes"]`.
function splitCollection(name) {
  const cut = name.lastIndexOf("/");
  return cut < 0 ? ["", name] : [name.slice(0, cut), name.slice(cut + 1)];
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
  // Lower this server's page ceiling, so a client's walk can be exercised over
  // a handful of objects instead of a thousand. `0` puts it back.
  // See `ownRevisionPerPage`.
  if (path === "/__test/pagerevision") {
    ownRevisionPerPage = url.searchParams.get("own") === "1";
    return json(res, 200, { ownRevisionPerPage });
  }
  if (path === "/__test/pagesize") {
    const n = Number(url.searchParams.get("n") || 0);
    pageCeiling = n > 0 ? n : MAX_PAGE_SIZE;
    return json(res, 200, { pageCeiling });
  }
  if (path === "/__test/reset") {
    pageCeiling = MAX_PAGE_SIZE;
    ownRevisionPerPage = false;
    seed();
    return json(res, 200, { ok: true });
  }

  if (!path.startsWith("/api/v1/")) return fail(res, 404, "NOT_FOUND", "no such path");

  // Signing in is the one route that must answer without a token: it is what
  // issues one. Modelled here as the real API does it, including the single
  // refusal message for every cause — a fake that distinguished them would let
  // the console grow a screen that shows the difference.
  if (path === "/api/v1/sessions" && req.method === "POST") {
    const body = await readBody(req);
    const ok = body && body.username === USERNAME && body.password === PASSWORD;
    if (!ok) {
      return fail(res, 401, "UNAUTHENTICATED",
        "that username and password were not accepted");
    }
    return json(res, 201, {
      token: TOKEN,
      subject: USERNAME,
      displayName: "Test Operator",
      cellAdmin: true,
      expiresAt: Date.now() + 8 * 3600 * 1000,
    });
  }

  const auth = req.headers.authorization || "";
  if (auth !== "Bearer " + TOKEN) {
    return fail(res, 401, "UNAUTHENTICATED", "a bearer token is required");
  }

  if (path === "/api/v1/sessions/current") {
    if (req.method === "DELETE") {
      res.writeHead(204);
      res.end();
      return;
    }
    return json(res, 200, {
      subject: USERNAME,
      displayName: "Test Operator",
      cellAdmin: true,
      session: true,
    });
  }

  // Setting a password touches no collection, so it is answered here rather
  // than falling through to the object routes and 404ing.
  //
  // The one caller this fake authenticates is the operator, who may set any
  // account's password — but a self-service change must prove the *current*
  // one, exactly as the real API does, so a console that forgot to send it
  // would fail here too rather than passing against a laxer fake.
  const passwordRoute = path.match(/^\/api\/v1\/users\/([^/]+)\/password$/);
  if (passwordRoute && req.method === "PUT") {
    const id = decodeURIComponent(passwordRoute[1]);
    const body = await readBody(req);
    const own = id === USERNAME;
    if (own && String((body && body.currentPassword) || "") !== PASSWORD) {
      return fail(res, 403, "PERMISSION_DENIED", "the current password was not correct");
    }
    if (!body || String(body.password || "").length < 12) {
      return fail(res, 400, "INVALID_ARGUMENT", "a password must be at least 12 characters");
    }
    res.writeHead(204);
    res.end();
    return;
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

  // `projects/p1:explainQuota` — what is left, and what could actually start.
  // Both halves, because either alone answers the wrong question.
  if (path.includes(":explainQuota")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const project = store.get(nameFrom(path.replace(":explainQuota", "")));
    if (!project) return fail(res, 404, "NOT_FOUND", "no such object");
    const limit = project.spec.quota || {};
    const used = project.status.used || {};
    const names = ["instances", "vcpus", "memoryMib", "volumes", "volumeGib",
      "floatingIps", "loadBalancers", "devices"];
    const dimensions = names.map((name) => {
      const cap = Number(limit[name] || 0);
      const now = Number(used[name] || 0);
      // A zero is a limit nobody set, not a limit of nothing — the same
      // convention the quota checker itself follows.
      const unlimited = cap === 0;
      return {
        name, limit: cap, used: now, unlimited,
        left: unlimited ? null : Math.max(0, cap - now),
        exhausted: !unlimited && now >= cap,
      };
    });

    // The cell's side: the largest single machine, never a sum. Free memory
    // does not add up into a guest.
    const at = Date.now();
    const shut = new Set([...store.values()]
      .filter((r) => r.meta.name.startsWith("maintenance-windows/") &&
        r.spec.startsAt <= at && at < r.spec.startsAt + r.spec.minutes * 60_000)
      .map((r) => r.spec.node));
    let fitV = 0, fitM = 0;
    for (const n of [...store.values()].filter((r) => r.meta.name.startsWith("nodes/"))) {
      if (!n.spec.schedulable || n.spec.evacuate) continue;
      if (shut.has(n.meta.name.split("/").pop())) continue;
      // Defensive as well, because a reader that adds machines up must not be
      // the thing that falls over when one of them has said nothing yet.
      fitV = Math.max(fitV, (n.status.capacity?.vcpus || 0) - (n.status.allocated?.vcpus || 0));
      fitM = Math.max(
        fitM,
        (n.status.capacity?.memoryMib || 0) - (n.status.allocated?.memoryMib || 0),
      );
    }
    const left = (name) => (dimensions.find((d) => d.name === name) || {}).left;
    const pick = (q, cell) => q === null || q === undefined
      ? [cell, "cell"]
      : q < cell ? [q, "quota"] : q > cell ? [cell, "cell"] : [q, "both"];
    const [vcpus, vcpusLimitedBy] = pick(left("vcpus"), fitV);
    const [memoryMib, memoryLimitedBy] = pick(left("memoryMib"), fitM);
    const out = dimensions.find((d) => d.name === "instances").exhausted;
    return json(res, 200, {
      project: project.meta.name,
      dimensions,
      largestStartable: {
        vcpus, memoryMib, vcpusLimitedBy, memoryLimitedBy,
        none: out || vcpus === 0 || memoryMib === 0,
      },
    });
  }

  // `…/i1:explainRecovery` — why a guest has, or has not, been brought back
  // from a node that stopped answering. Computed, never written onto the
  // guest: the agent on its node owns that status.
  if (path.includes(":explainRecovery")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const guest = store.get(nameFrom(path.replace(":explainRecovery", "")));
    if (!guest) return fail(res, 404, "NOT_FOUND", "no such object");
    const on = guest.spec.node;
    if (!on) {
      return json(res, 200, {
        node: null, recoverable: false, why: "NotPlaced",
        detail: "it is not on a node, so there is nothing to recover it from",
      });
    }
    const node = store.get("nodes/" + on);
    const fences = node && Number(node.spec.fenceAfterS || 0) > 0;
    if ((guest.spec.onNodeLoss || "leave") !== "restart") {
      return json(res, 200, {
        node: on, recoverable: false, why: "PolicyIsLeave",
        detail: "it is set to be left where it is when its node goes quiet",
      });
    }
    if (!fences) {
      return json(res, 200, {
        node: on, recoverable: false, why: "NodeDoesNotFence",
        detail: on + " does not stop its own guests, so nothing can tell unreachable from stopped",
      });
    }
    return json(res, 200, {
      node: on, recoverable: true, why: "Recoverable",
      detail: "its node fences and it is set to restart elsewhere",
    });
  }

  // `…/node-b:explainMaintenance` — what is scheduled for one machine, and
  // what the drain would cost. Computed from the windows in the store, so a
  // window created through the console changes this answer the same way the
  // real API's would.
  if (path.includes(":explainMaintenance")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const node = store.get(nameFrom(path.replace(":explainMaintenance", "")));
    if (!node) return fail(res, 404, "NOT_FOUND", "no such object");
    const id = node.meta.name.split("/").pop();
    const at = Date.now();
    const mine = [...store.values()]
      .filter((r) => r.meta.name.startsWith("maintenance-windows/") && r.spec.node === id)
      .map((w) => ({
        window: w.meta.name,
        startsAt: w.spec.startsAt,
        endsAt: w.spec.startsAt + w.spec.minutes * 60_000,
        minutes: w.spec.minutes,
        drain: !!w.spec.drain,
        note: w.spec.note || "",
        opensInMinutes: w.spec.startsAt > at
          ? Math.floor((w.spec.startsAt - at) / 60_000)
          : null,
      }));
    const open = mine.find((w) => w.startsAt <= at && at < w.endsAt) || null;
    const next = mine.filter((w) => w.startsAt > at).sort((a, b) => a.startsAt - b.startsAt)[0] || null;
    const here = [...store.values()].filter((r) =>
      r.meta.name.includes("/instances/") && r.status.node === id);
    const draining = (open && open.drain) || !!node.spec.evacuate;
    return json(res, 200, {
      node: id,
      open,
      next,
      draining,
      // A guest holding a passed-through device is bound to this machine; the
      // rest would move. The same split the real evacuation makes.
      willMove: !draining ? [] : here
        .filter((i) => !(i.status.devices || []).length)
        .map((i) => ({ instance: i.meta.name, to: "node-a" })),
      cannotMove: !draining ? [] : here
        .filter((i) => (i.status.devices || []).length)
        .map((i) => ({
          instance: i.meta.name,
          why: [{ node: "node-a", detail: "it holds a passed-through device" }],
        })),
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

  // `nodes:explainCapacity` — what the cell has room for.
  //
  // The fixture is deliberately the misleading shape: plenty free in total,
  // spread thin. A console that showed only the sum would say a large guest
  // fits, and this cell has no room for one anywhere.
  if (path.includes("nodes:explainCapacity")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const nodes = [...store.values()].filter((r) => r.meta.name.startsWith("nodes/"));
    const usable = nodes.filter(
      (n) => n.spec.schedulable && !n.spec.evacuate,
    );
    const sum = (list, pick) => list.reduce((a, n) => a + pick(n), 0);
    const freeOf = (n) =>
      (n.status.capacity?.memoryMib || 0) - (n.status.allocated?.memoryMib || 0);
    // What a node is prepared to hand out, which is silicon unless somebody
    // set a ratio. Zero and one both mean one for one.
    const offeredOf = (n) =>
      (n.status.capacity?.vcpus || 0) * Math.max(1, n.spec.vcpuOvercommit || 0);
    const freeVcpus = (n) => offeredOf(n) - (n.status.allocated?.vcpus || 0);
    return json(res, 200, {
      usableNodes: usable.length,
      unusableNodes: nodes.length - usable.length,
      offeredVcpus: sum(usable, offeredOf),
      total: {
        vcpus: sum(nodes, (n) => n.status.capacity?.vcpus || 0),
        memoryMib: sum(nodes, (n) => n.status.capacity?.memoryMib || 0),
        diskGib: sum(nodes, (n) => n.status.capacity?.diskGib || 0),
      },
      allocated: {
        vcpus: sum(nodes, (n) => n.status.allocated?.vcpus || 0),
        memoryMib: sum(nodes, (n) => n.status.allocated?.memoryMib || 0),
        diskGib: sum(nodes, (n) => n.status.allocated?.diskGib || 0),
      },
      free: {
        vcpus: sum(usable, freeVcpus),
        memoryMib: sum(usable, freeOf),
        diskGib: 0,
      },
      // The largest single node's free memory — never the sum.
      largestFit: {
        vcpus: Math.max(0, ...usable.map(freeVcpus)),
        memoryMib: Math.max(0, ...usable.map(freeOf)),
        diskGib: 0,
      },
    });
  }

  // `nodes:explainCpu` — the fleet's processors, computed from what the nodes
  // report. A verb on the collection, because who can exchange guests with
  // whom is a property of the set rather than of any member.
  if (path.includes("nodes:explainCpu")) {
    if (req.method !== "GET") {
      return fail(res, 405, "INVALID_ARGUMENT", "that method is not allowed here");
    }
    const nodes = [...store.values()].filter((r) => r.meta.name.startsWith("nodes/"));
    const reported = nodes.filter((n) => n.status && n.status.cpu);
    const byPresented = new Map();
    for (const n of reported) {
      const key = (n.status.cpu.presentedFlags || []).slice().sort().join(",");
      if (!byPresented.has(key)) byPresented.set(key, []);
      byPresented.get(key).push(n.meta.name.split("/").pop());
    }
    const domains = [...byPresented.entries()].map(([, ids]) => {
      const node = reported.find((n) => n.meta.name.endsWith("/" + ids[0]));
      return {
        nodes: ids.slice().sort(),
        arch: node.status.cpu.arch || "x86_64",
        level: node.status.cpu.presents === "host" ? "x86-64-v2" : node.status.cpu.presents,
        canBaseline: node.status.cpu.canMask !== false,
      };
    });
    const advice = domains.length > 1
      ? [{
          kind: "BaselineWouldMerge",
          nodes: reported.map((n) => n.meta.name.split("/").pop()).sort(),
          level: "x86-64-v2",
          featuresLost: [{ node: domains[1].nodes[0], flags: ["avx2"] }],
        }]
      : [{ kind: "AlreadyUniform", nodes: reported.length, level: domains[0]?.level || null }];
    return json(res, 200, {
      unreported: nodes
        .filter((n) => !(n.status && n.status.cpu))
        .map((n) => n.meta.name.split("/").pop()),
      domains,
      advice,
      pendingAdoption: [],
    });
  }

  const name = nameFrom(path);

  if (req.method === "GET" && isCollectionName(name)) {
    // `?labels=env=prod,tier=web` — every term must match, a bare key asks
    // whether the label is there at all, and an empty selector narrows
    // nothing. The server does this so a console never fetches a cell to show
    // six rows of it.
    let all = narrow(listUnder(name), url.searchParams.get("labels"));
    // `?target=` — the records *about* one object. Only operations and audit
    // carry one; anything else asking has misunderstood, and answering with
    // the whole collection would look as though the filter had been applied.
    const target = url.searchParams.get("target");
    if (target) {
      const kind = name.split("/").pop();
      if (kind !== "operations" && kind !== "audit") {
        return fail(res, 400, "INVALID_ARGUMENT",
          kind + " are not records about another object");
      }
      all = all.filter((r) => r.spec.target === target);
    }
    if (url.searchParams.get("watch") === "true") {
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
      });
      const from = Number(url.searchParams.get("fromRevision") || 0);
      // Nothing between the list and the watch is lost: anything already newer
      // than the revision the client came with is replayed at once.
      for (const r of all) {
        if (Number(r.meta.revision) > from) {
          res.write("data: " + JSON.stringify({ type: "PUT", resource: decorate(r) }) + "\n\n");
        }
      }
      const w = { res, prefix: name };
      watchers.add(w);
      req.on("close", () => watchers.delete(w));
      return;
    }
    // The revision of the collection as a whole, not of whatever subset this
    // answer happens to carry. A walk pins it on its first page and every later
    // page repeats it.
    const newest = String(all.reduce((n, r) => Math.max(n, Number(r.meta.revision)), 0) || clock);

    const askedSize = url.searchParams.get("pageSize");
    const askedToken = url.searchParams.get("pageToken");
    if (askedSize === null && askedToken === null) {
      // Unpaged: the whole collection, and no token field at all. A field that
      // appeared on every answer — even empty — would itself be the change that
      // every client written before paging has to cope with.
      return json(res, 200, { items: all.map(decorate), revision: newest },
        { "X-Velstra-Revision": newest });
    }

    if (askedSize !== null && !/^\d+$/.test(askedSize)) {
      // Refused, never ignored: ignoring it hands the whole cell to a client
      // that believes it asked for twenty.
      return fail(res, 400, "INVALID_ARGUMENT",
        `pageSize must be a whole number, and was ${JSON.stringify(askedSize)}`, "pageSize");
    }

    const [parent, kind] = splitCollection(name);
    let token = null;
    if (askedToken !== null) {
      token = decodeToken(askedToken);
      if (!token) {
        return fail(res, 400, "INVALID_ARGUMENT",
          "page token is not a token this API issued", "pageToken");
      }
      if (token.kind !== kind || token.parent !== parent) {
        return fail(res, 400, "INVALID_ARGUMENT",
          `this page token was issued for ${token.parent}/${token.kind} and was presented ` +
          `for ${parent}/${kind}; start the list again`, "pageToken");
      }
    }

    const asked = Number(askedSize || 0);
    const size = Math.min(asked || DEFAULT_PAGE_SIZE, pageCeiling);
    const remaining = token ? all.filter((r) => r.meta.name > token.after) : all;
    const page = remaining.slice(0, size);
    const revision = ownRevisionPerPage
      // The mistake, made deliberately: the newest revision among the objects on
      // *this* page.
      ? String(page.reduce((n, r) => Math.max(n, Number(r.meta.revision)), 0) || clock)
      : token
        ? token.revision
        : newest;
    // Strictly greater: a collection that ends exactly on a page boundary is
    // done, and offering a token for the empty page after it is one wasted round
    // trip per walk that no count of objects would ever reveal.
    const more = remaining.length > size;

    const body = { items: page.map(decorate), revision };
    if (more) {
      body.nextPageToken = encodeToken({
        kind, parent, revision,
        after: page[page.length - 1].meta.name,
      });
    }
    return json(res, 200, body, { "X-Velstra-Revision": revision });
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
    // The one refusal that is about a *security claim* rather than a shape.
    // Nothing verifies a signature, so the API will not hold one — see
    // `ImageSpec::signature`. Enforced here too, because a fake that accepted
    // it would let the console grow a box for a field the real API rejects.
    if (name.endsWith("/images") && body.spec && typeof body.spec.signature === "string" &&
        body.spec.signature.trim() !== "") {
      return fail(res, 400, "INVALID_ARGUMENT",
        "spec.signature is not stored, because nothing in this platform verifies it",
        "spec.signature");
    }
    const full = name + "/" + body.id;
    if (store.get(full)) return fail(res, 409, "ALREADY_EXISTS", "that name is taken", "id");
    if (body.status) return fail(res, 400, "INVALID_ARGUMENT", "status is not writable", "status");
    const wrong = misspelled(name.split("/").pop(), body.spec || {});
    if (wrong) return fail(res, 400, "INVALID_ARGUMENT", wrong.message, wrong.field);
    const derived = derive(name.split("/").pop(), body.spec || {});
    if (derived.error) return fail(res, derived.status, derived.code, derived.error, derived.field);
    // Created unconverged, which is the honest state: nothing has looked at it.
    //
    // And created *now*. `put`'s default backdates an object an hour, which is
    // right for the seed — a cell that has been up a while — and wrong here, in
    // a way that was invisible and expensive: a migration's timeout is computed
    // from `createdAt`, and the console's migrate form asks to give up after
    // 3600s. A migration created through this path was therefore born exactly at
    // its own deadline and read `Timeout` about a second later, from the clock,
    // with nothing written and no event. Every screen that asked again — the
    // recheck, a reconnecting watch, a reopened sheet — saw a different object
    // than the one the create returned, so which of them noticed depended on
    // where a 15s timer happened to land. Checks that name their subject by its
    // `Moved` status inherited that as a coin toss.
    const r = put(full, derived.spec, {
      observedGeneration: 0,
      conditions: [condition("Ready", "Unknown", "Converging", "nothing has reported on this object yet", 0)],
      ...blankStatus(name),
    }, { createdAt: now() });
    announce("PUT", r);
    const op = put(name.split("/").slice(0, 2).join("/") + "/operations/op-" + clock,
      { target: full, targetGeneration: 1, verb: "create", requestedBy: "console" },
      { observedGeneration: 1, conditions: ready(1), done: false, error: null, finishedAt: null });
    announce("PUT", op);
    // Registering a node mints its one-time token, returned exactly once —
    // the API keeps a hash and cannot show it again. A console that did not
    // catch it here would leave an operator with a node object and no way to
    // register the machine.
    const created = { operation: op.meta.name, target: full };
    if (full.startsWith("nodes/")) {
      created.nodeToken = "b".repeat(64);
    }
    return json(res, 202, created);
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
  // A node that has just been registered and has not reported yet: zeroes, not
  // absence. The real API's status is a typed struct, so `capacity` is always
  // there — and a fixture that left it out crashed every reader that adds a
  // machine up, which is what happened the first time a node was created from
  // the console.
  if (kind === "nodes") {
    const zero = { vcpus: 0, memoryMib: 0, diskGib: 0, numaFreeMib: [], hugepages1gi: 0 };
    return {
      capacity: zero,
      allocated: { ...zero },
      agentVersion: "",
      lastHeartbeat: 0,
      images: [],
      devices: [],
    };
  }
  return {};
}

seed();
const port = Number(process.env.FAKE_PORT || 0);
server.listen(port, "127.0.0.1", () => {
  // The port is announced rather than fixed: a run left behind by an
  // interrupted test must not make the next one attach to a ghost.
  process.stdout.write("listening " + server.address().port + "\n");
});
