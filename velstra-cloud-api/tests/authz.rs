//! Who may do what — the tests that make the answer real.
//!
//! Every other test in this crate acts as a cell operator, because that is what
//! setting up a cell needs. This one is about *tenants*: two of them, one
//! operator, and the questions that decide whether the platform is
//! multi-tenant or merely has the word "project" in its resource names.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Code, Filter, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::{
    access::Writer,
    meta::ResourceName,
    resources::{SnapshotSpec, SnapshotStatus, VolumeSpec, VolumeStatus},
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const OPERATOR: &str = "ops";
const ADA: &str = "ada";
const BOB: &str = "bob";

fn who(subject: &str) -> Identity {
    Identity::new(subject)
}

fn name(text: &str) -> ResourceName {
    ResourceName::parse(text).unwrap()
}

/// Two tenants: ada admins `p1`, bob admins `p2`, and neither is an operator.
async fn cell() -> Api {
    cell_and_store().await.0
}

/// The same cell, plus the store underneath it.
///
/// A snapshot that can be cloned from is one a pool has reported taking, and a
/// pool reports through its own writes rather than through the API — so the
/// tests about *following a reference into another project* need to reach past
/// the API to set up the thing being reached for.
async fn cell_and_store() -> (Api, Arc<dyn Store>) {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec![OPERATOR.to_string()]);

    for (project, admin) in [("p1", ADA), ("p2", BOB)] {
        api.create(
            "",
            "projects",
            &json!({
                "id": project,
                "spec": {
                    "quota": {},
                    "bindings": [{"role": "admin", "members": [admin]}]
                }
            }),
            &who(OPERATOR),
        )
        .await
        .expect("an operator creates a project");
    }
    (api, store)
}

#[tokio::test]
async fn a_tenant_cannot_read_another_tenants_object() {
    // The question the whole feature exists for. Before this, any accepted
    // token could read anything in the cell.
    let api = cell().await;
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .expect("ada may create in her own project");

    let refused = api
        .get(&name("projects/p1/networks/n1"), &who(BOB))
        .await
        .expect_err("bob read ada's network");
    assert!(refused.to_string().contains("no permission"), "{refused}");

    // And the refusal is the same one he gets for something that is not there,
    // so it cannot be used to find out what is.
    let absent = api
        .get(&name("projects/p1/networks/nope"), &who(BOB))
        .await
        .expect_err("bob read a network that does not exist");
    assert_eq!(
        refused.to_string(),
        absent.to_string(),
        "the two refusals can be told apart, which enumerates the other tenant"
    );
}

#[tokio::test]
async fn a_tenant_cannot_write_or_delete_in_another_project() {
    let api = cell().await;
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .unwrap();

    let created = api
        .create(
            "projects/p1",
            "networks",
            &json!({"id": "n2", "spec": {"vni": 5002, "mtu": 1500}}),
            &who(BOB),
        )
        .await;
    assert!(created.is_err(), "bob created an object in ada's project");

    let patched = api
        .patch(
            &name("projects/p1/networks/n1"),
            &json!({"spec": {"mtu": 9000}}),
            None,
            &who(BOB),
        )
        .await;
    assert!(patched.is_err(), "bob changed ada's network");

    let deleted = api
        .delete(&name("projects/p1/networks/n1"), None, &who(BOB))
        .await;
    assert!(deleted.is_err(), "bob deleted ada's network");
}

#[tokio::test]
async fn a_viewer_may_look_and_not_touch() {
    let api = cell().await;
    api.patch(
        &name("projects/p1"),
        &json!({"spec": {"bindings": [
            {"role": "admin", "members": [ADA]},
            {"role": "viewer", "members": [BOB]}
        ]}}),
        None,
        &who(ADA),
    )
    .await
    .expect("an admin may grant");

    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .unwrap();

    api.get(&name("projects/p1/networks/n1"), &who(BOB))
        .await
        .expect("a viewer may read");
    assert!(
        api.patch(
            &name("projects/p1/networks/n1"),
            &json!({"spec": {"mtu": 9000}}),
            None,
            &who(BOB),
        )
        .await
        .is_err(),
        "a viewer changed something"
    );
}

