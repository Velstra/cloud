//! What a project has left, and what it could actually start with it.
//!
//! ## Why the two halves are never shown apart
//!
//! A quota says what a tenant is *allowed*. A cell says what there is *room
//! for*. Either one alone answers the wrong question:
//!
//!  * "24 vCPUs of quota left" is what a tenant reads before creating a guest
//!    that will never be placed, because no node has more than eight free.
//!  * "no valid host" is what they get afterwards, from a scheduler that knows
//!    nothing about quotas, several minutes and one support ticket later.
//!
//! So this puts both in one answer and names which of them is the binding one.
//! `limitedBy` is the whole point: "your quota" and "the cell" are two
//! different afternoons — one is a message to an operator asking for more, the
//! other is waiting or picking a smaller shape.
//!
//! ## Counted, never tracked
//!
//! Every number here is a sum over objects that exist, as the quota checker
//! itself does. A running total that is incremented on create and decremented
//! on delete is wrong the first time either half is missed, and it fails
//! closed — a project that slowly loses capacity it never used.

use serde::{Deserialize, Serialize};

use crate::{
    reconcile::Headroom,
    resources::Quota,
};

/// One dimension of a quota, with both sides of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dimension {
    /// The wire name, matching the field on `Quota`: `vcpus`, `memoryMib`.
    pub name: String,
    pub limit: u64,
    pub used: u64,
}

impl Dimension {
    /// A limit of zero is a limit **nobody set**, not a limit of nothing.
    ///
    /// That is the platform's own convention — the quota checker skips a zero
    /// rather than refusing everything — and it has to be honoured here too. A
    /// project created without a quota would otherwise read as a project that
    /// may not start a single guest, which is the opposite of what it is.
    pub fn unlimited(&self) -> bool {
        self.limit == 0
    }

    /// What is left, or `None` where no limit was set.
    ///
    /// Saturating, because a limit lowered under what is already in use is a
    /// real thing an operator does — and "-4 vCPUs left" is not an answer
    /// anybody can act on.
    pub fn left(&self) -> Option<u64> {
        (!self.unlimited()).then(|| self.limit.saturating_sub(self.used))
    }

    /// Whether this dimension is the one standing in the way.
    pub fn exhausted(&self) -> bool {
        self.left() == Some(0)
    }
}

/// Every dimension of one project's quota, in a fixed order.
///
/// Fixed, because this is read as a list on a screen: an order that changed
/// with the data would make the same page rearrange itself between two reads.
pub fn dimensions(limit: &Quota, used: &Quota) -> Vec<Dimension> {
    let d = |name: &str, limit: u64, used: u64| Dimension {
        name: name.to_string(),
        limit,
        used,
    };
    vec![
        d("instances", limit.instances.into(), used.instances.into()),
        d("vcpus", limit.vcpus.into(), used.vcpus.into()),
        d("memoryMib", limit.memory_mib, used.memory_mib),
        d("volumes", limit.volumes.into(), used.volumes.into()),
        d("volumeGib", limit.volume_gib, used.volume_gib),
        d(
            "floatingIps",
            limit.floating_ips.into(),
            used.floating_ips.into(),
        ),
        d(
            "loadBalancers",
            limit.load_balancers.into(),
            used.load_balancers.into(),
        ),
        d("devices", limit.devices.into(), used.devices.into()),
    ]
}

/// Which side of the answer is the binding one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LimitedBy {
    /// The tenant's own allowance. The remedy is a message to an operator.
    Quota,
    /// The machines. The remedy is waiting, or a smaller shape, or another node.
    Cell,
    /// Both stop at the same number, which is worth saying rather than picking
    /// one: an operator who raises the quota gets nothing, and a tenant told
    /// only "the cell is full" asks for hardware they do not need.
    Both,
}

/// The largest guest this project could start right now.
///
/// Not the sum of anything. The cell's free memory does not add up into a
/// guest — a hundred nodes with 2 GiB each cannot run a 4 GiB machine — so the
/// cell's side of this is `Headroom::largest_fit`, which is a fact about one
/// node rather than about a fleet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Startable {
    pub vcpus: u64,
    pub memory_mib: u64,
    /// Which side each number came from.
    pub vcpus_limited_by: LimitedBy,
    pub memory_limited_by: LimitedBy,
    /// True when nothing at all can be started — a count is out, or one of the
    /// two numbers is zero. Computed here so that no reader has to work out
    /// that "0 vCPUs" and "your instance count is used up" mean the same thing
    /// on screen.
    pub none: bool,
}

