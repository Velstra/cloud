//! What a guest may claim to be, enforced where every frame is seen.
//!
//! **The gap this closes.** On the local datapath a guest was identified by
//! its source address and nothing else. The metadata service reads the peer's
//! address and hands over that guest's user-data and SSH keys; the firewall
//! jumps into a port's chains on the address the platform allocated; a routed
//! floating IP is delivered to an address. So a guest that took its
//! neighbour's address — one gratuitous ARP — read the neighbour's keys, was
//! unfiltered (it matched no jump, and the forward policy is accept), and
//! intercepted anything sent to it. The Velstra fabric binds address and MAC
//! to a port when it creates one; nothing here did.
//!
//! **Why the `bridge` family and not `inet`.** The `forward` hook of the
//! `inet` family is only traversed for packets the host *routes*. Two guests
//! on one segment are bridged, so their frames never reach it — unless
//! `br_netfilter` happens to be loaded, which nothing here arranges and which
//! a machine that also runs Docker has and a bare one does not. A security
//! property that depends on what else is installed is not a security property.
//! The `bridge` family hooks the bridge's own path and sees every frame,
//! whoever it is for.
//!
//! **What is bound.** For each tap: the addresses the platform gave that port,
//! in ARP and in IP, and — where the platform chose it — the MAC. A frame
//! arriving on a tap claiming anything else is dropped and counted. Nothing is
//! said about traffic *to* a guest: that is the firewall's question, and this
//! one is only "are you who you say you are".
//!
//! **A tap with no address is not filtered.** A port the platform has not
//! addressed yet, or one on a network whose addresses are somebody else's
//! (a host bridge), has nothing to check against, and a chain that dropped
//! everything for want of a fact would take a working guest off the air.

use std::collections::BTreeMap;

/// The table this module owns, and nothing else writes.
pub const TABLE: &str = "velstra-antispoof";

/// One tap and what the guest behind it may claim.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Bound {
    /// The port's full name, which is what a counter is reported against.
    pub port: String,
    /// The tap the frames arrive on. Matched on here — unlike the firewall,
    /// which cannot — because the `bridge` family sees the real ingress
    /// interface. By *name*, so a tap that is not up yet does not stop the
    /// table loading.
    pub tap: String,
    /// The addresses this port holds, both families where it has both.
    pub addresses: Vec<String>,
    /// The MAC the platform gave the guest, when it chose one.
    pub mac: Option<String>,
}

/// What a tap was caught claiming.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spoofed {
    pub packets: u64,
    pub bytes: u64,
}

/// The chain a tap's checks live in.
pub fn chain(tap: &str) -> String {
    format!("claim-{tap}")
}

/// The whole table, as `nft -f -` takes it.
///
/// One chain per tap, entered by ingress interface, and a terminal drop with a
/// counter on it. The counter is the point of the terminal rule: "how often did
/// this guest claim something that is not its own" is the question an operator
/// asks after an incident, and it cannot be answered afterwards.
pub fn ruleset(bound: &[Bound]) -> String {
    let bound: Vec<&Bound> = bound
        .iter()
        .filter(|b| !b.tap.is_empty() && !b.addresses.is_empty())
        .collect();

    let mut out = String::new();
    out.push_str(&format!("table bridge {TABLE} {{\n"));
    // Priority below the bridge filter's default so this runs before anything
    // an operator adds there, and on `prerouting` because the question is
    // about a frame's *origin* — asked as it arrives, before any decision
    // depends on the answer.
    out.push_str(
        "  chain claims {\n    type filter hook prerouting priority dstnat; policy accept;\n",
    );
    if bound.is_empty() {
        out.push_str("  }\n}\n");
        return out;
    }
    for b in &bound {
        // `iifname` and not `iif`: `iif` resolves the name to an interface
        // index when the table is *loaded*, so a tap that is not up yet makes
        // the whole ruleset fail — and taps come and go with guests. Checked
        // against a real `nft`, which refuses `iif "vtporta"` outright with
        // "Interface does not exist".
        out.push_str(&format!(
            "    iifname \"{}\" jump {}\n",
            b.tap,
            chain(&b.tap)
        ));
    }
    out.push_str("  }\n");

    for b in &bound {
        out.push_str(&format!("  chain {} {{\n", chain(&b.tap)));
        // The MAC first, when the platform chose one: an address claim made
        // from a MAC that is not this port's is already a lie, whatever the
        // address says.
        if let Some(mac) = b.mac.as_deref().filter(|m| !m.is_empty()) {
            out.push_str(&format!("    ether saddr != {mac} counter drop\n"));
        }
        for address in &b.addresses {
            if address.contains(':') {
                // Neighbour discovery carries the claim in the packet's own
                // source, so the address check below covers it.
                out.push_str(&format!("    ip6 saddr {address} counter accept\n"));
            } else {
                // ARP is where a claim is *made*: the sender protocol address
                // is what a neighbour will believe. Checked in its own right,
                // because an ARP frame carries no IP header for the rule below
                // to look at.
                out.push_str(&format!(
                    "    arp operation request arp saddr ip {address} counter accept\n"
                ));
                out.push_str(&format!(
                    "    arp operation reply arp saddr ip {address} counter accept\n"
                ));
                out.push_str(&format!("    ip saddr {address} counter accept\n"));
            }
        }
        // Everything a guest needs before it has an address of its own. A DHCP
        // discover is sent from 0.0.0.0, so a rule that insisted on the
        // allocated address would refuse the guest the very thing that gives
        // it one.
        out.push_str("    ip saddr 0.0.0.0 udp dport 67 counter accept\n");
        out.push_str("    ip6 saddr :: counter accept\n");
        // Anything else claiming to be somebody. Counted, so an operator can
        // ask afterwards.
        out.push_str("    counter drop\n");
        out.push_str("  }\n");
    }
    out.push_str("}\n");
    out
}

