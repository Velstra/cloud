//! Telling the fabric that a tenant's networks route to each other.
//!
//! A cloud router is a *statement of adjacency*: these networks reach one
//! another, everything else does not. The fabric calls the same thing an
//! IP-VRF — a routed VNI, an anycast gateway MAC, and the set of L2 VNIs it
//! routes between — and that is what this controller writes.
//!
//! Like [`crate::network`], this is a cell-wide fact no node can state, so a
//! controller states it and leader election means exactly one process does.
//!
//! **Why the numbers are derived and not allocated.** A router needs an L3 VNI
//! and a gateway MAC, and nothing in this control plane allocates numbers — the
//! one place that could (a VNI allocator for `NetworkSpec.vni`) does not exist
//! either. An allocator is state, and state that lives only in a controller is
//! state that a failover loses: a restarted controller that re-allocates hands
//! a running tenant a *different* routed VNI, and every guest's default gateway
//! goes away. So both numbers are a pure function of the router's name, exactly
//! as fabric derives a security group's `policy_id` from its name and for the
//! same reason — the same name maps to the same numbers on any host, across
//! restarts, in either of two controllers racing a handover. Nothing to lose,
//! nothing to reconcile, and a collision is refused by the fabric by name
//! rather than resolved arbitrarily.

use std::sync::Arc;

use tracing::{info, warn};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    meta::{Condition, ConditionStatus, condition, set_condition},
    resources::{NetworkSpec, NetworkStatus, Router, RouterSpec, RouterStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler, status::StatusWriter};

const WHO: &str = "router";

/// The condition this controller owns: whether the fabric routes these networks.
const ROUTED: &str = "Routed";

/// Derive a router's routed VNI from its name.
///
/// A 32-bit FNV-1a hash — the same function fabric uses for a security group's
/// `policy_id`, so the two derivations behave alike — folded into `1..=0xFF_FFFF`,
/// which is the whole of fabric's L3 VNI space. The L3 space is its own: an L3
/// VNI numerically equal to some L2 VNI is a different context, not the same
/// one, so there is nothing here to keep clear of.
pub fn l3_vni_for(name: &str) -> u32 {
    // `% 0xFF_FFFF` lands in `0..=0xFF_FFFE`; `+ 1` shifts it to `1..=0xFF_FFFF`,
    // which excludes the zero fabric refuses without excluding anything else.
    (fnv1a(name) % 0xFF_FFFF) + 1
}

/// Derive a router's anycast gateway MAC from its name.
///
/// The first octet is fixed at `0x02`: bit 0 clear makes it unicast (fabric
/// refuses a multicast one, and a multicast address can never be an inner
/// source), and bit 1 set makes it locally administered, which is what an
/// address nobody bought from the IEEE must claim. The remaining five octets
/// come from the hash, so every host serving the tenant derives the identical
/// address and a guest keeps its default-gateway ARP entry when it migrates.
pub fn gateway_mac_for(name: &str) -> String {
    let h = fnv1a(name);
    // Five octets from four hash bytes plus one more mixing pass, so the two
    // halves of the address do not repeat for names that share a prefix.
    let g = fnv1a(&format!("{name}/gw"));
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        (h >> 24) as u8,
        (h >> 16) as u8,
        (h >> 8) as u8,
        h as u8,
        g as u8
    )
}

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub struct RouterController {
    /// A router has no agent — no single machine owns a cell-wide routing fact
    /// — so its condition goes through the narrow path that lets a controller
    /// write `status` on an object nobody holds. See [`crate::status`].
    say: StatusWriter<RouterSpec, RouterStatus>,
    networks: TypedStore<NetworkSpec, NetworkStatus>,
    /// Where the fabric's orchestrator answers. `None` disables mirroring, which
    /// is what a cell with no data plane wants: the control plane works and
    /// nothing claims a router is in force.
    fabric: Option<Arc<str>>,
}

impl RouterController {
    pub fn new(
        store: Arc<dyn velstra_cloud_store::Store>,
        cell: &str,
        networks: TypedStore<NetworkSpec, NetworkStatus>,
        fabric: Option<String>,
    ) -> Self {
        Self {
            say: StatusWriter::new(store, cell, "routers", WHO),
            networks,
            fabric: fabric.map(Arc::from),
        }
    }

