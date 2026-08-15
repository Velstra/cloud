//! The work queue every controller reconciles out of.
//!
//! Three properties, and each one exists because of a specific way a control
//! plane falls over:
//!
//! * **Deduplicated.** A key queued while it is already waiting is queued once.
//!   A hundred watch events about one object are one reconcile, because the
//!   reconcile reads the object fresh and closes whatever gap it finds.
//! * **Rate limited.** No more than one key is handed out per `rate`, across
//!   the whole queue. This is what stops a resync of ten thousand objects, or
//!   one object rewriting itself in a cycle, from taking a core.
//! * **Backed off per object.** A key that fails goes to the back of a delay
//!   queue for [`crate::backoff`] long. The failure is charged to the object,
//!   never to the controller, so one poisoned object cannot slow its
//!   neighbours down.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    sync::Mutex,
    time::Duration,
};

use tokio::{sync::Notify, time::Instant};

use crate::backoff::backoff;

pub struct WorkQueue {
    rate: Duration,
    base: Duration,
    ceiling: Duration,
    inner: Mutex<Inner>,
    wake: Notify,
}

#[derive(Default)]
struct Inner {
    ready: VecDeque<String>,
    /// Keys waiting out a backoff, soonest first.
    delayed: BinaryHeap<Reverse<(Instant, String)>>,
    /// Everything in `ready` or `delayed`, so an add can be deduplicated
    /// without scanning either.
    waiting: HashSet<String>,
    /// Consecutive failures per key, cleared by a success.
    failures: HashMap<String, u32>,
    last_yield: Option<Instant>,
    closed: bool,
}

impl WorkQueue {
    pub fn new(rate: Duration, base: Duration, ceiling: Duration) -> Self {
        Self {
            rate,
            base,
            ceiling,
            inner: Mutex::new(Inner::default()),
            wake: Notify::new(),
        }
    }

    /// Ask for `key` to be reconciled. Idempotent while it is still waiting.
    pub fn add(&self, key: &str) {
        {
            let mut inner = self.lock();
            if inner.closed || inner.waiting.contains(key) {
                return;
            }
            inner.waiting.insert(key.to_string());
            inner.ready.push_back(key.to_string());
        }
        self.wake.notify_one();
    }

    /// Record a failure and requeue after the backoff. Returns the delay, for
    /// the log line that tells an operator how long this object has been bad.
    ///
    /// Any *ready* copy of the key is pulled back out first. Without that, an
    /// object whose failing reconcile also writes — and so wakes its own watch
    /// — would re-enter the queue ahead of its own backoff and spin exactly
    /// the way the backoff exists to prevent.
    pub fn failed(&self, key: &str) -> Duration {
        let delay = {
            let mut inner = self.lock();
            if inner.closed {
                return Duration::ZERO;
            }
            let failures = inner
                .failures
                .entry(key.to_string())
                .and_modify(|n| *n = n.saturating_add(1))
                .or_insert(1);
            let delay = backoff(*failures, self.base, self.ceiling);
            inner.ready.retain(|k| k != key);
            inner.waiting.insert(key.to_string());
            inner
                .delayed
                .push(Reverse((Instant::now() + delay, key.to_string())));
            delay
        };
        self.wake.notify_one();
        delay
    }

    /// The object reconciled cleanly: forget its failure history, so the next
    /// failure starts at the base delay rather than where the last streak
    /// ended.
    pub fn done(&self, key: &str) {
        self.lock().failures.remove(key);
    }

    pub fn depth(&self) -> usize {
        let inner = self.lock();
        inner.ready.len() + inner.delayed.len()
    }

    pub fn failures(&self, key: &str) -> u32 {
        self.lock().failures.get(key).copied().unwrap_or(0)
    }

    /// Stop handing out work; every waiter gets `None`.
    pub fn close(&self) {
        self.lock().closed = true;
        self.wake.notify_waiters();
    }

