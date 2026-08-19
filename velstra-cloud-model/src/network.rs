//! Addresses, as arithmetic rather than as strings.
//!
//! A subnet declares a range, a gateway and some reserved addresses; a port
//! carries one address out of that range. Three separate pieces of the platform
//! need to do sums on those: the controller that picks an address, the metadata
//! service that tells a guest what its address means, and the DHCP responder
//! that hands it over. Doing those sums in three places is how a guest ends up
//! with a netmask that disagrees with the subnet it is on, so they are done
//! once, here, as pure functions with no store and no I/O anywhere near them.
//!
//! The one design point worth stating: **there is no allocation table.** A free
//! address is computed from the addresses that are currently taken, which are
//! read off the ports themselves. A table would be a second record of the same
//! fact, and the two would eventually disagree — at which point two guests have
//! one address and neither the table nor the ports can say which is right.

use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CidrError {
    #[error("a CIDR is address/prefix")]
    NotCidr,
    #[error("not an address")]
    BadAddress,
    #[error("a prefix length is 0..={max} for this family")]
    BadPrefix { max: u8 },
}

/// An address together with the prefix length it was given with.
///
/// Deliberately *not* normalised to the network address on parse: a port's
/// `10.0.0.5/24` says both which address the guest has and how much of the
/// world is on its own link, and losing either half means asking somewhere else
/// for it. [`Cidr::network`] normalises when normalisation is what is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cidr {
    pub address: IpAddr,
    pub prefix_len: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, CidrError> {
        let (address, prefix) = s.split_once('/').ok_or(CidrError::NotCidr)?;
        let address: IpAddr = address.trim().parse().map_err(|_| CidrError::BadAddress)?;
        let prefix_len: u8 = prefix.trim().parse().map_err(|_| CidrError::NotCidr)?;
        let max = Self::max_prefix(&address);
        if prefix_len > max {
            return Err(CidrError::BadPrefix { max });
        }
        Ok(Self {
            address,
            prefix_len,
        })
    }

    fn max_prefix(address: &IpAddr) -> u8 {
        match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        }
    }

    /// The address with the host bits cleared.
    pub fn network(&self) -> IpAddr {
        match self.address {
            IpAddr::V4(a) => IpAddr::V4(Ipv4Addr::from(u32::from(a) & mask32(self.prefix_len))),
            IpAddr::V6(a) => IpAddr::V6(Ipv6Addr::from(u128::from(a) & mask128(self.prefix_len))),
        }
    }

    /// The all-ones address of an IPv4 range. IPv6 has no broadcast, and
    /// answering `None` rather than inventing one is what keeps the caller
    /// honest about which family it is holding.
    pub fn broadcast(&self) -> Option<Ipv4Addr> {
        match self.address {
            IpAddr::V4(a) => Some(Ipv4Addr::from(u32::from(a) | !mask32(self.prefix_len))),
            IpAddr::V6(_) => None,
        }
    }

    /// The dotted-quad netmask a guest is configured with. IPv6 has none —
    /// there the prefix length is the whole of it.
    pub fn netmask(&self) -> Option<Ipv4Addr> {
        match self.address {
            IpAddr::V4(_) => Some(Ipv4Addr::from(mask32(self.prefix_len))),
            IpAddr::V6(_) => None,
        }
    }

    pub fn contains(&self, other: IpAddr) -> bool {
        match (self.address, other) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                u32::from(a) & mask32(self.prefix_len) == u32::from(b) & mask32(self.prefix_len)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                u128::from(a) & mask128(self.prefix_len) == u128::from(b) & mask128(self.prefix_len)
            }
            // Two families are never on one another's link, whatever the bits
            // happen to say.
            _ => false,
        }
    }

    /// How many addresses a guest could be given here, saturating at `u64::MAX`
    /// because an IPv6 subnet's size is not a number anybody needs exactly.
    pub fn usable(&self) -> u64 {
        match self.address {
            IpAddr::V4(_) => match self.prefix_len {
                // A /31 is a point-to-point link (RFC 3021) and a /32 is one
                // address; neither has a host range this platform hands out of.
                31 | 32 => 0,
                p => (1u64 << (32 - p)).saturating_sub(2),
            },
            IpAddr::V6(_) => match self.prefix_len {
                128 => 0,
                // Anything at /64 or shorter holds more addresses than a u64
                // can count; the exact figure is of no use to anybody.
                p if p >= 65 => (1u64 << (128 - p)).saturating_sub(1),
                _ => u64::MAX,
            },
        }
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

impl std::str::FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn mask32(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    }
}

