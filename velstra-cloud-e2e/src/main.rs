//! A whole cell in one process: API, controllers, and a node — sharing one
//! store, which is the only way they can share one without etcd.
//!
//! Two reasons this exists rather than being a test fixture.
//!
//! The first is honesty about the other binaries. `velstra-cloud-api`,
//! `-controller` and `-nodeagent` each build their own in-memory store, so
//! running all three on a laptop gives three empty universes that cannot see
//! each other. They are meant to share an etcd, and until one is configured they
//! cannot be run together at all. Rather than let somebody discover that by
//! watching an instance sit unscheduled forever, this says it up front and
//! offers the thing they actually wanted.
//!
//! The second is the operational shape the platform is aiming at. A single
//! process holding one state, that you start and look at, is the "Proxmox feel"
//! the design is chasing — and the fact that it *is* the same code as a real
//! cell, wired differently, is the claim being made. If a development cell had
//! to be a special build, the claim would be false.
//!
//! It is not for production: one process, an in-memory store, and a fake
//! hypervisor. Nothing here survives a restart, and no guest is real.

use std::{sync::Arc, time::Duration};

use velstra_cloud_api::{Api, StaticTokenVerifier};
use velstra_cloud_controller::{
    LoopConfig, Metrics, address::AddressController, attachment::AttachmentController,
    quota::QuotaController, scheduler::Scheduler, snapshot::SnapshotController,
    status::StatusWriter, volume::VolumeController,
};
use velstra_cloud_nodeagent::{
    Agent, AgentConfig, FakeDatapath, FakePool, FakeVmm, PoolAgent, PoolConfig,
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const REGION: &str = "eu-central";
const CELL: &str = "cell-1";

#[tokio::main]
async fn main() {
    let listen = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let token = std::env::var("VELSTRA_TOKEN").unwrap_or_else(|_| "devtoken".to_string());

    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    // The dev cell's one token is its operator. It registers nodes and pools,
    // which are the cell's and not any tenant's — and a demo where half the
    // requests are refused teaches nothing about the platform.
    let api = Api::new(
        store.clone(),
        REGION,
        CELL,
        Arc::new(StaticTokenVerifier::single(&token)),
    )
    .with_cell_admins(vec!["dev".into()]);

    let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let metrics = Metrics::default();
    let config = LoopConfig::default();

    // The scheduler, the attachment controller and the quota counter, each the
    // same loop over a different pure function.
    let instances: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "instances");
    let nodes: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "nodes");
    let scheduler = Arc::new(Scheduler::new(
        instances.clone(),
        nodes.clone(),
        StatusWriter::new(store.clone(), CELL, "instances", "scheduler"),
        CELL,
    ));
    tokio::spawn(velstra_cloud_controller::run(
        scheduler,
        instances.clone(),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    // Addresses, so a port created here comes up on the network without an
    // operator picking one — which is also what makes the node's DHCP responder
    // have something to publish.
    let ports: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "ports");
    let address = Arc::new(AddressController::new(
        ports.clone(),
        TypedStore::new(store.clone(), CELL, "subnets"),
        TypedStore::new(store.clone(), CELL, "floatingips"),
        TypedStore::new(store.clone(), CELL, "load-balancers"),
        StatusWriter::new(store.clone(), CELL, "ports", "address"),
        CELL,
    ));
    tokio::spawn(velstra_cloud_controller::run(
        address,
        ports,
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    let port_controller = Arc::new(velstra_cloud_controller::port::PortController::new(
        TypedStore::new(store.clone(), CELL, "ports"),
        velstra_cloud_store::Cached::start(
            TypedStore::new(store.clone(), CELL, "instances"),
            store.clone(),
            velstra_cloud_store::prefix_for(CELL, "instances"),
        ),
        CELL,
    ));
    tokio::spawn(velstra_cloud_controller::run(
        port_controller,
        TypedStore::new(store.clone(), CELL, "ports"),
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    let attachments: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "attachments");
    let attachment = Arc::new(AttachmentController::new(attachments.clone()));
    tokio::spawn(velstra_cloud_controller::run(
        attachment,
        attachments,
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    let projects: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "projects");
    let quota = Arc::new(QuotaController::new(
        velstra_cloud_store::Cached::start(
            instances,
            store.clone(),
            velstra_cloud_store::prefix_for(CELL, "instances"),
        ),
        velstra_cloud_store::Cached::start(
            TypedStore::new(store.clone(), CELL, "volumes"),
            store.clone(),
            velstra_cloud_store::prefix_for(CELL, "volumes"),
        ),
        velstra_cloud_store::Cached::start(
            TypedStore::<
                velstra_cloud_model::resources::FloatingIpSpec,
                velstra_cloud_model::resources::FloatingIpStatus,
            >::new(store.clone(), CELL, "floatingips"),
            store.clone(),
            velstra_cloud_store::prefix_for(CELL, "floatingips"),
        ),
        velstra_cloud_store::Cached::start(
            TypedStore::<
                velstra_cloud_model::loadbalancer::LoadBalancerSpec,
                velstra_cloud_model::loadbalancer::LoadBalancerStatus,
            >::new(store.clone(), CELL, "load-balancers"),
            store.clone(),
            velstra_cloud_store::prefix_for(CELL, "load-balancers"),
        ),
        StatusWriter::new(store.clone(), CELL, "projects", "quota"),
        CELL,
    ));
    tokio::spawn(velstra_cloud_controller::run(
        quota,
        projects,
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    // Storage, on the same principle as the node below: a development cell that
    // cannot make a volume is a development cell you have to set up before it is
    // useful. The volume controller guards the deletion; the pool agent does the
    // work.
    let volumes_for_controller: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "volumes");
    let snapshots: TypedStore<_, _> = TypedStore::new(store.clone(), CELL, "snapshots");
    let volume = Arc::new(VolumeController::new(
        volumes_for_controller.clone(),
        snapshots.clone(),
        TypedStore::new(store.clone(), CELL, "pools"),
        CELL,
    ));
    tokio::spawn(velstra_cloud_controller::run(
        volume,
        volumes_for_controller,
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    // The copies' own guard. Separate from the volume controller because it
    // answers a different question — has the pool let go of *this* copy — and
    // because a controller that writes two collections is one that has to be
    // read twice to see what it writes.
    let snapshot = Arc::new(SnapshotController::new(snapshots.clone()));
    tokio::spawn(velstra_cloud_controller::run(
        snapshot,
        snapshots,
        store.clone(),
        config,
        metrics.clone(),
        shutdown.clone(),
    ));

    // Named for what the seed asks for: a development cell whose one volume
    // names a pool that does not exist would demonstrate the bug rather than
    // the feature.
    let pool_id = "rbd-standard";
    register_pool(store.clone(), pool_id).await;
    let mut pool_config = PoolConfig::new(pool_id, REGION, CELL);
    // Faster than the 30s default because the pool agent has no watch: claiming
    // and then provisioning is two passes, and a development cell where a new
    // volume takes a minute to appear teaches the wrong thing about how long
    // storage takes.
    pool_config.resync = Duration::from_secs(2);
    let pool_agent = PoolAgent::new(store.clone(), pool_config, Arc::new(FakePool::new(20_000)));
    let mut pool_shutdown = shutdown.clone();
    tokio::spawn(async move {
        pool_agent
            .run(async move {
                let _ = pool_shutdown.changed().await;
            })
            .await;
    });

    // One node, with a hypervisor that does not exist. Registering it here
    // rather than making the operator do it is the difference between a
    // development cell you can use and one you have to set up first.
    let node_id = "node-a";
    register_node(store.clone(), node_id).await;
    let agent = Agent::new(
        store.clone(),
        AgentConfig::new(node_id, REGION, CELL),
        Arc::new(FakeVmm::with_capacity(
            velstra_cloud_model::resources::Capacity {
                vcpus: 32,
                memory_mib: 131_072,
                disk_gib: 4096,
                numa_free_mib: vec![65_536, 65_536],
                hugepages_1gi: 0,
            },
        )),
        Arc::new(FakeDatapath::new()),
    );
    let mut agent_shutdown = shutdown.clone();
    tokio::spawn(async move {
        agent
            .run(async move {
                let _ = agent_shutdown.changed().await;
            })
            .await;
    });

    seed(store.clone()).await;
    attach_once_placed(store.clone()).await;

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("binding {listen}: {e}"));
    let address = listener.local_addr().unwrap();
    println!("velstra cloud — a development cell, all of it in this process");
    println!("  console  http://{address}/");
    println!("  api      http://{address}/api/v1");
    println!("  token    {token}");
    println!("  node     {node_id} (fake hypervisor, 32 vCPU / 128 GiB)");
    println!();
    println!("nothing here is persistent and no guest is real.");

    axum::serve(listener, velstra_cloud_api::server(api))
        .await
        .unwrap();
}

/// Create the node object an operator would create, so the cell has somewhere
/// to put work. The agent fills in its own status.
async fn register_pool(store: Arc<dyn Store>, id: &str) {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{PoolSpec, PoolStatus, Resource},
    };

    let pools: TypedStore<PoolSpec, PoolStatus> = TypedStore::new(store, CELL, "pools");
    let pool = Resource::new(
        Meta::new(
            ResourceName::parse(&format!("pools/{id}")).expect("a valid pool name"),
            Placement::new(REGION, CELL),
        ),
        PoolSpec {
            accepting: true,
            labels: vec!["dev".to_string()],
        },
        PoolStatus::default(),
    );
    if let Err(e) = pools
        .create(
            &pool,
            &velstra_cloud_model::access::Writer::controller("dev"),
        )
        .await
    {
        eprintln!("warning: could not register {id}: {e}");
    }
}

async fn register_node(store: Arc<dyn Store>, id: &str) {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{NodeSpec, NodeStatus, Resource},
    };

    let nodes: TypedStore<NodeSpec, NodeStatus> = TypedStore::new(store, CELL, "nodes");
    let node = Resource::new(
        Meta::new(
            ResourceName::parse(&format!("nodes/{id}")).expect("a valid node name"),
            Placement::new(REGION, CELL),
        ),
        NodeSpec {
            evacuate: false,
            vcpu_overcommit: 0,
            fence_after_s: 0,
            schedulable: true,
            labels: vec!["dev".to_string()],
            cpu_baseline: None,
            gateway: false,
        },
        NodeStatus::default(),
    );
    if let Err(e) = nodes
        .create(
            &node,
            &velstra_cloud_model::access::Writer::controller("dev"),
        )
        .await
    {
        eprintln!("warning: could not register {id}: {e}");
    }
    // Give the agent a moment to claim it before the first request arrives, so
    // an instance created immediately has somewhere to go.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Furnish the cell with one of everything, so that opening the console shows a
/// working system rather than eight empty boards.
///
/// Two rules held here. Every object is one an operator could have created —
/// nothing is written into a state the platform would not produce on its own.
/// And the one object that fails, fails *truthfully*: `too-big` asks for more
/// vCPUs than the only node has, so it converges to `Ready=False` and stays
/// there because that is the honest answer, not because anything was faked. A
/// seed that lies to make a screen look interesting is worse than an empty
/// screen, because the screen then lies too.
///
/// There is deliberately no permanently *drifting* object. A live cell
/// reconciles everything, so drift here would have to be manufactured; the
/// path is covered against the in-memory contract server instead, where
/// standing still is the truth.
async fn seed(store: Arc<dyn Store>) {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{
            ImageFormat, ImageSpec, ImageStatus, InstanceSpec, InstanceStatus, NetworkSpec,
            NetworkStatus, PortSpec, PortStatus, ProjectSpec, ProjectStatus, Quota, Resource,
            SnapshotSpec, SnapshotStatus, SubnetSpec, SubnetStatus, VolumeSpec, VolumeStatus,
        },
    };

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).expect("a valid seed name"),
            Placement::new(REGION, CELL),
        )
    }

    async fn put<S, T>(store: &Arc<dyn Store>, kind: &'static str, object: Resource<S, T>)
    where
        S: serde::Serialize + serde::de::DeserializeOwned + PartialEq + Send + Sync,
        T: serde::Serialize
            + serde::de::DeserializeOwned
            + PartialEq
            + velstra_cloud_model::resources::Observed
            + Send
            + Sync,
    {
        let collection: TypedStore<S, T> = TypedStore::new(store.clone(), CELL, kind);
        if let Err(e) = collection
            .create(
                &object,
                &velstra_cloud_model::access::Writer::controller("dev"),
            )
            .await
        {
            eprintln!("warning: seeding {}: {e}", object.meta.name);
        }
    }

    put(
        &store,
        "projects",
        Resource::new(
            meta("projects/p1"),
            ProjectSpec {
                policy: Default::default(),
                display_name: "Demo".into(),
                parent: "organizations/o1".into(),
                quota: Quota {
                    devices: 0,
                    instances: 50,
                    vcpus: 200,
                    memory_mib: 409_600,
                    volume_gib: 10_000,
                    ..Quota::default()
                },
                // Left empty on purpose: a dev cell is one cell, and an empty
                // home resolves to whichever cell is reading. Naming it here
                // would make the one-cell case look like it needs configuring.
                cell: String::new(),
                // The dev cell's one token is an admin of its one project, so
                // the console and the CLI can do everything against it. A cell
                // that handed out a viewer here would be a demo where half the
                // buttons refuse.
                bindings: vec![velstra_cloud_model::authz::Binding {
                    role: velstra_cloud_model::authz::Role::Admin,
                    members: vec!["dev".into()],
                }],
            },
            ProjectStatus::default(),
        ),
    )
    .await;

    let image = "projects/p1/images/sha256-3f9a2b";
    put(
        &store,
        "images",
        Resource::new(
            meta(image),
            ImageSpec {
                family: "debian-13".into(),
                version: "seed".into(),
                source_instance: None,
                digest: "sha256:bed9c5091e4cb31402af634b1c7a4494cb07c2119bb1a470cd12f9c3323a3b6f".into(),
                format: ImageFormat::Raw,
                size_bytes: 1_073_741_824,
                source_url: "https://example.invalid/debian-13-amd64.raw".into(),
                signature: None,
            },
            ImageStatus::default(),
        ),
    )
    .await;

    // A network, a subnet on it, and a port — three boards that are otherwise
    // empty, and the only way to see that a subnet picker offers the subnets of
    // the network you chose rather than all of them.
    put(
        &store,
        "networks",
        Resource::new(
            meta("projects/p1/networks/net-a"),
            NetworkSpec {
                host_bridge: String::new(),
                vni: 4711,
                mtu: 1450,
                external: false,
                announce: Default::default(),
            },
            NetworkStatus::default(),
        ),
    )
    .await;
    put(
        &store,
        "subnets",
        Resource::new(
            meta("projects/p1/subnets/sub-a"),
            SubnetSpec {
                network: "projects/p1/networks/net-a".into(),
                cidr: "10.20.0.0/24".into(),
                gateway: "10.20.0.1".into(),
                dns: vec!["10.20.0.1".into()],
                reserved: vec!["10.20.0.1".into()],
            },
            SubnetStatus::default(),
        ),
    )
    .await;
    put(
        &store,
        "ports",
        Resource::new(
            meta("projects/p1/ports/port-a"),
            PortSpec {
                network: "projects/p1/networks/net-a".into(),
                subnet: "projects/p1/subnets/sub-a".into(),
                // Left out on purpose: the address controller fills both in,
                // which is what a port an operator creates looks like.
                address: None,
                mac: None,
                security_groups: vec![],
                // Likewise: the port controller assigns it to whichever node
                // ends up holding the guest.
                node: None,
                rate_limit_mbit: Some(1000),
            },
            PortStatus::default(),
        ),
    )
    .await;

    put(
        &store,
        "volumes",
        Resource::new(
            meta("projects/p1/volumes/data-1"),
            VolumeSpec {
                source_backup: None,
                size_gib: 100,
                pool: "rbd-standard".into(),
                encryption_key: None,
                source_image: None,
                source_snapshot: None,
            },
            VolumeStatus::default(),
        ),
    )
    .await;

    // A copy of it, under it — a snapshot's source is its parent name rather
    // than a field. Seeded rather than left to the operator because the thing
    // worth seeing here is what it does to the *volume*: try to delete
    // `data-1` and it stays, saying which copies are in the way.
    //
    // It is created before the pool has made the volume, which is ordinary in
    // a level-triggered system: the pool copies nothing until there is
    // something to copy, and the pass that finds it is the one after.
    put(
        &store,
        "snapshots",
        Resource::new(
            meta("projects/p1/volumes/data-1/snapshots/nightly"),
            SnapshotSpec {
                pool: "rbd-standard".into(),
            },
            SnapshotStatus::default(),
        ),
    )
    .await;

    put(
        &store,
        "instances",
        Resource::new(
            meta("projects/p1/instances/web-1"),
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: Default::default(),
                console: false,
                devices: Vec::new(),
                vcpus: 2,
                memory_mib: 4096,
                image: image.into(),
                root_disk_gib: 20,
                ports: vec!["projects/p1/ports/port-a".into()],
                ..Default::default()
            },
            InstanceStatus::default(),
        ),
    )
    .await;

    // Asks for twice what the only node has. It will be refused by placement
    // for as long as the cell has one node, which makes it the one object here
    // whose failure is stable enough to look at — and the reason is written on
    // it, by the scheduler, in a sentence.
    put(
        &store,
        "instances",
        Resource::new(
            meta("projects/p1/instances/too-big"),
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: Default::default(),
                console: false,
                devices: Vec::new(),
                vcpus: 64,
                memory_mib: 262_144,
                image: image.into(),
                root_disk_gib: 20,
                ..Default::default()
            },
            InstanceStatus::default(),
        ),
    )
    .await;
}

