//! What a project has in use, counted rather than tracked.
//!
//! This controller never adds and never subtracts. It lists what exists, counts
//! it, and writes the total if it differs from what is stored. That is the
//! whole implementation, and it is the entire reason quota here cannot drift:
//! there is no increment to lose when a process dies between creating an
//! instance and charging for it, and no decrement to lose when one is deleted.
//!
//! The cost is a list per project reconcile, which is honest at the scale of a
//! cell and would need an index at the scale of a region. The failure mode of
//! the cheap version is a slow controller; the failure mode of the clever
//! version is a project that cannot start anything because a counter says it is
//! full and nothing in the system can prove otherwise.

use velstra_cloud_model::{
    backup::{BackupSpec, BackupStatus},
    loadbalancer::{LoadBalancerSpec, LoadBalancerStatus},
    meta::{Meta, ResourceName, Timestamp, set_condition},
    reconcile::{count_quota, quota_condition},
    resources::{
        FloatingIpSpec, FloatingIpStatus, InstanceSpec, InstanceStatus, Project, ProjectSpec,
        ProjectStatus, Quota, Resource, SnapshotSpec, SnapshotStatus, VolumeSpec, VolumeStatus,
    },
    usage::{UsageRecordSpec, UsageRecordStatus},
};
use velstra_cloud_store::{Cached, Store, TypedStore, prefix_for};

/// Who this controller writes as. Named once: a reading and a status written by
/// two different-looking parties would be two writers on one project.
const WRITER: &str = "quota";

use crate::{Related, Result, runner::Reconciler, status::StatusWriter};

pub struct QuotaController {
    /// Cached rather than listed. Usage is counted per *project*, and a count
    /// that lists the whole collection per project is projects × instances per
    /// resync — measured at 16 040 reads for 400 instances over 40 projects in
    /// `tests/scaling.rs`, against 42 at a twentieth the size. The same wall the
    /// port controller hit, found by the same test, fixed the same way.
    instances: Cached<InstanceSpec, InstanceStatus>,
    volumes: Cached<VolumeSpec, VolumeStatus>,
    floating: Cached<FloatingIpSpec, FloatingIpStatus>,
    balancers: Cached<LoadBalancerSpec, LoadBalancerStatus>,
    /// Snapshots and backups, for the two dimensions that are counted from
    /// what a pool reported rather than from what somebody asked for.
    snapshots: Cached<SnapshotSpec, SnapshotStatus>,
    backups: Cached<BackupSpec, BackupStatus>,
    status: StatusWriter<ProjectSpec, ProjectStatus>,
    /// Where a reading of what this project had goes, when one is taken.
    ///
    /// `None` on a cell that records none — a developer cell, or one whose
    /// operator turned it off. A controller with nowhere to write a reading
    /// takes none rather than counting into the void.
    usage: Option<TypedStore<UsageRecordSpec, UsageRecordStatus>>,
    /// How often a reading is taken, and how long one is kept.
    interval_ms: u64,
    retention_ms: u64,
    cell: String,
}

