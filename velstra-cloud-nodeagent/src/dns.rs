//! Names for guests, and a way off the cell by name.
//!
//! A guest used to come up with an address, a gateway and an MTU and no
//! resolver at all: the default subnet is created with `dns: []`, so the
//! out-of-the-box path produced a machine that could not install a package or
//! reach any host by name. That is the first thing anybody tries.
//!
//! This answers on the same devices the DHCP responder answers on, from the
//! subnet's gateway address, and does two things:
//!
//! * **Names the cell's own guests.** `db-1` and `db-1.<zone>` resolve to the
//!   addresses of a guest on the asker's own subnet. Scoped to the subnet on
//!   purpose: names are a tenant's, and answering across subnets would let one
//!   project enumerate another's machines by guessing.
//! * **Forwards everything else** to the resolvers the node itself uses. A
//!   guest asking for `deb.debian.org` is asking a question this platform has
//!   no opinion about, and refusing it would be the same broken machine with a
//!   different error.
//!
//! ## Why this is written out rather than pulled in
//!
//! A resolver library would be a dependency on the data path of every guest in
//! every cell, and what is needed here is one message type, three record types
//! and a forwarder. The parsing below is the whole of the wire format this
//! answers, and anything it does not understand is forwarded rather than
//! guessed at — which is also the honest failure mode.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use crate::guests::GuestRegistry;

/// The port a resolver answers on.
pub const PORT: u16 = 53;

/// How long a guest may cache an answer about another guest.
///
/// Short, because the thing it names can move: a guest that is rebuilt keeps
/// its name and gets another address, and a five-minute cache would send its
/// neighbours to the old one for five minutes. Nothing here is expensive
/// enough to be worth the staleness.
const TTL_SECONDS: u32 = 30;

/// The suffix a cell's own names live under.
///
/// `.internal` rather than a real TLD, and never a name that could collide
/// with something on the public internet: a guest that asks for
/// `db-1.internal` must never be answered by somebody else's zone.
pub const DEFAULT_ZONE: &str = "velstra.internal";

/// What one query wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub kind: u16,
    pub class: u16,
}

/// Record types this answers itself. Everything else is forwarded.
pub const A: u16 = 1;
pub const AAAA: u16 = 28;
pub const PTR: u16 = 12;
const IN: u16 = 1;

/// Read the header and the first question out of a query.
///
/// One question, because that is what every resolver in practice sends and
/// what every server in practice answers; a packet carrying more is forwarded
/// whole rather than half-answered.
pub fn parse_query(packet: &[u8]) -> Option<(u16, Question)> {
    if packet.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([packet[0], packet[1]]);
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    // A response, or anything that is not a standard query, is not ours.
    if flags & 0x8000 != 0 || (flags >> 11) & 0xf != 0 {
        return None;
    }
    if u16::from_be_bytes([packet[4], packet[5]]) != 1 {
        return None;
    }
    let (name, at) = read_name(packet, 12)?;
    if packet.len() < at + 4 {
        return None;
    }
    Some((
        id,
        Question {
            name,
            kind: u16::from_be_bytes([packet[at], packet[at + 1]]),
            class: u16::from_be_bytes([packet[at + 2], packet[at + 3]]),
        },
    ))
}

/// A name in wire format, as dotted text, and where it ended.
///
/// Compression pointers are not followed: a *question* is never compressed —
/// there is nothing before it to point at — and refusing to chase pointers
/// here is what keeps a malformed packet from becoming a loop.
fn read_name(packet: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut parts: Vec<String> = Vec::new();
    loop {
        let length = *packet.get(at)? as usize;
        at += 1;
        if length == 0 {
            break;
        }
        if length & 0xc0 != 0 {
            return None;
        }
        let label = packet.get(at..at + length)?;
        parts.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        at += length;
    }
    Some((parts.join("."), at))
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let bytes = label.as_bytes();
        // A label longer than 63 bytes cannot be written; it is also not a
        // name this platform ever produces, so truncating is honest.
        let take = bytes.len().min(63);
        out.push(take as u8);
        out.extend_from_slice(&bytes[..take]);
    }
    out.push(0);
}

