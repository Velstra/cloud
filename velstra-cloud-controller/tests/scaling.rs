//! What one event costs, as the cell grows.
//!
//! Not a benchmark — a **shape** test. It counts the objects the store hands out
//! in response to one change, at three sizes, and asserts the answer does not
//! grow with the square of the cell. That is the difference between a design
//! that scales by adding cells and one that stops working inside the first one.
//!
//! The reason it exists: two controllers ask questions their own collection
//! cannot answer. A port belongs to the node running whichever instance uses it,
//! and an instance's name does not say which ports those are; a subnet's
//! occupancy is a fact about the ports on it, and a port's name does not say
//! which subnet. Both are reverse lookups, and the naive way to answer one is to
//! read everything — per object, on every event. Two of those multiply.
//!
//! Google's own answer to this is the same shape as Kubernetes': a cell is
//! bounded (Borg: ~10k machines per cell, one Borgmaster) and *inside* it,
//! reverse lookups are served from an index maintained off the same watch, never
//! from a scan. The bound is what makes the index affordable; the index is what
//! makes the bound reachable.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use velstra_cloud_controller::{port::PortController, runner::Reconciler};
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName},
    resources::{
        Instance, InstanceSpec, InstanceStatus, Port, PortSpec, PortStatus, Resource, Subnet,
        SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Store, StoreError, TypedStore};

/// A store that answers exactly like the one underneath and counts what it hands
/// out.
///
/// Entries rather than calls, deliberately: one `list` of ten thousand objects
/// and ten thousand `get`s cost the same at the far end of a network, and a
/// controller that got cheaper by batching its scans would not have got cheaper.
struct Counting {
    inner: Arc<MemoryStore>,
    entries: AtomicUsize,
}

impl Counting {
    fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            entries: AtomicUsize::new(0),
        })
    }
    fn reset(&self) {
        self.entries.store(0, Ordering::SeqCst);
    }
    fn read(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Store for Counting {
    async fn get(&self, key: &str) -> Result<Option<Entry>, StoreError> {
        let out = self.inner.get(key).await?;
        self.entries
            .fetch_add(out.is_some() as usize, Ordering::SeqCst);
        Ok(out)
    }
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StoreError> {
        let out = self.inner.list(prefix).await?;
        self.entries.fetch_add(out.len(), Ordering::SeqCst);
        Ok(out)
    }
    async fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<velstra_cloud_store::Page, StoreError> {
        // Counted like every other read. Forwarded rather than expressed in
        // terms of `list`, which would make a page cost a whole collection and
        // this counter report it.
        let out = self.inner.list_page(prefix, after, limit).await?;
        self.entries.fetch_add(out.entries.len(), Ordering::SeqCst);
        Ok(out)
    }
    async fn put(
        &self,
        key: &str,
        value: Vec<u8>,
        expect: Expect,
    ) -> Result<velstra_cloud_model::meta::Revision, StoreError> {
        self.inner.put(key, value, expect).await
    }
    async fn delete(
        &self,
        key: &str,
        expect: Expect,
    ) -> Result<velstra_cloud_model::meta::Revision, StoreError> {
        self.inner.delete(key, expect).await
    }
    fn watch(
        &self,
        prefix: &str,
        from: Option<velstra_cloud_model::meta::Revision>,
    ) -> tokio::sync::mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }
    async fn revision(&self) -> Result<velstra_cloud_model::meta::Revision, StoreError> {
        self.inner.revision().await
    }
}

fn instance(id: &str, ports: Vec<String>, node: Option<&str>) -> Instance {
    let mut i: Instance = Resource::new(
        Meta::new(
            ResourceName::parse(&format!("projects/p1/instances/{id}")).unwrap(),
            Placement::new("eu", "cell-1"),
        ),
        InstanceSpec {
            ports,
            ..InstanceSpec::default()
        },
        InstanceStatus::default(),
    );
    i.status.node = node.map(str::to_string);
    i
}

fn port(id: &str, subnet: &str) -> Port {
    Resource::new(
        ResourceName::parse(&format!("projects/p1/ports/{id}"))
            .map(|n| Meta::new(n, Placement::new("eu", "cell-1")))
            .unwrap(),
        PortSpec {
            network: "projects/p1/networks/n1".into(),
            subnet: subnet.into(),
            ..PortSpec::default()
        },
        PortStatus::default(),
    )
}

