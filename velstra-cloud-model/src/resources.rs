//! The objects the platform is made of.
//!
//! Every one is the same shape — `meta`, `spec`, `status` — and the shape is
//! the point: one reconcile loop, one API surface, one console rendering, and
//! no per-type special cases about what "in progress" means.
//!
//! Read the `status` types with invariant 2 in mind: none of them can express
//! "half way". `Instance.status.state` is what the node *sees* right now, and a
//! node that is mid-boot reports `Stopped` with a `Ready=Unknown` condition —
//! never `BOOTING`, because a `BOOTING` that outlives the controller that wrote
//! it is exactly the object an operator has to fix by hand.

use serde::{Deserialize, Serialize};

use crate::meta::{Condition, Meta, Timestamp};

/// One resource: what was asked for, and what is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resource<S, T> {
    pub meta: Meta,
    pub spec: S,
    pub status: T,
}

impl<S, T> Resource<S, T> {
    pub fn new(meta: Meta, spec: S, status: T) -> Self {
        Self { meta, spec, status }
    }
}

/// The half of ownership a **controller** writes: who this object was given to
/// — a node for an instance, a storage pool for a volume. Read by the access
/// rule so the assignee can make its first report on an object it has been
/// given but not yet claimed.
pub trait Assigned {
    fn assigned_owner(&self) -> Option<&str> {
        None
    }
}

/// Whether the world has caught up with the ask.
pub trait Observed {
    fn observed_generation(&self) -> u64;
    fn conditions(&self) -> &[Condition];
    /// Whoever owns this object's `status`, if it has been claimed.
    ///
    /// Usually a node; for a volume it is the storage pool holding its bytes.
    /// The rule does not care which — it cares that exactly one party writes.
    fn owner(&self) -> Option<&str>;

    /// True when the object *is* the thing that reports on it.
    ///
    /// Two resources are like this, and for the same reason: nothing assigns a
    /// hypervisor to a hypervisor or a storage pool to a storage pool, so each
    /// one's owner is its own name. Without this the access rule — which asks
    /// the status who owns it — refuses every node's own capacity report, and a
    /// cell can never learn what it is made of. Found by running the layers
    /// together; each of them was individually right.
    fn self_owned(&self) -> bool {
        false
    }
}

impl<S, T: Observed> Resource<S, T> {
    /// True when the agent has seen and acted on the current spec.
    pub fn converged(&self) -> bool {
        self.status.observed_generation() == self.meta.generation
    }

    /// How far behind the world is. This is the number the drift metric and the
    /// console's "still converging" both read, and there is exactly one of it.
    pub fn drift(&self) -> u64 {
        self.meta
            .generation
            .saturating_sub(self.status.observed_generation())
    }
}

/// "Keep a recent snapshot of this volume." See
/// [`crate::storage::SnapshotScheduleSpec`].
pub type SnapshotSchedule =
    Resource<crate::storage::SnapshotScheduleSpec, crate::storage::SnapshotScheduleStatus>;

impl Observed for crate::storage::SnapshotScheduleStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        // Nothing owns an intention. What it caused is a set of snapshots,
        // each owned by the pool that holds it.
        None
    }
}

impl Assigned for crate::storage::SnapshotScheduleSpec {}

// ---- captures ------------------------------------------------------------

/// "Make an image out of this guest." See [`crate::capture`].
pub type Capture = Resource<crate::capture::CaptureSpec, crate::capture::CaptureStatus>;

impl Observed for crate::capture::CaptureStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

impl Assigned for crate::capture::CaptureSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // Only the machine with the disk can copy it.
        (!self.node.is_empty()).then_some(self.node.as_str())
    }
}

impl Assigned for crate::maintenance::MaintenanceWindowSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // Nobody, and deliberately not the node it is about. Assigning it to
        // the node would let that node's agent write the window that governs
        // it — a machine deciding when it is allowed to be out of service.
        // Whether it is open is arithmetic, and there is nothing here to claim.
        None
    }
}

// ---- audit ---------------------------------------------------------------

/// One thing that was refused, or one session that began or ended.
///
/// Cell-scoped, and read by cell operators only — a tenant who could read this
/// would learn the names of projects and people that are not theirs, which is
/// the opposite of what an audit trail is for.
pub type AuditRecord = Resource<crate::audit::AuditSpec, crate::audit::AuditStatus>;

impl Observed for crate::audit::AuditStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        // Nothing owns a record of something that already happened. It is
        // written once and never reported on.
        None
    }
}

impl Assigned for crate::audit::AuditSpec {}

// ---- backups -------------------------------------------------------------

/// A place backups are kept, and the agent that reports on it.
pub type BackupTarget = Resource<crate::backup::BackupTargetSpec, crate::backup::BackupTargetStatus>;

/// One copy of one volume, at one moment, on one target.
pub type Backup = Resource<crate::backup::BackupSpec, crate::backup::BackupStatus>;

/// "Keep a copy of this volume on that target, no older than this."
pub type BackupSchedule =
    Resource<crate::backup::BackupScheduleSpec, crate::backup::BackupScheduleStatus>;

impl Observed for crate::backup::BackupTargetStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

impl Observed for crate::backup::BackupStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

impl Observed for crate::backup::BackupScheduleStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        // Nothing owns a schedule: it is an intention, not a thing an agent
        // holds. What it *caused* is a set of backups, each owned by whoever
        // made it.
        None
    }
}

impl Assigned for crate::backup::BackupTargetSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // Named by an operator, never claimed. A target assigned to nobody is
        // one any agent could grab, and "an agent may only report on what it
        // was given" is the rule that makes a node token a boundary rather
        // than a formality.
        (!self.agent.is_empty()).then_some(self.agent.as_str())
    }
}

impl Assigned for crate::backup::BackupSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // The pool holding the source reads the bytes, so it is the party with
        // something to report — exactly as a snapshot is assigned to its pool.
        (!self.pool.is_empty()).then_some(self.pool.as_str())
    }
}

impl Assigned for crate::backup::BackupScheduleSpec {}

// ---- device class --------------------------------------------------------

/// A named set of interchangeable PCI devices, across the whole cell.
///
/// A cell-scoped resource rather than a project one: the hardware belongs to
/// the cell, and a class defined per project would be a different name for the
/// same silicon in every tenancy. What a project controls is how many it may
/// hold ([`Quota::devices`]), not what they are called.
pub type DeviceClass = Resource<crate::pci::DeviceClassSpec, DeviceClassStatus>;

/// A stretch of time in which one node is out of service.
///
/// Cell-scoped, like the node it is about. Nothing writes its status: whether
/// it is upcoming, open or over is arithmetic on the clock, and a stored copy
/// of that would be right only while somebody was awake to write it.
pub type MaintenanceWindow = Resource<
    crate::maintenance::MaintenanceWindowSpec,
    crate::maintenance::MaintenanceWindowStatus,
