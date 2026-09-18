//! How this machine was installed, and therefore how it can be updated.
//!
//! A cell has three kinds of machine and only two of them can be moved to a
//! new build by the cell. Which kind a node is decides what a rollout hands it
//! — an image for the inactive slot, a package for `apt` — and whether it may
//! hand it anything at all. So it is reported, by the node, from the
//! filesystem and the partition table, and never taken from a label somebody
//! set: a package-installed machine that an operator had mislabelled as an
//! appliance would have its "inactive slot" written onto a disk that has no
//! slots.
//!
//! ## The three kinds
//!
//! * **Appliance** — `/etc/NIXOS` exists *and* the disk carries the A/B slot
//!   partitions the node image lays down. Updated by writing the inactive
//!   slot and rebooting; rolled back by the other slot.
//! * **Package** — no `/etc/NIXOS`, and `dpkg` knows `velstra-cloud`. Debian
//!   or Ubuntu, the same `.deb` on both. Updated by installing the package.
//! * **NixOs** — `/etc/NIXOS` and no slots: the platform's NixOS *module* on
//!   an operating system the operator manages with their own configuration.
//!   The cell cannot update it, and a rollout refuses it by name rather than
//!   skipping it silently — a fleet half on a new build with one row nobody
//!   explained is the failure that looks most like success.
//!
//! The decision is a pure function of facts the agent observes, so it is
//! tested here without a machine, and the observing half (`installed.rs` in
//! the node agent) does nothing but read files and run `lsblk` and `dpkg`.

use serde::{Deserialize, Serialize};

/// Which way this machine was put together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallKind {
    /// Not reported yet, or not decidable from what was observed. A node that
    /// has never reported is this, and so is one whose agent predates the
    /// field — which is why it is the default rather than a guess.
    #[default]
    Unknown,
    /// The sealed image, with A/B slots.
    Appliance,
    /// The Debian package, on Debian or Ubuntu.
    Package,
    /// The NixOS module on an operator-managed NixOS. Spelled on the wire
    /// the way the operating system spells itself where a person reads it.
    #[serde(rename = "NixOS")]
    NixOs,
}

/// What the machine reports about its own installation.
///
/// Every field is a fact read off the machine. `version` is the build's own
/// stamp — `/etc/velstra-release` on an appliance, the package version on
/// Debian — and is the thing a rollout compares against a release, which
/// `status.agentVersion` cannot be: that is the crate version and says
/// `0.1.0` for every build there has ever been.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    #[serde(default)]
    pub kind: InstallKind,
    /// The operating system, as `os-release` names it for a person:
    /// `Debian GNU/Linux 13 (trixie)`, `NixOS 25.11 (Xantusia)`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub distro: String,
    /// The platform build this machine runs. Empty when the machine has no
    /// stamp to read — an image older than the stamp, or a package the query
    /// could not find — which the console shows as unknown rather than as a
    /// version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The A/B slot in use, `a` or `b`. Appliance only; empty elsewhere.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slot: String,
}

/// What the node agent observed, as facts and not conclusions.
///
/// Kept apart from [`Installed`] so the conclusion is drawn in one place,
/// [`detect`], and the agent's half is only reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// `/etc/NIXOS` exists.
    pub nixos_marker: bool,
    /// The disk carries a partition of the sealed store's type — the A/B
    /// slots the image lays down. `lsblk -o PARTTYPE` against the GUID.
    pub has_slots: bool,
    /// What `dpkg-query -W -f '${Version}' velstra-cloud` answered, if it
    /// answered.
    pub package_version: Option<String>,
    /// `/etc/velstra-release`, the stamp the image carries.
    pub image_version: Option<String>,
    /// `PRETTY_NAME` from `/etc/os-release`.
    pub pretty_name: Option<String>,
    /// The active slot, if the agent could tell.
    pub active_slot: Option<String>,
}

/// The conclusion, drawn from facts and nothing else.
pub fn detect(seen: &Observed) -> Installed {
    let distro = seen
        .pretty_name
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    let tidy = |v: &Option<String>| v.clone().unwrap_or_default().trim().to_string();
    match (
        seen.nixos_marker,
        seen.has_slots,
        seen.package_version.is_some(),
    ) {
        // The marker and the slots: the image. Even if `dpkg` somehow answers
        // — it cannot on the image, but a fact is a fact — the slots decide,
        // because the slots are what an update writes.
        (true, true, _) => Installed {
            kind: InstallKind::Appliance,
            distro,
            version: tidy(&seen.image_version),
            slot: tidy(&seen.active_slot),
        },
        // The marker and no slots: somebody's own NixOS running the module.
        (true, false, _) => Installed {
            kind: InstallKind::NixOs,
            distro,
            version: tidy(&seen.image_version),
            slot: String::new(),
        },
        // No marker, and the package is known to dpkg.
        (false, _, true) => Installed {
            kind: InstallKind::Package,
            distro,
            version: tidy(&seen.package_version),
            slot: String::new(),
        },
        // No marker and no package: something this crate has not met. Said
        // as unknown rather than guessed, so a rollout refuses it by name.
        (false, _, false) => Installed {
            kind: InstallKind::Unknown,
            distro,
            version: String::new(),
            slot: String::new(),
        },
    }
}

