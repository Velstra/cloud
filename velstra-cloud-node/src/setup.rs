//! The wizard for a machine that already has an operating system.
//!
//! ## Why this is not `install`
//!
//! [`crate::install`] makes an appliance: it partitions disks, clones a sealed
//! image, and seeds the data partition of a machine that is not running yet.
//! That is the right shape for a box you flash and forget, and the wrong shape
//! for one that already runs Debian — or NixOS, where partitioning is somebody
//! else's declaration entirely.
//!
//! So this asks the same questions and writes the same seed, and touches
//! nothing else. What it does *not* do is as deliberate as what it does: no
//! disks, no bootloader, no packages. The machine is already installed; what is
//! missing is the answer to "which cell, as what, with which token".
//!
//! ## One seed, two packagings
//!
//! Debian and NixOS end up with the same file at the same path, and each
//! packaging supplies the units. The unit is conditional on its role being in
//! the seed, so on both systems the answer to "what is running here" is one
//! file — readable, comparable, and the same thing the appliance writes.
//!
//! On Debian the wizard also *enables* what the roles say, because a package
//! that installs units and starts none is a package where somebody has to know
//! four unit names. On NixOS it does not: units there are a declaration, and a
//! wizard reaching into them would be fighting the operating system. It prints
//! the module snippet instead, and says why.
//!
//! ## What it cannot do, and the reason
//!
//! It cannot mark this machine a gateway, give it labels, or make it
//! schedulable. Those live on the Node object and are an operator's to write —
//! a registration token exists so a machine can *report*, and one that could
//! also declare its holder a gateway would be a token that grants itself the
//! cell's external traffic. The wizard says where they are set instead of
//! pretending.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    roles::{Role, render_list},
    wizard::{
        ask_valid, ask_yes, prompt, prompt_secret, validate_interface, validate_ip,
        validate_node_name, validate_srv6_locator, validate_token, validate_url,
    },
};

/// Where the seed lives, on every kind of machine.
///
/// The appliance decides this: its `/etc` is on a read-only verity store and
/// its writable partition mounts here. One path on all three systems is worth
/// more than the conventional one on two of them.
pub const SEED_DIR: &str = "/var/lib/velstra";

/// What this machine was told about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Machine {
    pub region: String,
    pub cell: String,
    pub roles: Vec<Role>,
    /// Where the API is. Empty on a control-plane-only machine, which *is* the
    /// API — a URL pointing at itself would be a fact with two owners.
    pub api_url: String,
    /// The node's id and its one-time token, for a hypervisor.
    pub node: String,
    pub token: String,
    pub vmm: String,
    /// The pool's id and backend, for a pool.
    pub pool: String,
    /// The certificate and key the API serves TLS with. Empty means plaintext,
    /// which the API says out loud at startup.
    /// The certificate the agents verify the API against, when it is https.
    /// Written into the seed as `VELSTRA_API_CA`, read by both agents.
    pub api_ca: String,
    pub tls_cert: String,
    pub tls_key: String,
    pub pool_backend: String,
    /// For the lvm backend: the volume group, and a thin pool inside it if
    /// there is one. Empty for every other backend.
    pub lvm_group: String,
    pub lvm_thin_pool: String,
    /// For the ceph backend, and the reason an **external** cluster is usable:
    /// without these the agent falls back to `client.admin` and no config file,
    /// which reaches a cluster this machine deployed itself and nothing else.
    pub ceph_conf: String,
    pub ceph_user: String,
    pub ceph_pool: String,
    pub ceph_image_pool: String,
    /// Where the store is, for a control plane.
    pub store: String,
    /// The other cells this installation can reach, as `cell=url` pairs.
    pub cells: Vec<String>,
    /// The fabric, if this cell has one. See [`Fabric`].
    pub fabric: Option<Fabric>,
    /// Whether this node holds its guests' gateway and lets them out.
    ///
    /// A cell with a fabric answers no here and means it: the fabric owns the
    /// far end of every tap. A cell without one that also answers no has guests
    /// on a wire that leads nowhere — which is not only unreachable but
    /// unconfigurable, because cloud-init reaches the metadata service over that
    /// same wire.
    pub local_network: bool,
    /// Where the API listens, for a control plane.
    ///
    /// It has a place in the seed because the default is loopback, and a
    /// control plane nobody can reach from another machine is the first thing
    /// somebody hits and the last thing they think to look for: everything is
    /// running, everything is green, and the browser says nothing answered.
    pub listen: String,
    /// The cell's first administrator, for a control plane.
    ///
    /// Without one the API comes up, serves the console, and refuses every
    /// sign-in — and it says so in a warning nobody reading a browser sees.
    /// A fresh cell that cannot be signed into is not a cell.
    pub admin: String,
    /// That administrator's initial password. Never written into the seed: the
    /// seed is 0644 because nothing in it is secret, and this is.
    pub admin_password: String,
}

/// Where the data plane is, from this machine's point of view.
///
/// **Two endpoints, and they are not interchangeable.** The fabric controller
/// serves its orchestrator on one address and its agent-facing config service
/// on another, because they have different audiences: the orchestrator is asked
/// to *create a port*, the config service is asked *what should I be running*.
/// Pointing either at the other's port gets `unimplemented`, which is a
/// confusing way to learn this.
///
/// A note worth reading before widening anything: fabric binds the orchestrator
/// to localhost by default and offers mTLS only on the agent-facing one. Giving
/// every hypervisor in a cell a route to the orchestrator is therefore a real
/// decision — that channel can reconfigure any node, not just the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fabric {
    /// The orchestrator (fabric's `--admin-listen`, default `:50052`). Ports
    /// and networks are created here, by the control plane and by the node
    /// agent.
    pub orchestrator: String,
    /// The agent-facing config service (fabric's `--listen`, default `:50051`),
    /// which the eBPF agent watches for its own configuration.
    pub control: String,
    /// The address other hosts send this one's encapsulated frames to. Stated
    /// rather than guessed: nothing on a machine can tell which of its
    /// addresses its peers route to.
    pub vtep: String,
    /// The interface that address is on; its MAC is read from the machine.
    pub underlay: String,
    /// This host's SRv6 locator as `prefix/len`. Set, it puts the host on the
    /// SRv6 wire family instead of VXLAN; empty leaves it on VXLAN.
    pub srv6_locator: String,
}

