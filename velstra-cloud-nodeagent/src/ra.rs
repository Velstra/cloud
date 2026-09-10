//! Router advertisements: how a guest with no IPv4 address gets an IPv6 one.
//!
//! **Why this exists at all.** Every other route into a guest's configuration
//! goes through the metadata service, and that service listens on
//! `169.254.169.254` — an IPv4 address. A dual-stack guest is fine: it comes up
//! on v4, fetches its netplan, and finds its v6 address written into it. A
//! guest with *no* v4 address cannot reach the service that would tell it what
//! its address is. That is a circle, and the only thing that breaks it is the
//! one mechanism IPv6 has for configuring a host that knows nothing: a router
//! saying, unasked, "this is the prefix and I am the way out".
//!
//! **Where it can run.** On the machine that holds the gateway, and nowhere
//! else — an advertisement has to arrive on the guest's own link, from the
//! router's link-local address, with a hop limit of 255 that no forwarding
//! device could have produced. On a cell whose datapath is the local bridge
//! ([`crate::localnet`]) that machine is this node. On a cell whose datapath is
//! the fabric it is not: the gateway lives there, this node has only a tap, and
//! an advertisement from here would be a router claiming a link it does not
//! route. So this runs for the segments this node actually holds, and says
//! nothing about the others. The same boundary shared egress runs into.
//!
//! **What it advertises.** One prefix per segment, with the autonomous flag
//! set, so a guest builds its own address out of the prefix and its interface
//! identifier. Not the *particular* address the platform allocated — SLAAC
//! cannot be told one, and a guest that took a different address from the one
//! the port holds would be a guest the datapath drops. This is deliberately the
//! v6-only rescue path and not the ordinary one: the ordinary one is the
//! netplan, which states the exact address, and a guest that can read it
//! ignores what is advertised here in favour of it.
//!
//! Both flags off (`M` and `O`): there is no DHCPv6 server here, and a guest
//! that went looking for one on our say-so would wait for a timeout it could
//! have skipped.
//!
//! **What is tested and what is not.** The packet is checked byte by byte
//! against RFC 4861 — a test that only counted its length would pass on one
//! with the flags in the wrong octet, and the guest would silently not
//! configure itself. The socket and the loop are not: they need a link with a
//! guest on it and `CAP_NET_RAW`, which is the same boundary the QMP paths in
//! [`crate::qemu`] draw and it is drawn for the same reason.

use std::net::{Ipv6Addr, SocketAddrV6};

use crate::host::{HostError, Result};

/// ICMPv6 type numbers, from RFC 4861.
pub const ROUTER_SOLICITATION: u8 = 133;
pub const ROUTER_ADVERTISEMENT: u8 = 134;

/// The all-nodes multicast group an unsolicited advertisement goes to.
pub const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

/// How long a guest may believe this router is a router, in seconds.
///
/// Nine hundred, which is three times the interval below. The relationship is
/// the point: a guest must hear three advertisements' worth of silence before
/// it stops believing, so one lost packet is not a guest that loses its
/// default route.
pub const ROUTER_LIFETIME_S: u16 = 900;

/// How often an unsolicited advertisement goes out.
pub const EVERY_S: u64 = 300;

/// How long a prefix stays valid and preferred, in seconds.
///
/// Both generous and both refreshed on every advertisement. A short lifetime
/// buys nothing here — the platform knows exactly when a segment goes away,
/// and it says so by advertising a router lifetime of zero rather than by
/// letting an address quietly expire.
pub const PREFIX_VALID_S: u32 = 2_592_000;
pub const PREFIX_PREFERRED_S: u32 = 604_800;