>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceClassStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
}

impl Observed for crate::maintenance::MaintenanceWindowStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        // Nobody. A window is a declaration about a stretch of time; whether it
        // is open is arithmetic on the clock, so there is nothing here for an
        // agent to report and nothing for it to claim.
        None
    }
}

impl Observed for DeviceClassStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        // Nothing owns a class: it is a definition, not a thing a node holds.
        // How many of its members exist and where is computed from the node
        // reports, never stored here — an aggregate is not a fact anybody owns.
        None
    }
}

impl Assigned for crate::pci::DeviceClassSpec {}

// ---- project -------------------------------------------------------------

/// The IAM and quota anchor. Everything chargeable hangs under one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSpec {
    pub display_name: String,
    /// `organizations/o1` or `folders/f2` — the parent policies are inherited
    /// from, kept as a name so the hierarchy is walked, not guessed.
    pub parent: String,
    pub quota: Quota,
    /// Who may do what inside this project.
    ///
    /// Here rather than in a collection of its own because a project **is** the
    /// unit of tenancy: everything the bindings govern is under this name, and a
    /// policy object one indirection away is one more thing that can be deleted
    /// while what it protects stays.
    ///
    /// Empty means nobody but a cell operator, which is the safe direction and
    /// the state a freshly created project is in: whoever created it grants
    /// themselves, deliberately, rather than being granted by a default nobody
    /// chose. See [`crate::authz`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<crate::authz::Binding>,

    /// Which cell this project's resources live in.
    ///
    /// **Not the same thing as `meta.placement.cell`, and the difference is the
    /// whole of cell routing.** `placement` says which store holds *this copy of
    /// this object* — it is checked on every decode, so an object read out of
    /// cell-1's key space that claims cell-2 is refused. Projects are global:
    /// every cell holds a copy of every project, and each copy is correctly
    /// stamped with the cell whose store it sits in. So `placement` cannot say
    /// where a project's *instances* go — it says where the project record is.
    ///
    /// This field says that, and it is what a router resolves: a request naming
    /// `projects/p1/instances/i1` is routed by looking up `p1` and reading this,
    /// which makes routing a lookup on the first two segments of a name and
    /// never deeper. See [`crate::routing`].
    ///
    /// Empty means "wherever it is being read", which is what every project
    /// written before routing existed means and what a single-cell installation
    /// goes on meaning. Resolved rather than defaulted at rest, so a one-cell
    /// deployment never has to name its cell for anything to work.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cell: String,
}

/// How much of a guest's console output travels on its status.
///
/// Eight kibibytes: enough for a kernel panic with its trace, a bootloader
/// giving up, or cloud-init's last complaint — the three things anybody reads
/// this for — and small enough that a cell full of watched guests does not
/// turn its store into a log shipper.
pub const CONSOLE_TAIL_BYTES: usize = 8 * 1024;

/// Limits, counted rather than reserved. A reservation that is not released on
/// a crash is a quota that shrinks over time; a count is recomputed from what
/// exists and cannot drift.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quota {
    pub instances: u32,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// A count of volume objects, distinct from `volume_gib`: a project can be
    /// held to a number of volumes as well as a number of gibibytes, and the two
    /// answer different worries — one is per-volume overhead, the other is
    /// capacity. Both are counted from what exists, never tracked as a running
    /// total.
    pub volumes: u32,
    pub volume_gib: u64,
    /// A count of floating IP objects a project may hold. An address that
    /// outlives the machine answering on it is a scarce, externally-routable
    /// resource, so it is one a cell operator wants to be able to cap.
    pub floating_ips: u32,
    /// A count of load balancer objects a project may hold. Each one takes an
    /// address out of a subnet and a set of datapath map entries on every
    /// ingress host, so it is capped the way a floating IP is. `default` so a
    /// quota stored before the field existed still reads back.
    #[serde(default)]
    pub load_balancers: u32,
    /// A count of passed-through PCI devices a project may hold.
    ///
    /// Capped for the plainest reason of all: each one is a piece of hardware
    /// that exists once and cannot be oversubscribed, so without a cap one
    /// project can take every accelerator in the cell. Counted from what
    /// exists, like every other dimension here.
    #[serde(default)]
    pub devices: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// What is actually in use, from counting the objects — never decremented
    /// by hand.
    pub used: Quota,
}

impl Observed for ProjectStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type Project = Resource<ProjectSpec, ProjectStatus>;

// ---- node ----------------------------------------------------------------

