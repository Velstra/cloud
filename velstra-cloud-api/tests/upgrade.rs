//! Releases, and the install medium cut from one.
//!
//! What is proven here is the API's half of `docs/upgrading.md`: a release is
//! the cell operator's to add and nobody else's; a medium is cut for a node
//! from the installer a release holds, and only from one the cell has fetched
//! and verified; the medium is the ISO with the node's join file on its tail,
//! collected once under a link that carries no other credential; and a node
//! fetches a release's files from its own cell, by the names the release's
//! status gives them and no other.

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, Meta, Placement, ResourceName},
    release::{Artefact, ReleaseSpec, ReleaseStatus},
    resources::Resource,
};
use velstra_cloud_store::{MemoryStore, TypedStore};

const OPERATOR: &str = "operator-token";
const TENANT: &str = "tenant-token";
const AGENT: &str = "peter-agent-token";
const ISO: &str = "velstra-cloud-installer_0.2.0+20260918.c571d71_amd64.iso";

struct Cell {
    router: axum::Router,
    store: Arc<MemoryStore>,
    dir: std::path::PathBuf,
}

impl Drop for Cell {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn cell(tag: &str) -> Cell {
    let mut agent = Identity::new("node:peter");
    agent.scopes.push("agent:peter".into());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([
        (OPERATOR.to_string(), Identity::new("ada")),
        (TENANT.to_string(), Identity::new("bob")),
        (AGENT.to_string(), agent),
    ]));
    let store = Arc::new(MemoryStore::new());
    let dir = std::env::temp_dir().join(format!("velstra-upgrade-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec!["ada".into()])
        .with_join_facts(
            vec!["https://cell-1:8443".into()],
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
        )
        .with_releases_dir(dir.clone());
    Cell {
        router: velstra_cloud_api::server(api),
        store,
        dir,
    }
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, bytes.to_vec())
}

async fn json(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let (status, _, bytes) = call(router, method, path, body, token).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A release the controller has finished with: the installer named, fetched
/// and on disk under the releases directory.
async fn a_ready_release(cell: &Cell, id: &str, fetched: bool) -> Vec<u8> {
    let iso: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let here = cell.dir.join(id);
    std::fs::create_dir_all(&here).unwrap();
    std::fs::write(here.join(ISO), &iso).unwrap();
    let releases: TypedStore<ReleaseSpec, ReleaseStatus> =
        TypedStore::new(cell.store.clone(), "cell-1", "releases");
    let mut status = ReleaseStatus {
        version: "0.2.0+20260918.c571d71".into(),
        installer: Some(Artefact {
            file: ISO.into(),
            sha256: "ab".repeat(32),
            fetched,
        }),
        ..Default::default()
    };
    if fetched {
        status.conditions.push(Condition::ready(1));
    }
    let release = Resource::new(
        Meta::new(
            ResourceName::parse(&format!("releases/{id}")).unwrap(),
            Placement::new("eu-central", "cell-1"),
        ),
        ReleaseSpec {
            url: "https://github.com/Velstra/cloud/releases/download/v0.2.0/".into(),
        },
        status,
    );
    releases
        .create(&release, &Writer::controller("release"))
        .await
        .unwrap();
    iso
}

/// A release is the cell's: an operator adds one by its channel, a tenant is
/// refused in the words every cell-wide resource refuses with.
#[tokio::test]
async fn a_release_is_the_cell_operators_to_add() {
    let cell = cell("add");
    let body = json!({ "id": "v0.2.0", "spec": {
        "url": "https://github.com/Velstra/cloud/releases/download/v0.2.0/"
    }});
    let (status, refused) =
        json(&cell.router, "POST", "releases", body.clone(), Some(TENANT)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, made) = json(&cell.router, "POST", "releases", body, Some(OPERATOR)).await;
    assert!(status.is_success(), "{status} {made}");
    let (status, read) = json(
        &cell.router,
        "GET",
        "releases/v0.2.0",
        Value::Null,
        Some(OPERATOR),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        read["spec"]["url"],
        json!("https://github.com/Velstra/cloud/releases/download/v0.2.0/")
    );
    // Nothing has read the channel yet, and the status says so rather than
    // guessing a version.
    assert_eq!(read["status"]["version"], Value::Null, "{read}");
}

/// The whole way through: a node, a ready release, a medium cut for the node,
/// collected once — the ISO's bytes and then the node's join file.
#[tokio::test]
async fn a_medium_is_the_installer_with_the_nodes_join_file_on_its_tail() {
    let cell = cell("cut");
    let iso = a_ready_release(&cell, "v0.2.0", true).await;
    let (status, made) = json(
        &cell.router,
        "POST",
        "nodes",
        json!({ "id": "peter", "spec": { "schedulable": true } }),
        Some(OPERATOR),
    )
    .await;
    assert!(status.is_success(), "{status} {made}");

    let (status, cut) = json(
        &cell.router,
        "POST",
        "nodes/peter:installMedium",
        json!({ "release": "releases/v0.2.0" }),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cut}");
    let url = cut["url"].as_str().expect("a link");
    assert!(url.starts_with("/api/v1/media/"), "{cut}");
    assert_eq!(
        cut["filename"],
        json!("velstra-cloud-installer_0.2.0+20260918.c571d71_peter.iso")
    );
    assert_eq!(cut["release"], json!("releases/v0.2.0"));
    let size = cut["size"].as_u64().expect("a size");
    assert!(size > iso.len() as u64, "the trailer is counted: {cut}");
    // The link carries no token: it is the credential, once.
    let path = url.trim_start_matches("/api/v1/").to_string();
    let (status, headers, body) = call(&cell.router, "GET", &path, Value::Null, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len() as u64, size);
    assert_eq!(
        &body[..iso.len()],
        &iso[..],
        "the ISO's bytes come first, untouched"
    );
    let trailer = velstra_cloud_wire::join::trailer_in(&body).expect("a trailer after the ISO");
    let token = velstra_cloud_node_join_token(&trailer);
    assert_eq!(token.node, "peter");
    assert_eq!(token.cell, "cell-1");
    assert!(
        !token.token.is_empty(),
        "the join file carries the node's credential"
    );
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(disposition.contains("peter.iso"), "{disposition}");
    assert_eq!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some(size.to_string().as_str())
    );
    // Once.
    let (status, _, _) = call(&cell.router, "GET", &path, Value::Null, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a collected medium is gone");
    // And the cut is on the record, as the credential it minted.
    let (_, audit) = json(
        &cell.router,
        "GET",
        "audit?target=nodes/peter",
        Value::Null,
        Some(OPERATOR),
    )
    .await;
    let verbs: Vec<&str> = audit["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["spec"]["verb"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(verbs.contains(&"installMedium"), "{audit}");
}

/// The join file is what `token_in` in the installer reads; this is the same
/// reading, so the test cannot pass on a trailer the installer would not.
fn velstra_cloud_node_join_token(text: &str) -> velstra_cloud_wire::join::JoinToken {
    text.lines()
        .find_map(|line| velstra_cloud_wire::join::JoinToken::decode(line).ok())
        .expect("a join token in the trailer")
}

/// Without a fetched installer there is no medium, and the refusal says what
/// to do — and a tenant is refused before anything is minted.
#[tokio::test]
async fn no_installer_on_the_cell_means_no_medium_and_says_so() {
    let cell = cell("none");
    let (status, _) = json(
        &cell.router,
        "POST",
        "nodes",
        json!({ "id": "peter", "spec": { "schedulable": true } }),
        Some(OPERATOR),
    )
    .await;
    assert!(status.is_success());
    let (status, refused) = json(
        &cell.router,
        "POST",
        "nodes/peter:installMedium",
        json!({}),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        json!("FAILED_PRECONDITION"),
        "{status} {refused}"
    );
    let said = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("Releases"), "{said}");

    // A release the controller is still fetching is not one to cut from.
    a_ready_release(&cell, "v0.3.0", false).await;
    let (_, refused) = json(
        &cell.router,
        "POST",
        "nodes/peter:installMedium",
        json!({ "release": "releases/v0.3.0" }),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        json!("FAILED_PRECONDITION"),
        "{refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no installer yet"),
        "{refused}"
    );
    // A release nobody added.
    let (status, refused) = json(
        &cell.router,
        "POST",
        "nodes/peter:installMedium",
        json!({ "release": "releases/v9" }),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");

    // A tenant may not cut one at all.
    let (status, refused) = json(
        &cell.router,
        "POST",
        "nodes/peter:installMedium",
        json!({}),
        Some(TENANT),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, _, _) = call(&cell.router, "GET", "media/nothing-here", Value::Null, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A node fetches a release's files from its cell: by the names the release
/// names, once verified, and by no other name — a path is not a name.
#[tokio::test]
async fn a_node_fetches_a_releases_file_from_its_cell_by_name_only() {
    let cell = cell("file");
    let iso = a_ready_release(&cell, "v0.2.0", true).await;
    let (status, _, body) = call(
        &cell.router,
        "GET",
        &format!("releases/v0.2.0/files/{ISO}"),
        Value::Null,
        Some(AGENT),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, iso);
    // An operator may, a tenant may not, and nobody anonymous.
    let (status, _, _) = call(
        &cell.router,
        "GET",
        &format!("releases/v0.2.0/files/{ISO}"),
        Value::Null,
        Some(OPERATOR),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(
        &cell.router,
        "GET",
        &format!("releases/v0.2.0/files/{ISO}"),
        Value::Null,
        Some(TENANT),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(
        &cell.router,
        "GET",
        &format!("releases/v0.2.0/files/{ISO}"),
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A file the release does not name — even one that exists beside it.
    std::fs::write(cell.dir.join("v0.2.0").join("notes.txt"), b"x").unwrap();
    let (status, _, _) = call(
        &cell.router,
        "GET",
        "releases/v0.2.0/files/notes.txt",
        Value::Null,
        Some(AGENT),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A path, however spelled.
    let (status, _, _) = call(
        &cell.router,
        "GET",
        "releases/v0.2.0/files/..%2F..%2Fetc%2Fpasswd",
        Value::Null,
        Some(AGENT),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    // A release that is still fetching serves nothing.
    a_ready_release(&cell, "v0.3.0", false).await;
    let (status, _, _) = call(
        &cell.router,
        "GET",
        &format!("releases/v0.3.0/files/{ISO}"),
        Value::Null,
        Some(AGENT),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_rollout_cannot_be_retargeted_after_it_was_created() {
    let c = cell("immutable-rollout");
    // Resource references are validated at admission, so both releases exist.
    for id in ["one", "two"] {
        let (code, body) = json(
            &c.router,
            "POST",
            "releases",
            json!({"id": id, "spec":{"url":"https://example.invalid/channel"}}),
            Some(OPERATOR),
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED, "{body}");
    }
    let (code, body) = json(
        &c.router,
        "POST",
        "rollouts",
        json!({"id":"upgrade", "spec":{"release":"releases/one"}}),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{body}");
    let (code, body) = json(
        &c.router,
        "PATCH",
        "rollouts/upgrade",
        json!({"spec":{"release":"releases/two"}}),
        Some(OPERATOR),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["field"], "spec.release");
}
