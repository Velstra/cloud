//! Load balancers: a VIP, and the fabric services that answer on it.
//!
//! Three facts have to line up, and each is stated by exactly one thing:
//!
//! 1. **Which address.** Decided here, by the same counting the ports and the
//!    floating IPs use — [`velstra_cloud_model::ipam::assign_vip`] — because
//!    one range must have one allocator. Written into `spec`, so a person can
//!    pin it.
//! 2. **What the fabric holds.** One load-balanced service per listener, under
//!    an id derived from the resource name ([`service_id`]), so a failover
//!    re-derives rather than remembers and a teardown can find its services
//!    with nothing but the name.
//! 3. **Who is in the pool.** `spec.members` names ports; the fabric wants its
//!    own port ids. The mapping is resolved on every pass and **refused rather
//!    than thinned**: a pool programmed with three of its four members looks
//!    balanced and quietly drops a quarter of nothing — it sends every fourth
//!    connection's worth of load to machines that were never told, and nothing
//!    anywhere says so. Waiting costs a reconcile; guessing costs a debugging
//!    session. (The router learned this exact lesson with its networks.)
//!
//! **Why there is a finalizer.** A floating IP holds one because the fabric id
//! it needs to release lives on the record; here the ids are derived, so the
//! release *could* run after the record is gone — but only if this process
//! sees the delete event. A controller that was down for the window would
//! leave a VIP answering forever, with no object left to say it exists. The
//! guard makes the teardown a fact on the object instead of a race with the
//! watch.

use std::sync::Arc;

use tracing::{info, warn};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    access::Writer,
    ipam::{assign_vip, unaddressable_condition},
    loadbalancer::{
        FabricMember, FabricService, LoadBalancer, LoadBalancerSpec, LoadBalancerStatus,
        MirrorAction, ObservedListener, Protocol, desired_services, mirror_actions,
        observed_listeners, owns_service, validate_listeners,
    },
    meta::{Condition, ConditionStatus, condition, set_condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{
        FABRIC_RELEASE_FINALIZER, NetworkSpec, NetworkStatus, PortSpec, PortStatus, SubnetSpec,
        SubnetStatus,
    },
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler, status::StatusWriter};

const WHO: &str = "load-balancer";

/// The condition this controller owns: whether the fabric answers on the VIP
/// as asked.
const READY: &str = "Ready";

pub struct LoadBalancerController {
    /// Written, for the VIP and the finalizer — a controller writing `spec`
    /// and metadata is the ordinary half of the split.
    balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus>,
    /// Written, for everything this controller observed. A load balancer has
    /// no agent: balancing happens on whichever host the packet arrives at.
    say: StatusWriter<LoadBalancerSpec, LoadBalancerStatus>,
    networks: TypedStore<NetworkSpec, NetworkStatus>,
    subnets: TypedStore<SubnetSpec, SubnetStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    floating: TypedStore<
        velstra_cloud_model::resources::FloatingIpSpec,
        velstra_cloud_model::resources::FloatingIpStatus,
    >,
    fabric: Option<Arc<str>>,
}

