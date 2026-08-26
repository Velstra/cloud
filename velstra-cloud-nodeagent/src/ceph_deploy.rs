//! This node's share of building the Ceph cluster.
//!
//! ## Every node computes the same step and acts only on its own
//!
//! There is no orchestrator handing out work. Each agent reads the same
//! [`CephClusterSpec`] and the same node reports, runs the same pure function —
//! [`ceph::next_step`] — and does the step only if it is this machine's to do.
//!
//! `next_step` is a function of stored state, so every node computes the same
//! answer; the answer names one executor; the others find it is not them and do
//! nothing. Nobody has to be told, nothing has to be delivered exactly once, and
//! a node that was rebooted mid-step comes back, recomputes, and finds the same
//! step still outstanding.
//!
//! **It is not quite a race-free story, and pretending otherwise would be the
//! kind of comment that costs somebody an afternoon.** Each agent substitutes
//! its own *fresh* self-report before computing — its daemons, its disks and
//! its heartbeat — so two nodes can each work out that they are the admin node.
//! The likeliest trigger is not exotic: an agent that has just restarted has
//! not written a heartbeat yet, so it reads itself as alive and everybody else
//! reads it as gone. That is every deploy and every upgrade. What that costs is bounded: every cluster
//! command here is idempotent — `orch apply mon` is a placement, `host add` and
//! `pool create` succeed on something that exists — except `daemon add osd`,
//! which fails on a device that is already one. So the visible symptom of the
//! race is a counted failure and a step asked for again, never damage.
//!
//! `Bootstrap` is not exposed to it at all: it is always `spec.monitors[0]`,
//! and only that node can match.
//!
//! The alternative — a controller writing a command for a node to obey — needs
//! at-least-once delivery, a record of what was carried out, and an answer for
//! what happens when the node crashes between reading and acting. None of those
//! questions exist here.
//!
//! ## Who executes is not who the step names
//!
//! Only two steps happen *on* the machine they name: creating the cluster, and
//! installing the cluster's SSH key. Everything else is a command to the
//! cluster — `orch host add`, `orch apply mon`, `orch daemon add osd`,
//! `osd pool create` — which names a host as an argument and has to be run
//! where the admin keyring is. So the executor for those is the first reporting
//! monitor, and the step's `node` field is its subject, not its actor.
//!
//! ## Ordering across machines, with no coordination
//!
//! The steps have to happen in an order: the key before the host, the host
//! before a daemon, quorum before storage, an OSD before a pool. That order is
//! enforced by `next_step` returning exactly one step — so a node that would
//! like to make an OSD simply finds that the current step is somebody else's,
//! and waits by doing nothing. The next pass, after that step is reported done,
//! is the one where the OSD is next.
//!
//! It costs a resync interval per step. A cluster comes up in a minute or two
//! rather than in seconds, and in exchange there is no distributed lock, no
//! leader for the deployment, and no state anywhere about what has been
//! attempted.

use std::net::IpAddr;

use velstra_cloud_model::{
    ceph::{BlockDevice, CephCluster, CephObserved, CephStep, DeviceUse, NodeCeph, observe},
    network::Cidr,
    resources::Node,
};

use crate::{cephadm::CephAdmin, host::Result};

/// What this node should do about the cluster, if anything.
///
/// Split from doing it so the decision can be exercised without a cluster: the
/// interesting cases here are all "did this node correctly decide the step is
/// not its business", and those need no Ceph at all.
pub fn my_step(me: &str, cluster: &CephCluster, nodes: &[Node]) -> Option<CephStep> {
    // The published key is a witness that a cluster exists, where "a monitor is
    // running" is a daemon reading that can go false — and the step that
    // follows from a false one is the irreversible one. See
    // [`velstra_cloud_model::ceph::CephObserved::with_published_key`].
    //
    // Two places carry the key and this takes whichever has it. The
    // controller's field is the better witness because it is never cleared, so
    // it is preferred; but a cell with no controller running has an empty field
    // and nodes that still report the key, and taking those too is what keeps
    // this guard from being the one thing here that needs a controller. It can
    // go empty again if every node with a keyring is lost — which returns the
    // guard to what it was without it, never to a *wrong* answer.
    let key = if cluster.status.ssh_pubkey.is_empty() {
        published_key(nodes)
    } else {
        cluster.status.ssh_pubkey.clone()
    };
    let observed = observe(nodes).with_published_key(&key);
    step_for(
        me,
        &velstra_cloud_model::ceph::next_step(&cluster.spec, &observed),
        &observed,
    )
}

