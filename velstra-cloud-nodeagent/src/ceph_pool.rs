//! A pool that is an RBD pool in a Ceph cluster.
//!
//! ## What this changes, and it is not speed
//!
//! Every other backend answers "make a volume from this image" by *copying* the
//! image. On a directory pool that is a `qemu-img convert` per volume, and the
//! image has to be on that machine first — which is why a node reports
//! `status.images` and why the platform computes which nodes hold what.
//!
//! Ceph answers it with `rbd clone`, against a snapshot of an image that lives
//! **once** in the cluster. No bytes move, the volume is ready immediately, and
//! the question "which nodes hold this image" stops existing: any node with
//! cluster access reaches it. That is the structural win. The speed is a
//! consequence.
//!
//! ## Two pools, on purpose
//!
//! Images and volumes are separate RBD pools ([`CephConfig::image_pool`],
//! [`CephConfig::pool`]). They have genuinely different lifecycles — an image is
//! written once and read for years, a volume is written constantly — so they
//! want different replication, different placement and different quota. RBD
//! clones across pools have been supported since Jewel and cost nothing.
//!
//! ## Why the `rbd` command and not librbd
//!
//! The same reason [`crate::directory_pool`] shells out to `qemu-img`: linking
//! librbd means a C toolchain, Ceph headers at build time, and a version of the
//! library that has to match a cluster this repository cannot have. The command
//! is a stable, documented interface with a `--format json` mode, and it is what
//! an operator would type to check the answer by hand.
//!
//! The cost is honest and worth stating: every operation is a process, so this
//! is not the backend for a pool doing thousands of volume operations a second.
//! Nothing in this platform does.
//!
//! ## What is testable here, and what is not
//!
//! Everything that decides — which command, with which arguments, and what its
//! output means — is a pure function with tests. What cannot be tested without a
//! cluster is that Ceph does what its manual says. So the argv construction and
//! the JSON parsing are pinned here, and the integration is pinned by a live
//! cluster or not at all; pretending otherwise would be a test that asserts a
//! mock agrees with itself.

use std::collections::BTreeMap;

use velstra_cloud_model::meta::ResourceName;

use crate::{
    host::{HostError, Result},
    pool::{Origin, PoolState, SnapshotInPool, Storage},
};

/// The snapshot every image carries, and what a volume is cloned from.
///
/// One well-known name rather than one per image: a clone needs a *protected*
/// snapshot, protecting is a separate act, and a scheme where each image picked
/// its own name would mean discovering that name before every clone. `base` is
/// what OpenStack's Cinder uses against Glance-on-Ceph, for the same reason.
pub const IMAGE_BASE_SNAPSHOT: &str = "base";

/// How this agent reaches its cluster.
#[derive(Clone, Debug)]
pub struct CephConfig {
    /// The RBD pool volumes and their snapshots live in.
    pub pool: String,
    /// The RBD pool images live in. Separate from `pool`; see the module doc.
    pub image_pool: String,
    /// The Ceph client name, e.g. `client.velstra`. Its keyring has to be where
    /// `rbd` looks for it — this agent does not manage credentials, because a
    /// process that could write its own keyring could grant itself a cluster.
    pub user: String,
    /// `ceph.conf`, when it is not in the default place.
    pub conf: Option<String>,
    /// The `rbd` binary. Overridable so a test can point at something else and
    /// an operator can point at a build that is not on the path.
    pub rbd: String,
}

impl CephConfig {
    pub fn new(pool: &str, image_pool: &str) -> Self {
        Self {
            pool: pool.to_string(),
            image_pool: image_pool.to_string(),
            user: "client.admin".to_string(),
            conf: None,
            rbd: "rbd".to_string(),
        }
    }

    /// The arguments every `rbd` invocation carries.
    ///
    /// Built once here rather than repeated at each call site, because a
    /// forgotten `--id` is an operation that runs as the wrong client and either
    /// fails confusingly or succeeds where it should not have.
    fn common(&self) -> Vec<String> {
        let mut args = vec!["--id".into(), strip_client(&self.user).to_string()];
        if let Some(conf) = &self.conf {
            args.push("--conf".into());
            args.push(conf.clone());
        }
        args
    }
}

