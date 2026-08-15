//! Moving a running guest to another node, without it becoming a state.
//!
//! A migration is the hardest test of invariant 2, and the reason it gets its
//! own file. The obvious design is `InstanceState::Migrating` — and it is
//! exactly the bug this platform exists to remove: a controller dies mid-flight,
//! the instance stays `MIGRATING` forever, and somebody has to decide by hand
//! whether the guest is on the old node, the new one, or both.
//!
//! So there is no such state. A migration is a **resource**: an ask that one
//! instance should be running on a different node. Every value in its status is
//! something a node can see on itself right now, and whether it is finished is
//! *computed* from where the instance actually runs — never stored, so it can
//! never disagree with the world.
//!
//! # Who writes what
//!
//! The destination acts first: both Cloud Hypervisor and QEMU require the
//! receiving side to be listening before the source may send. So the
//! **destination owns the migration's status** — it is the party with something
//! to report about it (the receiver's URL, whether it is listening). The source
//! reads that and reports what it always reports: the instance's own status,
//! which it owns until it lets go.
//!
//! That falls out of the ownership rule already in [`crate::access`]: the fact
//! wins while it exists, and the assignee may claim only when nobody holds it.
//! The instance is handed over in exactly one direction and at exactly one
//! moment — when the source says the guest is no longer here.
//!
//! # The order, and why each step is where it is
//!
//! 1. A `Migration` is created. **Nothing about the instance changes yet.**
//! 2. The destination sees a migration assigned to it, starts an empty VMM in
//!    receive mode, and publishes the URL it is listening on.
//! 3. The source sees a listening receiver, sends, and — when the guest is gone
//!    from this machine — reports `instance.status.node = None`.
//! 4. A controller sees the instance released and moves `instance.spec.node` to
//!    the destination. This is a spec write, so it belongs to a controller.
//! 5. The destination claims the instance (nobody holds it, it is the assignee)
//!    and reports it running — on the VMM it has had since step 2.
//!
//! Every step is idempotent and every one is recoverable by looking: if any
//! process dies at any point, the next pass sees the same world and continues.
//! A failed send leaves the guest running on the source, which is the whole
//! point of pre-copy.

use serde::{Deserialize, Serialize};

use crate::{
    meta::{Condition, ConditionStatus},
    resources::{Assigned, DesiredState, Instance, InstanceState, Node, Observed, Resource},
};

/// How the guest should be moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationMode {
    /// Pre-copy: memory is transferred while the guest runs, and it pauses only
    /// for the last dirty pages. The default in both VMMs, and the only mode
    /// where a failure is free — the source still has the guest.
    #[default]
    Live,
    /// Post-copy: the destination resumes first and faults pages in on demand.
    /// Lower total downtime, but a failure mid-flight loses the guest, because
    /// neither side has all of its memory. Never a default.
    PostCopy,
    /// Stop, move, start. Honest about the outage, and the only option for a
    /// guest whose devices cannot be migrated.
    Reboot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrationSpec {
    pub instance: String,
    /// Where it is now. Recorded at create so a migration is legible after the
    /// fact, and so the source can find its own work with one field.
    pub from_node: String,
    /// Where it should be. This is the assignment: the destination's agent
    /// watches for its own name here.
    pub to_node: String,
    pub mode: MigrationMode,
    /// The pause the guest may take at the end, in milliseconds. Both VMMs
    /// default to 300; a busier guest needs a larger budget or it never
    /// converges.
    pub downtime_ms: u32,
    /// Give up after this long. A migration that cannot converge is a migration
    /// that will run until somebody notices.
    pub timeout_s: u32,
    /// Parallel streams for the transfer. One is the default and the only value
    /// a local socket accepts.
    pub connections: u8,
}

impl Default for MigrationSpec {
    fn default() -> Self {
        Self {
            instance: String::new(),
            from_node: String::new(),
            to_node: String::new(),
            mode: MigrationMode::Live,
            downtime_ms: 300,
            timeout_s: 3600,
            connections: 1,
        }
    }
}

