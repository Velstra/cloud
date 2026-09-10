# The command line

`velstra` is the cell's own client. It ships in the same package as the
control plane, so a machine that runs a cell can already talk to one.

Everything it knows comes from the same description of the platform the web
console and the OpenAPI document are built from. A collection the API serves
is a noun here, with the console's own columns:

```
$ velstra get instances
NAME   STATE    ASKED    VCPU  MEMORY  NODE   ADDRESS      AWAITING RESTART
reise  Running  Running  1     1024    peter  10.19.136.2  —
test   Running  Running  2     4096    horst  10.19.136.3  —
```

## Pointing it at a cell

Four settings, all of which can be flags or environment variables:

| | |
|---|---|
| `--api`, `VELSTRA_API` | where the cell answers, e.g. `https://cell-1.example:8443` |
| `--token`, `VELSTRA_TOKEN` | a bearer token; mint one for a service account with `POST /api/v1/users/<id>/tokens` |
| `--project`, `VELSTRA_PROJECT` | the project to work in, for the collections that live in one |
| `--insecure`, `VELSTRA_INSECURE` | trust a certificate this machine cannot verify — what a fresh install serves |

A cell-scoped collection — nodes, pools, projects, images — needs no project.
A tenant's — instances, volumes, networks — is refused without one, by name,
rather than guessing.

## Reading

```
velstra collections                     # what this cell serves
velstra get nodes                       # a table
velstra get instances --json            # the objects themselves
velstra get instances db-1              # one object
velstra get instances --labels env=prod # narrowed by label
```

A list follows the API's paging to the end, so what comes back is the
collection and not its first page.

## Writing

```
velstra create volumes data-1 --set sizeGib=100 --set pool=ceph
velstra patch instances db-1 --set desiredState=Stopped
velstra delete volumes data-1
```

`--set` takes `key=value`, repeatable. A value that parses as a number, a
boolean or JSON is sent as that, so `sizeGib=100` is a number and
`schedulable=false` a boolean. A dotted key nests, the way the fields are
named everywhere else:

```
velstra create instances web-1 \
  --set image=families/debian-13 \
  --set flavor=m1-small \
  --set 'networks=["projects/p1/networks/default"]' \
  --set placementPolicy.spread=Required
```

A create answers with the operation to wait on and the name the object was
given, not with the object: it exists and has not converged yet.

A change is made against the version it was read at, so two people editing
one object at once are told rather than one of them silently losing.

In a script, give a create a key so it can be retried:

```
velstra create instances web-1 --idempotency-key "$JOB_ID" --set flavor=m1-small
```

Run it again with the same key and it answers with the first run's operation,
having made nothing new. Without one, a create whose answer was lost can only
be resolved by looking, which is a race.

## Asking why

```
velstra explain instances db-1 placement    # where it went, or who refused and why
velstra explain instances db-1 migration    # which machines could take it
velstra explain projects p1 quota           # what is left, and the largest guest that could start
```

## What it does not do

It keeps no state but the token: no contexts, no cached objects, no notion of
"the current cell" other than what you passed it. A tool that remembered
which cell you were pointed at is a tool that deletes the wrong thing on the
day you forget.
