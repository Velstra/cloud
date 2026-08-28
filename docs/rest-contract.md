# The REST surface, fixed

Two crates are written against this without talking to each other: the API
serves it, the console consumes it. Neither may change it alone.

gRPC is the native surface; this is the JSON gateway over the same handlers, so
a field that is not here is not in the proto either.

## Names and shapes

A resource is addressed the way AIP addresses one:

```
projects/{project}/instances/{instance}
```

and the HTTP path is that name under `/api/v1/`:

```
GET    /api/v1/projects/p1/instances/i1
GET    /api/v1/projects/p1/instances            # list
POST   /api/v1/projects/p1/instances            # create, id in the body
PATCH  /api/v1/projects/p1/instances/i1         # change spec
DELETE /api/v1/projects/p1/instances/i1
```

Collections, in the order the API serves them: `projects`, `users`,
`ceph-clusters`, `instances`, `migrations`, `volumes`, `snapshots`,
`attachments`, `networks`, `routers`, `floatingips`, `load-balancers`,
`subnets`, `ports`, `security-groups`, `images`, `nodes`, `pools`,
`device-classes`, `backup-targets`, `backups`, `backup-schedules`, `audit`,
`captures`, `console-sessions`, `image-sources`, `usage`,
`snapshot-schedules`,
`maintenance-windows`, `operations`.

### Narrowing a list by label

Every object carries `meta.labels`. A list can be narrowed by them:

```
GET /api/v1/projects/p1/instances?labels=env=prod,tier=web
GET /api/v1/projects/p1/instances?labels=deprecated
```

Every term must match. There is no "or" — it would need precedence rules, and
a filter whose meaning depends on precedence is one people get wrong silently.
Somebody who needs "or" runs two searches and can see both answers.

A bare key asks whether the label is there at all, whatever its value. An
**empty** selector matches everything, because that is what "no filter" has to
mean: the alternative is a filter box that empties the list when it is cleared,
which reads as "nothing here" rather than "no filter".

A selector matching nothing is an empty list and `200`, never an error — "no
guests are tagged that" is an answer, and refusing it would make a typo look
like a broken endpoint.

Filtering is **not** refusing, and the audit reflects that: narrowing a list
writes nothing, however many objects it skips. `audit` records somebody who
reached for a thing by name and was told no.

### Snapshot schedules

The cheap half of the pair:

```
POST /api/v1/projects/p1/snapshot-schedules
{ "spec": { "volume": "projects/p1/volumes/data-1", "everyHours": 1, "keep": 24 } }
```

A snapshot lives in the volume's **own pool**. It is taken in a moment, costs
almost nothing, and is the right tool for "let me be able to go back an hour".
It is not a backup: lose the pool and it goes with it. That is why these are
two collections rather than one with a flag — a flag is a thing people set
wrong, and the consequence only shows up on the day the pool is gone.

The retention rules are the *same code* as `backup-schedules`, not a second
copy of them: a snapshot still being taken holds the schedule, only finished
ones count toward `keep`, at least one always survives, and ones taken by hand
are never expired. A rule written twice is a rule that will eventually be two
rules.

### Networks: logical, or the machine's own wire

A network is **logical** by default: the platform allocates its addresses, the
fabric — or the node, as a first hop — carries its segment, and its security
groups mean something.

An operator may instead put one on a bridge that already exists on the nodes:

```
POST /api/v1/projects/p1/networks
{ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }
```

Guests on it go straight onto whatever the machine is on. Their addresses come
from whatever serves that wire, so this platform allocates none, holds no
gateway on it, translates nothing out of it, and **answers no DHCP** — a second
server on one segment is how a guest ends up with an address nobody agrees on.
The metadata service still answers them: a guest on a host bridge reaches
`169.254.169.254` on its node like any other neighbour.

**Only a cell operator may set it.** A tenant asking is refused:

```
403 { "error": { "code": "PERMISSION_DENIED", "field": "spec.hostBridge",
                 "message": "only a cell operator may put a network on a host bridge…" } }
```

A bridge that is not there is refused by the node, on the port, rather than
created — making one means deciding what goes in it, and the only useful answer
involves the machine's uplink.

### Usage: what a project had, and when

`project.status.used` says what is in use **now**, counted from the objects that
exist. It has no memory, so a guest that ran for three weeks and was deleted
this morning is indistinguishable from one that never existed — which is right
for a quota and useless for a bill.

So the present is written down, once an hour, under the project:

```
GET /api/v1/projects/p1/usage
→ { "items": [ { "meta": { "name": "projects/p1/usage/1787824800000" },
                 "spec": { "project": "projects/p1",
                           "at": 1787824800000,
                           "used": { "instances": 3, "vcpus": 12, … } } } ] }
```

Ids are the millisecond, zero-padded, so a listing is in time order without an
index. Readings are kept ninety days.

**A reading is a sample, not a total.** Something created and destroyed between
two readings is in neither, and is not billed. The alternative is charging from
the object lifecycle, which means a counter — and a counter loses a charge for
ever when the process holding it dies at the wrong moment, with nothing able to
prove afterwards which happened. The interval is the knob; a provider who needs
finer than an hour shortens it and pays for the rows.

Written by the controller and by nothing else. They are not creatable, editable
or deletable through the API: a usage record that could be changed after the
fact is a bill nobody can stand behind.

### A way into a guest: the console

A guest's serial line is reachable when the network is not — which is the only
time it matters. Ask for a console on the instance:

```
POST /api/v1/projects/p1/instances/i1:console
→ 200 { "session": "projects/p1/console-sessions/console-3f9a2c81",
        "ticket": "…", "readOnly": false, "expiresAt": … }
```

Then open a websocket to the API, which relays to the node holding the guest:

```
GET /api/v1/projects/p1/instances/i1:consoleStream?ticket=…
Upgrade: websocket
```

Binary frames are the guest's bytes in both directions. A caller with `Read`
but not `Write` on the project is given `readOnly`, and what it types is
dropped rather than refused mid-session.

**The ticket is spent once and expires in a minute.** It is minted by the API,
stored **hashed** on the session — every node in the cell may read the cell, and
a session carrying the ticket in the clear would hand each of them a way into a
guest on somebody else's machine — and the node that accepts it records the fact
so a second connection presenting it is refused.

A `console-sessions` object is therefore a record rather than something to
create by hand: posting to the collection is possible and pointless, because a
session whose ticket nobody knows opens nothing.

### Templates: capture a guest, stamp out copies

Build a guest by hand, get it right, stop it, capture it. Every guest made from
the result starts where that one left off.

```
POST /api/v1/projects/p1/captures
{ "spec": { "instance": "projects/p1/instances/golden",
            "label": "debian-13-golden",
            "target": "backup-targets/shared" } }
```

**The guest must be stopped.** A disk copied from under a running machine is
crash-consistent at best — survivable for a backup, which is read once in an
emergency by somebody who knows what happened, and not survivable for a
template that will be stamped out a hundred times by people who assume it is
clean:

```
409 { "error": { "code": "FAILED_PRECONDITION", "field": "spec.instance",
                 "message": "…is running, and a disk copied from under a running machine is crash-consistent — which a template stamped out a hundred times must not be. Stop it first. If what you want is a copy of a live guest, take a backup: that is read once, by somebody who knows what happened." } }
```

It is a resource rather than a call, and that follows from something already
true: **an image's name carries its digest**, which is what makes fetching one
verifiable — the agent refuses any image whose name does not. So there is no
name to hand back before the bytes exist. The node copies and reports
`status.digest`; a controller then creates the image, named
`<label>-sha256-<digest>` and pointing at `file://<target>/<digest>`. Any node
that can reach that path can boot from it.

Which image a capture became is **computed**, not stored: the node owns the
capture's status and does not create the image, so a field saying "it is over
there" would need a second writer on one object. `label` and `digest` are both
on the capture, and a derived link cannot go stale — a capture whose image was
deleted stops naming one instead of pointing at something gone.

Cloning is then what it already was: create an instance with
`spec.image` naming the captured image. `image.spec.sourceInstance` records
which guest it came from, so a list of near-identical templates says which is
which.

### Audit

`audit` is **not** a log of everything that happened. Every successful write
already creates an `operation` carrying its target, its verb, who asked for it
and when it finished — that is the record of what was done, and a second log
beside it would eventually disagree with it about one afternoon.

What no operation exists for is a request that was **refused**: nothing is
created, nothing changes, and the only trace is a status code somebody else
received. That is exactly what a multi-tenant cell is asked about afterwards —
who tried to reach another tenant's guests, and from when. Sign-ins are here
for the same reason.

```
GET /api/v1/audit
{ "items": [ { "spec": { "kind": "refused", "subject": "alice@example.com",
                         "verb": "read", "target": "projects/p2/instances/i1",
                         "detail": "…", "at": 1787687886000 } } ] }
```

`detail` is the *same* sentence the caller was given, not a paraphrase — an
audit line an operator has to correlate by hand against what somebody actually
saw is one they stop trusting.

Cell-scoped, and readable by cell operators only: a tenant who could read this
would learn the names of projects and people that are not theirs, which is the
opposite of what it is for.

**It cannot be flooded.** A refusal is something an attacker can cause at will,
so the record's id is derived from who, what, which verb and which *minute* — a
thousand attempts in one minute collide on create and leave one record. The
exact count is lost and the fact is not, which is the right way round.

Nothing expires it. Refusals are rare in a working cell and sign-ins are bounded
by how many people there are, so the volume is small — and a record that quietly
went away before somebody came looking is worse than a disk they can see
filling. Records can be deleted by an operator who has read them.

### Backups

