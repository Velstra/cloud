//! What a machine does, and the one thing it deliberately cannot say about
//! itself.
//!
//! ## Two different questions that look like one
//!
//! "What is this node's role" splits in two, and conflating them is how a
//! platform ends up letting a machine promote itself:
//!
//!  * **What runs here** — which units this box starts. That is the machine's
//!    own business, it is what an installer decides, and it lives in the seed
//!    next to the cell name and the token.
//!  * **What the cell believes about it** — that this machine carries external
//!    traffic ([`velstra_cloud_model::resources::NodeSpec::gateway`]), which
//!    labels it has, whether it is schedulable. Those live on the Node object,
//!    are written by an operator, and a node's own token may not touch them.
//!
//! The second one is not a gap in this file. A registration token exists so a
//! machine can *report* — capacity, health, what it is running — and a token
//! that could also declare its holder a gateway would be a token that grants
//! itself the cell's external traffic. So the wizard offers the first list and
//! says where the second is set.
//!
//! ## Roles are not exclusive
//!
//! The smallest real cell is one box that is all of them. The largest has
//! machines that are exactly one. So this is a set, written into the seed as a
//! comma-separated list, and every unit is conditional on its own name being in
//! it — the same shape the node agent already had, generalised from one role to
//! four.
//!
//! ## One seed, two packagings
//!
//! The file is the same on a sealed appliance, on Debian and on NixOS:
//! `/var/lib/velstra/node.env`. The appliance decides the path — its `/etc` is
//! on a read-only verity store and its writable partition mounts at
//! `/var/lib/velstra` — and having one path everywhere is worth more than
//! having the conventional one in two places out of three.

use std::fmt;

/// One thing a machine can be running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// The API and the controllers — and, unless told otherwise, the store.
    ControlPlane,
    /// A hypervisor: the node agent, and the guests it holds.
    Hypervisor,
    /// A storage pool: volumes, snapshots, backups, captures.
    Pool,
}

impl Role {
    /// As it is written in the seed and in a unit's condition.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ControlPlane => "control-plane",
            Self::Hypervisor => "hypervisor",
            Self::Pool => "pool",
        }
    }

    /// What an operator is choosing, in one line each.
    pub fn describes(self) -> &'static str {
        match self {
            Self::ControlPlane => {
                "the API and the controllers — the thing every other machine talks to"
            }
            Self::Hypervisor => "runs guests",
            Self::Pool => "holds volumes, snapshots and backups",
        }
    }

    /// The systemd units this role starts.
    pub fn units(self) -> &'static [&'static str] {
        match self {
            Self::ControlPlane => &["velstra-cloud-api", "velstra-cloud-controller"],
            Self::Hypervisor => &["velstra-cloud-nodeagent"],
            Self::Pool => &["velstra-cloud-poolagent"],
        }
    }

    pub const ALL: [Role; 3] = [Role::ControlPlane, Role::Hypervisor, Role::Pool];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == text.trim())
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The roles in a seed's `VELSTRA_ROLES`, in a fixed order and without
/// duplicates.
///
/// Fixed order because this is written into a file a person reads and a machine
/// re-reads: a set that came back in a different order every time would make
/// every seed look changed to anything comparing them.
pub fn parse_list(text: &str) -> Vec<Role> {
    let mut out: Vec<Role> = text.split(',').filter_map(Role::parse).collect();
    out.sort();
    out.dedup();
    out
}

