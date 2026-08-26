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

/// The person whose request was refused can read the sentence explaining it.
///
/// Backwards until now: "I clicked delete and nothing happened" was answered by
/// a record only a cell operator could open. A record is readable by whoever
/// may read **what it is about**, and by **the person it is about** — neither
/// of which leaks anything, and everything else about the cell stays the
/// operator's.
#[tokio::test]
async fn a_tenant_reads_the_refusals_about_their_own_objects_and_nobody_elses() {
    let api = cell().await;
    api.create(
        "projects/p1",
        "networks",
        &json!({"id": "n1", "spec": {"vni": 5001, "mtu": 1500}}),
        &who(ADA),
    )
    .await
    .unwrap();

    // Bob reaches into ada's project and is refused. That refusal is the
    // record this test is about.
    api.get(&name("projects/p1/networks/n1"), &who(BOB))
        .await
        .expect_err("bob read ada's network");

    // The operator sees the whole cell, so this checks the fixture rather than
    // the rule: something was recorded to read.
    let all = api
        .list_for("", "audit", &Default::default(), &who(OPERATOR))
        .await
        .unwrap();
    assert!(
        all.items
            .iter()
            .any(|r| r["spec"]["target"] == json!("projects/p1/networks/n1")),
        "nothing was recorded, so this test would pass without checking anything"
    );

    // Ada may read the object, so she may read the refusals about it — and
    // that is how she finds out somebody has been reaching for her network.
    let hers = api
        .list_for("", "audit", &Default::default(), &who(ADA))
        .await
        .unwrap();
    assert!(
        hers.items
            .iter()
            .any(|r| r["spec"]["target"] == json!("projects/p1/networks/n1")),
        "the owner of the object cannot read what was refused about it"
    );

    // Bob may read neither the object nor ada's project — but he is the
    // subject, so his own refusal is his to read, and nothing else in the cell
    // is.
    let his = api
        .list_for("", "audit", &Default::default(), &who(BOB))
        .await
        .unwrap();
    assert!(
        !his.items.is_empty(),
        "the person who was refused cannot read the sentence they were given"
    );
    assert!(
        his.items
            .iter()
            .all(|r| r["spec"]["subject"] == json!(BOB)),
        "somebody else's refusals were handed to a tenant: {:?}",
        his.items
    );
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

// ---- quota ----------------------------------------------------------------

/// A quota is enforced at admission, counted from what exists, and freed by a
/// delete — the three properties that make it a bound rather than a suggestion.
#[tokio::test]
async fn a_project_at_its_instance_limit_refuses_the_next_create() {
    let api = cell().await;
    // Only an operator may set the limit; ada admins the project but not the
    // cell (the refusal for that is its own test below).
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "quota": { "instances": 1 } } }),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("an operator sets a quota");

    // The first fits.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .expect("the first instance is within the limit");

    // The second is refused, and named as a quota exhaustion rather than any
    // other kind of no.
    let err = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "i2", "spec": { "vcpus": 1, "memory_mib": 512 } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a create past the limit was admitted");
    assert_eq!(err.code, Code::ResourceExhausted, "{}", err.message);

    // Deleting the first frees the count — quota is counted from what exists,
    // so the room comes back the moment the object is gone.
    api.delete(&name("projects/p1/instances/i1"), None, &who(ADA))
        .await
        .expect("ada deletes her own instance");
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i3", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .expect("a delete freed the quota and the next create fits");
}

/// An unset limit is unlimited, which is the early-phase default: a project
/// nobody has decided limits for must not be a project that can do nothing.
#[tokio::test]
async fn an_unset_quota_is_unlimited() {
    let api = cell().await;
    // p1 was created with `"quota": {}` — every field zero, which means unset.
    for i in 0..5 {
        api.create(
            "projects/p1",
            "instances",
            &json!({ "id": format!("i{i}"), "spec": { "vcpus": 4, "memory_mib": 4096 } }),
            &who(ADA),
        )
        .await
        .expect("an unset quota admits everything");
    }
}