/// One advertisement, ready for the wire.
///
/// The checksum is deliberately left at zero: the kernel computes it for a raw
/// `IPPROTO_ICMPV6` socket, and it is the one field this code must *not* fill
/// in — a wrong checksum on a packet the kernel then re-checksums is a packet
/// that arrives corrupt exactly once, on the first machine that does it
/// differently.
pub fn advertisement(prefix: Ipv6Addr, prefix_len: u8, lifetime_s: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(ROUTER_ADVERTISEMENT);
    out.push(0); // code
    out.extend_from_slice(&[0, 0]); // checksum, filled in by the kernel
    // Current hop limit: zero means "unspecified — use your own default",
    // which is the honest answer from a router that has no opinion.
    out.push(0);
    // Flags. M and O both clear: there is no DHCPv6 here, and a guest that
    // went looking for one on our say-so would wait for a timeout.
    out.push(0);
    out.extend_from_slice(&lifetime_s.to_be_bytes());
    // Reachable time and retransmit timer: zero for "unspecified", which is
    // what a router says when it is not tuning anybody's neighbour cache.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());

    // Prefix Information (RFC 4861 §4.6.2). Type 3, length 4 (in units of 8
    // bytes), on-link and autonomous both set: the prefix is on this link, and
    // a guest may build an address out of it.
    out.push(3);
    out.push(4);
    out.push(prefix_len);
    out.push(0b1100_0000);
    out.extend_from_slice(&PREFIX_VALID_S.to_be_bytes());
    out.extend_from_slice(&PREFIX_PREFERRED_S.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // reserved
    // The prefix itself, with the host bits cleared — an advertisement carrying
    // a *host* address as its prefix is one every guest on the link would build
    // the same address from.
    out.extend_from_slice(&masked(prefix, prefix_len).octets());
    out
}

/// A prefix with everything below its length cleared.
pub fn masked(address: Ipv6Addr, prefix_len: u8) -> Ipv6Addr {
    let bits = u128::from(address);
    let keep = if prefix_len >= 128 {
        u128::MAX
    } else {
        u128::MAX << (128 - prefix_len)
    };
    Ipv6Addr::from(bits & keep)
}

/// Whether a received ICMPv6 packet is a router solicitation.
///
/// The whole of the check: a solicitation is answered with the same
/// advertisement that goes out unsolicited, so nothing else about it matters.
/// Anything that is not one is somebody else's packet — this socket sees every
/// ICMPv6 frame on the link, including the neighbour discovery every guest does
/// with every other guest.
pub fn is_solicitation(frame: &[u8]) -> bool {
    frame.first() == Some(&ROUTER_SOLICITATION)
}

/// A raw ICMPv6 socket on one device, ready to advertise.
///
/// **Hop limit 255, both ways.** RFC 4861 requires it and the requirement is
/// the security property: a packet that arrives with 255 cannot have crossed a
/// router, so a neighbour on the link is the only thing that can have sent it.
/// Setting it on the way out is what makes our advertisements acceptable to a
/// guest that checks — and every implementation checks.
pub fn bind(device: &str) -> Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6)).map_err(|e| {
        HostError::failed(format!("an ICMPv6 socket: {e} — this needs CAP_NET_RAW"))
    })?;
    socket
        .bind_device(Some(device.as_bytes()))
        .map_err(|e| HostError::failed(format!("binding an ICMPv6 socket to {device}: {e}")))?;
    socket
        .set_multicast_hops_v6(255)
        .map_err(|e| HostError::failed(format!("the hop limit: {e}")))?;
    socket
        .set_unicast_hops_v6(255)
        .map_err(|e| HostError::failed(format!("the hop limit: {e}")))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| HostError::failed(format!("the socket mode: {e}")))?;
    // Through `UdpSocket` because tokio has no raw-socket type and a raw
    // socket is a datagram socket in every way this code uses it: one
    // `send_to`, one `recv_from`, no connection.
    tokio::net::UdpSocket::from_std(socket.into())
        .map_err(|e| HostError::failed(format!("the ICMPv6 socket could not be registered: {e}")))
}

