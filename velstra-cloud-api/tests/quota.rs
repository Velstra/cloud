//! The limits a project is actually held to.
//!
//! Every dimension here was reported on the project's own status before it was
//! enforced at the door — which is the worst arrangement of the two, because a
//! screen says a project is over its limit while the API keeps saying yes.

use std::sync::Arc;

use serde_json::json;
use velstra_cloud_api::{Api, Identity, StaticTokenVerifier, TokenVerifier};
use velstra_cloud_store::{MemoryStore, Store};

const OPS: &str = "ops";

fn who() -> Identity {
    Identity::new(OPS)
}

async fn cell(quota: serde_json::Value) -> Api {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api =
        Api::new(store, "eu-central", "cell-1", verifier).with_cell_admins(vec![OPS.to_string()]);
    api.create(
        "",
        "projects",
        &json!({"id": "p1", "spec": {"quota": quota}}),
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
    api
}

/// A volume a pool has actually made. A snapshot of one that has not been
/// provisioned is refused before quota is ever consulted, which is right and
/// is not what these tests are about.
async fn provisioned(api: &Api, id: &str, gib: u64) {
    volume(api, id, gib).await.unwrap();
    let mut agent = Identity::new("node:pool-a");
    agent.scopes.push("agent:pool-a".into());
    api.report_status(
        &velstra_cloud_model::meta::ResourceName::parse(&format!("projects/p1/volumes/{id}"))
            .unwrap(),
        &json!({"status": {"observed_generation": 1, "conditions": [], "provisioned": true, "actual_size_gib": gib, "pool": "pools/pool-a"}}),
        None,
        &agent,
    )
    .await
    .expect("a pool reporting a volume it made");
}

async fn volume(api: &Api, id: &str, gib: u64) -> Result<(), String> {
    api.create(
        "projects/p1",
        "volumes",
        &json!({"id": id, "spec": {"size_gib": gib}}),
        &who(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

async fn guest(api: &Api, id: &str, disk: u64) -> Result<(), String> {
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": id, "spec": {
            "vcpus": 1, "memory_mib": 512, "root_disk_gib": disk, "networks": []
        }}),
        &who(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// A guest's root disk is storage the project holds. The count on the project's
/// status always said so; the door did not, so a project capped at 100 GiB
/// could take a terabyte in root disks and read back a status saying it was ten
/// times over its own limit.
#[tokio::test]
async fn a_root_disk_counts_against_the_storage_limit() {
    let api = cell(json!({"volume_gib": 100})).await;
    guest(&api, "web-1", 80).await.expect("80 GiB fits in 100");
    let why = guest(&api, "web-2", 40)
        .await
        .expect_err("120 GiB of root disk fitted in a 100 GiB quota");
    assert!(why.contains("GiB of volume"), "{why}");
}

/// And it counts from the other side too: a volume created after the guests
/// sees the disks they already hold.
#[tokio::test]
async fn a_volume_sees_the_root_disks_already_taken() {
    let api = cell(json!({"volume_gib": 100})).await;
    guest(&api, "web-1", 80).await.unwrap();
    let why = volume(&api, "data-1", 40)
        .await
        .expect_err("a volume ignored the root disks in the same project");
    assert!(why.contains("GiB of volume"), "{why}");
    volume(&api, "data-1", 10).await.expect("10 GiB still fits");
}

/// Devices were counted and never enforced, which makes the limit decoration:
/// hardware exists once, so the one dimension that cannot be oversubscribed was
/// the one nothing checked.
#[tokio::test]
async fn a_device_limit_is_a_limit() {
    let api = cell(json!({"devices": 1})).await;
    api.create(
        "projects/p1",
        "instances",
        &json!({"id": "gpu-1", "spec": {
            "vcpus": 1, "memory_mib": 512, "root_disk_gib": 1, "networks": [],
            "devices": ["device-classes/gpu"]
        }}),
        &who(),
    )
    .await
    .expect("one device fits in a limit of one");

    let refused = api
        .create(
            "projects/p1",
            "instances",
            &json!({"id": "gpu-2", "spec": {
                "vcpus": 1, "memory_mib": 512, "root_disk_gib": 1, "networks": [],
                "devices": ["device-classes/gpu"]
            }}),
            &who(),
        )
        .await;
    let Err(why) = refused else {
        panic!("a second device was taken against a limit of one");
    };
    assert!(why.to_string().contains("devices"), "{why}");
}

/// Snapshots were the one thing a project could make without limit. They are
/// one call each and they occupy a pool for as long as they exist.
#[tokio::test]
async fn a_snapshot_limit_is_a_limit() {
    let api = cell(json!({"snapshots": 1})).await;
    provisioned(&api, "data-1", 1).await;
    api.create(
        "projects/p1/volumes/data-1",
        "snapshots",
        &json!({"id": "s1", "spec": {}}),
        &who(),
    )
    .await
    .expect("one snapshot fits in a limit of one");

    let refused = api
        .create(
            "projects/p1/volumes/data-1",
            "snapshots",
            &json!({"id": "s2", "spec": {}}),
            &who(),
        )
        .await;
    let Err(why) = refused else {
        panic!("a second snapshot was taken against a limit of one");
    };
    assert!(why.to_string().contains("snapshots"), "{why}");
}

/// A limit nobody set is not a limit of zero — the convention every other
/// dimension follows, checked here because two of these are new.
#[tokio::test]
async fn an_unset_limit_allows_everything() {
    let api = cell(json!({})).await;
    provisioned(&api, "data-1", 1).await;
    for i in 0..3 {
        api.create(
            "projects/p1/volumes/data-1",
            "snapshots",
            &json!({"id": format!("s{i}"), "spec": {}}),
            &who(),
        )
        .await
        .expect("a snapshot was refused against a limit nobody set");
    }
}

/// A firewall rule the datapath cannot key on is refused where it is written,
/// not discovered on a wire.
///
/// It used to be accepted, stored and shown on a screen — and then the *whole
/// port* failed to program and the guest waiting on it never started, with the
/// sentence only in an agent's journal on a machine the author does not have.
#[tokio::test]
async fn a_rule_no_datapath_can_program_is_refused_where_it_is_written() {
    let api = cell(json!({})).await;

    let any = api
        .create(
            "projects/p1",
            "security-groups",
            &json!({"id": "g1", "spec": {"rules": [
                {"direction": "ingress", "protocol": "any", "remote": {"cidr": "10.0.0.0/8"}}
            ]}}),
            &who(),
        )
        .await
        .err()
        .map(|e| e.to_string())
        .expect("`any` was accepted");
    assert!(any.contains("separate tcp, udp and icmp"), "{any}");

    let wide = api
        .create(
            "projects/p1",
            "security-groups",
            &json!({"id": "g2", "spec": {"rules": [
                {"direction": "ingress", "protocol": "tcp", "remote": {"cidr": "0.0.0.0/0"}}
            ]}}),
            &who(),
        )
        .await
        .err()
        .map(|e| e.to_string())
        .expect("all-TCP-from-anywhere was accepted");
    assert!(wide.contains("Name a port"), "{wide}");

    // And the ordinary rule still goes through. A check that refused
    // everything would be indistinguishable from a broken endpoint.
    api.create(
        "projects/p1",
        "security-groups",
        &json!({"id": "g3", "spec": {"rules": [
            {"direction": "ingress", "protocol": "tcp", "ports": {"from": 443, "to": 443},
             "remote": {"cidr": "0.0.0.0/0"}}
        ]}}),
        &who(),
    )
    .await
    .expect("a rule naming a protocol and a port was refused");
}

/// A second subnet on a network the fabric is carrying is refused where it is
/// written. It used to be accepted, and the network then quietly stopped being
/// mirrored — working until the next fabric restart, and then not.
#[tokio::test]
async fn a_second_subnet_on_a_mirrored_network_is_refused() {
    use velstra_cloud_model::meta::{Condition, ConditionStatus};

    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let verifier: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
    let api = Api::new(store.clone(), "eu-central", "cell-1", verifier)
        .with_cell_admins(vec![OPS.to_string()]);
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
    let subnet = |id: &str, cidr: &str| {
        json!({"id": id, "spec": {
            "network": "projects/p1/networks/n1", "cidr": cidr,
            "gateway": format!("{}1", &cidr[..cidr.len() - 4]), "dns": [], "reserved": []
        }})
    };
    api.create(
        "projects/p1",
        "subnets",
        &subnet("s1", "10.20.0.0/24"),
        &who(),
    )
    .await
    .unwrap();

    // Not mirrored yet — a cell with no fabric carries several subnets per
    // network perfectly well, and refusing there would take away something
    // that works.
    api.create(
        "projects/p1",
        "subnets",
        &subnet("s2", "10.21.0.0/24"),
        &who(),
    )
    .await
    .expect("a second subnet was refused on a network nothing is carrying");

    // Now the controller says the fabric holds it. Written the way the
    // controller writes it — a network's status is nobody's agent's.
    let networks: velstra_cloud_store::TypedStore<
        velstra_cloud_model::resources::NetworkSpec,
        velstra_cloud_model::resources::NetworkStatus,
    > = velstra_cloud_store::TypedStore::new(store.clone(), "cell-1", "networks");
    let mut network = networks
        .get("projects/p1/networks/n1")
        .await
        .unwrap()
        .expect("the network");
    velstra_cloud_model::meta::set_condition(
        &mut network.status.conditions,
        Condition::new("Mirrored", ConditionStatus::True, "Mirrored", "", 1),
    );
    // Written straight into the store rather than through a typed writer: what
    // is being set up is the *state* the API reads, and which writer is allowed
    // to produce it is the store's own rule, tested where it lives.
    let key = velstra_cloud_store::key_for("cell-1", "networks", "projects/p1/networks/n1");
    let held = store.get(&key).await.unwrap().expect("the stored network");
    let mut document: serde_json::Value = serde_json::from_slice(&held.value).unwrap();
    document["status"] = serde_json::to_value(&network.status).unwrap();
    store
        .put(
            &key,
            serde_json::to_vec(&document).unwrap(),
            velstra_cloud_store::Expect::Revision(held.revision),
        )
        .await
        .expect("the controller saying the fabric holds it");

    let refused = api
        .create(
            "projects/p1",
            "subnets",
            &subnet("s3", "10.22.0.0/24"),
            &who(),
        )
        .await;
    let Err(why) = refused else {
        panic!("a third subnet was accepted on a mirrored network");
    };
    let why = why.to_string();
    assert!(why.contains("no faithful mirror"), "{why}");
    assert!(why.contains("Make another network"), "{why}");
}
