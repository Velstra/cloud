//! DHCP, answered on the node, for the guests on the node.
//!
//! ## There are no leases here
//!
//! This looks like a DHCP server and is not one in the part that usually
//! matters: it allocates nothing and remembers nothing. The binding between a
//! guest and an address already exists — it is the `Port` object, whose
//! `spec.address` a controller wrote and whose `spec.mac` identifies the NIC —
//! and this responder only *publishes* it on the wire.
//!
//! That is a deliberate reading of the platform's second invariant. A lease
//! file would be a second record of a fact the Port already holds, with its own
//! lifetime and its own way of being wrong; the two would eventually disagree,
//! and at that point nothing can say which of them the guest actually has. So:
//!
//! * **Nothing is written when a guest is answered.** An ACK changes no state
//!   anywhere, which is why answering twice is answering once and why an agent
//!   that is restarted mid-handshake needs no recovery.
//! * **Expiry needs no timer and no cleanup.** The lease time is finite, so the
//!   guest asks again; the answer is re-derived from whatever the objects say
//!   *then*. A port that was deleted stops being answered for on the pass that
//!   removed it, and the guest's own renewal is what notices.
//! * **A node restart loses nothing**, because there was nothing to lose.
//!
//! ## Who is asking
//!
//! The pair `(tap, chaddr)` and nothing else — see [`crate::guests`]. A guest
//! can write any MAC into a packet; it cannot make that packet arrive on
//! another guest's tap, which is why both halves are required to find a
//! binding.
//!
//! ## What is exercised by tests and what is not
//!
//! [`answer`] is a pure function from bytes to bytes and is tested as one. The
//! socket layer under it is not: binding one UDP socket per tap needs
//! privileges a test does not have, and a tap with a guest behind it is not
//! something this repository can conjure. It is kept to as few lines as
//! possible for exactly that reason, in the same spirit as the two hypervisor
//! backends that say so at the top of their own files.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use crate::guests::GuestRegistry;

/// The BOOTP header, before any options.
const HEADER: usize = 240;
/// `99.130.83.99`, the four bytes that make a BOOTP packet a DHCP one.
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
/// Ethernet, ten megabits, as the registry of hardware types has it.
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;
/// The flag a client sets when it cannot receive a unicast reply yet.
const FLAG_BROADCAST: u16 = 0x8000;
/// The shortest packet many clients will accept, from BOOTP's fixed size.
const MIN_REPLY: usize = 300;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

/// How long a guest may hold an address before asking again.
///
/// An hour, and the number is a staleness bound rather than a resource
/// question: nothing is reserved, so a short lease costs only packets. It is
/// how long a guest can keep using an address after the platform stopped
/// agreeing that it has one — the client renews at half of it.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(3600);

mod option {
    pub const PAD: u8 = 0;
    pub const SUBNET_MASK: u8 = 1;
    pub const ROUTER: u8 = 3;
    pub const DNS: u8 = 6;
    pub const HOSTNAME: u8 = 12;
    pub const MTU: u8 = 26;
    pub const REQUESTED_ADDRESS: u8 = 50;
    pub const LEASE_TIME: u8 = 51;
    pub const MESSAGE_TYPE: u8 = 53;
    pub const SERVER_ID: u8 = 54;
    pub const MESSAGE: u8 = 56;
    pub const END: u8 = 255;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
    Inform,
}

impl MessageType {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Discover,
            2 => Self::Offer,
            3 => Self::Request,
            4 => Self::Decline,
            5 => Self::Ack,
            6 => Self::Nak,
            7 => Self::Release,
            8 => Self::Inform,
            _ => return None,
        })
    }

    fn byte(self) -> u8 {
        match self {
            Self::Discover => 1,
            Self::Offer => 2,
            Self::Request => 3,
            Self::Decline => 4,
            Self::Ack => 5,
            Self::Nak => 6,
            Self::Release => 7,
            Self::Inform => 8,
        }
    }
}

/// A parsed request. Only the fields an answer depends on — a field this
/// responder does not read is a field it cannot get wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub kind: MessageType,
    pub xid: u32,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 6],
    pub requested_address: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
}

/// Why nothing is being sent back. Each variant is a different thing an
/// operator would do about it, which is why silence is not one value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Ignored {
    #[error("not a DHCP packet")]
    NotDhcp,
    #[error("a reply, not a request")]
    NotARequest,
    #[error("hardware type {htype}/{hlen} is not ethernet")]
    NotEthernet { htype: u8, hlen: u8 },
    #[error("no message type")]
    NoMessageType,
    #[error("nothing on {tap} has {mac}")]
    Unknown { tap: String, mac: String },
    #[error("{instance} has no IPv4 address to be given")]
    NoAddress { instance: String },
    #[error("{instance} let go of {address}")]
    Released { instance: String, address: Ipv4Addr },
    /// The loud one. A guest declines an address when it found somebody else
    /// already answering for it, which on a tenant network means two guests
    /// share an address — a datapath fault, not a DHCP one.
    #[error("{instance} refused {address}: something else is using it")]
    Declined { instance: String, address: Ipv4Addr },
    #[error("nothing to say to a {kind:?}")]
    NotOurs { kind: MessageType },
}

