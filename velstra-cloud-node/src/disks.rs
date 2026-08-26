//! Disk discovery and install planning — the *pure* half of the installer.
//!
//! Everything here is either a parser over captured tool output or arithmetic
//! over the result, so it is unit-tested without touching real hardware.
//! Executing a plan (the destructive writes) lives in [`crate::install`], gated
//! behind an explicit confirmation.

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::product;

/// A candidate target block device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Kernel name, e.g. `sda`, `nvme0n1`.
    pub name: String,
    /// Capacity in bytes.
    pub size: u64,
    /// Model string (may be empty).
    pub model: String,
    /// Whether the kernel marks it removable (a USB stick — usually the
    /// installer medium, not a target).
    pub removable: bool,
}

impl Disk {
    /// The `/dev` path of the whole disk.
    pub fn dev_path(&self) -> String {
        format!("/dev/{}", self.name)
    }
}

/// A RAID level for the data array. The dm-verity store is read-only and
/// identical on every member, so only the writable data partition is arrayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raid {
    /// No array — a single target disk.
    None,
    /// Stripe across 2+ disks: capacity sum, NO redundancy (RAID0).
    Stripe,
    /// Mirror across 2+ disks (survives losing all but one) (RAID1).
    Mirror,
    /// Striped mirror across 4+ disks: capacity + redundancy (RAID10).
    Mirror10,
}

impl Raid {
    /// The minimum number of member disks this level needs.
    pub fn min_disks(self) -> usize {
        match self {
            Raid::None => 1,
            Raid::Stripe | Raid::Mirror => 2,
            Raid::Mirror10 => 4,
        }
    }

    /// The `mdadm --level` value, if this level uses an array.
    pub fn mdadm_level(self) -> Option<&'static str> {
        match self {
            Raid::None => None,
            Raid::Stripe => Some("0"),
            Raid::Mirror => Some("1"),
            Raid::Mirror10 => Some("10"),
        }
    }
}

/// Discover whole disks that could be install targets. Reads `lsblk`
/// (JSON-free, stable columns): name, size in bytes, type, removable flag,
/// model. Only `type == "disk"` entries are returned.
pub fn discover_disks() -> Result<Vec<Disk>> {
    let out = Command::new("lsblk")
        .args(["-dnb", "-o", "NAME,SIZE,TYPE,RM,MODEL"])
        .output()
        .context("running lsblk — is util-linux on the PATH?")?;
    if !out.status.success() {
        bail!("lsblk failed (exit {:?})", out.status.code());
    }
    Ok(parse_lsblk(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `lsblk -dnb -o NAME,SIZE,TYPE,RM,MODEL` output into disks. Kept pure
/// for testing. Lines that aren't `type == disk` (partitions, loop, rom) are
/// skipped; the model is whatever remains after the first four
/// whitespace-split fields.
fn parse_lsblk(text: &str) -> Vec<Disk> {
    let mut disks = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // NAME SIZE TYPE RM [MODEL...] — columns are space-padded, so collapse
        // runs of whitespace; the model is whatever remains after the 4 fixed
        // fields (and may itself contain spaces).
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [name, size, kind, rm, model @ ..] = parts.as_slice() else {
            continue;
        };
        // Only real disks: lsblk types out loop/rom, but zram (compressed RAM
        // swap) and md/dm virtual devices still report as "disk" — never
        // install targets.
        if *kind != "disk"
            || name.starts_with("zram")
            || name.starts_with("md")
            || name.starts_with("dm-")
        {
            continue;
        }
        let Ok(size) = size.parse::<u64>() else {
            continue;
        };
        disks.push(Disk {
            name: (*name).to_string(),
            size,
            model: model.join(" "),
            removable: *rm == "1",
        });
    }
    disks
}

/// Render a byte count as a short human-readable size (binary units).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The number of bytes a target must have for the source layout to be written
/// onto it: the end of the source's last partition, plus the 33 sectors the
/// backup GPT header occupies at the very end of the disk.
///
/// Parsed from `sgdisk -p` output — pure, so the arithmetic is testable
/// without a disk. Returns `None` if the listing carries no partition rows.
pub(crate) fn parse_layout_bytes(sgdisk_print: &str) -> Option<u64> {
    let sector = sgdisk_print
        .lines()
        .find_map(|l| l.split_once("Sector size (logical/physical):"))
        .and_then(|(_, rest)| rest.trim().split(['/', ' ']).next()?.parse::<u64>().ok())
        .unwrap_or(512);
    // Partition rows begin with the partition number; every other line either
    // has a non-numeric first field or too few fields to be one.
    let last_end = sgdisk_print
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let [num, _start, end, ..] = f.as_slice() else {
                return None;
            };
            num.parse::<u32>().ok()?;
            end.parse::<u64>().ok()
        })
        .max()?;
    // The backup GPT sits in the last 33 sectors; `sgdisk --move-second-header`
    // has to have somewhere to put it.
    Some((last_end + 1 + 33) * sector)
}

