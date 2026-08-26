//! The router and the contract must not drift apart.
//!
//! `docs/rest-contract.md` is written against by two crates that never talk to
//! each other — the API serves it, the console consumes it — so a route the API
//! grows without a line in the document is a route the console cannot know
//! exists, and a collection dropped from the document is one the console will
//! stop drawing while the API goes on serving it. Neither shows up as a test
//! failure anywhere else, because each crate is internally consistent; the drift
//! is only visible from the seam between them.
//!
//! This is deliberately a **coarse** check: it asks whether each thing the
//! router serves is *mentioned* in the document, not whether the prose around it
//! is right. That is enough to make "added a route, forgot the contract" a red
//! build, which is the failure worth catching automatically — the wording is a
//! human review.

use std::path::Path;

/// The contract document, read from the workspace root beside this crate.
fn contract() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/rest-contract.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read the REST contract at {}: {e}",
            path.display()
        )
    })
}

/// Every collection the API serves appears in the contract.
///
/// The list is [`velstra_cloud_api::COLLECTIONS`] itself — the same array the
/// router builds its handlers from — so a collection added there without a
/// mention in the document fails here rather than shipping a route no client was
/// told about.
#[test]
fn every_served_collection_is_named_in_the_contract() {
    let doc = contract();
    let mut missing = Vec::new();
    for kind in velstra_cloud_api::COLLECTIONS {
        // The collection name as it appears in a path — `security-groups`,
        // `ceph-clusters`, `floatingips`. A bare mention is all this asks for;
        // the contract lists them in one sentence and again in the paths.
        if !doc.contains(kind) {
            missing.push(kind);
        }
    }
    assert!(
        missing.is_empty(),
        "these collections are served by the API but are not mentioned in \
         docs/rest-contract.md: {missing:?}. Add them to the contract, or stop \
         serving them — the console is written against that document and cannot \
         see a route that is only in the router."
    );
}

/// Every route with a fixed path — the ones outside the collection catch-all —
/// appears in the contract.
///
/// These are the authentication routes and the custom methods. The catch-all
/// `/api/v1/*name` is covered by the collection check above; these are the
/// handlers the router spells out by hand, and each is a string the contract
/// spells out too.
#[test]
fn every_fixed_route_and_verb_is_named_in_the_contract() {
    let doc = contract();

    // The session and password routes, exactly as `rest.rs` registers them and
    // as the "Sessions and passwords" section of the contract writes them.
    let fixed_routes = [
        "/api/v1/sessions",
        "/api/v1/sessions/current",
        "/api/v1/users/{id}/password",
    ];
    // The custom methods the router dispatches — AIP-136 verbs — read out of
    // the router itself.
    //
    // This used to be a hand-kept list, and a hand-kept list of what the
    // router serves is blind to exactly the thing this check exists to catch:
    // a verb added to `rest.rs` and not to the contract sails past, because
    // nobody thought to add it here either. Two had already slipped through
    // when this was rewritten.
    let router = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rest.rs"))
        .expect("the router is beside this check");
    let mut verbs: Vec<String> = router
        .match_indices("verb == \"")
        .filter_map(|(at, needle)| {
            let rest = &router[at + needle.len()..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .collect();
    verbs.sort();
    verbs.dedup();
    assert!(
        !verbs.is_empty(),
        "no verbs were found in the router, so this check is proving nothing — \
         has the dispatch changed shape?"
    );

    let mut missing = Vec::new();
    for route in fixed_routes {
        if !doc.contains(route) {
            missing.push(route.to_string());
        }
    }
    for verb in &verbs {
        if !doc.contains(verb.as_str()) {
            missing.push(verb.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "these routes or verbs are served by the API but are not mentioned in \
         docs/rest-contract.md: {missing:?}. A fixed route the contract does not \
         describe is one the console cannot rely on."
    );
}
