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
//! **Deny per direction, once a port has a rule in that direction.** A port
//! with no rules at all is unfiltered. A port that carries ingress rules is
//! denied inbound except for those; a port that carries egress rules is denied
//! outbound except for those. A direction nobody has written a rule for is
//! open, which is the platform's stated stance — `velstra_cloud_model::security`
//! says it in as many words, and the REST contract publishes it as something a
//! client may rely on.
//!
//! It used to deny both directions the moment a port carried any rule at all,
//! which meant a port with one ingress rule got an egress chain whose only
//! entry was the drop: every guest with a security group could be reached and
//! could reach nothing. That is the failure the model's own doc argues
//! against — "a guest that cannot reach anything cannot finish its own boot,
//! and a platform whose default leaves every new instance broken teaches
//! people to attach a permit-everything group and stop thinking about it".
//!
//! The property that reading was protecting survives intact: adding a rule
//! still cannot widen anything. An egress rule added to a port that had none
//! turns egress from open into that-rule-only, which narrows; the same the
//! other way round.
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

/// The same rules, in the `bridge` family — where two guests on one bridge
/// actually meet.
///
/// The `inet` table's `forward` hook is traversed only for packets the host
/// *routes*. Two guests on one segment share a bridge, and a frame from one to
/// the other is switched, never routed: it goes past that hook without being
/// judged — unless `br_netfilter` happens to be loaded, which a machine that
/// also runs Docker has and a bare one does not. A security property that
/// depends on what else is installed is not a security property, and the
/// console showed a firewall that did not apply to the guest next door.
///
/// So the per-port rules are written a second time here, entered by interface
/// rather than by address, because the bridge family sees the real ingress
/// and egress port of a frame. After the anti-spoof table the tap and the
/// address are bound to each other, so the two tables agree by construction.
///
/// **Only written where it is true**, because a bridge-family `ct state` is
/// tracked only when `nf_conntrack_bridge` is loaded — and without it every
/// frame is `untracked`, the established/related accept never matches, and
/// every reply to an allowed request is dropped. That is a whole-cell outage,
/// not a partial one. `localnet` verifies the module before writing this and
/// says so on the port when it cannot.
pub const BRIDGE_TABLE: &str = "velstra-filter-bridge";

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
            for direction in [Direction::Ingress, Direction::Egress] {
                // **Only the direction this port actually has rules in.** No
                // jump means no chain means no drop: the direction is carried
                // by the `forward` policy above, which accepts. A chain with
                // nothing in it but its own drop is a closed direction nobody
                // asked to close.
                if !has_rules(g, direction) {
                    continue;
                }
                let which = match direction {
                    Direction::Ingress => "daddr",
                    Direction::Egress => "saddr",
                };
                out.push_str(&format!(
                    "    {family} {which} {address} jump {}\n",
                    chain(&g.tap, direction)
                ));
            }
        }
    }
    out.push_str("  }\n");
    for g in &filtered {
        for direction in [Direction::Ingress, Direction::Egress] {
            if !has_rules(g, direction) {
                continue;
            }
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

/// The bridge-family half of the firewall. See [`BRIDGE_TABLE`].
///
/// Same `matcher`, same per-direction chains, same terminal drop — with three
/// differences that are the whole point: the jump is on `iifname`/`oifname`
/// rather than on the address; each jump is guarded by `meta protocol` so an
/// ARP frame is never judged by an IP rule; and the chains carry their own
/// suffix (`-bin`/`-bout`) so `counters` keeps the two tables' drops apart.
pub fn ruleset_bridge(guarded: &[Guarded]) -> String {
    let filtered: Vec<&Guarded> = guarded
        .iter()
        .filter(|g| !g.rules.is_empty() && !g.addresses.is_empty() && !g.tap.is_empty())
        .collect();
    let mut out = String::new();
    out.push_str(&format!("add table bridge {BRIDGE_TABLE}\n"));
    out.push_str(&format!("delete table bridge {BRIDGE_TABLE}\n"));
    out.push_str(&format!("table bridge {BRIDGE_TABLE} {{\n"));
    out.push_str(
        "  chain forward {\n    type filter hook forward priority filter; policy accept;\n",
    );
    if filtered.is_empty() {
        out.push_str("  }\n}\n");
        return out;
    }
    out.push_str("    ct state established,related counter accept\n");
    for g in &filtered {
        for direction in [Direction::Ingress, Direction::Egress] {
            if !has_rules(g, direction) {
                continue;
            }
            // Towards the guest is *out of* the bridge into its tap; away
            // from it is *in from* its tap.
            let which = match direction {
                Direction::Ingress => "oifname",
                Direction::Egress => "iifname",
            };
            for family in ["ip", "ip6"] {
                // Only the families this port holds an address in, so a
                // v4-only guest is not sent through a chain that judges
                // nothing of its own.
                if !g.addresses.iter().any(|a| family_of(a) == family) {
                    continue;
                }
                out.push_str(&format!(
                    "    {which} \"{}\" meta protocol {family} jump {}\n",
                    g.tap,
                    bridge_chain(&g.tap, direction)
                ));
            }
        }
    }
    out.push_str("  }\n");
    for g in &filtered {
        for direction in [Direction::Ingress, Direction::Egress] {
            if !has_rules(g, direction) {
                continue;
            }
            out.push_str(&format!("  chain {} {{\n", bridge_chain(&g.tap, direction)));
            for rule in g.rules.iter().filter(|r| r.direction == direction) {
                if let Some(line) = matcher(rule, direction) {
                    out.push_str(&format!("    {line} counter accept\n"));
                }
            }
            out.push_str("    counter drop\n");
            out.push_str("  }\n");
        }
    }
    out.push_str("}\n");
    out
}

/// The bridge-family chain for one tap and one direction. A different suffix
/// from [`chain`], so a listing of both tables never counts one drop twice.
pub fn bridge_chain(tap: &str, direction: Direction) -> String {
    let side = match direction {
        Direction::Ingress => "bin",
        Direction::Egress => "bout",
    };
    format!("{tap}-{side}")
}

/// Whether this port has anything to say about that direction.
///
/// The whole of the per-direction stance: a direction with a rule is denied
/// except for it, and a direction with none is left to the `forward` policy,
/// which accepts.
fn has_rules(g: &Guarded, direction: Direction) -> bool {
    g.rules.iter().any(|r| r.direction == direction)
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
            // **The family follows the rule, not the remote string.**
            //
            // Both families are named separately because nftables does: a cell
            // that is dual-stack has guests that are pinged over both. The
            // choice used to be made from whether the *remote* contained a
            // colon — which is the wrong fact. A remote that is a real prefix
            // does settle it: `10.0.0.0/8` can only be v4. But `0.0.0.0/0` is
            // how "allow ping from anywhere" is written, and it is deliberately
            // a statement about both families (the address match above is
            // dropped for exactly that reason) — so it emitted `icmp` alone,
            // and every ICMPv6 to that guest fell through to the terminal drop.
            //
            // One set rather than two rules, so "what matched" stays one number
            // per rule and `counters()` keeps its shape.
            parts.push(match rule.remote.as_str() {
                "0.0.0.0/0" | "::/0" => "meta l4proto { icmp, ipv6-icmp }".to_string(),
                _ if family == "ip6" => "meta l4proto ipv6-icmp".to_string(),
                _ => "meta l4proto icmp".to_string(),
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
        // Both tables, summed: a drop is a drop whichever hook judged the
        // frame, and the question this answers — "why can my guest not reach
        // that" — does not care whether the other end was routed or switched.
        let sum = |a: Option<(u64, u64)>, b: Option<(u64, u64)>| -> Option<(u64, u64)> {
            match (a, b) {
                (None, None) => None,
                (x, y) => {
                    let (xp, xb) = x.unwrap_or_default();
                    let (yp, yb) = y.unwrap_or_default();
                    Some((xp + yp, xb + yb))
                }
            }
        };
        let inbound = sum(
            by_chain.get(&chain(&g.tap, Direction::Ingress)).copied(),
            by_chain
                .get(&bridge_chain(&g.tap, Direction::Ingress))
                .copied(),
        );
        let outbound = sum(
            by_chain.get(&chain(&g.tap, Direction::Egress)).copied(),
            by_chain
                .get(&bridge_chain(&g.tap, Direction::Egress))
                .copied(),
        );
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
        let bridged = ruleset_bridge(&[guarded(Vec::new())]);
        assert!(!bridged.contains("vt0web1a2b"), "{bridged}");
        assert!(bridged.contains("policy accept"), "{bridged}");
    }

    /// **A guest on the same segment is judged by the same rules as the world.**
    ///
    /// The `inet` forward hook sees only what the host routes; two guests on
    /// one bridge are switched past it. The bridge-family table enters the
    /// port's chains by interface, where the frame actually is.
    #[test]
    fn a_guest_on_the_same_segment_is_judged_by_the_same_rules_as_the_world() {
        let text = ruleset_bridge(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])]);
        assert!(
            text.contains("table bridge velstra-filter-bridge"),
            "{text}"
        );
        assert!(
            text.contains("oifname \"vt0web1a2b\" meta protocol ip jump vt0web1a2b-bin"),
            "{text}"
        );
        assert!(text.contains("tcp dport 443 counter accept"), "{text}");
        // No egress rule: no egress chain, no egress jump — the same
        // per-direction stance the inet table takes.
        assert!(!text.contains("vt0web1a2b-bout"), "{text}");
        // And a v4-only guest is not sent through a v6 chain of nothing.
        assert!(!text.contains("meta protocol ip6"), "{text}");
        // Replaced whole, like every table here.
        assert!(
            text.contains("delete table bridge velstra-filter-bridge"),
            "{text}"
        );
    }

    /// The two tables' drops are read as one number per port and direction:
    /// the chains carry different suffixes so nothing is counted twice, and the
    /// sum is what "why can my guest not reach that" wants.
    #[test]
    fn drops_from_both_tables_are_read_as_one_per_port() {
        let listing = serde_json::json!({"nftables": [
            {"rule": {"chain": "vt0web1a2b-in", "expr": [
                {"counter": {"packets": 12, "bytes": 800}}, {"drop": null}]}},
            {"rule": {"chain": "vt0web1a2b-bin", "expr": [
                {"counter": {"packets": 5, "bytes": 300}}, {"drop": null}]}},
        ]});
        let ports = [guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])];
        let got = counters(&listing, &ports);
        let d = got.get("projects/p1/ports/web").expect("the port");
        assert_eq!(d.inbound_packets, 17);
        assert_eq!(d.inbound_bytes, 1100);
        assert_eq!(d.outbound_packets, 0);
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
        // A rule each way, so both chains exist and both jumps can be read.
        let text = ruleset(&[guarded(vec![
            rule(
                Direction::Ingress,
                Protocol::Tcp,
                Some((22, 22)),
                "10.0.0.0/8",
            ),
            rule(
                Direction::Egress,
                Protocol::Tcp,
                Some((53, 53)),
                "0.0.0.0/0",
            ),
        ])]);
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

    /// **A port with only ingress rules still reaches the world.**
    ///
    /// The platform's stance is written down twice — in
    /// `velstra_cloud_model::security` and in the REST contract, as something a
    /// client may rely on — and this table said the opposite: any rule at all
    /// closed *both* directions, so a guest with a security group could be
    /// reached and could reach nothing. It could not resolve a name, fetch a
    /// package, or finish its own cloud-init.
    #[test]
    fn a_port_with_only_ingress_rules_still_reaches_the_world() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])]);
        assert!(
            text.contains("ip daddr 10.19.136.5 jump vt0web1a2b-in"),
            "the ingress rule was not programmed: {text}"
        );
        assert!(
            !text.contains("jump vt0web1a2b-out"),
            "a port with no egress rule was sent into an egress chain: {text}"
        );
        assert!(
            !text.contains("chain vt0web1a2b-out"),
            "an egress chain was written whose only rule is the drop: {text}"
        );
    }

    /// And the other way round: an egress-only port is closed outbound except
    /// for what it names, and left open inbound.
    #[test]
    fn a_port_with_only_egress_rules_is_not_closed_inbound() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Egress,
            Protocol::Udp,
            Some((53, 53)),
            "0.0.0.0/0",
        )])]);
        assert!(
            text.contains("chain vt0web1a2b-out"),
            "the egress rule was not programmed: {text}"
        );
        assert!(
            !text.contains("chain vt0web1a2b-in"),
            "an ingress chain was written whose only rule is the drop: {text}"
        );
    }

    /// Adding a rule still cannot widen anything — the property the
    /// both-directions reading was protecting.
    #[test]
    fn adding_an_egress_rule_narrows_egress_rather_than_widening_it() {
        let open = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((443, 443)),
            "0.0.0.0/0",
        )])]);
        let narrowed = ruleset(&[guarded(vec![
            rule(
                Direction::Ingress,
                Protocol::Tcp,
                Some((443, 443)),
                "0.0.0.0/0",
            ),
            rule(
                Direction::Egress,
                Protocol::Tcp,
                Some((53, 53)),
                "0.0.0.0/0",
            ),
        ])]);
        assert!(!open.contains("chain vt0web1a2b-out"), "{open}");
        assert!(narrowed.contains("chain vt0web1a2b-out"), "{narrowed}");
        assert!(
            narrowed.contains("counter drop"),
            "the new egress chain does not end in a drop: {narrowed}"
        );
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
        // A real prefix settles the family: `10.0.0.0/8` can only be v4.
        // "Anywhere" does not, and has its own test.
        let v4 = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Icmp,
            None,
            "10.0.0.0/8",
        )])]);
        assert!(v4.contains("meta l4proto icmp"), "{v4}");
        assert!(!v4.contains("ipv6-icmp"), "{v4}");
        let v6 = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Icmp,
            None,
            "fd00::/8",
        )])]);
        assert!(v6.contains("meta l4proto ipv6-icmp"), "{v6}");
    }

    /// **"Allow ping from anywhere" means from anywhere, on either family.**
    ///
    /// The family used to be chosen from whether the *remote* carried a colon,
    /// which is a fact about the remote and not about the packet. `0.0.0.0/0`
    /// is how the rule is written, and it emitted `icmp` alone — so on a guest
    /// with a v6 address every ICMPv6 hit the terminal drop, on a port whose
    /// operator had asked for ICMP and been told it was in force. Ping,
    /// traceroute and any Packet Too Big for a flow conntrack does not hold.
    #[test]
    fn an_icmp_rule_from_anywhere_reaches_a_guest_on_either_family() {
        let text = ruleset(&[guarded(vec![rule(
            Direction::Ingress,
            Protocol::Icmp,
            None,
            "0.0.0.0/0",
        )])]);
        assert!(
            text.contains("ipv6-icmp"),
            "a rule from anywhere refused ICMPv6: {text}"
        );
        assert!(
            text.contains("icmp"),
            "a rule from anywhere refused ICMPv4: {text}"
        );
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
