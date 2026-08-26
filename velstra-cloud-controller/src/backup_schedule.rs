//! Making backups happen, and letting the expired ones go.
//!
//! A schedule is an intention — "there should be a copy of this volume on that
//! target, no older than N hours, and keep the last K" — and this is the loop
//! that turns it into objects. It decides nothing itself: [`backup::due`] and
//! [`backup::prune`] are pure functions over what exists, and this reads the
//! world, calls them, and performs what comes back.
//!
//! ## Nothing is ever "in progress"
//!
//! A copy that is still being made is an ordinary `Backup` object with
//! `taken == false`. `due` counts it, so a volume that takes an hour to copy
//! does not have a second copy asked for every few seconds — and a copy that is
//! *stuck* holds the schedule for one interval and no longer, after which
//! another is made. That is the whole recovery model: no state to resume, no
//! memory to lose, and a controller that dies mid-pass computes the same list
//! next time minus what already happened.
//!
//! ## What this does not do
//!
//! It does not copy anything. The pool holding the source reads the bytes and
//! writes them to the target, and reports what it did — the same division as
//! every other agent in this platform. This controller only ever creates and
//! deletes objects.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    backup::{self, BackupScheduleSpec, BackupScheduleStatus, BackupSpec, BackupStatus, BackupView},
    meta::{Meta, ResourceName, Timestamp},
    resources::{Backup, BackupSchedule, Resource},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "backup-schedule";

pub struct BackupScheduleController {
    backups: TypedStore<BackupSpec, BackupStatus>,
    /// Read to find which pool holds the source.
    ///
    /// Not optional, and not something the schedule carries. A backup whose
    /// `pool` is empty is assigned to nobody, and the access rule refuses
    /// every agent that tries to claim it — so it would sit there forever,
    /// unmade, with the schedule believing a copy was in flight. The rule
    /// caught that; this is the fix.
    volumes: TypedStore<
        velstra_cloud_model::resources::VolumeSpec,
        velstra_cloud_model::resources::VolumeStatus,
    >,
    /// Where "now" comes from.
    ///
    /// A field rather than a call to the clock, because a schedule is the one
    /// decision in this platform that genuinely depends on the time — and a
    /// check that had to wait twenty-four hours to prove retention works is a
    /// check nobody runs.
    ///
    /// A closure rather than a function pointer, so each caller can own its
    /// own clock. A pointer forces a shared static, and a shared static
    /// between checks that run on different threads at the same time is a
    /// flake waiting for a slow day.
    now: std::sync::Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl BackupScheduleController {
    pub fn new(
        backups: TypedStore<BackupSpec, BackupStatus>,
        volumes: TypedStore<
            velstra_cloud_model::resources::VolumeSpec,
            velstra_cloud_model::resources::VolumeStatus,
        >,
    ) -> Self {
        Self {
            backups,
            volumes,
            now: std::sync::Arc::new(Timestamp::now),
        }
    }

    /// Drive this controller from a clock the caller owns.
    pub fn with_clock(
        mut self,
        now: impl Fn() -> Timestamp + Send + Sync + 'static,
    ) -> Self {
        self.now = std::sync::Arc::new(now);
        self
    }

    /// One backup, seen the way the schedule logic sees it.
    fn view(b: &Backup) -> BackupView {
        BackupView {
            name: b.meta.name.to_string(),
            schedule: b.spec.schedule.clone(),
            taken: b.status.taken,
            created_at: b.meta.created_at,
            taken_at: b.status.taken_at,
            deleting: b.meta.is_deleting(),
        }
    }
}

/// The name for the copy a schedule is asking for now.
///
/// Derived from the schedule and the moment, so that two controllers racing on
/// the same pass ask for the *same* object — and the second one's create is
/// refused as a duplicate instead of producing a second copy of the same
/// volume at the same second.
///
/// Seconds rather than milliseconds: a name is read by people, and a schedule
/// cannot be due twice within one second.
fn backup_name(schedule: &ResourceName, at: Timestamp) -> String {
    format!("{}-{}", schedule.id(), at.0 / 1000)
}

impl Reconciler for BackupScheduleController {
    type Spec = BackupScheduleSpec;
    type Status = BackupScheduleStatus;

