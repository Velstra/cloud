//! How much of a shared pool one volume may take.
//!
//! A pool is shared and its disks are finite. Without a ceiling, one tenant
//! running `fio` against one volume takes the latency of every other volume in
//! the same pool with it — and the platform has nothing to say about it and no
//! lever to pull. The operator finds out from somebody else's ticket.
//!
//! Two numbers and two places, and the split is the whole design:
//!
//! * A **volume** carries what it asked for. A tenant sizing a database volume
//!   differently from a log volume is a legitimate thing to want, and it is
//!   theirs to say.
//! * A **pool** carries the ceiling. It is the operator's, it applies to every
//!   volume in the pool, and a tenant cannot raise it — asking for more than
//!   the ceiling gets the ceiling, not a refusal, because a refusal would make
//!   a ceiling somebody lowered break every volume that was already above it.
//!
//! The default matters more than either. A volume that names no limit on a
//! pool that has a ceiling gets **the ceiling**, not "unlimited" — otherwise
//! the lever does nothing at all, since nobody sets a limit on themselves.
//!
//! Zero means unlimited, in both places, and that is the state a cell starts
//! in: nothing here changes behaviour until an operator sets a ceiling.

use serde::{Deserialize, Serialize};

/// What one volume may do, once the pool has had its say.
///
/// Every field defaults, because zero already means "no limit of my own" — so
/// a caller naming one of the three is saying exactly that and nothing about
/// the other two. Requiring all three would make the common request (an
/// operation rate, bandwidth left to the pool) impossible to write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Read and write operations per second, together. Zero is unlimited.
    ///
    /// One number rather than one per direction: the thing a pool runs out of
    /// is operations, and a split budget is two numbers to tune for a limit
    /// that binds on their sum.
    pub iops: u32,
    /// Mebibytes per second read. Zero is unlimited.
    pub read_mibps: u32,
    /// Mebibytes per second written. Zero is unlimited.
    ///
    /// Separate from reads because they are not the same cost on any backend
    /// this platform runs on: a write to a replicated Ceph pool is three
    /// writes, and a read is one.
    pub write_mibps: u32,
}

impl Limits {
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Whether this asks for anything at all.
    pub fn is_unlimited(&self) -> bool {
        *self == Self::default()
    }
}

/// One volume's limits, once the pool's ceiling has been applied.
///
/// Field by field, because the three are independent: a volume may name an
/// operation rate and leave the bandwidth to the pool.
///
/// * asked for nothing → the ceiling (see the module doc — this is the line
///   that makes a ceiling mean anything);
/// * asked for more than the ceiling → the ceiling;
/// * asked for less → what was asked for;
/// * no ceiling → what was asked for, which may be nothing.
pub fn effective(asked: Limits, ceiling: Limits) -> Limits {
    Limits {
        iops: settle(asked.iops, ceiling.iops),
        read_mibps: settle(asked.read_mibps, ceiling.read_mibps),
        write_mibps: settle(asked.write_mibps, ceiling.write_mibps),
    }
}

fn settle(asked: u32, ceiling: u32) -> u32 {
    match (asked, ceiling) {
        (_, 0) => asked,
        (0, ceiling) => ceiling,
        (asked, ceiling) => asked.min(ceiling),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(iops: u32, read: u32, write: u32) -> Limits {
        Limits {
            iops,
            read_mibps: read,
            write_mibps: write,
        }
    }

    /// The line the whole thing turns on: a volume that asks for nothing on a
    /// pool with a ceiling gets the ceiling. Anything else and the lever does
    /// nothing, because nobody limits themselves.
    #[test]
    fn a_volume_that_asks_for_nothing_gets_the_ceiling() {
        assert_eq!(
            effective(Limits::unlimited(), l(5_000, 200, 100)),
            l(5_000, 200, 100)
        );
    }

    #[test]
    fn a_volume_asking_for_less_than_the_ceiling_keeps_what_it_asked_for() {
        assert_eq!(
            effective(l(1_000, 50, 25), l(5_000, 200, 100)),
            l(1_000, 50, 25)
        );
    }

    /// Clamped rather than refused: a ceiling somebody lowers must not break
    /// every volume that was already above it.
    #[test]
    fn a_volume_asking_for_more_than_the_ceiling_is_brought_down_to_it() {
        assert_eq!(
            effective(l(50_000, 9_000, 9_000), l(5_000, 200, 100)),
            l(5_000, 200, 100)
        );
    }

    #[test]
    fn each_of_the_three_settles_on_its_own() {
        // An operation rate asked for, bandwidth left to the pool.
        assert_eq!(
            effective(l(1_000, 0, 0), l(5_000, 200, 100)),
            l(1_000, 200, 100)
        );
    }

    /// A cell with no ceiling anywhere behaves exactly as it did before any of
    /// this existed.
    #[test]
    fn a_pool_with_no_ceiling_changes_nothing() {
        assert_eq!(
            effective(Limits::unlimited(), Limits::unlimited()),
            Limits::unlimited()
        );
        assert_eq!(
            effective(l(1_000, 0, 0), Limits::unlimited()),
            l(1_000, 0, 0)
        );
    }
    /// Naming one of the three is saying nothing about the other two. The
    /// common request — an operation rate, bandwidth left to the pool — has to
    /// be writable, and requiring all three fields made it an error.
    #[test]
    fn naming_one_limit_leaves_the_others_at_the_pools_say() {
        let asked: Limits = serde_json::from_str(r#"{"iops": 1000}"#).expect("a partial limit");
        assert_eq!(asked, l(1_000, 0, 0));
        assert_eq!(effective(asked, l(5_000, 200, 100)), l(1_000, 200, 100));
    }
}
