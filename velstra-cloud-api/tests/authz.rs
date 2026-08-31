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

    // A cell with somewhere to put a volume. It used to have none, and the
    // volumes these tests make were accepted anyway — which is exactly the
    // silence that was fixed: a volume naming a pool nothing holds is never
    // provisioned and never says so.
    for pool in ["pool-a", "pool-b"] {
        api.create(
            "",
            "pools",
            &json!({ "id": pool, "spec": {} }),
            &who(OPERATOR),
        )
        .await
        .expect("a cell has somewhere to put a volume");
    }

    for (project, admin) in [("p1", ADA), ("p2", BOB)] {
        api.create(
            "",
            "projects",
            &json!({
                "id": project,
                "spec": {
                    "quota": {},
                    // Said out loud, because a project created today is closed:
                    // these tests are about quotas and bindings, not about what
                    // the cell allowed the tenant, and a fixture that left it
                    // out would be testing the policy by accident.
                    "policy": { "floating_ips": true, "device_passthrough": true },
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
        his.items.iter().all(|r| r["spec"]["subject"] == json!(BOB)),
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
        policy: Default::default(),
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
        &json!({"id": "sha256-abc", "spec": {"source_url": "http://x.invalid/i.qcow2", "digest": "sha256:abc"}}),
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
        &json!({"id": "sha256-def", "spec": {"source_url": "http://x.invalid/i.qcow2", "digest": "sha256:def"}}),
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
        !seen
            .items
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

/// The image catalogue: everybody may boot from it, only the cell may fill it.
///
/// Until this rule existed the platform had no notion of a public image at all.
/// Images live under projects, so an operator registering Debian registered it
/// for *one* tenant; every other project had to fetch and store its own copy of
/// the same bytes, and nobody opening the console could see what was on offer
/// before they had already put something there. "I do not see at a glance which
/// images there are" was not a gap in the console — there was nothing to show.
#[tokio::test]
async fn a_cell_image_is_everybodys_to_boot_and_nobodys_to_write() {
    let api = cell().await;

    api.create(
        "",
        "images",
        &json!({
            "id": "sha256-3f9a2b",
            "spec": {
                "digest": "sha256:3f9a2b",
                "format": "Qcow2",
                "source_url": "https://example.invalid/debian-13.qcow2"
            }
        }),
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("an operator publishes to the catalogue");

    // A tenant may read it — that is what makes it a catalogue.
    api.get(&name("images/sha256-3f9a2b"), &who(ADA))
        .await
        .expect("a tenant may read the catalogue");
    api.get(&name("images/sha256-3f9a2b"), &who(BOB))
        .await
        .expect("so may every other tenant");

    // And boot from it: the reference out of their project into the cell is
    // followed, where before it was refused as a cell-wide resource.
    api.create(
        "projects/p1",
        "instances",
        &json!({
            "id": "i1",
            "spec": { "image": "images/sha256-3f9a2b", "vcpus": 1, "memory_mib": 512 }
        }),
        &who(ADA),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("a tenant boots a catalogue image");

    // Nobody but the cell puts anything there. This is the half of the question
    // that has a security answer: an image is bytes every guest in the cell will
    // execute.
    let refused = api
        .create(
            "",
            "images",
            &json!({
                "id": "sha256-beef",
                "spec": {
                    "digest": "sha256:beef",
                    "format": "Qcow2",
                        "source_url": "https://example.invalid/mine.qcow2"
                }
            }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a tenant published to the cell catalogue");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // Nor may one be deleted or changed by a tenant: read is the whole grant.
    let refused = api
        .patch(
            &name("images/sha256-3f9a2b"),
            &json!({ "spec": { "source_url": "https://example.invalid/other" } }),
            None,
            &who(ADA),
        )
        .await
        .expect_err("a tenant rewrote a catalogue image");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // A tenant's own image stays their own: the catalogue rule is about the
    // cell's images, not about everybody's.
    api.create(
        "projects/p2",
        "images",
        &json!({
            "id": "sha256-cafe",
            "spec": {
                "digest": "sha256:cafe",
                "format": "Qcow2",
                "source_url": "https://example.invalid/private.qcow2"
            }
        }),
        &who(BOB),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("a tenant may keep a private image");
    let refused = api
        .get(&name("projects/p2/images/sha256-cafe"), &who(ADA))
        .await
        .expect_err("one tenant read another's image");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");
}

/// A way into a guest is a permission question, answered here and only here.
///
/// The node has no bindings to read and must never be the place one is decided,
/// so what it is told is the *answer*: whether this holder may type. A viewer
/// gets a window, not a refusal — watching a console is reading a machine, and
/// somebody who may read it should be able to see why it will not boot.
#[tokio::test]
async fn a_console_is_granted_to_a_reader_and_a_keyboard_only_to_a_writer() {
    let (api, store) = cell_and_store().await;

    // A guest a node has claimed. The status is the node's to write, so it is
    // written the way a node writes it — past the API, like the snapshot tests
    // above.
    let instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    > = TypedStore::new(store.clone(), "cell-1", "instances");
    let mut instance = velstra_cloud_model::resources::Instance::new(
        velstra_cloud_model::meta::Meta::new(
            "projects/p1/instances/i1".parse().unwrap(),
            velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
        ),
        Default::default(),
        velstra_cloud_model::resources::InstanceStatus {
            node: Some("node-a".into()),
            ..Default::default()
        },
    );
    instance.meta.generation = 1;
    instances
        .create(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .expect("a claimed guest");

    // Ada admins p1: a keyboard.
    let opened = api
        .open_console(&name("projects/p1/instances/i1"), velstra_cloud_model::console::ConsoleKind::Serial, &who(ADA))
        .await
        .map_err(|e| e.to_string())
        .expect("an admin of the project may open a console");
    assert_eq!(opened["readOnly"], serde_json::json!(false));
    let ticket = opened["ticket"].as_str().expect("a ticket").to_string();
    assert!(!ticket.is_empty());

    // The ticket left the API exactly once. What is stored is its hash — every
    // node in the cell may read the cell, and a stored ticket would be a way
    // into a guest on somebody else's machine.
    let session = api
        .get(
            &name(opened["session"].as_str().expect("a session name")),
            &who(OPERATOR),
        )
        .await
        .map_err(|e| e.to_string())
        .expect("the session exists");
    let text = serde_json::to_string(&session).unwrap();
    assert!(!text.contains(&ticket), "the ticket was stored: {text}");
    assert!(
        text.contains(&velstra_cloud_model::console::sha256_hex(&ticket)),
        "{text}"
    );
    // And it says which node, because that is what stops one node serving a
    // session for a guest on another.
    assert_eq!(session["spec"]["node"], serde_json::json!("node-a"));

    // A viewer of the project gets a window and no keyboard. Not a refusal:
    // somebody who may read a machine should be able to see why it will not
    // boot, which is the whole reason a console exists.
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "admin", "members": [ADA] },
            { "role": "viewer", "members": ["cleo"] }
        ] } }),
        None,
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("an operator changes the bindings");

    let watching = api
        .open_console(&name("projects/p1/instances/i1"), velstra_cloud_model::console::ConsoleKind::Serial, &who("cleo"))
        .await
        .map_err(|e| e.to_string())
        .expect("a viewer may watch");
    assert_eq!(
        watching["readOnly"],
        serde_json::json!(true),
        "a viewer was handed a keyboard"
    );
    assert_ne!(
        watching["ticket"], opened["ticket"],
        "two sessions shared a ticket"
    );

    // Bob has nothing in p1 at all: no window either.
    let refused = api
        .open_console(&name("projects/p1/instances/i1"), velstra_cloud_model::console::ConsoleKind::Serial, &who(BOB))
        .await
        .expect_err("a stranger opened a console");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");
}

/// Who may put a guest on the machine's own wire.
///
/// The strongest thing an operator can hand out, and the one a tenant most
/// wants: "just put this VM on my LAN". A guest on a host bridge is on whatever
/// the machine is on — past this platform's addressing, its security groups and
/// its idea of who may talk to whom. So the operator decides, per network, and a
/// tenant gets logical networks.
#[tokio::test]
async fn only_an_operator_may_put_a_network_on_the_machines_own_wire() {
    let api = cell().await;

    let refused = api
        .create(
            "projects/p1",
            "networks",
            &json!({ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a tenant put a network on a host bridge");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");
    assert!(
        refused.to_string().contains("host bridge"),
        "the refusal has to say what was refused: {refused}"
    );

    // The operator may, in the tenant's own project — which is the shape of the
    // answer: the admin decides, the tenant uses.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }),
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("an operator may");

    // And a tenant's ordinary network is still theirs to make.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "sha256-cafe", "spec": { "mtu": 1500 } }),
        &who(ADA),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("a tenant may still have a logical network");

    // Nor by editing one afterwards, which is the same grant arriving later.
    let refused = api
        .patch(
            &name("projects/p1/networks/private"),
            &json!({ "spec": { "host_bridge": "br0" } }),
            None,
            &who(ADA),
        )
        .await
        .expect_err("a tenant moved their network onto a host bridge");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");
}

/// What a project may reach for is the cell's to decide, per project.
///
/// This is the difference between a platform one company runs for itself and
/// one a provider runs for customers. Most of what a hypervisor can do is not
/// something every tenant should be able to ask for, and until this existed
/// each of those was answered the same way everywhere — cell operator or
/// nobody — which is right for one tenant and useless for a hundred.
#[tokio::test]
async fn a_project_reaches_only_as_far_as_the_cell_let_it() {
    let (api, _store) = cell_and_store().await;

    // p1's fixture allows passthrough and public addresses and no bridges. p2's
    // is the same; what differs below is what each is *given*.
    let closed = api
        .create(
            "projects/p1",
            "networks",
            &json!({ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a project with no bridges was given one");
    assert_eq!(closed.code, Code::PermissionDenied, "{closed}");
    assert!(
        closed.to_string().contains("hostBridges"),
        "the refusal has to say where the answer lives: {closed}"
    );

    // The cell grants it — naming the bridge, because what anybody means by a
    // host bridge is a particular wire.
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "policy": { "host_bridges": ["br0"], "floating_ips": true } } }),
        None,
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("the cell sets what a project may reach for");

    // Now the tenant's own admin may make one — without ever being a cell
    // operator, which is the whole shape of the thing.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }),
        &who(ADA),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("a project given a bridge may use it");

    // And only that one. A grant is a wire, not a capability.
    let other = api
        .create(
            "projects/p1",
            "networks",
            &json!({ "id": "other", "spec": { "mtu": 1500, "host_bridge": "br99" } }),
            &who(ADA),
        )
        .await
        .err()
        .expect("a project used a bridge it was not given");
    assert_eq!(other.code, Code::PermissionDenied, "{other}");
    assert!(other.to_string().contains("br0"), "{other}");

    // The neighbour got nothing, and one tenant's grant is not another's.
    let bob = api
        .create(
            "projects/p2",
            "networks",
            &json!({ "id": "lan", "spec": { "mtu": 1500, "host_bridge": "br0" } }),
            &who(BOB),
        )
        .await
        .err()
        .expect("a grant leaked between projects");
    assert_eq!(bob.code, Code::PermissionDenied, "{bob}");
}

/// The escalation this policy has to be closed against.
///
/// Every check that consults the policy is decorative if the tenant can write
/// the policy: a project admin who could set `hostBridges` would grant
/// themselves the machine's own wire by editing the object that says they may
/// not.
#[tokio::test]
async fn a_project_admin_cannot_widen_their_own_project() {
    let api = cell().await;

    let refused = api
        .patch(
            &name("projects/p1"),
            &json!({ "spec": { "policy": { "host_bridges": ["br0"] } } }),
            None,
            &who(ADA),
        )
        .await
        .expect_err("a project admin widened their own project");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // Their own bindings they may still change — that is what being the
    // project's admin is.
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "admin", "members": [ADA] },
            { "role": "operator", "members": ["cleo"] }
        ] } }),
        None,
        &who(ADA),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("a project admin manages their own bindings");
}

/// The rung a platform with customers needs.
///
/// Somebody who keeps an estate running reboots machines and does not delete
/// them. Before this rung, anybody who needed to restart a guest had to be
/// given the ability to destroy one.
#[tokio::test]
async fn an_operator_may_run_a_guest_and_not_create_or_destroy_one() {
    let api = cell().await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "admin", "members": [ADA] },
            { "role": "operator", "members": ["cleo"] }
        ] } }),
        None,
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("the bindings are set");

    // Not theirs to bring into existence.
    let refused = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "i1", "spec": { "vcpus": 1, "memory_mib": 512 } }),
            &who("cleo"),
        )
        .await
        .err()
        .expect("an operator created a guest");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // The admin makes it; the operator runs it.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "i1", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("an admin creates a guest");

    api.patch(
        &name("projects/p1/instances/i1"),
        &json!({ "spec": { "desired_state": "Stopped" } }),
        None,
        &who("cleo"),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("an operator stops a guest");

    // And not theirs to take away.
    let refused = api
        .delete(&name("projects/p1/instances/i1"), None, &who("cleo"))
        .await
        .err()
        .expect("an operator deleted a guest");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");

    // A viewer may not even stop it.
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "admin", "members": [ADA] },
            { "role": "viewer", "members": ["dana"] }
        ] } }),
        None,
        &who(OPERATOR),
    )
    .await
    .map_err(|e| e.to_string())
    .expect("the bindings are set");
    let refused = api
        .patch(
            &name("projects/p1/instances/i1"),
            &json!({ "spec": { "desired_state": "Running" } }),
            None,
            &who("dana"),
        )
        .await
        .expect_err("a viewer started a guest");
    assert_eq!(refused.code, Code::PermissionDenied, "{refused}");
}

