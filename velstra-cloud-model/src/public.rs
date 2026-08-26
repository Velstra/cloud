//! Public addresses: the ones a guest actually holds.
//!
//! ## Two ways an address can reach a guest, and why only one of them is any
//! good
//!
//! **Translated** (`Delivery::Nat`) is the classic floating IP: the address
//! lives at the edge, something rewrites the destination on the way in and the
//! source on the way out, and the guest never sees it. It works, it is what
//! most platforms do, and it costs three things — every inbound packet crosses
//! whatever does the translating, the guest cannot tell anybody its own address
//! (which breaks SIP, FTP, IPsec, mDNS and every protocol that puts an address
//! in a payload), and connection state lives somewhere that has to survive.
//!
//! **Routed** (`Delivery::Routed`) hands the address to the guest. It is bound
//! to the guest's port as a second address, the guest configures it, and
//! nothing anywhere translates anything. What makes it work is not a table but
//! a *route*: somebody announces "this /32 is here" to the network above, and
//! packets arrive where the guest is.
//!
//! This module is about the second, because it is the one that puts the address
//! in the machine.
//!
//! ## Who announces it, and the reason the choice is real
//!
//! [`Announce::FromHost`] — the hypervisor holding the guest announces the /32
//! itself. Inbound packets go to that machine and no further; outbound leave
//! from it directly. **Nothing is encapsulated and nothing is hairpinned**: for
//! north-south traffic the overlay is not in the path at all. And the route
//! follows a live migration by construction — each host announces the addresses
//! of the ports it holds, so the old one stops when the port leaves and the new
//! one starts when it arrives. There is no sequencing to get right and no
//! central thing to be down.
//!
//! The price is that every hypervisor must be able to peer with the network
//! above it. That is a decision about the rack, not about this platform, and a
//! cell where it is untrue must not be forced into it.
//!
//! [`Announce::FromGateway`] — a machine marked as a gateway announces it, and
//! traffic reaches the guest over the overlay. The upstream sees a small, stable
//! set of next hops; the cost is a hairpin through the gateway in both
//! directions and one more thing that can be full.
//!
//! Neither is a default worth imposing, so the **network** says what this cell
//! does and an address may say otherwise. Those are two different questions:
//! one is about the wiring, one is about a particular service.
//!
//! ## What the guest is told
//!
//! A routed address is given as a `/32` (or `/128`) with an **on-link next hop
//! that is not in any subnet**: `169.254.1.1`, answered by the host itself.
//! That is what makes the address independent of any L2 segment — there is no
//! broadcast domain to belong to, so the same address works on any hypervisor
//! in the cell, and nothing has to move a VLAN when a guest migrates.
//!
//! It also decides the guest's **default route**: a guest holding a public
//! address defaults out through it. Leaving the default on the tenant gateway
//! would send replies from the public address out of a door they cannot return
//! through, which is the asymmetric-routing bug that takes a day to find.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

/// The next hop a guest routes through for a public address it holds.
///
/// Link-local and in no subnet anybody declared, on purpose: it belongs to no
/// broadcast domain, so the same configuration is correct on every hypervisor
/// in the cell and stays correct across a migration. The host answers for it.
pub const NEXT_HOP_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 1);

/// The same for IPv6. `fe80::1` is link-local by construction, so it needs no
/// on-link flag anywhere.
pub const NEXT_HOP_V6: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);

/// How an address reaches the guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Translated at the edge; the guest never sees the address.
    ///
    /// The default, because it is what this object meant before there was a
    /// choice — an object written last year must not change behaviour because
    /// the platform learned a second trick.
    #[default]
    Nat,
    /// The guest holds the address. Nothing translates anything.
    Routed,
}

/// Who tells the network above where this address is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Announce {
    /// A machine marked as a gateway. The upstream sees few, stable next hops;
    /// traffic hairpins through it in both directions.
    ///
    /// The default because it is the one that works without every hypervisor
    /// being allowed to speak to the router above it — which is a fact about
    /// somebody's rack, and the safe assumption is the one that needs less.
    #[default]
    FromGateway,
    /// The hypervisor holding the guest. Shortest path, no encapsulation for
    /// north-south traffic, and the route follows the guest.
    FromHost,
}

