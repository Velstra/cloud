//! Seeing, expiring and taking away a machine's way into the cell.
//!
//! `:issueCredential` is additive on purpose — an operator who mistypes the new
//! token into a file must not thereby take the agent down — but that made
//! rotation a one-way street: three issues meant three live credentials, and
//! nothing could list them, name them or remove one. The only revocation was
//! deleting the machine.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::{identity::AgentKind, meta::ResourceName};
use velstra_cloud_store::{MemoryStore, Store};

const OPS: &str = "ops";

fn who() -> Identity {
    Identity::new(OPS)
}

async fn cell() -> Api {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    Api::new(store, "eu-central", "cell-1", verifier).with_cell_admins(vec![OPS.to_string()])
}

/// Registering hands back a token that works, and an operator can see that it
/// exists without being able to read it back.
#[tokio::test]
async fn a_registered_machine_has_a_credential_that_can_be_listed() {
    let api = cell().await;
    let made = api
        .create("", "nodes", &json!({"id": "hv-1", "spec": {}}), &who())
        .await
        .unwrap();
    let token = made.node_token.expect("registering a node mints a token");

    let me = api.identity().identify_node(&token).await.unwrap();
    assert_eq!(me.subject, "node:hv-1");

    let held = api.identity().agent_credentials_for("hv-1").await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].spec.kind, AgentKind::Node);
    // The row is named by the token's digest, which is not the token.
    assert_ne!(held[0].meta.name.id(), token);
}

/// Rotation, as an operator actually does it: issue the second, put it on the
/// machine, then take the first away. There is no window where the machine has
/// no way in.
#[tokio::test]
async fn a_credential_can_be_taken_away_without_touching_the_other() {
    let api = cell().await;
    let first = api
        .create("", "nodes", &json!({"id": "hv-1", "spec": {}}), &who())
        .await
        .unwrap()
        .node_token
        .unwrap();
    let second = api
        .identity()
        .mint_agent_credential_for("hv-1", AgentKind::Node, None, "rotation")
        .await
        .unwrap();

    let held = api.identity().agent_credentials_for("hv-1").await.unwrap();
    assert_eq!(held.len(), 2, "issuing did not add a second credential");
    let old = held
        .iter()
        .find(|c| c.spec.purpose.is_empty())
        .expect("the original")
        .meta
        .name
        .id()
        .to_string();

    api.identity()
        .revoke_agent_credential("hv-1", &old)
        .await
        .unwrap();

    assert!(
        api.identity().identify_node(&first).await.is_err(),
        "a revoked credential still opened the door"
    );
    assert_eq!(
        api.identity().identify_node(&second).await.unwrap().subject,
        "node:hv-1",
        "revoking one credential took the other with it"
    );
}

/// A credential may not be revoked by naming another machine's row.
#[tokio::test]
async fn one_machines_credential_is_not_anothers_to_revoke() {
    let api = cell().await;
    for id in ["hv-1", "hv-2"] {
        api.create("", "nodes", &json!({"id": id, "spec": {}}), &who())
            .await
            .unwrap();
    }
    let theirs = api.identity().agent_credentials_for("hv-2").await.unwrap()[0]
        .meta
        .name
        .id()
        .to_string();
    assert!(
        api.identity()
            .revoke_agent_credential("hv-1", &theirs)
            .await
            .is_err(),
        "one machine's credential was revoked by naming another"
    );
    assert_eq!(
        api.identity()
            .agent_credentials_for("hv-2")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// An expiry, when one was asked for. Off by default, because an agent reads
/// its token once at startup and cannot fetch another — an expiry that arrived
/// on its own would take a machine down at a moment nobody chose.
#[tokio::test]
async fn a_credential_with_an_end_date_stops_working_at_it() {
    let api = cell().await;
    api.create("", "nodes", &json!({"id": "hv-1", "spec": {}}), &who())
        .await
        .unwrap();
    let ending = api
        .identity()
        .mint_agent_credential_for(
            "hv-1",
            AgentKind::Node,
            Some(velstra_cloud_model::meta::Timestamp(1)),
            "for the contractor",
        )
        .await
        .unwrap();
    assert!(
        api.identity().identify_node(&ending).await.is_err(),
        "a credential past its end date was accepted"
    );
    // And it is not left lying in the store: a credential nobody can use is a
    // row nothing would ever come back for.
    let held = api.identity().agent_credentials_for("hv-1").await.unwrap();
    assert_eq!(held.len(), 1, "the expired row was kept");
    assert!(held[0].spec.expires_at.is_none());
}

/// Deleting a pool takes its credential with it. It did not: the branch named
/// only nodes, so a deleted pool left a working way into the cell for ever,
/// under the name of a machine that was gone.
#[tokio::test]
async fn deleting_a_pool_forgets_its_credential() {
    let api = cell().await;
    let token = api
        .create("", "pools", &json!({"id": "pool-a", "spec": {}}), &who())
        .await
        .unwrap()
        .pool_token
        .expect("registering a pool mints a token");
    assert_eq!(
        api.identity().identify_node(&token).await.unwrap().subject,
        "pool:pool-a"
    );

    api.delete(&ResourceName::parse("pools/pool-a").unwrap(), None, &who())
        .await
        .unwrap();

    assert!(
        api.identity().identify_node(&token).await.is_err(),
        "a deleted pool's agent token still opened the door"
    );
}
