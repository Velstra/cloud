//! Copies that survive the pool they came from.
//!
//! ## Why this is not a snapshot
//!
//! A snapshot lives in the volume's own pool. It is fast, it costs almost
//! nothing, and it is the right tool for "I am about to upgrade this, let me be
//! able to go back an hour". It is **not** a backup: when the pool is lost, the
//! snapshot is lost with it, and the moment somebody needs it most is exactly
//! the moment it is gone.
//!
//! A backup is a copy on a [`BackupTarget`] — somewhere that is not the source
//! pool. That single property is the whole reason this module exists, and it is
//! enforced rather than documented: [`may_back_up`] refuses a target that is
//! the volume's own pool.
//!
//! ## A schedule creates objects; it is not a command
//!
//! `BackupSchedule` is a spec: "there should be a copy of this volume on that
//! target, no older than N hours, and keep the last K". A controller reads it,
//! looks at what exists, and creates a `Backup` when the newest is too old.
//! Nothing is remembered between passes and nothing is "in progress" — the same
//! shape as every other decision in this platform.
//!
//! Time enters as a parameter. [`due`] and [`prune`] take `now`, so both are
//! pure and both can be argued about in a test without a clock.
//!
//! ## Restoring
//!
//! Never in place. A volume is created *from* a backup — `source_backup`,
//! beside `source_image` and `source_snapshot` — for the reason already written
//! on those fields: an in-place restore is a command sitting in a spec, and a
//! command in a spec is performed again on every resync, undoing whatever the
//! guest wrote in between, forever, with nothing on the object to say it
//! happened.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// Where backups are kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A path an agent can write to: a local disk, or an NFS or CIFS mount the
    /// operator has already arranged.
    ///
    /// The mount is deliberately not this platform's business. A target that
    /// managed its own mounts would be a second, worse copy of what the host's
    /// init system already does — and one that could not be checked by anybody
    /// looking at the machine.
    #[default]
    Directory,
}

/// A place backups are kept. Cell-scoped: the storage belongs to the cell, and
/// which projects may use it is an authorisation question rather than a naming
/// one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupTargetSpec {
    pub kind: TargetKind,
    /// Where on the agent's filesystem. Absolute, and existing — an agent that
    /// created a target directory could create it on the wrong machine, on a
    /// root filesystem, exactly when a mount failed to come up.
    pub path: String,
    /// False stops new backups going here; what is already here stays and can
    /// still be restored from. A spec change rather than a command, so a
    /// restart cannot lose it half way.
    #[serde(default = "yes")]
    pub accepting: bool,
    /// Which pool agent reports on this target — whether the path is there,
    /// whether it can be written, how much room is left.
    ///
    /// Named by an operator rather than claimed by whoever gets there first,
    /// because a target assigned to nobody is one any agent could grab, and
    /// that is the rule this platform's access check exists to keep. A
    /// directory target lives on one machine and the operator knows which; a
    /// shared mount is visible to several and exactly one of them should be
    /// answering "is the mount up".
    ///
    /// Empty means **nobody is looking**, which is not the same as "it is
    /// broken": copies may still be written there by the pool holding the
    /// volume, and a path it cannot reach fails loudly on the backup rather
    /// than quietly here.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// How often each copy here is read back and checked. See [`never`].
    #[serde(default = "never")]
    pub verify_every_hours: u32,
}

/// How often a copy on this target should be read back and checked against the
/// digest recorded when it was written. `0` — the default — never checks.
///
/// Off by default because verification is not free: it reads every byte of a
/// copy, and on a target holding a fleet's worth of them that is real I/O
/// somebody has to have decided to spend. What is *not* a decision is whether
/// the platform can tell you: without this, "the backup exists" is the only
/// thing anybody can say, and a copy nobody has read is a belief rather than a
/// backup.
///
/// One copy per pass, the most overdue first — see [`next_to_verify`]. That
/// bounds the cost to one read per pass however many copies a target holds, and
/// means a target with more copies checks each of them less often rather than
/// the agent falling behind on everything else.
fn never() -> u32 {
    0
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupTargetStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
    /// The agent reporting on it.
    pub agent: Option<String>,
    /// Whether the path is there and writable, as the agent last looked.
    ///
    /// The one thing worth reporting: a target whose mount has gone is a target
    /// whose backups are silently not happening. `None` when nobody is looking
    /// — no agent is named in the spec — which is deliberately not the same as
    /// `Some(false)`: one is "unknown", the other is "this path could not be
    /// written half a minute ago".
    #[serde(default)]
    pub writable: Option<bool>,
    #[serde(default)]
    pub free_gib: u64,
}

