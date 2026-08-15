//! The one place state lives, behind a trait narrow enough to swap.
//!
//! Everything above this crate sees `get / list / watch / compare-and-swap /
//! delete` and an opaque revision. That is deliberately the intersection of what
//! etcd and FoundationDB can both do well: etcd is the first backend because its
//! watch semantics are exactly this shape, and the day a cell outgrows it, the
//! replacement is a file in this crate rather than a rewrite of everything that
//! reads state.
//!
//! What must never leak upward: an etcd lease, a FoundationDB transaction
//! handle, a key layout, or the revision's arithmetic. A caller that computes
//! `revision + 1` has already made the swap impossible.

use std::collections::BTreeMap;

use async_trait::async_trait;
use velstra_cloud_model::meta::Revision;

pub mod memory;
pub mod typed;

pub use memory::MemoryStore;
pub use typed::TypedStore;

/// One stored object, as bytes plus the revision it was read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    pub revision: Revision,
}

/// What the caller believes about the current state, so a write can refuse to
/// clobber a change it has not seen.
///
/// There is no unconditional write on purpose. Every writer either knows it is
/// creating (`Absent`), knows which version it is replacing (`Revision`), or has
/// explicitly said it does not care (`Any`) — and that last one is a decision
/// somebody had to type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    Absent,
    Revision(Revision),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Put(Entry),
    Delete { key: String, revision: Revision },
}

impl Event {
    pub fn key(&self) -> &str {
        match self {
            Self::Put(e) => &e.key,
            Self::Delete { key, .. } => key,
        }
    }

    pub fn revision(&self) -> Revision {
        match self {
            Self::Put(e) => e.revision,
            Self::Delete { revision, .. } => *revision,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{key} exists")]
    Exists { key: String },
    #[error("{key} is at revision {actual}, not {expected}")]
    Conflict {
        key: String,
        expected: Revision,
        actual: Revision,
    },
    #[error("{key} does not exist")]
    Missing { key: String },
    /// The watcher fell behind and the events it missed are gone. The only
    /// correct response is a full re-list — which every controller here does
    /// anyway on its resync timer, so this is a hint, not a crisis.
    #[error("watch fell behind at revision {from}")]
    Compacted { from: Revision },
    #[error("{0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The whole contract. Deliberately small.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Option<Entry>>;

    /// Everything under a prefix, in key order. Callers page by key, never by
    /// offset — an offset into a changing collection skips objects.
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>>;

    /// Write, subject to `expect`. Returns the new revision.
    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision>;

    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision>;

    /// Changes under a prefix, starting after `from` (or from now, if `None`).
    ///
    /// The receiver is a plain channel: a watcher that stops reading is dropped
    /// rather than allowed to grow a queue inside the store. Falling behind is
    /// survivable — the periodic resync catches it — and unbounded memory in
    /// the one process that holds all the state is not.
    fn watch(&self, prefix: &str, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event>;

    /// The store's current revision, for a watcher that wants to list first and
    /// then watch from exactly where the list ended.
    async fn revision(&self) -> Result<Revision>;
}

/// How a resource name becomes a key. One function, so a key layout change is
/// one edit and not a search across the codebase.
///
/// The cell leads the key because a cell is the shard: when there are two, the
/// prefix is what routes a request without parsing the rest.
pub fn key_for(cell: &str, kind: &str, name: &str) -> String {
    format!("/{cell}/{kind}/{name}")
}

pub fn prefix_for(cell: &str, kind: &str) -> String {
    format!("/{cell}/{kind}/")
}

/// Split a key back into its parts. Returns `None` for anything this crate did
/// not write.
pub fn parse_key(key: &str) -> Option<(&str, &str, &str)> {
    let rest = key.strip_prefix('/')?;
    let (cell, rest) = rest.split_once('/')?;
    let (kind, name) = rest.split_once('/')?;
    if cell.is_empty() || kind.is_empty() || name.is_empty() {
        return None;
    }
    Some((cell, kind, name))
}

/// A snapshot of a prefix, for a caller that wants a map rather than a list.
pub fn as_map(entries: Vec<Entry>) -> BTreeMap<String, Entry> {
    entries.into_iter().map(|e| (e.key.clone(), e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_leads_with_the_cell_because_the_cell_is_the_shard() {
        let k = key_for("cell-1", "instances", "projects/p1/instances/i1");
        assert_eq!(k, "/cell-1/instances/projects/p1/instances/i1");
        let (cell, kind, name) = parse_key(&k).unwrap();
        assert_eq!((cell, kind), ("cell-1", "instances"));
        assert_eq!(name, "projects/p1/instances/i1");
    }

    #[test]
    fn a_prefix_ends_with_a_separator_so_it_cannot_swallow_a_sibling() {
        // Without the trailing slash, `instances` would also match
        // `instances-archive`, and a controller would reconcile objects that
        // are not its own.
        assert_eq!(prefix_for("cell-1", "instances"), "/cell-1/instances/");
    }

    #[test]
    fn a_foreign_key_is_not_parsed_into_nonsense() {
        assert!(parse_key("nonsense").is_none());
        assert!(parse_key("/cell-1/instances").is_none());
        assert!(parse_key("//instances/x").is_none());
    }
}
