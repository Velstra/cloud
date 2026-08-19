//! The four claims this design is making, tested against all of it at once.
//!
//! Each of these fails loudly if the level-triggered discipline is broken
//! somewhere, in a way no unit test would catch: a controller that writes what
//! it computed without comparing, a decision that is durable before it is
//! committed, a placement that is not a compare-and-swap.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use velstra_cloud_controller::{
    LoopConfig, Metrics, attachment::AttachmentController, operations::OperationsController,
    quota::QuotaController, run, run_when_leading, scheduler::Scheduler, status::StatusWriter,
    sweep,
};
use velstra_cloud_model::{
    Condition,
    meta::{Meta, Placement, ResourceName, Revision, Timestamp, set_condition},
    resources::{
        Attachment, AttachmentSpec, AttachmentStatus, Capacity, Instance, InstanceSpec,
        InstanceState, InstanceStatus, NODE_RELEASE_FINALIZER, Node, NodeSpec, NodeStatus,
        Operation, OperationSpec, OperationStatus, Project, ProjectSpec, ProjectStatus, Quota,
        Resource, Volume, VolumeSpec, VolumeStatus,
    },
};
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Store, TypedStore};

// ---- a cell, and the pieces to fill it -----------------------------------

struct Cell {
    raw: Arc<dyn Store>,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    volumes: TypedStore<VolumeSpec, VolumeStatus>,
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    projects: TypedStore<ProjectSpec, ProjectStatus>,
    operations: TypedStore<OperationSpec, OperationStatus>,
}

impl Cell {
    fn on(raw: Arc<dyn Store>) -> Self {
        Self {
            instances: TypedStore::new(raw.clone(), "cell-1", "instances"),
            nodes: TypedStore::new(raw.clone(), "cell-1", "nodes"),
            volumes: TypedStore::new(raw.clone(), "cell-1", "volumes"),
            attachments: TypedStore::new(raw.clone(), "cell-1", "attachments"),
            projects: TypedStore::new(raw.clone(), "cell-1", "projects"),
            operations: TypedStore::new(raw.clone(), "cell-1", "operations"),
            raw,
        }
    }

    fn new() -> Self {
        Self::on(Arc::new(MemoryStore::new()))
    }

    fn scheduler(&self) -> Scheduler {
        Scheduler::new(
            self.instances.clone(),
            self.nodes.clone(),
            StatusWriter::new(self.raw.clone(), "cell-1", "instances", "scheduler"),
            "cell-1",
        )
    }

    fn quota(&self) -> QuotaController {
        QuotaController::new(
            velstra_cloud_store::Cached::start(
                self.instances.clone(),
                self.raw.clone(),
                velstra_cloud_store::prefix_for("cell-1", "instances"),
            ),
            velstra_cloud_store::Cached::start(
                self.volumes.clone(),
                self.raw.clone(),
                velstra_cloud_store::prefix_for("cell-1", "volumes"),
            ),
            StatusWriter::new(self.raw.clone(), "cell-1", "projects", "quota"),
            "cell-1",
        )
    }

    fn operations(&self) -> OperationsController {
        OperationsController::new(
            self.raw.clone(),
            StatusWriter::new(self.raw.clone(), "cell-1", "operations", "operations"),
            "cell-1",
        )
    }

    fn attachment(&self) -> AttachmentController {
        AttachmentController::new(self.attachments.clone())
    }

    /// Every controller, once, in the order a process would start them.
    async fn reconcile_everything(&self) {
        sweep(&self.scheduler(), &self.instances).await.unwrap();
        sweep(&self.attachment(), &self.attachments).await.unwrap();
        sweep(&self.quota(), &self.projects).await.unwrap();
        sweep(&self.operations(), &self.operations).await.unwrap();
    }

    async fn instance(&self, id: &str) -> Instance {
        self.instances
            .get(&format!("projects/p1/instances/{id}"))
            .await
            .unwrap()
            .unwrap()
    }
}

