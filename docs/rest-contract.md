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

Collections: `projects`, `instances`, `volumes`, `attachments`, `networks`,
`subnets`, `ports`, `images`, `nodes`, `operations`, `migrations`.

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

A field the platform already knows is not asked for twice. There are two today,
and they follow one rule: omitted means the API fills it in, stated means it
must agree, and a value that disagrees is refused rather than quietly
corrected — rewriting what somebody typed changes what an object says without
them asking.

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

## Computed fields

Three fields are answered on every read and stored nowhere, so none of them can
disagree with the world it describes:

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

`Authorization: Bearer <token>`. In development the API accepts a static token
from `--token-file`; in production the token is an OIDC-issued JWT and the
verifier is a trait implementation. The console only ever sees a bearer token
and must not care which kind it is.