A **snapshot** lives in the volume's own pool. It is the right tool for "let me
be able to go back an hour" and it is not a backup: lose the pool and the
snapshot goes with it, at exactly the moment somebody needs it.

A **backup** is a copy on a `backup-target` — somewhere that is not the source
pool. That single property is the reason the collection exists, and it is
enforced rather than documented:

```
POST /api/v1/projects/p1/backups
{ "spec": { "volume": "projects/p1/volumes/data-1", "target": "backup-targets/same-pool" } }
409 { "error": { "code": "FAILED_PRECONDITION", "field": "spec.target",
                 "message": "backup-targets/same-pool is pools/fast, which is where projects/p1/volumes/data-1 already lives — a copy in the same pool is a snapshot, and is lost with the pool it is in. Back up to a different target." } }
```

`backup-targets` is cell-scoped: the storage belongs to the cell. `backups` and
`backup-schedules` are a project's.

A **schedule** is an intention, never a job queue:

```
POST /api/v1/projects/p1/backup-schedules
{ "spec": { "volume": "projects/p1/volumes/data-1", "target": "backup-targets/nightly",
            "everyHours": 24, "keep": 7 } }
```

A controller creates a `backup` when nothing this schedule made is younger than
`everyHours`, and deletes this schedule's oldest once more than `keep` have
**finished**. Two rules that are not obvious and both matter:

* A copy still being made counts toward "is one recent enough" — so a volume
  that takes an hour to copy does not have a second asked for every pass, and a
  copy that is *stuck* holds the schedule for one interval and no longer.
* Only **finished** copies count toward `keep`, and at least one always
  survives. A week of failed attempts must not expire the last copy that
  worked, which is precisely the week somebody will need it.
* Copies taken by hand carry no `schedule` and are never expired by one. They
  are somebody's decision.

**Restoring is making a new volume**, never writing one back:

```
POST /api/v1/projects/p1/volumes
{ "spec": { "sizeGib": 40, "pool": "pools/fast",
            "sourceBackup": "projects/p1/backups/nightly-1787687886" } }
```

`sourceBackup` sits beside `sourceImage` and `sourceSnapshot` — three spellings
of one statement about where a volume came from. An in-place restore would be a
command living in a spec, carried out again on every resync, undoing whatever
the guest wrote in between, forever, with nothing on the object to say it
happened.

`device-classes` is cell-scoped, like `nodes`: a class names interchangeable
PCI hardware across the fleet, and defining it per project would be a different
name for the same silicon in every tenancy. What a project controls is
`quota.devices` — how many passed-through devices its instances may hold.

An instance asks for a class, never a PCI address:

```
POST /api/v1/projects/p1/instances
{ "spec": { "devices": ["gpu-a100"], ... } }
```

An address (`0000:41:00.0`) belongs to one machine, so an instance naming one
could only ever be scheduled there. Two entries of the same class mean two
different devices. A device is offered only when **everything in its IOMMU
group is free** — a group is the unit of isolation, so it is passed through
whole or not at all — and `:explainPlacement` says which neighbouring device
stands in the way:

```
{ "node": "node-b", "why": "NoDevice",
  "detail": "no free gpu-a100 here — 0000:41:00.0: 0000:41:00.1 is in the same IOMMU group (17) and is bound to snd_hda_intel; a group is passed through whole or not at all" }
```

A guest holding one cannot be live-migrated — a device's state is in hardware
and there is nothing to send — and the refusal names the alternative:

```
{ "node": "node-b", "allowed": false, "why": "HoldsDevices",
  "detail": "it holds 0000:41:00.0 — a device's state is in hardware and cannot be transferred; move it with mode Reboot, which stops the guest and gives it the destination's devices" }
```

Three collections stored beside these are deliberately **not** served and are on
no path here: `credentials` (a password hash), `sessions` (a live bearer token),
and `node-credentials` (the digest of a per-node agent token). A route that does
not exist cannot leak what it would have returned; signing in and out reach the
first two through the session endpoints below, and a node token is minted at
registration and verified on every request, never listed.

One of them hangs off another object rather than off a project: a snapshot is
created under the volume it copies, at
`projects/p1/volumes/data-1/snapshots/nightly`. A name is a path, so it is still
listed by any prefix of itself — `…/volumes/data-1/snapshots` for one volume's
copies, `projects/p1/snapshots` for the project's.

Every resource body is the same three parts, always all three:

```json
{
  "meta": {
    "name": "projects/p1/instances/i1",
    "uid": "…", "generation": 3, "revision": "412",
    "placement": { "region": "eu-central", "cell": "cell-1" },
    "createdAt": 1786732800000,
    "deletedAt": null,
    "finalizers": ["node.velstra.io/release"],
    "labels": {}
  },
  "spec":   { … },
  "status": {
    "observedGeneration": 2,
    "conditions": [
      { "kind": "Ready", "status": "Unknown", "reason": "Converging",
        "message": "the node has not reported on this change yet",
        "observedGeneration": 2, "lastTransition": 1786732801000 }
    ],
    …
  }
}
```

`status.status` is one of `True`, `False`, `Unknown` — never a free string.

## Who may do what

A **project is the unit of tenancy**: everything under `projects/p1` is governed
by `p1`'s bindings, whatever its kind. Resources outside every project — nodes,
pools, the projects collection itself — are the **cell operator's**, and the
cell's operators are named in the cell's own configuration rather than in any
object. They are the provider; they may do anything anywhere.

### The roles

| Role | May | Cannot |
|---|---|---|
| `viewer` | look at everything in the project | change anything |
| `operator` | run what is there — start, stop, resize, attach, open a console with a keyboard | bring anything into existence or take it away |
| `editor` | that, and create and delete | change who may |
| `admin` | everything, including the bindings | widen what the cell allowed the project |

```
PATCH /api/v1/projects/p1
{ "spec": { "bindings": [
    { "role": "admin",    "members": ["ada@example.com"] },
    { "role": "operator", "members": ["oncall@example.com"] },
    { "role": "viewer",   "members": ["audit@example.com"] }
] } }
```

`operator` is the rung a platform serving customers needs: the people who keep
an estate running are usually not the people who decide what it consists of, and
before it existed anybody who needed to restart a guest was given the ability to
destroy one.

Changing bindings needs `admin` — kept apart from everything else so an editor
cannot grant themselves more than they were given. A role name that does not
parse reads as `viewer`: a typo lands on the least, not the most.

### What the cell allows a project

A quota says **how much**. A project's policy says **what kind**, and it is the
cell operator's to set:

```
PATCH /api/v1/projects/p1
{ "spec": { "policy": {
    "hostBridges": ["br0"],
    "devicePassthrough": false,
    "floatingIps": true
} } }
```

* `hostBridges` — bridges on the nodes this project's networks may be put on.
  Empty means logical networks only. Named rather than a yes/no, because what
  anybody means by a host bridge is a particular wire.
* `devicePassthrough` — whether guests here may be given a GPU or a NIC of their
  own.
* `floatingIps` — whether this project may hold addresses the world can reach.

A **project admin may not change it**, which is what makes the rest of it worth
anything:

```
403 { "error": { "code": "PERMISSION_DENIED", "field": "spec.policy",
                 "message": "what a project may reach for is set by a cell operator…" } }
```

A project created today gets the closed policy. A project stored before the
field existed reads as it behaved before — open — because yesterday's object has
to keep meaning what it meant.

## The rules a client may rely on

- **`spec` is writable, `status` is not.** A PATCH carrying `status` is
  rejected with 400 and a message naming the field. This is the same rule the
  store enforces; the API does not get to be more permissive.
- **`generation` moves iff `spec` changed.** A client that PATCHes an identical
  spec gets 200 and an unchanged generation, not a no-op error.
- **`revision` is the ETag.** Send it back as `If-Match` to make a write
  conditional; without `If-Match` a write is last-writer-wins and the client
  said so by omission. On mismatch: 409 with the current revision in the body.
- **Convergence is `status.observedGeneration == meta.generation`.** There is no
  `state: PENDING` anywhere to poll on. A client waiting for a change waits for
  that equality plus a `Ready` condition.
- **Deletion is two-phase and visible.** DELETE sets `meta.deletedAt` and
  returns 202 with the object; the object stays listable, with its finalizers,
  until they are released. A client that wants "gone" waits for 404.

## Derived fields

A field the platform already knows is not asked for twice. They follow one rule:
omitted means the API fills it in, stated means it must agree, and a value that
disagrees is refused rather than quietly corrected — rewriting what somebody
typed changes what an object says without them asking.

**`attachment.spec.node` is derived from the instance.** Omit it at create and
the API copies it from `instance.spec.node`. State it and it must agree; a value
that disagrees is refused rather than stored, because an attachment whose node
is not the instance's is a meaningless object — the node it names does not have
the guest, and the node that does is not watching for it, so the volume is never
opened and nothing says why. Derived, that state cannot be written down.

Attaching to an instance that has not been placed yet is a
`FAILED_PRECONDITION` on `spec.node`, with the sentence saying so: there is no
node to open the volume on. An interface should show that where the choice is
made, not after the request.

**`port.spec.node` is derived from the guest that uses it.** Not by the API but
by a controller, because the answer changes over the port's life: it is the node
holding whichever instance names the port, taken from `instance.status.node` —
where the guest *is*, not where a scheduler decided it should go. A client never
sets it, and a client never needs to.

It exists because it is what makes a port claimable at all. A node may write an
object's status only when it owns it or when the object is assigned to it, and a
port had neither, so every node's report on a port it was carrying was refused.
The symptom was quiet and thoroughly misleading: the guest ran, and its port sat
at `programmed: false` with no `tapDevice` for ever. A port now reports honestly,
and `securityGroup.status.conditions[Applied]` above depends on it.

