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
        source: &VolumeSource,
        encryption_key: Option<&str>,
    ) -> Result<()>;
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
                // gets created twice. Report nothing and try again next pass.
                tracing::error!(error = %e, "could not read this pool; doing nothing this pass");
                pass.failures += 1;
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
        for volume in volumes.iter().filter(|v| v.spec.pool == self.config.pool) {
            self.volume_pass(volume, &seen, &mut pass).await;
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

        self.pool_pass(&volumes, &seen, &mut pass).await;
        pass
    }

    async fn volume_pass(&self, stored: &Volume, seen: &PoolState, pass: &mut Pass) {
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
            match self.perform(&action).await {
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

    async fn perform(&self, action: &VolumeAction) -> Result<()> {
        match action {
            VolumeAction::Provision {
                volume,
                gib,
                source,
                encryption_key,
            } => {
                self.storage
                    .provision(volume, *gib, source, encryption_key.as_deref())
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
    /// Every copy this fake holds.
    snapshots: BTreeMap<String, SnapshotInPool>,
    encrypted: BTreeMap<String, String>,
}

impl FakePool {
    pub fn new(capacity_gib: u64) -> Self {
        let me = Self::default();
        me.inner.lock().unwrap().capacity_gib = capacity_gib;
        me
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
        source: &VolumeSource,
        encryption_key: Option<&str>,
    ) -> Result<()> {
        self.fault("provision", volume)?;
        let mut inner = self.inner.lock().unwrap();
        // A volume made from a snapshot that this pool does not hold is not an
        // empty volume, it is a mistake — and a fake that quietly made one
        // anyway could not be used to prove the platform refuses it.
        if let VolumeSource::Snapshot(snapshot) = source
            && !inner.snapshots.contains_key(snapshot)
        {
            return Err(HostError::failed(format!(
                "{snapshot} is not in this pool, so nothing can be copied from it"
            )));
        }
        inner.volumes.insert(volume.to_string(), gib);
        match source {
            VolumeSource::Image(image) => {
                inner
                    .from_image
                    .insert(volume.to_string(), image.to_string());
            }
            VolumeSource::Snapshot(snapshot) => {
                inner
                    .from_snapshot
                    .insert(volume.to_string(), snapshot.to_string());
            }
            VolumeSource::Blank => {}
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
        fake: FakePool,
    }

    fn cell(pool: &str) -> (Cell, PoolAgent) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let fake = FakePool::new(1000);
        let cell = Cell {
            volumes: TypedStore::new(store.clone(), "cell-1", "volumes"),
            snapshots: TypedStore::new(store.clone(), "cell-1", "snapshots"),
            pools: TypedStore::new(store.clone(), "cell-1", "pools"),
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
                    size_gib: gib,
                    pool: pool.to_string(),
                    encryption_key: None,
                    source_image: None,
                    source_snapshot: None,
                },
                VolumeStatus::default(),
            );
            v.meta.finalizers = vec![POOL_RELEASE_FINALIZER.to_string()];
            self.volumes.create(&v).await.unwrap();
            self.reload().await
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
            self.pools.create(&p).await.unwrap();
        }

        async fn reload(&self) -> Volume {
            self.volumes.get(VOLUME).await.unwrap().unwrap()
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
            self.snapshots.create(&s).await.unwrap();
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
        // Zero writes is the right assertion *here*, unlike on a settled pass: a
        // pool that cannot be read returns before it reaches its own object, so
        // not even the heartbeat goes out. That silence is the point — a node
        // reporting a heartbeat it could not verify would look alive.
        assert_eq!(pass.reports, 0);
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
}
