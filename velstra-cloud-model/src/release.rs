//! A build a cell can move to, and where to get it.
//!
//! A release is a **channel**: a directory somewhere — a GitHub release, a web
//! server, a path on the control plane's own disk — holding the artefacts CI
//! publishes and the `SHA256SUMS` that names them. The cell reads the sums,
//! learns which build the channel carries, fetches the artefacts onto its own
//! disk and verifies each against its digest. From then on **the cell is the
//! channel for its nodes**: a rollout hands a node an artefact the cell holds,
//! and an install medium is cut from the installer the cell holds. Nothing on a
//! node ever reaches out past its own control plane, which is what lets a cell
//! with no route to the internet be upgraded from a directory somebody carried
//! in.
//!
//! The arithmetic is here and pure: what a checksums file says, which artefact
//! a file is and which build it carries, whether the files agree on one build,
//! and what a node of a given kind is handed. The controller that fetches and
//! the API that serves decide nothing of their own. See `docs/upgrading.md`.

use serde::{Deserialize, Serialize};

use crate::{
    installed::InstallKind,
    meta::{Condition, ConditionStatus, Timestamp},
};

/// The checksums file every channel carries, as `sha256sum` writes it.
pub const SUMS: &str = "SHA256SUMS";

/// The condition a release settles on: every artefact it names is on this cell
/// and verified, so a rollout or an install medium can be served from here.
pub const READY: &str = "Ready";

/// The condition a node reports while it moves to what it is wanted to run.
/// True with a reason while working — `Fetching`, `Verifying`, `Applying`,
/// `Rebooting` — and False with reason `Failed` and the sentence when it could
/// not, which is what the rollout reads to stop.
pub const UPDATING: &str = "Updating";

/// The three artefacts a release publishes, told apart by the name CI gives
/// each: `velstra-cloud_<build>_<arch>.deb`,
/// `velstra-cloud-node_<build>_<arch>.raw.zst` and
/// `velstra-cloud-installer_<build>_<arch>.iso`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtefactKind {
    /// The Debian package, for a node installed with `apt`.
    Package,
    /// The sealed node image, for an appliance's inactive slot.
    Image,
    /// The installer medium, for a machine that has nothing yet.
    Installer,
}

impl ArtefactKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Package => "velstra-cloud_",
            Self::Image => "velstra-cloud-node_",
            Self::Installer => "velstra-cloud-installer_",
        }
    }

    fn suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Package => &[".deb"],
            Self::Image => &[".raw.zst", ".raw"],
            Self::Installer => &[".iso"],
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Image => "image",
            Self::Installer => "installer",
        }
    }
}

/// One file in a channel: its name, the digest the sums name for it, and
/// whether this cell holds a copy that hashed to that digest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artefact {
    pub file: String,
    pub sha256: String,
    /// The bytes are on this cell and verified. A rollout and an install
    /// medium are served from here; until this is true they wait, and the
    /// release's `Ready` condition says what is still being fetched.
    #[serde(default)]
    pub fetched: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSpec {
    /// The channel: a directory holding the artefacts and `SHA256SUMS`, as an
    /// `https://`, `http://` or `file://` URL. A GitHub release's download
    /// directory is one; so is a directory an operator copied onto the control
    /// plane's disk. What the directory holds is what the release is — its
    /// version is read off the files, never typed.
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReleaseStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
    /// The build every artefact in the channel carries in its name — the
    /// string a node's `status.installed.version` reads once it runs this.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Artefact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<Artefact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installer: Option<Artefact>,
    /// When the channel's sums were last read. Zero until they were.
    #[serde(default, skip_serializing_if = "unset")]
    pub checked_at: Timestamp,
}

fn unset(at: &Timestamp) -> bool {
    at.0 == 0
}

impl ReleaseStatus {
    /// Every artefact the cell holds, verified.
    pub fn is_ready(&self) -> bool {
        self.conditions
            .iter()
            .any(|c| c.kind == READY && c.status == ConditionStatus::True)
    }

    /// Why it is not ready, in the words the condition carries.
    pub fn not_ready_because(&self) -> String {
        match self.conditions.iter().find(|c| c.kind == READY) {
            Some(c) if c.status != ConditionStatus::True => c.message.clone(),
            Some(_) => String::new(),
            None => "the channel has not been read yet".to_string(),
        }
    }

