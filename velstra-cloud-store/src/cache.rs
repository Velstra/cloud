//! One watch on the store, however many readers.
//!
//! This is the piece that decides how big a cell can get, and it is worth being
//! precise about why.
//!
//! Every node agent needs to know about the objects it holds, and the obvious
//! way to arrange that is for each agent to list and watch the store itself.
//! That works beautifully at ten nodes and stops working somewhere in the
//! hundreds, for two separate reasons:
//!
//! * **Every agent reads the whole cell.** The store cannot filter a range read
//!   by anything but a key prefix, and an object's node is not in its key — it
//!   is in a field that changes when a guest moves. So a node lists everything
//!   and throws most of it away.
//! * **Every write is delivered to every agent.** A thousand nodes means a
//!   thousand watchers on one etcd cluster and a thousand deliveries per write.
//!
//! Putting the API in the middle only moves the second problem unless the API
//! holds **one** watch and fans out from it. That is what this is, and it is what
//! Kubernetes calls the watch cache — the reason its apiserver scales to
//! thousands of nodes on an etcd that would not survive them as direct clients.
//!
//! ## What it costs, honestly
//!
//! A read served from here is **eventually consistent**: it reflects everything
//! up to the last event this has applied, and it can be one event behind.
//! Kubernetes makes the same trade and spells it the same way — a cached read is
//! opt-in, and a caller who needs read-after-write goes to the store.
//!
//! For a node agent it is the right trade and not merely an acceptable one:
//! every decision an agent makes is level-triggered, written with a
//! compare-and-swap, and repeated on a resync. Acting on a world one event old
//! costs a lost write and another pass. For a console showing somebody the
//! object they just changed it is the wrong trade, so that path is not served
//! from here.
//!
//! ## Backpressure
//!
//! A subscriber that stops reading is **dropped**, exactly as the store drops a
//! watcher that falls behind, and for the same reason: unbounded memory in the
//! one process that everything talks to is a worse failure than a client having
//! to list again. Falling behind costs a re-list, which every client can do.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};
use velstra_cloud_model::{
    meta::Revision,
    resources::{Observed, Resource},
};

use crate::{Event, Store, TypedStore, WATCH_QUEUE, parse_key};

/// Resource name to the object.
type Held<S, St> = Arc<RwLock<BTreeMap<String, Arc<Resource<S, St>>>>>;

pub struct Cached<S, St> {
    objects: Held<S, St>,
    /// The highest revision this has applied. A list served from here reports
    /// it, so a watch that starts there starts exactly where the list ended —
    /// the same contract the store gives, which is what lets a caller not care
    /// which of the two answered.
    revision: Arc<AtomicU64>,
    subscribers: Arc<Mutex<Vec<mpsc::Sender<Event>>>>,
    store: TypedStore<S, St>,
    /// Whether the first list has landed. Every read waits for it: a caller
    /// handed an empty collection at startup would conclude the cell is empty
    /// and act on it.
    ready: watch::Receiver<bool>,
}

impl<S, St> Clone for Cached<S, St> {
    fn clone(&self) -> Self {
        Self {
            objects: self.objects.clone(),
            revision: self.revision.clone(),
            subscribers: self.subscribers.clone(),
            store: self.store.clone(),
            ready: self.ready.clone(),
        }
    }
}

