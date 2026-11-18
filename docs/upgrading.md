# Roll out a node release

A **release** imports artifacts and checksums into a cell. A **rollout** selects that release and the nodes to update. Track progress from each node's installed build and its update condition.

## Import a release

1. Make the chosen artifacts and their `SHA256SUMS` file available on a trusted HTTPS or operator-controlled local channel. Cloud checks the artifact digest. The current Cloud channel does not verify a signed release manifest.
2. In **Releases**, create a release with the channel URL. Wait for the ready condition. Verify that an artifact exists for each intended install kind.
3. In a replicated control plane, mount the same durable release directory on every API and controller replica. [Release storage](operations.md#replicated-control-planes-and-release-storage)

```sh
velstra get releases
velstra get nodes
```

## Start with one node

Select one node that can be taken out of service. Inspect its current build and maintenance report before using the upgrade action. For a group, create a rollout specifying the release, target nodes, maximum unavailable count and whether to evacuate.

```sh
velstra explain nodes node-a maintenance
velstra get rollouts
```

An appliance writes the new image to its inactive slot. A package installation follows the package path. Nodes without a supported apply method need their installation managed externally.

## Verify or pause

Inspect the rollout and node status. Continue once the intended build is installed and the node is usable. The rollout owns only its own cordons. A pre-existing cordon or an operator edit needs inspection before retrying.

If a node reports `Updating=False` with reason `Failed`, read the condition before taking action. An interrupted or rolled-back attempt is recorded. To repeat the same target deliberately, clear `spec.wanted`, wait for the agent to observe the change, then request the retry. A different release requires a new rollout.

[Node storage and recovery](operations.md) · [API release and rollout contract](rest-contract.md#releases-rollouts-and-the-install-medium)

## Interpret the install kind

| Node | Applied artifact | Recovery boundary |
| --- | --- | --- |
| Sealed appliance | Raw node image for its inactive slot | Boot counting can return to the previous slot. Verify the installed build after boot. |
| Debian package | Matching Debian artifact | Package installation uses the host package lifecycle. It does not create a bootable A/B rollback of Debian. |
| Other install kind | No automatic artifact for this kind | Manage the update through its declared host lifecycle. |

Before a group rollout, verify quorum and spare capacity for the configured unavailable count. After an interruption, inspect `velstra.io/rollout-owner` and the node’s existing maintenance state before clearing a cordon. The rollout must not release maintenance that another operator owns.