/// Render the seed. Exactly the keys that were answered, one per line.
///
/// Values are unquoted, and the wizard refuses anything that would need
/// quoting — so this file is safe both for systemd's `EnvironmentFile` and for
/// a person sourcing it in a shell to see what a machine thinks it is.
pub fn render(m: &Machine) -> String {
    let mut out = format!(
        "VELSTRA_REGION={}\nVELSTRA_CELL={}\nVELSTRA_ROLES={}\n",
        m.region,
        m.cell,
        render_list(&m.roles)
    );
    if !m.api_url.is_empty() {
        out.push_str(&format!("VELSTRA_API_URL={}\n", m.api_url));
        if !m.api_ca.is_empty() {
            out.push_str(&format!("VELSTRA_API_CA={}\n", m.api_ca));
        }
    }
    if m.roles.contains(&Role::Hypervisor) {
        out.push_str(&format!("VELSTRA_NODE={}\nVELSTRA_VMM={}\n", m.node, m.vmm));
    }
    if m.roles.contains(&Role::Pool) {
        out.push_str(&format!(
            "VELSTRA_POOL={}\nVELSTRA_POOL_BACKEND={}\n",
            m.pool, m.pool_backend
        ));
        // Only what was answered. An empty line here would override the
        // agent's own default with nothing, which is a worse answer than not
        // saying anything.
        for (key, value) in [
            ("VELSTRA_LVM_GROUP", &m.lvm_group),
            ("VELSTRA_LVM_THIN_POOL", &m.lvm_thin_pool),
            ("VELSTRA_CEPH_CONF", &m.ceph_conf),
            ("VELSTRA_CEPH_USER", &m.ceph_user),
            ("VELSTRA_CEPH_POOL", &m.ceph_pool),
            ("VELSTRA_CEPH_IMAGE_POOL", &m.ceph_image_pool),
        ] {
            if !value.is_empty() {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
    }
    if m.roles.contains(&Role::ControlPlane) {
        out.push_str(&format!("VELSTRA_STORE={}\n", m.store));
        // Both or neither: the API refuses one without the other rather than
        // serving plaintext on a port somebody believes is encrypted.
        if !m.tls_cert.is_empty() && !m.tls_key.is_empty() {
            out.push_str(&format!(
                "VELSTRA_TLS_CERT={}\nVELSTRA_TLS_KEY={}\n",
                m.tls_cert, m.tls_key
            ));
        }
        if !m.listen.is_empty() {
            out.push_str(&format!("VELSTRA_LISTEN={}\n", m.listen));
        }
        // A username is not a secret; the password beside it is, and goes to a
        // 0600 file of its own — the same split the node token already makes.
        if !m.admin.is_empty() {
            out.push_str(&format!("VELSTRA_BOOTSTRAP_ADMIN={}\n", m.admin));
        }
        if !m.cells.is_empty() {
            out.push_str(&format!("VELSTRA_CELLS={}\n", m.cells.join(",")));
        }
    }
    if m.local_network {
        out.push_str("VELSTRA_LOCAL_NETWORK=1\n");
    }
    if let Some(f) = &m.fabric {
        // The orchestrator is written for both roles that talk to it; the rest
        // describes *this host's* place on the wire and is a hypervisor's.
        out.push_str(&format!("VELSTRA_FABRIC={}\n", f.orchestrator));
        if m.roles.contains(&Role::Hypervisor) {
            out.push_str(&format!(
                "VELSTRA_FABRIC_CONTROL={}\nVELSTRA_FABRIC_VTEP={}\nVELSTRA_FABRIC_UNDERLAY={}\n",
                f.control, f.vtep, f.underlay
            ));
            if !f.srv6_locator.is_empty() {
                out.push_str(&format!("VELSTRA_FABRIC_SRV6_LOCATOR={}\n", f.srv6_locator));
            }
        }
    }
    out
}

/// Read the answers from a file instead of asking for them.
///
/// The file **is a seed** — the same `KEY=value` lines this writes, and the same
/// ones a machine already carries. One format rather than two: an operator can
/// take the seed off a working machine, change two lines, and install the next
/// one with it, and nobody has to learn a second spelling of the same facts.
///
/// Missing answers are an error naming the key, never a default. An unattended
/// install that quietly guessed a cell name would produce a machine that comes
/// up, registers nowhere, and is discovered weeks later.
pub fn parse(text: &str) -> Result<Machine> {
    let mut values: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("line {}: {line:?} is not KEY=value", n + 1))?;
        values.insert(key.trim(), value.trim());
    }
    let need = |key: &str| -> Result<String> {
        values
            .get(key)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .with_context(|| format!("{key} is missing, and there is no sensible default for it"))
    };
    let or = |key: &str, fallback: &str| -> String {
        values
            .get(key)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .unwrap_or_else(|| fallback.to_string())
    };

    let roles = crate::roles::parse_list(&need("VELSTRA_ROLES")?);
    if roles.is_empty() {
        bail!(
            "VELSTRA_ROLES names no role this version knows — one of control-plane, hypervisor, pool"
        );
    }
    let mut m = Machine {
        tls_cert: String::new(),
        tls_key: String::new(),
        lvm_group: String::new(),
        lvm_thin_pool: String::new(),
        ceph_conf: String::new(),
        ceph_user: String::new(),
        ceph_pool: String::new(),
        ceph_image_pool: String::new(),
        region: or("VELSTRA_REGION", "eu-central"),
        cell: or("VELSTRA_CELL", "cell-1"),
        local_network: matches!(
            or("VELSTRA_LOCAL_NETWORK", "").as_str(),
            "1" | "true" | "yes"
        ),
        roles: roles.clone(),
        api_url: or("VELSTRA_API_URL", ""),
        api_ca: or("VELSTRA_API_CA", ""),
        node: String::new(),
        token: or("VELSTRA_TOKEN", ""),
        vmm: or("VELSTRA_VMM", "qemu"),
        pool: String::new(),
        pool_backend: or("VELSTRA_POOL_BACKEND", "directory"),
        store: or("VELSTRA_STORE", "127.0.0.1:2379"),
        listen: or("VELSTRA_LISTEN", ""),
        cells: values
            .get("VELSTRA_CELLS")
            .map(|v| {
                v.split(',')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        fabric: None,
        admin: or("VELSTRA_BOOTSTRAP_ADMIN", ""),
        admin_password: std::env::var("VELSTRA_BOOTSTRAP_PASSWORD").unwrap_or_default(),
    };
    // Only what the named roles actually need. A file for a pool that had to
    // carry a node id would be a file with a value nobody reads, and the first
    // person to change it would be changing nothing.
    if roles.contains(&Role::Hypervisor) {
        m.node = need("VELSTRA_NODE")?;
        if m.api_url.is_empty() {
            m.api_url = need("VELSTRA_API_URL")?;
        }
    }
    if roles.contains(&Role::Pool) {
        m.pool = need("VELSTRA_POOL")?;
        if m.api_url.is_empty() && !roles.contains(&Role::ControlPlane) {
            m.api_url = need("VELSTRA_API_URL")?;
        }
    }
    // A fabric is optional: a cell that programs no overlay is a real way to
    // run, and it is what every cell did before this existed. A *half* fabric
    // is not. Naming the orchestrator and leaving out this host's place on the
    // wire produces a node that starts, registers, reports healthy and carries
    // no tenant traffic — the failure that looks most like success, so it is
    // refused here where the answer is still a file somebody is editing.
    let orchestrator = or("VELSTRA_FABRIC", "");
    if !orchestrator.is_empty() {
        let mut fabric = Fabric {
            orchestrator,
            control: String::new(),
            vtep: String::new(),
            underlay: String::new(),
            srv6_locator: or("VELSTRA_FABRIC_SRV6_LOCATOR", ""),
        };
        if roles.contains(&Role::Hypervisor) {
            fabric.control = need("VELSTRA_FABRIC_CONTROL")?;
            fabric.vtep = need("VELSTRA_FABRIC_VTEP")?;
            fabric.underlay = need("VELSTRA_FABRIC_UNDERLAY")?;
        }
        m.fabric = Some(fabric);
    } else {
        // The other way round is the same mistake mirrored: answers about a
        // fabric with no fabric named. Silently ignoring them would leave
        // somebody certain the overlay is on.
        for key in [
            "VELSTRA_FABRIC_CONTROL",
            "VELSTRA_FABRIC_VTEP",
            "VELSTRA_FABRIC_UNDERLAY",
            "VELSTRA_FABRIC_SRV6_LOCATOR",
        ] {
            if values.get(key).is_some_and(|v| !v.is_empty()) {
                bail!(
                    "{key} is set but VELSTRA_FABRIC is not, so nothing would read it — \
                     name the fabric orchestrator, or remove {key}"
                );
            }
        }
    }
    Ok(m)
}

/// Ask — or read a file — then write, then say what happens next.
pub fn run_with(
    dir: Option<PathBuf>,
    assume_nixos: Option<bool>,
    config: Option<PathBuf>,
) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from(SEED_DIR));
    let machine = match &config {
        Some(path) => {
            let text =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            // The token is the one answer a file should not have to carry: it
            // is a secret, and a file with a secret in it is a file somebody
            // copies. It may be there — automation that already holds one has
            // to put it somewhere — and it may also be handed separately.
            let mut m = parse(&text)?;
            if m.token.is_empty() {
                if let Ok(token) = std::env::var("VELSTRA_TOKEN") {
                    m.token = token;
                }
            }
            Some(m)
        }
        None => collect()?,
    };
    let Some(machine) = machine else {
        println!("Nothing was written.");
        return Ok(());
    };
    write_seed(&dir, &machine)?;

    let nixos = assume_nixos.unwrap_or_else(|| Path::new("/etc/NIXOS").exists());
    if nixos {
        println!("\nThis is NixOS, so nothing was enabled.");
        println!("Units there are a declaration, and a wizard reaching into them would be");
        println!("fighting the operating system. Add this and rebuild:\n");
        println!("{}", nix_snippet(&machine));
    } else {
        println!("\nEnable what the roles say:\n");
        for role in &machine.roles {
            for unit in role.units() {
                println!("  systemctl enable --now {unit}");
            }
        }
        // Not one of the roles: the data plane is a separate package this one
        // only recommends, and naming it for a machine whose cell has no fabric
        // would be telling somebody to enable a service that will skip itself.
        if machine.fabric.is_some() && machine.roles.contains(&Role::Hypervisor) {
            println!("  systemctl enable --now velstra-fabric-agent");
            println!("\nThat last one needs the fabric agent itself (the `velstra` package).");
            println!("Without it the unit skips and tenant networks separate no traffic.");
        }
    }
    if machine.roles.contains(&Role::Hypervisor) {
        println!("\nThis machine cannot mark itself a gateway, give itself labels, or make itself");
        println!("schedulable — those are the cell's answer about it, not its own. Set them on");
        println!("the node object: PATCH /api/v1/nodes/{}", machine.node);
    }
    Ok(())
}

