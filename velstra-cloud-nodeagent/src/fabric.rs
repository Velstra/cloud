//! Programming a port on the Velstra fabric.
//!
//! [`crate::datapath::TapDatapath`] gives a guest a wire and enforces nothing —
//! it refuses a port that carries security groups rather than reporting rules it
//! does not apply. This is the other half: the same tap, plus a port on the
//! fabric's overlay with the tenant's rules actually programmed into it.
//!
//! ## What it does, in the order it has to happen
//!
//! 1. **The tap**, by delegating to [`TapDatapath`]. The fabric does not create
//!    it: `CreatePortRequest.tap` names a device that must already be on the
//!    host, and both VMM backends name the same device on their command line.
//! 2. **The rules**, as a security group named after the port. One group per
//!    port rather than one per *cloud* group, because a cloud port's allowances
//!    are the union of its groups resolved against who currently holds which
//!    address — that union belongs to the port and to nothing else.
//! 3. **The port**, with the VNI of its network, its address and the MAC the
//!    platform allocated, bound to that group.
//!
//! ## Restating, not creating
//!
//! Every pass says the whole thing again. That is not waste: a port's allowances
//! change whenever a member of a group it names gains or loses an address, and
//! nothing tells this node which of those happened. Fabric's `AddSecurityGroup`
//! replaces a group of the same name, and its policy id comes from the name, so
//! a restatement lands under the id the data plane is already using — ports stay
//! bound and there is no window. (It used to refuse a duplicate name, which made
//! "these are the rules now" inexpressible; that is why it does not any more.)
//!
//! ## What it refuses
//!
//! Fabric's rule key is `(policy, protocol, destination port, source)`, with a
//! single port and a single protocol. Two shapes a cloud rule can take have no
//! encoding there:
//!
//! * **any port** — `tcp` with no port range, which is what "allow all TCP from
//!   these members" compiles to; and
//! * **any protocol** — [`Protocol::Any`], which is what the default
//!   "everything, within this group" rule compiles to.
//!
//! Both are **refused with a sentence naming the rule**, and the port is not
//! programmed. The alternative was to drop the rule and program the rest, which
//! is a port reporting its groups in force while the widest of them silently
//! does nothing — the exact failure this whole file exists to end. A port range
//! is expanded when it is small enough to expand and refused when it is not,
//! because sixty-five thousand rules is not an encoding either.

use std::collections::BTreeMap;

use async_trait::async_trait;
/// The fabric's contract, from the one crate that owns it.
///
/// Re-exported rather than generated here: a controller mirrors networks to the
/// same fabric, and two vendored copies of one schema drift in ways that show
/// up as a field quietly meaning something different on one side.
pub use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    resources::{NetworkSpec, PortSpec},
    security::{Direction, Protocol, ResolvedRule},
};

use crate::{
    datapath::TapDatapath,
    host::{Datapath, HostError, ProgrammedPort, Result},
};

/// The widest port range this will expand into individual rules.
///
/// A range is expanded because fabric's rule key holds one port. Sixty-four is
/// where "expand it" stops being an encoding and starts being a denial of
/// service against the control plane — and a range wider than that is almost
/// always somebody meaning "any", which has its own answer above.
const MAX_EXPANDED_PORTS: u32 = 64;

