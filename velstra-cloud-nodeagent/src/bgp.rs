//! Announcing the cell's public prefixes to the router in front of it.
//!
//! A cell's floating addresses are only addresses if something outside the
//! cell routes toward it, and typing static routes into the firewall is the
//! step everybody forgets on the day the second prefix arrives. So the
//! operator writes a `bgp-peers` object — which machine speaks, to whom, as
//! which AS — and the gateway's agent keeps the host's routing daemon saying
//! the truth: every external subnet, and a host route for every floating
//! address that is actually in front of something.
//!
//! **Derived, never listed.** What gets announced is computed from the same
//! objects `:explainReach` reads, so the router ahead of the cell and the
//! operator's console cannot disagree about what the cell claims to be.
//!
//! The daemon is FRR, spoken to the way FRR wants to be spoken to: a rendered
//! `frr.conf` and a reload. Rendering the whole file rather than issuing
//! deltas is the same level-triggered argument as everywhere else — the file
//! *is* the desired state, and a daemon restarted by hand converges to it
//! instead of to the sum of whatever deltas it happened to hear.

use std::collections::BTreeMap;

use async_trait::async_trait;
use velstra_cloud_model::resources::{BgpPeer, FloatingIp, Network, Subnet};

use crate::host::Result;

/// One session this machine should be speaking, plus what to say on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BgpDesired {
    pub sessions: Vec<BgpSession>,
    /// Prefixes announced on every session: the external subnets, verbatim.
    pub networks_v4: Vec<String>,
    pub networks_v6: Vec<String>,
    /// Host routes for the floating addresses this cell answers for —
    /// `203.0.113.7/32`, `2001:db8::7/128`.
    pub hosts_v4: Vec<String>,
    pub hosts_v6: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BgpSession {
    pub peer: String,
    pub peer_as: u32,
    pub local_as: u32,
}

/// What the daemon says about one neighbour.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeerObservation {
    /// FRR's own word — `Established`, `Active`, `Idle`, `Connect` — reported
    /// verbatim, because inventing a vocabulary over BGP's would leave an
    /// operator translating back at three in the morning.
    pub state: String,
    /// How many prefixes we are sending it.
    pub announced: u32,
}

/// The half of the host that speaks BGP.
///
/// A trait for the same reason the VMM and the datapath are: the agent's
/// reconcile is about *what* to say, and a test that needed a routing daemon
/// to say it would not be a test of the reconcile.
#[async_trait]
pub trait BgpSpeaker: Send + Sync + 'static {
    /// Make the daemon say exactly this. Idempotent: applying the same
    /// desired state twice is once.
    async fn apply(&self, desired: &BgpDesired) -> Result<()>;
    /// What the daemon reports, keyed by neighbour address.
    async fn observe(&self) -> Result<BTreeMap<String, PeerObservation>>;
}

/// Compute what this node should be announcing.
///
/// From the objects, every pass: the external networks' subnets whole, and a
/// host route per floating address that names a port — an address in front of
/// nothing is a reservation, and announcing a reservation blackholes whoever
/// follows it. Sorted, so equality between passes means what it says.
pub fn desired_for(
    me: &str,
    peers: &[BgpPeer],
    networks: &[Network],
    subnets: &[Subnet],
    floating: &[FloatingIp],
) -> BgpDesired {
    let mut desired = BgpDesired::default();
    for p in peers {
        if p.spec.node != me || p.meta.is_deleting() {
            continue;
        }
        desired.sessions.push(BgpSession {
            peer: p.spec.peer.clone(),
            peer_as: p.spec.peer_as,
            local_as: p.spec.local_as,
        });
    }
    if desired.sessions.is_empty() {
        return desired;
    }
    let external: Vec<String> = networks
        .iter()
        .filter(|n| n.spec.external)
        .map(|n| n.meta.name.to_string())
        .collect();
    for s in subnets {
        if !external.contains(&s.spec.network) || s.spec.cidr.is_empty() {
            continue;
        }
        if s.spec.cidr.contains(':') {
            desired.networks_v6.push(s.spec.cidr.clone());
        } else {
            desired.networks_v4.push(s.spec.cidr.clone());
        }
    }
    for f in floating {
        if f.meta.is_deleting() || f.spec.port.is_empty() {
            continue;
        }
        let Some(address) = f.spec.address.as_deref().filter(|a| !a.is_empty()) else {
            continue;
        };
        if address.contains(':') {
            desired.hosts_v6.push(format!("{address}/128"));
        } else {
            desired.hosts_v4.push(format!("{address}/32"));
        }
    }
    desired.sessions.sort_by(|a, b| a.peer.cmp(&b.peer));
    desired.networks_v4.sort();
    desired.networks_v6.sort();
    desired.hosts_v4.sort();
    desired.hosts_v6.sort();
    desired.networks_v4.dedup();
    desired.networks_v6.dedup();
    desired.hosts_v4.dedup();
    desired.hosts_v6.dedup();
    desired
}