#[tokio::test]
async fn an_editor_cannot_grant_themselves_more() {
    // The escalation a role system has to be closed against, checked at the
    // surface rather than only in the model: an editor who can write the
    // bindings is an admin one request later.
    let api = cell().await;
    api.patch(
        &name("projects/p1"),
        &json!({"spec": {"bindings": [
            {"role": "admin", "members": [ADA]},
            {"role": "editor", "members": [BOB]}
        ]}}),
        None,
        &who(ADA),
    )
    .await
    .unwrap();

    // He may change ordinary things.
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(BOB),
    )
    .await
    .expect("an editor may create");

    // He may not change who may.
    let escalated = api
        .patch(
            &name("projects/p1"),
            &json!({"spec": {"bindings": [{"role": "admin", "members": [BOB]}]}}),
            None,
            &who(BOB),
        )
        .await;
    assert!(escalated.is_err(), "an editor made themselves an admin");
}

#[tokio::test]
async fn a_tenant_cannot_touch_the_cells_own_resources() {
    // A tenant with Admin on their project must not be able to register or
    // drain a hypervisor: a node is not inside anybody's project.
    let api = cell().await;
    for kind in ["nodes", "pools"] {
        let created = api
            .create(
                "",
                kind,
                &json!({"id": "x", "spec": {"schedulable": true, "labels": [], "accepting": true}}),
                &who(ADA),
            )
            .await;
        assert!(created.is_err(), "a tenant created a {kind}");
    }
    api.create(
        "",
        "nodes",
        &json!({"id": "node-a", "spec": {"schedulable": true, "labels": []}}),
        &who(OPERATOR),
    )
    .await
    .expect("an operator registers a node");
    assert!(
        api.get(&name("nodes/node-a"), &who(ADA)).await.is_err(),
        "a tenant read a node"
    );
}

#[tokio::test]
async fn a_list_shows_what_the_caller_may_see_rather_than_refusing() {
    // Filtered, not refused: a caller has no permission on the *collection* of
    // projects, and answering 403 would mean nobody could find the projects
    // they do have.
    let api = cell().await;
    let ada = api
        .list_for("", "projects", &Filter::none(), &who(ADA))
        .await
        .unwrap();
    let names: Vec<String> = ada
        .items
        .iter()
        .filter_map(|p| {
            p["meta"]["name"]["segments"].as_array().map(|s| {
                s.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            })
        })
        .collect();
    assert_eq!(
        names,
        vec!["projects/p1"],
        "ada sees the wrong set: {names:?}"
    );

    let bob = api
        .list_for("", "projects", &Filter::none(), &who(BOB))
        .await
        .unwrap();
    assert_eq!(bob.items.len(), 1, "bob sees more than his own");

    let ops = api
        .list_for("", "projects", &Filter::none(), &who(OPERATOR))
        .await
        .unwrap();
    assert_eq!(ops.items.len(), 2, "an operator cannot see the cell");
}

#[tokio::test]
async fn a_list_under_another_tenants_project_is_refused() {
    let api = cell().await;
    let refused = api
        .list_for("projects/p1", "networks", &Filter::none(), &who(BOB))
        .await;
    assert!(refused.is_err(), "bob listed inside ada's project");
}

