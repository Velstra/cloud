//! Per-node identity — the trust boundary a node token is, end to end.
//!
//! Before this, every node agent wrote to the store with a self-declared writer
//! identity, holding the operator's own token: a compromised node could write or
//! delete any object of any tenant. This exercises the closed version through the
//! API surface a node actually uses in `--api` mode: a node is minted a token at
//! registration, and that token may write the status of its own objects and
//! nothing else — enforced in one place, `access::judge`, reached through the
//! same `store.update` a direct report uses.

use std::sync::Arc;

use serde_json::{Value, json};
use velstra_cloud_api::{
    Api, Code, Identity, StaticTokenVerifier, TokenVerifier,
    sessions::{IdentityStore, StoreTokenVerifier},
};
use velstra_cloud_model::meta::ResourceName;
use velstra_cloud_store::{MemoryStore, Store};

const OPERATOR: &str = "ops";

fn name(text: &str) -> ResourceName {
    ResourceName::parse(text).unwrap()
}

/// A cell whose verifier recognises node tokens, plus a handle to drive it.
///
/// The verifier wraps the *same* store the API mints credentials into, so a
/// token minted by a registration is one this verifier can then verify — which
/// is exactly the wiring a real process has.
fn cell() -> (Api, Arc<dyn Store>) {
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
    (api, store)
}

fn who(subject: &str) -> Identity {
    Identity::new(subject)
}

/// Register a node as the operator and return the token it is handed once.
async fn register_node(api: &Api, id: &str) -> String {
    let created = api
        .create(
            "",
            "nodes",
            &json!({ "id": id, "spec": { "schedulable": true } }),
            &who(OPERATOR),
        )
        .await
        .expect("an operator registers a node");
    created
        .node_token
        .expect("registering a node mints a token")
}

/// The identity a node token authenticates as, through the real verifier.
async fn agent(api: &Api, token: &str) -> Identity {
    api.verifier()
        .verify(token)
        .await
        .expect("a freshly minted node token verifies")
}

/// The status of an object, read back the way an agent reads it before it writes.
async fn status_of(api: &Api, n: &ResourceName) -> Value {
    api.get(n, &who(OPERATOR)).await.unwrap()["status"].clone()
}

#[tokio::test]
async fn a_node_token_is_minted_once_and_verifies_as_its_node() {
    let (api, _store) = cell();
    let token = register_node(&api, "node-a").await;
    assert_eq!(token.len(), 64, "a node token is 256 bits of hex");

    let id = agent(&api, &token).await;
    // It authenticates as the node, and as nothing else — not an operator, not a
    // project subject.
    assert!(!api.is_operator(&id), "a node token is not a cell operator");
    assert_eq!(
        velstra_cloud_api::sessions::agent_node(&id),
        Some("node-a"),
        "the identity carries the node it is the agent for"
    );

    // A made-up token is refused, and a real node's own token is not recoverable
    // from the cell — only its digest was stored.
    assert!(api.verifier().verify("not-a-real-token").await.is_err());
}

#[tokio::test]
async fn a_node_writes_the_status_of_its_own_node_and_instances() {
    let (api, _store) = cell();
    let token = register_node(&api, "node-a").await;
    let node_a = agent(&api, &token).await;

    // Its own node object: a heartbeat is a status write, and the node owns its
    // own status (a hypervisor is its own owner).
    let mut node_status = status_of(&api, &name("nodes/node-a")).await;
    node_status["agent_version"] = json!("1.2.3");
    let written = api
        .report_status(
            &name("nodes/node-a"),
            &json!({ "status": node_status }),
            None,
            &node_a,
        )
        .await
        .expect("a node writes its own node status");
    assert_eq!(written["status"]["agent_version"], json!("1.2.3"));

    // An instance the operator assigned to node-a: the node claims it and reports
    // it running. Ownership comes from the assignment (spec.node) on the first
    // report, exactly as it does against the store directly.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "node": "node-a", "vcpus": 1, "memory_mib": 512 } }),
        &who(OPERATOR),
    )
    .await
    .expect("the operator creates an instance assigned to node-a");

    let mut istatus = status_of(&api, &name("projects/p1/instances/i1")).await;
    istatus["node"] = json!("node-a");
    istatus["state"] = json!("Running");
    let written = api
        .report_status(
            &name("projects/p1/instances/i1"),
            &json!({ "status": istatus }),
            None,
            &node_a,
        )
        .await
        .expect("the node reports the status of an instance assigned to it");
    assert_eq!(written["status"]["node"], json!("node-a"));
    assert_eq!(written["status"]["state"], json!("Running"));
}

