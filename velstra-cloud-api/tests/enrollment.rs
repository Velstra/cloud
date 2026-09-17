//! A machine announces itself, an operator approves it, the machine collects
//! its credential — and every way of getting that wrong is refused.
//!
//! The model's own tests cover the rules. This covers the wiring, which is
//! where the faults in this feature's neighbours all were: a token written to
//! one directory and read from another, five keys rendered and never parsed
//! back, a control-plane seed its own parser refused, an API answering on its
//! own loopback behind a closed firewall. Each of those passed every unit test
//! on both sides.

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use base64::Engine;
use ring::signature::KeyPair;
use serde_json::{Value, json};
use tower::ServiceExt;
use velstra_cloud_api::{Api, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_store::MemoryStore;

const TOKEN: &str = "development-token";

fn api() -> axum::Router {
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([(
        TOKEN.to_string(),
        velstra_cloud_api::Identity::new("ada"),
    )]));
    let api = Api::new(
        Arc::new(MemoryStore::new()),
        "eu-central",
        "cell-1",
        verifier,
    )
    .with_cell_admins(vec!["ada".into()])
    // A join token is only mintable when the API knows what to advertise, and
    // the whole point of a claim is that it answers with one.
    .with_join_facts(
        vec!["https://cell-1:8443".into()],
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
    );
    velstra_cloud_api::server(api)
}

/// With a token, the way an operator's console calls.
async fn send(router: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    call(router, method, path, body, Some(TOKEN)).await
}

/// Without one, the way a machine that has just booted an installer calls.
async fn anon(router: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    call(router, method, path, body, None).await
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/v1/{path}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn keypair() -> (ring::signature::Ed25519KeyPair, String) {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let public = base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref());
    (pair, public)
}

fn sign_claim(pair: &ring::signature::Ed25519KeyPair, id: &str) -> String {
    let message = velstra_cloud_model::enrollment::claim_message(id);
    base64::engine::general_purpose::STANDARD.encode(pair.sign(&message).as_ref())
}

fn announcement(public: &str) -> Value {
    json!({
        "publicKey": public,
        "seenCertificate": "9F:2C:11:22:33:44:55:66",
        "reported": {
            "hostname": "nixos",
            "addresses": ["10.10.10.47"],
            "vcpus": 16,
            "memoryMib": 65536,
            "disks": ["nvme0n1"],
            "serial": "PT-0042",
        },
    })
}

/// The whole way through, in the order it happens on a real machine.
#[tokio::test]
async fn a_machine_announces_is_approved_and_collects_its_credential() {
    let router = api();
    let (pair, public) = keypair();

    // The machine, holding nothing.
    let (status, announced) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{announced}");
    let id = announced["id"].as_str().expect("an id").to_string();
    let fingerprint = announced["fingerprint"].as_str().expect("a fingerprint");
    assert_eq!(
        fingerprint,
        velstra_cloud_model::enrollment::fingerprint_of(&public),
        "the machine is told the fingerprint it will print on its screen"
    );
    assert_eq!(announced["phase"], json!("Pending"));
    // Nothing secret comes back: the machine has no credential yet, and this
    // answer goes over a certificate nobody has verified.
    let text = announced.to_string();
    assert!(!text.contains("Token"), "{text}");

    // It cannot claim yet, and is told what to do about it rather than "no".
    let (status, refused) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        json!("FAILED_PRECONDITION"),
        "{refused}"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("fingerprint"), "{message}");

    // The operator sees it, with what the machine said about itself.
    let (status, list) = send(&router, "GET", "enrollments", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = &list["items"][0];
    assert_eq!(row["status"]["fingerprint"], json!(fingerprint));
    assert_eq!(row["status"]["reported"]["serial"], json!("PT-0042"));
    assert_eq!(
        row["status"]["seenCertificate"],
        json!("9F:2C:11:22:33:44:55:66")
    );

    // They compare the fingerprint, name it, say what it is for, approve.
    let (status, approved) = send(
        &router,
        "PATCH",
        &format!("enrollments/{id}"),
        json!({ "spec": { "node": "peter", "roles": ["hypervisor"], "approved": true } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");

    // Who approved it is recorded by the API, in the status, which nothing a
    // caller sends can set. This is the authority the registration happens
    // under.
    let (_, after) = send(&router, "GET", &format!("enrollments/{id}"), Value::Null).await;
    assert_eq!(after["status"]["approvedBy"], json!("ada"), "{after}");

    // The machine polls again and gets its credential.
    let (status, issued) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    assert!(
        issued["nodeToken"].as_str().is_some_and(|t| t.len() == 64),
        "{issued}"
    );
    assert!(
        issued["joinToken"]
            .as_str()
            .is_some_and(|t| t.starts_with("velstra1.")),
        "the claim answers with the whole hand-off: {issued}"
    );

    // And the node exists, which is what the machine will report as.
    let (status, node) = send(&router, "GET", "nodes/peter", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{node}");

    // Minted once. A second claim is a retry that lost its answer, or somebody
    // else holding the key; neither gets a second credential.
    let (_status, again) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(
        again["error"]["code"],
        json!("FAILED_PRECONDITION"),
        "{again}"
    );
}

/// Announcing twice is one row. A machine reboots, or loses its answer, and
/// polls — and an operator must not end up looking at two pending machines
/// that are one machine.
#[tokio::test]
async fn a_machine_that_announces_again_lands_on_its_own_row() {
    let router = api();
    let (_, public) = keypair();
    let (_, first) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    let (_, second) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    assert_eq!(first["id"], second["id"]);
    let (_, list) = send(&router, "GET", "enrollments", Value::Null).await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1), "{list}");

    // And a differently padded spelling of the same key is the same key.
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&public)
        .unwrap();
    let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
    let (_, third) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&url_safe),
    )
    .await;
    assert_eq!(first["id"], third["id"], "one key, one row");
    let (_, list) = send(&router, "GET", "enrollments", Value::Null).await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1), "{list}");
}

