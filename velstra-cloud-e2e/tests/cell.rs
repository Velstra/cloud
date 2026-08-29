//! One cell, all of it, running together.
//!
//! Every crate here is tested on its own, and each of those suites is honest
//! about its own layer. None of them can catch the failure this file exists for:
//! two layers that each behave correctly against their *idea* of the other. The
//! console is tested against a fake API, the controllers against a store they
//! drive themselves, the agent against objects it was handed — so the first time
//! a real instance travels from an HTTP request, through placement, to a node
//! that boots it and reports back, is here.
//!
//! What it asserts is the platform's whole claim, in order:
//!
//! 1. a request creates an object and nothing more — no work has happened yet;
//! 2. the scheduler places it, writing only `spec`;
//! 3. the node observes, acts, and reports, writing only `status`;
//! 4. the object converges, and says so on itself;
//! 5. **a settled cell reconciles to zero writes** — the property that makes
//!    level-triggered reconciliation worth the trouble;
//! 6. deletion runs the finalizer to the end rather than dropping the object on
//!    a node that still holds it.
//!
//! Point 6 was a claim in this comment and nowhere else, and it was false: an
//! instance carried no finalizer, so a delete took the object out of the store
//! inside the request that asked for it and the node was never told to stop the
//! guest. `deleting_an_instance_stops_the_guest` is what makes it a property
//! rather than a sentence — and it asks the *hypervisor*, because the store
//! agreeing with itself was never the thing in doubt.

use std::sync::Arc;

use velstra_cloud_api::{Api, StaticTokenVerifier};
use velstra_cloud_controller::{
    instance::InstanceController, migration::MigrationController, scheduler::Scheduler,
    status::StatusWriter, sweep,
};
use velstra_cloud_model::{
    Writer,
    meta::{Condition, ConditionStatus, Meta, Placement, ResourceName, set_condition},
    migration::{MigrationSpec, MigrationStatus},
    resources::{
        Capacity, Image, ImageFormat, ImageSpec, ImageStatus, InstanceSpec, InstanceState,
        InstanceStatus, Node, NodeSpec, NodeStatus, Resource,
    },
};
use velstra_cloud_nodeagent::{Agent, AgentConfig, FakeDatapath, FakeNetwork, FakeVmm, Fault};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const REGION: &str = "eu-central";
const CELL: &str = "cell-1";
/// The image every instance in this file boots from.
const IMAGE: &str = "projects/p1/images/sha256-abc";
/// What the bytes behind it are filed under, which is a different question.
const IMAGE_DIGEST: &str = "sha256:17bfebfb2d61335a30fb1119cc9894ed15605293c627cf8428f28864a832bf5a";
const TOKEN: &str = "e2e-token";

/// Everything a cell is made of, sharing one store — which is the point: there
/// is one place state lives, and every component reads and writes it under its
/// own identity.
/// An agent on a cell whose machines share one state directory.
///
/// The whole of this file's migration story depends on it: a move transfers
/// memory and not disks, so a guest whose root disk is private to its
/// machine cannot arrive anywhere and `may_migrate` refuses outright. These
/// are the machines where it can — and the case where it cannot has its own
/// test in `evacuation`, which is where the cost of getting it wrong is a
/// maintenance window that never finishes.
fn shared(node_id: &str) -> AgentConfig {
    let mut config = AgentConfig::new(node_id, REGION, CELL);
    config.shared_state = true;
    config
}

struct Cell {
    store: Arc<dyn Store>,
    address: String,
    agent: Agent,
    vmm: Arc<FakeVmm>,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    windows: TypedStore<
        velstra_cloud_model::maintenance::MaintenanceWindowSpec,
        velstra_cloud_model::maintenance::MaintenanceWindowStatus,
    >,
    images: TypedStore<ImageSpec, ImageStatus>,
    /// The wire between the machines. A migration is the one thing a node
    /// cannot do alone, so the fake hypervisors have to be able to reach each
    /// other or the interesting half of it is untested.
    network: FakeNetwork,
}

