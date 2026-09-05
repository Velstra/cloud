//! The agent, as a process.
//!
//! One node, one cell, one stream. It serves the metadata service for the
//! guests on this machine and runs the reconcile loop until it is asked to
//! stop — and stopping it does not stop the guests, which is the whole point of
//! [`velstra_cloud_nodeagent::cloud_hypervisor`] putting them under systemd.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use clap::{Parser, ValueEnum};
use velstra_cloud_nodeagent::{
    Agent, AgentConfig, Boot, Datapath, FakeDatapath, FakeVmm, Layout, Scope, Vmm,
    api_cell::ApiCell,
    cloud_hypervisor::CloudHypervisorVmm,
    datapath::TapDatapath,
    dhcp,
    fabric::{FabricDatapath, Underlay},
    metadata,
    qemu::QemuVmm,
};
use velstra_cloud_store::{EtcdStore, MemoryStore, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum VmmKind {
    /// In-process guests. Real enough to run the platform against, and the
    /// only one that works without a hypervisor on the machine.
    Fake,
    /// One `cloud-hypervisor` process per guest, under its own systemd slice.
    CloudHypervisor,
    /// One `qemu-system-*` process per guest, driven over QMP.
    Qemu,
}

#[derive(Debug, Parser)]
#[command(
    name = "velstra-cloud-nodeagent",
    about = "Watches the objects assigned to this node, does what they ask, reports what is"
)]
struct Args {
    /// Where the state lives: `memory`, or one or more etcd endpoints.
    ///
    /// `memory` is a single process talking to itself — useful for a demo and
    /// for nothing else, because the state dies with the process and no second
    /// binary can see it. Anything else is read as a comma-separated list of
    /// etcd endpoints, which is what makes the API, the controllers and the
    /// node agents parts of one cell rather than three separate universes.
    #[arg(long, env = "VELSTRA_STORE", default_value = "memory")]
    store: String,
    /// This node's id. It must match the id in the node object, because that
    /// is what every assignment is written against.
    #[arg(long)]
    node: String,

    #[arg(long)]
    cell: String,

    #[arg(long)]
    region: String,

    #[arg(long, value_enum, default_value_t = VmmKind::Fake)]
    vmm: VmmKind,

    /// Where the console service listens.
    ///
    /// The **API** connects here, never a browser: a tenant's browser has no
    /// business on the machines a cell is made of, and a console it could reach
    /// directly would be one it could reach without asking the API first.
    ///
    /// Not fatal to bind: a node whose console port is taken still runs guests,
    /// and it reports no console endpoint rather than one that answers nothing.
    #[arg(long, default_value = "0.0.0.0:8447")]
    console_listen: SocketAddr,

    /// What to tell the cell this node's console address is.
    ///
    /// Empty derives it, which is what a single-homed machine wants: the local
    /// address this node would use to reach the API is by definition one the API
    /// can reach it back on. A node behind an address translation, or one that
    /// should be reached on a different interface, says so here.
    #[arg(long, default_value = "")]
    console_advertise: String,

    /// Where the metadata service listens. The guests' side of it is always
    /// `169.254.169.254:80`; this is configurable so a test, or a node whose
    /// link-local address is not up yet, can bind somewhere else.
    #[arg(long, default_value = "169.254.169.254:80")]
    metadata_listen: SocketAddr,

    /// Answer DHCP on the taps of the guests this node runs.
    ///
    /// On by default, because a guest that boots a stock cloud image asks for
    /// its address before it can ask anything else, and a node that does not
    /// answer leaves it with no network and no way to say so. Turn it off on a
    /// cell where something else is authoritative for these subnets — two
    /// servers answering one guest is how an address ends up in two places.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    dhcp: bool,

    /// How long a guest may hold its address before asking again. Nothing is
    /// reserved, so this is a staleness bound rather than a resource question.
    #[arg(long, default_value = "3600")]
    dhcp_lease_secs: u64,