/// Somebody who read an id off a screen, and holds no key, gets nothing — and
/// learns nothing about whether that machine was approved.
#[tokio::test]
async fn a_claim_without_the_key_is_indistinguishable_from_an_unknown_id() {
    let router = api();
    let (_, public) = keypair();
    let (other, _) = keypair();
    let (_, announced) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    let id = announced["id"].as_str().unwrap().to_string();

    // Approved, named, ready to be claimed by the right machine.
    send(
        &router,
        "PATCH",
        &format!("enrollments/{id}"),
        json!({ "spec": { "node": "peter", "roles": ["hypervisor"], "approved": true } }),
    )
    .await;

    let (wrong_key, wrong_body) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&other, &id) }),
    )
    .await;
    let (no_such, _) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": "m-000000000000", "signature": sign_claim(&other, "m-000000000000") }),
    )
    .await;
    assert_eq!(wrong_key, StatusCode::NOT_FOUND, "{wrong_body}");
    assert_eq!(
        wrong_key, no_such,
        "the wrong key and an id nobody announced answer the same, so a stranger cannot \
         enumerate which machines are waiting"
    );
    // Nothing was minted, so the node was never made.
    let (status, _) = send(&router, "GET", "nodes/peter", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// "Not yet" and "no" are different answers all the way out to the wire: a
/// machine polling on the first has to stop on the second.
#[tokio::test]
async fn a_refusal_answers_differently_from_a_wait() {
    let router = api();
    let (pair, public) = keypair();
    let (_, announced) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    let id = announced["id"].as_str().unwrap().to_string();

    let (waiting, _) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(waiting, StatusCode::BAD_REQUEST);

    send(
        &router,
        "PATCH",
        &format!("enrollments/{id}"),
        json!({ "spec": { "refused": true } }),
    )
    .await;
    let (turned_away, body) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(turned_away, StatusCode::FORBIDDEN, "{body}");
}

/// Approved and nothing else said. The machine is told which half is missing,
/// because "the API said no" at three in the morning is the failure this
/// platform keeps removing.
#[tokio::test]
async fn approving_without_naming_the_machine_says_which_field_is_missing() {
    let router = api();
    let (pair, public) = keypair();
    let (_, announced) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    let id = announced["id"].as_str().unwrap().to_string();

    send(
        &router,
        "PATCH",
        &format!("enrollments/{id}"),
        json!({ "spec": { "approved": true } }),
    )
    .await;
    let (_status, body) = anon(
        &router,
        "POST",
        "enrollments:claim",
        json!({ "id": &id, "signature": sign_claim(&pair, &id) }),
    )
    .await;
    assert_eq!(
        body["error"]["code"],
        json!("FAILED_PRECONDITION"),
        "{body}"
    );
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("spec.node"), "{message}");
}

/// The unauthenticated door refuses rubbish by name. It is the one place a
/// stranger can write here, so what it will not take matters.
#[tokio::test]
async fn the_open_door_refuses_what_is_not_a_key() {
    let router = api();
    for (key, why) in [
        ("", "no key at all"),
        ("not base64 at all!!", "not base64"),
        ("AAAA", "the wrong length"),
    ] {
        let (status, body) = anon(
            &router,
            "POST",
            "enrollments:announce",
            json!({ "publicKey": key }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(body["error"]["field"], json!("publicKey"), "{why}: {body}");
    }
    // And nothing was written, so a loop of rubbish cannot grow the store.
    let (_, list) = send(&router, "GET", "enrollments", Value::Null).await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(0), "{list}");
}

/// An operator may not set the authority a registration happens under, even
/// though it is a field on an object they can write to.
#[tokio::test]
async fn nobody_can_send_the_approver() {
    let router = api();
    let (_, public) = keypair();
    let (_, announced) = anon(
        &router,
        "POST",
        "enrollments:announce",
        announcement(&public),
    )
    .await;
    let id = announced["id"].as_str().unwrap().to_string();
    let (status, body) = send(
        &router,
        "PATCH",
        &format!("enrollments/{id}"),
        json!({ "status": { "approvedBy": "root" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, after) = send(&router, "GET", &format!("enrollments/{id}"), Value::Null).await;
    assert_eq!(after["status"]["approvedBy"], Value::Null, "{after}");
}
