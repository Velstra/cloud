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
}

struct Answer {
    status: StatusCode,
    body: Value,
    etag: Option<String>,
    revision_header: Option<String>,
}

impl Answer {
    fn error_code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    fn field(&self) -> &str {
        self.body["error"]["field"].as_str().unwrap_or("")
    }
}

impl Harness {
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
            router: velstra_cloud_api::server(api),
            store,
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

    fn nodes(&self) -> TypedStore<NodeSpec, NodeStatus> {
        TypedStore::new(self.store.clone(), "cell-1", "nodes")
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
            schedulable: true,
            labels: vec![],
        },
        NodeStatus {
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
    h.nodes().create(&node).await.unwrap();

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
        h.post("projects/p1/instances", json!({ "spec": {} })).await,
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
                schedulable: true,
                labels: vec![],
            },
            NodeStatus {
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
                    vec!["projects/p1/images/sha256-abc".into()]
                } else {
                    vec![]
                },
                ..Default::default()
            },
        );
        velstra_cloud_model::meta::set_condition(&mut node.status.conditions, Condition::ready(1));
        h.nodes().create(&node).await.unwrap();
    }
    // A small node nothing will fit on, so a refusal has numbers behind it.
    let mut small = Resource::new(
        Meta::new(
            ResourceName::parse("nodes/node-tiny").unwrap(),
            Placement::new("eu-central", "cell-1"),
        ),
        NodeSpec {
            schedulable: true,
            labels: vec![],
        },
        NodeStatus {
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
    h.nodes().create(&small).await.unwrap();
}

/// An instance running on `node-a`, with its image cached on both real nodes.
async fn running_guest(h: &Harness) -> String {
    let image: TypedStore<
        velstra_cloud_model::resources::ImageSpec,
        velstra_cloud_model::resources::ImageStatus,
    > = TypedStore::new(h.store.clone(), "cell-1", "images");
    image
        .create(&Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/images/sha256-abc").unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::resources::ImageSpec {
                digest: "sha256:abc".into(),
                ..Default::default()
            },
            velstra_cloud_model::resources::ImageStatus::default(),
        ))
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
            schedulable: true,
            labels: vec![],
        },
        NodeStatus {
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
    h.nodes().create(&bare).await.unwrap();

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

#[tokio::test]
async fn where_a_volume_came_from_is_history_rather_than_a_control() {
    let h = Harness::new();
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
            schedulable: true,
            labels: vec![],
        },
        NodeStatus {
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
    h.nodes().create(&node).await.unwrap();

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