pub fn render_list(roles: &[Role]) -> String {
    let mut roles = roles.to_vec();
    roles.sort();
    roles.dedup();
    roles
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// An empty list is a machine that has been seeded and told to do nothing.
///
/// Treated as the hypervisor role, and **only** when the seed predates roles
/// entirely — which is what `VELSTRA_ROLES` being absent means. A seed that
/// says `VELSTRA_ROLES=` out loud is somebody who meant it, and gets what they
/// asked for: nothing runs.
pub fn roles_of_seed(seed: &str) -> Vec<Role> {
    for line in seed.lines() {
        if let Some(value) = line.trim().strip_prefix("VELSTRA_ROLES=") {
            return parse_list(value);
        }
    }
    // No key at all: a seed written before this existed, and every one of those
    // was a hypervisor — that was the only thing the installer could make.
    vec![Role::Hypervisor]
}

/// Answer a unit's `ExecCondition`: does this machine's seed name `role`?
///
/// Exits rather than returning, because that is the whole interface — systemd
/// reads the status and nothing reads a message. A seed that cannot be read is
/// "no", not an error: a machine with no seed has no roles, which is exactly
/// what a freshly unpacked package looks like.
pub fn has_role_or_exit(role: &str) -> anyhow::Result<()> {
    let seed =
        std::fs::read_to_string(format!("{}/node.env", crate::setup::SEED_DIR)).unwrap_or_default();
    if has_role(&seed, role)? {
        std::process::exit(0);
    }
    std::process::exit(1);
}

/// The decision behind it, without the exit — so it can be argued about in a
/// test rather than by starting a process.
pub fn has_role(seed: &str, role: &str) -> anyhow::Result<bool> {
    let Some(wanted) = Role::parse(role) else {
        // A name that is not a role at all is a packaging mistake, and it is
        // worth being loud about: a unit conditional on a role nobody has would
        // never run and never say why.
        anyhow::bail!("{role:?} is not a role — one of control-plane, hypervisor, pool");
    };
    Ok(roles_of_seed(seed).contains(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_written_before_roles_existed_is_a_hypervisor() {
        // The only machine the installer could make until now. Reading it as
        // "nothing" would turn an upgrade into a fleet that stops running
        // guests.
        let old = "VELSTRA_NODE=node-a\nVELSTRA_CELL=cell-1\n";
        assert_eq!(roles_of_seed(old), vec![Role::Hypervisor]);
    }

    /// An empty list is different from an absent one, and it has to be: one is
    /// "this file is older than the question", the other is somebody answering
    /// it with "none".
    #[test]
    fn a_seed_that_says_no_roles_out_loud_gets_none() {
        assert_eq!(roles_of_seed("VELSTRA_ROLES=\n"), Vec::new());
    }

    #[test]
    fn roles_round_trip_in_a_fixed_order_without_duplicates() {
        let parsed = parse_list("pool,hypervisor,pool");
        assert_eq!(parsed, vec![Role::Hypervisor, Role::Pool]);
        assert_eq!(render_list(&parsed), "hypervisor,pool");
        // Whatever order they were written in, the file reads the same — a set
        // that reordered itself would make every seed look changed.
        assert_eq!(
            render_list(&parse_list("pool,hypervisor")),
            "hypervisor,pool"
        );
    }

    #[test]
    fn something_that_is_not_a_role_is_left_out_rather_than_guessed_at() {
        assert_eq!(parse_list("hypervisor,gateway"), vec![Role::Hypervisor]);
        // `gateway` in particular: it is a real thing, and it is not a unit.
        // It is what the *cell* believes about this machine, set on the Node
        // object by an operator — a token that could set it would be a token
        // that grants itself the cell's external traffic.
        assert_eq!(Role::parse("gateway"), None);
    }

    /// What a unit's `ExecCondition` asks, and the three answers it can get.
    #[test]
    fn a_unit_asks_the_seed_whether_this_machine_is_for_it() {
        let seed = "VELSTRA_ROLES=control-plane,pool\n";
        assert!(has_role(seed, "pool").unwrap());
        assert!(!has_role(seed, "hypervisor").unwrap());

        // No seed at all — a freshly unpacked package. Not an error: a machine
        // that has not been told what it is for runs nothing, and systemd shows
        // that as skipped rather than failed.
        assert!(!has_role("", "pool").unwrap());

        // A name that is not a role is a packaging mistake, and loud: a unit
        // conditional on a role nobody has would never run and never say why.
        assert!(has_role(seed, "gateway").is_err());
    }

    #[test]
    fn every_role_names_at_least_one_unit_and_says_what_it_is_for() {
        for role in Role::ALL {
            assert!(!role.units().is_empty(), "{role} starts nothing");
            assert!(!role.describes().is_empty(), "{role} explains nothing");
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
    }
}
