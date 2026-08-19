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
//! ## Why this is a read-only seam
//!
//! Writes still go straight to the store, and that is not an oversight. A node's
//! writes are already proportional to its own objects — it reports on what it
//! holds and nothing else — so there is nothing to fix there, and routing them
//! through the API would put a second process in the path of a status report
//! that has to land for a guest to be usable. What was unbounded was the
//! *reading*, and this is the seam that bounds it.
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
}

impl StorePool {
    pub fn new(store: std::sync::Arc<dyn Store>, cell: &str) -> Self {
        Self {
            volumes: TypedStore::new(store.clone(), cell, "volumes"),
            snapshots: TypedStore::new(store, cell, "snapshots"),
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
    fn describe(&self) -> String {
        "the store, unfiltered: this pool reads every volume and snapshot in the cell on every \
         pass"
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
    async fn networks(&self) -> Result<Vec<Network>>;
    /// The registered images, which is where an image's *source* lives.
    ///
    /// A node verifies an image against the sha256 in its own name, but the
    /// bytes have to come from somewhere, and that somewhere is a field on the
    /// Image object. Without this the node could check an image and never
    /// obtain one: `spec.source_url` was carried through the wire, shown in
    /// the console, and read by nothing.
    async fn images(&self) -> Result<Vec<Image>>;

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
    images: TypedStore<ImageSpec, ImageStatus>,
    migrations: TypedStore<MigrationSpec, MigrationStatus>,
}

impl StoreCell {
    pub fn new(store: std::sync::Arc<dyn Store>, cell: &str, node: &str) -> Self {
        Self {
            node: node.to_string(),
            instances: TypedStore::new(store.clone(), cell, "instances"),
            attachments: TypedStore::new(store.clone(), cell, "attachments"),
            ports: TypedStore::new(store.clone(), cell, "ports"),
            groups: TypedStore::new(store.clone(), cell, "security-groups"),
            subnets: TypedStore::new(store.clone(), cell, "subnets"),
            networks: TypedStore::new(store.clone(), cell, "networks"),
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
    async fn images(&self) -> Result<Vec<Image>> {
        self.images.list().await.map_err(|e| failed("images", e))
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
