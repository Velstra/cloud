//! The pool agent, as a process.
//!
//! [`velstra_cloud_nodeagent::pool::PoolAgent`] existed, was tested, and had no
//! `main`. The only thing that ever constructed one was the e2e harness, with
//! the fake — so a cell could hold a Pool object and a Volume object and there
//! was no process anywhere that would put a byte on a disk. This is that
//! process.
//!
//! It is deliberately **not** part of the node agent. A pool is not a machine:
//! several nodes reach one Ceph pool, one node may export three volume groups.
//! Tying storage to whichever hypervisor happened to be asked is how a volume
//! becomes unreachable the moment that node is drained.

use std::{path::PathBuf, sync::Arc, time::Duration};

use clap::{Parser, ValueEnum};
use velstra_cloud_nodeagent::{
    api_cell::ApiCell,
    directory_pool::DirectoryPool,
    pool::{FakePool, PoolAgent, PoolConfig, Storage},
};
use velstra_cloud_store::{EtcdStore, MemoryStore, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// A directory of qcow2 files, made with `qemu-img`. Needs a writable
    /// directory and nothing else.
    Directory,
    /// A pool in a process. Everything it holds dies with it — useful for
    /// exercising the loop and for nothing else.
    Fake,
}

#[derive(Debug, Parser)]
#[command(
    name = "velstra-cloud-poolagent",
    about = "Owns one storage pool: provisions what is asked for, reports what is there"
)]
struct Args {
    /// Where the state lives: `memory`, or one or more etcd endpoints.
    #[arg(long, env = "VELSTRA_STORE", default_value = "memory")]
    store: String,

    /// This pool's id. It must match the id in the pool object, because that is
    /// what every volume is written against.
    #[arg(long)]
    pool: String,

    #[arg(long, env = "VELSTRA_CELL", default_value = "cell-1")]
    cell: String,

    #[arg(long, default_value = "eu-central")]
    region: String,

    #[arg(long, value_enum, default_value_t = Backend::Directory)]
    backend: Backend,

    /// Where volumes live, for the directory backend. Copies live in
    /// `snapshots/` underneath it, and the whole directory belongs to the
    /// platform.
    #[arg(long, default_value = "/var/lib/velstra/pool")]
    dir: PathBuf,

    /// Where an image named by a volume is found. The same directory a node
    /// agent publishes images into, so a node and a pool on one machine share
    /// one copy rather than keeping two.
    #[arg(long, default_value = "/var/lib/velstra/images")]
    images: PathBuf,

    /// What the fake backend claims to hold. Ignored by every real one, which
    /// measures the filesystem instead.
    #[arg(long, default_value = "1000")]
    fake_capacity_gib: u64,

    /// Read the cell through the API instead of the store, and be handed only
    /// this pool's share.
    ///
    /// Without it, this agent lists every volume and every snapshot in the cell
    /// on every pass — so its load grows with the cell rather than with what it
    /// holds. With it, the API serves every agent from one watch per collection
    /// and hands each one its own objects.
    ///
    /// Writes still go straight to the store either way: a pool's writes are
    /// already proportional to its own work.
    #[arg(long)]
    api: Option<String>,

    /// The bearer token for `--api`. A file rather than a flag, so it is not in
    /// anybody's process list.
    #[arg(long)]
    api_token_file: Option<PathBuf>,

    /// How often the pool is re-read and reconciled.
    ///
    /// Slower than a node's, and there is no watch at all: storage work is
    /// measured in seconds to minutes, so the latency a watch would buy is lost
    /// in the noise of a copy.
    #[arg(long, default_value = "30")]
    resync_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let storage: Arc<dyn Storage> = match args.backend {
        Backend::Directory => {
            // Said out loud and early: a pool agent that cannot write where it
            // was pointed should fail here rather than on the first volume
            // somebody creates.
            std::fs::create_dir_all(&args.dir)
                .map_err(|e| format!("cannot use {} as a pool: {e}", args.dir.display()))?;
            tracing::info!(dir = %args.dir.display(), images = %args.images.display(), "directory pool");
            Arc::new(DirectoryPool::new(args.dir.clone(), args.images.clone()))
        }
        Backend::Fake => {
            tracing::warn!(
                "using the fake pool: nothing is written to a disk and everything it \
                 holds dies with this process"
            );
            Arc::new(FakePool::new(args.fake_capacity_gib))
        }
    };

    let store: Arc<dyn Store> = open_store(&args.store).await?;
    let mut config = PoolConfig::new(&args.pool, &args.region, &args.cell);
    config.resync = Duration::from_secs(args.resync_secs);
    let agent = match &args.api {
        Some(url) => {
            let token = match &args.api_token_file {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|e| format!("reading {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
                None => {
                    return Err("--api needs --api-token-file: the API will refuse an \
                                unauthenticated reader, and finding that out as an empty pool is \
                                the worst way to learn it"
                        .into());
                }
            };
            let reader = Arc::new(ApiCell::for_pool(url, &token, &args.pool)?);
            PoolAgent::reading(store, config, storage, reader)
        }
        None => PoolAgent::new(store, config, storage),
    };
    match &args.api {
        Some(url) => {
            tracing::info!(api = %url, pool = %args.pool, "reading this pool's share through the API")
        }
        None => tracing::warn!(
            "reading the store directly: this agent lists every volume and snapshot in the cell \
             on every pass. Point it at the API with --api to be handed only its own."
        ),
    }

    tracing::info!(pool = agent.pool(), cell = args.cell, "pool agent running");
    agent
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("stopping");
        })
        .await;
    Ok(())
}

async fn open_store(spec: &str) -> Result<Arc<dyn Store>, velstra_cloud_store::StoreError> {
    if spec == "memory" {
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
