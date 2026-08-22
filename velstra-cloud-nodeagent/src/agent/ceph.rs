//! The node's Ceph pass: report what is here, then do the one step that is ours.
//!
//! Two halves that have to stay in this order. Reporting first means the step is
//! decided against what this machine actually looks like right now — including
//! whatever the *previous* pass did — rather than against a picture from before
//! it. A pass that acted first and reported afterwards would decide the next
//! step from stale facts, and the one step where that matters is `Bootstrap`:
//! it is the only irreversible one, and it is guarded by "no monitor is running
//! anywhere", which is a fact this pass is responsible for publishing.
//!
//! Doing nothing is the overwhelmingly common case. A cell with no Ceph cluster
//! reads one empty list and stops; a cell with a finished one reads the cluster,
//! computes `Settled`, and stops. See [`crate::ceph_deploy`] for why every node
//! computing the same answer independently is the whole of the coordination.

use std::sync::atomic::Ordering;

use velstra_cloud_model::ceph::CephStep;

use super::{Agent, Pass};
use crate::{
    ceph_deploy::{my_step, observe_node, perform, published_key, running_daemons, spec_names},
    host::HostState,
};

impl Agent {
    /// Report this machine's Ceph, and carry out this machine's step if it has
    /// one.
    ///
    /// `host` is updated in place rather than returned, because the node status
    /// is written at the end of the pass from exactly this value — and a second
    /// place holding "what this node runs of Ceph" is a second answer that can
    /// disagree with the first.
    pub(super) async fn ceph_pass(&self, host: &mut HostState, pass: &mut Pass) {
        let clusters = match self.cell.ceph_clusters().await {
            Ok(clusters) => clusters,
            Err(e) => {
                // Not fatal, and deliberately not counted as a pass failure on
                // a cell that has no Ceph: an API that does not serve the
                // collection at all is the ordinary state of a platform nobody
                // asked for storage on.
                tracing::debug!(error = %e, "could not read the Ceph cluster");
                return;
            }
        };
        // A cell holds one cluster. More would be an operator asking two
        // questions of one set of disks, and the first one is the one that
        // owns them.
        let Some(cluster) = clusters.first() else {
            // Empty is the ordinary answer — most cells never ask for Ceph —
            // and it is *also* what a misconfigured agent sees, which is the
            // problem. `ceph-clusters` is cell-wide, like `nodes`, and the API
            // narrows a cell-wide list to what the caller may read rather than
            // refusing it. An agent whose identity is not a cell operator is
            // therefore handed an empty list and reads it as "nobody asked for
            // Ceph" — the whole feature silently doing nothing, with no error,
            // no refusal and nothing in a log.
            //
            // The two cases are indistinguishable *here*, so they are told
            // apart somewhere they are not: a node always exists for itself, so
            // a node list that does not contain this machine has been filtered.
            self.probe_cell_reads_once().await;
            return;
        };

        // Whether this machine has any business with the deployment, asked
        // before the one cell-wide read the agent makes.
        //
        // Being named in the spec is the ordinary way. Running a monitor is the
        // other one, and it is not redundant: a node an operator has taken out
        // of the monitor list still holds the keyring and can still carry out
        // cluster commands, and it is the node that would otherwise be chosen.
        // Both questions are about this machine — one from an object already in
        // hand, one from systemd — so neither costs a list.
        //
        // `installed` comes first and gates the systemd call, because a machine
        // with no cephadm cannot be running a Ceph daemon and asking systemd
        // about one is a subprocess spawned to learn nothing. In a cell with a
        // Ceph cluster and Ceph on three of a thousand nodes, that is 997
        // pointless execs per resync — the same waste the cell-wide read was
        // just taken out of.
        let installed = self.cephadm.installed().await;
        let named = spec_names(&cluster.spec, &self.config.node);
        let daemons = if installed.installed {
            running_daemons(&self.cephadm).await
        } else {
            (false, false)
        };
        let concerns_me = named || daemons.0;

        // The node list is read *only* by a node that has business here, and
        // that is the difference between one collection read per Ceph node per
        // resync and one per *cell* node per resync. On a settled cluster the
        // second is pure waste, paid for ever, by machines with nothing to do
        // with the feature.
        let nodes = if concerns_me {
            match self.cell.nodes().await {
                Ok(nodes) => nodes,
                Err(e) => {
                    tracing::warn!(error = %e, "could not read the cell's nodes; no Ceph step this pass");
                    pass.failures += 1;
                    return;
                }
            }
        } else {
            Vec::new()
        };

        if concerns_me {
            self.say_if_filtered(&nodes);
        }

        // The key this node checks itself against.
        //
        // The cluster's status first, because that is where it belongs and
        // where an operator looks for it. But *falling back* to what the nodes
        // themselves report matters, and the reason is a livelock rather than a
        // nicety: whether root here trusts the key is a node-local fact, and if
        // the only place a node could learn the key were a field a controller
        // writes, then with no controller running every node past the first
        // would report `trusts_key: false` for ever, be handed `TrustKey` for
        // ever, install a key it already has for ever, and the deployment would
        // sit on that step with nothing saying why. The union of what the nodes
        // report is the same key, available without anybody else's help.
        let key = if cluster.status.ssh_pubkey.is_empty() {
            published_key(&nodes)
        } else {
            cluster.status.ssh_pubkey.clone()
        };

        // What this machine has to say about itself. Written even when the step
        // below belongs to somebody else — in fact *especially* then, because
        // the step somebody else is waiting on is usually this node's report.
        host.ceph = Some(
            observe_node(
                &self.cephadm,
                &cluster.spec.public_network,
                &host.devices,
                &key,
                installed,
                daemons,
            )
            .await,
        );

        // Reported, and then done with: a node the deployment has no business
        // with still publishes its disks and its daemons, because that is what
        // an operator reads when deciding to add it.
        if !concerns_me {
            return;
        }

        // The node list came from the store, so it does not contain what this
        // pass just observed about *this* node — that is written at the end.
        // Substituting it here is what keeps a node from acting on its own
        // stale report, which for `Bootstrap` would mean creating a second
        // cluster on top of the first.
        //
        // The disks go in for the same reason and it matters more: whether a
        // device may be handed to Ceph is decided from this list, and deciding
        // it from a copy written a pass ago would mean erasing a disk on the
        // strength of what it looked like a minute earlier.
        let mut nodes = nodes;
        if let Some(me) = nodes
            .iter_mut()
            .find(|n| n.meta.name.id() == self.config.node)
        {
            me.status.ceph = host.ceph.clone();
            me.status.devices = host.devices.clone();
            // And this machine is manifestly reporting: it is doing it right
            // now. The stored heartbeat is one pass old at best and absent on a
            // freshly started agent, and a node that judged *itself* dead would
            // refuse to take work it is plainly able to do.
            me.status.last_heartbeat = velstra_cloud_model::meta::Timestamp::now();
        }

        let Some(step) = my_step(&self.config.node, cluster, &nodes) else {
            return;
        };
        if matches!(step, CephStep::Settled | CephStep::Paused) {
            return;
        }

        let me = host.ceph.clone().unwrap_or_default();
        match perform(&self.cephadm, &step, cluster, &me).await {
            Ok(()) => pass.actions += 1,
            Err(e) => {
                // One step, one pass. A step that failed is asked for again next
                // time, from facts read again — which is why nothing here
                // retries, backs off, or remembers.
                tracing::warn!(error = %e, ?step, "a Ceph step did not go through");
                pass.failures += 1;
            }
        }
    }
}

