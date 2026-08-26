//! A/B updates: write a new image into the INACTIVE slot, then make
//! systemd-boot try it next. It rolls back to the current slot if the new one
//! fails 3 boots, which is why an update never touches the running slot.
//!
//! Local-file only, for now. A signed update channel — a release manifest, an
//! Ed25519 signature over it, a subscription key, and a fetch whose image is
//! verified against the manifest's SHA-256 before it ever reaches the
//! slot-writer — is a documented seam, not an accident of omission: Sentinel's
//! `src/update.rs` is the pattern to port when the node grows one, and the
//! hook-in point is an `update_from_channel()` that calls [`run_update`] only after
//! both signature and digest checks have passed, so the channel path stays
//! fail-closed by construction.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    disks::part_path,
    install::{LoopGuard, MountGuard, find_source_disk, losetup_attach, require_root, run},
    product::{SLOT_A, SLOT_B, Slot, USR_TYPE, USR_VERITY_TYPE},
};

/// `velstra-cloud-node update --image <path>`: write `image`'s sealed store
/// into the inactive slot and switch the boot default to it. `image` may be a
/// raw image file (loop-mounted) or a block device (used directly).
/// DESTRUCTIVE to the inactive slot only; the running slot is untouched.
pub fn run_update(image: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    require_root("update")?;
    let disk = find_source_disk()?;
    let active = active_slot(&disk)?;
    let inactive = inactive_of(active);
    eprintln!(
        "active slot: {} — updating inactive slot {} on {disk}",
        active.name, inactive.name
    );

    // Resolve the source to a partitioned block device.
    let is_block = std::fs::metadata(image)
        .map(|m| m.file_type().is_block_device())
        .unwrap_or(false);
    let (srcdev, _loop) = if is_block {
        (image.to_string_lossy().into_owned(), None)
    } else {
        let dev = losetup_attach(image)?;
        (dev.clone(), Some(LoopGuard(dev)))
    };

    // The *identity* of the slot, not only its contents, and it is checked
    // BEFORE any destructive write. systemd finds a verity pair by partition
    // GUID — the two halves of the image's root hash — so a slot holding the
    // new image while keeping the old slot's randomly assigned GUIDs is a slot
    // the initrd cannot find. It then waits for a device that will never
    // appear: the machine hangs rather than failing, so it never resets, so
    // the boot counter never decrements and the automatic rollback never
    // fires. That is exactly how an updated Sentinel went dark on the bench.
    //
    // The same mechanism bites the *identical-image* case: if this image is
    // byte-for-byte the one already running, its slot-A verity/store GUIDs ARE
    // the active slot's GUIDs, and stamping them onto the inactive slot leaves
    // two partitions sharing one PARTUUID — which the initrd cannot tell
    // apart, so it hangs exactly as above. The GUIDs cannot be randomised away
    // (verity derives them from the root hash the UKI expects), so the only
    // safe answer is to refuse up front, before the inactive slot is touched.
    let src_verity = partition_guid(&srcdev, SLOT_A.verity_part)?;
    let src_store = partition_guid(&srcdev, SLOT_A.store_part)?;
    let active_verity = partition_guid(&disk, active.verity_part)?;
    let active_store = partition_guid(&disk, active.store_part)?;
    if src_verity == active_verity || src_store == active_store {
        bail!(
            "this image is identical to the running slot {active} (its verity/store \
             partitions carry the same GUIDs): writing it to slot {inactive} would \
             leave two partitions with the same PARTUUID, which the initrd cannot \
             tell apart — the node would hang at boot instead of auto-rolling-back. \
             Nothing was changed. Update to a freshly built image, which carries its \
             own verity GUIDs.",
            active = active.name,
            inactive = inactive.name,
        );
    }

    // Clone the source's slot-A store + verity hash into our inactive slot.
    eprintln!("writing slot {} from {srcdev} …", inactive.name);
    let clone = |from: u32, to: u32| -> Result<()> {
        run(
            "dd",
            &[
                &format!("if={}", part_path(&srcdev, from)),
                &format!("of={}", part_path(&disk, to)),
                "bs=4M",
                "conv=fsync",
            ],
        )
    };
    clone(SLOT_A.verity_part, inactive.verity_part)?;
    clone(SLOT_A.store_part, inactive.store_part)?;
    // Re-type the inactive (reserved-generic) partitions to the verity GUIDs
    // so the initrd's veritysetup considers them, and give them the source's
    // unique GUIDs so it can identify which image they hold.
    run(
        "sgdisk",
        &[
            &format!("--typecode={}:{USR_VERITY_TYPE}", inactive.verity_part),
            &format!("--typecode={}:{USR_TYPE}", inactive.store_part),
            &format!("--partition-guid={}:{src_verity}", inactive.verity_part),
            &format!("--partition-guid={}:{src_store}", inactive.store_part),
            &disk,
        ],
    )?;
    run("partprobe", &[&disk]).ok();
    run("udevadm", &["settle"]).ok();

    switch_boot(&disk, &srcdev, active, inactive)?;
    eprintln!(
        "update complete — reboot to switch to slot {} (a failed boot rolls back to {} after 3 tries)",
        inactive.name, active.name
    );
    Ok(())
}