    /// A public key an image's signature must verify under before this node
    /// fetches it: Ed25519, the raw 32 bytes as base64. Repeat the flag, or
    /// separate keys with commas in the variable. The same keys the API was
    /// started with, normally.
    #[arg(long, env = "VELSTRA_IMAGE_SIGNING_KEYS", value_delimiter = ',')]
    image_signing_key: Vec<String>,

    /// Fetch signed images only: an image with no signature is refused with
    /// the reason on the guest that needed it. Needs at least one key.
    #[arg(long, env = "VELSTRA_REQUIRE_SIGNED_IMAGES")]
    require_signed_images: bool,

    /// How often the responder looks for taps that have appeared or gone.
    #[arg(long, default_value = "2")]
    dhcp_scan_secs: u64,

    /// Read and write the cell through the API instead of the store, as this
    /// node's own token.
    ///
    /// Two things at once, and both matter. **Reads** are bounded: without this,
    /// the agent lists every instance, port, attachment and migration in the cell
    /// on every pass and watches them unfiltered, so a thousand nodes are a
    /// thousand watchers on one store; with it, the API serves every node from
    /// one watch per collection and hands each one its own objects. **Writes**
    /// are a trust boundary: a status report goes through the API authenticated
    /// as this node, and the API refuses anything that is not this node's — so
    /// the credential a node holds can write only its own objects' status, which
    /// the direct-store default (a shared operator token) never was.
    #[arg(long)]
    api: Option<String>,

    /// The per-node token for `--api`, from the `nodeToken` a registration
    /// returned once. A file rather than a flag, so it is not in anybody's
    /// process list.
    #[arg(long)]
    api_token_file: Option<PathBuf>,

    /// The floor under everything a watch can miss.
    #[arg(long, default_value = "30")]
    resync_secs: u64,

    /// Where guests and images live, for the cloud-hypervisor backend.
    #[arg(long, default_value = "/var/lib/velstra")]
    state_dir: PathBuf,

    /// What this node offers for guest disks, in GiB. Reported to the
    /// scheduler; not derivable from `std`, so it is stated rather than
    /// guessed at.
    #[arg(long, default_value = "0")]
    disk_gib: u64,

    /// The address other nodes reach this one at to move a guest here.
    ///
    /// Left out, this node only accepts a transfer over a unix socket — one
    /// machine, which is what an in-place VMM upgrade is. Nothing on the
    /// machine can tell which of its addresses its peers route to, so it is
    /// stated rather than guessed at.
    #[arg(long)]
    migration_address: Option<String>,

    /// This machine's state directory is storage every node in the cell reaches.
    ///
    /// Stated rather than guessed at, like `--migration-address` and for a
    /// stronger reason: nothing on a machine can tell whether
    /// `/var/lib/velstra` is a local filesystem or one mount of an NFS export
    /// that every other node has too, and being wrong about it is a guest
    /// started twice on two machines over the same disk.
    ///
    /// It is what makes moving a guest possible. A migration transfers memory
    /// and not disks, so a guest whose root disk is private to one machine
    /// cannot arrive anywhere — `may_migrate` refuses the move and says so
    /// rather than leaving it to hang.
    #[arg(long)]
    shared_state: bool,

    /// The first port a receiver may bind. One arriving guest needs one port.
    #[arg(long, default_value = "4900")]
    migration_port_first: u16,

    #[arg(long, default_value = "4950")]
    migration_port_last: u16,

    /// Certificates for an encrypted transfer. Only used over TCP — both VMMs
    /// refuse TLS over a unix socket, and there is no network between the two
    /// ends of one of those.
    #[arg(long)]
    migration_tls_dir: Option<PathBuf>,

    /// Boot every guest from this kernel, with the host supplying the command
    /// line (`--boot-cmdline`) and optionally an initramfs (`--boot-initrd`).
    ///
    /// This is not an exotic mode. **Cloud Hypervisor has no firmware of its
    /// own**: `--kernel` takes a Linux kernel or a PVH blob and nothing else, and
    /// a BIOS-partitioned cloud image simply does not boot. Until this flag
    /// existed the model could express a kernel boot and no command line could
    /// ask for one, so the cloud-hypervisor backend could be selected and had no
    /// way to start anything.
    ///
    /// Left out, guests boot from firmware (`--boot-firmware`) and their own
    /// disk, which is what QEMU does by default and what a cloud image expects.
    #[arg(long)]
    boot_kernel: Option<PathBuf>,

