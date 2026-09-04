//! Every controller a cell runs, wired once.
//!
//! Two processes start the controllers: the controller binary, behind a leader
//! election, and the development cell, in-process beside the API and a fake
//! node. They used to wire them separately, and the two lists drifted: the dev
//! cell ran seven of twenty, so a maintenance window drained nothing, a
//! migration never moved, and the seeded network sat "unreported" for ever —
//! on the path that is supposed to be how somebody first meets the platform.
//!
//! [`every_controller`] is the one list. Each entry is a named future that runs
//! that controller's reconcile loop for as long as `loops.leader` says so; the
//! caller spawns them however it likes — a `JoinSet` behind an election, or
//! plain `tokio::spawn` with a leader that is always true.

use std::{future::Future, pin::Pin, sync::Arc};

use tokio::sync::watch;
use tracing::{error, warn};
use velstra_cloud_model::{
    ceph::{CephClusterSpec, CephClusterStatus},
    loadbalancer::{LoadBalancerSpec, LoadBalancerStatus},
    meta::Timestamp,
    migration::{MigrationSpec, MigrationStatus},
    resources::{
        AttachmentSpec, AttachmentStatus, FloatingIpSpec, FloatingIpStatus, InstanceSpec,
        InstanceStatus, NetworkSpec, NetworkStatus, NodeSpec, NodeStatus, OperationSpec,
        OperationStatus, PortSpec, PortStatus, ProjectSpec, ProjectStatus, RouterSpec,
        RouterStatus, SnapshotSpec, SnapshotStatus, SubnetSpec, SubnetStatus, VolumeSpec,
        VolumeStatus,
    },
};
use velstra_cloud_store::{Cached, Store, TypedStore, prefix_for};

use crate::{
    LoopConfig, Metrics,
    address::AddressController,
    attachment::AttachmentController,
    ceph::CephController,
    disk::DiskController,
    drift,
    floating_ip::FloatingIpController,
    instance::InstanceController,
    load_balancer::LoadBalancerController,
    migration::MigrationController,
    network::NetworkController,
    operations::OperationsController,
    port::PortController,
    quota::QuotaController,
    router::RouterController,
    run_when_leading,
    scheduler::Scheduler,
    snapshot::SnapshotController,
    status::StatusWriter,
    volume::VolumeController,
};

/// The cell the controllers work for.
pub struct Cell {
    pub store: Arc<dyn Store>,
    pub region: String,
    pub cell: String,
    /// The fabric's northbound endpoint, when this cell has one; the network,
    /// router, floating-IP and load-balancer controllers program it.
    pub fabric: Option<String>,
}

/// How every loop runs: its pacing, where its numbers go, when it stops, and
/// whether this process is the one that should be acting at all.
pub struct Loops {
    pub config: LoopConfig,
    pub metrics: Metrics,
    pub shutdown: watch::Receiver<bool>,
    pub leader: watch::Receiver<bool>,
}

impl Loops {
    /// Loops for a process with no election in front of it: it leads
    /// unconditionally, which is what a single-process cell wants.
    pub fn unelected(config: LoopConfig, metrics: Metrics, shutdown: watch::Receiver<bool>) -> Self {
        let (always, leader) = watch::channel(true);
        // The sender is kept alive for the life of the process; dropping it
        // would close the channel and every loop would read "not leading".
        std::mem::forget(always);
        Self {
            config,
            metrics,
            shutdown,
            leader,
        }
    }
}

