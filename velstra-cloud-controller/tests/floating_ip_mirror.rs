//! A floating IP reaches a real port on a real fabric, and follows when the
//! operator points it somewhere else.
//!
//! The same shape as `network_mirror` and `router_mirror`: nothing is declared
//! on the fabric by the test except the things a node genuinely states about
//! itself — a host and its ports. Everything else is asked of the fabric
//! afterwards, so a control plane that agreed with itself and programmed
//! nothing would fail here.
//!
//! It also covers the move, which is the case a create-only mirror gets wrong:
//! an association is one-to-one, so pointing an address at a second port has to
//! release the first, and an implementation that only ever associates leaves
//! the address on the machine that was replaced.

use std::{path::PathBuf, process::Child, sync::Arc, time::Duration};

use velstra_cloud_controller::{
    floating_ip::FloatingIpController, network::NetworkController, sweep,
};
use velstra_cloud_fabric::pb;
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName},
    resources::{
        FABRIC_RELEASE_FINALIZER, FloatingIpSpec, FloatingIpStatus, NetworkSpec, NetworkStatus,
        PortSpec, PortStatus, Resource, SubnetSpec, SubnetStatus,
    },
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const CELL: &str = "cell-1";
const VNI: u32 = 7001;
const NETWORK: &str = "projects/p1/networks/blue";
const SUBNET: &str = "projects/p1/subnets/s1";
const FIP: &str = "projects/p1/floatingips/front";
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
    /// `base` is this test's own three ports. Two tests in one binary run
    /// concurrently, so a fixed triple would have them starting two fabrics on
    /// one port — and the loser would silently test against the winner's,
    /// which is the failure the assertion below is about.
    async fn start(binary: &PathBuf, base: u16) -> Option<Self> {
        let (listen, admin, raft) = (base, base + 1, base + 2);
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
async fn a_floating_ip_reaches_a_port_and_follows_when_it_is_moved() {
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    let Some(fabric) = Fabric::start(&binary, 50981).await else {
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
                    cidr: "10.40.0.0/24".into(),
                    gateway: "10.40.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    // Two cloud ports, each already placed and programmed — that is the node
    // agent's half, and the test stands in for it only there.
    for (id, address, tap) in [
        ("web", "10.40.0.10", "vt-web"),
        ("web2", "10.40.0.11", "vt-web2"),
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

    // The network — and, with it, the fabric subnet a floating IP is allocated
    // out of. Without that subnet the whole collection would be inert, which is
    // exactly why the network controller declares it.
    let net_controller = NetworkController::new(
        raw.clone(),
        CELL,
        subnets.clone(),
        Some(fabric.admin.clone()),
    );
    sweep(&net_controller, &networks).await.unwrap();
    let known = client
        .list_subnets(pb::ListSubnetsRequest {})
        .await
        .unwrap()
        .into_inner()
        .subnets;
    assert_eq!(known.len(), 1, "the fabric has no subnet to allocate from");
    assert_eq!(known[0].id, SUBNET);

    for (id, address, tap) in [
        ("web", "10.40.0.10", "vt-web"),
        ("web2", "10.40.0.11", "vt-web2"),
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

    floating
        .create(
            &Resource::new(
                meta(FIP),
                FloatingIpSpec {
                    instance: String::new(),
                    subnet: SUBNET.into(),
                    address: None,
                    port: "projects/p1/ports/web".into(),
                    delivery: Default::default(),
                    announce: None,
                },
                FloatingIpStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();

    let c = FloatingIpController::new(
        raw.clone(),
        CELL,
        floating.clone(),
        subnets.clone(),
        ports.clone(),
        TypedStore::new(raw.clone(), CELL, "load-balancers"),
        Some(fabric.admin.clone()),
    );
    // Three passes, one write each: the guard that makes a delete releasable,
    // then the address, then the fabric. That is the arrangement, not a
    // workaround — a controller that carried on with a copy it had just written
    // would be acting on a revision it might not hold.
    for _ in 0..3 {
        sweep(&c, &floating).await.unwrap();
    }

    // Asked again until it is there, rather than once and immediately.
    //
    // The controller has told the fabric; the fabric applies through Raft, and
    // append-commit-apply is asynchronous even for a single bootstrapped node.
    // Reading straight after the push therefore sometimes reads the moment
    // before it landed — which failed perhaps one full-workspace run in twenty,
    // always here, always looking like the controller had done nothing.
    //
    // The assertion is unchanged: still exactly one, still that address. This
    // only allows the system the moment it needs to get there.
    let mut fips = Vec::new();
    for _ in 0..50 {
        fips = client
            .list_floating_ips(pb::ListFloatingIpsRequest {})
            .await
            .unwrap()
            .into_inner()
            .floating_ips;
        if fips.len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fips.len(), 1, "{fips:?}");
    // Not an address a port holds — .1 is the gateway, .10 and .11 are the two
    // ports, so the lowest free one is .2.
    assert_eq!(fips[0].addr, "10.40.0.2");
    assert_eq!(fips[0].assoc_fixed, "10.40.0.10");

    let after = floating.get(FIP).await.unwrap().unwrap();
    assert_eq!(after.spec.address.as_deref(), Some("10.40.0.2"));
    assert_eq!(after.status.fabric_id, fips[0].id);
    assert_eq!(after.status.associated, "10.40.0.10");
    assert!(
        format!("{:?}", after.status.conditions).contains("Forwarding"),
        "{:?}",
        after.status.conditions
    );

    // A settled floating IP costs nothing.
    let before = after.meta.revision;
    sweep(&c, &floating).await.unwrap();
    assert_eq!(
        floating.get(FIP).await.unwrap().unwrap().meta.revision,
        before,
        "a settled floating IP was written again"
    );

    // Now move it. An association is one-to-one, so this only works if the old
    // one is released first — an implementation that only ever associates
    // leaves the address on the machine that was replaced.
    let mut moved = floating.get(FIP).await.unwrap().unwrap();
    moved.spec.port = "projects/p1/ports/web2".into();
    moved.meta.generation += 1;
    floating
        .update(
            &moved,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    sweep(&c, &floating).await.unwrap();

    let fips = client
        .list_floating_ips(pb::ListFloatingIpsRequest {})
        .await
        .unwrap()
        .into_inner()
        .floating_ips;
    assert_eq!(fips.len(), 1, "the move left a second allocation: {fips:?}");
    assert_eq!(
        fips[0].assoc_fixed, "10.40.0.11",
        "the address still reaches the port it was moved off"
    );
    assert_eq!(fips[0].addr, "10.40.0.2", "the address itself moved");

    // And detaching is an ordinary state, not a deletion: the address is still
    // allocated and still this operator's, reaching nothing.
    let mut held = floating.get(FIP).await.unwrap().unwrap();
    held.spec.port = String::new();
    held.meta.generation += 1;
    floating
        .update(
            &held,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    sweep(&c, &floating).await.unwrap();

    let fips = client
        .list_floating_ips(pb::ListFloatingIpsRequest {})
        .await
        .unwrap()
        .into_inner()
        .floating_ips;
    assert_eq!(fips.len(), 1);
    assert_eq!(fips[0].addr, "10.40.0.2", "detaching released the address");
    assert!(
        fips[0].assoc_port.is_empty(),
        "the address still reaches a port: {:?}",
        fips[0]
    );
    let after = floating.get(FIP).await.unwrap().unwrap();
    assert!(after.status.associated.is_empty());
    assert!(format!("{:?}", after.status.conditions).contains("Held"));

    // And deleting the record hands the address back. Without the finalizer the
    // record would go and the allocation would stay — an address the fabric
    // holds that nothing in the control plane can name, and the only symptom is
    // a subnet that fills up.
    assert!(
        after.meta.has_finalizer(FABRIC_RELEASE_FINALIZER),
        "nothing guards the allocation against the record disappearing"
    );
    let mut deleting = after;
    deleting.meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp::now());
    floating
        .update(
            &deleting,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    sweep(&c, &floating).await.unwrap();

    assert!(
        client
            .list_floating_ips(pb::ListFloatingIpsRequest {})
            .await
            .unwrap()
            .into_inner()
            .floating_ips
            .is_empty(),
        "the fabric still holds an address whose record is being deleted"
    );
    let gone = floating.get(FIP).await.unwrap().unwrap();
    assert!(
        !gone.meta.has_finalizer(FABRIC_RELEASE_FINALIZER),
        "the guard stayed on after the fabric let go, pinning the record forever"
    );
}

/// A **routed** address is a different thing on the fabric, and this is what
/// makes that a property rather than a claim: nothing is allocated as a
/// floating IP and nothing is associated — the address is bound to the port, so
/// the datapath accepts it as a source from that port and delivers it as a
/// destination to it. That is what "the guest holds the address" means down
/// here.
#[tokio::test]
async fn a_routed_address_is_bound_to_the_port_rather_than_translated() {
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    let Some(fabric) = Fabric::start(&binary, 50985).await else {
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
    let writer = velstra_cloud_model::access::Writer::controller("test");

    networks
        .create(
            &Resource::new(
                meta(NETWORK),
                NetworkSpec {
                    host_bridge: String::new(),
                    vni: VNI,
                    mtu: 1500,
                    // The prefix on this network's subnets is real. The fabric
                    // is told nothing about that — what "external" means is a
                    // fact about the world above it.
                    external: true,
                    announce: velstra_cloud_model::public::Announce::FromHost,
                },
                NetworkStatus::default(),
            ),
            &writer,
        )
        .await
        .unwrap();
    subnets
        .create(
            &Resource::new(
                meta(SUBNET),
                SubnetSpec {
                    network: NETWORK.into(),
                    cidr: "10.40.0.0/24".into(),
                    gateway: "10.40.0.1".into(),
                    dns: vec![],
                    reserved: vec![],
                },
                SubnetStatus::default(),
            ),
            &writer,
        )
        .await
        .unwrap();

    let mut port = Resource::new(
        meta("projects/p1/ports/web"),
        PortSpec {
            network: NETWORK.into(),
            subnet: SUBNET.into(),
            address: Some("10.40.0.10".into()),
            ..Default::default()
        },
        PortStatus::default(),
    );
    port.status.node = Some(HOST.into());
    port.status.tap_device = Some("vt-web".into());
    ports.create(&port, &writer).await.unwrap();

    let net_controller = NetworkController::new(
        raw.clone(),
        CELL,
        subnets.clone(),
        Some(fabric.admin.clone()),
    );
    sweep(&net_controller, &networks).await.unwrap();

    let mut client = velstra_cloud_fabric::connect(&fabric.admin).await.unwrap();
    client
        .add_host(pb::HostSpec {
            id: HOST.into(),
            vtep: "10.0.0.1".into(),
            underlay_iface: "eth0".into(),
            underlay_mac: "02:00:00:00:00:01".into(),
            encap: pb::Encap::Vxlan as i32,
            udp_port: 0,
            underlay_mtu: 0,
            srv6_locator: String::new(),
        })
        .await
        .expect("declaring the host");
    client
        .create_port(pb::CreatePortRequest {
            network: VNI,
            host: HOST.into(),
            tap: "vt-web".into(),
            ip: "10.40.0.10".into(),
            policy: None,
            mac: None,
        })
        .await
        .expect("creating the fabric port");

    floating
        .create(
            &Resource::new(
                meta(FIP),
                FloatingIpSpec {
                    instance: String::new(),
                    subnet: SUBNET.into(),
                    address: None,
                    port: "projects/p1/ports/web".into(),
                    delivery: velstra_cloud_model::public::Delivery::Routed,
                    announce: None,
                },
                FloatingIpStatus::default(),
            ),
            &writer,
        )
        .await
        .unwrap();

    let c = FloatingIpController::new(
        raw.clone(),
        CELL,
        floating.clone(),
        subnets.clone(),
        ports.clone(),
        TypedStore::new(raw.clone(), CELL, "load-balancers"),
        Some(fabric.admin.clone()),
    );
    for _ in 0..3 {
        sweep(&c, &floating).await.unwrap();
    }

    let after = floating.get(FIP).await.unwrap().unwrap();
    assert_eq!(after.spec.address.as_deref(), Some("10.40.0.2"));
    // Nothing was allocated as a floating IP: there is nothing to translate,
    // so there is no translation to hold an id for.
    assert!(after.status.fabric_id.is_empty(), "{:?}", after.status);
    assert!(
        format!("{:?}", after.status.conditions).contains("Held By The Guest"),
        "{:?}",
        after.status.conditions
    );
    let fips = client
        .list_floating_ips(pb::ListFloatingIpsRequest {})
        .await
        .unwrap()
        .into_inner()
        .floating_ips;
    assert!(
        fips.is_empty(),
        "a routed address was allocated as a translated one: {fips:?}"
    );

    // And a settled one costs nothing, like everything else here.
    let before = after.meta.revision;
    sweep(&c, &floating).await.unwrap();
    assert_eq!(
        floating.get(FIP).await.unwrap().unwrap().meta.revision,
        before,
        "a settled routed address was written again"
    );
}