/// Whether `me` is the one to carry out `step`.
fn step_for(me: &str, step: &CephStep, observed: &CephObserved) -> Option<CephStep> {
    let mine = match step {
        // The two that happen on the machine they name.
        CephStep::Bootstrap { node } | CephStep::TrustKey { node, .. } => node == me,
        // The rest are commands to the cluster, run where the keyring is.
        CephStep::AddHost { .. }
        | CephStep::AddMonitor { .. }
        | CephStep::AddOsd { .. }
        | CephStep::CreatePool { .. } => admin_node(observed).as_deref() == Some(me),
        // Nothing to do, or nothing anybody can do.
        CephStep::Settled | CephStep::Paused | CephStep::Blocked { .. } => false,
    };
    mine.then(|| step.clone())
}

/// Whether the cluster's spec names this machine at all.
///
/// Cheap, and it decides whether this node has to read the cell's node list —
/// which is the only cell-wide read the whole agent makes. Every node doing it
/// on every pass is one full collection read per node per resync, for ever,
/// including on a settled cluster that needs nothing. A cell of a thousand
/// machines with Ceph on three would spend all of that on a feature 997 of them
/// have nothing to do with.
///
/// Being named is not the only way to have business here: a node running a
/// monitor holds the keyring and can carry out cluster commands even after an
/// operator has taken it out of the spec. The caller checks that too, from
/// systemd, which is a question about this machine rather than about the cell.
pub fn spec_names(spec: &velstra_cloud_model::ceph::CephClusterSpec, me: &str) -> bool {
    spec.monitors.iter().any(|m| m == me) || spec.osds.iter().any(|o| o.node == me)
}

/// The node that talks to the cluster: the first monitor that is still
/// reporting, by name.
///
/// The same answer on every node **given the same facts**, so nobody has to be
/// told and nothing has to be agreed. The qualifier is real and is the window
/// documented at the top of this module: a node substitutes its own fresh
/// reading of itself before computing, so a machine whose agent has just
/// restarted counts itself alive while every other node still reads its stored
/// `Timestamp(0)` and counts it dead. If that machine is the lowest-named
/// monitor, it picks itself and the others pick somebody else.
///
/// Every monitor carries the `_admin` label (see
/// [`crate::cephadm::add_host_argv`]), so any of them *can* do this.
///
/// **`alive` is what makes that promise real.** A node whose agent is gone
/// keeps its last status for ever, monitor flag and all — so picking the
/// lowest-named monitor without checking would go on choosing a destroyed
/// machine, every other node would find the step is not theirs, and adding a
/// disk to an otherwise healthy cluster would never happen again with nothing
/// anywhere saying why. The label makes another monitor capable; this is what
/// gets one chosen.
fn admin_node(observed: &CephObserved) -> Option<String> {
    let mut monitors: Vec<&str> = observed
        .nodes
        .iter()
        .filter(|n| n.monitor && n.alive)
        .map(|n| n.node.as_str())
        .collect();
    monitors.sort_unstable();
    monitors.first().map(|n| n.to_string())
}