#[tokio::test]
async fn a_tenant_is_refused_the_machine_room_rather_than_shown_an_empty_one() {
    // Found by signing in as a customer and asking the questions a customer's
    // console asks. Every cell-wide collection answered **200 with an empty
    // list** — so a tenant on a cell with one node, one pool and two users was
    // told, in three separate answers, that it had none of each.
    //
    // Filtering is right where a tenant has objects of their own: `images` is a
    // catalogue everybody may boot, `projects` shows them theirs, and an empty
    // answer there means "none of yours". It is wrong where no object will ever
    // pass, because then the empty list is not a filter result — it is a
    // statement about the cell, and it is false.
    let api = cell().await;
    for kind in ["nodes", "pools", "device-classes", "users"] {
        let refused = api.list_for("", kind, &Filter::none(), &who(ADA)).await;
        let Err(e) = refused else {
            panic!("{kind} was listed to a tenant instead of refused");
        };
        assert_eq!(e.code, Code::PermissionDenied, "{kind}: {e}");
        assert!(
            e.to_string().contains("not an empty list"),
            "{kind}: {e} — the refusal has to say that, or it reads as 'there are none'"
        );
    }
    // And the ones a tenant genuinely has a stake in still answer.
    api.list_for("", "images", &Filter::none(), &who(ADA))
        .await
        .expect("a catalogue is everybody's to read");
    api.list_for("", "projects", &Filter::none(), &who(ADA))
        .await
        .expect("a tenant lists their own projects");
}

