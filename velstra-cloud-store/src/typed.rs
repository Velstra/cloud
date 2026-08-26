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
    access::{Changed, Ownership, Writer, judge, judge_create, judge_delete},
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
    /// An object stored in one cell that says it lives in another.
    ///
    /// Names both cells, because the two things worth knowing are which store
    /// this was read from and which one the object believes it belongs to —
    /// with those, the cause is usually one line of configuration.
    #[error(
        "{kind} {name} is stored in cell {here} and says it lives in cell {claimed}; \
         one of the two is misconfigured, and until it is fixed two cells may both \
         believe they own this object"
    )]
    Misplaced {
        kind: &'static str,
        name: String,
        here: String,
        claimed: String,
    },
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

    /// Which collection this is — `instances`, `nodes`, … Exposed so a writer
    /// that routes a report through the API rather than the store can name the
    /// collection it is writing to without a second source of that string.
    pub fn kind(&self) -> &'static str {
        self.kind
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

    /// One page, resuming strictly after the object called `after`.
    ///
    /// The resume point is a **resource name**, not a store key, and that is not
    /// cosmetic: the same walk may be answered from the store on one page and
    /// from the API's watch cache on the next, and the cache holds objects by
    /// name. A token carrying a key could not be honoured by the cache, and one
    /// carrying a name is honoured identically by both — the key is this prefix
    /// plus the name, so the two orderings are the same ordering.
    pub async fn list_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Resource<S, T>>, bool)> {
        let key;
        let after = match after {
            Some(name) => {
                key = self.key(name);
                Some(key.as_str())
            }
            None => None,
        };
        let page = self.store.list_page(&self.prefix(), after, limit).await?;
        let objects = page
            .entries
            .into_iter()
            .map(|e| self.decode(&e.value, e.revision))
            .collect::<Result<Vec<_>>>()?;
        Ok((objects, page.more))
    }

    fn decode(&self, bytes: &[u8], revision: Revision) -> Result<Resource<S, T>> {
        let mut r: Resource<S, T> =
            serde_json::from_slice(bytes).map_err(|source| TypedError::Corrupt {
                kind: self.kind,
                source,
            })?;
        // Where the object says it lives has to be where it was read from.
        //
        // `meta.placement` has been written on every object since the first
        // commit and read by **nothing**. A field that records where an object
        // lives and is never consulted is not placement information — it is a
        // claim, and the system behaves identically whether it is right or
        // wrong. This is the one place that can check it cheaply and for
        // everybody: the key already carries the cell, because this store builds
        // it, so the comparison needs no extra read and covers the API, the
        // controllers and every agent at once.
        //
        // The failure it catches is not hypothetical corruption. Point two API
        // processes with different `--cell` at one etcd — a plausible
        // configuration slip, and one nothing else would notice — and each
        // stamps its own cell on what it creates while both serve everything.
        // Two owners for one object, both writing, and a tenant in one cell
        // reading another's. Restoring a backup into the wrong cell is the same
        // shape and happens on purpose.
        //
        // Refused rather than skipped. A list that quietly dropped foreign
        // objects would hide the misconfiguration for as long as it took
        // somebody to notice missing machines, and a partial answer is what a
        // controller would then reconcile against.
        if r.meta.placement.cell != self.cell {
            return Err(TypedError::Misplaced {
                kind: self.kind,
                name: r.meta.name.to_string(),
                here: self.cell.clone(),
                claimed: r.meta.placement.cell.clone(),
            });
        }
        // The revision is where the object was read from, not part of it.
        r.meta.revision = revision;
        Ok(r)
    }

    /// Create, as `writer`. Fails if it already exists, so two racing creates
    /// cannot both believe they won.
    ///
    /// Judged like every other write rather than trusted: this used to write
    /// straight to the backend, so an agent could bring an object into being,
    /// which it never legitimately does — and which is the one way it could hand
    /// itself an object whose status already named it as owner, bypassing the
    /// claim ownership is otherwise earned through. That is now refused by
    /// [`velstra_cloud_model::access::judge_create`]: an agent cannot create, so
    /// it cannot create a pre-owned object either.
    pub async fn create(&self, resource: &Resource<S, T>, writer: &Writer) -> Result<Revision> {
        judge_create(writer)?;
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
            Ownership::of(previous.spec.assigned_owner(), previous.status.owner())
        };
        judge(writer, changed, held)?;

        let bytes = serde_json::to_vec(next).expect("a resource always serialises");
        Ok(self
            .store
            .put(&key, bytes, Expect::Revision(next.meta.revision))
            .await?)
    }

    /// Delete, as `writer`. Judged like every other write: a delete is a
    /// metadata decision, so only a controller may make one — an agent reports
    /// on objects and never asks for one to be gone, even the objects it runs.
    /// See [`velstra_cloud_model::access::judge_delete`].
    pub async fn delete(
        &self,
        name: &str,
        revision: Revision,
        writer: &Writer,
    ) -> Result<Revision> {
        judge_delete(writer)?;
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

    /// An object stored in one cell that claims another is refused, not served.
    ///
    /// This is the test that makes `meta.placement` mean something. It was
    /// written on every object from the first commit and read by nothing, so the
    /// system behaved identically whether it was right or wrong — and the way it
    /// goes wrong is not exotic: two API processes with different `--cell` on one
    /// etcd, or a backup restored into the wrong cell. Both leave two cells
    /// believing they own the same object, both writing to it, and each serving
    /// it to its own tenants.
    #[tokio::test]
    async fn an_object_that_says_it_lives_elsewhere_is_refused() {
        let raw = Arc::new(MemoryStore::new());
        // Written by "cell-2" — the same store, a differently configured writer.
        let elsewhere: Instances = TypedStore::new(raw.clone(), "cell-2", "instances");
        let mut theirs = instance();
        theirs.meta.placement = Placement::new("eu", "cell-2");
        elsewhere
            .create(
                &theirs,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();

        // Now read it back through a store that believes it is cell-2's — the
        // key matches, so nothing but the body says otherwise. It is served.
        assert!(
            elsewhere
                .get("projects/p1/instances/i1")
                .await
                .unwrap()
                .is_some(),
            "the cell that owns it cannot read its own object"
        );

        // And through cell-1, reading cell-2's key space directly, which is what
        // a misconfigured pair of processes amounts to.
        let ours: Instances = TypedStore::new(raw.clone(), "cell-1", "instances");
        let mut also_ours = instance();
        also_ours.meta.name = ResourceName::parse("projects/p1/instances/i2").unwrap();
        also_ours.meta.placement = Placement::new("eu", "cell-2");
        ours.create(
            &also_ours,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

        let error = ours
            .get("projects/p1/instances/i2")
            .await
            .expect_err("an object claiming another cell was served as if it were ours");
        let text = error.to_string();
        assert!(
            text.contains("cell-1") && text.contains("cell-2"),
            "the refusal has to name both cells or it is not actionable: {text}"
        );
    }

    #[tokio::test]
    async fn a_resource_round_trips_with_its_revision() {
        let s = store();
        let i = instance();
        s.create(&i, &velstra_cloud_model::access::Writer::controller("test"))
            .await
            .unwrap();
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
        s.create(&i, &velstra_cloud_model::access::Writer::controller("test"))
            .await
            .unwrap();
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
        s.create(&i, &velstra_cloud_model::access::Writer::controller("test"))
            .await
            .unwrap();
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
        s.create(&i, &velstra_cloud_model::access::Writer::controller("test"))
            .await
            .unwrap();
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
        s.create(&i, &velstra_cloud_model::access::Writer::controller("test"))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn an_agent_may_neither_create_nor_delete() {
        // The two write paths that used to bypass the judgement entirely. An
        // agent reports status on objects a controller made and assigned; it
        // never brings one into being and never asks for one to be gone. Both
        // are refused here, at the single write path, rather than trusted not to
        // happen — which is what makes a per-node token a boundary and not a note.
        let s = store();
        let i = instance();

        // Create as an agent: refused, and named as such. This is also the whole
        // of "a node cannot smuggle an ownership claim by creating a pre-owned
        // object" — it cannot create an object at all.
        let err = s.create(&i, &Writer::agent("node-a")).await.unwrap_err();
        assert!(
            matches!(
                err,
                TypedError::Refused(
                    velstra_cloud_model::access::WriteRefused::CreateIsNotYours { .. }
                )
            ),
            "an agent created an object: {err}"
        );

        // A controller makes it, and then an agent tries to delete it: refused.
        s.create(&i, &Writer::controller("api")).await.unwrap();
        let stored = s.get("projects/p1/instances/i1").await.unwrap().unwrap();
        let err = s
            .delete(
                "projects/p1/instances/i1",
                stored.meta.revision,
                &Writer::agent("node-a"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                TypedError::Refused(
                    velstra_cloud_model::access::WriteRefused::DeleteIsNotYours { .. }
                )
            ),
            "an agent deleted an object: {err}"
        );

        // And the controller may, so the object is not simply undeletable.
        let stored = s.get("projects/p1/instances/i1").await.unwrap().unwrap();
        s.delete(
            "projects/p1/instances/i1",
            stored.meta.revision,
            &Writer::controller("api"),
        )
        .await
        .unwrap();
    }
}
