//! Which address a port gets, and why.
//!
//! The decision, as a pure function; the controller that carries it out is
//! [`velstra_cloud_controller::address`]. It is the same shape as placing an
//! instance on a node: a controller reads the world, asks here what should be
//! true, and writes one field with a compare-and-swap.
//!
//! **There is no allocation table, and no reservation.** The set of addresses
//! in use is counted from the ports that exist, exactly as quota is counted
//! from the instances that exist. A table would be a second record of the same
//! fact and would eventually disagree with the ports; a reservation that is not
//! released when a controller dies is an address that never comes back. Two
//! controllers looking at one port produce one assignment and one retry,
//! because the write is a compare-and-swap and the loser re-reads a port that
//! now needs nothing.
//!
//! **An address, once given, is never moved.** A port whose address changes
//! under a running guest is an outage with no error message, so this only ever
//! fills in what is absent.

use std::collections::BTreeSet;

use crate::{
    loadbalancer::LoadBalancer,
    meta::{Condition, ConditionStatus},
    network::{Cidr, allocate, mac_for},
    resources::{FloatingIp, Port, Subnet},
};

/// What is missing from a port and can be filled in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    pub address: Option<String>,
    pub mac: Option<String>,
}

impl Assignment {
    pub fn is_empty(&self) -> bool {
        self.address.is_none() && self.mac.is_none()
    }
}

/// Why a port cannot be given an address. Each one is a different thing an
/// operator would change, which is why it is not one error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unaddressable {
    #[error("{subnet} does not exist")]
    NoSuchSubnet { subnet: String },
    #[error("{subnet} declares {cidr:?}, which is not a range: {why}")]
    SubnetNotAddressable {
        subnet: String,
        cidr: String,
        why: String,
    },
    #[error("{subnet} has no free address left in {cidr}")]
    SubnetFull { subnet: String, cidr: String },
}

/// Whether there is anything to do for this port at all.
///
/// A port being deleted is left alone: filling in an address on the way out
/// would be a spec write on an object nobody will ever read, and it would move
/// the generation under an agent that is trying to let go of it.
pub fn needs_assignment(port: &Port) -> bool {
    !port.meta.is_deleting() && (port.spec.address.is_none() || port.spec.mac.is_none())
}

/// Decide what `port` should be given.
///
/// `others` is every port in the cell; the ones on the same subnet are what
/// make an address taken. Passing all of them rather than a filtered list is
/// deliberate — the filter is part of the decision, and a caller that filtered
/// differently would allocate an address that is already in use.
pub fn assign(
    port: &Port,
    subnet: Option<&Subnet>,
    others: &[Port],
    floating: &[FloatingIp],
    balancers: &[LoadBalancer],
) -> Result<Assignment, Unaddressable> {
    let mac = port
        .spec
        .mac
        .is_none()
        // Derived from the uid rather than drawn at random, so a write that is
        // lost and retried produces the same address rather than a new NIC.
        .then(|| mac_for(&port.meta.uid));

    if port.spec.address.is_some() {
        return Ok(Assignment { address: None, mac });
    }

    let Some(subnet) = subnet else {
        return Err(Unaddressable::NoSuchSubnet {
            subnet: port.spec.subnet.clone(),
        });
    };
    let name = subnet.meta.name.to_string();
    let cidr =
        Cidr::parse(&subnet.spec.cidr).map_err(|why| Unaddressable::SubnetNotAddressable {
            subnet: name.clone(),
            cidr: subnet.spec.cidr.clone(),
            why: why.to_string(),
        })?;

    let address = allocate(&cidr, &taken(subnet, others, floating, balancers)).ok_or(
        Unaddressable::SubnetFull {
            subnet: name,
            cidr: subnet.spec.cidr.clone(),
        },
    )?;
    Ok(Assignment {
        address: Some(address.to_string()),
        mac,
    })
}

