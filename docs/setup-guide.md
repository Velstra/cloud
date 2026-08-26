# Setting up a cell

From nothing to a machine running guests, twice: once by hand through the
console, once from a file with nobody watching. Both end at the same place —
the same seed, the same units, the same objects — because they are the same two
halves in a different order.

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
sudo dpkg -i velstra-cloud_*.deb
sudo velstra-cloud-node setup
```

Answer `1` at the roles question. The wizard writes the seed and names the two
units to enable. Nothing started before that, on purpose: a unit is conditional
on its role being in the seed, and a machine that has just been unpacked has no
seed.

Either way, open `https://<host>:8443/` and sign in.

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
three volume groups — so it is its own role and its own module. Register the
pool object, then give the machine the `pool` role:

```
curl -fsS -X POST … -d '{"id": "nvme", "spec": {"accepting": true}}' …/api/v1/pools
```

The id in the seed has to match: every volume is written against it, and a
mismatch is a pool that claims nothing and volumes that are never provisioned —
quietly.

---

## 5. More than one cell

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

## 6. What to check when it does not work

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
