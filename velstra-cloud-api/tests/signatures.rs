//! An image signature is judged at admission: verified is stored, anything
//! else is refused at the field, and a cell with no key refuses them all.

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
use velstra_cloud_model::images::SigningKey;
use velstra_cloud_store::MemoryStore;

const TOKEN: &str = "development-token";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn keypair() -> (ring::signature::Ed25519KeyPair, SigningKey) {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let public: [u8; 32] = pair.public_key().as_ref().try_into().unwrap();
    (pair, SigningKey::from_bytes(public))
}

fn sign(pair: &ring::signature::Ed25519KeyPair, message: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(pair.sign(message.as_bytes()).as_ref())
}

fn api(keys: Vec<SigningKey>) -> axum::Router {
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([(
        TOKEN.to_string(),
        velstra_cloud_api::Identity::new("dev"),
    )]));
    let api = Api::new(
        Arc::new(MemoryStore::new()),
        "eu-central",
        "cell-1",
        verifier,
    )
    .with_cell_admins(vec!["dev".into()])
    .with_image_signing_keys(keys);
    velstra_cloud_api::server(api)
}

async fn send(router: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(format!("/api/v1/{path}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
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

fn image(id: &str, signature: Option<&str>) -> Value {
    let mut spec = json!({
        "family": "debian-13", "version": "1", "digest": DIGEST,
        "format": "Qcow2", "sizeBytes": 1, "sourceUrl": "https://images.invalid/x.qcow2",
    });
    if let Some(s) = signature {
        spec["signature"] = json!(s);
    }
    json!({ "id": id, "spec": spec })
}

#[tokio::test]
async fn a_cell_with_no_key_refuses_every_signature_and_keeps_taking_unsigned_images() {
    let router = api(Vec::new());
    let (pair, _) = keypair();
    let (status, body) = send(
        &router,
        "POST",
        "images",
        image("signed", Some(&sign(&pair, DIGEST))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "spec.signature");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--image-signing-key"),
        "{body}"
    );
    let (status, _) = send(&router, "POST", "images", image("plain", None)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn a_signature_under_the_cells_key_is_stored_and_one_under_another_is_refused() {
    let (pair, key) = keypair();
    let (stranger, _) = keypair();
    let router = api(vec![key]);
    let (status, body) = send(
        &router,
        "POST",
        "images",
        image("good", Some(&sign(&pair, DIGEST))),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (_, stored) = send(&router, "GET", "images/good", Value::Null).await;
    assert_eq!(
        stored["spec"]["signature"],
        json!(sign(&pair, DIGEST)),
        "{stored}"
    );
    let (status, body) = send(
        &router,
        "POST",
        "images",
        image("bad", Some(&sign(&stranger, DIGEST))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "spec.signature");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("another key"),
        "{body}"
    );
}

#[tokio::test]
async fn a_patch_that_adds_a_signature_is_judged_over_the_digest_it_carries() {
    let (pair, key) = keypair();
    let router = api(vec![key]);
    let (status, _) = send(&router, "POST", "images", image("later", None)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // An image patch restates the digest (the API refuses one that does not),
    // so the signature is judged over the digest the patch carries.
    let mut spec = image("later", Some(&sign(&pair, DIGEST)))["spec"].clone();
    let (status, body) = send(&router, "PATCH", "images/later", json!({ "spec": spec })).await;
    assert!(status.is_success(), "{status} {body}");
    let (_, stored) = send(&router, "GET", "images/later", Value::Null).await;
    assert_eq!(
        stored["spec"]["signature"],
        json!(sign(&pair, DIGEST)),
        "{stored}"
    );
    spec["signature"] = json!(sign(&pair, "sha256:not-this"));
    let (status, body) = send(&router, "PATCH", "images/later", json!({ "spec": spec })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "spec.signature");
}
