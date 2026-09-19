//! Moving this machine to what it is wanted to run.
//!
//! A rollout — or an operator, for one machine — writes `spec.wanted` on this
//! node: the release, the build, the one artefact for this machine's kind of
//! installation, and its digest. Everything the machine needs is in it, so
//! this reads its own node and nothing else. The four things only the machine
//! can do are done here, in this order, and each is a sentence on the node's
//! `Updating` condition while it happens:
//!
//! 1. **fetch** the file from its own cell, streamed to the state directory;
//! 2. **verify** it against the digest — a mismatch names both digests and
//!    keeps nothing;
//! 3. **apply** it the way its kind is applied: an appliance writes the image
//!    into its inactive slot (`velstra-cloud-node update`), a package runs
//!    `apt-get install`;
//! 4. **reboot**, for an appliance, or let the package's own `postinst`
//!    restart what runs — and report `installed` again on the way back up.
//!
//! It does **not** cordon, drain or decide when. Those are the rollout's, and a
//! node that decided them for itself would be a node that reboots under its
//! own guests. A failure is `Updating=False` with reason `Failed` and the
//! sentence, and the machine does not try again until it is wanted something
//! else: a rollout reads the sentence and stops, and an operator reads it and
//! knows what to fix.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use velstra_cloud_model::{
    installed::{InstallKind, Installed},
    meta::{Condition, ConditionStatus},
    release::{UPDATING, Wanted},
};

use crate::cell::CellReader;

/// How long after the slot is written the machine reboots: long enough for
/// the next status pass to say *rebooting*, so the rollout — and a person —
/// see why the machine went away.
const REBOOT_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Working { version: String, message: String },
    Applied { version: String, message: String },
    Failed { version: String, why: String },
}

/// What a pass should do about the build this machine is wanted to run.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Decision {
    /// Nothing is wanted; nothing to say.
    Nothing,
    /// Report this, and do nothing else.
    Say(ConditionStatus, &'static str, String),
    /// Start the work, and report that it started.
    Start,
}

/// The whole decision, from what is wanted, what runs, and what is under way.
fn decide(phase: &Phase, wanted: Option<&Wanted>, installed: &Installed) -> Decision {
    match phase {
        Phase::Working { message, .. } => {
            return Decision::Say(ConditionStatus::True, "Working", message.clone());
        }
        Phase::Applied { version, message } if !installed.runs(version) => {
            return Decision::Say(ConditionStatus::True, "Applied", message.clone());
        }
        _ => {}
    }
    let Some(wanted) = wanted else {
        return Decision::Nothing;
    };
    if installed.runs(&wanted.version) {
        return Decision::Say(
            ConditionStatus::False,
            "UpToDate",
            format!("on {}", wanted.version),
        );
    }
    match phase {
        Phase::Working { version, message } if *version == wanted.version => {
            Decision::Say(ConditionStatus::True, "Working", message.clone())
        }
        Phase::Applied { version, message } if *version == wanted.version => {
            Decision::Say(ConditionStatus::True, "Applied", message.clone())
        }
        Phase::Failed { version, why } if version.is_empty() || *version == wanted.version => {
            Decision::Say(ConditionStatus::False, "Failed", why.clone())
        }
        _ => Decision::Start,
    }
}

pub struct Updater {
    /// Where the fetched file lands: `<state>/updates/`.
    dir: PathBuf,
    phase: Arc<Mutex<Phase>>,
}

