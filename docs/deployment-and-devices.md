# Installing Velstra Cloud, and passing hardware into guests

Written 2026-08-24, against the code as it stands. Two decisions that turn out
to be one: **how a compute node is installed** determines **what hardware it can
give a guest**, because both come down to who owns the kernel.

## 0. Where we start

The cloud repository has no deployment artefacts at all — no flake, no package,
no systemd units, no container images. The node agent already expects systemd
(it starts each guest as its own unit, `velstra-cloud-nodeagent.rs:28`), but
nothing ships it. Today the answer to "how do I install this" is "you build it
with cargo", which is fine for the people writing it and for nobody else.

That is the gap this document closes.

## 1. The plane splits in two, and the halves want different things

**The control plane** — `velstra-cloud-api`, the controllers, the store, the
console — is a set of Rust binaries and an etcd. It touches no hardware, needs
no particular kernel, and does not care what it runs on. It should be easy to
put anywhere.

**A compute node** — the node agent, QEMU or Cloud Hypervisor, the fabric agent
with its eBPF/XDP data plane — is the opposite. It needs KVM, a kernel new
enough for the XDP features the fabric uses, IOMMU for passthrough, hugepages,
the right NIC drivers, and possibly out-of-tree drivers no distribution ships.

Treating these two as one deployment problem is the mistake to avoid. Almost
every hard question below is about compute nodes.

## 2. Three shapes for a compute node

### A. Nix appliance — immutable, A/B, signed

The same factory Sentinel uses: `nix build .#node-image` produces a flashable,
signed, verity-sealed image; A/B slots with boot counting; LUKS2 on the writable
partition; an installer ISO with the first-boot wizard.

What makes this cheap is that **it is already built, in the sibling repo**. The
A/B slot logic, the verity sealing, the update channel with per-channel signing
keys and subscription entitlement, the installer, the boot-counting rollback —
all of it exists and is VM-tested for Sentinel. A compute-node image is a
different package set in the same machinery, not a new machinery.

- **Kernel patches:** `boot.kernelPatches = [{ name; patch; extraConfig; }]`.
  Declarative, reproducible, and — this is the part that matters commercially —
  the patch becomes part of the image hash. "Which kernel is this box running"
  has a provable answer rather than a remembered one.
- **Weakness:** out-of-tree proprietary modules. There is no DKMS culture here.
  An NVIDIA vGPU host driver must be built against each kernel you ship, by you,
  forever. See §4.
- **Best for:** appliance-style and edge deployments, and any customer who is
  buying *tested stability* — the thing the subscription channel sells.

### B. Debian package on the customer's own Debian

A `.deb` for the node agent plus a metapackage pulling in QEMU, Cloud
Hypervisor and the fabric agent. The customer owns the kernel, the drivers and
the update cadence.

- **Kernel patches:** not yours. If the customer needs one, they carry it.
- **Weakness:** you cannot say what is underneath. Every support case starts
  with archaeology, and the "we tested this exact image" claim evaporates —
  which is precisely the claim the subscription is meant to sell.
- **Strength:** reach. It gets Velstra into shops that will not take a sealed
  box, onto hardware whose vendor only ships `.deb` drivers, and past
  procurement questions that a sealed appliance answers badly.
- **Best for:** existing Debian fleets, exotic hardware, and GPU vendor stacks.

### C. Debian-derived appliance ISO — the Proxmox shape

You ship a Debian-based installer ISO **and your own kernel package**. The
customer gets a familiar system; you still control the kernel and the drivers.

- **Kernel patches:** you maintain a kernel source package. This is exactly what
  Proxmox does, so it is well-trodden — and it means owning a kernel: tracking
  upstream security fixes, rebuilding, and publishing to a repository.
- **Best for:** the commercial middle ground, if hardware breadth becomes the
  blocker that A cannot answer.

### Recommendation

**Build A and B. Do not build C until something forces it.**

A is nearly free because Sentinel already paid for the machinery, and it is the
only shape that can honestly carry the tested-image subscription. B costs a
packaging pipeline and buys reach — and reach is what an infrastructure product
needs before it can sell stability.

C is the most expensive of the three to keep alive, because owning a kernel is a
standing obligation rather than a one-off. Reach for it only when a driver
situation makes A untenable and B unsupportable — and if that day comes, note
that C and A are not exclusive: the appliance can stay Nix while a Debian-based
variant exists for the hardware that needs it.

### The control plane, separately

Ship it as OCI images plus a plain systemd package. It has no hardware opinions,
so it should impose none: a customer running Kubernetes gets containers, a
customer running three VMs gets a package, and neither has to adopt the other's
world. The store (etcd) is theirs to run or ours to bundle — offer both, default
to bundled for the single-cell case.

