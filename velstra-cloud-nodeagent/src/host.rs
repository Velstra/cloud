//! What a node can be asked to do to itself, and what it can be asked about.
//!
//! Two traits, and the split between them is deliberate. [`Vmm`] owns
//! everything with a lifetime on this machine — image bytes, root disks, guest
//! processes, open volumes. [`Datapath`] owns ports, because programming a port
//! is the fabric's job and the fabric is a different piece of software with a
//! different failure mode; a hypervisor that cannot reach its network is a
//! different incident from a hypervisor that cannot start a guest, and folding
//! them into one trait would make the two indistinguishable in a report.
//!
//! The important method on both is `observe`. Every other method changes the
//! machine; `observe` is the *only* source of what is true on it. Nothing in
//! this crate remembers what it did — see [`crate::agent`] for why that is the
//! whole recovery model rather than an optimisation.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use velstra_cloud_model::{
    meta::Timestamp,
    migration::MigrationMode,
    resources::{Capacity, InstanceState, NetworkSpec, PortSpec},
    security::ResolvedRule,
};

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The machine refused, and this sentence is what an operator will read on
    /// the object. It is written for them, not for a log parser.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl HostError {
    pub fn failed(what: impl std::fmt::Display) -> Self {
        Self::Failed(what.to_string())
    }
}

pub type Result<T> = std::result::Result<T, HostError>;

/// One guest, as seen on the host right now.
///
/// There is no `Booting` here for the same reason there is none in the model:
/// a value that means "in progress" outlives whatever wrote it, and then it is
/// a lie nobody owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmObservation {
    pub state: InstanceState,
    /// The host process, for the console and for an operator with `ps`. Absent
    /// once the VMM is gone, which is exactly how a crash becomes visible.
    pub pid: Option<u32>,
    pub started_at: Option<Timestamp>,
}

/// A receiver waiting for a guest that is being moved here.
///
/// Everything in it is read off the machine on every scan, which is the point:
/// a receiver is a process, and a process that died has to stop being ready or
/// the source sends into nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receiver {
    /// Where the source must send — read back from what is actually listening,
    /// never the URL this node asked for. The two differ exactly when it
    /// matters: a port that could not be bound, a receiver started by an
    /// earlier agent, a socket that was cleaned up underneath us.
    pub url: String,
    /// What has arrived so far. Progress for an operator watching a large guest
    /// move, and zero from a VMM that will not say — a backend that cannot
    /// count reports nothing rather than an invention.
    pub received_mib: u64,
}

/// Everything the reconcile functions need to know about this machine, gathered
/// in one scan so a single pass sees one consistent picture of the host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostState {
    /// Keyed by instance resource name.
    pub vms: BTreeMap<String, VmObservation>,
    /// Instance names that have a root disk.
    pub disks: BTreeSet<String>,
    /// Image digests present *and verified*. An unverified copy is not in here,
    /// because "cached" is what the agent boots from without asking again.
    pub images: BTreeSet<String>,
    /// Volume name to the device the guest sees, for the volumes this node
    /// currently holds open.
    pub volumes: BTreeMap<String, String>,
    /// Receivers listening on this machine right now, keyed by the instance
    /// each one is waiting for. A guest that has arrived is in `vms` and no
    /// longer here: the receiver was the thing that took delivery, and once it
    /// has, it is a running VMM rather than a receiver.
    pub receivers: BTreeMap<String, Receiver>,
    /// Instances this machine is sending away right now.
    ///
    /// Observed, like everything else, because a transfer outlives the pass
    /// that started it: without this an agent would start a second send on top
    /// of the first on every resync while a large guest copies.
    pub sending: BTreeSet<String>,
}

/// What a guest needs to exist. Assembled by the agent from the instance's
/// spec plus what the datapath handed back, so a `Vmm` never reads a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmRequest {
    /// The instance resource name — the identity everything on the host is
    /// keyed by, so a restarted agent can match a running process back to an
    /// object without a local table.
    pub instance: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub image: String,
    pub root_disk_gib: u64,
    /// The guest's NICs, in the order the instance's ports are declared. The
    /// order is the guest's NIC order, and a guest that finds its addresses on
    /// the wrong NIC after a restart is an outage with no error message.
    pub nics: Vec<Nic>,
}

/// One NIC, as the host has to build it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nic {
    /// The host tap device the datapath handed back for this port.
    pub tap: String,
    /// The hardware address the guest must come up with — the port's, so that
    /// the platform and the guest agree about which NIC this is.
    ///
    /// Everything downstream of a NIC is keyed by it: DHCP finds the guest's
    /// binding by it, the metadata service's network document matches on it,
    /// and the fabric's filters are written against it. A VMM left to invent
    /// one gives the guest an identity nothing else in the system has heard of,
    /// and a guest that is migrated comes back as a different machine.
    ///
    /// `None` only for a port that has no MAC of its own, where letting the
    /// VMM choose is still better than refusing to start the guest.
    pub mac: Option<String>,
}

/// What a transfer needs. Assembled by the agent from the migration's spec, so
/// a `Vmm` never reads a resource — the same rule [`VmRequest`] follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub instance: String,
    /// `unix:/path` on one machine or `tcp:host:port` between two, exactly as
    /// the destination published it.
    pub url: String,
    pub mode: MigrationMode,
    pub downtime_ms: u32,
    pub timeout_s: u32,
    /// Parallel streams. More than one is unsupported over a unix socket, and a
    /// backend that is handed both must refuse rather than half-obey.
    pub connections: u8,
}

