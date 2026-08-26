//! Which cell answers for a request.
//!
//! A cell is the failure and scaling domain, and growing means adding cells
//! rather than growing one. That only works if a request naming a resource can
//! find the cell holding it — and if a request that reaches the *wrong* cell is
//! refused with the name of the right one, rather than answered.
//!
//! The second half is what this file is about. Without it, a stale router, a
//! client with a hardcoded endpoint or a project created a moment ago would
//! scatter one project's resources across cells, with nothing recording that
//! they are scattered.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_store::{MemoryStore, Store};

const OPERATOR: &str = "operator";

fn who() -> Identity {
    Identity::new(OPERATOR)
}

/// An API for `cell`, sharing `store` so several cells can be pointed at one
/// backing store in a test — which is not how a deployment looks, and is the
/// cheapest way to ask "what would the other cell say".
fn api_for(store: Arc<dyn Store>, cell: &str) -> Api {
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    Api::new(store, "eu-central", cell, verifier).with_cell_admins(vec![OPERATOR.to_string()])
}

/// Create a project whose resources live in `home` (empty = unspecified).
async fn project(api: &Api, id: &str, home: &str) {
    let mut spec = json!({"quota": {}});
    if !home.is_empty() {
        spec["cell"] = json!(home);
    }
    api.create("", "projects", &json!({"id": id, "spec": spec}), &who())
        .await
        .expect("an operator creates a project");
}

async fn make_instance(api: &Api, project: &str, id: &str) -> Result<(), String> {
    api.create(
        &format!("projects/{project}"),
        "instances",
        &json!({"id": id, "spec": {"vcpus": 1, "memory_mib": 512}}),
        &who(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.message)
}

/// A resource for a project that lives elsewhere is refused, and the refusal
/// names the cell that should have answered.
///
/// Naming it is the whole point: an error a router can follow, and a person can
/// read, rather than one that only says no.
#[tokio::test]
async fn a_request_for_another_cells_project_is_refused_with_where_to_go() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let here = api_for(store.clone(), "cell-1");
    project(&here, "p-elsewhere", "cell-2").await;

    let error = make_instance(&here, "p-elsewhere", "i1")
        .await
        .expect_err("cell-1 created a resource for a project that lives in cell-2");

    assert!(
        error.contains("cell-2"),
        "the refusal does not name the cell that should answer: {error}"
    );
    assert!(
        error.contains("cell-1"),
        "the refusal does not say which cell refused: {error}"
    );
}

/// The same request, sent to the cell that owns the project, is answered.
///
/// Paired with the test above on purpose: a check that refuses everything would
/// pass that one on its own.
#[tokio::test]
async fn the_cell_that_owns_the_project_answers() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let here = api_for(store.clone(), "cell-2");
    project(&here, "p-here", "cell-2").await;

    make_instance(&here, "p-here", "i1")
        .await
        .expect("the owning cell refused its own project's resource");
}

/// A project that records no home is answered by whichever cell is asked.
///
/// This is the single-cell installation and every project written before
/// routing existed. Requiring a home would mean a one-cell deployment has to
/// name its cell before anything works, and would refuse a project created a
/// moment ago while the record propagates — turning a delay into an error the
/// tenant sees.
#[tokio::test]
async fn a_project_with_no_recorded_home_is_answered_anywhere() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let here = api_for(store.clone(), "cell-1");
    project(&here, "p-homeless", "").await;

    make_instance(&here, "p-homeless", "i1")
        .await
        .expect("a project with no recorded home was refused");
}

/// A cell's own hardware belongs to no project and is never routed away.
#[tokio::test]
async fn a_cells_own_hardware_is_not_routed() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let here = api_for(store.clone(), "cell-1");

    here.create(
        "",
        "nodes",
        &json!({"id": "node-a", "spec": {"schedulable": true}}),
        &who(),
    )
    .await
    .expect("a cell refused to register its own node");
}

/// The home cell survives being written and read back, so a router asking the
/// API where a project lives gets the answer that was set.
#[tokio::test]
async fn a_projects_home_is_readable_back() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let here = api_for(store.clone(), "cell-1");
    project(&here, "p1", "cell-3").await;

    let name = velstra_cloud_model::meta::ResourceName::parse("projects/p1").unwrap();
    let document = here.get(&name, &who()).await.unwrap();
    assert_eq!(
        document["spec"]["cell"].as_str(),
        Some("cell-3"),
        "the home cell did not survive create and read back"
    );
}