/// A hypervisor. Its `spec` is what an operator decides about it (may it take
/// work, is it being drained); its `status` is what the agent reports.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// False drains the node: nothing new is placed, what runs keeps running.
    /// Draining is a spec change, not a command, so a controller restart cannot
    /// lose it half way.
    pub schedulable: bool,
    /// Move what is already running here, somewhere else.
    ///
    /// Separate from `schedulable`, and the pair is deliberate: draining says
    /// "nothing new", evacuating says "and none of the old either". They are
    /// different intentions and an operator taking a machine out for an hour
    /// wants the first without the second.
    ///
    /// A desired state, not a command — "there should be no guests here". A
    /// controller creates one migration per guest that can move, and a guest
    /// that cannot stays where it is with the reason answerable at
    /// `:explainMigration`. Turning it off stops further moves; it does not
    /// bring anything back.
    #[serde(default)]
    pub evacuate: bool,
    #[serde(default)]
    pub labels: Vec<String>,
    /// How long this node may fail to report before it stops its own guests.
    ///
    /// Zero — the default — means it does not, and a node that does not stop
    /// its own guests is **never recovered from**: "unreachable" and "stopped"
    /// are different statements, and acting on the first as though it were the
    /// second is how two guests come to write to one volume.
    ///
    /// Set it, and the node runs its own deadline against its own clock,
    /// needing nothing from anybody — which is the situation this is for. The
    /// control plane then waits longer still. See [`crate::ha`].
    #[serde(default)]
    pub fence_after_s: u32,
    /// How many vCPUs this node may hand out per real core.
    ///
    /// `0` — the default — means one for one: every vCPU a guest was promised
    /// has a core of its own. That is the safe reading of a zero and it is the
    /// same convention a quota uses, so a node stored before this field existed
    /// behaves exactly as it did.
    ///
    /// ## Why only the processor
    ///
    /// A processor can be *shared*: two guests that both want a core get one
    /// each in turn, and the cost of being wrong is that they run slowly. That
    /// is a trade an operator makes on purpose, and it is how nearly every
    /// hypervisor fleet in the world is run — a machine idles most of the day.
    ///
    /// Memory cannot be shared that way. A guest promised 8 GiB and handed 4
    /// does not run slowly; it is killed, or it kills its neighbour, and the
    /// operator finds out from a guest that has vanished. So there is
    /// deliberately **no memory ratio here**, and there should not be one until
    /// this platform can hand a guest back the page it lent out — which means
    /// ballooning, and which is a feature, not a number.
    ///
    /// ## Where it applies
    ///
    /// Placement only, and nowhere else. Nothing about the guest changes: it is
    /// still given the vCPUs it asked for, and the hypervisor still schedules
    /// them. What changes is how many the cell believes a node has room for.
    #[serde(default)]
    pub vcpu_overcommit: u32,
    /// This machine carries traffic between the cell and the world.
    ///
    /// What it means concretely: a public address whose network says
    /// `FromGateway` is announced from here, and packets for it reach the guest
    /// over the overlay. Several machines may carry it — the upstream sees them
    /// as equal next hops — and a cell with none simply cannot use that mode,
    /// which is refused by name rather than silently doing nothing.
    ///
    /// Not the same as being a hypervisor. A gateway may hold guests and
    /// usually does; a cell may also keep two machines that do nothing else.
    #[serde(default)]
    pub gateway: bool,
    /// The CPU this node presents to guests, if it has been told to present
    /// something other than its own.
    ///
    /// Declared here, on the node, rather than as a resource with a label
    /// selector. The selector version reads better on paper and behaves worse:
    /// a machine added later would silently join an aggregate because of a
    /// label somebody set for another reason, and quietly deciding what
    /// processor a new machine offers is not a decision to make on an
    /// operator's behalf. Declaring it per node keeps one writer per object —
    /// the operator — and needs no controller.
    ///
    /// The cost is that a new node does not join an aggregate by itself. That
    /// is paid back by [`crate::cpu::advise`], which says so out loud rather
    /// than leaving it to be noticed.
    ///
    /// **A change here governs guests started after it.** Guests already
    /// running keep the CPU they booted with until they are restarted — see
    /// the invariant on [`crate::cpu`] — so lowering a baseline over a running
    /// fleet does not move anybody; it means the fleet adopts it as guests
    /// come and go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_baseline: Option<crate::cpu::CpuLevel>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    pub capacity: Capacity,
    /// What the agent sees in use on itself. The scheduler reads this rather
    /// than a placement table it maintains, because a table drifts and a report
    /// cannot.
    pub allocated: Capacity,
    pub agent_version: String,
    pub last_heartbeat: Timestamp,
    /// Block devices this node can see, and what each is being used for.
    ///
    /// Here for the same reason `images` is: it is one node's observation of its
    /// own hardware, which nobody else can make. The console reads it to offer
    /// disks for a Ceph OSD, and
    /// [`ceph::may_consume`](crate::ceph::may_consume) decides which of them may
    /// be offered — never this list on its own, because a list is a list and the
    /// rule is the rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<crate::ceph::BlockDevice>,

    /// Whether this node has what it takes to run a Ceph daemon, and what it is
    /// already running. Reported rather than assumed: a cell may have Ceph on
    /// three nodes out of twenty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceph: Option<crate::ceph::NodeCeph>,

    /// What processor this node has, and whether its VMM can present another.
    ///
    /// One node's observation of its own hardware, like `devices` above, and
    /// read the same way: never on its own, always through
    /// [`cpu::may_run_on`](crate::cpu::may_run_on) or the domain functions
    /// beside it. Empty on a node whose agent is too old to report, which
    /// every caller treats as "cannot be shown compatible" rather than as
    /// "compatible".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<crate::cpu::NodeCpu>,

    /// PCI devices this machine has, and what each is being used for.
    ///
    /// One node's observation of its own hardware, like `devices` and `cpu`
    /// above. Which of these may be *offered* is
    /// [`pci::offerable`](crate::pci::offerable) — never this list on its own,
    /// because the list says what is there and the rule says what is safe: a
    /// device whose IOMMU group holds something busy is present and not
    /// available, and the difference is a guest that steals the host's audio
    /// controller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<crate::pci::PciDevice>,

    /// Images this node holds a verified copy of, by resource name.
    ///
    /// It lives here, and not as `cached_on` on the image, for the reason that
    /// keeps coming up: a list of nodes holding an image is an *aggregate*, and
    /// an aggregate is not a fact anybody owns — every node would have to write
    /// one field, which is exactly the shared-mutable-list that invariant 1
    /// exists to forbid. Here it is what it really is: one node's report about
    /// itself. Whoever needs the aggregate computes it from these.
    pub images: Vec<String>,
}

/// What a running guest was actually given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSize {
    pub vcpus: u32,
    pub memory_mib: u64,
    pub root_disk_gib: u64,
}

/// One thing an operator asked for that the running guest does not have.
///
/// Serialisable because it is answered on a read: the API computes these on the
/// way out rather than storing them, so the shape has to cross the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChange {
    /// The spec field, as the API spells it — so a console can point at the
    /// control that caused it rather than at a paragraph.
    pub field: &'static str,
    pub from: String,
    pub to: String,
}

impl std::fmt::Display for PendingChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} → {}", self.field, self.from, self.to)
    }
}

/// What has been asked for that this guest will only get when it next starts.
///
/// Empty for a guest that is not running: there is nothing to differ from, and
/// the next start gives it whatever the spec says by construction.
///
/// This is not a failure and it is not drift. It is the ordinary result of
/// resizing a machine that is up, and the whole point is that it is **said**:
/// the alternative — which this platform shipped — is a spec that reads as
/// applied while the guest runs on the old numbers.
pub fn pending_changes(instance: &Instance) -> Vec<PendingChange> {
    let Some(running) = instance.status.running_size else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if running.vcpus != instance.spec.vcpus {
        out.push(PendingChange {
            field: "vcpus",
            from: running.vcpus.to_string(),
            to: instance.spec.vcpus.to_string(),
        });
    }
    if running.memory_mib != instance.spec.memory_mib {
        out.push(PendingChange {
            field: "memoryMib",
            from: running.memory_mib.to_string(),
            to: instance.spec.memory_mib.to_string(),
        });
    }
    if running.root_disk_gib != instance.spec.root_disk_gib {
        out.push(PendingChange {
            field: "rootDiskGib",
            from: running.root_disk_gib.to_string(),
            to: instance.spec.root_disk_gib.to_string(),
        });
    }
    out
}

/// Which nodes hold an image, computed from what each node reports about
/// itself. Never stored — see [`NodeStatus::images`].
pub fn nodes_holding(image: &str, nodes: &[Node]) -> Vec<String> {
    nodes
        .iter()
        .filter(|n| n.status.images.iter().any(|i| i == image))
        .map(|n| n.meta.name.id().to_string())
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub vcpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    /// Per-NUMA-node free memory, so placement can refuse a host that has the
    /// total but not on one node.
    ///
    /// `default` because an agent old enough not to report it exists, and an
    /// empty list already means "this machine said nothing about NUMA" —
    /// which placement reads as "do not make the per-node check". Without the
    /// attribute, a capacity written before this field could not be read at
    /// all, and a cell upgrading would stop being able to read its own nodes.
    #[serde(default)]
    pub numa_free_mib: Vec<u64>,
    /// On the wire this is `hugepages1gi`, all lowercase, and it has to stay
    /// that way.
    ///
    /// `hugepages_1_gi` would read better — `hugepages1Gi` — and cannot
    /// round-trip: coming back, a digit followed by an uppercase letter is how
    /// `l3Vni` is told from `hugepages1gi`, so one convention cannot serve
    /// both. A field that does not survive its own wire is a field a client
    /// cannot write, which is worse than an ugly name.
    #[serde(default)]
    pub hugepages_1gi: u32,
}

