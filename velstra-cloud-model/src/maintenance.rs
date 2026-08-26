//! Taking a machine out of service on purpose, at a time somebody chose.
//!
//! ## Why this is a declaration and not a switch
//!
//! A node already has the two switches this needs: `schedulable: false` says
//! "nothing new here", `evacuate: true` says "and none of the old either". An
//! operator emptying a machine at two in the morning could flip both by hand —
//! and that is exactly the problem. Somebody has to be awake to flip them, and
//! somebody has to remember to flip them back.
//!
//! So a window is a **declaration about a stretch of time**, and the platform
//! derives the behaviour from it. Nothing writes those two switches on the
//! operator's behalf; placement and evacuation ask "is this node inside an open
//! window right now" and act. That keeps one writer per field — the operator
//! owns `schedulable`, and a controller that also wrote it would be the second
//! — and it means a window that ends puts everything back by *ceasing to be
//! open*, with nothing to unwind and nothing left flipped if a controller died
//! in the middle.
//!
//! ## Why `drain` is separate from the window itself
//!
//! The same distinction as the two switches, for the same reason. A firmware
//! update that takes four minutes wants nothing new placed and everything left
//! where it is; pulling a machine out of the rack wants the guests gone first.
//! One field, two intentions, and conflating them would move a fleet for a
//! reboot.
//!
//! ## Which state is stored: none
//!
//! Upcoming, open and past are computed from `starts_at`, `minutes` and the
//! clock. A stored `state` field would be a transient state — a thing that has
//! to be written by somebody at the right moment, and is therefore wrong
//! whenever nobody was there to write it.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// A window longer than this is almost certainly a typo — somebody meaning four
/// hours and typing minutes, or meaning to take a machine out permanently. Two
/// weeks is refused with a sentence rather than accepted as an unschedulable
/// node nobody remembers declaring.
pub const LONGEST_WINDOW_MINUTES: u32 = 14 * 24 * 60;

/// "This node is out of service from then, for that long."
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceWindowSpec {
    /// The node this is about.
    pub node: String,
    /// When it opens. In the past means "now" — somebody declaring a window for
    /// work they have already started is telling the truth, and refusing them
    /// would only teach them to lie about the time.
    pub starts_at: Timestamp,
    /// How long it stays open. There is no end timestamp: two fields that can
    /// disagree about the same fact are one field too many, and a duration is
    /// what people actually say out loud.
    pub minutes: u32,
    /// Whether the guests should leave, or only stay put.
    ///
    /// `false` — nothing new is placed here, everything already here keeps
    /// running. The right answer for a firmware update measured in minutes.
    ///
    /// `true` — and the guests are migrated off as well, by the ordinary
    /// evacuation machinery. The right answer for pulling the machine.
    #[serde(default)]
    pub drain: bool,
    /// What it is for, in the operator's own words. Shown wherever the window
    /// is the reason something was refused, so that "no capacity" reads as
    /// "node-b is out until 03:00 for the memory swap" instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Nothing is written here.
///
/// Whether a window is upcoming, open or past is arithmetic on the clock, and a
/// stored copy of it would be a transient state: correct only for as long as
/// whoever wrote it kept being awake. `observed_generation` and `conditions`
/// are here because every resource has them, not because anything sets them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceWindowStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
}

/// Where a window sits relative to now. Computed, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Declared, not yet begun.
    Upcoming,
    /// In force this instant.
    Open,
    /// Over. Kept because what happened last Tuesday is a question people ask.
    Past,
}

/// Why a window will not be accepted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Zero minutes is a window that is never open — it would be accepted,
    /// listed, and do nothing, which is worse than being told.
    #[error("a window of zero minutes would never be open; give it a length")]
    ZeroLength,
    /// Absurdly long, which is nearly always a unit mix-up.
    #[error(
        "{minutes} minutes is longer than {LONGEST_WINDOW_MINUTES} — if this node is going away \
         for good, take it out of the cell instead of leaving a window nobody remembers"
    )]
    TooLong { minutes: u32 },
    /// Already over before it was declared.
    #[error("that window closed before it was declared; it would never take effect")]
    AlreadyOver,
    /// Another window on the same node covers part of the same time.
    ///
    /// Two overlapping windows are two answers to "may this node take work at
    /// four o'clock", and the operator who declared the second one believes
    /// theirs is the answer.
    #[error(
        "{other} already covers part of that time on {node}; change that one rather than \
         declaring a second"
    )]
    Overlaps { node: String, other: String },
}

