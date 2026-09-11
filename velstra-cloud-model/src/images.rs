//! Where a family's images come from, and how the newest one arrives.
//!
//! ## The trust question, first
//!
//! An image's id is its `sha256`, and the node fetches the bytes over plain
//! `http://` on purpose: content-addressed bytes need no transport security,
//! because a wrong byte gives a wrong digest and the fetch fails. That argument
//! is sound and it is written down in the agent.
//!
//! It does **not** extend to learning *which* digest is current. Whoever can
//! rewrite the answer to "what is the newest debian-13" chooses what every new
//! guest in the cell boots. So the digest is learned over `https://` with the
//! certificate checked, and the bytes are then fetched however is convenient and
//! verified against it. Two different jobs, two different mechanisms, and
//! confusing them is how a platform ends up booting whatever a network can
//! inject.
//!
//! ## What rotation is, and what it deliberately is not
//!
//! Checking a source **publishes a new image**. It does not touch a single
//! running guest, and nothing anywhere rewrites an instance's image: a machine
//! keeps the bytes it was built from for as long as it exists. "Always the
//! newest" means *new* machines get the newest, through `families/<name>`,
//! resolved once when they are created.
//!
//! Anything else would be a platform that changes the operating system under a
//! running service at a moment nobody chose.

use serde::{Deserialize, Serialize};

use crate::{
    meta::{Condition, Timestamp},
    resources::ImageFormat,
};

/// How often a source is looked at when it does not say.
///
/// Six hours. Cloud images are published daily at best, and a cell that asks
/// every minute is a cell that spends its day fetching a checksum file to learn
/// nothing.
pub const DEFAULT_EVERY_MS: u64 = 6 * 60 * 60 * 1000;

/// How many versions of a family to keep when the source does not say.
pub const DEFAULT_KEEP: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageSourceSpec {
    /// The family every image this publishes belongs to: `debian-13`.
    pub family: String,
    /// Where the bytes are. Handed to the node as the new image's `sourceUrl`,
    /// so it may be `http://` — the digest is what makes that safe.
    pub url: String,
    /// A checksums file covering `url`'s filename, in the `sha256sum` layout
    /// every distribution publishes: `<hex>  <filename>` per line.
    ///
    /// **`https://` is required**, and refused otherwise. This is the value the
    /// whole arrangement trusts; fetching it over a channel anybody can rewrite
    /// would make the digest below decorative.
    pub checksums: String,
    /// How often to look. Zero means [`DEFAULT_EVERY_MS`].
    #[serde(default)]
    pub every_ms: u64,
    /// How many of this family to keep. Zero means [`DEFAULT_KEEP`].
    ///
    /// Older ones are removed only when **nothing names them**: an image an
    /// instance was built from is never taken away, however old, because the
    /// guest would then be unable to start on its next move.
    #[serde(default)]
    pub keep: u32,
    /// Stop looking, without forgetting where this came from.
    #[serde(default)]
    pub paused: bool,
    /// What the bytes at `url` are.
    ///
    /// There is no way to learn this without the bytes — this controller reads
    /// a checksums file and never the image — and the filename does not say:
    /// Ubuntu's cloud images are qcow2 under `.img`. It used to be assumed to
    /// be qcow2, which a node then refused at disk time ("correct spec.format
    /// on the image rather than have this node decide which of the two to
    /// believe") — and since every rotation mints a new image, correcting it by
    /// hand had to be done again on every publish.
    ///
    /// Left out where the filename cannot be wrong (`.qcow2`), which is Debian,
    /// Fedora, Rocky and AlmaLinux; demanded everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ImageFormat>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageSourceStatus {
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// When this was last looked at, whatever the answer was.
    #[serde(default)]
    pub last_checked: Timestamp,
    /// The digest the last successful check found — which is not necessarily
    /// one this cell published, because it may already have had it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_digest: String,
    /// The image this source published most recently, if it ever has.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published: String,
}

/// How often this source wants to be looked at.
pub fn every(spec: &ImageSourceSpec) -> u64 {
    if spec.every_ms == 0 {
        DEFAULT_EVERY_MS
    } else {
        spec.every_ms
    }
}

/// How many of the family to keep.
pub fn keep(spec: &ImageSourceSpec) -> u32 {
    if spec.keep == 0 {
        DEFAULT_KEEP
    } else {
        spec.keep
    }
}

