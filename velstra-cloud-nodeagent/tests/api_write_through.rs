//! `--api` mode reports through the API as the node's own token — proven against
//! a real API, over real HTTP.
//!
//! The agent's other tests drive the direct-store path. This one exercises the
//! seam that makes `--api` a trust boundary: a node registered through the API is
//! handed a token, reads its own share with it, and writes status back through it
//! — and a write for another node's object is refused by the API, not by the
//! agent trusting itself. Everything the agent does here goes over HTTP to a
//! server built from the real API crate.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{
    Api, Identity, StaticTokenVerifier, TokenVerifier,
    sessions::{IdentityStore, StoreTokenVerifier},
};
use velstra_cloud_model::{access::Writer, resources::InstanceState};
use velstra_cloud_nodeagent::{
    api_cell::ApiCell,
    cell::CellReader,
    sink::{SinkOutcome, StatusSink},
};
use velstra_cloud_store::{MemoryStore, Store};

const OPERATOR: &str = "ops";

/// Bring up a real API on a loopback port, register a node, and hand back the
/// base URL, the node's token, and the operator's own handle for setup.
async fn api_with_a_node(node: &str) -> (String, String, Api) {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let identity = IdentityStore::new(store.clone(), "eu-central", "cell-1");
    let operator: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([(
        "optok".to_string(),
        Identity::new(OPERATOR),
    )]));
    let verifier: Arc<dyn TokenVerifier> =
        Arc::new(StoreTokenVerifier::new(identity).with_fallback(operator));
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec![OPERATOR.to_string()]);

    let created = api
        .create(
            "",
            "nodes",
            &json!({ "id": node, "spec": { "schedulable": true } }),
            &Identity::new(OPERATOR),
        )
        .await
        .expect("the operator registers a node");
    let token = created
        .node_token
        .expect("a node registration mints a token");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = api.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, velstra_cloud_api::server(served)).await;
    });
    (format!("http://{addr}"), token, api)
}

/// Create an instance assigned to `node`, as the operator.
async fn assign_instance(api: &Api, node: &str, id: &str) {
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": id, "spec": { "node": node, "vcpus": 1, "memory_mib": 512 } }),
        &Identity::new(OPERATOR),
    )
    .await
    .expect("the operator creates an instance");
}

