# Joining a cell

> How a machine becomes part of a cell — the first one and every one after —
> and why the shape is what it is. `setup-guide.md` is the walkthrough; this is
> the design behind its shortest path.

Written 2026-09-17, after installing a two-machine cell by hand twice.

## What it took, before

Adding the second machine meant carrying six facts from the control plane to
the new box, each by a different route:

| fact | how it travelled |
|---|---|
| the API's URL | typed — and it had to be a name the certificate carries, so `https://<ip>:8443` failed verification while the hostname needed DNS |
| the API's certificate | `scp` of `/var/lib/velstra/tls/cert.pem`, then a path typed into the wizard |
| region and cell | typed, or asked of the cell — which the installer ISO cannot reach |
| the node's id | typed, and it had to match the object an operator created first |
| the registration token | 64 hex characters shown once, in a console that had lost them |
| the hypervisor | a menu, with the only answer that can open Ceph not marked as such |

Every one of those is a place a fleet loses a machine. And the React console
made it worse than the old one: a create answers `202 { operation, target,
nodeToken }`, and nothing read the third field, so the token shown once was
shown to nobody.

Two more things were true. The sealed appliance could not be **the first**
machine — `quickstart` enables units with `systemctl` and refuses NixOS, and
nothing on the image created the Node and Pool objects at first boot. And a
hypervisor installed before Ceph existed could not use Ceph afterwards without
somebody copying `ceph.conf` and a keyring onto it by hand, because the platform
distributes the cluster's SSH key to nodes and nothing else.

## How the others do it

**Proxmox VE** has one ISO. The first node *is* the cluster. Every later node
pastes a *join information* blob (addresses, fingerprint) and the first node's
root password, from a web page on either side. Ceph is a later, per-node
"Install Ceph" button, then monitors and OSDs by clicking.

**Harvester** has one ISO with *create* / *join* at first boot; joining needs
the VIP and a cluster token chosen at creation. One token admits every machine
for ever.

**Incus / LXD** mints a **per-node join token** on an existing member
(`incus cluster add <name>`): addresses, the cluster certificate's fingerprint
and a secret, base64 in one string. The joiner fetches the certificate and
checks the fingerprint.

**k3s** is `K3S_URL` plus one cluster-wide token. The simplest, and the one with
no rotation story.

**Talos** flips the direction: a booted machine listens in maintenance mode and
the operator *pushes* a configuration at it.

What to take from that:

* Per-node tokens are the right security shape, and this platform already had
  them — a token is issued *for* one Node object, can be re-issued additively,
  and cannot promote its holder to a gateway.
* One pasteable blob is the right UX shape **where there is a paste buffer**.
  Proxmox and Incus both arrived at it, and both hand it to a *browser*. An
  installer on bare metal has a console and a keyboard; see "Getting the token
  onto the machine".
* Create-or-join at first boot is the right installer shape.
* Ceph as a later, UI-driven step is right — and it is only honest if the
  platform hands every hypervisor the client configuration it needs, or the
  "later" is somebody's `scp`.

## The join token

```
velstra1.<base64url(JSON)>
{ "v": 1,
  "region": "eu-central", "cell": "cell-1",
  "node": "peter",
  "urls": ["https://10.10.10.8:8443", "https://horst:8443"],
  "ca": "-----BEGIN CERTIFICATE-----…",
  "token": "<64 hex>",
  "pool": { "id": "local-2", "token": "<64 hex>" } }      // only for a pool
```

**Self-contained, on purpose.** The installer ISO seeds a filesystem that has
never booted, on a machine that may have no network yet: *nothing needs to be
reachable during the install*, and that promise is worth more than a shorter
string. So the token carries the certificate itself rather than a fingerprint
to check it against — no unverified first contact, no extra endpoint, and a
seed that is correct before the cable is plugged in. A P-256 self-signed
certificate makes the whole thing about 1.3 KB.