/// What each port was caught claiming, read from `nft -j list table`.
///
/// The terminal drop of each chain, by the chain's name. A chain nothing has
/// hit yet reports zero rather than being absent: "nobody has tried" and "this
/// port is not guarded" are different answers and a caller must not have to
/// guess which one an empty map means.
pub fn spoofed(listing: &serde_json::Value, bound: &[Bound]) -> BTreeMap<String, Spoofed> {
    let mut out: BTreeMap<String, Spoofed> = bound
        .iter()
        .filter(|b| !b.tap.is_empty() && !b.addresses.is_empty())
        .map(|b| (b.port.clone(), Spoofed::default()))
        .collect();
    let by_chain: BTreeMap<String, &Bound> = bound.iter().map(|b| (chain(&b.tap), b)).collect();

    let Some(items) = listing.get("nftables").and_then(|n| n.as_array()) else {
        return out;
    };
    for item in items {
        let Some(rule) = item.get("rule") else {
            continue;
        };
        if rule.get("table").and_then(|t| t.as_str()) != Some(TABLE) {
            continue;
        }
        let Some(chain_name) = rule.get("chain").and_then(|c| c.as_str()) else {
            continue;
        };
        let Some(b) = by_chain.get(chain_name) else {
            continue;
        };
        let Some(expressions) = rule.get("expr").and_then(|e| e.as_array()) else {
            continue;
        };
        // The terminal drop: a rule that counts and drops and matches nothing
        // else. Any other counted rule in the chain is an *accept*.
        let drops = expressions.iter().any(|e| e.get("drop").is_some());
        if !drops {
            continue;
        }
        // The MAC rule also drops, and it is a real catch too — both are "this
        // guest claimed something that is not its own", so both are counted
        // into the same number.
        for e in expressions {
            let Some(counter) = e.get("counter") else {
                continue;
            };
            let entry = out.entry(b.port.clone()).or_default();
            entry.packets += counter
                .get("packets")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            entry.bytes += counter
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(tap: &str, address: &str, mac: Option<&str>) -> Bound {
        Bound {
            port: format!("projects/p1/ports/{tap}"),
            tap: tap.to_string(),
            addresses: vec![address.to_string()],
            mac: mac.map(str::to_string),
        }
    }

    /// A guest may claim its own address and nothing else — and the claim is
    /// checked in ARP, which is where one is actually made.
    ///
    /// The attack this closes: guest B sends a gratuitous ARP for guest A's
    /// address. Afterwards the metadata service, which identifies a caller by
    /// its source address alone, hands B the user-data and SSH keys the
    /// platform wrote for A; B matches no firewall jump, so it is unfiltered
    /// in both directions; and anything sent to A's address arrives at B.
    #[test]
    fn a_guest_may_claim_its_own_address_and_nothing_else() {
        let text = ruleset(&[bound("vta", "10.19.136.2", Some("52:54:00:aa:bb:cc"))]);

        // Entered by the wire the frame arrived on. That is the whole reason
        // this table is in the `bridge` family: the `inet` forward hook never
        // sees a frame from one guest to its neighbour on the same segment.
        assert!(text.contains("table bridge velstra-antispoof"), "{text}");
        assert!(text.contains("hook prerouting"), "{text}");
        assert!(text.contains("iifname \"vta\" jump claim-vta"), "{text}");

        // ARP, in its own right: an ARP frame carries no IP header, so a rule
        // matching `ip saddr` would let every forged claim through.
        assert!(
            text.contains("arp operation request arp saddr ip 10.19.136.2"),
            "nothing checked the sender's ARP claim: {text}"
        );
        assert!(
            text.contains("arp operation reply arp saddr ip 10.19.136.2"),
            "a gratuitous ARP is a reply, and it was unchecked: {text}"
        );
        assert!(
            text.contains("ip saddr 10.19.136.2 counter accept"),
            "{text}"
        );
        assert!(
            text.contains("ether saddr != 52:54:00:aa:bb:cc counter drop"),
            "the MAC the platform chose was not bound: {text}"
        );

        // And a terminal drop, with a counter on it: "how often did this guest
        // claim something that is not its own" is the question afterwards.
        assert!(text.contains("counter drop"), "{text}");

        // DHCP still works. A discover comes from 0.0.0.0, so a rule insisting
        // on the allocated address would refuse a guest the very exchange that
        // gives it one.
        assert!(
            text.contains("ip saddr 0.0.0.0 udp dport 67 counter accept"),
            "a guest could not ask for its own address: {text}"
        );
    }

    /// A port with nothing to check against is not guarded.
    ///
    /// A port the platform has not addressed yet, or one on a network whose
    /// addresses belong to somebody else, has no claim to verify — and a chain
    /// that dropped everything for want of a fact would take a working guest
    /// off the air.
    #[test]
    fn a_port_with_no_address_is_left_alone() {
        let mut unaddressed = bound("vtb", "10.19.136.3", None);
        unaddressed.addresses.clear();
        let text = ruleset(&[unaddressed]);
        assert!(!text.contains("claim-vtb"), "{text}");
        // The table still exists, and is empty: a table that is absent is one
        // whose counters cannot be read, and "nobody has tried" would be
        // indistinguishable from "nothing is guarded".
        assert!(text.contains("table bridge velstra-antispoof"), "{text}");
    }

    /// Both families, each checked in its own terms.
    #[test]
    fn a_dual_stack_guest_is_bound_on_both() {
        let mut b = bound("vtc", "10.19.136.4", None);
        b.addresses.push("fd00:1::4".into());
        let text = ruleset(&[b]);
        assert!(text.contains("ip saddr 10.19.136.4"), "{text}");
        assert!(text.contains("ip6 saddr fd00:1::4"), "{text}");
        // Neighbour discovery needs a source of `::` before an address is
        // settled, exactly as DHCP does for v4.
        assert!(text.contains("ip6 saddr :: counter accept"), "{text}");
    }

    /// The counter is read back per port, and a port nobody has attacked
    /// reports zero rather than being missing.
    #[test]
    fn what_a_port_was_caught_claiming_is_read_back_by_name() {
        let guarded = vec![bound("vta", "10.19.136.2", None)];
        let listing = serde_json::json!({ "nftables": [
            { "rule": { "table": TABLE, "chain": "claim-vta", "expr": [
                { "counter": { "packets": 7, "bytes": 420 } },
                { "drop": null }
            ] } },
            // An accept in the same chain is not a catch.
            { "rule": { "table": TABLE, "chain": "claim-vta", "expr": [
                { "counter": { "packets": 99, "bytes": 9999 } },
                { "accept": null }
            ] } },
        ]});
        let seen = spoofed(&listing, &guarded);
        let caught = seen
            .get("projects/p1/ports/vta")
            .expect("a guarded port reports, even at zero");
        assert_eq!(caught.packets, 7);
        assert_eq!(caught.bytes, 420);

        let quiet = spoofed(&serde_json::json!({ "nftables": [] }), &guarded);
        assert_eq!(
            quiet.get("projects/p1/ports/vta").copied(),
            Some(Spoofed::default()),
            "a port nobody has attacked went missing instead of reporting zero"
        );
    }
}
