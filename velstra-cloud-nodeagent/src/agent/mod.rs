//! The loop on the node.
//!
//! One shape, and everything else in this crate exists to serve it:
//!
//! ```text
//! observe this machine → ask the pure function what to do → do it → report status
//! ```
//!
//! Three properties fall out of that and are worth stating, because each one is
//! a thing the systems this replaces do differently and pay for:
//!
//! 1. **The node is the only source of what is on the node.** Nothing here
//!    remembers what it did. `observed` is re-derived from the VMM and the
//!    datapath on every pass, so an agent that is killed mid-action, restarted,
//!    upgraded or moved to a different binary comes back knowing exactly as much
//!    as it did before: whatever the machine says. There is no local database to
//!    disagree with the machine, and therefore no reconciliation between two
//!    local truths.
//! 2. **Nothing is a command.** The agent receives objects, not RPCs. No
//!    controller ever calls a node — which is why a controller may die at any
//!    point without leaving a half-delivered instruction, and why an agent that
//!    was unreachable for an hour needs no catch-up protocol. It reads the
//!    objects and closes the gap.
//! 3. **Work is proportional to this node's own objects.** A pass is a loop over
//!    the objects assigned to *this* node. Nothing here scales with the number
//!    of nodes, instances or tenants in the cell.
//!
//! On (3) there is one honest caveat in this build: [`velstra_cloud_store`]
//! offers a prefix `list` and a prefix `watch`, so the *fetch* is cell-wide even
//! though the work is not. The seam for fixing that is [`Agent::concerns_me`]
//! and the list calls in [`Agent::resync`] — when the cell gateway grows a
//! node-scoped list and watch, they move there and nothing else in this file
//! changes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, ConditionStatus, Placement, set_condition},
    migration::{MigrationSpec, MigrationStatus},
    reconcile::{Action, instance_condition, reconcile_attachment, reconcile_instance},
    resources::{
        Attachment, AttachmentSpec, AttachmentStatus, ImageSpec, Instance, InstanceSpec,
        InstanceStatus, NODE_RELEASE_FINALIZER, NetworkSpec, NodeSpec, NodeStatus, Port, PortSpec,
        PortStatus,
    },
    security::{ResolvedRule, SecurityGroup, SecurityGroupSpec, effective_rules_with, members_in},
};
use velstra_cloud_store::{Store, TypedStore};

use crate::{
    cell::{CellReader, StoreCell},
    guests::GuestRegistry,
    host::{Datapath, HostState, Nic, ProgrammedPort, VmRequest, Vmm},
};

mod ceph;
mod migrate;
mod status;
mod writing;

use migrate::Moving;
use status::{attachment_condition, host_condition, observe_instance, release_condition};

/// How often a single object may be pushed forward within one pass.
///
/// Convergence is deliberately allowed to take several rounds inside a pass —
/// image, disk, ports, then the guest — because a create that needed four
/// resync intervals to boot would make the interval a latency knob, and a
/// latency knob is how a cluster ends up resyncing every 200ms. The cap is
/// there because a host that reports the work as undone after doing it would
/// otherwise spin forever; hitting it is a fault on the object, not a retry.
const ROUNDS: usize = 4;

/// What one pass did. Counters rather than a log, so a test can state a
/// property ("a converged node does nothing") as an assertion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Actions performed on this machine.
    pub actions: usize,
    /// Status writes accepted.
    pub reports: usize,
    /// Writes that lost a compare-and-swap. Not an error: somebody changed the
    /// object while we were acting, and the next pass sees the new one.
    pub conflicts: usize,
    /// Writes the store refused on ownership grounds. Always worth an
    /// operator's attention — it means two parties disagree about who runs
    /// this object.
    pub refused: usize,
    /// Actions the machine could not carry out. The reason is on the object.
    pub failures: usize,
}

/// Who this agent is, and how eagerly it re-reads the world.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub node: String,
    pub placement: Placement,
    /// The full sweep interval. Watch events drive the common case; this is the
    /// floor under everything a watch can miss — a dropped connection, a
    /// compaction, a change made while the process was not running.
    pub resync: Duration,
    pub agent_version: String,
    /// Where this node answers a console attach, as `host:port`.
    ///
    /// The address actually bound and actually reachable, filled in by the
    /// process once its listener is up — not what was asked for on a command
    /// line, because a node that could not have the port it wanted must
    /// advertise what it got. Empty means this node serves no console, which is
    /// a real answer: the API then says so rather than offering a button that
    /// leads nowhere.
    pub console_endpoint: String,
    /// Whether this machine's state directory is storage every node reaches.
    /// Told, never worked out — see the flag's own documentation.
    pub shared_state: bool,
    /// The keys an image's signature must verify under before this node fetches
    /// it. Judged here as well as at the API, because a node's copy of the
    /// cell is what it acts on, and a store written around the API is still a
    /// store this node reads.
    pub image_signing_keys: Vec<velstra_cloud_model::images::SigningKey>,
    /// Refuse to fetch an image that carries no signature at all.
    pub require_signed_images: bool,
}

impl AgentConfig {
    pub fn new(node: &str, region: &str, cell: &str) -> Self {
        Self {
            node: node.to_string(),
            placement: Placement::new(region, cell),
            resync: Duration::from_secs(30),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            console_endpoint: String::new(),
            shared_state: false,
            image_signing_keys: Vec::new(),
            require_signed_images: false,
        }
    }
}

impl Agent {
    /// Make this node the first hop for the segments its guests are on.
    ///
    /// Off by default, and that is the safe default rather than the useful one:
    /// a node that starts routing and translating a tenant's frames because
    /// nobody said otherwise is a node that quietly overrode the cell's network
    /// design. `quickstart` turns it on, because a home cell has no fabric and a
    /// guest nobody can reach is not a guest.
    pub fn as_first_hop(mut self, prefix: &str) -> Self {
        self.localnet = Some(crate::localnet::LocalNet::new(prefix));
        self
    }

    /// Make the far end of every wire this node carries.
    ///
    /// Idempotent and computed whole every time — see [`crate::localnet`] — so
    /// calling it twice in a pass costs a handful of `ip` calls and leaves the
    /// machine where it was. That matters, because it is called at two moments
    /// on purpose: once before this node's guests are acted on, and again the
    /// instant a port is programmed.
    ///
    /// The second call is the fix for a race that looked like a metadata bug for
    /// a long time. A guest starts in the **same pass** as the tap it runs on,
    /// so a picture of the segments built before that pass acted cannot contain
    /// the tap — and the bridge, the gateway address and the route to
    /// `169.254.169.254` all arrived a resync later. A stock cloud image does
    /// its entire cloud-init inside fifteen seconds, finds nothing, and comes up
    /// with no user and no SSH key.
    /// Answers whether the machine now looks the way it should, so a caller with
    /// a pass to count against can count it. A node that could not make the far
    /// end has guests that will boot and reach nothing, which is a failure of
    /// the pass and not a detail of it.
    async fn ensure_first_hop(
        &self,
        ports: &BTreeMap<String, Port>,
        subnets: &BTreeMap<String, velstra_cloud_model::resources::Subnet>,
        networks: &BTreeMap<String, NetworkSpec>,
        taps: &BTreeMap<String, String>,
    ) -> bool {
        let Some(localnet) = &self.localnet else {
            return true;
        };
        let segments = crate::localnet::segments(ports, subnets, networks, taps);
        match localnet.apply(&segments).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "could not make this node the first hop");
                false
            }
        }
    }
}

/// Whether this agent may touch an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ownership {
    /// The stored status says this node owns it. Act.
    Mine,
    /// A controller assigned it here, but the status is owned by nobody or by
    /// another node. Report — the store's answer to that report is how this
    /// node learns whether the handover has completed — and do nothing else.
    Claim,
    /// Not ours in any sense.
    Skip,
}

/// Just the tap devices, for the many places that need to know a port is
/// carried and nothing more.
fn taps_of(programmed: &BTreeMap<String, ProgrammedPort>) -> BTreeMap<String, String> {
    programmed
        .iter()
        .map(|(port, p)| (port.clone(), p.tap.clone()))
        .collect()
}

/// What one pass has read about the cell, handed down whole.
///
/// Both of these are needed together wherever either is: what a port is allowed
/// is a function of the groups *and* of every port, so a caller holding one
/// without the other cannot answer the question anyway.
pub(super) struct CellView<'a> {
    pub ports: &'a BTreeMap<String, Port>,
    /// The whole group object, not just its spec.
    ///
    /// `status.members` is the part that matters and it is easy to drop: the
    /// API computes, once and centrally, every address in a group, precisely so
    /// a node does not have to read every port in the cell to expand a rule
    /// that names another group. Keeping only the spec here threw that away —
    /// and the fallback (work it out from the ports this node can see) is
    /// *silently narrower* through the API, which hands a node only its own
    /// ports. A rule naming a group whose members are on other machines then
    /// expanded to nothing, and a guest lost traffic its tenant had allowed
    /// with no error anywhere.
    pub groups: &'a BTreeMap<String, SecurityGroup>,
    /// Read once per pass and handed down, for the same reason the groups are:
    /// a port's segment is a fact about the cell, and looking it up again per
    /// port would be the same answer fetched many times. It also replaces a
    /// second read this pass used to make when it described guests.
    pub networks: &'a BTreeMap<String, NetworkSpec>,
    /// The segments those networks are cut into — where a port's address, mask
    /// and gateway come from.
    ///
    /// Here rather than only in the guest description because the far end of a
    /// wire has to exist before the guest on it starts, and the guest starts in
    /// the same pass that makes the wire. See [`crate::localnet`].
    pub subnets: &'a BTreeMap<String, velstra_cloud_model::resources::Subnet>,
    /// The registered images, by name — where an image's bytes come from.
    ///
    /// Read once per pass and handed down like the networks above. A node can
    /// verify an image from the digest in its name but cannot invent its
    /// source, so without this the agent could refuse a bad image and never
    /// obtain a good one.
    pub images: &'a BTreeMap<String, ImageSpec>,
    /// Every instance this cell holds.
    ///
    /// Here rather than as one more parameter, and that is what this struct is
    /// for: it is what this pass read of the cell, handed down once. The start
    /// order needs it — whether a guest may start is a question about the
    /// machine, not about the guest, and it can only be answered with the
    /// whole list in hand.
    pub instances: &'a [Instance],
}

