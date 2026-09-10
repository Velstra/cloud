//! Security-group rules as nftables, for the datapath that has no fabric.
//!
//! Until this existed, a cell on the local datapath could not have a firewall
//! at all. The tap datapath refused a port carrying rules — honestly, with a
//! sentence, which is better than accepting them and filtering nothing — but
//! the consequence was that a home or single-node cell had one choice: no
//! rules, or no guests.
//!
//! The kernel already has what is needed: every guest holds an address this
//! node handed out, so a `forward` chain matching on addresses is precisely a
//! per-port firewall, and nftables counts what it drops without being asked
//! twice.
//!
//! **On the address, not on the interface.** The first shape matched
//! `iifname`/`oifname` against the guest's tap, which is what a tap datapath
//! makes it tempting to do — and it matched nothing at all. A guest's tap is
//! enslaved to a bridge, the routing decision is taken on the bridge, and the
//! interface the `forward` hook sees is therefore the bridge. Every guest on a
//! segment shares it. The address does not: it is the one thing that
//! identifies a port to the kernel at the layer this hook runs at.
//!
//! **Default deny, both directions**, once a port has any rule at all. A port
//! with no rules is unfiltered, which is what it was before and what a cell
//! with no security groups expects. The moment a port carries one rule, that
//! rule is the whole of what is allowed — which is what a security group
//! means everywhere else, and the only reading under which adding a rule
//! cannot silently widen anything.
//!
//! Established traffic comes back. A rule saying "TCP 443 from anywhere" is a
//! rule about who may *start* a conversation; making the answer to an
//! allowed request depend on a second rule in the other direction is how
//! everybody's first firewall breaks.
//!
//! What is deliberately not here:
//!
//! * **Rules the model already refuses.** `protocol: any`, TCP or UDP without
//!   ports, and ranges over sixty-four ports are turned away where they are
//!   written — see [`velstra_cloud_model::security::programmable`] — so
//!   nothing in this file has to decide what to do with them.
//! * **Logging every packet.** A counter per rule says how much matched; a log
//!   line per packet is a way to fill a disk during an incident.

use std::collections::BTreeMap;

use velstra_cloud_model::security::{Direction, Protocol, ResolvedRule};

/// The table this module owns, and nothing else writes.
pub const TABLE: &str = "velstra-filter";

/// One port, its addresses, and what it is allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guarded {
    /// The port's full name, which is what a counter is reported against.
    pub port: String,
    /// The tap this node made for it. Not matched on — see the module doc —
    /// but it is what the chain is named after, because it is short, unique
    /// and stable across passes.
    pub tap: String,
    /// The MAC the platform gave this port, when it chose one.
    ///
    /// Not used by the firewall — it matches addresses — but carried here so
    /// one description of a port serves both tables: `antispoof` binds the
    /// address to the wire it may arrive on, and doing that from a second,
    /// separately-built list is how the two drift.
    pub mac: Option<String>,
    /// The addresses the guest holds. Both families where it has both.
    ///
    /// A port with none is left unfiltered even when it has rules: there is
    /// nothing to match on, and a chain that matched nothing would read as a
    /// firewall while filtering nobody. The next pass, once DHCP has settled,
    /// writes the real thing.
    pub addresses: Vec<String>,
    /// What the port may do. Empty leaves the port unfiltered.
    pub rules: Vec<ResolvedRule>,
}

