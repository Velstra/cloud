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
    let snapshots: TypedStore<SnapshotSpec, SnapshotStatus> =
        TypedStore::new(store.clone(), cell, "snapshots");
    let ports: TypedStore<PortSpec, PortStatus> = TypedStore::new(store.clone(), cell, "ports");
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(store.clone(), cell, "subnets");
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), cell, "networks");
    let routers: TypedStore<RouterSpec, RouterStatus> =
        TypedStore::new(store.clone(), cell, "routers");
    let floating_ips: TypedStore<FloatingIpSpec, FloatingIpStatus> =
        TypedStore::new(store.clone(), cell, "floatingips");

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
        leader.clone(),
    ));
    // A port with no address is a guest with no network, and nothing else in
    // the cell fills one in.
    tasks.spawn(run_when_leading(
        Arc::new(AddressController::new(
            ports.clone(),
            subnets.clone(),
            floating_ips.clone(),
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
        Arc::new(QuotaController::new(
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
            StatusWriter::new(store.clone(), cell, "projects", "quota"),
            cell,
        )),
        projects.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
        leader.clone(),
    ));
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
            args.fabric.clone(),
        )),
        floating_ips.clone(),
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
