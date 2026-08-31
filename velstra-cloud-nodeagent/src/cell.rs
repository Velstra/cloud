//! Where a node learns what it is supposed to be doing.
//!
//! Everything an agent reads about the cell goes through here, and there are two
//! ways to answer it.
//!
//! [`StoreCell`] reads the store directly. It is the simplest thing that works
//! and it is what this agent did from the beginning — but the store cannot
//! filter a range read by anything except a key prefix, and which node holds an
//! object is a *field*, not part of its key, because it changes when a guest
//! moves. So every agent reads every instance, port, attachment and migration in
//! the cell on every pass, and watches those collections unfiltered. A thousand
//! nodes are then a thousand watchers on one store, and every write is delivered
//! a thousand times. That, and not the store's own capacity, is what bounds a
//! cell.
//!
//! [`ApiCell`] asks the API for this node's share. The API holds one watch per
//! collection and serves every agent from it, so the store sees one watcher
//! however many nodes there are, and a node is handed its own objects rather
//! than the cell's.
//!
//! ## Reads here, writes in [`crate::sink`]
//!
//! This module is the *read* seam. The matching *write* seam is
//! [`crate::sink::StatusSink`]: a direct-store agent writes to the store and the
//! store judges a self-declared identity, while a `--api` agent reports through
//! [`ApiCell`] as its own token and the API authenticates it. The two were split
//! for a reason beyond scaling — reading through the API bounds a cell's watch
//! load, and writing through it is what makes `--api` a trust boundary rather
//! than a reader in front of a writer that still holds the operator's own store.
//!
//! ## Security groups
//!
//! A group's rules can name another group, and expanding that needs the
//! addresses of every port in it — which used to mean reading every port in the
//! cell. The API now computes that membership and puts it on the group
//! (`status.members`), so a node reads the groups, which are few, and its own
//! ports, which are its own. It never needs anybody else's port object, which is
//! also one fewer thing a compromised node can see.

use std::collections::BTreeMap;

use async_trait::async_trait;
use velstra_cloud_model::{
    migration::{Migration, MigrationSpec, MigrationStatus},
    resources::{
        Attachment, AttachmentSpec, AttachmentStatus, Image, ImageSpec, ImageStatus, Instance,
        InstanceSpec, InstanceStatus, Network, NetworkSpec, NetworkStatus, Port, PortSpec,
        PortStatus, Subnet, SubnetSpec, SubnetStatus,
    },
    security::{SecurityGroup, SecurityGroupSpec, SecurityGroupStatus},
};
use velstra_cloud_store::{Store, TypedStore};

use crate::host::{HostError, Result};

/// One pass's worth of the cell, as a **pool** needs to see it.
///
/// The storage half of the same idea, and the same two implementations behind
/// it: read the store and get every volume and snapshot in the cell, or ask the
/// API and get the ones this pool holds or has been given.
#[async_trait]
pub trait PoolReader: Send + Sync + 'static {
    async fn volumes(&self) -> Result<Vec<velstra_cloud_model::resources::Volume>>;
    async fn snapshots(&self) -> Result<Vec<velstra_cloud_model::resources::Snapshot>>;
    /// The copies asked for, and the targets they go to.
    ///
    /// Two lists rather than one call per backup: a target is read once per
    /// pass however many copies point at it, and a pool with forty backups
    /// should not make forty reads to learn the same three paths.
    async fn backups(&self) -> Result<Vec<velstra_cloud_model::resources::Backup>>;
    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>>;

    /// This pool's own object, or `None` if nobody registered it.
    ///
    /// Read through here rather than off a store handle for one reason, and it
    /// was found on a real machine: a pool agent talking to the API has no
    /// store at all. Reading its own object out of a placeholder answered "no
    /// such pool", the pass returned early — a pool nobody registered is not an
    /// agent's to invent — and the pool sat `unreported` on the board while its
    /// agent logged that it was running perfectly.
    async fn pool(&self, id: &str) -> Result<Option<velstra_cloud_model::resources::Pool>>;

    /// What this reads, for a log line at startup.
    fn describe(&self) -> String;
}