impl Assigned for MigrationSpec {
    fn assigned_owner(&self) -> Option<&str> {
        // The destination, because it is the party that must act first and
        // therefore the party with something to report.
        Some(self.to_node.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The destination agent, once it has claimed this object.
    pub node: Option<String>,
    /// Where the source should send: `unix:/path` on one machine,
    /// `tcp:host:port` between two. Written by the destination, because only it
    /// knows what it managed to bind.
    pub receiver_url: Option<String>,
    /// The receiver is listening *now*. Not "was started" — a receiver whose
    /// process died must stop being ready, or the source sends into nothing.
    pub receiver_ready: bool,
    /// What the source last reported having transferred, for an operator
    /// watching a large guest move. Progress, never a state.
    pub transferred_mib: u64,
}

impl Observed for MigrationStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        self.node.as_deref()
    }
}

pub type Migration = Resource<MigrationSpec, MigrationStatus>;

/// Why a migration cannot be started. Answered before anything moves, because
/// every one of these is knowable in advance — and finding out half way through
/// a transfer is how an operator loses a guest to a preventable mismatch.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("it is already on {node}")]
    AlreadyThere { node: String },
    #[error("it is not running: {state:?}")]
    NotRunning { state: InstanceState },
    #[error("it is not on {claimed}, it is on {actual}")]
    NotFromThere { claimed: String, actual: String },
    #[error("{node} is not accepting work")]
    DestinationDraining { node: String },
    #[error("{node} has {free} free, it needs {want}")]
    DestinationTooSmall {
        node: String,
        free: String,
        want: String,
    },
    /// Both VMMs deserialise device state written by the other side, and both
    /// document a version window. Outside it the transfer fails after the
    /// memory has been copied, which is the most expensive moment to fail.
    #[error(
        "{from_node} runs {from_version} and {to_node} runs {to_version}; migrate through an intermediate version"
    )]
    VersionsTooFarApart {
        from_node: String,
        from_version: String,
        to_node: String,
        to_version: String,
    },
    /// Cloud Hypervisor requires the kernel and initramfs at the *same path* on
    /// both machines, and neither VMM ships the guest's disk. A destination
    /// without the image cannot start the receiver.
    #[error("{node} does not have {image}")]
    DestinationLacksImage { node: String, image: String },
}

/// Whether this guest may be moved there, and if not, the sentence that says
/// why.
///
/// Deliberately a pure function of what has already been reported: capacity from
/// the destination's own report, the image from where it is cached, versions
/// from the two agents. Nothing here asks a node a question, so it can be
/// answered at the moment somebody clicks, not after the transfer starts.
pub fn may_migrate(
    instance: &Instance,
    from: &Node,
    to: &Node,
    image_cached_on: &[String],
) -> Result<(), Refusal> {
    let from_id = from.meta.name.id();
    let to_id = to.meta.name.id();

    if from_id == to_id {
        return Err(Refusal::AlreadyThere {
            node: to_id.to_string(),
        });
    }
    match instance.status.node.as_deref() {
        Some(actual) if actual != from_id => {
            return Err(Refusal::NotFromThere {
                claimed: from_id.to_string(),
                actual: actual.to_string(),
            });
        }
        _ => {}
    }
    // A stopped guest has nothing to transfer; moving it is a spec change, not
    // a migration, and saying so is kinder than starting a receiver for it.
    if instance.spec.desired_state == DesiredState::Running
        && instance.status.state != InstanceState::Running
    {
        return Err(Refusal::NotRunning {
            state: instance.status.state,
        });
    }
    if !to.spec.schedulable {
        return Err(Refusal::DestinationDraining {
            node: to_id.to_string(),
        });
    }

    let free = free_memory_mib(to);
    if free < instance.spec.memory_mib {
        return Err(Refusal::DestinationTooSmall {
            node: to_id.to_string(),
            free: format!("{free} MiB"),
            want: format!("{} MiB", instance.spec.memory_mib),
        });
    }
    if !image_cached_on.iter().any(|n| n == to_id) {
        return Err(Refusal::DestinationLacksImage {
            node: to_id.to_string(),
            image: instance.spec.image.clone(),
        });
    }
    if !versions_compatible(&from.status.agent_version, &to.status.agent_version) {
        return Err(Refusal::VersionsTooFarApart {
            from_node: from_id.to_string(),
            from_version: from.status.agent_version.clone(),
            to_node: to_id.to_string(),
            to_version: to.status.agent_version.clone(),
        });
    }
    Ok(())
}

fn free_memory_mib(node: &Node) -> u64 {
    node.status
        .capacity
        .memory_mib
        .saturating_sub(node.status.allocated.memory_mib)
}