fn meta(name: &str) -> Meta {
    Meta::new(
        ResourceName::parse(name).unwrap(),
        Placement::new("eu-central", "cell-1"),
    )
}

fn node(id: &str) -> Node {
    let mut n = Resource::new(
        meta(&format!("nodes/{id}")),
        NodeSpec {
            schedulable: true,
            labels: vec![],
        },
        NodeStatus {
            observed_generation: 1,
            capacity: Capacity {
                vcpus: 32,
                memory_mib: 65536,
                disk_gib: 4000,
                numa_free_mib: vec![32768, 32768],
                hugepages_1gi: 0,
            },
            ..Default::default()
        },
    );
    set_condition(&mut n.status.conditions, Condition::ready(1));
    n
}

fn unplaced(id: &str) -> Instance {
    Resource::new(
        meta(&format!("projects/p1/instances/{id}")),
        InstanceSpec {
            vcpus: 2,
            memory_mib: 2048,
            image: "sha256:abc".into(),
            root_disk_gib: 20,
            ..Default::default()
        },
        InstanceStatus::default(),
    )
}

// ---- 1. a settled cluster is free to reconcile ---------------------------

/// Build a cluster in which every object already says what a full reconcile
/// would make it say.
async fn settled_cell() -> Cell {
    let cell = Cell::new();
    cell.nodes.create(&node("node-a")).await.unwrap();

    let mut instance = unplaced("i1");
    instance.spec.node = Some("node-a".into());
    instance.meta.generation = 2;
    instance.status.observed_generation = 2;
    instance.status.node = Some("node-a".into());
    instance.status.state = InstanceState::Running;
    set_condition(&mut instance.status.conditions, Condition::ready(2));
    cell.instances.create(&instance).await.unwrap();

    let volume: Volume = Resource::new(
        meta("projects/p1/volumes/v1"),
        VolumeSpec {
            size_gib: 100,
            pool: "rbd".into(),
            ..Default::default()
        },
        VolumeStatus {
            observed_generation: 1,
            provisioned: true,
            actual_size_gib: 100,
            ..Default::default()
        },
    );
    cell.volumes.create(&volume).await.unwrap();

    let mut attachment: Attachment = Resource::new(
        meta("projects/p1/attachments/a1"),
        AttachmentSpec {
            volume: "projects/p1/volumes/v1".into(),
            instance: "projects/p1/instances/i1".into(),
            node: "node-a".into(),
            read_only: false,
        },
        AttachmentStatus {
            observed_generation: 1,
            attached: true,
            device: Some("/dev/vdb".into()),
            node: Some("node-a".into()),
            ..Default::default()
        },
    );
    attachment.meta.add_finalizer(NODE_RELEASE_FINALIZER);
    cell.attachments.create(&attachment).await.unwrap();

    let mut project: Project = Resource::new(
        meta("projects/p1"),
        ProjectSpec {
            display_name: "one".into(),
            parent: "organizations/o1".into(),
            quota: Quota {
                instances: 10,
                vcpus: 64,
                memory_mib: 131072,
                volume_gib: 1000,
            },
            bindings: Vec::new(),
            cell: String::new(),
        },
        ProjectStatus {
            observed_generation: 1,
            used: Quota {
                instances: 1,
                vcpus: 2,
                memory_mib: 2048,
                volume_gib: 120,
            },
            ..Default::default()
        },
    );
    set_condition(&mut project.status.conditions, Condition::ready(1));
    cell.projects.create(&project).await.unwrap();

    let mut operation: Operation = Resource::new(
        meta("projects/p1/operations/op-1"),
        OperationSpec {
            target: "projects/p1/instances/i1".into(),
            target_generation: 2,
            verb: "create".into(),
            requested_by: "someone".into(),
        },
        OperationStatus {
            observed_generation: 1,
            done: true,
            error: None,
            // Recently, not in 2023. A finished operation past its retention is
            // not settled — it has one thing left to do, which is to go — so a
            // fixture dated years ago described a cluster with work in it and
            // the pass below rightly wrote. See `operations::keeping_for`.
            finished_at: Some(Timestamp::now()),
            ..Default::default()
        },
    );
    set_condition(&mut operation.status.conditions, Condition::ready(1));
    cell.operations.create(&operation).await.unwrap();

    cell
}