/// One copy of one volume, at one moment, on one target.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupSpec {
    /// The volume copied.
    pub volume: String,
    /// Where the copy goes.
    ///
    /// Left empty, the API settles it to the cell's most roomy accepting
    /// target — a tenant cannot list targets and should not have to: where the
    /// cell keeps copies is the cell's business. Stored, it is always filled.
    #[serde(default)]
    pub target: String,
    /// The pool holding the source, derived by the API rather than asked for —
    /// the same reason a snapshot carries one: a pool agent's watch filter is
    /// then one comparison.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pool: String,
    /// The schedule that asked for it, when one did.
    ///
    /// Carried so retention can count a schedule's own copies without guessing
    /// from names, and so a backup somebody took by hand is never expired by a
    /// schedule that did not make it. A hand-made copy is somebody's decision,
    /// and expiring it would be the platform overruling them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
    /// The agent that has claimed this backup and reports on it.
    pub agent: Option<String>,
    /// True while the copy is on the target.
    ///
    /// Consulted as well as reported, exactly as a snapshot's `taken` is and
    /// for the same reason: a copy that the target no longer holds must not be
    /// made again, because a copy made now is a copy of a different moment
    /// wearing the same name.
    #[serde(default)]
    pub taken: bool,
    /// How large the source was when the copy was made — and therefore the
    /// smallest volume that can be restored from it.
    #[serde(default)]
    pub size_gib: u64,
    /// What the copy occupies on the target, which is not the same number: a
    /// compressed or sparse copy is smaller, and a target's free space is what
    /// an operator is actually watching.
    #[serde(default)]
    pub stored_bytes: u64,
    /// When the copy was made. Reported by the agent that made it, so it is the
    /// moment the bytes were read rather than the moment somebody asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_at: Option<Timestamp>,
    /// What the copy hashed to when it was written, `sha256:…`.
    ///
    /// Recorded at write time and never recomputed in place: the whole value of
    /// this field is that it was taken when the bytes were known good. A digest
    /// refreshed during verification would bless whatever is on the target now,
    /// which is precisely the thing being questioned.
    ///
    /// `None` on copies made before this existed. Those are honestly
    /// unverifiable rather than quietly assumed sound — see [`verify_error`].
    ///
    /// [`verify_error`]: BackupStatus::verify_error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// When the copy was last read back and matched its digest.
    ///
    /// This, not `taken`, is what makes a copy a backup. `taken` says an agent
    /// once wrote bytes; this says somebody has since read them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<Timestamp>,
    /// Why the last read-back did not match, when it did not.
    ///
    /// Set alongside a false `Ready` and **never** by deleting the copy. A
    /// backup that failed verification is the one moment somebody has to look
    /// at it themselves: it may be the copy that rotted, or the target's
    /// filesystem, or a restore already under way from this very file. The
    /// platform's job is to say so loudly, not to destroy the only artefact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_error: Option<String>,
}

/// "Keep a copy of this volume on that target, no older than this."
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackupScheduleSpec {
    pub volume: String,
    pub target: String,
    /// How stale the newest copy may get before another is made.
    ///
    /// Hours rather than a cron expression. A cron line can express "02:00 on
    /// the second Tuesday", and every one of those is a rule somebody has to
    /// hold in their head to answer "when will this next run" — which is the
    /// only question anybody asks a schedule. An interval answers it by
    /// looking at the newest copy.
    pub every_hours: u32,
    /// How many of this schedule's copies to keep.
    ///
    /// Counted from what exists, never tracked. Only copies this schedule made
    /// are counted or expired: a backup somebody took by hand is theirs.
    pub keep: u32,
}

