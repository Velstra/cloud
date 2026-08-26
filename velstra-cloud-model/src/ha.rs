//! Bringing a guest back after its node stops answering.
//!
//! ## The only thing that makes this safe
//!
//! A node that has gone quiet is not a node that has stopped. It may be
//! unreachable and still running every guest it holds — a switch failed, a
//! kernel is paging, the control plane's own network is down. Starting those
//! guests somewhere else then produces two of each, both writing to the same
//! shared volume, and the filesystem does not survive that. It is the failure
//! that turns an outage into a restore from backup.
//!
//! So recovery here rests on one mechanism and nothing else: **the agent stops
//! its own guests before anybody else may start them.** Each node is given a
//! deadline; if it has not managed to report for that long, it stops everything
//! it holds, whatever it thinks about why. The control plane then waits
//! *longer* than that deadline before re-placing anything, so by the time it
//! acts the guests are stopped or the machine is gone.
//!
//! That inference is a promise about somebody's data, so its three conditions
//! are stated rather than assumed:
//!
//! 1. The agent really does self-fence. Built, and tested by killing its
//!    connection rather than by asking it nicely.
//! 2. The two clocks agree to within the margin. They are compared directly,
//!    so a node whose clock is minutes off is a node whose deadline is wrong.
//! 3. The margin covers skew and scheduling delay. It is deliberately generous:
//!    the cost of waiting is downtime, and the cost of being early is a
//!    corrupted volume.
//!
//! **A node with no deadline set is never recovered from.** Nothing guarantees
//! its guests have stopped, so nothing may be started in their place — see
//! [`NotRecoverable::NodeDoesNotFence`]. That is the honest default and it is
//! why this is opt-in rather than on.
//!
//! ## Why this is not the scheduler re-placing things
//!
//! The scheduler never re-places a guest: `needs_placement` asks whether
//! `spec.node` is empty, and a placed guest's is not. Recovery does not break
//! that rule, it *uses* it — the recovery controller clears `spec.node`, and
//! the scheduler then does what it always does with an unplaced guest. One
//! deliberate act, by one controller, that turns into ordinary placement.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// How long a node may fail to report before it stops its own guests.
///
/// Zero means it does not fence, and a node that does not fence is never
/// recovered from. Not a default anybody should stumble into, which is why the
/// refusal names it.
pub const DEFAULT_FENCE_AFTER_S: u32 = 60;

/// How much longer than the node's own deadline the control plane waits.
///
/// One deadline again, doubled. The agent's clock, the control plane's clock,
/// the moment a status write actually lands, and the resync interval all move
/// around inside this, and every one of them moving the wrong way at once is
/// still covered. Waiting costs downtime; being early costs a volume.
pub const RECOVERY_MARGIN_S: u32 = 60;

/// What an instance's operator wants done if its node stops answering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnNodeLoss {
    /// Nothing. The guest stays where it is and comes back when its node does.
    ///
    /// The default, and not out of caution alone: a guest whose disk is local
    /// to that machine has nothing to come back *to* elsewhere, and starting an
    /// empty copy of it under the same name would be worse than leaving it
    /// down.
    #[default]
    Leave,
    /// Start it on another node.
    ///
    /// Only for a guest whose storage every node can reach. A guest on local
    /// storage that is "recovered" elsewhere is a new, empty machine wearing a
    /// familiar name — which is how somebody restores from backup on top of
    /// data that was never lost.
    Restart,
}

/// Why a guest cannot be brought back.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotRecoverable {
    /// Its operator did not ask for this.
    #[error("its policy is to leave it where it is")]
    PolicyIsLeave,
    /// The node has not been quiet long enough to be sure.
    #[error("{node} was last heard from {quiet_s}s ago; {need_s}s is when its guests are certainly stopped")]
    NotQuietLongEnough {
        node: String,
        quiet_s: u64,
        need_s: u64,
    },
    /// The node does not stop its own guests, so nothing can know they stopped.
    ///
    /// The refusal that keeps this feature honest. Without self-fencing, "the
    /// node is unreachable" and "the node is stopped" are different statements,
    /// and acting on the first as though it were the second is what produces
    /// two guests writing to one volume.
    #[error(
        "{node} does not stop its own guests when it loses contact, so nothing can be sure they \
         are stopped — set a fencing deadline on it, or move this guest by hand once you have \
         checked the machine"
    )]
    NodeDoesNotFence { node: String },
    /// It holds hardware that only exists on that machine.
    #[error("it holds {devices}, which exist only on {node}")]
    HoldsDevices { node: String, devices: String },
    /// It is not running, so there is nothing to bring back.
    #[error("it is not running")]
    NotRunning,
}

