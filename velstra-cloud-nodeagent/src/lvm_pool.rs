//! Volumes as LVM logical volumes.
//!
//! The third backend, beside a directory of qcow2 files and Ceph RBD. It is the
//! one most single-machine estates already have: a volume group over the disks
//! that are in the box, which gives a guest a block device with no filesystem
//! and no image format between it and the disk.
//!
//! ## What a volume is here
//!
//! One logical volume per resource, in one volume group. The guest is handed the
//! device node — `/dev/<vg>/<lv>` — so there is no qcow2 layer, no host page
//! cache doubling, and no file to grow: a `lvextend` is the whole of a resize.
//!
//! ## Thin pools, and why they are asked for rather than assumed
//!
//! A **snapshot** of a thick logical volume needs its own space reserved up
//! front, and one that fills up is dropped by the kernel — a snapshot that
//! silently stops being one. A thin pool has neither problem, and its snapshots
//! cost nothing until something is written.
//!
//! So `--lvm-thin-pool` is a real question with a real consequence, and this
//! backend says which it is running rather than guessing: without one, snapshots
//! are made with an explicit size and a full one is a failure somebody sees;
//! with one, they are made thin.
//!
//! ## Names
//!
//! LVM's name charset is `[a-zA-Z0-9+_.-]`, which excludes both `/` and the `~`
//! that [`crate::hostfs::slug`] uses everywhere else — so this backend has its
//! own spelling, with `/` becoming `-`. That is not reversible on its own
//! (`a-b` could have been `a/b` or `a-b`), so the mapping is *checked* rather
//! than inverted: a listing is matched against the volumes the pool was asked
//! about, and an LV this cell did not make is left alone rather than guessed at.

use std::collections::BTreeMap;

use velstra_cloud_model::meta::ResourceName;

use crate::{
    host::{HostError, Result},
    pool::{Origin, PoolState, SnapshotInPool, Storage},
};

/// How to reach LVM, and what to make volumes in.
#[derive(Clone, Debug)]
pub struct LvmConfig {
    /// The volume group everything is made in. One group per pool: a pool is a
    /// place bytes go, and two groups would be two places.
    pub group: String,
    /// A thin pool inside the group, if there is one. See the module note — the
    /// difference is what a snapshot costs and how it fails.
    pub thin_pool: Option<String>,
    /// Where the tools are, for a machine that keeps them somewhere unusual.
    pub lvs: String,
    pub vgs: String,
    pub lvcreate: String,
    pub lvremove: String,
    pub lvextend: String,
    /// For writing an image into a fresh volume, and reading one out.
    pub qemu_img: String,
}

impl LvmConfig {
    pub fn new(group: &str) -> Self {
        Self {
            group: group.to_string(),
            thin_pool: None,
            lvs: "lvs".into(),
            vgs: "vgs".into(),
            lvcreate: "lvcreate".into(),
            lvremove: "lvremove".into(),
            lvextend: "lvextend".into(),
            qemu_img: "qemu-img".into(),
        }
    }
}

pub struct LvmPool {
    config: LvmConfig,
}

/// A resource name as a logical volume name.
///
/// `/` becomes `-`, because LVM's charset has no `/` and no `~`. Prefixed, so
/// this cell's volumes are distinguishable from whatever else the operator keeps
/// in the same group — and so a listing can be filtered without guessing.
pub fn lv_name(resource: &str) -> String {
    format!("velstra-{}", resource.replace('/', "-"))
}

/// A snapshot's logical volume name. The same rule, under its own prefix, so a
/// listing can tell the two apart without parsing resource names back out.
pub fn snap_lv_name(resource: &str) -> String {
    format!("velstrasnap-{}", resource.replace('/', "-"))
}

impl LvmPool {
    pub fn new(config: LvmConfig) -> Self {
        Self { config }
    }

    /// `/dev/<vg>/<lv>`, which is what a guest is handed.
    fn device(&self, lv: &str) -> String {
        format!("/dev/{}/{lv}", self.config.group)
    }

