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
    meta::{ResourceName, set_condition},
    reconcile::{count_quota, quota_condition},
    resources::{
        FloatingIpSpec, FloatingIpStatus, InstanceSpec, InstanceStatus, Project, ProjectSpec,
        ProjectStatus, VolumeSpec, VolumeStatus,
    },
};
use velstra_cloud_store::{Cached, prefix_for};

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
            cell: cell.to_string(),
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