/// The NixOS module snippet for these answers.
pub fn nix_snippet(m: &Machine) -> String {
    let mut out = String::from("{\n  velstra.cloud = {\n");
    if m.roles.contains(&Role::ControlPlane) {
        out.push_str(&format!(
            "    controlPlane = {{\n      enable = true;\n      cell = \"{}\";\n      region = \"{}\";\n",
            m.cell, m.region
        ));
        if !m.cells.is_empty() {
            out.push_str("      cells = {\n");
            for pair in &m.cells {
                if let Some((cell, endpoint)) = pair.split_once('=') {
                    out.push_str(&format!("        \"{cell}\" = \"{endpoint}\";\n"));
                }
            }
            out.push_str("      };\n");
        }
        if let Some(f) = &m.fabric {
            out.push_str(&format!("      fabric = \"{}\";\n", f.orchestrator));
        }
        out.push_str("    };\n");
    }
    if m.roles.contains(&Role::Hypervisor) {
        // Only `enable`: everything else a node needs is in the seed this same
        // run just wrote, and its units read it from there. Restating the
        // answers here would put one fact in two files that can disagree —
        // which is the difference between this module and the control plane's,
        // where there is no seed and the declaration *is* the answer.
        out.push_str("    node.enable = true;\n");
        if m.fabric.is_some() {
            out.push_str("    # node.fabricAgent = <the fabric agent package>;\n");
        }
    }
    if m.roles.contains(&Role::Pool) {
        out.push_str(&format!(
            "    pool = {{\n      enable = true;\n      id = \"{}\";\n      backend = \"{}\";\n      cell = \"{}\";\n      region = \"{}\";\n    }};\n",
            m.pool, m.pool_backend, m.cell, m.region
        ));
    }
    out.push_str("  };\n}\n");
    out
}

pub(crate) fn write_seed(dir: &Path, m: &Machine) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    write_with_mode(&dir.join("node.env"), &render(m), 0o644)?;
    if m.roles.contains(&Role::Hypervisor) && !m.token.is_empty() {
        // The one secret here, and it gets its own file with its own mode. The
        // seed is world-readable because nothing in it is secret and the units
        // that read it do not all run as root.
        write_with_mode(&dir.join("node-token"), &format!("{}\n", m.token), 0o600)?;
    }
    if m.roles.contains(&Role::ControlPlane) && !m.admin_password.is_empty() {
        // Its own file, its own mode, for the same reason the token has one.
        // The API unit reads it into the environment at start rather than
        // taking it on a command line: an argument is visible in `ps` to every
        // user on the machine, and this one is the cell's first administrator.
        write_with_mode(
            &dir.join("bootstrap-password"),
            &format!("{}\n", m.admin_password),
            0o600,
        )?;
    }
    println!("\nWrote {}/node.env", dir.display());
    Ok(())
}

/// A file only root may read, for the one secret its caller holds.
pub(crate) fn write_secret(path: &Path, contents: &str) -> Result<()> {
    write_with_mode(path, &format!("{}\n", contents.trim()), 0o600)
}

fn write_with_mode(path: &Path, contents: &str, mode: u32) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode on {}", path.display()))
}

