//! The same contract, on storage that outlives the process.
//!
//! Nothing here is a translation layer worth the name: the trait in this crate
//! was drawn around what etcd already does, so `Expect` is a transaction
//! compare, a revision is etcd's revision, and a watch from a revision is a
//! watch from a revision. The interesting code is all in the places where the
//! two backends could otherwise disagree — which errors a refused write
//! produces, where a watch starts when the caller says "now", and what happens
//! to a watcher that stops reading. Those are the parts a caller can feel, and
//! a caller must never be able to tell which backend it has.
//!
//! What is deliberately absent: leases. Nothing in this platform expires by
//! itself. A node that stops reporting is not a node whose objects should
//! vanish — a controller decides that, from a heartbeat it can see, and writes
//! the decision down. Keys that delete themselves would make that judgement
//! invisible.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use etcd_client::{
    Client, Compare, CompareOp, EventType, GetOptions, KeyValue, ResponseHeader, Txn, TxnOp,
    TxnOpResponse, TxnResponse, WatchOptions,
};
use tokio::sync::mpsc;
use velstra_cloud_model::meta::Revision;

use crate::{Entry, Event, Expect, Page, Result, Store, StoreError, WATCH_QUEUE};

/// A handle on one etcd cluster. Cheap to clone — the client multiplexes over
/// one connection, so a clone is another user of it rather than another socket.
#[derive(Clone)]
pub struct EtcdStore {
    client: Client,
    /// The newest revision this handle has been told about by any response.
    ///
    /// It exists for one reason: [`Store::watch`] is synchronous but etcd's is
    /// not, so between `watch()` returning and the server accepting the stream
    /// there is a window in which writes would be missed. A watch that says
    /// "from now" therefore starts from the newest revision this handle has
    /// actually seen, which closes the window. The cost is that another
    /// process's writes in that gap are replayed rather than skipped — and
    /// replaying is free here, because every reader in this platform is
    /// level-triggered and idempotent, while missing a write is not.
    newest_seen: Arc<AtomicU64>,
}

impl EtcdStore {
    /// Connect and learn where the store currently is.
    ///
    /// The revision read is not a health check dressed up as a read: it seeds
    /// the watermark above, so a handle that watches before it has done
    /// anything else still starts from a defensible point rather than from
    /// zero — which would replay the entire history — or from "now", which
    /// would race.
    pub async fn connect<E: AsRef<str>, S: AsRef<[E]>>(endpoints: S) -> Result<Self> {
        let client = Client::connect(endpoints, None).await.map_err(backend)?;
        let store = Self {
            client,
            newest_seen: Arc::new(AtomicU64::new(0)),
        };
        store.revision().await?;
        Ok(store)
    }

    /// Record a revision seen in a response and hand it back as a `Revision`.
    fn observe(&self, header: Option<&ResponseHeader>) -> Revision {
        let revision = header.map(|h| h.revision()).unwrap_or_default().max(0) as u64;
        self.newest_seen.fetch_max(revision, Ordering::Relaxed);
        Revision(revision)
    }
}