/// Why an address cannot be published.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// A routed address hands the guest a real address, and a tenant prefix is
    /// not one.
    #[error(
        "{subnet} is not on an external network, so an address from it is not one the world can \
         reach. A routed address is given to the guest as its own; taking it from a tenant range \
         would hand the guest an address that is real nowhere."
    )]
    NotExternal { subnet: String },
    /// Announced from a gateway, in a cell with none.
    #[error(
        "no node in this cell is marked as a gateway, so there is nothing to announce {address} \
         from. Mark one, or announce from the host holding the guest — which needs every \
         hypervisor to peer with the network above it."
    )]
    NoGateway { address: String },
    /// A routed address on a port that is not on a network the gateway can
    /// reach — only meaningful for the gateway mode, where the packets have to
    /// travel over the overlay to get to the guest.
    #[error(
        "{port} is on {network}, and no router joins it to {external}. A gateway can only carry \
         traffic into a network it can route into."
    )]
    NoRouteToPort {
        port: String,
        network: String,
        external: String,
    },
}

/// One public address, as these decisions see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressView {
    pub name: String,
    /// The address itself, once something has decided it.
    pub address: Option<IpAddr>,
    /// The subnet it comes from.
    pub subnet: String,
    /// Whether that subnet is on a network an operator marked external.
    pub subnet_is_external: bool,
    pub delivery: Delivery,
    /// What the address asks for; `None` means "whatever the network says".
    pub announce: Option<Announce>,
    /// The port it is bound to, empty when it is held and pointing at nothing.
    pub port: String,
}

/// Whether this address may be published as asked.
///
/// Answered before anything is programmed, because all of it is knowable then —
/// and an address that turns out to be unreachable is discovered by somebody's
/// customer.
pub fn may_publish(
    view: &AddressView,
    network_default: Announce,
    gateways: usize,
) -> Result<(), Refusal> {
    if view.delivery == Delivery::Routed && !view.subnet_is_external {
        return Err(Refusal::NotExternal {
            subnet: view.subnet.clone(),
        });
    }
    // Only when it is actually bound to something. An address held and pointing
    // at nothing announces nothing, so a cell with no gateway may still hold
    // one — which is the whole reason an unassociated address exists.
    if !view.port.is_empty()
        && view.announce.unwrap_or(network_default) == Announce::FromGateway
        && gateways == 0
    {
        return Err(Refusal::NoGateway {
            address: view
                .address
                .map(|a| a.to_string())
                .unwrap_or_else(|| view.name.clone()),
        });
    }
    Ok(())
}

/// Where the announcement comes from, right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Announcer {
    /// The machine holding the guest. Shortest path.
    Host(String),
    /// These machines, over the overlay.
    Gateways(Vec<String>),
    /// Nobody, and why.
    Nowhere(Silent),
}

/// Why nothing is announcing an address.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Silent {
    #[error("it is not bound to a port, which is what an address held for later looks like")]
    Unbound,
    #[error("the guest holding {port} has not been placed on a machine yet")]
    NotPlaced { port: String },
    #[error("no node in this cell is marked as a gateway")]
    NoGateway,
    /// The address is translated at the edge, so there is no /32 to announce —
    /// the edge answers for it as part of whatever range it already holds.
    #[error("a translated address is answered for at the edge, not announced as a route of its own")]
    Translated,
}