/// A floating IP count is a quota dimension of its own, and the same admission
/// rule reaches it.
#[tokio::test]
async fn a_floating_ip_limit_is_enforced() {
    let (api, store) = cell_and_store().await;
    // A subnet for the addresses to come from, seeded past the API the way the
    // other reference-following tests do — the create only needs the count.
    let subnets: TypedStore<
        velstra_cloud_model::resources::SubnetSpec,
        velstra_cloud_model::resources::SubnetStatus,
    > = TypedStore::new(store.clone(), "cell-1", "subnets");
    subnets
        .create(
            &velstra_cloud_model::resources::Subnet::new(
                velstra_cloud_model::meta::Meta::new(
                    name("projects/p1/subnets/s1"),
                    velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
                ),
                velstra_cloud_model::resources::SubnetSpec {
                    network: "projects/p1/networks/n1".into(),
                    cidr: "10.0.0.0/24".into(),
                    gateway: "10.0.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                velstra_cloud_model::resources::SubnetStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "quota": { "floating_ips": 1 } } }),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    api.create(
        "projects/p1",
        "floatingips",
        &json!({ "id": "f1", "spec": { "subnet": "projects/p1/subnets/s1" } }),
        &who(ADA),
    )
    .await
    .expect("the first floating IP is within the limit");
    let err = api
        .create(
            "projects/p1",
            "floatingips",
            &json!({ "id": "f2", "spec": { "subnet": "projects/p1/subnets/s1" } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a second floating IP past the limit was admitted");
    assert_eq!(err.code, Code::ResourceExhausted, "{}", err.message);
}

/// A tenant may not raise their own quota — the property that makes a quota a
/// bound at all. ada admins her project, which lets her change its spec; the
/// quota is the one field on that spec she cannot touch, because it is the
/// cell's decision about her and not hers about herself.
#[tokio::test]
async fn a_tenant_cannot_raise_their_own_quota() {
    let api = cell().await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "quota": { "instances": 1 } } }),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("an operator sets the initial quota");

    // ada is an admin of p1 — she can change its display name...
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "display_name": "Ada's project" } }),
        None,
        &who(ADA),
    )
    .await
    .expect("a project admin may edit the project");

    // ...but not lift her own limit.
    let err = api
        .patch(
            &name("projects/p1"),
            &json!({ "spec": { "quota": { "instances": 1000 } } }),
            None,
            &who(ADA),
        )
        .await
        .expect_err("a tenant raised their own quota");
    assert_eq!(err.code, Code::PermissionDenied, "{}", err.message);

    // And the limit still bites: the refusal was not cosmetic.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .expect("the first fits");
    let err = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "i2", "spec": { "vcpus": 1, "memory_mib": 512 } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("the quota ada tried to raise did not hold");
    assert_eq!(err.code, Code::ResourceExhausted, "{}", err.message);
}

/// A refusal leaves a record, carrying the sentence the person was given.
///
/// The event a multi-tenant cell is asked about afterwards. Nothing else marks
/// it: no object is created, nothing changes, and the only other trace is a
/// status code somebody else received.
#[tokio::test]
async fn a_refusal_is_recorded_with_who_what_and_the_same_sentence() {
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

    let records = api
        .list_for("", "audit", &Filter::none(), &who(OPERATOR))
        .await
        .expect("an operator reads the audit");
    let mine: Vec<&serde_json::Value> = records
        .items
        .iter()
        .filter(|r| r["spec"]["subject"] == json!(BOB))
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "bob's refusal was not recorded: {:?}",
        records.items
    );

    let it = mine[0];
    assert_eq!(it["spec"]["kind"], json!("refused"));
    assert_eq!(it["spec"]["verb"], json!("read"));
    assert_eq!(it["spec"]["target"], json!("projects/p1/networks/n1"));
    // The same sentence, not a paraphrase: an audit line an operator has to
    // correlate by hand against what somebody actually saw is one they stop
    // trusting.
    assert_eq!(
        it["spec"]["detail"].as_str().unwrap_or_default(),
        refused.to_string(),
    );
}

