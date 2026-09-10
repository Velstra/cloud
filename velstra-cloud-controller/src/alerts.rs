//! Telling a human, now, that the cell is not doing its job.
//!
//! The metrics say everything; an alert is for the handful of conditions where
//! waiting until somebody reads a dashboard is already too late. Sentinel has
//! the same thing for a failed unit, and the mechanics are deliberately the
//! same: every configured target is tried, one failing target never stops the
//! others, and a failed delivery is logged rather than propagated — a
//! controller that fell over because a webhook was down would be a second
//! outage caused by the first.
//!
//! ## What fires
//!
//! The rules are few and fixed, because a rule nobody asked for is a rule
//! somebody learns to ignore:
//!
//! | rule | when |
//! |---|---|
//! | `node-wires-nowhere` | a machine carries guest traffic with a bare tap, so its guests have a wire with nothing at the other end |
//! | `node-silent` | a machine has not reported for longer than its own fencing deadline plus a margin — the point at which its guests are certainly stopped ([`velstra_cloud_model::ha::is_fenced`]) |
//! | `pool-nearly-full` | a pool has allocated more than a share of its capacity (80 % unless told otherwise) |
//! | `quota-exhausted` | a project has used every unit of some quota dimension, so its next create is refused |
//! | `ceph-error` / `ceph-warning` | the Ceph cluster says `HEALTH_ERR` or `HEALTH_WARN`, with the checks it named |
//! | `ceph-osd-down` | a disk is not up, or is up and out, so the cluster is carrying data on fewer disks than it has |
//! | `ceph-nearly-full` | the cluster holds more than a share of its raw capacity; at its full ratio every tenant's writes stop |
//! | `stuck` | an object has disagreed with itself — unconverged, unreported, not ready, or blocked from deleting — for longer than a threshold (15 min unless told otherwise) |
//!
//! Each one is `critical` or `warning` — see [`RULES`].
//!
//! The judgement is a pure function of what was listed ([`evaluate`]); the
//! delivery is the only part with the outside in it.
//!
//! ## Transitions, not levels
//!
//! A pool at 85 % is at 85 % on every pass. What a person wants to hear is that
//! it *became* so, and later that it stopped being so — so the notifier keeps
//! the set of alerts that are firing and delivers the difference: a `firing`
//! message when one appears and a `resolved` message when it goes. The set is
//! in memory: a restarted controller repeats every open alert once, which is
//! the right side to err on for something that exists to be noticed.
//!
//! Only the leader delivers. Every process evaluates (the gauge is per process
//! and cheap), but two controllers telling the same person the same thing is
//! how a pager gets silenced.
//!
//! ## Getting there
//!
//! Three things stand between a rule firing and somebody knowing:
//!
//! - **How loud.** Every rule carries a [`Severity`], in one table
//!   ([`RULES`]) that is also what the gauge iterates. It is on the webhook
//!   body and in the mail subject, because that is what a receiver routes on.
//! - **Whether it arrives.** A delivery a target refuses is kept and tried
//!   again on the next pass, bounded. Before that it was dropped: the firing
//!   set is updated before delivery is attempted, so nothing would ever
//!   mention it again.
//! - **Whether the silence means anything.** Every rule here fires only when
//!   something is wrong, so a controller that is down produces exactly the
//!   stream a healthy cell does. A periodic `heartbeat` to the webhook is what
//!   lets an external watcher tell those apart — alert on its absence.
//!
//! And one thing that stops a rule firing at all: a node inside a maintenance
//! window somebody declared is not reported silent. Being paged for work you
//! announced is how a team learns to ignore the pager.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::Serialize;
use tracing::{info, warn};
use velstra_cloud_model::{
    allowance::dimensions,
    ha::{NodeView, is_fenced},
    meta::{Timestamp, condition},
    resources::{NodeSpec, NodeStatus, PoolSpec, PoolStatus, ProjectSpec, ProjectStatus, Resource},
};

use crate::{Metrics, drift::Divergent};

/// Where an alert goes. Empty means nobody is told, and the gauge still moves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Targets {
    /// A URL to POST one JSON object per transition to.
    pub webhook: Option<String>,
    /// Addresses to mail, through a sendmail-compatible binary.
    pub mail_to: Vec<String>,
    /// The sender the mail carries.
    pub mail_from: String,
    /// The binary that takes the message on stdin with `-t`.
    pub sendmail: PathBuf,
}

impl Targets {
    pub fn is_empty(&self) -> bool {
        self.webhook.is_none() && self.mail_to.is_empty()
    }
}

/// The thresholds. Named so the flags and the sentences agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// A pool is nearly full at this share of its capacity, in percent.
    pub pool_full_percent: u8,
    /// A backup target with less than this many GiB free is worth a word.
    ///
    /// Absolute rather than a share, because a target's total size is not
    /// reported — only what is left — and because what matters is whether the
    /// next copy fits, which is an absolute question.
    pub backup_target_free_gib: u32,
    /// An object is stuck when it has diverged for this long.
    pub stuck_after: Duration,
    /// Added to a node's fencing deadline before it counts as silent — the
    /// same margin recovery uses, so the two agree about when a machine is
    /// gone.
    pub silence_margin: Duration,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            pool_full_percent: 80,
            backup_target_free_gib: 32,
            stuck_after: Duration::from_secs(15 * 60),
            silence_margin: Duration::from_secs(60),
        }
    }
}

/// Everything the notifier is told at startup.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub targets: Targets,
    pub rules: Rules,
}

/// How much of a hurry somebody is in.
///
/// Two rungs and no more. A scheme with five is a scheme where three of them
/// mean "later", and the only decision an on-call rota actually makes is
/// whether to wake somebody up. `Critical` is "a tenant's workload is stopped
/// or their data is one failure from gone"; `Warning` is "this becomes that if
/// nobody looks".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
        }
    }
}

