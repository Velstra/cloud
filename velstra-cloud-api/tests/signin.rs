//! Signing in over HTTP, as the console does it.
//!
//! `tests/sessions.rs` exercises the identity store directly; this walks the
//! whole route — the unauthenticated sign-in, the token it hands back, an
//! administrator creating a user, that user signing in and being refused what
//! they may not do, and signing out.
//!
//! The reason both exist: the store tests prove the *rules*, and this proves
//! they are actually wired to a URL. A rule with no route is a rule nobody can
//! reach, and a route with no rule is worse.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use velstra_cloud_api::{Api, sessions::IdentityStore};
use velstra_cloud_store::{MemoryStore, Store};

const ROOT: &str = "an operator passphrase";
const ADA: &str = "ada's own passphrase";

struct Cell {
    router: Router,
}

impl Cell {
    /// A cell whose only administrator was bootstrapped, exactly as an
    /// installation does it. No static token anywhere: this is the path a
    /// person takes.
    async fn new() -> Self {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let identity = IdentityStore::new(store.clone(), "eu-central", "cell-1");
        assert!(identity.bootstrap_admin("root", ROOT).await.unwrap());
        let verifier = Arc::new(velstra_cloud_api::sessions::StoreTokenVerifier::new(
            identity,
        ));
        let api = Api::new(store, "eu-central", "cell-1", verifier);
        Self {
            router: velstra_cloud_api::server(api),
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    async fn sign_in(&self, user: &str, password: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            "/api/v1/sessions",
            None,
            Some(json!({"username": user, "password": password})),
        )
        .await
    }
}

/// The whole arc: sign in, act, sign out — and the token stops working.
#[tokio::test]
async fn an_operator_signs_in_acts_and_signs_out() {
    let cell = Cell::new().await;

    // Sign-in needs no token. It is the route that issues one, so demanding one
    // would be a locked door with the key inside.
    let (status, body) = cell.sign_in("root", ROOT).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"].as_str().unwrap().to_string();
    assert_eq!(body["subject"], "root");
    assert_eq!(body["cellAdmin"], true);

    // The token identifies its holder, and says what they may do at cell scope —
    // so the console draws from the API's answer rather than its own guess.
    let (status, who) = cell
        .send("GET", "/api/v1/sessions/current", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{who}");
    assert_eq!(who["subject"], "root");
    assert_eq!(who["cellAdmin"], true);
    assert_eq!(who["session"], true);

    // It works on an ordinary route.
    let (status, _) = cell
        .send("GET", "/api/v1/projects", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = cell
        .send("DELETE", "/api/v1/sessions/current", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // And now it does not. This is the assertion that makes "sign out" mean
    // something on the server rather than only in the tab.
    let (status, _) = cell
        .send("GET", "/api/v1/projects", Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An administrator creates a user, gives them a password, and that user can
/// sign in — the flow an operator actually performs on day one.
/// `whoami` says which rung the account holds in each project, so a console
/// can draw only what will be accepted — and says nothing about projects the
/// account is not named in.
#[tokio::test]
async fn whoami_names_the_projects_and_the_rung_held_in_each() {
    let cell = Cell::new().await;
    let (_, root) = cell.sign_in("root", ROOT).await;
    let root = root["token"].as_str().unwrap().to_string();
    for user in ["ada", "bob"] {
        let (status, _) = cell
            .send("POST", "/api/v1/users", Some(&root), Some(json!({"id": user, "spec": {}})))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "creating {user}");
        let (status, _) = cell
            .send(
                "PUT",
                &format!("/api/v1/users/{user}/password"),
                Some(&root),
                Some(json!({"password": ADA})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "setting {user}'s password");
    }
    for (id, bindings) in [
        ("p1", json!([{"role": "admin", "members": ["ada"]}, {"role": "viewer", "members": ["ada", "bob"]}])),
        ("p2", json!([{"role": "editor", "members": ["bob"]}])),
    ] {
        let (status, _) = cell
            .send("POST", "/api/v1/projects", Some(&root),
                  Some(json!({"id": id, "spec": {"bindings": bindings}})))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "creating {id}");
    }

    let (_, ada) = cell.sign_in("ada", ADA).await;
    let ada = ada["token"].as_str().unwrap().to_string();
    let (status, who) = cell.send("GET", "/api/v1/sessions/current", Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK);
    // The strongest rung, not the first binding that names her.
    assert_eq!(who["projects"], json!({"p1": "admin"}), "{who}");

    let (_, bob) = cell.sign_in("bob", ADA).await;
    let bob = bob["token"].as_str().unwrap().to_string();
    let (_, who) = cell.send("GET", "/api/v1/sessions/current", Some(&bob), None).await;
    assert_eq!(who["projects"], json!({"p1": "viewer", "p2": "editor"}), "{who}");

    // An operator is not measured in rungs; the map is what they are named in.
    let (_, who) = cell.send("GET", "/api/v1/sessions/current", Some(&root), None).await;
    assert_eq!(who["cellAdmin"], json!(true));
    assert_eq!(who["projects"], json!({}), "{who}");
}

#[tokio::test]
async fn an_administrator_can_create_a_user_who_can_then_sign_in() {
    let cell = Cell::new().await;
    let (_, body) = cell.sign_in("root", ROOT).await;
    let root = body["token"].as_str().unwrap().to_string();

    let (status, created) = cell
        .send(
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"id": "ada", "spec": {"displayName": "Ada Lovelace"}})),
        )
        .await;
    // 202 and an operation to follow: a create is a request, and the contract
    // says so for every collection rather than making users the exception.
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    assert_eq!(created["target"], "users/ada");

    // A user with no password cannot sign in, and is refused in the same words
    // as a wrong one.
    let (status, refused) = cell.sign_in("ada", ADA).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");

    let (status, _) = cell
        .send(
            "PUT",
            "/api/v1/users/ada/password",
            Some(&root),
            Some(json!({"password": ADA})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = cell.sign_in("ada", ADA).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["subject"], "ada");
    // Created without the switch, so not an operator. The default has to be the
    // safe one: a console that offered a new account the whole cell would be
    // one mis-click from handing it over.
    assert_eq!(body["cellAdmin"], false);
}

/// A user's own password is theirs; everybody else's is the operator's.
///
/// The middle case is the one worth pinning: a project administrator
/// administers a *project*, and letting them take over an account would make
/// project membership a route to the cell.
#[tokio::test]
async fn only_an_operator_or_the_owner_may_set_a_password() {
    let cell = Cell::new().await;
    let (_, body) = cell.sign_in("root", ROOT).await;
    let root = body["token"].as_str().unwrap().to_string();

    for id in ["ada", "bob"] {
        cell.send(
            "POST",
            "/api/v1/users",
            Some(&root),
            Some(json!({"id": id, "spec": {}})),
        )
        .await;
        cell.send(
            "PUT",
            &format!("/api/v1/users/{id}/password"),
            Some(&root),
            Some(json!({"password": ADA})),
        )
        .await;
    }
    let (_, body) = cell.sign_in("ada", ADA).await;
    let ada = body["token"].as_str().unwrap().to_string();

    // Her own, without proving the current one: refused. A self-service change
    // that skipped this would let a stolen session set a new password and lock
    // the owner out for good.
    let (status, denied) = cell
        .send(
            "PUT",
            "/api/v1/users/ada/password",
            Some(&ada),
            Some(json!({"password": "a brand new passphrase"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    // Her own, proving the current one: allowed.
    let (status, _) = cell
        .send(
            "PUT",
            "/api/v1/users/ada/password",
            Some(&ada),
            Some(json!({"currentPassword": ADA, "password": "a brand new passphrase"})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Somebody else's: refused.
    let (status, denied) = cell
        .send(
            "PUT",
            "/api/v1/users/bob/password",
            Some(&ada),
            Some(json!({"password": "not hers to set"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    // ...and bob's password is untouched.
    assert_eq!(cell.sign_in("bob", ADA).await.0, StatusCode::CREATED);
}

/// An ordinary user is not an operator, and the API says so at the route rather
/// than by showing them an empty list.
#[tokio::test]
async fn an_ordinary_user_cannot_administer_the_cell() {
    let cell = Cell::new().await;
    let (_, body) = cell.sign_in("root", ROOT).await;
    let root = body["token"].as_str().unwrap().to_string();
    cell.send(
        "POST",
        "/api/v1/users",
        Some(&root),
        Some(json!({"id": "ada", "spec": {}})),
    )
    .await;
    cell.send(
        "PUT",
        "/api/v1/users/ada/password",
        Some(&root),
        Some(json!({"password": ADA})),
    )
    .await;
    let (_, body) = cell.sign_in("ada", ADA).await;
    let ada = body["token"].as_str().unwrap().to_string();

    let (_, who) = cell
        .send("GET", "/api/v1/sessions/current", Some(&ada), None)
        .await;
    assert_eq!(who["cellAdmin"], false);

    // Registering a hypervisor is the cell's business, not a tenant's.
    let (status, denied) = cell
        .send(
            "POST",
            "/api/v1/nodes",
            Some(&ada),
            Some(json!({"id": "node-a", "spec": {"schedulable": true}})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");

    // So is making another user.
    let (status, _) = cell
        .send(
            "POST",
            "/api/v1/users",
            Some(&ada),
            Some(json!({"id": "mallory", "spec": {"cellAdmin": true}})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// An operator registers a hypervisor, and can drain it.
///
/// The point of the second half: draining is a *spec* change, so it survives a
/// controller restart. A command would not.
#[tokio::test]
async fn an_operator_adds_a_hypervisor_and_can_drain_it() {
    let cell = Cell::new().await;
    let (_, body) = cell.sign_in("root", ROOT).await;
    let root = body["token"].as_str().unwrap().to_string();

    let (status, created) = cell
        .send(
            "POST",
            "/api/v1/nodes",
            Some(&root),
            Some(json!({"id": "hv-1", "spec": {"schedulable": true, "labels": ["zone-a"]}})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{created}");
    assert_eq!(created["target"], "nodes/hv-1");

    let (status, drained) = cell
        .send(
            "PATCH",
            "/api/v1/nodes/hv-1",
            Some(&root),
            Some(json!({"spec": {"schedulable": false}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{drained}");
    assert_eq!(drained["spec"]["schedulable"], false);
    // The labels it was registered with are still there: a patch is a patch.
    assert_eq!(drained["spec"]["labels"][0], "zone-a");
}

/// Deleting a user takes their password and their live sessions with them.
///
/// Without this, an account is gone from every listing and still opens the
/// door: the credential outlives its user, and a token issued before the
/// deletion keeps working until it expires.
#[tokio::test]
async fn deleting_a_user_closes_the_door_behind_them() {
    let cell = Cell::new().await;
    let (_, body) = cell.sign_in("root", ROOT).await;
    let root = body["token"].as_str().unwrap().to_string();
    cell.send(
        "POST",
        "/api/v1/users",
        Some(&root),
        Some(json!({"id": "ada", "spec": {}})),
    )
    .await;
    cell.send(
        "PUT",
        "/api/v1/users/ada/password",
        Some(&root),
        Some(json!({"password": ADA})),
    )
    .await;
    let (_, body) = cell.sign_in("ada", ADA).await;
    let ada = body["token"].as_str().unwrap().to_string();
    assert_eq!(
        cell.send("GET", "/api/v1/projects", Some(&ada), None)
            .await
            .0,
        StatusCode::OK
    );

    let (status, _) = cell
        .send("DELETE", "/api/v1/users/ada", Some(&root), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The live token stops working...
    assert_eq!(
        cell.send("GET", "/api/v1/projects", Some(&ada), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "a deleted user's token still worked"
    );
    // ...and the password does not let them back in.
    assert_eq!(cell.sign_in("ada", ADA).await.0, StatusCode::UNAUTHORIZED);
}
