//! Turning what the machine says into what the object says.
//!
//! Pure functions, kept apart from the loop for the same reason the model keeps
//! its decisions pure: what a node reports is worth reading on its own, and it
//! is worth being able to argue about without a store or a hypervisor in the
//! way.

use velstra_cloud_model::{
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
) {
    match host.vms.get(name) {
        Some(vm) => {
            status.state = vm.state;
            status.vmm_pid = vm.pid;
            status.started_at = vm.started_at;
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