impl Updater {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            phase: Arc::new(Mutex::new(read_attempt(&dir))),
            dir,
        }
    }

    /// Once per status pass: what this machine is wanted to run, what it
    /// runs, and the condition to report — starting the work when there is
    /// work to start.
    pub fn consider(
        &self,
        wanted: Option<&Wanted>,
        installed: &Installed,
        cell: Arc<dyn CellReader>,
        generation: u64,
    ) -> Option<Condition> {
        let mut held = self.phase.lock().unwrap();
        let current = held.clone();
        match decide(&current, wanted, installed) {
            Decision::Nothing => {
                if !matches!(current, Phase::Working { .. }) {
                    if let Err(message) = clear_attempt(&self.dir) {
                        return Some(Condition::new(
                            UPDATING,
                            ConditionStatus::False,
                            "Failed",
                            &message,
                            generation,
                        ));
                    }
                    *held = Phase::Idle;
                }
                None
            }
            Decision::Say(status, reason, message) => {
                if reason == "UpToDate" {
                    if let Err(message) = clear_attempt(&self.dir) {
                        return Some(Condition::new(
                            UPDATING,
                            ConditionStatus::False,
                            "Failed",
                            &message,
                            generation,
                        ));
                    }
                    *held = Phase::Idle;
                }
                Some(Condition::new(
                    UPDATING, status, reason, &message, generation,
                ))
            }
            Decision::Start => {
                let wanted = wanted
                    .expect("a start is only decided for a wanted build")
                    .clone();
                let message = format!("fetching {}", wanted.file);
                *held = Phase::Working {
                    version: wanted.version.clone(),
                    message: message.clone(),
                };
                drop(held);
                let phase = self.phase.clone();
                let dir = self.dir.clone();
                let kind = installed.kind;
                tokio::spawn(async move {
                    let outcome = run(&dir, &wanted, kind, cell, &phase).await;
                    let mut phase = phase.lock().unwrap();
                    *phase = match outcome {
                        Ok(message) => Phase::Applied {
                            version: wanted.version.clone(),
                            message,
                        },
                        Err(why) => {
                            tracing::error!(%why, version = %wanted.version, "the update failed");
                            Phase::Failed {
                                version: wanted.version.clone(),
                                why,
                            }
                        }
                    };
                });
                Some(Condition::new(
                    UPDATING,
                    ConditionStatus::True,
                    "Fetching",
                    &message,
                    generation,
                ))
            }
        }
    }
}

/// An unfinished attempt must never be replayed on boot: the machine may
/// have just rolled back from that very image. Clearing wanted explicitly
/// acknowledges the failure and permits a deliberate retry.
fn read_attempt(dir: &Path) -> Phase {
    match std::fs::read(dir.join("attempt.json")) {
        Ok(bytes) => match serde_json::from_slice::<Wanted>(&bytes) {
            Ok(wanted) => Phase::Failed { version: wanted.version, why: "the previous update was interrupted or rolled back; clear wanted before retrying".into() },
            Err(_) => Phase::Failed { version: String::new(), why: "the update attempt record is unreadable; inspect and remove attempt.json before retrying".into() },
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Phase::Idle,
        Err(e) => Phase::Failed { version: String::new(), why: format!("cannot read the update attempt: {e}") },
    }
}

fn attempt_lock(dir: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("update.lock"))
        .map_err(|e| e.to_string())?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|e| format!("another update owns this machine: {e}"))?;
    Ok(lock)
}

fn clear_attempt(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let _lock = attempt_lock(dir)?;
    match std::fs::remove_file(dir.join("attempt.json")) {
        Ok(()) => std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot clear the update attempt: {e}")),
    }
}

