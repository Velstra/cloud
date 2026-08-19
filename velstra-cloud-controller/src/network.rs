//! Telling the fabric about a tenant network.
//!
//! A network is the one thing in this system that a node cannot state. It is a
//! cell-wide fact — a VNI, the subnet its ports are allocated from, the default
//! the firewall applies to them — and every node in the cell would otherwise be
//! asserting it at once, which is both a disagreement waiting to happen and a
//! load that grows with the cell: a thousand nodes restating a hundred networks
//! is a hundred thousand calls a pass, to say something that has not changed.
//!
//! So a controller says it, and leader election means exactly one process does.
//!
//! **Why this exists at all.** Nothing mirrored networks. The node agent called
//! `create_port` with a VNI the fabric had never been told about, and fabric
//! answered `unknown network vni` — so in a real cell the *first* port was
//! refused and no guest ever reached the network. It passed in tests because
//! the test declared the network itself; the fixture said so out loud
//! ("here the test plays that part, which is exactly the gap it documents") and
//! the gap stayed open. Tenant isolation was designed, tested, and not in force.

use std::sync::Arc;

use tracing::{info, warn};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    meta::{Condition, ConditionStatus, condition, set_condition},
    resources::{Network, NetworkSpec, NetworkStatus, Subnet, SubnetSpec, SubnetStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler, status::StatusWriter};

const WHO: &str = "network";

/// The condition this controller owns: whether the fabric knows this network.
const MIRRORED: &str = "Mirrored";

pub struct NetworkController {
    /// A network has no agent and never will — no machine owns a cell-wide
    /// fact — so the condition below goes through the narrow path that lets a
    /// controller write `status` on an object nobody holds. See
    /// [`crate::status`].
    say: StatusWriter<NetworkSpec, NetworkStatus>,
    subnets: TypedStore<SubnetSpec, SubnetStatus>,
    /// Where the fabric's orchestrator answers. `None` disables mirroring
    /// entirely, which is what a deployment with no fabric — a test cell, a
    /// developer's laptop — wants: the rest of the control plane works, and
    /// nothing pretends a data plane is being programmed.
    fabric: Option<Arc<str>>,
}

impl NetworkController {
    /// The networks themselves are not held here: the runner reads them and
    /// hands each one to `reconcile`, so a second handle would be a second way
    /// to read the same objects at a different moment.
    pub fn new(
        store: Arc<dyn velstra_cloud_store::Store>,
        cell: &str,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
        fabric: Option<String>,
    ) -> Self {
        Self {
            say: StatusWriter::new(store, cell, "networks", WHO),
            subnets,
            fabric: fabric.map(Arc::from),
        }
    }