/// Build an answer to `question` carrying `addresses`, or a name-not-found.
pub fn answer(id: u16, question: &Question, addresses: &[IpAddr], names: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&id.to_be_bytes());
    let found = !addresses.is_empty() || !names.is_empty();
    // Response, authoritative, recursion available; NXDOMAIN when nothing is
    // here. Authoritative because for these names it is: nothing else in the
    // world may answer for a cell's own guests.
    let flags: u16 = 0x8580 | if found { 0 } else { 3 };
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    let count = (addresses.len() + names.len()) as u16;
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    write_name(&mut out, &question.name);
    out.extend_from_slice(&question.kind.to_be_bytes());
    out.extend_from_slice(&question.class.to_be_bytes());
    for address in addresses {
        write_name(&mut out, &question.name);
        let (kind, bytes): (u16, Vec<u8>) = match address {
            IpAddr::V4(a) => (A, a.octets().to_vec()),
            IpAddr::V6(a) => (AAAA, a.octets().to_vec()),
        };
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&IN.to_be_bytes());
        out.extend_from_slice(&TTL_SECONDS.to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    for name in names {
        write_name(&mut out, &question.name);
        out.extend_from_slice(&PTR.to_be_bytes());
        out.extend_from_slice(&IN.to_be_bytes());
        out.extend_from_slice(&TTL_SECONDS.to_be_bytes());
        let mut target = Vec::new();
        write_name(&mut target, name);
        out.extend_from_slice(&(target.len() as u16).to_be_bytes());
        out.extend_from_slice(&target);
    }
    out
}

/// The name a guest is known by inside the cell, and its fully qualified form.
pub fn names_for(hostname: &str, zone: &str) -> (String, String) {
    let short = hostname.to_ascii_lowercase();
    (short.clone(), format!("{short}.{zone}"))
}

/// The reverse name for an address, as a query would spell it.
pub fn reverse_name(address: IpAddr) -> String {
    match address {
        IpAddr::V4(a) => {
            let o = a.octets();
            format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(a) => {
            let mut out = String::new();
            for byte in a.octets().iter().rev() {
                out.push_str(&format!("{:x}.{:x}.", byte & 0xf, byte >> 4));
            }
            out.push_str("ip6.arpa");
            out
        }
    }
}

/// Every guest in the cell that has a name and an address, by subnet.
///
/// Cell-wide, not this node's: a name that resolved only for guests that
/// happened to share a hypervisor would be a name nobody could rely on, and
/// the first thing anybody tries is reaching the machine next door. The agent
/// already reads every instance and port on every pass to decide what is
/// its own; this is the same reading, kept.
#[derive(Clone, Default)]
pub struct Names {
    inner: Arc<std::sync::RwLock<Vec<Named>>>,
}

/// One guest's name on one subnet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    pub hostname: String,
    pub subnet: String,
    pub address: IpAddr,
}

impl Names {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole index. Level-triggered, like everything else the
    /// agent does: what is here now is what the cell says now.
    pub fn replace(&self, names: Vec<Named>) {
        *self.inner.write().unwrap_or_else(|p| {
            self.inner.clear_poison();
            p.into_inner()
        }) = names;
    }

    /// The names one subnet holds.
    pub fn zone(&self, subnet: &str, suffix: &str) -> Zone {
        let held = self.inner.read().unwrap_or_else(|p| {
            self.inner.clear_poison();
            p.into_inner()
        });
        let mut out = Zone::default();
        for named in held.iter().filter(|n| n.subnet == subnet) {
            let (short, long) = names_for(&named.hostname, suffix);
            out.forward.entry(short).or_default().push(named.address);
            out.forward
                .entry(long.clone())
                .or_default()
                .push(named.address);
            out.reverse.insert(reverse_name(named.address), long);
        }
        out
    }
}

/// The cell's names, from the objects the agent already reads on every pass.
///
/// A guest's name is the last segment of its own — `db-1`, not
/// `projects/p1/instances/db-1` — and its addresses are the ones its ports
/// carry. A guest with no port, or a port with no address yet, has no name to
/// give: a name that resolved to nothing would be worse than one that does
/// not resolve, because the caller would connect to the answer.
pub fn names_in(
    instances: &[velstra_cloud_model::resources::Instance],
    ports: &std::collections::BTreeMap<String, velstra_cloud_model::resources::Port>,
) -> Vec<Named> {
    let mut out = Vec::new();
    for instance in instances {
        if instance.meta.is_deleting() {
            continue;
        }
        let hostname = instance.meta.name.id().to_string();
        for port in &instance.spec.ports {
            let Some(port) = ports.get(port.as_str()) else {
                continue;
            };
            let Some(address) = port.spec.address.as_deref() else {
                continue;
            };
            let Some(address) = address
                .split('/')
                .next()
                .and_then(|a| a.parse::<IpAddr>().ok())
            else {
                continue;
            };
            out.push(Named {
                hostname: hostname.clone(),
                subnet: port.spec.subnet.clone(),
                address,
            });
        }
    }
    out
}

