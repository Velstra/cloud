# Diagnose and operate a cell

Use the cell operator identity for placement, capacity and host maintenance questions. Tenant users can read the state of their own instances subject to their project permissions. [Connect the CLI](cli.md) before trying the examples.

<a id="1-ask-before-you-act"></a>

## VM placement

The [development cell](quickstart.md) seeds a VM called `too-big` in project `p1`. It asks for 64 vCPUs and 262144 MiB. Neither default host can satisfy it.

```sh
velstra --project p1 explain instances too-big placement
velstra --project p1 get instances too-big --json | jq '.status.conditions'
```

The first command evaluates the current inventory. When no host fits, `placed` is null and `rejected` contains a host, reason and detail for each refusal. The second reads the scheduler’s last recorded decision. A `Ready=False` condition with reason `NoValidHost` means the scheduler has evaluated the request. If the cluster was just started, allow a reconciliation pass.

One run of the default cell returned this placement report after `web-1` started. The free counts reflect the inventory at query time.

```json
{
  "placed": null,
  "rejected": [
    {"node": "node-a", "why": "InsufficientVcpus", "detail": "30 free, 64 wanted"},
    {"node": "node-b", "why": "InsufficientVcpus", "detail": "16 free, 64 wanted"}
  ]
}
```

| What you see | Check next |
| --- | --- |
| Node has insufficient free memory | Look for a **single** host with enough headroom. Free memory summed across hosts cannot fit a larger VM. |
| Node is draining or inside maintenance | Read the window and decide whether to wait or use a different host. |
| A required device class or affinity rule excludes every host | Compare the instance request to host inventory and existing group membership. |
| Request rejected at admission | Inspect project quota and permissions. Such a request may never reach the scheduler. |

## Before maintenance

```sh
velstra explain nodes node-a maintenance
velstra --project p1 explain instances web-1 migration
velstra explain projects p1 quota
```

The maintenance report lists affected guests. The migration report lists potential destinations with reasons. The quota report distinguishes project limits from physical capacity. A VM that owns a PCI device may need a maintenance outage.

Create a **Maintenance window** for a particular node, start time and duration. A window with `drain=false` blocks new placements while existing guests continue. With `drain=true`, check the migration report and wait for guests to move before touching the host.

## When a host disappears

A missed heartbeat is insufficient evidence that a guest stopped. Check the recovery report and the independent fencing state before authorizing a restart elsewhere.

```sh
velstra --project p1 explain instances web-1 recovery
```

[Recovery, external fencing and store restore](operations.md) · [Cold migration of local disks](local-disk-migration.md)

## Choose the right copy

| Copy | Protects | What to verify |
| --- | --- | --- |
| Volume snapshot | Point-in-time state on its backing pool | Snapshot is ready and the pool survives |
| Backup to another target | Volume data after loss of the source pool | Restore or verify the backup from the target |
| etcd snapshot | Cell objects, users, policies and metadata | Restore into an isolated etcd member and read the inventory |

If a detached volume remains pending deletion, check the pool agent and its backend. A finalizer can mean disk cleanup is still outstanding. Removing the finalizer bypasses that work.

[Storage and backups](rest-contract.md#backups) · [CPU compatibility](cpu-heterogeneity.md) · [Release rollouts](upgrading.md)

## Verify a backup after it was written

Set the backup target’s `verifyEveryHours` to enable periodic read-back. Its default, zero, schedules no verification. Inspect the **Read back** result and **Trouble** field after a verification pass. Old backups without an original digest can be restorable but unverifiable. Retention counts copies independently of verification; choose an interval that leaves time to react before older good copies expire.

To change a volume's backing pool, take a backup to a different target, create a volume in the new pool using `spec.sourceBackup`, then test the restored data before switching attachments. Retain the original volume until the owner confirms the new one.

## Know what an image signature covers

A tenant image may carry an Ed25519 signature over the digest string. Configure trusted image signing keys on the API and node agents, and require signed images on nodes where unsigned guest images must be refused. This policy covers guest images; node release channels have separate verification rules. [Signed images](rest-contract.md#signed-images) · [Node rollouts](upgrading.md)

## More operator checks

| Task | Read |
| --- | --- |
| Find what changed after a request | [Object history](rest-contract.md#the-records-about-one-object) |
| Understand vCPU overcommit and hard memory limits | [Capacity rules](rest-contract.md#sharing-a-processor-and-never-sharing-memory) |
| Keep replicas on different hosts | [Affinity and anti-affinity](rest-contract.md#keeping-guests-apart-and-keeping-them-together) |
| Diagnose floating and guest public addresses | [Address pools](rest-contract.md#public-address-pools-and-who-sees-machines) |
