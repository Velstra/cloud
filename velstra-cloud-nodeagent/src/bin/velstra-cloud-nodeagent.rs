//! The agent, as a process.
//!
//! One node, one cell, one stream. It serves the metadata service for the
//! guests on this machine and runs the reconcile loop until it is asked to
//! stop — and stopping it does not stop the guests, which is the whole point of
//! [`velstra_cloud_nodeagent::cloud_hypervisor`] putting them under systemd.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use clap::{Parser, ValueEnum};
use velstra_cloud_nodeagent::{
    Agent, AgentConfig, Datapath, FakeDatapath, FakeVmm, Layout, Vmm,
    cloud_hypervisor::CloudHypervisorVmm, metadata, qemu::QemuVmm,
};
use velstra_cloud_store::{MemoryStore, Store};

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let layout = Layout {
        run_dir: args.state_dir.join("instances"),
        image_dir: args.state_dir.join("images"),
        incoming_dir: args.state_dir.join("images/incoming"),
        disk_gib: args.disk_gib,
        migration_address: args.migration_address.clone(),
        migration_ports: args.migration_port_first..args.migration_port_last,
        migration_tls_dir: args.migration_tls_dir.clone(),
        ..Default::default()
    };
    let (vmm, datapath): (Arc<dyn Vmm>, Arc<dyn Datapath>) = match args.vmm {
        VmmKind::Fake => (Arc::new(FakeVmm::new()), Arc::new(FakeDatapath::new())),
        // The fabric-backed datapath lands with the fabric integration; until
        // then a node started with a real hypervisor programs nothing, and
        // every instance on it will say so on its own object rather than
        // silently coming up with no network.
        VmmKind::CloudHypervisor => (
            Arc::new(CloudHypervisorVmm::new(layout)),
            Arc::new(FakeDatapath::new()),
        ),
        VmmKind::Qemu => (
            Arc::new(QemuVmm::new(Layout {
                binary: "qemu-system-x86_64".to_string(),
                ..layout
            })),
            Arc::new(FakeDatapath::new()),
        ),
    };

    // The cell's store. There is no client to a remote cell yet — that surface
    // belongs to the API crate — so this process currently reconciles against
    // an empty in-process store and is useful for exercising the loop, the
    // metadata service and the VMM backends, not for running a cell.
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());

    let mut config = AgentConfig::new(&args.node, &args.region, &args.cell);
    config.resync = Duration::from_secs(args.resync_secs);
    let agent = Agent::new(store, config, vmm, datapath);

    let (bound, _server) = metadata::serve(args.metadata_listen, agent.metadata()).await?;
    tracing::info!(
        node = %args.node, cell = %args.cell, region = %args.region,
        metadata = %bound, "agent up"
    );

    agent
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("stopping; the guests keep running");
        })
        .await;
    Ok(())
}
