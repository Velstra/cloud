//! Withdrawing an image without breaking what is already running.
//!
//! An image is content-addressed and immutable, so it is never replaced — a
//! newer one is published beside it. That left every superseded image looking
//! exactly as current as its replacement on every list and every form, with no
//! way to tell them apart but comparing dates by eye, and no way at all to say
//! "stop building new things on this".

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::meta::ResourceName;
use velstra_cloud_store::{MemoryStore, Store};

const OPS: &str = "ops";

fn who() -> Identity {
    Identity::new(OPS)
}

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
    api.create(
        "",
        "pools",
        &json!({"id": "pool-a", "spec": {"accepting": true}}),
        &who(),
    )
    .await
    .unwrap();
    for (id, version) in [("old", "1"), ("new", "2")] {
        api.create(
            "",
            "images",
            &json!({"id": id, "spec": {
                "family": "debian-13",
                "version": version,
                "digest": format!("sha256:{}", if id == "old" { "a".repeat(64) } else { "b".repeat(64) }),
                "source_url": "http://images.invalid/x.qcow2",
                "format": "Qcow2"
            }}),
            &who(),
        )
        .await
        .unwrap();
    }
    api
}

async fn set_state(api: &Api, id: &str, state: &str, replacement: &str) {
    let name = ResourceName::parse(&format!("images/{id}")).unwrap();
    api.patch(
        &name,
        &json!({"spec": {"state": state, "replacement": replacement}}),
        None,
        &who(),
    )
    .await
    .unwrap();
}

async fn make_guest(api: &Api, id: &str, image: &str) -> Result<(), String> {
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": id, "spec": {
            "vcpus": 1, "memory_mib": 512, "root_disk_gib": 1,
            "image": image,
            "networks": []
        }}),
        &who(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// A deprecated image still boots. Everything pinned to that digest keeps
/// working, which is the difference between a notice period and an outage.
#[tokio::test]
async fn a_deprecated_image_still_builds_a_guest() {
    let api = cell().await;
    set_state(&api, "old", "Deprecated", "images/new").await;
    make_guest(&api, "web-1", "images/old")
        .await
        .expect("a deprecated image refused a guest");
}

/// A retired one does not, and the refusal names where to go.
#[tokio::test]
async fn a_retired_image_refuses_a_new_guest_and_says_what_to_use() {
    let api = cell().await;
    set_state(&api, "old", "Obsolete", "images/new").await;
    let why = make_guest(&api, "web-1", "images/old")
        .await
        .expect_err("a retired image built a guest");
    assert!(why.contains("retired"), "{why}");
    assert!(
        why.contains("images/new"),
        "the refusal does not say what to use instead: {why}"
    );
}

/// The same door, for a volume made from an image.
#[tokio::test]
async fn a_retired_image_refuses_a_volume_too() {
    let api = cell().await;
    set_state(&api, "old", "Obsolete", "").await;
    let refused = api
        .create(
            "projects/p1",
            "volumes",
            &json!({"id": "v1", "spec": {"size_gib": 2, "source_image": "images/old"}}),
            &who(),
        )
        .await;
    let Err(why) = refused else {
        panic!("a retired image filled a volume");
    };
    let why = why.to_string();
    assert!(why.contains("retired"), "{why}");
    // Nothing to point at is said plainly rather than left as a dangling
    // sentence with no name in it.
    assert!(why.contains("Nothing was named"), "{why}");
}

/// A guest already running on a retired image is untouched: withdrawing an
/// image must not be a way to take a fleet down.
#[tokio::test]
async fn retiring_does_not_disturb_what_is_already_running() {
    let api = cell().await;
    make_guest(&api, "web-1", "images/old").await.unwrap();
    set_state(&api, "old", "Obsolete", "images/new").await;
    let guest: serde_json::Value = api
        .get(
            &ResourceName::parse("projects/p1/instances/web-1").unwrap(),
            &who(),
        )
        .await
        .expect("the guest is still there");
    assert_eq!(guest["spec"]["image"], "images/old");
}

/// A family resolves past what was superseded, so a tenant asking for
/// `families/debian-13` lands on the newer image without knowing any of this
/// happened.
#[tokio::test]
async fn a_family_lands_on_the_image_that_is_still_current() {
    let api = cell().await;
    set_state(&api, "new", "Deprecated", "").await;
    make_guest(&api, "web-1", "families/debian-13")
        .await
        .unwrap();
    let guest: serde_json::Value = api
        .get(
            &ResourceName::parse("projects/p1/instances/web-1").unwrap(),
            &who(),
        )
        .await
        .unwrap();
    assert_eq!(
        guest["spec"]["image"], "images/old",
        "the family resolved to an image that had been deprecated"
    );
}

/// A replacement that is not an image is refused: the field exists to be read
/// out to a tenant, and a sentence sending them somewhere that is not an image
/// is worse than one that names nothing.
#[tokio::test]
async fn a_replacement_must_be_an_image() {
    let api = cell().await;
    let name = ResourceName::parse("images/old").unwrap();
    let refused = api
        .patch(
            &name,
            &json!({"spec": {"state": "Obsolete", "replacement": "images/not-here"}}),
            None,
            &who(),
        )
        .await;
    assert!(
        refused.is_err(),
        "an image was told to replace itself with something that does not exist"
    );
}

/// A refusal must not leave a wire behind. The check used to sit after
/// `settle_default_network`, which mints a port for a guest that names no
/// network — so a guest that was refused left a port for a machine that never
/// existed, and only the controller's own tidying took it away.
#[tokio::test]
async fn a_refused_guest_leaves_no_port_behind() {
    let api = cell().await;
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
            "cidr": "10.20.0.0/24", "gateway": "10.20.0.1", "dns": [], "reserved": []
        }}),
        &who(),
    )
    .await
    .unwrap();
    set_state(&api, "old", "Obsolete", "images/new").await;

    assert!(
        make_guest(&api, "web-1", "images/old").await.is_err(),
        "a retired image built a guest"
    );
    let ports = api.list("projects/p1", "ports").await.unwrap();
    assert!(
        ports.items.is_empty(),
        "a refused guest left a port behind: {:?}",
        ports.items
    );
}
