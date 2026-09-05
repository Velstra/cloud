//! The contract, tested as a client sees it.
//!
//! Every test here is a promise from `docs/rest-contract.md` that a console is
//! being written against right now, without talking to this crate. If one of
//! them fails, somebody else's screen is wrong.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use velstra_cloud_api::{Api, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, Meta, Placement, ResourceName},
    resources::{Capacity, NodeSpec, NodeStatus, PortSpec, PortStatus, Resource},
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const TOKEN: &str = "development-token";

struct Harness {
    router: Router,
    store: Arc<dyn Store>,
    /// The same `Api` the router serves, for the few tests that drive something
    /// no request reaches — a sweep, which takes the time to sweep against so
    /// the decision is a function of the objects and the clock rather than of
    /// waiting.
    api: Api,
}

struct Answer {
    status: StatusCode,
    body: Value,
    etag: Option<String>,
    revision_header: Option<String>,
    headers: Vec<(String, String)>,
}

impl Answer {
    fn error_code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    fn field(&self) -> &str {
        self.body["error"]["field"].as_str().unwrap_or("")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

impl Harness {
    /// The same cell, with a cap on how fast one caller may write. Off in every
    /// other test here: a limiter is about one tenant taking the write path
    /// from another, and these tests have one caller.
    fn with_write_rate(rate: velstra_cloud_model::limit::Rate) -> Self {
        let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single(TOKEN));
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
            .with_cell_admins(vec!["dev".into()])
            .with_write_rate(rate);
        Self {
            router: velstra_cloud_api::server(api.clone()),
            store,
            api,
        }
    }

    fn new() -> Self {
        let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single(TOKEN));
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        // The harness acts as a cell operator: it registers nodes and creates
        // projects, which are the cell's and not any tenant's. Authorisation is
        // exercised on purpose in `tests/authz.rs` rather than incidentally
        // here.
        let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
            .with_cell_admins(vec!["dev".into()]);
        Self {
            // The whole server, gRPC routes and all: the REST paths have to
            // keep working next to their twins rather than only in isolation.
            router: velstra_cloud_api::server(api.clone()),
            store,
            api,
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Answer {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/v1/{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let etag = header_of(&response, header::ETAG.as_str());
        let revision_header = header_of(&response, velstra_cloud_api::rest::REVISION_HEADER);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Answer {
            status,
            body,
            etag,
            revision_header,
            headers,
        }
    }

    /// The same request with **no** `Authorization` header — which is the only
    /// kind of request a browser can make when it opens a WebSocket.
    async fn send_bare(&self, path: &str) -> Answer {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/{path}"))
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Answer {
            status,
            body,
            etag: None,
            revision_header: None,
            headers: Vec::new(),
        }
    }

    async fn get(&self, path: &str) -> Answer {
        self.send("GET", path, None, &[]).await
    }

    async fn post(&self, path: &str, body: Value) -> Answer {
        self.send("POST", path, Some(body), &[]).await
    }

    async fn patch(&self, path: &str, body: Value) -> Answer {
        self.send("PATCH", path, Some(body), &[]).await
    }

    /// Create an instance and hand back its name.
    async fn instance(&self, project: &str, id: &str, spec: Value) -> String {
        let created = self
            .post(
                &format!("projects/{project}/instances"),
                json!({ "id": id, "spec": spec }),
            )
            .await;
        assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);
        created.body["target"].as_str().unwrap().to_string()
    }

    /// Somewhere to put a volume.
    ///
    /// A cell with no pools cannot hold one, and the API says so now rather
    /// than accepting a volume nothing will ever provision — so a test that
    /// makes a volume has to say where it goes. Written straight to the store
    /// because a pool is the cell's and these tests are about what a tenant
    /// does inside a project.
    async fn pool(&self, id: &str) {
        let pool = velstra_cloud_model::resources::Resource::new(
            velstra_cloud_model::meta::Meta::new(
                format!("pools/{id}").parse().unwrap(),
                velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
            ),
            Default::default(),
            Default::default(),
        );
        let _ = self
            .pools()
            .create(
                &pool,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await;
    }

    fn nodes(&self) -> TypedStore<NodeSpec, NodeStatus> {
        TypedStore::new(self.store.clone(), "cell-1", "nodes")
    }

    fn pools(
        &self,
    ) -> TypedStore<
        velstra_cloud_model::resources::PoolSpec,
        velstra_cloud_model::resources::PoolStatus,
    > {
        TypedStore::new(self.store.clone(), "cell-1", "pools")
    }

    fn backup_targets(
        &self,
    ) -> TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    > {
        TypedStore::new(self.store.clone(), "cell-1", "backup-targets")
    }
}

/// Seeding a fixture straight into the store, as a controller would.
fn writer() -> velstra_cloud_model::access::Writer {
    velstra_cloud_model::access::Writer::controller("test")
}

/// The processor every node in these fixtures has.
///
/// One machine, repeated: these tests are about migrations, quotas and the
/// REST surface, and a cell whose nodes differ would make each of them also a
/// test of the CPU rules — which live in the model and are tested there.
fn a_cpu() -> velstra_cloud_model::cpu::NodeCpu {
    let flags: std::collections::BTreeSet<String> = [
        "cx16", "lahf_lm", "popcnt", "sse3", "sse4_1", "sse4_2", "ssse3",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    velstra_cloud_model::cpu::NodeCpu {
        arch: "x86_64".into(),
        vendor: "GenuineIntel".into(),
        presents: "host".into(),
        presented_flags: flags.clone(),
        flags,
        can_mask: true,
        ..Default::default()
    }
}

fn header_of(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

// ---- the halves ----------------------------------------------------------

#[tokio::test]
async fn spec_is_writable_and_status_is_refused_by_name() {
    // Invariant 1, as a client meets it. The field matters as much as the
    // refusal: a client told only "invalid request" will guess, and the guess
    // that gets made is "retry".
    let h = Harness::new();
    let name = h
        .instance("p1", "i1", json!({ "vcpus": 2, "memory_mib": 2048 }))
        .await;

    let ok = h.patch(&name, json!({ "spec": { "vcpus": 4 } })).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body["spec"]["vcpus"], json!(4));

    let refused = h
        .patch(&name, json!({ "status": { "state": "Running" } }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.error_code(), "INVALID_ARGUMENT");
    assert_eq!(
        refused.field(),
        "status",
        "the refusal did not name the field"
    );

    // The same rule at create: a client may not describe a world it has not
    // observed.
    let refused = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i2", "spec": {}, "status": { "state": "Running" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "status");
}

#[tokio::test]
async fn generation_moves_when_the_spec_changed_and_not_otherwise() {
    // The number every agent compares against `observedGeneration`. Bumping it
    // for a write that changed nothing makes every node in the cell redo its
    // work; not bumping it for a real change makes them all miss it.
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let first = h.get(&name).await;
    assert_eq!(first.body["meta"]["generation"], json!(1));

    let changed = h.patch(&name, json!({ "spec": { "vcpus": 4 } })).await;
    assert_eq!(changed.body["meta"]["generation"], json!(2));

    let identical = h.patch(&name, json!({ "spec": { "vcpus": 4 } })).await;
    assert_eq!(
        identical.status,
        StatusCode::OK,
        "an identical change is not an error"
    );
    assert_eq!(
        identical.body["meta"]["generation"],
        json!(2),
        "a no-op moved the generation"
    );
    assert_eq!(
        identical.body["meta"]["revision"], changed.body["meta"]["revision"],
        "a no-op wrote to the store and woke every watcher in the cell"
    );
}

// ---- optimistic concurrency ----------------------------------------------

#[tokio::test]
async fn if_match_refuses_a_stale_write_and_says_what_the_revision_is_now() {
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let read = h.get(&name).await;
    let stale = read.body["meta"]["revision"].as_str().unwrap().to_string();
    assert_eq!(
        read.etag,
        Some(format!("\"{stale}\"")),
        "the ETag is the revision"
    );

    // Somebody else writes.
    h.patch(&name, json!({ "spec": { "vcpus": 8 } })).await;

    let refused = h
        .send(
            "PATCH",
            &name,
            Some(json!({ "spec": { "vcpus": 4 } })),
            &[("if-match", &stale)],
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.error_code(), "ABORTED");
    let current = h.get(&name).await.body["meta"]["revision"].clone();
    assert_eq!(
        refused.body["error"]["revision"], current,
        "the conflict did not say what to re-read from"
    );
    assert_eq!(
        h.get(&name).await.body["spec"]["vcpus"],
        json!(8),
        "the stale write landed"
    );
}

#[tokio::test]
async fn a_write_without_if_match_is_last_writer_wins_because_the_client_said_so() {
    // Omitting the header is a decision, not an oversight: a client that never
    // reads before writing has nothing to be stale about.
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    h.patch(&name, json!({ "spec": { "vcpus": 8 } })).await;
    let late = h.patch(&name, json!({ "spec": { "vcpus": 16 } })).await;
    assert_eq!(late.status, StatusCode::OK);
    assert_eq!(late.body["spec"]["vcpus"], json!(16));
}

#[tokio::test]
async fn two_writers_who_both_said_last_writer_wins_both_land() {
    // Neither sent an `If-Match`, so neither asked to be told about the other.
    // An API that answered one of them with a conflict anyway would be handing
    // back a failure for a race it was told not to care about — and the change
    // that lost would be gone.
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let (first, second) = tokio::join!(
        h.patch(&name, json!({ "spec": { "vcpus": 4 } })),
        h.patch(&name, json!({ "spec": { "memoryMib": 8192 } })),
    );
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.body);

    let settled = h.get(&name).await;
    assert_eq!(settled.body["spec"]["vcpus"], json!(4));
    assert_eq!(settled.body["spec"]["memoryMib"], json!(8192));
}

// ---- deletion ------------------------------------------------------------

#[tokio::test]
async fn a_delete_stays_visible_until_the_last_finalizer_goes() {
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;

    // A controller takes a hold on the object, the way the node agent does
    // before it has anything to release.
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut held = instances.get(&name).await.unwrap().unwrap();
    held.meta
        .add_finalizer(velstra_cloud_model::resources::NODE_RELEASE_FINALIZER);
    instances
        .update(
            &held,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let deleted = h.send("DELETE", &name, None, &[]).await;
    assert_eq!(deleted.status, StatusCode::ACCEPTED);
    assert!(
        !deleted.body["meta"]["deletedAt"].is_null(),
        "deletedAt was not stamped"
    );

    let still_there = h.get(&name).await;
    assert_eq!(
        still_there.status,
        StatusCode::OK,
        "a deleting object stopped being readable"
    );
    let listed = h.get("projects/p1/instances").await;
    assert_eq!(
        listed.body["items"].as_array().unwrap().len(),
        1,
        "it left the list too early"
    );

    // The holder lets go, and only now may the object really disappear.
    let mut released = instances.get(&name).await.unwrap().unwrap();
    released
        .meta
        .remove_finalizer(velstra_cloud_model::resources::NODE_RELEASE_FINALIZER);
    instances
        .update(
            &released,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let gone = h.send("DELETE", &name, None, &[]).await;
    assert_eq!(gone.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.get(&name).await.status,
        StatusCode::NOT_FOUND,
        "404 is the only 'gone'"
    );
}

// ---- list, then watch ----------------------------------------------------

#[tokio::test]
async fn a_list_says_where_to_watch_from_and_the_watch_loses_nothing() {
    let h = Harness::new();
    h.instance("p1", "i1", json!({ "vcpus": 2 })).await;

    let listed = h.get("projects/p1/instances").await;
    let revision = listed
        .revision_header
        .clone()
        .expect("the list did not say where it ended");
    assert_eq!(listed.body["revision"], json!(revision));
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);

    // Written *between* the list and the watch: this is the event a naive
    // implementation drops, and the one a console never recovers from without
    // a reload.
    h.instance("p1", "i2", json!({ "vcpus": 2 })).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/v1/projects/p1/instances?watch=true&fromRevision={revision}"
        ))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("the watch delivered nothing")
        .expect("the stream ended")
        .unwrap();
    let text = String::from_utf8(chunk.to_vec()).unwrap();
    let payload = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("not a server-sent event");
    let event: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(event["type"], json!("PUT"));
    assert_eq!(
        event["resource"]["meta"]["name"],
        json!("projects/p1/instances/i2")
    );
    assert!(
        event["resource"]["status"]["observedGeneration"].is_number(),
        "the event is not in the contract's spelling"
    );
}

// ---- quota ---------------------------------------------------------------

#[tokio::test]
async fn quota_is_counted_from_the_store_and_refused_at_create() {
    let h = Harness::new();
    let project = h
        .post(
            "projects",
            json!({ "id": "p1", "spec": { "quota": { "instances": 1, "vcpus": 8 } } }),
        )
        .await;
    assert_eq!(project.status, StatusCode::ACCEPTED, "{:?}", project.body);

    h.instance("p1", "i1", json!({ "vcpus": 2 })).await;

    let refused = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i2", "spec": { "vcpus": 2 } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.error_code(), "RESOURCE_EXHAUSTED");

    // Counted, not reserved: what exists is what is charged, so deleting the
    // first instance makes room again without anything having to remember to
    // give a reservation back.
    h.send("DELETE", "projects/p1/instances/i1", None, &[])
        .await;
    let allowed = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i2", "spec": { "vcpus": 2 } }),
        )
        .await;
    assert_eq!(allowed.status, StatusCode::ACCEPTED, "{:?}", allowed.body);

    // A second project, with a limit on vCPUs and none on the count: a zero is
    // a limit nobody set, not a limit of nothing — otherwise a project created
    // without a quota would refuse every create and look broken.
    h.post(
        "projects",
        json!({ "id": "p2", "spec": { "quota": { "vcpus": 8 } } }),
    )
    .await;
    h.instance("p2", "i1", json!({ "vcpus": 4 })).await;
    let too_big = h
        .post(
            "projects/p2/instances",
            json!({ "id": "i2", "spec": { "vcpus": 16 } }),
        )
        .await;
    assert_eq!(too_big.error_code(), "RESOURCE_EXHAUSTED");
    assert_eq!(
        too_big.field(),
        "spec.vcpus",
        "the refusal did not name the limit that bit"
    );
}

// ---- explain -------------------------------------------------------------

#[tokio::test]
async fn explain_placement_answers_with_the_chain_of_rejections() {
    let h = Harness::new();
    // A node the way an agent would have reported it: ready, with capacity.
    let mut node = Resource::new(
        Meta::new(
            ResourceName::parse("nodes/node-a").unwrap(),
            Placement::new("eu-central", "cell-1"),
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
            // Like the fixture's other machines: one state directory between
            // them, so what these tests are about is what they say they are
            // about — capacity, an image, a CPU — and not the disk rule.
            shared_state: true,
            vmm: "qemu".into(),
            fetching: Vec::new(),
            pci_devices: Vec::new(),
            cpu: Some(a_cpu()),
            capacity: Capacity {
                vcpus: 8,
                memory_mib: 16384,
                disk_gib: 1000,
                numa_free_mib: vec![16384],
                hugepages_1gi: 0,
            },
            ..Default::default()
        },
    );
    velstra_cloud_model::meta::set_condition(&mut node.status.conditions, Condition::ready(1));
    h.nodes()
        .create(
            &node,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let name = h
        .instance("p1", "i1", json!({ "vcpus": 2, "memory_mib": 99999 }))
        .await;
    let answer = h.get(&format!("{name}:explainPlacement")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        answer.body["placed"].is_null(),
        "an impossible instance was placed"
    );
    let rejected = answer.body["rejected"].as_array().unwrap();
    assert_eq!(
        rejected.len(),
        1,
        "an operator must learn about every candidate"
    );
    assert_eq!(rejected[0]["node"], json!("node-a"));
    assert_eq!(rejected[0]["why"], json!("InsufficientMemory"));
    assert_eq!(
        rejected[0]["detail"],
        json!("16384 free, 99999 wanted"),
        "the numbers behind the refusal are the answer"
    );
}

// ---- operations ----------------------------------------------------------

#[tokio::test]
async fn an_operation_is_computed_from_the_object_it_describes() {
    let h = Harness::new();
    let created = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i1", "spec": { "vcpus": 2 } }),
        )
        .await;
    let operation = created.body["operation"].as_str().unwrap().to_string();
    assert_eq!(created.body["target"], json!("projects/p1/instances/i1"));

    let waiting = h.get(&operation).await;
    assert_eq!(waiting.status, StatusCode::OK);
    assert_eq!(waiting.body["spec"]["targetGeneration"], json!(1));
    assert_eq!(
        waiting.body["status"]["done"],
        json!(false),
        "an operation was done before anything had reported"
    );

    // The target goes. An operation that stored its own `done` would wait for
    // an object nobody will ever report on again.
    h.send("DELETE", "projects/p1/instances/i1", None, &[])
        .await;
    let finished = h.get(&operation).await;
    assert_eq!(finished.body["status"]["done"], json!(true));
    assert!(
        finished.body["status"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no longer exists")
    );
}

#[tokio::test]
async fn an_operation_is_done_when_the_target_has_caught_up() {
    // The arithmetic, from the other side: an agent has reported the
    // generation the operation is waiting for.
    let h = Harness::new();
    let name = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let created = h.get("projects/p1/operations").await;
    let operation = created.body["items"][0]["meta"]["name"]
        .as_str()
        .unwrap()
        .to_string();

    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut reported = instances.get(&name).await.unwrap().unwrap();
    reported.status.observed_generation = reported.meta.generation;
    reported.status.state = velstra_cloud_model::resources::InstanceState::Running;
    // Written as the agent that owns the object would write it — status is its
    // half, and this test would be lying if it wrote it as a controller.
    reported.status.node = Some("node-a".into());
    h.store
        .put(
            &velstra_cloud_store::key_for("cell-1", "instances", &name),
            serde_json::to_vec(&reported).unwrap(),
            velstra_cloud_store::Expect::Revision(reported.meta.revision),
        )
        .await
        .unwrap();

    let finished = h.get(&operation).await;
    assert_eq!(finished.body["status"]["done"], json!(true));
    assert!(finished.body["status"]["error"].is_null());
}

// ---- authentication ------------------------------------------------------

#[tokio::test]
async fn a_request_without_an_accepted_token_gets_nowhere() {
    let h = Harness::new();
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/projects/p1/instances")
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/projects/p1/instances")
        .header(header::AUTHORIZATION, "Bearer guessed")
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], json!("UNAUTHENTICATED"));
}

// ---- refusals an operator can act on -------------------------------------

#[tokio::test]
async fn a_taken_name_is_refused_on_the_id_in_a_sentence() {
    // Every refusal has to land on the control that caused it. A name
    // collision has exactly one control — the id — and a refusal without
    // `field` lands in a banner instead, which is the one place an operator
    // cannot do anything about it.
    let h = Harness::new();
    h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let again = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i1", "spec": { "vcpus": 2 } }),
        )
        .await;

    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.error_code(), "ALREADY_EXISTS");
    assert_eq!(again.field(), "id", "the refusal did not name the control");

    let message = again.body["error"]["message"].as_str().unwrap();
    assert_eq!(
        message, "an instance called i1 already exists in projects/p1",
        "the message is what an operator reads, article and all"
    );
    // The store's key layout is the store's business. A message carrying it is
    // one that clients start parsing, and an operator has to decode.
    assert!(
        !message.contains("/cell-1/"),
        "the message leaked a store key: {message}"
    );
}

