//! A pool that is a directory of qcow2 files.
//!
//! The first real [`Storage`]. Until this existed the only one was
//! [`crate::pool::FakePool`], so nothing anywhere provisioned a byte: a Volume
//! could be created through the API, claimed, and reported `provisioned` by a
//! backend that had put it in a `BTreeMap`.
//!
//! ## Why a directory and not LVM, ZFS or Ceph
//!
//! Because it is the one backend that needs nothing from the machine. LVM wants
//! a volume group and `CAP_SYS_ADMIN`, ZFS wants a pool and a kernel module,
//! Ceph wants a cluster — each is a better production answer and none of them
//! can be exercised on a laptop, which is how a backend goes a year without
//! anybody running it. This one needs a writable directory and `qemu-img`, and
//! it is a perfectly reasonable production choice on a node with a good local
//! filesystem underneath it.
//!
//! Everything here is done by `qemu-img` rather than by reimplementing qcow2.
//!
//! ## Nothing is ever reported half-written
//!
//! Every file is built under a `.partial` name and **renamed into place** when
//! `qemu-img` says it is finished. A rename within one directory is atomic, so
//! `observe` sees a file that is either complete or not there. This is not
//! ceremony: an agent killed during a copy would otherwise leave a truncated
//! file that reads as a finished volume — and for a *snapshot* it would be
//! permanent, because [`velstra_cloud_model::storage::reconcile_snapshot`]
//! deliberately never takes one twice.
//!
//! ## What it refuses
//!
//! A volume asking for encryption. `VolumeSpec::encryption_key` names an entry
//! in the project's KMS, there is no KMS in this codebase yet, and nothing here
//! can turn that name into key material. Making the volume in plaintext and
//! reporting it `Ready` would be the platform quietly declining to encrypt
//! something somebody asked it to encrypt, which is the one outcome worth
//! failing loudly to avoid.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use velstra_cloud_model::meta::ResourceName;

use crate::{
    host::{HostError, Result},
    hostfs::{slug, unslug},
    pool::{Origin, PoolState, SnapshotInPool, Storage},
};

const GIB: u64 = 1024 * 1024 * 1024;

pub struct DirectoryPool {
    /// Volumes live here as `<slug>.qcow2`; copies live in `snapshots/`
    /// underneath it. The whole directory belongs to the platform.
    dir: PathBuf,
    /// Where an image named by a volume's `source_image` is found, under the
    /// same slugging the node agent uses for the images it publishes. A pool and
    /// a node on one machine can therefore share one directory.
    images: PathBuf,
    /// Only ever `qemu-img`, except in the tests that ask what happens when it
    /// cannot be run.
    qemu_img: String,
}