It is derived once, at create. A migration moves the attachment deliberately;
nothing follows a guest silently.

**`migration.spec.fromNode` is derived from the instance.** Omit it at create
and the API copies it from `instance.status.node` — the node's own report of
where the guest is, not `spec.node`, which is only where it was assigned. Those
two disagree for as long as a handover takes, and the machine that can send a
guest is the machine that has it; naming the assignment would name the wrong
source in exactly the case that matters, a second migration asked for while the
first is still in flight. State it and it must agree; a value that disagrees is
refused with the sentence saying where the guest actually is.

An instance that no node is reporting has no source to move from, and that is a
`FAILED_PRECONDITION` on `spec.instance` rather than a migration that no agent
will ever pick up.

**`snapshot.spec.pool` is derived from the volume it copies.** A copy is made
where the bytes already are; no backend makes one in a pool that does not hold
the original. Stated and disagreeing is refused on `spec.pool`, with a sentence
naming the pool the volume is really in.

**A volume made from a snapshot takes `spec.pool` and `spec.sizeGib` from it.**
Both are omitted-means-filled-in as above, with one relaxation, stated here
because it is the only one in this document: `sizeGib` must be *at least* the
snapshot's size rather than equal to it. A volume is grown, so asking for a
larger one at the moment it is created is an ordinary thing to want; asking for a
smaller one is the clone not fitting into what it is written into, and that is a
`FAILED_PRECONDITION` on `spec.sizeGib`.

**`port.spec.address` and `port.spec.mac` are filled in after the create, not
during it.** They are the one pair in this document that is derived by a
*controller* rather than by the API, and the difference is visible to a client:
a `POST` returns a port with both fields `null`, and they appear a moment later.

That is deliberate. Whether an address can be given depends on objects other
than the port — the subnet may not exist yet, or may be full — and none of those
are reasons for the create to fail. A port whose subnet arrives second would
otherwise have to be created twice. So the port exists immediately, and until it
has an address it says why on itself: `Ready=False` with reason `NoSuchSubnet`,
`SubnetNotAddressable` or `SubnetFull`, and a sentence naming what to change.

Stating either field is allowed and is never overruled — an operator who picks
`10.20.0.99` keeps it. An address is only ever filled in, never moved: a port
whose address changed under a running guest is an outage with no error message.
The MAC is derived from the port's `uid`, so a write that was lost and retried
produces the same NIC rather than a new one.

A client waiting for a usable port waits for `spec.address` to be non-null, the
same way it waits for anything else here — there is no `ALLOCATING` state.

## Computed fields

Six fields are answered on every read and stored nowhere, so none of them can
disagree with the world they describe:

- **`operation.status.done`** — from the target's convergence.
- **`migration.status.conditions[Moved]`** — from the migration and the instance
  together, including its deadline. A migration whose destination agent has died
  can write nothing at all, and that is precisely when somebody needs to be told
  it timed out.
- **`image.status.cachedOn`** — from what each node reports in
  `node.status.images`. Which nodes hold an image is an *aggregate*, and an
  aggregate is not a fact anybody owns: a list on the image would need every node
  in the cell writing into one field, which is the shared mutable list the
  one-writer rule exists to forbid. Each node says what it holds; the API adds
  them up.
- **`instance.status.pendingChanges`** — what a running guest has been asked for
  and will only get at its next start, from `spec` against `status.runningSize`.
  Absent, not empty, when there is nothing pending. Resizing a running machine
  is ordinary; what was not ordinary is that nothing said so, and a spec reading
  as applied while the guest ran on the old numbers is a disagreement no screen
  showed.
- **`node.status.pciDevices[].groupWith`** — every device sharing an IOMMU
  group with this one, from `pci::group_members`. Passing one device through
  takes its whole group; `pci::offerable` already refuses an unsafe assignment,
  and this is the sentence before the decision. Computed rather than left to a
  client because a device with **no** group is answered as being alone: grouping
  by equal group number would instead collect every un-isolatable device into
  one imaginary group, which reads as "these come together" when the truth is
  that none of them can be passed at all.
- **`securityGroup.status.conditions[Applied]`** — from the ports that name the
  group. Same shape as the one above: nothing about a group is a fact any single
  writer owns. Each port reports whether its own node has it in force, and the
  API adds those up; `observedGeneration` follows only once every one of them
  has caught up, so a group whose rules were just changed does not claim to be
  in force before any port has re-read it. A group nothing references is
  `Applied` — vacuously, and truthfully: there is nothing for it to wait on.

A client reads all three as ordinary fields. What it may not do is `PATCH` one —
they are `status`, and `status` is not writable.

**`lastTransition` on a computed condition is not an age.** A stored condition
keeps the moment it changed; a computed one is built during the request, so its
`lastTransition` is the moment of *that read*. Rendering it as "changed 2s ago"
would put movement on a transfer that stalled an hour back. There is one
exception, and it is anchored because the moment is genuinely knowable: a
`Moved` condition with reason `Timeout` carries `createdAt + timeoutS`, which is
accurate to the minute and worth showing. For every other computed condition,
show the message and leave the time alone.

## Networking, from the guest's side

Two things a guest talks to are not part of this REST surface at all, and they
are described here because what they say is built entirely out of objects that
are: both run on the node that runs the guest, and both answer only for the
guests on that node. A cell's control plane can be down, and a node that is up
still tells its own guests who they are.

**The metadata service, `169.254.169.254:80`.** A guest is identified by the
source address of its connection — an address this node programmed onto its
port. Nothing in the request selects an answer: no token, no header, no
parameter, so there is no request one guest can make that asks about another.
An address the node does not run gets a 404, the same answer as a path that does
not exist.

The paths are EC2 IMDS, which is what an unmodified cloud image probes with no
configuration:

```
/latest/meta-data/instance-id          projects/p1/instances/i1
/latest/meta-data/hostname             i1
/latest/meta-data/local-ipv4           the address it is asking from
/latest/meta-data/mac                  that NIC's MAC
/latest/meta-data/public-keys/0/openssh-key
/latest/meta-data/network/interfaces/macs/<mac>/local-ipv4s
/latest/meta-data/network/interfaces/macs/<mac>/subnet-ipv4-cidr-block
/latest/user-data                      instance.spec.userData
```

The same facts are served at the three flat NoCloud paths — `/meta-data`,
`/user-data` and `/network-config` — for an image told `ds=nocloud-net`. The
reason for both is one gap rather than a wish to support everything: the EC2
shape has no key for a gateway and none for a resolver (an AWS guest learns both
from DHCP), and `network-config` is netplan, which can say them. Both renderings
come from one document, so they cannot disagree.

`network-config` matches interfaces by MAC and states addresses rather than
`dhcp4: true`, so a guest whose DHCP client is slow, disabled or replaced still
comes up on the network it was given.

**DHCP, on each guest's tap.** The node answers DISCOVER and REQUEST with the
address the port already carries: mask and gateway from the subnet, resolvers
from the subnet, MTU from the network, hostname from the instance. A guest is
identified by the pair (tap it arrived on, MAC in the packet) — it can forge the
MAC, but not which tap the frame came out of, so a spoofed neighbour's MAC finds
no binding at all.

**There is no lease anywhere.** The binding is the `Port` object and nothing
else; an ACK writes nothing, and the answer is re-derived from the objects each
time it is asked for. That is why a port that was deleted stops being answered
for immediately, why a node restart loses nothing, and why there is no
`leases` collection in this API. A `REQUEST` for an address that is not the
port's is answered with a NAK naming the one that is.

## Security groups

A port names its groups in `spec.securityGroups`, by resource name. A group is a
list of rules, and a rule is one allowance:

```json
{
  "direction": "ingress",
  "protocol": "tcp",
  "ports": { "from": 443, "to": 443 },
  "remote": { "cidr": "0.0.0.0/0" }
}
```

Four things a client may rely on:

- **Ingress is denied, egress is allowed, replies are always allowed.** Rules
  only add allowances. There is no deny rule and no ordering, so two groups on
  one port cannot contradict each other and "which rule won" is not a question.
- **`remote` is either a `cidr` or a `group`.** A group remote — "anything in
  `web` may reach me" — is resolved to the addresses its members hold *at the
  time of the pass*, so it keeps working as guests come and go, and it is never
  stored anywhere. Membership does not chain: naming `web` admits the ports in
  `web`, not whatever `web` itself admits.
- **A group with no members admits nothing**, which is emphatically not the same
  as admitting everything.
- **Naming a group that does not exist is not an error.** Rules only add
  allowances, so a missing group is strictly fewer of them — the safe direction
  — and the port keeps working rather than a typo costing a guest its network.
  It is reported on the node that noticed.

A rule whose port range is set on a protocol that has no ports, or that runs
backwards, or whose `cidr` is not a prefix, is refused on write with the index
of the offending rule in the error's `field`.

## Storage

A volume lives in a **pool** — `spec.pool` — and the pool is what reports on it.
That is the same two-field ownership as an instance and its node, and it is why
`volume.status.pool` exists: `spec.pool` is the ask, `status.pool` is the pool
that has claimed it. A volume whose `status.pool` is null has not been picked up
by anything, which usually means no agent is running for the pool it names.

Pools are an ordinary collection. `status.backend` is what the agent found
itself running, not something an operator declares; `status.allocatedGib` is
counted from the volumes in the pool rather than tracked as a total.

Two behaviours a client may rely on:

- **A volume is grown, never shrunk.** A `sizeGib` smaller than what exists is
  accepted as a spec — it is a legitimate thing to write — but nothing happens
  to the bytes, and the volume reports `Ready=False` with reason
  `WillNotShrink` and a sentence saying what to do instead. Shrinking would
  destroy whatever is past the new end, and no request makes that recoverable.
