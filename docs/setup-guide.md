# Setting up a cell

From nothing to a machine running guests, twice: once by hand through the
console, once from a file with nobody watching. Both end at the same place —
the same seed, the same units, the same objects — because they are the same two
halves in a different order.

**Running this at home, on one machine?** Start at §0, which is the whole thing
for a single box. A cell of one is a supported shape, not a cut-down one: the
same roles, the same objects, all on the same host.

## The two halves

Every machine in a cell is described in two places, and keeping them apart is
what makes a registration token safe:

| | Where it lives | Who writes it | Examples |
|---|---|---|---|
| **What runs here** | the machine's seed, `/var/lib/velstra/node.env` | the installer, or a file you wrote | `VELSTRA_ROLES=hypervisor,pool` |
| **What the cell believes about it** | the Node object in the API | an operator, through the console | `gateway`, labels, `schedulable`, `cpuBaseline`, `vcpuOvercommit` |

A machine cannot write the second. Its token exists so it can **report** —
capacity, health, what it is running — and a token that could also declare its
holder a gateway would be a token that grants itself the cell's external
traffic.

So the order is always: **the cell is told a machine is coming** (and hands out
a token), then **the machine is told what it is** (and uses the token).

---

## 0. One machine, all of it

**One command, if this box is the whole cell:**

```
sudo apt install etcd-server qemu-system-x86 qemu-utils
sudo apt install ./velstra-cloud_*.deb
sudo velstra-cloud-node quickstart
```

That writes the seed, brings up the control plane, creates the node and pool
objects, moves the node's one-time token, and starts the agents. It is
idempotent — run it again after a failure rather than reinstalling — and it
refuses on NixOS, where the module below is the answer instead.

Unattended, for config management:

```
VELSTRA_BOOTSTRAP_PASSWORD=… velstra-cloud-node quickstart \
  --node home-1 --listen 0.0.0.0:8443
```

The password comes through the environment rather than a flag: an argument is
in `ps` for every user on the machine, and this one is the cell's first
administrator.

The rest of this section is the same thing spelled out, for when you want to
know what it did or do it differently.

### The long way

The smallest real installation is **one box that is every role**, and it is a
supported shape rather than a degraded one. If that is what you want, this
section is the whole guide; the rest is for when you add a second machine.

On Debian:

```
sudo apt install ./velstra-cloud_*.deb
sudo velstra-cloud-node setup
```

Answer `1 2 3` at the roles question — control plane, hypervisor and storage
pool together. Then enable the four units it names, create the node and pool
objects, and move the node's one-time token onto the machine. That last part is
what `quickstart` above does for you.

On NixOS, the same thing declared:

```nix
{
  imports = with velstra.nixosModules; [ controlPlane node pool ];
  velstra.cloud = {
    controlPlane = {
      enable = true;
      package = velstra-cloud;
      listen = "0.0.0.0:8443";
      cell = "cell-1";
      region = "eu-central";
      cellAdmins = [ "ops" ];
      tokenFile = "/etc/velstra/tokens";
    };
    node.enable = true;
    pool = { enable = true; id = "local"; backend = "directory"; store = "127.0.0.1:2379"; };
  };
}
```

Then create the node and pool objects through the console (§2 and §4) with the
ids this machine's seed uses, and it is a cell.

**What one machine changes, honestly:**

* **Nothing to spread across.** `spread` and `affinity` set to `Preferred` still
  place — the guest runs beside its sibling rather than not at all. Set to
  `Required` they are refused, and `:explainPlacement` names this node and the
  rule. That refusal is the correct answer, not a bug.
* **A backup on the same machine survives a lost pool, not a lost machine.** A
  target on a second disk is worth having; a target on a NAS is worth more. The
  platform refuses a target that is the volume's own pool and will not pretend
  otherwise.
* **Maintenance has nowhere to evacuate to.** You can still drain the node —
  it is your machine — and `:explainMaintenance` says what it costs.
* **No overlay needed.** Skip §5 entirely. Without a fabric, guests get real tap
  devices and reach the network; what you do not get is tenant separation, which
  is not what a household is asking for. A network carrying security groups is
  then refused rather than silently unenforced.
* **etcd is a single member.** Fine for one machine — it is the same disk either
  way. It is not an HA story, and the platform does not claim one.

`nix build .#checks.x86_64-linux.single-node` runs exactly this: all three roles
on one 2 GiB machine, a volume provisioned locally, a guest placed, and both
halves of the placement behaviour above.

---

## 1. The control plane

