//! Standing a Ceph cluster up from the console, and the rules that keep it from
//! destroying anything.
//!
//! ## What this is for
//!
//! Ceph is the difference between a pool per machine and storage the whole cell
//! shares — and installing it by hand across a dozen nodes is the kind of job
//! people put off, do inconsistently, and cannot repeat. Proxmox solved that by
//! making it a screen: pick the nodes, pick the disks, watch it come up. This is
//! the model behind that screen.
//!
//! **Optional, and it has to stay optional.** A cell with directory pools is a
//! working cell. Nothing here is on the path of a platform that never asks for
//! it, and no default turns it on.
//!
//! ## The one thing worth being frightened of
//!
//! Handing a disk to Ceph **erases it**. That is one click in a list, and the
//! list is generated from whatever the machine happens to have plugged in — so
//! the difference between a spare disk and somebody's data is a judgement this
//! file has to make correctly, every time, with no context about what the
//! operator meant.
//!
//! So the rule is inverted from the usual: a device is offered only when it is
//! *provably* free, and everything else is refused **with the reason**. Not
//! greyed out — refused, in words, because "why can I not select this disk" is
//! the question, and an answer of "it has an ext4 filesystem on it" is the whole
//! of the help an operator needs.
//!
//! An operator who genuinely wants to wipe a disk that has something on it can:
//! wipe it themselves, outside the platform, and it becomes eligible. That is a
//! deliberate speed bump on the one action in this system that cannot be undone.
//!
//! ## Everything here is pure
//!
//! What a device looks like, whether it may be consumed, and what the next step
//! toward a cluster is — all functions of what was observed. The agent that runs
//! the steps is dumb on purpose: it reports and it executes, and the decisions
//! are here where they can be exercised without a disk to lose.

use serde::{Deserialize, Serialize};

use crate::{
    meta::Condition,
    resources::{Observed, Resource},
};

// ---- what a node sees of its own disks -------------------------------------

/// Why a block device is not free.
///
/// Every variant carries what an operator needs to act: the filesystem's type,
/// the mount point, the pool it already belongs to. A bare "in use" would leave
/// them running `lsblk` on every node to find out what this already knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DeviceUse {
    /// Nothing on it. The only state from which a device may be consumed.
    ///
    /// The default, and that is the one place in this file where the safe
    /// direction is *not* the conservative one — a device the agent could not
    /// classify must never default to free. It does not: the agent's inventory
    /// classifies every device explicitly and reaches for `Unsuitable` when it
    /// cannot tell. This default exists for `BlockDevice::default()` in tests.
    #[default]
    Free,
    /// It carries a partition table, so something laid it out deliberately.
    Partitioned { partitions: u32 },
    /// A filesystem signature was found on the whole device.
    Filesystem { fstype: String },
    /// It — or a partition of it — is mounted right now.
    Mounted { at: String },
    /// It holds swap, or the root filesystem. Consuming this one takes the
    /// machine down with it.
    System,
    /// It is already a Ceph OSD. Not a refusal so much as an answer: this one is
    /// doing the job.
    Osd { id: String },
    /// It is a member of an MD array, LVM group or ZFS pool.
    Volume { of: String },
    /// Removable, or reporting no size. Neither is a disk to build storage on.
    Unsuitable { why: String },
}

impl DeviceUse {
    /// Whether this state is one a device may be consumed from.
    pub fn is_free(&self) -> bool {
        matches!(self, DeviceUse::Free)
    }
}

/// One block device, as the node that holds it sees it.
///
/// A node's own report about its own hardware — the same shape as
/// `NodeStatus::images` and for the same reason: nobody else can see it, and an
/// aggregate assembled elsewhere would be a guess that goes stale.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDevice {
    /// `/dev/disk/by-id/…` where the machine offers one, `/dev/sdb` otherwise.
    ///
    /// By-id is preferred and it matters: `/dev/sdb` is assigned in discovery
    /// order, so a reboot with one extra disk renames every device after it —
    /// and an OSD spec that named `/dev/sdb` would then point at somebody else's
    /// disk. A stable name is the difference between a repeatable deployment and
    /// a lottery.
    pub path: String,
    /// What the kernel calls it, for an operator matching this against `lsblk`.
    pub kernel_name: String,
    pub size_gib: u64,
    /// Spinning rust or solid state. Shown because mixing them in one pool is a
    /// decision, not an accident.
    pub rotational: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub serial: String,
    pub state: DeviceUse,
}

/// What one node reports about Ceph on itself.
///
/// On the node's own status, because it is the only party that can see whether a
/// daemon is running there. The controller assembles these into the cluster's
/// status; nothing assembles them into a *decision* except
/// [`next_step`], which takes them as observation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCeph {
    /// Whether the tooling is present at all. False means every step involving
    /// this node is blocked, by name, rather than attempted and failed.
    pub installed: bool,
    /// The version of that tooling, for an operator looking at a mixed cell.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// A monitor is running here.
    #[serde(default)]
    pub monitor: bool,
    /// A manager daemon is running here.
    #[serde(default)]
    pub manager: bool,
    /// Devices on this node that are OSDs, by the path they were made from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub osd_devices: Vec<String>,
    /// The pools the cluster holds, as seen from here.
    ///
    /// A property of the cluster rather than of this node — but only a node can
    /// ask, and asking needs the **admin keyring**, not a manager daemon. So
    /// the nodes that have one report what they see and a reader takes the
    /// union; a node that cannot ask reports nothing, which is not evidence
    /// that there are no pools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<String>,
    /// The hosts the orchestrator knows about, reported the same way and for
    /// the same reason as `pools`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cluster_hosts: Vec<String>,

    /// This node's own address inside the cluster's public network.
    ///
    /// The node picks it, because the node is the only thing that can see its
    /// own interfaces — a machine with four NICs has four answers and the
    /// platform has none. It is reported rather than configured so that adding
    /// a node to a Ceph cluster does not require somebody to type an address
    /// that is already sitting in `ip addr`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub address: String,

    /// The cluster's SSH public key, as reported by a node that can ask for it.
    ///
    /// cephadm drives every other host over SSH as root, so this key has to
    /// reach each of them before the cluster can place anything there. It
    /// travels through the status of the node that owns the cluster and back
    /// down into every other node's `authorized_keys` — which is to say it is
    /// published, not delivered, and a node that missed it simply picks it up
    /// on its next pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_pubkey: String,

    /// Whether this node has that key installed for root.
    ///
    /// Reported by the node itself, because "did the key arrive" is a question
    /// only the destination can answer. Without it `ceph orch host add` fails
    /// with a connection error, which is a confusing way to learn that a key
    /// was missing.
    #[serde(default)]
    pub trusts_key: bool,
}