#[tokio::test]
async fn a_service_account_carries_a_token_and_signs_in_with_nothing() {
    // A service account used to be a line in a static token file: no object, no
    // bindings, nothing in the audit trail, and no way to take one away except
    // by editing a file and restarting the API. So the thing every automated
    // caller needs — an identity a project can grant something to — did not
    // exist, and the answer was to hand out a person's password.
    let api = cell().await;
    api.create(
        "",
        "users",
        &json!({ "id": "ci", "spec": { "service": true }}),
        &who(OPERATOR),
    )
    .await
    .expect("a cell operator creates a service account");

    // It has no password, and the refusal says what to do instead rather than
    // just refusing.
    // (The password refusal lives on the REST handler, where the account is
    // read; `tests/rest.rs` covers it from the outside.)

    // A token, minted by an operator and shown once.
    let token = api
        .identity()
        .mint_service_credential("ci", "nightly backups")
        .await
        .expect("an operator mints a token");
    let identity = api
        .identity()
        .identify_service(&token)
        .await
        .expect("the token names its account");
    assert_eq!(identity.subject, "ci");

    // And disabling the account stops it — read back on every request, not
    // copied into the credential, so an operator shutting a door does not have
    // to hunt for tokens down first.
    api.patch(
        &name("users/ci"),
        &json!({ "spec": { "disabled": true }}),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("an operator disables it");
    assert!(
        api.identity().identify_service(&token).await.is_err(),
        "a disabled service account's token still worked"
    );
}

#[tokio::test]
async fn a_tenant_boots_the_newest_of_a_family_like_anybody_else() {
    // Found live, as the customer the feature exists for. `families/debian-13`
    // was judged as a reference before it was resolved, parsed as a two-segment
    // name of a collection no project governs, and the create answered "this is
    // a cell-wide resource; only a cell operator may touch it" — a refusal
    // about a name that names nothing. The family is resolved first now, and
    // what gets authorised is the resolved image, which is the question that
    // means something.
    let api = cell().await;
    api.create(
        "",
        "images",
        &json!({ "spec": {
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "format": "Qcow2",
            "family": "debian-13",
            "source_url": "http://x.invalid/d.qcow2"
        }}),
        &who(OPERATOR),
    )
    .await
    .expect("the operator stocks the catalogue");

    let made = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "vom-katalog", "spec": {
                "image": "families/debian-13",
                "vcpus": 1, "memory_mib": 512, "root_disk_gib": 2, "ports": []
            }}),
            &who(ADA),
        )
        .await
        .expect("a tenant boots the newest of a family");
    let _ = made;
    let stored: velstra_cloud_model::resources::Instance = api
        .typed(&name("projects/p1/instances/vom-katalog"))
        .await
        .unwrap();
    assert!(
        stored.spec.image.starts_with("images/debian-13-"),
        "the family was not resolved to the catalogue image: {}",
        stored.spec.image
    );
}


#[tokio::test]
async fn the_catalogue_can_be_listed_and_not_only_guessed() {
    // `families/debian-13` is the reference somebody is supposed to write, and
    // for a long time the only way to learn one existed was to guess it and read
    // the refusal — which helpfully lists them. A picker cannot offer what
    // nothing can enumerate, so the console showed people
    // `images/debian-13-d2af37c5` and let them pin themselves to one build.
    let api = cell().await;
    for (digest, family) in [("aa", "debian-13"), ("bb", "ubuntu-24-04")] {
        api.create(
            "",
            "images",
            &json!({ "spec": {
                "source_url": "http://x.invalid/i.qcow2", "digest": format!("sha256:{}", digest.repeat(32)),
                "format": "Qcow2",
                "family": family,
                "source_url": "http://x.invalid/d.qcow2"
            }}),
            &who(OPERATOR),
        )
        .await
        .expect("the operator stocks the catalogue");
    }

    // The customer's seat: the catalogue is what publishing one is *for*.
    let listed = api
        .list_for(
            "",
            "families",
            &velstra_cloud_api::Filter::none(),
            &who(ADA),
        )
        .await
        .expect("a tenant reads the catalogue");
    let names: Vec<String> = listed
        .items
        .iter()
        .map(|i| i["meta"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["families/debian-13", "families/ubuntu-24-04"],
        "the catalogue did not list what a create can resolve"
    );
    assert!(
        listed.items.iter().all(|i| i["spec"]["public"] == json!(true)),
        "a cell image is the catalogue's and everybody may boot it"
    );
    // Every entry names bytes that exist — a picker offering a family that
    // resolves to nothing is worse than no picker.
    for entry in &listed.items {
        let image = entry["spec"]["image"].as_str().unwrap();
        let found: Result<velstra_cloud_model::resources::Image, _> =
            api.typed(&name(image)).await;
        assert!(
            found.is_ok(),
            "the catalogue offered {image}, which is not there"
        );
    }
}

#[tokio::test]
async fn a_projects_own_family_shadows_the_catalogues_in_the_listing_too() {
    // Resolution already prefers the project's own, so a listing that showed the
    // cell's would be a picker whose entries lie about what they boot.
    let api = cell().await;
    for (parent, digest, version) in [
        ("", "aa", "20260101"),
        ("projects/p1", "bb", "our-build"),
    ] {
        api.create(
            parent,
            "images",
            &json!({ "spec": {
                "source_url": "http://x.invalid/i.qcow2", "digest": format!("sha256:{}", digest.repeat(32)),
                "format": "Qcow2",
                "family": "debian-13",
                "version": version,
                "source_url": "http://x.invalid/d.qcow2"
            }}),
            &who(if parent.is_empty() { OPERATOR } else { ADA }),
        )
        .await
        .expect("both are published");
    }

    let listed = api
        .list_for(
            "projects/p1",
            "families",
            &velstra_cloud_api::Filter::none(),
            &who(ADA),
        )
        .await
        .expect("a tenant reads their own catalogue");
    assert_eq!(listed.items.len(), 1, "one family, whichever wins");
    let entry = &listed.items[0];
    assert_eq!(entry["spec"]["version"], json!("our-build"));
    assert_eq!(
        entry["spec"]["public"],
        json!(false),
        "an image under a project is that project's alone"
    );
}

#[tokio::test]
async fn a_guest_goes_on_a_named_network_without_anybody_making_a_port() {
    // A port is a join — this guest, that network, this address — which is right
    // in the model and wrong in a form. Asked for one machine on their own
    // network, a customer had to make the port themselves: knowing a port
    // exists, that it hangs off a subnet rather than a network, and that it has
    // to be made before the guest and not after.
    let api = cell().await;
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "prod", "spec": { "mtu": 1450, "vni": 0 }}),
        &who(ADA),
    )
    .await
    .expect("a tenant makes a network");
    api.create(
        "projects/p1",
        "subnets",
        &json!({ "id": "prod", "spec": {
            "network": "projects/p1/networks/prod",
            "cidr": "10.60.0.0/24",
            "gateway": "10.60.0.1"
        }}),
        &who(ADA),
    )
    .await
    .expect("with a range on it");

    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "auf-prod", "spec": {
            "image": "images/x", "vcpus": 1, "memory_mib": 512, "root_disk_gib": 2,
            "networks": ["projects/p1/networks/prod"]
        }}),
        &who(ADA),
    )
    .await
    .expect("and puts a machine on it by naming it");

    let stored: velstra_cloud_model::resources::Instance = api
        .typed(&name("projects/p1/instances/auf-prod"))
        .await
        .unwrap();
    assert_eq!(stored.spec.ports.len(), 1, "no port was minted");
    assert!(
        stored.spec.networks.is_empty(),
        "`networks` is a request, not a record — two fields describing one set \
         of interfaces is two fields that drift"
    );
    let port: velstra_cloud_model::resources::Port =
        api.typed(&name(&stored.spec.ports[0])).await.unwrap();
    assert_eq!(port.spec.network, "projects/p1/networks/prod");
    assert_eq!(
        port.spec.subnet, "projects/p1/subnets/prod",
        "the port was not put on the network's own subnet"
    );
}