impl LoadBalancerController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn velstra_cloud_store::Store>,
        cell: &str,
        balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus>,
        networks: TypedStore<NetworkSpec, NetworkStatus>,
        subnets: TypedStore<SubnetSpec, SubnetStatus>,
        ports: TypedStore<PortSpec, PortStatus>,
        floating: TypedStore<
            velstra_cloud_model::resources::FloatingIpSpec,
            velstra_cloud_model::resources::FloatingIpStatus,
        >,
        fabric: Option<String>,
    ) -> Self {
        Self {
            balancers,
            say: StatusWriter::new(store, cell, "load-balancers", WHO),
            networks,
            subnets,
            ports,
            floating,
            fabric: fabric.map(Arc::from),
        }
    }

    /// Give this load balancer its VIP if it has none.
    ///
    /// Returns whether the pass is over — a write bumps the generation and the
    /// queue hands the object back, and a refusal was said on the object.
    async fn address(&self, lb: &LoadBalancer) -> Result<bool> {
        if lb.spec.vip.is_some() {
            return Ok(false);
        }
        let subnet = self.subnets.get(&lb.spec.subnet).await?;
        let ports = self.ports.list().await?;
        let floating = self.floating.list().await?;
        let others = self.balancers.list().await?;
        match assign_vip(lb, subnet.as_ref(), &ports, &floating, &others) {
            Ok(None) => Ok(false),
            Ok(Some(vip)) => {
                let mut next = lb.clone();
                next.spec.vip = Some(vip.clone());
                // The spec moved, so the generation moves with it — the store
                // refuses a spec change that forgot.
                next.meta.generation += 1;
                self.balancers
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(load_balancer = %lb.meta.name, %vip, "gave a load balancer its address");
                Ok(true)
            }
            Err(why) => {
                // The same words a port and a floating IP get for the same
                // three situations, so an operator reads one vocabulary.
                let want = unaddressable_condition(&why, lb.meta.generation);
                let mut next = lb.clone();
                set_condition(&mut next.status.conditions, want);
                next.status.observed_generation = lb.meta.generation;
                if next.status != lb.status {
                    self.say.write(lb, &next).await?;
                }
                Ok(true)
            }
        }
    }

    /// The pool, resolved to the fabric's own port ids — or why it cannot be
    /// yet. One fabric id per member, order preserved; a member that resolves
    /// to nothing stops the whole pool, loudly, for the reason in the module
    /// doc.
    async fn resolve_members(
        &self,
        fabric_ports: &[pb::PortInfo],
        members: &[String],
    ) -> Result<std::result::Result<Vec<String>, String>> {
        if members.is_empty() {
            return Ok(Ok(Vec::new()));
        }
        let mut ids = Vec::new();
        let mut waiting = Vec::new();
        for name in members {
            let Some(port) = self.ports.get(name).await? else {
                waiting.push(format!("{name} does not exist"));
                continue;
            };
            // Matched on `(tap, host)`, like every other lookup into the
            // fabric's ports: a tap name is deliberately identical on every
            // node, so the name alone would find a different guest's port.
            let (Some(tap), Some(node)) = (
                port.status.tap_device.as_deref(),
                port.status.node.as_deref(),
            ) else {
                waiting.push(format!("{name} is not on a node yet"));
                continue;
            };
            let host = node.strip_prefix("nodes/").unwrap_or(node);
            match fabric_ports.iter().find(|p| p.tap == tap && p.host == host) {
                Some(p) => ids.push(p.id.clone()),
                None => waiting.push(format!(
                    "{name} is placed on {host} but the fabric has no port for it yet"
                )),
            }
        }
        if !waiting.is_empty() {
            return Ok(Err(format!(
                "waiting for the whole pool: {}. A pool programmed without some of its members \
                 would look balanced and silently leave them out",
                waiting.join("; ")
            )));
        }
        Ok(Ok(ids))
    }

    /// The services the fabric holds, read back as the model speaks them.
    ///
    /// Only this load balancer's own, and split honestly: a held service whose
    /// protocol this model cannot name (nothing here ever writes one) comes
    /// back in the second list, to be removed rather than compared wrongly.
    async fn held(
        &self,
        client: &mut velstra_cloud_fabric::Connected,
        name: &str,
    ) -> std::result::Result<(Vec<FabricService>, Vec<String>), Box<velstra_cloud_fabric::Status>>
    {
        let mut held = Vec::new();
        let mut alien = Vec::new();
        for s in client
            .list_load_balancers(pb::ListLoadBalancersRequest {})
            .await
            .map_err(Box::new)?
            .into_inner()
            .load_balancers
        {
            if !owns_service(name, &s.id) {
                continue;
            }
            let protocol = match pb::Proto::try_from(s.proto) {
                Ok(pb::Proto::Tcp) => Protocol::Tcp,
                Ok(pb::Proto::Udp) => Protocol::Udp,
                _ => {
                    alien.push(s.id);
                    continue;
                }
            };
            held.push(FabricService {
                id: s.id,
                vni: s.vni,
                vip: s.vip,
                protocol,
                port: s.port as u16,
                members: s
                    .members
                    .into_iter()
                    .map(|m| FabricMember {
                        port_id: m.port_id,
                        port: m.port as u16,
                    })
                    .collect(),
            });
        }
        Ok((held, alien))
    }

    /// Hand every service back to the fabric — the teardown half.
    async fn release(&self, name: &str) -> std::result::Result<(), String> {
        let Some(endpoint) = self.fabric.clone() else {
            // A cell with no fabric never programmed anything either. Refusing
            // to let the object go would pin every load balancer in such a
            // cell forever.
            return Ok(());
        };
        let mut client = velstra_cloud_fabric::connect(&endpoint)
            .await
            .map_err(|e| e.to_string())?;
        let (held, alien) = self
            .held(&mut client, name)
            .await
            .map_err(|e| e.to_string())?;
        for id in held.into_iter().map(|s| s.id).chain(alien) {
            client
                .remove_load_balancer(pb::RemoveLoadBalancerRequest { id })
                .await
                .map_err(|e| e.message().to_string())?;
        }
        Ok(())
    }

    /// One fabric service, in the fabric's spelling.
    fn to_fabric(service: &FabricService) -> pb::LoadBalancerSpec {
        pb::LoadBalancerSpec {
            id: service.id.clone(),
            vni: service.vni,
            vip: service.vip.clone(),
            port: service.port as u32,
            proto: match service.protocol {
                Protocol::Tcp => pb::Proto::Tcp,
                Protocol::Udp => pb::Proto::Udp,
            } as i32,
            members: service
                .members
                .iter()
                .map(|m| pb::LbMember {
                    port_id: m.port_id.clone(),
                    port: m.port as u32,
                })
                .collect(),
        }
    }
}

