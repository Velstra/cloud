//! Moving a cell to a release, one machine at a time.
//!
//! A rollout names a release and which machines, and a controller walks them:
//! cordon, drain if asked, hand the machine what it should run, wait until it
//! is back on the new build, put it back in service, next. At most
//! `max_unavailable` machines are away at once. The control plane goes last,
//! because the controller runs there and its own reboot ends the pass — the
//! state is on this object, so the next controller carries on where it was.
//!
//! **Fail closed.** A machine that does not come back, or that reports its
//! apply failed, stops the rollout with that machine named and every machine
//! not yet started left as it was. A cell half on one build and half on
//! another, with a reason on the object, beats a cell that kept going.
//!
//! **Refuse by name.** A machine the cell cannot update — the NixOS module on
//! somebody's own operating system, or one that has not reported how it was
//! installed — is a row on this status that says so, not a row that is
//! silently missing. The rollout still finishes for the rest.
//!
//! Everything decided is decided in [`plan`], which is pure: it takes the
//! rollout, the release and what the fleet reports, and answers with the
//! status to write and the writes to make on nodes. The controller reads and
//! writes; it does not think.

use serde::{Deserialize, Serialize};

use crate::{
    installed::Installed,
    meta::{Condition, Timestamp},
    release::{ReleaseStatus, Wanted},
};

/// One machine away at a time, unless told otherwise.
pub const DEFAULT_MAX_UNAVAILABLE: u32 = 1;

/// How long a machine has to come back on the new build before the rollout
/// stops on it: a fetch of the image, a write of the slot and two boots on
/// slow hardware fit in it, and a machine not back after it is one somebody
/// has to look at.
pub const APPLY_BUDGET_MS: u64 = 45 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RolloutSpec {
    /// What to move to: `releases/v0.2.0`.
    pub release: String,
    /// Which machines, by id. Empty means every node in the cell.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// Move every guest off a machine before it is updated. Without this a
    /// machine's guests go down with its reboot, which is the right choice
    /// for a lab and the wrong one for anything else.
    #[serde(default)]
    pub evacuate: bool,
    /// How many machines may be out of service at once.
    #[serde(default = "one")]
    pub max_unavailable: u32,
    /// Finish the machine in flight and start no other. A field rather than a
    /// verb so it survives the controller's own restart, which a rollout of
    /// the control plane causes.
    #[serde(default)]
    pub paused: bool,
}

fn one() -> u32 {
    DEFAULT_MAX_UNAVAILABLE
}

impl Default for RolloutSpec {
    fn default() -> Self {
        Self {
            release: String::new(),
            nodes: Vec::new(),
            evacuate: false,
            max_unavailable: DEFAULT_MAX_UNAVAILABLE,
            paused: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolloutPhase {
    /// Waiting for the release to be ready, or for a first look.
    #[default]
    Planning,
    Running,
    Paused,
    Done,
    /// Stopped on a machine that did not make it; named on the status.
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodePhase {
    #[default]
    Pending,
    /// The cell cannot update this machine, and says why.
    Refused,
    /// Taking no new guests.
    Cordoned,
    /// Its guests are moving away.
    Draining,
    /// Told what to run; fetching, verifying, applying, rebooting.
    Applying,
    Done,
    Failed,
}

impl NodePhase {
    pub fn settled(self) -> bool {
        matches!(self, Self::Refused | Self::Done | Self::Failed)
    }

    pub fn in_flight(self) -> bool {
        matches!(self, Self::Cordoned | Self::Draining | Self::Applying)
    }
}

/// One machine's row on the rollout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeProgress {
    pub node: String,
    /// The build it ran when the rollout looked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    /// The build it is moving to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    #[serde(default)]
    pub phase: NodePhase,
    /// What is happening, or why it stopped.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// When this phase began.
    #[serde(default)]
    pub since: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RolloutStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub phase: RolloutPhase,
    #[serde(default)]
    pub nodes: Vec<NodeProgress>,
    /// The rollout in a sentence: how far, and what it is waiting on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "unset")]
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "unset")]
    pub finished_at: Timestamp,
}

fn unset(at: &Timestamp) -> bool {
    at.0 == 0
}

/// What the planner knows about one machine: its own report, as the node
/// object carries it, and how many guests are running on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub node: String,
    pub installed: Installed,
    pub schedulable: bool,
    pub evacuating: bool,
    pub wanted: Option<Wanted>,
    /// The `Ready` condition is True: the agent is running and answering.
    pub ready: bool,
    pub last_heartbeat: Timestamp,
    /// What the node's `Updating` condition says while it works — shown on the
    /// row so an operator sees "fetching" rather than a phase name.
    pub updating: String,
    /// The node reported its apply failed, in these words.
    pub update_failed: Option<String>,
    /// Guests running here: what a drain waits for.
    pub running_guests: u32,
    /// This machine is the control plane the rollout itself runs on.
    pub control_plane: bool,
}

