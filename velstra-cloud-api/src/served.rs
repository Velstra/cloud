//! One watch on the store per collection, however many node agents.
//!
//! The API is the only process that has to hold the whole cell anyway. Whether
//! it holds it *once* or a thousand agents each hold a copy is the difference
//! between a cell bounded by the store and a cell bounded by the agents around
//! it.
//!
//! Without this, every node agent lists and watches the store directly, so:
//!
//! * a thousand nodes are a thousand watchers on one etcd cluster, and every
//!   write is delivered a thousand times; and
//! * each of them lists the whole cell on every pass, because the store cannot
//!   filter a range read by anything but a key prefix — and which node holds an
//!   object is a field, not part of its key, and it changes when a guest moves.
//!
//! Putting the API in front only moves the first problem unless it holds one
//! watch and fans out. That is what this is: what Kubernetes calls the watch
//! cache, and the reason its apiserver serves thousands of nodes from an etcd
//! that would not survive them as direct clients.
//!
//! ## Documents, not typed resources
//!
//! It caches what the API already deals in. That is not laziness: the filter
//! ([`velstra_cloud_model::assignment::concerns`]) reads two fields out of a
//! document, so caching typed objects would mean serialising every one of them
//! on every read *before* discovering that the caller wanted a tenth of them.
//! Ten thousand serialisations per node per pass is exactly the cost this exists
//! to remove.
//!
//! ## What it costs
//!
//! A read served from here is **eventually consistent** — everything up to the
//! last event applied, and possibly one behind. That is why it is reached only
//! through a filtered read, which in this platform means a node agent: every
//! decision an agent makes is level-triggered, written with a compare-and-swap
//! and repeated on a resync, so acting on a world one event old costs a lost
//! write and another pass. An operator reading back the object they just changed
//! goes to the store, and this is never in that path.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};
use velstra_cloud_model::meta::Revision;
use velstra_cloud_store::{Event, WATCH_QUEUE};

use crate::collection::Collection;

/// Resource name to the document, as the API hands it out.
type Held = Arc<RwLock<BTreeMap<String, Arc<Value>>>>;

#[derive(Clone)]
pub struct Served {
    documents: Held,
    /// The highest revision applied. A list served from here reports it, so a
    /// watch that starts there starts exactly where the list ended — the same
    /// contract the store gives, which is what lets a caller not care which of
    /// the two answered.
    revision: Arc<AtomicU64>,
    subscribers: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    ready: watch::Receiver<bool>,
}

impl Served {
    pub fn start(collection: Arc<dyn Collection>) -> Self {
        let (ready_tx, ready) = watch::channel(false);
        let me = Self {
            documents: Arc::new(RwLock::new(BTreeMap::new())),
            revision: Arc::new(AtomicU64::new(0)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            ready,
        };
        let feeding = me.clone();
        tokio::spawn(async move { feeding.feed(collection, ready_tx).await });
        me
    }

    /// Every document, and the revision they are current as of.
    pub async fn all(&self) -> (Vec<Arc<Value>>, Revision) {
        self.wait().await;
        let held = self.documents.read().unwrap();
        (
            held.values().cloned().collect(),
            Revision(self.revision.load(Ordering::SeqCst)),
        )
    }

    /// Changes from here on, in the store's own event shape, so the API's
    /// existing event path does not have to know where they came from.
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(WATCH_QUEUE);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn subscribers(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }

    async fn wait(&self) {
        let mut ready = self.ready.clone();
        while !*ready.borrow() {
            if ready.changed().await.is_err() {
                return;
            }
        }
    }

    /// Hand an event to everyone listening, and let go of anyone who has stopped.
    ///
    /// `try_send`, never `send`: awaiting one slow subscriber would stall the
    /// single task that keeps this current, so one wedged agent would freeze the
    /// view for every other. A dropped subscriber has to list again, which every
    /// client of this API can do — and that is a far better failure than
    /// unbounded memory in the one process everything talks to.
    fn fan_out(&self, event: &Event) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| match tx.try_send(event.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("dropping a subscriber that fell behind; it will have to list again");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }

    /// Revision, then watch, then list — the order used everywhere here, so a
    /// write between the two is replayed rather than lost.
    async fn feed(self, collection: Arc<dyn Collection>, ready: watch::Sender<bool>) {
        let kind = collection.kind();
        loop {
            let from = match collection.revision().await {
                Ok(from) => from,
                Err(error) => {
                    warn!(%error, kind, "cache cannot reach the store; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let mut events = collection.watch(Some(from));
            match collection.list().await {
                Ok(fresh) => {
                    let mut held = self.documents.write().unwrap();
                    held.clear();
                    for document in fresh {
                        if let Some(name) = name_of(&document) {
                            held.insert(name, Arc::new(document));
                        }
                    }
                    self.revision.store(from.0, Ordering::SeqCst);
                }
                Err(error) => {
                    warn!(%error, kind, "cache cannot list; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            }
            let _ = ready.send(true);
            debug!(kind, held = self.documents.read().unwrap().len(), "cached");

            while let Some(event) = events.recv().await {
                match &event {
                    Event::Put(entry) => {
                        match collection.decode(&entry.value, entry.revision) {
                            Ok(document) => {
                                if let Some(name) = name_of(&document) {
                                    self.documents
                                        .write()
                                        .unwrap()
                                        .insert(name, Arc::new(document));
                                }
                            }
                            // Something is there and this cannot read it.
                            // Keeping the old copy would have the cache answer
                            // with a version that no longer exists, so it goes
                            // and the re-list is what repairs it.
                            Err(error) => {
                                warn!(%error, kind, "cache could not decode an object");
                                if let Some((_, _, name)) =
                                    velstra_cloud_store::parse_key(&entry.key)
                                {
                                    self.documents.write().unwrap().remove(name);
                                }
                            }
                        }
                        self.revision.store(entry.revision.0, Ordering::SeqCst);
                    }
                    Event::Delete { key, revision } => {
                        if let Some((_, _, name)) = velstra_cloud_store::parse_key(key) {
                            self.documents.write().unwrap().remove(name);
                        }
                        self.revision.store(revision.0, Ordering::SeqCst);
                    }
                }
                // After the cache is current, never before: an agent woken by an
                // event and then handed a view without it would decide nothing
                // had changed and go back to sleep.
                self.fan_out(&event);
            }
            warn!(kind, "the cache's watch ended; re-listing");
        }
    }
}

/// A document's resource name, which is a segment list in model shape rather
/// than a string.
fn name_of(document: &Value) -> Option<String> {
    serde_json::from_value::<velstra_cloud_model::meta::ResourceName>(
        document.get("meta")?.get("name")?.clone(),
    )
    .ok()
    .map(|name| name.to_string())
}
