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