/// The same pass over a cluster carrying a *stale* record does write, exactly
/// once, to remove it.
///
/// Its own test rather than an extra assertion in the settled one, so the
/// difference between "nothing to do" and "one thing to do" is visible rather
/// than a fixture detail somebody has to notice. An operation finished long
/// enough ago has one thing left: to go.
#[tokio::test]
async fn a_record_past_its_retention_is_removed_by_an_ordinary_pass() {
    let cell = settled_cell().await;
    let mut old = cell
        .operations
        .get("projects/p1/operations/op-1")
        .await
        .unwrap()
        .unwrap();
    old.meta.name = ResourceName::parse("projects/p1/operations/op-old").unwrap();
    old.meta.revision = Revision(0);
    old.status.finished_at = Some(Timestamp(1_700_000_000_000));
    cell.operations.create(&old).await.unwrap();

    cell.reconcile_everything().await;
    assert!(
        cell.operations
            .get("projects/p1/operations/op-old")
            .await
            .unwrap()
            .is_none(),
        "an operation finished years ago is still in the store"
    );
    assert!(
        cell.operations
            .get("projects/p1/operations/op-1")
            .await
            .unwrap()
            .is_some(),
        "a recent operation was removed with the stale one"
    );
}

#[tokio::test]
async fn a_reconcile_pass_over_a_settled_cluster_writes_nothing() {
    // The load-bearing claim of the whole design. If a resync churns the store,
    // then what was built is not level-triggered reconciliation: it is a cron
    // job that rewrites the cluster every few minutes, wakes every watcher in
    // it, and makes the resync interval a capacity decision instead of a
    // latency one.
    let cell = settled_cell().await;
    let before = cell.raw.revision().await.unwrap();

    cell.reconcile_everything().await;
    assert_eq!(
        cell.raw.revision().await.unwrap(),
        before,
        "a reconcile of a settled cluster wrote to the store"
    );

    // And again, because "idempotent" means the second pass is like the first.
    cell.reconcile_everything().await;
    assert_eq!(cell.raw.revision().await.unwrap(), before);
}

// ---- 2. a controller that dies mid-flight -------------------------------

/// A store that stops accepting writes after `writes`, the way a process stops
/// existing: no warning, and nothing half-written.
struct DiesAfter {
    inner: Arc<MemoryStore>,
    left: AtomicUsize,
}

impl DiesAfter {
    fn wrapping(inner: Arc<MemoryStore>, writes: usize) -> Arc<dyn Store> {
        Arc::new(Self {
            inner,
            left: AtomicUsize::new(writes),
        })
    }
}

#[async_trait]
impl Store for DiesAfter {
    async fn get(&self, key: &str) -> velstra_cloud_store::Result<Option<Entry>> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> velstra_cloud_store::Result<Vec<Entry>> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> velstra_cloud_store::Result<velstra_cloud_store::Page> {
        self.inner.list_page(prefix, after, limit).await
    }

    async fn put(
        &self,
        key: &str,
        value: Vec<u8>,
        expect: Expect,
    ) -> velstra_cloud_store::Result<Revision> {
        if self
            .left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_err()
        {
            return Err(velstra_cloud_store::StoreError::Backend(
                "the controller died here".into(),
            ));
        }
        self.inner.put(key, value, expect).await
    }

    async fn delete(&self, key: &str, expect: Expect) -> velstra_cloud_store::Result<Revision> {
        self.inner.delete(key, expect).await
    }

    fn watch(&self, prefix: &str, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }

