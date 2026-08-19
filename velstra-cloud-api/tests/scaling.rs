//! What a list costs, as the cell grows.
//!
//! The API computes several fields rather than storing them — an operation's
//! `done`, a migration's `Moved`, which nodes hold an image, whether a security
//! group is in force, how full a subnet is. Each of those is a judgement about
//! *other* objects, and each is right to be computed: a stored copy of a fact
//! about something else goes stale, and an operator believes the stale copy and
//! debugs the wrong thing.
//!
//! What is easy to miss is that the function filling them in runs **per
//! document**. A field that needs a collection scan then costs one scan per
//! item, so listing a thousand security groups in a cell of ten thousand ports
//! was ten million reads — quadratic in the size of the cell, on an ordinary
//! read.
//!
//! This counts the objects the store hands out for one list, at three sizes, and
//! asserts the answer grows with the cell rather than with its square.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::json;
use velstra_cloud_api::{Api, Filter, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::meta::Revision;
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Store, StoreError};

struct Counting {
    inner: Arc<MemoryStore>,
    entries: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(MemoryStore::new()),
            entries: AtomicUsize::new(0),
        })
    }
    fn reset(&self) {
        self.entries.store(0, Ordering::SeqCst);
    }
    fn read(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }
    fn inner_watchers(&self) -> usize {
        self.inner.watchers()
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
        // Forwarded, not expressed in terms of `list`. A decorator that reached
        // for the whole collection here would make every page cost a cell, and
        // this counter — the one written to prove that cost is gone — would
        // dutifully report it as gone anyway, because it counts what it is
        // handed rather than what was read underneath.
        let out = self.inner.list_page(prefix, after, limit).await?;
        self.entries.fetch_add(out.entries.len(), Ordering::SeqCst);
        Ok(out)
    }
    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision, StoreError> {
        self.inner.put(key, value, expect).await
    }
    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision, StoreError> {
        self.inner.delete(key, expect).await
    }
    fn watch(&self, prefix: &str, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }
    async fn revision(&self) -> Result<Revision, StoreError> {
        self.inner.revision().await
    }
}

/// A cell with `n` ports spread over `n / 10` subnets, and one security group
/// that every port names.
async fn cell_of(n: usize) -> (Arc<Counting>, Api) {
    let counting = Counting::new();
    let store: Arc<dyn Store> = counting.clone();
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    // An operator: this harness creates projects and pools, which are the
    // cell's. What it measures is cost, not permission.
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec!["scaling-test".into()]);
    let who = Identity::new("scaling-test");

    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who,
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who,
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "security-groups",
        &json!({"id": "g1", "spec": {"rules": []}}),
        &who,
    )
    .await
    .unwrap();

    let subnets = (n / 10).max(1);
    for s in 0..subnets {
        api.create(
            "projects/p1",
            "subnets",
            &json!({"id": format!("s{s}"), "spec": {
                "network": "projects/p1/networks/n1",
                "cidr": "10.0.0.0/8",
                "gateway": "10.0.0.1",
                "dns": [],
                "reserved": []
            }}),
            &who,
        )
        .await
        .unwrap();
    }
    for i in 0..n {
        api.create(
            "projects/p1",
            "ports",
            &json!({"id": format!("pt{i}"), "spec": {
                "network": "projects/p1/networks/n1",
                "subnet": format!("projects/p1/subnets/s{}", i % subnets),
                "securityGroups": ["projects/p1/securityGroups/g1"]
            }}),
            &who,
        )
        .await
        .unwrap();
    }
    (counting, api)
}