/// The filename `url` ends in, which is what a checksums file names its lines by.
pub fn filename_of(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

/// Which hash a digest is of.
///
/// Two, because the distributions disagree and an image nobody can point at is
/// worse than a second hash function: Debian publishes only `SHA512SUMS` for
/// its cloud images, and the RPM family and Ubuntu publish `SHA256SUMS`. The
/// platform addresses images by content either way — what varies is which
/// content function, and it is written down in the digest rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Algorithm {
    Sha256,
    Sha512,
}

impl Algorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// How many hex digits its output is. This is also how a checksums line is
    /// recognised: a coreutils line carries no tag, only the hex.
    pub const fn hex_len(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    /// From the tag a BSD-layout checksums line carries. Not `FromStr`: this
    /// reads one field of a line, and a value that is not one of the two is a
    /// line to skip rather than an error to carry.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag.to_ascii_lowercase().as_str() {
            "sha256" => Some(Self::Sha256),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    fn from_hex_len(len: usize) -> Option<Self> {
        match len {
            64 => Some(Self::Sha256),
            128 => Some(Self::Sha512),
            _ => None,
        }
    }
}

/// A content digest: which hash, and the hex of it.
///
/// Parsed rather than assumed, and from either spelling — `sha256:<hex>` is
/// what a spec carries and `sha256-<hex>` is what a file on a node is called,
/// because a colon is not a thing to put in a filename. One type so the two
/// cannot drift.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    pub algorithm: Algorithm,
    pub hex: String,
}

impl Digest {
    /// From `sha256:<hex>`, `sha512-<hex>`, or the last segment of a path
    /// spelled either way. `None` for anything whose hex is not the right
    /// length for the algorithm it claims — a sha512 announced as a sha256 is
    /// a digest no bytes will ever match, which is a failure at fetch time on
    /// a machine nobody is looking at.
    pub fn parse(value: &str) -> Option<Self> {
        let last = value.rsplit('/').next()?;
        let (tag, hex) = last.split_once(':').or_else(|| last.split_once('-'))?;
        let algorithm = Algorithm::from_tag(tag)?;
        let hex = hex.to_ascii_lowercase();
        (hex.len() == algorithm.hex_len() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(Self { algorithm, hex })
    }

    /// Bare hex, with the algorithm known from elsewhere.
    pub fn from_hex(algorithm: Algorithm, hex: &str) -> Option<Self> {
        let hex = hex.to_ascii_lowercase();
        (hex.len() == algorithm.hex_len() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(Self { algorithm, hex })
    }

    /// What a spec carries: `sha256:<hex>`.
    pub fn value(&self) -> String {
        format!("{}:{}", self.algorithm.as_str(), self.hex)
    }

    /// What a node files the bytes under: `sha256-<hex>`.
    pub fn stored(&self) -> String {
        format!("{}-{}", self.algorithm.as_str(), self.hex)
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value())
    }
}

/// What a node files an image's bytes under: `<algorithm>-<hex>`, from its
/// digest.
///
/// The one place that spelling is decided, so the node that writes the file, the
/// agent that reports it and the API that matches them cannot disagree — which
/// they did: the API compared object *names* against filed *digests* and every
/// image reported as cached nowhere.
pub fn stored_name(digest: &str) -> Option<String> {
    Digest::parse(digest).map(|d| d.stored())
}

/// Why a source could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unusable {
    #[error(
        "a checksums file is fetched over https so its certificate can be checked — it is the \
         one value this arrangement trusts, and over any other scheme the digest it carries \
         means nothing"
    )]
    ChecksumsNotHttps,
    #[error("this source names no family, so nothing it publishes could ever be asked for")]
    NoFamily,
    #[error("this source names no url, so there are no bytes to publish")]
    NoUrl,
    #[error("the url ends in no filename, so no line of a checksums file can be matched to it")]
    NoFilename,
    #[error(
        "this source does not say what its bytes are. A digest says *which* bytes, never what \
         they are, and this source never fetches them — so `format` has to be stated unless the \
         filename settles it (`.qcow2`). Ubuntu ships qcow2 under `.img`, which is exactly the \
         case a guess gets wrong: a node handed a mis-declared image refuses the disk, and every \
         guest of this family goes without one."
    )]
    NoFormat,
}

/// What the filename says the bytes are, where it cannot be wrong.
///
/// `.qcow2` and nothing else. `.img` and `.raw` are both used for qcow2 by
/// somebody, and a wrong answer here is a node refusing every guest of a
/// family — so the derivation stops where certainty does.
pub fn format_from_filename(url: &str) -> Option<ImageFormat> {
    filename_of(url)
        .rsplit('.')
        .next()
        .filter(|ext| ext.eq_ignore_ascii_case("qcow2"))
        .map(|_| ImageFormat::Qcow2)
}

