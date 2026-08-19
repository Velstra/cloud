//! Moving a guest, from a node's side of it.
//!
//! A node is on exactly one side of any migration and the two sides do
//! different things, so this file is two functions and not one:
//!
//! * [`Agent::destination_pass`] runs where `spec.to_node` is this node. The
//!   destination **owns the migration's status** — it is the party with
//!   something to report about the object — so this is where `receiver_url`,
//!   `receiver_ready` and `transferred_mib` are written, and they are written
//!   from what the machine says right now rather than from what was started.
//! * [`Agent::source_pass`] runs where `spec.from_node` is this node. The source
//!   **never writes the migration's status**; it owns the *instance's*, which is
//!   the only thing it has to say — and the one moment it says something new is
//!   when the guest is no longer here.
//!
//! The order is not ours to choose. Both hypervisors require the receiving side
//! to be listening before the sending side may send, which is why the
//! destination acts first and why [`reconcile_source`] refuses to send on a URL
//! that is merely *published* rather than confirmed listening.
//!
//! Three things live here rather than in the model, because all three are
//! observations about this machine and the model is pure:
//!
//! 1. A `Send` is skipped while a transfer of that guest is already under way.
//!    `reconcile_source` asks for a send on every pass until the guest is gone,
//!    which is right — the ask does not change — but obeying it twice would put
//!    a second `ch-remote` on top of the first while a large guest copies.
//! 2. A `Cancel` is skipped when there is no transfer to cancel, so a migration
//!    that has been abandoned stops costing anything on the very next pass.
//! 3. What "the guest is not here any more" *means*. It is the same observation
//!    whether the guest left, whether its VMM died, or whether it was never
//!    started here, and the source has nothing else to go on. The three are told
//!    apart by what this node last reported on the instance — which lives on the
//!    object rather than in this process, so a restarted agent reads the same
//!    answer — and where they cannot be told apart, this node does the thing
//!    that cannot produce two copies of a live guest: it starts nothing while a
//!    migration of it is open, and says so on the instance.

use std::collections::{BTreeMap, BTreeSet};

use velstra_cloud_model::{
    meta::set_condition,
    migration::{
        DestinationAction, Migration, MigrationMode, SourceAction, reconcile_destination,
        reconcile_source,
    },
    resources::{Instance, InstanceState},
};

use super::{Agent, Ownership, Pass, status::host_condition};
use crate::host::{HostState, Transfer};

/// What this node did as the source of a migration, for the instance pass to
/// finish saying.
///
/// The source cannot report on the migration object at all, so everything it
/// has to say goes on the instance — including why a transfer would not start.
/// That is the object an operator is looking at anyway.
#[derive(Debug, Default)]
pub(super) struct Moving {
    /// Instances this node no longer holds because they have been moved away.
    ///
    /// The one signal that completes a handover, and the reason it is a set
    /// computed here rather than a flag anywhere: the instance pass must not
    /// act on these — not start them, and above all not claim them back once
    /// the status says nobody owns them — it must only report, once, that this
    /// node has let go.
    pub released: BTreeSet<String>,
    /// Instances that are not on this machine while a migration of them is
    /// open, and that this node must therefore leave alone.
    ///
    /// This is the safe half of an ambiguity the source cannot resolve on its
    /// own: "the guest is not here" is the same observation whether it left or
    /// whether its VMM died. Starting it would be the more expensive mistake by
    /// far — a second copy of a guest that may well be running on the
    /// destination — so while a migration of it is open, this node starts
    /// nothing and says why on the object.
    pub stalled: BTreeSet<String>,
    /// Why a transfer could not be started, per instance.
    pub trouble: BTreeMap<String, String>,
}

impl Agent {
    // ---- the source ------------------------------------------------------

