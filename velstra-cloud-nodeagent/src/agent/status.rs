//! Turning what the machine says into what the object says.
//!
//! Pure functions, kept apart from the loop for the same reason the model keeps
//! its decisions pure: what a node reports is worth reading on its own, and it
//! is worth being able to argue about without a store or a hypervisor in the
//! way.

use velstra_cloud_model::{
    cpu::GuestCpu,
    meta::{Condition, ConditionStatus, Timestamp},
    resources::{Attachment, GuestUsage, InstanceState, InstanceStatus},
};

use crate::host::HostState;

/// One pass's raw reading of a guest, before it is a status.
///
/// Counters only. The rate is worked out from this and the previous reading,
/// in [`usage_from`], so the arithmetic that a person reads a graph from is a
/// pure function of two numbers and a clock rather than something buried in
/// the loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Sampled {
    pub cpu_ms: u64,
    pub memory_mib: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub disk: crate::host::DiskTraffic,
}

/// This reading, and the share of its vCPUs the guest used getting here.
///
/// `previous` is what the object already says, which is what makes this work
/// without the agent keeping a table: the last reading is on the object, where
/// everything else about the guest is.
///
/// The percentage is left at zero when there is no previous reading to
/// difference against, when the clock has not moved, or when the counter went
/// **backwards** — which happens on a restart, and inventing a rate out of a
/// counter reset would put a spike on the graph at exactly the moment somebody
/// is trying to read what happened.
pub(super) fn usage_from(
    previous: Option<&GuestUsage>,
    now: Timestamp,
    sample: Sampled,
    vcpus: u32,
) -> GuestUsage {
    let cpu_percent = match previous {
        Some(before)
            if now.0 > before.at.0
                && sample.cpu_ms >= before.cpu_ms
                // A reading from a previous run of the same guest: the counter
                // restarted, and the elapsed time is not the time it ran for.
                && before.at.0 > 0 =>
        {
            let elapsed_ms = now.0 - before.at.0;
            let used_ms = sample.cpu_ms - before.cpu_ms;
            let percent = used_ms.saturating_mul(100) / elapsed_ms.max(1);
            // Capped at what the guest was given. A guest cannot use more
            // than its vCPUs, and a number above that is a clock that moved
            // oddly rather than a machine that did.
            percent.min(u64::from(vcpus.max(1)) * 100) as u32
        }
        _ => 0,
    };
    // The billable totals, carried across a tap that was remade. A counter
    // that went backwards did not move backwards: it started again, and
    // everything it now reads is new traffic.
    let carry = |raw: u64, before_raw: u64, before_total: u64| -> u64 {
        let moved = if raw >= before_raw {
            raw - before_raw
        } else {
            raw
        };
        before_total.saturating_add(moved)
    };
    let (rx_total, tx_total) = match previous {
        Some(before) => (
            carry(sample.rx_bytes, before.rx_bytes, before.rx_total),
            carry(sample.tx_bytes, before.tx_bytes, before.tx_total),
        ),
        // Nothing to carry from. The tap's own counter is the whole of what
        // this node has seen, which is the honest starting point — and it is
        // where a guest that has just been created starts anyway.
        None => (sample.rx_bytes, sample.tx_bytes),
    };
    GuestUsage {
        at: now,
        cpu_ms: sample.cpu_ms,
        cpu_percent,
        memory_mib: sample.memory_mib,
        rx_bytes: sample.rx_bytes,
        tx_bytes: sample.tx_bytes,
        rx_packets: sample.rx_packets,
        tx_packets: sample.tx_packets,
        rx_total,
        tx_total,
        disk_read_bytes: sample.disk.read_bytes,
        disk_write_bytes: sample.disk.write_bytes,
        disk_read_ops: sample.disk.read_ops,
        disk_write_ops: sample.disk.write_ops,
    }
}

