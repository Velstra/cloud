//! The node installer: write the running verified-boot image onto a target
//! disk (or a RAID array) so the machine boots from internal storage.
//!
//! The node ships as an immutable dm-verity image from the same Nix image
//! factory as the Sentinel appliance, and this is a port of that installer's
//! proven flow: validate the plan, pre-flight every check that can fail, and
//! only then start erasing — in that order, always. The store is
//! integrity-sealed, so what lands on disk is the exact verified image.
//!
//! Tools (`lsblk`, `sgdisk`, `dd`, `mdadm`, `cryptsetup`, …) are resolved by
//! name on the PATH: the Nix service and ISO supply the PATH here, so an
//! env-var-per-tool scheme would be a knob nothing turns.

use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{
    disks::{self, Disk, Raid, human_size, part_path},
    product::{self, DATA_PART, SEALED_PARTS, data_mapper_path},
    seed, wizard,
};

/// How the writable data partition is protected at rest.
///
/// The A/B root is dm-verity and read-only, so there is nothing on it worth
/// encrypting — its integrity, not its secrecy, is what matters. The secrets
/// live on the ONE writable partition: the node token, and whatever the
/// tenants' guests leave in `/var/lib/velstra`. That is what LUKS protects
/// here, and nothing else needs to.
#[derive(Debug, Clone)]
pub enum Crypto {
    /// Plaintext ext4 — the default.
    None,
    /// LUKS2 over the data partition, unlocked at boot with this passphrase.
    Luks { passphrase: String },
}

/// The `install` subcommand: discover disks, run the wizard, execute, seed.
///
/// Interactive only, by design: an unattended node install is an image-factory
/// job, not a console one, and a half-scripted wizard is worse than either.
/// Without a terminal the candidates are listed and nothing is touched.
pub fn run_install(source: Option<PathBuf>) -> Result<()> {
    // A bundled source image may come from the flag or the environment (the
    // ISO sets $VELSTRA_NODE_INSTALL_SOURCE).
    let source =
        source.or_else(|| std::env::var_os(product::INSTALL_SOURCE_ENV).map(PathBuf::from));
    let disks = disks::discover_disks()?;
    if disks.is_empty() {
        println!("no disks found");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        // Non-interactive: list the candidates and stop. Erasing a disk on the
        // say-so of a closed stdin is not a behaviour anybody wants to script.
        wizard::list_disks(&disks);
        println!("\nthe installer is interactive — run it on a terminal.");
        return Ok(());
    }

    let Some(answers) = wizard::collect(&disks)? else {
        println!("aborted — nothing was written.");
        return Ok(());
    };

    let targets: Vec<String> = answers
        .picks
        .iter()
        .filter_map(|i| disks.get(*i))
        .map(|d| d.dev_path())
        .collect();
    let chosen = disks::plan_targets(&disks, &targets, answers.raid)?;

    let crypto = match &answers.passphrase {
        Some(p) => Crypto::Luks {
            passphrase: p.clone(),
        },
        None => Crypto::None,
    };
    execute(&chosen, answers.raid, source.as_deref(), &crypto)?;
    // Seeded after the image is written, because the data partition it lands
    // on is created by the install.
    seed::seed(&chosen, answers.raid, &answers, &crypto)?;
    println!("\nInstalled. Remove the medium and reboot; the node comes up registered.");
    Ok(())
}