#[async_trait]
impl Store for EtcdStore {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        let response = self
            .client
            .kv_client()
            .get(key, None)
            .await
            .map_err(backend)?;
        self.observe(response.header());
        response.kvs().first().map(entry).transpose()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        // etcd answers a range in one message and a message has a size ceiling,
        // so a collection large enough to reach it has to be read in pages.
        // Every page after the first is pinned to the revision the first was
        // read at: without that, a write landing between two pages would show
        // one object twice or hide another, and a controller that lists in
        // order to reconcile would be acting on a collection that never
        // existed.
        //
        // Key order is what makes this work at all, and it is the same order on
        // both backends: etcd compares keys as bytes, which is how Rust orders
        // the `String` keys the memory store holds.
        let mut kv = self.client.kv_client();
        let end = range_end(prefix);
        let mut start = prefix.as_bytes().to_vec();
        let mut pinned: Option<i64> = None;
        let mut entries = Vec::new();
        loop {
            let mut options = GetOptions::new()
                .with_range(end.clone())
                .with_limit(LIST_PAGE);
            if let Some(revision) = pinned {
                options = options.with_revision(revision);
            }
            let response = kv.get(start, Some(options)).await.map_err(backend)?;
            let revision = self.observe(response.header());
            for kv in response.kvs() {
                entries.push(entry(kv)?);
            }
            if !response.more() {
                return Ok(entries);
            }
            let Some(last) = entries.last() else {
                return Err(StoreError::Backend(
                    "etcd reported more of a range after returning none of it".to_string(),
                ));
            };
            // The next page starts at the key just past the last one seen.
            start = last.key.as_bytes().to_vec();
            start.push(0);
            pinned.get_or_insert(revision.0 as i64);
        }
    }

    async fn list_page(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Page> {
        // One range request, bounded by the caller's limit — the point of the
        // whole exercise. `list` above loops because it must return everything;
        // here the ceiling is the answer, so there is nothing to loop over.
        //
        // No `with_revision`: see the trait's note on what paging promises. A
        // revision held across client round trips is one etcd may compact
        // underneath the caller, and the price of that promise is answering
        // `410 Gone` to somebody whose token merely got old.
        let mut kv = self.client.kv_client();
        let start = match after {
            // Strictly after: the resume key is the last one already delivered,
            // and etcd ranges are inclusive at the start. Appending the zero byte
            // is the successor of a key under byte-wise comparison, which is the
            // ordering etcd uses.
            Some(after) => {
                let mut start = after.as_bytes().to_vec();
                start.push(0);
                start
            }
            None => prefix.as_bytes().to_vec(),
        };
        let options = GetOptions::new()
            .with_range(range_end(prefix))
            .with_limit(limit as i64);
        let response = kv.get(start, Some(options)).await.map_err(backend)?;
        self.observe(response.header());
        let entries = response
            .kvs()
            .iter()
            .map(entry)
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            entries,
            more: response.more(),
        })
    }

    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision> {
        let mut kv = self.client.kv_client();
        let compare = match expect {
            Expect::Any => {
                let response = kv.put(key, value, None).await.map_err(backend)?;
                return Ok(self.observe(response.header()));
            }
            // A key that has never existed has a create revision of zero. That
            // is etcd's way of spelling "absent", and it is why a create needs
            // no read of its own.
            Expect::Absent => Compare::create_revision(key, CompareOp::Equal, 0),
            Expect::Revision(want) => Compare::mod_revision(key, CompareOp::Equal, want.0 as i64),
        };
        // The `or_else` read is what makes a refusal informative, and it has to
        // be in the same transaction: a follow-up read would report whatever
        // the key looks like by then, not the state the compare actually lost
        // to.
        let txn = Txn::new()
            .when([compare])
            .and_then([TxnOp::put(key, value, None)])
            .or_else([TxnOp::get(key, None)]);
        let response = kv.txn(txn).await.map_err(backend)?;
        let revision = self.observe(response.header());
        if response.succeeded() {
            return Ok(revision);
        }
        Err(match (expect, found(&response)) {
            // The create compare only fails one way, and this is it.
            (Expect::Absent, _) => StoreError::Exists {
                key: key.to_string(),
            },
            // A key that is gone reads as revision zero rather than as a
            // separate error, because to a writer holding a stale copy the
            // situation is the same one: what you have is not what is there.
            (Expect::Revision(expected), actual) => StoreError::Conflict {
                key: key.to_string(),
                expected,
                actual: actual.unwrap_or(Revision(0)),
            },
            (Expect::Any, _) => StoreError::Backend(format!(
                "an unconditional put of {key} was refused by a compare it never made"
            )),
        })
    }

    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision> {
        let compare = match expect {
            Expect::Revision(want) => Compare::mod_revision(key, CompareOp::Equal, want.0 as i64),
            // Existence is a condition even for an unconditional delete: a key
            // that is not there is `Missing`, not a quiet no-op that reports a
            // revision as if something happened. `Expect::Absent` lands here
            // too, and means the same as `Any` — it is not a sentence anyone
            // can finish about a delete, and the memory store ignores it in the
            // same place.
            _ => Compare::create_revision(key, CompareOp::Greater, 0),
        };
        let txn = Txn::new()
            .when([compare])
            .and_then([TxnOp::delete(key, None)])
            .or_else([TxnOp::get(key, None)]);
        let response = self.client.kv_client().txn(txn).await.map_err(backend)?;
        let revision = self.observe(response.header());
        if response.succeeded() {
            return Ok(revision);
        }
        Err(match (expect, found(&response)) {
            // Gone beats stale, and the order matters: a caller that deleted
            // the same object twice should be told it is gone, not handed a
            // revision complaint it cannot act on. The memory store checks
            // existence first for the same reason.
            (_, None) => StoreError::Missing {
                key: key.to_string(),
            },
            (Expect::Revision(expected), Some(actual)) => StoreError::Conflict {
                key: key.to_string(),
                expected,
                actual,
            },
            (_, Some(_)) => StoreError::Backend(format!(
                "an unconditional delete of {key} was refused while the key was present"
            )),
        })
    }

    fn watch(&self, prefix: &str, from: Option<Revision>) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(WATCH_QUEUE);
        // etcd's start revision includes the revision named; the trait's `from`
        // excludes it, because the caller listed *at* `from` and wants what
        // happened after. One `+ 1` here, and none anywhere a caller can reach.
        let start = from
            .map(|r| r.0)
            .unwrap_or_else(|| self.newest_seen.load(Ordering::Relaxed))
            .saturating_add(1);
        let mut watch = self.client.watch_client();
        let prefix = prefix.to_string();
        let newest_seen = self.newest_seen.clone();
        tokio::spawn(async move {
            let options = WatchOptions::new()
                .with_prefix()
                .with_start_revision(start as i64);
            // Every early return below drops `tx`, which closes the channel.
            // That is the only signal this shape of API has: a receiver that
            // yields `None` means "you are on your own now, re-list" — which is
            // what a controller does on its resync anyway.
            let Ok(mut stream) = watch.watch(prefix, Some(options)).await else {
                return;
            };
            while let Ok(Some(response)) = stream.message().await {
                if let Some(header) = response.header() {
                    newest_seen.fetch_max(header.revision().max(0) as u64, Ordering::Relaxed);
                }
                // A cancelled watch is usually a compacted one: the events this
                // watcher wanted are gone and no amount of waiting brings them
                // back. `StoreError::Compacted` cannot travel down a plain
                // receiver, so the channel closes instead and the caller
                // re-lists — the same recovery, arrived at without an error
                // type the signature has no room for.
                if response.canceled() {
                    return;
                }
                for event in response.events() {
                    let Some(kv) = event.kv() else { continue };
                    let Ok(key) = key_string(kv.key()) else {
                        // Not a key this crate wrote, and not one it can name.
                        continue;
                    };
                    let revision = Revision(kv.mod_revision().max(0) as u64);
                    let mapped = match event.event_type() {
                        EventType::Put => Event::Put(Entry {
                            key,
                            value: kv.value().to_vec(),
                            revision,
                        }),
                        EventType::Delete => Event::Delete { key, revision },
                    };
                    // Full or gone, the answer is the same and it is the memory
                    // store's answer: drop the watcher. Unbounded memory in the
                    // process that holds all the state is worse than a
                    // controller that has to re-list.
                    if tx.try_send(mapped).is_err() {
                        return;
                    }
                }
            }
        });
        rx
    }

    async fn snapshot(&self, dir: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
        use tokio::io::AsyncWriteExt;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| StoreError::Backend(format!("creating {}: {e}", dir.display())))?;
        // Named by the moment, so a listing is a history and pruning is a sort.
        let at = velstra_cloud_model::meta::Timestamp::now().0;
        let final_path = dir.join(format!("etcd-{at:013}.snap"));
        let partial = dir.join(format!("etcd-{at:013}.partial"));
        let mut stream = self
            .client
            .maintenance_client()
            .snapshot()
            .await
            .map_err(backend)?;
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(|e| StoreError::Backend(format!("creating {}: {e}", partial.display())))?;
        while let Some(chunk) = stream.message().await.map_err(backend)? {
            file.write_all(chunk.blob())
                .await
                .map_err(|e| StoreError::Backend(format!("writing the snapshot: {e}")))?;
        }
        // Flushed before the rename: a snapshot that exists but is missing its
        // tail is worse than one that plainly never finished.
        file.sync_all()
            .await
            .map_err(|e| StoreError::Backend(format!("syncing the snapshot: {e}")))?;
        drop(file);
        tokio::fs::rename(&partial, &final_path)
            .await
            .map_err(|e| StoreError::Backend(format!("finishing the snapshot: {e}")))?;
        Ok(Some(final_path))
    }

    async fn compact(&self, keep: Revision) -> Result<()> {
        match self.client.kv_client().compact(keep.0 as i64, None).await {
            Ok(_) => Ok(()),
            // Somebody — another API replica, an operator's etcdctl — got
            // there first. The history is gone either way, which is all this
            // asked for.
            Err(e) if e.to_string().contains("has been compacted") => Ok(()),
            Err(e) => Err(backend(e)),
        }
    }

    async fn revision(&self) -> Result<Revision> {
        // An empty transaction is the cheapest thing that returns a header and
        // nothing else. It compares nothing and writes nothing, so it does not
        // move the revision it reports.
        let response = self
            .client
            .kv_client()
            .txn(Txn::new())
            .await
            .map_err(backend)?;
        Ok(self.observe(response.header()))
    }
}

