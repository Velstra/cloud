//! Security groups: which traffic a port is allowed to carry.
//!
//! A port already names its groups — `PortSpec::security_groups` has been in
//! the model, the proto and the API from the start. What was missing was the
//! groups themselves, which made that field the worst thing a security control
//! can be: a tenant writes `security_groups: ["web"]`, the platform accepts it,
//! stores it, hands it back on read, and enforces nothing. This module is the
//! other half.
//!
//! ## The stance
//!
//! **Ingress is denied, egress is allowed, replies are always allowed.** Rules
//! only ever *add* an allowance; there is no deny rule and no ordering, so two
//! groups on one port can never contradict each other and "which rule won" is
//! not a question anybody has to ask. Egress defaults open because a guest that
//! cannot reach anything cannot finish its own boot, and a platform whose
//! default leaves every new instance broken teaches people to attach a
//! permit-everything group and stop thinking about it.
//!
//! ## Resolution is computed, never stored
//!
//! A rule may name a **remote group** instead of a CIDR — "anything in `web`
//! may reach me" — which is the form worth having, because it keeps working as
//! guests come and go. What that expands to is a function of the ports that
//! exist right now, so it is computed on every pass from the objects themselves
//! ([`effective_rules`]) and never written down. A stored expansion would be a
//! second record of who is in a group, and the moment it disagreed with the
//! ports, traffic would be allowed on the strength of a member that had gone.
//!
//! Membership does **not** chain: a rule naming group `web` expands to the
//! addresses of ports in `web`, not to whatever `web`'s own rules allow. One
//! level, always, so an allowance can be read off one group and its members
//! without following a graph.
//!
//! ## How a group is named
//!
//! By its full resource name — `projects/p1/security-groups/web` — both where a
//! port lists its groups and where a rule names a remote one, the same as every
//! other reference in the model (`PortSpec::network` is a resource name, not an
//! id). This module never parses them: it compares them, so a reference is
//! either exactly the group or it is not one, and there is no scope in which
//! `web` might mean a different group than the one the writer meant.
//!
//! ## A group that does not exist
//!
//! Naming a group nobody created is reported on the port and is otherwise
//! harmless *by construction*: rules only add allowances, so a missing group is
//! strictly fewer allowances — the safe direction. That is why the port is
//! still programmed with what it does have rather than left unprogrammed. It is
//! reported anyway, loudly, because "your firewall rule silently did nothing"
//! is exactly the failure this module exists to stop.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    meta::{Condition, ConditionStatus},
    resources::{Assigned, Observed, PortSpec, Resource},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Ingress,
    Egress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Every protocol, including the ones with no ports.
    Any,
    Tcp,
    Udp,
    Icmp,
}

impl Protocol {
    /// Whether a port range means anything for this protocol.
    pub fn has_ports(self) -> bool {
        matches!(self, Protocol::Tcp | Protocol::Udp)
    }
}

/// An inclusive port range. `from == to` is a single port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortRange {
    pub from: u16,
    pub to: u16,
}