    async fn revision(&self) -> velstra_cloud_store::Result<Revision> {
        self.inner.revision().await
    }
}

#[tokio::test]
async fn a_controller_that_dies_before_its_write_leaves_the_object_untouched() {
    let backing = Arc::new(MemoryStore::new());
    let seed = Cell::on(backing.clone());
    seed.nodes.create(&node("node-a")).await.unwrap();
    seed.instances.create(&unplaced("i1")).await.unwrap();
    let before = seed.instance("i1").await;

    // Dies on its first write — after `place()` has already chosen a node.
    let dying = Cell::on(DiesAfter::wrapping(backing.clone(), 0));
    let err = sweep(&dying.scheduler(), &dying.instances)
        .await
        .unwrap_err();
    assert!(!err.is_conflict(), "{err}");

    let after_crash = seed.instance("i1").await;
    assert_eq!(
        after_crash.meta.revision, before.meta.revision,
        "a decision that was never committed left something behind"
    );
    assert!(after_crash.spec.node.is_none());

    // A fresh process makes the same decision and completes it.
    let restarted = Cell::on(backing.clone());
    sweep(&restarted.scheduler(), &restarted.instances)
        .await
        .unwrap();
    let converged = seed.instance("i1").await;
    assert_eq!(converged.spec.node.as_deref(), Some("node-a"));
    assert_eq!(converged.meta.generation, 2);
}

#[tokio::test]
async fn a_controller_that_dies_between_two_writes_needs_no_repair() {
    // The scheduler corrects a stale rejection and then assigns: two writes,
    // and the interesting crash is between them. What must not happen is an
    // object a human has to fix — a half-applied decision, or one that can
    // never be made again.
    let backing = Arc::new(MemoryStore::new());
    let seed = Cell::on(backing.clone());
    seed.instances.create(&unplaced("i1")).await.unwrap();

    // No nodes yet: the instance collects a rejection.
    sweep(&seed.scheduler(), &seed.instances).await.unwrap();
    seed.nodes.create(&node("node-a")).await.unwrap();

    // Now exactly one write survives — the condition — and the process dies
    // before the assignment.
    let dying = Cell::on(DiesAfter::wrapping(backing.clone(), 1));
    assert!(sweep(&dying.scheduler(), &dying.instances).await.is_err());

    let stranded = seed.instance("i1").await;
    assert_eq!(
        velstra_cloud_model::meta::condition(&stranded.status.conditions, "Ready")
            .unwrap()
            .reason,
        "Scheduled",
        "the crash landed before the first write, so this proves nothing about the second"
    );
    assert!(
        stranded.spec.node.is_none(),
        "half of a decision became durable"
    );
    assert_eq!(
        stranded.meta.generation, 1,
        "a generation moved without a spec change"
    );

    let restarted = Cell::on(backing.clone());
    sweep(&restarted.scheduler(), &restarted.instances)
        .await
        .unwrap();
    let converged = seed.instance("i1").await;
    assert_eq!(converged.spec.node.as_deref(), Some("node-a"));
    assert_eq!(converged.meta.generation, 2);
    assert!(
        converged.meta.finalizers.is_empty(),
        "a crash left a guard nobody will remove"
    );

    // And the settled object is now free to reconcile, which is the real
    // definition of "nothing to repair".
    let revision = backing.revision().await.unwrap();
    restarted.reconcile_everything().await;
    assert_eq!(backing.revision().await.unwrap(), revision);
}

// ---- 3. two schedulers on one cluster ------------------------------------