/// One node, as this decision sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub name: String,
    /// When it last managed to report.
    pub last_heartbeat: Timestamp,
    /// How long it may go without reporting before it stops its own guests.
    /// Zero means it does not.
    pub fence_after_s: u32,
    /// Whether it currently says it is ready. A node that is reporting and
    /// says it is *not* ready is a different problem — a drained node, a node
    /// with a broken datapath — and is not what this is for.
    pub ready: bool,
}

/// One guest, as this decision sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestView {
    pub name: String,
    pub on_node_loss: OnNodeLoss,
    /// Whether the last thing anybody heard was that it was running.
    pub was_running: bool,
    /// PCI addresses it holds, which exist only on the machine it is on.
    pub devices: Vec<String>,
    pub deleting: bool,
}

/// Whether this guest may be brought back somewhere else.
///
/// Every refusal is a different action for whoever reads it, which is why they
/// are separate: "wait", "change the policy", "set up fencing", and "this one
/// can never move" are four different afternoons.
pub fn may_recover(
    guest: &GuestView,
    node: &NodeView,
    now: Timestamp,
    margin_s: u32,
) -> Result<(), NotRecoverable> {
    if guest.deleting || !guest.was_running {
        return Err(NotRecoverable::NotRunning);
    }
    if guest.on_node_loss != OnNodeLoss::Restart {
        return Err(NotRecoverable::PolicyIsLeave);
    }
    if !guest.devices.is_empty() {
        return Err(NotRecoverable::HoldsDevices {
            node: node.name.clone(),
            devices: guest.devices.join(", "),
        });
    }
    if node.fence_after_s == 0 {
        return Err(NotRecoverable::NodeDoesNotFence {
            node: node.name.clone(),
        });
    }
    // Saturating: a heartbeat from the future is a clock that disagrees with
    // itself, and it reads as "heard from just now" — which delays recovery
    // rather than hurrying it. That is the safe direction.
    let quiet_ms = now.0.saturating_sub(node.last_heartbeat.0);
    let need_ms = u64::from(node.fence_after_s + margin_s) * 1000;
    if quiet_ms < need_ms {
        return Err(NotRecoverable::NotQuietLongEnough {
            node: node.name.clone(),
            quiet_s: quiet_ms / 1000,
            need_s: need_ms / 1000,
        });
    }
    Ok(())
}

/// Whether a node has been quiet long enough that its guests are certainly
/// stopped.
///
/// The same arithmetic [`may_recover`] uses, on its own so a console can say
/// "node-b is being recovered from" without having to ask about a guest.
pub fn is_fenced(node: &NodeView, now: Timestamp, margin_s: u32) -> bool {
    node.fence_after_s > 0
        && now.0.saturating_sub(node.last_heartbeat.0)
            >= u64::from(node.fence_after_s + margin_s) * 1000
}