#[tokio::test]
async fn naming_a_network_and_a_port_at_once_is_refused_rather_than_merged() {
    // Two answers to one question. Picking one silently is how somebody ends up
    // with a machine on a network they did not ask for.
    let api = cell().await;
    let refusal = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "beides", "spec": {
                "image": "images/x", "vcpus": 1, "memory_mib": 512, "root_disk_gib": 2,
                "networks": ["projects/p1/networks/prod"],
                "ports": ["projects/p1/ports/p"]
            }}),
            &who(ADA),
        )
        .await
        .err()
        .expect("both at once is not a request anybody can answer");
    assert_eq!(refusal.code, velstra_cloud_api::Code::InvalidArgument);
    assert!(
        refusal.to_string().contains("not both"),
        "the refusal does not say what to do: {refusal}"
    );
}

#[tokio::test]
async fn a_network_in_somebody_elses_project_is_not_one_to_mint_on() {
    // The minting path does not authorise — it is for objects the platform
    // decided on — so a name from somewhere else would make a port in a
    // stranger's project, on their quota, at their request.
    let api = cell().await;
    let refusal = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "fremd", "spec": {
                "image": "images/x", "vcpus": 1, "memory_mib": 512, "root_disk_gib": 2,
                "networks": ["projects/p2/networks/default"]
            }}),
            &who(ADA),
        )
        .await
        .err()
        .expect("a guest cannot be put on another project's network");
    assert!(
        refusal.to_string().contains("its own project"),
        "wrong refusal: {refusal}"
    );
    let strangers = api
        .list_for(
            "projects/p2",
            "ports",
            &velstra_cloud_api::Filter::none(),
            &who(OPERATOR),
        )
        .await
        .unwrap();
    assert!(
        strangers.items.is_empty(),
        "a port was minted in a project the caller does not hold"
    );
}

#[tokio::test]
async fn a_network_with_no_subnet_is_refused_before_the_guest_exists() {
    // A port on a network with no range gets no address, and the guest boots
    // with a dead NIC and no sign of why — the quietest failure this path has.
    let api = cell().await;
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "leer", "spec": { "mtu": 1450, "vni": 0 }}),
        &who(ADA),
    )
    .await
    .expect("a network with nothing on it");
    let refusal = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "ohne-bereich", "spec": {
                "image": "images/x", "vcpus": 1, "memory_mib": 512, "root_disk_gib": 2,
                "networks": ["projects/p1/networks/leer"]
            }}),
            &who(ADA),
        )
        .await
        .err()
        .expect("a network with no subnet cannot carry a port");
    assert!(
        refusal.to_string().contains("no subnet"),
        "wrong refusal: {refusal}"
    );
}

// ---- folders ---------------------------------------------------------------

/// A folder, as an operator makes one.
async fn folder(api: &Api, id: &str, parent: &str, role: &str, member: &str) {
    api.create(
        "",
        "folders",
        &json!({ "id": id, "spec": {
            "display_name": id,
            "parent": parent,
            "bindings": [{ "role": role, "members": [member] }]
        }}),
        &who(OPERATOR),
    )
    .await
    .unwrap_or_else(|e| panic!("making {id}: {e}"));
}

#[tokio::test]
async fn a_role_granted_on_a_folder_reaches_the_projects_under_it() {
    // `ProjectSpec.parent` has said since the beginning that it names the
    // parent "policies are inherited from, kept as a name so the hierarchy is
    // walked, not guessed". Nothing walked it. A customer could set it, the
    // console showed it, and it changed nothing about who could do what — a
    // field that was a promise the platform did not keep.
    let api = cell().await;
    folder(&api, "engineering", "", "editor", "ada").await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "parent": "engineering" }}),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("a project goes into a folder");

    // Ada holds nothing on p1 itself.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "durch-den-ordner", "spec": { "vni": 5001, "mtu": 1500 }}),
        &who("ada"),
    )
    .await
    .expect("a role granted on the folder reaches the project");
}

#[tokio::test]
async fn a_role_reaches_all_the_way_down_a_chain() {
    let api = cell().await;
    folder(&api, "alles", "", "editor", "ada").await;
    folder(&api, "eng", "alles", "viewer", "bob").await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "parent": "eng" }}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    // Two levels up, and it still counts.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "von-ganz-oben", "spec": { "vni": 5002, "mtu": 1500 }}),
        &who("ada"),
    )
    .await
    .expect("a role two levels up did not reach");

    // And the nearer one grants only what it grants: roles add up, nothing
    // subtracts, but a viewer is still a viewer.
    let refused = api
        .create(
            "projects/p1",
            "networks",
            &json!({ "id": "nur-gucken", "spec": { "vni": 5003, "mtu": 1500 }}),
            &who("bob"),
        )
        .await;
    assert!(refused.is_err(), "a viewer created something");
}

#[tokio::test]
async fn a_project_outside_the_folder_is_untouched() {
    // The whole risk of this feature in one test. A grant that reaches further
    // than the tree says is a tenant looking at somebody else's estate, and it
    // is the kind of mistake that is invisible until it is a breach.
    let api = cell().await;
    folder(&api, "engineering", "", "admin", "ada").await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "parent": "engineering" }}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    let refused = api
        .list_for(
            "projects/p2",
            "instances",
            &velstra_cloud_api::Filter::none(),
            &who("ada"),
        )
        .await;
    assert!(
        refused.is_err(),
        "a folder grant reached a project that is not in it"
    );
}

#[tokio::test]
async fn a_folder_with_a_project_in_it_cannot_be_tidied_away() {
    // Written the other way round first, to prove that deleting a folder does
    // not take a tenant's own access with it — and the delete was refused, which
    // is the better answer. Housekeeping above somebody cannot silently remove
    // every role granted there, because it cannot happen at all while anything
    // is inside.
    //
    // The walk is forgiving anyway, for the case this cannot reach: a store
    // restored, edited by hand, or written by a version without the guard. See
    // `hierarchy::walking_upward::a_folder_that_is_gone_does_not_take_a_tenant
    // _down_with_it` — a missing folder grants nothing and ends the walk rather
    // than refusing, so the project's own bindings still govern it.
    let api = cell().await;
    folder(&api, "weg", "", "viewer", "bob").await;
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "parent": "weg" }}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    let refusal = api
        .delete(&name("folders/weg"), None, &who(OPERATOR))
        .await
        .err()
        .expect("a folder with a project in it was deleted");
    assert!(
        refusal.to_string().contains("projects/p1"),
        "the refusal does not say what is inside: {refusal}"
    );
}

#[tokio::test]
async fn a_parent_that_names_something_other_than_a_folder_is_refused() {
    // `parent` was a free string nothing read, so any text was as good as any
    // other. Now that it decides who may do what, a value that names nothing is
    // a grant that silently does not apply — and one that names a *project*
    // would be a walk climbing sideways into somebody's tenancy.
    let api = cell().await;
    for bad in ["projects/p2", "organizations/o1", "folders/gibt-es-nicht"] {
        let refusal = api
            .patch(
                &name("projects/p1"),
                &json!({ "spec": { "parent": bad }}),
                None,
                &who(OPERATOR),
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("`{bad}` was accepted as a parent"));
        let said = refusal.to_string();
        assert!(
            said.contains("is not one") || said.contains("no folder called"),
            "wrong refusal for `{bad}`: {said}"
        );
    }
}

