//! Reconciliation as arithmetic.
//!
//! Every decision in this system is a pure function of `(spec, observed)`. No
//! clock, no store, no network, no ordering assumption — so every decision is
//! unit-testable without a cluster, and running it twice is the same as running
//! it once (invariant 3).
//!
//! A caller reads the world, calls one of these, and performs what comes back.
//! If it dies half way through, the next pass computes the same list minus what
//! already happened. That is the whole recovery model: there is nothing to
//! resume, because nothing was ever "in progress".

use crate::{
    meta::{Condition, ConditionStatus, Meta, Timestamp},
    resources::{
        Attachment, Capacity, DesiredState, Instance, InstanceState, NODE_RELEASE_FINALIZER, Node,
        Observed, OperationSpec, Quota, Resource, Volume,
    },
};

/// Something an agent must do to close the gap. Deliberately coarse: an action
/// is a thing that either happened or did not, and can be asked for again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Fetch and verify an image before it can be booted from.
    PullImage {
        digest: String,
    },
    /// Program the datapath for a port. Idempotent by construction — the fabric
    /// takes a desired map, not a delta.
    ProgramPort {
        port: String,
    },
    UnprogramPort {
        port: String,
    },
    CreateDisk {
        instance: String,
        gib: u64,
        /// The image the disk starts life as a copy of.
        ///
        /// Carried here rather than looked up on the node, because cloning the
        /// image *is* creating the disk. It used to be created empty and the
        /// image was never used at all: every instance this platform started
        /// booted a blank disk, which on a direct-kernel boot looks exactly
        /// like a guest that is simply slow.
        image: String,
    },
    StartVm {
        instance: String,
    },
    StopVm {
        instance: String,
    },
    /// The VMM is gone but the guest should be running: start it again. Named
    /// separately from `StartVm` so the log says what happened rather than
    /// leaving an operator to infer a crash from two identical lines.
    RestartCrashedVm {
        instance: String,
    },
    DeleteVm {
        instance: String,
    },
    OpenVolume {
        volume: String,
        read_only: bool,
    },
    CloseVolume {
        volume: String,
    },
    /// Drop the finalizer: this node no longer holds the object.
    ReleaseFinalizer {
        who: String,
    },
}

/// What a controller decided about a resource it does not own the status of.
#[derive(Clone, Debug, PartialEq)]
pub enum ControllerAction {
    /// Bind the instance to a node (a spec write — the scheduler is a
    /// controller).
    Assign { instance: String, node: String },
    /// The object may finally go.
    Delete { name: String },
}

/// What the node agent should do about one instance.
///
/// The order is load-bearing: image, then disk, then ports, then the VM. A VM
/// started before its port is programmed is a guest that comes up on a dead
/// network and has to be poked afterwards — which is an operation nobody can
/// make idempotent.
pub fn reconcile_instance(
    instance: &Instance,
    image_cached: bool,
    ports_programmed: &[bool],
    disk_present: bool,
) -> Vec<Action> {
    let name = instance.meta.name.to_string();
    let mut actions = Vec::new();

    // A deleted object is torn down in the reverse order it was built, and the
    // finalizer goes last so nothing can observe a half-removed instance.
    if instance.meta.is_deleting() {
        if instance.status.state != InstanceState::Unknown {
            actions.push(Action::DeleteVm {
                instance: name.clone(),
            });
        }
        for port in &instance.spec.ports {
            actions.push(Action::UnprogramPort { port: port.clone() });
        }
        if instance.meta.has_finalizer(NODE_RELEASE_FINALIZER) {
            actions.push(Action::ReleaseFinalizer {
                who: NODE_RELEASE_FINALIZER.to_string(),
            });
        }
        return actions;
    }

    if !image_cached {
        actions.push(Action::PullImage {
            digest: instance.spec.image.clone(),
        });
    }
    if !disk_present {
        actions.push(Action::CreateDisk {
            instance: name.clone(),
            gib: instance.spec.root_disk_gib,
            image: instance.spec.image.clone(),
        });
    }
    for (i, port) in instance.spec.ports.iter().enumerate() {
        if !ports_programmed.get(i).copied().unwrap_or(false) {
            actions.push(Action::ProgramPort { port: port.clone() });
        }
    }

    // Everything above must be in place before the guest runs. Asking for a VM
    // whose image is still downloading is how a start ends up "failed" for a
    // reason that was only ever a race.
    let ready_to_run = image_cached
        && disk_present
        && instance
            .spec
            .ports
            .iter()
            .enumerate()
            .all(|(i, _)| ports_programmed.get(i).copied().unwrap_or(false));

    match (instance.spec.desired_state, instance.status.state) {
        (DesiredState::Running, InstanceState::Running) => {}
        (DesiredState::Running, InstanceState::Failed) => actions.push(Action::RestartCrashedVm {
            instance: name.clone(),
        }),
        (DesiredState::Running, _) if ready_to_run => {
            actions.push(Action::StartVm {
                instance: name.clone(),
            });
        }
        (DesiredState::Running, _) => {}
        (DesiredState::Stopped, InstanceState::Running) => actions.push(Action::StopVm {
            instance: name.clone(),
        }),
        (DesiredState::Stopped, _) => {}
    }
    actions
}

