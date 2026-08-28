//! A datapath that gives a guest a wire.
//!
//! Until this existed the only [`Datapath`] was the fake, and the consequence
//! was sharper than it sounds: both VMM backends name a port's tap device on
//! their command line (`-netdev tap,ifname=…,script=no`, `--net tap=…`) and both
//! require it to **already exist**. So an instance with a port could not start
//! at all, and every guest this platform had ever run was one with no NIC.
//!
//! ## What it does and does not do
//!
//! It creates the tap, brings it up, labels it with the port it carries, and
//! reports it. It enforces **nothing** — no VNI, no security groups, no ceiling.
//! That is not a gap left quietly: a port that asks for rules or a rate limit is
//! **refused** here rather than programmed and silently unfiltered, because a
//! port reporting `programmed: true` while its security groups apply to nothing
//! is the worst shape this platform can take. The enforcement half lives in the
//! Velstra fabric and arrives through this same trait.
//!
//! ## Where the port ↔ tap mapping lives
//!
//! [`Datapath::observe`] is keyed by port name, and a tap name cannot be turned
//! back into one. The obvious fix — remember it in this process — is the one
//! thing [`crate::host`] rules out: nothing in this crate remembers what it did,
//! because that is the recovery model and not an optimisation.
//!
//! So the kernel remembers instead. `ip link set <tap> alias <port>` puts the
//! port's resource name where `ip link show` reads it back, and it survives this
//! process, its crash, and its replacement by a different build. The machine
//! stays the only source of truth about the machine, and a restarted agent adopts
//! the taps it made last time instead of orphaning them.
//!
//! ## The name has to be the same on every node, and that is not a preference
//!
//! A live migration carries the guest's *configuration* to the destination, and
//! for both VMMs that configuration names the tap. The destination does not
//! substitute one of its own: it opens the device the source's command line
//! names. So a port whose tap is `vtweb1a2b` here must be `vtweb1a2b` there, or
//! the guest cannot be moved.
//!
//! Deriving the name from the port rather than allocating one is what makes that
//! true for free — every node computes the same name from the same port, with
//! nothing to agree on and nothing to distribute. The one way to break it is to
//! give two nodes different prefixes, which is why `--tap-prefix` says so.
//!
//! Measured, not assumed. Two Cloud Hypervisor VMMs on one machine, each in its
//! own network namespace with its own `vtnic0`, migrate a running guest between
//! them successfully. The same two VMMs sharing one machine's single `vtnic0`
//! fail — the destination cannot open a device the source still holds, and the
//! receiver aborts with "Failed to receive migratable component snapshot". Which
//! is also why a migration cannot be exercised end to end on one host once the
//! guest has a NIC: the two "nodes" are one machine, and there is one device.
//!
//! Everything here goes through `ip` rather than `/sys/class/net`, and that is
//! deliberate: sysfs shows the network namespace it was *mounted* in, not the one
//! this process is in. An agent in its own namespace — which is how the whole
//! thing can be exercised without root — would read the host's interfaces out of
//! sysfs and conclude that a tap it had just made did not exist.
//!
//! ## Privilege
//!
//! Creating a tap needs `CAP_NET_ADMIN`. There are two ways to have it, and both
//! are supported on purpose:
//!
//! * the agent holds the capability, and creates taps itself; or
//! * something else creates them — a host provisioning step, an operator, a
//!   `systemd` service — and the agent finds them.
//!
//! The second is what lets a node agent run unprivileged, which is worth having:
//! the process reachable from the network is the last one to hand
//! `CAP_NET_ADMIN` to. A tap that is already there is used as it is; only its
//! label and its state are set, and if even that is refused it is reported rather
//! than guessed at.
//!
//! A tap must also be openable by whoever runs the VMM, which is what `ip tuntap
//! add … user <uid>` arranges for the taps this creates.

use std::collections::BTreeMap;