fn subnet(id: &str) -> Subnet {
    Resource::new(
        ResourceName::parse(&format!("projects/p1/subnets/{id}"))
            .map(|n| Meta::new(n, Placement::new("eu", "cell-1")))
            .unwrap(),
        SubnetSpec {
            network: "projects/p1/networks/n1".into(),
            cidr: "10.0.0.0/8".into(),
            gateway: "10.0.0.1".into(),
            dns: vec![],
            reserved: vec![],
        },
        SubnetStatus::default(),
    )
}

/// A cell of `n` instances, each with one port, spread over `n / 10` subnets.
async fn cell_of(n: usize) -> (Arc<Counting>, Vec<Port>, Vec<Subnet>) {
    let raw = Arc::new(MemoryStore::new());
    let counting = Counting::new(raw);
    let store: Arc<dyn Store> = counting.clone();
    let instances: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), "cell-1", "instances");
    let ports: TypedStore<PortSpec, PortStatus> = TypedStore::new(store.clone(), "cell-1", "ports");
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(store.clone(), "cell-1", "subnets");

    let subnet_count = (n / 10).max(1);
    let mut made_subnets = Vec::new();
    for s in 0..subnet_count {
        let object = subnet(&format!("s{s}"));
        subnets
            .create(
                &object,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
        made_subnets.push(
            subnets
                .get(&object.meta.name.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let mut made_ports = Vec::new();
    for i in 0..n {
        let s = format!("projects/p1/subnets/s{}", i % subnet_count);
        let object = port(&format!("pt{i}"), &s);
        ports
            .create(
                &object,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
        made_ports.push(
            ports
                .get(&object.meta.name.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        instances
            .create(
                &instance(
                    &format!("i{i}"),
                    vec![format!("projects/p1/ports/pt{i}")],
                    Some("node-a"),
                ),
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
    }
    (counting, made_ports, made_subnets)
}

/// How the cost of one instance moving grows with the size of the cell.
///
/// One instance changing node is the commonest event a cell has, and every port
/// it uses has to be re-pointed. What must not happen is that *every other*
/// port is re-examined too, each one reading *every* instance.
#[tokio::test]
async fn one_instance_moving_does_not_cost_the_whole_cell() {
    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, ports, _) = cell_of(n).await;
        let store: Arc<dyn Store> = counting.clone();
        // The mirror is warmed here on purpose, and it does not distort the
        // measurement: what is counted is the cost of *one more* event once the
        // cell is up, which is what a running cell pays. The first list is paid
        // once per process, whatever the design.
        let mirror = velstra_cloud_store::Cached::start(
            TypedStore::new(store.clone(), "cell-1", "instances"),
            store.clone(),
            velstra_cloud_store::prefix_for("cell-1", "instances"),
        );
        mirror.all().await;
        let controller = PortController::new(
            TypedStore::new(store.clone(), "cell-1", "ports"),
            mirror,
            "cell-1",
        );

        // What the runner does when one instance changes: reconcile whatever
        // that event fans out to. The fan-out itself is measured separately
        // below; this is the cost of the reconciles it causes.
        counting.reset();
        controller
            .reconcile(&ports[0].meta.name.to_string(), Some(&ports[0]))
            .await
            .unwrap();
        measured.push((n, counting.read()));
    }

    for (n, cost) in &measured {
        println!("  {n:>4} instances → {cost:>6} objects read to re-point one port");
    }
    let (small, big) = (measured[0].1, measured[2].1);
    let growth = big as f64 / small.max(1) as f64;
    assert!(
        growth < 2.0,
        "re-pointing one port costs {growth:.0}x more in a 20x bigger cell — it reads the \
         whole instance collection, so a cell of 10k instances pays 10k reads per port and a \
         sweep pays 100M: {measured:?}"
    );
}

/// The controllers this file did not yet measure: what one resync of each costs
/// as the cell grows. The port controller was fixed after this test caught it
/// reading the cell per object; these three have the same shape and were not
/// looked at until the audit asked.
#[tokio::test]
async fn a_resync_of_every_controller_grows_with_the_cell_not_its_square() {
    use velstra_cloud_controller::{quota::QuotaController, scheduler::Scheduler};
    use velstra_cloud_model::resources::{
        NodeSpec, NodeStatus, ProjectSpec, ProjectStatus, VolumeSpec, VolumeStatus,
    };

    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let raw = Arc::new(MemoryStore::new());
        let counting = Counting::new(raw);
        let store: Arc<dyn Store> = counting.clone();
        let instances: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(store.clone(), "cell-1", "instances");
        let projects: TypedStore<ProjectSpec, ProjectStatus> =
            TypedStore::new(store.clone(), "cell-1", "projects");
        let nodes: TypedStore<NodeSpec, NodeStatus> =
            TypedStore::new(store.clone(), "cell-1", "nodes");
        // One project per ten instances, one node per ten.
        for p in 0..(n / 10).max(1) {
            projects
                .create(
                    &Resource::new(
                        ResourceName::parse(&format!("projects/p{p}"))
                            .map(|nm| Meta::new(nm, Placement::new("eu", "cell-1")))
                            .unwrap(),
                        ProjectSpec::default(),
                        ProjectStatus::default(),
                    ),
                    &velstra_cloud_model::access::Writer::controller("test"),
                )
                .await
                .unwrap();
            nodes
                .create(
                    &Resource::new(
                        ResourceName::parse(&format!("nodes/node-{p}"))
                            .map(|nm| Meta::new(nm, Placement::new("eu", "cell-1")))
                            .unwrap(),
                        NodeSpec::default(),
                        NodeStatus::default(),
                    ),
                    &velstra_cloud_model::access::Writer::controller("test"),
                )
                .await
                .unwrap();
        }
        for i in 0..n {
            // Placed and held: `spec.node` set so the scheduler has nothing to
            // do, `status.node` set so the world is settled. What is being
            // measured is the cost of confirming that, which is what a resync
            // of a quiet cell is made of.
            let mut object = instance(&format!("i{i}"), vec![], Some("node-0"));
            object.spec.node = Some("node-0".into());
            instances
                .create(
                    &object,
                    &velstra_cloud_model::access::Writer::controller("test"),
                )
                .await
                .unwrap();
        }

        // A quota resync: every project, through the controller's own reconcile.
        let quota = QuotaController::new(
            velstra_cloud_store::Cached::start(
                instances.clone(),
                store.clone(),
                velstra_cloud_store::prefix_for("cell-1", "instances"),
            ),
            velstra_cloud_store::Cached::start(
                TypedStore::<VolumeSpec, VolumeStatus>::new(store.clone(), "cell-1", "volumes"),
                store.clone(),
                velstra_cloud_store::prefix_for("cell-1", "volumes"),
            ),
            velstra_cloud_store::Cached::start(
                TypedStore::<
                    velstra_cloud_model::resources::FloatingIpSpec,
                    velstra_cloud_model::resources::FloatingIpStatus,
                >::new(store.clone(), "cell-1", "floatingips"),
                store.clone(),
                velstra_cloud_store::prefix_for("cell-1", "floatingips"),
            ),
            velstra_cloud_store::Cached::start(
                TypedStore::<
                    velstra_cloud_model::loadbalancer::LoadBalancerSpec,
                    velstra_cloud_model::loadbalancer::LoadBalancerStatus,
                >::new(store.clone(), "cell-1", "load-balancers"),
                store.clone(),
                velstra_cloud_store::prefix_for("cell-1", "load-balancers"),
            ),
            velstra_cloud_controller::status::StatusWriter::new(
                store.clone(),
                "cell-1",
                "projects",
                "quota",
            ),
            "cell-1",
        );
        counting.reset();
        for project in projects.list().await.unwrap() {
            let name = project.meta.name.to_string();
            quota.reconcile(&name, Some(&project)).await.unwrap();
        }
        let quota_cost = counting.read();

        // A scheduler resync: every instance. They are all placed, so the
        // reconcile should be cheap — is it?
        let scheduler = Scheduler::new(
            instances.clone(),
            nodes.clone(),
            velstra_cloud_controller::status::StatusWriter::new(
                store.clone(),
                "cell-1",
                "instances",
                "scheduler",
            ),
            "cell-1",
        );
        counting.reset();
        for object in instances.list().await.unwrap() {
            let name = object.meta.name.to_string();
            scheduler.reconcile(&name, Some(&object)).await.unwrap();
        }
        let scheduler_cost = counting.read();

        measured.push((n, quota_cost, scheduler_cost));
    }
    for (n, q, s) in &measured {
        println!("  {n:>4} instances → quota resync {q:>7} reads, scheduler resync {s:>7} reads");
    }
    let growth = |small: usize, big: usize| big as f64 / small.max(1) as f64;
    let (q_growth, s_growth) = (
        growth(measured[0].1, measured[2].1),
        growth(measured[0].2, measured[2].2),
    );
    assert!(
        q_growth < 40.0,
        "a quota resync grows {q_growth:.0}x for a 20x cell — every project reads every \
         instance, which is projects × instances: {measured:?}"
    );
    assert!(
        s_growth < 40.0,
        "a scheduler resync grows {s_growth:.0}x for a 20x cell: {measured:?}"
    );
}