/// `client.velstra` and `velstra` both mean the same client to Ceph, and `rbd
/// --id` wants the second spelling. Accepting either is not laxity: `ceph auth`
/// prints the first, so it is what an operator copies.
fn strip_client(user: &str) -> &str {
    user.strip_prefix("client.").unwrap_or(user)
}

/// A resource name as an RBD image name.
///
/// RBD forbids `/`, and a resource name is full of them. `~` is the same
/// substitution [`crate::hostfs::slug`] makes for filenames, so one object has
/// one recognisable spelling wherever an operator meets it.
pub fn rbd_name(resource: &str) -> String {
    resource.replace('/', "~")
}

/// The inverse, for reading a pool's own listing back into resource names.
pub fn from_rbd_name(image: &str) -> String {
    image.replace('~', "/")
}

/// `pool/image`, the spelling every `rbd` subcommand takes.
fn spec(pool: &str, image: &str) -> String {
    format!("{pool}/{image}")
}

/// `pool/image@snapshot`.
fn snap_spec(pool: &str, image: &str, snapshot: &str) -> String {
    format!("{pool}/{image}@{snapshot}")
}

/// A volume's snapshot, as RBD holds it.
///
/// A snapshot's resource name is *under* its volume —
/// `projects/p1/volumes/v1/snapshots/s1` — which is exactly RBD's own model: a
/// snapshot belongs to an image and is named within it. So the volume becomes
/// the RBD image and the snapshot's **leaf id** becomes the RBD snapshot name,
/// and no separate mapping has to be kept anywhere.
///
/// Returns `None` for a name that is not a snapshot of a volume, which is the
/// same thing [`velstra_cloud_model::storage::source_volume`] refuses.
pub fn snapshot_location(snapshot: &str) -> Option<(String, String)> {
    let name = ResourceName::parse(snapshot).ok()?;
    let volume = name.parent().filter(|p| p.collection() == "volumes")?;
    Some((volume.to_string(), name.id().to_string()))
}

/// The resource name of the snapshot `snap` on the volume `volume`.
fn snapshot_name(volume: &str, snap: &str) -> String {
    format!("{volume}/snapshots/{snap}")
}

/// Bytes to GiB, rounded **up**.
///
/// Up, because this number is compared against what was asked for: rounding a
/// 10 GiB volume down to 9 would have the model see a volume smaller than its
/// spec and try to grow it on every pass, for ever.
fn gib_of(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024 * 1024)
}

// ---- what `rbd` says -------------------------------------------------------

/// One row of `rbd ls --long --format json`.
#[derive(serde::Deserialize)]
struct RbdImage {
    image: String,
    #[serde(default)]
    size: u64,
}

/// One row of `rbd snap ls --format json`.
#[derive(serde::Deserialize)]
struct RbdSnap {
    name: String,
    #[serde(default)]
    size: u64,
}

/// One row of `ceph df --format json`'s `pools`, which is where a pool's usable
/// size comes from.
#[derive(serde::Deserialize)]
struct CephDfPool {
    name: String,
    stats: CephDfStats,
}

#[derive(serde::Deserialize)]
struct CephDfStats {
    /// What can still be written, after replication. `max_avail` and not
    /// `stored`: a three-way replicated pool over 30 TiB of raw disk holds 10
    /// TiB of data, and reporting the raw number would have the scheduler
    /// place three times what fits.
    #[serde(default)]
    max_avail: u64,
    #[serde(default)]
    stored: u64,
}

/// Parse `rbd ls --long --format json` into `(resource name, gib)`.
///
/// Images whose names are not resource names are skipped rather than reported:
/// a pool may hold volumes this platform did not create, and claiming one would
/// have the model try to reconcile something nobody asked for.
pub fn parse_volumes(json: &str) -> Result<BTreeMap<String, u64>> {
    let rows: Vec<RbdImage> = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`rbd ls` did not answer with json: {e}")))?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let name = from_rbd_name(&row.image);
            ResourceName::parse(&name)
                .ok()
                .filter(|n| n.collection() == "volumes")
                .map(|_| (name, gib_of(row.size)))
        })
        .collect())
}

/// Parse `rbd snap ls <volume> --format json` for one volume.
///
/// The volume's own name is needed because RBD snapshot names are only unique
/// *within* an image, and the model's are full resource names.
pub fn parse_snapshots(volume: &str, json: &str) -> Result<BTreeMap<String, SnapshotInPool>> {
    let rows: Vec<RbdSnap> = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`rbd snap ls` did not answer with json: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                snapshot_name(volume, &row.name),
                SnapshotInPool {
                    volume: volume.to_string(),
                    gib: gib_of(row.size),
                },
            )
        })
        .collect())
}