#[tokio::test]
async fn two_schedulers_place_each_instance_exactly_once() {
    // Two replicas, no leader election, no lock. The only thing keeping them
    // honest is that an assignment is a compare-and-swap on the instance — so
    // the loser is told the object moved, reads it again, and finds nothing to
    // do. A double placement would show up as a generation of three.
    let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let cell = Cell::on(raw.clone());
    for id in ["node-a", "node-b", "node-c"] {
        cell.nodes.create(&node(id)).await.unwrap();
    }
    for i in 0..25 {
        cell.instances
            .create(&unplaced(&format!("i{i}")))
            .await
            .unwrap();
    }

    let config = LoopConfig {
        resync: std::time::Duration::from_millis(50),
        rate: std::time::Duration::ZERO,
        backoff_base: std::time::Duration::from_millis(20),
        backoff_ceiling: std::time::Duration::from_millis(200),
    };
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let mut both = Vec::new();
    for _ in 0..2 {
        both.push(tokio::spawn(run(
            Arc::new(cell.scheduler()),
            cell.instances.clone(),
            raw.clone(),
            config,
            Metrics::new(),
            shutdown.clone(),
        )));
    }

    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        if cell
            .instances
            .list()
            .await
            .unwrap()
            .iter()
            .all(|i| i.spec.node.is_some())
        {
            break;
        }
    }
    let _ = stop.send(true);
    for handle in both {
        let _ = handle.await;
    }

    let placed = cell.instances.list().await.unwrap();
    assert_eq!(placed.len(), 25);
    for instance in &placed {
        assert!(
            instance.spec.node.is_some(),
            "{} was never placed",
            instance.meta.name
        );
        assert_eq!(
            instance.meta.generation, 2,
            "{} was assigned more than once",
            instance.meta.name
        );
    }
}

/// A controller stood down and then given the lease back still works.
///
/// This is here because of a bug written while adding leader election and caught
/// before it shipped: standing down closed the work queue, and closing it is
/// permanent — so a process that lost the lease and won it again came back with
/// a queue that would never hand out another name. It would have looked like a
/// controller that leads and does nothing, which is the hardest failure to see
/// from outside: the lease record is healthy, the logs say "elected", and no
/// object moves.
///
/// The queue now belongs to the establishment rather than to the process, so a
/// stand-down drops it and a takeover builds a fresh one along the same path a
/// cold start takes.
#[tokio::test]
async fn a_controller_that_stands_down_and_returns_still_reconciles() {
    let cell = Cell::new();
    let raw = cell.raw.clone();
    for id in ["node-a", "node-b"] {
        cell.nodes.create(&node(id)).await.unwrap();
    }

    let config = LoopConfig {
        resync: std::time::Duration::from_millis(30),
        rate: std::time::Duration::ZERO,
        backoff_base: std::time::Duration::from_millis(20),
        backoff_ceiling: std::time::Duration::from_millis(100),
    };
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (lead, leader) = tokio::sync::watch::channel(true);

    let handle = tokio::spawn(run_when_leading(
        Arc::new(cell.scheduler()),
        cell.instances.clone(),
        raw.clone(),
        config,
        Metrics::new(),
        shutdown,
        leader,
    ));

    // Leading: it places what it is given.
    cell.instances.create(&unplaced("before")).await.unwrap();
    assert!(
        placed(&cell, "before").await,
        "the controller did not place an instance while it was leading"
    );

    // Stood down: it must not act at all.
    lead.send(false).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    cell.instances.create(&unplaced("during")).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let during = cell
        .instances
        .get("projects/p1/instances/during")
        .await
        .unwrap()
        .unwrap();
    assert!(
        during.spec.node.is_none(),
        "a follower placed an instance it had no lease to touch"
    );

    // Given the lease back: it picks up everything, including what arrived while
    // it was standing down.
    lead.send(true).unwrap();
    assert!(
        placed(&cell, "during").await,
        "the controller never acted again after being given the lease back"
    );

    let _ = stop.send(true);
    let _ = handle.await;
}

/// Wait for an instance to be scheduled, or give up.
async fn placed(cell: &Cell, id: &str) -> bool {
    let name = format!("projects/p1/instances/{id}");
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Ok(Some(i)) = cell.instances.get(&name).await
            && i.spec.node.is_some()
        {
            return true;
        }
    }
    false
}
