//! Emptying a node that has been asked to give up its guests.
//!
//! `schedulable: false` says "nothing new here". `evacuate: true` says "and
//! none of the old either". They are separate fields because they are separate
//! intentions: an operator taking a machine out for an hour wants the first
//! without the second, and conflating them would move a fleet for a reboot.
//!
//! ## What this creates, and what it does not
//!
//! One `Migration` per guest that can move. Nothing else — the migration
//! machinery already knows how to move a guest, and a second path that also
//! moved guests would be a second set of rules about when that is safe.
//!
//! A guest that cannot move is left where it is. Some never can: one holding a
//! passed-through device is bound to that machine. A node that refused to drain
//! the rest because of it would be worse than one that moves what it can and
//! leaves the reason answerable at `:explainMigration`.
//!
//! ## Level-triggered, like everything else
//!
//! Every pass asks the same question — "which guests are still here, and where
//! could each go" — and creates what is missing. A guest with a migration
//! already in flight is not offered again, so a controller that dies mid-pass
//! computes the same list next time minus what already happened. Turning
//! `evacuate` off stops further moves; it does not bring anything back, because
//! a migration that has started is a thing to see through rather than a state
//! to unwind.

use tracing::{info, warn};
use velstra_cloud_model::{
    access::Writer,
    meta::{Meta, ResourceName},
    migration::{MigrationMode, MigrationSpec, MigrationStatus, evacuate},
    resources::{
        Instance, InstanceSpec, InstanceState, InstanceStatus, Node, NodeSpec, NodeStatus, Resource,
    },
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "evacuation";

pub struct EvacuationController {
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    nodes: TypedStore<NodeSpec, NodeStatus>,
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    /// Read only to turn an image's name into the name its bytes are filed
    /// under. The aggregate itself still comes from the nodes.
    images: TypedStore<
        velstra_cloud_model::resources::ImageSpec,
        velstra_cloud_model::resources::ImageStatus,
    >,
    /// The cell's maintenance windows. A window with `drain` set says the same
    /// thing `spec.evacuate` says, for a stretch of time somebody chose in
    /// advance — and it says it without writing that field, so the operator
    /// goes on being its only writer and a window that closes takes nothing of
    /// theirs with it.
    windows: Option<
        TypedStore<
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus,
        >,
    >,
}

impl EvacuationController {
    pub fn new(
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        nodes: TypedStore<NodeSpec, NodeStatus>,
        migrations: TypedStore<MigrationSpec, MigrationStatus>,
        images: TypedStore<
            velstra_cloud_model::resources::ImageSpec,
            velstra_cloud_model::resources::ImageStatus,
        >,
    ) -> Self {
        Self {
            instances,
            nodes,
            migrations,
            images,
            windows: None,
        }
    }

    /// Give this controller the cell's maintenance windows.
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

    /// Whether an open window is asking this node to empty right now.
    ///
    /// `false` when the windows cannot be read. A failed read must not start
    /// moving a fleet: the cost of missing a drain is that it happens on the
    /// next pass, and the cost of inventing one is a night of migrations
    /// nobody asked for.
    async fn window_is_draining(&self, node: &str) -> bool {
        let Some(store) = &self.windows else {
            return false;
        };
        match store.list().await {
            Ok(all) => velstra_cloud_model::maintenance::draining(
                node,
                &all.iter()
                    .map(crate::scheduler::window_view)
                    .collect::<Vec<_>>(),
                velstra_cloud_model::meta::Timestamp::now(),
            ),
            Err(e) => {
                warn!(error = %e, node, "could not read this cell's maintenance windows");
                false
            }
        }
    }
}

/// The name for the migration that empties one guest off one node.
///
/// Derived from the guest and the node it is leaving, so two controllers on the
/// same pass ask for the *same* object and the second one's create is refused
/// as a duplicate. A random name would give one guest two transfers, and the
/// loser of that race is a machine nobody can account for.
fn migration_name(instance: &ResourceName, from: &str) -> String {
    format!("{}-off-{}", instance.id(), from)
}

impl Reconciler for EvacuationController {
    type Spec = NodeSpec;
    type Status = NodeStatus;

    fn name(&self) -> &'static str {
        "evacuation"
    }

    async fn reconcile(&self, name: &str, object: Option<&Node>) -> Result<()> {
        let Some(node) = object else {
            return Ok(());
        };
        if node.meta.is_deleting() {
            return Ok(());
        }
        let here = node.meta.name.id().to_string();
        // Not asked to. Nearly every node in nearly every cell takes this path,
        // so the field is read first and the windows only when it is unset —
        // one read and no writes for a cell where nothing is being emptied.
        if !node.spec.evacuate && !self.window_is_draining(&here).await {
            return Ok(());
        }

        let all = self.instances.list().await?;
        let mine: Vec<&Instance> = all
            .iter()
            .filter(|i| {
                i.status.node.as_deref() == Some(here.as_str())
                    && i.status.state == InstanceState::Running
                    && !i.meta.is_deleting()
            })
            .collect();
        if mine.is_empty() {
            return Ok(());
        }

        let nodes = self.nodes.list().await?;
        let others: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.meta.name.id() != here && !n.meta.is_deleting())
            .collect();

        // Which nodes hold which image, added up from what each node reports
        // about itself. Never the image collection: which nodes hold a copy is
        // an aggregate, and an aggregate is not a fact anybody owns.
        // Through the image object, because a node files bytes under their
        // digest and an object's name is a name. Comparing the two directly
        // answered "nobody has it" for every image, and an evacuation then moved
        // nothing at all — the machine stayed full with the window open.
        let images = self.images.list().await?;
        let cached = |image: &str| {
            images
                .iter()
                .find(|i| i.meta.name.to_string() == image)
                .and_then(|i| velstra_cloud_model::images::stored_name(&i.spec.digest))
                .map(|stored| velstra_cloud_model::resources::nodes_holding(&stored, &nodes))
                .unwrap_or_default()
        };

        let migrations = self.migrations.list().await?;
        let moving: Vec<String> = migrations
            .iter()
            .filter(|m| !m.meta.is_deleting())
            .map(|m| m.spec.instance.clone())
            .collect();

        let (going, stranded) = evacuate(node, &mine, &others, &cached, &moving);

        for handover in going {
            let Ok(instance_name) = ResourceName::parse(&handover.instance) else {
                continue;
            };
            let Some(project) = instance_name.parent() else {
                continue;
            };
            let id = migration_name(&instance_name, &here);
            let full = format!("{project}/migrations/{id}");
            let Ok(migration_name) = ResourceName::parse(&full) else {
                continue;
            };
            let asked = Resource::new(
                Meta::new(migration_name, node.meta.placement.clone()),
                MigrationSpec {
                    instance: handover.instance.clone(),
                    from_node: here.clone(),
                    to_node: handover.to_node.clone(),
                    // Live, because emptying a node is maintenance and the
                    // point of maintenance is that nobody notices. A guest
                    // that cannot move live is one of the stranded ones below,
                    // with the reason on it.
                    mode: MigrationMode::Live,
                    ..MigrationSpec::default()
                },
                MigrationStatus::default(),
            );
            match self
                .migrations
                .create(&asked, &Writer::controller(WHO))
                .await
            {
                Ok(_) => info!(
                    node = %name,
                    instance = %handover.instance,
                    to = %handover.to_node,
                    "moving a guest off a node being emptied"
                ),
                // Already asked for, by an earlier pass or another controller.
                // The derived name is what makes that harmless.
                Err(e) if is_taken(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }

        for stuck in stranded {
            // Said once per pass rather than written onto the guest: the agent
            // on this node owns that status, and `:explainMigration` answers
            // the same question on demand and cannot go stale.
            warn!(
                node = %name,
                instance = %stuck.instance,
                refused_by = stuck.refusals.len(),
                "a guest cannot be moved off a node being emptied; ask :explainMigration why"
            );
        }
        Ok(())
    }
}

fn is_taken(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Exists { .. }
        )
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Condition, Placement, Timestamp, set_condition},
        resources::{Capacity, NodeStatus},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    struct Fixture {
        nodes: TypedStore<NodeSpec, NodeStatus>,
        migrations: TypedStore<MigrationSpec, MigrationStatus>,
        windows: TypedStore<
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus,
        >,
        controller: EvacuationController,
    }

    async fn fixture(evacuating: bool, guest_holds_device: bool) -> Fixture {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let instances: TypedStore<InstanceSpec, InstanceStatus> =
            TypedStore::new(store.clone(), "cell-1", "instances");
        let nodes: TypedStore<NodeSpec, NodeStatus> =
            TypedStore::new(store.clone(), "cell-1", "nodes");
        let migrations: TypedStore<MigrationSpec, MigrationStatus> =
            TypedStore::new(store.clone(), "cell-1", "migrations");

        for (id, evac) in [("node-a", evacuating), ("node-b", false)] {
            let mut n: Node = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                NodeSpec {
                    evacuate: evac,
                    vcpu_overcommit: 0,
                    fence_after_s: 0,
                    schedulable: !evac,
                    labels: vec![],
                    cpu_baseline: None,
                    gateway: false,
                },
                NodeStatus {
                    // Both machines on one state directory. Emptying a node is
                    // only possible where a guest can leave its machine at all,
                    // and what happens when it cannot has its own test below.
                    shared_state: true,
                    vmm: "qemu".into(),
            fetching: Vec::new(),
                    capacity: Capacity {
                        vcpus: 32,
                        memory_mib: 65536,
                        disk_gib: 1000,
                        numa_free_mib: vec![65536],
                        hugepages_1gi: 0,
                    },
                    agent_version: "1.0.0".into(),
                    last_heartbeat: Timestamp::now(),
                    // Both hold the image, so the move is not refused for a
                    // reason this check is not about.
                    images: vec!["sha256-392d11b010cde76dc82ebe107aa399feff105625fd332b1ba58624426fcba7ca".into()],
                    cpu: Some(velstra_cloud_model::cpu::NodeCpu {
                        arch: "x86_64".into(),
                        flags: ["sse4_2"].iter().map(|s| s.to_string()).collect(),
                        presents: "host".into(),
                        presented_flags: ["sse4_2"].iter().map(|s| s.to_string()).collect(),
                        can_mask: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            set_condition(&mut n.status.conditions, Condition::ready(1));
            nodes.create(&n, &Writer::controller("test")).await.unwrap();
        }

        let mut i: Instance = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                node: Some("node-a".into()),
                vcpus: 2,
                memory_mib: 4096,
                image: "projects/p1/images/sha256-abc".into(),
                root_disk_gib: 20,
                ..Default::default()
            },
            InstanceStatus {
                state: InstanceState::Running,
                node: Some("node-a".into()),
                devices: if guest_holds_device {
                    vec!["0000:41:00.0".into()]
                } else {
                    Vec::new()
                },
                cpu: Some(velstra_cloud_model::cpu::GuestCpu {
                    model: "host".into(),
                    arch: "x86_64".into(),
                    flags: ["sse4_2"].iter().map(|s| s.to_string()).collect(),
                }),
                ..Default::default()
            },
        );
        i.status.observed_generation = i.meta.generation;
        instances
            .create(&i, &Writer::controller("test"))
            .await
            .unwrap();

        let windows: TypedStore<
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus,
        > = TypedStore::new(store.clone(), "cell-1", "maintenance-windows");

        // The image object itself, because the aggregate is now taken through it:
        // a node files bytes under their digest, and the name is a name.
        let images: TypedStore<
            velstra_cloud_model::resources::ImageSpec,
            velstra_cloud_model::resources::ImageStatus,
        > = TypedStore::new(store.clone(), "cell-1", "images");
        images
            .create(
                &velstra_cloud_model::resources::Image::new(
                    Meta::new(
                        "projects/p1/images/sha256-abc".parse().unwrap(),
                        Placement::new("eu", "cell-1"),
                    ),
                    velstra_cloud_model::resources::ImageSpec {
                        digest: "sha256:392d11b010cde76dc82ebe107aa399feff105625fd332b1ba58624426fcba7ca".into(),
                        ..Default::default()
                    },
                    Default::default(),
                ),
                &velstra_cloud_model::Writer::controller("test"),
            )
            .await
            .unwrap();

        Fixture {
            nodes: nodes.clone(),
            migrations: migrations.clone(),
            windows: windows.clone(),
            controller: EvacuationController::new(instances, nodes, migrations, images)
                .with_maintenance(windows),
        }
    }

    /// Declare a window over node-a, open now.
    async fn declare(f: &Fixture, drain: bool) {
        let w: velstra_cloud_model::resources::MaintenanceWindow = Resource::new(
            Meta::new(
                ResourceName::parse("maintenance-windows/dimm-swap").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            velstra_cloud_model::maintenance::MaintenanceWindowSpec {
                node: "node-a".into(),
                // A minute ago, so it is open without depending on how long the
                // test takes to get here.
                starts_at: Timestamp(Timestamp::now().0.saturating_sub(60_000)),
                minutes: 60,
                drain,
                note: "swapping the failed DIMM in slot 3".into(),
            },
            Default::default(),
        );
        f.windows
            .create(&w, &Writer::controller("test"))
            .await
            .unwrap();
    }

    impl Fixture {
        async fn pass(&self) {
            let n = self.nodes.get("nodes/node-a").await.unwrap().unwrap();
            self.controller
                .reconcile("nodes/node-a", Some(&n))
                .await
                .unwrap();
        }

        async fn asked(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .migrations
                .list()
                .await
                .unwrap()
                .into_iter()
                .map(|m| format!("{} -> {}", m.spec.instance, m.spec.to_node))
                .collect();
            out.sort();
            out
        }
    }

    /// One migration per guest, and a second pass asks for nothing.
    ///
    /// The second half is what makes this level-triggered rather than a job: a
    /// loop that runs again in a second must not start a second transfer of a
    /// machine it is already moving.
    #[tokio::test]
    async fn a_node_being_emptied_asks_once_per_guest() {
        let f = fixture(true, false).await;

        f.pass().await;
        assert_eq!(f.asked().await, ["projects/p1/instances/i1 -> node-b"]);

        f.pass().await;
        assert_eq!(
            f.asked().await.len(),
            1,
            "a second pass started a second transfer of the same guest"
        );
    }

    /// A node nobody asked to empty writes nothing.
    #[tokio::test]
    async fn a_node_that_was_not_asked_to_empty_is_left_alone() {
        let f = fixture(false, false).await;
        f.pass().await;
        assert!(
            f.asked().await.is_empty(),
            "a node that was merely draining had its guests moved"
        );
    }

    /// The point of the whole feature: nobody flipped `evacuate`, and the node
    /// empties anyway because the window somebody declared last week is open.
    ///
    /// And it empties *without that field being written*. The operator goes on
    /// being its only writer, so a window that closes takes nothing of theirs
    /// with it — there is nothing to unwind and nothing left flipped if this
    /// controller died in the middle.
    #[tokio::test]
    async fn an_open_window_that_asks_for_a_drain_empties_the_node_on_its_own() {
        let f = fixture(false, false).await;
        declare(&f, true).await;

        f.pass().await;
        assert_eq!(f.asked().await, ["projects/p1/instances/i1 -> node-b"]);

        let node = f.nodes.get("nodes/node-a").await.unwrap().unwrap();
        assert!(
            !node.spec.evacuate,
            "the controller wrote the operator's own field to make this happen"
        );
    }

    /// The distinction the `drain` field exists for. A window without it says
    /// "nothing new here"; a controller that read it as "and none of the old
    /// either" would move a fleet for a firmware update.
    #[tokio::test]
    async fn a_window_that_does_not_ask_for_a_drain_moves_nothing() {
        let f = fixture(false, false).await;
        declare(&f, false).await;
        f.pass().await;
        assert!(
            f.asked().await.is_empty(),
            "a four-minute firmware window started migrating guests"
        );
    }

    /// A guest bound to its machine is left, and the rest of the node still
    /// empties.
    #[tokio::test]
    async fn a_guest_holding_hardware_is_left_where_it_is() {
        let f = fixture(true, true).await;
        f.pass().await;
        assert!(
            f.asked().await.is_empty(),
            "a guest holding a PCI device was asked to migrate"
        );
    }
    #[tokio::test]
    async fn is_not_emptied_by_asking_for_the_impossible() {
        // The finding this whole rule came from, at the place it would have hurt
        // most. A guest's root disk is a file on the machine it runs on, and
        // moving a guest transfers memory and not disks — so on an ordinary
        // cell, every migration this controller creates is one the destination
        // can never complete. The receiver answers `has no root disk on this
        // node`, once a pass, for ever.
        //
        // Which means an operator opening a maintenance window on a real cell
        // would have got: a node that never empties, one migration per guest
        // that never finishes, and nothing at all said about why. Emptying a
        // machine is the moment somebody is *waiting*, usually with a
        // maintenance window booked around it.
        //
        // Nothing is asked for now. The guest stays, and the reason is
        // answerable at `:explainMigration` like every other refusal.
        let f = fixture(false, false).await;
        for id in ["node-a", "node-b"] {
            let mut node = f.nodes.get(&format!("nodes/{id}")).await.unwrap().unwrap();
            node.status.shared_state = false;
            f.nodes
                .update(&node, &Writer::agent(id))
                .await
                .unwrap();
        }
        declare(&f, true).await;

        f.pass().await;
        assert_eq!(
            f.asked().await,
            Vec::<String>::new(),
            "a migration was created that no destination could ever complete"
        );
    }
}
