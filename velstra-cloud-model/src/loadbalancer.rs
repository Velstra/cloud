//! A load balancer: one address in front of many ports.
//!
//! The fabric already implements this in the datapath — a VIP whose flows are
//! DNAT-rewritten in XDP at the ingress host, with connection tracking and
//! reverse NAT — and exposes it as a load-balanced service keyed on an id.
//! What this module adds is the resource an operator declares and the pure
//! decisions that keep the fabric matching it. Nothing here talks to the
//! fabric; the controller reads both sides and performs what
//! [`mirror_actions`] returns.
//!
//! **A load balancer is a declaration, not a box.** Like a router, there is no
//! appliance to place and no machine that owns it: balancing happens on
//! whichever host ingress traffic arrives at, so no node can report on it and
//! `status` is written by the controller through the narrow
//! no-agent-owns-it path (see [`crate::reconcile::controller_may_write_status`]).
//!
//! **What is deliberately not modelled, and why.** The fabric's balancer
//! spreads flows uniformly by connection hash and reports nothing about the
//! health of a member. So there is:
//!
//! * no `weight` on a member — a stored weight nothing honours would be shown
//!   by the console as if it biased traffic, which is the `Signed` column
//!   defect over again: a claim the platform displays and cannot keep;
//! * no `algorithm` — the datapath has exactly one behaviour (flow-hash with
//!   connection stickiness), and an enum of choices it ignores is a control
//!   that lies;
//! * no health check and no per-member health in `status` — nothing in this
//!   platform probes a backend, and the fabric's listing carries no health, so
//!   any value here would be an assertion made by something that cannot see.
//!   A member that is gone *as an object* is refused loudly at reference-check
//!   and reconcile time instead.
//!
//! When the fabric grows any of those capabilities, the field arrives in the
//! same commit as the code that reads it — never before.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    meta::Condition,
    resources::{Assigned, Observed, Resource},
};

/// What the datapath can balance. TCP or UDP and nothing else — the fabric's
/// contract says so ("the datapath balances no others"), and offering a
/// protocol it would refuse is a form field that produces an error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
        }
    }
}

/// One port the VIP answers on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    pub protocol: Protocol,
    /// The port on the VIP. Never zero — zero is what an unset number looks
    /// like coming off a wire, and a listener on it is refused at the door.
    pub port: u16,
    /// The port the members answer on. Zero means the client's own destination
    /// port — the same spelling the fabric uses, so a listener forwarding
    /// 443 → 443 does not have to say 443 twice.
    #[serde(default)]
    pub member_port: u16,
}

/// What was asked for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadBalancerSpec {
    /// The network the VIP lives on, by resource name. It scopes the service:
    /// two tenants may front the same address on different networks.
    pub network: String,
    /// The subnet the VIP comes from, by resource name. The address is counted
    /// out of the same range the ports and floating IPs use, by the same
    /// allocator — see [`crate::ipam::assign_vip`].
    pub subnet: String,
    /// The address, once something has decided it.
    ///
    /// `None` means "any", and a controller fills it in — the same arrangement
    /// as a port's address and a floating IP's, and written into `spec` for the
    /// same reason: it is the thing a person may pin, so it must be a field
    /// they can write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vip: Option<String>,
    /// The ports the VIP answers on. Empty is a load balancer that is not yet
    /// decided rather than an error — it says `Incomplete` on itself and
    /// programs nothing.
    #[serde(default)]
    pub listeners: Vec<Listener>,
    /// The backend pool, as port resource names — a port, not an address, so a
    /// migrated guest stays in the pool and a member can never point at an
    /// address nothing serves. A set written as a list, like a router's
    /// networks: duplicates are one member, not an error. Empty is a drained
    /// pool, which is a legitimate state to hold a VIP in.
    #[serde(default)]
    pub members: Vec<String>,
}