/// What the node agent should do about one attachment.
///
/// The finalizer is the entire point. `spec.attached_to` disappearing does not
/// mean the volume is free — it means the node has been *asked* to let go. Only
/// after the node has closed it, reported `attached = false`, and dropped its
/// finalizer may the object go and the volume be attached elsewhere. Two nodes
/// with the same RBD image open is the failure this prevents.
pub fn reconcile_attachment(attachment: &Attachment) -> Vec<Action> {
    let mut actions = Vec::new();
    if attachment.meta.is_deleting() {
        if attachment.status.attached {
            actions.push(Action::CloseVolume {
                volume: attachment.spec.volume.clone(),
            });
            // Not released yet: the finalizer goes only once the close has been
            // observed, which is the next pass.
            return actions;
        }
        if attachment.meta.has_finalizer(NODE_RELEASE_FINALIZER) {
            actions.push(Action::ReleaseFinalizer {
                who: NODE_RELEASE_FINALIZER.to_string(),
            });
        }
        return actions;
    }
    if !attachment.status.attached {
        actions.push(Action::OpenVolume {
            volume: attachment.spec.volume.clone(),
            read_only: attachment.spec.read_only,
        });
    }
    actions
}

/// Why a node was not chosen. The chain of these *is* the explain API: an
/// operator asking "why did this not schedule" gets the filter that removed
/// each candidate, not a log to grep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    Unschedulable,
    NotReady,
    InsufficientVcpus {
        free: u32,
        want: u32,
    },
    InsufficientMemory {
        free_mib: u64,
        want_mib: u64,
    },
    /// The host has the memory, but not on any single NUMA node.
    NoNumaNodeFits {
        want_mib: u64,
    },
    MissingLabel {
        label: String,
    },
    AntiAffinity {
        group: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    pub node: String,
    pub why: Rejected,
}

/// Choose a node, or explain every rejection.
///
/// Optimistic by construction: this reads reported capacity and returns a
/// candidate. The caller writes the assignment with a compare-and-swap on the
/// node's revision and retries on conflict. There is no claim, no reservation,
/// and therefore no reservation to leak when a scheduler dies — the bug that
/// makes a cluster report "no valid host" while half of it is idle.
pub fn place(
    instance: &Instance,
    nodes: &[Node],
    occupied_groups: &[(String, String)],
) -> Result<String, Vec<Explanation>> {
    let mut rejected = Vec::new();
    let mut best: Option<(&Node, u64)> = None;

    for node in nodes {
        let id = node.meta.name.id().to_string();
        if !node.spec.schedulable {
            rejected.push(Explanation {
                node: id,
                why: Rejected::Unschedulable,
            });
            continue;
        }
        let ready = crate::meta::condition(&node.status.conditions, "Ready")
            .map(|c| c.status == ConditionStatus::True)
            .unwrap_or(false);
        if !ready {
            rejected.push(Explanation {
                node: id,
                why: Rejected::NotReady,
            });
            continue;
        }
        if let Some(missing) = instance
            .spec
            .placement_policy
            .required_labels
            .iter()
            .find(|l| !node.spec.labels.contains(l))
        {
            rejected.push(Explanation {
                node: id,
                why: Rejected::MissingLabel {
                    label: missing.clone(),
                },
            });
            continue;
        }
        if let Some(group) = &instance.spec.placement_policy.anti_affinity_group {
            if occupied_groups.iter().any(|(g, n)| g == group && n == &id) {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::AntiAffinity {
                        group: group.clone(),
                    },
                });
                continue;
            }
        }

        let free = free_capacity(node);
        if free.vcpus < instance.spec.vcpus {
            rejected.push(Explanation {
                node: id,
                why: Rejected::InsufficientVcpus {
                    free: free.vcpus,
                    want: instance.spec.vcpus,
                },
            });
            continue;
        }
        if free.memory_mib < instance.spec.memory_mib {
            rejected.push(Explanation {
                node: id,
                why: Rejected::InsufficientMemory {
                    free_mib: free.memory_mib,
                    want_mib: instance.spec.memory_mib,
                },
            });
            continue;
        }
        // A guest is pinned to one NUMA node, so the total is not the question.
        // Reporting the total and then failing at boot is the shape of the
        // "scheduled but will not start" bug.
        if !node.status.capacity.numa_free_mib.is_empty()
            && !node
                .status
                .capacity
                .numa_free_mib
                .iter()
                .any(|m| *m >= instance.spec.memory_mib)
        {
            rejected.push(Explanation {
                node: id,
                why: Rejected::NoNumaNodeFits {
                    want_mib: instance.spec.memory_mib,
                },
            });
            continue;
        }

        // Least-loaded wins, so one node does not collect every small instance
        // while its neighbours idle.
        let score = free.memory_mib;
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((node, score));
        }
    }

    match best {
        Some((node, _)) => Ok(node.meta.name.id().to_string()),
        None => Err(rejected),
    }
}

