//! The agent that owns a storage pool.
//!
//! Same loop as the node agent, and deliberately so:
//!
//! ```text
//! observe this pool → ask the pure function what to do → do it → report status
//! ```
//!
//! It is a separate agent rather than a job inside the node agent because a pool
//! is not a machine. Several nodes reach one Ceph pool; one node may export
//! three LVM groups. Tying storage to whichever hypervisor happened to be asked
//! is how a volume ends up unreachable when that node is drained.
//!
//! Nothing here remembers what it did. `observe()` re-reads the backend on every
//! pass, so an agent that was killed mid-`lvcreate` comes back knowing exactly
//! what the disks know — which is the only thing that is true.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use velstra_cloud_model::{
    access::Writer,
    meta::{Condition, ConditionStatus, Placement, Timestamp, set_condition},
    resources::{
        POOL_RELEASE_FINALIZER, Pool, PoolSpec, PoolStatus, Snapshot, SnapshotSpec, SnapshotStatus,
        Volume, VolumeSpec, VolumeStatus,
    },
    storage::{
        SeenInPool, SeenSnapshot, SnapshotAction, VolumeAction, VolumeSource, reconcile_snapshot,
        reconcile_volume, snapshot_condition, volume_condition,
    },
};
use velstra_cloud_store::{Store, TypedStore};

use crate::{
    agent::Pass,
    cell::PoolReader,
    host::{HostError, Result},
    reporting,
};

/// What a pool looks like from outside, right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolState {
    /// Volume resource name to its size in GiB, for the volumes this pool
    /// actually has. Absent means absent — not "not yet reported".
    pub volumes: BTreeMap<String, u64>,
    pub capacity_gib: u64,
    /// `lvm`, `zfs`, `ceph`, `directory`. Read from the backend rather than
    /// configured, because an operator's guess about what a machine is running
    /// is not a fact about it.
    pub backend: String,
    /// Every copy this pool holds, by snapshot resource name.
    ///
    /// Observed, like everything else here, and counted rather than trusted:
    /// this is what makes destroying a volume safe or unsafe, and the backend is
    /// the only place that knows about a copy somebody took with a shell.
    pub snapshots: BTreeMap<String, SnapshotInPool>,
}

/// One copy, as the pool holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotInPool {
    /// The volume it was taken from. What makes destroying that volume unsafe.
    pub volume: String,
    /// How large the volume was at the moment the copy was made, which is the
    /// smallest volume that can be made from it.
    pub gib: u64,
}

impl PoolState {
    /// What this pool sees of one volume.
    ///
    /// In one place because the two call sites — before acting and after — must
    /// answer the same question, and a second copy of this arithmetic is how
    /// they would come to differ.
    pub fn of(&self, volume: &str) -> SeenInPool {
        SeenInPool {
            exists: self.volumes.contains_key(volume),
            gib: self.volumes.get(volume).copied().unwrap_or(0),
            snapshots: self
                .snapshots
                .values()
                .filter(|copy| copy.volume == volume)
                .count() as u32,
        }
    }

    /// What this pool sees of one snapshot. Same reason as `of`: the pass asks
    /// before acting and again after, and two copies of this would drift.
    pub fn snapshot_of(&self, snapshot: &str) -> SeenSnapshot {
        match self.snapshots.get(snapshot) {
            Some(copy) => SeenSnapshot {
                exists: true,
                gib: copy.gib,
            },
            None => SeenSnapshot::default(),
        }
    }
}

/// Is this path there, can this process write to it, and how much room is left.
///
/// Answered by *trying*, not by reading permissions: a directory can be
/// writable by its mode and read-only because the filesystem under it is, or
/// because the mount is gone and what is left is the empty mountpoint on the
/// root disk — which is the failure this exists to catch, and the one a stat
/// cannot see.
fn look_at_target(path: &str) -> (bool, u64, Option<String>) {
    let dir = std::path::Path::new(path);
    if !dir.is_dir() {
        return (
            false,
            0,
            Some(format!(
                "{path} is not a directory on this machine. Is the target mounted here?"
            )),
        );
    }
    // Written and removed. The alternative is trusting the mode bits, which say
    // nothing about a read-only filesystem underneath them.
    let probe = dir.join(".velstra-writable");
    if let Err(e) = std::fs::write(&probe, b"") {
        return (false, 0, Some(format!("{path} cannot be written: {e}")));
    }
    let _ = std::fs::remove_file(&probe);
    (true, free_gib(dir), None)
}

/// How much room is left where this path is. Zero when it cannot be asked,
/// which reads as "unknown" everywhere it is shown rather than as "full".
fn free_gib(dir: &std::path::Path) -> u64 {
    let Ok(text) = std::process::Command::new("df")
        .arg("-BG")
        .arg("--output=avail")
        .arg(dir)
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&text.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.trim().trim_end_matches('G').parse().ok())
        .unwrap_or(0)
}

/// The cell's backups and targets, as one pass has them.
#[derive(Default)]
struct Copies {
    backups: Vec<velstra_cloud_model::resources::Backup>,
    targets: Vec<velstra_cloud_model::resources::BackupTarget>,
}

impl Copies {
    /// Where this backup's bytes are, if there are any.
    ///
    /// `None` when the backup is unknown, when its copy was never made, or when
    /// its target is not one this machine knows about — three different reasons
    /// and one answer, because the caller's next move is the same for all
    /// three: refuse, and say so on the volume.
    fn path_of(&self, backup: &str) -> Option<String> {
        let backup = self
            .backups
            .iter()
            .find(|b| b.meta.name.to_string() == backup)?;
        if !backup.status.taken {
            return None;
        }
        let target = self
            .targets
            .iter()
            .find(|t| t.meta.name.to_string() == backup.spec.target)?;
        Some(backup_path(
            &target.spec.path,
            &backup.meta.name.to_string(),
        ))
    }
}

