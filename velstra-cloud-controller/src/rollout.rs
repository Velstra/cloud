//! Walking the cell's machines to a release.
//!
//! The thinking is `velstra_cloud_model::rollout::plan`, which is pure and
//! tested without a store. This controller reads what the plan needs — the
//! rollout, the release it names, every node, every running guest — hands it
//! over, and then does exactly what the plan said: cordons, evacuates, hands a
//! node what to run, and puts it back. Actions first, then the status, so a
//! status that says *cordoned* is one whose cordon was written.
//!
//! Woken by the nodes as well as by the rollout: a machine coming back on the
//! new build is a node status write, and the rollout has to see it to move on.
//! The control plane's own reboot ends the pass; the next controller reads the
//! status and carries on.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, ConditionStatus, Timestamp, set_condition},
    release::{ReleaseSpec, ReleaseStatus, UPDATING},
    resources::{
        InstanceSpec, InstanceState, InstanceStatus, Node, NodeSpec, NodeStatus, Resource,
    },
    rollout::{self, Action, RolloutPhase, RolloutSpec, RolloutStatus, Seen},
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{
    Result,
    runner::{Reconciler, Related},
    status::StatusWriter,
};

const WHO: &str = "rollout";
const OWNER: &str = "velstra.io/rollout-owner";

/// One edit of a node's spec, decided by the plan.
type Change = Box<dyn Fn(&mut NodeSpec) + Send>;

pub struct RolloutController {
    nodes: TypedStore<NodeSpec, NodeStatus>,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    releases: TypedStore<ReleaseSpec, ReleaseStatus>,
    status: StatusWriter<RolloutSpec, RolloutStatus>,
    /// The node this controller runs on, if it was told. Rolled out last.
    control_plane: Option<String>,
    cell: String,
}

impl RolloutController {
    pub fn new(
        nodes: TypedStore<NodeSpec, NodeStatus>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        releases: TypedStore<ReleaseSpec, ReleaseStatus>,
        status: StatusWriter<RolloutSpec, RolloutStatus>,
        control_plane: Option<String>,
        cell: &str,
    ) -> Self {
        Self {
            nodes,
            instances,
            releases,
            status,
            control_plane,
            cell: cell.to_string(),
        }
    }

    /// One write on one node, as the plan decided it. Nothing is written when
    /// the node already reads that way, so a repeated plan is free.
    async fn apply(&self, action: &Action, owner: &str) -> Result<()> {
        let (id, change): (&str, Change) = match action {
            Action::Cordon(n) => (n, Box::new(|s| s.schedulable = false)),
            Action::Evacuate(n) => (n, Box::new(|s| s.evacuate = true)),
            Action::Want(n, wanted) => {
                let wanted = wanted.clone();
                (n, Box::new(move |s| s.wanted = Some(wanted.clone())))
            }
            Action::Release(n) => (
                n,
                Box::new(|s| {
                    s.schedulable = true;
                    s.evacuate = false;
                    s.wanted = None;
                }),
            ),
        };
        let Some(node) = self.nodes.get(&format!("nodes/{id}")).await? else {
            return Ok(());
        };
        let owns = node
            .meta
            .labels
            .get(OWNER)
            .is_some_and(|held| held == owner);
        if !owns
            && (!matches!(action, Action::Cordon(_))
                || node.meta.labels.contains_key(OWNER)
                || !node.spec.schedulable
                || node.spec.evacuate)
        {
            return Err(crate::Error::Refused(format!(
                "{id} has a maintenance state owned outside {owner}"
            )));
        }
        let mut next = node.clone();
        change(&mut next.spec);
        if matches!(action, Action::Release(_)) {
            next.meta.labels.remove(OWNER);
        } else {
            next.meta.labels.insert(OWNER.into(), owner.into());
        }
        if next.spec == node.spec && next.meta.labels == node.meta.labels {
            return Ok(());
        }
        if next.spec != node.spec {
            next.meta.generation += 1;
        }
        self.nodes.update(&next, &Writer::controller(WHO)).await?;
        info!(node = id, ?action, "written for the rollout");
        Ok(())
    }
}

