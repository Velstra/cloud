//! Ten resource types, one set of handlers.
//!
//! The handlers above this file do not know what an instance is. They know that
//! a collection can be read, listed, created into, patched, deleted from and
//! watched — and that every object in it is `meta`/`spec`/`status`. That is why
//! there is one implementation of "generation moves iff spec changed" rather
//! than ten, and why adding a resource type is a line in a registry rather than
//! a new REST handler that will drift from its siblings.
//!
//! Erasure is at the JSON boundary and nowhere else: a document is deserialised
//! into the real typed resource before anything is decided about it, so the
//! comparison that drives `generation` is the model's `PartialEq` and not a
//! guess about how two JSON objects relate.

use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use velstra_cloud_model::{
    access::Writer,
    meta::{Revision, Timestamp},
    reconcile::may_delete,
    resources::{Assigned, Observed, Resource},
};
use velstra_cloud_store::{Event, Store, TypedStore};

use crate::error::{ApiError, ApiResult, Code};

/// The identity every write from this API is made under.
///
/// The API is a controller: it writes `spec` and metadata on an operator's
/// behalf, and it may not write `status` — the store refuses it, and this is
/// the name that appears in the refusal.
pub const API_WRITER: &str = "api";

/// How many times a write that said "last writer wins" will re-read and try
/// again before giving up. Small on purpose: this is for the ordinary case of
/// two writers meeting, not for an object somebody is hammering.
const ATTEMPTS: usize = 4;

/// What a client asked to change. Only the two halves it owns.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub spec: Option<Value>,
    pub labels: Option<Value>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.spec.is_none() && self.labels.is_none()
    }
}

/// The outcome of a delete: the object as it now stands, and whether it is
/// really gone.
pub struct Deleted {
    pub resource: Value,
    pub gone: bool,
}

/// One collection, with its types erased. Every document in and out is
/// model-shaped JSON — the wire spelling is applied a layer up.
#[async_trait]
pub trait Collection: Send + Sync {
    /// Which collection this is, for the read paths that decorate a document
    /// with something computed rather than stored.
    fn kind(&self) -> &'static str;

    async fn get(&self, name: &str) -> ApiResult<Option<Value>>;
    async fn list(&self) -> ApiResult<Vec<Value>>;

    /// One page of the collection, resuming strictly after the object named
    /// `after`, and whether anything follows it.
    ///
    /// Separate from `list` rather than a parameter on it because the callers
    /// differ in kind: a controller reconciling a collection wants all of it and
    /// would only re-implement this loop, while an API answering a person wants
    /// a bounded read. Both are legitimate; conflating them is what produced an
    /// API whose cost grew with the cell.
    async fn list_page(&self, after: Option<&str>, limit: usize) -> ApiResult<(Vec<Value>, bool)>;

    /// This collection's spec with every field at its default.
    ///
    /// A create merges the client's `spec` onto this rather than deserialising
    /// what arrived: a client that sends three of an instance's ten fields
    /// means "the rest as they come", and the alternative is an API where
    /// creating a machine requires spelling out every field the model has ever
    /// grown.
    fn empty_spec(&self) -> Value;

    /// Check that a complete spec really is one, naming the field if it is not.
    ///
    /// Separate from `create` because everything a create does before writing —
    /// counting quota, above all — has to read the spec as its real type, and
    /// whichever of those touches it first would otherwise be the one to report
    /// the failure, in its own words and without the field.
    fn check_spec(&self, spec: &Value) -> ApiResult<()>;

    /// Refuse a field this platform has never heard of.
    ///
    /// Serde ignores what it does not recognise, which is the right default for
    /// **reading stored objects** — a field removed from the code must not make
    /// yesterday's data unreadable — and precisely the wrong answer at the
    /// door. An operator who sets `memoryOvercommit: 2`, is answered `200`, and
    /// goes home believing memory is overcommitted has been told something
    /// untrue by a success.
    ///
    /// So the strictness lives here, at the boundary, and not on the types.
    fn check_known(&self, sent: &Value) -> ApiResult<()>;