impl Installed {
    /// Why a rollout may not touch this machine, if it may not.
    ///
    /// Said in a sentence the rollout puts on its own status beside the node's
    /// name, because the alternative — skipping the node — is a fleet half on
    /// a new build with one row nobody explained.
    pub fn rollout_refusal(&self) -> Option<String> {
        match self.kind {
            InstallKind::Appliance | InstallKind::Package => None,
            InstallKind::NixOs => Some(
                "this machine runs the NixOS module on an operating system its operator \
                 manages; it is updated by its own configuration, and the cell cannot do that \
                 for it"
                    .to_string(),
            ),
            InstallKind::Unknown => Some(
                "this machine has not reported how it was installed, so the cell does not know \
                 what to hand it — an image for a slot, or a package for apt — and will not \
                 guess"
                    .to_string(),
            ),
        }
    }

    /// Whether this is the build a release names.
    ///
    /// Exact on the version string. A rollout that treated `0.2.0` and
    /// `0.2.0+20260918.abc` as the same would declare a node done that is
    /// running yesterday's build of the same version, which is the case a
    /// rollout exists to catch.
    pub fn runs(&self, version: &str) -> bool {
        !self.version.is_empty() && self.version.trim() == version.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen() -> Observed {
        Observed {
            pretty_name: Some("Debian GNU/Linux 13 (trixie)".into()),
            ..Default::default()
        }
    }

    /// The image: marker and slots. The slots decide, because the slots are
    /// what an update writes.
    #[test]
    fn marker_and_slots_is_the_appliance() {
        let i = detect(&Observed {
            nixos_marker: true,
            has_slots: true,
            image_version: Some("0.2.0+20260918.c571d71\n".into()),
            active_slot: Some("a".into()),
            pretty_name: Some("NixOS 25.11 (Xantusia)".into()),
            ..Default::default()
        });
        assert_eq!(i.kind, InstallKind::Appliance);
        assert_eq!(i.version, "0.2.0+20260918.c571d71");
        assert_eq!(i.slot, "a");
        assert_eq!(i.distro, "NixOS 25.11 (Xantusia)");
        assert_eq!(i.rollout_refusal(), None);
    }

    /// The marker without slots is somebody's own NixOS, and the cell must
    /// say so rather than write a slot that is not there.
    #[test]
    fn marker_without_slots_is_a_nixos_module_and_is_refused_by_name() {
        let i = detect(&Observed {
            nixos_marker: true,
            has_slots: false,
            ..seen()
        });
        assert_eq!(i.kind, InstallKind::NixOs);
        let why = i.rollout_refusal().expect("refused");
        assert!(why.contains("own configuration"), "{why}");
    }

    /// No marker and dpkg knows the package: Debian or Ubuntu, the same deb.
    #[test]
    fn a_package_dpkg_knows_is_a_package() {
        let i = detect(&Observed {
            package_version: Some("0.1.0+20260918.c571d71".into()),
            ..seen()
        });
        assert_eq!(i.kind, InstallKind::Package);
        assert_eq!(i.version, "0.1.0+20260918.c571d71");
        assert_eq!(i.slot, "");
        assert_eq!(i.rollout_refusal(), None);
    }

    /// Nothing recognisable is unknown, not a guess — and a rollout refuses it
    /// by name rather than handing it an artefact for a shape it does not have.
    #[test]
    fn nothing_recognisable_is_unknown_and_refused() {
        let i = detect(&seen());
        assert_eq!(i.kind, InstallKind::Unknown);
        assert!(i.version.is_empty());
        assert!(i.rollout_refusal().is_some());
    }

    /// A node that has never reported reads as unknown — the default — so an
    /// agent older than this field is not mistaken for a machine of some kind.
    #[test]
    fn the_default_is_unknown() {
        assert_eq!(Installed::default().kind, InstallKind::Unknown);
        assert!(Installed::default().rollout_refusal().is_some());
    }

    /// Exact on the version: yesterday's build of the same version is not
    /// this release, which is the case a rollout exists to catch.
    #[test]
    fn runs_is_exact_on_the_build_stamp() {
        let i = Installed {
            version: "0.2.0+20260918.c571d71".into(),
            ..Default::default()
        };
        assert!(i.runs("0.2.0+20260918.c571d71"));
        assert!(i.runs(" 0.2.0+20260918.c571d71\n"));
        assert!(!i.runs("0.2.0"));
        assert!(!i.runs("0.2.0+20260917.681269c"));
        assert!(!Installed::default().runs(""), "no stamp runs nothing");
    }
}