/// Everything this responder knows: names to addresses, and back.
///
/// Built per subnet, because that is the scope a name has. Two projects may
/// each have a `web`, and each must see their own — answering across subnets
/// would also let one tenant enumerate another's machines by guessing names.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Zone {
    pub forward: BTreeMap<String, Vec<IpAddr>>,
    pub reverse: BTreeMap<String, String>,
}

impl Zone {
    /// The answer to one question, or `None` when this is not ours to answer.
    pub fn look_up(&self, question: &Question) -> Option<(Vec<IpAddr>, Vec<String>)> {
        if question.class != IN {
            return None;
        }
        match question.kind {
            A | AAAA => {
                let want_v4 = question.kind == A;
                let found = self.forward.get(&question.name)?;
                // A name this cell knows answers for both families, with
                // whichever addresses it has of the family that was asked for.
                // An empty answer with no error is the right shape: the name
                // exists, this family does not.
                Some((
                    found
                        .iter()
                        .filter(|a| a.is_ipv4() == want_v4)
                        .copied()
                        .collect(),
                    Vec::new(),
                ))
            }
            PTR => {
                let name = self.reverse.get(&question.name)?;
                Some((Vec::new(), vec![name.clone()]))
            }
            // A kind this does not serve, for a name it does know: answered
            // empty rather than forwarded, because forwarding a question about
            // a private name leaks it to the internet.
            _ if self.forward.contains_key(&question.name) => Some((Vec::new(), Vec::new())),
            _ => None,
        }
    }

    /// Whether a name belongs to this cell at all, however it was asked about.
    pub fn holds(&self, name: &str) -> bool {
        self.forward.contains_key(name) || self.reverse.contains_key(name)
    }
}

/// Where questions this cell cannot answer are sent.
///
/// The node's own resolvers, read once at startup from `/etc/resolv.conf`. A
/// node with none forwards nothing and says so on every miss rather than
/// pretending the name does not exist: "there is no such host" and "I was
/// never told where to ask" are different answers, and a guest that cannot
/// tell them apart is a guest whose operator debugs the wrong thing.
pub fn upstreams_from_resolv_conf(text: &str) -> Vec<IpAddr> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let rest = line.strip_prefix("nameserver")?;
            rest.trim().parse::<IpAddr>().ok()
        })
        // A node whose resolver is itself would loop: the address this
        // responder answers on is the guest's gateway, and a node that put its
        // own loopback here would ask itself for ever.
        .filter(|a| !a.is_loopback())
        .collect()
}

/// One responder, on one device, for the subnets reachable through it.
pub struct Responder {
    pub zone_suffix: String,
    pub upstreams: Vec<IpAddr>,
    /// Who is asking — this node's own guests, by address.
    pub guests: GuestRegistry,
    /// What there is to answer with — every guest in the cell.
    pub names: Names,
}

impl Responder {
    /// The subnets whose names this asker may see: the ones its own interfaces
    /// are on. A name is a tenant's, so answering across subnets would let one
    /// project enumerate another's machines by guessing.
    pub fn subnets_of(&self, asker: IpAddr) -> Vec<String> {
        match self.guests.at_address(asker) {
            Some((view, _)) => {
                let mut out: Vec<String> =
                    view.interfaces.iter().map(|i| i.subnet.clone()).collect();
                out.sort();
                out.dedup();
                out
            }
            None => Vec::new(),
        }
    }

    /// Answer one packet, forwarding what is not ours.
    pub async fn respond(&self, packet: &[u8], subnets: &[String]) -> Option<Vec<u8>> {
        let Some((id, question)) = parse_query(packet) else {
            // Not a shape this understands. Forwarded whole, because a
            // resolver that dropped what it could not parse would be a
            // resolver that broke DNSSEC, EDNS and every future record type.
            return self.forward(packet).await;
        };
        for subnet in subnets {
            let zone = self.names.zone(subnet, &self.zone_suffix);
            if let Some((addresses, names)) = zone.look_up(&question) {
                return Some(answer(id, &question, &addresses, &names));
            }
            // A name under this cell's own suffix that nobody here holds is
            // *not* forwarded: it is this cell's zone, so the answer is that
            // there is no such host.
            if question.name.ends_with(&self.zone_suffix) && !zone.holds(&question.name) {
                return Some(answer(id, &question, &[], &[]));
            }
        }
        self.forward(packet).await
    }

