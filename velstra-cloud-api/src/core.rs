//! The handlers. Both transports are skins over this file.
//!
//! Everything a caller can ask for is a method here, and neither the REST
//! router nor the gRPC service is allowed to decide anything: they parse a
//! request, call one of these, and render the answer. A rule that lived in one
//! of them — "reject a `status` write", "bump the generation" — would hold on
//! one transport and not the other, and the two would drift apart in the exact
//! place where nobody looks.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use velstra_cloud_model::{
    assignment::Assignee,
    authz::{Verb, governing_project, may},
    ceph::{CephClusterSpec, CephClusterStatus},
    identity::{UserSpec, UserStatus},
    loadbalancer::{LoadBalancerSpec, LoadBalancerStatus},
    meta::{Meta, Placement, ResourceName, Revision, Timestamp, set_condition},
    migration::{Migration, MigrationSpec, MigrationStatus, may_migrate, migration_condition},
    reconcile::place,
    resources::{
        AttachmentSpec, AttachmentStatus, FloatingIpSpec, FloatingIpStatus, ImageSpec, ImageStatus,
        Instance, InstanceSpec, InstanceStatus, NetworkSpec, NetworkStatus, Node, NodeSpec,
        NodeStatus, OperationSpec, OperationStatus, PoolSpec, PoolStatus, PortSpec, PortStatus,
        Project, ProjectPolicy, ProjectSpec, ProjectStatus, Resource, RouterSpec, RouterStatus,
        SnapshotSpec, SnapshotStatus, SubnetSpec, SubnetStatus, Volume, VolumeSpec, VolumeStatus,
        nodes_holding,
    },
    security::{SecurityGroupSpec, SecurityGroupStatus, group_condition, validate},
    storage::{may_create_volume, may_snapshot},
};
use velstra_cloud_store::{Event, Store};

use crate::{
    auth::{Identity, TokenVerifier},
    collection::{Collection, Deleted, Patch, TypedCollection, merge},
    error::{ApiError, ApiResult, Code},
    json::joined,
    paging::{PageToken, Paging},
    served::Served,
};

/// The collections this API serves, in the order `docs/rest-contract.md` lists
/// them. A name that is not here is a 404 rather than an empty list: an
/// interface that answers a typo with `[]` sends somebody looking for their
/// missing objects.
pub const COLLECTIONS: [&str; 32] = [
    "projects",
    "folders",
    "roles",
    "users",
    "ceph-clusters",
    "instances",
    "migrations",
    "volumes",
    "snapshots",
    "attachments",
    "networks",
    "routers",
    "floatingips",
    "load-balancers",
    "subnets",
    "ports",
    "security-groups",
    "images",
    "nodes",
    "pools",
    "device-classes",
    "backup-targets",
    "backups",
    "backup-schedules",
    "audit",
    "captures",
    "console-sessions",
    "image-sources",
    "usage",
    "snapshot-schedules",
    "maintenance-windows",
    "operations",
];

/// A bare folder id becomes the full name.
///
/// Two spellings reach this field and both are somebody being reasonable. The
/// console writes every cell-scoped reference bare — `hv-1`, not `nodes/hv-1` —
/// because that is what a node and a device class are called, and a picker that
/// spelled one collection differently would be a picker somebody has to
/// remember. The field itself has said `folders/f2` since long before anything
/// walked it.
///
/// Stored in full, because the *model's* gate is spelled that way: a `parent`
/// of `projects/p1` must not climb sideways into somebody's tenancy, and
/// `hierarchy::folder_above` is what stops it. A bare id carries no kind and so
/// carries no such gate.
fn settle_parent(spec: &mut Value) {
    let Some(parent) = spec.get("parent").and_then(Value::as_str) else {
        return;
    };
    if parent.is_empty() || parent.contains('/') {
        return;
    }
    spec["parent"] = Value::String(format!(
        "{}{parent}",
        velstra_cloud_model::hierarchy::FOLDER_PREFIX
    ));
}

/// Said the same way wherever somebody tries to write a usage record.
///
/// A bill that can be written, edited or deleted through the same door the
/// customer comes in is not a bill. Readings are written by the controller,
/// straight to the store, and this is the whole of the API's part in it: no.
const RECORDS_ARE_NOT_WRITTEN_HERE: &str = "usage records are readings taken by the platform, not documents anybody writes. They cannot \
     be created, changed or deleted here — a record that could be would be a bill nobody can \
     stand behind. They are read with GET, and they go away with their project or with their \
     retention.";

/// How long a spent console session is kept as a record.
///
/// A day. The ticket is dead after a minute; what lives on is the answer to
/// "who opened a console into that machine, and when" — worth keeping for as
/// long as somebody might ask, and not worth keeping for ever. The audit trail
/// proper is `audit`, which has its own retention.
const CONSOLE_RECORD_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// One stored maintenance window as the model's decisions see it.
fn window_view(
    w: &velstra_cloud_model::resources::MaintenanceWindow,
) -> velstra_cloud_model::maintenance::WindowView {
    velstra_cloud_model::maintenance::WindowView {
        name: w.meta.name.to_string(),
        node: w.spec.node.clone(),
        starts_at: w.spec.starts_at,
        minutes: w.spec.minutes,
        drain: w.spec.drain,
        note: w.spec.note.clone(),
    }
}

/// How often the background reaper deletes expired sessions.
///
/// An hour: a session lives eight (see
/// [`velstra_cloud_model::identity::SESSION_LIFETIME_MS`]), so this is well
/// inside the lifetime and an expired record lingers at most one interval past
/// the moment it stopped being accepted. Longer would let dead rows accumulate
/// between sweeps; shorter would list the collection more often to reclaim
/// almost nothing. The bound this protects against is slow growth, not a leak
/// that matters within the hour.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How many store snapshots stay. Hourly, so a day of history — enough to step
/// back over a bad afternoon, small enough that the directory never becomes
/// the thing that fills a disk.
const KEEP_STORE_SNAPSHOTS: usize = 24;

/// One event on a watch, in the two shapes the contract defines.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEvent {
    Put(Value),
    Delete { name: String, revision: Revision },
}

/// A list, and the revision to watch from so nothing between the two is lost.
#[derive(Debug)]
pub struct Listing {
    pub items: Vec<Value>,
    /// The revision this walk started at — the *first* page's, carried forward
    /// on every page after it. See [`crate::paging`] for why that, and not each
    /// page's own, is what keeps list-then-watch correct.
    pub revision: Revision,
    /// Present exactly when there is more to fetch. Absent means the walk is
    /// over, so a caller loops until it is `None` and never has to guess.
    pub next_page_token: Option<String>,
}

/// What a create produced: the operation to follow, and the object it is about.
pub struct Created {
    pub operation: Value,
    pub target: String,
    /// The per-node agent token, present only when the created object is a node.
    ///
    /// Returned **once**, here and nowhere else: only its digest is stored, so it
    /// cannot be recovered from the cell afterwards. A node agent registered with
    /// `--api` presents this token, and the API serves it only its own objects
    /// and accepts status writes only for them.
    pub node_token: Option<String>,
    /// The same, for a storage pool.
    ///
    /// A separate field rather than one `agentToken`, because the two are
    /// genuinely different things a caller does different work with — unlike a
    /// `role`/`customRole` pair, where both would have named one grant. A
    /// registration answers with exactly one of them, and which one says what
    /// was registered.
    pub pool_token: Option<String>,
}

/// Which objects a caller is asking for.
///
/// There is exactly one thing to filter on and it is the agent asking, because
/// there is exactly one kind of caller that must not be handed the cell. A
/// console, an operator or a controller is asking about the cell on purpose.
///
/// This is the piece that decides whether a cell is bounded by its store or by
/// the agents around it. Without it every node lists every instance, port,
/// attachment and migration on every pass and every pool lists every volume and
/// snapshot, so load per agent grows with the cell and a thousand agents
/// multiply every write by a thousand. It is the same wall Kubernetes hit, and
/// the same answer: filter in front of the store rather than making the store
/// bigger.
///
/// Applied by the API and never by the caller. A filter the caller applies has
/// already cost the read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only what this agent has business with — see
    /// [`velstra_cloud_model::assignment`] for what that means and why it is not
    /// simply the assignment.
    pub assignee: Option<Assignee>,
    /// Only objects carrying these labels. Empty matches everything, which is
    /// what "no filter" has to mean — the alternative is a filter box that
    /// empties the list when it is cleared.
    pub labels: Vec<velstra_cloud_model::meta::LabelTerm>,
    /// Only records *about* this resource: operations whose `spec.target` is
    /// it, audit lines whose `spec.target` is it.
    ///
    /// The one filter that is about a field rather than about who is asking,
    /// and it exists because "what has happened to this guest" is the question
    /// every console user has and no listing answered. Without it a console
    /// would fetch every operation in the cell to show six lines about one
    /// object — which is the cost these filters exist to avoid.
    pub target: Option<String>,
}

impl Filter {
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether this object carries the labels asked for.
    ///
    /// Read off the document rather than the typed object: this runs before
    /// the expensive half of a listing, and parsing an object to reject it
    /// would be paying exactly the cost this ordering exists to avoid.
    fn labels_admit(&self, document: &Value) -> bool {
        if self.labels.is_empty() {
            return true;
        }
        let labels: std::collections::BTreeMap<String, String> = document
            .get("meta")
            .and_then(|m| m.get("labels"))
            .and_then(|l| serde_json::from_value(l.clone()).ok())
            .unwrap_or_default();
        velstra_cloud_model::meta::labels_match(&labels, &self.labels)
    }

    pub fn for_node(node: impl Into<String>) -> Self {
        Self {
            labels: Vec::new(),
            target: None,
            assignee: Some(Assignee::Node(node.into())),
        }
    }

    pub fn for_pool(pool: impl Into<String>) -> Self {
        Self {
            labels: Vec::new(),
            target: None,
            assignee: Some(Assignee::Pool(pool.into())),
        }
    }

    /// Whether this record is about the resource that was asked about.
    ///
    /// Read off the document, like the label check and for the same reason:
    /// this runs before the expensive half of a listing.
    fn target_admits(&self, document: &Value) -> bool {
        let Some(wanted) = &self.target else {
            return true;
        };
        document
            .get("spec")
            .and_then(|s| s.get("target"))
            .and_then(Value::as_str)
            == Some(wanted.as_str())
    }

    /// Whether this object passes.
    ///
    /// Three answers, and conflating the last two is a hole rather than a
    /// simplification — see
    /// [`velstra_cloud_model::assignment::is_shared_collection`].
    pub fn admits(&self, kind: &str, document: &Value) -> bool {
        use velstra_cloud_model::assignment as who_reads;
        let Some(who) = &self.assignee else {
            return true;
        };
        // Shared: nobody owns one, everybody reads them whole.
        if who_reads::is_shared_collection(kind) {
            return true;
        }
        // Somebody's, but not this kind of agent's. Nothing, not everything.
        if !who_reads::is_assigned_to(kind, who) {
            return false;
        }
        who_reads::concerns_assignee(document, who)
    }
}

/// What a batch of computed fields needs from *other* collections, read at most
/// once per request.
///
/// Without this, `answer` is per-document and every computed field that needs a
/// collection scan costs one scan **per document** — so listing a thousand
/// security groups in a cell of ten thousand ports was ten million reads, and
/// the same shape would have made a subnet's occupancy worse still. One list per
/// request instead of one per item is the difference between a read that grows
/// with the cell and one that grows with its square.
///
/// Deliberately not a cache that outlives the request. A computed field's whole
/// point is that it cannot disagree with the world, and a value kept between
/// requests can.
#[derive(Default)]
struct Scratch {
    ports: Option<Arc<Vec<velstra_cloud_model::resources::Port>>>,
    nodes: Option<Arc<Vec<Node>>>,
    floating: Option<Arc<Vec<velstra_cloud_model::resources::FloatingIp>>>,
    balancers: Option<Arc<Vec<velstra_cloud_model::loadbalancer::LoadBalancer>>>,
}

impl Scratch {
    async fn ports(
        &mut self,
        api: &Api,
    ) -> ApiResult<Arc<Vec<velstra_cloud_model::resources::Port>>> {
        if let Some(ports) = &self.ports {
            return Ok(ports.clone());
        }
        let ports = Arc::new(api.typed_list("", "ports").await?);
        self.ports = Some(ports.clone());
        Ok(ports)
    }

    async fn floating(
        &mut self,
        api: &Api,
    ) -> ApiResult<Arc<Vec<velstra_cloud_model::resources::FloatingIp>>> {
        if let Some(floating) = &self.floating {
            return Ok(floating.clone());
        }
        let floating = Arc::new(api.typed_list("", "floatingips").await?);
        self.floating = Some(floating.clone());
        Ok(floating)
    }

    async fn balancers(
        &mut self,
        api: &Api,
    ) -> ApiResult<Arc<Vec<velstra_cloud_model::loadbalancer::LoadBalancer>>> {
        if let Some(balancers) = &self.balancers {
            return Ok(balancers.clone());
        }
        let balancers = Arc::new(api.typed_list("", "load-balancers").await?);
        self.balancers = Some(balancers.clone());
        Ok(balancers)
    }

    async fn nodes(&mut self, api: &Api) -> ApiResult<Arc<Vec<Node>>> {
        if let Some(nodes) = &self.nodes {
            return Ok(nodes.clone());
        }
        let nodes = Arc::new(api.typed_list("", "nodes").await?);
        self.nodes = Some(nodes.clone());
        Ok(nodes)
    }
}

struct Inner {
    /// The store itself, beside the typed views over it — for the one job no
    /// collection can carry: compacting the history they all share.
    store: Arc<dyn Store>,
    /// Where the store's snapshots go, when an operator named a place.
    ///
    /// `None` — the default — takes none, which is the honest state of a dev
    /// cell. Named, the sweeper writes one per round and keeps the newest few:
    /// guests survive their control plane dying, but a cell whose store is
    /// gone is a cell nobody will ever manage again. Point it somewhere that
    /// is not this machine's own disk.
    store_backup_dir: Option<std::path::PathBuf>,
    collections: BTreeMap<&'static str, Arc<dyn Collection>>,
    /// Subjects that may do anything, anywhere in this cell.
    ///
    /// Configuration rather than data, and deliberately: it is what a fresh cell
    /// is bootstrapped from, and a permission stored inside the thing it
    /// protects has no answer for the first request. Empty means **nobody is an
    /// operator**, which is the safe direction — a cell started without one can
    /// be read and written only where a project grants it.
    cell_admins: Vec<String>,
    /// One watch on the store per assigned collection, however many node agents.
    ///
    /// Only the four that grow with the cell, and only reached through a
    /// filtered read — which in this platform means a node agent. Everything
    /// else, including every path a person looks at, goes to the store, because
    /// a cached read is eventually consistent and somebody reading back what
    /// they just changed must not be.
    ///
    /// Empty until [`Api::serve_nodes`] is called, so a process that has no node
    /// agents talking to it — a test, a one-shot tool — pays nothing.
    served: RwLock<BTreeMap<&'static str, Served>>,
    placement: Placement,
    verifier: Arc<dyn TokenVerifier>,
    /// The two collections the API stores and never serves — a user's password
    /// and their live sessions. Held here so the sign-in routes can reach them
    /// and nothing else can: they are not in `collections`, so there is no
    /// route, no list, no watch and no proxy hop that arrives at them.
    identity: crate::sessions::IdentityStore,
    /// One allowance per caller, for **writes**.
    ///
    /// A mutex rather than anything cleverer: taking a token is a few integer
    /// operations, and a lock held for that long is not the thing that will
    /// ever be slow here. See [`velstra_cloud_model::limit`] for what this is
    /// and — more to the point — what it is not.
    limiter: std::sync::Mutex<velstra_cloud_model::limit::Limiter>,
    /// `None` turns it off entirely, which is what a single-tenant cell and
    /// every test wants: a limiter is about one tenant taking the write path
    /// from another, and a cell with one tenant has no such problem.
    write_rate: Option<velstra_cloud_model::limit::Rate>,
}

#[derive(Clone)]
pub struct Api {
    inner: Arc<Inner>,
}

impl Api {
    pub fn new(
        store: Arc<dyn Store>,
        region: &str,
        cell: &str,
        verifier: Arc<dyn TokenVerifier>,
    ) -> Self {
        macro_rules! collection {
            ($kind:literal, $spec:ty, $status:ty) => {
                (
                    $kind,
                    Arc::new(TypedCollection::<$spec, $status>::new(
                        store.clone(),
                        cell,
                        $kind,
                    )) as Arc<dyn Collection>,
                )
            };
        }
        let collections = BTreeMap::from([
            collection!("projects", ProjectSpec, ProjectStatus),
            collection!(
                "folders",
                velstra_cloud_model::hierarchy::FolderSpec,
                velstra_cloud_model::hierarchy::FolderStatus
            ),
            collection!(
                "roles",
                velstra_cloud_model::hierarchy::RoleSpec,
                velstra_cloud_model::hierarchy::RoleStatus
            ),
            // Servable, unlike `credentials` and `sessions`, which are stored
            // beside it and deliberately have no route at all — see
            // `crate::sessions`. A user record holds no secret, so listing one
            // is an ordinary read; the thing worth protecting is not in it.
            collection!("users", UserSpec, UserStatus),
            // Cell-scoped, and effectively a singleton: a cell has one Ceph
            // cluster or none. Not enforced by the type — the refusal belongs
            // where it can say why, which is `create` below.
            collection!("ceph-clusters", CephClusterSpec, CephClusterStatus),
            collection!("instances", InstanceSpec, InstanceStatus),
            collection!("volumes", VolumeSpec, VolumeStatus),
            collection!("snapshots", SnapshotSpec, SnapshotStatus),
            collection!("attachments", AttachmentSpec, AttachmentStatus),
            collection!("networks", NetworkSpec, NetworkStatus),
            collection!("routers", RouterSpec, RouterStatus),
            collection!("floatingips", FloatingIpSpec, FloatingIpStatus),
            collection!("load-balancers", LoadBalancerSpec, LoadBalancerStatus),
            collection!("subnets", SubnetSpec, SubnetStatus),
            collection!("ports", PortSpec, PortStatus),
            collection!("security-groups", SecurityGroupSpec, SecurityGroupStatus),
            collection!("images", ImageSpec, ImageStatus),
            collection!("nodes", NodeSpec, NodeStatus),
            collection!(
                "device-classes",
                velstra_cloud_model::pci::DeviceClassSpec,
                velstra_cloud_model::resources::DeviceClassStatus
            ),
            collection!(
                "backup-targets",
                velstra_cloud_model::backup::BackupTargetSpec,
                velstra_cloud_model::backup::BackupTargetStatus
            ),
            collection!(
                "backups",
                velstra_cloud_model::backup::BackupSpec,
                velstra_cloud_model::backup::BackupStatus
            ),
            collection!(
                "snapshot-schedules",
                velstra_cloud_model::storage::SnapshotScheduleSpec,
                velstra_cloud_model::storage::SnapshotScheduleStatus
            ),
            collection!(
                "captures",
                velstra_cloud_model::capture::CaptureSpec,
                velstra_cloud_model::capture::CaptureStatus
            ),
            collection!(
                "console-sessions",
                velstra_cloud_model::console::ConsoleSessionSpec,
                velstra_cloud_model::console::ConsoleSessionStatus
            ),
            collection!(
                "image-sources",
                velstra_cloud_model::images::ImageSourceSpec,
                velstra_cloud_model::images::ImageSourceStatus
            ),
            collection!(
                "usage",
                velstra_cloud_model::usage::UsageRecordSpec,
                velstra_cloud_model::usage::UsageRecordStatus
            ),
            collection!(
                "audit",
                velstra_cloud_model::audit::AuditSpec,
                velstra_cloud_model::audit::AuditStatus
            ),
            collection!(
                "backup-schedules",
                velstra_cloud_model::backup::BackupScheduleSpec,
                velstra_cloud_model::backup::BackupScheduleStatus
            ),
            collection!(
                "maintenance-windows",
                velstra_cloud_model::maintenance::MaintenanceWindowSpec,
                velstra_cloud_model::maintenance::MaintenanceWindowStatus
            ),
            collection!("pools", PoolSpec, PoolStatus),
            collection!("operations", OperationSpec, OperationStatus),
            collection!("migrations", MigrationSpec, MigrationStatus),
        ]);
        Self {
            inner: Arc::new(Inner {
                store: store.clone(),
                store_backup_dir: None,
                collections,
                cell_admins: Vec::new(),
                served: RwLock::new(BTreeMap::new()),
                placement: Placement::new(region, cell),
                verifier: verifier.clone(),
                identity: crate::sessions::IdentityStore::new(store.clone(), region, cell),
                limiter: std::sync::Mutex::new(velstra_cloud_model::limit::Limiter::new()),
                // Off unless a caller asks for it: a limiter is about one
                // tenant taking the write path from another, and a cell with
                // one tenant has no such problem.
                write_rate: None,
            }),
        }
    }

    pub fn verifier(&self) -> &Arc<dyn TokenVerifier> {
        &self.inner.verifier
    }

    /// The sign-in surface. Everything that touches a password or a session goes
    /// through this one handle, which is what keeps that list auditable.
    pub fn identity(&self) -> &crate::sessions::IdentityStore {
        &self.inner.identity
    }

    /// Whether `who` may administer the cell — either from the started-with
    /// operator list or from their own user record.
    pub fn is_operator(&self, who: &Identity) -> bool {
        self.inner.cell_admins.contains(&who.subject) || crate::sessions::is_cell_admin(who)
    }

    fn collection(&self, kind: &str) -> ApiResult<Arc<dyn Collection>> {
        self.inner.collections.get(kind).cloned().ok_or_else(|| {
            ApiError::new(
                Code::NotFound,
                format!("there is no collection called {kind}"),
            )
        })
    }

    // ---- reads ------------------------------------------------------------

    /// One object, by name.
    pub async fn get(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
        // An audit record is judged by what it is about, so the decision needs
        // the record — see `may_read`. Reading it first is safe: a refusal is
        // still a refusal, and the object's existence was already implied by
        // the caller naming it.
        let collection = self.collection(name.collection())?;
        if name.collection() == "audit" {
            let document = collection
                .get(&name.to_string())
                .await?
                .ok_or_else(|| ApiError::not_found(name))?;
            if !self.may_read(who, name, &document).await {
                return Err(self.refuse_a_read(who, name).await);
            }
            let mut document = document;
            self.answer(&mut document, &mut Scratch::default()).await?;
            return Ok(document);
        }
        self.authorize(who, Verb::Read, name).await?;
        let mut document = collection
            .get(&name.to_string())
            .await?
            .ok_or_else(|| ApiError::not_found(name))?;
        // Raw for a machine, for the reason `Gate::Machine` gives: an agent
        // that reads a decorated object writes its own undecorated one back,
        // every pass, for ever.
        if crate::sessions::agent_node(who).is_none() {
            self.answer(&mut document, &mut Scratch::default()).await?;
        }
        self.redact_for(who, name.collection(), &mut document);
        Ok(document)
    }

    /// Take the cell's machine names off an answer that is leaving for a tenant.
    ///
    /// Hosts are not part of a project's view — a tenant cannot list them, and a
    /// migration is refused to them whole — yet `status.node` on every instance
    /// and attachment named the machine anyway, so the console dutifully showed
    /// a customer which hypervisor runs their guest. What a tenant needs from
    /// those fields is nothing: placement is the cell's business, and where the
    /// guest is running is not something they can act on.
    ///
    /// Removed rather than blanked (`skip_serializing_if` keeps an absent field
    /// absent), on the way *out* only: agents and operators read the same
    /// objects unredacted, and nothing stored changes.
    fn redact_for(&self, who: &Identity, kind: &str, document: &mut Value) {
        if self.is_operator(who) || crate::sessions::agent_node(who).is_some() {
            return;
        }
        if !matches!(
            kind,
            "instances" | "attachments" | "console-sessions" | "ports"
        ) {
            return;
        }
        for half in ["spec", "status"] {
            if let Some(part) = document.get_mut(half).and_then(Value::as_object_mut) {
                part.remove("node");
            }
        }
        // The fifth door, found live in a tenant's own list after the other
        // four were shut: a port carries the machine's name twice — the two
        // `node` fields above, and again in the words of the computed Ready
        // condition ("carried by horst"). The fields go the same way as an
        // instance's; the sentence is rewritten without the name, keeping the
        // status and reason a tenant legitimately reads.
        if kind == "ports" {
            if let Some(conditions) = document
                .pointer_mut("/status/conditions")
                .and_then(Value::as_array_mut)
            {
                for c in conditions.iter_mut() {
                    if c.get("kind").and_then(Value::as_str) != Some("Ready") {
                        continue;
                    }
                    let message = match c.get("reason").and_then(Value::as_str) {
                        Some("Programmed") => "programmed and carried",
                        Some("NotProgrammed") => "the guest that uses it is not up yet",
                        _ => continue,
                    };
                    c["message"] = json!(message);
                }
            }
        }
    }

    /// Everything in a collection under `parent`, plus the revision the list is
    /// good from.
    ///
    /// The revision is taken **before** the read, so a watch started from it
    /// may repeat an event but can never skip one. The other order loses
    /// whatever was written while the list was being assembled, which is the
    /// bug that makes a console quietly stale until somebody reloads it.
    /// Name the subjects that may do anything anywhere in this cell.
    ///
    /// Called by the server process from its own configuration. A cell with none
    /// is not broken: it simply has no operator, and every request is decided by
    /// the bindings on the project it touches. What it cannot do is register a
    /// node or create a project, which is the honest consequence of nobody
    /// having been made responsible for the cell.
    pub fn with_cell_admins(mut self, admins: Vec<String>) -> Self {
        let inner =
            Arc::get_mut(&mut self.inner).expect("cell admins are named before the API is shared");
        inner.cell_admins = admins;
        self
    }