#[tokio::test]
async fn a_folder_cannot_be_put_inside_itself() {
    // A loop is not an error the read path would ever report: the walk is
    // bounded, so it would simply stop somewhere arbitrary, and a folder's
    // grandparent would quietly be itself.
    let api = cell().await;
    folder(&api, "a", "", "viewer", "ada").await;
    folder(&api, "b", "a", "viewer", "ada").await;

    let refusal = api
        .patch(
            &name("folders/a"),
            &json!({ "spec": { "parent": "b" }}),
            None,
            &who(OPERATOR),
        )
        .await
        .expect_err("a loop was written down");
    assert!(
        refusal.to_string().contains("inside itself"),
        "wrong refusal: {refusal}"
    );
}

#[tokio::test]
async fn a_folder_with_something_in_it_is_not_deleted_out_from_under_it() {
    // Otherwise tidying up above somebody silently takes every role granted
    // there away from every project below, with nothing said.
    let api = cell().await;
    folder(&api, "eng", "", "editor", "ada").await;
    folder(&api, "team", "eng", "viewer", "bob").await;

    let refusal = api
        .delete(&name("folders/eng"), None, &who(OPERATOR))
        .await
        .err()
        .expect("a folder with a folder in it was deleted");
    assert!(
        refusal.to_string().contains("folders/team"),
        "the refusal does not say what is inside: {refusal}"
    );
}

#[tokio::test]
async fn a_folder_is_the_cells_and_a_tenant_may_not_make_one() {
    // A tenant who could make a folder and put their project in it could grant
    // themselves anything, which is the whole of the access model undone.
    let api = cell().await;
    let refused = api
        .create(
            "",
            "folders",
            &json!({ "id": "meiner", "spec": { "display_name": "Meiner" }}),
            &who(ADA),
        )
        .await;
    assert!(refused.is_err(), "a tenant made a folder");
}

#[tokio::test]
async fn an_operator_publishes_a_tenants_image_without_retyping_it() {
    // A tenant captures a guest and gets an image in their own project. Putting
    // it in the cell's catalogue meant reading its digest, its format, its size
    // and its source off one object and typing them into another — correctly, or
    // publishing bytes nobody tested.
    //
    // Nothing is copied. An image is content-addressed, so the same digest is
    // the same bytes: every node that had them cached still has them.
    let api = cell().await;
    let digest = format!("sha256:{}", "ab".repeat(32));
    api.create(
        "projects/p1",
        "images",
        &json!({ "id": "unser-basis", "spec": {
            "digest": digest,
            "format": "Qcow2",
            "family": "our-base",
            "version": "20260829",
            "size_bytes": 4_294_967_296u64,
            "source_url": "http://x.invalid/base.qcow2"
        }}),
        &who(ADA),
    )
    .await
    .expect("a tenant's own image");

    api.create(
        "",
        "images",
        &json!({ "id": "veroeffentlicht", "spec": {
            "from": "projects/p1/images/unser-basis"
        }}),
        &who(OPERATOR),
    )
    .await
    .expect("an operator publishes it");

    let published: velstra_cloud_model::resources::Image =
        api.typed(&name("images/veroeffentlicht")).await.unwrap();
    assert_eq!(published.spec.digest, digest, "the bytes are not the same bytes");
    assert_eq!(published.spec.family, "our-base");
    assert_eq!(published.spec.version, "20260829");
    assert_eq!(published.spec.size_bytes, 4_294_967_296);
    assert!(
        published.spec.from.is_empty(),
        "`from` is a request, not a record — an image that remembered where it \
         was published from would be one whose source can be deleted"
    );

    // And it is the catalogue's now: everybody may boot it.
    let listed = api
        .list_for("", "families", &velstra_cloud_api::Filter::none(), &who(ADA))
        .await
        .unwrap();
    assert!(
        listed
            .items
            .iter()
            .any(|f| f["spec"]["family"] == json!("our-base") && f["spec"]["public"] == json!(true)),
        "the published image is not in the catalogue"
    );
}

#[tokio::test]
async fn publishing_under_another_name_is_the_point_of_publishing() {
    // Somebody promoting `our-base` out of a project usually wants it called
    // something the whole cell will recognise. What the caller says wins; only
    // what they leave out is copied.
    let api = cell().await;
    api.create(
        "projects/p1",
        "images",
        &json!({ "id": "roh", "spec": {
            "source_url": "http://x.invalid/i.qcow2", "digest": format!("sha256:{}", "cd".repeat(32)),
            "format": "Qcow2",
            "family": "our-base",
            "version": "roh",
            "source_url": "http://x.invalid/base.qcow2"
        }}),
        &who(ADA),
    )
    .await
    .unwrap();

    api.create(
        "",
        "images",
        &json!({ "id": "gehaertet", "spec": {
            "from": "projects/p1/images/roh",
            "family": "debian-13-hardened",
            "version": "1"
        }}),
        &who(OPERATOR),
    )
    .await
    .unwrap();

    let published: velstra_cloud_model::resources::Image =
        api.typed(&name("images/gehaertet")).await.unwrap();
    assert_eq!(published.spec.family, "debian-13-hardened");
    assert_eq!(published.spec.version, "1");
    assert_eq!(
        published.spec.digest,
        format!("sha256:{}", "cd".repeat(32)),
        "the bytes changed under the new name"
    );
}

#[tokio::test]
async fn a_tenant_cannot_publish_their_own_image_to_the_cell() {
    // The whole rule the catalogue rests on: anybody may boot from it, only the
    // cell may put something in it.
    let api = cell().await;
    api.create(
        "projects/p1",
        "images",
        &json!({ "id": "meins", "spec": {
            "source_url": "http://x.invalid/i.qcow2", "digest": format!("sha256:{}", "ef".repeat(32)),
            "format": "Qcow2",
            "family": "meins",
            "source_url": "http://x.invalid/x.qcow2"
        }}),
        &who(ADA),
    )
    .await
    .unwrap();

    let refused = api
        .create(
            "",
            "images",
            &json!({ "id": "meins", "spec": { "from": "projects/p1/images/meins" }}),
            &who(ADA),
        )
        .await;
    assert!(refused.is_err(), "a tenant published to the cell catalogue");
}

#[tokio::test]
async fn publishing_from_something_that_is_not_there_says_so() {
    let api = cell().await;
    let refusal = api
        .create(
            "",
            "images",
            &json!({ "id": "leer", "spec": { "from": "projects/p1/images/gibt-es-nicht" }}),
            &who(OPERATOR),
        )
        .await
        .err()
        .expect("published from nothing");
    assert!(
        refusal.to_string().contains("no image called"),
        "wrong refusal: {refusal}"
    );
}

// ---- roles the cell wrote down ---------------------------------------------

