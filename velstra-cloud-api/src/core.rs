//! The handlers. Both transports are skins over this file.
//!
//! Everything a caller can ask for is a method here, and neither the REST
//! router nor the gRPC service is allowed to decide anything: they parse a
//! request, call one of these, and render the answer. A rule that lived in one
//! of them — "reject a `status` write", "bump the generation" — would hold on
//! one transport and not the other, and the two would drift apart in the exact
//! place where nobody looks.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use velstra_cloud_model::{
    meta::{Meta, Placement, ResourceName, Revision, Timestamp, set_condition},
    migration::{Migration, MigrationSpec, MigrationStatus, may_migrate, migration_condition},
    reconcile::place,
    resources::{
        AttachmentSpec, AttachmentStatus, ImageSpec, ImageStatus, Instance, InstanceSpec,
        InstanceStatus, NetworkSpec, NetworkStatus, Node, NodeSpec, NodeStatus, OperationSpec,
        OperationStatus, PortSpec, PortStatus, ProjectSpec, ProjectStatus, Resource, SubnetSpec,
        SubnetStatus, Volume, VolumeSpec, VolumeStatus, nodes_holding,
    },
};
use velstra_cloud_store::{Event, Store};

use crate::{
    auth::{Identity, TokenVerifier},
    collection::{Collection, Deleted, Patch, TypedCollection, merge},
    error::{ApiError, ApiResult, Code},
    json::joined,
};

/// The collections this API serves, in the order `docs/rest-contract.md` lists
/// them. A name that is not here is a 404 rather than an empty list: an
/// interface that answers a typo with `[]` sends somebody looking for their
/// missing objects.
pub const COLLECTIONS: [&str; 11] = [
    "projects",
    "instances",
    "migrations",
    "volumes",
    "attachments",
    "networks",
    "subnets",
    "ports",
    "images",
    "nodes",
    "operations",
];

/// One event on a watch, in the two shapes the contract defines.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEvent {
    Put(Value),
    Delete { name: String, revision: Revision },
}

/// A list, and the revision to watch from so nothing between the two is lost.
pub struct Listing {
    pub items: Vec<Value>,
    pub revision: Revision,
}

/// What a create produced: the operation to follow, and the object it is about.
pub struct Created {
    pub operation: Value,
    pub target: String,
}

struct Inner {
    collections: BTreeMap<&'static str, Arc<dyn Collection>>,
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
            collection!("attachments", AttachmentSpec, AttachmentStatus),
            collection!("networks", NetworkSpec, NetworkStatus),
            collection!("subnets", SubnetSpec, SubnetStatus),
            collection!("ports", PortSpec, PortStatus),
            collection!("images", ImageSpec, ImageStatus),
            collection!("nodes", NodeSpec, NodeStatus),
            collection!("operations", OperationSpec, OperationStatus),
            collection!("migrations", MigrationSpec, MigrationStatus),
        ]);
        Self {
            inner: Arc::new(Inner {
                collections,
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
    pub async fn get(&self, name: &ResourceName) -> ApiResult<Value> {
        let collection = self.collection(name.collection())?;
        let mut document = collection
            .get(&name.to_string())
            .await?
            .ok_or_else(|| ApiError::not_found(name))?;
        self.answer(&mut document).await?;
        Ok(document)
    }

    /// Everything in a collection under `parent`, plus the revision the list is
    /// good from.
    ///
    /// The revision is taken **before** the read, so a watch started from it
    /// may repeat an event but can never skip one. The other order loses
    /// whatever was written while the list was being assembled, which is the
    /// bug that makes a console quietly stale until somebody reloads it.
    pub async fn list(&self, parent: &str, kind: &str) -> ApiResult<Listing> {
        let collection = self.collection(kind)?;
        let revision = collection.revision().await?;
        let mut items = Vec::new();
        for mut document in collection.list().await? {
            if !under(&document, parent) {
                continue;
            }
            self.answer(&mut document).await?;
            items.push(document);
        }
        Ok(Listing { items, revision })
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
        if kind == "attachments" {
            self.settle_node(&mut spec, None).await?;
        }
        if kind == "migrations" {
            self.settle_migration(&mut spec).await?;
        }
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
    ) -> ApiResult<Value> {
        let collection = self.collection(name.collection())?;
        refuse_unwritable(body)?;
        let mut patch = Patch {
            spec: body.get("spec").cloned(),
            labels: body.get("meta").and_then(|m| m.get("labels")).cloned(),
        };
        if let Some(spec) = &mut patch.spec {
            crate::refs::check(name.collection(), spec)?;
            // A change may move an attachment's node — after a migration, to
            // agree with the instance again — but never away from it.
            if name.collection() == "attachments" && spec.get("node").is_some() {
                let stored: Value = self.get(name).await?;
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
        self.answer(&mut document).await?;
        Ok(document)
    }

    /// Ask for a deletion. Two-phase and visible: the object stays readable,
    /// carrying its `deletedAt` and its finalizers, until the last holder lets
    /// go.
    pub async fn delete(
        &self,
        name: &ResourceName,
        expect: Option<Revision>,
    ) -> ApiResult<Deleted> {
        let collection = self.collection(name.collection())?;
        collection.delete(&name.to_string(), expect).await
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
        let collection = self.collection(kind)?;
        let receiver = collection.watch(from);
        let api = self.clone();
        let parent = parent.to_string();
        Ok(
            tokio_stream::wrappers::ReceiverStream::new(receiver).filter_map(move |event| {
                let api = api.clone();
                let collection = collection.clone();
                let parent = parent.clone();
                async move { api.event(&collection, &parent, event).await }
            }),
        )
    }

    async fn event(
        &self,
        collection: &Arc<dyn Collection>,
        parent: &str,
        event: Event,
    ) -> Option<WatchEvent> {
        match event {
            Event::Put(entry) => {
                let mut document = collection.decode(&entry.value, entry.revision).ok()?;
                if !under(&document, parent) {
                    return None;
                }
                self.answer(&mut document).await.ok()?;
                Some(WatchEvent::Put(document))
            }
            Event::Delete { key, revision } => {
                let (_, _, name) = velstra_cloud_store::parse_key(&key)?;
                if !parent.is_empty() && !name.starts_with(&format!("{parent}/")) {
                    return None;
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
    pub async fn explain_placement(&self, name: &ResourceName) -> ApiResult<Value> {
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
    pub async fn wait_operation(&self, name: &ResourceName, timeout: Duration) -> ApiResult<Value> {
        let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(60));
        loop {
            let document = self.get(name).await?;
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
    async fn answer(&self, document: &mut Value) -> ApiResult<()> {
        self.answer_operation(document).await?;
        self.answer_migration(document).await?;
        self.answer_image(document).await
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
    async fn answer_image(&self, document: &mut Value) -> ApiResult<()> {
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
        let held = self.image_cached_on(&name).await?;
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

        let cached = self.image_cached_on(&instance.spec.image).await?;
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
    pub async fn explain_migration(&self, name: &ResourceName) -> ApiResult<Value> {
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
        let cached = self.image_cached_on(&instance.spec.image).await?;

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
    async fn image_cached_on(&self, image: &str) -> ApiResult<Vec<String>> {
        let Ok(name) = ResourceName::parse(image) else {
            return Ok(Vec::new());
        };
        if name.collection() != "images" {
            return Ok(Vec::new());
        }
        let nodes: Vec<Node> = self.typed_list("", "nodes").await?;
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