/// One window, as these decisions see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowView {
    pub name: String,
    pub node: String,
    pub starts_at: Timestamp,
    pub minutes: u32,
    pub drain: bool,
    pub note: String,
}

impl WindowView {
    pub fn ends_at(&self) -> Timestamp {
        Timestamp(
            self.starts_at
                .0
                .saturating_add(minutes_in_millis(self.minutes)),
        )
    }

    pub fn phase(&self, now: Timestamp) -> Phase {
        if now.0 < self.starts_at.0 {
            Phase::Upcoming
        } else if now.0 < self.ends_at().0 {
            Phase::Open
        } else {
            Phase::Past
        }
    }

    pub fn is_open(&self, now: Timestamp) -> bool {
        self.phase(now) == Phase::Open
    }

    fn covers(&self, other: &WindowView) -> bool {
        self.starts_at.0 < other.ends_at().0 && other.starts_at.0 < self.ends_at().0
    }
}

fn minutes_in_millis(minutes: u32) -> u64 {
    u64::from(minutes).saturating_mul(60_000)
}

/// Whether this window may be declared, given the ones already there.
///
/// Answered before it is stored, because every one of these is knowable then —
/// and a window that turns out to be meaningless is discovered at three in the
/// morning by the person relying on it.
pub fn may_declare(
    asked: &WindowView,
    existing: &[WindowView],
    now: Timestamp,
) -> Result<(), Refusal> {
    if asked.minutes == 0 {
        return Err(Refusal::ZeroLength);
    }
    if asked.minutes > LONGEST_WINDOW_MINUTES {
        return Err(Refusal::TooLong {
            minutes: asked.minutes,
        });
    }
    if asked.ends_at().0 <= now.0 {
        return Err(Refusal::AlreadyOver);
    }
    if let Some(other) = existing
        .iter()
        .filter(|w| w.node == asked.node && w.name != asked.name)
        // A window that is already over cannot conflict with anything: it is
        // kept as a record, not as a claim on the future.
        .filter(|w| w.phase(now) != Phase::Past)
        .find(|w| w.covers(asked))
    {
        return Err(Refusal::Overlaps {
            node: asked.node.clone(),
            other: other.name.clone(),
        });
    }
    Ok(())
}

/// The window in force on this node this instant, if there is one.
///
/// The earliest-opened one wins where two somehow overlap — [`may_declare`]
/// refuses that pair, but a store can hold a pair declared before this rule
/// existed, and answering deterministically beats answering by list order.
pub fn open_on<'a>(
    node: &str,
    windows: &'a [WindowView],
    now: Timestamp,
) -> Option<&'a WindowView> {
    windows
        .iter()
        .filter(|w| w.node == node && w.is_open(now))
        .min_by_key(|w| w.starts_at.0)
}

/// The next window declared for this node, if any is still to come.
pub fn next_on<'a>(
    node: &str,
    windows: &'a [WindowView],
    now: Timestamp,
) -> Option<&'a WindowView> {
    windows
        .iter()
        .filter(|w| w.node == node && w.phase(now) == Phase::Upcoming)
        .min_by_key(|w| w.starts_at.0)
}

/// Every node that is out of service this instant, and until when.
///
/// The shape placement wants: it asks about a node it is already looking at,
/// and the answer has to carry the end time, because "no capacity" and "back at
/// 03:00" are the same fact told two ways and only one of them is any use.
pub fn closed_now(windows: &[WindowView], now: Timestamp) -> Vec<Closed> {
    let mut out: Vec<Closed> = Vec::new();
    for w in windows.iter().filter(|w| w.is_open(now)) {
        // The one that ends last is the one that matters: a node covered by two
        // windows is free when the later of them closes.
        match out.iter_mut().find(|c| c.node == w.node) {
            Some(existing) if existing.until.0 < w.ends_at().0 => {
                existing.until = w.ends_at();
                existing.minutes_left = left(w, now);
                existing.note = w.note.clone();
                existing.window = w.name.clone();
            }
            Some(_) => {}
            None => out.push(Closed {
                node: w.node.clone(),
                until: w.ends_at(),
                minutes_left: left(w, now),
                note: w.note.clone(),
                window: w.name.clone(),
            }),
        }
    }
    out.sort_by(|a, b| a.node.cmp(&b.node));
    out
}