impl Default for BackupScheduleSpec {
    fn default() -> Self {
        Self {
            volume: String::new(),
            target: String::new(),
            every_hours: 24,
            // Seven daily copies: a week is long enough to notice that
            // something went wrong before the evidence expires, which is what
            // retention is actually for.
            keep: 7,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackupScheduleStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
}

/// Why a backup cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The target is the volume's own pool.
    ///
    /// The one refusal this module exists for. A copy beside the original
    /// survives nothing that matters, and a platform that accepted it would be
    /// selling a promise it does not keep — the operator finds out when the
    /// pool is gone.
    #[error(
        "{target} is {pool}, which is where {volume} already lives — a copy in the same pool is \
         a snapshot, and is lost with the pool it is in. Back up to a different target."
    )]
    SameAsSource {
        volume: String,
        pool: String,
        target: String,
    },
    #[error("{target} is not accepting backups")]
    TargetNotAccepting { target: String },
    /// Reported by the agent, so this is what it last saw rather than a guess.
    #[error("{target} is not writable: the agent cannot reach {path}")]
    TargetNotWritable { target: String, path: String },
}

/// One target, as the caller has it. Taken apart rather than as a resource so
/// this module stays free of the store's types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetView {
    pub name: String,
    pub path: String,
    pub accepting: bool,
    /// What the reporting agent last saw. `None` when nobody is looking — see
    /// [`BackupTargetSpec::agent`] — which is deliberately not the same as
    /// `Some(false)`: one is "unknown", the other is "this path could not be
    /// written half a minute ago".
    pub writable: Option<bool>,
    /// The pool this target's path belongs to, when it *is* one.
    ///
    /// Set only when a target has been pointed at a directory a pool also
    /// uses — which is the mistake [`Refusal::SameAsSource`] catches. `None`
    /// for an ordinary target, which is nearly all of them.
    pub same_pool_as: Option<String>,
}

/// Whether this volume may be backed up to this target.
///
/// Answered before anything is created, because every one of these is knowable
/// in advance — and a backup that fails after reading a terabyte has cost real
/// time to tell somebody something that was true before it started.
pub fn may_back_up(volume: &str, volume_pool: &str, target: &TargetView) -> Result<(), Refusal> {
    if target.same_pool_as.as_deref() == Some(volume_pool) {
        return Err(Refusal::SameAsSource {
            volume: volume.to_string(),
            pool: volume_pool.to_string(),
            target: target.name.clone(),
        });
    }
    if !target.accepting {
        return Err(Refusal::TargetNotAccepting {
            target: target.name.clone(),
        });
    }
    // Only a reported `false` refuses. An unknown is not a refusal: a target
    // nobody is reporting on may be perfectly good, and turning silence into
    // "no" would make every backup wait for a field an operator has to know to
    // set. A path that cannot be written then fails on the copy, loudly, on the
    // backup itself.
    if target.writable == Some(false) {
        return Err(Refusal::TargetNotWritable {
            target: target.name.clone(),
            path: target.path.clone(),
        });
    }
    Ok(())
}

/// One existing backup, as the schedule logic sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupView {
    pub name: String,
    /// The schedule that made it, if any.
    pub schedule: Option<String>,
    pub taken: bool,
    /// When the object was created — known for every backup, including one
    /// that never finished.
    pub created_at: Timestamp,
    /// When the copy was finished, for the ones that were.
    pub taken_at: Option<Timestamp>,
    pub deleting: bool,
}

impl BackupView {
    /// The moment this copy represents: when it was taken if it was, otherwise
    /// when it was asked for.
    fn moment(&self) -> Timestamp {
        self.taken_at.unwrap_or(self.created_at)
    }
}