pub struct Agent {
    config: AgentConfig,
    writer: Writer,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    /// Written, not read: what this node reports about a capture it is making.
    captures: TypedStore<
        velstra_cloud_model::capture::CaptureSpec,
        velstra_cloud_model::capture::CaptureStatus,
    >,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    /// Written, not read: the destination of a migration owns its status and
    /// reports on it. What this node is *told* about migrations comes through
    /// `cell`, which hands it only the ones naming it.
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    /// Written, not read: what this gateway reports about the sessions it
    /// speaks. What it is told comes through `cell`, like everything else.
    bgp_peers: TypedStore<
        velstra_cloud_model::resources::BgpPeerSpec,
        velstra_cloud_model::resources::BgpPeerStatus,
    >,
    /// Everything this node *reads* about the cell, and the only thing that ever
    /// grew with the cell rather than with this node's own work. See
    /// [`crate::cell`] for the two ways it can be answered and why it matters.
    cell: Arc<dyn CellReader>,
    vmm: Arc<dyn Vmm>,
    datapath: Arc<dyn Datapath>,
    /// The far end of the wire, when this node is it. `None` is the fabric case
    /// and the deliberate dead end — see [`crate::localnet`], which is also
    /// where the reason a guest on a first-hop-less node cannot be logged into
    /// is written down.
    localnet: Option<crate::localnet::LocalNet>,
    guests: GuestRegistry,
    /// How this node runs Ceph's own tools. A field so a test can point it at
    /// something that is not `cephadm`, and so the pass does not construct one
    /// per call.
    cephadm: crate::cephadm::CephAdmin,
    /// An agent whose Ceph reads come back filtered says so once. A
    /// configuration mistake repeated every resync would bury everything else.
    warned_about_ceph_reads: AtomicBool,
    /// Whether the "can this agent read the cell at all" probe has been made.
    /// Once per process: it tests a configuration fact, not a runtime one.
    probed_ceph_reads: AtomicBool,
    /// An agent that cannot write its own node object says so once. Repeating
    /// it every resync would bury everything else in the journal.
    warned_about_node: AtomicBool,
    /// Where status reports go. `None` is the direct-store default — this agent
    /// writes to the store as `Writer::agent`, and the store judges it. `Some` is
    /// `--api` mode: reports go through the API as this node's own token, which
    /// authenticates them rather than trusting a self-declared identity. See
    /// [`crate::sink`].
    sink: Option<Arc<dyn crate::sink::StatusSink>>,
    /// When this agent last managed to report its own node object.
    ///
    /// The watchdog's only input, and the reason it needs nothing from anybody:
    /// a node that cannot reach the control plane cannot be *told* to stop, so
    /// it has to decide by itself, against its own clock. Milliseconds since
    /// the epoch, in an atomic because the pass that updates it and the loop
    /// that reads it are not the same task.
    ///
    /// Starts at the moment the agent was built. An agent that has never
    /// managed to report is one that has not been trusted with anything yet,
    /// and giving it a grace period from startup is what stops a control plane
    /// that is merely slow to come up from taking every guest down with it.
    last_report: AtomicU64,
    /// When this node last asked a guest to power down for a cold move, per
    /// instance, in milliseconds since the epoch.
    ///
    /// Process-local on purpose, and the same shape as the `sending` guard: it
    /// is an observation about what this machine has already done, not a fact
    /// about the cell. An agent restarted mid-handover asks again, which is
    /// right — level-triggered, and a power button pressed twice costs nothing
    /// where pressing it four times a second for ever does.
    handover_asked: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
    /// This node's fencing deadline, as last read from its own object.
    ///
    /// Cached rather than read when it is needed, and that is the whole design:
    /// the moment fencing matters is the moment this agent cannot read
    /// anything. A deadline it has to fetch is a deadline it will never get.
    fence_after_s: AtomicU32,
    /// The half of the host that speaks BGP, when it has one. `None` — the
    /// default, and every test's — makes the whole pass a no-op: a machine
    /// with no routing daemon must never be asked to reload one.
    bgp: Option<Arc<dyn crate::bgp::BgpSpeaker>>,
    /// What the speaker was last asked to say, so a settled cell reloads
    /// nothing. Process-local like `handover_asked`: an observation about what
    /// this machine already did.
    bgp_applied: std::sync::Mutex<Option<crate::bgp::BgpDesired>>,
}

impl Agent {
    /// Reads the store directly, which is what this did from the beginning.
    /// [`Agent::reading`] is the same agent pointed at something that hands it
    /// only its own share.
    pub fn new(
        store: Arc<dyn Store>,
        config: AgentConfig,
        vmm: Arc<dyn Vmm>,
        datapath: Arc<dyn Datapath>,
    ) -> Self {
        let reader = Arc::new(StoreCell::new(
            store.clone(),
            &config.placement.cell,
            &config.node,
        ));
        Self::reading(store, config, vmm, datapath, reader)
    }

    pub fn reading(
        store: Arc<dyn Store>,
        config: AgentConfig,
        vmm: Arc<dyn Vmm>,
        datapath: Arc<dyn Datapath>,
        reader: Arc<dyn CellReader>,
    ) -> Self {
        let cell = config.placement.cell.clone();
        Self {
            writer: Writer::agent(&config.node),
            instances: TypedStore::new(store.clone(), &cell, "instances"),
            attachments: TypedStore::new(store.clone(), &cell, "attachments"),
            ports: TypedStore::new(store.clone(), &cell, "ports"),
            captures: TypedStore::new(store.clone(), &cell, "captures"),
            nodes: TypedStore::new(store.clone(), &cell, "nodes"),
            bgp_peers: TypedStore::new(store.clone(), &cell, "bgp-peers"),
            migrations: TypedStore::new(store, &cell, "migrations"),
            cell: reader,
            config,
            vmm,
            datapath,
            localnet: None,
            guests: GuestRegistry::new(),
            cephadm: crate::cephadm::CephAdmin::default(),
            warned_about_ceph_reads: AtomicBool::new(false),
            probed_ceph_reads: AtomicBool::new(false),
            warned_about_node: AtomicBool::new(false),
            sink: None,
            handover_asked: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            last_report: AtomicU64::new(velstra_cloud_model::meta::Timestamp::now().0),
            fence_after_s: AtomicU32::new(0),
            bgp: None,
            bgp_applied: std::sync::Mutex::new(None),
        }
    }

    /// Give this agent a routing daemon to keep honest. Production hands it
    /// FRR; a test hands it a fake; most machines never get one at all.
    pub fn with_bgp(mut self, speaker: Arc<dyn crate::bgp::BgpSpeaker>) -> Self {
        self.bgp = Some(speaker);
        self
    }

    /// Route this agent's status reports through `sink` — the API — instead of
    /// straight to the store.
    ///
    /// This is what makes `--api` mode a trust boundary: with it, a report is
    /// authenticated as this node's own token and the API refuses anything that
    /// is not this node's, so the credential a node holds can write only its own
    /// objects. Without it (the default), the agent writes to the store directly
    /// and the store judges a self-declared identity — the single-operator phase.
    ///
    /// Reads follow the same seam through [`Agent::reading`]; a `--api` agent uses
    /// the API for both.
    pub fn with_status_sink(mut self, sink: Arc<dyn crate::sink::StatusSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Point the Ceph pass at different binaries.
    ///
    /// Exists for the tests, and it is the honest seam: `cephadm` and `ceph` are
    /// the entire interface to Ceph, so a test that substitutes them is testing
    /// the real argv the real pass builds — rather than a trait that could
    /// drift from what the commands actually are.
    pub fn with_ceph_tools(mut self, admin: crate::cephadm::CephAdmin) -> Self {
        self.cephadm = admin;
        self
    }

    /// How often this node writes its heartbeat, which is **not** how often it
    /// reconciles.
    ///
    /// The two used to be the same number, and that was the bug. A node's
    /// heartbeat happens to be written by the reconcile pass, and the platform
    /// judges a node gone after
    /// [`velstra_cloud_model::ceph::NODE_STALE_AFTER_MS`] — so an operator who
    /// lengthened the interval to quieten a large cell made every node in it
    /// permanently dead. The Ceph deployment then blocks on the first stale
    /// node it walks, and a blocked step halts everything behind it: total,
    /// silent, and survivable only on a single-node cluster, so it fails in
    /// exactly the configuration least likely to be tested.
    ///
    /// The first fix was to shorten the reconcile interval to match, which is
    /// the wrong end. An operator who asked for a long interval asked to cut
    /// *list* load, and overriding that would hand them twenty times the load
    /// they were avoiding — in a cell that may have no Ceph cluster and never
    /// will. So the cadences are separate instead: the reconcile keeps the
    /// interval it was given, and the heartbeat runs at whatever is short
    /// enough to stay true, which is O(1) — one read and one write of this
    /// node's own object, and none of the list calls that make a pass cost
    /// anything.
    pub fn heartbeat_interval(&self) -> std::time::Duration {
        let longest =
            std::time::Duration::from_millis(velstra_cloud_model::ceph::longest_useful_resync_ms());
        self.config.resync.min(longest)
    }

    /// Say this node is still here, and nothing else.
    ///
    /// Deliberately not a small reconcile: it reads one object and writes one
    /// field. A conflict is not worth reporting — the other writer is this same
    /// agent's own pass, which has just written a fresher heartbeat than this
    /// one would have.
    async fn touch_heartbeat(&self) {
        let stored = match self.own_node().await {
            Ok(Some(stored)) => stored,
            // A node that is not there is not this agent's to invent, and is
            // silent by design. A read that *failed* is different — the store is
            // unreachable and the heartbeat is missed — so it gets a line rather
            // than passing for "nothing to do".
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(error = %e, "could not read this node's own object for a heartbeat");
                return;
            }
        };
        let mut next = stored.clone();
        next.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
        if let Some(sink) = &self.sink {
            let value = serde_json::to_value(&next).expect("a node always serialises");
            if let crate::sink::SinkOutcome::Failed(why) =
                sink.write_status("nodes", &value, &self.writer).await
            {
                tracing::debug!(error = %why, "could not write this node's heartbeat through the API");
            }
            return;
        }
        if let Err(e) = self.nodes.update(&next, &self.writer).await {
            tracing::debug!(error = %e, "could not write this node's heartbeat");
        }
    }

