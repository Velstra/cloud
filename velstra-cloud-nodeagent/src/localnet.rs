//! The node as the guest's first hop.
//!
//! [`crate::datapath`] gives a guest a wire and says so plainly: it creates the
//! tap, brings it up, and connects it to **nothing**. That is the right shape
//! for a cell with a fabric, where the far end of every wire is the fabric's
//! business. It is a dead end for a cell without one, and the dead end is worse
//! than it looks, because it is silent:
//!
//! * The guest DHCPs, is answered with an address and the subnet's gateway, and
//!   the gateway is held by nobody.
//! * The guest's cloud-init reaches for `169.254.169.254`, which lives on this
//!   node, over a link with no route to it. It times out, finds no datasource,
//!   and writes no user, no SSH key and no network configuration. The guest
//!   boots to a login prompt that no key opens.
//!
//! Both were true of every guest this platform started on a node with no
//! fabric — the guest ran, reported `Running`, and could not be reached or
//! logged into by anybody. Nothing failed; there was simply nothing on the other
//! side of the wire.
//!
//! This module is the other side of the wire. Per subnet with a guest on this
//! node: a bridge, the subnet's **gateway address on it**, every tap enslaved,
//! forwarding on, and one NAT rule so the segment reaches whatever the node
//! reaches. Which is to say: what libvirt's default network and every home
//! hypervisor do, expressed as a function of the objects.
//!
//! ## Why it is opt-in
//!
//! Masquerading a tenant's frames out of a node's uplink is a policy, not a
//! detail, and a datacentre that runs a fabric has already decided otherwise. So
//! the node is a first hop only when it was told to be one
//! (`--local-network` / `VELSTRA_LOCAL_NETWORK=1`), which `quickstart` sets and
//! a node joining somebody else's cell does not. With it off, nothing here runs
//! and the behaviour is exactly the dead-end wire above — deliberately, and
//! documented, rather than by omission.
//!
//! ## It remembers nothing
//!
//! Every pass computes the whole picture from the objects and applies it: the
//! bridges are derived from subnet names, the addresses from the subnets, the
//! NAT table is **deleted and rewritten** rather than added to. An agent that
//! crashed mid-change and an agent that has never run reach the same machine,
//! which is the only recovery model this crate has.

use std::{collections::BTreeMap, net::IpAddr};

use crate::host::{HostError, Result};

/// The kernel's limit on an interface name, minus the terminator.
const IFNAMSIZ: usize = 15;

/// The nftables table this module owns, whole. Named so a person reading `nft
/// list ruleset` on a node knows who wrote it and what deleting it would undo.
const TABLE: &str = "velstra-localnet";

/// One of our bridges as the machine has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bridge {
    pub name: String,
    /// As `10.19.136.1/24` or `fd00:1::1/64`. The gateway is the only address
    /// this platform puts on a bridge, so anything else here is something to
    /// remove.
    pub addresses: Vec<String>,
    /// The interfaces enslaved to it — the guests' taps.
    ///
    /// Read for one reason: `removals` promises not to delete a bridge that
    /// still has something on it, and until this field existed it could not
    /// tell. The promise was in prose and the code deleted unconditionally.
    pub members: Vec<String>,
}

/// One segment with a guest on this node: where it is, and who is on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The subnet's resource name. Identity, and the bridge's alias.
    pub subnet: String,
    /// The address this node holds on the segment — the gateway the guests were
    /// handed. Held here or held by nobody.
    ///
    /// Either family. A v6 subnet used to be dropped on the floor here — the
    /// gateway would not parse as a `Ipv4Addr`, the loop `continue`d, and the
    /// segment was never built: no bridge, no gateway, no taps enslaved, and
    /// nothing anywhere saying so. The model, the allocator and the guest's own
    /// netplan have all handled v6 for as long as they have existed; this was
    /// the one place that quietly did not.
    pub gateway: IpAddr,
    /// How much of the world is on the link, from the subnet's CIDR.
    pub prefix_len: u8,
    /// The range, for the one NAT rule.
    pub network: String,
    /// The taps on this node carrying ports on this segment, in port order.
    pub taps: Vec<String>,
    /// Load-balancer addresses this node answers on for this segment.
    ///
    /// Held as host routes on the bridge — `/32`, or `/128` for v6 — because a
    /// VIP belongs to no broadcast domain: it is an address this machine
    /// answers for, not one it hands out. Without them on the bridge the
    /// listener has nothing to bind and the packets never arrive.
    pub vips: Vec<IpAddr>,
    /// Routed public addresses held by guests on this segment.
    ///
    /// A routed address lives *in* the guest — it configures it, and defaults
    /// out through a next hop that is in no subnet. For that to work on this
    /// datapath the node has to answer for the next hop on the bridge and hold
    /// a host route back in for the address, or the guest has an address and
    /// no way to send from it and the world has no way to reach it. Nothing
    /// on this datapath did either: the address appeared in the guest's
    /// netplan and pointed its default route at a next hop nobody answered.
    pub public: Vec<IpAddr>,
}

/// The next hop a guest with a routed public address defaults through, held
/// on every bridge that carries such a guest so the host answers ARP for it.
/// The same constant the metadata service renders — see
/// [`velstra_cloud_model::public`].
const NEXT_HOP_V4: &str = "169.254.1.1/32";

/// One change to the machine: a command and its arguments.
///
/// The plan is a value so that what this module would do is testable without
/// being root and without a machine to do it to — which is the only way the
/// interesting part (that it is the same plan twice, that a segment with no
/// gateway is left alone) gets exercised at all.
pub type Step = Vec<String>;

pub struct LocalNet {
    prefix: String,
    ip: String,
    nft: String,
    /// Which bridges have somebody advertising a route on them, and the handle
    /// to stop each.
    ///
    /// One task per segment, started when the segment first appears and left
    /// running: an advertiser is cheap and the alternative — starting one per
    /// pass — is a guest hearing a hundred routers claim the same link. What
    /// the set alone never said was when one *stops*. It never did: the task
    /// outlived its bridge, sending into a deleted device for ever, and — the
    /// sharp half — because the name stayed in the set, a subnet that came
    /// **back** started no new advertiser. The old socket held the old
    /// ifindex; the new bridge had a new one; a re-created v6 segment got no
    /// router advertisements at all, and a v6-only guest booted with no
    /// address and nothing saying why.
    ///
    /// Reconciled every pass now: gone or changed means stopped — with a
    /// lifetime-zero withdrawal, which is the message the module doc promised
    /// and nothing sent — and absent means started.
    advertising: std::sync::Mutex<std::collections::BTreeMap<String, Advertiser>>,
    /// The filter table as last written **and as the kernel then held it**.
    ///
    /// Every other ruleset here is rewritten whole on every pass, on purpose:
    /// a table assembled by adding and removing rules is one whose state
    /// depends on every pass that came before. The filter table is the
    /// exception, and for one reason — **it counts**. `add`/`delete`/rebuild
    /// re-creates the table, which sets every counter back to zero, so a
    /// firewall rewritten every few seconds reports that it has never dropped
    /// anything. Found on a live cell, where the drops were real and the
    /// number stayed at nought.
    ///
    /// Keyed on the kernel's own account, not on the text alone. Comparing
    /// only what was written made the cache blind to the one thing it exists
    /// beside: a table that is *gone* — a `flush ruleset`, an operator's hand
    /// — read as "unchanged" for ever, and the firewall was never written
    /// again. Now the skip needs two things: the intent is unchanged, *and*
    /// the kernel's current account of the table equals the account taken
    /// when it was written (handles and counters removed, because those two
    /// change without anybody having written). Where no account can be taken
    /// at all it falls back to the intent-only comparison — the conservative
    /// direction for the counters, and what a machine whose `nft` has no
    /// `-j` gets.
    filter_in_force: std::sync::Mutex<Option<InForce>>,
    /// The same, for the bridge-family half of the filter.
    bridge_filter_in_force: std::sync::Mutex<Option<InForce>>,
    /// The same, for the anti-spoofing table. Its counters answer "how often
    /// did this guest claim somebody else's address", which a rewrite erases.
    antispoof_in_force: std::sync::Mutex<Option<InForce>>,
    /// Whether this node can judge a frame between two guests on one bridge.
    ///
    /// True only once `nf_conntrack_bridge` has been verified present — see
    /// [`LocalNet::verify_same_segment_filtering`]. Until then the bridge-family
    /// filter is not written, because without that module every reply on the
    /// bridge is untracked and dropped: a whole-cell outage, not a partial
    /// one. The port says so instead.
    same_segment: std::sync::atomic::AtomicBool,
}

/// One running router advertiser, and how to stop it.
struct Advertiser {
    stop: tokio::sync::watch::Sender<bool>,
    gateway: std::net::Ipv6Addr,
    prefix_len: u8,
}

/// What was written, and the kernel's own account of it straight afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InForce {
    wanted: String,
    /// `None` when no account could be taken — an `nft` without `-j`, or the
    /// write happened through something a test substituted for it.
    kernel: Option<String>,
}