impl Observed for NodeStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
    fn self_owned(&self) -> bool {
        true
    }
}

pub type Node = Resource<NodeSpec, NodeStatus>;

// ---- image ---------------------------------------------------------------

/// Content-addressed and immutable: the id *is* the digest.
///
/// There is no way to replace the bytes behind an image that instances were
/// created from, so "the image was deleted and now the VM will not start" is
/// not a state this system can reach.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageSpec {
    /// `sha256:…` — carried in the resource id too, which is what makes
    /// fetching one verifiable.
    pub digest: String,
    /// The guest this was captured from, when it came from one rather than
    /// from a URL somebody typed.
    ///
    /// Beside `source_url` rather than replacing it, and for the same reason a
    /// volume carries three of these: it describes how the object came into
    /// existence. An image made from a guest that has since been deleted still
    /// says where it came from, which is what somebody looking at a list of
    /// near-identical templates actually needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_instance: Option<String>,
    pub format: ImageFormat,
    pub size_bytes: u64,
    pub source_url: String,
    /// **Nothing verifies this, and the API refuses to store it.**
    ///
    /// It was declared as "a cosign-style signature, verified before a node will
    /// boot it". No code has ever read it: not the node that pulls the image,
    /// not the one that boots it, not the API. What did read it was the
    /// console, which offered a box to type one into and a column headed
    /// *Signed* showing yes or no — so an operator could paste anything at all
    /// and the platform would report, at a glance, that the image was signed.
    ///
    /// A field that is merely unused is dead weight. One that is unused while
    /// something reports a security property from it is worse than not having
    /// it, because every place it is displayed becomes evidence somebody will
    /// cite. So it is refused on the way in
    /// ([`crate::resources::UNVERIFIED_SIGNATURE`]) rather than stored and
    /// ignored: the platform will not hold a claim it cannot check.
    ///
    /// The field stays on the type and on the wire so that implementing
    /// verification is a change in one direction rather than a schema
    /// migration. When it lands, the refusal goes with it, in the same commit.
    pub signature: Option<String>,
}

/// What the API says when an image arrives carrying a signature.
///
/// Here rather than in the API crate because it is a statement about the model:
/// the reason is a property of the field, and a caller reading this type should
/// find out why without going looking.
pub const UNVERIFIED_SIGNATURE: &str = "spec.signature is not stored, because nothing in this platform verifies it. It was \
     declared as a cosign-style signature checked before boot, and no code has ever read it — \
     while the console showed a `Signed` column derived from it. An unchecked claim that is \
     displayed as a checked one is worse than no field at all, so it is refused rather than \
     kept. Publish the image without it; when verification exists this will accept it again.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    #[default]
    Raw,
    Qcow2,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
}

impl Observed for ImageStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type Image = Resource<ImageSpec, ImageStatus>;

// ---- instance ------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InstanceSpec {
    pub vcpus: u32,
    pub memory_mib: u64,
    /// `projects/p1/images/sha256-…`
    pub image: String,
    pub root_disk_gib: u64,
    /// What an operator wants it to be doing. Not a command — asking twice is
    /// the same as asking once.
    pub desired_state: DesiredState,
    /// Ports on Velstra networks, in order.
    pub ports: Vec<String>,
    pub ssh_keys: Vec<String>,
    pub user_data: Option<String>,
    /// Set by the scheduler, once. Moving it is a migration, which is a
    /// deliberate act with its own resource — never a silent re-place.
    pub node: Option<String>,
    pub placement_policy: PlacementPolicy,
    /// When this guest starts, relative to others on the same node.
    ///
    /// Lower goes first; the same number is a group that starts together.
    /// Zero — the default — is the first group, which is where everything sits
    /// until somebody has a reason to say otherwise.
    ///
    /// The case this exists for is the one a platform is judged on: power comes
    /// back, a node starts forty guests at once, and the database everything
    /// else needs loses the race for disk to a dozen web servers.
    #[serde(default)]
    pub start_order: u32,
    /// How long to let the group ahead settle before this one starts.
    ///
    /// Measured from the newest start in that group, not from each member in
    /// turn — otherwise a fleet's boot is the sum of every delay rather than
    /// the longest one, and a hundred guests at thirty seconds each is fifty
    /// minutes of nothing happening.
    ///
    /// Zero means "as soon as they are up", which is what most things want:
    /// the wait is for a database to finish recovering, not a ritual.
    #[serde(default)]
    pub start_delay_s: u32,
    /// What to do if this guest's node stops answering.
    ///
    /// `Leave` by default, and not out of caution alone: a guest whose disk is
    /// local to that machine has nothing to come back *to* elsewhere, and an
    /// empty machine wearing a familiar name is worse than one that is down and
    /// says so. See [`crate::ha`] for what makes the other answer safe.
    #[serde(default)]
    pub on_node_loss: crate::ha::OnNodeLoss,
    /// Publish this guest's console output on its status.
    ///
    /// A desired state, not a command: "I am watching this guest's console".
    /// Turning it on is idempotent, turning it off stops the publishing, and a
    /// controller restart loses nothing — which is why it is a spec field and
    /// not a request.
    ///
    /// Off by default, and that default is load-bearing. An agent writes a
    /// status only when it has changed, which is what keeps a converged cell
    /// quiet; a console tail that moved every time a guest logged a line would
    /// turn every chatty guest into a write per pass. So the cost is paid only
    /// for the guests somebody is actually looking at.
    ///
    /// A guest that is **not** ready publishes its tail regardless — see
    /// [`InstanceStatus::console_tail`]. Requiring an operator to switch this
    /// on and then wait for the failure to happen again would be the wrong
    /// answer to the only question a dead guest is ever asked.
    #[serde(default)]
    pub console: bool,
    /// PCI device classes this guest wants passed through, by resource id.
    ///
    /// A class and never an address: `0000:41:00.0` is node-specific, so an
    /// instance naming one could only ever be scheduled on the one machine
    /// that has it. Two entries of the same class mean two different devices.
    ///
    /// A guest holding one of these cannot be live-migrated — the device's
    /// state lives in hardware nobody can copy — and the platform refuses the
    /// migration by name rather than discovering it mid-transfer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlacementPolicy {
    /// Instances that must not share a node — an availability group.
    pub anti_affinity_group: Option<String>,
    /// Only nodes carrying all of these labels.
    pub required_labels: Vec<String>,
    /// Only nodes whose processor is at least this psABI level.
    ///
    /// A level, deliberately, and never a model name. A level is what a
    /// distribution states as its requirement, and it stays meaningful across
    /// vendors and generations; naming a model would make one instance
    /// placeable on one kind of machine and turn a fleet-wide property into
    /// thousands of per-guest ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cpu_level: Option<crate::cpu::CpuLevel>,
    /// Instances that should share a node — the opposite ask.
    ///
    /// The case it exists for is a pair that talks constantly: an application
    /// and the cache it reads on every request, where a hop between machines is
    /// the whole latency budget. Anti-affinity keeps a service alive when a
    /// machine dies; affinity keeps it fast while they all live, and a platform
    /// with only the first can express only half of what people actually run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_group: Option<String>,
    /// Whether keeping the anti-affinity group apart is a rule or a wish.
    ///
    /// `Required` — the default, and what this platform did before the field
    /// existed — refuses a node that already runs a member. `Preferred` places
    /// elsewhere if anywhere else will take it, and puts it beside its sibling
    /// rather than not running at all.
    ///
    /// Both are right answers to different questions. Three replicas of a
    /// database must not share a machine even if that means one stays down;
    /// twelve web servers would rather be crowded than short.
    #[serde(default)]
    pub spread: Strength,
    /// Whether keeping the affinity group together is a rule or a wish.
    #[serde(default)]
    pub affinity: Strength,
}