/// Where a pool reads the bytes a new volume starts as.
///
/// The *agent's* view of [`VolumeSource`], and the difference is the last
/// variant: a backup has already been resolved to a path by the time a backend
/// sees it. A path is a fact about one machine's filesystem, so the model must
/// not carry one — and a backend cannot resolve a backup's name, because that
/// needs the backup, the target it names, and the target's path. Whoever has
/// all three does the resolving, once, here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin<'a> {
    Blank,
    Image(&'a str),
    Snapshot(&'a str),
    /// A file on this machine — a backup on a target that is mounted here.
    File(&'a str),
}

/// Where one backup's bytes go inside a target.
///
/// The resource's name with its slashes flattened, exactly as an image is
/// stored on a node — so a person looking at a target with `ls` can read which
/// copy is which, and two cells sharing a target cannot collide.
pub fn backup_path(target_path: &str, backup: &str) -> String {
    format!(
        "{}/{}",
        target_path.trim_end_matches('/'),
        backup.replace('/', "~")
    )
}

/// A storage backend. One implementation per real technology; the fake is what
/// every test uses.
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    async fn observe(&self) -> Result<PoolState>;
    /// Create the backing store, from whatever `source` says it starts as.
    ///
    /// Cloning is part of creating it, never a step afterwards — a volume that
    /// exists blank for one pass is a volume a guest can be started from, and
    /// the guest that boots it finds nothing.
    ///
    /// Takes the model's own [`VolumeSource`] rather than a pair of options, so
    /// "an image" and "a snapshot" cannot both be set by a caller that has not
    /// thought about which wins.
    async fn provision(
        &self,
        volume: &str,
        gib: u64,
        source: Origin<'_>,
        encryption_key: Option<&str>,
    ) -> Result<()>;
    /// Where this backend keeps a volume's bytes, in the words the machine
    /// opening them needs: a path for a directory or LVM pool, an `rbd:` name
    /// for Ceph.
    ///
    /// `None` when the volume is not this pool's, or when this backend has
    /// nothing a hypervisor could open directly.
    ///
    /// It is asked of the backend rather than derived from the volume's name
    /// because deriving it means every backend agreeing on a layout none of them
    /// share — which is precisely the mistake that left attaching a disk broken:
    /// the node built `…/instances/<guest>/<volume>`, a path nothing ever writes.
    fn at(&self, volume: &str) -> Option<String>;

    async fn grow(&self, volume: &str, to_gib: u64) -> Result<()>;
    async fn destroy(&self, volume: &str) -> Result<()>;

    /// Copy `volume` as it stands, under this name.
    ///
    /// Not "start copying": when this returns the copy is a copy of the moment
    /// it was asked for. A backend that returned early would have the pool
    /// report `taken` over a half-written file, and `reconcile_snapshot`
    /// deliberately never takes a snapshot twice — so a copy reported before it
    /// is one is a copy that is wrong for ever.
    async fn take_snapshot(&self, snapshot: &str, volume: &str) -> Result<()>;

    /// Destroy the copy. The volume it was taken from is untouched.
    async fn destroy_snapshot(&self, snapshot: &str) -> Result<()>;

    /// Write this volume's bytes out to `path`, and answer how many landed.
    ///
    /// The other half of a snapshot, and deliberately a different method: a
    /// snapshot stays in the pool and is cheap because it shares blocks with
    /// the volume; this leaves the pool entirely, which is the whole point and
    /// also why it costs a full copy. A backend that cannot do it says so
    /// rather than pretending — an empty file reported as a backup is worse
    /// than no backup, because somebody will believe it.
    ///
    /// Like `take_snapshot`, this returns when the copy *is* one. A backend
    /// that returned early would have the agent report `taken` over a
    /// half-written file, and a backup is never taken twice: the second copy
    /// would be of a different moment under a name somebody trusts.
    async fn copy_out(&self, volume: &str, path: &str) -> Result<u64>;
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub pool: String,
    pub placement: Placement,
    pub resync: Duration,
    pub agent_version: String,
}

impl PoolConfig {
    pub fn new(pool: &str, region: &str, cell: &str) -> Self {
        Self {
            pool: pool.to_string(),
            placement: Placement::new(region, cell),
            resync: Duration::from_secs(30),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub struct PoolAgent {
    config: PoolConfig,
    writer: Writer,
    /// Written, not read: what this pool reports about a volume it holds. What
    /// it is *told* about comes through `cell`.
    volumes: TypedStore<VolumeSpec, VolumeStatus>,
    snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
    backups: TypedStore<
        velstra_cloud_model::backup::BackupSpec,
        velstra_cloud_model::backup::BackupStatus,
    >,
    targets: TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    >,
    pools: TypedStore<PoolSpec, PoolStatus>,
    /// Everything this pool reads about the cell — the only thing that grew with
    /// the cell rather than with this pool's own work. See [`crate::cell`].
    cell: Arc<dyn PoolReader>,
    storage: Arc<dyn Storage>,
}

impl PoolAgent {
    /// Reads the store directly. [`PoolAgent::reading`] is the same agent
    /// pointed at something that hands it only its own share.
    pub fn new(store: Arc<dyn Store>, config: PoolConfig, storage: Arc<dyn Storage>) -> Self {
        let reader = Arc::new(crate::cell::StorePool::new(
            store.clone(),
            &config.placement.cell,
        ));
        Self::reading(store, config, storage, reader)
    }

    pub fn reading(
        store: Arc<dyn Store>,
        config: PoolConfig,
        storage: Arc<dyn Storage>,
        reader: Arc<dyn PoolReader>,
    ) -> Self {
        let cell = config.placement.cell.clone();
        Self {
            // A pool writes under its own name, so a store refusal says which
            // pool tried — the same identity discipline as a node.
            writer: Writer::agent(&config.pool),
            volumes: TypedStore::new(store.clone(), &cell, "volumes"),
            snapshots: TypedStore::new(store.clone(), &cell, "snapshots"),
            backups: TypedStore::new(store.clone(), &cell, "backups"),
            targets: TypedStore::new(store.clone(), &cell, "backup-targets"),
            pools: TypedStore::new(store, &cell, "pools"),
            config,
            cell: reader,
            storage,
        }
    }

    pub fn pool(&self) -> &str {
        &self.config.pool
    }

    /// Resync on a timer until `shutdown` completes.
    ///
    /// No watch, unlike the node agent, and that is a considered difference
    /// rather than a gap: storage work is measured in seconds to minutes, so the
    /// latency a watch buys is lost in the noise of an `lvcreate`. When it stops
    /// being true, the seam is here and nothing else changes.
    pub async fn run(&self, shutdown: impl std::future::Future<Output = ()> + Send) {
        let mut ticker = tokio::time::interval(self.config.resync);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => return,
                _ = ticker.tick() => {
                    let pass = self.resync().await;
                    if pass.failures > 0 || pass.refused > 0 {
                        tracing::warn!(
                            pool = %self.config.pool,
                            failures = pass.failures,
                            refused = pass.refused,
                            "a pool pass did not go cleanly"
                        );
                    }
                }
            }
        }
    }

    /// One full pass: every volume assigned here, then the pool's own object.
    pub async fn resync(&self) -> Pass {
        let mut pass = Pass::default();

        let seen = match self.storage.observe().await {
            Ok(seen) => seen,
            Err(e) => {
                // Without a reading of the backend there is nothing to compare
                // against, and acting on the last known picture is how a volume
                // gets created twice. So nothing is *done* this pass — but it
                // is said.
                //
                // It used to only be logged, and that made an unreachable
                // backend the quietest failure in the platform: every volume on
                // it sat unprovisioned with no condition, no reason and no
                // event, and the only record was a line on whichever machine
                // runs this pool. Which is exactly the argument `backup_trouble`
                // makes a few hundred lines down — somebody asking "why is
                // there no volume" is looking at the object, months later, and a
                // log line is not where they will look.
                //
                // On the pool and not on the volumes: one backend being down is
                // one fact, and writing it onto a hundred objects would be a
                // hundred writes saying the same thing, all of which then have
                // to be cleaned up.
                tracing::error!(error = %e, "could not read this pool; doing nothing this pass");
                pass.failures += 1;
                self.unreachable(&e.to_string(), &mut pass).await;
                return pass;
            }
        };

        let volumes = match self.cell.volumes().await {
            Ok(volumes) => volumes,
            Err(e) => {
                tracing::error!(error = %e, "could not list volumes");
                pass.failures += 1;
                return pass;
            }
        };
        // Read once for the whole pass and handed to both halves: a volume
        // being restored needs to find its copy, and a backup being taken needs
        // its target. Two lists rather than one per object, for the same reason
        // the targets are read once rather than per backup.
        let mut copies = self.copies(&mut pass).await;
        // The targets first, and their answers kept: a volume being restored
        // and a backup being taken are both judged against what a target says
        // about itself, and a list read before anybody looked says nothing.
        copies.targets = self.target_sweep(copies.targets, &mut pass).await;

        for volume in volumes.iter().filter(|v| v.spec.pool == self.config.pool) {
            self.volume_pass(volume, &seen, &copies, &mut pass).await;
        }

        // Volumes first, copies second: a snapshot is of a volume, and a copy
        // asked for in the same pass that provisions its source would be a copy
        // of something not there yet. The other order converges one pass sooner
        // when a volume and its snapshots are deleted together, which is the
        // cheaper of the two problems to have.
        match self.cell.snapshots().await {
            Ok(snapshots) => {
                for snapshot in snapshots.iter().filter(|s| s.spec.pool == self.config.pool) {
                    self.snapshot_pass(snapshot, &mut pass).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "could not list snapshots");
                pass.failures += 1;
            }
        }

        // Copies that leave the pool, last: a backup is of a volume, and one
        // asked for in the same pass that provisions its source would be a copy
        // of something not there yet.
        self.backup_sweep(&copies, &mut pass).await;

        self.pool_pass(&volumes, &seen, &mut pass).await;
        pass
    }

    async fn volume_pass(
        &self,
        stored: &Volume,
        seen: &PoolState,
        copies: &Copies,
        pass: &mut Pass,
    ) {
        let name = stored.meta.name.to_string();

        // Claim first, act never before. A volume whose status another pool
        // still owns is one that pool has not let go of, and provisioning it
        // here would put the same name on two backends.
        match stored.status.pool.as_deref() {
            Some(owner) if owner == self.config.pool => {}
            Some(_) => return,
            None => {
                let me = self.config.pool.clone();
                reporting::claim(
                    &self.volumes,
                    stored,
                    |status| status.pool = Some(me),
                    &self.writer,
                    pass,
                )
                .await;
                return;
            }
        }

        let here = seen.of(&name);

        let mut next = stored.clone();
        let mut trouble = None;
        for action in reconcile_volume(stored, here) {
            match self.perform(&action, copies).await {
                Ok(()) => pass.actions += 1,
                Err(e) => {
                    tracing::warn!(volume = %name, error = %e, "this volume could not be brought into line");
                    pass.failures += 1;
                    trouble = Some(e.to_string());
                    break;
                }
            }
        }

        // Re-read: acting changed the pool, and reporting the picture from
        // before it would be a status that describes a world one pass old.
        let after = match self.storage.observe().await {
            Ok(after) => after.of(&name),
            Err(e) => {
                tracing::error!(error = %e, "could not re-read this pool after acting");
                pass.failures += 1;
                here
            }
        };

        next.status.provisioned = after.exists;
        next.status.actual_size_gib = after.gib;
        // Where the bytes are, for whoever has to open them. Only once they
        // exist: a path to a volume that is not provisioned yet is a promise
        // this pool cannot keep, and a node acting on it fails in the confusing
        // way rather than waiting in the obvious one.
        next.status.at = after.exists.then(|| self.storage.at(&name)).flatten();
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            match trouble {
                Some(why) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "PoolFailed",
                    &why,
                    stored.meta.generation,
                ),
                None => volume_condition(stored, after),
            },
        );
        // The pool cannot drop its own finalizer — `meta` belongs to a
        // controller — so it publishes the one fact a controller needs to drop
        // it. An explicit condition rather than a controller inferring release
        // from `provisioned == false`, because that inference would be a second
        // definition of "let go" living somewhere else, and the two would
        // disagree the first time a volume failed to provision.
        set_condition(
            &mut next.status.conditions,
            release_condition(
                !after.exists,
                stored.meta.is_deleting(),
                stored.meta.generation,
            ),
        );
        reporting::report(&self.volumes, stored, next, &self.writer, pass).await;
    }

    /// Every copy this pool owes a target.
    ///
    /// Read as two lists — the backups and the targets — because a target is
    /// one row per place copies are kept, and forty backups pointing at three
    /// targets should be three reads rather than forty.
    /// The cell's copies and the places they are kept, read once per pass.
    ///
    /// Empty when they cannot be read, and that direction is deliberate: a
    /// restore then refuses with "no copy this pool can read", which is
    /// recoverable and true, rather than provisioning a blank volume under a
    /// name somebody expects their data to be behind.
    async fn copies(&self, pass: &mut Pass) -> Copies {
        let backups = match self.cell.backups().await {
            Ok(backups) => backups,
            Err(e) => {
                tracing::error!(error = %e, "could not list backups");
                pass.failures += 1;
                return Copies::default();
            }
        };
        let targets = match self.cell.backup_targets().await {
            Ok(targets) => targets,
            Err(e) => {
                tracing::error!(error = %e, "could not list backup targets");
                pass.failures += 1;
                return Copies::default();
            }
        };
        Copies { backups, targets }
    }

    /// What each target looks like from this machine.
    ///
    /// The one thing worth reporting about a target, and it was consulted long
    /// before anything wrote it: `may_back_up` refuses a target that is not
    /// writable, so with nobody reporting, *every* backup was refused — the
    /// platform holding a door shut and blaming the door.
    ///
    /// Claimed like anything else, and the claim matters more here than usual:
    /// a target can be mounted on several machines, and two pools reporting on
    /// one object would be two answers to "is the mount up" from two different
    /// mounts.
    /// Hands back what it wrote, rather than leaving the pass to re-read it.
    ///
    /// The targets were read at the top of the pass, and looking at them
    /// changes them — so a backup judged against the list as it was read would
    /// be judged against a target that had not reported yet, and refused for a
    /// pass. Returning the updated rows costs nothing and removes the whole
    /// question.
    async fn target_sweep(
        &self,
        targets: Vec<velstra_cloud_model::resources::BackupTarget>,
        pass: &mut Pass,
    ) -> Vec<velstra_cloud_model::resources::BackupTarget> {
        let mut out = Vec::with_capacity(targets.len());
        for target in targets {
            let target = &target;
            // Only what an operator gave this pool. A target names its reporter
            // in its spec; one that names another pool — or nobody — is not
            // this agent's to answer for, and the access rule would refuse the
            // write anyway.
            if target.spec.agent != self.config.pool {
                out.push(target.clone());
                continue;
            }
            // Claimed in the same breath as the first report: the assignment
            // is already an operator's decision, so there is nothing to race
            // over and nothing to wait a pass for.
            let mut next = target.clone();
            if next.status.agent.as_deref() != Some(self.config.pool.as_str()) {
                next.status.agent = Some(self.config.pool.clone());
            }
            out.push(self.look_and_report(target, next, pass).await);
        }
        out
    }

    /// Look at one target and say what is there.
    async fn look_and_report(
        &self,
        target: &velstra_cloud_model::resources::BackupTarget,
        mut next: velstra_cloud_model::resources::BackupTarget,
        pass: &mut Pass,
    ) -> velstra_cloud_model::resources::BackupTarget {
        {
            let (writable, free_gib, why) = look_at_target(&target.spec.path);
            next.status.writable = Some(writable);
            next.status.free_gib = free_gib;
            next.status.observed_generation = target.meta.generation;
            set_condition(
                &mut next.status.conditions,
                match &why {
                    Some(why) => Condition::new(
                        "Ready",
                        ConditionStatus::False,
                        "NotWritable",
                        why,
                        target.meta.generation,
                    ),
                    None => Condition::new(
                        "Ready",
                        ConditionStatus::True,
                        "Writable",
                        "",
                        target.meta.generation,
                    ),
                },
            );
            reporting::report(&self.targets, target, next.clone(), &self.writer, pass).await;
            next
        }
    }

    async fn backup_sweep(&self, copies: &Copies, pass: &mut Pass) {
        for backup in copies
            .backups
            .iter()
            .filter(|b| b.spec.pool == self.config.pool)
        {
            self.backup_pass(backup, &copies.targets, pass).await;
        }
        self.verify_sweep(copies, pass).await;
    }

    /// Read one copy back per target and check it against its digest.
    ///
    /// Until this existed the platform could say a backup had been *written*
    /// and nothing more. That is a weaker claim than it looks: bit rot, a
    /// filesystem that lied about a flush, a target quietly remounted read-only
    /// over an old copy of itself — none of them change the fact that bytes
    /// were once written, and all of them are found at restore time, which is
    /// the worst moment to find anything.
    ///
    /// One copy per target per pass. The pass also provisions volumes and takes
    /// snapshots, and those are not optional; reading every overdue copy would
    /// let a target with a hundred of them starve the work somebody is waiting
    /// on. See [`velstra_cloud_model::backup::next_to_verify`].
    async fn verify_sweep(&self, copies: &Copies, pass: &mut Pass) {
        for target in &copies.targets {
            let target_name = target.meta.name.to_string();
            // Only the agent that owns the target reads from it. A shared mount
            // visible to three machines would otherwise be read by all three,
            // which triples the I/O to answer one question.
            if target.spec.agent != self.config.pool {
                continue;
            }
            if target.spec.verify_every_hours == 0 {
                continue;
            }

            // Only copies this pool made: a backup's bytes came out of one pool
            // and its object is claimed by that pool's agent.
            let mine: Vec<velstra_cloud_model::backup::CopyView> = copies
                .backups
                .iter()
                .filter(|b| b.spec.pool == self.config.pool)
                .filter(|b| b.spec.target == target_name)
                .filter(|b| b.status.taken)
                .filter_map(|b| {
                    Some(velstra_cloud_model::backup::CopyView {
                        name: b.meta.name.to_string(),
                        taken_at: b.status.taken_at?,
                        verified_at: b.status.verified_at,
                        deleting: b.meta.is_deleting(),
                    })
                })
                .collect();

            let Some(chosen) = velstra_cloud_model::backup::next_to_verify(
                target.spec.verify_every_hours,
                &mine,
                Timestamp::now(),
            ) else {
                continue;
            };
            let Some(stored) = copies
                .backups
                .iter()
                .find(|b| b.meta.name.to_string() == chosen)
            else {
                continue;
            };
            self.verify_one(stored, &target.spec.path, pass).await;
        }
    }

    /// Read one copy back, compare, and say what was found on the backup.
    async fn verify_one(
        &self,
        stored: &velstra_cloud_model::resources::Backup,
        target_path: &str,
        pass: &mut Pass,
    ) {
        let name = stored.meta.name.to_string();
        let path = backup_path(target_path, &name);

        // A copy written before digests existed. It is not sound and not
        // broken: nobody can tell, and saying so is the only honest answer.
        // Recording a digest now would bless whatever is on the target today,
        // which is exactly the question being asked.
        let Some(want) = stored.status.digest.clone() else {
            let mut next = stored.clone();
            next.status.verify_error = Some(
                "no digest was recorded when this copy was made, so reading it back \
                 proves nothing; the next copy of this volume will carry one"
                    .into(),
            );
            next.status.observed_generation = stored.meta.generation;
            set_condition(
                &mut next.status.conditions,
                Condition::new(
                    "Ready",
                    ConditionStatus::True,
                    "Unverifiable",
                    "the copy is here; whether it is intact cannot be established",
                    stored.meta.generation,
                ),
            );
            reporting::report(&self.backups, stored, next, &self.writer, pass).await;
            return;
        };

        pass.actions += 1;
        let found = match crate::hostfs::sha256_file(std::path::Path::new(&path)).await {
            Ok(hex) => format!("sha256:{hex}"),
            Err(e) => {
                // The copy could not be read at all — gone, or a mount that is
                // no longer there. Louder than a mismatch, not different in
                // kind: either way this is not a backup any more.
                pass.failures += 1;
                self.verify_failed(
                    stored,
                    format!("this copy could not be read back from {path}: {e}"),
                    "Unreadable",
                    pass,
                )
                .await;
                return;
            }
        };

        if found != want {
            pass.failures += 1;
            self.verify_failed(
                stored,
                format!(
                    "this copy no longer matches what was written: expected {want}, \
                     found {found}. The bytes are still on the target and nothing here \
                     will remove them — a restore from this copy would not be the volume \
                     it was made from"
                ),
                "DigestMismatch",
                pass,
            )
            .await;
            return;
        }

        let mut next = stored.clone();
        next.status.verified_at = Some(Timestamp::now());
        next.status.verify_error = None;
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::True,
                "Verified",
                &format!("read back from {path} and it matches"),
                stored.meta.generation,
            ),
        );
        reporting::report(&self.backups, stored, next, &self.writer, pass).await;
    }

    /// Say on the backup that reading it back did not work out.
    ///
    /// The copy is never deleted. A failed verification is the one moment
    /// somebody has to look themselves: it may be the copy that rotted, or the
    /// filesystem under it, or a restore already running from this very file.
    /// Destroying the only artefact would take that decision away, and it is
    /// not the platform's to take.
    async fn verify_failed(
        &self,
        stored: &velstra_cloud_model::resources::Backup,
        why: String,
        reason: &str,
        pass: &mut Pass,
    ) {
        tracing::error!(backup = %stored.meta.name, reason, "{why}");
        let mut next = stored.clone();
        next.status.verify_error = Some(why.clone());
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::False,
                reason,
                &why,
                stored.meta.generation,
            ),
        );
        reporting::report(&self.backups, stored, next, &self.writer, pass).await;
    }

    /// One backup: claim it, copy the bytes out, report what is on the target.
    ///
    /// The same shape as a snapshot and the same reason for it — `status.taken`
    /// is consulted, not just reported, so a copy is never made twice. What is
    /// different is where the bytes go: out of the pool entirely, which is the
    /// whole point of a backup and also why it costs a full copy rather than a
    /// share of blocks.
    async fn backup_pass(
        &self,
        stored: &velstra_cloud_model::resources::Backup,
        targets: &[velstra_cloud_model::resources::BackupTarget],
        pass: &mut Pass,
    ) {
        let name = stored.meta.name.to_string();

        match stored.status.agent.as_deref() {
            Some(owner) if owner == self.config.pool => {}
            Some(_) => return,
            None => {
                let me = self.config.pool.clone();
                reporting::claim(
                    &self.backups,
                    stored,
                    |status| status.agent = Some(me),
                    &self.writer,
                    pass,
                )
                .await;
                return;
            }
        }

        // Already made. This is every pass after the first, and it has to cost
        // one comparison: a copy that exists is never made again, because a
        // copy made later is of a different moment under a name somebody
        // trusts.
        if stored.status.taken {
            return;
        }

        let Some(target) = targets
            .iter()
            .find(|t| t.meta.name.to_string() == stored.spec.target)
        else {
            self.backup_trouble(
                stored,
                format!("{} does not exist", stored.spec.target),
                "NoTarget",
                pass,
            )
            .await;
            return;
        };

        // The model's rule, asked here rather than re-derived: a target that is
        // the volume's own pool is a copy that dies with what it was copied
        // from, and it is refused by name.
        let view = velstra_cloud_model::backup::TargetView {
            name: target.meta.name.to_string(),
            path: target.spec.path.clone(),
            accepting: target.spec.accepting,
            writable: target.status.writable,
            same_pool_as: None,
        };
        if let Err(why) =
            velstra_cloud_model::backup::may_back_up(&stored.spec.volume, &self.config.pool, &view)
        {
            self.backup_trouble(stored, why.to_string(), "TargetUnusable", pass)
                .await;
            return;
        }

        let path = backup_path(&target.spec.path, &name);
        let written = match self.storage.copy_out(&stored.spec.volume, &path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(backup = %name, error = %e, "this copy could not be made");
                pass.failures += 1;
                self.backup_trouble(stored, e.to_string(), "CopyFailed", pass)
                    .await;
                return;
            }
        };
        pass.actions += 1;

        // The source's size as this pool has it, which is the smallest volume
        // that can be restored from the copy — a different number from what the
        // copy occupies, and both are worth knowing.
        let size_gib = self
            .storage
            .observe()
            .await
            .ok()
            .and_then(|seen| seen.volumes.get(&stored.spec.volume).copied())
            .unwrap_or(0);

        // Hash what was just written, while it is still the thing that was
        // written. This is the only moment a digest is worth anything: taken
        // now, it is a record of bytes known good, and every later read-back is
        // measured against it. Computed on a second pass it would only ever
        // certify whatever the target holds by then.
        //
        // A hash that cannot be computed is not a failed backup. The copy is
        // there and restorable; what is missing is the ability to prove it
        // later, so the copy stands and the field stays `None` — which reads
        // exactly as it should when verification comes round.
        let digest = match crate::hostfs::sha256_file(std::path::Path::new(&path)).await {
            Ok(hex) => Some(format!("sha256:{hex}")),
            Err(e) => {
                tracing::warn!(
                    backup = %name,
                    error = %e,
                    "the copy was made but could not be hashed; it cannot be verified later"
                );
                None
            }
        };

        let mut next = stored.clone();
        next.status.taken = true;
        next.status.size_gib = size_gib;
        next.status.stored_bytes = written;
        next.status.digest = digest;
        next.status.taken_at = Some(Timestamp::now());
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::True,
                "Taken",
                &format!("copied to {path}"),
                stored.meta.generation,
            ),
        );
        reporting::report(&self.backups, stored, next, &self.writer, pass).await;
    }

    /// Say on the object why a copy has not been made.
    ///
    /// On the backup rather than in a log on whichever machine happens to run
    /// this pool: "why is there no copy" is asked by somebody looking at the
    /// backup, months later, and a log line is not where they will look.
    async fn backup_trouble(
        &self,
        stored: &velstra_cloud_model::resources::Backup,
        why: String,
        reason: &str,
        pass: &mut Pass,
    ) {
        let mut next = stored.clone();
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::False,
                reason,
                &why,
                stored.meta.generation,
            ),
        );
        reporting::report(&self.backups, stored, next, &self.writer, pass).await;
    }

    /// One snapshot: claim it, do what the model says, report what is there.
    ///
    /// The pool is re-observed rather than reasoned about, exactly as for a
    /// volume — and here it matters more, because `status.taken` is the one
    /// stored value the model *consults*. Setting it from what was attempted
    /// rather than from what is there would make a failed copy permanent: a
    /// snapshot that says `taken` is never taken again, on purpose, because a
    /// copy made later is a copy of a different moment under a name somebody
    /// trusts.
    async fn snapshot_pass(&self, stored: &Snapshot, pass: &mut Pass) {
        let name = stored.meta.name.to_string();

        match stored.status.pool.as_deref() {
            Some(owner) if owner == self.config.pool => {}
            Some(_) => return,
            None => {
                let me = self.config.pool.clone();
                reporting::claim(
                    &self.snapshots,
                    stored,
                    |status| status.pool = Some(me),
                    &self.writer,
                    pass,
                )
                .await;
                return;
            }
        }

        // Read again rather than reuse the pass's picture: a volume acted on
        // earlier in this same pass may have changed what is here, and taking a
        // copy of a stale reading is how one gets made twice.
        let before = match self.storage.observe().await {
            Ok(seen) => seen.snapshot_of(&name),
            Err(e) => {
                tracing::error!(error = %e, "could not read this pool before a snapshot");
                pass.failures += 1;
                return;
            }
        };

        let mut trouble = None;
        for action in reconcile_snapshot(stored, before) {
            let done = match &action {
                SnapshotAction::Take { snapshot, volume } => {
                    self.storage.take_snapshot(snapshot, volume).await
                }
                SnapshotAction::Destroy { snapshot } => {
                    self.storage.destroy_snapshot(snapshot).await
                }
            };
            match done {
                Ok(()) => pass.actions += 1,
                Err(e) => {
                    tracing::warn!(snapshot = %name, error = %e, "this copy could not be brought into line");
                    pass.failures += 1;
                    trouble = Some(e.to_string());
                    break;
                }
            }
        }

        let after = match self.storage.observe().await {
            Ok(seen) => seen.snapshot_of(&name),
            Err(e) => {
                tracing::error!(error = %e, "could not re-read this pool after a snapshot");
                pass.failures += 1;
                before
            }
        };

        let mut next = stored.clone();
        next.status.taken = after.exists;
        next.status.size_gib = after.gib;
        next.status.observed_generation = stored.meta.generation;
        set_condition(
            &mut next.status.conditions,
            match trouble {
                Some(why) => Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "PoolFailed",
                    &why,
                    stored.meta.generation,
                ),
                None => snapshot_condition(stored, after),
            },
        );
        set_condition(
            &mut next.status.conditions,
            release_condition(
                !after.exists,
                stored.meta.is_deleting(),
                stored.meta.generation,
            ),
        );
        reporting::report(&self.snapshots, stored, next, &self.writer, pass).await;
    }

    async fn perform(&self, action: &VolumeAction, copies: &Copies) -> Result<()> {
        match action {
            VolumeAction::Provision {
                volume,
                gib,
                source,
                encryption_key,
            } => {
                // A backup is resolved here and nowhere else: it takes the
                // backup, the target it names and that target's path, and a
                // backend has none of the three. What a backend is handed is a
                // file it can open.
                let origin = match source {
                    VolumeSource::Blank => Origin::Blank,
                    VolumeSource::Image(image) => Origin::Image(image),
                    VolumeSource::Snapshot(snapshot) => Origin::Snapshot(snapshot),
                    VolumeSource::Backup(backup) => {
                        let Some(path) = copies.path_of(backup) else {
                            // Refused rather than made blank. A volume that was
                            // asked to be a restore and quietly came up empty
                            // is the worst outcome available: it boots, it is
                            // the right size, and everything that was on it is
                            // gone.
                            return Err(HostError::failed(format!(
                                "{backup} has no copy on a target this pool can read, so there is \
                                 nothing to restore from. A backup that was never taken, or whose \
                                 target is not mounted here, is not an empty volume."
                            )));
                        };
                        return self
                            .storage
                            .provision(volume, *gib, Origin::File(&path), encryption_key.as_deref())
                            .await;
                    }
                };
                self.storage
                    .provision(volume, *gib, origin, encryption_key.as_deref())
                    .await
            }
            VolumeAction::Grow { volume, to_gib } => self.storage.grow(volume, *to_gib).await,
            VolumeAction::Destroy { volume } => self.storage.destroy(volume).await,
        }
    }

    /// The pool's own object: capacity, what is used, and a heartbeat.
    ///
    /// `allocated_gib` is counted from the volumes this pool holds rather than
    /// tracked as a running total — the same reason quota is counted. A total
    /// that is incremented and decremented drifts; a count of what exists
    /// cannot.
    async fn pool_pass(&self, volumes: &[Volume], seen: &PoolState, pass: &mut Pass) {
        let name = format!("pools/{}", self.config.pool);
        let stored = match self.pools.get(&name).await {
            Ok(Some(pool)) => pool,
            // A pool nobody registered is not this agent's to invent.
            Ok(None) => return,
            Err(e) => {
                tracing::error!(error = %e, "could not read this pool's own object");
                pass.failures += 1;
                return;
            }
        };

        let allocated: u64 = volumes
            .iter()
            .filter(|v| v.spec.pool == self.config.pool && !v.meta.is_deleting())
            .map(|v| v.spec.size_gib)
            .sum();

        let mut next: Pool = stored.clone();
        next.status.observed_generation = stored.meta.generation;
        next.status.backend = seen.backend.clone();
        next.status.capacity_gib = seen.capacity_gib;
        next.status.allocated_gib = allocated;
        next.status.agent_version = self.config.agent_version.clone();
        next.status.last_heartbeat = Timestamp::now();
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::True,
                "Ready",
                "the pool agent is running and answering",
                stored.meta.generation,
            ),
        );
        reporting::report(&self.pools, &stored, next, &self.writer, pass).await;
    }

    /// Say on the pool that its backend could not be read.
    ///
    /// Deliberately does **not** touch capacity or what is allocated: those are
    /// the last numbers anybody read off a working cluster, and replacing them
    /// with zeroes would tell the scheduler this pool is full — turning "we
    /// cannot see it" into "it has no room", which is a different and much
    /// worse claim. They stay, stale, next to a condition saying they are.
    async fn unreachable(&self, why: &str, pass: &mut Pass) {
        let name = format!("pools/{}", self.config.pool);
        let Ok(Some(stored)) = self.pools.get(&name).await else {
            // A pool nobody registered is not this agent's to invent — the same
            // rule `pool_pass` holds to. There is nowhere to say this, and
            // saying it in a place of our own choosing would be worse.
            return;
        };
        let mut next: Pool = stored.clone();
        next.status.observed_generation = stored.meta.generation;
        next.status.agent_version = self.config.agent_version.clone();
        // The heartbeat still moves: the *agent* is alive and answering, and it
        // is the agent that would otherwise look dead. What is unreachable is
        // the storage behind it, and the condition is where that is said.
        next.status.last_heartbeat = Timestamp::now();
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                "Ready",
                ConditionStatus::False,
                "BackendUnreachable",
                &format!(
                    "the pool agent is running but could not read its backend, so nothing \
                     here is being provisioned: {why}"
                ),
                stored.meta.generation,
            ),
        );
        reporting::report(&self.pools, &stored, next, &self.writer, pass).await;
    }
}