One machine first, because everything else needs an address to talk to.

### NixOS

```nix
{
  imports = [ velstra.nixosModules.controlPlane ];
  velstra.cloud.controlPlane = {
    enable = true;
    package = velstra-cloud;
    listen = "0.0.0.0:8443";
    cell = "cell-1";
    region = "eu-central";
    cellAdmins = [ "ops" ];
    tokenFile = "/etc/velstra/tokens";   # `<token> <subject>` per line
  };
}
```

A single-member etcd comes with it (`store.bundledEtcd`, on by default). For a
cell that has its own, set `store.bundledEtcd = false;` and `store.endpoints`.

### Debian

```
sudo apt install ./velstra-cloud_*.deb
sudo velstra-cloud-node setup
```

Answer `1` at the roles question. The wizard writes the seed and names the two
units to enable. Nothing started before that, on purpose: a unit is conditional
on its role being in the seed, and a machine that has just been unpacked has no
seed.

Either way, open `http://<host>:8443/` and sign in as the administrator the
wizard asked for. `http`, not `https`: the API serves plain HTTP and belongs
behind something that terminates TLS — and it binds loopback unless the seed
says otherwise, which is the question the wizard asks just before the password.

---

## 2. Adding a machine — through the console

**Nodes → New node.** Give it an id. That id is what everything else will refer
to, and it cannot be changed afterwards.

The response carries a **registration token, shown once**. The platform keeps a
hash of it and cannot show it again — if it is lost, delete the node and add it
back. The panel carries the command to run on the machine and will not close by
itself.

On the machine:

```
sudo velstra-cloud-node setup
```

Answer: region, cell, roles (`2` for a hypervisor), the control-plane URL, the
node id you just chose, and the token. Then enable what it names.

Within a pass the node appears on the board with its capacity — that first
status report *is* the registration working.

### Then give it its role in the cell

Open the node and edit:

* **Accepts work** — a machine can be registered and drained from the first
  moment.
* **Carries external traffic** — this is the "gateway" role, and the reason it
  is here rather than in the installer.
* **vCPUs per core** — sharing a processor is a trade somebody makes on
  purpose.
* **CPU baseline** — what this machine presents to guests, refused if it cannot
  reach it.
* **Labels** — what `placementPolicy.requiredLabels` matches.

---

## 3. Adding a machine — from a file

The same, with nobody watching. Create the node through the API and keep the
token:

```
token=$(curl -fsS -X POST -H "Authorization: Bearer $OPS" \
  -H 'Content-Type: application/json' \
  -d '{"id": "node-a", "spec": {"schedulable": true}}' \
  https://cell-1:8443/api/v1/nodes | jq -r .nodeToken)
```

Write the machine's answers — **the file is a seed**, the same `KEY=value` lines
the wizard writes, so one taken off a working machine installs the next with two
lines changed:

```sh
cat > /var/lib/velstra/setup.env <<'EOF'
# the Frankfurt cell, hypervisor + local storage
VELSTRA_REGION=eu-central
VELSTRA_CELL=cell-1
VELSTRA_ROLES=hypervisor,pool
VELSTRA_API_URL=https://cell-1:8443
VELSTRA_NODE=node-a
VELSTRA_VMM=qemu
VELSTRA_POOL=nvme
VELSTRA_POOL_BACKEND=directory
EOF

VELSTRA_TOKEN="$token" velstra-cloud-node setup --config /var/lib/velstra/setup.env
systemctl enable --now velstra-cloud-nodeagent velstra-cloud-poolagent
```

The token comes through the environment rather than the file: a file with a
secret in it is a file somebody copies.

A missing answer is an **error naming the key**, never a default — an unattended
install that guessed a cell name would produce a machine that comes up,
registers nowhere, and is found weeks later.

Only what the named roles need is required. A pool file that had to carry a node
id would be a file with a value nobody reads.

### On NixOS

The seed is the same; the units are a declaration rather than something a wizard
enables. `setup` prints the module snippet for the answers it was given:

```nix
{
  velstra.cloud = {
    node.enable = true;
    pool = { enable = true; id = "nvme"; backend = "directory"; cell = "cell-1"; };
  };
}
```

---

## 4. Storage

A pool is not a machine — several nodes reach one Ceph pool, one node may export
three volume groups — so it is its own role and its own module.

Same two halves as a node, same order: **Pools → New pool** in the console (or a
`POST` to `/api/v1/pools`), then give the machine the `pool` role with that id
in its seed.

