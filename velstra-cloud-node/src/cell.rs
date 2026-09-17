//! The first machine of a cell, brought up from a seed alone.
//!
//! `quickstart` does two halves on a Debian box: it writes the seed, and then
//! it makes a certificate, waits for the API, and creates the Node and Pool
//! objects this machine is. On a flashed appliance the first half happened at
//! install, on a filesystem that had never booted; the second half cannot,
//! because there was no API yet and no addresses to put in a certificate — a
//! DHCP lease is not known at install time. So the second half runs at first
//! boot, as two oneshots, and both are here. Every step is the same idempotent
//! step `quickstart` takes; it calls the same functions.
//!
//! Two units rather than one because the certificate has to exist *before* the
//! API starts and the objects can only be made *after* it answers, and a
//! single unit that sat between the two would be ordered against itself.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::{quickstart, setup, tls};

/// Part one: the certificate, and the seed told about it.
///
/// A no-op on every boot but the first — `tls::ensure` keeps a certificate
/// that exists, and a seed that already names one is left alone — so a real
/// certificate dropped in by an operator survives, and so does everything else
/// in the seed.
pub fn ensure_tls(dir: &Path) -> Result<()> {
    let seed_path = dir.join("node.env");
    let text = std::fs::read_to_string(&seed_path)
        .with_context(|| format!("reading {}", seed_path.display()))?;
    let mut m = setup::parse(&text)?;
    if !m.roles.contains(&setup_role_control_plane()) {
        println!("not a control plane; nothing to do");
        return Ok(());
    }
    let hostname = crate::wizard::hostname();
    let addresses = own_addresses();
    let cert = tls::ensure(dir, &hostname, &addresses)?;
    println!(
        "{} {} ({})",
        if cert.made { "made" } else { "kept" },
        cert.cert.display(),
        cert.fingerprint
    );

    let port = m
        .listen
        .rsplit(':')
        .next()
        .filter(|p| !p.is_empty())
        .unwrap_or("8443")
        .to_string();
    let advertise = tls::advertise_urls(&hostname, &addresses, &port);
    let mut changed = false;
    if m.tls_cert.is_empty() || m.tls_key.is_empty() {
        m.tls_cert = cert.cert.display().to_string();
        m.tls_key = cert.key.display().to_string();
        changed = true;
    }
    // The advertise list follows the addresses, which a DHCP lease can move:
    // rewritten whenever it differs, because a join token naming yesterday's
    // address is a token nobody can use.
    if m.advertise != advertise {
        m.advertise = advertise;
        changed = true;
    }
    // The machine's own API, for the node and pool agents standing on it.
    //
    // The seed the installer wrote names no API, correctly — a control plane
    // is the API, and a URL pointing at itself would be a fact with two
    // owners. But the agents here are ordinary clients and need one, and this
    // is the first moment anything knows both the scheme and the port: the
    // certificate did not exist at install time. `localhost` and not the
    // address, because the certificate names hostnames and curl matches what
    // was typed. `quickstart` computes the same URL the same way on Debian;
    // the image had nobody doing it, so a flashed first machine came up with
    // a control plane and two agents that could not reach it.
    let own_api = format!("https://localhost:{port}");
    if m.api_url.is_empty() {
        m.api_url = own_api;
        m.api_ca = cert.cert.display().to_string();
        changed = true;
    }
    if changed {
        setup::write_with_mode(&seed_path, &setup::render(&m), 0o644)?;
        println!(
            "updated {} (TLS paths, VELSTRA_ADVERTISE)",
            seed_path.display()
        );
    }

    Ok(())
}