/// The whole ruleset, computed from nothing but the ports.
///
/// Written whole and replaced atomically — `add` then `delete` then the body,
/// which is the idiom that leaves no window in which a guest is unfiltered.
/// The same shape [`crate::localnet::LocalNet::ruleset`] uses, for the same
/// reason: a ruleset assembled by adding and removing individual rules is one
/// whose state depends on every pass that came before.
pub fn ruleset(guarded: &[Guarded]) -> String {
    let filtered: Vec<&Guarded> = guarded
        .iter()
        .filter(|g| !g.rules.is_empty() && !g.addresses.is_empty())
        .collect();
    let mut out = String::new();
    out.push_str(&format!("add table inet {TABLE}\n"));
    out.push_str(&format!("delete table inet {TABLE}\n"));
    out.push_str(&format!("table inet {TABLE} {{\n"));
    // Priority just below the fabric's would-be place and above nothing else
    // on the machine: `filter` is where a forward decision belongs, and the
    // NAT table this node also writes is on a different hook entirely.
    out.push_str(
        "  chain forward {\n    type filter hook forward priority filter; policy accept;\n",
    );
    if filtered.is_empty() {
        out.push_str("  }\n}\n");
        return out;
    }
    // The answer to something this node already allowed. Before the per-port
    // chains, because a reply must not have to match an egress rule somebody
    // did not think to write.
    out.push_str("    ct state established,related counter accept\n");
    for g in &filtered {
        // Ingress is "towards the guest", so the guest is the destination.
        // Egress is the other way. Naming them from the guest's point of view
        // is what a person writing a rule means.
        for address in &g.addresses {
            let family = family_of(address);
            out.push_str(&format!(
                "    {family} daddr {address} jump {}\n",
                chain(&g.tap, Direction::Ingress)
            ));
            out.push_str(&format!(
                "    {family} saddr {address} jump {}\n",
                chain(&g.tap, Direction::Egress)
            ));
        }
    }
    out.push_str("  }\n");
    for g in &filtered {
        for direction in [Direction::Ingress, Direction::Egress] {
            out.push_str(&format!("  chain {} {{\n", chain(&g.tap, direction)));
            for rule in g.rules.iter().filter(|r| r.direction == direction) {
                if let Some(line) = matcher(rule, direction) {
                    out.push_str(&format!("    {line} counter accept\n"));
                }
            }
            // The deny, counted. This is the number that answers "what was
            // dropped", and it is per port and per direction because that is
            // the granularity somebody debugging has a question about.
            out.push_str("    counter drop\n");
            out.push_str("  }\n");
        }
    }
    out.push_str("}\n");
    out
}

/// `ip` or `ip6`, from an address or prefix.
fn family_of(address: &str) -> &'static str {
    if address.contains(':') { "ip6" } else { "ip" }
}

/// The chain name for one tap and one direction.
///
/// Derived from the tap, which is itself derived from the port name, so the
/// same port is the same chain on every pass and nothing has to be remembered.
pub fn chain(tap: &str, direction: Direction) -> String {
    let side = match direction {
        Direction::Ingress => "in",
        Direction::Egress => "out",
    };
    format!("{tap}-{side}")
}

