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

### Rehearsing it without touching the cell

A restore procedure nobody has run is a procedure, not a capability. This one
can be exercised on a live control plane without going near it, because a
restored store is just a second etcd on other ports:

```
SNAP=$(ls -t /var/lib/velstra/store-backups/etcd-*.snap | head -1)
etcdutl snapshot restore "$SNAP" --data-dir /var/lib/etcd.rehearsal \
  --name rehearsal \
  --initial-cluster rehearsal=http://127.0.0.1:23800 \
  --initial-advertise-peer-urls http://127.0.0.1:23800
etcd --data-dir /var/lib/etcd.rehearsal --name rehearsal \
  --listen-client-urls http://127.0.0.1:23790 \
  --advertise-client-urls http://127.0.0.1:23790 \
  --listen-peer-urls http://127.0.0.1:23800 \
  --initial-advertise-peer-urls http://127.0.0.1:23800 \
  --initial-cluster rehearsal=http://127.0.0.1:23800 \
  --initial-cluster-state existing &
```

The `--name` and the peer URL have to be given to **both** commands and have to
match: `etcdutl` writes the member into the restored data directory, and etcd
refuses to start against a directory that names a different member. Leaving
them at the defaults puts the rehearsal on 2380, which is the live store's peer
port.

Then compare, and read something real out of it:

```
etcdctl --endpoints=127.0.0.1:23790 endpoint health
etcdctl --endpoints=127.0.0.1:23790 get --prefix --keys-only "" | grep -c .
etcdctl --endpoints=127.0.0.1:2379  get --prefix --keys-only "" | grep -c .
etcdctl --endpoints=127.0.0.1:23790 get /<cell>/instances/ --prefix --keys-only
```

A key count one or two short of the live one is the writes since the snapshot,
not a fault. Afterwards, stop it by port rather than by name — `kill $(ss -lptn
'sport = :23790' | grep -oP 'pid=\K[0-9]+')` — and remove the directory. A
restore of a 225 MiB snapshot takes about a second and lands in roughly 280 MiB
on disk, so check `df` first.

Done on this cell on 10 September 2026: 234 930 keys against 234 931 live, the
guests and their specs readable out of the restored store, and the live control
plane untouched throughout.

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


## A Ceph pool for the cell

Two units serve pools on one machine: `velstra-cloud-poolagent` for its local
disks and `velstra-cloud-poolagent-ceph` for the cluster. The second does
nothing unless the seed names `VELSTRA_CEPH_POOL_ID` — **the pool object's id
in this cell**, what a volume's `spec.pool` names, as against
`VELSTRA_CEPH_POOL` / `VELSTRA_CEPH_IMAGE_POOL`, which are RBD pools inside
the cluster.

Bringing one up, in the order the platform expects:

1. `apt-get install cephadm podman` on the machines that will run daemons.
2. Create the `ceph-clusters` object (monitors, OSDs, pools). The agents
   bootstrap, add hosts and create the pools by themselves; the board says
   which step is outstanding and who owns it.
3. Create a `pools` object for it (`POST /api/v1/pools`, id `ceph`), put its
   id in the seed as `VELSTRA_CEPH_POOL_ID`, and start
   `velstra-cloud-poolagent-ceph`.
4. Publish images into the cluster once, from any machine with the keyring:
   `velstra-cloud-poolagent --import-image images/sha256-… --from <file>
   --backend ceph`. The image lands in the image pool with a protected
   `@base` snapshot, and every volume made from it is an `rbd clone` — no
   bytes move, and "which nodes hold this image" stops being a question.

**A pool object with no agent is a black hole, and the cell now says so.**
Steps 3 and 4 are easy to leave half-done — the `pools` object exists, the
agent was never started — and the result is a pool that swallows work
silently: a volume created on it waits for ever, and a volume *deleted* from it
keeps its `pool.velstra.io/release` finalizer for ever, because the agent that
would remove it does not exist. The `pool-unwatched` alert fires for a pool
nothing has ever reported on, and again for one whose agent has stopped (ten
minutes by default, `--alert-pool-silent-after`). If you find such a pool with
volumes already stuck on it, the way out is to start its agent — nothing else
removes that finalizer, by design: a platform that dropped it would be
declaring bytes released that nobody has released.

Two things Ceph itself insists on, both handled by the platform and both worth
knowing when reading a cluster by hand: a pool with `size: 1` needs
`mon_allow_pool_size_one` set globally *and* `--yes-i-really-mean-it` on the
`set`; and `ceph-volume` refuses a whole disk that carries a partition table,
so a disk that was something else has to be wiped (`wipefs -a`) before it can
be an OSD.
