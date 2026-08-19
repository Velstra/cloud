//! Addresses: read the subnet, ask [`velstra_cloud_model::ipam::assign`], write
//! one field.
//!
//! The same shape as the scheduler, and for the same reasons. There is no
//! allocation table and no reservation: what is taken is counted from the ports
//! that exist, so a controller that dies mid-decision leaves nothing behind,
//! and two controllers looking at one port produce one assignment and one
//! retry — the compare-and-swap is the whole race protocol.
//!
//! Why a controller and not the API at create: an address that could not be
//! given yet is an ordinary state to be in (the subnet object arrives a moment
//! later, or is full until somebody deletes something), and a create that
//! refused would make a port's existence depend on the order two objects
//! happened to be written in. Here the port exists, says on itself why it has
//! no address, and gets one the moment it can.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    ipam::{assign, needs_assignment, unaddressable_condition},
    meta::{condition, set_condition},
    resources::{
        FloatingIpSpec, FloatingIpStatus, Port, PortSpec, PortStatus, SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{Related, Result, runner::Reconciler, status::StatusWriter};

pub struct AddressController {
    ports: TypedStore<PortSpec, PortStatus>,
    subnets: TypedStore<SubnetSpec, SubnetStatus>,
    /// Read, never written: a floating address is one this controller must not
    /// hand to a port.
    floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
    status: StatusWriter<PortSpec, PortStatus>,
    cell: String,
    /// Ports that could not be given an address, so that a subnet appearing or
    /// being emptied wakes exactly them. A hint for the queue and never a fact
    /// about the world: losing it in a restart costs one resync of latency.
    pending: Arc<Mutex<BTreeSet<String>>>,
}

impl AddressController {
    pub fn new(
        ports: TypedStore<PortSpec, PortStatus>,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
        floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
        status: StatusWriter<PortSpec, PortStatus>,
        cell: &str,
    ) -> Self {
        Self {
            ports,
            subnets,
            floating,
            status,
            cell: cell.to_string(),
            pending: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Say on the port why it has no address.
    ///
    /// Only while no node owns the port's status — once an agent has claimed
    /// it, the agent is the only writer, and this controller saying anything
    /// there would be the two-writers bug the whole model exists to prevent. A
    /// port an agent has claimed already has an address anyway, or the guest
    /// would not have started.
    async fn explain(
        &self,
        port: &Port,
        why: &velstra_cloud_model::ipam::Unaddressable,
    ) -> Result<()> {
        if port.status.node.is_some() {
            return Ok(());
        }
        let mut next = port.clone();
        set_condition(
            &mut next.status.conditions,
            unaddressable_condition(why, port.meta.generation),
        );
        self.status.write(port, &next).await?;
        Ok(())
    }
}

impl Reconciler for AddressController {
    type Spec = PortSpec;
    type Status = PortStatus;

    fn name(&self) -> &'static str {
        "address"
    }

    fn related(&self) -> Vec<Related> {
        // A subnet arriving, or one that was full gaining a free address, is
        // what unblocks everything waiting. Without this an operator who has
        // just made room waits out the resync interval wondering whether
        // anything noticed.
        let pending = self.pending.clone();
        vec![Related::named(
            prefix_for(&self.cell, "subnets"),
            move |_subnet: &str| pending.lock().unwrap().iter().cloned().collect(),
        )]
    }

    async fn reconcile(&self, name: &str, object: Option<&Port>) -> Result<()> {
        let Some(port) = object else {
            self.pending.lock().unwrap().remove(name);
            return Ok(());
        };
        if !needs_assignment(port) {
            self.pending.lock().unwrap().remove(name);
            return Ok(());
        }

        let subnet = self.subnets.get(&port.spec.subnet).await?;
        let others = self.ports.list().await?;
        // Floating IPs come out of the same range. Reading them here is what
        // keeps one allocator over that range rather than two — see
        // [`velstra_cloud_model::ipam`].
        let floating = self.floating.list().await?;

        let assignment = match assign(port, subnet.as_ref(), &others, &floating) {
            Ok(assignment) => assignment,
            Err(why) => {
                self.explain(port, &why).await?;
                self.pending.lock().unwrap().insert(name.to_string());
                return Ok(());
            }
        };
        if assignment.is_empty() {
            self.pending.lock().unwrap().remove(name);
            return Ok(());
        }

        let mut next = port.clone();
        // A stale refusal on a port that has just been given an address would
        // otherwise sit there until its node first reports. Written first and
        // separately, because a spec write and a status write are two different
        // writers' halves and the store refuses one that does both.
        if condition(&port.status.conditions, "Ready").is_some() && port.status.node.is_none() {
            let mut corrected = port.clone();
            set_condition(
                &mut corrected.status.conditions,
                velstra_cloud_model::Condition::new(
                    "Ready",
                    velstra_cloud_model::ConditionStatus::Unknown,
                    "Addressed",
                    "waiting for a node to program it",
                    port.meta.generation,
                ),
            );
            if let Some(revision) = self.status.write(port, &corrected).await? {
                corrected.meta.revision = revision;
                next = corrected;
            }
        }

        if let Some(address) = assignment.address {
            next.spec.address = Some(address);
        }
        if let Some(mac) = assignment.mac {
            next.spec.mac = Some(mac);
        }
        next.meta.generation += 1;
        self.ports
            .update(&next, &Writer::controller("address"))
            .await?;
        self.pending.lock().unwrap().remove(name);
        info!(
            port = name,
            address = next.spec.address.as_deref().unwrap_or("-"),
            mac = next.spec.mac.as_deref().unwrap_or("-"),
            "addressed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::{Resource, Subnet},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    struct Fixture {
        raw: Arc<MemoryStore>,
        ports: TypedStore<PortSpec, PortStatus>,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
    }

    fn fixture() -> Fixture {
        let raw = Arc::new(MemoryStore::new());
        Fixture {
            ports: TypedStore::new(raw.clone(), "cell-1", "ports"),
            subnets: TypedStore::new(raw.clone(), "cell-1", "subnets"),
            raw,
        }
    }

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu", "cell-1"),
        )
    }

    impl Fixture {
        fn controller(&self) -> AddressController {
            AddressController::new(
                self.ports.clone(),
                self.subnets.clone(),
                TypedStore::new(self.raw.clone(), "cell-1", "floatingips"),
                StatusWriter::new(self.raw.clone(), "cell-1", "ports", "address"),
                "cell-1",
            )
        }

        async fn subnet(&self, cidr: &str) -> Subnet {
            let s = Resource::new(
                meta("projects/p1/subnets/sub-a"),
                SubnetSpec {
                    network: "projects/p1/networks/net-a".into(),
                    cidr: cidr.into(),
                    gateway: "10.20.0.1".into(),
                    dns: vec!["10.20.0.1".into()],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            );
            self.subnets.create(&s).await.unwrap();
            s
        }

        async fn port(&self, id: &str) -> Port {
            self.port_owned_by(id, None).await
        }

        /// `create` does not go through the access rule, which is what lets a
        /// test arrange a port a node has already claimed without a node.
        async fn port_owned_by(&self, id: &str, node: Option<&str>) -> Port {
            let p = Resource::new(
                meta(&format!("projects/p1/ports/{id}")),
                PortSpec {
                    network: "projects/p1/networks/net-a".into(),
                    subnet: "projects/p1/subnets/sub-a".into(),
                    ..Default::default()
                },
                PortStatus {
                    node: node.map(str::to_string),
                    ..Default::default()
                },
            );
            self.ports.create(&p).await.unwrap();
            p
        }

        /// Change a port the way its writer would: read it fresh, then write.
        /// The revision on the copy is the compare-and-swap, so a test that
        /// edited the object it was handed at create would lose to itself.
        async fn edit(&self, id: &str, writer: &Writer, edit: impl FnOnce(&mut Port)) {
            let mut port = self.read(id).await;
            edit(&mut port);
            self.ports.update(&port, writer).await.unwrap();
        }

        async fn read(&self, id: &str) -> Port {
            self.ports
                .get(&format!("projects/p1/ports/{id}"))
                .await
                .unwrap()
                .unwrap()
        }

        async fn run(&self, id: &str) {
            let name = format!("projects/p1/ports/{id}");
            let port = self.ports.get(&name).await.unwrap();
            self.controller()
                .reconcile(&name, port.as_ref())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_port_created_with_nothing_is_given_an_address_and_a_mac() {
        // What an operator actually types: a port on a subnet, and no address.
        let f = fixture();
        f.subnet("10.20.0.0/24").await;
        f.port("port-a").await;
        f.run("port-a").await;

        let port = f.read("port-a").await;
        assert_eq!(port.spec.address.as_deref(), Some("10.20.0.2"));
        assert!(port.spec.mac.is_some());
        // The generation moved with the spec, or no agent would notice.
        assert_eq!(port.meta.generation, 2);
    }

    #[tokio::test]
    async fn two_ports_on_one_subnet_never_get_one_address() {
        let f = fixture();
        f.subnet("10.20.0.0/24").await;
        f.port("port-a").await;
        f.port("port-b").await;
        f.run("port-a").await;
        f.run("port-b").await;

        let a = f.read("port-a").await;
        let b = f.read("port-b").await;
        assert_ne!(a.spec.address, b.spec.address);
        assert_ne!(a.spec.mac, b.spec.mac);
    }

    #[tokio::test]
    async fn a_settled_port_is_written_to_no_further() {
        // The property that makes the resync interval a matter of taste: a
        // second pass over a port that has everything writes nothing at all.
        let f = fixture();
        f.subnet("10.20.0.0/24").await;
        f.port("port-a").await;
        f.run("port-a").await;
        let revision = f.raw.revision().await.unwrap();
        f.run("port-a").await;
        assert_eq!(
            f.raw.revision().await.unwrap(),
            revision,
            "a settled port was written again"
        );
    }

    #[tokio::test]
    async fn a_port_whose_subnet_is_not_there_yet_says_so_and_is_addressed_when_it_arrives() {
        let f = fixture();
        f.port("port-a").await;
        f.run("port-a").await;

        let port = f.read("port-a").await;
        assert_eq!(port.spec.address, None);
        let ready = condition(&port.status.conditions, "Ready").expect("nothing said why");
        assert_eq!(ready.reason, "NoSuchSubnet");

        // …and the subnet turning up is enough, with no operator involved.
        f.subnet("10.20.0.0/24").await;
        f.run("port-a").await;
        assert_eq!(
            f.read("port-a").await.spec.address.as_deref(),
            Some("10.20.0.2")
        );
    }

    #[tokio::test]
    async fn a_full_subnet_is_a_sentence_on_the_port_rather_than_silence() {
        let f = fixture();
        // A /30 has two host addresses and the gateway is one of them.
        f.subnet("10.20.0.0/30").await;
        f.port("port-a").await;
        f.port("port-b").await;
        f.run("port-a").await;
        f.run("port-b").await;

        assert!(f.read("port-a").await.spec.address.is_some());
        let stuck = f.read("port-b").await;
        assert_eq!(stuck.spec.address, None);
        let ready = condition(&stuck.status.conditions, "Ready").unwrap();
        assert_eq!(ready.reason, "SubnetFull");
        assert!(ready.message.contains("10.20.0.0/30"), "{}", ready.message);
    }

    #[tokio::test]
    async fn a_port_a_node_already_owns_is_not_spoken_for() {
        // Two writers on one status is the bug the model exists to prevent, so
        // a port whose agent has claimed it gets no condition from here even
        // when there is something to say.
        let f = fixture();
        f.port_owned_by("port-a", Some("node-a")).await;

        f.run("port-a").await;
        let port = f.read("port-a").await;
        assert!(
            port.status.conditions.is_empty(),
            "{:?}",
            port.status.conditions
        );
    }

    #[tokio::test]
    async fn a_port_being_deleted_is_left_alone() {
        // Filling in an address on the way out moves the generation under an
        // agent that is trying to let go of the object.
        let f = fixture();
        f.subnet("10.20.0.0/24").await;
        f.port("port-a").await;
        f.edit("port-a", &Writer::controller("test"), |p| {
            p.meta.deleted_at = Some(Timestamp::now())
        })
        .await;

        f.run("port-a").await;
        assert_eq!(f.read("port-a").await.spec.address, None);
    }

    #[tokio::test]
    async fn an_address_an_operator_chose_is_kept() {
        // Stating one is allowed; being overruled by a controller is not.
        let f = fixture();
        f.subnet("10.20.0.0/24").await;
        f.port("port-a").await;
        f.edit("port-a", &Writer::controller("test"), |p| {
            p.spec.address = Some("10.20.0.99".into());
            p.meta.generation += 1;
        })
        .await;

        f.run("port-a").await;
        let port = f.read("port-a").await;
        assert_eq!(port.spec.address.as_deref(), Some("10.20.0.99"));
        assert!(port.spec.mac.is_some(), "the MAC was not filled in either");
    }
}