    /// Ask the node's own resolvers, and hand back whatever they say.
    async fn forward(&self, packet: &[u8]) -> Option<Vec<u8>> {
        for upstream in &self.upstreams {
            let bind: SocketAddr = if upstream.is_ipv4() {
                (Ipv4Addr::UNSPECIFIED, 0).into()
            } else {
                (Ipv6Addr::UNSPECIFIED, 0).into()
            };
            let Ok(socket) = tokio::net::UdpSocket::bind(bind).await else {
                continue;
            };
            if socket.send_to(packet, (*upstream, PORT)).await.is_err() {
                continue;
            }
            let mut buffer = vec![0u8; 4096];
            match tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buffer)).await {
                Ok(Ok(n)) => {
                    buffer.truncate(n);
                    return Some(buffer);
                }
                // This one did not answer. The next one might, and a guest
                // waiting on a resolver that is down is the failure this loop
                // exists to avoid.
                _ => continue,
            }
        }
        None
    }
}

/// Answer DNS at the metadata address, for every subnet this node holds.
///
/// **Not** on the guest-facing bridge, and the difference matters: a guest
/// sends its query to whatever address DHCP named, and the gateway of a
/// fabric cell is the fabric rather than this node — a socket bound to the
/// tap would never see the packet, because the kernel has no local address to
/// deliver it to. `169.254.169.254` is link-local, is always the node the
/// guest is on, and is already the one address every guest can reach. It is
/// the same argument the DHCP responder makes for its `fallback_id`.
///
/// One socket for the whole node, therefore, and the subnet a query is about
/// is worked out from who asked: the sender's address names a guest, and that
/// guest's subnets are the ones whose names it may see.
pub async fn serve(
    guests: GuestRegistry,
    names: Names,
    listen_on: SocketAddr,
    zone_suffix: String,
    upstreams: Vec<IpAddr>,
    every: Duration,
) {
    loop {
        match tokio::net::UdpSocket::bind(listen_on).await {
            Ok(socket) => {
                tracing::info!(%listen_on, "answering DNS");
                let responder = Responder {
                    zone_suffix: zone_suffix.clone(),
                    upstreams: upstreams.clone(),
                    guests: guests.clone(),
                    names: names.clone(),
                };
                listen(Arc::new(socket), responder).await;
                tracing::warn!(%listen_on, "the DNS socket stopped; binding again");
            }
            Err(e) => {
                // The address comes up with the first guest's tap, so a node
                // with none yet cannot bind. Retried rather than fatal, and
                // logged at debug so a quiet node is quiet.
                tracing::debug!(%listen_on, error = %e, "cannot answer DNS yet");
            }
        }
        tokio::time::sleep(every).await;
    }
}