/// Who announces this address, given where its port actually is.
///
/// Takes the port's **observed** node rather than its assignment: an address is
/// reachable where the guest *is*, and announcing from where it was assigned is
/// how a migration's last moments become a black hole.
pub fn announcer(
    view: &AddressView,
    network_default: Announce,
    port_node: Option<&str>,
    gateways: &[String],
) -> Announcer {
    if view.delivery == Delivery::Nat {
        return Announcer::Nowhere(Silent::Translated);
    }
    if view.port.is_empty() {
        return Announcer::Nowhere(Silent::Unbound);
    }
    match view.announce.unwrap_or(network_default) {
        Announce::FromHost => match port_node {
            Some(node) => Announcer::Host(node.to_string()),
            None => Announcer::Nowhere(Silent::NotPlaced {
                port: view.port.clone(),
            }),
        },
        Announce::FromGateway => {
            if gateways.is_empty() {
                Announcer::Nowhere(Silent::NoGateway)
            } else {
                Announcer::Gateways(gateways.to_vec())
            }
        }
    }
}

/// What a guest holding this address must have configured.
///
/// Rendered by the metadata service and by nothing else, so what a guest is
/// told is one function rather than a shape assembled twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestRoute {
    /// The address, as a host route: `/32` or `/128`. It belongs to no
    /// broadcast domain, which is what lets it be correct on every machine in
    /// the cell.
    pub address: IpAddr,
    pub prefix_len: u8,
    /// The next hop, answered by the host itself.
    pub via: IpAddr,
    /// Whether the guest has to be told the next hop is on-link. True for v4,
    /// where `169.254.1.1` is in none of its own subnets; false for v6, where a
    /// link-local address is on-link by construction.
    pub on_link: bool,
}

/// The configuration one routed address implies.
pub fn guest_route(address: IpAddr) -> GuestRoute {
    match address {
        IpAddr::V4(_) => GuestRoute {
            address,
            prefix_len: 32,
            via: IpAddr::V4(NEXT_HOP_V4),
            on_link: true,
        },
        IpAddr::V6(_) => GuestRoute {
            address,
            prefix_len: 128,
            via: IpAddr::V6(NEXT_HOP_V6),
            on_link: false,
        },
    }
}