/// One major version apart at most, which is the window both VMMs document for
/// their migration protocol. An unparseable version is treated as compatible:
/// refusing a migration because a version string was not what we expected is a
/// worse failure than letting the VMM refuse it itself, with its own message.
fn versions_compatible(from: &str, to: &str) -> bool {
    let major = |v: &str| -> Option<u32> {
        v.trim_start_matches(|c: char| !c.is_ascii_digit())
            .split(['.', '-', '+'])
            .next()?
            .parse()
            .ok()
    };
    match (major(from), major(to)) {
        (Some(a), Some(b)) => a.abs_diff(b) <= 1,
        _ => true,
    }
}

/// What the **destination** must do. It acts first, always.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestinationAction {
    /// Start an empty VMM and put it in receive mode. Idempotent: a receiver
    /// already listening is success.
    PrepareReceiver {
        instance: String,
        mode: MigrationMode,
    },
    /// The migration is finished or abandoned; nothing should still be
    /// listening.
    TearDownReceiver { instance: String },
}

/// What the **source** must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceAction {
    Send {
        instance: String,
        url: String,
        mode: MigrationMode,
        downtime_ms: u32,
        timeout_s: u32,
        connections: u8,
    },
    /// Stop the transfer and keep the guest. Only ever safe under pre-copy.
    Cancel { instance: String },
}

/// The destination's half.
pub fn reconcile_destination(
    migration: &Migration,
    instance: Option<&Instance>,
    receiver_listening: bool,
) -> Vec<DestinationAction> {
    let arrived = instance
        .map(|i| i.status.node.as_deref() == Some(migration.spec.to_node.as_str()))
        .unwrap_or(false);

    // Finished, or asked to go away: nothing should be left listening. A
    // receiver kept alive holds the guest's memory reservation on a node that
    // is not running it.
    if migration.meta.is_deleting() || arrived {
        return if receiver_listening {
            vec![DestinationAction::TearDownReceiver {
                instance: migration.spec.instance.clone(),
            }]
        } else {
            Vec::new()
        };
    }
    if receiver_listening {
        return Vec::new();
    }
    vec![DestinationAction::PrepareReceiver {
        instance: migration.spec.instance.clone(),
        mode: migration.spec.mode,
    }]
}

/// The source's half.
///
/// `here` is whether this machine still has the guest — read from the machine,
/// not from the store, because that is the one fact the source is the authority
/// on.
pub fn reconcile_source(migration: &Migration, here: bool) -> Vec<SourceAction> {
    if migration.meta.is_deleting() {
        return if here {
            vec![SourceAction::Cancel {
                instance: migration.spec.instance.clone(),
            }]
        } else {
            Vec::new()
        };
    }
    // Gone from here: the transfer succeeded, and there is nothing left to do.
    // Note that this is the *only* thing "done" means on the source side —
    // there is no flag to set.
    if !here {
        return Vec::new();
    }
    let Some(url) = migration.status.receiver_url.as_ref() else {
        return Vec::new();
    };
    if !migration.status.receiver_ready {
        // The destination has published a URL but is not listening yet, or has
        // stopped. Sending now is sending into nothing.
        return Vec::new();
    }
    vec![SourceAction::Send {
        instance: migration.spec.instance.clone(),
        url: url.clone(),
        mode: migration.spec.mode,
        downtime_ms: migration.spec.downtime_ms,
        timeout_s: migration.spec.timeout_s,
        connections: migration.spec.connections,
    }]
}

/// Whether the move has happened — **computed**, like an operation's `done`.
///
/// The instance running on the destination is the whole definition. There is no
/// stored flag that could say otherwise, so a migration cannot report success
/// for a guest that is not there, nor keep saying "in progress" about one that
/// arrived.
pub fn arrived(migration: &Migration, instance: &Instance) -> bool {
    instance.status.node.as_deref() == Some(migration.spec.to_node.as_str())
        && instance.status.state == InstanceState::Running
}

