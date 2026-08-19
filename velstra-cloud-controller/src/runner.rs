//! The loop. One of them, for every resource type there will ever be.
//!
//! ```text
//!   revision  ──►  watch(from)  ──►  list  ──►  queue  ──►  reconcile  ──►  write
//!        ▲              │                          ▲            │
//!        │              └── dropped ───────────────┘            │
//!        └────────────────── resync every N ────────────────────┘
//! ```
//!
//! Four things that look like details and are not:
//!
//! * **The revision is taken before the list, not after.** Watching from a
//!   revision read *after* listing would silently skip anything written while
//!   the list was being assembled. Watching from before it replays a few events
//!   the list already covered, which costs one redundant reconcile of an object
//!   that is idempotent anyway. One of those two mistakes is free.
//! * **A dropped watch re-lists rather than dies.** A slow consumer or a
//!   compaction ends the stream. The response is to establish again from a
//!   fresh revision — the same code path as startup, so it is exercised on
//!   every run rather than only during an incident.
//! * **The resync re-lists everything on a timer.** This is what turns a missed
//!   event from a corruption into a delay, and it is only affordable because a
//!   reconcile of a settled object writes nothing.
//! * **The object is read fresh, immediately before deciding.** The queue
//!   carries names, never objects. A stale object in a queue is a decision made
//!   about a world that has moved on.

use std::{sync::Arc, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use velstra_cloud_model::resources::{Observed, Resource};
use velstra_cloud_store::{Store, TypedStore, parse_key};

use crate::{Metrics, Result, queue::WorkQueue};

#[derive(Clone, Copy, Debug)]
pub struct LoopConfig {
    /// How often to re-list everything and reconcile it again.
    pub resync: Duration,
    /// The shortest interval between two reconciles in one controller.
    pub rate: Duration,
    pub backoff_base: Duration,
    pub backoff_ceiling: Duration,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            resync: Duration::from_secs(300),
            // Roughly two hundred objects a second per controller: fast enough
            // that a ten-thousand-object resync is under a minute, slow enough
            // that a reconcile cycle cannot spin a core.
            rate: Duration::from_millis(5),
            backoff_base: Duration::from_millis(250),
            backoff_ceiling: Duration::from_secs(120),
        }
    }
}

/// Another collection whose changes make an object of *this* kind worth
/// looking at again.
///
/// A quota is a fact about a project computed from its instances, so an
/// instance changing is a reason to look at a project. Without this the only
/// thing that would notice is the resync, and a quota that is five minutes
/// stale is a quota that admits work it should have refused.
pub struct Related {
    pub prefix: String,
    /// Which of this controller's objects that change makes worth another look.
    #[allow(clippy::type_complexity)]
    pub wake: Arc<dyn Fn(Changed<'_>) -> Wake + Send + Sync>,
}

/// What changed over there: its name, and its bytes when there are any.
///
/// The bytes matter more than they look. A relation is almost always in the
/// **spec** — an instance names its ports, a port names its subnet — and a
/// resource *name* cannot carry it. Without the object, a controller in that
/// position has only two options, and both are bad: wake everything it owns, or
/// wake nothing. The port controller shipped with the second, written as a
/// mapping that returned an empty list, which reads as "all of them" and means
/// the opposite.
///
/// `value` is `None` for a delete, because a deletion carries no object. That is
/// the one case where the relation genuinely cannot be computed, and [`Wake::All`]
/// is the honest answer to it.
pub struct Changed<'a> {
    pub name: &'a str,
    pub value: Option<&'a [u8]>,
}

/// Which of this controller's own objects to look at again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Exactly these. The cheap case, and the one to reach for: the cost of an
    /// event is then the cost of the objects it really affects, not of the cell.
    These(Vec<String>),
    /// Everything this controller owns, because which ones are affected cannot
    /// be worked out from what changed.
    ///
    /// Measured before being written down: with one of these on a collection of
    /// N objects, each of whose reconciles reads a collection of N, a single
    /// event costs N². At ten thousand instances that is a hundred million reads
    /// for one guest moving. Use it for deletes and for genuine unknowns, and
    /// not as a way of saying "I did not work out the relation".
    All,
}

