//! One recorded shape, two implementations of the contract.
//!
//! ## The hole this closes
//!
//! The console is tested against `fake-api.mjs`, which implements
//! `docs/rest-contract.md` in memory. That is deliberate and it is good: the
//! console can be written before the API exists, and a console that passes
//! there is one that read the contract the same way. But it means every custom
//! method now has **two** implementations — the real one and the fixture — and
//! until this file there was nothing comparing them. A verb that answered
//! `largest_startable` here and `largestStartable` there would pass both suites
//! and fail the first time a browser met the real server.
//!
//! ## What is compared, and what is not
//!
//! The **shape**: which keys exist, nested, and what kind of value each holds.
//! Not the values — the two fixtures describe different cells on purpose, and a
//! test that demanded the same numbers would be a test of the fixtures.
//!
//! Two readings are treated as "no information" rather than as a mismatch,
//! because they are:
//!
//!  * `null` — a field that is genuinely empty in one fixture and filled in the
//!    other says nothing about the contract.
//!  * `[]` — the same, for a list.
//!
//! What is left is exactly the class of bug worth catching: a key that is
//! spelled differently, is missing, or holds a number on one side and a string
//! on the other.
//!
//! ## Which side is the authority
//!
//! This one. The file is a recording of what the real API answers, and the
//! console's suite checks the fixture against the same recording — so there is
//! one artifact, generated from the implementation that ships.
//!
//! Regenerate deliberately, and read the diff:
//!
//! ```text
//! UPDATE_SHAPES=1 cargo test -p velstra-cloud-api --test contract_shapes
//! ```

use std::{fs, path::PathBuf, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, header},
};
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use velstra_cloud_api::{Api, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, Meta, Placement, ResourceName},
    resources::{Capacity, NodeSpec, NodeStatus, Resource},
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const TOKEN: &str = "development-token";

/// The requests both sides answer.
///
/// Kept here rather than in the file so that adding one is a code change with a
/// reason next to it. Every path has to exist in *both* fixtures, which is why
/// the ids are the ones `fake-api.mjs` seeds.
const REQUESTS: &[&str] = &[
    // The list envelope itself: `items` and the revision every walk depends on.
    "projects/p1/instances",
    "nodes",
    "maintenance-windows",
    // One object, whole.
    "projects/p1/instances/web-1",
    "nodes/node-a",
    // An image, listed and whole: the console reads `spec.family` off one
    // (the catalogue is derived from it), and a recording without any image
    // said nothing about that field.
    "projects/p1/images",
    "projects/p1/images/debian-13",
    // Every custom method. These are the ones with two implementations and no
    // compiler between them.
    "nodes:explainCapacity",
    "nodes:explainCpu",
    "nodes/node-a:explainMaintenance",
    "nodes/node-b:explainMaintenance",
    "projects/p1:explainQuota",
    "projects/p1/instances/web-1:explainPlacement",
    "projects/p1/instances/web-1:explainMigration",
    "projects/p1/instances/web-1:explainRecovery",
    // The records about one object, which the console's history panel reads.
    // Both halves: what was accepted, and what was refused.
    "projects/p1/operations?target=projects/p1/instances/web-1",
    "audit?target=projects/p1/instances/web-1",
];

fn shapes_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/contract/shapes.json")
}

/// What a value looks like, with the values taken out.
fn shape(value: &Value) -> Value {
    match value {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        // The first element stands for the list. A collection whose items
        // disagree about their own shape is a bug this would only half catch,
        // and the alternative — a union over every element — reports a
        // difference between two fixtures as a difference in the contract.
        Value::Array(items) => match items.first() {
            None => json!([]),
            Some(first) => json!([shape(first)]),
        },
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect::<Map<String, Value>>(),
        ),
    }
}

struct Harness {
    router: Router,
    store: Arc<dyn Store>,
}

