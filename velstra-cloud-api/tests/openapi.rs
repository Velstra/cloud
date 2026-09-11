//! The OpenAPI document and the router must not drift apart, and the checked-in
//! copy must be the generated one.
//!
//! `docs/openapi.json` is what a client generator reads from the repository,
//! and `GET /api/v1/openapi.json` is what one reads from a running cell. Both
//! come from `velstra_cloud_api::openapi::document()`, and this is what makes
//! the file in `docs/` a copy of it rather than a document somebody once wrote:
//! a change to the surface fails here until the file is regenerated.
//!
//!     VELSTRA_WRITE_OPENAPI=1 cargo test -p velstra-cloud-api --test openapi
//!
//! rewrites it. `velstra-cloud-api --openapi` prints the same document.

use std::path::Path;

fn checked_in() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/openapi.json")
}

/// The file in `docs/` is the generated document, byte for byte.
#[test]
fn the_checked_in_document_is_the_generated_one() {
    let generated = velstra_cloud_api::openapi::pretty();
    let path = checked_in();
    if std::env::var_os("VELSTRA_WRITE_OPENAPI").is_some() {
        std::fs::write(&path, &generated).expect("writing docs/openapi.json");
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {}: {e}. Generate it with \
             VELSTRA_WRITE_OPENAPI=1 cargo test -p velstra-cloud-api --test openapi",
            path.display()
        )
    });
    assert!(
        stored == generated,
        "docs/openapi.json is not the document the API generates. The surface \
         changed — a collection, a field, a verb — and the file has to follow: \
         VELSTRA_WRITE_OPENAPI=1 cargo test -p velstra-cloud-api --test openapi"
    );
}

/// Every custom method the router dispatches is in the document.
///
/// Read out of `rest.rs` the way `contract_routes.rs` does, so a verb added to
/// the router and not to `openapi::VERBS` is a red build rather than a method
/// no generated client can call.
#[test]
fn every_verb_the_router_dispatches_is_documented() {
    let router = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rest.rs"))
        .expect("the router is beside this check");
    let known = velstra_cloud_api::openapi::verbs();
    let mut missing: Vec<String> = router
        .match_indices("verb == \"")
        .filter_map(|(at, needle)| {
            let rest = &router[at + needle.len()..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .filter(|v| !known.contains(&v.as_str()))
        .collect();
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "these verbs are dispatched by rest.rs but not in openapi::VERBS: {missing:?}"
    );
}

/// The routes the router spells out by hand are in the document too.
#[test]
fn every_fixed_route_is_documented() {
    let doc = velstra_cloud_api::openapi::document();
    for route in [
        "/api/v1/sessions",
        "/api/v1/sessions/current",
        "/api/v1/users/{id}/password",
        "/api/v1/users/{id}/tokens",
        "/metrics",
        "/api/v1/openapi.json",
    ] {
        assert!(
            doc["paths"].get(route).is_some(),
            "{route} is served but not documented"
        );
    }
}

/// The document is one every collection in the contract can be found in, by
/// the name the contract uses.
#[test]
fn every_collection_in_the_contract_is_a_path() {
    let doc = velstra_cloud_api::openapi::document();
    let paths: Vec<&String> = doc["paths"].as_object().unwrap().keys().collect();
    for kind in velstra_cloud_api::COLLECTIONS {
        assert!(
            paths.iter().any(|p| p.ends_with(&format!("/{kind}"))),
            "{kind} has no list path"
        );
    }
}

/// **Every operation the API serves is one the React console has heard of.**
///
/// `velstra-cloud-console-react/src/api/operations.json` is what
/// `scripts/coverage.mjs` measures "every operation the API documents is
/// reachable from this UI" against — so a stale copy makes that claim about a
/// document that is not the API's, and an operation the console has never
/// heard of is a button that cannot exist and a coverage run that cannot see
/// it missing. It was generated from a *third* copy of the OpenAPI document
/// checked in beside it, which nothing generated and nothing checked: four
/// operations the API serves (the node and pool credential paths) were absent
/// from both.
///
/// The third copy is gone; this is what stands in its place.
#[test]
fn the_console_knows_every_operation_this_api_serves() {
    let document = velstra_cloud_api::openapi::document();
    let mut served: Vec<String> = document["paths"]
        .as_object()
        .expect("paths")
        .values()
        .flat_map(|path| path.as_object().expect("a path item").values())
        .filter_map(|op| op.get("operationId")?.as_str().map(str::to_string))
        .collect();
    served.sort();

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../velstra-cloud-console-react/src/api/operations.json");
    let stored = std::fs::read_to_string(&path).expect("the console's operations.json");
    let listed: serde_json::Value = serde_json::from_str(&stored).expect("valid JSON");
    let mut known: Vec<String> = listed
        .as_array()
        .expect("an array of operations")
        .iter()
        .filter_map(|op| op.get("id")?.as_str().map(str::to_string))
        .collect();
    known.sort();

    let missing: Vec<&String> = served.iter().filter(|id| !known.contains(id)).collect();
    let extra: Vec<&String> = known.iter().filter(|id| !served.contains(id)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "the React console's operations.json is not this API's surface. \n\
         It has never heard of: {missing:?}\n\
         It lists what this API does not serve: {extra:?}\n\
         Regenerate it from docs/openapi.json, which this test keeps honest."
    );
}