The id has to match. Every volume is written against it, and a mismatch is a
pool that claims nothing and volumes that are never provisioned — quietly.
Creating the object before writing the seed is what stops that, which is why it
is worth the extra step rather than letting an agent invent one.

Unlike a node, a pool is handed no token: its agent authenticates with one you
supply (`velstra.cloud.pool.tokenFile`, or `--api-token-file`).

---

## 5. The data plane

Everything so far decides what *should* be true — which guest is on which
network, which rules apply to its port. The fabric is what makes it true on the
wire, and it is the one part of a cell that can be missing without anything
looking wrong: guests boot, addresses are handed out, every dashboard is green,
and tenant networks separate no traffic.

So it is asked for explicitly and never guessed.

**Two addresses, and they are different services.** The fabric controller
serves them on different ports for different audiences:

| | Seed key | Who talks to it | Asked to |
|---|---|---|---|
| Orchestrator | `VELSTRA_FABRIC` | control plane, node agent | create a port, a network, a route |
| Config service | `VELSTRA_FABRIC_CONTROL` | the eBPF agent on each node | say what this host should be running |

Pointing either at the other's port gets `unimplemented`, which is a confusing
way to learn this. Worth knowing before you widen anything: fabric binds the
orchestrator to **localhost** by default and offers mTLS on the config service
only — so giving every hypervisor a route to the orchestrator is a real
decision, because that channel can reconfigure any node in the cell.

On the control plane:

```nix
velstra.cloud.controlPlane.fabric = "http://fabric.cell-1:50052";
```

On a hypervisor, the seed carries both, plus this host's own place on the wire:

```sh
VELSTRA_FABRIC=http://fabric.cell-1:50052
VELSTRA_FABRIC_CONTROL=http://fabric.cell-1:50051
VELSTRA_FABRIC_VTEP=10.0.0.7          # what other hosts send frames to
VELSTRA_FABRIC_UNDERLAY=eth1          # the interface that address is on
VELSTRA_FABRIC_SRV6_LOCATOR=fc00:0:1::/64   # optional; empty stays VXLAN
```

The VTEP address is stated rather than derived: nothing on a machine can tell
which of its addresses its peers route to, and picking one would pick wrong on
every host with more than one interface. The locator is the same argument one
step further — it is a slice of your own IPv6 plan, has to be routable in the
underlay and unique per host, and nothing local knows any of that.

Naming a fabric and leaving out this host's place on it is **an error**, not a
default. That combination is the failure that looks most like success.

On NixOS the unit comes with the node module once you give it the agent:

```nix
velstra.cloud.node.fabricAgent = pkgs.velstra;   # the fabric agent
```

On Debian the agent is a `Recommends:` — install the `velstra` package, then
`systemctl enable --now velstra-fabric-agent`. Either way the unit skips itself
on a machine whose seed names no fabric, and says so in the journal rather than
turning red.

---

## 6. More than one cell

A cell is the failure and scaling domain: **a machine belongs to exactly one**,
and growing means adding cells rather than making one bigger. Objects carry
their region and cell from creation and neither can be changed afterwards.

Working across them is several control planes, with one (or each) told where the
others are:

```nix
velstra.cloud.controlPlane.cells = {
  "cell-2" = "https://cell-2.example:8443";
};
```

A client then reaches one address and the request lands in the cell holding the
resource. Which cell owns what is read from the **projects**, not from that map
— the map only says where each cell is. A project this installation has not
heard of yet is answered locally rather than refused: a router a few seconds
behind must not turn propagation delay into an error a tenant sees.

---

## 7. What to check when it does not work

| Symptom | Ask |
|---|---|
| the node never appears | `journalctl -u velstra-cloud-nodeagent`; is the token file 0600 and the id the same on both sides? |
| a unit says "skipped" | that is a role that is not in the seed — `cat /var/lib/velstra/node.env` |
| a guest will not place | `…/instances/i1:explainPlacement` |
| a public address answers nothing | `…/floatingips/web:explainReach` |
| a machine is out of service and nobody said so | `nodes/node-b:explainMaintenance` |
| a tenant cannot start anything | `projects/p1:explainQuota` |

Every one of those is a plain `GET` that changes nothing, and every one answers
in the operator's own words rather than "no valid host".

---

## What this guide does not cover

* **The sealed appliance.** `docs/install.md` — a machine you flash and forget,
  with A/B slots and verity. The wizard there also partitions.
* **Day two.** `docs/operating.md` — maintenance windows, recovery, overcommit,
  placement groups, which copy survives which loss.
* **The API itself.** `docs/rest-contract.md`.