/// Whether this schedule should make a copy now.
///
/// Takes the interval rather than a spec, because the arithmetic is about
/// intervals and retention and not about backups — a snapshot schedule wants
/// exactly this and would otherwise be a second copy of it, drifting.
///
/// Due when nothing this schedule made is younger than its interval —
/// counting copies that are still being made as well as finished ones.
///
/// Counting the unfinished ones is what stops a flood: a copy of a large
/// volume takes longer than a pass, and a rule that looked only at finished
/// copies would ask for a new one every few seconds until the first landed.
/// A copy that is stuck therefore holds the schedule for one interval and no
/// longer, after which another is made — which is self-healing without anybody
/// having to notice.
pub fn due(every_hours: u32, name: &str, mine: &[BackupView], now: Timestamp) -> bool {
    let interval_ms = i128::from(every_hours) * 3_600_000;
    !mine
        .iter()
        .filter(|b| !b.deleting && b.schedule.as_deref() == Some(name))
        .any(|b| {
            let age = i128::from(now.0) - i128::from(b.moment().0);
            // A copy from the future is a clock that disagrees with itself, not
            // a copy that is due. Treated as young, because making a second
            // copy over a clock skew is worse than waiting one interval.
            age < interval_ms
        })
}

/// Which of this schedule's copies have expired.
///
/// Returns names, oldest first, and only ever copies **this schedule made**: a
/// backup somebody took by hand is theirs, and a schedule expiring it would be
/// the platform overruling a person.
///
/// Two rules that both matter:
///
/// * Only **finished** copies count toward `keep`. A week of failed attempts
///   must not expire the last copy that actually worked — which is precisely
///   the week somebody will need it.
/// * At least one finished copy always survives, whatever `keep` says. A
///   schedule set to keep zero is a misconfiguration, and the moment it is
///   noticed is the moment somebody wants the copy it would have deleted.
pub fn prune(keep: u32, name: &str, mine: &[BackupView]) -> Vec<String> {
    let mut taken: Vec<&BackupView> = mine
        .iter()
        .filter(|b| !b.deleting && b.taken && b.schedule.as_deref() == Some(name))
        .collect();
    taken.sort_by_key(|b| b.moment().0);

    let keep = keep.max(1) as usize;
    if taken.len() <= keep {
        return Vec::new();
    }
    taken[..taken.len() - keep]
        .iter()
        .map(|b| b.name.clone())
        .collect()
}

/// When this schedule will next want a copy, for a console to show.
///
/// Computed rather than stored, like everything else here. `None` when nothing
/// has been made yet, which reads as "as soon as anything runs" — the honest
/// answer, and better than inventing a time from the moment the schedule
/// happened to be created.
pub fn next_due(every_hours: u32, name: &str, mine: &[BackupView]) -> Option<Timestamp> {
    let newest = mine
        .iter()
        .filter(|b| !b.deleting && b.schedule.as_deref() == Some(name))
        .map(|b| b.moment().0)
        .max()?;
    Some(Timestamp(newest + u64::from(every_hours) * 3_600_000))
}

/// One finished copy, as verification sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyView {
    pub name: String,
    /// When the copy was finished — the earliest anything could have read it.
    pub taken_at: Timestamp,
    /// When it was last read back and matched.
    pub verified_at: Option<Timestamp>,
    pub deleting: bool,
}

