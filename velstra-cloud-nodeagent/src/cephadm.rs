//! Running the steps that build a Ceph cluster.
//!
//! The dumb half of the deployment. What to do next is
//! [`velstra_cloud_model::ceph::next_step`]; this turns one of those into a
//! command and runs it, and knows nothing about order, retries or state.
//!
//! ## `cephadm`, not packages
//!
//! Proxmox installs Ceph as distribution packages because Proxmox owns the
//! distribution. This platform does not, and a deployer that shelled out to
//! `apt` on Debian, `dnf` on Rocky and `pacman` somewhere else would be three
//! code paths, two of which are never run.
//!
//! `cephadm` is upstream's own answer to exactly that: one small Python script,
//! containers for the daemons, and the same commands on every distribution that
//! can run a container. So the platform's job is to make sure `cephadm` is on
//! the node and then to drive it — and "make sure it is there" is deliberately
//! **not** something this does. See below.
//!
//! ## Why this does not install anything
//!
//! Fetching a script over the network and running it as root is the single most
//! dangerous thing a control plane could do on somebody's behalf, and doing it
//! because a checkbox in a browser was ticked is worse. So the agent *reports*
//! whether `cephadm` is present ([`installed`]) and every step that needs it is
//! blocked by name until it is.
//!
//! Putting it there is one command an operator runs once, from their own
//! package manager or upstream's installer, and the console says which. That is
//! a worse experience than Proxmox's one-click and a much better one than a
//! platform that can silently install arbitrary software on every machine it
//! manages.
//!
//! ## What is tested here
//!
//! The argv for every step, and the parsing of what Ceph reports back. Whether
//! `cephadm bootstrap` builds a cluster is not something a test without a
//! cluster can say, and a mock that agreed with itself would be worse than the
//! gap it hides.

use std::sync::{Arc, Mutex};

use velstra_cloud_model::ceph::{CephPoolSpec, NodeCeph};

use crate::host::{HostError, Result};

/// How this agent runs Ceph's own tools.
#[derive(Clone, Debug)]
pub struct CephAdmin {
    /// The `cephadm` binary.
    pub cephadm: String,
    /// The `ceph` binary, for everything after bootstrap.
    pub ceph: String,
    /// `systemctl`, for the one question the cluster cannot answer: which
    /// daemons are running *here*. A node that has not been added yet has no
    /// keyring and cannot ask the cluster anything, and that is exactly the
    /// node whose daemons somebody is waiting on.
    pub systemctl: String,
    /// The cluster's own configuration file.
    ///
    /// Read for one question — does this machine already hold a cluster — and a
    /// field for the same reason the others are: a test has to be able to say
    /// yes and no without one existing.
    pub ceph_conf: String,
    /// Where root's trusted keys live.
    ///
    /// A field rather than a constant so a test can point it somewhere else: a
    /// test that appended to the real `/root/.ssh/authorized_keys` would be a
    /// test that changes who can log into the machine running it.
    pub authorized_keys: String,
    /// The last spawn failure already reported by [`Self::installed`], so that a
    /// standing condition is stated once instead of once per observation pass.
    ///
    /// Shared across clones on purpose: the agent clones this struct per pass,
    /// and per-clone state would make every pass the first one and reinstate
    /// exactly the flood this exists to stop.
    reported_spawn_failure: Arc<Mutex<Option<String>>>,
}

impl Default for CephAdmin {
    fn default() -> Self {
        Self {
            cephadm: "cephadm".to_string(),
            ceph: "ceph".to_string(),
            systemctl: "systemctl".to_string(),
            ceph_conf: "/etc/ceph/ceph.conf".to_string(),
            authorized_keys: "/root/.ssh/authorized_keys".to_string(),
            reported_spawn_failure: Arc::new(Mutex::new(None)),
        }
    }
}

