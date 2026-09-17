//! The text wizard: every question the installer asks, one per line.
//!
//! Text-mode only, on purpose. Sentinel carries a full-screen ratatui
//! installer alongside its line-by-line one; a compute node is installed over
//! IPMI, a serial line, or an `expect` script, and the line-by-line form works
//! on all three while the full-screen one works on none of the interesting
//! ones. Prompt mechanics are ported from Sentinel's `collect_text`: defaults
//! in brackets, an empty answer takes the default, invalid answers re-ask with
//! a sentence saying what would be valid, and nothing destructive happens
//! before the final typed `YES`.
//!
//! Validation here is also what keeps the seed shell-safe: `node.env` is read
//! by systemd's `EnvironmentFile`, and rather than quoting values on the way
//! out, values that would need quoting (whitespace, quotes, control
//! characters, `$`, backslash, backtick) are refused on the way in.

use std::{io::IsTerminal, net::Ipv4Addr, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    disks::{self, Disk, Raid, human_size},
    product,
};

/// How the uplink gets its address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// The image's default: systemd-networkd DHCP on every interface. The
    /// wizard writes nothing, so there is nothing to drift from that default.
    Dhcp,
    /// A static uplink, written to the seed as a systemd-networkd unit.
    Static {
        iface: String,
        /// `a.b.c.d/len`.
        address: String,
        gateway: String,
        dns: String,
    },
}

/// Everything the wizard collected, validated and confirmed.
#[derive(Debug, Clone)]
pub struct Answers {
    pub raid: Raid,
    /// Indices into the discovered-disk list.
    pub picks: Vec<usize>,
    /// `Some` = encrypt the data partition with this LUKS2 passphrase.
    pub passphrase: Option<String>,
    pub hostname: String,
    pub network: Network,
    pub api_url: String,
    pub node: String,
    pub cell: String,
    pub region: String,
    /// The one-time registration token, 64 lowercase hex characters.
    pub token: String,
    /// `cloud-hypervisor` or `qemu`, as the nodeagent's `--vmm` spells them.
    pub vmm: String,
    /// What this machine is for.
    ///
    /// The installer did not ask this until the sealed image could serve more
    /// than one role, and it had to stay silent while it could not: a seed
    /// naming `pool` on an image with no pool agent would be a machine
    /// promising something nothing on it can do. Now that the image carries
    /// all three, this is the question that decides which units come up.
    pub roles: Vec<crate::roles::Role>,
    /// The pool's id and backend, asked only when `roles` names a pool.
    pub pool: String,
    pub pool_backend: String,
    /// The pool's own one-time token. A second credential, because a pool
    /// agent authenticates as `pool:<id>` and a node agent as `node:<id>` —
    /// one token presented by the other is a 401 for ever, with the seed
    /// looking complete.
    pub pool_token: String,
    /// What the API binds, asked only when `roles` names the control plane.
    pub listen: String,
    /// The cell's first administrator, for the first machine of a new cell.
    /// Empty everywhere else.
    pub admin: String,
    pub admin_password: String,
    /// The API's certificate, PEM, when it arrived inside a join token.
    /// Written beside the seed as `api-ca.pem`; see `seed::seed`.
    pub api_ca_pem: String,
    /// Disks the first machine gives to Ceph, as kernel names (`sdb`). Empty
    /// is "no Ceph now" — it can still be added from the console later.
    pub ceph_osds: Vec<String>,
    /// An SSH public key that may log in as root, and a root password for the
    /// console. Both empty is the sealed default; see `ask_for_access`.
    pub ssh_key: String,
    pub root_password: String,
}

