//! The number that says whether a cluster is healthy.
//!
//! Not instrumentation. In a system where every object carries what was asked
//! for and what is, "how many objects disagree with themselves, and for how
//! long" is the only question worth asking about the whole platform at once —
//! and it is answerable without a console, an incident, or somebody who knows
//! which log to read.
//!
//! Two series, both computed by scanning:
//!
//! ```text
//! objects_with_spec_status_mismatch{type,reason}   how many
//! spec_status_divergence_age_seconds{type,reason}  and the oldest of them
//! ```
//!
//! The age is deliberately the *oldest* rather than an average: a hundred
//! objects converging normally and one that has been stuck since Tuesday are
//! the same average and very different clusters.

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    meta::Timestamp,
    reconcile::{DivergenceReason, divergence},
    resources::Observed,
};
use velstra_cloud_store::TypedStore;

use crate::{Metrics, Result};

/// One object that is not where it should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergent {
    pub name: String,
    pub reason: DivergenceReason,
    pub age_seconds: u64,
}

pub const MISMATCH: &str = "objects_with_spec_status_mismatch";
pub const AGE: &str = "spec_status_divergence_age_seconds";

/// Scan one collection, publish its two series, and return what diverged.
///
/// Returning the list as well as counting it is the point: a count tells an
/// operator that something is wrong, and the list tells them which object,
/// without a second query against a different system.
pub async fn scan<S, T>(
    kind: &str,
    store: &TypedStore<S, T>,
    metrics: &Metrics,
    now: Timestamp,
) -> Result<Vec<Divergent>>
where
    S: Serialize + DeserializeOwned + PartialEq + Send + Sync,
    T: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync,
{
    let mut found = Vec::new();
    for object in store.list().await? {
        if let Some(d) = divergence(&object) {
            found.push(Divergent {
                name: object.meta.name.to_string(),
                reason: d.reason,
                age_seconds: d.since.age(now).as_secs(),
            });
        }
    }

    // Every reason this kind used to report is dropped first, so a count that
    // went to zero reads as zero rather than as the last number anybody saw.
    metrics.clear(MISMATCH, &[("type", kind)]);
    metrics.clear(AGE, &[("type", kind)]);
    for reason in [
        DivergenceReason::Unconverged,
        DivergenceReason::NotReady,
        DivergenceReason::Unreported,
        DivergenceReason::DeletionBlocked,
    ] {
        let matching: Vec<&Divergent> = found.iter().filter(|d| d.reason == reason).collect();
        if matching.is_empty() {
            continue;
        }
        let labels = [("type", kind), ("reason", reason.label())];
        metrics.set(MISMATCH, &labels, matching.len() as f64);
        metrics.set(
            AGE,
            &labels,
            matching.iter().map(|d| d.age_seconds).max().unwrap_or(0) as f64,
        );
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        Condition, ConditionStatus,
        meta::{Meta, Placement, ResourceName, set_condition},
        resources::{InstanceSpec, InstanceStatus, Resource},
    };
    use velstra_cloud_store::MemoryStore;

    use super::*;

    type Instances = TypedStore<InstanceSpec, InstanceStatus>;

    async fn store() -> Instances {
        TypedStore::new(Arc::new(MemoryStore::new()), "cell-1", "instances")
    }

    async fn put(
        store: &Instances,
        id: &str,
        build: impl FnOnce(&mut Resource<InstanceSpec, InstanceStatus>),
    ) {
        let mut i = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("projects/p1/instances/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec::default(),
            InstanceStatus::default(),
        );
        i.meta.created_at = Timestamp(0);
        build(&mut i);
        store
            .create(
                &i,
                &velstra_cloud_model::access::Writer::controller("drift"),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_settled_cluster_reports_nothing_at_all() {
        let store = store().await;
        put(&store, "i1", |i| {
            i.status.observed_generation = i.meta.generation;
            set_condition(&mut i.status.conditions, Condition::ready(1));
        })
        .await;

        let metrics = Metrics::new();
        let found = scan("instances", &store, &metrics, Timestamp(10_000))
            .await
            .unwrap();
        assert!(found.is_empty());
        assert_eq!(
            metrics.render(),
            "",
            "a healthy cluster still published drift"
        );
    }

    #[tokio::test]
    async fn each_kind_of_divergence_is_counted_and_dated() {
        let store = store().await;
        put(&store, "behind", |i| {
            i.meta.generation = 3;
            i.status.observed_generation = 1;
            set_condition(
                &mut i.status.conditions,
                Condition::new("Ready", ConditionStatus::True, "Ready", "", 1),
            );
            i.status.conditions[0].last_transition = Timestamp(1_000);
        })
        .await;
        put(&store, "broken", |i| {
            i.status.observed_generation = i.meta.generation;
            set_condition(
                &mut i.status.conditions,
                Condition::new("Ready", ConditionStatus::False, "VmFailed", "exited", 1),
            );
            i.status.conditions[0].last_transition = Timestamp(50_000);
        })
        .await;
        put(&store, "quiet", |i| {
            i.status.observed_generation = i.meta.generation;
        })
        .await;

        let metrics = Metrics::new();
        let found = scan("instances", &store, &metrics, Timestamp(100_000))
            .await
            .unwrap();
        assert_eq!(found.len(), 3);

        let labels = |reason: &'static str| [("type", "instances"), ("reason", reason)];
        assert_eq!(metrics.get(MISMATCH, &labels("Unconverged")), Some(1.0));
        assert_eq!(metrics.get(AGE, &labels("Unconverged")), Some(99.0));
        assert_eq!(metrics.get(MISMATCH, &labels("NotReady")), Some(1.0));
        assert_eq!(metrics.get(AGE, &labels("NotReady")), Some(50.0));
        assert_eq!(
            metrics.get(MISMATCH, &labels("Unreported")),
            Some(1.0),
            "an object nothing has ever said anything about is not healthy"
        );
    }

    #[tokio::test]
    async fn the_age_is_the_oldest_and_not_the_average() {
        // A hundred objects converging normally and one stuck since Tuesday
        // must not average into a number nobody pages on.
        let store = store().await;
        for (id, at) in [("new", 90_000u64), ("ancient", 1_000)] {
            put(&store, id, |i| {
                i.meta.generation = 2;
                i.status.observed_generation = 1;
                set_condition(&mut i.status.conditions, Condition::ready(1));
                i.status.conditions[0].last_transition = Timestamp(at);
            })
            .await;
        }
        let metrics = Metrics::new();
        scan("instances", &store, &metrics, Timestamp(100_000))
            .await
            .unwrap();
        assert_eq!(
            metrics.get(AGE, &[("type", "instances"), ("reason", "Unconverged")]),
            Some(99.0)
        );
    }

    #[tokio::test]
    async fn a_divergence_that_healed_stops_being_reported() {
        let store = store().await;
        put(&store, "i1", |i| {
            i.meta.generation = 2;
            i.status.observed_generation = 1;
        })
        .await;
        let metrics = Metrics::new();
        scan("instances", &store, &metrics, Timestamp(1_000))
            .await
            .unwrap();
        assert!(
            metrics
                .get(
                    MISMATCH,
                    &[("type", "instances"), ("reason", "Unconverged")]
                )
                .is_some()
        );

        let stored = store
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();
        store
            .delete(
                "projects/p1/instances/i1",
                stored.meta.revision,
                &velstra_cloud_model::access::Writer::controller("drift"),
            )
            .await
            .unwrap();

        scan("instances", &store, &metrics, Timestamp(2_000))
            .await
            .unwrap();
        assert_eq!(
            metrics.get(
                MISMATCH,
                &[("type", "instances"), ("reason", "Unconverged")]
            ),
            None,
            "the dashboard still shows a divergence that healed"
        );
    }
}