/// Where a reply goes. The socket layer decides how; this decides where, so
/// the choice is testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// Out of the tap the request came in on. Each tap carries one guest, so a
    /// broadcast here reaches exactly the guest that asked and nobody else.
    Broadcast,
    /// To a client that already has an address, or to the relay that forwarded
    /// the request.
    Unicast(SocketAddrV4),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub to: Destination,
    pub bytes: Vec<u8>,
}

/// What this node is, in the guest's eyes.
#[derive(Clone, Copy, Debug)]
pub struct Server {
    /// The address a guest should send its renewals to when the subnet does not
    /// name a gateway. The metadata address by default: it is link-local, it is
    /// this node, and it is already the one address every guest can reach.
    pub fallback_id: Ipv4Addr,
    pub lease: Duration,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            fallback_id: crate::metadata::ADDRESS,
            lease: DEFAULT_LEASE,
        }
    }
}

/// Parse a request, far enough to answer it.
pub fn parse(packet: &[u8]) -> Result<Request, Ignored> {
    if packet.len() < HEADER || packet[236..240] != COOKIE {
        return Err(Ignored::NotDhcp);
    }
    if packet[0] != BOOTREQUEST {
        return Err(Ignored::NotARequest);
    }
    if packet[1] != HTYPE_ETHERNET || packet[2] != HLEN_ETHERNET {
        return Err(Ignored::NotEthernet {
            htype: packet[1],
            hlen: packet[2],
        });
    }
    let options = options(&packet[HEADER..]);
    let kind = options
        .get(&option::MESSAGE_TYPE)
        .and_then(|v| v.first())
        .and_then(|b| MessageType::from_byte(*b))
        .ok_or(Ignored::NoMessageType)?;
    Ok(Request {
        kind,
        xid: u32::from_be_bytes(packet[4..8].try_into().expect("four bytes")),
        flags: u16::from_be_bytes(packet[10..12].try_into().expect("two bytes")),
        ciaddr: address_at(&packet[12..16]),
        giaddr: address_at(&packet[24..28]),
        chaddr: packet[28..34].try_into().expect("six bytes"),
        requested_address: options
            .get(&option::REQUESTED_ADDRESS)
            .map(|v| address_at(v)),
        server_id: options.get(&option::SERVER_ID).map(|v| address_at(v)),
    })
}

fn address_at(bytes: &[u8]) -> Ipv4Addr {
    match <[u8; 4]>::try_from(bytes) {
        Ok(octets) => Ipv4Addr::from(octets),
        // A malformed address field is read as "none given", which is what an
        // absent one means too. There is no answer that depends on telling
        // those apart, and inventing octets would be worse.
        Err(_) => Ipv4Addr::UNSPECIFIED,
    }
}

/// Options as a map. A repeated option keeps its first appearance, which is
/// what a client that sent one twice most likely meant.
fn options(mut rest: &[u8]) -> BTreeMap<u8, Vec<u8>> {
    let mut found = BTreeMap::new();
    while let Some((&code, tail)) = rest.split_first() {
        match code {
            option::PAD => rest = tail,
            option::END => break,
            _ => {
                let Some((&len, tail)) = tail.split_first() else {
                    break;
                };
                let len = len as usize;
                if tail.len() < len {
                    break;
                }
                found.entry(code).or_insert_with(|| tail[..len].to_vec());
                rest = &tail[len..];
            }
        }
    }
    found
}

