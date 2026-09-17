//! A machine that has announced itself, and is waiting to be let in.
//!
//! ## The problem this solves
//!
//! Joining a cell needs one secret moved from the control plane to a machine
//! that has no credential yet. A join token does that well and has one
//! weakness: it is about 1.3 KB, and somebody standing at a console cannot
//! type it. A file on a stick fixes that ([`crate`]'s sibling story, in the
//! installer) and still needs somebody to carry a stick.
//!
//! So: turn the direction round. The machine generates a keypair, announces
//! itself to a cell address — the one short thing anybody types, and a DHCP
//! option can carry it — and shows a **fingerprint** on its screen. The same
//! fingerprint appears in the console beside what the machine says about
//! itself. An operator compares them, picks what the machine is for, and
//! approves. Nothing secret is typed in either direction.
//!
//! ## Why the comparison is the authentication
//!
//! Two questions have to be answered before a cell hands out a credential, and
//! one gesture answers both.
//!
//! *Is this the machine I am standing in front of?* The fingerprint is
//! [`fingerprint_of`] the public key, which only the machine holding the
//! private key can produce and which nothing on the wire can change without
//! changing the fingerprint. Two machines announcing at once are two rows with
//! two fingerprints, and the operator reads the one on the screen in front of
//! them.
//!
//! *Is this the cell I meant?* The machine has no CA yet, so it cannot verify
//! the API's certificate — it records what it was served and shows that
//! fingerprint too. The control plane's own console banner prints the same
//! value. A man in the middle has to show a certificate it holds the key for,
//! which is a different fingerprint, on the machine's own screen.
//!
//! Neither direction is proved by a secret. Both are proved by a human looking
//! at two screens, which is the same gesture as checking a certificate warning
//! and is the gesture MAAS and Proxmox both settled on.
//!
//! ## What it deliberately is not
//!
//! **Not attestation.** A machine that proves what it is in hardware is a
//! different design, and this is shaped so as not to preclude it: everything
//! here is about introducing a public key, and a key vouched for by a TPM
//! would enter the same door.
//!
//! **Not a replacement for the token.** This needs the network at install
//! time. A token on a medium does not, which is why both exist: one is the
//! door when somebody is watching, the other when nobody is — and only the
//! second works on a network that is not up yet.
//!
//! ## The facts are reported, the decision is declared
//!
//! `status` carries what the machine said about itself and `spec` carries what
//! an operator decided, which is this platform's rule everywhere else. The one
//! twist is that the reporter here has no credential — so the announce writes
//! a status once, through a door of its own, and nothing may write it again.

use serde::{Deserialize, Serialize};

use crate::meta::{Condition, Timestamp};

/// How long an unapproved announcement stands.
///
/// Long enough that somebody can walk from the machine to a desk, short enough
/// that a rack flashed by mistake does not leave forty rows to clean up. An
/// expired machine re-announces on its next pass, so the cost of being wrong
/// here is a row that comes back, not a machine that cannot join.
pub const DEFAULT_TTL_SECS: u64 = 60 * 60;

/// The most announcements a cell holds at once.
///
/// The announce door is unauthenticated — it has to be, the machine has no
/// credential — so it is the one place in this API where a stranger can make
/// the store grow. This is the ceiling that makes that bounded: past it, an
/// announcement is refused by name rather than accepted into a list nobody can
/// read. A rack is tens of machines; a thousand is somebody else.
pub const MAX_PENDING: usize = 512;

/// What an operator decided about a machine that announced itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentSpec {
    /// What the node will be called. Empty until an operator says, and then it
    /// is the name everything in the cell uses for this machine for ever.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// What the machine is for: `hypervisor`, `pool`, `control-plane`.
    ///
    /// The operator's answer and not the machine's. A machine that could
    /// nominate itself a control plane would be a machine that could nominate
    /// itself the cell — the same reason a node cannot make itself a gateway.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Which pool it serves, when `roles` names one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pool: String,
    /// Set by an operator to let the machine in. There is no `approved: false`
    /// that means "rejected" — that is [`EnrollmentSpec::refused`], because
    /// "not yet" and "no" are different answers and a machine polling on the
    /// first should stop on the second.
    #[serde(default)]
    pub approved: bool,
    /// Set by an operator to turn a machine away. The row stays, so somebody
    /// who wonders what happened to it can see.
    #[serde(default)]
    pub refused: bool,
}