/// Which copy on this target to read back now, if any.
///
/// **One per pass, the most overdue first.** Verification reads every byte of a
/// copy, so a pass that checked everything due would turn a target holding a
/// hundred copies into a pass that never ends — and the agent has volumes to
/// provision in the same loop. Bounding it to one means a busier target checks
/// each copy less often, which is the right thing to give up: the alternative
/// is an agent that falls behind on the work that is not optional.
///
/// A copy is due when it has never been read back and is older than the
/// interval, or when its last read-back is. Never reading back a copy that was
/// finished seconds ago is deliberate — the interval is how stale a *proof* may
/// get, and a fresh copy's proof is the write that just succeeded.
///
/// `every_hours == 0` verifies nothing and is the default; see
/// [`BackupTargetSpec::verify_every_hours`].
pub fn next_to_verify(every_hours: u32, copies: &[CopyView], now: Timestamp) -> Option<String> {
    if every_hours == 0 {
        return None;
    }
    let interval = i128::from(every_hours) * 3_600_000;
    copies
        .iter()
        .filter(|c| !c.deleting)
        .filter_map(|c| {
            // How long this copy's proof has been stale. Measured from the last
            // read-back, or from when it was written for one nobody has read.
            let since = c.verified_at.unwrap_or(c.taken_at);
            let stale = i128::from(now.0) - i128::from(since.0);
            // A copy from the future is a clock that disagrees with itself, not
            // a copy that is overdue — the same reading `due` takes.
            (stale >= interval).then_some((stale, &c.name))
        })
        // Most overdue first; by name where two are equally so, so that a pass
        // is reproducible rather than dependent on map order.
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
        .map(|(_, name)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    fn target(name: &str) -> TargetView {
        TargetView {
            name: name.into(),
            path: "/srv/backups".into(),
            accepting: true,
            writable: Some(true),
            same_pool_as: None,
        }
    }

    fn made(name: &str, schedule: Option<&str>, taken: bool, at: u64) -> BackupView {
        BackupView {
            name: name.into(),
            schedule: schedule.map(str::to_string),
            taken,
            created_at: Timestamp(at),
            taken_at: taken.then_some(Timestamp(at)),
            deleting: false,
        }
    }

    fn daily() -> BackupScheduleSpec {
        BackupScheduleSpec {
            volume: "projects/p1/volumes/v1".into(),
            target: "backup-targets/nightly".into(),
            every_hours: 24,
            keep: 3,
        }
    }

    /// The refusal the module exists for.
    #[test]
    fn a_target_that_is_the_volumes_own_pool_is_refused_with_the_reason() {
        let mut same = target("backup-targets/oops");
        same.same_pool_as = Some("pools/fast".into());

        let Err(Refusal::SameAsSource { pool, .. }) =
            may_back_up("projects/p1/volumes/v1", "pools/fast", &same)
        else {
            panic!("a backup into the volume's own pool was allowed");
        };
        assert_eq!(pool, "pools/fast");

        // And the sentence says *why* it is not a backup, because "refused" on
        // its own reads as a rule somebody invented.
        let said = Refusal::SameAsSource {
            volume: "v1".into(),
            pool: "pools/fast".into(),
            target: "t".into(),
        }
        .to_string();
        assert!(said.contains("lost with the pool"), "{said}");

        // A different pool is the ordinary case and is allowed.
        assert_eq!(
            may_back_up("projects/p1/volumes/v1", "pools/other", &same),
            Ok(())
        );
    }

    #[test]
    fn a_target_that_is_draining_or_unreachable_is_refused_separately() {
        let mut closed = target("backup-targets/t");
        closed.accepting = false;
        assert!(matches!(
            may_back_up("v", "pools/a", &closed),
            Err(Refusal::TargetNotAccepting { .. })
        ));

        let mut gone = target("backup-targets/t");
        gone.writable = Some(false);
        // Two reasons, not one: "an operator turned it off" and "the mount is
        // gone" lead to different actions, and a console showing them the same
        // way sends somebody to the wrong machine.
        assert!(matches!(
            may_back_up("v", "pools/a", &gone),
            Err(Refusal::TargetNotWritable { .. })
        ));
    }

    #[test]
    fn nothing_yet_means_due_now() {
        assert!(due(daily().every_hours, "s", &[], Timestamp(10 * HOUR)));
    }

    #[test]
    fn a_recent_copy_holds_the_schedule_and_an_old_one_does_not() {
        let now = Timestamp(100 * HOUR);
        let fresh = [made("b1", Some("s"), true, 90 * HOUR)];
        assert!(!due(daily().every_hours, "s", &fresh, now));

        let stale = [made("b1", Some("s"), true, 70 * HOUR)];
        assert!(due(daily().every_hours, "s", &stale, now));
    }

    /// A copy still being made holds the schedule.
    ///
    /// Without this, a volume that takes longer than a pass to copy gets a new
    /// backup asked for every few seconds until the first one lands.
    #[test]
    fn a_copy_in_flight_holds_the_schedule_rather_than_flooding_it() {
        let now = Timestamp(100 * HOUR);
        let in_flight = [made("b1", Some("s"), false, 99 * HOUR)];
        assert!(!due(daily().every_hours, "s", &in_flight, now));

        // And it holds it for one interval only: a stuck copy does not stop
        // backups forever, which is the failure that is only noticed later.
        let stuck = [made("b1", Some("s"), false, 60 * HOUR)];
        assert!(due(daily().every_hours, "s", &stuck, now));
    }

    /// Another schedule's copies, and hand-made ones, are not this schedule's.
    #[test]
    fn a_schedule_only_counts_its_own_copies() {
        let now = Timestamp(100 * HOUR);
        let others = [
            made("b1", Some("other"), true, 99 * HOUR),
            made("b2", None, true, 99 * HOUR),
        ];
        assert!(
            due(daily().every_hours, "s", &others, now),
            "a schedule was held by copies it did not make"
        );
    }

    #[test]
    fn retention_keeps_the_newest_and_expires_the_rest_oldest_first() {
        let mine = [
            made("b1", Some("s"), true, 10 * HOUR),
            made("b2", Some("s"), true, 20 * HOUR),
            made("b3", Some("s"), true, 30 * HOUR),
            made("b4", Some("s"), true, 40 * HOUR),
            made("b5", Some("s"), true, 50 * HOUR),
        ];
        assert_eq!(prune(daily().keep, "s", &mine), ["b1", "b2"]);
    }

    /// Failed attempts do not count toward `keep`.
    ///
    /// The trap this pins: a week of failures, each counted as a copy, expires
    /// the last one that actually worked — in exactly the week somebody will
    /// need it.
    #[test]
    fn a_run_of_failures_does_not_expire_the_last_copy_that_worked() {
        let mine = [
            made("good", Some("s"), true, 10 * HOUR),
            made("f1", Some("s"), false, 20 * HOUR),
            made("f2", Some("s"), false, 30 * HOUR),
            made("f3", Some("s"), false, 40 * HOUR),
            made("f4", Some("s"), false, 50 * HOUR),
        ];
        assert!(
            prune(daily().keep, "s", &mine).is_empty(),
            "failed attempts expired the only copy that worked"
        );
    }

    /// A schedule set to keep nothing still keeps one.
    #[test]
    fn keeping_zero_still_keeps_the_newest() {
        let mut zero = daily();
        zero.keep = 0;
        let mine = [
            made("b1", Some("s"), true, 10 * HOUR),
            made("b2", Some("s"), true, 20 * HOUR),
        ];
        assert_eq!(prune(zero.keep, "s", &mine), ["b1"]);
    }

    #[test]
    fn a_copy_on_its_way_out_is_neither_counted_nor_expired_twice() {
        let mut going = made("b1", Some("s"), true, 10 * HOUR);
        going.deleting = true;
        let mine = [going, made("b2", Some("s"), true, 20 * HOUR)];
        let mut tight = daily();
        tight.keep = 1;
        assert!(
            prune(tight.keep, "s", &mine).is_empty(),
            "a backup already being deleted was asked for again"
        );
    }

    #[test]
    fn hand_made_copies_are_never_expired_by_a_schedule() {
        let mine = [
            made("by-hand-1", None, true, 10 * HOUR),
            made("by-hand-2", None, true, 20 * HOUR),
            made("by-hand-3", None, true, 30 * HOUR),
            made("by-hand-4", None, true, 40 * HOUR),
        ];
        let mut tight = daily();
        tight.keep = 1;
        assert!(
            prune(tight.keep, "s", &mine).is_empty(),
            "a schedule expired copies somebody took by hand"
        );
    }

    #[test]
    fn when_the_next_copy_is_due_is_computed_from_the_newest_one() {
        let mine = [
            made("b1", Some("s"), true, 10 * HOUR),
            made("b2", Some("s"), true, 30 * HOUR),
        ];
        assert_eq!(
            next_due(daily().every_hours, "s", &mine),
            Some(Timestamp(54 * HOUR))
        );
        // Nothing yet: no answer rather than one invented from the schedule's
        // own creation time.
        assert_eq!(next_due(daily().every_hours, "s", &[]), None);
    }

    /// A copy stamped in the future does not trigger a second one.
    #[test]
    fn a_clock_that_disagrees_with_itself_does_not_cause_a_second_copy() {
        let now = Timestamp(100 * HOUR);
        let future = [made("b1", Some("s"), true, 200 * HOUR)];
        assert!(!due(daily().every_hours, "s", &future, now));
    }

    fn copy(name: &str, taken: u64, verified: Option<u64>) -> CopyView {
        CopyView {
            name: name.into(),
            taken_at: Timestamp(taken * HOUR),
            verified_at: verified.map(|h| Timestamp(h * HOUR)),
            deleting: false,
        }
    }

    /// Off by default, and off means off: a target nobody asked to verify
    /// reads nothing back, however old its copies are.
    #[test]
    fn a_target_that_was_not_asked_to_verify_reads_nothing_back() {
        let now = Timestamp(1000 * HOUR);
        let ancient = [copy("b1", 1, None)];
        assert_eq!(next_to_verify(0, &ancient, now), None);
    }

    /// One per pass, and the one whose proof is stalest — so a target whose
    /// copies outnumber its passes still gets round to all of them.
    #[test]
    fn the_copy_whose_proof_is_stalest_goes_first() {
        let now = Timestamp(100 * HOUR);
        let copies = [
            // Read back 10h ago.
            copy("recent", 1, Some(90)),
            // Never read back, written 50h ago — staler than `recent`.
            copy("never-read", 50, None),
            // Read back 40h ago.
            copy("middling", 1, Some(60)),
        ];
        assert_eq!(
            next_to_verify(24, &copies, now).as_deref(),
            Some("never-read")
        );
    }

    /// A copy that was just written is not read straight back. The interval is
    /// how stale a *proof* may get, and a fresh copy's proof is the write.
    #[test]
    fn a_copy_written_moments_ago_is_not_read_straight_back() {
        let now = Timestamp(100 * HOUR);
        let fresh = [copy("b1", 99, None)];
        assert_eq!(next_to_verify(24, &fresh, now), None);
    }

    /// Verifying resets the clock: the copy just read is not the one picked
    /// again on the next pass while another is older.
    #[test]
    fn a_copy_just_read_back_goes_to_the_end_of_the_queue() {
        let now = Timestamp(100 * HOUR);
        let before = [copy("a", 1, Some(20)), copy("b", 1, Some(30))];
        assert_eq!(next_to_verify(24, &before, now).as_deref(), Some("a"));

        let after = [copy("a", 1, Some(100)), copy("b", 1, Some(30))];
        assert_eq!(next_to_verify(24, &after, now).as_deref(), Some("b"));
    }

    /// A copy on its way out is not read back. Spending a pass proving the
    /// integrity of something about to be deleted is the one read nobody wants.
    #[test]
    fn a_copy_being_deleted_is_not_read_back() {
        let now = Timestamp(100 * HOUR);
        let mut going = copy("b1", 1, None);
        going.deleting = true;
        assert_eq!(next_to_verify(24, &[going], now), None);
    }

    /// Equally overdue copies resolve by name rather than by order, so a pass
    /// is reproducible and two agents reading the same list agree.
    #[test]
    fn equally_overdue_copies_are_picked_in_a_fixed_order() {
        let now = Timestamp(100 * HOUR);
        let copies = [copy("b", 1, None), copy("a", 1, None)];
        assert_eq!(next_to_verify(24, &copies, now).as_deref(), Some("a"));
        let reversed = [copy("a", 1, None), copy("b", 1, None)];
        assert_eq!(next_to_verify(24, &reversed, now).as_deref(), Some("a"));
    }
}