    /// The VNIs of the networks this router joins, or why they cannot be had.
    ///
    /// A name that resolves to nothing is **refused rather than skipped**. A
    /// router silently routing three of its four networks is the worst outcome
    /// available: it looks configured, the fourth tenant's traffic is dropped by
    /// a data plane that was never told about it, and nothing anywhere says so.
    /// Waiting until the name exists costs a reconcile; guessing costs a
    /// debugging session.
    async fn vnis_for(&self, router: &Router) -> Result<std::result::Result<Vec<u32>, String>> {
        // No `..`: everything a router says has to reach the fabric or be
        // refused here. One field today, and the next one is a compile error
        // until somebody decides which.
        let RouterSpec { networks } = &router.spec;
        if networks.is_empty() {
            return Ok(Err(
                "no networks, so there is nothing to route between; add at least two".into(),
            ));
        }
        let mut vnis = Vec::new();
        let mut missing = Vec::new();
        for name in networks {
            match self.networks.get(name).await? {
                Some(network) => vnis.push(network.spec.vni),
                None => missing.push(name.clone()),
            }
        }
        if !missing.is_empty() {
            return Ok(Err(format!(
                "waiting for {}, which {} not exist yet",
                missing.join(", "),
                if missing.len() == 1 { "does" } else { "do" }
            )));
        }
        // A set written as a list: the same network named twice is one network
        // routed once, and fabric would otherwise see a duplicate VNI.
        vnis.sort_unstable();
        vnis.dedup();
        Ok(Ok(vnis))
    }
}

impl Reconciler for RouterController {
    type Spec = RouterSpec;
    type Status = RouterStatus;

    fn name(&self) -> &'static str {
        "router"
    }

    async fn reconcile(&self, name: &str, object: Option<&Router>) -> Result<()> {
        let Some(router) = object else {
            // Gone — and this is the one teardown that needs nothing from the
            // record that just disappeared, because the number to retire is a
            // function of the name. No finalizer, no id to have remembered:
            // deriving paid for itself here.
            //
            // Removing one the fabric does not have is not an error there, so a
            // second pass over a router that is already gone is quiet.
            let Some(endpoint) = self.fabric.clone() else {
                return Ok(());
            };
            let l3_vni = l3_vni_for(name);
            match velstra_cloud_fabric::connect(&endpoint).await {
                Ok(mut client) => {
                    if let Err(status) = client
                        .remove_ip_vrf(pb::RemoveIpVrfRequest { l3_vni })
                        .await
                    {
                        warn!(router = %name, error = %status, "the fabric kept the routed context");
                    } else {
                        info!(router = %name, l3_vni, "retired the routed context");
                    }
                }
                Err(e) => warn!(router = %name, error = %e, "cannot reach the fabric"),
            }
            return Ok(());
        };
        let Some(endpoint) = self.fabric.clone() else {
            return Ok(());
        };

        let vnis = match self.vnis_for(router).await? {
            Ok(vnis) => vnis,
            Err(why) => {
                return self
                    .say(router, ConditionStatus::False, "Incomplete", &why, None)
                    .await;
            }
        };

        let l3_vni = l3_vni_for(name);
        let gateway_mac = gateway_mac_for(name);
        // Restated on every pass. Fabric's `add_ip_vrf` is a restatement keyed
        // on the VRF's name, so this converges after a fabric restart, a
        // controller failover, or a network joining late — and a *different*
        // router reaching for the same numbers is still refused, loudly, which
        // is what a derived number space needs.
        let spec = pb::IpVrfSpec {
            l3_vni,
            name: name.to_string(),
            gateway_mac: gateway_mac.clone(),
            networks: vnis,
        };
        let assigned = Some((l3_vni, gateway_mac));
        match velstra_cloud_fabric::connect(&endpoint).await {
            Ok(mut client) => match client.add_ip_vrf(spec).await {
                Ok(_) => {
                    self.say(router, ConditionStatus::True, "Routed", "", assigned)
                        .await
                }
                Err(status) => {
                    warn!(router = %name, error = %status, "the fabric refused the router");
                    // The numbers are still recorded on a refusal: they are what
                    // the operator needs to read the fabric's message, which
                    // names the VNI and not the router.
                    self.say(
                        router,
                        ConditionStatus::False,
                        "Refused",
                        status.message(),
                        assigned,
                    )
                    .await
                }
            },
            Err(e) => {
                // Unreachable is a wait, not a verdict.
                warn!(router = %name, error = %e, "cannot reach the fabric");
                Ok(())
            }
        }
    }
}