impl Cell {
    async fn start(node_id: &str) -> Cell {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        // An operator: this cell registers its own nodes and creates its own
        // project, which are the cell's rather than any tenant's. What a
        // *tenant* may do is exercised on purpose in `api/tests/authz.rs`.
        let api = Api::new(
            store.clone(),
            REGION,
            CELL,
            Arc::new(StaticTokenVerifier::single(TOKEN)),
        )
        .with_cell_admins(vec!["dev".into()]);

        // A real socket, not a tower service called in-process: the console and
        // any customer SDK reach this over HTTP, and the parts that only break
        // over a socket — headers, status codes, streaming — are exactly what
        // this file is for.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let router = velstra_cloud_api::server(api.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let network = FakeNetwork::new();
        let vmm = Arc::new(network.host(node_id));
        let agent = Agent::new(
            store.clone(),
            shared(node_id),
            vmm.clone(),
            Arc::new(FakeDatapath::new()),
        );

        let cell = Cell {
            instances: TypedStore::new(store.clone(), CELL, "instances"),
            nodes: TypedStore::new(store.clone(), CELL, "nodes"),
            migrations: TypedStore::new(store.clone(), CELL, "migrations"),
            windows: TypedStore::new(store.clone(), CELL, "maintenance-windows"),
            images: TypedStore::new(store.clone(), CELL, "images"),
            store,
            address,
            agent,
            vmm,
            network,
        };
        // Registered here rather than per test: every instance in this file
        // boots the same image, and an unregistered one is not a thing a node
        // can obtain. See `register_image`.
        cell.register_image(IMAGE).await;
        cell
    }

    /// A second hypervisor in the same cell: same store, same network, its own
    /// machine. Nothing about it is special — it is the same `Agent` the first
    /// one is, which is the point.
    fn join(&self, node_id: &str) -> (Agent, Arc<FakeVmm>) {
        let vmm = Arc::new(self.network.host(node_id));
        let agent = Agent::new(
            self.store.clone(),
            shared(node_id),
            vmm.clone(),
            Arc::new(FakeDatapath::new()),
        );
        (agent, vmm)
    }

    /// One turn of a cell that is moving a guest: the schedulers, the migration
    /// controller, then every node in turn.
    ///
    /// The evacuation controller runs here too, keyed on nodes rather than on
    /// guests — it is what turns "this machine should be empty", however that
    /// came to be true, into one migration per guest that can move.
    async fn turn_with(&self, others: &[&Agent]) {
        sweep(
            &velstra_cloud_controller::evacuation::EvacuationController::new(
                self.instances.clone(),
                self.nodes.clone(),
                self.migrations.clone(),
                self.images.clone(),
            )
            .with_maintenance(self.windows.clone()),
            &self.nodes,
        )
        .await
        .unwrap();
        sweep(&self.scheduler(), &self.instances).await.unwrap();
        sweep(
            &InstanceController::new(self.instances.clone()),
            &self.instances,
        )
        .await
        .unwrap();
        sweep(
            &MigrationController::new(self.instances.clone(), CELL),
            &self.migrations,
        )
        .await
        .unwrap();
        self.agent.resync().await;
        for other in others {
            other.resync().await;
        }
    }

    /// Register a hypervisor the way one registers itself: the node object is
    /// created by an operator (spec), and the agent reports what it has
    /// (status). Both halves, by their rightful writers.
    /// Register the image every instance in this file boots from.
    ///
    /// Not decoration: a node fetches an image from the registered object's
    /// `source_url`, so an instance naming an image that exists only as a
    /// string in its spec can never boot. These tests used to do exactly that
    /// and pass, because the node only ever needed the digest out of the name —
    /// the source was carried on the wire, shown in the console, and read by
    /// nothing. `file://` because the fake VMM does not fetch; what is being
    /// exercised here is that the node is *told where to look*.
    async fn register_image(&self, name: &str) {
        let image: Image = Resource::new(
            Meta::new(
                ResourceName::parse(name).unwrap(),
                Placement::new(REGION, CELL),
            ),
            ImageSpec {
                from: String::new(),
                family: "debian-13".into(),
                version: "20260815".into(),
                source_instance: None,
                // A real-length digest, because the length is load-bearing now: the
                // bytes are filed on a node under `sha256-<64 hex>`, and a
                // short one is not a digest a node can verify anything against.
                digest: IMAGE_DIGEST.into(),
                format: ImageFormat::Raw,
                size_bytes: 1024,
                source_url: "file:///var/lib/velstra/images/abc.raw".into(),
                signature: None,
            },
            ImageStatus::default(),
        );
        self.images
            .create(
                &image,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
    }

    async fn add_node(&self, id: &str, vcpus: u32, memory_mib: u64) {
        let mut node: Node = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new(REGION, CELL),
            ),
            NodeSpec {
                evacuate: false,
                vcpu_overcommit: 0,
                fence_after_s: 0,
                schedulable: true,
                labels: vec![],
                cpu_baseline: None,
                gateway: false,
            },
            NodeStatus {
                // One state directory between these machines. It is what makes a
                // guest movable at all — a migration transfers memory and not
                // disks — and it is the arrangement this whole file's migration
                // tests are written against.
                shared_state: true,
                ..NodeStatus::default()
            },
        );
        self.nodes
            .create(
                &node,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();

        node = self
            .nodes
            .get(&format!("nodes/{id}"))
            .await
            .unwrap()
            .unwrap();
        node.status.capacity = Capacity {
            vcpus,
            memory_mib,
            disk_gib: 1000,
            numa_free_mib: vec![memory_mib],
            hugepages_1gi: 0,
        };
        node.status.observed_generation = node.meta.generation;
        set_condition(
            &mut node.status.conditions,
            Condition::ready(node.meta.generation),
        );
        self.nodes
            .update(&node, &Writer::agent(id))
            .await
            .expect("a node reporting its own capacity");
    }

