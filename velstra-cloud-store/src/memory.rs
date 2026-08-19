//! An in-process store with real MVCC semantics.
//!
//! Not a mock. It is the store the whole platform runs on in tests and in a
//! single-node development cell, so it has to behave exactly like the etcd
//! backend where it matters: monotonic revisions, compare-and-swap that refuses
//! stale writes, and watches that deliver in revision order from a point the
//! caller chooses. A test that passes here and fails on etcd would mean this
//! file is lying, so the conformance suite in `tests/` runs against both.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::mpsc;
use velstra_cloud_model::meta::Revision;

use crate::{Entry, Event, Expect, Page, Result, Store, StoreError, WATCH_QUEUE};

#[derive(Default)]
struct Inner {
    /// Key order matters: `list` returns sorted, which is what makes paging by
    /// key stable while the collection changes underneath.
    data: BTreeMap<String, Entry>,
    revision: u64,
    /// Every live watcher, with the prefix it asked for. Dropped receivers are
    /// reaped on the next send rather than tracked separately.
    watchers: Vec<(String, mpsc::Sender<Event>)>,
    /// Recent events, so a watcher that asks for a past revision gets what it
    /// missed instead of a silent gap. Bounded: beyond this the honest answer
    /// is `Compacted`, and the caller re-lists.
    history: Vec<Event>,
}

const HISTORY: usize = 4096;

#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn publish(inner: &mut Inner, event: Event) {
        inner.history.push(event.clone());
        if inner.history.len() > HISTORY {
            let drop = inner.history.len() - HISTORY;
            inner.history.drain(..drop);
        }
        inner.watchers.retain(|(prefix, tx)| {
            !event.key().starts_with(prefix.as_str()) || tx.try_send(event.clone()).is_ok()
        });
    }
}

