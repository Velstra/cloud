# Cold migration of local root disks

Ceph-backed root volumes can be opened by both QEMU hosts. Node-local root
files require an explicit copy before the instance changes owner. This path
supports `Reboot` migrations: the guest shuts down, its VMM closes the disk,
rsync copies the sectors over SSH, and the destination starts the guest after
ownership has moved. Memory is not preserved.

Set `VELSTRA_DISK_TRANSFER_CONFIG` on the source node agent to a root-owned JSON
file:

```json
{
  "identity_file": "/etc/velstra/migration-key",
  "known_hosts_file": "/etc/velstra/migration-known-hosts",
  "peers": {
    "node-b": {
      "host": "node-b.internal",
      "user": "migration",
      "run_dir": "/var/lib/velstra/instances"
    }
  }
}
```

The destination directory must match that node agent's instance directory. The
SSH account must be able to create and replace files there, and the resulting
files must be readable by its VMM. Install rsync on both hosts, authorize the
source's public key, and pin the destination's verified SSH host key. The agent
uses batch authentication and strict host-key checking. Configure the reverse
peer separately if guests should move in both directions.

Only named peers appear in the node's `localMigrationTargets`. The migration
API allows local root copies only in `Reboot` mode. Select that mode in the
instance's Migrate menu. Configure `VELSTRA_MIGRATION_ADDRESS` separately for
live transfers of guests using shared storage.

The source retains ownership until rsync succeeds. Sparse data is preserved,
checksums determine which content differs, and the incoming file is synced and
renamed before the destination may start. Work runs asynchronously so the agent
continues sending heartbeats. A failed transfer leaves the guest stopped on the
source and exposes the error; cancellation permits it to restart there. A
restarted agent repeats an unfinished copy from the stopped source disk.

This path moves the instance's local `root.raw`. It does not relocate separately
managed directory/LVM pool volumes or attached data disks. Those require their
own pool ownership and attachment handover; treating their paths as shared is
not a substitute. Live migration of local disks requires block replication in
addition to memory migration and is not enabled by this configuration.

The destination records `completedAt` only after it owns a running guest. That
receipt keeps the migration as history without replaying it after a return
move. The API refuses another unfinished request for the same guest. During a
live move, the destination prepares an unadvertised tap; the source releases
its network address at handover before the destination advertises that address.