/// Listing subnets fills in each one's occupancy from the ports on it. What must
/// not happen is that every subnet re-reads every port.
#[tokio::test]
async fn listing_subnets_does_not_read_the_ports_once_per_subnet() {
    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, api) = cell_of(n).await;
        counting.reset();
        let listing = api.list("projects/p1", "subnets").await.unwrap();
        measured.push((n, listing.items.len(), counting.read()));
    }
    for (n, subnets, cost) in &measured {
        println!("  {n:>4} ports, {subnets:>3} subnets → {cost:>6} objects read for one list");
    }

    // Quadratic would be subnets × ports: 40 × 400 = 16000 at the top size,
    // against 2 × 20 = 40 at the bottom — four hundred times more. Linear is
    // twenty times more, for a cell twenty times bigger.
    let (small, big) = (measured[0].2, measured[2].2);
    let growth = big as f64 / small.max(1) as f64;
    assert!(
        growth < 40.0,
        "one list costs {growth:.0}x more in a 20x bigger cell, so it is reading the ports once \
         per subnet: {measured:?}"
    );

    // And the answer is right, not merely cheap.
    let (_, api) = cell_of(20).await;
    let listing = api.list("projects/p1", "subnets").await.unwrap();
    let first = &listing.items[0];
    assert_eq!(
        first["status"]["allocated"], 0,
        "a subnet with no addresses handed out says otherwise"
    );
    assert!(
        first["status"]["available"].as_u64().unwrap_or(0) > 0,
        "a subnet with a whole /8 free reports none: {first}"
    );
}

/// The same for security groups, which had this shape before subnets did.
#[tokio::test]
async fn listing_security_groups_does_not_read_the_ports_once_per_group() {
    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, api) = cell_of(n).await;
        counting.reset();
        api.list("projects/p1", "security-groups").await.unwrap();
        measured.push((n, counting.read()));
    }
    for (n, cost) in &measured {
        println!("  {n:>4} ports → {cost:>6} objects read to list the groups");
    }
    let (small, big) = (measured[0].1, measured[2].1);
    assert!((big as f64 / small.max(1) as f64) < 40.0, "{measured:?}");
}

/// A cell of `n` instances spread over `nodes` nodes, each instance holding one
/// port that is carried by the same node.
async fn cell_on_nodes(n: usize, nodes: usize) -> (Arc<Counting>, Api) {
    let counting = Counting::new();
    let store: Arc<dyn Store> = counting.clone();
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    // An operator: this harness creates projects and pools, which are the
    // cell's. What it measures is cost, not permission.
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec!["scaling-test".into()]);
    let who = Identity::new("scaling-test");

    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who,
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who,
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "subnets",
        &json!({"id": "s1", "spec": {
            "network": "projects/p1/networks/n1",
            "cidr": "10.0.0.0/8",
            "gateway": "10.0.0.1",
            "dns": [],
            "reserved": []
        }}),
        &who,
    )
    .await
    .unwrap();

    for i in 0..n {
        let node = format!("node-{}", i % nodes);
        api.create(
            "projects/p1",
            "ports",
            &json!({"id": format!("pt{i}"), "spec": {
                "network": "projects/p1/networks/n1",
                "subnet": "projects/p1/subnets/s1",
                "node": node,
                "securityGroups": []
            }}),
            &who,
        )
        .await
        .unwrap();
        api.create(
            "projects/p1",
            "instances",
            &json!({"id": format!("i{i}"), "spec": {
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 1,
                "desiredState": "Running",
                "node": node,
                "ports": [format!("projects/p1/ports/pt{i}")]
            }}),
            &who,
        )
        .await
        .unwrap();
    }
    (counting, api)
}

/// The claim the whole node-agent design rests on: what one node reads does not
/// grow with the cell.
///
/// Ten nodes share the cell, so a node's own share is a tenth of it. Without the
/// filter a node lists everything and the answer grows one-for-one with the
/// cell; with it, a node's list grows with its own objects.
#[tokio::test]
async fn one_node_is_not_handed_the_cell() {
    let mut unfiltered = Vec::new();
    let mut filtered = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, api) = cell_on_nodes(n, 10).await;

        counting.reset();
        let all = api.list("projects/p1", "instances").await.unwrap();
        unfiltered.push((n, all.items.len(), counting.read()));

        counting.reset();
        let mine = api
            .list_filtered("projects/p1", "instances", &Filter::for_node("node-0"))
            .await
            .unwrap();
        filtered.push((n, mine.items.len(), counting.read()));
    }
    for ((n, all, cost_all), (_, mine, cost_mine)) in unfiltered.iter().zip(&filtered) {
        println!(
            "  cell of {n:>4}: whole cell {all:>4} objects / {cost_all:>5} reads   \
             one node {mine:>3} objects / {cost_mine:>5} reads"
        );
    }

    // The objects a node is handed are its own tenth, at every size.
    for ((n, _, _), (_, mine, _)) in unfiltered.iter().zip(&filtered) {
        assert_eq!(*mine, n / 10, "a node was handed more than its own share");
    }
    // A node's list still *scans* the collection — etcd cannot filter a range
    // read — but what crosses the wire, what gets its computed fields filled in
    // and what the agent then walks is its own share. The scan is the next thing
    // to fix, in the store's key layout; the fan-out is what was making a cell
    // unusable.
    assert!(
        filtered[2].2 <= unfiltered[2].2,
        "filtering cost more than not filtering: {filtered:?}"
    );
}