/// Every address in this subnet that must not be handed out.
///
/// The gateway and whatever the subnet reserves, plus every address a port
/// already carries, plus every floating address already allocated. A port that
/// is being deleted still counts: its guest may still be running, and an
/// address handed out from under it would be two machines with one address for
/// as long as the teardown takes.
///
/// **Floating IPs and load balancer VIPs are in here because they are the
/// second and third allocator this range would otherwise have.** A floating
/// address, a VIP and a port address all come out of the same subnet, and a
/// count that saw only one kind would hand the same address to two of them —
/// silently, and only visible as a guest whose traffic sometimes goes
/// somewhere else. None of the parameters is optional for the same reason
/// `others` is not: a caller that passed an empty slice would be allocating
/// against a range it had only half looked at.
fn taken(
    subnet: &Subnet,
    ports: &[Port],
    floating: &[FloatingIp],
    balancers: &[LoadBalancer],
) -> BTreeSet<std::net::IpAddr> {
    let name = subnet.meta.name.to_string();
    subnet
        .spec
        .reserved
        .iter()
        .chain(std::iter::once(&subnet.spec.gateway))
        .filter_map(|a| a.parse().ok())
        .chain(
            ports
                .iter()
                .filter(|p| p.spec.subnet == name)
                .filter_map(|p| p.spec.address.as_deref())
                .filter_map(|a| a.split('/').next())
                .filter_map(|a| a.parse().ok()),
        )
        .chain(
            floating
                .iter()
                .filter(|f| f.spec.subnet == name)
                .filter_map(|f| f.spec.address.as_deref())
                .filter_map(|a| a.split('/').next())
                .filter_map(|a| a.parse().ok()),
        )
        .chain(
            balancers
                .iter()
                .filter(|l| l.spec.subnet == name)
                .filter_map(|l| l.spec.vip.as_deref())
                .filter_map(|a| a.split('/').next())
                .filter_map(|a| a.parse().ok()),
        )
        .collect()
}

/// Decide what address a floating IP should be given, or why it cannot have one.
///
/// The same counting as [`assign`], over the same range, which is the entire
/// point: one allocator decides for both kinds of holder. A floating IP that
/// already names an address keeps it — a person pinning one is the ordinary way
/// to move an address between subnetted environments.
pub fn assign_floating(
    fip: &FloatingIp,
    subnet: Option<&Subnet>,
    ports: &[Port],
    others: &[FloatingIp],
    balancers: &[LoadBalancer],
) -> Result<Option<String>, Unaddressable> {
    if fip.spec.address.is_some() {
        return Ok(None);
    }
    let Some(subnet) = subnet else {
        return Err(Unaddressable::NoSuchSubnet {
            subnet: fip.spec.subnet.clone(),
        });
    };
    let name = subnet.meta.name.to_string();
    let cidr =
        Cidr::parse(&subnet.spec.cidr).map_err(|why| Unaddressable::SubnetNotAddressable {
            subnet: name.clone(),
            cidr: subnet.spec.cidr.clone(),
            why: why.to_string(),
        })?;
    // This one excluded from the count it is being allocated against — it holds
    // nothing yet, and a self-collision would be a subnet that looks one
    // address fuller than it is.
    let others: Vec<FloatingIp> = others
        .iter()
        .filter(|f| f.meta.name != fip.meta.name)
        .cloned()
        .collect();
    let address = allocate(&cidr, &taken(subnet, ports, &others, balancers)).ok_or(
        Unaddressable::SubnetFull {
            subnet: name,
            cidr: subnet.spec.cidr.clone(),
        },
    )?;
    Ok(Some(address.to_string()))
}

/// Decide what address a load balancer's VIP should be, or why it cannot have
/// one.
///
/// The same counting as [`assign`] and [`assign_floating`], over the same
/// range: one allocator decides for all three kinds of holder. A load balancer
/// that already names a VIP keeps it — pinning one is how an operator moves a
/// known address in front of a rebuilt pool.
pub fn assign_vip(
    balancer: &LoadBalancer,
    subnet: Option<&Subnet>,
    ports: &[Port],
    floating: &[FloatingIp],
    others: &[LoadBalancer],
) -> Result<Option<String>, Unaddressable> {
    if balancer.spec.vip.is_some() {
        return Ok(None);
    }
    let Some(subnet) = subnet else {
        return Err(Unaddressable::NoSuchSubnet {
            subnet: balancer.spec.subnet.clone(),
        });
    };
    let name = subnet.meta.name.to_string();
    let cidr =
        Cidr::parse(&subnet.spec.cidr).map_err(|why| Unaddressable::SubnetNotAddressable {
            subnet: name.clone(),
            cidr: subnet.spec.cidr.clone(),
            why: why.to_string(),
        })?;
    // Excluded from the count it is being allocated against, exactly as a
    // floating IP is: it holds nothing yet, and a self-collision would make
    // the subnet look one address fuller than it is.
    let others: Vec<LoadBalancer> = others
        .iter()
        .filter(|l| l.meta.name != balancer.meta.name)
        .cloned()
        .collect();
    let address = allocate(&cidr, &taken(subnet, ports, floating, &others)).ok_or(
        Unaddressable::SubnetFull {
            subnet: name,
            cidr: subnet.spec.cidr.clone(),
        },
    )?;
    Ok(Some(address.to_string()))
}

