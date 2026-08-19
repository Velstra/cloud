//! [`FabricDatapath`] against a real fabric controller.
//!
//! The unit tests in `src/fabric.rs` ask whether a cloud rule becomes the right
//! fabric rule. This asks the only question they cannot: whether the fabric
//! accepts what this agent sends it. A schema is vendored from another
//! repository, and a copy that compiles is not a copy that is *right* about what
//! the far end expects.
//!
//! Skips loudly rather than failing when either half is missing — the fabric
//! controller has to be built, and creating a tap needs `CAP_NET_ADMIN`:
//!
//!     cargo build --manifest-path ../fabric/Cargo.toml
//!     unshare -Urn cargo test -p velstra-cloud-nodeagent --test fabric_datapath

use std::{path::PathBuf, process::Child, time::Duration};

use velstra_cloud_model::{
    resources::{NetworkSpec, PortSpec},
    security::{Direction, PortRange, Protocol, ResolvedRule},
};
use velstra_cloud_nodeagent::{
    Datapath,
    datapath::TapDatapath,
    fabric::{FabricDatapath, Underlay, pb},
};

const VNI: u32 = 5001;
const PORT: &str = "projects/p1/ports/web";

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

fn may_make_a_tap() -> bool {
    let probe = "vtprobe9";
    let made = std::process::Command::new("ip")
        .args(["tuntap", "add", "dev", probe, "mode", "tap"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if made {
        let _ = std::process::Command::new("ip")
            .args(["tuntap", "del", "dev", probe, "mode", "tap"])
            .status();
    }
    made
}

/// A controller that stops when it drops.
struct Fabric {
    child: Child,
    admin: String,
}

impl Drop for Fabric {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Fabric {
    async fn start(binary: &PathBuf) -> Option<Self> {
        Self::start_on(binary, 50951).await
    }

    /// Its own three ports, so two fixtures in one test binary do not race for
    /// a listener. A fixture that hard-codes one port is a fixture that passes
    /// alone and fails the moment a second test needs the same thing.
    async fn start_on(binary: &PathBuf, base: u16) -> Option<Self> {
        // `lo` is down in a fresh network namespace, and the controller and this
        // test talk to each other over it.
        let _ = std::process::Command::new("ip")
            .args(["link", "set", "lo", "up"])
            .status();
        let dir = std::env::temp_dir().join(format!("velstra-fab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let (listen, admin, raft) = (base, base + 1, base + 2);
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
        // Waited for by asking it something, not by sleeping: a fixture that
        // sleeps is a fixture that is flaky on a busy machine.
        for _ in 0..80 {
            if me.client().await.is_ok() {
                return Some(me);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }

    async fn client(
        &self,
    ) -> Result<
        pb::velstra_orchestrator_client::VelstraOrchestratorClient<tonic::transport::Channel>,
        tonic::transport::Error,
    > {
        pb::velstra_orchestrator_client::VelstraOrchestratorClient::connect(self.admin.clone())
            .await
    }
}

fn rule(protocol: Protocol, from: u16, to: u16) -> ResolvedRule {
    ResolvedRule {
        direction: Direction::Ingress,
        protocol,
        ports: Some(PortRange { from, to }),
        remote: "10.20.0.0/24".into(),
    }
}

#[tokio::test]
async fn a_port_with_rules_reaches_the_fabric() {
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    if !may_make_a_tap() {
        eprintln!("skipped: no CAP_NET_ADMIN here — run under `unshare -Urn`");
        return;
    }
    let Some(fabric) = Fabric::start(&binary).await else {
        eprintln!("skipped: the fabric controller would not start on its ports");
        return;
    };
    let mut client = fabric.client().await.expect("a client");

    // The one thing a node cannot state: a network is a cell-wide fact, not a
    // fact about any machine. In a real cell something above the nodes mirrors
    // it; here the test plays that part, which is exactly the gap it documents.
    client
        .add_network(pb::NetworkSpec {
            vni: VNI,
            name: "blue".into(),
            subnet: "10.20.0.0/24".into(),
            default_action: pb::Action::Drop as i32,
            drop_icmp: false,
        })
        .await
        .expect("declaring the network");

    let underlay = Underlay {
        vtep: "127.0.0.1".into(),
        iface: "lo".into(),
        mac: "02:00:00:00:00:01".into(),
    };
    let datapath = FabricDatapath::new(
        TapDatapath::new("vt", None),
        &fabric.admin,
        "node-a",
        underlay,
    );

    let spec = PortSpec {
        network: "projects/p1/networks/n1".into(),
        subnet: "projects/p1/subnets/s1".into(),
        address: Some("10.20.0.7".into()),
        mac: Some("02:ab:cd:ef:00:07".into()),
        security_groups: vec!["projects/p1/security-groups/web".into()],
        ..PortSpec::default()
    };
    let network = NetworkSpec {
        vni: VNI,
        mtu: 1450,
    };
    let rules = vec![
        rule(Protocol::Tcp, 443, 443),
        rule(Protocol::Tcp, 8000, 8001),
    ];

    let tap = datapath
        .program(PORT, &spec, &network, &rules)
        .await
        .expect("programming a port with rules");
    assert!(tap.starts_with("vt"), "{tap}");

    // What the fabric actually holds, asked of the fabric.
    let ports = client
        .list_ports(pb::ListPortsRequest {})
        .await
        .expect("listing ports")
        .into_inner()
        .ports;
    let mine = ports
        .iter()
        .find(|p| p.tap == tap)
        .expect("the fabric does not have the port this agent just created");
    assert_eq!(mine.vni, VNI);
    assert_eq!(
        mine.ip, "10.20.0.7",
        "the platform's address did not arrive"
    );
    assert_eq!(
        mine.mac, "02:ab:cd:ef:00:07",
        "the platform's MAC did not arrive, so port security would drop every frame"
    );
    assert_eq!(
        mine.host, "node-a",
        "the host this agent declared is not the one it used"
    );

    let groups = client
        .list_security_groups(pb::ListSecurityGroupsRequest {})
        .await
        .expect("listing groups")
        .into_inner()
        .groups;
    let group = groups
        .iter()
        .find(|g| g.name == format!("cloud:{PORT}"))
        .expect("the port's rules never reached the fabric");
    // Two rules in, three out: 8000-8001 is a range and the fabric keys one port.
    assert_eq!(group.rules.len(), 3, "{:?}", group.rules);
    let mut ports_allowed: Vec<u32> = group.rules.iter().map(|r| r.port).collect();
    ports_allowed.sort_unstable();
    assert_eq!(ports_allowed, vec![443, 8000, 8001]);
    assert_eq!(
        group.default_action,
        pb::Action::Drop as i32,
        "an empty allowance list has to be a closed port, not an open one"
    );

    // Restating is what keeps a port current as its groups' members come and go.
    // It used to be refused, which left that inexpressible.
    let fewer = vec![rule(Protocol::Tcp, 443, 443)];
    datapath
        .program(PORT, &spec, &network, &fewer)
        .await
        .expect("restating the port's rules");
    let groups = client
        .list_security_groups(pb::ListSecurityGroupsRequest {})
        .await
        .expect("listing groups")
        .into_inner()
        .groups;
    let group = groups
        .iter()
        .find(|g| g.name == format!("cloud:{PORT}"))
        .expect("the group vanished when it was restated");
    assert_eq!(
        group.rules.len(),
        1,
        "restating the rules did not replace them: {:?}",
        group.rules
    );

    let _ = datapath.unprogram(PORT).await;
}

#[tokio::test]
async fn a_rule_the_fabric_cannot_key_leaves_no_port_behind() {
    // The refusal has to happen before anything is created. A port that exists
    // on the fabric with three of its four rules in force is open where its
    // author believes it is closed.
    if !may_make_a_tap() {
        eprintln!("skipped: no CAP_NET_ADMIN here — run under `unshare -Urn`");
        return;
    }
    let datapath = FabricDatapath::new(
        TapDatapath::new("vy", None),
        // Deliberately unreachable: if the refusal did not come first, this
        // would fail with a connection error instead of the sentence about the
        // rule, and the test would still be red — but for the wrong reason.
        "http://127.0.0.1:1",
        "node-a",
        Underlay {
            vtep: "127.0.0.1".into(),
            iface: "lo".into(),
            mac: "02:00:00:00:00:01".into(),
        },
    );
    let unsayable = ResolvedRule {
        direction: Direction::Ingress,
        protocol: Protocol::Any,
        ports: None,
        remote: "10.20.0.0/24".into(),
    };
    let why = datapath
        .program(
            "projects/p1/ports/wide",
            &PortSpec::default(),
            &NetworkSpec {
                vni: VNI,
                mtu: 1450,
            },
            &[unsayable],
        )
        .await
        .expect_err("a rule the fabric cannot key was accepted")
        .to_string();
    assert!(why.contains("every protocol"), "{why}");
    assert!(
        std::process::Command::new("ip")
            .args(["link", "show", "dev", "vywide"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true),
        "a refused port left a tap behind"
    );
}

#[tokio::test]
async fn unprogramming_a_port_leaves_the_fabric_holding_nothing() {
    // The teardown half, which nothing asked the fabric about until now: the
    // one test that ran it ended with `let _ = datapath.unprogram(PORT).await`
    // and then looked at nothing, so it would have passed with the function
    // empty — and the function very nearly was. It removed the tap, asked for
    // the security group to go and threw the answer away, and never mentioned
    // the *port*. The fabric refuses to remove a group a port is still bound
    // to, so the discarded answer was always a refusal and both objects leaked:
    // an address and a MAC allocated for ever, on a host with no such device.
    //
    // Deliberately does not go through `program`, so it needs no `CAP_NET_ADMIN`
    // and runs wherever the fabric controller is built. What it sets up is
    // exactly what `program` leaves behind, and the tap name is the one `program`
    // would have derived — which is also the only handle `unprogram` has on the
    // fabric's port id.
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    let Some(fabric) = Fabric::start_on(&binary, 50961).await else {
        eprintln!("skipped: the fabric controller would not start on its ports");
        return;
    };
    let mut client = fabric.client().await.expect("a client");

    const VNI_2: u32 = 5002;
    client
        .add_network(pb::NetworkSpec {
            vni: VNI_2,
            name: "green".into(),
            subnet: "10.30.0.0/24".into(),
            default_action: pb::Action::Drop as i32,
            drop_icmp: false,
        })
        .await
        .expect("declaring the network");

    let taps = TapDatapath::new("vz", None);
    let tap = taps.tap_for(PORT);
    let group = format!("cloud:{PORT}");

    client
        .add_host(pb::HostSpec {
            id: "node-a".into(),
            vtep: "127.0.0.1".into(),
            underlay_iface: "lo".into(),
            underlay_mac: "02:00:00:00:00:01".into(),
            encap: 0,
            udp_port: 0,
            underlay_mtu: 0,
        })
        .await
        .expect("declaring the host");
    client
        .add_security_group(pb::SecurityGroupSpec {
            name: group.clone(),
            default_action: pb::Action::Drop as i32,
            drop_icmp: false,
            stateful: true,
            blocklist: Vec::new(),
            rules: Vec::new(),
        })
        .await
        .expect("declaring the group");
    let info = client
        .create_port(pb::CreatePortRequest {
            network: VNI_2,
            host: "node-a".into(),
            tap: tap.clone(),
            ip: "10.30.0.9".into(),
            policy: None,
            mac: Some("02:ab:cd:ef:00:09".into()),
        })
        .await
        .expect("creating the port")
        .into_inner();
    client
        .bind_port_security_group(pb::BindPortSecurityGroupRequest {
            port_id: info.id.clone(),
            group: Some(group.clone()),
        })
        .await
        .expect("binding the group");

    let datapath = FabricDatapath::new(
        taps,
        &fabric.admin,
        "node-a",
        Underlay {
            vtep: "127.0.0.1".into(),
            iface: "lo".into(),
            mac: "02:00:00:00:00:01".into(),
        },
    );
    datapath
        .unprogram(PORT)
        .await
        .expect("tearing a port down was refused");

    let ports = client
        .list_ports(pb::ListPortsRequest {})
        .await
        .expect("listing ports")
        .into_inner()
        .ports;
    assert!(
        !ports.iter().any(|p| p.tap == tap),
        "the fabric still holds the port, with its address and MAC allocated: {ports:?}"
    );
    let groups = client
        .list_security_groups(pb::ListSecurityGroupsRequest {})
        .await
        .expect("listing groups")
        .into_inner()
        .groups;
    assert!(
        !groups.iter().any(|g| g.name == group),
        "the port's rules outlived the port: {groups:?}"
    );

    // Asking twice is asking once. A teardown that failed on a port already
    // gone would be retried for ever by a level-triggered agent.
    datapath
        .unprogram(PORT)
        .await
        .expect("tearing down an already-torn-down port was an error");
}
