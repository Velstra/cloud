//! The controller half of the finalizer dance.
//!
//! Two writes, in a fixed order, and the order is the safety property: the
//! guard goes on before any node can be told to open the volume, and the object
//! goes away only once every holder has said it has let go. Everything between
//! those two moments is the node's business, decided by
//! [`velstra_cloud_model::reconcile::reconcile_attachment`].
//!
//! What this prevents is one thing, and it is the thing that eats data: two
//! nodes with one RBD image open. A `spec` field saying "detached" cannot
//! express "asked to let go, has not yet"; a finalizer can, and it is the node
//! itself that removes it.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{Attachment, AttachmentSpec, AttachmentStatus, NODE_RELEASE_FINALIZER},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

pub struct AttachmentController {
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
}

impl AttachmentController {
    pub fn new(attachments: TypedStore<AttachmentSpec, AttachmentStatus>) -> Self {
        Self { attachments }
    }
}

impl Reconciler for AttachmentController {
    type Spec = AttachmentSpec;
    type Status = AttachmentStatus;

    fn name(&self) -> &'static str {
        "attachment"
    }

    async fn reconcile(&self, name: &str, object: Option<&Attachment>) -> Result<()> {
        let Some(attachment) = object else {
            return Ok(());
        };

        match finalizer_step(&attachment.meta, NODE_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                let mut next = attachment.clone();
                next.meta.add_finalizer(NODE_RELEASE_FINALIZER);
                self.attachments
                    .update(&next, &Writer::controller("attachment"))
                    .await?;
                Ok(())
            }
            FinalizerStep::Delete => {
                // Everybody has let go. The delete is conditional on the
                // revision, so an attachment that gained a finalizer between
                // the read and now survives instead of being torn out from
                // under whoever added it.
                self.attachments
                    .delete(name, attachment.meta.revision)
                    .await?;
                info!(attachment = name, "released");
                Ok(())
            }
            FinalizerStep::Wait => {
                // Deleting, and still guarded. This arm used to be `Ok(())`,
                // and nothing anywhere else took the finalizer off — so an
                // attachment the node had closed and reported `Released` on sat
                // in the store for ever, carrying its `deletedAt`. That is not
                // only an object nobody can be rid of: a volume cannot be
                // deleted while an attachment names it, so one detach that never
                // completed made its volume undeletable too.
                //
                // It survived because the test for the second half removed the
                // finalizer by hand to get to the case it was interested in,
                // which is exactly the shape of a test that would pass with the
                // code under it deleted.
                if !attachment.meta.is_deleting() || !node_has_let_go(attachment) {
                    return Ok(());
                }
                let mut next = attachment.clone();
                next.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
                self.attachments
                    .update(&next, &Writer::controller("attachment"))
                    .await?;
                info!(attachment = name, "the node let go; the guard is off");
                Ok(())
            }
        }
    }
}