/// A write the controller makes on a node, decided here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `spec.schedulable = false`.
    Cordon(String),
    /// `spec.evacuate = true`.
    Evacuate(String),
    /// `spec.wanted = …`.
    Want(String, Wanted),
    /// Back in service: schedulable, not evacuating, wanting nothing.
    Release(String),
}

/// What one pass decided.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub status: RolloutStatus,
    pub actions: Vec<Action>,
}

/// The whole decision, from facts.
///
/// `release` is the release the spec names, with its name, or `None` when no
/// such object exists. `fleet` is every node the controller could read.
pub fn plan(
    spec: &RolloutSpec,
    status: &RolloutStatus,
    release: Option<(&str, &ReleaseStatus)>,
    fleet: &[Seen],
    now: Timestamp,
) -> Plan {
    let mut next = status.clone();
    let mut actions = Vec::new();

    // Nothing moves before the release is on this cell and verified. A
    // rollout that started fetching on the first node would be a rollout
    // whose failure looked like a node's.
    let (release_name, release) = match release {
        Some(found) => found,
        None => {
            next.phase = RolloutPhase::Planning;
            next.message = format!("{} does not exist", spec.release);
            return Plan {
                status: next,
                actions,
            };
        }
    };
    if !release.is_ready() {
        next.phase = RolloutPhase::Planning;
        next.message = format!(
            "{} is not ready: {}",
            release_name,
            release.not_ready_because()
        );
        return Plan {
            status: next,
            actions,
        };
    }
    let to = release.version.clone();

    // Which machines, in what order: as named, or every node by name — and the
    // control plane last either way.
    let mut names: Vec<String> = if spec.nodes.is_empty() {
        let mut all: Vec<String> = fleet.iter().map(|s| s.node.clone()).collect();
        all.sort();
        all
    } else {
        spec.nodes.clone()
    };
    names.dedup();
    let is_control_plane = |name: &str| fleet.iter().any(|s| s.node == name && s.control_plane);
    let (mut ordered, last): (Vec<String>, Vec<String>) =
        names.into_iter().partition(|n| !is_control_plane(n));
    ordered.extend(last);

    // Every row that should exist, keeping what earlier passes wrote on it.
    let mut rows: Vec<NodeProgress> = ordered
        .iter()
        .map(|name| {
            status
                .nodes
                .iter()
                .find(|r| &r.node == name)
                .cloned()
                .unwrap_or_else(|| NodeProgress {
                    node: name.clone(),
                    to: to.clone(),
                    since: now,
                    ..Default::default()
                })
        })
        .collect();

    // Move each unsettled row on from what its machine reports.
    for row in rows.iter_mut() {
        if row.phase.settled() {
            continue;
        }
        let Some(seen) = fleet.iter().find(|s| s.node == row.node) else {
            settle(row, NodePhase::Refused, "no such node", now);
            continue;
        };
        if row.from.is_empty() {
            row.from = seen.installed.version.clone();
        }
        match row.phase {
            NodePhase::Pending => {
                if seen.installed.runs(&to) {
                    settle(row, NodePhase::Done, &format!("already on {to}"), now);
                } else if let Some(why) = seen.installed.rollout_refusal() {
                    settle(row, NodePhase::Refused, &why, now);
                } else if Wanted::for_node(release_name, release, seen.installed.kind).is_none() {
                    settle(
                        row,
                        NodePhase::Refused,
                        &format!("{release_name} has no artefact for a machine installed this way"),
                        now,
                    );
                }
            }
            NodePhase::Cordoned | NodePhase::Draining => {
                if spec.evacuate && seen.running_guests > 0 {
                    if !seen.evacuating {
                        actions.push(Action::Evacuate(row.node.clone()));
                    }
                    let n = seen.running_guests;
                    let message = format!(
                        "{n} guest{} still here, moving off",
                        if n == 1 { "" } else { "s" }
                    );
                    if row.phase != NodePhase::Draining {
                        move_to(row, NodePhase::Draining, &message, now);
                    } else {
                        row.message = message;
                    }
                } else {
                    // Handed what to run. `for_node` answered above for every
                    // row that got this far.
                    if let Some(wanted) =
                        Wanted::for_node(release_name, release, seen.installed.kind)
                    {
                        actions.push(Action::Want(row.node.clone(), wanted));
                    }
                    move_to(row, NodePhase::Applying, "told what to run", now);
                }
            }
            NodePhase::Applying => {
                let back =
                    seen.installed.runs(&to) && seen.ready && seen.last_heartbeat > row.since;
                if back {
                    actions.push(Action::Release(row.node.clone()));
                    settle(row, NodePhase::Done, &format!("on {to}"), now);
                } else if let Some(why) = &seen.update_failed {
                    settle(row, NodePhase::Failed, why, now);
                } else if now.0.saturating_sub(row.since.0) > APPLY_BUDGET_MS {
                    settle(
                        row,
                        NodePhase::Failed,
                        &format!(
                            "not back on {to} within {} minutes; last reported {}",
                            APPLY_BUDGET_MS / 60_000,
                            if seen.installed.version.is_empty() {
                                "no build".to_string()
                            } else {
                                seen.installed.version.clone()
                            }
                        ),
                        now,
                    );
                } else if !seen.updating.is_empty() {
                    row.message = seen.updating.clone();
                }
            }
            NodePhase::Refused | NodePhase::Done | NodePhase::Failed => {}
        }
    }

    let failed = rows.iter().any(|r| r.phase == NodePhase::Failed);
    let in_flight = rows.iter().filter(|r| r.phase.in_flight()).count() as u32;

    // Start the next machines, unless something says not to: a failure stops
    // everything that has not begun, and a pause finishes what is in flight
    // and starts nothing.
    if !failed && !spec.paused {
        let room = spec.max_unavailable.max(1).saturating_sub(in_flight);
        let mut started = 0;
        for row in rows.iter_mut() {
            if started >= room {
                break;
            }
            if row.phase != NodePhase::Pending {
                continue;
            }
            actions.push(Action::Cordon(row.node.clone()));
            let mut message = "taking no new guests".to_string();
            if spec.evacuate {
                actions.push(Action::Evacuate(row.node.clone()));
                message = "taking no new guests; guests moving off".to_string();
            }
            move_to(row, NodePhase::Cordoned, &message, now);
            started += 1;
        }
    }

    let done = rows.iter().filter(|r| r.phase == NodePhase::Done).count();
    let refused = rows
        .iter()
        .filter(|r| r.phase == NodePhase::Refused)
        .count();
    let all_settled = rows.iter().all(|r| r.phase.settled());
    if next.started_at.0 == 0 {
        next.started_at = now;
    }
    next.phase = if all_settled {
        if next.finished_at.0 == 0 {
            next.finished_at = now;
        }
        if failed {
            RolloutPhase::Failed
        } else {
            RolloutPhase::Done
        }
    } else if failed {
        RolloutPhase::Failed
    } else if spec.paused {
        RolloutPhase::Paused
    } else {
        RolloutPhase::Running
    };
    next.message = summary(&rows, done, refused, &to, next.phase);
    next.nodes = rows;
    Plan {
        status: next,
        actions,
    }
}