/// Parse `ceph df --format json` for one pool's usable capacity, in GiB.
///
/// "Usable" is what is still free plus what this pool already holds — the
/// capacity of the pool as the scheduler means it, not the cluster's raw size.
pub fn parse_capacity(pool: &str, json: &str) -> Result<u64> {
    #[derive(serde::Deserialize)]
    struct Df {
        #[serde(default)]
        pools: Vec<CephDfPool>,
    }
    let df: Df = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`ceph df` did not answer with json: {e}")))?;
    let found = df.pools.iter().find(|p| p.name == pool).ok_or_else(|| {
        HostError::failed(format!(
            "the cluster has no pool called {pool}; `ceph osd pool ls` says what it does have"
        ))
    })?;
    Ok(gib_of(
        found.stats.max_avail.saturating_add(found.stats.stored),
    ))
}

// ---- the backend -----------------------------------------------------------

pub struct CephPool {
    config: CephConfig,
}

impl CephPool {
    pub fn new(config: CephConfig) -> Self {
        Self { config }
    }

    /// The argv for one `rbd` call, common arguments included.
    ///
    /// Public so the tests can pin it: which flags an operation carries is the
    /// part of this backend that decides whether it is correct, and it is the
    /// part that can be checked without a cluster.
    pub fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut argv = self.config.common();
        argv.extend(args.iter().map(|a| a.to_string()));
        argv
    }

    async fn rbd(&self, args: &[&str]) -> Result<Vec<u8>> {
        let argv = self.argv(args);
        let output = tokio::process::Command::new(&self.config.rbd)
            .args(&argv)
            .output()
            .await
            .map_err(|e| {
                HostError::failed(format!(
                    "running `{} {}`: {e}. Is the rbd command installed?",
                    self.config.rbd,
                    argv.join(" ")
                ))
            })?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(HostError::failed(format!(
            "`{} {}` failed: {}",
            self.config.rbd,
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// `ceph df`, for the pool's capacity. A different binary from `rbd`, and
    /// the only place this backend needs it.
    async fn ceph_df(&self) -> Result<Vec<u8>> {
        let mut argv = self.config.common();
        argv.extend(["df".into(), "--format".into(), "json".into()]);
        let output = tokio::process::Command::new("ceph")
            .args(&argv)
            .output()
            .await
            .map_err(|e| HostError::failed(format!("running `ceph df`: {e}")))?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(HostError::failed(format!(
            "`ceph df` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// Whether an image carries the protected snapshot a clone needs.
    ///
    /// Checked before cloning so the refusal names the missing act rather than
    /// echoing librbd's `error: parent snapshot must be protected`, which sends
    /// an operator looking at the volume rather than at the image.
    async fn image_is_clonable(&self, image: &str) -> Result<bool> {
        let out = self
            .rbd(&[
                "snap",
                "ls",
                &spec(&self.config.image_pool, image),
                "--format",
                "json",
            ])
            .await?;
        // Junk is an error, not "no snapshots": treating an unreadable answer as
        // an empty list would report the base snapshot absent and re-import an
        // image that is already there.
        let rows: Vec<RbdSnap> = serde_json::from_slice(&out).map_err(|e| {
            HostError::failed(format!("`rbd snap ls` did not answer with json: {e}"))
        })?;
        Ok(rows.iter().any(|s| s.name == IMAGE_BASE_SNAPSHOT))
    }
}

#[async_trait::async_trait]
impl Storage for CephPool {
    async fn observe(&self) -> Result<PoolState> {
        let listing = self
            .rbd(&["ls", "--long", "--format", "json", &self.config.pool])
            .await?;
        let volumes = parse_volumes(&String::from_utf8_lossy(&listing))?;

        // One `snap ls` per volume. A pool-wide listing would be one call, and
        // `rbd` has no such subcommand — so this is per-volume or it is nothing.
        // It is the reason this backend's observe cost grows with the pool, and
        // it is worth saying out loud rather than discovering at a thousand
        // volumes.
        let mut snapshots = BTreeMap::new();
        for volume in volumes.keys() {
            let out = self
                .rbd(&[
                    "snap",
                    "ls",
                    &spec(&self.config.pool, &rbd_name(volume)),
                    "--format",
                    "json",
                ])
                .await?;
            snapshots.extend(parse_snapshots(volume, &String::from_utf8_lossy(&out))?);
        }

        let df = self.ceph_df().await?;
        Ok(PoolState {
            volumes,
            capacity_gib: parse_capacity(&self.config.pool, &String::from_utf8_lossy(&df))?,
            backend: "ceph".to_string(),
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
            // The same refusal the directory pool makes, and for the same
            // reason: RBD *can* encrypt (`rbd encryption format`), and this
            // agent still has no way to turn a KMS entry into key material.
            // Making it in plaintext and reporting it ready would be the
            // platform quietly declining to do the one thing asked of it.
            return Err(HostError::failed(format!(
                "this volume asks to be encrypted with {key}, which names an entry in the \
                 project's KMS — and there is no KMS for this pool to ask. It is refused rather \
                 than made in plaintext and reported ready."
            )));
        }
        let target = spec(&self.config.pool, &rbd_name(volume));

        match source {
            Origin::Blank => {
                self.rbd(&["create", "--size", &format!("{gib}G"), &target])
                    .await?;
            }
            Origin::Image(image) => {
                let parent = rbd_name(image);
                if !self.image_is_clonable(&parent).await? {
                    return Err(HostError::failed(format!(
                        "{image} has no protected `@{IMAGE_BASE_SNAPSHOT}` snapshot in pool {}, \
                         so nothing can be cloned from it. An image is made clonable when it is \
                         imported; this one was not, or the snapshot was unprotected since.",
                        self.config.image_pool
                    )));
                }
                // The whole point of this backend: copy-on-write, no bytes
                // moved, ready when the command returns. The clone lands in the
                // volume pool while its parent stays in the image pool.
                self.rbd(&[
                    "clone",
                    &snap_spec(&self.config.image_pool, &parent, IMAGE_BASE_SNAPSHOT),
                    &target,
                ])
                .await?;
                // A clone starts at the parent's size. Growing to the asked-for
                // size is part of provisioning and not a later pass, for the
                // same reason cloning is: a volume that exists at the wrong size
                // for one pass is one a guest can be started from.
                self.grow(volume, gib).await?;
            }
            Origin::Snapshot(snapshot) => {
                let Some((from_volume, snap)) = snapshot_location(snapshot) else {
                    return Err(HostError::failed(format!(
                        "{snapshot} is not a snapshot of a volume, so there is nothing to clone \
                         it from"
                    )));
                };
                let parent = rbd_name(&from_volume);
                // A volume snapshot is not protected when it is taken — nothing
                // clones from one until somebody asks. Protecting is idempotent
                // in effect: a second attempt on an already-protected snapshot
                // is an error that means "already true", so it is not fatal.
                let _ = self
                    .rbd(&[
                        "snap",
                        "protect",
                        &snap_spec(&self.config.pool, &parent, &snap),
                    ])
                    .await;
                self.rbd(&[
                    "clone",
                    &snap_spec(&self.config.pool, &parent, &snap),
                    &target,
                ])
                .await?;
                self.grow(volume, gib).await?;
            }
            // A restore: bytes from outside the cluster entirely. `rbd import`
            // creates the image from the file, which is why there is no
            // `create` before it — and why a half-finished import leaves no
            // image rather than an empty one under the right name.
            Origin::File(from) => {
                if !std::path::Path::new(from).exists() {
                    return Err(HostError::failed(format!(
                        "{from} is not on this machine, so nothing can be restored from it. Is \
                         the target mounted here?"
                    )));
                }
                self.rbd(&["import", from, &target]).await?;
                self.grow(volume, gib).await?;
            }
        }
        Ok(())
    }

    async fn grow(&self, volume: &str, to_gib: u64) -> Result<()> {
        // `--allow-shrink` is deliberately not passed. The model refuses to
        // shrink a volume and this is the second lock on the same door: a spec
        // that somehow asked for less would have `rbd` refuse rather than
        // discard whatever is past the new end.
        self.rbd(&[
            "resize",
            "--size",
            &format!("{to_gib}G"),
            &spec(&self.config.pool, &rbd_name(volume)),
        ])
        .await
        .map(|_| ())
    }

    async fn destroy(&self, volume: &str) -> Result<()> {
        // `rbd rm`, deliberately: a volume that still has clones cannot be
        // removed, and `rm` says so with `image has snapshots - not removing`.
        // That refusal is correct and this backend keeps it — the model's own
        // rule is that a volume with snapshots is not destroyed underneath them,
        // and Ceph enforcing it too means the two cannot disagree.
        self.rbd(&["rm", &spec(&self.config.pool, &rbd_name(volume))])
            .await
            .map(|_| ())
    }

    async fn copy_out(&self, volume: &str, path: &str) -> Result<u64> {
        // `rbd export` writes the image out as a flat file. It returns when the
        // file is written, which is what `Storage::copy_out` requires — and it
        // is written to a `.partial` first, because the name a restore will
        // read months from now must never have been on a half-written file.
        let partial = format!("{path}.partial");
        let _ = std::fs::remove_file(&partial);
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                HostError::failed(format!("{} is not usable: {e}", parent.display()))
            })?;
        }
        if let Err(e) = self
            .rbd(&[
                "export",
                &spec(&self.config.pool, &rbd_name(volume)),
                &partial,
            ])
            .await
        {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        std::fs::rename(&partial, path).map_err(|e| {
            HostError::failed(format!("{path} was exported and could not be moved into place: {e}"))
        })?;
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| HostError::failed(format!("{path} was written and cannot be read: {e}")))
    }

    async fn take_snapshot(&self, snapshot: &str, volume: &str) -> Result<()> {
        let Some((of_volume, snap)) = snapshot_location(snapshot) else {
            return Err(HostError::failed(format!(
                "{snapshot} is not a snapshot of a volume"
            )));
        };
        if of_volume != volume {
            // The name says one volume and the caller says another. Refusing is
            // the only safe answer: taking it under the caller's volume would
            // file a copy of the wrong disk under the right label, and the
            // model never takes a snapshot twice — so it would be wrong for ever.
            return Err(HostError::failed(format!(
                "{snapshot} names {of_volume} as its source and was asked for against {volume}"
            )));
        }
        // `rbd snap create` returns when the snapshot exists. RBD snapshots are
        // point-in-time by construction, so there is no half-written moment for
        // the pool to observe — which is what `Storage::take_snapshot` requires
        // and what the directory pool needs its rename dance to achieve.
        self.rbd(&[
            "snap",
            "create",
            &snap_spec(&self.config.pool, &rbd_name(volume), &snap),
        ])
        .await
        .map(|_| ())
    }

    async fn destroy_snapshot(&self, snapshot: &str) -> Result<()> {
        let Some((volume, snap)) = snapshot_location(snapshot) else {
            return Err(HostError::failed(format!(
                "{snapshot} is not a snapshot of a volume"
            )));
        };
        let target = snap_spec(&self.config.pool, &rbd_name(&volume), &snap);
        // Unprotect first, and ignore its failure: a snapshot nothing was ever
        // cloned from was never protected, and "it was not protected" is the
        // same outcome as "it no longer is". A snapshot that still has clones
        // fails the *remove* below, with the message that names them.
        let _ = self.rbd(&["snap", "unprotect", &target]).await;
        self.rbd(&["snap", "rm", &target]).await.map(|_| ())
    }
}

// ---- getting an image into the cluster -------------------------------------

/// Publishing an image into the cluster, so every node can clone from it.
///
/// This is the half a cloud usually calls an image service. It is small because
/// the platform's image model does the hard part already: an image is
/// **content-addressed** — its id *is* its digest — so "is this the right image"
/// is a question with an arithmetic answer rather than a registry to trust.
///
/// ## The order matters, and it is the whole of the correctness here
///
/// 1. Is it already there *and clonable*? A re-run then costs one cluster read
///    instead of hashing gigabytes.
/// 2. Hash the file and compare it against the name — before anything is
///    **written**. An image published under a name it does not match is one
///    every future volume silently inherits, and the digest is the only thing
///    that would have caught it.
/// 3. Import under a temporary name.
/// 4. Snapshot it and protect the snapshot.
/// 5. Rename into place.
///
/// Four and five are the reason for three. An image is clonable only once it has
/// a *protected* snapshot, so an agent that died between the import and the
/// protect would leave an image that exists, looks finished, and refuses every
/// clone — with an error about the parent, pointing an operator at the volume.
/// Publishing the name last means the name never exists unfinished, which is the
/// same discipline `directory_pool` gets from writing `.partial` and renaming.
impl CephPool {
    /// The image pool this publishes into.
    pub fn image_pool(&self) -> &str {
        &self.config.image_pool
    }

    /// Whether this image is already in the cluster and clonable.
    ///
    /// Both halves: an image that exists without its protected snapshot is not
    /// importable-as-done, it is half-imported, and reporting it present would
    /// leave it that way for ever.
    pub async fn image_present(&self, image: &str) -> Result<bool> {
        let name = rbd_name(image);
        let listing = self
            .rbd(&["ls", "--format", "json", &self.config.image_pool])
            .await?;
        // Junk is an error, not "no images": an unreadable listing read as empty
        // would report the image absent and re-import it over a copy that exists.
        let names: Vec<String> = serde_json::from_slice(&listing)
            .map_err(|e| HostError::failed(format!("`rbd ls` did not answer with json: {e}")))?;
        if !names.iter().any(|n| n == &name) {
            return Ok(false);
        }
        self.image_is_clonable(&name).await
    }

    /// Publish `file` as `image`, verifying it against the digest in its name.
    ///
    /// Idempotent: an image already present and clonable is left alone, so this
    /// is safe to run from a loop, a retry or an operator who is not sure.
    pub async fn import_image(&self, image: &str, file: &std::path::Path) -> Result<bool> {
        let expected = crate::hostfs::digest_of(image).ok_or_else(|| {
            HostError::failed(format!(
                "{image} does not carry a sha256 in its name, so there is nothing to verify \
                 the bytes against. An image's id is its digest — that is what makes it safe \
                 to clone from years later."
            ))
        })?;

        if self.image_present(image).await? {
            return Ok(false);
        }

        // First, and before a byte is written into the cluster. An image
        // published under a name it does not match is inherited by every volume
        // cloned from it, and this is the only place the mismatch is visible.
        let actual = crate::hostfs::sha256_file(file)
            .await
            .map_err(|e| HostError::failed(format!("hashing {}: {e}", file.display())))?;
        if actual != expected {
            return Err(HostError::failed(format!(
                "{} hashes to sha256:{actual}, and {image} commits to sha256:{expected}. \
                 Refused: publishing it would put the wrong bytes behind a name every future \
                 volume trusts.",
                file.display()
            )));
        }

        let final_name = rbd_name(image);
        let staging = format!("{final_name}.importing");
        let staged = spec(&self.config.image_pool, &staging);

        // A staging image from a previous attempt is exactly as unfinished as
        // this one is about to be, and removing it is safe for that reason.
        let _ = self.rbd(&["rm", &staged]).await;

        self.rbd(&["import", &file.to_string_lossy(), &staged])
            .await?;
        self.rbd(&[
            "snap",
            "create",
            &snap_spec(&self.config.image_pool, &staging, IMAGE_BASE_SNAPSHOT),
        ])
        .await?;
        self.rbd(&[
            "snap",
            "protect",
            &snap_spec(&self.config.image_pool, &staging, IMAGE_BASE_SNAPSHOT),
        ])
        .await?;
        // The name appears finished or not at all.
        self.rbd(&["mv", &staged, &spec(&self.config.image_pool, &final_name)])
            .await?;
        Ok(true)
    }

    /// Remove an image from the cluster.
    ///
    /// Refuses while any volume is still a clone of it, and does not offer to
    /// flatten them: flattening is a full copy per volume, so a delete that
    /// quietly did it would turn one operator's tidy-up into hours of cluster
    /// traffic and a storage bill nobody agreed to. The refusal names the
    /// children, which is what an operator needs to decide.
    pub async fn remove_image(&self, image: &str) -> Result<()> {
        let name = rbd_name(image);
        let children = self
            .rbd(&[
                "children",
                &snap_spec(&self.config.image_pool, &name, IMAGE_BASE_SNAPSHOT),
            ])
            .await
            .unwrap_or_default();
        let children = String::from_utf8_lossy(&children);
        let still: Vec<&str> = children
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if !still.is_empty() {
            return Err(HostError::failed(format!(
                "{image} is still the parent of {} volume(s) — {}{}. Removing it would make \
                 them unreadable. Flatten them first if that is really what you want; it copies \
                 every volume in full.",
                still.len(),
                still
                    .iter()
                    .take(3)
                    .map(|c| from_rbd_name(c))
                    .collect::<Vec<_>>()
                    .join(", "),
                if still.len() > 3 { ", …" } else { "" }
            )));
        }
        let _ = self
            .rbd(&[
                "snap",
                "unprotect",
                &snap_spec(&self.config.image_pool, &name, IMAGE_BASE_SNAPSHOT),
            ])
            .await;
        let _ = self
            .rbd(&[
                "snap",
                "rm",
                &snap_spec(&self.config.image_pool, &name, IMAGE_BASE_SNAPSHOT),
            ])
            .await;
        self.rbd(&["rm", &spec(&self.config.image_pool, &name)])
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> CephPool {
        let mut config = CephConfig::new("velstra-volumes", "velstra-images");
        config.user = "client.velstra".into();
        CephPool::new(config)
    }

    /// An image whose name carries no digest cannot be published at all.
    ///
    /// The name *is* the promise about the bytes. Without one there is nothing
    /// to verify against, and an image that cannot be verified is one every
    /// volume cloned from it inherits on trust.
    #[tokio::test]
    async fn an_image_with_no_digest_in_its_name_is_refused_before_anything_is_written() {
        // `rbd` deliberately points at something that is not there: if this
        // reached the cluster at all, the error would be about the command
        // rather than about the name, and that is the failure being ruled out.
        let mut config = CephConfig::new("v", "i");
        config.rbd = "/nonexistent/rbd".into();
        let pool = CephPool::new(config);
        let err = pool
            .import_image(
                "projects/p1/images/ubuntu-24.04",
                std::path::Path::new("/dev/null"),
            )
            .await
            .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("does not carry a sha256"), "{text}");
        assert!(
            !text.contains("rbd"),
            "it reached the cluster first: {text}"
        );
    }

    #[test]
    fn the_digest_in_a_name_is_what_an_import_verifies_against() {
        // Both spellings, because `sha256:` is what the model writes and
        // `sha256-` is what survives a resource name — and an import that
        // understood only one would refuse half the images in the cell.
        let hex = "a".repeat(64);
        assert_eq!(
            crate::hostfs::digest_of(&format!("projects/p1/images/sha256:{hex}")),
            Some(hex.clone())
        );
        assert_eq!(
            crate::hostfs::digest_of(&format!("projects/p1/images/sha256-{hex}")),
            Some(hex)
        );
        // Not a digest: too short, and not hex.
        assert_eq!(
            crate::hostfs::digest_of("projects/p1/images/sha256-abc"),
            None
        );
        assert_eq!(
            crate::hostfs::digest_of(&format!("projects/p1/images/sha256-{}", "z".repeat(64))),
            None
        );
    }

    #[test]
    fn a_resource_name_survives_the_trip_through_rbd_and_back() {
        let name = "projects/p1/volumes/v1";
        assert_eq!(rbd_name(name), "projects~p1~volumes~v1");
        assert_eq!(from_rbd_name(&rbd_name(name)), name);
    }

    #[test]
    fn every_call_carries_the_client_it_should_run_as() {
        // A forgotten `--id` runs as `client.admin` if a keyring happens to be
        // there, which either fails confusingly or succeeds where it should not.
        let argv = pool().argv(&["ls", "velstra-volumes"]);
        assert_eq!(argv, ["--id", "velstra", "ls", "velstra-volumes"]);

        // `ceph auth` prints `client.velstra` and `rbd --id` wants `velstra`.
        // Accepting the spelling an operator copies is not laxity.
        assert_eq!(strip_client("client.velstra"), "velstra");
        assert_eq!(strip_client("velstra"), "velstra");
    }

    #[test]
    fn a_configured_conf_file_reaches_every_call() {
        let mut config = CephConfig::new("v", "i");
        config.conf = Some("/etc/ceph/other.conf".into());
        let argv = CephPool::new(config).argv(&["ls"]);
        assert_eq!(
            argv,
            ["--id", "admin", "--conf", "/etc/ceph/other.conf", "ls"]
        );
    }

    #[test]
    fn a_snapshot_is_located_by_its_name_and_nothing_else() {
        // The volume is the RBD image and the leaf is the RBD snapshot, which is
        // RBD's own model — so nothing has to be stored to find one again.
        assert_eq!(
            snapshot_location("projects/p1/volumes/v1/snapshots/nightly"),
            Some(("projects/p1/volumes/v1".into(), "nightly".into()))
        );
        // A name that is not a snapshot of a volume is refused, exactly as
        // `storage::source_volume` refuses it.
        assert_eq!(snapshot_location("projects/p1/snapshots/orphan"), None);
        assert_eq!(snapshot_location("nonsense"), None);
    }

    #[test]
    fn a_listing_becomes_volumes_and_skips_what_is_not_ours() {
        // A real pool holds things this platform did not create — another
        // tenant's, an operator's scratch image, a Cinder volume from a cluster
        // this one shares. Claiming one would have the model reconcile something
        // nobody asked for.
        let json = r#"[
            {"image":"projects~p1~volumes~v1","size":10737418240},
            {"image":"projects~p1~volumes~v2","size":1073741825},
            {"image":"somebody-elses-disk","size":1073741824},
            {"image":"projects~p1~instances~i1","size":1073741824}
        ]"#;
        let volumes = parse_volumes(json).unwrap();
        assert_eq!(volumes.len(), 2, "{volumes:?}");
        assert_eq!(volumes["projects/p1/volumes/v1"], 10);
        // Rounded **up**: one byte over 1 GiB is 2, because this number is
        // compared against what was asked for and rounding down would have the
        // model grow the volume on every pass, for ever.
        assert_eq!(volumes["projects/p1/volumes/v2"], 2);
    }

    #[test]
    fn snapshots_are_named_under_the_volume_they_belong_to() {
        let json = r#"[{"name":"nightly","size":10737418240},{"name":"before-upgrade","size":10737418240}]"#;
        let snaps = parse_snapshots("projects/p1/volumes/v1", json).unwrap();
        assert_eq!(snaps.len(), 2);
        let one = &snaps["projects/p1/volumes/v1/snapshots/nightly"];
        assert_eq!(one.volume, "projects/p1/volumes/v1");
        assert_eq!(one.gib, 10);
    }

    #[test]
    fn capacity_is_what_the_pool_can_hold_not_what_the_cluster_has_raw() {
        // `max_avail` is post-replication. A three-way replicated pool over 30
        // TiB of raw disk holds 10 TiB, and reporting the raw number would have
        // the scheduler place three times what fits.
        let json = r#"{"pools":[
            {"name":"velstra-volumes","stats":{"stored":107374182400,"max_avail":1073741824000}},
            {"name":"other","stats":{"stored":1,"max_avail":1}}
        ]}"#;
        // 100 GiB stored + 1000 GiB still writable.
        assert_eq!(parse_capacity("velstra-volumes", json).unwrap(), 1100);

        // A pool that is not there is an error that names how to find out what
        // is, rather than a zero the scheduler would read as "full".
        let err = parse_capacity("velstra-missing", json).unwrap_err();
        assert!(format!("{err}").contains("osd pool ls"), "{err}");
    }

    #[test]
    fn output_that_is_not_json_is_an_error_rather_than_an_empty_pool() {
        // What a permission failure, a wrong `--id` or an rbd that wrote a
        // warning to stdout looks like. Reading it as "no volumes" would have
        // the agent report an empty pool and the model provision everything in
        // it a second time.
        for junk in ["", "not json", "rbd: error opening pool"] {
            assert!(parse_volumes(junk).is_err(), "{junk:?}");
            assert!(parse_snapshots("projects/p1/volumes/v1", junk).is_err());
            assert!(parse_capacity("p", junk).is_err());
        }
    }

    #[test]
    fn a_clone_names_the_image_pool_for_its_parent_and_the_volume_pool_for_itself() {
        // The two pools are the design, so this pins that a clone crosses them
        // in the right direction. Getting it backwards makes a volume in the
        // image pool, which no quota or replication policy expects.
        assert_eq!(
            snap_spec(
                "velstra-images",
                "projects~p1~images~sha256-abc",
                IMAGE_BASE_SNAPSHOT
            ),
            "velstra-images/projects~p1~images~sha256-abc@base"
        );
        assert_eq!(
            spec("velstra-volumes", "projects~p1~volumes~v1"),
            "velstra-volumes/projects~p1~volumes~v1"
        );
    }
}