/// Whether this pool still holds any of a volume that is being deleted.
///
/// The same shape the node agent uses for its own finalizer, and for the same
/// reason: an agent may not write `meta`, so it states the fact and a controller
/// acts on it.
fn release_condition(gone: bool, deleting: bool, at_generation: u64) -> Condition {
    if !deleting {
        return Condition::new(
            "Released",
            ConditionStatus::False,
            "InUse",
            "",
            at_generation,
        );
    }
    if gone {
        Condition::new(
            "Released",
            ConditionStatus::True,
            "Released",
            "the pool holds nothing of it; the finalizer may go",
            at_generation,
        )
    } else {
        Condition::new(
            "Released",
            ConditionStatus::False,
            "Destroying",
            "the backing store is still here",
            at_generation,
        )
    }
}

/// The finalizer every volume in a pool carries. Placed by the volume
/// controller; dropped by it once this agent reports `Released`.
pub const RELEASE: &str = POOL_RELEASE_FINALIZER;

// ---- the fake ------------------------------------------------------------

/// An in-process pool. Cloning shares the storage.
#[derive(Clone, Default)]
pub struct FakePool {
    inner: Arc<std::sync::Mutex<FakeInner>>,
}

#[derive(Default)]
struct FakeInner {
    volumes: BTreeMap<String, u64>,
    capacity_gib: u64,
    faults: BTreeMap<String, String>,
    /// What each volume was cloned from, so a test can prove a bootable volume
    /// was never briefly blank.
    from_image: BTreeMap<String, String>,
    /// The same, for volumes made from a snapshot.
    from_snapshot: BTreeMap<String, String>,
    /// And for volumes restored from a file on a target.
    from_file: BTreeMap<String, String>,
    /// Every copy this fake holds.
    snapshots: BTreeMap<String, SnapshotInPool>,
    encrypted: BTreeMap<String, String>,
    /// Every copy that has left this pool, by the path it was written to. A
    /// test asks this the way an operator would ask a target: is it there, and
    /// what is it a copy of.
    copied_out: BTreeMap<String, (String, u64)>,
}

