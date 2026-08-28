//! Turning what the machine says into what the object says.
//!
//! Pure functions, kept apart from the loop for the same reason the model keeps
//! its decisions pure: what a node reports is worth reading on its own, and it
//! is worth being able to argue about without a store or a hypervisor in the
//! way.

use velstra_cloud_model::{
    cpu::GuestCpu,
    meta::{Condition, ConditionStatus},
    resources::{Attachment, InstanceState, InstanceStatus},
};

use crate::host::HostState;

/// What this machine says about one guest, written into a status.
pub(super) fn observe_instance(
    status: &mut InstanceStatus,
    host: &HostState,
    name: &str,
    deleting: bool,
    // Whether this guest's operator asked to watch its console.
    watched: bool,
) {
    match host.vms.get(name) {
        Some(vm) => {
            // Whether this is the same run we saw last pass, decided before
            // `started_at` is overwritten. A guest that has restarted was
            // given whatever this node presents *now*, which may not be what
            // it was given before — a baseline can have been declared in
            // between.
            let same_run = status.started_at == vm.started_at && status.cpu.is_some();

            status.state = vm.state;
            status.vmm_pid = vm.pid;
            status.started_at = vm.started_at;

            // A guest that is no longer running is no longer being waited for.
            // Clearing it here rather than where the stop is issued keeps the
            // field a description of the world: it is set while somebody is
            // waiting and absent when nobody is.
            if vm.state != InstanceState::Running {
                status.stop_requested_at = None;
            }

            // The devices this guest holds. Reported by the VMM rather than
            // re-chosen, and cleared when the guest is not running, for the
            // same reason the CPU is: while it runs, what it holds is a fact;
            // while it does not, holding a claim on a piece of hardware
            // nobody is using would keep it out of everyone else's reach.
            status.devices = match vm.state {
                InstanceState::Running => vm.devices.clone(),
                _ => Vec::new(),
            };

            // Two rules, and the second one is why this is not simply
            // `if watched`.
            //
            // A watched guest publishes because somebody is looking. A guest
            // that is **not running** publishes because it is the only moment
            // its last words are worth anything — and making an operator turn
            // the switch on and then wait for the failure to happen again
            // would be the wrong answer to the only question a dead guest is
            // ever asked.
            //
            // Everything else publishes nothing, which is what keeps a
            // converged agent quiet: a status is written only when it changed,
            // and a console tail on every healthy guest would move on every
            // line any of them logged.
            let say = watched || vm.state != InstanceState::Running;
            status.console_tail = if say {
                vm.console_tail.clone()
            } else {
                String::new()
            };
            status.console_bytes = if say { vm.console_bytes } else { 0 };
            // What the machine actually is. Same life as the CPU below and for
            // the same reason: while it runs, this is a fact; while it does
            // not, there is nothing to differ from and a stale value would
            // read as a change nobody asked for.
            status.running_size = match vm.state {
                InstanceState::Running => vm.size,
                _ => None,
            };
            status.cpu = match vm.state {
                // Recorded once per run, from what this node presents. Not
                // re-derived on every pass: the point of the field is to
                // outlast a change to the node's baseline, and a value
                // recomputed each pass would silently adopt one.
                InstanceState::Running if !same_run => host.cpu.as_ref().map(|node| GuestCpu {
                    model: node.presents.clone(),
                    arch: node.arch.clone(),
                    flags: node.presented_flags.clone(),
                }),
                InstanceState::Running => status.cpu.take(),
                // Not running: there is no CPU it is running with. Cleared
                // rather than kept, because the field means "what this live
                // guest may not be parted from", and a stale one would
                // over-constrain where it is allowed to start next.
                _ => None,
            };
        }
        None => {
            // Nothing of this instance is on this machine. While it is being
            // torn down that is the finished state and `Unknown` is the honest
            // word for "this node holds nothing of it"; otherwise the node has
            // looked and found no guest, which is an observation, and the word
            // for that is `Stopped`.
            status.state = if deleting {
                InstanceState::Unknown
            } else {
                InstanceState::Stopped
            };
            status.vmm_pid = None;
            status.started_at = None;
            status.cpu = None;
            status.devices = Vec::new();
            // Nothing of this guest is here, so this node has nothing to say
            // about what it wrote. Whatever it did say went with the machine.
            status.console_tail = String::new();
            status.console_bytes = 0;
            status.running_size = None;
        }
    }
}

/// Whether the machine did what was asked.
///
/// Kept apart from `Ready` deliberately. `Ready` is decided in one place for the
/// whole system ([`instance_condition`]); this says why the machine could not
/// get there, which is a fact only the node has. Two conditions, two owners, no
/// argument about what a single field means.
pub(super) fn host_condition(outcome: &Result<(), String>, at_generation: u64) -> Condition {
    match outcome {
        Ok(()) => Condition::new(
            "HostActions",
            ConditionStatus::True,
            "Done",
            "",
            at_generation,
        ),
        Err(why) => Condition::new(
            "HostActions",
            ConditionStatus::False,
            "ActionFailed",
            why,
            at_generation,
        ),
    }
}

/// Whether this node still holds anything of an object that is being deleted.
///
/// The agent cannot drop the finalizer — metadata belongs to a controller — so
/// it publishes the fact a controller needs in order to drop it. An explicit
/// condition, rather than a controller inferring release from some combination
/// of other fields, because the inference would be a second definition of
/// "let go" living somewhere else.
pub(super) fn release_condition(released: bool, deleting: bool, at_generation: u64) -> Condition {
    if !deleting {
        return Condition::new(
            "Released",
            ConditionStatus::False,
            "InUse",
            "",
            at_generation,
        );
    }
    if released {
        Condition::new(
            "Released",
            ConditionStatus::True,
            "Released",
            "this node holds nothing of it; the finalizer may go",
            at_generation,
        )
    } else {
        Condition::new(
            "Released",
            ConditionStatus::Unknown,
            "TearingDown",
            "the node is still taking it apart",
            at_generation,
        )
    }
}

/// What `Ready` means for an attachment.
///
/// The model decides this for instances and nothing else yet; this is the same
/// judgement in the same shape, and it belongs beside `instance_condition` the
/// moment a second party needs to agree with it.
pub(super) fn attachment_condition(attachment: &Attachment) -> Condition {
    let at_generation = attachment.meta.generation;
    if !attachment.converged() {
        return Condition::new(
            "Ready",
            ConditionStatus::Unknown,
            "Converging",
            "the node has not reported on this change yet",
            attachment.status.observed_generation,
        );
    }
    match (attachment.meta.is_deleting(), attachment.status.attached) {
        (false, true) => Condition::ready(at_generation),
        (false, false) => Condition::new(
            "Ready",
            ConditionStatus::False,
            "NotOpen",
            "the node does not have the volume open",
            at_generation,
        ),
        (true, true) => Condition::new(
            "Ready",
            ConditionStatus::False,
            "Detaching",
            "asked to let go, still open",
            at_generation,
        ),
        (true, false) => Condition::new(
            "Ready",
            ConditionStatus::False,
            "Released",
            "closed; the volume may be attached elsewhere",
            at_generation,
        ),
    }
}