/// Whether `wanted` is already what the kernel holds.
///
/// Pure, so the decision that used to be one `==` and is now the whole point
/// can be tested without a machine: an unchanged intent with a recorded
/// account and no current one (the table is gone) is **not** in force.
fn already_in_force(wanted: &str, recorded: Option<&InForce>, current: Option<&str>) -> bool {
    let Some(recorded) = recorded else {
        return false;
    };
    if recorded.wanted != wanted {
        return false;
    }
    match (&recorded.kernel, current) {
        // Both accounts exist: the table has to still be the table we wrote.
        (Some(then), Some(now)) => then == now,
        // No account either time: fall back to the intent alone.
        (None, None) => true,
        // One account and not the other: something changed about what can
        // be seen, and the safe answer is to write.
        _ => false,
    }
}

/// An `nft -j list table` answer with everything that changes by itself
/// removed: every `handle`, and every counter's packets and bytes.
fn settled(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("handle");
            if let Some(counter) = map.get_mut("counter").and_then(|c| c.as_object_mut()) {
                counter.insert("packets".into(), serde_json::json!(0));
                counter.insert("bytes".into(), serde_json::json!(0));
            }
            for v in map.values_mut() {
                settled(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                settled(v);
            }
        }
        _ => {}
    }
}

impl LocalNet {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            ip: "ip".to_string(),
            nft: "nft".to_string(),
            advertising: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            filter_in_force: std::sync::Mutex::new(None),
            bridge_filter_in_force: std::sync::Mutex::new(None),
            antispoof_in_force: std::sync::Mutex::new(None),
            same_segment: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether frames between two guests on one bridge are judged here.
    pub fn filters_same_segment(&self) -> bool {
        self.same_segment.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Load and verify what the bridge-family filter depends on.
    ///
    /// `nf_conntrack_bridge` is what makes `ct state` mean anything on a
    /// bridged frame. Without it the established/related accept never
    /// matches and every reply to an allowed request is dropped — so the
    /// bridge filter is written only once this has said yes. Verified by
    /// asking the kernel whether the module is present, **not** by a probe
    /// ruleset: `nft -c` accepts `ct state` in the bridge family whether or
    /// not the module is loaded (checked on a real Debian 13), so a probe
    /// would say yes about a machine that would drop every reply.
    ///
    /// Best effort on the `modprobe`: a machine without it, or a kernel that
    /// built the module in, still gets the honest answer from `/sys/module`.
    pub async fn verify_same_segment_filtering(&self) -> bool {
        let _ = tokio::process::Command::new("modprobe")
            .arg("nf_conntrack_bridge")
            .output()
            .await;
        let present = tokio::fs::metadata("/sys/module/nf_conntrack_bridge")
            .await
            .is_ok();
        if !present {
            tracing::warn!(
                "nf_conntrack_bridge is not loaded, so frames between two guests on one \
                 bridge are not judged by their security groups on this node; the ports say so"
            );
        }
        self.same_segment
            .store(present, std::sync::atomic::Ordering::Relaxed);
        present
    }

    /// The bridge carrying `subnet`.
    ///
    /// Derived, not allocated, for the same reason tap names are: the same
    /// subnet is the same bridge on every pass, with nothing written down and
    /// nothing to reconcile after a crash. The digest is what keeps two subnets
    /// whose names agree for the first several characters off one another's
    /// segment.
    pub fn bridge_for(&self, subnet: &str) -> String {
        let leaf = subnet.rsplit('/').next().unwrap_or(subnet);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(subnet.as_bytes());
        let tail = format!("{:02x}{:02x}", digest[0], digest[1]);
        let room = IFNAMSIZ.saturating_sub(self.prefix.len() + tail.len());
        let head: String = leaf
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(room)
            .collect();
        format!("{}{head}{tail}", self.prefix)
    }

    /// What this node would have to become for `segments` to be reachable.
    ///
    /// Pure, and every step idempotent on its own: `addr replace` rather than
    /// `add`, `set master` unconditionally (setting the master a tap already has
    /// is a no-op), and a bridge whose creation is allowed to fail because it is
    /// already there.
    pub fn plan(&self, segments: &[Segment]) -> Vec<Step> {
        let mut steps: Vec<Step> = Vec::new();
        for segment in segments {
            let bridge = self.bridge_for(&segment.subnet);
            steps.push(step(["link", "add", "name", &bridge, "type", "bridge"]));
            // A bridge that learns is a bridge that waits: STP would hold every
            // new tap down for the better part of a minute, which a guest's DHCP
            // client reads as a link with nothing on it.
            steps.push(step([
                "link",
                "set",
                &bridge,
                "type",
                "bridge",
                "stp_state",
                "0",
            ]));
            steps.push(step(["link", "set", &bridge, "alias", &segment.subnet]));
            steps.push(step(["link", "set", &bridge, "up"]));
            steps.push(step([
                "addr",
                "replace",
                &format!("{}/{}", segment.gateway, segment.prefix_len),
                "dev",
                &bridge,
            ]));
            for vip in &segment.vips {
                // A host route: the address is answered for, not handed out.
                let width = if vip.is_ipv4() { 32 } else { 128 };
                steps.push(step([
                    "addr",
                    "replace",
                    &format!("{vip}/{width}"),
                    "dev",
                    &bridge,
                ]));
            }
            if segment.public.iter().any(|a| a.is_ipv4()) {
                // The next hop, answered for here. `/32` on the bridge is what
                // makes the host reply to the guest's ARP for it; the address
                // is in no subnet, which is exactly what lets it be right on
                // whichever bridge the guest is behind.
                steps.push(step(["addr", "replace", NEXT_HOP_V4, "dev", &bridge]));
            }
            for public in &segment.public {
                // The way back in. The guest holds the address; the kernel
                // needs to know which bridge to put a packet for it on.
                let width = if public.is_ipv4() { 32 } else { 128 };
                steps.push(step([
                    "route",
                    "replace",
                    &format!("{public}/{width}"),
                    "dev",
                    &bridge,
                ]));
            }
            for tap in &segment.taps {
                steps.push(step(["link", "set", tap, "master", &bridge]));
            }
        }
        steps
    }

    /// The whole NAT table, rewritten.
    ///
    /// `add` before `delete` so the delete cannot fail on a node where the table
    /// was never there — the standard way to say "this table is mine and this is
    /// all of it" in one atomic load. `oifname != <bridge>` is what keeps a
    /// guest talking to its neighbour on the same segment from being translated
    /// on the way.
    /// Only the v4 segments are translated, and that is deliberate rather than
    /// unfinished. Masquerading IPv6 is a thing people do and a thing every
    /// v6 document asks them not to: the address space exists so that a host
    /// can be reached, and a private cloud that hides its guests behind one
    /// address has thrown that away. A v6 segment here gets a bridge, a
    /// gateway and forwarding — routed, as v6 is meant to be — and reaches
    /// whatever the node's own routing can reach.
    pub fn ruleset(&self, segments: &[Segment]) -> String {
        let mut out = String::new();
        out.push_str(&format!("add table ip {TABLE}\n"));
        out.push_str(&format!("delete table ip {TABLE}\n"));
        out.push_str(&format!("table ip {TABLE} {{\n"));
        out.push_str(
            "  chain postrouting {\n    type nat hook postrouting priority srcnat; policy accept;\n",
        );
        for segment in segments.iter().filter(|s| s.gateway.is_ipv4()) {
            out.push_str(&format!(
                "    ip saddr {} oifname != \"{}\" masquerade\n",
                segment.network,
                self.bridge_for(&segment.subnet)
            ));
        }
        out.push_str("  }\n}\n");
        out
    }

    /// What this node currently has of ours: bridge name, its addresses, and
    /// whether anything is on it.
    ///
    /// Only ours — the prefix is what says so — because a plan that removed
    /// interfaces it did not make would be a plan that takes a machine's own
    /// networking away.
    pub async fn observed(&self) -> Vec<Bridge> {
        let Ok(out) = self
            .ip_output(&["-j", "addr", "show", "type", "bridge"])
            .await
        else {
            return Vec::new();
        };
        // Who is enslaved to what, in one call. An `ip link show` entry carries
        // `master` when it has one, so the members of every bridge fall out of a
        // single read rather than one per bridge.
        let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Ok(links) = self.ip_output(&["-j", "link", "show"]).await
            && let Ok(links) = serde_json::from_slice::<serde_json::Value>(&links)
        {
            for link in links.as_array().map(Vec::as_slice).unwrap_or_default() {
                let (Some(name), Some(master)) = (
                    link.get("ifname").and_then(|v| v.as_str()),
                    link.get("master").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                members
                    .entry(master.to_string())
                    .or_default()
                    .push(name.to_string());
            }
        }
        let Ok(links) = serde_json::from_slice::<serde_json::Value>(&out) else {
            return Vec::new();
        };
        links
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|link| {
                let name = link.get("ifname")?.as_str()?.to_string();
                if !name.starts_with(&self.prefix) {
                    return None;
                }
                let addresses = addresses_of_value(link);
                let members = members.get(&name).cloned().unwrap_or_default();
                Some(Bridge {
                    name,
                    addresses,
                    members,
                })
            })
            .collect()
    }

    /// What has to go before the plan runs.
    ///
    /// The plan alone is additive — `addr replace` adds and never takes away —
    /// and additive is not convergent. Found on a live cell in the worst
    /// possible shape: a project's network was remade with a different range,
    /// the bridge kept the *old* gateway beside the new one, and the same
    /// `10.19.136.1/24` ended up on two bridges at once. The kernel picks one
    /// route for the range, it picked the bridge with nothing on it, and every
    /// new guest in that project answered `No route to host` while the control
    /// plane reported it Running with an address.
    ///
    /// Two removals, both keyed on the prefix so nothing outside this platform
    /// is ever touched:
    ///
    /// * an address on one of our bridges that is not the gateway that bridge is
    ///   for — the stale-gateway case above;
    /// * a bridge of ours that no segment names — the network was deleted.
    ///
    /// A bridge with something still on it is left alone and said out loud: a
    /// guest whose port the control plane has forgotten is a guest that is still
    /// running, and cutting its wire is the one mistake here that a person
    /// cannot undo from the console.
    pub fn removals(&self, segments: &[Segment], observed: &[Bridge]) -> Vec<Step> {
        let mut steps = Vec::new();
        for bridge in observed {
            // The prefix is checked *here*, where the decision is made, and not
            // only in `observed` where the list is gathered. Two places is not
            // belt and braces: this function is the one that emits `link del`,
            // and a caller handing it a list from somewhere else — a test, a
            // future observer, a refactor — would otherwise take `docker0` off a
            // machine. Its own test proposed exactly that before this line.
            if !bridge.name.starts_with(&self.prefix) {
                continue;
            }
            let wanted = segments
                .iter()
                .find(|s| self.bridge_for(&s.subnet) == bridge.name);
            match wanted {
                Some(segment) => {
                    // The gateway, and every balancer address this node is
                    // answering for. Anything else on one of our bridges is
                    // something a previous shape left behind.
                    let mut keep = vec![format!("{}/{}", segment.gateway, segment.prefix_len)];
                    keep.extend(
                        segment
                            .vips
                            .iter()
                            .map(|vip| format!("{vip}/{}", if vip.is_ipv4() { 32 } else { 128 })),
                    );
                    if segment.public.iter().any(|a| a.is_ipv4()) {
                        keep.push(NEXT_HOP_V4.to_string());
                    }
                    // Compared as parsed `(IpAddr, prefix)`, not as strings.
                    // `keep` is built with Rust's `Display` and `bridge.addresses`
                    // with iproute2's; for v4 the two always agree, for v6 they
                    // agree in the ordinary case and are not guaranteed to for
                    // every spelling — and a disagreement here is not cosmetic,
                    // it deletes the gateway it has just added, every pass.
                    let kept: Vec<_> = keep.iter().filter_map(|a| parse_cidr(a)).collect();
                    for address in &bridge.addresses {
                        let mine = parse_cidr(address);
                        if mine.is_none_or(|m| !kept.contains(&m)) {
                            steps.push(step(["addr", "del", address, "dev", &bridge.name]));
                        }
                    }
                }
                // Nothing wants this bridge — but something is still on it.
                // A guest whose port the control plane has forgotten is a guest
                // that is still running, and cutting its wire is the one mistake
                // here a person cannot undo from the console. Said out loud, and
                // left standing; the next pass looks again.
                None if !bridge.members.is_empty() => {
                    tracing::warn!(
                        bridge = %bridge.name,
                        members = %bridge.members.join(", "),
                        "no segment wants this bridge, but it still carries interfaces; \
                         leaving it alone"
                    );
                }
                None => steps.push(step(["link", "del", &bridge.name])),
            }
        }
        steps
    }

    /// Apply the plan, then the ruleset, then turn forwarding on.
    ///
    /// In that order on purpose: a segment that is forwarded before it exists is
    /// a window in which the node routes for a range it does not hold.
    ///
    /// `guarded` is what each port on this node is allowed — see
    /// [`crate::nftfilter`]. The firewall goes on **before** forwarding, and
    /// that ordering is the same rule as the one above: a guest that is
    /// routed for before its rules are in force is a guest that was briefly
    /// open to everything.
    pub async fn apply(
        &self,
        segments: &[Segment],
        guarded: &[crate::nftfilter::Guarded],
    ) -> Result<()> {
        // Before anything else, and unconditionally: what a guest may claim to
        // be does not depend on whether it carries firewall rules, and a pass
        // that built the wires first would leave a window in which it could
        // claim anything. See [`crate::antispoof`].
        self.antispoof(guarded).await?;
        // Advertisers before removals, on both paths: a withdrawal has to go
        // out on a device that still exists, and the one case the old shape
        // certainly never stopped was the last v6 segment leaving — the empty
        // path never reached `advertise` at all.
        self.advertise(segments);
        if segments.is_empty() {
            // Still swept, and still rewritten: the last guest leaving a node
            // has to take its NAT rule *and* its bridge with it. "Nothing to do"
            // is how a stale rule outlives the segment it was written for, and
            // how a bridge holding a gateway outlives the network it was for.
            for step in self.removals(segments, &self.observed().await) {
                let args: Vec<&str> = step.iter().map(String::as_str).collect();
                if let Err(e) = self.ip(&args).await {
                    tracing::warn!(error = %e, "could not clear a stale piece of the datapath");
                }
            }
            self.nft(&self.ruleset(segments)).await?;
            self.filter(guarded).await?;
            return Ok(());
        }
        // Removals first: a bridge on its way out may be holding the very
        // address the bridge on its way in needs.
        for step in self.removals(segments, &self.observed().await) {
            let args: Vec<&str> = step.iter().map(String::as_str).collect();
            if let Err(e) = self.ip(&args).await {
                // Not fatal. Something that could not be taken away is a stale
                // interface, and a stale interface is better than a pass that
                // stops before it has built the new one.
                tracing::warn!(error = %e, "could not clear a stale piece of the datapath");
            }
        }
        for step in self.plan(segments) {
            let args: Vec<&str> = step.iter().map(String::as_str).collect();
            // Creating a bridge that exists is the expected case on every pass
            // after the first, and it is the one failure that means the machine
            // is already how we want it.
            let tolerate_exists = args.first() == Some(&"link") && args.get(1) == Some(&"add");
            match self.ip(&args).await {
                Ok(()) => {}
                Err(e) if tolerate_exists && e.to_string().contains("exists") => {}
                Err(e) => return Err(e),
            }
        }
        self.nft(&self.ruleset(segments)).await?;
        self.filter(guarded).await?;
        forwarding_on().await?;
        Ok(())
    }

    /// Say, on each v6 segment this node holds, that this node is the way out.
    ///
    /// **Only v6, and only here.** IPv4 needs none of this: a guest gets its
    /// address and its gateway from DHCP, or from the netplan the metadata
    /// service hands it. IPv6 has no DHCP in this platform and the metadata
    /// service listens on a v4 address — so a guest with no v4 address at all
    /// cannot reach the thing that would tell it what its v6 address is. A
    /// router advertisement is the one mechanism that breaks that circle.
    ///
    /// **Only on this datapath.** An advertisement has to come from the machine
    /// that holds the gateway. Here it is this node. On a cell whose datapath
    /// is the fabric it is not, and this function is not called there — a
    /// router claiming a link it does not route is worse than no router.
    fn advertise(&self, segments: &[Segment]) {
        let wanted: std::collections::BTreeMap<String, (std::net::Ipv6Addr, u8)> = segments
            .iter()
            .filter_map(|s| match s.gateway {
                std::net::IpAddr::V6(gateway) => {
                    Some((self.bridge_for(&s.subnet), (gateway, s.prefix_len)))
                }
                std::net::IpAddr::V4(_) => None,
            })
            .collect();
        let mut running = self.advertising.lock().expect("the map is never poisoned");
        // Stop what nothing wants, or wants differently. A changed gateway or
        // prefix under the same bridge name is the same as gone: the running
        // task holds its frame by value.
        let stale: Vec<String> = running
            .iter()
            .filter(|(device, a)| {
                wanted
                    .get(*device)
                    .is_none_or(|(g, l)| *g != a.gateway || *l != a.prefix_len)
            })
            .map(|(device, _)| device.clone())
            .collect();
        for device in stale {
            if let Some(a) = running.remove(&device) {
                // The receiver may already be gone (the socket failed to bind
                // and the task returned); nothing to withdraw then.
                let _ = a.stop.send(true);
            }
        }
        // Start what is missing.
        for (device, (gateway, prefix_len)) in wanted {
            if running.contains_key(&device) {
                continue;
            }
            let (stop, rx) = tokio::sync::watch::channel(false);
            running.insert(
                device.clone(),
                Advertiser {
                    stop,
                    gateway,
                    prefix_len,
                },
            );
            tokio::spawn(crate::ra::serve(device, gateway, prefix_len, rx));
        }
    }

    /// Which bridges are being advertised on right now — for a test, since
    /// the observable effect of `advertise` is the map.
    #[cfg(test)]
    fn advertising_on(&self) -> Vec<String> {
        self.advertising
            .lock()
            .expect("the map is never poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// The same, keeping what it said.
    async fn ip_output(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = tokio::process::Command::new(&self.ip)
            .args(args)
            .output()
            .await
            .map_err(|e| HostError::failed(format!("running `ip {}`: {e}", args.join(" "))))?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(HostError::failed(format!(
            "`ip {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    async fn ip(&self, args: &[&str]) -> Result<()> {
        let output = tokio::process::Command::new(&self.ip)
            .args(args)
            .output()
            .await
            .map_err(|e| HostError::failed(format!("running `ip {}`: {e}", args.join(" "))))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(HostError::failed(format!(
            "`ip {}` failed: {stderr}",
            args.join(" ")
        )))
    }

    /// Write what each guest may claim to be, unless it is already in force.
    ///
    /// Cached like the firewall and for the same reason: rewriting a table
    /// zeroes its counters, and "how often did this guest claim somebody
    /// else's address" is a number an operator reads after the fact.
    async fn antispoof(&self, guarded: &[crate::nftfilter::Guarded]) -> Result<()> {
        let bound: Vec<crate::antispoof::Bound> = guarded
            .iter()
            .map(|g| crate::antispoof::Bound {
                port: g.port.clone(),
                tap: g.tap.clone(),
                addresses: g.addresses.clone(),
                mac: g.mac.clone(),
            })
            .collect();
        let wanted = crate::antispoof::ruleset(&bound);
        self.write_once(
            &self.antispoof_in_force,
            "bridge",
            crate::antispoof::TABLE,
            wanted,
        )
        .await
    }

    /// Write the per-port firewall, unless it is already what is in force.
    ///
    /// See [`LocalNet::filter_in_force`] for why this one table is not
    /// rewritten every pass like the others — and for what "in force" has to
    /// mean for that to be safe.
    ///
    /// Two tables where the module allows: the `inet` one for routed traffic,
    /// and the `bridge` one for frames between two guests on one bridge — see
    /// [`crate::nftfilter::BRIDGE_TABLE`]. The second is written only once
    /// [`LocalNet::verify_same_segment_filtering`] has said the kernel can
    /// track a bridged flow; without that every reply would be dropped.
    async fn filter(&self, guarded: &[crate::nftfilter::Guarded]) -> Result<()> {
        let wanted = crate::nftfilter::ruleset(guarded);
        self.write_once(
            &self.filter_in_force,
            "inet",
            crate::nftfilter::TABLE,
            wanted,
        )
        .await?;
        if self.filters_same_segment() {
            let bridged = crate::nftfilter::ruleset_bridge(guarded);
            self.write_once(
                &self.bridge_filter_in_force,
                "bridge",
                crate::nftfilter::BRIDGE_TABLE,
                bridged,
            )
            .await?;
        }
        Ok(())
    }

    /// Write `wanted` unless the kernel still holds exactly what was written
    /// last time. The one place the three counting tables go through.
    async fn write_once(
        &self,
        cache: &std::sync::Mutex<Option<InForce>>,
        family: &str,
        table: &str,
        wanted: String,
    ) -> Result<()> {
        let current = self.as_held(family, table).await;
        {
            let recorded = cache.lock().expect("the cache is never poisoned");
            if already_in_force(&wanted, recorded.as_ref(), current.as_deref()) {
                return Ok(());
            }
        }
        self.nft(&wanted).await?;
        // Only after it took: a remembered ruleset that was never applied is
        // the one way this cache could skip a write that mattered. And the
        // kernel's account taken straight after, which is what the next pass
        // compares against.
        let kernel = self.as_held(family, table).await;
        *cache.lock().expect("the cache is never poisoned") = Some(InForce { wanted, kernel });
        Ok(())
    }

    /// The table as `nft` holds it, with everything that changes by itself
    /// removed. `None` when it cannot be read at all — including when the
    /// table is gone, which is the case this exists for.
    async fn as_held(&self, family: &str, table: &str) -> Option<String> {
        let out = tokio::process::Command::new(&self.nft)
            .args(["-j", "list", "table", family, table])
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let mut value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        settled(&mut value);
        Some(value.to_string())
    }

    /// The firewall tables as `nft` reports them, counters and all — both
    /// families, folded into one listing so `nftfilter::counters` reads them
    /// as one.
    ///
    /// JSON because the text form is for people: a counter read out of
    /// `nft list ruleset` by regular expression is a reading that breaks the
    /// day somebody changes a word.
    pub async fn filter_counters(&self) -> Result<serde_json::Value> {
        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut first_error: Option<String> = None;
        let tables: &[(&str, &str)] = if self.filters_same_segment() {
            &[
                ("inet", crate::nftfilter::TABLE),
                ("bridge", crate::nftfilter::BRIDGE_TABLE),
            ]
        } else {
            &[("inet", crate::nftfilter::TABLE)]
        };
        for (family, table) in tables {
            let out = tokio::process::Command::new(&self.nft)
                .args(["-j", "list", "table", family, table])
                .output()
                .await?;
            if !out.status.success() {
                first_error.get_or_insert_with(|| {
                    format!(
                        "nft list table {family} {table}: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )
                });
                continue;
            }
            let listing: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
                std::io::Error::other(format!("nft answered something that is not JSON: {e}"))
            })?;
            if let Some(items) = listing.get("nftables").and_then(|n| n.as_array()) {
                entries.extend(items.iter().cloned());
            }
        }
        if entries.is_empty() {
            if let Some(why) = first_error {
                return Err(std::io::Error::other(why).into());
            }
        }
        Ok(serde_json::json!({ "nftables": entries }))
    }

    async fn nft(&self, ruleset: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new(&self.nft)
            .args(["-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                HostError::failed(format!(
                    "running `nft`: {e} — a node that is a first hop needs nftables. \
                     Install it, or turn --local-network off and let a fabric carry the segment."
                ))
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(ruleset.as_bytes())
                .await
                .map_err(|e| HostError::failed(format!("writing the ruleset to nft: {e}")))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| HostError::failed(format!("waiting for nft: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(HostError::failed(format!(
            "nft refused the ruleset: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Turn forwarding on, saying which knob refused.
///
/// Written through `/proc` rather than `sysctl(8)`, which is one fewer binary a
/// node has to have — and the file is the thing `sysctl` writes anyway.
async fn forwarding_on() -> Result<()> {
    // Both families. The v4 knob alone was enough for as long as this datapath
    // silently dropped every v6 subnet; now that it carries one, a guest on it
    // would have a gateway that answers and forwards nothing.
    let path = "/proc/sys/net/ipv4/ip_forward";
    tokio::fs::write(path, b"1\n").await.map_err(|e| {
        HostError::failed(format!("writing {path}: {e} — this needs CAP_NET_ADMIN"))
    })?;
    // Best effort: a kernel built without IPv6 has no such file, and refusing
    // to bring a v4 cell up because of that would be the wrong trade.
    let v6 = "/proc/sys/net/ipv6/conf/all/forwarding";
    if let Err(e) = tokio::fs::write(v6, b"1\n").await {
        tracing::debug!(error = %e, "{v6} could not be written; v6 segments will not forward");
    }
    Ok(())
}

/// The global addresses on one link, from `ip -j addr show`.
///
/// **Filtered by scope, not by family.** It was `family == "inet"`, which meant
/// `removals` never saw a v6 address at all: a v6 gateway that moved stayed
/// behind on the old bridge for ever, and a deleted balancer's `/128` VIP was
/// never taken back. It cannot simply take *every* v6 address either — the
/// kernel puts an `fe80::/10` link-local on every bridge by itself, and
/// `removals` would delete it on every pass with the kernel putting it straight
/// back. `scope == "global"` keeps the reason the family filter existed and
/// drops the accident. Pure, so it is tested without a machine.
/// `10.0.0.1/24` as `(IpAddr, prefix)`, or `None` for anything that is not one.
/// Two renderings of the same address parse to the same pair, which is why the
/// removal check compares these and not the strings.
fn parse_cidr(text: &str) -> Option<(std::net::IpAddr, u8)> {
    let (addr, prefix) = text.split_once('/')?;
    Some((addr.parse().ok()?, prefix.parse().ok()?))
}

fn addresses_of_value(link: &serde_json::Value) -> Vec<String> {
    link.get("addr_info")
        .and_then(|a| a.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|a| a.get("scope").and_then(|s| s.as_str()) == Some("global"))
        .filter_map(|a| {
            Some(format!(
                "{}/{}",
                a.get("local")?.as_str()?,
                a.get("prefixlen")?.as_u64()?
            ))
        })
        .collect()
}

fn step<const N: usize>(args: [&str; N]) -> Step {
    args.iter().map(|a| a.to_string()).collect()
}

/// The segments this node is the first hop for.
///
/// From the **ports** it carries rather than from the guests running on them,
/// and that is the fix for a race, not a refactor. The far end of a wire has to
/// exist before the guest on it boots, and the guest boots in the same pass that
/// makes the wire: a picture derived from running guests cannot include the tap
/// that was created one action ago, so the bridge appeared a pass later — up to
/// a resync interval. A guest that boots in fifteen seconds does its whole
/// cloud-init inside that window, finds no datasource, and comes up with no
/// user and no key. It looked exactly like a metadata bug and was an ordering
/// one.
///
/// One segment per subnet, not one per port: several guests on one segment share
/// a bridge, and giving each its own would put two guests the platform says are
/// neighbours on two links that cannot see each other.
///
/// A port with no tap here, no address, or on a subnet with no gateway
/// contributes **nothing** rather than a partial segment. A subnet whose gateway
/// nobody declared has its first hop somewhere else, and a node that invented one
/// would be answering for a range it was never given.
pub fn segments(
    ports: &BTreeMap<String, velstra_cloud_model::resources::Port>,
    subnets: &BTreeMap<String, velstra_cloud_model::resources::Subnet>,
    networks: &BTreeMap<String, velstra_cloud_model::resources::NetworkSpec>,
    taps: &BTreeMap<String, String>,
    balancers: &[velstra_cloud_model::loadbalancer::LoadBalancer],
    // Routed public addresses, so the segment holding the guest gets the next
    // hop and the host route. Empty in a cell that hands out none.
    floating: &[velstra_cloud_model::resources::FloatingIp],
) -> Vec<Segment> {
    let mut by_subnet: BTreeMap<String, Segment> = BTreeMap::new();
    for (name, port) in ports {
        let Some(tap) = taps.get(name) else {
            continue;
        };
        // A network on a host bridge is the machine's own wire. Holding a
        // gateway on it, or translating out of it, would be this platform
        // quietly taking over a network somebody else runs — and the address the
        // guest has did not come from us in the first place.
        if networks
            .get(&port.spec.network)
            .is_some_and(|n| !n.host_bridge.is_empty())
        {
            continue;
        }
        let Some(subnet) = subnets.get(&port.spec.subnet) else {
            continue;
        };
        let Ok(cidr) = velstra_cloud_model::network::Cidr::parse(&subnet.spec.cidr) else {
            continue;
        };
        let Ok(gateway) = subnet.spec.gateway.parse::<IpAddr>() else {
            continue;
        };
        // A gateway of one family on a range of the other is a subnet nobody
        // could use, and putting it on a bridge would be this node claiming an
        // address that belongs to neither.
        if gateway.is_ipv4() != cidr.address.is_ipv4() {
            continue;
        }
        let segment = by_subnet
            .entry(port.spec.subnet.clone())
            .or_insert_with(|| Segment {
                subnet: port.spec.subnet.clone(),
                gateway,
                prefix_len: cidr.prefix_len,
                network: format!("{}/{}", cidr.network(), cidr.prefix_len),
                taps: Vec::new(),
                vips: Vec::new(),
                public: Vec::new(),
            });
        if !segment.taps.iter().any(|t| t == tap) {
            segment.taps.push(tap.clone());
        }
    }
    // The balancer addresses that belong to each segment this node holds. A
    // balancer on a subnet no guest of this node is on is not this node's to
    // answer for — its VIP belongs wherever its members are.
    for balancer in balancers {
        if balancer.meta.deleted_at.is_some() {
            continue;
        }
        let Some(segment) = by_subnet.get_mut(&balancer.spec.subnet) else {
            continue;
        };
        let Some(vip) = balancer
            .spec
            .vip
            .as_ref()
            .and_then(|v| v.parse::<IpAddr>().ok())
        else {
            continue;
        };
        if !segment.vips.contains(&vip) {
            segment.vips.push(vip);
        }
    }
    // Routed public addresses held by a port on one of these segments. Only
    // routed: a translated address is answered for at the edge and the guest
    // must not hold it. Keyed by the port the address names, because that is
    // the guest that holds it and the segment is the port's.
    for fip in floating {
        if fip.meta.deleted_at.is_some()
            || fip.spec.delivery != velstra_cloud_model::public::Delivery::Routed
            || fip.spec.port.is_empty()
        {
            continue;
        }
        let Some(port) = ports.get(&fip.spec.port) else {
            continue;
        };
        let Some(segment) = by_subnet.get_mut(&port.spec.subnet) else {
            continue;
        };
        let Some(address) = fip
            .spec
            .address
            .as_deref()
            .and_then(|a| a.parse::<IpAddr>().ok())
        else {
            continue;
        };
        if !segment.public.contains(&address) {
            segment.public.push(address);
        }
    }
    for segment in by_subnet.values_mut() {
        segment.vips.sort();
        segment.public.sort();
    }
    by_subnet.into_values().collect()
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement},
        resources::{Port, PortSpec, Subnet, SubnetSpec},
    };

    use super::*;

    pub(crate) fn net() -> LocalNet {
        LocalNet::new("vbr")
    }

    pub(crate) fn meta(name: &str) -> Meta {
        Meta::new(
            name.parse().expect("a resource name"),
            Placement::new("eu-central", "cell-1"),
        )
    }

    pub(crate) fn port(name: &str, subnet: &str, address: &str) -> (String, Port) {
        (
            name.to_string(),
            Port::new(
                meta(name),
                PortSpec {
                    subnet: subnet.to_string(),
                    network: "projects/p/networks/n".into(),
                    address: Some(address.to_string()),
                    ..PortSpec::default()
                },
                Default::default(),
            ),
        )
    }

    /// A cell with one ordinary network, which is what every test here means
    /// unless it says otherwise.
    pub(crate) fn logical() -> BTreeMap<String, velstra_cloud_model::resources::NetworkSpec> {
        BTreeMap::new()
    }

    /// One the operator put on the machine's own wire.
    pub(crate) fn on_a_host_bridge(
        network: &str,
    ) -> BTreeMap<String, velstra_cloud_model::resources::NetworkSpec> {
        BTreeMap::from([(
            network.to_string(),
            velstra_cloud_model::resources::NetworkSpec {
                host_bridge: "br0".into(),
                ..Default::default()
            },
        )])
    }

    pub(crate) fn subnet(name: &str, cidr: &str, gateway: &str) -> (String, Subnet) {
        (
            name.to_string(),
            Subnet::new(
                meta(name),
                SubnetSpec {
                    cidr: cidr.to_string(),
                    gateway: gateway.to_string(),
                    ..SubnetSpec::default()
                },
                Default::default(),
            ),
        )
    }

    /// The whole point of a segment. Two bridges here would be two guests the
    /// platform says are neighbours and that cannot see each other.
    #[test]
    fn two_ports_on_one_subnet_share_one_bridge() {
        let ports = BTreeMap::from([
            port("projects/p/ports/a", "projects/p/subnets/s", "10.42.0.2"),
            port("projects/p/ports/b", "projects/p/subnets/s", "10.42.0.3"),
        ]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([
            ("projects/p/ports/a".to_string(), "vta".to_string()),
            ("projects/p/ports/b".to_string(), "vtb".to_string()),
        ]);

        let segments = segments(&ports, &subnets, &logical(), &taps, &[], &[]);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].taps, ["vta", "vtb"]);
        assert_eq!(segments[0].network, "10.42.0.0/24");
        assert_eq!(segments[0].gateway, "10.42.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn two_subnets_are_two_bridges() {
        let ports = BTreeMap::from([
            port("projects/p/ports/a", "projects/p/subnets/one", "10.42.0.2"),
            port("projects/p/ports/b", "projects/p/subnets/two", "10.43.0.2"),
        ]);
        let subnets = BTreeMap::from([
            subnet("projects/p/subnets/one", "10.42.0.0/24", "10.42.0.1"),
            subnet("projects/p/subnets/two", "10.43.0.0/24", "10.43.0.1"),
        ]);
        let taps = BTreeMap::from([
            ("projects/p/ports/a".to_string(), "vta".to_string()),
            ("projects/p/ports/b".to_string(), "vtb".to_string()),
        ]);

        let segments = segments(&ports, &subnets, &logical(), &taps, &[], &[]);
        assert_eq!(segments.len(), 2);
        let bridges: Vec<String> = segments
            .iter()
            .map(|s| net().bridge_for(&s.subnet))
            .collect();
        assert_ne!(bridges[0], bridges[1], "{bridges:?}");
    }

    /// A subnet nobody gave a gateway has its first hop somewhere else, and a
    /// node that invented one would answer for a range it was never given.
    #[test]
    fn a_segment_with_no_gateway_is_not_this_nodes_to_answer_for() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);
        assert!(segments(&ports, &subnets, &logical(), &taps, &[], &[]).is_empty());
    }

    /// A port the fabric carries, or one on another node, has no tap here.
    /// Bridging it would be this node claiming a segment that is not on it.
    #[test]
    fn a_port_with_no_tap_on_this_node_is_not_bridged_here() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        assert!(segments(&ports, &subnets, &logical(), &BTreeMap::new(), &[], &[]).is_empty());
    }

    /// The picture comes from the **ports**, not from running guests — which is
    /// the fix for a race, not a preference. A tap created one action ago
    /// belongs to no guest yet, and a first hop that waited for one arrived
    /// after the guest had finished looking for it.
    #[test]
    fn a_port_with_a_tap_and_no_guest_yet_still_gets_its_far_end() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);
        assert_eq!(
            segments(&ports, &subnets, &logical(), &taps, &[], &[]).len(),
            1
        );
    }

    /// Every step has to survive being run on a machine already in the state it
    /// describes, because after the first pass that is every pass.
    #[test]
    fn the_plan_is_the_same_plan_twice() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);
        let segments = segments(&ports, &subnets, &logical(), &taps, &[], &[]);
        let net = net();
        assert_eq!(net.plan(&segments), net.plan(&segments));

        let flat: Vec<String> = net.plan(&segments).iter().map(|s| s.join(" ")).collect();
        let bridge = net.bridge_for("projects/p/subnets/s");
        // `replace`, not `add`: the second pass must not fail on the address the
        // first one put there.
        assert!(
            flat.contains(&format!("addr replace 10.42.0.1/24 dev {bridge}")),
            "{flat:?}"
        );
        assert!(
            !flat.iter().any(|s| s.starts_with("addr add")),
            "an `add` here fails on every pass after the first: {flat:?}"
        );
        // The one fact that makes the whole thing work: DHCP hands out the
        // subnet's gateway, so the node has to *be* it.
        assert!(
            flat.contains(&format!("link set vta master {bridge}")),
            "{flat:?}"
        );
    }