impl Harness {
    fn new() -> Self {
        let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single(TOKEN));
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
            .with_cell_admins(vec!["dev".into()]);
        Self {
            router: velstra_cloud_api::server(api),
            store,
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let request = Request::builder()
            .method(method)
            .uri(format!("/api/v1/{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert!(
            status.is_success(),
            "{method} {path} answered {status}: {body}"
        );
        body
    }
}

/// A cell shaped like the one `fake-api.mjs` seeds: the same ids, so one list
/// of requests reaches both.
async fn seed(h: &Harness) {
    let nodes: TypedStore<NodeSpec, NodeStatus> =
        TypedStore::new(h.store.clone(), "cell-1", "nodes");
    // Deliberately split: node-a carries one flag node-b does not, so
    // `:explainCpu` answers with the advice that has something to say —
    // "these could be baselined, and here is what it costs" — rather than
    // "already uniform", which records nothing about the interesting shape.
    let v2 = velstra_cloud_model::cpu::CpuLevel::V2.flags();
    for id in ["node-a", "node-b"] {
        let mut flags = v2.clone();
        if id == "node-a" {
            flags.insert("avx2".into());
        }
        let mut node: velstra_cloud_model::resources::Node = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            NodeSpec {
                schedulable: true,
                ..Default::default()
            },
            NodeStatus {
                shared_state: false,
                vmm: "qemu".into(),
                fetching: Vec::new(),
                capacity: Capacity {
                    vcpus: 16,
                    memory_mib: 16384,
                    disk_gib: 1000,
                    numa_free_mib: vec![16384],
                    hugepages_1gi: 0,
                },
                agent_version: "0.1.0".into(),
                // Filled in rather than left at its default, because a field
                // that is empty here is a field this recording says nothing
                // about — and the console reads all three.
                images: vec!["projects/p1/images/sha256-abc".into()],
                devices: vec![velstra_cloud_model::ceph::BlockDevice {
                    path: "/dev/disk/by-id/nvme-eui.0001".into(),
                    kernel_name: "nvme0n1".into(),
                    size_gib: 1863,
                    rotational: false,
                    model: "WD Black SN850X".into(),
                    serial: "WD-0001".into(),
                    state: velstra_cloud_model::ceph::DeviceUse::Free,
                }],
                // Hardware that can be passed to a guest, so the recording
                // carries `groupWith` — what else comes along when one of these
                // is claimed. Two devices in one group, because a card that
                // drags its audio function with it is the ordinary case and the
                // one an operator has to see before deciding.
                pci_devices: vec![
                    velstra_cloud_model::pci::PciDevice {
                        address: "0000:41:00.0".into(),
                        vendor_device: "10de:2204".into(),
                        description: "NVIDIA GA102 [GeForce RTX 3090]".into(),
                        kind: velstra_cloud_model::pci::DeviceKind::Gpu,
                        iommu_group: Some(17),
                        state: velstra_cloud_model::pci::DeviceUse::Free,
                    },
                    velstra_cloud_model::pci::PciDevice {
                        address: "0000:41:00.1".into(),
                        vendor_device: "10de:1aef".into(),
                        description: "NVIDIA GA102 High Definition Audio".into(),
                        kind: velstra_cloud_model::pci::DeviceKind::Other,
                        iommu_group: Some(17),
                        state: velstra_cloud_model::pci::DeviceUse::Free,
                    },
                ],
                cpu: Some(velstra_cloud_model::cpu::NodeCpu {
                    arch: "x86_64".into(),
                    vendor: "GenuineIntel".into(),
                    model_name: "Intel(R) Xeon(R) Gold 6248R".into(),
                    flags: flags.clone(),
                    presents: "host".into(),
                    presented_flags: flags.clone(),
                    can_mask: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        velstra_cloud_model::meta::set_condition(&mut node.status.conditions, Condition::ready(1));
        nodes
            // A controller, because only an agent's *status* is its own — a
            // node object is registered by the cell.
            .create(&node, &Writer::controller("fixture"))
            .await
            .expect("the fixture registers its own nodes");
    }

    h.send(
        "POST",
        "projects",
        Some(json!({ "id": "p1", "spec": { "quota": { "instances": 20, "vcpus": 200 } } })),
    )
    .await;
    // The same image `fake-api.mjs` seeds, so both sides answer the same path.
    h.send(
        "POST",
        "projects/p1/images",
        Some(json!({ "id": "debian-13", "spec": {
            "family": "debian-13", "version": "20260815",
            "digest": format!("sha256:{}", "a".repeat(64)), "format": "Qcow2",
            "sizeBytes": 1_181_116_006_u64,
            "sourceUrl": "https://images.invalid/debian-13.qcow2",
        }})),
    )
    .await;
    h.send(
        "POST",
        "projects/p1/instances",
        Some(json!({ "id": "web-1", "spec": {
            "vcpus": 2, "memoryMib": 4096, "rootDiskGib": 20, "desiredState": "Running",
        }})),
    )
    .await;
    // A second guest, unplaced, and named to sort **before** the running one.
    //
    // It is here because of what the list recording compares: the first element
    // stands for the list, and the fixture on the console side holds several
    // guests in different states. With one running guest here and an unplaced
    // one first over there, the two lists were representative of different
    // kinds of object, and every optional status field — `runningSize` is
    // simply the first — read as a contract difference. Both sides now lead
    // with a guest that is nowhere, and the running guest's full shape is
    // recorded on its own path.
    h.send(
        "POST",
        "projects/p1/instances",
        Some(json!({ "id": "db-1", "spec": {
            "vcpus": 2, "memoryMib": 4096, "rootDiskGib": 20, "desiredState": "Stopped",
        }})),
    )
    .await;

    // Placed and running, as the scheduler and the node agent would leave it.
    // Written here because neither runs in this test, and half of these verbs
    // have nothing to say about a guest that is nowhere.
    {
        let instances: TypedStore<
            velstra_cloud_model::resources::InstanceSpec,
            velstra_cloud_model::resources::InstanceStatus,
        > = TypedStore::new(h.store.clone(), "cell-1", "instances");
        let mut guest = instances
            .get("projects/p1/instances/web-1")
            .await
            .unwrap()
            .unwrap();
        guest.spec.node = Some("node-a".into());
        // A spec change is a new generation, as it is for every writer.
        guest.meta.generation += 1;
        instances
            .update(&guest, &Writer::controller("scheduler"))
            .await
            .unwrap();
        let mut guest = instances
            .get("projects/p1/instances/web-1")
            .await
            .unwrap()
            .unwrap();
        guest.status.node = Some("node-a".into());
        guest.status.state = velstra_cloud_model::resources::InstanceState::Running;
        guest.status.observed_generation = guest.meta.generation;
        guest.status.addresses = vec!["10.20.0.11".into()];
        // What it is actually running on, and deliberately *not* what the spec
        // says: the recording has to carry `status.pendingChanges`, and that is
        // computed from the difference. A fixture whose guest ran on exactly
        // what was asked for would record the shape as absent, and a client
        // written against that recording would have no idea the field exists
        // until a real guest was resized under it.
        guest.status.running_size = Some(velstra_cloud_model::resources::RunningSize {
            vcpus: 1,
            memory_mib: 2048,
            root_disk_gib: 20,
        });
        instances
            .update(&guest, &Writer::agent("node-a"))
            .await
            .unwrap();
    }

    // A refusal, so the recording carries the shape the history panel reads —
    // `detail`, not `reason`, which is the spelling that had the panel showing
    // a blank line under every refused request.
    {
        let audit: TypedStore<
            velstra_cloud_model::audit::AuditSpec,
            velstra_cloud_model::audit::AuditStatus,
        > = TypedStore::new(h.store.clone(), "cell-1", "audit");
        let at = velstra_cloud_model::meta::Timestamp::now();
        let id = velstra_cloud_model::audit::record_id(
            velstra_cloud_model::audit::AuditKind::Refused,
            "bob",
            "write",
            "projects/p1/instances/web-1",
            at,
        );
        let record: velstra_cloud_model::resources::AuditRecord = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("audit/{id}")).unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::audit::AuditSpec {
                kind: velstra_cloud_model::audit::AuditKind::Refused,
                subject: "bob".into(),
                target: "projects/p1/instances/web-1".into(),
                verb: "write".into(),
                detail: "bob is a viewer on projects/p1".into(),
                at,
            },
            Default::default(),
        );
        audit
            .create(&record, &Writer::controller("fixture"))
            .await
            .unwrap();
    }

    // A window over node-b and none over node-a, so `:explainMaintenance` is
    // recorded in both of its shapes — an open one and an empty one.
    h.send(
        "POST",
        "maintenance-windows",
        Some(json!({ "id": "dimm-swap", "spec": {
            "node": "node-b",
            "startsAt": velstra_cloud_model::meta::Timestamp::now().0 - 60_000,
            "minutes": 60,
            "drain": false,
            "note": "swapping the failed DIMM in slot 3",
        }})),
    )
    .await;
}

/// Every answer, reduced to its shape.
async fn record(h: &Harness) -> Map<String, Value> {
    let mut out = Map::new();
    for request in REQUESTS {
        let body = h.send("GET", request, None).await;
        out.insert((*request).to_string(), shape(&body));
    }
    out
}

/// Whether a recorded shape and a fresh one say the same thing.
///
/// `null` and `[]` on either side mean "this fixture had nothing here", which
/// is a fact about the fixture and not about the contract.
fn agrees(recorded: &Value, fresh: &Value, path: &str, gaps: &mut Vec<String>) {
    // `{}` counts too: an object with nothing in it — a resource with no
    // labels — says as little about the contract as a null or an empty list.
    let empty = |v: &Value| *v == json!("null") || *v == json!([]) || *v == json!({});
    if empty(recorded) || empty(fresh) {
        return;
    }
    match (recorded, fresh) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) => agrees(value, other, &format!("{path}.{key}"), gaps),
                    None => gaps.push(format!("{path}.{key} is recorded and no longer answered")),
                }
            }
            for key in b.keys() {
                if !a.contains_key(key) {
                    gaps.push(format!("{path}.{key} is answered and not recorded"));
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            agrees(&a[0], &b[0], &format!("{path}[]"), gaps);
        }
        (a, b) if a == b => {}
        (a, b) => gaps.push(format!("{path} was {a} and is now {b}")),
    }
}