    /// Send the guests this node is being asked to give up.
    ///
    /// Runs *before* the instance pass, and that order matters twice over: a
    /// successful send takes the guest off this machine, and an instance pass
    /// that ran first would have seen a running guest and reported it as still
    /// here; one that ran after without knowing about the migration would see
    /// a missing guest and start it again — a second copy of a guest that is
    /// now running somewhere else.
    /// Leave alone every instance a transfer is bringing *here*.
    ///
    /// The mirror of [`Moving::stalled`] on the receiving side, and it was
    /// missing. Once the source has let go, the destination claims the instance
    /// — and the instance pass then sees a guest that should be running and is
    /// not, and starts one. On one machine that collides with the outgoing VMM's
    /// unit name and fails loudly, which is how this was found; on two machines
    /// it is a **second copy of a live guest writing the same disk**, which is
    /// the worst thing this platform could do.
    ///
    /// The guard lifts by itself in both directions: once the guest is here,
    /// `host.vms` has it and nothing is stalled; once the migration is
    /// abandoned, it is deleting and no longer holds anything back.
    pub(super) fn freeze_arrivals(
        &self,
        migrations: &[Migration],
        host: &HostState,
        moving: &mut Moving,
    ) {
        for migration in migrations {
            if migration.spec.to_node != self.config.node || migration.meta.is_deleting() {
                continue;
            }
            let name = migration.spec.instance.clone();
            if host.vms.contains_key(&name) {
                // It has arrived. This node runs it like any other guest.
                continue;
            }
            moving.trouble.insert(
                name.clone(),
                "a transfer is bringing this guest to this node; it will not be started here \
                 until that transfer lands or the migration is abandoned"
                    .to_string(),
            );
            moving.stalled.insert(name);
        }
    }

    pub(super) async fn source_pass(
        &self,
        migrations: &[Migration],
        host: &HostState,
        pass: &mut Pass,
    ) -> Moving {
        let mut moving = Moving::default();
        for migration in migrations {
            if migration.spec.from_node != self.config.node
                || migration.spec.to_node == self.config.node
            {
                continue;
            }
            let name = migration.spec.instance.clone();
            let here = host.vms.contains_key(&name);
            let running_here = host
                .vms
                .get(&name)
                .map(|vm| vm.state == InstanceState::Running)
                .unwrap_or(false);

            // A guest this node reported as running, that is now not running
            // here, is one this node may not start again while a transfer of it
            // is open — **including when it is still here but stopped**, which
            // is exactly what a handed-over guest looks like on the source:
            // Cloud Hypervisor leaves the VMM up with the machine shut down.
            // Read as "wanted Running, reports Stopped", the instance pass
            // starts a fresh guest from the same disk that is by then running on
            // the destination.
            //
            // **While a transfer of this guest is open, this node does not start
            // it.** Not when the guest is gone, and not when it is still here but
            // stopped — which is what a handed-over guest looks like on the
            // source, and what a VMM that exited leaves behind.
            //
            // The invariant that makes this safe rather than merely cautious is
            // `may_migrate`: a migration only exists for an instance that was
            // *running*. So an open migration from this node is proof the guest
            // ran here, and anything other than "running here now" means it has
            // either left or died — and starting it in either case risks a second
            // copy of a guest that is by then on the destination.
            //
            // Every narrower version of this was tried and each failed on a real
            // hypervisor: keyed on the store's reported state, the source's own
            // honest "Stopped" erased the signal; keyed on the VMM still being
            // present, a VMM that exited after the send slipped through.
            if !migration.meta.is_deleting() && !running_here {
                moving.stalled.insert(name.clone());
            }

            for action in reconcile_source(migration, here) {
                match self.perform_source(&action, migration, host).await {
                    Ok(acted) => pass.actions += usize::from(acted),
                    Err(why) => {
                        // Counted by the instance pass, which is also where it
                        // is reported: this node may not write the migration's
                        // status, and a failure nobody can read is a failure
                        // nobody fixes.
                        tracing::warn!(instance = %name, %why, "this guest could not be moved");
                        moving.trouble.insert(name.clone(), why);
                    }
                }
            }

            // Read the machine again: a send that landed took the guest with
            // it, and the whole handover hangs on noticing that in this pass
            // rather than the next one.
            // "Still here" means **still running here**, not "there is still a
            // VMM". Cloud Hypervisor's source VMM does not exit when a transfer
            // completes: it stays up holding a stopped machine. Read as presence,
            // the source never noticed the handover, never reported letting go,
            // and so `spec.node` never moved and no migration could finish — even
            // with the guest already running on the destination.
            let still_here = match self.vmm.observe().await {
                Ok(fresh) => fresh
                    .vms
                    .get(&name)
                    .map(|vm| vm.state == InstanceState::Running)
                    .unwrap_or(false),
                Err(e) => {
                    tracing::error!(error = %e, "could not re-read this machine after sending");
                    pass.failures += 1;
                    true
                }
            };
            if still_here || migration.meta.is_deleting() {
                continue;
            }

            // The guest is not on this machine. What that *means* depends on
            // what this node last said about it, which is on the object rather
            // than in this process — and is the difference between a guest that
            // has been handed over and one that was never started here.
            let stored = self.instances.get(&name).await.ok().flatten();
            let owner = stored.as_ref().and_then(|i| i.status.node.clone());

            match owner.as_deref() {
                // This node has already let go. There is nothing left to say —
                // and, more to the point, nothing left to claim back: the
                // instance is on its way to the destination and a node that
                // picked it up again here would be racing the guest it just
                // sent away.
                None => {
                    moving.released.insert(name);
                    continue;
                }
                // Still ours, and the guest is not running here. That is all this
                // node needs, and reading the *reported state* as well was what
                // stopped it: the pass that notices the guest is gone also reports
                // it, and the next pass then read its own honest report as "it was
                // never running here" and let go of nothing, for ever.
                //
                // `may_migrate` is what makes ownership enough — a migration only
                // exists for an instance that was running — so an open migration
                // plus "ours" plus "not running here" is a guest that has left or
                // died, and both want the same thing from this node: let go, and
                // do not start it again.
                Some(node) if node == self.config.node => {}
                // It was not running here in the first place, so nothing can
                // have been handed over. An ordinary missing guest, and the
                // instance pass deals with it exactly as it always does.
                _ => continue,
            }

            // Not gated on `receiver_ready`. It reads as "is the far end ready to
            // receive", and a transfer that has *completed* turns it back off —
            // the receiver is not listening any more, it is running the guest. So
            // the fast case, where the guest has arrived before anybody looked,
            // is precisely the case the gate refused, and the handover could
            // never finish. What is left is the ambiguity the comment below
            // already names, and it is narrowed the same way.
            {
                // Gone from here while something was listening for it there.
                // That is what a completed transfer looks like from this side,
                // and it is the whole of what "done" means to a source: there
                // is no flag to set and nothing to remember.
                //
                // It is not *proof*. A VMM that died while a receiver happened
                // to be listening looks the same, and the source cannot tell
                // the two apart — the only fact it has is that the guest is not
                // here. The model narrows it as far as it can be narrowed
                // without asking a node about another node: `may_migrate`
                // refuses a migration of anything that is not already running,
                // so this cannot be a guest that never started.
                moving.released.insert(name);
            }
        }
        moving
    }