    fn scheduler(&self) -> Scheduler {
        Scheduler::new(
            self.instances.clone(),
            self.nodes.clone(),
            StatusWriter::new(self.store.clone(), CELL, "instances", "scheduler"),
            CELL,
        )
        // Given the windows, because a machine inside an open one must stop
        // taking work — and a scheduler that placed onto it would undo the
        // evacuation in the same turn that emptied it.
        .with_maintenance(self.windows.clone())
    }

    /// One turn of the whole cell: controllers reconcile, then the node does.
    /// Deterministic on purpose — a test that waits on background loops tests
    /// the sleep it happened to pick.
    async fn turn(&self) {
        sweep(&self.scheduler(), &self.instances).await.unwrap();
        sweep(
            &InstanceController::new(self.instances.clone()),
            &self.instances,
        )
        .await
        .unwrap();
        self.agent.resync().await;
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}{path}", self.address))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let response = reqwest::Client::new()
            .get(format!("{}{path}", self.address))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    async fn delete(&self, path: &str) -> (u16, serde_json::Value) {
        let response = reqwest::Client::new()
            .delete(format!("{}{path}", self.address))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    async fn instance(&self, id: &str) -> Resource<InstanceSpec, InstanceStatus> {
        self.instances
            .get(&format!("projects/p1/instances/{id}"))
            .await
            .unwrap()
            .expect("the instance exists")
    }

    /// Turn until the instance has converged, and say how many turns it took.
    ///
    /// The number is worth asserting rather than hiding behind a sleep: an
    /// instance reaches a running guest through a fixed handshake — place,
    /// claim ownership, act, report — and if that count grows, something has
    /// started taking an extra round trip per instance, which is the kind of
    /// regression that only shows up as latency under load.
    async fn settle(&self, id: &str, most: usize) -> usize {
        for turn in 1..=most {
            self.turn().await;
            let object = self.instance(id).await;
            if object.converged() && object.status.state == InstanceState::Running {
                return turn;
            }
        }
        let object = self.instance(id).await;
        panic!(
            "{id} did not settle in {most} turns: state {:?}, generation {} vs observed {}, conditions {:?}",
            object.status.state,
            object.meta.generation,
            object.status.observed_generation,
            object.status.conditions
        );
    }

    /// Which keys a turn writes. Counting is not enough — a settled cell that
    /// writes once per pass is either broken or has one deliberate periodic
    /// write, and the difference is the key.
    async fn writes_during_a_turn(&self) -> Vec<String> {
        let from = self.store.revision().await.unwrap();
        let mut events = self.store.watch("/", Some(from));
        self.turn().await;
        let mut keys = Vec::new();
        while let Ok(event) = events.try_recv() {
            keys.push(event.key().to_string());
        }
        keys.sort();
        keys.dedup();
        keys
    }
}

async fn create_instance(cell: &Cell, id: &str) -> serde_json::Value {
    let (status, body) = cell
        .post(
            "/api/v1/projects/p1/instances",
            serde_json::json!({
                "id": id,
                "spec": {
                    "vcpus": 2,
                    "memoryMib": 2048,
                    "image": IMAGE,
                    "rootDiskGib": 20,
                    "desiredState": "Running",
                    "ports": []
                }
            }),
        )
        .await;
    assert!(
        status == 200 || status == 201 || status == 202,
        "creating an instance answered {status}: {body}"
    );
    body
}

#[tokio::test]
async fn an_instance_travels_from_a_request_to_a_running_guest() {
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;

    create_instance(&cell, "i1").await;

    // Nothing has happened yet, and that is correct: a create records what was
    // asked for. Every system that does work inside the request is a system
    // that loses that work when the request dies.
    let fresh = cell.instance("i1").await;
    assert!(
        fresh.spec.node.is_none(),
        "the API placed an instance itself"
    );
    assert_eq!(fresh.status.state, InstanceState::Unknown);
    assert!(!fresh.converged());

    // One turn: the scheduler assigns.
    cell.turn().await;

    let placed = cell.instance("i1").await;
    assert_eq!(
        placed.spec.node.as_deref(),
        Some("node-a"),
        "the scheduler did not place a placeable instance"
    );

    // Then the node takes ownership, does the work, and reports. Each of those
    // is a separate observation of the world rather than a step in a script,
    // which is why it takes more than one turn and why none of them can be
    // lost by a crash.
    let turns = cell.settle("i1", 8).await;
    assert!(
        turns <= 5,
        "an instance took {turns} turns from placement to running; the handshake grew"
    );

    let running = cell.instance("i1").await;
    assert_eq!(
        running.status.state,
        InstanceState::Running,
        "the node did not start a guest it was given: {:?}",
        running.status.conditions
    );
    assert!(
        running.converged(),
        "generation {} vs observed {}",
        running.meta.generation,
        running.status.observed_generation
    );
    let ready = velstra_cloud_model::meta::condition(&running.status.conditions, "Ready")
        .expect("a converged instance says so on itself");
    assert_eq!(ready.status, ConditionStatus::True, "{ready:?}");

    // And the guest is genuinely running on the hypervisor, not merely recorded
    // as running in the store.
    assert!(
        cell.vmm.is_running("projects/p1/instances/i1"),
        "the store says Running and the hypervisor disagrees"
    );
}

#[tokio::test]
async fn a_settled_cell_reconciles_to_nothing() {
    // The property the whole design is for. If a resync over a converged
    // cluster writes, then every controller and agent is churning the store in
    // proportion to the size of the cluster, and level-triggered reconciliation
    // has bought nothing but complexity.
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    create_instance(&cell, "i1").await;
    cell.settle("i1", 8).await;

    // Nothing is written except each node saying it is still alive. That one
    // write is deliberate and its cost is O(nodes per interval), which is a
    // different thing entirely from reconciliation that writes in proportion to
    // the objects it looked at — the cost that makes a large cluster impossible
    // to run. So the assertion is not "zero writes" but "zero *reconciliation*
    // writes", which is the property that actually matters.
    for pass in 1..=3 {
        let wrote = cell.writes_during_a_turn().await;
        let not_a_heartbeat: Vec<&String> = wrote
            .iter()
            .filter(|k| !k.starts_with("/cell-1/nodes/"))
            .collect();
        assert!(
            not_a_heartbeat.is_empty(),
            "pass {pass} of a settled cell reconciled something: {not_a_heartbeat:?}"
        );
        assert!(
            wrote.len() <= 1,
            "pass {pass} wrote more than one heartbeat: {wrote:?}"
        );
    }
}

#[tokio::test]
async fn an_instance_that_cannot_be_placed_says_why_on_itself() {
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 2048).await;