That size is also the token's one real weakness, and the next section is about
it: a thousand characters is nothing to move between programs and impossible
to move through a keyboard.

**Minted where the facts live.** `POST /api/v1/nodes` and
`POST /api/v1/nodes/<id>:issueCredential` answer `joinToken` beside
`nodeToken`; pools likewise. The API needs two things it did not know about
itself: the certificate it serves (it has the path) and the URLs a stranger
should try. The second is `--advertise` / `VELSTRA_ADVERTISE`, and it is
written by the same code that chooses the certificate's names — one list,
consumed by `rcgen` and by the token, so a URL in the token is always a name
the certificate verifies for. Nothing parses X.509 to find out.

**Consumed in two places, one way.** `velstra-cloud-node setup --join <token>`
on a machine that already runs Debian; *Join a cell* in the installer ISO. Both
write the certificate beside the seed as `api-ca.pem`, the seed with
`VELSTRA_API_CA` pointing at it, and the credential files — and nothing else
is asked. The hypervisor is `qemu` unless the token says the cell has no Ceph,
because a joiner cannot know what storage the cell will grow.

## The first machine

`install` opens with three doors:

```
[1] The first machine of a new cell   control plane, hypervisor, storage
[2] Joining a cell                    paste the join token from the console
[3] Custom                            the questions, one by one
```

Door 1 asks what `quickstart` asks — the administrator's password and where
the API listens — and seeds all three roles. What `quickstart` did *after* the
seed has to happen at first boot, so two oneshots do it, each an idempotent
subcommand of the same binary:

* `velstra-cell-tls` runs before the API: makes the certificate with the
  machine's *real* addresses (a DHCP lease is not known at install time), and
  writes `VELSTRA_TLS_*` and `VELSTRA_ADVERTISE` into the seed if they are not
  there. `tls::ensure` keeps a certificate that exists, so a real one dropped
  in later survives every boot.
* `velstra-cell-bootstrap` runs after the API answers: creates the Node and
  Pool objects for this machine and writes their tokens. Every step is the
  same idempotent step `quickstart` takes; the two are one function now.

Both are gated on `has-role control-plane`, like everything else on the image. And
because the appliance has no local accounts, `login:` would be the last thing a
first boot shows — so `velstra-cell-tls` also writes a banner to the screen:
where the console is, and the certificate's fingerprint to check the browser's
warning against.

**Born with Ceph, or not.** Door 1 also asks what storage the first machine
has: a directory on its install disk, or Ceph on the other disks in the box —
the USB stick included, since the installer was asked for exactly those disks.
The names travel in the seed (`VELSTRA_BOOTSTRAP_CEPH_OSDS=vdc`) and
`velstra-cell-bootstrap` starts the node agent, waits for its first inventory,
and asks for a one-node cluster on the disks *as the node names them* —
`/dev/disk/by-id/…`, not the kernel name the installer saw, because an OSD
spec naming a path the node does not use asks for an OSD it will never make.
One monitor and replication 1 is what one machine can be; `quorum_advice` says
so, and the cell grows from here by adding monitors and OSDs to the same
object. Choosing the directory instead closes no door: Ceph is added from the
console once the cell has the machines and the disks for it.

## Ceph afterwards

Creating a cluster was already a later step — `ceph-clusters` is an object an
operator makes whenever, and the deployment blocks by name until `cephadm` is
present. Two things were missing for a hypervisor that was installed first:

**Client configuration is published, not copied.** The bootstrap node, once
the cluster is up, asks it for `ceph config generate-minimal-conf` and
`ceph auth get-or-create client.velstra` with read/write on the platform's
pools, and reports both. The controller publishes them on
`CephClusterStatus`, the way it publishes the SSH key. Every node agent writes
what it reads to `/var/lib/velstra/ceph/` and opens RBD images with it unless
the seed named something else. The keyring is a secret and is redacted for
everybody but a cell operator, which is who can read `ceph-clusters` at all.