/// What the screen says before anybody signs in.
///
/// Every machine, not only a control plane — a hypervisor that shows nothing
/// leaves its operator with no way to learn the address they need, which is
/// the first thing anybody wants from a box that just came up. agetty reads
/// `/run/issue.d/*.issue` beside `/etc/issue`, so this is a file and not a
/// patch of the image.
pub fn banner(dir: &Path) -> Result<()> {
    let seed_path = dir.join("node.env");
    let m = std::fs::read_to_string(&seed_path)
        .ok()
        .and_then(|t| setup::parse(&t).ok());
    let fingerprint = tls::fingerprint_at(dir).unwrap_or_default();
    let text = issue_text(
        &crate::wizard::hostname(),
        &own_addresses(),
        m.as_ref(),
        &fingerprint,
    );
    std::fs::create_dir_all("/run/issue.d")
        .and_then(|()| std::fs::write("/run/issue.d/50-velstra.issue", &text))
        .with_context(|| "writing the console banner")?;
    print!("{text}");
    Ok(())
}

/// Open the console and SSH to whoever the seed says may use them.
///
/// Runs on every boot, not only the first: `/etc` is a tmpfs on this image, so
/// a password set last week is gone by morning. The seed is the only durable
/// statement of who may log in, and applying it every boot is what makes it
/// one — an operator who changes their mind edits the seed and reboots, rather
/// than editing a file that the next boot discards.
///
/// A seed that asks for nothing closes both, deliberately and every time: the
/// default is a machine nobody can log in to, and it stays that way even if
/// something wrote a password into the running system.
pub fn apply_access(dir: &Path) -> Result<()> {
    let seed_path = dir.join("node.env");
    let m = std::fs::read_to_string(&seed_path)
        .ok()
        .and_then(|t| setup::parse(&t).ok())
        .unwrap_or_default();

    let password = std::fs::read_to_string(dir.join("root-password"))
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());

    match &password {
        Some(secret) => {
            // Through stdin, never a command line: an argument is in `ps` for
            // every process on the machine, and this one is the root password.
            crate::install::run_stdin("chpasswd", &[], format!("root:{secret}\n").as_bytes())
                .context("setting the root password")?;
            println!("console login is open for root");
        }
        None => {
            // `--lock` rather than deleting the hash: a locked account cannot
            // be logged into and can still be unlocked by seeding a password,
            // which is what makes this reversible without a reinstall.
            let _ = crate::install::run("passwd", &["--lock", "root"]);
            println!("console login is closed");
        }
    }

    let ssh_dir = std::path::Path::new("/root/.ssh");
    let authorized = ssh_dir.join("authorized_keys");
    if m.ssh_key.trim().is_empty() {
        let _ = std::fs::remove_file(&authorized);
        let _ = crate::install::run("systemctl", &["stop", "sshd.service"]);
        println!("ssh is closed");
    } else {
        std::fs::create_dir_all(ssh_dir).context("creating /root/.ssh")?;
        setup::write_with_mode(&authorized, &format!("{}\n", m.ssh_key.trim()), 0o600)?;
        let _ = crate::install::run("chmod", &["0700", "/root/.ssh"]);
        crate::install::run("systemctl", &["start", "sshd.service"]).context("starting sshd")?;
        println!("ssh is open for the key in the seed");
    }
    Ok(())
}

