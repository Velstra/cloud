//! The controller half of a snapshot's life.
//!
//! One guard, and it is the volume controller's guard again with a different
//! noun: the pool releases the copy's bytes before the object may go, and this
//! reads the fact from the condition the pool publishes rather than inferring it
//! — because `taken == false` is true of a copy nobody has made yet as well as
//! of one whose bytes are gone, and the two must not be confused at the moment
//! an object is being removed.
//!
//! What is deliberately **not** here is anything about the source volume. The
//! guard a snapshot puts on the volume it came from is recomputed by the volume
//! controller from the snapshots that exist, on the loop that already visits
//! every volume. Putting it here instead was the first design, and it was wrong
//! in one specific way worth recording: the moment that matters is the *last*
//! copy going away, and after that there is no snapshot object left for anything
//! to wake this controller about. It would have needed a memory of which volume
//! each snapshot belonged to, and a memory that does not survive a restart is a
//! volume nobody can delete without editing the store.
//!
//! So this controller writes exactly one object — the snapshot — and only its
//! `meta`. A settled snapshot costs one read and no writes at all.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{POOL_RELEASE_FINALIZER, Snapshot, SnapshotSpec, SnapshotStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "snapshot";

pub struct SnapshotController {
    snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
}

impl SnapshotController {
    pub fn new(snapshots: TypedStore<SnapshotSpec, SnapshotStatus>) -> Self {
        Self { snapshots }
    }
}

/// Whether the pool has said it holds nothing of this copy any more.
fn pool_has_let_go(snapshot: &Snapshot) -> bool {
    condition(&snapshot.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
}

impl Reconciler for SnapshotController {
    type Spec = SnapshotSpec;
    type Status = SnapshotStatus;

    fn name(&self) -> &'static str {
        "snapshot"
    }

    async fn reconcile(&self, name: &str, object: Option<&Snapshot>) -> Result<()> {
        let Some(snapshot) = object else {
            return Ok(());
        };

        match finalizer_step(&snapshot.meta, POOL_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                // Before the pool can be asked to make the copy. Added
                // afterwards, there would be a window in which a delete takes
                // the record and leaves the bytes.
                let mut next = snapshot.clone();
                next.meta.add_finalizer(POOL_RELEASE_FINALIZER);
                self.snapshots
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                Ok(())
            }
            FinalizerStep::Wait => {
                if !snapshot.meta.is_deleting() || !pool_has_let_go(snapshot) {
                    return Ok(());
                }
                let mut next = snapshot.clone();
                next.meta.remove_finalizer(POOL_RELEASE_FINALIZER);
                self.snapshots
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(snapshot = name, "the pool let go; the guard is off");
                Ok(())
            }
            FinalizerStep::Delete => {
                // Conditional on the revision, so a snapshot that gained a
                // finalizer between the read and now survives instead of being
                // torn out from under whoever added it.
                self.snapshots
                    .delete(
                        name,
                        snapshot.meta.revision,
                        &velstra_cloud_model::access::Writer::controller("snapshot"),
                    )
                    .await?;
                info!(snapshot = name, "gone");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const NAME: &str = "projects/p1/volumes/data-1/snapshots/nightly";

    async fn fixture() -> (TypedStore<SnapshotSpec, SnapshotStatus>, SnapshotController) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
            TypedStore::new(store, "cell-1", "snapshots");
        let s: Snapshot = Resource::new(
            Meta::new(
                ResourceName::parse(NAME).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            SnapshotSpec {
                pool: "pool-a".into(),
            },
            SnapshotStatus::default(),
        );
        snapshots
            .create(
                &s,
                &velstra_cloud_model::access::Writer::controller("snapshot"),
            )
            .await
            .unwrap();
        let controller = SnapshotController::new(snapshots.clone());
        (snapshots, controller)
    }

    async fn reload(snapshots: &TypedStore<SnapshotSpec, SnapshotStatus>) -> Option<Snapshot> {
        snapshots.get(NAME).await.unwrap()
    }

    async fn pass(
        snapshots: &TypedStore<SnapshotSpec, SnapshotStatus>,
        controller: &SnapshotController,
    ) {
        let s = reload(snapshots).await.unwrap();
        controller.reconcile(NAME, Some(&s)).await.unwrap();
    }

    async fn deleting(snapshots: &TypedStore<SnapshotSpec, SnapshotStatus>) {
        let mut s = reload(snapshots).await.unwrap();
        s.meta.deleted_at = Some(Timestamp::now());
        snapshots
            .update(&s, &Writer::controller("api"))
            .await
            .unwrap();
    }

    async fn say_released(
        snapshots: &TypedStore<SnapshotSpec, SnapshotStatus>,
        released: bool,
        taken: bool,
    ) {
        let mut s = reload(snapshots).await.unwrap();
        s.status.pool = Some("pool-a".into());
        s.status.taken = taken;
        set_condition(
            &mut s.status.conditions,
            Condition::new(
                "Released",
                if released {
                    ConditionStatus::True
                } else {
                    ConditionStatus::False
                },
                if released { "Released" } else { "Destroying" },
                "",
                s.meta.generation,
            ),
        );
        snapshots
            .update(&s, &Writer::agent("pool-a"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_guard_goes_on_before_the_copy_is_made() {
        let (snapshots, controller) = fixture().await;
        pass(&snapshots, &controller).await;
        assert!(
            reload(&snapshots)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER)
        );
    }

    #[tokio::test]
    async fn a_copy_whose_bytes_are_still_there_does_not_go() {
        let (snapshots, controller) = fixture().await;
        pass(&snapshots, &controller).await;
        say_released(&snapshots, false, true).await;
        deleting(&snapshots).await;

        pass(&snapshots, &controller).await;
        assert!(
            reload(&snapshots).await.is_some(),
            "the record went while the pool still held the copy"
        );
    }

    #[tokio::test]
    async fn a_copy_the_pool_has_let_go_of_is_deleted() {
        let (snapshots, controller) = fixture().await;
        pass(&snapshots, &controller).await;
        deleting(&snapshots).await;
        say_released(&snapshots, true, false).await;

        pass(&snapshots, &controller).await;
        assert!(
            !reload(&snapshots)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER)
        );
        pass(&snapshots, &controller).await;
        assert!(reload(&snapshots).await.is_none());
    }

    #[tokio::test]
    async fn a_copy_nobody_has_reported_on_is_not_mistaken_for_a_released_one() {
        // `taken == false` is true of a copy that has not been made yet and of
        // one whose bytes are gone. Inferring release from it deletes the
        // record of a copy the pool is still busy writing.
        let (snapshots, controller) = fixture().await;
        pass(&snapshots, &controller).await;
        deleting(&snapshots).await;

        assert!(!reload(&snapshots).await.unwrap().status.taken);
        pass(&snapshots, &controller).await;
        assert!(
            reload(&snapshots).await.is_some(),
            "a copy nobody has reported on was deleted on the strength of a default"
        );
    }

    #[tokio::test]
    async fn a_settled_snapshot_costs_nothing_to_look_at_again() {
        let (snapshots, controller) = fixture().await;
        pass(&snapshots, &controller).await;

        let revision = reload(&snapshots).await.unwrap().meta.revision;
        for _ in 0..2 {
            pass(&snapshots, &controller).await;
        }
        assert_eq!(
            reload(&snapshots).await.unwrap().meta.revision,
            revision,
            "a settled snapshot was written to again"
        );
    }

    #[tokio::test]
    async fn an_object_that_is_already_gone_is_not_an_error() {
        let (_, controller) = fixture().await;
        controller.reconcile(NAME, None).await.unwrap();
    }
}
