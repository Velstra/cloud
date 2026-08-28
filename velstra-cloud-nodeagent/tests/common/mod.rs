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
        Attachment, AttachmentSpec, AttachmentStatus, Image, ImageFormat, ImageSpec, ImageStatus,
        Instance, InstanceSpec, InstanceStatus, NODE_RELEASE_FINALIZER, NetworkSpec, NetworkStatus,
        Node, NodeSpec, NodeStatus, Port, PortSpec, PortStatus, Resource, SubnetSpec, SubnetStatus,
    },
    security::{SecurityGroupSpec, SecurityGroupStatus, SecurityRule},
};
use velstra_cloud_nodeagent::{Agent, AgentConfig, FakeDatapath, FakeVmm};
use velstra_cloud_store::{Entry, Event, Expect, MemoryStore, Store, StoreError, TypedStore};

pub const REGION: &str = "eu-central";
pub const CELL: &str = "cell-1";
pub const IMAGE: &str = "projects/p1/images/sha256-abc";
/// What the node files that image's bytes under — its digest, not its name.
pub const IMAGE_STORED: &str = "sha256-a563f72f58d859571895f7b2777cada5baf5c39c00560a285ad546a899833913";

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
            evacuate: false,
            vcpu_overcommit: 0,
            fence_after_s: 0,
            schedulable: true,
            labels: vec![],
            cpu_baseline: None,
            gateway: false,
        },
        NodeStatus::default(),
    );
    nodes(store)
        .create(
            &node,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
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
    migrations(store)
        .create(
            &migration,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
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

pub fn meta(name: &str) -> Meta {
    let mut meta = Meta::new(
        ResourceName::parse(name).unwrap(),
        Placement::new(REGION, CELL),
    );
    // The controller that assigns work also holds the release finalizer, so
    // every object here carries one from the start — the same as in a cell.
    //
    // That sentence was false for two years and this line is why nobody found
    // out. Nothing added a finalizer to an instance or a port: `InstanceController`
    // and the guard in `PortController` did not exist, so `Api::delete` removed
    // both outright the moment it was asked, and the teardown paths below —
    // which are reached only by reading a *stored* object that is being deleted —
    // never ran outside this file. A fixture that manufactures the precondition
    // production never produced is a fixture that tests a branch nothing
    // reaches. It is true now, and `velstra-cloud-e2e/tests/cell.rs` asserts it
    // from the outside so that this line cannot go back to being the only place
    // it holds.
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
            start_order: 0,
            start_delay_s: 0,
            on_node_loss: Default::default(),
            console: false,
            devices: Vec::new(),
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
    // The image this instance names has to exist as an object, not only as a
    // string in the spec: a node fetches from the registered image's
    // `source_url`, so an instance pointing at an unregistered image can never
    // boot. Registered here, idempotently, because every instance in these
    // tests boots the same one.
    register_image(store, IMAGE).await;
    instances(store)
        .create(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    instance
}

/// Register the image the instances here boot from, if it is not already there.
///
/// `file://` because these tests drive the fake VMM, which does not fetch. What
/// they exercise is that the node is told *where* to look — the field that was
/// carried on the wire, shown in the console, and read by nothing until the
/// agent learned to resolve it.
pub async fn register_image(store: &Arc<dyn Store>, name: &str) {
    let images: TypedStore<ImageSpec, ImageStatus> = TypedStore::new(store.clone(), CELL, "images");
    if images.get(name).await.ok().flatten().is_some() {
        return;
    }
    let image = Resource::new(
        meta(name),
        ImageSpec {
            family: "debian-13".into(),
            version: "20260815".into(),
            source_instance: None,
            // A real-length digest: the bytes are filed on a node under
            // `sha256-<64 hex>`, so a short one names nothing.
            digest: "sha256:a563f72f58d859571895f7b2777cada5baf5c39c00560a285ad546a899833913".into(),
            format: ImageFormat::Raw,
            size_bytes: 1024,
            source_url: "file:///var/lib/velstra/images/abc.raw".into(),
            signature: None,
        },
        ImageStatus::default(),
    );
    let _ = images
        .create(
            &image,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await;
}

/// The network and subnet every port here names.
///
/// Both, because they carry different halves of what a guest is told: the
/// network has the MTU, the subnet has the range, the gateway and the
/// resolvers. A cell that has ports but neither of these can still run guests —
/// they are simply told less — which is why they are a separate helper rather
/// than something `create_port` does.
pub async fn create_network(store: &Arc<dyn Store>, cidr: &str, gateway: &str) {
    let networks: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), CELL, "networks");
    networks
        .create(
            &Resource::new(
                meta("projects/p1/networks/n1"),
                NetworkSpec {
                    host_bridge: String::new(),
                    vni: 4711,
                    mtu: 1450,
                    external: false,
                    announce: Default::default(),
                },
                NetworkStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    let subnets: TypedStore<SubnetSpec, SubnetStatus> =
        TypedStore::new(store.clone(), CELL, "subnets");
    subnets
        .create(
            &Resource::new(
                meta("projects/p1/subnets/s1"),
                SubnetSpec {
                    network: "projects/p1/networks/n1".into(),
                    cidr: cidr.to_string(),
                    gateway: gateway.to_string(),
                    dns: vec![gateway.to_string()],
                    reserved: vec![gateway.to_string()],
                },
                SubnetStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

/// The segment a fixture port sits on, if nobody has written it yet.
///
/// A port really is on one: the datapath is handed the network so it can put the
/// frame on the right wire, and a port naming a network nobody wrote down is a
/// port no real datapath could program. The fixtures used to leave it out, and
/// the agent used to not notice — which is what the trait taking only the
/// network's *name* had been hiding.
///
/// Idempotent, because several ports share one segment and each of them asks.
async fn ensure_segment(store: &Arc<dyn Store>) {
    let typed: TypedStore<NetworkSpec, NetworkStatus> =
        TypedStore::new(store.clone(), CELL, "networks");
    let _ = typed
        .create(
            &Resource::new(
                meta("projects/p1/networks/n1"),
                NetworkSpec {
                    host_bridge: String::new(),
                    vni: 4711,
                    mtu: 1450,
                    external: false,
                    announce: Default::default(),
                },
                NetworkStatus::default(),
            ),
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await;
}

pub async fn create_port(
    store: &Arc<dyn Store>,
    name: &str,
    address: &str,
    owned_by: &str,
) -> Port {
    ensure_segment(store).await;
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
    ports(store)
        .create(
            &port,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
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
    attachments(store)
        .create(
            &attachment,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    attachment
}

/// A place captures and backups are written to.
pub async fn create_target(store: &Arc<dyn Store>, id: &str, path: &str) {
    let targets: TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    > = TypedStore::new(store.clone(), CELL, "backup-targets");
    let target: velstra_cloud_model::resources::BackupTarget = Resource::new(
        meta(&format!("backup-targets/{id}")),
        velstra_cloud_model::backup::BackupTargetSpec {
            kind: velstra_cloud_model::backup::TargetKind::Directory,
            path: path.to_string(),
            accepting: true,
            // Nobody reports on it here: this is a node writing to a path, and
            // whether some pool agent is watching it is a different question.
            // `writable: None` means "unknown", which is not a refusal.
            agent: String::new(),

            verify_every_hours: 0,
        },
        Default::default(),
    );
    targets
        .create(
            &target,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

/// "Make an image out of this guest."
pub async fn create_capture(store: &Arc<dyn Store>, id: &str, instance: &str, node: &str) {
    let captures: TypedStore<
        velstra_cloud_model::capture::CaptureSpec,
        velstra_cloud_model::capture::CaptureStatus,
    > = TypedStore::new(store.clone(), CELL, "captures");
    let capture: velstra_cloud_model::resources::Capture = Resource::new(
        meta(&format!("projects/p1/captures/{id}")),
        velstra_cloud_model::capture::CaptureSpec {
            instance: instance.to_string(),
            target: "backup-targets/archive".into(),
            node: node.to_string(),
            label: id.to_string(),
        },
        Default::default(),
    );
    captures
        .create(
            &capture,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

pub async fn read_capture(
    store: &Arc<dyn Store>,
    id: &str,
) -> velstra_cloud_model::resources::Capture {
    let captures: TypedStore<
        velstra_cloud_model::capture::CaptureSpec,
        velstra_cloud_model::capture::CaptureStatus,
    > = TypedStore::new(store.clone(), CELL, "captures");
    captures
        .get(&format!("projects/p1/captures/{id}"))
        .await
        .unwrap()
        .unwrap()
}

/// Ask a guest to stop, as an operator would.
pub async fn stop_instance(store: &Arc<dyn Store>, name: &str) {
    let store_i = instances(store);
    let mut next = store_i.get(name).await.unwrap().unwrap();
    next.spec.desired_state = velstra_cloud_model::resources::DesiredState::Stopped;
    next.meta.generation += 1;
    store_i
        .update(
            &next,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
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

pub async fn request_delete_port(store: &Arc<dyn Store>, name: &str) {
    let mut port = read_port(store, name).await;
    port.meta.deleted_at = Some(Timestamp::now());
    ports(store)
        .update(&port, &velstra_cloud_model::Writer::controller("test"))
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

    async fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<velstra_cloud_store::Page, StoreError> {
        self.inner.list_page(prefix, after, limit).await
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

pub fn security_groups(
    store: &Arc<dyn Store>,
) -> TypedStore<SecurityGroupSpec, SecurityGroupStatus> {
    TypedStore::new(store.clone(), "cell-1", "security-groups")
}

pub async fn create_security_group(store: &Arc<dyn Store>, name: &str, rules: Vec<SecurityRule>) {
    let group = Resource::new(
        meta(name),
        SecurityGroupSpec { rules },
        SecurityGroupStatus::default(),
    );
    security_groups(store)
        .create(
            &group,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

/// A port that names security groups. The plain [`create_port`] deliberately
/// names none, so that every existing test keeps describing a port with no
/// allowances rather than quietly acquiring some.
pub async fn create_port_in_groups(
    store: &Arc<dyn Store>,
    name: &str,
    address: &str,
    owned_by: &str,
    groups: &[&str],
) -> Port {
    ensure_segment(store).await;
    let port = Resource::new(
        meta(name),
        PortSpec {
            network: "projects/p1/networks/n1".into(),
            subnet: "projects/p1/subnets/s1".into(),
            address: Some(address.to_string()),
            mac: Some("52:54:00:12:34:56".into()),
            security_groups: groups.iter().map(|g| (*g).to_string()).collect(),
            ..Default::default()
        },
        PortStatus {
            node: Some(owned_by.to_string()),
            ..Default::default()
        },
    );
    ports(store)
        .create(
            &port,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
    port
}

/// A reader shaped like the API rather than like the store.
///
/// ## Why this exists
///
/// Every agent test builds its agent over [`velstra_cloud_nodeagent::StoreCell`],
/// which hands the whole cell over unfiltered. Production, through
/// [`velstra_cloud_nodeagent::ApiCell`], does not: the API hands a node **its
/// own** ports and computes group membership centrally, putting it on the group
/// as `status.members`.
///
/// That difference is not cosmetic. A node that works membership out from the
/// ports it can see gets the right answer against the store and a *silently
/// narrower* one against the API — a rule naming a group whose members live on
/// other machines expands to nothing, and a guest loses traffic its tenant
/// allowed with no error anywhere. Testing only against the store is what let
/// that sit there: the test that should have caught it was green, because it
/// was asking the wrong reader.
///
/// So this reproduces the two real differences and nothing else:
///
/// * `ports()` returns only ports this node holds or has been given.
/// * `security_groups()` returns groups with `status.members` filled in from
///   **all** ports, which is what the API computes.
///
/// `nodes()` and `ceph_clusters()` delegate unfiltered, and that is not an
/// omission: they are cell-wide collections, and a cell-wide list is served
/// whole to a cell operator — which the node agent's identity has to be, since
/// nothing else may read a node. The case that is *not* covered here is the
/// misconfigured one, where the agent authenticates as an ordinary subject and
/// the API narrows both lists to nothing rather than refusing them. That has no
/// test double because it has no interesting behaviour to model: everything
/// simply comes back empty. It is caught at run time instead, by the agent
/// noticing that a node list came back without the agent's own node in it,
/// which cannot happen in a healthy cell.
pub struct ApiShaped {
    inner: velstra_cloud_nodeagent::StoreCell,
    ports: TypedStore<PortSpec, PortStatus>,
    groups: TypedStore<
        velstra_cloud_model::security::SecurityGroupSpec,
        velstra_cloud_model::security::SecurityGroupStatus,
    >,
    node: String,
}

impl ApiShaped {
    pub fn new(store: Arc<dyn Store>, node: &str) -> Self {
        Self {
            inner: velstra_cloud_nodeagent::StoreCell::new(store.clone(), CELL, node),
            ports: ports(&store),
            groups: TypedStore::new(store.clone(), CELL, "security-groups"),
            node: node.to_string(),
        }
    }

    async fn all_ports(&self) -> Vec<Port> {
        self.ports.list().await.unwrap_or_default()
    }
}

#[async_trait]
impl velstra_cloud_nodeagent::CellReader for ApiShaped {
    async fn ports(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Port>> {
        let me = self.node.as_str();
        Ok(self
            .all_ports()
            .await
            .into_iter()
            .filter(|p| p.status.node.as_deref() == Some(me) || p.spec.node.as_deref() == Some(me))
            .collect())
    }

    async fn security_groups(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::security::SecurityGroup>>
    {
        let specs: std::collections::BTreeMap<String, PortSpec> = self
            .all_ports()
            .await
            .into_iter()
            .map(|p| (p.meta.name.to_string(), p.spec))
            .collect();
        let mut groups = self.groups.list().await.unwrap_or_default();
        for group in &mut groups {
            group.status.members =
                velstra_cloud_model::security::members_in(&group.meta.name.to_string(), &specs);
        }
        Ok(groups)
    }

    async fn instances(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Instance>> {
        self.inner.instances().await
    }
    async fn attachments(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Attachment>> {
        self.inner.attachments().await
    }
    async fn migrations(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Migration>> {
        self.inner.migrations().await
    }
    async fn subnets(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Subnet>> {
        self.inner.subnets().await
    }
    async fn networks(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::resources::Network>> {
        self.inner.networks().await
    }
    async fn images(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Image>> {
        self.inner.images().await
    }
    async fn nodes(&self) -> velstra_cloud_nodeagent::HostResult<Vec<Node>> {
        self.inner.nodes().await
    }
    async fn ceph_clusters(
        &self,
    ) -> velstra_cloud_nodeagent::HostResult<Vec<velstra_cloud_model::ceph::CephCluster>> {
        self.inner.ceph_clusters().await
    }
    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()> {
        self.inner.wake().await
    }
    fn describe(&self) -> String {
        "shaped like the API: this node's ports, and membership computed centrally".to_string()
    }
}

/// An agent that reads the cell the way production does.
pub fn api_shaped_agent(
    store: Arc<dyn Store>,
    node: &str,
    vmm: &FakeVmm,
    datapath: &FakeDatapath,
) -> Agent {
    Agent::reading(
        store.clone(),
        AgentConfig::new(node, REGION, CELL),
        Arc::new(vmm.clone()),
        Arc::new(datapath.clone()),
        Arc::new(ApiShaped::new(store, node)),
    )
}

/// Take an instance's user-data away, which is how most instances are created.
///
/// The fixture above gives every instance some, and that turned out to hide a
/// defect completely: the two metadata layouts want **opposite** answers for an
/// instance that has none, and a fixture where none exists can never say so.
pub async fn clear_user_data(store: &Arc<dyn Store>, name: &str) {
    let store: TypedStore<InstanceSpec, InstanceStatus> =
        TypedStore::new(store.clone(), CELL, "instances");
    let mut instance = store.get(name).await.unwrap().expect("no such instance");
    instance.spec.user_data = None;
    // A spec change is a new generation, which the store insists on: a spec
    // edited without one is a change nothing downstream would notice.
    instance.meta.generation += 1;
    store
        .update(
            &instance,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}
