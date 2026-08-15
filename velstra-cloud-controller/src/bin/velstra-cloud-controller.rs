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
    LoopConfig, Metrics, attachment::AttachmentController, drift, migration::MigrationController,
    operations::OperationsController, quota::QuotaController, run, scheduler::Scheduler,
    status::StatusWriter,
};
use velstra_cloud_model::{
    meta::Timestamp,
    migration::{MigrationSpec, MigrationStatus},
    resources::{
        AttachmentSpec, AttachmentStatus, InstanceSpec, InstanceStatus, NodeSpec, NodeStatus,
        OperationSpec, OperationStatus, ProjectSpec, ProjectStatus, VolumeSpec, VolumeStatus,
    },
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

#[derive(Parser, Debug)]
#[command(name = "velstra-cloud-controller", about = "Velstra Cloud controllers")]
struct Args {
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

    // In memory for now. Everything above the `Store` trait is written against
    // etcd semantics — monotonic revisions, compare-and-swap, watch from a
    // revision — so this swaps for a real backend without a line changing here.
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let cell = args.cell.as_str();

    let instances: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), cell, "instances");
    let nodes: TypedStore<NodeSpec, NodeStatus> = TypedStore::new(store.clone(), cell, "nodes");
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

    let (stop, shutdown) = watch::channel(false);
    let mut tasks = tokio::task::JoinSet::new();

    tasks.spawn(run(
        Arc::new(Scheduler::new(
            instances.clone(),
            nodes.clone(),
            StatusWriter::new(store.clone(), cell, "instances", "scheduler"),
            cell,
        )),
        instances.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));
    tasks.spawn(run(
        Arc::new(AttachmentController::new(attachments.clone())),
        attachments.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));
    tasks.spawn(run(
        Arc::new(QuotaController::new(
            instances.clone(),
            volumes.clone(),
            StatusWriter::new(store.clone(), cell, "projects", "quota"),
            cell,
        )),
        projects.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));
    tasks.spawn(run(
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
    ));

    tasks.spawn(run(
        // No status writer, and deliberately so: this controller has no business
        // writing a migration's status, and not handing it the means is the
        // cheapest way to keep it that way.
        Arc::new(MigrationController::new(instances.clone(), cell)),
        migrations.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
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
