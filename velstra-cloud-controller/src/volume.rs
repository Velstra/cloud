//! The controller half of a volume's life.
//!
//! Two writes, in a fixed order, and the order is the safety property: the guard
//! goes on before any pool can be asked to create the backing store, and it
//! comes off only once the pool has said the bytes are gone.
//!
//! What it prevents is the thing that quietly costs money: an object deleted
//! from the API while a pool still holds the gigabytes. Nobody is billed for
//! them and nobody can find them — the record that named them is gone. A `spec`
//! field saying "deleted" cannot express "asked to destroy, has not yet"; a
//! finalizer can.
//!
//! The pool cannot drop the finalizer itself, because `meta` belongs to a
//! controller. So it publishes a `Released` condition and this reads it. That
//! indirection is deliberate: the alternative is a controller inferring release
//! from `provisioned == false`, which would be a second definition of "let go"
//! living somewhere else — and the two would disagree the first time a volume
//! failed to provision in the first place.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{POOL_RELEASE_FINALIZER, Volume, VolumeSpec, VolumeStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "volume";

pub struct VolumeController {
    volumes: TypedStore<VolumeSpec, VolumeStatus>,
}

impl VolumeController {
    pub fn new(volumes: TypedStore<VolumeSpec, VolumeStatus>) -> Self {
        Self { volumes }
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

    async fn reconcile(&self, name: &str, object: Option<&Volume>) -> Result<()> {
        let Some(volume) = object else {
            return Ok(());
        };

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
        meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const NAME: &str = "projects/p1/volumes/data-1";

    async fn fixture() -> (TypedStore<VolumeSpec, VolumeStatus>, VolumeController) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let volumes: TypedStore<VolumeSpec, VolumeStatus> =
            TypedStore::new(store, "cell-1", "volumes");
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
            },
            VolumeStatus::default(),
        );
        volumes.create(&v).await.unwrap();
        let controller = VolumeController::new(volumes.clone());
        (volumes, controller)
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
        let (volumes, controller) = fixture().await;
        let v = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&v)).await.unwrap();
        assert!(
            reload(&volumes)
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
        let (volumes, controller) = fixture().await;
        let v = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&v)).await.unwrap();
        say_released(&volumes, false).await;

        let mut deleting = reload(&volumes).await.unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        volumes
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();

        let held = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&held)).await.unwrap();
        assert!(
            reload(&volumes).await.is_some(),
            "the object went while the pool still held it"
        );
        assert!(
            reload(&volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER),
            "the guard came off before the pool said it had let go"
        );
    }

    #[tokio::test]
    async fn a_volume_the_pool_has_let_go_of_is_deleted() {
        let (volumes, controller) = fixture().await;
        let v = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&v)).await.unwrap();

        let mut deleting = reload(&volumes).await.unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        volumes
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();
        say_released(&volumes, true).await;

        // One pass takes the guard off…
        let held = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&held)).await.unwrap();
        assert!(
            !reload(&volumes)
                .await
                .unwrap()
                .meta
                .has_finalizer(POOL_RELEASE_FINALIZER)
        );
        // …and the next takes the object.
        let free = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&free)).await.unwrap();
        assert!(reload(&volumes).await.is_none());
    }

    #[tokio::test]
    async fn a_volume_that_was_never_provisioned_is_not_mistaken_for_a_released_one() {
        // `provisioned == false` is true of a volume nobody has made yet and of
        // one whose bytes are gone. Inferring release from it would delete the
        // record of a half-made backing store, which is the one case where
        // guessing costs the most.
        let (volumes, controller) = fixture().await;
        let v = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&v)).await.unwrap();

        let mut deleting = reload(&volumes).await.unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        volumes
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();

        // No `Released` condition at all — the pool has never reported.
        let quiet = reload(&volumes).await.unwrap();
        assert!(!quiet.status.provisioned);
        controller.reconcile(NAME, Some(&quiet)).await.unwrap();
        assert!(
            reload(&volumes).await.is_some(),
            "a volume nobody has reported on was deleted on the strength of a default"
        );
    }

    #[tokio::test]
    async fn a_settled_volume_costs_nothing_to_look_at_again() {
        let (volumes, controller) = fixture().await;
        let v = reload(&volumes).await.unwrap();
        controller.reconcile(NAME, Some(&v)).await.unwrap();

        let guarded = reload(&volumes).await.unwrap();
        let revision = guarded.meta.revision;
        for _ in 0..2 {
            let now = reload(&volumes).await.unwrap();
            controller.reconcile(NAME, Some(&now)).await.unwrap();
        }
        assert_eq!(
            reload(&volumes).await.unwrap().meta.revision,
            revision,
            "a settled volume was written to again"
        );
    }
}