impl QuotaController {
    /// Eight arguments, six of them the collections a quota is counted from.
    /// A struct holding them would exist to be built at the one call site that
    /// has them and taken apart here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instances: Cached<InstanceSpec, InstanceStatus>,
        volumes: Cached<VolumeSpec, VolumeStatus>,
        floating: Cached<FloatingIpSpec, FloatingIpStatus>,
        balancers: Cached<LoadBalancerSpec, LoadBalancerStatus>,
        snapshots: Cached<SnapshotSpec, SnapshotStatus>,
        backups: Cached<BackupSpec, BackupStatus>,
        status: StatusWriter<ProjectSpec, ProjectStatus>,
        cell: &str,
    ) -> Self {
        Self {
            instances,
            volumes,
            floating,
            balancers,
            snapshots,
            backups,
            status,
            usage: None,
            interval_ms: velstra_cloud_model::usage::INTERVAL_MS,
            retention_ms: velstra_cloud_model::usage::RETENTION_MS,
            cell: cell.to_string(),
        }
    }

    /// Also write down what each project had, so somebody can bill for it.
    ///
    /// Off unless asked for: a cell that nobody bills does not need the rows,
    /// and a controller that wrote them anyway would be charging a developer's
    /// laptop for storage.
    pub fn recording_usage(mut self, store: std::sync::Arc<dyn Store>) -> Self {
        self.usage = Some(TypedStore::new(store, &self.cell, "usage"));
        self
    }

    /// The interval readings are filed under and how long they are kept, for a
    /// test that cannot wait an hour and for an operator who bills by the
    /// minute.
    pub fn every(mut self, interval_ms: u64, retention_ms: u64) -> Self {
        self.interval_ms = interval_ms;
        self.retention_ms = retention_ms;
        self
    }

    /// Write down what this project has, once per interval.
    ///
    /// Filed under the interval it falls in rather than the instant this ran,
    /// so two controllers reconciling the same project a second apart write
    /// **one** record: the id is the same and the second create is refused as a
    /// duplicate. There is no leader election here because there does not need
    /// to be one.
    ///
    /// Best effort on purpose. A reading that could not be written is a row
    /// missing from a bill, which is a thing to notice; a reading that could
    /// not be written and took the quota count down with it would be a cell
    /// that stops enforcing limits because its accountant is unwell.
    async fn record(&self, project: &Project, used: &Quota, carried: (u64, u64), now: Timestamp) {
        let Some(store) = &self.usage else {
            return;
        };
        let at = velstra_cloud_model::usage::window_of(now, self.interval_ms);
        let id = velstra_cloud_model::usage::id_for(at);
        let name = format!("{}/usage/{id}", project.meta.name);
        let Ok(name) = ResourceName::parse(&name) else {
            return;
        };
        // The reading before this one, for the traffic. Read rather than
        // remembered: a controller that kept a table would forget it on every
        // restart and bill a month's traffic as one hour's.
        let previous = self
            .newest_reading(&project.meta.name.to_string(), at)
            .await;
        let traffic = velstra_cloud_model::usage::Traffic::between(
            previous.as_ref().map(|r| &r.spec.traffic),
            carried.0,
            carried.1,
        );
        let record = Resource::new(
            Meta::new(name, project.meta.placement.clone()),
            UsageRecordSpec {
                project: project.meta.name.to_string(),
                at,
                used: used.clone(),
                traffic,
            },
            UsageRecordStatus::default(),
        );
        match store
            .create(
                &record,
                &velstra_cloud_model::access::Writer::controller(WRITER),
            )
            .await
        {
            Ok(_) => {}
            // Already there: another pass, or another controller, took this
            // window's reading. That is the design working, not a failure.
            Err(e) if e.to_string().contains("exists") => {}
            Err(e) => tracing::warn!(project = %project.meta.name, error = %e,
                                     "this project's usage was not written down"),
        }
        self.prune(project, now).await;
    }

    /// The newest reading of this project taken strictly before `at`.
    ///
    /// `None` when there is none, which is the first reading a project ever
    /// gets. The readings are filed under a zero-padded millisecond, so the
    /// lexical order of their names is time order and "the newest before this"
    /// is the last one that sorts below the id being written.
    async fn newest_reading(
        &self,
        project: &str,
        at: Timestamp,
    ) -> Option<velstra_cloud_model::resources::UsageRecord> {
        let store = self.usage.as_ref()?;
        let records = store.list().await.ok()?;
        let mine = format!("{project}/usage/");
        records
            .into_iter()
            .filter(|r| r.meta.name.to_string().starts_with(&mine) && r.spec.at.0 < at.0)
            .max_by_key(|r| r.spec.at.0)
    }

    /// Take away readings older than the retention.
    ///
    /// Here rather than in a sweep of its own, because the reconcile that adds
    /// a row is the natural place to drop one — a project nothing reconciles
    /// has stopped accumulating rows anyway.
    async fn prune(&self, project: &Project, now: Timestamp) {
        let Some(store) = &self.usage else {
            return;
        };
        let Ok(records) = store.list().await else {
            return;
        };
        let mine = format!("{}/usage/", project.meta.name);
        for record in records {
            let name = record.meta.name.to_string();
            if !name.starts_with(&mine) {
                continue;
            }
            if !velstra_cloud_model::usage::expired(record.spec.at, now, self.retention_ms) {
                continue;
            }
            let _ = store
                .delete(
                    &name,
                    record.meta.revision,
                    &velstra_cloud_model::access::Writer::controller(WRITER),
                )
                .await;
        }
    }

    /// Take away every reading of a project that no longer exists.
    ///
    /// Deleting a project deliberately leaves its records behind — they are
    /// what a bill is reconstructed from, and holding a project hostage to its
    /// own accounting was a real incident. But "left behind" was taken
    /// literally: nothing ever came back for them. This is the coming back.
    ///
    /// Not on a retention: the project is gone, so the readings are already
    /// past being about anything. An operator who needs the history takes it
    /// out before deleting, which is the same thing every cloud asks.
    async fn forget(&self, project: &str) {
        let Some(store) = &self.usage else {
            return;
        };
        let Ok(records) = store.list().await else {
            tracing::warn!(
                project,
                "could not read the usage to forget a deleted project's"
            );
            return;
        };
        let mine = format!("{project}/usage/");
        let mut gone = 0;
        for record in records {
            let name = record.meta.name.to_string();
            if !name.starts_with(&mine) {
                continue;
            }
            if store
                .delete(
                    &name,
                    record.meta.revision,
                    &velstra_cloud_model::access::Writer::controller(WRITER),
                )
                .await
                .is_ok()
            {
                gone += 1;
            }
        }
        if gone > 0 {
            tracing::info!(project, readings = gone, "forgot a deleted project's usage");
        }
    }
}