/// The smallest disk worth making an OSD of.
///
/// Below this, the OSD's own metadata is a meaningful fraction of the device and
/// the cluster spends more on bookkeeping than it stores. Ceph itself will
/// happily make one; this refuses so nobody discovers the arithmetic afterwards.
pub const MIN_OSD_GIB: u64 = 20;

/// Whether this device may be handed to Ceph, and if not, why.
///
/// The message is written for the person reading it in a dialog, so it says what
/// is on the disk and what would have to change — never "invalid device".
pub fn may_consume(device: &BlockDevice) -> Result<(), String> {
    match &device.state {
        DeviceUse::Free => {}
        DeviceUse::Partitioned { partitions } => {
            return Err(format!(
                "it has a partition table with {partitions} partition(s) on it. Something laid \
                 this disk out deliberately; wipe it outside the platform if it really is spare."
            ));
        }
        DeviceUse::Filesystem { fstype } => {
            return Err(format!(
                "it holds a {fstype} filesystem. Handing it to Ceph erases that, so it is not \
                 offered until the filesystem is gone."
            ));
        }
        DeviceUse::Mounted { at } => {
            return Err(format!(
                "it is mounted at {at} right now. Whatever is using it is using it."
            ));
        }
        DeviceUse::System => {
            return Err(
                "it holds swap or the root filesystem — consuming it takes this node down."
                    .to_string(),
            );
        }
        DeviceUse::Osd { id } => {
            return Err(format!("it is already OSD {id}."));
        }
        DeviceUse::Volume { of } => {
            return Err(format!(
                "it is a member of {of}. Take it out of that first, if that is really what you \
                 want."
            ));
        }
        DeviceUse::Unsuitable { why } => return Err(why.clone()),
    }
    if device.size_gib < MIN_OSD_GIB {
        return Err(format!(
            "it is {} GiB, and an OSD wants at least {MIN_OSD_GIB}. Below that the OSD's own \
             bookkeeping is a meaningful fraction of the disk.",
            device.size_gib
        ));
    }
    Ok(())
}

// ---- the cluster ------------------------------------------------------------

/// One OSD an operator asked for: a disk, on a node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsdSpec {
    /// The node's bare id — `hv-1`, not `nodes/hv-1`.
    ///
    /// Every reference to a node in this platform is a bare id, because a node
    /// is a cell-scoped root object. One spelling everywhere is what keeps a
    /// reference from having to be re-expanded somewhere, and somewhere is
    /// where it goes wrong.
    pub node: String,
    /// The device path, as the node reported it.
    pub device: String,
    /// Take a device the platform calls unsuitable — removable media, mostly.
    ///
    /// A lab's answer, spelled out per disk so it cannot be a default anybody
    /// inherits: a home cell testing Ceph on the USB stick it has is doing
    /// something legitimate, and a platform that only ever says no to it
    /// teaches people to test nothing. Every *other* refusal stands — a disk
    /// with a filesystem, a mounted disk, the root disk — because those are
    /// not judgement calls; this only waives the "is this sensible hardware"
    /// opinion, and the cluster board still shows what the disk is.
    #[serde(default)]
    pub even_if_unsuitable: bool,
}

/// A pool to create once the cluster is up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CephPoolSpec {
    /// The RBD pool's name in the cluster.
    ///
    /// Not `name`: the wire contract reserves that key for a *resource* name and
    /// converts it into segments on the way past, so a Ceph pool called
    /// `velstra-volumes` came back as `{"segments":["velstra-volumes"]}`. The
    /// field is called what it is instead of fighting the convention.
    pub pool: String,
    /// How many copies of every object. Three is the default everywhere for a
    /// reason: two survives one failure and cannot tell which copy is right when
    /// they disagree.
    #[serde(default = "default_size")]
    pub size: u32,
    /// The fewest copies that may be written to. Below this the pool refuses
    /// writes rather than accepting data it cannot protect.
    #[serde(default = "default_min_size")]
    pub min_size: u32,
}

fn default_size() -> u32 {
    3
}
fn default_min_size() -> u32 {
    2
}

/// What an operator asked for.
///
/// Cell-scoped and effectively a singleton: a cell has one Ceph cluster or none.
/// Not enforced by the type — a second one is refused at the API, where the
/// refusal can say why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CephClusterSpec {
    /// The network the daemons talk to each other and to clients over, as a
    /// CIDR. Stated rather than guessed: a node with four interfaces has four
    /// answers and picking wrong puts replication traffic on the tenant network.
    pub public_network: String,
    /// A separate network for replication, when there is one. Empty means the
    /// public network carries both, which is the ordinary small-cluster answer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cluster_network: String,
    /// Nodes that run a monitor.
    ///
    /// Odd, and at least three for anything that must survive a failure — a
    /// quorum of two tolerates none. One is legitimate for a lab and is refused
    /// by nothing here; [`quorum_advice`] is what says so on screen.
    #[serde(default)]
    pub monitors: Vec<String>,
    #[serde(default)]
    pub osds: Vec<OsdSpec>,
    #[serde(default)]
    pub pools: Vec<CephPoolSpec>,
    /// False pauses the deployment where it stands. Nothing is torn down — the
    /// spec is still what was asked for, and turning it back on resumes.
    ///
    /// A spec field rather than a command, so a controller restart cannot lose
    /// a pause half way and start installing again.
    #[serde(default)]
    pub paused: bool,
}

/// How far along the cluster is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CephPhase {
    /// Nothing has been done yet.
    #[default]
    Pending,
    /// The first monitor is being brought up — until it exists there is no
    /// cluster to add anything to.
    Bootstrapping,
    /// Monitors and OSDs are being added.
    Expanding,
    /// Everything asked for exists.
    Ready,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CephClusterStatus {
    /// The SSH key cephadm drives its hosts with, published for every node to
    /// install.
    ///
    /// It has to travel: cephadm reaches every other machine as root over SSH,
    /// and only the node that owns the cluster can ask what the key is. So the
    /// bootstrap node reports it on its own status, this is where the
    /// controller puts it, and every other node reads it from here and adds it
    /// to root's `authorized_keys`. Published rather than delivered — a node
    /// that missed it picks it up on its next pass, and nothing has to remember
    /// who has been told.
    ///
    /// A public key, so there is nothing here to keep secret.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_pubkey: String,
    pub observed_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    pub phase: CephPhase,
    /// Nodes reporting a running monitor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors_up: Vec<String>,
    /// Nodes reporting a running manager.
    ///
    /// Separate from the monitors because the failure is separate and reads
    /// nothing like it: with no manager the cluster keeps serving I/O and stops
    /// answering questions — `ceph status`, the pool list, the orchestrator.
    /// A cluster whose storage is fine and whose management has quietly gone is
    /// worth being able to see, and every node already reports it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managers_up: Vec<String>,
    /// `(node, device)` pairs reporting a running OSD.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub osds_up: Vec<OsdSpec>,
    /// Pools that exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools_present: Vec<String>,
}

