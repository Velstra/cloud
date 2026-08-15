//! AIP-151 operations, computed rather than remembered.
//!
//! An operation's `done` is derived on every pass from the object it points at:
//! has the target caught up with the generation this operation was created for,
//! and what does the target say about itself now. Nothing here writes a fact
//! that could later be wrong, so the failure this design is built against —
//! an operation that says `false` forever because whatever was going to mark it
//! done died first — cannot happen. The worst a crash costs is one pass of
//! latency.
//!
//! The target is read as bytes and peeked at through the two fields every
//! resource has. One code path serves every kind of target, and adding a
//! resource type does not mean touching this file.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use serde::Deserialize;
use velstra_cloud_model::{
    Condition, ConditionStatus,
    meta::{ResourceName, Timestamp, condition, set_condition},
    reconcile::{TargetView, operation_progress},
    resources::{Operation, OperationSpec, OperationStatus},
};
use velstra_cloud_store::{Store, key_for, prefix_for};

use crate::{Related, Result, runner::Reconciler, status::StatusWriter};

/// Collections an operation can be about. A watch on each, so an operation
/// finishes when its target does rather than when the resync comes round.
const TARGET_KINDS: [&str; 6] = [
    "instances",
    "volumes",
    "attachments",
    "networks",
    "subnets",
    "ports",
];

pub struct OperationsController {
    store: Arc<dyn Store>,
    status: StatusWriter<OperationSpec, OperationStatus>,
    cell: String,
    /// Target name to the operations waiting on it, learned as operations are
    /// reconciled. Only ever an optimisation: an operation missing from here is
    /// found by the resync, one interval later.
    watching: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
}

