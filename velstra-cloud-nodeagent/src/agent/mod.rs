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
    sync::{Arc, atomic::AtomicBool},
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
    security::{ResolvedRule, SecurityGroupSpec, effective_rules},
};
use velstra_cloud_store::{Store, TypedStore};

use crate::{
    cell::{CellReader, StoreCell},
    guests::GuestRegistry,
    host::{Datapath, HostState, Nic, ProgrammedPort, VmRequest, Vmm},
};

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
}

impl AgentConfig {
    pub fn new(node: &str, region: &str, cell: &str) -> Self {
        Self {
            node: node.to_string(),
            placement: Placement::new(region, cell),
            resync: Duration::from_secs(30),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
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
    pub groups: &'a BTreeMap<String, SecurityGroupSpec>,
    /// Read once per pass and handed down, for the same reason the groups are:
    /// a port's segment is a fact about the cell, and looking it up again per
    /// port would be the same answer fetched many times. It also replaces a
    /// second read this pass used to make when it described guests.
    pub networks: &'a BTreeMap<String, NetworkSpec>,
    /// The registered images, by name — where an image's bytes come from.
    ///
    /// Read once per pass and handed down like the networks above. A node can
    /// verify an image from the digest in its name but cannot invent its
    /// source, so without this the agent could refuse a bad image and never
    /// obtain a good one.
    pub images: &'a BTreeMap<String, ImageSpec>,
}

pub struct Agent {
    config: AgentConfig,
    writer: Writer,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    /// Written, not read: the destination of a migration owns its status and
    /// reports on it. What this node is *told* about migrations comes through
    /// `cell`, which hands it only the ones naming it.
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    /// Everything this node *reads* about the cell, and the only thing that ever
    /// grew with the cell rather than with this node's own work. See
    /// [`crate::cell`] for the two ways it can be answered and why it matters.
    cell: Arc<dyn CellReader>,
    vmm: Arc<dyn Vmm>,
    datapath: Arc<dyn Datapath>,
    guests: GuestRegistry,
    /// An agent that cannot write its own node object says so once. Repeating
    /// it every resync would bury everything else in the journal.
    warned_about_node: AtomicBool,
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
            nodes: TypedStore::new(store.clone(), &cell, "nodes"),
            migrations: TypedStore::new(store, &cell, "migrations"),
            cell: reader,
            config,
            vmm,
            datapath,
            guests: GuestRegistry::new(),
            warned_about_node: AtomicBool::new(false),
        }
    }

