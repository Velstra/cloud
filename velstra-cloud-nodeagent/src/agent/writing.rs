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
    /// **Known gap, and it is not in this crate:** `NodeStatus::owner()`
    /// returns `None`, so [`velstra_cloud_model::access::judge`] refuses every
    /// agent write to a node object — including this one. The code is written
    /// the way it should work and the refusal is reported once, loudly, rather
    /// than worked around; when the model gives a node an owner this starts
    /// working with no change here.
    pub(super) async fn node_pass(&self, mine: &[&Instance], host: &HostState, pass: &mut Pass) {
        let name = format!("nodes/{}", self.config.node);
        let stored = match self.nodes.get(&name).await {
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

        let mut next = stored.clone();
        next.status.observed_generation = stored.meta.generation;
        next.status.capacity = capacity;
        next.status.allocated = allocated;
        next.status.agent_version = self.config.agent_version.clone();
        next.status.last_heartbeat = Timestamp::now();
        // What this machine holds, so that anybody who needs to know which
        // nodes have an image can work it out from these reports rather than
        // from a shared list every node would have to write into.
        next.status.images = host.images.iter().cloned().collect();
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

        if let Err(e) = self.nodes.update(&next, &self.writer).await {
            match e {
                TypedError::Store(StoreError::Conflict { .. }) => pass.conflicts += 1,
                TypedError::Refused(_) => {
                    pass.refused += 1;
                    if !self.warned_about_node.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            node = %self.config.node,
                            "this node may not write its own status; the scheduler is reading \
                             stale capacity and no heartbeat is arriving"
                        );
                    }
                }
                other => {
                    tracing::warn!(error = %other, "could not report this node's status");
                    pass.failures += 1;
                }
            }
        } else {
            pass.reports += 1;
        }
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
        self.guests
            .replace(guests::derive(mine, ports, &subnets, &networks, taps));
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
        crate::reporting::report(store, stored, next, &self.writer, pass).await;
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
        crate::reporting::claim(store, stored, take_ownership, &self.writer, pass).await;
    }
}