/// Whether *this* agent should stop the guests it holds.
///
/// Run on the node, against its own clock and its own last successful report.
/// It asks nothing of anybody: a node that cannot reach the control plane
/// cannot be told to fence, which is exactly the situation this is for.
///
/// The deadline is the node's own, without the margin — the margin belongs to
/// the control plane, which must wait *longer* than this so that by the time it
/// acts, this has already happened.
pub fn should_self_fence(
    fence_after_s: u32,
    last_report: Timestamp,
    now: Timestamp,
) -> bool {
    fence_after_s > 0
        && now.0.saturating_sub(last_report.0) >= u64::from(fence_after_s) * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    /// A node whose last report was at [`HEARD`]. How long ago that was is
    /// `now_after`'s business — one moving part per check, rather than two
    /// that have to be kept in step.
    const HEARD: u64 = 1_000_000;

    fn node(fence_after_s: u32) -> NodeView {
        NodeView {
            name: "nodes/node-b".into(),
            last_heartbeat: Timestamp(HEARD),
            fence_after_s,
            ready: false,
        }
    }

    /// The moment `quiet_s` seconds after that node was last heard from.
    fn now_after(quiet_s: u64) -> Timestamp {
        Timestamp(HEARD + quiet_s * S)
    }

    fn guest() -> GuestView {
        GuestView {
            name: "projects/p1/instances/i1".into(),
            on_node_loss: OnNodeLoss::Restart,
            was_running: true,
            devices: Vec::new(),
            deleting: false,
        }
    }

    /// The whole safety argument, as arithmetic: the control plane waits longer
    /// than the node's own deadline.
    #[test]
    fn the_control_plane_waits_longer_than_the_node_stops_itself() {
        let n = node(60);

        // The node has stopped its guests by 60s. The control plane has not
        // acted yet, and that gap is the margin.
        assert!(should_self_fence(60, n.last_heartbeat, now_after(60)));
        assert!(matches!(
            may_recover(&guest(), &n, now_after(60), 60),
            Err(NotRecoverable::NotQuietLongEnough { .. })
        ));

        // At 120s both agree.
        assert!(should_self_fence(60, n.last_heartbeat, now_after(120)));
        assert_eq!(may_recover(&guest(), &n, now_after(120), 60), Ok(()));
    }

    /// A node that does not fence is never recovered from, however long it has
    /// been quiet.
    ///
    /// The refusal that keeps the feature honest. "Unreachable" and "stopped"
    /// are different statements, and treating the first as the second is how
    /// two guests come to write to one volume.
    #[test]
    fn a_node_that_does_not_fence_is_never_recovered_from() {
        let n = node(0);
        let Err(NotRecoverable::NodeDoesNotFence { .. }) =
            may_recover(&guest(), &n, now_after(86_400), 60)
        else {
            panic!("a guest was recovered from a node that never stops its own");
        };
        assert!(!is_fenced(&n, now_after(86_400), 60));

        // And the sentence offers the two things that do work.
        let said = NotRecoverable::NodeDoesNotFence {
            node: "nodes/node-b".into(),
        }
        .to_string();
        assert!(said.contains("fencing deadline"), "{said}");
        assert!(said.contains("by hand"), "{said}");
    }

    #[test]
    fn a_guest_whose_policy_is_leave_stays_where_it_is() {
        let mut g = guest();
        g.on_node_loss = OnNodeLoss::Leave;
        assert_eq!(
            may_recover(&g, &node(60), now_after(600), 60),
            Err(NotRecoverable::PolicyIsLeave)
        );
    }

    /// A guest holding hardware cannot be recovered, and the refusal says what
    /// it holds.
    ///
    /// The device exists on that machine and nowhere else. Starting the guest
    /// elsewhere would produce one that is missing the accelerator it was built
    /// around, which is worse than one that is down and says so.
    #[test]
    fn a_guest_holding_hardware_is_not_recovered_elsewhere() {
        let mut g = guest();
        g.devices = vec!["0000:41:00.0".into()];
        let Err(NotRecoverable::HoldsDevices { devices, .. }) =
            may_recover(&g, &node(60), now_after(600), 60)
        else {
            panic!("a guest holding a PCI device was recovered onto another machine");
        };
        assert_eq!(devices, "0000:41:00.0");
    }

    #[test]
    fn a_guest_that_was_not_running_has_nothing_to_bring_back() {
        let mut g = guest();
        g.was_running = false;
        assert_eq!(
            may_recover(&g, &node(60), now_after(600), 60),
            Err(NotRecoverable::NotRunning)
        );

        let mut going = guest();
        going.deleting = true;
        assert_eq!(
            may_recover(&going, &node(60), now_after(600), 60),
            Err(NotRecoverable::NotRunning)
        );
    }

    /// A heartbeat from the future delays recovery rather than hurrying it.
    #[test]
    fn a_clock_that_disagrees_with_itself_delays_rather_than_hurries() {
        let mut ahead = node(60);
        ahead.last_heartbeat = Timestamp(2_000_000);
        assert!(matches!(
            may_recover(&guest(), &ahead, now_after(600), 60),
            Err(NotRecoverable::NotQuietLongEnough { .. })
        ));
        assert!(!is_fenced(&ahead, now_after(600), 60));
    }

    /// An agent that has just reported does not fence itself.
    #[test]
    fn an_agent_that_is_being_heard_keeps_its_guests() {
        assert!(!should_self_fence(60, Timestamp(HEARD), now_after(0)));
        assert!(!should_self_fence(60, Timestamp(HEARD), now_after(59)));
        assert!(should_self_fence(60, Timestamp(HEARD), now_after(60)));
        // Fencing off means never.
        assert!(!should_self_fence(0, Timestamp(HEARD), now_after(99_999)));
    }

    /// The refusal says how long is left, because "not yet" without a number
    /// is a thing somebody refreshes rather than waits for.
    #[test]
    fn waiting_says_how_long_is_left() {
        let Err(NotRecoverable::NotQuietLongEnough {
            quiet_s, need_s, ..
        }) = may_recover(&guest(), &node(60), now_after(30), 60)
        else {
            panic!("a guest was recovered thirty seconds in");
        };
        assert_eq!(quiet_s, 30);
        assert_eq!(need_s, 120);
    }
}