/// The banner: who this machine is, where to reach it, and what runs here.
///
/// The addresses come first because they are the answer to the question
/// somebody standing at the machine actually has. A machine with a DHCP lease
/// has no other way of telling anybody what it got.
///
/// The fingerprint is the other half, and only for a control plane: a
/// self-signed certificate makes a browser warn — correctly — and the warning
/// is worth something only to somebody who can check what they are agreeing
/// to. `quickstart` printed it once on a Debian box; a flashed machine shows
/// it whenever the screen is looked at.
///
/// Pure, so the shape is tested without a machine.
fn issue_text(
    hostname: &str,
    addresses: &[String],
    seed: Option<&setup::Machine>,
    fingerprint: &str,
) -> String {
    let mut out = format!("\nVelstra Cloud — {hostname}\n");
    if addresses.is_empty() {
        out.push_str("  address:      (none yet — no link, or no lease)\n");
    } else {
        out.push_str(&format!("  address:      {}\n", addresses[0]));
        for more in addresses.iter().skip(1) {
            out.push_str(&format!("                {more}\n"));
        }
    }
    let Some(m) = seed else {
        // Flashed and not yet installed, or a seed this build cannot read.
        out.push_str("  status:       no seed — this machine has not been told what it is\n\n");
        return out;
    };
    out.push_str(&format!(
        "  runs:         {}\n",
        crate::roles::render_list(&m.roles)
    ));
    if m.roles.contains(&crate::roles::Role::ControlPlane) {
        let port = m
            .listen
            .rsplit(':')
            .next()
            .filter(|p| !p.is_empty())
            .unwrap_or("8443");
        match addresses.first() {
            Some(a) => out.push_str(&format!("  console:      https://{a}:{port}\n")),
            None => out.push_str("  console:      (no reachable address yet)\n"),
        }
        if !fingerprint.is_empty() {
            out.push_str(&format!("  certificate:  sha256 {fingerprint}\n"));
        }
        out.push_str("  sign in as the administrator named at install.\n");
    } else if !m.api_url.is_empty() {
        out.push_str(&format!("  cell:         {} at {}\n", m.cell, m.api_url));
    }
    out.push('\n');
    out
}

/// Part two: the objects, and their credentials on disk.
pub fn bootstrap(dir: &Path) -> Result<()> {
    let seed_path = dir.join("node.env");
    let text = std::fs::read_to_string(&seed_path)
        .with_context(|| format!("reading {}", seed_path.display()))?;
    let m = setup::parse(&text)?;
    if !m.roles.contains(&setup_role_control_plane()) {
        println!("not a control plane; nothing to do");
        return Ok(());
    }
    if m.node.is_empty() {
        bail!("the seed names no node id (VELSTRA_NODE), so there is no object to create");
    }
    let password_path = dir.join("bootstrap-password");
    let password = std::fs::read_to_string(&password_path)
        .with_context(|| {
            format!(
                "reading {} — the installer writes it for the first machine of a cell",
                password_path.display()
            )
        })?
        .trim()
        .to_string();
    let admin = if m.admin.is_empty() {
        "admin"
    } else {
        &m.admin
    };
    let tls = !m.tls_cert.is_empty();
    let api = quickstart::local_api(&m.listen, tls);
    if tls {
        // For this process's own curl calls; the agents get the same path
        // through the seed.
        unsafe { std::env::set_var("VELSTRA_QUICKSTART_CA", &m.tls_cert) };
    }
    quickstart::wait_for(&api)?;
    quickstart::say("the API is answering");
    let token = quickstart::api_token(&api, admin, &password)?;
    quickstart::ensure_node(&api, &token, &m.node, dir)?;
    if !m.pool.is_empty() {
        quickstart::ensure_pool(&api, &token, &m.pool)?;
    }

    // The agents were parked on the token that now exists. `start` re-reads
    // the condition; this is not enabling anything behind anybody's back —
    // both units are wanted by multi-user already.
    for unit in ["velstra-cloud-nodeagent", "velstra-cloud-poolagent"] {
        let _ = Command::new("systemctl")
            .args(["start", "--no-block", unit])
            .status();
    }

    if !m.bootstrap_ceph_osds.is_empty() {
        ensure_ceph(&api, &token, &m)?;
    }
    println!("this machine is node {} in cell {}", m.node, m.cell);
    Ok(())
}