    #[arg(long)]
    boot_initrd: Option<PathBuf>,

    /// The kernel command line. `console=ttyS0` is not decoration: without it a
    /// kernel boots perfectly and says nothing, which is indistinguishable from
    /// not booting at all.
    #[arg(long, default_value = "console=ttyS0 root=/dev/vda rw")]
    boot_cmdline: String,

    /// Firmware to boot guests with, when they boot from their own disk. Unset
    /// leaves it to the VMM's own default, which is what QEMU's SeaBIOS is.
    #[arg(long)]
    boot_firmware: Option<PathBuf>,

    /// Run guests as this user's own units (`systemd-run --user`) instead of
    /// system units.
    ///
    /// A data path that can only be exercised as root is one nobody exercises.
    /// The default stays `system`, because that is what a node in a cell is.
    #[arg(long, default_value = "system")]
    scope: ScopeKind,

    /// The VMM to run, when it is not simply on the path.
    ///
    /// Guests are started as systemd units, and a unit does **not** inherit the
    /// PATH of whoever started the agent — a system unit gets the system default.
    /// So a hypervisor installed anywhere else (a Nix profile, `/opt`, a
    /// hand-built binary) is not found by name, and the only symptom is a guest
    /// that will not start on a machine where the VMM plainly works.
    ///
    /// Left out, the backend's own name is used, which is right when the VMM is
    /// installed the way the distribution installs it.
    #[arg(long)]
    vmm_binary: Option<String>,

    /// What gives a guest its network.
    ///
    /// The default follows `--vmm`, because the pairing anybody wants is the
    /// obvious one: a fake hypervisor with a fake network, a real one with real
    /// taps. Naming it explicitly is for the case those come apart — a real
    /// hypervisor on a machine where something else already made the taps.
    #[arg(long, value_enum)]
    datapath: Option<DatapathKind>,

    /// Prefix for the tap devices this node makes, and how it recognises its
    /// own.
    ///
    /// **Every node in a cell must use the same one.** A live migration carries
    /// the guest's configuration to the destination, and that configuration
    /// names the tap — the destination opens the device the source named rather
    /// than one of its own. The rest of the name is derived from the port, so
    /// every node computes the same one; the prefix is the only part anybody can
    /// set differently, and setting it differently means guests on that node
    /// cannot be moved.
    ///
    /// Two agents on **one machine** are the exception that proves it: they need
    /// two prefixes to stay out of each other's way, and a guest with a NIC
    /// cannot be migrated between them anyway, because there is only one device.
    #[arg(long, default_value = "vt")]
    tap_prefix: String,

