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
        ask_valid, ask_valid_or, ask_yes, prompt, prompt_secret, validate_interface, validate_ip,
        validate_node_name, validate_srv6_locator, validate_token, validate_url,
    },
};

/// Where a machine keeps what it holds: guests, images, pool metadata, its TLS
/// material.
///
/// The appliance decides this: its `/etc` is on a read-only verity store and
/// its writable partition mounts here. One path on all three systems is worth
/// more than the conventional one on two of them.
pub const SEED_DIR: &str = "/var/lib/velstra";

/// Where a machine keeps *who it is*.
///
/// Separate from [`SEED_DIR`] for one reason, and it is not tidiness. A cell
/// whose machines share one filesystem — which is what makes moving a guest
/// possible at all — has every agent reading the same state directory. Put the
/// seed there and the second machine to mount it renames the first: it answers
/// to the other's node id, runs the other's roles, and reports the other's
/// pool. Found on two real machines, and it took the control plane down —
/// `has-role control-plane` read a seed that belonged to a hypervisor.
///
/// So identity lives in `/etc`, which is per-machine by construction, and the
/// state directory is left holding only what a cell shares.
pub const IDENTITY_DIR: &str = "/etc/velstra";

/// What this machine was told about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Machine {
    pub region: String,
    pub cell: String,
    pub roles: Vec<Role>,
    /// What this box should call itself, for a machine that does not have a
    /// name yet.
    ///
    /// Only the installer answers it: it is seeding a filesystem that has never
    /// booted, and `velstra-node-boot` reads this to set the hostname on the
    /// first start. [`run_with`] leaves it empty, because a machine that
    /// already runs an operating system already has a name and renaming it is
    /// not a thing a wizard about cloud roles should do.
    ///
    /// Here rather than in the installer's own seed writer because there used
    /// to be two of those, and only one of them knew about `VELSTRA_ROLES` —
    /// which is why every flashed machine was a hypervisor whatever it was
    /// installed to be.
    pub hostname: String,
    /// The API's certificate as PEM, when it arrived *inside* a join token
    /// rather than as a path on this machine. `write_seed` puts it beside the
    /// seed as `api-ca.pem` and points `api_ca` at it, so the agents read a
    /// file like they always have. Empty on every other path.
    pub api_ca_pem: String,
    /// Where this control plane tells joiners to reach it: URLs, comma
    /// separated, each a name its certificate carries. Written for the API
    /// as `VELSTRA_ADVERTISE`. Empty on a machine that is not a control
    /// plane, and on one nobody can reach.
    pub advertise: String,
    /// Disks the first machine hands to Ceph at first boot, as kernel names.
    /// `bootstrap-cell` resolves them to the stable paths the node agent
    /// reports and creates the cluster. Empty on every other machine.
    pub bootstrap_ceph_osds: Vec<String>,
    /// An SSH public key that may log in as root, and whether a password was
    /// set. Both empty is the default and the sealed shape: no account anybody
    /// can use, fleet access through the control plane, break-glass by booting
    /// the installer medium.
    ///
    /// Offered at install rather than decided here. A machine nobody can log
    /// in to is the safer default and a genuinely awkward one to debug, and
    /// which of those matters more is the operator's call, not this
    /// platform's.
    pub ssh_key: String,
    /// The root password, in the clear, on its way to a 0600 file beside the
    /// seed. Never rendered into `node.env`, which is world-readable — the
    /// same split the tokens and the bootstrap password already make.
    pub root_password: String,
    /// PCI devices this machine holds back for guests: addresses
    /// (`0000:41:00.0`) or vendor:device pairs (`10de:2204`), comma
    /// separated. Empty is the default and means the host keeps every card.
    ///
    /// In the seed rather than on the kernel command line, which is where the
    /// usual `vfio-pci.ids=` recipe puts it: this image's command line is
    /// sealed into a signed UKI, so a device id there would be a fact about
    /// the image — and an image is built once for a fleet of boxes with
    /// different cards in them.
    ///
    /// **No wizard asks for this.** Which cards a machine holds back is a
    /// decision about what it will run, and nobody has made it while they are
    /// standing in front of a disk that is about to be erased. It is one line
    /// added to the seed afterwards, and a restart of the node agent — which
    /// is also how it is taken back, where an answer given at install time
    /// would have meant a reinstall.
    pub passthrough: String,
    /// Where the API is. Empty on a control-plane-only machine, which *is* the
    /// API — a URL pointing at itself would be a fact with two owners.
    pub api_url: String,
    /// The node's id and its one-time token, for a hypervisor.
    pub node: String,
    pub token: String,
    pub vmm: String,
    /// The pool's id and backend, for a pool.
    pub pool: String,
    /// The pool's own one-time token.
    ///
    /// A second credential, because a pool is a second agent. The pool agent
    /// authenticates as `pool:<id>` and the node agent as `node:<id>`, and the
    /// API issues them separately — `poolToken` when the pool object is made,
    /// `nodeToken` when the node object is. One token cannot be both: a node
    /// token presented by a pool agent is answered `401 the bearer token was
    /// not accepted`, which is what a machine joined as hypervisor **and** pool
    /// used to get, for ever, with the seed looking complete.
    ///
    /// Empty on a control plane, whose pool agent reaches the store directly
    /// and is given no token at all.
    pub pool_token: String,
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
    /// The **pool object's** id in this cell — what a volume's `spec.pool`
    /// names — as against the two RBD pool names above, which live inside the
    /// cluster. Set on a machine that serves the cell's Ceph pool; empty
    /// everywhere else, and the Ceph unit then does nothing.
    pub ceph_pool_id: String,
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
    if !m.hostname.is_empty() {
        out.push_str(&format!("VELSTRA_HOSTNAME={}\n", m.hostname));
    }
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
            // The pool *object's* id in this cell — what a volume's
            // `spec.pool` names — as against the two keys above, which are RBD
            // pools inside the cluster. Empty on a machine that serves only a
            // local pool, and the Ceph unit then reads as "not for this box".
            ("VELSTRA_CEPH_POOL_ID", &m.ceph_pool_id),
        ] {
            if !value.is_empty() {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
    }
    if m.roles.contains(&Role::ControlPlane) {
        // Named or absent, never empty: `VELSTRA_STORE=` sets the variable to
        // the empty string, and the API reads its own `env =` fallback as
        // present-and-empty rather than falling back at all.
        if !m.store.is_empty() {
            out.push_str(&format!("VELSTRA_STORE={}\n", m.store));
        }
        // Both or neither: the API refuses one without the other rather than
        // serving plaintext on a port somebody believes is encrypted.
        if !m.tls_cert.is_empty() && !m.tls_key.is_empty() {
            out.push_str(&format!(
                "VELSTRA_TLS_CERT={}\nVELSTRA_TLS_KEY={}\n",
                m.tls_cert, m.tls_key
            ));
        }
        if !m.advertise.is_empty() {
            out.push_str(&format!("VELSTRA_ADVERTISE={}\n", m.advertise));
        }
        if !m.bootstrap_ceph_osds.is_empty() {
            out.push_str(&format!(
                "VELSTRA_BOOTSTRAP_CEPH_OSDS={}\n",
                m.bootstrap_ceph_osds.join(",")
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
    // Console and SSH access, when the operator asked for it. The key is not a
    // secret and rides in the seed; the password does not — it goes to a 0600
    // file beside it, the same split the tokens already make.
    if !m.ssh_key.is_empty() {
        out.push_str(&format!("VELSTRA_SSH_KEY={}\n", m.ssh_key));
    }
    if !m.root_password.is_empty() {
        out.push_str("VELSTRA_CONSOLE_LOGIN=1\n");
    }
    if !m.passthrough.is_empty() {
        out.push_str(&format!("VELSTRA_PASSTHROUGH={}\n", m.passthrough));
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

/// The answers from a join token, which is every one of them.
///
/// The token was minted by the cell for exactly one node object, so region,
/// cell and node id are facts rather than questions, the certificate is inside
/// it, and the URL is one the certificate verifies for — the six things that
/// used to travel by six routes. See `docs/joining.md`.
///
/// `qemu` unconditionally. Cloud Hypervisor takes a path and nothing else, so
/// it cannot open a Ceph volume; a machine joining a cell cannot know what
/// storage the cell will grow, and the hypervisor that can open everything is
/// the only safe answer for a wizard that is not going to ask.
pub(crate) fn from_join(token: &str, dir: &Path) -> Result<Machine> {
    let t = velstra_cloud_wire::join::JoinToken::decode(token)?;
    let mut roles = Vec::new();
    if !t.token.is_empty() {
        roles.push(Role::Hypervisor);
    }
    if t.pool.is_some() {
        roles.push(Role::Pool);
    }
    if roles.is_empty() {
        bail!(
            "the join token carries neither a node nor a pool credential, so there is nothing \
             for this machine to be"
        );
    }
    let (pool, pool_token, pool_backend) = match &t.pool {
        // The backend is this machine's to answer, not the token's: it is a
        // fact about the disks here. Directory unless told otherwise — the one
        // that needs nothing but a writable directory.
        Some(p) => (p.id.clone(), p.token.clone(), "directory".to_string()),
        None => (String::new(), String::new(), String::new()),
    };
    Ok(Machine {
        region: t.region,
        cell: t.cell,
        roles,
        api_ca_pem: t.ca,
        // The path the seed names; `write_seed` puts the PEM there first.
        api_ca: dir.join("api-ca.pem").display().to_string(),
        api_url: t
            .urls
            .into_iter()
            .find(|u| !u.trim().is_empty())
            .unwrap_or_default(),
        node: t.node,
        token: t.token,
        vmm: "qemu".into(),
        pool,
        pool_token,
        pool_backend,
        ..Default::default()
    })
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
        // Read back, every one of them, because `render` writes every one of
        // them — and this is where that stopped being true.
        //
        // A seed goes through parse→render whenever anything rewrites it:
        // `ensure-tls` on a control plane's first boot, `migrate-seed` on an
        // upgrade, `setup --config`. Five keys were written and not read, so
        // every rewrite silently dropped them. `VELSTRA_TLS_CERT` cost the
        // most: `bootstrap-cell` decides whether the cell speaks TLS by
        // whether that field is set, read it back as empty, and spent ninety
        // seconds asking an https port over http before giving up — so the
        // first machine of a cell got a certificate and an API, and no Node,
        // no Pool and no token. `VELSTRA_HOSTNAME` cost the quietest: a
        // machine installed under a name lost it on the first rewrite and
        // answered to the image's default from the next boot on.
        //
        // `seed_survives_a_round_trip` is what keeps this honest now: it
        // populates every field, renders, parses, and compares — so a key
        // added to one side and not the other fails a test rather than a
        // fleet.
        hostname: or("VELSTRA_HOSTNAME", ""),
        tls_cert: or("VELSTRA_TLS_CERT", ""),
        tls_key: or("VELSTRA_TLS_KEY", ""),
        lvm_group: or("VELSTRA_LVM_GROUP", ""),
        lvm_thin_pool: or("VELSTRA_LVM_THIN_POOL", ""),
        // Read from the seed like everything else, which they were not: an
        // unattended install could name a Ceph backend and had no way to say
        // which cluster, which pools or which client — so `--config` could
        // configure every backend but that one.
        ceph_conf: or("VELSTRA_CEPH_CONF", ""),
        ceph_user: or("VELSTRA_CEPH_USER", ""),
        ceph_pool: or("VELSTRA_CEPH_POOL", ""),
        ceph_image_pool: or("VELSTRA_CEPH_IMAGE_POOL", ""),
        ceph_pool_id: or("VELSTRA_CEPH_POOL_ID", ""),
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
        pool_token: or("VELSTRA_POOL_TOKEN", ""),
        pool_backend: or("VELSTRA_POOL_BACKEND", "directory"),
        store: or("VELSTRA_STORE", "127.0.0.1:2379"),
        listen: or("VELSTRA_LISTEN", ""),
        advertise: or("VELSTRA_ADVERTISE", ""),
        ssh_key: or("VELSTRA_SSH_KEY", ""),
        passthrough: or("VELSTRA_PASSTHROUGH", ""),
        // Read back as a marker only: the password itself lives in its own
        // file, and a seed that carried it would be a secret in a
        // world-readable file.
        root_password: if or("VELSTRA_CONSOLE_LOGIN", "") == "1" {
            "(set)".into()
        } else {
            String::new()
        },
        bootstrap_ceph_osds: or("VELSTRA_BOOTSTRAP_CEPH_OSDS", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        api_ca_pem: String::new(),
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
        // The same exemption the pool below has always had, and its absence
        // here broke every first machine of a cell installed from the image.
        //
        // A control plane *is* the API, so the installer writes no
        // `VELSTRA_API_URL` for one — and door 1 writes
        // `control-plane,hypervisor,pool`. This branch then refused the seed
        // by name, `velstra-cell-tls` died on the first boot, the API had no
        // certificate to serve, and the console was never there. The banner
        // reads the same seed through `.ok()`, so it printed a machine with
        // no name, no roles and no address — three symptoms, one missing
        // condition. `ensure-tls` fills the URL in afterwards, for the agents
        // on this machine that do need one.
        if m.api_url.is_empty() && !roles.contains(&Role::ControlPlane) {
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
    join: Option<String>,
    join_file: Option<PathBuf>,
) -> Result<()> {
    // A token from a file is the same token. Read here rather than threaded
    // through: a token on a command line is in `ps` for every user on the
    // machine and in the shell's history afterwards, and configuration
    // management would rather write a file than quote a kilobyte.
    let join = match (join, join_file) {
        (Some(t), _) => Some(t),
        (None, Some(path)) => {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let token = crate::joinfile::token_in(&text).ok_or_else(|| {
                anyhow::anyhow!(
                    "{} holds no join token — one line starting `velstra1.`, \
                     with or without a comment above it",
                    path.display()
                )
            })?;
            Some(token.encode())
        }
        (None, None) => None,
    };
    let nixos = assume_nixos.unwrap_or_else(|| Path::new("/etc/NIXOS").exists());
    // Where the seed goes is decided by which machine this is, not by taste.
    //
    // On Debian it goes to [`IDENTITY_DIR`], because the state directory may be
    // a filesystem the whole cell mounts and a seed there renames whoever
    // mounts it next. On the appliance it goes to [`SEED_DIR`]: `/etc` is on a
    // read-only verity store there, and a machine whose state directory is
    // its own alone has nobody to be renamed by.
    let dir = dir.unwrap_or_else(|| PathBuf::from(if nixos { SEED_DIR } else { IDENTITY_DIR }));
    let machine = match (&config, &join) {
        // Everything is in the token; nothing is asked. See `from_join`.
        (_, Some(token)) => Some(from_join(token, &dir)?),
        (Some(path), None) => {
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
        (None, None) => collect()?,
    };
    let Some(machine) = machine else {
        println!("Nothing was written.");
        return Ok(());
    };
    write_seed(&dir, &machine)?;

    if nixos {
        println!("\nThis is NixOS, so nothing was enabled.");
        println!("Units there are a declaration, and a wizard reaching into them would be");
        println!("fighting the operating system. Add this and rebuild:\n");
        println!("{}", nix_snippet(&machine));
    } else {
        // Enabled, not printed. Printing two commands and hoping both are run is
        // exactly the shape this platform keeps removing — and it cost a real
        // cell: a machine answered `pool` alongside `hypervisor`, the node agent
        // was started, the pool agent was not, and the console showed a pool that
        // reported nothing at all with no hint that anybody had missed a step.
        //
        // A unit that will not start says so here, where somebody is still
        // watching, rather than in a journal nobody thinks to open.
        let mut units: Vec<String> = Vec::new();
        for role in &machine.roles {
            for unit in role.units() {
                units.push(unit.to_string());
            }
        }
        println!("\nStarting what the roles say:\n");
        for unit in &units {
            match enable_now(unit) {
                Ok(()) => println!("  {unit}"),
                Err(e) => {
                    println!("  {unit} — did not start: {e}");
                    println!("    systemctl status {unit}");
                }
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

/// Turn one unit on, now and at boot.
///
/// `enable --now` on a unit that is already running is a no-op, which is what
/// makes re-running the wizard safe.
fn enable_now(unit: &str) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(["enable", "--now", unit])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()).into())
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
    // The certificate first, so the seed that names its path never exists
    // without it. World-readable: it is a certificate, not a secret, and the
    // agents that verify against it do not all run as root.
    if !m.api_ca_pem.trim().is_empty() {
        write_with_mode(&dir.join("api-ca.pem"), &m.api_ca_pem, 0o644)?;
    }
    write_with_mode(&dir.join("node.env"), &render(m), 0o644)?;
    if m.roles.contains(&Role::Hypervisor) && !m.token.is_empty() {
        // The one secret here, and it gets its own file with its own mode. The
        // seed is world-readable because nothing in it is secret and the units
        // that read it do not all run as root.
        write_with_mode(&dir.join("node-token"), &format!("{}\n", m.token), 0o600)?;
    }
    // The pool's own credential, on the same terms. The unit reads
    // `/etc/velstra/pool-token` and falls back to the shared state directory;
    // writing it here is what makes a pool agent on any machine but the
    // control plane able to speak at all.
    if m.roles.contains(&Role::Pool)
        && !m.roles.contains(&Role::ControlPlane)
        && !m.pool_token.is_empty()
    {
        write_with_mode(
            &dir.join("pool-token"),
            &format!("{}\n", m.pool_token),
            0o600,
        )?;
    }
    // The root password, its own file with its own mode, for the same reason
    // the tokens have one: `node.env` is world-readable and the units that
    // read it do not all run as root.
    if !m.root_password.is_empty() && m.root_password != "(set)" {
        write_with_mode(
            &dir.join("root-password"),
            &format!("{}\n", m.root_password),
            0o600,
        )?;
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

pub(crate) fn write_with_mode(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    // A private temporary file avoids both a permissive creation window and
    // truncating a live credential if the process dies midway through a write.
    let parent = path.parent().context("the file has no parent directory")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let tmp = parent.join(format!(".seed-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

/// What the cell says about the machine holding this token.
///
/// Three answers an operator would otherwise give twice: once when they created
/// the node object, and again — identically, by hand, on a console — when they
/// typed the region, the cell and the node id into this wizard. The token was
/// issued *for* one node object in one cell, so the cell can simply say which,
/// and a typo stops being a machine that comes up, registers nowhere and is
/// found weeks later.
///
/// `None` when the cell cannot be reached or will not have the token, and that
/// is not an error: a control plane that is not up yet, an air-gapped install
/// and a seed written ahead of time are all real, so the questions are still
/// there to fall back on. What is *not* done is guessing.
#[derive(Debug)]
struct CellSays {
    node: String,
    region: String,
    cell: String,
}

fn ask_the_cell(api_url: &str, token: &str, ca: &str) -> Result<CellSays, String> {
    let url = format!("{}/api/v1/sessions/current", api_url.trim_end_matches('/'));
    let auth = format!("Authorization: Bearer {token}");
    // Against the cell's own certificate, which is the one the seed is about to
    // name and the agents are about to verify. Not `-k`: a lookup that trusted
    // anything would be a lookup that can be answered by anything, and this one
    // decides which cell the machine joins.
    let mut args: Vec<&str> = vec!["-H", &auth];
    if !ca.is_empty() {
        args.extend(["--cacert", ca]);
    }
    args.push(&url);
    // The reason is carried out rather than swallowed. The failure that
    // actually happens is a certificate that does not name the address typed a
    // moment ago — a cell's own certificate names its hostname, `localhost` and
    // `127.0.0.1`, so an operator who reaches for the IP gets a machine whose
    // agents refuse the API hours later, with nothing connecting the two
    // events. Said here, it is one line and a second attempt.
    let body = fetch(&args).map_err(|e| {
        // The first line of what curl said, which is the sentence. The rest is
        // its standing advice about certificates, and five lines of it in the
        // middle of a wizard buries the one line that names the problem.
        let said = e.to_string();
        let first = said
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(&said)
            .trim();
        // Without the helper's own prefix: "curl failed: curl: (6) …" says
        // curl twice and the reader only needs the half that names the problem.
        first
            .strip_prefix("curl failed: ")
            .unwrap_or(first)
            .to_string()
    })?;
    cell_from_body(&body)
}

fn fetch(args: &[&str]) -> Result<String> {
    crate::quickstart::curl(args)
}

/// What `sessions/current` said, read into the three answers it settles.
///
/// Split from the request so the reading can be tested against bodies this
/// same API produces — including the one an older control plane produces, which
/// accepts the token and does not name the cell.
fn cell_from_body(body: &str) -> Result<CellSays, String> {
    // `subject` is the node the credential was minted for — see
    // `IdentityStore::identify_node`, which spells it `node:<id>`; the seed and
    // every object want the bare id.
    let subject = crate::quickstart::field(body, "subject")
        .ok_or_else(|| format!("the answer named no subject: {}", body.trim()))?;
    let node = node_from_subject(&subject).to_string();
    // An older control plane answers this route without naming the cell. That
    // is not a refusal and not worth a complaint — it is the one case where the
    // questions below are the only way to know.
    let (Some(region), Some(cell)) = (
        crate::quickstart::field(body, "region"),
        crate::quickstart::field(body, "cell"),
    ) else {
        return Err(format!(
            "the cell accepted the token for node {node} but did not say which cell it is — \
             an older control plane"
        ));
    };
    if node.is_empty() || region.is_empty() || cell.is_empty() {
        return Err(format!("the answer was incomplete: {}", body.trim()));
    }
    Ok(CellSays { node, region, cell })
}

/// The node id inside the subject the API answers with.
///
/// `identify_node` spells an agent `node:<id>`, and everything this wizard
/// writes — the seed, the node object, the units' filter — wants the bare id.
/// Taken as-is when there is no prefix, so an older or a different control
/// plane is not made to fit a shape it never promised.
fn node_from_subject(subject: &str) -> &str {
    subject.strip_prefix("node:").unwrap_or(subject)
}

/// The registration token, from the environment when it is there.
///
/// 64 hex characters is not something anybody types correctly at a console, and
/// a fresh machine is exactly where somebody is at a console. So the same
/// `VELSTRA_TOKEN` the unattended path uses is honoured here too, and a pasted
/// answer is taken with whatever whitespace and capitals came with it.
fn ask_for_the_token() -> Result<String> {
    ask_for_a_token("Registration token", "VELSTRA_TOKEN")
}

/// One token, asked for by the name it goes by and honoured from the
/// environment under the variable the unattended path uses.
///
/// Shared, because there are two of them now — a node's and a pool's — and two
/// copies of "paste 64 hex characters, and take it from the environment if it
/// is there" is two places for them to drift.
fn ask_for_a_token(label: &str, env: &str) -> Result<String> {
    if let Ok(from_env) = std::env::var(env) {
        let tidy = tidy_token(&from_env);
        if validate_token(&tidy).is_ok() {
            println!("{label}: taken from {env}.");
            return Ok(tidy);
        }
        if !from_env.trim().is_empty() {
            println!("  {env} is set but is not a token; asking instead.");
        }
    }
    loop {
        let raw = prompt(&format!("{label}: "))?;
        let tidy = tidy_token(&raw);
        match validate_token(&tidy) {
            Ok(()) => return Ok(tidy),
            Err(e) => println!("  {e} — expected 64 lowercase hex characters."),
        }
    }
}

/// A pasted token, as the paste arrives: outer whitespace gone, inner
/// whitespace gone (a terminal wraps), capitals folded down.
fn tidy_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Ask everything. `None` when the operator declines the final confirmation —
/// nothing has been written at that point.
fn collect() -> Result<Option<Machine>> {
    println!("Velstra Cloud — set up this machine\n");
    println!("This writes {IDENTITY_DIR}/node.env and nothing else: no disks, no bootloader,");
    println!("no packages. The machine is already installed; what is missing is the answer");
    println!("to which cell it belongs to, as what, and with which credential.\n");

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

    // The address and the credential, before the facts they can settle.
    //
    // This used to open with "Region" and "Cell", which are two answers the
    // control plane already holds — and a machine being joined is holding the
    // one thing that can ask it. So the order is now what the operator was
    // handed: what this machine does, where the cell is, and the token. What
    // the cell can say, it says.
    let needs_api = roles.iter().any(|r| *r != Role::ControlPlane);
    let api_url = if needs_api {
        ask_valid(
            "\nControl-plane URL (https://host:8443): ",
            validate_url,
            "a URL with a scheme and a host",
        )?
    } else {
        String::new()
    };
    // The certificate, when the address is https.
    //
    // The agents refuse https without one — `api_cell` says so in as many words
    // — and this wizard never asked, so a machine joined interactively wrote a
    // seed its own agents would not accept, and the operator found out when a
    // unit failed to start. `migrate-seed` had the fix-up; the first install did
    // not. The default is where the control plane writes it and therefore where
    // it is copied to.
    //
    // Not for the control plane: that machine *is* the API and makes its own
    // certificate, so the cell's root is a file it will write rather than one
    // it has to be given.
    let api_ca = if api_url.starts_with("https://") && !roles.contains(&Role::ControlPlane) {
        println!("\nThe agents verify the API against the cell's own certificate. Copy");
        println!("/var/lib/velstra/tls/cert.pem from the control plane to this machine —");
        println!("it is a certificate, not a secret — and name it here.");
        let raw = prompt("Certificate [/var/lib/velstra/tls/cert.pem]: ")?;
        let path = match raw.trim() {
            "" => "/var/lib/velstra/tls/cert.pem".to_string(),
            other => other.to_string(),
        };
        // Taken whether or not it is there yet. An operator who runs this
        // before copying the file gets a sentence and a finished seed; a loop
        // would get them a wizard they cannot leave, on a machine where the
        // only other terminal is the one they are already in.
        if !std::path::Path::new(&path).exists() {
            println!("  {path} is not there yet — the seed will name it anyway.");
            println!("  Copy it from the control plane before starting the agents; until then");
            println!("  they will not talk to an https cell at all, and will say so.");
        }
        path
    } else {
        String::new()
    };

    let wants_token = roles.contains(&Role::Hypervisor) || roles.contains(&Role::Pool);
    let token = if wants_token {
        println!("\nThe token is the one an operator was shown once, when they created this");
        println!("machine's node object. It is what says which node this is.");
        ask_for_the_token()?
    } else {
        String::new()
    };

    let said = if api_url.is_empty() || token.is_empty() {
        Err(String::new())
    } else {
        ask_the_cell(&api_url, &token, &api_ca)
    };

    let (region, cell, node_from_cell) = match &said {
        Ok(c) => {
            println!(
                "\nThe cell answered: this token is node {} in cell {}, region {}.",
                c.node, c.cell, c.region
            );
            println!("Those three are taken from the cell rather than asked.");
            (c.region.clone(), c.cell.clone(), c.node.clone())
        }
        Err(why) => {
            if needs_api {
                if !why.is_empty() {
                    println!("\nThe cell did not answer: {why}");
                }
                println!("\nSo the next three are");
                println!("questions rather than facts. A control plane that is not up yet and an");
                println!("air-gapped install both land here; a wrong answer makes a machine that");
                println!("comes up and registers nowhere, so they are worth reading twice.");
            }
            println!("\nA cell is the failure domain: a machine belongs to exactly one.");
            println!("Working across cells is several cells, each with its own control plane —");
            println!(
                "and one of them told where the others are, so a client reaches one address.\n"
            );
            // Same defect as the installer's administrator prompt had: the
            // validator refused the empty answer before the map could turn it
            // into the default, so Enter at `[eu-central]` asked again.
            let region = ask_valid_or(
                "eu-central",
                "Region [eu-central]: ",
                validate_node_name,
                "lowercase letters, digits and '-'",
            )?;
            let cell = ask_valid_or(
                "cell-1",
                "Cell [cell-1]: ",
                validate_node_name,
                "lowercase letters, digits and '-'",
            )?;
            (region, cell, String::new())
        }
    };

    let mut m = Machine {
        hostname: String::new(),
        api_ca_pem: String::new(),
        advertise: String::new(),
        bootstrap_ceph_osds: Vec::new(),
        ssh_key: String::new(),
        root_password: String::new(),
        passthrough: String::new(),
        pool_token: String::new(),
        api_ca: api_ca.clone(),
        tls_cert: String::new(),
        tls_key: String::new(),
        lvm_group: String::new(),
        lvm_thin_pool: String::new(),
        ceph_conf: String::new(),
        ceph_user: String::new(),
        ceph_pool: String::new(),
        ceph_image_pool: String::new(),
        ceph_pool_id: String::new(),
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

    // Asked above, before anything the cell could settle.
    m.api_url = api_url.clone();
    m.token = token.clone();

    if roles.contains(&Role::Hypervisor) {
        if node_from_cell.is_empty() {
            println!(
                "\nThe node id has to match the node object an operator created — that object"
            );
            println!("is where the one-time token came from.");
            m.node = ask_valid(
                "Node id: ",
                validate_node_name,
                "lowercase letters, digits and '-'",
            )?;
        } else {
            // The token named it. Asking anyway would be asking somebody to
            // retype an answer that is already in hand, and to get it wrong.
            m.node = node_from_cell.clone();
        }
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
        // And the pool's own token, for the same reason the node has one: a
        // pool is a separate agent with a separate identity, and the API mints
        // it separately when the pool object is created.
        //
        // Never asked until now. A machine joined as hypervisor **and** pool
        // wrote a seed that looked complete, started both agents, and the pool
        // one answered `401 the bearer token was not accepted` on every pass —
        // because it was reading the node's token, the only one the wizard
        // knew about. The pool then never claimed a volume, never reported
        // capacity, and the cell went on accepting volumes into it.
        //
        // Not asked of a control plane: that machine's pool agent goes straight
        // to the store and is given no token on purpose.
        if !roles.contains(&Role::ControlPlane) {
            println!("The pool has its own token, shown once when its pool object was created.");
            m.pool_token = ask_for_a_token("Pool token", "VELSTRA_POOL_TOKEN")?;
        }
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
            m.lvm_thin_pool = prompt("Thin pool inside it, if any []: ")?
                .trim()
                .to_string();
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
            let pool = prompt("RBD pool for volumes [velstra-volumes]: ")?
                .trim()
                .to_string();
            m.ceph_pool = if pool.is_empty() {
                "velstra-volumes".to_string()
            } else {
                pool
            };
            let images = prompt("RBD pool for images [velstra-images]: ")?
                .trim()
                .to_string();
            m.ceph_image_pool = if images.is_empty() {
                "velstra-images".to_string()
            } else {
                images
            };
            // The cell's own name for this storage, which is a different
            // namespace from the RBD pools above: a volume asks for
            // `pool: ceph`, and the cluster is where `velstra-volumes` lives.
            let id = prompt("Pool object id in this cell [ceph]: ")?
                .trim()
                .to_string();
            m.ceph_pool_id = if id.is_empty() {
                "ceph".to_string()
            } else {
                id
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
        // The number or the name. The prompt prints both — `[2] hypervisor` —
        // and typing the word it just showed you was answered "hypervisor" is
        // not a number from 1 to 3, three times over, because the answers
        // afterwards go on being read as roles. Nothing is ambiguous between
        // the two spellings, so there is no reason to accept only one.
        let role = match token.parse::<usize>() {
            Ok(index) => *Role::ALL
                .get(index.wrapping_sub(1))
                .ok_or_else(|| format!("there is no role {index}"))?,
            Err(_) => {
                let want = token.to_ascii_lowercase();
                *Role::ALL
                    .iter()
                    .find(|r| r.as_str() == want)
                    .ok_or_else(|| {
                        format!(
                            "{token:?} is neither a number from 1 to {} nor one of {}",
                            Role::ALL.len(),
                            Role::ALL
                                .iter()
                                .map(|r| r.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?
            }
        };
        out.push(role);
    }
    if out.is_empty() {
        return Err("pick at least one — a machine with no role runs nothing".into());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The one test that makes "render writes it, parse reads it" structural.
///
/// Three keys had drifted apart before this existed — `VELSTRA_API_URL` (a
/// control plane's seed could not be read back at all), `VELSTRA_TLS_CERT`
/// and `VELSTRA_TLS_KEY` (so `bootstrap-cell` thought its own cell spoke
/// plain HTTP), `VELSTRA_HOSTNAME` and the two LVM keys (dropped on every
/// rewrite). Each side had tests. Nothing ran a value through both.
///
/// So: populate every field, render, parse, compare. A key added to one side
/// and not the other fails here, by name, the first time somebody runs the
/// suite.
#[cfg(test)]
mod round_trip {
    use super::*;

    #[test]
    fn seed_survives_a_round_trip() {
        let m = Machine {
            region: "eu-west".into(),
            cell: "cell-9".into(),
            roles: vec![Role::ControlPlane, Role::Hypervisor, Role::Pool],
            hostname: "horst".into(),
            advertise: "https://horst:8443,https://10.0.0.8:8443".into(),
            bootstrap_ceph_osds: vec!["/dev/disk/by-id/a".into(), "/dev/disk/by-id/b".into()],
            ssh_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA nobody@example".into(),
            passthrough: "10de:2204,10de:1aef".into(),
            api_url: "https://localhost:8443".into(),
            node: "horst".into(),
            vmm: "qemu".into(),
            pool: "local".into(),
            api_ca: "/var/lib/velstra/tls/cert.pem".into(),
            tls_cert: "/var/lib/velstra/tls/cert.pem".into(),
            tls_key: "/var/lib/velstra/tls/key.pem".into(),
            pool_backend: "lvm-thin".into(),
            lvm_group: "vg0".into(),
            lvm_thin_pool: "thin".into(),
            ceph_conf: "/var/lib/velstra/ceph/ceph.conf".into(),
            ceph_user: "velstra".into(),
            ceph_pool: "rbd".into(),
            ceph_image_pool: "images".into(),
            ceph_pool_id: "ceph".into(),
            store: "127.0.0.1:2379".into(),
            cells: vec!["cell-2=https://cell-2:8443".into()],
            local_network: true,
            listen: "0.0.0.0:8443".into(),
            admin: "admin".into(),
            fabric: Some(Fabric {
                orchestrator: "https://fabric:9443".into(),
                control: "10.0.0.8:4789".into(),
                vtep: "10.0.0.8".into(),
                underlay: "eth0".into(),
                srv6_locator: String::new(),
            }),
            // Deliberately not in the file, each for a stated reason, so each
            // is compared against what it *should* come back as rather than
            // against itself.
            token: "ab".repeat(32),
            pool_token: "cd".repeat(32),
            admin_password: "correcthorsebattery".into(),
            root_password: "hunter2hunter2".into(),
            api_ca_pem: "-----BEGIN CERTIFICATE-----\n".into(),
        };

        let rendered = render(&m);
        let back = parse(&rendered).expect("a rendered seed parses");

        // Everything the file is allowed to carry comes back unchanged.
        assert_eq!(back.region, m.region, "{rendered}");
        assert_eq!(back.cell, m.cell, "{rendered}");
        assert_eq!(back.roles, m.roles, "{rendered}");
        assert_eq!(back.hostname, m.hostname, "{rendered}");
        assert_eq!(back.advertise, m.advertise, "{rendered}");
        assert_eq!(
            back.bootstrap_ceph_osds, m.bootstrap_ceph_osds,
            "{rendered}"
        );
        assert_eq!(back.ssh_key, m.ssh_key, "{rendered}");
        assert_eq!(back.passthrough, m.passthrough, "{rendered}");
        assert_eq!(back.api_url, m.api_url, "{rendered}");
        assert_eq!(back.node, m.node, "{rendered}");
        assert_eq!(back.vmm, m.vmm, "{rendered}");
        assert_eq!(back.pool, m.pool, "{rendered}");
        assert_eq!(back.api_ca, m.api_ca, "{rendered}");
        assert_eq!(back.tls_cert, m.tls_cert, "{rendered}");
        assert_eq!(back.tls_key, m.tls_key, "{rendered}");
        assert_eq!(back.pool_backend, m.pool_backend, "{rendered}");
        assert_eq!(back.lvm_group, m.lvm_group, "{rendered}");
        assert_eq!(back.lvm_thin_pool, m.lvm_thin_pool, "{rendered}");
        assert_eq!(back.ceph_conf, m.ceph_conf, "{rendered}");
        assert_eq!(back.ceph_user, m.ceph_user, "{rendered}");
        assert_eq!(back.ceph_pool, m.ceph_pool, "{rendered}");
        assert_eq!(back.ceph_image_pool, m.ceph_image_pool, "{rendered}");
        assert_eq!(back.ceph_pool_id, m.ceph_pool_id, "{rendered}");
        assert_eq!(back.store, m.store, "{rendered}");
        assert_eq!(back.cells, m.cells, "{rendered}");
        assert_eq!(back.local_network, m.local_network, "{rendered}");
        assert_eq!(back.listen, m.listen, "{rendered}");
        assert_eq!(back.admin, m.admin, "{rendered}");
        assert_eq!(back.fabric, m.fabric, "{rendered}");

        // And the secrets are not in it. Each of these lives in its own 0600
        // file beside the seed; `node.env` is world-readable.
        for secret in [
            &m.token,
            &m.pool_token,
            &m.admin_password,
            &m.root_password,
            &m.api_ca_pem,
        ] {
            assert!(
                !rendered.contains(secret.trim()),
                "a secret reached node.env: {rendered}"
            );
        }
        assert!(back.token.is_empty(), "{:?}", back.token);
        assert!(back.pool_token.is_empty(), "{:?}", back.pool_token);
        assert!(back.api_ca_pem.is_empty(), "{:?}", back.api_ca_pem);
        // The console password is a marker in the file and the secret in a
        // file of its own, so what comes back is "there is one", not the one.
        assert!(!back.root_password.is_empty(), "the marker is lost");
    }
}

#[cfg(test)]
mod tests {
    /// A pasted token is taken as the paste arrives.
    ///
    /// 64 hex characters is not something anybody types correctly, and a fresh
    /// machine is exactly where somebody is at a console with no clipboard. So
    /// what arrives is what a terminal does to a paste: a trailing newline, a
    /// line break in the middle, and whatever case the thing was displayed in.
    #[test]
    fn a_pasted_token_is_taken_as_it_arrives() {
        let real = "ab".repeat(32);
        assert_eq!(super::tidy_token(&format!("  {real}\n")), real);
        assert_eq!(super::tidy_token(&real.to_uppercase()), real);
        let split = format!("{} {}", &real[..32], &real[32..]);
        assert_eq!(super::tidy_token(&split), real);
        assert!(crate::wizard::validate_token(&super::tidy_token(&format!("{real}\r\n"))).is_ok());
    }

    /// The cell's answer, read into the three facts it settles.
    ///
    /// The bodies are the ones this API serves: the first was taken off a live
    /// cell, the second is the same route on a control plane old enough not to
    /// name the cell — which accepts the token and therefore must not be read
    /// as a refusal.
    #[test]
    fn the_cells_answer_settles_the_node_the_region_and_the_cell() {
        let full = r#"{"cellAdmin":false,"displayName":"","projects":{},"session":false,"subject":"node:qa-join","region":"eu-central","cell":"cell-1"}"#;
        let said = super::cell_from_body(full).expect("a complete answer");
        assert_eq!(said.node, "qa-join");
        assert_eq!(said.region, "eu-central");
        assert_eq!(said.cell, "cell-1");

        // Taken off the live cell before it carried the cell's name. The token
        // was accepted — the subject proves it — so what comes back says which
        // node it is and that the cell is the one thing still to ask.
        let older = r#"{"cellAdmin":false,"displayName":"","projects":{},"session":false,"subject":"node:qa-join"}"#;
        let why = super::cell_from_body(older).expect_err("no cell named");
        assert!(
            why.contains("qa-join") && why.contains("older"),
            "the reason has to name the node it did accept: {why}"
        );

        // A refusal names no subject at all, and saying which body came back is
        // the whole of what a reader can act on.
        let refused =
            r#"{"error":{"code":"UNAUTHENTICATED","message":"the bearer token was not accepted"}}"#;
        let why = super::cell_from_body(refused).expect_err("a refusal");
        assert!(why.contains("not accepted"), "{why}");
    }

    /// The cell names an agent `node:<id>`; the seed wants the id.
    #[test]
    fn the_subject_the_cell_answers_with_names_the_node() {
        assert_eq!(super::node_from_subject("node:peter"), "peter");
        // No prefix, no change: a different control plane is not reshaped to
        // fit a spelling it never promised.
        assert_eq!(super::node_from_subject("peter"), "peter");
        assert_eq!(super::node_from_subject(""), "");
    }

    use super::*;

    pub(super) fn hypervisor() -> Machine {
        Machine {
            hostname: String::new(),
            api_ca_pem: String::new(),
            advertise: String::new(),
            bootstrap_ceph_osds: Vec::new(),
            ssh_key: String::new(),
            root_password: String::new(),
            passthrough: String::new(),
            pool_token: String::new(),
            api_ca: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            lvm_group: String::new(),
            lvm_thin_pool: String::new(),
            ceph_conf: String::new(),
            ceph_user: String::new(),
            ceph_pool: String::new(),
            ceph_image_pool: String::new(),
            ceph_pool_id: String::new(),
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
        assert!(
            seed.contains("VELSTRA_CEPH_CONF=/etc/ceph/ceph.conf"),
            "{seed}"
        );
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
pub fn migrate_seed(dir: &std::path::Path, identity: &std::path::Path) -> Result<Vec<String>> {
    let mut changed = settle_identity(dir, identity)?;
    let path = seed_path(dir, identity);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(changed);
    };
    let cert = dir.join("tls").join("cert.pem");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

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
        changed.push(format!(
            "VELSTRA_API_URL is now {now}: this machine serves TLS"
        ));
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

    if lines.join("\n") == text.trim_end_matches('\n') {
        return Ok(changed);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(changed)
}

/// Which file *is* the seed: the machine's own, or — on a machine installed
/// before there was such a thing — the one in the state directory.
///
/// Whole-file, never merged. A merge would be the systemd behaviour and it is
/// the wrong one here: the keys the machine's own file does not mention would
/// still come from somebody else's, so a control plane would inherit a
/// hypervisor's pool and a hypervisor its neighbour's node id. One file
/// answers, or the other does.
pub fn seed_path(state: &std::path::Path, identity: &std::path::Path) -> std::path::PathBuf {
    let mine = identity.join("node.env");
    if mine.exists() {
        mine
    } else {
        state.join("node.env")
    }
}

/// Move a seed out of the shared state directory and into the machine's own.
///
/// Run on every upgrade. A machine installed before identity was separated has
/// its seed in the state directory, which may be — and on the cell that found
/// this, was — the same filesystem on every machine. Moving rather than
/// copying is the point: a copy would leave the file that does the renaming.
///
/// Does nothing when the machine already has its own, and nothing when there is
/// no seed at all, which is what a freshly unpacked package looks like.
fn settle_identity(state: &std::path::Path, identity: &std::path::Path) -> Result<Vec<String>> {
    let from = state.join("node.env");
    let to = identity.join("node.env");
    if to.exists() || !from.exists() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&from).with_context(|| format!("reading {}", from.display()))?;
    std::fs::create_dir_all(identity)
        .with_context(|| format!("creating {}", identity.display()))?;
    std::fs::write(&to, &text).with_context(|| format!("writing {}", to.display()))?;
    // Only after the new one is on disk. A rename that failed halfway would be
    // a machine with no seed at all, which is a machine that stops.
    std::fs::remove_file(&from).with_context(|| format!("removing {}", from.display()))?;
    Ok(vec![format!(
        "the seed moved to {}: the state directory can be shared between machines, and \
         a seed there is one machine answering to another's name",
        to.display()
    )])
}

#[cfg(test)]
mod migrating_a_seed {
    use super::*;

    /// A machine as an upgrade finds it: a state directory holding the seed,
    /// and an `/etc` that does not have one yet.
    fn scratch(
        name: &str,
        seed: &str,
        with_cert: bool,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("velstra-seed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (state, identity) = (root.join("var"), root.join("etc"));
        std::fs::create_dir_all(state.join("tls")).unwrap();
        std::fs::write(state.join("node.env"), seed).unwrap();
        if with_cert {
            std::fs::write(state.join("tls").join("cert.pem"), "x").unwrap();
        }
        (state, identity)
    }

    #[test]
    fn a_seed_from_before_tls_is_pointed_at_the_certificate() {
        // The upgrade that would otherwise cut a node off from its own cell.
        let (state, identity) = scratch(
            "old",
            "VELSTRA_ROLES=control-plane,hypervisor\nVELSTRA_API_URL=http://127.0.0.1:8443\n",
            true,
        );
        let said = migrate_seed(&state, &identity).unwrap();
        // Three now: the move, and the two the TLS migration makes.
        assert_eq!(said.len(), 3, "{said:?}");
        let seed = std::fs::read_to_string(identity.join("node.env")).unwrap();
        assert!(
            seed.contains("VELSTRA_API_URL=https://localhost:8443"),
            "{seed}"
        );
        assert!(seed.contains("VELSTRA_API_CA="), "{seed}");
        // Idempotent: a second upgrade says nothing and changes nothing.
        assert!(migrate_seed(&state, &identity).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn a_cell_without_a_certificate_is_left_alone() {
        // Plaintext is a supported configuration. Rewriting it would break a
        // machine that was working. The move still happens: where the seed
        // lives is not a question about TLS.
        let (state, identity) = scratch("plain", "VELSTRA_API_URL=http://127.0.0.1:8443\n", false);
        assert_eq!(migrate_seed(&state, &identity).unwrap().len(), 1);
        let seed = std::fs::read_to_string(identity.join("node.env")).unwrap();
        assert!(seed.contains("http://127.0.0.1:8443"), "{seed}");
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn somebody_elses_control_plane_is_not_guessed_at() {
        // This machine's certificate says nothing about whether the cell it
        // joined serves TLS, so the seed is not touched.
        let (state, identity) =
            scratch("remote", "VELSTRA_API_URL=http://cell.example:8443\n", true);
        assert_eq!(migrate_seed(&state, &identity).unwrap().len(), 1);
        let seed = std::fs::read_to_string(identity.join("node.env")).unwrap();
        assert!(seed.contains("http://cell.example:8443"), "{seed}");
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
    }

    /// The one that cost a cell its control plane.
    ///
    /// Two machines, one shared state directory. The second to be set up wrote
    /// its seed there, and from then on the first read it: `has-role
    /// control-plane` answered no, the API and the controller were skipped as
    /// "not for this machine", and the whole cell went dark on a package
    /// upgrade — with systemd reporting it as a condition politely unmet.
    #[test]
    fn a_shared_state_directory_cannot_rename_the_machine_that_mounts_it() {
        let (state, identity) = scratch(
            "shared",
            "VELSTRA_NODE=peter\nVELSTRA_ROLES=hypervisor,pool\nVELSTRA_POOL=local-2\n",
            false,
        );
        // The other machine's, already where it belongs.
        std::fs::create_dir_all(&identity).unwrap();
        std::fs::write(
            identity.join("node.env"),
            "VELSTRA_NODE=horst\nVELSTRA_ROLES=control-plane,hypervisor\n",
        )
        .unwrap();

        // Nothing moves: this machine already knows who it is, and the shared
        // file is somebody else's.
        assert!(migrate_seed(&state, &identity).unwrap().is_empty());
        let seed = std::fs::read_to_string(seed_path(&state, &identity)).unwrap();
        assert!(seed.contains("VELSTRA_NODE=horst"), "{seed}");
        // And not a word of the neighbour's, which is the point of reading one
        // file rather than merging two: a control plane does not inherit a
        // hypervisor's pool.
        assert!(!seed.contains("local-2"), "{seed}");
        assert!(crate::roles::has_role(&seed, "control-plane").unwrap());
        let _ = std::fs::remove_dir_all(state.parent().unwrap());
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
        assert!(
            text.contains("ETCD_NAME=cell-1"),
            "their other settings went"
        );
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

#[cfg(test)]
mod pool_token_tests {
    use super::*;

    /// A machine that holds volumes gets the pool's own credential, in its own
    /// file, and not the node's.
    ///
    /// The two are different identities — `pool:<id>` against `node:<id>` — and
    /// the API mints them separately. A machine joined as hypervisor and pool
    /// used to be given only the node's, and its pool agent answered
    /// `401 the bearer token was not accepted` on every pass, for ever, with
    /// the seed looking complete.
    #[test]
    fn a_pool_gets_its_own_token_file() {
        let dir = std::env::temp_dir().join(format!("velstra-pool-token-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut m = tests::hypervisor();
        m.roles = vec![Role::Hypervisor, Role::Pool];
        m.pool = "bulk".into();
        m.token = "a".repeat(64);
        m.pool_token = "b".repeat(64);
        write_seed(&dir, &m).expect("the seed is written");

        let node = fs::read_to_string(dir.join("node-token")).expect("a node token");
        let pool = fs::read_to_string(dir.join("pool-token")).expect("a pool token");
        assert_eq!(node.trim(), "a".repeat(64));
        assert_eq!(pool.trim(), "b".repeat(64));
        assert_ne!(node, pool, "the pool was given the node's token");

        // Neither is in the file every unit reads.
        let seed = fs::read_to_string(dir.join("node.env")).expect("a seed");
        assert!(!seed.contains(&"a".repeat(64)), "{seed}");
        assert!(!seed.contains(&"b".repeat(64)), "{seed}");

        // And the control plane is given none: its pool agent reaches the store
        // directly, and a token there would be one nothing reads.
        let mut cp = m.clone();
        cp.roles = vec![Role::ControlPlane, Role::Hypervisor, Role::Pool];
        let cp_dir = dir.join("cp");
        write_seed(&cp_dir, &cp).expect("the seed is written");
        assert!(!cp_dir.join("pool-token").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod role_spelling_tests {
    use super::*;

    /// The roles question takes the word it printed, not only the number.
    ///
    /// Measured: the prompt lists `[2] hypervisor` and `[3] pool`, and
    /// answering `hypervisor pool` was refused with "not a number from 1 to 3"
    /// — after which the URL and the certificate were read as roles too, and
    /// the wizard derailed three questions deep. Typing the name it just showed
    /// is the obvious thing to do.
    #[test]
    fn the_roles_question_takes_a_name_as_well_as_a_number() {
        let by_number = resolve_roles("2 3").expect("numbers");
        let by_name = resolve_roles("hypervisor pool").expect("names");
        let mixed = resolve_roles("2 pool").expect("both");
        assert_eq!(by_number, by_name);
        assert_eq!(by_number, mixed);
        assert_eq!(by_number, vec![Role::Hypervisor, Role::Pool]);
        assert_eq!(
            resolve_roles("CONTROL-PLANE").expect("case is not the point"),
            vec![Role::ControlPlane]
        );

        // And a word that is not a role says so, with the words that are.
        let why = resolve_roles("storage").expect_err("not a role");
        assert!(why.contains("control-plane"), "{why}");
        assert!(why.contains("hypervisor"), "{why}");
        assert!(why.contains("pool"), "{why}");
    }
}

#[cfg(test)]
mod join_tests {
    use velstra_cloud_wire::join::{JoinToken, PoolJoin};

    use super::*;

    fn token(pool: bool) -> String {
        JoinToken {
            v: 1,
            region: "eu-central".into(),
            cell: "cell-1".into(),
            node: "peter".into(),
            urls: vec![
                "https://10.10.10.8:8443".into(),
                "https://horst:8443".into(),
            ],
            ca: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
            token: "ab".repeat(32),
            pool: pool.then(|| PoolJoin {
                id: "local-2".into(),
                token: "cd".repeat(32),
            }),
        }
        .encode()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("velstra-join-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// One string in, a machine out: every fact the six routes used to carry,
    /// and the certificate as a file beside the seed where the agents look.
    #[test]
    fn a_join_token_is_the_whole_answer() {
        let dir = scratch("node");
        let m = from_join(&token(false), &dir).expect("a machine");
        assert_eq!(m.roles, vec![Role::Hypervisor]);
        assert_eq!(m.region, "eu-central");
        assert_eq!(m.cell, "cell-1");
        assert_eq!(m.node, "peter");
        assert_eq!(m.api_url, "https://10.10.10.8:8443");
        assert_eq!(m.vmm, "qemu", "the only hypervisor that can open Ceph");
        assert_eq!(m.api_ca, dir.join("api-ca.pem").display().to_string());

        write_seed(&dir, &m).expect("written");
        let env = fs::read_to_string(dir.join("node.env")).unwrap();
        assert!(env.contains("VELSTRA_ROLES=hypervisor\n"), "{env}");
        assert!(
            env.contains(&format!("VELSTRA_API_CA={}\n", m.api_ca)),
            "{env}"
        );
        assert!(
            env.contains("VELSTRA_API_URL=https://10.10.10.8:8443\n"),
            "{env}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("api-ca.pem")).unwrap(),
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("node-token")).unwrap(),
            format!("{}\n", "ab".repeat(32))
        );
        // The token is a secret and the seed is world-readable.
        assert!(!env.contains(&"ab".repeat(32)), "{env}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pool rides along as a second role with its own credential.
    #[test]
    fn a_pool_in_the_token_is_a_second_role_with_its_own_token() {
        let dir = scratch("pool");
        let m = from_join(&token(true), &dir).expect("a machine");
        assert_eq!(m.roles, vec![Role::Hypervisor, Role::Pool]);
        assert_eq!(m.pool, "local-2");
        assert_eq!(m.pool_backend, "directory");
        write_seed(&dir, &m).expect("written");
        assert_eq!(
            fs::read_to_string(dir.join("pool-token")).unwrap(),
            format!("{}\n", "cd".repeat(32))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The wrong paste is refused by name, before anything is written.
    #[test]
    fn the_wrong_paste_is_refused_by_name() {
        let dir = scratch("bad");
        let err = from_join("https://horst:8443", &dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a join token"), "{err}");
        assert!(!dir.exists(), "nothing may be written on refusal");
    }
}