/// A rule, or a wish.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strength {
    /// Refuse a node that does not satisfy it.
    ///
    /// The default, because it is what this platform did before the choice
    /// existed — and because the safe direction for a rule nobody has thought
    /// about is the one that says no rather than the one that quietly does
    /// something else.
    #[default]
    Required,
    /// Prefer a node that satisfies it, and take one that does not over not
    /// running at all.
    Preferred,
}

/// What the node sees. Every value here is observable on the host right now;
/// none of them describe a transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceState {
    /// The node has not reported yet — the honest answer before first contact,
    /// and the only one that is not an observation.
    #[default]
    Unknown,
    Stopped,
    Running,
    /// The VMM exited in a way the node could not repair.
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    pub state: InstanceState,
    /// The node reporting this status. The only agent allowed to write it.
    pub node: Option<String>,
    /// `default` for the same reason as everything else here: an agent old
    /// enough not to have sent this exists, and an empty list already means
    /// "it has no address yet", which every reader handles.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Host-side identity of the running machine, for the console and for
    /// re-derivation after an agent restart.
    pub vmm_pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    /// The size the running guest actually has.
    ///
    /// Recorded when it starts, by the agent that asked the VMM for it, and
    /// cleared when it stops — the same life as [`Self::cpu`] and
    /// [`Self::devices`], for the same reason: what a running machine *is* is
    /// a fact somebody observed, not something to re-derive from the ask.
    ///
    /// Without this, changing `spec.vcpus` on a running guest was accepted,
    /// did nothing, and the object read as converged — the platform reporting
    /// that a guest matched a spec it did not match. That is the one failure
    /// every invariant here exists to prevent, and it was reachable from a
    /// text box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_size: Option<RunningSize>,

    /// The last of what this guest wrote to its serial console.
    ///
    /// Published when [`InstanceSpec::console`] is on, and always while the
    /// guest is not ready — a guest that cannot boot is the one that most
    /// needs to be heard and the one that says the least, which is why the
    /// node captures this at all.
    ///
    /// A **tail**, capped at [`CONSOLE_TAIL_BYTES`]. Never the whole log: a
    /// guest that has been up for a month has megabytes of it, and a status is
    /// read by everything in the cell.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_tail: String,
    /// How much the guest has written in total, so a reader can tell a tail
    /// from the whole thing. Without it, eight kibibytes of output looks like
    /// everything the guest ever said.
    #[serde(default)]
    pub console_bytes: u64,

    /// PCI addresses this guest holds on its node, recorded when it started.
    ///
    /// Written by the agent that assigned them, for the same reason the CPU is:
    /// nobody else can know, and re-deriving it every pass would hand a guest a
    /// different device the moment another one freed up — restarting it for no
    /// reason a person could see.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,

    /// The CPU this guest was actually given, recorded when it started.
    ///
    /// **Not derived from the node it sits on**, now or ever. A baseline
    /// declared over a fleet does not change what a guest that is already
    /// running can see, and computing compatibility from the node would say
    /// otherwise — then move the guest somewhere missing instructions it has
    /// been executing for hours. Written once by the agent that launched the
    /// VMM, which is the only party that knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<crate::cpu::GuestCpu>,
}

impl Observed for InstanceStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

pub type Instance = Resource<InstanceSpec, InstanceStatus>;

// ---- volume and attachment ----------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub size_gib: u64,
    pub pool: String,
    /// LUKS with a key from the project's KMS entry. Absent means plaintext,
    /// which is a decision an operator has to make rather than a default.
    pub encryption_key: Option<String>,
    pub source_image: Option<String>,
    /// The snapshot this volume was cloned from, if it came from one.
    ///
    /// Alongside `source_image` rather than replacing it, because they are the
    /// same statement about two different origins — and like it, this describes
    /// how the volume came into existence rather than something that keeps
    /// happening. A volume is never restored *in place* from a snapshot: that
    /// would be a command, and asking a command twice undoes whatever happened
    /// in between. Restoring is making a new volume from a snapshot, which is
    /// this field.
    pub source_snapshot: Option<String>,
    /// The backup this volume was restored from, if it came from one.
    ///
    /// The third of the same statement — `source_image`, `source_snapshot`,
    /// and this — and it is a *field* for the reason written on the one above:
    /// restoring in place would be a command sitting in a spec, performed
    /// again on every resync, undoing whatever the guest wrote in between.
    /// Restoring is making a new volume from a copy, which is this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_backup: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    pub provisioned: bool,
    pub actual_size_gib: u64,
    /// The pool that has claimed this volume and is reporting on it.
    ///
    /// Until this exists, nothing had written a volume's status at all — not
    /// because the code was missing but because `owner()` returned `None` and
    /// `VolumeSpec` was not `Assigned`, so the access rule refused every writer
    /// there could ever be. A volume lives in a pool, not on a node, so the pool
    /// is the party with something to report about it.
    pub pool: Option<String>,
}

impl Observed for VolumeStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.pool.as_deref()
    }
}

impl Assigned for VolumeSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // `spec.pool` is the assignment, exactly as `instance.spec.node` is: an
        // operator (or later a scheduler) says where the bytes belong, and the
        // pool agent watching for its own name claims it.
        Some(self.pool.as_str())
    }
}