/// One rule as an nftables match, or `None` for one that cannot be expressed.
///
/// `None` is not silence: the caller has already refused unprogrammable rules
/// at the API, so reaching it means the model and this file disagree, and
/// dropping the rule is the safe direction — it narrows, never widens.
fn matcher(rule: &ResolvedRule, direction: Direction) -> Option<String> {
    let family = if rule.remote.contains(':') {
        "ip6"
    } else {
        "ip"
    };
    // The remote is where the *other* end is, which is the source for traffic
    // coming towards the guest and the destination for traffic leaving it.
    let side = match direction {
        Direction::Ingress => "saddr",
        Direction::Egress => "daddr",
    };
    let mut parts = Vec::new();
    // `0.0.0.0/0` and `::/0` are "anywhere", and writing them out as a match
    // would refuse the other family for no reason.
    if !matches!(rule.remote.as_str(), "0.0.0.0/0" | "::/0") {
        parts.push(format!("{family} {side} {}", rule.remote));
    }
    match rule.protocol {
        Protocol::Tcp | Protocol::Udp => {
            let proto = if rule.protocol == Protocol::Tcp {
                "tcp"
            } else {
                "udp"
            };
            let ports = rule.ports?;
            // `dport` in both directions, and that is not an oversight. A
            // port number in a security rule always names the service being
            // reached: ingress 443 is somebody connecting *to* the guest's
            // 443, egress 443 is the guest connecting *to* somebody's 443.
            // Both are the destination port of the packet being judged. The
            // address is the field that flips, because that one really does
            // mean "the other end".
            if ports.from == ports.to {
                parts.push(format!("{proto} dport {}", ports.from));
            } else {
                parts.push(format!("{proto} dport {}-{}", ports.from, ports.to));
            }
        }
        Protocol::Icmp => {
            // Both families, and named separately because nftables does: a
            // cell that is dual-stack has guests that are pinged over both.
            parts.push(if family == "ip6" {
                "meta l4proto ipv6-icmp".to_string()
            } else {
                "meta l4proto icmp".to_string()
            });
        }
        // Refused where it is written; see the module doc.
        Protocol::Any => return None,
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// What each port's chains have counted, read from `nft -j list table`.
///
/// Keyed by port name, and by direction inside that, so the answer to "what is
/// being dropped on this guest" is one lookup. Missing rather than zero for a
/// port with no chains: nothing has been counted because nothing is filtering,
/// and reporting zero would say the opposite.
pub fn counters(listing: &serde_json::Value, guarded: &[Guarded]) -> BTreeMap<String, Dropped> {
    let by_chain: BTreeMap<String, (u64, u64)> = listing
        .get("nftables")
        .and_then(|n| n.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let rule = entry.get("rule")?;
            let chain = rule.get("chain")?.as_str()?.to_string();
            // The drop is the rule with no match expression before the verdict
            // — the last one in every chain this file writes.
            let expr = rule.get("expr")?.as_array()?;
            let drops = expr.iter().any(|e| e.get("drop").is_some());
            if !drops {
                return None;
            }
            let counter = expr.iter().find_map(|e| e.get("counter"))?;
            Some((
                chain,
                (
                    counter.get("packets")?.as_u64()?,
                    counter.get("bytes")?.as_u64()?,
                ),
            ))
        })
        .collect();
    let mut out = BTreeMap::new();
    for g in guarded
        .iter()
        .filter(|g| !g.rules.is_empty() && !g.addresses.is_empty())
    {
        let inbound = by_chain.get(&chain(&g.tap, Direction::Ingress)).copied();
        let outbound = by_chain.get(&chain(&g.tap, Direction::Egress)).copied();
        if inbound.is_none() && outbound.is_none() {
            continue;
        }
        let (in_packets, in_bytes) = inbound.unwrap_or_default();
        let (out_packets, out_bytes) = outbound.unwrap_or_default();
        out.insert(
            g.port.clone(),
            Dropped {
                inbound_packets: in_packets,
                inbound_bytes: in_bytes,
                outbound_packets: out_packets,
                outbound_bytes: out_bytes,
            },
        );
    }
    out
}

/// What one port's firewall has turned away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub inbound_packets: u64,
    pub inbound_bytes: u64,
    pub outbound_packets: u64,
    pub outbound_bytes: u64,
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::security::PortRange;

    use super::*;

    fn rule(
        direction: Direction,
        protocol: Protocol,
        ports: Option<(u16, u16)>,
        remote: &str,
    ) -> ResolvedRule {
        ResolvedRule {
            direction,
            protocol,
            ports: ports.map(|(from, to)| PortRange { from, to }),
            remote: remote.to_string(),
        }
    }

    fn guarded(rules: Vec<ResolvedRule>) -> Guarded {
        Guarded {
            mac: None,
            port: "projects/p1/ports/web".into(),
            tap: "vt0web1a2b".into(),
            addresses: vec!["10.19.136.5".into()],
            rules,
        }
    }

    /// A port with no rules is not filtered — which is what it was before this
    /// existed, and what a cell with no security groups expects.
    #[test]
    fn a_port_with_no_rules_is_left_alone() {
        let text = ruleset(&[guarded(Vec::new())]);
        assert!(!text.contains("vt0web1a2b"), "{text}");
        assert!(text.contains("policy accept"), "{text}");
    }

    /// One rule makes that rule the whole of what is allowed. Anything else
    /// and adding a rule would silently widen a port.
    #[test]
    fn one_rule_makes_everything_else_a_drop() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])]);
        assert!(text.contains("tcp dport 443 counter accept"), "{text}");
        assert!(text.contains("counter drop"), "{text}");
    }

    /// The reply to something this node allowed comes back without needing a
    /// second rule. Everybody's first firewall breaks on this.
    #[test]
    fn the_answer_to_an_allowed_request_is_not_a_second_rule() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Egress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])]);
        assert!(
            text.contains("ct state established,related counter accept"),
            "{text}"
        );
    }

    /// Towards the guest is towards its address; away from it is away from
    /// its address. Getting this the wrong way round is a firewall that looks
    /// right and blocks everything.
    #[test]
    fn the_directions_are_named_from_the_guests_point_of_view() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((22, 22)),
            "10.0.0.0/8",
        )])]);
        assert!(
            text.contains("ip daddr 10.19.136.5 jump vt0web1a2b-in"),
            "{text}"
        );
        assert!(
            text.contains("ip saddr 10.19.136.5 jump vt0web1a2b-out"),
            "{text}"
        );
        assert!(text.contains("ip saddr 10.0.0.0/8 tcp dport 22"), "{text}");
    }

    #[test]
    fn a_v6_remote_is_matched_in_the_v6_family() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "fd00:19::/64",
        )])]);
        assert!(text.contains("ip6 saddr fd00:19::/64"), "{text}");
    }

    /// "From anywhere" is written out in the model and must not become a match
    /// on one family — a `0.0.0.0/0` rule that refused v6 would be a rule
    /// nobody wrote.
    #[test]
    fn anywhere_matches_both_families() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((80, 80)),
            "::/0",
        )])]);
        assert!(!text.contains("saddr ::/0"), "{text}");
        assert!(text.contains("tcp dport 80 counter accept"), "{text}");
    }

    #[test]
    fn a_range_is_a_range() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Udp,
            Some((30_000, 30_010)),
            "0.0.0.0/0",
        )])]);
        assert!(text.contains("udp dport 30000-30010"), "{text}");
    }

    #[test]
    fn icmp_is_named_per_family() {
        let v4 = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Icmp,
            None,
            "0.0.0.0/0",
        )])]);
        assert!(v4.contains("meta l4proto icmp"), "{v4}");
        let v6 = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Icmp,
            None,
            "fd00::/8",
        )])]);
        assert!(v6.contains("meta l4proto ipv6-icmp"), "{v6}");
    }

    /// Two passes over an unchanged world write the same bytes, which is what
    /// lets the caller tell "nothing to do" from "something moved".
    #[test]
    fn the_ruleset_is_stable() {
        let ports = vec![
            guarded(vec![rule(
                Direction::Ingress,
                Protocol::Tcp,
                Some((443, 443)),
                "0.0.0.0/0",
            )]),
            Guarded {
                mac: None,
                port: "projects/p1/ports/db".into(),
                tap: "vt0db99ff".into(),
                addresses: vec!["10.19.136.6".into()],
                rules: vec![rule(
                    Direction::Ingress,
                    Protocol::Tcp,
                    Some((5432, 5432)),
                    "10.0.0.0/8",
                )],
            },
        ];
        assert_eq!(ruleset(&ports), ruleset(&ports));
    }

    #[test]
    fn what_was_dropped_is_read_back_per_port_and_direction() {
        let listing = serde_json::json!({"nftables": [
            {"rule": {"chain": "vt0web1a2b-in", "expr": [
                {"counter": {"packets": 12, "bytes": 800}}, {"drop": null}]}},
            {"rule": {"chain": "vt0web1a2b-out", "expr": [
                {"counter": {"packets": 3, "bytes": 200}}, {"drop": null}]}},
            // An accept's counter is not a drop and must not be read as one.
            {"rule": {"chain": "vt0web1a2b-in", "expr": [
                {"match": {}}, {"counter": {"packets": 900, "bytes": 90000}}, {"accept": null}]}},
        ]});
        let ports = [guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])];
        let seen = counters(&listing, &ports);
        assert_eq!(
            seen.get("projects/p1/ports/web").copied(),
            Some(Dropped {
                inbound_packets: 12,
                inbound_bytes: 800,
                outbound_packets: 3,
                outbound_bytes: 200,
            })
        );
    }

    /// A port nothing is filtering reports nothing, rather than zero: zero
    /// would say "nothing was dropped", and the truth is "nothing was judged".
    #[test]
    fn a_port_with_no_chains_reports_nothing_rather_than_zero() {
        let ports = [guarded(Vec::new())];
        assert!(counters(&serde_json::json!({"nftables": []}), &ports).is_empty());
    }
}