/// The store, read directly and unfiltered.
pub struct StorePool {
    volumes: TypedStore<
        velstra_cloud_model::resources::VolumeSpec,
        velstra_cloud_model::resources::VolumeStatus,
    >,
    snapshots: TypedStore<
        velstra_cloud_model::resources::SnapshotSpec,
        velstra_cloud_model::resources::SnapshotStatus,
    >,
    backups: TypedStore<
        velstra_cloud_model::backup::BackupSpec,
        velstra_cloud_model::backup::BackupStatus,
    >,
    pools: TypedStore<
        velstra_cloud_model::resources::PoolSpec,
        velstra_cloud_model::resources::PoolStatus,
    >,
    targets: TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    >,
}

impl StorePool {
    pub fn new(store: std::sync::Arc<dyn Store>, cell: &str) -> Self {
        Self {
            volumes: TypedStore::new(store.clone(), cell, "volumes"),
            snapshots: TypedStore::new(store.clone(), cell, "snapshots"),
            backups: TypedStore::new(store.clone(), cell, "backups"),
            targets: TypedStore::new(store.clone(), cell, "backup-targets"),
            pools: TypedStore::new(store, cell, "pools"),
        }
    }
}

#[async_trait]
impl PoolReader for StorePool {
    async fn volumes(&self) -> Result<Vec<velstra_cloud_model::resources::Volume>> {
        self.volumes.list().await.map_err(|e| failed("volumes", e))
    }
    async fn snapshots(&self) -> Result<Vec<velstra_cloud_model::resources::Snapshot>> {
        self.snapshots
            .list()
            .await
            .map_err(|e| failed("snapshots", e))
    }
    async fn backups(&self) -> Result<Vec<velstra_cloud_model::resources::Backup>> {
        self.backups.list().await.map_err(|e| failed("backups", e))
    }
    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>> {
        self.targets
            .list()
            .await
            .map_err(|e| failed("backup targets", e))
    }
    async fn pool(&self, id: &str) -> Result<Option<velstra_cloud_model::resources::Pool>> {
        self.pools
            .get(&format!("pools/{id}"))
            .await
            .map_err(|e| failed("this pool's own object", e))
    }
    fn describe(&self) -> String {
        "the store, unfiltered: this pool reads every volume, snapshot and backup in the cell on \
         every pass"
            .to_string()
    }
}