    let (_, _) = cell
        .post(
            "/api/v1/projects/p1/instances",
            serde_json::json!({
                "id": "huge",
                "spec": {
                    "vcpus": 2,
                    "memoryMib": 999999,
                    "image": IMAGE,
                    "rootDiskGib": 20,
                    "desiredState": "Running",
                    "ports": []
                }
            }),
        )
        .await;
    cell.turn().await;

    let stuck = cell.instance("huge").await;
    assert!(stuck.spec.node.is_none());
    let ready = velstra_cloud_model::meta::condition(&stuck.status.conditions, "Ready")
        .expect("an unplaceable instance must carry its reason, not leave it in a log");
    assert_ne!(ready.status, ConditionStatus::True);
    assert!(
        !ready.message.is_empty(),
        "the condition had no sentence for the person reading it"
    );

    // …and the same answer is available over the API, per node, rather than as
    // a spinner that never resolves.
    let (status, body) = cell
        .get("/api/v1/projects/p1/instances/huge:explainPlacement")
        .await;
    assert_eq!(status, 200, "{body}");
    let rejected = body
        .get("rejected")
        .and_then(|r| r.as_array())
        .expect("the explanation names candidates");
    assert!(!rejected.is_empty(), "no node was accounted for: {body}");
}

#[tokio::test]
async fn a_guest_that_dies_is_brought_back_without_anybody_asking() {
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    create_instance(&cell, "i1").await;
    cell.settle("i1", 8).await;

    // The hypervisor loses the guest. Nothing tells the platform; it notices.
    cell.vmm.crash("projects/p1/instances/i1");

    cell.settle("i1", 8).await;

    assert_eq!(
        cell.instance("i1").await.status.state,
        InstanceState::Running,
        "a crashed guest stayed down: nobody was watching the level"
    );
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));
}

