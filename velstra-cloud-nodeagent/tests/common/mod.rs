//! A cell, some objects in it, and a machine to run them on.
//!
//! The store here is the real one ([`velstra_cloud_store::MemoryStore`]), not a
//! stand-in: these tests exercise the actual access rules, which is the point —
//! an agent that is only ever tested against a permissive fake would never find
//! out that the store refuses half of what it tries.
//!
//! Each integration test binary compiles this module separately, so whatever
//! one of them does not use looks unused to that one. Hence the allow.
#![allow(dead_code)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName, Revision, Timestamp},
    migration::{Migration, MigrationSpec, MigrationStatus},
    resources::{
        Attachment, AttachmentSpec, AttachmentStatus, Instance, InstanceSpec, InstanceStatus,
        NODE_RELEASE_FINALIZER, Node, NodeSpec, NodeStatus, Port, PortSpec, PortStatus, Resource,
    },
};
use velstra_cloud_nodeagent::{Agent, AgentConfig, FakeDatapath, FakeVmm};
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Store, StoreError, TypedStore};

pub const REGION: &str = "eu-central";
pub const CELL: &str = "cell-1";
pub const IMAGE: &str = "projects/p1/images/sha256-abc";

pub fn store() -> Arc<dyn Store> {
    Arc::new(MemoryStore::new())
}

pub fn instances(store: &Arc<dyn Store>) -> TypedStore<InstanceSpec, InstanceStatus> {
    TypedStore::new(store.clone(), CELL, "instances")
}

pub fn attachments(store: &Arc<dyn Store>) -> TypedStore<AttachmentSpec, AttachmentStatus> {
    TypedStore::new(store.clone(), CELL, "attachments")
}

pub fn ports(store: &Arc<dyn Store>) -> TypedStore<PortSpec, PortStatus> {
    TypedStore::new(store.clone(), CELL, "ports")
}

pub fn nodes(store: &Arc<dyn Store>) -> TypedStore<NodeSpec, NodeStatus> {
    TypedStore::new(store.clone(), CELL, "nodes")
}

/// A node object as a controller would have registered it.
pub async fn create_node(store: &Arc<dyn Store>, id: &str) {
    let node = Resource::new(
        meta(&format!("nodes/{id}")),
        NodeSpec {
            schedulable: true,
            labels: vec![],
        },
        NodeStatus::default(),
    );
    nodes(store).create(&node).await.unwrap();
}

pub async fn read_node(store: &Arc<dyn Store>, id: &str) -> Node {
    nodes(store)
        .get(&format!("nodes/{id}"))
        .await
        .unwrap()
        .unwrap()
}

pub fn migrations(store: &Arc<dyn Store>) -> TypedStore<MigrationSpec, MigrationStatus> {
    TypedStore::new(store.clone(), CELL, "migrations")
}

/// A migration as a controller would have created it: nothing about the
/// instance changes, and its own status is empty — the destination has not
/// claimed it yet.
///
/// `status` is settable because `create` does not go through the access rule,
/// which is what lets a test arrange a half-finished handover — a URL published
/// by a receiver that is no longer listening, say — without a second agent.
pub async fn create_migration(
    store: &Arc<dyn Store>,
    name: &str,
    instance: &str,
    from: &str,
    to: &str,
    status: MigrationStatus,
) -> Migration {
    let migration = Resource::new(
        meta(name),
        MigrationSpec {
            instance: instance.to_string(),
            from_node: from.to_string(),
            to_node: to.to_string(),
            ..Default::default()
        },
        status,
    );
    migrations(store).create(&migration).await.unwrap();
    migration
}

pub async fn read_migration(store: &Arc<dyn Store>, name: &str) -> Migration {
    migrations(store).get(name).await.unwrap().unwrap()
}

pub async fn request_delete_migration(store: &Arc<dyn Store>, name: &str) {
    let mut migration = read_migration(store, name).await;
    migration.meta.deleted_at = Some(Timestamp::now());
    migrations(store)
        .update(&migration, &velstra_cloud_model::Writer::controller("test"))
        .await
        .unwrap();
}

/// The one spec write in the whole handover, made by a controller at the one
/// moment the model allows: after the source has reported that it let go.
pub async fn reassign_instance(store: &Arc<dyn Store>, name: &str, to: &str) {
    edit_instance(store, name, |spec| spec.node = Some(to.to_string())).await;
}

pub fn node_agent(
    store: Arc<dyn Store>,
    node: &str,
    vmm: &FakeVmm,
    datapath: &FakeDatapath,
) -> Agent {
    Agent::new(
        store,
        AgentConfig::new(node, REGION, CELL),
        Arc::new(vmm.clone()),
        Arc::new(datapath.clone()),
    )
}

fn meta(name: &str) -> Meta {
    let mut meta = Meta::new(
        ResourceName::parse(name).unwrap(),
        Placement::new(REGION, CELL),
    );
    // The controller that assigns work also holds the release finalizer, so
    // every object here carries one from the start — the same as in a cell.
    meta.add_finalizer(NODE_RELEASE_FINALIZER);
    meta
}

