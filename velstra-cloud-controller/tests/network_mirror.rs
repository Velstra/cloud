//! The fabric learns about a tenant network from the control plane.
//!
//! This is the gap that made tenant isolation designed-but-not-in-force: the
//! node agent called `create_port` with a VNI, fabric answered `unknown network
//! vni`, and nothing above the nodes had ever said what that VNI was. It passed
//! in the node agent's own test only because that test declared the network
//! itself — its fixture said so out loud, and the gap stayed open anyway.
//!
//! So this test declares **nothing** on the fabric. It starts a real fabric,
//! runs the controller once, and asks the fabric what it knows. If the mirror
//! does not happen, there is nothing there to find.

use std::{path::PathBuf, process::Child, sync::Arc, time::Duration};

use velstra_cloud_controller::{network::NetworkController, sweep};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName},
    resources::{NetworkSpec, NetworkStatus, Resource, SubnetSpec, SubnetStatus},
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const CELL: &str = "cell-1";
const VNI: u32 = 5001;
const NETWORK: &str = "projects/p1/networks/blue";

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
        // Its own ports, so this fixture and the node agent's can both exist.
        let (listen, admin, raft) = (50961, 50962, 50963);
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
        // Asked, not slept for.
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
async fn a_tenant_network_reaches_the_fabric_without_anybody_declaring_it() {
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

    networks
        .create(
            &Resource::new(
                meta(NETWORK),
                NetworkSpec {
                    host_bridge: String::new(),
                    vni: VNI,
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
                meta("projects/p1/subnets/s1"),
                SubnetSpec {
                    network: NETWORK.into(),
                    cidr: "10.20.0.0/24".into(),
                    gateway: "10.20.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let mut client = velstra_cloud_fabric::connect(&fabric.admin).await.unwrap();

    // A host, which is the one thing a node genuinely does state about itself —
    // the test stands in for a node agent here, and only for that.
    client
        .add_host(pb::HostSpec {
            id: "node-a".into(),
            vtep: "10.10.0.1".into(),
            underlay_iface: "eth0".into(),
            underlay_mac: "02:00:00:00:00:01".into(),
            encap: pb::Encap::Vxlan as i32,
            srv6_locator: String::new(),
            udp_port: 0,
            underlay_mtu: 0,
        })
        .await
        .expect("declaring a host");

    // The proof is a port, because the port is what was broken: before anything
    // mirrored a network, this is where a real cell stopped, with
    // `unknown network vni`.
    let port = |tap: &str| pb::CreatePortRequest {
        network: VNI,
        host: "node-a".into(),
        tap: tap.into(),
        ip: "10.20.0.10".into(),
        policy: None,
        // No opinion: fabric derives it from the address, which is what a caller
        // without a platform-assigned MAC gets.
        mac: None,
    };

    let before = client.create_port(port("tap-before")).await;
    let refusal = before
        .expect_err("the fabric accepted a port for a network nobody had declared")
        .message()
        .to_string();
    assert!(
        refusal.contains("unknown network"),
        "the precondition this test is about is not the one that failed: {refusal}"
    );

    // Now the controller says what the network is. Nothing else does.
    let controller = NetworkController::new(
        raw.clone(),
        CELL,
        subnets.clone(),
        Some(fabric.admin.clone()),
    );
    sweep(&controller, &networks).await.unwrap();

    client
        .create_port(port("tap-after"))
        .await
        .expect("a port still could not be created after the network was mirrored");

    // And the network says so about itself, so an operator sees it without
    // asking the fabric.
    let after = networks.get(NETWORK).await.unwrap().unwrap();
    let said = format!("{:?}", after.status.conditions);
    assert!(
        said.contains("Mirrored"),
        "the network does not record that it reached the fabric: {said}"
    );

    // Restated on the next pass rather than refused — the whole reason
    // `add_network` stopped being create-only.
    sweep(&controller, &networks)
        .await
        .expect("a second pass must restate, not fail");
}