impl Reconciler for LoadBalancerController {
    type Spec = LoadBalancerSpec;
    type Status = LoadBalancerStatus;

    fn name(&self) -> &'static str {
        "load-balancer"
    }

    async fn reconcile(&self, name: &str, object: Option<&LoadBalancer>) -> Result<()> {
        let Some(lb) = object else {
            // Gone, and gone properly: the finalizer below means the fabric
            // had already let every service go before the record could
            // disappear.
            return Ok(());
        };

        // The guard goes on before anything is programmed, and comes off only
        // after the fabric has let go. See the module doc for why a derived id
        // is not enough on its own.
        match finalizer_step(&lb.meta, FABRIC_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                let mut next = lb.clone();
                next.meta.add_finalizer(FABRIC_RELEASE_FINALIZER);
                self.balancers
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                return Ok(());
            }
            FinalizerStep::Delete => return Ok(()),
            FinalizerStep::Wait if lb.meta.is_deleting() => {
                if !lb.meta.has_finalizer(FABRIC_RELEASE_FINALIZER) {
                    // Somebody else's guard; this one is already released.
                    return Ok(());
                }
                if let Err(why) = self.release(name).await {
                    warn!(load_balancer = %name, error = %why, "cannot release the services yet");
                    return Ok(());
                }
                let mut next = lb.clone();
                next.meta.remove_finalizer(FABRIC_RELEASE_FINALIZER);
                self.balancers
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(load_balancer = %name, "the fabric let the services go; the guard is off");
                return Ok(());
            }
            FinalizerStep::Wait => {}
        }

        // No `..`: every field of a load balancer is acted on below, and a new
        // one is a compile error here until somebody says how.
        let LoadBalancerSpec {
            network: _,   // resolved to a VNI below
            subnet: _,    // where the VIP comes from — used by `address`
            vip: _,       // the address itself — decided by `address`
            listeners: _, // one fabric service each, below
            members: _,   // resolved to fabric port ids below
        } = &lb.spec;

        // The address first, and always: a cell with no fabric still decides
        // it, so an operator sees what they will get before any data plane
        // exists.
        if self.address(lb).await? {
            return Ok(());
        }
        let Some(vip) = lb.spec.vip.clone() else {
            return Ok(());
        };

        // Refused at the API too; checked again here for an object written by
        // an older version of this software. Belt to that brace.
        if let Err(why) = validate_listeners(&lb.spec.listeners) {
            return self
                .settle(
                    lb,
                    ConditionStatus::False,
                    "Invalid",
                    &why.to_string(),
                    None,
                )
                .await;
        }
        if lb.spec.listeners.is_empty() {
            return self
                .settle(
                    lb,
                    ConditionStatus::False,
                    "Incomplete",
                    "no listeners, so there is no port to answer on; add at least one",
                    None,
                )
                .await;
        }

        let Some(endpoint) = self.fabric.clone() else {
            // No fabric, no data plane — and nothing claims one is being
            // programmed.
            return Ok(());
        };

        let Some(network) = self.networks.get(&lb.spec.network).await? else {
            return self
                .settle(
                    lb,
                    ConditionStatus::False,
                    "Incomplete",
                    &format!("waiting for {}, which does not exist yet", lb.spec.network),
                    None,
                )
                .await;
        };

        let mut client = match velstra_cloud_fabric::connect(&endpoint).await {
            Ok(client) => client,
            Err(e) => {
                // Unreachable is a wait, not a verdict.
                warn!(load_balancer = %name, error = %e, "cannot reach the fabric");
                return Ok(());
            }
        };

        let fabric_ports = match client.list_ports(pb::ListPortsRequest {}).await {
            Ok(answer) => answer.into_inner().ports,
            Err(status) => {
                warn!(load_balancer = %name, error = %status, "asking the fabric for the ports");
                return Ok(());
            }
        };
        let member_ids = match self
            .resolve_members(&fabric_ports, &lb.spec.members)
            .await?
        {
            Ok(ids) => ids,
            Err(why) => {
                return self
                    .settle(lb, ConditionStatus::False, "MembersNotReady", &why, None)
                    .await;
            }
        };

        let desired = desired_services(name, &lb.spec, network.spec.vni, &vip, &member_ids);
        let (held, alien) = match self.held(&mut client, name).await {
            Ok(answer) => answer,
            Err(status) => {
                warn!(load_balancer = %name, error = %status, "asking the fabric for the services");
                return Ok(());
            }
        };
        // A held service whose protocol this model cannot read is one nothing
        // here wrote; it is retired rather than compared wrongly.
        for id in alien {
            if let Err(status) = client
                .remove_load_balancer(pb::RemoveLoadBalancerRequest { id: id.clone() })
                .await
            {
                warn!(load_balancer = %name, service = %id, error = %status,
                      "the fabric kept a service this model cannot read");
            }
        }

        for action in mirror_actions(name, &desired, &held) {
            match action {
                MirrorAction::Remove { id } => {
                    if let Err(status) = client
                        .remove_load_balancer(pb::RemoveLoadBalancerRequest { id: id.clone() })
                        .await
                    {
                        warn!(load_balancer = %name, service = %id, error = %status,
                              "the fabric kept the service");
                        return self
                            .settle(
                                lb,
                                ConditionStatus::False,
                                "Refused",
                                status.message(),
                                None,
                            )
                            .await;
                    }
                }
                MirrorAction::Add(service) => {
                    if let Err(status) = client.add_load_balancer(Self::to_fabric(&service)).await {
                        warn!(load_balancer = %name, service = %service.id, error = %status,
                              "the fabric refused the service");
                        return self
                            .settle(
                                lb,
                                ConditionStatus::False,
                                "Refused",
                                status.message(),
                                None,
                            )
                            .await;
                    }
                }
            }
        }

        let message = if member_ids.is_empty() {
            "the pool is empty; the address answers and forwards to nothing"
        } else {
            ""
        };
        self.settle(
            lb,
            ConditionStatus::True,
            "Programmed",
            message,
            Some((vip, observed_listeners(&desired))),
        )
        .await
    }
}