/// How many addresses a subnet has given out and how many are left.
///
/// Counted rather than tracked, for the same reason as everything else here: a
/// running total that is incremented and decremented drifts, and a count of
/// what exists cannot.
pub fn counts(
    subnet: &Subnet,
    ports: &[Port],
    floating: &[FloatingIp],
    balancers: &[LoadBalancer],
) -> (u32, u32) {
    let Ok(cidr) = Cidr::parse(&subnet.spec.cidr) else {
        return (0, 0);
    };
    let name = subnet.meta.name.to_string();
    let allocated = ports
        .iter()
        .filter(|p| p.spec.subnet == name && p.spec.address.is_some())
        .count() as u64
        + floating
            .iter()
            .filter(|f| f.spec.subnet == name && f.spec.address.is_some())
            .count() as u64
        + balancers
            .iter()
            .filter(|l| l.spec.subnet == name && l.spec.vip.is_some())
            .count() as u64;
    let usable = cidr.usable();
    // Reserved addresses are not available even though nothing holds them.
    let reserved = taken(subnet, &[], &[], &[]).len() as u64;
    (
        allocated.min(u32::MAX as u64) as u32,
        usable
            .saturating_sub(allocated)
            .saturating_sub(reserved)
            .min(u32::MAX as u64) as u32,
    )
}

