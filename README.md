# Velstra Cloud

An IaaS control plane: OpenStack's scope and tenancy, Proxmox's operational
feel, hyperscaler mechanics underneath. The network is
[Velstra fabric](https://github.com/Velstra/fabric), the eBPF/XDP SDN this is
built around.

**Status: early.** The model, the store and the reconciliation decisions are
real and tested. The API, controllers, node agent and console are being built on
top of them. Nothing here is production software yet.

## The one idea

Every "stuck" object in the systems this replaces — an instance in `BOOTING`
that never boots, a volume in `attaching` that cannot be attached anywhere, a
load balancer in `PENDING_UPDATE` that no longer has anything to update — is the
same bug wearing different clothes: **two writers disagreeing about one field,
and a command that died between sender and receiver.**

So this platform has no commands. It has:

| | |
|---|---|
| `spec` | what was asked for. Only a controller writes it. |
| `status` | what is. Only the agent that owns the object writes it. |
| `generation` / `observedGeneration` | whether the second has caught up with the first. |
| `conditions[]` | why, if it has not. |

There is no third state. Nothing means "in progress" — a change that has not
landed is simply a generation the world has not observed yet, which is a fact
anybody can read off the object rather than a lock somebody has to clean up.

Three invariants make that hold, and they are enforced rather than documented:

1. **One object, one writer.** `velstra-cloud-model/src/access.rs` judges every
   write against what is stored; `velstra-cloud-store/src/typed.rs` runs that
   judgement before the compare-and-swap. A controller that touches `status` is
   refused, whatever code path it came from.
2. **No transient states.** Look through `resources.rs`: no status field can
   express "half way". An instance the node has not reported on is `Unknown` —
   an honest absence of knowledge, replaced by an observation — not `PENDING`,
   which is a claim about the world made by something that cannot see it.
3. **Level-triggered and idempotent.** Every decision is a pure function of
   `(spec, observed)` in `reconcile.rs`: no clock, no store, no ordering. Running
   it twice is running it once. A process that dies mid-flight leaves nothing to
   resume, because nothing was ever in flight.

What it buys, concretely: a resync over a converged cluster performs **zero
writes**, a controller can be killed at any instant, and "why is this not
ready" is answered by the object itself.

## Why not just use Kubernetes' apiserver

The API machinery there is well designed and worth copying — `resourceVersion`,
optimistic concurrency, watch with a starting revision, finalizers, conditions,
server-side apply. This repository copies the patterns and not the
implementation, because adopting the apiserver means adopting the whole
operational surface this project exists to avoid, and its own ceiling at a few
thousand nodes.

## Why the cell is in every identity from the first commit

A cell is the failure and scaling domain: one cell must never take a region with
it. It is also the only architectural decision here that cannot be retrofitted —
a resource id that does not carry its cell cannot be routed to the right store
once there is a second one. So `Placement { region, cell }` is in `Meta` today,
while there is exactly one cell and it looks like ceremony.

## Layout

| Crate | What it is |
|---|---|
| `velstra-cloud-model` | Resources, access rules, and every decision as a pure function. No I/O at all — the hard parts are testable without a cluster. |
| `velstra-cloud-store` | `get / list / watch / compare-and-swap / delete` behind a trait, with an in-process MVCC backend. etcd is the first real backend; the trait is what makes FoundationDB a file rather than a rewrite. |
| `velstra-cloud-proto` | Protobuf as the source of truth; gRPC native, REST generated over the same handlers. |
| `velstra-cloud-api` | The surface in `docs/rest-contract.md`, AIP conventions, long-running operations as resources. |
| `velstra-cloud-controller` | One reconcile loop, several thin controllers over the pure functions. |
| `velstra-cloud-nodeagent` | One stream per node, a `Vmm` trait, local metadata and DHCP. Load per node is O(its own objects). |
| `velstra-cloud-console` | The web interface, self-contained, no external fetches. |

## Documents

- `docs/rest-contract.md` — the HTTP surface, fixed. The API serves it and the
  console consumes it; neither changes it alone.

## Licence

AGPL-3.0-or-later, matching the data plane. See the workspace `Cargo.toml`.
