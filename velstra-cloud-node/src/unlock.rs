//! Unlocking the encrypted data partition at boot.
//!
//! An encrypted install lays the data filesystem inside a LUKS2 volume (see
//! [`crate::install::Crypto`]). Everything writable — the node token, the
//! guests' disks, whatever lands in `/var/lib/velstra` — lives there, so the
//! node cannot mount it until the volume is open. This is the command a
//! boot-time unit runs, ordered before that mount, to open it.
//!
//! It is deliberately conditional: the SAME image serves encrypted and
//! plaintext installs, so a static "always unlock" would break every
//! unencrypted node. So it looks at what is actually on the disk and does the
//! right thing —
//!
//!   * the data volume is already open → nothing to do;
//!   * there is no LUKS data volume (a plaintext install) → nothing to do, and
//!     the ordinary `/dev/disk/by-label/data` mount takes it from here;
//!   * there IS a locked LUKS data volume → prompt for the passphrase (via
//!     `systemd-ask-password`, so a console operator or a remote
//!     `systemd-tty-ask-password-agent` can answer) and open it.
//!
//! Passphrase-based only, for now: TPM2 unattended unlock is a documented
//! follow-up (`systemd-cryptenroll`), and the on-disk format is standard LUKS2
//! so a TPM2 token can be enrolled onto the same volume later without
//! reformatting.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    install::{require_root, run_stdin},
    product::{self, DATA_MAPPER, data_mapper_path},
};

/// The filesystem type `lsblk`/`blkid` report for a LUKS container.
const LUKS_FSTYPE: &str = "crypto_LUKS";

/// How many times to re-prompt for a wrong passphrase before giving up. A boot
/// prompt an operator fat-fingers should not drop them straight to a rescue
/// shell, but an endless loop is its own denial of service.
const MAX_ATTEMPTS: u32 = 3;

/// Open the encrypted data volume, prompting for its passphrase — or do
/// nothing if there is no encrypted volume (or it is already open).
///
/// Run by the node's unlock unit before `/var/lib/velstra` is mounted, not by
/// hand. Needs root (it drives `cryptsetup`).
pub fn run() -> Result<()> {
    require_root("unlock")?;
    if Path::new(&data_mapper_path()).exists() {
        eprintln!("data volume already unlocked");
        return Ok(());
    }
    let Some(dev) = find_luks_data()? else {
        // A plaintext install: the by-label/data mount handles it. Say so
        // plainly rather than looking like a failure.
        eprintln!("no encrypted data volume found — nothing to unlock");
        return Ok(());
    };
    eprintln!("unlocking the encrypted data volume on {dev} …");
    for attempt in 1..=MAX_ATTEMPTS {
        let passphrase = ask_passphrase()?;
        match open(&dev, &passphrase) {
            Ok(()) => {
                eprintln!("data volume unlocked");
                return Ok(());
            }
            Err(e) => eprintln!("attempt {attempt}/{MAX_ATTEMPTS} failed: {e}"),
        }
    }
    bail!("could not unlock the data volume after {MAX_ATTEMPTS} attempts")
}

/// Find the locked LUKS data device, if there is one.
///
/// Reads `lsblk` (no partition table parsing, stable columns) and picks the
/// crypto_LUKS device — see [`pick_luks_data`]. Kept apart from that pure
/// chooser so the choice logic is tested without a disk.
fn find_luks_data() -> Result<Option<String>> {
    let out = Command::new("lsblk")
        .args(["-rno", "NAME,FSTYPE,PARTLABEL"])
        .output()
        .context("listing block devices to find the encrypted data volume")?;
    if !out.status.success() {
        bail!("lsblk failed (exit {:?})", out.status.code());
    }
    Ok(pick_luks_data(&String::from_utf8_lossy(&out.stdout)).map(|name| format!("/dev/{name}")))
}

