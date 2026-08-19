//! Which controller process is allowed to act.
//!
//! Every controller here is level-triggered and idempotent, so two of them
//! reconciling the same object mostly write the same thing twice. "Mostly" is
//! not a guarantee anybody should build on: two schedulers can place the same
//! instance on two different nodes in the same instant, and two operation
//! pruners can both decide a record is old enough to delete. So exactly one
//! process acts, the others stand ready, and a failure of the acting one is a
//! pause rather than an outage.
//!
//! **Built on compare-and-swap, not on a lease the store grants.** The
//! [`Store`] trait deliberately exposes no leases — it is the intersection of
//! what etcd and FoundationDB both do well, and a lease is neither. What it does
//! expose is a conditional write, and that is enough: the lease is an ordinary
//! record, and every acquisition, renewal and takeover is a write conditional on
//! the revision the writer read. Two challengers racing therefore cannot both
//! win; one of the two writes is refused by the store.
//!
//! ## The clock, which is where this usually goes wrong
//!
//! The tempting design is to write "this lease expires at T" and let a
//! challenger compare T against its own clock. That makes correctness depend on
//! two machines agreeing about the time. They do not: a challenger whose clock
//! runs ahead takes over a lease that is still held, and there are two leaders.
//!
//! So no clock is ever compared with another clock here. A challenger measures
//! **how long the record has failed to change, on its own clock**: it remembers
//! the revision it last saw and when it saw it, and only when *that* has stood
//! still for a whole lease duration does it conclude the holder is gone. Every
//! process only ever measures elapsed time locally, which is the one thing a
//! clock is reliable for.
//!
//! `renewed_at` is written into the record for a person reading it with a
//! debugger. **Nothing reads it back.** It is there so a renewal is a genuinely
//! different value on a store that might otherwise collapse an identical write,
//! and the moment a decision starts depending on it this becomes the design in
//! the paragraph above.
//!
//! ## What this does not give you
//!
//! It bounds how many processes *believe* they lead. It does not fence a write
//! from a process that believed it led a moment ago — a leader can be paused
//! between reading an object and writing it, lose the lease meanwhile, and wake
//! up to complete the write. Real fencing needs a token the store itself checks
//! against, which no trait built on "get, put-if, delete, watch" can express.
//!
//! What bounds the damage instead is the rest of the design: every write is a
//! compare-and-swap on the object's own revision, so a stale leader's write
//! lands only if nothing has touched that object since it read it — and a new
//! leader that has already acted on it makes the stale write fail. The exposure
//! is one object, in the window between one leader's read and its write, and
//! only when the new leader has not yet reached the same object. Stated plainly
//! because the alternative is to imply a guarantee that is not here.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use velstra_cloud_model::meta::Timestamp;
use velstra_cloud_store::{Expect, Store, key_for};

/// How long a lease stands without renewal before a challenger may take it, and
/// how often the holder renews.
#[derive(Clone, Copy, Debug)]
pub struct ElectionConfig {
    /// How long an unchanged lease record is tolerated before takeover.
    ///
    /// This is the outage a leader's death costs: nothing is reconciled between
    /// its last renewal and a challenger noticing. Everything here is
    /// level-triggered, so the cost is latency and never lost work.
    pub lease: Duration,
    /// How often the holder renews. Must be comfortably below `lease`, because
    /// the difference is the budget for a slow store, a scheduling hiccup and a
    /// retry — spend it all and a healthy leader loses its own lease.
    pub renew: Duration,
    /// How often a follower looks again.
    pub poll: Duration,
}

impl Default for ElectionConfig {
    fn default() -> Self {
        Self {
            // 15/5 is three renewal attempts inside one lease: two may fail
            // outright and the leader still keeps it.
            lease: Duration::from_secs(15),
            renew: Duration::from_secs(5),
            poll: Duration::from_secs(2),
        }
    }
}

