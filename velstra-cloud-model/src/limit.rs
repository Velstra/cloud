//! How much one caller may ask for at once.
//!
//! ## What this is for, and what it is not
//!
//! It is not security. A tenant who wants to hurt this cell has better ways
//! than making API calls, and none of them go through here. What this stops is
//! the ordinary accident: a script in a loop, a controller somebody wrote with
//! no backoff, a console left open on a page that refreshes — one tenant taking
//! the cell's write path for everybody else without ever meaning to.
//!
//! ## Writes only, and why
//!
//! A read is answered from a page-capped listing; the expensive ones were made
//! cheap where they were expensive (see `Scratch` in the API). A write is a
//! compare-and-swap against a store every controller is also writing, and a
//! thousand of them a second is contention the whole cell pays for. So the
//! bucket counts writes, and a caller reading in a loop is left alone — they
//! are only slowing themselves down.
//!
//! ## Agents are never throttled
//!
//! A node agent reports status on a cadence it does not choose: it reports when
//! something changed, and something changing is not something it can defer. An
//! agent that was refused would retry, fall behind, and eventually be judged
//! unreachable by a control plane that was itself the reason — a self-inflicted
//! outage with a plausible-looking cause. Whatever the limit is, it is not for
//! them.
//!
//! ## A bucket, not a window
//!
//! A fixed window ("100 per minute") lets a caller spend the whole allowance in
//! the first instant of every minute, which is exactly the burst it was meant
//! to stop, twice a minute, for ever. A token bucket smooths that out and still
//! lets somebody who has been quiet do a burst of real work — creating twenty
//! guests at once is a normal Tuesday, and a limiter that refuses it is a
//! limiter people route around.

use std::collections::BTreeMap;

use crate::meta::Timestamp;

/// A caller's allowance, as it stands right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bucket {
    /// Tokens available, in thousandths — integer arithmetic, because a limiter
    /// that drifts by a rounding error every request is one nobody can reason
    /// about after an hour.
    milli_tokens: u64,
    /// When it was last refilled.
    at: Timestamp,
}

/// How fast a bucket fills and how much it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    /// Sustained writes per second.
    pub per_second: u32,
    /// How many may be spent at once after being quiet. Never below
    /// `per_second`: a burst smaller than the sustained rate is not a burst.
    pub burst: u32,
}

impl Rate {
    /// The default, and the reasoning rather than the number: a person editing
    /// through a console makes a handful of writes a minute, a script creating
    /// a fleet makes a few dozen in a moment, and neither comes near this. What
    /// does is a loop with no sleep in it.
    pub const fn ordinary() -> Self {
        Self {
            per_second: 20,
            burst: 200,
        }
    }

    fn capacity_milli(&self) -> u64 {
        u64::from(self.burst.max(self.per_second)) * 1000
    }
}

/// What a limiter says about one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead.
    Allowed,
    /// Refused, and this is how long until one token is back.
    ///
    /// Carried rather than left to the caller to guess: a client that retries
    /// immediately turns one refusal into a loop, and a client that backs off
    /// arbitrarily waits far longer than it needed to.
    Wait { millis: u64 },
}

impl Bucket {
    pub fn full(rate: Rate, now: Timestamp) -> Self {
        Self {
            milli_tokens: rate.capacity_milli(),
            at: now,
        }
    }

    /// Take one token, or say how long until there is one.
    pub fn take(&mut self, rate: Rate, now: Timestamp) -> Verdict {
        self.refill(rate, now);
        if self.milli_tokens >= 1000 {
            self.milli_tokens -= 1000;
            return Verdict::Allowed;
        }
        let short = 1000 - self.milli_tokens;
        // `per_second` tokens a second is `per_second` milli-tokens a
        // millisecond, so the wait is the shortfall over the rate — and rounded
        // up, because a wait that says "0 ms" to somebody who cannot go yet is
        // an invitation to spin.
        let millis = short.div_ceil(u64::from(rate.per_second.max(1)));
        Verdict::Wait { millis }
    }

    fn refill(&mut self, rate: Rate, now: Timestamp) {
        let elapsed = now.0.saturating_sub(self.at.0);
        if elapsed == 0 {
            return;
        }
        self.at = now;
        let gained = elapsed.saturating_mul(u64::from(rate.per_second));
        self.milli_tokens = self
            .milli_tokens
            .saturating_add(gained)
            .min(rate.capacity_milli());
    }
}

/// Every caller's allowance, kept together.
///
/// A map that only grows would be a leak with a tenant's name on each row, so
/// [`Limiter::forget_idle`] drops anybody whose bucket has been full for a
/// while — a full bucket is indistinguishable from never having been seen, so
/// forgetting one changes no answer.
#[derive(Debug, Default)]
pub struct Limiter {
    buckets: BTreeMap<String, Bucket>,
}

