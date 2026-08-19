//! Two cells, two listeners, and a request that lands in the right one.
//!
//! Every other routing test asks what a cell *says* about a resource it does not
//! own. This one asks whether the request actually gets there — over a socket,
//! through the hop, into the other cell's store — because "refuses with the right
//! answer" and "delivers" are different claims and only one of them is a router.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{
    Api, Identity, StaticTokenVerifier, TokenVerifier,
    proxy::{Cells, Router},
    server, server_routed,
};
use velstra_cloud_store::{MemoryStore, Store};

const TOKEN: &str = "t";
/// The subject `StaticTokenVerifier::single` mints for its token. Setup calls
/// the API directly as this identity and every HTTP request arrives as it, so
/// the two halves of each test are the same caller.
const OPERATOR: &str = "dev";

fn who() -> Identity {
    Identity::new(OPERATOR)
}

fn api_for(store: Arc<dyn Store>, cell: &str) -> Api {
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single(TOKEN));
    Api::new(store, "eu-central", cell, verifier).with_cell_admins(vec![OPERATOR.to_string()])
}

/// Serve `router` on a loopback port and hand back its address.
async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

async fn get(address: &str, path: &str) -> (u16, String) {
    use http_body_util::BodyExt;

    let uri: hyper::Uri = format!("{address}{path}").parse().unwrap();
    let host = uri.host().unwrap().to_string();
    let port = uri.port_u16().unwrap();
    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .unwrap();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let request = hyper::Request::builder()
        .method("GET")
        .uri(uri.path_and_query().unwrap().as_str())
        .header(hyper::header::HOST, &host)
        .header(hyper::header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(String::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// A request for a project that lives in cell-2, sent to cell-1, is answered by
/// cell-2 — with cell-2's data.
#[tokio::test]
async fn a_request_for_another_cells_project_is_delivered_there() {
    // Two genuinely separate stores. Sharing one would let this pass without a
    // hop ever happening, which is the whole thing under test.
    let store_1: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let store_2: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api_1 = api_for(store_1.clone(), "cell-1");
    let api_2 = api_for(store_2.clone(), "cell-2");

    // The project lives in cell-2, and BOTH cells know that: projects are
    // global, so each holds its own correctly-stamped copy.
    for api in [&api_1, &api_2] {
        api.create(
            "",
            "projects",
            &json!({"id": "p1", "spec": {"quota": {}, "cell": "cell-2"}}),
            &who(),
        )
        .await
        .unwrap();
    }
    // The instance exists only in cell-2, which is where it belongs.
    api_2
        .create(
            "projects/p1",
            "instances",
            &json!({"id": "i1", "spec": {"vcpus": 1, "memoryMib": 512}}),
            &who(),
        )
        .await
        .unwrap();

    let cell_2 = serve(server(api_2)).await;
    let cells = Cells::parse(&[format!("cell-2={cell_2}")]).unwrap();
    let cell_1 = serve(server_routed(
        api_1,
        Router::new(store_1.clone(), "cell-1", cells),
    ))
    .await;

    let (status, body) = get(&cell_1, "/api/v1/projects/p1/instances/i1").await;
    assert_eq!(
        status, 200,
        "cell-1 did not deliver the request to cell-2: {body}"
    );
    assert!(
        body.contains("\"i1\"") || body.contains("i1"),
        "the answer did not come from cell-2's store: {body}"
    );
}

/// A request for a project that lives here is answered here, without a hop.
///
/// Paired with the test above: a router that forwarded everything would pass
/// that one on its own.
#[tokio::test]
async fn a_request_for_this_cells_project_is_answered_here() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api = api_for(store.clone(), "cell-1");
    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}, "cell": "cell-1"}}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": "i1", "spec": {"vcpus": 1, "memoryMib": 512}}),
        &who(),
    )
    .await
    .unwrap();

    // An endpoint for cell-2 that does not exist: reaching for it would fail
    // loudly, so a pass here means no hop was attempted.
    let cells = Cells::parse(&["cell-2=http://127.0.0.1:1".to_string()]).unwrap();
    let address = serve(server_routed(api, Router::new(store, "cell-1", cells))).await;

    let (status, body) = get(&address, "/api/v1/projects/p1/instances/i1").await;
    assert_eq!(
        status, 200,
        "a local request was not answered locally: {body}"
    );
}

/// A cell that owns nothing the directory knows about answers everything.
///
/// The single-cell installation, and every project written before routing
/// existed. A router that demanded a home would break both.
#[tokio::test]
async fn a_project_with_no_home_is_answered_without_a_hop() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api = api_for(store.clone(), "cell-1");
    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}}}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": "i1", "spec": {"vcpus": 1, "memoryMib": 512}}),
        &who(),
    )
    .await
    .unwrap();

    let cells = Cells::parse(&["cell-2=http://127.0.0.1:1".to_string()]).unwrap();
    let address = serve(server_routed(api, Router::new(store, "cell-1", cells))).await;

    let (status, body) = get(&address, "/api/v1/projects/p1/instances/i1").await;
    assert_eq!(status, 200, "a homeless project was routed away: {body}");
}

