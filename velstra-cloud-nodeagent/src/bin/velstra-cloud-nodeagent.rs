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

    /// How often the responder looks for taps that have appeared or gone.
    #[arg(long, default_value = "2")]
    dhcp_scan_secs: u64,

    /// Read the cell through the API instead of the store, and be handed only
    /// this node's share.
    ///
    /// Without it, this agent lists every instance, port, attachment and
    /// migration in the cell on every pass and watches them unfiltered — so its
    /// load grows with the cell, and a thousand nodes are a thousand watchers on
    /// one store with every write delivered a thousand times. With it, the API
    /// serves every node from one watch per collection and hands each one its
    /// own objects.
    ///
    /// Writes still go straight to the store either way: a node's writes are
    /// already proportional to its own work, and putting a second process in the
    /// path of a status report buys nothing.
    #[arg(long)]
    api: Option<String>,

    /// The bearer token for `--api`. A file rather than a flag, so it is not in
    /// anybody's process list.
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
            let underlay = Underlay::read(vtep, iface)?;
            tracing::info!(
                vtep = %underlay.vtep, iface = %underlay.iface, mac = %underlay.mac,
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
            let reader = Arc::new(ApiCell::for_node(url, &token, &args.node)?);
            Agent::reading(store, config, vmm, datapath, reader)
        }
        None => Agent::new(store, config, vmm, datapath),
    };

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
