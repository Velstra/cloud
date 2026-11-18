# Set up a cell

A cell has one shared control-plane store. Compute nodes report what their hypervisors observe. Pool agents report the storage they manage. An object visible in the console is ready for work once its agent has reported.

## Bring the first node online

Follow [Installation](install.md) and sign in as an operator. Check these things before adding more hosts:

| Check | Passing result |
| --- | --- |
| API and controller use the same region, cell and etcd endpoints | The controller observes resources created through the API |
| The compute role is configured and its agent is running | Node shows its current capacity and heartbeat |
| The pool object's ID matches its pool agent configuration | Pool reports backend health and capacity |
| Networking mode and the subnet's gateway suit the deployment | A test guest receives metadata and can reach its intended gateway |

With the CLI configured for your cell:

```sh
velstra get nodes
velstra get pools
velstra get projects
```

## Add a compute node and storage

Use [Join a node](joining.md) for registration. A remote compute agent uses its own node credential. An independently authenticated pool agent needs the credential for its pool.

| Pool type | What to set | What to check |
| --- | --- | --- |
| Directory | Absolute directory and owning node | The agent can allocate a test volume on that host |
| LVM | Volume group, optional thin pool, owning node | The agent reports available capacity |
| Ceph | Velstra pool ID, RBD pool and image pool, keyring and cluster config | The separate Ceph pool agent reports a healthy backend |

The Velstra pool identifier used by `spec.pool` can differ from the RBD pool name. A remote guest using shared storage needs access to the same bytes on the destination host. [Storage operations](operations.md#ceph-pools-for-the-cell)

## Connect tenant networking

For a small single-host evaluation, the local gateway option connects its guests to the host. For tenant networks spanning hosts, configure Fabric.

| Setting | Consumer | Purpose |
| --- | --- | --- |
| `VELSTRA_FABRIC` | Orchestrator client | Create and reconcile networks, ports and routes |
| `VELSTRA_FABRIC_CONTROL` | Fabric agent | Receive configuration for this host |

Use the endpoint and TLS identity that belong to each service. Swapping endpoints can yield an `unimplemented` response even when both are reachable. Verify registration in Fabric and connectivity between disposable tenant endpoints that should be able to communicate. [Fabric documentation](https://velstra.github.io/fabric/)

## Multiple cells or control-plane replicas

A second cell has its own store and placement domain. Configure the API's cell routing for access to multiple cells from a common entry point. Several replicas of one cell instead share its store and durable release files. [Replication and recovery](operations.md#replicated-control-planes-and-release-storage)

[CLI](cli.md) · [Deployment models and PCI](deployment-and-devices.md)

## Wire Fabric to a real host

Example values for a host with a routable VTEP at `192.0.2.27` and a verified underlay interface. Replace every address and interface with inventory values before applying.

```ini
VELSTRA_FABRIC=https://fabric.example.net:50052
VELSTRA_FABRIC_CONTROL=https://fabric.example.net:50051
VELSTRA_FABRIC_VTEP=192.0.2.27
VELSTRA_FABRIC_UNDERLAY=ens5
VELSTRA_FABRIC_CA=/etc/velstra/fabric/ca.pem
VELSTRA_FABRIC_CERT=/etc/velstra/fabric/orchestrator.pem
VELSTRA_FABRIC_KEY=/etc/velstra/fabric/orchestrator.key
VELSTRA_FABRIC_AGENT_CA=/etc/velstra/fabric/ca.pem
VELSTRA_FABRIC_AGENT_CERT=/etc/velstra/fabric/node.pem
VELSTRA_FABRIC_AGENT_KEY=/etc/velstra/fabric/node.key
```

The Cloud orchestrator identity needs authorization for the resources it programs, including security groups. Give the dataplane agent its own node-scoped identity matching `VELSTRA_NODE`. Its underlay attachment handles traffic arriving from remote hosts. A controller cluster accepts comma-separated endpoints of the same scheme in each service list. For SRv6, each host needs its own routable locator (`VELSTRA_FABRIC_SRV6_LOCATOR`).

On Debian, point `VELSTRA_FABRIC_AGENT_BINARY` at the actual Fabric executable, or install it at `/usr/local/libexec/velstra-fabric-agent`. Cloud installs its own CLI as `/usr/bin/velstra`. A host running only local gateway networking needs a route from the LAN to the guest subnet for direct inbound reachability.

## Route requests across cells

For a NixOS control plane, add peer API endpoints under the cell names used in project placement:

```nix
velstra.cloud.controlPlane.cells = {
  "cell-2" = "https://cell-2.example.net:8443";
};
```

Every node belongs to one cell. A single host cannot drain a guest to another node in that cell. Place backup targets on an independent machine or storage system when protection against host loss matters.