/// What the machine said about itself, and where it has got to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentStatus {
    /// The public half of the keypair the machine generated, base64url, no
    /// padding. Written once, at the announce, and never again: it is what the
    /// fingerprint is computed from and what the claim is checked against.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub public_key: String,
    /// [`fingerprint_of`] the public key. Stored rather than computed on read
    /// so that what an operator compared is what was recorded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    /// The fingerprint of the certificate the machine was served when it
    /// announced. The machine shows this on its screen; the control plane's
    /// banner shows the same value for itself. They differ when somebody is in
    /// the middle.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seen_certificate: String,
    /// What the machine says it has. Unverified by construction — it is a
    /// stranger's description of itself — and useful anyway: it is how an
    /// operator tells two identical pending rows apart.
    #[serde(default)]
    pub reported: Reported,
    /// Where this has got to.
    #[serde(default)]
    pub phase: EnrollmentPhase,
    /// When an unapproved announcement stops standing.
    #[serde(default)]
    pub expires_at: Timestamp,
    /// Who approved this machine, recorded by the API the moment somebody did.
    ///
    /// In the **status** and not the spec, because the status is the
    /// platform's to write and nothing an operator sends can set it. The claim
    /// mints the node's credential as this person, so this field is what makes
    /// "a machine registered itself" untrue: it registered on the recorded
    /// authority of somebody who may already create nodes, once, for the one
    /// node they named. The audit line then carries a human.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub approved_by: String,
    /// When the credential was collected, if it has been.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<Timestamp>,
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// What a machine says about itself when it announces.
///
/// Every field is the machine's own claim. None of it is trusted for anything
/// — it decides nothing, grants nothing, and is not matched against a policy.
/// It exists so that a person looking at three pending rows can tell which one
/// is the box in front of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reported {
    /// What the machine calls itself right now — usually the image default,
    /// because nobody has named it yet.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    /// Addresses it answers on, so an operator can tell it apart from the
    /// identical box in the next slot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// Hardware, in the words a person recognises: `32 vCPU, 128 GiB, 2 disks`
    /// is built from these three.
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_mib: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<String>,
    /// The machine's own serial or asset tag, when the firmware has one. The
    /// most useful field on this whole object in a rack of identical boxes,
    /// and absent often enough that nothing may depend on it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub serial: String,
}

impl Reported {
    /// One line for a console row.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.vcpus > 0 {
            parts.push(format!("{} vCPU", self.vcpus));
        }
        if self.memory_mib > 0 {
            parts.push(format!("{} GiB", self.memory_mib / 1024));
        }
        if !self.disks.is_empty() {
            parts.push(format!(
                "{} disk{}",
                self.disks.len(),
                if self.disks.len() == 1 { "" } else { "s" }
            ));
        }
        if !self.serial.is_empty() {
            parts.push(format!("serial {}", self.serial));
        }
        if parts.is_empty() {
            // A machine that reported nothing is still a machine somebody is
            // standing in front of. Saying so beats an empty cell that reads
            // like a rendering bug.
            return "reported nothing about itself".to_string();
        }
        parts.join(", ")
    }
}

/// Where an announcement has got to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnrollmentPhase {
    /// Announced, and waiting for somebody to look at it.
    #[default]
    Pending,
    /// An operator said yes. The machine has not collected its credential yet.
    Approved,
    /// The credential has been collected. Nothing more happens here; the Node
    /// object is what the cell talks about from now on.
    Claimed,
    /// An operator said no.
    Refused,
    /// Nobody said anything for long enough. The machine re-announces.
    Expired,
}

impl EnrollmentPhase {
    /// Whether this is over, one way or another.
    ///
    /// What the expiry sweep and the console's default filter both ask: a
    /// finished row is history, and history does not belong in a list of
    /// things waiting for a decision.
    pub fn settled(self) -> bool {
        matches!(self, Self::Claimed | Self::Refused | Self::Expired)
    }
}

/// The length of an Ed25519 public key, in bytes.
const KEY_BYTES: usize = 32;

/// Why a key or a claim was not acceptable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BadKey {
    /// Not base64 at all, in any of the four spellings.
    NotBase64,
    /// Base64 of the wrong length to be an Ed25519 key.
    WrongLength(usize),
    /// A signature of the wrong length to be Ed25519.
    NotASignature(usize),
    /// It decoded and did not verify. Either another key made it, or it was
    /// made over something other than the message this cell asks for.
    DoesNotVerify,
}