/// The stored record. Small on purpose — everything a decision needs is the
/// holder and the revision the store attaches.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lease {
    holder: String,
    /// Diagnostic only. See the module note: reading this back would reintroduce
    /// the cross-machine clock comparison the design exists to avoid.
    renewed_at: Timestamp,
}

fn lease_key(cell: &str) -> String {
    // One lease for the process, not one per controller. They share a store and
    // partition their work by resource kind, so a split where one process leads
    // the scheduler and another leads the volumes buys nothing and doubles the
    // number of ways to be half-elected.
    key_for(cell, "leases", "controller")
}

/// Campaign for leadership until `shutdown`, publishing whether this process
/// currently leads.
///
/// The receiver starts `false`: a process that cannot reach the store has not
/// won, and starting optimistically is how a partitioned process acts for as
/// long as it takes to find out otherwise.
pub fn elect(
    store: Arc<dyn Store>,
    cell: &str,
    identity: &str,
    config: ElectionConfig,
    shutdown: watch::Receiver<bool>,
) -> (watch::Receiver<bool>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = watch::channel(false);
    let key = lease_key(cell);
    let me = identity.to_string();
    let handle = tokio::spawn(async move {
        campaign(store, key, me, config, tx, shutdown).await;
    });
    (rx, handle)
}

async fn campaign(
    store: Arc<dyn Store>,
    key: String,
    me: String,
    config: ElectionConfig,
    leading: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    // What this process last observed about somebody else's lease: the revision,
    // and when it first saw that revision — on this machine's clock, never
    // anybody else's.
    let mut seen: Option<(velstra_cloud_model::meta::Revision, Instant)> = None;
    // When this process last renewed successfully. A leader that cannot reach
    // the store must give up before a challenger could take over, or there are
    // two leaders — so this is measured and acted on, not assumed.
    let mut held_since: Option<Instant> = None;

    loop {
        if *shutdown.borrow() {
            break;
        }
        let am_leader = *leading.borrow();
        let wait = match step(&store, &key, &me, config, am_leader, &mut seen).await {
            Outcome::Leading => {
                held_since = Some(Instant::now());
                if !am_leader {
                    info!(holder = %me, "elected: this process now acts for the cell");
                    let _ = leading.send(true);
                }
                config.renew
            }
            Outcome::Following { holder } => {
                held_since = None;
                if am_leader {
                    info!(%holder, "lost the lease; standing down");
                    let _ = leading.send(false);
                }
                config.poll
            }
            Outcome::Unreachable(error) => {
                warn!(%error, "cannot reach the store to renew the lease");
                // A leader that has not renewed within a whole lease duration
                // must assume a challenger has taken over, whether or not it can
                // see that happen. Standing down here is what keeps a partition
                // from producing two leaders.
                if am_leader {
                    let stale = held_since
                        .map(|t| t.elapsed() >= config.lease)
                        .unwrap_or(true);
                    if stale {
                        warn!("could not renew within the lease; standing down");
                        let _ = leading.send(false);
                        held_since = None;
                    }
                }
                config.poll
            }
        };

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {}
        }
    }

    // Release on the way out, so a planned restart is a pause of one poll rather
    // than of a whole lease. Best effort: if this fails the lease simply expires,
    // which is the path an unplanned death takes anyway.
    if *leading.borrow() {
        let _ = leading.send(false);
        if let Ok(Some(entry)) = store.get(&key).await
            && let Ok(lease) = serde_json::from_slice::<Lease>(&entry.value)
            && lease.holder == me
        {
            let _ = store.delete(&key, Expect::Revision(entry.revision)).await;
            info!(holder = %me, "released the lease on the way out");
        }
    }
}

enum Outcome {
    Leading,
    Following { holder: String },
    Unreachable(velstra_cloud_store::StoreError),
}