/// Carry out one step.
///
/// Every one of these is idempotent at the Ceph end — `orch apply mon` is
/// declarative, `orch host add` on a known host succeeds, `pool create` on an
/// existing pool succeeds, and an OSD on a device that is already one fails in
/// a way that reports the truth. That is what makes it safe for a node to act
/// again after a reboot mid-step.
///
/// The one that is **not** safely repeatable is `Bootstrap`, and it is not
/// repeated: [`velstra_cloud_model::ceph::next_step`] only returns it while no
/// monitor is up anywhere, and a second cluster bootstrapped on top of the
/// first is the one mistake here that cannot be undone.
pub async fn perform(
    admin: &CephAdmin,
    step: &CephStep,
    cluster: &CephCluster,
    me: &NodeCeph,
) -> Result<()> {
    match step {
        CephStep::Bootstrap { node } => {
            // The last line of defence, and it is node-local on purpose.
            //
            // `next_step` only returns `Bootstrap` while no monitor is reported
            // anywhere — but "reported" is a `systemctl` reading taken on this
            // machine, and it goes false while a monitor is restarting, while
            // its unit is `activating`, and whenever systemctl cannot be asked
            // at all. On a single-monitor cluster nothing else reports a
            // monitor either, so a reboot lands a pass in exactly that window
            // and this is the one command with no undo: a second cluster
            // bootstrapped on top of the first.
            //
            // So the machine is asked whether it already holds a cluster. That
            // question has an answer on disk that does not depend on any
            // daemon's run state, which is precisely what the guard upstream
            // does not have.
            if admin.has_cluster().await {
                tracing::warn!(
                    node,
                    "not creating a cluster: this machine already has one. Something reported \
                     no monitor anywhere — most likely a monitor restarting — and a second \
                     bootstrap on top of the first is the one thing here that cannot be undone."
                );
                return Ok(());
            }
            tracing::info!(node, address = %me.address, "creating the Ceph cluster");
            admin
                .bootstrap(
                    &me.address,
                    &cluster.spec.public_network,
                    &cluster.spec.cluster_network,
                    cluster.spec.monitors.len() == 1,
                )
                .await
        }
        CephStep::TrustKey { node, pubkey } => {
            tracing::info!(node, "trusting the cluster's SSH key");
            trust_key(&admin.authorized_keys, pubkey).await
        }
        CephStep::AddHost {
            node,
            address,
            admin: is_admin,
        } => {
            tracing::info!(node, address, admin = is_admin, "adding the host");
            admin.add_host(node, address, *is_admin).await
        }
        CephStep::AddMonitor { node } => {
            tracing::info!(node, "placing a monitor");
            // The whole set, every time: `orch apply mon` is a placement, so
            // handing it the current list is the level-triggered shape rather
            // than a sequence whose result depends on what ran before.
            admin.apply_monitors(&cluster.spec.monitors).await
        }
        CephStep::AddOsd { node, device } => {
            // Loud, because this erases the disk. By the time it runs the
            // decision has been made three times — the console offered only
            // disks `may_consume` accepts, the operator confirmed, and
            // `next_step` refused again against the node's current inventory.
            tracing::warn!(node, device, "making an OSD, which erases this device");
            admin.add_osd(node, device).await
        }
        CephStep::CreatePool { pool } => {
            tracing::info!(pool = %pool.pool, "creating the pool");
            admin.create_pool(pool).await
        }
        CephStep::Settled | CephStep::Paused | CephStep::Blocked { .. } => Ok(()),
    }
}

/// Put the cluster's key in root's `authorized_keys`, once.
///
/// Append rather than replace: the machine's existing keys are how its operator
/// gets in, and a platform that took that away while adding storage would be
/// remembered for the wrong thing. Idempotent by reading first, because this
/// runs on every pass until the node reports the key is there.
async fn trust_key(path: &str, pubkey: &str) -> Result<()> {
    use crate::host::HostError;

    let key = pubkey.trim();
    if key.is_empty() {
        return Err(HostError::failed(
            "the cluster reported an empty SSH key, and writing that would trust nothing while \
             looking like it worked",
        ));
    }
    let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
    if existing.lines().any(|line| line.trim() == key) {
        return Ok(());
    }
    if let Some(dir) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| HostError::failed(format!("making {}: {e}", dir.display())))?;
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(key);
    next.push('\n');
    tokio::fs::write(path, next)
        .await
        .map_err(|e| HostError::failed(format!("writing {path}: {e}")))
}