/// One node as the plan sees it.
fn seen(
    node: &Node,
    instances: &[Resource<InstanceSpec, InstanceStatus>],
    control_plane: Option<&str>,
) -> Seen {
    let id = node.meta.name.id();
    let updating = node.status.conditions.iter().find(|c| c.kind == UPDATING);
    Seen {
        node: id.to_string(),
        installed: node.status.installed.clone(),
        schedulable: node.spec.schedulable,
        evacuating: node.spec.evacuate,
        wanted: node.spec.wanted.clone(),
        ready: node
            .status
            .conditions
            .iter()
            .any(|c| c.kind == "Ready" && c.status == ConditionStatus::True),
        last_heartbeat: node.status.last_heartbeat,
        updating: updating
            .filter(|c| c.status == ConditionStatus::True)
            .map(|c| c.message.clone())
            .unwrap_or_default(),
        update_failed: updating
            .filter(|c| c.status == ConditionStatus::False && c.reason == "Failed")
            .map(|c| c.message.clone()),
        running_guests: instances
            .iter()
            .filter(|i| {
                i.status.node.as_deref() == Some(id)
                    && i.status.state == InstanceState::Running
                    && !i.meta.is_deleting()
            })
            .count() as u32,
        control_plane: control_plane == Some(id),
    }
}

impl Reconciler for RolloutController {
    type Spec = RolloutSpec;
    type Status = RolloutStatus;