## 3. Kernel patches, honestly compared

Both worlds can patch a kernel. They differ in what you get afterwards.

| | Nix appliance | Debian (B or C) |
|---|---|---|
| Mechanism | `boot.kernelPatches` in the flake | quilt series in a kernel source package |
| Declarative | yes — the patch is config | no — it is a build recipe you run |
| Reproducible | yes, and the patch is inside the signed image hash | weakly; you sign what you built |
| Cost per patch | a full kernel rebuild (cacheable in CI) | a full kernel rebuild plus a package repo to host it |
| Out-of-tree modules | rebuilt by you per kernel, no DKMS | DKMS, and vendor `.deb`s often just work |
| Who tracks CVEs | you, for the kernel you pin | you, once you fork the package |

The asymmetry worth internalising: **Nix is better at patches you write, Debian
is better at drivers somebody else wrote.** In-tree fix for an XDP bug — Nix,
easily, and it lands in a provable artifact. Proprietary vGPU host driver
tracking a vendor's release cadence — Debian, with much less pain.

That asymmetry is the actual argument for shipping both A and B rather than
choosing.

## 4. Passing hardware into guests

### What exists today

Nothing. No VFIO, no PCI concept, no device field on an instance or flavor. This
is a green field, which means it can be modelled properly the first time.

### The physics, so the model does not fight it

Passthrough is a kernel capability, not a distribution one: IOMMU enabled,
`vfio-pci` bound to the device, and — the constraint that shapes everything —
**the IOMMU group is the unit of isolation, not the device.** A group containing
the GPU, its audio function and a bridge is claimed whole or not at all.

Slicing is three different things:

- **SR-IOV / vGPU** (NVIDIA vGPU, AMD MxGPU): needs a vendor host driver. For
  NVIDIA that is a proprietary out-of-tree module tied to a licence server.
- **MIG** (A100/H100): hardware partitioning; a MIG instance still reaches a VM
  through vGPU or through passthrough of the MIG-backed device.
- **Time-slicing** via the Kubernetes device plugin: container-level, and
  irrelevant to a VM platform.

### The model

Following the three invariants the platform already enforces.

**A node reports what it has.** Extend the `status.devices` pattern that Ceph
already uses (`schema.rs`, the disk list): each PCI device reports address,
vendor:device, class, the driver currently bound, its IOMMU group, and every
other device in that group. Observation only — the node states facts, it decides
nothing.

**A device is offerable only when the whole group is free.** The Ceph disk rule
("offered only when it is provably empty") transfers exactly: if anything in the
IOMMU group is in use by the host or another guest, the device is not on offer.
Refuse with the group listed, so the operator sees *why* — that is a hardware
fact they can act on, not a platform mood.

**Instances ask for a class, not an address.** A raw `0000:41:00.0` is
node-specific, and an instance that names one can only ever be scheduled on one
machine. So: a `DeviceClass` resource names a set of matching devices across the
fleet (`vendor:device`, optionally a model label), and an instance spec says
`devices: ["gpu-a100"]`. The scheduler then has something to place.

**Placement treats it as exclusive and non-oversubscribable**, and
`explainPlacement` — which already exists — must be able to say "no node has a
free device of class gpu-a100; three exist, all claimed" rather than failing
silently.

**Live migration is refused, with the reason.** A guest holding a passthrough
device cannot migrate. State that as a named refusal at the API door
(`MigrationRefused: instance holds passthrough device`), not as a runtime
surprise. The platform already refuses rather than degrades everywhere else.

**A quota dimension** for devices, like the load balancer got.

### What phase one deliberately does not model

**No vGPU, no slicing, no weights.** Whole-device passthrough only. The reason
is the one the load balancer taught: a field the console shows and the platform
cannot deliver is worse than an absent field. vGPU needs a proprietary licensed
driver stack that nothing here has today; when the driver exists, the field
arrives in the same change as the code that reads it.

That also keeps phase one buildable on **either** installation shape, since
plain VFIO needs nothing a stock kernel lacks.

## 5. Suggested order

1. **Compute-node image (A)** reusing Sentinel's machinery — this is the
   deployment story and the subscription product in one move.
2. **PCI passthrough, phase one** — node device inventory, `DeviceClass`,
   instance spec field, placement, migration refusal, quota.
3. **Debian package (B)** — reach, once there is something worth reaching with.
4. **vGPU** — only against a paying customer, because it buys a licence
   agreement and permanent driver maintenance.
