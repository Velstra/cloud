//! The controller half of a volume's life.
//!
//! Two guards, and their order is the safety property in both directions: the
//! pool's guard goes on before any backing store can exist and comes off only
//! once the pool says the bytes are gone, and the snapshots' guard goes on
//! before the first copy can be made and comes off only once the last one is.
//!
//! What the pool's guard prevents is the thing that quietly costs money: an
//! object deleted from the API while a pool still holds the gigabytes. Nobody is
//! billed for them and nobody can find them — the record that named them is
//! gone. A `spec` field saying "deleted" cannot express "asked to destroy, has
//! not yet"; a finalizer can.
//!
//! The pool cannot drop the finalizer itself, because `meta` belongs to a
//! controller. So it publishes a `Released` condition and this reads it. That
//! indirection is deliberate: the alternative is a controller inferring release
//! from `provisioned == false`, which would be a second definition of "let go"
//! living somewhere else — and the two would disagree the first time a volume
//! failed to provision in the first place.
//!
//! What the snapshots' guard prevents is worse than money: a snapshot is a delta
//! against the volume it was taken from, so a source that goes first takes its
//! copies with it, at the moment somebody deletes something they believe they
//! have backups of. The pool refuses the destroy on its own — it can see the
//! copies — and this keeps the *record* alive to say so, which is what turns
//! silent loss into a delete that is visibly waiting for a second one.
//!
//! Why the volume holds it rather than the snapshot controller placing it: it is
//! recomputed from the snapshots that exist, on every pass, by the loop that
//! already visits every volume. Nothing is remembered between passes, so nothing
//! is lost by a restart — and a guard that outlived the copies it was for would
//! leave a volume nobody can delete without editing the store.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, ResourceName, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{
        POOL_RELEASE_FINALIZER, SNAPSHOT_SOURCE_FINALIZER, SnapshotSpec, SnapshotStatus, Volume,
        VolumeSpec, VolumeStatus,
    },
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{Related, Result, runner::Reconciler};

const WHO: &str = "volume";

pub struct VolumeController {
    volumes: TypedStore<VolumeSpec, VolumeStatus>,
    snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
    cell: String,
}

impl VolumeController {
    pub fn new(
        volumes: TypedStore<VolumeSpec, VolumeStatus>,
        snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
        cell: &str,
    ) -> Self {
        Self {
            volumes,
            snapshots,
            cell: cell.to_string(),
        }
    }

    /// Whether any snapshot has been taken from this volume.
    ///
    /// One that is itself being deleted still counts, for the same reason a
    /// dying instance still counts against quota: it exists until its own
    /// finalizers are released, and it is still read through this volume until
    /// the pool has destroyed it.
    async fn has_copies(&self, volume: &Volume) -> Result<bool> {
        Ok(self
            .snapshots
            .list()
            .await?
            .iter()
            .any(|s| s.meta.name.is_under(&volume.meta.name)))
    }

    /// Put the snapshots' guard on, or take it off. One write, and only when
    /// the answer has actually changed.
    ///
    /// It is never *added* to a volume already being deleted: nobody would ever
    /// be asked to release it, and the object would be undeletable without
    /// somebody editing the store. A volume that was already guarded when the
    /// delete arrived keeps the guard, which is the whole point.
    async fn copies_guard(&self, volume: &Volume) -> Result<bool> {
        let held = volume.meta.has_finalizer(SNAPSHOT_SOURCE_FINALIZER);
        let copies = self.has_copies(volume).await?;
        let mut next = volume.clone();
        match (copies, held) {
            (true, false) if !volume.meta.is_deleting() => {
                next.meta.add_finalizer(SNAPSHOT_SOURCE_FINALIZER)
            }
            (false, true) => next.meta.remove_finalizer(SNAPSHOT_SOURCE_FINALIZER),
            _ => return Ok(false),
        }
        self.volumes.update(&next, &Writer::controller(WHO)).await?;
        Ok(true)
    }
}