impl Limiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&mut self, subject: &str, rate: Rate, now: Timestamp) -> Verdict {
        let bucket = self
            .buckets
            .entry(subject.to_string())
            .or_insert_with(|| Bucket::full(rate, now));
        bucket.take(rate, now)
    }

    /// Drop callers who are back to full. Cheap, and changes no answer.
    pub fn forget_idle(&mut self, rate: Rate, now: Timestamp) {
        self.buckets.retain(|_, bucket| {
            bucket.refill(rate, now);
            bucket.milli_tokens < rate.capacity_milli()
        });
    }

    pub fn tracked(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Timestamp {
        Timestamp(1_700_000_000_000 + millis)
    }

    const RATE: Rate = Rate {
        per_second: 10,
        burst: 20,
    };

    /// Somebody who has been quiet may do a burst of real work. Creating twenty
    /// guests at once is a normal Tuesday, and a limiter that refuses it is one
    /// people route around.
    #[test]
    fn a_quiet_caller_may_spend_the_whole_burst_at_once() {
        let mut limiter = Limiter::new();
        for i in 0..20 {
            assert_eq!(
                limiter.take("ada", RATE, at(0)),
                Verdict::Allowed,
                "refused at write {i} of a burst that fits"
            );
        }
        // And the twenty-first, in the same instant, waits.
        let Verdict::Wait { millis } = limiter.take("ada", RATE, at(0)) else {
            panic!("the burst was not a limit at all");
        };
        // One token at ten a second is a tenth of a second.
        assert_eq!(millis, 100);
    }

    /// The refusal says how long, because a client that guesses either spins or
    /// waits far longer than it needed to.
    #[test]
    fn waiting_the_time_you_were_told_is_enough() {
        let mut limiter = Limiter::new();
        for _ in 0..20 {
            limiter.take("ada", RATE, at(0));
        }
        let Verdict::Wait { millis } = limiter.take("ada", RATE, at(0)) else {
            panic!("not refused");
        };
        assert_eq!(limiter.take("ada", RATE, at(millis)), Verdict::Allowed);
    }

    /// The sustained rate is the sustained rate: after the burst, one a tenth
    /// of a second, for ever.
    #[test]
    fn a_caller_who_keeps_going_settles_at_the_sustained_rate() {
        let mut limiter = Limiter::new();
        for _ in 0..20 {
            limiter.take("ada", RATE, at(0));
        }
        let mut allowed = 0;
        // One second, asked every 10ms. A token arrives every 100ms, so ten of
        // the hundred-and-one attempts fit — the one at t=0 is not among them,
        // because the burst was spent in that instant.
        for step in 0..=100 {
            if limiter.take("ada", RATE, at(step * 10)) == Verdict::Allowed {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 10, "the sustained rate is not the sustained rate");
    }

    /// One caller's loop does not spend anybody else's allowance. This is the
    /// whole reason the limit is per subject rather than per cell.
    #[test]
    fn one_tenant_in_a_loop_leaves_everybody_else_alone() {
        let mut limiter = Limiter::new();
        for _ in 0..1000 {
            limiter.take("noisy", RATE, at(0));
        }
        assert_eq!(limiter.take("quiet", RATE, at(0)), Verdict::Allowed);
    }

    /// A map that only grows is a leak with a tenant's name on every row.
    #[test]
    fn a_caller_who_has_gone_quiet_is_forgotten_and_nothing_changes() {
        let mut limiter = Limiter::new();
        limiter.take("ada", RATE, at(0));
        assert_eq!(limiter.tracked(), 1);

        // Still spending: still remembered.
        limiter.forget_idle(RATE, at(50));
        assert_eq!(limiter.tracked(), 1);

        // Back to full — which is indistinguishable from never having been
        // seen, so forgetting changes no answer.
        limiter.forget_idle(RATE, at(60_000));
        assert_eq!(limiter.tracked(), 0);
        assert_eq!(limiter.take("ada", RATE, at(60_000)), Verdict::Allowed);
    }

    /// A burst smaller than the sustained rate is not a burst, and a limiter
    /// that took the smaller number would refuse a caller who is within their
    /// own rate.
    #[test]
    fn a_burst_below_the_sustained_rate_is_read_as_the_sustained_rate() {
        let odd = Rate {
            per_second: 10,
            burst: 1,
        };
        let mut limiter = Limiter::new();
        for i in 0..10 {
            assert_eq!(
                limiter.take("ada", odd, at(0)),
                Verdict::Allowed,
                "refused at {i}, inside the caller's own per-second rate"
            );
        }
    }
}