/// The argv for bootstrapping a cluster on this node.
///
/// `--skip-monitoring-stack` on purpose: the default drags in Prometheus,
/// Grafana, Alertmanager and node-exporter containers on every host. That is a
/// monitoring decision, this platform has its own opinions about observability,
/// and installing four services nobody asked for because they came bundled is
/// not a decision to make on somebody's behalf.
///
/// `--single-host-defaults` when there is one monitor: without it a one-node
/// cluster comes up `HEALTH_WARN` forever, because the default CRUSH rule wants
/// replicas on distinct hosts and there is one host. An operator building a lab
/// then debugs a warning that is telling them the truth about a cluster they
/// deliberately built.
pub fn bootstrap_argv(
    mon_ip: &str,
    public_network: &str,
    cluster_network: &str,
    single_host: bool,
) -> Vec<String> {
    let mut argv = vec![
        "bootstrap".to_string(),
        "--mon-ip".to_string(),
        mon_ip.to_string(),
        "--cluster-network".to_string(),
        if cluster_network.is_empty() {
            public_network.to_string()
        } else {
            cluster_network.to_string()
        },
        // Nothing this platform does needs a dashboard, and one more listening
        // service with its own credentials is one more thing to secure.
        "--skip-dashboard".to_string(),
        "--skip-monitoring-stack".to_string(),
        // The console is the interface; a second one nobody maintains is worse
        // than none.
        "--skip-firewalld".to_string(),
    ];
    if single_host {
        argv.push("--single-host-defaults".to_string());
    }
    argv
}

/// The argv for adding a host to the cluster.
///
/// Adding the host comes before placing a daemon on it: `ceph orch` can only
/// place where it has an inventory, and a monitor asked for on a host the
/// orchestrator has never seen fails with a message about placement rather than
/// about the host.
/// `_admin` on every monitor is deliberate: cephadm copies `ceph.conf` and the
/// admin keyring to hosts carrying that label, and without it only the node that
/// bootstrapped can administer the cluster. Losing that one machine would then
/// mean losing the ability to add a disk to a cluster that is otherwise fine.
pub fn add_host_argv(host: &str, address: &str, admin: bool) -> Vec<String> {
    let mut argv = vec![
        "orch".into(),
        "host".into(),
        "add".into(),
        host.to_string(),
        address.to_string(),
    ];
    if admin {
        argv.push("--labels=_admin".into());
    }
    argv
}

/// The argv for asking the cluster for the SSH key it drives hosts with.
pub fn pubkey_argv() -> Vec<String> {
    vec!["cephadm".into(), "get-pub-key".into()]
}

/// The argv for listing the hosts the orchestrator knows.
pub fn host_ls_argv() -> Vec<String> {
    vec![
        "orch".into(),
        "host".into(),
        "ls".into(),
        "--format".into(),
        "json".into(),
    ]
}

/// One row of `ceph orch host ls --format json`.
#[derive(serde::Deserialize)]
struct HostRow {
    #[serde(default)]
    hostname: String,
}

/// The host names out of `ceph orch host ls --format json`.
pub fn parse_hosts(json: &str) -> Result<Vec<String>> {
    let rows: Vec<HostRow> = serde_json::from_str(json).map_err(|e| {
        HostError::failed(format!("`ceph orch host ls` did not answer with json: {e}"))
    })?;
    Ok(rows.into_iter().map(|r| r.hostname).collect())
}

/// The argv for placing a monitor on a set of hosts.
///
/// `ceph orch apply mon` takes the **whole** list every time, and that is not a
/// wart: it is a declarative placement, so handing it the current set is exactly
/// the level-triggered shape the rest of this platform uses. Adding one at a
/// time with `--daemon-type` would be a sequence of commands whose result
/// depends on what ran before.
pub fn apply_mon_argv(hosts: &[String]) -> Vec<String> {
    vec![
        "orch".into(),
        "apply".into(),
        "mon".into(),
        format!("--placement={}", hosts.join(",")),
    ]
}