    /// The registry the metadata service and the DHCP responder both answer
    /// from. Handed out so all three share one map rather than three that can
    /// differ — a guest leased an address the metadata service does not think
    /// it has is a guest nobody can debug.
    pub fn guests(&self) -> GuestRegistry {
        self.guests.clone()
    }

    /// What this agent reads the cell through, for the console service, which
    /// checks a ticket against the same sessions the pass would see.
    pub fn cell(&self) -> Arc<dyn CellReader> {
        self.cell.clone()
    }

    /// Where this agent reports status, for the console service, which has one
    /// thing to say: that a ticket has been spent.
    ///
    /// `None` on an agent writing straight to a store, which is the developer
    /// cell — there is no second writer there to go through.
    pub fn status_sink(&self) -> Option<Arc<dyn crate::sink::StatusSink>> {
        self.sink.clone()
    }

    /// Where this node answers a console attach, once its listener is up.
    pub fn set_console_endpoint(&mut self, endpoint: &str) {
        self.config.console_endpoint = endpoint.to_string();
    }

    /// The interval this agent reconciles at — exactly what it was asked for.
    /// The heartbeat runs separately; see [`Agent::heartbeat_interval`].
    pub fn resync_interval(&self) -> std::time::Duration {
        self.config.resync
    }

    pub fn node(&self) -> &str {
        &self.config.node
    }

    // ---- the loop --------------------------------------------------------