fn mask128(prefix_len: u8) -> u128 {
    if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - prefix_len)
    }
}

/// The lowest address in `cidr` that nothing has taken.
///
/// `taken` is everything that must not be handed out — the gateway, whatever
/// the subnet reserves, and every address already on a port. The scan starts at
/// the bottom of the range and stops at the first gap, so it visits at most
/// `taken.len() + 1` candidates however large the range is; an IPv6 /64 costs
/// exactly as much as a /24.
///
/// Returns `None` when the range is full, which is a thing an operator has to
/// be told rather than a thing to work around.
pub fn allocate(cidr: &Cidr, taken: &BTreeSet<IpAddr>) -> Option<IpAddr> {
    let usable = cidr.usable();
    if usable == 0 {
        return None;
    }
    // At most one more candidate than there are addresses in the way: the
    // taken ones cannot block more than their own number of slots.
    let limit = (taken.len() as u64).saturating_add(1).min(usable);
    match cidr.network() {
        IpAddr::V4(network) => {
            let base = u32::from(network);
            (1..=limit)
                .map(|offset| IpAddr::V4(Ipv4Addr::from(base + offset as u32)))
                .find(|candidate| !taken.contains(candidate))
        }
        IpAddr::V6(network) => {
            // Offset zero is the subnet-router anycast address (RFC 4291), so
            // the host range starts one above it, exactly as in IPv4.
            let base = u128::from(network);
            (1..=limit)
                .map(|offset| IpAddr::V6(Ipv6Addr::from(base + offset as u128)))
                .find(|candidate| !taken.contains(candidate))
        }
    }
}

/// A stable MAC address for a port, derived from something that never changes
/// about it.
///
/// Derived rather than random for one reason: a MAC that is drawn fresh is a
/// MAC that changes when a write is lost and retried, and a guest whose NIC
/// changes identity underneath it loses its address, its ARP neighbours and its
/// DHCP lease at once. Feed this the port's `uid` and the same port has the
/// same MAC forever, on any machine that computes it.
///
/// The first byte is forced to locally-administered unicast (`x2`), which is
/// the range set aside for exactly this — addresses nobody bought from the
/// IEEE and that therefore cannot collide with a real card.
pub fn mac_for(seed: &str) -> String {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes());
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&digest[..6]);
    mac[0] = (mac[0] & 0b1111_1100) | 0b0000_0010;
    format_mac(&mac)
}