#[tokio::test]
async fn a_freshly_created_project_grants_nobody_by_default() {
    // Empty bindings mean nobody but an operator. A default that granted the
    // creator would be a grant nobody chose, and one that granted everybody
    // would be the state this feature exists to end.
    let api = cell().await;
    api.create(
        "",
        "projects",
        &json!({"id": "p3", "spec": {"quota": {}}}),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    assert!(
        api.get(&name("projects/p3"), &who(ADA)).await.is_err(),
        "a project with no bindings was readable"
    );
    api.get(&name("projects/p3"), &who(OPERATOR))
        .await
        .expect("an operator can always see the cell");
}

#[tokio::test]
async fn a_tenant_cannot_watch_inside_another_project() {
    // The hole every other test in this file walked past. A watch is a read,
    // and it was the one read that took no identity at all: REST parsed the
    // bearer token and dropped it, gRPC called `who()` and discarded the
    // answer, and both then streamed whatever the parent named. Nothing here
    // noticed because a watch's answer arrives *after* the call that asked for
    // it, so a suite that checks return values never sees it.
    let api = cell().await;
    let refused = api
        .watch_for("projects/p1", "networks", None, Filter::none(), &who(BOB))
        .await;
    assert!(
        refused.is_err(),
        "bob opened a watch on everything in ada's project"
    );

    let _hers = api
        .watch_for("projects/p1", "networks", None, Filter::none(), &who(ADA))
        .await
        .expect("ada may watch her own project");
}

#[tokio::test]
async fn a_cell_wide_watch_carries_only_what_the_caller_may_see() {
    // Refusing outright would be wrong for the same reason it is wrong for a
    // list: a caller has no permission on the *collection*, so a `403` would
    // leave them unable to watch their own project's objects at all. So the
    // stream is gated per event, exactly as the listing is gated per object.
    use futures::StreamExt;

    let api = cell().await;
    let mut bobs = Box::pin(
        api.watch_for("", "networks", None, Filter::none(), &who(BOB))
            .await
            .expect("a cell-wide watch is filtered rather than refused"),
    );

    // Ada writes in her project; Bob writes in his. Bob must be told about
    // exactly one of them.
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "hers", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .unwrap();
    api.create(
        "projects/p2",
        "networks",
        &json!({"id": "his", "spec": {"vni": 5002, "mtu": 1500}}),
        &who(BOB),
    )
    .await
    .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), bobs.next())
        .await
        .expect("bob was told about nothing at all")
        .expect("the stream ended");
    let name = match event {
        velstra_cloud_api::WatchEvent::Put(document) => document["meta"]["name"]["segments"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default(),
        other => panic!("unexpected event: {other:?}"),
    };
    assert_eq!(
        name, "projects/p2/networks/his",
        "the first thing bob was told about was not his own object"
    );

    // And an operator is not gated: they are asking about the cell on purpose.
    let mut ops = Box::pin(
        api.watch_for("", "networks", None, Filter::none(), &who(OPERATOR))
            .await
            .unwrap(),
    );
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "another", "spec": {"vni": 5003, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), ops.next())
        .await
        .expect("an operator was not told about a write in the cell")
        .expect("the stream ended");
}

#[tokio::test]
async fn a_grpc_update_of_a_project_does_not_revoke_its_grants() {
    // There is no protobuf message for a binding, so an `UpdateProject` over
    // gRPC sends a `ProjectSpec` whose `bindings` are empty. What stops that
    // wiping the project's whole policy is one serde attribute:
    // `skip_serializing_if = "Vec::is_empty"` means the patch body has no
    // `bindings` key at all, and the merge leaves the stored ones alone.
    //
    // That is a load-bearing accident, so it is pinned here. Without it, every
    // gRPC update to a project's display name or quota would silently lock its
    // own admins out — and the gRPC suite would not have noticed, because it
    // asserts on the spec fields it *did* send.
    let api = cell().await;
    let sent = velstra_cloud_model::resources::ProjectSpec {
        display_name: "Payments".into(),
        parent: String::new(),
        quota: Default::default(),
        bindings: Vec::new(),
        cell: String::new(),
    };
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": sent }),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("an operator may change a project");

    api.get(&name("projects/p1"), &who(ADA))
        .await
        .expect("ada's grant on her own project was revoked by an update to its name");
}