#[async_trait]
pub trait Vmm: Send + Sync + 'static {
    /// Read the machine. Never a cache, never a file this process wrote.
    async fn observe(&self) -> Result<HostState>;

    /// Fetch and verify an image. Must be idempotent: an image already present
    /// and verified is a success, not an error.
    async fn pull_image(&self, digest: &str) -> Result<()>;

    /// Make the guest's root disk: a copy of `image`, grown to `gib`.
    ///
    /// The copy is not a step afterwards. A disk that exists empty for even one
    /// pass is a disk a guest can be started from, and a guest booted off an
    /// empty disk fails in the least legible way there is.
    async fn create_disk(&self, instance: &str, gib: u64, image: &str) -> Result<()>;

    async fn start(&self, request: &VmRequest) -> Result<()>;

    async fn stop(&self, instance: &str) -> Result<()>;

    /// Remove the guest and everything of it on this node.
    async fn delete(&self, instance: &str) -> Result<()>;

    /// Open a volume for a guest and return the device the guest sees.
    async fn open_volume(&self, instance: &str, volume: &str, read_only: bool) -> Result<String>;

    async fn close_volume(&self, instance: &str, volume: &str) -> Result<()>;

    /// What this machine has. Reported, never assumed by a scheduler.
    async fn capacity(&self) -> Result<Capacity>;

    // ---- moving a guest in and out ---------------------------------------
    //
    // Four methods and no fifth: whether a receiver is listening and how much
    // has arrived are *observations*, and they are in `observe` with everything
    // else this machine can say about itself. A method that answered "is it
    // ready" separately would be a second source of truth about one machine,
    // and the two would disagree on the pass where it matters.

    /// Start listening for a guest being moved here, and return the URL the
    /// source must send to.
    ///
    /// The whole request is handed over because the receiving side of a
    /// migration is not an empty box: the guest resumes into devices that have
    /// to exist here — the taps by the names its configuration carries — and
    /// one of the two backends builds its full command line up front.
    ///
    /// Idempotent. A receiver already listening is success, and the URL it
    /// answers with is the one it is listening on, not the one that was wanted.
    async fn prepare_receiver(&self, request: &VmRequest, mode: MigrationMode) -> Result<String>;

    /// Stop listening. Must never take down a VMM that is by now holding a
    /// guest — on both backends the receiver *becomes* the guest's VMM the
    /// moment the transfer lands, so "tear down the receiver" and "kill the
    /// guest that just arrived" are one command apart.
    async fn tear_down_receiver(&self, instance: &str) -> Result<()>;

    /// Begin sending, and return once the transfer is under way — not once it
    /// has finished.
    ///
    /// A method that blocked until the last page was copied would hold this
    /// node's whole pass for as long as a large guest takes to move, which
    /// means no heartbeat, no other instance converging, nothing. Whether it
    /// finished is `observe`'s answer: the guest is gone from here.
    async fn send(&self, transfer: &Transfer) -> Result<()>;

    /// Abandon a transfer and keep the guest. Only ever safe under pre-copy,
    /// which is why the model refuses to call it under any other mode.
    async fn cancel_send(&self, instance: &str) -> Result<()>;
}

/// A port as the datapath currently has it.
///
/// The rules are part of the observation and not merely of the request, because
/// a decision that cannot see them cannot notice when they have gone stale —
/// and they go stale without anything about the port being touched, since a
/// group's membership is a property of the cell. A datapath that reported only
/// its taps would make "programmed" mean "present", and a guest that joined a
/// group would keep the allowances it had before it joined for ever.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgrammedPort {
    pub tap: String,
    pub rules: Vec<ResolvedRule>,
}

#[async_trait]
pub trait Datapath: Send + Sync + 'static {
    /// Port resource name to what the datapath has for it, for the ports
    /// programmed on this node right now.
    async fn observe(&self) -> Result<BTreeMap<String, ProgrammedPort>>;

    /// Program a port and return its tap device. Idempotent: the fabric takes a
    /// desired map, not a delta, so asking twice is asking once.
    ///
    /// `rules` is what the port's security groups came to, already resolved to
    /// prefixes — no group names, nothing this implementation would have to look
    /// up. It is passed separately from the spec rather than folded into it
    /// because it is not part of what anybody declared: it is a function of the
    /// groups *and* of which ports currently hold which addresses, recomputed
    /// every pass, so a member coming or going changes it without the port
    /// having been touched.
    ///
    /// An empty list is not "unrestricted": it is the platform's own default —
    /// ingress denied, egress allowed, replies allowed — with nothing added.
    ///
    /// `network` is the segment the port is on, and it is passed rather than
    /// looked up because `spec.network` is only its *name*. A datapath that has
    /// to put a frame on a wire needs the segment's identity **on** that wire —
    /// the VNI — and its MTU, and neither is derivable from a resource name. The
    /// fake and the tap-only datapath ignore it; the one that programs an
    /// overlay cannot.
    async fn program(
        &self,
        port: &str,
        spec: &PortSpec,
        network: &NetworkSpec,
        rules: &[ResolvedRule],
    ) -> Result<String>;

    async fn unprogram(&self, port: &str) -> Result<()>;
}