/// The argv for making an OSD of one device.
///
/// **This erases the device.** The safety is upstream of here, in
/// [`velstra_cloud_model::ceph::may_consume`] and in the console that will not
/// offer a disk it refuses — by the time this runs, the decision has been made
/// twice and confirmed once.
pub fn add_osd_argv(host: &str, device: &str) -> Vec<String> {
    vec![
        "orch".into(),
        "daemon".into(),
        "add".into(),
        "osd".into(),
        format!("{host}:{device}"),
    ]
}

/// The argv for creating a pool and its two size rules.
///
/// Three commands rather than one, because `ceph osd pool create` takes neither
/// `size` nor `min_size`. Returned together so a caller cannot create the pool
/// and forget the rules — a pool at the cluster default is a pool whose
/// durability is whatever somebody set globally, which is exactly the thing the
/// spec was written to pin.
pub fn create_pool_argv(pool: &CephPoolSpec) -> Vec<Vec<String>> {
    vec![
        vec![
            "osd".into(),
            "pool".into(),
            "create".into(),
            pool.pool.clone(),
        ],
        vec![
            "osd".into(),
            "pool".into(),
            "set".into(),
            pool.pool.clone(),
            "size".into(),
            pool.size.to_string(),
        ],
        vec![
            "osd".into(),
            "pool".into(),
            "set".into(),
            pool.pool.clone(),
            "min_size".into(),
            pool.min_size.to_string(),
        ],
        // Without this, `ceph health` reports `application not enabled on pool`
        // for ever — a warning that is correct, permanent, and drowns the ones
        // that matter.
        vec![
            "osd".into(),
            "pool".into(),
            "application".into(),
            "enable".into(),
            pool.pool.clone(),
            "rbd".into(),
        ],
    ]
}

/// What `ceph orch ps --format json` says is running on this host.
#[derive(serde::Deserialize)]
struct Daemon {
    #[serde(default)]
    daemon_type: String,
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    status_desc: String,
}

/// Read one host's daemons out of `ceph orch ps --format json`.
///
/// Only daemons reported *running*: a monitor that is `stopped` or `error` is
/// not a monitor for quorum's sake, and counting it would have the deployment
/// believe it is finished while the cluster is short.
pub fn parse_daemons(host: &str, json: &str) -> Result<(bool, bool)> {
    let daemons: Vec<Daemon> = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`ceph orch ps` did not answer with json: {e}")))?;
    let running = |kind: &str| {
        daemons.iter().any(|d| {
            d.hostname == host
                && d.daemon_type == kind
                && d.status_desc.eq_ignore_ascii_case("running")
        })
    };
    Ok((running("mon"), running("mgr")))
}

/// One row of `ceph osd pool ls --format json`, which is a bare array of names.
pub fn parse_pools(json: &str) -> Result<Vec<String>> {
    serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`ceph osd pool ls` did not answer with json: {e}")))
}

impl CephAdmin {
    /// Whether the tooling is on this machine, and what it reports about itself.
    ///
    /// Absent is not an error: most nodes in most cells will never run Ceph, and
    /// an agent that failed its whole observation because `cephadm` is missing
    /// would take those nodes down for a feature they do not use.
    pub async fn installed(&self) -> NodeCeph {
        let version = tokio::process::Command::new(&self.cephadm)
            .arg("version")
            .output()
            .await;
        match version {
            Ok(out) if out.status.success() => {
                self.forget_spawn_failure();
                NodeCeph {
                    installed: true,
                    version: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    ..NodeCeph::default()
                }
            }
            // It ran and said no. Ordinary, and the reason this is not an
            // error: most nodes in most cells will never run Ceph.
            Ok(_) => {
                self.forget_spawn_failure();
                NodeCeph::default()
            }
            // It could not be run at all, which is a different thing and is
            // worth saying. The answer stays `false`, because a machine that
            // cannot spawn a process is not one to hand a cluster to — but
            // "does not have Ceph installed yet", which is what the deployment
            // will report from here, is then the wrong sentence about the right
            // machine, and this line is the only place the truth appears.
            //
            // Said once per spell of it, not once per pass: observation runs on
            // a loop, and a standing condition logged every pass buries the
            // events worth reading — including the one that ends this spell.
            Err(e) => {
                if self.spawn_failure_is_news(&e) {
                    tracing::warn!(
                        error = %e,
                        binary = %self.cephadm,
                        "could not run cephadm at all; reporting this node as not having it. \
                         Repeats are not logged until this changes."
                    );
                }
                NodeCeph::default()
            }
        }
    }