    #[test]
    fn the_nat_table_is_rewritten_whole_so_a_departed_segment_takes_its_rule_with_it() {
        let net = net();
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);
        let full = net.ruleset(&segments(&ports, &subnets, &logical(), &taps, &[], &[]));
        assert!(full.contains("ip saddr 10.42.0.0/24"), "{full}");
        // Added before deleted, or the delete fails on a node where the table
        // was never there.
        assert!(
            full.find(&format!("add table ip {TABLE}"))
                < full.find(&format!("delete table ip {TABLE}")),
            "{full}"
        );
        // A guest talking to its neighbour on the same segment must not be
        // translated on the way: it would arrive from the node instead of from
        // the guest, and a security group about the neighbour would match
        // nothing.
        let bridge = net.bridge_for("projects/p/subnets/s");
        assert!(
            full.contains(&format!("oifname != \"{bridge}\" masquerade")),
            "{full}"
        );

        let empty = net.ruleset(&[]);
        assert!(!empty.contains("masquerade"), "{empty}");
        assert!(
            empty.contains(&format!("delete table ip {TABLE}")),
            "{empty}"
        );
    }

    #[test]
    fn a_bridge_name_fits_the_kernels_limit_and_is_the_same_every_pass() {
        let net = net();
        let long = "projects/a-very-long-project-name/subnets/a-very-long-subnet-name";
        assert!(
            net.bridge_for(long).len() <= IFNAMSIZ,
            "{}",
            net.bridge_for(long)
        );
        assert_eq!(net.bridge_for(long), net.bridge_for(long));
        // Two subnets agreeing for the first many characters are still two.
        assert_ne!(
            net.bridge_for("projects/p/subnets/storage-network-one"),
            net.bridge_for("projects/p/subnets/storage-network-two")
        );
    }
}

