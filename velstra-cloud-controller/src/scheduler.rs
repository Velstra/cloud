//! Placement: read the nodes, ask [`place`], write one field.
//!
//! There is no claim, no reservation and no placement table, and the absence of
//! all three is the design. A reservation that is not released when a scheduler
//! dies is capacity that never comes back, and a table that has to agree with
//! what the nodes report is a second source of truth that will eventually
//! disagree with the first. Here the only durable record of a placement is
//! `spec.node` on the instance, written with a compare-and-swap: two schedulers
//! looking at one instance produce one assignment and one retry, and a
//! scheduler that dies mid-decision leaves nothing behind at all.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    meta::{condition, set_condition},
    reconcile::{needs_placement, place, scheduled_condition, unschedulable_condition},
    resources::{Instance, InstanceSpec, InstanceStatus, NodeSpec, NodeStatus},
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{Related, Result, runner::Reconciler, status::StatusWriter};

/// One stored window as the model's decisions see it.
pub(crate) fn window_view(
    w: &velstra_cloud_model::resources::MaintenanceWindow,
) -> velstra_cloud_model::maintenance::WindowView {
    velstra_cloud_model::maintenance::WindowView {
        name: w.meta.name.to_string(),
        node: w.spec.node.clone(),
        starts_at: w.spec.starts_at,
        minutes: w.spec.minutes,
        drain: w.spec.drain,
        note: w.spec.note.clone(),
    }
}

pub struct Scheduler {
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    status: StatusWriter<InstanceSpec, InstanceStatus>,
    cell: String,
    /// Instances that could not be placed, so that a node coming back can wake
    /// exactly them. A hint for the queue and never a fact about the world:
    /// losing it in a restart costs one resync of latency and nothing else.
    pending: Arc<Mutex<BTreeSet<String>>>,
    /// The cell's PCI device classes, read to answer what an instance asked
    /// for. `None` in a cell that has none — which is most cells, and where
    /// an instance naming a class is refused by name rather than placed onto
    /// a machine that cannot give it anything.
    classes: Option<
        TypedStore<
            velstra_cloud_model::pci::DeviceClassSpec,
            velstra_cloud_model::resources::DeviceClassStatus,
        >,
    >,
    /// The cell's maintenance windows, so that a node somebody has declared out
    /// of service is not handed new work at two in the morning. `None` in a
    /// cell where nothing has ever been scheduled for maintenance.
    windows: Option<
        TypedStore<
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus,
        >,
    >,
}

impl Scheduler {
    pub fn new(
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        nodes: TypedStore<NodeSpec, NodeStatus>,
        status: StatusWriter<InstanceSpec, InstanceStatus>,
        cell: &str,
    ) -> Self {
        Self {
            classes: None,
            windows: None,
            instances,
            nodes,
            status,
            cell: cell.to_string(),
            pending: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Give this scheduler the cell's device classes.
    ///
    /// Opt-in rather than required, so a cell that passes no hardware through
    /// needs no extra store — and so adding this could not change the shape of
    /// every existing caller.
    pub fn with_device_classes(
        mut self,
        classes: TypedStore<
            velstra_cloud_model::pci::DeviceClassSpec,
            velstra_cloud_model::resources::DeviceClassStatus,
        >,
    ) -> Self {
        self.classes = Some(classes);
        self
    }

    /// The classes, by id. Empty when this cell has none, and empty when they
    /// cannot be read — a scheduler that placed a device-hungry guest onto an
    /// arbitrary node because a list did not load would be worse than one that
    /// refuses and says the class was not found.
    async fn device_classes(
        &self,
    ) -> std::collections::BTreeMap<String, velstra_cloud_model::pci::DeviceClassSpec> {
        let Some(store) = &self.classes else {
            return Default::default();
        };
        match store.list().await {
            Ok(all) => all
                .into_iter()
                .map(|c| (c.meta.name.id().to_string(), c.spec))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read this cell's device classes");
                Default::default()
            }
        }
    }

    /// Give this scheduler the cell's maintenance windows.
    pub fn with_maintenance(
        mut self,
        windows: TypedStore<
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus,
        >,
    ) -> Self {
        self.windows = Some(windows);
        self
    }

    /// The nodes that are out of service this instant.
    ///
    /// Empty when they cannot be read — and that direction is deliberate. A
    /// list that fails to load makes this scheduler place onto a node somebody
    /// is about to unplug, which costs one migration; treating every node as
    /// closed would stop the cell placing anything at all because one read
    /// failed, which is the worse of the two by a distance.
    async fn closed_nodes(&self) -> Vec<velstra_cloud_model::maintenance::Closed> {
        let Some(store) = &self.windows else {
            return Vec::new();
        };
        match store.list().await {
            Ok(all) => velstra_cloud_model::maintenance::closed_now(
                &all.iter().map(window_view).collect::<Vec<_>>(),
                velstra_cloud_model::meta::Timestamp::now(),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "could not read this cell's maintenance windows");
                Vec::new()
            }
        }
    }

