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
pub const COLLECTIONS: [&str; 16] = [
    "projects",
    "instances",
    "migrations",
    "volumes",
    "snapshots",
    "attachments",
    "networks",
    "routers",
    "floatingips",
    "subnets",
    "ports",
    "security-groups",
    "images",
    "nodes",
    "pools",
    "operations",
];

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
}

impl Filter {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn for_node(node: impl Into<String>) -> Self {
        Self {
            assignee: Some(Assignee::Node(node.into())),
        }
    }

    pub fn for_pool(pool: impl Into<String>) -> Self {
        Self {
            assignee: Some(Assignee::Pool(pool.into())),
        }
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
            collection!("instances", InstanceSpec, InstanceStatus),
            collection!("volumes", VolumeSpec, VolumeStatus),
            collection!("snapshots", SnapshotSpec, SnapshotStatus),
            collection!("attachments", AttachmentSpec, AttachmentStatus),
            collection!("networks", NetworkSpec, NetworkStatus),
            collection!("routers", RouterSpec, RouterStatus),
            collection!("floatingips", FloatingIpSpec, FloatingIpStatus),
            collection!("subnets", SubnetSpec, SubnetStatus),
            collection!("ports", PortSpec, PortStatus),
            collection!("security-groups", SecurityGroupSpec, SecurityGroupStatus),
            collection!("images", ImageSpec, ImageStatus),
            collection!("nodes", NodeSpec, NodeStatus),
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
            }),
        }
    }

    pub fn verifier(&self) -> &Arc<dyn TokenVerifier> {
        &self.inner.verifier
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
        self.authorize(who, Verb::Read, name).await?;
        let collection = self.collection(name.collection())?;
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

    /// Whether `who` may `verb` `name`, from the bindings on the project that
    /// governs it — or from the operator list, for anything outside every
    /// project.
    ///
    /// One function, called at the top of every entry point, because an
    /// authorisation rule spread across eleven call sites is an authorisation
    /// rule with a hole in it.
    async fn authorize(&self, who: &Identity, verb: Verb, name: &ResourceName) -> ApiResult<()> {
        if self.inner.cell_admins.contains(&who.subject) {
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
        let gate = if self.inner.cell_admins.contains(&who.subject) {
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
                // Before `answer`, for the same reason `admits` is: the derived
                // fields are the expensive part, and an object the caller may
                // not see is an object nobody should pay for.
                if let Gate::Readable(who) = gate {
                    let Some(name) = name_of(&document).and_then(|n| ResourceName::parse(&n).ok())
                    else {
                        continue;
                    };
                    if self.authorize(who, Verb::Read, &name).await.is_err() {
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
        // Authorised on the **parent**, because the object does not exist yet
        // and has no bindings of its own. Creating inside a project is a write
        // to that project; creating without one is a write to the cell.
        if parent.is_empty() {
            if !self.inner.cell_admins.contains(&who.subject) {
                return Err(ApiError::forbidden(
                    "creating a project, a node or a pool is a change to the cell; only a cell \
                     operator may make one",
                ));
            }
        } else {
            let name = ResourceName::parse(parent).map_err(ApiError::from)?;
            self.authorize(who, Verb::Write, &name).await?;
        }
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
        crate::refs::check(kind, &spec)?;
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
        if kind == "volumes" {
            self.settle_volume_source(&mut spec).await?;
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
        Ok(Created {
            operation,
            target: name.to_string(),
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
        let collection = self.collection(name.collection())?;
        refuse_unwritable(body)?;
        let mut patch = Patch {
            spec: body.get("spec").cloned(),
            labels: body.get("meta").and_then(|m| m.get("labels")).cloned(),
        };
        if let Some(spec) = &mut patch.spec {
            crate::refs::check(name.collection(), spec)?;
            check_rules(name.collection(), spec)?;
            if name.collection() == "volumes" {
                self.refuse_a_new_source(name, spec).await?;
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

    /// Ask for a deletion. Two-phase and visible: the object stays readable,
    /// carrying its `deletedAt` and its finalizers, until the last holder lets
    /// go.
    pub async fn delete(
        &self,
        name: &ResourceName,
        expect: Option<Revision>,
        who: &Identity,
    ) -> ApiResult<Deleted> {
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
        collection.delete(&name.to_string(), expect).await
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
            if self.inner.cell_admins.contains(&who.subject) {
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

        let (candidate, rejected) = match place(&instance, &nodes, &occupied) {
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
        // Floating addresses come out of this same range, so a count that saw
        // only ports would tell an operator a full subnet had room.
        let floating = scratch.floating(self).await?;
        let (allocated, available) = velstra_cloud_model::ipam::counts(&subnet, &ports, &floating);
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
    async fn settle_volume_source(&self, spec: &mut Value) -> ApiResult<()> {
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
        if let Err(refusal) = may_create_volume(&wanted, from.as_ref()) {
            use velstra_cloud_model::storage::Refusal;
            let field = match &refusal {
                Refusal::SmallerThanItsSnapshot { .. } => "spec.sizeGib",
                Refusal::AnotherPool { .. } => "spec.pool",
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
        if let Err(refusal) = may_migrate(&instance, &source, &destination, &cached) {
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
                let verdict = may_migrate(&instance, source, to, &cached);
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
        if kind != "instances" && kind != "volumes" {
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

        if kind == "instances" {
            let wanted: InstanceSpec = serde_json::from_value(spec.clone())?;
            let existing: Vec<Instance> = self.typed_list(&parent, "instances").await?;
            let count = existing.len() as u32 + 1;
            let vcpus = existing.iter().map(|i| i.spec.vcpus).sum::<u32>() + wanted.vcpus;
            let memory =
                existing.iter().map(|i| i.spec.memory_mib).sum::<u64>() + wanted.memory_mib;
            exceeded(quota.instances as u64, count as u64, "instances", "spec")?;
            exceeded(quota.vcpus as u64, vcpus as u64, "vCPUs", "spec.vcpus")?;
            exceeded(quota.memory_mib, memory, "MiB of memory", "spec.memoryMib")?;
        } else {
            let wanted: VolumeSpec = serde_json::from_value(spec.clone())?;
            let existing: Vec<Volume> = self.typed_list(&parent, "volumes").await?;
            let gib = existing.iter().map(|v| v.spec.size_gib).sum::<u64>() + wanted.size_gib;
            exceeded(quota.volume_gib, gib, "GiB of volume", "spec.sizeGib")?;
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
