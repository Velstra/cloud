//! The Ceph deployment loop, run for real against recorded commands.
//!
//! ## Why this test substitutes the binaries rather than a trait
//!
//! `cephadm` and `ceph` *are* the interface to Ceph — there is nothing between
//! the agent and them. A trait in the middle would let this test pass while the
//! argv the agent really builds was wrong, which is the exact failure this file
//! exists to catch: the model, the controller and the console all existed for a
//! while with nothing in between that ran a command, and every test was green.
//!
//! So the agent is pointed at a script that records what it was called with, and
//! the assertions are about that recording.

mod common;

use std::sync::Arc;

use common::{CELL, REGION, create_node, meta, nodes, store};
use velstra_cloud_model::{
    ceph::{CephClusterSpec, CephClusterStatus, CephPoolSpec, NodeCeph, OsdSpec},
    resources::Resource,
};
use velstra_cloud_nodeagent::{Agent, AgentConfig, FakeDatapath, FakeVmm, cephadm::CephAdmin};
use velstra_cloud_store::{Store, TypedStore};

/// Held for the length of any test that spawns child processes.
///
/// ## Why this exists, and why it is not papering over anything
///
/// Every test here is `#[tokio::test]`, which builds a **current-thread runtime
/// per test**, and the harness runs them on several threads at once. So this
/// one process ends up with a dozen independent tokio runtimes all spawning and
/// reaping children — and child-exit notification is delivered per process, not
/// per runtime. Under that arrangement a `Command::output()` can come back with
/// a failure that has nothing to do with the command: the pass then reads
/// "cephadm is not here", records nothing, and a test about argv fails about
/// the machine instead.
///
/// It is an artefact of the harness and not a property of the code. Production
/// has exactly one runtime. Serialising the child-spawning tests reproduces that
/// arrangement; it costs nothing, because there are fifteen of them and they
/// take forty milliseconds together.
///
/// The alternative — retrying a pass until something was recorded — would have
/// hidden a real "the pass never got there" bug behind the same retry, which is
/// the failure mode this whole file exists to prevent.
/// An async lock rather than a `std::sync::Mutex`, and not to quieten a lint:
/// this is held across every await in a test body, and a blocking guard held
/// across an await is the shape that deadlocks as soon as somebody moves one of
/// these tests to a multi-threaded runtime. It also cannot be poisoned, so a
/// test that panics while holding it does not take the rest down with it.
static SPAWNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn spawning() -> tokio::sync::MutexGuard<'static, ()> {
    SPAWNING.lock().await
}

/// A stand-in for `cephadm` and `ceph` that records its arguments and succeeds.
///
/// Written to a directory of its own so two tests running at once cannot read
/// each other's recording.
struct Recorder {
    dir: std::path::PathBuf,
}