/// Whether a guest holding these addresses should default out through a public
/// one rather than through its tenant gateway.
///
/// It should, whenever it has one. Leaving the default on the tenant gateway
/// sends replies from the public address out of a door they cannot return
/// through — the asymmetric-routing bug that takes a day to find and looks like
/// a firewall problem the whole time.
pub fn defaults_through_public(routed: &[GuestRoute]) -> Option<&GuestRoute> {
    // The first, and in the order the addresses were given. A guest with two
    // public addresses has one default route; which of them is arbitrary, and
    // an arbitrary answer stated once is better than one that changes between
    // boots because a map iterated differently.
    routed.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed() -> AddressView {
        AddressView {
            name: "projects/p1/floatingips/f1".into(),
            address: Some("203.0.113.7".parse().unwrap()),
            subnet: "projects/p1/subnets/public".into(),
            subnet_is_external: true,
            delivery: Delivery::Routed,
            announce: None,
            port: "projects/p1/ports/web-1-eth0".into(),
        }
    }

    /// The refusal the whole external-network idea exists for: a routed address
    /// hands the guest a real address, and a tenant prefix is not one.
    #[test]
    fn a_routed_address_from_a_tenant_range_is_refused_in_words() {
        let mut inside = routed();
        inside.subnet_is_external = false;
        let Err(why @ Refusal::NotExternal { .. }) =
            may_publish(&inside, Announce::FromHost, 0)
        else {
            panic!("a routed address was taken from a tenant range");
        };
        assert!(why.to_string().contains("real nowhere"), "{why}");

        // Translated is a different claim: the guest never sees the address, so
        // where it comes from is the edge's business.
        inside.delivery = Delivery::Nat;
        assert_eq!(may_publish(&inside, Announce::FromHost, 0), Ok(()));
    }

    /// Announcing from a gateway in a cell with none is refused, and the
    /// refusal names both ways out.
    #[test]
    fn a_gateway_announcement_with_no_gateway_says_what_to_do_instead() {
        let mut address = routed();
        address.announce = Some(Announce::FromGateway);
        let Err(why @ Refusal::NoGateway { .. }) = may_publish(&address, Announce::FromHost, 0)
        else {
            panic!("an address was published from a gateway that does not exist");
        };
        assert!(why.to_string().contains("Mark one"), "{why}");
        assert!(why.to_string().contains("peer with the network above"), "{why}");

        assert_eq!(may_publish(&address, Announce::FromHost, 1), Ok(()));
    }

    /// An address held and pointing at nothing is the reason the object exists;
    /// a cell with no gateway may still hold one.
    #[test]
    fn an_address_pointing_at_nothing_is_never_refused() {
        let mut held = routed();
        held.port = String::new();
        held.announce = Some(Announce::FromGateway);
        assert_eq!(may_publish(&held, Announce::FromGateway, 0), Ok(()));
    }

    /// The network says what the cell does; an address may say otherwise. Two
    /// different questions, and this is the one that proves they are.
    #[test]
    fn an_address_may_disagree_with_its_networks_default() {
        let mut address = routed();
        let gateways = vec!["node-gw".to_string()];

        // Silent about it: the network decides.
        assert_eq!(
            announcer(&address, Announce::FromGateway, Some("node-a"), &gateways),
            Announcer::Gateways(gateways.clone())
        );
        assert_eq!(
            announcer(&address, Announce::FromHost, Some("node-a"), &gateways),
            Announcer::Host("node-a".into())
        );

        // And when it does say: it wins.
        address.announce = Some(Announce::FromHost);
        assert_eq!(
            announcer(&address, Announce::FromGateway, Some("node-a"), &gateways),
            Announcer::Host("node-a".into())
        );
    }

    /// The route follows the guest, and it follows where the guest *is* rather
    /// than where it was assigned — announcing from an assignment is how a
    /// migration's last moments become a black hole.
    #[test]
    fn nothing_announces_an_address_whose_guest_is_nowhere() {
        let address = routed();
        let Announcer::Nowhere(why) = announcer(&address, Announce::FromHost, None, &[]) else {
            panic!("an address was announced from a machine that is not holding the guest");
        };
        assert!(matches!(why, Silent::NotPlaced { .. }));
        assert!(why.to_string().contains("has not been placed"), "{why}");
    }

    /// A translated address has no route of its own to announce, and saying so
    /// is better than reporting an announcement nobody made.
    #[test]
    fn a_translated_address_is_not_announced_at_all() {
        let mut nat = routed();
        nat.delivery = Delivery::Nat;
        assert_eq!(
            announcer(&nat, Announce::FromHost, Some("node-a"), &[]),
            Announcer::Nowhere(Silent::Translated)
        );
    }

    /// The guest gets a host route and an on-link next hop that is in no
    /// subnet — which is what makes the address independent of any L2 segment
    /// and correct on every machine in the cell.
    #[test]
    fn a_guest_is_given_a_host_route_through_a_next_hop_in_no_subnet() {
        let v4 = guest_route("203.0.113.7".parse().unwrap());
        assert_eq!(v4.prefix_len, 32);
        assert_eq!(v4.via, IpAddr::V4(NEXT_HOP_V4));
        assert!(v4.on_link, "a v4 next hop outside every subnet needs saying so");

        let v6 = guest_route("2001:db8::7".parse().unwrap());
        assert_eq!(v6.prefix_len, 128);
        assert_eq!(v6.via, IpAddr::V6(NEXT_HOP_V6));
        assert!(!v6.on_link, "a link-local next hop is on-link by construction");
    }

    /// A guest holding a public address defaults out through it. The other way
    /// round is the asymmetric-routing bug that looks like a firewall problem
    /// for a day.
    #[test]
    fn a_guest_with_a_public_address_defaults_out_through_it() {
        let routes = vec![
            guest_route("203.0.113.7".parse().unwrap()),
            guest_route("203.0.113.8".parse().unwrap()),
        ];
        let default = defaults_through_public(&routes).expect("it has one");
        assert_eq!(default.address, "203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(defaults_through_public(&[]), None);
    }
}