/// A node is not told about work that is not its own, and *is* told about work
/// it still holds after being re-assigned away from.
#[tokio::test]
async fn the_filter_keeps_the_case_that_makes_it_subtle() {
    let (counting, api) = cell_on_nodes(20, 10).await;
    // The real sequence, and the store enforces it: node-0 takes the guest
    // while it is still node-0's, and only then does a scheduler give it to
    // node-1. Writing the status afterwards is refused — "not your object" —
    // which is the access rule working.
    let store: Arc<dyn Store> = counting.clone();
    let instances: velstra_cloud_store::TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = velstra_cloud_store::TypedStore::new(store, "cell-1", "instances");
    let store_name = "projects/p1/instances/i0";
    let mut held = instances.get(store_name).await.unwrap().unwrap();
    held.status.node = Some("node-0".into());
    instances
        .update(&held, &velstra_cloud_model::access::Writer::agent("node-0"))
        .await
        .unwrap();

    api.patch(
        &velstra_cloud_model::meta::ResourceName::parse(store_name).unwrap(),
        &json!({"spec": {"node": "node-1"}}),
        None,
        &Identity::new("scaling-test"),
    )
    .await
    .unwrap();

    let for_zero = api
        .list_filtered("projects/p1", "instances", &Filter::for_node("node-0"))
        .await
        .unwrap();
    assert!(
        for_zero.items.iter().any(|i| crate_name(i) == store_name),
        "the node still running the guest stopped being told about it, so it would never let go"
    );
    let for_one = api
        .list_filtered("projects/p1", "instances", &Filter::for_node("node-1"))
        .await
        .unwrap();
    assert!(
        for_one.items.iter().any(|i| crate_name(i) == store_name),
        "the node it was given to was never told"
    );
    let for_two = api
        .list_filtered("projects/p1", "instances", &Filter::for_node("node-2"))
        .await
        .unwrap();
    assert!(
        !for_two.items.iter().any(|i| crate_name(i) == store_name),
        "a node with no business with the guest was told about it anyway"
    );
}

/// The name as the model shape carries it: a segment list, not a string.
fn crate_name(document: &serde_json::Value) -> String {
    serde_json::from_value::<velstra_cloud_model::meta::ResourceName>(
        document["meta"]["name"].clone(),
    )
    .map(|n| n.to_string())
    .unwrap_or_default()
}

/// The claim the cell's size rests on: with the cache running, what a node reads
/// costs the store **nothing**, whatever size the cell is.
///
/// Before it, every node listed the whole cell on every pass and watched it
/// unfiltered — so a thousand nodes were a thousand watchers on one etcd and
/// every write was delivered a thousand times. That, and not the store's own
/// capacity, was what bounded a cell.
#[tokio::test]
async fn a_node_reading_its_own_share_does_not_touch_the_store() {
    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, api) = cell_on_nodes(n, 10).await;
        api.serve_agents();
        // One list to settle the cache, which is the cost paid once per process
        // rather than once per node per pass.
        api.list_filtered("projects/p1", "instances", &Filter::for_node("node-0"))
            .await
            .unwrap();

        counting.reset();
        let mine = api
            .list_filtered("projects/p1", "instances", &Filter::for_node("node-0"))
            .await
            .unwrap();
        measured.push((n, mine.items.len(), counting.read()));
    }
    for (n, mine, cost) in &measured {
        println!("  cell of {n:>4}: one node gets {mine:>3} objects for {cost:>3} store reads");
    }
    for (n, mine, cost) in &measured {
        assert_eq!(*mine, n / 10, "a node was handed more than its own share");
        assert_eq!(
            *cost, 0,
            "a node's list still reads the store, so the cell is bounded by its agents"
        );
    }
}