#[tokio::test]
async fn the_console_the_api_serves_is_the_console_that_was_built() {
    // The console suite runs against a fake API, which is the right way to test
    // an interface — but it cannot notice if the API stops serving the page, or
    // serves a different one. This is that one check, and it is cheap.
    let cell = Cell::start("node-a").await;
    let (status, body) = cell.get("/").await;
    assert_eq!(status, 200);
    let page = body.as_str().unwrap_or_default();
    assert!(
        page.contains("<!doctype html") || page.contains("<!DOCTYPE html"),
        "the API served something that is not the console"
    );
    assert_eq!(
        page.len(),
        velstra_cloud_console::page().len(),
        "the API is serving a different build of the console than the crate holds"
    );
}

#[tokio::test]
async fn a_running_guest_moves_to_another_node_and_nobody_is_ever_in_two_places() {
    // The whole dance, through the real API, with two agents that only know
    // what the store tells them. Each layer is tested alone elsewhere; what can
    // only fail here is the handover — the moment one machine stops owning a
    // guest and another starts.
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    cell.add_node("node-b", 8, 16384).await;
    let (node_b, vmm_b) = cell.join("node-b");
    // Both machines hold the image, because a destination that does not have it
    // cannot start a receiver — and the platform refuses the migration rather
    // than finding out after the memory has been copied.
    for vmm in [&cell.vmm, &vmm_b] {
        vmm.cache_image(IMAGE_DIGEST);
    }

    create_instance(&cell, "i1").await;
    for _ in 0..6 {
        cell.turn_with(&[&node_b]).await;
    }
    let instance = cell.instance("i1").await;
    assert_eq!(instance.status.node.as_deref(), Some("node-a"));
    assert_eq!(instance.status.state, InstanceState::Running);
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));

    let (status, body) = cell
        .post(
            "/api/v1/projects/p1/migrations",
            serde_json::json!({
                "id": "m1",
                "spec": { "instance": "projects/p1/instances/i1", "toNode": "node-b" }
            }),
        )
        .await;
    assert!(
        (200..300).contains(&status),
        "asking for a migration answered {status}: {body}"
    );
    // The source is derived, exactly like an attachment's node: the platform
    // knows where the guest is and does not ask twice.
    let asked = cell
        .migrations
        .get("projects/p1/migrations/m1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(asked.spec.from_node, "node-a");

    // Nothing has moved yet. A migration is a request, and a request is not an
    // event — the guest keeps running until two machines have agreed.
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));

    // Several turns, because arriving is not one write: the destination takes
    // delivery, the source lets go, a controller moves the assignment, and only
    // then does the destination claim the instance and report it running. Each
    // of those is a separate pass on purpose — that is what makes any of them
    // survivable.
    for turn in 1..=12 {
        cell.turn_with(&[&node_b]).await;
        let i = cell.instance("i1").await;
        if i.status.node.as_deref() == Some("node-b") && i.status.state == InstanceState::Running {
            break;
        }
        assert!(turn < 12, "the guest never arrived: {:?}", i.status);
    }

    let instance = cell.instance("i1").await;
    assert_eq!(instance.status.node.as_deref(), Some("node-b"));
    assert_eq!(instance.spec.node.as_deref(), Some("node-b"));
    assert_eq!(instance.status.state, InstanceState::Running);

    // The property the whole design exists for: at no point does a guest exist
    // twice, and at the end the source is genuinely empty rather than merely
    // no longer assigned.
    assert!(
        vmm_b.is_running("projects/p1/instances/i1"),
        "the destination does not actually have the guest"
    );
    assert!(
        !cell.vmm.is_running("projects/p1/instances/i1"),
        "the source is still running a guest that moved away"
    );
    assert!(
        !vmm_b.is_receiving("projects/p1/instances/i1"),
        "a receiver was left listening, holding memory on a node that is now running the guest"
    );

    // And what the API says about it is computed from where the guest is, so it
    // cannot be the last thing a dead agent managed to write.
    let (_, document) = cell.get("/api/v1/projects/p1/migrations/m1").await;
    let moved = document["status"]["conditions"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["kind"] == "Moved"))
        .expect("a migration says what it did")
        .clone();
    assert_eq!(moved["status"], "True", "{moved}");
    assert_eq!(moved["reason"], "Arrived", "{moved}");

    // A settled cell writes nothing, and a finished migration is part of what
    // "settled" means — otherwise every resync pays for every guest ever moved.
    let writes = cell.writes_during_a_turn().await;
    let noise: Vec<_> = writes
        .iter()
        .filter(|k| !k.contains("/nodes/"))
        .cloned()
        .collect();
    assert!(noise.is_empty(), "a settled cell wrote {noise:?}");
}