    /// Which anti-affinity group already occupies which node.
    ///
    /// Derived from the instances themselves on every pass rather than cached:
    /// a group membership that outlives its instance is an anti-affinity rule
    /// that refuses a node forever, for a guest that stopped existing.
    fn occupied_groups(instances: &[Instance]) -> Vec<(String, String)> {
        Self::groups_by(instances, |i| {
            i.spec.placement_policy.anti_affinity_group.clone()
        })
    }

    /// Which affinity group is already on which node — the opposite ask, read
    /// the same way and for the same reason.
    fn grouped_with(instances: &[Instance]) -> Vec<(String, String)> {
        Self::groups_by(instances, |i| {
            i.spec.placement_policy.affinity_group.clone()
        })
    }

    fn groups_by(
        instances: &[Instance],
        group_of: impl Fn(&Instance) -> Option<String>,
    ) -> Vec<(String, String)> {
        instances
            .iter()
            .filter_map(|i| Some((group_of(i)?, i.spec.node.clone()?)))
            .collect()
    }
}

impl Reconciler for Scheduler {
    type Spec = InstanceSpec;
    type Status = InstanceStatus;

    fn name(&self) -> &'static str {
        "scheduler"
    }

    fn related(&self) -> Vec<Related> {
        // A node coming back, or being made schedulable again, is the event
        // that unblocks everything that could not be placed. Without it an
        // operator who has just fixed a node waits out the resync interval
        // wondering whether anything noticed.
        let pending = self.pending.clone();
        vec![Related::named(
            prefix_for(&self.cell, "nodes"),
            move |_node: &str| pending.lock().unwrap().iter().cloned().collect(),
        )]
    }

    async fn reconcile(&self, name: &str, object: Option<&Instance>) -> Result<()> {
        let Some(instance) = object else {
            self.pending.lock().unwrap().remove(name);
            return Ok(());
        };
        if !needs_placement(instance) {
            self.pending.lock().unwrap().remove(name);
            return Ok(());
        }

        let nodes = self.nodes.list().await?;
        let all = self.instances.list().await?;
        let generation = instance.meta.generation;

        let classes = self.device_classes().await;
        let closed = self.closed_nodes().await;
        match place(
            instance,
            &nodes,
            &Self::occupied_groups(&all),
            &Self::grouped_with(&all),
            &classes,
            &closed,
        ) {
            Err(why) => {
                // The rejection chain goes on the object, because an operator
                // asking "why is this not running" should not have to find the
                // scheduler's log on whichever machine happened to run it.
                let mut next = instance.clone();
                set_condition(
                    &mut next.status.conditions,
                    unschedulable_condition(&why, generation),
                );
                self.status.write(instance, &next).await?;
                self.pending.lock().unwrap().insert(name.to_string());
                Ok(())
            }
            Ok(node) => {
                let mut assigned = instance.clone();
                // Only when something was said before: an instance nobody has
                // spoken about needs no correction, and writing one anyway
                // would double the writes on the ordinary path. What this
                // corrects is a stale `NoValidHost` that would otherwise sit on
                // a freshly placed instance until its node first reports.
                if condition(&instance.status.conditions, "Ready").is_some() {
                    let mut corrected = instance.clone();
                    set_condition(
                        &mut corrected.status.conditions,
                        scheduled_condition(&node, generation),
                    );
                    if let Some(revision) = self.status.write(instance, &corrected).await? {
                        corrected.meta.revision = revision;
                        assigned = corrected;
                    }
                }

                assigned.spec.node = Some(node.clone());
                assigned.meta.generation += 1;
                // The compare-and-swap is the entire race protocol: whoever
                // writes second is told the object moved, reads it again, and
                // finds an instance that needs no placement at all.
                self.instances
                    .update(&assigned, &Writer::controller("scheduler"))
                    .await?;
                self.pending.lock().unwrap().remove(name);
                info!(instance = name, node, "placed");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        Condition, ConditionStatus,
        meta::{Meta, Placement, ResourceName},
        resources::{Capacity, Resource},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    struct Fixture {
        raw: Arc<MemoryStore>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        nodes: TypedStore<NodeSpec, NodeStatus>,
    }

    fn fixture() -> Fixture {
        let raw = Arc::new(MemoryStore::new());
        Fixture {
            instances: TypedStore::new(raw.clone(), "cell-1", "instances"),
            nodes: TypedStore::new(raw.clone(), "cell-1", "nodes"),
            raw,
        }
    }

    impl Fixture {
        fn scheduler(&self) -> Scheduler {
            Scheduler::new(
                self.instances.clone(),
                self.nodes.clone(),
                StatusWriter::new(self.raw.clone(), "cell-1", "instances", "scheduler"),
                "cell-1",
            )
        }

        async fn instance(&self, id: &str, memory_mib: u64) -> Instance {
            let i = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("projects/p1/instances/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                InstanceSpec {
                    start_order: 0,
                    start_delay_s: 0,
                    on_node_loss: Default::default(),
                    console: false,
                    devices: Vec::new(),
                    vcpus: 2,
                    memory_mib,
                    ..Default::default()
                },
                InstanceStatus::default(),
            );
            self.instances
                .create(
                    &i,
                    &velstra_cloud_model::access::Writer::controller("scheduler"),
                )
                .await
                .unwrap();
            self.instances
                .get(&i.meta.name.to_string())
                .await
                .unwrap()
                .unwrap()
        }

        async fn node(&self, id: &str, memory_mib: u64, ready: bool) {
            let mut n = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                NodeSpec {
                    evacuate: false,
                    vcpu_overcommit: 0,
                    fence_after_s: 0,
                    schedulable: true,
                    labels: vec![],
                    cpu_baseline: None,
                    gateway: false,
                },
                NodeStatus {
                    shared_state: false,
                    vmm: "qemu".into(),
            fetching: Vec::new(),
                    capacity: Capacity {
                        vcpus: 16,
                        memory_mib,
                        disk_gib: 1000,
                        numa_free_mib: vec![memory_mib],
                        hugepages_1gi: 0,
                    },
                    ..Default::default()
                },
            );
            if ready {
                set_condition(&mut n.status.conditions, Condition::ready(1));
            }
            self.nodes
                .create(
                    &n,
                    &velstra_cloud_model::access::Writer::controller("scheduler"),
                )
                .await
                .unwrap();
        }

        async fn reload(&self, id: &str) -> Instance {
            self.instances
                .get(&format!("projects/p1/instances/{id}"))
                .await
                .unwrap()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn an_unplaced_instance_gets_a_node_and_a_generation() {
        let f = fixture();
        f.node("a", 16384, true).await;
        let i = f.instance("i1", 2048).await;

        f.scheduler()
            .reconcile(&i.meta.name.to_string(), Some(&i))
            .await
            .unwrap();

        let placed = f.reload("i1").await;
        assert_eq!(placed.spec.node.as_deref(), Some("a"));
        assert_eq!(
            placed.meta.generation,
            i.meta.generation + 1,
            "a spec change no agent can notice is not a spec change"
        );
    }

    #[tokio::test]
    async fn a_placed_instance_is_never_re_placed() {
        // Moving a running guest is a migration — a deliberate act with its own
        // resource, never something a scheduler pass decides on its own.
        let f = fixture();
        f.node("a", 16384, true).await;
        let i = f.instance("i1", 2048).await;
        let scheduler = f.scheduler();
        scheduler
            .reconcile(&i.meta.name.to_string(), Some(&i))
            .await
            .unwrap();

        let placed = f.reload("i1").await;
        let revision = f.raw.revision().await.unwrap();
        scheduler
            .reconcile(&placed.meta.name.to_string(), Some(&placed))
            .await
            .unwrap();
        assert_eq!(
            f.raw.revision().await.unwrap(),
            revision,
            "reconciling a placed instance wrote something"
        );
    }

    #[tokio::test]
    async fn nothing_fits_and_the_object_says_why() {
        let f = fixture();
        f.node("a", 4096, true).await;
        f.node("b", 8192, false).await;
        let i = f.instance("i1", 65536).await;

        f.scheduler()
            .reconcile(&i.meta.name.to_string(), Some(&i))
            .await
            .unwrap();

        let after = f.reload("i1").await;
        let ready = condition(&after.status.conditions, "Ready").unwrap();
        assert_eq!(ready.status, ConditionStatus::False);
        assert_eq!(ready.reason, "NoValidHost");
        assert!(
            ready.message.contains("a: 4096 MiB free"),
            "{}",
            ready.message
        );
        assert!(ready.message.contains("b: not ready"), "{}", ready.message);
        assert!(
            after.spec.node.is_none(),
            "an instance was placed on nothing"
        );
    }

    #[tokio::test]
    async fn saying_the_same_thing_twice_writes_once() {
        let f = fixture();
        f.node("a", 4096, true).await;
        let i = f.instance("i1", 65536).await;
        let scheduler = f.scheduler();
        scheduler
            .reconcile(&i.meta.name.to_string(), Some(&i))
            .await
            .unwrap();

        let after = f.reload("i1").await;
        let revision = f.raw.revision().await.unwrap();
        scheduler
            .reconcile(&after.meta.name.to_string(), Some(&after))
            .await
            .unwrap();
        assert_eq!(
            f.raw.revision().await.unwrap(),
            revision,
            "an unchanged rejection was written again, and every watcher woken for it"
        );
    }

    #[tokio::test]
    async fn a_stale_rejection_is_corrected_when_the_instance_is_finally_placed() {
        let f = fixture();
        let i = f.instance("i1", 2048).await;
        let scheduler = f.scheduler();
        scheduler
            .reconcile(&i.meta.name.to_string(), Some(&i))
            .await
            .unwrap();
        assert_eq!(
            condition(&f.reload("i1").await.status.conditions, "Ready")
                .unwrap()
                .reason,
            "NoValidHost"
        );

        f.node("a", 16384, true).await;
        let pending = f.reload("i1").await;
        scheduler
            .reconcile(&pending.meta.name.to_string(), Some(&pending))
            .await
            .unwrap();

        let placed = f.reload("i1").await;
        assert_eq!(placed.spec.node.as_deref(), Some("a"));
        let ready = condition(&placed.status.conditions, "Ready").unwrap();
        assert_eq!(
            ready.status,
            ConditionStatus::Unknown,
            "a placed instance still claimed there was no valid host"
        );
        assert_eq!(ready.reason, "Scheduled");
    }

    #[tokio::test]
    async fn two_schedulers_on_one_instance_place_it_once() {
        // Both hold the same copy — the exact race two replicas produce. One
        // compare-and-swap wins; the other must be told it lost rather than
        // overwrite the assignment with its own.
        let f = fixture();
        f.node("a", 16384, true).await;
        f.node("b", 16384, true).await;
        let i = f.instance("i1", 2048).await;
        let name = i.meta.name.to_string();

        let first = f.scheduler();
        let second = f.scheduler();
        first.reconcile(&name, Some(&i)).await.unwrap();
        let err = second.reconcile(&name, Some(&i)).await.unwrap_err();
        assert!(err.is_conflict(), "the loser overwrote the winner: {err}");

        // …and on the retry it finds nothing to do, which is what "does not
        // double-place" means.
        let placed = f.reload("i1").await;
        let node = placed.spec.node.clone();
        second.reconcile(&name, Some(&placed)).await.unwrap();
        assert_eq!(f.reload("i1").await.spec.node, node);
        assert_eq!(f.reload("i1").await.meta.generation, i.meta.generation + 1);
    }

    #[tokio::test]
    async fn anti_affinity_is_read_off_the_instances_that_exist() {
        let f = fixture();
        f.node("a", 16384, true).await;
        let scheduler = f.scheduler();

        let mut first = f.instance("i1", 2048).await;
        first.spec.placement_policy.anti_affinity_group = Some("web".into());
        first.meta.generation += 1;
        f.instances
            .update(&first, &Writer::controller("test"))
            .await
            .unwrap();
        let first = f.reload("i1").await;
        scheduler
            .reconcile(&first.meta.name.to_string(), Some(&first))
            .await
            .unwrap();

        let mut second = f.instance("i2", 2048).await;
        second.spec.placement_policy.anti_affinity_group = Some("web".into());
        second.meta.generation += 1;
        f.instances
            .update(&second, &Writer::controller("test"))
            .await
            .unwrap();
        let second = f.reload("i2").await;
        scheduler
            .reconcile(&second.meta.name.to_string(), Some(&second))
            .await
            .unwrap();

        let after = f.reload("i2").await;
        assert!(
            after.spec.node.is_none(),
            "an availability group landed on one host"
        );
        assert!(
            condition(&after.status.conditions, "Ready")
                .unwrap()
                .message
                .contains("web")
        );
    }
}