/// One pass's worth of the cell, as this node needs to see it.
#[async_trait]
pub trait CellReader: Send + Sync + 'static {
    /// The instances this node holds or has been given.
    async fn instances(&self) -> Result<Vec<Instance>>;
    async fn attachments(&self) -> Result<Vec<Attachment>>;
    async fn ports(&self) -> Result<Vec<Port>>;
    /// Migrations with this node at either end.
    async fn migrations(&self) -> Result<Vec<Migration>>;

    /// Shared, and read whole: a group is a declaration every node reads and
    /// none of them owns.
    async fn security_groups(&self) -> Result<Vec<SecurityGroup>>;
    async fn subnets(&self) -> Result<Vec<Subnet>>;

    /// Consoles somebody has been granted into a guest on this node.
    ///
    /// A default rather than a required method: a cell whose reader is a test
    /// double, or an agent built before consoles existed, has none and needs
    /// none — an empty list means "nobody has asked for one", which refuses
    /// every attach, which is the safe answer.
    async fn console_sessions(
        &self,
    ) -> Result<Vec<velstra_cloud_model::resources::ConsoleSession>> {
        Ok(Vec::new())
    }
    async fn networks(&self) -> Result<Vec<Network>>;
    /// The cell's PCI device classes, by resource id.
    ///
    /// Cell-wide, like the images: the hardware belongs to the cell, and a
    /// class defined per project would be a different name for the same
    /// silicon in every tenancy.
    ///
    /// A default rather than a required method, so a cell that passes no
    /// hardware through — which is most of them — needs no extra collection
    /// and no reader has to grow one to compile.
    async fn device_classes(
        &self,
    ) -> Result<std::collections::BTreeMap<String, velstra_cloud_model::pci::DeviceClassSpec>> {
        Ok(Default::default())
    }

    /// The captures asked for on this node.
    ///
    /// A default method: a cell where nobody has ever made a template out of a
    /// guest needs no extra collection, and no reader has to grow one.
    async fn captures(&self) -> Result<Vec<velstra_cloud_model::resources::Capture>> {
        Ok(Vec::new())
    }

    /// The places copies are kept, so a capture knows where to write.
    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>> {
        Ok(Vec::new())
    }

    /// The public addresses in this cell.
    ///
    /// Read because a routed one is an address the **guest** holds: the
    /// metadata this node serves has to contain it, or the guest comes up
    /// without the address the world is being told to send to. A default
    /// method, like the device classes, so a cell that hands out no public
    /// addresses needs no extra collection and no reader has to grow one.
    async fn floating_ips(&self) -> Result<Vec<velstra_cloud_model::resources::FloatingIp>> {
        Ok(Vec::new())
    }
    /// The BGP sessions the operator has written, cell-wide.
    ///
    /// A default method like the floating IPs: a cell that announces nothing
    /// needs no extra collection, and an empty list makes the whole pass a
    /// no-op on every machine.
    async fn bgp_peers(&self) -> Result<Vec<velstra_cloud_model::resources::BgpPeer>> {
        Ok(Vec::new())
    }
    /// The registered images, which is where an image's *source* lives.
    ///
    /// A node verifies an image against the sha256 in its own name, but the
    /// bytes have to come from somewhere, and that somewhere is a field on the
    /// Image object. Without this the node could check an image and never
    /// obtain one: `spec.source_url` was carried through the wire, shown in
    /// the console, and read by nothing.
    async fn images(&self) -> Result<Vec<Image>>;

    /// Every node in the cell, and the Ceph cluster if the cell has one.
    ///
    /// Read by exactly one pass — the Ceph one — and read whole, because the
    /// decision a node makes about Ceph is a function of what *every* node
    /// reports. That is unusual here and it is the point: nothing hands a node
    /// its step, so a node works out whether the step is its own by computing
    /// the same answer everybody else computes, over the same facts.
    ///
    /// A cell with no Ceph cluster answers with an empty list on both, and the
    /// pass does nothing at all.
    async fn nodes(&self) -> Result<Vec<velstra_cloud_model::resources::Node>>;
    /// One node by id — this agent's own.
    ///
    /// Beside `nodes()` rather than derived from it, because they are different
    /// reads with different entitlements: the list is the Ceph pass's, whole and
    /// cell-wide; this is a machine asking for the object it reports on.
    async fn node(&self, id: &str) -> Result<Option<velstra_cloud_model::resources::Node>>;

    /// One instance by name, for the two questions a migration asks about a
    /// guest that is not in this node's own list: who owns it, and what is
    /// being brought here.
    ///
    /// Beside `instances()` for the same reason `node` is beside `nodes`, and
    /// with a sharper one behind it: those two reads were going to a store
    /// handle, which under `--api` is a placeholder that answers "no such
    /// object". On the source that reads as "this node has already let go" —
    /// the moment a migration exists, before the guest has moved anywhere.
    async fn instance(
        &self,
        name: &str,
    ) -> Result<Option<velstra_cloud_model::resources::Instance>>;

    async fn ceph_clusters(&self) -> Result<Vec<velstra_cloud_model::ceph::CephCluster>>;

    /// Woken whenever something this node has business with changes.
    ///
    /// A plain "look again", never the change itself. Every pass is
    /// level-triggered and re-reads what it owns, so a missed wake-up costs
    /// latency until the next resync and nothing else — which is what lets this
    /// be an unreliable channel instead of a protocol.
    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()>;

    /// What this reads, for a log line at startup. A node that is quietly
    /// reading the whole cell should say so.
    fn describe(&self) -> String;
}

/// How many wake-ups may queue before the loop is simply told "something
/// happened". Small on purpose: a pass answers all of them at once.
const WAKE_QUEUE: usize = 16;

/// The store, read directly and unfiltered.
pub struct StoreCell {
    node: String,
    instances: TypedStore<InstanceSpec, InstanceStatus>,
    attachments: TypedStore<AttachmentSpec, AttachmentStatus>,
    ports: TypedStore<PortSpec, PortStatus>,
    groups: TypedStore<SecurityGroupSpec, SecurityGroupStatus>,
    subnets: TypedStore<SubnetSpec, SubnetStatus>,
    networks: TypedStore<NetworkSpec, NetworkStatus>,
    console_sessions: TypedStore<
        velstra_cloud_model::console::ConsoleSessionSpec,
        velstra_cloud_model::console::ConsoleSessionStatus,
    >,
    images: TypedStore<ImageSpec, ImageStatus>,
    floating_ips: TypedStore<
        velstra_cloud_model::resources::FloatingIpSpec,
        velstra_cloud_model::resources::FloatingIpStatus,
    >,
    bgp_peers: TypedStore<
        velstra_cloud_model::resources::BgpPeerSpec,
        velstra_cloud_model::resources::BgpPeerStatus,
    >,
    captures: TypedStore<
        velstra_cloud_model::capture::CaptureSpec,
        velstra_cloud_model::capture::CaptureStatus,
    >,
    backup_targets: TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    >,
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
    all_nodes: TypedStore<
        velstra_cloud_model::resources::NodeSpec,
        velstra_cloud_model::resources::NodeStatus,
    >,
    ceph_clusters: TypedStore<
        velstra_cloud_model::ceph::CephClusterSpec,
        velstra_cloud_model::ceph::CephClusterStatus,
    >,
}

