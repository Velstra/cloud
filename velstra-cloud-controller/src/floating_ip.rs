//! Floating IPs: an address, and what it currently reaches.
//!
//! Three facts have to line up, and each is stated by exactly one thing:
//!
//! 1. **Which address.** Decided here, by counting what the subnet already
//!    holds — the same counting the ports use, from
//!    [`velstra_cloud_model::ipam`], because one range must have one allocator.
//!    Written into `spec`, like a port's address, so a person can pin it.
//! 2. **That the fabric holds it.** `AllocateFloatingIp` against the fabric
//!    subnet the network controller mirrored. The fabric answers with an id,
//!    and that id — not the address — is what every later call is keyed on, so
//!    it is recorded on the object.
//! 3. **What it reaches.** `spec.port` is what an operator asked for;
//!    `status.associated` is what the fabric has. The two differing is what a
//!    reconcile is *for*, and a floating IP pointing at nothing is not a
//!    failure — it is the state the whole idea exists to make possible.
//!
//! **Why the fabric is asked rather than remembered.** The association could be
//! inferred from `status`, and then a fabric that lost or never applied it would
//! never be corrected: the control plane would agree with itself forever while
//! the address reached nothing. Each pass reads the fabric's own answer and
//! makes it match.

use std::sync::Arc;

