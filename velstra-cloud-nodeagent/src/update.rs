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
        Phase::Failed { version, why } if *version == wanted.version => {
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
            dir,
            phase: Arc::new(Mutex::new(Phase::Idle)),
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
        let current = self.phase.lock().unwrap().clone();
        match decide(&current, wanted, installed) {
            Decision::Nothing => {
                if !matches!(current, Phase::Working { .. }) {
                    *self.phase.lock().unwrap() = Phase::Idle;
                }
                None
            }
            Decision::Say(status, reason, message) => {
                if reason == "UpToDate" {
                    *self.phase.lock().unwrap() = Phase::Idle;
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
                *self.phase.lock().unwrap() = Phase::Working {
                    version: wanted.version.clone(),
                    message: message.clone(),
                };
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
    tokio::spawn(async {
        tokio::time::sleep(REBOOT_AFTER).await;
        let _ = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .status()
            .await;
    });
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
}
