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
        Attachment, AttachmentSpec, AttachmentStatus, Instance, InstanceSpec, InstanceStatus,
        NODE_RELEASE_FINALIZER, NodeSpec, NodeStatus, Port, PortSpec, PortStatus,
    },
};
use velstra_cloud_store::{Store, TypedStore};

use crate::{
    host::{Datapath, HostState, VmRequest, Vmm},
    metadata::MetadataRegistry,
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

pub struct Agent {
    config: AgentConfig,
    writer: Writer,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    vmm: Arc<dyn Vmm>,
    datapath: Arc<dyn Datapath>,
    metadata: MetadataRegistry,
    /// An agent that cannot write its own node object says so once. Repeating
    /// it every resync would bury everything else in the journal.
    warned_about_node: AtomicBool,
}

impl Agent {
    pub fn new(
        store: Arc<dyn Store>,
        config: AgentConfig,
        vmm: Arc<dyn Vmm>,
        datapath: Arc<dyn Datapath>,
    ) -> Self {
        let cell = config.placement.cell.clone();
        Self {
            writer: Writer::agent(&config.node),
            instances: TypedStore::new(store.clone(), &cell, "instances"),
            attachments: TypedStore::new(store.clone(), &cell, "attachments"),
            ports: TypedStore::new(store.clone(), &cell, "ports"),
            nodes: TypedStore::new(store.clone(), &cell, "nodes"),
            migrations: TypedStore::new(store, &cell, "migrations"),
            config,
            vmm,
            datapath,
            metadata: MetadataRegistry::new(),
            warned_about_node: AtomicBool::new(false),
        }
    }

    /// The registry the metadata service answers from. Handed out so the
    /// service and the agent share one map rather than two that can differ.
    pub fn metadata(&self) -> MetadataRegistry {
        self.metadata.clone()
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
        let from = self.instances.revision().await.ok();
        let mut instances = self.instances.watch(from);
        let mut attachments = self.attachments.watch(from);
        let mut ports = self.ports.watch(from);
        let mut migrations = self.migrations.watch(from);

        self.resync().await;

        let mut ticker = tokio::time::interval(self.config.resync);
        ticker.tick().await; // the first tick is immediate, and we just swept
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let mut woken = tokio::select! {
                _ = &mut shutdown => return,
                _ = ticker.tick() => true,
                Some(event) = instances.recv() => self.concerns_me(&event),
                Some(event) = attachments.recv() => self.concerns_me(&event),
                Some(event) = ports.recv() => self.concerns_me(&event),
                Some(event) = migrations.recv() => self.concerns_me(&event),
            };
            // A pass is level-triggered, so a burst of events collapses into
            // one sweep rather than one sweep each.
            woken |= self.drain(&mut instances);
            woken |= self.drain(&mut attachments);
            woken |= self.drain(&mut ports);
            woken |= self.drain(&mut migrations);
            if woken {
                self.resync().await;
            }
        }
    }

    /// Take everything already queued, and say whether any of it was ours.
    fn drain(&self, rx: &mut tokio::sync::mpsc::Receiver<velstra_cloud_store::Event>) -> bool {
        let mut mine = false;
        while let Ok(event) = rx.try_recv() {
            mine |= self.concerns_me(&event);
        }
        mine
    }

    /// Whether a change is about an object this node has anything to do with.
    ///
    /// Read out of the stored JSON rather than by decoding into a resource
    /// type, because the answer is the same field in every one of them and a
    /// node should not have to know the schema of an object that is not its
    /// own. This is the client-side half of a filter that belongs on the
    /// server: see the caveat in the module documentation.
    fn concerns_me(&self, event: &velstra_cloud_store::Event) -> bool {
        let velstra_cloud_store::Event::Put(entry) = event else {
            // A delete carries no object to judge. It is also rare, and a
            // needless pass is cheap; guessing wrong in the other direction
            // would strand an object.
            return true;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&entry.value) else {
            return true;
        };
        let owner = value
            .get("status")
            .and_then(|s| s.get("node"))
            .and_then(|n| n.as_str());
        let spec = value.get("spec");
        let assigned = spec.and_then(|s| s.get("node")).and_then(|n| n.as_str());
        // A migration names two nodes and neither of them is called `node`.
        // Both halves of it are this node's business when they name it: the
        // destination owns the object, and the source has to read it to know
        // where to send.
        let moving = ["to_node", "from_node"].iter().any(|field| {
            spec.and_then(|s| s.get(field)).and_then(|n| n.as_str())
                == Some(self.config.node.as_str())
        });
        owner == Some(self.config.node.as_str())
            || assigned == Some(self.config.node.as_str())
            || moving
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
        let taps = match self.datapath.observe().await {
            Ok(taps) => taps,
            Err(e) => {
                tracing::error!(error = %e, "could not read the datapath; skipping the pass");
                pass.failures += 1;
                return pass;
            }
        };

        let ports = match self.ports.list().await {
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

        let migrations = match self.migrations.list().await {
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
        let moving = self.source_pass(&migrations, &host, &mut pass).await;
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

        let instances = match self.instances.list().await {
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
                    self.instance_pass(instance, &host, &taps, &ports, &moving, &mut pass)
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

        match self.attachments.list().await {
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
            Ok(taps) => taps,
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
        self.destination_pass(&migrations, &taps, &ports, &host, &mut pass)
            .await;

        self.refresh_metadata(&mine, &ports);
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
        taps: &BTreeMap<String, String>,
        ports: &BTreeMap<String, Port>,
        moving: &Moving,
        pass: &mut Pass,
    ) {
        let name = stored.meta.name.to_string();
        let acted_on = stored.meta.generation;
        let mut host = host.clone();
        let mut taps = taps.clone();
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
                &stored
                    .spec
                    .ports
                    .iter()
                    .map(|p| taps.contains_key(p))
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
                match self.perform_instance(action, stored, &taps, ports).await {
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
            taps = match self.datapath.observe().await {
                Ok(taps) => taps,
                Err(e) => {
                    outcome = Err(e.to_string());
                    break;
                }
            };
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

    async fn perform_instance(
        &self,
        action: &Action,
        instance: &Instance,
        taps: &BTreeMap<String, String>,
        ports: &BTreeMap<String, Port>,
    ) -> Result<(), String> {
        let result = match action {
            Action::PullImage { digest } => self.vmm.pull_image(digest).await.map(|_| ()),
            Action::CreateDisk { instance, gib } => self.vmm.create_disk(instance, *gib).await,
            Action::ProgramPort { port } => match ports.get(port) {
                Some(p) => self.datapath.program(port, &p.spec).await.map(|_| ()),
                // The port object has not reached this cell's store yet. Not an
                // error on the machine — a thing to wait for, said out loud on
                // the instance so nobody has to guess which half is late.
                None => Err(crate::host::HostError::failed(format!(
                    "{port} is not in the store yet"
                ))),
            },
            Action::UnprogramPort { port } => self.datapath.unprogram(port).await,
            Action::StartVm { .. } | Action::RestartCrashedVm { .. } => {
                match self.vm_request(instance, taps) {
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
    ) -> Result<VmRequest, String> {
        let mut wanted = Vec::with_capacity(instance.spec.ports.len());
        for port in &instance.spec.ports {
            match taps.get(port) {
                Some(tap) => wanted.push(tap.clone()),
                None => return Err(format!("{port} is not programmed on this node")),
            }
        }
        Ok(VmRequest {
            instance: instance.meta.name.to_string(),
            vcpus: instance.spec.vcpus,
            memory_mib: instance.spec.memory_mib,
            image: instance.spec.image.clone(),
            root_disk_gib: instance.spec.root_disk_gib,
            taps: wanted,
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

        if stored.meta.is_deleting() && taps.contains_key(&name) {
            match self.datapath.unprogram(&name).await {
                Ok(()) => {
                    pass.actions += 1;
                    match self.datapath.observe().await {
                        Ok(fresh) => taps = fresh,
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