/// Execute the validated install onto `targets`: partition each disk like the
/// sealed source image, clone the ESP + dm-verity partitions onto it, then
/// make the data filesystem (a plain ext4, or an mdadm array for RAID).
/// DESTRUCTIVE.
///
/// `source_image` is a raw node image to clone from (the ISO/live-boot case);
/// when `None`, the source is the booted verity medium itself.
pub fn execute(
    targets: &[&Disk],
    raid: Raid,
    source_image: Option<&Path>,
    crypto: &Crypto,
) -> Result<()> {
    require_root("install")?;
    // The source is either a loop device over the bundled image, or the booted
    // medium's own disk. The guard detaches the loop device on return.
    let (source, _loop) = match source_image {
        Some(img) => {
            let dev = losetup_attach(img)?;
            (dev.clone(), Some(LoopGuard(dev)))
        }
        None => (find_source_disk()?, None),
    };
    eprintln!("source (install medium): {source}");

    // Pre-flight: reject any target that collides with the source medium
    // BEFORE erasing anything. With two targets, a check inside the prepare
    // loop would wipe disk 1 before finding disk 2 is the source — leaving a
    // blank disk and no install. Sentinel learnt this the hard way.
    for t in targets {
        let dev = t.dev_path();
        if dev == source {
            bail!("refusing to install onto the source medium {dev}");
        }
    }

    // Pre-flight: every target must be big enough for the source's own layout.
    // `sgdisk --replicate` writes the source's partition table verbatim, so a
    // disk that is merely over the 5 GiB floor is not enough — and finding
    // that out inside `prepare_disk` means finding it out after `wipefs`.
    let need = source_layout_bytes(&source)?;
    for t in targets {
        if t.size < need {
            bail!(
                "disk {} is {} — the node layout on {source} needs {}",
                t.dev_path(),
                human_size(t.size),
                human_size(need)
            );
        }
    }

    // From here on disks are being erased. Track which, so a mid-way failure
    // reports exactly which disks are left blank (none are recoverable once
    // wipefs has run — but the operator must know which to re-install).
    let mut erased: Vec<String> = Vec::new();
    for t in targets {
        let dev = t.dev_path();
        eprintln!("preparing {dev} (ERASING) …");
        erased.push(dev.clone());
        if let Err(e) = prepare_disk(&source, &dev, raid) {
            return Err(partial_install_error(e, &erased));
        }
    }

    let data_parts: Vec<String> = targets
        .iter()
        .map(|t| part_path(&t.dev_path(), DATA_PART))
        .collect();
    // Build the data filesystem (or RAID array, optionally LUKS-wrapped). A
    // failure here leaves every erased disk partitioned but without a bootable
    // system — report which.
    let fs_result: Result<()> = (|| {
        // The block device the data filesystem sits on: the single data
        // partition, or the assembled array across all of them.
        let base = build_data_base(&data_parts, raid)?;
        // Optionally wrap that device in LUKS2. The ext4 (LABEL=data) is then
        // made *inside* the opened volume, so at rest the partition is a LUKS
        // container and nothing readable. The volume is opened only for the
        // mkfs and closed again here, so the install ends in a clean, LOCKED
        // state — the node (or the seed step) unlocks it fresh with the
        // passphrase.
        match crypto {
            Crypto::None => {
                run("mkfs.ext4", &["-q", "-F", "-L", "data", &base])?;
            }
            Crypto::Luks { passphrase } => {
                eprintln!("encrypting the data partition (LUKS2) …");
                luks_format_open(&base, passphrase)?;
                let guard = CryptGuard;
                run("udevadm", &["settle"]).ok();
                run(
                    "mkfs.ext4",
                    &["-q", "-F", "-L", "data", &data_mapper_path()],
                )?;
                drop(guard); // close the volume now, not at end of scope
            }
        }
        Ok(())
    })();
    if let Err(e) = fs_result {
        return Err(partial_install_error(e, &erased));
    }
    eprintln!("install complete.");
    Ok(())
}