/// A pool releases a volume's bytes before the object may go.
///
/// Separate from the node finalizer because they answer different questions: a
/// node is asked to stop *using* a volume, a pool is asked to destroy it. A
/// volume that vanished from the API while its pool still held gigabytes would
/// be storage nobody is billed for and nobody can find.
pub const POOL_RELEASE_FINALIZER: &str = "pool.velstra.io/release";

// ---- snapshot ------------------------------------------------------------

/// A tenant's router: the networks whose subnets reach each other.
///
/// Without one, a project's networks are separate L2 segments and a guest on one
/// cannot reach a guest on another even inside the same project — which is right
/// as a default (two segments a tenant made separate stay separate) and useless
/// as the only option.
///
/// **A router is a membership, not a box.** There is no appliance to place, no
/// interface to attach and nothing to fail over: the fabric implements it as an
/// IP-VRF with an *anycast* gateway — the same gateway MAC and address on every
/// host serving the tenant — so a guest keeps its default-gateway ARP entry when
/// it migrates, and routing happens on whichever machine the packet is already
/// on. Modelling it as a resource with a list of networks says exactly that and
/// nothing that is not true.
///
/// A network belongs to at most one router. Two routers claiming one network
/// would be two answers to "where does this subnet's traffic go", and the fabric
/// refuses it for the same reason.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouterSpec {
    /// The networks that route to each other, by resource name.
    ///
    /// Order does not matter and duplicates are not an error — this is a set
    /// written as a list, because a list is what a person types and what every
    /// other collection of names here is.
    #[serde(default)]
    pub networks: Vec<String>,
}

// The `alias`es carry objects written under the old `rename_all = "camelCase"`
// representation. Dropping the rename was needed to fix the API write path, but
// it changed how these fields spell on the wire and in the store; without the
// aliases an already-stored router would fail to deserialise and read back as
// `TypedError::Corrupt`. New writes use the snake_case field names.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouterStatus {
    #[serde(alias = "observedGeneration")]
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The routed VNI the platform gave this router, in its own number space.
    ///
    /// Assigned rather than asked for, like a network's VNI: a tenant choosing
    /// one would be choosing whether it collides.
    #[serde(alias = "l3Vni")]
    pub l3_vni: u32,
    /// The anycast gateway's hardware address, identical on every host serving
    /// this tenant. Recorded because a person debugging an ARP table needs to
    /// recognise it, and derived from the VNI so it is stable across a restart.
    #[serde(alias = "gatewayMac")]
    pub gateway_mac: String,
}

impl Observed for RouterStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    /// Nobody. A router is a cell-wide fact like the networks it joins — no
    /// machine holds it, which is what lets a controller write its status.
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl Assigned for RouterSpec {}

pub type Router = Resource<RouterSpec, RouterStatus>;

/// A **floating IP**: an address that outlives the machine answering on it.
///
/// The point of one is that it is not a property of a port. A guest is
/// rebuilt, replaced, or moved to a different instance entirely, and the
/// address the outside world knows follows the operator's declaration rather
/// than the machine — which is why `port` is a field a person edits and why
/// detaching is an ordinary state rather than deletion.
///
/// The address itself comes from a subnet, allocated by the same counting the
/// ports use, so a floating address and a port address are never the same
/// address. That is the whole reason [`crate::ipam`] counts both: two
/// allocators over one range is the defect this design exists to not have.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FloatingIpSpec {
    /// The subnet the address comes from, by resource name.
    pub subnet: String,
    /// The address, once something has decided it.
    ///
    /// `None` means "any", and a controller fills it in — the same arrangement
    /// as a port's address, and written into `spec` for the same reason: it is
    /// the thing a person may pin, so it must be a field they can write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The port this address currently forwards to, by resource name. Empty
    /// means allocated and pointing at nothing, which is a floating IP an
    /// operator is holding on to — the reason to have one at all.
    #[serde(default)]
    pub port: String,
    /// Whether the guest holds this address itself, or something translates it
    /// at the edge.
    ///
    /// `Routed` is the one that puts the address *in* the machine: it is bound
    /// to the port as a second address, the guest configures it, and nothing
    /// anywhere rewrites a packet. See [`crate::public`].
    #[serde(default)]
    pub delivery: crate::public::Delivery,
    /// Who announces it, when the network's own answer is not the one this
    /// address wants. `None` — the default — is "whatever the network says".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announce: Option<crate::public::Announce>,
}

// Same as `RouterStatus`: `alias`es keep objects stored under the old
// camelCase representation deserialisable after the `rename_all` was dropped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FloatingIpStatus {
    #[serde(alias = "observedGeneration")]
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The id the fabric gave this address. Recorded because every later call
    /// about it — associate, disassociate, release — is keyed on that id and
    /// not on the address, so losing it would strand the allocation in the
    /// fabric with nothing able to name it.
    #[serde(alias = "fabricId")]
    pub fabric_id: String,
    /// The port's fixed address this is forwarding to right now, as the fabric
    /// has it. Empty means not associated. It is the *observed* half of
    /// `spec.port`: the two differing is what a reconcile is for.
    pub associated: String,
}

impl Observed for FloatingIpStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    /// Nobody. A floating IP is a fabric-wide fact, not a machine's — the port
    /// it points at may not even be placed yet.
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl Assigned for FloatingIpSpec {}

/// Held while the fabric still holds this address.
///
/// The id needed to release an allocation lives on the object being deleted, so
/// without a guard the record goes and the allocation stays — an address the
/// fabric holds that nothing in the control plane can name, and a subnet that
/// fills up for no visible reason.
pub const FABRIC_RELEASE_FINALIZER: &str = "fabric.velstra.io/release";

pub type FloatingIp = Resource<FloatingIpSpec, FloatingIpStatus>;

/// A point-in-time copy of a volume, in that volume's own pool.
///
/// **The source is in the name, not in a field.**
/// `projects/p1/volumes/data-1/snapshots/nightly` is a copy of `data-1` and can
/// never be a copy of anything else. That is not tidiness. Which volume a copy
/// came from is the one thing about it that must never change — a snapshot
/// repointed at another volume is a restore that quietly hands back somebody
/// else's data — and a fact that lives in the identity cannot be edited, cannot
/// disagree with a second copy of itself, and outlives the object: a controller
/// reconciling the *name* of a snapshot that has just been deleted still knows
/// which volume it was holding.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpec {
    /// The pool holding it, which is always the source volume's pool — a copy
    /// is made where the bytes already are, and no backend copies one between
    /// pools without reading and writing every block.
    ///
    /// Derived by the API from the volume rather than asked for. It is a field
    /// at all so that a pool agent's watch filter is one comparison, exactly as
    /// an attachment carries the node it is opened on.
    pub pool: String,
}