/// The `frr.conf` that says `desired`, whole.
///
/// One router block per local AS (FRR allows one `router bgp` per AS/VRF), a
/// neighbour per session, and both address families announcing everything of
/// their family. `redistribute` is deliberately absent: this file announces
/// what the *cell* claims, not whatever the kernel picked up.
pub fn render_frr(desired: &BgpDesired) -> String {
    use std::fmt::Write;
    let mut out = String::from(
        "! Written by velstra-cloud-nodeagent. Edits are overwritten;\n\
         ! the bgp-peers objects are where this file comes from.\n\
         frr defaults traditional\n\
         log syslog informational\n\
         !\n",
    );
    let mut by_as: BTreeMap<u32, Vec<&BgpSession>> = BTreeMap::new();
    for s in &desired.sessions {
        by_as.entry(s.local_as).or_default().push(s);
    }
    for (local_as, sessions) in by_as {
        let _ = writeln!(out, "router bgp {local_as}");
        // The daemon must not guess an id from whichever interface it saw
        // first: derive one stable answer from the AS so two gateways never
        // collide by accident. An operator who needs a specific id sets up
        // FRR's own config for it — this file is the derived truth, not a
        // hand-edited one.
        let _ = writeln!(
            out,
            " bgp router-id 10.255.{}.{}",
            (local_as >> 8) & 0xff,
            local_as & 0xff
        );
        for s in &sessions {
            let _ = writeln!(out, " neighbor {} remote-as {}", s.peer, s.peer_as);
        }
        let _ = writeln!(out, " address-family ipv4 unicast");
        for p in desired.networks_v4.iter().chain(&desired.hosts_v4) {
            let _ = writeln!(out, "  network {p}");
        }
        for s in &sessions {
            if !s.peer.contains(':') {
                let _ = writeln!(out, "  neighbor {} activate", s.peer);
            }
        }
        let _ = writeln!(out, " exit-address-family");
        let _ = writeln!(out, " address-family ipv6 unicast");
        for p in desired.networks_v6.iter().chain(&desired.hosts_v6) {
            let _ = writeln!(out, "  network {p}");
        }
        for s in &sessions {
            let _ = writeln!(out, "  neighbor {} activate", s.peer);
        }
        let _ = writeln!(out, " exit-address-family");
        let _ = writeln!(out, "exit");
    }
    out
}

/// FRR on this host: the rendered file, a reload, and `vtysh` for the answers.
pub struct FrrSpeaker {
    /// `/etc/frr/frr.conf` in production; a scratch path in tests.
    pub config: std::path::PathBuf,
}

impl FrrSpeaker {
    pub fn new() -> Self {
        Self {
            config: "/etc/frr/frr.conf".into(),
        }
    }
}

