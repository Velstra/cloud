//! What this node knows about the guests on it, and how it recognises one.
//!
//! Two services answer guests on a hypervisor — the metadata service on
//! `169.254.169.254` and the DHCP responder on each tap — and they answer the
//! same questions: who is this, what address does it have, which gateway and
//! resolvers go with it. Deriving that twice is how a guest ends up leased an
//! address the metadata service does not think it has, so it is derived **once**
//! here, from the objects this node holds, and both services read the result.
//!
//! ## How a guest is recognised
//!
//! Never by anything it says about itself. A request carries no token, no
//! header and no name this node trusts, because a guest can put anything in
//! those and a neighbour can copy them. What it carries instead is where it
//! came from, and this node assigned that:
//!
//! * **The metadata service** knows a guest by the **source address** of the
//!   TCP connection — an address this node programmed onto a port.
//! * **The DHCP responder** knows a guest by the **tap it arrived on together
//!   with the MAC in the packet**. A guest can forge the MAC; it cannot forge
//!   which tap the frame came out of, because the tap is the wire this node
//!   plugged it into. Both must match, so a guest that spoofs its neighbour's
//!   MAC finds no binding at all rather than its neighbour's address.
//!
//! ## Why this is rebuilt rather than edited
//!
//! Every pass replaces the whole thing. An entry that outlived the guest it
//! describes is not a stale cache — an address gets re-used, and the next
//! tenant of it would be handed the previous one's SSH keys.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, RwLock},
};

use velstra_cloud_model::{
    network::{Cidr, parse_mac},
    resources::{Instance, Network, Port, Subnet},
};

/// Everything this node may tell one guest about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuestView {
    /// The instance resource name.
    pub instance_id: String,
    pub hostname: String,
    pub ssh_keys: Vec<String>,
    pub user_data: Option<String>,
    /// In the order the instance declares its ports, which is the order the
    /// guest sees its NICs.
    pub interfaces: Vec<Interface>,
}

/// One NIC, as the platform knows it — never as the guest reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    /// The port resource name.
    pub port: String,
    /// The subnet resource name. Not for the guest — for the node, which cannot
    /// otherwise tell two of its guests being on one segment from being on two.
    pub subnet: String,
    pub mac: Option<[u8; 6]>,
    /// The address and how much of the world is on this link. The prefix comes
    /// from the subnet, which is where the range is declared; a port carrying
    /// one of its own is trusted only when the subnet cannot be read.
    pub cidr: Option<Cidr>,
    pub gateway: Option<IpAddr>,
    pub dns: Vec<IpAddr>,
    /// From the network, because an overlay's MTU is a property of the
    /// encapsulation and not of any one guest. A guest that does not learn it
    /// blackholes every large packet it sends, with nothing to see anywhere.
    pub mtu: Option<u32>,
    /// The host device this NIC is on, when it is programmed on this node.
    pub tap: Option<String>,
    /// Whether this NIC is on the machine's own wire rather than one this
    /// platform builds.
    ///
    /// What it changes is what this node **does not do**: it does not answer
    /// DHCP for the guest, because whatever serves that wire already does, and
    /// two servers on one segment is how a guest gets an address nobody agrees
    /// on. The address on the Port is then the operator's note about where the
    /// guest is expected to be, not an allocation this platform made.
    pub on_host_bridge: bool,
    /// Public addresses this port holds, in the order they were declared.
    ///
    /// Held by the **guest**, which is the whole difference between a routed
    /// address and a translated one: there is nothing at the edge rewriting a
    /// packet, so the guest has to configure the address or it answers to
    /// nothing. A guest that holds one also defaults out through it — see
    /// [`velstra_cloud_model::public`].
    pub public: Vec<velstra_cloud_model::public::GuestRoute>,
}

impl Interface {
    pub fn address(&self) -> Option<IpAddr> {
        self.cidr.map(|c| c.address)
    }

    /// The IPv4 view of this NIC: address, mask, gateway. `None` for a
    /// v6-only NIC, which DHCPv4 has nothing to say to.
    pub fn v4(&self) -> Option<(Ipv4Addr, Ipv4Addr, Option<Ipv4Addr>)> {
        let cidr = self.cidr?;
        let IpAddr::V4(address) = cidr.address else {
            return None;
        };
        let mask = cidr.netmask()?;
        let gateway = match self.gateway {
            Some(IpAddr::V4(g)) => Some(g),
            _ => None,
        };
        Some((address, mask, gateway))
    }

