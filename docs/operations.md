# Recover and maintain the infrastructure

This runbook covers control-plane state, host loss and storage. For ordinary VM placement and maintenance decisions, use the [operator guide](operating.md).

## Back up the store

The etcd store contains resource specifications, observed state, identities and policies. Volume data and release artifacts have separate lifecycles. Configure `VELSTRA_STORE_BACKUP_DIR` on durable storage outside the control-plane machine. Keep the directory private and verify that snapshots continue arriving. The Debian default is `/var/lib/velstra/store-backups`.

Check one snapshot before relying on it. Supply the path of a **completed** snapshot:

```sh
etcdutl snapshot status "$SNAPSHOT" --write-out=table
```

## Rehearse a restore without touching the cell

Choose a fresh directory and free loopback ports on a rehearsal machine. The following creates a separate etcd member. Leave the live store and its data directory untouched.

```sh
work=$(mktemp -d)
etcdutl snapshot restore "$SNAPSHOT" --name rehearsal \
  --data-dir "$work/store" \
  --initial-cluster rehearsal=http://127.0.0.1:23800 \
  --initial-advertise-peer-urls http://127.0.0.1:23800
etcd --name rehearsal --data-dir "$work/store" \
  --listen-client-urls http://127.0.0.1:23790 \
  --advertise-client-urls http://127.0.0.1:23790 \
  --listen-peer-urls http://127.0.0.1:23800 \
  --initial-advertise-peer-urls http://127.0.0.1:23800 \
  --initial-cluster rehearsal=http://127.0.0.1:23800
```

In a separate shell, run `etcdctl --endpoints=http://127.0.0.1:23790 endpoint health`. Inspect the keys for a known resource and compare the snapshot timestamp with the recovery objective. Stop the rehearsal process with Ctrl+C and protect the temporary directory as a copy of the cell.

## Restore after loss

1. Stop the API, controller, node and pool agents before reintroducing a restored inventory. Managed guest processes may continue running independently.
2. Retain the original data directory. Restore a known snapshot into a fresh directory using the deployment's actual etcd identity, ownership and membership. Debian and appliance paths differ.
3. Start the store and verify endpoint health before reconnecting API and controller replicas.
4. Compare restored instance and pool records with the actual hosts and disks. Reconcile missing or newer objects deliberately, then restart the agents. Existing guests absent from the snapshot need operator reconciliation.
5. Sign in again and verify a read and reconciliation on the recovered cell. Watches relist; session and console tickets need to be reissued.

## Recover after host loss

A node may still be running guests after its management path fails. Cordon it and verify external fencing through an independent mechanism. Keep it fenced. Read the node’s last reported heartbeat **after** fencing and use the recovery procedure for that same host and heartbeat:

```sh
velstra get nodes node-a --json | jq '.status.lastHeartbeat'
velstra --project p1 explain instances web-1 recovery
```

Only after confirming that external fencing succeeded, a cell operator may record the millisecond heartbeat as string label `velstra.io/fenced-heartbeat` on that node. A newer heartbeat invalidates the confirmation. Recovery also requires a configured fence deadline, elapsed margin, an instance with restart-on-loss policy and accessible storage. Investigate `FenceNotConfirmed` before proceeding.

## Ceph pools for the cell

A pool object represents the Velstra allocation target; the RBD pool stores the bytes. Configure the Ceph pool agent with the correct pool ID, RBD pool, cluster configuration and credentials. The `velstra-cloud-poolagent-ceph` unit is separate from the local pool agent.

```sh
systemctl status velstra-cloud-poolagent-ceph --no-pager
velstra get pools
```

Check the backend condition before offering the pool to a project. If a volume or deletion stalls, restore agent/backend health before intervening in the volume lifecycle. The pool agent owns physical cleanup. [Storage semantics](rest-contract.md#storage)

## Replicated control planes and release storage

The bundled single-member etcd setup cannot tolerate losing its only voter. For a replicated deployment, declare a complete odd voting set and follow the [control-plane reconciliation procedure](https://github.com/Velstra/cloud/blob/main/deploy/control-plane/README.md). Use mutually authenticated etcd connections and verify membership and health on every replica.

Mount the same durable `VELSTRA_RELEASES_DIR` on all API and controller replicas and back it up separately. A restored etcd record can say an artifact is ready while the bytes it references are missing on an individual replica.

[Guest networking and security groups](rest-contract.md#security-groups) · [Release rollouts](upgrading.md)

## If etcd reports database space exceeded

Inspect the write rate and available disk space first. Take a verified snapshot, then plan compaction and per-member defragmentation for the affected cluster. Defragmentation blocks that member while it runs. Recheck health and disk use before clearing a space alarm. Use the deployment's TLS credentials for all `etcdctl` calls. A production quorum restore must use the intended membership and the same snapshot for each member.

## Connect Ceph clients

Ensure host-native `librbd` and the QEMU RBD driver support the network cluster's authentication format. A working CLI in a `cephadm` container does not verify that host QEMU can open the volume. Specify compatible Ceph and client versions before admitting workloads. Never reinitialize a disk containing useful data to satisfy OSD selection.

The agent obtains the managed Ceph configuration and client keyring for its configured pools. Restrict access to keyrings. A pool can restrict project access; removing a project stops new placement there while existing volumes remain. When retiring a pool, verify ownership, move dependencies and confirm it is empty before requesting removal.

## Check announcements and guest metadata

Gateway node agents render FRR from operator-managed BGP peer objects. Install FRR on Debian gateways, configure the adjacent router's import/export policy, and inspect the reported session and actual announcement count. [BGP peer contract](rest-contract.md#bgp-peers-announcing-the-cell-to-the-router-in-front-of-it)

Guest metadata is scoped to the guest network. If defaults `vbr+` or `vt+` do not match your guest-facing interfaces, set the NixOS node's `metadataInterfaces` option to the intended bridges or taps. Keep the uplink out of that list. Verify metadata access from a test guest before changing host firewall rules.