/// A node that is out of service, and the shape of why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub node: String,
    pub until: Timestamp,
    /// How much longer, in whole minutes.
    ///
    /// Alongside the absolute time rather than instead of it, because the two
    /// answers are read by two different readers. A person looking at a board
    /// wants "back at 03:00"; a rejection written into an instance's condition
    /// is read hours later, when "back at 03:00" no longer says whether that
    /// has happened, and "for another 40 minutes" was true when it was written.
    pub minutes_left: u64,
    pub note: String,
    pub window: String,
}

/// Whether the guests on this node should be leaving right now.
///
/// Read by the evacuation controller alongside `spec.evacuate`. The window does
/// not set that field — it stands beside it, so an operator who set it by hand
/// keeps owning it, and a window that closes takes nothing of theirs with it.
pub fn draining(node: &str, windows: &[WindowView], now: Timestamp) -> bool {
    windows
        .iter()
        .any(|w| w.node == node && w.drain && w.is_open(now))
}

/// How much longer an open window has to run, rounded up: a window with twenty
/// seconds left has one minute left, not none. Nothing that reads this wants to
/// hear "0 minutes" about a node it still cannot use.
fn left(window: &WindowView, now: Timestamp) -> u64 {
    window.ends_at().0.saturating_sub(now.0).div_ceil(60_000)
}

