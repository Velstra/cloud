//! Resources on top of bytes — and the one gate every write goes through.
//!
//! This is where invariant 1 stops being a rule people remember and becomes a
//! thing the system does: a write reads the stored copy, diffs it against the
//! new one, and hands the diff to [`velstra_cloud_model::access::judge`] along
//! with the identity of the writer. A controller that touches `status`, or an
//! agent that touches `spec`, is refused — regardless of which code path it
//! came from, and without that code path needing to be careful.

use std::marker::PhantomData;

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    access::{Changed, Ownership, Writer, judge},
    meta::Revision,
    resources::{Assigned, Observed, Resource},
};

use crate::{Expect, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum TypedError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Refused(#[from] velstra_cloud_model::access::WriteRefused),
    #[error("stored object is not valid {kind}: {source}")]
    Corrupt {
        kind: &'static str,
        source: serde_json::Error,
    },
    #[error("{0} does not exist")]
    Missing(String),
}

pub type Result<T> = std::result::Result<T, TypedError>;

/// One collection of one resource type, in one cell.
pub struct TypedStore<S, T> {
    store: std::sync::Arc<dyn Store>,
    cell: String,
    kind: &'static str,
    _marker: PhantomData<(S, T)>,
}

impl<S, T> Clone for TypedStore<S, T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            cell: self.cell.clone(),
            kind: self.kind,
            _marker: PhantomData,
        }
    }
}