/// Advertise on one device, for ever: unsolicited on a timer, and again
/// whenever a guest asks.
///
/// A guest that has just booted sends a solicitation rather than waiting up to
/// five minutes for the next unsolicited one, and answering it is the
/// difference between a machine that is on the network at boot and one that is
/// on the network eventually.
pub async fn serve(device: String, prefix: Ipv6Addr, prefix_len: u8) {
    let socket = match bind(&device) {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!(%device, error = %e, "no router advertisements on this segment");
            return;
        }
    };
    let to = SocketAddrV6::new(ALL_NODES, 0, 0, 0);
    let frame = advertisement(prefix, prefix_len, ROUTER_LIFETIME_S);
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(EVERY_S));
    let mut buffer = vec![0u8; 1500];
    tracing::info!(%device, prefix = %masked(prefix, prefix_len), prefix_len, "advertising a route");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = socket.send_to(&frame, to).await {
                    tracing::debug!(%device, error = %e, "an advertisement did not go out");
                }
            }
            received = socket.recv_from(&mut buffer) => {
                match received {
                    Ok((n, from)) if is_solicitation(&buffer[..n]) => {
                        tracing::debug!(%device, %from, "answering a solicitation");
                        if let Err(e) = socket.send_to(&frame, to).await {
                            tracing::debug!(%device, error = %e, "the answer did not go out");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%device, error = %e, "the ICMPv6 socket failed");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes, field by field, against RFC 4861 §4.2 and §4.6.2. A test
    /// that only checked the length would pass on a packet with the flags in
    /// the wrong byte, and a guest would silently not configure itself.
    #[test]
    fn an_advertisement_is_the_shape_the_rfc_describes() {
        let frame = advertisement("fd00:19:136::1".parse().unwrap(), 64, 900);
        assert_eq!(frame.len(), 16 + 32, "header plus one prefix option");

        assert_eq!(frame[0], ROUTER_ADVERTISEMENT);
        assert_eq!(frame[1], 0, "code");
        // The checksum stays zero: the kernel fills it in for a raw ICMPv6
        // socket, and a wrong one here would be re-checksummed over.
        assert_eq!(&frame[2..4], &[0, 0]);
        assert_eq!(frame[4], 0, "hop limit unspecified");
        assert_eq!(
            frame[5], 0,
            "neither managed nor other-config: no DHCPv6 here"
        );
        assert_eq!(&frame[6..8], &900u16.to_be_bytes());

        // The prefix option.
        assert_eq!(frame[16], 3, "prefix information");
        assert_eq!(frame[17], 4, "four units of eight bytes");
        assert_eq!(frame[18], 64, "prefix length");
        assert_eq!(frame[19], 0b1100_0000, "on-link and autonomous");
        assert_eq!(&frame[20..24], &PREFIX_VALID_S.to_be_bytes());
        assert_eq!(&frame[24..28], &PREFIX_PREFERRED_S.to_be_bytes());
        assert_eq!(&frame[28..32], &[0, 0, 0, 0], "reserved");
        assert_eq!(
            &frame[32..48],
            &"fd00:19:136::".parse::<Ipv6Addr>().unwrap().octets(),
            "the gateway's own address leaked into the prefix"
        );
    }

    /// A router lifetime of zero is how a segment is withdrawn: it says "I am
    /// not a router" while still carrying the prefix, so a guest drops the
    /// default route and keeps its address.
    #[test]
    fn a_lifetime_of_zero_withdraws_the_route_and_keeps_the_prefix() {
        let frame = advertisement("fd00:1::1".parse().unwrap(), 64, 0);
        assert_eq!(&frame[6..8], &[0, 0]);
        assert_eq!(frame[16], 3, "the prefix is still advertised");
    }

    /// The host bits go. An advertisement carrying a host address as its prefix
    /// is one every guest on the link builds the same address from.
    #[test]
    fn a_prefix_is_a_prefix_and_not_an_address() {
        assert_eq!(
            masked("fd00:19:136::1".parse().unwrap(), 64),
            "fd00:19:136::".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(
            masked("2001:db8:1:2:3:4:5:6".parse().unwrap(), 48),
            "2001:db8:1::".parse::<Ipv6Addr>().unwrap()
        );
        // And a /128 keeps everything, rather than shifting by 128 and
        // wrapping — which in release mode would have cleared the whole
        // address.
        let one = "fd00::5".parse::<Ipv6Addr>().unwrap();
        assert_eq!(masked(one, 128), one);
    }

    /// This socket sees every ICMPv6 frame on the link, and almost none of them
    /// are for us: neighbour discovery between two guests is the ordinary
    /// traffic of any segment.
    #[test]
    fn only_a_solicitation_is_answered() {
        assert!(is_solicitation(&[ROUTER_SOLICITATION, 0, 0, 0]));
        assert!(!is_solicitation(&[ROUTER_ADVERTISEMENT, 0, 0, 0]));
        assert!(!is_solicitation(&[135, 0, 0, 0]), "neighbour solicitation");
        assert!(!is_solicitation(&[]));
    }
}