/// Every rule, with how loud it is. One table, so a rule cannot exist without
/// somebody having decided whether it is worth a phone call.
///
/// It is also what the gauge iterates, so a rule added here is a rule the
/// metrics know about — the two used to be separate lists, and a rule in one
/// and not the other fired and delivered while reading as zero on the
/// dashboard.
pub const RULES: &[(&str, Severity)] = &[
    // A machine is gone and its guests are stopped. Nothing about this waits.
    ("node-silent", Severity::Critical),
    // Guests on it have wires that lead nowhere. Not critical — nothing is
    // *lost* — but it is the shape that looks like a working cell and is not,
    // and an operator who does not know will spend the day on the guest.
    ("node-wires-nowhere", Severity::Warning),
    // Data is being carried on fewer disks than it should be, or the cluster
    // says it is in trouble. One more failure is the whole point.
    ("ceph-error", Severity::Critical),
    ("ceph-osd-down", Severity::Critical),
    // Everything below becomes one of the above if nobody looks.
    ("ceph-warning", Severity::Warning),
    ("ceph-nearly-full", Severity::Warning),
    ("pool-nearly-full", Severity::Warning),
    // The place the copies go is filling up. A leading indicator on purpose:
    // by the time a backup *fails* for want of room, the target is already
    // full for everybody else on it — including for the delete that would
    // make room.
    ("backup-target-nearly-full", Severity::Warning),
    // Nobody is looking at whether the mount is up. A target with no agent
    // named is one whose disappearance is discovered by a failing copy.
    ("backup-target-unwatched", Severity::Warning),
    ("quota-exhausted", Severity::Warning),
    ("stuck", Severity::Warning),
];

/// How loud a rule is, or `Warning` for one nothing has decided about — the
/// safe direction: an unclassified rule that woke somebody at three in the
/// morning would be the last time anybody trusted the severity.
pub fn severity_of(rule: &str) -> Severity {
    RULES
        .iter()
        .find(|(name, _)| *name == rule)
        .map(|(_, severity)| *severity)
        .unwrap_or(Severity::Warning)
}

/// One condition worth a person's attention.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Alert {
    /// The rule, as the table above names it.
    pub rule: &'static str,
    /// The object it is about, by full name.
    pub subject: String,
    /// A sentence.
    pub message: String,
}

impl Alert {
    pub fn severity(&self) -> Severity {
        severity_of(self.rule)
    }
}

/// What the notifier did on one pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transitions {
    pub fired: Vec<Alert>,
    pub resolved: Vec<Alert>,
}