use tracing::{info, warn};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    access::Writer,
    ipam::{assign_floating, unaddressable_condition},
    meta::{Condition, ConditionStatus, condition, set_condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{
        FABRIC_RELEASE_FINALIZER, FloatingIp, FloatingIpSpec, FloatingIpStatus, Port, PortSpec,
        PortStatus, SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler, status::StatusWriter};

const WHO: &str = "floating-ip";

/// The condition this controller owns: whether the address exists and reaches
/// what it was told to.
const ALLOCATED: &str = "Allocated";

pub struct FloatingIpController {
    /// Written, for the address — a controller writing `spec` is the ordinary
    /// half of the split, and it is the same arrangement a port's address has.
    floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
    /// Written, for everything a controller observed. A floating IP has no
    /// agent: the port it names may not even be placed yet.
    say: StatusWriter<FloatingIpSpec, FloatingIpStatus>,
    subnets: TypedStore<SubnetSpec, SubnetStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    fabric: Option<Arc<str>>,
}

impl FloatingIpController {
    pub fn new(
        store: Arc<dyn velstra_cloud_store::Store>,
        cell: &str,
        floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
        ports: TypedStore<PortSpec, PortStatus>,
        fabric: Option<String>,
    ) -> Self {
        Self {
            floating,
            say: StatusWriter::new(store, cell, "floatingips", WHO),
            subnets,
            ports,
            fabric: fabric.map(Arc::from),
        }
    }

    /// Give this floating IP an address if it has none.
    ///
    /// Returns whether one was written. A write is the end of the pass: it bumps
    /// the generation, the queue hands the object back, and the next pass sees a
    /// floating IP that has an address — rather than this one carrying on with a
    /// copy it wrote and might not have.
    async fn address(&self, fip: &FloatingIp) -> Result<bool> {
        if fip.spec.address.is_some() {
            return Ok(false);
        }
        let subnet = self.subnets.get(&fip.spec.subnet).await?;
        let ports = self.ports.list().await?;
        let others = self.floating.list().await?;
        match assign_floating(fip, subnet.as_ref(), &ports, &others) {
            Ok(None) => Ok(false),
            Ok(Some(address)) => {
                let mut next = fip.clone();
                next.spec.address = Some(address.clone());
                // The spec moved, so the generation moves with it — that is what
                // makes anything downstream notice, and the store refuses a
                // spec change that forgot it.
                next.meta.generation += 1;
                self.floating
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(floating_ip = %fip.meta.name, %address, "gave a floating IP its address");
                Ok(true)
            }
            Err(why) => {
                // The same words a port gets for the same three situations, so
                // an operator reads one vocabulary and not two.
                let want = unaddressable_condition(&why, fip.meta.generation);
                let mut next = fip.clone();
                set_condition(&mut next.status.conditions, want);
                next.status.observed_generation = fip.meta.generation;
                if next.status != fip.status {
                    self.say.write(fip, &next).await?;
                }
                Ok(true)
            }
        }
    }

    /// The fabric's own view of this address, allocating it if it has none.
    async fn on_fabric(
        &self,
        client: &mut velstra_cloud_fabric::Connected,
        fip: &FloatingIp,
        address: &str,
    ) -> std::result::Result<pb::FloatingIpInfo, Box<velstra_cloud_fabric::Status>> {
        if !fip.status.fabric_id.is_empty() {
            // Asked, not assumed: an id this side remembers and the fabric has
            // forgotten — a fabric restored from an older snapshot, a cell
            // rebuilt — must become an allocation again, not a run of calls
            // against an id that names nothing.
            let known = client
                .list_floating_ips(pb::ListFloatingIpsRequest {})
                .await
                .map_err(Box::new)?
                .into_inner()
                .floating_ips
                .into_iter()
                .find(|f| f.id == fip.status.fabric_id);
            if let Some(known) = known {
                return Ok(known);
            }
            warn!(
                floating_ip = %fip.meta.name,
                fabric_id = %fip.status.fabric_id,
                "the fabric no longer knows this address; allocating it again"
            );
        }
        Ok(client
            .allocate_floating_ip(pb::AllocateFloatingIpRequest {
                subnet_id: fip.spec.subnet.clone(),
                // Never empty. The address was decided by the counting above,
                // and letting the fabric pick would be the second allocator this
                // whole design exists to not have.
                ip: address.to_string(),
            })
            .await
            .map_err(Box::new)?
            .into_inner())
    }

    /// The fabric's id for the port this floating IP names, and the fixed
    /// address to forward to — or why there isn't one yet.
    ///
    /// Matched on `(tap, host)` for the same reason the node agent's teardown
    /// is: a tap name is deliberately identical on every node, so a match on the
    /// name alone would find a *different* guest's port on another machine.
    async fn target(
        &self,
        client: &mut velstra_cloud_fabric::Connected,
        port: &Port,
    ) -> std::result::Result<
        std::result::Result<(String, String), String>,
        Box<velstra_cloud_fabric::Status>,
    > {
        let (Some(tap), Some(node)) = (
            port.status.tap_device.as_deref(),
            port.status.node.as_deref(),
        ) else {
            return Ok(Err(format!(
                "{} is not on a node yet, so there is nothing to forward to",
                port.meta.name
            )));
        };
        let host = node.strip_prefix("nodes/").unwrap_or(node);
        let Some(address) = port.spec.address.as_deref() else {
            return Ok(Err(format!(
                "{} has no address of its own yet",
                port.meta.name
            )));
        };
        let found = client
            .list_ports(pb::ListPortsRequest {})
            .await
            .map_err(Box::new)?
            .into_inner()
            .ports
            .into_iter()
            .find(|p| p.tap == tap && p.host == host);
        Ok(match found {
            Some(p) => Ok((p.id, address.to_string())),
            None => Err(format!(
                "{} is placed on {host} but the fabric has no port for it yet",
                port.meta.name
            )),
        })
    }

    /// Hand the address back to the fabric, association first.
    ///
    /// The fabric refuses to release one that is still associated, and it is
    /// right to: an address released out from under a live association would
    /// leave a port forwarding from an address somebody else can now be given.
    async fn release(&self, fip: &FloatingIp) -> std::result::Result<(), String> {
        if fip.status.fabric_id.is_empty() {
            // Never reached the fabric, so there is nothing there to hand back.
            return Ok(());
        }
        let Some(endpoint) = self.fabric.clone() else {
            // A cell with no fabric never allocated it either. Refusing to let
            // the object go here would pin every floating IP in such a cell
            // forever.
            return Ok(());
        };
        let mut client = velstra_cloud_fabric::connect(&endpoint)
            .await
            .map_err(|e| e.to_string())?;
        let id = fip.status.fabric_id.clone();
        let known = client
            .list_floating_ips(pb::ListFloatingIpsRequest {})
            .await
            .map_err(|e| e.message().to_string())?
            .into_inner()
            .floating_ips
            .into_iter()
            .find(|f| f.id == id);
        let Some(known) = known else {
            // Already gone from the fabric. Releasing again would be an error
            // about a thing that is already in the state being asked for.
            return Ok(());
        };
        if !known.assoc_port.is_empty() {
            client
                .disassociate_floating_ip(pb::DisassociateFloatingIpRequest { id: id.clone() })
                .await
                .map_err(|e| e.message().to_string())?;
        }
        client
            .release_floating_ip(pb::ReleaseFloatingIpRequest { id })
            .await
            .map_err(|e| e.message().to_string())?;
        Ok(())
    }
}

impl Reconciler for FloatingIpController {
    type Spec = FloatingIpSpec;
    type Status = FloatingIpStatus;

    fn name(&self) -> &'static str {
        "floating-ip"
    }

    async fn reconcile(&self, name: &str, object: Option<&FloatingIp>) -> Result<()> {
        let Some(fip) = object else {
            // Gone, and gone properly: the finalizer below means the fabric had
            // already let the address go before the record could disappear.
            return Ok(());
        };

        // The guard goes on before the address is ever taken, and comes off only
        // after the fabric has let it go. Added afterwards there would be a
        // window in which a delete takes the record and leaves the allocation —
        // and the id needed to release it lives on the record.
        match finalizer_step(&fip.meta, FABRIC_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                let mut next = fip.clone();
                next.meta.add_finalizer(FABRIC_RELEASE_FINALIZER);
                self.floating
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                return Ok(());
            }
            // Every guard is off, this one included — the record is the store's
            // to remove now, and there is nothing left here to hold it up.
            FinalizerStep::Delete => return Ok(()),
            FinalizerStep::Wait if fip.meta.is_deleting() => {
                if !fip.meta.has_finalizer(FABRIC_RELEASE_FINALIZER) {
                    // Somebody else's guard is still on. Waiting is the whole
                    // point of theirs, and this one is already released.
                    return Ok(());
                }
                // Let the fabric go first, then the guard: the other order is
                // the leak this exists to prevent.
                if let Err(why) = self.release(fip).await {
                    warn!(floating_ip = %name, error = %why, "cannot release the address yet");
                    return Ok(());
                }
                let mut next = fip.clone();
                next.meta.remove_finalizer(FABRIC_RELEASE_FINALIZER);
                self.floating
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(floating_ip = %name, "the fabric let the address go; the guard is off");
                return Ok(());
            }
            FinalizerStep::Wait => {}
        }

        // No `..`: every field of a floating IP is acted on below, and a new one
        // is a compile error here until somebody says how.
        let FloatingIpSpec {
            subnet: _,  // where the address comes from — used by `address`
            address: _, // the address itself — allocated on the fabric below
            port: _,    // what it reaches — associated or detached below
        } = &fip.spec;

        // The address first, and always: a cell with no fabric still decides it,
        // so an operator sees what they will get before any data plane exists.
        if self.address(fip).await? {
            return Ok(());
        }
        let Some(address) = fip.spec.address.clone() else {
            return Ok(());
        };
        let Some(endpoint) = self.fabric.clone() else {
            return Ok(());
        };

        let mut client = match velstra_cloud_fabric::connect(&endpoint).await {
            Ok(client) => client,
            Err(e) => {
                // Unreachable is a wait, not a verdict.
                warn!(floating_ip = %name, error = %e, "cannot reach the fabric");
                return Ok(());
            }
        };

        let info = match self.on_fabric(&mut client, fip, &address).await {
            Ok(info) => info,
            Err(status) => {
                warn!(floating_ip = %name, error = %status, "the fabric refused the address");
                return self
                    .settle(
                        fip,
                        ConditionStatus::False,
                        "Refused",
                        status.message(),
                        None,
                    )
                    .await;
            }
        };

        // What it should reach.
        if fip.spec.port.is_empty() {
            if !info.assoc_port.is_empty()
                && let Err(status) = client
                    .disassociate_floating_ip(pb::DisassociateFloatingIpRequest {
                        id: info.id.clone(),
                    })
                    .await
            {
                warn!(floating_ip = %name, error = %status, "the fabric refused to detach");
                return self
                    .settle(
                        fip,
                        ConditionStatus::False,
                        "Refused",
                        status.message(),
                        Some((info.id, info.assoc_fixed)),
                    )
                    .await;
            }
            return self
                .settle(
                    fip,
                    ConditionStatus::True,
                    "Held",
                    "allocated and forwarding to nothing",
                    Some((info.id, String::new())),
                )
                .await;
        }

        let Some(port) = self.ports.get(&fip.spec.port).await? else {
            return self
                .settle(
                    fip,
                    ConditionStatus::False,
                    "NoSuchPort",
                    &format!("{} does not exist", fip.spec.port),
                    Some((info.id, info.assoc_fixed)),
                )
                .await;
        };
        let (port_id, fixed) = match self.target(&mut client, &port).await {
            Ok(Ok(target)) => target,
            Ok(Err(why)) => {
                return self
                    .settle(
                        fip,
                        ConditionStatus::False,
                        "PortNotReady",
                        &why,
                        Some((info.id, info.assoc_fixed)),
                    )
                    .await;
            }
            Err(status) => {
                warn!(floating_ip = %name, error = %status, "asking the fabric for the port");
                return Ok(());
            }
        };

        if info.assoc_port == port_id && info.assoc_fixed == fixed {
            return self
                .settle(
                    fip,
                    ConditionStatus::True,
                    "Forwarding",
                    "",
                    Some((info.id, fixed)),
                )
                .await;
        }
        // An association is one-to-one, so a move goes through nothing: leaving
        // the old one in place and asking for a new one is a refusal, and the
        // address would stay pointed at the machine that was replaced.
        if !info.assoc_port.is_empty()
            && let Err(status) = client
                .disassociate_floating_ip(pb::DisassociateFloatingIpRequest {
                    id: info.id.clone(),
                })
                .await
        {
            warn!(floating_ip = %name, error = %status, "the fabric refused to detach");
            return self
                .settle(
                    fip,
                    ConditionStatus::False,
                    "Refused",
                    status.message(),
                    Some((info.id, info.assoc_fixed)),
                )
                .await;
        }
        match client
            .associate_floating_ip(pb::AssociateFloatingIpRequest {
                id: info.id.clone(),
                port_id,
                fixed_addr: fixed.clone(),
            })
            .await
        {
            Ok(_) => {
                self.settle(
                    fip,
                    ConditionStatus::True,
                    "Forwarding",
                    "",
                    Some((info.id, fixed)),
                )
                .await
            }
            Err(status) => {
                warn!(floating_ip = %name, error = %status, "the fabric refused the association");
                self.settle(
                    fip,
                    ConditionStatus::False,
                    "Refused",
                    status.message(),
                    Some((info.id, String::new())),
                )
                .await
            }
        }
    }
}