/// Take down whatever is holding `target` open, so it can be erased.
///
/// Scoped to this disk on purpose. A volume group that also has a physical
/// volume on another disk is left alone and named instead: deactivating it
/// would take down storage the operator did not offer to this installer, and
/// "it erased the disk I did not pick" is not a failure anybody recovers from.
///
/// Every step is best-effort and in the order the holders stack — swap and
/// mounts sit on top of dm and md, which sit on the partitions. What is left
/// after this is reported by [`holders_of`], which is what makes the refusal
/// name a cause instead of an errno.
fn release_holders(target: &str) -> Result<()> {
    let kernel = target.trim_start_matches("/dev/");

    // Swap first: a swap partition is an open holder and `swapoff` is the only
    // thing that closes it.
    if let Ok(swaps) = std::fs::read_to_string("/proc/swaps") {
        for line in swaps.lines().skip(1) {
            let Some(dev) = line.split_whitespace().next() else {
                continue;
            };
            if dev.starts_with(target) {
                eprintln!("  swapoff {dev}");
                let _ = run("swapoff", &[dev]);
            }
        }
    }

    // Then mounts, deepest first: /var before /, or the shallower umount
    // fails on a busy subtree.
    let mut mounts: Vec<(String, String)> = Vec::new();
    if let Ok(mi) = std::fs::read_to_string("/proc/self/mountinfo") {
        for line in mi.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            let Some(sep) = f.iter().position(|x| *x == "-") else {
                continue;
            };
            let (Some(point), Some(dev)) = (f.get(4), f.get(sep + 2)) else {
                continue;
            };
            if dev.starts_with(target) {
                mounts.push(((*point).to_string(), (*dev).to_string()));
            }
        }
    }
    mounts.sort_by_key(|(point, _)| std::cmp::Reverse(point.matches('/').count()));
    for (point, dev) in mounts {
        eprintln!("  umount {point} ({dev})");
        let _ = run("umount", &[&point]);
    }

    // Device-mapper and md, from `/sys/class/block/<part>/holders`. That
    // directory is the kernel's own answer to "who has this open", so it needs
    // no guessing about naming and covers LUKS, LVM and md alike.
    for part in partitions_of(kernel) {
        for holder in holders_named(&part) {
            if holder.starts_with("dm-") {
                let name = std::fs::read_to_string(format!("/sys/class/block/{holder}/dm/name"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                // An LVM volume group may span disks this installer was not
                // given. Named rather than deactivated; `wipefs` then refuses
                // and says so, which is the safe half of the two.
                if is_shared_volume_group(&name, target) {
                    eprintln!(
                        "  leaving {name} alone: its volume group also lives on another disk"
                    );
                    continue;
                }
                eprintln!("  dmsetup remove {name}");
                let _ = run("dmsetup", &["remove", "--retry", &name]);
            } else if holder.starts_with("md") {
                eprintln!("  mdadm --stop /dev/{holder}");
                let _ = run("mdadm", &["--stop", &format!("/dev/{holder}")]);
            }
        }
    }

    // The kernel needs a moment to notice; `wipefs` right after a `dmsetup
    // remove` can still see the old holder.
    let _ = run("udevadm", &["settle"]);

    let left = holders_of(kernel);
    if !left.is_empty() {
        bail!(
            "{target} is still held by {} — nothing has been written. \
             Take it down and run the installer again, or pick another disk. \
             `lsblk {target}` shows what is on it",
            left.join(", ")
        );
    }
    Ok(())
}

/// The partition kernel names of a whole disk, `nvme0n1p1`, `sda1`.
fn partitions_of(kernel: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(format!("/sys/class/block/{kernel}")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(kernel) && n != kernel)
        .collect()
}

/// What the kernel says currently holds a block device open.
fn holders_named(kernel: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(format!("/sys/class/block/{kernel}/holders")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// Everything still holding the disk or any of its partitions, for a refusal
/// that names a cause. Device-mapper devices are reported by their real name
/// (`vg0-root`) rather than `dm-3`, because that is what an operator can act on.
fn holders_of(kernel: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut parts = vec![kernel.to_string()];
    parts.extend(partitions_of(kernel));
    for part in parts {
        for holder in holders_named(&part) {
            let named = std::fs::read_to_string(format!("/sys/class/block/{holder}/dm/name"))
                .ok()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or(holder);
            if !out.contains(&named) {
                out.push(named);
            }
        }
    }
    out
}

/// Whether a device-mapper name belongs to an LVM volume group with a physical
/// volume outside `target`.
///
/// `pvs` is asked rather than inferred: an LVM name is `<vg>-<lv>` with dashes
/// doubled inside each half, and reconstructing the volume group from the
/// mapper name is a parser nobody should write. No LVM on the machine means no
/// `pvs`, which means nothing shared — the `false` is right either way.
fn is_shared_volume_group(mapper_name: &str, target: &str) -> bool {
    let Ok(out) = Command::new("pvs")
        .args(["--noheadings", "-o", "pv_name,vg_name"])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // The volume groups that have a PV on this disk …
    let mine: Vec<&str> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pv, vg) = (f.next()?, f.next()?);
            pv.starts_with(target).then_some(vg)
        })
        .collect();
    // … and whether any of them also has one somewhere else.
    text.lines().any(|l| {
        let mut f = l.split_whitespace();
        let (Some(pv), Some(vg)) = (f.next(), f.next()) else {
            return false;
        };
        mine.contains(&vg) && !pv.starts_with(target) && mapper_name.starts_with(vg)
    })
}

