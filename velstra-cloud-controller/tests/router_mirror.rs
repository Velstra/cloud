//! Two tenant networks route to each other because a router said so.
//!
//! The same shape as `network_mirror`, and for the same reason: a routed
//! context that exists only in the control plane's model is a tenant who
//! declared a router and got no routing. So this test declares **nothing** on
//! the fabric. It starts a real fabric, runs the two controllers, and asks the
//! fabric what it knows.
//!
//! It also exercises the pair together, which is the arrangement that actually
//! ships: the network controller says what a VNI is, and only then can the
//! router controller say that two VNIs route.

use std::{path::PathBuf, process::Child, sync::Arc, time::Duration};

use velstra_cloud_controller::{
    network::NetworkController,
    router::{RouterController, gateway_mac_for, l3_vni_for},
    runner::Reconciler,
    sweep,
};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName},
    resources::{
        NetworkSpec, NetworkStatus, Resource, RouterSpec, RouterStatus, SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const CELL: &str = "cell-1";
const ROUTER: &str = "projects/p1/routers/core";
const BLUE: (&str, u32) = ("projects/p1/networks/blue", 6001);
const GREEN: (&str, u32) = ("projects/p1/networks/green", 6002);

fn controller_binary() -> Option<PathBuf> {
    for candidate in [
        "../../fabric/target/debug/velstra-controller",
        "../fabric/target/debug/velstra-controller",
    ] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// A fabric controller that stops when it drops.
struct Fabric {
    child: Child,
    admin: String,
}

impl Drop for Fabric {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Whether nothing is already listening here.
///
/// Checked before spawning, because the failure it prevents is silent: a
/// fixture whose port is taken does not fail to start — it connects to
/// whatever *is* there and tests against somebody else's state.
///
/// **50950–50999 belongs to `velstra-cloud-controller`**; the node agent crate
/// has 50900–50949. `cargo test` runs test binaries concurrently, and when the
/// two ranges overlapped this produced three different intermittent failures
/// in three different files, each looking like a bug in whatever it hit.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

impl Fabric {
    async fn start(binary: &PathBuf) -> Option<Self> {
        // Its own ports, so every fixture in this crate can exist at once.
        let (listen, admin, raft) = (50971, 50972, 50973);
        for port in [listen, admin, raft] {
            assert!(
                port_is_free(port),
                "127.0.0.1:{port} is already listening. This fixture would connect to it and \
                 test against somebody else's fabric — which is how an intermittent failure \
                 that looks like a bug in the code under test is really a port collision."
            );
        }
        let _ = std::process::Command::new("ip")
            .args(["link", "set", "lo", "up"])
            .status();
        let child = std::process::Command::new(binary)
            .args([
                "serve",
                "--node-id",
                "1",
                "--peer",
                &format!("1=127.0.0.1:{raft}"),
                "--bootstrap",
                "--listen",
                &format!("127.0.0.1:{listen}"),
                "--admin-listen",
                &format!("127.0.0.1:{admin}"),
                "--raft-listen",
                &format!("127.0.0.1:{raft}"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let me = Self {
            child,
            admin: format!("http://127.0.0.1:{admin}"),
        };
        for _ in 0..80 {
            if velstra_cloud_fabric::connect(&me.admin).await.is_ok() {
                return Some(me);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

fn meta(name: &str) -> Meta {
    Meta::new(
        ResourceName::parse(name).unwrap(),
        Placement::new("eu-central", CELL),
    )
}

#[tokio::test]
async fn a_router_makes_two_tenant_networks_route_on_the_real_fabric() {
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    let Some(fabric) = Fabric::start(&binary).await else {
        eprintln!("skipped: the fabric controller would not start here");
        return;
    };

    let raw: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(raw.clone(), CELL, "networks");
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(raw.clone(), CELL, "subnets");
    let routers: TypedStore<RouterSpec, RouterStatus> =
        TypedStore::new(raw.clone(), CELL, "routers");

    for (i, (name, vni)) in [BLUE, GREEN].into_iter().enumerate() {
        networks
            .create(
                &Resource::new(
                    meta(name),
                    NetworkSpec {
                        vni,
                        mtu: 1500,
                        external: false,
                        announce: Default::default(),
                    },
                    NetworkStatus::default(),
                ),
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
        subnets
            .create(
                &Resource::new(
                    meta(&format!("projects/p1/subnets/s{i}")),
                    SubnetSpec {
                        network: name.into(),
                        cidr: format!("10.3{i}.0.0/24"),
                        gateway: format!("10.3{i}.0.1"),
                        dns: vec![],
                        reserved: vec![],
                    },
                    SubnetStatus::default(),
                ),
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
    }
    routers
        .create(
            &Resource::new(
                meta(ROUTER),
                RouterSpec {
                    networks: vec![BLUE.0.into(), GREEN.0.into()],
                },
                RouterStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let mut client = velstra_cloud_fabric::connect(&fabric.admin).await.unwrap();
    assert!(
        client
            .list_ip_vrfs(pb::ListIpVrfsRequest {})
            .await
            .unwrap()
            .into_inner()
            .ip_vrfs
            .is_empty(),
        "the fabric already knew a routed context nobody had declared"
    );

    // The networks first — a router may only route VNIs the fabric knows, so a
    // router mirrored before its networks is refused, which is the fabric
    // catching exactly the mistake it should.
    let net_controller = NetworkController::new(
        raw.clone(),
        CELL,
        subnets.clone(),
        Some(fabric.admin.clone()),
    );
    let router_controller = RouterController::new(
        raw.clone(),
        CELL,
        networks.clone(),
        Some(fabric.admin.clone()),
    );

    sweep(&router_controller, &routers).await.unwrap();
    let early = routers.get(ROUTER).await.unwrap().unwrap();
    assert!(
        format!("{:?}", early.status.conditions).contains("Refused"),
        "a router over networks the fabric had never heard of claimed to be routed: {:?}",
        early.status.conditions
    );

    sweep(&net_controller, &networks).await.unwrap();
    sweep(&router_controller, &routers).await.unwrap();

    // The proof: the fabric holds one routed context, over both VNIs, with the
    // numbers derived from the router's name — not numbers this test chose and
    // then read back.
    let vrfs = client
        .list_ip_vrfs(pb::ListIpVrfsRequest {})
        .await
        .unwrap()
        .into_inner()
        .ip_vrfs;
    assert_eq!(vrfs.len(), 1, "{vrfs:?}");
    let vrf = &vrfs[0];
    assert_eq!(vrf.name, ROUTER);
    assert_eq!(vrf.l3_vni, l3_vni_for(ROUTER));
    assert_eq!(vrf.gateway_mac, gateway_mac_for(ROUTER));
    let mut got = vrf.networks.clone();
    got.sort_unstable();
    assert_eq!(got, vec![BLUE.1, GREEN.1]);

    // And the router says so about itself, with the numbers an operator needs
    // to recognise the gateway in an ARP table.
    let after = routers.get(ROUTER).await.unwrap().unwrap();
    assert!(
        format!("{:?}", after.status.conditions).contains("Routed"),
        "the router does not record that it reached the fabric: {:?}",
        after.status.conditions
    );
    assert_eq!(after.status.l3_vni, l3_vni_for(ROUTER));
    assert_eq!(after.status.gateway_mac, gateway_mac_for(ROUTER));

    // Restated on the next pass rather than refused — the reason `add_ip_vrf`
    // stopped being create-only in two ways at once.
    sweep(&router_controller, &routers)
        .await
        .expect("a second pass must restate, not fail");
    assert_eq!(
        client
            .list_ip_vrfs(pb::ListIpVrfsRequest {})
            .await
            .unwrap()
            .into_inner()
            .ip_vrfs
            .len(),
        1,
        "a second pass created a second routed context"
    );

    // And deleting the router retires the routed context. It needs nothing from
    // the record that just went, because the number is a function of the name —
    // which is the payoff of deriving it rather than allocating one.
    router_controller
        .reconcile(ROUTER, None)
        .await
        .expect("retiring a deleted router");
    assert!(
        client
            .list_ip_vrfs(pb::ListIpVrfsRequest {})
            .await
            .unwrap()
            .into_inner()
            .ip_vrfs
            .is_empty(),
        "the fabric still routes a tenant whose router is gone"
    );
    // Twice, because a resync will do exactly that: removing one the fabric does
    // not have must be quiet, not an error.
    router_controller
        .reconcile(ROUTER, None)
        .await
        .expect("a second pass over a router that is already gone");
}