/// However many agents watch, the store sees one watcher per collection.
#[tokio::test]
async fn a_thousand_watchers_are_one_watch_on_the_store() {
    let (counting, api) = cell_on_nodes(40, 10).await;
    api.serve_agents();
    api.list_filtered("projects/p1", "instances", &Filter::for_node("node-0"))
        .await
        .unwrap();
    let before = counting.inner_watchers();

    let mut streams = Vec::new();
    for node in 0..50 {
        streams.push(
            api.watch_filtered(
                "projects/p1",
                "instances",
                None,
                Filter::for_node(format!("node-{node}")),
            )
            .unwrap(),
        );
    }
    assert_eq!(
        counting.inner_watchers(),
        before,
        "fifty agents opened fifty watches on the store"
    );
    drop(streams);
}

/// A cell of `n` volumes spread over `pools` pools, each with one snapshot.
async fn volumes_on_pools(n: usize, pools: usize) -> (Arc<Counting>, Api) {
    let counting = Counting::new();
    let store: Arc<dyn Store> = counting.clone();
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    // An operator: this harness creates projects and pools, which are the
    // cell's. What it measures is cost, not permission.
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec!["scaling-test".into()]);
    let who = Identity::new("scaling-test");

    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who,
    )
    .await
    .unwrap();
    for p in 0..pools {
        api.create(
            "",
            "pools",
            &json!({"id": format!("pool-{p}"), "spec": {"accepting": true, "labels": []}}),
            &who,
        )
        .await
        .unwrap();
    }
    for i in 0..n {
        let pool = format!("pool-{}", i % pools);
        api.create(
            "projects/p1",
            "volumes",
            &json!({"id": format!("v{i}"), "spec": {"sizeGib": 1, "pool": pool}}),
            &who,
        )
        .await
        .unwrap();
    }
    (counting, api)
}

/// The storage half of the same claim: a pool is handed what it holds, and it
/// costs the store nothing.
#[tokio::test]
async fn one_pool_is_not_handed_every_volume_in_the_cell() {
    let mut measured = Vec::new();
    for n in [20usize, 100, 400] {
        let (counting, api) = volumes_on_pools(n, 10).await;
        api.serve_agents();
        api.list_filtered("projects/p1", "volumes", &Filter::for_pool("pool-0"))
            .await
            .unwrap();

        counting.reset();
        let mine = api
            .list_filtered("projects/p1", "volumes", &Filter::for_pool("pool-0"))
            .await
            .unwrap();
        measured.push((n, mine.items.len(), counting.read()));
    }
    for (n, mine, cost) in &measured {
        println!("  cell of {n:>4} volumes: one pool gets {mine:>3} for {cost:>3} store reads");
    }
    for (n, mine, cost) in &measured {
        assert_eq!(*mine, n / 10, "a pool was handed more than its own share");
        assert_eq!(*cost, 0, "a pool's list still reads the store");
    }
}

/// A node and a pool are told about different things, through the same API.
#[tokio::test]
async fn a_node_filter_does_not_hand_out_volumes_and_a_pool_filter_does_not_hand_out_guests() {
    let (_counting, api) = volumes_on_pools(20, 2).await;
    api.serve_agents();

    // A pool asking about instances is asking about a collection it is assigned
    // none of, so it gets the collection — which is empty here, and would be the
    // shared collections in a real cell. What must not happen is the reverse:
    // that a *volume* reaches something asking as a node.
    let as_node = api
        .list_filtered("projects/p1", "volumes", &Filter::for_node("pool-0"))
        .await
        .unwrap();
    assert_eq!(
        as_node.items.len(),
        0,
        "a caller asking as a node was handed volumes belonging to a pool of the same name"
    );
    let as_pool = api
        .list_filtered("projects/p1", "volumes", &Filter::for_pool("pool-0"))
        .await
        .unwrap();
    assert_eq!(as_pool.items.len(), 10);
}