fn free_capacity(node: &Node) -> Capacity {
    let c = &node.status.capacity;
    let a = &node.status.allocated;
    Capacity {
        vcpus: c.vcpus.saturating_sub(a.vcpus),
        memory_mib: c.memory_mib.saturating_sub(a.memory_mib),
        disk_gib: c.disk_gib.saturating_sub(a.disk_gib),
        numa_free_mib: c.numa_free_mib.clone(),
        hugepages_1gi: c.hugepages_1gi.saturating_sub(a.hugepages_1gi),
    }
}

/// The condition an instance should carry, given what the node reported.
///
/// One place decides what "ready" means, so the API, the console and an alert
/// cannot disagree about it.
pub fn instance_condition(instance: &Instance) -> Condition {
    let at_generation = instance.meta.generation;
    if !instance.converged() {
        return Condition::new(
            "Ready",
            ConditionStatus::Unknown,
            "Converging",
            "the node has not reported on this change yet",
            instance.status.observed_generation,
        );
    }
    match (instance.spec.desired_state, instance.status.state) {
        (DesiredState::Running, InstanceState::Running) => Condition::ready(at_generation),
        (DesiredState::Stopped, InstanceState::Stopped) => Condition::new(
            "Ready",
            ConditionStatus::True,
            "Stopped",
            "stopped, as asked",
            at_generation,
        ),
        (_, InstanceState::Failed) => Condition::new(
            "Ready",
            ConditionStatus::False,
            "VmFailed",
            "the virtual machine exited and could not be restarted",
            at_generation,
        ),
        (_, InstanceState::Unknown) => Condition::new(
            "Ready",
            ConditionStatus::Unknown,
            "NoReport",
            "no node has reported on this instance",
            at_generation,
        ),
        (want, is) => Condition::new(
            "Ready",
            ConditionStatus::False,
            "WrongState",
            &format!("wanted {want:?}, the node reports {is:?}"),
            at_generation,
        ),
    }
}

/// Whether a controller may finally remove an object: deletion was asked for
/// and every finalizer has been released.
pub fn may_delete(meta: &Meta) -> bool {
    meta.is_deleting() && meta.finalizers.is_empty()
}

/// Whether a controller may write an object's `status` at all.
///
/// Invariant 1 hands `status` to the owning agent — but some objects have no
/// agent and never will. A project's `used` quota is counted from what exists,
/// an operation's `done` is computed from its target, and an instance that has
/// not been placed yet is owned by nobody, which is exactly when the scheduler
/// has to say on the object *why* it could not place it.
///
/// So the rule is the same rule, stated where the object has no agent: **a
/// controller may write `status` only while no agent owns it.** The moment an
/// agent takes ownership, the controller is out — there is still exactly one
/// writer, and it is still never two at once.
pub fn controller_may_write_status(owner: Option<&str>) -> bool {
    owner.is_none()
}

/// Whether the scheduler has anything to do with this instance.
///
/// Deliberately not "has no node **or** the node is gone": moving a running
/// instance is a migration, which is a deliberate act with its own resource. A
/// scheduler that re-places on its own would restart a guest because a node
/// stopped heartbeating for thirty seconds.
pub fn needs_placement(instance: &Instance) -> bool {
    instance.spec.node.is_none() && !instance.meta.is_deleting()
}

/// The rejection chain, as the sentence an operator reads on the object.
///
/// The same chain the explain API returns, rendered once here so the two can
/// never tell different stories about the same failure.
pub fn unschedulable_condition(why: &[Explanation], at_generation: u64) -> Condition {
    let message = if why.is_empty() {
        "no node exists in this cell".to_string()
    } else {
        let each: Vec<String> = why
            .iter()
            .map(|e| format!("{}: {}", e.node, describe(&e.why)))
            .collect();
        each.join("; ")
    };
    Condition::new(
        "Ready",
        ConditionStatus::False,
        "NoValidHost",
        &message,
        at_generation,
    )
}

fn describe(why: &Rejected) -> String {
    match why {
        Rejected::Unschedulable => "draining".to_string(),
        Rejected::NotReady => "not ready".to_string(),
        Rejected::InsufficientVcpus { free, want } => format!("{free} vcpus free, {want} wanted"),
        Rejected::InsufficientMemory { free_mib, want_mib } => {
            format!("{free_mib} MiB free, {want_mib} MiB wanted")
        }
        Rejected::NoNumaNodeFits { want_mib } => {
            format!("no single NUMA node holds {want_mib} MiB")
        }
        Rejected::MissingLabel { label } => format!("missing label {label}"),
        Rejected::AntiAffinity { group } => format!("already runs a member of {group}"),
    }
}

