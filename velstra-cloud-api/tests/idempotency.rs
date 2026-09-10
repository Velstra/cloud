//! Retrying a create without making a second one.
//!
//! The gap these close: a create that lets the platform pick the name has no
//! handle a retry can hold. A client whose request timed out could not tell
//! whether it had landed, so it either retried and made two guests or gave up
//! and lost the one it asked for. An idempotency key gives the retry the first
//! attempt's answer instead.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_store::{MemoryStore, Store};

const OPS: &str = "ops";

fn who() -> Identity {
    Identity::new(OPS)
}

/// A project with a network and a subnet, so an instance can be created into
/// it without naming a port.
async fn cell() -> Api {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api =
        Api::new(store, "eu-central", "cell-1", verifier).with_cell_admins(vec![OPS.to_string()]);
    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who(),
    )
    .await
    .unwrap();
    // Somewhere to put a volume: without a pool every create in this file is
    // refused before it ever reaches the key.
    api.create(
        "",
        "pools",
        &json!({"id": "pool-a", "spec": {"accepting": true}}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "subnets",
        &json!({"id": "s1", "spec": {
            "network": "projects/p1/networks/n1",
            "cidr": "10.20.0.0/24",
            "gateway": "10.20.0.1",
            "dns": [],
            "reserved": []
        }}),
        &who(),
    )
    .await
    .unwrap();
    api
}

fn a_volume() -> serde_json::Value {
    json!({"spec": {"size_gib": 1}})
}

async fn volumes(api: &Api) -> usize {
    api.list("projects/p1", "volumes")
        .await
        .unwrap()
        .items
        .len()
}

/// The whole point: two identical creates under one key make one object, and
/// the second answer is the first one.
#[tokio::test]
async fn the_same_key_twice_makes_one_object() {
    let api = cell().await;
    let (first, replayed) = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-1"))
        .await
        .unwrap();
    assert!(!replayed, "the first attempt was called a replay");

    let (second, replayed) = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-1"))
        .await
        .unwrap();
    assert!(replayed, "the retry was not recognised as one");
    assert_eq!(
        first.target, second.target,
        "the retry was answered with a different object"
    );
    assert_eq!(
        first.operation, second.operation,
        "the retry was given an operation the first attempt never saw"
    );
    assert_eq!(volumes(&api).await, 1, "the retry made a second volume");
}

/// Without a key the old behaviour stands, and two creates are two objects.
/// This is the control: if it ever fails, the key stopped being opt-in.
#[tokio::test]
async fn without_a_key_two_creates_are_two_objects() {
    let api = cell().await;
    api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), None)
        .await
        .unwrap();
    api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), None)
        .await
        .unwrap();
    assert_eq!(volumes(&api).await, 2);
}

/// A key reused for a different request is refused rather than answered with
/// somebody else's object — the failure mode that would otherwise turn a
/// client's loop bug into one object and forty-nine successful-looking lies.
#[tokio::test]
async fn a_key_reused_for_a_different_request_is_refused() {
    let api = cell().await;
    api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-2"))
        .await
        .unwrap();
    let Err(refused) = api
        .create_with_key(
            "projects/p1",
            "volumes",
            &json!({"spec": {"size_gib": 9}}),
            &who(),
            Some("k-2"),
        )
        .await
    else {
        panic!("a key was spent twice on two different creates");
    };
    assert!(
        refused.to_string().contains("different request"),
        "the refusal does not say why: {refused}"
    );
    assert_eq!(volumes(&api).await, 1);
}

/// Field order is not part of the request. A client that serialises its map
/// differently on the retry is still retrying.
#[tokio::test]
async fn the_same_request_written_differently_is_still_a_replay() {
    let api = cell().await;
    api.create_with_key(
        "projects/p1",
        "volumes",
        &json!({"spec": {"size_gib": 2, "pool": ""}}),
        &who(),
        Some("k-3"),
    )
    .await
    .unwrap();
    let (_, replayed) = api
        .create_with_key(
            "projects/p1",
            "volumes",
            &json!({"spec": {"pool": "", "size_gib": 2}}),
            &who(),
            Some("k-3"),
        )
        .await
        .unwrap();
    assert!(replayed, "the same request in another order was redone");
    assert_eq!(volumes(&api).await, 1);
}

