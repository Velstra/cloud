//! A load balancer becomes real fabric services, follows its spec, and leaves
//! nothing behind when it is deleted.
//!
//! The same shape as `network_mirror`, `router_mirror` and
//! `floating_ip_mirror`: the test declares on the fabric only what a node
//! genuinely states about itself — a host and its ports — and everything else
//! is asked of the fabric afterwards, so a control plane that agreed with
//! itself and programmed nothing would fail here.
//!
//! It also covers the change, which is the case a create-only mirror gets
//! wrong: the fabric fails a duplicate id, so shrinking the pool has to be a
//! remove-and-add, and an implementation that only ever adds leaves the old
//! pool serving forever.

use std::{path::PathBuf, process::Child, sync::Arc, time::Duration};

use velstra_cloud_controller::{
    load_balancer::LoadBalancerController, network::NetworkController, sweep,
};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    loadbalancer::{Listener, LoadBalancerSpec, LoadBalancerStatus, Protocol},
    meta::{Meta, Placement, ResourceName, condition},
    resources::{
        FABRIC_RELEASE_FINALIZER, FloatingIpSpec, FloatingIpStatus, NetworkSpec, NetworkStatus,
        PortSpec, PortStatus, Resource, SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const CELL: &str = "cell-1";
const VNI: u32 = 7002;
const NETWORK: &str = "projects/p1/networks/blue";
const SUBNET: &str = "projects/p1/subnets/s1";
const LB: &str = "projects/p1/load-balancers/front";
const HOST: &str = "node-a";

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
        let (listen, admin, raft) = (50991, 50992, 50993);
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
async fn a_load_balancer_is_programmed_follows_its_spec_and_is_torn_down() {
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
    let ports: TypedStore<PortSpec, PortStatus> = TypedStore::new(raw.clone(), CELL, "ports");
    let floating: TypedStore<FloatingIpSpec, FloatingIpStatus> =
        TypedStore::new(raw.clone(), CELL, "floatingips");
    let balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus> =
        TypedStore::new(raw.clone(), CELL, "load-balancers");

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
                meta(SUBNET),
                SubnetSpec {
                    network: NETWORK.into(),
                    cidr: "10.41.0.0/24".into(),
                    gateway: "10.41.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    // Two cloud ports, each already placed and programmed — the node agent's
    // half, and the test stands in for it only there.
    for (id, address, tap) in [
        ("web", "10.41.0.10", "vt-web"),
        ("web2", "10.41.0.11", "vt-web2"),
    ] {
        let mut port = Resource::new(
            meta(&format!("projects/p1/ports/{id}")),
            PortSpec {
                network: NETWORK.into(),
                subnet: SUBNET.into(),
                address: Some(address.into()),
                ..Default::default()
            },
            PortStatus::default(),
        );
        port.status.node = Some(HOST.into());
        port.status.tap_device = Some(tap.into());
        ports
            .create(
                &port,
                &velstra_cloud_model::access::Writer::controller("test"),
            )
            .await
            .unwrap();
    }

    let mut client = velstra_cloud_fabric::connect(&fabric.admin).await.unwrap();
    client
        .add_host(pb::HostSpec {
            id: HOST.into(),
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

    let net_controller = NetworkController::new(
        raw.clone(),
        CELL,
        subnets.clone(),
        Some(fabric.admin.clone()),
    );
    sweep(&net_controller, &networks).await.unwrap();

    for (id, address, tap) in [
        ("web", "10.41.0.10", "vt-web"),
        ("web2", "10.41.0.11", "vt-web2"),
    ] {
        client
            .create_port(pb::CreatePortRequest {
                network: VNI,
                host: HOST.into(),
                tap: tap.into(),
                ip: address.into(),
                policy: None,
                mac: None,
            })
            .await
            .unwrap_or_else(|e| panic!("creating the fabric port for {id}: {e}"));
    }

    balancers
        .create(
            &Resource::new(
                meta(LB),
                LoadBalancerSpec {
                    network: NETWORK.into(),
                    subnet: SUBNET.into(),
                    vip: None,
                    listeners: vec![Listener {
                        protocol: Protocol::Tcp,
                        port: 443,
                        member_port: 8080,
                    }],
                    members: vec![
                        "projects/p1/ports/web".into(),
                        "projects/p1/ports/web2".into(),
                    ],
                },
                LoadBalancerStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let c = LoadBalancerController::new(
        raw.clone(),
        CELL,
        balancers.clone(),
        networks.clone(),
        subnets.clone(),
        ports.clone(),
        floating.clone(),
        Some(fabric.admin.clone()),
    );
    // Three passes, one write each: the guard that makes a delete releasable,
    // then the address, then the fabric.
    for _ in 0..3 {
        sweep(&c, &balancers).await.unwrap();
    }

    let services = client
        .list_load_balancers(pb::ListLoadBalancersRequest {})
        .await
        .unwrap()
        .into_inner()
        .load_balancers;
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(
        services[0].vip, "10.41.0.2",
        "the VIP is the lowest address nothing else holds"
    );
    assert_eq!(services[0].port, 443);
    assert_eq!(services[0].vni, VNI);
    assert_eq!(services[0].members.len(), 2, "{services:?}");
    assert!(services[0].members.iter().all(|m| m.port == 8080));

    let programmed = balancers.get(LB).await.unwrap().unwrap();
    assert_eq!(programmed.status.vip, "10.41.0.2");
    assert_eq!(programmed.status.listeners.len(), 1);
    assert_eq!(programmed.status.listeners[0].members, 2);
    let said = condition(&programmed.status.conditions, "Ready").expect("nothing was said");
    assert_eq!(said.reason, "Programmed");
    assert!(
        programmed.converged(),
        "the world caught up and nothing says so"
    );

    // A settled object costs nothing: another pass writes nothing at all.
    sweep(&c, &balancers).await.unwrap();
    let after = balancers.get(LB).await.unwrap().unwrap();
    assert_eq!(
        after.meta.revision, programmed.meta.revision,
        "a resync over a converged load balancer performed a write"
    );

    // Shrink the pool. The fabric fails a duplicate id, so this only lands if
    // the mirror replaces rather than re-adds.
    let mut next = after.clone();
    next.spec.members = vec!["projects/p1/ports/web".into()];
    next.meta.generation += 1;
    balancers
        .update(
            &next,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    sweep(&c, &balancers).await.unwrap();

    let services = client
        .list_load_balancers(pb::ListLoadBalancersRequest {})
        .await
        .unwrap()
        .into_inner()
        .load_balancers;
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(
        services[0].members.len(),
        1,
        "the pool did not follow the spec: {services:?}"
    );

    // Delete. The record stays until the fabric has let go, and then the guard
    // comes off; what must not remain is a service answering on the VIP.
    let mut deleting = balancers.get(LB).await.unwrap().unwrap();
    deleting.meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp::now());
    balancers
        .update(
            &deleting,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    sweep(&c, &balancers).await.unwrap();

    let services = client
        .list_load_balancers(pb::ListLoadBalancersRequest {})
        .await
        .unwrap()
        .into_inner()
        .load_balancers;
    assert!(
        services.is_empty(),
        "a deleted load balancer's services kept answering: {services:?}"
    );
    let gone = balancers.get(LB).await.unwrap().unwrap();
    assert!(
        !gone.meta.has_finalizer(FABRIC_RELEASE_FINALIZER),
        "the fabric let go and the guard stayed on"
    );
}