#[cfg(test)]
mod on_the_machines_own_wire {
    use super::{
        tests::{logical, net, on_a_host_bridge, port, subnet},
        *,
    };

    /// A network the operator put on a host bridge is somebody else's to run.
    ///
    /// Holding its gateway would be this platform quietly taking over a network
    /// it did not build, and translating out of it would make every guest on the
    /// house LAN arrive from the node instead of from itself. The guest's
    /// address did not come from here either — whatever serves that wire gave it
    /// one.
    #[test]
    fn a_host_bridged_network_gets_no_gateway_and_no_translation() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);

        // On an ordinary network this node is the first hop.
        assert_eq!(
            segments(&ports, &subnets, &logical(), &taps, &[], &[]).len(),
            1
        );

        // On the machine's own wire it is nothing at all — no bridge of ours, no
        // address on it, and no rule in the NAT table.
        let host = on_a_host_bridge("projects/p/networks/n");
        let segments = segments(&ports, &subnets, &host, &taps, &[], &[]);
        assert!(segments.is_empty(), "{segments:?}");
        assert!(!net().ruleset(&segments).contains("masquerade"));
    }
}

#[cfg(test)]
mod the_datapath_has_to_take_things_away_too {
    use super::*;

    fn net() -> LocalNet {
        LocalNet::new("vbr")
    }

