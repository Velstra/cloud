# Operating a cell

The short list of things an operator does that are not in the REST contract:
what to back up, how to restore it, and what to do when a machine is gone.

## The store is the cell

Every object — guests, networks, addresses, users, grants — lives in etcd on
the control-plane machine. **The guests survive their control plane dying**:
QEMU keeps running, DHCP and metadata keep answering, nothing a tenant is
running notices. What dies with the store is the ability to *manage* any of it,
for ever. So the store is the one thing to back up.

The API snapshots it hourly into `VELSTRA_STORE_BACKUP_DIR`
(`/var/lib/velstra/store-backups` by default on Debian; empty disables), keeps
the newest 24, and writes each file under a temporary name first — a snapshot
that exists is a snapshot that finished. **Point the directory at storage that
is not the control plane's own disk** (an NFS mount, a disk on another
machine): the failure this exists for is that disk dying.

## Restoring the store

On a fresh or repaired control-plane machine:

```
systemctl stop velstra-cloud-api velstra-cloud-controller etcd
etcdutl snapshot restore /path/to/etcd-<newest>.snap \
  --data-dir /var/lib/etcd.new
mv /var/lib/etcd /var/lib/etcd.dead && mv /var/lib/etcd.new /var/lib/etcd
chown -R etcd:etcd /var/lib/etcd    # if etcd runs as its own user
systemctl start etcd velstra-cloud-api velstra-cloud-controller
```

(Older etcd installs ship the same verb as `etcdctl snapshot restore`.)

What comes back is the cell as of the snapshot: up to an hour of writes are
gone, which for this platform means *asks*, not machines — a guest created in
that hour is still running on its node, and the node agent's next resync
reports it against an instance object that no longer exists. Delete or
re-create such objects deliberately; nothing does it for you.

Two things restore does **not** bring back, by design:

* **Sessions and console tickets** — everybody signs in again.
* **The store's own history** — watches resume by re-listing, which every
  agent does on its resync anyway.

## When the store filled up anyway

`mvcc: database space exceeded` means history outgrew etcd's quota. The API
compacts hourly, so this points at something writing far faster than usual —
find that first. To recover:

```
export ETCDCTL_API=3
rev=$(etcdctl endpoint status -w json | jq '.[0].Status.header.revision')
etcdctl compact "$rev"
etcdctl defrag --command-timeout=120s
etcdctl alarm disarm
```

Compaction stops the growth (freed pages are reused); `defrag` is what shrinks
the file, and it blocks the store briefly — run it in a quiet moment.

## A machine that stopped answering

The platform deliberately declares no node dead on its own. What you configure
decides what happens:

* `node.spec.fenceAfterS` — after this many seconds of silence the node's own
  agent stops its guests (it fences *itself*; it needs no network to do so).
  `0`, the default, never fences.
* `instance.spec.onNodeLoss` — `leave` (default) strands the guest until the
  node returns; `restart` lets the cell start it elsewhere **once the node is
  provably fenced** (silent for `fenceAfterS` plus margin).

The safe pairing for machines that flap (laptops, Wi-Fi) is the default. The
available pairing for real servers on real power is `fenceAfterS: 120` and
`onNodeLoss: restart` on the guests that may move.


## Announcing the cell over BGP

A gateway that should speak BGP needs one thing installed by hand: `apt-get
install frr`. Everything else is the platform's — the agent enables `bgpd`
(and `staticd`, for the blackhole routes that satisfy `network` statements),
renders `/etc/frr/frr.conf` from the `bgp-peers` objects, and reloads FRR only
when the derived announcement set actually changed.

Create a session as the operator:

```
POST /api/v1/bgp-peers
{ "id": "edge", "spec": { "peer": "10.10.10.1", "peerAs": 65000,
                          "localAs": 65010, "node": "gw-1" } }
```

`status.session` reports FRR's own word (`Established`, `Active`, …) and
`status.announced` the prefix count. What is announced is derived: every
external subnet, plus a host route per floating address that names a port.
The far end must accept eBGP without an import policy or carry its own
(RFC 8212 — modern FRR filters everything until a policy exists; the rendered
config on our side already says `no bgp ebgp-requires-policy` because the
network statements *are* the policy).