impl FakePool {
    pub fn new(capacity_gib: u64) -> Self {
        let me = Self::default();
        me.inner.lock().unwrap().capacity_gib = capacity_gib;
        me
    }

    /// What a volume was restored from, if it was.
    pub fn restored_from(&self, volume: &str) -> Option<String> {
        self.inner.lock().unwrap().from_file.get(volume).cloned()
    }

    /// What was written out to `path`, if anything: the volume it came from
    /// and how large that volume was.
    pub fn copied_out(&self, path: &str) -> Option<(String, u64)> {
        self.inner.lock().unwrap().copied_out.get(path).cloned()
    }

    /// Make one operation fail. `what` is `provision`, `grow`, `destroy` or
    /// `observe`; `target` is the volume name, or empty for `observe`.
    pub fn fail(&self, what: &str, target: &str, why: &str) {
        self.inner
            .lock()
            .unwrap()
            .faults
            .insert(format!("{what}:{target}"), why.to_string());
    }

    pub fn heal(&self, what: &str, target: &str) {
        self.inner
            .lock()
            .unwrap()
            .faults
            .remove(&format!("{what}:{target}"));
    }

    pub fn has(&self, volume: &str) -> bool {
        self.inner.lock().unwrap().volumes.contains_key(volume)
    }

    pub fn size_of(&self, volume: &str) -> Option<u64> {
        self.inner.lock().unwrap().volumes.get(volume).copied()
    }