- **Deleting waits for the pool.** `DELETE` returns 202 and the volume stays
  listable, carrying `pool.velstra.io/release`, until the pool reports it holds
  nothing of it. A volume that vanished from the API while its pool still held
  the gigabytes would be storage nobody is billed for and nobody can find.

## Snapshots

A snapshot is a point-in-time copy of a volume, in that volume's own pool, and
it is created **under** the volume:

```
POST /api/v1/projects/p1/volumes/data-1/snapshots → 202
{ "id": "nightly" }

GET /api/v1/projects/p1/volumes/data-1/snapshots/nightly
{ "meta": …, "spec": { "pool": "pool-a" },
  "status": { "taken": true, "sizeGib": 100, "takenAt": 1786732800000, … } }
```

**The source is in the name, and there is no field for it.** Which volume a copy
came from is the one thing about it that must never change — a snapshot
repointed at another volume is a restore that quietly hands back somebody else's
data — and a name cannot be patched. It is also why a copy is created under a
volume rather than in a collection of its own: a `POST` to
`projects/p1/snapshots` is refused on `meta.name`, with the shape that works.

`status.sizeGib` is the *logical* size of the copy: how large the volume was
when it was made, and therefore the smallest volume that can be made from it.
It is not what the copy occupies in the pool — a delta grows as the volume it
was taken from moves on, and that is a billing question this API does not answer
yet. For the same reason a snapshot does not count against a project's
`volumeGib` quota: quota is counted from what was asked for, and nobody asks for
a snapshot's size.

Four behaviours a client may rely on:

- **A snapshot is taken once and never again.** A volume that vanished from its
  pool is created again on the next pass; a snapshot that vanished is not. A
  copy made now is a copy of a different moment, and it would be restored under
  the name of the one somebody wanted. It reports `Ready=False` with reason
  `Vanished` and stays that way until it is deleted.
- **A copy is refused before it costs anything.** Of a volume whose pool has not
  provisioned it yet — there is nothing to copy — or of one that is being
  deleted, which the copy would pin forever. Both are `FAILED_PRECONDITION`,
  answered before the object exists.
- **The source may not go first.** A snapshot is a delta against the volume it
  came from on every backend this platform speaks to, so deleting the volume
  would make the copies unreadable — silently, at the moment somebody deletes
  something they believe they have backups of. `DELETE` on a volume with
  snapshots returns 202 as always, and then the volume *stays*: it carries
  `snapshot.velstra.io/source` alongside the pool's finalizer, and the pool
  destroys nothing while it can still see copies, reporting `Ready=False` with
  reason `SnapshotsDependOnIt` and how many are in the way. Delete the snapshots
  and the volume goes by itself. Nothing cascades: this platform does not delete
  objects an operator did not ask about.
- **Restoring is creating a volume, not writing one back.** There is no
  restore verb and no `RestoreSnapshot` RPC. A volume is created with
  `spec.sourceSnapshot` set, and the clone is part of creating it — so there is
  no pass in which the new volume exists blank.

```
POST /api/v1/projects/p1/volumes → 202
{ "id": "data-1-restored",
  "spec": { "sourceSnapshot": "projects/p1/volumes/data-1/snapshots/nightly" } }
```

**A `PATCH` that sets `spec.sourceSnapshot` on an existing volume is refused**,
on `spec.sourceSnapshot`, with a sentence saying to create a volume instead.
That refusal is the shape of the whole decision, so it is worth stating why: an
in-place restore is a *command*, and a command that lives in a `spec` is carried
out again on every resync — the second time undoing everything the guest wrote
since the first, with nothing on the object to say it happened. The alternative
was a `Restore` resource of its own, the way a migration is one; it would work,
and what it buys over `sourceSnapshot` is keeping the volume's name — which is
what attachments point at. That is a real cost and it is paid deliberately:
re-attaching a new volume is a visible, reversible step, and overwriting a
volume a guest has open is not. `spec.sourceImage` is refused on change for the
plainer reason that nothing would be re-cloned, so the field would start
describing a volume that does not exist.

A volume comes from one place: naming both `sourceImage` and `sourceSnapshot` is
a `FAILED_PRECONDITION`. So is naming a snapshot that has not been taken yet, or
one held by a different pool than the volume would live in — no backend clones
between pools behind a single command.

## Load balancers

One address in front of many ports, served by the fabric's own L4 balancer:
flows are DNAT-rewritten at the ingress host with connection tracking and
reverse NAT, so there is no appliance to place and no machine that owns the
object — its status is written by the controller, like a router's.

```json
{ "spec": {
    "network": "projects/p1/networks/prod",
    "subnet":  "projects/p1/subnets/prod-a",
    "vip": "10.20.0.20",
    "listeners": [ { "protocol": "Tcp", "port": 443, "memberPort": 8080 } ],
    "members":   [ "projects/p1/ports/web-1-eth0" ] },
  "status": {
    "vip": "10.20.0.20",
    "listeners": [ { "protocol": "Tcp", "port": 443, "members": 1 } ], … } }
```

What a client may rely on:

- **`spec.vip` is derived like a floating IP's address.** Omitted, a controller
  fills in the lowest address nothing else holds — the same counting the ports
  and the floating IPs use, so no two holders ever share an address. Stated, it
  is kept and never overruled. `status.vip` is the observed half: the address
  the fabric actually serves, empty until it does.
- **A listener is `protocol` (`Tcp` or `Udp` — the datapath balances no
  others), the `port` the VIP answers on, and `memberPort`, the port the
  members answer on; `0` or omitted keeps the client's own destination port.**
  A listener on port 0, or two listeners claiming one `protocol/port`, is
  refused on write with the index of the offending listener in the error's
  `field`. A load balancer with **no** listeners is accepted and waits,
  reporting `Ready=False` with reason `Incomplete` — the same shape as a router
  with no networks.
- **`members` names ports, not addresses**, so a migrated guest stays in the
  pool, and a member can never point at an address nothing serves. Empty is a
  drained pool — a legitimate state to hold an address in. A member that does
  not resolve yet (no such port, not placed, not programmed) holds the whole
  pool back with `Ready=False` reason `MembersNotReady` naming it; the fabric
  is never programmed with part of a pool, because a pool serving three of its
  four members looks balanced and silently leaves one out.
- **There are no weights, no algorithm and no health checks — deliberately.**
  The fabric spreads flows uniformly by connection hash and reports nothing
  about a member's health, and this API does not carry a field nothing reads:
  a weight the console displayed as if it biased traffic, or a health-check
  policy nothing runs, would be the unverified-signature defect over again.
  `status.listeners[].members` is a count of what is programmed, emphatically
  not a health verdict. When the fabric grows any of these, the field arrives
  with the code that honours it.
- **Deleting waits for the fabric.** `DELETE` returns 202 and the object stays
  listable, carrying `fabric.velstra.io/release`, until every fabric service
  has been retired — an address that kept answering after its object vanished
  would be traffic arriving somewhere nothing can explain.
- **Quota**: a project's `quota.loadBalancers` caps how many a project may
  hold, counted from what exists like every other dimension, and refused at
  create with `RESOURCE_EXHAUSTED`.

Like `security-groups`, this collection is served on the JSON surface only;
there is no gRPC service for it yet.

## Long-running operations

Anything that cannot finish inside the request returns an operation, AIP-151:

```
POST /api/v1/projects/p1/instances → 202
{ "operation": "projects/p1/operations/op-7", "target": "projects/p1/instances/i1" }

GET /api/v1/projects/p1/operations/op-7
{ "meta": …, "spec": { "target": "…", "targetGeneration": 1, "verb": "create" },
  "status": { "done": false, "error": null, … } }
```

`done` is computed from the target's convergence, never stored independently —
an operation cannot disagree with the object it describes.

## Migrations

Moving a running guest to another node is a **resource**, not a state on the
instance. There is no `MIGRATING` anywhere to poll on, for the same reason there
is no `PENDING`: a controller that dies mid-flight would leave it set forever,
and somebody would have to decide by hand whether the guest is on the old node,
the new one, or both.

```
POST /api/v1/projects/p1/migrations → 202
{ "id": "m1", "spec": { "instance": "projects/p1/instances/i1", "toNode": "node-b" } }
```

`mode` (`Live`, `PostCopy`, `Reboot`), `downtimeMs`, `timeoutS` and
`connections` may be given; omitted, they are the model's defaults — live
pre-copy, a 300 ms final pause, an hour, one stream — never zeroes.

**Creating one refuses anything that cannot work**, before a byte moves:
the destination is draining, has less free memory than the guest, does not hold
the image, or runs an agent too many versions away. Each is a
`FAILED_PRECONDITION` naming the field an operator would change — `spec.toNode`
for a destination that cannot receive, `spec.instance` for a guest that is not
running — with a sentence saying which node lacks what. Nothing is created when
one of those answers comes back. This is the point: the far end refusing after a
gigabyte of memory has been copied is the same refusal, an hour later.

**Creating one changes nothing about the instance.** The migration is an ask;
the instance keeps its node until the source reports that the guest has left it,
and only then does a controller move `instance.spec.node`. That is the single
moment of handover, and it is why a failed transfer costs nothing: under
pre-copy the guest is still running on the source.

**What it is doing arrives as a `Moved` condition that is computed on read and
never stored** — the same rule as an operation's `done`, and for the same
reason. It is a judgement about *another* object: the instance running on the
destination is the whole definition of finished. A stored copy could go stale,
and the case it goes stale in is the worst one — a migration whose destination
agent has died cannot write anything, and that is precisely when somebody wants
to be told it timed out. Computed, the answer comes from the process being
asked:

