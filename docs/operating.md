# Running a cell

The day-2 half: what to do when a machine has to come out, when a tenant runs
out of room, when a guest will not start, and when something has gone wrong and
nobody can see why.

`docs/rest-contract.md` says what each call answers. This says which one to
reach for, and — more usefully — which of them will tell you the answer
*before* you commit to anything.

---

## 1. Ask before you act

Seven calls answer questions that are otherwise found out afterwards, at the
worst moment. Every one of them is a plain `GET` that changes nothing.

| Question | Ask |
|---|---|
| Why is this guest not running? | `…/instances/g1:explainPlacement` |
| What will taking this machine out cost? | `nodes/node-b:explainMaintenance` |
| Can this guest move, and where to? | `…/instances/g1:explainMigration` |
| Is this cell one migration domain? | `nodes:explainCpu` |
| Has this tenant room, and is it their quota or the machines? | `projects/p1:explainQuota` |
| What is left in the cell at all? | `nodes:explainCapacity` |
| Why has this guest not come back? | `…/instances/g1:explainRecovery` |

Two of them are worth reading even when nothing is wrong.

**`:explainMaintenance` before every maintenance window.** It lists the guests
that *cannot* move. A guest holding a passed-through device is bound to that
machine and will be stopped when the machine is; finding that out while the
machine is on a trolley is finding it out too late.

**`:explainQuota` before promising a tenant anything.** It names the binding
limit. `quota` means a message to you; `cell` means waiting, a smaller shape, or
another machine. Answering the wrong one sends somebody to buy hardware they do
not need.

---

## 2. Taking a machine out of service

Do not flip switches by hand at two in the morning. Declare the window in
advance:

```
POST /api/v1/maintenance-windows
{ "id": "dimm-swap",
  "spec": { "node": "node-b", "startsAt": 1755600000000, "minutes": 60,
            "drain": false, "note": "swapping the failed DIMM in slot 3" } }
```

* `drain: false` — nothing new is placed there, everything running stays put.
  A firmware update, a reboot, anything measured in minutes.
* `drain: true` — and the guests are migrated away first. Pulling the machine.

The window is a **declaration**, not a switch. Nothing writes `schedulable` or
`evacuate` on your behalf, so those two stay yours — and when the window ends,
service resumes because the window has *stopped being open*. There is nothing
to flip back and nothing left flipped if a controller died in the middle.

Two consequences worth knowing:

* A window that has passed is a **record**, not a claim. Leave it; it is the
  answer to "what did we do to node-b last Tuesday".
* A node inside an open window is not counted into `largestFit`, so
  `:explainQuota` stops promising a shape that cannot be placed.

`note` earns its place: it is what turns "no capacity" into "node-b is out
until 03:00 for the memory swap" wherever a placement is refused.

---

## 3. When a machine dies without being asked to

Recovery is **off** unless two things are true, and both are deliberate:

1. the node has `fenceAfterS` set, so it stops its own guests when it loses the
   control plane; and
2. the guest has `onNodeLoss: "restart"`.

Without the first, nothing can tell "unreachable" from "stopped", and starting
the guest elsewhere is how two machines come to write to one volume. Without the
second, a guest on local storage would be restarted somewhere with nothing to
restart *into* — an empty machine wearing a familiar name.

`:explainRecovery` says which of the two is missing, per guest, in a sentence.

---

## 4. Sharing a processor

```
PATCH /api/v1/nodes/node-a   { "spec": { "vcpuOvercommit": 4 } }
```

Placement only: the guest still gets the vCPUs it asked for. What changes is how
many the cell believes the machine has room for. `nodes:explainCapacity` then
reports both numbers — `total.vcpus` is silicon, `offeredVcpus` is the promise.

There is no memory ratio and there should not be one until this platform can
take a page back. A guest promised 8 GiB and handed 4 is not slow; it is killed.

---

## 5. Keeping guests apart, and together

```
"placementPolicy": {
  "antiAffinityGroup": "web", "spread": "Required",
  "affinityGroup": "checkout", "affinity": "Preferred"
}
```

Anti-affinity keeps a service alive when a machine dies. Affinity keeps it fast
while they all live — an application and the cache it reads on every request.

`Required` refuses; `Preferred` prefers and takes the second-best rather than
not running. Three replicas of a database want the first even if it means one
stays down. Twelve web servers want the second.

---

## 6. Public addresses

Two decisions, and both are worth making on purpose.

**Does the guest hold the address?** `delivery: "Routed"` binds it to the
guest's port and the guest configures it — nothing rewrites a packet, and the
guest can tell anybody its own address, which SIP, FTP, IPsec and mDNS all need.
`delivery: "Nat"` keeps it at the edge; the guest never knows.

**Who tells the world where it is?**

| | `FromHost` | `FromGateway` |
|---|---|---|
| Path | straight to the machine holding the guest | via a gateway, then the overlay |
| Encapsulation for public traffic | none | both directions |
| Follows a live migration | by itself | by itself |
| Needs | every hypervisor to peer upstream | one machine (or a few) to peer |
| Fails when | the rack will not let hypervisors peer | the gateway is full or down |