/// Whether the node has said it holds nothing of this attachment any more.
///
/// Read from the condition the node writes, never inferred from
/// `status.attached`. An attachment that never opened has `attached == false`
/// too, and treating that as "let go" would take the record away while a node
/// was part way through opening the image — which is the one case where two
/// nodes end up with one volume open, and that eats data.
fn node_has_let_go(attachment: &Attachment) -> bool {
    condition(&attachment.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    async fn fixture() -> (
        Arc<MemoryStore>,
        TypedStore<AttachmentSpec, AttachmentStatus>,
        AttachmentController,
    ) {
        let raw = Arc::new(MemoryStore::new());
        let store: TypedStore<AttachmentSpec, AttachmentStatus> =
            TypedStore::new(raw.clone(), "cell-1", "attachments");
        let controller = AttachmentController::new(store.clone());
        let a = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/attachments/a1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            AttachmentSpec {
                volume: "projects/p1/volumes/v1".into(),
                instance: "projects/p1/instances/i1".into(),
                node: "node-a".into(),
                read_only: false,
            },
            AttachmentStatus::default(),
        );
        store.create(&a).await.unwrap();
        (raw, store, controller)
    }

    async fn reload(store: &TypedStore<AttachmentSpec, AttachmentStatus>) -> Option<Attachment> {
        store.get("projects/p1/attachments/a1").await.unwrap()
    }

    #[tokio::test]
    async fn the_guard_goes_on_before_anything_can_depend_on_it() {
        let (_, store, controller) = fixture().await;
        let a = reload(&store).await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&a))
            .await
            .unwrap();
        assert!(
            reload(&store)
                .await
                .unwrap()
                .meta
                .has_finalizer(NODE_RELEASE_FINALIZER)
        );
    }

    #[tokio::test]
    async fn an_object_a_node_still_holds_does_not_go() {
        // The failure this prevents: the record of the attachment disappearing
        // while the node still has the image open, after which nothing in the
        // system knows it must be closed before it can be opened elsewhere.
        let (_, store, controller) = fixture().await;
        let a = reload(&store).await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&a))
            .await
            .unwrap();

        let mut deleting = reload(&store).await.unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        store
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();

        let held = reload(&store).await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&held))
            .await
            .unwrap();
        assert!(
            reload(&store).await.is_some(),
            "the object went while a node held it"
        );

        let mut released = reload(&store).await.unwrap();
        released.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
        store
            .update(&released, &Writer::controller("node-release"))
            .await
            .unwrap();

        let free = reload(&store).await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&free))
            .await
            .unwrap();
        assert!(
            reload(&store).await.is_none(),
            "a fully released object stayed"
        );
    }

    #[tokio::test]
    async fn a_settled_attachment_is_reconciled_without_writing() {
        let (raw, store, controller) = fixture().await;
        let a = reload(&store).await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&a))
            .await
            .unwrap();

        let settled = reload(&store).await.unwrap();
        let revision = raw.revision().await.unwrap();
        controller
            .reconcile("projects/p1/attachments/a1", Some(&settled))
            .await
            .unwrap();
        assert_eq!(raw.revision().await.unwrap(), revision);
    }

    #[tokio::test]
    async fn an_object_that_is_already_gone_is_not_an_error() {
        let (_, _, controller) = fixture().await;
        controller
            .reconcile("projects/p1/attachments/gone", None)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod release_tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const NAME: &str = "projects/p1/attachments/a1";

    async fn fixture() -> (
        TypedStore<AttachmentSpec, AttachmentStatus>,
        AttachmentController,
    ) {
        let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let store: TypedStore<AttachmentSpec, AttachmentStatus> =
            TypedStore::new(raw, "cell-1", "attachments");
        let controller = AttachmentController::new(store.clone());
        store
            .create(&Resource::new(
                Meta::new(
                    ResourceName::parse(NAME).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                AttachmentSpec {
                    volume: "projects/p1/volumes/v1".into(),
                    instance: "projects/p1/instances/i1".into(),
                    node: "node-a".into(),
                    read_only: false,
                },
                AttachmentStatus::default(),
            ))
            .await
            .unwrap();
        (store, controller)
    }

    async fn pass(
        controller: &AttachmentController,
        store: &TypedStore<AttachmentSpec, AttachmentStatus>,
    ) {
        let object = store.get(NAME).await.unwrap();
        controller.reconcile(NAME, object.as_ref()).await.unwrap();
    }

    #[tokio::test]
    async fn an_attachment_the_node_has_let_go_of_is_deleted() {
        // Nothing used to take this finalizer off, so a detach never finished
        // and the volume underneath could never be deleted either.
        let (store, controller) = fixture().await;
        pass(&controller, &store).await;

        let mut deleting = store.get(NAME).await.unwrap().unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        store
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();

        let mut released = store.get(NAME).await.unwrap().unwrap();
        released.status.node = Some("node-a".into());
        set_condition(
            &mut released.status.conditions,
            Condition::new(
                "Released",
                ConditionStatus::True,
                "Released",
                "this node no longer has the volume open",
                released.meta.generation,
            ),
        );
        store
            .update(&released, &Writer::agent("node-a"))
            .await
            .unwrap();

        // One pass takes the guard off, the next takes the object.
        pass(&controller, &store).await;
        pass(&controller, &store).await;
        assert!(
            store.get(NAME).await.unwrap().is_none(),
            "the node let go and the attachment stayed for ever"
        );
    }

    #[tokio::test]
    async fn a_node_that_has_not_reported_keeps_the_guard_on() {
        // `Released` absent is not `Released == False`. Two nodes with one RBD
        // image open is what this whole dance exists to prevent.
        let (store, controller) = fixture().await;
        pass(&controller, &store).await;
        let mut deleting = store.get(NAME).await.unwrap().unwrap();
        deleting.meta.deleted_at = Some(Timestamp::now());
        store
            .update(&deleting, &Writer::controller("api"))
            .await
            .unwrap();
        for _ in 0..3 {
            pass(&controller, &store).await;
        }
        assert!(
            store.get(NAME).await.unwrap().is_some(),
            "a silent node was taken for a node that had let go"
        );
    }
}