/// Run the wizard. Returns `None` when the operator declines the final YES —
/// nothing has been written at that point.
pub fn collect(disks: &[Disk]) -> Result<Option<Answers>> {
    // The mode first, then the disks — and the disks are listed *after* the
    // mode is known, so the list is read with its question already in mind.
    //
    // It ran the other way round: a table of disks, then a mode, then "select
    // disk number(s)" about a table that had scrolled past. Somebody reading
    // top to bottom met a list before there was anything to do with it, and a
    // plural prompt whether or not the mode they had just picked takes more
    // than one disk.
    println!("\nInstall mode:");
    println!("  [1] single disk");
    println!("  [2] RAID1  (mirror — redundancy, 2+ disks)");
    println!("  [3] RAID0  (stripe — capacity, no redundancy, 2+ disks)");
    println!("  [4] RAID10 (striped mirror, 4+ disks)");
    let raid = loop {
        match prompt("Mode [1-4]: ")?.trim() {
            "1" => break Raid::None,
            "2" => break Raid::Mirror,
            "3" => break Raid::Stripe,
            "4" => break Raid::Mirror10,
            other => println!("  {other:?} is not a mode — pick a number from 1 to 4."),
        }
    };

    println!();
    list_disks(disks);

    // Picks are validated against the full plan (count, size, removable) here,
    // so a refused disk costs a re-ask and not a restart of the wizard.
    let disk_prompt = match raid {
        Raid::None => "Disk number: ".to_string(),
        _ => format!(
            "Disk numbers for {:?}, space-separated (at least {}): ",
            raid,
            raid.min_disks()
        ),
    };
    let (picks, chosen) = loop {
        let raw = prompt(&disk_prompt)?;
        match resolve_picks(disks, raw.trim()) {
            Ok(picks) => {
                let targets: Vec<String> = picks
                    .iter()
                    .filter_map(|i| disks.get(*i))
                    .map(|d| d.dev_path())
                    .collect();
                match disks::plan_targets(disks, &targets, raid) {
                    Ok(chosen) => break (picks, chosen),
                    Err(e) => println!("  {e:#}"),
                }
            }
            Err(e) => println!("  {e:#}"),
        }
    };

    let (ssh_key, root_password) = ask_for_access()?;

    let passphrase = if ask_yes("Encrypt the data partition with LUKS2?", false)? {
        Some(resolve_passphrase()?)
    } else {
        None
    };

    let hostname = ask_safe("Hostname", product::DEFAULT_HOSTNAME)?;

    let network = if ask_yes("Use DHCP for the uplink?", true)? {
        Network::Dhcp
    } else {
        let iface = ask_safe("Uplink interface name", "")?;
        let address = ask_valid(
            "Static address (CIDR, e.g. 192.0.2.10/24): ",
            validate_cidr,
            "an IPv4 address with a prefix length, like 192.0.2.10/24",
        )?;
        let gateway = ask_valid(
            "Gateway (IPv4 address): ",
            validate_ipv4,
            "a plain IPv4 address, like 192.0.2.1",
        )?;
        // The gateway is the default DNS server: on a small static network the
        // router usually forwards DNS, and a default that resolves nothing
        // would leave the node unable to reach the control plane by name.
        let dns = loop {
            let got = ask("DNS server", &gateway)?;
            match validate_ipv4(&got) {
                Ok(()) => break got,
                Err(e) => println!("  {e:#} — expected a plain IPv4 address."),
            }
        };
        Network::Static {
            iface,
            address,
            gateway,
            dns,
        }
    };

    // What this box is for, asked before anything that depends on the answer.
    //
    // A set rather than a choice: the smallest real cell is one machine that is
    // all three, and a wizard that made them exclusive would make that cell
    // impossible to install. `gateway` is deliberately absent — it lives on the
    // Node object and is an operator's to write, because a registration token
    // exists so a machine can *report*, and one that could also declare its
    // holder a gateway would be a token that grants itself the cell's external
    // traffic.
    // Three doors, and the two that matter ask almost nothing. The first
    // machine of a cell has one question that is not about disks — who its
    // administrator is — and every machine after it has none, because the
    // cell already answered them all into a join token. The third door is the
    // wizard as it was, for the machine that is neither: joining without a
    // token, or a pool on its own.
    println!("\nInstall as:");
    println!("  [1] the first machine of a new cell   control plane, hypervisor, storage");
    println!("  [2] a machine joining a cell          paste the join token from the console");
    println!("  [3] custom                            the questions, one by one");
    let door = loop {
        match prompt("Install as [1]: ")?.trim() {
            "" | "1" => break 1,
            "2" => break 2,
            "3" => break 3,
            other => println!("  {other:?} is not an option — pick 1, 2 or 3."),
        }
    };

    if door == 2 {
        return join(
            chosen,
            raid,
            picks,
            passphrase,
            hostname,
            network,
            ssh_key,
            root_password,
        );
    }

    let (admin, admin_password) = if door == 1 {
        println!("\nThe cell's first administrator signs into the console as this user.");
        // `ask_valid_or`, not `ask_valid` with a map: the validator ran on the
        // empty answer first and refused it, so the promised default was one
        // nobody could take — the prompt asked again, for ever.
        let admin = ask_valid_or(
            "admin",
            "Administrator [admin]: ",
            validate_node_name,
            "lowercase letters, digits and '-'",
        )?;
        let password = loop {
            let first = prompt_secret("Password (not echoed): ")?;
            if first.len() < 12 {
                println!("  at least 12 characters — this is the cell's administrator");
                continue;
            }
            let again = prompt_secret("Again: ")?;
            if first == again {
                break first;
            }
            println!("  they differ — once more");
        };
        (admin, password)
    } else {
        (String::new(), String::new())
    };

    // Storage for the first machine: a directory on its own disk, or Ceph on
    // the disks it did not install onto. Ceph now is a real choice — a
    // one-node cluster on a USB stick is a lab, and `quorum_advice` will say
    // so on the console — and it is also not the only chance: a cell can
    // grow a cluster later, from the console, once it has the machines.
    let ceph_osds: Vec<String> = if door == 1 {
        let spare: Vec<(usize, &Disk)> = disks
            .iter()
            .enumerate()
            .filter(|(i, _)| !picks.contains(i))
            .collect();
        if spare.is_empty() {
            println!("\nNo other disk here, so storage is a directory on the install disk.");
            println!("Ceph can be added from the console once the cell has disks for it.");
            Vec::new()
        } else {
            println!("\nStorage:");
            println!("  [1] a directory on the install disk   simplest; guests cannot move");
            println!("  [2] Ceph, on other disks here          every node reaches every volume;");
            println!("                                         one node and one disk is a lab");
            let ceph = loop {
                match prompt("Storage [1]: ")?.trim() {
                    "" | "1" => break false,
                    "2" => break true,
                    other => println!("  {other:?} is not an option — pick 1 or 2."),
                }
            };
            if ceph {
                println!("\nDisks for Ceph — they are ERASED when the cluster is made:");
                for (i, d) in &spare {
                    println!(
                        "  [{}] {} {} {}",
                        i + 1,
                        d.dev_path(),
                        human_size(d.size),
                        d.model
                    );
                }
                loop {
                    let got = prompt("Disk numbers, comma-separated: ")?;
                    let mut names = Vec::new();
                    let mut bad = false;
                    for part in got.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                        match part.parse::<usize>() {
                            Ok(n) if n >= 1 && spare.iter().any(|(i, _)| *i == n - 1) => {
                                names.push(disks[n - 1].name.clone());
                            }
                            _ => {
                                println!("  {part:?} is not one of the disks above");
                                bad = true;
                            }
                        }
                    }
                    if !bad && !names.is_empty() {
                        break names;
                    }
                    if !bad {
                        println!("  at least one disk, or go back and pick a directory");
                    }
                }
            } else {
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let roles = if door == 1 {
        vec![
            crate::roles::Role::ControlPlane,
            crate::roles::Role::Hypervisor,
            crate::roles::Role::Pool,
        ]
    } else {
        println!("\nWhat does this machine run?");
        println!("  control-plane  the API, the controllers and this cell's store");
        println!("  hypervisor     guests");
        println!("  pool           storage");
        println!("\nSeveral, comma-separated, is normal: one box that is all three is a whole");
        println!("cell, and is what a first machine usually is.");
        loop {
            let got = prompt("Roles [hypervisor]: ")?;
            let text = if got.trim().is_empty() {
                "hypervisor"
            } else {
                got.trim()
            };
            let parsed = crate::roles::parse_list(text);
            // `parse_list` drops what it does not recognise, so a typo would
            // otherwise become a machine quietly missing a role. Comparing counts
            // is what turns that into a question asked again.
            if !parsed.is_empty()
                && parsed.len() == text.split(',').filter(|p| !p.trim().is_empty()).count()
            {
                break parsed;
            }
            println!("  not a role list — one or more of control-plane, hypervisor, pool");
        }
    };
    let is_control_plane = roles.contains(&crate::roles::Role::ControlPlane);
    let is_hypervisor = roles.contains(&crate::roles::Role::Hypervisor);
    let is_pool = roles.contains(&crate::roles::Role::Pool);

    // A control plane *is* the API. Asking it for a URL pointing at itself
    // would be a fact with two owners, and the two would disagree the first
    // time somebody changed one of them.
    let (api_url, listen) = if is_control_plane {
        let listen = loop {
            let got = ask("API listen address", "0.0.0.0:8443")?;
            match validate_safe_value(&got) {
                Ok(()) => break got,
                Err(e) => println!("  {e:#}"),
            }
        };
        (String::new(), listen)
    } else {
        let url = ask_valid(
            "Control plane URL (http:// or https://): ",
            validate_url,
            "a URL starting http:// or https://, e.g. https://cloud.example.net",
        )?;
        (url, String::new())
    };

    let node = if door == 1 {
        // The first machine names itself: there is no operator yet to have
        // created a node object, and `bootstrap-cell` creates it at first boot
        // under this id. The hostname is the obvious one.
        hostname.clone()
    } else if is_hypervisor {
        ask_valid(
            "Node name (e.g. node-7): ",
            validate_node_name,
            "the id the operator created via POST /api/v1/nodes — lowercase letters, digits and dashes",
        )?
    } else {
        String::new()
    };

    let cell = loop {
        let got = ask("Cell", product::DEFAULT_CELL)?;
        match validate_safe_value(&got) {
            Ok(()) => break got,
            Err(e) => println!("  {e:#}"),
        }
    };
    let region = loop {
        let got = ask("Region", product::DEFAULT_REGION)?;
        match validate_safe_value(&got) {
            Ok(()) => break got,
            Err(e) => println!("  {e:#}"),
        }
    };

    // Not echoed, for the reason on `prompt_secret`: this token is a bearer
    // credential that admits a machine to the cluster, and an install console
    // is a serial line or an IPMI session that keeps its scrollback. The
    // summary below already declines to print it; the prompt has to agree.
    let token = if door == 1 {
        // Minted at first boot by `bootstrap-cell`, once there is an API to
        // mint it. A token typed here would be one nothing had issued.
        String::new()
    } else if is_hypervisor {
        ask_valid_secret(
            "Node registration token (64 hex chars): ",
            validate_token,
            "the one-time nodeToken the API returned when the operator created this \
             node (POST /api/v1/nodes): exactly 64 lowercase hex characters",
        )?
    } else {
        String::new()
    };

    let vmm = if is_hypervisor {
        // The default follows the storage: a machine born with Ceph, or
        // joining a cell that may grow it, opens its volumes only with QEMU.
        // Cloud Hypervisor takes a path and nothing else, so offering it as
        // the default beside a Ceph answer would be a wizard that builds a
        // machine unable to boot the guests it was installed for.
        let qemu_default = !ceph_osds.is_empty();
        println!("\nVMM:");
        if qemu_default {
            println!("  [1] cloud-hypervisor");
            println!(
                "  [2] qemu (default) — the only one that can open the Ceph volumes just chosen"
            );
        } else {
            println!("  [1] cloud-hypervisor (default)");
            println!("  [2] qemu — the only one that can open a Ceph volume");
        }
        let ask = if qemu_default {
            "VMM [2]: "
        } else {
            "VMM [1]: "
        };
        loop {
            match prompt(ask)?.trim() {
                "" => {
                    break if qemu_default {
                        "qemu"
                    } else {
                        "cloud-hypervisor"
                    }
                    .to_string();
                }
                "1" => break "cloud-hypervisor".to_string(),
                "2" => break "qemu".to_string(),
                other => println!("  {other:?} is not an option — pick 1 or 2."),
            }
        }
    } else {
        String::new()
    };

    let (pool, pool_backend, pool_token) = if door == 1 {
        // The first machine's own pool, created at first boot: Ceph on the
        // disks just named, or a directory called `local`. Either way the
        // cell can grow a Ceph cluster later, from the console.
        if ceph_osds.is_empty() {
            ("local".to_string(), "directory".to_string(), String::new())
        } else {
            ("ceph".to_string(), "ceph".to_string(), String::new())
        }
    } else if is_pool {
        let id = ask_valid(
            "Pool id (e.g. local): ",
            validate_node_name,
            "the id the operator created via POST /api/v1/pools — lowercase letters, digits and dashes",
        )?;
        println!("\nBackend:");
        println!("  [1] directory  qcow2 files on this machine. Everything it holds is here,");
        println!("                 so a guest on such a volume cannot move to another node.");
        println!("  [2] ceph       RBD images. Every node reaches every volume.");
        println!("  [3] lvm        logical volumes on this machine, no image format.");
        let backend = loop {
            match prompt("Backend [1]: ")?.trim() {
                "" | "1" => break "directory".to_string(),
                "2" => break "ceph".to_string(),
                "3" => break "lvm".to_string(),
                other => println!("  {other:?} is not an option — pick 1, 2 or 3."),
            }
        };
        // A control plane's pool agent reaches the store directly and is given
        // no token at all. Every other pool needs its own, because the API
        // issues `poolToken` and `nodeToken` separately and authenticates a
        // pool agent as `pool:<id>` — a node token presented by a pool agent is
        // answered 401 for ever, with the seed looking complete.
        let token = if is_control_plane {
            String::new()
        } else {
            ask_valid_secret(
                "Pool registration token (64 hex chars): ",
                validate_token,
                "the one-time poolToken the API returned when the operator created this \
                 pool (POST /api/v1/pools): exactly 64 lowercase hex characters",
            )?
        };
        (id, backend, token)
    } else {
        (String::new(), String::new(), String::new())
    };

    // The review: everything the erase will produce, with the two secrets —
    // the passphrase and the token — deliberately not echoed back into
    // whatever scrollback this console keeps.
    println!();
    print_plan(&chosen, raid);
    if passphrase.is_some() {
        println!("  data partition: LUKS2 encrypted (passphrase asked at each boot)");
    }
    println!("\nThe installed node will come up as:");
    println!("  hostname:      {hostname}");
    match &network {
        Network::Dhcp => println!("  network:       DHCP on the uplink"),
        Network::Static {
            iface,
            address,
            gateway,
            dns,
        } => {
            println!("  network:       {iface} static {address}, gateway {gateway}, DNS {dns}");
        }
    }
    println!("  roles:         {}", crate::roles::render_list(&roles));
    if is_control_plane {
        println!("  api listens:   {listen}");
        if !admin.is_empty() {
            println!("  administrator: {admin} (password not echoed)");
        }
    } else {
        println!("  control plane: {api_url}");
    }
    println!("  cell/region:   {cell} / {region}");
    if is_hypervisor {
        println!("  node:          {node}");
        println!("  vmm:           {vmm}");
        println!("  node token:    (64 hex chars — not echoed)");
    }
    print_access(&ssh_key, &root_password);
    if is_pool {
        println!("  pool:          {pool} ({pool_backend})");
        if !ceph_osds.is_empty() {
            println!(
                "  ceph osds:     {} — ERASED at first boot",
                ceph_osds.join(", ")
            );
        }
        if !pool_token.is_empty() {
            println!("  pool token:    (64 hex chars — not echoed)");
        }
    }

    let confirm = prompt("\nThis ERASES the selected disk(s). Type YES to proceed: ")?;
    if confirm.trim() != "YES" {
        return Ok(None);
    }

    Ok(Some(Answers {
        raid,
        picks,
        passphrase,
        hostname,
        network,
        api_url,
        node,
        cell,
        region,
        token,
        vmm,
        roles,
        pool,
        pool_backend,
        pool_token,
        listen,
        admin,
        admin_password,
        api_ca_pem: String::new(),
        ceph_osds,
        ssh_key,
        root_password,
    }))
}

/// Whether anybody may log in to this machine, and how.
///
/// The image has no accounts: `root` exists with no password and
/// `allowNoPasswordLogin` off, so the `login:` prompt on the screen is a dead
/// end by construction. Fleet access is the control plane, and break-glass is
/// booting the installer medium — which is a defensible default and a
/// genuinely awkward one the first time something is wrong at three in the
/// morning.
///
/// So it is offered rather than decided. Nothing is the default, and the
/// question says what nothing means, because an operator who does not know
/// the machine has no accounts cannot know to ask for one.
fn ask_for_access() -> Result<(String, String)> {
    println!("\nAccess to this machine itself:");
    println!("  [1] none (default) — the console and SSH are closed. Manage it");
    println!("      through the cell; to get a shell, boot this installer again.");
    println!("  [2] an SSH key");
    println!("  [3] a console password");
    println!("  [4] both");
    let (want_key, want_password) = loop {
        match prompt("Access [1]: ")?.trim() {
            "" | "1" => break (false, false),
            "2" => break (true, false),
            "3" => break (false, true),
            "4" => break (true, true),
            other => println!("  {other:?} is not an option — pick 1 to 4."),
        }
    };

    let ssh_key = if want_key {
        loop {
            let got = prompt("Public key (ssh-ed25519 … / ssh-rsa …): ")?;
            let got = got.trim().to_string();
            match validate_ssh_key(&got) {
                Ok(()) => break got,
                Err(e) => println!("  {e:#}"),
            }
        }
    } else {
        String::new()
    };

    let root_password = if want_password {
        loop {
            let first = prompt_secret("Root password (not echoed): ")?;
            if first.len() < 12 {
                println!(
                    "  at least 12 characters — this is a console anybody standing at the machine can reach"
                );
                continue;
            }
            let again = prompt_secret("Again: ")?;
            if first == again {
                break first;
            }
            println!("  they differ — once more");
        }
    } else {
        String::new()
    };
    Ok((ssh_key, root_password))
}

/// What the review says about access, in both doors.
fn print_access(ssh_key: &str, root_password: &str) {
    match (ssh_key.is_empty(), root_password.is_empty()) {
        (true, true) => println!("  access:        none — console and SSH closed"),
        (false, true) => println!("  access:        SSH key only"),
        (true, false) => println!("  access:        console password (not echoed)"),
        (false, false) => println!("  access:        SSH key and console password"),
    }
}

/// One line, as `ssh-keygen` writes it. Checked here rather than at first
/// boot: a key with a newline in it is a seed that never produces a login, and
/// finding that out needs a second trip to the machine.
fn validate_ssh_key(key: &str) -> Result<()> {
    let mut parts = key.split_whitespace();
    let (Some(kind), Some(body)) = (parts.next(), parts.next()) else {
        bail!("a public key is `<type> <base64> [comment]` — this has fewer than two words");
    };
    if !kind.starts_with("ssh-") && !kind.starts_with("ecdsa-") && !kind.starts_with("sk-") {
        bail!("{kind:?} is not a key type — expected ssh-ed25519, ssh-rsa or similar");
    }
    if body.len() < 16
        || !body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    {
        bail!("the key body is not base64 — this looks like a private key or a truncated paste");
    }
    if key.contains('\n') {
        bail!("a public key is one line; this has a line break in it");
    }
    Ok(())
}

/// Door 2: the join token is every cloud answer, so only the disk answers
/// already given are needed — and the review, because the erase is still an
/// erase.
#[allow(clippy::too_many_arguments)]
fn join(
    chosen: Vec<&Disk>,
    raid: Raid,
    picks: Vec<usize>,
    passphrase: Option<String>,
    hostname: String,
    network: Network,
    // Asked once, in `collect`, before the door was chosen — the question is
    // about this machine and not about how it joins. Passed in rather than
    // asked again, which is what door 2 did the first time round.
    ssh_key: String,
    root_password: String,
) -> Result<Option<Answers>> {
    println!("\nPaste the join token the console showed when this node was created.");
    println!("It starts with `velstra1.` and is one line; a wrapped paste is fine.");
    let token = loop {
        let pasted = prompt("Join token: ")?;
        match velstra_cloud_wire::join::JoinToken::decode(&pasted) {
            Ok(t) => break t,
            Err(e) => println!("  {e}"),
        }
    };
    let mut roles = Vec::new();
    if !token.token.is_empty() {
        roles.push(crate::roles::Role::Hypervisor);
    }
    if token.pool.is_some() {
        roles.push(crate::roles::Role::Pool);
    }
    let (pool, pool_token) = token
        .pool
        .as_ref()
        .map(|p| (p.id.clone(), p.token.clone()))
        .unwrap_or_default();
    let api_url = token
        .urls
        .iter()
        .find(|u| !u.trim().is_empty())
        .cloned()
        .unwrap_or_default();

    println!();
    print_plan(&chosen, raid);
    if passphrase.is_some() {
        println!("  data partition: LUKS2 encrypted (passphrase asked at each boot)");
    }
    println!("\nThe installed node will come up as:");
    println!("  hostname:      {hostname}");
    println!(
        "  joining:       {} (cell {}, region {})",
        token.node, token.cell, token.region
    );
    println!("  control plane: {api_url}");
    println!("  roles:         {}", crate::roles::render_list(&roles));
    if !pool.is_empty() {
        println!("  pool:          {pool} (directory)");
    }
    println!("  vmm:           qemu");
    println!("  credentials:   (from the token — not echoed)");
    print_access(&ssh_key, &root_password);

    let confirm = prompt("\nThis ERASES the selected disk(s). Type YES to proceed: ")?;
    if confirm.trim() != "YES" {
        return Ok(None);
    }
    Ok(Some(Answers {
        raid,
        picks,
        passphrase,
        hostname,
        network,
        api_url,
        node: token.node,
        cell: token.cell,
        region: token.region,
        token: token.token,
        // The only hypervisor that can open a Ceph volume, and a joiner
        // cannot know what storage the cell will grow.
        vmm: "qemu".into(),
        roles,
        pool,
        pool_backend: if pool_token.is_empty() {
            String::new()
        } else {
            "directory".into()
        },
        pool_token,
        listen: String::new(),
        admin: String::new(),
        admin_password: String::new(),
        api_ca_pem: token.ca,
        ceph_osds: Vec::new(),
        ssh_key,
        root_password,
    }))
}

/// Print the candidate disks as a numbered table.
pub fn list_disks(disks: &[Disk]) {
    if disks.is_empty() {
        println!("no disks found");
        return;
    }
    println!("Candidate install disks:");
    for (i, d) in disks.iter().enumerate() {
        println!(
            "  [{}] {:<12} {:>10}  {}{}",
            i + 1,
            d.dev_path(),
            human_size(d.size),
            if d.model.is_empty() {
                "(no model)"
            } else {
                &d.model
            },
            if d.removable { "  [removable]" } else { "" },
        );
    }
}

/// Print the resolved install plan.
fn print_plan(chosen: &[&Disk], raid: Raid) {
    println!("Install plan ({raid:?}):");
    for d in chosen {
        println!("  target {} ({})", d.dev_path(), human_size(d.size));
    }
    println!("  layout: ESP + dm-verity store (sealed, read-only) + data partition");
    if let Some(level) = raid.mdadm_level() {
        println!("  data partition as mdadm RAID{level} across the targets");
    }
}

/// Map numbered picks (`"1 3"`) to disk indices.
fn resolve_picks(disks: &[Disk], picks: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for tok in picks.split_whitespace() {
        let i: usize = tok
            .parse()
            .map_err(|_| anyhow::anyhow!("not a number: {tok:?}"))?;
        let idx = i.wrapping_sub(1);
        if disks.get(idx).is_none() {
            bail!("no disk [{i}]");
        }
        out.push(idx);
    }
    if out.is_empty() {
        bail!("no disks selected");
    }
    Ok(out)
}

/// The LUKS passphrase: from $VELSTRA_NODE_LUKS_PASSPHRASE (scripted
/// installs), else prompted for twice without echo and checked to match.
/// Minimum 8 characters either way — an encrypted volume with a trivial
/// passphrase is worse than an honest plaintext one.
fn resolve_passphrase() -> Result<String> {
    if let Some(p) = std::env::var_os(product::LUKS_PASSPHRASE_ENV) {
        let p = p.to_string_lossy().into_owned();
        if p.chars().count() < 8 {
            bail!(
                "${} is set but shorter than 8 characters",
                product::LUKS_PASSPHRASE_ENV
            );
        }
        return Ok(p);
    }
    loop {
        let first = prompt_secret("Passphrase for the encrypted data partition: ")?;
        let first = first.trim_end_matches(['\n', '\r']).to_string();
        if first.chars().count() < 8 {
            println!("  the passphrase must be at least 8 characters.");
            continue;
        }
        let again = prompt_secret("Repeat the passphrase: ")?;
        let again = again.trim_end_matches(['\n', '\r']).to_string();
        if first != again {
            println!("  the passphrases did not match — try again.");
            continue;
        }
        break Ok(first);
    }
}

// ---- Prompt plumbing -------------------------------------------------------

/// Print a prompt and read one line. A closed stdin is an error, not an empty
/// answer: every caller here loops on invalid input, and looping on EOF would
/// spin forever.
pub(crate) fn prompt(msg: &str) -> Result<String> {
    use std::io::Write;
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .context("reading input")?;
    if n == 0 {
        bail!("input ended before the installer finished its questions");
    }
    Ok(line)
}

/// Ask a question with a default; an empty answer takes the default.
fn ask(msg: &str, default: &str) -> Result<String> {
    let shown = if default.is_empty() {
        format!("{msg}: ")
    } else {
        format!("{msg} [{default}]: ")
    };
    let got = prompt(&shown)?;
    let got = got.trim();
    Ok(if got.is_empty() {
        default.to_string()
    } else {
        got.to_string()
    })
}

/// Ask a yes/no question.
pub(crate) fn ask_yes(msg: &str, default_yes: bool) -> Result<bool> {
    let d = if default_yes { "Y/n" } else { "y/N" };
    let got = prompt(&format!("{msg} [{d}]: "))?;
    Ok(match got.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" | "j" | "ja" => true,
        _ => false,
    })
}

/// Ask with a validator, re-asking until the answer passes. The `hint` is a
/// sentence saying what a valid answer looks like — printed on the first
/// refusal, because "invalid" alone teaches nothing.
pub(crate) fn ask_valid(msg: &str, validate: fn(&str) -> Result<()>, hint: &str) -> Result<String> {
    ask_valid_or("", msg, validate, hint)
}

/// [`ask_valid`] with a default that an empty answer takes.
///
/// Separate because the two cannot be the same function by accident: a prompt
/// that prints a default and then refuses an empty answer with "the node name
/// cannot be empty" is a promise the code does not keep, and it was exactly that
/// — the default was applied *after* a loop that could never leave with an empty
/// string, so pressing Enter at the very first question of the installer
/// rejected itself, twice, before the operator gave up and typed the default out
/// by hand.
pub(crate) fn ask_valid_or(
    default: &str,
    msg: &str,
    validate: fn(&str) -> Result<()>,
    hint: &str,
) -> Result<String> {
    loop {
        let got = prompt(msg)?;
        let got = got.trim();
        let got = if got.is_empty() { default } else { got }.to_string();
        match validate(&got) {
            Ok(()) => break Ok(got),
            Err(e) => println!("  {e:#} — expected {hint}."),
        }
    }
}

/// [`ask_valid`], for an answer that must not appear on the console.
///
/// The same loop, reading through [`prompt_secret`]. Split out rather than
/// given a flag because the two differ in one more way than the echo: a
/// refusal here cannot quote what was typed back at the operator, which is
/// exactly what it must not do with a credential.
fn ask_valid_secret(msg: &str, validate: fn(&str) -> Result<()>, hint: &str) -> Result<String> {
    loop {
        let got = prompt_secret(msg)?;
        let got = got.trim().to_string();
        match validate(&got) {
            Ok(()) => break Ok(got),
            Err(e) => println!("  {e:#} — expected {hint}."),
        }
    }
}

/// Ask with a default and the shell-safety check, re-asking until it passes.
fn ask_safe(msg: &str, default: &str) -> Result<String> {
    loop {
        let got = ask(msg, default)?;
        match validate_safe_value(&got) {
            Ok(()) => break Ok(got),
            Err(e) => println!("  {e:#}"),
        }
    }
}

/// Ask for a secret: prompt, read one line, but do not echo what is typed.
///
/// An install is done over whatever console is at hand — a serial line, an
/// IPMI session, someone's laptop over a shoulder — and an echoed passphrase
/// stays in that scrollback long after the install is finished. The echo is
/// turned off with `stty`(1) rather than termios, because this workspace
/// carries no libc crate and the coreutils are already a hard dependency of
/// every other step; if stty is somehow missing the read still works, just
/// echoed, which is the honest failure mode.
///
/// The echo goes off **before** the prompt is printed, not after. Printing
/// first opens a window between the operator seeing the question and the
/// terminal being told to stay quiet, and anything arriving inside it — a
/// pasted token, a fast typist, an automated install — is echoed by the line
/// discipline before `stty` has run. The window is small and entirely real:
/// it is how the wizard check caught this, by answering the moment it saw the
/// prompt, which is also what a paste does.
pub(crate) fn prompt_secret(msg: &str) -> Result<String> {
    use std::io::Write;
    let is_term = std::io::stdin().is_terminal();
    if is_term {
        let _ = Command::new("stty").arg("-echo").status();
    }

    print!("{msg}");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if is_term {
        let _ = Command::new("stty").arg("echo").status();
        // The newline the user typed was swallowed with the echo; put it back
        // so the next prompt starts on its own line.
        println!();
    }
    read.context("reading input")?;
    // The line terminator is not part of the answer, and here it is not
    // harmless: a password carrying `\n` was sent as-is and the API refused the
    // whole sign-in with "control character (\u0000-\u001F) found while parsing
    // a string" — at the end of an install, after everything else had worked.
    //
    // Only the terminator. A password may legitimately end in a space, and
    // trimming one away would make a credential that works here and nowhere
    // else.
    Ok(strip_terminator(&line).to_string())
}

// ---- Validators ------------------------------------------------------------

/// The line terminator, and nothing else.
///
/// A password carrying its `\n` was sent as-is and the API refused the whole
/// sign-in with "control character (\u0000-\u001F) found while parsing a
/// string" — at the very end of an install, after everything else had worked. A
/// trailing space, by contrast, is a strange thing to put in a password and it
/// is theirs: trimming it away would make a credential that works at this prompt
/// and nowhere else.
pub(crate) fn strip_terminator(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(line)
}

/// Refuse a value the seed could not carry verbatim. `node.env` is read by
/// systemd's `EnvironmentFile`, whose quoting rules are their own small
/// language; instead of writing a correct quoter, the wizard refuses any value
/// that would need one. Whitespace, control characters, quotes, `$`,
/// backslash and backtick are the characters that change meaning somewhere
/// between systemd, a shell sourcing the file by hand, and a log line.
pub(crate) fn validate_safe_value(s: &str) -> Result<()> {
    if s.is_empty() {
        bail!("an empty value is not allowed here");
    }
    if let Some(c) = s
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || ['\'', '"', '\\', '`', '$'].contains(c))
    {
        bail!("{c:?} cannot appear in this value (it would need shell quoting in the seed)");
    }
    Ok(())
}