impl Observed for CephClusterStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    /// Nobody: the cluster spans nodes, so no single agent owns it. The
    /// controller writes this status from what the nodes report about
    /// themselves, which is the one case in this platform where a status is
    /// assembled rather than claimed — and it is why the *parts* are still each
    /// reported by whoever can see them.
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl crate::resources::Assigned for CephClusterSpec {}

pub type CephCluster = Resource<CephClusterSpec, CephClusterStatus>;

// ---- what to do next --------------------------------------------------------

/// One step toward the cluster that was asked for.
///
/// Deliberately **one** step per pass, not a plan. A plan computed up front is a
/// plan that is wrong the moment anything fails, and the recovery is then a
/// second code path nobody exercises. One step, re-derived from what is actually
/// there, means a failed step is retried and a half-finished cluster is just a
/// cluster with fewer things in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CephStep {
    /// Nothing to do; everything asked for exists.
    Settled,
    /// The operator paused it.
    Paused,
    /// Create the cluster on this node. Only ever the first monitor.
    Bootstrap { node: String },
    /// Install the cluster's SSH key for root on this node, so the cluster can
    /// reach it. Done by that node, and by nobody else — it is the one step
    /// here whose target is a file on the local disk.
    TrustKey { node: String, pubkey: String },
    /// Tell the orchestrator this host exists, at this address.
    ///
    /// `admin` carries the `_admin` label, which is what makes cephadm
    /// distribute the admin keyring there. Every monitor gets it, so losing the
    /// node that bootstrapped the cluster does not lose the ability to
    /// administer it.
    AddHost {
        node: String,
        address: String,
        admin: bool,
    },
    /// Add a monitor on this node.
    AddMonitor { node: String },
    /// Make an OSD of this device.
    AddOsd { node: String, device: String },
    /// Create this pool.
    CreatePool { pool: CephPoolSpec },
    /// Something is asked for that cannot be done, with the reason.
    ///
    /// **It halts what is left**, and that is a consequence of one step per
    /// pass rather than a choice made for this case: `next_step` returns one
    /// answer, so a blocked item is the answer and nothing after it is reached.
    /// One OSD naming a disk its node will not give up stops every later OSD
    /// and every pool; one node named in the spec that never reports stops the
    /// monitors on the healthy ones too.
    ///
    /// This used to claim the opposite — "a cluster with one impossible OSD
    /// should still bring up everything else" — which is the sort of comment
    /// that makes somebody debug the wrong thing for an hour. Skipping past a
    /// blocked item would need a plan rather than a step, and a plan computed
    /// up front is wrong the moment anything fails.
    ///
    /// It is not an error *return* either: the reason lands on the object,
    /// where an operator is already looking, and the next pass asks again.
    Blocked { why: String },
}

/// What one node reports about its own Ceph daemons.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeCephState {
    pub node: String,
    /// A monitor is running here.
    pub monitor: bool,
    /// A manager is running here.
    pub manager: bool,
    /// Devices on this node that are already OSDs.
    pub osd_devices: Vec<String>,
    /// Whether the node has what it needs to run any of this.
    pub ceph_installed: bool,
    /// The address this node offers for cluster traffic, empty if it has not
    /// found one inside the public network.
    pub address: String,
    /// The cluster's SSH key is installed for root here.
    pub trusts_key: bool,
    /// Whether this node has reported recently enough to be given work.
    ///
    /// Not "is it in the cluster" — a node whose agent is gone keeps its last
    /// status for ever, monitor flag and all. Every command to the cluster is
    /// run by *some* node, and choosing a dead one hands the work to a machine
    /// that will never do it: the others compute the same answer, find it is
    /// not them, and the deployment stops with nothing saying why. The `_admin`
    /// label makes another monitor *capable*; this is what makes one get
    /// *chosen*.
    pub alive: bool,
    /// Devices on this node that [`may_consume`] accepts.
    ///
    /// The safety rule, carried into the decision rather than checked at the
    /// point of execution. The node that runs `orch daemon add osd` is not the
    /// node that holds the disk, so by then it has no inventory to check
    /// against — the only place both facts are in one hand is here, where the
    /// reports have been collected.
    pub consumable: Vec<String>,
    /// Devices whose *only* fault is the platform's taste — `Unsuitable`,
    /// which today means removable media. These may be taken when the spec
    /// says `even_if_unsuitable` for them; a mounted disk or one with a
    /// filesystem is never in this list, so the waiver cannot reach it.
    pub waivable: Vec<String>,
}

/// Everything the controller knows when it decides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CephObserved {
    pub nodes: Vec<NodeCephState>,
    /// Pools that exist in the cluster.
    pub pools: Vec<String>,
    /// Hosts the orchestrator has been told about.
    pub hosts: Vec<String>,
    /// The cluster's SSH public key, empty until some node has reported it.
    pub ssh_pubkey: String,
    /// The cluster exists at all.
    ///
    /// Deliberately **not** gated on [`NodeCephState::alive`], and the reason is
    /// the one asymmetry in this file worth stating twice: gating it would let
    /// a cell whose monitors have all gone quiet read as never-bootstrapped,
    /// and the step that follows from *that* is `Bootstrap` — a second cluster
    /// on top of the first, which is the one thing here with no undo. A stale
    /// report saying a monitor exists is exactly the direction to fail in.
    pub bootstrapped: bool,
}