    pub(crate) fn segment(subnet: &str, gateway: &str) -> Segment {
        Segment {
            subnet: subnet.to_string(),
            gateway: gateway.parse().unwrap(),
            prefix_len: 24,
            network: format!(
                "{}/24",
                gateway.rsplit_once('.').unwrap().0.to_string() + ".0"
            ),
            taps: Vec::new(),
            vips: Vec::new(),
            public: Vec::new(),
        }
    }

    /// An empty bridge — nothing enslaved to it, so `removals` may take it.
    pub(crate) fn bridge(name: &str, addresses: &[&str]) -> Bridge {
        Bridge {
            name: name.to_string(),
            addresses: addresses.iter().map(|a| a.to_string()).collect(),
            members: Vec::new(),
        }
    }

    /// The same, still carrying a guest's tap.
    pub(crate) fn bridge_with(name: &str, addresses: &[&str], members: &[&str]) -> Bridge {
        Bridge {
            members: members.iter().map(|m| m.to_string()).collect(),
            ..bridge(name, addresses)
        }
    }

    /// A bridge nothing wants is taken away — unless a guest is still on it.
    ///
    /// The promise was in `removals`' own doc comment and nowhere in its code:
    /// `Bridge` did not carry its members, so "left alone" could not be decided
    /// and every unwanted bridge was deleted. That is survivable when the
    /// control plane really has forgotten one port; it is a node-wide outage
    /// when the *reason* nothing wants the bridge is that a list call failed and
    /// the pass carried on with an empty world.
    #[test]
    fn a_bridge_with_a_guest_still_on_it_is_left_alone() {
        let net = LocalNet::new("vt");
        let name = net.bridge_for("projects/p/subnets/gone");

        // Nothing wants it and nothing is on it: it goes.
        let empty = bridge(&name, &["10.42.0.1/24"]);
        let flat: Vec<String> = net
            .removals(&[], &[empty])
            .iter()
            .map(|s| s.join(" "))
            .collect();
        assert!(
            flat.iter().any(|s| s == &format!("link del {name}")),
            "an empty bridge nothing wants was kept: {flat:?}"
        );

        // Nothing wants it and a tap is still enslaved to it: it stays.
        let carrying = bridge_with(&name, &["10.42.0.1/24"], &["vtportabc123"]);
        let flat: Vec<String> = net
            .removals(&[], &[carrying])
            .iter()
            .map(|s| s.join(" "))
            .collect();
        assert!(
            !flat.iter().any(|s| s.starts_with("link del")),
            "a guest's wire was cut: {flat:?}"
        );
    }