#[tokio::test]
async fn a_field_of_the_wrong_shape_is_named() {
    // What a form produces most often: one control holding something the type
    // cannot be. `serde` says what is wrong but not where, so the API finds
    // the key itself rather than handing back "somewhere in your body".
    let h = Harness::new();
    // Under a project that exists, so quota really runs: quota reads the spec
    // as its real type too, and whichever check touches it first is the one
    // that reports the failure. Without a project this test passes for the
    // wrong reason — which is exactly how it slipped through the first time.
    h.post(
        "projects",
        json!({ "id": "p1", "spec": { "quota": { "vcpus": 64 } } }),
    )
    .await;
    let refused = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i1", "spec": { "vcpus": "four", "memoryMib": 2048 } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.vcpus");

    // The volume path, where quota counts gibibytes and would otherwise be the
    // first thing to read the field.
    let refused = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "v1", "spec": { "sizeGib": "fifty", "pool": "nvme" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.sizeGib");

    // The same on the way through a change, where the known-good copy is what
    // is already stored rather than the type's defaults.
    let name = h.instance("p1", "i2", json!({ "vcpus": 2 })).await;
    let refused = h
        .patch(&name, json!({ "spec": { "rootDiskGib": [] } }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.rootDiskGib");
}

#[tokio::test]
async fn nothing_an_operator_is_shown_carries_a_store_key() {
    // A store key is `/{cell}/{kind}/{name}` and only the name was ever asked
    // for. This is the sweep the console asked for after finding one.
    let h = Harness::new();
    let mut messages = Vec::new();
    for answer in [
        h.get("projects/p1/instances/missing").await,
        h.patch(
            "projects/p1/instances/missing",
            json!({ "spec": { "vcpus": 2 } }),
        )
        .await,
        h.send("DELETE", "projects/p1/instances/missing", None, &[])
            .await,
        // A create that is refused for the *spec*, since a create with no id is
        // no longer refused at all — the API mints one.
        h.post(
            "projects/p1/instances",
            json!({ "id": "i9", "spec": { "nope": 1 } }),
        )
        .await,
        h.get("projects/p1/machines").await,
    ] {
        assert!(answer.status.is_client_error(), "{:?}", answer.body);
        messages.push(
            answer.body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        );
    }
    for message in messages {
        assert!(
            !message.starts_with('/'),
            "a store key reached an operator: {message}"
        );
        assert!(
            !message.contains("/cell-1/"),
            "a store key reached an operator: {message}"
        );
    }
}

#[tokio::test]
async fn a_node_reference_that_would_never_match_an_agent_is_refused() {
    // The two spellings in this system are both correct, for different things,
    // and getting them the wrong way round fails silently on both sides: the
    // object is assigned to a node that does not answer to that name, so the
    // agent simply never becomes its owner and nothing ever starts.
    let h = Harness::new();
    let refused = h
        .post(
            "projects/p1/instances",
            json!({ "id": "i1", "spec": { "node": "nodes/node-a" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.node");

    // …and the same on a change, where an edit could otherwise re-spell a
    // field that was right when it was created.
    let name = h.instance("p1", "i2", json!({ "node": "node-a" })).await;
    let refused = h
        .patch(&name, json!({ "spec": { "node": "nodes/node-a" } }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.node");

    // The other direction: a reference something has to follow needs the whole
    // name, because a bare id under an unstated parent finds nothing.
    let refused = h
        .post(
            "projects/p1/attachments",
            json!({ "id": "a1", "spec": { "node": "node-a", "volume": "vol-a", "instance": "projects/p1/instances/i2" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.volume");
}

// ---- attachments ---------------------------------------------------------

#[tokio::test]
async fn an_attachment_takes_its_node_from_the_instance() {
    // The model says the node is "copied from the instance so the agent's
    // watch filter is a single field". Copying it here is what makes that
    // sentence true — and it means an attachment whose node disagrees with its
    // instance's cannot be written down at all.
    let h = Harness::new();
    h.instance("p1", "i1", json!({ "vcpus": 2, "node": "node-a" }))
        .await;
    let created = h
        .post(
            "projects/p1/attachments",
            json!({ "id": "a1", "spec": {
                "volume": "projects/p1/volumes/v1",
                "instance": "projects/p1/instances/i1"
            }}),
        )
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);

    let attachment = h.get("projects/p1/attachments/a1").await;
    assert_eq!(
        attachment.body["spec"]["node"],
        json!("node-a"),
        "the caller was made to repeat something the platform already knew"
    );
}

#[tokio::test]
async fn an_attachment_may_not_name_a_node_the_instance_is_not_on() {
    // Refused rather than corrected: silently rewriting the field would change
    // what the object says without the caller asking, and they may have meant
    // the instance rather than the node.
    let h = Harness::new();
    h.instance("p1", "i1", json!({ "vcpus": 2, "node": "node-a" }))
        .await;
    let refused = h
        .post(
            "projects/p1/attachments",
            json!({ "id": "a1", "spec": {
                "volume": "projects/p1/volumes/v1",
                "instance": "projects/p1/instances/i1",
                "node": "node-b"
            }}),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.node");
    let message = refused.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("node-a") && message.contains("node-b"),
        "{message}"
    );
}

#[tokio::test]
async fn an_unplaced_instance_has_no_node_to_lend() {
    // The honest answer, rather than an attachment carrying an empty node that
    // no agent's watch will ever match.
    let h = Harness::new();
    h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let refused = h
        .post(
            "projects/p1/attachments",
            json!({ "id": "a1", "spec": {
                "volume": "projects/p1/volumes/v1",
                "instance": "projects/p1/instances/i1"
            }}),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.error_code(), "FAILED_PRECONDITION");
    assert_eq!(refused.field(), "spec.node");
}

#[tokio::test]
async fn an_attachment_may_follow_a_migration_but_not_wander_off() {
    let h = Harness::new();
    let instance = h
        .instance("p1", "i1", json!({ "vcpus": 2, "node": "node-a" }))
        .await;
    h.post(
        "projects/p1/attachments",
        json!({ "id": "a1", "spec": {
            "volume": "projects/p1/volumes/v1",
            "instance": "projects/p1/instances/i1"
        }}),
    )
    .await;

    // Away from the instance: refused, for the life of the object and not only
    // at its birth.
    let refused = h
        .patch(
            "projects/p1/attachments/a1",
            json!({ "spec": { "node": "node-b" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.node");

    // The guest moves — a deliberate act on the instance — and only then may
    // the attachment follow it.
    h.patch(&instance, json!({ "spec": { "node": "node-b" } }))
        .await;
    let followed = h
        .patch(
            "projects/p1/attachments/a1",
            json!({ "spec": { "node": "node-b" } }),
        )
        .await;
    assert_eq!(followed.status, StatusCode::OK, "{:?}", followed.body);
    assert_eq!(followed.body["spec"]["node"], json!("node-b"));
}

// ---- the console ---------------------------------------------------------

#[tokio::test]
async fn the_console_is_served_without_a_token_and_survives_a_deep_link() {
    // The page is markup with no data in it, and it carries the sign-in form.
    // Requiring a token to fetch the form that asks for one would be a locked
    // door with the key inside.
    let h = Harness::new();
    for path in ["/", "/instances", "/projects/p1/instances/i1"] {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let response = h.router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{path} did not serve the console"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
            .await
            .unwrap();
        let page = String::from_utf8_lossy(&bytes);
        assert!(
            page.starts_with("<!doctype html"),
            "{path} answered with something else"
        );
    }

    // …and the API behind it is still shut.
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/projects")
        .body(Body::empty())
        .unwrap();
    let response = h.router.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "serving the page opened the API with it"
    );
}

// ---- shapes --------------------------------------------------------------

#[tokio::test]
async fn a_body_is_always_all_three_parts_in_the_contracts_spelling() {
    let h = Harness::new();
    let name = h
        .instance("p1", "i1", json!({ "vcpus": 2, "memory_mib": 2048 }))
        .await;
    let body = h.get(&name).await.body;
    for part in ["meta", "spec", "status"] {
        assert!(body[part].is_object(), "a body arrived without {part}");
    }
    assert_eq!(body["meta"]["name"], json!("projects/p1/instances/i1"));
    assert_eq!(body["meta"]["placement"]["cell"], json!("cell-1"));
    assert!(
        body["meta"]["revision"].is_string(),
        "revision must be opaque"
    );
    assert!(body["meta"]["createdAt"].is_number());
    assert_eq!(body["meta"]["deletedAt"], Value::Null);
    assert_eq!(body["status"]["observedGeneration"], json!(0));
    // camelCase on the way in as well, or a console's own round trip breaks.
    let patched = h
        .patch(&name, json!({ "spec": { "memoryMib": 8192 } }))
        .await;
    assert_eq!(patched.body["spec"]["memoryMib"], json!(8192));
}

#[tokio::test]
async fn every_collection_the_contract_lists_is_served() {
    // The console is written from the same list. A collection that is named
    // there and missing here is a page that renders an error.
    let h = Harness::new();
    for kind in velstra_cloud_api::core::COLLECTIONS {
        // `projects` and `nodes` are at the root; everything else hangs under a
        // project, which is what makes a project the thing quota is counted on.
        let path = match kind {
            "projects" | "nodes" => kind.to_string(),
            _ => format!("projects/p1/{kind}"),
        };
        let answer = h.get(&path).await;
        assert_eq!(answer.status, StatusCode::OK, "{kind} is not served");
        assert!(
            answer.body["items"].is_array(),
            "{kind} did not answer with a list"
        );
        assert!(
            answer.revision_header.is_some(),
            "{kind} did not say where to watch from"
        );
    }
}

#[tokio::test]
async fn a_collection_nobody_serves_is_a_404_rather_than_an_empty_list() {
    // An interface that answers a typo with `[]` sends somebody looking for
    // objects that were never missing.
    let h = Harness::new();
    let answer = h.get("projects/p1/machines").await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.error_code(), "NOT_FOUND");
}

// ---- migration -----------------------------------------------------------

/// A cell with two nodes an operator could move a guest between: one holding
/// the image and one that does not, so the picker has something to say about
/// each.
async fn two_nodes(h: &Harness) {
    for (id, memory, cached) in [("node-a", 16384u64, true), ("node-b", 16384, true)] {
        let mut node = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new("eu-central", "cell-1"),
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
                vmm: "qemu".into(),
                // The fixture's two machines share one state directory, which is
                // what makes a guest movable between them at all. Every
                // migration test in this file is about a refusal downstream of
                // that; the disk rule itself is tested in the model, where the
                // node objects are built by hand.
                shared_state: true,
                fetching: Vec::new(),
                pci_devices: Vec::new(),
                cpu: Some(a_cpu()),
                capacity: Capacity {
                    vcpus: 16,
                    memory_mib: memory,
                    disk_gib: 1000,
                    numa_free_mib: vec![memory],
                    hugepages_1gi: 0,
                },
                agent_version: "0.1.0".into(),
                // Which nodes hold an image is worked out from these reports, so
                // a node that has it says so itself.
                images: if cached {
                    vec![
                        "sha256-9df3b1ed942629573eb17b71a0b34f560a183be8811bf770586815c6138da5f5"
                            .into(),
                    ]
                } else {
                    vec![]
                },
                ..Default::default()
            },
        );
        velstra_cloud_model::meta::set_condition(&mut node.status.conditions, Condition::ready(1));
        h.nodes()
            .create(
                &node,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
    }
    // A small node nothing will fit on, so a refusal has numbers behind it.
    let mut small = Resource::new(
        Meta::new(
            ResourceName::parse("nodes/node-tiny").unwrap(),
            Placement::new("eu-central", "cell-1"),
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
            // Like the fixture's other machines: one state directory between
            // them, so what these tests are about is what they say they are
            // about — capacity, an image, a CPU — and not the disk rule.
            shared_state: true,
            vmm: "qemu".into(),
            fetching: Vec::new(),
            pci_devices: Vec::new(),
            cpu: Some(a_cpu()),
            capacity: Capacity {
                vcpus: 2,
                memory_mib: 1024,
                disk_gib: 100,
                numa_free_mib: vec![1024],
                hugepages_1gi: 0,
            },
            agent_version: "0.1.0".into(),
            ..Default::default()
        },
    );
    velstra_cloud_model::meta::set_condition(&mut small.status.conditions, Condition::ready(1));
    h.nodes()
        .create(
            &small,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

/// An instance running on `node-a`, with its image cached on both real nodes.
async fn running_guest(h: &Harness) -> String {
    let image: TypedStore<
        velstra_cloud_model::resources::ImageSpec,
        velstra_cloud_model::resources::ImageStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "images");
    image
        .create(
            &Resource::new(
                Meta::new(
                    ResourceName::parse("projects/p1/images/sha256-abc").unwrap(),
                    Placement::new("eu-central", "cell-1"),
                ),
                velstra_cloud_model::resources::ImageSpec {
                    from: String::new(),
                    family: "debian-13".into(),
                    version: "20260815".into(),
                    source_instance: None,
                    digest:
                        "sha256:9df3b1ed942629573eb17b71a0b34f560a183be8811bf770586815c6138da5f5"
                            .into(),
                    ..Default::default()
                },
                velstra_cloud_model::resources::ImageStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let name = h
        .instance(
            "p1",
            "i1",
            json!({
                "vcpus": 2, "memoryMib": 4096, "node": "node-a",
                "image": "projects/p1/images/sha256-abc"
            }),
        )
        .await;

    // The guest is running, which only an agent may say — so it is said the way
    // an agent says it.
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut running = instances.get(&name).await.unwrap().unwrap();
    running.status.node = Some("node-a".into());
    running.status.state = velstra_cloud_model::resources::InstanceState::Running;
    running.status.observed_generation = running.meta.generation;
    h.store
        .put(
            &velstra_cloud_store::key_for("cell-1", "instances", &name),
            serde_json::to_vec(&running).unwrap(),
            velstra_cloud_store::Expect::Revision(running.meta.revision),
        )
        .await
        .unwrap();
    name
}

#[tokio::test]
async fn explain_migration_gives_every_node_a_verdict() {
    // Placement and migration ask different questions. A scheduler picks, so
    // `:explainPlacement` answers with the one it picked; a person picks, so
    // this has to say something about each candidate — a destination missing
    // from the answer would be one a console cannot decide about.
    let h = Harness::new();
    two_nodes(&h).await;
    let name = running_guest(&h).await;

    // Asking is free, and it has to stay free: a console asks this every time
    // somebody opens the picker, and an answer that wrote something would make
    // hovering over a menu a change to the cluster.
    let before = h.store.revision().await.unwrap();
    let answer = h.get(&format!("{name}:explainMigration")).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
    assert_eq!(answer.body["from"], json!("node-a"));
    assert_eq!(
        h.store.revision().await.unwrap(),
        before,
        "explaining a migration wrote to the store"
    );
    let migrations = h.get("projects/p1/migrations").await;
    assert!(
        migrations.body["items"].as_array().unwrap().is_empty(),
        "asking where a guest could go created a migration"
    );

    let destinations = answer.body["destinations"].as_array().unwrap();
    assert_eq!(
        destinations.len(),
        3,
        "a node was left out, so the picker cannot decide about it"
    );

    let by_node = |id: &str| -> Value {
        destinations
            .iter()
            .find(|d| d["node"] == json!(id))
            .cloned()
            .expect("node missing from the answer")
    };
    assert_eq!(by_node("node-b")["allowed"], json!(true));
    // The node it is already on is refused, with the reason rather than by
    // being absent.
    assert_eq!(by_node("node-a")["allowed"], json!(false));
    assert_eq!(by_node("node-a")["why"], json!("AlreadyThere"));
    let tiny = by_node("node-tiny");
    assert_eq!(tiny["allowed"], json!(false));
    assert_eq!(tiny["why"], json!("DestinationTooSmall"));
    assert!(
        tiny["detail"].as_str().unwrap().contains("4096 MiB"),
        "the numbers behind the refusal are the answer: {tiny}"
    );
}

#[tokio::test]
async fn a_migration_takes_its_source_from_the_instance() {
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;

    let created = h
        .post(
            "projects/p1/migrations",
            json!({ "id": "m1", "spec": {
                "instance": "projects/p1/instances/i1",
                "toNode": "node-b"
            }}),
        )
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);

    let migration = h.get("projects/p1/migrations/m1").await;
    assert_eq!(migration.body["spec"]["fromNode"], json!("node-a"));
    assert_eq!(migration.body["spec"]["toNode"], json!("node-b"));
    // The model's defaults, not zeroes: a guest that may never pause and a
    // transfer that may never end are not what "unset" means.
    assert_eq!(migration.body["spec"]["mode"], json!("Live"));
    assert_eq!(migration.body["spec"]["downtimeMs"], json!(300));
    assert_eq!(migration.body["spec"]["timeoutS"], json!(3600));

    // Nothing about the instance has changed yet — step 1 of the dance.
    let instance = h.get("projects/p1/instances/i1").await;
    assert_eq!(instance.body["spec"]["node"], json!("node-a"));
}

#[tokio::test]
async fn a_migration_that_cannot_work_is_refused_before_it_costs_anything() {
    // Every one of these is knowable in advance, and the alternative is
    // finding out after the memory has been copied.
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;

    let too_small = h
        .post(
            "projects/p1/migrations",
            json!({ "id": "m1", "spec": {
                "instance": "projects/p1/instances/i1",
                "toNode": "node-tiny"
            }}),
        )
        .await;
    assert_eq!(too_small.status, StatusCode::BAD_REQUEST);
    assert_eq!(too_small.error_code(), "FAILED_PRECONDITION");
    assert_eq!(
        too_small.field(),
        "spec.toNode",
        "the refusal did not name the control"
    );
    assert!(
        too_small.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1024 MiB"),
        "{:?}",
        too_small.body
    );

    // And a source that disagrees with where the guest actually is.
    let wrong_source = h
        .post(
            "projects/p1/migrations",
            json!({ "id": "m2", "spec": {
                "instance": "projects/p1/instances/i1",
                "fromNode": "node-b",
                "toNode": "node-b"
            }}),
        )
        .await;
    assert_eq!(wrong_source.status, StatusCode::BAD_REQUEST);
    assert_eq!(wrong_source.field(), "spec.fromNode");

    // And neither refusal left anything behind. An object created for a
    // migration that was refused is one an operator has to notice and delete,
    // and a list of them is indistinguishable from a list of real ones.
    let listed = h.get("projects/p1/migrations").await;
    assert!(
        listed.body["items"].as_array().unwrap().is_empty(),
        "a refused migration was created anyway: {:?}",
        listed.body
    );
}

/// The `Moved` condition as a client sees it, or nothing.
fn moved(document: &Value) -> Option<Value> {
    document["status"]["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["kind"] == json!("Moved"))
        .cloned()
}

fn migrations(
    h: &Harness,
) -> TypedStore<
    velstra_cloud_model::migration::MigrationSpec,
    velstra_cloud_model::migration::MigrationStatus,
> {
    TypedStore::new(h.store.clone(), "cell-1", "migrations")
}

/// A migration of `i1` to node-b, created the way a client creates one.
async fn migrate(h: &Harness) {
    let created = h
        .post(
            "projects/p1/migrations",
            json!({ "id": "m1", "spec": {
                "instance": "projects/p1/instances/i1",
                "toNode": "node-b"
            }}),
        )
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);
}

#[tokio::test]
async fn what_a_migration_is_doing_is_computed_when_it_is_read() {
    // `Moved` is a judgement over the whole dance, not a fact anybody owns —
    // the same shape as an operation's `done`. Stored, it would be a second
    // copy that can go stale; computed, it cannot disagree with the world.
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;
    migrate(&h).await;

    let read = h.get("projects/p1/migrations/m1").await;
    let condition = moved(&read.body).expect("a migration says what it is doing");
    assert_eq!(condition["reason"], json!("PreparingReceiver"));
    assert_eq!(condition["status"], json!("Unknown"));
    assert!(
        condition["message"]
            .as_str()
            .unwrap()
            .contains("node-b is not listening"),
        "{condition}"
    );

    // A list says the same thing as a read. A console that learns about an
    // object from a list and then polls it must not see two different answers.
    let listed = h.get("projects/p1/migrations").await;
    let from_list = moved(&listed.body["items"][0]).expect("a listed migration says it too");
    assert_eq!(from_list["reason"], json!("PreparingReceiver"));

    // And none of it was written down: what is *stored* in `status.conditions`
    // is only what the destination can say about itself.
    let stored = migrations(&h)
        .get("projects/p1/migrations/m1")
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.status.conditions.is_empty(),
        "the condition was stored, and a stored one can go stale: {:?}",
        stored.status.conditions
    );
}

#[tokio::test]
async fn a_migration_that_ran_out_of_time_reports_it_with_nothing_running() {
    // The case a stored condition handles worst. Nobody is left to write
    // anything — the destination may be dead, which is often *why* it timed
    // out — and this is exactly the moment an operator needs an answer. Because
    // it is computed, the answer arrives from the process being asked.
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;
    migrate(&h).await;

    // Created an hour ago with a one-minute budget: a transfer that was never
    // going to converge. Backdating is a metadata write, which is a
    // controller's to make.
    let store = migrations(&h);
    let mut m = store
        .get("projects/p1/migrations/m1")
        .await
        .unwrap()
        .unwrap();
    m.meta.created_at = velstra_cloud_model::meta::Timestamp(
        velstra_cloud_model::meta::Timestamp::now().0 - 3_600_000,
    );
    m.spec.timeout_s = 60;
    m.meta.generation += 1;
    store
        .update(&m, &velstra_cloud_model::Writer::controller("test"))
        .await
        .unwrap();

    let read = h.get("projects/p1/migrations/m1").await;
    let condition = moved(&read.body).expect("it reports the outcome");
    assert_eq!(condition["status"], json!("False"));
    assert_eq!(condition["reason"], json!("Timeout"));
    assert!(
        condition["message"].as_str().unwrap().contains("node-a"),
        "the sentence did not say where the guest is: {condition}"
    );

    // The guest is exactly where it was. Under pre-copy the source still has
    // it, so a timeout is something to report and never something to repair by
    // moving anything.
    let instance = h.get("projects/p1/instances/i1").await;
    assert_eq!(instance.body["spec"]["node"], json!("node-a"));
    assert_eq!(instance.body["status"]["state"], json!("Running"));

    let stored = store
        .get("projects/p1/migrations/m1")
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.status.conditions.is_empty(),
        "reading a migration wrote to it"
    );

    // A computed condition is built fresh on every read, so its timestamp is
    // normally the moment of the read — useless for "how long has it been like
    // this". A timeout is the exception: it happened at exactly
    // `createdAt + timeoutS`, and an interface can say "gave up forty minutes
    // ago" rather than "just now".
    let gave_up_at = read.body["meta"]["createdAt"].as_u64().unwrap() + 60_000;
    assert_eq!(
        condition["lastTransition"].as_u64().unwrap(),
        gave_up_at,
        "the timeout was stamped when it was read, not when it happened"
    );
    let again = h.get("projects/p1/migrations/m1").await;
    assert_eq!(
        moved(&again.body).unwrap()["lastTransition"],
        condition["lastTransition"],
        "two reads disagreed about when it gave up"
    );
}

#[tokio::test]
async fn a_destination_without_the_image_is_refused_with_the_sentence() {
    // The headline case for asking first. Neither VMM ships the guest's disk,
    // so a destination without the image cannot even start its receiver — and
    // finding that out from the far end happens after the memory has been
    // copied, which is the most expensive moment to fail. The operator is told
    // now, on the control they would change.
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;

    // A node that could hold the guest in every respect but one.
    let mut bare = Resource::new(
        Meta::new(
            ResourceName::parse("nodes/node-c").unwrap(),
            Placement::new("eu-central", "cell-1"),
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
            // Like the fixture's other machines: one state directory between
            // them, so what these tests are about is what they say they are
            // about — capacity, an image, a CPU — and not the disk rule.
            shared_state: true,
            vmm: "qemu".into(),
            fetching: Vec::new(),
            pci_devices: Vec::new(),
            cpu: Some(a_cpu()),
            capacity: Capacity {
                vcpus: 16,
                memory_mib: 16384,
                disk_gib: 1000,
                numa_free_mib: vec![16384],
                hugepages_1gi: 0,
            },
            agent_version: "0.1.0".into(),
            ..Default::default()
        },
    );
    velstra_cloud_model::meta::set_condition(&mut bare.status.conditions, Condition::ready(1));
    h.nodes()
        .create(
            &bare,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let refused = h
        .post(
            "projects/p1/migrations",
            json!({ "id": "m1", "spec": {
                "instance": "projects/p1/instances/i1",
                "toNode": "node-c"
            }}),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.error_code(), "FAILED_PRECONDITION");
    assert_eq!(refused.field(), "spec.toNode");
    let message = refused.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("node-c") && message.contains("projects/p1/images/sha256-abc"),
        "the refusal did not say which node was missing which image: {message}"
    );

    // Nothing was written. This is the whole claim of checking first.
    assert_eq!(
        h.get("projects/p1/migrations/m1").await.status,
        StatusCode::NOT_FOUND,
        "a migration that was refused exists anyway"
    );
}

#[tokio::test]
async fn which_nodes_hold_an_image_is_added_up_from_what_each_node_reports() {
    // The field exists on the image, but nothing writes it there — and that is
    // the point. A list of nodes is an aggregate, and an aggregate is not a fact
    // anybody owns: storing it would need every node in the cell writing into
    // one field. Each node says what it holds; this is the sum, computed on
    // every read so a node that has gone away leaves the answer by itself.
    let h = Harness::new();
    two_nodes(&h).await;
    running_guest(&h).await;

    let answer = h.get("projects/p1/images/sha256-abc").await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let image = &answer.body;
    let held: Vec<&str> = image["status"]["cachedOn"]
        .as_array()
        .expect("an image says where it is held")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(held.contains(&"node-a"), "{image}");
    assert!(held.contains(&"node-b"), "{image}");
    // The small node never reported holding it, so it is not in the sum — and
    // no writer had to remember to take it out.
    assert!(!held.contains(&"node-tiny"), "{image}");
}

// ---- snapshots -----------------------------------------------------------

type Volumes = TypedStore<
    velstra_cloud_model::resources::VolumeSpec,
    velstra_cloud_model::resources::VolumeStatus,
>;
type Snapshots = TypedStore<
    velstra_cloud_model::resources::SnapshotSpec,
    velstra_cloud_model::resources::SnapshotStatus,
>;

impl Harness {
    fn volumes(&self) -> Volumes {
        TypedStore::new(self.store.clone(), "cell-1", "volumes")
    }

    fn snapshots(&self) -> Snapshots {
        TypedStore::new(self.store.clone(), "cell-1", "snapshots")
    }

    /// A volume its pool has claimed and made, which is the only kind that can
    /// be copied.
    async fn volume(&self, id: &str, gib: u64) -> String {
        let created = self
            .post(
                "projects/p1/volumes",
                json!({ "id": id, "spec": { "sizeGib": gib, "pool": "pool-a" } }),
            )
            .await;
        assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.body);
        let name = created.body["target"].as_str().unwrap().to_string();

        let volumes = self.volumes();
        let mut v = volumes.get(&name).await.unwrap().unwrap();
        v.status.pool = Some("pool-a".into());
        v.status.provisioned = true;
        v.status.actual_size_gib = gib;
        v.status.observed_generation = v.meta.generation;
        volumes
            .update(&v, &velstra_cloud_model::access::Writer::agent("pool-a"))
            .await
            .expect("the pool claiming a volume assigned to it");
        name
    }

    /// The pool reporting that it has made the copy.
    async fn taken(&self, name: &str, gib: u64) {
        let snapshots = self.snapshots();
        let mut s = snapshots.get(name).await.unwrap().unwrap();
        s.status.pool = Some("pool-a".into());
        s.status.taken = true;
        s.status.size_gib = gib;
        s.status.observed_generation = s.meta.generation;
        snapshots
            .update(&s, &velstra_cloud_model::access::Writer::agent("pool-a"))
            .await
            .expect("the pool claiming a snapshot assigned to it");
    }
}

#[tokio::test]
async fn a_snapshot_is_created_under_the_volume_it_copies() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;

    let created = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.body);
    let name = created.body["target"].as_str().unwrap();
    assert_eq!(name, "projects/p1/volumes/data-1/snapshots/nightly");

    // The pool is derived from the volume: a copy is made where the bytes
    // already are, and nobody is asked for a fact the platform holds.
    let snapshot = h.get(name).await;
    assert_eq!(snapshot.body["spec"]["pool"], json!("pool-a"));
    assert_eq!(snapshot.body["status"]["taken"], json!(false));

    // It is in the volume's subtree and in the project's, because a name is a
    // path and both of those are prefixes of it.
    let under_volume = h.get(&format!("{volume}/snapshots")).await;
    assert_eq!(under_volume.body["items"].as_array().unwrap().len(), 1);
    let under_project = h.get("projects/p1/snapshots").await;
    assert_eq!(under_project.body["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_snapshot_that_is_not_under_a_volume_is_refused() {
    // The source lives in the name, so a name that does not carry one is a
    // copy of nothing — and it would sit there being reconciled forever.
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    h.volume("data-1", 100).await;
    let refused = h
        .post("projects/p1/snapshots", json!({ "id": "nightly" }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "meta.name");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("volumes/data-1/snapshots"),
        "the refusal did not show the shape that works: {}",
        refused.body
    );
}

#[tokio::test]
async fn a_copy_is_refused_of_a_volume_that_has_nothing_in_it_yet() {
    // Knowable before anything is written. Otherwise the object exists, the
    // pool fails on it, and an operator reads a backend error instead of a
    // sentence.
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let created = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-1", "spec": { "sizeGib": 100, "pool": "pool-a" } }),
        )
        .await;
    let volume = created.body["target"].as_str().unwrap().to_string();

    let refused = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    assert_eq!(refused.error_code(), "FAILED_PRECONDITION");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nothing to copy"),
        "{}",
        refused.body
    );
    assert_eq!(
        h.get(&format!("{volume}/snapshots")).await.body["items"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "a refused copy was created anyway"
    );
}

#[tokio::test]
async fn a_copy_is_refused_of_a_volume_on_its_way_out() {
    // It would put a guard on an object nobody would ever release it from.
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;

    // Guarded by its pool, as every provisioned volume is, so that the delete
    // is the two-phase one an operator actually sees.
    let volumes = h.volumes();
    let mut held = volumes.get(&volume).await.unwrap().unwrap();
    held.meta
        .add_finalizer(velstra_cloud_model::resources::POOL_RELEASE_FINALIZER);
    volumes
        .update(
            &held,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    h.send("DELETE", &volume, None, &[]).await;

    let refused = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    assert_eq!(refused.error_code(), "FAILED_PRECONDITION");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("being deleted"),
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn a_snapshot_may_not_name_a_pool_the_volume_is_not_in() {
    // Stated and wrong is refused rather than corrected, exactly as an
    // attachment's node is: rewriting what somebody typed changes what the
    // object says without them asking.
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let refused = h
        .post(
            &format!("{volume}/snapshots"),
            json!({ "id": "nightly", "spec": { "pool": "pool-b" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.pool");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pool-a"),
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn a_volume_made_from_a_snapshot_takes_its_size_and_pool_from_it() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let created = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    let snapshot = created.body["target"].as_str().unwrap().to_string();
    h.taken(&snapshot, 100).await;

    let restored = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-2", "spec": { "sourceSnapshot": snapshot } }),
        )
        .await;
    assert_eq!(restored.status, StatusCode::ACCEPTED, "{}", restored.body);
    let body = h.get(restored.body["target"].as_str().unwrap()).await.body;
    assert_eq!(body["spec"]["sizeGib"], json!(100));
    assert_eq!(body["spec"]["pool"], json!("pool-a"));
    assert_eq!(body["spec"]["sourceSnapshot"], json!(snapshot));

    // Bigger is ordinary — a volume is grown.
    let bigger = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-3", "spec": { "sizeGib": 200, "sourceSnapshot": snapshot } }),
        )
        .await;
    assert_eq!(bigger.status, StatusCode::ACCEPTED, "{}", bigger.body);

    // Smaller is the clone not fitting in what it is written into.
    let smaller = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-4", "spec": { "sizeGib": 50, "sourceSnapshot": snapshot } }),
        )
        .await;
    assert_eq!(smaller.error_code(), "FAILED_PRECONDITION");
    assert_eq!(smaller.field(), "spec.sizeGib");
    assert!(
        smaller.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at least as big"),
        "{}",
        smaller.body
    );
}

#[tokio::test]
async fn a_volume_comes_from_one_place() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let created = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    let snapshot = created.body["target"].as_str().unwrap().to_string();

    // Not taken yet: there is nothing to clone, and the pool would fail on it
    // one pass later.
    let early = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-2", "spec": { "sizeGib": 100, "sourceSnapshot": snapshot } }),
        )
        .await;
    assert_eq!(early.error_code(), "FAILED_PRECONDITION");
    assert_eq!(early.field(), "spec.sourceSnapshot");

    h.taken(&snapshot, 100).await;
    let both = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-3", "spec": {
                "sizeGib": 100,
                "sourceSnapshot": snapshot,
                "sourceImage": "projects/p1/images/sha256-abc",
            } }),
        )
        .await;
    assert_eq!(both.error_code(), "FAILED_PRECONDITION");
    assert!(
        both.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not from both"),
        "{}",
        both.body
    );
}

#[tokio::test]
async fn a_volume_is_not_restored_in_place() {
    // What an operator reaches for when they mean "restore this". It is
    // refused, and the refusal says what to do instead — because an in-place
    // restore would be a command sitting in a spec, carried out again on every
    // resync, undoing whatever the guest wrote in between.
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let created = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    let snapshot = created.body["target"].as_str().unwrap().to_string();
    h.taken(&snapshot, 100).await;

    let refused = h
        .patch(&volume, json!({ "spec": { "sourceSnapshot": snapshot } }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.field(), "spec.sourceSnapshot");
    let sentence = refused.body["error"]["message"].as_str().unwrap();
    assert!(sentence.contains("in place"), "{sentence}");
    assert!(
        sentence.contains("Create a volume"),
        "the refusal did not say what to do instead: {sentence}"
    );

    // Nothing was written: the volume still says where it came from, which is
    // nowhere.
    assert_eq!(
        h.get(&volume).await.body["spec"]["sourceSnapshot"],
        Value::Null
    );
}

/// Re-pointing a volume at another pool moves no bytes, and what it used to
/// produce was worse than a refusal: the pool holding it stopped matching its
/// own watch filter and let go, the named pool saw a volume another pool still
/// had claimed and declined it, and the volume sat converging on nothing — with
/// no condition, no event and no log line, because every component had done
/// exactly the right thing.
/// A volume a pool cannot hold is refused before it exists.
///
/// Found on a real cell: the pool was an LVM group the operating system had
/// filled, the volume was accepted, and `lvcreate: insufficient free space`
/// repeated for ever in a journal on another machine — an answer, in a place
/// nobody was looking.
#[tokio::test]
async fn a_volume_a_pool_cannot_hold_is_refused_at_the_door() {
    let h = Harness::new();
    h.pool("tight").await;
    // The pool has spoken: 10 GiB of room, 8 already promised.
    let mut p = h.pools().get("pools/tight").await.unwrap().unwrap();
    p.status.backend = "lvm".into();
    p.status.capacity_gib = 10;
    p.status.allocated_gib = 8;
    h.pools()
        .update(&p, &velstra_cloud_model::access::Writer::agent("tight"))
        .await
        .unwrap();

    let refused = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "zu-gross", "spec": { "pool": "tight", "sizeGib": 5 } }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    let why = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(why.contains("2 GiB left") && why.contains("5 GiB"), "{why}");
    assert_eq!(refused.body["error"]["field"], "spec.sizeGib");

    // What fits, fits.
    let fits = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "passt", "spec": { "pool": "tight", "sizeGib": 2 } }),
        )
        .await;
    assert_eq!(fits.status, StatusCode::ACCEPTED, "{:?}", fits.body);

    // A pool whose agent has not spoken yet refuses nothing: its zero is
    // "nothing is known", not "no room", and a freshly registered pool must
    // not be unusable until its first heartbeat.
    h.pool("neu").await;
    let unknown = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "blind", "spec": { "pool": "neu", "sizeGib": 100 } }),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::ACCEPTED, "{:?}", unknown.body);
}

