//! Every controller in one process, one cell.
//!
//! One process because they are all the same loop over the same store, and
//! splitting them into four deployments before there is a reason to buys four
//! things to operate and nothing else. The seam is already where it needs to
//! be: each controller is an independent task with its own queue, so pulling
//! one out is moving a `tokio::spawn`, not an untangling.

use std::{sync::Arc, time::Duration};

use clap::Parser;
use tokio::sync::watch;
use tracing::{error, info};
use velstra_cloud_controller::{
    LoopConfig, Metrics,
    address::AddressController,
    attachment::AttachmentController,
    ceph::CephController,
    drift,
    election::{ElectionConfig, elect},
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
use velstra_cloud_store::{EtcdStore, MemoryStore, Store, TypedStore};

#[derive(Parser, Debug)]
#[command(name = "velstra-cloud-controller", about = "Velstra Cloud controllers")]
struct Args {
    /// Where the state lives: `memory`, or one or more etcd endpoints.
    ///
    /// `memory` is a single process talking to itself — useful for a demo and
    /// for nothing else, because the state dies with the process and no second
    /// binary can see it. Anything else is read as a comma-separated list of
    /// etcd endpoints, which is what makes the API, the controllers and the
    /// node agents parts of one cell rather than three separate universes.
    #[arg(long, env = "VELSTRA_STORE", default_value = "memory")]
    store: String,
    /// The failure domain this process serves. Every key it reads and writes is
    /// under it, and nothing it does crosses into another.
    #[arg(long, env = "VELSTRA_CELL", default_value = "cell-1")]
    cell: String,

    #[arg(long, env = "VELSTRA_REGION", default_value = "eu-central")]
    region: String,

    /// How often to re-list everything and reconcile it again, in seconds.
    ///
    /// This is the longest a missed watch event can cost, which is the only
    /// thing it buys — and the reason it can be short is that a reconcile of a
    /// settled object writes nothing.
    #[arg(long, default_value_t = 300)]
    resync_interval: u64,

    /// The shortest interval between two reconciles in one controller, in
    /// milliseconds. The ceiling on what any one controller can cost.
    #[arg(long, default_value_t = 5)]
    rate_limit_ms: u64,

    /// Where to serve Prometheus metrics, or `off`.
    #[arg(long, default_value = "127.0.0.1:9310")]
    metrics_addr: String,

    /// What this process calls itself in the leader lease.
    ///
    /// Defaults to the hostname, which is what makes a lease record readable
    /// during an incident: "which machine is acting" is the first question, and
    /// a random id answers it with a lookup. Two processes on one host must be
    /// given distinct identities — the election is correct either way (the
    /// compare-and-swap does not care what the holder is called), but a lease
    /// naming a host twice tells an operator nothing.
    #[arg(long, env = "VELSTRA_IDENTITY")]
    identity: Option<String>,

    /// How long an unrenewed lease stands before another process may take it,
    /// in seconds. Also the longest a leader's death pauses reconciliation.
    #[arg(long, default_value_t = 15)]
    lease_seconds: u64,

    /// Where the fabric's orchestrator answers, if this cell has one.
    ///
    /// Given, this process mirrors each tenant network to the fabric — the one
    /// fact no node can state, because it belongs to the cell rather than to any
    /// machine. Omitted, nothing is mirrored and nothing pretends to be: a cell
    /// with no fabric is a control plane that runs and programs no data plane,
    /// which is exactly what a test cell is.
    #[arg(long, env = "VELSTRA_FABRIC")]
    fabric: Option<String>,

    /// Run without leader election, acting unconditionally.
    ///
    /// For a single-process deployment and for a developer cell, where the
    /// lease is one more thing to wait for and nothing else is contending. Never
    /// for two processes against one store — that is the case the election
    /// exists for.
    #[arg(long)]
    no_leader_election: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let config = LoopConfig {
        resync: Duration::from_secs(args.resync_interval),
        rate: Duration::from_millis(args.rate_limit_ms),
        ..LoopConfig::default()
    };
    let metrics = Metrics::new();

    // Everything above the `Store` trait is written against etcd semantics —
    // monotonic revisions, compare-and-swap, watch from a revision — which is
    // why choosing a backend here changes nothing below it.
    let store: Arc<dyn Store> = match open_store(&args.store).await {
        Ok(store) => store,
        Err(error) => {
            error!(%error, store = %args.store, "cannot reach the state store");
            std::process::exit(1);
        }
    };
    let cell = args.cell.as_str();

    let instances: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), cell, "instances");
    let nodes: TypedStore<NodeSpec, NodeStatus> = TypedStore::new(store.clone(), cell, "nodes");
    let ceph_clusters: TypedStore<CephClusterSpec, CephClusterStatus> =
        TypedStore::new(store.clone(), cell, "ceph-clusters");
    let volumes: TypedStore<VolumeSpec, VolumeStatus> =
        TypedStore::new(store.clone(), cell, "volumes");
    let attachments: TypedStore<AttachmentSpec, AttachmentStatus> =
        TypedStore::new(store.clone(), cell, "attachments");
    let projects: TypedStore<ProjectSpec, ProjectStatus> =
        TypedStore::new(store.clone(), cell, "projects");
    let operations: TypedStore<OperationSpec, OperationStatus> =
        TypedStore::new(store.clone(), cell, "operations");
    let migrations: TypedStore<MigrationSpec, MigrationStatus> =
        TypedStore::new(store.clone(), cell, "migrations");
    let device_classes: TypedStore<
        velstra_cloud_model::pci::DeviceClassSpec,
        velstra_cloud_model::resources::DeviceClassStatus,
    > = TypedStore::new(store.clone(), cell, "device-classes");
    let maintenance_windows: TypedStore<
        velstra_cloud_model::maintenance::MaintenanceWindowSpec,
        velstra_cloud_model::maintenance::MaintenanceWindowStatus,
    > = TypedStore::new(store.clone(), cell, "maintenance-windows");
    let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
        TypedStore::new(store.clone(), cell, "snapshots");
    let backups: TypedStore<
        velstra_cloud_model::backup::BackupSpec,
        velstra_cloud_model::backup::BackupStatus,
    > = TypedStore::new(store.clone(), cell, "backups");
    let backup_schedules: TypedStore<
        velstra_cloud_model::backup::BackupScheduleSpec,
        velstra_cloud_model::backup::BackupScheduleStatus,
    > = TypedStore::new(store.clone(), cell, "backup-schedules");
    let ports: TypedStore<PortSpec, PortStatus> = TypedStore::new(store.clone(), cell, "ports");
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(store.clone(), cell, "subnets");
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), cell, "networks");
    let routers: TypedStore<RouterSpec, RouterStatus> =
        TypedStore::new(store.clone(), cell, "routers");
    let floating_ips: TypedStore<FloatingIpSpec, FloatingIpStatus> =
        TypedStore::new(store.clone(), cell, "floatingips");
    let load_balancers: TypedStore<LoadBalancerSpec, LoadBalancerStatus> =
        TypedStore::new(store.clone(), cell, "load-balancers");

    let (stop, shutdown) = watch::channel(false);

    // Exactly one process acts; the others stand ready. Every controller below
    // is gated on this, so a follower holds no watch and no queue — see
    // `runner::run_when_leading`.
    let identity = args.identity.clone().unwrap_or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| format!("controller-{}", std::process::id()))
    });
    let (leader, election) = if args.no_leader_election {
        info!("leader election disabled; this process acts unconditionally");
        let (tx, rx) = watch::channel(true);
        // Held for the life of the process: dropping the sender would close the
        // channel and every controller would read it as a stand-down.
        std::mem::forget(tx);
        (rx, None)
    } else {
        let config = ElectionConfig {
            lease: Duration::from_secs(args.lease_seconds),
            // A third of the lease: three attempts fit inside one, so two may
            // fail outright and a healthy leader still keeps it.
            renew: Duration::from_secs((args.lease_seconds / 3).max(1)),
            ..Default::default()
        };
        let (rx, handle) = elect(store.clone(), cell, &identity, config, shutdown.clone());
        info!(identity = %identity, lease_s = args.lease_seconds, "campaigning for the cell");
        (rx, Some(handle))
    };
    let mut tasks = tokio::task::JoinSet::new();

    tasks.spawn(run_when_leading(
        Arc::new(
            Scheduler::new(
                instances.clone(),
                nodes.clone(),
                StatusWriter::new(store.clone(), cell, "instances", "scheduler"),
                cell,
            )
            // Without these the scheduler answers "no such device class" to
            // every guest that asks for one, in a cell where the class is
            // sitting right there — and refuses to notice a node somebody
            // declared out of service for tonight.
            .with_device_classes(device_classes.clone())
            .with_maintenance(maintenance_windows.clone()),
        ),
        instances.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // A port with no address is a guest with no network, and nothing else in
    // the cell fills one in.
    tasks.spawn(run_when_leading(
        Arc::new(AddressController::new(
            ports.clone(),
            subnets.clone(),
            floating_ips.clone(),
            load_balancers.clone(),
            StatusWriter::new(store.clone(), cell, "ports", "address"),
            cell,
        )),
        ports.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    tasks.spawn(run_when_leading(
        Arc::new(PortController::new(
            ports.clone(),
            // One copy of the instances in memory, shared by whoever needs a
            // reverse lookup into them, fed by one watch. The alternative is one
            // list of the whole collection per port per event.
            velstra_cloud_store::Cached::start(
                instances.clone(),
                store.clone(),
                velstra_cloud_store::prefix_for(cell, "instances"),
            ),
            &args.cell,
        )),
        ports.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // The guard that makes a delete a teardown: without it the object leaves
    // the store in the same request that asks for it, and the node never sees
    // an instance it was meant to stop.
    tasks.spawn(run_when_leading(
        Arc::new(InstanceController::new(instances.clone())),
        instances.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    tasks.spawn(run_when_leading(
        Arc::new(AttachmentController::new(attachments.clone())),
        attachments.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    tasks.spawn(run_when_leading(
        Arc::new(
            QuotaController::new(
                // One in-memory copy each, fed by one watch — without it a quota
                // resync reads every instance once per project, which is measured
                // quadratic in tests/scaling.rs.
                velstra_cloud_store::Cached::start(
                    instances.clone(),
                    store.clone(),
                    velstra_cloud_store::prefix_for(cell, "instances"),
                ),
                velstra_cloud_store::Cached::start(
                    volumes.clone(),
                    store.clone(),
                    velstra_cloud_store::prefix_for(cell, "volumes"),
                ),
                velstra_cloud_store::Cached::start(
                    floating_ips.clone(),
                    store.clone(),
                    velstra_cloud_store::prefix_for(cell, "floatingips"),
                ),
                velstra_cloud_store::Cached::start(
                    load_balancers.clone(),
                    store.clone(),
                    velstra_cloud_store::prefix_for(cell, "load-balancers"),
                ),
                StatusWriter::new(store.clone(), cell, "projects", "quota"),
                cell,
            )
            // What each project had, once an hour, kept for ninety days. A cell
            // that nobody bills still gets them: the rows are small, the question
            // "what did they use last month" has no other answer, and it is not
            // one anybody asks in time to turn the recording on.
            .recording_usage(store.clone()),
        ),
        projects.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Keeping a family current is one job for the cell, not one per node, so it
    // runs where every other cell-wide loop does — behind the leader, so three
    // control planes do not each publish the same image.
    match velstra_cloud_controller::imagesource::OverHttps::new() {
        Ok(fetch) => {
            tasks.spawn(run_when_leading(
                Arc::new(
                    velstra_cloud_controller::imagesource::ImageSourceController::new(
                        velstra_cloud_store::TypedStore::new(store.clone(), cell, "images"),
                        velstra_cloud_store::TypedStore::new(store.clone(), cell, "instances"),
                        StatusWriter::new(store.clone(), cell, "image-sources", "images"),
                        Arc::new(fetch),
                        &args.region,
                        cell,
                    ),
                ),
                velstra_cloud_store::TypedStore::new(store.clone(), cell, "image-sources"),
                store.clone(),
                config,
                metrics.clone(),
                shutdown.clone(),
                leader.clone(),
            ));
        }
        // A cell whose TLS stack will not build is a cell that cannot learn a
        // digest safely, and the honest thing is to run without the loop and say
        // so — not to fall back to fetching it over something unverified.
        Err(why) => tracing::warn!(
            error = %why,
            "no image-source loop: this cell will not rotate images by itself"
        ),
    }

    tasks.spawn(run_when_leading(
        Arc::new(OperationsController::new(
            store.clone(),
            StatusWriter::new(store.clone(), cell, "operations", "operations"),
            cell,
        )),
        operations.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));

    tasks.spawn(run_when_leading(
        // No status writer, and deliberately so: this controller has no business
        // writing a migration's status, and not handing it the means is the
        // cheapest way to keep it that way.
        Arc::new(MigrationController::new(instances.clone(), cell)),
        migrations.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));

    // The two guards storage lives by: nothing is billed for bytes nobody can
    // find, and no volume is destroyed underneath the copies that are read
    // through it. Both are finalizers, so both are a controller's work.
    tasks.spawn(run_when_leading(
        Arc::new(VolumeController::new(
            volumes.clone(),
            snapshots.clone(),
            velstra_cloud_store::TypedStore::new(store.clone(), cell, "pools"),
            cell,
        )),
        volumes.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // The fabric learns about a tenant network from here and nowhere else: a
    // node agent calls `create_port` with a VNI, and until something has said
    // what that VNI *is*, the fabric answers `unknown network vni` and no guest
    // reaches the network.
    tasks.spawn(run_when_leading(
        Arc::new(NetworkController::new(
            store.clone(),
            cell,
            subnets.clone(),
            args.fabric.clone(),
        )),
        networks.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // And the fabric learns that two of those networks route to each other from
    // here. Without it a tenant's networks are each reachable and mutually
    // isolated — which is the correct default, and not what the operator asked
    // for when they declared a router.
    tasks.spawn(run_when_leading(
        Arc::new(RouterController::new(
            store.clone(),
            cell,
            networks.clone(),
            args.fabric.clone(),
        )),
        routers.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // And the Ceph cluster, if there is one. It runs unconditionally and costs
    // nothing in a cell without one: no object, no reconcile.
    //
    // It runs no commands. Every step of a deployment happens on the machine the
    // daemon will live on, driven by that machine's own agent from the same
    // stored spec — this assembles what the nodes report into "the cluster is
    // what was asked for", which is a judgement about other objects and so
    // belongs to a controller rather than to any one of them.
    tasks.spawn(run_when_leading(
        Arc::new(CephController::new(store.clone(), cell, nodes.clone())),
        ceph_clusters.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // And a floating IP, which is the one address in this system that is not a
    // property of the machine answering on it.
    tasks.spawn(run_when_leading(
        Arc::new(FloatingIpController::new(
            store.clone(),
            cell,
            floating_ips.clone(),
            subnets.clone(),
            ports.clone(),
            load_balancers.clone(),
            args.fabric.clone(),
        )),
        floating_ips.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // And a load balancer: one address the fabric answers on for a pool of
    // ports. The VIP is decided even in a cell with no fabric; the services
    // are mirrored only where one exists.
    tasks.spawn(run_when_leading(
        Arc::new(LoadBalancerController::new(
            store.clone(),
            cell,
            load_balancers.clone(),
            networks.clone(),
            subnets.clone(),
            ports.clone(),
            floating_ips.clone(),
            args.fabric.clone(),
        )),
        load_balancers.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    tasks.spawn(run_when_leading(
        Arc::new(SnapshotController::new(snapshots.clone())),
        snapshots.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Hourly snapshots. Beside the backup schedule rather than folded into
    // it: a snapshot lives in the volume's own pool and is lost with it, and
    // one field distinguishing "cheap and local" from "survives the pool"
    // would be a flag people set wrong.
    tasks.spawn(run_when_leading(
        Arc::new(
            velstra_cloud_controller::snapshot_schedule::SnapshotScheduleController::new(
                snapshots.clone(),
                volumes.clone(),
            ),
        ),
        TypedStore::new(store.clone(), cell, "snapshot-schedules"),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Turning finished captures into images. Leader-only: two of these would
    // race to create the same image, and the loser's error is noise about a
    // thing that worked.
    tasks.spawn(run_when_leading(
        Arc::new(velstra_cloud_controller::capture::CaptureController::new(
            TypedStore::new(store.clone(), cell, "images"),
            TypedStore::new(store.clone(), cell, "backup-targets"),
        )),
        TypedStore::new(store.clone(), cell, "captures"),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Emptying a node that has been asked to give up its guests. Keyed on
    // nodes, unlike its neighbours: the ask is on the node and the guests are
    // what follows from it.
    tasks.spawn(run_when_leading(
        Arc::new(
            velstra_cloud_controller::evacuation::EvacuationController::new(
                instances.clone(),
                nodes.clone(),
                migrations.clone(),
            )
            .with_maintenance(maintenance_windows.clone()),
        ),
        nodes.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Bringing guests back from a node that stopped answering. Leader-only,
    // and emphatically so: two controllers unplacing the same guest would be
    // two of them handing it to the scheduler, and the second one would do it
    // after the first had already been placed.
    tasks.spawn(run_when_leading(
        Arc::new(velstra_cloud_controller::recovery::RecoveryController::new(
            instances.clone(),
            nodes.clone(),
        )),
        instances.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
    // Backups. Leader-only like the rest: two controllers asking for the same
    // copy on the same second would be refused by the derived name, but two of
    // them *expiring* would race on a delete for no reason.
    tasks.spawn(run_when_leading(
        Arc::new(
            velstra_cloud_controller::backup_schedule::BackupScheduleController::new(
                backups.clone(),
                volumes.clone(),
            ),
        ),
        backup_schedules.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));

    // The drift scan is not a controller: it writes nothing and decides
    // nothing. It reads the cluster on the resync cadence and publishes how
    // much of it disagrees with itself.
    tasks.spawn({
        let metrics = metrics.clone();
        let mut shutdown = shutdown.clone();
        async move {
            let mut every = tokio::time::interval(config.resync);
            loop {
                tokio::select! {
                    _ = shutdown.changed() => if *shutdown.borrow() { return },
                    _ = every.tick() => {
                        let now = Timestamp::now();
                        let scans = [
                            drift::scan("instances", &instances, &metrics, now).await.err(),
                            drift::scan("attachments", &attachments, &metrics, now).await.err(),
                            drift::scan("projects", &projects, &metrics, now).await.err(),
                            drift::scan("operations", &operations, &metrics, now).await.err(),
                            drift::scan("migrations", &migrations, &metrics, now).await.err(),
                            drift::scan("volumes", &volumes, &metrics, now).await.err(),
                            drift::scan("snapshots", &snapshots, &metrics, now).await.err(),
                        ];
                        for error in scans.into_iter().flatten() {
                            error!(%error, "drift scan failed");
                        }
                    }
                }
            }
        }
    });

    if args.metrics_addr != "off" {
        tasks.spawn(serve_metrics(
            args.metrics_addr.clone(),
            metrics.clone(),
            shutdown.clone(),
        ));
    }

    info!(
        cell = args.cell,
        region = args.region,
        resync_seconds = args.resync_interval,
        "controllers running"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("stopping"),
        _ = tasks.join_next() => error!("a controller stopped on its own"),
    }
    let _ = stop.send(true);
    tasks.shutdown().await;
    // Awaited, not aborted: the campaign releases the lease on its way out, so a
    // planned restart costs a follower one poll instead of a whole lease. Aborting
    // here would throw that away and make every deliberate restart look like a
    // crash to the rest of the cell.
    if let Some(election) = election {
        let _ = election.await;
    }
}

async fn serve_metrics(addr: String, metrics: Metrics, mut shutdown: watch::Receiver<bool>) {
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let metrics = metrics.clone();
            async move { metrics.render() }
        }),
    );
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(addr, %error, "cannot serve metrics");
            return;
        }
    };
    info!(addr, "serving metrics");
    let served = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown.changed().await;
    });
    if let Err(error) = served.await {
        error!(%error, "the metrics server stopped");
    }
}

/// Open whichever store the operator asked for.
///
/// One function in each binary rather than a shared helper, because the two
/// have different error types and a shared one would exist only to be
/// converted twice. If a third appears, that is the moment to extract it.
async fn open_store(spec: &str) -> Result<Arc<dyn Store>, velstra_cloud_store::StoreError> {
    if spec == "memory" {
        // Said out loud: a process whose state dies with it should not be a
        // surprise to whoever started it.
        tracing::warn!(
            "using the in-memory store: this process shares state with nobody \
             and forgets everything when it stops"
        );
        return Ok(Arc::new(MemoryStore::new()));
    }
    let endpoints: Vec<&str> = spec.split(',').map(str::trim).collect();
    let store = EtcdStore::connect(&endpoints).await?;
    tracing::info!(endpoints = %spec, "state store connected");
    Ok(Arc::new(store))
}