    pub fn cloned_from(&self, volume: &str) -> Option<String> {
        self.inner.lock().unwrap().from_image.get(volume).cloned()
    }

    pub fn is_encrypted(&self, volume: &str) -> bool {
        self.inner.lock().unwrap().encrypted.contains_key(volume)
    }

    /// Delete a volume behind the platform's back, the way a person with a
    /// shell does.
    pub fn vanish(&self, volume: &str) {
        self.inner.lock().unwrap().volumes.remove(volume);
    }

    /// The same for a copy — which is the more interesting case, because the
    /// model refuses to take it again.
    pub fn vanish_snapshot(&self, snapshot: &str) {
        self.inner.lock().unwrap().snapshots.remove(snapshot);
    }

    pub fn has_snapshot(&self, snapshot: &str) -> bool {
        self.inner.lock().unwrap().snapshots.contains_key(snapshot)
    }

    /// The volume a copy was taken from, so a test can prove it was taken from
    /// the right one.
    pub fn snapshot_of(&self, snapshot: &str) -> Option<SnapshotInPool> {
        self.inner.lock().unwrap().snapshots.get(snapshot).cloned()
    }

    fn fault(&self, what: &str, target: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        for key in [format!("{what}:{target}"), format!("{what}:")] {
            if let Some(why) = inner.faults.get(&key) {
                return Err(HostError::failed(why));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Storage for FakePool {
    fn at(&self, volume: &str) -> Option<String> {
        Some(format!("/fake/{}", crate::hostfs::slug(volume)))
    }

    async fn observe(&self) -> Result<PoolState> {
        self.fault("observe", "")?;
        let inner = self.inner.lock().unwrap();
        Ok(PoolState {
            volumes: inner.volumes.clone(),
            capacity_gib: inner.capacity_gib,
            backend: "fake".to_string(),
            snapshots: inner.snapshots.clone(),
        })
    }

    async fn provision(
        &self,
        volume: &str,
        gib: u64,
        source: Origin<'_>,
        encryption_key: Option<&str>,
    ) -> Result<()> {
        self.fault("provision", volume)?;
        let mut inner = self.inner.lock().unwrap();
        // A volume made from a snapshot that this pool does not hold is not an
        // empty volume, it is a mistake — and a fake that quietly made one
        // anyway could not be used to prove the platform refuses it.
        if let Origin::Snapshot(snapshot) = source
            && !inner.snapshots.contains_key(snapshot)
        {
            return Err(HostError::failed(format!(
                "{snapshot} is not in this pool, so nothing can be copied from it"
            )));
        }
        // The same rule for a restore: a file that is not there is not an empty
        // volume. This is the fake's stand-in for the copy existing on the
        // target — nothing was written there, so what it knows is what it was
        // asked to write.
        if let Origin::File(path) = source
            && !inner.copied_out.contains_key(path)
        {
            return Err(HostError::failed(format!(
                "{path} is not on this machine, so nothing can be restored from it"
            )));
        }
        inner.volumes.insert(volume.to_string(), gib);
        match source {
            Origin::Image(image) => {
                inner
                    .from_image
                    .insert(volume.to_string(), image.to_string());
            }
            Origin::Snapshot(snapshot) => {
                inner
                    .from_snapshot
                    .insert(volume.to_string(), snapshot.to_string());
            }
            Origin::File(path) => {
                inner.from_file.insert(volume.to_string(), path.to_string());
            }
            Origin::Blank => {}
        }
        if let Some(key) = encryption_key {
            inner.encrypted.insert(volume.to_string(), key.to_string());
        }
        Ok(())
    }

    async fn grow(&self, volume: &str, to_gib: u64) -> Result<()> {
        self.fault("grow", volume)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(size) = inner.volumes.get_mut(volume) {
            // Even the fake refuses to shrink: a fake that would do the thing
            // the platform forbids cannot be used to prove the platform does not
            // ask for it.
            if to_gib >= *size {
                *size = to_gib;
            }
        }
        Ok(())
    }

    async fn destroy(&self, volume: &str) -> Result<()> {
        self.fault("destroy", volume)?;
        self.inner.lock().unwrap().volumes.remove(volume);
        Ok(())
    }

    async fn take_snapshot(&self, snapshot: &str, volume: &str) -> Result<()> {
        self.fault("take_snapshot", snapshot)?;
        let mut inner = self.inner.lock().unwrap();
        // A copy of nothing is not an empty copy, it is a mistake — and a fake
        // that made one anyway could not be used to prove the platform refuses
        // it. Same shape as provisioning from a snapshot that is not here.
        let Some(gib) = inner.volumes.get(volume).copied() else {
            return Err(HostError::failed(format!(
                "{volume} is not in this pool, so there is nothing to copy"
            )));
        };
        inner.snapshots.insert(
            snapshot.to_string(),
            SnapshotInPool {
                volume: volume.to_string(),
                gib,
            },
        );
        Ok(())
    }

    async fn destroy_snapshot(&self, snapshot: &str) -> Result<()> {
        self.fault("destroy_snapshot", snapshot)?;
        self.inner.lock().unwrap().snapshots.remove(snapshot);
        Ok(())
    }

    async fn copy_out(&self, volume: &str, path: &str) -> Result<u64> {
        self.fault("copy_out", volume)?;
        let mut inner = self.inner.lock().unwrap();
        // A copy of nothing is a mistake, not an empty copy — the same rule
        // `take_snapshot` holds to, and for the same reason: a fake that made
        // one anyway could not be used to prove the platform refuses it.
        let Some(gib) = inner.volumes.get(volume).copied() else {
            return Err(HostError::failed(format!(
                "{volume} is not in this pool, so there is nothing to copy out"
            )));
        };
        inner
            .copied_out
            .insert(path.to_string(), (volume.to_string(), gib));
        // Real bytes, small and deterministic.
        //
        // This used to write nothing and return a plausible number, which made
        // every reader of a copy untestable — including the one that reads a
        // copy back to check it, which is the entire point of having made one.
        // A fake that only records that a copy "happened" can prove the copy
        // was requested and nothing about the copy.
        //
        // Deterministic in the volume and its size so that a test can corrupt a
        // copy and know the difference is the corruption.
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                HostError::failed(format!("{} could not be made: {e}", parent.display()))
            })?;
        }
        let body = format!("velstra fake copy of {volume} at {gib} GiB\n");
        std::fs::write(path, &body)
            .map_err(|e| HostError::failed(format!("{path} could not be written: {e}")))?;
        // What the caller is told is what a real backend reports: the size of
        // the source, not of whatever this fake happened to put on disk. A
        // target's free space is computed from the former.
        Ok(gib * 1024 * 1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, ResourceName},
        resources::Resource,
    };
    use velstra_cloud_store::MemoryStore;

    use super::*;

    const VOLUME: &str = "projects/p1/volumes/data-1";

    struct Cell {
        volumes: TypedStore<VolumeSpec, VolumeStatus>,
        snapshots: TypedStore<SnapshotSpec, SnapshotStatus>,
        pools: TypedStore<PoolSpec, PoolStatus>,
        backups: TypedStore<
            velstra_cloud_model::backup::BackupSpec,
            velstra_cloud_model::backup::BackupStatus,
        >,
        targets: TypedStore<
            velstra_cloud_model::backup::BackupTargetSpec,
            velstra_cloud_model::backup::BackupTargetStatus,
        >,
        fake: FakePool,
        /// Unique per cell, so two tests running side by side do not share a
        /// backup target directory.
        ///
        /// It used to be keyed on the process id alone, which was fine only
        /// because the fake pool wrote no bytes: every test used the target id
        /// "archive" and the backup id "b1", so they all pointed at one path
        /// that never had anything in it. The moment `copy_out` became honest,
        /// tests that passed alone started failing together — and would have
        /// gone on doing it intermittently, which is the worst way to find out.
        tag: u64,
    }

    fn cell(pool: &str) -> (Cell, PoolAgent) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let fake = FakePool::new(1000);
        let cell = Cell {
            tag: NEXT.fetch_add(1, Ordering::Relaxed),
            volumes: TypedStore::new(store.clone(), "cell-1", "volumes"),
            snapshots: TypedStore::new(store.clone(), "cell-1", "snapshots"),
            pools: TypedStore::new(store.clone(), "cell-1", "pools"),
            backups: TypedStore::new(store.clone(), "cell-1", "backups"),
            targets: TypedStore::new(store.clone(), "cell-1", "backup-targets"),
            fake: fake.clone(),
        };
        let agent = PoolAgent::new(store, PoolConfig::new(pool, "eu", "cell-1"), Arc::new(fake));
        (cell, agent)
    }