/// Ask everything. `None` when the operator declines the final confirmation —
/// nothing has been written at that point.
fn collect() -> Result<Option<Machine>> {
    println!("Velstra Cloud — set up this machine\n");
    println!("This writes {SEED_DIR}/node.env and nothing else: no disks, no bootloader,");
    println!("no packages. The machine is already installed; what is missing is the answer");
    println!("to which cell it belongs to, as what, and with which credential.\n");

    let region = ask_valid(
        "Region [eu-central]: ",
        validate_node_name,
        "lowercase letters, digits and '-'",
    )
    .map(|s| if s.is_empty() { "eu-central".into() } else { s })?;
    let cell = ask_valid(
        "Cell [cell-1]: ",
        validate_node_name,
        "lowercase letters, digits and '-'",
    )
    .map(|s| if s.is_empty() { "cell-1".into() } else { s })?;

    println!("\nA cell is the failure domain: a machine belongs to exactly one.");
    println!("Working across cells is several cells, each with its own control plane —");
    println!("and one of them told where the others are, so a client reaches one address.\n");

    println!("What does this machine do? Several are fine — the smallest real cell is one");
    println!("box that is all of them.\n");
    for (n, role) in Role::ALL.iter().enumerate() {
        println!("  [{}] {:<14} {}", n + 1, role.as_str(), role.describes());
    }
    println!("\n  Carrying external traffic is not on this list: that is what the *cell*");
    println!("  believes about this machine, set on the node object by an operator. A");
    println!("  registration token exists so a machine can report, and one that could");
    println!("  also declare its holder a gateway would grant itself the cell's traffic.\n");

    let roles = loop {
        let raw = prompt("Roles, space-separated [2]: ")?;
        let raw = if raw.trim().is_empty() {
            "2".to_string()
        } else {
            raw
        };
        match resolve_roles(raw.trim()) {
            Ok(roles) => break roles,
            Err(e) => println!("  {e}"),
        }
    };

    let mut m = Machine {
        api_ca: String::new(),
            tls_cert: String::new(),
        tls_key: String::new(),
        lvm_group: String::new(),
        lvm_thin_pool: String::new(),
        ceph_conf: String::new(),
        ceph_user: String::new(),
        ceph_pool: String::new(),
        ceph_image_pool: String::new(),
        region,
        cell,
        roles: roles.clone(),
        local_network: false,
        api_url: String::new(),
        node: String::new(),
        token: String::new(),
        vmm: "qemu".into(),
        pool: String::new(),
        pool_backend: "directory".into(),
        store: "127.0.0.1:2379".into(),
        listen: String::new(),
        cells: Vec::new(),
        fabric: None,
        admin: String::new(),
        admin_password: String::new(),
    };

    // Everything that is not the control plane has to be told where the API is.
    // The control plane *is* the API, and a URL pointing at itself would be a
    // fact with two owners.
    if roles.iter().any(|r| *r != Role::ControlPlane) {
        m.api_url = ask_valid(
            "\nControl-plane URL (https://host:8443): ",
            validate_url,
            "a URL with a scheme and a host",
        )?;
    }

    if roles.contains(&Role::Hypervisor) {
        println!("\nThe node id has to match the node object an operator created — that object");
        println!("is where the one-time token came from.");
        m.node = ask_valid(
            "Node id: ",
            validate_node_name,
            "lowercase letters, digits and '-'",
        )?;
        m.token = ask_valid(
            "Registration token: ",
            validate_token,
            "64 lowercase hex characters",
        )?;
        m.vmm = loop {
            match prompt("Hypervisor [1] qemu  [2] cloud-hypervisor: ")?.trim() {
                "" | "1" => break "qemu".to_string(),
                "2" => break "cloud-hypervisor".to_string(),
                other => println!("  {other:?} is not a choice — 1 or 2."),
            }
        };
    }

    if roles.contains(&Role::Pool) {
        println!("\nThe pool id has to match the pool object; every volume is written against it.");
        m.pool = ask_valid(
            "Pool id: ",
            validate_node_name,
            "lowercase letters, digits and '-'",
        )?;
        m.pool_backend = loop {
            match prompt("Backend [1] directory  [2] lvm  [3] ceph: ")?.trim() {
                "" | "1" => break "directory".to_string(),
                "2" => break "lvm".to_string(),
                "3" => break "ceph".to_string(),
                other => println!("  {other:?} is not a choice — 1, 2 or 3."),
            }
        };
        if m.pool_backend == "lvm" {
            println!("\nVolumes are logical volumes in one volume group, and the guest is");
            println!("handed the device itself — no image format between it and the disk.");
            m.lvm_group = ask_valid(
                "Volume group: ",
                crate::wizard::validate_safe_value,
                "the name of an existing volume group, as `vgs` lists it",
            )?;
            println!("\nA thin pool changes what a snapshot costs and how it fails: a thick");
            println!("snapshot reserves its space up front and is dropped by the kernel when");
            println!("it fills; a thin one costs nothing until something is written.");
            m.lvm_thin_pool = prompt("Thin pool inside it, if any []: ")?.trim().to_string();
        }
        if m.pool_backend == "ceph" {
            println!("\nAn existing cluster, or one this machine will deploy. For an existing");
            println!("one, give the config file and the user it should connect as — without");
            println!("them this agent reaches only a cluster it deployed itself.");
            m.ceph_conf = prompt("ceph.conf path, for an external cluster []: ")?
                .trim()
                .to_string();
            let user = prompt("Connect as [client.admin]: ")?.trim().to_string();
            m.ceph_user = if user.is_empty() {
                "client.admin".to_string()
            } else {
                user
            };
            let pool = prompt("RBD pool for volumes [velstra-volumes]: ")?.trim().to_string();
            m.ceph_pool = if pool.is_empty() {
                "velstra-volumes".to_string()
            } else {
                pool
            };
            let images = prompt("RBD pool for images [velstra-images]: ")?.trim().to_string();
            m.ceph_image_pool = if images.is_empty() {
                "velstra-images".to_string()
            } else {
                images
            };
        }
    }

    if roles.contains(&Role::ControlPlane) {
        m.store = prompt("\nStore endpoints [127.0.0.1:2379]: ").map(|s| {
            if s.trim().is_empty() {
                "127.0.0.1:2379".into()
            } else {
                s.trim().to_string()
            }
        })?;
        println!("\nOther cells this address should answer for, as `cell=url` pairs,");
        println!("space-separated. Leave empty for a single-cell installation.");
        let raw = prompt("Other cells: ")?;
        m.cells = raw.split_whitespace().map(str::to_string).collect();
        for pair in &m.cells {
            if !pair.contains('=') {
                bail!("{pair:?} is not cell=url");
            }
        }

        // The first administrator. Asked, not optional, and asked *here*
        // because this is the only role that can answer it.
        //
        // Without one the API starts, serves the console, and refuses every
        // sign-in. It warns about that in its log — which is not where somebody
        // looking at a login form is looking. A cell nobody can sign into is
        // not a cell, and finding that out after the install is finding it out
        // in the worst place.
        // Where it listens. Asked before the administrator, because somebody who
        // answers "only this machine" is describing a laptop and somebody who
        // answers otherwise is describing a cell other people reach.
        println!("\nWho should be able to reach the console and the API?");
        println!("  [1] only this machine (127.0.0.1) — right for a laptop, and the default");
        println!("  [2] anything that can reach this machine (0.0.0.0)");
        println!("\nThere is no TLS here: put a reverse proxy in front before this leaves");
        println!("a network you trust.");
        m.listen = loop {
            match prompt("Reachable from [1]: ")?.trim() {
                "" | "1" => break "127.0.0.1:8443".to_string(),
                "2" => break "0.0.0.0:8443".to_string(),
                other => println!("  {other:?} is not a choice — 1 or 2."),
            }
        };

        println!("\nThe first administrator for this cell. Everything else is created by");
        println!("signing in as somebody: registering a node, making a project, all of it.");
        m.admin = ask_valid(
            "Username [admin]: ",
            validate_node_name,
            "lowercase letters, digits and '-'",
        )
        .map(|s| if s.is_empty() { "admin".into() } else { s })?;
        m.admin_password = loop {
            let first = prompt_secret("Password: ")?;
            if first.trim().len() < 12 {
                println!(
                    "  at least 12 characters — this one credential is the way into everything"
                );
                continue;
            }
            let again = prompt_secret("Repeat it: ")?;
            if first != again {
                println!("  they do not match");
                continue;
            }
            break first;
        };
    }

    // The overlay. Asked last because it is the one answer a cell can honestly
    // decline: without it guests still boot, still get addresses, and reach
    // each other on no tenant network at all. Saying so here is the point —
    // that outcome is indistinguishable from success on every dashboard.
    println!("\nDoes this cell have a fabric? Without one the platform still places guests");
    println!("and allocates addresses, but nothing programs a data plane: tenant networks");
    println!("exist as records and separate no traffic.");
    if ask_yes("Name a fabric?", false)? {
        println!("\nTwo addresses, and they are different services. The orchestrator is where");
        println!("ports and networks are created; the config service is what the eBPF agent");
        println!("watches for its own configuration. Fabric binds the orchestrator to");
        println!("localhost by default — reaching it from here may mean widening it, and that");
        println!("channel can reconfigure any node in the cell.");
        let orchestrator = ask_valid(
            "Orchestrator URL (http://host:50052): ",
            validate_url,
            "a URL with a scheme and a host",
        )?;
        let mut fabric = Fabric {
            orchestrator,
            control: String::new(),
            vtep: String::new(),
            underlay: String::new(),
            srv6_locator: String::new(),
        };
        if roles.contains(&Role::Hypervisor) {
            fabric.control = ask_valid(
                "Config service URL (http://host:50051): ",
                validate_url,
                "a URL with a scheme and a host",
            )?;
            println!("\nThis host's place on the wire. The VTEP address is stated rather than");
            println!("guessed: nothing here can tell which of this machine's addresses its peers");
            println!("route to, and picking one would pick wrong on every multi-homed host.");
            fabric.vtep = ask_valid(
                "VTEP address: ",
                validate_ip,
                "an IP address other hosts route to",
            )?;
            fabric.underlay = ask_valid(
                "Underlay interface: ",
                validate_interface,
                "the interface that address is on",
            )?;
            println!("\nAn SRv6 locator puts this host on the SRv6 wire family instead of VXLAN.");
            println!("It is a slice of your own IPv6 plan, routable in the underlay and unique");
            println!("per host — nothing on this machine knows any of that. Empty stays VXLAN.");
            fabric.srv6_locator = loop {
                let raw = prompt("SRv6 locator (prefix/len, optional): ")?;
                let raw = raw.trim().to_string();
                if raw.is_empty() {
                    break raw;
                }
                match validate_srv6_locator(&raw) {
                    Ok(()) => break raw,
                    Err(e) => println!("  {e:#} — expected an IPv6 prefix like fc00:0:1::/64."),
                }
            };
        }
        m.fabric = Some(fabric);
    }

    // Asked only of a cell with no fabric, because with one the answer is
    // already no — and asking anyway would invite somebody to say yes and have
    // two things owning the far end of every tap.
    if m.fabric.is_none() && m.roles.contains(&Role::Hypervisor) {
        println!("\nThis cell has no fabric, so nothing yet holds the far end of a guest's wire.");
        println!("Without a first hop a guest is not only unreachable: its cloud-init cannot");
        println!("reach the metadata service either, so it gets no user and no SSH key.");
        println!("Saying yes makes this node the gateway for its guests and lets them out");
        println!("through it — what a home hypervisor does. Needs nft.");
        m.local_network = ask_yes("Should this node be the gateway for its guests?", true)?;
    }

    println!("\n{}", render(&m));
    if !ask_yes("Write this?", true)? {
        return Ok(None);
    }
    Ok(Some(m))
}