impl Related {
    /// Wake objects named from the changed object's *name* alone.
    ///
    /// Right when the relation is in the name — a snapshot lives under its
    /// volume, an operation names its target — and wrong whenever it is in the
    /// spec. [`Related::of`] is for that.
    pub fn named(
        prefix: impl Into<String>,
        map: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            wake: Arc::new(move |changed| Wake::These(map(changed.name))),
        }
    }

    /// Wake objects named from the changed object itself.
    ///
    /// A delete carries no object, so it wakes everything — correct, and rare
    /// enough to be affordable.
    pub fn of<S, St>(
        prefix: impl Into<String>,
        map: impl Fn(&Resource<S, St>) -> Vec<String> + Send + Sync + 'static,
    ) -> Self
    where
        S: DeserializeOwned + Send + Sync + 'static,
        St: DeserializeOwned + Send + Sync + 'static,
    {
        Self {
            prefix: prefix.into(),
            wake: Arc::new(move |changed| match changed.value {
                Some(bytes) => match serde_json::from_slice::<Resource<S, St>>(bytes) {
                    Ok(object) => Wake::These(map(&object)),
                    // Unreadable bytes are not "nothing changed". Something is
                    // there and this cannot say what, which is exactly the case
                    // the sweep is for.
                    Err(_) => Wake::All,
                },
                None => Wake::All,
            }),
        }
    }

    /// Re-examine everything this controller owns when anything under `prefix`
    /// changes. See [`Wake::All`] for what that costs.
    pub fn all(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            wake: Arc::new(|_| Wake::All),
        }
    }
}