#[tokio::test]
async fn a_role_the_cell_wrote_down_reaches_exactly_as_far_as_it_says() {
    // The case a rung cannot express, end to end through the API: somebody who
    // may restart the database machines and must not touch the network.
    let api = cell().await;
    api.create(
        "",
        "roles",
        &json!({ "id": "db-operator", "spec": {
            "display_name": "Database operator",
            "grants": [{ "verb": "operate", "collections": ["instances"] }]
        }}),
        &who(OPERATOR),
    )
    .await
    .expect("the cell writes down a role");
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "roles/db-operator", "members": ["nina"] }
        ]}}),
        None,
        &who(OPERATOR),
    )
    .await
    .expect("and grants it");

    // Reading the machines: yes, because acting on them carries seeing them.
    api.list_for(
        "projects/p1",
        "instances",
        &velstra_cloud_api::Filter::none(),
        &who("nina"),
    )
    .await
    .expect("a role that may operate instances may see them");

    // The network: nothing at all.
    let refused = api
        .list_for(
            "projects/p1",
            "networks",
            &velstra_cloud_api::Filter::none(),
            &who("nina"),
        )
        .await;
    assert!(refused.is_err(), "a role reached past what it names");

    // And not a rung by another name: no creating, anywhere.
    let refused = api
        .create(
            "projects/p1",
            "instances",
            &json!({ "id": "neu", "spec": {
                "image": "images/x", "vcpus": 1, "memory_mib": 512,
                "root_disk_gib": 2, "ports": []
            }}),
            &who("nina"),
        )
        .await;
    assert!(refused.is_err(), "an operate grant created something");
}

#[tokio::test]
async fn a_binding_naming_a_role_that_is_not_there_is_refused_at_the_door() {
    // The strict half of a deliberate pair. The read path is lenient — a
    // `roles/…` nobody defined grants nothing, so a role deleted under a project
    // does not refuse every request in it. Neither of those tells anybody about
    // their typo. This is where they are told, while the tab is still open.
    let api = cell().await;
    let refusal = api
        .patch(
            &name("projects/p1"),
            &json!({ "spec": { "bindings": [
                { "role": "roles/gibt-es-nicht", "members": ["nina"] }
            ]}}),
            None,
            &who(OPERATOR),
        )
        .await
        .expect_err("a binding named a role that does not exist");
    assert!(
        refusal.to_string().contains("no role called"),
        "wrong refusal: {refusal}"
    );
}

#[tokio::test]
async fn a_role_that_grants_nothing_in_particular_is_refused() {
    // No wildcard, and no empty grant. A role that could mean *everything* would
    // be a second spelling of `admin` with no way to tell them apart in a list
    // of who may do what.
    let api = cell().await;
    for (what, spec) in [
        ("no grants", json!({ "display_name": "Leer" })),
        (
            "a grant over nothing",
            json!({ "grants": [{ "verb": "operate", "collections": [] }] }),
        ),
        (
            "a collection that does not exist",
            json!({ "grants": [{ "verb": "operate", "collections": ["maschinen"] }] }),
        ),
    ] {
        let refused = api
            .create("", "roles", &json!({ "id": "leer", "spec": spec }), &who(OPERATOR))
            .await;
        assert!(refused.is_err(), "{what} was accepted");
    }
}

#[tokio::test]
async fn a_tenant_cannot_write_down_a_role() {
    // Somebody who could define one could define one granting more than they
    // hold, and the whole point of keeping `Administer` apart is that an editor
    // cannot widen themselves.
    let api = cell().await;
    let refused = api
        .create(
            "",
            "roles",
            &json!({ "id": "meine", "spec": {
                "grants": [{ "verb": "write", "collections": ["instances"] }]
            }}),
            &who(ADA),
        )
        .await;
    assert!(refused.is_err(), "a tenant wrote down a role");
}

#[tokio::test]
async fn a_role_granted_on_a_folder_reaches_down_like_a_rung_does() {
    // The two features meet here, and it is the arrangement somebody would
    // actually build: one role, granted once, over an estate.
    let api = cell().await;
    api.create(
        "",
        "roles",
        &json!({ "id": "db-operator", "spec": {
            "grants": [{ "verb": "operate", "collections": ["instances"] }]
        }}),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "",
        "folders",
        &json!({ "id": "kunden", "spec": {
            "bindings": [{ "role": "roles/db-operator", "members": ["nina"] }]
        }}),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "parent": "kunden" }}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    api.list_for(
        "projects/p1",
        "instances",
        &velstra_cloud_api::Filter::none(),
        &who("nina"),
    )
    .await
    .expect("a role granted on a folder did not reach the project");
    assert!(
        api.list_for(
            "projects/p1",
            "networks",
            &velstra_cloud_api::Filter::none(),
            &who("nina"),
        )
        .await
        .is_err(),
        "inheriting a role widened it"
    );
}

/// The fleet's own questions are the fleet's.
///
/// `nodes:explainCapacity` and `nodes:explainCpu` answer with machine names,
/// free memory and CPU domains — the same facts listing the nodes would give,
/// which the same caller is refused. Both used to throw `who` away with a
/// comment claiming they were authorised; a tenant's overview rendered the
/// cell. Found by signing in as one.
#[tokio::test]
async fn the_fleet_reports_are_refused_to_whoever_may_not_list_the_fleet() {
    let api = cell().await;

    api.explain_capacity(&who(OPERATOR))
        .await
        .expect("an operator reads the fleet");
    api.explain_cpu(&who(OPERATOR))
        .await
        .expect("an operator reads the domains");

    for (what, verdict) in [
        ("capacity", api.explain_capacity(&who(ADA)).await.err()),
        ("cpu", api.explain_cpu(&who(ADA)).await.err()),
    ] {
        let refused = verdict.unwrap_or_else(|| panic!("a tenant read the fleet's {what}"));
        assert_eq!(refused.code, velstra_cloud_api::Code::PermissionDenied, "{refused}");
    }
}