/// Everything that can be judged about a source without going near the network.
pub fn refuse_an_unusable_source(spec: &ImageSourceSpec) -> Result<(), Unusable> {
    if spec.family.trim().is_empty() {
        return Err(Unusable::NoFamily);
    }
    if spec.url.trim().is_empty() {
        return Err(Unusable::NoUrl);
    }
    if !spec.checksums.starts_with("https://") {
        return Err(Unusable::ChecksumsNotHttps);
    }
    if filename_of(&spec.url).is_empty() {
        return Err(Unusable::NoFilename);
    }
    if spec.format.is_none() && format_from_filename(&spec.url).is_none() {
        return Err(Unusable::NoFormat);
    }
    Ok(())
}

/// Find the digest a checksums file gives for one filename.
///
/// The layout every distribution ships: `<hex>  <name>` per line, sometimes with
/// a `*` before the name for "binary mode". Lines that name something else are
/// skipped rather than guessed at — a file that does not mention this image is
/// not a file that says anything about it.
///
/// **Which hash it is comes off the line, never off the file's name.** A
/// coreutils line carries no tag, so the hex length decides — 64 is a sha256,
/// 128 a sha512 — and a BSD line says it outright. `SHA512SUMS` and
/// `SHA256SUMS` look identical and are not; reading one as the other yields a
/// digest no bytes will ever match, and the failure lands at fetch time on a
/// node nobody is watching.
///
/// When a file names both for one image — some directories ship a combined
/// list — the sha256 wins, because it is shorter to read and every other part
/// of this platform already speaks it.
pub fn digest_for(checksums: &str, filename: &str) -> Option<String> {
    let mut fallback: Option<Digest> = None;
    for line in checksums.lines() {
        // Both layouts are tried on every line, not the first that parses:
        // a BSD-tag line also parses as a coreutils one, with `SHA256` as the
        // hex and `(name)` as the name, and stopping there would skip the line
        // that actually says something.
        for (tag, hex, name) in [bsd_line(line), coreutils_line(line)].into_iter().flatten() {
            if name != filename {
                continue;
            }
            let algorithm = match tag {
                // A BSD line names its function; trust it over the length, and
                // refuse the pair when they disagree.
                Some(tag) => match Algorithm::from_tag(tag) {
                    Some(a) => a,
                    None => continue,
                },
                None => match Algorithm::from_hex_len(hex.len()) {
                    Some(a) => a,
                    None => continue,
                },
            };
            let Some(digest) = Digest::from_hex(algorithm, hex) else {
                continue;
            };
            if digest.algorithm == Algorithm::Sha256 {
                return Some(digest.value());
            }
            fallback.get_or_insert(digest);
        }
    }
    fallback.map(|d| d.value())
}

/// `<hex>  <name>`, with an optional `*` for binary mode. Debian, Ubuntu and
/// everything else that ships `SHA256SUMS` or `SHA512SUMS` from GNU coreutils.
/// No tag: the caller reads the function off the hex's length.
fn coreutils_line(line: &str) -> Option<(Option<&str>, &str, &str)> {
    let mut parts = line.split_whitespace();
    let (hex, name) = (parts.next()?, parts.next()?);
    Some((None, hex, name.strip_prefix('*').unwrap_or(name)))
}