/// A controller: what it watches, and what it does about one object.
///
/// Everything a controller needs beyond the object it holds itself. The loop
/// hands it a name and the object under that name, freshly read — or `None` if
/// it is gone, which is a perfectly ordinary thing for an object to be.
pub trait Reconciler: Send + Sync + 'static {
    type Spec: Serialize + DeserializeOwned + PartialEq + Send + Sync + 'static;
    type Status: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync + 'static;

    /// For logs and metrics. Stable.
    fn name(&self) -> &'static str;

    fn related(&self) -> Vec<Related> {
        Vec::new()
    }

    fn reconcile(
        &self,
        name: &str,
        object: Option<&Resource<Self::Spec, Self::Status>>,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// One pass over every object of this kind, with no watch and no queue.
///
/// The honest shape of a reconcile, for a caller that wants it once: startup
/// checks, tests, and the assertion that a settled cluster is settled.
///
/// Unlike [`run`], this stops at the first failure and hands it back rather
/// than backing the object off and carrying on. A caller doing one pass wants
/// to know it did not finish; a loop wants to keep serving everything else.
pub async fn sweep<R: Reconciler>(
    reconciler: &R,
    store: &TypedStore<R::Spec, R::Status>,
) -> Result<()> {
    for object in store.list().await? {
        let name = object.meta.name.to_string();
        reconciler.reconcile(&name, Some(&object)).await?;
    }
    Ok(())
}

/// Run `reconciler` until `shutdown` says otherwise.
pub async fn run<R: Reconciler>(
    reconciler: Arc<R>,
    store: TypedStore<R::Spec, R::Status>,
    raw: Arc<dyn Store>,
    config: LoopConfig,
    metrics: Metrics,
    shutdown: watch::Receiver<bool>,
) {
    // A process with no election in front of it leads unconditionally, which is
    // what a single-process deployment and every test that drives `run` directly
    // want. See `run_when_leading`.
    let (_always, leader) = watch::channel(true);
    run_when_leading(reconciler, store, raw, config, metrics, shutdown, leader).await
}

/// [`run`], but acting only while `leader` says this process holds the lease.
///
/// Leadership gates the whole loop rather than the write at the end of it, and
/// that is deliberate: a follower that kept watching and queueing would hold a
/// watch per controller against the store for no reason, and would come out of a
/// handover with a queue built from a world it had not been acting on. Standing
/// down drops the watch and the queue; taking over establishes both from
/// scratch, along the same path a fresh start takes — so the handover path is
/// the startup path, exercised on every run rather than only during an incident.
pub async fn run_when_leading<R: Reconciler>(
    reconciler: Arc<R>,
    store: TypedStore<R::Spec, R::Status>,
    raw: Arc<dyn Store>,
    config: LoopConfig,
    metrics: Metrics,
    mut shutdown: watch::Receiver<bool>,
    mut leader: watch::Receiver<bool>,
) {
    let controller = reconciler.name();
    // Depth one, on purpose. A pending sweep already covers everything a second
    // request would, so coalescing is the correct behaviour and not a
    // concession — see `Fanout::All`.
    let (sweep_now, mut sweep_requested) = mpsc::channel::<()>(1);

    'establish: loop {
        if *shutdown.borrow() {
            break;
        }
        // Nothing is read, watched or queued while another process leads.
        while !*leader.borrow() {
            if *shutdown.borrow() {
                break 'establish;
            }
            tokio::select! {
                _ = leader.changed() => {}
                _ = shutdown.changed() => {}
            }
        }

        // One queue per establishment, not one per process. It holds names
        // decided under a particular lease and a particular watch; carrying it
        // across a stand-down would mean coming back and reconciling a list
        // assembled while another process was the one acting. A dropped watch
        // re-establishes the same way, so this is the existing rule — the queue
        // belongs to the stream that filled it — rather than a new one for
        // leases.
        let queue = Arc::new(WorkQueue::new(
            config.rate,
            config.backoff_base,
            config.backoff_ceiling,
        ));

        // Read the revision first, then watch from it, then list. See the note
        // at the top of the file: the only tolerable direction to be wrong in
        // is "reconcile something twice".
        let from = match store.revision().await {
            Ok(r) => r,
            Err(error) => {
                warn!(controller, %error, "cannot reach the store; retrying");
                if wait_or_shutdown(config.backoff_base, &mut shutdown).await {
                    break;
                }
                continue;
            }
        };
        let mut events = store.watch(Some(from));
        let related = spawn_related(&reconciler, &raw, from, &queue, &sweep_now);

        match store.list().await {
            Ok(objects) => {
                for object in &objects {
                    queue.add(&object.meta.name.to_string());
                }
                info!(controller, objects = objects.len(), %from, "listed and watching");
            }
            Err(error) => {
                warn!(controller, %error, "cannot list; retrying");
                related.abort_all();
                if wait_or_shutdown(config.backoff_base, &mut shutdown).await {
                    break;
                }
                continue;
            }
        }

        let mut resync = tokio::time::interval(config.resync);
        // The tick that fires immediately is the list we just did.
        resync.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        related.abort_all();
                        queue.close();
                        info!(controller, "stopped");
                        return;
                    }
                }
                // Leadership lost mid-flight. Everything established under the
                // old lease is dropped rather than carried across: the queue
                // holds names decided under an assumption that no longer holds,
                // and the watch is a stream this process has no business
                // reading. Not `queue.close()` — closing is permanent and this
                // loop is coming back; the queue goes out of scope with the
                // iteration, which is the whole of what dropping it means here.
                _ = leader.changed() => {
                    if !*leader.borrow() {
                        related.abort_all();
                        info!(controller, "stood down; another process leads");
                        continue 'establish;
                    }
                }
                _ = resync.tick() => {
                    metrics.count("controller_resync_total", &[("controller", controller)]);
                    match store.list().await {
                        Ok(objects) => {
                            debug!(controller, objects = objects.len(), "resync");
                            for object in &objects {
                                queue.add(&object.meta.name.to_string());
                            }
                        }
                        Err(error) => warn!(controller, %error, "resync could not list"),
                    }
                }
                // Something this controller depends on changed, and which of its
                // own objects that affects cannot be told from the other one's
                // name. Same body as the resync, on demand rather than on a
                // timer — which is the whole difference between a port assigned
                // the moment its guest lands and one assigned five minutes later.
                Some(()) = sweep_requested.recv() => {
                    metrics.count("controller_related_sweep_total", &[("controller", controller)]);
                    match store.list().await {
                        Ok(objects) => {
                            debug!(controller, objects = objects.len(), "sweeping for a related change");
                            for object in &objects {
                                queue.add(&object.meta.name.to_string());
                            }
                        }
                        Err(error) => warn!(controller, %error, "could not list for a related change"),
                    }
                }
                event = events.recv() => {
                    match event {
                        Some(event) => {
                            if let Some((_, _, name)) = parse_key(event.key()) {
                                queue.add(name);
                            }
                        }
                        // The store dropped us: we fell behind, or the history
                        // we were reading was compacted. Establish again.
                        None => {
                            metrics.count("controller_watch_restart_total", &[("controller", controller)]);
                            warn!(controller, "the watch ended; re-listing");
                            related.abort_all();
                            continue 'establish;
                        }
                    }
                }
                key = queue.next() => {
                    let Some(key) = key else { return };
                    reconcile_one(&reconciler, &store, &queue, &metrics, &key).await;
                    metrics.set(
                        "controller_queue_depth",
                        &[("controller", controller)],
                        queue.depth() as f64,
                    );
                }
            }
        }
    }
}