impl Agent {
    /// Find out, once per process, whether this agent can read the cell at all.
    ///
    /// Only reached when the cluster list came back empty, which is the
    /// overwhelmingly common case — so it must not cost a read per pass. It
    /// costs one read per *process*, latched, because what it is testing is a
    /// configuration fact: an identity either may read cell-wide collections or
    /// it may not, and that does not change while the agent runs.
    async fn probe_cell_reads_once(&self) {
        if self.probed_ceph_reads.swap(true, Ordering::Relaxed) {
            return;
        }
        match self.cell.nodes().await {
            Ok(nodes) => self.say_if_filtered(&nodes),
            // A read that *failed* is a different thing from one that was
            // narrowed, and it says so itself. Nothing to add.
            Err(e) => tracing::debug!(error = %e, "could not read the cell's nodes"),
        }
    }

    /// Say so if a cell-wide read came back without this machine in it.
    ///
    /// A node always exists for itself, so this cannot happen in a healthy
    /// cell: the list was filtered, which means this agent's identity is not a
    /// cell operator. Said once, because it is a configuration mistake and
    /// repeating it every resync would bury everything else.
    fn say_if_filtered(&self, nodes: &[velstra_cloud_model::resources::Node]) {
        if nodes.iter().any(|n| n.meta.name.id() == self.config.node) {
            return;
        }
        if self.warned_about_ceph_reads.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            node = %self.config.node,
            "the cell's node list came back without this node in it, which cannot happen unless \
             it was filtered: this agent's identity is probably not a cell operator, so every \
             cell-wide collection reads as empty and the whole Ceph deployment will quietly do \
             nothing"
        );
    }
}