impl FloatingIpController {
    /// Record what happened, and write only when it changed — a settled object
    /// must cost nothing.
    async fn settle(
        &self,
        fip: &FloatingIp,
        status: ConditionStatus,
        reason: &str,
        message: &str,
        observed: Option<(String, String)>,
    ) -> Result<()> {
        let generation = fip.meta.generation;
        let mut next = fip.clone();
        if let Some((fabric_id, associated)) = observed {
            next.status.fabric_id = fabric_id;
            next.status.associated = associated;
        }
        let want = Condition::new(ALLOCATED, status, reason, message, generation);
        let settled = condition(&fip.status.conditions, ALLOCATED).is_some_and(|c| {
            c.status == want.status
                && c.reason == want.reason
                && c.message == want.message
                && c.observed_generation == generation
        });
        // The observed halves count as part of "settled": an association that
        // moved while the condition stayed `Forwarding` would otherwise never be
        // written down, and `status.associated` is the only place an operator
        // can see *what* it forwards to.
        if settled
            && next.status.fabric_id == fip.status.fabric_id
            && next.status.associated == fip.status.associated
        {
            return Ok(());
        }
        set_condition(&mut next.status.conditions, want);
        next.status.observed_generation = generation;
        self.say.write(fip, &next).await?;
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
    const SUBNET: &str = "projects/p1/subnets/s1";
    const FIP: &str = "projects/p1/floatingips/f1";

    struct Fixture {
        raw: Arc<dyn Store>,
        floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
        ports: TypedStore<PortSpec, PortStatus>,
    }

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", CELL),
        )
    }

    impl Fixture {
        fn new() -> Self {
            let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
            Self {
                floating: TypedStore::new(raw.clone(), CELL, "floatingips"),
                subnets: TypedStore::new(raw.clone(), CELL, "subnets"),
                ports: TypedStore::new(raw.clone(), CELL, "ports"),
                raw,
            }
        }

        fn controller(&self) -> FloatingIpController {
            FloatingIpController::new(
                self.raw.clone(),
                CELL,
                self.floating.clone(),
                self.subnets.clone(),
                self.ports.clone(),
                None,
            )
        }

        async fn subnet(&self, cidr: &str) {
            self.subnets
                .create(&Resource::new(
                    meta(SUBNET),
                    SubnetSpec {
                        network: "projects/p1/networks/n1".into(),
                        cidr: cidr.into(),
                        gateway: "10.20.0.1".into(),
                        dns: vec![],
                        reserved: vec![],
                    },
                    SubnetStatus::default(),
                ))
                .await
                .unwrap();
        }

        async fn port(&self, id: &str, address: &str) {
            self.ports
                .create(&Resource::new(
                    meta(&format!("projects/p1/ports/{id}")),
                    PortSpec {
                        network: "projects/p1/networks/n1".into(),
                        subnet: SUBNET.into(),
                        address: Some(address.into()),
                        ..Default::default()
                    },
                    PortStatus::default(),
                ))
                .await
                .unwrap();
        }

        async fn fip(&self, address: Option<&str>) -> FloatingIp {
            let f = Resource::new(
                meta(FIP),
                FloatingIpSpec {
                    subnet: SUBNET.into(),
                    address: address.map(str::to_string),
                    port: String::new(),
                },
                FloatingIpStatus::default(),
            );
            self.floating.create(&f).await.unwrap();
            self.floating.get(FIP).await.unwrap().unwrap()
        }
    }

    /// The ordinary case: a floating IP with no address is given the lowest one
    /// nothing else holds, and it is written where a person can see and pin it.
    #[tokio::test]
    async fn a_floating_ip_is_given_an_address() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        let fip = f.fip(None).await;

        assert!(f.controller().address(&fip).await.unwrap());

        let after = f.floating.get(FIP).await.unwrap().unwrap();
        assert_eq!(after.spec.address.as_deref(), Some("10.20.0.2"));
    }

    /// And it is not an address a port already holds. This is the invariant the
    /// whole design turns on, tested through the controller rather than only
    /// through the arithmetic — the controller is what actually reads the ports.
    #[tokio::test]
    async fn the_address_is_not_one_a_port_holds() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        f.port("web", "10.20.0.2").await;
        f.port("db", "10.20.0.3").await;
        let fip = f.fip(None).await;

        f.controller().address(&fip).await.unwrap();

        let after = f.floating.get(FIP).await.unwrap().unwrap();
        assert_eq!(after.spec.address.as_deref(), Some("10.20.0.4"));
    }

    /// A second pass over an addressed floating IP writes nothing. A settled
    /// object must cost nothing, and an address that moved on every pass would
    /// not be an address.
    #[tokio::test]
    async fn an_addressed_floating_ip_is_left_alone() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        let fip = f.fip(Some("10.20.0.99")).await;

        assert!(!f.controller().address(&fip).await.unwrap());

        let after = f.floating.get(FIP).await.unwrap().unwrap();
        assert_eq!(after.spec.address.as_deref(), Some("10.20.0.99"));
        assert_eq!(
            after.meta.revision, fip.meta.revision,
            "a settled object was written"
        );
    }

    /// A subnet that does not exist yet is a wait, said on the object in the
    /// same words a port gets.
    #[tokio::test]
    async fn a_floating_ip_with_no_subnet_says_so_in_the_ports_words() {
        let f = Fixture::new();
        let fip = f.fip(None).await;

        f.controller().address(&fip).await.unwrap();

        let after = f.floating.get(FIP).await.unwrap().unwrap();
        let said = condition(&after.status.conditions, "Ready")
            .expect("nothing was said about a floating IP that could not be given an address");
        assert_eq!(said.reason, "NoSuchSubnet");
        assert!(after.spec.address.is_none());
    }

    /// With no fabric configured, the address is still decided — an operator can
    /// see what they will get before any data plane exists — and nothing claims
    /// to be allocated on a fabric that is not there.
    #[tokio::test]
    async fn with_no_fabric_the_address_is_decided_and_nothing_claims_more() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        f.fip(None).await;
        let c = f.controller();

        // Twice: the first pass writes the address and stops, the second finds
        // nothing left it can do without a fabric.
        for _ in 0..2 {
            let now = f.floating.get(FIP).await.unwrap().unwrap();
            c.reconcile(FIP, Some(&now)).await.unwrap();
        }

        let after = f.floating.get(FIP).await.unwrap().unwrap();
        assert_eq!(after.spec.address.as_deref(), Some("10.20.0.2"));
        assert!(
            condition(&after.status.conditions, ALLOCATED).is_none(),
            "a floating IP claimed an allocation with no fabric to hold it"
        );
        assert!(after.status.fabric_id.is_empty());
    }
}
