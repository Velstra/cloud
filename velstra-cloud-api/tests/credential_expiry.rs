//! `:issueCredential` takes an `expiresAt`, and the contract says so.
//!
//! Every body reaching the core has been through `from_wire`, which turns the
//! `expiresAt` a client sends into `expires_at`. A read of the camel spelling
//! there is a read of a key that is never present — so the field would be
//! accepted, ignored, and the credential would never expire. Found while
//! wiring enrolment, which reads a body the same way.

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
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
    velstra_cloud_api::server(
        Api::new(
            Arc::new(MemoryStore::new()),
            "eu-central",
            "cell-1",
            verifier,
        )
        .with_cell_admins(vec!["ada".into()]),
    )
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An end date already past is refused, which is the only externally visible
/// proof that the field is read at all.
///
/// The contract promises this refusal by name: "that end date has already
/// passed, so the credential would be refused the first time it was
/// presented". A body whose `expiresAt` never arrives produces a credential
/// with no end instead, silently — the caller asked for something and got
/// something else.
#[tokio::test]
async fn an_end_date_in_the_past_is_refused() {
    let router = api();
    let (status, made) = send(
        &router,
        "POST",
        "nodes",
        json!({ "id": "peter", "spec": { "schedulable": true } }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{made}");

    let (status, body) = send(
        &router,
        "POST",
        "nodes/peter:issueCredential",
        json!({ "purpose": "a stick", "expiresAt": 1_000_000_000_000u64 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], json!("expiresAt"), "{body}");
}
