//! The one place a controller writes `status`, and the reason it is not
//! [`velstra_cloud_store::TypedStore::update`].
//!
//! Invariant 1 is enforced in the store: a controller writes `spec`, an agent
//! writes `status`, and the store refuses either the other's half. That rule is
//! right for every object an agent owns — and three of the objects here have no
//! agent and never will. A project's `used` quota is counted from what exists.
//! An operation's `done` is computed from its target. An instance nobody has
//! placed yet is owned by nobody, which is precisely when the scheduler must
//! say on the object *why* it could not be placed.
//!
//! So this writer exists, and it is deliberately narrow: it goes through
//! [`velstra_cloud_model::reconcile::controller_may_write_status`], which
//! permits a controller to write `status` **only while no agent owns the
//! object**. There is still exactly one writer per field at any moment, and the
//! handover — an agent taking ownership — is itself a compare-and-swap, so
//! there is no window where both could write.
//!
//! It re-checks the other half of the rule too: a write through here that also
//! touched `spec` or metadata is refused before the store ever sees it, so this
//! path cannot be used to smuggle a spec change past the store's gate.

use std::{marker::PhantomData, sync::Arc};

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    meta::Revision,
    reconcile::controller_may_write_status,
    resources::{Assigned, Observed, Resource},
};
use velstra_cloud_store::{Expect, Store, key_for};

use crate::{Error, Result};

pub struct StatusWriter<S, T> {
    store: Arc<dyn Store>,
    cell: String,
    kind: &'static str,
    who: &'static str,
    _marker: PhantomData<(S, T)>,
}

impl<S, T> Clone for StatusWriter<S, T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            cell: self.cell.clone(),
            kind: self.kind,
            who: self.who,
            _marker: PhantomData,
        }
    }
}

impl<S, T> StatusWriter<S, T>
where
    S: Serialize + DeserializeOwned + PartialEq + Assigned + Send + Sync,
    T: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync,
{
    pub fn new(store: Arc<dyn Store>, cell: &str, kind: &'static str, who: &'static str) -> Self {
        Self {
            store,
            cell: cell.to_string(),
            kind,
            who,
            _marker: PhantomData,
        }
    }

    /// Write `next`, which must differ from `previous` in `status` and nothing
    /// else. Returns `None` when there was nothing to write.
    ///
    /// That `None` is the whole reason this takes both copies: comparing before
    /// writing is what makes a resync over a settled cluster free. A controller
    /// that writes what it computed without checking whether it differs turns
    /// every resync into a write storm, and every watcher in the cluster into a
    /// consumer of it.
    pub async fn write(
        &self,
        previous: &Resource<S, T>,
        next: &Resource<S, T>,
    ) -> Result<Option<Revision>> {
        if previous.spec != next.spec || previous.meta.generation != next.meta.generation {
            return Err(Error::Refused(format!(
                "{} changed spec on the status path for {}",
                self.who, next.meta.name
            )));
        }
        if meta_changed(previous, next) {
            return Err(Error::Refused(format!(
                "{} changed metadata on the status path for {}",
                self.who, next.meta.name
            )));
        }
        let owner = previous.status.owner();
        if !controller_may_write_status(owner) {
            return Err(Error::Refused(format!(
                "{} wrote the status of {}, which the agent on {} owns",
                self.who,
                next.meta.name,
                owner.unwrap_or("nobody")
            )));
        }
        if previous.status == next.status {
            return Ok(None);
        }

        let key = key_for(&self.cell, self.kind, &next.meta.name.to_string());
        let bytes = serde_json::to_vec(next).expect("a resource always serialises");
        // The revision on the copy we read is the compare-and-swap: if an agent
        // claimed the object in the meantime, that claim moved the revision and
        // this write is refused rather than overwriting it.
        Ok(Some(
            self.store
                .put(&key, bytes, Expect::Revision(previous.meta.revision))
                .await?,
        ))
    }
}

