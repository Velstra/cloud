//! Which node a port belongs to.
//!
//! Without this, no port created through the API could ever be claimed by
//! anybody. The access rule is "the fact wins while it exists; the assignee may
//! claim only when nobody holds it" — and a port had neither: nothing assigned
//! it, so `assigned_owner()` was `None`, and nobody owned it, so `owner()` was
//! `None` too, and every node's attempt to report on it was refused. The
//! visible symptom was mild and completely misleading: an instance ran fine, and
//! its port sat at `programmed: false` with no tap device for ever, because the
//! node carrying it was not allowed to say so.
//!
//! The fix is the platform's own precedent. An attachment does not decide which
//! node it is on either — `attachment.spec.node` is *derived* from the instance,
//! so an attachment naming the wrong node is unrepresentable. A port is the same
//! shape: it belongs to whichever node is running the guest that uses it, which
//! is one fact the platform already holds.
//!
//! **From `status.node`, not `spec.node`.** The assignment follows where the
//! guest actually is, not where a scheduler has decided it should go. During a
//! migration those differ for as long as the transfer takes, and assigning the
//! port to the destination early would invite it to report on a port the source
//! is still carrying — which is exactly the two-writers-on-one-object case the
//! access rule exists to prevent.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{ConditionStatus, condition},
    reconcile::{FinalizerStep, finalizer_step},
    resources::{InstanceSpec, InstanceStatus, NODE_RELEASE_FINALIZER, Port, PortSpec, PortStatus},
};
use velstra_cloud_store::{Cached, TypedStore, prefix_for};

use crate::{
    Result,
    runner::{Reconciler, Related},
};

const WHO: &str = "port";

/// Whether the node has said its datapath no longer carries this port.
///
/// Read from the condition the agent writes and never inferred from
/// `status.programmed`: a port that failed to program has `programmed == false`
/// too, and taking that for "torn down" would drop the guard while a tap — and,
/// on the fabric, an allocated address — was still there.
///
/// The second clause is for a port no node ever touched. Nothing will ever write
/// `Released` on one, so waiting for it would leave a port nobody can delete: a
/// port is created before the guest that will use it and may never be used at
/// all.
fn node_has_let_go(port: &Port) -> bool {
    if condition(&port.status.conditions, "Released")
        .is_some_and(|c| c.status == ConditionStatus::True)
    {
        return true;
    }
    port.status.node.is_none() && port.spec.node.is_none()
}

pub struct PortController {
    ports: TypedStore<PortSpec, PortStatus>,
    /// Mirrored rather than listed. Which instance uses a port is a reverse
    /// lookup — an instance names its ports and a port names no instance — so
    /// answering it from the store means reading every instance, per port, on
    /// every event. Measured at 401 objects in a cell of 400, which is ten
    /// thousand in a cell of ten thousand and a hundred million for one resync.
    instances: Cached<InstanceSpec, InstanceStatus>,
    /// Needed only to watch another collection: keys are `/<cell>/<kind>/…`, and
    /// a watch prefix without the cell in it matches nothing at all.
    cell: String,
}

impl PortController {
    pub fn new(
        ports: TypedStore<PortSpec, PortStatus>,
        instances: Cached<InstanceSpec, InstanceStatus>,
        cell: &str,
    ) -> Self {
        Self {
            ports,
            instances,
            cell: cell.to_string(),
        }
    }

    /// The release guard, in the same three steps every other one takes.
    ///
    /// Returns whether it did something, so the assignment below is skipped on
    /// a pass that has already written.
    ///
    /// A port had none, and the consequence was a device left on a machine for
    /// every port anybody ever deleted. `Api::delete` removes an object outright
    /// the moment nothing holds it, so a deleted port left the store inside the
    /// request that asked for it — and the node's teardown, which is reached
    /// only by reading a *stored* port that is being deleted, never ran. With
    /// the tap datapath that is a leaked interface per port; with the fabric
    /// one it is also a fabric port holding an address and a MAC that nothing
    /// will ever release.
    ///
    /// It was invisible because the node agent's own fixtures put a finalizer on
    /// every object they made, with a comment saying that is what a cell does.
    /// It was not what a cell did.
    async fn guard(&self, name: &str, port: &Port) -> Result<bool> {
        match finalizer_step(&port.meta, NODE_RELEASE_FINALIZER) {
            FinalizerStep::Add => {
                let mut next = port.clone();
                next.meta.add_finalizer(NODE_RELEASE_FINALIZER);
                self.ports.update(&next, &Writer::controller(WHO)).await?;
                Ok(true)
            }
            FinalizerStep::Wait => {
                if !port.meta.is_deleting() || !node_has_let_go(port) {
                    return Ok(false);
                }
                let mut next = port.clone();
                next.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
                self.ports.update(&next, &Writer::controller(WHO)).await?;
                info!(port = name, "the node let go; the guard is off");
                Ok(true)
            }
            FinalizerStep::Delete => {
                self.ports.delete(name, port.meta.revision).await?;
                info!(port = name, "gone");
                Ok(true)
            }
        }
    }