    fn name(&self) -> &'static str {
        "backup-schedule"
    }

    async fn reconcile(&self, name: &str, object: Option<&BackupSchedule>) -> Result<()> {
        let Some(schedule) = object else {
            return Ok(());
        };
        // A schedule on its way out asks for nothing more. Its copies stay:
        // deleting the intention is not deleting the data, and an operator who
        // meant both can say so about each.
        if schedule.meta.is_deleting() {
            return Ok(());
        }

        let now = (self.now)();
        // Listed once and kept: the views drive the decision, and the objects
        // carry the revisions a delete needs. Two lists would be two pictures
        // of a world that moved in between.
        let objects = self.backups.list().await?;
        let all: Vec<BackupView> = objects.iter().map(Self::view).collect();
        let writer = Writer::controller(WHO);

        if backup::due(schedule.spec.every_hours, name, &all, now) {
            // Which pool holds the source. Read here rather than carried on
            // the schedule, because a volume can be moved and a schedule that
            // remembered the old pool would keep asking the wrong agent.
            //
            // A volume that has gone is not an error and not a copy: the
            // schedule simply has nothing to copy until somebody points it at
            // something that exists.
            let pool = match self.volumes.get(&schedule.spec.volume).await? {
                Some(v) if !v.spec.pool.is_empty() => v.spec.pool,
                _ => return Ok(()),
            };
            let id = backup_name(&schedule.meta.name, now);
            // A sibling of the schedule: same project, different collection.
            // A schedule with no parent is not a shape this platform makes, so
            // there is nothing sensible to do with one but leave it alone.
            let Some(project) = schedule.meta.name.parent() else {
                return Ok(());
            };
            let full = format!("{project}/backups/{id}");
            let Ok(asked_name) = ResourceName::parse(&full) else {
                // Unreachable in practice: both halves came from names the
                // store already accepted. Refusing quietly beats a panic in a
                // loop that runs forever.
                return Ok(());
            };
            let asked = Resource::new(
                Meta::new(asked_name, schedule.meta.placement.clone()),
                BackupSpec {
                    volume: schedule.spec.volume.clone(),
                    target: schedule.spec.target.clone(),
                    pool,
                    schedule: Some(name.to_string()),
                },
                BackupStatus::default(),
            );
            match self.backups.create(&asked, &writer).await {
                Ok(_) => info!(schedule = %name, backup = %full, "asked for a backup"),
                // Somebody else got there first on the same second, which is
                // what the derived name is for. Not an error: the copy this
                // pass wanted exists.
                Err(e) if is_taken(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }

        for expired in backup::prune(schedule.spec.keep, name, &all) {
            let Some(object) = objects.iter().find(|b| b.meta.name.to_string() == expired) else {
                continue;
            };
            info!(schedule = %name, backup = %expired, "expiring a backup");
            // Deleting the object; the pool releases the bytes and drops its
            // finalizer, the same as a snapshot. A backup whose bytes are gone
            // but whose object remains would be a copy somebody counts on and
            // cannot restore from.
            //
            // At the revision this pass read. A copy that changed underneath —
            // one that has just finished, say — is left for the next pass to
            // look at again rather than deleted on a stale picture.
            if let Err(e) = self
                .backups
                .delete(&expired, object.meta.revision, &writer)
                .await
            {
                if !is_gone(&e) {
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

/// Whether a create failed because the name is already taken.
fn is_taken(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Exists { .. }
        )
    )
}

/// Whether a delete failed because there was nothing there.
///
/// Not an error: two passes agreeing that a copy has expired is the ordinary
/// outcome of a level-triggered loop, and the second one finding it already
/// gone is the system working.
fn is_gone(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Missing { .. }
        )
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const SCHEDULE: &str = "projects/p1/backup-schedules/nightly";
    const HOUR: u64 = 3_600_000;

    struct Fixture {
        schedules: TypedStore<BackupScheduleSpec, BackupScheduleStatus>,
        backups: TypedStore<BackupSpec, BackupStatus>,
        controller: BackupScheduleController,
        /// This check's own clock. One per fixture, so checks that run at the
        /// same time on different threads cannot move each other's time.
        now: Arc<AtomicU64>,
        /// Where that clock starts.
        ///
        /// Real wall-clock time, deliberately. `due` compares against
        /// `meta.created_at`, which the **store** stamps with its own clock —
        /// so a fake clock starting at hour zero puts every object a
        /// half-century in the future and nothing is ever due. The controller's
        /// clock and the store's have to be the same kind of number.
        base: u64,
    }

    async fn fixture(keep: u32) -> Fixture {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let schedules: TypedStore<BackupScheduleSpec, BackupScheduleStatus> =
            TypedStore::new(store.clone(), "cell-1", "backup-schedules");
        let backups: TypedStore<BackupSpec, BackupStatus> =
            TypedStore::new(store.clone(), "cell-1", "backups");
        let volumes: TypedStore<
            velstra_cloud_model::resources::VolumeSpec,
            velstra_cloud_model::resources::VolumeStatus,
        > = TypedStore::new(store, "cell-1", "volumes");

        let v: velstra_cloud_model::resources::Volume = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/volumes/data-1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            velstra_cloud_model::resources::VolumeSpec {
                source_backup: None,
                size_gib: 40,
                pool: "pools/fast".into(),
                encryption_key: None,
                source_image: None,
                source_snapshot: None,
            },
            velstra_cloud_model::resources::VolumeStatus::default(),
        );
        volumes.create(&v, &Writer::controller("test")).await.unwrap();

        let s: BackupSchedule = Resource::new(
            Meta::new(
                ResourceName::parse(SCHEDULE).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            BackupScheduleSpec {
                volume: "projects/p1/volumes/data-1".into(),
                target: "backup-targets/nightly".into(),
                every_hours: 24,
                keep,
            },
            BackupScheduleStatus::default(),
        );
        schedules
            .create(&s, &Writer::controller("test"))
            .await
            .unwrap();

        let base = Timestamp::now().0;
        let now = Arc::new(AtomicU64::new(base));
        let reading = now.clone();
        let controller = BackupScheduleController::new(backups.clone(), volumes)
                .with_clock(move || Timestamp(reading.load(Ordering::Relaxed)));
        Fixture {
            schedules,
            backups,
            controller,
            now,
            base,
        }
    }

    impl Fixture {
        async fn pass(&self) {
            let s = self.schedules.get(SCHEDULE).await.unwrap().unwrap();
            self.controller.reconcile(SCHEDULE, Some(&s)).await.unwrap();
        }

        async fn names(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .backups
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|b| !b.meta.is_deleting())
                .map(|b| b.meta.name.id().to_string())
                .collect();
            out.sort();
            out
        }

        /// Move this check's clock to `hours` after the fixture was built.
        fn after(&self, hours: u64) {
            self.now.store(self.base + hours * HOUR, Ordering::Relaxed);
        }

        /// That moment, as the copy names are derived from it.
        fn stamp(&self, hours: u64) -> u64 {
            (self.base + hours * HOUR) / 1000
        }

        /// Mark a copy finished, as the pool agent would.
        ///
        /// Written as the *pool*, not as a controller. The access rule refuses
        /// a controller writing somebody else's status, which is the rule
        /// working: only the party that did the thing may report it.
        async fn finish(&self, id: &str, at: u64) {
            let full = format!("projects/p1/backups/{id}");
            let mut b = self.backups.get(&full).await.unwrap().unwrap();
            b.status.taken = true;
            b.status.taken_at = Some(Timestamp(at));
            b.status.agent = Some("pools/fast".into());
            b.status.observed_generation = b.meta.generation;
            self.backups
                .update(&b, &Writer::agent("pools/fast"))
                .await
                .unwrap();
        }
    }

    /// Nothing yet: one copy is asked for, and asking again changes nothing.
    ///
    /// The second half is the one that matters. A level-triggered loop runs
    /// again in a second, and a controller that asked for a copy every pass
    /// would fill a target with copies of the same moment.
    #[tokio::test]
    async fn a_schedule_asks_for_one_copy_and_a_second_pass_asks_for_nothing() {
        let f = fixture(3).await;
        f.after(0);

        f.pass().await;
        let after_one = f.names().await;
        assert_eq!(after_one.len(), 1, "{after_one:?}");

        f.pass().await;
        assert_eq!(
            f.names().await,
            after_one,
            "a second pass asked for another copy of the same moment"
        );
    }

    /// A copy still being made holds the schedule; time moving on releases it.
    #[tokio::test]
    async fn a_copy_in_flight_holds_the_schedule_and_a_day_later_another_is_asked_for() {
        let f = fixture(3).await;
        f.after(0);
        f.pass().await;
        assert_eq!(f.names().await.len(), 1);

        // Twelve hours on, with the first still unfinished: nothing new.
        f.after(12);
        f.pass().await;
        assert_eq!(
            f.names().await.len(),
            1,
            "a copy was asked for while one was still being made"
        );

        // A day past the first: the schedule is due again even though the
        // first never finished, which is what stops a stuck copy from
        // stopping backups forever.
        f.after(25);
        f.pass().await;
        assert_eq!(f.names().await.len(), 2, "a stuck copy blocked the schedule");
    }

    /// Retention expires the oldest finished copies and never the newest.
    #[tokio::test]
    async fn retention_expires_the_oldest_and_leaves_what_was_asked_for() {
        let f = fixture(2).await;

        // Four days, one copy each, each finished as the pool would.
        for day in 0..4u64 {
            f.after(day * 24);
            f.pass().await;
            let id = format!("nightly-{}", f.stamp(day * 24));
            f.finish(&id, f.base + day * 24 * HOUR).await;
        }

        // One more pass, and this is not a detail. Expiry happens on the pass
        // *after* a copy finishes: the pass that asked for it saw it as
        // unfinished, and an unfinished copy does not count toward `keep`. So
        // the newest copy always earns its place before anything is expired
        // for it — which is the safe order, and the reason a run of failures
        // never deletes the last copy that worked.
        f.pass().await;

        let left = f.names().await;
        assert_eq!(left.len(), 2, "keep 2 left {left:?}");
        // The two newest, by the moment they were taken.
        assert!(left.iter().all(|n| n.starts_with("nightly-")), "{left:?}");
        let newest = format!("nightly-{}", f.stamp(3 * 24));
        assert!(left.contains(&newest), "the newest copy was expired: {left:?}");
    }

    /// A run of failures does not expire the last copy that worked.
    ///
    /// The trap: count attempts instead of copies, and a week of failures
    /// deletes the only thing anybody can restore from — in exactly the week
    /// somebody needs it.
    #[tokio::test]
    async fn failures_do_not_expire_the_last_copy_that_worked() {
        let f = fixture(1).await;

        f.after(0);
        f.pass().await;
        f.finish(&format!("nightly-{}", f.stamp(0)), f.base).await;

        // Three days of attempts that never finish.
        for day in 1..4u64 {
            f.after(day * 24);
            f.pass().await;
        }

        let left = f.names().await;
        assert!(
            left.contains(&format!("nightly-{}", f.stamp(0))),
            "the only finished copy was expired by failed attempts: {left:?}"
        );
    }

    /// A schedule on its way out asks for nothing, and takes nothing with it.
    #[tokio::test]
    async fn a_deleted_schedule_stops_asking_and_leaves_its_copies_alone() {
        let f = fixture(1).await;
        f.after(0);
        f.pass().await;
        f.finish(&format!("nightly-{}", f.stamp(0)), f.base).await;
        let before = f.names().await;

        let mut s = f.schedules.get(SCHEDULE).await.unwrap().unwrap();
        s.meta.deleted_at = Some(Timestamp(f.base + HOUR));
        f.schedules
            .update(&s, &Writer::controller("test"))
            .await
            .unwrap();

        f.after(100);
        f.controller.reconcile(SCHEDULE, Some(&s)).await.unwrap();

        assert_eq!(
            f.names().await,
            before,
            "deleting the intention deleted the data, or asked for more of it"
        );
    }
}