/// The other slot.
fn inactive_of(active: &'static Slot) -> &'static Slot {
    if active.name == SLOT_A.name {
        &SLOT_B
    } else {
        &SLOT_A
    }
}

/// The slot whose store currently backs the running `/dev/mapper/usr`.
fn active_slot(disk: &str) -> Result<&'static Slot> {
    let bare = disk.trim_start_matches("/dev/");
    let out = Command::new("lsblk")
        .args(["-nsro", "NAME,TYPE", "/dev/mapper/usr"])
        .output()
        .context("inspecting the active verity device")?;
    slot_from_lsblk(&String::from_utf8_lossy(&out.stdout), bare)
        .ok_or_else(|| anyhow::anyhow!("could not determine the active slot from /dev/mapper/usr"))
}

/// Map `lsblk -nsro NAME,TYPE /dev/mapper/usr` output to the slot whose store
/// partition backs it. Pure, so the walk is testable with captured output.
fn slot_from_lsblk(text: &str, bare_disk: &str) -> Option<&'static Slot> {
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (name, kind) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        if kind != "part" {
            continue;
        }
        if let Some(rest) = name.strip_prefix(bare_disk) {
            let num: u32 = rest.trim_start_matches('p').parse().unwrap_or(0);
            if num == SLOT_A.store_part {
                return Some(&SLOT_A);
            }
            if num == SLOT_B.store_part {
                return Some(&SLOT_B);
            }
        }
    }
    None
}

/// The GPT unique GUID of one partition, as `sgdisk` reports it.
///
/// Needed because a verity slot is not identified by where it sits on the
/// disk but by this GUID: systemd derives the pair it looks for from the
/// image's root hash, so the partitions carrying an image must carry the
/// GUIDs that hash produces. Cloning the bytes without them yields a slot
/// whose contents are right and which nothing can find.
fn partition_guid(disk: &str, part: u32) -> Result<String> {
    let out = Command::new("sgdisk")
        .args([&format!("-i{part}"), disk])
        .output()
        .with_context(|| format!("reading the GUID of {disk} partition {part}"))?;
    parse_partition_guid(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| anyhow::anyhow!("no unique GUID for {disk} partition {part}"))
}

/// Pull the unique GUID out of `sgdisk -i` output. Pure for testing.
fn parse_partition_guid(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.split_once("Partition unique GUID: "))
        .map(|(_, guid)| guid.trim().to_string())
}

/// The mountpoint of a block device, if it's currently mounted.
fn mountpoint_of(dev: &str) -> Option<String> {
    let out = Command::new("findmnt")
        .args(["-nro", "TARGET", "-S", dev])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Install the source image's UKI onto the running ESP as the inactive slot's
/// boot-counted entry (`<slot>+3.efi`) and point loader.conf's default at it.
fn switch_boot(disk: &str, srcdev: &str, active: &Slot, inactive: &Slot) -> Result<()> {
    // The running ESP is already mounted (systemd-gpt-auto puts it on /boot,
    // read-only). Reuse that mountpoint and flip it writable rather than
    // mounting the device a second time. If somehow unmounted, mount it
    // ourselves.
    let disk_esp = part_path(disk, 1);
    let (dst, _dg): (std::path::PathBuf, Option<MountGuard>) = match mountpoint_of(&disk_esp) {
        Some(mp) => {
            run("mount", &["-o", "remount,rw", &mp])?;
            (std::path::PathBuf::from(mp), None)
        }
        None => {
            let p = std::path::PathBuf::from("/run/velstra-node/upd-dst");
            std::fs::create_dir_all(&p)?;
            run("mount", &[&disk_esp, p.to_str().unwrap()])?;
            (p.clone(), Some(MountGuard(p)))
        }
    };

    // Source ESP: a self-reseal from the same disk shares the dest ESP; a
    // separate image gets its ESP mounted read-only at a temp path.
    let (src, _sg): (std::path::PathBuf, Option<MountGuard>) = if srcdev == disk {
        (dst.clone(), None)
    } else {
        let p = std::path::PathBuf::from("/run/velstra-node/upd-src");
        std::fs::create_dir_all(&p)?;
        run(
            "mount",
            &["-o", "ro", &part_path(srcdev, 1), p.to_str().unwrap()],
        )?;
        (p.clone(), Some(MountGuard(p)))
    };

    // The source UKI: for a self-reseal pick the active slot's entry; for a
    // separate image there's exactly one UKI in its /EFI/Linux.
    let want = if srcdev == disk {
        Some(active.name)
    } else {
        None
    };
    let uki = std::fs::read_dir(src.join("EFI/Linux"))?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().is_some_and(|x| x == "efi")
                && want.is_none_or(|n| {
                    p.file_name()
                        .is_some_and(|f| f.to_string_lossy().starts_with(n))
                })
        })
        .ok_or_else(|| anyhow::anyhow!("no UKI in the source ESP"))?;

    let lin = dst.join("EFI/Linux");
    std::fs::create_dir_all(&lin)?;
    // Replace any prior entry for this slot, then install the new one with 3
    // boot tries.
    for e in std::fs::read_dir(&lin)?.flatten() {
        if e.file_name().to_string_lossy().starts_with(inactive.name) {
            std::fs::remove_file(e.path())?;
        }
    }
    std::fs::copy(&uki, lin.join(format!("{}+3.efi", inactive.name)))?;

    // Point the boot default at the new slot.
    let conf = dst.join("loader/loader.conf");
    let current = std::fs::read_to_string(&conf).unwrap_or_default();
    std::fs::write(&conf, rewrite_loader_default(&current, inactive.name))
        .with_context(|| format!("rewriting {}", conf.display()))?;
    Ok(())
}

