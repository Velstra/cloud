# Installing Velstra Cloud

> Looking for "how do I get a cell running"? That is `docs/setup-guide.md` —
> control plane, then a machine, through the console or from a file. This
> document is the packaging underneath it: what each artefact is and what it
> imposes on its host.

Written 2026-08-24, implementing `deployment-and-devices.md`. Two deployment
problems, deliberately kept apart: the **control plane** is Rust binaries plus
an etcd and imposes nothing on its host; a **compute node** owns a kernel, KVM,
and (soon) an IOMMU, so it ships as a sealed appliance. Everything below comes
out of `flake.nix`.

## The compute-node image

```
nix build .#node-image     # → result/…/velstra-cloud-node.raw (signed, sealed)
nix build .#node-iso       # → result/iso/velstra-cloud-node-installer.iso
```

The image is built by the same factory as the Velstra Sentinel firewall
appliance (the Sentinel flake exports it as `nixosModules.applianceImage` /
`applianceIso`; this flake consumes it as an input): a dm-verity-protected
store whose root hash is sealed into a signed Unified Kernel Image, a volatile
tmpfs root, **two A/B store slots** with systemd-boot boot counting — a bad
update gets three boot attempts and then the machine is back on the previous
slot — and one writable partition, mounted at `/var/lib/velstra`, optionally
LUKS2-encrypted. Secure Boot ships with generated demo PK/KEK/db keys under
`/loader/keys/velstra-node/`; a real fleet enrolls its own.

On the image: the node agent, **QEMU and Cloud Hypervisor**, the Velstra
fabric agent binary, and a kernel booted with
`intel_iommu=on amd_iommu=on iommu=pt` — the passthrough phase of
`deployment-and-devices.md` needs the IOMMU on from the first install, because
a node that must reboot to *see* its devices cannot report them.

## Installing a node, start to finish

1. In the console (or `POST /api/v1/nodes`), create the node. The response
   carries `nodeToken` — a one-time credential, shown exactly once, that lets
   this node read its cell and write only its own status.
2. Boot the installer ISO. It drops straight into the wizard
   (`velstra-cloud-node install`): target disk or RAID set, optional LUKS2
   encryption of the data partition, DHCP or a static uplink, and then the
   hand-off that matters — **the control-plane URL and the node token**.
3. The wizard clones the sealed image onto the disk(s) and seeds the data
   partition (`node.env`, `node-token`, optional `network/`). Nothing needs to
   be reachable during the install; the wizard records.
4. First boot: the seed applies (hostname, network), the agent starts with
   `--api <url> --api-token-file …`, and the node appears in the console
   reporting its capacity. That is the whole onboarding.

A flashed-but-unseeded node parks instead of crash-looping: the agent unit's
condition names the missing file, `systemctl status velstra-cloud-nodeagent`
shows it.

Updates are A/B: `velstra-cloud-node update --image <new.raw>` writes the
inactive slot, re-types it, points systemd-boot at it with three tries, and a
clean boot blesses it. The **signed update channel** (manifest + Ed25519 +
subscription key, as Sentinel ships) is a documented seam in
`velstra-cloud-node/src/update.rs` — the slot writer refuses nothing it will
later need for it.

## The control plane, two shapes

**OCI images**, for the customer who already runs a scheduler:

```
nix build .#api-image && docker load < result
nix build .#controller-image && docker load < result
```

`velstra-cloud-api:<version>` listens on 8443 (REST + gRPC + console, one
port); `velstra-cloud-controller:<version>` serves Prometheus metrics on 9310.
Both take their store as `--store <etcd endpoints>` / `VELSTRA_STORE`. Neither
terminates TLS — put them behind the ingress the cluster already trusts.

**A systemd module**, for the customer with one machine:

```nix
{
  imports = [ velstra-cloud.nixosModules.controlPlane ];
  velstra.cloud.controlPlane = {
    enable = true;
    package = velstra-cloud.packages.x86_64-linux.velstra-cloud;
    listen = "0.0.0.0:8443";           # front with TLS before leaving loopback
    bootstrapAdmin = {
      username = "admin";
      passwordFile = "/run/keys/velstra-admin-password";
    };
  };
}
```

etcd is **bundled by default** (`store.bundledEtcd`), single-member, loopback —
the single-cell case from the design doc. A cell with its own etcd disables it
and sets `store.endpoints`. There is deliberately no in-memory option here: two
processes with two memory stores are two empty universes, which is exactly the
failure `velstra-cloud-dev` exists to prevent — and that one-process cell is
what `nix run .#dev` starts.

The node side has its own module, `nixosModules.node`, used by the image and
usable on any NixOS host: agent unit gated on the seed, both hypervisors on
PATH, the metadata dummy interface (the agent's 169.254.169.254:80 bind is
fatal by design), unlock unit for encrypted installs.

## Which machine is which: roles

Two questions that look like one, and conflating them is how a platform ends up
letting a machine promote itself:

* **What runs here** — the units this box starts. The machine's own business,
  decided at setup, written into its seed:
  `VELSTRA_ROLES=control-plane,hypervisor,pool`.
* **What the cell believes about it** — that it carries external traffic
  (`node.spec.gateway`), its labels, whether it is schedulable. Those live on
  the Node object and are an operator's to write.

`gateway` is deliberately **not** a setup role. A registration token exists so a
machine can *report*; one that could also declare its holder a gateway would be
a token that grants itself the cell's external traffic.

Roles are a set, not a choice: the smallest real cell is one box that is all of
them. Every unit is conditional on its own role being named in the seed
(`ExecCondition`), so a machine that has the package and no seed runs nothing —
and systemd shows those units as *skipped*, not failed, which is the difference
between "this box is not a pool" and "the pool agent is broken".