impl CephObserved {
    /// Take a published SSH key as proof that a cluster was created.
    ///
    /// `bootstrapped` is otherwise a `systemctl` boolean underneath, and a
    /// boolean that can go false is a poor guard for an irreversible step. The
    /// key is monotonic where the daemon reading is not: it exists only because
    /// a cluster was bootstrapped, the controller sets it and never clears it,
    /// and no restart, reboot or unit failure can take it away.
    ///
    /// The window this closes: `spec.monitors[0]` is a node that does not
    /// currently run a monitor — an operator prepended a new first monitor, or
    /// replaced the original — and the node that *does* run the only monitor
    /// reports `false` for one pass. Without this, `monitors[0]` is told to
    /// bootstrap; it has no cluster of its own, so the node-local interlock
    /// does not fire either, and the cell ends up with two.
    pub fn with_published_key(mut self, key: &str) -> Self {
        self.bootstrapped |= !key.trim().is_empty();
        self
    }
}

impl CephObserved {
    fn node(&self, name: &str) -> Option<&NodeCephState> {
        self.nodes.iter().find(|n| n.node == name)
    }
}

/// The next step toward `spec`, given what is there.
///
/// The order is the whole of it, and each step is blocked on the one before for
/// a reason that is not taste:
///
/// 1. **Bootstrap.** Until one monitor exists there is no cluster, and every
///    other command has nothing to talk to.
/// 2. **Monitors.** They form the quorum that decides what the cluster is. Add
///    them before the OSDs so the map every OSD joins is already the real one.
/// 3. **OSDs.** They need somewhere to register.
/// 4. **Pools.** A pool created before there is an OSD is a pool with no
///    placement groups that can be peered, which reads as a broken cluster.
pub fn next_step(spec: &CephClusterSpec, observed: &CephObserved) -> CephStep {
    if spec.paused {
        return CephStep::Paused;
    }
    if spec.monitors.is_empty() {
        return CephStep::Blocked {
            why: "no monitors were chosen, and a cluster with no monitor is not a cluster"
                .to_string(),
        };
    }
    if spec.public_network.trim().is_empty() {
        return CephStep::Blocked {
            why: "no public network was given. A node with several interfaces has several \
                  answers, and picking the wrong one puts replication traffic on the tenant \
                  network."
                .to_string(),
        };
    }

    // 1. The cluster has to exist before anything can join it.
    if !observed.bootstrapped {
        let first = &spec.monitors[0];
        return match observed.node(first) {
            // Liveness before anything else, and it has to be checked *here*
            // rather than only in the stage below, because this stage returns
            // and that one is never reached. `Bootstrap` is always
            // `monitors[0]` — there is no other node that could stand in — so a
            // first monitor whose agent is gone is a cluster that will never be
            // created, and the honest answer is a loud refusal naming the
            // machine, so an operator can pick a different one.
            //
            // Left as "waiting for a to create the cluster", it is the exact
            // sentence that sends somebody to check cephadm on a box where
            // cephadm is fine and the agent is missing.
            Some(state) if !state.alive => CephStep::Blocked {
                why: format!(
                    "{first} was chosen as the first monitor and has not reported for a while. \
                     Its agent, not its Ceph, is what to look at first — or choose a different \
                     first monitor."
                ),
            },
            Some(state) if state.ceph_installed => CephStep::Bootstrap {
                node: first.clone(),
            },
            Some(_) => CephStep::Blocked {
                why: format!("{first} does not have Ceph installed yet"),
            },
            None => CephStep::Blocked {
                why: format!("{first} was chosen as the first monitor and there is no such node"),
            },
        };
    }

    // 2. Every node the spec names has to be reachable by the cluster before
    //    anything can be placed on it. Two steps, in this order, because
    //    `orch host add` connects over SSH and fails with a connection error if
    //    the key is not there yet — a confusing way to learn that a key was
    //    missing.
    for (node, is_admin) in named_nodes(spec) {
        let state = match observed.node(&node) {
            Some(state) => state,
            None => {
                return CephStep::Blocked {
                    why: format!("{node} is named in the cluster and there is no such node"),
                };
            }
        };
        // Order matters, and it is the difference between an operator looking
        // in the right place and the wrong one. A node whose agent is gone
        // reports nothing at all, which reads identically to a node whose agent
        // is running and has no cephadm — and "install cephadm on X" sends
        // somebody to a machine where cephadm is fine and the agent is missing.
        if !state.alive {
            return CephStep::Blocked {
                why: format!(
                    "{node} has not reported for a while. Its agent, not its Ceph, is what to \
                     look at first."
                ),
            };
        }
        if !state.ceph_installed {
            return CephStep::Blocked {
                why: format!("{node} does not have Ceph installed yet"),
            };
        }
        if !state.trusts_key {
            if observed.ssh_pubkey.is_empty() {
                return CephStep::Blocked {
                    why: "the cluster has not reported its SSH key yet, so no other node can be \
                          reached"
                        .to_string(),
                };
            }
            return CephStep::TrustKey {
                node,
                pubkey: observed.ssh_pubkey.clone(),
            };
        }
        if !observed.hosts.contains(&node) {
            if state.address.is_empty() {
                return CephStep::Blocked {
                    why: format!(
                        "{node} has no address inside {}, so the cluster has no way to reach it",
                        spec.public_network
                    ),
                };
            }
            return CephStep::AddHost {
                address: state.address.clone(),
                node,
                admin: is_admin,
            };
        }
    }

    // 3. Quorum before storage.
    for node in &spec.monitors {
        match observed.node(node) {
            Some(state) if state.monitor => continue,
            Some(state) if !state.ceph_installed => {
                return CephStep::Blocked {
                    why: format!("{node} does not have Ceph installed yet"),
                };
            }
            Some(_) => return CephStep::AddMonitor { node: node.clone() },
            None => {
                return CephStep::Blocked {
                    why: format!("{node} was chosen as a monitor and is not reporting"),
                };
            }
        }
    }

    // 4. OSDs.
    for osd in &spec.osds {
        match observed.node(&osd.node) {
            Some(state) if state.osd_devices.contains(&osd.device) => continue,
            Some(state) if !state.ceph_installed => {
                return CephStep::Blocked {
                    why: format!("{} does not have Ceph installed yet", osd.node),
                };
            }
            // The lab waiver, honoured here too: a disk whose only fault is
            // the platform's taste may be taken when the spec says so for it.
            // `waivable` never contains a mounted disk or one with data, so
            // the flag cannot reach those.
            Some(state) if osd.even_if_unsuitable && state.waivable.contains(&osd.device) => {
                return CephStep::AddOsd {
                    node: osd.node.clone(),
                    device: osd.device.clone(),
                };
            }
            // The disk is not one the node offers. Refused rather than
            // attempted, because `orch daemon add osd` on a disk with something
            // on it is the one mistake in this file with no undo — and the node
            // that would run it cannot see the disk to check.
            Some(state) if !state.consumable.contains(&osd.device) => {
                return CephStep::Blocked {
                    why: format!(
                        "{} does not offer {} for an OSD. A disk is offered only when it is \
                         provably empty; anything else has to be wiped outside the platform \
                         first.",
                        osd.node, osd.device
                    ),
                };
            }
            Some(_) => {
                return CephStep::AddOsd {
                    node: osd.node.clone(),
                    device: osd.device.clone(),
                };
            }
            None => {
                return CephStep::Blocked {
                    why: format!("{} holds a chosen disk and is not reporting", osd.node),
                };
            }
        }
    }

    // 5. Pools, once there is somewhere to put them.
    for pool in &spec.pools {
        if !observed.pools.contains(&pool.pool) {
            if observed.nodes.iter().all(|n| n.osd_devices.is_empty()) {
                return CephStep::Blocked {
                    why: format!(
                        "{} cannot be created before there is an OSD to hold it",
                        pool.pool
                    ),
                };
            }
            return CephStep::CreatePool { pool: pool.clone() };
        }
    }

    CephStep::Settled
}