    /// The registry the metadata service and the DHCP responder both answer
    /// from. Handed out so all three share one map rather than three that can
    /// differ — a guest leased an address the metadata service does not think
    /// it has is a guest nobody can debug.
    pub fn guests(&self) -> GuestRegistry {
        self.guests.clone()
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

        let mut ticker = tokio::time::interval(self.config.resync);
        ticker.tick().await; // the first tick is immediate, and we just swept
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let woken = tokio::select! {
                _ = &mut shutdown => return,
                _ = ticker.tick() => true,
                woken = wake.recv() => woken.is_some(),
            };
            // A pass is level-triggered, so a burst collapses into one sweep
            // rather than one sweep each.
            while wake.try_recv().is_ok() {}
            if woken {
                self.resync().await;
            }
        }
    }

    /// One full pass over everything this node owns.
    ///
    /// Safe to call as often as anyone likes, and on a converged node it
    /// performs no actions and writes nothing — which is the property that
    /// makes the resync interval a matter of taste rather than of load.
    pub async fn resync(&self) -> Pass {
        let mut pass = Pass::default();

        let host = match self.vmm.observe().await {
            Ok(host) => host,
            Err(e) => {
                // Without a picture of the machine there is nothing honest to
                // do: acting blind is how two copies of a guest happen.
                tracing::error!(error = %e, "could not read this machine; skipping the pass");
                pass.failures += 1;
                return pass;
            }
        };
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
                .map(|g| (g.meta.name.to_string(), g.spec))
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

        let cell = CellView {
            ports: &ports,
            groups: &groups,
            networks: &networks,
            images: &images,
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

        let instances = match self.cell.instances().await {
            Ok(instances) => instances,
            Err(e) => {
                tracing::error!(error = %e, "could not list instances");
                pass.failures += 1;
                return pass;
            }
        };
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
                self.port_pass(port, &taps, &mut pass).await;
            }
        }

        // Receiving comes last, so that on the pass where a guest arrives and
        // is claimed above, the receiver it came through is taken down in the
        // same sweep. A receiver left listening holds a memory reservation on a
        // node that is not running the guest.
        self.destination_pass(&migrations, &taps, &cell, &host, &mut pass)
            .await;

        self.refresh_guests(&mine, &ports, &taps, &mut pass).await;
        self.node_pass(&mine, &host, &mut pass).await;
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
            );

            let actions = reconcile_instance(
                &observed,
                host.images.contains(&stored.spec.image),
                // Not "is there a tap": a port whose group gained a member is
                // carried and out of date, and a check that only asked whether
                // it was present would never notice.
                &stored
                    .spec
                    .ports
                    .iter()
                    .map(|p| match (programmed.get(p.as_str()), ports.get(p)) {
                        (Some(have), Some(port)) => {
                            have.rules == self.rules_for(&port.spec, groups, ports)
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
                match self.perform_instance(action, stored, &taps, cell).await {
                    Ok(()) => pass.actions += 1,
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
        observe_instance(&mut next.status, &host, &name, stored.meta.is_deleting());
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
        groups: &BTreeMap<String, SecurityGroupSpec>,
        ports: &BTreeMap<String, Port>,
    ) -> Vec<ResolvedRule> {
        if spec.security_groups.is_empty() {
            return Vec::new();
        }
        let specs: BTreeMap<String, PortSpec> = ports
            .iter()
            .map(|(name, p)| (name.clone(), p.spec.clone()))
            .collect();
        let effective = effective_rules(spec, groups, &specs);
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
    ) -> Result<(), String> {
        let (ports, groups, networks) = (cell.ports, cell.groups, cell.networks);
        let result = match action {
            // Resolved from the cell, exactly as ProgramPort resolves its port:
            // the decision names what must be present, the agent looks up what
            // it needs to make that true.
            Action::PullImage { digest } => match cell.images.get(digest) {
                Some(image) => self
                    .vmm
                    .pull_image(digest, &image.source_url)
                    .await
                    .map(|_| ()),
                None => Err(crate::host::HostError::failed(format!(
                    "{digest} is not a registered image in this cell, so this \
                     node has nowhere to fetch it from"
                ))),
            },
            Action::CreateDisk {
                instance,
                gib,
                image,
            } => self.vmm.create_disk(instance, *gib, image).await,
            Action::ProgramPort { port } => match ports.get(port) {
                Some(p) => {
                    // Said out loud rather than guessed at: a datapath that
                    // programmed the port without knowing its segment would be
                    // putting a tenant's frames somewhere nobody chose.
                    match networks.get(&p.spec.network) {
                        Some(network) => {
                            let rules = self.rules_for(&p.spec, groups, ports);
                            self.datapath
                                .program(port, &p.spec, network, &rules)
                                .await
                                .map(|_| ())
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
                match self.vm_request(instance, taps, ports) {
                    Ok(request) => self.vmm.start(&request).await,
                    Err(why) => Err(crate::host::HostError::failed(why)),
                }
            }
            Action::StopVm { instance } => self.vmm.stop(instance).await,
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
            image: instance.spec.image.clone(),
            root_disk_gib: instance.spec.root_disk_gib,
            nics: wanted,
        })
    }

    // ---- attachments -----------------------------------------------------

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
            Action::OpenVolume { volume, read_only } => self
                .vmm
                .open_volume(instance, volume, *read_only)
                .await
                .map(|_| ()),
            Action::CloseVolume { volume } => self.vmm.close_volume(instance, volume).await,
            other => Err(crate::host::HostError::failed(format!(
                "{other:?} is not an attachment action"
            ))),
        };
        result.map_err(|e| e.to_string())
    }

    // ---- ports -----------------------------------------------------------

    /// A port carries no actions of its own — an instance's reconcile programs
    /// it, because a port exists to be plugged into something. What is left is
    /// to say what the datapath actually has, and to clean up a port that is
    /// being deleted on its own.
    async fn port_pass(&self, stored: &Port, taps: &BTreeMap<String, String>, pass: &mut Pass) {
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
        next.status.node = Some(self.config.node.clone());
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