#[tokio::test]
async fn a_node_reads_its_share_and_reports_status_through_the_api() {
    let (base, token, api) = api_with_a_node("node-a").await;
    assign_instance(&api, "node-a", "i1").await;

    let client = ApiCell::for_node(&base, &token, "node-a").expect("an api client");

    // The node reads its own share over HTTP, with its own token — the read half
    // of `--api` mode, which a per-node identity has to be allowed or the agent
    // sees an empty cell and does nothing.
    let instances = client
        .instances()
        .await
        .expect("the node reads its instances");
    let i1 = instances
        .iter()
        .find(|i| i.meta.name.id() == "i1")
        .cloned()
        .expect("the node was handed the instance assigned to it");

    // It claims the instance and reports it running — the write half, back
    // through the API as its own token.
    let mut next = i1.clone();
    next.status.node = Some("node-a".into());
    next.status.state = InstanceState::Running;
    next.status.observed_generation = i1.meta.generation;
    let value = serde_json::to_value(&next).unwrap();
    let outcome = client
        .write_status("instances", &value, &Writer::agent("node-a"))
        .await;
    assert!(
        matches!(outcome, SinkOutcome::Wrote),
        "the node could not report its instance's status: {outcome:?}"
    );

    // Read it back through the API: the status landed.
    let after = client.instances().await.unwrap();
    let i1 = after.iter().find(|i| i.meta.name.id() == "i1").unwrap();
    assert_eq!(i1.status.node.as_deref(), Some("node-a"));
    assert_eq!(i1.status.state, InstanceState::Running);

    // An unchanged re-report writes nothing new and is still accepted — the same
    // quiet a converged agent has against the store.
    let value = serde_json::to_value(i1).unwrap();
    let outcome = client
        .write_status("instances", &value, &Writer::agent("node-a"))
        .await;
    assert!(
        matches!(outcome, SinkOutcome::Wrote | SinkOutcome::Conflict),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_node_reporting_another_nodes_object_is_refused_by_the_api() {
    let (base, token_a, api) = api_with_a_node("node-a").await;
    // A second node and an instance assigned to it, both set up by the operator.
    api.create(
        "",
        "nodes",
        &json!({ "id": "node-b", "spec": { "schedulable": true } }),
        &Identity::new(OPERATOR),
    )
    .await
    .unwrap();
    assign_instance(&api, "node-b", "onb").await;

    // node-a reads node-b's instance (reads are cell-wide for an agent) and tries
    // to claim it. The API refuses the *write* as an ownership violation — the
    // node cannot write what is not its own, and finds out from the far end
    // rather than by trusting itself.
    let client = ApiCell::for_node(&base, &token_a, "node-a").expect("an api client");
    let onb: velstra_cloud_model::resources::Instance = api
        .get(
            &velstra_cloud_model::meta::ResourceName::parse("projects/p1/instances/onb").unwrap(),
            &Identity::new(OPERATOR),
        )
        .await
        .map(|v| serde_json::from_value(v).unwrap())
        .unwrap();

    let mut next = onb.clone();
    next.status.node = Some("node-a".into());
    let value = serde_json::to_value(&next).unwrap();
    let outcome = client
        .write_status("instances", &value, &Writer::agent("node-a"))
        .await;
    assert!(
        matches!(outcome, SinkOutcome::Refused(_)),
        "node-a wrote node-b's object through the API: {outcome:?}"
    );
}

/// A pool agent reads its own object over HTTP, and its own volumes.
///
/// The half that had no test at all, and it showed twice on real machines: the
/// pool object was read off a store handle that in `--api` mode is a
/// placeholder, so it came back "no such pool"; and once that was fixed, the
/// read was built as a path where a name belongs and the API answered `400 empty
/// segment in "/api/v1/pools"`. Neither is visible to a test whose agent talks
/// to a store — which is every other pool test in this crate.
#[tokio::test]
async fn a_pool_reads_its_own_object_and_its_share_through_the_api() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let identity = IdentityStore::new(store.clone(), "eu-central", "cell-1");
    let operator: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([(
        "optok".to_string(),
        Identity::new(OPERATOR),
    )]));
    let verifier: Arc<dyn TokenVerifier> =
        Arc::new(StoreTokenVerifier::new(identity).with_fallback(operator));
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec![OPERATOR.to_string()]);

    let created = api
        .create(
            "",
            "pools",
            &json!({ "id": "nvme", "spec": { "accepting": true } }),
            &Identity::new(OPERATOR),
        )
        .await
        .expect("the operator registers a pool");
    let token = created
        .pool_token
        .expect("a pool registration mints a token");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = api.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, velstra_cloud_api::server(served)).await;
    });
    let base = format!("http://{addr}");

    let client = ApiCell::for_pool(&base, &token, "nvme").expect("an api client");

    // Its own object, by name. This is the read that decides whether the pool
    // reports at all: without it the pass returns early on the rule that a pool
    // nobody registered is not an agent's to invent.
    let mine = velstra_cloud_nodeagent::cell::PoolReader::pool(&client, "nvme")
        .await
        .expect("the pool reads its own object")
        .expect("the pool it was told it is");
    assert_eq!(mine.meta.name.id(), "nvme");

    // And it can say something about it, as itself.
    let mut next = mine.clone();
    next.status.backend = "lvm".into();
    next.status.capacity_gib = 500;
    next.status.observed_generation = mine.meta.generation;
    let value = serde_json::to_value(&next).unwrap();
    let outcome = client
        .write_status("pools", &value, &Writer::agent("nvme"))
        .await;
    assert!(
        matches!(outcome, SinkOutcome::Wrote),
        "the pool could not report its own status: {outcome:?}"
    );

    let after = api
        .get(
            &velstra_cloud_model::meta::ResourceName::parse("pools/nvme").unwrap(),
            &Identity::new(OPERATOR),
        )
        .await
        .unwrap();
    // `api.get` answers below the wire layer, so the fields are the model's own
    // spelling — `capacity_gib`, not `capacityGib`.
    assert_eq!(after["status"]["backend"], "lvm");
    assert_eq!(after["status"]["capacity_gib"], 500);
}

/// The source of a migration reads the instance where its answers live.
///
/// It read it off a store handle, which under `--api` is a placeholder that
/// answers "no such object" — and on the source that reads as "this node has
/// already let go", the moment a migration exists and before the guest has
/// moved anywhere. Letting go is what moves the assignment, so the destination
/// would claim a guest that is still running here.
///
/// Not found on a machine, and that is the point: it is the same defect as the
/// pool's own object, in the one place where being wrong costs a second copy of
/// a live guest.
#[tokio::test]
async fn a_node_reads_a_moving_instance_through_the_api_and_not_a_placeholder() {
    let (base, token, api) = api_with_a_node("node-a").await;
    assign_instance(&api, "node-a", "moving").await;

    let client = ApiCell::for_node(&base, &token, "node-a").expect("an api client");
    let seen = velstra_cloud_nodeagent::cell::CellReader::instance(
        &client,
        "projects/p1/instances/moving",
    )
    .await
    .expect("the node reads the instance being moved");
    let seen = seen.expect("the instance it was assigned");
    assert_eq!(seen.meta.name.id(), "moving");
    assert_eq!(seen.spec.node.as_deref(), Some("node-a"));

    // And a name nobody registered is `None`, not an error: an agent starting
    // before its objects exist is a normal order of events.
    assert!(
        velstra_cloud_nodeagent::cell::CellReader::instance(
            &client,
            "projects/p1/instances/never-made",
        )
        .await
        .expect("a missing instance is an answer")
        .is_none()
    );
}