/// What this node can say about Ceph on itself.
///
/// Three sources, and which one answers what is not arbitrary:
///
/// * **systemd** for the daemons running here. Node-local, needs no keyring,
///   and true on a machine that cannot talk to the cluster at all — which is
///   every node before it has been added.
/// * **the local disk inventory** for the OSD devices, because the paths have
///   to be the same paths the spec named. Ceph answers `/dev/sdb`; the spec
///   says `/dev/disk/by-id/…`; comparing those would have the deployment ask
///   for an OSD it already made, for ever.
/// * **the cluster** for the pools, the host list and the SSH key. Asking needs
///   the admin keyring — not a manager daemon, which is what the field's own
///   doc used to say — so it is simply attempted and the answer kept if it
///   came. Most nodes report nothing here, and a reader takes the union rather
///   than one node's answer.
///
/// `cluster_key` is the key the cluster published, read from the cluster's own
/// status. It has to come from there and not from what *this* node can ask:
/// only a node holding the admin keyring can ask at all, and the nodes that
/// need to install the key are exactly the ones that cannot.
pub async fn observe_node(
    admin: &CephAdmin,
    public_network: &str,
    devices: &[BlockDevice],
    cluster_key: &str,
    installed: NodeCeph,
    daemons: (bool, bool),
) -> NodeCeph {
    let mut me = installed;
    if !me.installed {
        return me;
    }

    me.address = local_address_in(public_network).await;
    me.osd_devices = devices
        .iter()
        .filter(|d| matches!(d.state, DeviceUse::Osd { .. }))
        .map(|d| d.path.clone())
        .collect();

    // Handed in rather than asked for again: the pass has already had to know
    // whether a monitor runs here, because that is what decides whether this
    // node has any business with the cluster at all.
    let (monitor, manager) = daemons;
    me.monitor = monitor;
    me.manager = manager;

    // Everything below needs the admin keyring, so a node that is not an admin
    // fails here and reports nothing — which is correct, and is why the reader
    // takes a union.
    if let Ok(pools) = admin.pools().await {
        me.pools = pools;
    }
    if let Ok(hosts) = admin.hosts().await {
        me.cluster_hosts = hosts;
    }
    if let Ok(key) = admin.pubkey().await {
        me.ssh_pubkey = key;
    }
    me.trusts_key = trusts(&admin.authorized_keys, cluster_key).await;
    me
}

/// Whether root already trusts this key here.
///
/// An empty key is not trusted, however empty the file is: "there is no key to
/// check" and "the key is installed" are different answers, and conflating them
/// would have the deployment believe a node is reachable before anything was
/// published.
async fn trusts(path: &str, pubkey: &str) -> bool {
    let key = pubkey.trim();
    if key.is_empty() {
        return false;
    }
    tokio::fs::read_to_string(path)
        .await
        .unwrap_or_default()
        .lines()
        .any(|line| line.trim() == key)
}

/// Which Ceph daemons are running on this machine, from systemd.
pub async fn running_daemons(admin: &CephAdmin) -> (bool, bool) {
    let out = tokio::process::Command::new(&admin.systemctl)
        .args([
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "ceph-*",
        ])
        .output()
        .await;
    match out {
        Ok(out) if out.status.success() => parse_units(&String::from_utf8_lossy(&out.stdout)),
        // "systemctl said no" and "systemctl could not be asked" are different
        // answers and this returns the same one for both, because there is no
        // third thing to return: the status field is a boolean and a node that
        // reported nothing would look identical anyway. It is said out loud
        // instead, because the consequence — a monitor that exists reading as
        // absent — is what the bootstrap interlock in `perform` exists to catch.
        other => {
            tracing::warn!(
                ?other,
                "could not ask systemd which Ceph daemons are running here; reporting none"
            );
            (false, false)
        }
    }
}

/// Read `(monitor, manager)` out of `systemctl list-units`.
///
/// cephadm names its units `ceph-<fsid>@<daemon>.<host>.service`, so the daemon
/// kind is between the `@` and the first dot after it. Matched on that rather
/// than on a substring anywhere in the line, because the description column at
/// the end of each line also contains the word `mon`.
pub fn parse_units(text: &str) -> (bool, bool) {
    let mut monitor = false;
    let mut manager = false;
    for line in text.lines() {
        let Some(unit) = line.split_whitespace().next() else {
            continue;
        };
        let Some((_, daemon)) = unit.split_once('@') else {
            continue;
        };
        match daemon.split('.').next() {
            Some("mon") => monitor = true,
            Some("mgr") => manager = true,
            _ => {}
        }
    }
    (monitor, manager)
}