impl std::fmt::Display for BadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBase64 => f.write_str("that is not base64"),
            Self::WrongLength(n) => write!(
                f,
                "an Ed25519 public key is {KEY_BYTES} bytes, this decodes to {n}"
            ),
            Self::NotASignature(n) => {
                write!(f, "an Ed25519 signature is 64 bytes, this decodes to {n}")
            }
            Self::DoesNotVerify => f.write_str(
                "the signature does not verify under the key this machine announced — either \
                 another key made it, or it was made over something other than the claim \
                 message",
            ),
        }
    }
}

/// One spelling for one key.
///
/// Base64 has four spellings of the same bytes — standard and url-safe,
/// padded and not — and the id a machine's row lives at is derived from the
/// *string*. Without this, a client that padded differently on its second
/// announce would land on a second row, and an operator would be looking at
/// two pending machines that are one machine. So every key is decoded at the
/// door and re-encoded one way, and everything downstream is a function of
/// that one spelling.
///
/// Standard base64 with padding, which is what `images.rs` already uses for
/// signing keys — two encodings in one codebase is one more than anybody can
/// keep straight.
pub fn canonical_key(offered: &str) -> Result<String, BadKey> {
    let raw = decode_any(offered)?;
    if raw.len() != KEY_BYTES {
        return Err(BadKey::WrongLength(raw.len()));
    }
    Ok(encode_standard(&raw))
}

/// Base64 in any of its four spellings.
///
/// Tried in turn rather than sniffed: the alphabets overlap, so a string that
/// is valid under two of them decodes to the same bytes under both, and one
/// that is valid under none is not base64. Cheap — this runs once per
/// announcement, over at most 512 characters.
fn decode_any(text: &str) -> Result<Vec<u8>, BadKey> {
    use base64::{Engine, engine::general_purpose as b64};
    let text = text.trim();
    if let Ok(raw) = b64::STANDARD.decode(text) {
        return Ok(raw);
    }
    if let Ok(raw) = b64::STANDARD_NO_PAD.decode(text) {
        return Ok(raw);
    }
    if let Ok(raw) = b64::URL_SAFE.decode(text) {
        return Ok(raw);
    }
    if let Ok(raw) = b64::URL_SAFE_NO_PAD.decode(text) {
        return Ok(raw);
    }
    Err(BadKey::NotBase64)
}