use async_trait::async_trait;
use velstra_cloud_model::{
    resources::{NetworkSpec, PortSpec},
    security::ResolvedRule,
};

use crate::host::{Datapath, HostError, ProgrammedPort, Result};

/// The longest an interface name may be: `IFNAMSIZ` is 16, less the terminator.
const IFNAMSIZ: usize = 15;

pub struct TapDatapath {
    /// Prefix for the tap names this node makes, and the marker by which it
    /// recognises its own. Short, because the whole name has to fit in fifteen
    /// characters along with something that identifies the port.
    prefix: String,
    /// The uid a created tap is owned by, so an unprivileged VMM can open it.
    /// `None` leaves it to this process's own uid, which is what a root agent
    /// running root guests wants.
    owner: Option<u32>,
    /// The program that talks to the kernel. Only ever `ip`, except in the test
    /// that asks what happens when it cannot be run at all — which is the
    /// difference between "no ports" and "no answer", and the agent acts very
    /// differently on the two.
    ip: String,
}

impl TapDatapath {
    pub fn new(prefix: &str, owner: Option<u32>) -> Self {
        Self {
            prefix: prefix.to_string(),
            owner,
            ip: "ip".to_string(),
        }
    }

    /// The tap device carrying `port`.
    ///
    /// Derived from the name rather than allocated, so it needs nothing written
    /// down: the same port is the same device on every pass. The leaf is what
    /// identifies a port to a person reading `ip link`, and the digest is what
    /// stops two ports whose names differ only past the fifteenth character from
    /// sharing a wire — which, between two tenants, is the whole ballgame.
    pub fn tap_for(&self, port: &str) -> String {
        let leaf = port.rsplit('/').next().unwrap_or(port);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(port.as_bytes());
        let tail = format!("{:02x}{:02x}", digest[0], digest[1]);
        let room = IFNAMSIZ.saturating_sub(self.prefix.len() + tail.len());
        let head: String = leaf
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(room)
            .collect();
        format!("{}{head}{tail}", self.prefix)
    }

    async fn run(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        tokio::process::Command::new(&self.ip)
            .args(args)
            .output()
            .await
    }

    /// Change the machine, and say what it refused in a sentence somebody can
    /// act on.
    async fn ip(&self, args: &[&str]) -> Result<()> {
        let output = self
            .run(args)
            .await
            .map_err(|e| HostError::failed(format!("running `ip {}`: {e}", args.join(" "))))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // Named, because the bare message is not enough to act on: "operation
        // not permitted" from `ip tuntap` means one specific missing capability,
        // and whoever reads this on a Port should not have to know which.
        let hint = if stderr.contains("not permitted") {
            " — this needs CAP_NET_ADMIN. Give the agent that capability, or create the taps \
             outside it: an existing tap is adopted as it is."
        } else {
            ""
        };
        Err(HostError::failed(format!(
            "`ip {}` failed: {stderr}{hint}",
            args.join(" ")
        )))
    }