    /// Be the far end of the wire: hold each segment's gateway, and let its
    /// guests out through this node.
    ///
    /// Without this, `--datapath tap` creates a tap that leads **nowhere** —
    /// which is correct on a node whose fabric carries the segment, and a silent
    /// dead end on one without. The dead end costs more than reachability: a
    /// guest whose cloud-init cannot reach `169.254.169.254` over its own link
    /// gets no user, no SSH key and no network configuration, and boots to a
    /// login prompt that nothing opens.
    ///
    /// So: on for one box that is the whole cell, off for a node joining a cell
    /// that has a fabric. Needs `nft` and CAP_NET_ADMIN.
    #[arg(
        long,
        env = "VELSTRA_LOCAL_NETWORK",
        default_value_t = false,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = truthy,
    )]
    local_network: bool,

    /// The bridge-name prefix, for the same reason `--tap-prefix` exists: two
    /// agents on one machine need two, and everything else derives from the
    /// subnet.
    #[arg(long, default_value = "vbr")]
    bridge_prefix: String,

    /// The uid created taps are owned by, so a guest running as that user can
    /// open one. Unset means this process's own, which is what a root agent
    /// running root guests wants.
    #[arg(long)]
    tap_owner: Option<u32>,

    /// The Velstra fabric orchestrator, for `--datapath fabric`.
    ///
    /// This is what turns a tap into a tenant port: the VNI of its network, the
    /// address and MAC the platform allocated, and the security groups resolved
    /// into rules the data plane enforces. Without it a port carrying groups is
    /// refused rather than programmed unfiltered.
    #[arg(long)]
    fabric: Option<String>,

    /// The address other hosts send this one's encapsulated frames to.
    ///
    /// Stated rather than guessed at, for the same reason `--migration-address`
    /// is: nothing on a machine can tell which of its addresses its peers route
    /// to, and picking one would be picking wrong on every host with more than
    /// one interface.
    #[arg(long)]
    fabric_vtep: Option<String>,

    /// The interface that address is on. Its MAC is read from the machine.
    #[arg(long)]
    fabric_underlay: Option<String>,

    /// This host's SRv6 locator, as `prefix/len` (e.g. `fc00:0:1::/64`).
    ///
    /// Setting it puts the host on the SRv6 wire family instead of VXLAN. Stated
    /// rather than derived, and for a different reason than `--fabric-vtep`: a
    /// locator is a slice of the operator's own IPv6 address plan, has to be
    /// routable in the underlay, and has to be unique per host. Nothing on the
    /// machine knows any of that.
    ///
    /// Everything else SRv6 needs — this host's tunnel source, and the service
    /// SID of every segment it serves — is derived from this one value, so it is
    /// the only SRv6 knob a node has.
    #[arg(long)]
    fabric_srv6_locator: Option<String>,
}