/// What the pool can see of the copy. Every value here is observable on the
/// backend right now; none of them describe a transition.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The pool that has claimed this snapshot and reports on it.
    pub pool: Option<String>,
    /// True while the pool holds the copy.
    ///
    /// Unlike everything else in a status, this one is also *consulted* — see
    /// [`crate::storage::reconcile_snapshot`]. A snapshot that the pool no
    /// longer holds but once reported must not be taken again, because a copy
    /// made now is a copy of a different moment wearing the same name.
    pub taken: bool,
    /// How big the copy is, logically: how large the volume was at the moment
    /// it was made, and therefore the smallest volume that can be made from it.
    /// Not what it occupies in the pool — a delta against a live volume grows
    /// as the volume moves on, and billing is a separate question.
    pub size_gib: u64,
    /// The moment the copy is *of*, as the backend records it. Read from the
    /// pool rather than stamped when we noticed: an agent that was restarted
    /// mid-pass would otherwise date a week-old snapshot to this morning.
    pub taken_at: Option<Timestamp>,
}

impl Observed for SnapshotStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.pool.as_deref()
    }
}

impl Assigned for SnapshotSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // The same two-field ownership as a volume, for the same reason: the
        // bytes are in a pool, so the pool is the party with something to
        // report about them.
        Some(self.pool.as_str())
    }
}

pub type Snapshot = Resource<SnapshotSpec, SnapshotStatus>;

/// The guard a **volume** carries while any snapshot has been taken from it.
///
/// Held on the source, not on the copy, because the danger runs that way: on
/// every backend this platform will speak to — LVM thin, ZFS, Ceph RBD — a
/// snapshot is a delta against the volume it came from, and destroying the
/// volume makes the copies unreadable. Which of them are full copies is a
/// property of the backend and not of anything an operator wrote down, so the
/// platform assumes the dependency exists; the cost of assuming wrongly is a
/// delete that waits for an explicit second delete, and the cost of guessing
/// the other way is data nobody can get back.
pub const SNAPSHOT_SOURCE_FINALIZER: &str = "snapshot.velstra.io/source";

// ---- pool ----------------------------------------------------------------

/// Somewhere volumes live: an LVM volume group, a ZFS dataset, a Ceph pool, a
/// directory.
///
/// It is to storage what a [`Node`] is to compute, and deliberately the same
/// shape — `spec` is what an operator decides about it, `status` is what its
/// agent reports. Nothing here describes *how* to talk to the backend, because
/// that is not an operator's statement about the world: the agent knows what it
/// is running and reports it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolSpec {
    /// False drains the pool: nothing new is provisioned into it, what exists
    /// stays. A spec change rather than a command, so a restart cannot lose it
    /// half way.
    pub accepting: bool,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// What the agent found itself running — `lvm`, `zfs`, `ceph`, `directory`.
    /// Observed rather than declared: an operator writing `zfs` over an LVM pool
    /// would be describing a world that does not exist.
    pub backend: String,
    pub capacity_gib: u64,
    /// Counted from the volumes this pool holds, never tracked as a running
    /// total — the same reason quota is counted rather than incremented.
    pub allocated_gib: u64,
    pub agent_version: String,
    pub last_heartbeat: Timestamp,
}

impl Observed for PoolStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
    fn self_owned(&self) -> bool {
        true
    }
}

impl Assigned for PoolSpec {}

pub type Pool = Resource<PoolSpec, PoolStatus>;

pub type Volume = Resource<VolumeSpec, VolumeStatus>;

/// Attaching a volume to an instance is its **own resource**, not a field on
/// either one.
///
/// This is the single most important shape in the storage model. A field on the
/// volume has two writers (the controller that wants it attached, the node that
/// knows whether it is) and no way to express "detached but the node still has
/// it open". As a resource with a finalizer, the sequence is forced: the
/// controller asks, the node acts and reports, the node releases the finalizer,
/// and only then does the object go. A crash anywhere in that leaves a truthful
/// object rather than a volume that cannot be reattached.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttachmentSpec {
    pub volume: String,
    pub instance: String,
    /// The node that must open it — copied from the instance so the agent's
    /// watch filter is a single field.
    pub node: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttachmentStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// True only while the node has it open. There is no third value.
    pub attached: bool,
    /// `/dev/vdb` — what the guest sees, reported by the node.
    pub device: Option<String>,
    pub node: Option<String>,
}

impl Observed for AttachmentStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

pub type Attachment = Resource<AttachmentSpec, AttachmentStatus>;

/// The finalizer a node holds on an attachment until it has really let go.
pub const NODE_RELEASE_FINALIZER: &str = "node.velstra.io/release";

// ---- network -------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkSpec {
    /// The VNI on the Velstra fabric. Assigned by the controller from the
    /// cell's range, never chosen by a tenant.
    pub vni: u32,
    pub mtu: u32,
    /// This network carries addresses the world can reach.
    ///
    /// An operator's declaration and never a tenant's: a tenant that could mark
    /// a network external could mint itself a public range by writing a CIDR
    /// into a subnet. What it means is that the prefixes on this network's
    /// subnets are **real** — routed to this cell by whoever is above it — so
    /// an address taken from one is an address somebody can reach.
    #[serde(default)]
    pub external: bool,
    /// How this cell tells the network above it where an address is.
    ///
    /// The cell's answer, which an individual address may override. See
    /// [`crate::public`] for what the two modes cost. Meaningless on a network
    /// that is not external, and ignored there rather than refused: a tenant
    /// network carrying the default value is not a mistake worth a sentence.
    #[serde(default)]
    pub announce: crate::public::Announce,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    // There was a `programmed_on: Vec<String>` here, and nothing ever wrote it:
    // a list of nodes is an aggregate, and an aggregate is not a fact anybody
    // owns — every node in the cell would have been writing into one field,
    // which the one-writer rule forbids. It is the same shape that was removed
    // from `ImageStatus`, and the same answer applies if it is ever wanted
    // back: each node says what it holds, and the API adds them up on read.
}

impl Observed for NetworkStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type Network = Resource<NetworkSpec, NetworkStatus>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubnetSpec {
    pub network: String,
    pub cidr: String,
    pub gateway: String,
    pub dns: Vec<String>,
    /// Addresses the platform will not hand out.
    pub reserved: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubnetStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    pub allocated: u32,
    pub available: u32,
}

impl Observed for SubnetStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type Subnet = Resource<SubnetSpec, SubnetStatus>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortSpec {
    pub network: String,
    pub subnet: String,
    /// The node this port is assigned to — **derived**, never written by a
    /// client: it is the node holding the guest that uses the port, copied from
    /// that instance by the port controller.
    ///
    /// It is here because the access rule needs it. "The fact wins while it
    /// exists; the assignee may claim only when nobody holds it" leaves a port
    /// with neither unclaimable by anybody, which is what it was: every node's
    /// attempt to report on a port it was carrying was refused, and the port sat
    /// at `programmed: false` for ever while the guest ran perfectly. Same shape
    /// as `AttachmentSpec::node`, and derived the same way, so a port naming the
    /// wrong node is not something anybody can write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Allocated by the IPAM controller, then never changed — an address that
    /// moves under a running guest is an outage.
    pub address: Option<String>,
    pub mac: Option<String>,
    pub security_groups: Vec<String>,
    /// Egress and ingress ceilings, in megabits. Multi-tenancy without these is
    /// one noisy neighbour away from an incident.
    pub rate_limit_mbit: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The node whose datapath carries it.
    pub node: Option<String>,
    /// True when the Velstra agent has the port in its maps.
    pub programmed: bool,
    pub tap_device: Option<String>,
}