    /// Do one thing, and say whether anything was done.
    ///
    /// The distinction is the whole reason for the `bool`: an ask that is
    /// already satisfied costs nothing and must not be *counted* as an action,
    /// or an abandoned migration would report work on every pass forever and a
    /// settled node would never be quiet.
    async fn perform_source(
        &self,
        action: &SourceAction,
        migration: &Migration,
        host: &HostState,
    ) -> Result<bool, String> {
        match action {
            SourceAction::Send {
                instance,
                url,
                mode,
                downtime_ms,
                timeout_s,
                connections,
            } => {
                if host.sending.contains(instance) {
                    // Already going. The ask has not changed and neither has
                    // the answer; a second `send` would be a second transfer of
                    // one guest.
                    return Ok(false);
                }
                if *mode == MigrationMode::Reboot {
                    // A reboot migration is a stop here and a start there, not
                    // a transfer. Nothing in this build performs one, and
                    // saying so is better than sending a guest to a hypervisor
                    // that will refuse it after the memory has been copied.
                    return Err(
                        "a reboot migration is not a transfer, and this node cannot perform one"
                            .to_string(),
                    );
                }
                self.vmm
                    .send(&Transfer {
                        instance: instance.clone(),
                        url: url.clone(),
                        mode: *mode,
                        downtime_ms: *downtime_ms,
                        timeout_s: *timeout_s,
                        connections: *connections,
                    })
                    .await
                    .map(|()| true)
                    .map_err(|e| e.to_string())
            }
            SourceAction::Cancel { instance } => {
                if !host.sending.contains(instance) {
                    // Nothing is in flight. Cancelling again every pass would
                    // make an abandoned migration a permanent cost.
                    return Ok(false);
                }
                tracing::info!(%instance, migration = %migration.meta.name, "abandoning a transfer");
                self.vmm
                    .cancel_send(instance)
                    .await
                    .map(|()| true)
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// Say that this node no longer has the guest.
    ///
    /// This is step 3 of the whole dance and the only write in it that changes
    /// who owns an instance. Nothing else here touches the machine: the guest
    /// is running on the destination, and every action an instance pass would
    /// otherwise take — starting it, claiming it back — would be a second copy
    /// of it.
    pub(super) async fn release_instance(&self, stored: &Instance, pass: &mut Pass) {
        let mut next = stored.clone();
        // The one moment ownership is given up. A controller sees this and
        // moves `spec.node`; the destination claims it after that, and not one
        // step earlier.
        next.status.node = None;
        // `Unknown` rather than `Stopped`, because this node has stopped
        // nothing. It is the same word the tear-down path uses for the same
        // situation: this node holds nothing of it and cannot see it.
        next.status.state = InstanceState::Unknown;
        next.status.vmm_pid = None;
        next.status.started_at = None;
        next.status.addresses = Vec::new();
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            host_condition(&Ok(()), stored.meta.generation),
        );
        self.report(&self.instances, stored, next, pass).await;
    }

    // ---- the destination -------------------------------------------------

    /// Take delivery of the guests this node is being asked to accept.
    ///
    /// Runs after the instance pass, so that on the pass where the guest
    /// arrives and is claimed here, the receiver it arrived through is torn
    /// down in the same sweep rather than the next one.
    pub(super) async fn destination_pass(
        &self,
        migrations: &[Migration],
        taps: &BTreeMap<String, String>,
        cell: &super::CellView<'_>,
        seen: &HostState,
        pass: &mut Pass,
    ) {
        let mine: Vec<&Migration> = migrations
            .iter()
            .filter(|m| m.spec.to_node == self.config.node)
            .collect();

        // A receiver with no migration behind it belongs to nobody. It happens
        // whenever a migration is abandoned — the object is deleted outright, so
        // there is no deleting object left for the loop below to act on, and
        // nothing will ever arrive through it. Left alone it holds the guest's
        // whole memory reserved on a machine that will never run it.
        //
        // The fix is not a finalizer that makes deletion wait on this agent. It
        // is the discipline the rest of the crate already follows: look at the
        // machine, compare it with the objects, and close the gap. That also
        // covers the case a finalizer could not — the migration deleted while
        // this agent was down.
        for instance in seen
            .receivers
            .keys()
            .filter(|i| !mine.iter().any(|m| &&m.spec.instance == i))
            .cloned()
            .collect::<Vec<_>>()
        {
            tracing::info!(
                %instance,
                "tearing down a receiver no migration asks for any more"
            );
            match self.vmm.tear_down_receiver(&instance).await {
                Ok(()) => pass.actions += 1,
                Err(e) => {
                    tracing::error!(error = %e, %instance, "could not take down an orphaned receiver");
                    pass.failures += 1;
                }
            }
        }

        if mine.is_empty() {
            return;
        }
        // The instance loop has been starting and stopping guests since this
        // pass began, so the picture it started with is out of date. One fresh
        // read for the whole destination half.
        let host = match self.vmm.observe().await {
            Ok(host) => host,
            Err(e) => {
                tracing::error!(error = %e, "could not re-read this machine before receiving");
                pass.failures += 1;
                return;
            }
        };

        for migration in mine {
            match self.ownership(
                migration.status.node.as_deref(),
                Some(migration.spec.to_node.as_str()),
            ) {
                Ownership::Mine => {}
                Ownership::Claim => {
                    let me = self.config.node.clone();
                    // Nothing on the machine happens first. A receiver started
                    // before the cell knows which node is running this
                    // migration is a memory reservation nobody can account for.
                    self.claim(
                        &self.migrations,
                        migration,
                        |status| status.node = Some(me),
                        pass,
                    )
                    .await;
                    continue;
                }
                Ownership::Skip => continue,
            }

            let name = migration.spec.instance.clone();
            let instance = match self.instances.get(&name).await {
                Ok(instance) => instance,
                Err(e) => {
                    tracing::error!(error = %e, instance = %name, "could not read the instance being moved here");
                    pass.failures += 1;
                    continue;
                }
            };
            let actions = reconcile_destination(
                migration,
                instance.as_ref(),
                host.receivers.contains_key(&name),
                // Read from this machine. See `reconcile_destination`: the
                // instance's `status.node` is this node's own claim, not an
                // answer to "is the guest here".
                host.vms.contains_key(&name),
            );

            let mut outcome = Ok(());
            for action in actions {
                let result = match action {
                    DestinationAction::PrepareReceiver { instance: _, mode } => {
                        self.prepare_to_receive(&name, instance.as_ref(), mode, &host, taps, cell)
                            .await
                    }
                    DestinationAction::TearDownReceiver { instance } => self
                        .vmm
                        .tear_down_receiver(&instance)
                        .await
                        .map(|()| true)
                        .map_err(|e| e.to_string()),
                };
                match result {
                    Ok(acted) => pass.actions += usize::from(acted),
                    Err(why) => {
                        outcome = Err(why);
                        break;
                    }
                }
            }

            // What is reported is what the machine says after all of that —
            // never what `prepare_receiver` returned. The difference matters on
            // the pass where a receiver was started and died again before this
            // line: the URL would be published for something that is not there,
            // and the source would send into nothing.
            let fresh = match self.vmm.observe().await {
                Ok(fresh) => fresh,
                Err(e) => {
                    tracing::error!(error = %e, "could not re-read this machine after preparing to receive");
                    pass.failures += 1;
                    continue;
                }
            };
            let receiver = fresh.receivers.get(&name);

            let mut next = migration.clone();
            next.status.node = Some(self.config.node.clone());
            next.status.observed_generation = migration.meta.generation;
            next.status.receiver_url = receiver.map(|r| r.url.clone());
            next.status.receiver_ready = receiver.is_some();
            // Zero when nothing is listening any more, because that is what
            // this machine can see. Keeping the last number would be a memory,
            // and a memory is the one thing a status may not be — whether the
            // guest arrived is computed from where it runs, not from a count
            // that was left behind.
            next.status.transferred_mib = receiver.map(|r| r.received_mib).unwrap_or(0);
            // Only what this node can say about itself goes in the conditions.
            // Whether the guest has moved is computed by whoever reads the two
            // objects; a node writing it would be writing about a machine it is
            // not.
            set_condition(
                &mut next.status.conditions,
                host_condition(&outcome, migration.meta.generation),
            );
            if outcome.is_err() {
                pass.failures += 1;
            }
            self.report(&self.migrations, migration, next, pass).await;
        }
    }

    /// Everything that has to be true here before a guest can arrive.
    ///
    /// A receiver on its own is not enough. The guest resumes into its own root
    /// disk and its own taps, by the names its configuration carries, and every
    /// one of them has to already exist on this machine — the transfer carries
    /// memory and device state, not storage and not network. Each step is
    /// skipped when the machine already has it, so a second pass over a
    /// prepared destination does nothing.
    async fn prepare_to_receive(
        &self,
        name: &str,
        instance: Option<&Instance>,
        mode: MigrationMode,
        host: &HostState,
        taps: &BTreeMap<String, String>,
        cell: &super::CellView<'_>,
    ) -> Result<bool, String> {
        let (ports, groups) = (cell.ports, cell.groups);
        let Some(instance) = instance else {
            // Not an error on the machine — a thing to wait for, said out loud
            // so nobody has to guess which half is late.
            return Err(format!("{name} is not in this cell's store yet"));
        };
        if host.vms.contains_key(name) {
            // It is already here: the transfer landed and this node has not yet
            // been given the instance to claim. There is nothing to receive,
            // and a second receiver for a guest that has arrived is what the
            // VMM would refuse anyway.
            return Ok(false);
        }

        if !host.images.contains(&instance.spec.image) {
            // The destination fetches from the registered source, the same way
            // an ordinary pass does. A guest cannot arrive onto a node that
            // cannot obtain its image, and finding that out here — before a
            // receiver is opened — is what keeps a half-prepared destination
            // from waiting for a transfer that will never be usable.
            let source = cell
                .images
                .get(&instance.spec.image)
                .map(|i| i.source_url.clone())
                .ok_or_else(|| {
                    format!(
                        "{} is not a registered image in this cell, so this node \
                         has nowhere to fetch it from",
                        instance.spec.image
                    )
                })?;
            self.vmm
                .pull_image(&instance.spec.image, &source)
                .await
                .map_err(|e| e.to_string())?;
        }
        if !host.disks.contains(name) {
            self.vmm
                .create_disk(name, instance.spec.root_disk_gib, &instance.spec.image)
                .await
                .map_err(|e| e.to_string())?;
        }

        let mut taps = taps.clone();
        for port in &instance.spec.ports {
            if taps.contains_key(port) {
                continue;
            }
            let Some(stored) = ports.get(port) else {
                return Err(format!("{port} is not in the store yet"));
            };
            // The destination resolves the groups itself rather than being told
            // what the source had: membership is a property of the cell, not of
            // the node the guest is leaving.
            let Some(network) = cell.networks.get(&stored.spec.network) else {
                return Err(format!(
                    "{port} is on {}, which is not in the store yet",
                    stored.spec.network
                ));
            };
            let rules = self.rules_for(&stored.spec, groups, ports);
            let tap = self
                .datapath
                .program(port, &stored.spec, network, &rules)
                .await
                .map_err(|e| e.to_string())?;
            taps.insert(port.clone(), tap);
        }

        let request = self.vm_request(instance, &taps, ports)?;
        self.vmm
            .prepare_receiver(&request, mode)
            .await
            .map(|url| {
                tracing::info!(instance = %name, %url, "listening for a guest");
                true
            })
            .map_err(|e| e.to_string())
    }
}
