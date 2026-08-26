//! Seeding the freshly installed data partition, so the node's first boot is
//! its working one.
//!
//! This is what makes the wizard worth having. Without it the operator
//! installs, reboots into a node with no name and no token, and has to answer
//! everything again at the console — which is exactly the step a guided
//! installer exists to remove.
//!
//! The data partition IS `/var/lib/velstra` on the running node — it is
//! mounted there — so the seed lands at the partition's ROOT. (Sentinel once
//! wrote its seed into a nested directory and every install answer was gone by
//! the first login prompt; that lesson is why this comment exists.) The
//! consuming systemd units read `node.env` via `EnvironmentFile` and hand the
//! agent `--api-token-file /var/lib/velstra/node-token`.

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result};

use crate::{
    disks::{Disk, Raid},
    install::{CryptGuard, Crypto, MountGuard, existing_data_base, luks_open, run},
    product,
    wizard::{Answers, Network},
};

/// Mount the installed data filesystem, write the seed files, and unmount.
///
/// On an encrypted install `execute` has left the data partition a closed LUKS
/// volume, so it is re-opened with the passphrase to write the seed, and
/// closed again afterwards so the freshly installed node unlocks it fresh on
/// its first boot. Both guards run however this returns, error paths included;
/// the mount guard is declared *after* the crypt guard so it drops first
/// (unmount before close). A plaintext install writes the base device
/// directly, unchanged.
pub fn seed(targets: &[&Disk], raid: Raid, answers: &Answers, crypto: &Crypto) -> Result<()> {
    let (dev, _crypt_guard) = match crypto {
        Crypto::None => (existing_data_base(targets, raid), None),
        Crypto::Luks { passphrase } => {
            luks_open(&existing_data_base(targets, raid), passphrase)?;
            (product::data_mapper_path(), Some(CryptGuard))
        }
    };
    let mnt = Path::new(product::SEED_MOUNTPOINT);
    fs::create_dir_all(mnt).context("creating the seed mountpoint")?;
    run("mount", &[&dev, product::SEED_MOUNTPOINT])?;
    let _guard = MountGuard(mnt.to_path_buf());

    // node.env is world-readable: nothing in it is secret, and the units that
    // read it do not all run as root. The token is the secret, and it gets its
    // own file with its own mode.
    write_with_mode(&mnt.join("node.env"), &render_node_env(answers), 0o644)?;
    write_with_mode(
        &mnt.join("node-token"),
        &format!("{}\n", answers.token),
        0o600,
    )?;

    if let Network::Static {
        iface,
        address,
        gateway,
        dns,
    } = &answers.network
    {
        // Only a static uplink writes anything: the image's default is
        // DHCP-everywhere, and a seed that restated the default would be a
        // second copy of it to drift.
        let dir = mnt.join("network");
        fs::create_dir_all(&dir).context("creating the seed network directory")?;
        write_with_mode(
            &dir.join("10-uplink.network"),
            &render_network_unit(iface, address, gateway, dns),
            0o644,
        )?;
    }
    eprintln!("seeded {} (node.env, node-token)", mnt.display());
    Ok(())
}

/// Render `node.env` — exactly these keys, one per line, values verbatim.
///
/// No quoting on purpose: the wizard refused any value that would need it (see
/// [`crate::wizard::validate_safe_value`]), so a value here is safe for
/// systemd's `EnvironmentFile` and for a shell sourcing the file by hand.
pub(crate) fn render_node_env(a: &Answers) -> String {
    format!(
        "VELSTRA_NODE={}\n\
         VELSTRA_CELL={}\n\
         VELSTRA_REGION={}\n\
         VELSTRA_API_URL={}\n\
         VELSTRA_VMM={}\n\
         VELSTRA_HOSTNAME={}\n",
        a.node, a.cell, a.region, a.api_url, a.vmm, a.hostname
    )
}

/// Render the systemd-networkd unit for a static uplink.
pub(crate) fn render_network_unit(iface: &str, address: &str, gateway: &str, dns: &str) -> String {
    format!(
        "[Match]\n\
         Name={iface}\n\
         \n\
         [Network]\n\
         Address={address}\n\
         Gateway={gateway}\n\
         DNS={dns}\n"
    )
}

/// Write a file with an explicit mode, atomically enough for a fresh
/// filesystem nobody else has mounted: the mode is fixed after the write, and
/// the only reader is a future boot.
fn write_with_mode(path: &Path, contents: &str, mode: u32) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disks::Raid;

    fn answers() -> Answers {
        Answers {
            raid: Raid::None,
            picks: vec![0],
            passphrase: None,
            hostname: "velstra-node".into(),
            network: Network::Dhcp,
            api_url: "https://cloud.example.net".into(),
            node: "node-7".into(),
            cell: "cell-1".into(),
            region: "eu-central".into(),
            token: "ab".repeat(32),
            vmm: "cloud-hypervisor".into(),
        }
    }

    #[test]
    fn node_env_carries_exactly_the_agreed_keys_in_order() {
        let env = render_node_env(&answers());
        assert_eq!(
            env,
            "VELSTRA_NODE=node-7\n\
             VELSTRA_CELL=cell-1\n\
             VELSTRA_REGION=eu-central\n\
             VELSTRA_API_URL=https://cloud.example.net\n\
             VELSTRA_VMM=cloud-hypervisor\n\
             VELSTRA_HOSTNAME=velstra-node\n"
        );
        // One key per line, no quoting, trailing newline — the exact shape
        // systemd's EnvironmentFile and a hand `source` both accept.
        assert!(env.ends_with('\n'));
        assert_eq!(env.lines().count(), 6);
    }

    #[test]
    fn the_network_unit_is_a_complete_networkd_match_and_network() {
        let unit = render_network_unit("eth0", "192.0.2.10/24", "192.0.2.1", "192.0.2.53");
        assert_eq!(
            unit,
            "[Match]\n\
             Name=eth0\n\
             \n\
             [Network]\n\
             Address=192.0.2.10/24\n\
             Gateway=192.0.2.1\n\
             DNS=192.0.2.53\n"
        );
    }
}