/// Where this machine's overlay traffic enters and leaves it.
///
/// Stated rather than guessed at, for the same reason `--migration-address` is:
/// nothing on a machine can tell which of its addresses its peers route to, and
/// a control plane that picked one would be picking wrong on every host with
/// more than one interface.
#[derive(Clone, Debug)]
pub struct Underlay {
    /// The VTEP address other hosts send this one's encapsulated frames to.
    pub vtep: String,
    /// The interface that address is on.
    pub iface: String,
    /// Its hardware address, read from the machine rather than configured — it
    /// is a fact about the interface, and asking an operator to repeat it is
    /// asking them to get it wrong.
    pub mac: String,
    /// Its MTU, read the same way and for the same reason.
    ///
    /// It matters more than it looks. The fabric derives the overlay MTU and
    /// the MSS clamp from this, and it reads an absent one as 1500 — so a node
    /// on a 1450-byte underlay (which is every guest-in-a-guest cloud) was
    /// clamping to a size its own path cannot carry. The symptom is not a
    /// broken network: it is large transfers that hang on some paths, because
    /// PMTUD's ICMP is filtered almost everywhere, which is the whole reason
    /// the clamp exists.
    pub mtu: u32,
    /// This host's SRv6 locator as `prefix/len`, or `None` to stay on VXLAN.
    ///
    /// Stated, never read: unlike the MAC and the MTU, a locator is not a fact
    /// about the interface. It is a slice of the operator's IPv6 address plan
    /// that has to be routable in the underlay and unique per host, and a node
    /// that invented one would build an overlay nothing routes to.
    ///
    /// Its presence is also what selects the wire family — there is no separate
    /// encapsulation flag, because the two could then disagree, and a host whose
    /// declared format and locator disagree is refused by the fabric anyway.
    pub srv6_locator: Option<String>,
}

impl Underlay {
    /// Put this host on the SRv6 wire family, or leave it on VXLAN for `None`.
    pub fn with_srv6_locator(mut self, locator: Option<String>) -> Self {
        self.srv6_locator = locator;
        self
    }

    /// Read the interface's MAC and MTU, so only the address and the name are
    /// stated.
    pub fn read(vtep: &str, iface: &str) -> Result<Self> {
        let mac = Self::sysfs(iface, "address")?;
        // Refused rather than guessed. Falling back to 1500 here would be this
        // agent *asserting* an MTU it did not read, and the assertion travels:
        // the fabric clamps to it and the guests live with the result.
        let mtu = Self::sysfs(iface, "mtu")?.parse::<u32>().map_err(|e| {
            HostError::failed(format!("{iface} reports an MTU that is not a number: {e}"))
        })?;
        Ok(Self {
            vtep: vtep.to_string(),
            iface: iface.to_string(),
            mac,
            mtu,
            // Not readable from the machine — see the field. `with_srv6_locator`
            // is how a caller states one.
            srv6_locator: None,
        })
    }

    fn sysfs(iface: &str, what: &str) -> Result<String> {
        let path = format!("/sys/class/net/{iface}/{what}");
        Ok(std::fs::read_to_string(&path)
            .map_err(|e| {
                HostError::failed(format!(
                    "{iface} is meant to carry this host's overlay traffic and {path} cannot be \
                     read: {e}"
                ))
            })?
            .trim()
            .to_string())
    }
}

pub struct FabricDatapath {
    /// The tap half, unchanged. A port needs a device before it can be a port.
    taps: TapDatapath,
    /// `http://host:port` of the fabric orchestrator.
    endpoint: String,
    /// What this node calls itself to fabric — its host id, which is the same
    /// name the cloud knows it by so that one machine is one identity.
    host: String,
    underlay: Underlay,
    /// Whether this host has been declared to the fabric in this process's life.
    ///
    /// Not a cache of what the fabric knows — this crate does not remember what
    /// it did — but a note that the declaration has been *made*. A host is a
    /// fact about this machine and nobody else can state it; restating it on
    /// every port would be a round trip per port to say something that has not
    /// changed since the process started.
    declared: std::sync::atomic::AtomicBool,
    /// What the fabric said its groups held, as of the last [`Datapath::observe`].
    ///
    /// Not a record of what this process programmed — that would be the thing
    /// this crate refuses to keep, and it would go on claiming a port was
    /// current after somebody changed it out from under the platform. It is the
    /// *fabric's* answer, thrown away and asked for again on every pass, and it
    /// lives here only because [`Datapath::agrees`] is asked the question
    /// outside the call that can go and ask.
    fabric_rules: std::sync::Mutex<BTreeMap<String, Vec<pb::PortRule>>>,
}