    /// Name where the store's snapshots go. See `Inner::store_backup_dir`.
    pub fn with_store_backups(mut self, dir: std::path::PathBuf) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("the backup dir is named before the API is shared");
        inner.store_backup_dir = Some(dir);
        self
    }

    /// Cap how fast one caller may **write**.
    ///
    /// Off unless asked for. What it stops is the ordinary accident — a script
    /// in a loop, a controller with no backoff — taking the cell's write path
    /// from everybody else; it is not a security boundary and does not pretend
    /// to be one.
    pub fn with_write_rate(mut self, rate: velstra_cloud_model::limit::Rate) -> Self {
        let inner =
            Arc::get_mut(&mut self.inner).expect("a write rate is set before the API is shared");
        inner.write_rate = Some(rate);
        self
    }

    /// Take one caller's write token, or say how long to wait.
    ///
    /// Node agents are exempt, and that is not a convenience: an agent reports
    /// when something changed, and something changing is not something it can
    /// defer. Refusing one would make it retry, fall behind, and eventually be
    /// judged unreachable by the control plane that was itself the reason.
    pub fn may_write_now(&self, who: &Identity) -> ApiResult<()> {
        let Some(rate) = self.inner.write_rate else {
            return Ok(());
        };
        if crate::sessions::agent_node(who).is_some() {
            return Ok(());
        }
        let now = velstra_cloud_model::meta::Timestamp::now();
        let verdict = {
            let mut limiter = self.inner.limiter.lock().unwrap();
            // Swept here rather than on a timer: the map only grows when
            // somebody is spending, and this runs exactly then.
            limiter.forget_idle(rate, now);
            limiter.take(&who.subject, rate, now)
        };
        match verdict {
            velstra_cloud_model::limit::Verdict::Allowed => Ok(()),
            velstra_cloud_model::limit::Verdict::Wait { millis } => Err(ApiError::new(
                Code::ResourceExhausted,
                format!(
                    "too many writes at once; this one would be the {} in a second. Try again in \
                     {millis} ms — the same request, unchanged, will be accepted then.",
                    rate.per_second + 1
                ),
            )
            .retry_after(millis)),
        }
    }

    /// Whether `who` may `verb` `name`, from the bindings on the project that
    /// governs it — or from the operator list, for anything outside every
    /// project.
    ///
    /// One function, called at the top of every entry point, because an
    /// authorisation rule spread across eleven call sites is an authorisation
    /// rule with a hole in it.
    /// The gate every request passes, and the one place a refusal is recorded.
    ///
    /// Wrapped rather than scattered: a refusal written at each call site is a
    /// refusal somebody forgets to write at the next one, and the call site
    /// that forgets is the one an audit is eventually about.
    async fn authorize(&self, who: &Identity, verb: Verb, name: &ResourceName) -> ApiResult<()> {
        self.authorize_for(who, verb, name, name.collection()).await
    }

    /// The same, asking about a collection the name does not itself carry.
    ///
    /// One case: **listing**. `GET /projects/p1/instances` authorises Read on
    /// `projects/p1`, and asking that as a question about *projects* was
    /// harmless while every role was a rung — a viewer reads everything, so the
    /// answer was the same either way. It stops being harmless the moment a role
    /// can name collections: somebody granted `operate` on `instances` could not
    /// list them, because the question asked was whether they may read the
    /// project object, and their role says nothing about projects.
    ///
    /// So the list path asks about what is being listed. Nothing else changes:
    /// reading one object already asks about that object's own kind.
    async fn authorize_for(
        &self,
        who: &Identity,
        verb: Verb,
        name: &ResourceName,
        kind: &str,
    ) -> ApiResult<()> {
        let verdict = self.judge(who, verb, name, kind).await;
        if let Err(refusal) = &verdict {
            self.record_refusal(who, verb, name, &refusal.to_string())
                .await;
        }
        verdict
    }

    /// Note that somebody was told no.
    ///
    /// Best-effort on purpose. A refusal that could not be written is still a
    /// refusal, and failing the request because the record failed would turn a
    /// full disk into an outage — while *granting* it would be worse. So the
    /// answer is unchanged either way and the miss is logged.
    ///
    /// The id collapses repeats within a minute (see
    /// [`velstra_cloud_model::audit::record_id`]), which is what stops somebody
    /// filling the store by hammering a forbidden path.
    async fn record_refusal(&self, who: &Identity, verb: Verb, name: &ResourceName, detail: &str) {
        use velstra_cloud_model::audit::{AuditKind, AuditSpec, AuditStatus, record_id};

        let at = velstra_cloud_model::meta::Timestamp::now();
        let spelled = match verb {
            Verb::Read => "read",
            Verb::Operate => "operate",
            Verb::Write => "write",
            Verb::Administer => "administer",
        };
        let id = record_id(
            AuditKind::Refused,
            &who.subject,
            spelled,
            &name.to_string(),
            at,
        );
        let Ok(record_name) = ResourceName::parse(&format!("audit/{id}")) else {
            return;
        };
        let Ok(collection) = self.collection("audit") else {
            return;
        };
        let spec = AuditSpec {
            kind: AuditKind::Refused,
            subject: who.subject.clone(),
            target: name.to_string(),
            verb: spelled.to_string(),
            // The same sentence the caller was given. A paraphrase is one an
            // operator has to correlate by hand against what the person
            // actually saw.
            detail: detail.to_string(),
            at,
        };
        let meta = velstra_cloud_model::meta::Meta::new(record_name, self.inner.placement.clone());
        let (Ok(meta), Ok(spec)) = (serde_json::to_value(&meta), serde_json::to_value(&spec))
        else {
            return;
        };
        // An id already taken means this was noted within the last minute.
        // That is the flood defence working, not a failure.
        if let Err(e) = collection.create(meta, spec).await {
            if e.code != Code::AlreadyExists {
                tracing::warn!(error = %e.message, "could not record a refusal");
            }
        }
        let _ = AuditStatus::default();
    }

    /// The refusal a read gets, recorded as one — the same sentence `authorize`
    /// would have produced, so a caller cannot tell the two paths apart and the
    /// audit carries one shape of line for "was told no".
    async fn refuse_a_read(&self, who: &Identity, name: &ResourceName) -> ApiError {
        match self.authorize(who, Verb::Read, name).await {
            Err(e) => e,
            // Reached only if the ordinary rule would have allowed it, which
            // `may_read` already tried. Refusing anyway would be a lie; this is
            // the branch that cannot happen, spelled out rather than unwrapped.
            Ok(()) => ApiError::forbidden("this record is not yours to read"),
        }
    }

    /// Whether this caller may read this object, given the object itself.
    ///
    /// Ordinarily this is `judge` on the name and nothing more. The exception
    /// is an **audit record**, which is a cell-wide object *about* something
    /// else — and judging it by its own name means only a cell operator ever
    /// sees one. That left the person whose request was refused unable to read
    /// the sentence explaining it, which is precisely backwards: "I clicked
    /// delete and nothing happened" is answered by a record they may not open.
    ///
    /// So a record is readable by anyone who may read **what it is about**, and
    /// by **the person it is about**. Neither leaks: the first already reads
    /// the target, and the second is their own refusal. Everything else about
    /// the cell — who else was refused what — stays an operator's.
    async fn may_read(&self, who: &Identity, name: &ResourceName, document: &Value) -> bool {
        if self.judge(who, Verb::Read, name, name.collection()).await.is_ok() {
            return true;
        }
        if name.collection() != "audit" {
            return false;
        }
        let spec = &document["spec"];
        if spec.get("subject").and_then(Value::as_str) == Some(who.subject.as_str())
            && !who.subject.is_empty()
        {
            return true;
        }
        let Some(target) = spec.get("target").and_then(Value::as_str) else {
            return false;
        };
        let Ok(target) = ResourceName::parse(target) else {
            return false;
        };
        self.judge(who, Verb::Read, &target, target.collection())
            .await
            .is_ok()
    }

    async fn judge(
        &self,
        who: &Identity,
        verb: Verb,
        name: &ResourceName,
        kind: &str,
    ) -> ApiResult<()> {
        // Two ways to be an operator, and both are checked here so no call site
        // has to remember either. The started-with list is configuration and
        // cannot be revoked from inside the cell — it is the escape hatch for an
        // installation whose stored administrators are all disabled. The scope
        // is a fact about the signed-in user, resolved once at authentication,
        // and it is what lets an operator be *appointed* without a restart.
        if self.is_operator(who) {
            return Ok(());
        }
        // A node agent may **read** the cell, and only read it. A node needs
        // tenant network config, images and the node list to run its guests, and
        // it reads all of that the way it always has — the change per-node
        // identity makes is not to what a node may see but to what it may write.
        // Its one write is a status report, which does not come through here at
        // all: it goes through `report_status`, authorised by the ownership rule
        // in `judge` rather than by a project binding. So a node's `Read` is
        // granted and its `Write`/`Administer` fall through to the ordinary rules
        // below, which refuse it — a node holds no binding anywhere.
        if verb == Verb::Read && crate::sessions::agent_node(who).is_some() {
            if velstra_cloud_model::authz::a_machine_may_read(kind) {
                return Ok(());
            }
            // The cell's accounts are not the machine room. This used to be
            // granted here and refused one screen away, in the list path — so a
            // pool agent's token was told no for `/users` and handed
            // `/users/admin`. Whichever of the two was right, having both was
            // not.
            return Err(ApiError::forbidden(format!(
                "{kind} are the cell's own. A machine agent reads what it runs on —                  nodes, pools, backup targets, Ceph clusters — and not this."
            )));
        }
        // A migration is about machines, whoever's project holds the record.
        // Its spec names two hosts, and hosts are invisible to a tenant by
        // design — so the object is the operator's whole, reads included. The
        // guest's own sheet tells a tenant everything they were ever going to
        // learn from it: where their machine is running is not on it.
        //
        // Below the agent pass, above the project bindings: node agents read
        // migrations to do the moving, and a project editor must not.
        if kind == "migrations" {
            return Err(ApiError::forbidden(
                "migrations are a cell operator's: they name the machines a guest moves \
                 between, and machines are not part of a project's view",
            ));
        }
        // The image catalogue: cell-scoped images are the cell's *published*
        // ones, and everybody in it may read them.
        //
        // Without this rule the platform has no notion of a public image at all.
        // Images live under projects, so an operator who registers Debian once
        // has registered it for one tenant; every other project has to fetch and
        // store its own copy of the same bytes, and nobody browsing the console
        // can see what is on offer before they have already put something there.
        //
        // Read, and only read. Creating outside a project is already an
        // operator's alone (see `create`), which is exactly the rule worth
        // having: anybody may boot from the catalogue, only the cell may put
        // something in it. A tenant that wants a private image still makes one
        // under its own project, where its own bindings govern it.
        if verb == Verb::Read && name.collection() == "images" && name.parent().is_none() {
            return Ok(());
        }
        // The cell's public networks, by the same argument as the catalogue.
        //
        // A public address pool is an external network the operator made at
        // cell scope, with real prefixes on its subnets. A tenant assigns an
        // address out of it to their guest — which means they have to be able
        // to *see* the pool: its name to write into a floating IP, its subnets
        // to know whether the cell offers v4, v6 or both. Read, and only read:
        // creating at cell scope is already an operator's alone, which is
        // exactly the split — anybody may draw from the pool, only the cell
        // may declare one.
        if verb == Verb::Read
            && matches!(name.collection(), "networks" | "subnets")
            && name.parent().is_none()
        {
            return Ok(());
        }
        // A folder is governed by itself and by the folders above it: granting
        // somebody Admin on `folders/eng` is what lets them manage `eng`, the
        // same way a project governs itself.
        if name.collection() == "folders" && name.parent().is_none() {
            let bindings = self.bindings_from(&name.to_string()).await;
            return may(
                &who.subject,
                &self.inner.cell_admins,
                &bindings,
                verb,
                kind,
                &self.defined_roles(&bindings).await,
            )
            .map_err(|denied| ApiError::forbidden(denied.to_string()));
        }
        let Some(project) = governing_project(name) else {
            // Outside every project: a node, a pool, the projects collection.
            // These are the cell's, and only an operator has the cell.
            return Err(ApiError::forbidden(
                "this is a cell-wide resource; only a cell operator may touch it",
            ));
        };
        // Read the project's bindings, then the bindings of every folder above
        // it. A project that is not there refuses in the same words as one that
        // refuses, so the error is not an oracle for which projects exist.
        let mut bindings = Vec::new();
        let mut parent = String::new();
        if let Ok(Some(p)) = self.typed_project(&project).await {
            bindings = p.spec.bindings;
            parent = p.spec.parent;
        }
        bindings.extend(self.bindings_above(&parent).await);
        let defined = self.defined_roles(&bindings).await;
        may(
            &who.subject,
            &self.inner.cell_admins,
            &bindings,
            verb,
            kind,
            &defined,
        )
        .map_err(|denied| ApiError::forbidden(denied.to_string()))
    }

    /// Whether `who` may follow the references a spec carries out of its own
    /// project.
    ///
    /// [`crate::refs::check`] says a reference is *well-formed*. It says nothing
    /// about who may follow it, and for a long time nothing else did either: a
    /// write was authorised against the project the object lives in, and then
    /// the object was allowed to name whatever it liked. A tenant could create a
    /// volume in their own project whose `sourceSnapshot` pointed at another
    /// tenant's snapshot — every check passed, and the pool then cloned somebody
    /// else's bytes into a volume the caller owns. `image`, `volume` and
    /// `instance` are the same shape.
    ///
    /// A reference that stays inside the object's own project costs nothing at
    /// all: the projects match and no binding is read, which is every ordinary
    /// request. Only crossing a boundary is a question worth asking.
    ///
    /// The question is asked of the *project*, never of the object being named:
    /// [`Api::authorize`] reads the governing project's bindings and never goes
    /// looking for the reference itself. So the fields that are deliberately
    /// shape-only — a port's `security_groups`, a router's `networks`, a
    /// floating IP's `port` — stay shape-only, and naming something that does
    /// not exist yet is still allowed. It also means a caller who is refused is
    /// refused in the same words whether the thing they named is there or not,
    /// so this cannot be used to find out what another tenant has.
    /// What the project this request lands in is allowed to reach for.
    ///
    /// Read once per request that needs it. A project that is not there — or
    /// cannot be read — answers with the closed policy, which is the safe
    /// direction: a permission question whose input is missing must not resolve
    /// to yes.
    async fn policy_of(&self, project: Option<&str>) -> ProjectPolicy {
        let Some(project) = project else {
            return ProjectPolicy::default();
        };
        match self.typed_project(project).await {
            Ok(Some(p)) => p.spec.policy,
            _ => ProjectPolicy::default(),
        }
    }

    /// Whether this caller may put a network on the bridge it named.
    ///
    /// Three answers in one place, in this order:
    ///
    /// * an empty bridge is a logical network and is nobody's business;
    /// * a cell operator may name anything — they are the provider;
    /// * anybody else may name a bridge **their project was given**.
    ///
    /// That last one is the whole point of a per-project policy: "may use host
    /// bridges" is not a thing anybody means. What they mean is *this* VLAN, the
    /// one that customer's cage is on, and the cell says which per customer.
    async fn refuse_a_bridge_this_project_was_not_given(
        &self,
        spec: &Value,
        project: Option<&str>,
        who: &Identity,
    ) -> ApiResult<()> {
        let bridge = spec
            .get("host_bridge")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if bridge.is_empty() || self.is_operator(who) {
            return Ok(());
        }
        let policy = self.policy_of(project).await;
        if policy.may_use_bridge(bridge) {
            return Ok(());
        }
        Err(ApiError::forbidden(if policy.host_bridges.is_empty() {
            format!(
                "this project may not put a network on a host bridge, so `{bridge}` is not \
                 something it can ask for. A guest on one is on whatever the machine is on, past \
                 this platform's addressing and its security groups — which is why it is granted \
                 per project by whoever runs the cell, on `spec.policy.hostBridges`."
            )
        } else {
            format!(
                "this project was given {}, not `{bridge}`. Host bridges are granted by name \
                 because what anybody means by one is a particular wire, not the ability to \
                 name wires.",
                policy
                    .host_bridges
                    .iter()
                    .map(|b| format!("`{b}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .at("spec.hostBridge"))
    }

    /// Whether this caller may hand a guest a piece of the host.
    async fn refuse_a_device_this_project_was_not_given(
        &self,
        spec: &Value,
        project: Option<&str>,
        who: &Identity,
    ) -> ApiResult<()> {
        let wants = spec
            .get("devices")
            .and_then(Value::as_array)
            .is_some_and(|d| !d.is_empty());
        if !wants || self.is_operator(who) || self.policy_of(project).await.device_passthrough {
            return Ok(());
        }
        Err(ApiError::forbidden(
            "this project may not pass hardware through to a guest. A passed-through device is a \
             physical thing one guest holds and no other guest can have, so it is granted per \
             project by whoever runs the cell, on `spec.policy.devicePassthrough`.",
        )
        .at("spec.devices"))
    }

    /// Whether this caller may claim an address the world can reach.
    async fn refuse_a_public_address_this_project_was_not_given(
        &self,
        project: Option<&str>,
        who: &Identity,
    ) -> ApiResult<()> {
        if self.is_operator(who) || self.policy_of(project).await.floating_ips {
            return Ok(());
        }
        Err(ApiError::forbidden(
            "this project may not hold public addresses. They are a claim on address space the \
             cell was given by whoever is above it, so they are granted per project by whoever \
             runs the cell, on `spec.policy.floatingIps`.",
        )
        .at("spec"))
    }

    /// A port belongs to one guest.
    ///
    /// Nothing used to say so, and the consequence was silent and total: two
    /// instances naming one port claim one MAC on one tap, the node sees the
    /// clash and — correctly — answers DHCP for **neither**, so *both* guests
    /// boot with no address, no metadata, no user and no SSH key. The node says
    /// so in its journal and nowhere else, so what an operator sees is two
    /// machines that run and cannot be reached, with nothing on either object
    /// to explain it.
    ///
    /// Refused here instead, where the second instance is still a request
    /// somebody is making, and the sentence can name the guest that already has
    /// it. A port with no guest is the ordinary case and costs one list.
    async fn refuse_a_port_two_guests_would_share(
        &self,
        name: &ResourceName,
        spec: &Value,
    ) -> ApiResult<()> {
        let wanted: Vec<String> = spec
            .get("ports")
            .and_then(Value::as_array)
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if wanted.is_empty() {
            return Ok(());
        }
        let mine = name.to_string();
        let instances: Vec<Instance> = self.typed_list("", "instances").await?;
        for other in instances {
            let theirs = other.meta.name.to_string();
            // Its own ports are not a clash, and neither are those of an
            // instance on its way out: a port is released when the guest
            // holding it is gone, and refusing until then would make replacing
            // a machine a two-step wait.
            if theirs == mine || other.meta.is_deleting() {
                continue;
            }
            for port in &wanted {
                if other.spec.ports.iter().any(|held| held == port) {
                    return Err(ApiError::new(
                        Code::FailedPrecondition,
                        format!(
                            "{port} is already {theirs}'s. A port is one guest's NIC — two \
                             guests holding it would share a MAC on one wire, and this node \
                             would answer DHCP for neither, so both would come up with no \
                             address at all. Give this guest its own port."
                        ),
                    )
                    .at("spec.ports"));
                }
            }
        }
        Ok(())
    }

    async fn authorize_references(
        &self,
        who: &Identity,
        kind: &str,
        spec: &Value,
        home: Option<&str>,
    ) -> ApiResult<()> {
        for text in crate::refs::names_to_authorize(kind, spec) {
            // Anything unparseable was already refused by `refs::check`, which
            // says which field it was; re-refusing it here in worse words helps
            // nobody.
            let Ok(name) = ResourceName::parse(&text) else {
                continue;
            };
            if governing_project(&name).as_deref() == home {
                continue;
            }
            self.authorize(who, Verb::Read, &name).await?;
        }
        Ok(())
    }

    /// Every binding that governs something whose parent is `parent`.
    ///
    /// The walk upward, and the whole of what a folder does. Bounded by
    /// [`velstra_cloud_model::hierarchy::MAX_DEPTH`] and by having seen a name
    /// before, because this is a *permission check*: it has to answer with
    /// whatever the store holds rather than be the place a bad store is found.
    ///
    /// One read per folder in the chain, which is one or two in every cell
    /// anybody has drawn, and none at all for a project at the top — the
    /// ordinary case costs exactly what it cost before folders existed.
    async fn bindings_above(&self, parent: &str) -> Vec<velstra_cloud_model::authz::Binding> {
        use velstra_cloud_model::hierarchy::{FOLDER_PREFIX, MAX_DEPTH, folder_above};

        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut here = folder_above(parent).map(str::to_string);
        while let Some(name) = here {
            if seen.contains(&name) || seen.len() >= MAX_DEPTH {
                break;
            }
            seen.push(name.clone());
            let Some(folder) = self.typed_folder(&name).await else {
                // A folder that is not there grants nothing and ends the walk.
                // A project whose folder was deleted is a project whose own
                // bindings still govern it: an outage caused by housekeeping
                // above somebody is the wrong answer.
                break;
            };
            out.extend(folder.spec.bindings);
            here = folder_above(&folder.spec.parent).map(str::to_string);
        }
        let _ = FOLDER_PREFIX;
        out
    }

    /// A folder's own bindings, plus those of every folder above it.
    async fn bindings_from(&self, folder: &str) -> Vec<velstra_cloud_model::authz::Binding> {
        let mut out = Vec::new();
        if let Some(here) = self.typed_folder(folder).await {
            out.extend(here.spec.bindings);
            out.extend(self.bindings_above(&here.spec.parent).await);
        }
        out
    }

    /// The roles these bindings actually name, read once.
    ///
    /// Only the ones named: a cell with forty roles and a project that uses one
    /// reads one object, not forty. Nothing at all for the ordinary case, where
    /// every binding carries a rung — which is the case this must not make
    /// slower, because it is every request.
    async fn defined_roles(
        &self,
        bindings: &[velstra_cloud_model::authz::Binding],
    ) -> Vec<velstra_cloud_model::authz::CustomRole> {
        use velstra_cloud_model::authz::Role;

        let mut wanted: Vec<&str> = bindings
            .iter()
            .filter_map(|b| match &b.role {
                Role::Custom(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        let Ok(collection) = self.collection("roles") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for name in wanted {
            let Ok(Some(document)) = collection.get(name).await else {
                continue;
            };
            let Ok(object) =
                serde_json::from_value::<velstra_cloud_model::hierarchy::RoleObject>(document)
            else {
                continue;
            };
            out.push(velstra_cloud_model::authz::CustomRole {
                name: name.to_string(),
                grants: object.spec.grants,
            });
        }
        out
    }

    async fn typed_folder(&self, name: &str) -> Option<velstra_cloud_model::hierarchy::Folder> {
        let document = self.collection("folders").ok()?.get(name).await.ok()??;
        serde_json::from_value(document).ok()
    }

    async fn typed_project(&self, name: &str) -> ApiResult<Option<Project>> {
        let collection = self.collection("projects")?;
        match collection.get(name).await? {
            Some(document) => Ok(Some(serde_json::from_value(document)?)),
            None => Ok(None),
        }
    }

    /// Start serving agents — nodes and pools — from one watch per collection
    /// instead of one per agent.
    ///
    /// Called by the server process, not by a test or a one-shot tool: it spawns
    /// a task per agent-facing collection that holds a copy of it in memory for as
    /// long as the process lives. What it buys is the difference between an etcd
    /// cluster with one watcher per collection and one with a thousand.
    pub fn serve_agents(&self) {
        let mut served = self.inner.served.write().unwrap();
        for (kind, collection) in &self.inner.collections {
            if velstra_cloud_model::assignment::is_assigned_collection(kind)
                || velstra_cloud_model::assignment::is_pooled_collection(kind)
            {
                served.insert(kind, Served::start(collection.clone()));
            }
        }
    }

    /// Start a background task that deletes expired sessions on a timer.
    ///
    /// The request path sweeps a session the moment it is presented past its
    /// expiry, but a token never presented again leaves a record nothing ever
    /// reads — so without this the store grows one row per such sign-in for ever.
    /// This is the periodic reaper for exactly those; see
    /// [`crate::sessions::IdentityStore::sweep_expired_sessions`] for why it is
    /// expiry-only.
    ///
    /// Spawned by the server process and not by [`Api::new`], for the same reason
    /// [`Api::serve_agents`] is: a test or a one-shot tool that builds an `Api`
    /// pays for none of it. The clock lives here, so the swept-against time is
    /// real in production and injectable in a test that calls
    /// `sweep_expired_sessions` directly.
    pub fn spawn_session_sweeper(&self) {
        let identity = self.inner.identity.clone();
        // A second thing to reap on the same timer, for the same reason: a
        // console session outlives the ticket it carried, and without this every
        // click on Console leaves a row behind for ever.
        let api = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SESSION_SWEEP_INTERVAL);
            // The store's history, on the same heartbeat. A store that keeps
            // every revision for ever fills up and stops taking writes — found
            // live: a two-day-old cell held 393 kB of objects under two
            // gigabytes of their history, hit etcd's quota, and answered
            // `database space exceeded` to a login. Nobody was compacting, so
            // nothing could recover without an operator and etcdctl.
            //
            // Compacted to where the revision stood a full interval ago, never
            // to now: a watcher that reconnects resumes from the revision its
            // list gave it, and a window of one interval is what makes an
            // ordinary reconnect land inside history that still exists. One
            // that sleeps longer gets the compaction error the watch path
            // already turns into a clean resync.
            //
            // The file does not shrink — freed pages are reused, which is what
            // stops the growth; `etcdctl defrag` is the operator's tool for
            // reclaiming disk after an incident.
            let mut behind: Option<Revision> = None;
            loop {
                ticker.tick().await;
                if let Err(e) = identity.sweep_expired_sessions(Timestamp::now()).await {
                    tracing::warn!(error = %e, "the session sweep could not run this round");
                }
                if let Err(e) = api.sweep_spent_consoles(Timestamp::now()).await {
                    tracing::warn!(error = %e, "the console sweep could not run this round");
                }
                if let Some(dir) = &api.inner.store_backup_dir {
                    match api.inner.store.snapshot(dir).await {
                        Ok(Some(wrote)) => {
                            tracing::info!(snapshot = %wrote.display(), "the store was snapshotted");
                            prune_snapshots(dir, KEEP_STORE_SNAPSHOTS).await;
                        }
                        Ok(None) => { /* a backend with nothing durable to copy */ }
                        Err(e) => {
                            tracing::warn!(error = %e, "the store could not be snapshotted this round");
                        }
                    }
                }
                match api.inner.store.revision().await {
                    Ok(now) => {
                        if let Some(keep) = behind.take()
                            && keep.0 > 1
                            && let Err(e) = api.inner.store.compact(keep).await
                        {
                            tracing::warn!(error = %e, "the store's history could not be compacted this round");
                        }
                        behind = Some(now);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read the store's revision, so nothing was compacted");
                    }
                }
            }
        });
    }

    /// The cache for a collection, if a filtered caller may be served from one.
    ///
    /// An unfiltered read is never served from here — see the field's own note.
    fn served(&self, kind: &str, filter: &Filter) -> Option<Served> {
        filter.assignee.as_ref()?;
        self.inner.served.read().unwrap().get(kind).cloned()
    }

    pub async fn list(&self, parent: &str, kind: &str) -> ApiResult<Listing> {
        self.list_filtered(parent, kind, &Filter::none()).await
    }

    /// A list as a caller may see it.
    ///
    /// **Filtered, not refused**, and the difference matters: a caller asking
    /// for their projects has no permission on the collection as a whole, and
    /// answering `403` would mean nobody could ever find the projects they do
    /// have. So a listing under no parent — or under a parent they may not
    /// read — comes back containing exactly the objects they may read, which
    /// for most callers is a short list and for none of them is an oracle.
    ///
    /// A list *under* a parent they may read is authorised once, on the parent,
    /// and then served whole: everything under a project is that project's.
    pub async fn list_for(
        &self,
        parent: &str,
        kind: &str,
        filter: &Filter,
        who: &Identity,
    ) -> ApiResult<Listing> {
        self.list_page_for(parent, kind, filter, &Paging::unpaged(), who)
            .await
    }

    /// [`Self::list_for`], a page at a time.
    ///
    /// The per-object permission check runs **inside** the page loop, and that
    /// is a fix rather than a refactor. It used to run over a finished listing,
    /// which meant every derived field of every object in the cell was computed
    /// and then thrown away for the ones the caller could not see — the whole
    /// cost of a cell-wide read to answer a tenant who owns three machines. It
    /// also meant a paged answer would come back short, or empty, while claiming
    /// to be a full page, because the trimming happened after the page was cut.
    pub async fn list_page_for(
        &self,
        parent: &str,
        kind: &str,
        filter: &Filter,
        paging: &Paging,
        who: &Identity,
    ) -> ApiResult<Listing> {
        if kind == velstra_cloud_model::resources::FAMILIES {
            return self.list_families(parent, who).await;
        }
        if !parent.is_empty() {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize_for(who, Verb::Read, &name, kind).await?;
            // Raw for a machine here too — a project-scoped list is the same
            // pass an agent builds its view from. See `Gate::Machine`.
            let gate = if crate::sessions::agent_node(who).is_some() {
                Gate::Machine
            } else {
                Gate::Everything
            };
            let mut listing = self.list_gated(parent, kind, filter, paging, &gate).await?;
            // The third door, after the read and the watch. A project list is
            // authorised at the parent and served whole — and it was serving
            // the machine names the other two doors had already stopped, which
            // is where the tenant's board actually got them from.
            for item in &mut listing.items {
                self.redact_for(who, kind, item);
            }
            return Ok(listing);
        }
        // No parent: a cell-wide collection. An operator sees it whole;
        // everybody else sees the objects they may read, one decision each.
        //
        // Except where no object will ever pass, in which case filtering states
        // an untruth: a customer asking for `/nodes` was answered "zero
        // machines" by a cell that has one. "You may not look" and "there is
        // nothing there" lead to different next steps, and only one of them is
        // true.
        // A node agent is not a customer, and the refusal below says something
        // false to it: "there may be plenty, and they are not yours" — some of
        // them are. A machine in the machine room reads the machine room, gated
        // per object like anybody else.
        //
        // Found on a live cell, in the node agent's own log, four times a second
        // for as long as the service ran: the agent looks itself up in `/nodes`,
        // and this refusal cut it off from its own object. The refusal was
        // written for the customer's seat and applied to every seat that is not
        // an operator's, which is one seat too many.
        let a_machine = crate::sessions::agent_node(who).is_some();
        let its_own_pass = a_machine && velstra_cloud_model::authz::a_node_reads_the_cells(kind);
        if !self.is_operator(who)
            && !its_own_pass
            && velstra_cloud_model::authz::belongs_to_the_cell(kind)
        {
            return Err(ApiError::new(
                Code::PermissionDenied,
                format!(
                    "{kind} are the cell's own, and reading them is a cell operator's. \
                     This is not an empty list: there may be plenty, and they are not yours."
                ),
            ));
        }
        let gate = if a_machine {
            Gate::Machine
        } else if self.is_operator(who) {
            Gate::Everything
        } else {
            Gate::Readable(who.clone())
        };
        self.list_gated(parent, kind, filter, paging, &gate).await
    }

    /// The catalogue, one entry per family rather than one per set of bytes.
    ///
    /// Derived and read-only: nothing stores a family. It exists because
    /// `families/debian-13` is the reference somebody is *supposed* to write —
    /// "the newest Debian 13", the one handle that stays right when the bytes
    /// change — and until now the only way to learn that the handle existed was
    /// to guess it and read the refusal, which helpfully lists them. A picker
    /// cannot offer what nothing can enumerate, so the console showed people
    /// `images/debian-13-d2af37c5` and let them pin themselves to one build.
    ///
    /// Scoped like resolution is, and for the same reason: a project's own
    /// `debian-13` shadows the cell's, so the caller is shown the entry they
    /// would actually get, not both.
    async fn list_families(&self, parent: &str, who: &Identity) -> ApiResult<Listing> {
        if !parent.is_empty() {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Read, &name).await?;
        }
        let mut images: Vec<velstra_cloud_model::resources::Image> = Vec::new();
        if !parent.is_empty() {
            images.extend(self.typed_list(parent, "images").await?);
        }
        // The cell's catalogue is everybody's — that is what publishing one
        // means — so it is read whole rather than gated per object.
        images.extend(self.typed_list("", "images").await?);

        let mut names: Vec<String> = images
            .iter()
            .filter(|i| !i.spec.family.is_empty() && i.meta.deleted_at.is_none())
            .map(|i| i.spec.family.clone())
            .collect();
        names.sort();
        names.dedup();

        let mut items = Vec::new();
        for family in names {
            // Whichever one a create would resolve to, by the same precedence.
            let mine: Vec<_> = if parent.is_empty() {
                Vec::new()
            } else {
                images
                    .iter()
                    .filter(|i| i.meta.name.to_string().starts_with(&format!("{parent}/")))
                    .collect()
            };
            let chosen = velstra_cloud_model::resources::newest_of_family(mine, &family)
                .or_else(|| {
                    let theirs: Vec<_> = images
                        .iter()
                        .filter(|i| !i.meta.name.to_string().starts_with("projects/"))
                        .collect();
                    velstra_cloud_model::resources::newest_of_family(theirs, &family)
                });
            let Some(image) = chosen else { continue };
            let private = image.meta.name.to_string().starts_with("projects/");
            items.push(serde_json::json!({
                "meta": {
                    "name": format!("{}/{family}", velstra_cloud_model::resources::FAMILIES),
                    "createdAt": image.meta.created_at,
                },
                "spec": {
                    "family": family,
                    "version": image.spec.version,
                    "image": image.meta.name.to_string(),
                    "sizeBytes": image.spec.size_bytes,
                    // What "public" means here is placement, which is the only
                    // thing that has ever decided it: an image under the cell is
                    // the catalogue's and everybody may boot it; one under a
                    // project is that project's alone. Said out loud because the
                    // rule was invisible, and an operator publishing a template
                    // had no way to check which of the two they had made.
                    "public": !private,
                },
                "status": { "conditions": [] },
            }));
        }
        // The revision the underlying images were read at: a family is a view
        // of them, so a watcher resuming from here resumes from the right place.
        let at = self.list_filtered("", "images", &Filter::none()).await?.revision;
        Ok(Listing {
            items,
            revision: at,
            next_page_token: None,
        })
    }

    /// The same, for a caller that must not be handed the whole cell.
    pub async fn list_filtered(
        &self,
        parent: &str,
        kind: &str,
        filter: &Filter,
    ) -> ApiResult<Listing> {
        self.list_page(parent, kind, filter, &Paging::unpaged())
            .await
    }

    /// A filtered list, a page at a time.
    ///
    /// **The page size is a promise about the answer, not about the read**, and
    /// that is the whole subtlety here. The store hands back objects in key
    /// order; the parent scope and the filter then reject some of them. Ask for
    /// twenty instances on a node holding three, and a single store page of
    /// twenty yields three — so this keeps reading until it has a full page or
    /// the collection runs out. A page size that quietly meant "up to twenty,
    /// often far fewer" would make every caller write the same retry loop, and
    /// most of them would write it wrong: the natural mistake is to stop on a
    /// short page, which stops the walk in the middle.
    ///
    /// The cost stays bounded because the rejects are cheap — `under` and
    /// `admits` read two fields — while the expensive part, `answer`, runs only
    /// on objects that survived. That ordering is load-bearing and predates
    /// paging; paging only made it matter more.
    pub async fn list_page(
        &self,
        parent: &str,
        kind: &str,
        filter: &Filter,
        paging: &Paging,
    ) -> ApiResult<Listing> {
        self.list_gated(parent, kind, filter, paging, &Gate::Everything)
            .await
    }

    async fn list_gated(
        &self,
        parent: &str,
        kind: &str,
        filter: &Filter,
        paging: &Paging,
        gate: &Gate,
    ) -> ApiResult<Listing> {
        if let Some(token) = &paging.token {
            token.check(kind, parent)?;
        }
        let collection = self.collection(kind)?;
        let unpaged = !paging.is_paged();
        let want = paging.resolved_size();

        let mut items = Vec::new();
        // One scratch for the whole listing, not one per item: that is the whole
        // difference between a list that costs the cell once and one that costs
        // it once per object.
        let mut scratch = Scratch::default();
        let mut after: Option<String> = paging.token.as_ref().map(|t| t.after.clone());
        let mut revision = None;
        let mut more;

        loop {
            let (documents, has_more, at) = match self.served(kind, filter) {
                // From memory, and no read of the store at all. This is the line
                // that decides how many nodes a cell can hold.
                Some(cache) => {
                    let (held, at) = cache.all().await;
                    let mut documents: Vec<Value> =
                        held.iter().map(|d| (**d).clone()).collect::<Vec<_>>();
                    // The cache holds a whole collection, so its page is a slice.
                    // That still removes the expensive half — the computed fields
                    // and the serialising — and the cheap half is a memory scan
                    // the cache exists to make cheap.
                    if let Some(after) = &after {
                        documents
                            .retain(|d| name_of(d).is_some_and(|n| n.as_str() > after.as_str()));
                    }
                    let has_more = !unpaged && documents.len() > want;
                    if !unpaged {
                        documents.truncate(want);
                    }
                    (documents, has_more, at)
                }
                None if unpaged => (
                    collection.list().await?,
                    false,
                    collection.revision().await?,
                ),
                None => {
                    let at = collection.revision().await?;
                    let (documents, has_more) =
                        collection.list_page(after.as_deref(), want).await?;
                    (documents, has_more, at)
                }
            };
            // The revision of the first page of this walk, and of no other — a
            // resumed walk reports the one its token carries.
            revision.get_or_insert(
                paging
                    .token
                    .as_ref()
                    .map(|t| Revision(t.revision))
                    .unwrap_or(at),
            );
            more = has_more;

            let last = documents.last().and_then(name_of);
            for mut document in documents {
                if !under(&document, parent) {
                    continue;
                }
                // Before the computed fields, not after: those are the expensive
                // part, and paying for them on an object nobody asked for is the
                // cost this exists to avoid.
                if !filter.admits(kind, &document) {
                    continue;
                }
                // Labels next, and before the computed fields for the same
                // reason: an object nobody asked for should cost nothing.
                if !filter.labels_admit(&document) {
                    continue;
                }
                // "What has happened to this guest": a record about something
                // else costs nothing beyond this line.
                if !filter.target_admits(&document) {
                    continue;
                }
                // Before `answer`, for the same reason `admits` is: the derived
                // fields are the expensive part, and an object the caller may
                // not see is an object nobody should pay for.
                if let Gate::Readable(who) = gate {
                    let Some(name) = name_of(&document).and_then(|n| ResourceName::parse(&n).ok())
                    else {
                        continue;
                    };
                    // `judge`, not `authorize`: filtering a list is not
                    // refusing a request, which is what this function's own
                    // doc says two screens up — and `authorize` records a
                    // refusal in the audit. One tenant listing a cell of four
                    // hundred guests would have written four hundred audit
                    // records, one per object they were never asking for by
                    // name. The audit is for somebody who reached for a thing
                    // and was told no.
                    if !self.may_read(who, &name, &document).await {
                        continue;
                    }
                }
                if !matches!(gate, Gate::Machine) {
                    self.answer(&mut document, &mut scratch).await?;
                }
                // The same trimming a single read gets. A list gated on the
                // reader is a tenant's list, and their board is where the
                // machine names were actually on screen.
                if let Gate::Readable(who) = gate {
                    self.redact_for(who, kind, &mut document);
                }
                items.push(document);
            }

            if unpaged || items.len() >= want || !more {
                after = last.or(after);
                break;
            }
            // A short page after filtering: read on rather than hand back a page
            // the caller would mistake for the end.
            let Some(last) = last else { break };
            after = Some(last);
        }

        // More was read than asked for only if the loop was told to stop early;
        // trim so the page size is honoured exactly and the token points at what
        // was actually delivered.
        if !unpaged && items.len() > want {
            items.truncate(want);
            more = true;
            after = items.last().and_then(name_of);
        }

        let next_page_token = (!unpaged && more)
            .then(|| {
                after.as_ref().map(|after| {
                    PageToken {
                        kind: kind.to_string(),
                        parent: parent.to_string(),
                        after: after.clone(),
                        revision: revision.unwrap_or(Revision(0)).0,
                    }
                    .encode()
                })
            })
            .flatten();

        Ok(Listing {
            items,
            revision: revision.unwrap_or(Revision(0)),
            next_page_token,
        })
    }

    // ---- writes -----------------------------------------------------------

    /// Create, and mint the operation that describes the wait.
    ///
    /// `body` is a model-shaped document: `spec`, optionally `meta.labels`, and
    /// an `id`. It may not carry `status` — that half belongs to the agent that
    /// will own the object, and a client that sets it is describing a world it
    /// has not observed.
    pub async fn create(
        &self,
        parent: &str,
        kind: &str,
        body: &Value,
        who: &Identity,
    ) -> ApiResult<Created> {
        // Before the authorisation, deliberately: cutting the work is the whole
        // point of a limiter, and the cost of the other order is that a script
        // in a loop pays for a permission check on every pass. The trade is
        // that somebody who would have been refused anyway is told they are
        // going too fast first — which is true, and which they discover the
        // moment they slow down.
        self.may_write_now(who)?;
        // Authorised on the **parent**, because the object does not exist yet
        // and has no bindings of its own. Creating inside a project is a write
        // to that project; creating without one is a write to the cell.
        let home = if parent.is_empty() {
            if !self.is_operator(who) {
                return Err(ApiError::forbidden(
                    "creating a project, a node or a pool is a change to the cell; only a cell \
                     operator may make one",
                ));
            }
            None
        } else {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Write, &name).await?;
            governing_project(&name)
        };
        if kind == "operations" {
            return Err(ApiError::invalid(
                "operations are minted by the API when it accepts a change, never created directly",
            ));
        }
        if kind == "usage" {
            return Err(ApiError::invalid(RECORDS_ARE_NOT_WRITTEN_HERE).at("spec"));
        }
        let collection = self.collection(kind)?;
        refuse_unwritable(body)?;

        let name = self.name_for(parent, kind, body)?;
        let mut spec = collection.empty_spec();
        if let Some(patch) = body.get("spec") {
            merge(&mut spec, patch);
        }
        // Before anything reads the spec as its real type — quota counts vCPUs
        // and gibibytes — so that a field of the wrong shape is reported by the
        // one check that knows which field it was.
        collection.check_spec(&spec)?;
        if let Some(sent) = body.get("spec") {
            collection.check_known(sent)?;
        }
        // Before the references are judged, because `families/debian-13` is not
        // a resource anybody could be authorised on — it is a request that the
        // platform pick one. Judged raw, it parsed as a two-segment name of a
        // collection no project governs, and the whole create answered "this is
        // a cell-wide resource; only a cell operator may touch it" — to a
        // customer doing exactly what the catalogue is for. The *resolved*
        // image then goes through the ordinary reference check below, which is
        // the authorisation that means something: may this caller boot that.
        if kind == "instances" {
            self.settle_image_family(parent, &mut spec).await?;
        }
        // Before the shape check, not after: the bare spelling is a *spelling*
        // and not a malformed name, and `refs::check` cannot know that without
        // being taught about this one field. One place decides what a parent is.
        if kind == "folders" || kind == "projects" {
            settle_parent(&mut spec);
        }
        crate::refs::check(kind, &spec)?;
        // Before anything follows one of those references — `settle_volume_source`
        // reads the snapshot, `settle_migration` reads the instance — so that a
        // caller who may not read the thing they named is refused for that
        // reason and learns nothing about whether it is there.
        self.authorize_references(who, kind, &spec, home.as_deref())
            .await?;
        check_rules(kind, &spec)?;
        // What this project was given, as against what this caller may do. Two
        // different questions, both asked: a project admin may create a network,
        // and only the cell decides whether one of this project's networks may
        // sit on a machine's own wire.
        let allowed_in = home.as_deref();
        if kind == "networks" {
            self.refuse_a_bridge_this_project_was_not_given(&spec, allowed_in, who)
                .await?;
        }
        if kind == "instances" {
            self.refuse_a_device_this_project_was_not_given(&spec, allowed_in, who)
                .await?;
            self.refuse_a_port_two_guests_would_share(&name, &spec)
                .await?;
        }
        if kind == "floatingips" {
            self.refuse_a_public_address_this_project_was_not_given(allowed_in, who)
                .await?;
            self.settle_floating_ip(parent, &mut spec).await?;
        }
        if kind == "volumes" || kind == "backups" {
            self.refuse_a_pool_this_cell_does_not_have(&spec).await?;
        }
        if matches!(kind, "instances" | "attachments")
            && spec.get("node").and_then(Value::as_str).is_some_and(|n| !n.is_empty())
            && !self.is_operator(who)
        {
            // The same rule the patch enforces, at birth: see there.
            return Err(ApiError::forbidden(
                "which machine runs a guest is the cell's decision — a tenant does not see \
                 hosts and cannot pin to one",
            )
            .at("spec.node"));
        }
        if kind == "attachments" {
            self.settle_node(&mut spec, None).await?;
        }
        if kind == "migrations" {
            // Moving a guest between machines is running the *cell*, not the
            // project: the object names two hosts, and its whole point is which
            // hardware runs what. A tenant has no hosts to choose between and no
            // way to see them — a migration they created would be an ask about
            // machines they cannot name. Refused here even though the object
            // lives under the project, because that is where the record of the
            // move belongs, not where the decision does.
            if !self.is_operator(who) {
                return Err(ApiError::forbidden(
                    "moving a guest between machines is a cell operator's decision — a \
                     migration names hosts, and hosts are the cell's. The guest itself is \
                     unaffected by who moves it.",
                ));
            }
            self.settle_migration(&mut spec).await?;
        }
        if kind == "snapshots" {
            self.settle_snapshot(&name, &mut spec).await?;
        }
        if kind == "backups" {
            self.settle_backup(&mut spec).await?;
        }
        if kind == "captures" {
            self.settle_capture(&mut spec).await?;
        }
        if kind == "networks" {
            self.settle_network(&mut spec).await?;
        }
        if kind == "images" {
            self.settle_published_image(&mut spec).await?;
        }
        if kind == "folders" || kind == "projects" {
            self.refuse_a_parent_that_cannot_be_one(kind, &name, &spec)
                .await?;
        }
        self.refuse_a_role_nobody_defined(&spec).await?;
        if kind == "instances" {
            self.settle_default_network(&name, parent, body.get("spec"), &mut spec)
                .await?;
        }
        if kind == "volumes" {
            self.settle_volume_source(&name, &mut spec).await?;
            // After the source, on purpose: a clone inherits the pool holding
            // its snapshot, and a choice made before that would put the copy in
            // a different pool from the bytes it is cloned from. Only a volume
            // that still names none gets one chosen.
            self.settle_volume_pool(&mut spec).await?;
            self.refuse_a_pool_this_cell_does_not_have(&spec).await?;
        }
        if kind == "ceph-clusters" {
            self.refuse_a_second_ceph_cluster(&name).await?;
            self.refuse_a_disk_that_is_not_free(&spec).await?;
        }
        if kind == "images" {
            refuse_an_unverified_signature(&spec)?;
        }
        if kind == "image-sources" {
            refuse_an_unusable_image_source(&spec)?;
        }

        if kind == "nodes" {
            refuse_an_unusable_overcommit(&spec)?;
        }
        if kind == "floatingips" {
            self.refuse_an_address_that_reaches_nothing(&name, &spec)
                .await?;
        }
        if kind == "networks" {
            refuse_an_external_network_from_a_tenant(&spec, who, self.is_operator(who))?;
        }
        if kind == "maintenance-windows" {
            self.refuse_a_window_that_would_do_nothing(&name, &spec)
                .await?;
        }
        self.check_cell(&name, kind).await?;
        self.check_quota(&name, kind, &spec).await?;

        let mut meta = Meta::new(name.clone(), self.inner.placement.clone());
        if let Some(labels) = body.get("meta").and_then(|m| m.get("labels")) {
            meta.labels = serde_json::from_value(labels.clone()).map_err(|e| {
                ApiError::invalid(format!("labels are a flat map of strings: {e}"))
                    .at("meta.labels")
            })?;
        }
        let meta = serde_json::to_value(&meta).expect("meta always serialises");
        let created = collection
            .create(meta, spec)
            .await
            .map_err(|e| taken(e, kind, &name))?;

        let generation = created["meta"]["generation"].as_u64().unwrap_or(1);
        let operation = self
            .mint_operation(&name, generation, "create", who)
            .await?;
        // Registering a node mints its per-node agent token, returned once. The
        // node object exists first, so a mint that fails leaves a node an
        // operator can see and re-issue a token for rather than a half-registered
        // ghost — and only a cell operator reaches this path (a node is a
        // cell-wide resource), so nobody but an operator ever receives a token.
        let node_token = if kind == "nodes" {
            Some(self.inner.identity.mint_node_credential(name.id()).await?)
        } else {
            None
        };
        // A pool gets one for the same reason and on the same terms. Without it
        // a pool could only ever run on the control plane's own machine: its
        // agent wrote straight to the store, and a machine with no etcd has no
        // store to write to. The `pool + hypervisor` answer the setup wizard
        // offers produced a unit that could not start.
        let pool_token = if kind == "pools" {
            Some(self.inner.identity.mint_pool_credential(name.id()).await?)
        } else {
            None
        };
        Ok(Created {
            operation,
            target: name.to_string(),
            node_token,
            pool_token,
        })
    }

    /// Issue a fresh agent credential for a machine that already exists.
    ///
    /// A credential is minted once, at registration, and only its digest is
    /// kept — which is the right shape for a secret and the wrong shape for the
    /// only way to get one. A machine that lost its token, or a pool that was
    /// registered before pools had credentials at all, had exactly one way back:
    /// delete the object and make it again. For a pool that means deleting
    /// something every volume in it is written against.
    ///
    /// Found on two real machines. The second one's pool agent could not start,
    /// and nothing short of destroying the pool would have let it.
    ///
    /// Issuing does not revoke: the old digest still opens the door until it is
    /// deleted. That is deliberate — an operator who mis-types the new token
    /// into a file would otherwise take the agent down while fixing it — and it
    /// is why this is `issueCredential` rather than `rotateCredential`, which
    /// would be a name promising the other thing.
    pub async fn issue_credential(
        &self,
        name: &ResourceName,
        who: &Identity,
    ) -> ApiResult<Value> {
        let kind = name.collection();
        if kind != "nodes" && kind != "pools" {
            return Err(ApiError::invalid(format!(
                "only a node or a pool has an agent credential, and {name} is a {kind}"
            )));
        }
        // `Write`, the verb that brings machines into existence, not `Operate`.
        // Somebody who may run the estate may not hand out the credential a
        // machine speaks with — a token given to whoever can start a guest is a
        // token given to most of the cell.
        self.authorize_for(who, Verb::Write, name, kind).await?;
        // It has to exist. Minting against a name nobody registered would write
        // a credential for a machine the cell has never heard of, and it would
        // authenticate.
        self.get(name, who).await?;
        let mut body = Map::new();
        body.insert("target".into(), Value::String(name.to_string()));
        let (field, token) = if kind == "nodes" {
            ("nodeToken", self.inner.identity.mint_node_credential(name.id()).await?)
        } else {
            ("poolToken", self.inner.identity.mint_pool_credential(name.id()).await?)
        };
        body.insert(field.into(), Value::String(token));
        // No `operation`: nothing converges here. A create answers with one
        // because the object it made has not settled yet; a credential is
        // finished the moment it is in the answer, and a field naming an
        // operation nobody will ever finish is a field a client waits on.
        Ok(Value::Object(body))
    }

    /// Change a `spec`, and nothing else a client does not own.
    pub async fn patch(
        &self,
        name: &ResourceName,
        body: &Value,
        expect: Option<Revision>,
        who: &Identity,
    ) -> ApiResult<Value> {
        self.may_write_now(who)?;
        // Changing who else may is a different permission from changing
        // anything else, or an editor is an admin one request later.
        let verb = if body
            .get("spec")
            .and_then(|spec| spec.get("bindings"))
            .is_some()
        {
            Verb::Administer
        } else {
            // A change to something that already exists. Creating and deleting
            // are `Write`; this is the rung below, so somebody who keeps an
            // estate running can start, stop and resize without also being able
            // to take a machine away.
            Verb::Operate
        };
        self.authorize(who, verb, name).await?;
        // A project's quota is the cell operator's to set, and a project admin
        // may not raise their own. It lives in `spec.quota` only because a
        // project is where a quota applies — changing it is a change to the cell,
        // not to the project, and a tenant who could lift their own limit would
        // make the limit advice rather than a bound. Checked after `authorize`,
        // so a caller who may not touch the project at all is refused in the same
        // words as for any other field and learns nothing extra.
        if body.get("spec").and_then(|s| s.get("quota")).is_some() && !self.is_operator(who) {
            return Err(ApiError::forbidden(
                "a project's quota is set by a cell operator; a project admin may not change it",
            ));
        }
        // The same rule about a stronger thing. Without it every check that
        // consults the policy is decorative: a project admin who could write
        // `policy.hostBridges` could grant themselves the machine's own wire,
        // and one who could write `policy.floatingIps` could grant themselves
        // the cell's address space — by editing the object that says they may
        // not.
        if body.get("spec").and_then(|s| s.get("policy")).is_some() && !self.is_operator(who) {
            return Err(ApiError::forbidden(
                "what a project may reach for is set by a cell operator; a project admin works \
                 within it and may not widen it. Host bridges, hardware passthrough and public \
                 addresses are the cell's to grant.",
            )
            .at("spec.policy"));
        }
        if name.collection() == "usage" {
            return Err(ApiError::invalid(RECORDS_ARE_NOT_WRITTEN_HERE));
        }
        let collection = self.collection(name.collection())?;
        refuse_unwritable(body)?;
        let mut patch = Patch {
            spec: body.get("spec").cloned(),
            labels: body.get("meta").and_then(|m| m.get("labels")).cloned(),
        };
        if let Some(spec) = &mut patch.spec {
            if name.collection() == "folders" || name.collection() == "projects" {
                settle_parent(spec);
            }
            crate::refs::check(name.collection(), spec)?;
            self.authorize_references(
                who,
                name.collection(),
                spec,
                governing_project(name).as_deref(),
            )
            .await?;
            // Only when this change carries it. A patch carries what it changes,
            // so a project stored before folders existed — with the
            // `organizations/o1` the field's own documentation used to promise —
            // stays editable in every other respect. Moving it is a decision;
            // renaming it is not the moment to make somebody make one.
            if (name.collection() == "folders" || name.collection() == "projects")
                && spec.get("parent").is_some()
            {
                self.refuse_a_parent_that_cannot_be_one(name.collection(), name, spec)
                    .await?;
            }
            self.refuse_a_role_nobody_defined(spec).await?;
            check_rules(name.collection(), spec)?;
            if name.collection() == "volumes" {
                self.refuse_a_new_source(name, spec).await?;
                self.refuse_a_moved_pool(name, spec).await?;
            }
            if name.collection() == "ceph-clusters" {
                self.refuse_a_disk_that_is_not_free(spec).await?;
            }
            if name.collection() == "images" {
                refuse_an_unverified_signature(spec)?;
            }
            if name.collection() == "instances" && spec.get("root_disk_gib").is_some() {
                self.refuse_a_smaller_disk(name, spec, who).await?;
            }
            // Pinning a guest to a machine names a host, and hosts are not part
            // of a project's view — a tenant cannot list them and does not see
            // where their guest runs. A pin they wrote would be a name they
            // guessed, honoured silently by the scheduler.
            if matches!(name.collection(), "instances" | "attachments")
                && spec.get("node").is_some()
                && !self.is_operator(who)
            {
                return Err(ApiError::forbidden(
                    "which machine runs a guest is the cell's decision — a tenant does not \
                     see hosts and cannot pin to one",
                )
                .at("spec.node"));
            }
            // The same rule on the way in as on creation: a port handed to a
            // second guest by an edit is the same silent failure as one handed
            // to it at birth.
            if name.collection() == "instances" && spec.get("ports").is_some() {
                self.refuse_a_port_two_guests_would_share(name, spec)
                    .await?;
            }
            // `cpu_baseline`, not `cpuBaseline`: the body was converted out of
            // its wire spelling before it got here.
            if name.collection() == "floatingips" {
                let stored: Value = self.get(name, who).await?;
                let mut merged = stored["spec"].clone();
                merge(&mut merged, spec);
                self.refuse_an_address_that_reaches_nothing(name, &merged)
                    .await?;
            }
            // Every network patch, not only one that touches `external`. The
            // narrower condition was a gap the moment a second operator-only
            // field arrived: a patch setting `host_bridge` alone walked past the
            // check that exists to stop exactly that. A rule that names the
            // fields it guards has to be edited every time one is added, and the
            // edit is the part that gets forgotten.
            if name.collection() == "networks" {
                refuse_an_external_network_from_a_tenant(spec, who, self.is_operator(who))?;
                self.refuse_a_bridge_this_project_was_not_given(
                    spec,
                    governing_project(name).as_deref(),
                    who,
                )
                .await?;
            }
            if name.collection() == "maintenance-windows" {
                // Merged onto what is stored, because a change that moves only
                // the start time must still be judged as the whole window it
                // leaves behind.
                let stored: Value = self.get(name, who).await?;
                let mut merged = stored["spec"].clone();
                merge(&mut merged, spec);
                self.refuse_a_window_that_would_do_nothing(name, &merged)
                    .await?;
            }
            // Before anything reads the change: a field nobody has is a change
            // that will not happen, and answering `200` to one is agreeing to
            // something that will not be done.
            collection.check_known(spec)?;
            if name.collection() == "nodes" && spec.get("vcpu_overcommit").is_some() {
                refuse_an_unusable_overcommit(spec)?;
            }
            if name.collection() == "nodes" && spec.get("cpu_baseline").is_some() {
                self.refuse_a_baseline_this_node_cannot_present(name, spec, who)
                    .await?;
            }
            // A change may move an attachment's node — after a migration, to
            // agree with the instance again — but never away from it.
            if name.collection() == "attachments" && spec.get("node").is_some() {
                let stored: Value = self.get(name, who).await?;
                let instance = stored["spec"]["instance"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.settle_node(spec, Some(&instance)).await?;
            }
        }
        if patch.is_empty() {
            return Err(ApiError::invalid(
                "a change has to carry a spec, or labels; there is nothing else a client may write",
            )
            .at("spec"));
        }
        let mut document = collection.patch(&name.to_string(), &patch, expect).await?;
        self.answer(&mut document, &mut Scratch::default()).await?;
        Ok(document)
    }

    /// Report the status of an object, as the node agent that owns it.
    ///
    /// This is the write half of `--api` mode, and the reason a node token is a
    /// trust boundary rather than a shared operator key. Only a caller that
    /// authenticated with a per-node token may reach it, and only for an object
    /// that token's node owns or was assigned — but neither of those checks lives
    /// here. The caller being an *agent at all* is the gate this function keeps;
    /// **which** objects it may write is [`velstra_cloud_model::access::judge`]'s,
    /// applied inside `store.update` exactly as it is for a direct-store report,
    /// so there is one answer to "may this node write this status" and it is not
    /// re-derived on the API path.
    ///
    /// A caller who is not a node agent — a person, an operator, a service
    /// account — has no agent scope and is refused here, because status is not a
    /// thing any of them writes: it is the agent's half, and the API does not get
    /// to be more permissive than the store.
    pub async fn report_status(
        &self,
        name: &ResourceName,
        body: &Value,
        expect: Option<Revision>,
        who: &Identity,
    ) -> ApiResult<Value> {
        let Some(node) = crate::sessions::agent_node(who) else {
            return Err(ApiError::forbidden(
                "status is written by the node agent that owns the object; this token is not a \
                 node agent",
            ));
        };
        let Some(status) = body.get("status") else {
            return Err(ApiError::invalid("a status report carries a status").at("status"));
        };
        let collection = self.collection(name.collection())?;
        let writer = velstra_cloud_model::Writer::agent(node);
        let document = collection
            .report_status(&name.to_string(), status, expect, &writer)
            .await?;
        // Raw, not decorated: the caller here is an agent by definition, and
        // the computed answers are presentation for people — see `Gate::Machine`.
        Ok(document)
    }

    /// Ask for a deletion. Two-phase and visible: the object stays readable,
    /// carrying its `deletedAt` and its finalizers, until the last holder lets
    /// go.
    pub async fn delete(
        &self,
        name: &ResourceName,
        expect: Option<Revision>,
        who: &Identity,
    ) -> ApiResult<Deleted> {
        self.may_write_now(who)?;
        if name.collection() == "usage" {
            return Err(ApiError::invalid(RECORDS_ARE_NOT_WRITTEN_HERE));
        }
        self.authorize(who, Verb::Write, name).await?;
        // Asked before anything is written down. Deleting an object something
        // still names does not fail loudly anywhere: the reference simply stops
        // resolving, and whoever finds out is an agent that cannot program a
        // port, or a guest with no image, at a moment nobody connects to this
        // request.
        let held_by = self.holders(name).await?;
        if !held_by.is_empty() {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "{name} is still named by {}{}. Delete those first, or change what they \
                     point at.",
                    held_by
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    if held_by.len() > 5 {
                        format!(" and {} more", held_by.len() - 5)
                    } else {
                        String::new()
                    }
                ),
            ));
        }
        let collection = self.collection(name.collection())?;
        let deleted = collection.delete(&name.to_string(), expect).await?;
        // A deleted user's password and live sessions go with them. Leaving
        // either behind would mean the account is gone from every listing and
        // still opens the door: the credential outlives its user, and a token
        // issued before the deletion keeps authenticating until it expires.
        //
        // After the delete, not before: an object that could not be deleted —
        // because something still names it, or because the revision moved — must
        // not have had its credential destroyed on the way to finding out.
        if name.collection() == "users" {
            if let Err(e) = self.inner.identity.forget(name.id()).await {
                // Loud, and not fatal. The user is gone either way; what is left
                // is a credential nobody can reach through a route that exists,
                // and a person has to know to clean it up.
                tracing::error!(
                    user = name.id(),
                    "deleted the user but could not remove their credential or sessions: {e}"
                );
            }
        }
        // A deleted node's agent token goes with it, for the same reason a
        // deleted user's does: a token that outlived the node it speaks for is a
        // credential for an object that no longer exists, and the only honest
        // lifetime for it is the node's.
        if name.collection() == "nodes" {
            if let Err(e) = self.inner.identity.forget_node(name.id()).await {
                tracing::error!(
                    node = name.id(),
                    "deleted the node but could not remove its agent credential: {e}"
                );
            }
        }
        Ok(deleted)
    }

    /// Everything that still names `target`.
    ///
    /// Read from the objects rather than from a reference count, for the reason
    /// every other total in this system is counted rather than tracked: a count
    /// that is incremented and decremented is wrong the first time either half
    /// is missed, and it fails in the direction that refuses a delete nobody can
    /// explain.
    ///
    /// A scan per delete, which is affordable precisely because a delete is rare
    /// and somebody is waiting for the answer — unlike a reconcile, which
    /// happens constantly and where the same shape was worth removing.
    async fn holders(&self, target: &ResourceName) -> ApiResult<Vec<String>> {
        let wanted = target.to_string();
        let mut held_by = Vec::new();

        // A project holds everything under it. Nothing names a project as a
        // *reference*, so this is its own question — and the answer somebody
        // needs is "there are still machines in it", not "no such field".
        if target.collection() == "projects" {
            let prefix = format!("{wanted}/");
            for kind in COLLECTIONS {
                // A record is not a thing anybody holds. Counting operations
                // made a project undeletable the moment it was created, because
                // creating it produced one; counting usage readings made every
                // project undeletable an hour after that, and *that* one had no
                // way out at all — a usage record cannot be deleted through this
                // API on purpose, so the advice below ("delete those first") was
                // impossible to follow. Records outlive the project and go away
                // with their own retention, not with this.
                if kind == "projects" || velstra_cloud_model::reconcile::is_a_record(kind) {
                    continue;
                }
                for document in self.collection(kind)?.list().await? {
                    if let Some(name) = joined(&document["meta"]["name"])
                        && name.starts_with(&prefix)
                    {
                        held_by.push(name);
                    }
                }
            }
            return Ok(held_by);
        }

        for kind in crate::refs::REFERRING_KINDS {
            for document in self.collection(kind)?.list().await? {
                let Some(name) = joined(&document["meta"]["name"]) else {
                    continue;
                };
                // An object does not hold itself, which matters for `projects`,
                // whose `parent` is a reference field.
                if name == wanted {
                    continue;
                }
                // An object the platform made *for* this one is not a reason to
                // keep it: deleting a guest is a deletion of its own disks'
                // attachments too, and the disk controller removes them.
                //
                // Without this, a machine with a disk could never be deleted at
                // all. `spec.volumes` exists so that nobody has to know
                // attachments are a thing — and the answer to
                //
                //   "…is still named by projects/p1/attachments/db-data.
                //    Delete those first, or change what they point at."
                //
                // is a step the customer was deliberately spared at the other
                // end. Found live, on the first machine created with a disk that
                // somebody then tried to delete.
                if document["meta"]["labels"]
                    [velstra_cloud_model::resources::MINTED_FOR]
                    .as_str()
                    == Some(wanted.as_str())
                {
                    continue;
                }
                let spec = document.get("spec").cloned().unwrap_or(Value::Null);
                if crate::refs::names_referenced(kind, &spec).contains(&wanted) {
                    held_by.push(name);
                }
            }
        }
        Ok(held_by)
    }

    // ---- watching ---------------------------------------------------------

    /// Changes to a collection, from a revision the caller chooses.
    ///
    /// Nothing here buffers. The store hands over a bounded channel and drops a
    /// watcher that stops reading; this stream is polled by whatever is writing
    /// to the client, so a client that stalls stops being read from, and the
    /// store lets it go rather than growing a queue in the one process that
    /// holds all the state. Falling behind costs a re-list, which every client
    /// of this API can do.
    pub fn watch(
        &self,
        parent: &str,
        kind: &str,
        from: Option<Revision>,
    ) -> ApiResult<impl Stream<Item = WatchEvent> + Send + use<>> {
        self.watch_filtered(parent, kind, from, Filter::none())
    }

    /// A watch as a *caller* may see it — the streaming half of
    /// [`Self::list_page_for`], and the same rule applied event by event.
    ///
    /// It did not exist, and neither transport asked for it: REST read the
    /// bearer token, gRPC called `self.who()`, and both then handed
    /// `watch_filtered` a parent and a kind with no identity attached. So
    /// `GET /api/v1/projects/{someone-else}/instances?watch=true` with any
    /// accepted token streamed another tenant's objects, and the same request
    /// with no parent streamed the whole cell. Every other entry point on this
    /// type authorises at the top; the watch was the one that never did.
    ///
    /// It survived because `tests/authz.rs` asks about `get`, `create`, `patch`,
    /// `delete` and `list`, and a watch is the one read whose answer arrives
    /// after the request that asked for it — so no test that only checks a
    /// return value would have noticed.
    ///
    /// The rule is not a new one: it is `list_page_for`'s, unchanged. A watch
    /// **under a parent** is authorised once, on the parent, because everything
    /// under a project is that project's. A **cell-wide** watch is gated per
    /// object, which is what a list under no parent already does — an operator
    /// sees the cell, and everybody else sees the objects they may read rather
    /// than a `403` that would leave them unable to find their own.
    pub async fn watch_for(
        &self,
        parent: &str,
        kind: &str,
        from: Option<Revision>,
        filter: Filter,
        who: &Identity,
    ) -> ApiResult<impl Stream<Item = WatchEvent> + Send + use<>> {
        let a_machine = crate::sessions::agent_node(who).is_some();
        let gate = if parent.is_empty() {
            // A cell-wide stream. An operator is asking about the cell on
            // purpose; anybody else is told about what they may read.
            if a_machine {
                Gate::Machine
            } else if self.is_operator(who) {
                Gate::Everything
            } else {
                Gate::Readable(who.clone())
            }
        } else {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Read, &name).await?;
            if a_machine { Gate::Machine } else { Gate::Everything }
        };
        let stream = self.watch_gated(parent, kind, from, filter, gate)?;
        // The same trimming a read gets, or the watch is the hole: the list
        // arrives redacted and the first event puts the machine name back on
        // the tenant's screen — which is exactly where it was seen.
        let api = self.clone();
        let who = who.clone();
        let kind = kind.to_string();
        Ok(stream.map(move |event| match event {
            WatchEvent::Put(mut document) => {
                api.redact_for(&who, &kind, &mut document);
                WatchEvent::Put(document)
            }
            delete => delete,
        }))
    }

    /// The same, for a caller that must not be sent every event in the cell.
    ///
    /// A `Put` that no longer passes the filter is dropped rather than turned
    /// into a synthetic delete. That is safe here and would not be everywhere:
    /// this platform's agents use a watch as a **wake-up** and re-read what they
    /// own on every pass, so an object that left a node disappears from that
    /// node's next list. An agent that treated the stream as its state would
    /// need the delete.
    ///
    /// **Unauthorised**, and only for callers inside the platform — a
    /// controller, a test, the cache. Anything reachable from the network wants
    /// [`Self::watch_for`].
    pub fn watch_filtered(
        &self,
        parent: &str,
        kind: &str,
        from: Option<Revision>,
        filter: Filter,
    ) -> ApiResult<impl Stream<Item = WatchEvent> + Send + use<>> {
        self.watch_gated(parent, kind, from, filter, Gate::Everything)
    }

    fn watch_gated(
        &self,
        parent: &str,
        kind: &str,
        from: Option<Revision>,
        filter: Filter,
        gate: Gate,
    ) -> ApiResult<impl Stream<Item = WatchEvent> + Send + use<>> {
        let collection = self.collection(kind)?;
        // A filtered watcher is a node agent, and there may be a thousand of
        // them. One store watch feeds all of them; without this the store has
        // one watcher per agent and every write is delivered once per agent.
        //
        // `from` is dropped when the cache answers, and that is the honest
        // consequence of not keeping history in memory: a subscriber is told
        // what happens *next*. It is safe here because the revision a cached
        // list reported is the revision that list was current as of, and an
        // agent lists before it watches.
        let receiver = match self.served(kind, &filter) {
            Some(cache) => cache.subscribe(),
            None => collection.watch(from),
        };
        let api = self.clone();
        let parent = parent.to_string();
        let kind = kind.to_string();
        Ok(
            tokio_stream::wrappers::ReceiverStream::new(receiver).filter_map(move |event| {
                let api = api.clone();
                let collection = collection.clone();
                let parent = parent.clone();
                let kind = kind.clone();
                let filter = filter.clone();
                let gate = gate.clone();
                async move {
                    api.event(&collection, &parent, &kind, &filter, &gate, event)
                        .await
                }
            }),
        )
    }

    /// One store event, as this subscriber should see it — or nothing.
    ///
    /// The gate is `Readable` only for a cell-wide watch by somebody who is not
    /// an operator, and then it costs one permission check per event. That is the
    /// same price `list_gated` pays per object and for the same reason: the
    /// alternative is a stream that hands the cell to whoever asks.
    ///
    /// A **delete** is gated too. It carries a name and no document, which is
    /// all the check needs — and letting it through unchecked would tell one
    /// tenant the names of another's objects at the moment they are removed,
    /// which is the same oracle the refusals are worded to avoid.
    async fn event(
        &self,
        collection: &Arc<dyn Collection>,
        parent: &str,
        kind: &str,
        filter: &Filter,
        gate: &Gate,
        event: Event,
    ) -> Option<WatchEvent> {
        match event {
            Event::Put(entry) => {
                let mut document = collection.decode(&entry.value, entry.revision).ok()?;
                if !under(&document, parent) {
                    return None;
                }
                // Before the computed fields: those are the work, and doing it
                // for a subscriber who will not be sent the result is exactly
                // the cost this filter exists to remove.
                if !filter.admits(kind, &document) {
                    return None;
                }
                if let Gate::Readable(who) = gate {
                    let name = name_of(&document).and_then(|n| ResourceName::parse(&n).ok())?;
                    self.authorize(who, Verb::Read, &name).await.ok()?;
                }
                if !matches!(gate, Gate::Machine) {
                    self.answer(&mut document, &mut Scratch::default())
                        .await
                        .ok()?;
                }
                Some(WatchEvent::Put(document))
            }
            Event::Delete { key, revision } => {
                let (_, _, name) = velstra_cloud_store::parse_key(&key)?;
                if !parent.is_empty() && !name.starts_with(&format!("{parent}/")) {
                    return None;
                }
                if let Gate::Readable(who) = gate {
                    let parsed = ResourceName::parse(name).ok()?;
                    self.authorize(who, Verb::Read, &parsed).await.ok()?;
                }
                Some(WatchEvent::Delete {
                    name: name.to_string(),
                    revision,
                })
            }
        }
    }

    // ---- placement --------------------------------------------------------

    /// Why an instance is where it is, or why it is nowhere.
    ///
    /// The answer is computed from the same function the scheduler runs, on the
    /// state as it stands. An explain that consulted a separate copy of the
    /// rules would eventually disagree with the scheduler, and an operator
    /// would believe the wrong one.
    pub async fn explain_placement(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        if name.collection() != "instances" {
            return Err(ApiError::invalid("only an instance is placed on a node"));
        }
        let instance: Instance = self.typed(name).await?;
        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        let instances: Vec<Instance> = self.typed_list("", "instances").await?;

        // An anti-affinity group is occupied by whatever is already assigned —
        // read from the objects rather than from a table the scheduler keeps,
        // because a table drifts and this cannot.
        let occupied: Vec<(String, String)> = instances
            .iter()
            .filter(|other| other.meta.name != instance.meta.name)
            .filter_map(|other| {
                let group = other.spec.placement_policy.anti_affinity_group.clone()?;
                let node = other.spec.node.clone()?;
                Some((group, node))
            })
            .collect();

        // The cell's device classes, so that an instance asking for hardware
        // gets the same answer here as the scheduler will give it. Two
        // explanations that disagree would be worse than one that is late.
        let classes: std::collections::BTreeMap<String, velstra_cloud_model::pci::DeviceClassSpec> = {
            let all: Vec<velstra_cloud_model::resources::DeviceClass> = self
                .typed_list("", "device-classes")
                .await
                .unwrap_or_default();
            all.into_iter()
                .map(|c| (c.meta.name.id().to_string(), c.spec))
                .collect()
        };
        let closed = self.closed_nodes().await?;
        // The opposite ask, read the same way and from the same objects.
        let with_group: Vec<(String, String)> = instances
            .iter()
            .filter(|other| other.meta.name != instance.meta.name)
            .filter_map(|i| {
                Some((
                    i.spec.placement_policy.affinity_group.clone()?,
                    i.spec.node.clone()?,
                ))
            })
            .collect();
        let (candidate, rejected) =
            match place(&instance, &nodes, &occupied, &with_group, &classes, &closed) {
                Ok(node) => (Some(node), Vec::new()),
                Err(chain) => (None, chain),
            };
        let placed = instance.spec.node.clone().or(candidate);
        let rejected: Vec<Value> = rejected
            .iter()
            .map(|e| {
                let r = velstra_cloud_proto::v1::Rejection::from(e);
                json!({ "node": r.node, "why": r.why, "detail": r.detail })
            })
            .collect();
        Ok(json!({ "placed": placed, "rejected": rejected }))
    }

    // ---- operations -------------------------------------------------------

    /// Wait for an operation, up to `timeout`.
    ///
    /// Polling on the server rather than on the client, and bounded: waiting is
    /// an optimisation over asking again, never a promise that the answer will
    /// be `done`.
    pub async fn wait_operation(
        &self,
        name: &ResourceName,
        timeout: Duration,
        who: &Identity,
    ) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(60));
        loop {
            let document = self.get(name, who).await?;
            if document["status"]["done"].as_bool().unwrap_or(false)
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(document);
            }
            tokio::time::sleep(Duration::from_millis(100).min(timeout)).await;
        }
    }

    /// Everything this API computes rather than stores, applied to an object on
    /// its way out.
    ///
    /// There are two, and they are the same idea twice: an operation's `done`
    /// and a migration's `Moved`. Both are judgements about *another* object, so
    /// storing either would create a second copy of a fact that can go stale —
    /// and a stale copy is worse than no copy, because an operator believes it
    /// and debugs the wrong thing. Computed on every read, they cannot disagree
    /// with the world.
    ///
    /// One function so that a read, a list, a change and a watch all answer
    /// alike: a console that learns about an object through a watch event must
    /// see what a `GET` would have told it.
    async fn answer(&self, document: &mut Value, scratch: &mut Scratch) -> ApiResult<()> {
        self.answer_operation(document).await?;
        self.answer_migration(document).await?;
        self.answer_security_group(document, scratch).await?;
        self.answer_port(document).await?;
        self.answer_subnet(document, scratch).await?;
        answer_instance(document);
        answer_node(document);
        self.answer_image(document, scratch).await
    }

    /// Fill in how full a subnet is, from the ports on it.
    ///
    /// The fifth of the same idea, and it arrived here by the same route as the
    /// others after a detour: it was first written as a controller that stored
    /// the numbers, which is a write on every subnet every time any port
    /// anywhere changes. Occupancy is an aggregate over ports, no writer owns
    /// it, and it is looked at by people far less often than it changes — so it
    /// is added up on the way out, where it cannot be stale and costs nothing
    /// when nobody asks.
    async fn answer_subnet(&self, document: &mut Value, scratch: &mut Scratch) -> ApiResult<()> {
        // Only a subnet has a cidr and a gateway.
        if document.get("spec").and_then(|s| s.get("cidr")).is_none()
            || document
                .get("spec")
                .and_then(|s| s.get("gateway"))
                .is_none()
        {
            return Ok(());
        }
        let Ok(subnet) =
            serde_json::from_value::<velstra_cloud_model::resources::Subnet>(document.clone())
        else {
            return Ok(());
        };
        let ports = scratch.ports(self).await?;
        // Floating addresses and load balancer VIPs come out of this same
        // range, so a count that saw only ports would tell an operator a full
        // subnet had room.
        let floating = scratch.floating(self).await?;
        let balancers = scratch.balancers(self).await?;
        let (allocated, available) =
            velstra_cloud_model::ipam::counts(&subnet, &ports, &floating, &balancers);
        document["status"]["allocated"] = json!(allocated);
        document["status"]["available"] = json!(available);
        Ok(())
    }

    /// Say whether a port is waiting on anybody, which for most of them is no.
    ///
    /// Nothing programs a port until a guest that names it runs on a node, so a
    /// port on no guest has no reporter at all — and a reader that calls that
    /// "not reported" puts a permanent entry on the attention list for an object
    /// nobody can act on. Found by signing in to a real cell: two of the first
    /// two entries were a free port and the operation that had created it.
    async fn answer_port(&self, document: &mut Value) -> ApiResult<()> {
        // Only a port has `spec.network` beside `status.programmed`.
        if document
            .get("status")
            .and_then(|s| s.get("programmed"))
            .is_none()
            || document.get("spec").and_then(|s| s.get("network")).is_none()
        {
            return Ok(());
        }
        let generation = document["meta"]["generation"].as_u64().unwrap_or(0);
        let node = document["status"]["node"].as_str().map(str::to_string);
        let programmed = document["status"]["programmed"].as_bool().unwrap_or(false);
        let condition = velstra_cloud_model::resources::port_condition(
            generation,
            node.as_deref(),
            programmed,
        );
        let mut conditions: Vec<velstra_cloud_model::Condition> =
            serde_json::from_value(document["status"]["conditions"].clone()).unwrap_or_default();
        set_condition(&mut conditions, condition);
        document["status"]["conditions"] = json!(conditions);
        // A port nobody carries has been seen by everybody who is ever going to
        // see it, which is what `observedGeneration` is for.
        if node.is_none() {
            document["status"]["observed_generation"] = json!(generation);
        }
        Ok(())
    }

    /// Fill in whether a security group is in force, from the ports that use it.
    ///
    /// The fourth computed field, and for the same reason as the other three:
    /// nothing about a group is a fact any writer owns. A controller writing
    /// "applied" would be guessing, and a node writing it would be one of many
    /// writers on one field. What is true is on the ports — each says whether
    /// its own node has programmed it — so the answer is added up here, on read,
    /// and cannot disagree with the objects it is drawn from.
    async fn answer_security_group(
        &self,
        document: &mut Value,
        scratch: &mut Scratch,
    ) -> ApiResult<()> {
        // Only a security group has `spec.rules`.
        if document.get("spec").and_then(|s| s.get("rules")).is_none() {
            return Ok(());
        }
        let Some(name) = joined(&document["meta"]["name"]) else {
            return Ok(());
        };
        let generation = document["meta"]["generation"].as_u64().unwrap_or(0);
        let (carried, referenced) = self.ports_using(&name, scratch).await?;
        // The addresses in this group, so a node can expand a rule that names it
        // without reading every port in the cell to find out who is in it.
        // Computed here for the same reason as everything else on a group: no
        // writer owns it, and the ports are the only record of it.
        let ports = scratch.ports(self).await?;
        let specs: std::collections::BTreeMap<String, velstra_cloud_model::resources::PortSpec> =
            ports
                .iter()
                .map(|p| (p.meta.name.to_string(), p.spec.clone()))
                .collect();
        document["status"]["members"] =
            json!(velstra_cloud_model::security::members_in(&name, &specs));
        let condition = group_condition(generation, &carried, referenced);
        let mut conditions: Vec<velstra_cloud_model::Condition> =
            serde_json::from_value(document["status"]["conditions"].clone()).unwrap_or_default();
        set_condition(&mut conditions, condition);
        document["status"]["conditions"] = json!(conditions);
        // Observed at the generation the ports have caught up with, so a group
        // whose rules were just changed does not claim to be in force before any
        // port has re-read it.
        if carried.iter().all(|(_, programmed)| *programmed) {
            document["status"]["observed_generation"] = json!(generation);
        }
        Ok(())
    }

    /// The carried ports that name this group with whether each has it in
    /// force, and how many name it at all.
    async fn ports_using(
        &self,
        group: &str,
        scratch: &mut Scratch,
    ) -> ApiResult<(Vec<(String, bool)>, usize)> {
        let ports = scratch.ports(self).await?;
        let naming: Vec<_> = ports
            .iter()
            .filter(|port| port.spec.security_groups.iter().any(|g| g == group))
            .collect();
        let referenced = naming.len();
        let carried = naming
            .into_iter()
            // A port no node carries is not pending: nobody is expected to
            // program it. It still counts as a reference, so the answer can say
            // so rather than claiming nothing names the group.
            .filter(|port| port.status.node.is_some())
            .map(|port| {
                // "Programmed" alone is not enough: a port carrying an older
                // generation of itself is carrying older rules with it.
                let current = port.status.programmed
                    && port.status.observed_generation >= port.meta.generation;
                (port.meta.name.to_string(), current)
            })
            .collect();
        Ok((carried, referenced))
    }

    /// Fill in which nodes hold an image, from what each node reports about
    /// itself.
    ///
    /// The third of the same idea. It is not stored on the image because a list
    /// of nodes is an aggregate, and an aggregate is not a fact anybody owns —
    /// every node in the cell would be writing into one field. Each node reports
    /// what it holds, and this adds them up, which also means a node that has
    /// gone away stops being in the answer instead of lingering in a list
    /// nobody is left to correct.
    async fn answer_image(&self, document: &mut Value, scratch: &mut Scratch) -> ApiResult<()> {
        // Only an image has a digest in its spec.
        if document
            .get("spec")
            .and_then(|s| s.get("digest"))
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Ok(());
        }
        // By the name the bytes are filed under, not by the object's — an image
        // may be called `debian-13`, and a node's copy of it is the same file as
        // every other object carrying those bytes. Comparing names left
        // `cachedOn` permanently empty, on every image, including one a guest
        // was demonstrably running from.
        let stored = document["spec"]["digest"]
            .as_str()
            .and_then(velstra_cloud_model::images::stored_name)
            .unwrap_or_default();
        let nodes = scratch.nodes(self).await?;
        document["status"]["cached_on"] =
            json!(velstra_cloud_model::resources::nodes_holding(&stored, &nodes));
        // And what is on its way. An image had two visible states — here or not
        // — while a gigabyte takes minutes to arrive, so "downloading" and
        // "stuck" looked identical to the person waiting.
        document["status"]["fetching_on"] =
            json!(velstra_cloud_model::resources::nodes_fetching(&stored, &nodes));
        Ok(())
    }

    /// Fill in an operation's `done` from the object it is about.
    ///
    /// `done` is never stored. An operation that kept its own copy of "finished"
    /// could disagree with the resource it describes — and when those two
    /// disagree, an operator believes the operation and debugs the wrong thing.
    async fn answer_operation(&self, document: &mut Value) -> ApiResult<()> {
        let Some(target) = document
            .get("spec")
            .and_then(|s| s.get("target"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        // Only an operation has a spec.target; everything else leaves here
        // untouched.
        if document.get("status").and_then(|s| s.get("done")).is_none() {
            return Ok(());
        }
        let wanted = document["spec"]["target_generation"].as_u64().unwrap_or(0);
        let (done, error) = match ResourceName::parse(target) {
            Ok(name) => match self.collection(name.collection()) {
                Ok(collection) => match collection.get(target).await? {
                    Some(object) => {
                        let observed = object["status"]["observed_generation"]
                            .as_u64()
                            .unwrap_or(0);
                        (observed >= wanted, None)
                    }
                    // The object is gone. Whatever this operation was waiting
                    // for will not happen, and saying so is better than a
                    // client waiting forever on an object nobody will report.
                    None => (true, Some(format!("{target} no longer exists"))),
                },
                Err(e) => (true, Some(e.message)),
            },
            Err(e) => (true, Some(e.to_string())),
        };
        document["status"]["done"] = Value::Bool(done);
        if let Some(error) = error {
            document["status"]["error"] = Value::String(error);
        }
        Ok(())
    }

    /// Mix a migration's `Moved` condition in as it is read.
    ///
    /// What a migration is doing is a judgement over the whole dance — a pure
    /// function of the migration and the instance — and the model computes it in
    /// [`migration_condition`]. It is never written down, and that is what makes
    /// it trustworthy in the case an operator needs most: a migration whose
    /// destination agent has died cannot write anything at all, so a *stored*
    /// condition would be frozen on the last thing that agent managed to say.
    /// Computed, the timeout still arrives on time, from a process that is
    /// still running.
    ///
    /// The clock lives here because the model stays pure: `age_s` is how long
    /// this migration has existed.
    ///
    /// What remains stored in `status.conditions` is only what the destination
    /// can say about itself — that it could not bind a receiver, say. Never
    /// this one.
    async fn answer_migration(&self, document: &mut Value) -> ApiResult<()> {
        // Only a migration names a destination in its spec; every other kind of
        // object leaves here untouched.
        if document
            .get("spec")
            .and_then(|s| s.get("to_node"))
            .is_none()
        {
            return Ok(());
        }
        let mut migration: Migration = serde_json::from_value(document.clone())?;
        let instance = self.instance_of(&migration.spec.instance).await?;
        let age = migration.meta.created_at.age(Timestamp::now()).as_secs();

        let mut condition = migration_condition(&migration, instance.as_ref(), age);
        // A computed condition has no stored moment it changed, so `Condition`
        // stamps the moment it was built — which is the moment of *this read*.
        // An interface showing "changed just now" over a transfer that stalled
        // an hour ago is worse than showing nothing, so a client is right to
        // ignore it.
        //
        // Except here: a timeout happened at exactly `created_at + timeout_s`,
        // and that is worth knowing to the minute — "gave up forty minutes ago"
        // is the difference between a migration to look at now and one somebody
        // has already dealt with.
        if condition.reason == "Timeout" {
            condition.last_transition = Timestamp(
                migration.meta.created_at.0 + u64::from(migration.spec.timeout_s) * 1_000,
            );
        }
        set_condition(&mut migration.status.conditions, condition);
        *document = serde_json::to_value(&migration)?;
        Ok(())
    }

    /// The instance an object names, or `None` if it is not there.
    ///
    /// A missing instance is not an error: objects in a level-triggered system
    /// arrive in any order, and the model has a sentence for a migration whose
    /// instance does not exist. Failing the read instead would hide the
    /// migration that most needs looking at.
    async fn instance_of(&self, name: &str) -> ApiResult<Option<Instance>> {
        let Ok(name) = ResourceName::parse(name) else {
            return Ok(None);
        };
        if name.collection() != "instances" {
            return Ok(None);
        }
        match self.typed::<InstanceSpec, InstanceStatus>(&name).await {
            Ok(instance) => Ok(Some(instance)),
            Err(e) if e.code == Code::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn mint_operation(
        &self,
        target: &ResourceName,
        generation: u64,
        verb: &str,
        who: &Identity,
    ) -> ApiResult<Value> {
        let id = format!("op-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        // The operation lives beside what it is about, so a project's history
        // is under the project rather than in one global list somebody has to
        // filter.
        let name = match target.project() {
            Some(project) => format!("projects/{project}/operations/{id}"),
            None => format!("operations/{id}"),
        };
        let name = ResourceName::parse(&name)?;
        let meta = Meta::new(name, self.inner.placement.clone());
        let spec = serde_json::to_value(OperationSpec {
            target: target.to_string(),
            target_generation: generation,
            verb: verb.to_string(),
            requested_by: who.subject.clone(),
        })
        .expect("an operation spec always serialises");
        self.collection("operations")?
            .create(
                serde_json::to_value(&meta).expect("meta always serialises"),
                spec,
            )
            .await
    }

    // ---- attachments ------------------------------------------------------

    /// An attachment's node is the instance's node.
    ///
    /// The model says so — "copied from the instance so the agent's watch
    /// filter is a single field" — and copying it here rather than asking a
    /// caller for it is what makes the copy true. An attachment whose node
    /// disagrees with its instance's is a meaningless object: the node it names
    /// does not have the guest, and the node that does is not watching for it,
    /// so the volume is never opened and nothing says why. Derived, that state
    /// cannot be written down.
    ///
    /// It is derived **once**, at create, and afterwards only ever changed to
    /// agree again — moving a guest is a migration, and a migration has to move
    /// the attachment deliberately rather than have it follow silently.
    ///
    /// `fallback` is the instance a change is being made against, since a
    /// change carries only what it changes.
    async fn settle_node(&self, spec: &mut Value, fallback: Option<&str>) -> ApiResult<()> {
        let stated = spec
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let instance = spec
            .get("instance")
            .and_then(Value::as_str)
            .filter(|i| !i.is_empty())
            .map(str::to_string)
            .or_else(|| fallback.map(str::to_string))
            .unwrap_or_default();

        let placed = self.node_of(&instance).await?;
        match (stated.as_str(), placed) {
            // Nothing said, and the platform knows: copy it.
            ("", Some(node)) => {
                spec["node"] = Value::String(node);
                Ok(())
            }
            // Nothing said, and nothing to copy. Creating it anyway would make
            // an object no agent will ever pick up.
            ("", None) if instance.is_empty() => Err(ApiError::new(
                Code::FailedPrecondition,
                "an attachment names a volume and an instance, and takes its node from the instance",
            )
            .at("spec.instance")),
            ("", None) => Err(ApiError::new(
                Code::FailedPrecondition,
                format!("{instance} is not on a node yet, so there is no node to open the volume"),
            )
            .at("spec.node")),
            // Said, and wrong. Refused rather than corrected: rewriting what
            // somebody typed changes what the object says without them asking.
            (said, Some(node)) if said != node => Err(ApiError::invalid(format!(
                "{instance} is on {node}, not on {said}; an attachment is opened by the node that \
                 has the guest"
            ))
            .at("spec.node")),
            _ => Ok(()),
        }
    }

    /// The node an instance is on, or `None` if it is unplaced or not there.
    ///
    /// A missing instance is `None` rather than an error: objects in a
    /// level-triggered system are allowed to arrive in any order, and the
    /// caller above decides whether not knowing is fatal for what it is doing.
    async fn node_of(&self, instance: &str) -> ApiResult<Option<String>> {
        let Ok(name) = ResourceName::parse(instance) else {
            return Ok(None);
        };
        if name.collection() != "instances" {
            return Ok(None);
        }
        match self.typed::<InstanceSpec, InstanceStatus>(&name).await {
            Ok(instance) => Ok(instance.spec.node.filter(|n| !n.is_empty())),
            Err(e) if e.code == Code::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ---- snapshots --------------------------------------------------------

    /// A root disk may grow. It may not shrink.
    ///
    /// Shrinking is not a resize, it is a truncation: the bytes past the new
    /// end are gone, and the filesystem that was using them finds out at the
    /// worst possible moment. No backend asks the guest first, and none can —
    /// so the only honest answer is to refuse, and to say what does work.
    ///
    /// Growing is allowed and takes effect when the guest next starts, like
    /// every other size change. What the running guest actually has is on its
    /// status, so nothing here reads as applied while it is not.
    async fn refuse_a_smaller_disk(
        &self,
        name: &ResourceName,
        spec: &Value,
        who: &Identity,
    ) -> ApiResult<()> {
        let Some(asked) = spec.get("root_disk_gib").and_then(Value::as_u64) else {
            return Ok(());
        };
        let _ = who;
        let stored: Instance = self.typed(name).await?;
        if asked >= stored.spec.root_disk_gib {
            return Ok(());
        }
        Err(ApiError::new(
            Code::FailedPrecondition,
            format!(
                "{name} has a {} GiB root disk and cannot be shrunk to {asked}: the bytes past \
                 the new end would be gone and the filesystem using them would find out later. \
                 Make a smaller guest from a backup instead.",
                stored.spec.root_disk_gib
            ),
        )
        .at("spec.rootDiskGib"))
    }

    /// Fill in a capture's node from its guest, and refuse the one thing that
    /// makes a template untrustworthy.
    ///
    /// A disk copied from under a running machine is crash-consistent at best.
    /// That is survivable for a backup — read once, in an emergency, by
    /// somebody who knows what happened — and not survivable for a template
    /// that will be stamped out a hundred times by people who assume it is
    /// clean. The corruption then arrives a hundred times, later, with nothing
    /// pointing back at this moment.
    async fn settle_capture(&self, spec: &mut Value) -> ApiResult<()> {
        let instance_name = spec
            .get("instance")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let instance: Instance = self
            .typed(&ResourceName::parse(&instance_name).map_err(|e| {
                ApiError::invalid(format!("spec.instance: {e}")).at("spec.instance")
            })?)
            .await?;

        let target_name = spec
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let target: velstra_cloud_model::resources::BackupTarget = self
            .typed(
                &ResourceName::parse(&target_name).map_err(|e| {
                    ApiError::invalid(format!("spec.target: {e}")).at("spec.target")
                })?,
            )
            .await?;

        let guest = velstra_cloud_model::capture::GuestView {
            name: instance_name.clone(),
            running: instance.status.state
                == velstra_cloud_model::resources::InstanceState::Running,
            node: instance.status.node.clone().filter(|n| !n.is_empty()),
            deleting: instance.meta.is_deleting(),
        };
        // `None` is "nobody is looking", which is not "no": a target whose
        // spec names no reporting agent may be perfectly good, and turning
        // silence into a refusal would make every capture wait for a field an
        // operator has to know to set. A path that cannot be written fails on
        // the copy, loudly, where it can be seen.
        let usable = target.spec.accepting && target.status.writable != Some(false);
        if let Err(refusal) =
            velstra_cloud_model::capture::may_capture(&guest, usable, &target_name)
        {
            // The field is the control somebody can act on: a running guest is
            // a different problem from a target that has gone.
            let field = match &refusal {
                velstra_cloud_model::capture::Refusal::TargetUnusable { .. } => "spec.target",
                _ => "spec.instance",
            };
            return Err(ApiError::new(Code::FailedPrecondition, refusal.to_string()).at(field));
        }

        // The node holding the disk, derived rather than asked for. Without it
        // the object is assigned to nobody and no agent may ever claim it —
        // the same hole the backup schedule had.
        spec["node"] = Value::String(guest.node.unwrap_or_default());
        Ok(())
    }

    /// Fill in a backup's pool from its volume, and refuse the one target that
    /// makes it not a backup.
    ///
    /// The refusal is the whole point of the collection. A copy in the source's
    /// own pool is a snapshot wearing a backup's name: it is lost with the pool
    /// it is in, which is the one failure a backup is bought to survive. A
    /// platform that accepted it would be selling a promise it does not keep,
    /// and the operator finds out at the worst possible moment.
    async fn settle_backup(&self, spec: &mut Value) -> ApiResult<()> {
        let volume_name = spec
            .get("volume")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let volume: Volume = self
            .typed(
                &ResourceName::parse(&volume_name).map_err(|e| {
                    ApiError::invalid(format!("spec.volume: {e}")).at("spec.volume")
                })?,
            )
            .await?;

        let mut target_name = spec
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Left empty, the cell answers — the same shape as a volume's pool. A
        // target is the cell's infrastructure, invisible to a tenant by
        // design, so requiring its name made tenant backups impossible by
        // construction: the form's picker was empty and the refusal named an
        // object they may not list. Chosen: accepting, writable, most room.
        if target_name.is_empty() {
            let targets: Vec<velstra_cloud_model::resources::BackupTarget> =
                self.typed_list("", "backup-targets").await?;
            let chosen = targets
                .iter()
                .filter(|t| {
                    t.spec.accepting
                        && t.meta.deleted_at.is_none()
                        && t.status.writable == Some(true)
                })
                .max_by_key(|t| t.status.free_gib);
            let Some(target) = chosen else {
                return Err(ApiError::new(
                    Code::FailedPrecondition,
                    "nowhere in this cell takes backups: no backup target is accepting and \
                     writable. A cell operator declares one — a directory on a machine, or a \
                     mount every pool agent reaches.",
                )
                .at("spec.target"));
            };
            target_name = target.meta.name.to_string();
            spec["target"] = Value::String(target_name.clone());
        }
        let target: velstra_cloud_model::resources::BackupTarget = self
            .typed(
                &ResourceName::parse(&target_name).map_err(|e| {
                    ApiError::invalid(format!("spec.target: {e}")).at("spec.target")
                })?,
            )
            .await?;

        // Whether this target's path is a pool's. Answered from the pools
        // themselves rather than from a flag somebody set on the target: a
        // flag is a claim, and the thing that matters here is a fact.
        let pools: Vec<velstra_cloud_model::resources::Pool> =
            self.typed_list("", "pools").await.unwrap_or_default();
        let same_pool_as = pools
            .iter()
            .find(|p| {
                p.status
                    .conditions
                    .iter()
                    .any(|c| c.reason == "PathIs" && c.message == target.spec.path)
            })
            .map(|p| p.meta.name.to_string());

        let view = velstra_cloud_model::backup::TargetView {
            name: target.meta.name.to_string(),
            path: target.spec.path.clone(),
            accepting: target.spec.accepting,
            writable: target.status.writable,
            same_pool_as,
        };
        if let Err(refusal) =
            velstra_cloud_model::backup::may_back_up(&volume_name, &volume.spec.pool, &view)
        {
            return Err(
                ApiError::new(Code::FailedPrecondition, refusal.to_string()).at("spec.target")
            );
        }

        // The pool holding the source, derived rather than asked for — the same
        // reason a snapshot's is. Without it the object is assigned to nobody
        // and no agent may ever claim it.
        spec["pool"] = Value::String(volume.spec.pool.clone());
        Ok(())
    }

    /// Fill in the pool a copy is made in, and refuse a copy that cannot work.
    ///
    /// A snapshot's source is its **parent**: `projects/p1/volumes/data-1/
    /// snapshots/nightly` is a copy of `data-1`, and there is no field saying
    /// so. That is why this reads the name rather than the spec — and it is the
    /// point of the shape, because which volume a copy came from is the one
    /// thing about it that must never change, and a name cannot be patched.
    ///
    /// `spec.pool` is derived from the volume for the same reason an
    /// attachment's node is derived from its instance: it is one fact, the
    /// platform has it, and a copy in a pool that does not hold the original is
    /// not something any backend does. Stated and wrong is refused rather than
    /// corrected.
    async fn settle_snapshot(&self, name: &ResourceName, spec: &mut Value) -> ApiResult<()> {
        let source = name
            .parent()
            .filter(|parent| parent.collection() == "volumes")
            .ok_or_else(|| {
                ApiError::invalid(
                    "a snapshot is a copy of one volume and lives under it: post to \
                     projects/p1/volumes/data-1/snapshots, not to a collection of its own",
                )
                .at("meta.name")
            })?;
        let volume: Volume = self.typed(&source).await?;

        // Neither of these is anything an operator can fix by changing a field
        // — one is waiting, the other is a volume on its way out — so they
        // arrive as a sentence rather than pointing at a control.
        if let Err(refusal) = may_snapshot(&volume) {
            return Err(ApiError::new(Code::FailedPrecondition, refusal.to_string()));
        }

        match spec.get("pool").and_then(Value::as_str).unwrap_or_default() {
            "" => {
                spec["pool"] = Value::String(volume.spec.pool.clone());
                Ok(())
            }
            said if said != volume.spec.pool => Err(ApiError::invalid(format!(
                "{source} is in {}, not in {said}; a copy is made by the pool that holds the \
                 volume",
                volume.spec.pool
            ))
            .at("spec.pool")),
            _ => Ok(()),
        }
    }

    /// Fill in what a volume made from a snapshot inherits, and refuse one that
    /// cannot be made.
    ///
    /// Two fields are derived, both under the rule the contract already states
    /// — omitted is filled in, stated must agree:
    ///
    /// * `spec.pool`, because a clone is written by the pool that holds the
    ///   snapshot and no backend copies between pools behind one command;
    /// * `spec.sizeGib`, because the size of the clone is the size of the
    ///   snapshot. "Must agree" is relaxed to "must be at least" here, and only
    ///   here: a volume is grown, so asking for a bigger one at the moment it is
    ///   made is an ordinary thing to want. Asking for a smaller one is the
    ///   clone not fitting in what it is written into.
    async fn settle_volume_source(&self, volume: &ResourceName, spec: &mut Value) -> ApiResult<()> {
        let named = spec
            .get("source_snapshot")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let from = match &named {
            Some(name) => {
                let name = ResourceName::parse(name).map_err(|_| {
                    ApiError::invalid(
                        "a snapshot is named in full, like \
                                       projects/p1/volumes/data-1/snapshots/nightly",
                    )
                    .at("spec.sourceSnapshot")
                })?;
                if name.collection() != "snapshots" {
                    return Err(ApiError::invalid(format!(
                        "{name} is not a snapshot; a volume is made from one, like \
                         projects/p1/volumes/data-1/snapshots/nightly"
                    ))
                    .at("spec.sourceSnapshot"));
                }
                Some(self.typed::<SnapshotSpec, SnapshotStatus>(&name).await?)
            }
            None => None,
        };

        if let Some(snapshot) = &from {
            if spec
                .get("pool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .is_empty()
            {
                spec["pool"] = Value::String(snapshot.spec.pool.clone());
            }
            if spec.get("size_gib").and_then(Value::as_u64).unwrap_or(0) == 0 {
                spec["size_gib"] = json!(snapshot.status.size_gib);
            }
        }

        let wanted: VolumeSpec = serde_json::from_value(spec.clone())?;
        if let Err(refusal) = may_create_volume(volume, &wanted, from.as_ref()) {
            use velstra_cloud_model::storage::Refusal;
            let field = match &refusal {
                Refusal::SmallerThanItsSnapshot { .. } => "spec.sizeGib",
                Refusal::AnotherPool { .. } => "spec.pool",
                // Either source can be the one reaching out of the project, and
                // an operator told the wrong field bisects a spec by hand.
                Refusal::AnotherProject { origin, .. }
                    if wanted.source_image.as_deref() == Some(origin.as_str()) =>
                {
                    "spec.sourceImage"
                }
                _ => "spec.sourceSnapshot",
            };
            return Err(ApiError::new(Code::FailedPrecondition, refusal.to_string()).at(field));
        }
        Ok(())
    }

    /// Where a volume's bytes came from is history, and history is not a
    /// control.
    ///
    /// Changing `sourceSnapshot` on an existing volume is what an operator
    /// reaches for when they mean "restore this". It is refused, and the
    /// refusal says what to do instead, because an in-place restore is the one
    /// storage operation this model cannot express honestly: it would be a
    /// command sitting in a `spec`, and a command in a spec is performed again
    /// on every resync — undoing whatever the guest wrote in between, forever,
    /// with nothing on the object to say it happened.
    ///
    /// Changing `sourceImage` is refused for the plainer reason: nothing is
    /// re-cloned, so the field would simply start describing a volume that does
    /// not exist.
    /// Refuse a Ceph OSD on a disk the node holding it does not offer.
    ///
    /// The console only ever offers disks
    /// [`velstra_cloud_model::ceph::may_consume`] accepts, and
    /// [`velstra_cloud_model::ceph::next_step`] refuses again against the
    /// node's current inventory before any command runs — so nothing here
    /// stands between a disk and being erased. What it stands between is an
    /// operator and a silence: a hand-written spec naming the wrong disk would
    /// otherwise be accepted, and the only sign of the mistake would be a
    /// cluster that quietly never finishes, with the reason on a condition
    /// nobody is looking at yet.
    ///
    /// Answered from what the nodes report, so a disk on a node that has not
    /// reported yet is *not* refused: "I cannot see it" is not "it is not
    /// free", and refusing a cluster because a machine is booting would be
    /// wrong about the one thing this is for.
    /// A cell holds at most one Ceph cluster.
    ///
    /// The invariant is not carried by the type — `ceph-clusters` is an ordinary
    /// collection — so create is the one place to state it, and the one place it
    /// can say why it refused. Only on create: an update names the cluster that
    /// already exists and is editing the singleton, not adding a second one.
    async fn refuse_a_second_ceph_cluster(&self, name: &ResourceName) -> ApiResult<()> {
        let this = name.to_string();
        for existing in self.collection("ceph-clusters")?.list().await? {
            let id = existing["meta"]["name"].as_str().unwrap_or_default();
            // The name being created cannot already be in the list — the store
            // refuses a duplicate create on its own — so anything here is a
            // *different* cluster, and a second one is what the cell may not have.
            if id != this {
                return Err(ApiError::new(
                    Code::AlreadyExists,
                    format!(
                        "this cell already has a Ceph cluster ({id}); a cell has at most one. \
                         Edit that one, or delete it before creating another"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// A baseline a machine cannot reach is refused here rather than at boot.
    ///
    /// `-cpu <level>,enforce` would catch it: QEMU refuses to start a guest it
    /// cannot give the promised processor. That is the right behaviour and the
    /// wrong moment to find out — "the guests on node-c stopped booting" is a
    /// long way from "node-c is a generation too old for the level you typed".
    ///
    /// Reads the node's *reported* flags, which is why this cannot be a plain
    /// spec check: the answer depends on what the machine said about itself.
    /// A node that has not reported a CPU yet is refused too — nothing about
    /// it can be shown, and a baseline set on a machine nobody has heard from
    /// is a promise with no basis.
    async fn refuse_a_baseline_this_node_cannot_present(
        &self,
        name: &ResourceName,
        spec: &Value,
        who: &Identity,
    ) -> ApiResult<()> {
        let Some(level) = spec.get("cpu_baseline") else {
            return Ok(());
        };
        // Clearing it is always allowed: going back to the host's own
        // processor asks nothing of the machine.
        if level.is_null() {
            return Ok(());
        }
        let level: velstra_cloud_model::cpu::CpuLevel = serde_json::from_value(level.clone())
            .map_err(|_| {
                ApiError::invalid(
                    "a cpu baseline is one of x86-64-v1, x86-64-v2, x86-64-v3, x86-64-v4",
                )
                .at("spec.cpuBaseline")
            })?;

        let _ = who;
        let stored: Node = self.typed(name).await?;
        let Some(reported) = stored.status.cpu else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "{} has not reported a cpu yet, so it cannot be shown to present {level}",
                    name.id()
                ),
            )
            .at("spec.cpuBaseline"));
        };
        if let Err(missing) = velstra_cloud_model::cpu::can_present(&reported, level) {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "{} cannot present {level}: it lacks {}",
                    name.id(),
                    missing.join(", ")
                ),
            )
            .at("spec.cpuBaseline"));
        }
        Ok(())
    }

    /// An address that could not reach anything is refused where it is asked
    /// for, not discovered by somebody's customer.
    ///
    /// Two of the three refusals need the rest of the cell — whether the subnet
    /// is on an external network, and whether any machine is a gateway — so
    /// this reads both rather than deciding from the body alone.
    async fn refuse_an_address_that_reaches_nothing(
        &self,
        name: &ResourceName,
        spec: &Value,
    ) -> ApiResult<()> {
        let asked: velstra_cloud_model::resources::FloatingIpSpec =
            serde_json::from_value(spec.clone()).map_err(|e| {
                ApiError::invalid(format!("that is not a floating address: {e}")).at("spec")
            })?;

        // The subnet, and the network it is on. A subnet that is not there is
        // somebody else's refusal — `refs::check` and the address controller
        // both say so — and inventing a second sentence for it here would be
        // two answers to one mistake.
        let (external, network_announce) = match self.external_of(&asked.subnet).await {
            Some(pair) => pair,
            None => return Ok(()),
        };

        let nodes: Vec<Node> = self.typed_list("", "nodes").await.unwrap_or_default();
        let gateways = nodes.iter().filter(|n| n.spec.gateway).count();

        let view = velstra_cloud_model::public::AddressView {
            name: name.to_string(),
            address: asked.address.as_deref().and_then(|a| a.parse().ok()),
            subnet: asked.subnet.clone(),
            subnet_is_external: external,
            delivery: asked.delivery,
            announce: asked.announce,
            port: asked.port.clone(),
        };
        velstra_cloud_model::public::may_publish(&view, network_announce, gateways)
            .map_err(|why| ApiError::new(Code::FailedPrecondition, why.to_string()).at("spec"))
    }

    /// Whether this subnet is on an external network, and what that network
    /// says about announcements.
    async fn external_of(
        &self,
        subnet: &str,
    ) -> Option<(bool, velstra_cloud_model::public::Announce)> {
        let name = ResourceName::parse(subnet).ok()?;
        let subnet: velstra_cloud_model::resources::Subnet = self.typed(&name).await.ok()?;
        let network = ResourceName::parse(&subnet.spec.network).ok()?;
        let network: velstra_cloud_model::resources::Network = self.typed(&network).await.ok()?;
        Some((network.spec.external, network.spec.announce))
    }

    /// A maintenance window that could never take effect is refused now.
    ///
    /// Every one of these is knowable at the moment it is declared, and the
    /// alternative is finding out at three in the morning: a window of zero
    /// minutes sits in the list looking like a plan, and two overlapping
    /// windows are two answers to "may this node take work at four o'clock"
    /// where whoever declared the second believes theirs is the answer.
    async fn refuse_a_window_that_would_do_nothing(
        &self,
        name: &ResourceName,
        spec: &Value,
    ) -> ApiResult<()> {
        let asked: velstra_cloud_model::maintenance::MaintenanceWindowSpec =
            serde_json::from_value(spec.clone()).map_err(|e| {
                ApiError::invalid(format!("that is not a maintenance window: {e}")).at("spec")
            })?;
        let existing: Vec<velstra_cloud_model::resources::MaintenanceWindow> =
            self.typed_list("", "maintenance-windows").await?;
        let view =
            |name: String, spec: &velstra_cloud_model::maintenance::MaintenanceWindowSpec| {
                velstra_cloud_model::maintenance::WindowView {
                    name,
                    node: spec.node.clone(),
                    starts_at: spec.starts_at,
                    minutes: spec.minutes,
                    drain: spec.drain,
                    note: spec.note.clone(),
                }
            };
        let others: Vec<_> = existing
            .iter()
            .map(|w| view(w.meta.name.to_string(), &w.spec))
            .collect();
        velstra_cloud_model::maintenance::may_declare(
            &view(name.to_string(), &asked),
            &others,
            velstra_cloud_model::meta::Timestamp::now(),
        )
        .map_err(|why| ApiError::new(Code::FailedPrecondition, why.to_string()).at("spec"))
    }

    /// The nodes that are out of service this instant.
    ///
    /// Read wherever placement is computed, so that "no valid host" says which
    /// machines are out and when they come back rather than leaving an operator
    /// to work out that the node they can see is deliberately unavailable.
    async fn closed_nodes(&self) -> ApiResult<Vec<velstra_cloud_model::maintenance::Closed>> {
        let windows: Vec<velstra_cloud_model::resources::MaintenanceWindow> =
            self.typed_list("", "maintenance-windows").await?;
        Ok(velstra_cloud_model::maintenance::closed_now(
            &windows.iter().map(window_view).collect::<Vec<_>>(),
            velstra_cloud_model::meta::Timestamp::now(),
        ))
    }

    /// Where this address is, who says so, and what the guest should have.
    ///
    /// The question somebody asks when an address does not answer, and every
    /// part of it is knowable without touching a packet: which machine is
    /// announcing it (or why nothing is), what the guest must have configured,
    /// and what the guest would have to do differently if it were moved.
    /// Grant somebody a way into a guest's serial line.
    ///
    /// Returns the ticket **once**, in this answer and nowhere else: what is
    /// stored is its hash, because every node in the cell may read the cell and
    /// a session carrying the ticket in the clear would hand each of them a way
    /// into a guest on somebody else's machine.
    ///
    /// The permission question is answered here and only here. The node has no
    /// bindings to read and must never be the place one is decided; it is told
    /// on the session whether the holder may type.
    pub async fn open_console(
        &self,
        name: &ResourceName,
        kind: velstra_cloud_model::console::ConsoleKind,
        who: &Identity,
    ) -> ApiResult<Value> {
        use velstra_cloud_model::console::ConsoleKind;
        // Read is the floor: watching a guest's console is reading it. Whether
        // the holder may also *type* is a second question, asked below and
        // answered on the object.
        self.authorize(who, Verb::Read, name).await?;
        let instance: Instance = self.typed(name).await?;
        let Some(node) = instance.status.node.clone().filter(|n| !n.is_empty()) else {
            return Err(ApiError::new(Code::FailedPrecondition, format!(
                "{name} is not on a node yet, so there is nothing to attach to. A guest gets a                  console when a node has claimed it."
            ))
            .at("status.node"));
        };

        // A viewer gets a window; somebody who may change the guest gets a
        // keyboard. Asked as a question rather than taken from the refusal path,
        // so a viewer is *given a console* rather than told no.
        // Typing into a guest is operating it, not creating one. Somebody who
        // may reboot a machine may also fix it from its console; somebody who
        // may only look at it gets a window.
        let read_only = self.authorize(who, Verb::Operate, name).await.is_err();
        // A read-only screen is not built. The serial relay enforces "may only
        // watch" by dropping what the viewer types; VNC cannot be watched that
        // way — the protocol's own handshake is bytes the client sends, so a
        // relay that dropped client bytes would never finish opening. Refused
        // with the way that works rather than granted and broken.
        if kind == ConsoleKind::Vnc && read_only {
            return Err(ApiError::forbidden(
                "this account may watch this guest but not operate it, and a view-only screen \
                 is not something this platform can serve yet — the serial console can be \
                 watched read-only",
            ));
        }

        let ticket = uuid::Uuid::new_v4().to_string();
        let now = Timestamp::now();
        let project = governing_project(name).unwrap_or_default();
        let id = format!(
            "console-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let session_name = if project.is_empty() {
            format!("console-sessions/{id}")
        } else {
            format!("{project}/console-sessions/{id}")
        };
        let session = ResourceName::parse(&session_name)?;
        // Minted by the API on somebody's behalf, exactly as an operation is,
        // and for the same reason: creating it *as* them would be a write, and a
        // viewer who may watch a console has no write anywhere. The permission
        // question was answered above; this is only the record of the answer.
        let meta = Meta::new(session, self.inner.placement.clone());
        let spec = serde_json::to_value(velstra_cloud_model::console::ConsoleSessionSpec {
            instance: name.to_string(),
            node,
            subject: who.subject.clone(),
            ticket_sha256: velstra_cloud_model::console::sha256_hex(&ticket),
            expires_at: Timestamp(now.0 + velstra_cloud_model::console::TICKET_LIFETIME_MS),
            read_only,
            kind,
        })
        .expect("a console session spec always serialises");
        self.collection("console-sessions")?
            .create(
                serde_json::to_value(&meta).expect("meta always serialises"),
                spec,
            )
            .await?;

        Ok(serde_json::json!({
            "session": session_name,
            // The one and only time this leaves the API.
            "ticket": ticket,
            "readOnly": read_only,
            "expiresAt": now.0 + velstra_cloud_model::console::TICKET_LIFETIME_MS,
        }))
    }

    /// Where a console attach should be forwarded, for a ticket the caller
    /// holds.
    ///
    /// The ticket is **not** checked here: the check is against the session
    /// object and the node reads the same one, so checking twice would be two
    /// copies of a rule with two chances to disagree. What this answers is only
    /// "which machine, and does it serve consoles at all" — and a node that
    /// serves none is said out loud rather than silently connected to nothing.
    /// Take away console sessions nobody can use any more.
    ///
    /// A ticket is spent in a minute and the object outlives it; without this,
    /// every click on Console leaves one behind for ever, and a cell that has
    /// been running a year holds a collection nothing reads.
    ///
    /// Kept for a day rather than deleted at expiry, because the object is also
    /// the record of **who opened a console into which guest** — which is what
    /// somebody investigating a machine actually needs, and it is worth more
    /// than the row costs. Anything older than that has been read or never will
    /// be; the audit trail proper is `audit`.
    pub async fn sweep_spent_consoles(&self, now: Timestamp) -> ApiResult<usize> {
        let collection = self.collection("console-sessions")?;
        let sessions: Vec<velstra_cloud_model::resources::ConsoleSession> =
            self.typed_list("", "console-sessions").await?;
        let mut swept = 0;
        for session in sessions {
            let over = now.0.saturating_sub(session.spec.expires_at.0);
            if over < CONSOLE_RECORD_LIFETIME_MS {
                continue;
            }
            // A delete that loses a race is not a failure of this sweep: the row
            // is gone, which is all it wanted.
            if collection
                .delete(&session.meta.name.to_string(), None)
                .await
                .is_ok()
            {
                swept += 1;
            }
        }
        Ok(swept)
    }

    pub async fn console_endpoint_for(&self, session: &ResourceName) -> ApiResult<String> {
        let session: velstra_cloud_model::resources::ConsoleSession = self.typed(session).await?;
        let node = ResourceName::parse(&format!("nodes/{}", session.spec.node))?;
        let node: velstra_cloud_model::resources::Node = self.typed(&node).await?;
        if node.status.console_endpoint.is_empty() {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "{} runs this guest and serves no console. A node serves one when its agent                      could bind the console port; check the agent's journal on that machine.",
                    session.spec.node
                ),
            )
            .at("status.consoleEndpoint"));
        }
        Ok(node.status.console_endpoint.clone())
    }

    /// Who a console stream is opened as, when the ticket is the only credential
    /// the caller can present.
    ///
    /// **A browser cannot put a header on a WebSocket.** `new WebSocket(url)`
    /// takes a URL and nothing else — there is no options bag, and no way to add
    /// `Authorization`. So a console that authenticated only the way every other
    /// request here does was a console no browser could ever open, which is what
    /// it was: the grant succeeded, the stream was refused with "no valid
    /// credentials available", and every test passed because a test client sends
    /// the header a browser cannot.
    ///
    /// The ticket already is a credential and was built as one: minted for one
    /// person against one guest, stored only as a hash, valid for a minute,
    /// spendable once. Reading the identity off the session it names is not a
    /// weaker check than the bearer token — it is a *narrower* one, good for one
    /// machine for one minute.
    ///
    /// Spending is still the node's to do. This only says who is asking, and the
    /// authorisation that follows is the same `get` every reader passes.
    pub async fn identity_from_console_ticket(
        &self,
        session: &ResourceName,
        ticket: &str,
    ) -> ApiResult<Identity> {
        if session.collection() != "console-sessions" {
            return Err(ApiError::new(
                Code::Unauthenticated,
                "that is not a console session",
            ));
        }
        let session: velstra_cloud_model::resources::ConsoleSession = self
            .typed(session)
            .await
            .map_err(|_| ApiError::new(Code::Unauthenticated, "no such console session"))?;
        if session.status.attached_at.is_some() {
            return Err(ApiError::new(
                Code::Unauthenticated,
                velstra_cloud_model::console::Refused::Spent.to_string(),
            ));
        }
        if Timestamp::now().0 >= session.spec.expires_at.0 {
            return Err(ApiError::new(
                Code::Unauthenticated,
                velstra_cloud_model::console::Refused::Expired.to_string(),
            ));
        }
        if !velstra_cloud_model::console::constant_time_eq(
            &velstra_cloud_model::console::sha256_hex(ticket),
            &session.spec.ticket_sha256,
        ) {
            return Err(ApiError::new(
                Code::Unauthenticated,
                velstra_cloud_model::console::Refused::WrongTicket.to_string(),
            ));
        }
        let mut identity = Identity::new(&session.spec.subject);
        // Scoped to the one guest it was minted against, and nothing else. A
        // ticket that carried the asker's own powers would be a session key with
        // a short life, sitting in a URL.
        identity.scopes.push(format!(
            "{}{}",
            crate::sessions::CONSOLE_SCOPE_PREFIX,
            session.spec.instance
        ));
        Ok(identity)
    }

    pub async fn explain_reach(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        let fip: velstra_cloud_model::resources::FloatingIp = self.typed(name).await?;
        let (external, network_announce) = self
            .external_of(&fip.spec.subnet)
            .await
            .unwrap_or((false, Default::default()));

        // Where the guest holding the port actually is — not where it was
        // assigned. An address is reachable where the guest *is*, and
        // announcing from an assignment is how a migration's last moments
        // become a black hole.
        let port_node = match ResourceName::parse(&fip.spec.port) {
            Ok(port_name) if !fip.spec.port.is_empty() => {
                let port: velstra_cloud_model::resources::Port = self.typed(&port_name).await?;
                let instances: Vec<Instance> = self.typed_list("", "instances").await?;
                instances
                    .iter()
                    .find(|i| i.spec.ports.iter().any(|p| p == &fip.spec.port))
                    .and_then(|i| i.status.node.clone())
                    .or(port.spec.node.clone())
            }
            _ => None,
        };

        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        let gateways: Vec<String> = nodes
            .iter()
            .filter(|n| n.spec.gateway)
            .map(|n| n.meta.name.id().to_string())
            .collect();

        let view = velstra_cloud_model::public::AddressView {
            name: name.to_string(),
            address: fip.spec.address.as_deref().and_then(|a| a.parse().ok()),
            subnet: fip.spec.subnet.clone(),
            subnet_is_external: external,
            delivery: fip.spec.delivery,
            announce: fip.spec.announce,
            port: fip.spec.port.clone(),
        };
        let who_announces = velstra_cloud_model::public::announcer(
            &view,
            network_announce,
            port_node.as_deref(),
            &gateways,
        );

        use velstra_cloud_model::public::{Announcer, Delivery};
        let announced = match &who_announces {
            Announcer::Host(node) => json!({ "from": "host", "nodes": [node] }),
            Announcer::Gateways(nodes) => json!({ "from": "gateway", "nodes": nodes }),
            Announcer::Nowhere(why) => json!({
                "from": null, "nodes": [], "why": why.to_string(),
            }),
        };

        // What the guest must have. Rendered from the same function the
        // metadata service renders from, so what an operator is told here and
        // what the guest was told cannot disagree.
        let guest = view
            .address
            .filter(|_| view.delivery == Delivery::Routed)
            .map(|address| {
                let route = velstra_cloud_model::public::guest_route(address);
                json!({
                    "address": format!("{}/{}", route.address, route.prefix_len),
                    "via": route.via.to_string(),
                    "onLink": route.on_link,
                    "defaultRoute": true,
                })
            });

        let mut answer = json!({
            "address": fip.spec.address,
            "delivery": match view.delivery {
                Delivery::Routed => "Routed",
                Delivery::Nat => "Nat",
            },
            "external": external,
            "port": fip.spec.port,
            "on": port_node,
            "announced": announced,
            // `null` for a translated address: there is nothing for the guest
            // to configure, which is the whole difference between the two.
            "guest": guest,
        });
        // The same rule every other answer keeps: machines are not part of a
        // tenant's view. `on` and the announcing nodes are the operator's half
        // of this explanation; the tenant's half — the address, how it is
        // delivered, what the guest configures — stands on its own.
        if !self.is_operator(who) {
            answer.as_object_mut().unwrap().remove("on");
            if let Some(a) = answer.get_mut("announced").and_then(Value::as_object_mut) {
                a.remove("nodes");
            }
        }
        Ok(answer)
    }

    /// One month's consumption, summed the way a bill is.
    ///
    /// The hourly readings exist for exactly this and were only ever served
    /// raw: forty-nine rows of "at 14:00 you had one guest" that nobody was
    /// going to add up by hand. This adds them up — each reading is one hour
    /// at what the reading says, which is the industry's own arithmetic — and
    /// answers in metric-hours: vCPU-hours, memory-GiB-hours, storage
    /// GiB-hours, address-hours.
    ///
    /// **`hours` is the number of readings, and that is the honest count.** A
    /// cell that was down took no readings; those hours are missing from the
    /// sum rather than invented, and a caller reconciling an invoice can see
    /// the gap (`hours` vs. the hours in the month so far).
    ///
    /// Authorised as a read of the project, so a tenant sums their own bill
    /// and nobody else's.
    pub async fn explain_usage(
        &self,
        name: &ResourceName,
        month: Option<&str>,
        who: &Identity,
    ) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        if name.collection() != "projects" {
            return Err(ApiError::invalid("usage is summed for a project"));
        }
        let now = Timestamp::now();
        let (year, month_no) = match month {
            Some(text) => {
                let mut halves = text.splitn(2, '-');
                let parsed = (
                    halves.next().and_then(|y| y.parse::<i64>().ok()),
                    halves.next().and_then(|m| m.parse::<u32>().ok()),
                );
                match parsed {
                    (Some(y), Some(m)) if (1..=12).contains(&m) => (y, m),
                    _ => {
                        return Err(ApiError::invalid(format!(
                            "month is spelled 2026-08, and was {text:?}"
                        ))
                        .at("month"));
                    }
                }
            }
            None => {
                let days = now.0 / 86_400_000;
                // Civil-from-days (Howard Hinnant's algorithm), which is how a
                // millisecond timestamp becomes "which month" without pulling a
                // calendar crate in for one division.
                let z = days as i64 + 719_468;
                let era = z.div_euclid(146_097);
                let doe = z.rem_euclid(146_097);
                let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let m = if mp < 10 { mp + 3 } else { mp - 9 };
                let y = yoe + era * 400 + i64::from(m <= 2);
                (y, m as u32)
            }
        };
        // The month's bounds, in the same civil arithmetic, run forward.
        let days_from = |y: i64, m: u32| -> i64 {
            let y = if m <= 2 { y - 1 } else { y };
            let era = y.div_euclid(400);
            let yoe = y.rem_euclid(400);
            let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
            let doy = (153 * mp + 2) / 5;
            era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
        };
        let start = Timestamp(days_from(year, month_no) as u64 * 86_400_000);
        let (next_y, next_m) = if month_no == 12 { (year + 1, 1) } else { (year, month_no + 1) };
        let end = Timestamp(days_from(next_y, next_m) as u64 * 86_400_000);

        let records: Vec<velstra_cloud_model::resources::UsageRecord> =
            self.typed_list(&name.to_string(), "usage").await?;
        let mut hours: u64 = 0;
        let mut vcpu_hours: u64 = 0;
        let mut memory_mib_hours: u64 = 0;
        let mut volume_gib_hours: u64 = 0;
        let mut instance_hours: u64 = 0;
        let mut floating_ip_hours: u64 = 0;
        for r in &records {
            if r.spec.at.0 < start.0 || r.spec.at.0 >= end.0 {
                continue;
            }
            hours += 1;
            vcpu_hours += u64::from(r.spec.used.vcpus);
            memory_mib_hours += r.spec.used.memory_mib;
            volume_gib_hours += r.spec.used.volume_gib;
            instance_hours += u64::from(r.spec.used.instances);
            floating_ip_hours += u64::from(r.spec.used.floating_ips);
        }
        // How many billable hours the month has held so far, so a gap is a
        // number and not a suspicion.
        let elapsed_ms = now.0.clamp(start.0, end.0).saturating_sub(start.0);
        Ok(json!({
            "month": format!("{year:04}-{month_no:02}"),
            "hours": hours,
            "hoursInMonthSoFar": elapsed_ms / velstra_cloud_model::usage::INTERVAL_MS,
            "vcpuHours": vcpu_hours,
            "memoryGibHours": memory_mib_hours / 1024,
            "volumeGibHours": volume_gib_hours,
            "instanceHours": instance_hours,
            "floatingIpHours": floating_ip_hours,
        }))
    }

    /// The cell as numbers, in Prometheus' text format.
    ///
    /// What a person watches the overview for, made scrapeable — because
    /// production is when nobody is watching the overview. Deliberately small:
    /// the numbers an alert would fire on (a silent node, a full pool, guests
    /// off their asked state), not a metric per field. An operator's read,
    /// like the overview it mirrors: these lines carry machine names.
    pub async fn metrics(&self, who: &Identity) -> ApiResult<String> {
        self.authorize_for(who, Verb::Read, &ResourceName::parse("nodes/any")?, "nodes")
            .await?;
        let now = Timestamp::now();
        let mut out = String::new();
        use std::fmt::Write;

        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        let _ = writeln!(out, "# TYPE velstra_node_heartbeat_age_seconds gauge");
        for n in &nodes {
            let age = now.0.saturating_sub(n.status.last_heartbeat.0) / 1000;
            let _ = writeln!(
                out,
                "velstra_node_heartbeat_age_seconds{{node=\"{}\"}} {age}",
                n.meta.name.id()
            );
        }
        let _ = writeln!(out, "# TYPE velstra_node_memory_mib gauge");
        for n in &nodes {
            let _ = writeln!(
                out,
                "velstra_node_memory_mib{{node=\"{}\",kind=\"capacity\"}} {}",
                n.meta.name.id(),
                n.status.capacity.memory_mib
            );
            let _ = writeln!(
                out,
                "velstra_node_memory_mib{{node=\"{}\",kind=\"allocated\"}} {}",
                n.meta.name.id(),
                n.status.allocated.memory_mib
            );
        }

        let pools: Vec<Resource<PoolSpec, PoolStatus>> = self.typed_list("", "pools").await?;
        let _ = writeln!(out, "# TYPE velstra_pool_gib gauge");
        for p in &pools {
            for (kind, v) in [("capacity", p.status.capacity_gib), ("allocated", p.status.allocated_gib)] {
                let _ = writeln!(
                    out,
                    "velstra_pool_gib{{pool=\"{}\",kind=\"{kind}\"}} {v}",
                    p.meta.name.id()
                );
            }
        }

        // Guests by state, cell-wide — and, the alertable one, how many are
        // not in the state they were asked for.
        let instances: Vec<Instance> = self.typed_list("", "instances").await?;
        let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
        let mut drifting = 0u64;
        for i in &instances {
            *by_state.entry(format!("{:?}", i.status.state)).or_default() += 1;
            let wants_running =
                i.spec.desired_state == velstra_cloud_model::resources::DesiredState::Running;
            let is_running =
                i.status.state == velstra_cloud_model::resources::InstanceState::Running;
            if wants_running != is_running && i.meta.deleted_at.is_none() {
                drifting += 1;
            }
        }
        let _ = writeln!(out, "# TYPE velstra_instances gauge");
        for (state, n) in &by_state {
            let _ = writeln!(out, "velstra_instances{{state=\"{state}\"}} {n}");
        }
        let _ = writeln!(out, "# TYPE velstra_instances_off_desired_state gauge");
        let _ = writeln!(out, "velstra_instances_off_desired_state {drifting}");

        // The store's high-water mark: rising fast is the alarm the quota
        // incident would have fired.
        if let Ok(rev) = self.inner.store.revision().await {
            let _ = writeln!(out, "# TYPE velstra_store_revision counter");
            let _ = writeln!(out, "velstra_store_revision {}", rev.0);
        }
        Ok(out)
    }

    /// What maintenance is planned for one node, and what it will cost.
    ///
    /// The question an operator asks *before* the window opens: is anything
    /// scheduled, will the guests move, and — the one that matters — which of
    /// them cannot. A guest holding a passed-through device is bound to this
    /// machine and will be stopped rather than moved, and finding that out
    /// while the machine is on a trolley is finding it out too late.
    pub async fn explain_maintenance(
        &self,
        name: &ResourceName,
        who: &Identity,
    ) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        let node: Node = self.typed(name).await?;
        let here = node.meta.name.id().to_string();
        let now = velstra_cloud_model::meta::Timestamp::now();

        let windows: Vec<velstra_cloud_model::resources::MaintenanceWindow> =
            self.typed_list("", "maintenance-windows").await?;
        let views: Vec<_> = windows.iter().map(window_view).collect();
        let open = velstra_cloud_model::maintenance::open_on(&here, &views, now);
        let next = velstra_cloud_model::maintenance::next_on(&here, &views, now);

        let describe = |w: &velstra_cloud_model::maintenance::WindowView| {
            json!({
                "window": w.name,
                "startsAt": w.starts_at.0,
                "endsAt": w.ends_at().0,
                "minutes": w.minutes,
                "drain": w.drain,
                "note": w.note,
                "opensInMinutes": velstra_cloud_model::maintenance::opens_in_minutes(w, now),
            })
        };

        // What the drain would cost, computed whether or not one is running:
        // the answer is only useful *before* somebody commits to the window,
        // and after it has opened it is too late to be told.
        let draining = open.map(|w| w.drain).unwrap_or(false)
            || next.map(|w| w.drain).unwrap_or(false)
            || node.spec.evacuate;
        let (going, stranded) = if draining {
            let all: Vec<Instance> = self.typed_list("", "instances").await?;
            let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
            let mine: Vec<&Instance> = all
                .iter()
                .filter(|i| {
                    i.status.node.as_deref() == Some(here.as_str())
                        && i.status.state == velstra_cloud_model::resources::InstanceState::Running
                        && !i.meta.is_deleting()
                })
                .collect();
            let others: Vec<&Node> = nodes
                .iter()
                .filter(|n| n.meta.name.id() != here && !n.meta.is_deleting())
                .collect();
            let cached = |image: &str| velstra_cloud_model::resources::nodes_holding(image, &nodes);
            let migrations: Vec<
                velstra_cloud_model::resources::Resource<
                    velstra_cloud_model::migration::MigrationSpec,
                    velstra_cloud_model::migration::MigrationStatus,
                >,
            > = self.typed_list("", "migrations").await?;
            let moving: Vec<String> = migrations
                .iter()
                .filter(|m| !m.meta.is_deleting())
                .map(|m| m.spec.instance.clone())
                .collect();
            velstra_cloud_model::migration::evacuate(&node, &mine, &others, &cached, &moving)
        } else {
            (Vec::new(), Vec::new())
        };

        Ok(json!({
            "node": here,
            "open": open.map(&describe),
            "next": next.map(&describe),
            // Whether the guests are being asked to leave right now, from
            // either source: the switch an operator flipped, or a window that
            // is open with `drain` set.
            "draining": open.map(|w| w.drain).unwrap_or(false) || node.spec.evacuate,
            "willMove": going.iter().map(|h| json!({
                "instance": h.instance,
                "to": h.to_node,
            })).collect::<Vec<_>>(),
            // The half that decides whether tonight goes well. A guest that
            // cannot move will be stopped when the machine is, and the reason
            // is on each line rather than in a footnote.
            "cannotMove": stranded.iter().map(|s| json!({
                "instance": s.instance,
                // Every node's verdict, not a flattened "no host found": the
                // remedy for "node-b is a generation too old" and the remedy
                // for "it holds a GPU" are nothing like each other.
                "why": s.refusals.iter().map(|(node, why)| json!({
                    "node": node,
                    "detail": why.to_string(),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))
    }

    async fn refuse_a_disk_that_is_not_free(&self, spec: &Value) -> ApiResult<()> {
        let Some(osds) = spec.get("osds").and_then(Value::as_array) else {
            return Ok(());
        };
        if osds.is_empty() {
            return Ok(());
        }
        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        for (at, osd) in osds.iter().enumerate() {
            let (Some(node), Some(device)) = (
                osd.get("node").and_then(Value::as_str),
                osd.get("device").and_then(Value::as_str),
            ) else {
                continue;
            };
            let Some(reporting) = nodes.iter().find(|n| n.meta.name.id() == node) else {
                continue;
            };
            // Already an OSD is the answer "this one is doing the job", not a
            // refusal — and it is the ordinary state of every disk in a cluster
            // that is already up, so refusing it would make a settled spec
            // un-editable.
            let Some(disk) = reporting
                .status
                .devices
                .iter()
                .find(|d| d.path == device || d.kernel_name == device)
            else {
                continue;
            };
            if matches!(disk.state, velstra_cloud_model::ceph::DeviceUse::Osd { .. }) {
                continue;
            }
            if let Err(why) = velstra_cloud_model::ceph::may_consume(disk) {
                return Err(
                    ApiError::invalid(format!("{node} will not give up {device}: {why}"))
                        .at(format!("spec.osds[{at}].device")),
                );
            }
        }
        Ok(())
    }

    async fn refuse_a_new_source(&self, name: &ResourceName, spec: &Value) -> ApiResult<()> {
        let asked = |field: &str| -> Option<Option<String>> {
            spec.get(field).map(|value| match value {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
        };
        let (snapshot, image) = (asked("source_snapshot"), asked("source_image"));
        if snapshot.is_none() && image.is_none() {
            return Ok(());
        }
        let stored: Volume = self.typed(name).await?;

        // Sending back what is already there is not a change. A client that
        // read an object and is writing part of it back must not be refused for
        // carrying a field it did not touch.
        if let Some(asked) = snapshot
            && asked != stored.spec.source_snapshot
        {
            let what = asked.unwrap_or_else(|| "nothing".to_string());
            return Err(ApiError::invalid(format!(
                "{name} cannot be restored in place from {what}: writing a snapshot back over the \
                 volume it came from would overwrite whatever a guest has open, and asking for it \
                 again — which every resync does — would undo everything written since. Create a \
                 volume with spec.sourceSnapshot set to the copy you want and attach that instead"
            ))
            .at("spec.sourceSnapshot"));
        }
        if let Some(asked) = image
            && asked != stored.spec.source_image
        {
            return Err(ApiError::invalid(format!(
                "{name} was cloned when it was created, and spec.sourceImage says what from; \
                 changing it now re-clones nothing and would describe a volume that does not \
                 exist — make a new volume from the image you want"
            ))
            .at("spec.sourceImage"));
        }
        Ok(())
    }

    /// A volume's pool is where its bytes are, and re-pointing it moves none.
    ///
    /// This was accepted until now, and what it produced was worse than a
    /// refusal. A pool agent watches for `spec.pool == its own name` and claims
    /// what it finds by writing `status.pool`. Change the spec and the old
    /// pool's filter stops matching, so it lets go without being asked; the new
    /// pool's filter matches, sees a volume another pool still has claimed, and
    /// declines to touch something that is not its own. The volume then sits
    /// there converging on nothing, its bytes intact and unmanaged on a disk
    /// nobody is looking at any more, with no condition, no event and no log
    /// line saying so — because from every component's point of view it did
    /// exactly the right thing.
    ///
    /// So it is refused here, where the answer is still a form somebody has
    /// open, and the refusal names the way that does work.
    /// A volume whose pool this cell does not have would never become real.
    ///
    /// Shape is not enough here. A pool agent watches for volumes naming *its*
    /// id, so a volume naming a pool that is not there is claimed by nobody: it
    /// sits with an empty status and `provisioned: false` for ever, and there is
    /// nothing on the object, in any log, or in any answer to say why. It is the
    /// quietest way this platform can fail, and the fix is one list at the
    /// moment somebody is still asking.
    ///
    /// Named in the refusal, because the usual cause is a spelling — `local`
    /// against `pools/local` — and a message that says which pools exist ends
    /// the guessing.
    /// Choose the pool for a volume whose caller named none, and write it down.
    ///
    /// Which pool holds a volume's bytes is the platform's business, not the
    /// customer's — no tenant of any large provider names a storage pool, and
    /// this one *could not*: pools are the cell's own, refused to a tenant's
    /// list, so the form asked for a name the caller had no way to learn.
    ///
    /// Worse than the awkwardness was the failure. An **empty** pool slipped
    /// past the wrong-pool guard, matched no pool agent's filter, and the
    /// volume sat unprovisioned for ever with an empty status — the quietest
    /// failure this platform has, reachable by leaving a field blank.
    ///
    /// The choice: among pools that are accepting, the one with the most room
    /// left. Settled **at create and stored**, like a family reference or a
    /// backup's node, so the object records where its bytes are and the answer
    /// cannot drift with the pool population. An operator who wants a specific
    /// pool still names it, and is still checked against what exists.
    /// The VNI, the MTU, and the CIDR nobody should have to choose.
    ///
    /// A tenant of any provider clicks "new network" and gets one. Here they were
    /// asked for a **VXLAN network identifier** — a number whose only correct
    /// value is "one nothing else in this cell uses", which a tenant cannot know
    /// and has no business knowing. The model's own comment says as much: "the
    /// VNI on the Velstra fabric, assigned by the controller from the cell's
    /// range, never chosen by a tenant." The form asked anyway.
    ///
    /// Chosen from what exists rather than from a counter: a counter is state to
    /// keep, to lose, and to disagree with reality after a restore. The smallest
    /// free number above the floor is a fact about the cell, recomputed every
    /// time and correct after anything.
    /// Create one object the platform decided on, with no authorisation and no
    /// quota check.
    ///
    /// Both omissions are deliberate and both are the point. The caller has
    /// already been authorised for the thing they *asked* for; a default network
    /// made on their behalf is the platform's own act, and asking whether they
    /// may create a network would refuse a viewer their machine's NIC. The quota
    /// is counted over what a project holds, and these are not what a customer
    /// meant to spend it on.
    async fn make(&self, name: &str, kind: &str, spec: Value) -> ApiResult<()> {
        self.make_marked(None, name, kind, spec).await
    }

    /// The same, marked as made *for* one object.
    ///
    /// The mark is what lets it be collected again. A port minted for a guest is
    /// the platform's own object: nobody asked for it, nobody knows its name,
    /// and when the guest goes there is nothing left that would ever think to
    /// remove it. Six of them accumulated in one project over an afternoon of
    /// testing, each holding an address, and — until the datapath learned to
    /// take things away — each leaving a gateway behind on a bridge.
    ///
    /// Same label the disk controller uses, for the same reason: only work this
    /// platform made is work this platform may remove.
    async fn make_for(
        &self,
        owner: &ResourceName,
        name: &str,
        kind: &str,
        spec: Value,
    ) -> ApiResult<()> {
        self.make_marked(Some(owner), name, kind, spec).await
    }

    async fn make_marked(
        &self,
        owner: Option<&ResourceName>,
        name: &str,
        kind: &str,
        spec: Value,
    ) -> ApiResult<()> {
        let parsed = ResourceName::parse(name)?;
        let mut object = Meta::new(parsed, self.inner.placement.clone());
        if let Some(owner) = owner {
            object.labels.insert(
                velstra_cloud_model::resources::MINTED_FOR.to_string(),
                owner.to_string(),
            );
        }
        let meta = serde_json::to_value(object).expect("meta always serialises");
        match self.collection(kind)?.create(meta, spec).await {
            Ok(_) => Ok(()),
            // Two guests created at once both find no default network and both
            // make one. The loser takes the winner's, which is the right answer
            // and the reason this is not a transaction.
            Err(e) if e.code == Code::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Give a guest with no ports a wire, making the project's default network
    /// if it has none.
    ///
    /// The largest gap between this platform and one somebody would buy. A
    /// customer who wanted one machine had to create a **network**, then a
    /// **subnet** on it, then a **port** on that, in that order, and only then
    /// the guest — four objects and a dependency order, none of which they asked
    /// about. Every provider hands you a default network and puts the NIC on the
    /// machine; the parts stay there for whoever needs them.
    ///
    /// So: no ports named means "give it the usual one". The default network,
    /// its subnet and the port are made once per project, and after that reused
    /// — a second guest joins the first one's network, which is what makes two
    /// machines in a project able to talk without anybody configuring anything.
    ///
    /// A guest that genuinely wants no network says so by sending `ports: []`.
    /// Saying nothing and saying "none" are different requests, and only the
    /// **sent** body can tell them apart: the parsed spec renders both as an
    /// empty list. So the raw body is what is asked, and a field nobody invented
    /// carries the meaning.
    async fn settle_default_network(
        &self,
        guest: &ResourceName,
        parent: &str,
        sent: Option<&Value>,
        spec: &mut Value,
    ) -> ApiResult<()> {
        if parent.is_empty() {
            return Ok(());
        }
        let named_ports = sent
            .and_then(|s| s.get("ports"))
            .and_then(Value::as_array)
            .is_some_and(|p| !p.is_empty());
        let asked = spec
            .get("networks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if !asked.is_empty() {
            if named_ports {
                return Err(ApiError::invalid(
                    "name networks or name ports, not both: they are two answers to one \
                     question, and picking one silently is how a machine ends up on a network \
                     nobody asked for",
                )
                .at("spec.networks"));
            }
            let mut minted = Vec::new();
            for entry in &asked {
                let Some(network) = entry.as_str().filter(|n| !n.is_empty()) else {
                    return Err(ApiError::invalid(
                        "a network is named, as in `projects/p1/networks/default`",
                    )
                    .at("spec.networks"));
                };
                minted.push(Value::String(self.mint_a_port_on(guest, parent, network).await?));
            }
            spec["ports"] = Value::Array(minted);
            // Consumed: what gets stored is `ports`. Two fields describing one
            // set of interfaces is two fields that drift.
            spec["networks"] = json!([]);
            return Ok(());
        }
        spec["networks"] = json!([]);

        // Named some: theirs. Named none at all: ours. Named an empty list: a
        // guest on no network, which the console already warns about.
        match sent.and_then(|s| s.get("ports")).and_then(Value::as_array) {
            Some(named) if !named.is_empty() => return Ok(()),
            Some(_) => return Ok(()),
            None => {}
        }

        let network = format!("{parent}/networks/default");
        let subnet = format!("{parent}/subnets/default");

        if self
            .collection("networks")?
            .get(&network)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            let mut net = json!({ "mtu": 1450, "vni": 0 });
            self.settle_network(&mut net).await?;
            let vni = net["vni"].as_u64().unwrap_or(5000);
            self.make(&network, "networks", net).await?;
            // A range that is this project's and nobody else's. Derived from the
            // VNI rather than counted, so two cells restoring from a backup do
            // not hand out the same /24 to different tenants.
            let cidr = format!("10.{}.{}.0/24", (vni / 256) % 256, vni % 256);
            let gateway = cidr.replace(".0/24", ".1");
            self.make(
                &subnet,
                "subnets",
                json!({
                    "network": network,
                    "cidr": cidr,
                    "gateway": gateway,
                    "dns": [],
                    "reserved": []
                }),
            )
            .await?;
        }

        let port = format!("{parent}/ports/{}", minted("ports"));
        self.make_for(
            guest,
            &port,
            "ports",
            json!({ "network": network, "subnet": subnet, "security_groups": [] }),
        )
        .await?;
        spec["ports"] = json!([port]);
        Ok(())
    }

    /// One port where a guest was asked to be: a network, or one of its subnets.
    ///
    /// Two spellings for one question, the same way `image` takes a family or a
    /// concrete build. A network is what most people mean — "put it on my
    /// network" — and it is a complete answer exactly when that network has one
    /// subnet. A subnet is the answer when it does not, and it is the more
    /// precise one: a subnet is where the range an address comes out of lives.
    ///
    /// Whichever is named has to be this project's. `make` does not authorise —
    /// it is for objects the platform decided on — so a name from somewhere else
    /// would mint a port in a stranger's project on their behalf, which is the
    /// whole hole. Checked here rather than trusted.
    async fn mint_a_port_on(
        &self,
        guest: &ResourceName,
        parent: &str,
        asked: &str,
    ) -> ApiResult<String> {
        let subnets: Vec<velstra_cloud_model::resources::Subnet> =
            self.typed_list(parent, "subnets").await?;
        let alive = |s: &&velstra_cloud_model::resources::Subnet| s.meta.deleted_at.is_none();

        // A subnet, named directly. Its network is the subnet's own — asking for
        // both would be asking the same question twice and inviting them to
        // disagree.
        if asked.starts_with(&format!("{parent}/subnets/")) {
            let Some(subnet) = subnets.iter().filter(alive).find(|s| s.meta.name.to_string() == asked) else {
                return Err(ApiError::new(
                    Code::FailedPrecondition,
                    format!("there is no subnet called `{asked}`"),
                )
                .at("spec.networks"));
            };
            return self
                .port_on(guest, parent, &subnet.spec.network, &subnet.meta.name.to_string())
                .await;
        }

        let network = asked;
        if !network.starts_with(&format!("{parent}/networks/")) {
            return Err(ApiError::invalid(format!(
                "`{network}` is neither a network nor a subnet of this project. A guest can \
                 only be put on a network its own project holds."
            ))
            .at("spec.networks"));
        }
        if self
            .collection("networks")?
            .get(network)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!("there is no network called `{network}`"),
            )
            .at("spec.networks"));
        }

        // The subnet is what carries the range an address comes out of, and a
        // network without one can hold a port that never gets an address —
        // a guest that boots with a dead NIC and no sign of why.
        let mut on_it = subnets
            .iter()
            .filter(alive)
            .filter(|s| s.spec.network == network);
        let Some(subnet) = on_it.next() else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "`{network}` has no subnet, so a port on it could never be given an \
                     address. Add a subnet to it first."
                ),
            )
            .at("spec.networks"));
        };
        // More than one, and the network is no longer a complete answer. This
        // used to take whichever came first by name — an address out of a range
        // nobody chose, decided silently, which is the same thing this create
        // refuses when both `networks` and `ports` are named.
        if let Some(second) = on_it.next() {
            let mut all: Vec<String> = vec![
                format!("{} ({})", subnet.meta.name, subnet.spec.cidr),
                format!("{} ({})", second.meta.name, second.spec.cidr),
            ];
            all.extend(on_it.map(|s| format!("{} ({})", s.meta.name, s.spec.cidr)));
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!(
                    "`{network}` has more than one subnet, so naming it does not say which \
                     range this guest's address comes out of. Name the subnet instead: {}",
                    all.join(", ")
                ),
            )
            .at("spec.networks"));
        }
        let subnet = subnet.meta.name.to_string();
        self.port_on(guest, parent, network, &subnet).await
    }

    /// The port itself, once it is settled which network and which subnet.
    async fn port_on(
        &self,
        guest: &ResourceName,
        parent: &str,
        network: &str,
        subnet: &str,
    ) -> ApiResult<String> {

        let port = format!("{parent}/ports/{}", minted("ports"));
        self.make_for(
            guest,
            &port,
            "ports",
            json!({
                "network": network,
                "subnet": subnet,
                "security_groups": []
            }),
        )
        .await?;
        Ok(port)
    }

    /// A binding may only name a role that exists.
    ///
    /// The strict half of a deliberate pair. The *read* path is lenient — a name
    /// that is not a rung reads as `viewer`, and a `roles/…` nobody defined
    /// grants nothing — because a typo must land on the least and never on the
    /// most, and because a role deleted under a project must not refuse every
    /// request in it. Neither of those tells anybody about their typo.
    ///
    /// This is where they are told: at the door, naming the field, while they
    /// still have the tab open.
    async fn refuse_a_role_nobody_defined(&self, spec: &Value) -> ApiResult<()> {
        use velstra_cloud_model::authz::CUSTOM_ROLE_PREFIX;

        let Some(bindings) = spec.get("bindings").and_then(Value::as_array) else {
            return Ok(());
        };
        for (i, binding) in bindings.iter().enumerate() {
            let named = binding.get("role").and_then(Value::as_str).unwrap_or_default();
            if !named.starts_with(CUSTOM_ROLE_PREFIX) {
                continue;
            }
            let known = self
                .collection("roles")?
                .get(named)
                .await?
                .is_some();
            if !known {
                return Err(ApiError::new(
                    Code::FailedPrecondition,
                    format!(
                        "there is no role called `{named}`. A binding naming one grants nothing \
                         at all, which reads exactly like a grant that was never made."
                    ),
                )
                .at(format!("spec.bindings[{i}].role")));
            }
        }
        Ok(())
    }

    /// A parent has to be a folder, and it has to be one that is there.
    ///
    /// Either spelling arrives: `engineering` — which is how the console writes
    /// every cell-scoped reference, and how a node and a device class are named
    /// — or `folders/engineering`, which is what the field has claimed to hold
    /// since before anything read it. [`settle_parent`] makes them one before
    /// this runs, so what is stored is always the full name and the model's own
    /// gate (`hierarchy::folder_above`) still catches a `parent` that names
    /// something which is not a folder at all.
    ///
    /// `parent` used to be a free string nothing read — the field said it named
    /// "`organizations/o1` or `folders/f2`" and the platform walked nothing, so
    /// any text at all was as good as any other. Now that it decides who may do
    /// what, a value that names nothing is a grant that silently does not apply,
    /// and a loop is a chain whose top is wherever the depth bound happens to
    /// fall.
    async fn refuse_a_parent_that_cannot_be_one(
        &self,
        kind: &str,
        name: &ResourceName,
        spec: &Value,
    ) -> ApiResult<()> {
        use velstra_cloud_model::hierarchy::folder_above;

        let parent = spec.get("parent").and_then(Value::as_str).unwrap_or("");
        if parent.is_empty() {
            return Ok(());
        }
        if folder_above(parent).is_none() {
            return Err(ApiError::invalid(format!(
                "a parent is a folder, as in `folders/engineering` — `{parent}` is not one"
            ))
            .at("spec.parent"));
        }
        if self.typed_folder(parent).await.is_none() {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!("there is no folder called `{parent}`"),
            )
            .at("spec.parent"));
        }
        if kind == "folders" {
            let me = name.to_string();
            let mut seen: Vec<String> = Vec::new();
            let mut here = Some(parent.to_string());
            while let Some(step) = here.and_then(|p| folder_above(&p).map(str::to_string)) {
                if step == me {
                    return Err(ApiError::invalid(
                        "that would put this folder inside itself. A folder above another one \
                         cannot also be below it — the tree is what makes \"who may do this\" \
                         answerable by reading upward once.",
                    )
                    .at("spec.parent"));
                }
                if seen.contains(&step)
                    || seen.len() >= velstra_cloud_model::hierarchy::MAX_DEPTH
                {
                    break;
                }
                seen.push(step.clone());
                here = self.typed_folder(&step).await.map(|f| f.spec.parent);
            }
            if seen.len() >= velstra_cloud_model::hierarchy::MAX_DEPTH {
                return Err(ApiError::invalid(format!(
                    "folders go {} deep and this would be deeper. A permission question is \
                     answered by reading every level above the object, so the depth is what \
                     one of those costs.",
                    velstra_cloud_model::hierarchy::MAX_DEPTH
                ))
                .at("spec.parent"));
            }
        }
        Ok(())
    }

    /// Publish an image that already exists somewhere else.
    ///
    /// `spec.from` names one; everything that describes the bytes is copied from
    /// it and the field is consumed. What this is *for* is the case an operator
    /// had no way to do: a tenant captures a guest, gets an image in their own
    /// project, and the cell wants it in the catalogue for everybody. Before
    /// this, that meant reading the digest, the format, the size and the source
    /// off one object and typing them into another — correctly, or publishing
    /// bytes nobody tested.
    ///
    /// Nothing is copied. An image is content-addressed, so a cell-wide image
    /// with the same digest **is** the same bytes: every node that had them
    /// cached still has them, under that digest, and a guest booting the
    /// published one boots what it booted before.
    ///
    /// Whatever the caller also sent wins over what is copied — a published
    /// image may be given a different family or version, which is exactly what
    /// somebody promoting `our-base` from a project into `debian-13-hardened`
    /// wants. Only what they did not say is taken.
    async fn settle_published_image(&self, spec: &mut Value) -> ApiResult<()> {
        let Some(from) = spec
            .get("from")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .map(str::to_string)
        else {
            return Ok(());
        };
        let name = ResourceName::parse(&from).map_err(ApiError::from)?;
        if name.collection() != "images" {
            return Err(
                ApiError::invalid("`from` names an image to publish, as in \
                                   `projects/p1/images/sha256-3f9a2b`")
                .at("spec.from"),
            );
        }
        let Some(source) = self
            .collection("images")?
            .get(&from)
            .await?
            .and_then(|d| {
                serde_json::from_value::<velstra_cloud_model::resources::Image>(d).ok()
            })
        else {
            return Err(
                ApiError::new(Code::FailedPrecondition, format!("there is no image called `{from}`"))
                    .at("spec.from"),
            );
        };

        // The model's spelling, not the wire's: by the time a spec reaches a
        // settle step the wire layer has already turned `sizeBytes` into
        // `size_bytes`, and a camelCase key here writes a field nothing reads.
        // It fails silently — the image publishes, with a size of zero.
        let copied = json!({
            "digest": source.spec.digest,
            "format": source.spec.format,
            "size_bytes": source.spec.size_bytes,
            "source_url": source.spec.source_url,
            "family": source.spec.family,
            "version": source.spec.version,
            "source_instance": source.spec.source_instance,
        });
        for (key, value) in copied.as_object().expect("an object") {
            // Only what the caller left out. Publishing under a different family
            // is the point of publishing.
            let said = spec.get(key.as_str());
            let empty = matches!(said, None | Some(Value::Null))
                || said.and_then(Value::as_str) == Some("")
                || said.and_then(Value::as_u64) == Some(0);
            if empty && !value.is_null() {
                spec[key.as_str()] = value.clone();
            }
        }
        spec["from"] = Value::String(String::new());
        Ok(())
    }

    async fn settle_network(&self, spec: &mut Value) -> ApiResult<()> {
        const FIRST_VNI: u32 = 5000;
        if spec.get("mtu").and_then(Value::as_u64).unwrap_or(0) == 0 {
            // 1450, not 1500: a VXLAN header is 50 bytes, and a tenant network
            // handed the wire's own MTU black-holes every large packet in a way
            // that looks like an application bug for a week.
            spec["mtu"] = json!(1450);
        }
        if spec.get("vni").and_then(Value::as_u64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let taken: std::collections::BTreeSet<u32> = self
            .typed_list::<velstra_cloud_model::resources::NetworkSpec,
                          velstra_cloud_model::resources::NetworkStatus>("", "networks")
            .await?
            .iter()
            .map(|n| n.spec.vni)
            .collect();
        let vni = (FIRST_VNI..).find(|v| !taken.contains(v)).unwrap_or(FIRST_VNI);
        spec["vni"] = json!(vni);
        Ok(())
    }

    /// Answer a floating IP's two questions the way a customer asks them.
    ///
    /// **The guest, not the port.** `spec.instance` names a VM; the API
    /// resolves its port and stores that, the same way an instance's
    /// `networks` becomes `ports`. Naming both is refused rather than merged.
    /// A guest with two NICs is asked which, by name — silently picking one
    /// would put a public address in front of an interface nobody chose.
    ///
    /// **The pool, not the subnet.** Left empty, `spec.subnet` settles to one
    /// of the cell's public subnets — an external network at cell scope, which
    /// is what an operator declares a pool *as*. IPv4 wins a tie because it is
    /// what "give my VM a public IP" means until somebody says `v6`; naming
    /// the subnet is how they say it.
    async fn settle_floating_ip(&self, parent: &str, spec: &mut Value) -> ApiResult<()> {
        let named_instance = spec
            .get("instance")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !named_instance.is_empty() {
            if spec.get("port").and_then(Value::as_str).is_some_and(|p| !p.is_empty()) {
                return Err(ApiError::invalid(
                    "name instance or name port, not both: they are two answers to one \
                     question, and picking one silently is how an address ends up in front \
                     of an interface nobody chose",
                )
                .at("spec.instance"));
            }
            if !named_instance.starts_with(&format!("{parent}/instances/")) {
                return Err(ApiError::invalid(format!(
                    "`{named_instance}` is not an instance of this project. A public address \
                     can only be put in front of a guest its own project holds."
                ))
                .at("spec.instance"));
            }
            let instance: Instance = self
                .typed(&ResourceName::parse(&named_instance)?)
                .await
                .map_err(|e| {
                    if e.code == Code::NotFound {
                        ApiError::new(
                            Code::FailedPrecondition,
                            format!("there is no instance called `{named_instance}`"),
                        )
                        .at("spec.instance")
                    } else {
                        e
                    }
                })?;
            let port = match instance.spec.ports.as_slice() {
                [] => {
                    return Err(ApiError::new(
                        Code::FailedPrecondition,
                        format!(
                            "`{named_instance}` has no network interface, so there is nothing \
                             to put an address in front of"
                        ),
                    )
                    .at("spec.instance"));
                }
                [one] => one.clone(),
                many => {
                    return Err(ApiError::new(
                        Code::FailedPrecondition,
                        format!(
                            "`{named_instance}` has {} interfaces, so naming it does not say \
                             which one the address fronts. Name the port instead: {}",
                            many.len(),
                            many.join(", ")
                        ),
                    )
                    .at("spec.instance"));
                }
            };
            spec["port"] = Value::String(port);
            // Consumed: what is stored is `port`.
            spec["instance"] = Value::String(String::new());
        }

        let named_subnet = spec
            .get("subnet")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !named_subnet.is_empty() {
            return Ok(());
        }
        let networks: Vec<Resource<NetworkSpec, NetworkStatus>> =
            self.typed_list("", "networks").await?;
        let public: Vec<String> = networks
            .iter()
            .filter(|n| n.spec.external && n.meta.deleted_at.is_none())
            .map(|n| n.meta.name.to_string())
            .collect();
        let subnets: Vec<velstra_cloud_model::resources::Subnet> =
            self.typed_list("", "subnets").await?;
        let mut candidates: Vec<&velstra_cloud_model::resources::Subnet> = subnets
            .iter()
            .filter(|s| s.meta.deleted_at.is_none() && public.contains(&s.spec.network))
            .collect();
        // IPv4 first, then by name, so the same cell always answers the same.
        candidates.sort_by_key(|s| (s.spec.cidr.contains(':'), s.meta.name.to_string()));
        let Some(chosen) = candidates.first() else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                "this cell offers no public addresses: no external network with a subnet \
                 exists at cell scope. A cell operator declares one — a network with \
                 `external: true` and a subnet carrying a real prefix.",
            )
            .at("spec.subnet"));
        };
        spec["subnet"] = Value::String(chosen.meta.name.to_string());
        Ok(())
    }

    async fn settle_volume_pool(&self, spec: &mut Value) -> ApiResult<()> {
        let named = spec.get("pool").and_then(Value::as_str).unwrap_or_default();
        if !named.is_empty() {
            return Ok(());
        }
        let pools: Vec<Resource<PoolSpec, PoolStatus>> = self.typed_list("", "pools").await?;
        let chosen = pools
            .iter()
            .filter(|p| p.spec.accepting && p.meta.deleted_at.is_none())
            .max_by_key(|p| {
                p.status
                    .capacity_gib
                    .saturating_sub(p.status.allocated_gib)
            });
        let Some(pool) = chosen else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                "no storage pool is accepting volumes, so there is nowhere to put this \
                 one. A cell operator brings a pool up or sets `accepting` on one that \
                 exists.",
            )
            .at("spec.pool"));
        };
        spec["pool"] = Value::String(pool.meta.name.id().to_string());
        Ok(())
    }

    async fn refuse_a_pool_this_cell_does_not_have(&self, spec: &Value) -> ApiResult<()> {
        let Some(asked) = spec.get("pool").and_then(Value::as_str) else {
            return Ok(());
        };
        if asked.is_empty() {
            return Ok(());
        }
        let pools: Vec<Resource<PoolSpec, PoolStatus>> = self.typed_list("", "pools").await?;
        let ids: Vec<String> = pools.iter().map(|p| p.meta.name.id().to_string()).collect();
        if let Some(pool) = pools.iter().find(|p| p.meta.name.id() == asked) {
            // The pool exists — does it have the room? Refused here, before a
            // byte moves, for the same reason a migration is: the far end
            // refusing after the object exists is the same refusal, later and
            // quieter. On a real cell a volume on a full pool sat unprovisioned
            // with `lvcreate: insufficient free space` repeating in a journal
            // on another machine — an answer, in a place nobody was looking.
            //
            // Only for a pool that has *reported*: one whose agent has not
            // spoken yet has capacity 0 because nothing is known, and refusing
            // every volume until the first heartbeat would make a freshly
            // registered pool unusable for no stated reason.
            let asked_gib = spec.get("size_gib").and_then(Value::as_u64).unwrap_or(0);
            let has_reported = !pool.status.backend.is_empty();
            let free = pool
                .status
                .capacity_gib
                .saturating_sub(pool.status.allocated_gib);
            if has_reported && asked_gib > free {
                return Err(ApiError::new(
                    Code::FailedPrecondition,
                    format!(
                        "`{asked}` has {free} GiB left and this volume wants {asked_gib} GiB. \
                         Nothing was created: a volume on a pool that cannot hold it would sit \
                         unprovisioned for ever."
                    ),
                )
                .at("spec.size_gib"));
            }
            return Ok(());
        }
        if ids.iter().any(|id| id == asked) {
            return Ok(());
        }
        Err(ApiError::new(
            Code::FailedPrecondition,
            if ids.is_empty() {
                format!(
                    "there is no storage pool called `{asked}` — this cell has no pools at all. \
                     A volume names the pool that will hold its bytes, and one nothing holds is \
                     never made."
                )
            } else {
                format!(
                    "there is no storage pool called `{asked}`. This cell has {}. A pool is \
                     named by its id, not by its resource name.",
                    ids.iter()
                        .map(|i| format!("`{i}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        )
        .at("spec.pool"))
    }

    /// Turn `families/debian-13` into the image it means, once, here.
    ///
    /// The resolution happens **at create time and is written down**, so the
    /// object records the concrete image it was built from. A family reference
    /// that stayed a family reference would be a guest whose operating system
    /// changed on the next restart, at a moment nobody chose — which is the
    /// opposite of what somebody asking for "the newest" wants. What they want
    /// is that *new* machines get the newest, and that is what this does.
    ///
    /// The scope is the caller's: a project's own family beats the cell's, so a
    /// tenant who publishes `debian-13` of their own gets theirs, and a tenant
    /// who does not gets the catalogue's. Anything else would let one project's
    /// choice of name decide what another project boots.
    async fn settle_image_family(&self, parent: &str, spec: &mut Value) -> ApiResult<()> {
        let Some(asked) = spec.get("image").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(family) = asked.strip_prefix(velstra_cloud_model::resources::FAMILY_PREFIX) else {
            return Ok(());
        };
        let family = family.to_string();
        if family.is_empty() {
            return Err(ApiError::invalid(
                "a family reference names the family after the prefix, as in `families/debian-13`",
            )
            .at("spec.image"));
        }

        let mut candidates: Vec<velstra_cloud_model::resources::Image> = Vec::new();
        if !parent.is_empty() {
            candidates.extend(self.typed_list(parent, "images").await?);
        }
        candidates.extend(self.typed_list("", "images").await?);

        // The project's own first, then the cell's — `newest_of_family` orders by
        // age, so the two lists are asked separately rather than merged.
        let chosen = if parent.is_empty() {
            velstra_cloud_model::resources::newest_of_family(candidates.iter(), &family).cloned()
        } else {
            let mine: Vec<_> = candidates
                .iter()
                .filter(|i| i.meta.name.to_string().starts_with(&format!("{parent}/")))
                .collect();
            velstra_cloud_model::resources::newest_of_family(mine, &family)
                .cloned()
                .or_else(|| {
                    let theirs: Vec<_> = candidates
                        .iter()
                        .filter(|i| !i.meta.name.to_string().starts_with("projects/"))
                        .collect();
                    velstra_cloud_model::resources::newest_of_family(theirs, &family)
                        .cloned()
                })
        };

        let Some(image) = chosen else {
            let mut families: Vec<String> = candidates
                .iter()
                .filter(|i| !i.spec.family.is_empty())
                .map(|i| i.spec.family.clone())
                .collect();
            families.sort();
            families.dedup();
            return Err(ApiError::new(
                Code::FailedPrecondition,
                if families.is_empty() {
                    format!(
                        "there is no image family called `{family}`, and no image here declares a \
                         family at all. Publish one with `spec.family` set, or name an image by \
                         its digest."
                    )
                } else {
                    format!(
                        "there is no image family called `{family}`. This cell offers {}.",
                        families
                            .iter()
                            .map(|f| format!("`{f}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            )
            .at("spec.image"));
        };

        spec["image"] = Value::String(image.meta.name.to_string());
        Ok(())
    }

    async fn refuse_a_moved_pool(&self, name: &ResourceName, spec: &Value) -> ApiResult<()> {
        let Some(asked) = spec.get("pool").and_then(Value::as_str) else {
            return Ok(());
        };
        let stored: Volume = self.typed(name).await?;
        // Writing back what is already there is not a change — the same rule
        // every other field check here follows, so that a client echoing an
        // object it read is never refused for a field it did not touch.
        if asked == stored.spec.pool {
            return Ok(());
        }
        Err(ApiError::invalid(format!(
            "{name} has its bytes in {}, and changing spec.pool moves none of them: the \
             pool that has it would stop watching and the named pool would decline a \
             volume it does not own, leaving this converging on nothing. To put it on \
             {asked}: back it up to a target, create a volume there with spec.sourceBackup \
             set to that copy, point whatever uses this at the new one, and delete this",
            stored.spec.pool
        ))
        .at("spec.pool"))
    }

    // ---- migration --------------------------------------------------------

    /// Fill in where the guest is now, and refuse a move that cannot work.
    ///
    /// `from_node` is derived from the instance for the same reason an
    /// attachment's node is: it is one fact, the platform has it, and a
    /// migration claiming to start somewhere the guest is not is a migration
    /// whose source agent will never find its own work.
    ///
    /// It is taken from `status.node` — the node's own report — and not from
    /// `spec.node`, which is only where the instance was *assigned*. The two
    /// disagree for as long as a handover takes, and the machine that can send
    /// a guest is the machine that has it. Reading the assignment would name
    /// the wrong source in exactly the case that matters: a second migration
    /// asked for while the first is still in flight.
    ///
    /// The refusal happens **here**, before the object exists, because every
    /// reason a migration cannot work is knowable in advance — and the
    /// alternative is finding out after the memory has been copied, which is
    /// the most expensive moment to fail.
    async fn settle_migration(&self, spec: &mut Value) -> ApiResult<()> {
        let instance_name = spec
            .get("instance")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let to = spec
            .get("to_node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if to.is_empty() {
            return Err(
                ApiError::invalid("a migration says which node the guest should move to")
                    .at("spec.toNode"),
            );
        }

        let name = ResourceName::parse(&instance_name).map_err(|_| {
            ApiError::invalid("a migration names the instance to move").at("spec.instance")
        })?;
        let instance: Instance = self.typed(&name).await?;
        let Some(from) = instance.status.node.clone().filter(|n| !n.is_empty()) else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!("{instance_name} is not on a node, so there is nothing to move it from"),
            )
            .at("spec.instance"));
        };

        // Stated and wrong is refused rather than corrected — the same rule as
        // an attachment's node, and for the same reason.
        match spec.get("from_node").and_then(Value::as_str) {
            Some(said) if !said.is_empty() && said != from => {
                return Err(ApiError::invalid(format!(
                    "{instance_name} is on {from}, not on {said}"
                ))
                .at("spec.fromNode"));
            }
            _ => {}
        }
        spec["from_node"] = Value::String(from.clone());

        let (source, destination) = (self.node(&from).await?, self.node(&to).await?);
        let source = source.ok_or_else(|| {
            ApiError::new(
                Code::FailedPrecondition,
                format!("{from} is not a node this cell knows"),
            )
            .at("spec.fromNode")
        })?;
        let destination = destination.ok_or_else(|| {
            ApiError::new(
                Code::FailedPrecondition,
                format!("{to} is not a node this cell knows"),
            )
            .at("spec.toNode")
        })?;

        let cached = self
            .image_cached_on(&instance.spec.image, &mut Scratch::default())
            .await?;
        // The mode the caller asked for, because it decides one of the
        // refusals: only `Reboot` can carry a guest that holds hardware, and
        // answering as though every migration were live would refuse a move
        // the platform can actually make.
        let mode: velstra_cloud_model::migration::MigrationMode = spec
            .get("mode")
            .and_then(|m| serde_json::from_value(m.clone()).ok())
            .unwrap_or_default();
        if let Err(refusal) = may_migrate(&instance, &source, &destination, &cached, mode) {
            // The field is the control an operator can act on: a destination
            // that cannot receive is a different problem from a guest that is
            // not running.
            let field = match &refusal {
                velstra_cloud_model::migration::Refusal::NotRunning { .. }
                | velstra_cloud_model::migration::Refusal::NotFromThere { .. } => "spec.instance",
                _ => "spec.toNode",
            };
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!("{instance_name} cannot move to {to}: {refusal}"),
            )
            .at(field));
        }
        Ok(())
    }

    /// What this cell has, what is spoken for, and what would actually fit.
    ///
    /// A verb on the node collection, like `:explainCpu`: it is a property of
    /// the fleet, and hanging it off one node would read as that node's answer.
    ///
    /// The field worth pointing at is `largestFit`. Free memory does **not**
    /// add up into a guest — sixty-four gibibytes spread over eight nodes fits
    /// no sixteen-gibibyte guest — and a summary that showed only the sum would
    /// tell somebody a guest fits when it does not. That is the whole reason
    /// this is computed here rather than left to whoever draws the dashboard.
    pub async fn explain_capacity(&self, who: &Identity) -> ApiResult<Value> {
        // The whole fleet, by name — so the question is the same one listing
        // the nodes asks, and it is asked. The comment above `explain_cpu` used
        // to *say* this was authorised as a read of the collection while the
        // code threw `who` away, and a tenant's overview rendered the cell's
        // machine names, free memory and CPU domains. Found by signing in as
        // one.
        self.authorize_for(who, Verb::Read, &ResourceName::parse("nodes/any")?, "nodes")
            .await?;
        let nodes: Vec<velstra_cloud_model::resources::Node> = self.typed_list("", "nodes").await?;
        let h = velstra_cloud_model::reconcile::headroom(&nodes, &self.closed_nodes().await?);

        let cap = |c: &velstra_cloud_model::resources::Capacity| {
            json!({
                "vcpus": c.vcpus,
                "memoryMib": c.memory_mib,
                "diskGib": c.disk_gib,
            })
        };
        Ok(json!({
            "usableNodes": h.usable_nodes,
            // Silicon and promise, side by side. They differ exactly where an
            // operator has set a ratio, and one without the other reads as
            // though the cell had grown a processor.
            "offeredVcpus": h.offered_vcpus,
            // Named rather than folded into a total: "we have twelve nodes"
            // and "eight will take a guest" are different sentences, and the
            // second is the one somebody planning capacity needs.
            "unusableNodes": h.unusable_nodes,
            "total": cap(&h.total),
            "allocated": cap(&h.allocated),
            // Across the usable nodes only, so this and `total` disagree by
            // exactly the drained capacity — on purpose.
            "free": cap(&h.free),
            "largestFit": cap(&h.largest_fit),
        }))
    }

    /// What a project has left, and what it could actually start with it.
    ///
    /// Both halves in one answer, because either alone answers the wrong
    /// question. "24 vCPUs of quota left" is what a tenant reads before
    /// creating a guest that will never be placed; "no valid host" is what they
    /// get afterwards, from a scheduler that knows nothing about quotas. The
    /// binding side is named, because "your quota" and "the cell" are two
    /// different afternoons: one is a message to an operator, the other is
    /// waiting or picking a smaller shape.
    pub async fn explain_quota(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
        // A read of the project, which is exactly what it is — so a tenant sees
        // their own allowance and nobody sees somebody else's.
        self.authorize(who, Verb::Read, name).await?;
        let project: velstra_cloud_model::resources::Resource<ProjectSpec, ProjectStatus> =
            self.typed(name).await?;

        let dimensions = velstra_cloud_model::allowance::dimensions(
            &project.spec.quota,
            // Counted by the quota controller from the objects that exist,
            // never from a running total: a total incremented on create and
            // decremented on delete is wrong the first time either half is
            // missed, and it fails closed.
            &project.status.used,
        );
        let nodes: Vec<velstra_cloud_model::resources::Node> = self.typed_list("", "nodes").await?;
        let room = velstra_cloud_model::reconcile::headroom(&nodes, &self.closed_nodes().await?);
        let startable = velstra_cloud_model::allowance::largest_startable(&dimensions, &room);

        Ok(json!({
            "project": name.to_string(),
            "dimensions": dimensions.iter().map(|d| json!({
                "name": d.name,
                "limit": d.limit,
                "used": d.used,
                // `null` where nobody set a limit, which is not the same
                // answer as zero and must not render as one.
                "left": d.left(),
                "unlimited": d.unlimited(),
                "exhausted": d.exhausted(),
            })).collect::<Vec<_>>(),
            "largestStartable": startable,
        }))
    }

    /// What the fleet's processors look like, and what to do about them.
    ///
    /// One answer rather than three endpoints, because the three parts are only
    /// useful together: the domains say what you have, the advice says what
    /// could change, and the pending list says what the last change is still
    /// working through. An operator asking "can this cell migrate freely" is
    /// asking all three at once.
    ///
    /// Computed on every read and stored nowhere. A cached answer about a fleet
    /// outlives the fleet that justified it, and this one is cheap: it is set
    /// arithmetic over what the nodes already report.
    pub async fn explain_cpu(&self, who: &Identity) -> ApiResult<Value> {
        // Every node in the cell, because a migration domain is a property of
        // the whole set. Authorised as a read of the node collection, which is
        // what it is — and enforced, not just said: see `explain_capacity`.
        self.authorize_for(who, Verb::Read, &ResourceName::parse("nodes/any")?, "nodes")
            .await?;
        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        let entries: Vec<velstra_cloud_model::cpu::NodeEntry> = nodes
            .iter()
            .filter_map(|n| {
                Some(velstra_cloud_model::cpu::NodeEntry {
                    node: n.meta.name.id().to_string(),
                    cpu: n.status.cpu.clone()?,
                })
            })
            .collect();

        // Only running guests have a CPU to be pending about; a stopped one
        // adopts whatever it is given when it next starts, so listing it would
        // be inventing work.
        let instances: Vec<Instance> = self.typed_list("", "instances").await?;
        let guests: Vec<(String, String, velstra_cloud_model::cpu::GuestCpu)> = instances
            .iter()
            .filter_map(|i| {
                Some((
                    i.meta.name.to_string(),
                    i.status.node.clone()?,
                    i.status.cpu.clone()?,
                ))
            })
            .collect();

        let domains = velstra_cloud_model::cpu::migration_domains(&entries);
        let advice = velstra_cloud_model::cpu::advise(&entries, &guests);
        let pending = velstra_cloud_model::cpu::pending_adoption(&guests, &entries);

        Ok(json!({
            // Nodes that have not reported a CPU are named rather than
            // silently dropped: "3 of 5 nodes" with no list is how an operator
            // concludes the report is broken.
            "unreported": nodes
                .iter()
                .filter(|n| n.status.cpu.is_none())
                .map(|n| n.meta.name.id().to_string())
                .collect::<Vec<_>>(),
            "domains": domains.iter().map(|d| json!({
                "nodes": d.nodes,
                "arch": d.arch,
                "level": d.level.map(|l| l.as_str()),
                "canBaseline": d.can_mask,
            })).collect::<Vec<_>>(),
            "advice": advice.iter().map(advice_json).collect::<Vec<_>>(),
            "pendingAdoption": pending.iter().map(|p| json!({
                "instance": p.instance,
                "node": p.node,
                "running": p.running.map(|l| l.as_str()),
                "wouldGet": p.would_get.map(|l| l.as_str()),
            })).collect::<Vec<_>>(),
        }))
    }

    /// Why a guest has, or has not, been brought back from a node that stopped
    /// answering.
    ///
    /// Computed on demand rather than written onto the instance, and that is
    /// not a nicety. The agent on the node owns that status — the access rule
    /// refuses a controller writing it — so a note about recovery could only
    /// exist by having two parties write one object, which is the thing this
    /// platform is built to prevent.
    ///
    /// The same function the recovery controller runs, on the same objects, so
    /// the two can never tell different stories about one guest.
    pub async fn explain_recovery(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        if name.collection() != "instances" {
            return Err(ApiError::invalid("only a guest is recovered"));
        }
        let instance: Instance = self.typed(name).await?;
        let Some(node_name) = instance.spec.node.clone().filter(|n| !n.is_empty()) else {
            return Ok(json!({
                "node": null,
                "recoverable": false,
                "why": "NotPlaced",
                "detail": "it is not on a node, so there is nothing to recover it from",
            }));
        };
        // `spec.node` is a **bare id** — `node-a` — as every node reference in
        // this API is, for the reason `crate::refs` states: a node is a
        // cell-wide object under no parent, so there is no parent to spell.
        // Parsing it as a whole resource name answered `500` for every guest
        // that was actually placed, which is every guest this method exists
        // for. Found by the recorded-shape test on its first run.
        let node: velstra_cloud_model::resources::Node = self
            .typed(
                &ResourceName::parse(&format!("nodes/{node_name}")).map_err(|e| {
                    ApiError::internal(format!(
                        "a guest names {node_name}, which is not an id: {e}"
                    ))
                })?,
            )
            .await?;

        let guest = velstra_cloud_model::ha::GuestView {
            name: name.to_string(),
            on_node_loss: instance.spec.on_node_loss,
            was_running: instance.status.state
                == velstra_cloud_model::resources::InstanceState::Running,
            devices: instance.status.devices.clone(),
            deleting: instance.meta.is_deleting(),
        };
        let view = velstra_cloud_model::ha::NodeView {
            name: node_name.clone(),
            last_heartbeat: node.status.last_heartbeat,
            fence_after_s: node.spec.fence_after_s,
            ready: velstra_cloud_model::meta::condition(&node.status.conditions, "Ready")
                .is_some_and(|c| c.status == velstra_cloud_model::meta::ConditionStatus::True),
        };

        let verdict = velstra_cloud_model::ha::may_recover(
            &guest,
            &view,
            velstra_cloud_model::meta::Timestamp::now(),
            velstra_cloud_model::ha::RECOVERY_MARGIN_S,
        );
        Ok(match verdict {
            Ok(()) => json!({
                "node": node_name,
                "recoverable": true,
                "why": "",
                "detail": "",
            }),
            Err(why) => {
                use velstra_cloud_model::ha::NotRecoverable as N;
                // A stable token beside the sentence, because the four reasons
                // are four different actions and a console branches on which.
                let token = match &why {
                    N::PolicyIsLeave => "PolicyIsLeave",
                    N::NotQuietLongEnough { .. } => "WaitingForFencing",
                    N::NodeDoesNotFence { .. } => "NodeDoesNotFence",
                    N::HoldsDevices { .. } => "HoldsDevices",
                    N::NotRunning => "NotRunning",
                };
                json!({
                    "node": node_name,
                    "recoverable": false,
                    "why": token,
                    "detail": why.to_string(),
                })
            }
        })
    }

    /// Where this guest could go, with a verdict for **every** node.
    ///
    /// Placement and migration ask different questions, so they cannot share an
    /// answer: a scheduler picks, and `place` returns the one it picked; a
    /// person picks, and needs to know about each candidate. `may_migrate` is
    /// per-destination, so this enumerates rather than choosing — and a node
    /// missing from the list means it does not exist, never that it is
    /// undecided.
    pub async fn explain_migration(
        &self,
        name: &ResourceName,
        mode: velstra_cloud_model::migration::MigrationMode,
        who: &Identity,
    ) -> ApiResult<Value> {
        self.authorize(who, Verb::Read, name).await?;
        if name.collection() != "instances" {
            return Err(ApiError::invalid("a migration moves an instance"));
        }
        let instance: Instance = self.typed(name).await?;
        // The report, not the assignment — the same rule the create follows, so
        // the two answers cannot disagree about where a guest is.
        let Some(from) = instance.status.node.clone().filter(|n| !n.is_empty()) else {
            return Err(ApiError::new(
                Code::FailedPrecondition,
                format!("{name} is not on a node, so there is nowhere to move it from"),
            )
            .at("spec.node"));
        };
        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
        let source = nodes
            .iter()
            .find(|n| n.meta.name.id() == from)
            .ok_or_else(|| {
                ApiError::new(
                    Code::FailedPrecondition,
                    format!("{name} is on {from}, which is not a node this cell knows"),
                )
            })?;
        let cached = self
            .image_cached_on(&instance.spec.image, &mut Scratch::default())
            .await?;

        let destinations: Vec<Value> = nodes
            .iter()
            .map(|to| {
                let id = to.meta.name.id();
                // Answered for the mode being asked about, defaulting to
                // `Live` — which is what a console's picker opens on.
                //
                // It used to answer for `Live` and nothing else, which was
                // fine while the two modes refused the same things. They do
                // not: a cold move crosses processors a live one cannot, so a
                // fleet of unlike machines was being told its guests could not
                // move at all when every one of them could, with a restart.
                let verdict = may_migrate(&instance, source, to, &cached, mode);
                let d = velstra_cloud_proto::convert::destination_of(id, verdict.as_ref().err());
                json!({ "node": d.node, "allowed": d.allowed, "why": d.why, "detail": d.detail })
            })
            .collect();
        // Echoed back, because the answer is only true of one mode and a client
        // that asked for the other should be able to tell.
        Ok(json!({ "from": from, "mode": mode, "destinations": destinations }))
    }

    async fn node(&self, id: &str) -> ApiResult<Option<Node>> {
        let Ok(name) = ResourceName::parse(&format!("nodes/{id}")) else {
            return Ok(None);
        };
        match self.typed::<NodeSpec, NodeStatus>(&name).await {
            Ok(node) => Ok(Some(node)),
            Err(e) if e.code == Code::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The nodes holding a verified copy of an image. An image nobody has is
    /// not an error here — it is a destination that cannot receive, which is
    /// what `may_migrate` says about it.
    ///
    /// Worked out from what each node reports about itself, not read off the
    /// image. Which nodes hold a copy is an *aggregate*, and an aggregate is not
    /// a fact anybody owns — a `cached_on` list on the image would need every
    /// node in the cell writing into one field, which is the shared mutable list
    /// invariant 1 exists to forbid. So each node says what it holds and this
    /// adds them up, which also means the answer cannot be stale in the way a
    /// list maintained by a departed node would be.
    async fn image_cached_on(&self, image: &str, scratch: &mut Scratch) -> ApiResult<Vec<String>> {
        let Ok(name) = ResourceName::parse(image) else {
            return Ok(Vec::new());
        };
        if name.collection() != "images" {
            return Ok(Vec::new());
        }
        // Through the object, because a node files the bytes under their digest
        // and the object's name is a name. Comparing the two directly answered
        // "cached nowhere" about every image in the cell, including one a guest
        // was demonstrably running from.
        let Ok(object): ApiResult<velstra_cloud_model::resources::Image> = self.typed(&name).await
        else {
            return Ok(Vec::new());
        };
        let Some(stored) = velstra_cloud_model::images::stored_name(&object.spec.digest) else {
            return Ok(Vec::new());
        };
        let nodes = scratch.nodes(self).await?;
        Ok(nodes_holding(&stored, &nodes))
    }

    // ---- quota ------------------------------------------------------------

    /// Refuse a create that would take a project past what it may have.
    ///
    /// Counted from the store every time, never from a running total: a total
    /// that is incremented on create and decremented on delete is wrong the
    /// first time either half is missed, and it fails closed — a project that
    /// slowly loses capacity it never used is the classic version of this bug.
    ///
    /// A zero limit means "no limit set" rather than "nothing allowed",
    /// because a project created without a quota is one nobody has decided
    /// about yet, and the alternative is an API that refuses everything until
    /// somebody notices.
    /// Refuse to create a resource in a cell that does not own its project.
    ///
    /// A project records where its resources live (`ProjectSpec.cell`), and that
    /// is what a router resolves. This is the check behind the router: without
    /// it, a request that reached the wrong cell — a stale directory, a client
    /// with a hardcoded endpoint, a router that has not learned a new project
    /// yet — would be answered rather than redirected, and the project's
    /// resources would end up scattered across cells with nothing recording
    /// that they are.
    ///
    /// The refusal **names the cell that should have answered**. That is the
    /// difference between an error a client can act on and one that only says
    /// no: a router follows it, and a person reading it knows where to look.
    ///
    /// A project with no recorded home is answered here, which is what makes a
    /// single-cell installation need no configuration at all and what keeps a
    /// project created a moment ago from being refused while the record
    /// propagates. Only a project that has *said* it lives elsewhere is refused.
    async fn check_cell(&self, name: &ResourceName, kind: &str) -> ApiResult<()> {
        // Global collections are not routed: every cell holds every project.
        if velstra_cloud_model::routing::is_global_collection(kind) {
            return Ok(());
        }
        let Some(project) = velstra_cloud_model::routing::project_of(name) else {
            // A cell's own hardware. It never moves, so the cell being asked is
            // the cell that owns it.
            return Ok(());
        };
        let project_name = ResourceName::parse(&format!("projects/{project}"))?;
        let Ok(project) = self
            .typed::<ProjectSpec, ProjectStatus>(&project_name)
            .await
        else {
            // No such project here. Refusing on that basis would be this check
            // deciding a question that belongs to the create itself, which
            // reports a missing parent in its own words.
            return Ok(());
        };
        let home = &project.spec.cell;
        if home.is_empty() || home == &self.inner.placement.cell {
            return Ok(());
        }
        Err(ApiError::new(
            Code::FailedPrecondition,
            format!(
                "{} lives in cell {home}, and this is cell {}; send this request there",
                project_name, self.inner.placement.cell
            ),
        )
        .at("meta.name"))
    }

    async fn check_quota(&self, name: &ResourceName, kind: &str, spec: &Value) -> ApiResult<()> {
        if kind != "instances"
            && kind != "volumes"
            && kind != "floatingips"
            && kind != "load-balancers"
        {
            // `device-classes` is deliberately absent: a class is a definition
            // of what hardware exists, not a thing a project holds. What is
            // capped is how many devices an *instance* asks for, which is
            // counted on the instance above.
            return Ok(());
        }
        let Some(project) = name.project() else {
            return Ok(());
        };
        let project_name = ResourceName::parse(&format!("projects/{project}"))?;
        // Nothing to charge against. Refusing here would add a precondition the
        // contract does not state; the create stands, uncounted.
        let Ok(project) = self
            .typed::<ProjectSpec, ProjectStatus>(&project_name)
            .await
        else {
            return Ok(());
        };
        let quota = &project.spec.quota;
        let parent = project_name.to_string();

        // Counted from what is stored each time, never from a running total, for
        // the reason the whole quota system is: a total that is incremented on
        // create and decremented on delete is wrong the first time either half
        // is missed, and it fails closed — a project that slowly loses capacity
        // it never used. So every dimension here is a fresh sum over the objects
        // that exist plus the one being created.
        match kind {
            "instances" => {
                let wanted: InstanceSpec = serde_json::from_value(spec.clone())?;
                let existing: Vec<Instance> = self.typed_list(&parent, "instances").await?;
                let count = existing.len() as u32 + 1;
                let vcpus = existing.iter().map(|i| i.spec.vcpus).sum::<u32>() + wanted.vcpus;
                let memory =
                    existing.iter().map(|i| i.spec.memory_mib).sum::<u64>() + wanted.memory_mib;
                exceeded(quota.instances as u64, count as u64, "instances", "spec")?;
                exceeded(quota.vcpus as u64, vcpus as u64, "vCPUs", "spec.vcpus")?;
                exceeded(quota.memory_mib, memory, "MiB of memory", "spec.memoryMib")?;
            }
            "volumes" => {
                let wanted: VolumeSpec = serde_json::from_value(spec.clone())?;
                let existing: Vec<Volume> = self.typed_list(&parent, "volumes").await?;
                let count = existing.len() as u32 + 1;
                let gib = existing.iter().map(|v| v.spec.size_gib).sum::<u64>() + wanted.size_gib;
                // Two independent limits on the same collection: a count of
                // objects and a sum of gibibytes. Either can be the one a
                // project hits, so both are checked and the one that fails names
                // itself.
                exceeded(quota.volumes as u64, count as u64, "volumes", "spec")?;
                exceeded(quota.volume_gib, gib, "GiB of volume", "spec.sizeGib")?;
            }
            "floatingips" => {
                let existing: Vec<velstra_cloud_model::resources::FloatingIp> =
                    self.typed_list(&parent, "floatingips").await?;
                let count = existing.len() as u64 + 1;
                exceeded(quota.floating_ips as u64, count, "floating IPs", "spec")?;
            }
            _ => {
                let existing: Vec<velstra_cloud_model::loadbalancer::LoadBalancer> =
                    self.typed_list(&parent, "load-balancers").await?;
                let count = existing.len() as u64 + 1;
                exceeded(quota.load_balancers as u64, count, "load balancers", "spec")?;
            }
        }
        Ok(())
    }

    // ---- typed helpers ----------------------------------------------------

    /// Read an object as the model type it is. The round trip through JSON is
    /// the price of one erased collection layer, and it is paid on reads that
    /// need the real type — placement and quota — rather than on every request.
    pub async fn typed<S, T>(&self, name: &ResourceName) -> ApiResult<Resource<S, T>>
    where
        S: serde::de::DeserializeOwned,
        T: serde::de::DeserializeOwned,
    {
        let collection = self.collection(name.collection())?;
        let document = collection
            .get(&name.to_string())
            .await?
            .ok_or_else(|| ApiError::not_found(name))?;
        Ok(serde_json::from_value(document)?)
    }

    pub async fn typed_list<S, T>(&self, parent: &str, kind: &str) -> ApiResult<Vec<Resource<S, T>>>
    where
        S: serde::de::DeserializeOwned,
        T: serde::de::DeserializeOwned,
    {
        let collection = self.collection(kind)?;
        collection
            .list()
            .await?
            .into_iter()
            .filter(|document| under(document, parent))
            .map(|document| serde_json::from_value(document).map_err(ApiError::from))
            .collect()
    }

    fn name_for(&self, parent: &str, kind: &str, body: &Value) -> ApiResult<ResourceName> {
        let stated = body
            .get("meta")
            .and_then(|m| m.get("name"))
            .and_then(joined);
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        // An image's name is for people; its bytes are addressed by their
        // digest, which is a separate thing and lives in the spec.
        //
        // Those two were one string for a while and it was wrong in both
        // directions: the node parsed the digest out of the *name*, so an image
        // called `debian13` failed at boot with "carries no sha256 digest in its
        // name" — and the fix that made the name *be* the digest gave every
        // operator a list of `sha256-cbf3e1f588f02f8d738dbecb…` to choose an
        // operating system from. The node reads `spec.digest` now, so a name can
        // be a name.
        //
        // What is minted when nobody supplies one is readable: the family and
        // enough of the digest to tell two builds apart — `debian-13-cbf3e1f5`
        // — because an id nobody can say out loud is an id nobody will use.
        if kind == "images"
            && stated.is_none()
            && id.is_none()
            && let Some(digest) = body
                .get("spec")
                .and_then(|s| s.get("digest"))
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
        {
            let short: String = digest
                .rsplit(':')
                .next()
                .unwrap_or(digest)
                .chars()
                .take(8)
                .collect();
            let family = body
                .get("spec")
                .and_then(|s| s.get("family"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let readable = if family.is_empty() {
                format!("image-{short}")
            } else {
                format!("{family}-{short}")
            };
            let full = if parent.is_empty() {
                format!("{kind}/{readable}")
            } else {
                format!("{parent}/{kind}/{readable}")
            };
            return Ok(ResourceName::parse(&full)?);
        }
        let name = match (stated, id) {
            (Some(name), _) => name,
            (None, Some(id)) if parent.is_empty() => format!("{kind}/{id}"),
            (None, Some(id)) => format!("{parent}/{kind}/{id}"),
            // Minted, because the alternative is asking a person to invent an
            // identifier before they may have a machine. That was the old
            // behaviour and it put the platform's naming scheme in front of
            // every first use of it — a console cannot offer "create" without
            // first teaching what a resource id is.
            //
            // The trade is real and is the reason this used to be refused: a
            // create with no id is **not idempotent**, so a client that retries
            // a request whose answer it never saw gets a second object. A client
            // that needs a retry to be safe sends the id, which is still the way
            // every controller and every script here does it. The name is in the
            // response either way.
            (None, None) if parent.is_empty() => format!("{kind}/{}", minted(kind)),
            (None, None) => format!("{parent}/{kind}/{}", minted(kind)),
        };
        let name = ResourceName::parse(&name)?;
        if name.collection() != kind {
            return Err(ApiError::invalid(format!(
                "{name} is not in {kind}; the name in the body and the collection in the path must agree"
            ))
            .at("meta.name"));
        }
        let stated_parent = name.parent().map(|p| p.to_string()).unwrap_or_default();
        if stated_parent != parent {
            return Err(
                ApiError::invalid(format!("{name} does not live under {parent}")).at("meta.name"),
            );
        }
        Ok(name)
    }
}

/// An id for a caller who did not bring one.
///
/// The kind in the singular and enough of a uuid to be unique in a cell —
/// `instance-3f9a2c81` — because an id nobody chose still has to be readable in
/// a list, a URL and an error message. Not a uuid on its own: a person reading
/// `projects/home/instances/3f9a2c81-…` cannot tell what they are looking at.
fn minted(kind: &str) -> String {
    // Every collection this API serves is a plain plural — instances, images,
    // networks, floatingips — so one `s` is the whole of it. A kind that is not
    // keeps its name rather than losing a letter.
    let singular = kind.strip_suffix('s').unwrap_or(kind);
    let uid = uuid::Uuid::new_v4().to_string();
    format!("{singular}-{}", &uid[..8])
}

/// The answer to "that name is taken", as a sentence about the thing the caller
/// asked for.
///
/// The store can only say that a key exists, and the key is not what anybody
/// typed. Here the collection and the id are both known, and the id is the
/// control the operator has to change — so the error names it, and lands on the
/// field rather than in a banner.
fn taken(error: ApiError, kind: &str, name: &ResourceName) -> ApiError {
    if error.code != Code::AlreadyExists {
        return error;
    }
    let singular = kind.strip_suffix('s').unwrap_or(kind);
    // "a instance" is the sort of thing that makes an interface look
    // machine-written, and this sentence is read by a person every time
    // somebody reuses a name.
    let article = match singular.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    };
    let message = match name.parent() {
        Some(parent) => format!(
            "{article} {singular} called {} already exists in {parent}",
            name.id()
        ),
        None => format!("{article} {singular} called {} already exists", name.id()),
    };
    ApiError::new(Code::AlreadyExists, message).at("id")
}

fn exceeded(limit: u64, wanted: u64, what: &str, field: &str) -> ApiResult<()> {
    if limit == 0 || wanted <= limit {
        return Ok(());
    }
    Err(ApiError::new(
        Code::ResourceExhausted,
        format!("the project may have {limit} {what}, and this would make {wanted}"),
    )
    .at(field))
}

/// Who a read is for, as far as the page loop and the event stream are
/// concerned.
///
/// Two cases and no third: either every object that survives the filter goes
/// through, or each one is checked against a caller first. Modelled as a type
/// rather than an `Option<Identity>` so "no identity" cannot be read as "deny
/// everything" by a future reader — it means the caller has *already* been
/// authorised, on the parent, which is the common path and the one where being
/// wrong is silent.
///
/// One type for both paths on purpose. A list and a watch answer the same
/// question about the same objects, and the moment they answer it in two
/// different shapes is the moment one of them grows a case the other does not —
/// which is how the watch came to have no authorisation at all while the list
/// had it all along.
///
/// Owns its identity rather than borrowing: the stream outlives the call that
/// built it, and a lifetime here would push that problem into every caller.
#[derive(Clone)]
enum Gate {
    Everything,
    /// A node agent's stream. Everything it may read, and *raw*: the computed
    /// answers below are presentation for people, and an agent that reads a
    /// decorated object writes its own undecorated one straight back — every
    /// pass, for ever. Found live as three writes a second on a settled cell:
    /// `answer_port` rewrote `Ready` on the way out, the agent rewrote it on
    /// the way back, and each write woke the watch that started the next pass.
    Machine,
    Readable(Identity),
}

/// A document's resource name, which is also the order everything is paged in.
///
/// The store keys an object as its collection prefix plus this exact string, so
/// key order inside a collection *is* name order — which is what lets a page
/// token carry a name and still be honoured by a read that goes to the store.
fn name_of(document: &Value) -> Option<String> {
    joined(&document["meta"]["name"])
}

/// Whether a document's name sits under `parent`. An empty parent is the whole
/// collection, which is what a root collection like `nodes` always is.
fn under(document: &Value, parent: &str) -> bool {
    if parent.is_empty() {
        return true;
    }
    joined(&document["meta"]["name"])
        .map(|name| name.starts_with(&format!("{parent}/")))
        .unwrap_or(false)
}

/// The halves a client does not own.
///
/// `status` is the agent's, and every other piece of `meta` is the platform's:
/// a uid it did not mint, a generation it did not earn, a finalizer it does not
/// hold. Naming the field is the whole point — "invalid request" on a body with
/// forty fields is a guessing game.
/// Refuse an image that arrives carrying a signature nothing will check.
///
/// The reason is on the field itself
/// ([`velstra_cloud_model::resources::ImageSpec::signature`]); the short version
/// is that the platform will not hold a security claim it cannot verify,
/// because everywhere it is displayed becomes evidence somebody will cite.
///
/// A tenant that could mark a network external could mint itself a public
/// range by writing a CIDR into a subnet.
///
/// So the flag is an operator's, and it is refused rather than ignored: a
/// silently dropped `external: true` would leave somebody believing they had a
/// public network and wondering why nothing reaches it.
/// What else comes with each of a node's passable devices.
///
/// Passing one device through takes its whole IOMMU group — the hardware cannot
/// isolate less than that — and an operator who learns it afterwards learns it
/// from an outage. [`velstra_cloud_model::pci::offerable`] already refuses a
/// device whose group has a busy member, so nothing unsafe can be *assigned*;
/// what was missing is the sentence before the decision.
///
/// Computed rather than reported, for one specific reason. The wire already
/// carries every device's `iommuGroup`, so a console could group them itself —
/// and would get the interesting case backwards. A device with no group means
/// "this machine cannot isolate it", and `group_members` answers with the
/// device alone; a client-side filter on equality would instead collect every
/// IOMMU-less device on the node into one imaginary group, which reads as "all
/// of these come together" when the truth is that none of them can be passed at
/// all.
fn answer_node(document: &mut Value) {
    // `pci_devices`, not `devices` — the latter is this node's *block* devices,
    // which the Ceph disk picker reads. Two inventories of one machine's
    // hardware, and they are not the same list.
    let Some(devices) = document
        .get("status")
        .and_then(|s| s.get("pci_devices"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let Ok(parsed) =
        serde_json::from_value::<Vec<velstra_cloud_model::pci::PciDevice>>(json!(devices))
    else {
        return;
    };
    let with: Vec<Value> = parsed
        .iter()
        .map(|d| json!(velstra_cloud_model::pci::group_members(d, &parsed)))
        .collect();
    for (device, members) in document["status"]["pci_devices"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .zip(with)
    {
        device["group_with"] = members;
    }
}

/// What a running guest has been asked for and will only get when it restarts.
///
/// The fifth computed field, and the one with the sharpest reason for existing.
/// A guest that is resized while it runs takes the new numbers at its next
/// start, not now — that is ordinary and correct. What was not correct is that
/// nothing said so: the spec read 8 vCPUs, `observedGeneration` caught up
/// because the agent had genuinely handled the change, `Ready` was true, and
/// the guest went on running on 4. Every screen agreed it had converged.
///
/// The arithmetic and the reason were written down in
/// [`velstra_cloud_model::resources::pending_changes`] — whose own comment says
/// the alternative "is a spec that reads as applied while the guest runs on the
/// old numbers", because that is what shipped — and then nothing ever called
/// it. The agent already reports `status.runningSize`; this is the half that
/// reads it.
///
/// Computed rather than stored, like the four above: it is a comparison between
/// a spec and a status, and a third copy of it could disagree with both.
/// Absent, rather than an empty list, when there is nothing pending — a field
/// that is always there is one a reader has to inspect to learn nothing.
fn answer_instance(document: &mut Value) {
    // Only a guest has one, and only a running guest has it set: a stopped one
    // has nothing to differ from, and its next start gives it the spec by
    // construction.
    if document
        .get("status")
        .and_then(|s| s.get("running_size"))
        .is_none_or(Value::is_null)
    {
        return;
    }
    let Ok(instance) =
        serde_json::from_value::<velstra_cloud_model::resources::Instance>(document.clone())
    else {
        // A document that will not parse as an instance is not one. Reading it
        // as a guest with nothing pending would be an answer about an object
        // this function never understood.
        return;
    };
    let pending = velstra_cloud_model::resources::pending_changes(&instance);
    if pending.is_empty() {
        return;
    }
    document["status"]["pending_changes"] = json!(pending);
}

fn refuse_an_external_network_from_a_tenant(
    spec: &Value,
    who: &Identity,
    is_operator: bool,
) -> ApiResult<()> {
    let _ = who;
    if spec.get("external").and_then(Value::as_bool) == Some(true) && !is_operator {
        return Err(ApiError::forbidden(
            "only a cell operator may mark a network external. What the flag means is that the \
             prefixes on its subnets are real — routed to this cell by whoever is above it — and \
             that is not a claim a tenant can make about their own range.",
        )
        .at("spec.external"));
    }
    // `host_bridge` is deliberately **not** here, and it used to be. It was
    // "cell operator or nobody", which is the right answer for a cell with one
    // tenant and useless for a provider: the question is not whether a tenant
    // may use host bridges, it is which wire this customer bought. So it moved
    // to the project's policy, where the cell answers it per project — see
    // `refuse_a_bridge_this_project_was_not_given`.
    //
    // `external` stays here because it does not decompose the same way: what it
    // claims is that a prefix is routed to this cell by whoever is above it,
    // which is a fact about the world and not a thing to hand out per customer.
    Ok(())
}

/// A ratio that would promise a cell's worth of processor out of one machine.
///
/// There is no correct number here and the platform does not pretend to know
/// one — four is ordinary, sixteen is a lab, and the right answer depends on
/// what the guests do all day. What is refused is the range where the ratio has
/// stopped being a trade and become a way of hiding that a cell is full: past
/// thirty-two, a machine's guests are getting a thirty-second of a core each
/// and "it is slow" stops being diagnosable from anything the platform reports.
const MOST_VCPUS_PER_CORE: u64 = 32;

fn refuse_an_unusable_overcommit(spec: &Value) -> ApiResult<()> {
    let Some(ratio) = spec.get("vcpu_overcommit") else {
        return Ok(());
    };
    let Some(ratio) = ratio.as_u64() else {
        return Err(
            ApiError::invalid("a vcpu overcommit is a whole number of vcpus per core")
                .at("spec.vcpuOvercommit"),
        );
    };
    if ratio > MOST_VCPUS_PER_CORE {
        return Err(ApiError::invalid(format!(
            "{ratio} vcpus per core is past {MOST_VCPUS_PER_CORE}, where the ratio stops being a              trade and becomes a way of hiding that the cell is full — every guest on the machine              would get a fraction of a core and being slow would stop being diagnosable"
        ))
        .at("spec.vcpuOvercommit"));
    }
    Ok(())
}

/// An explicitly *empty* signature is not a claim and is not refused: a client
/// echoing back an object it read, or clearing the field, must not be told off
/// for it.
fn refuse_an_unverified_signature(spec: &Value) -> ApiResult<()> {
    let carried = spec
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if carried {
        return Err(
            ApiError::invalid(velstra_cloud_model::resources::UNVERIFIED_SIGNATURE)
                .at("spec.signature"),
        );
    }
    Ok(())
}

fn refuse_unwritable(body: &Value) -> ApiResult<()> {
    if body.get("status").is_some() {
        return Err(ApiError::invalid(
            "status is written by the agent that owns the object, and is read-only here",
        )
        .at("status"));
    }
    if let Some(Value::Object(meta)) = body.get("meta") {
        for key in meta.keys() {
            if key != "labels" && key != "name" {
                return Err(ApiError::invalid(format!(
                    "meta.{key} is maintained by the platform; a client writes spec and labels"
                ))
                .at(format!("meta.{key}")));
            }
        }
    }
    Ok(())
}

/// The shape a create answers with, per the contract: the operation to follow
/// and the object it is about.
/// Keep the newest `keep` snapshots in `dir`, quietly.
///
/// Quietly, because this runs on a timer with nobody watching: a prune that
/// cannot read the directory logs and moves on, and the next round tries
/// again. The names sort by time by construction (`etcd-<ms>.snap`).
async fn prune_snapshots(dir: &std::path::Path, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut snaps: Vec<std::path::PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("snap") {
            snaps.push(path);
        }
    }
    snaps.sort();
    if snaps.len() <= keep {
        return;
    }
    let excess = snaps.len() - keep;
    for old in &snaps[..excess] {
        if let Err(e) = tokio::fs::remove_file(old).await {
            tracing::warn!(snapshot = %old.display(), error = %e, "an old snapshot would not go");
        }
    }
}

pub fn created_body(created: &Created) -> Value {
    let mut body = Map::new();
    body.insert(
        "operation".into(),
        Value::String(joined(&created.operation["meta"]["name"]).unwrap_or_default()),
    );
    body.insert("target".into(), Value::String(created.target.clone()));
    // Present only when a node was registered, and even then it is the one field
    // in this API returned once and never again — the node's agent token.
    if let Some(token) = &created.node_token {
        body.insert("nodeToken".into(), Value::String(token.clone()));
    }
    if let Some(token) = &created.pool_token {
        body.insert("poolToken".into(), Value::String(token.clone()));
    }
    Value::Object(body)
}

/// Refuse a security-group rule that cannot mean what it says.
///
/// At the edge rather than in the agent, because the alternative is a rule that
/// is accepted, stored, shown back on read and then quietly skipped by whichever
/// node reads it — which is the failure this whole feature exists to remove, in
/// miniature. The agent skips such a rule too, but only as the belt to this
/// brace, for a group written by an older version of this software.
/// A role has to grant something, and something narrower than a rung.
///
/// Every grant names the collections it applies to and the list may not be
/// empty. There is no wildcard, and that is the whole shape of the feature: a
/// custom role is **always narrower than a rung, by construction**. Somebody who
/// wants "everything" has four of those already, and a custom role that could
/// mean it would be a second spelling of `admin` with no way to tell them apart
/// in a list of who may do what.
fn check_role(spec: &Value) -> ApiResult<()> {
    let grants = spec.get("grants").and_then(Value::as_array);
    if grants.is_none_or(|g| g.is_empty()) {
        return Err(ApiError::invalid(
            "a role grants something. Name at least one verb and the collections it applies to, \
             as in `[{\"verb\": \"operate\", \"collections\": [\"instances\"]}]`.",
        )
        .at("spec.grants"));
    }
    for (i, grant) in grants.expect("checked").iter().enumerate() {
        let collections = grant.get("collections").and_then(Value::as_array);
        if collections.is_none_or(|c| c.is_empty()) {
            return Err(ApiError::invalid(
                "a grant names the collections it applies to, and there is no wildcard: a role \
                 that meant `everything` would be a second spelling of one of the four rungs.",
            )
            .at(format!("spec.grants[{i}].collections")));
        }
        for (j, named) in collections.expect("checked").iter().enumerate() {
            let named = named.as_str().unwrap_or_default();
            if !COLLECTIONS.contains(&named) {
                return Err(ApiError::invalid(format!(
                    "there is no collection called `{named}`. A grant over one that does not \
                     exist is a permission nobody will ever hold, which reads exactly like a \
                     permission that was never granted."
                ))
                .at(format!("spec.grants[{i}].collections[{j}]")));
            }
        }
    }
    Ok(())
}

/// An image has to say what bytes it is.
///
/// Checked here rather than left to the console, because the console cannot
/// express it: `from` supplies the digest, the format and the source, so those
/// three are required *unless* it is set — and a form field is required or it is
/// not. The browser asks for what it can; the API is what decides.
///
/// Without this an image with no digest was a perfectly acceptable object, and
/// the first thing to notice would have been a node with nothing to fetch.
fn check_image(spec: &Value) -> ApiResult<()> {
    let said = |key: &str| {
        spec.get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
    };
    if said("from") {
        // Publishing. What is missing comes off the image being published from,
        // and `settle_published_image` has already put it here — so anything
        // still absent means that image was itself incomplete, which the
        // checks below then say.
        return Ok(());
    }
    if !said("digest") {
        return Err(ApiError::invalid(
            "an image says which bytes it is — `sha256:…` — because that is what makes fetching \
             one verifiable. Name an existing image in `from` to publish it instead, and \
             everything describing the bytes is taken from that one.",
        )
        .at("spec.digest"));
    }
    // A URL *or* a guest. An image captured from a running machine has no URL
    // and never did: it says which instance it came from, which is the same
    // question answered the other way. Demanding the URL refused every capture
    // this platform makes.
    if !said("source_url") && !said("source_instance") {
        return Err(ApiError::invalid(
            "an image says where it came from: `sourceUrl` for bytes to fetch, or \
             `sourceInstance` for a guest it was captured from. Name an existing image in \
             `from` to publish it instead.",
        )
        .at("spec.source_url"));
    }
    Ok(())
}

fn check_rules(kind: &str, spec: &Value) -> ApiResult<()> {
    if kind == "load-balancers" {
        return check_listeners(spec);
    }
    if kind == "images" {
        return check_image(spec);
    }
    if kind == "roles" {
        return check_role(spec);
    }
    if kind != "security-groups" {
        return Ok(());
    }
    let Some(rules) = spec.get("rules").and_then(Value::as_array) else {
        return Ok(());
    };
    for (i, rule) in rules.iter().enumerate() {
        let parsed: velstra_cloud_model::security::SecurityRule =
            serde_json::from_value(rule.clone())
                .map_err(|e| ApiError::invalid(format!("{e}")).at(format!("spec.rules[{i}]")))?;
        validate(&parsed)
            .map_err(|e| ApiError::invalid(e.to_string()).at(format!("spec.rules[{i}]")))?;
    }
    Ok(())
}

/// Refuse a listener that cannot mean what it says, at the edge and by index.
///
/// The same reasoning as a security-group rule: the alternative is a spec that
/// is accepted, shown back on read, and then quietly refused by the fabric
/// with an error naming a service id nobody typed. The controller checks again
/// for an object written by an older version of this software — belt to this
/// brace.
fn check_listeners(spec: &Value) -> ApiResult<()> {
    let Some(listeners) = spec.get("listeners").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut parsed = Vec::with_capacity(listeners.len());
    for (i, listener) in listeners.iter().enumerate() {
        let one: velstra_cloud_model::loadbalancer::Listener =
            serde_json::from_value(listener.clone()).map_err(|e| {
                ApiError::invalid(format!("{e}")).at(format!("spec.listeners[{i}]"))
            })?;
        parsed.push(one);
    }
    velstra_cloud_model::loadbalancer::validate_listeners(&parsed).map_err(|why| {
        ApiError::invalid(why.to_string()).at(format!("spec.listeners[{}]", why.at()))
    })
}

/// One piece of advice as JSON.
///
/// A tagged shape — every variant carries `kind` — so a console can branch on
/// what it is rather than sniffing which fields are present. The cost of each
/// recommendation travels with it: a suggestion that names only the benefit
/// arrives wearing the platform's authority.
fn advice_json(a: &velstra_cloud_model::cpu::Advice) -> Value {
    use velstra_cloud_model::cpu::{Advice, CannotMerge};
    match a {
        Advice::AlreadyUniform { nodes, level } => json!({
            "kind": "AlreadyUniform",
            "nodes": nodes,
            "level": level.map(|l| l.as_str()),
        }),
        Advice::BaselineWouldMerge {
            nodes,
            level,
            features_lost,
        } => json!({
            "kind": "BaselineWouldMerge",
            "nodes": nodes,
            "level": level.as_str(),
            // Per node, and only for the nodes that pay. A single number here
            // would be a decision nobody can make.
            "featuresLost": features_lost.iter().map(|(node, lost)| json!({
                "node": node,
                "flags": lost,
            })).collect::<Vec<_>>(),
        }),
        Advice::CannotMerge { nodes, reason } => json!({
            "kind": "CannotMerge",
            "nodes": nodes,
            "reason": match reason {
                CannotMerge::VmmCannotMask { nodes } => json!({
                    "kind": "VmmCannotMask",
                    "nodes": nodes,
                }),
                CannotMerge::WouldDropBelow { level } => json!({
                    "kind": "WouldDropBelow",
                    "level": level.as_str(),
                }),
            },
        }),
        Advice::SplitByArch { groups } => json!({
            "kind": "SplitByArch",
            "groups": groups.iter().map(|(arch, nodes)| json!({
                "arch": arch,
                "nodes": nodes,
            })).collect::<Vec<_>>(),
        }),
        Advice::NodeOutsideTheAggregate {
            node,
            presents,
            aggregate,
            aggregate_nodes,
            missing,
        } => json!({
            "kind": "NodeOutsideTheAggregate",
            "node": node,
            "presents": presents,
            "aggregate": aggregate,
            "aggregateNodes": aggregate_nodes,
            // Empty means it could join and simply has not been told. Non-empty
            // means it never can, and the honest remedy is a second aggregate.
            "missing": missing,
        }),
        Advice::AdoptionPending { guests, target } => json!({
            "kind": "AdoptionPending",
            "guests": guests,
            "target": target.map(|l| l.as_str()),
        }),
    }
}

/// Everything about a source that can be judged before anybody waits on a
/// network. Chief among them: the checksums file is fetched over https, because
/// it is the one value the whole arrangement trusts.
fn refuse_an_unusable_image_source(spec: &Value) -> ApiResult<()> {
    let parsed: velstra_cloud_model::images::ImageSourceSpec =
        serde_json::from_value(spec.clone()).map_err(|e| ApiError::invalid(e.to_string()))?;
    velstra_cloud_model::images::refuse_an_unusable_source(&parsed).map_err(|e| {
        use velstra_cloud_model::images::Unusable;
        let field = match e {
            Unusable::ChecksumsNotHttps => "spec.checksums",
            Unusable::NoFamily => "spec.family",
            Unusable::NoUrl | Unusable::NoFilename => "spec.url",
        };
        ApiError::invalid(e.to_string()).at(field)
    })
}