| `reason` | `status` | what it means |
|---|---|---|
| `PreparingReceiver` | `Unknown` | the destination is not listening yet |
| `Transferring` | `Unknown` | memory is moving; `status.transferredMib` says how much |
| `HandingOver` | `Unknown` | the source has let go, the destination has not claimed it yet |
| `Arrived` | `True` | it is running on the destination |
| `Timeout` | `False` | it did not finish within `spec.timeoutS`, and it says where the guest is now |
| `NoSuchInstance` | `False` | the instance it names does not exist |

Arrival beats the clock: a guest that landed a second after the deadline
landed. Anything else in `status.conditions` is stored, and is the destination
reporting on itself — that it could not bind a receiver, say. `Moved` is never
among those, so a client must read it from a `GET`, a list or a watch event
rather than expect it on an object it wrote.

`Timeout` is a report, not a repair: nothing is moved back, because nothing was
moved. The deadline that actually stops a transfer is `spec.timeoutS`, handed to
the hypervisor with the send. Abandoning a migration is a `DELETE`, which tells
the source to stop sending and the destination to tear its receiver down; the
guest stays where it is.

## Watching

```
GET /api/v1/projects/p1/instances?watch=true&fromRevision=412
```

Server-sent events, one JSON object per event:

```
data: {"type":"PUT","resource":{…}}
data: {"type":"DELETE","name":"projects/p1/instances/i1","revision":"460"}
```

A client lists first, notes `meta.revision` of the newest object (or the
`X-Velstra-Revision` header on the list response), then watches from it. Nothing
between the list and the watch is lost.

## Errors

```json
{ "error": { "code": "FAILED_PRECONDITION", "message": "…", "field": "spec.vcpus" } }
```

`code` is one of `INVALID_ARGUMENT`, `NOT_FOUND`, `ALREADY_EXISTS`,
`FAILED_PRECONDITION`, `ABORTED` (revision conflict), `RESOURCE_EXHAUSTED`
(quota), `PERMISSION_DENIED`, `UNAUTHENTICATED`, `INTERNAL`. `message` is a
sentence for a person; `field` points at the offending path when there is one.

## Explain

Placement failures are answerable, not greppable:

```
GET /api/v1/projects/p1/instances/i1:explainPlacement
{ "placed": null,
  "rejected": [ { "node": "node-a", "why": "InsufficientMemory", "detail": "4096 free, 8192 wanted" },
                { "node": "node-b", "why": "NoNumaNodeFits",    "detail": "8192 wanted" } ] }
```

So are the destinations a guest could be moved to, answered per node and before
anything is created:

```
GET /api/v1/projects/p1/instances/i1:explainMigration
{ "from": "node-a",
  "destinations": [ { "node": "node-a", "allowed": false, "why": "AlreadyThere",
                      "detail": "it is already on node-a" },
                    { "node": "node-b", "allowed": true,  "why": "", "detail": "" },
                    { "node": "node-c", "allowed": false, "why": "DestinationLacksImage",
                      "detail": "node-c does not have projects/p1/images/sha256-3f9a2b" } ] }
```

**Every** node gets a verdict, including the one the guest is on: a console
greys out what cannot work *before* the click, and a destination missing from
the answer is one it cannot decide about. The verdict is the same function the
create runs, so the two can never disagree. `why` is a stable token
(`AlreadyThere`, `NotRunning`, `NotFromThere`, `DestinationDraining`,
`DestinationTooSmall`, `VersionsTooFarApart`, `DestinationLacksImage`) and
`detail` is the sentence; both are empty on a destination that is allowed.
Asking creates nothing and writes nothing.

And so is the state of the cell's processors — the one question that is about
the fleet rather than about any member of it, which is why it hangs off the
collection:

```
GET /api/v1/nodes:explainCpu
{ "unreported": [],
  "domains": [ { "nodes": ["node-a","node-b"], "arch": "x86_64",
                 "level": "x86-64-v2", "canBaseline": true },
               { "nodes": ["node-c"], "arch": "x86_64",
                 "level": "x86-64-v4", "canBaseline": true } ],
  "advice": [ { "kind": "NodeOutsideTheAggregate", "node": "node-c",
                "presents": "host", "aggregate": "x86-64-v2",
                "aggregateNodes": 2, "missing": [] } ],
  "pendingAdoption": [ { "instance": "projects/p1/instances/i1", "node": "node-a",
                         "running": "x86-64-v4", "wouldGet": "x86-64-v2" } ] }
```

And what would actually still fit:

```
GET /api/v1/nodes:explainCapacity
{ "usableNodes": 8, "unusableNodes": 2,
  "total":      { "vcpus": 320, "memoryMib": 1310720, "diskGib": 40960 },
  "allocated":  { "vcpus": 210, "memoryMib":  917504, "diskGib": 22000 },
  "free":       { "vcpus":  88, "memoryMib":  262144, "diskGib": 14000 },
  "largestFit": { "vcpus":  16, "memoryMib":   32768, "diskGib":  4000 } }
```

`largestFit` is the field worth having and the reason this is computed here
rather than left to whoever draws the dashboard: **free memory does not add up
into a guest.** Sixty-four gibibytes spread over eight nodes fits no
sixteen-gibibyte guest, and a summary showing only `free` tells somebody one
does.

`free` counts the **usable** nodes only, so it and `total` disagree by exactly
the drained capacity — on purpose. A node that is draining, being emptied, or
not reporting still exists and still has memory; none of it is somewhere a
guest can go. `unusableNodes` is named rather than folded in, because "we have
twelve nodes" and "eight will take a guest" are different sentences and the
second is the one somebody planning capacity needs.

A **domain** is a set of nodes that can exchange a guest. Computed on every
read, never stored: a stored grouping drifts from the fleet the moment a
machine is replaced.

`advice` is tagged by `kind`, so a console branches on what it is rather than
sniffing which fields are present. Every recommendation carries its **cost**:
`BaselineWouldMerge` lists `featuresLost` per node, because a suggestion that
names only the benefit arrives wearing the platform's authority. On
`NodeOutsideTheAggregate`, an empty `missing` means the machine could join and
simply has not been told; a non-empty one means it never can, and the honest
remedy is a second aggregate.

`pendingAdoption` lists guests still running a CPU their node no longer hands
out — the ordinary, self-clearing state after a baseline change. Each adopts
on its next restart. Until then it can only move to a node still presenting
what it booted with, which after a fleet-wide change may be nowhere.

`unreported` names nodes whose agent has not said what processor they have.
Named rather than dropped: "2 of 5 nodes" with no list reads as a broken
report. Such a node is in no domain and is never shown as compatible with
anything — the whole surface fails closed.

Asking creates nothing and writes nothing.

### Start order

After a power cut a node brings back everything it holds at once, and the
database everything else needs loses the race for disk to a dozen web servers.

```
PATCH /api/v1/projects/p1/instances/db-1  { "spec": { "startOrder": 1 } }
PATCH /api/v1/projects/p1/instances/web-1 { "spec": { "startOrder": 2, "startDelayS": 30 } }
```

Lower starts first; the same number is a group that starts together. `0` — the
default — is the first group. `startDelayS` is measured from the **newest**
start in the group ahead, not from each member in turn: otherwise a fleet's
boot is the sum of every delay rather than the longest one.

Nothing is queued and there is no "starting" state. Every pass asks the same
question of the same world, and a guest that may not start yet is simply not
started this pass.

What counts as *settled*, ahead of you, is where the deadlock would live:

* `Running` — up.
* `Failed` — it had its chance. Waiting forever for a guest that cannot start
  would take a whole node down for one broken disk.
* Anything whose `desiredState` is `Stopped` — it is not coming up.

Everything else holds the guests behind it, and that is deliberate: the
alternative is starting the application servers without the database and
calling it success.

### Resizing a guest

`spec.vcpus`, `spec.memoryMib` and `spec.rootDiskGib` can be changed on a guest
that is running. **Nothing changes on the running machine.** The guest gets what
was asked for the next time it starts.

That is said rather than implied, and this is the fix for a real hole: the
platform used to accept such a change, do nothing, and report the object as
converged — a spec that read as applied while the guest ran on the old numbers.

What a running guest actually has is on its status:

```
GET /api/v1/projects/p1/instances/i1
{ "spec":   { "vcpus": 8, "memoryMib": 8192, "rootDiskGib": 40, ... },
  "status": { "runningSize": { "vcpus": 4, "memoryMib": 8192, "rootDiskGib": 40 }, ... } }
```

The difference between the two *is* the pending change; there is no separate
flag, because a flag can be stale and a comparison cannot. `runningSize` is
absent while the guest is not running — there is nothing to differ from, and
its next start gives it the spec by construction.

A root disk may grow. It may **not** shrink:

```
PATCH /api/v1/projects/p1/instances/i1   { "spec": { "rootDiskGib": 10 } }
400 { "error": { "code": "FAILED_PRECONDITION", "field": "spec.rootDiskGib",
                 "message": "…has a 40 GiB root disk and cannot be shrunk to 10: the bytes past the new end would be gone and the filesystem using them would find out later. Make a smaller guest from a backup instead." } }
```

Shrinking is not a resize, it is a truncation. No backend asks the guest first
and none can.

### Taking a node out of service

Two fields, and they are separate because they are separate intentions:

* `nodes/<id>.spec.schedulable: false` — **drain**. Nothing new is placed here;
  what runs keeps running. This is what an operator wants for a reboot.
* `nodes/<id>.spec.evacuate: true` — **empty it**. And none of the old stays
  either.