impl DirectoryPool {
    pub fn new(dir: impl Into<PathBuf>, images: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            images: images.into(),
            qemu_img: "qemu-img".to_string(),
        }
    }

    fn snapshot_dir(&self) -> PathBuf {
        self.dir.join("snapshots")
    }

    fn volume_path(&self, volume: &str) -> PathBuf {
        self.dir.join(format!("{}.qcow2", slug(volume)))
    }

    fn snapshot_path(&self, snapshot: &str) -> PathBuf {
        self.snapshot_dir()
            .join(format!("{}.qcow2", slug(snapshot)))
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = tokio::process::Command::new(&self.qemu_img)
            .args(args)
            .output()
            .await
            .map_err(|e| {
                HostError::failed(format!("running `qemu-img {}`: {e}", args.join(" ")))
            })?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(HostError::failed(format!(
            "`qemu-img {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// The size a guest sees, in GiB, rounded **up**.
    ///
    /// Up rather than down because the number is compared with what somebody
    /// asked for: a 100 GiB volume that read back as 99 would be grown on every
    /// pass, for ever.
    /// How big a volume is, as declared rather than as occupied.
    ///
    /// `-U` — "force share", QEMU's own name for it — and the whole pool depends
    /// on it. `qemu-img info` takes a **write** lock by default, so a volume a
    /// guest has open answers
    ///
    /// ```text
    /// Failed to get shared "write" lock
    /// Is another process using the image […]?
    /// ```
    ///
    /// and `observe` returned `Err` for the pool as a whole: "could not read this
    /// pool; doing nothing this pass", every thirty seconds, for ever. **One
    /// attached disk stopped every volume in the pool from being provisioned**,
    /// which made attaching a disk a way to take storage down for everybody in
    /// the cell.
    ///
    /// Found on a live cell within minutes of the first attach that worked. It
    /// could not have appeared before that: nothing had ever held a volume open.
    ///
    /// Reading is all this does, so sharing is not a compromise — it is what the
    /// operation actually is.
    async fn virtual_gib(&self, file: &Path) -> Result<u64> {
        let out = self
            .run(&["info", "-U", "--output=json", &file.to_string_lossy()])
            .await?;
        let info: serde_json::Value = serde_json::from_slice(&out).map_err(|e| {
            HostError::failed(format!("{}: unreadable qemu-img info: {e}", file.display()))
        })?;
        let bytes = info
            .get("virtual-size")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                HostError::failed(format!("{}: qemu-img info said no size", file.display()))
            })?;
        Ok(bytes.div_ceil(GIB))
    }

    /// The qcow2 files in one directory, as resource name to path.
    ///
    /// A `.partial` is skipped by construction: it does not end in `.qcow2`.
    fn files_in(dir: &Path) -> Vec<(String, PathBuf)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_name()?.to_str()?;
                let stem = name.strip_suffix(".qcow2")?;
                Some((unslug(stem), path))
            })
            .collect()
    }

    /// How large the filesystem under this pool is.
    ///
    /// The bytes **available** on the filesystem behind this pool — not its
    /// total size.
    ///
    /// A pool's directory very often shares a filesystem with the operating
    /// system, and reporting the filesystem's total told the scheduler a disk
    /// was empty that the OS had half-filled. `df`'s fourth column already
    /// accounts for everything else on the filesystem, so it is the honest
    /// starting point; `observe` adds back what this pool itself holds, so that
    /// `capacity - allocated` comes back to exactly this free space.
    ///
    /// Asked of the machine, because a pool's room is not something an
    /// operator's configuration file knows. A failure here is an error rather
    /// than a zero: a pool reporting no room is a pool the scheduler will not
    /// place anything on, which is a very confident statement to make out of not
    /// knowing.
    async fn available_gib(&self) -> Result<u64> {
        let out = tokio::process::Command::new("df")
            .args(["-B1", "-P", &self.dir.to_string_lossy()])
            .output()
            .await
            .map_err(|e| {
                HostError::failed(format!("asking df about {}: {e}", self.dir.display()))
            })?;
        if !out.status.success() {
            return Err(HostError::failed(format!(
                "df could not measure {}: {}",
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // `Filesystem 1-blocks Used Available Capacity Mounted` — the fourth
        // field is what is free, and a long device name can wrap the header
        // onto two lines, so the data row is found by its mount point rather
        // than by counting lines.
        let blocks = text
            .lines()
            .find_map(|line| {
                let cols: Vec<&str> = line.split_whitespace().collect();
                (cols.len() >= 4 && cols[0] != "Filesystem")
                    .then(|| cols[3].parse::<u64>().ok())
                    .flatten()
            })
            .ok_or_else(|| {
                HostError::failed(format!(
                    "df said something unreadable about {}: {text}",
                    self.dir.display()
                ))
            })?;
        Ok(blocks / GIB)
    }

    /// Build a file under a `.partial` name and move it into place only once
    /// `qemu-img` has finished with it. See the module doc.
    async fn atomically(&self, final_path: &Path, args: Vec<String>) -> Result<()> {
        let partial = final_path.with_extension("partial");
        // A partial from a previous life of this agent. Removing it is safe
        // precisely because it was never a complete anything.
        let _ = std::fs::remove_file(&partial);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(e) = self.run(&refs).await {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        std::fs::rename(&partial, final_path).map_err(|e| {
            HostError::failed(format!(
                "{} was built and could not be moved into place: {e}",
                final_path.display()
            ))
        })
    }
}

#[async_trait::async_trait]
impl Storage for DirectoryPool {
    fn at(&self, volume: &str) -> Option<String> {
        Some(self.volume_path(volume).to_string_lossy().into_owned())
    }

    async fn observe(&self) -> Result<PoolState> {
        std::fs::create_dir_all(self.snapshot_dir())
            .map_err(|e| HostError::failed(format!("{} is not usable: {e}", self.dir.display())))?;

        let mut volumes = BTreeMap::new();
        for (name, path) in Self::files_in(&self.dir) {
            // One file this pool cannot measure is one volume's size unknown,
            // not a pool that cannot be read. The `?` that used to be here made
            // every volume in the pool wait on the worst file in it — and the
            // report that comes out of a failed observe is "doing nothing this
            // pass", which is a whole pool stopped by one bad object.
            match self.virtual_gib(&path).await {
                Ok(gib) => {
                    volumes.insert(name, gib);
                }
                Err(e) => tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    "could not measure this volume; the rest of the pool is reported anyway"
                ),
            }
        }

        let mut snapshots = BTreeMap::new();
        for (name, path) in Self::files_in(&self.snapshot_dir()) {
            // The source is in a snapshot's identity rather than in a field, so
            // it is read back out of the name — the same rule
            // `storage::source_volume` applies to the object. A file whose name
            // is not a snapshot of a volume is not attributed to one.
            let Some(volume) = ResourceName::parse(&name)
                .ok()
                .and_then(|n| n.parent())
                .filter(|parent| parent.collection() == "volumes")
            else {
                continue;
            };
            snapshots.insert(
                name,
                SnapshotInPool {
                    volume: volume.to_string(),
                    gib: self.virtual_gib(&path).await?,
                },
            );
        }

        // Free space plus what this pool already holds, so the number a
        // scheduler subtracts an allocation from lands back on the free space —
        // the same accounting the LVM pool does, for the same reason: the
        // filesystem may not be this pool's alone.
        let mine_gib: u64 =
            volumes.values().sum::<u64>() + snapshots.values().map(|s| s.gib).sum::<u64>();
        Ok(PoolState {
            volumes,
            capacity_gib: self.available_gib().await? + mine_gib,
            backend: "directory".to_string(),
            snapshots,
        })
    }

    async fn provision(
        &self,
        volume: &str,
        gib: u64,
        source: Origin<'_>,
        encryption_key: Option<&str>,
    ) -> Result<()> {
        if let Some(key) = encryption_key {
            return Err(HostError::failed(format!(
                "this volume asks to be encrypted with {key}, which names an entry in the \
                 project's KMS — and there is no KMS for this pool to ask. It is refused rather \
                 than made in plaintext and reported ready."
            )));
        }
        let path = self.volume_path(volume);
        let partial = path.with_extension("partial").to_string_lossy().to_string();
        let size = format!("{gib}G");

        match source {
            Origin::Blank => {
                self.atomically(
                    &path,
                    vec!["create".into(), "-f".into(), "qcow2".into(), partial, size],
                )
                .await
            }
            // Cloning **is** creating it, never a step afterwards: a volume that
            // exists blank for one pass is one a guest can be started from, and
            // that guest finds nothing. The convert writes the partial and the
            // rename publishes it, so there is no moment at which the name exists
            // and the bytes do not.
            Origin::Image(image) => {
                let from = self.images.join(slug(image));
                if !from.exists() {
                    return Err(HostError::failed(format!(
                        "{image} is not on this machine, so nothing can be copied from it — \
                         looked in {}",
                        from.display()
                    )));
                }
                self.atomically(
                    &path,
                    vec![
                        "convert".into(),
                        "-O".into(),
                        "qcow2".into(),
                        from.to_string_lossy().to_string(),
                        partial,
                    ],
                )
                .await?;
                self.grow(volume, gib).await
            }
            Origin::Snapshot(snapshot) => {
                let from = self.snapshot_path(snapshot);
                if !from.exists() {
                    return Err(HostError::failed(format!(
                        "{snapshot} is not in this pool, so nothing can be copied from it"
                    )));
                }
                // A full copy rather than a backing-file chain. A chain is
                // cheaper and makes the new volume unreadable the moment the
                // snapshot it hangs off is deleted — which the platform allows,
                // because a snapshot's own guard is about volumes it was taken
                // *from*, not volumes made from it.
                self.atomically(
                    &path,
                    vec![
                        "convert".into(),
                        "-O".into(),
                        "qcow2".into(),
                        from.to_string_lossy().to_string(),
                        partial,
                    ],
                )
                .await?;
                self.grow(volume, gib).await
            }
            // A restore. The same convert as a snapshot, from a file that is
            // not in this pool at all — which is the whole point of a backup,
            // and the reason the path is resolved by the agent rather than
            // guessed here.
            Origin::File(from) => {
                if !std::path::Path::new(from).exists() {
                    return Err(HostError::failed(format!(
                        "{from} is not on this machine, so nothing can be restored from it. Is \
                         the target mounted here?"
                    )));
                }
                self.atomically(
                    &path,
                    vec![
                        "convert".into(),
                        "-O".into(),
                        "qcow2".into(),
                        from.to_string(),
                        partial,
                    ],
                )
                .await?;
                self.grow(volume, gib).await
            }
        }
    }

    async fn grow(&self, volume: &str, to_gib: u64) -> Result<()> {
        let path = self.volume_path(volume);
        // Never smaller, and checked here as well as in the model: `qemu-img
        // resize` will happily shrink a qcow2 with `--shrink`, and without it it
        // refuses — but a backend that relied on the caller for that would be
        // one `--shrink` away from destroying a filesystem.
        let now = self.virtual_gib(&path).await?;
        if to_gib <= now {
            return Ok(());
        }
        self.run(&["resize", &path.to_string_lossy(), &format!("{to_gib}G")])
            .await
            .map(|_| ())
    }

    async fn destroy(&self, volume: &str) -> Result<()> {
        let path = self.volume_path(volume);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already gone is what was wanted. The pass is level-triggered and
            // asks again on every round.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HostError::failed(format!(
                "{} could not be removed: {e}",
                path.display()
            ))),
        }
    }

    async fn copy_out(&self, volume: &str, path: &str) -> Result<u64> {
        let from = self.volume_path(volume);
        if !from.exists() {
            return Err(HostError::failed(format!(
                "{volume} is not in this pool, so there is nothing to copy out"
            )));
        }
        let to = std::path::Path::new(path);
        if let Some(parent) = to.parent() {
            // The target's own directory has to be there already — an agent
            // that created one could create it on the wrong machine, on a root
            // filesystem, exactly when a mount failed to come up. What is made
            // here is only the layer *inside* it that groups one cell's copies.
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    HostError::failed(format!("{} is not usable: {e}", parent.display()))
                })?;
            }
        }
        // The same rename dance as a snapshot, for a stronger reason: this file
        // is what a restore reads, months later, from a machine that has
        // forgotten everything about today. A half-written one that carried the
        // final name would be a backup somebody trusts and cannot use.
        self.atomically(
            to,
            vec![
                "convert".into(),
                "-O".into(),
                "qcow2".into(),
                from.to_string_lossy().to_string(),
                to.with_extension("partial").to_string_lossy().to_string(),
            ],
        )
        .await?;
        let written = std::fs::metadata(to).map(|m| m.len()).map_err(|e| {
            HostError::failed(format!(
                "{} was written and cannot be read: {e}",
                to.display()
            ))
        })?;
        Ok(written)
    }

    async fn take_snapshot(&self, snapshot: &str, volume: &str) -> Result<()> {
        let from = self.volume_path(volume);
        if !from.exists() {
            return Err(HostError::failed(format!(
                "{volume} is not in this pool, so there is nothing to copy"
            )));
        }
        std::fs::create_dir_all(self.snapshot_dir()).map_err(|e| {
            HostError::failed(format!(
                "{} is not usable: {e}",
                self.snapshot_dir().display()
            ))
        })?;
        let path = self.snapshot_path(snapshot);
        let partial = path.with_extension("partial").to_string_lossy().to_string();
        // A full copy, and the rename is what makes it exist. Anything that
        // reported a half-written copy would be reporting it for ever: the model
        // never takes a snapshot a second time, because a copy made later is a
        // copy of a different moment under a name somebody trusts.
        self.atomically(
            &path,
            vec![
                "convert".into(),
                "-O".into(),
                "qcow2".into(),
                from.to_string_lossy().to_string(),
                partial,
            ],
        )
        .await
    }

    async fn destroy_snapshot(&self, snapshot: &str) -> Result<()> {
        let path = self.snapshot_path(snapshot);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HostError::failed(format!(
                "{} could not be removed: {e}",
                path.display()
            ))),
        }
    }
}