/// Lay the image's A/B partition layout onto `target` and clone the sealed
/// partitions block-for-block from `source`. The data partition (the last
/// one) is recreated to fill the target, typed for a filesystem or a RAID
/// member.
fn prepare_disk(source: &str, target: &str, raid: Raid) -> Result<()> {
    // Everything the kernel is still holding this disk open with, taken down
    // first. Without it `wipefs` fails with `probing initialization failed:
    // Device or resource busy` and the install stops before it has written
    // anything — on any machine that already carries an installation, which is
    // every machine being *re*-installed.
    //
    // A live medium assembles what it finds: an existing LUKS volume becomes a
    // `/dev/mapper/…`, an LVM volume group is activated, an md array is
    // started, a swap partition may be switched on. Each of those is an open
    // holder of the whole-disk device, and none of them is visible in the
    // candidate listing — so the operator sees a disk offered, picks it, says
    // YES, and is told the device is busy with no indication of what by.
    release_holders(target)?;
    run("wipefs", &["-a", target])?;
    // Replicate the source GPT onto the target (`--replicate=<dest>` takes the
    // DESTINATION; the source is the positional device), then move the backup
    // header to the end of the (larger) target. This also lays down the
    // (empty) slot-B partition entries for later updates.
    run("sgdisk", &[&format!("--replicate={target}"), source])?;
    run("sgdisk", &["--move-second-header", target])?;
    // Give the disk a fresh **disk** GUID so it doesn't collide with the
    // source medium — but do NOT randomise the *partition* GUIDs. The
    // dm-verity store relies on the systemd Discoverable Partitions
    // convention: the verity and store partition UUIDs are derived from the
    // roothash the UKI carries as `usrhash=`, so systemd auto-binds
    // `/dev/mapper/usr` by matching them at boot — it never needs to be told
    // which partitions to use, and that is why the GUIDs are load-bearing.
    // `sgdisk --randomize-guids` would overwrite those roothash-derived UUIDs,
    // so the installed system could never activate the verity device — it
    // would time out waiting for `/dev/mapper/usr` and drop to emergency mode
    // (while a directly-booted image, with the UUIDs intact, works fine).
    // `--disk-guid=R` randomises only the disk GUID and leaves every partition
    // UUID as replicated. The data partition below is recreated and so gets
    // its own fresh UUID regardless.
    run("sgdisk", &["--disk-guid=R", target])?;
    // Recreate the data partition to fill the disk, typed for the install
    // mode.
    let typecode = if raid.mdadm_level().is_some() {
        "FD00" // Linux RAID
    } else {
        "8300" // Linux filesystem
    };
    run("sgdisk", &[&format!("--delete={DATA_PART}"), target])?;
    run(
        "sgdisk",
        &[
            &format!("--new={DATA_PART}:0:0"),
            &format!("--typecode={DATA_PART}:{typecode}"),
            &format!("--change-name={DATA_PART}:data"),
            target,
        ],
    )?;
    run("partprobe", &[target]).ok();
    run("udevadm", &["settle"]).ok();
    // Clone the sealed partitions (ESP/UKI + slot A's verity hash + store).
    // Slot B (the reserved generic partitions) is left empty for a future
    // update.
    for n in SEALED_PARTS {
        run(
            "dd",
            &[
                &format!("if={}", part_path(source, n)),
                &format!("of={}", part_path(target, n)),
                "bs=4M",
                "conv=fsync",
            ],
        )?;
    }
    Ok(())
}

/// The block device the data filesystem sits on: the single data partition, or
/// the freshly-assembled RAID array across all of them. For the RAID case this
/// *creates* the array, so it is called exactly once per install.
fn build_data_base(data_parts: &[String], raid: Raid) -> Result<String> {
    match raid.mdadm_level() {
        None => Ok(data_parts[0].clone()),
        Some(level) => {
            run("udevadm", &["settle"]).ok();
            let n = data_parts.len().to_string();
            let mut args = vec![
                "--create",
                product::MD_ARRAY,
                "--level",
                level,
                "--raid-devices",
                &n,
                "--metadata=1.2",
                "--run",
                "--force",
            ];
            args.extend(data_parts.iter().map(String::as_str));
            eprintln!("creating RAID{level} across {} disk(s) …", data_parts.len());
            run("mdadm", &args)?;
            Ok(product::MD_ARRAY.to_string())
        }
    }
}