Evacuating creates one ordinary `migration` per guest that can move — nothing
else, because the migration machinery already knows when moving a guest is
safe, and a second path that also moved guests would be a second set of rules
about that. The emptiest destination wins by free memory: first-fit would move
every guest to the same machine, which is how emptying one node fills another.

A guest that cannot move is left where it is, and the node still empties around
it. Some never can — one holding a passed-through device is bound to that
machine — and `:explainMigration` says which node refused for what. Turning
`evacuate` off stops further moves; it does not bring anything back, because a
transfer that has started is a thing to see through rather than a state to
unwind.

### When a node stops answering

A node that has gone quiet is not a node that has stopped. It may be
unreachable and still running every guest it holds, and starting those guests
elsewhere then produces two of each writing to one volume — an outage turned
into a restore from backup.

So recovery rests on one mechanism: **the node's own agent stops its guests
before anything may start them.** `nodes/<id>.spec.fenceAfterS` is how long the
agent may fail to report before it does, decided against its own clock, needing
nothing from anybody. The control plane then waits that long *again* before
unplacing anything.

A node whose `fenceAfterS` is zero — the default — is **never recovered from**.
Nothing can tell "unreachable" from "stopped", so nothing is assumed.

A guest opts in with `spec.onNodeLoss: "restart"` (default `"leave"`). Only for
one whose storage every node can reach: a guest on local storage started
elsewhere is an empty machine wearing a familiar name.

Why a guest has or has not come back is answered, never written onto it — the
agent on its node owns that status, and two writers on one object is the thing
this platform is built to prevent:

```
GET /api/v1/projects/p1/instances/i1:explainRecovery
{ "node": "nodes/node-b", "recoverable": false, "why": "WaitingForFencing",
  "detail": "nodes/node-b was last heard from 30s ago; 120s is when its guests are certainly stopped" }
```

`why` is a stable token — `PolicyIsLeave`, `WaitingForFencing`,
`NodeDoesNotFence`, `HoldsDevices`, `NotRunning`, `NotPlaced` — because the
reasons are four different afternoons for whoever reads them. Recovery itself
is one write: the controller clears `spec.node`, and the scheduler then places
the guest exactly as it would any unplaced one.

### Keeping guests apart, and keeping them together

```
"placementPolicy": {
  "antiAffinityGroup": "web", "spread": "Required",
  "affinityGroup": "checkout", "affinity": "Preferred"
}
```

Anti-affinity keeps a service alive when a machine dies; affinity keeps it fast
while they all live — an application and the cache it reads on every request,
where a hop between machines is the whole latency budget. A platform with only
the first can express half of what people actually run.

`spread` and `affinity` are each `Required` (the default, and what this platform
did before the fields existed) or `Preferred`. Both are right answers to
different questions: three replicas of a database must not share a machine even
if that means one stays down; twelve web servers would rather be crowded than
short. A `Preferred` constraint is carried into the score instead of rejecting,
where "beside its sibling" loses to "anywhere else" and still beats "not running
at all". The order is lexicographic and not weighted — be with your group, then
away from your siblings, then on the emptiest machine — because a weight is a
number somebody has to tune and the first time it is wrong it is wrong silently.

Affinity only bites once a member is placed: before that there is nothing to be
near, and refusing every node would mean a group whose first member could never
start. When it cannot be honoured, the rejection says where the group actually
is:

```
"rejected": [ { "node": "node-a", "why": "NotWithGroup", "detail": "checkout is on node-b" } ]
```

### Sharing a processor, and never sharing memory

```
PATCH /api/v1/nodes/node-a   { "spec": { "vcpuOvercommit": 4 } }
```

Zero — the default — means one for one, the same reading a quota's zero gets, so
a node stored before the field existed behaves exactly as it did. It applies to
**placement only**: the guest still gets the vCPUs it asked for and the
hypervisor still schedules them; what changes is how many the cell believes the
node has room for.

There is deliberately **no memory ratio**. A processor can be shared — two
guests that both want a core get one each in turn, and being wrong costs speed —
and that is a trade an operator makes on purpose. Memory cannot: a guest
promised 8 GiB and handed 4 is not slow, it is killed, and the operator finds
out from a guest that has vanished. There should be no such field until this
platform can take a page back, which means ballooning, and which is a feature
rather than a number.

Past `32` vCPUs per core the ratio is refused: that is where it stops being a
trade and becomes a way of hiding that a cell is full.

`nodes:explainCapacity` reports both numbers — `total.vcpus` is silicon,
`offeredVcpus` is what the usable nodes will hand out. Either alone reads as
though the cell had grown a processor.

### One recorded shape, two implementations

The console is tested against `velstra-cloud-console/tests/console/fake-api.mjs`,
which implements this document in memory — so a console can be written and
tested before the server runs. The cost is that every custom method has two
implementations with nothing between them.

So the API records what it answers. `velstra-cloud-api/tests/contract_shapes.rs`
writes `velstra-cloud-api/tests/contract/shapes.json` — the **shape** of each
answer, keys and value kinds, never values — and the console's own suite holds
its fixture to the same file. One artifact, generated by the implementation that
ships.

`null`, `[]` and `{}` on either side mean "this fixture had nothing here", which
is a fact about a fixture and not about the contract. What is left is what
breaks a screen: a key spelled differently, missing, or holding a number on one
side and a string on the other. Re-record deliberately and read the diff:

```
UPDATE_SHAPES=1 cargo test -p velstra-cloud-api --test contract_shapes
```

**One spelling worth knowing about.** `hugepages1gi` is all lowercase on the
wire. The field is `hugepages_1gi`, and `hugepages_1_gi` — which would read
`hugepages1Gi` — cannot round-trip: coming back, a digit followed by an
uppercase letter is how `l3Vni` is told from `hugepages1gi`, and one convention
cannot serve both.

### Public addresses: the ones the guest actually holds

Two ways an address can reach a guest, and they are not variants of one thing:

```
POST /api/v1/projects/p1/floatingips
{ "id": "web", "spec": { "subnet": "projects/p1/subnets/public",
                         "port": "projects/p1/ports/web-1-eth0",
                         "delivery": "Routed", "announce": "FromHost" } }
```

* `delivery: "Nat"` (the default, and what this object meant before there was a
  choice) — translated at the edge; the guest never sees the address. The
  datapath half of it is the fabric's and is deferred there.
* `delivery: "Routed"` — the address is **bound to the port as a second address
  and configured by the guest**. Nothing rewrites a packet. The guest gets it as
  a `/32` (or `/128`) with an on-link next hop, `169.254.1.1`, which is in no
  subnet anybody declared — that is what frees the address from any L2 segment,
  so the same configuration is correct on every hypervisor and stays correct
  across a live migration.

A guest holding a public address **defaults out through it**. Leaving the
default on the tenant gateway would send replies from the public address out of
a door they cannot return through.

A routed address must come from a subnet on a network an operator marked
`external: true` — a tenant range is not an address the world can reach, and
only a cell operator may set that flag (a tenant who could would mint themselves
a public prefix by typing a CIDR).

**Who announces it** is the second choice, and it is a real one:

* `FromHost` — the hypervisor holding the guest announces the /32 itself.
  Shortest path: for north-south traffic the overlay is not in the path at all,
  and the route follows a migration by construction, because each host announces
  the addresses of the ports it holds. Needs every hypervisor to peer with the
  network above it.
* `FromGateway` (the default) — a node with `spec.gateway: true` announces it and
  traffic reaches the guest over the overlay. Few, stable next hops upstream; a
  hairpin in both directions. A cell with no gateway node is refused by name
  rather than silently doing nothing.

The network says what the cell does (`network.spec.announce`); an address may
disagree (`floatingip.spec.announce`). Those are two different questions — one
about the wiring, one about a particular service — and an address that is silent
takes the network's answer.

```
GET /api/v1/projects/p1/floatingips/web:explainReach
{ "address": "203.0.113.7", "delivery": "Routed", "external": true,
  "port": "projects/p1/ports/web-1-eth0", "on": "node-a",
  "announced": { "from": "host", "nodes": ["node-a"] },
  "guest": { "address": "203.0.113.7/32", "via": "169.254.1.1",
             "onLink": true, "defaultRoute": true } }
```

`announced.from` is `null` with a `why` when nothing is announcing it — held and
pointing at nothing, a guest not yet placed, a cell with no gateway, or a
translated address, which has no route of its own to announce.

### A capture is real bytes now

The node holding the guest claims the capture, copies the disk to the target,
hashes it, and reports the digest. Until then the object was created, assigned
to that node, and nothing ever picked it up — so the controller that turns a
finished capture into an image never had a finished one to act on.

The copy is written under a temporary name and hashed *there*: the final name
carries the digest, and a digest cannot be known before the bytes exist. That is
the same reason `capture` is a resource rather than a call that hands back an
image.

A running guest is refused, in the model's own words — and the refusal names the
tool that does what they wanted (a backup, which is read once by somebody who
knows what happened, rather than stamped out a hundred times by people who
assume it is clean).

A guest whose root disk is a pool volume answers `NoDisk`: the node has no
bytes to read, and the honest place to capture it from is the pool.

### Where copies are kept

```
POST /api/v1/backup-targets
{ "id": "archive",
  "spec": { "kind": "directory", "path": "/srv/archive", "accepting": true,
            "agent": "nvme" } }
```

`kind` is **`directory`**, lowercase — the one field on this object people
mis-type, because it reads as an exception beside `Running` and `Stopped`. The
refusal names the spelling it wanted.