/// The rules, applied.
///
/// Pure: the same inputs give the same alerts, in a stable order, so a test
/// can say exactly what fires and the notifier can diff two passes.
///
/// Eight arguments, and each one is a different collection the rules read. A
/// struct wrapping them would be a struct built at one call site and taken
/// apart at the other.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    nodes: &[Resource<NodeSpec, NodeStatus>],
    pools: &[Resource<PoolSpec, PoolStatus>],
    projects: &[Resource<ProjectSpec, ProjectStatus>],
    ceph: &[velstra_cloud_model::ceph::CephCluster],
    targets: &[Resource<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    >],
    divergent: &[Divergent],
    windows: &[velstra_cloud_model::maintenance::WindowView],
    rules: &Rules,
    now: Timestamp,
) -> Vec<Alert> {
    let mut out = Vec::new();

    // What Ceph says about itself. A provider running it got no page for a
    // down OSD, a degraded placement group or a cluster in HEALTH_ERR — the
    // states where a tenant's data is one more failure from being gone. They
    // found out by somebody opening the console.
    for cluster in ceph {
        let seen = &cluster.status.seen;
        if seen.health == "HEALTH_ERR" || seen.health == "HEALTH_WARN" {
            let what: Vec<String> = seen
                .warnings
                .iter()
                .map(|w| format!("{}: {}", w.code, w.message))
                .collect();
            out.push(Alert {
                rule: if seen.health == "HEALTH_ERR" {
                    "ceph-error"
                } else {
                    "ceph-warning"
                },
                subject: cluster.meta.name.to_string(),
                message: if what.is_empty() {
                    format!("{} reports {}", cluster.meta.name, seen.health)
                } else {
                    format!(
                        "{} reports {}: {}",
                        cluster.meta.name,
                        seen.health,
                        what.join("; ")
                    )
                },
            });
        }
        let down: Vec<String> = seen
            .osds
            .iter()
            .filter(|o| !o.up || !o.r#in)
            .map(|o| format!("osd.{} on {}", o.id, o.host))
            .collect();
        if !down.is_empty() {
            out.push(Alert {
                rule: "ceph-osd-down",
                subject: cluster.meta.name.to_string(),
                message: format!(
                    "{} of {}'s disks are not carrying data: {}",
                    down.len(),
                    cluster.meta.name,
                    down.join(", ")
                ),
            });
        }
        // Fill, on the raw numbers Ceph reports. A cluster that reaches its
        // full ratio stops accepting writes for every tenant at once, and the
        // warning about it is worth having well before then.
        if let Some(percent) = seen
            .used_bytes
            .saturating_mul(100)
            .checked_div(seen.total_bytes)
        {
            if percent >= u64::from(rules.pool_full_percent) {
                out.push(Alert {
                    rule: "ceph-nearly-full",
                    subject: cluster.meta.name.to_string(),
                    message: format!(
                        "{} holds {percent} % of its raw capacity; a cluster that reaches its \
                         full ratio stops accepting writes for every tenant at once",
                        cluster.meta.name
                    ),
                });
            }
        }
    }

    for node in nodes {
        let view = NodeView {
            name: node.meta.name.to_string(),
            last_heartbeat: node.status.last_heartbeat,
            fence_after_s: node.spec.fence_after_s,
            ready: condition(&node.status.conditions, "Ready")
                .is_some_and(|c| c.status == velstra_cloud_model::meta::ConditionStatus::True),
        };
        // A machine that has never reported is a machine being registered,
        // not one that fell silent; nothing is stopped on it yet.
        if node.status.last_heartbeat == Timestamp(0) {
            continue;
        }
        // Nor is a machine somebody is deliberately working on. An operator
        // who declared a window and pulled the power got paged for doing
        // exactly what they said they were going to do — which is how a team
        // learns that the pager is usually wrong. The window is the operator's
        // own statement of intent; honouring it is the least this can do.
        if is_out_for_maintenance(&view.name, windows, now) {
            continue;
        }
        if is_fenced(&view, now, rules.silence_margin.as_secs() as u32) {
            let quiet = node.status.last_heartbeat.age(now).as_secs();
            out.push(Alert {
                rule: "node-silent",
                subject: view.name.clone(),
                message: format!(
                    "{} has not reported for {quiet} s, past its fencing deadline of {} s; \
                     its guests are stopped and recovery may move them",
                    view.name, node.spec.fence_after_s
                ),
            });
        }
    }

    // A node whose datapath is a bare tap gives every guest a wire that leads
    // nowhere: an address, a gateway that answers nothing, and no way off its
    // own machine. It is the *right* answer on a node whose fabric carries the
    // segment — which is why it cannot simply be forbidden — and a silent dead
    // end on one without.
    //
    // Found on a live cell whose seed named no datapath at all, so every node
    // took the default and nobody was ever asked. The symptom was guests that
    // could not reach the internet; the cause was a setting that had no name
    // anywhere on the platform.
    for node in nodes.iter().filter(|n| n.status.datapath == "tap") {
        if node.status.last_heartbeat == Timestamp(0) {
            continue;
        }
        out.push(Alert {
            rule: "node-wires-nowhere",
            subject: node.meta.name.to_string(),
            message: format!(
                "{} carries guest traffic with a bare tap: each guest gets a wire and nothing \
                 at the other end of it. That is correct if a fabric carries this cell's \
                 segments, and a dead end if nothing does — set VELSTRA_LOCAL_NETWORK=1 in \
                 this node's seed to have it hold the gateways itself, or point it at a fabric.",
                node.meta.name
            ),
        });
    }

    for pool in pools {
        let (capacity, allocated) = (pool.status.capacity_gib, pool.status.allocated_gib);
        if capacity == 0 {
            continue;
        }
        let percent = allocated.saturating_mul(100) / capacity;
        if percent >= u64::from(rules.pool_full_percent) {
            out.push(Alert {
                rule: "pool-nearly-full",
                subject: pool.meta.name.to_string(),
                message: format!(
                    "{} has allocated {allocated} of {capacity} GiB ({percent} %); a volume \
                     that does not fit is refused at creation",
                    pool.meta.name
                ),
            });
        }
    }

    // Where the copies go. A backup target is the one piece of storage on a
    // cell that nothing else watches: pools are measured because the scheduler
    // needs them, and a target is measured only if somebody said who looks.
    for target in targets {
        if !target.spec.accepting {
            continue;
        }
        if target.spec.agent.is_empty() {
            out.push(Alert {
                rule: "backup-target-unwatched",
                subject: target.meta.name.to_string(),
                message: format!(
                    "{} is accepting backups and no agent reports on it: whether {} is mounted, \
                     writable or full is unknown, and a mount that has gone shows up as a \
                     failing copy rather than as this. Name an agent in spec.agent.",
                    target.meta.name, target.spec.path
                ),
            });
            continue;
        }
        // Only a measured target can be called full. `free_gib` is zero both
        // for a full target and for one whose agent has not reported yet, and
        // the condition is what tells them apart.
        if target.status.writable.is_none() {
            continue;
        }
        if target.status.free_gib < u64::from(rules.backup_target_free_gib) {
            out.push(Alert {
                rule: "backup-target-nearly-full",
                subject: target.meta.name.to_string(),
                message: format!(
                    "{} has {} GiB free; a copy that does not fit is refused, and a target run \
                     to the last byte fails every tenant's next backup — including the delete \
                     that would make room",
                    target.meta.name, target.status.free_gib
                ),
            });
        }
    }

    for project in projects {
        let exhausted: Vec<String> = dimensions(&project.spec.quota, &project.status.used)
            .into_iter()
            .filter(|d| d.exhausted())
            .map(|d| format!("{} ({} of {})", d.name, d.used, d.limit))
            .collect();
        if !exhausted.is_empty() {
            out.push(Alert {
                rule: "quota-exhausted",
                subject: project.meta.name.to_string(),
                message: format!(
                    "{} has used all of: {}; the next create in that dimension is refused",
                    project.meta.name,
                    exhausted.join(", ")
                ),
            });
        }
    }

    for d in divergent {
        if d.age_seconds >= rules.stuck_after.as_secs() {
            out.push(Alert {
                rule: "stuck",
                subject: d.name.clone(),
                message: format!(
                    "{} has been {} for {} s; the object's conditions say why",
                    d.name,
                    d.reason.label(),
                    d.age_seconds
                ),
            });
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Whether a node is inside a window somebody declared for it.
///
/// Any open window, not only a draining one: a firmware update that takes the
/// machine off the network for ten minutes is exactly the case, and it is
/// declared with `drain: false`.
fn is_out_for_maintenance(
    node: &str,
    windows: &[velstra_cloud_model::maintenance::WindowView],
    now: Timestamp,
) -> bool {
    // The node's id, because a window names the machine the way a node object
    // is named and this list carries full resource names.
    let id = node.rsplit('/').next().unwrap_or(node);
    windows
        .iter()
        .any(|w| (w.node == id || w.node == node) && w.is_open(now))
}

/// Keeps what is firing, delivers the difference.
pub struct Notifier {
    config: Config,
    cell: String,
    metrics: Metrics,
    /// What is currently wrong, keyed by rule and subject rather than by the
    /// whole alert — see [`Notifier::observe`].
    firing: BTreeMap<(&'static str, String), Alert>,
    /// Transitions a target would not take, kept to try again.
    ///
    /// Without this a transition lost to a 500 or a dropped connection was
    /// lost for good: the firing set is updated before delivery is attempted,
    /// so the next pass sees no transition and says nothing. An alert that
    /// silently did not arrive is worse than no alerting, because the silence
    /// reads as "nothing is wrong".
    undelivered: Vec<(&'static str, Alert)>,
    /// When the last dead-man beat went out.
    last_beat: Timestamp,
    client: reqwest::Client,
}

pub const FIRING: &str = "alerts_firing";
pub const DELIVERIES: &str = "alert_deliveries_total";
/// How many transitions are waiting for a target that would not take them.
pub const WAITING: &str = "alerts_undelivered";

/// How many undelivered transitions are kept before the oldest are dropped.
///
/// A cell has a few dozen objects that can be wrong at once; this is room for
/// several passes' worth of everything being wrong at the same time as the
/// webhook being down, which is the case worth surviving.
const MOST_UNDELIVERED: usize = 512;

/// How often the dead-man beat goes out, at most. The pass itself is slower
/// than this by default, so in practice it is one beat per pass.
const BEAT_EVERY: Duration = Duration::from_secs(60);

/// How long a target gets. Short, because a pass that waits on an unreachable
/// endpoint is a pass that notices nothing else.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

impl Notifier {
    pub fn new(config: Config, cell: &str, metrics: Metrics) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DELIVERY_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            config,
            cell: cell.to_string(),
            metrics,
            firing: BTreeMap::new(),
            undelivered: Vec::new(),
            last_beat: Timestamp(0),
            client,
        }
    }

    pub fn rules(&self) -> &Rules {
        &self.config.rules
    }

    /// How many transitions are waiting for a target that would not take them.
    pub fn waiting(&self) -> usize {
        self.undelivered.len()
    }

    /// Take this pass's alerts, publish the gauge, and — when `deliver` — tell
    /// the targets what changed. Returns what changed either way, so a test
    /// can assert on transitions without a target.
    pub async fn observe(&mut self, current: Vec<Alert>, deliver: bool) -> Transitions {
        // Compared by *what is wrong with what*, not by the sentence. Several
        // rules put the elapsed time in their message — "has been NotReady for
        // 10053 s" — so a set of whole alerts changed on every pass: the same
        // condition resolved and fired again every five minutes, for ever, and
        // an on-call reading that could not tell a new problem from an old one.
        let now: BTreeMap<(&'static str, String), Alert> = current
            .into_iter()
            .map(|a| ((a.rule, a.subject.clone()), a))
            .collect();
        let fired: Vec<Alert> = now
            .iter()
            .filter(|(key, _)| !self.firing.contains_key(*key))
            .map(|(_, a)| a.clone())
            .collect();
        let resolved: Vec<Alert> = self
            .firing
            .iter()
            .filter(|(key, _)| !now.contains_key(*key))
            .map(|(_, a)| a.clone())
            .collect();
        self.firing = now;

        self.metrics.clear(FIRING, &[]);
        for (rule, severity) in RULES {
            let n = self.firing.keys().filter(|(r, _)| r == rule).count();
            self.metrics.set(
                FIRING,
                &[("rule", rule), ("severity", severity.as_str())],
                n as f64,
            );
        }

        if deliver {
            // What an earlier pass could not get out, first: an alert is worth
            // less the later it arrives, and the oldest one waiting is the one
            // that has already lost the most.
            let waiting = std::mem::take(&mut self.undelivered);
            for (kind, alert) in waiting {
                self.attempt(kind, &alert).await;
            }
            for a in &fired {
                warn!(rule = a.rule, severity = a.severity().as_str(), subject = %a.subject, "{}", a.message);
                self.attempt("firing", a).await;
            }
            for a in &resolved {
                info!(rule = a.rule, subject = %a.subject, "resolved: {}", a.message);
                self.attempt("resolved", a).await;
            }
            self.beat(Timestamp::now()).await;
        }
        Transitions { fired, resolved }
    }

    /// Deliver, and keep it if nobody took it.
    ///
    /// Bounded: a webhook that has been down for a day must not turn this into
    /// a queue that eats the process. Past the bound the oldest are dropped
    /// with a line saying so, because a controller that ran out of memory
    /// holding alerts would be the outage.
    async fn attempt(&mut self, kind: &'static str, alert: &Alert) {
        if self.deliver(kind, alert).await {
            return;
        }
        self.undelivered.push((kind, alert.clone()));
        if self.undelivered.len() > MOST_UNDELIVERED {
            let dropped = self.undelivered.len() - MOST_UNDELIVERED;
            self.undelivered.drain(..dropped);
            warn!(
                dropped,
                "the alert targets have been unreachable long enough that older messages are \
                 being dropped"
            );
        }
        self.metrics
            .set(WAITING, &[], self.undelivered.len() as f64);
    }

    /// Say, periodically, that this is still running.
    ///
    /// Every rule here fires only when something is wrong, which means a
    /// controller that is down, or has lost the election, or whose store reads
    /// are failing, produces exactly the same stream as a healthy cell: none.
    /// Nothing outside can tell those apart, and "the pager has been quiet"
    /// stops being evidence of anything. A beat is what makes the quiet mean
    /// something — an external watcher alerts when it stops.
    ///
    /// The webhook only. A heartbeat in somebody's inbox every five minutes is
    /// a heartbeat somebody makes a mail rule for.
    async fn beat(&mut self, now: Timestamp) {
        if now.0.saturating_sub(self.last_beat.0) < BEAT_EVERY.as_millis() as u64 {
            return;
        }
        let Some(url) = self.config.targets.webhook.clone() else {
            return;
        };
        self.last_beat = now;
        let body = serde_json::json!({
            "kind": "heartbeat",
            "rule": "watchdog",
            "severity": Severity::Warning.as_str(),
            "subject": self.cell,
            "message": "the alerting controller is running and is the leader; \
                        alert on the absence of this",
            "cell": self.cell,
            "firing": self.firing.len(),
            "at": now.0,
        });
        let sent = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        let outcome = match sent {
            Ok(r) if r.status().is_success() => "ok",
            Ok(_) => "refused",
            Err(_) => "unreachable",
        };
        self.metrics
            .count(DELIVERIES, &[("target", "heartbeat"), ("outcome", outcome)]);
    }

    /// One attempt at every target. `false` when any of them would not take it,
    /// which is what puts it in the retry queue.
    async fn deliver(&self, kind: &str, alert: &Alert) -> bool {
        let mut delivered = true;
        let targets = &self.config.targets;
        if let Some(url) = &targets.webhook {
            let body = serde_json::json!({
                "kind": kind,
                "rule": alert.rule,
                // The one field an on-call rota routes on. Without it every
                // alert arrived looking the same, and a receiver could only
                // tell a stopped machine from a pool at 80 % by keeping its
                // own copy of this table.
                "severity": alert.severity().as_str(),
                "subject": alert.subject,
                "message": alert.message,
                "cell": self.cell,
                "at": Timestamp::now().0,
            });
            let sent = self
                .client
                .post(url)
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await;
            let outcome = match sent {
                Ok(r) if r.status().is_success() => "ok",
                Ok(r) => {
                    warn!(status = %r.status(), "the alert webhook refused the message");
                    "refused"
                }
                Err(e) => {
                    warn!(error = %e, "the alert webhook could not be reached");
                    "unreachable"
                }
            };
            delivered &= outcome == "ok";
            self.metrics
                .count(DELIVERIES, &[("target", "webhook"), ("outcome", outcome)]);
        }
        if !targets.mail_to.is_empty() {
            let outcome = match send_mail(targets, &self.cell, kind, alert).await {
                Ok(()) => "ok",
                Err(e) => {
                    warn!(error = %e, "the alert mail could not be handed to sendmail");
                    "failed"
                }
            };
            delivered &= outcome == "ok";
            self.metrics
                .count(DELIVERIES, &[("target", "mail"), ("outcome", outcome)]);
        }
        delivered
    }
}

/// The message as sendmail reads it from stdin.
pub fn mail_body(targets: &Targets, cell: &str, kind: &str, alert: &Alert) -> String {
    format!(
        "From: {}\r\nTo: {}\r\nSubject: [velstra {cell}] {} {} {} {}\r\n\r\n{}\r\n",
        targets.mail_from,
        targets.mail_to.join(", "),
        kind.to_uppercase(),
        // In the subject, because a mail client sorts and filters on the
        // subject and nothing else.
        alert.severity().as_str().to_uppercase(),
        alert.rule,
        alert.subject,
        alert.message
    )
}

async fn send_mail(targets: &Targets, cell: &str, kind: &str, alert: &Alert) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&targets.sendmail)
        .arg("-t")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting {}: {e}", targets.sendmail.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(mail_body(targets, cell, kind, alert).as_bytes())
            .await
            .map_err(|e| format!("writing the message: {e}"))?;
    }
    let done = tokio::time::timeout(DELIVERY_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "sendmail did not finish in time".to_string())?
        .map_err(|e| format!("waiting for sendmail: {e}"))?;
    if done.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sendmail exited with {}: {}",
            done.status,
            String::from_utf8_lossy(&done.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        reconcile::DivergenceReason,
        resources::Quota,
    };

    use super::*;

    fn meta(name: &str) -> Meta {
        Meta::new(
            ResourceName::parse(name).unwrap(),
            Placement {
                region: "r".into(),
                cell: "c".into(),
            },
        )
    }

    fn node(name: &str, fence_after_s: u32, heard: u64) -> Resource<NodeSpec, NodeStatus> {
        Resource::new(
            meta(&format!("nodes/{name}")),
            NodeSpec {
                fence_after_s,
                ..Default::default()
            },
            NodeStatus {
                last_heartbeat: Timestamp(heard),
                ..Default::default()
            },
        )
    }

    fn pool(name: &str, capacity: u64, allocated: u64) -> Resource<PoolSpec, PoolStatus> {
        Resource::new(
            meta(&format!("pools/{name}")),
            PoolSpec::default(),
            PoolStatus {
                capacity_gib: capacity,
                allocated_gib: allocated,
                ..Default::default()
            },
        )
    }

    fn target(
        name: &str,
        agent: &str,
        writable: Option<bool>,
        free_gib: u64,
    ) -> Resource<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    > {
        Resource::new(
            meta(&format!("backup-targets/{name}")),
            velstra_cloud_model::backup::BackupTargetSpec {
                path: format!("/srv/{name}"),
                accepting: true,
                agent: agent.to_string(),
                ..Default::default()
            },
            velstra_cloud_model::backup::BackupTargetStatus {
                agent: (!agent.is_empty()).then(|| agent.to_string()),
                writable,
                free_gib,
                ..Default::default()
            },
        )
    }

    /// A target filling up is said *before* a copy fails for want of room —
    /// by then the target is full for every other tenant on it too.
    #[test]
    fn a_target_running_out_of_room_is_worth_a_word() {
        let roomy = target("nas", "nodes/hv-1", Some(true), 500);
        let tight = target("usb", "nodes/hv-1", Some(true), 4);
        let alerts = evaluate(
            &[],
            &[],
            &[],
            &[],
            &[roomy, tight],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        let named: Vec<&str> = alerts
            .iter()
            .filter(|a| a.rule == "backup-target-nearly-full")
            .map(|a| a.subject.as_str())
            .collect();
        assert_eq!(named, vec!["backup-targets/usb"], "{alerts:?}");
    }

    /// Zero free and nobody reporting are the same number and different facts.
    #[test]
    fn a_target_nobody_has_measured_is_not_called_full() {
        let unmeasured = target("nas", "nodes/hv-1", None, 0);
        let alerts = evaluate(
            &[],
            &[],
            &[],
            &[],
            &[unmeasured],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        assert!(
            !alerts.iter().any(|a| a.rule == "backup-target-nearly-full"),
            "{alerts:?}"
        );
    }

    /// A target accepting copies with no agent named is one whose mount could
    /// have gone an hour ago with nothing to say so.
    #[test]
    fn a_target_with_nobody_watching_it_says_so() {
        let orphan = target("nas", "", None, 0);
        let alerts = evaluate(
            &[],
            &[],
            &[],
            &[],
            &[orphan],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        let found: Vec<&Alert> = alerts
            .iter()
            .filter(|a| a.rule == "backup-target-unwatched")
            .collect();
        assert_eq!(found.len(), 1, "{alerts:?}");
        assert!(found[0].message.contains("spec.agent"), "{:?}", found[0]);
    }

    fn project(name: &str, limit: u32, used: u32) -> Resource<ProjectSpec, ProjectStatus> {
        Resource::new(
            meta(&format!("projects/{name}")),
            ProjectSpec {
                quota: Quota {
                    instances: limit,
                    ..Default::default()
                },
                ..Default::default()
            },
            ProjectStatus {
                used: Quota {
                    instances: used,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
    }

    const NOW: Timestamp = Timestamp(1_000_000);

    #[test]
    fn a_node_past_its_deadline_is_silent_and_one_still_reporting_is_not() {
        let nodes = [
            node("quiet", 30, NOW.0 - 120_000),
            node("fresh", 30, NOW.0 - 5_000),
            node("new", 30, 0),
        ];
        let alerts = evaluate(&nodes, &[], &[], &[], &[], &[], &[], &Rules::default(), NOW);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0].rule, "node-silent");
        assert_eq!(alerts[0].subject, "nodes/quiet");
        assert!(alerts[0].message.contains("120 s"));
    }

    /// A transition a target would not take is kept and tried again. Without
    /// this it was lost for good: the firing set is updated before delivery is
    /// attempted, so the next pass sees no transition and says nothing, and
    /// the silence reads as "nothing is wrong".
    #[tokio::test]
    async fn a_refused_delivery_is_kept_and_tried_again() {
        let metrics = Metrics::new();
        // Nothing is listening there, so the connection is refused at once.
        let mut n = Notifier::new(
            Config {
                targets: Targets {
                    webhook: Some("http://127.0.0.1:1/alerts".into()),
                    ..Default::default()
                },
                rules: Rules::default(),
            },
            "cell-1",
            metrics,
        );
        let a = Alert {
            rule: "stuck",
            subject: "projects/p1/instances/i1".into(),
            message: "m".into(),
        };
        n.observe(vec![a.clone()], true).await;
        assert_eq!(n.waiting(), 1, "a delivery nobody took was forgotten");

        // The next pass tries it again — and, still failing, keeps it. What it
        // must not do is drop it or double it.
        n.observe(vec![a.clone()], true).await;
        assert_eq!(n.waiting(), 1);

        // And a pass that is not the leader's does not touch the queue.
        n.observe(vec![], false).await;
        assert_eq!(n.waiting(), 1);
    }

    /// A node whose wires lead nowhere says so. It is the shape that looks like
    /// a working cell and is not: guests get an address, a gateway that answers
    /// nothing, and no way off their own machine — and until this fired,
    /// nothing anywhere named the setting that caused it.
    #[test]
    fn a_node_with_a_bare_tap_says_its_guests_have_nowhere_to_go() {
        let mut bare = node("quiet-wire", 30, NOW.0 - 1_000);
        bare.status.datapath = "tap".into();
        let mut carried = node("on-a-fabric", 30, NOW.0 - 1_000);
        carried.status.datapath = "fabric".into();
        let mut gateway = node("holds-the-gateway", 30, NOW.0 - 1_000);
        gateway.status.datapath = "local-network".into();
        // An older agent reports nothing at all, and an empty answer is not a
        // claim that the wires lead nowhere.
        let silent_about_it = node("older-agent", 30, NOW.0 - 1_000);

        let alerts = evaluate(
            &[bare, carried, gateway, silent_about_it],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        let named: Vec<&str> = alerts
            .iter()
            .filter(|a| a.rule == "node-wires-nowhere")
            .map(|a| a.subject.as_str())
            .collect();
        assert_eq!(named, vec!["nodes/quiet-wire"], "{alerts:?}");
        assert!(
            alerts[0].message.contains("VELSTRA_LOCAL_NETWORK"),
            "the alert does not name the setting that fixes it: {}",
            alerts[0].message
        );
    }

    /// A machine somebody declared out of service does not page anybody for
    /// being out of service. An operator who filed a window and pulled the
    /// power got paged for doing exactly what they said they would — which is
    /// how a team learns to ignore the pager.
    #[tokio::test]
    async fn a_node_in_a_declared_window_does_not_page() {
        let nodes = [node("quiet", 30, NOW.0 - 120_000)];
        let window = velstra_cloud_model::maintenance::WindowView {
            name: "maintenance-windows/w1".into(),
            node: "quiet".into(),
            starts_at: Timestamp(NOW.0 - 600_000),
            minutes: 60,
            drain: false,
            note: "memory swap".into(),
        };
        assert!(
            evaluate(
                &nodes,
                &[],
                &[],
                &[],
                &[],
                &[],
                std::slice::from_ref(&window),
                &Rules::default(),
                NOW
            )
            .is_empty(),
            "declared work paged somebody"
        );

        // And a window that has closed suppresses nothing: the machine is
        // supposed to be back.
        let over = velstra_cloud_model::maintenance::WindowView {
            minutes: 1,
            ..window
        };
        assert_eq!(
            evaluate(
                &nodes,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[over],
                &Rules::default(),
                NOW
            )
            .len(),
            1,
            "a window that had ended was still suppressing the alert"
        );
    }

    /// Two rungs, decided in one table, and every rule is in it. The table is
    /// also what the gauge iterates, so a rule missing from it would fire and
    /// deliver while reading as zero on the dashboard.
    #[test]
    fn every_rule_has_a_severity_and_the_loud_ones_are_loud() {
        assert_eq!(severity_of("node-silent"), Severity::Critical);
        assert_eq!(severity_of("ceph-osd-down"), Severity::Critical);
        assert_eq!(severity_of("pool-nearly-full"), Severity::Warning);
        // A rule nobody classified is quiet, not loud: an unclassified rule
        // that woke somebody at three in the morning would be the last time
        // anybody trusted the field.
        assert_eq!(severity_of("something-new"), Severity::Warning);

        // Every rule the evaluator can produce is in the table. Collected from
        // the rules themselves rather than typed out again, because a second
        // list is a second list to forget.
        let mut produced: Vec<&str> = Vec::new();
        for health in ["HEALTH_ERR", "HEALTH_WARN"] {
            let mut cluster: velstra_cloud_model::ceph::CephCluster = Resource::new(
                velstra_cloud_model::meta::Meta::new(
                    velstra_cloud_model::meta::ResourceName::parse("ceph-clusters/lab").unwrap(),
                    velstra_cloud_model::meta::Placement::new("eu", "cell-1"),
                ),
                Default::default(),
                Default::default(),
            );
            cluster.status.seen = velstra_cloud_model::ceph::CephSeen {
                health: health.into(),
                osds: vec![velstra_cloud_model::ceph::OsdSeen {
                    id: 1,
                    host: "hv-2".into(),
                    up: false,
                    r#in: true,
                    ..Default::default()
                }],
                used_bytes: 900,
                total_bytes: 1000,
                ..Default::default()
            };
            for a in evaluate(
                &[],
                &[],
                &[],
                &[cluster],
                &[],
                &[],
                &[],
                &Rules::default(),
                NOW,
            ) {
                produced.push(a.rule);
            }
        }
        for a in evaluate(
            &[node("quiet", 30, NOW.0 - 120_000)],
            &[pool("p", 100, 95)],
            &[],
            &[],
            &[],
            &[],
            &[],
            &Rules::default(),
            NOW,
        ) {
            produced.push(a.rule);
        }
        for rule in produced {
            assert!(
                RULES.iter().any(|(name, _)| *name == rule),
                "{rule} fires and is not in the severity table"
            );
        }
    }

    /// Several rules put the elapsed time in their message, so a set of whole
    /// alerts changed on every pass: the same condition resolved and fired
    /// again every five minutes, for ever. An on-call reading that cannot tell
    /// a new problem from an old one.
    #[tokio::test]
    async fn a_standing_problem_fires_once_however_its_sentence_changes() {
        let metrics = Metrics::new();
        let mut notifier = Notifier::new(Config::default(), "cell-1", metrics.clone());
        let first = Alert {
            rule: "stuck",
            subject: "projects/p1/instances/i1".into(),
            message: "has been NotReady for 60 s".into(),
        };
        let later = Alert {
            rule: "stuck",
            subject: "projects/p1/instances/i1".into(),
            message: "has been NotReady for 360 s".into(),
        };

        let opened = notifier.observe(vec![first], false).await;
        assert_eq!(opened.fired.len(), 1);
        let again = notifier.observe(vec![later], false).await;
        assert!(
            again.fired.is_empty(),
            "the same problem fired twice: {:?}",
            again.fired
        );
        assert!(
            again.resolved.is_empty(),
            "the same problem resolved while still true"
        );

        // And when it really goes, it resolves once.
        let gone = notifier.observe(vec![], false).await;
        assert_eq!(gone.resolved.len(), 1);
    }

    /// A provider running Ceph got no page for a down disk, a degraded
    /// placement group or a cluster in HEALTH_ERR — the states where a
    /// tenant's data is one more failure from being gone. They found out by
    /// somebody opening the console.
    #[test]
    fn ceph_says_what_is_wrong_and_the_cell_hears_it() {
        use velstra_cloud_model::ceph::{CephSeen, CephWarning, OsdSeen};
        let mut cluster: velstra_cloud_model::ceph::CephCluster = Resource::new(
            velstra_cloud_model::meta::Meta::new(
                velstra_cloud_model::meta::ResourceName::parse("ceph-clusters/lab").unwrap(),
                velstra_cloud_model::meta::Placement::new("eu", "cell-1"),
            ),
            Default::default(),
            Default::default(),
        );
        cluster.status.seen = CephSeen {
            health: "HEALTH_WARN".into(),
            warnings: vec![CephWarning {
                code: "POOL_NO_REDUNDANCY".into(),
                severity: "HEALTH_WARN".into(),
                message: "2 pool(s) have no replicas configured".into(),
            }],
            osds: vec![
                OsdSeen {
                    id: 0,
                    host: "hv-1".into(),
                    up: true,
                    r#in: true,
                    ..Default::default()
                },
                OsdSeen {
                    id: 1,
                    host: "hv-2".into(),
                    up: false,
                    r#in: true,
                    ..Default::default()
                },
            ],
            used_bytes: 900,
            total_bytes: 1000,
            ..Default::default()
        };

        let fired = evaluate(
            &[],
            &[],
            &[],
            &[cluster],
            &[],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        let rules: Vec<&str> = fired.iter().map(|a| a.rule).collect();
        assert!(rules.contains(&"ceph-warning"), "{rules:?}");
        assert!(rules.contains(&"ceph-osd-down"), "{rules:?}");
        assert!(rules.contains(&"ceph-nearly-full"), "{rules:?}");
        assert!(
            fired
                .iter()
                .any(|a| a.message.contains("POOL_NO_REDUNDANCY")),
            "the alert does not carry what Ceph said: {fired:?}"
        );

        // A healthy cluster is quiet.
        let mut ok: velstra_cloud_model::ceph::CephCluster = Resource::new(
            velstra_cloud_model::meta::Meta::new(
                velstra_cloud_model::meta::ResourceName::parse("ceph-clusters/lab").unwrap(),
                velstra_cloud_model::meta::Placement::new("eu", "cell-1"),
            ),
            Default::default(),
            Default::default(),
        );
        ok.status.seen = CephSeen {
            health: "HEALTH_OK".into(),
            osds: vec![OsdSeen {
                id: 0,
                host: "hv-1".into(),
                up: true,
                r#in: true,
                ..Default::default()
            }],
            used_bytes: 1,
            total_bytes: 1000,
            ..Default::default()
        };
        let quiet = evaluate(&[], &[], &[], &[ok], &[], &[], &[], &Rules::default(), NOW);
        assert!(
            quiet.is_empty(),
            "a healthy cluster woke somebody up: {quiet:?}"
        );
    }

    #[test]
    fn a_pool_at_the_threshold_fires_and_one_below_does_not() {
        let pools = [
            pool("full", 100, 80),
            pool("fine", 100, 79),
            pool("unsized", 0, 0),
        ];
        let alerts = evaluate(&[], &pools, &[], &[], &[], &[], &[], &Rules::default(), NOW);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].subject, "pools/full");
        assert!(alerts[0].message.contains("80 %"));
        let strict = Rules {
            pool_full_percent: 50,
            ..Rules::default()
        };
        assert_eq!(
            evaluate(&[], &pools, &[], &[], &[], &[], &[], &strict, NOW).len(),
            2
        );
    }

    #[test]
    fn an_exhausted_quota_names_the_dimension_and_an_unlimited_one_never_fires() {
        let projects = [
            project("tight", 2, 2),
            project("roomy", 2, 1),
            project("free", 0, 9),
        ];
        let alerts = evaluate(
            &[],
            &[],
            &projects,
            &[],
            &[],
            &[],
            &[],
            &Rules::default(),
            NOW,
        );
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0].subject, "projects/tight");
        assert!(alerts[0].message.contains("instances (2 of 2)"));
    }

    #[test]
    fn an_object_diverged_long_enough_is_stuck() {
        let divergent = [
            Divergent {
                name: "projects/p/instances/old".into(),
                reason: DivergenceReason::Unconverged,
                age_seconds: 1_000,
            },
            Divergent {
                name: "projects/p/instances/young".into(),
                reason: DivergenceReason::Unconverged,
                age_seconds: 10,
            },
        ];
        let alerts = evaluate(
            &[],
            &[],
            &[],
            &[],
            &[],
            &divergent,
            &[],
            &Rules::default(),
            NOW,
        );
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].rule, "stuck");
        assert_eq!(alerts[0].subject, "projects/p/instances/old");
    }

    #[tokio::test]
    async fn the_notifier_reports_only_what_changed_and_keeps_the_gauge_current() {
        let metrics = Metrics::new();
        let mut n = Notifier::new(Config::default(), "c", metrics.clone());
        let a = Alert {
            rule: "stuck",
            subject: "x".into(),
            message: "m".into(),
        };
        let b = Alert {
            rule: "pool-nearly-full",
            subject: "y".into(),
            message: "m".into(),
        };
        let first = n.observe(vec![a.clone(), b.clone()], false).await;
        assert_eq!(first.fired, vec![b.clone(), a.clone()]);
        assert!(first.resolved.is_empty());
        assert_eq!(
            metrics.get(FIRING, &[("rule", "stuck"), ("severity", "warning")]),
            Some(1.0)
        );

        let second = n.observe(vec![a.clone()], false).await;
        assert!(second.fired.is_empty());
        assert_eq!(second.resolved, vec![b.clone()]);
        assert_eq!(
            metrics.get(
                FIRING,
                &[("rule", "pool-nearly-full"), ("severity", "warning")]
            ),
            Some(0.0)
        );

        let third = n.observe(vec![a.clone()], false).await;
        assert_eq!(third, Transitions::default());
    }

    #[test]
    fn the_mail_is_one_message_with_the_cell_and_the_rule_in_its_subject() {
        let targets = Targets {
            webhook: None,
            mail_to: vec!["noc@example.org".into()],
            mail_from: "velstra@example.org".into(),
            sendmail: "/usr/sbin/sendmail".into(),
        };
        let body = mail_body(
            &targets,
            "cell-1",
            "firing",
            &Alert {
                rule: "node-silent",
                subject: "nodes/hv-2".into(),
                message: "hv-2 has not reported".into(),
            },
        );
        assert!(body.starts_with("From: velstra@example.org\r\nTo: noc@example.org\r\n"));
        // The severity is in the subject, because a mail client sorts and
        // filters on the subject and nothing else.
        assert!(
            body.contains(
                "Subject: [velstra cell-1] FIRING CRITICAL node-silent nodes/hv-2\r\n\r\n"
            )
        );
        assert!(body.ends_with("hv-2 has not reported\r\n"));
    }
}