/// The other end of an allowance.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Remote {
    /// A literal prefix. `0.0.0.0/0` is how a rule says "from anywhere", and it
    /// has to be written out rather than implied by omission — a rule whose
    /// scope depends on a field being absent is one nobody reads correctly.
    Cidr(String),
    /// The addresses of the ports in another group, resolved on every pass.
    Group(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityRule {
    pub direction: Direction,
    pub protocol: Protocol,
    /// Only meaningful for TCP and UDP; [`validate`] refuses it elsewhere
    /// rather than ignoring it, because a range that is quietly dropped reads
    /// like a narrower rule than the one in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<PortRange>,
    pub remote: Remote,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SecurityGroupSpec {
    pub rules: Vec<SecurityRule>,
}

/// Nothing here is written by anybody.
///
/// A group is a declaration; whether it is in force is a property of the ports
/// that reference it, and those already report whether they are programmed and
/// at which generation. So the condition is computed on read by the API — see
/// [`group_condition`] — the same way `operation.status.done` and a migration's
/// `Moved` are. The alternative was a `programmed_on` list, which is the shape
/// this platform has already had to remove twice: an aggregate is not a fact
/// anybody owns, and no writer can legally maintain it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SecurityGroupStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The addresses currently held by ports in this group.
    ///
    /// **Computed on read, never stored**, like everything else about a group:
    /// membership is read off the ports themselves, for the same reason the IPAM
    /// has no allocation table — a second record of the same fact is a second
    /// record that can be wrong.
    ///
    /// It is here at all because of what it saves. Expanding a port's groups
    /// needs to know who is in each referenced group, and the only place that is
    /// written down is *every port in the project*. A node resolving its own
    /// rules therefore had to read the whole cell's ports — the single largest
    /// collection there is — to program the handful it carries. Handing it the
    /// membership instead lets it read only its own ports, and it learns the
    /// addresses it was going to be told about anyway rather than every port
    /// object in the cell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// Assigned to nobody, and there is nothing to assign it to: a group is a
/// declaration that every node reads and none of them owns.
impl Assigned for SecurityGroupSpec {}

impl Observed for SecurityGroupStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type SecurityGroup = Resource<SecurityGroupSpec, SecurityGroupStatus>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("a port range only means something for tcp and udp")]
    PortsOnPortlessProtocol,
    #[error("a port range runs from the lower number to the higher one")]
    RangeInverted,
    #[error("the remote is not a CIDR")]
    RemoteNotCidr,
}

/// How many ports one rule may span.
///
/// The datapath keys a rule on a single port, so a range becomes one rule per
/// port. Sixty-four is where that stops being a translation and starts being a
/// way to fill a table.
pub const MOST_PORTS_IN_A_RULE: u32 = 64;

/// Why a rule that is perfectly well-formed cannot be put on a wire.
///
/// Separate from [`RuleError`] because it is a different kind of no.
/// `RuleError` says the rule contradicts itself and would be wrong on any
/// platform. This says the rule is coherent and *this* datapath cannot key on
/// it — a limit that could lift tomorrow without the rule having changed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unprogrammable {
    #[error(
        "`any` names every protocol, and the datapath keys a rule on one. Write it as separate tcp, udp and icmp rules, or say which protocol you meant."
    )]
    EveryProtocol,
    #[error(
        "this allows every {protocol} port, and the datapath keys a rule on one. Name a port, or a range of at most {MOST_PORTS_IN_A_RULE}."
    )]
    EveryPort { protocol: &'static str },
    #[error(
        "this spans {width} ports ({from}-{to}), and the datapath keys a rule on one. At most {MOST_PORTS_IN_A_RULE} are expanded."
    )]
    TooManyPorts { width: u32, from: u16, to: u16 },
}

/// Whether a datapath can be given this rule at all.
///
/// **Why this is refused at the door rather than discovered on a wire.** These
/// three shapes are exactly what the fabric turns down, and until it was asked
/// the rule had already been accepted, stored and shown on a screen. The port
/// then failed to program — the *whole* port, not the one rule — and the guest
/// waiting on it never started. Nothing anywhere said why: the sentence existed,
/// in an agent's journal, on a machine the person who wrote the rule does not
/// have.
///
/// Two of these are the commonest rules in any cloud — "everything from this
/// group" and "all TCP from anywhere" — so refusing them is a real loss. It is
/// still the better half of the trade: a rule that is refused with a sentence
/// naming what to write instead costs somebody a minute, and a rule that is
/// accepted and silently wedges an instance costs an afternoon.
pub fn programmable(rule: &SecurityRule) -> Result<(), Unprogrammable> {
    programmable_shape(rule.protocol, rule.ports)
}