/// A tenant sees machines nowhere: not on the fleet, not on their own guests,
/// not through migrations, and not by pinning.
///
/// Hosts are the cell's, and "invisible" is only true if it is true through
/// every window at once. Each of these was a real pane: the instance sheet
/// showed which hypervisor runs the guest, a project editor could create a
/// migration between hosts they cannot list, and a pin to a guessed machine
/// name was honoured silently by the scheduler.
#[tokio::test]
async fn a_tenant_sees_no_machines_through_any_window() {
    let (api, store) = cell_and_store().await;
    // A placed guest, as the cell sees it.
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "web", "spec": { "vcpus": 1, "memory_mib": 512, "node": "hv-1" } }),
        &who(OPERATOR),
    )
    .await
    .expect("an operator may pin");

    // Reading it as the tenant: the machine's name is not in the answer —
    // through the read, and through the project list the boards are built
    // from, which was the door the console actually used.
    let seen = api.get(&name("projects/p1/instances/web"), &who(ADA)).await.unwrap();
    assert!(seen["spec"].get("node").is_none(), "{:?}", seen["spec"]);
    assert!(seen["status"].get("node").is_none(), "{:?}", seen["status"]);
    let listed = api
        .list_for("projects/p1", "instances", &Default::default(), &who(ADA))
        .await
        .unwrap();
    for item in &listed.items {
        assert!(item["spec"].get("node").is_none(), "the project list leaks: {:?}", item["spec"]);
    }
    // And the operator still sees it whole: redaction is a view, not a change.
    let whole = api.get(&name("projects/p1/instances/web"), &who(OPERATOR)).await.unwrap();
    assert_eq!(whole["spec"]["node"], "hv-1");

    // Ports: the fifth door, found live in a tenant's own list after the
    // other four were shut. A port names the machine twice — `status.node`,
    // and again in the words of the computed Ready condition ("carried by
    // hv-1"). Both are taken off the tenant's answer; the condition's status
    // and reason stay, because whether their wire is programmed is theirs.
    api.create(
        "projects/p1",
        "networks",
        &json!({ "id": "net", "spec": { "mtu": 1450, "vni": 0 } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "subnets",
        &json!({ "id": "net", "spec": {
            "network": "projects/p1/networks/net",
            "cidr": "10.9.0.0/24", "gateway": "10.9.0.1" } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "ports",
        &json!({ "id": "wire", "spec": {
            "network": "projects/p1/networks/net",
            "subnet": "projects/p1/subnets/net" } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    {
        use velstra_cloud_model::access::Writer;
        use velstra_cloud_model::resources::{PortSpec, PortStatus};
        let ports: velstra_cloud_store::TypedStore<PortSpec, PortStatus> =
            velstra_cloud_store::TypedStore::new(store.clone(), "cell-1", "ports");
        let mut port = ports.get("projects/p1/ports/wire").await.unwrap().unwrap();
        port.spec.node = Some("hv-1".to_string());
        port.meta.generation += 1;
        ports.update(&port, &Writer::controller("test")).await.unwrap();
        let mut port = ports.get("projects/p1/ports/wire").await.unwrap().unwrap();
        port.status.node = Some("hv-1".to_string());
        port.status.programmed = true;
        ports.update(&port, &Writer::agent("hv-1")).await.unwrap();
    }
    let port_seen = api.get(&name("projects/p1/ports/wire"), &who(ADA)).await.unwrap();
    assert!(port_seen["spec"].get("node").is_none(), "{:?}", port_seen["spec"]);
    assert!(port_seen["status"].get("node").is_none(), "{:?}", port_seen["status"]);
    let ready = port_seen["status"]["conditions"]
        .as_array().unwrap().iter().find(|c| c["kind"] == "Ready").cloned().unwrap();
    assert_eq!(ready["reason"], "Programmed");
    assert!(
        !ready["message"].as_str().unwrap_or_default().contains("hv-1"),
        "the condition's words leak the machine: {ready}"
    );
    let port_listed = api
        .list_for("projects/p1", "ports", &Default::default(), &who(ADA))
        .await
        .unwrap();
    for item in &port_listed.items {
        assert!(item["status"].get("node").is_none(), "the port list leaks: {:?}", item["status"]);
    }
    // The operator's view keeps the machine, words and all.
    let port_whole = api.get(&name("projects/p1/ports/wire"), &who(OPERATOR)).await.unwrap();
    assert_eq!(port_whole["status"]["node"], "hv-1");

    // Migrations: refused whole — create and read alike.
    let refused = api
        .create(
            "projects/p1",
            "migrations",
            &json!({ "id": "m", "spec": { "instance": "projects/p1/instances/web", "to_node": "hv-2" } }),
            &who(ADA),
        )
        .await
        .map(|_| ()).expect_err("a tenant moved a guest between machines");
    assert!(refused.to_string().contains("cell operator"), "{refused}");
    assert!(
        api.list_for("projects/p1", "migrations", &Default::default(), &who(ADA))
            .await
            .is_err(),
        "a tenant listed migrations, which name the machines"
    );

    // Pinning: a name they cannot see is not a name they may write.
    for (kind, spec) in [
        ("instances", json!({ "vcpus": 1, "memory_mib": 512, "node": "hv-1" })),
        ("attachments", json!({ "volume": "projects/p1/volumes/v", "instance": "projects/p1/instances/web", "node": "hv-1" })),
    ] {
        let refused = api
            .create("projects/p1", kind, &json!({ "id": "pinned", "spec": spec }), &who(ADA))
            .await
            .map(|_| ()).expect_err(&format!("a tenant pinned a {kind} to a machine"));
        assert!(refused.to_string().contains("cannot pin"), "{refused}");
    }
    let refused = api
        .patch(
            &name("projects/p1/instances/web"),
            &json!({ "spec": { "node": "hv-2" } }),
            None,
            &who(ADA),
        )
        .await
        .expect_err("a tenant re-pinned a guest by patch");
    assert!(refused.to_string().contains("cannot pin"), "{refused}");

    // And not through `:explainReach` either, which used to answer a tenant
    // with `"on": "<machine>"` and the list of announcing nodes.
    api.create(
        "",
        "networks",
        &json!({ "id": "public", "spec": { "external": true, "mtu": 1500 } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "",
        "subnets",
        &json!({ "id": "public-v4", "spec": {
            "network": "networks/public", "cidr": "203.0.113.0/24",
            "gateway": "203.0.113.1", "dns": [], "reserved": [] } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "",
        "nodes",
        &json!({ "id": "gw", "spec": { "gateway": true } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "floatingips",
        &json!({ "id": "ip", "spec": { "instance": "projects/p1/instances/web" } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    let reach = api.explain_reach(&name("projects/p1/floatingips/ip"), &who(ADA)).await.unwrap();
    assert!(reach.get("on").is_none(), "{reach}");
    assert!(reach["announced"].get("nodes").is_none(), "{reach}");
    let whole = api
        .explain_reach(&name("projects/p1/floatingips/ip"), &who(OPERATOR))
        .await
        .unwrap();
    assert!(whole.get("on").is_some(), "{whole}");
}

/// The cell's public pools are readable by everyone, like the image catalogue.
///
/// A pool is an external network the operator declared at cell scope; a tenant
/// draws an address from it, which means they must be able to see it — its
/// name, and whether the cell offers v4, v6 or both.
#[tokio::test]
async fn the_cells_public_networks_are_readable_and_only_readable_by_tenants() {
    let api = cell().await;
    api.create(
        "",
        "networks",
        &json!({ "id": "public", "spec": { "external": true, "mtu": 1500 } }),
        &who(OPERATOR),
    )
    .await
    .expect("an operator declares a pool");
    api.create(
        "",
        "subnets",
        &json!({ "id": "public-v4", "spec": {
            "network": "networks/public", "cidr": "203.0.113.0/24",
            "gateway": "203.0.113.1", "dns": [], "reserved": [] } }),
        &who(OPERATOR),
    )
    .await
    .expect("and its range");

    // A tenant reads the offer…
    let net = api.get(&name("networks/public"), &who(ADA)).await.unwrap();
    assert_eq!(net["spec"]["external"], true);
    let listed = api.list_for("", "subnets", &Default::default(), &who(ADA)).await.unwrap();
    assert!(
        listed
            .items
            .iter()
            .any(|s| serde_json::to_string(&s["meta"]["name"]).unwrap().contains("public-v4")),
        "the public subnet is not in a tenant's list ({} items)",
        listed.items.len()
    );
    // …and may not touch it.
    assert!(
        api.patch(&name("networks/public"), &json!({ "spec": { "mtu": 1400 } }), None, &who(ADA))
            .await
            .is_err(),
        "a tenant edited the cell's pool"
    );
}

/// A public address is asked for the way a customer asks: "give my VM an IP".
///
/// The instance is named and the platform finds the port; the subnet is left
/// out and the platform finds the pool, v4 first. Nobody has to know that
/// ports exist or what the pool is called.
#[tokio::test]
async fn a_public_address_is_assigned_by_instance_out_of_the_cells_pool() {
    let api = cell().await;
    // Somewhere to announce from: the door check rightly refuses a public
    // address in a cell where no machine carries external traffic.
    api.create(
        "",
        "nodes",
        &json!({ "id": "gw", "spec": { "gateway": true, "schedulable": true } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    for (id, spec) in [
        ("public", json!({ "external": true, "mtu": 1500 })),
    ] {
        api.create("", "networks", &json!({ "id": id, "spec": spec }), &who(OPERATOR))
            .await
            .unwrap();
    }
    for (id, cidr, gw) in [
        ("public-v6", "2001:db8::/64", "2001:db8::1"),
        ("public-v4", "203.0.113.0/24", "203.0.113.1"),
    ] {
        api.create(
            "",
            "subnets",
            &json!({ "id": id, "spec": {
                "network": "networks/public", "cidr": cidr, "gateway": gw,
                "dns": [], "reserved": [] } }),
            &who(OPERATOR),
        )
        .await
        .unwrap();
    }
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "web", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .expect("a tenant makes a guest");

    api.create(
        "projects/p1",
        "floatingips",
        &json!({ "id": "web-ip", "spec": { "instance": "projects/p1/instances/web" } }),
        &who(ADA),
    )
    .await
    .expect("a tenant gives their VM a public address");
    let stored = api.get(&name("projects/p1/floatingips/web-ip"), &who(ADA)).await.unwrap();
    // The port was derived from the guest's one interface, and the pool chosen
    // v4-first; `instance` was consumed.
    let port = stored["spec"]["port"].as_str().unwrap_or_default();
    assert!(port.contains("/ports/"), "{stored}");
    assert!(
        serde_json::to_string(&stored["spec"]["subnet"]).unwrap().contains("public-v4"),
        "{:?}",
        stored["spec"]["subnet"]
    );
    assert!(
        stored["spec"].get("instance").is_none()
            || stored["spec"]["instance"].as_str() == Some(""),
        "{:?}",
        stored["spec"]
    );

    // The v6 pool is one name away.
    api.create(
        "projects/p1",
        "floatingips",
        &json!({ "id": "web-ip6", "spec": {
            "instance": "projects/p1/instances/web", "subnet": "subnets/public-v6" } }),
        &who(ADA),
    )
    .await
    .expect("naming the v6 subnet is how you say v6");

    // And a cell with no pool says so instead of accepting an address from
    // nowhere — checked against a fresh cell.
    let bare = cell().await;
    bare.create(
        "",
        "nodes",
        &json!({ "id": "gw", "spec": { "gateway": true, "schedulable": true } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    bare.create(
        "projects/p1",
        "instances",
        &json!({ "id": "web", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .unwrap();
    let refused = bare
        .create(
            "projects/p1",
            "floatingips",
            &json!({ "id": "ip", "spec": { "instance": "projects/p1/instances/web" } }),
            &who(ADA),
        )
        .await
        .map(|_| ()).expect_err("an address out of no pool");
    assert!(refused.to_string().contains("no public addresses"), "{refused}");
}

/// The month added up, for whoever may read the project — and nobody else.
#[tokio::test]
async fn a_tenant_sums_their_own_bill_and_not_a_neighbours() {
    // Two readings this month, written the way the controller writes them —
    // past the API, because readings are not creatable through it on purpose.
    let (api2, raw) = cell_and_store().await;
    let usage = TypedStore::<
        velstra_cloud_model::usage::UsageRecordSpec,
        velstra_cloud_model::usage::UsageRecordStatus,
    >::new(raw, "cell-1", "usage");
    let now = velstra_cloud_model::meta::Timestamp::now();
    for (i, vcpus) in [(1u64, 2u32), (2, 4)] {
        let at = velstra_cloud_model::meta::Timestamp(
            now.0 - i * velstra_cloud_model::usage::INTERVAL_MS,
        );
        let record = velstra_cloud_model::resources::Resource::new(
            velstra_cloud_model::meta::Meta::new(
                format!("projects/p1/usage/{}", velstra_cloud_model::usage::id_for(at))
                    .parse()
                    .unwrap(),
                velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::usage::UsageRecordSpec {
                project: "projects/p1".into(),
                at,
                used: velstra_cloud_model::resources::Quota {
                    vcpus,
                    memory_mib: 2048,
                    volume_gib: 10,
                    instances: 1,
                    ..Default::default()
                },
            },
            Default::default(),
        );
        usage
            .create(&record, &velstra_cloud_model::access::Writer::controller("usage"))
            .await
            .unwrap();
    }
    let sum = api2
        .explain_usage(&name("projects/p1"), None, &who(ADA))
        .await
        .expect("a tenant reads their own bill");
    // Two readings might straddle a month boundary at exactly the wrong hour;
    // both fields still have to agree with what was counted.
    let hours = sum["hours"].as_u64().unwrap();
    assert!(hours >= 1, "{sum}");
    if hours == 2 {
        assert_eq!(sum["vcpuHours"], 6, "{sum}");
        assert_eq!(sum["memoryGibHours"], 4, "{sum}");
        assert_eq!(sum["volumeGibHours"], 20, "{sum}");
    }
    assert!(sum["hoursInMonthSoFar"].as_u64().unwrap() >= hours, "{sum}");

    // Somebody else's bill is somebody else's.
    assert!(
        api2.explain_usage(&name("projects/p1"), None, &who(BOB)).await.is_err(),
        "a tenant summed a neighbour's project"
    );
    // And a spelling that is not a month is refused, not read as now.
    let refused = api2
        .explain_usage(&name("projects/p1"), Some("August"), &who(ADA))
        .await
        .expect_err("a month called August");
    assert!(refused.to_string().contains("2026-08"), "{refused}");
}

/// The numbers an alert fires on, behind the same door as the fleet.
///
/// The lines carry machine names and capacities, so they are an operator's
/// read — an unauthenticated metrics endpoint would hand the cell's layout to
/// whoever finds the port.
#[tokio::test]
async fn metrics_are_the_operators_and_carry_what_an_alert_needs() {
    let api = cell().await;
    api.create(
        "",
        "nodes",
        &json!({ "id": "hv-1", "spec": { "schedulable": true } }),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "instances",
        &json!({ "id": "web", "spec": { "vcpus": 1, "memory_mib": 512 } }),
        &who(ADA),
    )
    .await
    .unwrap();

    let text = api.metrics(&who(OPERATOR)).await.expect("an operator scrapes");
    for needle in [
        "velstra_node_heartbeat_age_seconds{node=\"hv-1\"}",
        "# TYPE velstra_pool_gib gauge",
        "velstra_instances_off_desired_state",
        "velstra_store_revision",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    // A guest asked to run and not yet running is the alertable number.
    assert!(text.contains("velstra_instances_off_desired_state 1"), "{text}");

    assert!(api.metrics(&who(ADA)).await.is_err(), "a tenant scraped the cell");
}

#[tokio::test]
async fn listing_asks_about_what_is_being_listed_and_not_about_the_project() {
    // `GET /projects/p1/instances` authorises Read on `projects/p1`. Asking that
    // as a question about *projects* was harmless while every role was a rung —
    // a viewer reads everything, so the answer was the same either way.
    //
    // It stops being harmless the moment a role can name collections. Somebody
    // granted `operate` on `instances` could not list them: the question asked
    // was whether they may read the project object, and their role says nothing
    // about projects. A permission that reads as "you may not" when the person
    // holds exactly the thing they are using is the worst kind — it looks like
    // the grant never happened.
    let api = cell().await;
    api.create(
        "",
        "roles",
        &json!({ "id": "nur-maschinen", "spec": {
            "grants": [{ "verb": "read", "collections": ["instances"] }]
        }}),
        &who(OPERATOR),
    )
    .await
    .unwrap();
    api.patch(
        &name("projects/p1"),
        &json!({ "spec": { "bindings": [
            { "role": "roles/nur-maschinen", "members": ["nina"] }
        ]}}),
        None,
        &who(OPERATOR),
    )
    .await
    .unwrap();

    api.list_for(
        "projects/p1",
        "instances",
        &velstra_cloud_api::Filter::none(),
        &who("nina"),
    )
    .await
    .expect("the list asked about the project instead of the instances");

    // And the project object itself is still not theirs to read: the grant names
    // instances, and nothing else.
    assert!(
        api.get(&name("projects/p1"), &who("nina")).await.is_err(),
        "a grant over instances read the project"
    );
}
