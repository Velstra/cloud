//! Taking a snapshot every so often, and letting the old ones go.
//!
//! The cheap half of the pair. A snapshot lives in the volume's own pool: it
//! costs almost nothing, it is taken in a moment, and it is the right tool for
//! "let me be able to go back an hour". It is **not** a backup — lose the pool
//! and it goes with it — which is why [`crate::backup_schedule`] exists beside
//! this one and why they are two schedules rather than one with a flag.
//!
//! ## Why the arithmetic is borrowed rather than copied
//!
//! `backup::due` and `backup::prune` take an interval and a count, not a
//! backup. So this shares the three rules that were hard to get right —
//!
//! * a copy still being made holds the schedule, so a slow one is not asked
//!   for again every pass;
//! * only **finished** copies count toward `keep`, so a run of failures never
//!   expires the last one that worked;
//! * at least one always survives —
//!
//! rather than growing a second implementation of them that drifts. A rule
//! written twice is a rule that will eventually be two rules.
//!
//! ## Where snapshots live
//!
//! Under the volume: `projects/p1/volumes/data-1/snapshots/<id>`. The API
//! settles the pool from the volume, exactly as it does for one taken by hand.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    backup::{self, BackupView},
    meta::{Meta, ResourceName, Timestamp},
    resources::{Resource, Snapshot, SnapshotSpec, SnapshotStatus},
    storage::{SnapshotScheduleSpec, SnapshotScheduleStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "snapshot-schedule";

pub struct SnapshotScheduleController {
    snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
    volumes: TypedStore<
        velstra_cloud_model::resources::VolumeSpec,
        velstra_cloud_model::resources::VolumeStatus,
    >,
    now: std::sync::Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl SnapshotScheduleController {
    pub fn new(
        snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
        volumes: TypedStore<
            velstra_cloud_model::resources::VolumeSpec,
            velstra_cloud_model::resources::VolumeStatus,
        >,
    ) -> Self {
        Self {
            snapshots,
            volumes,
            now: std::sync::Arc::new(Timestamp::now),
        }
    }

    pub fn with_clock(mut self, now: impl Fn() -> Timestamp + Send + Sync + 'static) -> Self {
        self.now = std::sync::Arc::new(now);
        self
    }
}

/// This schedule's snapshots, seen the way the shared arithmetic sees a copy.
///
/// The `schedule` field is the id carried in the snapshot's own name rather
/// than a field on it, because a snapshot has no place to record who asked for
/// one — and inventing a field for it would mean touching the object a pool
/// agent owns. The name is a fact nobody else writes.
fn view(s: &Snapshot, schedule_id: &str) -> BackupView {
    let id = s.meta.name.id().to_string();
    BackupView {
        schedule: id
            .starts_with(&format!("{schedule_id}-"))
            .then(|| schedule_id.to_string()),
        name: s.meta.name.to_string(),
        taken: s.status.taken,
        created_at: s.meta.created_at,
        // A snapshot records no moment of its own; the object's creation is
        // when it was asked for, and a pool takes one in an instant.
        taken_at: None,
        deleting: s.meta.is_deleting(),
    }
}

impl Reconciler for SnapshotScheduleController {
    type Spec = SnapshotScheduleSpec;
    type Status = SnapshotScheduleStatus;

    fn name(&self) -> &'static str {
        "snapshot-schedule"
    }

    async fn reconcile(
        &self,
        name: &str,
        object: Option<&Resource<Self::Spec, Self::Status>>,
    ) -> Result<()> {
        let Some(schedule) = object else {
            return Ok(());
        };
        // Deleting the intention is not deleting the data, and an operator who
        // meant both can say so about each.
        if schedule.meta.is_deleting() {
            return Ok(());
        }
        let Ok(volume_name) = ResourceName::parse(&schedule.spec.volume) else {
            return Ok(());
        };
        let id = schedule.meta.name.id().to_string();

        let objects = self.snapshots.list().await?;
        // This volume's, and this schedule's. A snapshot under another volume
        // is not something this schedule made or should expire.
        let mine: Vec<&Snapshot> = objects
            .iter()
            .filter(|s| s.meta.name.parent().as_ref() == Some(&volume_name))
            .collect();
        let seen: Vec<BackupView> = mine.iter().map(|s| view(s, &id)).collect();
        let now = (self.now)();

        if backup::due(schedule.spec.every_hours, &id, &seen, now) {
            // A volume that has gone is not an error and not a snapshot: there
            // is simply nothing to copy until somebody points this at
            // something that exists.
            let Some(volume) = self.volumes.get(&schedule.spec.volume).await? else {
                return Ok(());
            };
            // Derived from the schedule and the second, so two controllers on
            // one pass ask for the same object and the loser's create is
            // refused as a duplicate rather than making a second snapshot of
            // the same moment.
            let taken_id = format!("{id}-{}", now.0 / 1000);
            let full = format!("{volume_name}/snapshots/{taken_id}");
            let Ok(snapshot_name) = ResourceName::parse(&full) else {
                return Ok(());
            };
            let asked: Snapshot = Resource::new(
                Meta::new(snapshot_name, schedule.meta.placement.clone()),
                SnapshotSpec {
                    pool: volume.spec.pool.clone(),
                },
                SnapshotStatus::default(),
            );
            match self
                .snapshots
                .create(&asked, &Writer::controller(WHO))
                .await
            {
                Ok(_) => info!(schedule = %name, snapshot = %full, "asked for a snapshot"),
                Err(e) if is_taken(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }

        for expired in backup::prune(schedule.spec.keep, &id, &seen) {
            let Some(object) = mine.iter().find(|s| s.meta.name.to_string() == expired) else {
                continue;
            };
            info!(schedule = %name, snapshot = %expired, "expiring a snapshot");
            if let Err(e) = self
                .snapshots
                .delete(&expired, object.meta.revision, &Writer::controller(WHO))
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

fn is_taken(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Exists { .. }
        )
    )
}

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
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use velstra_cloud_model::{
        meta::Placement,
        resources::{Volume, VolumeSpec, VolumeStatus},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const SCHEDULE: &str = "projects/p1/snapshot-schedules/hourly";
    const VOLUME: &str = "projects/p1/volumes/data-1";
    const HOUR: u64 = 3_600_000;

    struct Fixture {
        schedules: TypedStore<SnapshotScheduleSpec, SnapshotScheduleStatus>,
        snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
        controller: SnapshotScheduleController,
        now: Arc<AtomicU64>,
        base: u64,
    }

    async fn fixture(keep: u32) -> Fixture {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let schedules: TypedStore<SnapshotScheduleSpec, SnapshotScheduleStatus> =
            TypedStore::new(store.clone(), "cell-1", "snapshot-schedules");
        let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
            TypedStore::new(store.clone(), "cell-1", "snapshots");
        let volumes: TypedStore<VolumeSpec, VolumeStatus> =
            TypedStore::new(store.clone(), "cell-1", "volumes");

        let v: Volume = Resource::new(
            Meta::new(
                ResourceName::parse(VOLUME).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            VolumeSpec {
                source_backup: None,
                size_gib: 40,
                pool: "pools/fast".into(),
                encryption_key: None,
                source_image: None,
                source_snapshot: None,
            },
            VolumeStatus::default(),
        );
        volumes
            .create(&v, &Writer::controller("test"))
            .await
            .unwrap();

        let s: Resource<SnapshotScheduleSpec, SnapshotScheduleStatus> = Resource::new(
            Meta::new(
                ResourceName::parse(SCHEDULE).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            SnapshotScheduleSpec {
                volume: VOLUME.into(),
                every_hours: 1,
                keep,
            },
            SnapshotScheduleStatus::default(),
        );
        schedules
            .create(&s, &Writer::controller("test"))
            .await
            .unwrap();

        // Anchored to real time: `due` compares against `meta.created_at`,
        // which the store stamps with its own clock, and the two have to be
        // the same kind of number.
        let base = Timestamp::now().0;
        let now = Arc::new(AtomicU64::new(base));
        let reading = now.clone();
        let controller = SnapshotScheduleController::new(snapshots.clone(), volumes)
            .with_clock(move || Timestamp(reading.load(Ordering::Relaxed)));

        Fixture {
            schedules,
            snapshots,
            controller,
            now,
            base,
        }
    }

    impl Fixture {
        fn after(&self, hours: u64) {
            self.now.store(self.base + hours * HOUR, Ordering::Relaxed);
        }

        async fn pass(&self) {
            let s = self.schedules.get(SCHEDULE).await.unwrap().unwrap();
            self.controller.reconcile(SCHEDULE, Some(&s)).await.unwrap();
        }

        async fn ids(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .snapshots
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|s| !s.meta.is_deleting())
                .map(|s| s.meta.name.id().to_string())
                .collect();
            out.sort();
            out
        }

        /// Mark one taken, as the pool agent would.
        async fn finish(&self, id: &str) {
            let full = format!("{VOLUME}/snapshots/{id}");
            let mut s = self.snapshots.get(&full).await.unwrap().unwrap();
            s.status.taken = true;
            s.status.pool = Some("pools/fast".into());
            s.status.observed_generation = s.meta.generation;
            self.snapshots
                .update(&s, &Writer::agent("pools/fast"))
                .await
                .unwrap();
        }
    }

    /// One per interval, and a second pass in the same hour asks for nothing.
    #[tokio::test]
    async fn a_schedule_takes_one_snapshot_an_hour() {
        let f = fixture(24).await;

        f.pass().await;
        assert_eq!(f.ids().await.len(), 1);
        f.pass().await;
        assert_eq!(
            f.ids().await.len(),
            1,
            "a second pass in the same hour took another snapshot"
        );

        f.after(2);
        f.pass().await;
        assert_eq!(f.ids().await.len(), 2);
    }

    /// The pool's own rules are borrowed, not re-implemented: a snapshot that
    /// never completed does not count toward `keep`.
    #[tokio::test]
    async fn unfinished_snapshots_do_not_expire_the_ones_that_worked() {
        let f = fixture(1).await;

        f.pass().await;
        let first = f.ids().await[0].clone();
        f.finish(&first).await;

        // Two more hours of attempts that never complete.
        for hour in 2..4 {
            f.after(hour);
            f.pass().await;
        }

        assert!(
            f.ids().await.contains(&first),
            "the only finished snapshot was expired by attempts that were not"
        );
    }

    /// Snapshots under another volume are none of this schedule's business.
    #[tokio::test]
    async fn a_schedule_does_not_touch_another_volumes_snapshots() {
        let f = fixture(1).await;
        let other: Snapshot = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/volumes/other/snapshots/hourly-1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            SnapshotSpec {
                pool: "pools/fast".into(),
            },
            SnapshotStatus {
                taken: true,
                ..Default::default()
            },
        );
        f.snapshots
            .create(&other, &Writer::controller("test"))
            .await
            .unwrap();

        f.pass().await;
        assert!(
            f.ids().await.contains(&"hourly-1".to_string()),
            "a snapshot under another volume was expired"
        );
    }

    /// A deleted schedule stops asking and takes nothing with it.
    #[tokio::test]
    async fn a_deleted_schedule_leaves_its_snapshots_alone() {
        let f = fixture(1).await;
        f.pass().await;
        let before = f.ids().await;

        let mut s = f.schedules.get(SCHEDULE).await.unwrap().unwrap();
        s.meta.deleted_at = Some(Timestamp(f.base + HOUR));
        f.schedules
            .update(&s, &Writer::controller("test"))
            .await
            .unwrap();

        f.after(48);
        f.controller.reconcile(SCHEDULE, Some(&s)).await.unwrap();

        assert_eq!(
            f.ids().await,
            before,
            "deleting the intention deleted the data, or asked for more of it"
        );
    }
}
