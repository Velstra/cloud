//! The controller half of an instance's life.
//!
//! One guard, and its order is the safety property in both directions: it goes
//! on before a scheduler can put the instance anywhere, and comes off only once
//! the node holding it says it has let go.
//!
//! Without it a delete was not a teardown. `Api::delete` stamps `deletedAt` and
//! then removes the object outright when nothing holds it, so an instance that
//! carried no finalizer disappeared from the store inside the same request that
//! asked for it to go. The node agent's whole teardown path —
//! [`velstra_cloud_model::reconcile::reconcile_instance`]'s `is_deleting`
//! branch, which stops the VM and unprograms its ports — is reached only by
//! reading a *stored* object that is being deleted, and there was never one to
//! read. The API answered 200, the console showed the machine gone, and the
//! hypervisor kept running it with its taps up. Nothing noticed, because every
//! test of that branch handed `reconcile_instance` an object it had built by
//! hand rather than one that had been through a delete.
//!
//! This is the storage half's own pattern — see [`crate::volume`] — applied to
//! compute, and for the same reason: the node cannot drop the finalizer itself
//! because `meta` belongs to a controller, so it publishes a `Released`
//! condition and this reads it.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{Instance, InstanceSpec, InstanceStatus, NODE_RELEASE_FINALIZER},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "instance";

pub struct InstanceController {
    instances: TypedStore<InstanceSpec, InstanceStatus>,
}

impl InstanceController {
    pub fn new(instances: TypedStore<InstanceSpec, InstanceStatus>) -> Self {
        Self { instances }
    }
}

