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
        Project, ProjectSpec, ProjectStatus, Resource, RouterSpec, RouterStatus, SnapshotSpec,
        SnapshotStatus, SubnetSpec, SubnetStatus, Volume, VolumeSpec, VolumeStatus, nodes_holding,
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
pub const COLLECTIONS: [&str; 28] = [
    "projects",
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
    "snapshot-schedules",
    "maintenance-windows",
    "operations",
];

/// Said the same way wherever somebody tries to write a usage record.
///

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
        self.answer(&mut document, &mut Scratch::default()).await?;
        Ok(document)
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
        let verdict = self.judge(who, verb, name).await;
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
        if self.judge(who, Verb::Read, name).await.is_ok() {
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
        self.judge(who, Verb::Read, &target).await.is_ok()
    }

    async fn judge(&self, who: &Identity, verb: Verb, name: &ResourceName) -> ApiResult<()> {
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
            return Ok(());
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
        let Some(project) = governing_project(name) else {
            // Outside every project: a node, a pool, the projects collection.
            // These are the cell's, and only an operator has the cell.
            return Err(ApiError::forbidden(
                "this is a cell-wide resource; only a cell operator may touch it",
            ));
        };
        // Read the project's bindings. A project that is not there refuses in
        // the same words as one that refuses, so the error is not an oracle for
        // which projects exist.
        let bindings = match self.typed_project(&project).await {
            Ok(Some(p)) => p.spec.bindings,
            _ => Vec::new(),
        };
        may(&who.subject, &self.inner.cell_admins, &bindings, verb)
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
            // The first tick is immediate; a fresh cell has nothing to sweep, and
            // sweeping an empty collection costs one list.
            loop {
                ticker.tick().await;
                if let Err(e) = identity.sweep_expired_sessions(Timestamp::now()).await {
                    tracing::warn!(error = %e, "the session sweep could not run this round");
                }
                if let Err(e) = api.sweep_spent_consoles(Timestamp::now()).await {
                    tracing::warn!(error = %e, "the console sweep could not run this round");
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
        if !parent.is_empty() {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Read, &name).await?;
            return self.list_page(parent, kind, filter, paging).await;
        }
        // No parent: a cell-wide collection. An operator sees it whole;
        // everybody else sees the objects they may read, one decision each.
        let gate = if self.is_operator(who) {
            Gate::Everything
        } else {
            Gate::Readable(who.clone())
        };
        self.list_gated(parent, kind, filter, paging, &gate).await
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
                self.answer(&mut document, &mut scratch).await?;
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
        crate::refs::check(kind, &spec)?;
        // Before anything follows one of those references — `settle_volume_source`
        // reads the snapshot, `settle_migration` reads the instance — so that a
        // caller who may not read the thing they named is refused for that
        // reason and learns nothing about whether it is there.
        self.authorize_references(who, kind, &spec, home.as_deref())
            .await?;
        check_rules(kind, &spec)?;
        if kind == "attachments" {
            self.settle_node(&mut spec, None).await?;
        }
        if kind == "migrations" {
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
        if kind == "volumes" {
            self.settle_volume_source(&name, &mut spec).await?;
        }
        if kind == "ceph-clusters" {
            self.refuse_a_second_ceph_cluster(&name).await?;
            self.refuse_a_disk_that_is_not_free(&spec).await?;
        }
        if kind == "images" {
            refuse_an_unverified_signature(&spec)?;
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
        Ok(Created {
            operation,
            target: name.to_string(),
            node_token,
        })
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
            Verb::Write
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
        let collection = self.collection(name.collection())?;
        refuse_unwritable(body)?;
        let mut patch = Patch {
            spec: body.get("spec").cloned(),
            labels: body.get("meta").and_then(|m| m.get("labels")).cloned(),
        };
        if let Some(spec) = &mut patch.spec {
            crate::refs::check(name.collection(), spec)?;
            self.authorize_references(
                who,
                name.collection(),
                spec,
                governing_project(name).as_deref(),
            )
            .await?;
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
            // `cpu_baseline`, not `cpuBaseline`: the body was converted out of
            // its wire spelling before it got here.
            if name.collection() == "floatingips" {
                let stored: Value = self.get(name, who).await?;
                let mut merged = stored["spec"].clone();
                merge(&mut merged, spec);
                self.refuse_an_address_that_reaches_nothing(name, &merged)
                    .await?;
            }
            if name.collection() == "networks" && spec.get("external").is_some() {
                refuse_an_external_network_from_a_tenant(spec, who, self.is_operator(who))?;
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
        let mut document = collection
            .report_status(&name.to_string(), status, expect, &writer)
            .await?;
        self.answer(&mut document, &mut Scratch::default()).await?;
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
                // An operation is a *record of a request*, not a thing anybody
                // holds. Counting them made a project undeletable the moment it
                // was created, because creating it produced one — which the
                // tests caught and is the right answer either way: nobody is
                // waiting on an audit line. They outlive the project and are
                // pruned by their own retention, not by this.
                if kind == "projects" || kind == "operations" {
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
        let gate = if parent.is_empty() {
            // A cell-wide stream. An operator is asking about the cell on
            // purpose; anybody else is told about what they may read.
            if self.is_operator(who) {
                Gate::Everything
            } else {
                Gate::Readable(who.clone())
            }
        } else {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Read, &name).await?;
            Gate::Everything
        };
        self.watch_gated(parent, kind, from, filter, gate)
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
                self.answer(&mut document, &mut Scratch::default())
                    .await
                    .ok()?;
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
        // The name is a segment list in model shape, not a string — reading it
        // as one is a silent no-op that looks like a missing field.
        let Some(name) = joined(&document["meta"]["name"]) else {
            return Ok(());
        };
        let held = self.image_cached_on(&name, scratch).await?;
        document["status"]["cached_on"] = json!(held);
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
    pub async fn open_console(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
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
        let read_only = self.authorize(who, Verb::Write, name).await.is_err();

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

        Ok(json!({
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
        }))
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
        let _ = who;
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
        // what it is.
        let _ = who;
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
    pub async fn explain_migration(&self, name: &ResourceName, who: &Identity) -> ApiResult<Value> {
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
                // Answered for a live migration, which is the default and
                // what a console's picker is about. A guest holding hardware
                // is therefore shown as unmovable here, and the refusal names
                // `Reboot` as the way to move it anyway.
                let verdict = may_migrate(
                    &instance,
                    source,
                    to,
                    &cached,
                    velstra_cloud_model::migration::MigrationMode::Live,
                );
                let d = velstra_cloud_proto::convert::destination_of(id, verdict.as_ref().err());
                json!({ "node": d.node, "allowed": d.allowed, "why": d.why, "detail": d.detail })
            })
            .collect();
        Ok(json!({ "from": from, "destinations": destinations }))
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
        let nodes = scratch.nodes(self).await?;
        Ok(nodes_holding(image, &nodes))
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
            (None, None) => {
                return Err(ApiError::invalid(
                    "a create carries the id it wants: names are chosen by the caller, not minted here",
                )
                .at("id"));
            }
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
    Value::Object(body)
}

/// Refuse a security-group rule that cannot mean what it says.
///
/// At the edge rather than in the agent, because the alternative is a rule that
/// is accepted, stored, shown back on read and then quietly skipped by whichever
/// node reads it — which is the failure this whole feature exists to remove, in
/// miniature. The agent skips such a rule too, but only as the belt to this
/// brace, for a group written by an older version of this software.
fn check_rules(kind: &str, spec: &Value) -> ApiResult<()> {
    if kind == "load-balancers" {
        return check_listeners(spec);
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