/// What a migration is doing — **computed on read, never stored.**
///
/// This started as a condition a controller wrote, and that was wrong for a
/// reason worth recording: the destination owns this object's status from the
/// moment it claims it, so a controller writing here would be the second writer
/// on one status — the exact thing invariant 1 forbids. The first instinct is a
/// carve-out for conditions. The right answer is that `Moved` was never a fact
/// anybody owns.
///
/// It is a *judgement over the whole dance* — a pure function of the migration
/// and the instance — and this platform already has the precedent: an
/// operation's `done` is computed from its target's convergence and never
/// stored, so it cannot disagree with the object it describes. `Moved` is the
/// same shape, and being computed buys the case a stored condition handles
/// worst: a migration whose destination agent is dead reports the timeout
/// correctly, because nothing has to be running to write it down.
///
/// `age_s` is how long the migration has existed. The caller has the clock; this
/// function stays pure.
///
/// What *is* stored in `status.conditions` is only what the destination can say
/// about itself — that it could not bind a receiver, say. Never this.
pub fn migration_condition(
    migration: &Migration,
    instance: Option<&Instance>,
    age_s: u64,
) -> Condition {
    let at = migration.meta.generation;
    let Some(instance) = instance else {
        return Condition::new(
            "Moved",
            ConditionStatus::False,
            "NoSuchInstance",
            "the instance this migration names does not exist",
            at,
        );
    };
    if arrived(migration, instance) {
        return Condition::new(
            "Moved",
            ConditionStatus::True,
            "Arrived",
            &format!("running on {}", migration.spec.to_node),
            at,
        );
    }
    // Checked after arrival, never before: a guest that landed on second 3601
    // landed. Only a migration that is still not there has run out of time.
    if age_s > u64::from(migration.spec.timeout_s) {
        let where_it_is = instance
            .status
            .node
            .as_deref()
            .unwrap_or("neither node — the handover was interrupted");
        return Condition::new(
            "Moved",
            ConditionStatus::False,
            "Timeout",
            &format!(
                "gave up after {}s; the guest is on {where_it_is}",
                migration.spec.timeout_s
            ),
            at,
        );
    }
    if !migration.status.receiver_ready {
        return Condition::new(
            "Moved",
            ConditionStatus::Unknown,
            "PreparingReceiver",
            &format!("{} is not listening yet", migration.spec.to_node),
            at,
        );
    }
    if instance.status.node.is_none() {
        return Condition::new(
            "Moved",
            ConditionStatus::Unknown,
            "HandingOver",
            "the source has let go and the destination has not claimed it yet",
            at,
        );
    }
    Condition::new(
        "Moved",
        ConditionStatus::Unknown,
        "Transferring",
        &format!(
            "{} MiB copied to {}",
            migration.status.transferred_mib, migration.spec.to_node
        ),
        at,
    )
}

