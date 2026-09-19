# Upgrading a cell

Written 2026-09-18, against the code as it stands. What it takes today to move
a cell to a new build, what the systems this replaces do about it, and the
design that follows — with what exists already marked, because most of it
does.

## What it takes today

A cell has two kinds of machine and two ways to update each, and none of the
four is connected to the others.

| machine        | how it was installed        | how it is updated today                                 |
|----------------|-----------------------------|---------------------------------------------------------|
| appliance node | flashed from the node image | `velstra-cloud-node update --image <raw>`, by hand, then a reboot by hand |
| Debian node    | `apt install ./velstra-cloud.deb` | `apt install ./velstra-cloud_<v>.deb`, by hand; `postinst` restarts what runs |

Every step is somebody's ssh session. Until `status.installed` (below) nothing
knew which build a node ran beyond `status.agentVersion`, which is the crate
version and says `0.1.0` for every build there has ever been. Nothing drains a node before its reboot
unless somebody remembers `spec.evacuate`. Nothing stops the second node being
updated while the first is still coming back. And nothing verifies the file
that was scp'd onto a machine is the one that was published — the A/B writer
takes a local path and trusts it, which its own doc comment names as a seam:

> A signed update channel — a release manifest, an Ed25519 signature over it,
> a subscription key, and a fetch whose image is verified against the
> manifest's SHA-256 before it ever reaches the slot-writer — is a documented
> seam, not an accident of omission: Sentinel's `src/update.rs` is the pattern
> to port.

## How the others do it

**Flatcar / CoreOS** — `update_engine` on every machine polls a *channel*
(Nebraska, speaking Omaha): a server publishes a manifest naming the payload
and its hash, the machine pulls it, verifies it, writes it into the inactive
partition, and reboots when a *lock* lets it — `locksmith` holds one reboot
slot in etcd, so a fleet reboots one machine at a time without anybody
sequencing it. Rollback is the previous partition.

**Talos** — `talosctl upgrade --image <ref>` per node, or the controller
rolling through them: cordon, drain, write the new image to the other slot,
reboot, wait for the node to rejoin, uncordon, next. Fails closed: a node that
does not come back healthy stops the rollout.

**Harvester / Rancher** — an `Upgrade` object. A controller downloads the
release, then per node: cordon, drain, upgrade the OS, reboot, uncordon,
verify, next. Everything visible on the object's status, per node.

**Kubernetes** — `kured` watches for a reboot-required marker on each node and
takes a cluster-wide lock before draining and rebooting one; the drain is the
platform's own eviction machinery, not kured's.

**Proxmox** — `apt` on each node, in the order the operator chooses. The
cluster does not sequence it, which is the thing operators most often get
wrong.

**OpenStack** — no built-in path; every deployment tool is a different answer
to this question.

What to take from that:

* **Nodes pull and verify; nothing pushes bytes.** Every one of the working
  designs has the machine fetch a payload named by a manifest and check its
  hash before touching a partition. This platform already pulls from a seed
  and verifies image digests at admission; the shape is native.
* **A manifest is the unit of trust.** Version, artefact, digest, signature.
  Sentinel has exactly this (`Manifest { version, image, sha256 }` under an
  Ed25519 `.sig`, per channel), tested and shipped.
* **One at a time, and fail closed.** A rollout is a controller reconciling a
  per-node status, advancing when the last node is healthy on the new build,
  and stopping — not continuing — when one is not. Flatcar's lock, Talos's
  sequencing and Harvester's `Upgrade` are three spellings of it.
* **Drain is the platform's own drain.** Nobody writes a second migration
  path for upgrades. `spec.evacuate` already creates one Migration per movable
  guest and leaves the reason answerable for the rest.
* **Rollback is the other slot.** A/B with boot counting gives the appliance
  rollback for free; a package system does not, and the honest answer there
  is to keep the previous package where the machine can reach it.