    /// Whether this spawn failure is worth a line, and remember it if so.
    ///
    /// News means the situation changed: the first failure of a spell, or a
    /// different failure than the one standing (`permission denied` after a
    /// spell of `not found` is a different fact about the machine and is not
    /// swallowed as a repeat).
    fn spawn_failure_is_news(&self, e: &std::io::Error) -> bool {
        let now = e.to_string();
        let mut last = self.reported_spawn_failure.lock().unwrap_or_else(|p| {
            // A poisoned lock here means some other thread panicked while
            // holding it. That is not a reason to take the agent down over a
            // log-suppression detail, and the worst case of carrying on is one
            // extra line.
            self.reported_spawn_failure.clear_poison();
            p.into_inner()
        });
        if last.as_deref() == Some(now.as_str()) {
            return false;
        }
        *last = Some(now);
        true
    }

    /// Forget any standing spawn failure, so the next one is news again.
    ///
    /// Called on every pass where `cephadm` ran — including the passes where it
    /// ran and said no — because both mean the machine can spawn it, which is
    /// the condition the warning was about.
    fn forget_spawn_failure(&self) {
        let mut last = self.reported_spawn_failure.lock().unwrap_or_else(|p| {
            self.reported_spawn_failure.clear_poison();
            p.into_inner()
        });
        *last = None;
    }