/// Whether a controller should now move the assignment.
///
/// The one spec write in the whole dance, and it happens at exactly one moment:
/// the source has reported that it no longer has the guest. Moving `spec.node`
/// any earlier would tell the destination to claim an instance the source still
/// runs, and moving it later would leave a guest nobody is assigned.
pub fn should_reassign(migration: &Migration, instance: &Instance) -> bool {
    !migration.meta.is_deleting()
        && instance.status.node.is_none()
        && instance.spec.node.as_deref() == Some(migration.spec.from_node.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
        resources::{Capacity as Cap, InstanceSpec, InstanceStatus, NodeSpec, NodeStatus},
    };

    fn instance(state: InstanceState, on: Option<&str>) -> Instance {
        let mut i = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                vcpus: 2,
                memory_mib: 4096,
                image: "projects/p1/images/sha256-abc".into(),
                node: on.map(str::to_string),
                ..Default::default()
            },
            InstanceStatus {
                state,
                node: on.map(str::to_string),
                ..Default::default()
            },
        );
        i.meta.generation = 1;
        i.status.observed_generation = 1;
        i
    }

    fn node(id: &str, mem: u64, version: &str) -> Node {
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
                capacity: Cap {
                    vcpus: 16,
                    memory_mib: mem,
                    disk_gib: 500,
                    numa_free_mib: vec![mem],
                    hugepages_1gi: 0,
                },
                agent_version: version.to_string(),
                ..Default::default()
            },
        );
        set_condition(&mut n.status.conditions, Condition::ready(1));
        n
    }

    fn migration(to: &str) -> Migration {
        let mut m = Resource::new(
            Meta::new(
                ResourceName::parse("projects/p1/migrations/m1").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            MigrationSpec {
                instance: "projects/p1/instances/i1".into(),
                from_node: "node-a".into(),
                to_node: to.into(),
                ..Default::default()
            },
            MigrationStatus::default(),
        );
        m.meta.generation = 1;
        m
    }

    #[test]
    fn a_migration_is_refused_before_it_costs_anything() {
        let i = instance(InstanceState::Running, Some("node-a"));
        let a = node("node-a", 16384, "0.1.0");
        let cached = vec!["node-a".to_string(), "node-b".to_string()];

        assert!(may_migrate(&i, &a, &node("node-b", 16384, "0.1.0"), &cached).is_ok());

        // Every one of these is knowable without touching a hypervisor, and
        // every one of them would otherwise fail after the memory was copied.
        assert_eq!(
            may_migrate(&i, &a, &a, &cached),
            Err(Refusal::AlreadyThere {
                node: "node-a".into()
            })
        );
        assert!(matches!(
            may_migrate(&i, &a, &node("node-b", 1024, "0.1.0"), &cached),
            Err(Refusal::DestinationTooSmall { .. })
        ));
        assert!(matches!(
            may_migrate(
                &i,
                &a,
                &node("node-b", 16384, "0.1.0"),
                &["node-a".to_string()]
            ),
            Err(Refusal::DestinationLacksImage { .. })
        ));
        let mut draining = node("node-b", 16384, "0.1.0");
        draining.spec.schedulable = false;
        assert!(matches!(
            may_migrate(&i, &a, &draining, &cached),
            Err(Refusal::DestinationDraining { .. })
        ));
    }

    #[test]
    fn a_version_gap_is_refused_before_the_memory_is_copied() {
        // Both VMMs document a one-version window for the migration protocol.
        // Outside it the transfer fails at the far end, after everything
        // expensive has already happened.
        let i = instance(InstanceState::Running, Some("node-a"));
        let cached = vec!["node-a".to_string(), "node-b".to_string()];
        assert!(matches!(
            may_migrate(
                &i,
                &node("node-a", 16384, "0.1.0"),
                &node("node-b", 16384, "2.0.0"),
                &cached
            ),
            Err(Refusal::VersionsTooFarApart { .. })
        ));
        // One apart is the documented window and must pass.
        assert!(
            may_migrate(
                &i,
                &node("node-a", 16384, "1.0.0"),
                &node("node-b", 16384, "2.0.0"),
                &cached
            )
            .is_ok()
        );
        // An unreadable version is the VMM's business to refuse, with its own
        // message. Guessing here would refuse a migration that would work.
        assert!(
            may_migrate(
                &i,
                &node("node-a", 16384, "dev"),
                &node("node-b", 16384, "?"),
                &cached
            )
            .is_ok()
        );
    }

    #[test]
    fn a_guest_that_is_not_running_is_not_migrated() {
        let stopped = instance(InstanceState::Stopped, Some("node-a"));
        assert!(matches!(
            may_migrate(
                &stopped,
                &node("node-a", 16384, "0.1.0"),
                &node("node-b", 16384, "0.1.0"),
                &["node-b".to_string()]
            ),
            Err(Refusal::NotRunning { .. })
        ));
    }

    #[test]
    fn the_destination_listens_before_the_source_sends() {
        // This ordering is not ours: both VMMs require the receiver to be up
        // first. The model has to make it impossible to get wrong.
        let m = migration("node-b");
        assert_eq!(
            reconcile_destination(
                &m,
                Some(&instance(InstanceState::Running, Some("node-a"))),
                false
            ),
            vec![DestinationAction::PrepareReceiver {
                instance: "projects/p1/instances/i1".into(),
                mode: MigrationMode::Live
            }]
        );
        // …and the source does nothing at all until it is told the far end is
        // listening, not merely that a URL exists.
        assert!(reconcile_source(&m, true).is_empty());

        let mut with_url = m.clone();
        with_url.status.receiver_url = Some("tcp:10.0.0.2:9000".into());
        assert!(
            reconcile_source(&with_url, true).is_empty(),
            "the source sent to a receiver that had not confirmed it was listening"
        );

        let mut ready = with_url.clone();
        ready.status.receiver_ready = true;
        assert_eq!(
            reconcile_source(&ready, true),
            vec![SourceAction::Send {
                instance: "projects/p1/instances/i1".into(),
                url: "tcp:10.0.0.2:9000".into(),
                mode: MigrationMode::Live,
                downtime_ms: 300,
                timeout_s: 3600,
                connections: 1,
            }]
        );
    }

    #[test]
    fn the_source_stops_when_the_guest_is_gone_and_nothing_marks_it_done() {
        let mut m = migration("node-b");
        m.status.receiver_url = Some("tcp:10.0.0.2:9000".into());
        m.status.receiver_ready = true;
        // Not here any more: the transfer worked. There is no flag to set and
        // nothing to remember — the absence of the guest *is* the report.
        assert!(reconcile_source(&m, false).is_empty());
    }

    #[test]
    fn a_receiver_is_torn_down_once_the_guest_has_arrived() {
        let m = migration("node-b");
        let arrived_instance = instance(InstanceState::Running, Some("node-b"));
        assert_eq!(
            reconcile_destination(&m, Some(&arrived_instance), true),
            vec![DestinationAction::TearDownReceiver {
                instance: "projects/p1/instances/i1".into()
            }]
        );
        // And once it is gone, the pass is empty — reconciling a finished
        // migration must cost nothing, or every resync churns.
        assert!(reconcile_destination(&m, Some(&arrived_instance), false).is_empty());
    }

    #[test]
    fn abandoning_a_migration_keeps_the_guest_where_it_is() {
        // Pre-copy's one great property: until the last pages are sent, the
        // source still has a running guest. Cancelling must therefore be safe
        // and must not touch the instance at all.
        let mut m = migration("node-b");
        m.meta.deleted_at = Some(Timestamp::now());
        assert_eq!(
            reconcile_source(&m, true),
            vec![SourceAction::Cancel {
                instance: "projects/p1/instances/i1".into()
            }]
        );
        assert_eq!(
            reconcile_destination(
                &m,
                Some(&instance(InstanceState::Running, Some("node-a"))),
                true
            ),
            vec![DestinationAction::TearDownReceiver {
                instance: "projects/p1/instances/i1".into()
            }]
        );
    }

    #[test]
    fn the_assignment_moves_at_exactly_one_moment() {
        let m = migration("node-b");
        let mut i = instance(InstanceState::Running, Some("node-a"));

        // While the source still has it: no.
        assert!(
            !should_reassign(&m, &i),
            "the destination was told to claim a running guest"
        );

        // The source has let go — and only now.
        i.status.node = None;
        assert!(should_reassign(&m, &i));

        // Already moved: nothing more to do, so a resync writes nothing.
        i.spec.node = Some("node-b".into());
        assert!(!should_reassign(&m, &i));
    }

    #[test]
    fn what_it_says_while_it_works_is_always_an_observation() {
        let mut m = migration("node-b");
        let i = instance(InstanceState::Running, Some("node-a"));

        assert_eq!(
            migration_condition(&m, Some(&i), 0).reason,
            "PreparingReceiver"
        );

        m.status.receiver_ready = true;
        m.status.transferred_mib = 2048;
        let c = migration_condition(&m, Some(&i), 5);
        assert_eq!(c.reason, "Transferring");
        assert!(c.message.contains("2048 MiB"), "{}", c.message);

        let mut released = i.clone();
        released.status.node = None;
        assert_eq!(
            migration_condition(&m, Some(&released), 5).reason,
            "HandingOver"
        );

        let arrived_instance = instance(InstanceState::Running, Some("node-b"));
        let done = migration_condition(&m, Some(&arrived_instance), 5);
        assert_eq!(done.status, ConditionStatus::True);
        assert!(arrived(&m, &arrived_instance));
    }

    #[test]
    fn a_migration_that_ran_out_of_time_says_where_the_guest_is() {
        // The point of computing this rather than writing it: nothing has to be
        // alive to report it. A destination agent that died mid-transfer cannot
        // write a timeout into a status it owns — and this is exactly the case
        // an operator most needs answered.
        let mut m = migration("node-b");
        m.status.receiver_ready = true;
        let i = instance(InstanceState::Running, Some("node-a"));

        let c = migration_condition(&m, Some(&i), u64::from(m.spec.timeout_s) + 1);
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "Timeout");
        assert!(c.message.contains("node-a"), "{}", c.message);

        // Arrival beats the clock: landing on the last second is landing.
        let there = instance(InstanceState::Running, Some("node-b"));
        assert_eq!(
            migration_condition(&m, Some(&there), u64::from(m.spec.timeout_s) + 1).reason,
            "Arrived"
        );

        // Timed out mid-handover, when neither node holds it. Saying "the guest
        // is on None" would be worse than saying plainly that it is in between.
        let mut nowhere = i.clone();
        nowhere.status.node = None;
        let c = migration_condition(&m, Some(&nowhere), u64::from(m.spec.timeout_s) + 1);
        assert!(c.message.contains("interrupted"), "{}", c.message);
    }
}