`agent` names the pool agent that **reports** on the target: whether the path is
there, whether it can be written, how much room is left. Named by an operator
rather than claimed by whoever gets there first, because a target assigned to
nobody is one any agent could grab — and "an agent may only report on what it
was given" is what makes a node token a boundary rather than a formality.

Leaving it empty means nobody is looking. `status.writable` is then `null`,
which is **not** the same as `false`: copies are still written there by the pool
holding the volume, and a path it cannot reach fails loudly on the backup rather
than quietly on the target.

### Backups are real bytes now

A `Backup` is claimed by the pool holding the volume, which copies the bytes out
to the target's path under the backup's own name with its slashes flattened —
`/srv/archive/projects~p1~backups~b1` — so a person can read a target with `ls`
and two cells sharing one cannot collide.

`status.taken` is **consulted**, not merely reported: a copy that exists is
never made again, because a second one would be of a different moment under a
name somebody trusts. A copy that failed is never reported as taken, and the
reason lands on the backup itself — "why is there no copy" is asked months
later by somebody looking at the backup, not at a log on whichever machine
happened to run that pool.

Restoring is creating a volume **from** a copy:

```
POST /api/v1/projects/p1/volumes
{ "id": "restored", "spec": { "sizeGib": 40, "pool": "nvme",
                              "sourceBackup": "projects/p1/backups/b1" } }
```

The backup's name, never a path: a path is a fact about one machine's
filesystem. The pool resolves it — the backup says which target, the target says
where — and if it cannot (the copy was never taken, or the target is not mounted
on that machine) the volume is **refused rather than made blank**. A volume that
was asked to be a restore and quietly came up empty is the worst outcome
available: it boots, it is the right size, and everything that was on it is
gone.

### Writing too fast

A cell may cap how fast one caller **writes** (`--writes-per-second`, off by
default). Over the cap:

```
429 Retry-After: 1
{ "error": { "code": "RESOURCE_EXHAUSTED",
             "message": "too many writes at once; this one would be the 21st in a second. Try again in 50 ms — the same request, unchanged, will be accepted then." } }
```

The wait is on the response, in the header for a client library and in the
sentence for a person, because a client that guesses either spins or backs off
far longer than it needed to. `Retry-After` is rounded **up** to whole seconds,
so a client that obeys it to the letter is never turned away twice for the same
reason.

Three properties worth relying on:

* **Reads are never counted.** A caller reading in a loop is only slowing
  themselves down.
* **Node agents are never limited.** An agent reports when something changed,
  and something changing is not something it can defer; refusing one would make
  it fall behind and be judged unreachable by the control plane that was the
  reason.
* **It is a bucket, not a window.** Somebody who has been quiet may spend a
  burst at once — creating twenty guests is a normal Tuesday — and a caller who
  keeps going settles at the sustained rate.

This is not a security boundary and does not pretend to be one. What it stops is
the ordinary accident: one tenant taking the cell's write path from the rest
without ever meaning to.

### A field this platform does not have

A spec field nobody has heard of is **refused**, not ignored:

```
PATCH /api/v1/nodes/node-a   { "spec": { "memoryOvercommit": 2 } }
400 { "error": { "code": "INVALID_ARGUMENT", "field": "spec.memoryOvercommit",
                 "message": "there is no field called memoryOvercommit on a nodes; nothing would have been done with it" } }
```

Serde ignores what it does not recognise, which is right for **reading stored
objects** — a field removed from the code must not make yesterday's data
unreadable — and wrong at the door: an operator answered `200` goes home
believing memory is overcommitted. So the strictness is at the boundary and not
on the types.

An unknown field carrying *nothing* — `null`, `""`, `0`, `[]`, `{}` — is
accepted in silence. That is somebody echoing back an object or clearing a
field, and no intention is lost.

### The records about one object

```
GET /api/v1/projects/p1/operations?target=projects/p1/instances/i1
GET /api/v1/audit?target=projects/p1/instances/i1
```

Only `operations` and `audit` carry a target; any other collection asked for one
is refused rather than answered with the whole cell as though the filter had
applied. A target that matches nothing is an empty list — "nothing has happened
to it" is an answer.

The two together are the object's history, and reading only the first is how
somebody concludes their click did nothing: the **refusal** is the answer.

`audit` is cell-scoped, and a cell operator reads it whole. It is **not**
operators-only, and the exception has two exact edges: a record is readable by
whoever may read **what it is about**, and by **the person it is about**.
Neither leaks — the first already reads the target, the second is their own
refusal — and everything else about the cell stays the operator's, including a
refusal about another project and a sign-in, which is about no object at all.

As with every list, a caller who may see none of it gets an empty list rather
than a `403`: a refusal on a collection would be an oracle for what is in it.

### What a project has left, and what it could actually start

Two halves in one answer, because either alone answers the wrong question. "24
vCPUs of quota left" is what a tenant reads before creating a guest that will
never be placed; "no valid host" is what they get afterwards, from a scheduler
that knows nothing about quotas.

```
GET /api/v1/projects/p1:explainQuota
{ "project": "projects/p1",
  "dimensions": [
    { "name": "instances", "limit": 10, "used": 3, "left": 7,
      "unlimited": false, "exhausted": false },
    { "name": "memoryMib", "limit": 0, "used": 20480, "left": null,
      "unlimited": true, "exhausted": false }, … ],
  "largestStartable": { "vcpus": 16, "memoryMib": 16384,
                        "vcpusLimitedBy": "quota", "memoryLimitedBy": "cell",
                        "none": false } }
```

`limitedBy` is the point: `quota` is a message to an operator, `cell` is
waiting or picking a smaller shape, and `both` says that raising the quota
alone would give this tenant nothing. A **limit of zero is a limit nobody set**,
so `left` is `null` rather than `0` — the two are different answers and must not
render as one.

`largestStartable` is never a sum. The cell's free memory does not add up into a
guest — a hundred nodes with 2 GiB each cannot run a 4 GiB machine — so its
cell-side input is the largest single node, and a node inside an open
maintenance window is not one of them.

All eight dimensions are always present, in a fixed order: a list that showed
only the interesting ones would rearrange itself between two reads of the same
screen.

### Taking a machine out of service on purpose

A maintenance window is a declaration about a stretch of time. Nothing flips
`schedulable` or `evacuate` on the operator's behalf — placement and evacuation
ask "is this node inside an open window right now" and act, so the operator goes
on being the only writer of those two fields, and a window that ends puts
everything back by *ceasing to be open*:

```
POST /api/v1/maintenance-windows
{ "id": "dimm-swap",
  "spec": { "node": "node-b", "startsAt": 1755600000000, "minutes": 60,
            "drain": false, "note": "swapping the failed DIMM in slot 3" } }
```

`drain: false` says nothing new is placed here and everything already running
stays put — a four-minute firmware update. `drain: true` also migrates the
guests away, by the ordinary evacuation machinery. One field, two intentions;
conflating them would move a fleet for a reboot.

Upcoming, open and over are **not stored**. They are arithmetic on `startsAt`,
`minutes` and the clock — a stored `state` would be a transient state, right
only while somebody was awake to write it.

Four windows are refused at the moment they are declared, because all four are
knowable then and the alternative is finding out at three in the morning: a
length of zero, a length past `20160` minutes, a window whose end is already
past, and one that overlaps another window on the same node. A start time in the
past is *accepted* — work that has already begun is a true thing to declare.

A node in an open window is rejected by placement like any other unsuitable
node, with the reason on the instance:

```
"rejections": [ { "node": "node-b", "why": "InMaintenance",
                  "detail": "out of service for another 40 minutes: swapping the failed DIMM in slot 3" } ]
```

Relative, not a clock time: this is read out of an instance's condition hours
after it was written, when "back at 03:00" no longer says whether that has
happened.

What a window will cost is answerable **before** it opens, which is the only
time the answer is any use:

```
GET /api/v1/nodes/node-b:explainMaintenance
{ "node": "node-b",
  "open": { "window": "maintenance-windows/dimm-swap", "startsAt": …, "endsAt": …,
            "minutes": 60, "drain": false, "note": "…", "opensInMinutes": null },
  "next": null,
  "draining": false,
  "willMove": [ { "instance": "projects/p1/instances/web-1", "to": "node-a" } ],
  "cannotMove": [ { "instance": "projects/p1/instances/gpu-1",
                    "why": [ { "node": "node-a", "detail": "…holds a passed-through device…" } ] } ] }
```

`cannotMove` is the half that decides whether tonight goes well: a guest that
cannot move is stopped when the machine is, and every node's verdict is on the
line rather than a flattened "no host found" — the remedy for "a generation too
old" and the remedy for "it holds a GPU" are nothing like each other.

A baseline is declared per node, and refused if the machine cannot reach it:

```
PATCH /api/v1/nodes/node-c   { "spec": { "cpuBaseline": "x86-64-v4" } }
400 { "error": { "code": "FAILED_PRECONDITION", "field": "spec.cpuBaseline",
                 "message": "node-c cannot present x86-64-v4: it lacks avx512f, avx512dq" } }
```

Refused here rather than at boot. `-cpu <level>,enforce` would catch it —
QEMU refuses to start a guest it cannot give the promised processor — but "the
guests on node-c stopped booting" is a long way from the sentence above.
Clearing the field is always allowed: going back to the host's own processor
asks nothing of the machine.

## Authentication

**Who may do what.** Authentication says *who*; the bindings on a project say
*may they*. A project carries `spec.bindings` — a role (`viewer`, `editor`,
`admin`) and the subjects holding it — and every object under that project is
governed by them. `viewer` reads, `editor` also writes, `admin` also changes the
bindings themselves; the last is kept apart so an editor cannot grant themselves
more than they were given.

