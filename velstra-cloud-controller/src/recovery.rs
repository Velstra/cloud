//! Bringing guests back from a node that stopped answering.
//!
//! This controller does exactly one thing: it clears `spec.node` on a guest
//! whose node has been quiet long enough that the guest is certainly stopped.
//! The scheduler then does what it always does with an unplaced guest, and the
//! destination's agent starts it.
//!
//! That indirection is the design rather than an accident. The scheduler never
//! re-places anything — `needs_placement` asks whether `spec.node` is empty —
//! and this does not break that rule, it uses it. One deliberate act, by one
//! controller, that turns into ordinary placement.
//!
//! ## What makes it safe
//!
//! Nothing here decides that a node is dead. It reads how long ago the node
//! last reported and compares that against the node's **own** fencing deadline
//! plus a margin — and the node's agent stops its guests at that deadline,
//! using its own clock, needing nothing from anybody. So by the time this acts,
//! the guests are stopped or the machine is gone.
//!
//! A node with no deadline is never recovered from. The arithmetic and the
//! reasons live in [`velstra_cloud_model::ha`]; this is the loop that performs
//! what they decide.
//!
//! ## Where the reasons are, and why they are not here
//!
//! Most passes do nothing, and the refusals are the interesting part: "wait",
//! "the policy is leave", "this node does not fence", and "it holds hardware"
//! are four different afternoons for whoever is looking.
//!
//! They are **not** written onto the instance. The agent on the node owns that
//! status — `InstanceStatus::owner()` is `status.node` — and the access rule
//! refuses a controller writing it. That refusal is the rule working, and
//! arguing with it would mean two parties writing one object, which is the
//! thing this platform is built to prevent.
//!
//! So the answer is computed on demand instead: `:explainRecovery` on the API
//! runs the same function against the same objects and says what this
//! controller would say. It costs nothing when nobody asks, it cannot go stale,
//! and it is the shape `:explainPlacement` and `:explainMigration` already use
//! for exactly this kind of question.
//!
//! What that leaves here is a loop that either unplaces a guest or does
//! nothing at all — no writes on the ordinary pass, which is every pass in
//! every cell where nothing is broken.

