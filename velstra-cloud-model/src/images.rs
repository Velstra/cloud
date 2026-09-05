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