/// Read the object and hand it to the controller, then charge the outcome to
/// the object rather than to the loop.
async fn reconcile_one<R: Reconciler>(
    reconciler: &Arc<R>,
    store: &TypedStore<R::Spec, R::Status>,
    queue: &WorkQueue,
    metrics: &Metrics,
    name: &str,
) {
    let controller = reconciler.name();
    let object = match store.get(name).await {
        Ok(object) => object,
        Err(error) => {
            let delay = queue.failed(name);
            warn!(controller, name, %error, ?delay, "could not read the object");
            return;
        }
    };

    metrics.count("controller_reconcile_total", &[("controller", controller)]);
    match reconciler.reconcile(name, object.as_ref()).await {
        Ok(()) => queue.done(name),
        Err(error) if error.is_conflict() => {
            // Somebody wrote first. Read again and decide again — this is the
            // expected outcome of two controllers on one object, not a failure,
            // and charging it to the backoff would punish exactly the objects
            // that are being worked on.
            metrics.count("controller_conflict_total", &[("controller", controller)]);
            debug!(controller, name, "lost a compare-and-swap; retrying");
            queue.add(name);
        }
        Err(error) if error.is_missing() => {
            debug!(controller, name, "gone before it could be reconciled");
            queue.done(name);
        }
        Err(error) => {
            metrics.count("controller_error_total", &[("controller", controller)]);
            let delay = queue.failed(name);
            warn!(controller, name, %error, ?delay, "reconcile failed");
        }
    }
}

/// Watches on other collections, each forwarding into this controller's queue.
///
/// A task rather than another arm of the `select!`: the number of related
/// collections is a property of the controller, and a loop that has to be
/// rewritten to watch one more thing is a loop that will be copied instead.
struct RelatedWatches(Vec<tokio::task::JoinHandle<()>>);

impl RelatedWatches {
    fn abort_all(&self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

fn spawn_related<R: Reconciler>(
    reconciler: &Arc<R>,
    raw: &Arc<dyn Store>,
    from: velstra_cloud_model::meta::Revision,
    queue: &Arc<WorkQueue>,
    sweep_now: &mpsc::Sender<()>,
) -> RelatedWatches {
    RelatedWatches(
        reconciler
            .related()
            .into_iter()
            .map(|related| {
                let mut events = raw.watch(&related.prefix, Some(from));
                let queue = queue.clone();
                let sweep_now = sweep_now.clone();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        let Some((_, _, name)) = parse_key(event.key()) else {
                            continue;
                        };
                        let value = match &event {
                            velstra_cloud_store::Event::Put(entry) => Some(entry.value.as_slice()),
                            velstra_cloud_store::Event::Delete { .. } => None,
                        };
                        match (related.wake)(Changed { name, value }) {
                            Wake::These(keys) => {
                                for key in keys {
                                    queue.add(&key);
                                }
                            }
                            // `try_send` on a channel of one, and a full channel
                            // is not a dropped signal: it means a sweep is
                            // already pending, and a second one would look at
                            // the same objects. A burst of related events
                            // coalesces into one pass, which is what keeps a
                            // sweep affordable at all.
                            Wake::All => {
                                let _ = sweep_now.try_send(());
                            }
                        }
                    }
                })
            })
            .collect(),
    )
}