impl Default for FrrSpeaker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BgpSpeaker for FrrSpeaker {
    async fn apply(&self, desired: &BgpDesired) -> Result<()> {
        // Debian ships FRR with `bgpd=no`, and a daemon that is not started
        // reads every config in silence. Turned on here, next to the file that
        // needs it, so "install frr" is the whole of what an operator does by
        // hand. Only ever no→yes: this agent starts daemons it needs and stops
        // none it does not own.
        let daemons = self.config.with_file_name("daemons");
        if let Ok(current) = tokio::fs::read_to_string(&daemons).await {
            if current.contains("bgpd=no") {
                let turned_on = current.replace("bgpd=no", "bgpd=yes");
                tokio::fs::write(&daemons, turned_on).await.map_err(|e| {
                    crate::host::HostError::failed(format!(
                        "could not enable bgpd in {}: {e}",
                        daemons.display()
                    ))
                })?;
                let _ = tokio::process::Command::new("systemctl")
                    .args(["restart", "frr"])
                    .output()
                    .await;
            }
        }
        let rendered = render_frr(desired);
        let current = tokio::fs::read_to_string(&self.config)
            .await
            .unwrap_or_default();
        if current == rendered {
            return Ok(());
        }
        tokio::fs::write(&self.config, rendered).await.map_err(|e| {
            crate::host::HostError::failed(format!(
                "could not write {}: {e}",
                self.config.display()
            ))
        })?;
        // `reload`, not `restart`: FRR diffs the file against its running
        // state, so an unchanged neighbour keeps its session — a restart on
        // every prefix change would flap the very announcements this exists
        // to keep steady.
        let output = tokio::process::Command::new("systemctl")
            .args(["reload-or-restart", "frr"])
            .output()
            .await
            .map_err(|e| crate::host::HostError::failed(format!("systemctl: {e}")))?;
        if !output.status.success() {
            return Err(crate::host::HostError::failed(format!(
                "frr would not take the config: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn observe(&self) -> Result<BTreeMap<String, PeerObservation>> {
        let output = tokio::process::Command::new("vtysh")
            .args(["-c", "show bgp summary json"])
            .output()
            .await
            .map_err(|e| crate::host::HostError::failed(format!("vtysh: {e}")))?;
        if !output.status.success() {
            return Err(crate::host::HostError::failed(format!(
                "vtysh would not answer: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| crate::host::HostError::failed(format!("bgp summary was not json: {e}")))?;
        let mut peers = BTreeMap::new();
        // Both families: a v4 neighbour carrying the v6 family appears twice,
        // and `Established` on either is the session being up.
        for family in ["ipv4Unicast", "ipv6Unicast"] {
            let Some(list) = parsed.get(family).and_then(|f| f.get("peers")) else {
                continue;
            };
            let Some(map) = list.as_object() else { continue };
            for (peer, p) in map {
                let state = p
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let announced = p.get("pfxSnt").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let entry: &mut PeerObservation = peers.entry(peer.clone()).or_default();
                if entry.state != "Established" {
                    entry.state = state;
                }
                entry.announced += announced as u32;
            }
        }
        Ok(peers)
    }
}

#[cfg(test)]
mod what_the_cell_announces {
    use super::*;
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{
            BgpPeerSpec, BgpPeerStatus, FloatingIpSpec, FloatingIpStatus, NetworkSpec,
            NetworkStatus, Resource, SubnetSpec, SubnetStatus,
        },
    };

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    fn peer(name: &str, node: &str) -> BgpPeer {
        Resource::new(
            meta(name),
            BgpPeerSpec {
                peer: "10.10.10.1".into(),
                peer_as: 65000,
                local_as: 65010,
                node: node.into(),
                description: String::new(),
            },
            BgpPeerStatus::default(),
        )
    }

    fn network(name: &str, external: bool) -> Network {
        let spec = NetworkSpec { external, ..Default::default() };
        Resource::new(meta(name), spec, NetworkStatus::default())
    }

    fn subnet(name: &str, network: &str, cidr: &str) -> Subnet {
        Resource::new(
            meta(name),
            SubnetSpec {
                network: network.into(),
                cidr: cidr.into(),
                gateway: String::new(),
                dns: vec![],
                reserved: vec![],
            },
            SubnetStatus::default(),
        )
    }

    fn floating(name: &str, address: &str, port: &str) -> FloatingIp {
        let spec = FloatingIpSpec {
            address: Some(address.to_string()),
            port: port.to_string(),
            ..Default::default()
        };
        Resource::new(meta(name), spec, FloatingIpStatus::default())
    }

    #[test]
    fn announcements_are_derived_not_listed() {
        let desired = desired_for(
            "gw-1",
            &[peer("bgp-peers/edge", "gw-1"), peer("bgp-peers/other", "gw-2")],
            &[network("networks/public", true), network("networks/lan", false)],
            &[
                subnet("subnets/public-v4", "networks/public", "203.0.113.0/24"),
                subnet("subnets/public-v6", "networks/public", "2001:db8:77::/64"),
                subnet("subnets/lan", "networks/lan", "10.0.0.0/24"),
            ],
            &[
                floating("projects/p1/floatingips/a", "203.0.113.7", "projects/p1/ports/x"),
                // In front of nothing: a reservation, not a reachable address.
                floating("projects/p1/floatingips/b", "203.0.113.8", ""),
            ],
        );
        // Only this node's session; the other machine's is not ours to speak.
        assert_eq!(desired.sessions.len(), 1);
        assert_eq!(desired.networks_v4, vec!["203.0.113.0/24"]);
        assert_eq!(desired.networks_v6, vec!["2001:db8:77::/64"]);
        assert_eq!(desired.hosts_v4, vec!["203.0.113.7/32"]);
        assert!(desired.hosts_v6.is_empty());
        // The tenant network stays the cell's own business.
        assert!(!desired.networks_v4.contains(&"10.0.0.0/24".to_string()));
    }

    #[test]
    fn a_node_with_no_session_says_nothing_at_all() {
        let desired = desired_for(
            "gw-1",
            &[peer("bgp-peers/edge", "gw-2")],
            &[network("networks/public", true)],
            &[subnet("subnets/public-v4", "networks/public", "203.0.113.0/24")],
            &[],
        );
        assert_eq!(desired, BgpDesired::default());
    }

    #[test]
    fn the_rendered_config_is_stable_and_says_the_prefixes() {
        let desired = desired_for(
            "gw-1",
            &[peer("bgp-peers/edge", "gw-1")],
            &[network("networks/public", true)],
            &[
                subnet("subnets/public-v4", "networks/public", "203.0.113.0/24"),
                subnet("subnets/public-v6", "networks/public", "2001:db8:77::/64"),
            ],
            &[floating("projects/p1/floatingips/a", "203.0.113.7", "projects/p1/ports/x")],
        );
        let conf = render_frr(&desired);
        assert!(conf.contains("router bgp 65010"), "{conf}");
        assert!(conf.contains("neighbor 10.10.10.1 remote-as 65000"), "{conf}");
        assert!(conf.contains("  network 203.0.113.0/24"), "{conf}");
        assert!(conf.contains("  network 203.0.113.7/32"), "{conf}");
        assert!(conf.contains("  network 2001:db8:77::/64"), "{conf}");
        // Stable: the same objects render the same bytes, which is what lets
        // the speaker skip the reload on a settled cell.
        assert_eq!(conf, render_frr(&desired));
    }
}
