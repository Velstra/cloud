//! How this machine was installed, read off the machine.
//!
//! The reading half of [`velstra_cloud_model::installed`]: this runs `lsblk`
//! and `dpkg-query` and reads three files, and hands the facts to `detect`,
//! which draws the conclusion. Nothing here decides anything — the same split
//! as [`crate::pcidev`] and [`crate::hostcpu`], and for the same reason: a
//! conclusion drawn in one tested function cannot disagree with itself across
//! two machines.
//!
//! Every read is allowed to fail. A machine with no `lsblk` on the path, no
//! `dpkg`, no `/etc/os-release`, reports what it could see and the model
//! reads the rest as unknown — which is the honest answer, and one the
//! console shows as unknown rather than as a kind somebody guessed.

use std::{path::Path, process::Command};

use velstra_cloud_model::installed::{Installed, Observed, detect};

/// The GPT type GUID of the sealed store's partitions — the A/B slots the node
/// image lays down. Spelled here because this crate does not depend on the
/// installer, and pinned by a test against the installer's own constant so
/// the two cannot drift.
pub const USR_TYPE: &str = "8484680c-9521-48c6-9c11-b0720656f69e";

/// The partition numbers of the two slots' stores, as the installer lays
/// them down (`SLOT_A.store_part`, `SLOT_B.store_part` in the same file the
/// GUID comes from). Pinned by the same test.
const SLOT_A_STORE_PART: u32 = 3;
const SLOT_B_STORE_PART: u32 = 5;

/// Read the facts and draw the conclusion.
pub fn observe() -> Installed {
    detect(&observe_at(Path::new("/")))
}

/// The facts, rooted somewhere — `/` in production, a directory a test built.
fn observe_at(root: &Path) -> Observed {
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    Observed {
        nixos_marker: root.join("etc/NIXOS").exists(),
        has_slots: has_slot_partitions(),
        package_version: package_version(),
        image_version: read("etc/velstra-release"),
        pretty_name: read("etc/os-release").and_then(|t| pretty_name(&t)),
        active_slot: active_slot(),
    }
}

/// Whether any partition on this machine is of the sealed store's type.
///
/// `lsblk -rno PARTTYPE` lists every partition's type GUID, one per line,
/// lowercase. Any match means the disk was laid down by the image — the
/// slots are the one thing no other installation shape puts there.
fn has_slot_partitions() -> bool {
    let Ok(out) = Command::new("lsblk").args(["-rno", "PARTTYPE"]).output() else {
        return false;
    };
    slots_in(&String::from_utf8_lossy(&out.stdout))
}

fn slots_in(lsblk: &str) -> bool {
    lsblk
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case(USR_TYPE))
}

/// Which slot's store backs the running `/dev/mapper/usr`, if any does.
///
/// The walk the A/B writer makes before it writes: `lsblk -s` from the
/// verity device down to the partition under it, whose number says which
/// slot it is. A machine with no verity device — every machine that is not
/// the image — has no answer, and reports none.
fn active_slot() -> Option<String> {
    let out = Command::new("lsblk")
        .args(["-nsro", "NAME,TYPE", "/dev/mapper/usr"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    slot_from_lsblk(&String::from_utf8_lossy(&out.stdout))
}

/// The first partition on the walk, named as a slot. Pure, so the walk is
/// testable with captured output.
fn slot_from_lsblk(walk: &str) -> Option<String> {
    let number = walk.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (name, kind) = (fields.next()?, fields.next()?);
        if kind != "part" {
            return None;
        }
        // `sda3`, `nvme0n1p3`, `vda3`: the number is the digits at the end,
        // whatever the disk is called.
        let start = name
            .rfind(|c: char| !c.is_ascii_digit())
            .map_or(0, |i| i + 1);
        name[start..].parse::<u32>().ok()
    })?;
    match number {
        SLOT_A_STORE_PART => Some("a".to_string()),
        SLOT_B_STORE_PART => Some("b".to_string()),
        _ => None,
    }
}

