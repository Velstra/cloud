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

pub mod cache;
pub mod etcd;
pub mod memory;
pub mod typed;

pub use cache::Cached;
pub use etcd::EtcdStore;
pub use memory::MemoryStore;
pub use typed::TypedStore;

/// How far a watcher may fall behind before the store gives up on it.
///
/// Public because it is part of the contract rather than a tuning knob: every
/// backend queues exactly this much and then drops the watcher, and a test that
/// wants to prove that has to know the number to exceed it.
pub const WATCH_QUEUE: usize = 1024;

/// One stored object, as bytes plus the revision it was read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    pub revision: Revision,
}

/// One page of a prefix scan.
///
/// `more` is the answer to "is there anything after this", and it is a separate
/// field rather than something the caller infers from `entries.len() == limit`:
/// a collection whose size is an exact multiple of the page size would otherwise
/// always report one page too many, and the caller would ask for a page that
/// comes back empty. Small, but it is the difference between a client that stops
/// and one that makes a pointless round trip every single time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub more: bool,
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

    /// One page of a prefix, in key order, starting strictly after `after`.
    ///
    /// `list` answers a whole collection, which is right for a controller that
    /// reconciles it and wrong for an API serving a person: a cell of ten
    /// thousand instances is ten thousand objects built, computed and serialised
    /// to answer "show me the first twenty". This is the same scan, bounded.
    ///
    /// **Keys, not offsets.** An offset into a collection that is being written
    /// to skips objects — delete the tenth while somebody holds "give me from
    /// 10" and the eleventh is never seen by anybody. A key resumes exactly where
    /// the last page stopped whatever happened in between.
    ///
    /// **What paging across pages does and does not promise.** A page is a
    /// consistent read; a sequence of pages is not one. An object created before
    /// the resume key after an earlier page was read will not appear, and one
    /// deleted may still. That is deliberate rather than a shortcut: holding a
    /// snapshot open across client round-trips means holding a revision the
    /// backend is free to compact, and answering `410 Gone` to a caller whose
    /// token got old — the cost Kubernetes pays for the guarantee. The
    /// list-then-watch that actually needs consistency stays correct without it,
    /// because the caller watches from the revision the *first* page reported and
    /// the watch replays everything since; events carry whole objects, so
    /// applying them over a slightly-torn list converges.
    ///
    /// **Deliberately without a default implementation.** One in terms of `list`
    /// would be correct and would cost exactly what `list` costs — so a store
    /// that wraps another (every counting, failing or delaying decorator in the
    /// test suites) would inherit it, quietly reading the whole collection while
    /// the caller believed it was reading a page. The measurement would then
    /// report the cost of the thing it was written to prove was gone. Requiring
    /// the method makes forgetting to forward it a compile error instead.
    async fn list_page(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Page>;

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
    // `kind` repeats what the name already says — `/cell-1/nodes/nodes/node-a`,
    // `/cell-1/instances/projects/p1/instances/i1` — and that looks like a wart
    // until you try to remove it. A resource name carries its collection in the
    // *middle* (`projects/{p}/instances/{i}`), so it is not a prefix anything
    // can scan for. This segment is what makes `list` and `watch` a single
    // range read instead of a walk over the whole cell. The repetition is the
    // price of the prefix, and the prefix is the point.
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