/// Which systemd scope guests run in.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ScopeKind {
    System,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DatapathKind {
    /// A network in a process: ports are programmed by being remembered. Only
    /// honest alongside `--vmm fake`.
    Fake,
    /// Real tap devices on this machine, created and labelled here. Gives a
    /// guest a wire and enforces nothing, so it refuses a port carrying
    /// security groups or a ceiling instead of reporting them in force.
    Tap,
    /// The same taps, plus a port on the Velstra fabric's overlay with the
    /// tenant's rules actually programmed. Needs `--fabric`.
    Fabric,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Chosen before any TLS is spoken, because rustls will not choose: with
    // two providers compiled in (reqwest brings one, tokio-rustls the other) it
    // panics at first use — the same failure the API had, found the same way,
    // on a machine serving real traffic. This time it took the whole agent
    // down in a restart loop.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // A kernel wins over firmware when both are given: naming one is the more
    // specific instruction, and refusing the pair would be refusing a command
    // line somebody could reasonably build from a config template.
    let boot = match args.boot_kernel.clone() {
        Some(kernel) => Boot::Kernel {
            kernel,
            cmdline: args.boot_cmdline.clone(),
            initrd: args.boot_initrd.clone(),
        },
        None => Boot::Firmware(args.boot_firmware.clone()),
    };
    let layout = Layout {
        run_dir: args.state_dir.join("instances"),
        image_dir: args.state_dir.join("images"),
        incoming_dir: args.state_dir.join("images/incoming"),
        disk_gib: args.disk_gib,
        migration_address: args.migration_address.clone(),
        migration_ports: args.migration_port_first..args.migration_port_last,
        migration_tls_dir: args.migration_tls_dir.clone(),
        boot,
        scope: match args.scope {
            ScopeKind::System => Scope::System,
            ScopeKind::User => Scope::User,
        },
        ..Default::default()
    };
    // Taken before the hypervisor is built, because building one moves fields
    // out of the layout — and the console service needs only where a guest's
    // directory is.
    let console_layout = layout.clone();
    // A fake hypervisor gets a fake network and a real one gets real taps,
    // unless told otherwise. Pairing a real VMM with the fake network is what
    // used to happen unconditionally, and it does not merely program nothing: a
    // remembered tap does not exist on the machine, and both backends name the
    // tap on their command line and require it to be there, so an instance with
    // a port could not start at all.
    let datapath: Arc<dyn Datapath> = match args.datapath.unwrap_or(match args.vmm {
        VmmKind::Fake => DatapathKind::Fake,
        VmmKind::CloudHypervisor | VmmKind::Qemu => DatapathKind::Tap,
    }) {
        DatapathKind::Fake => Arc::new(FakeDatapath::new()),
        DatapathKind::Tap => Arc::new(TapDatapath::new(&args.tap_prefix, args.tap_owner)),
        DatapathKind::Fabric => {
            let Some(endpoint) = &args.fabric else {
                return Err(
                    "--datapath fabric needs --fabric <url>: without it there is nothing \
                            to program the tenant's rules into, and a port carrying security \
                            groups would be refused on every pass"
                        .into(),
                );
            };
            let (Some(vtep), Some(iface)) = (&args.fabric_vtep, &args.fabric_underlay) else {
                return Err(
                    "--datapath fabric needs --fabric-vtep and --fabric-underlay: the \
                            fabric has to be told where this host's encapsulated frames arrive, \
                            and nothing on the machine can work out which of its addresses its \
                            peers route to"
                        .into(),
                );
            };
            let underlay =
                Underlay::read(vtep, iface)?.with_srv6_locator(args.fabric_srv6_locator.clone());
            tracing::info!(
                vtep = %underlay.vtep, iface = %underlay.iface, mac = %underlay.mac,
                mtu = underlay.mtu,
                srv6_locator = underlay.srv6_locator.as_deref().unwrap_or("-"),
                "declaring this host to the fabric"
            );
            Arc::new(FabricDatapath::new(
                TapDatapath::new(&args.tap_prefix, args.tap_owner),
                endpoint,
                &args.node,
                underlay,
            ))
        }
    };
    let vmm: Arc<dyn Vmm> = match args.vmm {
        VmmKind::Fake => Arc::new(FakeVmm::new()),
        VmmKind::CloudHypervisor => Arc::new(CloudHypervisorVmm::new(Layout {
            binary: args.vmm_binary.clone().unwrap_or(layout.binary),
            ..layout
        })),
        VmmKind::Qemu => Arc::new(QemuVmm::new(Layout {
            binary: args
                .vmm_binary
                .clone()
                .unwrap_or_else(|| "qemu-system-x86_64".to_string()),
            ..layout
        })),
    };

    // The cell's store. There is no client to a remote cell yet — that surface
    // belongs to the API crate — so this process currently reconciles against
    // an empty in-process store and is useful for exercising the loop, the
    // metadata service and the VMM backends, not for running a cell.
    let store: Arc<dyn Store> = open_store(&args.store).await?;

    let mut config = AgentConfig::new(&args.node, &args.region, &args.cell);
    config.resync = Duration::from_secs(args.resync_secs);
    config.shared_state = args.shared_state;
    config.image_signing_keys = args
        .image_signing_key
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .map(|k| {
            velstra_cloud_model::images::SigningKey::parse(k)
                .map_err(|e| format!("--image-signing-key {k:?}: {e}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    config.require_signed_images = args.require_signed_images;
    if args.require_signed_images && config.image_signing_keys.is_empty() {
        return Err("--require-signed-images without an --image-signing-key would refuse every \
                    image; name the key the signatures are made under"
            .into());
    }
    let agent = match &args.api {
        Some(url) => {
            let token = match &args.api_token_file {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|e| format!("reading {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
                None => {
                    return Err("--api needs --api-token-file: the API will refuse an \
                                unauthenticated reader, and finding that out as an empty cell is \
                                the worst way to learn it"
                        .into());
                }
            };
            // One client, used for both halves: reads are handed this node's
            // share, and writes are reported back through it as this node's own
            // token — which is what makes `--api` a trust boundary rather than a
            // reader in front of a writer that still holds the operator's store.
            let client = Arc::new(ApiCell::for_node(url, &token, &args.node)?);
            Agent::reading(store, config, vmm, datapath, client.clone()).with_status_sink(client)
        }
        None => Agent::new(store, config, vmm, datapath),
    };
    // Every agent gets the speaker; the pass is a no-op until an operator
    // writes a `bgp-peers` object naming this machine, so a host without FRR
    // installed is never asked to reload it.
    let agent = agent.with_bgp(Arc::new(velstra_cloud_nodeagent::bgp::FrrSpeaker::new()));
    // Both would own the far end of the same taps, and the fabric's answer is
    // the one with the tenant separation in it. Said here rather than letting
    // the last writer win.
    if args.local_network && matches!(args.datapath, Some(DatapathKind::Fabric)) {
        return Err("--local-network and --datapath fabric both claim the far end of every tap.                     The fabric carries the segment on a node that has one; --local-network is for                     a cell that does not."
            .into());
    }
    let agent = if args.local_network {
        tracing::info!(prefix = %args.bridge_prefix, "this node is the first hop for its guests");
        agent.as_first_hop(&args.bridge_prefix)
    } else {
        agent
    };

    // The console service, before the agent starts reporting: what it binds is
    // what the node advertises, and a node that advertised an address before
    // knowing it had it would be publishing a hope.
    let mut agent = agent;
    match agent.status_sink() {
        // A console has one thing to write — that a ticket has been spent — and
        // without somewhere to write it a ticket could be replayed. So a node
        // with no status sink serves no console and says which it is: this is
        // the developer cell, where the agent writes straight to a store.
        None => tracing::info!(
            "no console service: this agent writes to a store directly and has no status sink"
        ),
        Some(sink) => {
            let consoles = velstra_cloud_nodeagent::console::Consoles {
                node: args.node.clone(),
                cell: agent.cell(),
                layout: console_layout.clone(),
                sink: Arc::new(velstra_cloud_nodeagent::console::SessionStatus {
                    sink,
                    node: args.node.clone(),
                }),
            };
            match velstra_cloud_nodeagent::console::serve(args.console_listen, consoles).await {
                Ok((bound, task)) => {
                    let advertised = if args.console_advertise.is_empty() {
                        advertise_from(bound, args.api.as_deref())
                    } else {
                        args.console_advertise.clone()
                    };
                    tracing::info!(%bound, %advertised, "serving guest consoles");
                    agent.set_console_endpoint(&advertised);
                    // Detached on purpose: it outlives this scope and stops when
                    // the process does.
                    std::mem::forget(task);
                }
                Err(e) => {
                    // Not fatal. A node that cannot bind its console port still
                    // runs guests, and it says it has no console rather than
                    // advertising one that answers nothing.
                    tracing::error!(
                        listen = %args.console_listen,
                        error = %e,
                        "no console service on this node; guests here cannot be attached to"
                    );
                }
            }
        }
    }
    let agent = agent;

    // Named, because the bare OS error is not enough to act on. A node whose
    // link-local address is not up yet fails here with "Cannot assign requested
    // address" and nothing to say which address that was — and the agent then
    // exits, so the node never becomes ready and the only symptom anybody sees
    // is an instance stuck on `NoValidHost`.
    let (bound, _server) = metadata::serve(args.metadata_listen, agent.guests())
        .await
        .map_err(|e| {
            std::io::Error::other(format!(
                "the metadata service could not listen on {}: {e}. Guests expect \
                 169.254.169.254:80, so a node serves it there once that address \
                 is up; pass --metadata-listen to bind somewhere else.",
                args.metadata_listen
            ))
        })?;

    // DHCP is not fatal to start, unlike the metadata service: it binds one
    // socket per tap as guests arrive, so there is nothing to fail here and now.
    // A tap it cannot bind is reported on that tap and costs that one guest its
    // address, rather than taking down a node that is running others.
    if args.dhcp {
        tokio::spawn(dhcp::serve(
            agent.guests(),
            dhcp::Server {
                lease: Duration::from_secs(args.dhcp_lease_secs),
                ..Default::default()
            },
            Duration::from_secs(args.dhcp_scan_secs),
        ));
    }

    tracing::info!(
        node = %args.node, cell = %args.cell, region = %args.region,
        metadata = %bound, dhcp = args.dhcp, "agent up"
    );

    agent
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("stopping; the guests keep running");
        })
        .await;
    Ok(())
}

/// Open whichever store the operator asked for.
///
/// One function in each binary rather than a shared helper, because the two
/// have different error types and a shared one would exist only to be
/// converted twice. If a third appears, that is the moment to extract it.
async fn open_store(spec: &str) -> Result<Arc<dyn Store>, velstra_cloud_store::StoreError> {
    if spec == "memory" {
        // Said out loud: a process whose state dies with it should not be a
        // surprise to whoever started it.
        tracing::warn!(
            "using the in-memory store: this process shares state with nobody \
             and forgets everything when it stops"
        );
        return Ok(Arc::new(MemoryStore::new()));
    }
    let endpoints: Vec<&str> = spec.split(',').map(str::trim).collect();
    let store = EtcdStore::connect(&endpoints).await?;
    tracing::info!(endpoints = %spec, "state store connected");
    Ok(Arc::new(store))
}

/// A yes or a no, spelled the way a shell-sourced seed spells them.
///
/// A bare `#[arg(long, env)]` on a `bool` takes only `true` and `false` from the
/// environment, and `VELSTRA_LOCAL_NETWORK=1` — which is what the wizard writes,
/// and what anybody would write by hand — makes the agent exit at startup with
/// `invalid value '1'`. It restarts, exits again, and the node never becomes
/// ready; the only symptom anybody sees is instances stuck on `NoValidHost`.
///
/// Found by installing the package on a machine, which is the only place it
/// could be found: every test in this repository passes the flag on a command
/// line, where clap never consults this parser at all.
fn truthy(raw: &str) -> std::result::Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" | "" => Ok(false),
        other => Err(format!(
            "expected a yes or a no (1, true, yes, on — or 0, false, no, off), not {other:?}"
        )),
    }
}

/// The address to tell the cell this node's console is at.
///
/// A bound address is often `0.0.0.0:8447`, which is a promise nobody can keep:
/// it is where this node listens, not where anybody reaches it. What the API can
/// reach is, by definition, the local address this node uses to reach the API —
/// so that is what gets asked for, with a connected UDP socket, which picks a
/// route and a source address without sending a packet.
///
/// A node whose bind was already specific keeps it. A node with no API to
/// measure against reports nothing rather than guessing: an endpoint that is
/// wrong is worse than one that is absent, because the absent one makes the
/// console button say why it is not there.
fn advertise_from(bound: SocketAddr, api: Option<&str>) -> String {
    if !bound.ip().is_unspecified() {
        return bound.to_string();
    }
    let Some(api) = api else {
        return String::new();
    };
    let Some(host) = api
        .rsplit("://")
        .next()
        .and_then(|rest| rest.split('/').next())
    else {
        return String::new();
    };
    // The port is irrelevant to the answer — only the route matters — but a
    // socket has to be connected to something, so the API's own is used.
    let target = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:443")
    };
    let Ok(probe) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return String::new();
    };
    match probe.connect(&target).and_then(|()| probe.local_addr()) {
        Ok(local) => format!("{}:{}", local.ip(), bound.port()),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod seed_spellings {
    use super::truthy;

    #[test]
    fn the_seed_writes_one_and_the_agent_has_to_read_it() {
        for yes in ["1", "true", "yes", "on", "Y", " 1 "] {
            assert_eq!(truthy(yes), Ok(true), "{yes:?}");
        }
        for no in ["0", "false", "no", "off", ""] {
            assert_eq!(truthy(no), Ok(false), "{no:?}");
        }
        // And a typo is an answer nobody gave, said out loud rather than read
        // as a no — a node silently not being a first hop is the failure this
        // whole module exists to stop.
        assert!(truthy("maybe").is_err());
    }
}