impl StoreCell {
    pub fn new(store: std::sync::Arc<dyn Store>, cell: &str, node: &str) -> Self {
        Self {
            node: node.to_string(),
            instances: TypedStore::new(store.clone(), cell, "instances"),
            attachments: TypedStore::new(store.clone(), cell, "attachments"),
            ports: TypedStore::new(store.clone(), cell, "ports"),
            groups: TypedStore::new(store.clone(), cell, "security-groups"),
            all_nodes: TypedStore::new(store.clone(), cell, "nodes"),
            floating_ips: TypedStore::new(store.clone(), cell, "floatingips"),
            bgp_peers: TypedStore::new(store.clone(), cell, "bgp-peers"),
            captures: TypedStore::new(store.clone(), cell, "captures"),
            backup_targets: TypedStore::new(store.clone(), cell, "backup-targets"),
            ceph_clusters: TypedStore::new(store.clone(), cell, "ceph-clusters"),
            subnets: TypedStore::new(store.clone(), cell, "subnets"),
            networks: TypedStore::new(store.clone(), cell, "networks"),
            console_sessions: TypedStore::new(store.clone(), cell, "console-sessions"),
            images: TypedStore::new(store.clone(), cell, "images"),
            migrations: TypedStore::new(store, cell, "migrations"),
        }
    }
}

fn failed(what: &str, e: impl std::fmt::Display) -> HostError {
    HostError::failed(format!("reading {what}: {e}"))
}

#[async_trait]
impl CellReader for StoreCell {
    async fn instances(&self) -> Result<Vec<Instance>> {
        self.instances
            .list()
            .await
            .map_err(|e| failed("instances", e))
    }
    async fn attachments(&self) -> Result<Vec<Attachment>> {
        self.attachments
            .list()
            .await
            .map_err(|e| failed("attachments", e))
    }
    async fn ports(&self) -> Result<Vec<Port>> {
        self.ports.list().await.map_err(|e| failed("ports", e))
    }
    async fn migrations(&self) -> Result<Vec<Migration>> {
        self.migrations
            .list()
            .await
            .map_err(|e| failed("migrations", e))
    }
    async fn security_groups(&self) -> Result<Vec<SecurityGroup>> {
        self.groups
            .list()
            .await
            .map_err(|e| failed("security groups", e))
    }
    async fn subnets(&self) -> Result<Vec<Subnet>> {
        self.subnets.list().await.map_err(|e| failed("subnets", e))
    }
    async fn networks(&self) -> Result<Vec<Network>> {
        self.networks
            .list()
            .await
            .map_err(|e| failed("networks", e))
    }

    async fn console_sessions(
        &self,
    ) -> Result<Vec<velstra_cloud_model::resources::ConsoleSession>> {
        self.console_sessions
            .list()
            .await
            .map_err(|e| failed("console-sessions", e))
    }
    async fn floating_ips(&self) -> Result<Vec<velstra_cloud_model::resources::FloatingIp>> {
        self.floating_ips
            .list()
            .await
            .map_err(|e| failed("floating ips", e))
    }

    async fn bgp_peers(&self) -> Result<Vec<velstra_cloud_model::resources::BgpPeer>> {
        self.bgp_peers
            .list()
            .await
            .map_err(|e| failed("bgp peers", e))
    }

    async fn captures(&self) -> Result<Vec<velstra_cloud_model::resources::Capture>> {
        self.captures
            .list()
            .await
            .map_err(|e| failed("captures", e))
    }

    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>> {
        self.backup_targets
            .list()
            .await
            .map_err(|e| failed("backup targets", e))
    }

