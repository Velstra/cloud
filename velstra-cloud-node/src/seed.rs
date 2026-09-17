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
    // The certificate a join token carried, at the path the seed names for
    // it. World-readable: a certificate, not a secret.
    if !answers.api_ca_pem.trim().is_empty() {
        write_with_mode(&mnt.join("api-ca.pem"), &answers.api_ca_pem, 0o644)?;
    }
    write_with_mode(&mnt.join("node.env"), &render_node_env(answers), 0o644)?;
    // The first administrator's password, for the first machine of a cell.
    // Its own file and mode, like the tokens: the API unit reads it into the
    // environment at start rather than taking it on a command line.
    if !answers.admin_password.is_empty() {
        write_with_mode(
            &mnt.join("bootstrap-password"),
            &format!("{}\n", answers.admin_password),
            0o600,
        )?;
    }
    // Only the credentials this machine was actually given. A control plane
    // that is not a hypervisor has no node token, and writing an empty file
    // would satisfy the node agent's `ConditionPathExists` with nothing in it
    // — a unit that starts, authenticates as nobody, and is refused for ever.
    let mut wrote = vec!["node.env"];
    // The root password, when one was asked for: its own file and mode, like
    // every other secret here.
    if !answers.root_password.is_empty() {
        write_with_mode(
            &mnt.join("root-password"),
            &format!("{}\n", answers.root_password),
            0o600,
        )?;
        wrote.push("root-password");
    }

    if !answers.token.is_empty() {
        write_with_mode(
            &mnt.join("node-token"),
            &format!("{}\n", answers.token),
            0o600,
        )?;
        wrote.push("node-token");
    }
    // A pool is a second agent with a second credential: the API authenticates
    // it as `pool:<id>` and the node agent as `node:<id>`, and a node token
    // presented by a pool agent is answered 401 for ever with the seed looking
    // complete.
    if !answers.pool_token.is_empty() {
        write_with_mode(
            &mnt.join("pool-token"),
            &format!("{}\n", answers.pool_token),
            0o600,
        )?;
        wrote.push("pool-token");
    }

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
    eprintln!("seeded {} ({})", mnt.display(), wrote.join(", "));
    Ok(())
}

/// Render `node.env` by handing the installer's answers to the one renderer.
///
/// **This used to write the file itself**, and that is the whole reason a
/// flashed machine could only ever be a hypervisor. Six keys were written here
/// and twenty-eight by [`crate::setup::render`]; `VELSTRA_ROLES` was among the
/// twenty-two this one did not know about, and an absent `VELSTRA_ROLES` reads
/// as `[hypervisor]` — correctly, for a seed written before roles existed, and
/// silently wrong for one written yesterday by an installer that had simply
/// never been told.
///
/// Nothing was ever going to notice: both renderers had tests, both passed,
/// and each was right about the keys it knew. So there is one now, and the
/// installer is a caller of it rather than a second author.
///
/// No quoting on purpose: the wizard refused any value that would need it (see
/// [`crate::wizard::validate_safe_value`]), so a value here is safe for
/// systemd's `EnvironmentFile` and for a shell sourcing the file by hand.
pub(crate) fn render_node_env(a: &Answers) -> String {
    crate::setup::render(&machine_from(a))
}