fn encode_standard(raw: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// What a machine signs to prove it holds the key it announced.
///
/// The enrolment's own id and nothing else. A nonce would be better against
/// replay and is not needed here: a claim succeeds once — the second is
/// [`NotClaimable::AlreadyClaimed`] — so a replayed signature buys an attacker
/// a refusal. Versioned, so a future message shape cannot be confused with
/// this one by a machine running older code.
pub fn claim_message(id: &str) -> Vec<u8> {
    format!("velstra-enrollment-claim:v1:{id}").into_bytes()
}

/// Whether this signature was made by the key that announced, over this
/// enrolment.
///
/// Pure, so the one piece of cryptography in this feature is tested against
/// real keys without a store or a network — see the tests below, which sign
/// with `ring` and check that the right signature passes, a signature over
/// another enrolment's id does not, and another key's does not.
pub fn verify_claim(public_key: &str, id: &str, signature: &str) -> Result<(), BadKey> {
    let key = decode_any(public_key)?;
    if key.len() != KEY_BYTES {
        return Err(BadKey::WrongLength(key.len()));
    }
    let sig = decode_any(signature)?;
    if sig.len() != 64 {
        return Err(BadKey::NotASignature(sig.len()));
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&claim_message(id), &sig)
        .map_err(|_| BadKey::DoesNotVerify)
}

/// The fingerprint of a public key, as a person compares it.
///
/// Eight bytes of SHA-256 over the key, in the colon-separated hex this
/// platform already prints for certificates — an operator who has checked one
/// certificate warning knows what to do with this without being told.
///
/// Eight and not four: the comparison is made against a value that does not
/// exist until the machine generates its key, so an attacker cannot prepare a
/// collision in advance — but 64 bits costs nothing and removes the argument.
/// Eight and not thirty-two: a fingerprint nobody reads to the end is a
/// fingerprint nobody compares.
pub fn fingerprint_of(public_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(public_key.trim().as_bytes());
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// What an announcement becomes when it is accepted.
///
/// Pure, so the rules are tested without a store: the phase a fresh
/// announcement starts in, when it stops standing, and the fingerprint an
/// operator will be asked to compare.
pub fn announced(
    public_key: &str,
    seen_certificate: &str,
    reported: Reported,
    now: u64,
) -> EnrollmentStatus {
    EnrollmentStatus {
        fingerprint: fingerprint_of(public_key),
        public_key: public_key.trim().to_string(),
        seen_certificate: seen_certificate.trim().to_string(),
        reported,
        phase: EnrollmentPhase::Pending,
        expires_at: Timestamp(now + DEFAULT_TTL_SECS * 1000),
        approved_by: String::new(),
        claimed_at: None,
        observed_generation: 0,
        conditions: Vec::new(),
    }
}

/// Why a claim was refused, when it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotClaimable {
    /// Nobody has approved it yet. The machine keeps asking.
    NotApproved,
    /// An operator said no. The machine stops asking.
    Refused,
    /// Already collected. A second claim is either a retry that lost its
    /// answer or somebody else holding the key; either way the credential is
    /// minted once.
    AlreadyClaimed,
    /// It stood too long. The machine announces again, which is cheap and is
    /// the only thing it can usefully do.
    Expired,
    /// Approved, and nobody said what the machine is for. A credential for no
    /// roles would make a machine that registers and does nothing.
    NoRoles,
    /// Approved, and nobody named it. The node id is what the whole cell calls
    /// this machine, and it cannot be filled in later.
    NoNode,
}

impl std::fmt::Display for NotClaimable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotApproved => {
                "nobody has approved this machine yet — compare the fingerprint on its screen \
                 with the one in the console, then approve it"
            }
            Self::Refused => "this machine was turned away",
            Self::AlreadyClaimed => {
                "this announcement's credential has already been collected; a machine that lost \
                 it announces again"
            }
            Self::Expired => "this announcement stood too long and has expired; announce again",
            Self::NoRoles => {
                "this machine was approved without being told what it is for, so there is no \
                 credential to mint — set spec.roles"
            }
            Self::NoNode => {
                "this machine was approved without being given a name, and the node id is what \
                 the whole cell calls it — set spec.node"
            }
        };
        f.write_str(text)
    }
}

