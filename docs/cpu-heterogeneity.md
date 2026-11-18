# Plan migrations across CPU generations

A live migration must preserve the CPU the guest saw at boot. Two x86-64 nodes can offer different instruction sets, and adding a new node does not widen the CPU contract of a running VM.

## Inspect the cell

Use a cell operator token. With `VELSTRA_API` and `VELSTRA_TOKEN` set as in the [quickstart](quickstart.md):

```sh
curl -fsS -H "Authorization: Bearer $VELSTRA_TOKEN" \
  "$VELSTRA_API/api/v1/nodes:explainCpu" \
  | jq '{domains, unreported, advice, pendingAdoption}'
```

| Field | Operator decision |
| --- | --- |
| `domains` | Identify hosts that present compatible CPUs and whether they can apply a baseline |
| `unreported` | Obtain a recent agent inventory before planning a migration |
| `advice` | Inspect missing CPU capabilities and proposed groupings |
| `pendingAdoption` | Identify running guests that will see the new baseline on their next start |

## Check a specific move

```sh
velstra --project p1 explain instances web-1 migration
```

A destination must meet more than CPU requirements: hypervisor capabilities, image availability, storage, devices and capacity also matter. Ask about the intended mode. The HTTP method accepts `?mode=Live` or `?mode=Reboot` to compare their decisions.

## Choose a baseline

On a QEMU node, an operator can declare a compatible x86-64 baseline, for example `x86-64-v2`. The API checks whether the host can present it. Cloud Hypervisor exposes the host CPU and does not support this QEMU baselining path. Leave its hosts in compatible groups or consider a planned reboot migration.

A baseline change applies as guests next start. It does not restart a running VM. Check the pending-adoption list before decommissioning hosts that a guest still needs. A colder migration has different storage requirements. [Local disk migration](local-disk-migration.md)

## Read a refusal before changing a baseline

| Observation | Consequence |
| --- | --- |
| A host lacks flags required by the requested level | Its baseline change is rejected. Inspect the missing flags before grouping it with newer hosts. |
| Host-passthrough CPUs differ on hosts that cannot mask them | A common marketing model or x86-64 level is insufficient. Check the guest's recorded CPU against the target. |
| A running guest has an older recorded baseline | Keep at least one suitable destination, or plan its restart before decommissioning the old domain. |
| Guest and target have different CPU architectures | CPU baselining cannot translate between architectures. |

For an operator-approved QEMU baseline change, inspect the current value first. The following is an example for a host verified to support that level:

```sh
velstra get nodes node-c --json | jq '{configured: .spec.cpuBaseline, cpu: .status.cpu}'
velstra patch nodes node-c --set cpuBaseline=x86-64-v2
```

After applying it, query `nodes:explainCpu` again and review `pendingAdoption`. Only schedule guest restarts when the resulting CPU meets workload requirements.