## The design

Three objects and one field, and two of the objects exist in spirit already.

### A `Release`: what there is to move to

```
releases/v0.2.0
  spec:
    version:   "0.2.0"
    url:       https://github.com/Velstra/cloud/releases/download/v0.2.0/
    publicKey: "-----BEGIN PUBLIC KEY-----…"      # who signs this channel
  status:
    manifest:
      image:    { file: velstra-cloud-node_0.2.0_amd64.raw.zst, sha256: … }
      package:  { file: velstra-cloud_0.2.0_amd64.deb,          sha256: … }
      installer:{ file: velstra-cloud-installer_0.2.0_amd64.iso, sha256: … }
    signature: Verified | Unsigned | Refused(why)
    conditions: [ Ready ]
```

`url` is a channel directory, exactly as Sentinel defines one: it holds
`manifest.json`, `manifest.json.sig`, and the artefacts. A GitHub release is
one such directory, which is why the release workflow will publish a signed
manifest beside `SHA256SUMS` — the same facts, in the shape a machine verifies.
`file://` is a channel too, for a cell that reaches nothing: an operator copies
the release directory onto the control plane's disk and points the `Release`
at it. That is the "upload" door, and it is a directory rather than an
endpoint because a 2 GB image through the API would be a second way to move
bytes that the platform then has to store, serve and clean up.

The controller fetches the manifest, verifies its signature under
`spec.publicKey`, and writes what it found onto the status. A release whose
signature does not verify is `Ready=False` with the reason and is not offered
to any rollout. An unsigned manifest is a state a release can be in — an
operator building locally has no key — and it is **refused for appliance
nodes**, because the A/B writer will only take a verified image, and accepted
for Debian nodes on the digest alone, because that is all a local `.deb` has
ever had.

### What a node runs: `status.installed`

```
nodes/peter
  status:
    installed:
      kind:    Appliance | Package | NixOS
      distro:  "Debian GNU/Linux 13 (trixie)"   # os-release, for a person
      version: "0.1.0+20260918.c571d71"
      slot:    "a"                               # appliance only
```

Reported by the node agent, because only the machine knows — and there are
**three** kinds, not the two the first draft of this document had.

* **Appliance** — `/etc/NIXOS` exists *and* the disk carries the A/B slot
  partitions the image lays down (`product.rs` knows their type GUIDs). Updated
  by writing the inactive slot. `version` is the image's own stamp,
  `/etc/velstra-release`, the same `<version>+<timestamp>.<revision>` the
  package calls itself; `slot` is read the way the A/B writer reads it, from
  the partition under `/dev/mapper/usr`.
* **Package** — no `/etc/NIXOS`, and `dpkg-query -W velstra-cloud` answers.
  Debian or Ubuntu; the same `.deb` on both, and `distro` says which for the
  person reading the row. Updated by installing the package.
* **NixOS** — `/etc/NIXOS` exists and there are no slots: a machine running
  the platform's NixOS *module* on an operating system the operator manages
  with their own configuration. **The cell cannot update it**, and a rollout
  that included one refuses it by name — "this machine is updated by its own
  configuration" — rather than skipping it silently, because a fleet that is
  half on a new build with one row nobody explained is the failure that looks
  most like success.

The distinction is decided on the machine, from the filesystem and the
partition table, never from a label somebody set: a `Package` node that an
operator mislabelled `Appliance` would have its inactive slot written on a
disk that has no slots. Without this field nothing can say which build a node
runs, which is the first thing an operator asks and the last thing a rollout
needs.

### What a node should run: `spec.wanted`

```
nodes/peter
  spec:
    wanted: "releases/v0.2.0"
```

One field, written by the rollout controller (or by hand, for one machine).
The node agent watches its own Node, as it already does, and when `wanted`
names a release whose version is not `installed.version`:

1. reads the `Release`'s status for the artefact of its own `kind` and the
   digest;