/// The already-assembled data base device to seed into, WITHOUT recreating a
/// RAID array (which `execute` has already built in this installer session).
pub(crate) fn existing_data_base(targets: &[&Disk], raid: Raid) -> String {
    match raid.mdadm_level() {
        Some(_) => product::MD_ARRAY.to_string(),
        None => part_path(&targets[0].dev_path(), DATA_PART),
    }
}

/// Wrap a destructive-phase failure with the list of disks already erased, so
/// the operator knows exactly which disks are left blank. Disk contents are
/// gone (wipefs ran) — this is a clear report, not a recovery: re-running the
/// install finishes the job, since every listed disk is an intended target
/// anyway.
fn partial_install_error(cause: anyhow::Error, erased: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "install failed after starting to erase {}: {cause}\n\
         these disk(s) are now BLANK (partitioned but WITHOUT a complete, bootable \
         system); re-run the install to finish, or restore them from backup. No \
         disk outside this list was touched.",
        erased.join(", ")
    )
}

// ---- LUKS ------------------------------------------------------------------

/// Format `dev` as a fresh LUKS2 container and open it as the data mapper,
/// with the passphrase fed on stdin (never on argv, where it would show in
/// `ps` and the install log). LUKS2 is the modern default: Argon2id key
/// derivation, a resilient on-disk header.
///
/// TPM2-backed unattended unlock is a deliberate follow-up, not wired here: it
/// needs `systemd-cryptenroll` against real hardware to test, and a half-wired
/// TPM path is worse than an honest passphrase one. The on-disk format is
/// standard LUKS2, so a later release can enrol a TPM2 token onto the very
/// same volume without reformatting.
fn luks_format_open(dev: &str, passphrase: &str) -> Result<()> {
    if passphrase.is_empty() {
        bail!("refusing to create an encrypted volume with an empty passphrase");
    }
    // `--key-file -` reads the passphrase from stdin; `--batch-mode` skips the
    // interactive "type uppercase YES" confirmation (the wizard already
    // confirmed the erase). luksFormat then open, both fed the same
    // passphrase.
    run_stdin(
        "cryptsetup",
        &[
            "luksFormat",
            "--type",
            "luks2",
            "--batch-mode",
            "--key-file",
            "-",
            dev,
        ],
        passphrase.as_bytes(),
    )
    .with_context(|| format!("creating the LUKS2 volume on {dev}"))?;
    luks_open(dev, passphrase)
}

/// Open an existing LUKS2 volume on `dev` as the data mapper, passphrase on
/// stdin.
pub(crate) fn luks_open(dev: &str, passphrase: &str) -> Result<()> {
    run_stdin(
        "cryptsetup",
        &["open", "--key-file", "-", dev, product::DATA_MAPPER],
        passphrase.as_bytes(),
    )
    .with_context(|| format!("opening the LUKS2 volume on {dev}"))
}

/// Close the data mapper device when dropped, so an encrypted install leaves
/// nothing unlocked behind it however it returns.
pub(crate) struct CryptGuard;
impl Drop for CryptGuard {
    fn drop(&mut self) {
        let _ = Command::new("cryptsetup")
            .args(["close", product::DATA_MAPPER])
            .status();
    }
}

// ---- Source discovery ------------------------------------------------------

/// The whole disk holding the running, sealed verity store — i.e. the install
/// medium we clone from. Follows the **active** dm-verity device
/// (`/dev/mapper/usr`) down to its backing disk; a partlabel would be
/// ambiguous once a target has been installed (it copies the same labels).
pub(crate) fn find_source_disk() -> Result<String> {
    // `-s` walks the dependency tree downward; `-r` (raw) avoids tree-drawing
    // characters in the NAME column. Pick the entry whose TYPE is `disk`.
    let out = Command::new("lsblk")
        .args(["-nsro", "NAME,TYPE", "/dev/mapper/usr"])
        .output()
        .context("locating the source disk")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let name = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (name, kind) = (f.next()?, f.next()?);
            (kind == "disk").then(|| name.to_string())
        })
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not resolve the source disk from /dev/mapper/usr"))?;
    Ok(format!("/dev/{name}"))
}