/// The partition device path for partition `n` on `disk` — `nvme0n1` →
/// `nvme0n1p2`, `sda`/`vda` → `sda2`. (A trailing digit needs the `p`.)
pub fn part_path(disk: &str, n: u32) -> String {
    let bare = disk.trim_start_matches("/dev/");
    let sep = if bare.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    format!("/dev/{bare}{sep}{n}")
}

/// Validate a target selection against a RAID level: enough disks, each big
/// enough, none removable. Returns the chosen disks in order, or an error
/// describing what's wrong. Pure — the caller has already discovered the
/// disks.
pub fn plan_targets<'a>(
    available: &'a [Disk],
    targets: &[String],
    raid: Raid,
) -> Result<Vec<&'a Disk>> {
    if targets.len() < raid.min_disks() {
        bail!(
            "{:?} needs at least {} disk(s), got {}",
            raid,
            raid.min_disks(),
            targets.len()
        );
    }
    if raid == Raid::None && targets.len() != 1 {
        bail!("a non-RAID install takes exactly one target disk");
    }
    let mut chosen = Vec::new();
    for t in targets {
        let want = t.trim_start_matches("/dev/");
        let disk = available
            .iter()
            .find(|d| d.name == want)
            .ok_or_else(|| anyhow::anyhow!("no such disk {t:?}"))?;
        if disk.size < product::MIN_TARGET_BYTES {
            bail!(
                "disk {} is {} — below the {} minimum",
                disk.dev_path(),
                human_size(disk.size),
                human_size(product::MIN_TARGET_BYTES)
            );
        }
        if disk.removable {
            bail!(
                "disk {} is removable (the installer medium?) — refusing to install \
                 onto it. There is no override; install to a non-removable disk.",
                disk.dev_path()
            );
        }
        chosen.push(disk);
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, gib: u64, removable: bool) -> Disk {
        Disk {
            name: name.into(),
            size: gib * 1024 * 1024 * 1024,
            model: String::new(),
            removable,
        }
    }

    #[test]
    fn parses_lsblk_disks_and_skips_non_disks() {
        let text = "\
sda    500107862016 disk 0 Samsung SSD 860
sda1     1048576000 part 0
nvme0n1 1000204886016 disk 0 WD_BLACK SN770
sdb       8000000000 disk 1 SanDisk Cruzer
zram0     4294967296 disk 0
sr0       1073741824 rom  1
";
        let disks = parse_lsblk(text);
        assert_eq!(disks.len(), 3);
        assert_eq!(disks[0].name, "sda");
        assert_eq!(disks[0].model, "Samsung SSD 860");
        assert!(!disks[0].removable);
        assert_eq!(disks[1].name, "nvme0n1");
        assert_eq!(disks[2].name, "sdb");
        assert!(disks[2].removable, "RM=1 → removable");
    }

    #[test]
    fn human_size_is_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(500_107_862_016), "465.8 GiB");
    }

    #[test]
    fn part_path_handles_nvme_and_sata() {
        assert_eq!(part_path("/dev/sda", 1), "/dev/sda1");
        assert_eq!(part_path("vdb", 4), "/dev/vdb4");
        assert_eq!(part_path("/dev/nvme0n1", 3), "/dev/nvme0n1p3");
        assert_eq!(part_path("/dev/mmcblk0", 2), "/dev/mmcblk0p2");
    }

    #[test]
    fn raid_levels_map_to_mdadm() {
        assert_eq!(Raid::None.mdadm_level(), None);
        assert_eq!(Raid::Stripe.mdadm_level(), Some("0"));
        assert_eq!(Raid::Mirror.mdadm_level(), Some("1"));
        assert_eq!(Raid::Mirror10.mdadm_level(), Some("10"));
        assert_eq!(Raid::Stripe.min_disks(), 2);
        assert_eq!(Raid::Mirror10.min_disks(), 4);
    }

    #[test]
    fn plan_targets_enforces_count_size_and_removable() {
        let avail = vec![
            disk("sda", 500, false),
            disk("sdb", 500, false),
            disk("usb0", 16, true),
            disk("tiny", 2, false),
        ];
        // Single-disk happy path.
        let p = plan_targets(&avail, &["/dev/sda".into()], Raid::None).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "sda");
        // Mirror needs 2.
        assert!(plan_targets(&avail, &["sda".into()], Raid::Mirror).is_err());
        let p = plan_targets(&avail, &["sda".into(), "sdb".into()], Raid::Mirror).unwrap();
        assert_eq!(p.len(), 2);
        // Too small / removable / unknown are rejected.
        assert!(plan_targets(&avail, &["tiny".into()], Raid::None).is_err());
        assert!(plan_targets(&avail, &["usb0".into()], Raid::None).is_err());
        assert!(plan_targets(&avail, &["nope".into()], Raid::None).is_err());
        // Non-RAID rejects multiple targets.
        assert!(plan_targets(&avail, &["sda".into(), "sdb".into()], Raid::None).is_err());
        // RAID10 needs 4.
        assert!(plan_targets(&avail, &["sda".into(), "sdb".into()], Raid::Mirror10).is_err());
    }

    #[test]
    fn layout_bytes_measures_the_end_of_the_last_partition() {
        // Abridged `sgdisk -p` of a node medium: an ESP, slot A, and the data
        // partition last.
        let printed = "\
Disk /dev/vda: 12582912 sectors, 6.0 GiB
Sector size (logical/physical): 512/512 bytes
Disk identifier (GUID): 2E4C1F0A-0000-4000-8000-000000000001
Partition table holds up to 128 entries
First usable sector is 34, last usable sector is 12582878

Number  Start (sector)    End (sector)  Size       Code  Name
   1            2048          264191   128.0 MiB   EF00  esp
   2          264192          657407   192.0 MiB   8300  store-verity-a
   3          657408         5900799   2.5 GiB     8300  store-a
   6        11534336        11796479   128.0 MiB   8300  data
";
        // The last partition ends at 11796479, and the backup GPT needs the 33
        // sectors after it.
        let want = (11_796_479u64 + 1 + 33) * 512;
        assert_eq!(parse_layout_bytes(printed), Some(want));
        // Which is more than the hand-kept floor — the very reason the floor
        // is not the check that guards the erase.
        assert!(want > product::MIN_TARGET_BYTES);
    }

    #[test]
    fn layout_bytes_ignores_a_listing_without_partitions() {
        assert_eq!(
            parse_layout_bytes("Disk /dev/vdb: 100 sectors\nNumber  Start (sector)\n"),
            None
        );
    }

    #[test]
    fn layout_bytes_honours_a_4k_sector_size() {
        let printed = "\
Sector size (logical/physical): 4096/4096 bytes
Number  Start (sector)    End (sector)  Size       Code  Name
   1            2048          264191   1.0 GiB     EF00  esp
";
        assert_eq!(parse_layout_bytes(printed), Some((264_191u64 + 34) * 4096));
    }
}