/// Whether the machine holding this key may collect its credential now.
///
/// Pure and total: every refusal is a named reason a machine can print and an
/// operator can act on, because "the API said no" at three in the morning is
/// the failure this platform keeps removing.
pub fn claimable(
    spec: &EnrollmentSpec,
    status: &EnrollmentStatus,
    now: u64,
) -> Result<(), NotClaimable> {
    if status.phase == EnrollmentPhase::Claimed || status.claimed_at.is_some() {
        return Err(NotClaimable::AlreadyClaimed);
    }
    if spec.refused || status.phase == EnrollmentPhase::Refused {
        return Err(NotClaimable::Refused);
    }
    // Expiry is checked against the clock and not against the stored phase: a
    // sweep that has not run yet is not a reason to let a machine in.
    if status.expires_at.0 != 0 && now >= status.expires_at.0 {
        return Err(NotClaimable::Expired);
    }
    if !spec.approved {
        return Err(NotClaimable::NotApproved);
    }
    if spec.node.trim().is_empty() {
        return Err(NotClaimable::NoNode);
    }
    if spec.roles.is_empty() {
        return Err(NotClaimable::NoRoles);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "MCowBQYDK2VwAyEAGuRmKcBkVc0vY0Q2Bp0qK9R7wqX4f1cQ8Zb2mS3nT4o";

    fn pending(now: u64) -> (EnrollmentSpec, EnrollmentStatus) {
        (
            EnrollmentSpec::default(),
            announced(KEY, "AB:CD:EF", Reported::default(), now),
        )
    }

    /// The same key gives the same fingerprint, and a different one does not.
    /// This is the whole authentication, so it is the first thing tested.
    #[test]
    fn a_fingerprint_follows_the_key_and_nothing_else() {
        assert_eq!(fingerprint_of(KEY), fingerprint_of(KEY));
        assert_ne!(
            fingerprint_of(KEY),
            fingerprint_of("MCowBQYDK2VwAyEAdifferent")
        );
        // Whitespace around a pasted key is not a different key.
        assert_eq!(fingerprint_of(KEY), fingerprint_of(&format!("  {KEY}\n")));
    }

    /// Eight colon-separated hex pairs, which is what this platform already
    /// prints for certificates — somebody who has checked a certificate
    /// warning knows what to do with it without being told.
    #[test]
    fn a_fingerprint_is_shaped_like_the_certificate_ones() {
        let f = fingerprint_of(KEY);
        let groups: Vec<&str> = f.split(':').collect();
        assert_eq!(groups.len(), 8, "{f}");
        for g in groups {
            assert_eq!(g.len(), 2, "{f}");
            assert!(g.chars().all(|c| c.is_ascii_hexdigit()), "{f}");
            assert!(g.chars().all(|c| !c.is_ascii_lowercase()), "{f}");
        }
    }

    /// A fresh announcement is pending, stands for an hour, and carries the
    /// fingerprint an operator will be asked to compare.
    #[test]
    fn a_fresh_announcement_is_pending_and_stands_for_an_hour() {
        let now = 1_700_000_000_000;
        let (_, status) = pending(now);
        assert_eq!(status.phase, EnrollmentPhase::Pending);
        assert_eq!(status.expires_at.0, now + 3_600_000);
        assert_eq!(status.fingerprint, fingerprint_of(KEY));
        assert!(status.claimed_at.is_none());
    }

    /// Nobody has looked yet: the machine keeps asking, and is told why in
    /// words that say what to do about it.
    #[test]
    fn an_unapproved_machine_is_told_to_wait_and_why() {
        let now = 1_700_000_000_000;
        let (spec, status) = pending(now);
        let e = claimable(&spec, &status, now).expect_err("not yet");
        assert_eq!(e, NotClaimable::NotApproved);
        assert!(e.to_string().contains("fingerprint"), "{e}");
    }

    /// Approved, named, and told what it is for: in.
    #[test]
    fn an_approved_machine_with_a_name_and_a_role_may_claim() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = pending(now);
        spec.approved = true;
        spec.node = "peter".into();
        spec.roles = vec!["hypervisor".into()];
        assert_eq!(claimable(&spec, &status, now), Ok(()));
    }

    /// Approved and nothing else said. Both halves are refused by name,
    /// because a credential for no roles makes a machine that registers and
    /// does nothing, and a node id cannot be filled in afterwards.
    #[test]
    fn approving_without_deciding_anything_is_not_enough() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = pending(now);
        spec.approved = true;
        assert_eq!(
            claimable(&spec, &status, now),
            Err(NotClaimable::NoNode),
            "a machine with no name"
        );
        spec.node = "peter".into();
        assert_eq!(
            claimable(&spec, &status, now),
            Err(NotClaimable::NoRoles),
            "a machine with no purpose"
        );
    }

    /// "Not yet" and "no" are different answers, and a machine polling on the
    /// first has to stop on the second.
    #[test]
    fn a_refusal_is_not_a_slow_yes() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = pending(now);
        spec.refused = true;
        assert_eq!(claimable(&spec, &status, now), Err(NotClaimable::Refused));
    }

    /// The clock decides, not the stored phase: a sweep that has not run yet
    /// is not a reason to let a machine in.
    #[test]
    fn expiry_is_the_clock_and_not_the_phase() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = pending(now);
        spec.approved = true;
        spec.node = "peter".into();
        spec.roles = vec!["hypervisor".into()];
        assert_eq!(status.phase, EnrollmentPhase::Pending);
        assert_eq!(
            claimable(&spec, &status, now + 3_600_001),
            Err(NotClaimable::Expired)
        );
    }

    /// Minted once. A second claim is either a retry that lost its answer or
    /// somebody else holding the key, and the credential is not minted twice
    /// for either.
    #[test]
    fn a_credential_is_collected_once() {
        let now = 1_700_000_000_000;
        let (mut spec, mut status) = pending(now);
        spec.approved = true;
        spec.node = "peter".into();
        spec.roles = vec!["hypervisor".into()];
        status.claimed_at = Some(Timestamp(now));
        assert_eq!(
            claimable(&spec, &status, now),
            Err(NotClaimable::AlreadyClaimed)
        );
    }

    /// Claimed, refused and expired are over; pending and approved are not.
    /// The sweep and the console's default filter both ask this.
    #[test]
    fn what_counts_as_over() {
        assert!(!EnrollmentPhase::Pending.settled());
        assert!(!EnrollmentPhase::Approved.settled());
        assert!(EnrollmentPhase::Claimed.settled());
        assert!(EnrollmentPhase::Refused.settled());
        assert!(EnrollmentPhase::Expired.settled());
    }

    /// The row a person reads. A machine that reported nothing says so, rather
    /// than leaving a blank that looks like a rendering fault.
    #[test]
    fn the_console_row_is_readable_even_when_the_machine_said_little() {
        let full = Reported {
            hostname: "nixos".into(),
            addresses: vec!["10.10.10.47".into()],
            vcpus: 32,
            memory_mib: 131_072,
            disks: vec!["nvme0n1".into(), "sda".into()],
            serial: "PT-0042".into(),
        };
        let line = full.summary();
        assert!(line.contains("32 vCPU"), "{line}");
        assert!(line.contains("128 GiB"), "{line}");
        assert!(line.contains("2 disks"), "{line}");
        assert!(line.contains("PT-0042"), "{line}");
        assert_eq!(
            Reported::default().summary(),
            "reported nothing about itself"
        );
        // One disk is one disk, not "1 disks".
        let one = Reported {
            disks: vec!["sda".into()],
            ..Default::default()
        };
        assert_eq!(one.summary(), "1 disk");
    }
}

