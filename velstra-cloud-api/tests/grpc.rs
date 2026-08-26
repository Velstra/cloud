//! The two transports are one API.
//!
//! These tests do the same thing twice — once over REST, once over gRPC — and
//! insist on the same answer. That is the whole reason the handlers live in one
//! place, and it is worth nothing unless something checks it: a rule that holds
//! on one surface and not the other is discovered by a customer whose SDK
//! writes what a console refuses.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request as HttpRequest, StatusCode, header},
};
use futures::StreamExt;
use serde_json::{Value, json};
use tonic::Request;
use tower::ServiceExt;
use velstra_cloud_api::{Api, StaticTokenVerifier, TokenVerifier, grpc::Service};
use velstra_cloud_proto::v1::{
    self, CreateInstanceRequest, DeleteRequest, GetRequest, ListRequest, UpdateInstanceRequest,
    WatchRequest, compute_server::Compute, storage_server::Storage,
};
use velstra_cloud_store::{MemoryStore, Store};

const TOKEN: &str = "development-token";

struct Both {
    rest: Router,
    grpc: Service,
}

impl Both {
    fn new() -> Self {
        let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single(TOKEN));
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        // The harness acts as a cell operator: it registers nodes and creates
        // projects, which are the cell's and not any tenant's. Authorisation is
        // exercised on purpose in `tests/authz.rs` rather than incidentally
        // here.
        let api =
            Api::new(store, "eu-central", "cell-1", verifier).with_cell_admins(vec!["dev".into()]);
        Self {
            rest: velstra_cloud_api::rest::router(api.clone()),
            grpc: Service::new(api),
        }
    }

    async fn http(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = HttpRequest::builder()
            .method(method)
            .uri(format!("/api/v1/{path}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json");
        let request = match body {
            Some(body) => request.body(Body::from(body.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.rest.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

/// A call that carries the same bearer token the REST gateway takes.
fn signed<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

fn instance(name: &str, vcpus: u32) -> v1::Instance {
    v1::Instance {
        meta: Some(v1::Meta {
            name: name.to_string(),
            ..Default::default()
        }),
        spec: Some(v1::InstanceSpec {
            start_order: 0,
            start_delay_s: 0,
            on_node_loss: Default::default(),
            console: false,
            devices: Vec::new(),
            vcpus,
            memory_mib: 2048,
            ..Default::default()
        }),
        status: None,
    }
}

async fn create(both: &Both, id: &str, vcpus: u32) -> v1::Operation {
    both.grpc
        .create_instance(signed(CreateInstanceRequest {
            parent: "projects/p1".into(),
            instance_id: id.into(),
            instance: Some(instance("", vcpus)),
        }))
        .await
        .expect("the create was refused")
        .into_inner()
}

#[tokio::test]
async fn an_object_created_over_grpc_is_the_object_rest_reads() {
    let both = Both::new();
    let operation = create(&both, "i1", 2).await;
    assert_eq!(
        operation.spec.as_ref().unwrap().target,
        "projects/p1/instances/i1"
    );
    assert_eq!(operation.spec.as_ref().unwrap().verb, "create");
    assert!(
        !operation.status.as_ref().unwrap().done,
        "done before anything reported"
    );

    let (status, body) = both.http("GET", "projects/p1/instances/i1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["spec"]["vcpus"], json!(2));

    let over_grpc = both
        .grpc
        .get_instance(signed(GetRequest {
            name: "projects/p1/instances/i1".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let meta = over_grpc.meta.as_ref().unwrap();
    assert_eq!(meta.name, body["meta"]["name"].as_str().unwrap());
    assert_eq!(
        meta.generation,
        body["meta"]["generation"].as_u64().unwrap()
    );
    assert_eq!(
        meta.revision,
        body["meta"]["revision"].as_str().unwrap(),
        "the two transports disagree about the revision a client would send back"
    );
}

#[tokio::test]
async fn a_change_over_one_transport_is_visible_over_the_other() {
    let both = Both::new();
    create(&both, "i1", 2).await;

    let updated = both
        .grpc
        .update_instance(signed(UpdateInstanceRequest {
            instance: Some(instance("projects/p1/instances/i1", 8)),
            revision: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(updated.spec.as_ref().unwrap().vcpus, 8);
    assert_eq!(
        updated.meta.as_ref().unwrap().generation,
        2,
        "the generation did not move"
    );

    let (_, body) = both.http("GET", "projects/p1/instances/i1", None).await;
    assert_eq!(body["spec"]["vcpus"], json!(8));

    // …and back the other way, including the rule that an identical change
    // moves nothing.
    both.http(
        "PATCH",
        "projects/p1/instances/i1",
        Some(json!({ "spec": { "vcpus": 8 } })),
    )
    .await;
    let unchanged = both
        .grpc
        .get_instance(signed(GetRequest {
            name: "projects/p1/instances/i1".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(unchanged.meta.as_ref().unwrap().generation, 2);
}

#[tokio::test]
async fn both_transports_refuse_a_status_write() {
    let both = Both::new();
    create(&both, "i1", 2).await;

    let mut sent = instance("projects/p1/instances/i1", 2);
    sent.status = Some(v1::InstanceStatus {
        state: v1::InstanceState::Running as i32,
        ..Default::default()
    });
    let refused = both
        .grpc
        .update_instance(signed(UpdateInstanceRequest {
            instance: Some(sent),
            revision: String::new(),
        }))
        .await
        .expect_err("gRPC accepted a status an agent had not reported");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(
        refused.message().contains("status"),
        "the refusal did not name the half"
    );

    let (status, body) = both
        .http(
            "PATCH",
            "projects/p1/instances/i1",
            Some(json!({ "status": { "state": "Running" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"]["code"],
        json!("INVALID_ARGUMENT"),
        "the twins disagree"
    );
}

#[tokio::test]
async fn a_grpc_update_is_conditional_on_a_revision_the_way_if_match_is() {
    let both = Both::new();
    create(&both, "i1", 2).await;
    let read = both
        .grpc
        .get_instance(signed(GetRequest {
            name: "projects/p1/instances/i1".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let stale = read.meta.as_ref().unwrap().revision.clone();

    both.http(
        "PATCH",
        "projects/p1/instances/i1",
        Some(json!({ "spec": { "vcpus": 4 } })),
    )
    .await;

    let refused = both
        .grpc
        .update_instance(signed(UpdateInstanceRequest {
            instance: Some(instance("projects/p1/instances/i1", 16)),
            revision: stale,
        }))
        .await
        .expect_err("a stale write won");
    assert_eq!(refused.code(), tonic::Code::Aborted);
}

#[tokio::test]
async fn a_grpc_list_says_where_to_watch_from_and_the_watch_catches_up() {
    let both = Both::new();
    create(&both, "i1", 2).await;
    let listed = both
        .grpc
        .list_instances(signed(ListRequest {
            parent: "projects/p1".into(),
            node: String::new(),
            pool: String::new(),
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.instances.len(), 1);
    assert!(
        !listed.revision.is_empty(),
        "the list did not say where it ended"
    );

    create(&both, "i2", 2).await;

    let mut events = both
        .grpc
        .watch_instances(signed(WatchRequest {
            parent: "projects/p1".into(),
            from_revision: listed.revision,
            node: String::new(),
            pool: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    let event = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await
        .expect("the watch delivered nothing")
        .expect("the stream ended")
        .unwrap();
    assert_eq!(event.r#type, v1::EventType::Put as i32);
    assert_eq!(event.name, "projects/p1/instances/i2");
    assert_eq!(
        event.resource.unwrap().spec.unwrap().vcpus,
        2,
        "the event did not carry the object"
    );
}

#[tokio::test]
async fn a_delete_over_grpc_is_the_same_two_phases() {
    let both = Both::new();
    create(&both, "i1", 2).await;
    let deleted = both
        .grpc
        .delete_instance(signed(DeleteRequest {
            name: "projects/p1/instances/i1".into(),
            revision: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(
        deleted.meta.as_ref().unwrap().deleted_at.is_some(),
        "the answer did not carry the stamp that makes a delete visible"
    );

    let (status, _) = both.http("GET", "projects/p1/instances/i1", None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "nothing held it, so it should be gone"
    );
}

#[tokio::test]
async fn explain_placement_reads_the_same_over_both() {
    let both = Both::new();
    create(&both, "i1", 2).await;

    let (_, rest) = both
        .http("GET", "projects/p1/instances/i1:explainPlacement", None)
        .await;
    let grpc = both
        .grpc
        .explain_placement(signed(GetRequest {
            name: "projects/p1/instances/i1".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(grpc.placed.is_none(), rest["placed"].is_null());
    assert_eq!(
        grpc.rejected.len(),
        rest["rejected"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn an_unsigned_grpc_call_gets_nowhere_either() {
    let both = Both::new();
    let refused = both
        .grpc
        .get_instance(Request::new(GetRequest {
            name: "projects/p1/instances/i1".into(),
        }))
        .await
        .expect_err("an unauthenticated call was served");
    assert_eq!(refused.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn a_derived_field_is_derived_on_both_transports() {
    // The node an attachment takes from its instance is decided in the
    // handler, not in the REST skin — an SDK that had to fill it in itself
    // would be the one place the invariant could still be broken.
    let both = Both::new();
    let mut placed = instance("", 2);
    placed.spec.as_mut().unwrap().node = Some("node-a".into());
    both.grpc
        .create_instance(signed(CreateInstanceRequest {
            parent: "projects/p1".into(),
            instance_id: "i1".into(),
            instance: Some(placed),
        }))
        .await
        .expect("the instance was refused");

    both.grpc
        .create_attachment(signed(v1::CreateAttachmentRequest {
            parent: "projects/p1".into(),
            attachment_id: "a1".into(),
            attachment: Some(v1::Attachment {
                meta: None,
                spec: Some(v1::AttachmentSpec {
                    volume: "projects/p1/volumes/v1".into(),
                    instance: "projects/p1/instances/i1".into(),
                    node: String::new(),
                    read_only: false,
                }),
                status: None,
            }),
        }))
        .await
        .expect("the attachment was refused");

    let (_, body) = both.http("GET", "projects/p1/attachments/a1", None).await;
    assert_eq!(body["spec"]["node"], json!("node-a"));
}

/// gRPC `ListOperations` used to hand every operation in the cell to any
/// accepted token.
///
/// It called `who()` for the side effect of authenticating and then threw the
/// identity away, reaching for the API's unfiltered read. An operation names the
/// object it acted on, so what a tenant got back was every other tenant's
/// machines, volumes and networks and what was recently done to each. The REST
/// surface for the same collection has always gone through the authorised path,
/// which is precisely the two-surfaces-one-API asymmetry this file exists to
/// catch — and did not, because nothing here had ever listed operations as
/// somebody who is not an operator.
#[tokio::test]
async fn grpc_list_operations_shows_a_tenant_only_their_own() {
    use velstra_cloud_api::Identity;
    use velstra_cloud_proto::v1::operations_server::Operations;

    const OPERATOR_TOKEN: &str = "operator-token";
    const ADA_TOKEN: &str = "ada-token";

    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([
        (OPERATOR_TOKEN.to_string(), Identity::new("operator")),
        (ADA_TOKEN.to_string(), Identity::new("ada")),
    ]));
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api =
        Api::new(store, "eu-central", "cell-1", verifier).with_cell_admins(vec!["operator".into()]);

    // Two projects; only p1 is Ada's.
    for (project, admin) in [("p1", "ada"), ("p2", "bob")] {
        api.create(
            "",
            "projects",
            &json!({"id": project, "spec": {
                "quota": {},
                "bindings": [{"role": "admin", "members": [admin]}]
            }}),
            &Identity::new("operator"),
        )
        .await
        .unwrap();
        // A create mints an operation, which is how operations come to exist.
        api.create(
            &format!("projects/{project}"),
            "networks",
            &json!({"id": format!("n-{admin}"), "spec": {"vni": 5001, "mtu": 1500}}),
            &Identity::new(admin),
        )
        .await
        .unwrap();
    }

    let grpc = Service::new(api);
    let as_ada = |message: ListRequest| {
        let mut request = Request::new(message);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {ADA_TOKEN}").parse().unwrap(),
        );
        request
    };

    let listed = grpc
        .list_operations(as_ada(ListRequest {
            parent: String::new(),
            node: String::new(),
            pool: String::new(),
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .expect("a tenant may list operations")
        .into_inner();

    let names: Vec<String> = listed
        .operations
        .iter()
        .filter_map(|o| o.meta.as_ref().map(|m| m.name.clone()))
        .collect();
    assert!(
        !names.is_empty(),
        "the tenant was shown nothing at all, so this proves nothing"
    );
    assert!(
        names.iter().all(|n| !n.contains("/p2/")),
        "gRPC handed a tenant another tenant's operations: {names:?}"
    );
}
