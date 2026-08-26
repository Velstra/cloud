# Velstra Cloud

An IaaS control plane: OpenStack's scope and tenancy, Proxmox's operational
feel, hyperscaler mechanics underneath. The network is
[Velstra fabric](https://github.com/Velstra/fabric), the eBPF/XDP SDN this is
built around.

## Quickstart

```
git clone https://github.com/Velstra/cloud && cd cloud
nix run .#dev
```

One process, one in-memory store, and a whole seeded cell in it: the command
prints the console URL and the token to sign in with, and the console has a
project, a network, volumes and instances to show — one of them deliberately
unschedulable, so the *why not* surfaces are populated too. No `/dev/kvm`
needed; the dev cell's hypervisor is fake and no guest is real, which the
banner says out loud.

Installing the real thing — the immutable compute-node image, its installer
ISO with the first-boot registration wizard, and the control plane as OCI
images or a systemd module — is `docs/install.md`.

**Status: the invariants and the model are the settled foundation; the API,
controllers, node agent and console are built on top and exercised by CI — but
this is not yet production software proven on hardware.** The model, the store
and the reconciliation decisions are real and tested (the three invariants
below are enforced, not documented). Built on top of them today:

- a **gRPC + REST API over one set of handlers** with bearer-token auth,
  role-based authz (Read / Write / Administer, project as the tenancy unit),
  **per-node identity** (a node token may only read its cell and write the
  status of its own objects), **quotas** enforced at admission and only counted
  (never incremented) thereafter, and an hourly **session sweeper**;
- a **controller set** over the pure reconcile functions — instances, volumes,
  snapshots, attachments, networks, routers, floating IPs, subnets, ports,
  security groups, images, Ceph clusters, migrations, operations;
- a **node agent** with a `Vmm` trait and **QEMU + Cloud Hypervisor** backends,
  **Ceph/RBD** storage, and **live migration** (QMP pre-copy);
- a self-contained **web console**.

CI runs fmt/clippy/`cargo test`, a **contract-drift gate** (every served
collection must appear in `docs/rest-contract.md`), the console in a real
headless browser against the contract, and **two real-VM boot tests**, one per
VMM backend, on a runner with `/dev/kvm`.

Honest caveats. The default posture is a **single-operator trust model** —
`docs/rest-contract.md` is explicit that Direct-mode agents holding the operator
token are a trusted deployment, not an enforced boundary; the hardened per-node
`--api` path is what scopes each node to its own objects. Image signatures are
carried on the wire but **deliberately rejected** until something verifies them
(a field kept for a future one-commit change, not a gap left open). And the
real-hardware exercise so far is the two guest-boot backends in CI — live
migration and a Ceph cluster are covered by unit/integration tests, not yet run
on metal. The **LoadBalancer kind** fronts the fabric's own L4 balancer (a VIP
DNAT-rewritten in XDP with connection tracking); it carries no weights, no
algorithm knob and no health checks, deliberately, because the data plane has
none — a field nothing reads is a claim the platform cannot keep.

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
| `velstra-cloud-nodeagent` | One stream per node, a `Vmm` trait, local metadata and DHCP. With `--api` a node is handed only its own objects and the API serves every node from one watch, so load per node is O(its own objects). |
| `velstra-cloud-console` | The web interface, self-contained, no external fetches. |

## Continuous integration

`.github/workflows/ci.yml` runs three lanes on every push and pull request, and
again nightly:

| Lane | What it runs |
|---|---|
| `check` | `cargo +nightly fmt --check` (the `rustfmt.toml` here is nightly-only, and stable rustfmt would degrade the gate to a no-op), `cargo clippy --all-targets -D warnings`, and `cargo test --workspace --locked`. |
| `console` | `velstra-cloud-console/tests/console/run.sh` — the whole console driven in a real headless browser against the contract server. |
| `guests` | The two tests that start a real virtual machine, one per backend. |

Some of those tests skip themselves when the machine cannot run them, which is
right on a laptop and wrong in CI — a green run that skipped everything proves
nothing. So CI installs the fixtures instead of accepting the gaps: `etcd` (the
store conformance suite fails rather than skips without it, and
`VELSTRA_ETCD_OPTIONAL` is deliberately never set in CI), QEMU, Cloud Hypervisor,
and the two Alpine guests the backends need — a BIOS cloud image for QEMU's own
firmware, a kernel and initramfs for Cloud Hypervisor, which has no firmware.
The `guests` lane then reads its own output and fails if a test printed
`skipping:`.

To run the same things here, install `etcd`, `qemu-system-x86_64`,
`cloud-hypervisor`, `protobuf-compiler` and a Chromium, and put a guest image at
`/tmp/vq/alpine.raw` plus `vmlinuz-virt`/`initramfs-virt` under `/tmp/vq/boot`
(or point `VELSTRA_TEST_IMAGE` and `VELSTRA_TEST_BOOT` elsewhere).

## Documents

Start with the first one if you have never run this.

- `docs/quickstart.md` — one machine, from nothing to a guest you can log into,
  with pictures of the console it is describing.
- `docs/rest-contract.md` — the HTTP surface, fixed. The API serves it and the
  console consumes it; neither changes it alone.
- `docs/deployment-and-devices.md` — how a node is installed and what hardware
  it can hand a guest: the decision record behind the flake.
- `docs/setup-guide.md` — from nothing to a machine running guests, twice: by
  hand through the console, and from a file with nobody watching.
- `docs/install.md` — installing it: the node image, the installer ISO and its
  wizard, the Debian package, machine roles, registration, and how several
  cells answer at one address.
- `docs/operating.md` — running it once it is up: which question to ask before
  acting, taking a machine out of service, recovery, overcommit, placement
  groups, and which copy survives which loss.
- `docs/cpu-heterogeneity.md` — a cell of mixed processor generations: what can
  migrate where, what baselining costs, and how a third generation is added.

## Licence

AGPL-3.0-or-later, matching the data plane. See the workspace `Cargo.toml`.