impl OperationsController {
    pub fn new(
        store: Arc<dyn Store>,
        status: StatusWriter<OperationSpec, OperationStatus>,
        cell: &str,
    ) -> Self {
        Self {
            store,
            status,
            cell: cell.to_string(),
            watching: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// What the target looks like, in the two fields any resource has.
    async fn look_at(&self, target: &str) -> Result<TargetView> {
        let Ok(name) = ResourceName::parse(target) else {
            // A target that is not a resource name can never resolve, and
            // saying so beats waiting for it forever.
            return Ok(TargetView::Gone);
        };
        let key = key_for(&self.cell, name.collection(), target);
        let Some(entry) = self.store.get(&key).await? else {
            return Ok(TargetView::Gone);
        };
        let Ok(peek) = serde_json::from_slice::<Peek>(&entry.value) else {
            return Ok(TargetView::Gone);
        };
        let ready = condition(&peek.status.conditions, "Ready");
        Ok(TargetView::Present {
            observed_generation: peek.status.observed_generation,
            ready: ready.map(|c| c.status).unwrap_or(ConditionStatus::Unknown),
            reason: ready.map(|c| c.reason.clone()).unwrap_or_default(),
            message: ready.map(|c| c.message.clone()).unwrap_or_default(),
        })
    }
}

/// Every resource, seen through the fields that are the same on all of them.
#[derive(Deserialize)]
struct Peek {
    status: PeekStatus,
}

#[derive(Deserialize)]
struct PeekStatus {
    observed_generation: u64,
    conditions: Vec<Condition>,
}

impl Reconciler for OperationsController {
    type Spec = OperationSpec;
    type Status = OperationStatus;

    fn name(&self) -> &'static str {
        "operations"
    }

    fn related(&self) -> Vec<Related> {
        TARGET_KINDS
            .iter()
            .map(|kind| {
                let watching = self.watching.clone();
                Related {
                    prefix: prefix_for(&self.cell, kind),
                    map: Arc::new(move |target: &str| {
                        watching
                            .lock()
                            .unwrap()
                            .get(target)
                            .map(|ops| ops.iter().cloned().collect())
                            .unwrap_or_default()
                    }),
                }
            })
            .collect()
    }

    async fn reconcile(&self, name: &str, object: Option<&Operation>) -> Result<()> {
        let Some(operation) = object else {
            self.watching.lock().unwrap().values_mut().for_each(|ops| {
                ops.remove(name);
            });
            return Ok(());
        };

        let target = self.look_at(&operation.spec.target).await?;
        let progress = operation_progress(&operation.spec, &target);

        let mut next = operation.clone();
        next.status.done = progress.done;
        next.status.error = progress.error.clone();
        next.status.observed_generation = operation.meta.generation;
        // Stamped once, when it first computes as finished. Re-stamping it on
        // every pass would make an operation's own timestamp move under a
        // client that is polling it.
        if progress.done && next.status.finished_at.is_none() {
            next.status.finished_at = Some(Timestamp::now());
        }
        set_condition(
            &mut next.status.conditions,
            match (progress.done, &progress.error) {
                (false, _) => Condition::new(
                    "Ready",
                    ConditionStatus::Unknown,
                    "Working",
                    "the target has not caught up yet",
                    operation.meta.generation,
                ),
                (true, None) => Condition::ready(operation.meta.generation),
                (true, Some(error)) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "Failed",
                    error,
                    operation.meta.generation,
                ),
            },
        );
        self.status.write(operation, &next).await?;

        let mut watching = self.watching.lock().unwrap();
        if progress.done {
            // Nothing more can change about it, so stop waking it.
            if let Some(ops) = watching.get_mut(&operation.spec.target) {
                ops.remove(name);
            }
        } else {
            watching
                .entry(operation.spec.target.clone())
                .or_default()
                .insert(name.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement},
        resources::{InstanceSpec, InstanceState, InstanceStatus, Resource},
    };
    use velstra_cloud_store::{MemoryStore, TypedStore};

    use super::*;

    struct Fixture {
        raw: Arc<MemoryStore>,
        operations: TypedStore<OperationSpec, OperationStatus>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
    }

    fn fixture() -> (Fixture, OperationsController) {
        let raw = Arc::new(MemoryStore::new());
        let f = Fixture {
            operations: TypedStore::new(raw.clone(), "cell-1", "operations"),
            instances: TypedStore::new(raw.clone(), "cell-1", "instances"),
            raw: raw.clone(),
        };
        let controller = OperationsController::new(
            raw.clone(),
            StatusWriter::new(raw, "cell-1", "operations", "operations"),
            "cell-1",
        );
        (f, controller)
    }

    impl Fixture {
        async fn operation(&self, verb: &str, generation: u64) -> Operation {
            let op = Resource::new(
                Meta::new(
                    ResourceName::parse("projects/p1/operations/op-7").unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                OperationSpec {
                    target: "projects/p1/instances/i1".into(),
                    target_generation: generation,
                    verb: verb.into(),
                    requested_by: "someone".into(),
                },
                OperationStatus::default(),
            );
            self.operations.create(&op).await.unwrap();
            self.reload().await
        }

        async fn instance(&self, observed: u64, ready: Option<Condition>) {
            let mut i = Resource::new(
                Meta::new(
                    ResourceName::parse("projects/p1/instances/i1").unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                InstanceSpec::default(),
                InstanceStatus {
                    observed_generation: observed,
                    state: InstanceState::Running,
                    // Owned from the start, because a status is only ever
                    // written by the agent that owns the object.
                    node: Some("node-a".into()),
                    ..Default::default()
                },
            );
            if let Some(c) = ready {
                set_condition(&mut i.status.conditions, c);
            }
            self.instances.create(&i).await.unwrap();
        }

        async fn reload(&self) -> Operation {
            self.operations
                .get("projects/p1/operations/op-7")
                .await
                .unwrap()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn an_operation_follows_its_target_and_never_leads_it() {
        let (f, controller) = fixture();
        f.instance(0, None).await;
        let op = f.operation("create", 1).await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();
        assert!(
            !f.reload().await.status.done,
            "done before the node said anything"
        );

        // The instance catches up and reports.
        let mut i = f
            .instances
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();
        i.status.observed_generation = 1;
        set_condition(&mut i.status.conditions, Condition::ready(1));
        f.instances
            .update(&i, &velstra_cloud_model::access::Writer::agent("node-a"))
            .await
            .unwrap();

        let op = f.reload().await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();
        let done = f.reload().await;
        assert!(done.status.done);
        assert!(done.status.error.is_none());
        assert!(done.status.finished_at.is_some());
    }

    #[tokio::test]
    async fn a_finished_operation_keeps_the_time_it_finished() {
        let (f, controller) = fixture();
        f.instance(1, Some(Condition::ready(1))).await;
        let op = f.operation("create", 1).await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();
        let first = f.reload().await;

        let revision = f.raw.revision().await.unwrap();
        controller
            .reconcile("projects/p1/operations/op-7", Some(&first))
            .await
            .unwrap();
        assert_eq!(
            f.raw.revision().await.unwrap(),
            revision,
            "a finished operation was rewritten, moving the timestamp a client is polling"
        );
        assert_eq!(
            f.reload().await.status.finished_at,
            first.status.finished_at
        );
    }

    #[tokio::test]
    async fn an_operation_carries_its_targets_failure() {
        let (f, controller) = fixture();
        f.instance(
            1,
            Some(Condition::new(
                "Ready",
                ConditionStatus::False,
                "NoValidHost",
                "node-a: draining",
                1,
            )),
        )
        .await;
        let op = f.operation("create", 1).await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();

        let done = f.reload().await;
        assert!(done.status.done, "a client would poll this forever");
        assert_eq!(
            done.status.error.as_deref(),
            Some("NoValidHost: node-a: draining")
        );
    }

    #[tokio::test]
    async fn a_delete_finishes_when_the_object_is_gone() {
        let (f, controller) = fixture();
        let op = f.operation("delete", 1).await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();
        let done = f.reload().await;
        assert!(done.status.done);
        assert!(done.status.error.is_none());
    }

    #[tokio::test]
    async fn a_target_that_vanished_ends_the_operation_rather_than_hanging() {
        let (f, controller) = fixture();
        let op = f.operation("create", 1).await;
        controller
            .reconcile("projects/p1/operations/op-7", Some(&op))
            .await
            .unwrap();
        let done = f.reload().await;
        assert!(done.status.done);
        assert!(done.status.error.is_some());
    }
}