/// A control plane URL: http(s), and safe to write into the seed.
pub(crate) fn validate_url(s: &str) -> Result<()> {
    validate_safe_value(s)?;
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        bail!("{s:?} does not start with http:// or https://");
    }
    // A bare scheme is a typo, not a URL.
    if s == "http://" || s == "https://" {
        bail!("{s:?} names no host");
    }
    Ok(())
}

/// A node name: the id the operator created, `[a-z0-9-]`, non-empty.
/// What to offer as this machine's node name: its hostname.
///
/// The operator already chose it, it means something to them, and a platform
/// that suggests a name of its own is a platform naming somebody's estate. The
/// first label only — a node name is an identifier in this cell, not a fully
/// qualified domain — lowercased, and anything a node name cannot hold dropped.
///
/// `None` when nothing usable comes out (a hostname of `localhost`, an empty
/// one, one that is all punctuation). Then the question is asked with no
/// default, which is better than inventing one: naming machines is the
/// operator's business, and a wrong suggestion accepted by pressing Enter is
/// harder to undo than a question answered.
pub(crate) fn suggested_node_name(hostname: &str) -> Option<String> {
    let first = hostname.split('.').next().unwrap_or("").trim();
    let cleaned: String = first
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    // `localhost` names every machine and therefore none of them; offering it
    // would put the same node name on every box in a fleet.
    if cleaned.is_empty() || cleaned == "localhost" {
        return None;
    }
    Some(cleaned)
}

