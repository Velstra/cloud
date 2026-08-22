//! Saying how far along the Ceph cluster is, from what the nodes report.
//!
//! ## What this controller does not do, and why
//!
//! It does not run a single command. Bringing up a monitor or an OSD happens on
//! the machine the daemon will live on, and no controller can reach into a node
//! — the platform has exactly one way to make a machine do something, which is
//! for the machine's own agent to read stored state and act on it.
//!
//! So the deployment works like this, and the shape is worth stating because the
//! obvious alternative is worse:
//!
//! * The operator writes a [`CephClusterSpec`]: the network, which nodes are
//!   monitors, which disks are OSDs, which pools to create.
//! * **Every node agent** computes [`ceph::next_step`] from the same spec and
//!   the same node reports, and acts only if the step names *it*.
//! * Every node reports what it is running on itself, in its own `NodeStatus`.
//! * This controller reads those reports, assembles the cluster's status, and
//!   says whether what was asked for exists.
//!
//! The obvious alternative is for a controller to decide the step and write it
//! somewhere for the node to obey. That makes the status a **command**, and a
//! command is the one thing this platform does not have: a command has to be
//! delivered exactly once, cannot be re-derived after a restart, and turns every
//! crash into a question about whether it was carried out. A pure function of
//! stored state, run independently on each node, needs none of that — two nodes
//! computing it reach the same answer, and a node that computes it twice does
//! the same thing twice, which for `cephadm` is doing it once.
//!
//! ## Why the status is assembled here rather than claimed
//!
//! Everywhere else in this platform, a status is written by the one party that
//! can see it. A cluster spans machines, so no such party exists — "the cluster
//! is ready" is a judgement about *other* objects, which is exactly the kind of
//! thing [`crate::status::StatusWriter`] is the narrow exception for. The parts
//! are still each reported by whoever can see them: this reads node reports and
//! never asks a node a question.

use std::sync::Arc;