impl<S, St> Cached<S, St>
where
    S: Serialize + DeserializeOwned + PartialEq + Send + Sync + 'static,
    St: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync + 'static,
{
    /// Start caching, and return immediately.
    pub fn start(store: TypedStore<S, St>, raw: Arc<dyn Store>, prefix: String) -> Self {
        let (ready_tx, ready) = watch::channel(false);
        let me = Self {
            objects: Arc::new(RwLock::new(BTreeMap::new())),
            revision: Arc::new(AtomicU64::new(0)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            store,
            ready,
        };
        let feeding = me.clone();
        tokio::spawn(async move { feeding.feed(raw, prefix, ready_tx).await });
        me
    }

    /// Everything in the collection, and the revision it is current as of.
    pub async fn all(&self) -> (Vec<Arc<Resource<S, St>>>, Revision) {
        self.wait().await;
        let held = self.objects.read().unwrap();
        (
            held.values().cloned().collect(),
            Revision(self.revision.load(Ordering::SeqCst)),
        )
    }

    /// One object, or `None`.
    pub async fn get(&self, name: &str) -> Option<Arc<Resource<S, St>>> {
        self.wait().await;
        self.objects.read().unwrap().get(name).cloned()
    }

    /// Changes from here on, in the store's own event shape.
    ///
    /// Deliberately not "from a revision": a subscriber that needs history goes
    /// to the store, which has it. This is for a caller that has just read
    /// [`all`](Self::all) and wants to be told what happens next — and the
    /// revision that read reported is what makes the two line up.
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(WATCH_QUEUE);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// How many subscribers are attached. For a metric, and for a test that
    /// wants to prove a dropped one is actually let go of.
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
    /// `try_send` rather than `send`: awaiting a slow subscriber would stall the
    /// one task that keeps the cache current, so one wedged client would freeze
    /// the view for every other.
    fn fan_out(&self, event: &Event) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping a subscriber that fell behind; it will have to list again");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }

    /// Revision, then watch, then list — the same order everything else here
    /// uses, and for the same reason: watching from before the list replays a
    /// few events the list already covered, and watching from after it loses
    /// whatever was written in between.
    async fn feed(self, raw: Arc<dyn Store>, prefix: String, ready: watch::Sender<bool>) {
        loop {
            let from = match self.store.revision().await {
                Ok(from) => from,
                Err(error) => {
                    warn!(%error, prefix, "cache cannot reach the store; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let mut events = raw.watch(&prefix, Some(from));
            match self.store.list().await {
                Ok(fresh) => {
                    let mut held = self.objects.write().unwrap();
                    held.clear();
                    let mut highest = from.0;
                    for object in fresh {
                        highest = highest.max(object.meta.revision.0);
                        held.insert(object.meta.name.to_string(), Arc::new(object));
                    }
                    self.revision.store(highest, Ordering::SeqCst);
                }
                Err(error) => {
                    warn!(%error, prefix, "cache cannot list; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            }
            let _ = ready.send(true);
            debug!(prefix, held = self.objects.read().unwrap().len(), "cached");

            while let Some(event) = events.recv().await {
                match &event {
                    Event::Put(entry) => {
                        if let Some((_, _, name)) = parse_key(&entry.key) {
                            match serde_json::from_slice::<Resource<S, St>>(&entry.value) {
                                Ok(object) => {
                                    self.objects
                                        .write()
                                        .unwrap()
                                        .insert(name.to_string(), Arc::new(object));
                                }
                                // Something is there and this cannot read it.
                                // Leaving the old copy would have the cache
                                // answer with a version that no longer exists.
                                Err(error) => {
                                    warn!(%error, name, "cache could not decode an object");
                                    self.objects.write().unwrap().remove(name);
                                }
                            }
                        }
                        self.revision.store(entry.revision.0, Ordering::SeqCst);
                    }
                    Event::Delete { key, revision } => {
                        if let Some((_, _, name)) = parse_key(key) {
                            self.objects.write().unwrap().remove(name);
                        }
                        self.revision.store(revision.0, Ordering::SeqCst);
                    }
                }
                // After the cache is current, never before: a subscriber woken
                // by an event it can then not see in a read would be told to
                // look at a world that has not arrived yet.
                self.fan_out(&event);
            }
            warn!(prefix, "the cache's watch ended; re-listing");
        }
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{InstanceSpec, InstanceStatus},
    };

    use super::*;
    use crate::MemoryStore;

    fn instance(id: &str) -> Resource<InstanceSpec, InstanceStatus> {
        Resource::new(
            Meta::new(
                ResourceName::parse(&format!("projects/p1/instances/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec::default(),
            InstanceStatus::default(),
        )
    }

    fn cell() -> (
        Arc<dyn Store>,
        TypedStore<InstanceSpec, InstanceStatus>,
        Arc<MemoryStore>,
    ) {
        let inner = Arc::new(MemoryStore::new());
        let raw: Arc<dyn Store> = inner.clone();
        let typed = TypedStore::new(raw.clone(), "cell-1", "instances");
        (raw, typed, inner)
    }

    async fn settle<F: Fn() -> bool>(done: F) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn one_watch_serves_every_subscriber() {
        // The whole point. Without it, a thousand node agents are a thousand
        // watchers on one etcd cluster and every write is delivered a thousand
        // times.
        let (raw, typed, inner) = cell();
        let cache = Cached::start(
            typed.clone(),
            raw.clone(),
            crate::prefix_for("cell-1", "instances"),
        );
        cache.all().await;
        assert_eq!(
            inner.watchers(),
            1,
            "the cache did not settle on a single upstream watch"
        );

        let mut a = cache.subscribe();
        let mut b = cache.subscribe();
        let mut c = cache.subscribe();
        assert_eq!(
            inner.watchers(),
            1,
            "subscribing opened another watch on the store"
        );

        typed.create(&instance("i1")).await.unwrap();
        for (who, rx) in [("a", &mut a), ("b", &mut b), ("c", &mut c)] {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("subscriber {who} was never told"));
            assert!(matches!(event, Some(Event::Put(_))));
        }
    }

    #[tokio::test]
    async fn a_read_is_current_by_the_time_a_subscriber_is_woken() {
        // The ordering that makes a cached read usable at all: an agent woken by
        // an event immediately reads, and reading a view that does not yet hold
        // what it was woken about would have it decide nothing has changed.
        let (raw, typed, _) = cell();
        let cache = Cached::start(
            typed.clone(),
            raw.clone(),
            crate::prefix_for("cell-1", "instances"),
        );
        cache.all().await;
        let mut events = cache.subscribe();

        typed.create(&instance("i1")).await.unwrap();
        events.recv().await.expect("no event");
        let (held, revision) = cache.all().await;
        assert_eq!(held.len(), 1, "woken about an object the read cannot see");
        assert!(revision.0 > 0, "the cache reported no revision");
    }

    #[tokio::test]
    async fn a_subscriber_that_went_away_is_let_go_of() {
        let (raw, typed, _) = cell();
        let cache = Cached::start(
            typed.clone(),
            raw.clone(),
            crate::prefix_for("cell-1", "instances"),
        );
        cache.all().await;
        let listening = cache.subscribe();
        assert_eq!(cache.subscribers(), 1);
        drop(listening);

        typed.create(&instance("i1")).await.unwrap();
        settle(|| cache.subscribers() == 0).await;
        assert_eq!(
            cache.subscribers(),
            0,
            "a receiver that is gone is still held"
        );
    }

    #[tokio::test]
    async fn a_subscriber_that_stops_reading_is_dropped_rather_than_queued_for() {
        // Asserted against `fan_out` directly rather than by flooding the store,
        // because what is being claimed is a property of this code and not of
        // how fast a test runtime happens to schedule two tasks. Unbounded
        // memory in the one process everything talks to is a worse failure than
        // a client having to list again.
        let (raw, typed, _) = cell();
        let cache = Cached::start(
            typed.clone(),
            raw.clone(),
            crate::prefix_for("cell-1", "instances"),
        );
        cache.all().await;
        let _sleeping = cache.subscribe();
        assert_eq!(cache.subscribers(), 1);

        let event = Event::Delete {
            key: "/cell-1/instances/projects/p1/instances/i1".into(),
            revision: Revision(1),
        };
        for _ in 0..WATCH_QUEUE {
            cache.fan_out(&event);
        }
        assert_eq!(
            cache.subscribers(),
            1,
            "let go of before the queue was full"
        );
        cache.fan_out(&event);
        assert_eq!(
            cache.subscribers(),
            0,
            "a subscriber that never read is still being queued for"
        );
    }

    #[tokio::test]
    async fn a_delete_is_applied_and_passed_on() {
        let (raw, typed, _) = cell();
        let cache = Cached::start(
            typed.clone(),
            raw.clone(),
            crate::prefix_for("cell-1", "instances"),
        );
        typed.create(&instance("i1")).await.unwrap();
        settle(|| true).await;
        cache.all().await;
        let mut events = cache.subscribe();

        let held = typed
            .get("projects/p1/instances/i1")
            .await
            .unwrap()
            .unwrap();
        typed
            .delete("projects/p1/instances/i1", held.meta.revision)
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("nothing arrived");
        assert!(matches!(event, Some(Event::Delete { .. })));
        assert!(cache.get("projects/p1/instances/i1").await.is_none());
    }
}