#[tokio::test]
async fn a_transfer_that_fails_leaves_the_guest_running_where_it_was() {
    // Pre-copy's one great property, and the reason it is the default: until
    // the last pages are sent, the source still has a running guest. A platform
    // that lost the guest on a failed send would be worse than one that never
    // migrated at all, so this is the assertion that matters most in the file.
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    cell.add_node("node-b", 8, 16384).await;
    let (node_b, vmm_b) = cell.join("node-b");
    // Both machines hold the image, because a destination that does not have it
    // cannot start a receiver — and the platform refuses the migration rather
    // than finding out after the memory has been copied.
    for vmm in [&cell.vmm, &vmm_b] {
        vmm.cache_image(IMAGE_DIGEST);
    }

    create_instance(&cell, "i1").await;
    for _ in 0..6 {
        cell.turn_with(&[&node_b]).await;
    }
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));

    // The network goes away mid-flight — the destination is listening, and the
    // send fails anyway.
    cell.vmm.fail(
        Fault::Send,
        "projects/p1/instances/i1",
        "the destination stopped answering",
    );

    cell.post(
        "/api/v1/projects/p1/migrations",
        serde_json::json!({
            "id": "m1",
            "spec": { "instance": "projects/p1/instances/i1", "toNode": "node-b" }
        }),
    )
    .await;

    for _ in 0..6 {
        cell.turn_with(&[&node_b]).await;
    }

    let instance = cell.instance("i1").await;
    assert_eq!(
        instance.status.node.as_deref(),
        Some("node-a"),
        "a failed transfer moved the guest anyway"
    );
    assert_eq!(instance.status.state, InstanceState::Running);
    assert!(
        cell.vmm.is_running("projects/p1/instances/i1"),
        "the guest was lost by a transfer that failed"
    );
    assert!(
        !vmm_b.is_running("projects/p1/instances/i1"),
        "the destination started a guest the source still has"
    );

    // Abandoning it is safe under pre-copy, and must leave nothing listening.
    let client = reqwest::Client::new();
    client
        .delete(format!("{}/api/v1/projects/p1/migrations/m1", cell.address))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    for _ in 0..3 {
        cell.turn_with(&[&node_b]).await;
    }
    assert!(
        cell.vmm.is_running("projects/p1/instances/i1"),
        "abandoning the migration took the guest with it"
    );
    assert!(
        !vmm_b.is_receiving("projects/p1/instances/i1"),
        "the destination is still listening for a migration that was called off"
    );
}