    fn name(&self) -> &'static str {
        "rollout"
    }

    fn related(&self) -> Vec<Related> {
        // A node coming back is what a rollout waits for, and a release
        // becoming ready is what lets one start.
        vec![
            Related::all(prefix_for(&self.cell, "nodes")),
            Related::all(prefix_for(&self.cell, "releases")),
        ]
    }

    async fn reconcile(
        &self,
        _name: &str,
        object: Option<&Resource<Self::Spec, Self::Status>>,
    ) -> Result<()> {
        let Some(rollout) = object else {
            return Ok(());
        };
        if rollout.meta.is_deleting() {
            return Ok(());
        }
        let releases = self.releases.list().await?;
        let release = releases.iter().find(|r| {
            r.meta.name.to_string() == rollout.spec.release
                || r.meta.name.id() == rollout.spec.release
        });
        let nodes: Vec<Node> = self
            .nodes
            .list()
            .await?
            .into_iter()
            .filter(|n| !n.meta.is_deleting())
            .collect();
        let instances = self.instances.list().await?;
        let fleet: Vec<Seen> = nodes
            .iter()
            .map(|n| {
                let mut view = seen(n, &instances, self.control_plane.as_deref());
                // A crash after the cordon but before rollout status was saved
                // must not mistake our own cordon for an operator's lock.
                if n.meta
                    .labels
                    .get(OWNER)
                    .is_some_and(|owner| owner == &rollout.meta.name.to_string())
                {
                    view.schedulable = true;
                    // Draining still needs its real value to avoid another write.
                    if !rollout
                        .status
                        .nodes
                        .iter()
                        .any(|row| row.node == view.node && row.phase.in_flight())
                    {
                        view.evacuating = false;
                    }
                }
                view
            })
            .collect();
        let plan = rollout::plan(
            &rollout.spec,
            &rollout.status,
            release
                .map(|r| (r.meta.name.to_string(), &r.status))
                .as_ref()
                .map(|(n, s)| (n.as_str(), *s)),
            &fleet,
            Timestamp::now(),
        );
        for action in &plan.actions {
            self.apply(action, &rollout.meta.name.to_string()).await?;
        }
        let mut next = rollout.clone();
        next.status = plan.status;
        next.status.observed_generation = rollout.meta.generation;
        // Ready when it is done: the one condition a board reads a rollout by.
        // False with the phase as its reason otherwise, so a running rollout
        // reads as something to watch and a failed one as something to act on.
        let (status, reason) = match next.status.phase {
            RolloutPhase::Done => (ConditionStatus::True, "Done"),
            RolloutPhase::Failed => (ConditionStatus::False, "Failed"),
            RolloutPhase::Paused => (ConditionStatus::False, "Paused"),
            RolloutPhase::Running => (ConditionStatus::False, "Running"),
            RolloutPhase::Planning => (ConditionStatus::False, "Planning"),
        };
        let message = next.status.message.clone();
        set_condition(
            &mut next.status.conditions,
            Condition::new("Ready", status, reason, &message, rollout.meta.generation),
        );
        if next.status == rollout.status {
            return Ok(());
        }
        self.status.write(rollout, &next).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        installed::{InstallKind, Installed},
        meta::{Meta, Placement, ResourceName},
        release::Artefact,
        rollout::NodePhase,
    };
    use velstra_cloud_store::MemoryStore;

    use super::*;

    const TO: &str = "0.2.0+20260918.c571d71";
    const FROM: &str = "0.1.0+20260910.681269c";

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    struct Bench {
        raw: Arc<MemoryStore>,
        nodes: TypedStore<NodeSpec, NodeStatus>,
        rollouts: TypedStore<RolloutSpec, RolloutStatus>,
        controller: RolloutController,
    }

    async fn bench() -> Bench {
        let raw = Arc::new(MemoryStore::new());
        let nodes: TypedStore<NodeSpec, NodeStatus> =
            TypedStore::new(raw.clone(), "cell-1", "nodes");
        let releases: TypedStore<ReleaseSpec, ReleaseStatus> =
            TypedStore::new(raw.clone(), "cell-1", "releases");
        let rollouts: TypedStore<RolloutSpec, RolloutStatus> =
            TypedStore::new(raw.clone(), "cell-1", "rollouts");
        for (id, kind) in [
            ("paul", InstallKind::Package),
            ("horst", InstallKind::Appliance),
        ] {
            let node = Resource::new(
                meta(&format!("nodes/{id}")),
                NodeSpec {
                    schedulable: true,
                    ..Default::default()
                },
                NodeStatus {
                    installed: Installed {
                        kind,
                        version: FROM.into(),
                        ..Default::default()
                    },
                    conditions: vec![Condition::ready(1)],
                    last_heartbeat: Timestamp(1_000),
                    ..Default::default()
                },
            );
            nodes
                .create(&node, &Writer::controller("test"))
                .await
                .unwrap();
        }
        let release = Resource::new(
            meta("releases/v0.2.0"),
            ReleaseSpec {
                url: "https://x.example/v0.2.0".into(),
            },
            ReleaseStatus {
                version: TO.into(),
                image: Some(Artefact {
                    file: "velstra-cloud-node_0.2.0_amd64.raw.zst".into(),
                    sha256: "ab".repeat(32),
                    fetched: true,
                }),
                package: Some(Artefact {
                    file: "velstra-cloud_0.2.0_amd64.deb".into(),
                    sha256: "cd".repeat(32),
                    fetched: true,
                }),
                conditions: vec![Condition::ready(1)],
                ..Default::default()
            },
        );
        releases
            .create(&release, &Writer::controller("test"))
            .await
            .unwrap();
        let rollout = Resource::new(
            meta("rollouts/spring"),
            RolloutSpec {
                release: "releases/v0.2.0".into(),
                ..Default::default()
            },
            RolloutStatus::default(),
        );
        rollouts
            .create(&rollout, &Writer::controller("test"))
            .await
            .unwrap();
        let controller = RolloutController::new(
            nodes.clone(),
            TypedStore::new(raw.clone(), "cell-1", "instances"),
            releases,
            StatusWriter::new(raw.clone(), "cell-1", "rollouts", WHO),
            Some("horst".into()),
            "cell-1",
        );
        Bench {
            raw,
            nodes,
            rollouts,
            controller,
        }
    }

    async fn pass(b: &Bench) -> Resource<RolloutSpec, RolloutStatus> {
        let current = b.rollouts.get("rollouts/spring").await.unwrap().unwrap();
        b.controller
            .reconcile("rollouts/spring", Some(&current))
            .await
            .unwrap();
        b.rollouts.get("rollouts/spring").await.unwrap().unwrap()
    }

    fn row<'a>(
        r: &'a Resource<RolloutSpec, RolloutStatus>,
        node: &str,
    ) -> &'a rollout::NodeProgress {
        r.status
            .nodes
            .iter()
            .find(|n| n.node == node)
            .expect("a row")
    }

    /// The plan's writes land on the nodes, and the machine's own report is
    /// what moves the rollout on — through to the control plane, last.
    #[tokio::test]
    async fn the_plan_is_written_on_the_nodes_and_read_back_from_them() {
        let b = bench().await;
        let _ = b.raw;

        // Pass 1: paul is cordoned; the control plane is not touched.
        let r = pass(&b).await;
        assert_eq!(r.status.phase, RolloutPhase::Running);
        assert_eq!(row(&r, "paul").phase, NodePhase::Cordoned);
        assert_eq!(row(&r, "horst").phase, NodePhase::Pending);
        let paul = b.nodes.get("nodes/paul").await.unwrap().unwrap();
        assert!(!paul.spec.schedulable, "cordoned on the object itself");
        assert!(
            r.status
                .conditions
                .iter()
                .any(|c| c.kind == "Ready" && c.reason == "Running")
        );

        // Pass 2: nothing to drain, so paul is told what to run — the package.
        let r = pass(&b).await;
        assert_eq!(row(&r, "paul").phase, NodePhase::Applying);
        let paul = b.nodes.get("nodes/paul").await.unwrap().unwrap();
        let wanted = paul.spec.wanted.clone().expect("told what to run");
        assert_eq!(wanted.file, "velstra-cloud_0.2.0_amd64.deb");
        assert_eq!(wanted.version, TO);

        // The machine comes back on the new build and says so.
        let mut back = paul.clone();
        back.status.installed.version = TO.into();
        // Heard from after it was told: a heartbeat later than the row's
        // `since`, which was stamped this same millisecond.
        back.status.last_heartbeat = Timestamp(Timestamp::now().0 + 5_000);
        b.nodes.update(&back, &Writer::agent("paul")).await.unwrap();

        // Pass 3: paul is released and the control plane starts.
        let r = pass(&b).await;
        assert_eq!(row(&r, "paul").phase, NodePhase::Done);
        assert_eq!(row(&r, "horst").phase, NodePhase::Cordoned);
        let paul = b.nodes.get("nodes/paul").await.unwrap().unwrap();
        assert!(
            paul.spec.schedulable && paul.spec.wanted.is_none(),
            "back in service, wanting nothing"
        );
    }

    #[tokio::test]
    async fn a_cordon_survives_a_crash_before_rollout_status_is_written() {
        let b = bench().await;
        b.controller
            .apply(&Action::Cordon("paul".into()), "rollouts/spring")
            .await
            .unwrap();
        let r = pass(&b).await;
        assert_eq!(row(&r, "paul").phase, NodePhase::Cordoned);
        let r = pass(&b).await;
        assert_eq!(row(&r, "paul").phase, NodePhase::Applying);
    }

    #[tokio::test]
    async fn another_owner_cannot_release_a_nodes_maintenance() {
        let b = bench().await;
        b.controller
            .apply(&Action::Cordon("paul".into()), "rollouts/other")
            .await
            .unwrap();
        assert!(
            b.controller
                .apply(&Action::Release("paul".into()), "rollouts/spring")
                .await
                .is_err()
        );
        let node = b.nodes.get("nodes/paul").await.unwrap().unwrap();
        assert!(!node.spec.schedulable);
        assert_eq!(node.meta.labels.get(OWNER).unwrap(), "rollouts/other");
    }

    /// A machine that reports its apply failed stops the rollout, and the
    /// rollout's own condition says so in the machine's words.
    #[tokio::test]
    async fn a_failed_apply_is_read_off_the_node_and_stops_the_rollout() {
        let b = bench().await;
        pass(&b).await;
        pass(&b).await;
        let paul = b.nodes.get("nodes/paul").await.unwrap().unwrap();
        let mut failed = paul.clone();
        set_condition(
            &mut failed.status.conditions,
            Condition::new(
                UPDATING,
                ConditionStatus::False,
                "Failed",
                "the digest did not match",
                1,
            ),
        );
        b.nodes
            .update(&failed, &Writer::agent("paul"))
            .await
            .unwrap();
        let r = pass(&b).await;
        assert_eq!(r.status.phase, RolloutPhase::Failed);
        assert_eq!(row(&r, "paul").phase, NodePhase::Failed);
        assert_eq!(row(&r, "paul").message, "the digest did not match");
        let ready = r
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .unwrap();
        assert_eq!(ready.reason, "Failed");
        assert!(
            ready.message.contains("paul: the digest did not match"),
            "{}",
            ready.message
        );
        assert_eq!(
            row(&r, "horst").phase,
            NodePhase::Pending,
            "nothing else starts"
        );
    }
}