/// One pass of the campaign.
async fn step(
    store: &Arc<dyn Store>,
    key: &str,
    me: &str,
    config: ElectionConfig,
    am_leader: bool,
    seen: &mut Option<(velstra_cloud_model::meta::Revision, Instant)>,
) -> Outcome {
    let current = match store.get(key).await {
        Ok(v) => v,
        Err(e) => return Outcome::Unreachable(e),
    };
    let record = Lease {
        holder: me.to_string(),
        renewed_at: Timestamp::now(),
    };
    let bytes = serde_json::to_vec(&record).expect("a lease always serialises");

    let Some(entry) = current else {
        // Nobody holds it. `Absent` is what makes a race between two empty-store
        // starters have exactly one winner.
        return match store.put(key, bytes, Expect::Absent).await {
            Ok(_) => {
                *seen = None;
                Outcome::Leading
            }
            Err(velstra_cloud_store::StoreError::Exists { .. }) => {
                debug!("another process created the lease first");
                Outcome::Following {
                    holder: "another process".into(),
                }
            }
            Err(e) => Outcome::Unreachable(e),
        };
    };

    let held_by = serde_json::from_slice::<Lease>(&entry.value)
        .map(|l| l.holder)
        // A record this version cannot read is not a reason to seize the lease:
        // it is far more likely a newer peer wrote it than that it is corrupt.
        .unwrap_or_else(|_| "an unreadable holder".into());

    if held_by == me {
        // Renew, conditional on the revision just read: if somebody took the
        // lease between the read and this write, the store refuses and this
        // process learns it is no longer the leader instead of asserting it.
        return match store
            .put(key, bytes, Expect::Revision(entry.revision))
            .await
        {
            Ok(_) => Outcome::Leading,
            Err(velstra_cloud_store::StoreError::Conflict { .. }) => Outcome::Following {
                holder: "another process".into(),
            },
            Err(e) => Outcome::Unreachable(e),
        };
    }

    // Somebody else holds it. Time how long *this* record has stood still, on
    // this machine's clock — never comparing against the holder's.
    let now = Instant::now();
    match seen {
        Some((revision, first_seen)) if *revision == entry.revision => {
            if now.duration_since(*first_seen) < config.lease {
                return Outcome::Following { holder: held_by };
            }
            // Unchanged for a whole lease: presume the holder is gone and
            // challenge, conditional on that same revision so a second
            // challenger cannot also win.
            match store
                .put(key, bytes, Expect::Revision(entry.revision))
                .await
            {
                Ok(_) => {
                    *seen = None;
                    info!(previous = %held_by, "took over a lease that stopped being renewed");
                    Outcome::Leading
                }
                Err(velstra_cloud_store::StoreError::Conflict { .. }) => {
                    // Somebody renewed or took it first; start timing again.
                    *seen = Some((entry.revision, now));
                    Outcome::Following { holder: held_by }
                }
                Err(e) => Outcome::Unreachable(e),
            }
        }
        _ => {
            // First sight, or it changed: the holder is alive as of now.
            if am_leader {
                debug!(%held_by, "another process holds the lease");
            }
            *seen = Some((entry.revision, now));
            Outcome::Following { holder: held_by }
        }
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_store::MemoryStore;

    use super::*;

    const CELL: &str = "cell-1";

    fn fast() -> ElectionConfig {
        // Short enough that a test is not a stopwatch exercise, and still in the
        // ratio the real config uses: renewal well inside the lease.
        ElectionConfig {
            lease: Duration::from_millis(300),
            renew: Duration::from_millis(80),
            poll: Duration::from_millis(40),
        }
    }

    async fn leader_of(store: &Arc<dyn Store>) -> Option<String> {
        let entry = store.get(&lease_key(CELL)).await.unwrap()?;
        serde_json::from_slice::<Lease>(&entry.value)
            .ok()
            .map(|l| l.holder)
    }

    /// A store that lets somebody else win the race between this candidate's
    /// read and its write.
    ///
    /// The interleaving that matters cannot be produced by running two
    /// candidates and hoping: `join!` polls them on one task and the memory
    /// store's operations do not yield, so the first `step` finishes before the
    /// second begins and there is no race to observe. This forces it — the
    /// lease is empty when the candidate looks, and taken by the time it writes,
    /// which is exactly the window `Expect::Absent` exists to close.
    struct RaceLost {
        inner: Arc<MemoryStore>,
        key: String,
    }

    #[async_trait::async_trait]
    impl Store for RaceLost {
        async fn get(
            &self,
            key: &str,
        ) -> std::result::Result<Option<velstra_cloud_store::Entry>, velstra_cloud_store::StoreError>
        {
            let out = self.inner.get(key).await?;
            if out.is_none() && key == self.key {
                // Another process takes the lease, after this caller has already
                // seen an empty one.
                let record = serde_json::to_vec(&Lease {
                    holder: "somebody-else".into(),
                    renewed_at: Timestamp::now(),
                })
                .unwrap();
                self.inner.put(key, record, Expect::Absent).await?;
            }
            Ok(out)
        }
        async fn list(
            &self,
            prefix: &str,
        ) -> std::result::Result<Vec<velstra_cloud_store::Entry>, velstra_cloud_store::StoreError>
        {
            self.inner.list(prefix).await
        }
        async fn list_page(
            &self,
            prefix: &str,
            after: Option<&str>,
            limit: usize,
        ) -> std::result::Result<velstra_cloud_store::Page, velstra_cloud_store::StoreError>
        {
            self.inner.list_page(prefix, after, limit).await
        }
        async fn put(
            &self,
            key: &str,
            value: Vec<u8>,
            expect: Expect,
        ) -> std::result::Result<velstra_cloud_model::meta::Revision, velstra_cloud_store::StoreError>
        {
            self.inner.put(key, value, expect).await
        }
        async fn delete(
            &self,
            key: &str,
            expect: Expect,
        ) -> std::result::Result<velstra_cloud_model::meta::Revision, velstra_cloud_store::StoreError>
        {
            self.inner.delete(key, expect).await
        }
        fn watch(
            &self,
            prefix: &str,
            from: Option<velstra_cloud_model::meta::Revision>,
        ) -> tokio::sync::mpsc::Receiver<velstra_cloud_store::Event> {
            self.inner.watch(prefix, from)
        }
        async fn revision(
            &self,
        ) -> std::result::Result<velstra_cloud_model::meta::Revision, velstra_cloud_store::StoreError>
        {
            self.inner.revision().await
        }
    }

    /// A candidate that loses the race between reading an empty lease and
    /// claiming it does not end up believing it leads.
    ///
    /// This is the acquisition half of the invariant, and it is the half a
    /// convergence test cannot see: with an unconditional write both starters
    /// take the lease, then the next pass settles on whichever name survived and
    /// the loser stands down — so sampling afterwards reports one leader either
    /// way. Verified by mutation: replacing `Expect::Absent` with `Expect::Any`
    /// leaves every other test in this module passing and fails this one.
    #[tokio::test]
    async fn a_candidate_that_loses_the_acquisition_race_does_not_lead() {
        let key = lease_key(CELL);
        let store: Arc<dyn Store> = Arc::new(RaceLost {
            inner: Arc::new(MemoryStore::new()),
            key: key.clone(),
        });
        let mut seen = None;

        let outcome = step(&store, &key, "a", fast(), false, &mut seen).await;

        assert!(
            matches!(outcome, Outcome::Following { .. }),
            "a candidate that lost the race still believes it leads"
        );
        assert_eq!(
            leader_of(&store).await.as_deref(),
            Some("somebody-else"),
            "the loser overwrote the winner's lease"
        );
    }

    /// Two candidates against one store: exactly one leads, and the other says
    /// so rather than assuming.
    #[tokio::test]
    async fn only_one_of_two_candidates_leads() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let (_stop, shutdown) = watch::channel(false);

        let (a, _ha) = elect(store.clone(), CELL, "a", fast(), shutdown.clone());
        let (b, _hb) = elect(store.clone(), CELL, "b", fast(), shutdown.clone());

        // Long enough for both to have tried several times.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let (leads_a, leads_b) = (*a.borrow(), *b.borrow());
        assert!(
            leads_a ^ leads_b,
            "expected exactly one leader, got a={leads_a} b={leads_b}"
        );
        // And the store agrees with whichever one thinks it leads.
        let holder = leader_of(&store).await.expect("a lease was written");
        assert_eq!(
            holder == "a",
            leads_a,
            "the record names a different process"
        );
    }

    /// A leader that stops renewing is replaced, and not before the lease is up.
    ///
    /// The two halves are one test on purpose: a takeover that is merely *fast*
    /// is the bug, not the feature — it means a healthy leader can be displaced.
    #[tokio::test]
    async fn a_silent_leader_is_replaced_only_after_the_lease_stands_still() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let config = fast();

        // "a" wins, then stops campaigning entirely — a process that died
        // without releasing anything.
        let (stop_a, shutdown_a) = watch::channel(false);
        let (a, ha) = elect(store.clone(), CELL, "a", config, shutdown_a);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            *a.borrow(),
            "the first candidate did not take an empty lease"
        );
        // Abort rather than shut down: shutting down releases the lease, which
        // is the graceful path. This is the ungraceful one.
        drop(stop_a);
        ha.abort();
        let died_at = Instant::now();

        let (_stop_b, shutdown_b) = watch::channel(false);
        let (b, _hb) = elect(store.clone(), CELL, "b", config, shutdown_b);

        // Before the lease has stood still long enough, "b" must NOT have taken
        // it: a lease that can be seized early is not a lease.
        tokio::time::sleep(config.lease / 2).await;
        assert!(
            !*b.borrow(),
            "a challenger seized the lease after {:?}, less than the {:?} lease",
            died_at.elapsed(),
            config.lease
        );

        // And after it has, "b" takes over without anybody intervening.
        for _ in 0..40 {
            if *b.borrow() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(*b.borrow(), "the lease was never taken over");
        assert!(
            died_at.elapsed() >= config.lease,
            "the takeover happened before the lease could have expired"
        );
        assert_eq!(leader_of(&store).await.as_deref(), Some("b"));
    }

    /// A leader that shuts down gracefully hands the lease back, so a restart
    /// costs one poll instead of a whole lease.
    #[tokio::test]
    async fn a_graceful_shutdown_releases_the_lease() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let (stop, shutdown) = watch::channel(false);
        let (a, handle) = elect(store.clone(), CELL, "a", fast(), shutdown);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(*a.borrow());

        stop.send(true).unwrap();
        handle.await.unwrap();

        assert_eq!(
            leader_of(&store).await,
            None,
            "a process that stopped on purpose left its lease behind"
        );
    }

    /// Losing the lease to somebody else stands this process down rather than
    /// letting it go on believing it leads.
    #[tokio::test]
    async fn a_leader_whose_lease_was_taken_stands_down() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let config = fast();
        let (_stop, shutdown) = watch::channel(false);
        let (a, _h) = elect(store.clone(), CELL, "a", config, shutdown);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(*a.borrow());

        // Somebody else writes the record — the shape a takeover has.
        let entry = store.get(&lease_key(CELL)).await.unwrap().unwrap();
        let usurper = serde_json::to_vec(&Lease {
            holder: "b".into(),
            renewed_at: Timestamp::now(),
        })
        .unwrap();
        store
            .put(&lease_key(CELL), usurper, Expect::Revision(entry.revision))
            .await
            .unwrap();

        for _ in 0..40 {
            if !*a.borrow() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !*a.borrow(),
            "the old leader still believes it leads after its lease was taken"
        );
    }
}
