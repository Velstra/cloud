//! Deleting something another object still names.
//!
//! Before these, `Api::delete` forwarded straight to the collection. Deleting a
//! port a running instance used, a subnet with addresses handed out of it, or a
//! project with machines in it all succeeded — and nothing failed loudly
//! anywhere. The reference simply stopped resolving, and whoever found out was
//! an agent that could not program a port, at a moment nobody connects to the
//! request that caused it.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_model::meta::ResourceName;
use velstra_cloud_store::{MemoryStore, Store};

const OPS: &str = "ops";

fn who() -> Identity {
    Identity::new(OPS)
}

fn name(text: &str) -> ResourceName {
    ResourceName::parse(text).unwrap()
}

/// A project with a network, a subnet, a port and an instance using the port.
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
            "cidr": "10.20.0.0/24",
            "gateway": "10.20.0.1",
            "dns": [],
            "reserved": []
        }}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "ports",
        &json!({"id": "pt1", "spec": {
            "network": "projects/p1/networks/n1",
            "subnet": "projects/p1/subnets/s1",
            "security_groups": []
        }}),
        &who(),
    )
    .await
    .unwrap();
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": "i1", "spec": {
            // Spelled as the model spells it: this calls the API core
            // directly, and the camelCase→snake_case conversion is the REST
            // layer's. Written the other way these three fields were silently
            // ignored, and the fixture quietly described a guest it was not
            // creating.
            "vcpus": 1, "memory_mib": 512, "root_disk_gib": 1,
            "desired_state": "Running",
            "ports": ["projects/p1/ports/pt1"]
        }}),
        &who(),
    )
    .await
    .unwrap();
    api
}

#[tokio::test]
async fn a_port_a_guest_is_using_is_not_deleted_out_from_under_it() {
    let api = cell().await;
    let Err(refused) = api
        .delete(&name("projects/p1/ports/pt1"), None, &who())
        .await
    else {
        panic!("the port a running guest uses was deleted");
    };
    let why = refused.to_string();
    assert!(
        why.contains("projects/p1/instances/i1"),
        "the refusal does not name what is holding it: {why}"
    );
}

#[tokio::test]
async fn a_subnet_with_a_port_on_it_is_not_deleted() {
    let api = cell().await;
    let Err(refused) = api
        .delete(&name("projects/p1/subnets/s1"), None, &who())
        .await
    else {
        panic!("a subnet with a port on it was deleted");
    };
    assert!(refused.to_string().contains("projects/p1/ports/pt1"));
}

#[tokio::test]
async fn a_network_is_held_by_both_its_subnet_and_its_ports() {
    let api = cell().await;
    let Err(refused) = api
        .delete(&name("projects/p1/networks/n1"), None, &who())
        .await
    else {
        panic!("a network with a subnet and a port on it was deleted");
    };
    let why = refused.to_string();
    assert!(why.contains("subnets/s1"), "{why}");
    assert!(why.contains("ports/pt1"), "{why}");
}

#[tokio::test]
async fn a_project_with_machines_in_it_is_not_deleted() {
    // Nothing *names* a project as a reference, so this is its own question —
    // and "there are still machines in it" is the answer somebody needs.
    let api = cell().await;
    let Err(refused) = api.delete(&name("projects/p1"), None, &who()).await else {
        panic!("a project with everything in it was deleted");
    };
    let why = refused.to_string();
    assert!(why.contains("projects/p1/"), "{why}");
}

#[tokio::test]
async fn releasing_the_last_holder_makes_the_delete_go_through() {
    // The guard has to be a gate and not a wall: the ordinary way to remove a
    // port is to stop using it first, and that has to work.
    let api = cell().await;
    api.delete(&name("projects/p1/instances/i1"), None, &who())
        .await
        .expect("nothing names an instance");
    api.delete(&name("projects/p1/ports/pt1"), None, &who())
        .await
        .expect("the port is free now");
    api.delete(&name("projects/p1/subnets/s1"), None, &who())
        .await
        .expect("the subnet is free now");
    api.delete(&name("projects/p1/networks/n1"), None, &who())
        .await
        .expect("the network is free now");
    api.delete(&name("projects/p1"), None, &who())
        .await
        .expect("the project is empty now");
}

#[tokio::test]
async fn an_object_does_not_hold_itself() {
    // `projects` has a reference field of its own (`parent`), so a naive scan
    // would find the project holding itself and refuse every project delete.
    let api = cell().await;
    api.create(
        "",
        "projects",
        &json!({"id": "empty", "spec": {"quota": {}, "parent": "organizations/o1"}}),
        &who(),
    )
    .await
    .unwrap();
    api.delete(&name("projects/empty"), None, &who())
        .await
        .expect("an empty project refused to go");
}

#[tokio::test]
async fn a_node_a_guest_is_placed_on_is_not_deleted() {
    // The bare-id spelling: `spec.node` is `node-a`, not `nodes/node-a`, and a
    // guard that only understood full names would protect nothing here.
    let api = cell().await;
    api.create(
        "",
        "nodes",
        &json!({"id": "node-a", "spec": {"schedulable": true, "labels": []}}),
        &who(),
    )
    .await
    .unwrap();
    api.patch(
        &name("projects/p1/instances/i1"),
        &json!({"spec": {"node": "node-a"}}),
        None,
        &who(),
    )
    .await
    .unwrap();

    let Err(refused) = api.delete(&name("nodes/node-a"), None, &who()).await else {
        panic!("a node with a guest on it was deleted");
    };
    let why = refused.to_string();
    assert!(why.contains("projects/p1/instances/i1"), "{why}");
}