#[tokio::test]
async fn a_node_cannot_write_another_nodes_objects() {
    let (api, _store) = cell();
    let token_a = register_node(&api, "node-a").await;
    register_node(&api, "node-b").await;
    let node_a = agent(&api, &token_a).await;

    // node-a tries to write node-b's node status: refused, as a permission
    // failure and not an invalid argument, with the refusal naming the confusion.
    let mut bstatus = status_of(&api, &name("nodes/node-b")).await;
    bstatus["agent_version"] = json!("hijacked");
    let err = api
        .report_status(
            &name("nodes/node-b"),
            &json!({ "status": bstatus }),
            None,
            &node_a,
        )
        .await
        .expect_err("node-a wrote node-b's status");
    assert_eq!(err.code, Code::PermissionDenied, "{}", err.message);

    // An instance assigned to node-b is likewise not node-a's to speak for.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "onb", "spec": { "node": "node-b", "vcpus": 1, "memory_mib": 512 } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    let mut istatus = status_of(&api, &name("projects/p1/instances/onb")).await;
    istatus["node"] = json!("node-a");
    let err = api
        .report_status(
            &name("projects/p1/instances/onb"),
            &json!({ "status": istatus }),
            None,
            &node_a,
        )
        .await
        .expect_err("node-a claimed an instance assigned to node-b");
    assert_eq!(err.code, Code::PermissionDenied, "{}", err.message);
}

#[tokio::test]
async fn a_node_cannot_touch_spec_create_or_delete() {
    let (api, _store) = cell();
    let token = register_node(&api, "node-a").await;
    let node_a = agent(&api, &token).await;
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "node": "node-a", "vcpus": 1, "memory_mib": 512 } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();

    // Spec, through the client path: a node holds no binding in the project, so
    // it may not PATCH at all.
    let patched = api
        .patch(
            &name("projects/p1/instances/i1"),
            &json!({ "spec": { "vcpus": 8 } }),
            None,
            &node_a,
        )
        .await;
    assert!(patched.is_err(), "a node changed an instance's spec");

    // Spec, through the status path: a report that tries to smuggle a spec
    // changes nothing, because the stored spec is kept whatever the agent sends.
    let mut istatus = status_of(&api, &name("projects/p1/instances/i1")).await;
    istatus["node"] = json!("node-a");
    api.report_status(
        &name("projects/p1/instances/i1"),
        &json!({ "status": istatus, "spec": { "vcpus": 99 } }),
        None,
        &node_a,
    )
    .await
    .expect("the status write itself is fine");
    let after = api
        .get(&name("projects/p1/instances/i1"), &who(OPERATOR))
        .await
        .unwrap();
    assert_eq!(
        after["spec"]["vcpus"],
        json!(1),
        "a node changed spec through the status path"
    );

    // Create: a node may not bring an object into being — it holds no operator
    // authority and no project binding.
    let created = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "rogue", "spec": { "vcpus": 1 } }),
            &node_a,
        )
        .await;
    assert!(created.is_err(), "a node created an object");

    // Delete: likewise refused — a node is not an operator, and deletion is a
    // cell/controller decision, not a report about a machine.
    let deleted = api.delete(&name("nodes/node-a"), None, &node_a).await;
    assert!(deleted.is_err(), "a node deleted an object");
}

#[tokio::test]
async fn a_person_or_operator_may_not_report_status() {
    let (api, _store) = cell();
    register_node(&api, "node-a").await;
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "node": "node-a", "vcpus": 1, "memory_mib": 512 } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();

    // Status is the agent's half. Even a cell operator — who may do everything
    // else here — is not a node agent, and the status path refuses a caller with
    // no agent scope by construction.
    let mut istatus = status_of(&api, &name("projects/p1/instances/i1")).await;
    istatus["state"] = json!("Running");
    let err = api
        .report_status(
            &name("projects/p1/instances/i1"),
            &json!({ "status": istatus }),
            None,
            &who(OPERATOR),
        )
        .await
        .expect_err("an operator wrote status through the agent path");
    assert_eq!(err.code, Code::PermissionDenied, "{}", err.message);
}

#[tokio::test]
async fn a_deleted_node_loses_its_token() {
    let (api, _store) = cell();
    let token = register_node(&api, "node-a").await;
    assert!(api.verifier().verify(&token).await.is_ok());

    // Delete the node as the operator. Its credential goes with it, so the token
    // stops authenticating — a token outliving the node it speaks for is a
    // credential for an object that no longer exists.
    api.delete(&name("nodes/node-a"), None, &who(OPERATOR))
        .await
        .expect("the operator deletes the node");
    assert!(
        api.verifier().verify(&token).await.is_err(),
        "a deleted node's token still authenticates"
    );
}