```
sudo velstra-cloud-node setup
```

asks region, cell, roles, and the role-specific answers (node id + token,
pool id + backend, store endpoints + other cells), writes
`/var/lib/velstra/node.env`, and then either names the units to enable (Debian)
or prints the NixOS module snippet (NixOS — units there are a declaration, and a
wizard reaching into them would be fighting the operating system).

**One seed, three systems.** The appliance, Debian and NixOS all read the same
file at the same path. The appliance decides the path: its `/etc` is on a
read-only verity store and its writable partition mounts at `/var/lib/velstra`.

## Debian

```
nix build .#deb        # → result: velstra-cloud_<version>_amd64.deb
sudo apt install ./velstra-cloud_*.deb   # not `dpkg -i`: that resolves nothing
sudo velstra-cloud-node setup
```

The package installs five binaries and five units and **enables none of them**;
`postinst` says so and points at the two ways in:

* `velstra-cloud-node quickstart` — one box that is the whole cell. Seed, units,
  node and pool objects, the one-time token, agents. Idempotent, and unattended
  when `VELSTRA_BOOTSTRAP_PASSWORD` is in the environment.
* `velstra-cloud-node setup` — say what this machine is and stop there, for a
  machine joining a cell somebody else runs. `--config <file>` takes the answers
  from a seed instead of asking, which is the unattended path. etcd, QEMU and Ceph are `Depends:`
and `Recommends:` resolved against Debian's own packages — a platform that
vendored its own etcd would be a platform whose security updates are its own
problem. Removing the package stops the units and leaves the seed: it holds
which cell this machine belongs to and the credential it was given, so taking it
would make reinstalling mean re-registering.

## Several cells, one address

A cell is the failure and scaling domain, so a machine belongs to exactly one
and growing means adding cells. Working across them is several control planes,
with one (or each) told where the others are:

```nix
velstra.cloud.controlPlane.cells = {
  "cell-2" = "https://cell-2.example:8443";
};
```

A request then lands in the cell holding the resource. Which cell owns what is
read from the **projects**, not from that map — the map only says where each
cell is. A project this installation has not heard of yet is answered locally
rather than refused: a router a few seconds behind must not turn propagation
delay into an error a tenant sees. Every forwarded request carries a hop marker,
so two routers whose directories disagree produce a wrong answer from a named
cell instead of a loop.

Regions are the coordinate above cells. Both are stamped into every object's
`meta.placement` at creation and neither can be changed afterwards — a resource
id that does not carry its cell cannot be routed to the right store.

## Storage: a third module, on purpose

```nix
{
  imports = [ velstra.nixosModules.pool ];
  velstra.cloud.pool = {
    enable = true;
    package = velstra-cloud;
    id = "nvme";                       # matches the `pools/nvme` object
    backend = "directory";             # or "ceph"
    store = "127.0.0.1:2379";
  };
}
```

A pool is **not** a machine, which is why this is not part of the node module:
several nodes reach one Ceph pool, one node may export three volume groups, and
tying storage to whichever hypervisor happened to be asked is how a volume
becomes unreachable the moment that node is drained. A box that is both a
hypervisor and a pool imports both modules and says so.

`directory` keeps volumes as qcow2 files and needs nothing but a writable
directory — with the property worth knowing: everything it holds is on one
machine, so a guest on such a volume cannot migrate to a node that cannot see
that directory. `ceph` keeps them as RBD images, and every node reaches every
volume.

Until this module existed the pool agent was a binary that nothing started, so a
cell built from this repository could hold a Pool object, a Volume object, and
no process anywhere that would put a byte on a disk.

## What the checks prove

```
nix build .#checks.x86_64-linux.dev-smoke        # the dev cell answers, seeded
nix build .#checks.x86_64-linux.console          # the console suite, real browser
nix build .#checks.x86_64-linux.node-image-boots # the sealed image boots: verity, parked agent, IOMMU cmdline
nix build .#checks.x86_64-linux.register         # operator creates node → token → agent reports (two VMs)
nix build .#checks.x86_64-linux.wizard           # the ISO wizard, driven prompt by prompt onto a blank disk
nix build .#checks.x86_64-linux.guest            # API → scheduler → agent → a real KVM guest prints a kernel banner
nix build .#checks.x86_64-linux.maintenance      # a window closes a node to new work, and expiry reopens it
nix build .#checks.x86_64-linux.storage          # a volume becomes a file, a backup becomes bytes, a restore reads them back
nix build .#checks.x86_64-linux.setup           # the setup wizard, answer by answer, with the seed read back
nix build .#checks.x86_64-linux.deb             # what is actually in the Debian package
```

`guest` needs nested KVM (the agent's QEMU backend is `accel=kvm`, on
purpose); on a host without it the check fails on the `/dev/kvm` assertion
rather than pretending TCG proved the same thing.

## Deliberate seams

- **Fabric endpoint**: the agent has a unit now (`velstra-fabric-agent`), and
  it starts when the seed names a fabric. What is still nobody's default is the
  *address*: `VELSTRA_FABRIC_CONTROL` has to be answered, because a service
  pointed at a controller nobody named would be a promise nothing keeps. A node
  whose seed has no fabric skips the unit rather than failing it — running a
  cell with no overlay is a real choice, and the skip says so in the journal.
- **Update channel**: slot writer shipped, signed channel not yet (above).
- **Sentinel input**: pinned to the public repository, so the flake evaluates
  for anybody — it used to point at a sibling checkout by absolute path, which
  is why none of these checks had ever run in CI. Working on both at once:
  `--override-input sentinel path:../sentinel` for the length of one command.
- **Local accounts**: the node image has none — fleet access is through the
  control plane, break-glass is the console on the ISO. If operations needs an
  on-box account, that is an installer question plus a module option, added
  together.