/// The recording is what the API answers, and it is checked in so the console's
/// own suite can hold its fixture to it without this crate being built.
#[tokio::test]
async fn the_recorded_shape_of_every_answer_is_what_the_api_still_gives() {
    let h = Harness::new();
    seed(&h).await;
    let fresh = record(&h).await;

    if std::env::var("UPDATE_SHAPES").is_ok() {
        let mut text = serde_json::to_string_pretty(&Value::Object(fresh)).unwrap();
        text.push('\n');
        fs::create_dir_all(shapes_path().parent().unwrap()).unwrap();
        fs::write(shapes_path(), text).unwrap();
        return;
    }

    let recorded: Map<String, Value> = fs::read_to_string(shapes_path())
        .map(|t| serde_json::from_str(&t).expect("the recording is JSON"))
        .unwrap_or_else(|_| {
            panic!(
                "there is no recording at {}. Make one with \
                 UPDATE_SHAPES=1 cargo test -p velstra-cloud-api --test contract_shapes",
                shapes_path().display()
            )
        });

    let mut gaps = Vec::new();
    for (request, shape) in &recorded {
        match fresh.get(request) {
            Some(now) => agrees(shape, now, request, &mut gaps),
            None => gaps.push(format!(
                "{request} is recorded and this test no longer asks for it"
            )),
        }
    }
    for request in fresh.keys() {
        if !recorded.contains_key(request) {
            gaps.push(format!("{request} is asked for and not recorded"));
        }
    }
    assert!(
        gaps.is_empty(),
        "the API's answers have moved away from the recording the console is held to:\n  {}\n\
         If the change is deliberate, re-record with UPDATE_SHAPES=1 and read the diff — \
         every line of it is a screen somewhere.",
        gaps.join("\n  ")
    );
}