    /// **The link-local the kernel adds is not ours to remove; a v6 global is.**
    ///
    /// The picker filtered on `family == "inet"`, so `removals` never saw a v6
    /// address at all — a v6 gateway that moved stayed for ever, and a deleted
    /// balancer's `/128` was never reclaimed. It filters on `scope == "global"`
    /// now, which takes the v6 global and leaves the `fe80::` the kernel puts
    /// on every bridge itself.
    /// **An advertiser stops when its segment does**, and a segment that comes
    /// back gets one again. The set used to only ever grow: a re-created v6
    /// segment started no new advertiser because its bridge name was already
    /// in it, and a v6-only guest booted with no address and nothing said why.
    #[tokio::test]
    async fn an_advertiser_stops_when_its_segment_does_and_a_returning_one_gets_another() {
        let net = LocalNet::new("vt");
        let subnet = "projects/p1/subnets/six";
        let v6 = Segment {
            subnet: subnet.to_string(),
            gateway: "fd00:1::1".parse().unwrap(),
            prefix_len: 64,
            network: "fd00:1::/64".to_string(),
            taps: Vec::new(),
            vips: Vec::new(),
            public: Vec::new(),
        };
        let device = net.bridge_for(subnet);
        net.advertise(std::slice::from_ref(&v6));
        assert_eq!(net.advertising_on(), vec![device.clone()]);

        net.advertise(&[]);
        assert!(
            net.advertising_on().is_empty(),
            "the advertiser outlived its segment"
        );

        net.advertise(std::slice::from_ref(&v6));
        assert_eq!(
            net.advertising_on(),
            vec![device],
            "a segment that came back got no advertiser"
        );

        // A changed gateway under the same bridge is stop-and-start, because
        // the running task holds its frame by value.
        let moved = Segment {
            gateway: "fd00:9::1".parse().unwrap(),
            ..v6.clone()
        };
        net.advertise(std::slice::from_ref(&moved));
        let held = net.advertising.lock().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(
            held.values().next().unwrap().gateway,
            "fd00:9::1".parse::<std::net::Ipv6Addr>().unwrap()
        );
    }