`FromHost` is the efficient one and it is the reason the address is a `/32` with
a next hop in no subnet: nothing is tied to an L2 segment, so the same guest
configuration is right on every machine, and the announcement follows the guest
because each host announces the addresses of the ports it holds. There is no
sequencing to get right — no central thing has to move an address at the same
moment the guest moves.

Set the cell's usual answer on the external network, and let a particular
address disagree when it has a reason to.

**Before it works, three things have to be true outside this control plane.**
Two are yours, one is the fabric's, and none of them is claimed by
`:explainReach` — that call says who *should* be announcing and what the guest
was told, never that a session exists.

*Yours:*

1. The prefix is routed to this cell, and the machine doing the announcing has
   a BGP session with the router above it. A rack decision, not a platform one.
2. The external network's subnet carries the real prefix and the real upstream
   gateway.

*The fabric's, and these are the open items:*

3. **Answer for the next hop.** A guest routes through `169.254.1.1` (or
   `fe80::1`), which is in no subnet: the host has to answer ARP/ND for it and
   route what arrives. Without this the guest has an address and no way to send
   from it.
4. **Announce a bound address upstream.** `velstra-app/src/wren.rs` already
   drives Wren for EVPN type-2 inside the fabric; what is missing is announcing
   a port's *bound* addresses to an external peer, and withdrawing them when the
   port leaves the host. That is what makes `FromHost` follow a migration — and
   it is the only piece of that mechanism that does not exist yet.
5. **List a port's bound addresses.** `PortInfo` reports one address, the fixed
   one, so the control plane cannot ask whether a public address is still bound
   and has to remember instead — the one fact in the floating-IP controller that
   is remembered rather than observed, marked as such in the code. A fabric
   restored from a snapshot older than the binding will not have it, and nothing
   notices; clearing `spec.port` and setting it again re-binds.

Everything on the cloud side is in place: the address is allocated, refused
where it could not work, bound to the port on the fabric, carried into the
guest's own network configuration, and answerable at `:explainReach`.

---

## 6a. Copies: which tool for which loss

| You lost | Reach for |
|---|---|
| the last hour's work, pool intact | a **snapshot** (or a snapshot schedule) |
| the pool | a **backup** (or a backup schedule) |
| nothing — you want ten more like this one | a **capture**, which becomes an image |

A snapshot lives in the volume's own pool: taken in a moment, costs almost
nothing, and is lost with the pool it is in. A backup is a copy on a target that
survives losing the pool. They are not substitutes and the platform will not
pretend otherwise.

Restoring is making a **new** volume from a copy (`sourceBackup`), never an
in-place restore: an in-place restore is a command sitting in a spec, performed
again on every resync, undoing whatever the guest wrote in between. If the copy
cannot be read — never taken, or its target is not mounted on that pool's
machine — the volume is refused rather than made blank.

A capture refuses a running guest, and the refusal says why: a disk copied from
under a running machine is crash-consistent, which a template stamped out a
hundred times must not be.

---

## 7. When somebody says "I clicked it and nothing happened"

Open the object and read its **History**. It carries two things:

* what was **accepted** — operations, with who asked and whether it finished;
* what was **refused** — audit records, carrying *the same sentence* the person
  was given.

Reading only the first is how the conversation goes in circles. The refusal is
the answer, and the person who was refused can read it themselves: a record is
readable by whoever may read what it is about, and by the person it is about.

---

## 8. The console, in one paragraph

It opens on the **Overview**: what needs attention across every collection, what
the machines have room for, what is out of service or scheduled to be, and what
the current project has left. Clicking a row goes to the board the object lives
on and opens it. `⌘K` finds a collection *or* an object by name. Long boards
take a label filter (`env=prod, tier=web`), narrowed by the API rather than in
the browser. Rows can be selected for a bulk start, stop or delete — done one at
a time, with every refusal reported in the API's own words and the selection
keeping exactly what did not work.

---

## 9. Checks worth running against a real machine

```
nix build .#checks.x86_64-linux.register -L      # a node registers over a token
nix build .#checks.x86_64-linux.guest -L         # a guest really boots (needs nested KVM)
nix build .#checks.x86_64-linux.maintenance -L   # a window closes a node, and expiry reopens it
nix build .#checks.x86_64-linux.wizard -L        # the installer ISO, prompt by prompt
nix build .#checks.x86_64-linux.console -L       # the console in a real browser
```

`register`, `guest` and `maintenance` run a real API, a real controller and a
real node agent in a VM; `wizard` boots the installer ISO and answers it prompt
by prompt.

`console` is different and it is worth knowing how: it drives a real browser
against `fake-api.mjs`, an in-memory implementation of the contract, so it can
run in a second and without a cell. What keeps that honest is
`velstra-cloud-api/tests/contract_shapes.rs`, which records the shape of every
answer the *real* API gives and holds the fixture to it — see the contract
document's "One recorded shape, two implementations".
