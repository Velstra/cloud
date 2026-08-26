//! Every constant that makes this the *node* product rather than the Sentinel
//! firewall it was ported from.
//!
//! The disk layout is identical — both images come out of the same Nix image
//! factory — but the names on it are not, and the names are load-bearing: the
//! boot entries, the mdadm array, the seed files. They live here, in one
//! module, so a rename is one edit and not a hunt. **The Nix module in the
//! flake must agree with every value in this file** — the slot names name the
//! UKI files the image ships, the mount unit expects the data partition at
//! `/var/lib/velstra`, and the unlock service prints [`UNLOCK_PROMPT`]. A
//! disagreement between the two is a node that installs and then does not boot,
//! so treat this file and the flake's node module as two halves of one contract.

use std::ops::RangeInclusive;

/// A store slot: its verity-hash + store partition numbers, and the
/// systemd-boot entry name its UKI is filed under in `/EFI/Linux`.
///
/// One definition for the installer and the updater, so the two can never
/// disagree about which partition is which.
pub struct Slot {
    pub name: &'static str,
    pub verity_part: u32,
    pub store_part: u32,
}

pub const SLOT_A: Slot = Slot {
    name: "velstra-node-a",
    verity_part: 2,
    store_part: 3,
};
pub const SLOT_B: Slot = Slot {
    name: "velstra-node-b",
    verity_part: 4,
    store_part: 5,
};

/// The A/B GPT layout: 1=ESP, 2=store-verity-A, 3=store-A, 4=store-verity-B,
/// 5=store-B, 6=data. Data is last so it can grow to fill the disk.
pub const DATA_PART: u32 = 6;

/// Partitions cloned block-for-block on install: the ESP and slot A's verity
/// hash + store. Slot B (4, 5) is reserved empty space — its GPT entries are
/// cloned by `sgdisk --replicate`, and a later `velstra-cloud-node update`
/// fills it.
pub const SEALED_PARTS: RangeInclusive<u32> = 1..=3;

/// systemd GPT type GUIDs for the verity store pair (x86-64) — used to re-type
/// slot B (reserved generic at build) once it holds a real verity image. The
/// same GUIDs Sentinel uses, because both images come from the same factory.
pub const USR_TYPE: &str = "8484680C-9521-48C6-9C11-B0720656F69E";
pub const USR_VERITY_TYPE: &str = "77FF5F63-E7B6-4633-ACF4-1565B864C0E6";

/// The mdadm array a RAID install assembles the data partitions into.
pub const MD_ARRAY: &str = "/dev/md/velstra-node-data";

/// The device-mapper name the unlocked data volume is opened as, both at
/// install time and at boot (`velstra-cloud-node unlock`). The ext4 filesystem
/// — LABEL=data — lives *inside* it, so the image's `/dev/disk/by-label/data`
/// mount finds it unchanged once the volume is open.
pub const DATA_MAPPER: &str = "data";

/// The `/dev/mapper` path of the unlocked data volume.
pub fn data_mapper_path() -> String {
    format!("/dev/mapper/{DATA_MAPPER}")
}

/// A raw node image to install from, when the booted medium is not the source
/// (the ISO sets this). The `--source` flag overrides it.
pub const INSTALL_SOURCE_ENV: &str = "VELSTRA_NODE_INSTALL_SOURCE";

/// A scripted install's LUKS passphrase. Read once, never echoed, never on
/// argv; absent, the wizard prompts twice.
pub const LUKS_PASSPHRASE_ENV: &str = "VELSTRA_NODE_LUKS_PASSPHRASE";

/// Where the freshly made data filesystem is mounted while the seed files are
/// written. On the running node that partition is mounted at
/// `/var/lib/velstra`, so the seed lands at the partition ROOT — a nested
/// directory here would put the answers where nothing looks.
pub const SEED_MOUNTPOINT: &str = "/run/velstra-node-seed";

/// What `systemd-ask-password` shows when the boot-time unlock prompts.
pub const UNLOCK_PROMPT: &str = "Unlock the Velstra node data volume:";

/// A floor below which no build of the node image could ever fit — enough to
/// reject an obviously wrong disk without reading anything. Deliberately NOT
/// the real requirement: that is the source medium's own layout, measured from
/// the source by the installer before a single byte is erased. Same rule as
/// Sentinel, same reasoning: the floor is coarse and conservative so it never
/// rejects a disk the real per-source check would have accepted.
pub const MIN_TARGET_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Wizard defaults. Stated here rather than inline so the review summary, the
/// prompts and the docs cannot drift apart.
pub const DEFAULT_HOSTNAME: &str = "velstra-node";
pub const DEFAULT_CELL: &str = "cell-1";
pub const DEFAULT_REGION: &str = "eu-central";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slots_are_distinct_and_map_to_the_fixed_layout() {
        assert_ne!(SLOT_A.name, SLOT_B.name);
        assert_eq!((SLOT_A.verity_part, SLOT_A.store_part), (2, 3));
        assert_eq!((SLOT_B.verity_part, SLOT_B.store_part), (4, 5));
        assert!(SLOT_A.name.starts_with("velstra-node-"));
        assert!(SLOT_B.name.starts_with("velstra-node-"));
    }

    #[test]
    fn the_unlocked_data_volume_has_a_stable_name() {
        assert_eq!(data_mapper_path(), "/dev/mapper/data");
        assert_eq!(DATA_MAPPER, "data");
    }
}