/// What an instance's condition should say between being placed and the node
/// first reporting on it.
///
/// `Unknown` rather than anything hopeful: the scheduler knows where the
/// instance should run and nothing at all about whether it does.
pub fn scheduled_condition(node: &str, at_generation: u64) -> Condition {
    Condition::new(
        "Ready",
        ConditionStatus::Unknown,
        "Scheduled",
        &format!("placed on {node}, waiting for the node to report"),
        at_generation,
    )
}

/// What a controller owes an object's finalizer, whoever holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalizerStep {
    /// Take the guard before anything can start depending on the object.
    Add,
    /// Everybody has let go; the object may be removed.
    Delete,
    /// Somebody still holds it, or there is nothing to do.
    Wait,
}

/// The controller half of the finalizer dance.
///
/// The ordering is the whole safety property, and it is one `if` in each
/// direction: the guard goes on **before** the object can be acted upon, and
/// comes off only when every holder has released it. A controller that adds a
/// finalizer to an object already being deleted would pin it forever, which is
/// why that case is a `Wait` and not an `Add`.
pub fn finalizer_step(meta: &Meta, who: &str) -> FinalizerStep {
    if meta.is_deleting() {
        if may_delete(meta) {
            return FinalizerStep::Delete;
        }
        return FinalizerStep::Wait;
    }
    if meta.has_finalizer(who) {
        FinalizerStep::Wait
    } else {
        FinalizerStep::Add
    }
}

/// What a project has in use, counted from what exists.
///
/// Counted, never incremented: a counter that is decremented on delete drifts
/// the first time a controller dies between the delete and the decrement, and a
/// quota that drifts downward silently stops admitting work.
///
/// An object being deleted still counts. It still occupies its node until its
/// finalizers are released, and a quota that frees before the resource does is
/// a quota that lets a project overcommit a cell.
pub fn count_quota(
    project: &crate::meta::ResourceName,
    instances: &[Instance],
    volumes: &[Volume],
) -> Quota {
    let mut used = Quota::default();
    for instance in instances.iter().filter(|i| i.meta.name.is_under(project)) {
        used.instances = used.instances.saturating_add(1);
        used.vcpus = used.vcpus.saturating_add(instance.spec.vcpus);
        used.memory_mib = used.memory_mib.saturating_add(instance.spec.memory_mib);
        used.volume_gib = used.volume_gib.saturating_add(instance.spec.root_disk_gib);
    }
    for volume in volumes.iter().filter(|v| v.meta.name.is_under(project)) {
        used.volume_gib = used.volume_gib.saturating_add(volume.spec.size_gib);
    }
    used
}

/// What a project's condition should say about the limits it is under.
///
/// A project can be over its quota without anybody having done anything wrong —
/// an operator lowering a limit is the usual way — so this is a fact reported
/// on the object rather than an error thrown at whoever asked next.
///
/// A limit of zero means unset, not "none allowed". A project created before
/// anybody decided its limits would otherwise report itself broken the moment
/// it was used, and "unset" is what a default of zero actually means here.
pub fn quota_condition(limit: &Quota, used: &Quota, at_generation: u64) -> Condition {
    let over: Vec<&str> = [
        (used.instances > limit.instances && limit.instances > 0).then_some("instances"),
        (used.vcpus > limit.vcpus && limit.vcpus > 0).then_some("vcpus"),
        (used.memory_mib > limit.memory_mib && limit.memory_mib > 0).then_some("memory"),
        (used.volume_gib > limit.volume_gib && limit.volume_gib > 0).then_some("storage"),
    ]
    .into_iter()
    .flatten()
    .collect();

    if over.is_empty() {
        return Condition::ready(at_generation);
    }
    Condition::new(
        "Ready",
        ConditionStatus::False,
        "OverQuota",
        &format!("over the {} limit", over.join(", ")),
        at_generation,
    )
}