#[tokio::test]
async fn a_volume_is_not_moved_between_pools_by_editing_its_pool() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;

    let refused = h
        .patch(&volume, json!({ "spec": { "pool": "spinning-rust" } }))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.field(), "spec.pool");
    let sentence = refused.body["error"]["message"].as_str().unwrap();
    assert!(sentence.contains("moves none of them"), "{sentence}");
    assert!(
        sentence.contains("sourceBackup"),
        "the refusal did not say what to do instead: {sentence}"
    );

    // And nothing moved, including the record of where it is.
    assert_ne!(
        h.get(&volume).await.body["spec"]["pool"],
        json!("spinning-rust")
    );
}

/// Writing back the pool it already has is not a change. A client that read an
/// object and is sending part of it back must never be refused for a field it
/// did not touch.
#[tokio::test]
async fn sending_back_the_pool_a_volume_already_has_is_not_a_move() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let same = h.get(&volume).await.body["spec"]["pool"]
        .as_str()
        .unwrap()
        .to_string();

    let accepted = h
        .patch(&volume, json!({ "spec": { "pool": same, "sizeGib": 200 } }))
        .await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
}

#[tokio::test]
async fn where_a_volume_came_from_is_history_rather_than_a_control() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let created = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data-1", "spec": {
                "sizeGib": 20,
                "pool": "pool-a",
                "sourceImage": "projects/p1/images/sha256-abc",
            } }),
        )
        .await;
    let volume = created.body["target"].as_str().unwrap().to_string();

    let refused = h
        .patch(
            &volume,
            json!({ "spec": { "sourceImage": "projects/p1/images/sha256-def" } }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.field(), "spec.sourceImage");

    // Sending back what is already there is not a change: a client that read
    // an object and is writing part of it back must not be refused for
    // carrying a field it did not touch.
    let unchanged = h
        .patch(
            &volume,
            json!({ "spec": {
                "sizeGib": 40,
                "sourceImage": "projects/p1/images/sha256-abc",
            } }),
        )
        .await;
    assert_eq!(unchanged.status, StatusCode::OK, "{}", unchanged.body);
    assert_eq!(unchanged.body["spec"]["sizeGib"], json!(40));
}