/// One listener as the fabric holds it: the fact, not the ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedListener {
    pub protocol: Protocol,
    pub port: u16,
    /// How many pool members the fabric holds for this listener. A count of
    /// what is programmed, emphatically not health — nothing in this platform
    /// probes a member, and a number that claimed to be "healthy" would be a
    /// claim made by something that cannot see.
    pub members: u32,
}

/// What is. Written by the controller: a load balancer is a cell-wide fact
/// like the network it fronts, and no machine owns it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadBalancerStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The VIP as the fabric serves it. Empty means not programmed. It is the
    /// *observed* half of `spec.vip`: the two differing is what a reconcile is
    /// for.
    #[serde(default)]
    pub vip: String,
    /// The listeners the fabric holds, with how many members each pool
    /// carries.
    #[serde(default)]
    pub listeners: Vec<ObservedListener>,
}

impl Observed for LoadBalancerStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    /// Nobody. Balancing happens on whichever host the packet arrives at, so
    /// there is no machine to report on it — which is what lets the controller
    /// write this status.
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl Assigned for LoadBalancerSpec {}

pub type LoadBalancer = Resource<LoadBalancerSpec, LoadBalancerStatus>;

// ---- validation ------------------------------------------------------------

/// Why a set of listeners cannot mean what it says. Refused at the API rather
/// than stored, because the alternative is a spec that is accepted, shown back
/// on read, and then quietly refused by the fabric with an error nobody is
/// looking at.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ListenerInvalid {
    #[error(
        "listener {at} has port 0, which is what an unset number looks like — say which port the \
         address answers on"
    )]
    PortZero { at: usize },
    #[error(
        "two listeners claim {protocol}/{port}; one address answering one port twice would be two \
         answers to where a connection goes"
    )]
    Duplicate {
        protocol: Protocol,
        port: u16,
        /// The index of the second claim — the one a person would remove.
        at: usize,
    },
}

impl ListenerInvalid {
    /// Which listener to blame, for an error that lands on the control.
    pub fn at(&self) -> usize {
        match self {
            Self::PortZero { at } | Self::Duplicate { at, .. } => *at,
        }
    }
}

/// Check that every listener is one the fabric could hold.
///
/// A partial function on purpose: emptiness is not checked here, because a
/// load balancer with no listeners yet is an ordinary object waiting to be
/// finished, and the controller says `Incomplete` on it rather than the API
/// refusing to store it.
pub fn validate_listeners(listeners: &[Listener]) -> Result<(), ListenerInvalid> {
    for (at, listener) in listeners.iter().enumerate() {
        if listener.port == 0 {
            return Err(ListenerInvalid::PortZero { at });
        }
        if listeners[..at]
            .iter()
            .any(|earlier| earlier.protocol == listener.protocol && earlier.port == listener.port)
        {
            return Err(ListenerInvalid::Duplicate {
                protocol: listener.protocol,
                port: listener.port,
                at,
            });
        }
    }
    Ok(())
}

// ---- the mirror decision ---------------------------------------------------

/// One member of a fabric pool: the fabric's port id and the backend port.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FabricMember {
    pub port_id: String,
    /// Zero keeps the client's destination port, as the fabric spells it.
    pub port: u16,
}

/// One load-balanced service as the fabric holds it — or should.
///
/// A cloud load balancer with three listeners is three of these: the fabric's
/// service is one `(vip, port, proto)`, so the mapping is per listener, and
/// each carries the whole pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FabricService {
    pub id: String,
    pub vni: u32,
    pub vip: String,
    pub protocol: Protocol,
    pub port: u16,
    /// Sorted, always: the pool is a set, and two orderings of one set must
    /// compare equal or a converged object would be re-programmed forever.
    pub members: Vec<FabricMember>,
}

/// The fabric id of one listener's service, derived from the resource name.
///
/// Derived and not allocated, for the reason a router's VNI is: an id that
/// lives only in a controller is an id a failover loses, and this one has to
/// survive even the *record* being gone — the teardown of a deleted load
/// balancer finds its services by this prefix with nothing else to go on.
pub fn service_id(name: &str, listener: &Listener) -> String {
    format!("{name}#{}/{}", listener.protocol, listener.port)
}