    /// The artefacts the channel named, in a fixed order.
    pub fn artefacts(&self) -> Vec<(ArtefactKind, &Artefact)> {
        [
            (ArtefactKind::Package, &self.package),
            (ArtefactKind::Image, &self.image),
            (ArtefactKind::Installer, &self.installer),
        ]
        .into_iter()
        .filter_map(|(kind, a)| a.as_ref().map(|a| (kind, a)))
        .collect()
    }

    /// What a machine of this kind is handed: the image for an appliance, the
    /// package for a Debian or Ubuntu node, nothing for the kinds the cell
    /// cannot update — which `Installed::rollout_refusal` says in words.
    pub fn artefact_for(&self, kind: InstallKind) -> Option<&Artefact> {
        match kind {
            InstallKind::Appliance => self.image.as_ref(),
            InstallKind::Package => self.package.as_ref(),
            InstallKind::NixOs | InstallKind::Unknown => None,
        }
    }
}

/// What a node should run: set by a rollout, or by hand for one machine.
///
/// Everything the machine needs is here, chosen for it — the artefact for its
/// own kind of installation, and the digest — so the agent reads its own Node
/// and nothing else: it fetches `file` from its cell
/// (`/api/v1/<release>/files/<file>`), verifies `sha256`, applies it the way
/// its kind is applied, and reports `installed` again on the way back. An
/// agent that had to read the release would be an agent with a permission it
/// has no other use for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wanted {
    /// The release, by name: `releases/v0.2.0`.
    pub release: String,
    /// The build it carries; what `status.installed.version` reads afterwards.
    pub version: String,
    /// The artefact for this node's kind, and what it must hash to.
    pub file: String,
    pub sha256: String,
}

impl Wanted {
    /// What a release hands a node of this kind — or nothing, when it has no
    /// artefact for that kind or the kind is not one the cell updates.
    pub fn for_node(release: &str, status: &ReleaseStatus, kind: InstallKind) -> Option<Wanted> {
        let artefact = status.artefact_for(kind)?;
        Some(Wanted {
            release: release.to_string(),
            version: status.version.clone(),
            file: artefact.file.clone(),
            sha256: artefact.sha256.clone(),
        })
    }

    /// Where the node fetches it: its own cell.
    pub fn path(&self) -> String {
        format!("/api/v1/{}/files/{}", self.release, self.file)
    }
}

/// What a channel offers, read off its sums.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Offered {
    pub version: String,
    pub image: Option<Artefact>,
    pub package: Option<Artefact>,
    pub installer: Option<Artefact>,
}

/// What a `SHA256SUMS` says: `<64 hex>  <file>` per line, as `sha256sum`
/// writes it — a `*` before the name, for binary mode, and a `./` before it,
/// from `sha256sum ./*`, are both tolerated. Anything else on a line — a
/// comment, a blank, a sha512 — is skipped rather than refused: the file is
/// read for the entries it has. A name with a path in it is skipped too: a
/// channel is one directory, and a sums file that reaches outside it is not
/// one this cell follows.
pub fn sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (hex, rest) = line.trim().split_once(|c: char| c.is_whitespace())?;
            if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let file = rest.trim_start().trim_start_matches('*');
            let file = file.strip_prefix("./").unwrap_or(file).trim();
            safe_file_name(file).then(|| (hex.to_ascii_lowercase(), file.to_string()))
        })
        .collect()
}

/// Which artefact a file is, and the build it carries:
/// `velstra-cloud_0.2.0+20260918.c571d71_amd64.deb` is the package for build
/// `0.2.0+20260918.c571d71`. Anything else is nothing this cell hands a node.
pub fn classify(file: &str) -> Option<(ArtefactKind, String)> {
    for kind in [
        ArtefactKind::Package,
        ArtefactKind::Image,
        ArtefactKind::Installer,
    ] {
        let Some(rest) = file.strip_prefix(kind.prefix()) else {
            continue;
        };
        // `<build>_<arch>.<ext>`: a build may hold `+` and `.`, never `_`.
        let Some((build, arch_ext)) = rest.rsplit_once('_') else {
            continue;
        };
        let known = kind
            .suffixes()
            .iter()
            .any(|s| arch_ext.ends_with(s) && arch_ext.len() > s.len());
        if build.is_empty() || !known {
            continue;
        }
        return Some((kind, build.to_string()));
    }
    None
}