    /// Create from a complete `meta` and a complete `spec`.
    async fn create(&self, meta: Value, spec: Value) -> ApiResult<Value>;
    async fn patch(&self, name: &str, patch: &Patch, expect: Option<Revision>) -> ApiResult<Value>;
    async fn delete(&self, name: &str, expect: Option<Revision>) -> ApiResult<Deleted>;

    /// Write only the `status` of an object, as `writer` — the one write a node
    /// agent makes, and the seam that makes `--api` mode a real trust boundary.
    ///
    /// The stored `spec` and `meta` are kept verbatim: an agent that sent a
    /// different spec through this path changes nothing, because this reads the
    /// stored object and overlays only the incoming status onto it. The write
    /// then goes through the same `store.update` every writer uses, so
    /// [`velstra_cloud_model::access::judge`] does the enforcing — a node may
    /// write status only of an object it owns or was assigned, and touching spec
    /// or another node's object is refused there, in one place, rather than
    /// re-checked here.
    async fn report_status(
        &self,
        name: &str,
        status: &Value,
        expect: Option<Revision>,
        writer: &Writer,
    ) -> ApiResult<Value>;

    fn watch(&self, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event>;

    /// Decode what a watch handed over. Bytes rather than a document because a
    /// store event carries the stored form, and this is the only place that
    /// knows which type it is.
    fn decode(&self, bytes: &[u8], revision: Revision) -> ApiResult<Value>;

    /// Current store revision, for a list that will be watched from.
    async fn revision(&self) -> ApiResult<Revision>;
}

pub struct TypedCollection<S, T> {
    store: TypedStore<S, T>,
    kind: &'static str,
    _marker: PhantomData<(S, T)>,
}

impl<S, T> TypedCollection<S, T>
where
    S: Serialize + DeserializeOwned + Default + PartialEq + Assigned + Send + Sync,
    T: Serialize + DeserializeOwned + Default + PartialEq + Observed + Send + Sync,
{
    pub fn new(store: Arc<dyn Store>, cell: &str, kind: &'static str) -> Self {
        Self {
            store: TypedStore::new(store, cell, kind),
            kind,
            _marker: PhantomData,
        }
    }

    fn document(resource: &Resource<S, T>) -> ApiResult<Value> {
        serde_json::to_value(resource)
            .map_err(|e| ApiError::internal(format!("a stored object could not be rendered: {e}")))
    }

    async fn read(&self, name: &str) -> ApiResult<Resource<S, T>> {
        self.store
            .get(name)
            .await?
            .ok_or_else(|| ApiError::not_found(name))
    }

    /// One attempt at a change: read, merge, and compare-and-swap.
    async fn patch_once(
        &self,
        name: &str,
        patch: &Patch,
        expect: Option<Revision>,
    ) -> ApiResult<Value> {
        let stored = self.read(name).await?;
        if let Some(expected) = expect {
            // Checked here rather than left to the store, because the store
            // would compare the revision of whatever this handler assembled —
            // which is by construction the current one. The client's belief is
            // the only one worth testing.
            if expected != stored.meta.revision {
                return Err(ApiError::conflict(stored.meta.revision));
            }
        }

        let mut document = Self::document(&stored)?;
        // What was there before the merge: it deserialised once, so it is the
        // known-good copy a failure is measured against.
        let before = document["spec"].clone();
        if let Some(spec) = &patch.spec {
            merge(
                document
                    .get_mut("spec")
                    .expect("a resource always has a spec"),
                spec,
            );
        }
        if let Some(labels) = &patch.labels {
            let meta = document
                .get_mut("meta")
                .expect("a resource always has meta");
            merge(
                meta.get_mut("labels").expect("meta always has labels"),
                labels,
            );
        }

        let merged = document["spec"].clone();
        let mut next: Resource<S, T> =
            serde_json::from_value(document).map_err(|e| blame::<S>(&merged, &before, e))?;
        next.meta.revision = stored.meta.revision;

        let spec_changed = next.spec != stored.spec;
        let labels_changed = next.meta.labels != stored.meta.labels;
        if !spec_changed && !labels_changed {
            // An identical PATCH is a success with nothing behind it. Writing
            // anyway would move the revision and wake every watcher in the cell
            // for a change nobody made.
            return Self::document(&stored);
        }
        if spec_changed {
            next.meta.generation += 1;
        }

        let revision = self
            .store
            .update(&next, &Writer::controller(API_WRITER))
            .await?;
        next.meta.revision = revision;
        Self::document(&next)
    }

    async fn delete_once(&self, name: &str, expect: Option<Revision>) -> ApiResult<Deleted> {
        let mut resource = self.read(name).await?;
        if let Some(expected) = expect {
            if expected != resource.meta.revision {
                return Err(ApiError::conflict(resource.meta.revision));
            }
        }

        // Asking twice is asking once: the stamp goes on only if it is not
        // already there, so a retried delete does not move the timestamp an
        // operator is reading as "since when has this been going".
        if !resource.meta.is_deleting() {
            resource.meta.deleted_at = Some(Timestamp::now());
            let revision = self
                .store
                .update(&resource, &Writer::controller(API_WRITER))
                .await?;
            resource.meta.revision = revision;
        }

        // Nothing holds it: there is no second phase to wait for, and leaving
        // the object behind for a controller to reap would mean a delete that
        // never completes in a cell whose controllers are down.
        let gone = may_delete(&resource.meta);
        if gone {
            self.store
                .delete(
                    name,
                    resource.meta.revision,
                    &velstra_cloud_model::access::Writer::controller("collection"),
                )
                .await?;
        }
        Ok(Deleted {
            resource: Self::document(&resource)?,
            gone,
        })
    }
}

#[async_trait]
impl<S, T> Collection for TypedCollection<S, T>
where
    S: Serialize + DeserializeOwned + Default + PartialEq + Assigned + Send + Sync + 'static,
    T: Serialize + DeserializeOwned + Default + PartialEq + Observed + Send + Sync + 'static,
{
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn get(&self, name: &str) -> ApiResult<Option<Value>> {
        match self.store.get(name).await? {
            Some(r) => Ok(Some(Self::document(&r)?)),
            None => Ok(None),
        }
    }

    async fn list(&self) -> ApiResult<Vec<Value>> {
        self.store
            .list()
            .await?
            .iter()
            .map(Self::document)
            .collect()
    }

    async fn list_page(&self, after: Option<&str>, limit: usize) -> ApiResult<(Vec<Value>, bool)> {
        let (objects, more) = self.store.list_page(after, limit).await?;
        let documents = objects
            .iter()
            .map(Self::document)
            .collect::<ApiResult<Vec<_>>>()?;
        Ok((documents, more))
    }

    fn empty_spec(&self) -> Value {
        serde_json::to_value(S::default()).expect("a spec always serialises")
    }

    fn check_spec(&self, spec: &Value) -> ApiResult<()> {
        match serde_json::from_value::<S>(spec.clone()) {
            Ok(_) => Ok(()),
            Err(e) => Err(blame::<S>(spec, &self.empty_spec(), e)),
        }
    }

    fn check_known(&self, sent: &Value) -> ApiResult<()> {
        let Some(fields) = sent.as_object() else {
            return Ok(());
        };
        // What the type knows, in both directions: the fields a default value
        // renders, plus the fields that survive a round trip of what was sent.
        // The second half matters because a field marked "skip when empty" is
        // absent from the first — and a patch that sets exactly such a field
        // would otherwise be refused for naming a field it does have.
        let mut merged = self.empty_spec();
        overlay(&mut merged, sent);
        let echoed = serde_json::from_value::<S>(merged)
            .ok()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or_else(|| json!({}));
        let known = |key: &str| {
            echoed.get(key).is_some() || self.empty_spec().get(key).is_some()
        };

        for (key, value) in fields {
            if known(key) {
                continue;
            }
            // An unknown field carrying nothing is somebody echoing back an
            // object, or clearing something: no intention was lost, so there is
            // nothing to tell them about. What is refused is a field somebody
            // *set*, because that is the one where saying `200` would be
            // agreeing to something that will not happen.
            if is_nothing(value) {
                continue;
            }
            return Err(ApiError::invalid(format!(
                "there is no field called {key} on a {}; nothing would have been done with it",
                self.kind()
            ))
            .at(format!("spec.{key}")));
        }
        Ok(())
    }

    async fn create(&self, meta: Value, spec: Value) -> ApiResult<Value> {
        let document = json!({ "meta": meta, "spec": spec, "status": T::default() });
        let mut resource: Resource<S, T> = serde_json::from_value(document)
            .map_err(|e| blame::<S>(&spec, &self.empty_spec(), e))?;
        let revision = self
            .store
            .create(
                &resource,
                &velstra_cloud_model::access::Writer::controller("collection"),
            )
            .await?;
        resource.meta.revision = revision;
        Self::document(&resource)
    }

    async fn patch(&self, name: &str, patch: &Patch, expect: Option<Revision>) -> ApiResult<Value> {
        let mut last = None;
        for _ in 0..ATTEMPTS {
            match self.patch_once(name, patch, expect).await {
                Err(e) if retryable(&e, expect) => last = Some(e),
                answer => return answer,
            }
        }
        Err(last.expect("a loop that fell through has an error"))
    }

    async fn delete(&self, name: &str, expect: Option<Revision>) -> ApiResult<Deleted> {
        let mut last = None;
        for _ in 0..ATTEMPTS {
            match self.delete_once(name, expect).await {
                Err(e) if retryable(&e, expect) => last = Some(e),
                answer => return answer,
            }
        }
        Err(last.expect("a loop that fell through has an error"))
    }

    async fn report_status(
        &self,
        name: &str,
        status: &Value,
        expect: Option<Revision>,
        writer: &Writer,
    ) -> ApiResult<Value> {
        let stored = self.read(name).await?;
        if let Some(expected) = expect {
            // The agent's own compare-and-swap: it read the object, acted, and is
            // writing the status it observed against the revision it read. A
            // mismatch means the object moved under it, and the next pass redoes
            // the work against the new one — exactly as a direct-store report does.
            if expected != stored.meta.revision {
                return Err(ApiError::conflict(stored.meta.revision));
            }
        }
        // Overlay only the status onto the stored object. The spec and metadata
        // are kept as stored, so an agent cannot change them here whatever it
        // sends — and `judge` refuses the write outright if the overlaid status
        // is not this agent's to write.
        let mut document = Self::document(&stored)?;
        document["status"] = status.clone();
        let mut next: Resource<S, T> = serde_json::from_value(document).map_err(|e| {
            ApiError::invalid(format!("that is not a valid status: {e}")).at("status")
        })?;
        next.meta.revision = stored.meta.revision;
        if next.status == stored.status {
            // An unchanged report writes nothing, the same quiet a converged
            // agent has against the store directly.
            return Self::document(&stored);
        }
        let revision = self.store.update(&next, writer).await?;
        next.meta.revision = revision;
        Self::document(&next)
    }

    fn watch(&self, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event> {
        self.store.watch(from)
    }

    fn decode(&self, bytes: &[u8], revision: Revision) -> ApiResult<Value> {
        let mut resource: Resource<S, T> = serde_json::from_slice(bytes).map_err(|e| {
            ApiError::internal(format!("a stored {} could not be read: {e}", self.kind))
        })?;
        resource.meta.revision = revision;
        Self::document(&resource)
    }

    async fn revision(&self) -> ApiResult<Revision> {
        Ok(self.store.revision().await?)
    }
}

/// Name the field that broke a spec.
///
/// `serde_json` says what went wrong — "invalid type: string, expected u32" —
/// but not where, and "somewhere in your body" is a refusal an operator cannot
/// act on: it lands in a banner instead of on the control that caused it. So
/// each key the caller touched is put back to a value that is known to
/// deserialise, in turn, and the one that fixes it is the one to blame.
///
/// Only on the error path, and only until it finds the culprit. If two fields
/// are wrong at once no single swap fixes it, and the caller gets the plain
/// message — which is exactly what it would have got anyway.
/// Copy `patch` onto `into`, one level at a time, keeping keys neither knows.
///
/// Unknown keys are kept deliberately: they are the whole point of the caller.
fn overlay(into: &mut Value, patch: &Value) {
    let (Some(into), Some(patch)) = (into.as_object_mut(), patch.as_object()) else {
        *into = patch.clone();
        return;
    };
    for (key, value) in patch {
        match into.get_mut(key) {
            Some(existing) if existing.is_object() && value.is_object() => overlay(existing, value),
            _ => {
                into.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Whether a value carries no intention: absent, empty, or zero.
fn is_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
    }
}

fn blame<S: DeserializeOwned>(
    spec: &Value,
    known_good: &Value,
    error: serde_json::Error,
) -> ApiError {
    let plain = ApiError::from(error);
    let (Some(sent), Some(good)) = (spec.as_object(), known_good.as_object()) else {
        return plain;
    };
    for key in sent.keys() {
        let Some(fallback) = good.get(key) else {
            continue;
        };
        let mut probe = spec.clone();
        probe[key] = fallback.clone();
        if serde_json::from_value::<S>(probe).is_ok() {
            return plain.at(format!("spec.{}", crate::json::to_camel(key)));
        }
    }
    plain
}

/// Whether losing a compare-and-swap is this caller's problem.
///
/// A client that sent an `If-Match` asked to be told, and the conflict is the
/// answer. A client that did not asked for last-writer-wins — and it would be a
/// poor sort of last-writer-wins that failed because somebody else wrote in
/// between this handler's read and its write.
fn retryable(error: &ApiError, expect: Option<Revision>) -> bool {
    expect.is_none() && error.code == Code::Aborted
}

/// RFC 7386 merge, restricted to what a spec can be.
///
/// `null` removes a key rather than setting it to null, so a client can clear
/// an optional field — `"userData": null` — without a second verb for it. Only
/// objects merge; an array is replaced whole, because a list of ports has an
/// order and merging positionally would silently reorder somebody's NICs.
pub(crate) fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                    continue;
                }
                match target.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_null_clears_a_field_rather_than_setting_it_to_null() {
        // Without this a client would need a separate "unset" verb to take
        // cloud-init data off an instance, and would end up sending an empty
        // string instead — which is a different thing entirely.
        let mut target = json!({ "vcpus": 2, "user_data": "#cloud-config" });
        merge(&mut target, &json!({ "user_data": null }));
        assert_eq!(target, json!({ "vcpus": 2 }));
    }

    #[test]
    fn a_list_is_replaced_whole_because_its_order_is_meaning() {
        let mut target = json!({ "ports": ["a", "b", "c"] });
        merge(&mut target, &json!({ "ports": ["c", "a"] }));
        assert_eq!(target, json!({ "ports": ["c", "a"] }));
    }

    #[test]
    fn an_untouched_field_survives_a_patch() {
        let mut target = json!({ "vcpus": 2, "memory_mib": 4096 });
        merge(&mut target, &json!({ "vcpus": 4 }));
        assert_eq!(target, json!({ "vcpus": 4, "memory_mib": 4096 }));
    }

    #[test]
    fn a_conflict_is_retried_only_for_a_writer_that_asked_for_last_writer_wins() {
        let conflict = ApiError::conflict(Revision(7));
        assert!(retryable(&conflict, None));
        assert!(
            !retryable(&conflict, Some(Revision(6))),
            "an If-Match was quietly ignored"
        );
        assert!(!retryable(&ApiError::invalid("no"), None));
    }
}