/// Answer one packet that arrived on a device carrying `taps`.
///
/// One tap on a plain tap device; several on a shared bridge, where the frame
/// does not say which of them it came from and the MAC is what picks the guest
/// out. Tried in order, and the first tap that has this MAC behind it wins —
/// which is exact, because a MAC belongs to one Port and a Port is on one tap.
pub fn answer_behind(
    taps: &[String],
    packet: &[u8],
    guests: &GuestRegistry,
    server: &Server,
) -> std::result::Result<Reply, Ignored> {
    let mut last = None;
    for tap in taps {
        match answer(tap, packet, guests, server) {
            Ok(reply) => return Ok(reply),
            // Keep looking: on a shared bridge, "nothing on this tap has that
            // MAC" is the expected answer for every tap but one.
            Err(e @ Ignored::Unknown { .. }) => last = Some(e),
            // Anything else is about the packet, not about which tap it came
            // from, and asking a second tap the same question would get the
            // same answer.
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or(Ignored::Unknown {
        tap: taps.join(", "),
        mac: String::new(),
    }))
}

/// Answer one packet that arrived on `tap`.
///
/// The whole of the responder's behaviour, as a function of the bytes, the
/// objects this node holds and nothing else. No clock, no socket, no stored
/// lease — which is what makes the tests below tests of the behaviour rather
/// than of a mock.
pub fn answer(
    tap: &str,
    packet: &[u8],
    guests: &GuestRegistry,
    server: &Server,
) -> Result<Reply, Ignored> {
    let request = parse(packet)?;
    let Some((view, n)) = guests.on_wire(tap, request.chaddr) else {
        return Err(Ignored::Unknown {
            tap: tap.to_string(),
            mac: velstra_cloud_model::network::format_mac(&request.chaddr),
        });
    };
    let interface = &view.interfaces[n];
    let Some((address, _, gateway)) = interface.v4() else {
        return Err(Ignored::NoAddress {
            instance: view.instance_id.clone(),
        });
    };
    let server_id = gateway.unwrap_or(server.fallback_id);

    match request.kind {
        MessageType::Discover => Ok(build(
            &request,
            MessageType::Offer,
            address,
            true,
            &view,
            interface,
            server_id,
            server.lease,
        )),
        MessageType::Request => {
            // What the client believes it has: the address it asked for, or
            // the one it is renewing from.
            let wanted = request
                .requested_address
                .filter(|a| !a.is_unspecified())
                .unwrap_or(request.ciaddr);
            if !wanted.is_unspecified() && wanted != address {
                // A NAK rather than silence, and it is the one case where
                // saying nothing would be worse: a guest holding an address
                // that is not its own keeps using it until the lease runs out,
                // and on a shared network that is somebody else's traffic.
                return Ok(nak(&request, &view, server_id, wanted, address));
            }
            Ok(build(
                &request,
                MessageType::Ack,
                address,
                true,
                &view,
                interface,
                server_id,
                server.lease,
            ))
        }
        // A client that already has an address and wants only the rest of the
        // configuration. No address and no lease in the answer, per RFC 2131.
        MessageType::Inform => Ok(build(
            &request,
            MessageType::Ack,
            Ipv4Addr::UNSPECIFIED,
            false,
            &view,
            interface,
            server_id,
            server.lease,
        )),
        MessageType::Release => Err(Ignored::Released {
            instance: view.instance_id.clone(),
            address,
        }),
        MessageType::Decline => Err(Ignored::Declined {
            instance: view.instance_id.clone(),
            address,
        }),
        kind => Err(Ignored::NotOurs { kind }),
    }
}

/// One answer that grants configuration: an OFFER, an ACK, or the ACK to an
/// INFORM.
///
/// `granting` is the only difference between them. An INFORM comes from a
/// client that already has an address and wants everything else, so it gets the
/// mask, the gateway, the resolvers and the MTU — and no address and no lease,
/// because it was not asking to be given one.
#[allow(clippy::too_many_arguments)]
fn build(
    request: &Request,
    kind: MessageType,
    yiaddr: Ipv4Addr,
    granting: bool,
    view: &crate::guests::GuestView,
    interface: &crate::guests::Interface,
    server_id: Ipv4Addr,
    lease: Duration,
) -> Reply {
    let (_, mask, gateway) = interface.v4().expect("the caller checked for one");
    let mut options = vec![
        (option::MESSAGE_TYPE, vec![kind.byte()]),
        (option::SERVER_ID, server_id.octets().to_vec()),
    ];
    if granting {
        options.push((
            option::LEASE_TIME,
            (lease.as_secs() as u32).to_be_bytes().to_vec(),
        ));
    }
    options.push((option::SUBNET_MASK, mask.octets().to_vec()));
    if let Some(gateway) = gateway {
        options.push((option::ROUTER, gateway.octets().to_vec()));
    }
    let dns = interface.v4_dns();
    if !dns.is_empty() {
        options.push((
            option::DNS,
            dns.iter().flat_map(|d| d.octets()).collect::<Vec<_>>(),
        ));
    }
    if !view.hostname.is_empty() {
        options.push((option::HOSTNAME, view.hostname.as_bytes().to_vec()));
    }
    // Sent whether or not the client asked for it. An overlay's MTU is not
    // something a guest can work out for itself, and a guest that keeps the
    // default blackholes every full-sized packet it sends — the failure that
    // looks like "the network is fine but nothing large works".
    if let Some(mtu) = interface.mtu {
        if let Ok(mtu) = u16::try_from(mtu) {
            options.push((option::MTU, mtu.to_be_bytes().to_vec()));
        }
    }
    Reply {
        to: destination(request, kind),
        bytes: encode(request, yiaddr, server_id, &options),
    }
}

fn nak(
    request: &Request,
    view: &crate::guests::GuestView,
    server_id: Ipv4Addr,
    wanted: Ipv4Addr,
    has: Ipv4Addr,
) -> Reply {
    let message = format!("{} has {has}, not {wanted}", view.instance_id);
    let options = vec![
        (option::MESSAGE_TYPE, vec![MessageType::Nak.byte()]),
        (option::SERVER_ID, server_id.octets().to_vec()),
        (option::MESSAGE, message.into_bytes()),
    ];
    Reply {
        to: destination(request, MessageType::Nak),
        bytes: encode(request, Ipv4Addr::UNSPECIFIED, server_id, &options),
    }
}

/// Where the answer goes, per RFC 2131 §4.1.
///
/// One deliberate simplification: a client that did *not* set the broadcast
/// flag is supposed to be sent a unicast to an address it does not have yet,
/// which means writing its ARP entry — a raw socket and a privilege this needs
/// no other reason to hold. It is broadcast instead, which every client
/// accepts, and which is contained here in a way it would not be on a shared
/// segment: the tap carries one guest, so the broadcast reaches exactly the
/// guest that asked.
fn destination(request: &Request, kind: MessageType) -> Destination {
    if !request.giaddr.is_unspecified() {
        return Destination::Unicast(SocketAddrV4::new(request.giaddr, SERVER_PORT));
    }
    if kind == MessageType::Nak {
        // A NAK says "the address you named is not yours", so it cannot be
        // sent to that address.
        return Destination::Broadcast;
    }
    if !request.ciaddr.is_unspecified() && request.flags & FLAG_BROADCAST == 0 {
        return Destination::Unicast(SocketAddrV4::new(request.ciaddr, CLIENT_PORT));
    }
    Destination::Broadcast
}

fn encode(
    request: &Request,
    yiaddr: Ipv4Addr,
    server_id: Ipv4Addr,
    options: &[(u8, Vec<u8>)],
) -> Vec<u8> {
    let mut packet = vec![0u8; HEADER];
    packet[0] = BOOTREPLY;
    packet[1] = HTYPE_ETHERNET;
    packet[2] = HLEN_ETHERNET;
    packet[4..8].copy_from_slice(&request.xid.to_be_bytes());
    packet[10..12].copy_from_slice(&request.flags.to_be_bytes());
    // ciaddr is echoed rather than filled in: it is the client's statement of
    // what it currently holds, and a reply that changed it would be answering
    // a question nobody asked.
    packet[16..20].copy_from_slice(&yiaddr.octets());
    packet[20..24].copy_from_slice(&server_id.octets());
    packet[24..28].copy_from_slice(&request.giaddr.octets());
    packet[28..34].copy_from_slice(&request.chaddr);
    packet[236..240].copy_from_slice(&COOKIE);
    for (code, value) in options {
        // An option longer than 255 bytes cannot be expressed; dropping it is
        // better than writing a length that truncates it into something the
        // client would read as a different option entirely.
        let Ok(len) = u8::try_from(value.len()) else {
            tracing::error!(
                code,
                len = value.len(),
                "a DHCP option was too long to send"
            );
            continue;
        };
        packet.push(*code);
        packet.push(len);
        packet.extend_from_slice(value);
    }
    packet.push(option::END);
    packet.resize(packet.len().max(MIN_REPLY), option::PAD);
    packet
}

// ---- the socket layer ----------------------------------------------------

/// Listen wherever this node's guests can be heard, and keep doing so as guests
/// come and go.
///
/// The set is re-read on a timer from the registry rather than pushed by the
/// agent, for the same reason everything else here is level-triggered: a missed
/// change costs one interval of latency, never a socket that is listening for a
/// guest that has gone.
///
/// ## Not always the tap
///
/// `SO_BINDTODEVICE` names an **L3** interface, and a tap that has been enslaved
/// to a bridge is not one any more: the address is on the bridge, the frame is
/// delivered on the bridge, and a socket bound to the tap is handed nothing.
///
/// That is not hypothetical — it is what [`crate::localnet`] does to every tap
/// on a node that is its guests' first hop, and it broke this responder
/// completely: the guest broadcast a DISCOVER, the kernel delivered it to the
/// bridge, nothing was bound there, and the guest went on to time out, find no
/// datasource, and boot unconfigured. Working DHCP became silent DHCP the moment
/// the segment gained the gateway that made it useful.
///
/// So the device is resolved from the kernel on every pass — the master if the
/// tap has one, the tap otherwise — and the listener is keyed by it. A tap that
/// gains or loses a master changes the key, which replaces the listener, which
/// is the whole recovery story.
pub async fn serve(guests: GuestRegistry, server: Server, every: Duration) {
    let mut listening: BTreeMap<String, (Vec<String>, tokio::task::JoinHandle<()>)> =
        BTreeMap::new();
    let mut ticker = tokio::time::interval(every);
    loop {
        ticker.tick().await;
        let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for tap in guests.taps() {
            wanted.entry(l3_device(&tap).await).or_default().push(tap);
        }
        listening.retain(|device, (taps, task)| {
            // The tap set is part of the key in everything but name: a guest
            // arriving on a bridge that is already listened to has to be
            // answerable, and a listener holding a stale set would not know it.
            let keep = wanted.get(device) == Some(taps) && !task.is_finished();
            if !keep {
                task.abort();
                tracing::info!(%device, "stopped answering DHCP");
            }
            keep
        });
        for (device, taps) in wanted {
            if listening.contains_key(&device) {
                continue;
            }
            match bind(&device) {
                Ok(socket) => {
                    tracing::info!(%device, taps = taps.len(), "answering DHCP");
                    let task = tokio::spawn(listen(
                        device.clone(),
                        taps.clone(),
                        socket,
                        guests.clone(),
                        server,
                    ));
                    listening.insert(device, (taps, task));
                }
                Err(e) => {
                    tracing::error!(%device, error = %e, "could not answer DHCP on this device")
                }
            }
        }
    }
}

/// Where frames from `tap` are actually delivered: its bridge, or itself.
///
/// Asked of `ip` rather than `/sys/class/net/<tap>/master`, for the reason
/// [`crate::datapath`] gives at length: sysfs shows the network namespace it was
/// *mounted* in, not the one this process is in.
///
/// An unanswerable question is answered with the tap. Binding to a tap that has
/// a master hears nothing, which is the failure this exists to prevent — but
/// binding to a device that does not exist fails outright, and a responder that
/// stopped answering everybody because one `ip` call failed would be worse.
async fn l3_device(tap: &str) -> String {
    let output = tokio::process::Command::new("ip")
        .args(["-o", "link", "show", "dev", tap])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            master_of(&String::from_utf8_lossy(&o.stdout)).unwrap_or_else(|| tap.to_string())
        }
        _ => tap.to_string(),
    }
}

