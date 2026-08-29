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
    ceph_pool::{CephConfig, CephPool},
    directory_pool::DirectoryPool,
    lvm_pool::{LvmConfig, LvmPool},
    pool::{FakePool, PoolAgent, PoolConfig, Storage},
};
use velstra_cloud_store::{EtcdStore, MemoryStore, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// A directory of qcow2 files, made with `qemu-img`. Needs a writable
    /// directory and nothing else.
    Directory,
    /// An RBD pool in a Ceph cluster. Volumes are copy-on-write clones of an
    /// image that lives once in the cluster, so nothing is copied per node and
    /// every node reaches every image.
    Ceph,
    /// Logical volumes in one LVM volume group. A guest is handed the device
    /// itself — no image format between it and the disk — and the volume group
    /// is what most single-machine estates already have.
    Lvm,
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

    /// The LVM volume group volumes are made in, for the lvm backend.
    #[arg(long, env = "VELSTRA_LVM_GROUP")]
    lvm_group: Option<String>,

    /// A thin pool inside that group, if there is one.
    ///
    /// It changes what a snapshot costs and how it fails: a thick snapshot
    /// reserves its own space up front and is dropped by the kernel when it
    /// fills, a thin one costs nothing until something is written. Said rather
    /// than detected, because using a thin pool that exists for something else
    /// is not this agent's decision to make.
    #[arg(long, env = "VELSTRA_LVM_THIN_POOL")]
    lvm_thin_pool: Option<String>,

    /// The RBD pool volumes and their snapshots live in, for the ceph backend.
    #[arg(long, env = "VELSTRA_CEPH_POOL", default_value = "velstra-volumes")]
    ceph_pool: String,

    /// The RBD pool images live in.
    ///
    /// Separate from `--ceph-pool` on purpose: an image is written once and read
    /// for years, a volume is written constantly, and the two want different
    /// replication, placement and quota. Clones across pools cost nothing.
    #[arg(
        long,
        env = "VELSTRA_CEPH_IMAGE_POOL",
        default_value = "velstra-images"
    )]
    ceph_image_pool: String,

    /// The Ceph client to act as. Its keyring has to be where `rbd` looks —
    /// this agent does not manage credentials, because a process that could
    /// write its own keyring could grant itself a cluster.
    #[arg(long, env = "VELSTRA_CEPH_USER", default_value = "client.admin")]
    ceph_user: String,

    /// `ceph.conf`, when it is not in the default place.
    #[arg(long, env = "VELSTRA_CEPH_CONF")]
    ceph_conf: Option<String>,

    /// Publish an image into the cluster and exit, instead of running the agent.
    ///
    /// The image service, such as it is. Takes the resource name — which carries
    /// the digest — and the file, verifies the second against the first, and
    /// leaves a protected `@base` snapshot every volume can clone from.
    ///
    ///   --import-image projects/p1/images/sha256-… --from ./noble.raw
    #[arg(long, requires = "import_from")]
    import_image: Option<String>,

    /// The file `--import-image` publishes.
    #[arg(long = "from")]
    import_from: Option<PathBuf>,

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
    // Chosen before any TLS is spoken, because rustls will not choose: with
    // two providers compiled in (reqwest brings one, tokio-rustls the other) it
    // panics at first use — the same failure the API had, found the same way,
    // on a machine serving real traffic. This time it took the whole agent
    // down in a restart loop.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // Publishing an image is a one-shot act, not a loop, so it happens here and
    // exits rather than being a mode the agent runs in.
    if let (Some(image), Some(file)) = (&args.import_image, &args.import_from) {
        let ceph = CephPool::new(ceph_config(&args));
        return match ceph.import_image(image, file).await {
            Ok(true) => {
                tracing::info!(image, pool = %args.ceph_image_pool, "published");
                Ok(())
            }
            Ok(false) => {
                tracing::info!(image, "already in the cluster and clonable; nothing to do");
                Ok(())
            }
            Err(e) => Err(format!("{e}").into()),
        };
    }

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
        Backend::Ceph => {
            tracing::info!(
                pool = %args.ceph_pool,
                images = %args.ceph_image_pool,
                user = %args.ceph_user,
                "ceph pool"
            );
            Arc::new(CephPool::new(ceph_config(&args)))
        }
        Backend::Lvm => {
            let Some(group) = args.lvm_group.clone() else {
                // Refused here rather than on the first volume: a pool agent
                // that does not know which volume group it is for cannot be
                // pointed at one later without a restart, and a machine usually
                // has more than one.
                return Err(
                    "--lvm-group names the volume group this pool makes its volumes in, \
                            and the lvm backend has no default for it: a machine may have several \
                            and guessing would put a tenant's bytes in whichever came first"
                        .into(),
                );
            };
            let mut config = LvmConfig::new(&group);
            config.thin_pool = args.lvm_thin_pool.clone();
            tracing::info!(
                group = %group,
                thin = ?config.thin_pool,
                "lvm pool"
            );
            Arc::new(LvmPool::new(config))
        }
        Backend::Fake => {
            tracing::warn!(
                "using the fake pool: nothing is written to a disk and everything it \
                 holds dies with this process"
            );
            Arc::new(FakePool::new(args.fake_capacity_gib))
        }
    };

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
            let cell = Arc::new(ApiCell::for_pool(url, &token, &args.pool)?);
            // No store at all. A pool on any machine but the control plane's has
            // no etcd to open, and opening one anyway is what this agent used to
            // do — it died with `invalid uri: empty string` on a machine whose
            // seed named an API and no store, which is exactly the machine the
            // setup wizard offers when somebody answers `pool` on a second box.
            //
            // The in-memory store is what the typed stores are built over so
            // that their *types* still exist; nothing is written to it, because
            // every write goes through the sink below. It is never read either:
            // reading is the `ApiCell`'s.
            let nowhere: Arc<dyn Store> = Arc::new(velstra_cloud_store::MemoryStore::new());
            PoolAgent::reading(nowhere, config, storage, cell.clone()).through(cell)
        }
        None => {
            let store: Arc<dyn Store> = open_store(&args.store).await?;
            PoolAgent::new(store, config, storage)
        }
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

/// The cluster this agent was pointed at.
///
/// One place, because the import path and the agent path both need it and two
/// copies would be two chances to point them at different pools.
fn ceph_config(args: &Args) -> CephConfig {
    let mut config = CephConfig::new(&args.ceph_pool, &args.ceph_image_pool);
    config.user = args.ceph_user.clone();
    config.conf = args.ceph_conf.clone();
    config
}
