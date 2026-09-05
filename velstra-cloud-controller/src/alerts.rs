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
//! | `node-silent` | a machine has not reported for longer than its own fencing deadline plus a margin — the point at which its guests are certainly stopped ([`velstra_cloud_model::ha::is_fenced`]) |
//! | `pool-nearly-full` | a pool has allocated more than a share of its capacity (80 % unless told otherwise) |
//! | `quota-exhausted` | a project has used every unit of some quota dimension, so its next create is refused |
//! | `stuck` | an object has disagreed with itself — unconverged, unreported, not ready, or blocked from deleting — for longer than a threshold (15 min unless told otherwise) |
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

use std::{collections::BTreeSet, path::PathBuf, time::Duration};

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
pub fn evaluate(
    nodes: &[Resource<NodeSpec, NodeStatus>],
    pools: &[Resource<PoolSpec, PoolStatus>],
    projects: &[Resource<ProjectSpec, ProjectStatus>],
    divergent: &[Divergent],
    rules: &Rules,
    now: Timestamp,
) -> Vec<Alert> {
    let mut out = Vec::new();

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

/// Keeps what is firing, delivers the difference.
pub struct Notifier {
    config: Config,
    cell: String,
    metrics: Metrics,
    firing: BTreeSet<Alert>,
    client: reqwest::Client,
}

pub const FIRING: &str = "alerts_firing";
pub const DELIVERIES: &str = "alert_deliveries_total";

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
            firing: BTreeSet::new(),
            client,
        }
    }

    pub fn rules(&self) -> &Rules {
        &self.config.rules
    }

    /// Take this pass's alerts, publish the gauge, and — when `deliver` — tell
    /// the targets what changed. Returns what changed either way, so a test
    /// can assert on transitions without a target.
    pub async fn observe(&mut self, current: Vec<Alert>, deliver: bool) -> Transitions {
        let now: BTreeSet<Alert> = current.into_iter().collect();
        let fired: Vec<Alert> = now.difference(&self.firing).cloned().collect();
        let resolved: Vec<Alert> = self.firing.difference(&now).cloned().collect();
        self.firing = now;

        self.metrics.clear(FIRING, &[]);
        for rule in [
            "node-silent",
            "pool-nearly-full",
            "quota-exhausted",
            "stuck",
        ] {
            let n = self.firing.iter().filter(|a| a.rule == rule).count();
            self.metrics.set(FIRING, &[("rule", rule)], n as f64);
        }

        if deliver {
            for a in &fired {
                warn!(rule = a.rule, subject = %a.subject, "{}", a.message);
                self.deliver("firing", a).await;
            }
            for a in &resolved {
                info!(rule = a.rule, subject = %a.subject, "resolved: {}", a.message);
                self.deliver("resolved", a).await;
            }
        }
        Transitions { fired, resolved }
    }

    async fn deliver(&self, kind: &str, alert: &Alert) {
        let targets = &self.config.targets;
        if let Some(url) = &targets.webhook {
            let body = serde_json::json!({
                "kind": kind,
                "rule": alert.rule,
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
            self.metrics
                .count(DELIVERIES, &[("target", "mail"), ("outcome", outcome)]);
        }
    }
}

/// The message as sendmail reads it from stdin.
pub fn mail_body(targets: &Targets, cell: &str, kind: &str, alert: &Alert) -> String {
    format!(
        "From: {}\r\nTo: {}\r\nSubject: [velstra {cell}] {} {} {}\r\n\r\n{}\r\n",
        targets.mail_from,
        targets.mail_to.join(", "),
        kind.to_uppercase(),
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
        let alerts = evaluate(&nodes, &[], &[], &[], &Rules::default(), NOW);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0].rule, "node-silent");
        assert_eq!(alerts[0].subject, "nodes/quiet");
        assert!(alerts[0].message.contains("120 s"));
    }

    #[test]
    fn a_pool_at_the_threshold_fires_and_one_below_does_not() {
        let pools = [
            pool("full", 100, 80),
            pool("fine", 100, 79),
            pool("unsized", 0, 0),
        ];
        let alerts = evaluate(&[], &pools, &[], &[], &Rules::default(), NOW);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].subject, "pools/full");
        assert!(alerts[0].message.contains("80 %"));
        let strict = Rules {
            pool_full_percent: 50,
            ..Rules::default()
        };
        assert_eq!(evaluate(&[], &pools, &[], &[], &strict, NOW).len(), 2);
    }

    #[test]
    fn an_exhausted_quota_names_the_dimension_and_an_unlimited_one_never_fires() {
        let projects = [
            project("tight", 2, 2),
            project("roomy", 2, 1),
            project("free", 0, 9),
        ];
        let alerts = evaluate(&[], &[], &projects, &[], &Rules::default(), NOW);
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
        let alerts = evaluate(&[], &[], &[], &divergent, &Rules::default(), NOW);
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
        assert_eq!(metrics.get(FIRING, &[("rule", "stuck")]), Some(1.0));

        let second = n.observe(vec![a.clone()], false).await;
        assert!(second.fired.is_empty());
        assert_eq!(second.resolved, vec![b.clone()]);
        assert_eq!(
            metrics.get(FIRING, &[("rule", "pool-nearly-full")]),
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
        assert!(body.contains("Subject: [velstra cell-1] FIRING node-silent nodes/hv-2\r\n\r\n"));
        assert!(body.ends_with("hv-2 has not reported\r\n"));
    }
}