/// Rewrite loader.conf so `default` names the given slot — as a glob, so it
/// keeps matching after a successful boot blesses the entry and strips the
/// `+N` counter from its file name. Pure for testing; every other line is
/// kept verbatim, and a conf with no `default` line gains one.
fn rewrite_loader_default(current: &str, slot_name: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for line in current.lines() {
        if line.trim_start().starts_with("default") {
            out.push_str(&format!("default {slot_name}*\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("default {slot_name}*\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_slot_is_read_from_the_lsblk_walk() {
        // Slot A active: /dev/mapper/usr sits on partition 3.
        let a = "usr  dm\nvda3 part\nvda  disk\n";
        assert_eq!(slot_from_lsblk(a, "vda").unwrap().name, "velstra-node-a");
        // Slot B active: partition 5 — including the nvme p-separator form.
        let b = "usr     dm\nnvme0n1p5 part\nnvme0n1 disk\n";
        assert_eq!(
            slot_from_lsblk(b, "nvme0n1").unwrap().name,
            "velstra-node-b"
        );
        // A walk that never crosses a store partition maps to nothing.
        let none = "usr  dm\nvda6 part\nvda  disk\n";
        assert!(slot_from_lsblk(none, "vda").is_none());
        // A partition on some OTHER disk must not be mistaken for ours.
        let other = "usr  dm\nsdb3 part\nsdb  disk\n";
        assert!(slot_from_lsblk(other, "vda").is_none());
    }

    #[test]
    fn the_inactive_slot_is_always_the_other_one() {
        assert_eq!(inactive_of(&SLOT_A).name, SLOT_B.name);
        assert_eq!(inactive_of(&SLOT_B).name, SLOT_A.name);
    }

    #[test]
    fn partition_guids_are_parsed_from_sgdisk_i() {
        let printed = "\
Partition GUID code: 8484680C-9521-48C6-9C11-B0720656F69E (unknown)
Partition unique GUID: 21A0DF77-1234-4321-9ABC-1D0BB8391C2C
First sector: 264192 (at 129.0 MiB)
";
        assert_eq!(
            parse_partition_guid(printed).as_deref(),
            Some("21A0DF77-1234-4321-9ABC-1D0BB8391C2C")
        );
        assert_eq!(parse_partition_guid("no guid here\n"), None);
    }

    #[test]
    fn loader_default_is_replaced_in_place_or_appended() {
        // An existing default line is rewritten, everything else kept.
        let conf = "timeout 3\ndefault velstra-node-a*\nconsole-mode keep\n";
        assert_eq!(
            rewrite_loader_default(conf, "velstra-node-b"),
            "timeout 3\ndefault velstra-node-b*\nconsole-mode keep\n"
        );
        // A conf without one gains one.
        assert_eq!(
            rewrite_loader_default("timeout 3\n", "velstra-node-a"),
            "timeout 3\ndefault velstra-node-a*\n"
        );
        // The glob is the point: a blessed entry loses its +N counter, and
        // the default must keep matching it.
        assert!(rewrite_loader_default("", "velstra-node-b").contains("velstra-node-b*"));
    }
}