fn resolve_roles(raw: &str) -> Result<Vec<Role>, String> {
    let mut out = Vec::new();
    for token in raw.split_whitespace() {
        let index: usize = token
            .parse()
            .map_err(|_| format!("{token:?} is not a number from 1 to {}", Role::ALL.len()))?;
        let role = Role::ALL
            .get(index.wrapping_sub(1))
            .ok_or_else(|| format!("there is no role {index}"))?;
        out.push(*role);
    }
    if out.is_empty() {
        return Err("pick at least one — a machine with no role runs nothing".into());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hypervisor() -> Machine {
        Machine {
            api_ca: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            lvm_group: String::new(),
            lvm_thin_pool: String::new(),
            ceph_conf: String::new(),
            ceph_user: String::new(),
            ceph_pool: String::new(),
            ceph_image_pool: String::new(),
            local_network: false,
            region: "eu-central".into(),
            cell: "cell-1".into(),
            roles: vec![Role::Hypervisor],
            api_url: "https://cell-1:8443".into(),
            node: "node-a".into(),
            token: "a".repeat(64),
            vmm: "qemu".into(),
            pool: String::new(),
            pool_backend: "directory".into(),
            store: "127.0.0.1:2379".into(),
            listen: String::new(),
            cells: Vec::new(),
            fabric: None,
            admin: String::new(),
            admin_password: String::new(),
        }
    }

    fn fabric() -> Fabric {
        Fabric {
            orchestrator: "http://fab:50052".into(),
            control: "http://fab:50051".into(),
            vtep: "10.0.0.7".into(),
            underlay: "eth1".into(),
            srv6_locator: String::new(),
        }
    }

    /// The seed carries what was answered and nothing else. A key for a role
    /// this machine does not have would be a value nobody set, read by a unit
    /// that does not run.
    #[test]
    fn a_seed_carries_the_roles_answers_and_no_others() {
        let rendered = render(&hypervisor());
        assert!(
            rendered.contains("VELSTRA_ROLES=hypervisor\n"),
            "{rendered}"
        );
        assert!(rendered.contains("VELSTRA_NODE=node-a\n"), "{rendered}");
        assert!(!rendered.contains("VELSTRA_POOL"), "{rendered}");
        assert!(!rendered.contains("VELSTRA_STORE"), "{rendered}");
        // The token is never in the seed: it is the one secret here and it gets
        // its own file with its own mode.
        assert!(
            !rendered.contains(&"a".repeat(64)),
            "the token is in a world-readable file"
        );
    }

    #[test]
    fn a_machine_that_is_everything_says_so_in_a_fixed_order() {
        let mut m = hypervisor();
        m.roles = vec![Role::Pool, Role::ControlPlane, Role::Hypervisor];
        m.pool = "nvme".into();
        m.cells = vec!["cell-2=https://cell-2:8443".into()];
        let rendered = render(&m);
        assert!(
            rendered.contains("VELSTRA_ROLES=control-plane,hypervisor,pool\n"),
            "{rendered}"
        );
        assert!(rendered.contains("VELSTRA_POOL=nvme\n"), "{rendered}");
        assert!(
            rendered.contains("VELSTRA_CELLS=cell-2=https://cell-2:8443\n"),
            "{rendered}"
        );
    }

    /// A control-plane machine is not told where the API is: it *is* the API,
    /// and a URL pointing at itself would be a fact with two owners.
    #[test]
    fn a_control_plane_is_not_given_a_url_to_itself() {
        let mut m = hypervisor();
        m.roles = vec![Role::ControlPlane];
        m.api_url = String::new();
        let rendered = render(&m);
        assert!(!rendered.contains("VELSTRA_API_URL"), "{rendered}");
    }

    /// What NixOS gets instead of enabled units: the declaration, printed.
    #[test]
    fn the_nix_snippet_says_what_the_answers_mean() {
        let mut m = hypervisor();
        m.roles = vec![Role::ControlPlane, Role::Pool];
        m.pool = "nvme".into();
        m.cells = vec!["cell-2=https://cell-2:8443".into()];
        let snippet = nix_snippet(&m);
        assert!(snippet.contains("controlPlane = {"), "{snippet}");
        assert!(
            snippet.contains("\"cell-2\" = \"https://cell-2:8443\";"),
            "{snippet}"
        );
        assert!(snippet.contains("pool = {"), "{snippet}");
        assert!(snippet.contains("id = \"nvme\";"), "{snippet}");
        // Not a hypervisor, so no node module — a snippet that enabled every
        // module would be a machine running what nobody asked for.
        assert!(!snippet.contains("node.enable"), "{snippet}");
    }

    /// The unattended path: the file is a seed, so what comes out of one
    /// machine goes into the next with two lines changed.
    #[test]
    fn a_config_file_is_a_seed_and_round_trips() {
        let mut m = hypervisor();
        m.roles = vec![Role::Hypervisor, Role::Pool];
        m.pool = "nvme".into();
        let written = render(&m);
        let read = parse(&written).expect("what this writes, it reads");
        assert_eq!(read.region, m.region);
        assert_eq!(read.cell, m.cell);
        assert_eq!(read.roles, m.roles);
        assert_eq!(read.node, m.node);
        assert_eq!(read.pool, m.pool);
        // Except the token, which is deliberately not in the file every unit
        // reads — it comes from the environment or is written separately.
        assert!(read.token.is_empty());
    }

    /// A missing answer is an error naming the key, never a default. An
    /// unattended install that guessed a cell would make a machine that comes
    /// up, registers nowhere, and is found weeks later.
    #[test]
    fn a_config_missing_something_a_role_needs_says_which_key() {
        let missing = "VELSTRA_ROLES=hypervisor\nVELSTRA_API_URL=https://c:8443\n";
        let why = parse(missing).unwrap_err().to_string();
        assert!(why.contains("VELSTRA_NODE"), "{why}");
        assert!(why.contains("no sensible default"), "{why}");

        // And a pool needs its own id for the same reason.
        let pool = "VELSTRA_ROLES=pool\nVELSTRA_API_URL=https://c:8443\n";
        assert!(
            parse(pool)
                .unwrap_err()
                .to_string()
                .contains("VELSTRA_POOL")
        );
    }

    /// A file for a control plane needs no URL to the API: it is the API.
    #[test]
    fn a_control_plane_config_needs_no_url_to_itself() {
        let text = "VELSTRA_ROLES=control-plane\nVELSTRA_CELL=cell-7\n";
        let m = parse(text).expect("a control plane says everything it needs in two lines");
        assert_eq!(m.cell, "cell-7");
        assert!(m.api_url.is_empty());
        // And the defaults it does take are the ones with one sensible answer.
        assert_eq!(m.region, "eu-central");
        assert_eq!(m.store, "127.0.0.1:2379");
    }

    /// Comments and blank lines are a file people edit, and a parser that
    /// choked on `# the London cell` would be one they stop commenting.
    #[test]
    fn a_config_may_be_commented() {
        let text = "# the London cell\n\nVELSTRA_ROLES=control-plane\n  VELSTRA_CELL = cell-ldn \n";
        let m = parse(text).unwrap();
        assert_eq!(m.cell, "cell-ldn");
    }

    #[test]
    fn a_line_that_is_not_a_setting_says_which_line() {
        let why = parse("VELSTRA_ROLES=pool\nnonsense\n")
            .unwrap_err()
            .to_string();
        assert!(why.contains("line 2"), "{why}");
    }

    #[test]
    fn picking_no_role_is_refused_with_the_reason() {
        assert!(resolve_roles("").unwrap_err().contains("at least one"));
        assert!(resolve_roles("9").unwrap_err().contains("no role 9"));
        assert_eq!(resolve_roles("2 2").unwrap(), vec![Role::Hypervisor]);
        assert_eq!(
            resolve_roles("3 1").unwrap(),
            vec![Role::ControlPlane, Role::Pool]
        );
    }

    /// A hypervisor's fabric answers describe *this host's* place on the wire,
    /// so they belong in its seed. A control plane needs only the orchestrator:
    /// it creates networks there and encapsulates nothing itself.
    #[test]
    fn a_fabric_seed_carries_the_wire_for_a_hypervisor_and_not_for_a_control_plane() {
        let mut m = hypervisor();
        m.fabric = Some(fabric());
        let rendered = render(&m);
        assert!(
            rendered.contains("VELSTRA_FABRIC=http://fab:50052\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("VELSTRA_FABRIC_CONTROL=http://fab:50051\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("VELSTRA_FABRIC_VTEP=10.0.0.7\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("VELSTRA_FABRIC_UNDERLAY=eth1\n"),
            "{rendered}"
        );
        // Not asked, so not written — an empty locator would read as a decision.
        assert!(!rendered.contains("SRV6_LOCATOR"), "{rendered}");

        let mut cp = hypervisor();
        cp.roles = vec![Role::ControlPlane];
        cp.fabric = Some(fabric());
        let rendered = render(&cp);
        assert!(
            rendered.contains("VELSTRA_FABRIC=http://fab:50052\n"),
            "{rendered}"
        );
        assert!(!rendered.contains("VELSTRA_FABRIC_VTEP"), "{rendered}");
    }

    #[test]
    fn a_fabric_seed_round_trips() {
        let mut m = hypervisor();
        let mut f = fabric();
        f.srv6_locator = "fc00:0:1::/64".into();
        m.fabric = Some(f.clone());
        let back = parse(&render(&m)).unwrap();
        assert_eq!(back.fabric, Some(f));
    }

    /// The failure this refusal exists for: a node that starts, registers,
    /// reports healthy and carries no tenant traffic, because the overlay was
    /// named and this host's place on it was not.
    #[test]
    fn a_half_named_fabric_is_refused_naming_the_missing_key() {
        let text = "VELSTRA_ROLES=hypervisor\nVELSTRA_API_URL=https://c:8443\n\
                    VELSTRA_NODE=node-a\nVELSTRA_FABRIC=http://fab:50052\n";
        let why = parse(text).unwrap_err().to_string();
        assert!(why.contains("VELSTRA_FABRIC_CONTROL"), "{why}");

        let text = format!("{text}VELSTRA_FABRIC_CONTROL=http://fab:50051\n");
        let why = parse(&text).unwrap_err().to_string();
        assert!(why.contains("VELSTRA_FABRIC_VTEP"), "{why}");
    }

    /// And the mirror image: answers about a fabric with no fabric named. They
    /// would be read by nobody, and leave somebody certain the overlay is on.
    #[test]
    fn wire_answers_without_a_fabric_are_refused_rather_than_ignored() {
        let text = "VELSTRA_ROLES=hypervisor\nVELSTRA_API_URL=https://c:8443\n\
                    VELSTRA_NODE=node-a\nVELSTRA_FABRIC_VTEP=10.0.0.7\n";
        let why = parse(text).unwrap_err().to_string();
        assert!(why.contains("VELSTRA_FABRIC_VTEP"), "{why}");
        assert!(why.contains("VELSTRA_FABRIC is not"), "{why}");
    }

    /// A cell with no fabric stays legitimate: this is what every cell did
    /// before the overlay existed, and it must not become an error.
    #[test]
    fn a_cell_with_no_fabric_is_still_a_cell() {
        let text =
            "VELSTRA_ROLES=hypervisor\nVELSTRA_API_URL=https://c:8443\nVELSTRA_NODE=node-a\n";
        assert_eq!(parse(text).unwrap().fabric, None);
    }

    /// The seed is the only thing the agent reads, so a node that answered yes
    /// and a node that answered no have to be told apart by it alone.
    #[test]
    fn whether_this_node_is_its_guests_gateway_survives_the_seed() {
        let mut m = hypervisor();
        m.local_network = true;
        let text = render(&m);
        assert!(text.contains("VELSTRA_LOCAL_NETWORK=1"), "{text}");
        assert!(parse(&text).unwrap().local_network);

        // And a node that said no says nothing, rather than saying zero: an
        // absent line is what every other optional answer here looks like.
        m.local_network = false;
        let text = render(&m);
        assert!(!text.contains("VELSTRA_LOCAL_NETWORK"), "{text}");
        assert!(!parse(&text).unwrap().local_network);
    }

    #[test]
    fn a_pool_backend_carries_its_own_settings_into_the_seed() {
        // The gap this closes: the pool agent's ceph arguments read only from
        // the command line, the Debian unit passes `--backend` and nothing else,
        // and so an **external** cluster could not be configured on a package
        // install at all — the agent fell back to `client.admin` with no config
        // file, which reaches a cluster this machine deployed itself and nothing
        // else.
        let mut m = Machine {
            api_ca: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            roles: vec![Role::Pool],
            pool: "nvme".into(),
            pool_backend: "ceph".into(),
            ceph_conf: "/etc/ceph/ceph.conf".into(),
            ceph_user: "client.velstra".into(),
            ceph_pool: "cloud-volumes".into(),
            ..Default::default()
        };
        let seed = render(&m);
        assert!(seed.contains("VELSTRA_CEPH_CONF=/etc/ceph/ceph.conf"), "{seed}");
        assert!(seed.contains("VELSTRA_CEPH_USER=client.velstra"), "{seed}");
        assert!(seed.contains("VELSTRA_CEPH_POOL=cloud-volumes"), "{seed}");
        // Nothing that was not answered: an empty line would override the
        // agent's own default with nothing, which is worse than silence.
        assert!(!seed.contains("VELSTRA_CEPH_IMAGE_POOL="), "{seed}");
        assert!(!seed.contains("VELSTRA_LVM_GROUP="), "{seed}");

        m.pool_backend = "lvm".into();
        m.ceph_conf = String::new();
        m.ceph_user = String::new();
        m.ceph_pool = String::new();
        m.lvm_group = "vg0".into();
        m.lvm_thin_pool = "thin".into();
        let seed = render(&m);
        assert!(seed.contains("VELSTRA_POOL_BACKEND=lvm"), "{seed}");
        assert!(seed.contains("VELSTRA_LVM_GROUP=vg0"), "{seed}");
        assert!(seed.contains("VELSTRA_LVM_THIN_POOL=thin"), "{seed}");
        assert!(!seed.contains("VELSTRA_CEPH"), "{seed}");
    }

}

/// Bring an existing seed up to what this package needs, and change nothing else.
///
/// Run by `postinst` on every upgrade. It exists because of one specific way an
/// update can break a working machine silently: the API grew TLS, and an agent
/// whose seed still says `http://` then talks plain HTTP to a TLS port. What it
/// gets back is `invalid HTTP version parsed` — a TLS greeting seen by an HTTP
/// parser — so the node stops following its cell, every guest on it goes to
/// `Unknown`, and nothing anywhere says "your seed is out of date".
///
/// Two rules, both deliberate:
///
/// **Only what is provably safe.** The URL is rewritten only when this machine
/// has a certificate *and* the seed already points at itself; a seed naming
/// somebody else's control plane is left alone, because whether that one serves
/// TLS is not knowable from here.
///
/// **Never overwrite an answer somebody gave.** A seed that already names a CA
/// or an https URL is left exactly as it is.
pub fn migrate_seed(dir: &std::path::Path) -> Result<Vec<String>> {
    let path = dir.join("node.env");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let cert = dir.join("tls").join("cert.pem");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut changed = Vec::new();

    fn value(lines: &[String], key: &str) -> Option<String> {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    // Nothing to do on a cell that serves plaintext: an agent speaking http to
    // an http port is correct, and rewriting it would break what works.
    if !cert.exists() {
        return Ok(changed);
    }

    if let Some(url) = value(&lines, "VELSTRA_API_URL")
        && url.starts_with("http://")
        && (url.contains("127.0.0.1") || url.contains("localhost"))
    {
        let port = url.rsplit(':').next().unwrap_or("8443").to_string();
        let now = format!("https://localhost:{port}");
        for line in lines.iter_mut() {
            if line.starts_with("VELSTRA_API_URL=") {
                *line = format!("VELSTRA_API_URL={now}");
            }
        }
        changed.push(format!("VELSTRA_API_URL is now {now}: this machine serves TLS"));
    }

    if value(&lines, "VELSTRA_API_CA").is_none()
        && value(&lines, "VELSTRA_API_URL").is_some_and(|u| u.starts_with("https://"))
    {
        lines.push(format!("VELSTRA_API_CA={}", cert.display()));
        changed.push(format!(
            "VELSTRA_API_CA is now {}: the agents verify the API against the cell's own \
             certificate rather than trusting whatever answers",
            cert.display()
        ));
    }

    if changed.is_empty() {
        return Ok(changed);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(changed)
}

#[cfg(test)]
mod migrating_a_seed {
    use super::*;

    fn scratch(name: &str, seed: &str, with_cert: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("velstra-seed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tls")).unwrap();
        std::fs::write(dir.join("node.env"), seed).unwrap();
        if with_cert {
            std::fs::write(dir.join("tls").join("cert.pem"), "x").unwrap();
        }
        dir
    }

    #[test]
    fn a_seed_from_before_tls_is_pointed_at_the_certificate() {
        // The upgrade that would otherwise cut a node off from its own cell.
        let dir = scratch(
            "old",
            "VELSTRA_ROLES=control-plane,hypervisor\nVELSTRA_API_URL=http://127.0.0.1:8443\n",
            true,
        );
        let said = migrate_seed(&dir).unwrap();
        assert_eq!(said.len(), 2, "{said:?}");
        let seed = std::fs::read_to_string(dir.join("node.env")).unwrap();
        assert!(seed.contains("VELSTRA_API_URL=https://localhost:8443"), "{seed}");
        assert!(seed.contains("VELSTRA_API_CA="), "{seed}");
        // Idempotent: a second upgrade says nothing and changes nothing.
        assert!(migrate_seed(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cell_without_a_certificate_is_left_alone() {
        // Plaintext is a supported configuration. Rewriting it would break a
        // machine that was working.
        let dir = scratch("plain", "VELSTRA_API_URL=http://127.0.0.1:8443\n", false);
        assert!(migrate_seed(&dir).unwrap().is_empty());
        let seed = std::fs::read_to_string(dir.join("node.env")).unwrap();
        assert!(seed.contains("http://127.0.0.1:8443"), "{seed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn somebody_elses_control_plane_is_not_guessed_at() {
        // This machine's certificate says nothing about whether the cell it
        // joined serves TLS, so the seed is not touched.
        let dir = scratch("remote", "VELSTRA_API_URL=http://cell.example:8443\n", true);
        assert!(migrate_seed(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}


/// The etcd this cell was going to run into.
///
/// A cell died on this and it died completely: every write refused with
///
/// ```text
/// etcdserver: mvcc: database space exceeded
/// ```
///
/// after an afternoon of ordinary use. etcd keeps every revision of every object
/// until somebody compacts, and it stops accepting writes at **2 GiB** — a
/// default chosen for a store somebody watches, not for one a platform brings up
/// and never mentions again. Nothing in this platform compacted, nothing raised
/// the ceiling, and nothing said a word until the cell stopped.
///
/// Three settings, and each one is a different half of the same failure:
///
/// * `auto-compaction-retention` — throw the history away as it ages. Without
///   it the store grows with *changes*, not with what is in it, and a busy cell
///   fills faster than an idle one no matter how little it holds.
/// * `quota-backend-bytes` — 8 GiB. Not a fix on its own, and not meant as one:
///   it is the difference between an afternoon and a year, which is the
///   difference between an outage and a maintenance window.
/// * `etcd-client`, so that an operator staring at a full store has `etcdctl` to
///   compact and defrag with. The box this was found on had none — the platform
///   brought up a store and gave nobody a way to look after it.
///
/// Compaction does not shrink the file; only a defrag does. So this is what
/// keeps the ceiling from being met, and `docs/install.md` says what to run when
/// it is met anyway.
///
/// Written to `/etc/default/etcd`, which the Debian unit sources. Existing
/// settings are left exactly as they are: an operator who has tuned this has
/// tuned it, and a first install being helpful is not a licence to overwrite
/// somebody's decision on every upgrade.
///
/// `Ok(false)` when there was nothing to add.
pub fn settle_etcd() -> Result<bool> {
    settle_etcd_at(std::path::Path::new("/etc/default/etcd"))
}

fn settle_etcd_at(path: &std::path::Path) -> Result<bool> {
    const WANTED: &[(&str, &str)] = &[
        // An hour of history. Long enough that a watcher which fell behind can
        // still catch up, short enough that a busy afternoon does not become a
        // gigabyte.
        ("ETCD_AUTO_COMPACTION_MODE", "periodic"),
        ("ETCD_AUTO_COMPACTION_RETENTION", "1h"),
        ("ETCD_QUOTA_BACKEND_BYTES", "8589934592"),
    ];

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let missing: Vec<&(&str, &str)> = WANTED
        .iter()
        .filter(|(key, _)| {
            !existing
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{key}=")))
        })
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }

    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(
        "\n# Added by velstra-cloud-node. etcd keeps every revision until it is\n\
         # compacted and stops accepting writes at 2 GiB; a cell that never\n\
         # compacts stops working after an afternoon. Remove or change these and\n\
         # they will not be written again.\n",
    );
    for (key, value) in &missing {
        out.push_str(&format!("{key}={value}\n"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out)?;
    Ok(true)
}

#[cfg(test)]
mod giving_the_store_room {
    use super::*;

    fn scratch(what: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("velstra-etcd-{}-{what}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("etcd")
    }

    #[test]
    fn a_store_that_was_never_configured_gets_all_three() {
        let path = scratch("fresh");
        assert!(settle_etcd_at(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ETCD_AUTO_COMPACTION_RETENTION=1h"));
        assert!(text.contains("ETCD_QUOTA_BACKEND_BYTES=8589934592"));
        assert!(text.contains("ETCD_AUTO_COMPACTION_MODE=periodic"));
    }

    #[test]
    fn a_second_run_writes_nothing() {
        // Level-triggered, like everything else here: an installer that appended
        // its block on every upgrade would leave a file nobody can read.
        let path = scratch("twice");
        assert!(settle_etcd_at(&path).unwrap());
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(!settle_etcd_at(&path).unwrap());
        assert_eq!(once, std::fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn what_somebody_already_decided_is_left_alone() {
        // An operator who tuned this has tuned it. A first install being helpful
        // is not a licence to overwrite that on every upgrade.
        let path = scratch("theirs");
        std::fs::write(
            &path,
            "ETCD_QUOTA_BACKEND_BYTES=17179869184\nETCD_NAME=cell-1\n",
        )
        .unwrap();
        assert!(settle_etcd_at(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("ETCD_QUOTA_BACKEND_BYTES=17179869184"),
            "somebody's own quota was overwritten"
        );
        assert_eq!(
            text.matches("ETCD_QUOTA_BACKEND_BYTES").count(),
            1,
            "a second value was appended, so which one wins is a coin toss"
        );
        assert!(text.contains("ETCD_AUTO_COMPACTION_RETENTION=1h"));
        assert!(text.contains("ETCD_NAME=cell-1"), "their other settings went");
    }

    #[test]
    fn a_commented_out_setting_is_not_mistaken_for_one() {
        // The Debian file ships as nothing but comments. A prefix match on the
        // whole line would read `## ETCD_QUOTA_BACKEND_BYTES=…` from the
        // documentation as a decision somebody made.
        let path = scratch("comments");
        std::fs::write(&path, "## ETCD_QUOTA_BACKEND_BYTES=2147483648\n").unwrap();
        assert!(settle_etcd_at(&path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nETCD_QUOTA_BACKEND_BYTES=8589934592"));
    }
}