    /// The next key to reconcile, once the rate limit and its own backoff
    /// allow it. `None` once the queue is closed.
    ///
    /// Cancellation-safe: nothing is removed from the queue until the moment a
    /// key is returned, so a caller that drops this future inside a `select!`
    /// has not swallowed a key.
    pub async fn next(&self) -> Option<String> {
        loop {
            let wait = {
                let mut inner = self.lock();
                if inner.closed {
                    return None;
                }
                let now = Instant::now();
                inner.promote(now);

                let rate_wait = inner
                    .last_yield
                    .map(|last| (last + self.rate).saturating_duration_since(now))
                    .unwrap_or(Duration::ZERO);

                if rate_wait.is_zero() {
                    if let Some(key) = inner.ready.pop_front() {
                        inner.waiting.remove(&key);
                        inner.last_yield = Some(now);
                        return Some(key);
                    }
                }

                if inner.ready.is_empty() {
                    inner
                        .delayed
                        .peek()
                        .map(|Reverse((at, _))| at.saturating_duration_since(now))
                } else {
                    Some(rate_wait)
                }
            };

            match wait {
                // Both arms, rather than a sleep alone: work that arrives while
                // we are waiting out a delay must not sit there until a timer
                // that was set for something else fires.
                Some(delay) => {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = self.wake.notified() => {}
                    }
                }
                None => self.wake.notified().await,
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("the work queue lock is never held across an await")
    }
}

impl Inner {
    fn promote(&mut self, now: Instant) {
        while let Some(Reverse((at, _))) = self.delayed.peek() {
            if *at > now {
                return;
            }
            let Some(Reverse((_, key))) = self.delayed.pop() else {
                return;
            };
            // A key can be in the heap twice if it failed twice without being
            // handed out in between; the second copy is dropped here rather
            // than reconciled twice.
            if self.waiting.contains(&key) && !self.ready.contains(&key) {
                self.ready.push_back(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> WorkQueue {
        WorkQueue::new(
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_secs(10),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn one_object_asked_for_twice_is_one_reconcile() {
        let q = queue();
        q.add("a");
        q.add("a");
        q.add("b");
        assert_eq!(q.next().await.unwrap(), "a");
        assert_eq!(q.next().await.unwrap(), "b");
        assert_eq!(q.depth(), 0, "a duplicated key was queued twice");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_object_waits_longer_every_time() {
        let q = queue();
        q.add("a");
        assert_eq!(q.next().await.unwrap(), "a");
        assert_eq!(q.failed("a"), Duration::from_millis(100));
        assert_eq!(q.next().await.unwrap(), "a");
        assert_eq!(q.failed("a"), Duration::from_millis(200));
        q.done("a");
        q.add("a");
        assert_eq!(q.next().await.unwrap(), "a");
        assert_eq!(
            q.failed("a"),
            Duration::from_millis(100),
            "a success did not clear the streak"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_poisoned_object_does_not_starve_the_rest() {
        // The shape of the incident this prevents: one object whose reconcile
        // can never succeed, holding the loop while a thousand healthy ones
        // wait behind it.
        let q = queue();
        q.add("poison");
        for i in 0..5 {
            q.add(&format!("healthy-{i}"));
        }
        assert_eq!(q.next().await.unwrap(), "poison");
        q.failed("poison");

        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(q.next().await.unwrap());
        }
        assert!(
            seen.iter().all(|k| k.starts_with("healthy-")),
            "the poisoned object came back before the healthy ones were served: {seen:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_backoff_survives_the_object_re_announcing_itself() {
        // A failing reconcile that writes wakes its own watch. If that re-add
        // beat the backoff, the object would be retried at full speed forever.
        let q = queue();
        q.add("a");
        q.next().await;
        q.failed("a");
        q.add("a");
        assert_eq!(
            q.depth(),
            1,
            "the re-add queued a second copy alongside the delayed one"
        );
        let started = Instant::now();
        q.next().await;
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the object came back before its backoff elapsed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_limit_spaces_work_out() {
        let q = WorkQueue::new(
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_secs(10),
        );
        for i in 0..4 {
            q.add(&format!("k{i}"));
        }
        let started = Instant::now();
        for _ in 0..4 {
            q.next().await;
        }
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "four keys came out faster than the rate limit allows"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_queue_releases_its_waiter() {
        let q = std::sync::Arc::new(queue());
        let waiter = tokio::spawn({
            let q = q.clone();
            async move { q.next().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        q.close();
        assert!(
            waiter.await.unwrap().is_none(),
            "a shutdown left a loop parked"
        );
    }
}