impl<S, T> TypedStore<S, T>
where
    S: Serialize + DeserializeOwned + PartialEq + Send + Sync,
    T: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync,
{
    pub fn new(store: std::sync::Arc<dyn Store>, cell: &str, kind: &'static str) -> Self {
        Self {
            store,
            cell: cell.to_string(),
            kind,
            _marker: PhantomData,
        }
    }

    fn key(&self, name: &str) -> String {
        crate::key_for(&self.cell, self.kind, name)
    }

    pub fn prefix(&self) -> String {
        crate::prefix_for(&self.cell, self.kind)
    }

    pub async fn get(&self, name: &str) -> Result<Option<Resource<S, T>>> {
        let Some(entry) = self.store.get(&self.key(name)).await? else {
            return Ok(None);
        };
        Ok(Some(self.decode(&entry.value, entry.revision)?))
    }

    pub async fn list(&self) -> Result<Vec<Resource<S, T>>> {
        let entries = self.store.list(&self.prefix()).await?;
        entries
            .into_iter()
            .map(|e| self.decode(&e.value, e.revision))
            .collect()
    }

    fn decode(&self, bytes: &[u8], revision: Revision) -> Result<Resource<S, T>> {
        let mut r: Resource<S, T> =
            serde_json::from_slice(bytes).map_err(|source| TypedError::Corrupt {
                kind: self.kind,
                source,
            })?;
        // The revision is where the object was read from, not part of it.
        r.meta.revision = revision;
        Ok(r)
    }

    /// Create. Fails if it already exists, so two racing creates cannot both
    /// believe they won.
    pub async fn create(&self, resource: &Resource<S, T>) -> Result<Revision> {
        let key = self.key(&resource.meta.name.to_string());
        let bytes = serde_json::to_vec(resource).expect("a resource always serialises");
        Ok(self.store.put(&key, bytes, Expect::Absent).await?)
    }

    /// Write, as `writer`, refusing anything that changes the other party's
    /// half — and refusing to clobber a copy the caller has not seen.
    ///
    /// The revision comes from the object itself: whoever read it holds the
    /// version they are replacing, so a caller cannot accidentally write
    /// unconditionally by forgetting an argument.
    /// `S: Assigned` sits on this method rather than on the type, because only
    /// a write needs to know who the object was given to. Putting it on the
    /// impl block would make every reader — a controller listing, an agent
    /// watching — carry a bound it has no use for.
    pub async fn update(&self, next: &Resource<S, T>, writer: &Writer) -> Result<Revision>
    where
        S: Assigned,
    {
        let key = self.key(&next.meta.name.to_string());
        let current = self
            .store
            .get(&key)
            .await?
            .ok_or_else(|| TypedError::Missing(next.meta.name.to_string()))?;
        let previous: Resource<S, T> = self.decode(&current.value, current.revision)?;

        // Staleness is judged before ownership, and the order is the whole
        // point: a writer holding an old copy would otherwise be told its
        // *generation arithmetic* is wrong — which is true of the copy it holds
        // and irrelevant to what it should do. "You are behind, re-read" is the
        // only answer that leads anywhere.
        if next.meta.revision != current.revision {
            return Err(TypedError::Store(StoreError::Conflict {
                key,
                expected: next.meta.revision,
                actual: current.revision,
            }));
        }

        let changed = Changed {
            spec: previous.spec != next.spec,
            status: previous.status != next.status,
            meta: meta_changed(&previous, next),
            generation: previous.meta.generation != next.meta.generation,
        };
        // The owner is taken from what is *stored*, not from what is being
        // written — otherwise an agent could claim an object by asserting
        // ownership in the same write. A self-owned resource (a node) is its own
        // owner: nothing assigns a hypervisor to a hypervisor.
        let held = if previous.status.self_owned() {
            let itself = previous.meta.name.id();
            Ownership::of(Some(itself), Some(itself))
        } else {
            Ownership::of(previous.spec.assigned_node(), previous.status.owner_node())
        };
        judge(writer, changed, held)?;

        let bytes = serde_json::to_vec(next).expect("a resource always serialises");
        Ok(self
            .store
            .put(&key, bytes, Expect::Revision(next.meta.revision))
            .await?)
    }

    pub async fn delete(&self, name: &str, revision: Revision) -> Result<Revision> {
        Ok(self
            .store
            .delete(&self.key(name), Expect::Revision(revision))
            .await?)
    }

    /// Watch this collection. `from` makes list-then-watch race-free.
    pub fn watch(&self, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<crate::Event> {
        self.store.watch(&self.prefix(), from)
    }

    pub async fn revision(&self) -> Result<Revision> {
        Ok(self.store.revision().await?)
    }
}

/// Everything in `meta` except the revision, which is not part of the object.
fn meta_changed<S, T>(a: &Resource<S, T>, b: &Resource<S, T>) -> bool {
    let x = &a.meta;
    let y = &b.meta;
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
        meta::{Meta, Placement, ResourceName, Timestamp},
        resources::{InstanceSpec, InstanceState, InstanceStatus},
    };

    use super::*;
    use crate::MemoryStore;

    type Instances = TypedStore<InstanceSpec, InstanceStatus>;

    fn store() -> Instances {
        TypedStore::new(Arc::new(MemoryStore::new()), "cell-1", "instances")
    }

    fn instance() -> Resource<InstanceSpec, InstanceStatus> {
        Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                vcpus: 2,
                ..Default::default()
            },
            InstanceStatus::default(),
        )
    }

    #[tokio::test]
    async fn a_resource_round_trips_with_its_revision() {
        let s = store();
        let i = instance();
        s.create(&i).await.unwrap();
        let back = s.get("projects/p1/instances/i1").await.unwrap().unwrap();
        assert_eq!(back.spec, i.spec);
        assert!(
            back.meta.revision.0 > 0,
            "the read did not carry a revision"
        );
    }

    #[tokio::test]
    async fn a_controller_may_change_spec_and_an_agent_may_not() {
        let s = store();
        let mut i = instance();
        s.create(&i).await.unwrap();
        i = s.get("projects/p1/instances/i1").await.unwrap().unwrap();

        let mut edit = i.clone();
        edit.spec.vcpus = 4;
        edit.meta.generation += 1;
        s.update(&edit, &Writer::controller("api")).await.unwrap();

        let mut sneaky = s.get("projects/p1/instances/i1").await.unwrap().unwrap();
        sneaky.spec.vcpus = 8;
        sneaky.meta.generation += 1;
        let err = s
            .update(&sneaky, &Writer::agent("node-a"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, TypedError::Refused(_)),
            "an agent rewrote the spec: {err}"
        );
    }

    #[tokio::test]
    async fn an_agent_may_report_status_only_for_its_own_object() {
        let s = store();
        let mut i = instance();
        // First report claims the object for node-a. Ownership comes from the
        // *stored* status, so this must be established by a controller
        // assignment, not by the agent asserting it.
        i.status.node = Some("node-a".into());
        s.create(&i).await.unwrap();
        let stored = s.get("projects/p1/instances/i1").await.unwrap().unwrap();

        let mut mine = stored.clone();
        mine.status.state = InstanceState::Running;
        mine.status.observed_generation = 1;
        s.update(&mine, &Writer::agent("node-a")).await.unwrap();

        let stored = s.get("projects/p1/instances/i1").await.unwrap().unwrap();
        let mut theirs = stored.clone();
        theirs.status.state = InstanceState::Failed;
        let err = s
            .update(&theirs, &Writer::agent("node-b"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, TypedError::Refused(_)),
            "a second node reported on an object it does not own: {err}"
        );
    }

    #[tokio::test]
    async fn a_writer_holding_a_stale_copy_is_refused() {
        let s = store();
        let i = instance();
        s.create(&i).await.unwrap();
        let first = s.get("projects/p1/instances/i1").await.unwrap().unwrap();

        let mut a = first.clone();
        a.spec.vcpus = 4;
        a.meta.generation += 1;
        s.update(&a, &Writer::controller("api")).await.unwrap();

        // `first` is now two revisions old.
        let mut b = first.clone();
        b.spec.vcpus = 8;
        b.meta.generation += 1;
        let err = s.update(&b, &Writer::controller("api")).await.unwrap_err();
        assert!(
            matches!(err, TypedError::Store(StoreError::Conflict { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_deletion_request_is_a_meta_change_only_a_controller_makes() {
        let s = store();
        let mut i = instance();
        i.status.node = Some("node-a".into());
        s.create(&i).await.unwrap();
        let stored = s.get("projects/p1/instances/i1").await.unwrap().unwrap();

        let mut agent_delete = stored.clone();
        agent_delete.meta.deleted_at = Some(Timestamp::now());
        assert!(
            s.update(&agent_delete, &Writer::agent("node-a"))
                .await
                .is_err(),
            "a node deleted an object it merely runs"
        );

        let mut controller_delete = stored.clone();
        controller_delete.meta.deleted_at = Some(Timestamp::now());
        s.update(&controller_delete, &Writer::controller("api"))
            .await
            .unwrap();
    }
}