/// The one piece of cryptography in this feature, against real keys.
#[cfg(test)]
mod claims {
    use ring::signature::KeyPair;

    use super::*;

    fn keypair() -> (ring::signature::Ed25519KeyPair, String) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = encode_standard(pair.public_key().as_ref());
        (pair, public)
    }

    fn sign(pair: &ring::signature::Ed25519KeyPair, id: &str) -> String {
        encode_standard(pair.sign(&claim_message(id)).as_ref())
    }

    /// The machine that announced can claim.
    #[test]
    fn the_key_that_announced_may_claim() {
        let (pair, public) = keypair();
        let id = "m-1a2b3c4d5e6f";
        assert_eq!(verify_claim(&public, id, &sign(&pair, id)), Ok(()));
    }

    /// A signature over a different enrolment does not open this one. Without
    /// the id in the message, one machine's claim would open every row.
    #[test]
    fn a_signature_for_another_machine_does_not_open_this_one() {
        let (pair, public) = keypair();
        let theirs = sign(&pair, "m-aaaaaaaaaaaa");
        assert_eq!(
            verify_claim(&public, "m-bbbbbbbbbbbb", &theirs),
            Err(BadKey::DoesNotVerify)
        );
    }

    /// Somebody else's key does not open it either — which is the whole
    /// property, and the reason the row is addressed by the key's own hash.
    #[test]
    fn another_key_does_not_open_it() {
        let (_, mine) = keypair();
        let (other, _) = keypair();
        let id = "m-1a2b3c4d5e6f";
        assert_eq!(
            verify_claim(&mine, id, &sign(&other, id)),
            Err(BadKey::DoesNotVerify)
        );
    }

    /// Four spellings of base64, one key. Without this a client that padded
    /// differently on its second announce would land on a second row, and an
    /// operator would see two pending machines that are one machine.
    #[test]
    fn every_spelling_of_one_key_is_one_key() {
        use base64::Engine;
        let (pair, standard) = keypair();
        let raw = pair.public_key().as_ref();
        for spelling in [
            base64::engine::general_purpose::STANDARD.encode(raw),
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(raw),
            base64::engine::general_purpose::URL_SAFE.encode(raw),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw),
        ] {
            assert_eq!(
                canonical_key(&spelling).expect("a key"),
                standard,
                "{spelling} is the same key"
            );
        }
        // And with the whitespace a paste carries.
        assert_eq!(
            canonical_key(&format!("  {standard}\n")).expect("a key"),
            standard
        );
    }

    /// Rubbish is refused by name, on a door anybody can knock on.
    #[test]
    fn what_is_not_a_key_is_said_to_not_be_one() {
        assert_eq!(canonical_key("not base64 at all!!"), Err(BadKey::NotBase64));
        // Valid base64, wrong length: an RSA key, a truncated paste, a typo.
        assert_eq!(canonical_key("AAAA"), Err(BadKey::WrongLength(3)));
        let (_, public) = keypair();
        assert_eq!(
            verify_claim(&public, "m-1", "AAAA"),
            Err(BadKey::NotASignature(3))
        );
    }
}