#[tokio::test]
async fn deleting_a_snapshot_waits_for_the_pool_like_a_volume_does() {
    let h = Harness::new();
    h.pool("pool-a").await;
    h.pool("pool-b").await;
    let volume = h.volume("data-1", 100).await;
    let created = h
        .post(&format!("{volume}/snapshots"), json!({ "id": "nightly" }))
        .await;
    let snapshot = created.body["target"].as_str().unwrap().to_string();

    // The guard a controller puts on before the pool is asked for anything.
    let snapshots = h.snapshots();
    let mut held = snapshots.get(&snapshot).await.unwrap().unwrap();
    held.meta
        .add_finalizer(velstra_cloud_model::resources::POOL_RELEASE_FINALIZER);
    snapshots
        .update(
            &held,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let deleted = h.send("DELETE", &snapshot, None, &[]).await;
    assert_eq!(deleted.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.get(&snapshot).await.status,
        StatusCode::OK,
        "a deleting copy stopped being readable while the pool still held it"
    );
}

// ---- security groups -----------------------------------------------------

#[tokio::test]
async fn a_security_group_is_an_ordinary_collection() {
    let h = Harness::new();
    let made = h
        .post(
            "projects/p1/security-groups",
            json!({
                "id": "web",
                "spec": { "rules": [{
                    "direction": "ingress",
                    "protocol": "tcp",
                    "ports": { "from": 443, "to": 443 },
                    "remote": { "cidr": "0.0.0.0/0" }
                }] }
            }),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);

    let read = h.get("projects/p1/security-groups/web").await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["spec"]["rules"][0]["protocol"], "tcp");
}