/// Whether a fabric service id belongs to the load balancer called `name`.
///
/// The `#` is what keeps prefixes honest: `…/load-balancers/web` must not
/// claim `…/load-balancers/web-2`'s services.
pub fn owns_service(name: &str, id: &str) -> bool {
    id.strip_prefix(name)
        .is_some_and(|rest| rest.starts_with('#'))
}

/// The services the fabric should hold for this spec.
///
/// `members` is the resolved pool — one fabric port id per pool member, in any
/// order, duplicates already collapsed by the resolver. Resolution (a port
/// name to the fabric's id for it) needs the store and the fabric, so it
/// happens in the controller; the decision about what should exist is here,
/// where it is testable without either.
pub fn desired_services(
    name: &str,
    spec: &LoadBalancerSpec,
    vni: u32,
    vip: &str,
    member_port_ids: &[String],
) -> Vec<FabricService> {
    spec.listeners
        .iter()
        .map(|listener| {
            let mut members: Vec<FabricMember> = member_port_ids
                .iter()
                .map(|port_id| FabricMember {
                    port_id: port_id.clone(),
                    port: listener.member_port,
                })
                .collect();
            members.sort();
            members.dedup();
            FabricService {
                id: service_id(name, listener),
                vni,
                vip: vip.to_string(),
                protocol: listener.protocol,
                port: listener.port,
                members,
            }
        })
        .collect()
}

/// What the controller must do to make the fabric match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorAction {
    Add(FabricService),
    Remove { id: String },
}

/// The gap between what should exist and what the fabric holds, as actions.
///
/// A pure function of the two lists: no clock, no store, no ordering
/// assumption. Running it twice is running it once — when `held` already
/// equals `desired`, the answer is empty, which is what makes a resync over a
/// converged cell free.
///
/// Two shapes of change, both explicit:
///
/// * a service the fabric holds under a name this load balancer owns and the
///   spec no longer asks for is removed;
/// * a service whose content moved — the pool, the VIP — is **removed and
///   added again**, because the fabric's `AddLoadBalancer` fails on a
///   duplicate id rather than restating. Every `Remove` is ordered before
///   every `Add` so the replacement can land.
///
/// Services the fabric holds under other names are not this decision's to
/// touch and never appear in the answer.
pub fn mirror_actions(
    name: &str,
    desired: &[FabricService],
    held: &[FabricService],
) -> Vec<MirrorAction> {
    let mine: Vec<&FabricService> = held.iter().filter(|s| owns_service(name, &s.id)).collect();

    let mut removes = Vec::new();
    let mut adds = Vec::new();

    for service in &mine {
        match desired.iter().find(|d| d.id == service.id) {
            // Held and no longer asked for.
            None => removes.push(MirrorAction::Remove {
                id: service.id.clone(),
            }),
            // Held with different content: replaced, never patched — the
            // fabric has no patch, and a remove that forgot its add would be
            // caught by the next pass computing the same add again.
            Some(want) if !same_service(want, service) => {
                removes.push(MirrorAction::Remove {
                    id: service.id.clone(),
                });
                adds.push(MirrorAction::Add((*want).clone()));
            }
            Some(_) => {}
        }
    }
    for want in desired {
        if !mine.iter().any(|s| s.id == want.id) {
            adds.push(MirrorAction::Add(want.clone()));
        }
    }

    removes.extend(adds);
    removes
}

/// Whether two copies of one service say the same thing. Members are compared
/// as sets — both sides sort before comparing, so an ordering difference is
/// not a difference.
fn same_service(a: &FabricService, b: &FabricService) -> bool {
    let sorted = |s: &FabricService| {
        let mut members = s.members.clone();
        members.sort();
        members
    };
    a.vni == b.vni
        && a.vip == b.vip
        && a.protocol == b.protocol
        && a.port == b.port
        && sorted(a) == sorted(b)
}