/// What dpkg says the package's version is, if the package is installed.
fn package_version() -> Option<String> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f", "${Version}", "velstra-cloud"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// `PRETTY_NAME` out of `os-release`, unquoted.
fn pretty_name(os_release: &str) -> Option<String> {
    os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One GUID, matched however `lsblk` cases it, among whatever else is on
    /// the disk.
    #[test]
    fn the_slot_type_is_found_among_other_partitions() {
        let listing = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b\n\
                       8484680C-9521-48C6-9C11-B0720656F69E\n\
                       77ff5f63-e7b6-4633-acf4-1565b864c0e6\n\
                       0fc63daf-8483-4772-8e79-3d69d8477de4\n";
        assert!(slots_in(listing));
        assert!(!slots_in("0fc63daf-8483-4772-8e79-3d69d8477de4\n\n"));
        assert!(!slots_in(""));
    }

    /// The constants here are the installer's constants. Two spellings of a
    /// GUID, or two ideas of which partition is slot A, would be a machine
    /// that is an appliance to one crate and not the other.
    #[test]
    fn the_slot_guid_and_numbers_are_the_installers() {
        // `product.rs` writes it uppercase; `lsblk` prints lowercase; the
        // comparison ignores case, and the bytes have to be the same.
        assert_eq!(
            USR_TYPE.to_uppercase(),
            "8484680C-9521-48C6-9C11-B0720656F69E",
            "keep this in step with velstra-cloud-node/src/product.rs USR_TYPE"
        );
        assert_eq!(
            (SLOT_A_STORE_PART, SLOT_B_STORE_PART),
            (3, 5),
            "keep these in step with velstra-cloud-node/src/product.rs SLOT_A/SLOT_B"
        );
    }

    /// The walk down from the verity device names the slot by the partition
    /// under it, whatever the disk is called — and nothing else on the walk
    /// is mistaken for it.
    #[test]
    fn the_active_slot_is_the_partition_under_the_verity_device() {
        let a = "usr dm\nnvme0n1p3 part\nnvme0n1 disk\n";
        assert_eq!(slot_from_lsblk(a).as_deref(), Some("a"));
        let b = "usr dm\nsda5 part\nsda disk\n";
        assert_eq!(slot_from_lsblk(b).as_deref(), Some("b"));
        // Some other partition backing it is not a slot, and is not guessed.
        assert_eq!(slot_from_lsblk("usr dm\nvda7 part\nvda disk\n"), None);
        // No verity device at all: `lsblk` prints nothing.
        assert_eq!(slot_from_lsblk(""), None);
    }

    #[test]
    fn pretty_name_is_read_unquoted() {
        let os = "NAME=\"Debian GNU/Linux\"\nVERSION_ID=\"13\"\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";
        assert_eq!(
            pretty_name(os).as_deref(),
            Some("Debian GNU/Linux 13 (trixie)")
        );
        assert_eq!(pretty_name("NAME=x\n"), None);
    }

    /// The reading half, against a directory a test built: marker present,
    /// stamp present. `lsblk` and `dpkg` are not run against a fake root, so
    /// the slot and package facts are whatever this machine says — the test
    /// asserts only what the files decide.
    #[test]
    fn the_files_are_read_from_the_root_given() {
        // A directory of this test's own, without a crate for it: the
        // process id keeps two test binaries apart, and it is removed at the
        // end whatever the assertions did.
        let dir = std::env::temp_dir().join(format!("velstra-installed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::write(dir.join("etc/NIXOS"), "").unwrap();
        std::fs::write(dir.join("etc/velstra-release"), "0.2.0+x\n").unwrap();
        std::fs::write(dir.join("etc/os-release"), "PRETTY_NAME=\"NixOS 25.11\"\n").unwrap();
        let seen = observe_at(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.nixos_marker);
        assert_eq!(seen.image_version.as_deref(), Some("0.2.0+x\n"));
        assert_eq!(seen.pretty_name.as_deref(), Some("NixOS 25.11"));
    }
}