/// How many objects a `list` asks for at a time. Small enough that a page of
/// ordinary resources is nowhere near etcd's message ceiling, large enough that
/// a collection anyone has today is one round trip.
const LIST_PAGE: i64 = 1024;

/// Where a prefix scan stops: the prefix with its last byte stepped up by one,
/// which is the first key that is no longer under it. An all-`0xff` prefix has
/// no successor, and neither does an empty one — both mean "to the end", which
/// etcd spells as the zero byte.
fn range_end(prefix: &str) -> Vec<u8> {
    let mut end = prefix.as_bytes().to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

/// The revision of the key as the failed transaction saw it, or `None` if it
/// was not there.
fn found(response: &TxnResponse) -> Option<Revision> {
    response.op_responses().into_iter().find_map(|op| match op {
        TxnOpResponse::Get(get) => get
            .kvs()
            .first()
            .map(|kv| Revision(kv.mod_revision().max(0) as u64)),
        _ => None,
    })
}

fn entry(kv: &KeyValue) -> Result<Entry> {
    Ok(Entry {
        key: key_string(kv.key())?,
        value: kv.value().to_vec(),
        // The revision of an object is when it last changed, not when the
        // cluster last did anything: `mod_revision`, never the header.
        revision: Revision(kv.mod_revision().max(0) as u64),
    })
}

/// Keys are `String` in this crate and bytes in etcd. Everything written
/// through `key_for` is UTF-8, so anything else came from another writer
/// sharing the cluster — better named in an error than quietly mangled into a
/// key no caller can act on.
fn key_string(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|e| StoreError::Backend(format!("etcd holds a key that is not utf-8: {e}")))
}

fn backend(e: etcd_client::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}