/// Combine what the tenant may have with what the cell can give.
pub fn largest_startable(dimensions: &[Dimension], room: &Headroom) -> Startable {
    let left = |name: &str| dimensions.iter().find(|d| d.name == name).and_then(Dimension::left);
    let pick = |quota: Option<u64>, cell: u64| -> (u64, LimitedBy) {
        // No limit set: the machines are the only thing in the way, and saying
        // "your quota" here would send a tenant asking for allowance they
        // already have without bound.
        let Some(quota) = quota else {
            return (cell, LimitedBy::Cell);
        };
        match quota.cmp(&cell) {
            std::cmp::Ordering::Less => (quota, LimitedBy::Quota),
            std::cmp::Ordering::Greater => (cell, LimitedBy::Cell),
            std::cmp::Ordering::Equal => (quota, LimitedBy::Both),
        }
    };
    let (vcpus, vcpus_limited_by) = pick(left("vcpus"), room.largest_fit.vcpus.into());
    let (memory_mib, memory_limited_by) = pick(left("memoryMib"), room.largest_fit.memory_mib);

    // The instance count is not a shape, so it does not narrow the numbers — it
    // decides whether there is any shape at all.
    let no_room_for_another = dimensions
        .iter()
        .find(|d| d.name == "instances")
        .map(Dimension::exhausted)
        .unwrap_or(false);

    Startable {
        vcpus,
        memory_mib,
        vcpus_limited_by,
        memory_limited_by,
        none: no_room_for_another || vcpus == 0 || memory_mib == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Capacity;

    fn room(vcpus: u32, memory_mib: u64) -> Headroom {
        Headroom {
            largest_fit: Capacity {
                vcpus,
                memory_mib,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn quota(instances: u32, vcpus: u32, memory_mib: u64) -> Quota {
        Quota {
            instances,
            vcpus,
            memory_mib,
            ..Default::default()
        }
    }

    #[test]
    fn what_is_left_is_the_limit_less_what_exists() {
        let d = dimensions(&quota(10, 40, 65_536), &quota(3, 12, 24_576));
        let vcpus = d.iter().find(|d| d.name == "vcpus").unwrap();
        assert_eq!(vcpus.left(), Some(28));
        assert!(!vcpus.exhausted());

        // A dimension nobody set a limit on is unlimited, not exhausted. The
        // quota checker skips a zero rather than refusing everything, and a
        // project created without a quota must not read here as one that may
        // not start a single guest.
        let devices = d.iter().find(|d| d.name == "devices").unwrap();
        assert!(devices.unlimited());
        assert_eq!(devices.left(), None);
        assert!(!devices.exhausted(), "an unset limit read as an exhausted one");

        // Every dimension is present whether or not it is in use: a screen that
        // showed only the interesting ones would rearrange itself between two
        // reads of the same page.
        assert_eq!(d.len(), 8);
        assert!(d.iter().any(|d| d.name == "devices"));
    }

    /// A limit lowered under what is already running is a real thing an
    /// operator does, and "-4 vCPUs left" is not an answer anybody can act on.
    #[test]
    fn a_quota_lowered_under_what_is_in_use_reads_as_nothing_left() {
        let d = dimensions(&quota(10, 8, 8192), &quota(3, 12, 24_576));
        let vcpus = d.iter().find(|d| d.name == "vcpus").unwrap();
        assert_eq!(vcpus.left(), Some(0));
        assert!(vcpus.exhausted());
    }

    /// The answer the whole module exists for: which of the two is in the way.
    #[test]
    fn the_binding_side_is_named_rather_than_left_to_be_guessed() {
        let d = dimensions(&quota(10, 40, 65_536), &quota(1, 4, 4096));

        // Plenty of quota, small machines. The remedy is a smaller guest or
        // another node — not a message asking for more allowance.
        let tight_cell = largest_startable(&d, &room(8, 16_384));
        assert_eq!(tight_cell.vcpus, 8);
        assert_eq!(tight_cell.vcpus_limited_by, LimitedBy::Cell);
        assert_eq!(tight_cell.memory_mib, 16_384);
        assert_eq!(tight_cell.memory_limited_by, LimitedBy::Cell);
        assert!(!tight_cell.none);

        // Empty machines, little allowance. The remedy is a message to an
        // operator, and telling this tenant "the cell is full" would send them
        // asking for hardware nobody needs to buy.
        let tight_quota = largest_startable(&d, &room(128, 1_048_576));
        assert_eq!(tight_quota.vcpus, 36);
        assert_eq!(tight_quota.vcpus_limited_by, LimitedBy::Quota);
        assert_eq!(tight_quota.memory_limited_by, LimitedBy::Quota);
    }

    /// Both stopping at the same number is said as such: raising the quota
    /// would give this tenant nothing, and so would emptying one node.
    #[test]
    fn two_limits_that_stop_at_the_same_number_are_both_named() {
        let d = dimensions(&quota(10, 8, 16_384), &quota(0, 0, 0));
        let both = largest_startable(&d, &room(8, 16_384));
        assert_eq!(both.vcpus_limited_by, LimitedBy::Both);
        assert_eq!(both.memory_limited_by, LimitedBy::Both);
    }

    /// A project at its instance count can start nothing, however much of
    /// everything else it has left — and that has to read as "nothing", not as
    /// a promising pair of numbers.
    #[test]
    fn a_project_at_its_instance_count_can_start_nothing_at_all() {
        let d = dimensions(&quota(3, 40, 65_536), &quota(3, 4, 4096));
        let out = largest_startable(&d, &room(64, 262_144));
        assert!(out.none, "a project that may not create another guest looked ready to");
        // The numbers are still reported: an operator raising the count wants
        // to know what it will be able to start afterwards.
        assert_eq!(out.vcpus, 36);
    }

    /// A project with no quota at all is bounded only by the machines.
    #[test]
    fn a_project_nobody_set_a_quota_on_is_limited_only_by_the_cell() {
        let d = dimensions(&Quota::default(), &quota(3, 12, 24_576));
        let out = largest_startable(&d, &room(64, 262_144));
        assert_eq!(out.vcpus, 64);
        assert_eq!(out.vcpus_limited_by, LimitedBy::Cell);
        assert_eq!(out.memory_limited_by, LimitedBy::Cell);
        assert!(!out.none, "a project with no quota looked like one that may start nothing");
    }

    /// An empty cell is "nothing can start", not "36 vCPUs available".
    #[test]
    fn a_cell_with_no_room_reads_as_nothing_rather_than_as_quota() {
        let d = dimensions(&quota(10, 40, 65_536), &quota(1, 4, 4096));
        let out = largest_startable(&d, &room(0, 0));
        assert!(out.none);
        assert_eq!(out.vcpus_limited_by, LimitedBy::Cell);
    }
}