/// Take away readings belonging to projects that are gone.
///
/// A project deleted while this process was down leaves rows nothing
/// reconciles: the resync lists projects that *exist*, so the missing one is
/// never visited and [`QuotaController::forget`] is never called. This is the
/// sweep that closes that window, run from the same periodic pass the alerts
/// are — a free function rather than a method because the controller it would
/// hang off is owned by the runner by then.
pub async fn sweep_orphaned_usage(
    store: &TypedStore<UsageRecordSpec, UsageRecordStatus>,
    live: &[Project],
) -> usize {
    {
        let Ok(records) = store.list().await else {
            return 0;
        };
        let live: std::collections::BTreeSet<String> =
            live.iter().map(|p| p.meta.name.to_string()).collect();
        let mut gone = 0;
        for record in records {
            // The record's own field, not the name it is filed under: they
            // agree, and the field is what a bill is read from.
            if live.contains(&record.spec.project) {
                continue;
            }
            if store
                .delete(
                    &record.meta.name.to_string(),
                    record.meta.revision,
                    &velstra_cloud_model::access::Writer::controller(WRITER),
                )
                .await
                .is_ok()
            {
                gone += 1;
            }
        }
        if gone > 0 {
            tracing::info!(
                readings = gone,
                "swept usage belonging to projects that are gone"
            );
        }
        gone
    }
}

/// The project a resource is charged to, as a name.
fn owning_project(name: &str) -> Vec<String> {
    ResourceName::parse(name)
        .ok()
        .and_then(|n| n.project().map(|p| format!("projects/{p}")))
        .into_iter()
        .collect()
}

impl Reconciler for QuotaController {
    type Spec = ProjectSpec;
    type Status = ProjectStatus;