/// A resource that belongs elsewhere and has no configured endpoint is refused,
/// not answered from the wrong cell.
#[tokio::test]
async fn an_unreachable_cell_is_refused_rather_than_answered_here() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api = api_for(store.clone(), "cell-1");
    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": {}, "cell": "cell-9"}}),
        &who(),
    )
    .await
    .unwrap();

    // No endpoint for cell-9 at all.
    let address = serve(server_routed(
        api,
        Router::new(store, "cell-1", Cells::default()),
    ))
    .await;

    let (status, body) = get(&address, "/api/v1/projects/p1/instances/i1").await;
    assert_ne!(status, 200, "a foreign resource was answered here: {body}");
    assert!(
        body.contains("cell-9"),
        "the refusal does not name the cell that owns it: {body}"
    );
}

/// Cells parse from `cell=endpoint`, and a malformed pair is refused at startup
/// rather than at the first request that needed it.
#[test]
fn cell_endpoints_are_checked_when_they_are_configured() {
    let ok = Cells::parse(&["cell-2=http://a:1".into(), "cell-3=http://b:2/".into()]).unwrap();
    assert_eq!(ok.endpoint("cell-2"), Some("http://a:1"));
    // The trailing slash is trimmed, so joining a path cannot produce `//`.
    assert_eq!(ok.endpoint("cell-3"), Some("http://b:2"));
    assert_eq!(ok.endpoint("cell-9"), None);

    assert!(Cells::parse(&["nonsense".into()]).is_err());
    assert!(Cells::parse(&["=http://a:1".into()]).is_err());
    assert!(Cells::parse(&["cell-2=".into()]).is_err());
}

/// Two routers whose directories disagree answer, rather than forwarding to
/// each other until something gives out.
///
/// This is the case a hop marker exists for and the one nothing else here
/// covers. Cell-1 believes the project lives in cell-2; cell-2 believes it lives
/// in cell-1. Without the marker each forwards to the other and the request
/// bounces until a socket, a task or the test runner runs out. With it, the
/// second cell answers — wrongly, if the directories are wrong, but with a real
/// answer from a named cell that an operator can act on.
///
/// A disagreement like this is not exotic: it is what a directory looks like for
/// the few seconds after a project moves.
#[tokio::test]
async fn two_routers_that_disagree_do_not_bounce_a_request_between_them() {
    let store_1: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let store_2: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let api_1 = api_for(store_1.clone(), "cell-1");
    let api_2 = api_for(store_2.clone(), "cell-2");

    // The disagreement: each cell's copy of the project names the *other* cell.
    api_1
        .create(
            "",
            "projects",
            &json!({"id": "p1", "spec": {"quota": {}, "cell": "cell-2"}}),
            &who(),
        )
        .await
        .unwrap();
    api_2
        .create(
            "",
            "projects",
            &json!({"id": "p1", "spec": {"quota": {}, "cell": "cell-1"}}),
            &who(),
        )
        .await
        .unwrap();

    // cell-2 is brought up first so cell-1 can be told where it is; cell-2's own
    // router is then pointed back at cell-1, closing the circle.
    let listener_2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address_2 = format!("http://{}", listener_2.local_addr().unwrap());
    let listener_1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address_1 = format!("http://{}", listener_1.local_addr().unwrap());

    let router_2 = Router::new(
        store_2.clone(),
        "cell-2",
        Cells::parse(&[format!("cell-1={address_1}")]).unwrap(),
    );
    tokio::spawn(async move {
        axum::serve(listener_2, server_routed(api_2, router_2))
            .await
            .unwrap();
    });
    let router_1 = Router::new(
        store_1.clone(),
        "cell-1",
        Cells::parse(&[format!("cell-2={address_2}")]).unwrap(),
    );
    tokio::spawn(async move {
        axum::serve(listener_1, server_routed(api_1, router_1))
            .await
            .unwrap();
    });

    // The request must come back — any answer at all. A bounce shows up as this
    // never returning, so the timeout is the assertion.
    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        get(&address_1, "/api/v1/projects/p1/instances/i1"),
    )
    .await;

    let (status, _body) = answered.expect(
        "the request never came back: two routers that disagree are forwarding it \
         to each other",
    );
    // 404 is the honest answer — cell-2 does not hold this instance either. What
    // matters is that exactly one hop happened and something replied.
    assert!(
        status == 404 || status == 403,
        "expected an answer from the second cell, got {status}"
    );
}