    async fn images(&self) -> Result<Vec<Image>> {
        self.images.list().await.map_err(|e| failed("images", e))
    }
    async fn nodes(&self) -> Result<Vec<velstra_cloud_model::resources::Node>> {
        self.all_nodes.list().await.map_err(|e| failed("nodes", e))
    }
    async fn node(&self, id: &str) -> Result<Option<velstra_cloud_model::resources::Node>> {
        self.all_nodes
            .get(&format!("nodes/{id}"))
            .await
            .map_err(|e| failed("nodes", e))
    }
    async fn instance(
        &self,
        name: &str,
    ) -> Result<Option<velstra_cloud_model::resources::Instance>> {
        self.instances.get(name).await.map_err(|e| failed("instances", e))
    }
    async fn ceph_clusters(&self) -> Result<Vec<velstra_cloud_model::ceph::CephCluster>> {
        self.ceph_clusters
            .list()
            .await
            .map_err(|e| failed("ceph clusters", e))
    }

    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()> {
        // Watch first, then let the caller list. The other order has a gap in it
        // exactly one change wide, and that change is invisible until the next
        // resync.
        let from = self.instances.revision().await.ok();
        let mut streams = vec![
            self.instances.watch(from),
            self.attachments.watch(from),
            self.ports.watch(from),
            self.migrations.watch(from),
        ];
        let (tx, rx) = tokio::sync::mpsc::channel(WAKE_QUEUE);
        let node = self.node.clone();
        tokio::spawn(async move {
            loop {
                let mut any = false;
                for stream in &mut streams {
                    while let Ok(event) = stream.try_recv() {
                        any |= concerns(&event, &node);
                    }
                }
                if any && tx.try_send(()).is_err() && tx.is_closed() {
                    return;
                }
                // Nothing queued: wait for whichever speaks first. Rebuilt each
                // time rather than held, because a `select!` over a Vec has to
                // borrow all of them and the loop above already drained what was
                // there.
                let next = futures_lite_select(&mut streams).await;
                match next {
                    Some(event) => {
                        if concerns(&event, &node) && tx.try_send(()).is_err() && tx.is_closed() {
                            return;
                        }
                    }
                    // Every stream ended. The store dropped this agent, and the
                    // resync timer is what carries it until a restart.
                    None => return,
                }
            }
        });
        rx
    }

    fn describe(&self) -> String {
        "the store, unfiltered: this node reads every instance, port, attachment \
         and migration in the cell on every pass"
            .to_string()
    }
}

/// Whichever stream speaks first, or `None` when all of them have ended.
async fn futures_lite_select(
    streams: &mut [tokio::sync::mpsc::Receiver<velstra_cloud_store::Event>],
) -> Option<velstra_cloud_store::Event> {
    // A hand-rolled select over a slice, because `tokio::select!` needs the
    // branches spelled out and the number of collections is a list, not a
    // literal.
    std::future::poll_fn(|cx| {
        let mut all_done = true;
        for stream in streams.iter_mut() {
            match stream.poll_recv(cx) {
                std::task::Poll::Ready(Some(event)) => {
                    return std::task::Poll::Ready(Some(event));
                }
                std::task::Poll::Ready(None) => {}
                std::task::Poll::Pending => all_done = false,
            }
        }
        if all_done {
            std::task::Poll::Ready(None)
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

/// Whether a stored change is about an object this node has anything to do with.
///
/// The same rule the API applies server-side, from the same function, so the two
/// cannot come to disagree — which they would exactly once, the moment an object
/// moved, and the symptom would be a guest running on a node that has stopped
/// being told about it.
fn concerns(event: &velstra_cloud_store::Event, node: &str) -> bool {
    let velstra_cloud_store::Event::Put(entry) = event else {
        // A delete carries no object to judge. It is also rare, and a needless
        // pass is cheap; guessing wrong the other way would strand an object.
        return true;
    };
    match serde_json::from_slice::<serde_json::Value>(&entry.value) {
        Ok(value) => velstra_cloud_model::assignment::concerns(&value, node),
        // Unreadable is not "not mine".
        Err(_) => true,
    }
}

/// Turn a list into a map by resource name, which is how every caller wants it.
pub fn by_name<T, F>(items: Vec<T>, name: F) -> BTreeMap<String, T>
where
    F: Fn(&T) -> String,
{
    items.into_iter().map(|item| (name(&item), item)).collect()
}