impl FabricDatapath {
    pub fn new(taps: TapDatapath, endpoint: &str, host: &str, underlay: Underlay) -> Self {
        Self {
            taps,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            host: host.to_string(),
            underlay,
            declared: std::sync::atomic::AtomicBool::new(false),
            fabric_rules: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Tell the fabric this machine exists, once per process.
    ///
    /// The node states it because the node is the only thing that knows it: its
    /// id, the address its peers reach it at, and the interface that address is
    /// on. A controller doing it would be guessing at all three.
    async fn declare_host(
        &self,
        client: &mut pb::velstra_orchestrator_client::VelstraOrchestratorClient<
            tonic::transport::Channel,
        >,
    ) -> Result<()> {
        use std::sync::atomic::Ordering;
        if self.declared.load(Ordering::SeqCst) {
            return Ok(());
        }
        client
            .add_host(pb::HostSpec {
                id: self.host.clone(),
                vtep: self.underlay.vtep.clone(),
                underlay_iface: self.underlay.iface.clone(),
                underlay_mac: self.underlay.mac.clone(),
                // The wire family follows the locator rather than being its own
                // flag: two fields can disagree, and the fabric refuses that
                // disagreement, so a node with a locator would have to remember to
                // set both or be rejected for a reason it did not cause. With no
                // locator this stays at the fabric's default (VXLAN), which is
                // what every existing deployment already runs.
                encap: match self.underlay.srv6_locator {
                    Some(_) => pb::Encap::Srv6 as i32,
                    None => pb::Encap::Vxlan as i32,
                },
                srv6_locator: self.underlay.srv6_locator.clone().unwrap_or_default(),
                // The fabric's own default for the port: this agent has no opinion
                // about it, and inventing one would be a per-host disagreement
                // nobody asked for.
                udp_port: 0,
                // Read from the interface, not left at the wire's 0 — which
                // fabric reads as 1500 whatever the interface actually is.
                underlay_mtu: self.underlay.mtu,
            })
            .await
            .map_err(|e| HostError::failed(format!("declaring this host to the fabric: {e}")))?;
        self.declared.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn client(
        &self,
    ) -> Result<pb::velstra_orchestrator_client::VelstraOrchestratorClient<tonic::transport::Channel>>
    {
        velstra_cloud_fabric::connect(&self.endpoint)
            .await
            .map_err(|e| {
                HostError::failed(format!("reaching the fabric at {}: {e}", self.endpoint))
            })
    }

    /// The fabric security group carrying one cloud port's allowances.
    ///
    /// Named after the port, because that is what the union of its groups
    /// belongs to. Fabric derives the policy id from this name, so the name must
    /// be stable for the life of the port — which a resource name is.
    fn group_for(port: &str) -> String {
        format!("cloud:{port}")
    }

    /// What the fabric currently holds, by group name.
    ///
    /// One call for the whole node rather than one per port: the answer is a
    /// property of the fabric, and asking it once per carried port would be the
    /// same answer fetched many times.
    async fn groups_in_fabric(&self) -> Result<BTreeMap<String, Vec<pb::PortRule>>> {
        let mut client = self.client().await?;
        let listed = client
            .list_security_groups(pb::ListSecurityGroupsRequest {})
            .await
            .map_err(|e| HostError::failed(format!("listing the fabric's groups: {e}")))?
            .into_inner();
        Ok(listed
            .groups
            .into_iter()
            .map(|g| (g.name, g.rules))
            .collect())
    }
}

/// One cloud allowance, as rules fabric can key.
///
/// Returns the rules or a sentence saying why this one cannot be expressed. The
/// sentence goes on the Port, where somebody is already looking.
pub fn translate(rule: &ResolvedRule) -> std::result::Result<Vec<pb::PortRule>, String> {
    let proto = match rule.protocol {
        Protocol::Tcp => pb::Proto::Tcp,
        Protocol::Udp => pb::Proto::Udp,
        Protocol::Icmp => pb::Proto::Icmp,
        // The default intra-group rule compiles to this, so it is the one most
        // worth naming precisely rather than approximating.
        Protocol::Any => {
            return Err(
                "names every protocol, and the fabric keys a rule on one. Split it into tcp, \
                 udp and icmp, or say which protocol you meant"
                    .into(),
            );
        }
    };

    // Two things follow from the direction, and they are not the same thing.
    //
    // Which *end* the address constrains: an ingress rule says where a packet
    // came from, an egress rule where it is going. The fabric ranks one address
    // dimension per rule and refuses one carrying both, which is exactly the
    // shape a cloud rule has.
    //
    // And which *hook* the rule applies at. Without that, an ingress allowance
    // would also admit the same traffic outbound — a rule matching more than it
    // says, which is the one direction a firewall must never be wrong in.
    let (src, dst, direction) = match rule.direction {
        Direction::Ingress => (rule.remote.clone(), String::new(), "in"),
        Direction::Egress => (String::new(), rule.remote.clone(), "out"),
    };

    let ports: Vec<u32> = match rule.ports {
        None if rule.protocol.has_ports() => {
            return Err(format!(
                "allows every {:?} port, and the fabric keys a rule on one. Name a port or a \
                 range of at most {MAX_EXPANDED_PORTS}",
                rule.protocol
            ));
        }
        // A protocol with no ports is keyed at port 0, which is what the fabric
        // already does for ICMP.
        None => vec![0],
        Some(range) => {
            let width = u32::from(range.to) - u32::from(range.from) + 1;
            if width > MAX_EXPANDED_PORTS {
                return Err(format!(
                    "spans {width} ports ({}-{}), and the fabric keys a rule on one. At most \
                     {MAX_EXPANDED_PORTS} are expanded into rules",
                    range.from, range.to
                ));
            }
            (u32::from(range.from)..=u32::from(range.to)).collect()
        }
    };

    Ok(ports
        .into_iter()
        .map(|port| pb::PortRule {
            proto: proto as i32,
            port,
            action: pb::Action::Pass as i32,
            log: false,
            src: src.clone(),
            dst: dst.clone(),
            limit: 0,
            burst: 0,
            icmp_type: 0,
            // Both families: a cloud rule's remote is a prefix, and which family
            // it is is already in the prefix. Constraining it here as well would
            // be saying the same thing twice and getting it wrong once.
            family: String::new(),
            direction: direction.to_string(),
            // Every interface the policy covers, which for a per-port group is
            // that port's own tap.
            in_interface: String::new(),
            // A cloud security-group rule has no sender-hardware-address form.
            // It says "this remote prefix may reach this port", and a MAC is not
            // routable, so a rule carrying one could only ever match traffic
            // from the same segment — which the port binding already governs by
            // identity. Left empty rather than filled with the port's own MAC:
            // that would read as a constraint and enforce a tautology.
            src_mac: String::new(),
        })
        .collect())
}

/// Everything a port's allowances come to, or the first one that cannot be said.
/// Whether two sets of fabric rules are the same set.
///
/// Sorted copies rather than a `HashSet`, because `pb::PortRule` is generated
/// and carries no `Hash`; and as a *set* rather than a sequence because nothing
/// promises an order at either end — `translate` emits one rule per port of a
/// range, and the fabric reports what it has in whatever order it has it.
fn same_rules(a: &[pb::PortRule], b: &[pb::PortRule]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // Destructured with **no** `..`, and that is the whole guard. Every field
    // has to appear or this does not compile — because a comparison that
    // skipped one would call two different rules the same, and a port would go
    // on carrying something nobody asked for while this said it agreed.
    //
    // The proto has already shipped that failure once: `src_mac` was added to
    // the message and not to the code that consumed it, and an operator
    // quarantining a device got a green apply and an empty rule table. Field
    // access spelled the same list out and would have compiled just as happily
    // without the new one.
    //
    // A string rather than a tuple because a tuple of this width has no `Ord`,
    // and the generated type has no `Hash`.
    let key = |r: &pb::PortRule| {
        let pb::PortRule {
            proto,
            port,
            action,
            log,
            src,
            dst,
            limit,
            burst,
            icmp_type,
            family,
            direction,
            in_interface,
            src_mac,
        } = r;
        format!(
            "{proto}|{port}|{action}|{log}|{src}|{dst}|{limit}|{burst}|{icmp_type}|{family}|\
             {direction}|{in_interface}|{src_mac}"
        )
    };
    let mut a: Vec<_> = a.iter().map(key).collect();
    let mut b: Vec<_> = b.iter().map(key).collect();
    a.sort();
    b.sort();
    a == b
}

fn translate_all(rules: &[ResolvedRule]) -> Result<Vec<pb::PortRule>> {
    let mut out = Vec::new();
    for rule in rules {
        match translate(rule) {
            Ok(mut some) => out.append(&mut some),
            // Refused whole rather than dropping the rule and programming the
            // rest: a port reporting its groups in force while the widest of
            // them silently does nothing is the failure this exists to end.
            Err(why) => {
                return Err(HostError::failed(format!(
                    "a rule on this port {why} — the port is not programmed rather than \
                     programmed without it"
                )));
            }
        }
    }
    Ok(out)
}

#[async_trait]
impl Datapath for FabricDatapath {
    async fn observe(&self) -> Result<BTreeMap<String, ProgrammedPort>> {
        // From the taps, not from the fabric. The question this answers is what
        // *this machine* is carrying, and the tap is the machine's own record of
        // it — the same reason nothing else in this crate remembers what it did.
        // A port the fabric knows about and this host has no tap for is not a
        // port this host is carrying.
        let carried = self.taps.observe().await?;

        // The rules are a different question with a different answer-holder.
        // The tap layer does not know them — it is deliberately programmed with
        // none, because this datapath is what enforces them — so `rules` on
        // what it returns is empty, and comparing that against what a port
        // wants would say "out of date" for ever. What the fabric holds is the
        // real answer, and it is fetched fresh here rather than remembered.
        //
        // A fabric that cannot be reached leaves the previous answer in place
        // rather than emptying it: "I could not ask" is not "there are no
        // rules", and treating it as such would reprogram every port on this
        // node the moment the orchestrator restarted.
        match self.groups_in_fabric().await {
            Ok(groups) => *self.fabric_rules.lock().unwrap() = groups,
            Err(e) => tracing::debug!(error = %e, "could not ask the fabric what it holds"),
        }
        Ok(carried)
    }

    /// Whether the fabric holds, for this port's group, exactly the rules this
    /// pass wants — compared in the fabric's vocabulary, which is the only one
    /// both sides can be spelled in.
    ///
    /// As a *set*: `translate` expands a port range into one rule per port, and
    /// the fabric is free to report them in any order. Comparing sequences
    /// would make the answer depend on ordering neither end promises.
    fn agrees(&self, port: &str, have: &ProgrammedPort, want: &[ResolvedRule]) -> bool {
        let _ = have;
        let Ok(wanted) = translate_all(want) else {
            // A rule that cannot be expressed is one `program` will refuse.
            // Saying "agreed" here would leave the port quietly carrying
            // whatever it had; saying "not agreed" sends it back through
            // `program`, which refuses with the sentence that names the rule.
            return false;
        };
        let held = self.fabric_rules.lock().unwrap();
        let Some(there) = held.get(&Self::group_for(port)) else {
            // Nothing under this port's name. Either the fabric has never been
            // asked, or it does not have the group — and wanting nothing is
            // then genuinely satisfied, while wanting something is not.
            return wanted.is_empty();
        };
        same_rules(there, &wanted)
    }

    async fn program(
        &self,
        port: &str,
        spec: &PortSpec,
        network: &NetworkSpec,
        rules: &[ResolvedRule],
    ) -> Result<String> {
        // The refusal first, before anything is created: a rule that cannot be
        // expressed must not leave a half-programmed port behind.
        let fabric_rules = translate_all(rules)?;

        // The tap, with no rules — this datapath, not the tap one, is what
        // enforces them, so the tap half must not refuse the port for carrying
        // some.
        let tap = self
            .taps
            .program(port, &PortSpec::default(), network, &[])
            .await?;

        let mut client = self.client().await?;
        self.declare_host(&mut client).await?;
        let group = Self::group_for(port);

        // Restated on every pass. Fabric replaces a group of the same name and
        // derives its policy id from that name, so this lands under the id the
        // data plane is already using: no unbind, no window.
        client
            .add_security_group(pb::SecurityGroupSpec {
                name: group.clone(),
                // The platform's own default: nothing is allowed that was not
                // asked for. An empty rule set is a closed port, not an open one.
                default_action: pb::Action::Drop as i32,
                drop_icmp: false,
                stateful: true,
                blocklist: Vec::new(),
                rules: fabric_rules,
            })
            .await
            .map_err(|e| HostError::failed(format!("declaring {group} on the fabric: {e}")))?;

        let info = client
            .create_port(pb::CreatePortRequest {
                network: network.vni,
                host: self.host.clone(),
                tap: tap.clone(),
                ip: spec.address.clone().unwrap_or_default(),
                policy: None,
                // The platform's MAC, not one the fabric derives. Two allocators
                // for one identity is every frame dropped and a network that
                // looks like it simply does not work.
                mac: spec.mac.clone(),
            })
            .await
            .map_err(|e| HostError::failed(format!("creating {port} on the fabric: {e}")))?
            .into_inner();

        client
            .bind_port_security_group(pb::BindPortSecurityGroupRequest {
                port_id: info.id.clone(),
                group: Some(group),
            })
            .await
            .map_err(|e| HostError::failed(format!("binding the rules for {port}: {e}")))?;

        // The send ceiling, restated on every pass like everything else here.
        //
        // It was carried, stored and echoed all the way down to this function
        // and then **dropped**: the tap datapath refuses a ceiling it cannot
        // enforce and says so, and this one — the datapath that can — silently
        // ignored it. One guest could take a node's network with it and the
        // port would report itself in force. Sent unconditionally rather than
        // only when set, so *removing* a ceiling is a decision that arrives too.
        client
            .limit_port(pb::LimitPortRequest {
                id: info.id.clone(),
                rate_limit_mbit: spec.rate_limit_mbit,
            })
            .await
            .map_err(|e| HostError::failed(format!("setting the ceiling for {port}: {e}")))?;

        Ok(tap)
    }

    /// Undo all three of the things [`Self::program`] made, in the reverse
    /// order it made them.
    ///
    /// It used to undo one and a half. The tap went, `RemoveSecurityGroup` was
    /// called and its answer discarded with `let _`, and nothing ever removed
    /// the **port** — so every deleted cloud port left a fabric port behind
    /// holding its address and its MAC, on a host that no longer had the device.
    /// Worse, the one call that was made could not have worked: the fabric
    /// refuses to remove a group while a port is still bound to it, which the
    /// port that was never removed always was. So the group leaked too, and the
    /// discarded `Result` is why nobody ever saw the refusal.
    ///
    /// It survived because the only test of the teardown ended with
    /// `let _ = datapath.unprogram(PORT).await` and asked the fabric nothing
    /// afterwards — an assertion that would have held with this function empty.
    async fn unprogram(&self, port: &str) -> Result<()> {
        // The tap goes whatever the fabric says: a device this host is done with
        // is this host's to remove, and leaving it would have the next pass
        // adopt a port nobody asked for.
        self.taps.unprogram(port).await?;
        let mut client = self.client().await?;

        // The fabric's port id is not something this crate remembers — nothing
        // here remembers anything, which is the recovery model — so it is asked
        // for. The tap name is the key because it is *derived* from the port
        // rather than allocated, so the same lookup works in a process that has
        // never seen this port before.
        //
        // Matched on the host as well as the tap, and that is not belt and
        // braces: a tap name is deliberately identical on every node, because a
        // migrating guest's VMM command line names it. Matching on the name
        // alone would have a source node remove the fabric port a destination
        // has just taken over, and the guest would lose its network on arrival.
        let tap = self.taps.tap_for(port);
        let ports = client
            .list_ports(pb::ListPortsRequest {})
            .await
            .map_err(|e| HostError::failed(format!("asking the fabric for {port}: {e}")))?
            .into_inner()
            .ports;
        if let Some(mine) = ports.iter().find(|p| p.tap == tap && p.host == self.host) {
            client
                .remove_port(pb::RemovePortRequest {
                    id: mine.id.clone(),
                })
                .await
                .map_err(|e| HostError::failed(format!("removing {port} from the fabric: {e}")))?;
        }

        // After the port and never before: the fabric refuses to remove a group
        // something is still bound to. Removing an absent group is not an error
        // there, so a second pass over a port that is already gone is quiet.
        client
            .remove_security_group(pb::RemoveSecurityGroupRequest {
                name: Self::group_for(port),
            })
            .await
            .map_err(|e| HostError::failed(format!("removing the rules for {port}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::security::PortRange;

    use super::*;

    fn rule(protocol: Protocol, ports: Option<PortRange>, direction: Direction) -> ResolvedRule {
        ResolvedRule {
            direction,
            protocol,
            ports,
            remote: "10.0.0.0/24".into(),
        }
    }

    #[test]
    fn one_port_is_one_rule() {
        let out = translate(&rule(
            Protocol::Tcp,
            Some(PortRange { from: 443, to: 443 }),
            Direction::Ingress,
        ))
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].port, 443);
        assert_eq!(out[0].src, "10.0.0.0/24", "an ingress rule lost its source");
        assert!(out[0].dst.is_empty());
        assert_eq!(out[0].direction, "in");
        assert_eq!(out[0].action, pb::Action::Pass as i32);
    }

    #[test]
    fn an_egress_rule_constrains_the_far_end_and_the_other_hook() {
        // Both halves matter. Without the address moving to `dst`, the rule
        // would be about where the packet came *from*; without the direction, an
        // outbound allowance would also admit the same traffic inbound.
        let out = translate(&rule(
            Protocol::Udp,
            Some(PortRange { from: 53, to: 53 }),
            Direction::Egress,
        ))
        .unwrap();
        assert!(out[0].src.is_empty(), "an egress rule kept a source");
        assert_eq!(out[0].dst, "10.0.0.0/24");
        assert_eq!(out[0].direction, "out");
    }

    #[test]
    fn a_small_range_is_expanded_because_the_fabric_keys_one_port() {
        let out = translate(&rule(
            Protocol::Tcp,
            Some(PortRange {
                from: 8000,
                to: 8003,
            }),
            Direction::Ingress,
        ))
        .unwrap();
        assert_eq!(
            out.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![8000, 8001, 8002, 8003]
        );
    }

    #[test]
    fn a_protocol_with_no_ports_is_keyed_at_zero() {
        let out = translate(&rule(Protocol::Icmp, None, Direction::Ingress)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].port, 0, "icmp was given a port it does not have");
    }

    #[test]
    fn every_port_is_refused_rather_than_narrowed() {
        // "Allow all TCP from these members" is an ordinary security-group rule
        // and the fabric cannot key it. Programming the port without it would be
        // reporting a group in force while its widest rule does nothing.
        let why = translate(&rule(Protocol::Tcp, None, Direction::Ingress)).unwrap_err();
        assert!(why.contains("every"), "{why}");
        assert!(why.contains("port"), "{why}");
    }

    #[test]
    fn every_protocol_is_refused_rather_than_approximated() {
        // The default intra-group rule compiles to this. Expanding it to tcp,
        // udp and icmp would silently drop everything else the tenant asked to
        // allow — over-restriction, still wrong, and invisible.
        let why = translate(&rule(Protocol::Any, None, Direction::Ingress)).unwrap_err();
        assert!(why.contains("every protocol"), "{why}");
    }

    #[test]
    fn a_range_too_wide_to_expand_is_refused_with_its_width() {
        let why = translate(&rule(
            Protocol::Tcp,
            Some(PortRange { from: 1, to: 65535 }),
            Direction::Ingress,
        ))
        .unwrap_err();
        assert!(why.contains("65535 ports"), "{why}");
    }

    #[test]
    fn one_rule_that_cannot_be_said_refuses_the_whole_port() {
        // Not "programme what we can": a port carrying four rules of which three
        // are in force reports `programmed: true` and is open where its author
        // believes it is closed — or closed where they believe it is open.
        let refused = translate_all(&[
            rule(
                Protocol::Tcp,
                Some(PortRange { from: 22, to: 22 }),
                Direction::Ingress,
            ),
            rule(Protocol::Any, None, Direction::Ingress),
        ]);
        assert!(refused.is_err());
        let why = refused.unwrap_err().to_string();
        assert!(why.contains("not programmed"), "{why}");
    }

    #[test]
    fn a_port_with_no_rules_is_a_closed_port_and_not_an_error() {
        // The platform's own default is nothing allowed that was not asked for.
        assert_eq!(translate_all(&[]).unwrap().len(), 0);
    }

    #[test]
    fn a_ports_group_is_named_after_the_port_and_nothing_else() {
        // Fabric derives the policy id from this name, so it has to be stable
        // for the life of the port — which a resource name is and a tap is not.
        assert_eq!(
            FabricDatapath::group_for("projects/p1/ports/web"),
            "cloud:projects/p1/ports/web"
        );
    }

    // ---- same_rules ------------------------------------------------------
    //
    // Pure over two vectors, so it needs no fabric, no tap and no root — which
    // matters, because the only other exercise of it is an integration test
    // that *skips and reports ok* without `CAP_NET_ADMIN`. It went for a while
    // with no coverage that actually ran anywhere, and replacing its body with
    // `true` left every test in the crate green.

    fn pb_rule(port: u32) -> pb::PortRule {
        pb::PortRule {
            proto: pb::Proto::Tcp as i32,
            port,
            action: pb::Action::Pass as i32,
            log: false,
            src: "10.0.0.0/24".into(),
            dst: String::new(),
            limit: 0,
            burst: 0,
            icmp_type: 0,
            family: String::new(),
            direction: "in".into(),
            in_interface: String::new(),
            src_mac: String::new(),
        }
    }

    #[test]
    fn the_same_rules_in_a_different_order_are_the_same_rules() {
        let a = vec![pb_rule(80), pb_rule(443), pb_rule(8080)];
        let b = vec![pb_rule(8080), pb_rule(80), pb_rule(443)];
        assert!(
            same_rules(&a, &b),
            "the order is nobody's promise: `translate` emits one rule per port \
             of a range, and the fabric reports what it holds however it holds it"
        );
    }

    #[test]
    fn a_rule_that_is_missing_or_extra_is_a_difference() {
        let want = vec![pb_rule(80), pb_rule(443)];
        assert!(!same_rules(&want, &[pb_rule(80)]), "one fewer allowance");
        assert!(
            !same_rules(&want, &[pb_rule(80), pb_rule(443), pb_rule(22)]),
            "an allowance nobody asked for — the direction that matters"
        );
        assert!(!same_rules(&want, &[]));
        assert!(same_rules(&[], &[]));
    }

    /// A difference in **any** field is a difference.
    ///
    /// Not a style point. Two rules that differ only in `src_mac`, or only in
    /// `direction`, are two different rules; a comparison that ignored one
    /// would declare a port current while it carried something else, and
    /// nothing would ever put it right. That exact omission has shipped once
    /// already — `src_mac` reached the wire and not the code that read it, and
    /// an operator quarantining a device got a green apply and an empty table.
    #[test]
    fn every_field_of_a_rule_is_compared() {
        let base = pb_rule(80);
        let mut differing = vec![];
        for change in [
            |r: &mut pb::PortRule| r.proto = pb::Proto::Udp as i32,
            |r: &mut pb::PortRule| r.port = 81,
            |r: &mut pb::PortRule| r.action = pb::Action::Drop as i32,
            |r: &mut pb::PortRule| r.log = true,
            |r: &mut pb::PortRule| r.src = "10.0.1.0/24".into(),
            |r: &mut pb::PortRule| r.dst = "10.0.2.0/24".into(),
            |r: &mut pb::PortRule| r.limit = 100,
            |r: &mut pb::PortRule| r.burst = 10,
            |r: &mut pb::PortRule| r.icmp_type = 8,
            |r: &mut pb::PortRule| r.family = "ipv6".into(),
            |r: &mut pb::PortRule| r.direction = "out".into(),
            |r: &mut pb::PortRule| r.in_interface = "eth0".into(),
            |r: &mut pb::PortRule| r.src_mac = "02:00:00:00:00:01".into(),
        ] {
            let mut other = base.clone();
            change(&mut other);
            assert!(
                !same_rules(std::slice::from_ref(&base), std::slice::from_ref(&other)),
                "two rules differing in one field compared equal: {other:?}"
            );
            differing.push(other);
        }
        // One case per field of `pb::PortRule`. The destructuring inside
        // `same_rules` is what makes a new field a compile error there; this is
        // what makes it a *test* failure here, so the number is asserted rather
        // than trusted.
        assert_eq!(differing.len(), 13, "a field of PortRule has no case here");
    }
}