/// Everything the decision needs, read out of what the cell's nodes report.
///
/// One implementation, called by both the controller (which turns it into a
/// status an operator reads) and by every node agent (which turns it into the
/// one step it might have to take). Two implementations of this would be two
/// answers to "is the cluster finished", and the interesting case — they
/// disagree — is one nobody would ever see happen.
///
/// Absent facts are absent, never assumed: a node that reports no Ceph at all
/// is a node with no monitor, no OSDs and no opinion about the pools, and the
/// difference between "reported none" and "did not report" is not one this
/// needs to keep — [`next_step`] blocks by name on a node that is missing
/// entirely, which is the case that matters.
pub fn observe(nodes: &[crate::resources::Node]) -> CephObserved {
    observe_at(nodes, crate::meta::Timestamp::now())
}

/// How long a node may go without reporting before it stops being given work.
///
/// Six resync intervals at the 30-second default. Generous on purpose: the cost
/// of calling a live node dead is that a *different* live node does the work,
/// which is nothing, while the cost of flapping is a step assigned to a
/// different machine every pass. The cost of calling a dead node live is the
/// deployment stopping, so the window has to close eventually — it just does
/// not have to close quickly.
///
/// **This is coupled to the node agent's resync interval and the two are not
/// independent**, which is worth saying out loud because one of them is a
/// command-line flag. A node's heartbeat is written *by* the resync, so the
/// interval is how stale a perfectly healthy node's report gets. Set it above
/// this window and every node in the cell reads as gone: stage 2 blocks on the
/// first stale node it walks, a blocked step halts everything behind it, and
/// nothing ever progresses. The agent enforces the relationship at startup —
/// see [`MIN_HEARTBEATS_PER_WINDOW`] — rather than leaving it to be discovered.
pub const NODE_STALE_AFTER_MS: u64 = 3 * 60 * 1000;

/// How many heartbeats have to fit in the window above.
///
/// One would be enough arithmetically and hopeless in practice: a single missed
/// write — one slow pass, one conflict, one restart — would make a healthy node
/// read as gone. Six leaves room for five to go missing before anything
/// changes its mind about a machine.
pub const MIN_HEARTBEATS_PER_WINDOW: u32 = 6;

/// The longest resync interval that keeps a healthy node looking alive.
pub const fn longest_useful_resync_ms() -> u64 {
    NODE_STALE_AFTER_MS / MIN_HEARTBEATS_PER_WINDOW as u64
}

/// The same, at a stated time.
///
/// Split out so the liveness rule can be exercised without waiting: a test that
/// had to sleep three minutes to prove a node goes stale is a test nobody runs.
pub fn observe_at(nodes: &[crate::resources::Node], now: crate::meta::Timestamp) -> CephObserved {
    let states: Vec<NodeCephState> = nodes
        .iter()
        .map(|node| {
            let ceph = node.status.ceph.as_ref();
            NodeCephState {
                node: node.meta.name.id().to_string(),
                monitor: ceph.is_some_and(|c| c.monitor),
                manager: ceph.is_some_and(|c| c.manager),
                osd_devices: ceph.map(|c| c.osd_devices.clone()).unwrap_or_default(),
                ceph_installed: ceph.is_some_and(|c| c.installed),
                address: ceph.map(|c| c.address.clone()).unwrap_or_default(),
                trusts_key: ceph.is_some_and(|c| c.trusts_key),
                // A node that has never reported at all has `Timestamp(0)`,
                // whose age is the whole of the epoch — dead, which is the
                // right answer for a machine nothing has ever heard from.
                alive: node.status.last_heartbeat.age(now).as_millis()
                    <= u128::from(NODE_STALE_AFTER_MS),
                consumable: node
                    .status
                    .devices
                    .iter()
                    .filter(|d| may_consume(d).is_ok())
                    .map(|d| d.path.clone())
                    .collect(),
                waivable: node
                    .status
                    .devices
                    .iter()
                    .filter(|d| {
                        matches!(d.state, DeviceUse::Unsuitable { .. }) && d.size_gib >= MIN_OSD_GIB
                    })
                    .map(|d| d.path.clone())
                    .collect(),
            }
        })
        .collect();

    // Pools, hosts and the key are properties of the *cluster*, and only a node
    // holding the admin keyring can ask about them. Taking the union rather
    // than one node's answer is what keeps a node that cannot ask from voting
    // "there are none".
    let mut pools: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.status.ceph.as_ref())
        .flat_map(|c| c.pools.iter().cloned())
        .collect();
    pools.sort_unstable();
    pools.dedup();
    let mut hosts: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.status.ceph.as_ref())
        .flat_map(|c| c.cluster_hosts.iter().cloned())
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    let ssh_pubkey = nodes
        .iter()
        .filter_map(|n| n.status.ceph.as_ref())
        .map(|c| c.ssh_pubkey.clone())
        .find(|k| !k.is_empty())
        .unwrap_or_default();

    CephObserved {
        bootstrapped: states.iter().any(|n| n.monitor),
        pools,
        hosts,
        ssh_pubkey,
        nodes: states,
    }
}