fn move_to(row: &mut NodeProgress, phase: NodePhase, message: &str, now: Timestamp) {
    row.phase = phase;
    row.message = message.to_string();
    row.since = now;
}

fn settle(row: &mut NodeProgress, phase: NodePhase, message: &str, now: Timestamp) {
    move_to(row, phase, message, now);
}

/// The rollout in a sentence.
fn summary(
    rows: &[NodeProgress],
    done: usize,
    refused: usize,
    to: &str,
    phase: RolloutPhase,
) -> String {
    let total = rows.len();
    let mut said = format!("{done} of {total} on {to}");
    if refused > 0 {
        let names: Vec<&str> = rows
            .iter()
            .filter(|r| r.phase == NodePhase::Refused)
            .map(|r| r.node.as_str())
            .collect();
        said.push_str(&format!("; refused: {}", names.join(", ")));
    }
    match phase {
        RolloutPhase::Failed => {
            let names: Vec<String> = rows
                .iter()
                .filter(|r| r.phase == NodePhase::Failed)
                .map(|r| format!("{}: {}", r.node, r.message))
                .collect();
            said.push_str(&format!("; stopped on {}", names.join("; ")));
        }
        RolloutPhase::Paused => said.push_str("; paused"),
        RolloutPhase::Running => {
            let busy: Vec<String> = rows
                .iter()
                .filter(|r| r.phase.in_flight())
                .map(|r| format!("{} {}", r.node, r.message))
                .collect();
            if !busy.is_empty() {
                said.push_str(&format!("; {}", busy.join("; ")));
            }
        }
        RolloutPhase::Planning | RolloutPhase::Done => {}
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        installed::InstallKind,
        meta::ConditionStatus,
        release::{Artefact, READY},
    };

    const TO: &str = "0.2.0+20260918.c571d71";
    const FROM: &str = "0.1.0+20260910.681269c";
    const RELEASE: &str = "releases/v0.2.0";

    fn release() -> ReleaseStatus {
        ReleaseStatus {
            version: TO.into(),
            image: Some(Artefact {
                file: "velstra-cloud-node_0.2.0_amd64.raw.zst".into(),
                sha256: "ab".repeat(32),
                fetched: true,
            }),
            package: Some(Artefact {
                file: "velstra-cloud_0.2.0_amd64.deb".into(),
                sha256: "cd".repeat(32),
                fetched: true,
            }),
            conditions: vec![Condition::ready(1)],
            ..Default::default()
        }
    }

    fn node(name: &str, kind: InstallKind, version: &str) -> Seen {
        Seen {
            node: name.into(),
            installed: Installed {
                kind,
                version: version.into(),
                ..Default::default()
            },
            schedulable: true,
            ready: true,
            last_heartbeat: Timestamp(1_000),
            ..Default::default()
        }
    }

    /// Two hypervisors and the control plane, all a build behind.
    fn fleet() -> Vec<Seen> {
        vec![
            Seen {
                control_plane: true,
                ..node("horst", InstallKind::Appliance, FROM)
            },
            node("peter", InstallKind::Appliance, FROM),
            node("paul", InstallKind::Package, FROM),
        ]
    }

    fn spec() -> RolloutSpec {
        RolloutSpec {
            release: RELEASE.into(),
            ..Default::default()
        }
    }

    fn row<'a>(out: &'a Plan, node: &str) -> &'a NodeProgress {
        out.status
            .nodes
            .iter()
            .find(|r| r.node == node)
            .unwrap_or_else(|| panic!("no row for {node}: {:?}", out.status.nodes))
    }

    /// The first pass cordons one machine — not the control plane — and
    /// touches nothing else.
    #[test]
    fn one_machine_at_a_time_and_the_control_plane_last() {
        let out = plan(
            &spec(),
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(out.status.phase, RolloutPhase::Running);
        assert_eq!(out.actions, vec![Action::Cordon("paul".into())]);
        assert_eq!(row(&out, "paul").phase, NodePhase::Cordoned);
        assert_eq!(row(&out, "peter").phase, NodePhase::Pending);
        assert_eq!(row(&out, "horst").phase, NodePhase::Pending);
        let order: Vec<&str> = out.status.nodes.iter().map(|r| r.node.as_str()).collect();
        assert_eq!(
            order,
            ["paul", "peter", "horst"],
            "alphabetical, control plane last"
        );
        assert_eq!(out.status.started_at, Timestamp(10_000));
        assert!(
            out.status.message.starts_with("0 of 3 on "),
            "{}",
            out.status.message
        );
    }

    /// Cordoned and nothing to drain: handed what to run, as the artefact for
    /// its own kind.
    #[test]
    fn a_cordoned_machine_is_handed_the_artefact_for_its_kind() {
        let first = plan(
            &spec(),
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(10_000),
        );
        let mut fleet = fleet();
        fleet[2].schedulable = false;
        let second = plan(
            &spec(),
            &first.status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(11_000),
        );
        let wanted = match &second.actions[..] {
            [Action::Want(node, wanted)] => {
                assert_eq!(node, "paul");
                wanted
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            wanted.file, "velstra-cloud_0.2.0_amd64.deb",
            "a package node gets the deb"
        );
        assert_eq!(wanted.version, TO);
        assert_eq!(row(&second, "paul").phase, NodePhase::Applying);
    }

    /// A drain waits for the guests, then hands over.
    #[test]
    fn a_drain_waits_for_the_guests_to_leave() {
        let spec = RolloutSpec {
            evacuate: true,
            ..spec()
        };
        let first = plan(
            &spec,
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(
            first.actions,
            vec![
                Action::Cordon("paul".into()),
                Action::Evacuate("paul".into())
            ]
        );
        let mut fleet = fleet();
        fleet[2].schedulable = false;
        fleet[2].evacuating = true;
        fleet[2].running_guests = 2;
        let second = plan(
            &spec,
            &first.status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(11_000),
        );
        assert!(second.actions.is_empty(), "{:?}", second.actions);
        assert_eq!(row(&second, "paul").phase, NodePhase::Draining);
        assert!(
            row(&second, "paul").message.contains("2 guests"),
            "{}",
            row(&second, "paul").message
        );
        fleet[2].running_guests = 0;
        let third = plan(
            &spec,
            &second.status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(12_000),
        );
        assert!(
            matches!(&third.actions[..], [Action::Want(n, _)] if n == "paul"),
            "{:?}",
            third.actions
        );
        assert_eq!(row(&third, "paul").phase, NodePhase::Applying);
    }

    /// Back on the new build, ready, and heard from since: released, and the
    /// next machine starts in the same pass.
    #[test]
    fn a_machine_back_on_the_build_is_released_and_the_next_starts() {
        let status = RolloutStatus {
            nodes: vec![NodeProgress {
                node: "paul".into(),
                from: FROM.into(),
                to: TO.into(),
                phase: NodePhase::Applying,
                since: Timestamp(10_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut fleet = fleet();
        fleet[2].installed.version = TO.into();
        fleet[2].last_heartbeat = Timestamp(20_000);
        fleet[2].schedulable = false;
        let out = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(21_000),
        );
        assert_eq!(
            out.actions,
            vec![
                Action::Release("paul".into()),
                Action::Cordon("peter".into())
            ]
        );
        assert_eq!(row(&out, "paul").phase, NodePhase::Done);
        assert_eq!(row(&out, "peter").phase, NodePhase::Cordoned);
    }

    /// A heartbeat from before the apply is not "back": the old agent was
    /// still answering when the row was written.
    #[test]
    fn an_old_heartbeat_does_not_count_as_back() {
        let status = RolloutStatus {
            nodes: vec![NodeProgress {
                node: "paul".into(),
                to: TO.into(),
                phase: NodePhase::Applying,
                since: Timestamp(10_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut fleet = fleet();
        fleet[2].installed.version = TO.into();
        fleet[2].last_heartbeat = Timestamp(9_000);
        let out = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(11_000),
        );
        assert_eq!(row(&out, "paul").phase, NodePhase::Applying);
        assert!(out.actions.is_empty(), "{:?}", out.actions);
    }

    /// A machine that reports its apply failed stops the rollout by name, and
    /// nothing not yet started is started.
    #[test]
    fn a_failed_apply_stops_the_rollout_with_the_machine_named() {
        let status = RolloutStatus {
            nodes: vec![NodeProgress {
                node: "paul".into(),
                to: TO.into(),
                phase: NodePhase::Applying,
                since: Timestamp(10_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut fleet = fleet();
        fleet[2].update_failed = Some("the digest did not match".into());
        let out = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(11_000),
        );
        assert_eq!(out.status.phase, RolloutPhase::Failed);
        assert_eq!(row(&out, "paul").phase, NodePhase::Failed);
        assert!(
            out.actions.is_empty(),
            "nothing new starts: {:?}",
            out.actions
        );
        assert!(
            out.status
                .message
                .contains("paul: the digest did not match"),
            "{}",
            out.status.message
        );
        assert_eq!(row(&out, "peter").phase, NodePhase::Pending);
    }

    /// A machine that never comes back is a failure after its budget, not a
    /// rollout that waits for ever.
    #[test]
    fn a_machine_that_does_not_come_back_fails_after_its_budget() {
        let status = RolloutStatus {
            nodes: vec![NodeProgress {
                node: "paul".into(),
                to: TO.into(),
                phase: NodePhase::Applying,
                since: Timestamp(10_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fleet = fleet();
        let early = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(10_000 + APPLY_BUDGET_MS),
        );
        assert_eq!(row(&early, "paul").phase, NodePhase::Applying);
        let late = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(10_001 + APPLY_BUDGET_MS),
        );
        assert_eq!(row(&late, "paul").phase, NodePhase::Failed);
        assert!(
            row(&late, "paul").message.contains("45 minutes"),
            "{}",
            row(&late, "paul").message
        );
        assert_eq!(late.status.phase, RolloutPhase::Failed);
    }

    /// The kinds the cell cannot update are refused by name, and the rollout
    /// goes on for the rest — and finishes as Done, with the refusal on it.
    #[test]
    fn a_machine_the_cell_cannot_update_is_refused_by_name() {
        let mut fleet = fleet();
        fleet.push(node("nixy", InstallKind::NixOs, FROM));
        fleet.push(node("mute", InstallKind::Unknown, ""));
        let out = plan(
            &spec(),
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(10_000),
        );
        assert_eq!(row(&out, "nixy").phase, NodePhase::Refused);
        assert!(
            row(&out, "nixy").message.contains("own configuration"),
            "{}",
            row(&out, "nixy").message
        );
        assert_eq!(row(&out, "mute").phase, NodePhase::Refused);
        assert!(
            out.status.message.contains("refused: mute, nixy"),
            "{}",
            out.status.message
        );
        // Still running for the rest: one machine cordoned.
        assert_eq!(out.status.phase, RolloutPhase::Running);
        assert_eq!(out.actions.len(), 1);
    }

    /// A machine already on the build is done without being touched, and a
    /// name that is no node is refused rather than waited for.
    #[test]
    fn already_there_is_done_and_nowhere_is_refused() {
        let mut fleet = fleet();
        fleet[1].installed.version = TO.into();
        let spec = RolloutSpec {
            nodes: vec!["peter".into(), "ghost".into()],
            ..spec()
        };
        let out = plan(
            &spec,
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet,
            Timestamp(10_000),
        );
        assert_eq!(row(&out, "peter").phase, NodePhase::Done);
        assert_eq!(row(&out, "ghost").phase, NodePhase::Refused);
        assert_eq!(row(&out, "ghost").message, "no such node");
        assert!(out.actions.is_empty(), "{:?}", out.actions);
        assert_eq!(out.status.phase, RolloutPhase::Done);
        assert_eq!(out.status.finished_at, Timestamp(10_000));
    }

    /// Paused: what is in flight goes on; nothing new starts.
    #[test]
    fn paused_finishes_the_machine_in_flight_and_starts_no_other() {
        let status = RolloutStatus {
            nodes: vec![NodeProgress {
                node: "paul".into(),
                to: TO.into(),
                phase: NodePhase::Cordoned,
                since: Timestamp(10_000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let spec = RolloutSpec {
            paused: true,
            ..spec()
        };
        let out = plan(
            &spec,
            &status,
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(11_000),
        );
        assert_eq!(out.status.phase, RolloutPhase::Paused);
        assert!(
            matches!(&out.actions[..], [Action::Want(n, _)] if n == "paul"),
            "{:?}",
            out.actions
        );
        assert_eq!(row(&out, "peter").phase, NodePhase::Pending);
    }

    /// Two at once when asked, and never the control plane among the first.
    #[test]
    fn max_unavailable_starts_that_many() {
        let spec = RolloutSpec {
            max_unavailable: 2,
            ..spec()
        };
        let out = plan(
            &spec,
            &RolloutStatus::default(),
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(
            out.actions,
            vec![
                Action::Cordon("paul".into()),
                Action::Cordon("peter".into())
            ]
        );
        assert_eq!(row(&out, "horst").phase, NodePhase::Pending);
    }

    /// The control plane is reached only when everything else is settled.
    #[test]
    fn the_control_plane_goes_last() {
        let status = RolloutStatus {
            nodes: vec![
                NodeProgress {
                    node: "paul".into(),
                    to: TO.into(),
                    phase: NodePhase::Done,
                    ..Default::default()
                },
                NodeProgress {
                    node: "peter".into(),
                    to: TO.into(),
                    phase: NodePhase::Done,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let out = plan(
            &spec(),
            &status,
            Some((RELEASE, &release())),
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(out.actions, vec![Action::Cordon("horst".into())]);
    }

    /// Nothing moves before the release is ready, and the status says why.
    #[test]
    fn nothing_moves_before_the_release_is_ready() {
        let mut not_ready = release();
        not_ready.conditions = vec![Condition::new(
            READY,
            ConditionStatus::False,
            "Fetching",
            "fetching the image (2 of 3)",
            1,
        )];
        let out = plan(
            &spec(),
            &RolloutStatus::default(),
            Some((RELEASE, &not_ready)),
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(out.status.phase, RolloutPhase::Planning);
        assert!(
            out.status.message.contains("fetching the image (2 of 3)"),
            "{}",
            out.status.message
        );
        assert!(out.actions.is_empty());
        let out = plan(
            &spec(),
            &RolloutStatus::default(),
            None,
            &fleet(),
            Timestamp(10_000),
        );
        assert_eq!(out.status.phase, RolloutPhase::Planning);
        assert!(
            out.status.message.contains("does not exist"),
            "{}",
            out.status.message
        );
    }
}
