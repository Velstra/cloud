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

/// Fails when the SRv6 end-to-end case could not run at all, unless somebody
/// said that is fine.
///
/// The other skips in this file guard a *datapath* the unit tests in
/// `src/fabric.rs` already cover from the cloud side. SRv6 is different: the
/// config served back — with its derived service SIDs — is the fabric's own
/// logic, reachable only over gRPC, so there is no unit fallback for it. A CI
/// that never built the fabric would skip
/// [`a_node_that_states_a_locator_is_served_an_srv6_overlay`] and go green,
/// which looks exactly like a run that proved SRv6 works. This turns that into
/// one loud failure. Set `VELSTRA_FABRIC_OPTIONAL=1` to accept the gap
/// deliberately.
#[test]
fn the_srv6_overlay_was_actually_tested() {
    if controller_binary().is_some() {
        return;
    }
    assert!(
        std::env::var("VELSTRA_FABRIC_OPTIONAL").is_ok(),
        "the fabric controller is not built, so the SRv6 end-to-end case skipped \
         and nothing exercised the served SRv6 config or its derived SIDs. A green \
         run here would look identical to one that proved SRv6 works, which is why \
         this one is red. Build it (cargo build --manifest-path ../fabric/Cargo.toml), \
         or set VELSTRA_FABRIC_OPTIONAL=1 to accept the gap."
    );
}

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
    /// The agent-facing channel. A node's *config* is served here, and that is
    /// where a host declaration has to end up to have meant anything.
    control: String,
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
            control: format!("http://127.0.0.1:{listen}"),
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

    async fn control(
        &self,
    ) -> Result<
        pb::velstra_control_client::VelstraControlClient<tonic::transport::Channel>,
        tonic::transport::Error,
    > {
        pb::velstra_control_client::VelstraControlClient::connect(self.control.clone()).await
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
        mtu: 1450,
        srv6_locator: None,
    };
    let datapath = FabricDatapath::new(
        TapDatapath::new("vt", None),
        &fabric.admin,
        "node-a",
        underlay,
    );

    // Built field by field, with **no** `..Default::default()`. A fixture that
    // leaves a field at its default cannot tell "it travelled" from "nothing was
    // there to travel", and every assertion about that field then passes for the
    // wrong reason. The destructuring below is the other half: adding a field to
    // `PortSpec` is a compile error here until somebody says where it goes.
    let spec = PortSpec {
        network: "projects/p1/networks/n1".into(),
        subnet: "projects/p1/subnets/s1".into(),
        node: Some("node-a".into()),
        address: Some("10.20.0.7".into()),
        mac: Some("02:ab:cd:ef:00:07".into()),
        security_groups: vec!["projects/p1/security-groups/web".into()],
        // Carried all the way down to the datapath and, until this was fixed,
        // dropped there — the tap datapath refuses a ceiling it cannot enforce
        // and says so, and this one, which can, silently ignored it.
        rate_limit_mbit: Some(250),
    };
    {
        // Where each field goes. Not a check that runs — a decision that cannot
        // be skipped, because the pattern has no `..` and the compiler will not
        // let a new field past it unnamed.
        //
        // This is the guard that was missing when `rate_limit_mbit` was carried
        // all the way here and dropped: everything else about the port arrived,
        // so nothing looked wrong.
        let PortSpec {
            network,
            subnet,
            node,
            address,
            mac,
            security_groups,
            rate_limit_mbit,
        } = &spec;
        // Deliberately **not** sent: the fabric is told the VNI, and a resource
        // name means nothing to it.
        let _ = network;
        // Deliberately not sent: the subnet is how *this* side decided the
        // address; the fabric is told the address itself.
        let _ = subnet;
        // Deliberately not sent: which node holds the port is what makes this
        // agent the one programming it, not something the request carries — the
        // host it declares is its own.
        let _ = node;
        // Sent, and asserted above, every one of them.
        assert!(address.is_some() && mac.is_some());
        assert!(!security_groups.is_empty());
        assert!(rate_limit_mbit.is_some());
    }
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
    assert_eq!(
        mine.rate_limit_mbit,
        Some(250),
        "the send ceiling never reached the fabric, so one guest can take the node's network"
    );

    // The underlay MTU, read back where it actually matters: the config this
    // node is served. The fabric derives the overlay MTU and the MSS clamp from
    // it and reads an absent one as 1500 — so a node on a 1450-byte underlay
    // (every guest-in-a-guest cloud) was clamping to a size its own path cannot
    // carry, and the symptom is large transfers that hang on some paths rather
    // than a network that is visibly broken.
    let config = fabric
        .control()
        .await
        .expect("the agent-facing channel")
        .get_config(pb::NodeRequest {
            node_id: "node-a".into(),
        })
        .await
        .expect("asking for this node's config")
        .into_inner();
    assert_eq!(
        config.overlay.as_ref().map(|o| o.underlay_mtu),
        Some(1450),
        "this node's real underlay MTU never reached the config it is served"
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

    // ---- and now the question the agent actually asks every pass ----------
    //
    // "Is this port still current?" The agent used to answer it by comparing
    // the rules the datapath *reported* with the ones it wanted. This datapath
    // cannot report them: it observes through the tap layer, which is programmed
    // with no rules on purpose because the fabric is what enforces them. So the
    // comparison was `[] == [something]` on every pass, for ever — and starting
    // a guest is gated on its ports being current, so an instance with any
    // security group never started on a real fabric.
    let observed = datapath.observe().await.expect("observing this machine");
    let mine = observed
        .get(PORT)
        .expect("the port this machine is carrying");
    assert!(
        mine.rules.is_empty(),
        "this datapath cannot report its rules, and a test that expected it to would be \
         testing a datapath that does not exist"
    );
    assert!(
        datapath.agrees(PORT, mine, &fewer),
        "the fabric holds exactly these rules and the datapath said otherwise"
    );

    // The range is the case a naive inverse could never get right: two rules in
    // become three out, so comparing what the fabric holds against what was
    // asked for has to happen in the fabric's vocabulary.
    datapath
        .program(PORT, &spec, &network, &rules)
        .await
        .expect("restating with a range");
    let observed = datapath.observe().await.expect("observing again");
    let mine = observed.get(PORT).expect("still carried");
    assert!(
        datapath.agrees(PORT, mine, &rules),
        "a rule carrying a port range never compares equal to the rules it became"
    );
    // And a different set is *not* agreed, or the answer would be worthless.
    assert!(
        !datapath.agrees(PORT, mine, &fewer),
        "this datapath agrees with anything"
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
            mtu: 1450,
            srv6_locator: None,
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
            srv6_locator: String::new(),
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
            mtu: 1450,
            srv6_locator: None,
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

/// A node that states an SRv6 locator is served an SRv6 overlay, end to end
/// through a real fabric controller.
///
/// The path this walks is the one that was previously impossible: the node agent
/// declares its wire family, the fabric stores it, and the config the node is
/// served back carries an SRv6 endpoint with derived service SIDs. Before this,
/// `NodeConfig` had no SRv6 message at all and the agent's own config conversion
/// hardcoded `srv6: None` on the way back in — so a fabric could not have served
/// an SRv6 config even if something had asked for one.
///
/// The two assertions that matter most are the negative ones. An SRv6 node must
/// get *no* VXLAN endpoint (two overlays at once is refused by the config's own
/// validation, so a node served both would fail to apply anything at all), and
/// its two service SIDs for one segment must differ (one SID means one behaviour;
/// equal SIDs would have every broadcast bridged to a single MAC).
#[tokio::test]
async fn a_node_that_states_a_locator_is_served_an_srv6_overlay() {
    let Some(binary) = controller_binary() else {
        eprintln!("skipped: build the fabric controller first (cargo build in ../fabric)");
        return;
    };
    let Some(fabric) = Fabric::start_on(&binary, 50971).await else {
        eprintln!("skipped: the fabric controller did not come up");
        return;
    };
    let mut client = fabric.client().await.expect("a client");

    const VNI: u32 = 5003;
    client
        .add_network(pb::NetworkSpec {
            vni: VNI,
            name: "srv6".into(),
            subnet: "10.40.0.0/24".into(),
            default_action: pb::Action::Drop as i32,
            drop_icmp: false,
        })
        .await
        .expect("declaring the network");

    // Exactly what `FabricDatapath::declare_host` sends for a node started with
    // `--fabric-srv6-locator`: the family follows the locator, never a separate
    // flag the two could disagree about.
    let underlay = Underlay {
        vtep: "127.0.0.1".into(),
        iface: "lo".into(),
        mac: "02:00:00:00:00:01".into(),
        mtu: 1450,
        srv6_locator: Some("fc00:0:7::/64".into()),
    };
    client
        .add_host(pb::HostSpec {
            id: "node-s".into(),
            vtep: underlay.vtep.clone(),
            underlay_iface: underlay.iface.clone(),
            underlay_mac: underlay.mac.clone(),
            encap: pb::Encap::Srv6 as i32,
            srv6_locator: underlay.srv6_locator.clone().unwrap_or_default(),
            udp_port: 0,
            underlay_mtu: underlay.mtu,
        })
        .await
        .expect("declaring the srv6 host");

    // A port is what makes the host *serve* the segment, and therefore what makes
    // it instantiate any SIDs at all.
    client
        .create_port(pb::CreatePortRequest {
            network: VNI,
            host: "node-s".into(),
            tap: "vs0".into(),
            ip: "10.40.0.9".into(),
            policy: None,
            mac: Some("02:ab:cd:ef:00:09".into()),
        })
        .await
        .expect("creating the port");

    let config = fabric
        .control()
        .await
        .expect("the agent-facing channel")
        .get_config(pb::NodeRequest {
            node_id: "node-s".into(),
        })
        .await
        .expect("asking for this node's config")
        .into_inner();

    let srv6 = config
        .srv6
        .as_ref()
        .expect("an srv6 host must be served an srv6 endpoint");
    // Derived from the locator, not stated anywhere: a peer computes the same
    // value from the same locator, which is what lets both ends agree with
    // nothing exchanged.
    assert_eq!(srv6.local_src, "fc00:0:7::");
    assert_eq!(srv6.underlay_iface, "lo");
    assert_eq!(srv6.underlay_mtu, 1450);

    assert!(
        config.overlay.is_none(),
        "an srv6 node must not also be served a VXLAN endpoint: the config refuses both at \
         once, so it would fail to apply anything at all"
    );

    // Both behaviours for the one segment served, on two distinct SIDs.
    let mut sids: Vec<(&str, u32, &str)> = config
        .srv6_local_sids
        .iter()
        .map(|ls| (ls.sid.as_str(), ls.vni, ls.behavior.as_str()))
        .collect();
    sids.sort_unstable();
    assert_eq!(sids.len(), 2, "{sids:?}");
    assert_eq!(sids[0].1, VNI);
    assert_eq!(sids[1].1, VNI);
    assert_ne!(
        sids[0].0, sids[1].0,
        "the unicast and flood SIDs of one segment must differ: RFC 9252 binds a SID to one \
         behaviour, and equal SIDs bridge every broadcast to a single MAC"
    );
    let behaviors: Vec<&str> = sids.iter().map(|s| s.2).collect();
    assert!(behaviors.contains(&"end.dt2u"), "{sids:?}");
    assert!(behaviors.contains(&"end.dt2m"), "{sids:?}");

    // A single-host fabric has nobody to talk to yet, and says so by omission
    // rather than by trusting the underlay: an empty peer set is fail-closed
    // decap, which is the correct state for a host no peer is sending to.
    assert!(srv6.peers.is_empty(), "{:?}", srv6.peers);
    assert!(config.srv6_routes.is_empty());
    assert!(config.srv6_floods.is_empty());
}