/// The target of an operation, as much of it as an operation needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetView {
    /// Nothing is stored under the target's name any more.
    Gone,
    Present {
        observed_generation: u64,
        ready: ConditionStatus,
        reason: String,
        message: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationProgress {
    pub done: bool,
    pub error: Option<String>,
}

/// Whether an operation has finished, computed from its target and nothing
/// else.
///
/// This is what makes AIP-151 honest here: `done` is never a fact somebody
/// remembered to write, so an operation cannot outlive the truth of the object
/// it describes. A controller that dies between "the instance came up" and
/// "mark the operation done" leaves an operation that computes to done on the
/// next pass, not one that says `false` forever.
pub fn operation_progress(spec: &OperationSpec, target: &TargetView) -> OperationProgress {
    match target {
        // For a delete, the object being gone *is* the success. For anything
        // else it means the thing the caller asked about no longer exists, and
        // an operation that waits for it would wait forever.
        TargetView::Gone if spec.verb == "delete" => OperationProgress {
            done: true,
            error: None,
        },
        TargetView::Gone => OperationProgress {
            done: true,
            error: Some(format!("{} no longer exists", spec.target)),
        },
        TargetView::Present {
            observed_generation,
            ..
        } if *observed_generation < spec.target_generation => OperationProgress::default(),
        TargetView::Present { ready, .. } if *ready == ConditionStatus::Unknown => {
            OperationProgress::default()
        }
        TargetView::Present {
            ready,
            reason,
            message,
            ..
        } if *ready == ConditionStatus::False => OperationProgress {
            done: true,
            error: Some(if message.is_empty() {
                reason.clone()
            } else {
                format!("{reason}: {message}")
            }),
        },
        TargetView::Present { .. } => OperationProgress {
            done: true,
            error: None,
        },
    }
}

/// Why an object is not where it should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergenceReason {
    /// The world has not caught up with the spec.
    Unconverged,
    /// Caught up, and reporting that it is not healthy.
    NotReady,
    /// Caught up, and nobody has said anything about it.
    Unreported,
    /// Deletion was asked for and a finalizer is still held.
    DeletionBlocked,
}

impl DivergenceReason {
    /// The metric label. Stable, because a dashboard is written against it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unconverged => "Unconverged",
            Self::NotReady => "NotReady",
            Self::Unreported => "Unreported",
            Self::DeletionBlocked => "DeletionBlocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub reason: DivergenceReason,
    /// When this began, as closely as the object can say.
    ///
    /// A lower bound, and knowingly so: the last condition transition is when
    /// somebody last said something about the object, and for a spec change
    /// nobody has acted on yet that is the moment before it. Storing a
    /// "spec changed at" would be a second thing that has to agree with
    /// `generation`, and two facts that must agree are one fact and one bug.
    pub since: Timestamp,
}