/// One controller's loop, ready to be spawned.
pub type Loop = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Every controller the cell runs, by name, as a future each.
///
/// The image-source loop is the one that can be missing: it needs an HTTPS
/// client, and a process without one says so rather than failing to start.
pub fn every_controller(cell: &Cell, loops: &Loops) -> Vec<(&'static str, Loop)> {
    let store = cell.store.clone();
    let id = cell.cell.as_str();
    let config = loops.config;
    let metrics = loops.metrics.clone();

    let instances: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), id, "instances");
    let nodes: TypedStore<NodeSpec, NodeStatus> = TypedStore::new(store.clone(), id, "nodes");
    let ceph_clusters: TypedStore<CephClusterSpec, CephClusterStatus> =
        TypedStore::new(store.clone(), id, "ceph-clusters");
    let volumes: TypedStore<VolumeSpec, VolumeStatus> = TypedStore::new(store.clone(), id, "volumes");
    let attachments: TypedStore<AttachmentSpec, AttachmentStatus> =
        TypedStore::new(store.clone(), id, "attachments");
    let projects: TypedStore<ProjectSpec, ProjectStatus> =
        TypedStore::new(store.clone(), id, "projects");
    let operations: TypedStore<OperationSpec, OperationStatus> =
        TypedStore::new(store.clone(), id, "operations");
    let migrations: TypedStore<MigrationSpec, MigrationStatus> =
        TypedStore::new(store.clone(), id, "migrations");
    let device_classes: TypedStore<
        velstra_cloud_model::pci::DeviceClassSpec,
        velstra_cloud_model::resources::DeviceClassStatus,
    > = TypedStore::new(store.clone(), id, "device-classes");
    let maintenance_windows: TypedStore<
        velstra_cloud_model::maintenance::MaintenanceWindowSpec,
        velstra_cloud_model::maintenance::MaintenanceWindowStatus,
    > = TypedStore::new(store.clone(), id, "maintenance-windows");
    let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
        TypedStore::new(store.clone(), id, "snapshots");
    let backups: TypedStore<
        velstra_cloud_model::backup::BackupSpec,
        velstra_cloud_model::backup::BackupStatus,
    > = TypedStore::new(store.clone(), id, "backups");
    let backup_schedules: TypedStore<
        velstra_cloud_model::backup::BackupScheduleSpec,
        velstra_cloud_model::backup::BackupScheduleStatus,
    > = TypedStore::new(store.clone(), id, "backup-schedules");
    let ports: TypedStore<PortSpec, PortStatus> = TypedStore::new(store.clone(), id, "ports");
    let subnets: TypedStore<SubnetSpec, SubnetStatus> = TypedStore::new(store.clone(), id, "subnets");
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), id, "networks");
    let routers: TypedStore<RouterSpec, RouterStatus> = TypedStore::new(store.clone(), id, "routers");
    let floating_ips: TypedStore<FloatingIpSpec, FloatingIpStatus> =
        TypedStore::new(store.clone(), id, "floatingips");
    let load_balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus> =
        TypedStore::new(store.clone(), id, "load-balancers");

    let mut out: Vec<(&'static str, Loop)> = Vec::new();
    // One shape for every loop: the reconciler, the collection it watches, and
    // the process-wide plumbing.
    macro_rules! spawn {
        ($name:literal, $reconciler:expr, $watched:expr) => {
            out.push((
                $name,
                Box::pin(run_when_leading(
                    Arc::new($reconciler),
                    $watched,
                    store.clone(),
                    config,
                    metrics.clone(),
                    loops.shutdown.clone(),
                    loops.leader.clone(),
                )),
            ));
        };
    }

    spawn!(
        "scheduler",
        Scheduler::new(
            instances.clone(),
            nodes.clone(),
            StatusWriter::new(store.clone(), id, "instances", "scheduler"),
            id,
        )
        .with_device_classes(device_classes.clone())
        .with_maintenance(maintenance_windows.clone()),
        instances.clone()
    );
    spawn!(
        "address",
        AddressController::new(
            ports.clone(),
            subnets.clone(),
            floating_ips.clone(),
            load_balancers.clone(),
            StatusWriter::new(store.clone(), id, "ports", "address"),
            id,
        ),
        ports.clone()
    );
    spawn!(
        "port",
        PortController::new(
            ports.clone(),
            Cached::start(instances.clone(), store.clone(), prefix_for(id, "instances")),
            id,
        ),
        ports.clone()
    );
    spawn!("instance", InstanceController::new(instances.clone()), instances.clone());
    spawn!(
        "disk",
        DiskController::new(attachments.clone(), instances.clone()),
        instances.clone()
    );
    spawn!(
        "attachment",
        AttachmentController::new(attachments.clone(), volumes.clone()),
        attachments.clone()
    );
    spawn!(
        "quota",
        QuotaController::new(
            Cached::start(instances.clone(), store.clone(), prefix_for(id, "instances")),
            Cached::start(volumes.clone(), store.clone(), prefix_for(id, "volumes")),
            Cached::start(
                floating_ips.clone(),
                store.clone(),
                prefix_for(id, "floatingips"),
            ),
            Cached::start(
                load_balancers.clone(),
                store.clone(),
                prefix_for(id, "load-balancers"),
            ),
            StatusWriter::new(store.clone(), id, "projects", "quota"),
            id,
        )
        .recording_usage(store.clone()),
        projects.clone()
    );
    match crate::imagesource::OverHttps::new() {
        Ok(fetch) => {
            spawn!(
                "image-source",
                crate::imagesource::ImageSourceController::new(
                    TypedStore::new(store.clone(), id, "images"),
                    TypedStore::new(store.clone(), id, "instances"),
                    StatusWriter::new(store.clone(), id, "image-sources", "images"),
                    Arc::new(fetch),
                    &cell.region,
                    id,
                ),
                TypedStore::new(store.clone(), id, "image-sources")
            );
        }
        Err(why) => warn!(
            error = %why,
            "no image-source loop: this cell will not rotate images by itself"
        ),
    }
    spawn!(
        "operations",
        OperationsController::new(
            store.clone(),
            StatusWriter::new(store.clone(), id, "operations", "operations"),
            id,
        ),
        operations.clone()
    );
    spawn!(
        "migration",
        MigrationController::new(instances.clone(), id),
        migrations.clone()
    );
    spawn!(
        "volume",
        VolumeController::new(
            volumes.clone(),
            snapshots.clone(),
            TypedStore::new(store.clone(), id, "pools"),
            id,
        ),
        volumes.clone()
    );
    spawn!(
        "network",
        NetworkController::new(store.clone(), id, subnets.clone(), cell.fabric.clone()),
        networks.clone()
    );
    spawn!(
        "router",
        RouterController::new(store.clone(), id, networks.clone(), cell.fabric.clone()),
        routers.clone()
    );
    spawn!(
        "ceph",
        CephController::new(store.clone(), id, nodes.clone()),
        ceph_clusters.clone()
    );
    spawn!(
        "floating-ip",
        FloatingIpController::new(
            store.clone(),
            id,
            floating_ips.clone(),
            subnets.clone(),
            ports.clone(),
            load_balancers.clone(),
            cell.fabric.clone(),
        ),
        floating_ips.clone()
    );
    spawn!(
        "load-balancer",
        LoadBalancerController::new(
            store.clone(),
            id,
            load_balancers.clone(),
            networks.clone(),
            subnets.clone(),
            ports.clone(),
            floating_ips.clone(),
            cell.fabric.clone(),
        ),
        load_balancers.clone()
    );
    spawn!(
        "snapshot",
        SnapshotController::new(snapshots.clone()),
        snapshots.clone()
    );
    spawn!(
        "snapshot-schedule",
        crate::snapshot_schedule::SnapshotScheduleController::new(
            snapshots.clone(),
            volumes.clone()
        ),
        TypedStore::new(store.clone(), id, "snapshot-schedules")
    );
    spawn!(
        "capture",
        crate::capture::CaptureController::new(
            TypedStore::new(store.clone(), id, "images"),
            TypedStore::new(store.clone(), id, "backup-targets"),
        ),
        TypedStore::new(store.clone(), id, "captures")
    );
    spawn!(
        "evacuation",
        crate::evacuation::EvacuationController::new(
            instances.clone(),
            nodes.clone(),
            migrations.clone(),
            TypedStore::new(store.clone(), id, "images"),
        )
        .with_maintenance(maintenance_windows.clone()),
        nodes.clone()
    );
    spawn!(
        "recovery",
        crate::recovery::RecoveryController::new(instances.clone(), nodes.clone()),
        instances.clone()
    );
    spawn!(
        "backup-schedule",
        crate::backup_schedule::BackupScheduleController::new(backups.clone(), volumes.clone()),
        backup_schedules.clone()
    );

    // Not a reconciler: the drift scan reads every collection on a timer and
    // publishes how far behind each one is.
    let scan_metrics = metrics.clone();
    let mut scan_shutdown = loops.shutdown.clone();
    out.push((
        "drift",
        Box::pin(async move {
            let mut every = tokio::time::interval(config.resync);
            loop {
                tokio::select! {
                    _ = scan_shutdown.changed() => if *scan_shutdown.borrow() { return },
                    _ = every.tick() => {
                        let now = Timestamp::now();
                        let scans = [
                            drift::scan("instances", &instances, &scan_metrics, now).await.err(),
                            drift::scan("attachments", &attachments, &scan_metrics, now).await.err(),
                            drift::scan("projects", &projects, &scan_metrics, now).await.err(),
                            drift::scan("operations", &operations, &scan_metrics, now).await.err(),
                            drift::scan("migrations", &migrations, &scan_metrics, now).await.err(),
                            drift::scan("volumes", &volumes, &scan_metrics, now).await.err(),
                            drift::scan("snapshots", &snapshots, &scan_metrics, now).await.err(),
                        ];
                        for error in scans.into_iter().flatten() {
                            error!(%error, "drift scan failed");
                        }
                    }
                }
            }
        }),
    ));
    out
}