    async fn run(&self, tool: &str, args: &[String]) -> Result<Vec<u8>> {
        let output = tokio::process::Command::new(tool)
            .args(args)
            .output()
            .await
            .map_err(|e| {
                HostError::failed(format!(
                    "running `{tool} {}`: {e}. Is lvm2 installed on this machine?",
                    args.join(" ")
                ))
            })?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(HostError::failed(format!(
            "`{tool} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// Create one logical volume of `gib`, thin if this pool has a thin pool.
    async fn create_lv(&self, lv: &str, gib: u64) -> Result<()> {
        let mut args: Vec<String> = vec!["--yes".into(), "--name".into(), lv.to_string()];
        match &self.config.thin_pool {
            Some(thin) => {
                args.extend([
                    "--virtualsize".into(),
                    format!("{gib}G"),
                    "--thinpool".into(),
                    format!("{}/{thin}", self.config.group),
                ]);
            }
            None => {
                args.extend([
                    "--size".into(),
                    format!("{gib}G"),
                    self.config.group.clone(),
                ]);
            }
        }
        self.run(&self.config.lvcreate, &args).await.map(|_| ())
    }

    /// Write an image or another volume into a device that already exists.
    ///
    /// `qemu-img convert` and not `dd`, because the source may be qcow2 and the
    /// destination is a raw block device: a byte copy of a qcow2 file onto a
    /// disk produces a disk holding a qcow2 file, which boots nothing.
    async fn write_into(&self, from: &str, device: &str) -> Result<()> {
        self.run(
            &self.config.qemu_img,
            &[
                "convert".into(),
                "-O".into(),
                "raw".into(),
                from.to_string(),
                device.to_string(),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `lvs`, as name/size pairs, for whatever prefix is asked for.
    ///
    /// `--units b --nosuffix` so the number is bytes and not a localised
    /// `4.00g`, which is what a report meant for people looks like and what a
    /// parser should never be handed.
    async fn list(&self, prefix: &str) -> Result<BTreeMap<String, u64>> {
        let out = self
            .run(
                &self.config.lvs,
                &[
                    "--noheadings".into(),
                    "--units".into(),
                    "b".into(),
                    "--nosuffix".into(),
                    "-o".into(),
                    "lv_name,lv_size".into(),
                    self.config.group.clone(),
                ],
            )
            .await?;
        let text = String::from_utf8_lossy(&out);
        let mut found = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(name), Some(size)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(rest) = name.strip_prefix(prefix) else {
                continue;
            };
            let bytes: u64 = size.split('.').next().unwrap_or(size).parse().unwrap_or(0);
            // Rounded up: a volume asked for as 10 GiB may sit in an extent
            // slightly larger, and reporting 9 would read as a pool that shrank
            // what somebody asked for.
            let gib = bytes.div_ceil(1 << 30);
            found.insert(rest.replace('-', "/"), gib);
        }
        Ok(found)
    }
}

#[async_trait::async_trait]
impl Storage for LvmPool {
    fn at(&self, volume: &str) -> Option<String> {
        Some(self.device(&lv_name(volume)))
    }

    async fn observe(&self) -> Result<PoolState> {
        let volumes = self.list("velstra-").await?;
        let raw_snapshots = self.list("velstrasnap-").await?;

        let mut snapshots = BTreeMap::new();
        for (name, gib) in raw_snapshots {
            // The source is in a snapshot's identity rather than in a field, so
            // it is read back out of the name — the same rule the object model
            // applies. A volume that is not a snapshot of a volume is not
            // attributed to one.
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
                    gib,
                },
            );
        }

        // `vgs`, for what the group holds in total. A pool that cannot say how
        // large it is cannot be scheduled into.
        let out = self
            .run(
                &self.config.vgs,
                &[
                    "--noheadings".into(),
                    "--units".into(),
                    "b".into(),
                    "--nosuffix".into(),
                    "-o".into(),
                    "vg_size".into(),
                    self.config.group.clone(),
                ],
            )
            .await?;
        let text = String::from_utf8_lossy(&out);
        let capacity_gib = text
            .split_whitespace()
            .next()
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            / (1 << 30);

        Ok(PoolState {
            volumes,
            capacity_gib,
            backend: "lvm".into(),
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
            // The same refusal the directory pool makes, for the same reason:
            // there is no KMS here to turn that name into key material, and a
            // volume made in plaintext and reported Ready would be the platform
            // quietly declining to encrypt something somebody asked it to.
            //
            // LVM *can* do this — `cryptsetup` over the LV — and when there is a
            // KMS this is where it goes.
            return Err(HostError::failed(format!(
                "this volume asks to be encrypted with {key}, which names an entry in the \
                 project's KMS — and there is no KMS for this pool to ask. It is refused rather \
                 than made in plaintext and reported ready."
            )));
        }

        let lv = lv_name(volume);
        let device = self.device(&lv);

        match source {
            Origin::Blank => self.create_lv(&lv, gib).await,
            // Cloning **is** creating it, never a step afterwards: a volume that
            // exists blank for one pass is one a guest can be started from, and
            // that guest finds an empty disk.
            //
            // There is no atomic "create from" for a thick LV, so the volume is
            // removed again if the copy fails — leaving a blank one behind would
            // be exactly the half-made volume this ordering exists to prevent.
            Origin::Image(path) | Origin::File(path) => {
                self.create_lv(&lv, gib).await?;
                match self.write_into(path, &device).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = self.destroy(volume).await;
                        Err(e)
                    }
                }
            }
            Origin::Snapshot(snapshot) => {
                let from = snap_lv_name(snapshot);
                self.create_lv(&lv, gib).await?;
                match self.write_into(&self.device(&from), &device).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = self.destroy(volume).await;
                        Err(e)
                    }
                }
            }
        }
    }

    async fn grow(&self, volume: &str, to_gib: u64) -> Result<()> {
        self.run(
            &self.config.lvextend,
            &[
                "--size".into(),
                format!("{to_gib}G"),
                self.device(&lv_name(volume)),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn destroy(&self, volume: &str) -> Result<()> {
        let device = self.device(&lv_name(volume));
        match self
            .run(&self.config.lvremove, &["--yes".into(), device])
            .await
        {
            Ok(_) => Ok(()),
            // Already gone is the answer, not a failure: a destroy that ran and
            // whose reply was lost must be safe to ask again, or a delete can
            // never finish.
            Err(e) if e.to_string().contains("Failed to find") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn take_snapshot(&self, snapshot: &str, volume: &str) -> Result<()> {
        let mut args: Vec<String> = vec![
            "--yes".into(),
            "--snapshot".into(),
            "--name".into(),
            snap_lv_name(snapshot),
        ];
        // A thick snapshot needs its own space up front, and one that fills is
        // dropped by the kernel — so it is asked for at the size of its source
        // rather than at some fraction somebody guessed. A thin snapshot needs
        // no size at all and costs nothing until something is written.
        if self.config.thin_pool.is_none() {
            let sizes = self.list("velstra-").await?;
            let gib = sizes.get(volume).copied().unwrap_or(1);
            args.extend(["--size".into(), format!("{gib}G")]);
        }
        args.push(self.device(&lv_name(volume)));
        self.run(&self.config.lvcreate, &args).await.map(|_| ())
    }

    async fn destroy_snapshot(&self, snapshot: &str) -> Result<()> {
        let device = self.device(&snap_lv_name(snapshot));
        match self
            .run(&self.config.lvremove, &["--yes".into(), device])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("Failed to find") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn copy_out(&self, volume: &str, path: &str) -> Result<u64> {
        let from = self.device(&lv_name(volume));
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                HostError::failed(format!("{} is not usable: {e}", parent.display()))
            })?;
        }
        // Out as qcow2, not raw: a backup of a 500 GiB volume holding 4 GiB of
        // data should cost 4 GiB on the target, and a raw copy of a block device
        // costs all of it.
        self.run(
            &self.config.qemu_img,
            &[
                "convert".into(),
                "-O".into(),
                "qcow2".into(),
                from,
                path.to_string(),
            ],
        )
        .await?;
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| HostError::failed(format!("reading back {path}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_name_becomes_a_name_lvm_will_take() {
        // LVM's charset is `[a-zA-Z0-9+_.-]`: no `/`, and — unlike every other
        // spelling in this codebase — no `~` either.
        let lv = lv_name("projects/p1/volumes/data");
        assert_eq!(lv, "velstra-projects-p1-volumes-data");
        assert!(
            lv.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+_.-".contains(c)),
            "{lv} holds something lvm will refuse"
        );
    }

    #[test]
    fn a_snapshot_is_told_apart_from_a_volume_by_its_prefix() {
        // Not by parsing: `velstra-projects-p1-volumes-data-nightly` could be a
        // volume called `…/data-nightly`. The prefix is what makes a listing
        // readable without guessing, which is why there are two.
        assert!(lv_name("projects/p1/volumes/data").starts_with("velstra-"));
        assert!(
            snap_lv_name("projects/p1/volumes/data/snapshots/nightly").starts_with("velstrasnap-")
        );
        assert!(!snap_lv_name("x").starts_with("velstra-"));
    }
}