/// Attach the volume to the instance, once the instance actually has a node.
///
/// Seeded after the fact rather than with the rest, because an attachment names
/// the node the volume will be opened on, and at seed time nothing has been
/// placed yet. Waiting for the placement is what an operator does too: you
/// attach a disk to a machine that exists. Writing `node-a` at seed time would
/// be guessing the scheduler's answer, and the API refuses callers that guess
/// for exactly this reason.
async fn attach_once_placed(store: Arc<dyn Store>) {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{AttachmentSpec, AttachmentStatus, InstanceSpec, InstanceStatus, Resource},
    };

    let instances: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), CELL, "instances");
    let mut node = None;
    // A second is many turns of a cell whose hypervisor is a fake. If placement
    // has not happened by then, something is wrong and a missing attachment is
    // the least of it — so this gives up quietly rather than blocking start-up.
    for _ in 0..20 {
        if let Ok(Some(instance)) = instances.get("projects/p1/instances/web-1").await {
            if let Some(placed) = instance.spec.node.clone() {
                node = Some(placed);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let Some(node) = node else {
        eprintln!("warning: web-1 was not placed, so nothing was attached to it");
        return;
    };

    let attachments: TypedStore<AttachmentSpec, AttachmentStatus> =
        TypedStore::new(store, CELL, "attachments");
    let attachment = Resource::new(
        Meta::new(
            ResourceName::parse("projects/p1/attachments/data-1-web-1").expect("a valid name"),
            Placement::new(REGION, CELL),
        ),
        AttachmentSpec {
            volume: "projects/p1/volumes/data-1".into(),
            instance: "projects/p1/instances/web-1".into(),
            node,
            read_only: false,
        },
        AttachmentStatus::default(),
    );
    if let Err(e) = attachments
        .create(
            &attachment,
            &velstra_cloud_model::access::Writer::controller("dev"),
        )
        .await
    {
        eprintln!("warning: seeding the attachment: {e}");
    }
}