/// Whether no node is holding this instance any more.
///
/// Two ways that can be true, and the second is not a shortcut:
///
/// * a node **said so** — the `Released` condition the agent writes once the
///   guest is off the machine and none of its ports are in the datapath. Read
///   from the condition and never inferred from `status.state`, for the same
///   reason a volume's release is: an instance that failed to start has
///   `state != Running` too, and treating that as "let go" would delete the
///   record while a half-built guest sat on a node.
///
/// * **no node has it and none was ever given it.** An instance the scheduler
///   could not place has no agent that will ever report on it, so a guard that
///   waits for one is an object nobody can delete without editing the store —
///   and the cell's own e2e cell keeps exactly such an instance around on
///   purpose. Both halves are required: an instance a scheduler has assigned but
///   no node has claimed still waits, because the node may be about to claim it
///   and the honest failure is a delete that is visibly pending rather than a
///   record removed from under a machine that is starting a guest.
fn node_has_let_go(instance: &Instance) -> bool {
    if condition(&instance.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
    {
        return true;
    }
    instance.status.node.is_none() && instance.spec.node.is_none()
}

impl Reconciler for InstanceController {
    type Spec = InstanceSpec;
    type Status = InstanceStatus;

    fn name(&self) -> &'static str {
        "instance"
    }

    async fn reconcile(&self, name: &str, object: Option<&Instance>) -> Result<()> {
        let Some(instance) = object else {
            return Ok(());
        };

        match finalizer_step(&instance.meta, NODE_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                // Before a scheduler can place it. Added afterwards, there would
                // be a window in which a delete takes the record and leaves the
                // guest — which is the whole defect this file exists to close,
                // narrowed to a race instead of the standing state of affairs.
                //
                // The generation is deliberately not bumped: a finalizer is not
                // a change to what was asked for, and bumping it would make
                // every instance in the cell look unconverged for one pass.
                let mut next = instance.clone();
                next.meta.add_finalizer(NODE_RELEASE_FINALIZER);
                self.instances
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                Ok(())
            }
            FinalizerStep::Wait => {
                // Deleting, and still guarded. Until the node says it has let
                // go, nothing happens — which is what keeps the object visible
                // while somebody looks at a hypervisor that will not stop a
                // guest.
                if !instance.meta.is_deleting() || !node_has_let_go(instance) {
                    return Ok(());
                }
                let mut next = instance.clone();
                next.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
                self.instances
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                info!(instance = name, "the node let go; the guard is off");
                Ok(())
            }
            FinalizerStep::Delete => {
                // Conditional on the revision, so an instance that gained a
                // finalizer between the read and now survives instead of being
                // torn out from under whoever added it.
                self.instances.delete(name, instance.meta.revision).await?;
                info!(instance = name, "gone");
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

    const NAME: &str = "projects/p1/instances/i1";

    async fn fixture() -> (
        Arc<MemoryStore>,
        TypedStore<InstanceSpec, InstanceStatus>,
        InstanceController,
    ) {
        let raw = Arc::new(MemoryStore::new());
        let store: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let controller = InstanceController::new(store.clone());
        store
            .create(&Resource::new(
                Meta::new(
                    ResourceName::parse(NAME).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                InstanceSpec::default(),
                InstanceStatus::default(),
            ))
            .await
            .unwrap();
        (raw, store, controller)
    }

    async fn reload(store: &TypedStore<InstanceSpec, InstanceStatus>) -> Option<Instance> {
        store.get(NAME).await.unwrap()
    }

    async fn pass(
        controller: &InstanceController,
        store: &TypedStore<InstanceSpec, InstanceStatus>,
    ) {
        let object = reload(store).await;
        controller.reconcile(NAME, object.as_ref()).await.unwrap();
    }

    /// Put the instance on a node, the way the scheduler and the agent do
    /// between them: the assignment in `spec` by a controller, the claim in
    /// `status` by the node. Two writes rather than one because the store
    /// refuses a single writer touching both halves, which is invariant 1 and
    /// exactly the rule this controller has to live inside.
    async fn placed_on(store: &TypedStore<InstanceSpec, InstanceStatus>, node: &str) {
        let mut assigned = reload(store).await.unwrap();
        assigned.spec.node = Some(node.to_string());
        assigned.meta.generation += 1;
        store
            .update(&assigned, &Writer::controller("scheduler"))
            .await
            .unwrap();

        let mut claimed = reload(store).await.unwrap();
        claimed.status.node = Some(node.to_string());
        store.update(&claimed, &Writer::agent(node)).await.unwrap();
    }

    async fn deleting(store: &TypedStore<InstanceSpec, InstanceStatus>) {
        let mut object = reload(store).await.unwrap();
        object.meta.deleted_at = Some(Timestamp::now());
        store
            .update(&object, &Writer::controller("api"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_guard_goes_on_before_anything_can_be_placed() {
        let (_, store, controller) = fixture().await;
        pass(&controller, &store).await;
        assert!(
            reload(&store)
                .await
                .unwrap()
                .meta
                .has_finalizer(NODE_RELEASE_FINALIZER),
            "an instance was left with nothing to stop a delete outrunning its node"
        );
    }

    #[tokio::test]
    async fn an_instance_a_node_still_holds_does_not_go() {
        // The failure this prevents, and the one measured in the e2e cell: the
        // record disappears while the hypervisor is still running the guest,
        // after which nothing in the system knows the machine exists.
        let (_, store, controller) = fixture().await;
        pass(&controller, &store).await;
        placed_on(&store, "node-a").await;
        deleting(&store).await;

        pass(&controller, &store).await;
        assert!(
            reload(&store).await.is_some(),
            "the object went while a node held it"
        );

        // The node reports what it observed: the guest is off this machine.
        let mut released = reload(&store).await.unwrap();
        set_condition(
            &mut released.status.conditions,
            Condition::new(
                "Released",
                ConditionStatus::True,
                "Released",
                "this node holds nothing of it",
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
            reload(&store).await.is_none(),
            "a fully released instance stayed"
        );
    }

    #[tokio::test]
    async fn a_node_that_has_not_answered_is_not_read_as_a_node_that_let_go() {
        // `Released` absent is not `Released == False`. Without reading the
        // condition itself, a node that has claimed the instance and not yet
        // reported would look exactly like one that had finished tearing down.
        let (_, store, controller) = fixture().await;
        pass(&controller, &store).await;
        placed_on(&store, "node-a").await;
        deleting(&store).await;
        for _ in 0..3 {
            pass(&controller, &store).await;
        }
        assert!(
            reload(&store).await.is_some(),
            "a silent node was taken for a node that had let go"
        );
    }

    #[tokio::test]
    async fn an_instance_nothing_ever_placed_can_still_be_deleted() {
        // No node will ever write `Released` on it, so waiting for one is a
        // guard nobody can lift. The e2e cell keeps such an instance on purpose.
        let (_, store, controller) = fixture().await;
        pass(&controller, &store).await;
        deleting(&store).await;
        pass(&controller, &store).await;
        pass(&controller, &store).await;
        assert!(
            reload(&store).await.is_none(),
            "an instance no node ever held could not be deleted"
        );
    }

    #[tokio::test]
    async fn a_settled_instance_is_reconciled_without_writing() {
        let (raw, store, controller) = fixture().await;
        pass(&controller, &store).await;
        let revision = raw.revision().await.unwrap();
        pass(&controller, &store).await;
        assert_eq!(
            raw.revision().await.unwrap(),
            revision,
            "a settled instance was written to"
        );
    }

    #[tokio::test]
    async fn an_object_that_is_already_gone_is_not_an_error() {
        let (_, _, controller) = fixture().await;
        controller
            .reconcile("projects/p1/instances/never", None)
            .await
            .unwrap();
    }
}