use tracing::{info, warn};
use velstra_cloud_model::{
    access::Writer,
    ha::{self, GuestView, NodeView, OnNodeLoss},
    meta::{ConditionStatus, Timestamp},
    resources::{Instance, InstanceSpec, InstanceState, InstanceStatus, NodeSpec, NodeStatus},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "recovery";

pub struct RecoveryController {
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    margin_s: u32,
    now: std::sync::Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl RecoveryController {
    pub fn new(
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        nodes: TypedStore<NodeSpec, NodeStatus>,
    ) -> Self {
        Self {
            instances,
            nodes,
            margin_s: ha::RECOVERY_MARGIN_S,
            now: std::sync::Arc::new(Timestamp::now),
        }
    }

    /// Drive this controller from a clock the caller owns.
    pub fn with_clock(mut self, now: impl Fn() -> Timestamp + Send + Sync + 'static) -> Self {
        self.now = std::sync::Arc::new(now);
        self
    }

    pub fn with_margin(mut self, margin_s: u32) -> Self {
        self.margin_s = margin_s;
        self
    }
}

impl Reconciler for RecoveryController {
    type Spec = InstanceSpec;
    type Status = InstanceStatus;

    fn name(&self) -> &'static str {
        "recovery"
    }

    async fn reconcile(&self, name: &str, object: Option<&Instance>) -> Result<()> {
        let Some(instance) = object else {
            return Ok(());
        };
        // Not placed: nothing to recover from, and the scheduler already owns
        // this guest's next move.
        let Some(node_name) = instance.spec.node.clone().filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        // A guest nobody asked to recover costs one read and no writes. Checked
        // before the node is fetched, because this is nearly every guest in
        // nearly every cell and a list read per pass per guest is a cost with
        // no answer attached.
        if instance.spec.on_node_loss != OnNodeLoss::Restart {
            return Ok(());
        }

        let Some(node) = self.nodes.get(&node_name).await? else {
            // The node object has gone while a guest still names it. Not this
            // controller's to fix: clearing `spec.node` here would race with
            // whoever is removing the node, and a guest placed on a machine
            // nobody can describe is a thing to look at rather than to move.
            warn!(instance = %name, node = %node_name, "a guest names a node that is not there");
            return Ok(());
        };

        let guest = GuestView {
            name: name.to_string(),
            on_node_loss: instance.spec.on_node_loss,
            was_running: instance.status.state == InstanceState::Running,
            devices: instance.status.devices.clone(),
            deleting: instance.meta.is_deleting(),
        };
        let view = NodeView {
            name: node_name.clone(),
            last_heartbeat: node.status.last_heartbeat,
            fence_after_s: node.spec.fence_after_s,
            ready: velstra_cloud_model::meta::condition(&node.status.conditions, "Ready")
                .is_some_and(|c| c.status == ConditionStatus::True),
        };

        match ha::may_recover(&guest, &view, (self.now)(), self.margin_s) {
            // Nothing, and nothing written. Why is answerable at
            // `:explainRecovery`, which runs this same function on demand.
            Err(_) => Ok(()),
            Ok(()) => {
                info!(
                    instance = %name,
                    node = %node_name,
                    "its node has been quiet past its fencing deadline; unplacing it so it can \
                     be started elsewhere"
                );
                // The one write. Clearing the assignment is a spec change and
                // therefore a controller's to make; what happens next is
                // ordinary placement, by the scheduler, on a guest that is
                // now unplaced like any other.
                let mut next = instance.clone();
                next.spec.node = None;
                // A spec change no agent can notice is not a spec change — the
                // same rule the scheduler follows when it assigns one.
                next.meta.generation += 1;
                self.instances
                    .update(&next, &Writer::controller(WHO))
                    .await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use velstra_cloud_model::{
        meta::{Condition, Meta, Placement, ResourceName, set_condition},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const GUEST: &str = "projects/p1/instances/i1";
    const NODE: &str = "nodes/node-b";
    const S: u64 = 1000;

    struct Fixture {
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        controller: RecoveryController,
        now: Arc<AtomicU64>,
        /// When the node was last heard from.
        heard: u64,
    }

    async fn fixture(fence_after_s: u32, policy: OnNodeLoss) -> Fixture {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let instances: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(store.clone(), "cell-1", "instances");
        let nodes: TypedStore<NodeSpec, NodeStatus> =
            TypedStore::new(store.clone(), "cell-1", "nodes");

        // Anchored to real time, because `last_heartbeat` is stamped with the
        // store's clock in production and the two have to be the same kind of
        // number.
        let heard = Timestamp::now().0;

        let mut n: velstra_cloud_model::resources::Node = Resource::new(
            Meta::new(
                ResourceName::parse(NODE).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            NodeSpec {
                evacuate: false,
                fence_after_s,
                vcpu_overcommit: 0,
                schedulable: true,
                labels: vec![],
                cpu_baseline: None,
                gateway: false,
            },
            NodeStatus {
                last_heartbeat: Timestamp(heard),
                ..Default::default()
            },
        );
        set_condition(&mut n.status.conditions, Condition::ready(1));
        nodes
            .create(&n, &Writer::controller("test"))
            .await
            .unwrap();

        let mut i: Instance = Resource::new(
            Meta::new(
                ResourceName::parse(GUEST).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: policy,
                node: Some(NODE.into()),
                vcpus: 2,
                memory_mib: 4096,
                image: "projects/p1/images/debian".into(),
                root_disk_gib: 20,
                ..Default::default()
            },
            InstanceStatus {
                state: InstanceState::Running,
                node: Some(NODE.into()),
                ..Default::default()
            },
        );
        i.status.observed_generation = i.meta.generation;
        instances
            .create(&i, &Writer::controller("test"))
            .await
            .unwrap();

        let now = Arc::new(AtomicU64::new(heard));
        let reading = now.clone();
        let controller = RecoveryController::new(instances.clone(), nodes)
            .with_margin(60)
        .with_clock(move || Timestamp(reading.load(Ordering::Relaxed)));

        Fixture {
            instances,
            controller,
            now,
            heard,
        }
    }

    impl Fixture {
        fn quiet_for(&self, seconds: u64) {
            self.now.store(self.heard + seconds * S, Ordering::Relaxed);
        }

        async fn pass(&self) {
            let i = self.instances.get(GUEST).await.unwrap().unwrap();
            self.controller.reconcile(GUEST, Some(&i)).await.unwrap();
        }

        async fn placed_on(&self) -> Option<String> {
            self.instances
                .get(GUEST)
                .await
                .unwrap()
                .unwrap()
                .spec
                .node
        }

        /// What the API's `:explainRecovery` would say about this guest right
        /// now — the same function, on the same objects.
        async fn verdict(&self) -> std::result::Result<(), ha::NotRecoverable> {
            let i = self.instances.get(GUEST).await.unwrap().unwrap();
            let n = self
                .controller
                .nodes
                .get(i.spec.node.as_deref().unwrap_or_default())
                .await
                .unwrap()
                .unwrap();
            ha::may_recover(
                &GuestView {
                    name: GUEST.into(),
                    on_node_loss: i.spec.on_node_loss,
                    was_running: i.status.state == InstanceState::Running,
                    devices: i.status.devices.clone(),
                    deleting: i.meta.is_deleting(),
                },
                &NodeView {
                    name: n.meta.name.to_string(),
                    last_heartbeat: n.status.last_heartbeat,
                    fence_after_s: n.spec.fence_after_s,
                    ready: true,
                },
                Timestamp(self.now.load(Ordering::Relaxed)),
                60,
            )
        }
    }

    /// The whole feature, in order: nothing, a note saying wait, then the one
    /// write that hands the guest back to the scheduler.
    #[tokio::test]
    async fn a_guest_is_unplaced_only_once_its_node_is_certainly_stopped() {
        let f = fixture(60, OnNodeLoss::Restart).await;

        // Just heard from: nothing, and the note says how long is left rather
        // than "not yet", which is a thing somebody refreshes instead of waits
        // for.
        f.quiet_for(10);
        f.pass().await;
        assert_eq!(f.placed_on().await.as_deref(), Some(NODE));
        let Err(ha::NotRecoverable::NotQuietLongEnough { need_s, .. }) = f.verdict().await else {
            panic!("nothing explained why the guest had not moved");
        };
        assert_eq!(need_s, 120, "the answer does not say how long is left");

        // Past the node's own deadline but inside the margin: still nothing.
        // The agent has stopped its guests by now; the margin is what covers
        // the two clocks disagreeing about when "now" is.
        f.quiet_for(70);
        f.pass().await;
        assert_eq!(f.placed_on().await.as_deref(), Some(NODE));

        // Past both: unplaced, which is all this controller ever does. The
        // scheduler takes it from here exactly as it would any unplaced guest.
        f.quiet_for(130);
        f.pass().await;
        assert_eq!(
            f.placed_on().await,
            None,
            "the guest was not handed back to the scheduler"
        );
    }

    /// A node that does not fence is never recovered from, however long it has
    /// been quiet — and the guest carries the reason.
    #[tokio::test]
    async fn a_node_that_does_not_fence_leaves_its_guests_where_they_are() {
        let f = fixture(0, OnNodeLoss::Restart).await;
        f.quiet_for(86_400);
        f.pass().await;

        assert_eq!(
            f.placed_on().await.as_deref(),
            Some(NODE),
            "a guest was moved off a node that never stops its own"
        );
        let Err(why @ ha::NotRecoverable::NodeDoesNotFence { .. }) = f.verdict().await else {
            panic!("the reason a guest was left alone is not answerable");
        };
        assert!(why.to_string().contains("fencing deadline"), "{why}");
    }

    /// A guest whose policy is `leave` costs one read and no writes.
    ///
    /// This is nearly every guest in nearly every cell, so it is the path that
    /// has to be free. A note about a decision nobody asked for would make a
    /// quiet cell noisy on every pass.
    #[tokio::test]
    async fn a_guest_that_did_not_ask_for_this_is_left_alone_and_silently() {
        let f = fixture(60, OnNodeLoss::Leave).await;
        let before = f.instances.get(GUEST).await.unwrap().unwrap().meta.revision;
        f.quiet_for(86_400);
        f.pass().await;

        assert_eq!(f.placed_on().await.as_deref(), Some(NODE));
        // And nothing was written about it. This is nearly every guest in
        // nearly every cell, so it is the path that has to be free.
        let after = f.instances.get(GUEST).await.unwrap().unwrap();
        assert_eq!(
            after.meta.revision, before,
            "a guest nobody asked to recover was written about anyway"
        );
    }

    /// Once unplaced, this controller has nothing more to say.
    ///
    /// Without the guard it would look at an unplaced guest every pass and
    /// write a note about a node it is no longer on.
    #[tokio::test]
    async fn an_unplaced_guest_is_left_to_the_scheduler() {
        let f = fixture(60, OnNodeLoss::Restart).await;
        f.quiet_for(130);
        f.pass().await;
        assert_eq!(f.placed_on().await, None);

        let before = f.instances.get(GUEST).await.unwrap().unwrap();
        f.pass().await;
        let after = f.instances.get(GUEST).await.unwrap().unwrap();
        assert_eq!(
            before.meta.revision, after.meta.revision,
            "a pass over an unplaced guest wrote something"
        );
    }
}