    /// Whether an interface is there. A question, not a change, so a refusal is
    /// an answer of `false` rather than an error.
    async fn present(&self, tap: &str) -> bool {
        self.run(&["-o", "link", "show", "dev", tap])
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// The tap name and the port it says it carries, from one line of `ip -o link
/// show`.
///
/// `-o` folds the whole entry onto one line, separating what would have been
/// continuations with a backslash, so the alias is on it:
///
/// ```text
/// 3: vtweb1a2b: <BROADCAST,MULTICAST,UP> mtu 1500 …\    link/ether …\    alias projects/p1/ports/web
/// ```
fn parse_link(line: &str) -> Option<(String, Option<String>)> {
    let (_index, rest) = line.split_once(':')?;
    let rest = rest.trim_start();
    let (name, rest) = rest.split_once(':')?;
    // `eth0@if12` for one end of a veth, and the part after the `@` is not the
    // interface's name.
    let name = name.split('@').next()?.trim().to_string();
    let alias = rest
        .split('\\')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("alias "))
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    Some((name, alias))
}

#[async_trait]
impl Datapath for TapDatapath {
    /// The ports this machine is carrying, read from the kernel.
    ///
    /// The rules come back empty because none are in force, and that is the
    /// truthful answer rather than an omission: the agent compares what it wants
    /// with what is here, so claiming rules would be the way to make it stop
    /// asking for them.
    async fn observe(&self) -> Result<BTreeMap<String, ProgrammedPort>> {
        // An error, never an empty map. "I could not ask" and "this node carries
        // nothing" are the same value to a caller that conflates them, and the
        // agent would act on the second by tearing down every port it believes
        // in.
        let output = self
            .run(&["-o", "link", "show"])
            .await
            .map_err(|e| HostError::failed(format!("running `ip -o link show`: {e}")))?;
        if !output.status.success() {
            return Err(HostError::failed(format!(
                "`ip -o link show` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let listing = String::from_utf8_lossy(&output.stdout);
        let mut out = BTreeMap::new();
        for line in listing.lines() {
            let Some((tap, alias)) = parse_link(line) else {
                continue;
            };
            if !tap.starts_with(&self.prefix) {
                continue;
            }
            // Only a tap that says which port it carries counts. An unlabelled
            // one is somebody else's, or the remains of a create that died
            // between the two commands, and either way naming a port for it
            // would be a guess.
            let Some(port) = alias else { continue };
            out.insert(
                port,
                ProgrammedPort {
                    tap,
                    rules: Vec::new(),
                },
            );
        }
        Ok(out)
    }

    async fn program(
        &self,
        port: &str,
        spec: &PortSpec,
        network: &NetworkSpec,
        rules: &[ResolvedRule],
    ) -> Result<String> {
        // Refused rather than under-enforced. A port whose security groups came
        // to something, programmed by a datapath that filters nothing, would
        // report itself in force and pass everything — the one failure a
        // multi-tenant platform must not have. Same for a ceiling: an unenforced
        // rate limit is how one guest takes a node's network with it.
        //
        // **The pattern has no `..`, deliberately.** Every field of a port must
        // be accounted for here, and adding one is a compile error until
        // somebody says whether this datapath can enforce it. Without that, a
        // new enforceable field is silently *accepted* by the datapath that
        // enforces the least — which reads as working and is the worst possible
        // reading. That is exactly how `rate_limit_mbit` reached the fabric
        // datapath and was dropped, one layer over.
        let PortSpec {
            // What the guest is on and where its address came from: this
            // datapath gives it a wire, and both are already decided above it.
            network: _,
            subnet: _,
            // Which node holds it — that is why *this* agent is the one here.
            node: _,
            // Given to the guest by the VMM and the DHCP responder, not by any
            // filtering this datapath would have to do.
            address: _,
            mac: _,
            // Arrive already resolved as `rules`; the names alone say nothing
            // about whether anything is being asked for.
            security_groups: _,
            rate_limit_mbit,
        } = spec;

        let mut unenforceable = Vec::new();
        if !rules.is_empty() {
            unenforceable.push(format!("{} security-group rule(s)", rules.len()));
        }
        if let Some(mbit) = rate_limit_mbit {
            unenforceable.push(format!("a {mbit} Mbit ceiling"));
        }
        if !unenforceable.is_empty() {
            return Err(HostError::failed(format!(
                "{port} asks for {} and this datapath enforces neither; it gives a guest a wire \
                 and nothing else. Point the node at the fabric to have them applied, or take \
                 them off the port.",
                unenforceable.join(" and ")
            )));
        }
        let tap = self.tap_for(port);
        if !self.present(&tap).await {
            let owner = self.owner.map(|uid| uid.to_string());
            let mut args = vec!["tuntap", "add", "dev", &tap, "mode", "tap"];
            if let Some(uid) = &owner {
                // So the VMM can open it without being root, which is the whole
                // point of running guests as ordinary units.
                args.push("user");
                args.push(uid);
            }
            self.ip(&args).await?;
        }
        // The label before the link goes up, so a tap is never carrying traffic
        // this node cannot name. Both are set on every pass and not only on
        // create: the agent asks for a desired state rather than a delta, and a
        // tap that exists with its link down is exactly what a host reboot
        // leaves behind.
        self.ip(&["link", "set", "dev", &tap, "alias", port])
            .await?;
        self.ip(&["link", "set", "dev", &tap, "up"]).await?;

        // A network the operator put on a host bridge is the machine's own wire.
        // The guest goes on it and this platform does nothing else for it: no
        // gateway of ours, no translation, no address — whatever serves that
        // wire serves the guest, which is the point of asking for it.
        if !network.host_bridge.is_empty() {
            if !self.present(&network.host_bridge).await {
                // Refused, not created. Making one would mean deciding what goes
                // in it, and the only useful answer involves the machine's
                // uplink — which is how a node takes itself off the network. An
                // operator who wants this makes the bridge; the platform uses it.
                return Err(HostError::failed(format!(
                    "{port} is on a network that asks for host bridge `{}`, and this machine has \
                     no such interface. Create the bridge on the node — with whatever uplink it \
                     should carry — or take `hostBridge` off the network. This platform will not \
                     make one, because deciding what goes in it means deciding whether this node \
                     keeps its own network.",
                    network.host_bridge
                )));
            }
            self.ip(&["link", "set", "dev", &tap, "master", &network.host_bridge])
                .await?;
        }
        Ok(tap)
    }

    async fn unprogram(&self, port: &str) -> Result<()> {
        let tap = self.tap_for(port);
        if !self.present(&tap).await {
            return Ok(());
        }
        self.ip(&["tuntap", "del", "dev", &tap, "mode", "tap"])
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_name_fits_an_interface_name() {
        let dp = TapDatapath::new("vt", None);
        for port in [
            "projects/p1/ports/web",
            "projects/p1/ports/a-very-long-port-name-indeed",
            "projects/some-long-project/ports/x",
        ] {
            let tap = dp.tap_for(port);
            assert!(
                tap.len() <= IFNAMSIZ,
                "{tap} is {} characters and the kernel takes {IFNAMSIZ}",
                tap.len()
            );
        }
    }

    #[test]
    fn the_same_port_is_always_the_same_wire() {
        // Derived, not allocated: nothing is written down, so a restarted agent
        // finds the tap it made last time instead of making a second one.
        let dp = TapDatapath::new("vt", None);
        assert_eq!(
            dp.tap_for("projects/p1/ports/web"),
            dp.tap_for("projects/p1/ports/web")
        );
    }

    #[test]
    fn two_ports_that_differ_late_do_not_share_a_wire() {
        // The name is truncated to fit fifteen characters, so the part that
        // differs can fall off the end. The digest is what stops that being one
        // wire shared between two tenants.
        let dp = TapDatapath::new("vt", None);
        let a = dp.tap_for("projects/p1/ports/aaaaaaaaaaaaaaaaaaaaaaaa-one");
        let b = dp.tap_for("projects/p1/ports/aaaaaaaaaaaaaaaaaaaaaaaa-two");
        assert_ne!(a, b, "two ports were given one tap");
    }

    #[test]
    fn a_tap_name_is_something_the_kernel_accepts() {
        // No slashes, no dots: a device name is not a resource name.
        let dp = TapDatapath::new("vt", None);
        let tap = dp.tap_for("projects/p1/ports/web.0");
        assert!(
            tap.chars().all(|c| c.is_ascii_alphanumeric()),
            "{tap} carries something the kernel will refuse"
        );
    }

    #[test]
    fn a_link_line_yields_the_tap_and_the_port_it_carries() {
        let (tap, alias) = parse_link(
            "3: vtweb1a2b: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc fq_codel state DOWN mode \
             DEFAULT group default qlen 1000\\    link/ether e6:67:7e:03:31:5c brd \
             ff:ff:ff:ff:ff:ff\\    alias projects/p1/ports/web",
        )
        .expect("a line ip prints was not understood");
        assert_eq!(tap, "vtweb1a2b");
        assert_eq!(alias.as_deref(), Some("projects/p1/ports/web"));
    }

    #[test]
    fn a_link_with_no_label_yields_no_port() {
        let (tap, alias) = parse_link(
            "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP mode \
             DEFAULT group default qlen 1000\\    link/ether 00:11:22:33:44:55 brd \
             ff:ff:ff:ff:ff:ff",
        )
        .unwrap();
        assert_eq!(tap, "eth0");
        assert_eq!(alias, None);
    }

    #[test]
    fn one_end_of_a_veth_is_named_by_its_own_name() {
        // `veth0@if12` is how `ip` prints a peer, and the part after the `@` is
        // not part of the interface's name. Left in, it would fail the prefix
        // test and a labelled tap would be missed.
        let (tap, _) = parse_link(
            "7: vtpeer99@if8: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc noqueue state UP mode \
             DEFAULT group default\\    link/ether aa:bb:cc:dd:ee:ff brd ff:ff:ff:ff:ff:ff",
        )
        .unwrap();
        assert_eq!(tap, "vtpeer99");
    }

    #[tokio::test]
    async fn not_knowing_is_not_reported_as_an_empty_machine() {
        // If this returned Ok(empty), the agent would conclude every port it
        // believes in had gone and tear all of them down.
        let mut dp = TapDatapath::new("vt", None);
        dp.ip = "/nonexistent/bin/ip".to_string();
        assert!(dp.observe().await.is_err());
    }

    #[tokio::test]
    async fn a_port_with_rules_is_refused_rather_than_left_unfiltered() {
        use velstra_cloud_model::security::{Direction, PortRange, Protocol};
        let dp = TapDatapath::new("vt", None);
        let rule = ResolvedRule {
            direction: Direction::Ingress,
            protocol: Protocol::Tcp,
            ports: Some(PortRange { from: 443, to: 443 }),
            remote: "0.0.0.0/0".into(),
        };
        let err = dp
            .program(
                "projects/p1/ports/web",
                &PortSpec::default(),
                &NetworkSpec::default(),
                &[rule],
            )
            .await
            .expect_err("a datapath that filters nothing accepted a filtered port");
        assert!(err.to_string().contains("security-group rule"), "{err}");
    }

    #[tokio::test]
    async fn a_port_with_a_ceiling_is_refused_too() {
        // An unenforced rate limit is how one guest takes a node's network with
        // it, and the port would have said the limit was in force.
        let dp = TapDatapath::new("vt", None);
        let spec = PortSpec {
            rate_limit_mbit: Some(100),
            ..PortSpec::default()
        };
        let err = dp
            .program(
                "projects/p1/ports/loud",
                &spec,
                &NetworkSpec::default(),
                &[],
            )
            .await
            .expect_err("a datapath with no shaper accepted a shaped port");
        assert!(err.to_string().contains("100 Mbit"), "{err}");
    }

    #[tokio::test]
    async fn the_refusal_happens_before_the_machine_is_touched() {
        // Order matters: a refused port must leave no tap behind, or the next
        // pass would adopt one carrying a port nobody agreed to program. Proved
        // by making the kernel unreachable — if anything had been attempted, the
        // error would name `ip` instead.
        let mut dp = TapDatapath::new("vt", None);
        dp.ip = "/nonexistent/bin/ip".to_string();
        let spec = PortSpec {
            rate_limit_mbit: Some(10),
            ..PortSpec::default()
        };
        let err = dp
            .program("projects/p1/ports/x", &spec, &NetworkSpec::default(), &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Mbit ceiling"), "{err}");
    }
}