/// What this machine says about one guest, written into a status.
///
/// Eight arguments, every one a different thing the machine said. A struct
/// holding them would be built at the one call site that has them and taken
/// apart here.
#[allow(clippy::too_many_arguments)]
pub(super) fn observe_instance(
    status: &mut InstanceStatus,
    host: &HostState,
    name: &str,
    deleting: bool,
    // Whether this guest's operator asked to watch its console.
    watched: bool,
    // This pass's reading, when the guest is running and the node could take
    // one. `None` is a machine that cannot read its own process table, and it
    // leaves the previous reading alone rather than clearing it.
    sample: Option<Sampled>,
    vcpus: u32,
    // How often a reading is *reported*, in milliseconds. See below.
    every_ms: u64,
) {
    let now = Timestamp::now();
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
            // What it is using. Only while it runs: a guest that is stopped is
            // using nothing, and leaving the last reading there would have a
            // console showing a stopped machine at 40 % for ever.
            status.usage = match (vm.state, sample) {
                // On a cadence of its own, not on the resync. Every counter
                // here moves on every pass, so reporting it whenever the pass
                // ran would make a status write per guest per resync — a
                // hundred guests on a thirty-second loop is three writes a
                // second of nothing but traffic counters, waking every watcher
                // in the cell. The reading is worth having; it is not worth
                // that. Until the window is up, the previous reading stands
                // with its own timestamp on it, which is what says how old it
                // is.
                (InstanceState::Running, Some(sample))
                    if status
                        .usage
                        .is_none_or(|before| now.0.saturating_sub(before.at.0) >= every_ms) =>
                {
                    Some(usage_from(status.usage.as_ref(), now, sample, vcpus))
                }
                // Running, and this node could not take a reading. The last
                // one stands with its own timestamp on it, which is what says
                // how old it is.
                (InstanceState::Running, _) => status.usage.take(),
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
            status.usage = None;
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

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn sample(cpu_ms: u64) -> Sampled {
        Sampled {
            cpu_ms,
            memory_mib: 512,
            rx_bytes: 10,
            tx_bytes: 20,
            rx_packets: 1,
            tx_packets: 2,
            disk: crate::host::DiskTraffic {
                read_bytes: 4_096,
                write_bytes: 8_192,
                read_ops: 1,
                write_ops: 2,
            },
        }
    }

    /// The rate is a difference over an interval. Half a second of CPU in one
    /// second is fifty percent of one core.
    #[test]
    fn the_share_is_the_difference_over_the_interval() {
        let before = GuestUsage {
            at: Timestamp(1_000_000),
            cpu_ms: 4_000,
            ..Default::default()
        };
        let now = usage_from(Some(&before), Timestamp(1_001_000), sample(4_500), 2);
        assert_eq!(now.cpu_percent, 50);
        assert_eq!(now.cpu_ms, 4_500, "the counter is carried, not the rate");
        assert_eq!(now.at, Timestamp(1_001_000));
    }

    /// Two cores fully busy is two hundred percent, and that is the cap.
    #[test]
    fn a_guest_cannot_use_more_than_it_was_given() {
        let before = GuestUsage {
            at: Timestamp(1_000_000),
            cpu_ms: 0,
            ..Default::default()
        };
        let now = usage_from(Some(&before), Timestamp(1_001_000), sample(2_000), 2);
        assert_eq!(now.cpu_percent, 200);
        // A clock that jumped backwards, or a counter that ran away, is capped
        // rather than shown as a machine doing the impossible.
        let wild = usage_from(Some(&before), Timestamp(1_000_010), sample(9_000), 1);
        assert_eq!(wild.cpu_percent, 100);
    }

    /// A counter that went backwards is a guest that restarted, not a guest
    /// that used negative time. Inventing a rate there puts a spike on the
    /// graph at exactly the moment somebody is reading it.
    #[test]
    fn a_restarted_counter_reports_no_rate_rather_than_a_spike() {
        let before = GuestUsage {
            at: Timestamp(1_000_000),
            cpu_ms: 900_000,
            ..Default::default()
        };
        let now = usage_from(Some(&before), Timestamp(1_030_000), sample(120), 4);
        assert_eq!(now.cpu_percent, 0);
        assert_eq!(now.cpu_ms, 120);
    }

    /// A tap that was remade starts counting at zero again, and the billable
    /// total must not. Differencing the raw counter would lose everything
    /// between the last reading and the reset — and a bill that quietly loses
    /// traffic is worse than one that has none.
    #[test]
    fn traffic_is_carried_across_a_counter_that_started_again() {
        let before = GuestUsage {
            at: Timestamp(1_000_000),
            rx_bytes: 9_000,
            tx_bytes: 4_000,
            rx_total: 9_000,
            tx_total: 4_000,
            ..Default::default()
        };
        // The guest kept running: the counter grew.
        let grew = usage_from(
            Some(&before),
            Timestamp(1_060_000),
            Sampled {
                rx_bytes: 11_000,
                tx_bytes: 5_500,
                ..Default::default()
            },
            1,
        );
        assert_eq!(grew.rx_total, 11_000);
        assert_eq!(grew.tx_total, 5_500);

        // The tap was remade: the counter is small again, and everything it
        // now reads is traffic that has not been counted before.
        let remade = usage_from(
            Some(&grew),
            Timestamp(1_120_000),
            Sampled {
                rx_bytes: 300,
                tx_bytes: 120,
                ..Default::default()
            },
            1,
        );
        assert_eq!(remade.rx_bytes, 300, "the raw counter is reported as it is");
        assert_eq!(
            remade.rx_total, 11_300,
            "the billable total went backwards over a reset"
        );
        assert_eq!(remade.tx_total, 5_620);
    }

    /// The first reading has nothing to difference against and says so.
    #[test]
    fn the_first_reading_claims_no_rate() {
        let first = usage_from(None, Timestamp(1_000_000), sample(50), 2);
        assert_eq!(first.cpu_percent, 0);
        assert_eq!(first.memory_mib, 512);
        assert_eq!(first.rx_bytes, 10);
        // The tap's own counter is the whole of what this node has seen.
        assert_eq!(first.rx_total, 10);
    }

    /// A stopped guest is using nothing, and the last reading does not linger:
    /// a console showing a stopped machine at forty percent for ever would be
    /// worse than showing nothing.
    #[test]
    fn a_guest_that_is_not_running_reports_no_usage() {
        let mut status = InstanceStatus {
            usage: Some(GuestUsage {
                at: Timestamp(1_000_000),
                cpu_percent: 40,
                ..Default::default()
            }),
            ..Default::default()
        };
        let host = HostState::default();
        observe_instance(
            &mut status,
            &host,
            "projects/p1/instances/i1",
            false,
            false,
            None,
            2,
            0,
        );
        assert_eq!(status.usage, None);
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    use crate::host::VmObservation;

    fn running(host: &mut HostState, name: &str) {
        host.vms.insert(
            name.to_string(),
            VmObservation {
                state: InstanceState::Running,
                pid: Some(1),
                size: None,
                console_tail: String::new(),
                console_bytes: 0,
                devices: Vec::new(),
                started_at: None,
            },
        );
    }

    /// A reading is reported on its own cadence, not on the resync. Every
    /// counter moves on every pass, so a reading per pass is a status write
    /// per guest per pass — three a second on a hundred-guest cell, of
    /// nothing but traffic counters, waking every watcher.
    #[test]
    fn a_fresh_reading_is_not_reported_again_until_the_window_is_up() {
        let name = "projects/p1/instances/i1";
        let mut host = HostState::default();
        running(&mut host, name);

        let mut status = InstanceStatus::default();
        let sample = Sampled {
            cpu_ms: 1_000,
            memory_mib: 400,
            ..Default::default()
        };

        // First pass: nothing to compare against, so it is taken.
        observe_instance(
            &mut status,
            &host,
            name,
            false,
            false,
            Some(sample),
            2,
            300_000,
        );
        let first = status.usage.expect("the first reading");
        assert_eq!(first.cpu_ms, 1_000);

        // Second pass, moments later: the same reading stands. Not a new one
        // with the same numbers — the *same* one, so the status has not
        // changed and nothing is written.
        let later = Sampled {
            cpu_ms: 1_600,
            ..sample
        };
        observe_instance(
            &mut status,
            &host,
            name,
            false,
            false,
            Some(later),
            2,
            300_000,
        );
        assert_eq!(
            status.usage,
            Some(first),
            "a reading was taken inside its own window"
        );
    }

    /// And with the window at nothing, every pass reports — which is what a
    /// test of the arithmetic wants and what production must not do.
    #[test]
    fn a_window_of_nothing_reports_every_pass() {
        let name = "projects/p1/instances/i1";
        let mut host = HostState::default();
        running(&mut host, name);
        let mut status = InstanceStatus::default();
        observe_instance(
            &mut status,
            &host,
            name,
            false,
            false,
            Some(Sampled {
                cpu_ms: 10,
                ..Default::default()
            }),
            2,
            0,
        );
        observe_instance(
            &mut status,
            &host,
            name,
            false,
            false,
            Some(Sampled {
                cpu_ms: 20,
                ..Default::default()
            }),
            2,
            0,
        );
        assert_eq!(status.usage.expect("a reading").cpu_ms, 20);
    }
}