/// Everything in `meta` except the revision, which is where the object was read
/// from rather than part of it.
fn meta_changed<S, T>(a: &Resource<S, T>, b: &Resource<S, T>) -> bool {
    let (x, y) = (&a.meta, &b.meta);
    x.name != y.name
        || x.uid != y.uid
        || x.placement != y.placement
        || x.created_at != y.created_at
        || x.deleted_at != y.deleted_at
        || x.finalizers != y.finalizers
        || x.labels != y.labels
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{InstanceSpec, InstanceState, InstanceStatus},
    };
    use velstra_cloud_store::{MemoryStore, TypedStore};

    use super::*;

    type Instances = TypedStore<InstanceSpec, InstanceStatus>;
    type Writer = StatusWriter<InstanceSpec, InstanceStatus>;

    fn instance() -> Resource<InstanceSpec, InstanceStatus> {
        Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: Default::default(),
                console: false,
                devices: Vec::new(),
                vcpus: 2,
                ..Default::default()
            },
            InstanceStatus::default(),
        )
    }

    fn pair() -> (Instances, Writer, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        (
            TypedStore::new(store.clone(), "cell-1", "instances"),
            StatusWriter::new(store.clone(), "cell-1", "instances", "test"),
            store,
        )
    }

    #[tokio::test]
    async fn a_controller_may_speak_for_an_object_no_agent_owns() {
        let (typed, writer, _) = pair();
        typed
            .create(
                &instance(),
                &velstra_cloud_model::access::Writer::controller("status"),
            )
            .await
            .unwrap();
        let before = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();

        let mut next = before.clone();
        velstra_cloud_model::meta::set_condition(
            &mut next.status.conditions,
            velstra_cloud_model::Condition::new(
                "Ready",
                velstra_cloud_model::ConditionStatus::False,
                "NoValidHost",
                "nothing fits",
                1,
            ),
        );
        assert!(writer.write(&before, &next).await.unwrap().is_some());
        let after = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.status.conditions[0].reason, "NoValidHost");
    }

    #[tokio::test]
    async fn a_controller_is_refused_once_an_agent_owns_the_object() {
        // The bug this prevents is the oldest one in the book: two writers on
        // one status, and the object's state becomes a function of who wrote
        // last rather than of what is true.
        let (typed, writer, _) = pair();
        let mut i = instance();
        i.status.node = Some("node-a".into());
        typed
            .create(
                &i,
                &velstra_cloud_model::access::Writer::controller("status"),
            )
            .await
            .unwrap();
        let before = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();

        let mut next = before.clone();
        next.status.state = InstanceState::Failed;
        let err = writer.write(&before, &next).await.unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
    }

    #[tokio::test]
    async fn the_status_path_cannot_smuggle_a_spec_change() {
        let (typed, writer, _) = pair();
        typed
            .create(
                &instance(),
                &velstra_cloud_model::access::Writer::controller("status"),
            )
            .await
            .unwrap();
        let before = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();

        let mut next = before.clone();
        next.spec.vcpus = 64;
        next.status.state = InstanceState::Running;
        assert!(matches!(
            writer.write(&before, &next).await.unwrap_err(),
            Error::Refused(_)
        ));
    }

    #[tokio::test]
    async fn writing_what_is_already_there_writes_nothing() {
        let (typed, writer, store) = pair();
        typed
            .create(
                &instance(),
                &velstra_cloud_model::access::Writer::controller("status"),
            )
            .await
            .unwrap();
        let before = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();
        let revision = store.revision().await.unwrap();

        assert!(
            writer
                .write(&before, &before.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.revision().await.unwrap(),
            revision,
            "an unchanged status still advanced the store"
        );
    }

    #[tokio::test]
    async fn a_stale_copy_loses_rather_than_clobbering() {
        let (typed, writer, _) = pair();
        typed
            .create(
                &instance(),
                &velstra_cloud_model::access::Writer::controller("status"),
            )
            .await
            .unwrap();
        let stale = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();

        let mut first = stale.clone();
        first.status.observed_generation = 1;
        writer.write(&stale, &first).await.unwrap();

        let mut second = stale.clone();
        second.status.state = InstanceState::Running;
        let err = writer.write(&stale, &second).await.unwrap_err();
        assert!(
            err.is_conflict(),
            "a stale writer overwrote a newer status: {err}"
        );
    }
}