/// Sleep, unless we are being shut down. Returns true if we are.
async fn wait_or_shutdown(how_long: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(how_long) => false,
        _ = shutdown.changed() => *shutdown.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{InstanceSpec, InstanceStatus},
    };
    use velstra_cloud_store::{MemoryStore, TypedStore};

    use super::*;

    /// Records what it was asked about, and can be told to fail.
    struct Recorder {
        seen: Mutex<Vec<String>>,
        passes: AtomicUsize,
        poison: Option<String>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                passes: AtomicUsize::new(0),
                poison: None,
            })
        }

        fn poisoning(key: &str) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                passes: AtomicUsize::new(0),
                poison: Some(key.to_string()),
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn count_of(&self, name: &str) -> usize {
            self.seen().iter().filter(|n| *n == name).count()
        }
    }

    impl Reconciler for Recorder {
        type Spec = InstanceSpec;
        type Status = InstanceStatus;

        fn name(&self) -> &'static str {
            "recorder"
        }

        async fn reconcile(
            &self,
            name: &str,
            _object: Option<&Resource<InstanceSpec, InstanceStatus>>,
        ) -> Result<()> {
            self.seen.lock().unwrap().push(name.to_string());
            self.passes.fetch_add(1, Ordering::SeqCst);
            if self.poison.as_deref() == Some(name) {
                return Err(crate::Error::Refused("this object can never work".into()));
            }
            Ok(())
        }
    }

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

    fn project(
        id: &str,
    ) -> Resource<
        velstra_cloud_model::resources::ProjectSpec,
        velstra_cloud_model::resources::ProjectStatus,
    > {
        Resource::new(
            Meta::new(
                ResourceName::parse(&format!("projects/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            velstra_cloud_model::resources::ProjectSpec::default(),
            velstra_cloud_model::resources::ProjectStatus::default(),
        )
    }

    fn config() -> LoopConfig {
        LoopConfig {
            resync: Duration::from_millis(500),
            rate: Duration::ZERO,
            backoff_base: Duration::from_millis(100),
            backoff_ceiling: Duration::from_secs(5),
        }
    }

    struct Running {
        handle: tokio::task::JoinHandle<()>,
        stop: watch::Sender<bool>,
    }

    impl Running {
        async fn stop(self) {
            let _ = self.stop.send(true);
            let _ = self.handle.await;
        }
    }

    fn start(
        reconciler: Arc<Recorder>,
        raw: Arc<MemoryStore>,
        config: LoopConfig,
    ) -> (Running, TypedStore<InstanceSpec, InstanceStatus>) {
        let store: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let (stop, rx) = watch::channel(false);
        let handle = tokio::spawn(run(
            reconciler,
            store.clone(),
            raw,
            config,
            Metrics::new(),
            rx,
        ));
        (Running { handle, stop }, store)
    }

    /// Give the loop a chance to make progress without asserting on wall time.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn what_existed_before_the_loop_started_is_reconciled() {
        let raw = Arc::new(MemoryStore::new());
        let seed: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        seed.create(&instance("i1")).await.unwrap();

        let recorder = Recorder::new();
        let (running, _) = start(recorder.clone(), raw, config());
        settle().await;
        running.stop().await;

        assert!(
            recorder
                .seen()
                .contains(&"projects/p1/instances/i1".to_string()),
            "an object that existed at startup was never looked at"
        );
    }

    #[tokio::test]
    async fn nothing_written_between_the_list_and_the_watch_is_lost() {
        // The race this exists to close: an object created in the window
        // between listing and subscribing. Watching from a revision read
        // *before* the list is what makes that window empty.
        let raw = Arc::new(MemoryStore::new());
        let seed: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let recorder = Recorder::new();
        let (running, _) = start(recorder.clone(), raw.clone(), config());

        for i in 0..20 {
            seed.create(&instance(&format!("i{i}"))).await.unwrap();
        }
        settle().await;
        running.stop().await;

        for i in 0..20 {
            let name = format!("projects/p1/instances/i{i}");
            assert!(
                recorder.seen().contains(&name),
                "{name} was never reconciled"
            );
        }
    }

    #[tokio::test]
    async fn a_dropped_watch_recovers_by_listing_again() {
        // A slow consumer is disconnected by the store rather than allowed to
        // grow a queue inside it. The controller must treat that as "catch up",
        // not as "exit".
        let raw = Arc::new(MemoryStore::new());
        let seed: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let recorder = Recorder::new();
        let (running, _) = start(recorder.clone(), raw.clone(), config());
        settle().await;

        // 1024 is the store's watch buffer; comfortably more than that, written
        // while the loop is busy, ends the stream.
        for i in 0..1500 {
            seed.create(&instance(&format!("i{i}"))).await.unwrap();
        }
        for _ in 0..40 {
            settle().await;
        }
        running.stop().await;

        let seen = recorder.seen();
        let distinct: std::collections::HashSet<&String> = seen.iter().collect();
        assert!(
            distinct.len() > 1400,
            "the controller died with the watch instead of re-listing: saw {} of 1500",
            distinct.len()
        );
    }

    #[tokio::test]
    async fn a_resync_looks_at_everything_again() {
        let raw = Arc::new(MemoryStore::new());
        let seed: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        seed.create(&instance("i1")).await.unwrap();

        let recorder = Recorder::new();
        let mut config = config();
        config.resync = Duration::from_millis(50);
        let (running, _) = start(recorder.clone(), raw, config);
        for _ in 0..10 {
            settle().await;
        }
        running.stop().await;

        assert!(
            recorder.count_of("projects/p1/instances/i1") > 1,
            "the resync never re-reconciled a quiet object"
        );
    }

    #[tokio::test]
    async fn an_object_that_can_never_reconcile_does_not_starve_its_neighbours() {
        let raw = Arc::new(MemoryStore::new());
        let seed: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        seed.create(&instance("poison")).await.unwrap();
        for i in 0..10 {
            seed.create(&instance(&format!("i{i}"))).await.unwrap();
        }

        let recorder = Recorder::poisoning("projects/p1/instances/poison");
        let mut config = config();
        config.resync = Duration::from_secs(3600);
        let (running, _) = start(recorder.clone(), raw, config);
        for _ in 0..5 {
            settle().await;
        }
        running.stop().await;

        let poison = recorder.count_of("projects/p1/instances/poison");
        assert!(poison >= 2, "the failing object was never retried");
        assert!(
            poison < 20,
            "the failing object was retried {poison} times: it is spinning, not backing off"
        );
        for i in 0..10 {
            let name = format!("projects/p1/instances/i{i}");
            assert!(
                recorder.count_of(&name) >= 1,
                "{name} was starved behind the poisoned object"
            );
        }
    }

    #[tokio::test]
    async fn a_related_collection_wakes_this_one() {
        struct Watcher {
            seen: Mutex<Vec<String>>,
        }

        impl Reconciler for Watcher {
            type Spec = velstra_cloud_model::resources::ProjectSpec;
            type Status = velstra_cloud_model::resources::ProjectStatus;

            fn name(&self) -> &'static str {
                "watcher"
            }

            fn related(&self) -> Vec<Related> {
                vec![Related::named(
                    velstra_cloud_store::prefix_for("cell-1", "instances"),
                    |name: &str| {
                        ResourceName::parse(name)
                            .ok()
                            .and_then(|n| n.project().map(|p| format!("projects/{p}")))
                            .into_iter()
                            .collect()
                    },
                )]
            }

            async fn reconcile(
                &self,
                name: &str,
                _object: Option<
                    &Resource<
                        velstra_cloud_model::resources::ProjectSpec,
                        velstra_cloud_model::resources::ProjectStatus,
                    >,
                >,
            ) -> Result<()> {
                self.seen.lock().unwrap().push(name.to_string());
                Ok(())
            }
        }

        let raw = Arc::new(MemoryStore::new());
        let instances: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let projects: TypedStore<
            velstra_cloud_model::resources::ProjectSpec,
            velstra_cloud_model::resources::ProjectStatus,
        > = TypedStore::new(raw.clone(), "cell-1", "projects");

        let watcher = Arc::new(Watcher {
            seen: Mutex::new(Vec::new()),
        });
        let (stop, rx) = watch::channel(false);
        let handle = tokio::spawn(run(
            watcher.clone(),
            projects,
            raw.clone(),
            config(),
            Metrics::new(),
            rx,
        ));
        settle().await;

        instances.create(&instance("i1")).await.unwrap();
        settle().await;
        let _ = stop.send(true);
        let _ = handle.await;

        assert!(
            watcher
                .seen
                .lock()
                .unwrap()
                .contains(&"projects/p1".to_string()),
            "an instance changing never woke the project that pays for it"
        );
    }
    #[tokio::test]
    async fn a_related_collection_can_wake_every_object_at_once() {
        // The case a mapping function cannot express: which of this controller's
        // objects a foreign change affects is not derivable from the foreign
        // object's *name*. Written as a mapping that returns nothing, it reads
        // like "look at all of them" and means "look at none of them" — which is
        // what shipped, and what nothing caught, because the controller that had
        // it was only ever tested by calling its `reconcile` directly.
        //
        // The resync is turned off here on purpose. With it on, this test would
        // pass on a broken fan-out simply by waiting.
        struct Sweeper {
            seen: Mutex<Vec<String>>,
        }

        impl Reconciler for Sweeper {
            type Spec = velstra_cloud_model::resources::ProjectSpec;
            type Status = velstra_cloud_model::resources::ProjectStatus;

            fn name(&self) -> &'static str {
                "sweeper"
            }

            fn related(&self) -> Vec<Related> {
                vec![Related::all(velstra_cloud_store::prefix_for(
                    "cell-1",
                    "instances",
                ))]
            }

            async fn reconcile(
                &self,
                name: &str,
                _object: Option<
                    &Resource<
                        velstra_cloud_model::resources::ProjectSpec,
                        velstra_cloud_model::resources::ProjectStatus,
                    >,
                >,
            ) -> Result<()> {
                self.seen.lock().unwrap().push(name.to_string());
                Ok(())
            }
        }

        let raw = Arc::new(MemoryStore::new());
        let instances: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(raw.clone(), "cell-1", "instances");
        let projects: TypedStore<
            velstra_cloud_model::resources::ProjectSpec,
            velstra_cloud_model::resources::ProjectStatus,
        > = TypedStore::new(raw.clone(), "cell-1", "projects");
        projects.create(&project("p1")).await.unwrap();
        projects.create(&project("p2")).await.unwrap();

        let sweeper = Arc::new(Sweeper {
            seen: Mutex::new(Vec::new()),
        });
        let mut config = config();
        config.resync = Duration::from_secs(3600);
        let (stop, rx) = watch::channel(false);
        let handle = tokio::spawn(run(
            sweeper.clone(),
            projects,
            raw.clone(),
            config,
            Metrics::new(),
            rx,
        ));
        settle().await;
        // Everything the startup list already reconciled. What matters is what
        // happens *after* this line.
        sweeper.seen.lock().unwrap().clear();

        // One instance, named after neither project.
        instances.create(&instance("i1")).await.unwrap();
        settle().await;
        let _ = stop.send(true);
        let _ = handle.await;

        let seen = sweeper.seen.lock().unwrap().clone();
        for name in ["projects/p1", "projects/p2"] {
            assert!(
                seen.contains(&name.to_string()),
                "{name} was never looked at again: {seen:?}"
            );
        }
    }
}