/// Whether an object is where it should be — the one definition the drift
/// metric, the console and an alert all read.
pub fn divergence<S, T: Observed>(resource: &Resource<S, T>) -> Option<Divergence> {
    let meta = &resource.meta;
    if meta.is_deleting() {
        return (!meta.finalizers.is_empty()).then(|| Divergence {
            reason: DivergenceReason::DeletionBlocked,
            since: meta.deleted_at.unwrap_or(meta.created_at),
        });
    }
    let ready = crate::meta::condition(resource.status.conditions(), "Ready");
    let since = ready.map(|c| c.last_transition).unwrap_or(meta.created_at);
    if !resource.converged() {
        return Some(Divergence {
            reason: DivergenceReason::Unconverged,
            since,
        });
    }
    match ready.map(|c| c.status) {
        Some(ConditionStatus::True) => None,
        Some(ConditionStatus::False) => Some(Divergence {
            reason: DivergenceReason::NotReady,
            since,
        }),
        // Both "reported as unknown" and "never reported at all" are the same
        // thing to somebody looking at a cluster: an object nothing is looking
        // after.
        _ => Some(Divergence {
            reason: DivergenceReason::Unreported,
            since,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        meta::{Meta, Placement, ResourceName, set_condition},
        resources::{
            AttachmentStatus, InstanceSpec, InstanceStatus, NodeSpec, NodeStatus, Resource,
        },
    };

    fn inst(name: &str) -> Instance {
        Resource::new(
            Meta::new(
                ResourceName::parse(name).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                vcpus: 2,
                memory_mib: 2048,
                image: "sha256:abc".into(),
                root_disk_gib: 20,
                ports: vec!["projects/p1/ports/port-a".into()],
                ..Default::default()
            },
            InstanceStatus::default(),
        )
    }

    fn node(id: &str, vcpus: u32, mem: u64) -> Node {
        let mut n = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("nodes/{id}")).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            NodeSpec {
                schedulable: true,
                labels: vec![],
            },
            NodeStatus {
                capacity: Capacity {
                    vcpus,
                    memory_mib: mem,
                    disk_gib: 1000,
                    numa_free_mib: vec![mem],
                    hugepages_1gi: 0,
                },
                ..Default::default()
            },
        );
        set_condition(&mut n.status.conditions, Condition::ready(1));
        n
    }

    #[test]
    fn nothing_starts_before_what_it_needs_exists() {
        let i = inst("projects/p1/instances/i1");
        let actions = reconcile_instance(&i, false, &[false], false);
        assert!(actions.contains(&Action::PullImage {
            digest: "sha256:abc".into()
        }));
        assert!(actions.contains(&Action::ProgramPort {
            port: "projects/p1/ports/port-a".into()
        }));
        assert!(
            !actions.iter().any(|a| matches!(a, Action::StartVm { .. })),
            "a VM was asked for before its image and port existed"
        );
    }

    #[test]
    fn once_everything_is_in_place_the_vm_starts() {
        let i = inst("projects/p1/instances/i1");
        let actions = reconcile_instance(&i, true, &[true], true);
        assert_eq!(
            actions,
            vec![Action::StartVm {
                instance: "projects/p1/instances/i1".into()
            }]
        );
    }

    #[test]
    fn reconciling_a_settled_instance_asks_for_nothing() {
        // Idempotence, stated as a test: the second pass over a converged
        // object must be empty, or every resync would churn the cluster.
        let mut i = inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Running;
        assert!(reconcile_instance(&i, true, &[true], true).is_empty());
    }

    #[test]
    fn a_crashed_guest_is_restarted_and_says_so() {
        let mut i = inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Failed;
        let actions = reconcile_instance(&i, true, &[true], true);
        assert_eq!(
            actions,
            vec![Action::RestartCrashedVm {
                instance: "projects/p1/instances/i1".into()
            }]
        );
    }

    #[test]
    fn deleting_tears_down_before_it_lets_go() {
        let mut i = inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Running;
        i.meta.deleted_at = Some(crate::meta::Timestamp::now());
        i.meta.add_finalizer(NODE_RELEASE_FINALIZER);
        let actions = reconcile_instance(&i, true, &[true], true);
        let del = actions
            .iter()
            .position(|a| matches!(a, Action::DeleteVm { .. }));
        let rel = actions
            .iter()
            .position(|a| matches!(a, Action::ReleaseFinalizer { .. }));
        assert!(del < rel, "the finalizer went before the machine was gone");
    }

    #[test]
    fn an_attachment_is_not_released_until_the_node_has_closed_it() {
        let mut a = Attachment::new(
            Meta::new(
                ResourceName::parse("projects/p1/attachments/a1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            crate::resources::AttachmentSpec {
                volume: "projects/p1/volumes/v1".into(),
                instance: "projects/p1/instances/i1".into(),
                node: "node-a".into(),
                read_only: false,
            },
            AttachmentStatus {
                attached: true,
                node: Some("node-a".into()),
                ..Default::default()
            },
        );
        a.meta.deleted_at = Some(crate::meta::Timestamp::now());
        a.meta.add_finalizer(NODE_RELEASE_FINALIZER);

        // Still open: close it, and hold the finalizer.
        let first = reconcile_attachment(&a);
        assert_eq!(
            first,
            vec![Action::CloseVolume {
                volume: "projects/p1/volumes/v1".into()
            }]
        );
        assert!(
            !may_delete(&a.meta),
            "the volume could have been reattached elsewhere while still open"
        );

        // The node reports it closed; only now does the finalizer go.
        a.status.attached = false;
        assert_eq!(
            reconcile_attachment(&a),
            vec![Action::ReleaseFinalizer {
                who: NODE_RELEASE_FINALIZER.into()
            }]
        );
        a.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
        assert!(may_delete(&a.meta));
    }

    #[test]
    fn placement_picks_the_emptiest_node_that_fits() {
        let i = inst("projects/p1/instances/i1");
        let nodes = vec![node("a", 8, 4096), node("b", 8, 16384)];
        assert_eq!(place(&i, &nodes, &[]).unwrap(), "b");
    }

    #[test]
    fn placement_explains_every_rejection() {
        let mut i = inst("projects/p1/instances/i1");
        i.spec.memory_mib = 99999;
        let nodes = vec![node("a", 8, 4096), node("b", 8, 16384)];
        let why = place(&i, &nodes, &[]).unwrap_err();
        assert_eq!(why.len(), 2, "an operator must learn about every candidate");
        assert!(matches!(why[0].why, Rejected::InsufficientMemory { .. }));
    }

    #[test]
    fn a_host_with_the_memory_but_not_on_one_numa_node_is_refused() {
        // Scheduling here would succeed and the guest would fail to start —
        // the worst outcome, because it looks like a hypervisor fault.
        let mut i = inst("projects/p1/instances/i1");
        i.spec.memory_mib = 8192;
        let mut n = node("a", 8, 16384);
        n.status.capacity.numa_free_mib = vec![4096, 4096];
        let why = place(&i, &[n], &[]).unwrap_err();
        assert_eq!(why[0].why, Rejected::NoNumaNodeFits { want_mib: 8192 });
    }

    #[test]
    fn anti_affinity_keeps_a_group_off_one_host() {
        let mut i = inst("projects/p1/instances/i1");
        i.spec.placement_policy.anti_affinity_group = Some("web".into());
        let nodes = vec![node("a", 8, 16384)];
        let occupied = vec![("web".to_string(), "a".to_string())];
        assert!(place(&i, &nodes, &occupied).is_err());
    }

    #[test]
    fn a_draining_node_takes_nothing_new() {
        let i = inst("projects/p1/instances/i1");
        let mut n = node("a", 8, 16384);
        n.spec.schedulable = false;
        let why = place(&i, &[n], &[]).unwrap_err();
        assert_eq!(why[0].why, Rejected::Unschedulable);
    }

    #[test]
    fn an_unconverged_instance_is_unknown_rather_than_wrong() {
        let mut i = inst("projects/p1/instances/i1");
        i.meta.generation = 2;
        i.status.observed_generation = 1;
        i.status.state = InstanceState::Stopped;
        let c = instance_condition(&i);
        assert_eq!(c.status, ConditionStatus::Unknown);
        assert_eq!(c.reason, "Converging");
    }

    #[test]
    fn a_controller_may_speak_for_an_object_only_while_no_agent_owns_it() {
        assert!(controller_may_write_status(None));
        assert!(
            !controller_may_write_status(Some("node-a")),
            "a controller and an agent would both be writing one status"
        );
    }

    #[test]
    fn only_an_unplaced_instance_is_the_schedulers_business() {
        let mut i = inst("projects/p1/instances/i1");
        assert!(needs_placement(&i));
        i.spec.node = Some("node-a".into());
        assert!(
            !needs_placement(&i),
            "a placed instance was re-placed, which is a migration nobody asked for"
        );
        i.spec.node = None;
        i.meta.deleted_at = Some(crate::meta::Timestamp::now());
        assert!(
            !needs_placement(&i),
            "an instance on its way out was placed"
        );
    }

    #[test]
    fn a_placement_failure_reads_as_a_sentence_about_every_candidate() {
        let why = vec![
            Explanation {
                node: "node-a".into(),
                why: Rejected::InsufficientMemory {
                    free_mib: 4096,
                    want_mib: 8192,
                },
            },
            Explanation {
                node: "node-b".into(),
                why: Rejected::Unschedulable,
            },
        ];
        let c = unschedulable_condition(&why, 3);
        assert_eq!(c.reason, "NoValidHost");
        assert!(c.message.contains("node-a: 4096 MiB free, 8192 MiB wanted"));
        assert!(c.message.contains("node-b: draining"));
        // An empty cell must still say something an operator can act on.
        assert!(unschedulable_condition(&[], 3).message.contains("no node"));
    }

    #[test]
    fn a_finalizer_goes_on_before_use_and_comes_off_only_when_everybody_let_go() {
        let mut m = Meta::new(
            ResourceName::parse("projects/p1/attachments/a1").unwrap(),
            Placement::new("eu", "cell-1"),
        );
        assert_eq!(
            finalizer_step(&m, NODE_RELEASE_FINALIZER),
            FinalizerStep::Add
        );
        m.add_finalizer(NODE_RELEASE_FINALIZER);
        assert_eq!(
            finalizer_step(&m, NODE_RELEASE_FINALIZER),
            FinalizerStep::Wait
        );

        m.deleted_at = Some(crate::meta::Timestamp::now());
        assert_eq!(
            finalizer_step(&m, NODE_RELEASE_FINALIZER),
            FinalizerStep::Wait,
            "the object went while a node still had the volume open"
        );
        m.remove_finalizer(NODE_RELEASE_FINALIZER);
        assert_eq!(
            finalizer_step(&m, NODE_RELEASE_FINALIZER),
            FinalizerStep::Delete
        );
    }

    #[test]
    fn a_finalizer_is_never_added_to_something_already_going_away() {
        // It would be added, nobody would ever release it, and the object would
        // be undeletable without a human editing the store.
        let mut m = Meta::new(
            ResourceName::parse("projects/p1/attachments/a1").unwrap(),
            Placement::new("eu", "cell-1"),
        );
        m.deleted_at = Some(crate::meta::Timestamp::now());
        assert_eq!(
            finalizer_step(&m, NODE_RELEASE_FINALIZER),
            FinalizerStep::Delete
        );
    }

    #[test]
    fn quota_is_counted_from_what_exists_including_what_is_going_away() {
        let project = ResourceName::parse("projects/p1").unwrap();
        let mut mine = inst("projects/p1/instances/i1");
        mine.spec.root_disk_gib = 20;
        let mut dying = inst("projects/p1/instances/i2");
        dying.meta.deleted_at = Some(crate::meta::Timestamp::now());
        let theirs = inst("projects/p2/instances/i1");

        let used = count_quota(&project, &[mine, dying, theirs], &[]);
        assert_eq!(
            used.instances, 2,
            "another project's instance was charged here"
        );
        assert_eq!(used.vcpus, 4);
        // The dying instance still occupies its node until its finalizers go;
        // freeing the quota first is how a project overcommits a cell.
        assert_eq!(used.memory_mib, 4096);
    }

    #[test]
    fn a_project_over_a_lowered_limit_says_so_rather_than_failing() {
        let limit = Quota {
            instances: 2,
            vcpus: 8,
            memory_mib: 8192,
            volume_gib: 100,
        };
        let within = Quota {
            instances: 1,
            vcpus: 2,
            memory_mib: 2048,
            volume_gib: 20,
        };
        assert_eq!(
            quota_condition(&limit, &within, 1).status,
            ConditionStatus::True
        );

        let over = Quota {
            instances: 3,
            vcpus: 32,
            ..within.clone()
        };
        let c = quota_condition(&limit, &over, 1);
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "OverQuota");
        assert!(c.message.contains("instances"), "{}", c.message);
        assert!(c.message.contains("vcpus"), "{}", c.message);

        // An unset limit is unset, not zero — otherwise every project is broken
        // until somebody remembers to set four numbers on it.
        assert_eq!(
            quota_condition(&Quota::default(), &over, 1).status,
            ConditionStatus::True
        );
    }

    #[test]
    fn an_operation_is_not_done_until_its_target_has_caught_up() {
        let spec = OperationSpec {
            target: "projects/p1/instances/i1".into(),
            target_generation: 4,
            verb: "update".into(),
            requested_by: "someone".into(),
        };
        let behind = TargetView::Present {
            observed_generation: 3,
            ready: ConditionStatus::True,
            reason: "Ready".into(),
            message: String::new(),
        };
        assert!(!operation_progress(&spec, &behind).done);

        let caught_up = TargetView::Present {
            observed_generation: 4,
            ready: ConditionStatus::True,
            reason: "Ready".into(),
            message: String::new(),
        };
        assert_eq!(
            operation_progress(&spec, &caught_up),
            OperationProgress {
                done: true,
                error: None
            }
        );
    }

    #[test]
    fn an_operation_carries_the_targets_failure_rather_than_hanging() {
        let spec = OperationSpec {
            target: "projects/p1/instances/i1".into(),
            target_generation: 1,
            verb: "create".into(),
            requested_by: "someone".into(),
        };
        let failed = TargetView::Present {
            observed_generation: 1,
            ready: ConditionStatus::False,
            reason: "NoValidHost".into(),
            message: "node-a: draining".into(),
        };
        let progress = operation_progress(&spec, &failed);
        assert!(progress.done);
        assert_eq!(
            progress.error.as_deref(),
            Some("NoValidHost: node-a: draining"),
            "the caller was left to guess why"
        );
    }

    #[test]
    fn a_delete_operation_finishes_when_the_object_is_gone_and_others_fail() {
        let mut spec = OperationSpec {
            target: "projects/p1/instances/i1".into(),
            target_generation: 1,
            verb: "delete".into(),
            requested_by: "someone".into(),
        };
        assert_eq!(
            operation_progress(&spec, &TargetView::Gone),
            OperationProgress {
                done: true,
                error: None
            }
        );
        spec.verb = "create".into();
        // Otherwise this operation is polled forever by a client waiting for an
        // object that will never exist.
        assert!(operation_progress(&spec, &TargetView::Gone).error.is_some());
    }

    #[test]
    fn divergence_is_one_definition_of_not_where_it_should_be() {
        let mut i = inst("projects/p1/instances/i1");
        i.meta.generation = 2;
        i.status.observed_generation = 1;
        assert_eq!(
            divergence(&i).unwrap().reason,
            DivergenceReason::Unconverged
        );

        i.status.observed_generation = 2;
        set_condition(&mut i.status.conditions, Condition::ready(2));
        assert!(
            divergence(&i).is_none(),
            "a healthy object was counted as drift"
        );

        set_condition(
            &mut i.status.conditions,
            Condition::new("Ready", ConditionStatus::False, "VmFailed", "exited", 2),
        );
        assert_eq!(divergence(&i).unwrap().reason, DivergenceReason::NotReady);
    }

    #[test]
    fn an_object_stuck_on_a_finalizer_is_its_own_kind_of_drift() {
        // "Deleted three hours ago and still here" is the single most useful
        // thing a drift metric can say, and it is invisible if a deleting
        // object is judged by its conditions like any other.
        let mut a = Attachment::new(
            Meta::new(
                ResourceName::parse("projects/p1/attachments/a1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            crate::resources::AttachmentSpec::default(),
            AttachmentStatus::default(),
        );
        a.meta.deleted_at = Some(crate::meta::Timestamp(1000));
        a.meta.add_finalizer(NODE_RELEASE_FINALIZER);
        let d = divergence(&a).unwrap();
        assert_eq!(d.reason, DivergenceReason::DeletionBlocked);
        assert_eq!(d.since, crate::meta::Timestamp(1000));

        a.meta.remove_finalizer(NODE_RELEASE_FINALIZER);
        assert!(
            divergence(&a).is_none(),
            "an object that may now go was counted as stuck"
        );
    }
}