/// The installer's answers as the thing the rest of this platform calls a
/// machine.
///
/// Every field the installer cannot answer is left empty, and `render` writes
/// only what was answered — so a seed from here has exactly the keys a seed
/// from `velstra-cloud-node setup` would have for the same machine.
fn machine_from(a: &Answers) -> crate::setup::Machine {
    crate::setup::Machine {
        region: a.region.clone(),
        cell: a.cell.clone(),
        roles: a.roles.clone(),
        hostname: a.hostname.clone(),
        api_url: a.api_url.clone(),
        node: a.node.clone(),
        token: a.token.clone(),
        vmm: a.vmm.clone(),
        pool: a.pool.clone(),
        pool_token: a.pool_token.clone(),
        pool_backend: a.pool_backend.clone(),
        listen: a.listen.clone(),
        admin: a.admin.clone(),
        admin_password: a.admin_password.clone(),
        api_ca_pem: a.api_ca_pem.clone(),
        bootstrap_ceph_osds: a.ceph_osds.clone(),
        ssh_key: a.ssh_key.clone(),
        root_password: a.root_password.clone(),
        // Not asked at install time. Which cards a machine holds back is a
        // decision about what it will run, which nobody has made while they
        // are standing in front of a disk that is about to be erased — and it
        // is reversible afterwards with one line in the seed, where an answer
        // given here would not have been. See `docs/setup-guide.md`.
        passthrough: String::new(),
        // A machine born with Ceph opens the cluster with the files the node
        // agent writes from the cell — the same files every hypervisor gets —
        // as the client the cell minted for them.
        ceph_conf: if a.ceph_osds.is_empty() {
            String::new()
        } else {
            format!("{}/ceph/ceph.conf", crate::setup::SEED_DIR)
        },
        ceph_user: if a.ceph_osds.is_empty() {
            String::new()
        } else {
            "velstra".into()
        },
        // The seed names the file `write_seed` would have written; here the
        // data partition is the seed directory, so the path is its root.
        api_ca: if a.api_ca_pem.is_empty() {
            String::new()
        } else {
            format!("{}/api-ca.pem", crate::setup::SEED_DIR)
        },
        ..Default::default()
    }
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
            roles: vec![crate::roles::Role::Hypervisor],
            pool: String::new(),
            pool_token: String::new(),
            pool_backend: String::new(),
            listen: String::new(),
            admin: String::new(),
            admin_password: String::new(),
            api_ca_pem: String::new(),
            ceph_osds: Vec::new(),
            ssh_key: String::new(),
            root_password: String::new(),
        }
    }

    #[test]
    fn node_env_carries_what_was_answered_and_nothing_else() {
        let env = render_node_env(&answers());
        for expected in [
            "VELSTRA_REGION=eu-central",
            "VELSTRA_CELL=cell-1",
            "VELSTRA_ROLES=hypervisor",
            "VELSTRA_HOSTNAME=velstra-node",
            "VELSTRA_API_URL=https://cloud.example.net",
            "VELSTRA_NODE=node-7",
            "VELSTRA_VMM=cloud-hypervisor",
        ] {
            assert!(
                env.lines().any(|l| l == expected),
                "{expected} missing:\n{env}"
            );
        }
        // Nothing this machine is not. A pool key on a hypervisor's seed would
        // start a pool agent against a pool called nothing.
        assert!(!env.contains("VELSTRA_POOL"), "{env}");
        assert!(!env.contains("VELSTRA_LISTEN"), "{env}");
        // One key per line, no quoting, trailing newline — the exact shape
        // systemd's EnvironmentFile and a hand `source` both accept.
        assert!(env.ends_with('\n'));
        assert!(
            env.lines().all(|l| l.contains('=') && !l.contains('"')),
            "{env}"
        );
    }

    /// The installer can now seed a machine that is not a hypervisor at all,
    /// which is the thing it could not express before.
    #[test]
    fn an_installer_can_seed_a_control_plane_that_is_also_a_pool() {
        let mut a = answers();
        a.roles = vec![crate::roles::Role::ControlPlane, crate::roles::Role::Pool];
        a.api_url = String::new();
        a.listen = "0.0.0.0:8443".into();
        a.pool = "local".into();
        a.pool_backend = "ceph".into();
        let env = render_node_env(&a);
        assert!(env.contains("VELSTRA_ROLES=control-plane,pool"), "{env}");
        assert!(env.contains("VELSTRA_LISTEN=0.0.0.0:8443"), "{env}");
        assert!(env.contains("VELSTRA_POOL=local"), "{env}");
        assert!(env.contains("VELSTRA_POOL_BACKEND=ceph"), "{env}");
        // No hypervisor keys: this box runs no guests, and a node id it never
        // registered would have it reporting capacity nobody asked for.
        assert!(!env.contains("VELSTRA_NODE="), "{env}");
        assert!(!env.contains("VELSTRA_VMM="), "{env}");
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

#[cfg(test)]
mod door_tests {
    use super::*;
    use crate::disks::Raid;

    fn base() -> Answers {
        Answers {
            raid: Raid::None,
            picks: vec![0],
            passphrase: None,
            hostname: "horst".into(),
            network: Network::Dhcp,
            api_url: String::new(),
            node: "horst".into(),
            cell: "cell-1".into(),
            region: "eu-central".into(),
            token: String::new(),
            vmm: "qemu".into(),
            roles: vec![],
            pool: String::new(),
            pool_token: String::new(),
            pool_backend: String::new(),
            listen: String::new(),
            admin: String::new(),
            admin_password: String::new(),
            api_ca_pem: String::new(),
            ceph_osds: Vec::new(),
            ssh_key: String::new(),
            root_password: String::new(),
        }
    }

    /// The first machine: all three roles, its administrator named in the
    /// seed, its password kept out of it, and no credentials that nothing has
    /// issued yet.
    #[test]
    fn the_first_machine_seeds_a_whole_cell_and_no_unissued_credential() {
        let mut a = base();
        a.roles = vec![
            crate::roles::Role::ControlPlane,
            crate::roles::Role::Hypervisor,
            crate::roles::Role::Pool,
        ];
        a.listen = "0.0.0.0:8443".into();
        a.pool = "local".into();
        a.pool_backend = "directory".into();
        a.admin = "admin".into();
        a.admin_password = "correcthorsebattery".into();
        let env = render_node_env(&a);
        assert!(
            env.contains("VELSTRA_ROLES=control-plane,hypervisor,pool\n"),
            "{env}"
        );
        assert!(env.contains("VELSTRA_BOOTSTRAP_ADMIN=admin\n"), "{env}");
        assert!(env.contains("VELSTRA_LISTEN=0.0.0.0:8443\n"), "{env}");
        assert!(env.contains("VELSTRA_NODE=horst\n"), "{env}");
        assert!(env.contains("VELSTRA_POOL=local\n"), "{env}");
        assert!(
            !env.contains("correcthorsebattery"),
            "the password is not in the seed: {env}"
        );
        assert!(
            !env.contains("VELSTRA_API_URL="),
            "a control plane is the API: {env}"
        );
    }

    /// A machine the installer wrote holds nothing back. The key exists and
    /// is added afterwards, by hand or by configuration management; what must
    /// not happen is an empty `VELSTRA_PASSTHROUGH=` that the binary would
    /// have to read as "none".
    #[test]
    fn a_freshly_installed_machine_has_no_passthrough_key() {
        let mut a = base();
        a.roles = vec![crate::roles::Role::Hypervisor];
        let env = render_node_env(&a);
        assert!(!env.contains("VELSTRA_PASSTHROUGH"), "{env}");
    }

    /// A joiner: the certificate rides in, and the seed names where it lands.
    #[test]
    fn a_joiner_seeds_the_certificate_it_was_handed() {
        let mut a = base();
        a.roles = vec![crate::roles::Role::Hypervisor];
        a.api_url = "https://10.10.10.8:8443".into();
        a.token = "ab".repeat(32);
        a.api_ca_pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into();
        let env = render_node_env(&a);
        assert!(
            env.contains("VELSTRA_API_CA=/var/lib/velstra/api-ca.pem\n"),
            "{env}"
        );
        assert!(
            env.contains("VELSTRA_API_URL=https://10.10.10.8:8443\n"),
            "{env}"
        );
        assert!(env.contains("VELSTRA_VMM=qemu\n"), "{env}");
    }
}

#[cfg(test)]
mod born_with_ceph {
    use super::*;
    use crate::disks::Raid;

    /// Born with Ceph: the disks travel in the seed for first boot, the pool
    /// is the Ceph pool, and it is opened with the files the cell publishes.
    #[test]
    fn the_first_machine_may_be_born_with_ceph_on_its_spare_disks() {
        let a = Answers {
            raid: Raid::None,
            picks: vec![0],
            passphrase: None,
            hostname: "horst".into(),
            network: Network::Dhcp,
            api_url: String::new(),
            node: "horst".into(),
            cell: "cell-1".into(),
            region: "eu-central".into(),
            token: String::new(),
            vmm: "qemu".into(),
            roles: vec![
                crate::roles::Role::ControlPlane,
                crate::roles::Role::Hypervisor,
                crate::roles::Role::Pool,
            ],
            pool: "ceph".into(),
            pool_token: String::new(),
            pool_backend: "ceph".into(),
            listen: "0.0.0.0:8443".into(),
            admin: "admin".into(),
            admin_password: "correcthorsebattery".into(),
            api_ca_pem: String::new(),
            ceph_osds: vec!["sdb".into(), "sdc".into()],
            ssh_key: String::new(),
            root_password: String::new(),
        };
        let env = render_node_env(&a);
        assert!(
            env.contains("VELSTRA_BOOTSTRAP_CEPH_OSDS=sdb,sdc\n"),
            "{env}"
        );
        assert!(env.contains("VELSTRA_POOL=ceph\n"), "{env}");
        assert!(env.contains("VELSTRA_POOL_BACKEND=ceph\n"), "{env}");
        assert!(
            env.contains("VELSTRA_CEPH_CONF=/var/lib/velstra/ceph/ceph.conf\n"),
            "{env}"
        );
        assert!(env.contains("VELSTRA_CEPH_USER=velstra\n"), "{env}");
        assert!(!env.contains("correcthorsebattery"), "{env}");
    }
}