impl MemoryStore {
    /// How many watchers this store is currently feeding.
    ///
    /// Exists so a test can assert the claim the watch cache makes: that one
    /// watch upstream serves however many readers downstream. Nothing else can
    /// see the difference between one watcher and a thousand.
    pub fn watchers(&self) -> usize {
        self.inner.lock().unwrap().watchers.len()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        Ok(self.inner.lock().unwrap().data.get(key).cloned())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .data
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.clone())
            .collect())
    }

    async fn list_page(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Page> {
        let inner = self.inner.lock().unwrap();
        // A BTreeMap range, so the scan starts at the resume key rather than at
        // the front of the collection. Sliced-after-listing would give the same
        // answer and re-read the whole cell to do it, which is the cost this
        // exists to remove — and a dev cell that behaves differently from a
        // production one is a dev cell that hides exactly this class of defect.
        let start = match after {
            // `String` compares by bytes, the same order etcd ranges in, so the
            // two backends page identically. The excluded bound is what makes
            // `after` mean "strictly after".
            Some(after) => std::ops::Bound::Excluded(after.to_string()),
            None => std::ops::Bound::Included(prefix.to_string()),
        };
        // One past the limit, purely to answer `more` without a second query.
        let mut entries: Vec<Entry> = inner
            .data
            .range((start, std::ops::Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(prefix))
            .take(limit + 1)
            .map(|(_, v)| v.clone())
            .collect();
        let more = entries.len() > limit;
        entries.truncate(limit);
        Ok(Page { entries, more })
    }

    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision> {
        let mut inner = self.inner.lock().unwrap();
        let current = inner.data.get(key).map(|e| e.revision);
        match (expect, current) {
            (Expect::Absent, Some(_)) => {
                return Err(StoreError::Exists {
                    key: key.to_string(),
                });
            }
            (Expect::Revision(want), Some(actual)) if want != actual => {
                return Err(StoreError::Conflict {
                    key: key.to_string(),
                    expected: want,
                    actual,
                });
            }
            (Expect::Revision(want), None) => {
                return Err(StoreError::Conflict {
                    key: key.to_string(),
                    expected: want,
                    actual: Revision(0),
                });
            }
            _ => {}
        }
        inner.revision += 1;
        let entry = Entry {
            key: key.to_string(),
            value,
            revision: Revision(inner.revision),
        };
        inner.data.insert(key.to_string(), entry.clone());
        Self::publish(&mut inner, Event::Put(entry));
        Ok(Revision(inner.revision))
    }

    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision> {
        let mut inner = self.inner.lock().unwrap();
        let current = match inner.data.get(key) {
            Some(e) => e.revision,
            None => {
                return Err(StoreError::Missing {
                    key: key.to_string(),
                });
            }
        };
        if let Expect::Revision(want) = expect {
            if want != current {
                return Err(StoreError::Conflict {
                    key: key.to_string(),
                    expected: want,
                    actual: current,
                });
            }
        }
        inner.revision += 1;
        inner.data.remove(key);
        let revision = Revision(inner.revision);
        Self::publish(
            &mut inner,
            Event::Delete {
                key: key.to_string(),
                revision,
            },
        );
        Ok(revision)
    }

    fn watch(&self, prefix: &str, from: Option<Revision>) -> mpsc::Receiver<Event> {
        // Bounded on purpose: see the note on `Store::watch`. A slow watcher is
        // disconnected, and its controller re-lists on the next resync.
        let (tx, rx) = mpsc::channel(WATCH_QUEUE);
        let mut inner = self.inner.lock().unwrap();
        if let Some(from) = from {
            for event in inner
                .history
                .iter()
                .filter(|e| e.revision() > from && e.key().starts_with(prefix))
                .cloned()
                .collect::<Vec<_>>()
            {
                let _ = tx.try_send(event);
            }
        }
        inner.watchers.push((prefix.to_string(), tx));
        rx
    }

    async fn revision(&self) -> Result<Revision> {
        Ok(Revision(self.inner.lock().unwrap().revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn put(s: &MemoryStore, k: &str, v: &str, e: Expect) -> Result<Revision> {
        s.put(k, v.as_bytes().to_vec(), e).await
    }

    #[tokio::test]
    async fn a_revision_moves_forward_and_never_repeats() {
        let s = MemoryStore::new();
        let a = put(&s, "/c/k/a", "1", Expect::Absent).await.unwrap();
        let b = put(&s, "/c/k/b", "1", Expect::Absent).await.unwrap();
        assert!(b > a, "two writes shared a revision");
        let d = s.delete("/c/k/a", Expect::Any).await.unwrap();
        assert!(d > b, "a delete did not advance the revision");
    }

    #[tokio::test]
    async fn a_stale_writer_is_refused_rather_than_winning() {
        let s = MemoryStore::new();
        let first = put(&s, "/c/k/a", "1", Expect::Absent).await.unwrap();
        put(&s, "/c/k/a", "2", Expect::Revision(first))
            .await
            .unwrap();
        // The second writer still holds `first` — this is the lost-update the
        // whole compare-and-swap discipline exists to prevent.
        let err = put(&s, "/c/k/a", "3", Expect::Revision(first))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn creating_twice_is_refused() {
        let s = MemoryStore::new();
        put(&s, "/c/k/a", "1", Expect::Absent).await.unwrap();
        assert!(matches!(
            put(&s, "/c/k/a", "1", Expect::Absent).await.unwrap_err(),
            StoreError::Exists { .. }
        ));
    }

    #[tokio::test]
    async fn a_list_is_prefix_scoped_and_ordered() {
        let s = MemoryStore::new();
        put(&s, "/c/instances/b", "1", Expect::Absent)
            .await
            .unwrap();
        put(&s, "/c/instances/a", "1", Expect::Absent)
            .await
            .unwrap();
        put(&s, "/c/instances-archive/z", "1", Expect::Absent)
            .await
            .unwrap();
        let got: Vec<_> = s
            .list("/c/instances/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(got, vec!["/c/instances/a", "/c/instances/b"]);
    }

    #[tokio::test]
    async fn a_watch_sees_changes_under_its_prefix_and_no_others() {
        let s = MemoryStore::new();
        let mut rx = s.watch("/c/instances/", None);
        put(&s, "/c/instances/a", "1", Expect::Absent)
            .await
            .unwrap();
        put(&s, "/c/volumes/v", "1", Expect::Absent).await.unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event.key(), "/c/instances/a");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "a watcher was woken for another collection's object"
        );
    }

    #[tokio::test]
    async fn a_watch_from_a_past_revision_gets_what_it_missed() {
        // This is what makes list-then-watch race-free: the caller lists at
        // revision R and watches from R, and nothing that happened in between
        // is lost.
        let s = MemoryStore::new();
        let r = put(&s, "/c/instances/a", "1", Expect::Absent)
            .await
            .unwrap();
        put(&s, "/c/instances/b", "1", Expect::Absent)
            .await
            .unwrap();
        let mut rx = s.watch("/c/instances/", Some(r));
        let event = rx.recv().await.unwrap();
        assert_eq!(
            event.key(),
            "/c/instances/b",
            "the missed write was not replayed"
        );
    }

    #[tokio::test]
    async fn a_delete_tells_watchers_which_key_went() {
        let s = MemoryStore::new();
        put(&s, "/c/instances/a", "1", Expect::Absent)
            .await
            .unwrap();
        let mut rx = s.watch("/c/instances/", None);
        s.delete("/c/instances/a", Expect::Any).await.unwrap();
        assert!(
            matches!(rx.recv().await.unwrap(), Event::Delete { key, .. } if key == "/c/instances/a")
        );
    }

    #[tokio::test]
    async fn a_dropped_watcher_does_not_wedge_the_store() {
        let s = MemoryStore::new();
        {
            let _rx = s.watch("/c/instances/", None);
        }
        // The receiver is gone; the store must reap it rather than block or
        // grow a queue for a listener that will never read again.
        put(&s, "/c/instances/a", "1", Expect::Absent)
            .await
            .unwrap();
        assert_eq!(s.inner.lock().unwrap().watchers.len(), 0);
    }
}