/// Operations are objects too, and one names the object it is about.
///
/// This pins the *authorised* path, which has always been right — the hole was
/// in the gRPC handler, which reached past it, and is closed and proved by
/// `grpc_list_operations_shows_a_tenant_only_their_own` in `tests/grpc.rs`.
/// Kept separate on purpose: that test would still pass if this behaviour
/// silently changed underneath it, and this one would still pass if gRPC grew
/// the same shortcut again. Neither substitutes for the other.
#[tokio::test]
async fn a_tenant_does_not_see_another_tenants_operations() {
    let api = cell().await;

    // Each tenant does something in their own project. A create mints an
    // operation, so this is the ordinary way operations come to exist.
    for (project, admin, id) in [("p1", ADA, "n-ada"), ("p2", BOB, "n-bob")] {
        api.create(
            &format!("projects/{project}"),
            "networks",
            &json!({"id": id, "spec": {"vni": 5001, "mtu": 1500}}),
            &who(admin),
        )
        .await
        .expect("a project admin creates a network in their own project");
    }

    let ada_sees = api
        .list_for("", "operations", &Filter::none(), &who(ADA))
        .await
        .expect("a tenant may list operations");
    let names: Vec<String> = ada_sees
        .items
        .iter()
        .map(|o| velstra_cloud_wire::joined(&o["meta"]["name"]).unwrap_or_default())
        .collect();

    assert!(
        !names.is_empty(),
        "the tenant was shown nothing at all, so this proves nothing"
    );
    assert!(
        names.iter().all(|n| !n.contains("/p2/")),
        "a tenant was shown another tenant's operations: {names:?}"
    );

    // And the operator still sees both, so the filter did not simply refuse
    // everybody — the failure that would make the assertion above vacuous.
    let operator_sees = api
        .list_for("", "operations", &Filter::none(), &who(OPERATOR))
        .await
        .expect("an operator may list operations");
    assert!(
        operator_sees.items.len() > ada_sees.items.len(),
        "the operator saw no more than the tenant, so nothing is being filtered"
    );
}

/// A reference is a way to reach another object, and reaching has to be
/// authorised like anything else.
///
/// This was open. A write was authorised against the project the new object
/// lives in, and every reference in its spec was checked for *spelling* and
/// nothing else — so bob could create a volume in `p2` whose `sourceSnapshot`
/// named ada's snapshot in `p1`, and the pool would clone ada's bytes into a
/// volume bob owns. Nothing in the platform would have recorded that it
/// happened: the volume is bob's, the snapshot is untouched, and the only
/// evidence is a resource name in a spec field.
#[tokio::test]
async fn a_tenant_cannot_make_a_volume_from_another_tenants_snapshot() {
    let (api, store) = cell_and_store().await;

    // Ada's volume, and her snapshot of it. Both halves of the pool's report
    // are written the way the pool writes them, because a snapshot that has
    // not been taken is refused for a different reason and would hide this one.
    api.create(
        "projects/p1",
        "volumes",
        &json!({"id": "data-1", "spec": {"size_gib": 100, "pool": "pool-a"}}),
        &who(ADA),
    )
    .await
    .expect("ada creates a volume in her own project");
    provisioned(&store, "projects/p1/volumes/data-1", 100).await;

    api.create(
        "projects/p1/volumes/data-1",
        "snapshots",
        &json!({"id": "nightly"}),
        &who(ADA),
    )
    .await
    .expect("ada takes a copy of her own volume");
    taken(&store, "projects/p1/volumes/data-1/snapshots/nightly", 100).await;

    let refused = api
        .create(
            "projects/p2",
            "volumes",
            &json!({
                "id": "stolen",
                "spec": {"source_snapshot": "projects/p1/volumes/data-1/snapshots/nightly"}
            }),
            &who(BOB),
        )
        .await
        .map(|_| ())
        .expect_err("bob cloned ada's snapshot into his own project");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // And the refusal says nothing about ada's snapshot being there: the same
    // request naming something that does not exist is refused in the same
    // words, so this cannot be used to find out what ada has.
    let absent = api
        .create(
            "projects/p2",
            "volumes",
            &json!({
                "id": "stolen",
                "spec": {"source_snapshot": "projects/p1/volumes/nope/snapshots/nope"}
            }),
            &who(BOB),
        )
        .await
        .map(|_| ())
        .expect_err("bob named a snapshot that does not exist");
    assert_eq!(
        refused.to_string(),
        absent.to_string(),
        "the two refusals can be told apart, which enumerates the other tenant"
    );

    // Nothing was written on the way to being refused.
    api.get(&name("projects/p2/volumes/stolen"), &who(OPERATOR))
        .await
        .expect_err("the volume exists, so the refusal came too late to matter");
}