    /// Watch, and resync on a timer, until `shutdown` completes.
    ///
    /// The watch is an optimisation over the timer, never a replacement for it:
    /// every event is answered with the same full pass a timer tick would run,
    /// so a missed event costs latency and nothing else. That is what lets the
    /// stream be a plain unreliable channel instead of a protocol.
    pub async fn run(&self, shutdown: impl std::future::Future<Output = ()> + Send) {
        // Watch first, then list. The other order has a gap in it exactly one
        // change wide, and that change is invisible until the next resync.
        let mut wake = self.cell.wake().await;
        tracing::info!(node = %self.config.node, reads = %self.cell.describe(), "watching");

        self.resync().await;

        // The ticker runs at the *heartbeat* cadence, which is the shorter of
        // the two, and a full pass happens when the reconcile interval has
        // elapsed. One task and one writer for this node's object — a second
        // timer writing it concurrently would be two writers of one object,
        // which is the thing this platform does not do.
        let tick = self.heartbeat_interval();
        let mut ticker = tokio::time::interval(tick);
        ticker.tick().await; // the first tick is immediate, and we just swept
        let mut since_swept = std::time::Duration::ZERO;
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            enum Next {
                Tick,
                Woken,
                Nothing,
            }
            let next = tokio::select! {
                _ = &mut shutdown => return,
                _ = ticker.tick() => Next::Tick,
                woken = wake.recv() => {
                    if woken.is_some() { Next::Woken } else { Next::Nothing }
                }
            };
            // A pass is level-triggered, so a burst collapses into one sweep
            // rather than one sweep each.
            while wake.try_recv().is_ok() {}
            match next {
                Next::Woken => {
                    self.resync().await;
                    since_swept = std::time::Duration::ZERO;
                }
                Next::Tick => {
                    since_swept += tick;
                    if since_swept >= self.config.resync {
                        self.resync().await;
                        since_swept = std::time::Duration::ZERO;
                    } else {
                        // Between sweeps, say this node is still here. One read
                        // and one write; none of the list calls that are what a
                        // long interval was chosen to avoid.
                        self.touch_heartbeat().await;
                    }
                }
                Next::Nothing => {}
            }
        }
    }

    /// Which devices this guest is to be given.
    ///
    /// **Already-held devices win.** A guest that is running was assigned
    /// these when it started, and re-choosing on every pass would hand it a
    /// different GPU the moment another one freed up — a restart for a reason
    /// nobody could see. This is the same rule the CPU follows and for the
    /// same reason: what a running guest holds is a fact recorded once.
    ///
    /// For a guest that holds none, the choice is made from what this node has
    /// free right now, and a shortfall is returned as a sentence rather than
    /// as an empty list — a guest silently started without the accelerator it
    /// asked for is worse than one that does not start.
    async fn devices_for(
        &self,
        instance: &Instance,
        host: &crate::host::HostState,
    ) -> Result<Vec<String>, String> {
        if instance.spec.devices.is_empty() {
            return Ok(Vec::new());
        }
        if !instance.status.devices.is_empty() {
            return Ok(instance.status.devices.clone());
        }
        let classes = self.device_classes().await;
        velstra_cloud_model::pci::assign(&instance.spec.devices, &classes, &host.pci_devices)
            .map_err(|why| why.to_string())
    }

    /// The cell's PCI device classes, by id.
    ///
    /// Empty when the cell has none or they cannot be read. A node that
    /// invented a class would start a guest without the hardware it asked
    /// for; an empty map refuses it by name instead.
    async fn device_classes(
        &self,
    ) -> std::collections::BTreeMap<String, velstra_cloud_model::pci::DeviceClassSpec> {
        match self.cell.device_classes().await {
            Ok(all) => all,
            Err(e) => {
                tracing::warn!(error = %e, "could not read this cell's device classes");
                Default::default()
            }
        }
    }

    /// Stop every guest on this machine, because nobody has heard from it.
    ///
    /// The mechanism the whole of recovery rests on. A node that has gone quiet
    /// is not a node that has stopped — it may be unreachable and still running
    /// everything it holds — so before anything may be started elsewhere, *this*
    /// has to have happened.
    ///
    /// It needs nothing from anybody, which is the point: an agent that cannot
    /// reach the control plane cannot be told to stop. It reads its own clock,
    /// its own last successful report, and its own node object's deadline.
    ///
    /// The deadline comes from the node's spec, which this agent last read while
    /// it could still read anything. A node that has never been given one does
    /// not fence — and is never recovered from, which is the honest pair.
    async fn self_fence_pass(&self, host: &crate::host::HostState, pass: &mut Pass) {
        let fence_after_s = self.fence_after_s.load(Ordering::Relaxed);
        let last = velstra_cloud_model::meta::Timestamp(self.last_report.load(Ordering::Relaxed));
        let now = velstra_cloud_model::meta::Timestamp::now();
        if !velstra_cloud_model::ha::should_self_fence(fence_after_s, last, now) {
            return;
        }

        let running: Vec<String> = host
            .vms
            .iter()
            .filter(|(_, vm)| vm.state == velstra_cloud_model::resources::InstanceState::Running)
            .map(|(name, _)| name.clone())
            .collect();
        if running.is_empty() {
            return;
        }

        // Said loudly and every time, unlike the once-per-spell warnings
        // elsewhere. This is the agent taking somebody's guests down, and a
        // line per occurrence is the record of why they went.
        tracing::error!(
            node = %self.config.node,
            quiet_s = (now.0.saturating_sub(last.0)) / 1000,
            fence_after_s,
            guests = running.len(),
            "this node has not been heard from for longer than its fencing deadline; \
             stopping its guests so they cannot run twice"
        );
        for instance in running {
            if let Err(e) = self.vmm.stop(&instance).await {
                // Reported and carried on. A guest that will not stop is worse
                // than one that will, and the remaining ones are still worth
                // stopping — leaving them running because a neighbour was
                // stubborn would be the wrong half of a bad situation.
                tracing::error!(instance = %instance, error = %e, "could not stop a guest while fencing");
                pass.failures += 1;
            }
        }
    }

    /// Whether this guest may start yet, given the rest of this node.
    ///
    /// Built here because the question is about the machine, not the guest: it
    /// needs every instance on this node and how far along each one is, which
    /// only the agent holding them has.
    ///
    /// The peers are taken from the *store's* view of each instance rather than
    /// from this pass's observation, for one reason: a guest on this node that
    /// this agent has not visited yet in this pass still counts, and leaving it
    /// out would let the group behind it go early exactly once per restart.
    fn start_gate_for(
        instance: &Instance,
        all: &[Instance],
        host: &crate::host::HostState,
    ) -> velstra_cloud_model::reconcile::StartGate {
        use velstra_cloud_model::reconcile::{StartGate, StartPeer, start_gate};

        // Nothing to work out for the first group with no wait — which is
        // every guest in every cell that has not asked for an order, so it is
        // the path that has to cost nothing.
        if instance.spec.start_order == 0 && instance.spec.start_delay_s == 0 {
            return StartGate::Go;
        }

        let here = instance
            .status
            .node
            .clone()
            .or_else(|| instance.spec.node.clone());
        let peers: Vec<StartPeer> = all
            .iter()
            .filter(|other| other.meta.name != instance.meta.name)
            .filter(|other| {
                let theirs = other
                    .status
                    .node
                    .clone()
                    .or_else(|| other.spec.node.clone());
                theirs.is_some() && theirs == here
            })
            .filter(|other| !other.meta.is_deleting())
            .map(|other| {
                let name = other.meta.name.to_string();
                // What this machine sees, falling back to what was last
                // reported: a guest this agent has looked at already is more
                // current than the stored copy, and one it has not is not
                // therefore absent.
                let seen = host.vms.get(&name);
                StartPeer {
                    order: other.spec.start_order,
                    state: seen.map(|vm| vm.state).unwrap_or(other.status.state),
                    desired: other.spec.desired_state,
                    started_at: seen
                        .and_then(|vm| vm.started_at)
                        .or(other.status.started_at),
                    name,
                }
            })
            .collect();

        start_gate(
            instance.spec.start_order,
            instance.spec.start_delay_s,
            &peers,
            velstra_cloud_model::meta::Timestamp::now(),
        )
    }

    /// Mark the devices this node's guests are holding.
    ///
    /// The VMM reports sysfs, which cannot tell a device passed to a guest
    /// from a free one — both are bound to `vfio-pci`. Only the instance
    /// objects know, and only this node's own instances matter, because a
    /// device is a piece of this machine.
    ///
    /// Applied in one place for the same reason the CPU baseline is: two call
    /// sites overlaying it separately is how a node comes to offer a device it
    /// has already given away.
    fn mark_held_devices(host: &mut crate::host::HostState, mine: &[&Instance]) {
        let held: std::collections::BTreeMap<String, String> = mine
            .iter()
            .flat_map(|i| {
                let name = i.meta.name.to_string();
                i.status
                    .devices
                    .iter()
                    .map(move |address| (address.clone(), name.clone()))
            })
            .collect();
        for device in &mut host.pci_devices {
            if let Some(instance) = held.get(&device.address) {
                device.state = velstra_cloud_model::pci::DeviceUse::Guest {
                    instance: instance.clone(),
                };
            }
        }
    }

    /// The baseline this node has been told to present, from its own object.
    ///
    /// Read from the node's spec rather than from local configuration so an
    /// operator can declare it through the API instead of editing files on
    /// every machine, and so what a node presents is visible to the same
    /// people who decide it. Unreadable node, or none declared: the host's own
    /// processor, which is the default everywhere else too.
    pub(super) async fn declared_baseline(&self) -> Option<velstra_cloud_model::cpu::CpuLevel> {
        self.own_node().await.ok().flatten()?.spec.cpu_baseline
    }

    /// Apply the declared baseline to what the VMM reported.
    ///
    /// One place, deliberately. The node's status and every guest's recorded
    /// CPU are both derived from this value, and two call sites applying it
    /// separately is how a node comes to report one CPU and hand out another.
    fn present_baseline(
        host: &mut crate::host::HostState,
        baseline: Option<velstra_cloud_model::cpu::CpuLevel>,
    ) {
        let Some(cpu) = host.cpu.as_mut() else {
            return;
        };
        match baseline {
            Some(level) => {
                cpu.presents = level.to_string();
                cpu.presented_flags = level.flags();
            }
            None => {
                cpu.presents = "host".to_string();
                cpu.presented_flags = cpu.flags.clone();
            }
        }
    }

    /// Safe to call as often as anyone likes, and on a converged node it
    /// One full pass over everything this node owns.
    ///
    /// performs no actions and writes nothing — which is the property that
    /// makes the resync interval a matter of taste rather than of load.
    pub async fn resync(&self) -> Pass {
        let mut pass = Pass::default();

        let mut host = match self.vmm.observe().await {
            Ok(host) => host,
            Err(e) => {
                // Without a picture of the machine there is nothing honest to
                // do: acting blind is how two copies of a guest happen.
                tracing::error!(error = %e, "could not read this machine; skipping the pass");
                pass.failures += 1;
                return pass;
            }
        };
        let baseline = self.declared_baseline().await;
        Self::present_baseline(&mut host, baseline);
        let host = host;
        let programmed = match self.datapath.observe().await {
            Ok(programmed) => programmed,
            Err(e) => {
                tracing::error!(error = %e, "could not read the datapath; skipping the pass");
                pass.failures += 1;
                return pass;
            }
        };

        let ports = match self.cell.ports().await {
            Ok(ports) => ports
                .into_iter()
                .map(|p| (p.meta.name.to_string(), p))
                .collect::<BTreeMap<_, _>>(),
            Err(e) => {
                tracing::error!(error = %e, "could not list ports");
                pass.failures += 1;
                return pass;
            }
        };

        // Read once per pass and handed down: what a port is allowed depends on
        // every port in the cell, so recomputing it per instance would be the
        // same answer fetched many times.
        let groups = match self.cell.security_groups().await {
            Ok(list) => list
                .into_iter()
                .map(|g| (g.meta.name.to_string(), g))
                .collect::<BTreeMap<_, _>>(),
            Err(e) => {
                // Fewer allowances, never more: a port is still programmed, with
                // whatever its groups came to, which with none readable is
                // nothing. A guest with no network at all would be the worse
                // failure.
                tracing::warn!(error = %e, "could not list security groups");
                BTreeMap::new()
            }
        };

        let networks = match self.cell.networks().await {
            Ok(list) => list
                .into_iter()
                .map(|n| (n.meta.name.to_string(), n.spec))
                .collect::<BTreeMap<_, _>>(),
            Err(e) => {
                // A port whose segment cannot be read is a port that must not be
                // programmed: putting a frame on the wrong wire is worse than
                // putting it on none. The pass says so on the object rather than
                // guessing at a VNI.
                tracing::warn!(error = %e, "could not list networks");
                BTreeMap::new()
            }
        };

        // The same read `refresh_guests` used to make on its own, moved up so the
        // pass that *creates* a wire can also make its far end. A segment that
        // cannot be read leaves the far end alone rather than inventing one.
        let subnets = match self.cell.subnets().await {
            Ok(list) => list
                .into_iter()
                .map(|s| (s.meta.name.to_string(), s))
                .collect::<BTreeMap<_, _>>(),
            Err(e) => {
                tracing::warn!(error = %e, "could not list subnets");
                BTreeMap::new()
            }
        };

        let images = match self.cell.images().await {
            Ok(list) => list
                .into_iter()
                .map(|i| (i.meta.name.to_string(), i.spec))
                .collect::<BTreeMap<_, _>>(),
            Err(e) => {
                // An image that cannot be looked up is an image that cannot be
                // fetched; the pull below says so on the object rather than
                // downloading from a guess.
                tracing::warn!(error = %e, "could not list images");
                BTreeMap::new()
            }
        };

        let instances = match self.cell.instances().await {
            Ok(instances) => instances,
            Err(e) => {
                tracing::error!(error = %e, "could not list instances");
                pass.failures += 1;
                return pass;
            }
        };

        let cell = CellView {
            ports: &ports,
            groups: &groups,
            networks: &networks,
            subnets: &subnets,
            images: &images,
            instances: &instances,
        };

        let migrations = match self.cell.migrations().await {
            Ok(migrations) => migrations,
            Err(e) => {
                tracing::error!(error = %e, "could not list migrations");
                pass.failures += 1;
                return pass;
            }
        };
        // Sending comes first in the pass because a send that lands changes
        // what the instance loop below is looking at: the guest is gone from
        // this machine, and the only right thing to do about that is to report
        // it, not to start it again.
        let mut moving = self.source_pass(&migrations, &host, &mut pass).await;
        // Both halves of "leave this instance alone", in one place and before
        // any instance is acted on: what this node is giving up, and what is on
        // its way here.
        self.freeze_arrivals(&migrations, &host, &mut moving);
        let host = if moving.released.is_empty() {
            host
        } else {
            match self.vmm.observe().await {
                Ok(host) => host,
                Err(e) => {
                    tracing::error!(error = %e, "could not re-read this machine after a handover");
                    pass.failures += 1;
                    return pass;
                }
            }
        };

        // Before a single guest is acted on. A port programmed on an earlier
        // pass — after a restart, say — has a tap and no bridge until somebody
        // makes one, and the guest on it is already running.
        let taps_now = taps_of(&programmed);
        if !self
            .ensure_first_hop(&ports, &subnets, &networks, &taps_now)
            .await
        {
            pass.failures += 1;
        }

        let mut mine = Vec::new();
        for instance in &instances {
            if moving.released.contains(&instance.meta.name.to_string()) {
                // Handed over. Not this node's to act on any more — and in
                // particular not to claim back, which is what the ownership
                // rule below would otherwise have it do the moment the status
                // says nobody owns it.
                //
                // Letting go is said once, by the node that was holding it.
                // Afterwards there is nothing left here to report, and saying
                // it again would be this node writing about an object it no
                // longer owns — which the store would rightly refuse.
                if instance.status.node.as_deref() == Some(self.config.node.as_str()) {
                    self.release_instance(instance, &mut pass).await;
                }
                continue;
            }
            match self.ownership(
                instance.status.node.as_deref(),
                instance.spec.node.as_deref(),
            ) {
                Ownership::Mine => {
                    self.instance_pass(instance, &host, &programmed, &cell, &moving, &mut pass)
                        .await;
                    mine.push(instance);
                }
                Ownership::Claim => {
                    let me = self.config.node.clone();
                    self.claim(
                        &self.instances,
                        instance,
                        |status| status.node = Some(me),
                        &mut pass,
                    )
                    .await
                }
                Ownership::Skip => {}
            }
        }

        // A guest with no instance anywhere in the cell. Nothing will ever ask
        // for it to be stopped: the delete pipeline stops a guest while its
        // record is still there, and once the record is gone no pass looks at
        // the guest again. Found live as a QEMU that outlived its instance by
        // two days, holding a tap for a port that no longer existed — which the
        // tap sweep below then tried to remove, and was refused, every pass,
        // for ever ("Device or resource busy").
        //
        // Cell-wide, not this node's share: a guest mid-arrival or mid-handover
        // still has its record, so the only guests this touches are the ones
        // nobody's books mention at all. The list read failing returns early
        // above, so an unreadable store never looks like an empty one.
        let known: BTreeSet<String> = instances.iter().map(|i| i.meta.name.to_string()).collect();
        for name in host.vms.keys() {
            if known.contains(name) {
                continue;
            }
            tracing::warn!(instance = %name,
                "stopping a guest whose instance is gone from the cell");
            // `kill`, not `stop`: the graceful path asks over the monitor, and
            // an orphan whose directory was deleted has no monitor left to ask
            // — its unit is the only handle that still works.
            if let Err(e) = self.vmm.kill(name).await {
                tracing::warn!(instance = %name, error = %e, "the orphaned guest would not stop");
                pass.failures += 1;
            } else {
                pass.actions += 1;
            }
        }

        match self.cell.attachments().await {
            Ok(attachments) => {
                for attachment in &attachments {
                    match self.ownership(
                        attachment.status.node.as_deref(),
                        Some(attachment.spec.node.as_str()),
                    ) {
                        Ownership::Mine => self.attachment_pass(attachment, &host, &mut pass).await,
                        Ownership::Claim => {
                            let me = self.config.node.clone();
                            self.claim(
                                &self.attachments,
                                attachment,
                                |status| status.node = Some(me),
                                &mut pass,
                            )
                            .await
                        }
                        Ownership::Skip => {}
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "could not list attachments");
                pass.failures += 1;
            }
        }

        // The instances above have been programming and unprogramming ports, so
        // the snapshot this pass started with is out of date. Ports are
        // reported from what the datapath says *now*, or a port would always be
        // one pass behind the guest that uses it.
        let taps = match self.datapath.observe().await {
            Ok(programmed) => taps_of(&programmed),
            Err(e) => {
                tracing::error!(error = %e, "could not re-read the datapath");
                pass.failures += 1;
                return pass;
            }
        };

        // A port is this node's business either because it says so or because
        // one of this node's guests is plugged into it.
        let referenced: BTreeSet<&str> = mine
            .iter()
            .flat_map(|i| i.spec.ports.iter().map(String::as_str))
            .collect();
        // Every port of every guest that concerns this node — assigned here,
        // still reported here, or named by an open migration with this node at
        // either end. A migration names both ends on purpose: the source keeps
        // its half of the wire standing until the record is gone, and the
        // destination may program its half before the guest lands.
        let me = self.config.node.as_str();
        let in_my_share: BTreeSet<&str> = instances
            .iter()
            .filter(|i| {
                i.spec.node.as_deref() == Some(me)
                    || i.status.node.as_deref() == Some(me)
                    || migrations.iter().any(|m| {
                        // A record on its way out no longer holds the wire
                        // open — asking for the migration's deletion is the
                        // gesture that lets the source's half go.
                        !m.meta.is_deleting()
                            && m.spec.instance == i.meta.name.to_string()
                            && (m.spec.from_node == me || m.spec.to_node == me)
                    })
            })
            .flat_map(|i| i.spec.ports.iter().map(String::as_str))
            .collect();
        // Taps for nobody. A cold move stops the guest here and starts it
        // there — and left this node's half of the wire standing: the tap
        // stayed programmed, the port went on reporting `node=<here>`, and the
        // node actually running the guest politely refused to claim a port
        // another machine spoke for. Found live as a write ping-pong on one
        // port object. Once the guest stops concerning this node (the
        // migration record is gone), its tap does too.
        for (port_name, _tap) in taps.clone() {
            if in_my_share.contains(port_name.as_str()) {
                continue;
            }
            // A deleting port's teardown is the owner path's below — it is the
            // one that reports `Released`, and a second unprogram here would
            // count the same failure twice.
            if ports.get(&port_name).is_some_and(|p| p.meta.is_deleting()) {
                continue;
            }
            tracing::info!(port = %port_name,
                "tearing down a tap for a port no guest of this node's uses");
            if let Err(e) = self.datapath.unprogram(&port_name).await {
                tracing::warn!(port = %port_name, error = %e, "the leftover tap would not go");
                pass.failures += 1;
            } else {
                pass.actions += 1;
            }
        }
        let taps = match self.datapath.observe().await {
            Ok(programmed) => taps_of(&programmed),
            Err(e) => {
                tracing::error!(error = %e, "could not re-read the datapath after the sweep");
                pass.failures += 1;
                return pass;
            }
        };
        for port in ports.values() {
            let name = port.meta.name.to_string();
            let owner = port.status.node.as_deref();
            // …but reporting on it is a different question from carrying it. A
            // port whose status belongs to another node stays that node's to
            // speak for, even when a guest of this one is plugged into it —
            // which is the ordinary state of affairs for the few moments after
            // a guest arrives here and before its port object is moved. Writing
            // it anyway would be refused, and being refused every pass is how a
            // real disagreement about ownership gets lost in the noise.
            let mine_to_report = owner == Some(self.config.node.as_str())
                || (owner.is_none() && referenced.contains(name.as_str()));
            if mine_to_report {
                self.port_pass(port, &taps, in_my_share.contains(name.as_str()), &mut pass)
                    .await;
            }
        }

        // The routing daemon, after the wires: what this cell announces is a
        // statement about addresses the passes above have just made true.
        self.bgp_pass(&mut pass).await;

        // Receiving comes last, so that on the pass where a guest arrives and
        // is claimed above, the receiver it came through is taken down in the
        // same sweep. A receiver left listening holds a memory reservation on a
        // node that is not running the guest.
        self.destination_pass(&migrations, &taps, &cell, &host, &mut pass)
            .await;

        // After the guests, because a capture is of a stopped guest and the
        // pass above is what stops one: asked for and acted on in the same
        // sweep rather than a resync later.
        self.capture_pass(&mine, &mut pass).await;

        self.refresh_guests(&mine, &ports, &taps, &mut pass).await;
        // Before the node status is written, because the status is where this
        // node's Ceph report goes and the pass fills it in.
        let mut host = host;
        // Which devices this node's guests are holding. Only the instance
        // objects know — sysfs cannot tell a passed-through device from a free
        // one — so the overlay happens here, once, on the way to the report.
        Self::mark_held_devices(&mut host, &mine);
        self.ceph_pass(&mut host, &mut pass).await;
        self.node_pass(&mine, &host, &mut pass).await;
        // Last, and after the report: if that report landed, the deadline has
        // just been pushed out and nothing here fires. If it did not, this is
        // the pass that notices — and the ordering means an agent never fences
        // itself over a report it was about to make successfully.
        self.self_fence_pass(&host, &mut pass).await;
        pass
    }

    fn ownership(&self, owner: Option<&str>, assigned: Option<&str>) -> Ownership {
        let me = self.config.node.as_str();
        // The stored status wins over the spec on purpose. A scheduler that
        // has re-assigned an instance does not thereby make it ours: the node
        // that holds it has to let go first, and until it has, acting here
        // would mean two machines running one guest.
        if owner == Some(me) {
            Ownership::Mine
        } else if assigned == Some(me) {
            Ownership::Claim
        } else {
            Ownership::Skip
        }
    }

    // ---- instances -------------------------------------------------------

    async fn instance_pass(
        &self,
        stored: &Instance,
        host: &HostState,
        programmed: &BTreeMap<String, ProgrammedPort>,
        cell: &CellView<'_>,
        moving: &Moving,
        pass: &mut Pass,
    ) {
        let (ports, groups) = (cell.ports, cell.groups);
        // Owned, because the loop below acts and then looks again: the decision
        // it makes on the second round has to be made against what the datapath
        // says *then*, not against the snapshot this pass opened with.
        let mut programmed = programmed.clone();
        let mut taps = taps_of(&programmed);
        let name = stored.meta.name.to_string();
        let acted_on = stored.meta.generation;
        let mut host = host.clone();
        let mut outcome = Ok(());
        let mut previous: Option<Vec<Action>> = None;
        // Outside the round loop: the note has to survive to the report below,
        // which is where the status this pass publishes is assembled.
        let mut asked_to_stop_at: Option<velstra_cloud_model::meta::Timestamp> =
            stored.status.stop_requested_at;
        // A guest that is missing while a migration of it is open is the one
        // case where this node reports and does nothing else. Tearing down is
        // still allowed: a delete is a decision somebody made about the object,
        // and it outranks a migration that is on its way to being abandoned.
        let frozen = !stored.meta.is_deleting() && moving.stalled.contains(&name);

        for round in 0..=ROUNDS {
            if frozen {
                break;
            }
            // The status the pure function judges is what this machine says,
            // never what the store remembers. This one line is the difference
            // between an agent that re-derives and an agent that repeats a
            // start it already performed but never got to report.
            let mut observed = stored.clone();
            observe_instance(
                &mut observed.status,
                &host,
                &name,
                stored.meta.is_deleting(),
                stored.spec.console,
            );

            let actions = reconcile_instance(
                &observed,
                // By digest, because that is what the bytes are stored under:
                // an image may be called `debian-13` and a node's copy of it is
                // the same file as every other object carrying those bytes.
                cell.images
                    .get(stored.spec.image.as_str())
                    .and_then(|i| crate::hostfs::stored_as(&i.digest))
                    .is_some_and(|name| host.images.contains(&name)),
                // Not "is there a tap": a port whose group gained a member is
                // carried and out of date, and a check that only asked whether
                // it was present would never notice.
                &stored
                    .spec
                    .ports
                    .iter()
                    .map(|p| match (programmed.get(p.as_str()), ports.get(p)) {
                        (Some(have), Some(port)) => {
                            // Asked of the datapath rather than compared here:
                            // the fabric holds the rules in its own shape and a
                            // comparison in this one can never come out equal.
                            self.datapath.agrees(
                                p,
                                have,
                                &self.rules_for(&port.spec, groups, ports),
                            )
                        }
                        // Carried, but its object has not reached this cell yet:
                        // there is nothing to compare against, and asking for it
                        // to be programmed again would fail for want of the very
                        // object that is missing.
                        (Some(_), None) => true,
                        (None, _) => false,
                    })
                    .collect::<Vec<_>>(),
                host.disks.contains(&name),
                // What else is on this machine, and how far along it is. The
                // order is a property of the node rather than of any guest, so
                // it can only be answered here — with the whole list in hand.
                Self::start_gate_for(stored, cell.instances, &host),
                velstra_cloud_model::meta::Timestamp::now(),
            );

            // Letting go of the finalizer is a metadata write, which belongs to
            // a controller (see `access::judge`). What the node can do is make
            // the fact observable, and it does that in the status below.
            let work: Vec<Action> = actions.into_iter().filter(|a| !is_release(a)).collect();
            if work.is_empty() {
                break;
            }
            // Doing it again asks for exactly the same thing: this is as far as
            // the machine goes right now. Whether that is a problem is settled
            // by the status below, not by trying harder.
            if previous.as_ref() == Some(&work) {
                break;
            }
            if round == ROUNDS {
                outcome = Err(format!(
                    "this node did the same work {ROUNDS} times and the machine still asks for it"
                ));
                break;
            }
            previous = Some(work.clone());

            let mut failed = None;
            for action in &work {
                match self
                    .perform_instance(action, stored, &taps, cell, &host)
                    .await
                {
                    Ok(()) => {
                        pass.actions += 1;
                        // The button was pressed; note when, so the next pass
                        // can tell a guest that is shutting down from one that
                        // is never going to. Written here rather than inside
                        // `perform_instance`, which does not own a status.
                        if matches!(action, Action::StopVm { .. }) {
                            asked_to_stop_at
                                .get_or_insert_with(velstra_cloud_model::meta::Timestamp::now);
                        }
                    }
                    Err(why) => {
                        // The order in `reconcile_instance` is load-bearing, so
                        // a failed step stops the rest: a guest started without
                        // the port that failed to program is worse than a guest
                        // that has not started.
                        failed = Some(why);
                        break;
                    }
                }
            }
            if let Some(why) = failed {
                outcome = Err(why);
                break;
            }

            host = match self.vmm.observe().await {
                Ok(host) => host,
                Err(e) => {
                    outcome = Err(e.to_string());
                    break;
                }
            };
            programmed = match self.datapath.observe().await {
                Ok(fresh) => fresh,
                Err(e) => {
                    outcome = Err(e.to_string());
                    break;
                }
            };
            taps = taps_of(&programmed);
        }

        // "Let go" is an observation, not a conclusion drawn from the action
        // list: this node holds nothing of the object when no guest of it is
        // here and none of its ports are in the datapath.
        let released = stored.meta.is_deleting()
            && !host.vms.contains_key(&name)
            && stored.spec.ports.iter().all(|p| !taps.contains_key(p));

        let mut next = stored.clone();
        next.status.node = Some(self.config.node.clone());
        next.status.observed_generation = acted_on;
        // Set before the observation, which clears it again the moment the
        // guest is actually gone — so the note lives exactly as long as
        // somebody is waiting on a shutdown.
        next.status.stop_requested_at = asked_to_stop_at;
        observe_instance(
            &mut next.status,
            &host,
            &name,
            stored.meta.is_deleting(),
            stored.spec.console,
        );
        next.status.addresses = if stored.meta.is_deleting() {
            Vec::new()
        } else {
            stored
                .spec
                .ports
                .iter()
                .filter_map(|p| ports.get(p))
                .filter_map(|p| p.spec.address.clone())
                .collect()
        };
        // A transfer this node could not start is reported here, on the object
        // an operator is already looking at. The source may not write the
        // migration's status — that belongs to the destination — so this is the
        // only place the sentence can go, and a failure nobody can read is a
        // failure nobody fixes.
        if let (Ok(()), Some(why)) = (&outcome, moving.trouble.get(&name)) {
            outcome = Err(why.clone());
        }

        let ready = instance_condition(&next);
        set_condition(&mut next.status.conditions, ready);
        set_condition(
            &mut next.status.conditions,
            host_condition(&outcome, acted_on),
        );
        set_condition(
            &mut next.status.conditions,
            release_condition(released, stored.meta.is_deleting(), acted_on),
        );
        if outcome.is_err() {
            pass.failures += 1;
        }

        self.report(&self.instances, stored, next, pass).await;
    }

    /// What this port's security groups currently come to.
    ///
    /// Recomputed from the objects rather than remembered, because it depends on
    /// which ports hold which addresses right now: a guest joining a group
    /// changes what its neighbours are allowed without any of their objects
    /// being touched. Pure, so the same question asked twice in one pass gives
    /// the same answer, which is what makes comparing it with the datapath's
    /// observation meaningful.
    fn rules_for(
        &self,
        spec: &PortSpec,
        groups: &BTreeMap<String, SecurityGroup>,
        ports: &BTreeMap<String, Port>,
    ) -> Vec<ResolvedRule> {
        if spec.security_groups.is_empty() {
            return Vec::new();
        }
        let specs: BTreeMap<String, PortSpec> = ports
            .iter()
            .map(|(name, p)| (name.clone(), p.spec.clone()))
            .collect();
        let group_specs: BTreeMap<String, SecurityGroupSpec> = groups
            .iter()
            .map(|(name, g)| (name.clone(), g.spec.clone()))
            .collect();
        // Membership comes from the group where the group has it, and is worked
        // out from the ports only where it does not.
        //
        // Which source answers is not a detail. Reading the store directly, a
        // node sees every port in the cell and can work it out; reading the
        // API, it sees only its own — so working it out there yields *fewer*
        // members, and a rule naming a group whose members live on other
        // machines quietly expands to nothing. The API computes this centrally
        // and puts it on the group for exactly that reason. An empty
        // `status.members` is not evidence of an empty group, so it falls
        // through to counting rather than being taken as an answer.
        let members = |group: &str| -> Vec<String> {
            match groups.get(group) {
                Some(g) if !g.status.members.is_empty() => g.status.members.clone(),
                _ => members_in(group, &specs),
            }
        };
        let effective = effective_rules_with(spec, &group_specs, &members);
        if !effective.unknown_groups.is_empty() {
            tracing::warn!(
                "port names security groups that do not exist: {}",
                effective.unknown_groups.join(", ")
            );
        }
        effective.rules
    }

    async fn perform_instance(
        &self,
        action: &Action,
        instance: &Instance,
        taps: &BTreeMap<String, String>,
        cell: &CellView<'_>,
        // This machine as it was observed at the top of the pass. Needed to
        // choose PCI devices, which is a question about the hardware here.
        host: &crate::host::HostState,
    ) -> Result<(), String> {
        let (ports, groups, networks) = (cell.ports, cell.groups, cell.networks);
        let result = match action {
            // Resolved from the cell, exactly as ProgramPort resolves its port:
            // the decision names what must be present, the agent looks up what
            // it needs to make that true.
            Action::PullImage { digest } => match cell.images.get(digest) {
                // `digest` is the image's *name* — what the decision refers to.
                // What verifies the download is `spec.digest`, which is why it
                // is passed separately: the two used to be the same string, and
                // that forced every image to be called `sha256-<64 hex>` in
                // every list an operator reads.
                Some(image) => match self.image_may_be_fetched(digest, image) {
                    Ok(()) => self
                        .vmm
                        .pull_image(digest, &image.digest, &image.source_url)
                        .await
                        .map(|_| ()),
                    Err(why) => Err(crate::host::HostError::failed(why)),
                },
                None => Err(crate::host::HostError::failed(format!(
                    "{digest} is not a registered image in this cell, so this \
                     node has nowhere to fetch it from"
                ))),
            },
            Action::CreateDisk {
                instance,
                gib,
                image,
            } => {
                // The image's declared format, taken from the object rather
                // than assumed: the disk is handed to the VMM as raw, so a
                // qcow2 image has to be converted and not copied. Absent means
                // this node was told to make a disk from an image the cell does
                // not have, which `pull_image` above refuses for the same
                // reason — but the default is the safe one either way, since a
                // raw copy of a raw image is what it always was.
                let known = cell.images.get(image.as_str());
                let format = known.map(|i| i.format).unwrap_or_default();
                // The bytes are on disk under their digest, so that is what
                // finds them. An image the cell does not have leaves this empty,
                // and `create_disk` makes an empty disk — the same answer as
                // before, reached the same way.
                let digest = known.map(|i| i.digest.as_str()).unwrap_or("");
                self.vmm.create_disk(instance, *gib, digest, format).await
            }
            Action::ProgramPort { port } => match ports.get(port) {
                Some(p) => {
                    // Said out loud rather than guessed at: a datapath that
                    // programmed the port without knowing its segment would be
                    // putting a tenant's frames somewhere nobody chose.
                    match networks.get(&p.spec.network) {
                        Some(network) => {
                            let rules = self.rules_for(&p.spec, groups, ports);
                            let programmed =
                                self.datapath.program(port, &p.spec, network, &rules).await;
                            // The far end, before the guest that will use this
                            // wire is started — which happens next, in this same
                            // list of actions. Waiting for the next pass means
                            // waiting past the whole of a stock image's
                            // cloud-init.
                            if programmed.is_ok() {
                                if let Ok(now) = self.datapath.observe().await {
                                    let taps = taps_of(&now);
                                    // Counted by the pass that runs this before
                                    // the guests; here the warning in the
                                    // journal is the record, because an action
                                    // arm reports on the *port*, and a bridge
                                    // this node could not make is not something
                                    // the port did wrong.
                                    self.ensure_first_hop(ports, cell.subnets, networks, &taps)
                                        .await;
                                }
                            }
                            programmed.map(|_| ())
                        }
                        None => Err(crate::host::HostError::failed(format!(
                            "{} is on {}, which is not in the store yet",
                            port, p.spec.network
                        ))),
                    }
                }
                // The port object has not reached this cell's store yet. Not an
                // error on the machine — a thing to wait for, said out loud on
                // the instance so nobody has to guess which half is late.
                None => Err(crate::host::HostError::failed(format!(
                    "{port} is not in the store yet"
                ))),
            },
            Action::UnprogramPort { port } => self.datapath.unprogram(port).await,
            Action::StartVm { .. } | Action::RestartCrashedVm { .. } => {
                let devices = match self.devices_for(instance, host).await {
                    Ok(devices) => devices,
                    // Said on the instance rather than logged: an operator
                    // asking why their guest will not start should not have to
                    // find the agent's log on whichever machine ran it.
                    Err(why) => return Err(why),
                };
                let image_digest = cell
                    .images
                    .get(instance.spec.image.as_str())
                    .map(|i| i.digest.clone())
                    .unwrap_or_default();
                match self.vm_request(
                    instance,
                    taps,
                    ports,
                    self.declared_baseline().await,
                    devices,
                    &image_digest,
                ) {
                    Ok(request) => self.vmm.start(&request).await,
                    Err(why) => Err(crate::host::HostError::failed(why)),
                }
            }
            // The ACPI button, and a note that it was pressed — the model needs
            // the second half to know when asking has stopped being useful.
            Action::StopVm { instance } => self.vmm.stop(instance).await,
            // The note is written by the caller of `perform`, which owns the
            // status it will report; see where the pass records it.
            Action::KillVm { instance } => self.vmm.kill(instance).await,
            Action::DeleteVm { instance } => self.vmm.delete(instance).await,
            other => Err(crate::host::HostError::failed(format!(
                "{other:?} is not an instance action"
            ))),
        };
        result.map_err(|e| e.to_string())
    }

    fn vm_request(
        &self,
        instance: &Instance,
        taps: &BTreeMap<String, String>,
        ports: &BTreeMap<String, Port>,
        // Passed in rather than read here: this is a pure builder, and the
        // baseline is one read of the node's own object that the caller has
        // already made for the pass it is in the middle of.
        baseline: Option<velstra_cloud_model::cpu::CpuLevel>,
        // The PCI addresses this guest is to be given, already chosen. Same
        // reasoning: choosing is a decision with a store read behind it, and
        // this builder stays pure.
        devices: Vec<String>,
        // What identifies the bytes, as against what identifies the object. A
        // VMM needs the first: the disk it boots was made from a file filed
        // under the digest, and an image called `debian-13` says nothing about
        // which bytes that is.
        image_digest: &str,
    ) -> Result<VmRequest, String> {
        let mut wanted = Vec::with_capacity(instance.spec.ports.len());
        for port in &instance.spec.ports {
            match taps.get(port) {
                Some(tap) => wanted.push(Nic {
                    tap: tap.clone(),
                    // From the object, so the guest comes up as the NIC the
                    // rest of the platform already knows. A port whose object
                    // has not arrived yet still starts the guest — the tap is
                    // what it cannot boot without.
                    mac: ports.get(port).and_then(|p| p.spec.mac.clone()),
                }),
                None => return Err(format!("{port} is not programmed on this node")),
            }
        }
        Ok(VmRequest {
            instance: instance.meta.name.to_string(),
            vcpus: instance.spec.vcpus,
            memory_mib: instance.spec.memory_mib,
            image: image_digest.to_string(),
            root_disk_gib: instance.spec.root_disk_gib,
            nics: wanted,
            // What this guest is to be given, as declared on this node right
            // now. A guest already running is unaffected: it keeps the CPU it
            // booted with, and adopts this one the next time it starts.
            cpu_baseline: baseline,
            devices,
        })
    }

    // ---- attachments -----------------------------------------------------

    /// Turning a guest that somebody built by hand into an image.
    ///
    /// The half nothing did: a `Capture` was created, assigned to the node
    /// holding the disk, and no agent ever claimed it — so the controller that
    /// makes an image out of a finished one never had a finished one to act on.
    ///
    /// What a capture is, concretely: copy the guest's disk somewhere it will
    /// survive, hash the bytes, and report the hash. The hash is not decoration
    /// — an image's *name* carries its digest and the agent that later fetches
    /// one refuses a name without it, which is what makes a pull verifiable.
    async fn capture_pass(&self, mine: &[&Instance], pass: &mut Pass) {
        let captures = match self.cell.captures().await {
            Ok(captures) => captures,
            Err(e) => {
                tracing::error!(error = %e, "could not list captures");
                pass.failures += 1;
                return;
            }
        };
        let wanted: Vec<&velstra_cloud_model::resources::Capture> = captures
            .iter()
            .filter(|c| !c.meta.is_deleting())
            // Already done. Every pass after the first takes this path, so it
            // has to cost one comparison and no reads.
            .filter(|c| c.status.digest.is_none())
            .collect();
        if wanted.is_empty() {
            return;
        }
        let targets = match self.cell.backup_targets().await {
            Ok(targets) => targets,
            Err(e) => {
                tracing::error!(error = %e, "could not list backup targets");
                pass.failures += 1;
                return;
            }
        };
        for capture in wanted {
            match self.ownership(
                capture.status.node.as_deref(),
                Some(capture.spec.node.as_str()),
            ) {
                Ownership::Mine => self.one_capture(capture, mine, &targets, pass).await,
                Ownership::Claim => {
                    let me = self.config.node.clone();
                    self.claim(
                        &self.captures,
                        capture,
                        |status| status.node = Some(me),
                        pass,
                    )
                    .await
                }
                Ownership::Skip => {}
            }
        }
    }

    /// One capture: refuse what the model refuses, copy, hash, report.
    async fn one_capture(
        &self,
        capture: &velstra_cloud_model::resources::Capture,
        mine: &[&Instance],
        targets: &[velstra_cloud_model::resources::BackupTarget],
        pass: &mut Pass,
    ) {
        let name = capture.meta.name.to_string();
        let guest = mine
            .iter()
            .find(|i| i.meta.name.to_string() == capture.spec.instance);
        let view = velstra_cloud_model::capture::GuestView {
            name: capture.spec.instance.clone(),
            running: guest
                .map(|g| g.status.state == velstra_cloud_model::resources::InstanceState::Running)
                .unwrap_or(false),
            // This node holds the guest — that is what being assigned here
            // means — so a guest it cannot find in its own list is one that has
            // gone, not one that is unplaced.
            node: guest.map(|_| self.config.node.clone()),
            deleting: guest.map(|g| g.meta.is_deleting()).unwrap_or(false),
        };
        let target = targets
            .iter()
            .find(|t| t.meta.name.to_string() == capture.spec.target);
        let usable = target
            .map(|t| t.spec.accepting && t.status.writable != Some(false))
            .unwrap_or(false);

        // The model's own rule, asked here rather than re-derived. A running
        // guest is the refusal this exists for: a disk copied from under one is
        // crash-consistent, which a template stamped out a hundred times must
        // not be.
        if let Err(why) =
            velstra_cloud_model::capture::may_capture(&view, usable, &capture.spec.target)
        {
            self.say_about_capture(
                capture,
                ConditionStatus::False,
                "Refused",
                &why.to_string(),
                pass,
            )
            .await;
            return;
        }
        let Some(target) = target else { return };

        let Some(from) = self.vmm.disk_path(&capture.spec.instance) else {
            self.say_about_capture(
                capture,
                ConditionStatus::False,
                "NoDisk",
                &format!(
                    "{} has no disk this node can read. A guest whose root disk is a pool volume \
                     is captured from the pool, not from here.",
                    capture.spec.instance
                ),
                pass,
            )
            .await;
            return;
        };

        // Written under a temporary name and hashed *there*. The final name
        // carries the digest, and a digest cannot be known before the bytes
        // exist — so the copy is made, read back, and only then given the name
        // an image will be fetched by.
        let staging = format!(
            "{}/{}.capturing",
            target.spec.path.trim_end_matches('/'),
            name.replace('/', "~")
        );
        if let Err(e) = tokio::fs::copy(&from, &staging).await {
            tracing::warn!(capture = %name, error = %e, "the disk could not be copied out");
            pass.failures += 1;
            self.say_about_capture(
                capture,
                ConditionStatus::False,
                "CopyFailed",
                &e.to_string(),
                pass,
            )
            .await;
            return;
        }
        let digest = match crate::hostfs::sha256_file(std::path::Path::new(&staging)).await {
            // Prefixed here rather than in the hasher: `sha256_file` answers
            // what the bytes hash to, and *which* algorithm that was is part of
            // the name an image is fetched by — `capture::image_id` and
            // `image_url` both split on the colon.
            Ok(digest) => format!("sha256:{digest}"),
            Err(e) => {
                let _ = std::fs::remove_file(&staging);
                tracing::warn!(capture = %name, error = %e, "the copy could not be read back");
                pass.failures += 1;
                self.say_about_capture(
                    capture,
                    ConditionStatus::False,
                    "Unreadable",
                    &e.to_string(),
                    pass,
                )
                .await;
                return;
            }
        };
        let final_path = velstra_cloud_model::capture::image_url(&target.spec.path, &digest)
            .trim_start_matches("file://")
            .to_string();
        if let Err(e) = std::fs::rename(&staging, &final_path) {
            let _ = std::fs::remove_file(&staging);
            self.say_about_capture(
                capture,
                ConditionStatus::False,
                "CopyFailed",
                &format!("{staging} could not be moved into place: {e}"),
                pass,
            )
            .await;
            return;
        }
        let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        pass.actions += 1;

        let mut next = capture.clone();
        next.status.digest = Some(digest);
        next.status.size_bytes = size;
        next.status.finished_at = Some(velstra_cloud_model::meta::Timestamp::now());
        next.status.observed_generation = capture.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::True,
                "Captured",
                &format!("copied to {final_path}"),
                capture.meta.generation,
            ),
        );
        self.report(&self.captures, capture, next, pass).await;
    }

    /// Say on the capture why it has not happened.
    async fn say_about_capture(
        &self,
        capture: &velstra_cloud_model::resources::Capture,
        status: ConditionStatus,
        reason: &str,
        message: &str,
        pass: &mut Pass,
    ) {
        let mut next = capture.clone();
        next.status.observed_generation = capture.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new("Ready", status, reason, message, capture.meta.generation),
        );
        self.report(&self.captures, capture, next, pass).await;
    }

    async fn attachment_pass(&self, stored: &Attachment, host: &HostState, pass: &mut Pass) {
        let acted_on = stored.meta.generation;
        let volume = stored.spec.volume.clone();
        let mut host = host.clone();
        let mut outcome = Ok(());
        let mut previous: Option<Vec<Action>> = None;

        for round in 0..=ROUNDS {
            let mut observed = stored.clone();
            observed.status.attached = host.volumes.contains_key(&volume);

            let actions = reconcile_attachment(&observed);
            let work: Vec<Action> = actions.into_iter().filter(|a| !is_release(a)).collect();
            if work.is_empty() || previous.as_ref() == Some(&work) {
                break;
            }
            if round == ROUNDS {
                outcome = Err(format!(
                    "this node did the same work {ROUNDS} times and the machine still asks for it"
                ));
                break;
            }
            previous = Some(work.clone());

            let mut failed = None;
            for action in &work {
                match self.perform_attachment(action, stored).await {
                    Ok(()) => pass.actions += 1,
                    Err(why) => {
                        failed = Some(why);
                        break;
                    }
                }
            }
            if let Some(why) = failed {
                outcome = Err(why);
                break;
            }
            host = match self.vmm.observe().await {
                Ok(host) => host,
                Err(e) => {
                    outcome = Err(e.to_string());
                    break;
                }
            };
        }

        // The same observation as for an instance: the node has let go when
        // the volume is not open here. Two nodes with one volume open is the
        // failure the whole finalizer dance exists to prevent, so the signal a
        // controller acts on is a fact rather than an inference.
        let released = stored.meta.is_deleting() && !host.volumes.contains_key(&volume);

        let mut next = stored.clone();
        next.status.node = Some(self.config.node.clone());
        next.status.observed_generation = acted_on;
        next.status.attached = host.volumes.contains_key(&volume);
        next.status.device = host.volumes.get(&volume).cloned();
        let ready = attachment_condition(&next);
        set_condition(&mut next.status.conditions, ready);
        set_condition(
            &mut next.status.conditions,
            host_condition(&outcome, acted_on),
        );
        set_condition(
            &mut next.status.conditions,
            release_condition(released, stored.meta.is_deleting(), acted_on),
        );
        if outcome.is_err() {
            pass.failures += 1;
        }

        self.report(&self.attachments, stored, next, pass).await;
    }

    async fn perform_attachment(
        &self,
        action: &Action,
        attachment: &Attachment,
    ) -> Result<(), String> {
        let instance = attachment.spec.instance.as_str();
        let result = match action {
            Action::OpenVolume { volume, read_only } => {
                // From the attachment, because a node is not told about volumes
                // — the pool's own answer, mirrored here by a controller. Empty
                // means the pool has not placed it yet, and waiting is the
                // honest thing: the version that guessed built a path out of the
                // guest's directory and failed for ever with `No such file`.
                if attachment.spec.at.is_empty() {
                    return Err(format!(
                        "{volume} has not been placed by its pool yet, so there is nothing to open"
                    ));
                }
                self.vmm
                    .open_volume(instance, volume, &attachment.spec.at, *read_only)
                    .await
                    .map(|_| ())
            }
            Action::CloseVolume { volume } => self.vmm.close_volume(instance, volume).await,
            other => Err(crate::host::HostError::failed(format!(
                "{other:?} is not an attachment action"
            ))),
        };
        result.map_err(|e| e.to_string())
    }

    // ---- bgp -------------------------------------------------------------

    /// Keep the routing daemon saying what the cell claims, and report what
    /// the far end made of it.
    ///
    /// A no-op on any machine without a speaker or without a session assigned
    /// to it — which is every machine on most cells — so the ordinary cost is
    /// one list read. See [`crate::bgp`] for what gets announced and why it is
    /// derived rather than listed.
    async fn bgp_pass(&self, pass: &mut Pass) {
        let Some(speaker) = &self.bgp else { return };
        let peers = match self.cell.bgp_peers().await {
            Ok(peers) => peers,
            Err(e) => {
                tracing::error!(error = %e, "could not list bgp peers");
                pass.failures += 1;
                return;
            }
        };
        let me = self.config.node.as_str();
        let mine: Vec<_> = peers.iter().filter(|p| p.spec.node == me).collect();
        if mine.is_empty() {
            // Nothing to say — which is different from nothing to do. A
            // `bgp-peers` object has no finalizer, so the last one naming this
            // machine vanishes between two passes, and the daemon it
            // programmed is still up, still announcing the cell to a router
            // that was told to stop trusting it. The one write this branch
            // may make is the empty file; whether it has to is asked of the
            // daemon (a machine that never spoke is not written to).
            let held = self
                .bgp_applied
                .lock()
                .expect("nothing panics holding the applied-config lock")
                .clone();
            let still_speaking = match &held {
                Some(applied) => !applied.sessions.is_empty(),
                None => speaker.is_speaking().await,
            };
            if still_speaking {
                let silence = crate::bgp::desired_for(me, &[], &[], &[], &[]);
                match speaker.apply(&silence).await {
                    Ok(()) => {
                        pass.actions += 1;
                        *self
                            .bgp_applied
                            .lock()
                            .expect("nothing panics holding the applied-config lock") =
                            Some(silence);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "could not silence the routing daemon");
                        pass.failures += 1;
                    }
                }
            }
            return;
        }
        let (networks, subnets, floating) = match (
            self.cell.networks().await,
            self.cell.subnets().await,
            self.cell.floating_ips().await,
        ) {
            (Ok(n), Ok(s), Ok(f)) => (n, s, f),
            _ => {
                // A daemon programmed from a half-read cell would announce a
                // half-truth to the router in front of everything.
                tracing::error!("could not read the cell for the bgp pass");
                pass.failures += 1;
                return;
            }
        };
        let desired = crate::bgp::desired_for(me, &peers, &networks, &subnets, &floating);
        let outcome = {
            let unchanged = self
                .bgp_applied
                .lock()
                .expect("nothing panics holding the applied-config lock")
                .as_ref()
                == Some(&desired);
            if unchanged {
                Ok(())
            } else {
                match speaker.apply(&desired).await {
                    Ok(()) => {
                        pass.actions += 1;
                        *self
                            .bgp_applied
                            .lock()
                            .expect("nothing panics holding the applied-config lock") =
                            Some(desired.clone());
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        let observed = match speaker.observe().await {
            Ok(observed) => observed,
            Err(e) => {
                tracing::warn!(error = %e, "the routing daemon would not say how its sessions are");
                pass.failures += 1;
                return;
            }
        };
        for stored in mine {
            let mut next = (*stored).clone();
            next.status.observed_generation = stored.meta.generation;
            next.status.node = Some(me.to_string());
            let seen = observed.get(&stored.spec.peer);
            next.status.session = seen.map(|o| o.state.clone()).unwrap_or_default();
            next.status.announced = seen.map(|o| o.announced).unwrap_or(0);
            let condition = match (&outcome, seen) {
                (Err(why), _) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "DaemonRefused",
                    why,
                    stored.meta.generation,
                ),
                (Ok(()), Some(o)) if o.state == "Established" => {
                    Condition::ready(stored.meta.generation)
                }
                (Ok(()), Some(o)) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "SessionDown",
                    &format!("the far end answers {}", o.state),
                    stored.meta.generation,
                ),
                (Ok(()), None) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "NotProgrammed",
                    "the daemon does not know this neighbour yet",
                    stored.meta.generation,
                ),
            };
            velstra_cloud_model::meta::set_condition(&mut next.status.conditions, condition);
            self.report(&self.bgp_peers, stored, next, pass).await;
        }
    }

    // ---- ports -----------------------------------------------------------

    /// A port carries no actions of its own — an instance's reconcile programs
    /// it, because a port exists to be plugged into something. What is left is
    /// to say what the datapath actually has, and to clean up a port that is
    /// being deleted on its own.
    async fn port_pass(
        &self,
        stored: &Port,
        taps: &BTreeMap<String, String>,
        in_my_share: bool,
        pass: &mut Pass,
    ) {
        let name = stored.meta.name.to_string();
        let mut outcome = Ok(());
        let mut taps = taps.clone();

        // Not gated on the tap still being there, and that guard was not a
        // cheap early exit — it was the bug. `unprogram` has more to undo than
        // the tap: on the fabric datapath it also removes the port and its
        // security group, which hold an address and a MAC. So a pass that
        // deleted the tap and then failed, or an agent replaced between the two,
        // left the rest in place and never came back for it — because on the
        // next pass there was no tap, so the condition that triggers the
        // teardown was false precisely because the teardown had half happened.
        //
        // `unprogram` is idempotent in both implementations, so asking again is
        // asking once.
        if stored.meta.is_deleting() {
            match self.datapath.unprogram(&name).await {
                Ok(()) => {
                    pass.actions += 1;
                    match self.datapath.observe().await {
                        Ok(fresh) => taps = taps_of(&fresh),
                        Err(e) => outcome = Err(e.to_string()),
                    }
                }
                Err(e) => outcome = Err(e.to_string()),
            }
        }

        let mut next = stored.clone();
        // A port whose guest is nobody of this node's is let go of, not
        // re-claimed: writing `node=<here>` back would keep the true holder
        // refusing to speak for it for ever. The sweep above has already taken
        // the tap; clearing the owner is what lets the node running the guest
        // claim the object on its next pass.
        next.status.node = if in_my_share || stored.meta.is_deleting() {
            Some(self.config.node.clone())
        } else {
            None
        };
        next.status.observed_generation = stored.meta.generation;
        next.status.programmed = taps.contains_key(&name);
        next.status.tap_device = taps.get(&name).cloned();
        let ready = if next.status.programmed {
            Condition::ready(stored.meta.generation)
        } else {
            Condition::new(
                "Ready",
                ConditionStatus::False,
                "NotProgrammed",
                "this node's datapath does not carry the port",
                stored.meta.generation,
            )
        };
        set_condition(&mut next.status.conditions, ready);
        set_condition(
            &mut next.status.conditions,
            host_condition(&outcome, stored.meta.generation),
        );
        // "This node holds nothing of it" — an observation, not an inference
        // from the action list, and the fact the port controller waits for
        // before it drops the release guard. It has to include `outcome`: a
        // teardown that failed part way leaves the tap gone and the fabric
        // still holding the port, and saying "released" on the strength of the
        // missing tap alone would drop the guard over exactly that state.
        let released = outcome.is_ok() && !next.status.programmed;
        set_condition(
            &mut next.status.conditions,
            release_condition(released, stored.meta.is_deleting(), stored.meta.generation),
        );
        if outcome.is_err() {
            pass.failures += 1;
        }

        self.report(&self.ports, stored, next, pass).await;
    }
}

