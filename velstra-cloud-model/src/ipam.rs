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
    meta::{Condition, ConditionStatus},
    network::{Cidr, allocate, mac_for},
    resources::{Port, Subnet},
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

    let address = allocate(&cidr, &taken(subnet, others)).ok_or(Unaddressable::SubnetFull {
        subnet: name,
        cidr: subnet.spec.cidr.clone(),
    })?;
    Ok(Assignment {
        address: Some(address.to_string()),
        mac,
    })
}

/// Every address in this subnet that must not be handed out.
///
/// The gateway and whatever the subnet reserves, plus every address a port
/// already carries. A port that is being deleted still counts: its guest may
/// still be running, and an address handed out from under it would be two
/// machines with one address for as long as the teardown takes.
fn taken(subnet: &Subnet, ports: &[Port]) -> BTreeSet<std::net::IpAddr> {
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
        .collect()
}

/// How many addresses a subnet has given out and how many are left.
///
/// Counted rather than tracked, for the same reason as everything else here: a
/// running total that is incremented and decremented drifts, and a count of
/// what exists cannot.
pub fn counts(subnet: &Subnet, ports: &[Port]) -> (u32, u32) {
    let Ok(cidr) = Cidr::parse(&subnet.spec.cidr) else {
        return (0, 0);
    };
    let name = subnet.meta.name.to_string();
    let allocated = ports
        .iter()
        .filter(|p| p.spec.subnet == name && p.spec.address.is_some())
        .count() as u64;
    let usable = cidr.usable();
    // Reserved addresses are not available even though nothing holds them.
    let reserved = taken(subnet, &[]).len() as u64;
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
        resources::{PortSpec, PortStatus, Resource, SubnetSpec, SubnetStatus},
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

    #[test]
    fn a_new_port_is_given_the_lowest_address_nothing_else_holds() {
        // .1 is the gateway and .2 is reserved, so the first port lands on .3.
        let existing = vec![port("port-a", Some("10.20.0.3"), None)];
        let assignment = assign(
            &port("port-b", None, None),
            Some(&subnet("10.20.0.0/24")),
            &existing,
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
        let assignment = assign(&has, Some(&subnet("10.20.0.0/24")), &[]).unwrap();
        assert_eq!(assignment, Assignment::default());
    }

    #[test]
    fn a_port_with_an_address_but_no_mac_gets_only_the_mac() {
        let half = port("port-a", Some("10.20.0.50"), None);
        assert!(needs_assignment(&half));
        let assignment = assign(&half, Some(&subnet("10.20.0.0/24")), &[]).unwrap();
        assert_eq!(assignment.address, None);
        assert!(assignment.mac.is_some());
    }

    #[test]
    fn the_mac_is_the_same_one_however_often_it_is_asked_for() {
        // A write that is lost and retried must not give the guest a new NIC.
        let p = port("port-a", None, None);
        let first = assign(&p, Some(&subnet("10.20.0.0/24")), &[]).unwrap();
        let second = assign(&p, Some(&subnet("10.20.0.0/24")), &[]).unwrap();
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
        )
        .unwrap();
        assert_eq!(assignment.address.as_deref(), Some("10.20.0.3"));
    }

    #[test]
    fn every_refusal_names_the_thing_an_operator_would_change() {
        let missing = assign(&port("port-a", None, None), None, &[]).unwrap_err();
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
        let (allocated, available) = counts(&subnet, &ports);
        assert_eq!(allocated, 2);
        // 254 usable, less the gateway and the one reserved address, less the
        // two that are out.
        assert_eq!(available, 250);
    }
}
