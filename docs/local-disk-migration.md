# Move a guest with a local root disk

Use **Reboot** migration for a guest whose `root.raw` exists only on the source. The VM stops, the source copies its closed root disk, and the destination starts the guest after ownership moves. Guest memory and separately attached data volumes are outside this transfer.

## Configure the transfer peer

Install rsync on both hosts and pin the destination's verified SSH host key. Give the destination account access to the instance directory, with the resulting files readable by its hypervisor. On the source set `VELSTRA_DISK_TRANSFER_CONFIG` to a root-owned JSON file such as:

```json
{
  "identity_file": "/etc/velstra/migration-key",
  "known_hosts_file": "/etc/velstra/migration-known-hosts",
  "peers": {
    "node-b": {
      "host": "node-b.example.net",
      "user": "migration",
      "run_dir": "/var/lib/velstra/instances"
    }
  }
}
```

Match `run_dir` to the destination agent's actual instance directory. Configure a separate reverse peer if migrations must work in both directions.

## Check before moving

With an operator token, request a plan for the intended mode:

```sh
curl -fsS -H "Authorization: Bearer $VELSTRA_TOKEN" \
  "$VELSTRA_API/api/v1/projects/p1/instances/web-1:explainMigration?mode=Reboot" \
  | jq '.destinations'
```

Check the target is listed and allowed. In the instance's **Migrate** menu select **Reboot** and the eligible destination. Verify the new node assignment, running state, network reachability and migration completion before releasing the source.

## Interrupted transfers

The source retains ownership until the copy succeeds. A failed copy leaves the guest stopped on the source with the reported error. Inspect peer access and free space before deciding to retry or cancel. A restarted agent can repeat an unfinished copy from the stopped disk.

Live migration of shared-storage guests is a different path. TCP transfers require `VELSTRA_MIGRATION_ADDRESS` and trusted certificates under `VELSTRA_MIGRATION_TLS_DIR` on both nodes. See [migration modes and admission](rest-contract.md#migrations).

## Configure shared-storage live migration separately

When two nodes receive TCP migration traffic, set their advertised migration addresses and provision `VELSTRA_MIGRATION_TLS_DIR` on both. The directory contains `ca-cert.pem` plus the node’s `server-cert.pem`, `server-key.pem`, `client-cert.pem` and `client-key.pem`. The server certificate must cover its advertised migration address. Validate guest CPU compatibility, shared disk access and network handover before using **Live**. The rsync configuration above handles only node-local root disk moves in **Reboot** mode.

A guest with additional Directory or LVM pool volumes needs an independent disk/attachment handover; copying the root disk cannot transfer those volumes. Keep the source available until the destination reports a running guest and the migration records completion.
