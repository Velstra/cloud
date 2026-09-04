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
use velstra_cloud_controller::{LoopConfig, Metrics};
use velstra_cloud_nodeagent::{
    Agent, AgentConfig, FakeDatapath, FakeNetwork, FakePool, FakeVmm, PoolAgent, PoolConfig,
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const REGION: &str = "eu-central";
/// The one image the seed publishes, and that every fake node holds.
const SEED_IMAGE: &str = "projects/p1/images/sha256-3f9a2b";
const SEED_IMAGE_DIGEST: &str =
    "sha256:bed9c5091e4cb31402af634b1c7a4494cb07c2119bb1a470cd12f9c3323a3b6f";
const SEED_IMAGE_URL: &str = "https://example.invalid/debian-13-amd64.raw";
const CELL: &str = "cell-1";

#[tokio::main]
async fn main() {
    // What the controllers and agents say, at `warn` unless `RUST_LOG` asks
    // for more: a dev cell whose loops could not say why a drain moved
    // nothing was one whose operator read the code instead.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let listen = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let token = std::env::var("VELSTRA_TOKEN").unwrap_or_else(|_| "devtoken".to_string());

    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    // The dev cell's one token is its operator. It registers nodes and pools,
    // which are the cell's and not any tenant's — and a demo where half the
    // requests are refused teaches nothing about the platform.
    //
    // Sessions first, the static token second — the same chain the real
    // binary builds. With the static verifier alone, a user created here could
    // sign in (that route needs no verifier) and then be refused by every
    // other route with the very token the sign-in had just minted; the
    // console's password form was a door painted on a wall.
    let identity = velstra_cloud_api::sessions::IdentityStore::new(store.clone(), REGION, CELL);
    let verifier = velstra_cloud_api::sessions::StoreTokenVerifier::new(identity)
        .with_fallback(Arc::new(StaticTokenVerifier::single(&token)));
    let api = Api::new(store.clone(), REGION, CELL, Arc::new(verifier))
        .with_cell_admins(vec!["dev".into()]);

    let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let metrics = Metrics::default();
    // A short resync: a maintenance window opening, a clock crossing a
    // schedule — the loops that notice by looking notice in seconds here,
    // not in the five minutes a production cell can afford.
    let config = LoopConfig {
        resync: Duration::from_secs(15),
        ..LoopConfig::default()
    };

    // Every controller the real cell runs, from the one list the controller
    // binary uses — not a hand-picked few. Seven of twenty used to run here,
    // and the demo showed the rest as features that never did anything: a
    // maintenance window that drained nothing, a network "unreported" for
    // ever, a migration with no controller to move it.
    let wiring = velstra_cloud_controller::wiring::Cell {
        store: store.clone(),
        region: REGION.into(),
        cell: CELL.into(),
        fabric: None,
    };
    let loops = velstra_cloud_controller::wiring::Loops::unelected(
        config,
        metrics.clone(),
        shutdown.clone(),
    );
    for (_, task) in velstra_cloud_controller::wiring::every_controller(&wiring, &loops) {
        tokio::spawn(task);
    }

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
    // Two machines by default, and `VELSTRA_DEV_NODES` for more or fewer. One
    // node made every second feature answer "nowhere to go": a maintenance
    // window drained nothing, a migration had no destination, anti-affinity
    // could not spread — and a demo of a scheduler that never schedules
    // anywhere teaches nothing about the platform. The second is smaller, so
    // "room" and "largest guest" have something to say.
    let node_count: usize = std::env::var("VELSTRA_DEV_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| (1..=26).contains(n))
        .unwrap_or(2);
    let mut node_lines = Vec::new();
    let mut vmms: Vec<Arc<FakeVmm>> = Vec::new();
    // One wire between the fake machines, so a live migration has a far end
    // to hand the guest to. Each on its own wire, the copy started and the
    // receiver was never found.
    let wire = FakeNetwork::new();
    for i in 0..node_count {
        let node_id = format!("node-{}", (b'a' + i as u8) as char);
        let (vcpus, memory_mib) = if i == 0 { (32, 131_072) } else { (16, 65_536) };
        register_node(store.clone(), &node_id).await;
        // One process holds every fake disk, so a guest's root disk is where
        // any node can reach it — which is what `--shared-state` declares on a
        // real machine, and without which a drain has nowhere to move a guest.
        let mut agent_config = AgentConfig::new(&node_id, REGION, CELL);
        agent_config.shared_state = true;
        let vmm = wire.host(&node_id);
        vmm.set_capacity(velstra_cloud_model::resources::Capacity {
            vcpus,
            memory_mib,
            disk_gib: 4096,
            numa_free_mib: vec![memory_mib / 2, memory_mib / 2],
            hugepages_1gi: 0,
        });
        // Every node holds the seed image, as a cell whose nodes have booted
        // the family once would: a guest can only move to a node that has its
        // image, and a second node that had never pulled it was a drain with
        // nowhere to go — "node-b does not have projects/p1/images/…".
        vmm.cache_image(SEED_IMAGE_DIGEST);
        let vmm = Arc::new(vmm);
        vmms.push(vmm.clone());
        let agent = Agent::new(
            store.clone(),
            agent_config,
            vmm,
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
        node_lines.push(format!(
            "  node     {node_id} (fake hypervisor, {vcpus} vCPU / {} GiB)",
            memory_mib / 1024
        ));
    }

    seed(store.clone()).await;
    // The wire itself. A fake copy finishes when something says it has —
    // in a test, the test; here, a thread that finishes every transfer a
    // machine has open a moment after it starts, which is what a live
    // migration of an idle fake guest looks like.
    for vmm in vmms.clone() {
        tokio::spawn(async move {
            use velstra_cloud_nodeagent::host::Vmm as _;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Ok(host) = vmm.observe().await else { continue };
                for instance in host.sending {
                    if let Err(e) = vmm.finish_transfer(&instance) {
                        tracing::debug!(instance, error = %e, "a fake transfer did not finish");
                    }
                }
            }
        });
    }
    attach_once_placed(store.clone()).await;

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("binding {listen}: {e}"));
    let address = listener.local_addr().unwrap();
    println!("velstra cloud — a development cell, all of it in this process");
    println!("  console  http://{address}/");
    println!("  api      http://{address}/api/v1");
    println!("  token    {token}");
    for line in &node_lines {
        println!("{line}");
    }
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

    let image = SEED_IMAGE;
    put(
        &store,
        "images",
        Resource::new(
            meta(image),
            ImageSpec {
                from: String::new(),
                family: "debian-13".into(),
                version: "seed".into(),
                source_instance: None,
                digest: SEED_IMAGE_DIGEST.into(),
                format: ImageFormat::Raw,
                size_bytes: 1_073_741_824,
                source_url: SEED_IMAGE_URL.into(),
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
            at: String::new(),
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