fn record_attempt(dir: &Path, wanted: &Wanted) -> Result<std::fs::File, String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let lock = attempt_lock(dir)?;
    if let Ok(bytes) = std::fs::read(dir.join("attempt.json")) {
        let previous: Wanted = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if previous.version != wanted.version {
            std::fs::remove_file(dir.join("attempt.json")).map_err(|e| e.to_string())?;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(dir.join("attempt.json"))
        .map_err(|e| format!("an update attempt already exists or cannot be recorded: {e}"))?;
    file.write_all(&serde_json::to_vec(wanted).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(lock)
}

fn say(phase: &Arc<Mutex<Phase>>, version: &str, message: &str) {
    tracing::info!(version, message, "updating");
    *phase.lock().unwrap() = Phase::Working {
        version: version.to_string(),
        message: message.to_string(),
    };
}

/// Fetch, verify, apply. `Ok` is the sentence the machine reports until it is
/// back; `Err` is the sentence it reports instead of coming back.
async fn run(
    dir: &Path,
    wanted: &Wanted,
    kind: InstallKind,
    cell: Arc<dyn CellReader>,
    phase: &Arc<Mutex<Phase>>,
) -> Result<String, String> {
    // Refused before a byte is fetched: the kinds the cell does not update.
    match kind {
        InstallKind::Appliance | InstallKind::Package => {}
        InstallKind::NixOs => {
            return Err(
                "this machine runs the NixOS module on an operating system its operator \
                        manages; it is updated by its own configuration"
                    .into(),
            );
        }
        InstallKind::Unknown => {
            return Err(
                "this machine has not worked out how it was installed, so it does not \
                        know how to apply a build"
                    .into(),
            );
        }
    }
    let _lock = record_attempt(dir, wanted)?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("making {}: {e}", dir.display()))?;
    let file = dir.join(&wanted.file);
    let partial = dir.join(format!("{}.partial", wanted.file));
    let _ = tokio::fs::remove_file(&partial).await;

    say(phase, &wanted.version, &format!("fetching {}", wanted.file));
    cell.download(&wanted.path(), &partial)
        .await
        .map_err(|e| format!("fetching {}: {e}", wanted.file))?;

    say(
        phase,
        &wanted.version,
        &format!("verifying {}", wanted.file),
    );
    let got = crate::hostfs::hash_file(&partial, velstra_cloud_model::images::Algorithm::Sha256)
        .await
        .map_err(|e| format!("hashing {}: {e}", wanted.file))?;
    if got != wanted.sha256 {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(mismatch(&wanted.file, &wanted.sha256, &got));
    }
    tokio::fs::rename(&partial, &file)
        .await
        .map_err(|e| format!("moving {} into place: {e}", wanted.file))?;

    let outcome = match kind {
        InstallKind::Appliance => apply_image(&file, wanted, phase).await,
        InstallKind::Package => apply_package(&file, wanted, phase).await,
        InstallKind::NixOs | InstallKind::Unknown => unreachable!("refused above"),
    };
    // The file has done its work either way; a failed apply keeps the reason,
    // not the gigabyte.
    let _ = tokio::fs::remove_file(&file).await;
    outcome
}

fn mismatch(file: &str, wanted: &str, got: &str) -> String {
    format!(
        "{file} did not hash to what the release names: expected {wanted}, got {got}. Nothing was \
         applied and the file was discarded."
    )
}

/// The image into the inactive slot, then a reboot into it.
async fn apply_image(
    file: &Path,
    wanted: &Wanted,
    phase: &Arc<Mutex<Phase>>,
) -> Result<String, String> {
    let raw = if file.extension().is_some_and(|e| e == "zst") {
        say(phase, &wanted.version, "unpacking the image");
        let raw = file.with_extension("");
        let _ = tokio::fs::remove_file(&raw).await;
        run_tool(
            &["zstd", "/run/current-system/sw/bin/zstd", "/usr/bin/zstd"],
            &[
                "-d",
                "-f",
                "-q",
                "-o",
                &raw.to_string_lossy(),
                &file.to_string_lossy(),
            ],
        )
        .await
        .map_err(|why| format!("unpacking the image: {why}"))?;
        raw
    } else {
        file.to_path_buf()
    };
    say(
        phase,
        &wanted.version,
        "writing the image into the inactive slot",
    );
    let written = run_tool(
        &[
            "velstra-cloud-node",
            "/run/current-system/sw/bin/velstra-cloud-node",
            "/usr/bin/velstra-cloud-node",
        ],
        &["update", "--image", &raw.to_string_lossy()],
    )
    .await;
    if raw != file {
        let _ = tokio::fs::remove_file(&raw).await;
    }
    written.map_err(|why| format!("writing the inactive slot: {why}"))?;
    say(
        phase,
        &wanted.version,
        "written into the inactive slot; rebooting into it",
    );
    tokio::time::sleep(REBOOT_AFTER).await;
    run_tool(
        &[
            "systemctl",
            "/run/current-system/sw/bin/systemctl",
            "/usr/bin/systemctl",
        ],
        &["reboot"],
    )
    .await
    .map_err(|why| format!("the image was written but reboot failed: {why}"))?;
    Ok(format!(
        "written into the inactive slot; rebooting into {} (a boot that fails three times rolls \
         back)",
        wanted.version
    ))
}

/// The package, the way an operator installs one by hand. Its `postinst`
/// restarts the agents, this one included, so this task does not usually get
/// to return — the next agent reports `installed` on the new build.
async fn apply_package(
    file: &Path,
    wanted: &Wanted,
    phase: &Arc<Mutex<Phase>>,
) -> Result<String, String> {
    say(phase, &wanted.version, "installing the package");
    run_tool(
        &["apt-get", "/usr/bin/apt-get"],
        &[
            "-o",
            "DPkg::Options::=--force-confdef",
            "-o",
            "DPkg::Options::=--force-confold",
            "install",
            "-y",
            "--allow-downgrades",
            &file.to_string_lossy(),
        ],
    )
    .await
    .map_err(|why| format!("installing the package: {why}"))?;
    Ok(format!(
        "installed {}; the agents are restarting on it",
        wanted.version
    ))
}

/// The first of `candidates` that runs, with the tail of its stderr on failure.
async fn run_tool(candidates: &[&str], args: &[&str]) -> Result<(), String> {
    for tool in candidates {
        let output = match tokio::process::Command::new(tool)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .await
        {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{tool}: {e}")),
        };
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines[lines.len().saturating_sub(4)..].join(" / ");
        return Err(format!("{tool} exited {}: {}", output.status, tail.trim()));
    }
    Err(format!("{}: not found on this machine", candidates[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted() -> Wanted {
        Wanted {
            release: "releases/v0.2.0".into(),
            version: "0.2.0+20260918.c571d71".into(),
            file: "velstra-cloud_0.2.0+20260918.c571d71_amd64.deb".into(),
            sha256: "cd".repeat(32),
        }
    }

    fn running(version: &str) -> Installed {
        Installed {
            kind: InstallKind::Package,
            version: version.into(),
            ..Default::default()
        }
    }

    /// Nothing wanted is nothing said; wanted and already there is said as
    /// up to date; wanted and not there starts the work, once.
    #[test]
    fn the_work_starts_once_and_is_reported_while_it_runs() {
        let w = wanted();
        assert_eq!(
            decide(&Phase::Idle, None, &running("0.1.0")),
            Decision::Nothing
        );
        assert_eq!(
            decide(&Phase::Idle, Some(&w), &running("0.2.0+20260918.c571d71")),
            Decision::Say(
                ConditionStatus::False,
                "UpToDate",
                "on 0.2.0+20260918.c571d71".into()
            )
        );
        assert_eq!(
            decide(&Phase::Idle, Some(&w), &running("0.1.0")),
            Decision::Start
        );
        let working = Phase::Working {
            version: w.version.clone(),
            message: "fetching".into(),
        };
        assert_eq!(
            decide(&working, Some(&w), &running("0.1.0")),
            Decision::Say(ConditionStatus::True, "Working", "fetching".into())
        );
    }

    /// A failure is reported and not retried — until something else is wanted.
    #[test]
    fn a_failure_is_said_and_not_retried_until_the_want_changes() {
        let w = wanted();
        let failed = Phase::Failed {
            version: w.version.clone(),
            why: "the digest did not match".into(),
        };
        assert_eq!(
            decide(&failed, Some(&w), &running("0.1.0")),
            Decision::Say(
                ConditionStatus::False,
                "Failed",
                "the digest did not match".into()
            )
        );
        let other = Wanted {
            version: "0.2.1".into(),
            ..w.clone()
        };
        assert_eq!(
            decide(&failed, Some(&other), &running("0.1.0")),
            Decision::Start
        );
        // Back on the wanted build after all: said so, whatever went before.
        assert!(matches!(
            decide(&failed, Some(&w), &running(&w.version)),
            Decision::Say(ConditionStatus::False, "UpToDate", _)
        ));
    }

    #[test]
    fn a_mismatch_names_both_digests() {
        let why = mismatch("x.deb", "aa", "bb");
        assert!(
            why.contains("expected aa") && why.contains("got bb"),
            "{why}"
        );
    }
    #[test]
    fn another_target_cannot_interrupt_an_inflight_update() {
        let mut next = wanted();
        next.version = "other".into();
        let active = Phase::Working {
            version: "first".into(),
            message: "writing a slot".into(),
        };
        assert!(matches!(
            decide(&active, Some(&next), &running("old")),
            Decision::Say(..)
        ));
        assert!(matches!(
            decide(&active, None, &running("old")),
            Decision::Say(..)
        ));
        let applied = Phase::Applied {
            version: "first".into(),
            message: "rebooting".into(),
        };
        assert!(matches!(
            decide(&applied, Some(&next), &running("old")),
            Decision::Say(..)
        ));
    }

    #[test]
    fn an_interrupted_update_is_not_replayed_and_attempts_are_exclusive() {
        let dir =
            std::env::temp_dir().join(format!("velstra-update-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let w = wanted();
        let lock = record_attempt(&dir, &w).unwrap();
        let other = Wanted {
            version: "another".into(),
            ..w.clone()
        };
        assert!(record_attempt(&dir, &other).is_err());
        assert!(clear_attempt(&dir).is_err());
        assert!(dir.join("attempt.json").exists());
        drop(lock);
        let restarted = Updater::new(dir.clone());
        let phase = restarted.phase.lock().unwrap();
        assert!(matches!(
            decide(&phase, Some(&w), &running("old")),
            Decision::Say(ConditionStatus::False, "Failed", _)
        ));
        assert!(matches!(
            decide(&phase, Some(&w), &running(&w.version)),
            Decision::Say(ConditionStatus::False, "UpToDate", _)
        ));
        assert!(record_attempt(&dir, &w).is_err());
        clear_attempt(&dir).unwrap();
        assert_eq!(read_attempt(&dir), Phase::Idle);
        drop(record_attempt(&dir, &w).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