    /// The node holding the guest that uses this port, if there is one.
    ///
    /// A port no instance uses belongs to nobody, and that is a perfectly
    /// ordinary state: a port is created before the guest that will use it, and
    /// outlives it.
    async fn holder(&self, port: &str) -> Result<Option<String>> {
        Ok(self
            .instances
            .all()
            .await
            .0
            .into_iter()
            .find(|i| i.spec.ports.iter().any(|p| p == port))
            .and_then(|i| i.status.node.clone()))
    }
}

impl Reconciler for PortController {
    type Spec = PortSpec;
    type Status = PortStatus;

    fn name(&self) -> &'static str {
        "port"
    }

    fn related(&self) -> Vec<Related> {
        // An instance landing on a node is what makes its ports that node's,
        // and it is the instance that changes, not the port. Without this the
        // port would wait for the next full sweep to notice.
        // Two things were wrong here, and each on its own was enough to make
        // the whole controller a no-op that nothing noticed: the prefix was the
        // bare collection name, and keys are `/<cell>/<kind>/…`; and the mapping
        // returned an empty list where "look at all of them" was meant, which
        // enqueues nothing at all.
        //
        // The first answer to the second was a sweep, and it was measured before
        // it was believed: a sweep of every port, each reading every instance, is
        // N² — a hundred million reads for one guest moving in a cell of ten
        // thousand. An instance carries the answer in its own spec, so it is
        // taken from there and exactly those ports are woken. A delete carries no
        // object and still sweeps, which is correct and rare.
        vec![Related::of::<InstanceSpec, InstanceStatus>(
            prefix_for(&self.cell, "instances"),
            |instance| instance.spec.ports.clone(),
        )]
    }

    async fn reconcile(&self, name: &str, object: Option<&Port>) -> Result<()> {
        let Some(port) = object else {
            return Ok(());
        };

        // Before the assignment, because the guard has to be on before any node
        // can be told to program the port.
        if self.guard(name, port).await? {
            return Ok(());
        }

        let holder = self.holder(name).await?;
        if port.spec.node == holder {
            // Level-triggered: the ordinary pass writes nothing at all, which is
            // what keeps a settled cell settled.
            return Ok(());
        }
        let mut next = port.clone();
        next.spec.node = holder.clone();
        next.meta.generation += 1;
        self.ports
            .update(&next, &Writer::controller("port"))
            .await?;
        match holder {
            Some(node) => info!(port = name, node, "assigned"),
            // Not an error and not a leak: the guest is gone, and the node that
            // carried the port lets go of the status on its own next pass.
            None => info!(port = name, "unassigned"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
        resources::Resource,
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement::new("eu-central", "cell-1"),
        )
    }

    async fn cell() -> (
        PortController,
        TypedStore<PortSpec, PortStatus>,
        TypedStore<InstanceSpec, InstanceStatus>,
    ) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let ports = TypedStore::new(store.clone(), "cell-1", "ports");
        let instances = TypedStore::new(store.clone(), "cell-1", "instances");
        (
            PortController::new(
                ports.clone(),
                Cached::start(
                    instances.clone(),
                    store.clone(),
                    velstra_cloud_store::prefix_for("cell-1", "instances"),
                ),
                "cell-1",
            ),
            ports,
            instances,
        )
    }

    /// Reconcile `n` times.
    ///
    /// More than one, because the guard and the assignment are separate writes
    /// on purpose: the finalizer has to be on before anything can be told to
    /// program the port, so the first pass only takes the guard. A test that
    /// reconciled once and asserted on the assignment would be asserting on the
    /// pass before the one it means.
    async fn passes(
        controller: &PortController,
        ports: &TypedStore<PortSpec, PortStatus>,
        name: &str,
        n: usize,
    ) {
        for _ in 0..n {
            let port = ports.get(name).await.unwrap().unwrap();
            controller.reconcile(name, Some(&port)).await.unwrap();
        }
    }

    async fn a_port(ports: &TypedStore<PortSpec, PortStatus>, name: &str) {
        ports
            .create(&Resource::new(
                meta(name),
                PortSpec {
                    network: "projects/p1/networks/n1".into(),
                    subnet: "projects/p1/subnets/s1".into(),
                    ..Default::default()
                },
                PortStatus::default(),
            ))
            .await
            .unwrap();
    }

    async fn a_guest(
        instances: &TypedStore<InstanceSpec, InstanceStatus>,
        name: &str,
        port: &str,
        on: Option<&str>,
    ) {
        instances
            .create(&Resource::new(
                meta(name),
                InstanceSpec {
                    ports: vec![port.to_string()],
                    ..Default::default()
                },
                InstanceStatus {
                    node: on.map(str::to_string),
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_port_nobody_uses_belongs_to_nobody() {
        let (controller, ports, _) = cell().await;
        a_port(&ports, "projects/p1/ports/port-a").await;
        passes(&controller, &ports, "projects/p1/ports/port-a", 1).await;
        let before = ports
            .get("projects/p1/ports/port-a")
            .await
            .unwrap()
            .unwrap();
        passes(&controller, &ports, "projects/p1/ports/port-a", 1).await;
        let after = ports
            .get("projects/p1/ports/port-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.spec.node, None);
        assert_eq!(
            after.meta.revision, before.meta.revision,
            "a settled port was written to"
        );
    }

    #[tokio::test]
    async fn a_port_belongs_to_the_node_holding_the_guest_that_uses_it() {
        let (controller, ports, instances) = cell().await;
        a_port(&ports, "projects/p1/ports/port-a").await;
        a_guest(
            &instances,
            "projects/p1/instances/i1",
            "projects/p1/ports/port-a",
            Some("node-a"),
        )
        .await;
        passes(&controller, &ports, "projects/p1/ports/port-a", 2).await;
        let after = ports
            .get("projects/p1/ports/port-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.spec.node.as_deref(), Some("node-a"));
    }

    #[tokio::test]
    async fn a_guest_that_has_not_landed_yet_assigns_nothing() {
        // `spec.node` on the instance is where a scheduler wants it;
        // `status.node` is where it is. Assigning the port on the strength of
        // the first would hand it to a node that is not running anything.
        let (controller, ports, instances) = cell().await;
        a_port(&ports, "projects/p1/ports/port-a").await;
        a_guest(
            &instances,
            "projects/p1/instances/i1",
            "projects/p1/ports/port-a",
            None,
        )
        .await;
        passes(&controller, &ports, "projects/p1/ports/port-a", 2).await;
        let after = ports
            .get("projects/p1/ports/port-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.spec.node, None);
    }

    #[tokio::test]
    async fn reconciling_twice_writes_once() {
        let (controller, ports, instances) = cell().await;
        a_port(&ports, "projects/p1/ports/port-a").await;
        a_guest(
            &instances,
            "projects/p1/instances/i1",
            "projects/p1/ports/port-a",
            Some("node-a"),
        )
        .await;
        passes(&controller, &ports, "projects/p1/ports/port-a", 3).await;
        let after = ports
            .get("projects/p1/ports/port-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.meta.generation, 2,
            "a settled port was written to a second time"
        );
    }

    const PORT: &str = "projects/p1/ports/port-a";

    async fn deleting(ports: &TypedStore<PortSpec, PortStatus>) {
        let mut port = ports.get(PORT).await.unwrap().unwrap();
        port.meta.deleted_at = Some(Timestamp::now());
        ports
            .update(&port, &Writer::controller("api"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_guard_goes_on_before_a_port_is_given_to_anybody() {
        let (controller, ports, _) = cell().await;
        a_port(&ports, PORT).await;
        passes(&controller, &ports, PORT, 1).await;
        let port = ports.get(PORT).await.unwrap().unwrap();
        assert!(
            port.meta.has_finalizer(NODE_RELEASE_FINALIZER),
            "a port could be deleted out from under the node carrying it"
        );
        assert_eq!(
            port.spec.node, None,
            "the guard pass also made the assignment; they have to be separate \
             writes or the guard is not on first"
        );
    }

    #[tokio::test]
    async fn a_port_a_node_still_carries_does_not_go() {
        // The leak this closes: `Api::delete` removes an object outright the
        // moment nothing holds it, so a deleted port used to vanish inside the
        // request that asked for it — leaving the tap on the machine and, on the
        // fabric datapath, a port holding an address and a MAC.
        let (controller, ports, instances) = cell().await;
        a_port(&ports, PORT).await;
        a_guest(&instances, "projects/p1/instances/i1", PORT, Some("node-a")).await;
        passes(&controller, &ports, PORT, 2).await;

        // The node claims it, the way an agent does.
        let mut claimed = ports.get(PORT).await.unwrap().unwrap();
        claimed.status.node = Some("node-a".into());
        ports
            .update(&claimed, &Writer::agent("node-a"))
            .await
            .unwrap();

        deleting(&ports).await;
        passes(&controller, &ports, PORT, 3).await;
        assert!(
            ports.get(PORT).await.unwrap().is_some(),
            "the port went while a node was still carrying it"
        );

        // The node reports what it observed.
        let mut released = ports.get(PORT).await.unwrap().unwrap();
        set_condition(
            &mut released.status.conditions,
            Condition::new(
                "Released",
                ConditionStatus::True,
                "Released",
                "this node holds nothing of it",
                released.meta.generation,
            ),
        );
        ports
            .update(&released, &Writer::agent("node-a"))
            .await
            .unwrap();

        passes(&controller, &ports, PORT, 2).await;
        assert!(
            ports.get(PORT).await.unwrap().is_none(),
            "a released port stayed"
        );
    }

    #[tokio::test]
    async fn a_port_no_node_ever_carried_can_still_be_deleted() {
        // Nothing will ever write `Released` on it, so waiting for one would be
        // a guard nobody can lift. A port is created before the guest that will
        // use it and may never be used at all.
        let (controller, ports, _) = cell().await;
        a_port(&ports, PORT).await;
        passes(&controller, &ports, PORT, 1).await;
        deleting(&ports).await;
        passes(&controller, &ports, PORT, 2).await;
        assert!(
            ports.get(PORT).await.unwrap().is_none(),
            "a port nobody ever carried could not be deleted"
        );
    }
}