/// The sums, read: every artefact recognised, and the one build they all
/// carry. A channel with two builds in it is refused rather than guessed at —
/// a release is one build, and a directory holding last month's image beside
/// this month's package would otherwise be a rollout that installs one
/// version on the appliances and another on the Debian machines.
pub fn offered(sums: &[(String, String)]) -> Result<Offered, String> {
    let mut out = Offered::default();
    for (sha256, file) in sums {
        let Some((kind, build)) = classify(file) else {
            continue;
        };
        if out.version.is_empty() {
            out.version = build.clone();
        } else if out.version != build {
            return Err(format!(
                "the channel carries two builds, {} and {build}, and a release is one build; \
                 put each in a directory of its own",
                out.version
            ));
        }
        let slot = match kind {
            ArtefactKind::Package => &mut out.package,
            ArtefactKind::Image => &mut out.image,
            ArtefactKind::Installer => &mut out.installer,
        };
        if let Some(already) = slot {
            return Err(format!(
                "the channel names two files as its {}: {} and {file}",
                kind.describe(),
                already.file
            ));
        }
        *slot = Some(Artefact {
            file: file.clone(),
            sha256: sha256.clone(),
            fetched: false,
        });
    }
    if out.version.is_empty() {
        return Err(format!(
            "{SUMS} names nothing this cell recognises; a release publishes \
             velstra-cloud_<build>_<arch>.deb, velstra-cloud-node_<build>_<arch>.raw.zst and \
             velstra-cloud-installer_<build>_<arch>.iso"
        ));
    }
    Ok(out)
}

/// A channel's file, addressed: the one place a slash is added.
pub fn file_url(channel: &str, file: &str) -> String {
    format!("{}/{file}", channel.trim_end_matches('/'))
}

