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
    loadbalancer::{LoadBalancerSpec, LoadBalancerStatus},
    meta::{Meta, ResourceName, Timestamp, set_condition},
    reconcile::{count_quota, quota_condition},
    resources::{
        FloatingIpSpec, FloatingIpStatus, InstanceSpec, InstanceStatus, Project, ProjectSpec,
        ProjectStatus, Quota, Resource, VolumeSpec, VolumeStatus,
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
    pub fn new(
        instances: Cached<InstanceSpec, InstanceStatus>,
        volumes: Cached<VolumeSpec, VolumeStatus>,
        floating: Cached<FloatingIpSpec, FloatingIpStatus>,
        balancers: Cached<LoadBalancerSpec, LoadBalancerStatus>,
        status: StatusWriter<ProjectSpec, ProjectStatus>,
        cell: &str,
    ) -> Self {
        Self {
            instances,
            volumes,
            floating,
            balancers,
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
    async fn record(&self, project: &Project, used: &Quota, now: Timestamp) {
        let Some(store) = &self.usage else {
            return;
        };
        let at = velstra_cloud_model::usage::window_of(now, self.interval_ms);
        let id = velstra_cloud_model::usage::id_for(at);
        let name = format!("{}/usage/{id}", project.meta.name);
        let Ok(name) = ResourceName::parse(&name) else {
            return;
        };
        let record = Resource::new(
            Meta::new(name, project.meta.placement.clone()),
            UsageRecordSpec {
                project: project.meta.name.to_string(),
                at,
                used: used.clone(),
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
        ["instances", "volumes", "floatingips", "load-balancers"]
            .into_iter()
            .map(|kind| Related::named(prefix_for(&self.cell, kind), owning_project))
            .collect()
    }

    async fn reconcile(&self, _name: &str, object: Option<&Project>) -> Result<()> {
        let Some(project) = object else {
            return Ok(());
        };

        let (instances, _) = self.instances.all().await;
        let (volumes, _) = self.volumes.all().await;
        let (floating, _) = self.floating.all().await;
        let (balancers, _) = self.balancers.all().await;
        let instances: Vec<_> = instances.iter().map(|i| (**i).clone()).collect();
        let volumes: Vec<_> = volumes.iter().map(|v| (**v).clone()).collect();
        let floating: Vec<_> = floating.iter().map(|f| (**f).clone()).collect();
        let balancers: Vec<_> = balancers.iter().map(|l| (**l).clone()).collect();
        let used = count_quota(
            &project.meta.name,
            &instances,
            &volumes,
            &floating,
            &balancers,
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
        self.record(project, &next.status.used, Timestamp::now())
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
                .record(&project, &used, Timestamp(base.0 + offset))
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
            .record(&project, &used, Timestamp(base.0 + 60 * 60_000))
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
                .record(&project, &Quota::default(), Timestamp(base + i * hour))
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
}