/// How long until this window opens, in whole minutes. `None` once it has.
pub fn opens_in_minutes(window: &WindowView, now: Timestamp) -> Option<u64> {
    (window.phase(now) == Phase::Upcoming)
        .then(|| window.starts_at.0.saturating_sub(now.0) / 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    fn at(minutes: u64) -> Timestamp {
        Timestamp(1_000_000_000_000 + minutes * MINUTE)
    }

    fn window(name: &str, node: &str, start: u64, minutes: u32) -> WindowView {
        WindowView {
            name: format!("maintenance-windows/{name}"),
            node: node.into(),
            starts_at: at(start),
            minutes,
            drain: false,
            note: String::new(),
        }
    }

    /// The three phases are arithmetic, and the boundaries land where a person
    /// would say they do: a window is open at its first instant and shut at its
    /// last.
    #[test]
    fn a_window_is_open_from_its_start_until_its_end_and_not_after() {
        let w = window("swap", "node-a", 60, 30);
        assert_eq!(w.phase(at(0)), Phase::Upcoming);
        assert_eq!(w.phase(at(59)), Phase::Upcoming);
        assert_eq!(w.phase(at(60)), Phase::Open, "it was not open at its start");
        assert_eq!(w.phase(at(89)), Phase::Open);
        assert_eq!(w.phase(at(90)), Phase::Past, "it was still open at its end");
        assert_eq!(w.ends_at(), at(90));
    }

    /// The refusal the length rules exist for: a window that could never do
    /// anything, said so at the time rather than discovered later.
    #[test]
    fn a_window_that_could_never_take_effect_is_refused_with_a_reason() {
        let mut zero = window("w", "node-a", 60, 0);
        assert_eq!(may_declare(&zero, &[], at(0)), Err(Refusal::ZeroLength));

        zero.minutes = LONGEST_WINDOW_MINUTES + 1;
        let Err(why @ Refusal::TooLong { .. }) = may_declare(&zero, &[], at(0)) else {
            panic!("a fortnight-long window was accepted without a word");
        };
        // The sentence has to name the thing to do instead, or it is just a
        // number somebody has to guess under.
        assert!(why.to_string().contains("take it out of the cell"), "{why}");

        // Already over. Declared for last night by somebody in the wrong
        // timezone, and it would sit in the list doing nothing.
        let past = window("w", "node-a", 0, 30);
        assert_eq!(may_declare(&past, &[], at(31)), Err(Refusal::AlreadyOver));

        // Starting in the past is fine: work that has already begun is a true
        // thing to say, and refusing it teaches people to lie about the time.
        let started = window("w", "node-a", 0, 30);
        assert_eq!(may_declare(&started, &[], at(5)), Ok(()));
    }

    /// Two windows over the same node and the same hour are two answers to one
    /// question, and whoever declared the second believes theirs is the answer.
    #[test]
    fn an_overlapping_window_on_the_same_node_is_refused_by_name() {
        let existing = vec![window("swap", "node-a", 60, 60)];

        let overlapping = window("reboot", "node-a", 90, 30);
        let Err(Refusal::Overlaps { other, .. }) = may_declare(&overlapping, &existing, at(0))
        else {
            panic!("two windows were allowed to cover the same hour of the same node");
        };
        assert!(
            other.ends_with("swap"),
            "the refusal did not name which one: {other}"
        );

        // Touching, not overlapping: one ends exactly as the other begins.
        assert_eq!(
            may_declare(&window("after", "node-a", 120, 30), &existing, at(0)),
            Ok(())
        );
        // Another node at the same hour is the ordinary case, not a conflict.
        assert_eq!(
            may_declare(&window("swap-b", "node-b", 60, 60), &existing, at(0)),
            Ok(())
        );
        // Editing the window itself must not collide with itself.
        let mut same = window("swap", "node-a", 60, 90);
        same.note = "longer than we thought".into();
        assert_eq!(may_declare(&same, &existing, at(0)), Ok(()));
    }

    /// A window that is over is a record, not a claim on the future.
    #[test]
    fn a_window_that_is_over_does_not_stand_in_the_way_of_a_new_one() {
        let done = vec![window("last-week", "node-a", 0, 60)];
        assert_eq!(
            may_declare(&window("today", "node-a", 630, 60), &done, at(600)),
            Ok(())
        );
    }

    #[test]
    fn the_open_window_is_the_one_placement_is_told_about() {
        let windows = vec![
            window("early", "node-a", 0, 30),
            window("now", "node-a", 60, 60),
            window("later", "node-a", 600, 30),
            window("elsewhere", "node-b", 60, 60),
        ];
        let open = open_on("node-a", &windows, at(70)).unwrap();
        assert!(open.name.ends_with("now"));
        assert!(
            open_on("node-a", &windows, at(45)).is_none(),
            "a gap read as maintenance"
        );

        let next = next_on("node-a", &windows, at(70)).unwrap();
        assert!(next.name.ends_with("later"));
        assert_eq!(opens_in_minutes(next, at(70)), Some(530));
        assert_eq!(
            opens_in_minutes(open, at(70)),
            None,
            "an open window still counts down"
        );
    }

    /// What placement is handed: one row per node, carrying when it comes back.
    #[test]
    fn a_node_under_two_windows_comes_back_when_the_later_one_closes() {
        let windows = vec![
            window("short", "node-a", 60, 10),
            window("long", "node-a", 60, 90),
            window("b", "node-b", 60, 10),
        ];
        let closed = closed_now(&windows, at(65));
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0].node, "node-a");
        assert_eq!(
            closed[0].until,
            at(150),
            "the node was reported free while a window was still open over it"
        );
        assert_eq!(closed[1].node, "node-b");
        // Read hours later out of an instance's condition, "back at 03:00" no
        // longer says whether that has happened. "For another 85 minutes" was
        // true when it was written.
        assert_eq!(closed[0].minutes_left, 85);
        // Rounded up: a node with twenty seconds left is still unusable, and
        // "0 minutes" about a node nothing may be placed on is a lie.
        assert_eq!(closed_now(&windows, at(149))[0].minutes_left, 1);

        assert!(closed_now(&windows, at(200)).is_empty());
    }

    /// The distinction the whole `drain` field exists for: nothing new here, or
    /// nothing here at all.
    #[test]
    fn only_a_draining_window_moves_the_guests_that_are_already_there() {
        let mut quiet = window("firmware", "node-a", 60, 10);
        let mut moving = window("pull-it", "node-b", 60, 10);
        moving.drain = true;

        let windows = vec![quiet.clone(), moving];
        assert!(
            !draining("node-a", &windows, at(65)),
            "a firmware window moved a fleet"
        );
        assert!(draining("node-b", &windows, at(65)));
        // And only while it is open. A window that closes puts everything back
        // by ceasing to be open, with nothing to unwind.
        assert!(!draining("node-b", &windows, at(200)));

        quiet.drain = true;
        assert!(draining("node-a", &[quiet], at(65)));
    }
}