    fn name(&self) -> &'static str {
        "quota"
    }

    fn related(&self) -> Vec<Related> {
        // Usage is a fact about the objects, so the objects are what wakes it.
        // Leaving this to the resync alone would mean the API admits work
        // against a number that is up to a resync interval out of date, which
        // is exactly how a project overshoots its limit.
        [
            "instances",
            "volumes",
            "floatingips",
            "load-balancers",
            "snapshots",
            "backups",
        ]
        .into_iter()
        .map(|kind| Related::named(prefix_for(&self.cell, kind), owning_project))
        .collect()
    }

    async fn reconcile(&self, name: &str, object: Option<&Project>) -> Result<()> {
        let Some(project) = object else {
            // Gone. Its readings are not, and nothing else would ever look at
            // them again: pruning runs inside this reconcile, so a deleted
            // project's rows were kept for ever — the delete guard skips
            // records on purpose, so there was nothing left to notice them.
            self.forget(name).await;
            return Ok(());
        };

        let (instances, _) = self.instances.all().await;
        let (volumes, _) = self.volumes.all().await;
        let (floating, _) = self.floating.all().await;
        let (balancers, _) = self.balancers.all().await;
        let (snapshots, _) = self.snapshots.all().await;
        let (backups, _) = self.backups.all().await;
        let instances: Vec<_> = instances.iter().map(|i| (**i).clone()).collect();
        let volumes: Vec<_> = volumes.iter().map(|v| (**v).clone()).collect();
        let floating: Vec<_> = floating.iter().map(|f| (**f).clone()).collect();
        let balancers: Vec<_> = balancers.iter().map(|l| (**l).clone()).collect();
        let snapshots: Vec<_> = snapshots.iter().map(|s| (**s).clone()).collect();
        let backups: Vec<_> = backups.iter().map(|b| (**b).clone()).collect();
        let used = count_quota(
            &project.meta.name,
            &instances,
            &volumes,
            &floating,
            &balancers,
            &snapshots,
            &backups,
        );

        let mut next = project.clone();
        next.status.used = used;
        next.status.observed_generation = project.meta.generation;
        set_condition(
            &mut next.status.conditions,
            quota_condition(
                &project.spec.quota,
                &next.status.used,
                project.meta.generation,
            ),
        );
        self.status.write(project, &next).await?;
        // After the count is stored, not before: a reading is of what the
        // project *had*, and writing one from a count this pass has not yet
        // stood behind would put a number in a bill that the project's own
        // status disagrees with.
        // Every guest's lifetime traffic, summed. The node carries each one
        // across a tap that was remade, so this is a total that only ever goes
        // up — which is what makes the difference between two readings a
        // number somebody can be charged for.
        let carried = instances
            .iter()
            .filter(|i| i.meta.name.is_under(&project.meta.name))
            .filter_map(|i| i.status.usage.as_ref())
            .fold((0u64, 0u64), |(rx, tx), u| {
                (rx.saturating_add(u.rx_total), tx.saturating_add(u.tx_total))
            });
        self.record(project, &next.status.used, carried, Timestamp::now())
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        ConditionStatus,
        meta::{Meta, Placement, condition},
        resources::{Quota, Resource},
    };
    use velstra_cloud_store::{MemoryStore, Store, TypedStore};

    use super::*;

    struct Fixture {
        raw: Arc<MemoryStore>,
        projects: TypedStore<ProjectSpec, ProjectStatus>,
        instances: TypedStore<InstanceSpec, InstanceStatus>,
        volumes: TypedStore<VolumeSpec, VolumeStatus>,
        floating: TypedStore<FloatingIpSpec, FloatingIpStatus>,
    }

    async fn fixture(limit: Quota) -> (Fixture, QuotaController) {
        let raw = Arc::new(MemoryStore::new());
        let f = Fixture {
            projects: TypedStore::new(raw.clone(), "cell-1", "projects"),
            instances: TypedStore::new(raw.clone(), "cell-1", "instances"),
            volumes: TypedStore::new(raw.clone(), "cell-1", "volumes"),
            floating: TypedStore::new(raw.clone(), "cell-1", "floatingips"),
            raw: raw.clone(),
        };
        f.projects
            .create(
                &Resource::new(
                    Meta::new(
                        ResourceName::parse("projects/p1").unwrap(),
                        Placement::new("eu", "cell-1"),
                    ),
                    ProjectSpec {
                        policy: Default::default(),
                        display_name: "one".into(),
                        parent: "organizations/o1".into(),
                        quota: limit,
                        bindings: Vec::new(),
                        cell: String::new(),
                    },
                    ProjectStatus::default(),
                ),
                &velstra_cloud_model::access::Writer::controller("quota"),
            )
            .await
            .unwrap();
        let controller = QuotaController::new(
            Cached::start(
                f.instances.clone(),
                raw.clone(),
                prefix_for("cell-1", "instances"),
            ),
            Cached::start(
                f.volumes.clone(),
                raw.clone(),
                prefix_for("cell-1", "volumes"),
            ),
            Cached::start(
                f.floating.clone(),
                raw.clone(),
                prefix_for("cell-1", "floatingips"),
            ),
            Cached::start(
                TypedStore::<LoadBalancerSpec, LoadBalancerStatus>::new(
                    raw.clone(),
                    "cell-1",
                    "load-balancers",
                ),
                raw.clone(),
                prefix_for("cell-1", "load-balancers"),
            ),
            Cached::start(
                TypedStore::<
                    velstra_cloud_model::resources::SnapshotSpec,
                    velstra_cloud_model::resources::SnapshotStatus,
                >::new(raw.clone(), "cell-1", "snapshots"),
                raw.clone(),
                prefix_for("cell-1", "snapshots"),
            ),
            Cached::start(
                TypedStore::<BackupSpec, BackupStatus>::new(raw.clone(), "cell-1", "backups"),
                raw.clone(),
                prefix_for("cell-1", "backups"),
            ),
            StatusWriter::new(raw, "cell-1", "projects", "quota"),
            "cell-1",
        );
        (f, controller)
    }

    impl Fixture {
        async fn instance(
            &self,
            name: &str,
            vcpus: u32,
            memory_mib: u64,
        ) -> Resource<InstanceSpec, InstanceStatus> {
            let i = Resource::new(
                Meta::new(
                    ResourceName::parse(name).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                InstanceSpec {
                    start_order: 0,
                    start_delay_s: 0,
                    on_node_loss: Default::default(),
                    console: false,
                    devices: Vec::new(),
                    vcpus,
                    memory_mib,
                    root_disk_gib: 10,
                    ..Default::default()
                },
                InstanceStatus::default(),
            );
            self.instances
                .create(
                    &i,
                    &velstra_cloud_model::access::Writer::controller("quota"),
                )
                .await
                .unwrap();
            i
        }

        async fn project(&self) -> Project {
            self.projects.get("projects/p1").await.unwrap().unwrap()
        }
    }

    #[tokio::test]
    async fn usage_is_counted_from_the_objects_that_exist() {
        let (f, controller) = fixture(Quota::default()).await;
        f.instance("projects/p1/instances/i1", 2, 2048).await;
        f.instance("projects/p1/instances/i2", 4, 4096).await;
        f.instance("projects/p2/instances/i1", 8, 8192).await;

        let project = f.project().await;
        controller
            .reconcile("projects/p1", Some(&project))
            .await
            .unwrap();

        let used = f.project().await.status.used;
        assert_eq!(used.instances, 2);
        assert_eq!(used.vcpus, 6);
        assert_eq!(used.memory_mib, 6144);
        assert_eq!(
            used.volume_gib, 20,
            "root disks are storage somebody pays for"
        );
    }

    #[tokio::test]
    async fn deleting_an_instance_gives_the_quota_back_without_anybody_subtracting() {
        let (f, controller) = fixture(Quota::default()).await;
        let i = f.instance("projects/p1/instances/i1", 2, 2048).await;
        controller
            .reconcile("projects/p1", Some(&f.project().await))
            .await
            .unwrap();
        assert_eq!(f.project().await.status.used.instances, 1);

        f.instances
            .delete(
                &i.meta.name.to_string(),
                f.instances
                    .get(&i.meta.name.to_string())
                    .await
                    .unwrap()
                    .unwrap()
                    .meta
                    .revision,
                &velstra_cloud_model::access::Writer::controller("quota"),
            )
            .await
            .unwrap();
        // Recounted, not decremented — and now counted from a *cache*, which is
        // eventually consistent. The reconcile is retried until the deletion has
        // reached it, which is exactly what the loop's resync does in
        // production: a count one event stale costs a pass, never a wrong
        // number that persists. Asserting on the first pass would encode an
        // immediacy the design deliberately does not promise.
        for _ in 0..200 {
            controller
                .reconcile("projects/p1", Some(&f.project().await))
                .await
                .unwrap();
            if f.project().await.status.used.instances == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(f.project().await.status.used.instances, 0);
        assert_eq!(f.project().await.status.used.vcpus, 0);
    }

    #[tokio::test]
    async fn counting_the_same_thing_twice_writes_once() {
        let (f, controller) = fixture(Quota::default()).await;
        f.instance("projects/p1/instances/i1", 2, 2048).await;
        controller
            .reconcile("projects/p1", Some(&f.project().await))
            .await
            .unwrap();

        let settled = f.project().await;
        let revision = f.raw.revision().await.unwrap();
        controller
            .reconcile("projects/p1", Some(&settled))
            .await
            .unwrap();
        assert_eq!(
            f.raw.revision().await.unwrap(),
            revision,
            "a resync rewrote a project whose usage had not changed"
        );
    }

    #[tokio::test]
    async fn a_project_over_its_limit_says_so_on_itself() {
        let (f, controller) = fixture(Quota {
            devices: 0,
            instances: 1,
            vcpus: 2,
            memory_mib: 2048,
            volume_gib: 100,
            ..Quota::default()
        })
        .await;
        f.instance("projects/p1/instances/i1", 2, 2048).await;
        f.instance("projects/p1/instances/i2", 2, 2048).await;
        controller
            .reconcile("projects/p1", Some(&f.project().await))
            .await
            .unwrap();

        let ready = condition(&f.project().await.status.conditions, "Ready")
            .unwrap()
            .clone();
        assert_eq!(ready.status, ConditionStatus::False);
        assert_eq!(ready.reason, "OverQuota");
    }

    #[tokio::test]
    async fn the_project_reports_that_it_has_caught_up() {
        // Without this a project is permanently "unconverged" in the drift
        // metric, and the number that says the cluster is healthy never
        // reaches zero.
        let (f, controller) = fixture(Quota::default()).await;
        controller
            .reconcile("projects/p1", Some(&f.project().await))
            .await
            .unwrap();
        assert!(f.project().await.converged());
    }
}

#[cfg(test)]
mod recording {
    use std::sync::Arc;

    use velstra_cloud_store::{MemoryStore, prefix_for};

    use super::*;

    /// A controller that records, over an empty cell.
    ///
    /// The caches are real and empty: this test is about what is *written
    /// down*, not about what is counted, and `record` is handed the counts
    /// directly.
    fn recorder(store: &Arc<dyn Store>, interval: u64, retention: u64) -> QuotaController {
        // One closure per type: the caches are typed, and a generic helper
        // would have to name each type at the call site anyway.
        macro_rules! cached {
            ($kind:literal) => {
                Cached::start(
                    TypedStore::new(store.clone(), "cell-1", $kind),
                    store.clone(),
                    prefix_for("cell-1", $kind),
                )
            };
        }
        QuotaController::new(
            cached!("instances"),
            cached!("volumes"),
            cached!("floatingips"),
            cached!("load-balancers"),
            cached!("snapshots"),
            cached!("backups"),
            StatusWriter::new(store.clone(), "cell-1", "projects", WRITER),
            "cell-1",
        )
        .recording_usage(store.clone())
        .every(interval, retention)
    }

    /// A reading is written where a bill can find it, and only one per window.
    ///
    /// The second half is the part that matters: two controllers reconciling
    /// the same project a second apart must not produce two rows. They do not,
    /// because both file the reading under the interval it fell in rather than
    /// under the instant they ran — which is why there is no leader election
    /// here.
    #[tokio::test]
    async fn one_window_is_one_reading_however_many_passes_take_it() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let usage: TypedStore<UsageRecordSpec, UsageRecordStatus> =
            TypedStore::new(store.clone(), "cell-1", "usage");

        let project = Resource::new(
            Meta::new(
                "projects/p1".parse().unwrap(),
                velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
            ),
            ProjectSpec::default(),
            ProjectStatus::default(),
        );
        let used = Quota {
            instances: 3,
            vcpus: 12,
            ..Quota::default()
        };

        let controller = recorder(
            &store,
            velstra_cloud_model::usage::INTERVAL_MS,
            velstra_cloud_model::usage::RETENTION_MS,
        );

        // Three passes inside one hour.
        let base = Timestamp(1_787_824_800_000);
        for offset in [0, 1_000, 59 * 60_000] {
            controller
                .record(&project, &used, (0, 0), Timestamp(base.0 + offset))
                .await;
        }
        let rows = usage.list().await.expect("the store answers");
        assert_eq!(rows.len(), 1, "one hour became {} rows", rows.len());
        assert_eq!(rows[0].spec.used, used);
        assert_eq!(rows[0].spec.project, "projects/p1");
        assert_eq!(rows[0].spec.at, base);

        // The next hour is its own row, or nothing would ever be recorded
        // twice.
        controller
            .record(&project, &used, (0, 0), Timestamp(base.0 + 60 * 60_000))
            .await;
        assert_eq!(usage.list().await.unwrap().len(), 2);
    }

    /// Readings do not accumulate for ever, and the one being written is not
    /// the one being dropped.
    #[tokio::test]
    async fn a_reading_is_taken_away_once_it_is_older_than_the_retention() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let usage: TypedStore<UsageRecordSpec, UsageRecordStatus> =
            TypedStore::new(store.clone(), "cell-1", "usage");
        let project = Resource::new(
            Meta::new(
                "projects/p1".parse().unwrap(),
                velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
            ),
            ProjectSpec::default(),
            ProjectStatus::default(),
        );

        // An hour's readings, kept for three hours.
        let hour = 60 * 60_000u64;
        let controller = recorder(&store, hour, 3 * hour);

        let base = 1_787_824_800_000u64;
        for i in 0..6 {
            controller
                .record(
                    &project,
                    &Quota::default(),
                    (0, 0),
                    Timestamp(base + i * hour),
                )
                .await;
        }
        let rows = usage.list().await.expect("the store answers");
        // The last three hours, and the one just written. Anything older is
        // gone.
        assert!(
            rows.len() <= 4,
            "{} rows survived a three-hour retention",
            rows.len()
        );
        assert!(
            rows.iter().all(|r| r.spec.at.0 >= base + 2 * hour),
            "a reading older than the retention was kept"
        );
    }

    /// A deleted project's readings go with it.
    ///
    /// They used to stay for ever: pruning runs inside the reconcile, and a
    /// project that no longer exists is never reconciled. The delete guard
    /// skips records deliberately — that was the fix for projects being held
    /// hostage by their own accounting — so nothing anywhere came back for
    /// them.
    #[tokio::test]
    async fn the_readings_of_a_deleted_project_are_forgotten() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let usage: TypedStore<UsageRecordSpec, UsageRecordStatus> =
            TypedStore::new(store.clone(), "cell-1", "usage");
        let hour = 60 * 60_000u64;
        let controller = recorder(&store, hour, 90 * 24 * hour);
        let base = 1_787_824_800_000u64;

        for id in ["p1", "p2"] {
            let project = Resource::new(
                Meta::new(
                    format!("projects/{id}").parse().unwrap(),
                    velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
                ),
                ProjectSpec::default(),
                ProjectStatus::default(),
            );
            for i in 0..3 {
                controller
                    .record(
                        &project,
                        &Quota::default(),
                        (0, 0),
                        Timestamp(base + i * hour),
                    )
                    .await;
            }
        }
        assert_eq!(usage.list().await.unwrap().len(), 6);

        // The reconcile that sees it gone.
        controller.reconcile("projects/p1", None).await.unwrap();

        let left = usage.list().await.unwrap();
        assert_eq!(left.len(), 3, "a deleted project's readings were kept");
        assert!(
            left.iter().all(|r| r.spec.project == "projects/p2"),
            "the wrong project's readings were taken away"
        );
    }

    /// And the ones no delete event was ever seen for — a project removed
    /// while this process was down — are swept by the periodic pass.
    #[tokio::test]
    async fn readings_of_a_project_nobody_saw_go_are_swept() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let usage: TypedStore<UsageRecordSpec, UsageRecordStatus> =
            TypedStore::new(store.clone(), "cell-1", "usage");
        let projects: TypedStore<ProjectSpec, ProjectStatus> =
            TypedStore::new(store.clone(), "cell-1", "projects");
        let hour = 60 * 60_000u64;
        let controller = recorder(&store, hour, 90 * 24 * hour);
        let base = 1_787_824_800_000u64;

        let mut live = Vec::new();
        for id in ["p1", "p2"] {
            let project = Resource::new(
                Meta::new(
                    format!("projects/{id}").parse().unwrap(),
                    velstra_cloud_model::meta::Placement::new("eu-central", "cell-1"),
                ),
                ProjectSpec::default(),
                ProjectStatus::default(),
            );
            controller
                .record(&project, &Quota::default(), (0, 0), Timestamp(base))
                .await;
            // Only one of them is still there.
            if id == "p2" {
                projects
                    .create(
                        &project,
                        &velstra_cloud_model::access::Writer::controller(WRITER),
                    )
                    .await
                    .unwrap();
                live.push(projects.get("projects/p2").await.unwrap().unwrap());
            }
        }
        assert_eq!(usage.list().await.unwrap().len(), 2);

        assert_eq!(sweep_orphaned_usage(&usage, &live).await, 1);
        let left = usage.list().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].spec.project, "projects/p2");

        // And it is not a sweep that eats its own tail on the next pass.
        assert_eq!(sweep_orphaned_usage(&usage, &live).await, 0);
    }
}