    /// **A firewall somebody flushed is written again.** The cache used to
    /// compare only the text it had written, so a table that was gone read as
    /// "unchanged" for ever.
    #[test]
    fn a_firewall_somebody_flushed_is_written_again() {
        let recorded = InForce {
            wanted: "table inet x {}".into(),
            kernel: Some("{\"nftables\":[]}".into()),
        };
        // Same intent, the table is gone: not in force.
        assert!(!already_in_force("table inet x {}", Some(&recorded), None));
        // Same intent, the same table: in force.
        assert!(already_in_force(
            "table inet x {}",
            Some(&recorded),
            Some("{\"nftables\":[]}")
        ));
        // Same intent, a table somebody has since added a rule to: written.
        assert!(!already_in_force(
            "table inet x {}",
            Some(&recorded),
            Some("{\"nftables\":[{\"rule\":{}}]}")
        ));
        // A different intent is never in force.
        assert!(!already_in_force(
            "table inet y {}",
            Some(&recorded),
            Some("{\"nftables\":[]}")
        ));
        // Nothing recorded: written.
        assert!(!already_in_force("table inet x {}", None, None));
        // No account either time — an `nft` with no `-j` — falls back to the
        // intent alone, the conservative direction for the counters.
        let blind = InForce {
            wanted: "table inet x {}".into(),
            kernel: None,
        };
        assert!(already_in_force("table inet x {}", Some(&blind), None));
    }

    /// A table that only counted is still the table we wrote: handles and
    /// counters are what change without anybody writing, and they are taken
    /// out before two accounts are compared.
    #[test]
    fn a_table_that_only_counted_is_still_the_table_we_wrote() {
        let mut then = serde_json::json!({"nftables": [
            {"table": {"family": "inet", "name": "velstra-filter", "handle": 12}},
            {"rule": {"chain": "vt-in", "handle": 40, "expr": [
                {"counter": {"packets": 0, "bytes": 0}}, {"drop": null}]}},
        ]});
        let mut now = serde_json::json!({"nftables": [
            {"table": {"family": "inet", "name": "velstra-filter", "handle": 12}},
            {"rule": {"chain": "vt-in", "handle": 40, "expr": [
                {"counter": {"packets": 9001, "bytes": 123456}}, {"drop": null}]}},
        ]});
        settled(&mut then);
        settled(&mut now);
        assert_eq!(
            then, now,
            "a firewall that merely worked would be rewritten every pass"
        );
    }

    /// **A routed public address gets the next hop on its bridge and a host
    /// route back in.** Neither existed: the guest was told to default through
    /// `169.254.1.1` and nothing on this datapath answered for it.
    #[test]
    fn a_routed_address_gets_a_host_route_and_the_next_hop_on_the_bridge() {
        use velstra_cloud_model::resources::{
            FloatingIpSpec, FloatingIpStatus, Subnet, SubnetSpec,
        };

        use super::tests::{meta, port};
        let net = net();
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.5",
        )]);
        let subnets = BTreeMap::from([(
            "projects/p/subnets/s".to_string(),
            Subnet::new(
                meta("projects/p/subnets/s"),
                SubnetSpec {
                    network: "projects/p/networks/n".into(),
                    cidr: "10.42.0.0/24".into(),
                    gateway: "10.42.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                Default::default(),
            ),
        )]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vta".to_string())]);
        let fip = velstra_cloud_model::resources::FloatingIp::new(
            meta("projects/p/floatingips/f"),
            FloatingIpSpec {
                subnet: "subnets/public".into(),
                address: Some("203.0.113.7".into()),
                port: "projects/p/ports/a".into(),
                delivery: velstra_cloud_model::public::Delivery::Routed,
                ..Default::default()
            },
            FloatingIpStatus::default(),
        );
        let segs = segments(&ports, &subnets, &BTreeMap::new(), &taps, &[], &[fip]);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0].public,
            vec!["203.0.113.7".parse::<IpAddr>().unwrap()]
        );

        let bridge = net.bridge_for("projects/p/subnets/s");
        let flat: Vec<String> = net.plan(&segs).iter().map(|s| s.join(" ")).collect();
        assert!(
            flat.contains(&format!("addr replace 169.254.1.1/32 dev {bridge}")),
            "the next hop is not answered for: {flat:?}"
        );
        assert!(
            flat.contains(&format!("route replace 203.0.113.7/32 dev {bridge}")),
            "no way back in to the address: {flat:?}"
        );
        // And the sweep leaves the next hop alone.
        let held = bridge_with(&bridge, &["10.42.0.1/24", "169.254.1.1/32"], &[]);
        let removed: Vec<String> = net
            .removals(&segs, &[held])
            .iter()
            .map(|s| s.join(" "))
            .collect();
        assert!(
            !removed.iter().any(|s| s.contains("169.254.1.1")),
            "the sweep took the next hop off again: {removed:?}"
        );
    }

    #[test]
    fn a_bridges_link_local_is_not_ours_but_its_globals_are() {
        let link = serde_json::json!({
            "ifname": "vtbr0",
            "addr_info": [
                { "local": "10.19.136.1", "prefixlen": 24, "scope": "global", "family": "inet" },
                { "local": "fd00:19::1", "prefixlen": 64, "scope": "global", "family": "inet6" },
                { "local": "fe80::1", "prefixlen": 64, "scope": "link", "family": "inet6" },
            ]
        });
        let got = super::addresses_of_value(&link);
        assert!(got.contains(&"10.19.136.1/24".to_string()), "{got:?}");
        assert!(
            got.contains(&"fd00:19::1/64".to_string()),
            "the v6 global was dropped: {got:?}"
        );
        assert!(
            !got.iter().any(|a| a.starts_with("fe80")),
            "the link-local was kept: {got:?}"
        );
    }

    /// The v6 mirror of the stale-gateway case: a network remade on a new v6
    /// range leaves the old gateway behind on the bridge, and nothing takes it
    /// off because `observed` never reported it.
    #[test]
    fn a_v6_gateway_that_moved_does_not_stay_behind() {
        let net = net();
        let subnet = "projects/p1/subnets/default";
        let name = net.bridge_for(subnet);
        let wanted = Segment {
            subnet: subnet.to_string(),
            gateway: "fd00:1::1".parse().unwrap(),
            prefix_len: 64,
            network: "fd00:1::/64".to_string(),
            taps: Vec::new(),
            vips: Vec::new(),
            public: Vec::new(),
        };
        let observed = vec![bridge(&name, &["fd00:1::1/64", "fd00:9::1/64"])];
        let steps: Vec<String> = net
            .removals(&[wanted], &observed)
            .iter()
            .map(|s| s.join(" "))
            .collect();
        assert!(
            steps
                .iter()
                .any(|s| s == &format!("addr del fd00:9::1/64 dev {name}")),
            "the stale v6 gateway was left behind: {steps:?}"
        );
        assert!(
            !steps.iter().any(|s| s.contains("fd00:1::1")),
            "the current v6 gateway was removed: {steps:?}"
        );
    }

    #[test]
    fn a_gateway_that_moved_does_not_stay_behind() {
        // The live shape, exactly. A project's network was remade with a
        // different range; `addr replace` added the new gateway and left the old
        // one, so `10.19.136.1/24` sat on two bridges at once. The kernel picks
        // one route for a range; it picked the bridge with nothing on it, and
        // every new guest in that project answered `No route to host` while the
        // control plane reported it Running with an address.
        let net = net();
        let wanted = segment("projects/p1/subnets/default", "10.19.138.1");
        let name = net.bridge_for(&wanted.subnet);
        let observed = vec![bridge(&name, &["10.19.138.1/24", "10.19.136.1/24"])];

        let steps = net.removals(&[wanted], &observed);
        assert_eq!(
            steps,
            vec![vec![
                "addr".to_string(),
                "del".to_string(),
                "10.19.136.1/24".to_string(),
                "dev".to_string(),
                name,
            ]],
            "the address that no longer belongs was left on the bridge"
        );
    }

    #[test]
    fn the_gateway_that_belongs_is_left_alone() {
        // Level-triggered: a settled pass does nothing at all, which is what
        // keeps a settled node settled. A removal-and-re-add every thirty
        // seconds would be a gap in every guest's default route every thirty
        // seconds.
        let net = net();
        let wanted = segment("projects/p1/subnets/default", "10.19.136.1");
        let name = net.bridge_for(&wanted.subnet);
        assert!(
            net.removals(&[wanted], &[bridge(&name, &["10.19.136.1/24"])])
                .is_empty()
        );
    }

    #[test]
    fn a_bridge_for_a_network_that_is_gone_goes_too() {
        // Otherwise they accumulate, each still holding a gateway, and the next
        // network to be handed that range collides with a bridge nobody
        // remembers making.
        let net = net();
        let steps = net.removals(&[], &[bridge("vbrdefaultab03", &["10.19.136.1/24"])]);
        assert_eq!(
            steps,
            vec![vec![
                "link".to_string(),
                "del".to_string(),
                "vbrdefaultab03".to_string()
            ]]
        );
    }

    #[test]
    fn nothing_outside_this_platform_is_ever_touched() {
        // The prefix is the whole boundary. A plan that removed interfaces it
        // did not make would take a machine's own networking away — `docker0`,
        // a libvirt bridge, whatever else is on the box.
        let net = net();
        let steps = net.removals(
            &[],
            &[
                bridge("docker0", &["172.17.0.1/16"]),
                bridge("virbr0", &["192.168.122.1/24"]),
            ],
        );
        assert!(
            steps.is_empty(),
            "the datapath proposed removing something that is not ours: {steps:?}"
        );
    }

    #[test]
    fn removals_are_idempotent_like_the_plan_is() {
        let net = net();
        let wanted = segment("projects/p1/subnets/default", "10.19.138.1");
        let name = net.bridge_for(&wanted.subnet);
        let observed = vec![
            bridge(&name, &["10.19.138.1/24", "10.19.136.1/24"]),
            bridge("vbraltab03", &["10.77.0.1/24"]),
        ];
        assert_eq!(
            net.removals(std::slice::from_ref(&wanted), &observed),
            net.removals(&[wanted], &observed)
        );
    }
}

