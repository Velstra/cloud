//! What a node says, and to whom.
//!
//! Two things live here, and they are the same thing seen twice: the status a
//! node writes about the objects it holds, and the status it writes about
//! itself. Both go through the store's ownership gate, and both treat a refusal
//! as information rather than as an error to swallow — a node that is not
//! allowed to report is a node whose objects are quietly going stale, and that
//! has to be visible.

use std::{collections::BTreeMap, net::IpAddr, sync::atomic::Ordering};

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    meta::{Condition, ConditionStatus, Timestamp, set_condition},
    resources::{Assigned, Capacity, Instance, Observed, Port, Resource},
};
use velstra_cloud_store::{StoreError, TypedStore, typed::TypedError};

use super::{Agent, Pass};
use crate::{
    host::HostState,
    metadata::{InstanceMetadata, address_of},
};

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

    // ---- metadata --------------------------------------------------------

    /// Rebuild the address-to-guest map the metadata service answers from.
    ///
    /// Built fresh from the objects this node holds, so an address that moved
    /// away stops resolving here on the same pass. An entry that outlived its
    /// guest would hand the next tenant of that address somebody else's keys.
    pub(super) fn refresh_metadata(&self, mine: &[&Instance], ports: &BTreeMap<String, Port>) {
        let mut by_address: BTreeMap<IpAddr, InstanceMetadata> = BTreeMap::new();
        for instance in mine.iter().filter(|i| !i.meta.is_deleting()) {
            let name = instance.meta.name.to_string();
            let meta = InstanceMetadata {
                instance_id: name.clone(),
                hostname: instance.meta.name.id().to_string(),
                ssh_keys: instance.spec.ssh_keys.clone(),
                user_data: instance.spec.user_data.clone(),
            };
            for port in instance.spec.ports.iter().filter_map(|p| ports.get(p)) {
                let Some(address) = port.spec.address.as_deref().and_then(address_of) else {
                    continue;
                };
                if let Some(other) = by_address.get(&address) {
                    // Two guests on one address is a datapath that would give
                    // the second one the first one's identity. Refuse to answer
                    // for either rather than pick.
                    tracing::error!(
                        %address, first = %other.instance_id, second = %name,
                        "two instances claim one address; answering for neither"
                    );
                    by_address.remove(&address);
                    continue;
                }
                by_address.insert(address, meta.clone());
            }
        }
        self.metadata.replace(by_address);
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