/// Two tenants who happened to pick the same string do not share an answer.
#[tokio::test]
async fn one_callers_key_is_not_anothers() {
    let api = cell().await;
    let other = Identity::new("someone-else");
    api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-4"))
        .await
        .unwrap();
    // Refused for want of permission rather than answered with the first
    // caller's object — which is the point: the record was never consulted.
    let refused = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &other, Some("k-4"))
        .await;
    assert!(
        refused.is_err(),
        "another caller's key answered with this caller's object"
    );
}

/// A create that failed does not spend the key: the retry that fixes the
/// request must be able to use it.
#[tokio::test]
async fn a_refused_create_leaves_the_key_unspent() {
    let api = cell().await;
    let bad = json!({"spec": {"size_gib": 1, "pool": "pools/nothing-here"}});
    assert!(
        api.create_with_key("projects/p1", "volumes", &bad, &who(), Some("k-5"))
            .await
            .is_err(),
        "a volume in a pool this cell does not have was accepted"
    );
    let (_, replayed) = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-5"))
        .await
        .unwrap();
    assert!(!replayed, "the corrected retry was answered from a record");
    assert_eq!(volumes(&api).await, 1);
}

/// Registering a machine is refused rather than replayed: the answer carries a
/// credential shown once, and a replay could only hand back a record missing
/// the one field the caller needed.
#[tokio::test]
async fn a_create_that_mints_a_credential_refuses_the_key() {
    let api = cell().await;
    let Err(refused) = api
        .create_with_key(
            "",
            "nodes",
            &json!({"id": "hv-1", "spec": {}}),
            &who(),
            Some("k-6"),
        )
        .await
    else {
        panic!("registering a node accepted an idempotency key");
    };
    assert!(
        refused.to_string().contains("shown once"),
        "the refusal does not say why: {refused}"
    );
}

/// A key that could not be one is refused at the door, and nothing is created.
#[tokio::test]
async fn an_unusable_key_is_refused() {
    let api = cell().await;
    assert!(
        api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("  "))
            .await
            .is_err(),
        "an empty key was honoured"
    );
    assert!(
        api.create_with_key(
            "projects/p1",
            "volumes",
            &a_volume(),
            &who(),
            Some(&"x".repeat(300))
        )
        .await
        .is_err(),
        "a key long enough to be storage was honoured"
    );
    assert_eq!(volumes(&api).await, 0);
}

/// A key nobody comes back for is not a row the store keeps for ever.
#[tokio::test]
async fn a_spent_key_is_swept_once_it_is_old() {
    use velstra_cloud_model::meta::Timestamp;

    let api = cell().await;
    api.create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-7"))
        .await
        .unwrap();

    // Fresh: nothing to sweep, and the record still answers a retry.
    assert_eq!(
        api.sweep_spent_idempotency_keys(Timestamp::now())
            .await
            .unwrap(),
        0,
        "a key spent a moment ago was swept"
    );
    let (_, replayed) = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-7"))
        .await
        .unwrap();
    assert!(replayed);

    // A day and a half later it is gone, and the same key starts again.
    let later =
        Timestamp(Timestamp::now().0 + velstra_cloud_model::idempotency::KEY_LIFETIME_MS + 60_000);
    assert_eq!(
        api.sweep_spent_idempotency_keys(later).await.unwrap(),
        1,
        "an expired key was kept"
    );
    let (_, replayed) = api
        .create_with_key("projects/p1", "volumes", &a_volume(), &who(), Some("k-7"))
        .await
        .unwrap();
    assert!(!replayed, "an expired record still answered a create");
    // Two: the original, and the one the expired key no longer stood for. The
    // replay between them made nothing, which is the whole point.
    assert_eq!(volumes(&api).await, 2);
}