    async fn ceph(&self, args: &[String]) -> Result<Vec<u8>> {
        let out = tokio::process::Command::new(&self.ceph)
            .args(args)
            .output()
            .await
            .map_err(|e| HostError::failed(format!("running `ceph {}`: {e}", args.join(" "))))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        Err(HostError::failed(format!(
            "`ceph {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }

    pub async fn pools(&self) -> Result<Vec<String>> {
        let out = self
            .ceph(&[
                "osd".into(),
                "pool".into(),
                "ls".into(),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        parse_pools(&String::from_utf8_lossy(&out))
    }

    pub async fn create_pool(&self, pool: &CephPoolSpec) -> Result<()> {
        for argv in create_pool_argv(pool) {
            self.ceph(&argv).await?;
        }
        Ok(())
    }

    pub async fn add_osd(&self, host: &str, device: &str) -> Result<()> {
        self.ceph(&add_osd_argv(host, device)).await.map(|_| ())
    }

    pub async fn apply_monitors(&self, hosts: &[String]) -> Result<()> {
        self.ceph(&apply_mon_argv(hosts)).await.map(|_| ())
    }

    pub async fn add_host(&self, host: &str, address: &str, admin: bool) -> Result<()> {
        self.ceph(&add_host_argv(host, address, admin))
            .await
            .map(|_| ())
    }

    /// Create the cluster here.
    ///
    /// The one command in this file that is not safely repeatable: run twice, it
    /// makes a second cluster on top of the first, and there is no undo for
    /// that. What keeps it to once is upstream —
    /// [`velstra_cloud_model::ceph::next_step`] only ever returns `Bootstrap`
    /// while no monitor is reported anywhere.
    pub async fn bootstrap(
        &self,
        mon_ip: &str,
        public_network: &str,
        cluster_network: &str,
        single_host: bool,
    ) -> Result<()> {
        let argv = bootstrap_argv(mon_ip, public_network, cluster_network, single_host);
        let out = tokio::process::Command::new(&self.cephadm)
            .args(&argv)
            .output()
            .await
            .map_err(|e| HostError::failed(format!("running `cephadm bootstrap`: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        Err(HostError::failed(format!(
            "`cephadm bootstrap` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }

    /// Whether this machine already holds a cluster.
    ///
    /// Asked of the disk rather than of a daemon, because the question is "has
    /// a cluster been created here" and a daemon's run state answers a
    /// different one: a monitor that is restarting is not a machine without a
    /// cluster. `cephadm bootstrap` writes this file, and it survives every
    /// restart, crash and reboot that a `systemctl` reading does not.
    pub async fn has_cluster(&self) -> bool {
        tokio::fs::metadata(&self.ceph_conf).await.is_ok()
    }

    /// The SSH public key the cluster drives its hosts with.
    pub async fn pubkey(&self) -> Result<String> {
        let out = self.ceph(&pubkey_argv()).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// The hosts the orchestrator knows about.
    pub async fn hosts(&self) -> Result<Vec<String>> {
        let out = self.ceph(&host_ls_argv()).await?;
        parse_hosts(&String::from_utf8_lossy(&out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-node cluster is a legitimate thing to build, and without
    /// `--single-host-defaults` it comes up warning for ever about a rule that
    /// cannot be satisfied.
    #[test]
    fn a_single_node_cluster_is_told_it_is_one() {
        let argv = bootstrap_argv("10.0.0.1", "10.0.0.0/24", "", true);
        assert!(
            argv.contains(&"--single-host-defaults".to_string()),
            "{argv:?}"
        );
        // And a real one is not, because then the default rule is the right one.
        let many = bootstrap_argv("10.0.0.1", "10.0.0.0/24", "", false);
        assert!(!many.contains(&"--single-host-defaults".to_string()));
    }

    #[test]
    fn nothing_nobody_asked_for_is_installed_alongside() {
        let argv = bootstrap_argv("10.0.0.1", "10.0.0.0/24", "", false);
        // Four containers per host of monitoring nobody chose, and a second web
        // interface with its own credentials.
        for skipped in ["--skip-monitoring-stack", "--skip-dashboard"] {
            assert!(
                argv.contains(&skipped.to_string()),
                "{skipped} was not skipped"
            );
        }
    }

    #[test]
    fn a_cluster_network_defaults_to_the_public_one_rather_than_to_nothing() {
        // Ceph without a cluster network puts replication on the public one,
        // which is the ordinary small-cluster answer — but it has to be *said*,
        // or bootstrap picks by itself from the node's routing table.
        let argv = bootstrap_argv("10.0.0.1", "10.0.0.0/24", "", false);
        let at = argv.iter().position(|a| a == "--cluster-network").unwrap();
        assert_eq!(argv[at + 1], "10.0.0.0/24");

        let split = bootstrap_argv("10.0.0.1", "10.0.0.0/24", "10.1.0.0/24", false);
        let at = split.iter().position(|a| a == "--cluster-network").unwrap();
        assert_eq!(split[at + 1], "10.1.0.0/24");
    }

    /// Monitor placement is declarative: the whole set, every time.
    #[test]
    fn monitors_are_placed_as_a_set_rather_than_one_at_a_time() {
        let argv = apply_mon_argv(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(argv.last().unwrap(), "--placement=a,b,c");
        // Which means asking twice with the same set is asking once — the same
        // level-triggered property the rest of the platform has.
        assert_eq!(argv, apply_mon_argv(&["a".into(), "b".into(), "c".into()]));
    }

    #[test]
    fn an_osd_names_the_host_and_the_device_it_will_erase() {
        assert_eq!(
            add_osd_argv("hv-1", "/dev/disk/by-id/wwn-0x5000")
                .last()
                .unwrap(),
            "hv-1:/dev/disk/by-id/wwn-0x5000"
        );
    }

    /// A pool is created *and* given its durability, in one call, because a pool
    /// at the cluster default is a pool whose durability is whatever somebody
    /// set globally — which is the thing the spec exists to pin.
    #[test]
    fn creating_a_pool_also_sets_what_makes_it_durable() {
        let pool = CephPoolSpec {
            pool: "velstra-volumes".into(),
            size: 3,
            min_size: 2,
        };
        let commands = create_pool_argv(&pool);
        let flat: Vec<String> = commands.iter().map(|c| c.join(" ")).collect();
        assert!(
            flat.iter()
                .any(|c| c.starts_with("osd pool create velstra-volumes")),
            "{flat:?}"
        );
        assert!(flat.iter().any(|c| c.ends_with("size 3")), "{flat:?}");
        assert!(flat.iter().any(|c| c.ends_with("min_size 2")), "{flat:?}");
        // And the application tag, without which `ceph health` warns for ever
        // and drowns the warnings that matter.
        assert!(
            flat.iter().any(|c| c.contains("application enable")),
            "{flat:?}"
        );
    }

    /// Only daemons actually running count.
    ///
    /// A monitor that is `stopped` or `error` is not a monitor for quorum's
    /// sake, and counting it would have the deployment believe it is finished
    /// while the cluster is one short of a quorum.
    #[test]
    fn a_daemon_that_is_not_running_is_not_counted() {
        let json = r#"[
            {"daemon_type":"mon","hostname":"hv-1","status_desc":"running"},
            {"daemon_type":"mgr","hostname":"hv-1","status_desc":"error"},
            {"daemon_type":"mon","hostname":"hv-2","status_desc":"running"}
        ]"#;
        assert_eq!(parse_daemons("hv-1", json).unwrap(), (true, false));
        assert_eq!(parse_daemons("hv-2", json).unwrap(), (true, false));
        // A host with nothing on it, and a host nobody has heard of, answer the
        // same — which is correct: neither is running anything.
        assert_eq!(parse_daemons("hv-9", json).unwrap(), (false, false));
    }

    /// A node without Ceph says so once, not once per observation pass.
    ///
    /// The regression this pins: the agent observes on a loop, and the missing
    /// binary was warned about on every pass — several lines a second in a VM
    /// test, and a log nobody can read anything else out of on a real node.
    #[test]
    fn a_standing_spawn_failure_is_reported_once_rather_than_every_pass() {
        let admin = CephAdmin::default();
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);

        assert!(admin.spawn_failure_is_news(&missing()), "the first one is");
        for _ in 0..100 {
            assert!(!admin.spawn_failure_is_news(&missing()), "repeats are not");
        }

        // A *different* failure is a different fact about the machine, and is
        // not swallowed as a repeat of the standing one.
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(admin.spawn_failure_is_news(&denied));
        assert!(!admin.spawn_failure_is_news(&denied));

        // And once it can be run again, the next spell is news on its own.
        admin.forget_spawn_failure();
        assert!(admin.spawn_failure_is_news(&missing()));
    }

    /// Clones share the suppression, because the agent clones per pass.
    ///
    /// Without this the flood comes back wearing a different hat: every pass
    /// would hold a fresh clone, every clone's first failure would be news, and
    /// the test above would keep passing while the log filled up anyway.
    #[test]
    fn clones_do_not_each_get_a_first_time() {
        let admin = CephAdmin::default();
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);

        assert!(admin.spawn_failure_is_news(&missing()));
        assert!(!admin.clone().spawn_failure_is_news(&missing()));

        // ...and a clone that sees it recover clears it for everyone holding
        // the same node's tools, rather than only for itself.
        admin.clone().forget_spawn_failure();
        assert!(admin.spawn_failure_is_news(&missing()));
    }

    #[test]
    fn output_that_is_not_json_is_an_error_rather_than_a_cluster_with_nothing_in_it() {
        for junk in ["", "Error EPERM: access denied", "no valid command found"] {
            assert!(parse_daemons("hv-1", junk).is_err(), "{junk:?}");
            assert!(parse_pools(junk).is_err(), "{junk:?}");
        }
        assert_eq!(parse_pools(r#"["a","b"]"#).unwrap(), ["a", "b"]);
    }
}