async fn listen(socket: Arc<tokio::net::UdpSocket>, responder: Responder) {
    let mut buffer = vec![0u8; 4096];
    loop {
        let (n, from) = match socket.recv_from(&mut buffer).await {
            Ok(got) => got,
            Err(e) => {
                tracing::warn!(error = %e, "a DNS socket stopped answering");
                return;
            }
        };
        // Which names this asker may see: the subnets of the guest it is.
        // A packet from an address no guest holds gets the forwarder and none
        // of the cell's own names, which is the right answer for something
        // this node cannot identify.
        let subnets = responder.subnets_of(from.ip());
        if let Some(reply) = responder.respond(&buffer[..n], &subnets).await
            && let Err(e) = socket.send_to(&reply, from).await
        {
            tracing::debug!(error = %e, "a DNS answer could not be sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut out = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        write_name(&mut out, name);
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&IN.to_be_bytes());
        out
    }

    #[test]
    fn a_query_is_read_and_an_answer_is_written() {
        let packet = query("db-1.velstra.internal", A);
        let (id, question) = parse_query(&packet).expect("a plain query is readable");
        assert_eq!(id, 0x1234);
        assert_eq!(question.name, "db-1.velstra.internal");
        assert_eq!(question.kind, A);

        let reply = answer(id, &question, &["10.0.0.5".parse().unwrap()], &[]);
        assert_eq!(u16::from_be_bytes([reply[0], reply[1]]), id);
        // Response, authoritative, no error.
        assert_eq!(reply[2] & 0x80, 0x80, "not marked as a response");
        assert_eq!(reply[3] & 0x0f, 0, "an answer was marked as an error");
        assert_eq!(
            u16::from_be_bytes([reply[6], reply[7]]),
            1,
            "no answer record"
        );
        // And the address is in it, in the last four bytes of an A record.
        assert_eq!(&reply[reply.len() - 4..], &[10, 0, 0, 5]);
    }

    #[test]
    fn a_name_nobody_holds_is_a_name_that_does_not_exist() {
        let packet = query("nothing.velstra.internal", A);
        let (id, question) = parse_query(&packet).unwrap();
        let reply = answer(id, &question, &[], &[]);
        assert_eq!(reply[3] & 0x0f, 3, "an unknown name was not NXDOMAIN");
    }

    #[test]
    fn a_response_is_never_taken_for_a_query() {
        // Otherwise a reply bounced back at this port would be answered, which
        // is how a resolver becomes an amplifier.
        let mut packet = query("db-1", A);
        packet[2] |= 0x80;
        assert!(parse_query(&packet).is_none());
    }

    /// A name that resolved only for guests sharing a hypervisor would be a
    /// name nobody could rely on: the first thing anybody tries is reaching
    /// the machine next door, and that is usually on another node.
    #[test]
    fn the_index_is_the_whole_cell_and_a_zone_is_one_subnet() {
        let names = Names::new();
        names.replace(vec![
            Named {
                hostname: "web".into(),
                subnet: "projects/p1/subnets/s1".into(),
                address: "10.0.0.7".parse().unwrap(),
            },
            Named {
                hostname: "db".into(),
                subnet: "projects/p1/subnets/s1".into(),
                address: "10.0.0.8".parse().unwrap(),
            },
            // Another tenant, another subnet. Never visible from the first.
            Named {
                hostname: "web".into(),
                subnet: "projects/p2/subnets/s1".into(),
                address: "10.9.9.9".parse().unwrap(),
            },
        ]);

        let zone = names.zone("projects/p1/subnets/s1", DEFAULT_ZONE);
        assert_eq!(
            zone.forward.get("web").map(|a| a.as_slice()),
            Some(["10.0.0.7".parse::<IpAddr>().unwrap()].as_slice())
        );
        assert!(zone.forward.contains_key("db.velstra.internal"));

        // The other project's `web` is its own, and is not reachable from here.
        let theirs = names.zone("projects/p2/subnets/s1", DEFAULT_ZONE);
        assert_eq!(
            theirs.forward.get("web").map(|a| a.as_slice()),
            Some(["10.9.9.9".parse::<IpAddr>().unwrap()].as_slice()),
            "two tenants may each have a `web`, and each must see their own"
        );
    }

    #[test]
    fn the_zone_answers_forwards_and_backwards() {
        let mut zone = Zone::default();
        zone.forward
            .insert("web".into(), vec!["10.0.0.7".parse().unwrap()]);
        zone.forward.insert(
            "web.velstra.internal".into(),
            vec!["10.0.0.7".parse().unwrap()],
        );
        zone.reverse.insert(
            reverse_name("10.0.0.7".parse().unwrap()),
            "web.velstra.internal".into(),
        );

        let (found, _) = zone
            .look_up(&Question {
                name: "web".into(),
                kind: A,
                class: IN,
            })
            .expect("a guest's short name is answered");
        assert_eq!(found, vec!["10.0.0.7".parse::<IpAddr>().unwrap()]);

        let (_, names) = zone
            .look_up(&Question {
                name: reverse_name("10.0.0.7".parse().unwrap()),
                kind: PTR,
                class: IN,
            })
            .expect("the address answers backwards");
        assert_eq!(names, vec!["web.velstra.internal".to_string()]);

        // A name this zone does not hold is not this zone's to answer.
        assert!(
            zone.look_up(&Question {
                name: "deb.debian.org".into(),
                kind: A,
                class: IN
            })
            .is_none()
        );

        // A name it does hold, asked for a kind it does not serve, is answered
        // empty rather than forwarded — forwarding would leak a private name.
        let (addresses, names) = zone
            .look_up(&Question {
                name: "web".into(),
                kind: 15,
                class: IN,
            })
            .expect("a known name is not forwarded");
        assert!(addresses.is_empty() && names.is_empty());
    }

    #[test]
    fn the_nodes_own_resolvers_are_read_and_loopback_is_not_one() {
        let text = "# Generated\nnameserver 1.1.1.1\nnameserver 127.0.0.53\nsearch lan\nnameserver 9.9.9.9 # secondary\n";
        assert_eq!(
            upstreams_from_resolv_conf(text),
            vec![
                "1.1.1.1".parse::<IpAddr>().unwrap(),
                "9.9.9.9".parse::<IpAddr>().unwrap()
            ],
            "a node that forwarded to its own stub resolver would ask itself for ever"
        );
    }

    #[test]
    fn a_reverse_name_is_spelled_the_way_a_resolver_asks_for_it() {
        assert_eq!(
            reverse_name("10.19.136.2".parse().unwrap()),
            "2.136.19.10.in-addr.arpa"
        );
    }
}
