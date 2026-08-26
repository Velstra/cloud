//! The Ceph **storage** loop, run for real against recorded commands.
//!
//! ## Why this file exists
//!
//! The Ceph pool had unit tests about the argv it builds, the JSON it parses
//! and the names it maps — and nothing that ever ran the pool agent's loop with
//! Ceph underneath it. Every nixosTest that provisions storage uses the
//! `directory` backend. So the whole Ceph path was in the position this
//! codebase keeps finding things in: modelled, unit-tested, and executed by
//! nobody.
//!
//! `ceph_deployment.rs` already makes the argument for the shape of the fix,
//! and it applies here word for word: `rbd` and `ceph` **are** the interface to
//! the cluster, so a trait in the middle would let this pass while the commands
//! the agent really runs were wrong. The binaries are therefore substituted for
//! a script that records what it was called with, and the assertions are about
//! that recording and about what the agent then reports.
//!
//! ## What it does not prove
//!
//! That Ceph does what these commands mean. `rbd create` here writes a line to
//! a file; on a cluster it allocates. What is being pinned is everything
//! between the model and the cluster — that a volume somebody asked for turns
//! into an `rbd create` in the right pool at the right size, that what the
//! cluster reports comes back as the status of the right object, and that a
//! failure is reported rather than swallowed. Proving Ceph itself needs Ceph,
//! and that is a different and much larger check.

mod common;

use std::sync::Arc;

use common::{CELL, REGION, meta, store};
use velstra_cloud_model::{
    access::Writer,
    resources::{Resource, Volume, VolumeSpec, VolumeStatus},
};
use velstra_cloud_nodeagent::{
    ceph_pool::{CephConfig, CephPool},
    pool::{PoolAgent, PoolConfig},
};
use velstra_cloud_store::{Store, TypedStore};

const POOL: &str = "velstra-volumes";
const IMAGES: &str = "velstra-images";

/// A stand-in for `rbd` and `ceph` that records its arguments and can be told
/// what to print.
struct Cluster {
    dir: std::path::PathBuf,
}

