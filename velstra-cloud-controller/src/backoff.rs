//! How long to wait before looking at an object that just failed.
//!
//! Arithmetic, so it is testable without a clock: `base` doubled once per
//! consecutive failure, capped at `ceiling`.
//!
//! Deliberately without jitter. Jitter exists to stop a thousand clients
//! retrying in lockstep, and the thing that would cause that here — a store
//! outage failing every object at once — is already bounded by the queue's rate
//! limit, which spaces retries out no matter what the backoff says. What jitter
//! would cost is a test that has to accept a range instead of a number, and a
//! backoff nobody can predict is a backoff nobody notices is wrong.

use std::time::Duration;

/// The delay after `failures` consecutive failures. `failures` counts from one:
/// the first failure waits `base`.
pub fn backoff(failures: u32, base: Duration, ceiling: Duration) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // Saturating rather than wrapping: an object that has failed sixty times is
    // asking for the ceiling, not for a two-second retry because a shift
    // overflowed.
    let doublings = failures - 1;
    let delay = base
        .checked_mul(1u32.checked_shl(doublings.min(31)).unwrap_or(u32::MAX))
        .unwrap_or(ceiling);
    delay.min(ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(100);
    const CEILING: Duration = Duration::from_secs(60);

    #[test]
    fn the_first_failure_waits_the_base_and_each_one_doubles() {
        assert_eq!(backoff(1, BASE, CEILING), Duration::from_millis(100));
        assert_eq!(backoff(2, BASE, CEILING), Duration::from_millis(200));
        assert_eq!(backoff(3, BASE, CEILING), Duration::from_millis(400));
    }

    #[test]
    fn a_permanently_broken_object_settles_at_the_ceiling() {
        // The failure mode this prevents: an object whose reconcile can never
        // succeed, retried forever at a rate that costs a core.
        assert_eq!(backoff(20, BASE, CEILING), CEILING);
        assert_eq!(backoff(u32::MAX, BASE, CEILING), CEILING);
    }

    #[test]
    fn success_is_no_wait_at_all() {
        assert_eq!(backoff(0, BASE, CEILING), Duration::ZERO);
    }
}