/// An instance as a controller would have left it: assigned in `spec`, and with
/// `status.node` already naming the node that owns the status.
///
/// The second half is not decoration. `access::judge` takes the owner from the
/// **stored status**, so an object whose status names nobody cannot be reported
/// on by any agent at all — see the test that pins that down.
pub async fn create_instance(
    store: &Arc<dyn Store>,
    name: &str,
    assigned_to: Option<&str>,
    owned_by: Option<&str>,
    ports: &[&str],
) -> Instance {
    let instance = Resource::new(
        meta(name),
        InstanceSpec {
            vcpus: 2,
            memory_mib: 2048,
            image: IMAGE.to_string(),
            root_disk_gib: 20,
            ports: ports.iter().map(|p| p.to_string()).collect(),
            node: assigned_to.map(str::to_string),
            ssh_keys: vec!["ssh-ed25519 AAAA-key".to_string()],
            user_data: Some(format!("#cloud-config\n# for {name}\n")),
            ..Default::default()
        },
        InstanceStatus {
            node: owned_by.map(str::to_string),
            ..Default::default()
        },
    );
    instances(store).create(&instance).await.unwrap();
    instance
}

pub async fn create_port(
    store: &Arc<dyn Store>,
    name: &str,
    address: &str,
    owned_by: &str,
) -> Port {
    let port = Resource::new(
        meta(name),
        PortSpec {
            network: "projects/p1/networks/n1".into(),
            subnet: "projects/p1/subnets/s1".into(),
            address: Some(address.to_string()),
            mac: Some("52:54:00:12:34:56".into()),
            ..Default::default()
        },
        PortStatus {
            node: Some(owned_by.to_string()),
            ..Default::default()
        },
    );
    ports(store).create(&port).await.unwrap();
    port
}

pub async fn create_attachment(
    store: &Arc<dyn Store>,
    name: &str,
    volume: &str,
    instance: &str,
    node: &str,
) -> Attachment {
    let attachment = Resource::new(
        meta(name),
        AttachmentSpec {
            volume: volume.to_string(),
            instance: instance.to_string(),
            node: node.to_string(),
            read_only: false,
        },
        AttachmentStatus {
            node: Some(node.to_string()),
            ..Default::default()
        },
    );
    attachments(store).create(&attachment).await.unwrap();
    attachment
}

pub async fn read_instance(store: &Arc<dyn Store>, name: &str) -> Instance {
    instances(store).get(name).await.unwrap().unwrap()
}

pub async fn read_attachment(store: &Arc<dyn Store>, name: &str) -> Attachment {
    attachments(store).get(name).await.unwrap().unwrap()
}

pub async fn read_port(store: &Arc<dyn Store>, name: &str) -> Port {
    ports(store).get(name).await.unwrap().unwrap()
}

/// What a controller does when an operator changes something: edit `spec`, move
/// the generation with it.
pub async fn edit_instance(
    store: &Arc<dyn Store>,
    name: &str,
    edit: impl FnOnce(&mut InstanceSpec),
) {
    let mut instance = read_instance(store, name).await;
    edit(&mut instance.spec);
    instance.meta.generation += 1;
    instances(store)
        .update(&instance, &velstra_cloud_model::Writer::controller("test"))
        .await
        .unwrap();
}

/// A delete request: metadata, so a controller makes it and the object stays
/// visible until its finalizers are gone.
pub async fn request_delete_instance(store: &Arc<dyn Store>, name: &str) {
    let mut instance = read_instance(store, name).await;
    instance.meta.deleted_at = Some(Timestamp::now());
    instances(store)
        .update(&instance, &velstra_cloud_model::Writer::controller("test"))
        .await
        .unwrap();
}

pub async fn request_delete_attachment(store: &Arc<dyn Store>, name: &str) {
    let mut attachment = read_attachment(store, name).await;
    attachment.meta.deleted_at = Some(Timestamp::now());
    attachments(store)
        .update(
            &attachment,
            &velstra_cloud_model::Writer::controller("test"),
        )
        .await
        .unwrap();
}

pub fn condition<'a>(
    conditions: &'a [velstra_cloud_model::Condition],
    kind: &str,
) -> &'a velstra_cloud_model::Condition {
    velstra_cloud_model::meta::condition(conditions, kind)
        .unwrap_or_else(|| panic!("no {kind} condition: {conditions:#?}"))
}

/// A store that can be made to stop accepting writes at an instant of the
/// test's choosing — which is how an agent is killed *between* acting on the
/// machine and reporting what it did, the one window that matters.
pub struct Brittle {
    inner: Arc<dyn Store>,
    dead: AtomicBool,
}

impl Brittle {
    pub fn wrapping(inner: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            dead: AtomicBool::new(false),
        })
    }

    /// From here on, nothing this agent says reaches the cell.
    pub fn cut(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    pub fn restore(&self) {
        self.dead.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl Store for Brittle {
    async fn get(&self, key: &str) -> Result<Option<Entry>, StoreError> {
        self.inner.get(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StoreError> {
        self.inner.list(prefix).await
    }

    async fn put(&self, key: &str, value: Vec<u8>, expect: Expect) -> Result<Revision, StoreError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("the agent stopped existing".into()));
        }
        self.inner.put(key, value, expect).await
    }

    async fn delete(&self, key: &str, expect: Expect) -> Result<Revision, StoreError> {
        self.inner.delete(key, expect).await
    }

    fn watch(&self, prefix: &str, from: Option<Revision>) -> tokio::sync::mpsc::Receiver<Event> {
        self.inner.watch(prefix, from)
    }

    async fn revision(&self) -> Result<Revision, StoreError> {
        self.inner.revision().await
    }
}