/// `SHA256 (<name>) = <hex>` — the BSD tag layout.
///
/// Fedora, Rocky, AlmaLinux and CentOS Stream all ship their `*-CHECKSUM`
/// files this way, so without it an operator pointing a source at any
/// RPM-family cloud image directory got an empty family and a message about a
/// SHA512SUMS mix-up that was not what happened.
///
/// The tag is returned rather than matched here: it says which function the
/// line is about, and that is the caller's decision to make.
fn bsd_line(line: &str) -> Option<(Option<&str>, &str, &str)> {
    let line = line.trim();
    let tag = ["SHA256", "SHA512"]
        .into_iter()
        .find(|t| line.starts_with(t))?;
    let rest = line.strip_prefix(tag)?.trim_start();
    let (name, hex) = rest.strip_prefix('(')?.split_once(')')?;
    Some((Some(tag), hex.trim_start().strip_prefix('=')?.trim(), name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksums_file_is_read_by_the_name_it_covers() {
        let file = "\
d2b1c3  debian-12-genericcloud-amd64.qcow2
cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d  debian-13-genericcloud-amd64.qcow2
aa  something-else.qcow2
";
        assert_eq!(
            digest_for(file, "debian-13-genericcloud-amd64.qcow2").as_deref(),
            Some("sha256:cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d")
        );
        assert_eq!(digest_for(file, "not-there.qcow2"), None);
        // The short one is neither length and is taken for neither function.
        assert_eq!(digest_for(file, "debian-12-genericcloud-amd64.qcow2"), None);
    }

    /// Fedora, Rocky, AlmaLinux and CentOS Stream ship their checksums this
    /// way. Without it, an operator pointing a source at any of them got an
    /// empty family and a message blaming a SHA512SUMS mix-up.
    #[test]
    fn the_bsd_tag_layout_is_read_too() {
        let file = "\
SHA256 (Rocky-9-GenericCloud.latest.x86_64.qcow2) = cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d
SHA256 (Rocky-9-GenericCloud-Base.latest.x86_64.qcow2) = aa
";
        assert_eq!(
            digest_for(file, "Rocky-9-GenericCloud.latest.x86_64.qcow2").as_deref(),
            Some("sha256:cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d")
        );
        // The short one is neither length and is taken for neither function.
        assert_eq!(
            digest_for(file, "Rocky-9-GenericCloud-Base.latest.x86_64.qcow2"),
            None
        );
        // A BSD line names its function, and is read under the one it names.
        let long = format!("SHA512 (disk.qcow2) = {}\n", "a".repeat(128));
        assert_eq!(
            digest_for(&long, "disk.qcow2").as_deref(),
            Some(format!("sha512:{}", "a".repeat(128)).as_str())
        );
        // A tag whose hex is the wrong length for it is a line that contradicts
        // itself, and neither half is believed.
        let wrong = format!("SHA512 (disk.qcow2) = {}\n", "a".repeat(64));
        assert_eq!(digest_for(&wrong, "disk.qcow2"), None);
    }

    /// Debian ships `SHA512SUMS` for its cloud images and no `SHA256SUMS`, so
    /// this is the ordinary case and not the exotic one. What must never happen
    /// is the sha512 coming back *labelled* a sha256: that is a digest no bytes
    /// will ever match, and the failure lands at fetch time on a node nobody is
    /// watching.
    #[test]
    fn a_sha512sums_file_is_read_as_a_sha512() {
        let file = format!("{}  disk.qcow2\n", "a".repeat(128));
        assert_eq!(
            digest_for(&file, "disk.qcow2").as_deref(),
            Some(format!("sha512:{}", "a".repeat(128)).as_str())
        );
    }

    /// A directory that ships both gets read as the one everything else in this
    /// platform already speaks — and the answer must not depend on which line
    /// happens to come first.
    #[test]
    fn sha256_wins_when_a_file_names_both() {
        let short = "c".repeat(64);
        let long = "d".repeat(128);
        for file in [
            format!("{long}  disk.qcow2\n{short}  disk.qcow2\n"),
            format!("{short}  disk.qcow2\n{long}  disk.qcow2\n"),
        ] {
            assert_eq!(
                digest_for(&file, "disk.qcow2").as_deref(),
                Some(format!("sha256:{short}").as_str())
            );
        }
    }

    #[test]
    fn binary_mode_names_are_read_too() {
        assert_eq!(
            digest_for(&format!("{}  *disk.qcow2\n", "b".repeat(64)), "disk.qcow2"),
            Some(format!("sha256:{}", "b".repeat(64)))
        );
    }

    #[test]
    fn a_filename_is_taken_from_the_url_and_not_from_a_query() {
        assert_eq!(
            filename_of("https://example.invalid/a/b/debian-13.qcow2?token=x"),
            "debian-13.qcow2"
        );
        assert_eq!(filename_of("https://example.invalid/a/b/"), "b");
    }

    #[test]
    fn checksums_over_anything_but_https_are_refused() {
        let spec = ImageSourceSpec {
            family: "debian-13".into(),
            url: "http://example.invalid/debian-13.qcow2".into(),
            checksums: "http://example.invalid/SHA256SUMS".into(),
            ..Default::default()
        };
        assert_eq!(
            refuse_an_unusable_source(&spec),
            Err(Unusable::ChecksumsNotHttps)
        );
        let ok = ImageSourceSpec {
            checksums: "https://example.invalid/SHA256SUMS".into(),
            ..spec
        };
        assert_eq!(refuse_an_unusable_source(&ok), Ok(()));
    }
}

// ---- signatures ------------------------------------------------------------
//
// An image's digest says the bytes are the bytes; it does not say whose bytes
// they are. A signature over the digest does, provided something checks it
// against a key the cell trusts. This is that check — Ed25519 over the digest
// line, under keys the cell was started with — and it is the only place the
// platform forms an opinion about `spec.signature`. The API consults it at
// admission and stores nothing that failed; the node agent consults it again
// before it fetches, so a store somebody wrote around the API still cannot get
// a refused image onto a machine.

use std::fmt;

/// A public key an image's signature may verify under: Ed25519, 32 raw bytes,
/// written as standard base64 wherever a person hands it over.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKey([u8; 32]);

impl SigningKey {
    /// Parse the base64 form. The raw 32 bytes, not a PEM or an SSH line —
    /// `openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | base64`
    /// is how one is made from an OpenSSL key.
    pub fn parse(text: &str) -> Result<Self, String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .map_err(|e| format!("not base64: {e}"))?;
        let key: [u8; 32] = bytes.try_into().map_err(|b: Vec<u8>| {
            format!("an Ed25519 public key is 32 bytes, this is {}", b.len())
        })?;
        Ok(Self(key))
    }

    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self(key)
    }

    /// The first eight hex digits of the key's sha256 — enough to say which
    /// key in a log without printing the key.
    pub fn fingerprint(&self) -> String {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(self.0);
        hash.iter().take(4).map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigningKey({})", self.fingerprint())
    }
}