/// The same hole, on the field a machine boots from.
///
/// An image is smaller than a volume and worse to leak: it is the disk a guest
/// starts from, so a tenant who may boot another tenant's image reads whatever
/// that image was built with — keys baked into a golden image, a customer's
/// data left in `/var`.
#[tokio::test]
async fn a_tenant_cannot_boot_another_tenants_image() {
    let api = cell().await;
    api.create(
        "projects/p1",
        "images",
        &json!({"id": "sha256-abc", "spec": {"digest": "sha256:abc"}}),
        &who(ADA),
    )
    .await
    .expect("ada uploads an image into her own project");

    let refused = api
        .create(
            "projects/p2",
            "instances",
            &json!({
                "id": "i1",
                "spec": {"vcpus": 2, "image": "projects/p1/images/sha256-abc"}
            }),
            &who(BOB),
        )
        .await
        .map(|_| ())
        .expect_err("bob booted a machine from ada's image");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    let absent = api
        .create(
            "projects/p2",
            "instances",
            &json!({
                "id": "i1",
                "spec": {"vcpus": 2, "image": "projects/p1/images/sha256-nope"}
            }),
            &who(BOB),
        )
        .await
        .map(|_| ())
        .expect_err("bob named an image that does not exist");
    assert_eq!(
        refused.to_string(),
        absent.to_string(),
        "the two refusals can be told apart, which enumerates the other tenant"
    );

    // The reference bob is entitled to is still ordinary: his own project's
    // image, named the same way, from the same request shape. A check that
    // refused this one would have made every reference in the system useless.
    api.create(
        "projects/p2",
        "images",
        &json!({"id": "sha256-def", "spec": {"digest": "sha256:def"}}),
        &who(BOB),
    )
    .await
    .expect("bob uploads an image into his own project");
    api.create(
        "projects/p2",
        "instances",
        &json!({
            "id": "i1",
            "spec": {"vcpus": 2, "image": "projects/p2/images/sha256-def"}
        }),
        &who(BOB),
    )
    .await
    .expect("bob boots a machine from his own image");
}

/// A change is a write like any other, and it carries references too.
#[tokio::test]
async fn a_tenant_cannot_patch_a_reference_into_another_project() {
    let api = cell().await;
    api.create(
        "projects/p2",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(BOB),
    )
    .await
    .unwrap();
    api.create(
        "projects/p2",
        "routers",
        &json!({"id": "r1", "spec": {"networks": ["projects/p2/networks/n1"]}}),
        &who(BOB),
    )
    .await
    .expect("bob creates a router over his own network");

    let refused = api
        .patch(
            &name("projects/p2/routers/r1"),
            &json!({"spec": {"networks": ["projects/p1/networks/n1"]}}),
            None,
            &who(BOB),
        )
        .await
        .expect_err("bob routed ada's network");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // Back to his own, unchanged, so the check bites the boundary rather than
    // the field.
    api.patch(
        &name("projects/p2/routers/r1"),
        &json!({"spec": {"networks": ["projects/p2/networks/n1"]}}),
        None,
        &who(BOB),
    )
    .await
    .expect("bob may still route his own network");
}

/// The pool's half of a volume's life, written the way the pool writes it.
async fn provisioned(store: &Arc<dyn Store>, name: &str, gib: u64) {
    let volumes: TypedStore<VolumeSpec, VolumeStatus> =
        TypedStore::new(store.clone(), "cell-1", "volumes");
    let mut v = volumes.get(name).await.unwrap().unwrap();
    v.status.pool = Some("pool-a".into());
    v.status.provisioned = true;
    v.status.actual_size_gib = gib;
    v.status.observed_generation = v.meta.generation;
    volumes
        .update(&v, &Writer::agent("pool-a"))
        .await
        .expect("the pool claiming a volume assigned to it");
}

/// The pool reporting that it has made the copy.
async fn taken(store: &Arc<dyn Store>, name: &str, gib: u64) {
    let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
        TypedStore::new(store.clone(), "cell-1", "snapshots");
    let mut s = snapshots.get(name).await.unwrap().unwrap();
    s.status.pool = Some("pool-a".into());
    s.status.taken = true;
    s.status.size_gib = gib;
    s.status.observed_generation = s.meta.generation;
    snapshots
        .update(&s, &Writer::agent("pool-a"))
        .await
        .expect("the pool claiming a snapshot assigned to it");
}
