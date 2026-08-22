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

/// Limits, counted rather than reserved. A reservation that is not released on
/// a crash is a quota that shrinks over time; a count is recomputed from what
/// exists and cannot drift.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    pub instances: u32,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub volume_gib: u64,
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
    pub labels: Vec<String>,
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
    pub numa_free_mib: Vec<u64>,
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
    /// `sha256:…` — also the resource id.
    pub digest: String,
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
    pub addresses: Vec<String>,
    /// Host-side identity of the running machine, for the console and for
    /// re-derivation after an agent restart.
    pub vmm_pid: Option<u32>,
    pub started_at: Option<Timestamp>,
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
                vcpus: 2,
                memory_mib: 4096,
                ..Default::default()
            },
            InstanceStatus {
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