/// The master in one line of `ip -o link show`, if the interface has one.
fn master_of(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        if word == "master" {
            return words.next().map(str::to_string);
        }
    }
    None
}

/// One socket per L3 device, seeing the frames of every guest behind it.
///
/// `SO_REUSEADDR` rather than `SO_REUSEPORT`: several sockets do have to share
/// port 67, but `SO_REUSEPORT` load-balances a broadcast to exactly one member
/// of the group, which would mean one tap's socket quietly swallowing another
/// tap's DISCOVER.
fn bind(device: &str) -> std::io::Result<Arc<tokio::net::UdpSocket>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    // The one line that narrows identity: this socket receives only frames that
    // arrived on this device, so which segment a request came from is a fact the
    // kernel established rather than something the packet claimed. On a tap that
    // is the guest; on a bridge it is the segment, and the MAC picks the guest
    // out of it — which is all that is knowable there, because guests sharing a
    // bridge share a wire.
    socket.bind_device(Some(device.as_bytes()))?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, SERVER_PORT)).into())?;
    socket.set_nonblocking(true)?;
    Ok(Arc::new(tokio::net::UdpSocket::from_std(socket.into())?))
}

async fn listen(
    device: String,
    taps: Vec<String>,
    socket: Arc<tokio::net::UdpSocket>,
    guests: GuestRegistry,
    server: Server,
) {
    // One frame's worth. A DHCP packet larger than this is not one this
    // responder has anything to say to.
    let mut buffer = vec![0u8; 1500];
    loop {
        let received = match socket.recv_from(&mut buffer).await {
            Ok((n, _)) => n,
            Err(e) => {
                tracing::error!(%device, error = %e, "the DHCP socket failed");
                return;
            }
        };
        match answer_behind(&taps, &buffer[..received], &guests, &server) {
            Ok(reply) => {
                let to = match reply.to {
                    Destination::Broadcast => SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT),
                    Destination::Unicast(to) => to,
                };
                if let Err(e) = socket.send_to(&reply.bytes, to).await {
                    tracing::error!(%device, error = %e, "could not answer a guest's DHCP request");
                }
            }
            // A guest saying an address is already in use is a datapath fault,
            // and the only place it can be noticed is here.
            Err(e @ Ignored::Declined { .. }) => tracing::error!(%device, "{e}"),
            Err(e) => tracing::debug!(%device, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use velstra_cloud_model::network::{Cidr, parse_mac};

    use super::*;
    use crate::guests::{GuestView, Interface};

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const OTHER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x99, 0x99, 0x99];
    const TAP: &str = "vt-port-a";

    fn view(instance: &str, address: &str, tap: &str, mac: [u8; 6]) -> GuestView {
        GuestView {
            instance_id: instance.to_string(),
            hostname: instance.rsplit('/').next().unwrap().to_string(),
            interfaces: vec![Interface {
                subnet: "projects/p1/subnets/s1".into(),
                on_host_bridge: false,
                port: "projects/p1/ports/port-a".into(),
                mac: Some(mac),
                cidr: Some(Cidr::parse(address).unwrap()),
                gateway: Some("10.20.0.1".parse().unwrap()),
                dns: vec!["10.20.0.1".parse().unwrap(), "fd00::1".parse().unwrap()],
                mtu: Some(1450),
                tap: Some(tap.to_string()),
                public: Vec::new(),
            }],
            ..Default::default()
        }
    }

    fn registry() -> GuestRegistry {
        let guests = GuestRegistry::new();
        guests.replace(vec![
            view("projects/p1/instances/i1", "10.20.0.10/24", TAP, MAC),
            view(
                "projects/p1/instances/i2",
                "10.20.0.11/24",
                "vt-port-b",
                OTHER_MAC,
            ),
        ]);
        guests
    }

    /// Two guests, one bridge, one socket. The frame does not say which tap it
    /// came from, so the MAC has to pick — and picking wrong would hand one
    /// tenant's guest the other's address.
    #[test]
    fn on_a_shared_bridge_the_mac_picks_the_guest_out() {
        let guests = registry();
        let behind = [TAP.to_string(), "vt-port-b".to_string()];
        let server = Server::default();

        let reply = answer_behind(
            &behind,
            &request(MessageType::Discover, OTHER_MAC, &[]),
            &guests,
            &server,
        )
        .expect("the second guest is behind this bridge");
        assert_eq!(&reply.bytes[16..20], &[10, 20, 0, 11], "the wrong guest");

        let reply = answer_behind(
            &behind,
            &request(MessageType::Discover, MAC, &[]),
            &guests,
            &server,
        )
        .expect("so is the first");
        assert_eq!(&reply.bytes[16..20], &[10, 20, 0, 10], "the wrong guest");

        // And a MAC behind neither is still nobody, rather than whoever was
        // asked first.
        assert!(matches!(
            answer_behind(
                &behind,
                &request(MessageType::Discover, [9, 9, 9, 9, 9, 9], &[]),
                &guests,
                &server,
            ),
            Err(Ignored::Unknown { .. })
        ));
    }

    /// A request as a client sends one.
    fn request(kind: MessageType, mac: [u8; 6], extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut packet = vec![0u8; HEADER];
        packet[0] = BOOTREQUEST;
        packet[1] = HTYPE_ETHERNET;
        packet[2] = HLEN_ETHERNET;
        packet[4..8].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        packet[10..12].copy_from_slice(&FLAG_BROADCAST.to_be_bytes());
        packet[28..34].copy_from_slice(&mac);
        packet[236..240].copy_from_slice(&COOKIE);
        packet.push(option::MESSAGE_TYPE);
        packet.push(1);
        packet.push(kind.byte());
        for (code, value) in extra {
            packet.push(*code);
            packet.push(value.len() as u8);
            packet.extend_from_slice(value);
        }
        packet.push(option::END);
        packet
    }

    fn parsed(reply: &Reply) -> (MessageType, Ipv4Addr, BTreeMap<u8, Vec<u8>>) {
        let bytes = &reply.bytes;
        assert_eq!(bytes[0], BOOTREPLY);
        assert_eq!(bytes[236..240], COOKIE);
        let options = options(&bytes[HEADER..]);
        let kind = MessageType::from_byte(options[&option::MESSAGE_TYPE][0]).unwrap();
        (kind, address_at(&bytes[16..20]), options)
    }

    #[test]
    fn a_discover_is_offered_the_address_its_port_already_has() {
        // The whole point: nothing is allocated here. The address in the offer
        // is the one on the Port object, which a controller wrote.
        let reply = answer(
            TAP,
            &request(MessageType::Discover, MAC, &[]),
            &registry(),
            &Server::default(),
        )
        .unwrap();
        let (kind, yiaddr, options) = parsed(&reply);
        assert_eq!(kind, MessageType::Offer);
        assert_eq!(yiaddr, "10.20.0.10".parse::<Ipv4Addr>().unwrap());
        assert_eq!(options[&option::SUBNET_MASK], vec![255, 255, 255, 0]);
        assert_eq!(options[&option::ROUTER], vec![10, 20, 0, 1]);
        // Only the v4 resolver: a v6 one in this option would be four bytes of
        // something else entirely to the client.
        assert_eq!(options[&option::DNS], vec![10, 20, 0, 1]);
        assert_eq!(options[&option::HOSTNAME], b"i1".to_vec());
        assert_eq!(options[&option::MTU], vec![0x05, 0xaa]);
        assert_eq!(options[&option::LEASE_TIME], 3600u32.to_be_bytes().to_vec());
        // The gateway answers for renewals, because it is the address the
        // guest can reach on its own network.
        assert_eq!(options[&option::SERVER_ID], vec![10, 20, 0, 1]);
    }

    #[test]
    fn a_reply_echoes_what_lets_the_client_recognise_it() {
        let reply = answer(
            TAP,
            &request(MessageType::Discover, MAC, &[]),
            &registry(),
            &Server::default(),
        )
        .unwrap();
        let bytes = &reply.bytes;
        assert_eq!(
            &bytes[4..8],
            &0xdead_beefu32.to_be_bytes(),
            "the xid changed"
        );
        assert_eq!(&bytes[28..34], &MAC, "the hardware address changed");
        assert!(
            bytes.len() >= MIN_REPLY,
            "a client that pads would drop this"
        );
    }

    #[test]
    fn a_request_for_the_address_the_port_has_is_acknowledged() {
        let asked = request(
            MessageType::Request,
            MAC,
            &[(option::REQUESTED_ADDRESS, vec![10, 20, 0, 10])],
        );
        let (kind, yiaddr, _) =
            parsed(&answer(TAP, &asked, &registry(), &Server::default()).unwrap());
        assert_eq!(kind, MessageType::Ack);
        assert_eq!(yiaddr, "10.20.0.10".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn a_request_for_somebody_elses_address_is_refused_out_loud() {
        // Silence here would leave the guest using an address that is not its
        // own until the lease it invented ran out.
        let asked = request(
            MessageType::Request,
            MAC,
            &[(option::REQUESTED_ADDRESS, vec![10, 20, 0, 11])],
        );
        let reply = answer(TAP, &asked, &registry(), &Server::default()).unwrap();
        let (kind, yiaddr, options) = parsed(&reply);
        assert_eq!(kind, MessageType::Nak);
        assert!(yiaddr.is_unspecified());
        assert_eq!(reply.to, Destination::Broadcast);
        let message = String::from_utf8(options[&option::MESSAGE].clone()).unwrap();
        assert!(message.contains("10.20.0.10"), "{message}");
    }

    #[test]
    fn a_guest_cannot_be_leased_its_neighbours_address_by_claiming_its_mac() {
        // The test that matters, and the reason the key is `(tap, mac)`. i1
        // puts i2's MAC in the packet; the packet still comes out of i1's tap.
        let guests = registry();
        let spoofed = request(MessageType::Discover, OTHER_MAC, &[]);
        let refused = answer(TAP, &spoofed, &guests, &Server::default()).unwrap_err();
        assert!(matches!(refused, Ignored::Unknown { .. }), "{refused:?}");

        // And from its own tap, the neighbour's MAC does resolve — proving the
        // refusal above was the tap and not the MAC being unknown everywhere.
        let honest = answer("vt-port-b", &spoofed, &guests, &Server::default()).unwrap();
        assert_eq!(parsed(&honest).1, "10.20.0.11".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn a_guest_this_node_does_not_run_gets_nothing() {
        let guests = registry();
        let stranger = request(MessageType::Discover, [0xaa; 6], &[]);
        assert!(matches!(
            answer(TAP, &stranger, &guests, &Server::default()),
            Err(Ignored::Unknown { .. })
        ));
    }

    #[test]
    fn a_release_and_a_decline_change_nothing_and_a_decline_is_reported() {
        // Nothing to release: the binding is the Port, and a guest does not get
        // to delete one by sending a packet.
        let guests = registry();
        let released = answer(
            TAP,
            &request(MessageType::Release, MAC, &[]),
            &guests,
            &Server::default(),
        )
        .unwrap_err();
        assert!(matches!(released, Ignored::Released { .. }));
        // The guest can still ask again straight afterwards and be given the
        // same address, because nothing was written down.
        assert!(
            answer(
                TAP,
                &request(MessageType::Discover, MAC, &[]),
                &guests,
                &Server::default()
            )
            .is_ok()
        );

        let declined = answer(
            TAP,
            &request(MessageType::Decline, MAC, &[]),
            &guests,
            &Server::default(),
        )
        .unwrap_err();
        assert!(matches!(declined, Ignored::Declined { .. }), "{declined:?}");
    }

    #[test]
    fn an_inform_gets_the_configuration_and_no_lease() {
        let asked = request(MessageType::Inform, MAC, &[]);
        let reply = answer(TAP, &asked, &registry(), &Server::default()).unwrap();
        let (kind, yiaddr, options) = parsed(&reply);
        assert_eq!(kind, MessageType::Ack);
        assert!(yiaddr.is_unspecified(), "an INFORM was given an address");
        assert!(!options.contains_key(&option::LEASE_TIME));
        assert_eq!(options[&option::ROUTER], vec![10, 20, 0, 1]);
    }

    #[test]
    fn a_renewal_from_a_client_that_has_an_address_is_answered_to_that_address() {
        let mut asked = request(MessageType::Request, MAC, &[]);
        asked[10..12].copy_from_slice(&0u16.to_be_bytes()); // it can receive unicast now
        asked[12..16].copy_from_slice(&[10, 20, 0, 10]); // ciaddr
        let reply = answer(TAP, &asked, &registry(), &Server::default()).unwrap();
        assert_eq!(parsed(&reply).0, MessageType::Ack);
        assert_eq!(
            reply.to,
            Destination::Unicast(SocketAddrV4::new(
                "10.20.0.10".parse().unwrap(),
                CLIENT_PORT
            ))
        );
    }

    #[test]
    fn a_relayed_request_is_answered_to_the_relay() {
        let mut asked = request(MessageType::Discover, MAC, &[]);
        asked[24..28].copy_from_slice(&[10, 20, 0, 1]); // giaddr
        let reply = answer(TAP, &asked, &registry(), &Server::default()).unwrap();
        assert_eq!(
            reply.to,
            Destination::Unicast(SocketAddrV4::new("10.20.0.1".parse().unwrap(), SERVER_PORT))
        );
    }

    #[test]
    fn rubbish_is_not_answered() {
        let guests = registry();
        let server = Server::default();
        assert_eq!(answer(TAP, &[], &guests, &server), Err(Ignored::NotDhcp));
        assert_eq!(
            answer(TAP, &vec![0u8; HEADER], &guests, &server),
            Err(Ignored::NotDhcp),
            "a packet without the magic cookie was read as DHCP"
        );
        // A reply, not a request: answering one would be two servers talking
        // to each other forever.
        let mut echo = request(MessageType::Offer, MAC, &[]);
        echo[0] = BOOTREPLY;
        assert_eq!(
            answer(TAP, &echo, &guests, &server),
            Err(Ignored::NotARequest)
        );
        // Truncated options, which is what a hostile or a broken client sends.
        let mut truncated = request(MessageType::Discover, MAC, &[]);
        truncated.pop();
        truncated.push(option::HOSTNAME);
        truncated.push(200);
        truncated.push(b'x');
        assert!(answer(TAP, &truncated, &guests, &server).is_ok());
    }

    #[test]
    fn a_port_with_no_address_yet_is_not_answered_for() {
        // Before a controller has allocated one there is nothing to publish,
        // and silence is right: the guest keeps asking, and gets an answer on
        // the pass after the address appears.
        let guests = GuestRegistry::new();
        guests.replace(vec![GuestView {
            instance_id: "projects/p1/instances/i1".into(),
            interfaces: vec![Interface {
                mac: Some(MAC),
                tap: Some(TAP.into()),
                ..Default::default()
            }],
            ..Default::default()
        }]);
        assert!(matches!(
            answer(
                TAP,
                &request(MessageType::Discover, MAC, &[]),
                &guests,
                &Server::default()
            ),
            Err(Ignored::NoAddress { .. })
        ));
    }

    #[test]
    fn a_subnet_with_no_gateway_makes_this_node_the_server_to_renew_from() {
        let guests = GuestRegistry::new();
        let mut only = view("projects/p1/instances/i1", "10.20.0.10/24", TAP, MAC);
        only.interfaces[0].gateway = None;
        guests.replace(vec![only]);
        let reply = answer(
            TAP,
            &request(MessageType::Discover, MAC, &[]),
            &guests,
            &Server::default(),
        )
        .unwrap();
        let (_, _, options) = parsed(&reply);
        assert_eq!(options[&option::SERVER_ID], vec![169, 254, 169, 254]);
        assert!(!options.contains_key(&option::ROUTER));
    }

    #[test]
    fn only_the_taps_with_a_guest_behind_them_are_listened_on() {
        assert_eq!(
            registry().taps(),
            BTreeSet::from([TAP.to_string(), "vt-port-b".to_string()])
        );
    }

    #[test]
    fn the_mac_in_a_packet_is_read_the_same_way_a_port_declares_one() {
        // Two spellings of one address, and the responder has to agree with
        // the object or nothing matches.
        assert_eq!(parse_mac("52:54:00:12:34:56").unwrap(), MAC);
    }
}

#[cfg(test)]
mod on_a_bridge {
    use super::*;

    /// The parse that decides where this responder listens. A tap whose master
    /// is missed here is a tap this node answers DHCP on and hears nothing from.
    #[test]
    fn a_tap_with_a_master_is_heard_on_the_master() {
        let enslaved = "4: vtp1e803: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel \
                        master vbrlan072ca state UP mode DEFAULT group default qlen 1000\\    \
                        link/ether 0a:91:95:6d:d5:b9 brd ff:ff:ff:ff:ff:ff";
        assert_eq!(master_of(enslaved).as_deref(), Some("vbrlan072ca"));

        let alone = "4: vtp1e803: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc fq_codel state UP \
                     mode DEFAULT group default qlen 1000\\    link/ether 0a:91:95:6d:d5:b9";
        assert_eq!(master_of(alone), None);

        // And an interface *named* master is not one.
        assert_eq!(master_of("7: master: <UP> mtu 1500 state UP"), None);
    }
}