2. fetches it from the channel into a private scratch directory;
3. verifies the digest — and for an appliance the manifest signature, because
   the slot writer refuses anything else;
4. applies it: `run_update(image)` into the inactive slot, or
   `apt-get install --allow-downgrades ./…deb`;
5. reboots (appliance) or lets `postinst` restart what runs (Debian);
6. reports `installed` again on the way back up.

It does **not** cordon, drain or decide when. Those are the rollout's, and a
node that decided them for itself would be a node that reboots under its own
guests. The agent's whole contribution is: fetch, verify, apply, report — the
four things only the machine can do.

The apply is fail-closed at every step and each failure is a sentence on the
node's status (`conditions: Updating=False, reason, message`), never a log
line. A digest that does not match names both digests. A signature that does
not verify names the key. A slot write that fails names the slot.

### A `Rollout`: moving the cell

```
rollouts/spring
  spec:
    release:  "releases/v0.2.0"
    nodes:    { all: true }  |  { labels: {…} }  |  { names: [peter, horst] }
    evacuate: true              # migrate guests off each node before it reboots
    maxUnavailable: 1
    paused:   false
  status:
    phase:  Planning | Running | Paused | Done | Failed
    nodes:
      - node: peter
        from: "0.1.0+…681269c"
        to:   "0.2.0"
        phase: Pending | Cordoned | Draining | Applying | Rebooting | Verifying | Done | Failed
        message: "…"
```

The controller is level-triggered like every other one here. Each pass:

1. **Refuse to start** on a release that is not `Ready`, or on a node whose
   `kind` the release has no artefact for, or during a maintenance window that
   says the node stays up. All three are named on the rollout's status before
   anything is touched.
2. **Pick the next node** — at most `maxUnavailable` in flight, and **the
   control plane last**, because the controller runs there and its own reboot
   ends the rollout; the state survives in the store and the controller picks
   up where it was.
3. **Cordon** (`schedulable: false`), then **drain** if asked
   (`evacuate: true`) and wait until nothing movable remains — which is the
   existing evacuation controller doing what it does, and the guests that
   cannot move are listed on the rollout so the operator knows what will go
   down with the reboot.
4. **Set `spec.wanted`** on the node and wait.
5. **Verify**: the node is `Ready` again, reports `installed.version` equal
   to the release, and has heartbeated since the reboot. Then uncordon, and
   the node is `Done`.
6. **Stop** on a node that is not back within its budget: the rollout goes
   `Failed` with that node named and the rest untouched. A cell half on one
   build and half on another, with a reason, beats a cell that kept going.

`paused: true` finishes the node in flight and starts no other. That is the
knob an operator reaches for at the first sign of trouble, and it has to be a
field so that it survives the controller's own restart.

### Rollback

**Appliance**: the other slot. A node that fails three boots on the new slot
is back on the old one by itself — that exists. A node that boots but is
wrong is rolled back by pointing `wanted` at the previous release: the writer
puts it in the inactive slot, which *is* the old build, and switches back.

**Debian**: the previous package, with `--allow-downgrades`. The release
directory for the previous version is still where it was, so a `Rollout` at
the previous `Release` is the rollback. The postinst already restarts running
units on a version change, in either direction.

### What this deliberately does not do

* **No upload endpoint.** A channel is a directory; a directory on the
  control plane's disk is a channel. Moving bytes through the API would mean
  the API stores, serves and expires 2 GB files, which is a product in itself.
* **No automatic rollouts.** A `Release` appearing does not move anything.
  Somebody makes a `Rollout`, and the fleet moves when they say and as far as
  they say.
* **No mixed cells for ever.** A rollout that stops half way stops with a
  reason, and the cell is left half way — that is the correct state, and the
  reason is the operator's to act on, not the platform's to paper over.

### The install medium, from the same release

