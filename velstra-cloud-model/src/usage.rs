//! What a project consumed, and when.
//!
//! ## Why a record and not a counter
//!
//! `project.status.used` says what is in use **now**, counted from the objects
//! that exist. It is exactly right for a quota and useless for a bill: it has
//! no memory, so a guest that ran for three weeks and was deleted this morning
//! is indistinguishable from one that never existed.
//!
//! A platform that serves customers has to answer "what did they use last
//! month", and there is no honest way to answer it from the present. So the
//! present is written down, at intervals, and the answer is the sum.
//!
//! ## Sampled, and it says so
//!
//! Each record is **a reading at a moment**, not an integral over the window.
//! A guest created and destroyed between two readings is not in either of them
//! and is not billed — and that is stated here rather than discovered by a
//! customer who noticed. Making it exact would mean charging from the object
//! lifecycle instead, which means a counter, which means the drift this
//! platform refuses everywhere else: a process that dies between creating an
//! instance and charging for it loses the charge for ever, and nothing can
//! prove afterwards which happened.
//!
//! The trade is deliberate and the interval is the knob. Hourly readings bill a
//! guest that lived twenty minutes as either an hour or as nothing; a provider
//! who needs better than that shortens the interval and pays for the rows.
//!
//! ## Where they live, and who may read them
//!
//! Under the project — `projects/p1/usage/1787824800000` — so a customer can
//! read their own consumption with the same token they use for everything else,
//! and so a project's whole history goes away with the project. Named for the
//! moment they were taken, so a listing is in time order without an index.
//!
//! Written by the controller, which already counts what a quota needs. Nobody
//! else writes them and nothing edits them: a usage record that could be
//! changed after the fact is a bill nobody can stand behind.

use serde::{Deserialize, Serialize};

use crate::{meta::Timestamp, resources::Quota};

/// How long a reading is kept, in milliseconds.
///
/// Ninety days. Long enough that a dispute about last month's invoice can be
/// answered from the platform rather than from whatever the billing system
/// happened to keep, and short enough that a cell does not accumulate rows for
/// ever. A provider who needs longer exports them; this is a platform, not an
/// archive.
pub const RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1000;

/// How often a reading is taken, in milliseconds.
///
/// An hour, which is what the industry bills in and therefore what a record
/// somebody has to reconcile against an invoice should line up with.
pub const INTERVAL_MS: u64 = 60 * 60 * 1000;

/// One reading of what a project had.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecordSpec {
    /// The project this is about. Also its parent in the name — carried here as
    /// well so a record handed to a billing system on its own still says whose
    /// it is.
    pub project: String,
    /// When the reading was taken.
    pub at: Timestamp,
    /// What was in use at that moment. The same shape a quota is expressed in,
    /// so a limit and a bill are about the same things measured the same way.
    pub used: Quota,
}

/// Nothing here is written by anybody.
///
/// A reading is a fact about a moment that has passed. There is no work for it
/// to converge to and nothing to report — an empty status that exists only
/// because every resource has one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecordStatus {
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default)]
    pub conditions: Vec<crate::meta::Condition>,
}

/// The id a reading taken at `at` is filed under.
///
/// The millisecond, zero-padded to the width the epoch will need for the next
/// few centuries, so that the lexical order a store lists in **is** time order.
/// Without the padding a listing goes 1000, 10000, 999 — and a page of "the
/// last hour" would be a page of whichever ids sorted late.
pub fn id_for(at: Timestamp) -> String {
    format!("{:013}", at.0)
}

/// The moment a reading belongs to: the start of the interval containing it.
///
/// Readings are filed on the interval boundary rather than at the instant the
/// controller happened to run, so two controllers taking the same reading a
/// second apart write **one** record rather than two — the id is the same, and
/// the second create is refused as a duplicate. That is the whole of the
/// leader-election story for usage: there does not need to be one.
pub fn window_of(now: Timestamp, interval_ms: u64) -> Timestamp {
    let interval = interval_ms.max(1);
    Timestamp(now.0 - (now.0 % interval))
}

/// Whether a reading taken at `at` is old enough to take away.
pub fn expired(at: Timestamp, now: Timestamp, retention_ms: u64) -> bool {
    now.0.saturating_sub(at.0) >= retention_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store lists lexically, and a bill is read in time order. Ids that do
    /// not sort the same way as the moments they name make "the last day" a
    /// page of whichever ids happened to sort late.
    #[test]
    fn ids_sort_the_way_the_moments_do() {
        let mut ids: Vec<String> = [1_u64, 999, 1_000, 10_000, 1_787_824_800_000]
            .into_iter()
            .map(|ms| id_for(Timestamp(ms)))
            .collect();
        let ordered = ids.clone();
        ids.sort();
        assert_eq!(ids, ordered, "the ids do not sort in time order");
    }

    /// Two controllers taking the same reading a second apart must write one
    /// record, not two — and they do, because both file it under the interval
    /// it fell in rather than under the instant they ran.
    #[test]
    fn two_readings_in_one_window_are_one_record() {
        let a = window_of(Timestamp(1_787_824_800_123), INTERVAL_MS);
        let b = window_of(Timestamp(1_787_824_800_987), INTERVAL_MS);
        assert_eq!(a, b);
        assert_eq!(id_for(a), id_for(b));

        // And the next hour is a different one, or nothing would ever be
        // recorded twice.
        let next = window_of(Timestamp(1_787_824_800_123 + INTERVAL_MS), INTERVAL_MS);
        assert_ne!(a, next);
    }

    #[test]
    fn a_window_starts_on_the_hour() {
        let w = window_of(Timestamp(1_787_824_800_000 + 61_000), INTERVAL_MS);
        assert_eq!(w.0 % INTERVAL_MS, 0);
        assert!(w.0 <= 1_787_824_800_000 + 61_000);
    }

    #[test]
    fn a_reading_is_kept_for_its_retention_and_no_longer() {
        let at = Timestamp(1_000_000);
        assert!(!expired(
            at,
            Timestamp(1_000_000 + RETENTION_MS - 1),
            RETENTION_MS
        ));
        assert!(expired(
            at,
            Timestamp(1_000_000 + RETENTION_MS),
            RETENTION_MS
        ));
        // A clock that went backwards does not delete the past.
        assert!(!expired(at, Timestamp(0), RETENTION_MS));
    }
}