fn is_release(action: &Action) -> bool {
    matches!(action, Action::ReleaseFinalizer { .. })
}

/// The finalizer this node holds, exposed so a controller and the agent name
/// the same string.
pub const RELEASE_FINALIZER: &str = NODE_RELEASE_FINALIZER;

impl Agent {
    /// Whether this node may fetch `image` at all: its signature, if it has
    /// one, verifies under this node's keys; and if the node was told to insist
    /// on signatures, it has one. The sentence names what stopped it, because
    /// "the image never arrived" is how this would otherwise read.
    fn image_may_be_fetched(
        &self,
        name: &str,
        image: &velstra_cloud_model::resources::ImageSpec,
    ) -> Result<(), String> {
        use velstra_cloud_model::images::{SignatureVerdict, judge_signature};
        match judge_signature(
            &image.digest,
            image.signature.as_deref(),
            &self.config.image_signing_keys,
        ) {
            SignatureVerdict::Verified { .. } => Ok(()),
            SignatureVerdict::Unsigned if self.config.require_signed_images => Err(format!(
                "{name} carries no signature and this node fetches signed images only \
                 (--require-signed-images)"
            )),
            SignatureVerdict::Unsigned => Ok(()),
            SignatureVerdict::Refused(why) if self.config.image_signing_keys.is_empty() => {
                Err(format!(
                    "{name} carries a signature and this node has no key to check it under \
                     (--image-signing-key); refusing to fetch what it cannot judge: {why}"
                ))
            }
            SignatureVerdict::Refused(why) => Err(format!("{name} is not fetched: {why}")),
        }
    }
}