/// A name that is safe to put under a directory, and nothing else: no path,
/// no `..`, nothing hidden, nothing empty. What the sums may name, what the
/// API may serve, and what a node may ask for are all held to this.
pub fn safe_file_name(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('.')
        && !file.contains('/')
        && !file.contains('\\')
        && !file.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEB: &str = "velstra-cloud_0.2.0+20260918.c571d71_amd64.deb";
    const IMG: &str = "velstra-cloud-node_0.2.0+20260918.c571d71_amd64.raw.zst";
    const ISO: &str = "velstra-cloud-installer_0.2.0+20260918.c571d71_amd64.iso";
    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const H3: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    /// As CI writes it: `sha256sum ./*.deb ./*.iso ./*.zst`.
    #[test]
    fn the_sums_are_read_the_way_sha256sum_writes_them() {
        let text = format!("{H1}  ./{DEB}\n{H2} *{IMG}\n\n# a comment\n{H3}  {ISO}\n");
        let read = sums(&text);
        assert_eq!(
            read,
            vec![
                (H1.into(), DEB.into()),
                (H2.into(), IMG.into()),
                (H3.into(), ISO.into())
            ]
        );
    }

    /// A line that is not a sha256 is skipped, and so is a name that reaches
    /// outside the directory — a channel is one directory.
    #[test]
    fn what_is_not_an_entry_is_skipped_not_refused() {
        let sha512 = "a".repeat(128);
        let text = format!(
            "{sha512}  {DEB}\n{H1}  ../{DEB}\n{H1}  sub/{DEB}\n{H1}  .hidden\n{H2}  {IMG}\n"
        );
        assert_eq!(sums(&text), vec![(H2.to_string(), IMG.to_string())]);
    }

    #[test]
    fn a_file_is_known_by_its_name_and_carries_its_build() {
        let build = "0.2.0+20260918.c571d71".to_string();
        assert_eq!(classify(DEB), Some((ArtefactKind::Package, build.clone())));
        assert_eq!(classify(IMG), Some((ArtefactKind::Image, build.clone())));
        assert_eq!(classify(ISO), Some((ArtefactKind::Installer, build)));
        assert_eq!(
            classify("velstra-cloud-node_0.2.0_arm64.raw"),
            Some((ArtefactKind::Image, "0.2.0".into()))
        );
        assert_eq!(classify("SHA256SUMS"), None);
        assert_eq!(classify("velstra-cloud_0.2.0_amd64.tar"), None);
        assert_eq!(classify("velstra-cloud-node_amd64.raw.zst"), None);
        assert_eq!(classify("velstra-cloud_.deb"), None);
    }

    /// The whole channel read at once: three artefacts, one build.
    #[test]
    fn a_channel_offers_one_build_in_three_shapes() {
        let read = offered(&[
            (H1.into(), DEB.into()),
            (H2.into(), IMG.into()),
            (H3.into(), ISO.into()),
            (H3.into(), "RELEASE_NOTES.md".into()),
        ])
        .expect("offered");
        assert_eq!(read.version, "0.2.0+20260918.c571d71");
        assert_eq!(read.package.as_ref().map(|a| a.sha256.as_str()), Some(H1));
        assert_eq!(read.image.as_ref().map(|a| a.file.as_str()), Some(IMG));
        assert_eq!(read.installer.as_ref().map(|a| a.sha256.as_str()), Some(H3));
        assert!(
            !read.image.unwrap().fetched,
            "nothing is fetched by being named"
        );
    }

    /// Two builds in one directory is not a release, and the refusal names
    /// both so somebody can split them.
    #[test]
    fn a_channel_with_two_builds_is_refused_by_name() {
        let why = offered(&[
            (H1.into(), DEB.into()),
            (
                H2.into(),
                "velstra-cloud-node_0.1.0+20260910.681269c_amd64.raw.zst".into(),
            ),
        ])
        .expect_err("refused");
        assert!(why.contains("0.2.0+20260918.c571d71"), "{why}");
        assert!(why.contains("0.1.0+20260910.681269c"), "{why}");
        let why = offered(&[
            (H1.into(), IMG.into()),
            (
                H2.into(),
                "velstra-cloud-node_0.2.0+20260918.c571d71_amd64.raw".into(),
            ),
        ])
        .expect_err("refused");
        assert!(why.contains("two files as its image"), "{why}");
        let why = offered(&[(H1.into(), "notes.txt".into())]).expect_err("refused");
        assert!(why.contains("names nothing this cell recognises"), "{why}");
    }

    /// What a node is handed follows from how it was installed, and the two
    /// kinds the cell cannot update are handed nothing.
    #[test]
    fn a_node_is_handed_the_artefact_for_its_own_kind() {
        let status = ReleaseStatus {
            version: "0.2.0".into(),
            image: Some(Artefact {
                file: IMG.into(),
                sha256: H2.into(),
                fetched: true,
            }),
            package: Some(Artefact {
                file: DEB.into(),
                sha256: H1.into(),
                fetched: true,
            }),
            ..Default::default()
        };
        let image = Wanted::for_node("releases/v0.2.0", &status, InstallKind::Appliance).unwrap();
        assert_eq!(image.file, IMG);
        assert_eq!(image.sha256, H2);
        assert_eq!(image.version, "0.2.0");
        assert_eq!(image.path(), format!("/api/v1/releases/v0.2.0/files/{IMG}"));
        let package = Wanted::for_node("releases/v0.2.0", &status, InstallKind::Package).unwrap();
        assert_eq!(package.file, DEB);
        assert_eq!(
            Wanted::for_node("releases/v0.2.0", &status, InstallKind::NixOs),
            None
        );
        assert_eq!(
            Wanted::for_node("releases/v0.2.0", &status, InstallKind::Unknown),
            None
        );
    }

    #[test]
    fn readiness_is_the_condition_and_its_words() {
        let mut status = ReleaseStatus::default();
        assert!(!status.is_ready());
        assert!(status.not_ready_because().contains("not been read"));
        status.conditions.push(Condition::new(
            READY,
            ConditionStatus::False,
            "Fetching",
            "fetching the image (2 of 3)",
            1,
        ));
        assert!(!status.is_ready());
        assert_eq!(status.not_ready_because(), "fetching the image (2 of 3)");
        status.conditions.clear();
        status.conditions.push(Condition::ready(1));
        assert!(status.is_ready());
        assert_eq!(status.not_ready_because(), "");
    }

    #[test]
    fn a_files_address_adds_one_slash() {
        assert_eq!(
            file_url("https://x.example/v0.2.0/", "SHA256SUMS"),
            "https://x.example/v0.2.0/SHA256SUMS"
        );
        assert_eq!(
            file_url("file:///var/lib/velstra/releases/v0.2.0", "a.deb"),
            "file:///var/lib/velstra/releases/v0.2.0/a.deb"
        );
    }

    #[test]
    fn only_a_plain_name_is_safe_under_a_directory() {
        assert!(safe_file_name(DEB));
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", ".hidden", "a\0b"] {
            assert!(!safe_file_name(bad), "{bad:?}");
        }
    }
}
