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

Collections: `projects`, `instances`, `migrations`, `volumes`, `snapshots`,
`pools`, `attachments`, `networks`, `subnets`, `ports`, `security-groups`,
`images`, `nodes`, `operations`.

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

Four fields are answered on every read and stored nowhere, so none of them can
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

## Authentication

**Who may do what.** Authentication says *who*; the bindings on a project say
*may they*. A project carries `spec.bindings` — a role (`viewer`, `editor`,
`admin`) and the subjects holding it — and every object under that project is
governed by them. `viewer` reads, `editor` also writes, `admin` also changes the
bindings themselves; the last is kept apart so an editor cannot grant themselves
more than they were given.

Resources outside every project — nodes, pools, and the projects collection
itself — belong to the cell, and only a subject named in the API's
`--cell-admin` may touch them. That list is configuration, not data: it is what a
fresh cell is bootstrapped from, and a permission stored inside the thing it
protects has no answer for the first request.

A refused request is `403 PERMISSION_DENIED` with the same sentence whether the
resource is yours and forbidden or somebody else's and invisible — an error that
told the two apart would enumerate other tenants. A **list** is the exception
that proves it: it is filtered rather than refused, because a caller has no
permission on a collection as a whole and answering `403` would leave nobody able
to find the projects they do have.


`Authorization: Bearer <token>`. In development the API accepts a static token
from `--token-file`; in production the token is an OIDC-issued JWT and the
verifier is a trait implementation. The console only ever sees a bearer token
and must not care which kind it is.