#[cfg(test)]
mod v6_tests {
    use super::{tests::*, *};

    /// A v6 subnet used to be dropped on the floor: the gateway would not parse
    /// as an `Ipv4Addr`, the loop moved on, and the segment was never built —
    /// no bridge, no gateway, no taps enslaved, and nothing anywhere saying so.
    /// The model, the allocator and the guest's own netplan have all handled v6
    /// for as long as they have existed; this was the one place that quietly
    /// did not.
    #[test]
    fn a_v6_subnet_gets_a_segment_like_any_other() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s6",
            "fd00:1::10",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s6", "fd00:1::/64", "fd00:1::1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vtap1".to_string())]);

        let segments = segments(&ports, &subnets, &logical(), &taps, &[], &[]);
        assert_eq!(segments.len(), 1, "a v6 subnet was skipped: {segments:?}");
        assert_eq!(segments[0].gateway.to_string(), "fd00:1::1");
        assert_eq!(segments[0].prefix_len, 64);
        assert_eq!(segments[0].taps, vec!["vtap1".to_string()]);
    }

    /// And it is **routed**, not translated. Masquerading IPv6 is a thing
    /// people do and a thing every v6 document asks them not to: the address
    /// space exists so a host can be reached, and a cloud that hides its guests
    /// behind one address has thrown that away.
    #[test]
    fn a_v6_segment_is_routed_and_not_translated() {
        let net = LocalNet::new("vt");
        let v6 = Segment {
            subnet: "projects/p/subnets/s6".into(),
            gateway: "fd00:1::1".parse().unwrap(),
            prefix_len: 64,
            network: "fd00:1::/64".into(),
            taps: vec!["vtap1".into()],
            vips: Vec::new(),
            public: Vec::new(),
        };
        let v4 = Segment {
            subnet: "projects/p/subnets/s4".into(),
            gateway: "10.19.136.1".parse().unwrap(),
            prefix_len: 24,
            network: "10.19.136.0/24".into(),
            taps: vec!["vtap2".into()],
            vips: Vec::new(),
            public: Vec::new(),
        };
        let rules = net.ruleset(&[v6, v4]);
        assert!(
            rules.contains("ip saddr 10.19.136.0/24"),
            "the v4 segment lost its translation: {rules}"
        );
        assert!(
            !rules.contains("fd00:1::/64"),
            "a v6 range was put behind NAT: {rules}"
        );
    }

    /// A gateway of one family on a range of the other is a subnet nobody could
    /// use, and putting it on a bridge would be this node claiming an address
    /// belonging to neither.
    #[test]
    fn a_gateway_of_the_wrong_family_builds_nothing() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s6",
            "fd00:1::10",
        )]);
        let subnets = BTreeMap::from([subnet(
            "projects/p/subnets/s6",
            "fd00:1::/64",
            "10.19.136.1",
        )]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vtap1".to_string())]);
        assert!(segments(&ports, &subnets, &logical(), &taps, &[], &[]).is_empty());
    }
}

#[cfg(test)]
mod balancer_addresses {
    use super::{
        tests::*,
        the_datapath_has_to_take_things_away_too::{bridge, segment},
        *,
    };

    fn a_balancer(subnet: &str, vip: &str) -> velstra_cloud_model::loadbalancer::LoadBalancer {
        velstra_cloud_model::resources::Resource::new(
            velstra_cloud_model::meta::Meta::new(
                "projects/p/load-balancers/web".parse().unwrap(),
                velstra_cloud_model::meta::Placement::new("eu", "cell-1"),
            ),
            velstra_cloud_model::loadbalancer::LoadBalancerSpec {
                network: "projects/p/networks/n".into(),
                subnet: subnet.to_string(),
                vip: Some(vip.to_string()),
                listeners: Vec::new(),
                members: Vec::new(),
                session_affinity: false,
                draining: Default::default(),
            },
            Default::default(),
        )
    }

    /// A balancer's address goes on the bridge of the segment it belongs to, as
    /// a host route: it is an address this machine answers for, not one it
    /// hands out. Without it the listener has nothing to bind and the packets
    /// never arrive.
    #[test]
    fn a_balancers_address_is_held_on_the_bridge() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vtap1".to_string())]);

        let segments = segments(
            &ports,
            &subnets,
            &logical(),
            &taps,
            &[a_balancer("projects/p/subnets/s", "10.42.0.9")],
            &[],
        );
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].vips.len(), 1);

        let plan = LocalNet::new("vt").plan(&segments);
        let flat: Vec<String> = plan.iter().map(|s| s.join(" ")).collect();
        assert!(
            flat.iter().any(|s| s.contains("addr replace 10.42.0.9/32")),
            "the balancer's address is not held: {flat:?}"
        );
    }

    /// And the sweep leaves it there. The removal rule takes off anything on
    /// one of our bridges that is not the gateway — which, before it knew about
    /// balancers, meant the address went on in one step and came off in the
    /// next, for ever.
    #[test]
    fn the_sweep_does_not_take_a_balancers_address_back_off() {
        let net = LocalNet::new("vt");
        let mut segment = segment("projects/p/subnets/s", "10.42.0.1");
        segment.vips = vec!["10.42.0.9".parse().unwrap()];
        let held = bridge(
            &net.bridge_for("projects/p/subnets/s"),
            &["10.42.0.1/24", "10.42.0.9/32"],
        );

        let steps = net.removals(std::slice::from_ref(&segment), &[held]);
        let flat: Vec<String> = steps.iter().map(|s| s.join(" ")).collect();
        assert!(
            !flat.iter().any(|s| s.contains("10.42.0.9")),
            "the sweep took the balancer's address off again: {flat:?}"
        );

        // A stale address that belongs to nothing still goes.
        let stale = bridge(
            &net.bridge_for("projects/p/subnets/s"),
            &["10.42.0.1/24", "10.42.0.9/32", "10.9.9.9/32"],
        );
        let steps = net.removals(&[segment], &[stale]);
        let flat: Vec<String> = steps.iter().map(|s| s.join(" ")).collect();
        assert!(flat.iter().any(|s| s.contains("10.9.9.9")), "{flat:?}");
    }

    /// A balancer on a subnet no guest of this node is on is not this node's to
    /// answer for: its address belongs wherever its members are.
    #[test]
    fn a_balancer_elsewhere_is_not_held_here() {
        let ports = BTreeMap::from([port(
            "projects/p/ports/a",
            "projects/p/subnets/s",
            "10.42.0.2",
        )]);
        let subnets = BTreeMap::from([subnet("projects/p/subnets/s", "10.42.0.0/24", "10.42.0.1")]);
        let taps = BTreeMap::from([("projects/p/ports/a".to_string(), "vtap1".to_string())]);
        let segments = segments(
            &ports,
            &subnets,
            &logical(),
            &taps,
            &[a_balancer("projects/p/subnets/somewhere-else", "10.99.0.9")],
            &[],
        );
        assert!(segments[0].vips.is_empty());
    }
    /// An unchanged firewall is not written again — because writing it resets
    /// every counter, and a counter that resets every few seconds is a
    /// firewall that reports it has never dropped anything.
    #[tokio::test]
    async fn an_unchanged_firewall_is_not_rewritten() {
        let mut net = LocalNet::new("vt");
        // A program that cannot be run: the first write fails, so nothing is
        // remembered, and the second must try again rather than conclude the
        // ruleset is in force.
        net.nft = "/nonexistent/bin/nft".to_string();
        let guarded = vec![crate::nftfilter::Guarded {
            mac: None,
            port: "projects/p1/ports/web".into(),
            tap: "vtweb1a2b".into(),
            addresses: vec!["10.19.136.5".into()],
            rules: vec![velstra_cloud_model::security::ResolvedRule {
                direction: velstra_cloud_model::security::Direction::Ingress,
                protocol: velstra_cloud_model::security::Protocol::Tcp,
                ports: Some(velstra_cloud_model::security::PortRange { from: 443, to: 443 }),
                remote: "0.0.0.0/0".into(),
            }],
        }];
        assert!(net.filter(&guarded).await.is_err());
        assert!(
            net.filter(&guarded).await.is_err(),
            "a write that failed was remembered as in force"
        );

        // And a write that took is remembered, so the same ruleset costs
        // nothing on the next pass.
        net.nft = "true".to_string();
        net.filter(&guarded).await.expect("this one takes");
        net.nft = "/nonexistent/bin/nft".to_string();
        net.filter(&guarded)
            .await
            .expect("an unchanged ruleset was written again");
    }
}