/// The same question, asked of a rule that has already been resolved.
///
/// A [`ResolvedRule`] differs only in carrying its remote as a string, so the
/// judgement is identical — and it has to be *the same code*, because the two
/// are asked at two different doors and a second copy would drift. The API asks
/// [`programmable`] on write; a node asks this of what actually reached it,
/// which is not the same set: an object written before that door existed, a
/// restore, or a direct store write all arrive without having passed it.
pub fn programmable_resolved(rule: &ResolvedRule) -> Result<(), Unprogrammable> {
    programmable_shape(rule.protocol, rule.ports)
}

fn programmable_shape(protocol: Protocol, ports: Option<PortRange>) -> Result<(), Unprogrammable> {
    if protocol == Protocol::Any {
        return Err(Unprogrammable::EveryProtocol);
    }
    match ports {
        None if protocol.has_ports() => Err(Unprogrammable::EveryPort {
            protocol: match protocol {
                Protocol::Tcp => "tcp",
                Protocol::Udp => "udp",
                _ => "",
            },
        }),
        Some(range) => {
            let width = u32::from(range.to).saturating_sub(u32::from(range.from)) + 1;
            if width > MOST_PORTS_IN_A_RULE {
                return Err(Unprogrammable::TooManyPorts {
                    width,
                    from: range.from,
                    to: range.to,
                });
            }
            Ok(())
        }
        None => Ok(()),
    }
}

/// Refuse a rule that cannot mean what it appears to mean.
///
/// Deliberately narrow: it rejects rules that are self-contradictory, never
/// rules that are merely broad. `0.0.0.0/0` on port 22 is a bad idea and an
/// entirely valid thing for an operator to ask for, and a platform that argues
/// with them about it is one they will work around.
pub fn validate(rule: &SecurityRule) -> Result<(), RuleError> {
    if let Some(range) = rule.ports {
        if !rule.protocol.has_ports() {
            return Err(RuleError::PortsOnPortlessProtocol);
        }
        if range.from > range.to {
            return Err(RuleError::RangeInverted);
        }
    }
    if let Remote::Cidr(cidr) = &rule.remote {
        crate::network::Cidr::parse(cidr).map_err(|_| RuleError::RemoteNotCidr)?;
    }
    Ok(())
}

/// One allowance, with the remote already resolved to a prefix.
///
/// This is what a datapath is given: no group names, no indirection, nothing it
/// would have to look up. Ordering is meaningless — they are alternatives, all
/// of which permit — so the list is sorted and deduplicated purely so that two
/// passes over an unchanged world produce an identical result and the agent can
/// tell "nothing to do" from "something moved".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRule {
    pub direction: Direction,
    pub protocol: Protocol,
    pub ports: Option<PortRange>,
    pub remote: String,
}

/// What a port is actually allowed, plus what it asked for and did not get.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effective {
    pub rules: Vec<ResolvedRule>,
    /// Groups the port names that do not exist. Empty in the ordinary case.
    pub unknown_groups: Vec<String>,
}

/// Expand a port's groups into the allowances a datapath can program.
///
/// `groups` is every group in the project; `ports` is every port, needed only to
/// answer "who is in group X" — which is read off the ports themselves for the
/// same reason the IPAM has no allocation table: a second record of the same
/// fact is a second record that can be wrong.
pub fn effective_rules(
    port: &PortSpec,
    groups: &BTreeMap<String, SecurityGroupSpec>,
    ports: &BTreeMap<String, PortSpec>,
) -> Effective {
    effective_rules_with(port, groups, &|group| members_of(group, ports))
}

/// Every address in a group, as the ports say it.
///
/// The whole of what a caller needs from "every port in the project", so that a
/// caller who has been handed *that* does not need the ports.
pub fn members_in(group: &str, ports: &BTreeMap<String, PortSpec>) -> Vec<String> {
    members_of(group, ports)
}