/// This machine's hostname, or an empty string if it cannot be read.
pub(crate) fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn validate_node_name(s: &str) -> Result<()> {
    if s.is_empty() {
        bail!("the node name cannot be empty");
    }
    if let Some(c) = s
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        bail!("{c:?} cannot appear in a node name");
    }
    Ok(())
}

/// A registration token: exactly 64 lowercase hex characters, as the API
/// mints them. Anything else is a paste error, and finding that out here
/// beats finding it out as a 401 on the node's first boot.
pub(crate) fn validate_token(s: &str) -> Result<()> {
    if s.len() != 64 {
        bail!("the token is {} characters, not 64", s.len());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        bail!("the token contains characters outside lowercase hex");
    }
    Ok(())
}

/// An IPv4 address in CIDR form, `a.b.c.d/len` with 1 ≤ len ≤ 32.
pub(crate) fn validate_cidr(s: &str) -> Result<()> {
    let Some((addr, len)) = s.split_once('/') else {
        bail!("{s:?} has no /prefix-length");
    };
    validate_ipv4(addr)?;
    let len: u8 = len
        .parse()
        .map_err(|_| anyhow::anyhow!("{len:?} is not a prefix length"))?;
    if !(1..=32).contains(&len) {
        bail!("/{len} is not a usable prefix length");
    }
    Ok(())
}