/// Hammering a forbidden path does not fill the store.
///
/// A refusal is something an attacker can cause at will, so a record per
/// refusal would be a way to fill somebody's store from the outside. The id
/// collapses repeats within a minute: the exact count is lost and the fact is
/// not, which is the right way round.
#[tokio::test]
async fn a_burst_of_refusals_leaves_one_record_rather_than_a_thousand() {
    let api = cell().await;
    for _ in 0..50 {
        let _ = api.get(&name("projects/p1/networks/n1"), &who(BOB)).await;
    }

    let records = api
        .list_for("", "audit", &Filter::none(), &who(OPERATOR))
        .await
        .expect("an operator reads the audit");
    let mine = records
        .items
        .iter()
        .filter(|r| r["spec"]["subject"] == json!(BOB))
        .count();
    assert_eq!(mine, 1, "fifty attempts left {mine} records");
}

/// A tenant sees nothing in the audit.
///
/// It carries the names of projects and people that are not theirs, which is
/// the opposite of what an audit trail is for.
///
/// The exception, and its two exact edges, are in
/// `a_tenant_reads_the_refusals_about_their_own_objects_and_nobody_elses`: a
/// record is readable by whoever may read **what it is about**, and by **the
/// person it is about**. This is the other side of that rule — everything
/// else stays the operator's, including a refusal about somebody else's
/// project and a sign-in, which is about no object at all.
///
/// **Empty rather than refused**, and that is this API's rule for every list:
/// a `403` on a collection would be an oracle for what is in it, and a caller
/// who may see none of it is told the same thing as one looking at a cell
/// where nothing has happened.
#[tokio::test]
async fn a_tenant_sees_nothing_in_the_audit() {
    let api = cell().await;
    // A refusal in *bob's* project, and one about nobody's object at all. Ada
    // may read neither, and is neither.
    let _ = api.get(&name("projects/p2/networks/n1"), &who(ADA)).await;
    let _ = api.get(&name("nodes/node-a"), &who(BOB)).await;
    assert!(
        !api.list_for("", "audit", &Filter::none(), &who(OPERATOR))
            .await
            .expect("an operator reads the audit")
            .items
            .is_empty(),
        "nothing was recorded, so this check proves nothing"
    );

    let seen = api
        .list_for("", "audit", &Filter::none(), &who(ADA))
        .await
        .expect("a list is filtered, never refused");
    assert!(
        seen.items
            .iter()
            .all(|r| r["spec"]["subject"] == json!(ADA)),
        "a tenant was shown refusals that are neither theirs nor about their \
         own objects: {:?}",
        seen.items
    );
    // And what she does see is only her own reaching, never bob's.
    assert!(
        !seen.items
            .iter()
            .any(|r| r["spec"]["target"] == json!("nodes/node-a")),
        "a tenant was shown a refusal about a cell-wide object: {:?}",
        seen.items
    );
}

/// Listing a collection does not write an audit record per object.
///
/// A regression this suite exists to catch. The audit records refusals, and a
/// list filters rather than refusing — but the list path once ran every object
/// through the same gate that records. One tenant listing a cell of four
/// hundred guests would have written four hundred records about objects they
/// never asked for by name, and the collection this is meant to make readable
/// would have been the first thing to become unreadable.
#[tokio::test]
async fn listing_a_collection_does_not_fill_the_audit_with_what_was_filtered() {
    let api = cell().await;
    for i in 1..=6 {
        api.create(
            "projects/p1",
            "networks",
            &json!({"id": format!("n{i}"), "spec": {"vni": 5000 + i, "mtu": 1500}}),
            &who(ADA),
        )
        .await
        .expect("ada may create in her own project");
    }

    // Bob may see none of them. He gets an empty list, not six refusals.
    let seen = api
        .list_for("", "networks", &Filter::none(), &who(BOB))
        .await
        .expect("a list is filtered, never refused");
    assert!(seen.items.is_empty(), "{:?}", seen.items);

    let records = api
        .list_for("", "audit", &Filter::none(), &who(OPERATOR))
        .await
        .expect("an operator reads the audit");
    let about_bob = records
        .items
        .iter()
        .filter(|r| r["spec"]["subject"] == json!(BOB))
        .count();
    assert_eq!(
        about_bob, 0,
        "listing wrote {about_bob} audit records about objects nobody asked for by name"
    );
}