/// This machine's own address inside the cluster's public network.
///
/// The node picks it because the node is the only thing that can see its own
/// interfaces. A machine with four NICs has four answers and the platform has
/// none — and picking the wrong one puts replication traffic on the tenant
/// network, which is the mistake the `public_network` field exists to prevent.
async fn local_address_in(network: &str) -> String {
    let Ok(cidr) = Cidr::parse(network) else {
        return String::new();
    };
    let out = tokio::process::Command::new("ip")
        .args(["-j", "addr"])
        .output()
        .await;
    let json = match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => return String::new(),
    };
    pick_address(&cidr, &json)
}

/// One interface out of `ip -j addr`.
#[derive(serde::Deserialize)]
struct Link {
    #[serde(default)]
    addr_info: Vec<AddrInfo>,
}

#[derive(serde::Deserialize)]
struct AddrInfo {
    #[serde(default)]
    local: String,
}

/// The first address in `ip -j addr` that is inside `cidr`.
///
/// First rather than "the only one": a machine may legitimately hold two
/// addresses on the storage network, and refusing to choose would leave the
/// cluster unable to reach a node over a link that works.
pub fn pick_address(cidr: &Cidr, json: &str) -> String {
    let links: Vec<Link> = match serde_json::from_str(json) {
        Ok(links) => links,
        Err(_) => return String::new(),
    };
    for link in links {
        for addr in link.addr_info {
            if let Ok(ip) = addr.local.parse::<IpAddr>()
                && cidr.contains(ip)
            {
                return addr.local;
            }
        }
    }
    String::new()
}

