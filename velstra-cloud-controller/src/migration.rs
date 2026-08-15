//! The one spec write in a migration, and the sentence it carries.
//!
//! Almost nothing about moving a guest is a controller's business. The
//! destination starts a receiver, the source sends, and both of them report what
//! they can see on their own machine — that is
//! [`velstra_cloud_model::migration`], and none of it is here. What is here is
//! the single moment neither agent may perform for itself: moving
//! `instance.spec.node` from the source to the destination. It is a `spec`
//! write, so by invariant 1 it belongs to a controller, and it happens at
//! exactly one instant — when the source has reported that the guest is no
//! longer on it.
//!
//! Moving it earlier tells the destination to claim an instance the source is
//! still running. Moving it later leaves a guest that no node is assigned. The
//! model pins the instant in [`should_reassign`]; this file does not get to
//! decide it, only to carry it out.
//!
//! **That is the whole of it.** This controller writes no conditions and
//! enforces no deadline, and both absences are deliberate:
//!
//! * What a migration is doing is
//!   [`migration_condition`](velstra_cloud_model::migration::migration_condition)
//!   — a pure function of the migration and the instance, computed by the API
//!   when somebody reads one, the same shape as an operation's `done`. A
//!   controller writing it would be the second writer on a status the
//!   destination owns, and a *stored* condition is worst in the case an
//!   operator needs it most: a migration whose destination agent has died can
//!   no longer write anything, and that is exactly when somebody wants to be
//!   told it timed out.
//! * The transfer's own deadline is `spec.timeout_s`, which is handed to the
//!   VMM with the send. The thing that can stop a transfer is the thing running
//!   it; a controller re-deciding it from outside would be a second opinion
//!   about a clock it does not hold.
//!
//! So a settled migration — arrived, abandoned, or still copying — costs this
//! controller exactly one read and no writes at all, which is what makes the
//! resync affordable.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    migration::{Migration, MigrationSpec, MigrationStatus, arrived, should_reassign},
    resources::{InstanceSpec, InstanceStatus},
};
use velstra_cloud_store::{TypedStore, prefix_for};

use crate::{Related, Result, runner::Reconciler};

/// The name this controller writes under. It appears in a store refusal, so it
/// is the word an operator sees when a write of ours was wrong.
const WHO: &str = "migration";

/// Note what this does **not** hold: a
/// [`StatusWriter`](crate::status::StatusWriter). It has no business writing a
/// migration's status at all, and not carrying the means to is the cheapest way
/// to keep it that way.
pub struct MigrationController {
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    cell: String,
    /// Instance name to the migrations that are about it, learned as migrations
    /// are reconciled. Only ever an optimisation — the handover is triggered by
    /// the *instance* changing, and without this the only thing that would
    /// notice is the resync. A migration missing from here is found one resync
    /// later, which costs latency and never correctness.
    watching: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
}

