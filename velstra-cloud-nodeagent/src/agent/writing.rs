//! What a node says, and to whom.
//!
//! Two things live here, and they are the same thing seen twice: the status a
//! node writes about the objects it holds, and the status it writes about
//! itself. Both go through the store's ownership gate, and both treat a refusal
//! as information rather than as an error to swallow — a node that is not
//! allowed to report is a node whose objects are quietly going stale, and that
//! has to be visible.

use std::{collections::BTreeMap, sync::atomic::Ordering};

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    meta::{Condition, ConditionStatus, Timestamp, set_condition},
    resources::{Assigned, Capacity, Instance, Observed, Port, Resource},
};
use velstra_cloud_store::{StoreError, TypedStore, typed::TypedError};

use super::{Agent, Pass};
use crate::{guests, host::HostState};

impl Agent {
    // ---- the node's own object ------------------------------------------

    /// Report capacity and a heartbeat on this node's own object.
    ///
    /// `allocated` is counted from the instances this node holds rather than
    /// tracked as a running total, for the same reason quota is counted: a
    /// total that is incremented and decremented drifts, and a count of what
    /// exists cannot.
    ///
    /// A node is its own owner: [`NodeStatus::self_owned`] returns `true`, and
    /// the typed store treats a self-owned object as owned by the id in its own
    /// name (see `velstra-cloud-store` `typed.rs`), so
    /// [`velstra_cloud_model::access::judge`] permits this agent's write to its
    /// own node object. Nothing assigns a hypervisor to a hypervisor.
    ///
    /// How this write is judged depends on the mode. In the direct-store default
    /// the writer identity is **self-declared** and the agent holds a cell
    /// operator token — a trust limit, not a boundary. In `--api` mode the report
    /// goes through [`crate::sink::StatusSink`] as this node's own token, and the
    /// API authenticates it and refuses anything that is not this node's. See
    /// `docs/rest-contract.md`, "Node agents, and the two ways they write".
    pub(super) async fn node_pass(&self, mine: &[&Instance], host: &HostState, pass: &mut Pass) {
        let stored = match self.own_node().await {
            Ok(Some(node)) => node,
            // A node that was never registered is not this agent's to invent.
            Ok(None) => return,
            Err(e) => {
                tracing::error!(error = %e, "could not read this node's own object");
                pass.failures += 1;
                return;
            }
        };
        let capacity = match self.vmm.capacity().await {
            Ok(capacity) => capacity,
            Err(e) => {
                tracing::error!(error = %e, "could not read this machine's capacity");
                pass.failures += 1;
                return;
            }
        };

        let mut allocated = Capacity::default();
        for instance in mine.iter().filter(|i| !i.meta.is_deleting()) {
            allocated.vcpus += instance.spec.vcpus;
            allocated.memory_mib += instance.spec.memory_mib;
            allocated.disk_gib += instance.spec.root_disk_gib;
        }

        // Remembered while this agent can still read anything. The moment
        // fencing matters is the moment it cannot, so a deadline it would have
        // to fetch is a deadline it will never get.
        self.fence_after_s
            .store(stored.spec.fence_after_s, Ordering::Relaxed);

        // The heartbeat moves only when it is old enough to matter.
        //
        // It used to move on every pass — and a pass is woken by watch events,
        // including the event this very write produces. The agent woke on its
        // own echo, ran a pass, wrote a fresh heartbeat, woke again: eight
        // store writes a second on an idle two-node cell, which is the traffic
        // that filled etcd's quota and took the control plane down. Found by
        // watching the store, not the code.
        //
        // Ten seconds is far below anything that reads it: fencing deadlines
        // are minutes, the console prints ages in seconds. When anything
        // *else* in the status changed, the report carries a fresh heartbeat
        // with it for free.
        let heartbeat_due = Timestamp::now()
            .0
            .saturating_sub(stored.status.last_heartbeat.0)
            >= 10_000;

        let mut next = stored.clone();
        next.status.observed_generation = stored.meta.generation;
        next.status.capacity = capacity;
        // Free-memory readings jitter by a few MiB between any two looks at
        // /proc, and on a machine that also runs the control plane they never
        // repeat — so "did anything change" was answered yes on every pass,
        // and the equality skip below never fired. They are telemetry, not
        // facts a controller acts on to the MiB: a movement smaller than this
        // carries the stored figure forward and lets the heartbeat cadence
        // publish the fresh one.
        const MIB_WORTH_REPORTING: u64 = 256;
        if !heartbeat_due {
            let close = |a: u64, b: u64| a.abs_diff(b) < MIB_WORTH_REPORTING;
            if close(
                next.status.capacity.memory_mib,
                stored.status.capacity.memory_mib,
            ) {
                next.status.capacity.memory_mib = stored.status.capacity.memory_mib;
            }
            if next.status.capacity.numa_free_mib.len()
                == stored.status.capacity.numa_free_mib.len()
                && next
                    .status
                    .capacity
                    .numa_free_mib
                    .iter()
                    .zip(&stored.status.capacity.numa_free_mib)
                    .all(|(a, b)| close(*a, *b))
            {
                next.status.capacity.numa_free_mib = stored.status.capacity.numa_free_mib.clone();
            }
        }
        next.status.allocated = allocated;
        next.status.agent_version = self.config.agent_version.clone();
        next.status.console_endpoint = self.config.console_endpoint.clone();
        // Reported like everything else about this machine, so a controller
        // deciding whether a guest can move reads a fact rather than a
        // configuration file it has no access to.
        next.status.shared_state = self.config.shared_state;
        next.status.last_heartbeat = if heartbeat_due {
            Timestamp::now()
        } else {
            stored.status.last_heartbeat
        };
        // What this machine holds, so that anybody who needs to know which
        // nodes have an image can work it out from these reports rather than
        // from a shared list every node would have to write into.
        next.status.images = host.images.iter().cloned().collect();
        next.status.vmm = self.vmm.vmm_name().to_string();
        next.status.fetching = host.fetching.iter().cloned().collect();
        // The disks this machine has, and what each is doing. Reported for the
        // same reason the images are: nobody else can see them, and the console
        // offers them for a Ceph OSD from this list. Which of them may be
        // *chosen* is `ceph::may_consume`, never this list on its own — the list
        // says what is there and the rule says what is safe.
        next.status.devices = host.devices.clone();
        next.status.ceph = host.ceph.clone();
        // What this machine's processor is, and what it presents. The
        // baseline was already applied by `Agent::present_baseline`, in one
        // place, so this and every guest's recorded CPU cannot disagree.
        next.status.cpu = host.cpu.clone();
        // The hardware this machine has, with this node's guests already
        // marked on it by `Agent::mark_held_devices`.
        next.status.pci_devices = host.pci_devices.clone();
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::True,
                "Ready",
                "the agent is running and answering",
                stored.meta.generation,
            ),
        );

        // Nothing changed, nothing written — through either door. The direct
        // store path had this via `reporting::report`; the sink path posted
        // every pass regardless, and an unconditional write per pass is half
        // of the echo loop the heartbeat note above describes.
        if next.status == stored.status {
            return;
        }
        // `--api`: the report goes through the API as this node's own token; the
        // once-only warning below is the same one, moved onto the sink's refusal.
        if let Some(sink) = &self.sink {
            use crate::sink::SinkOutcome;
            let value = serde_json::to_value(&next).expect("a node always serialises");
            match sink.write_status("nodes", &value, &self.writer).await {
                SinkOutcome::Wrote => {
                    pass.reports += 1;
                    self.heard();
                }
                SinkOutcome::Conflict => pass.conflicts += 1,
                SinkOutcome::Refused(_) => {
                    pass.refused += 1;
                    self.warn_about_node_once();
                }
                SinkOutcome::Failed(why) => {
                    tracing::warn!(error = %why, "could not report this node's status");
                    pass.failures += 1;
                }
            }
            return;
        }
        if let Err(e) = self.nodes.update(&next, &self.writer).await {
            match e {
                TypedError::Store(StoreError::Conflict { .. }) => pass.conflicts += 1,
                TypedError::Refused(_) => {
                    pass.refused += 1;
                    self.warn_about_node_once();
                }
                other => {
                    tracing::warn!(error = %other, "could not report this node's status");
                    pass.failures += 1;
                }
            }
        } else {
            pass.reports += 1;
            // The watchdog's only input. Set where the write actually
            // succeeded rather than where it was attempted: an agent that is
            // shouting into a dead store has *not* reported, and treating the
            // attempt as success is how a partitioned node keeps its guests
            // running past the moment somebody else is told they stopped.
            self.heard();
        }
    }

    /// Note that this agent managed to report.
    pub(super) fn heard(&self) {
        self.last_report.store(
            velstra_cloud_model::meta::Timestamp::now().0,
            Ordering::Relaxed,
        );
    }

    /// Warn, once per process, that this node cannot write its own status — a
    /// disagreement about who this node is that would otherwise repeat every
    /// resync and bury everything else.
    fn warn_about_node_once(&self) {
        if !self.warned_about_node.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                node = %self.config.node,
                "this node may not write its own status; the scheduler is reading \
                 stale capacity and no heartbeat is arriving"
            );
        }
    }

    /// This node's own object, read from wherever this agent reads the cell.
    ///
    /// Through the store directly in the default mode, and through the API in
    /// `--api` mode — where a keyed store read would hit an empty local store and
    /// this node would look unregistered to itself. The API hands a node the
    /// whole node list (it reads it for Ceph anyway), so its own object is in
    /// there; a linear scan for one's own name is cheap and happens once a pass.
    pub(super) async fn own_node(
        &self,
    ) -> crate::host::Result<Option<velstra_cloud_model::resources::Node>> {
        if self.sink.is_some() {
            // Its own object, by name. Listing the cell to find one row in it is
            // a read of every machine, on every pass, by every machine — and it
            // is the *wrong* read as well as an expensive one: what this agent
            // is entitled to is its own node, and asking for the collection made
            // the entitlement depend on a collection-wide rule.
            return self.cell.node(&self.config.node).await;
        }
        let name = format!("nodes/{}", self.config.node);
        self.nodes
            .get(&name)
            .await
            .map_err(|e| crate::host::HostError::failed(e.to_string()))
    }

    // ---- what this node tells its guests ----------------------------------

    /// Rebuild what the metadata service and the DHCP responder answer from.
    ///
    /// Built fresh from the objects this node holds, so an address that moved
    /// away stops resolving here on the same pass. An entry that outlived its
    /// guest would hand the next tenant of that address somebody else's keys.
    ///
    /// A subnet or network this node could not read costs a guest its gateway
    /// and its mask for one pass, and the refresh happens anyway rather than
    /// leaving the previous answer standing. That is the deliberate order of
    /// the two risks: an incomplete configuration is visible and self-repairing,
    /// and a stale *identity* is neither — it is somebody else's keys handed to
    /// whoever holds a re-used address.
    pub(super) async fn refresh_guests(
        &self,
        mine: &[&Instance],
        ports: &BTreeMap<String, Port>,
        taps: &BTreeMap<String, String>,
        pass: &mut Pass,
    ) {
        let subnets = self.shared(self.cell.subnets().await, "subnets", pass);
        let networks = self.shared(self.cell.networks().await, "networks", pass);
        // The public addresses this node's ports hold. A cell that hands out
        // none reads an empty list and pays a list call; one that does needs
        // them here, because a routed address is configured *by the guest* and
        // this is where a guest is told what it has.
        let public = match self.cell.floating_ips().await {
            Ok(floating) => guests::public_addresses(&floating),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the cell's public addresses");
                pass.failures += 1;
                Default::default()
            }
        };
        self.guests.replace(guests::derive(
            mine, ports, &subnets, &networks, taps, &public,
        ));
    }

    /// A collection this node only reads, keyed by name. An unreadable one is a
    /// counted failure and an empty map — see [`Agent::refresh_guests`] for why
    /// that is better here than keeping what was read last time.
    fn shared<S, T>(
        &self,
        read: crate::host::Result<Vec<Resource<S, T>>>,
        what: &str,
        pass: &mut Pass,
    ) -> BTreeMap<String, Resource<S, T>> {
        match read {
            Ok(objects) => objects
                .into_iter()
                .map(|o| (o.meta.name.to_string(), o))
                .collect(),
            Err(e) => {
                tracing::error!(error = %e, "could not list {what}; guests will be told less");
                pass.failures += 1;
                BTreeMap::new()
            }
        }
    }

    // ---- writing ---------------------------------------------------------

    /// Report the observed status. See [`crate::reporting::report`].
    ///
    /// In `--api` mode the write goes through the sink — the API, as this node's
    /// own token — instead of the store; the two are the same report with the
    /// ownership rule enforced in a different place. An unchanged status writes
    /// nothing either way, which is what keeps a converged agent quiet.
    pub(super) async fn report<S, T>(
        &self,
        store: &TypedStore<S, T>,
        stored: &Resource<S, T>,
        next: Resource<S, T>,
        pass: &mut Pass,
    ) where
        S: Serialize + DeserializeOwned + PartialEq + Assigned + Send + Sync,
        T: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync,
    {
        if self.sink.is_some() {
            if next.status == stored.status {
                return;
            }
            self.write_through_sink(store.kind(), &next, pass).await;
            return;
        }
        crate::reporting::report(store, None, stored, next, &self.writer, pass).await;
    }

    /// Say "this is mine now". See [`crate::reporting::claim`].
    pub(super) async fn claim<S, T>(
        &self,
        store: &TypedStore<S, T>,
        stored: &Resource<S, T>,
        take_ownership: impl FnOnce(&mut T),
        pass: &mut Pass,
    ) where
        S: Serialize + DeserializeOwned + PartialEq + Clone + Assigned + Send + Sync,
        T: Serialize + DeserializeOwned + PartialEq + Observed + Clone + Send + Sync,
    {
        if self.sink.is_some() {
            let mut next = stored.clone();
            take_ownership(&mut next.status);
            self.write_through_sink(store.kind(), &next, pass).await;
            return;
        }
        crate::reporting::claim(store, None, stored, take_ownership, &self.writer, pass).await;
    }

    /// Send one report through the sink and count what the far end made of it.
    ///
    /// The mapping from [`crate::sink::SinkOutcome`] to the pass counters is the
    /// same one [`crate::reporting::report`] applies to a direct-store write, so
    /// a `--api` agent and a direct one report the same shape of pass — a refusal
    /// is a refusal whichever wrote it.
    pub(super) async fn write_through_sink<S, T>(
        &self,
        kind: &str,
        next: &Resource<S, T>,
        pass: &mut Pass,
    ) where
        S: Serialize,
        T: Serialize,
    {
        use crate::sink::SinkOutcome;
        let Some(sink) = &self.sink else {
            return;
        };
        let value = serde_json::to_value(next).expect("a resource always serialises");
        match sink.write_status(kind, &value, &self.writer).await {
            SinkOutcome::Wrote => pass.reports += 1,
            SinkOutcome::Conflict => pass.conflicts += 1,
            SinkOutcome::Refused(why) => {
                tracing::warn!(%kind, %why, "the API refused this agent's report");
                pass.refused += 1;
            }
            SinkOutcome::Failed(why) => {
                tracing::warn!(%kind, %why, "could not report status through the API");
                pass.failures += 1;
            }
        }
    }
}