impl Recorder {
    fn new(name: &str) -> Self {
        // The name is the test's, so two of them running at once cannot read
        // each other's recording. No thread id: its Debug spelling carries
        // brackets, and a path with brackets in it inside an unquoted shell
        // redirect is a syntax error rather than a file.
        // Under this test binary's own directory rather than `/tmp`: cargo
        // gives every test target one, and it is not shared with whatever else
        // happens to be on the machine. A run here failed once because
        // something else had filled `/tmp`, which is a test failure about
        // somebody else's disk.
        //
        // The name is the test's, so two running at once cannot read each
        // other's recording. No thread id: its Debug spelling carries brackets,
        // and a path with brackets in an unquoted shell redirect is a syntax
        // error rather than a file.
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ceph-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("record");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> '{dir}/argv'\n\
                 if [ -f '{dir}/reply-'\"$1\" ]; then cat '{dir}/reply-'\"$1\"; fi\n\
                 exit 0\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn tools(&self) -> CephAdmin {
        let path = self.dir.join("record").to_str().unwrap().to_string();
        // Built from the default rather than as a literal: `CephAdmin` carries
        // private state of its own, so a literal here could not name every
        // field even though the paths below are all this test wants to change.
        let mut tools = CephAdmin::default();
        tools.cephadm = path.clone();
        tools.ceph = path.clone();
        tools.systemctl = path;
        tools.ceph_conf = self.dir.join("ceph.conf").display().to_string();
        tools.authorized_keys = self.dir.join("authorized_keys").display().to_string();
        tools
    }

    /// Pretend this machine already holds a cluster.
    fn already_has_a_cluster(&self) {
        std::fs::write(self.dir.join("ceph.conf"), "[global]\nfsid = abc\n").unwrap();
    }

    /// Pretend the cluster's key is already installed here.
    fn trusts(&self, key: &str) {
        std::fs::write(self.dir.join("authorized_keys"), format!("{key}\n")).unwrap();
    }

    /// What the stand-in prints when its first argument is `verb`.
    ///
    /// This is how a test says "a monitor is running on this machine": the node
    /// reads its own daemons from `systemctl`, never from the cluster, because
    /// a node that has not been added yet has no keyring to ask with.
    fn answers(&self, verb: &str, with: &str) {
        std::fs::write(self.dir.join(format!("reply-{verb}")), with).unwrap();
    }

    fn recorded(&self) -> String {
        std::fs::read_to_string(self.dir.join("argv")).unwrap_or_default()
    }

    /// What is actually in the recorder's directory.
    ///
    /// Only ever read when an assertion about the recording has failed, and
    /// there for one reason: "nothing was recorded" has two causes that look
    /// identical from the recording alone — the pass never ran a command, or
    /// the machine could not run the stand-in at all. The first is a bug in the
    /// code under test and the second is a fact about the machine, and telling
    /// them apart from a CI log is otherwise guesswork.
    fn state(&self) -> String {
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                format!("{} holds {names:?}", self.dir.display())
            }
            Err(e) => format!("{} could not be read: {e}", self.dir.display()),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const KEY: &str = "ssh-ed25519 AAAA cluster";

/// A cell reader that counts how often the node list is asked for.
///
/// The question this answers is not "does it work" but "what does it cost",
/// and that is not visible in any result — a pass that reads the whole cell and
/// a pass that does not produce the same output.
#[derive(Clone)]
struct CountingCell {
    inner: Arc<velstra_cloud_nodeagent::StoreCell>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingCell {
    fn new(store: Arc<dyn Store>, node: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(velstra_cloud_nodeagent::StoreCell::new(store, CELL, node)),
            reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    fn node_reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl velstra_cloud_nodeagent::CellReader for CountingCell {
    async fn nodes(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Node>> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.nodes().await
    }
    async fn ceph_clusters(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::ceph::CephCluster>> {
        self.inner.ceph_clusters().await
    }
    async fn instances(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Instance>> {
        self.inner.instances().await
    }
    async fn attachments(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Attachment>> {
        self.inner.attachments().await
    }
    async fn ports(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Port>> {
        self.inner.ports().await
    }
    async fn migrations(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::migration::Migration>> {
        self.inner.migrations().await
    }
    async fn security_groups(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::security::SecurityGroup>>
    {
        self.inner.security_groups().await
    }
    async fn subnets(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Subnet>> {
        self.inner.subnets().await
    }
    async fn networks(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Network>> {
        self.inner.networks().await
    }
    async fn images(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Image>> {
        self.inner.images().await
    }
    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()> {
        self.inner.wake().await
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

async fn cluster(store: &Arc<dyn Store>, spec: CephClusterSpec) {
    make_cluster(store, spec, CephClusterStatus::default()).await
}

/// A cluster whose key has been published, which is the state every node past
/// the first one meets.
async fn cluster_with_key(store: &Arc<dyn Store>, spec: CephClusterSpec) {
    make_cluster(
        store,
        spec,
        CephClusterStatus {
            ssh_pubkey: KEY.to_string(),
            ..CephClusterStatus::default()
        },
    )
    .await
}

async fn make_cluster(store: &Arc<dyn Store>, spec: CephClusterSpec, status: CephClusterStatus) {
    let clusters: TypedStore<CephClusterSpec, CephClusterStatus> =
        TypedStore::new(store.clone(), CELL, "ceph-clusters");
    clusters
        .create(
            &Resource::new(meta("ceph-clusters/ceph"), spec, status),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

fn spec(monitors: &[&str]) -> CephClusterSpec {
    CephClusterSpec {
        // Loopback, because it is the one network every machine running this
        // test is on — the address the node picks has to come from the machine
        // itself, and inventing one would test the wrong half.
        public_network: "127.0.0.0/8".into(),
        monitors: monitors.iter().map(|m| m.to_string()).collect(),
        osds: vec![],
        pools: vec![CephPoolSpec {
            pool: "velstra-volumes".into(),
            size: 3,
            min_size: 2,
        }],
        ..CephClusterSpec::default()
    }
}

fn agent(store: Arc<dyn Store>, node: &str, tools: CephAdmin) -> Agent {
    machine(store, node, tools, vec![])
}

/// The same agent, on a machine that has disks.
///
/// Given to the VMM rather than written into the node's status, because the
/// pass overwrites this node's own report with what it observes — which is the
/// point, and is what keeps a node from acting on a stale account of itself.
fn machine(
    store: Arc<dyn Store>,
    node: &str,
    tools: CephAdmin,
    devices: Vec<velstra_cloud_model::ceph::BlockDevice>,
) -> Agent {
    Agent::new(
        store,
        AgentConfig::new(node, REGION, CELL),
        Arc::new(FakeVmm::new().with_devices(devices)),
        Arc::new(FakeDatapath::new()),
    )
    .with_ceph_tools(tools)
}

fn free_disk(path: &str) -> velstra_cloud_model::ceph::BlockDevice {
    velstra_cloud_model::ceph::BlockDevice {
        path: path.into(),
        kernel_name: path.rsplit('/').next().unwrap().into(),
        size_gib: 512,
        rotational: false,
        state: velstra_cloud_model::ceph::DeviceUse::Free,
        ..Default::default()
    }
}

fn osd_disk(path: &str) -> velstra_cloud_model::ceph::BlockDevice {
    velstra_cloud_model::ceph::BlockDevice {
        state: velstra_cloud_model::ceph::DeviceUse::Osd {
            id: "on sdb".into(),
        },
        ..free_disk(path)
    }
}

/// The first monitor creates the cluster, and does it with the address it found
/// on the storage network.
///
/// This is the whole loop in one pass: read the cluster, observe this machine,
/// compute the step, run the command. Before this existed, every part of that
/// sentence was written and tested and nothing joined them up.
#[tokio::test]
async fn the_first_monitor_creates_the_cluster_and_the_others_do_not() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "b").await;
    cluster(&store, spec(&["a", "b"])).await;

    let _spawning = spawning().await;
    let recorder = Recorder::new("bootstrap");
    agent(store.clone(), "a", recorder.tools()).resync().await;

    let argv = recorder.recorded();
    assert!(
        argv.contains("bootstrap --mon-ip 127.0.0.1"),
        "the cluster was not created with this machine's own address: {argv}"
    );
    // The safety flags travel with it — a cluster made without them is one
    // nobody would notice until the dashboard turned up.
    assert!(argv.contains("--skip-dashboard"), "{argv}");
    assert!(argv.contains("--skip-monitoring-stack"), "{argv}");

    // And the node published what it found, so the *other* nodes can decide
    // from it rather than from nothing.
    let a = common::read_node(&store, "a").await;
    let ceph = a.status.ceph.expect("this node reported its Ceph");
    assert!(ceph.installed);
    assert_eq!(ceph.address, "127.0.0.1");

    // `b` is named as a monitor and is not the first one: it does nothing at
    // all, which is what keeps two machines from bootstrapping two clusters.
    let quiet = Recorder::new("quiet");
    agent(store.clone(), "b", quiet.tools()).resync().await;
    assert!(
        !quiet.recorded().contains("bootstrap"),
        "a second node created a second cluster: {}",
        quiet.recorded()
    );
}

/// A cell with no Ceph cluster runs no Ceph commands at all.
///
/// The feature is optional and has to stay optional: a platform that shelled out
/// to `cephadm` on every pass of every node would be one that made a cell with
/// directory pools slower and noisier for a feature nobody asked for.
#[tokio::test]
async fn a_cell_without_a_cluster_never_runs_a_ceph_command() {
    let store = store();
    create_node(&store, "a").await;

    let _spawning = spawning().await;
    let recorder = Recorder::new("absent");
    agent(store.clone(), "a", recorder.tools()).resync().await;
    assert_eq!(recorder.recorded(), "");

    let a = common::read_node(&store, "a").await;
    assert!(
        a.status.ceph.is_none(),
        "a node with no cluster reported Ceph anyway"
    );
}

/// A paused deployment stops the loop where it stands.
#[tokio::test]
async fn a_paused_cluster_stops_the_node_from_acting() {
    let store = store();
    create_node(&store, "a").await;
    let mut spec = spec(&["a"]);
    spec.paused = true;
    cluster(&store, spec).await;

    let _spawning = spawning().await;
    let recorder = Recorder::new("paused");
    agent(store.clone(), "a", recorder.tools()).resync().await;
    assert!(
        !recorder.recorded().contains("bootstrap"),
        "a paused deployment carried on: {}",
        recorder.recorded()
    );
    // Still reported, though: pausing stops the platform from acting, not from
    // looking. An operator who paused a deployment to work out what is wrong
    // needs the status more than ever.
    let a = common::read_node(&store, "a").await;
    assert!(a.status.ceph.is_some());
}

/// The step after the cluster exists is placing the monitors, and it is run by
/// the node holding the keyring — naming the whole set, every time.
#[tokio::test]
async fn the_admin_node_places_the_monitors_as_a_set() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "b").await;
    cluster_with_key(&store, spec(&["a", "b"])).await;

    // `a` is up and reachable, `b` has been added to the cluster and has no
    // monitor yet.
    let store_nodes = nodes(&store);
    for (id, monitor) in [("a", true), ("b", false)] {
        let mut node = store_nodes
            .get(&format!("nodes/{id}"))
            .await
            .unwrap()
            .unwrap();
        node.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
        node.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
        node.status.ceph = Some(NodeCeph {
            installed: true,
            monitor,
            address: format!("127.0.0.{}", if id == "a" { 1 } else { 2 }),
            ssh_pubkey: KEY.into(),
            cluster_hosts: vec!["a".into(), "b".into()],
            trusts_key: true,
            ..NodeCeph::default()
        });
        store_nodes
            .update(&node, &velstra_cloud_model::Writer::agent(id))
            .await
            .unwrap();
    }

    let _spawning = spawning().await;
    let recorder = Recorder::new("monitors");
    // `a` sees its own monitor running. This has to come from the machine and
    // not from the stored report: the pass overwrites this node's own status
    // with what it observes, which is the point — a node never acts on a stale
    // account of itself.
    recorder.answers(
        "list-units",
        "ceph-4f3a@mon.a.service loaded active running Ceph mon.a\n",
    );
    recorder.trusts(KEY);
    agent(store.clone(), "a", recorder.tools()).resync().await;
    let argv = recorder.recorded();
    assert!(
        argv.contains("orch apply mon --placement=a,b"),
        "the monitors were not placed as one declarative set: {argv}"
    );
}

/// An OSD is refused when the node no longer offers the disk.
///
/// The one action in this system with no undo. The spec asks for it, the node
/// reports the disk is not free, and nothing runs — the refusal is upstream of
/// the command, so there is no window in which it could have been carried out.
#[tokio::test]
async fn a_disk_that_is_not_free_is_never_handed_to_ceph() {
    let store = store();
    create_node(&store, "a").await;
    let mut spec = spec(&["a"]);
    spec.osds = vec![OsdSpec {
        node: "a".into(),
        device: "/dev/sdb".into(),
    }];
    cluster_with_key(&store, spec).await;

    let store_nodes = nodes(&store);
    let mut node = store_nodes.get("nodes/a").await.unwrap().unwrap();
    node.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
    node.status.ceph = Some(NodeCeph {
        installed: true,
        monitor: true,
        address: "127.0.0.1".into(),
        ssh_pubkey: KEY.into(),
        cluster_hosts: vec!["a".into()],
        trusts_key: true,
        ..NodeCeph::default()
    });
    // The machine reports no disks at all, so `/dev/sdb` is not one it offers.
    node.status.devices = vec![];
    store_nodes
        .update(&node, &velstra_cloud_model::Writer::agent("a"))
        .await
        .unwrap();

    let _spawning = spawning().await;
    let recorder = Recorder::new("osd");
    recorder.answers(
        "list-units",
        "ceph-4f3a@mon.a.service loaded active running Ceph mon.a\n",
    );
    // Without this the host list comes back empty, the pass stops at `AddHost`
    // and never reaches the OSD stage at all — so the assertion below would
    // hold for a reason that has nothing to do with the disk. That is exactly
    // what this test did before, and it survived deleting the rule it exists to
    // prove.
    recorder.answers("orch", r#"[{"hostname":"a"}]"#);
    recorder.trusts(KEY);
    // No disks on the machine, so `/dev/sdb` is not one it offers.
    machine(store.clone(), "a", recorder.tools(), vec![])
        .resync()
        .await;
    assert!(
        recorder.recorded().contains("orch host ls"),
        "the pass never got as far as looking at the cluster, so this proves nothing.\n\
         recorded: {:?}\n{}",
        recorder.recorded(),
        recorder.state()
    );
    assert!(
        !recorder.recorded().contains("daemon add osd"),
        "a disk the node does not offer was handed to Ceph: {}",
        recorder.recorded()
    );
}

/// A machine that already holds a cluster does not create a second one.
///
/// `next_step` guards `Bootstrap` on "no monitor reported anywhere" — but that
/// is a `systemctl` reading, and it goes false while a monitor is restarting,
/// while its unit is `activating`, and whenever systemctl cannot be asked. On a
/// single-monitor cluster nothing else reports a monitor either, so a reboot
/// lands a pass squarely in that window.
///
/// This is the one command in the system with no undo, so the last check is
/// node-local and asks the disk, which does not depend on any daemon's run
/// state.
#[tokio::test]
async fn a_machine_that_already_has_a_cluster_never_bootstraps_a_second_one() {
    let store = store();
    create_node(&store, "a").await;
    cluster(&store, spec(&["a"])).await;

    let _spawning = spawning().await;
    let recorder = Recorder::new("twice");
    // The cluster is here; its monitor is not running this instant, which is
    // what a restart looks like.
    recorder.already_has_a_cluster();
    recorder.answers("list-units", "");

    agent(store.clone(), "a", recorder.tools()).resync().await;
    assert!(
        !recorder.recorded().contains("bootstrap"),
        "a second cluster was created on top of the first: {}",
        recorder.recorded()
    );
}

/// The cluster is read but nothing is asked of a node the spec does not name.
#[tokio::test]
async fn a_node_the_spec_does_not_name_reports_and_stops() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "stranger").await;
    cluster(&store, spec(&["a"])).await;

    let _spawning = spawning().await;
    let recorder = Recorder::new("stranger");
    agent(store.clone(), "stranger", recorder.tools())
        .resync()
        .await;
    // Reading is fine and is what "reports" means; it is the commands that
    // change something that must not appear.
    let argv = recorder.recorded();
    for mutating in [
        "bootstrap",
        "orch host add",
        "orch apply",
        "daemon add osd",
        "pool create",
    ] {
        assert!(
            !argv.contains(mutating),
            "a node outside the spec ran `{mutating}`: {argv}"
        );
    }
    // It still reports — a node that is not in the cluster is exactly the node
    // an operator is about to add to it.
    assert!(
        common::read_node(&store, "stranger")
            .await
            .status
            .ceph
            .is_some()
    );
}

/// A node that does not trust the cluster's key installs it, and installs it
/// itself.
///
/// The one step whose target is a file on the local disk, so it is also the one
/// step no other machine could do — and it has to happen before `orch host add`,
/// which connects over SSH and would otherwise fail with a message about the
/// connection rather than about the key.
#[tokio::test]
async fn a_node_that_cannot_be_reached_yet_installs_the_key_itself() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "b").await;
    cluster_with_key(&store, spec(&["a", "b"])).await;

    // `a` holds the cluster; `b` is next and has nothing yet.
    let store_nodes = nodes(&store);
    let mut a = store_nodes.get("nodes/a").await.unwrap().unwrap();
    a.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
    a.status.ceph = Some(NodeCeph {
        installed: true,
        monitor: true,
        address: "127.0.0.1".into(),
        ssh_pubkey: KEY.into(),
        cluster_hosts: vec!["a".into()],
        trusts_key: true,
        ..NodeCeph::default()
    });
    store_nodes
        .update(&a, &velstra_cloud_model::Writer::agent("a"))
        .await
        .unwrap();

    let _spawning = spawning().await;
    let recorder = Recorder::new("trust");
    agent(store.clone(), "b", recorder.tools()).resync().await;

    let keys = std::fs::read_to_string(recorder.dir.join("authorized_keys")).unwrap_or_default();
    assert!(
        keys.contains(KEY),
        "the node did not install the cluster's key: {keys:?}"
    );
    // And it ran no cluster command doing it: `b` has no keyring to run one
    // with, which is the whole reason this step exists.
    assert!(
        !recorder.recorded().contains("orch host add"),
        "{}",
        recorder.recorded()
    );
}

/// Once a node is reachable, the admin node adds it — with the `_admin` label,
/// because it is a monitor.
///
/// Without that label cephadm never copies the keyring there, and the day the
/// node that bootstrapped the cluster is gone is the day nobody can administer
/// a cluster that is otherwise fine.
#[tokio::test]
async fn a_reachable_monitor_is_added_as_an_administrator() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "b").await;
    cluster_with_key(&store, spec(&["a", "b"])).await;

    let store_nodes = nodes(&store);
    let mut b = store_nodes.get("nodes/b").await.unwrap().unwrap();
    b.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
    b.status.ceph = Some(NodeCeph {
        installed: true,
        address: "127.0.0.2".into(),
        trusts_key: true,
        ..NodeCeph::default()
    });
    store_nodes
        .update(&b, &velstra_cloud_model::Writer::agent("b"))
        .await
        .unwrap();

    let _spawning = spawning().await;
    let recorder = Recorder::new("addhost");
    recorder.answers(
        "list-units",
        "ceph-4f3a@mon.a.service loaded active running Ceph mon.a\n",
    );
    recorder.answers("orch", r#"[{"hostname":"a"}]"#);
    recorder.trusts(KEY);
    agent(store.clone(), "a", recorder.tools()).resync().await;

    let argv = recorder.recorded();
    assert!(
        argv.contains("orch host add b 127.0.0.2 --labels=_admin"),
        "the host was not added as an administrator: {argv}"
    );
}

/// The disk gets consumed, and the pool follows it.
///
/// Two passes, because one pass does one step — and that is the property worth
/// asserting: nothing here plans ahead, so a step that failed is simply asked
/// for again from facts read again.
#[tokio::test]
async fn a_free_disk_becomes_an_osd_and_then_the_pool_is_created() {
    let store = store();
    create_node(&store, "a").await;
    let mut spec = spec(&["a"]);
    spec.osds = vec![OsdSpec {
        node: "a".into(),
        device: "/dev/sdb".into(),
    }];
    cluster_with_key(&store, spec).await;

    let store_nodes = nodes(&store);
    let mut a = store_nodes.get("nodes/a").await.unwrap().unwrap();
    a.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
    a.status.ceph = Some(NodeCeph {
        installed: true,
        monitor: true,
        address: "127.0.0.1".into(),
        ssh_pubkey: KEY.into(),
        cluster_hosts: vec!["a".into()],
        trusts_key: true,
        ..NodeCeph::default()
    });
    store_nodes
        .update(&a, &velstra_cloud_model::Writer::agent("a"))
        .await
        .unwrap();

    let _spawning = spawning().await;
    let recorder = Recorder::new("osd-made");
    recorder.answers(
        "list-units",
        "ceph-4f3a@mon.a.service loaded active running Ceph mon.a\n",
    );
    recorder.answers("orch", r#"[{"hostname":"a"}]"#);
    recorder.trusts(KEY);
    machine(
        store.clone(),
        "a",
        recorder.tools(),
        vec![free_disk("/dev/sdb")],
    )
    .resync()
    .await;
    assert!(
        recorder
            .recorded()
            .contains("orch daemon add osd a:/dev/sdb"),
        "the free disk was not made into an OSD: {}",
        recorder.recorded()
    );

    // The pool comes only after there is somewhere to put it. Ceph will happily
    // make one first, and the result is a pool whose placement groups can never
    // peer — which reads as a broken cluster rather than as an empty one.
    assert!(
        !recorder.recorded().contains("pool create"),
        "the pool was created before there was a disk: {}",
        recorder.recorded()
    );

    // Now the disk *is* an OSD, which the machine reports for itself — the
    // paths have to be the paths the spec named, and only the node holds both.
    let second = Recorder::new("pool-made");
    second.answers(
        "list-units",
        "ceph-4f3a@mon.a.service loaded active running Ceph mon.a\n",
    );
    second.answers("orch", r#"[{"hostname":"a"}]"#);
    second.trusts(KEY);
    machine(
        store.clone(),
        "a",
        second.tools(),
        vec![osd_disk("/dev/sdb")],
    )
    .resync()
    .await;
    let argv = second.recorded();
    assert!(argv.contains("osd pool create velstra-volumes"), "{argv}");
    // With its durability pinned, and its application set — a pool left at the
    // cluster default is one whose durability is whatever somebody set
    // globally, and one without an application warns for ever.
    assert!(
        argv.contains("osd pool set velstra-volumes size 3"),
        "{argv}"
    );
    assert!(
        argv.contains("osd pool set velstra-volumes min_size 2"),
        "{argv}"
    );
    assert!(
        argv.contains("osd pool application enable velstra-volumes rbd"),
        "{argv}"
    );
}

/// The deployment converges with no controller running at all.
///
/// ## The livelock this pins
///
/// Whether root on this machine trusts the cluster's key is a node-local fact,
/// and a node can only report it by checking against a key it knows. If the
/// only place it could learn that key were a field a controller writes, then
/// with no controller every node past the first would report `trusts_key:
/// false` for ever, be handed `TrustKey` for ever, install a key it already
/// has for ever, and the whole deployment would sit on that one step with
/// nothing anywhere saying why.
///
/// The key is also on the node that could ask for it, so the fallback needs
/// nobody's help. This test never writes `CephClusterStatus.ssh_pubkey`.
#[tokio::test]
async fn the_key_reaches_a_node_without_a_controller_publishing_it() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "b").await;
    // Deliberately the plain constructor: the cluster's status is empty, as it
    // is on a cell whose controller is not running.
    cluster(&store, spec(&["a", "b"])).await;

    // `a` holds the cluster and has reported the key on its own status, which
    // is the one place it exists.
    let store_nodes = nodes(&store);
    let mut a = store_nodes.get("nodes/a").await.unwrap().unwrap();
    a.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
    a.status.ceph = Some(NodeCeph {
        installed: true,
        monitor: true,
        address: "127.0.0.1".into(),
        ssh_pubkey: KEY.into(),
        cluster_hosts: vec!["a".into()],
        trusts_key: true,
        ..NodeCeph::default()
    });
    store_nodes
        .update(&a, &velstra_cloud_model::Writer::agent("a"))
        .await
        .unwrap();

    let _spawning = spawning().await;
    let recorder = Recorder::new("nocontroller");
    agent(store.clone(), "b", recorder.tools()).resync().await;

    let keys = std::fs::read_to_string(recorder.dir.join("authorized_keys")).unwrap_or_default();
    assert!(
        keys.contains(KEY),
        "the node never learned the key, so nothing can reach it: {keys:?}"
    );

    // The pass that installed it reported what it saw *before* acting — that
    // order is deliberate, and it is what makes `Bootstrap` safe. So the report
    // that ends the livelock is the next one, and this is the assertion that
    // matters: a node that installed the key and went on reporting
    // `trusts_key: false` would be handed the same step for ever.
    let b = common::read_node(&store, "b").await;
    assert!(
        !b.status.ceph.expect("b reported").trusts_key,
        "the pass reported the result of its own action, which would make Bootstrap unsafe"
    );

    agent(store.clone(), "b", recorder.tools()).resync().await;
    let b = common::read_node(&store, "b").await;
    assert!(
        b.status.ceph.expect("b reported").trusts_key,
        "the node installed the key and still reports that it has not, so it will be handed \
         the same step for ever"
    );
}

/// A node with nothing to do with the cluster does not read the cell.
///
/// The node list is the only cell-wide read this agent makes, and it is paid on
/// every pass by every node for ever — including on a settled cluster that
/// needs nothing. A cell of a thousand machines with Ceph on three would spend
/// all of that on a feature 997 of them have nothing to do with.
///
/// It still reports, because a node that is not in the cluster is exactly the
/// node an operator is about to add to it.
#[tokio::test]
async fn a_node_with_no_business_in_the_cluster_does_not_read_the_cell() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "bystander").await;
    cluster_with_key(&store, spec(&["a"])).await;

    let counting = CountingCell::new(store.clone(), "bystander");
    let _spawning = spawning().await;
    let recorder = Recorder::new("bystander");
    Agent::reading(
        store.clone(),
        AgentConfig::new("bystander", REGION, CELL),
        Arc::new(FakeVmm::new()),
        Arc::new(FakeDatapath::new()),
        counting.clone(),
    )
    .with_ceph_tools(recorder.tools())
    .resync()
    .await;

    assert_eq!(
        counting.node_reads(),
        0,
        "a node outside the cluster read the whole cell's node list"
    );
    assert!(
        common::read_node(&store, "bystander")
            .await
            .status
            .ceph
            .is_some(),
        "it stopped reporting as well, so an operator cannot see its disks"
    );
}

/// A node that runs a monitor does read it, even when the spec has dropped it.
///
/// It still holds the keyring, so it is still a machine that can carry out
/// cluster commands — and the one that would otherwise be chosen to.
#[tokio::test]
async fn a_node_running_a_monitor_reads_the_cell_even_if_the_spec_forgot_it() {
    let store = store();
    create_node(&store, "a").await;
    create_node(&store, "old-monitor").await;
    cluster_with_key(&store, spec(&["a"])).await;

    let counting = CountingCell::new(store.clone(), "old-monitor");
    let _spawning = spawning().await;
    let recorder = Recorder::new("oldmon");
    recorder.answers(
        "list-units",
        "ceph-4f3a@mon.old-monitor.service loaded active running Ceph mon\n",
    );
    Agent::reading(
        store.clone(),
        AgentConfig::new("old-monitor", REGION, CELL),
        Arc::new(FakeVmm::new()),
        Arc::new(FakeDatapath::new()),
        counting.clone(),
    )
    .with_ceph_tools(recorder.tools())
    .resync()
    .await;

    assert_eq!(counting.node_reads(), 1);
}

/// A cell that answers "no Ceph cluster" is checked once, not never and not
/// every pass.
///
/// The empty answer has two causes that look identical here: nobody asked for
/// Ceph, which is almost every cell — and the agent's identity may not read
/// cell-wide collections, so the API narrowed the list to nothing rather than
/// refusing it. The second is the whole feature silently doing nothing, with no
/// error and nothing in a log.
///
/// The first version of this check sat *after* the early return and could never
/// run: `ceph-clusters` is cell-wide too, so the misconfiguration it was written
/// for stops the pass before it. It is now the reason the pass reads the node
/// list at all in that case — once per process, because an identity either may
/// read the cell or it may not, and that does not change while the agent runs.
#[tokio::test]
async fn a_cell_with_no_cluster_is_probed_once_and_then_left_alone() {
    let store = store();
    create_node(&store, "a").await;
    // No cluster at all.

    let counting = CountingCell::new(store.clone(), "a");
    let _spawning = spawning().await;
    let recorder = Recorder::new("probe");
    let agent = Agent::reading(
        store.clone(),
        AgentConfig::new("a", REGION, CELL),
        Arc::new(FakeVmm::new()),
        Arc::new(FakeDatapath::new()),
        counting.clone(),
    )
    .with_ceph_tools(recorder.tools());

    agent.resync().await;
    assert_eq!(
        counting.node_reads(),
        1,
        "the empty answer was taken at face value, so a filtered cell would never be noticed"
    );
    agent.resync().await;
    agent.resync().await;
    assert_eq!(
        counting.node_reads(),
        1,
        "a configuration fact was re-checked every pass, in every cell that has no Ceph"
    );
}

/// A long reconcile interval is obeyed, and the heartbeat is not tied to it.
///
/// These two used to be one number, and that was the bug: a node's heartbeat
/// happens to be written by the reconcile pass, so lengthening the interval
/// made every node in the cell read as permanently gone — and a blocked step
/// halts everything behind it, silently, in every configuration except the
/// single-node one somebody would actually test.
///
/// The first fix shortened the reconcile interval to match, which is the wrong
/// end: an operator who lengthens it is cutting *list* load, and overriding
/// that hands them twenty times the load they were avoiding, in a cell that may
/// have no Ceph cluster at all. So the cadences are separate. The heartbeat is
/// one read and one write of this node's own object; none of the list calls
/// that a long interval was chosen to avoid.
#[tokio::test]
async fn a_long_reconcile_interval_is_obeyed_and_the_heartbeat_is_not() {
    use velstra_cloud_model::ceph::longest_useful_resync_ms;

    let mut config = AgentConfig::new("a", REGION, CELL);
    config.resync = std::time::Duration::from_secs(600);
    let agent = Agent::new(
        store(),
        config,
        Arc::new(FakeVmm::new()),
        Arc::new(FakeDatapath::new()),
    );
    assert_eq!(
        agent.resync_interval(),
        std::time::Duration::from_secs(600),
        "the operator's interval was overridden by a feature they may not use"
    );
    assert_eq!(
        agent.heartbeat_interval(),
        std::time::Duration::from_millis(longest_useful_resync_ms()),
        "the heartbeat would go stale, so every node in the cell reads as gone"
    );

    // A short interval is left entirely alone: the heartbeat never runs *more*
    // often than the pass that already writes one.
    let mut config = AgentConfig::new("a", REGION, CELL);
    config.resync = std::time::Duration::from_secs(5);
    let agent = Agent::new(
        store(),
        config,
        Arc::new(FakeVmm::new()),
        Arc::new(FakeDatapath::new()),
    );
    assert_eq!(agent.resync_interval(), std::time::Duration::from_secs(5));
    assert_eq!(
        agent.heartbeat_interval(),
        std::time::Duration::from_secs(5)
    );
}