/// A plain IPv4 address.
pub(crate) fn validate_ipv4(s: &str) -> Result<()> {
    s.parse::<Ipv4Addr>()
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("{s:?} is not an IPv4 address"))
}

/// A VTEP address: either family, because the underlay is the operator's and
/// plenty of them are v6-only.
///
/// An address rather than a name on purpose. This value is what *other* hosts
/// send encapsulated frames to, and a name resolved here would be resolved
/// against this machine's resolver — which is not the one that has to agree.
pub(crate) fn validate_ip(s: &str) -> Result<()> {
    validate_safe_value(s)?;
    s.parse::<std::net::IpAddr>()
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("{s:?} is not an IP address"))
}

/// A network interface name, as the kernel will accept it.
pub(crate) fn validate_interface(s: &str) -> Result<()> {
    if s.is_empty() {
        bail!("the interface name cannot be empty");
    }
    // IFNAMSIZ is 16 including the terminator, so 15 is the real ceiling.
    if s.len() > 15 {
        bail!("{s:?} is longer than the 15 characters an interface name can have");
    }
    if let Some(c) = s
        .chars()
        .find(|c| c.is_whitespace() || *c == '/' || *c == ':')
    {
        bail!("{c:?} cannot appear in an interface name");
    }
    validate_safe_value(s)
}