use tracing::info;
use velstra_cloud_model::{
    ceph::{
        CephCluster, CephClusterSpec, CephClusterStatus, CephObserved, CephPhase, CephStep,
        OsdSpec, next_step, observe, phase_of,
    },
    meta::{Condition, ConditionStatus, set_condition},
    resources::{Node, NodeSpec, NodeStatus},
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{
    Result,
    runner::{Reconciler, Related, Wake},
    status::StatusWriter,
};

const WHO: &str = "ceph";

/// The condition this controller owns: whether the cluster is what was asked for.
const READY: &str = "Ready";

pub struct CephController {
    say: StatusWriter<CephClusterSpec, CephClusterStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    cell: String,
}

impl CephController {
    pub fn new(
        store: Arc<dyn velstra_cloud_store::Store>,
        cell: &str,
        nodes: TypedStore<NodeSpec, NodeStatus>,
    ) -> Self {
        Self {
            say: StatusWriter::new(store, cell, "ceph-clusters", WHO),
            nodes,
            cell: cell.to_string(),
        }
    }

    /// What the cell's nodes report about Ceph on themselves.
    ///
    /// Read from the node objects and nowhere else. A controller that asked a
    /// node directly would be a second source of truth for a fact the node
    /// already publishes, and the two would disagree exactly when it mattered —
    /// during a partition, which is when somebody looks.
    /// Read through [`velstra_cloud_model::ceph::observe`] and not assembled
    /// here, because every node agent reads the same facts to decide what to do
    /// — and a controller that computed "the cluster is finished" differently
    /// from the nodes computing "there is nothing left to do" would disagree
    /// exactly when somebody was watching.
    async fn observe(&self, cluster: &CephCluster) -> Result<CephObserved> {
        let nodes: Vec<Node> = self.nodes.list().await?;
        // The published key is a monotonic witness that a cluster exists, where
        // "a monitor is running" is a daemon reading that can go false. See
        // [`CephObserved::with_published_key`].
        Ok(observe(&nodes).with_published_key(&cluster.status.ssh_pubkey))
    }
}

/// What the OSDs that exist are, as `(node, device)` pairs.
fn osds_up(observed: &CephObserved) -> Vec<OsdSpec> {
    observed
        .nodes
        .iter()
        .flat_map(|n| {
            n.osd_devices.iter().map(move |device| OsdSpec {
                node: n.node.clone(),
                device: device.clone(),
            })
        })
        .collect()
}

impl Reconciler for CephController {
    type Spec = CephClusterSpec;
    type Status = CephClusterStatus;

    fn name(&self) -> &'static str {
        "ceph"
    }

    fn related(&self) -> Vec<Related> {
        // A node reporting a monitor or an OSD it did not have before is the
        // whole of this controller's input, so a node changing is what makes
        // another look worthwhile.
        //
        // `Wake::All` rather than a mapping, and it is affordable here where it
        // would not be elsewhere: a cell has one Ceph cluster or none, so
        // "everything this controller owns" is one object. The cost of a node
        // heartbeat is therefore one reconcile that reads the node list once —
        // not the N² a mapping exists to avoid on a collection with thousands in
        // it.
        vec![Related {
            prefix: prefix_for(&self.cell, "nodes"),
            wake: Arc::new(|_| Wake::All),
        }]
    }

    async fn reconcile(&self, name: &str, object: Option<&CephCluster>) -> Result<()> {
        let Some(cluster) = object else {
            // Deleting the object does **not** tear the cluster down, and that
            // is deliberate: the data is on the disks, and a control-plane
            // record disappearing is not a reason to destroy it. An operator who
            // wants the cluster gone destroys it with Ceph's own tools, which is
            // an act with its own confirmations. So there is nothing to do here.
            return Ok(());
        };

        let observed = self.observe(cluster).await?;
        let phase = phase_of(&cluster.spec, &observed);
        let step = next_step(&cluster.spec, &observed);

        let mut next = cluster.clone();
        next.status.phase = phase;
        next.status.monitors_up = observed
            .nodes
            .iter()
            .filter(|n| n.monitor)
            .map(|n| n.node.clone())
            .collect();
        next.status.managers_up = observed
            .nodes
            .iter()
            .filter(|n| n.manager)
            .map(|n| n.node.clone())
            .collect();
        next.status.osds_up = osds_up(&observed);
        next.status.pools_present = observed.pools.clone();
        // Lifted from whichever node could ask for it, so every other node can
        // read it from one place. Never cleared once set: the answer stops
        // being available the moment the admin node is down, and a node that
        // then read an empty key would forget how to be reached.
        if !observed.ssh_pubkey.is_empty() {
            next.status.ssh_pubkey = observed.ssh_pubkey.clone();
        }

        // What the condition says, and it says the *step* rather than a
        // percentage: an operator watching a deployment wants to know what is
        // happening now and what is in the way, and "expanding, 60%" answers
        // neither.
        let (state, reason, message) = match &step {
            CephStep::Settled => (
                ConditionStatus::True,
                "Ready",
                "every monitor, OSD and pool that was asked for exists".to_string(),
            ),
            CephStep::Paused => (
                ConditionStatus::False,
                "Paused",
                "the deployment is paused; nothing has been torn down".to_string(),
            ),
            CephStep::Blocked { why } => (ConditionStatus::False, "Blocked", why.clone()),
            CephStep::Bootstrap { node } => (
                ConditionStatus::False,
                "Bootstrapping",
                format!("waiting for {node} to create the cluster"),
            ),
            CephStep::TrustKey { node, .. } => (
                ConditionStatus::False,
                "Expanding",
                format!("waiting for {node} to trust the cluster's SSH key"),
            ),
            CephStep::AddHost { node, address, .. } => (
                ConditionStatus::False,
                "Expanding",
                format!("waiting for {node} to be added to the cluster at {address}"),
            ),
            CephStep::AddMonitor { node } => (
                ConditionStatus::False,
                "Expanding",
                format!("waiting for a monitor on {node}"),
            ),
            CephStep::AddOsd { node, device } => (
                ConditionStatus::False,
                "Expanding",
                format!("waiting for {node} to make an OSD of {device}"),
            ),
            CephStep::CreatePool { pool } => (
                ConditionStatus::False,
                "Expanding",
                format!("waiting for the pool {}", pool.pool),
            ),
        };

        if phase == CephPhase::Ready && cluster.status.phase != CephPhase::Ready {
            info!(cluster = %name, "the cluster is what was asked for");
        }

        set_condition(
            &mut next.status.conditions,
            Condition::new(READY, state, reason, &message, cluster.meta.generation),
        );
        next.status.observed_generation = cluster.meta.generation;
        self.say.write(cluster, &next).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        ceph::{CephPoolSpec, NodeCeph},
        meta::{Meta, Placement, ResourceName},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    async fn cell() -> (Arc<dyn Store>, TypedStore<NodeSpec, NodeStatus>) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let nodes = TypedStore::new(store.clone(), "cell-1", "nodes");
        (store, nodes)
    }

    async fn node(nodes: &TypedStore<NodeSpec, NodeStatus>, id: &str, ceph: Option<NodeCeph>) {
        let node = Node {
            meta: Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            spec: NodeSpec {
                schedulable: true,
                labels: vec![],
            },
            status: NodeStatus {
                ceph,
                // Reporting now: these tests are about what a cluster's status
                // says, not about who is up, and the liveness rule has its own
                // test in the model.
                last_heartbeat: velstra_cloud_model::meta::Timestamp::now(),
                ..NodeStatus::default()
            },
        };
        nodes.create(&node).await.unwrap();
    }

    fn spec() -> CephClusterSpec {
        CephClusterSpec {
            public_network: "10.0.0.0/24".into(),
            monitors: vec!["a".into()],
            osds: vec![OsdSpec {
                node: "a".into(),
                device: "/dev/sdb".into(),
            }],
            pools: vec![CephPoolSpec {
                pool: "velstra-volumes".into(),
                size: 3,
                min_size: 2,
            }],
            ..CephClusterSpec::default()
        }
    }

    async fn cluster(store: &Arc<dyn Store>, spec: CephClusterSpec) -> CephCluster {
        let clusters: TypedStore<CephClusterSpec, CephClusterStatus> =
            TypedStore::new(store.clone(), "cell-1", "ceph-clusters");
        let object = Resource::new(
            Meta::new(
                ResourceName::parse("ceph-clusters/ceph").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            spec,
            CephClusterStatus::default(),
        );
        clusters.create(&object).await.unwrap();
        // Read back rather than returned as constructed: the store assigns the
        // revision, and that revision is the compare-and-swap the controller
        // writes against. Handing back the copy from before the create is how a
        // test asserts a conflict it created itself.
        clusters.get("ceph-clusters/ceph").await.unwrap().unwrap()
    }

    async fn read(store: &Arc<dyn Store>) -> CephCluster {
        let clusters: TypedStore<CephClusterSpec, CephClusterStatus> =
            TypedStore::new(store.clone(), "cell-1", "ceph-clusters");
        clusters.get("ceph-clusters/ceph").await.unwrap().unwrap()
    }

    /// A cluster nobody has started yet says what it is waiting for, by name.
    ///
    /// "Pending" on its own is the status that sends an operator looking through
    /// logs. The name of the node that has not done its part is the whole of the
    /// answer.
    #[tokio::test]
    async fn a_cluster_waiting_to_be_created_names_the_node_it_waits_for() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                ..NodeCeph::default()
            }),
        )
        .await;
        let object = cluster(&store, spec()).await;

        let controller = CephController::new(store.clone(), "cell-1", nodes);
        controller.reconcile("ceph", Some(&object)).await.unwrap();

        let after = read(&store).await;
        assert_eq!(after.status.phase, CephPhase::Bootstrapping);
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == READY)
            .expect("a Ready condition");
        assert_eq!(ready.status, ConditionStatus::False);
        assert!(ready.message.contains('a'), "{}", ready.message);
    }

    /// A node without the tooling blocks *by name*, rather than the cluster
    /// sitting at "pending" while an operator wonders which machine it is.
    #[tokio::test]
    async fn a_node_without_cephadm_is_named_rather_than_waited_on() {
        let (store, nodes) = cell().await;
        node(&nodes, "a", None).await;
        let object = cluster(&store, spec()).await;

        CephController::new(store.clone(), "cell-1", nodes)
            .reconcile("ceph", Some(&object))
            .await
            .unwrap();

        let after = read(&store).await;
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == READY)
            .unwrap();
        assert_eq!(ready.reason, "Blocked");
        assert!(
            ready.message.contains("Ceph installed"),
            "{}",
            ready.message
        );
    }

    /// A node that has not been reached yet is named, with the reason.
    ///
    /// cephadm drives every host over SSH, so a node that does not trust the
    /// cluster's key cannot be given a daemon — and `orch host add` would fail
    /// with a connection error, which is a confusing way to learn that a key is
    /// missing.
    #[tokio::test]
    async fn a_node_the_cluster_cannot_reach_yet_is_named_with_the_reason() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                monitor: true,
                address: "10.0.0.5".into(),
                ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
                cluster_hosts: vec!["a".into()],
                trusts_key: true,
                ..NodeCeph::default()
            }),
        )
        .await;
        node(
            &nodes,
            "b",
            Some(NodeCeph {
                installed: true,
                address: "10.0.0.6".into(),
                ..NodeCeph::default()
            }),
        )
        .await;
        let mut spec = spec();
        spec.monitors = vec!["a".into(), "b".into()];
        let object = cluster(&store, spec).await;

        CephController::new(store.clone(), "cell-1", nodes)
            .reconcile("ceph", Some(&object))
            .await
            .unwrap();

        let after = read(&store).await;
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == READY)
            .unwrap();
        assert!(ready.message.contains('b'), "{}", ready.message);
        assert!(ready.message.contains("SSH key"), "{}", ready.message);
    }

    /// Everything asked for exists: the phase is Ready and the parts are listed
    /// from what the nodes said, not from what the spec asked for.
    ///
    /// The difference matters. Echoing the spec back into the status would make
    /// a cluster look complete the moment somebody typed it.
    #[tokio::test]
    async fn a_finished_cluster_reports_the_parts_the_nodes_report() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                monitor: true,
                manager: true,
                osd_devices: vec!["/dev/sdb".into()],
                pools: vec!["velstra-volumes".into()],
                cluster_hosts: vec!["a".into()],
                address: "10.0.0.5".into(),
                ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
                trusts_key: true,
                ..NodeCeph::default()
            }),
        )
        .await;
        let object = cluster(&store, spec()).await;

        CephController::new(store.clone(), "cell-1", nodes)
            .reconcile("ceph", Some(&object))
            .await
            .unwrap();

        let after = read(&store).await;
        assert_eq!(after.status.phase, CephPhase::Ready);
        assert_eq!(after.status.monitors_up, ["a"]);
        // Reported separately, because with no manager the cluster keeps
        // serving I/O and stops answering questions — a failure that reads
        // nothing like a lost monitor.
        assert_eq!(after.status.managers_up, ["a"]);
        assert_eq!(after.status.osds_up.len(), 1);
        assert_eq!(after.status.pools_present, ["velstra-volumes"]);
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == READY)
            .unwrap();
        assert_eq!(ready.status, ConditionStatus::True);
    }

    /// A cluster asked for more than exists reports what is missing, and the
    /// listed parts are the ones that are really there.
    #[tokio::test]
    async fn a_half_built_cluster_lists_what_exists_not_what_was_asked() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                monitor: true,
                ..NodeCeph::default()
            }),
        )
        .await;
        let mut wanted = spec();
        wanted.monitors = vec!["a".into(), "b".into()];
        let object = cluster(&store, wanted).await;

        CephController::new(store.clone(), "cell-1", nodes)
            .reconcile("ceph", Some(&object))
            .await
            .unwrap();

        let after = read(&store).await;
        assert_eq!(after.status.phase, CephPhase::Expanding);
        // One monitor is up though two were asked for, and the status says one.
        assert_eq!(after.status.monitors_up, ["a"]);
        assert!(after.status.osds_up.is_empty());
        // And `b` is not reporting at all, which is a different problem from `b`
        // not having a monitor yet — so it is named as such.
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == READY)
            .unwrap();
        assert!(ready.message.contains('b'), "{}", ready.message);
    }

    /// Deleting the object does not tear the cluster down.
    ///
    /// The data is on the disks. A control-plane record disappearing — a bad
    /// `kubectl delete`, a botched migration, an operator tidying up — must not
    /// be a reason to destroy storage, and there is no confirmation on a delete
    /// that could make it one.
    #[tokio::test]
    async fn deleting_the_record_destroys_nothing() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                monitor: true,
                osd_devices: vec!["/dev/sdb".into()],
                ..NodeCeph::default()
            }),
        )
        .await;
        let controller = CephController::new(store.clone(), "cell-1", nodes.clone());
        controller.reconcile("ceph", None).await.unwrap();

        // The node still reports its OSD: nothing asked it to stop.
        let node = nodes.get("nodes/a").await.unwrap().unwrap();
        assert_eq!(node.status.ceph.unwrap().osd_devices, ["/dev/sdb"]);
    }

    /// A settled cluster reconciled twice writes once.
    ///
    /// The property every controller here has: a resync over a settled object
    /// costs a read and no write, which is what makes the resync interval a
    /// matter of taste rather than a load knob.
    #[tokio::test]
    async fn a_settled_cluster_is_not_written_again() {
        let (store, nodes) = cell().await;
        node(
            &nodes,
            "a",
            Some(NodeCeph {
                installed: true,
                monitor: true,
                manager: true,
                osd_devices: vec!["/dev/sdb".into()],
                pools: vec!["velstra-volumes".into()],
                cluster_hosts: vec!["a".into()],
                address: "10.0.0.5".into(),
                ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
                trusts_key: true,
                ..NodeCeph::default()
            }),
        )
        .await;
        let object = cluster(&store, spec()).await;
        let controller = CephController::new(store.clone(), "cell-1", nodes);

        controller.reconcile("ceph", Some(&object)).await.unwrap();
        let once = read(&store).await;
        controller.reconcile("ceph", Some(&once)).await.unwrap();
        let twice = read(&store).await;

        assert_eq!(
            once.meta.revision, twice.meta.revision,
            "a settled cluster was written again"
        );
    }
}