impl Observed for PortStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

pub type Port = Resource<PortSpec, PortStatus>;

// ---- operation -----------------------------------------------------------

/// AIP-151: a long-running operation is a resource an operator can look at,
/// not a connection they must hold open.
///
/// It carries no state of its own beyond a pointer at the target and what the
/// caller asked for — "done" is computed from the target's own convergence, so
/// an operation cannot disagree with the object it describes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OperationSpec {
    /// The resource this operation is about.
    pub target: String,
    /// The generation of the target this operation is waiting for.
    pub target_generation: u64,
    pub verb: String,
    pub requested_by: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OperationStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    pub done: bool,
    pub error: Option<String>,
    pub finished_at: Option<Timestamp>,
}

impl Observed for OperationStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

pub type Operation = Resource<OperationSpec, OperationStatus>;

impl Assigned for ProjectSpec {}

impl Assigned for NodeSpec {}

impl Assigned for ImageSpec {}

impl Assigned for InstanceSpec {
    fn assigned_owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

impl Assigned for AttachmentSpec {
    fn assigned_owner(&self) -> Option<&str> {
        Some(self.node.as_str())
    }
}

impl Assigned for NetworkSpec {}

impl Assigned for SubnetSpec {}

impl Assigned for PortSpec {
    fn assigned_owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

impl Assigned for OperationSpec {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{ConditionStatus, Placement, ResourceName};

    fn instance(generation: u64, observed: u64) -> Instance {
        let mut meta = Meta::new(
            ResourceName::parse("projects/p1/instances/i1").unwrap(),
            Placement::new("eu-central", "cell-1"),
        );
        meta.generation = generation;
        Resource::new(
            meta,
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: Default::default(),
                console: false,
                devices: Vec::new(),
                vcpus: 2,
                memory_mib: 4096,
                ..Default::default()
            },
            InstanceStatus {
                running_size: None,
                console_tail: String::new(),
                console_bytes: 0,
                devices: Vec::new(),
                observed_generation: observed,
                ..Default::default()
            },
        )
    }

    #[test]
    fn convergence_is_the_only_thing_in_progress_means() {
        assert!(instance(3, 3).converged());
        let behind = instance(4, 3);
        assert!(!behind.converged());
        assert_eq!(behind.drift(), 1);
    }

    #[test]
    fn an_instance_that_was_never_reported_is_unknown_not_pending() {
        // The distinction matters: `Unknown` says nobody has looked yet, and it
        // is replaced by an observation. A `Pending` would be a claim about the
        // world made by something that cannot see it.
        let i = instance(1, 0);
        assert_eq!(i.status.state, InstanceState::Unknown);
        assert!(i.status.node.is_none());
    }

    #[test]
    fn only_the_reporting_node_owns_an_instances_status() {
        let mut i = instance(1, 1);
        i.status.node = Some("node-a".into());
        assert_eq!(i.status.owner(), Some("node-a"));
    }

    #[test]
    fn an_attachment_carries_its_own_truth() {
        // Neither the volume nor the instance says whether it is attached —
        // this object does, and only the node writes it.
        let a = Attachment::new(
            Meta::new(
                ResourceName::parse("projects/p1/attachments/a1").unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            AttachmentSpec {
                volume: "projects/p1/volumes/v1".into(),
                instance: "projects/p1/instances/i1".into(),
                node: "node-a".into(),
                read_only: false,
            },
            AttachmentStatus::default(),
        );
        assert!(!a.status.attached);
        assert!(a.status.node.is_none(), "nobody has reported yet");
    }

    #[test]
    fn a_condition_carries_the_reason_onto_the_object() {
        let mut i = instance(2, 1);
        crate::meta::set_condition(
            &mut i.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::False,
                "NoCapacity",
                "no node in cell-1 has 4096 MiB free on one NUMA node",
                2,
            ),
        );
        let c = crate::meta::condition(&i.status.conditions, "Ready").unwrap();
        assert_eq!(c.reason, "NoCapacity");
        assert!(
            c.message.contains("NUMA"),
            "the sentence an operator reads is on the object"
        );
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;
    use crate::meta::{Meta, Placement, ResourceName};

    fn guest(spec_vcpus: u32, running: Option<RunningSize>) -> Instance {
        Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                vcpus: spec_vcpus,
                memory_mib: 4096,
                root_disk_gib: 20,
                ..Default::default()
            },
            InstanceStatus {
                state: InstanceState::Running,
                running_size: running,
                ..Default::default()
            },
        )
    }

    /// The bug this exists for: a resize of a running guest used to be
    /// accepted, do nothing, and read as applied.
    #[test]
    fn resizing_a_running_guest_is_reported_rather_than_silently_ignored() {
        let g = guest(
            8,
            Some(RunningSize {
                vcpus: 4,
                memory_mib: 4096,
                root_disk_gib: 20,
            }),
        );
        let pending = pending_changes(&g);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].field, "vcpus");
        // The sentence carries both numbers, because "pending" without them is
        // a badge somebody dismisses.
        assert_eq!(pending[0].to_string(), "vcpus: 4 → 8");
    }

    #[test]
    fn a_guest_running_what_was_asked_for_has_nothing_pending() {
        let g = guest(
            4,
            Some(RunningSize {
                vcpus: 4,
                memory_mib: 4096,
                root_disk_gib: 20,
            }),
        );
        assert!(pending_changes(&g).is_empty());
    }

    /// A guest that is not running has nothing to differ from.
    ///
    /// Its next start gives it whatever the spec says, by construction — so
    /// reporting a pending change would be reporting one that does not exist.
    #[test]
    fn a_stopped_guest_has_nothing_pending_whatever_the_spec_says() {
        assert!(pending_changes(&guest(64, None)).is_empty());
    }

    #[test]
    fn every_size_field_is_compared_not_just_the_first() {
        let mut g = guest(
            8,
            Some(RunningSize {
                vcpus: 4,
                memory_mib: 2048,
                root_disk_gib: 10,
            }),
        );
        g.spec.memory_mib = 4096;
        g.spec.root_disk_gib = 20;
        let fields: Vec<&str> = pending_changes(&g).iter().map(|c| c.field).collect();
        assert_eq!(fields, ["vcpus", "memoryMib", "rootDiskGib"]);
    }
}
