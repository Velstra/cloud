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
    /// Pull the plug: the guest was asked to shut down and did not.
    ///
    /// Separate from [`Action::StopVm`] rather than a flag on it, because they
    /// are different acts. One asks an operating system to close its files; the
    /// other takes the power away and may lose whatever was not written. A
    /// caller reading a list of actions should be able to see which of the two
    /// is about to happen.
    KillVm {
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
/// How long a guest is given to answer the ACPI button before the plug comes
/// out.
///
/// Sixty seconds, the same figure Proxmox and libvirt settle on: long enough
/// for an ordinary Linux to stop its services and unmount, short enough that
/// somebody watching does not conclude the platform is stuck. A guest that
/// needs longer is a guest whose operator should stop it from the inside.
pub const STOP_GRACE_MS: u64 = 60_000;

pub fn reconcile_instance(
    instance: &Instance,
    image_cached: bool,
    ports_programmed: &[bool],
    disk_present: bool,
    // Whether anything ahead of this guest in the node's start order is still
    // coming up. `Go` for everything a caller has no ordering opinion about,
    // which is most callers and every cell that has not asked for one.
    gate: StartGate,
    // Time enters as a parameter, as it does everywhere else here, so this
    // stays pure and a test can argue about a stop that has been waiting for
    // an hour without waiting for one.
    now: crate::meta::Timestamp,
) -> Vec<Action> {
    let name = instance.meta.name.to_string();
    let mut actions = Vec::new();

    // A deleted object is torn down in the reverse order it was built, and the
    // finalizer goes last so nothing can observe a half-removed instance.
    if instance.meta.is_deleting() {
        // **Always**, including from `Unknown`. That state means "nobody has
        // reported", which is what a guest looks like after its node agent
        // restarts — the process is still running, the agent has not yet
        // recognised it, and skipping the delete here left the tap held by a
        // VMM nobody was going to stop. `ip tuntap del` then failed with
        // `Device or resource busy`, that failure ended the pass before the
        // finalizer, and the object could never be removed by any means.
        //
        // Reading `Unknown` as "there is nothing to delete" is the error: it
        // means *we do not know*, which is exactly when you have to look.
        // `DeleteVm` is idempotent — it asks the monitor socket, stops the unit
        // and removes the directory, each of which is a no-op when there is
        // nothing there.
        // Until the node has said it let go. Two things turn on this, and both
        // were wrong before:
        //
        // The teardown runs **from `Unknown` too**. That state means "nobody has
        // reported", which is what every guest looks like after its node agent
        // restarts — the VMM is still running and the agent has not recognised
        // it. Skipping the delete there left the tap held by a process nobody
        // was going to stop, `ip tuntap del` failed with `Device or resource
        // busy`, and that failure ended the pass before the finalizer: the
        // object could not be removed by any means the API offers. Reading
        // `Unknown` as "there is nothing to delete" is the error — it means *we
        // do not know*, which is exactly when you have to look.
        //
        // And it stops once the node says `Released`. Without that the port
        // unprogram recurred on every resync of a torn-down object for as long
        // as it sat there — the one place in this function where a settled
        // object still asked for work, which `tests/agent.rs` had written down
        // as worth fixing here.
        let let_go = crate::meta::condition(&instance.status.conditions, "Released")
            .is_some_and(|c| c.status == crate::ConditionStatus::True);
        if !let_go {
            actions.push(Action::DeleteVm {
                instance: name.clone(),
            });
            for port in &instance.spec.ports {
                actions.push(Action::UnprogramPort { port: port.clone() });
            }
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
        // Held by the start order, not by anything about this guest. No
        // action, no state, nothing recorded: the next pass asks the same
        // question of a world that has moved on, and by then the group ahead
        // is up. Why it is waiting is answerable — see `StartGate` — rather
        // than written onto the object, because the node owns that status.
        (DesiredState::Running, _) if ready_to_run && gate != StartGate::Go => {}
        (DesiredState::Running, _) if ready_to_run => {
            actions.push(Action::StartVm {
                instance: name.clone(),
            });
        }
        (DesiredState::Running, _) => {}
        (DesiredState::Stopped, InstanceState::Running) => {
            // Ask once, then insist.
            //
            // `StopVm` presses the ACPI button. A guest that answers it shuts
            // down cleanly, which is what everybody wants and what happens
            // almost always. A guest that cannot answer — stuck in its
            // bootloader, no ACPI daemon, a kernel that has panicked — never
            // will, and pressing the button again every pass is a platform
            // politely asking a corpse to leave.
            //
            // So after `STOP_GRACE` the plug comes out. Nothing else in this
            // function needs a clock; this does, because "how long has it been
            // asked" is the only thing that separates a slow shutdown from one
            // that is never coming.
            match instance.status.stop_requested_at {
                Some(asked) if now.0.saturating_sub(asked.0) >= STOP_GRACE_MS => {
                    actions.push(Action::KillVm {
                        instance: name.clone(),
                    })
                }
                Some(_) => {}
                None => actions.push(Action::StopVm {
                    instance: name.clone(),
                }),
            }
        }
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

/// One guest on this node, as the start gate sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartPeer {
    pub name: String,
    pub order: u32,
    pub state: InstanceState,
    /// What its operator wants it doing. A guest nobody wants running is not
    /// something to wait for.
    pub desired: DesiredState,
    pub started_at: Option<Timestamp>,
}

/// Whether a guest may start yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartGate {
    /// Nothing is in the way.
    Go,
    /// Something earlier in the order has not come up.
    ///
    /// Carries which one, because "waiting" without a name is a thing an
    /// operator stares at. If a whole node stays down for one broken guest,
    /// the name of that guest is the entire fix.
    WaitingFor { instance: String },
    /// Everything earlier is up, and its settling time has not elapsed.
    WaitingOut { seconds: u64 },
}

/// Whether this guest may start yet, given what else is on its node.
///
/// The problem this solves is the one a platform is judged on: power comes
/// back, a node starts forty guests at once, and the database that everything
/// else needs loses the race for disk to a dozen web servers.
///
/// Level-triggered like everything else. There is no queue and no "starting"
/// state — every pass asks the same question of the same world, and a guest
/// that may not start yet is simply not started this pass.
///
/// **What counts as settled**, and this is where the deadlock lives:
///
/// * `Running` — up, obviously.
/// * `Failed` — it had its chance. Waiting forever for a guest that cannot
///   start would take a whole node down for one broken disk.
/// * Anything whose operator wants it `Stopped` — it is not coming up, and
///   waiting for a machine nobody asked to run is waiting for nothing.
///
/// Everything else blocks, which is the case somebody actually wants to wait
/// for: a guest that is genuinely still coming up. A guest stuck trying — no
/// image, no capacity — does hold the ones behind it, and that is deliberate:
/// the alternative is starting the application servers without the database
/// and calling it success. It is *visible*, which is the difference — the
/// refusal names what is being waited on.
pub fn start_gate(order: u32, delay_s: u32, peers: &[StartPeer], now: Timestamp) -> StartGate {
    let earlier: Vec<&StartPeer> = peers.iter().filter(|p| p.order < order).collect();

    for peer in &earlier {
        let settled = peer.state == InstanceState::Running
            || peer.state == InstanceState::Failed
            || peer.desired == DesiredState::Stopped;
        if !settled {
            return StartGate::WaitingFor {
                instance: peer.name.clone(),
            };
        }
    }

    if delay_s == 0 {
        return StartGate::Go;
    }
    // The newest start among everything earlier. A guest waits for the group
    // in front of it to have been up a while, not for each member in turn —
    // which would make a fleet's boot the sum of every delay rather than the
    // longest one.
    let newest = earlier
        .iter()
        .filter(|p| p.state == InstanceState::Running)
        .filter_map(|p| p.started_at)
        .map(|t| t.0)
        .max();
    let Some(newest) = newest else {
        // Nothing earlier is running: either there is nothing earlier, or what
        // is there is failed or deliberately stopped. Either way there is
        // nothing to settle after.
        return StartGate::Go;
    };
    let waited_ms = now.0.saturating_sub(newest);
    let need_ms = u64::from(delay_s) * 1000;
    if waited_ms >= need_ms {
        StartGate::Go
    } else {
        StartGate::WaitingOut {
            seconds: (need_ms - waited_ms).div_ceil(1000),
        }
    }
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
    /// The node's processor is below the level this instance asked for.
    CpuLevelTooLow {
        has: Option<crate::cpu::CpuLevel>,
        want: crate::cpu::CpuLevel,
    },
    /// The node cannot give this guest the PCI devices it asked for.
    ///
    /// Carries the shortfall rather than a count, because the two questions an
    /// operator asks next are different: "there are none here" sends them to
    /// another node, and "the audio function beside it is bound to
    /// snd_hda_intel" is fixed on this one in a minute.
    NoDevice {
        why: crate::pci::Shortfall,
    },
    /// This instance is already running somewhere, and this node cannot give
    /// it the CPU it is running with. Carries the mismatch rather than a
    /// flattened string so the console can tell "three flags short" from
    /// "this VMM can never mask" — two facts with two different remedies.
    CpuIncompatible {
        why: crate::cpu::CpuMismatch,
    },
    /// The rest of this guest's group is somewhere else, and it asked to be
    /// with them.
    ///
    /// Carries where they are, so the sentence an operator reads is "the rest
    /// of it is on node-b" rather than "no valid host" — one of those names
    /// the machine to go and look at.
    NotWithGroup {
        group: String,
        elsewhere: Vec<String>,
    },
    /// The node is out of service in a window somebody declared.
    ///
    /// Carries when it comes back and what it is for, because "no capacity" and
    /// "node-b is back at 03:00 for the memory swap" are the same fact told two
    /// ways, and only one of them ends the conversation.
    InMaintenance {
        minutes_left: u64,
        note: String,
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
    // Which anti-affinity group already occupies which node.
    occupied_groups: &[(String, String)],
    // Which affinity group is already on which node — the same shape, and a
    // separate list because a name may mean both things to two different
    // guests, and folding them together would let one guest's "keep apart"
    // answer another's "keep together".
    with_group: &[(String, String)],
    // The cell's device classes, needed to answer what an instance asked for.
    // Empty when nothing in the cell passes hardware through, which is most
    // cells — an instance asking for a class then finds none, by name.
    classes: &std::collections::BTreeMap<String, crate::pci::DeviceClassSpec>,
    // The nodes that are out of service this instant, from
    // [`crate::maintenance::closed_now`]. Passed in rather than read here
    // because this function is pure and the clock is not: a scheduler that
    // computed a different answer on a second call with the same arguments
    // would be untestable and unexplainable.
    closed: &[crate::maintenance::Closed],
) -> Result<String, Vec<Explanation>> {
    let mut rejected = Vec::new();
    let mut best: Option<(&Node, (u8, u8, u64))> = None;

    for node in nodes {
        let id = node.meta.name.id().to_string();
        if !node.spec.schedulable {
            rejected.push(Explanation {
                node: id,
                why: Rejected::Unschedulable,
            });
            continue;
        }
        if let Some(shut) = closed.iter().find(|c| c.node == id) {
            rejected.push(Explanation {
                node: id,
                why: Rejected::InMaintenance {
                    minutes_left: shut.minutes_left,
                    note: shut.note.clone(),
                },
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
        // The CPU, before capacity: a node that cannot run this guest at all
        // is not a near miss to be reported as "2 vcpus short".
        let node_cpu = node.status.cpu.clone().unwrap_or_default();
        if let Some(want) = instance.spec.placement_policy.min_cpu_level {
            let has = node_cpu.level();
            if has.is_none_or(|has| has < want) {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::CpuLevelTooLow { has, want },
                });
                continue;
            }
        }
        // An instance that has run before carries the CPU it was given. The
        // question is never "what could this node present" but "can it present
        // what this guest already sees" — see the invariant on `crate::cpu`.
        if let Some(guest_cpu) = &instance.status.cpu {
            if let Err(why) = crate::cpu::may_run_on(guest_cpu, &node_cpu) {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::CpuIncompatible { why },
                });
                continue;
            }
        }
        // The devices, before capacity: a node without the hardware is not a
        // near miss to be reported as "2 vcpus short", and the reason it
        // cannot take the guest is a hardware fact the operator can act on.
        if !instance.spec.devices.is_empty() {
            if let Err(why) =
                crate::pci::assign(&instance.spec.devices, classes, &node.status.pci_devices)
            {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::NoDevice { why },
                });
                continue;
            }
        }
        // Anti-affinity and affinity, both of which can be a rule or a wish.
        //
        // A wish does not reject here; it is carried to the score below, where
        // "beside its sibling" loses to "anywhere else" and still beats "not
        // running at all".
        let policy = &instance.spec.placement_policy;
        let mut crowded = false;
        if let Some(group) = &policy.anti_affinity_group
            && occupied_groups.iter().any(|(g, n)| g == group && n == &id)
        {
            if policy.spread == crate::resources::Strength::Required {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::AntiAffinity {
                        group: group.clone(),
                    },
                });
                continue;
            }
            crowded = true;
        }
        // Affinity only bites once a member is placed somewhere. Before that
        // there is nothing to be near, and refusing every node would mean a
        // group whose first member could never start.
        let mut together = false;
        if let Some(group) = &policy.affinity_group {
            let anywhere = with_group.iter().any(|(g, _)| g == group);
            together = with_group.iter().any(|(g, n)| g == group && n == &id);
            if anywhere && !together && policy.affinity == crate::resources::Strength::Required {
                rejected.push(Explanation {
                    node: id,
                    why: Rejected::NotWithGroup {
                        group: group.clone(),
                        // Where the group actually is, so "no valid host"
                        // becomes "node-b is where the rest of it is, and
                        // node-b has 2 GiB free".
                        elsewhere: with_group
                            .iter()
                            .filter(|(g, _)| g == group)
                            .map(|(_, n)| n.clone())
                            .collect(),
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

        // Wishes first, then least-loaded — so one node does not collect every
        // small instance while its neighbours idle.
        //
        // Lexicographic rather than weighted, deliberately. A weight is a
        // number somebody has to tune, and the first time it is wrong it is
        // wrong silently; an order is a sentence: be with your group, then be
        // away from your siblings, then be on the emptiest machine.
        let score = (u8::from(together), u8::from(!crowded), free.memory_mib);
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((node, score));
        }
    }

    match best {
        Some((node, _)) => Ok(node.meta.name.id().to_string()),
        None => Err(rejected),
    }
}

/// How many vCPUs this node is willing to hand out in total.
///
/// The ratio is a *spec* field, so it is an operator's decision, and it is
/// applied here rather than to the reported capacity: a node reports what it
/// has, and what a cell is prepared to promise on top of that is not something
/// an agent gets to say about itself.
pub fn offered_vcpus(node: &Node) -> u32 {
    let real = node.status.capacity.vcpus;
    match node.spec.vcpu_overcommit {
        // Zero is "nobody set one", the same reading a quota gives it — so a
        // node stored before the field existed behaves exactly as it did.
        0 | 1 => real,
        ratio => real.saturating_mul(ratio),
    }
}

fn free_capacity(node: &Node) -> Capacity {
    let c = &node.status.capacity;
    let a = &node.status.allocated;
    Capacity {
        vcpus: offered_vcpus(node).saturating_sub(a.vcpus),
        memory_mib: c.memory_mib.saturating_sub(a.memory_mib),
        disk_gib: c.disk_gib.saturating_sub(a.disk_gib),
        numa_free_mib: c.numa_free_mib.clone(),
        hugepages_1gi: c.hugepages_1gi.saturating_sub(a.hugepages_1gi),
    }
}

/// What a cell has, what is spoken for, and what is actually left.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headroom {
    /// Every node that is ready and accepting work.
    pub usable_nodes: usize,
    /// Nodes that exist and cannot take anything: draining, not ready, or
    /// being emptied. Counted separately because "we have twelve nodes" and
    /// "eight of them will take a guest" are different sentences, and the
    /// second one is the one somebody planning capacity needs.
    pub unusable_nodes: usize,
    pub total: Capacity,
    pub allocated: Capacity,
    /// What is free across the usable nodes — and *not* `total - allocated`,
    /// which would count a draining node's empty half as room.
    pub free: Capacity,
    /// How many vCPUs the usable nodes are prepared to hand out.
    ///
    /// Distinct from `total.vcpus`, which is silicon. They differ exactly where
    /// an operator has set a ratio, and keeping them apart is what stops a
    /// capacity page from reading as though the cell had grown a processor:
    /// "64 cores, offered as 256" is the true sentence and neither number tells
    /// it alone.
    pub offered_vcpus: u32,
    /// The largest guest that would still fit somewhere.
    ///
    /// The number an operator actually wants and cannot get from a sum: a cell
    /// with 64 GiB free spread over eight nodes fits no 16 GiB guest at all,
    /// and every dashboard that shows the sum has told somebody it would.
    pub largest_fit: Capacity,
}

/// Add up a cell, in the one way that does not lie.
///
/// Sums are the obvious approach and they mislead in two specific ways, both
/// of which this avoids:
///
/// * A drained node's free memory is not free — nothing may be placed there.
///   It is counted in `total` and excluded from `free`, so the two disagree on
///   purpose and the gap is the drained capacity.
/// * Free memory does not add up into a guest. Sixty-four gibibytes spread
///   over eight nodes fits no sixteen-gibibyte guest, and `largest_fit` is the
///   only honest answer to "will another one go in".
pub fn headroom(nodes: &[Node], closed: &[crate::maintenance::Closed]) -> Headroom {
    let mut out = Headroom::default();
    for node in nodes {
        if node.meta.is_deleting() {
            continue;
        }
        let c = &node.status.capacity;
        out.total.vcpus = out.total.vcpus.saturating_add(c.vcpus);
        out.total.memory_mib = out.total.memory_mib.saturating_add(c.memory_mib);
        out.total.disk_gib = out.total.disk_gib.saturating_add(c.disk_gib);

        let a = &node.status.allocated;
        out.allocated.vcpus = out.allocated.vcpus.saturating_add(a.vcpus);
        out.allocated.memory_mib = out.allocated.memory_mib.saturating_add(a.memory_mib);
        out.allocated.disk_gib = out.allocated.disk_gib.saturating_add(a.disk_gib);

        let ready = crate::meta::condition(&node.status.conditions, "Ready")
            .is_some_and(|c| c.status == ConditionStatus::True);
        // A machine inside an open maintenance window is unusable in exactly
        // the way a draining one is. Counting it would put its free memory into
        // `largestFit`, and `largestFit` is the number a tenant is told they
        // can start — a promise the scheduler would then refuse.
        let out_of_service = closed.iter().any(|c| c.node == node.meta.name.id());
        if !ready || !node.spec.schedulable || node.spec.evacuate || out_of_service {
            out.unusable_nodes += 1;
            continue;
        }
        out.usable_nodes += 1;
        out.offered_vcpus = out.offered_vcpus.saturating_add(offered_vcpus(node));

        let free = free_capacity(node);
        out.free.vcpus = out.free.vcpus.saturating_add(free.vcpus);
        out.free.memory_mib = out.free.memory_mib.saturating_add(free.memory_mib);
        out.free.disk_gib = out.free.disk_gib.saturating_add(free.disk_gib);

        // The largest single machine, per dimension. Deliberately not one
        // node's whole shape: a guest needs vcpus *and* memory on one host, so
        // the honest reading of this field is "no guest larger than this in
        // any dimension can fit anywhere", which is what a person is checking.
        out.largest_fit.vcpus = out.largest_fit.vcpus.max(free.vcpus);
        out.largest_fit.memory_mib = out.largest_fit.memory_mib.max(free.memory_mib);
        out.largest_fit.disk_gib = out.largest_fit.disk_gib.max(free.disk_gib);
    }
    out
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
        Rejected::CpuLevelTooLow { has, want } => match has {
            Some(has) => format!("cpu is {has}, {want} wanted"),
            // Distinct from a low level: an agent too old to report its CPU
            // cannot be shown to satisfy anything, and saying "cpu is v1"
            // would be a claim nobody made.
            None => format!("cpu unknown, {want} wanted"),
        },
        Rejected::CpuIncompatible { why } => why.to_string(),
        Rejected::NotWithGroup { group, elsewhere } => format!(
            "{group} is on {}, and this guest asked to be with it",
            elsewhere.join(", ")
        ),
        Rejected::NoDevice { why } => why.to_string(),
        Rejected::InMaintenance { minutes_left, note } if note.is_empty() => {
            format!("out of service for another {minutes_left} minutes")
        }
        Rejected::InMaintenance { minutes_left, note } => {
            format!("out of service for another {minutes_left} minutes: {note}")
        }
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
    floating_ips: &[crate::resources::FloatingIp],
    load_balancers: &[crate::loadbalancer::LoadBalancer],
) -> Quota {
    let mut used = Quota::default();
    for instance in instances.iter().filter(|i| i.meta.name.is_under(project)) {
        used.instances = used.instances.saturating_add(1);
        used.vcpus = used.vcpus.saturating_add(instance.spec.vcpus);
        used.memory_mib = used.memory_mib.saturating_add(instance.spec.memory_mib);
        used.volume_gib = used.volume_gib.saturating_add(instance.spec.root_disk_gib);
        // What the instance *asked for*, not what it was given. A guest that
        // is stopped still holds its claim on the fleet's accelerators as far
        // as a limit is concerned — otherwise a project could sit on every
        // GPU in the cell by keeping its guests stopped.
        used.devices = used
            .devices
            .saturating_add(instance.spec.devices.len() as u32);
    }
    for volume in volumes.iter().filter(|v| v.meta.name.is_under(project)) {
        used.volumes = used.volumes.saturating_add(1);
        used.volume_gib = used.volume_gib.saturating_add(volume.spec.size_gib);
    }
    used.floating_ips = floating_ips
        .iter()
        .filter(|f| f.meta.name.is_under(project))
        .count() as u32;
    used.load_balancers = load_balancers
        .iter()
        .filter(|l| l.meta.name.is_under(project))
        .count() as u32;
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
        (used.devices > limit.devices && limit.devices > 0).then_some("devices"),
        (used.vcpus > limit.vcpus && limit.vcpus > 0).then_some("vcpus"),
        (used.memory_mib > limit.memory_mib && limit.memory_mib > 0).then_some("memory"),
        (used.volumes > limit.volumes && limit.volumes > 0).then_some("volumes"),
        (used.volume_gib > limit.volume_gib && limit.volume_gib > 0).then_some("storage"),
        (used.floating_ips > limit.floating_ips && limit.floating_ips > 0)
            .then_some("floating IPs"),
        (used.load_balancers > limit.load_balancers && limit.load_balancers > 0)
            .then_some("load balancers"),
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
    /// The object is there and **nothing is ever going to report on it** — the
    /// per-object twin of [`nobody_reports_on`], for a kind where the answer
    /// depends on the object. A port assigned to no node is the case: no agent
    /// has it, so waiting for one is waiting for nobody.
    Unwatched,
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

/// The collection a resource name belongs to.
///
/// The last path segment before the id — `projects/p1/subnets/lan0` is a
/// `subnets`. Empty for anything that is not a resource name, which the caller
/// treats as "something reports on it", the safe direction.
pub fn kind_of(name: &str) -> &str {
    let mut parts = name.rsplit('/');
    let _id = parts.next();
    parts.next().unwrap_or("")
}

/// Whether anything in this platform ever writes a status on objects of this
/// kind.
///
/// **The one list, read by everybody who needs it.** An object nobody reports
/// on stays at `observedGeneration: 0` with no conditions for its whole life,
/// and every reader that treats that as "waiting" is wrong about it: the
/// console's attention list filled with them, and an operation created for one
/// never finished.
///
/// Two kinds of thing are on it. **Records** — an audit line, a usage reading —
/// are facts about something that already happened. **Declarations** — an
/// account, a hardware class, a place to put bytes, a statement about time, a
/// range on a network — are asks that need no work: what acts on them are the
/// objects that reference them, and those report for themselves.
///
/// Anything not named here is assumed to have a reporter, which is the safe
/// direction: a new collection reads as "waiting" until somebody says
/// otherwise, rather than silently reading as finished the moment it exists.
pub fn nobody_reports_on(kind: &str) -> bool {
    matches!(
        kind,
        "audit"
            | "usage"
            | "users"
            | "device-classes"
            | "backup-targets"
            | "maintenance-windows"
            | "subnets"
            | "images"
            | "snapshot-schedules"
            // A view of the images, grouped by family and computed on the way
            // out. There is no object here for anything to report on.
            | "families"
            // A place in a tree. Nothing runs one.
            | "folders"
    )
}

/// Whether objects of this kind are a record of something that already
/// happened, rather than something somebody holds.
///
/// The distinction matters at exactly one moment: deleting the thing they are
/// about. A machine in a project is a reason not to delete the project — the
/// person is being told there is still something running. An hourly usage
/// reading is not: nobody is waiting on it, it cannot be deleted through the
/// API by design (a number somebody can edit after the fact is not a bill), and
/// counting it made **every project older than an hour permanently
/// undeletable**. Found on a real cell, where thirty-five projects could not be
/// removed by any means the API offers.
///
/// Records outlive what they are about and go away with their own retention.
/// That is already how `operations` behaved, for the same reason, written down
/// once here instead of as an exception per caller — the next record kind
/// should inherit the answer rather than repeat the bug.
pub fn is_a_record(kind: &str) -> bool {
    matches!(kind, "audit" | "usage" | "operations")
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
    // Some things nobody reports on. A subnet, an image, a security group, a
    // schedule: no agent owns one, so `observedGeneration` stays at zero for
    // ever and there is never a condition to read. Waiting for one is waiting
    // for something that will not happen — and an operation is exactly what a
    // client is told to poll after a create.
    //
    // Measured on a real cell: seven operations from ordinary creates, every
    // one of them `done: false` for ever. A client following the documented
    // pattern hangs on a subnet that was made perfectly well.
    //
    // So for those, existing **is** finished. The list is the model's and the
    // console reads the same one, so the two cannot drift.
    if let TargetView::Present { .. } = target {
        if nobody_reports_on(kind_of(&spec.target)) {
            return OperationProgress {
                done: true,
                error: None,
            };
        }
    }
    match target {
        // Present, and nobody is coming. Existing is the whole of it — the same
        // answer `nobody_reports_on` gives for a whole kind.
        TargetView::Unwatched => OperationProgress {
            done: true,
            error: None,
        },
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

    pub(super) fn inst(name: &str) -> Instance {
        Resource::new(
            Meta::new(
                ResourceName::parse(name).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            InstanceSpec {
                start_order: 0,
                start_delay_s: 0,
                on_node_loss: Default::default(),
                console: false,
                devices: Vec::new(),
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
                evacuate: false,
                vcpu_overcommit: 0,
                fence_after_s: 0,
                schedulable: true,
                labels: vec![],
                cpu_baseline: None,
                gateway: false,
            },
            NodeStatus {
                shared_state: false,
                vmm: "qemu".into(),
            fetching: Vec::new(),
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
        let actions = reconcile_instance(
            &i,
            false,
            &[false],
            false,
            StartGate::Go,
            crate::meta::Timestamp::now(),
        );
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
        let actions = reconcile_instance(
            &i,
            true,
            &[true],
            true,
            StartGate::Go,
            crate::meta::Timestamp::now(),
        );
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
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Running;
        assert!(
            reconcile_instance(
                &i,
                true,
                &[true],
                true,
                StartGate::Go,
                crate::meta::Timestamp::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_crashed_guest_is_restarted_and_says_so() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Failed;
        let actions = reconcile_instance(
            &i,
            true,
            &[true],
            true,
            StartGate::Go,
            crate::meta::Timestamp::now(),
        );
        assert_eq!(
            actions,
            vec![Action::RestartCrashedVm {
                instance: "projects/p1/instances/i1".into()
            }]
        );
    }

    #[test]
    fn deleting_tears_down_before_it_lets_go() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.status.state = InstanceState::Running;
        i.meta.deleted_at = Some(crate::meta::Timestamp::now());
        i.meta.add_finalizer(NODE_RELEASE_FINALIZER);
        let actions = reconcile_instance(
            &i,
            true,
            &[true],
            true,
            StartGate::Go,
            crate::meta::Timestamp::now(),
        );
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
                at: String::new(),
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

    /// A node without the hardware is rejected by name, and the busy one
    /// blames the *neighbour* rather than the device that was asked for.
    ///
    /// The whole point of the IOMMU-group rule, seen from the outside: an
    /// operator asking why their guest will not start is told which device to
    /// unbind, on which node, rather than that the class is unavailable.
    #[test]
    fn a_node_without_the_device_is_rejected_with_the_hardware_reason() {
        use crate::pci::{DeviceClassSpec, DeviceKind, DeviceUse, PciDevice};

        let gpu = |address: &str, group: u32| PciDevice {
            address: address.into(),
            vendor_device: "10de:2204".into(),
            kind: DeviceKind::Gpu,
            iommu_group: Some(group),
            state: DeviceUse::Free,
            ..PciDevice::default()
        };
        let classes = std::collections::BTreeMap::from([(
            "gpu-a100".to_string(),
            DeviceClassSpec {
                matches: vec!["10de:2204".into()],
                ..DeviceClassSpec::default()
            },
        )]);

        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.spec.devices = vec!["gpu-a100".into()];

        let bare = node("bare", 8, 16384);
        let mut has = node("has", 8, 16384);
        has.status.pci_devices = vec![gpu("0000:41:00.0", 17)];
        let mut busy = node("busy", 8, 16384);
        busy.status.pci_devices = vec![
            gpu("0000:41:00.0", 17),
            PciDevice {
                address: "0000:41:00.1".into(),
                vendor_device: "10de:1aef".into(),
                kind: DeviceKind::Audio,
                iommu_group: Some(17),
                state: DeviceUse::HostDriver {
                    driver: "snd_hda_intel".into(),
                },
                ..PciDevice::default()
            },
        ];

        // The one that can give it a GPU is chosen.
        assert_eq!(
            place(
                &i,
                &[bare.clone(), has, busy.clone()],
                &[],
                &[],
                &classes,
                &[]
            )
            .unwrap(),
            "has"
        );

        // With only the two that cannot, both rejections say why.
        let why = place(&i, &[bare, busy], &[], &[], &classes, &[]).unwrap_err();
        let all: String = why
            .iter()
            .map(|e| format!("{}: {}\n", e.node, describe(&e.why)))
            .collect();
        assert!(all.contains("bare: no gpu-a100 here"), "{all}");
        assert!(
            all.contains("snd_hda_intel") && all.contains("0000:41:00.1"),
            "the busy node blamed the GPU instead of its group-mate:\n{all}"
        );
    }

    /// An instance asking for a class nobody defined is refused by name.
    #[test]
    fn an_instance_asking_for_an_undefined_class_is_told_so() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.spec.devices = vec!["gpu-h100".into()];
        let why = place(
            &i,
            &[node("a", 8, 16384)],
            &[],
            &[],
            &Default::default(),
            &[],
        )
        .unwrap_err();
        assert!(
            describe(&why[0].why).contains("no device class gpu-h100"),
            "{:?}",
            why[0].why
        );
    }

    fn peer(name: &str, order: u32, state: InstanceState, started_at: Option<u64>) -> StartPeer {
        StartPeer {
            name: name.into(),
            order,
            state,
            desired: DesiredState::Running,
            started_at: started_at.map(Timestamp),
        }
    }

    /// The case a platform is judged on: power comes back and forty guests
    /// start at once.
    #[test]
    fn a_guest_waits_for_what_is_ahead_of_it_and_says_what() {
        let db = peer("db-1", 1, InstanceState::Stopped, None);
        let gate = start_gate(2, 0, &[db], Timestamp(0));
        assert_eq!(
            gate,
            StartGate::WaitingFor {
                instance: "db-1".into()
            },
            "the web tier started without its database"
        );

        // Once it is up, the next order goes.
        let db = peer("db-1", 1, InstanceState::Running, Some(1000));
        assert_eq!(start_gate(2, 0, &[db], Timestamp(2000)), StartGate::Go);
    }

    /// A guest that cannot start does not take the whole node down with it.
    ///
    /// The deadlock this avoids: wait forever for something that has failed,
    /// and one broken disk keeps forty machines off.
    #[test]
    fn a_failed_guest_ahead_does_not_block_the_ones_behind_it() {
        let broken = peer("db-1", 1, InstanceState::Failed, None);
        assert_eq!(start_gate(2, 0, &[broken], Timestamp(0)), StartGate::Go);
    }

    /// A guest nobody wants running is not something to wait for.
    #[test]
    fn a_deliberately_stopped_guest_ahead_does_not_block() {
        let mut off = peer("db-1", 1, InstanceState::Stopped, None);
        off.desired = DesiredState::Stopped;
        assert_eq!(start_gate(2, 0, &[off], Timestamp(0)), StartGate::Go);
    }

    /// The settling time is measured from the group, not from each member.
    ///
    /// Otherwise a fleet's boot is the sum of every delay rather than the
    /// longest one, and a hundred guests at thirty seconds each is fifty
    /// minutes of nothing happening.
    #[test]
    fn the_delay_is_measured_from_the_group_ahead_not_each_member_in_turn() {
        let ahead = [
            peer("db-1", 1, InstanceState::Running, Some(1_000)),
            peer("db-2", 1, InstanceState::Running, Some(3_000)),
        ];
        // Ten seconds after the *newest* of them.
        assert_eq!(start_gate(2, 10, &ahead, Timestamp(13_000)), StartGate::Go);
        assert_eq!(
            start_gate(2, 10, &ahead, Timestamp(8_000)),
            StartGate::WaitingOut { seconds: 5 }
        );
    }

    /// Nothing ahead means nothing to wait for, delay or no delay.
    #[test]
    fn the_first_group_never_waits() {
        assert_eq!(start_gate(0, 30, &[], Timestamp(0)), StartGate::Go);
        let behind = [peer("web-1", 5, InstanceState::Stopped, None)];
        assert_eq!(
            start_gate(1, 30, &behind, Timestamp(0)),
            StartGate::Go,
            "a guest waited for one that comes after it"
        );
    }

    /// A settling time with nothing running ahead has nothing to settle after.
    #[test]
    fn a_delay_with_nothing_running_ahead_does_not_wait_forever() {
        let failed = [peer("db-1", 1, InstanceState::Failed, None)];
        assert_eq!(start_gate(2, 30, &failed, Timestamp(0)), StartGate::Go);
    }

    /// The remaining wait is rounded up, so "0 seconds left" never means
    /// "still waiting".
    #[test]
    fn the_wait_is_reported_in_whole_seconds_that_are_never_zero() {
        let ahead = [peer("db-1", 1, InstanceState::Running, Some(0))];
        assert_eq!(
            start_gate(2, 10, &ahead, Timestamp(9_900)),
            StartGate::WaitingOut { seconds: 1 }
        );
    }

    fn sized(id: &str, vcpus: u32, mem: u64, used_mem: u64) -> Node {
        let mut n = node(id, vcpus, mem);
        n.status.allocated.memory_mib = used_mem;
        n
    }

    /// The number a sum cannot give: free memory does not add up into a guest.
    ///
    /// Four nodes with 16 GiB free each is 64 GiB "free" and fits no 32 GiB
    /// guest at all. Every dashboard that shows the sum has told somebody it
    /// would.
    #[test]
    fn the_largest_guest_that_fits_is_not_the_sum_of_what_is_free() {
        let cell: Vec<Node> = (1..=4)
            .map(|i| sized(&format!("n{i}"), 8, 32_768, 16_384))
            .collect();
        let h = headroom(&cell, &[]);

        assert_eq!(
            h.free.memory_mib,
            4 * 16_384,
            "the sum is still worth having"
        );
        assert_eq!(
            h.largest_fit.memory_mib, 16_384,
            "a 32 GiB guest was reported as fitting a cell with no room for one"
        );
    }

    /// A drained node's empty half is not free, and the two numbers disagree
    /// on purpose.
    #[test]
    fn a_drained_node_counts_toward_the_total_and_not_toward_what_is_free() {
        let mut draining = sized("n2", 8, 32_768, 0);
        draining.spec.schedulable = false;
        let cell = vec![sized("n1", 8, 32_768, 8_192), draining];

        let h = headroom(&cell, &[]);
        assert_eq!(h.usable_nodes, 1);
        assert_eq!(h.unusable_nodes, 1);
        assert_eq!(h.total.memory_mib, 2 * 32_768, "the machine still exists");
        assert_eq!(
            h.free.memory_mib,
            32_768 - 8_192,
            "a drained node's empty memory was counted as room"
        );
        assert_eq!(h.largest_fit.memory_mib, 32_768 - 8_192);
    }

    /// A node being emptied is not somewhere to put things either.
    #[test]
    fn a_node_being_emptied_offers_no_room() {
        let mut leaving = sized("n1", 8, 32_768, 0);
        leaving.spec.evacuate = true;
        let h = headroom(&[leaving], &[]);
        assert_eq!(h.usable_nodes, 0);
        assert_eq!(h.free.memory_mib, 0);
    }

    /// A node nobody has heard from offers nothing, whatever it last said.
    #[test]
    fn a_node_that_is_not_ready_offers_no_room() {
        let mut quiet = sized("n1", 8, 32_768, 0);
        quiet.status.conditions.clear();
        let h = headroom(&[quiet], &[]);
        assert_eq!(h.usable_nodes, 0);
        assert_eq!(h.free.memory_mib, 0);
        assert_eq!(h.total.memory_mib, 32_768);
    }

    /// Sharing a processor is a trade an operator makes on purpose: two guests
    /// that both want a core get one each in turn, and being wrong costs speed.
    /// Sharing memory is not that trade, which is why there is no ratio for it.
    #[test]
    fn a_node_told_to_share_its_cores_has_room_for_more_guests_and_no_more_memory() {
        let mut plain = sized("n1", 8, 32_768, 0);
        assert_eq!(offered_vcpus(&plain), 8);
        // Zero is "nobody set one", so a node stored before the field existed
        // behaves exactly as it did.
        plain.spec.vcpu_overcommit = 0;
        assert_eq!(offered_vcpus(&plain), 8);

        let mut shared = plain.clone();
        shared.spec.vcpu_overcommit = 4;
        assert_eq!(offered_vcpus(&shared), 32);

        let h = headroom(&[shared.clone()], &[]);
        assert_eq!(
            h.free.vcpus, 32,
            "the ratio did not reach what can be placed"
        );
        assert_eq!(h.largest_fit.vcpus, 32);
        // Silicon is silicon. A capacity page that reported 32 cores would say
        // the cell had grown a processor.
        assert_eq!(h.total.vcpus, 8);
        assert_eq!(h.offered_vcpus, 32);
        // And not one mebibyte more memory: a guest promised 8 GiB and handed
        // 4 does not run slowly, it is killed.
        assert_eq!(h.free.memory_mib, 32_768);

        // It really places: a guest wanting more vCPUs than the node has cores
        // is refused on a plain node and taken by a shared one.
        let mut big = inst("projects/p1/instances/i1");
        big.spec.vcpus = 16;
        big.spec.memory_mib = 1024;
        assert!(place(&big, &[plain], &[], &[], &Default::default(), &[]).is_err());
        assert_eq!(
            place(&big, &[shared], &[], &[], &Default::default(), &[]).unwrap(),
            "n1"
        );
    }

    /// A machine inside an open maintenance window is unusable in exactly the
    /// way a draining one is — and it must not appear in `largest_fit`, which
    /// is the number a tenant is told they can start.
    #[test]
    fn a_node_out_of_service_lends_nothing_to_what_can_be_started() {
        let cell = vec![sized("n1", 8, 32_768, 0), sized("n2", 64, 262_144, 0)];
        let open = headroom(&cell, &[]);
        assert_eq!(open.largest_fit.memory_mib, 262_144);

        let closed = vec![crate::maintenance::Closed {
            node: "n2".into(),
            until: Timestamp(0),
            minutes_left: 40,
            note: String::new(),
            window: "maintenance-windows/w".into(),
        }];
        let h = headroom(&cell, &closed);
        assert_eq!(
            h.largest_fit.memory_mib, 32_768,
            "a machine out of service was still offered as room for a guest"
        );
        assert_eq!(h.usable_nodes, 1);
        assert_eq!(h.unusable_nodes, 1);
        // Still counted in the total: the hardware exists, it is merely out
        // this evening, and a capacity page that made it vanish would read as
        // a machine having been lost.
        assert_eq!(h.total.memory_mib, 294_912);
    }

    /// A node on its way out is not part of the cell at all.
    #[test]
    fn a_node_being_removed_is_in_neither_count() {
        let mut going = sized("n1", 8, 32_768, 0);
        going.meta.deleted_at = Some(Timestamp(1));
        let h = headroom(&[going], &[]);
        assert_eq!(h.usable_nodes, 0);
        assert_eq!(h.unusable_nodes, 0);
        assert_eq!(
            h.total.memory_mib, 0,
            "a machine being removed was still counted"
        );
    }

    #[test]
    fn placement_picks_the_emptiest_node_that_fits() {
        let i = inst("projects/p1/instances/i1");
        let nodes = vec![node("a", 8, 4096), node("b", 8, 16384)];
        assert_eq!(
            place(&i, &nodes, &[], &[], &Default::default(), &[]).unwrap(),
            "b"
        );
    }

    /// A machine somebody has declared out of service is not a candidate — and
    /// the rejection says when it comes back and what it is for, because
    /// otherwise "no valid host" sends an operator hunting for a fault they
    /// themselves scheduled.
    #[test]
    fn a_node_inside_a_maintenance_window_is_not_offered_and_says_why() {
        let i = inst("projects/p1/instances/i1");
        let nodes = vec![node("a", 8, 16384), node("b", 8, 65536)];
        let closed = vec![crate::maintenance::Closed {
            node: "b".into(),
            until: Timestamp(0),
            minutes_left: 40,
            note: "swapping the failed DIMM in slot 3".into(),
            window: "maintenance-windows/dimm-swap".into(),
        }];
        // The emptiest node would have won on every other pass.
        assert_eq!(
            place(&i, &nodes, &[], &[], &Default::default(), &closed).unwrap(),
            "a"
        );

        let mut alone = nodes;
        alone.remove(0);
        let why = place(&i, &alone, &[], &[], &Default::default(), &closed).unwrap_err();
        assert!(matches!(
            why[0].why,
            Rejected::InMaintenance {
                minutes_left: 40,
                ..
            }
        ));
        let said = describe(&why[0].why);
        assert!(said.contains("another 40 minutes"), "{said}");
        assert!(
            said.contains("DIMM"),
            "the operator's own words are not in it: {said}"
        );
    }

    /// Anti-affinity keeps a service alive when a machine dies; affinity keeps
    /// it fast while they all live. A platform with only the first can express
    /// only half of what people actually run.
    #[test]
    fn a_guest_that_asked_to_be_near_its_group_is_placed_beside_it() {
        let mut i = inst("projects/p1/instances/cache-1");
        i.spec.memory_mib = 1024;
        i.spec.placement_policy.affinity_group = Some("web".into());
        // The emptiest node is `a`, and it would win on every other pass.
        let nodes = vec![node("a", 8, 65_536), node("b", 8, 16_384)];
        let with = vec![("web".to_string(), "b".to_string())];
        assert_eq!(
            place(&i, &nodes, &[], &with, &Default::default(), &[]).unwrap(),
            "b"
        );

        // With nobody placed yet there is nothing to be near, and refusing
        // every node would mean a group whose first member could never start.
        assert_eq!(
            place(&i, &nodes, &[], &[], &Default::default(), &[]).unwrap(),
            "a"
        );
    }

    /// A required affinity that cannot be honoured says where the rest of the
    /// group is — one of those names is a machine to go and look at, and "no
    /// valid host" is not.
    #[test]
    fn a_group_that_will_not_fit_together_says_where_the_rest_of_it_is() {
        let mut i = inst("projects/p1/instances/cache-1");
        i.spec.memory_mib = 32_768;
        i.spec.placement_policy.affinity_group = Some("web".into());
        let nodes = vec![node("a", 8, 65_536), node("b", 8, 16_384)];
        let with = vec![("web".to_string(), "b".to_string())];

        let why = place(&i, &nodes, &[], &with, &Default::default(), &[]).unwrap_err();
        let about_a = why.iter().find(|e| e.node == "a").unwrap();
        assert!(matches!(about_a.why, Rejected::NotWithGroup { .. }));
        assert!(
            describe(&about_a.why).contains("on b"),
            "{}",
            describe(&about_a.why)
        );

        // A wish rather than a rule: crowded beats not running at all — and
        // here the roomy node wins outright because the group's own node is
        // too small.
        i.spec.placement_policy.affinity = crate::resources::Strength::Preferred;
        assert_eq!(
            place(&i, &nodes, &[], &with, &Default::default(), &[]).unwrap(),
            "a"
        );
    }

    /// Three replicas of a database must not share a machine even if that
    /// means one stays down; twelve web servers would rather be crowded than
    /// short. Both are right answers to different questions.
    #[test]
    fn a_preferred_spread_takes_a_crowded_node_over_not_running() {
        let mut i = inst("projects/p1/instances/web-3");
        i.spec.memory_mib = 1024;
        i.spec.placement_policy.anti_affinity_group = Some("web".into());
        let only = vec![node("a", 8, 65_536)];
        let taken = vec![("web".to_string(), "a".to_string())];

        // Required — the default, and what this platform did before the field
        // existed.
        let why = place(&i, &only, &taken, &[], &Default::default(), &[]).unwrap_err();
        assert!(matches!(why[0].why, Rejected::AntiAffinity { .. }));

        i.spec.placement_policy.spread = crate::resources::Strength::Preferred;
        assert_eq!(
            place(&i, &only, &taken, &[], &Default::default(), &[]).unwrap(),
            "a"
        );

        // And it is still a preference: given a choice, the empty node wins
        // even though it has less room than the crowded one.
        let both = vec![node("a", 8, 65_536), node("b", 8, 16_384)];
        assert_eq!(
            place(&i, &both, &taken, &[], &Default::default(), &[]).unwrap(),
            "b",
            "a preference for spreading lost to free memory"
        );
    }

    #[test]
    fn placement_explains_every_rejection() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.spec.memory_mib = 99999;
        let nodes = vec![node("a", 8, 4096), node("b", 8, 16384)];
        let why = place(&i, &nodes, &[], &[], &Default::default(), &[]).unwrap_err();
        assert_eq!(why.len(), 2, "an operator must learn about every candidate");
        assert!(matches!(why[0].why, Rejected::InsufficientMemory { .. }));
    }

    #[test]
    fn a_host_with_the_memory_but_not_on_one_numa_node_is_refused() {
        // Scheduling here would succeed and the guest would fail to start —
        // the worst outcome, because it looks like a hypervisor fault.
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.spec.memory_mib = 8192;
        let mut n = node("a", 8, 16384);
        n.status.capacity.numa_free_mib = vec![4096, 4096];
        let why = place(&i, &[n], &[], &[], &Default::default(), &[]).unwrap_err();
        assert_eq!(why[0].why, Rejected::NoNumaNodeFits { want_mib: 8192 });
    }

    #[test]
    fn anti_affinity_keeps_a_group_off_one_host() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.spec.placement_policy.anti_affinity_group = Some("web".into());
        let nodes = vec![node("a", 8, 16384)];
        let occupied = vec![("web".to_string(), "a".to_string())];
        assert!(place(&i, &nodes, &occupied, &[], &Default::default(), &[]).is_err());
    }

    #[test]
    fn a_draining_node_takes_nothing_new() {
        let i = inst("projects/p1/instances/i1");
        let mut n = node("a", 8, 16384);
        n.spec.schedulable = false;
        let why = place(&i, &[n], &[], &[], &Default::default(), &[]).unwrap_err();
        assert_eq!(why[0].why, Rejected::Unschedulable);
    }

    #[test]
    fn an_unconverged_instance_is_unknown_rather_than_wrong() {
        let mut i = super::tests::inst("projects/p1/instances/i1");
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
        let mut i = super::tests::inst("projects/p1/instances/i1");
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

        let used = count_quota(&project, &[mine, dying, theirs], &[], &[], &[]);
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
            devices: 0,
            instances: 2,
            vcpus: 8,
            memory_mib: 8192,
            volumes: 5,
            volume_gib: 100,
            floating_ips: 2,
            load_balancers: 2,
        };
        let within = Quota {
            devices: 0,
            instances: 1,
            vcpus: 2,
            memory_mib: 2048,
            volumes: 2,
            volume_gib: 20,
            floating_ips: 1,
            load_balancers: 1,
        };
        assert_eq!(
            quota_condition(&limit, &within, 1).status,
            ConditionStatus::True
        );

        let over = Quota {
            devices: 0,
            instances: 3,
            vcpus: 32,
            volumes: 9,
            floating_ips: 4,
            load_balancers: 3,
            ..within.clone()
        };
        let c = quota_condition(&limit, &over, 1);
        assert_eq!(c.status, ConditionStatus::False);
        assert_eq!(c.reason, "OverQuota");
        assert!(c.message.contains("instances"), "{}", c.message);
        assert!(c.message.contains("vcpus"), "{}", c.message);
        assert!(c.message.contains("volumes"), "{}", c.message);
        assert!(c.message.contains("floating IPs"), "{}", c.message);
        assert!(c.message.contains("load balancers"), "{}", c.message);

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

    /// An operation for something nobody reports on has to finish.
    ///
    /// A subnet, an image, a security group: no agent owns one, so
    /// `observedGeneration` stays at zero for its whole life. An operation that
    /// waited for a report waited for ever — and an operation is precisely what
    /// a client is told to poll after a create.
    ///
    /// Measured on a real cell: seven ordinary creates left seven operations
    /// that would never finish.
    #[test]
    fn an_operation_for_something_nobody_reports_on_finishes_when_it_exists() {
        for target in [
            "projects/p1/subnets/lan0",
            "projects/p1/images/sha256-abc",
            "projects/p1/snapshot-schedules/nightly",
            "users/ada",
        ] {
            let spec = OperationSpec {
                target: target.to_string(),
                target_generation: 1,
                verb: "create".into(),
                requested_by: "ada".into(),
            };
            // Exactly what such an object looks like for ever: nobody has
            // looked at it, and there is no condition to read.
            let never_reported = TargetView::Present {
                observed_generation: 0,
                ready: ConditionStatus::Unknown,
                reason: String::new(),
                message: String::new(),
            };
            let progress = operation_progress(&spec, &never_reported);
            assert!(progress.done, "{target} would have waited for ever");
            assert_eq!(progress.error, None);
        }

        // And something that *is* reported on still waits, or this would have
        // turned every create into an immediate success. A security group is
        // one of those: the API computes an `Applied` condition for it on the
        // way out, so there is a report to wait for.
        for target in [
            "projects/p1/instances/i1",
            "projects/p1/security-groups/web",
        ] {
            let spec = OperationSpec {
                target: target.to_string(),
                target_generation: 1,
                verb: "create".into(),
                requested_by: "ada".into(),
            };
            let not_yet = TargetView::Present {
                observed_generation: 0,
                ready: ConditionStatus::Unknown,
                reason: String::new(),
                message: String::new(),
            };
            assert!(!operation_progress(&spec, &not_yet).done, "{target}");
        }
        let spec = OperationSpec {
            target: "projects/p1/instances/i1".into(),
            target_generation: 1,
            verb: "create".into(),
            requested_by: "ada".into(),
        };
        let not_yet = TargetView::Present {
            observed_generation: 0,
            ready: ConditionStatus::Unknown,
            reason: String::new(),
            message: String::new(),
        };
        assert!(!operation_progress(&spec, &not_yet).done);
    }

    #[test]
    fn a_kind_is_the_segment_before_the_id() {
        assert_eq!(kind_of("projects/p1/subnets/lan0"), "subnets");
        assert_eq!(kind_of("nodes/node-a"), "nodes");
        assert_eq!(
            kind_of("projects/p1/volumes/data/snapshots/nightly"),
            "snapshots"
        );
        // Not a resource name: the caller then assumes something reports on it,
        // which is the safe direction.
        assert_eq!(kind_of("nonsense"), "");
        assert!(!nobody_reports_on(""));
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
        let mut i = super::tests::inst("projects/p1/instances/i1");
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

#[cfg(test)]
mod teardown_and_stopping {
    use super::*;
    use crate::meta::Timestamp;

    fn guest(state: InstanceState, asked: Option<u64>) -> Instance {
        let mut i = super::tests::inst("projects/p1/instances/g1");
        // No ports: what is under test is the stop, and a port that still
        // wants programming would put its own action in front of it.
        i.spec.ports = Vec::new();
        i.spec.desired_state = DesiredState::Stopped;
        i.status.state = state;
        i.status.stop_requested_at = asked.map(Timestamp);
        i
    }

    /// The button first. Almost every guest answers it, and one that does
    /// should shut down cleanly rather than lose what it had not written.
    #[test]
    fn a_stop_asks_before_it_insists() {
        let now = Timestamp(10 * STOP_GRACE_MS);
        let actions = reconcile_instance(
            &guest(InstanceState::Running, None),
            true,
            &[],
            true,
            StartGate::Go,
            now,
        );
        assert!(
            matches!(actions.as_slice(), [Action::StopVm { .. }]),
            "{actions:?}"
        );
    }

    /// And nothing while it is still within its grace: asking twice is asking
    /// once, and a guest halfway through unmounting is not stuck.
    #[test]
    fn a_guest_that_is_shutting_down_is_left_alone() {
        let now = Timestamp(10 * STOP_GRACE_MS);
        let asked = now.0 - STOP_GRACE_MS / 2;
        let actions = reconcile_instance(
            &guest(InstanceState::Running, Some(asked)),
            true,
            &[],
            true,
            StartGate::Go,
            now,
        );
        assert!(actions.is_empty(), "{actions:?}");
    }

    /// The failure this exists for: an ACPI press reaches a guest with no
    /// operating system to answer it — wedged in its bootloader, or panicked —
    /// and before this the platform pressed the button again every pass, for
    /// ever, while the object said "wanted Stopped, the node reports Running".
    /// Nothing was broken and nothing would ever happen.
    #[test]
    fn a_guest_that_never_answers_has_its_plug_pulled() {
        let now = Timestamp(10 * STOP_GRACE_MS);
        let asked = now.0 - STOP_GRACE_MS;
        let actions = reconcile_instance(
            &guest(InstanceState::Running, Some(asked)),
            true,
            &[],
            true,
            StartGate::Go,
            now,
        );
        assert!(
            matches!(actions.as_slice(), [Action::KillVm { .. }]),
            "{actions:?}"
        );
    }

    /// A guest that is already stopped is not killed for good measure.
    #[test]
    fn a_stopped_guest_is_left_stopped() {
        let now = Timestamp(10 * STOP_GRACE_MS);
        let actions = reconcile_instance(
            &guest(InstanceState::Stopped, Some(0)),
            true,
            &[],
            true,
            StartGate::Go,
            now,
        );
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn a_guest_nobody_has_reported_on_is_still_taken_apart() {
        // The shape a node agent restart leaves behind: the VMM is running, the
        // object says `Unknown` because nothing has reported yet, and somebody
        // asks for the guest to go away.
        //
        // Skipping the delete here does not save any work — it strands the guest.
        // The tap is still held by the VMM, `ip tuntap del` fails with `Device
        // or resource busy`, that failure ends the pass before the finalizer is
        // released, and the object cannot be removed by any means the API
        // offers. Found on a real cell, on three guests at once, after a deploy
        // restarted the agent.
        let mut i = super::tests::inst("projects/p1/instances/i1");
        i.meta.deleted_at = Some(Timestamp(1));
        i.meta.finalizers = vec![NODE_RELEASE_FINALIZER.to_string()];
        i.status.state = InstanceState::Unknown;
        i.spec.ports = vec!["projects/p1/ports/pt1".into()];

        let actions = reconcile_instance(&i, false, &[false], false, StartGate::Go, Timestamp(2));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::DeleteVm { .. })),
            "a guest nobody has reported on was left running: {actions:?}"
        );
        // And in the order that makes it work: the machine goes before the wire
        // it is holding.
        let vm = actions
            .iter()
            .position(|a| matches!(a, Action::DeleteVm { .. }))
            .unwrap();
        let port = actions
            .iter()
            .position(|a| matches!(a, Action::UnprogramPort { .. }))
            .unwrap();
        assert!(vm < port, "the tap was removed before the guest let go of it");
    }

}