Resources outside every project — nodes, pools, `ceph-clusters`, the `users`
collection, and the projects collection itself — belong to the cell, and only a
**cell operator** may touch them. A subject is a cell operator two ways: named
in the API's `--cell-admin`, **or** holding a user record whose
`spec.cellAdmin` is true. The first is configuration, not data: it is what a
fresh cell is bootstrapped from, and a permission stored inside the thing it
protects has no answer for the first request. The second is the ordinary,
storable grant, checked from the record the same way the config list is — so a
cell administers its own operators once it has a first one, without editing the
API's flags. `whoami` (below) reports the combined answer as `cellAdmin`.

A refused request is `403 PERMISSION_DENIED` with the same sentence whether the
resource is yours and forbidden or somebody else's and invisible — an error that
told the two apart would enumerate other tenants. A **list** is the exception
that proves it: it is filtered rather than refused, because a caller has no
permission on a collection as a whole and answering `403` would leave nobody able
to find the projects they do have.

**An agent must be a cell operator, and the consequence of getting that wrong is
silence.** A node agent reads two cell-wide collections — `nodes` and
`ceph-clusters` — and, by the rule above, a cell-wide list is *filtered* rather
than refused. So an agent authenticating as an ordinary subject is not told it
may not read them: it is handed `{"items": []}` and reads that as "there is
nothing to do". No error, no refusal, every node quiet, and a Ceph cluster stuck
at `Bootstrapping` for ever.

That is a configuration mistake this document cannot prevent, so the agent looks
for it instead: a node always exists for *itself*, so a node list that comes back
without the agent's own node in it has been filtered, and the agent says so once
at that point. The contract line is here because it is the thing to check first;
the run-time detector is what helps somebody who has already got it wrong.


`Authorization: Bearer <token>`. A token is verified one of two ways, tried in
order: it may be a **session token** minted by a password sign-in (below), or a
**static token** a service account or an agent holds — accepted by a fallback
verifier (a `--token-file` in development, an OIDC-issued JWT in production). The
console only ever sees a bearer token and must not care which kind it is; the
difference shows only at `whoami`, which reports whether a *session* stands
behind the token.

### Sessions and passwords

Passwords are exchanged for tokens, never sent on every request. The
`credentials` and `sessions` collections are unserved (above); these four routes
are the only way to reach them.

```
POST   /api/v1/sessions              # sign in: {username, password} -> {token, …}
GET    /api/v1/sessions/current      # whoami: who this token is, and what it may do
DELETE /api/v1/sessions/current      # sign out: end the session this token names
PUT    /api/v1/users/{id}/password   # set a password (a person)
POST   /api/v1/users/{id}/tokens     # mint a token (a service account)
```

- **`POST /sessions`** is the one route with no `Authorization` — it is what
  issues the token every other route requires. Body `{username, password}`; on
  success `201` with `{token, subject, displayName, cellAdmin, expiresAt}`. The
  `token` is returned **once** and only its digest is stored, so it cannot be
  recovered from the cell afterwards. Every failure — no such user, wrong
  password, disabled account — is the same `401` sentence, so the response is not
  an oracle for which usernames exist.
- **`GET /sessions/current`** (`whoami`) returns `{subject, displayName,
  cellAdmin, session}`. `cellAdmin` is the combined operator answer (config list
  or stored flag). `session` is true only when *this token* is a live session —
  a static token or service account reads `false`, because there is no session
  behind it for a sign-out to end.
- **`DELETE /sessions/current`** ends the session the caller presented, and only
  that one. It names no session: a route that ended a session by name would be a
  way to sign somebody else out. Idempotent — a token already gone is not an
  error.
- **`PUT /users/{id}/password`** sets a password. A cell operator may set
  anyone's; a subject may set their **own**, and only then must prove the
  *current* password in the body as `currentPassword` — without that, a stolen
  session would be a permanent account takeover. A project admin may set no
  one's: project membership is not a route to the cell. Changing your own
  password ends every *other* session you hold and keeps the one you are sitting
  in; an operator changing someone else's ends all of theirs, which is the point
  when an operator is shutting a door.

### Node agents, and the two ways they write

A node agent runs one of two ways, and the difference is a real trust boundary
rather than a configuration preference.

**Direct (the default).** The agent holds a cell-operator token and writes to the
store directly. The store's one-writer rule still refuses a write that is not
this node's, but the *identity* on that write is self-declared: anything holding
the operator token could write any node's status, so this is safe only because
the operator token is held by the operator's own agents. This is the
single-operator phase — a trusted deployment, not an enforced boundary.

**`--api` (per-node identity).** Each node is minted its own token when it is
registered, and the agent reads and writes **through the API** with that token
instead of touching the store. The token authenticates the caller as *that one
node*, and the API enforces what it may do:

- It may **read** the cell — a node needs tenant network config, images and the
  node list to run its guests, and reads them as it always has.
- It may **write status** only of objects it owns or was assigned — its own node
  object, and the instances, ports and attachments placed on it. A write for
  another node's object is refused `403 PERMISSION_DENIED`; it may not change any
  `spec`, and it may not create or delete anything.

So a compromised node holds a credential that can write only its own objects'
status, which the operator token never was.

**A node token is minted once, at registration.** A `POST` that creates a node
returns, alongside the operation, a `nodeToken` field — the only time the token
is shown. Only its digest is stored, in a collection the API does not serve
(beside `credentials` and `sessions`), so it cannot be recovered from the cell
afterwards, and a node cannot rotate its own credential: the digest is a `spec` a
node may never write. Deleting the node deletes its credential with it.

```
POST /api/v1/nodes → 202
{ "operation": "operations/op-3", "target": "nodes/node-a",
  "nodeToken": "…64 hex chars, shown once…" }
```

**A node reports status with a custom method**, AIP-136's `:reportStatus`, which
is the one write outside the `spec`-only PATCH surface because it is a different
caller doing a different thing:

```
POST   /api/v1/projects/p1/instances/i1:reportStatus
Authorization: Bearer <node token>
If-Match: "412"                 # the revision the agent read, for a compare-and-swap
{ "status": { … } }             # only the status is written; spec and meta are kept
```

A person, a service account or a cell operator has no node identity and so may
not use it: `status` is the agent's half, and the API does not get to be more
permissive than the store.

## Image families and where images come from

An image's name is its `sha256`, which is what makes fetching one verifiable and
what made every screen ask people to choose an operating system from
`images/sha256-cbf3e1f588f02f8d738dbecb…`. `spec.family` is the name a person
uses — `debian-13` — and `spec.version` says which one in the family.

An instance may name **`families/<family>`** instead of an image:

```
POST /api/v1/projects/p1/instances
{ "id": "web-1", "spec": { "image": "families/debian-13", … } }
```

That is resolved **once, when the instance is created**, and what is stored is
the concrete image. A guest never changes its operating system on a restart:
"always the newest" means new machines get the newest, not that existing ones are
rewritten under their owners. A project's own family beats the cell's, so a
tenant publishing `debian-13` of their own gets theirs.

`image-sources` keeps a family current:

```
POST /api/v1/image-sources
{ "id": "debian-13", "spec": {
    "family": "debian-13",
    "url": "http://cloud.example/debian-13-genericcloud-amd64.qcow2",
    "checksums": "https://cloud.example/SHA256SUMS",
    "everyMs": 21600000,
    "keep": 3
} }
```

The cell reads the checksums file, finds the line for the image's filename, and
publishes an image for that digest if it does not have one. Two different jobs
with two different trust models, and conflating them is the hazard:

* **the digest** is learned over `https://` with the certificate checked, and
  anything else is refused at this door — whoever can rewrite that answer chooses
  what every new guest in the cell boots;
* **the bytes** are then fetched by the node over whatever scheme the URL names,
  including plain `http://`, because a wrong byte gives a wrong digest and fails.

`keep` is retention, and it takes away only what it is safe to take: versions
this source published (matched by `url`, so a hand-made image sharing the family
is left alone), past the newest `keep`, that **no instance names**. A guest keeps
the bytes it was built from for as long as it exists, so its image survives
whatever `keep` says — and the source says on its own object how many it spared
and why, because "why does this family still hold eleven versions" should not
have to be worked out from a list of guests. Retention runs only after something
new was published: nothing can fall out of `keep` unless something came in.

A `SHA512SUMS` file is not a `SHA256SUMS` file. They sit side by side in every
distribution's directory, and a 128-digit line is refused rather than taken as a
digest — one that no bytes will ever match would be a source that looks healthy
and publishes something that cannot boot. The source says on its own object what
it found, or why it could not look.

## Service accounts

A caller that is a program is a `users` object with `spec.service` set. It has no
password and does not sign in; a cell operator mints it a token:

```
POST /api/v1/users/ci/tokens
{ "purpose": "nightly backups" }

200 { "token": "…", "user": "ci", "purpose": "nightly backups", "shownOnce": true }
```

**Shown once.** The platform keeps a digest, exactly as it does for a node's
credential, so a lost token is replaced rather than recovered — and a stolen
store holds nothing usable. Several tokens may exist for one account, which is
what makes rotation possible without a gap; `purpose` is what tells them apart
when one of them has to go.

Everything else about a service account is deliberately identical to a person's.
It is named in a project's `bindings` like anybody else, gets the same four
roles, and appears in the audit trail under its own subject — so "what may this
pipeline do here" is answered by reading the project.

Disabling one stops its tokens on the next request: the account is read back on
every call rather than copied into the credential, so an operator shutting a door
does not have to find the tokens first.

Before this, a service account was a line in a static token file: no object, no
bindings, nothing in the audit trail, and no way to revoke one but editing a file
and restarting the API — so the practical answer was to hand a program a person's
password.
