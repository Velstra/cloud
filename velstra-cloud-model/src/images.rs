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

use crate::meta::{Condition, Timestamp};

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

/// What a node files an image's bytes under: `sha256-<hex>`, from its digest.
///
/// The one place that spelling is decided, so the node that writes the file, the
/// agent that reports it and the API that matches them cannot disagree — which
/// they did: the API compared object *names* against filed *digests* and every
/// image reported as cached nowhere.
pub fn stored_name(digest: &str) -> Option<String> {
    let hex = digest
        .rsplit(':')
        .next()?
        .rsplit('-')
        .next()?
        .to_ascii_lowercase();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("sha256-{hex}"))
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
    Ok(())
}

/// Find the digest a checksums file gives for one filename.
///
/// The layout every distribution ships: `<hex>  <name>` per line, sometimes with
/// a `*` before the name for "binary mode". Lines that name something else are
/// skipped rather than guessed at — a file that does not mention this image is
/// not a file that says anything about it.
pub fn digest_for(checksums: &str, filename: &str) -> Option<String> {
    for line in checksums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        // `SHA512SUMS` and `SHA256SUMS` look identical and are not: a 128-digit
        // hex is a sha512, which this platform does not address images by. Taken
        // as one would be a digest that never matches any bytes.
        if name == filename && hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
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
            Some("cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d")
        );
        assert_eq!(digest_for(file, "not-there.qcow2"), None);
        // The short one is not a sha256 and is not taken for one.
        assert_eq!(digest_for(file, "debian-12-genericcloud-amd64.qcow2"), None);
    }

    #[test]
    fn a_sha512sums_file_is_not_read_as_sha256() {
        // Both files sit in the same directory with near-identical names, and a
        // sha512 taken for a sha256 is a digest no bytes will ever match — a
        // source that looks like it is working and publishes nothing that boots.
        let file = format!("{}  disk.qcow2\n", "a".repeat(128));
        assert_eq!(digest_for(&file, "disk.qcow2"), None);
    }

    #[test]
    fn binary_mode_names_are_read_too() {
        assert_eq!(
            digest_for(&format!("{}  *disk.qcow2\n", "b".repeat(64)), "disk.qcow2"),
            Some("b".repeat(64))
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
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|b: Vec<u8>| format!("an Ed25519 public key is 32 bytes, this is {}", b.len()))?;
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

/// What is signed: the digest line exactly as `spec.digest` carries it,
/// `sha256:<64 hex>`, with no trailing newline. Signing the digest rather than
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
    if !digest.starts_with("sha256:") || digest.len() != 7 + 64 {
        return SignatureVerdict::Refused(format!(
            "a signature is over the digest line, and {digest:?} is not one (`sha256:<64 hex>`)"
        ));
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
    use super::*;
    use base64::Engine;
    use ring::signature::KeyPair;

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
        let verdict = judge_signature(DIGEST, Some(&sign(&pair, DIGEST)), std::slice::from_ref(&key));
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
        match judge_signature("sha256-abc", Some(&good), &[key]) {
            SignatureVerdict::Refused(why) => assert!(why.contains("digest line"), "{why}"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn no_signature_is_unsigned_not_refused_and_garbage_is_refused_before_any_key() {
        let (_, key) = keypair();
        assert_eq!(judge_signature(DIGEST, None, std::slice::from_ref(&key)), SignatureVerdict::Unsigned);
        assert_eq!(judge_signature(DIGEST, Some("  "), &[]), SignatureVerdict::Unsigned);
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
        assert_eq!(format!("{key:?}"), format!("SigningKey({})", key.fingerprint()));
        assert_eq!(key.fingerprint().len(), 8);
    }
}