/// The same expansion, given a way to answer "who is in this group" rather than
/// the ports to work it out from.
///
/// This exists so a node agent can resolve the rules for the ports it carries
/// without reading every port in the cell: the membership is computed once,
/// centrally, and handed to it on the group.
pub fn effective_rules_with(
    port: &PortSpec,
    groups: &BTreeMap<String, SecurityGroupSpec>,
    members: &dyn Fn(&str) -> Vec<String>,
) -> Effective {
    let mut rules: BTreeSet<ResolvedRule> = BTreeSet::new();
    let mut unknown = Vec::new();

    for name in &port.security_groups {
        let Some(group) = groups.get(name) else {
            unknown.push(name.clone());
            continue;
        };
        for rule in &group.rules {
            if validate(rule).is_err() {
                // A rule that cannot mean what it says is not silently
                // approximated. Skipping it is the safe direction — one fewer
                // allowance — and it was refused at write time anyway; this is
                // the belt to that braces, for a group written by an older
                // version of this software.
                continue;
            }
            match &rule.remote {
                Remote::Cidr(cidr) => {
                    rules.insert(ResolvedRule {
                        direction: rule.direction,
                        protocol: rule.protocol,
                        ports: rule.ports,
                        remote: cidr.clone(),
                    });
                }
                Remote::Group(remote) => {
                    // Every address currently held by a port in that group. A
                    // group with no members yields nothing, which is an empty
                    // allowance and emphatically not an open one.
                    for member in members(remote) {
                        rules.insert(ResolvedRule {
                            direction: rule.direction,
                            protocol: rule.protocol,
                            ports: rule.ports,
                            remote: host_prefix(&member),
                        });
                    }
                }
            }
        }
    }

    Effective {
        rules: rules.into_iter().collect(),
        unknown_groups: unknown,
    }
}

/// The addresses of the ports in a group. Membership is one level: this does not
/// follow the group's own rules.
fn members_of(group: &str, ports: &BTreeMap<String, PortSpec>) -> Vec<String> {
    ports
        .values()
        .filter(|p| p.security_groups.iter().any(|g| g == group))
        .filter_map(|p| p.address.clone())
        .collect()
}

/// A single address as a prefix that covers only itself.
fn host_prefix(address: &str) -> String {
    if address.contains('/') {
        return address.to_string();
    }
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => format!("{address}/32"),
        Ok(std::net::IpAddr::V6(_)) => format!("{address}/128"),
        // Not an address at all: hand it back unchanged rather than inventing a
        // prefix length for it. The datapath refuses what it cannot parse, and
        // that refusal is the right place for this to surface.
        Err(_) => address.to_string(),
    }
}