/// Every node the cluster needs to reach, in a fixed order, and whether it
/// should carry the `_admin` label.
///
/// Monitors first and in the order they were chosen, so the first monitor — the
/// one that bootstraps — is always the first host in the cluster. OSD-only
/// nodes follow. The order is fixed rather than incidental because every agent
/// computes this list independently and they have to agree on which single step
/// is outstanding.
fn named_nodes(spec: &CephClusterSpec) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = spec.monitors.iter().map(|n| (n.clone(), true)).collect();
    for osd in &spec.osds {
        if !out.iter().any(|(n, _)| n == &osd.node) {
            out.push((osd.node.clone(), false));
        }
    }
    out
}

/// What phase to report, from the same observation.
pub fn phase_of(spec: &CephClusterSpec, observed: &CephObserved) -> CephPhase {
    if !observed.bootstrapped {
        return if spec.monitors.is_empty() {
            CephPhase::Pending
        } else {
            CephPhase::Bootstrapping
        };
    }
    match next_step(spec, observed) {
        CephStep::Settled => CephPhase::Ready,
        _ => CephPhase::Expanding,
    }
}

/// What to tell an operator about the monitor count they picked.
///
/// Advice and not a refusal: one monitor is a perfectly reasonable lab cluster,
/// and a platform that refused it would be wrong about somebody's intent. Two is
/// the one worth being loud about — it is *worse* than one, because a quorum of
/// two tolerates no failures and gives the impression of redundancy.
pub fn quorum_advice(monitors: usize) -> Option<String> {
    match monitors {
        0 => Some("A cluster needs at least one monitor.".to_string()),
        1 => Some(
            "One monitor means the cluster stops when that node does. Fine for a lab, not for \
             anything you would miss."
                .to_string(),
        ),
        2 => Some(
            "Two monitors is worse than one: a quorum of two survives no failures, so either \
             node going down stops the cluster — and it looks redundant. Use three."
                .to_string(),
        ),
        n if n % 2 == 0 => Some(format!(
            "{n} monitors is an even number, so it tolerates the same failures as {} and costs \
             one more node. Use an odd number.",
            n - 1
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &str, gib: u64, state: DeviceUse) -> BlockDevice {
        BlockDevice {
            path: path.to_string(),
            kernel_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            size_gib: gib,
            rotational: false,
            model: String::new(),
            serial: String::new(),
            state,
        }
    }

    /// Only a provably empty disk is offered, and every refusal says what is on
    /// it.
    ///
    /// This is the test that stands between an operator and somebody's data. The
    /// list a console shows comes straight from these answers, so a variant that
    /// slipped through as "free" is a disk erased by one click.
    #[test]
    fn a_disk_is_offered_only_when_it_is_provably_empty() {
        assert!(may_consume(&device("/dev/sdb", 500, DeviceUse::Free)).is_ok());

        let cases = [
            (DeviceUse::Partitioned { partitions: 3 }, "partition table"),
            (
                DeviceUse::Filesystem {
                    fstype: "ext4".into(),
                },
                "ext4",
            ),
            (DeviceUse::Mounted { at: "/srv".into() }, "/srv"),
            (DeviceUse::System, "root filesystem"),
            (DeviceUse::Osd { id: "osd.7".into() }, "osd.7"),
            (DeviceUse::Volume { of: "vg0".into() }, "vg0"),
            (
                DeviceUse::Unsuitable {
                    why: "it is removable".into(),
                },
                "removable",
            ),
        ];
        for (state, expected) in cases {
            let err = may_consume(&device("/dev/sdb", 500, state.clone())).unwrap_err();
            assert!(
                err.contains(expected),
                "{state:?} refused with {err:?}, which does not mention {expected:?}"
            );
        }
    }

    #[test]
    fn a_disk_too_small_to_be_worth_it_is_refused_with_the_number() {
        let err = may_consume(&device("/dev/sdb", MIN_OSD_GIB - 1, DeviceUse::Free)).unwrap_err();
        assert!(err.contains(&format!("{}", MIN_OSD_GIB - 1)), "{err}");
        assert!(err.contains(&MIN_OSD_GIB.to_string()), "{err}");
        // Exactly at the boundary is fine, so the rule is the rule and not an
        // off-by-one.
        assert!(may_consume(&device("/dev/sdb", MIN_OSD_GIB, DeviceUse::Free)).is_ok());
    }

    fn node(name: &str, monitor: bool, osds: &[&str]) -> NodeCephState {
        NodeCephState {
            node: name.to_string(),
            monitor,
            manager: monitor,
            osd_devices: osds.iter().map(|d| d.to_string()).collect(),
            ceph_installed: true,
            alive: true,
            address: format!("10.0.0.{}", name.as_bytes()[0]),
            trusts_key: true,
            // Every test disk is a free one unless the test says otherwise;
            // the refusal has its own test below.
            consumable: vec![
                "/dev/disk/by-id/one".to_string(),
                "/dev/disk/by-id/two".to_string(),
            ],
            waivable: Vec::new(),
        }
    }

    /// An observation of a cluster whose nodes are all already reachable, so a
    /// test about monitors or disks is not first a test about SSH keys.
    fn seen(nodes: Vec<NodeCephState>, pools: &[&str], bootstrapped: bool) -> CephObserved {
        CephObserved {
            hosts: nodes.iter().map(|n| n.node.clone()).collect(),
            ssh_pubkey: "ssh-rsa AAAA cluster".to_string(),
            pools: pools.iter().map(|p| p.to_string()).collect(),
            nodes,
            bootstrapped,
        }
    }

    fn spec() -> CephClusterSpec {
        CephClusterSpec {
            public_network: "10.0.0.0/24".into(),
            monitors: vec!["a".into(), "b".into(), "c".into()],
            osds: vec![
                OsdSpec {
                    node: "a".into(),
                    device: "/dev/disk/by-id/one".into(),
                    even_if_unsuitable: false,
                },
                OsdSpec {
                    node: "b".into(),
                    device: "/dev/disk/by-id/two".into(),
                    even_if_unsuitable: false,
                },
            ],
            pools: vec![CephPoolSpec {
                pool: "velstra-volumes".into(),
                size: 3,
                min_size: 2,
            }],
            ..CephClusterSpec::default()
        }
    }

    /// The whole deployment, one step at a time, in the order that is the only
    /// order that works.
    #[test]
    fn a_cluster_comes_up_in_the_order_its_parts_depend_on() {
        let spec = spec();
        let mut observed = seen(
            vec![
                node("a", false, &[]),
                node("b", false, &[]),
                node("c", false, &[]),
            ],
            &[],
            false,
        );
        // Nothing has been reached yet, whatever the nodes report about
        // themselves.
        observed.hosts.clear();
        observed.ssh_pubkey.clear();
        for n in &mut observed.nodes {
            n.trusts_key = false;
        }

        // Nothing exists: create the cluster, on the first monitor.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::Bootstrap { node: "a".into() }
        );

        // It exists, and bootstrapping reached the local host: the node that
        // made the cluster is already in it and already trusts the key.
        observed.bootstrapped = true;
        observed.nodes[0].monitor = true;
        observed.nodes[0].trusts_key = true;
        observed.hosts.push("a".into());
        observed.ssh_pubkey = "ssh-rsa AAAA cluster".into();

        // Every other node has to be reachable before anything is placed on it,
        // and the key has to arrive before the host is added — `orch host add`
        // connects over SSH.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::TrustKey {
                node: "b".into(),
                pubkey: "ssh-rsa AAAA cluster".into()
            }
        );
        observed.nodes[1].trusts_key = true;
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddHost {
                node: "b".into(),
                address: "10.0.0.98".into(),
                admin: true
            }
        );
        observed.hosts.push("b".into());
        // And the same for the third, before any monitor is placed anywhere.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::TrustKey {
                node: "c".into(),
                pubkey: "ssh-rsa AAAA cluster".into()
            }
        );
        observed.nodes[2].trusts_key = true;
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddHost {
                node: "c".into(),
                address: "10.0.0.99".into(),
                admin: true
            }
        );
        observed.hosts.push("c".into());

        // Quorum before storage — the map an OSD joins should already be the
        // real one.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddMonitor { node: "b".into() }
        );
        observed.nodes[1].monitor = true;
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddMonitor { node: "c".into() }
        );
        observed.nodes[2].monitor = true;

        // Then the disks.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddOsd {
                node: "a".into(),
                device: "/dev/disk/by-id/one".into()
            }
        );
        observed.nodes[0]
            .osd_devices
            .push("/dev/disk/by-id/one".into());
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddOsd {
                node: "b".into(),
                device: "/dev/disk/by-id/two".into()
            }
        );
        observed.nodes[1]
            .osd_devices
            .push("/dev/disk/by-id/two".into());

        // Then the pools, which need somewhere to live.
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::CreatePool {
                pool: spec.pools[0].clone()
            }
        );
        observed.pools.push("velstra-volumes".into());

        assert_eq!(next_step(&spec, &observed), CephStep::Settled);
        assert_eq!(phase_of(&spec, &observed), CephPhase::Ready);
    }

    /// A pool asked for before any disk is blocked, not created.
    ///
    /// Ceph will happily make it, and the result is a pool whose placement
    /// groups can never peer — which reads as a broken cluster rather than as an
    /// empty one, and sends an operator looking in the wrong place.
    #[test]
    fn a_pool_is_not_created_before_there_is_a_disk_to_put_it_on() {
        let mut spec = spec();
        spec.osds.clear();
        let observed = seen(
            vec![
                node("a", true, &[]),
                node("b", true, &[]),
                node("c", true, &[]),
            ],
            &[],
            true,
        );
        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => assert!(why.contains("before there is an OSD"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// One pass, one step. A plan computed up front is wrong the moment anything
    /// fails, and its recovery is a second path nobody runs.
    #[test]
    fn a_step_that_did_not_happen_is_simply_asked_for_again() {
        let spec = spec();
        let observed = seen(
            vec![
                node("a", true, &[]),
                node("b", false, &[]),
                node("c", false, &[]),
            ],
            &[],
            true,
        );
        // Asked twice, answered the same. Nothing anywhere remembers that the
        // last attempt was made, so a failed one costs a pass and not a
        // half-applied state.
        assert_eq!(next_step(&spec, &observed), next_step(&spec, &observed));
    }

    #[test]
    fn a_paused_cluster_does_nothing_and_is_not_torn_down() {
        let mut spec = spec();
        spec.paused = true;
        let observed = CephObserved::default();
        assert_eq!(next_step(&spec, &observed), CephStep::Paused);
        // The spec is untouched: resuming carries on from where it stopped
        // rather than starting again.
        assert_eq!(spec.monitors.len(), 3);
    }

    #[test]
    fn a_node_that_is_not_reporting_blocks_by_name_rather_than_being_skipped() {
        let spec = spec();
        // `nodes/c` is chosen as a monitor and is not here.
        let observed = seen(vec![node("a", true, &[]), node("b", true, &[])], &[], true);
        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => assert!(why.contains("c"), "{why}"),
            other => panic!("a missing node was skipped rather than reported: {other:?}"),
        }
    }

    /// A node that has stopped reporting is not given work, and is named.
    ///
    /// Every command to the cluster is run by *some* node. Choosing one whose
    /// agent is gone hands the work to a machine that will never do it — the
    /// others compute the same answer, find it is not them, and the deployment
    /// stops with nothing anywhere saying why. The `_admin` label makes another
    /// monitor capable; this is what makes one get chosen.
    #[test]
    fn a_node_that_has_stopped_reporting_is_named_rather_than_waited_on() {
        let spec = spec();
        let mut observed = seen(
            vec![
                node("a", true, &[]),
                node("b", true, &[]),
                node("c", false, &[]),
            ],
            &[],
            true,
        );
        observed.nodes[0].alive = false;
        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => {
                assert!(why.contains('a'), "{why}");
                // Points at the agent, not at Ceph: `a` may have cephadm
                // installed and running perfectly.
                assert!(why.contains("agent"), "{why}");
            }
            other => panic!("a node nobody has heard from was given work: {other:?}"),
        }
    }

    /// Liveness comes from the heartbeat, and a node nothing has ever heard
    /// from is dead rather than new.
    #[test]
    fn a_node_that_has_never_reported_is_not_alive() {
        use crate::{
            meta::{Meta, Placement, ResourceName, Timestamp},
            resources::{NodeSpec, NodeStatus, Resource},
        };

        let make = |id: &str, beat: u64| {
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
                    shared_state: false,
                    vmm: "qemu".into(),
                    fetching: Vec::new(),
                    last_heartbeat: Timestamp(beat),
                    ..NodeStatus::default()
                },
            )
        };
        let now = Timestamp(10_000_000);
        let nodes = vec![
            make("fresh", now.0 - 1_000),
            make("stale", now.0 - NODE_STALE_AFTER_MS - 1),
            make("never", 0),
        ];
        let observed = observe_at(&nodes, now);
        let alive: Vec<(&str, bool)> = observed
            .nodes
            .iter()
            .map(|n| (n.node.as_str(), n.alive))
            .collect();
        assert_eq!(alive, [("fresh", true), ("stale", false), ("never", false)]);
    }

    /// The first monitor being gone is a refusal, not a wait.
    ///
    /// `Bootstrap` is always `monitors[0]` — no other node can stand in — so a
    /// first monitor whose agent has gone is a cluster that will never be
    /// created. Left as "waiting for a to create the cluster" it is the
    /// sentence that sends somebody to check cephadm on a machine where cephadm
    /// is fine and the agent is missing.
    ///
    /// This stage `return`s, so the liveness check in the stage below is never
    /// reached from here — which is exactly how it was missed.
    #[test]
    fn a_first_monitor_that_is_gone_is_refused_rather_than_waited_on() {
        let spec = spec();
        let mut observed = seen(vec![node("a", false, &[])], &[], false);
        observed.nodes[0].alive = false;

        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => {
                assert!(why.contains('a'), "{why}");
                assert!(why.contains("agent"), "{why}");
                // And says what to do about it, since nothing else can.
                assert!(why.contains("different first monitor"), "{why}");
            }
            other => panic!("the cluster waited for a machine that is gone: {other:?}"),
        }
    }

    /// A published key is proof a cluster exists, and outlives every daemon.
    ///
    /// `bootstrapped` is a `systemctl` boolean underneath, and the step that
    /// follows from a false one is the irreversible one. The key is monotonic
    /// where the reading is not: it exists only because a cluster was created,
    /// and no restart or unit failure takes it away.
    ///
    /// The window: `monitors[0]` is a node that does not currently run a
    /// monitor — somebody prepended a new first monitor — and the node that
    /// does run the only one reports false for a pass. Without the key,
    /// `monitors[0]` is told to bootstrap; it holds no cluster of its own, so
    /// the node-local interlock does not fire either, and the cell ends up with
    /// two.
    #[test]
    fn a_published_key_stops_a_second_cluster_being_created() {
        let spec = spec();
        // Nobody reports a monitor this instant.
        let observed = seen(
            vec![
                node("a", false, &[]),
                node("b", false, &[]),
                node("c", false, &[]),
            ],
            &[],
            false,
        );
        assert!(
            matches!(next_step(&spec, &observed), CephStep::Bootstrap { .. }),
            "without the key there is nothing to say a cluster already exists"
        );

        let known = observed
            .clone()
            .with_published_key("ssh-ed25519 AAAA cluster");
        assert!(known.bootstrapped);
        assert!(
            !matches!(next_step(&spec, &known), CephStep::Bootstrap { .. }),
            "a second cluster was created on top of one that had already published its key"
        );

        // An empty or blank key proves nothing, and must not.
        assert!(!observed.clone().with_published_key("").bootstrapped);
        assert!(!observed.with_published_key("   \n").bootstrapped);
    }

    /// A disk the node does not offer is refused, not attempted.
    ///
    /// The node that runs `orch daemon add osd` is not the node holding the
    /// disk, so it has no inventory to check against and would carry the
    /// command out. This is where the refusal has to live, and the reason is
    /// the one an operator needs: the disk is not empty.
    /// The lab waiver reaches the deploy, and only for taste.
    ///
    /// A removable disk asked for with `evenIfUnsuitable` is added; the same
    /// flag on a disk that is unoffered for a real reason (a filesystem, a
    /// mount) still refuses, because `waivable` never contains one.
    #[test]
    fn the_lab_waiver_takes_a_removable_disk_and_nothing_else() {
        let mut spec = spec();
        spec.osds[0].even_if_unsuitable = true;
        let mut observed = seen(
            vec![
                node("a", true, &[]),
                node("b", true, &[]),
                node("c", true, &[]),
            ],
            &[],
            true,
        );
        // The disk stopped being consumable because the platform dislikes it —
        // removable — which is exactly what the flag waives.
        observed.nodes[0].consumable.clear();
        observed.nodes[0]
            .waivable
            .push("/dev/disk/by-id/one".into());
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddOsd {
                node: "a".into(),
                device: "/dev/disk/by-id/one".into(),
            },
            "a waived removable disk was not taken"
        );
        // A disk unoffered for a real reason is not in `waivable`, and the
        // flag does not reach it.
        observed.nodes[0].waivable.clear();
        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => assert!(why.contains("provably empty"), "{why}"),
            other => panic!("the waiver reached a disk with data on it: {other:?}"),
        }
    }

    #[test]
    fn an_osd_on_a_disk_the_node_does_not_offer_is_refused_by_name() {
        let spec = spec();
        let mut observed = seen(
            vec![
                node("a", true, &[]),
                node("b", true, &[]),
                node("c", true, &[]),
            ],
            &[],
            true,
        );
        // `a` no longer offers the disk the spec names — somebody put a
        // filesystem on it between choosing it and getting here.
        observed.nodes[0].consumable.clear();
        match next_step(&spec, &observed) {
            CephStep::Blocked { why } => {
                assert!(why.contains("/dev/disk/by-id/one"), "{why}");
                assert!(why.contains("provably empty"), "{why}");
            }
            other => panic!("a disk that is not free was going to be erased: {other:?}"),
        }
        // And a disk that is *already* an OSD is not re-offered and not
        // re-refused — it is done.
        observed.nodes[0]
            .osd_devices
            .push("/dev/disk/by-id/one".into());
        assert_eq!(
            next_step(&spec, &observed),
            CephStep::AddOsd {
                node: "b".into(),
                device: "/dev/disk/by-id/two".into()
            }
        );
    }

    /// Two monitors is the arrangement worth being loud about.
    #[test]
    fn the_monitor_count_advice_is_loudest_where_it_matters() {
        assert!(quorum_advice(3).is_none());
        assert!(quorum_advice(5).is_none());
        // Worse than one, and it looks redundant — which is the trap.
        let two = quorum_advice(2).unwrap();
        assert!(two.contains("worse than one"), "{two}");
        // One is allowed and honest about what it costs.
        assert!(quorum_advice(1).unwrap().contains("lab"));
        assert!(quorum_advice(0).unwrap().contains("at least one"));
        // An even number above two buys nothing over the odd one below it.
        assert!(quorum_advice(4).unwrap().contains("tolerates the same"));
    }

    #[test]
    fn a_cluster_with_no_network_says_why_rather_than_guessing_one() {
        let mut spec = spec();
        spec.public_network = String::new();
        match next_step(&spec, &CephObserved::default()) {
            CephStep::Blocked { why } => assert!(why.contains("public network"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