    impl Cell {
        async fn volume(&self, pool: &str, gib: u64) -> Volume {
            let mut v: Volume = Resource::new(
                Meta::new(
                    ResourceName::parse(VOLUME).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                VolumeSpec {
                    source_backup: None,
                    size_gib: gib,
                    pool: pool.to_string(),
                    encryption_key: None,
                    source_image: None,
                    source_snapshot: None,
                },
                VolumeStatus::default(),
            );
            v.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
            self.volumes
                .create(&v, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
            self.reload().await
        }

        /// A place copies are kept, as an operator declares one.
        ///
        /// A real directory when it is meant to be usable, because the agent
        /// now *looks*: it writes a probe file and removes it, which is the
        /// only way to tell a writable directory from an empty mountpoint on
        /// the root disk. A fixture that only set `writable: true` in the
        /// status would be testing a field the agent overwrites.
        async fn target(&self, id: &str, accepting: bool, writable: bool) {
            let t: velstra_cloud_model::resources::BackupTarget = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("backup-targets/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                velstra_cloud_model::backup::BackupTargetSpec {
                    kind: velstra_cloud_model::backup::TargetKind::Directory,
                    path: self.target_dir(id, writable),
                    accepting,
                    // Named by the operator, as the model requires: a target
                    // assigned to nobody is one no agent may report on.
                    agent: "nvme".into(),
                    verify_every_hours: 0,
                },
                velstra_cloud_model::backup::BackupTargetStatus::default(),
            );
            self.targets
                .create(&t, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
        }

        /// A copy somebody asked for, as the API stores it.
        async fn backup(
            &self,
            id: &str,
            pool: &str,
            target: &str,
        ) -> velstra_cloud_model::resources::Backup {
            let b: velstra_cloud_model::resources::Backup = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("projects/p1/backups/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                velstra_cloud_model::backup::BackupSpec {
                    volume: VOLUME.to_string(),
                    target: format!("backup-targets/{target}"),
                    pool: pool.to_string(),
                    schedule: None,
                },
                Default::default(),
            );
            self.backups
                .create(&b, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
            self.reload_backup(id).await
        }

        /// Where a target points. Real and made when it is meant to be
        /// usable; a path that is not there when it is not.
        fn target_dir(&self, id: &str, usable: bool) -> String {
            let dir = std::env::temp_dir().join(format!(
                "velstra-target-{}-{}-{id}",
                std::process::id(),
                self.tag
            ));
            if usable {
                std::fs::create_dir_all(&dir).unwrap();
            } else {
                let _ = std::fs::remove_dir_all(&dir);
            }
            dir.to_string_lossy().to_string()
        }

        async fn reload_target(&self, id: &str) -> velstra_cloud_model::resources::BackupTarget {
            self.targets
                .get(&format!("backup-targets/{id}"))
                .await
                .unwrap()
                .unwrap()
        }

        async fn reload_backup(&self, id: &str) -> velstra_cloud_model::resources::Backup {
            self.backups
                .get(&format!("projects/p1/backups/{id}"))
                .await
                .unwrap()
                .unwrap()
        }

        /// A writable, accepting target that reads its copies back.
        async fn target_verifying(&self, id: &str, hours: u32) {
            self.target(id, true, true).await;
            self.set_verify(id, hours).await;
        }

        /// Change how often this target verifies, as an operator would.
        async fn set_verify(&self, id: &str, hours: u32) {
            let name = format!("backup-targets/{id}");
            let mut t = self.targets.get(&name).await.unwrap().unwrap();
            // Setting it to what it already is is not an edit, and the store
            // says so (`GenerationWithoutChange`). Worth keeping rather than
            // working around: a generation that moved without a spec changing
            // is how a controller convinces itself it has work to do.
            if t.spec.verify_every_hours == hours {
                return;
            }
            t.spec.verify_every_hours = hours;
            t.meta.generation += 1;
            self.targets
                .update(&t, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
        }

        /// Age a finished copy, so its proof is stale enough to be due.
        ///
        /// The clock is moved rather than the test waiting: verification is
        /// deliberately not something that happens the moment a copy lands, so
        /// there is no way to observe it without time having passed.
        async fn backdate_backup(&self, id: &str, by_ms: u64) {
            let mut b = self.reload_backup(id).await;
            if let Some(at) = b.status.taken_at {
                b.status.taken_at = Some(Timestamp(at.0.saturating_sub(by_ms)));
            }
            if let Some(at) = b.status.verified_at {
                b.status.verified_at = Some(Timestamp(at.0.saturating_sub(by_ms)));
            }
            self.backups
                .update(&b, &velstra_cloud_model::access::Writer::agent("nvme"))
                .await
                .unwrap();
        }

        /// A copy as it would look if it had been made before digests existed.
        async fn forget_digest(&self, id: &str) {
            let mut b = self.reload_backup(id).await;
            b.status.digest = None;
            self.backups
                .update(&b, &velstra_cloud_model::access::Writer::agent("nvme"))
                .await
                .unwrap();
        }

        async fn register_pool(&self, id: &str) {
            let p: Pool = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("pools/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                PoolSpec {
                    accepting: true,
                    labels: vec![],
                },
                PoolStatus::default(),
            );
            self.pools
                .create(&p, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
        }

        async fn reload_pool(&self, id: &str) -> Pool {
            self.pools
                .get(&format!("pools/{id}"))
                .await
                .unwrap()
                .unwrap()
        }

        async fn reload(&self) -> Volume {
            self.volumes.get(VOLUME).await.unwrap().unwrap()
        }

        async fn reload_named(&self, id: &str) -> Volume {
            self.volumes
                .get(&format!("projects/p1/volumes/{id}"))
                .await
                .unwrap()
                .unwrap()
        }

        /// A volume asked to be a restore of a copy.
        async fn restored(&self, id: &str, pool: &str, backup: &str) -> Volume {
            let mut v: Volume = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("projects/p1/volumes/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                VolumeSpec {
                    source_backup: Some(backup.to_string()),
                    size_gib: 40,
                    pool: pool.to_string(),
                    encryption_key: None,
                    source_image: None,
                    source_snapshot: None,
                },
                VolumeStatus::default(),
            );
            v.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
            self.volumes
                .create(&v, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
            self.reload_named(id).await
        }

        async fn snapshot(&self, id: &str, pool: &str) -> Snapshot {
            // Under the volume, because a snapshot's source is in its identity
            // rather than in a field — `source_volume` reads the parent.
            let mut s: Snapshot = Resource::new(
                Meta::new(
                    ResourceName::parse(&format!("{VOLUME}/snapshots/{id}")).unwrap(),
                    Placement::new("eu", "cell-1"),
                ),
                SnapshotSpec {
                    pool: pool.to_string(),
                },
                SnapshotStatus::default(),
            );
            s.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
            self.snapshots
                .create(&s, &velstra_cloud_model::access::Writer::controller("pool"))
                .await
                .unwrap();
            self.reload_snapshot(id).await
        }

        async fn reload_snapshot(&self, id: &str) -> Snapshot {
            self.snapshots
                .get(&format!("{VOLUME}/snapshots/{id}"))
                .await
                .unwrap()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn a_volume_that_nothing_provisioned_is_provisioned_and_reported() {
        // The gap this whole file closes: before it, `data-1` sat at generation
        // 1 with observed 0 and no conditions, because the access rule refused
        // every writer there could be.
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;

        // First pass claims — and touches nothing on the backend, because a
        // volume whose ownership is unsettled must not be created twice.
        agent.resync().await;
        assert_eq!(cell.reload().await.status.pool.as_deref(), Some("pool-a"));
        assert!(
            !cell.fake.has(VOLUME),
            "the pool acted before it owned the object"
        );

        agent.resync().await;
        assert!(cell.fake.has(VOLUME));
        let v = cell.reload().await;
        assert!(v.status.provisioned);
        assert_eq!(v.status.actual_size_gib, 100);
        assert!(
            v.converged(),
            "the volume never caught up with its own spec"
        );

        // Settled: nothing happens to the volume on a further pass.
        //
        // Asserted on the volume rather than on the pass's write count, because
        // the pool's own object carries a heartbeat and that is *supposed* to be
        // written every time. Counting all writes made this test pass only when
        // two resyncs happened to land in the same millisecond — green on an
        // idle machine, red under load, and about the clock either way.
        let before = cell.reload().await;
        let quiet = agent.resync().await;
        let after = cell.reload().await;
        assert_eq!(quiet.actions, 0, "a settled volume was acted on again");
        assert_eq!(
            before.meta.revision, after.meta.revision,
            "a settled volume was written to again"
        );
    }

    #[tokio::test]
    async fn a_volume_belonging_to_another_pool_is_not_touched() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-b", 100).await;

        let pass = agent.resync().await;
        assert_eq!(pass.actions, 0);
        assert!(!cell.fake.has(VOLUME));
        assert_eq!(cell.reload().await.status.pool, None);
    }

    #[tokio::test]
    async fn a_volume_is_grown_and_never_shrunk() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        assert_eq!(cell.fake.size_of(VOLUME), Some(100));

        let mut bigger = cell.reload().await;
        bigger.spec.size_gib = 200;
        bigger.meta.generation += 1;
        cell.volumes
            .update(&bigger, &Writer::controller("test"))
            .await
            .unwrap();
        agent.resync().await;
        assert_eq!(cell.fake.size_of(VOLUME), Some(200));

        // Now ask for less. Nothing happens to the bytes, and the object says
        // why rather than sitting there looking converged.
        let mut smaller = cell.reload().await;
        smaller.spec.size_gib = 50;
        smaller.meta.generation += 1;
        cell.volumes
            .update(&smaller, &Writer::controller("test"))
            .await
            .unwrap();
        agent.resync().await;

        assert_eq!(cell.fake.size_of(VOLUME), Some(200), "data was destroyed");
        let v = cell.reload().await;
        let ready = v
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .expect("it says something about itself");
        assert_eq!(ready.reason, "WillNotShrink");
        assert!(ready.message.contains("copy"), "{}", ready.message);
    }

    #[tokio::test]
    async fn the_bytes_go_before_the_object_does() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        assert!(cell.fake.has(VOLUME));

        let mut going = cell.reload().await;
        going.meta.deleted_at = Some(Timestamp::now());
        cell.volumes
            .update(&going, &Writer::controller("test"))
            .await
            .unwrap();

        agent.resync().await;
        assert!(!cell.fake.has(VOLUME), "the storage outlived the delete");

        // The pool does not drop the finalizer itself — it may not write `meta`
        // — so what it owes is the fact a controller acts on, and it owes it
        // only once the bytes are observably gone.
        let mid = cell.reload().await;
        let said = mid
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Released")
            .expect("it says whether it has let go");
        assert_eq!(said.status, ConditionStatus::True, "{}", said.message);
    }

    #[tokio::test]
    async fn a_volume_whose_bytes_would_not_go_is_not_reported_as_released() {
        // The failure this prevents: the object disappears, the finalizer with
        // it, and the pool is left holding gigabytes nobody is billed for and
        // nobody can find.
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;

        cell.fake.fail("destroy", VOLUME, "the array is busy");
        let mut going = cell.reload().await;
        going.meta.deleted_at = Some(Timestamp::now());
        cell.volumes
            .update(&going, &Writer::controller("test"))
            .await
            .unwrap();

        agent.resync().await;
        assert!(cell.fake.has(VOLUME));
        let still = cell
            .reload()
            .await
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Released")
            .cloned()
            .expect("it says whether it has let go");
        assert_eq!(still.status, ConditionStatus::False);
        assert_eq!(still.reason, "Destroying");
    }

    #[tokio::test]
    async fn a_backend_that_fails_says_so_on_the_volume_and_keeps_trying() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        cell.fake
            .fail("provision", VOLUME, "the volume group is full");

        agent.resync().await;
        let pass = agent.resync().await;
        assert_eq!(pass.failures, 1);
        let v = cell.reload().await;
        assert!(!v.status.provisioned);
        let ready = v
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .unwrap();
        assert_eq!(ready.reason, "PoolFailed");
        assert!(
            ready.message.contains("volume group is full"),
            "the backend's own words are not on the object: {}",
            ready.message
        );

        // Nothing is stuck: the moment the backend works, the next pass closes
        // the gap without anybody clearing a state.
        cell.fake.heal("provision", VOLUME);
        agent.resync().await;
        assert!(cell.fake.has(VOLUME));
        assert!(cell.reload().await.status.provisioned);
    }

    #[tokio::test]
    async fn a_volume_deleted_behind_the_platforms_back_is_made_again() {
        // The reason `observe()` is re-read every pass instead of trusting
        // `status.provisioned`: believing the object here leaves a volume that
        // reads "ready" while every guest that opens it fails.
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;

        cell.fake.vanish(VOLUME);
        agent.resync().await;
        assert!(
            cell.fake.has(VOLUME),
            "the platform believed its own record"
        );
    }