/// The cluster the installer was asked for, on the disks it named.
///
/// The disks were named by kernel name at install time — `vdc` — and an OSD
/// spec has to name a device *as the node reports it*, which is a stable
/// `/dev/disk/by-id/…` where there is one. So this waits for the node agent
/// that has just been started to report its inventory, and takes the paths
/// from there. Guessing the rule instead would be a second copy of it, and a
/// spec that named a path the node does not use asks for an OSD it will never
/// make, for ever.
///
/// One monitor, replication 1: what one machine can be. `quorum_advice` says
/// so on the console; the cell grows from here by adding monitors and OSDs to
/// this same object.
fn ensure_ceph(api: &str, token: &str, m: &setup::Machine) -> Result<()> {
    {
        let wanted: Vec<&str> = m.bootstrap_ceph_osds.iter().map(String::as_str).collect();
        let mut devices: Vec<String> = Vec::new();
        for _ in 0..INVENTORY_WAIT_SECS {
            let body = quickstart::curl(&[
                "-H",
                &format!("Authorization: Bearer {token}"),
                &format!("{api}/nodes/{}", m.node),
            ])?;
            let node: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            devices = reported_paths(&node, &wanted);
            if devices.len() == wanted.len() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if devices.len() != wanted.len() {
            bail!(
                "the node reported {} of the {} disks the installer named for Ceph ({}); \
                 the cluster is not created. `velstra get nodes {}` shows what it sees",
                devices.len(),
                wanted.len(),
                wanted.join(", "),
                m.node
            );
        }
        let network = own_network().unwrap_or_else(|| "0.0.0.0/0".to_string());
        let body = quickstart::curl(&[
            "-X",
            "POST",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &ceph_cluster_body(&m.node, &network, &devices),
            &format!("{api}/ceph-clusters"),
        ])?;
        if body.contains("ALREADY_EXISTS") {
            quickstart::say("the Ceph cluster is already asked for");
        } else if body.contains("\"operation\"") {
            quickstart::say(&format!(
                "asked for a Ceph cluster on {} — the node agent builds it from here",
                devices.join(", ")
            ));
        } else {
            bail!(
                "creating the Ceph cluster did not go through: {}",
                body.trim()
            );
        }
        Ok(())
    }
}

/// How long to wait for the freshly started node agent's first inventory.
const INVENTORY_WAIT_SECS: u64 = 120;

/// The paths the node reports for the kernel names the installer wrote.
fn reported_paths(node: &serde_json::Value, wanted: &[&str]) -> Vec<String> {
    let devices = node["status"]["devices"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    wanted
        .iter()
        .filter_map(|name| {
            devices
                .iter()
                .find(|d| d["kernelName"].as_str() == Some(name))
                .and_then(|d| d["path"].as_str())
                .map(String::from)
        })
        .collect()
}

/// The request for a one-machine cluster: this node the monitor, these disks
/// its OSDs — taken even where the platform calls them unsuitable, because the
/// installer just asked for exactly these — and the two pools every hypervisor
/// opens, at replication 1 because there is one machine to replicate to.
fn ceph_cluster_body(node: &str, public_network: &str, devices: &[String]) -> String {
    let osds: Vec<serde_json::Value> = devices
        .iter()
        .map(|d| serde_json::json!({ "node": node, "device": d, "evenIfUnsuitable": true }))
        .collect();
    serde_json::json!({
        "id": "ceph",
        "spec": {
            "publicNetwork": public_network,
            "monitors": [node],
            "osds": osds,
            "pools": [
                { "pool": "velstra-volumes", "size": 1, "minSize": 1 },
                { "pool": "velstra-images", "size": 1, "minSize": 1 },
            ],
        }
    })
    .to_string()
}

/// This machine's first global IPv4 network, `10.10.10.0/24`, for the
/// cluster's public network. None where there is no such address.
fn own_network() -> Option<String> {
    let out = Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
        .ok()?;
    let links: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    first_global_network(&links)
}

fn first_global_network(links: &serde_json::Value) -> Option<String> {
    for link in links.as_array()? {
        if link["ifname"].as_str() == Some("lo") {
            continue;
        }
        for a in link["addr_info"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if a["scope"].as_str() != Some("global") || a["family"].as_str() != Some("inet") {
                continue;
            }
            let (Some(local), Some(prefix)) = (a["local"].as_str(), a["prefixlen"].as_u64()) else {
                continue;
            };
            if let Some(n) = network_of(local, prefix as u8) {
                return Some(n);
            }
        }
    }
    None
}

/// `10.10.10.8/24` → `10.10.10.0/24`.
fn network_of(address: &str, prefix: u8) -> Option<String> {
    let ip: std::net::Ipv4Addr = address.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask: u32 = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let net = std::net::Ipv4Addr::from(u32::from(ip) & mask);
    Some(format!("{net}/{prefix}"))
}

fn setup_role_control_plane() -> crate::roles::Role {
    crate::roles::Role::ControlPlane
}

/// This machine's global addresses, both families, from `ip -j addr`.
///
/// Global scope rather than a family filter: the kernel puts a link-local on
/// every interface by itself, and a certificate naming `fe80::…` would verify
/// for nothing anybody types. Empty when `ip` is missing or says nothing —
/// the certificate then carries the hostname and loopback, and the seed
/// advertises the hostname alone.
pub(crate) fn own_addresses() -> Vec<String> {
    let Ok(out) = Command::new("ip").args(["-j", "addr", "show"]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(links) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    addresses_of(&links)
}

/// The pure half, so it is tested without a machine.
fn addresses_of(links: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for link in links.as_array().map(Vec::as_slice).unwrap_or_default() {
        if link.get("ifname").and_then(|n| n.as_str()) == Some("lo") {
            continue;
        }
        for a in link
            .get("addr_info")
            .and_then(|a| a.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if a.get("scope").and_then(|s| s.as_str()) != Some("global") {
                continue;
            }
            if let Some(local) = a.get("local").and_then(|l| l.as_str()) {
                if !out.iter().any(|o| o == local) {
                    out.push(local.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Global addresses of both families, loopback and link-local left out,
    /// in interface order, each once.
    #[test]
    fn the_addresses_a_certificate_should_carry() {
        let links = json!([
            { "ifname": "lo", "addr_info": [ { "scope": "host", "local": "127.0.0.1" } ] },
            { "ifname": "eth0", "addr_info": [
                { "scope": "global", "local": "10.10.10.8", "prefixlen": 24 },
                { "scope": "link", "local": "fe80::1", "prefixlen": 64 },
                { "scope": "global", "local": "fd00::8", "prefixlen": 64 },
            ] },
            { "ifname": "br0", "addr_info": [
                { "scope": "global", "local": "10.10.10.8", "prefixlen": 24 },
            ] },
        ]);
        assert_eq!(addresses_of(&links), vec!["10.10.10.8", "fd00::8"]);
    }

    /// The disks the installer named come back as the paths the node uses,
    /// in the order asked; one the node has not (yet) reported is simply not
    /// there, which is what the caller waits on.
    #[test]
    fn the_installers_disks_are_resolved_to_what_the_node_reports() {
        let node = json!({ "status": { "devices": [
            { "kernelName": "vdb", "path": "/dev/disk/by-id/virtio-b" },
            { "kernelName": "vdc", "path": "/dev/disk/by-id/virtio-c" },
        ] } });
        assert_eq!(
            reported_paths(&node, &["vdc", "vdb"]),
            vec!["/dev/disk/by-id/virtio-c", "/dev/disk/by-id/virtio-b"]
        );
        assert_eq!(reported_paths(&node, &["vdd"]), Vec::<String>::new());
    }

    /// One machine, its disks, replication one, and the two pools the
    /// hypervisors open — and `evenIfUnsuitable`, because the installer was
    /// asked for exactly these disks, stick or not.
    #[test]
    fn a_first_machine_asks_for_a_one_node_cluster_on_exactly_its_disks() {
        let body: serde_json::Value = serde_json::from_str(&ceph_cluster_body(
            "horst",
            "10.10.10.0/24",
            &["/dev/disk/by-id/usb-stick".into()],
        ))
        .unwrap();
        assert_eq!(body["id"], "ceph");
        assert_eq!(body["spec"]["monitors"], json!(["horst"]));
        assert_eq!(body["spec"]["publicNetwork"], "10.10.10.0/24");
        assert_eq!(
            body["spec"]["osds"],
            json!([{ "node": "horst", "device": "/dev/disk/by-id/usb-stick", "evenIfUnsuitable": true }])
        );
        assert_eq!(body["spec"]["pools"][0]["size"], 1);
        assert_eq!(body["spec"]["pools"][1]["pool"], "velstra-images");
    }

    /// The network a one-machine cluster is told to use is the one its first
    /// global address is on, as a network and not as the address.
    #[test]
    fn the_public_network_is_the_first_global_v4_network() {
        assert_eq!(
            network_of("10.10.10.8", 24).as_deref(),
            Some("10.10.10.0/24")
        );
        assert_eq!(
            network_of("192.168.1.77", 16).as_deref(),
            Some("192.168.0.0/16")
        );
        assert_eq!(network_of("fd00::8", 64), None);
        let links = json!([
            { "ifname": "lo", "addr_info": [ { "scope": "host", "family": "inet", "local": "127.0.0.1", "prefixlen": 8 } ] },
            { "ifname": "eth0", "addr_info": [
                { "scope": "global", "family": "inet6", "local": "fd00::8", "prefixlen": 64 },
                { "scope": "global", "family": "inet", "local": "10.10.10.8", "prefixlen": 24 },
            ] },
        ]);
        assert_eq!(
            first_global_network(&links).as_deref(),
            Some("10.10.10.0/24")
        );
    }

    fn machine(roles: Vec<crate::roles::Role>) -> setup::Machine {
        setup::Machine {
            roles,
            cell: "cell-1".into(),
            listen: "0.0.0.0:8443".into(),
            api_url: "https://10.10.10.8:8443".into(),
            ..Default::default()
        }
    }

    /// The address first — it is the answer to the question somebody standing
    /// at the machine actually has — then what runs here, then the console and
    /// the fingerprint a browser warning can be checked against.
    #[test]
    fn a_control_plane_says_where_its_console_is() {
        let text = issue_text(
            "horst",
            &["10.10.10.8".into(), "fd00::8".into()],
            Some(&machine(vec![
                crate::roles::Role::ControlPlane,
                crate::roles::Role::Hypervisor,
            ])),
            "AB:CD",
        );
        assert!(text.contains("Velstra Cloud — horst\n"), "{text}");
        assert!(text.contains("address:      10.10.10.8\n"), "{text}");
        assert!(text.contains("                fd00::8\n"), "{text}");
        assert!(
            text.contains("runs:         control-plane,hypervisor\n"),
            "{text}"
        );
        assert!(
            text.contains("console:      https://10.10.10.8:8443\n"),
            "{text}"
        );
        assert!(text.contains("sha256 AB:CD"), "{text}");
    }

    /// A hypervisor has no console of its own, so it names the cell it joined
    /// instead — and still, first of all, its address.
    #[test]
    fn a_hypervisor_names_its_address_and_its_cell() {
        let text = issue_text(
            "peter",
            &["10.10.10.47".into()],
            Some(&machine(vec![crate::roles::Role::Hypervisor])),
            "",
        );
        assert!(text.contains("address:      10.10.10.47\n"), "{text}");
        assert!(
            text.contains("cell:         cell-1 at https://10.10.10.8:8443\n"),
            "{text}"
        );
        assert!(!text.contains("console:"), "{text}");
    }

    /// Flashed and not yet installed: say so, rather than showing a login
    /// prompt over a machine that has been told nothing.
    #[test]
    fn a_machine_with_no_seed_says_that_is_what_it_is() {
        let text = issue_text("velstra-node", &[], None, "");
        assert!(text.contains("address:      (none yet"), "{text}");
        assert!(text.contains("no seed"), "{text}");
    }

    /// Nothing global is nothing, not a crash and not a loopback address.
    #[test]
    fn a_machine_with_no_global_address_has_none() {
        let links = json!([
            { "ifname": "lo", "addr_info": [ { "scope": "host", "local": "127.0.0.1" } ] },
            { "ifname": "eth0", "addr_info": [ { "scope": "link", "local": "fe80::1" } ] },
        ]);
        assert!(addresses_of(&links).is_empty());
        assert!(addresses_of(&json!("not a list")).is_empty());
    }
}