impl RouterController {
    /// Record what happened on the router itself, and write only when it
    /// changed — a settled object must cost nothing.
    async fn say(
        &self,
        router: &Router,
        status: ConditionStatus,
        reason: &str,
        message: &str,
        assigned: Option<(u32, String)>,
    ) -> Result<()> {
        let generation = router.meta.generation;
        let mut next = router.clone();
        if let Some((l3_vni, gateway_mac)) = assigned {
            next.status.l3_vni = l3_vni;
            next.status.gateway_mac = gateway_mac;
        }
        let want = Condition::new(ROUTED, status, reason, message, generation);
        let settled = condition(&router.status.conditions, ROUTED).is_some_and(|c| {
            c.status == want.status
                && c.reason == want.reason
                && c.message == want.message
                && c.observed_generation == generation
        });
        // The numbers are part of what must match: a router whose condition is
        // already `Routed` but whose recorded VNI is stale would never be
        // corrected if the condition alone decided.
        if settled
            && next.status.l3_vni == router.status.l3_vni
            && next.status.gateway_mac == router.status.gateway_mac
        {
            return Ok(());
        }
        set_condition(&mut next.status.conditions, want);
        next.status.observed_generation = generation;
        self.say.write(router, &next).await?;
        if status == ConditionStatus::True {
            info!(
                router = %router.meta.name,
                l3_vni = next.status.l3_vni,
                "routed by the fabric"
            );
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
        TypedStore<RouterSpec, RouterStatus>,
        TypedStore<NetworkSpec, NetworkStatus>,
    ) {
        let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
        (
            raw.clone(),
            TypedStore::new(raw.clone(), CELL, "routers"),
            TypedStore::new(raw, CELL, "networks"),
        )
    }

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", CELL),
        )
    }

    async fn add_network(nets: &TypedStore<NetworkSpec, NetworkStatus>, id: &str, vni: u32) {
        nets.create(
            &Resource::new(
                meta(&format!("projects/p1/networks/{id}")),
                NetworkSpec {
                    vni,
                    mtu: 1500,
                    external: false,
                    announce: Default::default(),
                },
                NetworkStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("router"),
        )
        .await
        .unwrap();
    }

    fn router(networks: &[&str]) -> Router {
        Resource::new(
            meta("projects/p1/routers/r1"),
            RouterSpec {
                networks: networks.iter().map(|n| n.to_string()).collect(),
            },
            RouterStatus::default(),
        )
    }

    /// The derived numbers are a function of the name and nothing else — the
    /// property the whole design rests on, because a failover re-derives rather
    /// than remembers.
    #[test]
    fn the_numbers_are_a_function_of_the_name() {
        let a = "projects/p1/routers/r1";
        let b = "projects/p1/routers/r2";
        assert_eq!(l3_vni_for(a), l3_vni_for(a));
        assert_eq!(gateway_mac_for(a), gateway_mac_for(a));
        assert_ne!(l3_vni_for(a), l3_vni_for(b));
        assert_ne!(gateway_mac_for(a), gateway_mac_for(b));
    }

    /// Every derived VNI is one the fabric will accept: non-zero and 24-bit.
    ///
    /// The fold is the easy thing to get wrong by one — `% 0xFF_FFFF` alone can
    /// produce the zero fabric refuses, and `% 0x100_0000 + 1` can produce the
    /// `0x100_0000` it also refuses. Walk enough names to hit both ends.
    #[test]
    fn every_derived_l3_vni_is_one_the_fabric_accepts() {
        for i in 0..20_000 {
            let vni = l3_vni_for(&format!("projects/p{i}/routers/r{i}"));
            assert!(
                vni != 0 && vni <= 0xFF_FFFF,
                "name {i} derived l3_vni {vni}, which fabric refuses"
            );
        }
    }

    /// Every derived gateway MAC is one the fabric will accept: non-zero, and
    /// unicast — fabric refuses a group-bit address because it can never be an
    /// inner source.
    #[test]
    fn every_derived_gateway_mac_is_unicast_and_locally_administered() {
        for i in 0..20_000 {
            let mac = gateway_mac_for(&format!("projects/p{i}/routers/r{i}"));
            let octets: Vec<u8> = mac
                .split(':')
                .map(|o| u8::from_str_radix(o, 16).expect("not hex"))
                .collect();
            assert_eq!(octets.len(), 6, "{mac}");
            assert_eq!(octets[0] & 1, 0, "{mac} is multicast");
            assert_eq!(octets[0] & 2, 2, "{mac} is not locally administered");
            assert!(octets.iter().any(|&o| o != 0), "{mac} is all zero");
        }
    }

    /// The ordinary case: the router's networks resolve to their VNIs.
    #[tokio::test]
    async fn a_routers_networks_resolve_to_their_vnis() {
        let (raw, _routers, nets) = stores();
        add_network(&nets, "n1", 5001).await;
        add_network(&nets, "n2", 5002).await;
        let c = RouterController::new(raw, CELL, nets, None);

        let vnis = c
            .vnis_for(&router(&[
                "projects/p1/networks/n1",
                "projects/p1/networks/n2",
            ]))
            .await
            .unwrap();
        assert_eq!(vnis, Ok(vec![5001, 5002]));
    }

    /// A network named twice is one network routed once. Fabric would see a
    /// duplicate VNI in the set otherwise.
    #[tokio::test]
    async fn the_same_network_twice_is_routed_once() {
        let (raw, _routers, nets) = stores();
        add_network(&nets, "n1", 5001).await;
        let c = RouterController::new(raw, CELL, nets, None);

        let vnis = c
            .vnis_for(&router(&[
                "projects/p1/networks/n1",
                "projects/p1/networks/n1",
            ]))
            .await
            .unwrap();
        assert_eq!(vnis, Ok(vec![5001]));
    }

    /// A name that resolves to nothing stops the whole router, and says which
    /// name.
    ///
    /// **This is the point of the test.** Skipping the missing one would leave a
    /// router that looks configured and silently drops one tenant's traffic —
    /// the exact shape of defect this codebase keeps finding. Refusing costs a
    /// reconcile; guessing costs a debugging session.
    #[tokio::test]
    async fn a_network_that_does_not_exist_stops_the_router_and_is_named() {
        let (raw, _routers, nets) = stores();
        add_network(&nets, "n1", 5001).await;
        let c = RouterController::new(raw, CELL, nets, None);

        let why = c
            .vnis_for(&router(&[
                "projects/p1/networks/n1",
                "projects/p1/networks/gone",
            ]))
            .await
            .unwrap()
            .expect_err("a router routed around a network that does not exist");
        assert!(why.contains("projects/p1/networks/gone"), "{why}");
        assert!(
            !why.contains("networks/n1"),
            "the wait names a network that is present: {why}"
        );
    }

    /// A router with no networks is waiting, not broken — and the reason says so
    /// rather than leaving an empty IP-VRF on the fabric.
    #[tokio::test]
    async fn a_router_with_no_networks_says_what_it_is_waiting_for() {
        let (raw, _routers, nets) = stores();
        let c = RouterController::new(raw, CELL, nets, None);

        let why = c
            .vnis_for(&router(&[]))
            .await
            .unwrap()
            .expect_err("an empty router was mirrored anyway");
        assert!(why.contains("nothing to route"), "{why}");
    }

    /// With no fabric configured, a reconcile is a no-op — and nothing claims to
    /// be routed.
    #[tokio::test]
    async fn with_no_fabric_nothing_is_routed_and_nothing_claims_to_be() {
        let (raw, routers, nets) = stores();
        add_network(&nets, "n1", 5001).await;
        let r = router(&["projects/p1/networks/n1"]);
        routers
            .create(
                &r,
                &velstra_cloud_model::access::Writer::controller("router"),
            )
            .await
            .unwrap();
        let c = RouterController::new(raw, CELL, nets, None);

        c.reconcile("projects/p1/routers/r1", Some(&r))
            .await
            .expect("a cell with no fabric must still reconcile");

        let after = routers
            .get("projects/p1/routers/r1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            condition(&after.status.conditions, ROUTED).is_none(),
            "a router claimed a routing state with no fabric to route on"
        );
        assert_eq!(
            after.status.l3_vni, 0,
            "a router recorded a routed VNI nothing was told about"
        );
    }
}