The installer a release holds is the image a new machine is installed from,
and once it is on the cell the cell can hand it out **cut for one machine**:
`nodes/<id>:installMedium` answers a one-time link to the ISO with that node's
join file appended after the ISO's last byte. Nothing in the ISO is rewritten
— the bytes a digest covers are the bytes that are there — and the installer
reads past the end the volume descriptor declares and finds the file, so the
machine joins as that node with nothing typed. One download, one stick. The
release channel is what makes this possible without an upload endpoint: the
bytes are already on the control plane, verified, for the upgrade path.

### As built

What shipped differs from the draft above in three places, each on purpose.

* **The manifest is `SHA256SUMS`**, which CI already publishes, rather than a
  `manifest.json` of its own; the release's `spec.url` is the channel and the
  version is read off the file names. A signed `manifest.json` is the next
  step, and a release that carries one will verify it before trusting the sums.
* **`spec.wanted` carries the artefact**, not only the release name: the file
  for this node's kind and its digest, chosen by the rollout from the release's
  status. The agent reads its own node and nothing else, and fetches from its
  own cell (`releases/<id>/files/<file>`) rather than from the channel — so a
  cell with no route out is upgraded from a directory somebody carried in.
* **The node reports on an `Updating` condition** rather than on fields of its
  own: True with a reason and a sentence while it works, False with `Failed`
  and the sentence when it could not — which is what the rollout reads to stop.

## What exists, and what is built next

| piece                                          | state                                             |
|------------------------------------------------|---------------------------------------------------|
| A/B slot writer, boot counting, rollback        | shipped — `velstra-cloud-node update`             |
| signed manifest, channel fetch, verification    | shipped in Sentinel — to port, seam already named |
| cordon, evacuate, maintenance windows           | shipped                                           |
| release publishing image, deb, iso, `SHA256SUMS` | shipped — CI, this branch                        |
| `status.installed` on a node — three kinds      | shipped — this branch                             |
| `Release` resource: channel read, files fetched and verified by digest | shipped — this branch      |
| `spec.wanted` + the agent's fetch/verify/apply   | shipped — this branch                             |
| `Rollout` resource + controller                 | shipped — this branch                             |
| console: releases, rollouts, *Upgrade* on the nodes board, a node's build | shipped — this branch |
| the install medium: a release's installer with a node's join file on its tail | shipped — this branch |
| CI: publish a signed `manifest.json`; the release verifies the signature | **next**              |

The order is the order of risk: reporting what a node runs is safe and
immediately useful; a release that verifies a manifest changes nothing on any
machine; the agent applying `wanted` is the first step that writes a slot,
and it is the one the VM checks have to prove before a controller is allowed
to set it on forty machines.

## Retry, rollback, and maintenance ownership

Only one update may run on a machine. Changing the target while an update is
running does not start a second installer. The agent durably records an attempt
before downloading or applying anything; an interrupted attempt or an appliance
that rolled back reports `Updating=False/Failed` instead of reinstalling the
same image on every boot. After investigating, clear `spec.wanted`, wait for the
agent to observe that change, then explicitly request a retry. An unreadable
attempt record also blocks updates until inspected.

A rollout's release is immutable; create another rollout for a different target.
Nodes already cordoned or evacuating are refused and retain their maintenance
state. The rollout labels the nodes it cordons with `velstra.io/rollout-owner` and
only releases its own cordons. An explicit operator edit to `schedulable` or
`evacuate` or `wanted` removes that ownership, preventing the rollout from undoing the edit.

Use the shared release-directory arrangement in `operations.md` when running
multiple control-plane replicas. Channel checksums protect against corruption;
until signed release manifests are supported, use an operator-controlled HTTPS
or local channel. Plain HTTP is unsuitable across an untrusted network.

Rollouts already in progress when installing this safety update may have nodes
without an ownership label. They stop rather than assume ownership of an existing
cordon. Inspect the node's maintenance state before clearing it and starting a
new rollout.