    #[tokio::test]
    async fn the_pool_reports_what_it_holds_rather_than_counting_up() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;

        let pool = cell.pools.get("pools/pool-a").await.unwrap().unwrap();
        assert_eq!(pool.status.backend, "fake");
        assert_eq!(pool.status.capacity_gib, 1000);
        assert_eq!(pool.status.allocated_gib, 100);
        assert!(
            pool.status
                .conditions
                .iter()
                .any(|c| c.kind == "Ready" && c.status == ConditionStatus::True)
        );
    }

    #[tokio::test]
    async fn a_pool_that_cannot_be_read_does_nothing_rather_than_guessing() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        cell.fake.fail("observe", "", "the array is not answering");

        let pass = agent.resync().await;
        assert_eq!(pass.failures, 1);
        assert_eq!(pass.actions, 0, "it acted on a picture it could not read");

        // Zero *actions* is the invariant; zero writes was the old one, and it
        // was wrong.
        //
        // The argument for silence was that "a pool reporting a heartbeat it
        // could not verify would look alive". True of a bare heartbeat — and it
        // assumed those were the only two options. They are not: the agent
        // reports the heartbeat *and* a false `Ready` naming the backend, which
        // says the one thing neither alternative could. Silence is
        // indistinguishable from a dead agent, and those are different problems
        // with different fixes; a bare heartbeat is a lie. This is neither.
        //
        // Safe to move the heartbeat because nothing consults a *pool's*:
        // fencing and recovery read a node's (`ha.rs`), and the only reader here
        // is a person, for whom "heard two seconds ago, and it says its backend
        // is unreachable" beats "not heard from in ten minutes".
        assert_eq!(
            pass.reports, 1,
            "an unreadable backend said nothing anywhere"
        );
        let pool = cell.reload_pool("pool-a").await;
        let ready = ready_condition(&pool.status.conditions);
        assert_eq!(ready.status, ConditionStatus::False, "{ready:?}");
        assert_eq!(ready.reason, "BackendUnreachable", "{ready:?}");
        assert!(
            ready.message.contains("the array is not answering"),
            "{ready:?}"
        );
        // And the volume was not touched: one backend being down is one fact,
        // written once, not onto every object that depends on it.
        let v = cell.reload().await;
        assert!(!v.status.provisioned);
    }
    // ---- copies ----------------------------------------------------------
    //
    // `reconcile_snapshot`, `snapshot_condition` and `source_volume` were
    // written, reasoned about at length and fully tested in the model — and had
    // no caller anywhere. A Snapshot could be created through the API and
    // nothing on any machine would ever make one. These are the tests for the
    // half that was missing.

    #[tokio::test]
    async fn a_snapshot_is_claimed_before_it_is_taken() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await; // claim the volume
        agent.resync().await; // provision it
        cell.snapshot("s1", "pool-a").await;

        // Claiming touches nothing: a copy taken before ownership is settled is
        // a copy two pools could each make under one name.
        agent.resync().await;
        assert_eq!(
            cell.reload_snapshot("s1").await.status.pool.as_deref(),
            Some("pool-a")
        );
        assert!(
            !cell.fake.has_snapshot(&format!("{VOLUME}/snapshots/s1")),
            "the pool copied before it owned the object"
        );

        agent.resync().await;
        let after = cell.reload_snapshot("s1").await;
        assert!(after.status.taken, "the copy was never made");
        assert_eq!(
            after.status.size_gib, 100,
            "the copy does not know its size"
        );
        assert_eq!(
            cell.fake
                .snapshot_of(&format!("{VOLUME}/snapshots/s1"))
                .map(|c| c.volume),
            Some(VOLUME.to_string()),
            "the copy was taken from the wrong volume"
        );
    }

    #[tokio::test]
    async fn a_copy_that_vanished_is_not_quietly_made_again() {
        // The one property the model insists on and the reason `status.taken`
        // is the only stored value it consults: a copy made now is a copy of a
        // *different moment*, and it would wear the name of the one somebody is
        // about to restore from. Losing the copy has to be said out loud.
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        cell.snapshot("s1", "pool-a").await;
        agent.resync().await;
        agent.resync().await;
        assert!(cell.reload_snapshot("s1").await.status.taken);

        // Somebody with a shell removes it.
        cell.fake.vanish_snapshot(&format!("{VOLUME}/snapshots/s1"));
        agent.resync().await;

        assert!(
            !cell.fake.has_snapshot(&format!("{VOLUME}/snapshots/s1")),
            "a lost copy was silently remade, of a different moment, under the same name"
        );
        let after = cell.reload_snapshot("s1").await;
        assert!(!after.status.taken);
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .expect("nothing said the copy was gone");
        assert_eq!(ready.reason, "Vanished", "{ready:?}");
    }

    #[tokio::test]
    async fn a_volume_is_not_destroyed_under_the_copies_taken_from_it() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        cell.snapshot("s1", "pool-a").await;
        agent.resync().await;
        agent.resync().await;

        // Delete the volume while the copy is still there.
        let mut v = cell.reload().await;
        v.meta.deleted_at = Some(Timestamp::now());
        cell.volumes
            .update(&v, &Writer::controller("test"))
            .await
            .unwrap();
        agent.resync().await;

        assert!(
            cell.fake.has(VOLUME),
            "the volume was destroyed while a copy is read through it"
        );
        let ready = cell
            .reload()
            .await
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .cloned()
            .expect("nothing said why the volume is still here");
        assert!(
            ready.message.contains("snapshots"),
            "the refusal does not say what is holding it: {ready:?}"
        );
    }

    #[tokio::test]
    async fn deleting_a_copy_destroys_it_and_says_the_finalizer_may_go() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        cell.snapshot("s1", "pool-a").await;
        agent.resync().await;
        agent.resync().await;

        let mut s = cell.reload_snapshot("s1").await;
        s.meta.deleted_at = Some(Timestamp::now());
        cell.snapshots
            .update(&s, &Writer::controller("test"))
            .await
            .unwrap();
        agent.resync().await;

        assert!(!cell.fake.has_snapshot(&format!("{VOLUME}/snapshots/s1")));
        let after = cell.reload_snapshot("s1").await;
        assert!(!after.status.taken);
        let released = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Released")
            .expect("nothing said whether the bytes are gone");
        assert_eq!(
            released.status,
            ConditionStatus::True,
            "the controller is never told it may drop the finalizer: {released:?}"
        );
    }

    #[tokio::test]
    async fn a_copy_that_could_not_be_made_does_not_report_itself_made() {
        // If a failed take set `taken`, the model would refuse to try again for
        // ever — a snapshot that can never exist and can never be retried.
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        cell.snapshot("s1", "pool-a").await;
        agent.resync().await; // claim
        cell.fake.fail(
            "take_snapshot",
            &format!("{VOLUME}/snapshots/s1"),
            "the disks said no",
        );
        agent.resync().await;

        let after = cell.reload_snapshot("s1").await;
        assert!(
            !after.status.taken,
            "a copy that was never made says it was"
        );
        let ready = after
            .status
            .conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .expect("nothing said the copy failed");
        assert!(ready.message.contains("the disks said no"), "{ready:?}");

        // And it is retried once the pool is well again, which is the whole
        // point of not having written `taken`.
        cell.fake
            .heal("take_snapshot", &format!("{VOLUME}/snapshots/s1"));
        agent.resync().await;
        assert!(cell.reload_snapshot("s1").await.status.taken);
    }

    #[tokio::test]
    async fn a_copy_in_another_pool_is_left_alone() {
        let (cell, agent) = cell("pool-a");
        cell.register_pool("pool-a").await;
        cell.volume("pool-a", 100).await;
        agent.resync().await;
        agent.resync().await;
        cell.snapshot("s1", "pool-b").await;
        agent.resync().await;
        agent.resync().await;

        assert!(!cell.fake.has_snapshot(&format!("{VOLUME}/snapshots/s1")));
        assert_eq!(cell.reload_snapshot("s1").await.status.pool, None);
    }

    /// A backup is bytes leaving the pool, and until this the platform had the
    /// object, the schedule, the retention and the console — and nothing that
    /// ever wrote one.
    #[tokio::test]
    async fn a_backup_is_claimed_copied_out_and_reported() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        agent.resync().await;

        cell.backup("b1", "nvme", "archive").await;

        // First pass claims it. A pool that copied before claiming would have
        // two pools copying one volume the first time somebody added a second.
        agent.resync().await;
        let claimed = cell.reload_backup("b1").await;
        assert_eq!(claimed.status.agent.as_deref(), Some("nvme"));
        assert!(!claimed.status.taken, "it was copied before it was claimed");

        agent.resync().await;
        let taken = cell.reload_backup("b1").await;
        assert!(taken.status.taken, "the copy was never made");
        assert_eq!(
            taken.status.size_gib, 40,
            "the smallest volume this can be restored into is not recorded"
        );
        assert!(taken.status.stored_bytes > 0, "the copy occupies nothing");
        assert!(taken.status.taken_at.is_some());

        // And the bytes really left the pool, under a name a person can read
        // off the target with `ls`.
        let path = backup_path(&cell.target_dir("archive", true), "projects/p1/backups/b1");
        assert!(path.ends_with("/projects~p1~backups~b1"), "{path}");
        let (from, gib) = cell
            .fake
            .copied_out(&path)
            .expect("nothing was written to the target");
        assert_eq!(from, VOLUME);
        assert_eq!(gib, 40);
    }

    /// A copy that exists is never made again. The whole reason `taken` is
    /// consulted and not merely reported: a second copy would be of a different
    /// moment under a name somebody trusts.
    #[tokio::test]
    async fn a_backup_that_has_been_made_is_not_made_again() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        cell.backup("b1", "nvme", "archive").await;
        for _ in 0..3 {
            agent.resync().await;
        }
        assert!(cell.reload_backup("b1").await.status.taken);

        let before = cell.reload_backup("b1").await.meta.revision;
        let pass = agent.resync().await;
        assert_eq!(
            cell.reload_backup("b1").await.meta.revision,
            before,
            "a settled backup was written again"
        );
        assert_eq!(pass.actions, 0, "a copy that exists was made a second time");
    }

    /// A target whose mount has gone reports itself unwritable, and the copies
    /// pointed at it stop rather than being written to the empty mountpoint on
    /// the root disk.
    ///
    /// Answered by writing a probe file and removing it, because that is the
    /// only way to tell the two apart: an empty mountpoint has the same mode
    /// bits as the directory that was mounted over it.
    #[tokio::test]
    async fn a_target_that_is_not_there_says_so_and_takes_nothing() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        // Declared, and the path is not a directory on this machine.
        cell.target("gone", true, false).await;
        cell.backup("b1", "nvme", "gone").await;
        for _ in 0..3 {
            agent.resync().await;
        }

        let target = cell.reload_target("gone").await;
        assert_eq!(target.status.agent.as_deref(), Some("nvme"));
        assert_eq!(
            target.status.writable,
            Some(false),
            "a missing directory did not report itself unwritable"
        );
        let ready = velstra_cloud_model::meta::condition(&target.status.conditions, "Ready")
            .expect("a target that cannot be written says so");
        assert!(
            ready.message.contains("mounted"),
            "the reason does not point at the likely cause: {}",
            ready.message
        );

        let backup = cell.reload_backup("b1").await;
        assert!(
            !backup.status.taken,
            "a copy was reported onto a target that is not there"
        );
    }

    /// A target that is not accepting is refused, and the reason is on the
    /// backup rather than in a log on whichever machine happens to run this
    /// pool — "why is there no copy" is asked months later by somebody looking
    /// at the backup.
    #[tokio::test]
    async fn a_target_that_will_not_take_it_says_so_on_the_object() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", false, true).await;
        cell.backup("b1", "nvme", "archive").await;
        for _ in 0..3 {
            agent.resync().await;
        }

        let refused = cell.reload_backup("b1").await;
        assert!(!refused.status.taken);
        let ready = velstra_cloud_model::meta::condition(&refused.status.conditions, "Ready")
            .expect("a backup that was not made says why");
        assert_eq!(ready.status, ConditionStatus::False);
        assert!(
            ready.message.contains("not accepting"),
            "the reason is not the model's own words: {}",
            ready.message
        );
        assert!(
            cell.fake
                .copied_out(&backup_path(
                    &cell.target_dir("archive", true),
                    "projects/p1/backups/b1"
                ))
                .is_none(),
            "bytes were written to a target that is not accepting"
        );
    }

    /// A copy that could not be written is not reported as one. The failure
    /// mode this guards is the worst one a backup has: a file that is believed.
    #[tokio::test]
    async fn a_copy_that_failed_is_never_reported_as_taken() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        cell.backup("b1", "nvme", "archive").await;
        cell.fake.fail("copy_out", VOLUME, "the target filled up");

        for _ in 0..3 {
            agent.resync().await;
        }
        let after = cell.reload_backup("b1").await;
        assert!(
            !after.status.taken,
            "a copy that failed was reported as made"
        );
        let ready = velstra_cloud_model::meta::condition(&after.status.conditions, "Ready")
            .expect("a backup that failed says why");
        assert!(ready.message.contains("filled up"), "{}", ready.message);
    }

    /// The other half, and the one that matters when somebody is having a bad
    /// day: a volume made *from* a backup.
    #[tokio::test]
    async fn a_volume_is_restored_from_a_backup_that_was_taken() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        cell.backup("b1", "nvme", "archive").await;
        for _ in 0..3 {
            agent.resync().await;
        }
        assert!(cell.reload_backup("b1").await.status.taken);

        cell.restored("restored", "nvme", "projects/p1/backups/b1")
            .await;
        for _ in 0..3 {
            agent.resync().await;
        }

        let volume = cell.reload_named("restored").await;
        assert!(volume.status.provisioned, "the restore never happened");
        assert_eq!(
            cell.fake.restored_from("projects/p1/volumes/restored"),
            Some(backup_path(
                &cell.target_dir("archive", true),
                "projects/p1/backups/b1"
            )),
            "the volume was made from something other than the copy it named"
        );
    }

    /// A restore that cannot find its copy is **refused**, never made blank.
    ///
    /// The worst outcome available: a volume that boots, is the right size, and
    /// has nothing on it — under a name somebody expects their data behind.
    #[tokio::test]
    async fn a_restore_with_no_copy_to_read_is_refused_rather_than_made_empty() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        // Asked for, never taken — the schedule has not run, or the copy
        // failed.
        cell.backup("b1", "nvme", "archive").await;
        cell.restored("restored", "nvme", "projects/p1/backups/b1")
            .await;

        for _ in 0..2 {
            agent.resync().await;
        }
        let volume = cell.reload_named("restored").await;
        assert!(
            !volume.status.provisioned,
            "a volume was made blank in place of a restore"
        );
        let ready = velstra_cloud_model::meta::condition(&volume.status.conditions, "Ready")
            .expect("a volume that could not be restored says why");
        assert_eq!(ready.status, ConditionStatus::False);
        assert!(
            ready.message.contains("not an empty volume"),
            "the reason does not say what was avoided: {}",
            ready.message
        );
    }

    /// Another pool's copies are not this pool's business.
    #[tokio::test]
    async fn a_backup_of_another_pool_is_left_alone() {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target("archive", true, true).await;
        cell.backup("b1", "spinning-rust", "archive").await;
        for _ in 0..3 {
            agent.resync().await;
        }
        let untouched = cell.reload_backup("b1").await;
        assert_eq!(untouched.status.agent, None);
        assert!(!untouched.status.taken);
    }

    /// The `Ready` condition an object is carrying.
    fn ready_condition(conditions: &[Condition]) -> &Condition {
        conditions
            .iter()
            .find(|c| c.kind == "Ready")
            .expect("it says nothing about itself")
    }

    /// Turn a target's verification on and hand back the path of a finished
    /// copy, so a test can go and do something to it.
    async fn a_verified_copy(hours: u32) -> (Cell, PoolAgent, String) {
        let (cell, agent) = cell("nvme");
        cell.register_pool("nvme").await;
        cell.volume("nvme", 40).await;
        cell.target_verifying("archive", hours).await;
        cell.backup("b1", "nvme", "archive").await;
        // Claim, then copy.
        agent.resync().await;
        agent.resync().await;
        let path = backup_path(&cell.target_dir("archive", true), "projects/p1/backups/b1");
        (cell, agent, path)
    }

    /// The digest is taken when the bytes are written, because that is the only
    /// moment it means anything.
    #[tokio::test]
    async fn a_copy_records_what_it_hashed_to_when_it_was_written() {
        let (cell, _agent, path) = a_verified_copy(1).await;
        let taken = cell.reload_backup("b1").await;
        let digest = taken.status.digest.expect("the copy carries no digest");
        assert!(digest.starts_with("sha256:"), "{digest}");
        // Which algorithm it was is part of the answer, not folklore.
        let want = crate::hostfs::sha256_file(std::path::Path::new(&path))
            .await
            .unwrap();
        assert_eq!(digest, format!("sha256:{want}"));
    }

    /// The pass that makes "a backup exists" into "somebody has read it".
    #[tokio::test]
    async fn a_copy_is_read_back_and_says_when_it_last_matched() {
        let (cell, agent, _path) = a_verified_copy(0).await;
        // Not asked to verify: nothing is read back, however long it sits.
        agent.resync().await;
        assert!(
            cell.reload_backup("b1").await.status.verified_at.is_none(),
            "a target nobody asked to verify read a copy back anyway"
        );

        // Asked to verify, and the copy is older than the interval.
        cell.set_verify("archive", 1).await;
        cell.backdate_backup("b1", 4 * 3_600_000).await;
        agent.resync().await;

        let checked = cell.reload_backup("b1").await;
        assert!(
            checked.status.verified_at.is_some(),
            "the copy was never read back"
        );
        assert_eq!(checked.status.verify_error, None);
        let ready = ready_condition(&checked.status.conditions);
        assert_eq!(ready.reason, "Verified", "{ready:?}");
    }

    /// The failure this whole feature exists to find: bytes that are no longer
    /// the bytes that were written. Nothing else in the platform would notice
    /// until somebody tried to restore from it.
    #[tokio::test]
    async fn a_copy_that_has_rotted_is_reported_and_never_deleted() {
        let (cell, agent, path) = a_verified_copy(1).await;
        cell.backdate_backup("b1", 4 * 3_600_000).await;

        // One flipped byte is enough, and is what rot looks like.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        agent.resync().await;

        let bad = cell.reload_backup("b1").await;
        let why = bad
            .status
            .verify_error
            .expect("the corruption went unnoticed");
        assert!(why.contains("no longer matches"), "{why}");
        // It says what a restore from it would actually mean, because that is
        // the decision the reader has to make.
        assert!(
            why.contains("would not be the volume it was made from"),
            "{why}"
        );
        let ready = ready_condition(&bad.status.conditions);
        assert_eq!(ready.status, ConditionStatus::False, "{ready:?}");
        assert_eq!(ready.reason, "DigestMismatch", "{ready:?}");

        // And the bytes are still there. A failed verification is the one
        // moment somebody has to look themselves — it may be the copy that
        // rotted, or the filesystem under it, or a restore already running
        // from this very file. Deleting the only artefact takes that away.
        assert!(
            std::path::Path::new(&path).exists(),
            "the platform destroyed the copy it was asked to check"
        );
        assert!(bad.status.taken, "a failed check un-made the copy");
    }

    /// A copy that has gone missing entirely is louder, not different in kind.
    #[tokio::test]
    async fn a_copy_that_cannot_be_read_at_all_says_so() {
        let (cell, agent, path) = a_verified_copy(1).await;
        cell.backdate_backup("b1", 4 * 3_600_000).await;
        std::fs::remove_file(&path).unwrap();

        agent.resync().await;

        let gone = cell.reload_backup("b1").await;
        let why = gone
            .status
            .verify_error
            .expect("a missing copy went unnoticed");
        assert!(why.contains("could not be read back"), "{why}");
        assert_eq!(
            ready_condition(&gone.status.conditions).reason,
            "Unreadable"
        );
    }

    /// A copy from before digests existed. Not sound and not broken — nobody
    /// can tell, and a digest recorded now would only bless whatever is on the
    /// target today, which is the very thing being asked about.
    #[tokio::test]
    async fn a_copy_with_no_digest_is_called_unverifiable_rather_than_assumed_good() {
        let (cell, agent, _path) = a_verified_copy(1).await;
        cell.forget_digest("b1").await;
        cell.backdate_backup("b1", 4 * 3_600_000).await;

        agent.resync().await;

        let old = cell.reload_backup("b1").await;
        let why = old
            .status
            .verify_error
            .expect("it claimed to have checked something");
        assert!(why.contains("proves nothing"), "{why}");
        assert!(
            old.status.verified_at.is_none(),
            "it recorded a check it did not do"
        );
        let ready = ready_condition(&old.status.conditions);
        // The copy is here and restorable; what is unknown is whether it is
        // intact. That is not a broken backup, so `Ready` stays true.
        assert_eq!(ready.status, ConditionStatus::True, "{ready:?}");
        assert_eq!(ready.reason, "Unverifiable", "{ready:?}");
        assert_eq!(
            old.status.digest, None,
            "verification invented a digest, which would certify the wrong moment"
        );
    }
}