/// The cluster's SSH key as the nodes themselves report it.
///
/// The same union [`velstra_cloud_model::ceph::observe`] takes, and taken here
/// for the same reason: only a node holding the admin keyring can ask what the
/// key is, so most nodes report nothing and the first non-empty answer is the
/// answer. An empty result means nobody has asked yet, which is the honest
/// state of a cluster that does not exist.
pub fn published_key(nodes: &[velstra_cloud_model::resources::Node]) -> String {
    nodes
        .iter()
        .filter_map(|n| n.status.ceph.as_ref())
        .map(|c| c.ssh_pubkey.clone())
        .find(|k| !k.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        ceph::{CephClusterSpec, CephClusterStatus, CephPoolSpec, OsdSpec},
        meta::{Meta, Placement, ResourceName},
        resources::{NodeSpec, NodeStatus, Resource},
    };

    use super::*;

    fn disk(path: &str, state: DeviceUse) -> BlockDevice {
        BlockDevice {
            path: path.to_string(),
            kernel_name: path.rsplit('/').next().unwrap().to_string(),
            size_gib: 512,
            rotational: false,
            state,
            ..BlockDevice::default()
        }
    }

    fn node(id: &str, ceph: Option<NodeCeph>) -> Node {
        Resource::new(
            Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            NodeSpec {
                evacuate: false,
                vcpu_overcommit: 0,
                fence_after_s: 0,
                schedulable: true,
                labels: vec![],
                cpu_baseline: None,
                gateway: false,
            },
            NodeStatus {
                ceph,
                devices: vec![disk("/dev/sdb", DeviceUse::Free)],
                // Reporting right now, because these tests are about who does
                // what and not about who is up; the liveness rule has its own.
                last_heartbeat: alive_now(),
                ..NodeStatus::default()
            },
        )
    }

    /// A node that is fully reachable: Ceph installed, key trusted, address
    /// known. The state every test that is *not* about reachability starts in.
    fn alive_now() -> velstra_cloud_model::meta::Timestamp {
        velstra_cloud_model::meta::Timestamp::now()
    }

    fn reachable(id: &str, monitor: bool, osds: &[&str]) -> Option<NodeCeph> {
        Some(NodeCeph {
            installed: true,
            monitor,
            trusts_key: true,
            address: format!("10.0.0.{}", id.as_bytes()[id.len() - 1]),
            osd_devices: osds.iter().map(|d| d.to_string()).collect(),
            cluster_hosts: vec!["a".into(), "b".into(), "c".into()],
            // Only a node that can reach the cluster can ask it for its key —
            // which is to say only one that is running a monitor. A fixture
            // where a pre-bootstrap node reports one describes a state that
            // cannot happen, and it would hide the guard that reads the key as
            // proof a cluster exists.
            ssh_pubkey: if monitor {
                "ssh-ed25519 AAAA cluster".into()
            } else {
                String::new()
            },
            ..NodeCeph::default()
        })
    }

    fn cluster(spec: CephClusterSpec) -> CephCluster {
        Resource::new(
            Meta::new(
                ResourceName::parse("ceph-clusters/ceph").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            spec,
            CephClusterStatus::default(),
        )
    }

    fn three_node() -> CephClusterSpec {
        CephClusterSpec {
            public_network: "10.0.0.0/24".into(),
            monitors: vec!["a".into(), "b".into(), "c".into()],
            osds: vec![OsdSpec {
                node: "b".into(),
                device: "/dev/sdb".into(),
            }],
            pools: vec![CephPoolSpec {
                pool: "velstra-volumes".into(),
                size: 3,
                min_size: 2,
            }],
            ..CephClusterSpec::default()
        }
    }

    /// Exactly one node finds the step is its business, and the other two find
    /// it is not.
    ///
    /// This is the whole of the coordination: no lock, no leader for the
    /// deployment, no message. Every node runs the same function over the same
    /// facts and two of them do nothing.
    #[test]
    fn exactly_one_node_acts_on_each_step() {
        let object = cluster(three_node());
        let nodes = vec![
            node("a", reachable("a", false, &[])),
            node("b", reachable("b", false, &[])),
            node("c", reachable("c", false, &[])),
        ];

        let acting: Vec<&str> = ["a", "b", "c"]
            .into_iter()
            .filter(|me| my_step(me, &object, &nodes).is_some())
            .collect();
        assert_eq!(acting, ["a"], "the cluster is a's to create");
    }

    /// The step's subject is not its executor.
    ///
    /// `orch host add c` names `c` and has to run where the admin keyring is —
    /// which is `a`. A node that acted on every step naming it would have `c`
    /// run a command it has no keyring for, and the cluster would never grow.
    #[test]
    fn a_command_to_the_cluster_is_run_by_the_admin_node_not_by_its_subject() {
        let object = cluster(three_node());
        let mut nodes = vec![
            node("a", reachable("a", true, &[])),
            node("b", reachable("b", false, &[])),
            node("c", reachable("c", false, &[])),
        ];
        // `c` has not been added to the cluster yet.
        for n in &mut nodes {
            if let Some(ceph) = n.status.ceph.as_mut() {
                ceph.cluster_hosts = vec!["a".into(), "b".into()];
            }
        }
        assert!(
            my_step("c", &object, &nodes).is_none(),
            "c ran a cluster command it has no keyring for"
        );
        match my_step("a", &object, &nodes) {
            Some(CephStep::AddHost { node, admin, .. }) => {
                assert_eq!((node.as_str(), admin), ("c", true));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The key goes on the machine it is for, and on no other.
    #[test]
    fn the_ssh_key_is_installed_by_the_node_that_needs_to_trust_it() {
        let object = cluster(three_node());
        let mut nodes = vec![
            node("a", reachable("a", true, &[])),
            node("b", reachable("b", false, &[])),
            node("c", reachable("c", false, &[])),
        ];
        if let Some(ceph) = nodes[1].status.ceph.as_mut() {
            ceph.trusts_key = false;
        }
        assert!(my_step("a", &object, &nodes).is_none());
        assert!(my_step("c", &object, &nodes).is_none());
        match my_step("b", &object, &nodes) {
            Some(CephStep::TrustKey { node, pubkey }) => {
                assert_eq!(node, "b");
                assert_eq!(pubkey, "ssh-ed25519 AAAA cluster");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Once the quorum is there, the disk gets consumed — by the admin node,
    /// naming the machine that holds it.
    #[test]
    fn an_osd_is_made_by_the_admin_node_and_names_the_machine_holding_the_disk() {
        let object = cluster(three_node());
        let nodes = vec![
            node("a", reachable("a", true, &[])),
            node("b", reachable("b", true, &[])),
            node("c", reachable("c", true, &[])),
        ];
        assert!(my_step("b", &object, &nodes).is_none());
        assert!(my_step("c", &object, &nodes).is_none());
        match my_step("a", &object, &nodes) {
            Some(CephStep::AddOsd { node, device }) => {
                assert_eq!((node.as_str(), device.as_str()), ("b", "/dev/sdb"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The admin node is a *live* monitor, not merely a monitor.
    ///
    /// A machine whose agent is gone keeps its last status for ever, monitor
    /// flag included. Here `a` is not in the spec at all — an operator took it
    /// out of the monitor list and the machine then died — so nothing blocks on
    /// it, and it is still the lowest-named thing reporting a monitor.
    ///
    /// Choosing it would hand every cluster command to a node that will never
    /// run one, while `b` and `c` compute the same answer and correctly do
    /// nothing. An otherwise healthy cluster that can never be added to again,
    /// with nothing anywhere saying why. The `_admin` label makes another
    /// monitor capable; this is what gets one chosen.
    #[test]
    fn work_goes_to_a_monitor_that_is_still_reporting() {
        let mut spec = three_node();
        spec.monitors = vec!["b".into(), "c".into()];
        let object = cluster(spec);
        let mut nodes = vec![
            node("a", reachable("a", true, &[])),
            node("b", reachable("b", true, &[])),
            node("c", reachable("c", true, &[])),
        ];
        nodes[0].status.last_heartbeat = velstra_cloud_model::meta::Timestamp(0);

        assert!(
            my_step("a", &object, &nodes).is_none(),
            "a node nobody has heard from was handed the work"
        );
        // `b` takes it over, and names the same subject.
        match my_step("b", &object, &nodes) {
            Some(CephStep::AddOsd { node, device }) => {
                assert_eq!((node.as_str(), device.as_str()), ("b", "/dev/sdb"));
            }
            other => panic!("the deployment stopped instead of moving on: {other:?}"),
        }
        assert!(my_step("c", &object, &nodes).is_none());
    }

    /// A cluster is not created twice just because no controller is running.
    ///
    /// The witness that a cluster exists lives in two places: the controller
    /// sets it on the cluster's status and never clears it, and the nodes
    /// themselves report it because asking for it needs a cluster to ask. The
    /// first is the better witness; the second is what stops this guard being
    /// the one thing here that needs a controller.
    ///
    /// The state below is ordinary: `a` holds the cluster and is in it, and its
    /// monitor is restarting this instant, so nothing reports a running monitor
    /// anywhere. Without the node-reported key, that reads as never-bootstrapped
    /// and the answer is `Bootstrap` — the one step with no undo.
    #[test]
    fn a_key_the_nodes_report_is_enough_to_stop_a_second_cluster() {
        // The cluster's own status is empty, which is what a cell with no
        // controller running looks like.
        let object = cluster(three_node());
        assert!(object.status.ssh_pubkey.is_empty());

        let nodes = vec![node(
            "a",
            Some(NodeCeph {
                installed: true,
                // Not running this instant.
                monitor: false,
                trusts_key: true,
                address: "10.0.0.1".into(),
                cluster_hosts: vec!["a".into()],
                // But it has one to report, which it could only have got from a
                // cluster that exists.
                ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
                ..NodeCeph::default()
            }),
        )];

        assert!(
            !matches!(
                my_step("a", &object, &nodes),
                Some(CephStep::Bootstrap { .. })
            ),
            "a second cluster was created on top of one whose key the node was reporting"
        );
    }

    /// A finished cluster is nobody's work.
    #[test]
    fn a_settled_cluster_leaves_every_node_alone() {
        let object = cluster(three_node());
        let mut nodes = vec![
            node("a", reachable("a", true, &[])),
            node("b", reachable("b", true, &["/dev/sdb"])),
            node("c", reachable("c", true, &[])),
        ];
        if let Some(ceph) = nodes[0].status.ceph.as_mut() {
            ceph.pools = vec!["velstra-volumes".into()];
        }
        for me in ["a", "b", "c"] {
            assert!(my_step(me, &object, &nodes).is_none(), "{me} found work");
        }
    }

    /// A paused deployment is nobody's work either, and nothing is torn down.
    #[test]
    fn a_paused_deployment_stops_every_node_where_it_stands() {
        let mut spec = three_node();
        spec.paused = true;
        let object = cluster(spec);
        let nodes = vec![node("a", reachable("a", false, &[]))];
        assert!(my_step("a", &object, &nodes).is_none());
    }

    /// A node the spec does not name never finds work, however much of the
    /// cluster it happens to be running.
    #[test]
    fn a_node_the_spec_does_not_name_is_never_asked_to_do_anything() {
        let object = cluster(three_node());
        let nodes = vec![
            node("a", reachable("a", false, &[])),
            node("stranger", reachable("stranger", false, &[])),
        ];
        assert!(my_step("stranger", &object, &nodes).is_none());
    }

    /// Trusting a key adds it and keeps what was already there.
    ///
    /// The existing keys are how the machine's operator gets in. A platform that
    /// took those away while adding storage would be remembered for the wrong
    /// thing.
    #[tokio::test]
    async fn trusting_a_key_appends_it_and_never_replaces_what_is_there() {
        // Not `/tmp`: this writes an `authorized_keys` file, and a shared
        // directory is the wrong place to be doing that even in a test.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("velstra-ceph-keys-{}", std::process::id()));
        let path = dir.join("authorized_keys");
        let path = path.to_str().unwrap().to_string();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, "ssh-rsa AAAA operator\n")
            .await
            .unwrap();

        trust_key(&path, "ssh-ed25519 BBBB cluster").await.unwrap();
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(after.contains("operator"), "{after}");
        assert!(after.contains("cluster"), "{after}");

        // Twice is once: this runs on every pass until the node reports the key
        // is there, and a file that grew by a line each time would eventually be
        // the only thing on the disk.
        trust_key(&path, "ssh-ed25519 BBBB cluster").await.unwrap();
        assert_eq!(after, tokio::fs::read_to_string(&path).await.unwrap());
        assert!(trusts(&path, "ssh-ed25519 BBBB cluster").await);

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// An empty key is refused rather than written.
    ///
    /// Writing it would leave a file that looks like it has a trusted key, a
    /// node reporting `trusts_key`, and a cluster that cannot reach it.
    #[tokio::test]
    async fn an_empty_key_is_refused_rather_than_trusted() {
        assert!(
            trust_key("/nonexistent/authorized_keys", "  ")
                .await
                .is_err()
        );
        assert!(!trusts("/nonexistent/authorized_keys", "").await);
    }

    /// The daemon kind comes from the unit name, not from anywhere else on the
    /// line.
    ///
    /// `systemctl` puts a description at the end of every line, and it contains
    /// the word `mon`. A substring match would report a monitor on every node
    /// running any Ceph daemon at all — and a node that reports a monitor it
    /// does not have makes the deployment believe it has quorum.
    #[test]
    fn a_daemon_is_read_from_the_unit_name_and_not_from_the_description() {
        let (mon, mgr) = parse_units(
            "ceph-4f3a@osd.2.service loaded active running Ceph osd.2 for 4f3a, see also mon\n",
        );
        assert!(!mon, "the description column was read as a monitor");
        assert!(!mgr);

        let (mon, mgr) = parse_units(
            "ceph-4f3a@mon.hv-1.service loaded active running Ceph mon.hv-1 for 4f3a\n\
             ceph-4f3a@mgr.hv-1.abc.service loaded active running Ceph mgr.hv-1 for 4f3a\n",
        );
        assert!(mon);
        assert!(mgr);
    }

    /// The address is the one inside the storage network, not the first one the
    /// machine has.
    ///
    /// Picking the wrong one puts replication traffic on the tenant network,
    /// which is exactly what `public_network` exists to prevent.
    #[test]
    fn the_address_chosen_is_the_one_on_the_storage_network() {
        let json = r#"[
          {"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},
          {"ifname":"eth0","addr_info":[{"family":"inet","local":"192.168.1.20","prefixlen":24}]},
          {"ifname":"eth1","addr_info":[{"family":"inet","local":"10.0.0.7","prefixlen":24}]}
        ]"#;
        let cidr = Cidr::parse("10.0.0.0/24").unwrap();
        assert_eq!(pick_address(&cidr, json), "10.0.0.7");

        // No address on that network is an empty answer, not a wrong one:
        // `next_step` blocks by name on it rather than adding a host at an
        // address the cluster cannot reach.
        let elsewhere = Cidr::parse("172.16.0.0/16").unwrap();
        assert_eq!(pick_address(&elsewhere, json), "");
    }
}
