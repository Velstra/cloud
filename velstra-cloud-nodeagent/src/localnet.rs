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

use std::{collections::BTreeMap, net::Ipv4Addr};

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
    /// IPv4 only, as `10.19.136.1/24`. The gateway is the only address this
    /// platform puts on a bridge, so anything else here is something to remove.
    pub addresses: Vec<String>,
}

/// One segment with a guest on this node: where it is, and who is on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The subnet's resource name. Identity, and the bridge's alias.
    pub subnet: String,
    /// The address this node holds on the segment — the gateway the guests were
    /// handed. Held here or held by nobody.
    pub gateway: Ipv4Addr,
    /// How much of the world is on the link, from the subnet's CIDR.
    pub prefix_len: u8,
    /// The range, for the one NAT rule.
    pub network: String,
    /// The taps on this node carrying ports on this segment, in port order.
    pub taps: Vec<String>,
}

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
}

impl LocalNet {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            ip: "ip".to_string(),
            nft: "nft".to_string(),
        }
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
    pub fn ruleset(&self, segments: &[Segment]) -> String {
        let mut out = String::new();
        out.push_str(&format!("add table ip {TABLE}\n"));
        out.push_str(&format!("delete table ip {TABLE}\n"));
        out.push_str(&format!("table ip {TABLE} {{\n"));
        out.push_str(
            "  chain postrouting {\n    type nat hook postrouting priority srcnat; policy accept;\n",
        );
        for segment in segments {
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
                let addresses = link
                    .get("addr_info")?
                    .as_array()?
                    .iter()
                    .filter(|a| a.get("family").and_then(|f| f.as_str()) == Some("inet"))
                    .filter_map(|a| {
                        Some(format!(
                            "{}/{}",
                            a.get("local")?.as_str()?,
                            a.get("prefixlen")?.as_u64()?
                        ))
                    })
                    .collect();
                Some(Bridge { name, addresses })
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
                    let keep = format!("{}/{}", segment.gateway, segment.prefix_len);
                    for address in &bridge.addresses {
                        if address != &keep {
                            steps.push(step(["addr", "del", address, "dev", &bridge.name]));
                        }
                    }
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
    pub async fn apply(&self, segments: &[Segment]) -> Result<()> {
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
        forwarding_on().await
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
    let path = "/proc/sys/net/ipv4/ip_forward";
    tokio::fs::write(path, b"1\n")
        .await
        .map_err(|e| HostError::failed(format!("writing {path}: {e} — this needs CAP_NET_ADMIN")))
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
        let Ok(gateway) = subnet.spec.gateway.parse::<Ipv4Addr>() else {
            continue;
        };
        let segment = by_subnet
            .entry(port.spec.subnet.clone())
            .or_insert_with(|| Segment {
                subnet: port.spec.subnet.clone(),
                gateway,
                prefix_len: cidr.prefix_len,
                network: format!("{}/{}", cidr.network(), cidr.prefix_len),
                taps: Vec::new(),
            });
        if !segment.taps.iter().any(|t| t == tap) {
            segment.taps.push(tap.clone());
        }
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

        let segments = segments(&ports, &subnets, &logical(), &taps);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].taps, ["vta", "vtb"]);
        assert_eq!(segments[0].network, "10.42.0.0/24");
        assert_eq!(
            segments[0].gateway,
            "10.42.0.1".parse::<Ipv4Addr>().unwrap()
        );
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

        let segments = segments(&ports, &subnets, &logical(), &taps);
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
        assert!(segments(&ports, &subnets, &logical(), &taps).is_empty());
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
        assert!(segments(&ports, &subnets, &logical(), &BTreeMap::new()).is_empty());
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
        assert_eq!(segments(&ports, &subnets, &logical(), &taps).len(), 1);
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
        let segments = segments(&ports, &subnets, &logical(), &taps);
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
        let full = net.ruleset(&segments(&ports, &subnets, &logical(), &taps));
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
        assert_eq!(segments(&ports, &subnets, &logical(), &taps).len(), 1);

        // On the machine's own wire it is nothing at all — no bridge of ours, no
        // address on it, and no rule in the NAT table.
        let host = on_a_host_bridge("projects/p/networks/n");
        let segments = segments(&ports, &subnets, &host, &taps);
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

    fn segment(subnet: &str, gateway: &str) -> Segment {
        Segment {
            subnet: subnet.to_string(),
            gateway: gateway.parse().unwrap(),
            prefix_len: 24,
            network: format!(
                "{}/24",
                gateway.rsplit_once('.').unwrap().0.to_string() + ".0"
            ),
            taps: Vec::new(),
        }
    }

    fn bridge(name: &str, addresses: &[&str]) -> Bridge {
        Bridge {
            name: name.to_string(),
            addresses: addresses.iter().map(|a| a.to_string()).collect(),
        }
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
