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
    election::{ElectionConfig, elect},
};
use velstra_cloud_store::{EtcdStore, MemoryStore, Store};

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

    /// Where to POST an alert — one JSON object per transition, `firing` or
    /// `resolved`. Nothing is posted without it.
    #[arg(long, env = "VELSTRA_ALERT_WEBHOOK")]
    alert_webhook: Option<String>,

    /// Who to mail an alert, through the sendmail binary below. Repeat the
    /// flag, or separate addresses with commas in the variable.
    #[arg(long, env = "VELSTRA_ALERT_MAIL_TO", value_delimiter = ',')]
    alert_mail_to: Vec<String>,

    /// The sender an alert mail carries.
    #[arg(
        long,
        env = "VELSTRA_ALERT_MAIL_FROM",
        default_value = "velstra-cloud@localhost"
    )]
    alert_mail_from: String,

    /// A sendmail-compatible binary; it is given the message on stdin with
    /// `-t`, so any MTA or msmtp will do.
    #[arg(
        long,
        env = "VELSTRA_ALERT_SENDMAIL",
        default_value = "/usr/sbin/sendmail"
    )]
    alert_sendmail: std::path::PathBuf,

    /// A pool is "nearly full" at this share of its capacity, in percent.
    #[arg(long, default_value_t = 80)]
    alert_pool_full_percent: u8,

    /// An object that has disagreed with itself for this many seconds is
    /// "stuck".
    #[arg(long, default_value_t = 900)]
    alert_stuck_after: u64,
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
    // The one list of controllers, shared with the development cell — see
    // `wiring`. What this process adds is the election in front of them.
    let wiring = velstra_cloud_controller::wiring::Cell {
        store: store.clone(),
        region: args.region.clone(),
        cell: args.cell.clone(),
        fabric: args.fabric.clone(),
    };
    let alerts = velstra_cloud_controller::alerts::Config {
        targets: velstra_cloud_controller::alerts::Targets {
            webhook: args.alert_webhook.clone(),
            mail_to: args
                .alert_mail_to
                .iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect(),
            mail_from: args.alert_mail_from.clone(),
            sendmail: args.alert_sendmail.clone(),
        },
        rules: velstra_cloud_controller::alerts::Rules {
            pool_full_percent: args.alert_pool_full_percent,
            stuck_after: std::time::Duration::from_secs(args.alert_stuck_after),
            ..Default::default()
        },
    };
    if alerts.targets.is_empty() {
        info!("alerts are judged but nobody is told: no --alert-webhook or --alert-mail-to");
    }
    let loops = velstra_cloud_controller::wiring::Loops {
        config,
        metrics: metrics.clone(),
        shutdown: shutdown.clone(),
        leader: leader.clone(),
        alerts,
    };
    for (name, task) in velstra_cloud_controller::wiring::every_controller(&wiring, &loops) {
        tracing::debug!(controller = name, "starting");
        tasks.spawn(task);
    }

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