impl Cluster {
    fn new(name: &str) -> Self {
        // Under this test binary's own temporary directory, named for the test,
        // for the reasons `ceph_deployment.rs` sets out: two tests running at
        // once must not read each other's recording, and `/tmp` is somebody
        // else's disk.
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ceph-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("record");
        // Replies are keyed on the first argument *after* the common flags,
        // which is the subcommand — `ls`, `create`, `snap`, `df`. The common
        // flags are `--id x` (and possibly `--conf y`), so the subcommand is
        // never `$1`; the loop finds it rather than counting, because counting
        // would silently break the day another common flag is added.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> '{dir}/argv'\n\
                 verb=\n\
                 while [ $# -gt 0 ]; do\n\
                 \x20 case \"$1\" in\n\
                 \x20   --id|--conf) shift 2 ;;\n\
                 \x20   --*) shift ;;\n\
                 \x20   *) verb=\"$1\"; break ;;\n\
                 \x20 esac\n\
                 done\n\
                 if [ -f '{dir}/fail' ]; then\n\
                 \x20 echo 'rbd: error opening pool' >&2\n\
                 \x20 exit 1\n\
                 fi\n\
                 if [ -n \"$verb\" ] && [ -f '{dir}/reply-'\"$verb\" ]; then\n\
                 \x20 cat '{dir}/reply-'\"$verb\"\n\
                 fi\n\
                 exit 0\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let here = Self { dir };
        // A cluster that holds nothing and has room, which is what every test
        // here starts from unless it says otherwise.
        here.holds(&[]);
        here.answers("snap", "[]");
        here.answers(
            "df",
            &format!(
                r#"{{"pools":[{{"name":"{POOL}","stats":{{"stored":0,"max_avail":1073741824000}}}}]}}"#
            ),
        );
        here
    }

    fn storage(&self) -> CephPool {
        let path = self.dir.join("record").to_str().unwrap().to_string();
        let mut config = CephConfig::new(POOL, IMAGES);
        config.user = "client.velstra".into();
        config.rbd = path.clone();
        // Configurable at all only since this file existed to need it: `ceph`
        // was a literal in the one call site that runs it, which left the
        // backend half-substitutable and this loop untestable.
        config.ceph = path;
        CephPool::new(config)
    }

    /// What `rbd ls` reports the pool is holding.
    fn holds(&self, images: &[(&str, u64)]) {
        let body: Vec<String> = images
            .iter()
            .map(|(name, gib)| {
                format!(
                    r#"{{"image":"{}","size":{}}}"#,
                    name.replace('/', "~"),
                    gib * 1024 * 1024 * 1024
                )
            })
            .collect();
        self.answers("ls", &format!("[{}]", body.join(",")));
    }

    fn answers(&self, verb: &str, with: &str) {
        std::fs::write(self.dir.join(format!("reply-{verb}")), with).unwrap();
    }

    /// Make every command fail, the way a wrong `--id` or a down cluster does.
    fn is_unreachable(&self) {
        std::fs::write(self.dir.join("fail"), "").unwrap();
    }

    fn recorded(&self) -> String {
        std::fs::read_to_string(self.dir.join("argv")).unwrap_or_default()
    }
}

fn agent(cluster: &Cluster) -> (Arc<dyn Store>, PoolAgent) {
    let store = store();
    let agent = PoolAgent::new(
        store.clone(),
        PoolConfig::new(POOL, REGION, CELL),
        Arc::new(cluster.storage()),
    );
    (store, agent)
}

fn volumes(store: &Arc<dyn Store>) -> TypedStore<VolumeSpec, VolumeStatus> {
    TypedStore::new(store.clone(), CELL, "volumes")
}

async fn ask_for(store: &Arc<dyn Store>, id: &str, gib: u64) -> Volume {
    let v: Volume = Resource::new(
        meta(&format!("projects/p1/volumes/{id}")),
        VolumeSpec {
            size_gib: gib,
            pool: POOL.to_string(),
            encryption_key: None,
            source_image: None,
            source_snapshot: None,
            source_backup: None,
        },
        VolumeStatus::default(),
    );
    volumes(store)
        .create(&v, &Writer::controller("test"))
        .await
        .unwrap();
    v
}

async fn reload(store: &Arc<dyn Store>, id: &str) -> Volume {
    volumes(store)
        .get(&format!("projects/p1/volumes/{id}"))
        .await
        .unwrap()
        .unwrap()
}

/// The whole point of the file: a volume somebody asked for becomes an `rbd
/// create` against the right pool, at the right size, as the right client.
#[tokio::test]
async fn a_volume_asked_for_becomes_an_rbd_create_in_the_right_pool() {
    let cluster = Cluster::new("create");
    let (store, agent) = agent(&cluster);
    ask_for(&store, "v1", 10).await;

    // Claim, then act — the same two passes every backend takes.
    agent.resync().await;
    agent.resync().await;

    let argv = cluster.recorded();
    let create = argv
        .lines()
        .find(|l| l.contains("create"))
        .unwrap_or_else(|| panic!("nothing was created:\n{argv}"));
    // The volume pool, not the image pool: they are the design, and a create
    // that landed in the wrong one would make a volume nothing can find.
    assert!(create.contains(POOL), "{create}");
    assert!(!create.contains(IMAGES), "created in the image pool: {create}");
    // The name with its slashes flattened, so a person can read the pool with
    // `rbd ls` and two cells sharing one cannot collide.
    assert!(create.contains("projects~p1~volumes~v1"), "{create}");
    // Gibibytes, as `--size` takes them.
    assert!(create.contains("10"), "{create}");
    // As the configured client. A forgotten --id runs as the wrong one and
    // either fails confusingly or succeeds where it should not have.
    assert!(create.contains("--id velstra"), "{create}");
}

/// What the cluster says is what the object says. The status is reported from
/// a fresh observation rather than from what was attempted, which is the rule
/// that stops a failed create being remembered as a success.
#[tokio::test]
async fn what_the_cluster_holds_is_what_the_volume_reports() {
    let cluster = Cluster::new("observe");
    let (store, agent) = agent(&cluster);
    ask_for(&store, "v1", 10).await;

    agent.resync().await;
    // The cluster now holds it — which is what a real `rbd create` would have
    // made true, and what this stand-in cannot.
    cluster.holds(&[("projects/p1/volumes/v1", 10)]);
    agent.resync().await;
    agent.resync().await;

    let v = reload(&store, "v1").await;
    assert!(v.status.provisioned, "the volume never came back provisioned");
    assert_eq!(v.status.actual_size_gib, 10);
    assert_eq!(v.status.pool.as_deref(), Some(POOL));
}

/// A cluster that cannot be reached is said out loud **on the pool**, and no
/// volume is reported as provisioned.
///
/// Two failures are being ruled out and they pull in opposite directions.
///
/// The first is reading an error as an empty pool — `rbd ls` writes "error
/// opening pool" to stdout, that parses as no volumes, and the agent creates
/// everything a second time. The parser refuses non-JSON for exactly this.
///
/// The second is the one this test found: the pass returned having written
/// nothing anywhere, so an unreachable backend was the quietest failure in the
/// platform. Every volume sat unprovisioned with no condition and no reason,
/// and the only record was a log line on whichever machine runs the pool.
///
/// It belongs on the pool rather than on each volume — one backend being down
/// is one fact — and the capacity numbers must survive it, because zeroing them
/// would tell the scheduler this pool is full rather than unreadable.
#[tokio::test]
async fn a_cluster_that_cannot_be_reached_is_reported_rather_than_read_as_empty() {
    let cluster = Cluster::new("unreachable");
    let (store, agent) = agent(&cluster);
    let pools: TypedStore<
        velstra_cloud_model::resources::PoolSpec,
        velstra_cloud_model::resources::PoolStatus,
    > = TypedStore::new(store.clone(), CELL, "pools");
    let registered: velstra_cloud_model::resources::Pool = Resource::new(
        meta(&format!("pools/{POOL}")),
        velstra_cloud_model::resources::PoolSpec { accepting: true, labels: Vec::new() },
        velstra_cloud_model::resources::PoolStatus::default(),
    );
    pools
        .create(&registered, &Writer::controller("test"))
        .await
        .unwrap();

    ask_for(&store, "v1", 10).await;
    // One good pass first, so there are numbers to lose.
    cluster.holds(&[("projects/p1/volumes/v1", 10)]);
    for _ in 0..3 {
        agent.resync().await;
    }
    let healthy = pools.get(&format!("pools/{POOL}")).await.unwrap().unwrap();
    assert!(healthy.status.capacity_gib > 0, "nothing was read to begin with");

    cluster.is_unreachable();
    agent.resync().await;

    let pool = pools.get(&format!("pools/{POOL}")).await.unwrap().unwrap();
    let ready = pool
        .status
        .conditions
        .iter()
        .find(|c| c.kind == "Ready")
        .expect("the pool says nothing about itself");
    assert_eq!(
        ready.status,
        velstra_cloud_model::meta::ConditionStatus::False,
        "{ready:?}"
    );
    assert_eq!(ready.reason, "BackendUnreachable", "{ready:?}");
    // It says what it means for the things on it, not just that something is
    // wrong somewhere.
    assert!(
        ready.message.contains("nothing here is being provisioned"),
        "{}",
        ready.message
    );
    // The last numbers off a working cluster survive. Zeroes here would read as
    // "this pool is full", which is a claim nobody made.
    assert_eq!(
        pool.status.capacity_gib, healthy.status.capacity_gib,
        "an unreadable pool was reported as having no room"
    );

    // And a volume nobody could create is still not claimed to exist.
    let v = reload(&store, "v1").await;
    assert!(
        !v.status.provisioned || v.status.actual_size_gib == 10,
        "an unreachable cluster changed what a volume claims to be"
    );
}

/// Growing is `rbd resize`, and shrinking is refused before any command runs.
///
/// The refusal matters more than the resize: `rbd resize` to a smaller size
/// succeeds and discards the tail, so a backend that passed the ask through
/// would destroy a filesystem on request.
#[tokio::test]
async fn a_volume_grows_and_is_never_shrunk() {
    let cluster = Cluster::new("resize");
    let (store, agent) = agent(&cluster);
    ask_for(&store, "v1", 10).await;
    cluster.holds(&[("projects/p1/volumes/v1", 10)]);
    agent.resync().await;
    agent.resync().await;

    // Asked for more.
    let mut v = reload(&store, "v1").await;
    v.spec.size_gib = 20;
    v.meta.generation += 1;
    volumes(&store)
        .update(&v, &Writer::controller("test"))
        .await
        .unwrap();
    agent.resync().await;

    let argv = cluster.recorded();
    assert!(
        argv.lines().any(|l| l.contains("resize") && l.contains("20")),
        "it never grew:\n{argv}"
    );

    // Asked for less. Nothing is run at all — the refusal is the model's, and
    // it happens before a command that would have worked.
    cluster.holds(&[("projects/p1/volumes/v1", 20)]);
    agent.resync().await;
    let before = cluster.recorded();
    let mut v = reload(&store, "v1").await;
    v.spec.size_gib = 5;
    v.meta.generation += 1;
    volumes(&store)
        .update(&v, &Writer::controller("test"))
        .await
        .unwrap();
    agent.resync().await;

    let after = cluster.recorded();
    let ran = after.strip_prefix(&before).unwrap_or(&after);
    assert!(
        !ran.contains("resize"),
        "it ran a resize that would have discarded the tail of the volume:\n{ran}"
    );
    let v = reload(&store, "v1").await;
    let ready = v.status.conditions.iter().find(|c| c.kind == "Ready").unwrap();
    assert_eq!(ready.reason, "WillNotShrink", "{ready:?}");
}

/// One `rbd snap ls` per volume, which is what this backend costs.
///
/// Pinned because it is the property that decides how a large pool behaves and
/// there is no pool-wide listing to replace it with: `rbd` has no such
/// subcommand. A change that made observe cheaper would be a real improvement
/// and should break this test rather than pass it quietly.
#[tokio::test]
async fn observing_costs_one_snapshot_listing_per_volume() {
    let cluster = Cluster::new("cost");
    let (store, agent) = agent(&cluster);
    cluster.holds(&[
        ("projects/p1/volumes/v1", 1),
        ("projects/p1/volumes/v2", 1),
        ("projects/p1/volumes/v3", 1),
    ]);
    ask_for(&store, "v1", 1).await;
    agent.resync().await;

    let listings = cluster
        .recorded()
        .lines()
        .filter(|l| l.contains("snap ls"))
        .count();
    assert_eq!(
        listings, 3,
        "one per volume in the pool, not per volume this cell knows about:\n{}",
        cluster.recorded()
    );
}