/// What the status should record once the fabric holds `desired` — the
/// observed listeners, one per service, in listener order.
pub fn observed_listeners(desired: &[FabricService]) -> Vec<ObservedListener> {
    desired
        .iter()
        .map(|s| ObservedListener {
            protocol: s.protocol,
            port: s.port,
            members: s.members.len() as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(protocol: Protocol, port: u16, member_port: u16) -> Listener {
        Listener {
            protocol,
            port,
            member_port,
        }
    }

    fn spec(listeners: Vec<Listener>, members: Vec<&str>) -> LoadBalancerSpec {
        LoadBalancerSpec {
            network: "projects/p1/networks/n1".into(),
            subnet: "projects/p1/subnets/s1".into(),
            vip: Some("10.20.0.100".into()),
            listeners,
            members: members.into_iter().map(str::to_string).collect(),
        }
    }

    const NAME: &str = "projects/p1/load-balancers/web";

    #[test]
    fn a_listener_on_port_zero_or_said_twice_is_refused() {
        assert_eq!(
            validate_listeners(&[listener(Protocol::Tcp, 0, 0)]),
            Err(ListenerInvalid::PortZero { at: 0 })
        );
        assert_eq!(
            validate_listeners(&[
                listener(Protocol::Tcp, 443, 0),
                listener(Protocol::Tcp, 443, 8443),
            ]),
            Err(ListenerInvalid::Duplicate {
                protocol: Protocol::Tcp,
                port: 443,
                at: 1
            })
        );
        // The same port under two protocols is two different services, not a
        // duplicate — a DNS answering both is the ordinary case.
        assert!(
            validate_listeners(&[
                listener(Protocol::Tcp, 53, 0),
                listener(Protocol::Udp, 53, 0),
            ])
            .is_ok()
        );
        // Nothing yet is not wrong yet: emptiness is the controller's
        // `Incomplete`, never the API's refusal.
        assert!(validate_listeners(&[]).is_ok());
    }

    #[test]
    fn a_service_id_is_a_function_of_the_name_and_cannot_claim_a_neighbours() {
        let l = listener(Protocol::Tcp, 443, 0);
        assert_eq!(service_id(NAME, &l), service_id(NAME, &l));
        assert!(owns_service(NAME, &service_id(NAME, &l)));
        // The near-miss that makes the separator load-bearing: `web` must not
        // tear down `web-2`'s services when it is deleted.
        assert!(!owns_service(
            NAME,
            &service_id("projects/p1/load-balancers/web-2", &l)
        ));
        assert!(!owns_service(
            "projects/p1/load-balancers/web-2",
            &service_id(NAME, &l)
        ));
    }

    #[test]
    fn one_load_balancer_is_one_fabric_service_per_listener() {
        let s = spec(
            vec![
                listener(Protocol::Tcp, 443, 8443),
                listener(Protocol::Udp, 53, 0),
            ],
            vec!["projects/p1/ports/a", "projects/p1/ports/b"],
        );
        let desired = desired_services(
            NAME,
            &s,
            4711,
            "10.20.0.100",
            &["fp-a".into(), "fp-b".into()],
        );
        assert_eq!(desired.len(), 2);
        assert_eq!(desired[0].port, 443);
        assert_eq!(
            desired[0].members,
            vec![
                FabricMember {
                    port_id: "fp-a".into(),
                    port: 8443
                },
                FabricMember {
                    port_id: "fp-b".into(),
                    port: 8443
                },
            ]
        );
        // The UDP listener keeps the client's destination port.
        assert!(desired[1].members.iter().all(|m| m.port == 0));
    }

    #[test]
    fn a_member_named_twice_is_one_member() {
        let s = spec(vec![listener(Protocol::Tcp, 80, 0)], vec![]);
        let desired = desired_services(NAME, &s, 1, "10.0.0.9", &["fp-a".into(), "fp-a".into()]);
        assert_eq!(desired[0].members.len(), 1);
    }

    #[test]
    fn a_converged_fabric_gets_no_actions_at_all() {
        // Idempotence, stated as a test: the mirror of a settled object is
        // empty, or every resync would tear the datapath down and rebuild it.
        let s = spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]);
        let desired = desired_services(NAME, &s, 4711, "10.20.0.100", &["fp-a".into()]);
        assert!(mirror_actions(NAME, &desired, &desired).is_empty());

        // The same pool in another order is the same pool.
        let mut reordered = desired.clone();
        let s2 = spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]);
        let both = desired_services(
            NAME,
            &s2,
            4711,
            "10.20.0.100",
            &["fp-b".into(), "fp-a".into()],
        );
        reordered[0].members = {
            let mut m = both[0].members.clone();
            m.reverse();
            m
        };
        let desired_two = desired_services(
            NAME,
            &s2,
            4711,
            "10.20.0.100",
            &["fp-a".into(), "fp-b".into()],
        );
        assert!(mirror_actions(NAME, &desired_two, &reordered).is_empty());
    }

    #[test]
    fn a_changed_service_is_removed_before_it_is_added_again() {
        let before = desired_services(
            NAME,
            &spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]),
            4711,
            "10.20.0.100",
            &["fp-a".into()],
        );
        let after = desired_services(
            NAME,
            &spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]),
            4711,
            "10.20.0.100",
            &["fp-a".into(), "fp-b".into()],
        );
        let actions = mirror_actions(NAME, &after, &before);
        assert_eq!(
            actions,
            vec![
                MirrorAction::Remove {
                    id: before[0].id.clone()
                },
                MirrorAction::Add(after[0].clone()),
            ],
            "the fabric fails a duplicate id, so a change is a replace and the \
             remove has to come first"
        );

        // Running the answer twice is running it once: with the fabric now
        // holding `after`, there is nothing left to do.
        assert!(mirror_actions(NAME, &after, &after).is_empty());
    }

    #[test]
    fn a_listener_that_was_dropped_is_retired_and_a_new_one_lands() {
        let held = desired_services(
            NAME,
            &spec(vec![listener(Protocol::Tcp, 80, 0)], vec![]),
            4711,
            "10.20.0.100",
            &[],
        );
        let desired = desired_services(
            NAME,
            &spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]),
            4711,
            "10.20.0.100",
            &[],
        );
        let actions = mirror_actions(NAME, &desired, &held);
        assert!(actions.contains(&MirrorAction::Remove {
            id: held[0].id.clone()
        }));
        assert!(actions.contains(&MirrorAction::Add(desired[0].clone())));
    }

    #[test]
    fn a_teardown_removes_everything_it_owns_and_nothing_it_does_not() {
        let mine = desired_services(
            NAME,
            &spec(
                vec![
                    listener(Protocol::Tcp, 443, 0),
                    listener(Protocol::Udp, 53, 0),
                ],
                vec![],
            ),
            4711,
            "10.20.0.100",
            &[],
        );
        let theirs = desired_services(
            "projects/p1/load-balancers/web-2",
            &spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]),
            4711,
            "10.20.0.101",
            &[],
        );
        let mut held = mine.clone();
        held.extend(theirs.clone());

        let actions = mirror_actions(NAME, &[], &held);
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, MirrorAction::Remove { id } if owns_service(NAME, id))),
            "a teardown touched a neighbour's service: {actions:?}"
        );
    }

    #[test]
    fn what_the_status_records_is_a_count_of_the_pool_and_never_health() {
        let desired = desired_services(
            NAME,
            &spec(vec![listener(Protocol::Tcp, 443, 0)], vec![]),
            4711,
            "10.20.0.100",
            &["fp-a".into(), "fp-b".into()],
        );
        assert_eq!(
            observed_listeners(&desired),
            vec![ObservedListener {
                protocol: Protocol::Tcp,
                port: 443,
                members: 2
            }]
        );
    }
}