#[tokio::test]
async fn a_rule_that_cannot_mean_what_it_says_is_refused_with_its_index() {
    let h = Harness::new();
    let refused = h
        .post(
            "projects/p1/security-groups",
            json!({
                "id": "bad",
                "spec": { "rules": [
                    {
                        "direction": "ingress",
                        "protocol": "tcp",
                        "ports": { "from": 22, "to": 22 },
                        "remote": { "cidr": "0.0.0.0/0" }
                    },
                    {
                        // A port range on a protocol that has none. Accepting
                        // this would show the operator a narrower rule than the
                        // one in force.
                        "direction": "ingress",
                        "protocol": "icmp",
                        "ports": { "from": 0, "to": 65535 },
                        "remote": { "cidr": "0.0.0.0/0" }
                    }
                ] }
            }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    assert_eq!(refused.field(), "spec.rules[1]", "{:?}", refused.body);
}

#[tokio::test]
async fn whether_a_group_is_in_force_is_answered_from_the_ports_that_use_it() {
    let h = Harness::new();
    h.post(
        "projects/p1/security-groups",
        json!({ "id": "web", "spec": { "rules": [] } }),
    )
    .await;

    // Nothing references it yet: vacuously in force, and it says so rather than
    // sitting at "unknown" for ever.
    let alone = h.get("projects/p1/security-groups/web").await;
    assert_eq!(condition(&alone.body, "Applied")["status"], "True");
    assert_eq!(condition(&alone.body, "Applied")["reason"], "InForce");

    // A port names it, and no node carries that port. Not an alarm: nobody is
    // expected to have programmed anything, and saying "pending" here would be
    // an alarm about nothing.
    h.post(
        "projects/p1/ports",
        json!({
            "id": "port-a",
            "spec": {
                "network": "projects/p1/networks/n1",
                "subnet": "projects/p1/subnets/s1",
                "securityGroups": ["projects/p1/security-groups/web"]
            }
        }),
    )
    .await;
    let spare = h.get("projects/p1/security-groups/web").await;
    let applied = condition(&spare.body, "Applied");
    assert_eq!(applied["status"], "True", "{:?}", spare.body);
    assert!(
        applied["message"]
            .as_str()
            .unwrap_or_default()
            .contains("none of them carried"),
        "{applied:?}"
    );

    // A node picks the port up and has not programmed it yet. Two writes, in
    // this order and no other: the port controller assigns it — a node cannot
    // claim what nothing has assigned to it, which is the whole point of the
    // access rule — and only then may the node report on it.
    let ports: TypedStore<PortSpec, PortStatus> =
        TypedStore::new(h.store.clone(), "cell-1", "ports");
    let mut port = ports
        .get("projects/p1/ports/port-a")
        .await
        .unwrap()
        .expect("the port was created above");
    port.spec.node = Some("node-a".into());
    port.meta.generation += 1;
    ports
        .update(&port, &Writer::controller("port"))
        .await
        .unwrap();
    let mut port = ports
        .get("projects/p1/ports/port-a")
        .await
        .unwrap()
        .unwrap();
    port.status.node = Some("node-a".into());
    ports.update(&port, &Writer::agent("node-a")).await.unwrap();

    let pending = h.get("projects/p1/security-groups/web").await;
    let applied = condition(&pending.body, "Applied");
    assert_eq!(applied["status"], "False", "{:?}", pending.body);
    assert_eq!(applied["reason"], "PortsPending");
    assert!(
        applied["message"]
            .as_str()
            .unwrap_or_default()
            .contains("projects/p1/ports/port-a"),
        "the message does not say which port: {applied:?}"
    );
}

#[tokio::test]
async fn a_group_is_status_like_everything_else_and_cannot_be_patched() {
    let h = Harness::new();
    h.post(
        "projects/p1/security-groups",
        json!({ "id": "web", "spec": { "rules": [] } }),
    )
    .await;
    let refused = h
        .patch(
            "projects/p1/security-groups/web",
            json!({ "status": { "observedGeneration": 9 } }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
}

/// One condition out of a document, or `Value::Null` if it is not there.
fn condition(document: &Value, kind: &str) -> Value {
    document["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["kind"] == kind)
        .cloned()
        .unwrap_or(Value::Null)
}

// ---- paging ---------------------------------------------------------------

/// The query parameters a client actually sends, spelled the way AIP-158 spells
/// them. The behaviour is proved in `paging.rs`; what is proved here is that the
/// REST surface reaches it, and reaches it under the names a client will use.
#[tokio::test]
async fn a_rest_list_pages_under_the_names_a_client_expects() {
    let both = Harness::new();
    for i in 0..5 {
        both.instance(
            "p1",
            &format!("i{i}"),
            json!({"vcpus": 1, "memoryMib": 512}),
        )
        .await;
    }

    let first = both.get("projects/p1/instances?pageSize=2").await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
    assert_eq!(first.body["items"].as_array().unwrap().len(), 2);
    let token = first.body["nextPageToken"]
        .as_str()
        .expect("a short page did not offer a token")
        .to_string();

    let mut seen = 2;
    let mut token = Some(token);
    while let Some(t) = token.take() {
        let next = both
            .get(&format!("projects/p1/instances?pageSize=2&pageToken={t}"))
            .await;
        assert_eq!(next.status, StatusCode::OK, "{:?}", next.body);
        seen += next.body["items"].as_array().unwrap().len();
        token = next.body["nextPageToken"].as_str().map(str::to_string);
    }
    assert_eq!(seen, 5, "the walk lost or repeated objects");
}

#[tokio::test]
async fn a_rest_list_without_paging_is_unchanged() {
    // The contract every client written before paging relies on. A field that
    // appeared on every answer — even empty — would also be a change, so the
    // token is absent rather than "".
    let both = Harness::new();
    both.instance("p1", "i1", json!({"vcpus": 1, "memoryMib": 512}))
        .await;
    let listed = both.get("projects/p1/instances").await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);
    assert!(
        listed.body.get("nextPageToken").is_none(),
        "an unpaged answer grew a field: {:?}",
        listed.body
    );
}

#[tokio::test]
async fn a_page_size_that_is_not_a_number_is_refused_rather_than_ignored() {
    // Ignoring it hands back the whole cell to a client that believes it asked
    // for twenty — the shape where a load test passes and production does not.
    let both = Harness::new();
    let answer = both.get("projects/p1/instances?pageSize=twenty").await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
    assert!(
        answer.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("pageSize"),
        "the refusal did not name the parameter: {:?}",
        answer.body
    );
}

#[tokio::test]
async fn a_forged_page_token_is_refused() {
    let both = Harness::new();
    let answer = both
        .get("projects/p1/instances?pageSize=2&pageToken=obviously-not-one")
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
}

// ---- ceph ----------------------------------------------------------------

/// A cluster naming a disk the node will not give up is refused, with the
/// reason and the field.
///
/// Nothing stands between this disk and being erased *anyway* — the console
/// only offers disks the model accepts, and the deployment refuses again
/// against the node's live inventory before any command runs. What this stands
/// between is an operator and a silence: a hand-written spec naming the wrong
/// disk would otherwise be accepted, and the only sign of the mistake would be
/// a cluster that quietly never finishes.
#[tokio::test]
async fn a_ceph_cluster_naming_a_disk_that_is_not_free_is_refused_with_the_reason() {
    use velstra_cloud_model::ceph::{BlockDevice, DeviceUse};

    let h = Harness::new();
    let node = Resource::new(
        Meta::new(
            ResourceName::parse("nodes/hv-1").unwrap(),
            Placement::new("eu-central", "cell-1"),
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
            // Like the fixture's other machines: one state directory between
            // them, so what these tests are about is what they say they are
            // about — capacity, an image, a CPU — and not the disk rule.
            shared_state: true,
            vmm: "qemu".into(),
            fetching: Vec::new(),
            devices: vec![
                BlockDevice {
                    path: "/dev/sdb".into(),
                    kernel_name: "sdb".into(),
                    size_gib: 512,
                    rotational: false,
                    state: DeviceUse::Filesystem {
                        fstype: "ext4".into(),
                    },
                    ..Default::default()
                },
                BlockDevice {
                    path: "/dev/sdc".into(),
                    kernel_name: "sdc".into(),
                    size_gib: 512,
                    rotational: false,
                    state: DeviceUse::Free,
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    );
    h.nodes()
        .create(
            &node,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let answer = h
        .post(
            "ceph-clusters",
            json!({
                "id": "ceph",
                "spec": {
                    "publicNetwork": "10.0.0.0/24",
                    "monitors": ["hv-1"],
                    "osds": [{ "node": "hv-1", "device": "/dev/sdb" }],
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
    // The reason, in the words an operator can act on — "it has an ext4
    // filesystem on it" is the whole of the help. And the field, so a console
    // can point at the row rather than at the form.
    let message = answer.body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("ext4"), "{message}");
    assert_eq!(answer.field(), "spec.osds[0].device");

    // The empty one is accepted, so the refusal is about the disk and not about
    // the shape of the request.
    let answer = h
        .post(
            "ceph-clusters",
            json!({
                "id": "ceph",
                "spec": {
                    "publicNetwork": "10.0.0.0/24",
                    "monitors": ["hv-1"],
                    "osds": [{ "node": "hv-1", "device": "/dev/sdc" }],
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);
}

/// A disk on a machine that has not reported yet is not refused.
///
/// "I cannot see it" is not "it is not free". Refusing a cluster because a node
/// is still booting would be wrong about the one thing this check is for, and
/// would make the platform's answer depend on the order somebody did things in.
#[tokio::test]
async fn a_disk_on_a_node_that_has_not_reported_is_not_refused() {
    let h = Harness::new();
    let answer = h
        .post(
            "ceph-clusters",
            json!({
                "id": "ceph",
                "spec": {
                    "publicNetwork": "10.0.0.0/24",
                    "monitors": ["hv-1"],
                    "osds": [{ "node": "hv-1", "device": "/dev/sdb" }],
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);
}

// ---- images --------------------------------------------------------------

/// An image carrying a signature is refused, with the reason.
///
/// The field was declared as "a cosign-style signature, verified before a node
/// will boot it", and nothing has ever read it — while the console offered a
/// box to type one into and a column headed *Signed*. An operator could paste
/// anything and the platform reported, at a glance, that the image was signed.
///
/// Refused rather than stored and ignored, because storing it is where the
/// claim comes from: every place an unchecked claim is displayed becomes
/// evidence somebody will cite.
#[tokio::test]
async fn an_image_carrying_a_signature_nothing_verifies_is_refused() {
    let h = Harness::new();
    let answer = h
        .post(
            "projects/p1/images",
            json!({
                "id": "sha256-abc",
                "spec": {
                    "digest": "sha256:abc",
                    "format": "Raw",
                    "sizeBytes": 1024,
                    "sourceUrl": "https://example.invalid/alpine.img",
                    "signature": "MEUCIQD-not-checked-by-anything"
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
    assert_eq!(answer.field(), "spec.signature");
    let message = answer.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("verifies"),
        "the refusal did not say why: {message}"
    );

    // The same image without one is accepted, so the refusal is about the
    // claim and not about the shape of the request.
    let answer = h
        .post(
            "projects/p1/images",
            json!({
                "id": "sha256-abc",
                "spec": {
                    "digest": "sha256:abc",
                    "format": "Raw",
                    "sizeBytes": 1024,
                    "sourceUrl": "https://example.invalid/alpine.img"
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);

    // And an explicitly empty one is not a claim: a client echoing back an
    // object it read must not be told off for a field it did not touch.
    let answer = h
        .post(
            "projects/p1/images",
            json!({
                "id": "sha256-def",
                "spec": {
                    "digest": "sha256:def",
                    "format": "Raw",
                    "sizeBytes": 1024,
                    "sourceUrl": "https://example.invalid/other.img",
                    "signature": ""
                }
            }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);
}

/// Keeping a group apart can be a rule or a wish, and so can keeping it
/// together. Both are right answers to different questions.
#[tokio::test]
async fn a_guest_can_ask_to_be_near_its_group_and_be_told_where_the_group_is() {
    let h = Harness::new();
    two_nodes(&h).await;

    // One guest of the group, already placed by the fixture's own scheduler
    // path — read back rather than assumed.
    let first = h
        .instance(
            "p1",
            "web-1",
            json!({ "vcpus": 1, "memory_mib": 512,
                    "placement_policy": { "affinity_group": "checkout" } }),
        )
        .await;
    let placed = h.get(&first).await;
    assert_eq!(placed.status, StatusCode::OK, "{:?}", placed.body);

    // A second, too large for any machine, asking to be with the first. The
    // rejection names where the group is rather than answering "no valid host".
    let second = h
        .instance(
            "p1",
            "cache-1",
            json!({ "vcpus": 1, "memory_mib": 99999,
                    "placement_policy": { "affinity_group": "checkout" } }),
        )
        .await;
    let why = h.get(&format!("{second}:explainPlacement")).await;
    assert_eq!(why.status, StatusCode::OK, "{:?}", why.body);
    let rejected = why.body["rejected"].as_array().unwrap();
    assert!(!rejected.is_empty(), "nothing was even considered");

    // A wish rather than a rule places it anyway, given room.
    let loose = h
        .instance(
            "p1",
            "cache-2",
            json!({ "vcpus": 1, "memory_mib": 512,
                    "placement_policy": {
                        "affinity_group": "checkout",
                        "affinity": "Preferred",
                    } }),
        )
        .await;
    let answer = h.get(&format!("{loose}:explainPlacement")).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
    assert!(
        !answer.body["placed"].is_null(),
        "a preferred affinity refused a placement it should have taken: {:?}",
        answer.body
    );

    // And an unreadable strength is not invented: it reads as `Required`,
    // which is the value the field has when nobody set it.
    let typo = h
        .post(
            "projects/p1/instances",
            json!({ "id": "cache-3", "spec": { "vcpus": 1, "memory_mib": 512,
                    "placement_policy": { "affinity_group": "checkout", "affinity": "prefered" } } }),
        )
        .await;
    assert_ne!(
        typo.status,
        StatusCode::ACCEPTED,
        "a misspelled strength was accepted and silently read as something: {:?}",
        typo.body
    );
}

/// The address the guest actually holds, end to end through the API: which
/// network it may come from, who may declare one, who announces it, and what
/// the guest is told to configure.
#[tokio::test]
async fn a_routed_public_address_is_the_guests_own_and_says_where_it_is_announced() {
    let h = Harness::new();
    two_nodes(&h).await;

    // A tenant range, and a real one. The difference is a flag only an
    // operator may set.
    for (id, external, vni) in [("tenant", false, 5001u32), ("public", true, 5002)] {
        let made = h
            .post(
                "projects/p1/networks",
                json!({ "id": id, "spec": { "vni": vni, "mtu": 1450, "external": external }}),
            )
            .await;
        assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);
    }
    h.post(
        "projects/p1/subnets",
        json!({ "id": "tenant", "spec": {
            "network": "projects/p1/networks/tenant",
            "cidr": "10.20.0.0/24", "gateway": "10.20.0.1", "dns": [], "reserved": [],
        }}),
    )
    .await;
    h.post(
        "projects/p1/subnets",
        json!({ "id": "public", "spec": {
            "network": "projects/p1/networks/public",
            "cidr": "203.0.113.0/24", "gateway": "203.0.113.1", "dns": [], "reserved": [],
        }}),
    )
    .await;

    // A routed address from a tenant range is refused, in words that say why
    // it could never work.
    let inside = h
        .post(
            "projects/p1/floatingips",
            json!({ "id": "wrong", "spec": {
                "subnet": "projects/p1/subnets/tenant", "delivery": "Routed",
            }}),
        )
        .await;
    assert_eq!(inside.status, StatusCode::BAD_REQUEST, "{:?}", inside.body);
    let said = inside.body["error"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("real nowhere"), "{said}");

    // From the external one, announced from the host holding the guest.
    let made = h
        .post(
            "projects/p1/floatingips",
            json!({ "id": "web", "spec": {
                "subnet": "projects/p1/subnets/public",
                "delivery": "Routed", "announce": "FromHost",
                "address": "203.0.113.7",
            }}),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);

    let reach = h.get("projects/p1/floatingips/web:explainReach").await;
    assert_eq!(reach.status, StatusCode::OK, "{:?}", reach.body);
    assert_eq!(reach.body["delivery"], json!("Routed"));
    assert_eq!(reach.body["external"], json!(true));
    // Bound to nothing yet, which is a held address and not a fault — and the
    // answer says which of the four reasons it is.
    assert_eq!(reach.body["announced"]["from"], json!(null));
    assert!(
        reach.body["announced"]["why"]
            .as_str()
            .unwrap_or_default()
            .contains("held for later"),
        "{:?}",
        reach.body["announced"]
    );

    // What the guest must have: a host route through a next hop in no subnet,
    // and the default route through it.
    let guest = &reach.body["guest"];
    assert_eq!(guest["address"], json!("203.0.113.7/32"));
    assert_eq!(guest["via"], json!("169.254.1.1"));
    assert_eq!(guest["onLink"], json!(true));
    assert_eq!(guest["defaultRoute"], json!(true));
}

/// Announcing from a gateway in a cell that has none is refused where it is
/// asked for, and the refusal names both ways out.
#[tokio::test]
async fn a_gateway_announcement_in_a_cell_with_no_gateway_is_refused() {
    let h = Harness::new();
    two_nodes(&h).await;
    h.post(
        "projects/p1/networks",
        json!({ "id": "public", "spec": { "vni": 5002, "mtu": 1450, "external": true }}),
    )
    .await;
    h.post(
        "projects/p1/subnets",
        json!({ "id": "public", "spec": {
            "network": "projects/p1/networks/public",
            "cidr": "203.0.113.0/24", "gateway": "203.0.113.1", "dns": [], "reserved": [],
        }}),
    )
    .await;
    let port = h
        .post(
            "projects/p1/ports",
            json!({ "id": "pt1", "spec": {
                "network": "projects/p1/networks/public",
                "subnet": "projects/p1/subnets/public",
            }}),
        )
        .await;
    assert_eq!(port.status, StatusCode::ACCEPTED, "{:?}", port.body);

    let asked = h
        .post(
            "projects/p1/floatingips",
            json!({ "id": "web", "spec": {
                "subnet": "projects/p1/subnets/public",
                "port": "projects/p1/ports/pt1",
                "delivery": "Routed", "announce": "FromGateway",
            }}),
        )
        .await;
    assert_eq!(asked.status, StatusCode::BAD_REQUEST, "{:?}", asked.body);
    let said = asked.body["error"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("Mark one"), "{said}");
    assert!(said.contains("peer with the network above"), "{said}");
}

/// A place copies are kept, created the way an operator creates one — and the
/// spelling of `kind`, which is the one thing about this object somebody typing
/// JSON gets wrong.
///
/// `directory`, lowercase. It reads as an odd exception beside `Running` and
/// `Stopped`, and it is: the enum is kebab-cased, and a contract that is silent
/// about which convention a field follows is one people guess at. The refusal
/// at least says both spellings.
#[tokio::test]
async fn a_backup_target_is_created_with_the_fields_the_contract_names() {
    let h = Harness::new();
    let answer = h
        .post(
            "backup-targets",
            json!({ "id": "archive", "spec": {
                "kind": "directory", "path": "/srv/archive", "accepting": true,
            }}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);

    // And the guess, refused in a sentence that names what was expected.
    let wrong = h
        .post(
            "backup-targets",
            json!({ "id": "other", "spec": { "kind": "Directory", "path": "/srv/other" }}),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{:?}", wrong.body);
    let said = wrong.body["error"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("expected `directory`"), "{said}");
}

/// One tenant in a loop does not take the write path from everybody else — and
/// the refusal says exactly how long to wait, so a client that reads it stops
/// spinning.
#[tokio::test]
async fn a_caller_writing_in_a_loop_is_slowed_down_and_told_for_how_long() {
    let h = Harness::with_write_rate(velstra_cloud_model::limit::Rate {
        per_second: 2,
        burst: 4,
    });
    h.post("projects", json!({ "id": "p1", "spec": { "quota": {} } }))
        .await;

    // The burst is real work somebody may legitimately do at once, minus the
    // project above.
    let mut refused = None;
    for i in 0..12 {
        let answer = h
            .post(
                "projects/p1/instances",
                json!({ "id": format!("i{i}"), "spec": { "vcpus": 1, "memory_mib": 512 } }),
            )
            .await;
        if answer.status == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(answer);
            break;
        }
    }
    let refused = refused.expect("a caller writing as fast as it could was never slowed down");
    assert_eq!(
        refused.error_code(),
        "RESOURCE_EXHAUSTED",
        "{:?}",
        refused.body
    );
    let said = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(said.contains("Try again in"), "{said}");
    // The header, because that is what a client library reads — and at least
    // one second, so a client that obeys it to the letter is never turned away
    // twice for the same reason.
    let after = refused
        .header("retry-after")
        .expect("a rate limit says when to come back");
    assert!(after.parse::<u64>().is_ok_and(|s| s >= 1), "{after}");

    // Reads are not counted. A caller reading in a loop is only slowing
    // themselves down, and a limiter that refused them would make a console
    // that polls look broken.
    for _ in 0..20 {
        let read = h.get("projects/p1/instances").await;
        assert_eq!(read.status, StatusCode::OK, "a read was rate limited");
    }
}

/// Sharing a processor is a trade an operator makes on purpose; sharing memory
/// is not that trade, and there is deliberately no ratio for it.
#[tokio::test]
async fn a_node_can_be_told_to_share_its_cores_but_never_its_memory() {
    let h = Harness::new();
    two_nodes(&h).await;

    let ok = h
        .patch("nodes/node-a", json!({ "spec": { "vcpuOvercommit": 4 } }))
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);

    // The cell now offers more processor than it has silicon, and says both
    // numbers: one without the other reads as though it had grown a processor.
    let room = h.get("nodes:explainCapacity").await;
    assert_eq!(room.status, StatusCode::OK, "{:?}", room.body);
    let cores = room.body["total"]["vcpus"].as_u64().unwrap();
    let offered = room.body["offeredVcpus"].as_u64().unwrap();
    assert!(
        offered > cores,
        "the ratio did not reach the capacity report: {offered} of {cores}"
    );

    // And a ratio past the point where it stops being a trade is refused with
    // the reason, rather than accepted as a way of hiding that a cell is full.
    let absurd = h
        .patch("nodes/node-a", json!({ "spec": { "vcpuOvercommit": 500 } }))
        .await;
    assert_eq!(absurd.status, StatusCode::BAD_REQUEST, "{:?}", absurd.body);
    let said = absurd.body["error"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("hiding that the cell is full"), "{said}");

    // There is no memory ratio to set, and a body inventing one is refused as
    // an unknown field rather than quietly ignored.
    let memory = h
        .patch("nodes/node-a", json!({ "spec": { "memoryOvercommit": 2 } }))
        .await;
    assert_ne!(
        memory.status,
        StatusCode::OK,
        "a memory ratio was accepted: {:?}",
        memory.body
    );
}

/// "What has happened to this guest" — the question every console user has and
/// no listing answered.
#[tokio::test]
async fn the_records_about_one_object_can_be_asked_for_without_reading_the_cell() {
    let h = Harness::new();
    let one = h.instance("p1", "i1", json!({ "vcpus": 2 })).await;
    let other = h.instance("p1", "i2", json!({ "vcpus": 2 })).await;

    let all = h.get("projects/p1/operations").await;
    assert_eq!(all.status, StatusCode::OK, "{:?}", all.body);
    assert!(
        all.body["items"].as_array().unwrap().len() >= 2,
        "the fixture produced no operations to narrow"
    );

    let mine = h.get(&format!("projects/p1/operations?target={one}")).await;
    assert_eq!(mine.status, StatusCode::OK, "{:?}", mine.body);
    let items = mine.body["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "the object's own history came back empty"
    );
    assert!(
        items.iter().all(|o| o["spec"]["target"] == json!(one)),
        "somebody else's history came back: {items:?}"
    );
    assert!(
        !items.iter().any(|o| o["spec"]["target"] == json!(other)),
        "the filter let another object's records through"
    );

    // A selector that matches nothing is an empty list, not an error: "nothing
    // has happened to it" is an answer.
    let none = h
        .get("projects/p1/operations?target=projects/p1/instances/never")
        .await;
    assert_eq!(none.status, StatusCode::OK, "{:?}", none.body);
    assert_eq!(none.body["items"], json!([]));

    // And a collection that carries no target says so, rather than answering
    // with the whole cell as though the filter had been applied.
    let wrong = h.get(&format!("projects/p1/instances?target={one}")).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{:?}", wrong.body);
}

/// What a project has left and what it can actually start — and which of the
/// two is standing in the way, which is the whole reason both halves are in one
/// answer.
#[tokio::test]
async fn a_projects_allowance_says_whether_the_quota_or_the_cell_is_in_the_way() {
    let h = Harness::new();
    two_nodes(&h).await;
    let created = h
        .post(
            "projects",
            json!({ "id": "p1", "spec": { "quota": { "instances": 10, "vcpus": 40 } } }),
        )
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);

    let answer = h.get("projects/p1:explainQuota").await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);

    // Every dimension, in a fixed order, whether or not it is in use: a page
    // that showed only the interesting ones would rearrange itself between two
    // reads of the same screen.
    let dims = answer.body["dimensions"].as_array().unwrap();
    assert_eq!(dims.len(), 8, "{dims:?}");
    assert_eq!(dims[0]["name"], json!("instances"));
    assert!(dims.iter().any(|d| d["name"] == json!("devices")));

    // Nobody set a memory quota, so `left` is null rather than zero — the two
    // are different answers and a screen must not render one as the other.
    let memory = dims
        .iter()
        .find(|d| d["name"] == json!("memoryMib"))
        .unwrap();
    assert_eq!(memory["unlimited"], json!(true), "{memory:?}");
    assert_eq!(memory["left"], json!(null), "{memory:?}");
    assert_eq!(memory["exhausted"], json!(false), "{memory:?}");

    // The fixture's nodes are 16 GiB each and nothing caps memory, so the
    // machines are what is in the way — and the answer says so rather than
    // sending this tenant to ask for allowance they already have without bound.
    let most = &answer.body["largestStartable"];
    assert_eq!(most["memoryLimitedBy"], json!("cell"), "{most:?}");
    assert_eq!(most["none"], json!(false), "{most:?}");

    // And a machine out of service does not lend its memory to that promise.
    let now = velstra_cloud_model::meta::Timestamp::now().0;
    for id in ["node-a", "node-b", "node-tiny"] {
        let answer = h
            .post(
                "maintenance-windows",
                json!({ "id": format!("out-{id}"), "spec": {
                    "node": id, "startsAt": now - 60_000, "minutes": 60, "drain": false,
                }}),
            )
            .await;
        assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);
    }
    let answer = h.get("projects/p1:explainQuota").await;
    let most = &answer.body["largestStartable"];
    assert_eq!(
        most["none"],
        json!(true),
        "a cell with every machine out of service still promised a guest: {most:?}"
    );
}

/// A window is a declaration, and the four ways of declaring a meaningless one
/// are refused at the door — because all four are knowable then, and the
/// alternative is finding out at three in the morning.
#[tokio::test]
async fn a_maintenance_window_that_could_never_take_effect_is_refused() {
    let h = Harness::new();
    two_nodes(&h).await;
    let hour = 3_600_000u64;
    let now = velstra_cloud_model::meta::Timestamp::now().0;

    let declare = |id: &str, node: &str, starts: u64, minutes: u64| {
        json!({ "id": id, "spec": {
            "node": node, "startsAt": starts, "minutes": minutes, "drain": false,
        }})
    };

    // Zero minutes: it would sit in the list looking like a plan.
    let answer = h
        .post(
            "maintenance-windows",
            declare("nothing", "node-a", now + hour, 0),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);

    // Already over. Declared for last night by somebody in the wrong timezone.
    let answer = h
        .post(
            "maintenance-windows",
            declare("gone", "node-a", now - 2 * hour, 30),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);

    // A start in the past is *accepted*: work that has already begun is a true
    // thing to declare, and refusing it teaches people to lie about the time.
    let answer = h
        .post(
            "maintenance-windows",
            declare("started", "node-a", now - 600_000, 60),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);

    // And a second window over the same node and the same hour is two answers
    // to one question, so it is refused by name.
    let answer = h
        .post("maintenance-windows", declare("again", "node-a", now, 60))
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
    let said = answer.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        said.contains("started"),
        "the refusal did not name which one: {said}"
    );

    // The same hour on another machine is the ordinary case, not a conflict.
    let answer = h
        .post(
            "maintenance-windows",
            declare("elsewhere", "node-b", now, 60),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);
}

/// What an open window costs, answered before anybody commits to it — and the
/// node stops being a candidate for new work while it is open.
#[tokio::test]
async fn a_node_in_an_open_window_is_explained_and_no_longer_placed_on() {
    let h = Harness::new();
    two_nodes(&h).await;
    let now = velstra_cloud_model::meta::Timestamp::now().0;

    // node-b is out for the next hour, with the operator's own words on it.
    let answer = h
        .post(
            "maintenance-windows",
            json!({ "id": "dimm-swap", "spec": {
                "node": "node-b",
                "startsAt": now - 600_000,
                "minutes": 60,
                "drain": false,
                "note": "swapping the failed DIMM in slot 3",
            }}),
        )
        .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{:?}", answer.body);

    let answer = h.get("nodes/node-b:explainMaintenance").await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
    assert_eq!(
        answer.body["open"]["window"],
        json!("maintenance-windows/dimm-swap")
    );
    assert_eq!(answer.body["open"]["drain"], json!(false));
    // Nothing is being asked to leave: that is what `drain: false` means, and
    // a console reading this must not warn about a fleet that is staying put.
    assert_eq!(answer.body["draining"], json!(false));
    assert_eq!(answer.body["willMove"], json!([]));

    // A machine nobody scheduled anything for says so plainly rather than
    // answering with an error.
    let answer = h.get("nodes/node-a:explainMaintenance").await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
    assert_eq!(answer.body["open"], json!(null));
    assert_eq!(answer.body["next"], json!(null));

    // And placement now refuses it, in the operator's own words rather than
    // as "no valid host". Sized past node-a on purpose: a guest that fits
    // somewhere is simply placed, and the rejection chain is only written when
    // nothing took it.
    let name = h
        .instance("p1", "big", json!({ "vcpus": 2, "memory_mib": 99999 }))
        .await;
    let answer = h.get(&format!("{name}:explainPlacement")).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
    let rejected = answer.body["rejected"].as_array().unwrap();
    let out = rejected
        .iter()
        .find(|r| r["node"] == json!("node-b"))
        .unwrap_or_else(|| panic!("node-b was not even considered: {rejected:?}"));
    assert_eq!(out["why"], json!("InMaintenance"));
    let detail = out["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("DIMM"), "{detail}");
    // Relative, because this is read hours after it was written.
    assert!(detail.contains("another"), "{detail}");
}

/// The fleet's CPU report, against the scenario it exists for: a cell that was
/// baselined, then had a machine added that stands outside the aggregate.
#[tokio::test]
async fn the_cpu_report_names_the_stray_node_and_says_whether_it_can_join() {
    let h = Harness::new();
    two_nodes(&h).await;

    // node-a and node-b are the fixture's identical machines. Give them a
    // declared baseline, and add a third that presents its own processor.
    for id in ["node-a", "node-b"] {
        let key = velstra_cloud_store::key_for("cell-1", "nodes", &format!("nodes/{id}"));
        let raw = h.store.get(&key).await.unwrap().unwrap();
        let mut node: velstra_cloud_model::resources::Node =
            serde_json::from_slice(&raw.value).unwrap();
        node.spec.cpu_baseline = Some(velstra_cloud_model::cpu::CpuLevel::V2);
        let cpu = node.status.cpu.as_mut().unwrap();
        cpu.presents = "x86-64-v2".into();
        cpu.presented_flags = velstra_cloud_model::cpu::CpuLevel::V2.flags();
        h.store
            .put(
                &key,
                serde_json::to_vec(&node).unwrap(),
                velstra_cloud_store::Expect::Revision(raw.revision),
            )
            .await
            .unwrap();
    }

    let answer = h.get("nodes:explainCpu").await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);

    // Two identical baselined machines: one domain, and it can be baselined.
    let domains = answer.body["domains"].as_array().unwrap();
    assert_eq!(domains.len(), 1, "{domains:?}");
    assert_eq!(domains[0]["level"], json!("x86-64-v2"));
    assert_eq!(domains[0]["canBaseline"], json!(true));

    // Nothing is pending: no guest has been started against the new baseline
    // and none is running with a different one.
    assert_eq!(answer.body["pendingAdoption"], json!([]));

    // And a uniform fleet says so rather than answering with silence, which
    // reads the same as a broken report.
    let advice = answer.body["advice"].as_array().unwrap();
    assert!(
        advice.iter().any(|a| a["kind"] == "AlreadyUniform"),
        "{advice:?}"
    );
}

/// Declaring a baseline a machine cannot reach is refused at the door, with
/// the shortfall named.
#[tokio::test]
async fn a_baseline_a_node_cannot_reach_is_refused_before_it_is_declared() {
    let h = Harness::new();
    two_nodes(&h).await;

    // The fixture's nodes are x86-64-v2. Ask for v3.
    let answer = h
        .patch(
            "nodes/node-a",
            json!({ "spec": { "cpuBaseline": "x86-64-v3" } }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{:?}", answer.body);
    let message = answer.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("cannot present x86-64-v3") && message.contains("avx2"),
        "the refusal does not name what the machine is short of: {message}"
    );
    assert_eq!(answer.body["error"]["field"], json!("spec.cpuBaseline"));

    // The level it *can* reach is accepted.
    let ok = h
        .patch(
            "nodes/node-a",
            json!({ "spec": { "cpuBaseline": "x86-64-v2" } }),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
}

/// A backup into the volume's own pool is refused, and the reason says why it
/// would not have been a backup.
///
/// The one rule the collection exists for. A copy beside the original is lost
/// with the pool it is in — which is the single failure anybody buys a backup
/// to survive — so a platform that accepted it would be selling a promise it
/// does not keep, and the operator would find out at the worst moment there is.
#[tokio::test]
async fn a_backup_into_the_volumes_own_pool_is_refused_with_the_reason() {
    let h = Harness::new();

    // A pool, a volume in it, and a target pointed at that same pool's path.
    // The pool publishes where it is, which is how the API can tell — a flag on
    // the target would be a claim, and this has to be a fact.
    let mut pool: velstra_cloud_model::resources::Pool = Resource::new(
        Meta::new(
            ResourceName::parse("pools/fast").unwrap(),
            Placement::new("eu-central", "cell-1"),
        ),
        velstra_cloud_model::resources::PoolSpec {
            accepting: true,
            labels: vec![],
        },
        velstra_cloud_model::resources::PoolStatus {
            backend: "directory".into(),
            ..Default::default()
        },
    );
    velstra_cloud_model::meta::set_condition(
        &mut pool.status.conditions,
        velstra_cloud_model::meta::Condition::new(
            "Located",
            velstra_cloud_model::meta::ConditionStatus::True,
            "PathIs",
            "/srv/pool-fast",
            1,
        ),
    );
    h.pools().create(&pool, &writer()).await.unwrap();

    let volume: velstra_cloud_model::resources::Volume = Resource::new(
        Meta::new(
            ResourceName::parse("projects/p1/volumes/data-1").unwrap(),
            Placement::new("eu-central", "cell-1"),
        ),
        velstra_cloud_model::resources::VolumeSpec {
            source_backup: None,
            size_gib: 40,
            pool: "pools/fast".into(),
            encryption_key: None,
            source_image: None,
            source_snapshot: None,
        },
        Default::default(),
    );
    h.volumes().create(&volume, &writer()).await.unwrap();

    for (id, path) in [
        ("same-pool", "/srv/pool-fast"),
        ("elsewhere", "/srv/backups"),
    ] {
        let t: velstra_cloud_model::resources::BackupTarget = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("backup-targets/{id}")).unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::backup::BackupTargetSpec {
                kind: velstra_cloud_model::backup::TargetKind::Directory,
                path: path.into(),
                accepting: true,
                agent: String::new(),
                verify_every_hours: 0,
            },
            velstra_cloud_model::backup::BackupTargetStatus {
                writable: Some(true),
                ..Default::default()
            },
        );
        h.backup_targets().create(&t, &writer()).await.unwrap();
    }

    let refused = h
        .post(
            "projects/p1/backups",
            json!({ "meta": { "name": "projects/p1/backups/b1" },
                    "spec": { "volume": "projects/p1/volumes/data-1",
                              "target": "backup-targets/same-pool" } }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    let message = refused.body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("lost with the pool"),
        "the refusal does not say why it would not be a backup: {message}"
    );
    assert_eq!(refused.body["error"]["field"], json!("spec.target"));

    // A target somewhere else is the ordinary case, and the pool holding the
    // source is filled in — without it the object is assigned to nobody and no
    // agent could ever claim it.
    let made = h
        .post(
            "projects/p1/backups",
            json!({ "meta": { "name": "projects/p1/backups/b2" },
                    "spec": { "volume": "projects/p1/volumes/data-1",
                              "target": "backup-targets/elsewhere" } }),
        )
        .await;
    // Accepted, with an operation naming what it will become — this API's
    // shape for every create.
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);
    let target = made.body["target"].as_str().unwrap();

    let read = h.get(target).await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
    assert_eq!(
        read.body["spec"]["pool"],
        json!("pools/fast"),
        "the pool holding the source was not filled in, so no agent could ever claim this copy"
    );
}

/// A list can be narrowed by label, and clearing the filter shows everything
/// again.
///
/// The second half is the one worth pinning: an empty selector that matched
/// nothing would make a cleared filter box read as "nothing here" instead of
/// "no filter", which is how somebody concludes their guests are gone.
#[tokio::test]
async fn a_list_is_narrowed_by_label_and_an_empty_selector_narrows_nothing() {
    let h = Harness::new();

    for (id, env) in [("web-1", "prod"), ("web-2", "prod"), ("db-1", "staging")] {
        let made = h
            .post(
                "projects/p1/networks",
                json!({ "meta": { "name": format!("projects/p1/networks/{id}"),
                                  "labels": { "env": env } },
                        "spec": { "vni": 5000 + id.len(), "mtu": 1500 } }),
            )
            .await;
        assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);
    }

    let ids = |answer: &Answer| -> Vec<String> {
        let mut out: Vec<String> = answer.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                i["meta"]["name"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        out.sort();
        out
    };

    let all = h.get("projects/p1/networks").await;
    assert_eq!(ids(&all), ["db-1", "web-1", "web-2"]);

    let prod = h.get("projects/p1/networks?labels=env=prod").await;
    assert_eq!(ids(&prod), ["web-1", "web-2"]);

    // A bare key asks whether the label is there at all.
    let tagged = h.get("projects/p1/networks?labels=env").await;
    assert_eq!(ids(&tagged), ["db-1", "web-1", "web-2"]);

    // Nothing matching is an empty list, not an error: "no networks are tagged
    // that" is an answer, and refusing would make a typo look like a broken
    // endpoint.
    let none = h.get("projects/p1/networks?labels=env=nowhere").await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(ids(&none).is_empty());

    // And cleared shows everything again.
    let cleared = h.get("projects/p1/networks?labels=").await;
    assert_eq!(ids(&cleared), ["db-1", "web-1", "web-2"]);
}

/// Resizing a running guest is said out loud instead of reading as applied.
///
/// The failure this pins is one this platform shipped, and it is the shape that
/// costs most: everything agreed. The spec said eight vCPUs, the agent had
/// genuinely handled the change so `observedGeneration` caught up, `Ready` was
/// true — and the guest went on running on two. There was no screen anywhere
/// that disagreed, which is why nobody found it by looking.
#[tokio::test]
async fn a_running_guest_says_what_it_will_only_get_when_it_restarts() {
    let h = Harness::new();
    two_nodes(&h).await;
    let name = running_guest(&h).await;

    // An agent reports what the guest is actually running on — the half that
    // already existed.
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut running = instances.get(&name).await.unwrap().unwrap();
    running.status.running_size = Some(velstra_cloud_model::resources::RunningSize {
        vcpus: 2,
        memory_mib: 4096,
        root_disk_gib: running.spec.root_disk_gib,
    });
    h.store
        .put(
            &velstra_cloud_store::key_for("cell-1", "instances", &name),
            serde_json::to_vec(&running).unwrap(),
            velstra_cloud_store::Expect::Revision(running.meta.revision),
        )
        .await
        .unwrap();

    // Nothing pending yet: what it runs on is what was asked for, and a field
    // that is always there is one a reader has to inspect to learn nothing.
    let settled = h.get(&name).await;
    assert!(
        settled.body["status"]["pendingChanges"].is_null(),
        "{}",
        settled.body["status"]
    );

    // Now somebody resizes it while it runs.
    let asked = h.patch(&name, json!({ "spec": { "vcpus": 8 } })).await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);

    let after = h.get(&name).await;
    let pending = &after.body["status"]["pendingChanges"];
    assert!(
        !pending.is_null(),
        "the resize reads as applied: {}",
        after.body["status"]
    );
    assert_eq!(pending[0]["field"], json!("vcpus"));
    assert_eq!(pending[0]["from"], json!("2"), "{pending}");
    assert_eq!(pending[0]["to"], json!("8"), "{pending}");

    // Computed, never stored: a third copy of a comparison between a spec and a
    // status could disagree with both, and asking must not be a write.
    let before = h.store.revision().await.unwrap();
    let _ = h.get(&name).await;
    assert_eq!(
        h.store.revision().await.unwrap(),
        before,
        "reading wrote something"
    );

    // And a listing says the same thing a read does — a console that learns
    // about an object from a list must not see a different object.
    let listed = h.get("projects/p1/instances").await;
    let one = &listed.body["items"][0]["status"]["pendingChanges"];
    assert_eq!(one[0]["to"], json!("8"), "{}", listed.body);
}

/// A guest that is not running has nothing to differ from.
#[tokio::test]
async fn a_stopped_guest_has_nothing_pending() {
    let h = Harness::new();
    two_nodes(&h).await;
    let name = running_guest(&h).await;
    h.patch(&name, json!({ "spec": { "vcpus": 8 } })).await;

    // `runningSize` is cleared when a guest stops — the field describes a
    // running machine — so there is nothing to compare and nothing to say.
    let after = h.get(&name).await;
    assert!(
        after.body["status"]["pendingChanges"].is_null(),
        "a stopped guest claimed something was pending: {}",
        after.body["status"]
    );
}

/// A node says what else comes with each device it can pass through.
///
/// Passing one takes its whole IOMMU group — the hardware cannot isolate less —
/// and an operator who learns that afterwards learns it from an outage.
/// `pci::offerable` already stops an unsafe *assignment*; this is the sentence
/// before the decision, which did not exist.
#[tokio::test]
async fn a_nodes_devices_say_what_comes_with_them() {
    use velstra_cloud_model::pci::{DeviceKind, DeviceUse, PciDevice};

    let h = Harness::new();
    two_nodes(&h).await;
    let nodes = h.nodes();
    let mut node = nodes.get("nodes/node-a").await.unwrap().unwrap();
    let device = |address: &str, group: Option<u32>| PciDevice {
        address: address.into(),
        vendor_device: "10de:2204".into(),
        description: "NVIDIA GA102".into(),
        kind: DeviceKind::Gpu,
        iommu_group: group,
        state: DeviceUse::Free,
    };
    node.status.pci_devices = vec![
        // A GPU and its audio function: one group, and taking the card takes
        // both whether anybody wanted the audio device or not.
        device("0000:41:00.0", Some(17)),
        device("0000:41:00.1", Some(17)),
        // A different group entirely.
        device("0000:42:00.0", Some(18)),
        // No IOMMU at all. This is the case a client-side grouping gets
        // backwards: it is not in a group *with* the others, it is in no group
        // and can never be passed through.
        device("0000:43:00.0", None),
    ];
    nodes
        .update(&node, &velstra_cloud_model::access::Writer::agent("node-a"))
        .await
        .unwrap();

    let answer = h.get("nodes/node-a").await;
    let devices = answer.body["status"]["pciDevices"].as_array().unwrap();
    assert_eq!(
        devices[0]["groupWith"],
        json!(["0000:41:00.0", "0000:41:00.1"]),
        "{}",
        answer.body["status"]["pciDevices"]
    );
    // Both members answer the same list, in the same order, so a console does
    // not have to care which one an operator clicked.
    assert_eq!(devices[1]["groupWith"], devices[0]["groupWith"]);
    assert_eq!(devices[2]["groupWith"], json!(["0000:42:00.0"]));
    // The one that matters: alone, not lumped in with every other device that
    // also has no group.
    assert_eq!(devices[3]["groupWith"], json!(["0000:43:00.0"]));

    // Computed, never stored: asking must not be a write.
    let before = h.store.revision().await.unwrap();
    let _ = h.get("nodes/node-a").await;
    assert_eq!(
        h.store.revision().await.unwrap(),
        before,
        "reading wrote something"
    );
}

/// A person creating their first machine should not have to invent an
/// identifier first.
///
/// The API used to refuse a create with no id, and the refusal was defensible —
/// a create without one cannot be retried safely. But it put the platform's
/// naming scheme in front of every first use of it: a console cannot offer
/// "create a machine" without first teaching what a resource id is, and a
/// tenant with no interest in the scheme has to invent one anyway.
///
/// So: minted when absent, readable, and returned. A caller that needs its
/// create to be idempotent still sends the id, which is what every controller
/// here does.
#[tokio::test]
async fn a_create_with_no_id_is_given_a_readable_one() {
    let h = Harness::new();

    let created = h
        .post(
            "projects/p1/instances",
            json!({ "spec": { "vcpus": 1, "memoryMib": 512 } }),
        )
        .await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{:?}", created.body);
    let name = created.body["target"].as_str().unwrap();
    assert!(
        name.starts_with("projects/p1/instances/instance-"),
        "an id nobody chose still has to say what it is: {name}"
    );

    // And it is a real object at that name, not only a string in an answer.
    let read = h.get(name).await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);

    // Two creates are two machines, which is exactly the trade being made here:
    // no id means no idempotency.
    let again = h
        .post(
            "projects/p1/instances",
            json!({ "spec": { "vcpus": 1, "memoryMib": 512 } }),
        )
        .await;
    assert_ne!(again.body["target"].as_str().unwrap(), name);
}

/// An object this build cannot read must still be removable.
///
/// The ordinary delete reads first — for the revision, for the finalizers — so
/// an object whose stored shape no longer deserialises could not be deleted.
/// And because a list deserialises every object it walks, that one object
/// answered 500 for **every** list of its collection, for every caller, until
/// somebody reached past the API into the store. A cell that can be put into a
/// corner it cannot be got out of is the wrong shape.
///
/// Found by putting one there: a spec was written under one field spelling and
/// read back under another, and consoles stopped working cell-wide.
#[tokio::test]
async fn an_object_this_build_cannot_read_can_still_be_taken_away() {
    let h = Harness::new();

    // Straight into the store, because the API is exactly what cannot produce
    // this: a document that is a valid resource envelope and an invalid spec.
    let key = "/cell-1/instances/projects/p1/instances/broken";
    h.store
        .put(
            key,
            serde_json::to_vec(&json!({
                "meta": {
                    "name": "projects/p1/instances/broken",
                    "uid": "u1",
                    "placement": { "region": "eu-central", "cell": "cell-1" },
                    "generation": 1,
                    "createdAt": 1,
                    "deletedAt": null,
                    "finalizers": [],
                    "labels": {}
                },
                "spec": { "vcpus": "not a number" },
                "status": {}
            }))
            .unwrap(),
            velstra_cloud_store::Expect::Absent,
        )
        .await
        .expect("a broken object");

    // The collection still answers. It used not to: one object nobody could
    // read answered 500 for every list of that kind, for every caller — every
    // screen showing them, every agent reading its share, and the reference
    // check a delete runs before it will remove anything.
    let listed = h.get("projects/p1/instances").await;
    assert_eq!(
        listed.status,
        StatusCode::OK,
        "one unreadable object took the collection down: {:?}",
        listed.body
    );
    // The object itself is not in it, which is the cost of the fix and is said
    // out loud in the log rather than hidden: something nothing can read is
    // something nothing will act on.
    let items = listed.body["items"].as_array().expect("a list");
    assert!(
        !items
            .iter()
            .any(|i| i["meta"]["name"] == "projects/p1/instances/broken"),
        "an object that cannot be deserialised was handed to a caller"
    );

    // And it can be taken away, which is the part that used to be impossible:
    // the ordinary delete reads first, so the one thing that would have fixed
    // the cell was also the thing that could not be done.
    let deleted = h
        .send("DELETE", "projects/p1/instances/broken", None, &[])
        .await;
    assert!(
        deleted.status.is_success(),
        "an unreadable object could not be deleted: {:?}",
        deleted.body
    );

    // And it is gone from the store, not merely stamped for deletion: nothing
    // can run a finalizer for an object it cannot read, so there is no second
    // phase to wait for.
    assert!(
        h.store.get(key).await.expect("the store answers").is_none(),
        "the object is still there"
    );
}

/// A port is one guest's NIC, and the API is where that is said.
///
/// Nothing used to say it, and the failure was silent and total: two instances
/// naming one port claim one MAC on one tap, so the node — correctly — answers
/// DHCP for **neither**, and *both* guests come up with no address, no
/// metadata, no user and no SSH key. The node writes one line in its journal
/// and there is nothing on either object to explain it.
///
/// Found on a real machine, by leaving an old test instance behind: a guest
/// that had worked stopped getting an address, and the reason was a second
/// object nobody was looking at.
#[tokio::test]
async fn a_port_is_one_guests_and_the_second_asker_is_told_whose() {
    let h = Harness::new();
    let spec = json!({ "vcpus": 1, "memory_mib": 512, "ports": ["projects/p1/ports/nic0"] });

    let first = h
        .post(
            "projects/p1/instances",
            json!({ "id": "one", "spec": spec }),
        )
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);

    let second = h
        .post(
            "projects/p1/instances",
            json!({ "id": "two", "spec": spec }),
        )
        .await;
    assert_eq!(
        second.status,
        StatusCode::BAD_REQUEST,
        "a second guest was given a port that already had one: {:?}",
        second.body
    );
    assert_eq!(second.error_code(), "FAILED_PRECONDITION");
    let message = second.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("projects/p1/instances/one"),
        "the refusal has to name the guest that already holds it: {message}"
    );
    assert_eq!(second.body["error"]["field"], json!("spec.ports"));

    // Nor by an edit, which is the same failure arriving later.
    let bare = h
        .post(
            "projects/p1/instances",
            json!({ "id": "three", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        )
        .await;
    assert_eq!(bare.status, StatusCode::ACCEPTED, "{:?}", bare.body);
    let stolen = h
        .patch(
            "projects/p1/instances/three",
            json!({ "spec": { "ports": ["projects/p1/ports/nic0"] } }),
        )
        .await;
    assert_eq!(
        stolen.status,
        StatusCode::BAD_REQUEST,
        "a port was moved to a second guest by an edit: {:?}",
        stolen.body
    );

    // And an instance keeping its own port is not a clash with itself.
    let kept = h
        .patch(
            "projects/p1/instances/one",
            json!({ "spec": { "vcpus": 2 } }),
        )
        .await;
    assert_eq!(kept.status, StatusCode::OK, "{:?}", kept.body);
    let same = h
        .patch(
            "projects/p1/instances/one",
            json!({ "spec": { "ports": ["projects/p1/ports/nic0"] } }),
        )
        .await;
    assert_eq!(
        same.status,
        StatusCode::OK,
        "an instance was refused its own port: {:?}",
        same.body
    );
}

/// A console session outlives the ticket it carried, and something has to take
/// it away.
///
/// The ticket is dead after a minute; the object is the record of who opened a
/// console into which guest, which is worth keeping for as long as somebody
/// might ask and not worth keeping for ever. Without a sweep, every click on
/// Console leaves a row behind and a cell that has run a year holds a collection
/// nothing reads.
#[tokio::test]
async fn a_spent_console_session_is_kept_for_a_day_and_then_taken_away() {
    let h = Harness::new();
    // A guest a node has already claimed, written the way a claimed guest
    // exists: the status belongs to whoever runs the object, so it is created
    // with the owner already on it rather than taken over afterwards.
    let name = "projects/p1/instances/i1".to_string();
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut instance = velstra_cloud_model::resources::Instance::new(
        velstra_cloud_model::meta::Meta::new(
            name.parse().unwrap(),
            velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
        ),
        Default::default(),
        velstra_cloud_model::resources::InstanceStatus {
            node: Some("node-a".into()),
            ..Default::default()
        },
    );
    instance.meta.generation = 1;
    instances
        .create(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .expect("a claimed guest");

    let opened = h.post(&format!("{name}:console"), json!({})).await;
    assert_eq!(opened.status, StatusCode::OK, "{:?}", opened.body);
    let session = opened.body["session"]
        .as_str()
        .expect("a session")
        .to_string();
    let expires = opened.body["expiresAt"].as_u64().expect("an expiry");

    // A minute later the ticket is dead and the record is not: somebody asking
    // "who was on that machine" is asking now, not tomorrow.
    let swept = h
        .api
        .sweep_spent_consoles(velstra_cloud_model::meta::Timestamp(expires + 1_000))
        .await
        .expect("the sweep runs");
    assert_eq!(
        swept, 0,
        "a record was taken away while it was still useful"
    );
    assert_eq!(h.get(&session).await.status, StatusCode::OK);

    // A day later it is nobody's business any more.
    let swept = h
        .api
        .sweep_spent_consoles(velstra_cloud_model::meta::Timestamp(
            expires + 25 * 60 * 60 * 1000,
        ))
        .await
        .expect("the sweep runs");
    assert_eq!(swept, 1, "the record outlived its day");
    assert_eq!(h.get(&session).await.status, StatusCode::NOT_FOUND);
}

/// A volume named a pool nothing holds and waited for ever.
///
/// A pool agent watches for volumes naming **its** id. A volume naming
/// something else — `pools/local` instead of `local`, or a pool that was never
/// created — is claimed by nobody: it sits with an empty status and
/// `provisioned: false`, and there is nothing on the object, in any log, or in
/// any answer to say why. It is the quietest way this platform can fail.
///
/// Found live: a volume created with the pool's full resource name sat
/// unprovisioned while an identical one beside it, created with the bare id,
/// worked.
#[tokio::test]
async fn a_volume_naming_a_pool_that_is_not_there_is_refused_while_somebody_is_asking() {
    let h = Harness::new();
    h.pools()
        .create(
            &velstra_cloud_model::resources::Resource::new(
                velstra_cloud_model::meta::Meta::new(
                    "pools/local".parse().unwrap(),
                    velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
                ),
                Default::default(),
                Default::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .expect("a pool");

    // The spelling that used to be accepted and then did nothing for ever.
    let refused = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "v1", "spec": { "sizeGib": 1, "pool": "pools/local" } }),
        )
        .await;
    assert!(
        refused.status.is_client_error(),
        "a volume naming a pool by its resource name was accepted: {:?}",
        refused.body
    );

    // A pool that simply is not there, said with the list that ends the
    // guessing.
    let refused = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "v2", "spec": { "sizeGib": 1, "pool": "nvme" } }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("`local`"),
        "the refusal has to say which pools there are: {message}"
    );

    // And the spelling that works, still works.
    let ok = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "v3", "spec": { "sizeGib": 1, "pool": "local" } }),
        )
        .await;
    assert_eq!(ok.status, StatusCode::ACCEPTED, "{:?}", ok.body);
}

/// A bill nobody can write is the only kind worth having.
///
/// Usage records are readings the platform takes, and the controller writes
/// them straight to the store. If the same door a customer comes in could also
/// create, edit or delete one, the whole point of recording them would be gone:
/// a number somebody can change after the fact is not evidence of anything.
///
/// Found live: the API accepted a POST of a fabricated record with a timestamp
/// of 1.
#[tokio::test]
async fn a_usage_record_cannot_be_written_edited_or_deleted_through_the_api() {
    let h = Harness::new();

    let forged = h
        .post(
            "projects/p1/usage",
            json!({ "id": "0000000000001",
                    "spec": { "project": "projects/p1", "at": 1, "used": {} } }),
        )
        .await;
    assert!(
        forged.status.is_client_error(),
        "a usage record was accepted from a client: {:?}",
        forged.body
    );
    let message = forged.body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("readings"),
        "the refusal has to say what these are: {message}"
    );

    // One the controller wrote, past the API, as it really does.
    let store: TypedStore<
        velstra_cloud_model::usage::UsageRecordSpec,
        velstra_cloud_model::usage::UsageRecordStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "usage");
    let at = velstra_cloud_model::meta::Timestamp(1_787_824_800_000);
    let record = velstra_cloud_model::resources::Resource::new(
        velstra_cloud_model::meta::Meta::new(
            format!(
                "projects/p1/usage/{}",
                velstra_cloud_model::usage::id_for(at)
            )
            .parse()
            .unwrap(),
            velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
        ),
        velstra_cloud_model::usage::UsageRecordSpec {
            project: "projects/p1".into(),
            at,
            used: Default::default(),
        },
        Default::default(),
    );
    store
        .create(
            &record,
            &velstra_cloud_model::access::Writer::controller("quota"),
        )
        .await
        .expect("the controller writes one");

    let name = record.meta.name.to_string();
    // Readable, which is the whole reason it is a resource.
    assert_eq!(h.get(&name).await.status, StatusCode::OK);

    // And not changeable.
    let edited = h
        .patch(&name, json!({ "spec": { "used": { "vcpus": 9999 } } }))
        .await;
    assert!(
        edited.status.is_client_error(),
        "a usage record was edited: {:?}",
        edited.body
    );
    let removed = h.send("DELETE", &name, None, &[]).await;
    assert!(
        removed.status.is_client_error(),
        "a usage record was deleted: {:?}",
        removed.body
    );

    // Still there, unchanged.
    let read = h.get(&name).await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["spec"]["used"]["vcpus"], json!(0));
}

/// A guest a node has already claimed, which is what a console needs: the
/// session names the node, and only the machine with the guest has the socket.
async fn a_claimed_guest(h: &Harness) -> String {
    // A guest a node has already claimed, written the way a claimed guest
    // exists: the status belongs to whoever runs the object, so it is created
    // with the owner already on it rather than taken over afterwards.
    let name = "projects/p1/instances/i1".to_string();
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut instance = velstra_cloud_model::resources::Instance::new(
        velstra_cloud_model::meta::Meta::new(
            name.parse().unwrap(),
            velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
        ),
        Default::default(),
        velstra_cloud_model::resources::InstanceStatus {
            node: Some("node-a".into()),
            ..Default::default()
        },
    );
    instance.meta.generation = 1;
    instances
        .create(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .expect("a claimed guest");
    name
}

#[tokio::test]
async fn a_console_stream_is_opened_with_the_ticket_because_a_browser_has_no_header() {
    // The bug this pins shipped whole and passed every test, because every test
    // client sends the header a browser cannot: `new WebSocket(url)` takes a URL
    // and nothing else. The grant succeeded, the stream came back "HTTP
    // Authentication failed; no valid credentials available", and the console
    // was unusable from the only place it is ever used.
    let h = Harness::new();
    let name = a_claimed_guest(&h).await;

    let opened = h.post(&format!("{name}:console"), json!({})).await;
    assert_eq!(opened.status, StatusCode::OK, "{:?}", opened.body);
    let session = opened.body["session"].as_str().expect("a session");
    let ticket = opened.body["ticket"].as_str().expect("a ticket");

    let path = format!(
        "{name}:consoleStream?session={}&ticket={}",
        urlencode(session),
        urlencode(ticket)
    );
    let answered = h.send_bare(&path).await;
    assert_ne!(
        answered.status,
        StatusCode::UNAUTHORIZED,
        "the ticket was not accepted as the credential it is: {:?}",
        answered.body
    );
    // It gets as far as the handler and is turned away there for the one reason
    // a test client cannot avoid: this is not a real upgrade. That is the proof
    // it passed the door.
    assert_eq!(
        answered.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        answered.body
    );
    assert!(
        answered.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("upgrade"),
        "{:?}",
        answered.body
    );

    // A guessed ticket is still nobody.
    let guessed = format!(
        "{name}:consoleStream?session={}&ticket=00000000-0000-0000-0000-000000000000",
        urlencode(session)
    );
    assert_eq!(
        h.send_bare(&guessed).await.status,
        StatusCode::UNAUTHORIZED,
        "a guessed ticket opened a console"
    );

    // A ticket for one guest does not open another. This is the check that
    // replaced "can this person read it": the person may well be able to, and
    // the ticket still may not, because it was minted against one machine.
    let elsewhere_guest = a_second_claimed_guest(&h).await;
    let wrong_guest = format!(
        "{elsewhere_guest}:consoleStream?session={}&ticket={}",
        urlencode(session),
        urlencode(ticket)
    );
    assert_eq!(
        h.send_bare(&wrong_guest).await.status,
        StatusCode::FORBIDDEN,
        "a ticket opened a guest it was not minted for"
    );

    // And the exemption is exactly this one verb: nothing else opens without a
    // header just because it carries a ticket in its query.
    let elsewhere = format!(
        "{name}?session={}&ticket={}",
        urlencode(session),
        urlencode(ticket)
    );
    assert_eq!(
        h.send_bare(&elsewhere).await.status,
        StatusCode::UNAUTHORIZED,
        "a ticket in the query opened an ordinary read"
    );
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A second claimed guest, for the question "does this ticket open that one".
async fn a_second_claimed_guest(h: &Harness) -> String {
    let name = "projects/p1/instances/i2".to_string();
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "instances");
    let mut instance = velstra_cloud_model::resources::Instance::new(
        velstra_cloud_model::meta::Meta::new(
            name.parse().unwrap(),
            velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
        ),
        Default::default(),
        velstra_cloud_model::resources::InstanceStatus {
            node: Some("node-a".into()),
            ..Default::default()
        },
    );
    instance.meta.generation = 1;
    instances
        .create(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .expect("a second claimed guest");
    name
}

#[tokio::test]
async fn an_image_is_called_what_a_person_would_call_it() {
    // Two failures, in opposite directions, one week apart on the same cell.
    //
    // First an image published as `images/debian13` was accepted and its guest
    // failed at boot — "carries no sha256 digest in its name, so this node
    // cannot verify it" — because the node parsed the digest out of the name.
    //
    // Then the fix that made the name *be* the digest worked and was worse:
    // every list an operator reads offered
    // `sha256-d2af37c5246b899b63ed999281b927e2f241ee0340d26d2a74558b7636136d76`
    // as the thing to pick an operating system from.
    //
    // The name is for people. The digest is in the spec, where the node reads
    // it, and it addresses the bytes on disk.
    let h = Harness::new();
    let digest = "sha256:d2af37c5246b899b63ed999281b927e2f241ee0340d26d2a74558b7636136d76";
    let spec = json!({
        "digest": digest,
        "format": "Qcow2",
        "sourceUrl": "http://example.invalid/d.qcow2",
        "family": "debian-13"
    });

    let named = h
        .post("images", json!({ "id": "debian-13", "spec": spec }))
        .await;
    assert_eq!(named.status, StatusCode::ACCEPTED, "{:?}", named.body);
    assert_eq!(named.body["target"], json!("images/debian-13"));

    // And with no id at all, something a person can say: the family and enough
    // digest to tell two builds apart.
    let minted = h.post("images", json!({ "spec": spec })).await;
    assert_eq!(minted.status, StatusCode::ACCEPTED, "{:?}", minted.body);
    assert_eq!(minted.body["target"], json!("images/debian-13-d2af37c5"));
}

#[tokio::test]
async fn a_port_no_guest_uses_is_not_waiting_for_anybody() {
    // Found by signing in to a real cell: the first two entries on the attention
    // list were a port nobody was using and the operation that had created it.
    // Nothing programs a port until a guest that names it runs on a node, so a
    // free port sits at `observedGeneration: 0` with no conditions for ever —
    // and every reader that treats that as "waiting" is permanently wrong about
    // it. It is the same argument a security group already makes about itself:
    // an alarm about nothing is the kind that teaches people to ignore the real
    // one.
    let h = Harness::new();
    let made = h
        .post(
            "projects/p1/ports",
            json!({ "id": "spare", "spec": {
                "network": "projects/p1/networks/n1",
                "subnet": "projects/p1/subnets/s1",
                "securityGroups": []
            }}),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);

    let port = h.get("projects/p1/ports/spare").await;
    assert_eq!(port.status, StatusCode::OK, "{:?}", port.body);
    let ready = port.body["status"]["conditions"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["kind"] == "Ready"))
        .cloned()
        .expect("a port says whether it is waiting");
    assert_eq!(ready["status"], json!("True"), "{ready}");
    assert_eq!(ready["reason"], json!("Unused"), "{ready}");
    // And it has been seen by everybody who is ever going to see it, which is
    // what stops the console reading it as "nothing has looked at this".
    assert_eq!(
        port.body["status"]["observedGeneration"], port.body["meta"]["generation"],
        "{:?}",
        port.body["status"]
    );
}

#[tokio::test]
async fn a_volume_with_no_pool_named_is_put_somewhere_rather_than_nowhere() {
    // Found by walking the platform as a customer. A tenant cannot list pools —
    // they are the cell's own — yet the volume form required naming one: a form
    // no customer could fill in. And leaving it empty was worse than either
    // answer, because an empty pool slipped past the wrong-pool guard, matched
    // no pool agent's filter, and the volume sat unprovisioned for ever with an
    // empty status. The quietest failure this platform has, reachable by
    // leaving a field blank.
    let h = Harness::new();
    let writer = writer();
    // Two accepting pools with different room, and one that is not accepting.
    for (id, free, accepting) in [
        ("small", 50u64, true),
        ("roomy", 500, true),
        ("closed", 900, false),
    ] {
        let mut pool = velstra_cloud_model::resources::Pool::new(
            velstra_cloud_model::meta::Meta::new(
                format!("pools/{id}").parse().unwrap(),
                velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::resources::PoolSpec {
                accepting,
                labels: Vec::new(),
            },
            velstra_cloud_model::resources::PoolStatus {
                capacity_gib: 1000,
                allocated_gib: 1000 - free,
                ..Default::default()
            },
        );
        pool.meta.generation = 1;
        h.pools().create(&pool, &writer).await.unwrap();
    }

    let made = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "auto", "spec": { "sizeGib": 5 } }),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);

    let stored = h.get("projects/p1/volumes/auto").await;
    // The accepting pool with the most room, written down — never the closed
    // one, however large, and never the empty string that provisions nothing.
    assert_eq!(
        stored.body["spec"]["pool"],
        json!("roomy"),
        "{:?}",
        stored.body["spec"]
    );
}

#[tokio::test]
async fn a_network_needs_no_number_from_the_person_asking_for_one() {
    // A tenant clicking "new network" was asked for a **VXLAN network
    // identifier**: a number whose only correct value is "one nothing else in
    // this cell uses", which a tenant cannot know and has no business knowing.
    // The model's own comment already said it is "assigned by the controller
    // from the cell's range, never chosen by a tenant" — the form asked anyway.
    let h = Harness::new();
    let first = h
        .post("projects/p1/networks", json!({ "id": "a", "spec": {} }))
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    let second = h
        .post("projects/p1/networks", json!({ "id": "b", "spec": {} }))
        .await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{:?}", second.body);

    let a = h.get("projects/p1/networks/a").await;
    let b = h.get("projects/p1/networks/b").await;
    let (va, vb) = (
        a.body["spec"]["vni"].as_u64().unwrap(),
        b.body["spec"]["vni"].as_u64().unwrap(),
    );
    assert!(va >= 5000 && vb >= 5000, "{va} {vb}");
    assert_ne!(va, vb, "two networks were given the same VNI");
    // And an MTU that fits inside a VXLAN header rather than the wire's own,
    // which black-holes every large packet in a way that reads as an
    // application bug for a week.
    assert_eq!(a.body["spec"]["mtu"], json!(1450), "{:?}", a.body["spec"]);

    // An operator who does want a specific number still gets it.
    let pinned = h
        .post(
            "projects/p1/networks",
            json!({ "id": "c", "spec": { "vni": 9001, "mtu": 9000 } }),
        )
        .await;
    assert_eq!(pinned.status, StatusCode::ACCEPTED, "{:?}", pinned.body);
    let c = h.get("projects/p1/networks/c").await;
    assert_eq!(c.body["spec"]["vni"], json!(9001));
    assert_eq!(c.body["spec"]["mtu"], json!(9000));
}

#[tokio::test]
async fn one_machine_is_one_request() {
    // The largest gap between this and a platform somebody would buy. A customer
    // who wanted one machine had to create a network, then a subnet on it, then
    // a port on that, in that order, and only then the guest — four objects and
    // a dependency order, none of which they asked about, and each of which
    // asked for something like a VXLAN identifier.
    let h = Harness::new();
    let made = h
        .post(
            "projects/p2/instances",
            json!({ "id": "erste", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2
            }}),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);

    let guest = h.get("projects/p2/instances/erste").await;
    let ports = guest.body["spec"]["ports"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        ports.len(),
        1,
        "a guest was created with no wire: {:?}",
        guest.body["spec"]
    );

    // On this project's own default network, made for it — with an address
    // range nobody typed.
    let net = h.get("projects/p2/networks/default").await;
    assert_eq!(net.status, StatusCode::OK, "{:?}", net.body);
    let subnet = h.get("projects/p2/subnets/default").await;
    assert_eq!(subnet.status, StatusCode::OK, "{:?}", subnet.body);
    assert!(
        subnet.body["spec"]["cidr"]
            .as_str()
            .unwrap_or_default()
            .starts_with("10."),
        "{:?}",
        subnet.body["spec"]
    );

    // A second guest joins the first one's network rather than getting another,
    // which is what lets two machines in a project talk without anybody
    // configuring anything.
    let second = h
        .post(
            "projects/p2/instances",
            json!({ "id": "zweite", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2
            }}),
        )
        .await;
    assert_eq!(second.status, StatusCode::ACCEPTED, "{:?}", second.body);
    let nets = h.get("projects/p2/networks").await;
    assert_eq!(
        nets.body["items"].as_array().map(Vec::len),
        Some(1),
        "a second guest made a second network"
    );

    // And a guest that genuinely wants none says so.
    let alone = h
        .post(
            "projects/p2/instances",
            json!({ "id": "allein", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2,
                // Saying nothing and saying "none" are different requests.
                "ports": []
            }}),
        )
        .await;
    assert_eq!(alone.status, StatusCode::ACCEPTED, "{:?}", alone.body);
    let g = h.get("projects/p2/instances/allein").await;
    assert!(
        g.body["spec"]["ports"]
            .as_array()
            .is_none_or(|p| p.is_empty()),
        "{:?}",
        g.body["spec"]
    );
}

/// Naming a subnet is naming a network more precisely, and with two subnets it
/// is the only complete answer.
///
/// A network with one subnet is what most people mean by "put it on my network".
/// With two, the network no longer says which range the address comes out of —
/// and this used to take whichever came first by name, silently. That is the
/// same thing the create refuses when both `networks` and `ports` are named:
/// picking one of two answers on somebody's behalf.
#[tokio::test]
async fn a_guest_is_put_on_a_network_or_on_one_of_its_subnets() {
    let h = Harness::new();
    h.post(
        "projects/p2/networks",
        json!({ "id": "twin", "spec": { "mtu": 1450 }}),
    )
    .await;
    for (id, cidr) in [("front", "10.60.0.0/24"), ("back", "10.61.0.0/24")] {
        let made = h
            .post(
                "projects/p2/subnets",
                json!({ "id": id, "spec": {
                    "network": "projects/p2/networks/twin",
                    "cidr": cidr, "gateway": cidr.replace(".0/24", ".1"),
                    "dns": [], "reserved": []
                }}),
            )
            .await;
        assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);
    }

    // The network alone is no longer an answer, and the refusal lists what to
    // choose from rather than saying only that it cannot.
    let vague = h
        .post(
            "projects/p2/instances",
            json!({ "id": "vage", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2,
                "networks": ["projects/p2/networks/twin"]
            }}),
        )
        .await;
    assert_eq!(vague.status, StatusCode::BAD_REQUEST, "{:?}", vague.body);
    let why = vague.body["error"]["message"].as_str().unwrap_or_default();
    assert!(why.contains("more than one subnet"), "{why}");
    assert!(why.contains("front") && why.contains("back"), "{why}");
    assert!(
        why.contains("10.60.0.0/24"),
        "the ranges are not named: {why}"
    );
    assert_eq!(vague.body["error"]["field"], "spec.networks");

    // The subnet is. Its network is the subnet's own — nobody names both.
    let precise = h
        .post(
            "projects/p2/instances",
            json!({ "id": "genau", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2,
                "networks": ["projects/p2/subnets/back"]
            }}),
        )
        .await;
    assert_eq!(precise.status, StatusCode::ACCEPTED, "{:?}", precise.body);
    let guest = h.get("projects/p2/instances/genau").await;
    let port = guest.body["spec"]["ports"][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!port.is_empty(), "{:?}", guest.body["spec"]);
    let p = h.get(&port).await;
    assert_eq!(p.body["spec"]["subnet"], "projects/p2/subnets/back");
    assert_eq!(p.body["spec"]["network"], "projects/p2/networks/twin");

    // And a subnet nobody made is refused by name, not by silence.
    let ghost = h
        .post(
            "projects/p2/instances",
            json!({ "id": "geist", "spec": {
                "image": "projects/p1/images/sha256-abc",
                "vcpus": 1, "memoryMib": 512, "rootDiskGib": 2,
                "networks": ["projects/p2/subnets/gibtsnicht"]
            }}),
        )
        .await;
    assert_eq!(ghost.status, StatusCode::BAD_REQUEST, "{:?}", ghost.body);
    assert!(
        ghost.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no subnet called"),
        "{:?}",
        ghost.body
    );
}

/// A backup asked for the way a customer asks: no target named.
///
/// A target is the cell's infrastructure, invisible to a tenant by design —
/// so requiring its name made tenant backups impossible by construction: the
/// form's picker was empty and the refusal named an object they may not list.
/// Left empty, the cell's most roomy accepting target answers.
#[tokio::test]
async fn a_backup_without_a_target_goes_to_the_cells_most_roomy_one() {
    let h = Harness::new();
    h.pool("nvme").await;
    // Two targets, reported by their agent: the roomier one wins.
    for (id, free) in [("small", 5u64), ("big", 500u64)] {
        h.post(
            "backup-targets",
            json!({ "id": id, "spec": { "kind": "directory", "path": format!("/srv/{id}"), "accepting": true, "agent": "nvme" } }),
        )
        .await;
        let mut t = h
            .backup_targets()
            .get(&format!("backup-targets/{id}"))
            .await
            .unwrap()
            .unwrap();
        t.status.writable = Some(true);
        t.status.free_gib = free;
        h.backup_targets()
            .update(&t, &velstra_cloud_model::access::Writer::agent("nvme"))
            .await
            .unwrap();
    }
    let vol = h
        .post(
            "projects/p1/volumes",
            json!({ "id": "data", "spec": { "pool": "nvme", "sizeGib": 1 } }),
        )
        .await;
    assert_eq!(vol.status, StatusCode::ACCEPTED, "{:?}", vol.body);

    let made = h
        .post(
            "projects/p1/backups",
            json!({ "id": "b1", "spec": { "volume": "projects/p1/volumes/data" } }),
        )
        .await;
    assert_eq!(made.status, StatusCode::ACCEPTED, "{:?}", made.body);
    let stored = h.get("projects/p1/backups/b1").await;
    assert_eq!(
        stored.body["spec"]["target"], "backup-targets/big",
        "{:?}",
        stored.body["spec"]
    );

    // A cell with nowhere to put a copy says so, with what an operator must do.
    let h2 = Harness::new();
    h2.pool("nvme").await;
    h2.post(
        "projects/p1/volumes",
        json!({ "id": "data", "spec": { "pool": "nvme", "sizeGib": 1 } }),
    )
    .await;
    let refused = h2
        .post(
            "projects/p1/backups",
            json!({ "id": "b1", "spec": { "volume": "projects/p1/volumes/data" } }),
        )
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("nowhere in this cell takes backups"),
        "{:?}",
        refused.body
    );
}

#[test]
fn a_full_store_is_a_precondition_and_not_an_internal_error() {
    // Found live. An afternoon of ordinary use filled etcd's 2 GiB default and
    // every write in the cell answered
    //
    //     INTERNAL: the store refused: grpc request error: code: 'Some resource
    //     has been exhausted', message: "etcdserver: mvcc: database space
    //     exceeded"
    //
    // An operator reading that has been told the platform broke. It had not:
    // etcd keeps every revision until somebody compacts, and the answer is two
    // commands — which the sentence now carries, because the moment somebody
    // needs them is the moment they cannot use the console to look them up.
    let refusal: velstra_cloud_api::ApiError = velstra_cloud_store::StoreError::Backend(
        "grpc request error: code: 'Some resource has been exhausted', message: \
         \"etcdserver: mvcc: database space exceeded\""
            .to_string(),
    )
    .into();
    assert_eq!(refusal.code, velstra_cloud_api::Code::FailedPrecondition);
    let said = refusal.to_string();
    assert!(said.contains("etcdctl defrag"), "{said}");
    assert!(said.contains("compacted"), "{said}");

    // Anything else is still what it was: a backend that answered something
    // nobody anticipated is an internal error, and dressing it up as advice
    // would be worse than saying so.
    let other: velstra_cloud_api::ApiError =
        velstra_cloud_store::StoreError::Backend("connection reset".into()).into();
    assert_eq!(other.code, velstra_cloud_api::Code::Internal);
}

#[tokio::test]
async fn a_value_the_field_does_not_take_says_so_about_the_value() {
    // Found live, following the console's own advice. The Overview said
    // "Setting x86-64-v1 on horst, peter would put them in one domain", and
    //
    //   PATCH /nodes/horst { "spec": { "cpuBaseline": "V1" } }
    //
    // answered "there is no field called cpu_baseline on a nodes; nothing would
    // have been done with it". The field exists. `V1` is simply not how a level
    // is spelled — `x86-64-v1` is. Somebody reading that refusal goes looking
    // for a missing field, finds it in the model, and concludes the API is
    // broken.
    let h = Harness::new();
    two_nodes(&h).await;

    let refused = h
        .patch("nodes/node-a", json!({ "spec": { "cpuBaseline": "V1" }}))
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.body
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("does not take that value"),
        "the refusal still blames the field: {message}"
    );
    // The wire's own spelling. `error.field` names a wire path — a console
    // lands the refusal on the control whose key it matches — so it leaves the
    // API camelCase like every other field name on this surface, whatever the
    // model calls it inside.
    assert_eq!(refused.body["error"]["field"], json!("spec.cpuBaseline"));

    // The spelling the platform itself uses is accepted.
    let ok = h
        .patch(
            "nodes/node-a",
            json!({ "spec": { "cpuBaseline": "x86-64-v2" }}),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);

    // And a field that really is not there still says that, which is the
    // distinction this is about.
    let missing = h
        .patch("nodes/node-a", json!({ "spec": { "gibtEsNicht": 3 }}))
        .await;
    let message = missing.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("no field called"),
        "an unknown field stopped saying so: {message}"
    );
}