/// What the check said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureVerdict {
    /// No signature was offered. Not a failure: an unsigned image is an honest
    /// one, and whether the cell accepts those is a policy the caller holds.
    Unsigned,
    /// The signature verifies under one of the keys; the fingerprint says which.
    Verified { key: String },
    /// A signature was offered and does not hold. The sentence says why.
    Refused(String),
}

/// What is signed: the digest line exactly as `spec.digest` carries it —
/// `sha256:<64 hex>` or `sha512:<128 hex>` — with no trailing newline. Signing the digest rather than
/// the bytes means a signer never has to hold the image, and a verifier never
/// has to download it to know whether it may.
pub fn signed_message(digest: &str) -> &[u8] {
    digest.as_bytes()
}

/// Judge `signature` over `digest` under `keys`.
///
/// With no keys, every signature is refused: a claim nobody can check is
/// worse than no claim, because every place it is shown becomes evidence
/// somebody will cite. That is the posture the API kept before verification
/// existed, and it is kept on purpose for a cell that has not named a key.
pub fn judge_signature(
    digest: &str,
    signature: Option<&str>,
    keys: &[SigningKey],
) -> SignatureVerdict {
    use base64::Engine;
    let Some(signature) = signature.map(str::trim).filter(|s| !s.is_empty()) else {
        return SignatureVerdict::Unsigned;
    };
    if keys.is_empty() {
        return SignatureVerdict::Refused(
            "this cell was started without an image signing key, so no signature can be \
             checked; start the API with --image-signing-key, or publish the image without one"
                .into(),
        );
    }
    // The hex is checked, not just the shape. A signature is a claim *about a
    // digest*, so a digest that is not one makes the claim meaningless — and
    // `sha256:` followed by sixty-four arbitrary characters would otherwise
    // verify happily and be shown as `verified` in the console. The message is
    // over the line as written, so the spelling has to be exact: a value that
    // parses only after being normalised is not the line anybody signed.
    match Digest::parse(digest) {
        // Not a digest at all.
        None => {
            return SignatureVerdict::Refused(format!(
                "a signature is over the digest line, and {digest:?} is not one \
                 (`sha256:<64 hex>` or `sha512:<128 hex>`)"
            ));
        }
        // A digest, in another spelling. Refused on purpose and not normalised
        // — see above — but the old message said it was "not one", which is
        // both untrue and the wrong thing to go looking for. The API settles
        // the spelling at create, so this is reachable only for an object
        // written around the API or stored before that door existed.
        Some(canonical) if canonical.value() != digest => {
            return SignatureVerdict::Refused(format!(
                "a signature is over the digest line exactly as written, and this platform \
                 writes it lowercase; {digest:?} is that digest in another spelling — sign \
                 `{}` instead",
                canonical.value()
            ));
        }
        Some(_) => {}
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(signature) {
        Ok(b) => b,
        Err(e) => return SignatureVerdict::Refused(format!("the signature is not base64: {e}")),
    };
    if bytes.len() != 64 {
        return SignatureVerdict::Refused(format!(
            "an Ed25519 signature is 64 bytes, this is {}",
            bytes.len()
        ));
    }
    for key in keys {
        let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.0);
        if verifier.verify(signed_message(digest), &bytes).is_ok() {
            return SignatureVerdict::Verified {
                key: key.fingerprint(),
            };
        }
    }
    SignatureVerdict::Refused(format!(
        "the signature does not verify over {digest} under any of the {} configured signing \
         key(s); it was made with another key, or over something other than the digest line",
        keys.len()
    ))
}