/// Whether the pool has said it holds nothing of this volume any more.
///
/// Read from the condition the pool writes, never inferred. A volume that was
/// never provisioned has `provisioned == false` too, and treating that as "the
/// pool has let go" would delete the object while a half-made backing store sat
/// there — the one case where guessing is worst.
fn pool_has_let_go(volume: &Volume) -> bool {
    condition(&volume.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
}

impl Reconciler for VolumeController {
    type Spec = VolumeSpec;
    type Status = VolumeStatus;

    fn name(&self) -> &'static str {
        "volume"
    }

    fn related(&self) -> Vec<Related> {
        // A snapshot appearing or going is what changes the answer to "may this
        // volume be deleted", and it is a write to a *different* object. The
        // mapping is a pure function of the name because a snapshot's source is
        // its parent — which is also why it still works for the event that
        // matters most, the disappearance of the last copy.
        vec![Related::named(
            prefix_for(&self.cell, "snapshots"),
            |snapshot: &str| {
                ResourceName::parse(snapshot)
                    .ok()
                    .and_then(|s| s.parent())
                    .filter(|parent| parent.collection() == "volumes")
                    .map(|parent| parent.to_string())
                    .into_iter()
                    .collect()
            },
        )]
    }

    async fn reconcile(&self, name: &str, object: Option<&Volume>) -> Result<()> {
        let Some(volume) = object else {
            return Ok(());
        };

        // Before the pool's guard, so that a volume which gains its first copy
        // and its delete in the same instant is guarded by the time anything
        // else looks at it.
        if self.copies_guard(volume).await? {
            return Ok(());
        }

        match finalizer_step(&volume.meta, POOL_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                // Before anything can be asked to create bytes. A finalizer
                // added after the pool has already provisioned would leave a
                // window in which a delete takes the record and leaves the
                // storage.
                let mut next = volume.clone();
                next.meta.add_finalizer(POOL_RELEASE_FINALIZER);
                self.volumes.update(&next, &Writer::controller(WHO)).await?;
                Ok(())
            }
            FinalizerStep::Wait => {
                // Deleting, and still guarded. The only question left is whether
                // the pool has let go; until it says so, nothing happens — which
                // is what keeps the object visible while somebody investigates a
                // backend that will not destroy it.
                if !volume.meta.is_deleting() || !pool_has_let_go(volume) {
                    return Ok(());
                }
                let mut next = volume.clone();
                next.meta.remove_finalizer(POOL_RELEASE_FINALIZER);
                self.volumes.update(&next, &Writer::controller(WHO)).await?;
                info!(volume = name, "the pool let go; the guard is off");
                Ok(())
            }
            FinalizerStep::Delete => {
                // Conditional on the revision, so a volume that gained a
                // finalizer between the read and now survives instead of being
                // torn out from under whoever added it.
                self.volumes.delete(name, volume.meta.revision).await?;
                info!(volume = name, "gone");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Condition, Meta, Placement, Timestamp, set_condition},
        resources::{Resource, Snapshot},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const NAME: &str = "projects/p1/volumes/data-1";
    const COPY: &str = "projects/p1/volumes/data-1/snapshots/nightly";

    struct Cell {
        volumes: TypedStore<VolumeSpec, VolumeStatus>,
        snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
    }

    async fn fixture() -> (Cell, VolumeController) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let cell = Cell {
            volumes: TypedStore::new(store.clone(), "cell-1", "volumes"),
            snapshots: TypedStore::new(store, "cell-1", "snapshots"),
        };
        let v: Volume = Resource::new(
            Meta::new(
                ResourceName::parse(NAME).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            VolumeSpec {
                size_gib: 100,
                pool: "pool-a".into(),
                encryption_key: None,
                source_image: None,
                source_snapshot: None,
            },
            VolumeStatus::default(),
        );
        cell.volumes.create(&v).await.unwrap();
        let controller =
            VolumeController::new(cell.volumes.clone(), cell.snapshots.clone(), "cell-1");
        (cell, controller)
    }

    impl Cell {
        /// A copy of `data-1`, under it, as every stored snapshot is.
        async fn snapshot(&self) {
            let s: Snapshot = Resource::new(
                Meta::new(
                    ResourceName::parse(COPY).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                SnapshotSpec {
                    pool: "pool-a".into(),
                },
                SnapshotStatus::default(),
            );
            self.snapshots.create(&s).await.unwrap();
        }

        async fn drop_snapshot(&self) {
            let s = self.snapshots.get(COPY).await.unwrap().unwrap();
            self.snapshots.delete(COPY, s.meta.revision).await.unwrap();
        }

        /// Run the controller once over the volume as it now stands.
        async fn pass(&self, controller: &VolumeController) {
            let v = reload(&self.volumes).await.unwrap();
            controller.reconcile(NAME, Some(&v)).await.unwrap();
        }

        async fn deleting(&self) {
            let mut v = reload(&self.volumes).await.unwrap();
            v.meta.deleted_at = Some(Timestamp::now());
            self.volumes
                .update(&v, &Writer::controller("api"))
                .await
                .unwrap();
        }
    }

    async fn reload(volumes: &TypedStore<VolumeSpec, VolumeStatus>) -> Option<Volume> {
        volumes.get(NAME).await.unwrap()
    }

    async fn say_released(volumes: &TypedStore<VolumeSpec, VolumeStatus>, released: bool) {
        let mut v = reload(volumes).await.unwrap();
        v.status.pool = Some("pool-a".into());
        set_condition(
            &mut v.status.conditions,
            Condition::new(
                "Released",
                if released {
                    ConditionStatus::True
                } else {
                    ConditionStatus::False
                },
                if released { "Released" } else { "Destroying" },
                "",
                v.meta.generation,
            ),
        );
        volumes.update(&v, &Writer::agent("pool-a")).await.unwrap();
    }

    #[tokio::test]
    async fn the_guard_goes_on_before_any_bytes_exist() {
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        assert!(
            reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER)
        );
    }

    #[tokio::test]
    async fn a_volume_whose_bytes_are_still_there_does_not_go() {
        // The failure: the record disappears, and a pool is left holding
        // gigabytes nobody is billed for and nobody can find.
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        say_released(&cell.volumes, false).await;
        cell.deleting().await;

        cell.pass(&controller).await;
        assert!(
            reload(&cell.volumes).await.is_some(),
            "the object went while the pool still held it"
        );
        assert!(
            reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER),
            "the guard came off before the pool said it had let go"
        );
    }

    #[tokio::test]
    async fn a_volume_the_pool_has_let_go_of_is_deleted() {
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        cell.deleting().await;
        say_released(&cell.volumes, true).await;

        // One pass takes the guard off…
        cell.pass(&controller).await;
        assert!(
            !reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER)
        );
        // …and the next takes the object.
        cell.pass(&controller).await;
        assert!(reload(&cell.volumes).await.is_none());
    }

    #[tokio::test]
    async fn a_volume_that_was_never_provisioned_is_not_mistaken_for_a_released_one() {
        // `provisioned == false` is true of a volume nobody has made yet and of
        // one whose bytes are gone. Inferring release from it would delete the
        // record of a half-made backing store, which is the one case where
        // guessing costs the most.
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        cell.deleting().await;

        // No `Released` condition at all — the pool has never reported.
        assert!(!reload(&cell.volumes).await.unwrap().status.provisioned);
        cell.pass(&controller).await;
        assert!(
            reload(&cell.volumes).await.is_some(),
            "a volume nobody has reported on was deleted on the strength of a default"
        );
    }

    #[tokio::test]
    async fn a_settled_volume_costs_nothing_to_look_at_again() {
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;

        let revision = reload(&cell.volumes).await.unwrap().meta.revision;
        for _ in 0..2 {
            cell.pass(&controller).await;
        }
        assert_eq!(
            reload(&cell.volumes).await.unwrap().meta.revision,
            revision,
            "a settled volume was written to again"
        );
    }

    // ---- the guard the copies hold ---------------------------------------

    #[tokio::test]
    async fn a_volume_that_has_been_copied_is_guarded_by_its_copies() {
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        assert!(
            !reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(SNAPSHOT_SOURCE_FINALIZER),
            "a volume nobody has copied was guarded against a danger it is not in"
        );

        cell.snapshot().await;
        cell.pass(&controller).await;
        assert!(
            reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(SNAPSHOT_SOURCE_FINALIZER)
        );

        // …and it comes off by itself once the last copy is gone, from the
        // same recomputation, so nothing has to be remembered between passes.
        cell.drop_snapshot().await;
        cell.pass(&controller).await;
        assert!(
            !reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(SNAPSHOT_SOURCE_FINALIZER)
        );
    }

    #[tokio::test]
    async fn deleting_a_volume_that_has_copies_waits_for_them() {
        // The case that motivates the whole guard. An operator deletes a
        // volume they believe they have backups of; on every backend in sight
        // the copies are deltas read through it, so if the record went and the
        // pool destroyed it, the backups would go too — silently, at the
        // moment they were most wanted.
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        cell.snapshot().await;
        cell.pass(&controller).await;

        cell.deleting().await;
        // The pool reports it has let go — which it can, because a pool that
        // cannot destroy a volume with copies still says truthfully that it is
        // holding nothing *for this object*. The copies' guard is what keeps
        // the record here anyway.
        say_released(&cell.volumes, true).await;

        for pass in 1..=3 {
            cell.pass(&controller).await;
            let still = reload(&cell.volumes).await;
            assert!(
                still.is_some(),
                "pass {pass}: the source of a snapshot was deleted out from under it"
            );
            assert!(
                still.unwrap().meta.has_finalizer(SNAPSHOT_SOURCE_FINALIZER),
                "pass {pass}: the copies' guard came off while a copy still existed"
            );
        }

        // Delete the copy and the volume goes on its own, with nothing asked
        // for a second time.
        cell.drop_snapshot().await;
        cell.pass(&controller).await;
        cell.pass(&controller).await;
        assert!(
            reload(&cell.volumes).await.is_none(),
            "the volume stayed behind after its last copy went"
        );
    }

    #[tokio::test]
    async fn a_copy_taken_of_a_volume_already_going_never_pins_it() {
        // A guard added to something already on its way out is one nobody
        // would ever be asked to release: the object would be undeletable
        // without somebody editing the store. The API refuses to take a copy
        // of a deleting volume; this is the second half of that, for a copy
        // that was created in the same instant as the delete.
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;
        cell.deleting().await;
        cell.snapshot().await;

        cell.pass(&controller).await;
        assert!(
            !reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(SNAPSHOT_SOURCE_FINALIZER)
        );
    }

    #[tokio::test]
    async fn a_copy_of_another_volume_is_not_this_volumes_business() {
        // Containment, not a string prefix: `data-1` must not be guarded by a
        // snapshot of `data-10`.
        let (cell, controller) = fixture().await;
        cell.pass(&controller).await;

        let other: Snapshot = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/volumes/data-10/snapshots/nightly").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            SnapshotSpec {
                pool: "pool-a".into(),
            },
            SnapshotStatus::default(),
        );
        cell.snapshots.create(&other).await.unwrap();

        cell.pass(&controller).await;
        assert!(
            !reload(&cell.volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(SNAPSHOT_SOURCE_FINALIZER)
        );
    }
}