/// An SRv6 locator: an IPv6 prefix with a length, e.g. `fc00:0:1::/64`.
///
/// v6 specifically, and not [`validate_cidr`], which is v4: a locator is a
/// slice of an IPv6 address plan and there is no v4 spelling of one.
pub(crate) fn validate_srv6_locator(s: &str) -> Result<()> {
    validate_safe_value(s)?;
    let Some((addr, len)) = s.split_once('/') else {
        bail!("{s:?} has no /prefix-length");
    };
    addr.parse::<std::net::Ipv6Addr>()
        .map_err(|_| anyhow::anyhow!("{addr:?} is not an IPv6 address"))?;
    let len: u8 = len
        .parse()
        .map_err(|_| anyhow::anyhow!("{len:?} is not a prefix length"))?;
    // Shorter than /32 is not a locator anybody routes; /128 leaves no room for
    // the function bits every service SID needs.
    if !(32..=112).contains(&len) {
        bail!("/{len} is not a usable locator length — 32 to 112");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_exactly_64_lowercase_hex_chars() {
        let good = "0123456789abcdef".repeat(4);
        assert!(validate_token(&good).is_ok());
        // Too short, uppercase, and non-hex are each refused.
        assert!(validate_token(&good[..63]).is_err());
        assert!(validate_token(&good.to_uppercase()).is_err());
        let mut bad = good.clone();
        bad.replace_range(0..1, "g");
        assert!(validate_token(&bad).is_err());
    }

    #[test]
    fn urls_must_be_http_or_https_with_a_host() {
        assert!(validate_url("https://cloud.example.net").is_ok());
        assert!(validate_url("http://10.0.0.1:8080").is_ok());
        assert!(validate_url("ftp://cloud.example.net").is_err());
        assert!(validate_url("cloud.example.net").is_err());
        assert!(validate_url("https://").is_err());
        // Shell-unsafe URLs are refused too — they land in node.env verbatim.
        assert!(validate_url("https://a b").is_err());
    }

    #[test]
    fn node_names_are_lowercase_alphanumeric_dashes() {
        assert!(validate_node_name("node-7").is_ok());
        assert!(validate_node_name("n0de").is_ok());
        assert!(validate_node_name("").is_err());
        assert!(validate_node_name("Node-7").is_err());
        assert!(validate_node_name("node_7").is_err());
        assert!(validate_node_name("node 7").is_err());
    }

    #[test]
    fn cidr_addresses_are_checked_octet_by_octet() {
        assert!(validate_cidr("192.0.2.10/24").is_ok());
        assert!(validate_cidr("10.0.0.1/8").is_ok());
        assert!(validate_cidr("192.0.2.10").is_err(), "no prefix length");
        assert!(validate_cidr("192.0.2.256/24").is_err(), "octet overflow");
        assert!(validate_cidr("192.0.2.10/0").is_err(), "unusable prefix");
        assert!(validate_cidr("192.0.2.10/33").is_err(), "prefix too long");
        assert!(validate_cidr("not-an-ip/24").is_err());
    }

    #[test]
    fn plain_ipv4_addresses_are_checked() {
        assert!(validate_ipv4("192.0.2.1").is_ok());
        assert!(validate_ipv4("192.0.2.1/24").is_err());
        assert!(
            validate_ipv4("2001:db8::1").is_err(),
            "IPv6 is not the uplink family here"
        );
        assert!(validate_ipv4("gateway").is_err());
    }

    /// A VTEP takes either family — plenty of underlays are v6-only — but it is
    /// an address, never a name: this value is resolved by nobody, it is what
    /// other hosts send frames to.
    #[test]
    fn vtep_addresses_take_either_family_but_not_a_name() {
        assert!(validate_ip("10.0.0.7").is_ok());
        assert!(validate_ip("2001:db8::1").is_ok());
        assert!(validate_ip("underlay.example").is_err());
        assert!(validate_ip("10.0.0.7/32").is_err());
    }

    #[test]
    fn interface_names_fit_what_the_kernel_accepts() {
        assert!(validate_interface("eth1").is_ok());
        assert!(validate_interface("bond0.100").is_ok());
        assert!(validate_interface("").is_err());
        // IFNAMSIZ is 16 including the terminator.
        assert!(validate_interface(&"e".repeat(15)).is_ok());
        assert!(validate_interface(&"e".repeat(16)).is_err());
        assert!(validate_interface("eth 1").is_err());
        assert!(validate_interface("eth/1").is_err());
    }

    /// A locator is IPv6 and nothing else: it is a slice of an IPv6 address
    /// plan, and there is no v4 spelling of one.
    #[test]
    fn srv6_locators_are_ipv6_prefixes_with_a_usable_length() {
        assert!(validate_srv6_locator("fc00:0:1::/64").is_ok());
        assert!(validate_srv6_locator("2001:db8:1::/48").is_ok());
        assert!(validate_srv6_locator("10.0.0.0/24").is_err());
        assert!(validate_srv6_locator("fc00:0:1::").is_err());
        // /128 leaves no room for the function bits every service SID needs.
        assert!(validate_srv6_locator("fc00:0:1::/128").is_err());
        assert!(validate_srv6_locator("fc00:0:1::/8").is_err());
    }

    #[test]
    fn seed_values_refuse_anything_that_would_need_quoting() {
        assert!(validate_safe_value("velstra-node").is_ok());
        assert!(validate_safe_value("cell-1").is_ok());
        assert!(validate_safe_value("").is_err());
        assert!(validate_safe_value("two words").is_err());
        assert!(validate_safe_value("tab\there").is_err());
        assert!(validate_safe_value("quo'te").is_err());
        assert!(validate_safe_value("quo\"te").is_err());
        assert!(validate_safe_value("dollar$sign").is_err());
        assert!(validate_safe_value("back\\slash").is_err());
        assert!(validate_safe_value("ctrl\u{7}char").is_err());
    }

    #[test]
    fn picks_resolve_to_indices_and_refuse_nonsense() {
        let disks = vec![
            Disk {
                name: "sda".into(),
                size: 1,
                model: String::new(),
                removable: false,
            },
            Disk {
                name: "sdb".into(),
                size: 1,
                model: String::new(),
                removable: false,
            },
        ];
        assert_eq!(resolve_picks(&disks, "1 2").unwrap(), vec![0, 1]);
        assert!(resolve_picks(&disks, "3").is_err(), "out of range");
        assert!(resolve_picks(&disks, "zero").is_err(), "not a number");
        assert!(resolve_picks(&disks, "").is_err(), "nothing selected");
    }
}

#[cfg(test)]
mod reading_an_answer {
    use super::*;

    #[test]
    fn a_password_does_not_carry_the_newline_that_ended_it() {
        // The failure it caused, verbatim, at the last step of `quickstart`:
        //
        //   signing in as admin did not work: Failed to parse the request body
        //   as JSON: password: control character (\u0000-\u001F) found while
        //   parsing a string
        assert_eq!(strip_terminator("hunter2hunter2\n"), "hunter2hunter2");
        assert_eq!(strip_terminator("hunter2hunter2\r\n"), "hunter2hunter2");
        assert_eq!(strip_terminator("hunter2hunter2"), "hunter2hunter2");
    }

    #[test]
    fn the_node_name_offered_is_the_machines_own() {
        // `home-1` was a laboratory name in a product meant to run somebody's
        // estate. What a professional default looks like is the name the
        // operator already gave the machine.
        assert_eq!(suggested_node_name("hv-fra-03"), Some("hv-fra-03".into()));
        // The first label: a node name is an identifier in this cell, not a
        // fully qualified domain.
        assert_eq!(
            suggested_node_name("hv-fra-03.dc2.example.com"),
            Some("hv-fra-03".into())
        );
        assert_eq!(suggested_node_name("HV-FRA-03"), Some("hv-fra-03".into()));
        assert_eq!(suggested_node_name("hv_fra_03"), Some("hvfra03".into()));
    }

    #[test]
    fn and_nothing_at_all_when_the_hostname_says_nothing() {
        // `localhost` names every machine and so none of them: offering it would
        // put one node name on every box in a fleet. Better to ask.
        assert_eq!(suggested_node_name("localhost"), None);
        assert_eq!(suggested_node_name("localhost.localdomain"), None);
        assert_eq!(suggested_node_name(""), None);
        assert_eq!(suggested_node_name("___"), None);
    }

    #[test]
    fn and_nothing_a_person_may_have_meant() {
        // A trailing space is a strange thing to put in a password and it is
        // theirs. Trimming it would make a credential that works at this prompt
        // and nowhere else.
        assert_eq!(strip_terminator("with a space \n"), "with a space ");
        assert_eq!(strip_terminator("  padded  \n"), "  padded  ");
    }
}