/// What a port says about itself when it cannot be given an address.
///
/// On the object an operator is already looking at, with a reason they can act
/// on — a port silently without an address is a guest that will never come up
/// and nothing anywhere saying why.
pub fn unaddressable_condition(why: &Unaddressable, at_generation: u64) -> Condition {
    let reason = match why {
        Unaddressable::NoSuchSubnet { .. } => "NoSuchSubnet",
        Unaddressable::SubnetNotAddressable { .. } => "SubnetNotAddressable",
        Unaddressable::SubnetFull { .. } => "SubnetFull",
    };
    Condition::new(
        "Ready",
        ConditionStatus::False,
        reason,
        &why.to_string(),
        at_generation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::{FloatingIp, PortSpec, PortStatus, Resource, SubnetSpec, SubnetStatus},
    };

    const SUBNET: &str = "projects/p1/subnets/sub-a";

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    fn subnet(cidr: &str) -> Subnet {
        Resource::new(
            meta(SUBNET),
            SubnetSpec {
                network: "projects/p1/networks/net-a".into(),
                cidr: cidr.into(),
                gateway: "10.20.0.1".into(),
                dns: vec!["10.20.0.1".into()],
                reserved: vec!["10.20.0.2".into()],
            },
            SubnetStatus::default(),
        )
    }

    fn port(id: &str, address: Option<&str>, mac: Option<&str>) -> Port {
        Resource::new(
            meta(&format!("projects/p1/ports/{id}")),
            PortSpec {
                network: "projects/p1/networks/net-a".into(),
                subnet: SUBNET.into(),
                address: address.map(str::to_string),
                mac: mac.map(str::to_string),
                ..Default::default()
            },
            PortStatus::default(),
        )
    }

    fn fip(id: &str, address: Option<&str>) -> FloatingIp {
        Resource::new(
            meta(&format!("projects/p1/floatingips/{id}")),
            crate::resources::FloatingIpSpec {
                instance: String::new(),
                subnet: SUBNET.into(),
                address: address.map(str::to_string),
                port: String::new(),
                delivery: Default::default(),
                announce: None,
            },
            crate::resources::FloatingIpStatus::default(),
        )
    }

    /// The whole reason floating IPs are counted here: a port must never be
    /// given an address a floating IP already holds.
    ///
    /// Two allocators over one range is the defect this design exists to not
    /// have, and it is invisible until two machines answer to one address.
    #[test]
    fn a_port_is_not_given_an_address_a_floating_ip_holds() {
        let held = vec![fip("f1", Some("10.20.0.3"))];
        let assignment = assign(
            &port("port-a", None, None),
            Some(&subnet("10.20.0.0/24")),
            &[],
            &held,
            &[],
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.4"));
    }

    /// And the other way round: a floating IP must never be given a port's.
    #[test]
    fn a_floating_ip_is_not_given_an_address_a_port_holds() {
        let ports = vec![port("port-a", Some("10.20.0.3"), None)];
        let address = assign_floating(
            &fip("f1", None),
            Some(&subnet("10.20.0.0/24")),
            &ports,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(address.as_deref(), Some("10.20.0.4"));
    }

    /// A floating IP does not collide with itself.
    ///
    /// The list handed in is every floating IP in the cell, this one included —
    /// filtering the caller's own out is what keeps a re-run of the same
    /// decision from stepping one address forward each time.
    #[test]
    fn a_floating_ip_does_not_count_itself() {
        let me = fip("f1", None);
        let all = vec![me.clone(), fip("f2", Some("10.20.0.3"))];
        let address = assign_floating(&me, Some(&subnet("10.20.0.0/24")), &[], &all, &[]).unwrap();
        assert_eq!(address.as_deref(), Some("10.20.0.4"));
    }

    /// A floating IP that names an address keeps it — pinning one is the
    /// ordinary way an operator moves a known address.
    #[test]
    fn a_floating_ip_that_names_an_address_keeps_it() {
        let pinned = fip("f1", Some("10.20.0.200"));
        let address =
            assign_floating(&pinned, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        assert_eq!(address, None, "a pinned floating address was reassigned");
    }

    fn lb(id: &str, vip: Option<&str>) -> LoadBalancer {
        Resource::new(
            meta(&format!("projects/p1/load-balancers/{id}")),
            crate::loadbalancer::LoadBalancerSpec {
                network: "projects/p1/networks/net-a".into(),
                subnet: SUBNET.into(),
                vip: vip.map(str::to_string),
                listeners: vec![],
                members: vec![],
            },
            crate::loadbalancer::LoadBalancerStatus::default(),
        )
    }

    /// The third kind of holder: a port must never be given a VIP, and a VIP
    /// must never be a port's or a floating IP's address. One allocator over
    /// one range, or two machines answer to one address.
    #[test]
    fn a_vip_and_a_port_address_are_never_the_same_address() {
        let balancers = vec![lb("web", Some("10.20.0.3"))];
        let assignment = assign(
            &port("port-a", None, None),
            Some(&subnet("10.20.0.0/24")),
            &[],
            &[],
            &balancers,
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.4"));

        let ports = vec![port("port-a", Some("10.20.0.3"), None)];
        let held = vec![fip("f1", Some("10.20.0.4"))];
        let vip = assign_vip(
            &lb("web", None),
            Some(&subnet("10.20.0.0/24")),
            &ports,
            &held,
            &[],
        )
        .unwrap();
        assert_eq!(vip.as_deref(), Some("10.20.0.5"));
    }

    /// A load balancer does not collide with itself, and a pinned VIP is kept —
    /// the same two properties a floating IP's allocation has, for the same
    /// reasons.
    #[test]
    fn a_vip_is_stable_against_itself_and_a_pinned_one_is_kept() {
        let me = lb("web", None);
        let all = vec![me.clone(), lb("db", Some("10.20.0.3"))];
        let vip = assign_vip(&me, Some(&subnet("10.20.0.0/24")), &[], &[], &all).unwrap();
        assert_eq!(vip.as_deref(), Some("10.20.0.4"));

        let pinned = lb("web", Some("10.20.0.200"));
        let vip = assign_vip(&pinned, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        assert_eq!(vip, None, "a pinned VIP was reassigned");
    }

    /// The usage an operator reads counts both kinds of holder. A subnet that
    /// reported room it did not have is how a person finds out too late.
    #[test]
    fn usage_counts_floating_addresses_too() {
        let subnet = subnet("10.20.0.0/24");
        let ports = vec![port("port-a", Some("10.20.0.3"), None)];
        let held = vec![fip("f1", Some("10.20.0.4"))];
        let (with, _) = counts(&subnet, &ports, &held, &[]);
        let (without, _) = counts(&subnet, &ports, &[], &[]);
        assert_eq!(with, 2);
        assert_eq!(without, 1, "the fixture does not isolate the difference");
    }

    #[test]
    fn a_new_port_is_given_the_lowest_address_nothing_else_holds() {
        // .1 is the gateway and .2 is reserved, so the first port lands on .3.
        let existing = vec![port("port-a", Some("10.20.0.3"), None)];
        let assignment = assign(
            &port("port-b", None, None),
            Some(&subnet("10.20.0.0/24")),
            &existing,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.4"));
        assert!(assignment.mac.is_some());
    }

    #[test]
    fn an_address_that_was_already_given_is_never_moved() {
        // The failure this prevents has no error message: a guest whose address
        // changes underneath it simply stops being reachable.
        let has = port("port-a", Some("10.20.0.50"), Some("52:54:00:12:34:56"));
        assert!(!needs_assignment(&has));
        let assignment = assign(&has, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        assert_eq!(assignment, Assignment::default());
    }

    #[test]
    fn a_port_with_an_address_but_no_mac_gets_only_the_mac() {
        let half = port("port-a", Some("10.20.0.50"), None);
        assert!(needs_assignment(&half));
        let assignment = assign(&half, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        assert_eq!(assignment.address, None);
        assert!(assignment.mac.is_some());
    }

    #[test]
    fn the_mac_is_the_same_one_however_often_it_is_asked_for() {
        // A write that is lost and retried must not give the guest a new NIC.
        let p = port("port-a", None, None);
        let first = assign(&p, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        let second = assign(&p, Some(&subnet("10.20.0.0/24")), &[], &[], &[]).unwrap();
        assert_eq!(first.mac, second.mac);
    }

    #[test]
    fn a_port_on_its_way_out_is_left_alone() {
        let mut going = port("port-a", None, None);
        going.meta.deleted_at = Some(Timestamp::now());
        assert!(!needs_assignment(&going));
    }

    #[test]
    fn an_address_held_by_a_port_that_is_being_deleted_is_still_taken() {
        // Its guest may still be running, and the teardown is not instant.
        let mut going = port("port-a", Some("10.20.0.3"), None);
        going.meta.deleted_at = Some(Timestamp::now());
        let assignment = assign(
            &port("port-b", None, None),
            Some(&subnet("10.20.0.0/24")),
            &[going],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.4"));
    }

    #[test]
    fn a_port_on_another_subnet_does_not_take_an_address_from_this_one() {
        let mut elsewhere = port("port-a", Some("10.20.0.3"), None);
        elsewhere.spec.subnet = "projects/p1/subnets/sub-b".into();
        let assignment = assign(
            &port("port-b", None, None),
            Some(&subnet("10.20.0.0/24")),
            &[elsewhere],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.3"));
    }

    #[test]
    fn every_refusal_names_the_thing_an_operator_would_change() {
        let missing = assign(&port("port-a", None, None), None, &[], &[], &[]).unwrap_err();
        assert_eq!(
            missing,
            Unaddressable::NoSuchSubnet {
                subnet: SUBNET.into()
            }
        );
        assert_eq!(unaddressable_condition(&missing, 1).reason, "NoSuchSubnet");

        let nonsense = assign(
            &port("port-a", None, None),
            Some(&subnet("not-a-range")),
            &[],
            &[],
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            nonsense,
            Unaddressable::SubnetNotAddressable { .. }
        ));

        // A /30 holds two addresses, and this subnet reserves both of them.
        let full = assign(
            &port("port-a", None, None),
            Some(&subnet("10.20.0.0/30")),
            &[],
            &[],
            &[],
        )
        .unwrap_err();
        assert!(matches!(full, Unaddressable::SubnetFull { .. }), "{full:?}");
        assert!(full.to_string().contains("10.20.0.0/30"), "{full}");
    }

    #[test]
    fn a_subnet_counts_what_it_has_given_out_rather_than_tracking_a_total() {
        let subnet = subnet("10.20.0.0/24");
        let ports = vec![
            port("port-a", Some("10.20.0.3"), None),
            port("port-b", Some("10.20.0.4"), None),
            port("port-c", None, None),
        ];
        let (allocated, available) = counts(&subnet, &ports, &[], &[]);
        assert_eq!(allocated, 2);
        // 254 usable, less the gateway and the one reserved address, less the
        // two that are out.
        assert_eq!(available, 250);
    }
}