impl LoadBalancerController {
    /// Record what happened, and write only when it changed — a settled object
    /// must cost nothing.
    async fn settle(
        &self,
        lb: &LoadBalancer,
        status: ConditionStatus,
        reason: &str,
        message: &str,
        observed: Option<(String, Vec<ObservedListener>)>,
    ) -> Result<()> {
        let generation = lb.meta.generation;
        let mut next = lb.clone();
        if let Some((vip, listeners)) = observed {
            next.status.vip = vip;
            next.status.listeners = listeners;
        }
        let want = Condition::new(READY, status, reason, message, generation);
        let settled = condition(&lb.status.conditions, READY).is_some_and(|c| {
            c.status == want.status
                && c.reason == want.reason
                && c.message == want.message
                && c.observed_generation == generation
        });
        // The observed halves count as part of "settled": a pool that changed
        // while the condition stayed `Programmed` would otherwise never be
        // written down.
        if settled
            && next.status.vip == lb.status.vip
            && next.status.listeners == lb.status.listeners
        {
            return Ok(());
        }
        set_condition(&mut next.status.conditions, want);
        next.status.observed_generation = generation;
        self.say.write(lb, &next).await?;
        if status == ConditionStatus::True {
            info!(load_balancer = %lb.meta.name, vip = %next.status.vip, "programmed by the fabric");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        loadbalancer::Listener,
        meta::{Meta, Placement, ResourceName},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const CELL: &str = "cell-1";
    const SUBNET: &str = "projects/p1/subnets/s1";
    const LB: &str = "projects/p1/load-balancers/web";

    struct Fixture {
        raw: Arc<dyn Store>,
        balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus>,
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
                balancers: TypedStore::new(raw.clone(), CELL, "load-balancers"),
                subnets: TypedStore::new(raw.clone(), CELL, "subnets"),
                ports: TypedStore::new(raw.clone(), CELL, "ports"),
                raw,
            }
        }

        fn controller(&self) -> LoadBalancerController {
            LoadBalancerController::new(
                self.raw.clone(),
                CELL,
                self.balancers.clone(),
                TypedStore::new(self.raw.clone(), CELL, "networks"),
                self.subnets.clone(),
                self.ports.clone(),
                TypedStore::new(self.raw.clone(), CELL, "floatingips"),
                None,
            )
        }

        async fn subnet(&self, cidr: &str) {
            self.subnets
                .create(
                    &Resource::new(
                        meta(SUBNET),
                        SubnetSpec {
                            network: "projects/p1/networks/n1".into(),
                            cidr: cidr.into(),
                            gateway: "10.20.0.1".into(),
                            dns: vec![],
                            reserved: vec![],
                        },
                        SubnetStatus::default(),
                    ),
                    &Writer::controller(WHO),
                )
                .await
                .unwrap();
        }

        async fn port(&self, id: &str, address: &str) {
            self.ports
                .create(
                    &Resource::new(
                        meta(&format!("projects/p1/ports/{id}")),
                        PortSpec {
                            network: "projects/p1/networks/n1".into(),
                            subnet: SUBNET.into(),
                            address: Some(address.into()),
                            ..Default::default()
                        },
                        PortStatus::default(),
                    ),
                    &Writer::controller(WHO),
                )
                .await
                .unwrap();
        }

        async fn balancer(&self, vip: Option<&str>) -> LoadBalancer {
            let lb = Resource::new(
                meta(LB),
                LoadBalancerSpec {
                    network: "projects/p1/networks/n1".into(),
                    subnet: SUBNET.into(),
                    vip: vip.map(str::to_string),
                    listeners: vec![Listener {
                        protocol: Protocol::Tcp,
                        port: 443,
                        member_port: 0,
                    }],
                    members: vec![],
                },
                LoadBalancerStatus::default(),
            );
            self.balancers
                .create(&lb, &Writer::controller(WHO))
                .await
                .unwrap();
            self.balancers.get(LB).await.unwrap().unwrap()
        }

        async fn current(&self) -> LoadBalancer {
            self.balancers.get(LB).await.unwrap().unwrap()
        }
    }