#[cfg(test)]
mod signature_tests {
    use base64::Engine;
    use ring::signature::KeyPair;

    use super::*;

    fn keypair() -> (ring::signature::Ed25519KeyPair, SigningKey) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public: [u8; 32] = pair.public_key().as_ref().try_into().unwrap();
        (pair, SigningKey::from_bytes(public))
    }

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn sign(pair: &ring::signature::Ed25519KeyPair, message: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(pair.sign(message.as_bytes()).as_ref())
    }

    #[test]
    fn a_signature_under_a_configured_key_verifies_and_names_the_key() {
        let (pair, key) = keypair();
        let verdict = judge_signature(
            DIGEST,
            Some(&sign(&pair, DIGEST)),
            std::slice::from_ref(&key),
        );
        assert_eq!(
            verdict,
            SignatureVerdict::Verified {
                key: key.fingerprint()
            }
        );
    }

    #[test]
    fn the_wrong_key_the_wrong_message_and_no_key_at_all_are_refused_with_a_reason() {
        let (pair, key) = keypair();
        let (_, other) = keypair();
        let good = sign(&pair, DIGEST);
        match judge_signature(DIGEST, Some(&good), &[other]) {
            SignatureVerdict::Refused(why) => assert!(why.contains("another key"), "{why}"),
            v => panic!("{v:?}"),
        }
        let over_bytes = sign(&pair, "not the digest line");
        assert!(matches!(
            judge_signature(DIGEST, Some(&over_bytes), std::slice::from_ref(&key)),
            SignatureVerdict::Refused(_)
        ));
        match judge_signature(DIGEST, Some(&good), &[]) {
            SignatureVerdict::Refused(why) => assert!(why.contains("--image-signing-key"), "{why}"),
            v => panic!("{v:?}"),
        }
        match judge_signature("sha256-abc", Some(&good), std::slice::from_ref(&key)) {
            SignatureVerdict::Refused(why) => assert!(why.contains("digest line"), "{why}"),
            v => panic!("{v:?}"),
        }
        // Sixty-four characters that are not hex is not a digest, however
        // convincingly it is signed: the shape alone would have passed.
        let not_hex = format!("sha256:{}", "z".repeat(64));
        match judge_signature(
            &not_hex,
            Some(&sign(&pair, &not_hex)),
            std::slice::from_ref(&key),
        ) {
            SignatureVerdict::Refused(why) => assert!(why.contains("64 hex"), "{why}"),
            v => panic!("a signature over a digest that is not one was accepted: {v:?}"),
        }
    }

    #[test]
    fn no_signature_is_unsigned_not_refused_and_garbage_is_refused_before_any_key() {
        let (_, key) = keypair();
        assert_eq!(
            judge_signature(DIGEST, None, std::slice::from_ref(&key)),
            SignatureVerdict::Unsigned
        );
        assert_eq!(
            judge_signature(DIGEST, Some("  "), &[]),
            SignatureVerdict::Unsigned
        );
        assert!(matches!(
            judge_signature(DIGEST, Some("not base64!"), std::slice::from_ref(&key)),
            SignatureVerdict::Refused(_)
        ));
        assert!(matches!(
            judge_signature(DIGEST, Some("AAAA"), &[key]),
            SignatureVerdict::Refused(_)
        ));
    }

    #[test]
    fn a_key_round_trips_through_base64_and_prints_only_its_fingerprint() {
        let (_, key) = keypair();
        let text = base64::engine::general_purpose::STANDARD.encode(key.0);
        assert_eq!(SigningKey::parse(&format!(" {text}\n")).unwrap(), key);
        assert!(SigningKey::parse("AAAA").unwrap_err().contains("32 bytes"));
        assert!(SigningKey::parse("*").unwrap_err().contains("base64"));
        assert_eq!(
            format!("{key:?}"),
            format!("SigningKey({})", key.fingerprint())
        );
        assert_eq!(key.fingerprint().len(), 8);
    }
}