/// Ask `sgdisk` what the source layout needs, in bytes.
fn source_layout_bytes(source: &str) -> Result<u64> {
    let out = Command::new("sgdisk")
        .args(["-p", source])
        .output()
        .with_context(|| format!("reading the partition layout of {source}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    disks::parse_layout_bytes(&text)
        .ok_or_else(|| anyhow::anyhow!("{source} reports no partitions to clone"))
}

/// Detaches a loop device when dropped, so a `--source` image install cleans
/// up even on error.
pub(crate) struct LoopGuard(pub(crate) String);
impl Drop for LoopGuard {
    fn drop(&mut self) {
        let _ = Command::new("losetup").args(["-d", &self.0]).status();
    }
}

/// Attach a raw image file as a partitioned loop device, returning its path.
pub(crate) fn losetup_attach(image: &Path) -> Result<String> {
    let img = image
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 image path"))?;
    let out = Command::new("losetup")
        .args(["-P", "-f", "--show", img])
        .output()
        .context("attaching the source image via losetup")?;
    if !out.status.success() {
        bail!("losetup failed (exit {:?})", out.status.code());
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dev.is_empty() {
        bail!("losetup returned no device for {img}");
    }
    Ok(dev)
}

// ---- Process helpers -------------------------------------------------------

/// Unmounts a path when dropped, so a failed step never leaves the target's
/// filesystem mounted under the live medium.
pub(crate) struct MountGuard(pub(crate) PathBuf);
impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

/// Refuse to continue unless we are root. Asked of `id`(1) rather than
/// `geteuid`(2): this workspace carries no libc crate, and the one caller who
/// could hit the difference (a PATH without coreutils) could not have run the
/// installer's other tools either.
pub(crate) fn require_root(what: &str) -> Result<()> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running `id -u` to check for root")?;
    let uid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .context("`id -u` printed something that is not a uid")?;
    if uid != 0 {
        bail!("{what} must run as root (try `sudo velstra-cloud-node {what} …`)");
    }
    Ok(())
}

/// Run an external tool (resolved by name on the PATH), inheriting stdio,
/// failing on a non-zero exit.
pub(crate) fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("running {cmd} — is it on the PATH?"))?;
    if !status.success() {
        bail!(
            "`{cmd} {}` failed (exit {:?})",
            args.join(" "),
            status.code()
        );
    }
    Ok(())
}

/// Run an external tool feeding `input` on stdin — used to hand a LUKS
/// passphrase to `cryptsetup` without ever putting it on the command line.
/// stdout/stderr are inherited so the tool's own errors still reach the
/// operator.
pub(crate) fn run_stdin(cmd: &str, args: &[&str], input: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {cmd} — is it on the PATH?"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin for {cmd}"))?
        .write_all(input)
        .with_context(|| format!("feeding the passphrase to {cmd}"))?;
    let status = child.wait().with_context(|| format!("waiting for {cmd}"))?;
    if !status.success() {
        bail!(
            "`{cmd} {}` failed (exit {:?})",
            args.join(" "),
            status.code()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, gib: u64) -> Disk {
        Disk {
            name: name.into(),
            size: gib * 1024 * 1024 * 1024,
            model: String::new(),
            removable: false,
        }
    }

    #[test]
    fn the_seed_target_is_the_array_or_the_partition() {
        let sda = disk("sda", 500);
        let sdb = disk("sdb", 500);
        // Single disk: the raw data partition.
        assert_eq!(existing_data_base(&[&sda], Raid::None), "/dev/sda6");
        // RAID: the assembled array node execute created.
        assert_eq!(
            existing_data_base(&[&sda, &sdb], Raid::Mirror),
            "/dev/md/velstra-node-data"
        );
    }

    #[test]
    fn an_empty_luks_passphrase_is_refused_before_touching_the_disk() {
        // Fails on the empty-passphrase guard, before any cryptsetup spawn —
        // so this runs without cryptsetup present.
        let err = luks_format_open("/dev/does-not-matter", "").unwrap_err();
        assert!(format!("{err}").contains("empty passphrase"), "{err}");
    }

    #[test]
    fn partial_install_error_names_the_erased_disks() {
        let e = partial_install_error(
            anyhow::anyhow!("mkfs failed"),
            &["/dev/sda".into(), "/dev/sdb".into()],
        );
        let msg = format!("{e}");
        // Names every erased disk and its underlying cause.
        assert!(msg.contains("/dev/sda"), "{msg}");
        assert!(msg.contains("/dev/sdb"), "{msg}");
        assert!(msg.contains("mkfs failed"), "{msg}");
        // Makes the blank/not-bootable state unambiguous.
        assert!(msg.contains("BLANK"), "{msg}");
    }
}