    pub fn v4_dns(&self) -> Vec<Ipv4Addr> {
        self.dns
            .iter()
            .filter_map(|d| match d {
                IpAddr::V4(a) => Some(*a),
                IpAddr::V6(_) => None,
            })
            .collect()
    }
}

/// Which guest is which, on this node.
///
/// Cheap to clone and shared by every service that answers a guest, so there is
/// one map rather than several that can disagree.
#[derive(Clone, Default)]
pub struct GuestRegistry {
    inner: Arc<RwLock<Index>>,
}

/// A guest together with which of its NICs was asked through.
pub type Seen = (Arc<GuestView>, usize);

#[derive(Default)]
struct Index {
    by_address: BTreeMap<IpAddr, Seen>,
    by_wire: BTreeMap<(String, [u8; 6]), Seen>,
    taps: BTreeSet<String>,
}

impl GuestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything this node believes about its guests.
    ///
    /// Ambiguity is resolved by refusing to answer rather than by picking. Two
    /// guests on one address is a datapath fault, and whichever one the loop
    /// happened to visit second would otherwise inherit the other's identity —
    /// its keys, its user-data, its name.
    pub fn replace(&self, views: Vec<GuestView>) {
        let mut index = Index::default();
        let mut ambiguous_addresses = BTreeSet::new();
        let mut ambiguous_wires = BTreeSet::new();

        for view in views {
            let view = Arc::new(view);
            for (n, interface) in view.interfaces.iter().enumerate() {
                if let Some(address) = interface.address() {
                    match index.by_address.get(&address) {
                        Some((other, _)) if other.instance_id != view.instance_id => {
                            tracing::error!(
                                %address, first = %other.instance_id, second = %view.instance_id,
                                "two instances claim one address; answering for neither"
                            );
                            ambiguous_addresses.insert(address);
                        }
                        _ => {
                            index.by_address.insert(address, (view.clone(), n));
                        }
                    }
                }
                // A guest on the machine's own wire is not this node's to lease
                // to. Whatever serves that wire already answers, and a second
                // server on one segment is how a guest ends up with an address
                // nobody agrees on — including the one the Port says it has.
                //
                // Left out of the *wire* index only: the metadata service still
                // answers it, because a guest on a host bridge reaches
                // 169.254.169.254 on this machine like any other neighbour, and
                // that is where its keys and its hostname come from.
                if interface.on_host_bridge {
                    continue;
                }
                let (Some(tap), Some(mac)) = (interface.tap.clone(), interface.mac) else {
                    continue;
                };
                index.taps.insert(tap.clone());
                let wire = (tap, mac);
                match index.by_wire.get(&wire) {
                    Some((other, _)) if other.instance_id != view.instance_id => {
                        tracing::error!(
                            tap = %wire.0, first = %other.instance_id, second = %view.instance_id,
                            "two instances claim one MAC on one tap; leasing to neither"
                        );
                        ambiguous_wires.insert(wire);
                    }
                    _ => {
                        index.by_wire.insert(wire, (view.clone(), n));
                    }
                }
            }
        }

        for address in ambiguous_addresses {
            index.by_address.remove(&address);
        }
        for wire in ambiguous_wires {
            index.by_wire.remove(&wire);
        }
        *self.inner.write().unwrap() = index;
    }

    /// Who is at this address — the metadata service's whole notion of
    /// identity.
    pub fn at_address(&self, address: IpAddr) -> Option<Seen> {
        self.inner.read().unwrap().by_address.get(&address).cloned()
    }

    /// Who is this MAC, **on this tap** — the DHCP responder's whole notion of
    /// identity. Both halves are required: see the module docs.
    pub fn on_wire(&self, tap: &str, mac: [u8; 6]) -> Option<Seen> {
        self.inner
            .read()
            .unwrap()
            .by_wire
            .get(&(tap.to_string(), mac))
            .cloned()
    }

    /// The taps that carry a guest this node can answer for. What the DHCP
    /// responder listens on, and nothing else — a tap with no binding has
    /// nothing this node could say to it.
    pub fn taps(&self) -> BTreeSet<String> {
        self.inner.read().unwrap().taps.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().by_address.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Build the view of every guest on this node, from the objects and the
/// datapath.
///
/// Pure, and takes maps rather than a store, so the question "what would a
/// guest be told" is answerable in a unit test without a cell.
///
/// `taps` is port name to host device, exactly as [`crate::host::Datapath`]
/// reports it. An instance that is being deleted is left out: it is on its way
/// off this machine, and the last thing it should be told is how to keep
/// itself on the network.
pub fn derive(
    mine: &[&Instance],
    ports: &BTreeMap<String, Port>,
    subnets: &BTreeMap<String, Subnet>,
    networks: &BTreeMap<String, Network>,
    taps: &BTreeMap<String, String>,
    // Routed public addresses, by the port that holds them. Empty in a cell
    // that hands out none, which is most of them.
    public: &BTreeMap<String, Vec<velstra_cloud_model::public::GuestRoute>>,
) -> Vec<GuestView> {
    mine.iter()
        .filter(|i| !i.meta.is_deleting())
        .map(|instance| GuestView {
            instance_id: instance.meta.name.to_string(),
            hostname: instance.meta.name.id().to_string(),
            ssh_keys: instance.spec.ssh_keys.clone(),
            user_data: instance.spec.user_data.clone(),
            interfaces: instance
                .spec
                .ports
                .iter()
                .filter_map(|name| ports.get(name))
                .map(|port| interface(port, subnets, networks, taps, public))
                .collect(),
        })
        .collect()
}

fn interface(
    port: &Port,
    subnets: &BTreeMap<String, Subnet>,
    networks: &BTreeMap<String, Network>,
    taps: &BTreeMap<String, String>,
    public: &BTreeMap<String, Vec<velstra_cloud_model::public::GuestRoute>>,
) -> Interface {
    let name = port.meta.name.to_string();
    let subnet = subnets.get(&port.spec.subnet);
    let declared = subnet.and_then(|s| Cidr::parse(&s.spec.cidr).ok());
    let cidr = port
        .spec
        .address
        .as_deref()
        .and_then(|address| address_in(address, declared));
    Interface {
        subnet: port.spec.subnet.clone(),
        on_host_bridge: networks
            .get(&port.spec.network)
            .is_some_and(|n| !n.spec.host_bridge.is_empty()),
        mac: port.spec.mac.as_deref().and_then(parse_mac),
        cidr,
        gateway: subnet.and_then(|s| s.spec.gateway.parse().ok()),
        dns: subnet
            .map(|s| s.spec.dns.iter().filter_map(|d| d.parse().ok()).collect())
            .unwrap_or_default(),
        mtu: networks
            .get(&port.spec.network)
            .map(|n| n.spec.mtu)
            .filter(|mtu| *mtu > 0),
        tap: taps.get(&name).cloned(),
        public: public.get(&name).cloned().unwrap_or_default(),
        port: name,
    }
}

/// Which routed public addresses each port holds.
///
/// Only the routed ones: a translated address is answered for at the edge and
/// the guest must **not** configure it — a guest that put a NAT address on its
/// own interface would answer ARP for something the edge is also answering for,
/// and the two would take turns.
pub fn public_addresses(
    floating: &[velstra_cloud_model::resources::FloatingIp],
) -> BTreeMap<String, Vec<velstra_cloud_model::public::GuestRoute>> {
    let mut out: BTreeMap<String, Vec<velstra_cloud_model::public::GuestRoute>> = BTreeMap::new();
    for fip in floating {
        if fip.spec.delivery != velstra_cloud_model::public::Delivery::Routed {
            continue;
        }
        if fip.spec.port.is_empty() {
            continue;
        }
        let Some(address) = fip.spec.address.as_deref().and_then(|a| a.parse().ok()) else {
            continue;
        };
        out.entry(fip.spec.port.clone())
            .or_default()
            .push(velstra_cloud_model::public::guest_route(address));
    }
    out
}

/// A port's address, with the prefix length of the subnet it is on.
///
/// The subnet is authoritative about how large the link is; the port's own
/// prefix, when it carries one, is only a fallback for a port whose subnet
/// object has not reached this node yet. An address outside the subnet it
/// claims to be on is refused rather than served with the wrong mask — a guest
/// configured that way cannot reach its own gateway, and nothing says why.
fn address_in(address: &str, subnet: Option<Cidr>) -> Option<Cidr> {
    let own = Cidr::parse(address).ok();
    let bare: Option<IpAddr> = address.split('/').next().and_then(|a| a.parse().ok());
    let address = own.map(|c| c.address).or(bare)?;
    match subnet {
        Some(subnet) if subnet.contains(address) => Some(Cidr {
            address,
            prefix_len: subnet.prefix_len,
        }),
        Some(subnet) => {
            tracing::error!(
                %address, %subnet,
                "a port's address is not in the subnet it names; not serving it"
            );
            None
        }
        None => own,
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{
            InstanceSpec, InstanceStatus, NetworkSpec, NetworkStatus, PortSpec, PortStatus,
            Resource, SubnetSpec, SubnetStatus,
        },
    };

    use super::*;

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    fn network() -> BTreeMap<String, Network> {
        BTreeMap::from([(
            "projects/p1/networks/net-a".to_string(),
            Resource::new(
                meta("projects/p1/networks/net-a"),
                NetworkSpec {
                    host_bridge: String::new(),
                    vni: 4711,
                    mtu: 1450,
                    external: false,
                    announce: Default::default(),
                },
                NetworkStatus::default(),
            ),
        )])
    }

    fn subnet() -> BTreeMap<String, Subnet> {
        BTreeMap::from([(
            "projects/p1/subnets/sub-a".to_string(),
            Resource::new(
                meta("projects/p1/subnets/sub-a"),
                SubnetSpec {
                    network: "projects/p1/networks/net-a".into(),
                    cidr: "10.20.0.0/24".into(),
                    gateway: "10.20.0.1".into(),
                    dns: vec!["10.20.0.1".into(), "fd00:1::1".into()],
                    reserved: vec!["10.20.0.1".into()],
                },
                SubnetStatus::default(),
            ),
        )])
    }

    fn port(name: &str, address: Option<&str>, mac: Option<&str>) -> Port {
        Resource::new(
            meta(name),
            PortSpec {
                network: "projects/p1/networks/net-a".into(),
                subnet: "projects/p1/subnets/sub-a".into(),
                address: address.map(str::to_string),
                mac: mac.map(str::to_string),
                ..Default::default()
            },
            PortStatus::default(),
        )
    }

    fn instance(name: &str, ports: &[&str]) -> Instance {
        Resource::new(
            meta(name),
            InstanceSpec {
                ports: ports.iter().map(|p| p.to_string()).collect(),
                ssh_keys: vec!["ssh-ed25519 AAAA".into()],
                user_data: Some("#cloud-config\n".into()),
                ..Default::default()
            },
            InstanceStatus::default(),
        )
    }

    fn one_guest() -> Vec<GuestView> {
        let ports = BTreeMap::from([(
            "projects/p1/ports/port-a".to_string(),
            port(
                "projects/p1/ports/port-a",
                Some("10.20.0.10"),
                Some("52:54:00:12:34:56"),
            ),
        )]);
        let taps = BTreeMap::from([(
            "projects/p1/ports/port-a".to_string(),
            "vt-port-a".to_string(),
        )]);
        let i = instance("projects/p1/instances/i1", &["projects/p1/ports/port-a"]);
        derive(
            &[&i],
            &ports,
            &subnet(),
            &network(),
            &taps,
            &Default::default(),
        )
    }

    #[test]
    fn a_guest_learns_its_address_mask_gateway_resolvers_and_mtu() {
        // Everything a guest needs to be on the network, and every piece of it
        // from the object that owns that piece: the port has the address, the
        // subnet has the shape of the link, the network has the MTU.
        let views = one_guest();
        let nic = &views[0].interfaces[0];
        assert_eq!(nic.cidr.unwrap().to_string(), "10.20.0.10/24");
        assert_eq!(nic.gateway.unwrap().to_string(), "10.20.0.1");
        assert_eq!(nic.dns.len(), 2);
        assert_eq!(nic.mtu, Some(1450));
        assert_eq!(nic.tap.as_deref(), Some("vt-port-a"));
        assert_eq!(nic.v4().unwrap().1.to_string(), "255.255.255.0");
        assert_eq!(nic.v4_dns(), vec!["10.20.0.1".parse::<Ipv4Addr>().unwrap()]);
    }

    #[test]
    fn a_port_whose_address_is_not_in_its_subnet_is_not_served() {
        // Served with the subnet's mask it would be a guest that cannot reach
        // its own gateway; served with a mask of its own it would be a guest
        // that believes it is on a link it is not on. Neither is better than
        // saying nothing.
        let ports = BTreeMap::from([(
            "projects/p1/ports/port-a".to_string(),
            port(
                "projects/p1/ports/port-a",
                Some("10.99.0.10"),
                Some("52:54:00:12:34:56"),
            ),
        )]);
        let i = instance("projects/p1/instances/i1", &["projects/p1/ports/port-a"]);
        let views = derive(
            &[&i],
            &ports,
            &subnet(),
            &network(),
            &BTreeMap::new(),
            &Default::default(),
        );
        assert_eq!(views[0].interfaces[0].cidr, None);
    }

    #[test]
    fn an_instance_on_its_way_out_is_not_answered_for() {
        let ports = BTreeMap::from([(
            "projects/p1/ports/port-a".to_string(),
            port("projects/p1/ports/port-a", Some("10.20.0.10"), None),
        )]);
        let mut i = instance("projects/p1/instances/i1", &["projects/p1/ports/port-a"]);
        i.meta.deleted_at = Some(velstra_cloud_model::Timestamp::now());
        assert!(
            derive(
                &[&i],
                &ports,
                &subnet(),
                &network(),
                &BTreeMap::new(),
                &Default::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_guest_is_found_by_its_address_and_by_its_mac_on_its_own_tap() {
        let registry = GuestRegistry::new();
        registry.replace(one_guest());
        let mac = parse_mac("52:54:00:12:34:56").unwrap();

        let (view, n) = registry
            .at_address("10.20.0.10".parse().unwrap())
            .expect("the guest was not at its own address");
        assert_eq!(view.instance_id, "projects/p1/instances/i1");
        assert_eq!(n, 0);
        assert!(registry.on_wire("vt-port-a", mac).is_some());
        assert_eq!(registry.taps(), BTreeSet::from(["vt-port-a".to_string()]));
    }

    #[test]
    fn a_mac_on_a_tap_that_is_not_its_own_is_nobody() {
        // The spoofing case, and the reason the key is a pair. A guest can put
        // its neighbour's MAC in a packet; it cannot make the packet come out
        // of its neighbour's tap.
        let registry = GuestRegistry::new();
        registry.replace(one_guest());
        let mac = parse_mac("52:54:00:12:34:56").unwrap();
        assert!(registry.on_wire("vt-port-b", mac).is_none());
        assert!(registry.on_wire("vt-port-a", [0; 6]).is_none());
    }

    #[test]
    fn two_guests_on_one_address_are_answered_for_neither() {
        let registry = GuestRegistry::new();
        let shared = Interface {
            port: "projects/p1/ports/port-a".into(),
            cidr: Some(Cidr::parse("10.20.0.10/24").unwrap()),
            mac: parse_mac("52:54:00:12:34:56"),
            tap: Some("vt-shared".into()),
            ..Default::default()
        };
        registry.replace(vec![
            GuestView {
                instance_id: "projects/p1/instances/i1".into(),
                interfaces: vec![shared.clone()],
                ..Default::default()
            },
            GuestView {
                instance_id: "projects/p1/instances/i2".into(),
                interfaces: vec![shared],
                ..Default::default()
            },
        ]);
        assert!(registry.at_address("10.20.0.10".parse().unwrap()).is_none());
        assert!(
            registry
                .on_wire("vt-shared", parse_mac("52:54:00:12:34:56").unwrap())
                .is_none()
        );
    }

    #[test]
    fn replacing_forgets_a_guest_that_is_no_longer_here() {
        let registry = GuestRegistry::new();
        registry.replace(one_guest());
        assert!(!registry.is_empty());
        registry.replace(Vec::new());
        assert!(registry.at_address("10.20.0.10".parse().unwrap()).is_none());
        assert!(registry.taps().is_empty());
    }
}