impl MigrationController {
    pub fn new(instances: TypedStore<InstanceSpec, InstanceStatus>, cell: &str) -> Self {
        Self {
            instances,
            cell: cell.to_string(),
            watching: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn forget(&self, name: &str) {
        self.watching.lock().unwrap().values_mut().for_each(|set| {
            set.remove(name);
        });
    }

    fn wake_me_when(&self, instance: &str, migration: &str) {
        self.watching
            .lock()
            .unwrap()
            .entry(instance.to_string())
            .or_default()
            .insert(migration.to_string());
    }
}

impl Reconciler for MigrationController {
    type Spec = MigrationSpec;
    type Status = MigrationStatus;

    fn name(&self) -> &'static str {
        "migration"
    }

    fn related(&self) -> Vec<Related> {
        // The handover is triggered by the source letting go of the instance,
        // which is a write to the *instance*. Watching only migrations would
        // mean the one moment this controller exists for arrives at resync
        // speed — minutes of a guest belonging to nobody.
        let watching = self.watching.clone();
        vec![Related {
            prefix: prefix_for(&self.cell, "instances"),
            map: Arc::new(move |instance: &str| {
                watching
                    .lock()
                    .unwrap()
                    .get(instance)
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default()
            }),
        }]
    }

    async fn reconcile(&self, name: &str, object: Option<&Migration>) -> Result<()> {
        let Some(migration) = object else {
            self.forget(name);
            return Ok(());
        };
        // Being taken away is the agents' half: the source cancels and keeps the
        // guest, the destination tears its receiver down. There is no spec write
        // to make and nothing to say about an object on its way out.
        if migration.meta.is_deleting() {
            self.forget(name);
            return Ok(());
        }
        self.wake_me_when(&migration.spec.instance, name);

        let instance = self.instances.get(&migration.spec.instance).await?;

        if let Some(instance) = &instance
            && should_reassign(migration, instance)
        {
            let mut next = instance.clone();
            next.spec.node = Some(migration.spec.to_node.clone());
            // A spec change no agent can notice is not a spec change: the
            // destination compares generations to know there is something new
            // for it.
            next.meta.generation += 1;
            // The compare-and-swap is the whole race protocol. Two controllers
            // holding the same copy produce one handover and one retry, and the
            // retry finds an instance that needs none.
            self.instances
                .update(&next, &Writer::controller(WHO))
                .await?;
            info!(
                migration = name,
                instance = %instance.meta.name,
                from = migration.spec.from_node,
                to = migration.spec.to_node,
                "the source let go; the instance is now assigned to the destination"
            );
        }

        if instance.map(|i| arrived(migration, &i)).unwrap_or(false) {
            // Nothing more can happen to it; stop waking it when its instance
            // moves.
            self.forget(name);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName, Revision, Timestamp},
        migration::MigrationMode,
        resources::{Instance, InstanceState, Resource},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const MIGRATION: &str = "projects/p1/migrations/m1";
    const INSTANCE: &str = "projects/p1/instances/i1";

    struct Fixture {
        raw: Arc<MemoryStore>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        migrations: TypedStore<MigrationSpec, MigrationStatus>,
    }

    fn fixture() -> (Fixture, MigrationController) {
        let raw = Arc::new(MemoryStore::new());
        let f = Fixture {
            instances: TypedStore::new(raw.clone(), "cell-1", "instances"),
            migrations: TypedStore::new(raw.clone(), "cell-1", "migrations"),
            raw: raw.clone(),
        };
        let controller = MigrationController::new(f.instances.clone(), "cell-1");
        (f, controller)
    }

    impl Fixture {
        /// A guest running on `on`, assigned there — the state a migration is
        /// asked for from.
        async fn instance(&self, on: &str) {
            let i = Resource::new(
                Meta::new(
                    ResourceName::parse(INSTANCE).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                InstanceSpec {
                    vcpus: 2,
                    memory_mib: 4096,
                    node: Some(on.to_string()),
                    ..Default::default()
                },
                InstanceStatus {
                    observed_generation: 1,
                    state: InstanceState::Running,
                    node: Some(on.to_string()),
                    ..Default::default()
                },
            );
            self.instances.create(&i).await.unwrap();
        }

        async fn migration(&self, from: &str, to: &str) -> Migration {
            let m = Resource::new(
                Meta::new(
                    ResourceName::parse(MIGRATION).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                MigrationSpec {
                    instance: INSTANCE.into(),
                    from_node: from.into(),
                    to_node: to.into(),
                    mode: MigrationMode::Live,
                    ..Default::default()
                },
                MigrationStatus::default(),
            );
            self.migrations.create(&m).await.unwrap();
            self.reload_migration().await
        }

        async fn reload_migration(&self) -> Migration {
            self.migrations.get(MIGRATION).await.unwrap().unwrap()
        }

        async fn reload_instance(&self) -> Instance {
            self.instances.get(INSTANCE).await.unwrap().unwrap()
        }

        /// The source reports that the guest is no longer on it — the one event
        /// the handover waits for, written by the party that can see it.
        async fn source_lets_go(&self) {
            let mut i = self.reload_instance().await;
            i.status.node = None;
            i.status.state = InstanceState::Unknown;
            self.instances
                .update(&i, &Writer::agent("node-a"))
                .await
                .expect("the owning node reporting that it let go");
        }

        /// The destination claims the migration, as an agent does when it starts
        /// a receiver.
        async fn destination_claims(&self, node: &str) {
            let mut m = self.reload_migration().await;
            m.status.node = Some(node.to_string());
            m.status.receiver_url = Some("tcp:10.0.0.2:9000".into());
            m.status.receiver_ready = true;
            self.migrations
                .update(&m, &Writer::agent(node))
                .await
                .expect("the assignee claiming an object nobody holds");
        }

        async fn revision(&self) -> Revision {
            self.raw.revision().await.unwrap()
        }
    }

    #[tokio::test]
    async fn the_assignment_moves_only_after_the_source_has_let_go() {
        // The failure this prevents is the worst one in the whole dance: the
        // destination is told to claim a guest that is still running on the
        // source, and for a moment two hypervisors believe they have it.
        let (f, controller) = fixture();
        f.instance("node-a").await;
        let m = f.migration("node-a", "node-b").await;

        controller.reconcile(MIGRATION, Some(&m)).await.unwrap();
        assert_eq!(
            f.reload_instance().await.spec.node.as_deref(),
            Some("node-a"),
            "the instance was handed over while the source still had the guest"
        );

        f.source_lets_go().await;
        let m = f.reload_migration().await;
        let before = f.reload_instance().await;
        controller.reconcile(MIGRATION, Some(&m)).await.unwrap();

        let handed_over = f.reload_instance().await;
        assert_eq!(handed_over.spec.node.as_deref(), Some("node-b"));
        assert_eq!(
            handed_over.meta.generation,
            before.meta.generation + 1,
            "a spec change no agent can notice is not a spec change"
        );
    }

    #[tokio::test]
    async fn a_migration_that_has_already_moved_the_assignment_moves_nothing_again() {
        let (f, controller) = fixture();
        f.instance("node-a").await;
        f.migration("node-a", "node-b").await;
        f.source_lets_go().await;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .unwrap();

        let generation = f.reload_instance().await.meta.generation;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .unwrap();
        assert_eq!(
            f.reload_instance().await.meta.generation,
            generation,
            "the handover was performed twice, and every agent was told to redo its work"
        );
    }

    #[tokio::test]
    async fn a_finished_migration_costs_nothing_to_look_at_again() {
        // The property that makes the resync affordable. A settled cell that
        // writes once per object per pass is a cell whose store load grows with
        // its size, for no information at all — and since what a migration is
        // doing is computed when somebody reads it, an arrived one gives this
        // controller nothing to do on any pass, including the first.
        let (f, controller) = fixture();
        f.instance("node-b").await;
        f.migration("node-a", "node-b").await;

        for pass in 1..=2 {
            let revision = f.revision().await;
            controller
                .reconcile(MIGRATION, Some(&f.reload_migration().await))
                .await
                .unwrap();
            assert_eq!(
                f.revision().await,
                revision,
                "pass {pass} over a finished migration wrote something"
            );
        }
    }

    #[tokio::test]
    async fn a_migration_that_ran_out_of_time_is_left_entirely_alone() {
        // The deadline belongs to the VMM doing the sending, and what the
        // migration *says* about it is computed on read. There is nothing here
        // to enforce and nothing to write down — and above all nothing to
        // repair: under pre-copy the source still has the guest, so a timeout
        // that moved anything would be the one thing worse than the timeout.
        let (f, controller) = fixture();
        f.instance("node-a").await;
        let mut m = f.migration("node-a", "node-b").await;
        // Created an hour ago with a one-minute budget: a transfer that was
        // never going to converge.
        m.meta.created_at = Timestamp(Timestamp::now().0 - 3_600_000);
        m.spec.timeout_s = 60;
        m.meta.generation += 1;
        f.migrations
            .update(&m, &Writer::controller("test"))
            .await
            .unwrap();

        let revision = f.revision().await;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .unwrap();
        assert_eq!(
            f.revision().await,
            revision,
            "an overdue migration was written to"
        );

        let instance = f.reload_instance().await;
        assert_eq!(instance.spec.node.as_deref(), Some("node-a"));
        assert_eq!(instance.status.node.as_deref(), Some("node-a"));
        assert_eq!(instance.status.state, InstanceState::Running);
    }

    #[tokio::test]
    async fn the_controller_never_writes_a_migration_at_all() {
        // Invariant 1, at the one place in the system where it was genuinely
        // tempting to break it. The destination owns this object's status from
        // the moment it claims it; the controller has one job on the *instance*
        // and no business here, claimed or not.
        let (f, controller) = fixture();
        f.instance("node-a").await;
        f.migration("node-a", "node-b").await;
        f.destination_claims("node-b").await;

        let revision = f.revision().await;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .expect("a claimed migration is an ordinary object, not an error");
        assert_eq!(
            f.revision().await,
            revision,
            "the controller wrote a status the destination agent owns"
        );

        // …and the spec write it *does* own still happens.
        f.source_lets_go().await;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .unwrap();
        assert_eq!(
            f.reload_instance().await.spec.node.as_deref(),
            Some("node-b"),
            "the handover stopped happening because the destination had claimed the migration"
        );
    }

    #[tokio::test]
    async fn a_cancelled_migration_leaves_the_instance_where_it_is() {
        // Deleting a migration is how an operator abandons one, and pre-copy
        // makes that free: the source still has the guest. The handover must
        // not happen on the way out.
        let (f, controller) = fixture();
        f.instance("node-a").await;
        f.migration("node-a", "node-b").await;
        f.source_lets_go().await;

        let mut m = f.reload_migration().await;
        m.meta.deleted_at = Some(Timestamp::now());
        f.migrations
            .update(&m, &Writer::controller("api"))
            .await
            .unwrap();

        let revision = f.revision().await;
        controller
            .reconcile(MIGRATION, Some(&f.reload_migration().await))
            .await
            .unwrap();
        assert_eq!(
            f.revision().await,
            revision,
            "a migration on its way out was still acted on"
        );
        assert_eq!(
            f.reload_instance().await.spec.node.as_deref(),
            Some("node-a")
        );
    }

    #[tokio::test]
    async fn a_migration_whose_instance_does_not_exist_is_not_a_failure_here() {
        // Objects in a level-triggered system arrive in any order, and one that
        // names something that is not there yet is ordinary. Saying so is the
        // read path's job — `migration_condition` answers `NoSuchInstance` —
        // and backing this object off would only delay the pass that finds it.
        let (f, controller) = fixture();
        let m = f.migration("node-a", "node-b").await;
        let revision = f.revision().await;
        controller.reconcile(MIGRATION, Some(&m)).await.unwrap();
        assert_eq!(f.revision().await, revision);
    }

    #[tokio::test]
    async fn an_object_that_is_already_gone_is_not_an_error() {
        let (_, controller) = fixture();
        controller.reconcile(MIGRATION, None).await.unwrap();
    }
}