/// `52:54:00:12:34:56` to bytes. Accepts colons or dashes, upper or lower case,
/// and nothing else — a MAC that has to be guessed at is one that will be
/// guessed differently by the next reader.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split([':', '-']);
    for slot in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(mac)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_cidr_keeps_the_address_it_was_given_and_can_still_say_its_network() {
        // Both halves matter on a port: which address the guest has, and how
        // much of the world is on its own link.
        let c = Cidr::parse("10.20.0.10/24").unwrap();
        assert_eq!(c.address, ip("10.20.0.10"));
        assert_eq!(c.network(), ip("10.20.0.0"));
        assert_eq!(
            c.netmask().unwrap(),
            "255.255.255.0".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(
            c.broadcast().unwrap(),
            "10.20.0.255".parse::<Ipv4Addr>().unwrap()
        );
    }

    #[test]
    fn a_prefix_that_cannot_mean_anything_is_refused() {
        assert_eq!(Cidr::parse("10.0.0.0"), Err(CidrError::NotCidr));
        assert_eq!(
            Cidr::parse("10.0.0.0/33"),
            Err(CidrError::BadPrefix { max: 32 })
        );
        assert_eq!(
            Cidr::parse("fd00::/129"),
            Err(CidrError::BadPrefix { max: 128 })
        );
        assert_eq!(Cidr::parse("nonsense/24"), Err(CidrError::BadAddress));
        // A v6 prefix on a v4 address would be accepted by a parser that only
        // checked the number against 128.
        assert!(Cidr::parse("10.0.0.0/64").is_err());
    }

    #[test]
    fn containment_never_crosses_families() {
        let v4 = Cidr::parse("10.20.0.0/24").unwrap();
        assert!(v4.contains(ip("10.20.0.7")));
        assert!(!v4.contains(ip("10.20.1.7")));
        assert!(!v4.contains(ip("::ffff:10.20.0.7")));
        let v6 = Cidr::parse("fd00:1::/64").unwrap();
        assert!(v6.contains(ip("fd00:1::7")));
        assert!(!v6.contains(ip("fd00:2::7")));
    }

    #[test]
    fn allocation_takes_the_lowest_address_nothing_is_using() {
        let subnet = Cidr::parse("10.20.0.0/24").unwrap();
        let taken = BTreeSet::from([ip("10.20.0.1"), ip("10.20.0.2")]);
        assert_eq!(allocate(&subnet, &taken), Some(ip("10.20.0.3")));

        // …and fills a gap left by something that went away, rather than
        // marching up the range forever.
        let taken = BTreeSet::from([ip("10.20.0.1"), ip("10.20.0.3")]);
        assert_eq!(allocate(&subnet, &taken), Some(ip("10.20.0.2")));
    }

    #[test]
    fn allocation_never_hands_out_the_network_or_the_broadcast_address() {
        // The bottom of the range is skipped by construction; the top is the
        // one the loop has to be stopped before.
        let subnet = Cidr::parse("10.20.0.0/30").unwrap();
        let taken = BTreeSet::from([ip("10.20.0.1")]);
        assert_eq!(allocate(&subnet, &taken), Some(ip("10.20.0.2")));
        let full = BTreeSet::from([ip("10.20.0.1"), ip("10.20.0.2")]);
        assert_eq!(
            allocate(&subnet, &full),
            None,
            "the broadcast address was handed out"
        );
    }

    #[test]
    fn a_range_with_no_host_addresses_allocates_nothing() {
        for range in ["10.20.0.0/31", "10.20.0.1/32", "fd00::1/128"] {
            let cidr = Cidr::parse(range).unwrap();
            assert_eq!(allocate(&cidr, &BTreeSet::new()), None, "{range}");
        }
    }

    #[test]
    fn a_v6_subnet_costs_no_more_to_allocate_from_than_a_small_v4_one() {
        // The property, not the value: the scan is bounded by how many
        // addresses are in the way, so a /64 does not iterate 2^64 times.
        let subnet = Cidr::parse("fd00:1::/64").unwrap();
        let taken = BTreeSet::from([ip("fd00:1::1")]);
        assert_eq!(allocate(&subnet, &taken), Some(ip("fd00:1::2")));
    }

    #[test]
    fn a_derived_mac_is_stable_locally_administered_and_unicast() {
        let a = mac_for("uid-1");
        assert_eq!(a, mac_for("uid-1"), "the same port got two different MACs");
        assert_ne!(a, mac_for("uid-2"));
        let bytes = parse_mac(&a).unwrap();
        assert_eq!(
            bytes[0] & 0b11,
            0b10,
            "not a locally-administered unicast address"
        );
    }

    #[test]
    fn a_mac_round_trips_and_a_malformed_one_is_not_guessed_at() {
        assert_eq!(
            parse_mac("52:54:00:12:34:56"),
            Some([0x52, 0x54, 0, 0x12, 0x34, 0x56])
        );
        assert_eq!(
            parse_mac("52-54-00-12-34-56"),
            Some([0x52, 0x54, 0, 0x12, 0x34, 0x56])
        );
        assert_eq!(
            format_mac(&[0x52, 0x54, 0, 0x12, 0x34, 0x56]),
            "52:54:00:12:34:56"
        );
        assert_eq!(parse_mac("52:54:00:12:34"), None);
        assert_eq!(parse_mac("52:54:00:12:34:56:78"), None);
        assert_eq!(parse_mac("52:54:00:12:34:5"), None);
        assert_eq!(parse_mac("gg:54:00:12:34:56"), None);
    }
}