#[tokio::test]
async fn deleting_an_instance_stops_the_guest() {
    // Point 6 of what this file claims at the top. Nothing here asserted it
    // until now, and the claim was false: an instance carries no finalizer, so
    // `delete` dropped the object out of the store in the same request that
    // stamped `deletedAt`, and the node never saw a guest it was meant to tear
    // down. The API answered 200, the console showed the machine gone, and the
    // hypervisor kept running it.
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    create_instance(&cell, "i1").await;
    cell.settle("i1", 8).await;
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));

    let (status, body) = cell.delete("/api/v1/projects/p1/instances/i1").await;
    assert!(status < 300, "deleting answered {status}: {body}");

    // Turns, not a sleep: the teardown is a reconcile like every other, so if
    // it happens at all it happens within a bounded number of passes.
    for _ in 0..6 {
        cell.turn().await;
    }
    assert!(
        !cell.vmm.is_running("projects/p1/instances/i1"),
        "the object is gone from the API and the guest is still running on the hypervisor"
    );
}

/// A machine taken out of service empties itself, and nothing of the operator's
/// is touched to make it happen.
///
/// The controller's own suite proves the decision; this proves the *cell*: a
/// window somebody declared last week opens, a guest that was running on that
/// machine ends up running on another one, and `spec.evacuate` — the field an
/// operator would otherwise have had to flip at two in the morning and remember
/// to flip back — is false at the start, false at the end, and false the whole
/// way through.
#[tokio::test]
async fn an_open_window_empties_a_machine_without_anybody_flipping_a_switch() {
    let cell = Cell::start("node-a").await;
    cell.add_node("node-a", 8, 16384).await;
    cell.add_node("node-b", 8, 16384).await;
    let (node_b, vmm_b) = cell.join("node-b");
    for vmm in [&cell.vmm, &vmm_b] {
        vmm.cache_image(IMAGE_DIGEST);
    }

    create_instance(&cell, "i1").await;
    for _ in 0..6 {
        cell.turn_with(&[&node_b]).await;
    }
    assert_eq!(
        cell.instance("i1").await.status.node.as_deref(),
        Some("node-a")
    );
    assert!(cell.vmm.is_running("projects/p1/instances/i1"));

    // Declared through the API, as an operator would — open now, and asking
    // for the guests to leave.
    let (status, body) = cell
        .post(
            "/api/v1/maintenance-windows",
            serde_json::json!({
                "id": "rack-move",
                "spec": {
                    "node": "node-a",
                    "startsAt": velstra_cloud_model::meta::Timestamp::now().0 - 60_000,
                    "minutes": 60,
                    "drain": true,
                    "note": "moving it to rack 4",
                }
            }),
        )
        .await;
    assert!(
        (200..300).contains(&status),
        "declaring a window answered {status}: {body}"
    );

    for turn in 1..=16 {
        cell.turn_with(&[&node_b]).await;
        let i = cell.instance("i1").await;
        if i.status.node.as_deref() == Some("node-b") && i.status.state == InstanceState::Running {
            break;
        }
        assert!(
            turn < 16,
            "the window opened and nothing moved: {:?}",
            i.status
        );
    }

    let moved = cell.instance("i1").await;
    assert_eq!(moved.status.node.as_deref(), Some("node-b"));
    assert!(
        vmm_b.is_running("projects/p1/instances/i1"),
        "the destination does not actually have the guest"
    );
    assert!(
        !cell.vmm.is_running("projects/p1/instances/i1"),
        "the machine being taken out of service is still running the guest"
    );

    // The point of the whole design: the operator's own two switches were never
    // written. A window that closes therefore takes nothing of theirs with it,
    // and a controller that died half way through left nothing flipped.
    let node = cell.nodes.get("nodes/node-a").await.unwrap().unwrap();
    assert!(
        !node.spec.evacuate,
        "a controller wrote the operator's own field"
    );
    assert!(
        node.spec.schedulable,
        "a controller drained the node behind their back"
    );

    // And nothing comes back onto it while the window is open: an emptied
    // machine that the scheduler then fills again is a machine that never got
    // emptied.
    create_instance(&cell, "i2").await;
    for _ in 0..6 {
        cell.turn_with(&[&node_b]).await;
    }
    let second = cell.instance("i2").await;
    assert_ne!(
        second.spec.node.as_deref(),
        Some("node-a"),
        "a guest was placed onto a machine that is out of service"
    );
}