/// Whether a group is in force, computed from the ports that reference it.
///
/// `carried` is (port name, whether it is programmed at its current generation)
/// for the ports a node actually holds; `referenced` is how many name the group
/// at all. Both, because they answer different questions and only one of them
/// is an alarm: a group named by ports no node carries is not waiting on
/// anything, and reporting it as pending would be an alarm about nothing —
/// the kind that teaches people to ignore the real one.
pub fn group_condition(
    generation: u64,
    carried: &[(String, bool)],
    referenced: usize,
) -> Condition {
    let pending: Vec<&str> = carried
        .iter()
        .filter(|(_, programmed)| !programmed)
        .map(|(name, _)| name.as_str())
        .collect();
    if !pending.is_empty() {
        return Condition::new(
            "Applied",
            ConditionStatus::False,
            "PortsPending",
            &format!(
                "{} of {} carried ports have not programmed it yet: {}",
                pending.len(),
                carried.len(),
                pending.join(", ")
            ),
            generation,
        );
    }
    let message = match (referenced, carried.len()) {
        (0, _) => "not referenced by any port".to_string(),
        (referenced, 0) => {
            format!("named by {referenced} port(s), none of them carried by a node yet")
        }
        (_, 1) => "in force on 1 port".to_string(),
        (_, n) => format!("in force on {n} ports"),
    };
    Condition::new(
        "Applied",
        ConditionStatus::True,
        "InForce",
        &message,
        generation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(address: &str, groups: &[&str]) -> PortSpec {
        PortSpec {
            network: "projects/p/networks/n".into(),
            subnet: "projects/p/subnets/s".into(),
            address: Some(address.into()),
            mac: None,
            security_groups: groups.iter().map(|g| (*g).to_string()).collect(),
            rate_limit_mbit: None,
            node: None,
        }
    }

    fn rule(
        direction: Direction,
        protocol: Protocol,
        ports: Option<(u16, u16)>,
        remote: Remote,
    ) -> SecurityRule {
        SecurityRule {
            direction,
            protocol,
            ports: ports.map(|(from, to)| PortRange { from, to }),
            remote,
        }
    }

    fn web() -> SecurityGroupSpec {
        SecurityGroupSpec {
            rules: vec![rule(
                Direction::Ingress,
                Protocol::Tcp,
                Some((443, 443)),
                Remote::Cidr("0.0.0.0/0".into()),
            )],
        }
    }

    #[test]
    fn a_port_with_no_groups_is_allowed_nothing_extra() {
        let e = effective_rules(&port("10.0.0.5", &[]), &BTreeMap::new(), &BTreeMap::new());
        assert!(e.rules.is_empty());
        assert!(e.unknown_groups.is_empty());
    }

    #[test]
    fn a_cidr_rule_arrives_at_the_datapath_unchanged() {
        let groups = BTreeMap::from([("web".to_string(), web())]);
        let e = effective_rules(&port("10.0.0.5", &["web"]), &groups, &BTreeMap::new());
        assert_eq!(
            e.rules,
            vec![ResolvedRule {
                direction: Direction::Ingress,
                protocol: Protocol::Tcp,
                ports: Some(PortRange { from: 443, to: 443 }),
                remote: "0.0.0.0/0".into(),
            }]
        );
    }

    #[test]
    fn a_group_remote_expands_to_the_addresses_its_members_hold_now() {
        let groups = BTreeMap::from([(
            "db".to_string(),
            SecurityGroupSpec {
                rules: vec![rule(
                    Direction::Ingress,
                    Protocol::Tcp,
                    Some((5432, 5432)),
                    Remote::Group("web".into()),
                )],
            },
        )]);
        let ports = BTreeMap::from([
            (
                "projects/p/ports/w1".to_string(),
                port("10.0.0.5", &["web"]),
            ),
            (
                "projects/p/ports/w2".to_string(),
                port("10.0.0.6", &["web"]),
            ),
            (
                "projects/p/ports/other".to_string(),
                port("10.0.0.9", &["batch"]),
            ),
        ]);
        let e = effective_rules(&port("10.0.0.20", &["db"]), &groups, &ports);
        let remotes: Vec<&str> = e.rules.iter().map(|r| r.remote.as_str()).collect();
        assert_eq!(remotes, vec!["10.0.0.5/32", "10.0.0.6/32"]);
    }

    #[test]
    fn a_group_with_no_members_allows_nothing_rather_than_everything() {
        let groups = BTreeMap::from([(
            "db".to_string(),
            SecurityGroupSpec {
                rules: vec![rule(
                    Direction::Ingress,
                    Protocol::Tcp,
                    Some((5432, 5432)),
                    Remote::Group("web".into()),
                )],
            },
        )]);
        let e = effective_rules(&port("10.0.0.20", &["db"]), &groups, &BTreeMap::new());
        assert!(
            e.rules.is_empty(),
            "an empty group opened the port to everything: {:?}",
            e.rules
        );
    }

    #[test]
    fn a_group_that_names_itself_lets_its_members_talk_to_each_other() {
        // The single most common rule in any cloud, and the one that would break
        // if resolution followed rules instead of membership.
        let groups = BTreeMap::from([(
            "mesh".to_string(),
            SecurityGroupSpec {
                rules: vec![rule(
                    Direction::Ingress,
                    Protocol::Any,
                    None,
                    Remote::Group("mesh".into()),
                )],
            },
        )]);
        let ports = BTreeMap::from([
            (
                "projects/p/ports/a".to_string(),
                port("10.0.0.5", &["mesh"]),
            ),
            (
                "projects/p/ports/b".to_string(),
                port("10.0.0.6", &["mesh"]),
            ),
        ]);
        let e = effective_rules(&port("10.0.0.5", &["mesh"]), &groups, &ports);
        let remotes: Vec<&str> = e.rules.iter().map(|r| r.remote.as_str()).collect();
        assert_eq!(remotes, vec!["10.0.0.5/32", "10.0.0.6/32"]);
    }

    #[test]
    fn membership_does_not_chain() {
        // `db` admits `web`; `web` admits the world. A member of `web` may reach
        // `db` — the world may not, and would if resolution followed rules.
        let groups = BTreeMap::from([
            ("web".to_string(), web()),
            (
                "db".to_string(),
                SecurityGroupSpec {
                    rules: vec![rule(
                        Direction::Ingress,
                        Protocol::Tcp,
                        Some((5432, 5432)),
                        Remote::Group("web".into()),
                    )],
                },
            ),
        ]);
        let ports =
            BTreeMap::from([("projects/p/ports/w".to_string(), port("10.0.0.5", &["web"]))]);
        let e = effective_rules(&port("10.0.0.20", &["db"]), &groups, &ports);
        assert!(
            !e.rules.iter().any(|r| r.remote == "0.0.0.0/0"),
            "the world reached the database through a group reference: {:?}",
            e.rules
        );
    }

    #[test]
    fn an_unknown_group_is_reported_and_costs_no_allowance() {
        let groups = BTreeMap::from([("web".to_string(), web())]);
        let e = effective_rules(
            &port("10.0.0.5", &["web", "typo"]),
            &groups,
            &BTreeMap::new(),
        );
        assert_eq!(e.unknown_groups, vec!["typo".to_string()]);
        assert_eq!(e.rules.len(), 1, "the known group stopped working too");
    }

    #[test]
    fn two_groups_granting_the_same_thing_produce_one_allowance() {
        let groups = BTreeMap::from([("a".to_string(), web()), ("b".to_string(), web())]);
        let e = effective_rules(&port("10.0.0.5", &["a", "b"]), &groups, &BTreeMap::new());
        assert_eq!(e.rules.len(), 1);
    }

    #[test]
    fn the_expansion_is_stable_across_passes() {
        let groups = BTreeMap::from([(
            "mesh".to_string(),
            SecurityGroupSpec {
                rules: vec![
                    rule(
                        Direction::Ingress,
                        Protocol::Any,
                        None,
                        Remote::Group("mesh".into()),
                    ),
                    rule(
                        Direction::Ingress,
                        Protocol::Tcp,
                        Some((22, 22)),
                        Remote::Cidr("10.9.0.0/24".into()),
                    ),
                ],
            },
        )]);
        let ports = BTreeMap::from([
            (
                "projects/p/ports/b".to_string(),
                port("10.0.0.6", &["mesh"]),
            ),
            (
                "projects/p/ports/a".to_string(),
                port("10.0.0.5", &["mesh"]),
            ),
        ]);
        let once = effective_rules(&port("10.0.0.5", &["mesh"]), &groups, &ports);
        let twice = effective_rules(&port("10.0.0.5", &["mesh"]), &groups, &ports);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_port_range_on_icmp_is_refused_rather_than_ignored() {
        let bad = rule(
            Direction::Ingress,
            Protocol::Icmp,
            Some((0, 65535)),
            Remote::Cidr("0.0.0.0/0".into()),
        );
        assert_eq!(validate(&bad), Err(RuleError::PortsOnPortlessProtocol));
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let bad = rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((100, 10)),
            Remote::Cidr("0.0.0.0/0".into()),
        );
        assert_eq!(validate(&bad), Err(RuleError::RangeInverted));
    }

    #[test]
    fn a_remote_that_is_not_a_prefix_is_refused() {
        let bad = rule(
            Direction::Ingress,
            Protocol::Tcp,
            Some((22, 22)),
            Remote::Cidr("10.0.0.1".into()),
        );
        assert_eq!(validate(&bad), Err(RuleError::RemoteNotCidr));
    }

    #[test]
    fn a_group_named_only_by_ports_nobody_carries_is_not_an_alarm() {
        let c = group_condition(3, &[], 2);
        assert_eq!(c.status, ConditionStatus::True);
        assert!(c.message.contains("none of them carried"), "{}", c.message);
    }

    #[test]
    fn a_group_nothing_references_is_in_force_vacuously() {
        let c = group_condition(3, &[], 0);
        assert_eq!(c.status, ConditionStatus::True);
        assert_eq!(c.reason, "InForce");
    }

    #[test]
    fn a_group_a_port_has_not_programmed_yet_says_which_port() {
        let c = group_condition(
            3,
            &[
                ("projects/p/ports/a".to_string(), true),
                ("projects/p/ports/b".to_string(), false),
            ],
            2,
        );
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "PortsPending");
        assert!(c.message.contains("projects/p/ports/b"), "{}", c.message);
    }
}