    /// The ordinary case: a load balancer with no VIP is given the lowest
    /// address nothing else holds, where a person can see and pin it.
    #[tokio::test]
    async fn a_load_balancer_is_given_its_vip() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        let lb = f.balancer(None).await;

        assert!(f.controller().address(&lb).await.unwrap());
        assert_eq!(f.current().await.spec.vip.as_deref(), Some("10.20.0.2"));
    }

    /// And never an address a port already holds — the one-allocator property,
    /// tested through the controller because the controller is what reads the
    /// ports.
    #[tokio::test]
    async fn the_vip_is_not_an_address_a_port_holds() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        f.port("web", "10.20.0.2").await;
        let lb = f.balancer(None).await;

        f.controller().address(&lb).await.unwrap();
        assert_eq!(f.current().await.spec.vip.as_deref(), Some("10.20.0.3"));
    }

    /// A second pass over an addressed load balancer writes nothing — a
    /// settled object must cost nothing.
    #[tokio::test]
    async fn an_addressed_load_balancer_is_left_alone() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        let lb = f.balancer(Some("10.20.0.99")).await;

        assert!(!f.controller().address(&lb).await.unwrap());
        let after = f.current().await;
        assert_eq!(after.spec.vip.as_deref(), Some("10.20.0.99"));
        assert_eq!(
            after.meta.revision, lb.meta.revision,
            "a settled object was written"
        );
    }

    /// A subnet that does not exist yet is a wait, said on the object in the
    /// same words a port and a floating IP get.
    #[tokio::test]
    async fn a_missing_subnet_is_said_in_the_shared_vocabulary() {
        let f = Fixture::new();
        let lb = f.balancer(None).await;

        f.controller().address(&lb).await.unwrap();

        let after = f.current().await;
        let said = condition(&after.status.conditions, READY)
            .expect("nothing was said about a load balancer that could not be addressed");
        assert_eq!(said.reason, "NoSuchSubnet");
    }

    /// With no fabric configured the address is still decided, and nothing
    /// claims a data plane is being programmed.
    #[tokio::test]
    async fn with_no_fabric_the_vip_is_decided_and_nothing_claims_more() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        f.balancer(None).await;
        let c = f.controller();

        // Three passes: the guard, the address, and the discovery that there
        // is nothing more to do without a fabric.
        for _ in 0..3 {
            let now = f.current().await;
            c.reconcile(LB, Some(&now)).await.unwrap();
        }

        let after = f.current().await;
        assert!(
            after.meta.has_finalizer(FABRIC_RELEASE_FINALIZER),
            "nothing guards the fabric's half of a deletion"
        );
        assert_eq!(after.spec.vip.as_deref(), Some("10.20.0.2"));
        assert!(
            condition(&after.status.conditions, READY).is_none_or(|c| c.reason != "Programmed"),
            "a load balancer claimed to be programmed with no fabric to program"
        );
        assert!(after.status.vip.is_empty());
    }

    /// A load balancer with no listeners waits and says what for, rather than
    /// programming an answerless VIP or failing the create.
    #[tokio::test]
    async fn no_listeners_is_incomplete_not_an_error() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        let lb = f.balancer(Some("10.20.0.50")).await;
        let mut empty = lb.clone();
        empty.spec.listeners = vec![];
        empty.meta.generation += 1;
        f.balancers
            .update(&empty, &Writer::controller(WHO))
            .await
            .unwrap();
        let c = f.controller();

        for _ in 0..2 {
            let now = f.current().await;
            c.reconcile(LB, Some(&now)).await.unwrap();
        }

        let after = f.current().await;
        let said = condition(&after.status.conditions, READY)
            .expect("an empty load balancer said nothing");
        assert_eq!(said.reason, "Incomplete");
        assert!(said.message.contains("listener"), "{}", said.message);
    }

    /// Deleting with no fabric releases the guard rather than pinning the
    /// object forever.
    #[tokio::test]
    async fn a_deletion_in_a_fabricless_cell_lets_go() {
        let f = Fixture::new();
        f.subnet("10.20.0.0/24").await;
        f.balancer(Some("10.20.0.50")).await;
        let c = f.controller();

        // Take the guard first, as an ordinary pass would.
        let now = f.current().await;
        c.reconcile(LB, Some(&now)).await.unwrap();

        let mut deleting = f.current().await;
        deleting.meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp::now());
        f.balancers
            .update(&deleting, &Writer::controller(WHO))
            .await
            .unwrap();

        let now = f.current().await;
        c.reconcile(LB, Some(&now)).await.unwrap();
        assert!(
            !f.current()
                .await
                .meta
                .has_finalizer(FABRIC_RELEASE_FINALIZER),
            "a cell with no fabric held a guard nothing will ever release"
        );
    }
}