    /// The one subnet this network's addresses come from, or why there isn't one.
    ///
    /// **A real mismatch between the two models, stated rather than papered
    /// over.** Fabric's network holds a single CIDR and validates every port's
    /// address against it (`ip is outside network N's subnet`); a cloud network
    /// holds subnets as separate objects and may have several. There is no
    /// faithful mirror of two subnets into one field.
    ///
    /// Widening to a supernet that contains both would make fabric accept
    /// addresses from neither — it would stop validating, quietly, and the first
    /// sign would be a port programmed onto the wrong segment. Mirroring the
    /// first would work for one subnet's ports and refuse the other's, with an
    /// error naming a subnet the operator never asked about. So a network with
    /// more than one subnet is **not mirrored**, and says so on itself.
    async fn subnet_for(&self, network: &str) -> Result<std::result::Result<Subnet, String>> {
        let mut found: Vec<Subnet> = self
            .subnets
            .list()
            .await?
            .into_iter()
            .filter(|s| s.spec.network == network)
            .collect();
        found.sort_by_key(|s| s.meta.name.to_string());
        Ok(match found.len() {
            0 => Err("no subnet yet, so there is no range to allocate ports from".into()),
            1 => Ok(found.remove(0)),
            n => Err(format!(
                "{n} subnets ({}), and the fabric's network holds one range against which it \
                 checks every port's address; mirroring one of them would refuse the others' \
                 ports and widening to cover both would stop the check meaning anything",
                found
                    .iter()
                    .map(|s| s.spec.cidr.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        })
    }
}

impl Reconciler for NetworkController {
    type Spec = NetworkSpec;
    type Status = NetworkStatus;

    fn name(&self) -> &'static str {
        "network"
    }

    async fn reconcile(&self, name: &str, object: Option<&Network>) -> Result<()> {
        let Some(network) = object else {
            // Gone. Retiring it on the fabric is deliberately not done here: a
            // network with ports still on it cannot be retired anyway, and this
            // controller has no way to know whether the ports are gone — that is
            // the node agents' half, and it is the reason a delete is not simply
            // the mirror run backwards. Left for the same pass that learns to
            // count them.
            return Ok(());
        };
        let Some(endpoint) = self.fabric.clone() else {
            return Ok(());
        };

        let subnet = match self.subnet_for(name).await? {
            Ok(subnet) => subnet,
            Err(why) => {
                return self
                    .say(network, ConditionStatus::False, "Unmirrorable", &why)
                    .await;
            }
        };

        // Restated on every pass rather than created once. Fabric replaces a
        // network of the same VNI, so this converges after a fabric restart, a
        // controller failover, or a subnet that arrived late — none of which
        // leave anything to repair by hand.
        let spec = pb::NetworkSpec {
            vni: network.spec.vni,
            name: name.to_string(),
            subnet: subnet.spec.cidr.clone(),
            // Deny by default, matching what the cloud tells fabric for a port's
            // own group. A network whose default were `pass` would let anything
            // the per-port groups had not thought of straight through.
            default_action: pb::Action::Drop as i32,
            drop_icmp: false,
        };
        // The subnet as an object of its own, not only as the network's range.
        //
        // The network's `subnet` field is what fabric validates a port's address
        // against; a fabric *Subnet* is what its IPAM allocates out of, and a
        // floating IP is allocated by id from one. Without this a floating IP
        // has no subnet to name and the whole collection is inert.
        //
        // Fabric's own pool is never drawn from — every address this control
        // plane hands out is decided here, by counting, and passed explicitly.
        // Declaring the gateway is still worth doing: it is the one address
        // fabric reserves itself, and it agrees with what this side reserves.
        let fabric_subnet = pb::SubnetSpec {
            id: subnet.meta.name.to_string(),
            vni: network.spec.vni,
            cidr: subnet.spec.cidr.clone(),
            gateway: subnet.spec.gateway.clone(),
            pool_start: String::new(),
            pool_end: String::new(),
            enable_dhcp: false,
        };
        match velstra_cloud_fabric::connect(&endpoint).await {
            Ok(mut client) => match client.add_network(spec).await {
                Ok(_) => match client.add_subnet(fabric_subnet).await {
                    Ok(_) => {
                        self.say(network, ConditionStatus::True, "Mirrored", "")
                            .await
                    }
                    Err(status) => {
                        warn!(network = %name, error = %status, "the fabric refused the subnet");
                        self.say(network, ConditionStatus::False, "Refused", status.message())
                            .await
                    }
                },
                Err(status) => {
                    warn!(network = %name, error = %status, "the fabric refused the network");
                    self.say(network, ConditionStatus::False, "Refused", status.message())
                        .await
                }
            },
            Err(e) => {
                // Unreachable is a wait, not a verdict: saying "Refused" here
                // would blame the network for the network being down.
                warn!(network = %name, error = %e, "cannot reach the fabric");
                Ok(())
            }
        }
    }
}

impl NetworkController {
    /// Record what happened on the network itself, and write only when it
    /// changed — a settled object must cost nothing.
    async fn say(
        &self,
        network: &Network,
        status: ConditionStatus,
        reason: &str,
        message: &str,
    ) -> Result<()> {
        let generation = network.meta.generation;
        let mut next = network.clone();
        let want = Condition::new(MIRRORED, status, reason, message, generation);
        // Written only when it changed. A settled network must cost nothing —
        // otherwise every resync of every network is a write, and the resync is
        // only affordable because a settled object writes nothing.
        if condition(&network.status.conditions, MIRRORED).is_some_and(|c| {
            c.status == want.status
                && c.reason == want.reason
                && c.message == want.message
                && c.observed_generation == generation
        }) {
            return Ok(());
        }
        set_condition(&mut next.status.conditions, want);
        next.status.observed_generation = generation;
        self.say.write(network, &next).await?;
        if status == ConditionStatus::True {
            info!(network = %network.meta.name, vni = network.spec.vni, "mirrored to the fabric");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const CELL: &str = "cell-1";

    fn stores() -> (
        Arc<dyn Store>,
        TypedStore<NetworkSpec, NetworkStatus>,
        TypedStore<SubnetSpec, SubnetStatus>,
    ) {
        let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
        (
            raw.clone(),
            TypedStore::new(raw.clone(), CELL, "networks"),
            TypedStore::new(raw, CELL, "subnets"),
        )
    }

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", CELL),
        )
    }

    async fn add_subnet(
        subnets: &TypedStore<SubnetSpec, SubnetStatus>,
        id: &str,
        network: &str,
        cidr: &str,
    ) {
        subnets
            .create(&Resource::new(
                meta(&format!("projects/p1/subnets/{id}")),
                SubnetSpec {
                    network: network.to_string(),
                    cidr: cidr.to_string(),
                    gateway: "10.0.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ))
            .await
            .unwrap();
    }

    /// One subnet is the ordinary case, and its CIDR is what the fabric is told.
    #[tokio::test]
    async fn a_network_with_one_subnet_is_mirrored_with_that_range() {
        let (raw, _networks, subnets) = stores();
        add_subnet(&subnets, "s1", "projects/p1/networks/n1", "10.20.0.0/24").await;
        let c = NetworkController::new(raw, CELL, subnets, None);

        let picked = c
            .subnet_for("projects/p1/networks/n1")
            .await
            .unwrap()
            .expect("a network with one subnet was not mirrorable");
        assert_eq!(picked.spec.cidr, "10.20.0.0/24");
    }

    /// A network nobody has given a subnet yet is not an error to shout about —
    /// it is a thing to wait for, and the reason says which.
    #[tokio::test]
    async fn a_network_with_no_subnet_says_what_it_is_waiting_for() {
        let (raw, _networks, subnets) = stores();
        let c = NetworkController::new(raw, CELL, subnets, None);

        let why = c
            .subnet_for("projects/p1/networks/n1")
            .await
            .unwrap()
            .expect_err("a network with no subnet was mirrored anyway");
        assert!(why.contains("no subnet"), "{why}");
    }

    /// Two subnets cannot be mirrored, and the refusal explains the mismatch
    /// rather than picking one.
    ///
    /// The fabric's network holds a single range and checks every port's address
    /// against it. Mirroring one of two would refuse the other's ports with an
    /// error naming a subnet the operator never asked about; widening to cover
    /// both would stop the check meaning anything. Neither is better than saying
    /// so.
    #[tokio::test]
    async fn a_network_with_two_subnets_is_refused_with_the_reason() {
        let (raw, _networks, subnets) = stores();
        add_subnet(&subnets, "s1", "projects/p1/networks/n1", "10.20.0.0/24").await;
        add_subnet(&subnets, "s2", "projects/p1/networks/n1", "10.21.0.0/24").await;
        let c = NetworkController::new(raw, CELL, subnets, None);

        let why = c
            .subnet_for("projects/p1/networks/n1")
            .await
            .unwrap()
            .expect_err("two subnets were mirrored into one range");
        assert!(
            why.contains("10.20.0.0/24") && why.contains("10.21.0.0/24"),
            "{why}"
        );
        assert!(
            why.contains("refuse") || why.contains("check"),
            "the refusal does not say what would go wrong: {why}"
        );
    }

    /// Only this network's subnets count. A subnet of another network in the
    /// same project must not decide this one's range.
    #[tokio::test]
    async fn another_networks_subnet_is_not_counted() {
        let (raw, _networks, subnets) = stores();
        add_subnet(&subnets, "s1", "projects/p1/networks/n1", "10.20.0.0/24").await;
        add_subnet(&subnets, "s2", "projects/p1/networks/OTHER", "10.99.0.0/24").await;
        let c = NetworkController::new(raw, CELL, subnets, None);

        let picked = c
            .subnet_for("projects/p1/networks/n1")
            .await
            .unwrap()
            .expect("a network with one subnet was not mirrorable");
        assert_eq!(picked.spec.cidr, "10.20.0.0/24");
    }

    /// With no fabric configured, a reconcile is a no-op rather than a failure.
    ///
    /// A cell with no data plane is a real arrangement — a test cell, a
    /// developer's laptop — and the control plane working there is the point.
    /// What must not happen is a network that claims to be mirrored.
    #[tokio::test]
    async fn with_no_fabric_nothing_is_mirrored_and_nothing_claims_to_be() {
        let (raw, networks, subnets) = stores();
        let network = Resource::new(
            meta("projects/p1/networks/n1"),
            NetworkSpec {
                vni: 5001,
                mtu: 1500,
            },
            NetworkStatus::default(),
        );
        networks.create(&network).await.unwrap();
        let c = NetworkController::new(raw, CELL, subnets, None);

        c.reconcile("projects/p1/networks/n1", Some(&network))
            .await
            .expect("a cell with no fabric must still reconcile");

        let after = networks
            .get("projects/p1/networks/n1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            condition(&after.status.conditions, MIRRORED).is_none(),
            "a network claimed a mirror state with no fabric to mirror to"
        );
    }
}