/// Choose the data LUKS device from `lsblk -rno NAME,FSTYPE,PARTLABEL` output.
///
/// A crypto_LUKS device labelled `data` (the single-disk install: the data
/// PARTLABEL survives on the outer container) wins outright. Failing that,
/// when exactly one crypto_LUKS device exists it is taken — the RAID install,
/// whose LUKS sits on the assembled array and carries no partition label. Two
/// unlabelled LUKS devices is ambiguous and picks neither, rather than
/// opening the wrong one; the refusal names the candidates so the operator
/// can see exactly what confused it.
fn pick_luks_data(lsblk: &str) -> Option<String> {
    let luks: Vec<(String, Option<String>)> = lsblk
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let name = f.next()?;
            let fstype = f.next()?;
            if fstype != LUKS_FSTYPE {
                return None;
            }
            // Whatever remains is the partition label (empty for an md array).
            let label = f.next().map(str::to_string);
            Some((name.to_string(), label))
        })
        .collect();
    if let Some((name, _)) = luks
        .iter()
        .find(|(_, label)| label.as_deref() == Some("data"))
    {
        return Some(name.clone());
    }
    match luks.as_slice() {
        [(name, _)] => Some(name.clone()),
        [] => None,
        many => {
            // Refused rather than guessed: opening the wrong volume with the
            // right passphrase would mount somebody else's filesystem where
            // the node's state belongs.
            let names: Vec<&str> = many.iter().map(|(n, _)| n.as_str()).collect();
            eprintln!(
                "several unlabelled LUKS devices ({}) — refusing to guess which is the data volume",
                names.join(", ")
            );
            None
        }
    }
}

/// Ask the operator for the passphrase via `systemd-ask-password`, which
/// reaches the console AND any waiting `systemd-tty-ask-password-agent`, so an
/// unattended node can be unlocked over an already-open channel too. Returns
/// the entered passphrase (systemd prints it to stdout).
fn ask_passphrase() -> Result<String> {
    let out = Command::new("systemd-ask-password")
        .args(["--timeout=0", product::UNLOCK_PROMPT])
        .output()
        .context("prompting for the LUKS passphrase")?;
    if !out.status.success() {
        bail!("systemd-ask-password failed (exit {:?})", out.status.code());
    }
    // Only the trailing newline is stripped: a passphrase may legitimately
    // begin or end with a space, so nothing else is trimmed.
    let mut pass = String::from_utf8_lossy(&out.stdout).into_owned();
    if pass.ends_with('\n') {
        pass.pop();
        if pass.ends_with('\r') {
            pass.pop();
        }
    }
    Ok(pass)
}

/// `cryptsetup open <dev> data`, passphrase on stdin (never on argv).
fn open(dev: &str, passphrase: &str) -> Result<()> {
    run_stdin(
        "cryptsetup",
        &["open", "--key-file", "-", dev, DATA_MAPPER],
        passphrase.as_bytes(),
    )
    .with_context(|| format!("cryptsetup open refused the passphrase for {dev}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single-disk case: the data PARTLABEL rides on the outer LUKS
    /// partition, so a crypto_LUKS device labelled `data` is chosen outright
    /// even when other LUKS devices are present.
    #[test]
    fn a_labelled_luks_data_partition_is_chosen() {
        let lsblk = "\
vda      \nvda1     vfat  \nvda6     crypto_LUKS data\nvdb      \nvdb1     crypto_LUKS other\n";
        assert_eq!(pick_luks_data(lsblk), Some("vda6".to_string()));
    }

    /// The RAID case: the LUKS sits on the assembled array and carries no
    /// partition label, so the sole crypto_LUKS device is taken.
    #[test]
    fn a_sole_unlabelled_luks_device_is_chosen() {
        let lsblk = "\
vda      \nvdb      \nmd127    crypto_LUKS \n";
        assert_eq!(pick_luks_data(lsblk), Some("md127".to_string()));
    }

    /// A plaintext install has no LUKS device at all — nothing is chosen, and
    /// the caller does nothing (the by-label mount handles it).
    #[test]
    fn a_plaintext_disk_chooses_nothing() {
        let lsblk = "\
vda      \nvda1     vfat  \nvda6     ext4  data\n";
        assert_eq!(pick_luks_data(lsblk), None);
    }

    /// Two unlabelled LUKS devices is ambiguous: pick neither rather than
    /// open the wrong volume.
    #[test]
    fn ambiguous_unlabelled_luks_devices_choose_nothing() {
        let lsblk = "\
sda      crypto_LUKS \nsdb      crypto_LUKS \n";
        assert_eq!(pick_luks_data(lsblk), None);
    }
}