**The appliance carries `cephadm` and `podman`.** The platform still installs
nothing on its own — cephadm pulls the Ceph containers only when an operator
has asked for a cluster — but a flashed machine has no package manager to get
cephadm from, and *the image has to carry what it cannot fetch*.

## Getting the token onto the machine

The token removes every hand-copied fact between the control plane and the
installer, and then asks somebody standing at a machine to type a little over
a thousand characters of base64. That is not a hand-off; it is the same work
in a worse place. Proxmox is not a counter-example — its blob goes into a
browser, which has a paste buffer. A console does not.

There are exactly three honest answers, and they are for three different
situations rather than three sizes of fleet.

**A file on anything plugged in** — shipping, `joinfile.rs`. The wizard looks
at every partition the kernel knows, mounts it read-only for as long as it
takes to read one small file, and offers what it found by the *machine the
token is for*: `velstra/join for peter in cell cell-1 (on /dev/sdb1)`. So the
ISO is written once and the per-machine part is one text file — on a second
stick, or in the space after the image on the same one. `velstra-cloud-node
setup --join-file` is the same thing for a machine that already runs Debian,
and it keeps the token out of `ps` and out of the shell history. Nothing about
the protocol or the secret changes: a token is exactly as secret on a stick as
it is in a terminal's scrollback, and the install still needs nothing to be
reachable.

Mounted with `noload` first and the plain form as the fallback: `mount -o ro`
is not "do not write" — on a dirty ext4 the kernel replays the journal, and
writing to a disk somebody else's operating system owns, before this installer
has asked anybody anything, is not a thing to do. `noload` says do not, and
vfat rejects it, which is why there are two attempts and not one.

**The platform serves the medium** — for when nobody is watching. The seed for
a node is a small document the API already holds every fact for, so
`nodes/<id>` can answer it directly, and for Debian and Ubuntu the useful
artefact is not an image at all but the little thing each of them already
wants: a cloud-init NoCloud seed, a preseed or autoinstall file, or one
`setup --config <url>` line on a box that already boots. This is the PXE,
Terraform and configuration-management door, and the one that works on a
network that is not up yet, because the seed travels with the medium.

What not to build: a 2 GB image per node with the token baked in. The
appliance image is signed and A/B-sealed, so baking anything per-machine into
it changes its hash and destroys the property that makes it worth having — one
artefact for a whole fleet. Image plus a few kilobytes of seed is the same
convenience without either cost.

**Approval in the console** — for when somebody is. The machine boots, takes a
lease, generates a keypair, announces itself to a cell address (the one short
thing anybody types, and a DHCP option can carry it), and shows a short
fingerprint on its screen. A `Pending` row appears in the console with the
same fingerprint and what the machine reported about its hardware; the
operator compares, picks the roles, and approves. Nothing secret is typed —
the comparison *is* the authentication, in both directions, because the
machine also shows the fingerprint of the certificate it was served and the
banner on the control plane prints that cell's own. This is MAAS's enlistment
and Proxmox's join in one gesture, and it scales to a rack: twenty machines
from one ISO are twenty rows to approve.

It needs the network at install time, which the token deliberately does not,
so it is a door beside the others and not a replacement for them.

## What this does not do

* **No attestation.** Introduction is a shared secret, moved once. A machine
  that proves who it is in hardware and draws its own credential is what the
  large clouds do; nothing here is shaped against it, and nothing here does it.
* **No signed update channel.** The A/B slot writer ships; the manifest and
  key do not (see `install.md`). A fleet update is still images by hand.
* **No push.** Talos's direction — an operator pushing configuration at a
  listening machine — is a different platform. This one pulls, from a seed.
* **No enrolment yet.** The approval flow above is designed and not built:
  there is no `Enrollment` resource, no unauthenticated announce, and no
  pending list in the console. Until there is, a token on a medium is the way
  to avoid typing one.