#[cfg(test)]
mod programmable_tests {
    use super::*;

    fn rule(protocol: Protocol, ports: Option<PortRange>) -> SecurityRule {
        SecurityRule {
            direction: Direction::Ingress,
            protocol,
            ports,
            remote: Remote::Cidr("0.0.0.0/0".into()),
        }
    }

    /// The ordinary rule goes through untouched.
    #[test]
    fn a_rule_naming_a_protocol_and_a_port_is_fine() {
        assert!(programmable(&rule(Protocol::Tcp, Some(PortRange { from: 443, to: 443 }))).is_ok());
        assert!(
            programmable(&rule(
                Protocol::Tcp,
                Some(PortRange {
                    from: 8000,
                    to: 8063
                })
            ))
            .is_ok()
        );
        // A protocol with no ports is keyed at port zero, and needs none.
        assert!(programmable(&rule(Protocol::Icmp, None)).is_ok());
    }

    /// The two commonest rules in any cloud, both refused — with a sentence
    /// naming what to write instead. They were accepted, stored and shown, and
    /// then the whole *port* failed to program and the guest waiting on it
    /// never started, with the reason only in an agent's journal.
    #[test]
    fn every_protocol_and_every_port_are_refused_in_words() {
        let any = programmable(&rule(Protocol::Any, None)).expect_err("`any` was accepted");
        assert!(
            any.to_string().contains("separate tcp, udp and icmp"),
            "{any}"
        );

        let wide = programmable(&rule(Protocol::Tcp, None)).expect_err("portless tcp was accepted");
        assert!(wide.to_string().contains("Name a port"), "{wide}");
        assert!(wide.to_string().contains("tcp"), "{wide}");
    }

    /// And a range wider than the datapath expands.
    #[test]
    fn a_range_too_wide_to_expand_is_refused_with_its_width() {
        let all = programmable(&rule(Protocol::Tcp, Some(PortRange { from: 1, to: 65535 })))
            .expect_err("every port was accepted as a range");
        assert!(all.to_string().contains("65535 ports"), "{all}");
        assert!(all.to_string().contains("1-65535"), "{all}");
    }

    /// One more than the limit is refused; exactly the limit is not. A bound
    /// that is off by one is a bound somebody hits without understanding why.
    #[test]
    fn the_limit_is_where_it_says_it_is() {
        let at = PortRange {
            from: 100,
            to: 100 + MOST_PORTS_IN_A_RULE as u16 - 1,
        };
        assert!(programmable(&rule(Protocol::Tcp, Some(at))).is_ok());
        let over = PortRange {
            from: 100,
            to: 100 + MOST_PORTS_IN_A_RULE as u16,
        };
        assert!(programmable(&rule(Protocol::Tcp, Some(over))).is_err());
    }
}
