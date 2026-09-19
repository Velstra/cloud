//! The join token: everything a machine needs to become part of a cell, in
//! one string somebody can paste.
//!
//! ## Why one string
//!
//! Joining the second machine of a cell used to mean carrying six facts from
//! the control plane by six routes: a URL that had to be a name the certificate
//! carries, the certificate itself by `scp`, region and cell typed or asked of
//! a cell the installer cannot reach, a node id that had to match an object
//! made first, a 64-character token shown once, and the one hypervisor that can
//! open Ceph, unmarked in a menu. Every route is a place a fleet loses a
//! machine. Proxmox and Incus both arrived at the same answer: one blob, minted
//! on the side that knows the facts, pasted on the side that needs them.
//!
//! ## Self-contained, on purpose
//!
//! The token carries the API's certificate rather than a fingerprint to check
//! one against. A fingerprint is shorter, and it costs an unverified first
//! contact plus an endpoint to fetch the certificate from — and the installer
//! ISO seeds a filesystem that has never booted on a machine that may have no
//! cable in it yet. *Nothing needs to be reachable during the install* is a
//! promise this platform already makes, and a token that needs the network to
//! be useful would break it for the sake of a shorter string. A P-256
//! self-signed certificate makes the whole thing about 1.3 KB, which a serial
//! console takes in one line.
//!
//! ## The shape on the wire
//!
//! `velstra1.` and then base64url (no padding) of a JSON object. The prefix is
//! what lets a wizard tell a join token from a bare registration token or a
//! URL somebody pasted in the wrong field, and say which. The `1` is the
//! version: a machine running an older build that meets a `velstra2.` token
//! says so rather than reading half of it.
//!
//! Here rather than in the model crate because the installer reads this and
//! the installer deliberately does not depend on the model — it is one small
//! binary on an ISO — and this crate is the one both already agree on.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// The prefix every join token starts with.
pub const PREFIX: &str = "velstra1.";

/// A pool's credential, when the joining machine is also a pool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolJoin {
    pub id: String,
    pub token: String,
}

/// What a machine is told when it joins.
///
/// Field names are short because the whole thing is pasted, and stable because
/// a token minted today is pasted into an installer built next month.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken {
    /// Format version. Always 1 from this build.
    pub v: u32,
    pub region: String,
    pub cell: String,
    /// The node id this token was issued for — the object already exists.
    pub node: String,
    /// Where the API answers, in the order to try them. Every one is a name
    /// the certificate below verifies for, because the same list chose the
    /// certificate's names.
    pub urls: Vec<String>,
    /// The API's certificate, PEM. A self-signed certificate is its own root.
    pub ca: String,
    /// The node's one-time registration token.
    pub token: String,
    /// Present only when the machine is also a pool. A second credential
    /// because a pool is a second agent: the API authenticates it as
    /// `pool:<id>`, and a node token presented by a pool agent is answered
    /// `401` for ever with the seed looking complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<PoolJoin>,
}

/// Why a string is not a join token.
#[derive(Debug, PartialEq, Eq)]
pub enum JoinError {
    /// No `velstra1.` prefix — a URL, a bare token, or a paste of the wrong
    /// thing.
    NotAJoinToken,
    /// A prefix this build does not know how to read.
    UnknownVersion(String),
    /// The body after the prefix is not base64url.
    NotBase64,
    /// Decoded, but not the JSON this build expects.
    NotJson(String),
    /// A field that must not be empty is.
    Missing(&'static str),
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAJoinToken => write!(
                f,
                "not a join token: one starts with `{PREFIX}`. A bare 64-character token is a \
                 registration token, which goes in the token field, not here"
            ),
            Self::UnknownVersion(p) => write!(
                f,
                "a join token in a format this build does not read ({p}…): the cell that minted \
                 it is newer than this installer"
            ),
            Self::NotBase64 => write!(
                f,
                "the join token is damaged: the part after `{PREFIX}` is not base64url. A line \
                 break in the middle of a paste does this"
            ),
            Self::NotJson(e) => write!(f, "the join token decodes but does not parse: {e}"),
            Self::Missing(what) => write!(f, "the join token carries no {what}"),
        }
    }
}

impl std::error::Error for JoinError {}

impl JoinToken {
    /// One pasteable string.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("a join token serialises");
        format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    /// Back from a pasted string. Whitespace around and *inside* is dropped:
    /// a serial console that wrapped the line, or a paste that picked up a
    /// newline, is still one token.
    pub fn decode(text: &str) -> Result<Self, JoinError> {
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let Some(body) = compact.strip_prefix(PREFIX) else {
            return Err(match compact.split_once('.') {
                Some((p, _)) if p.starts_with("velstra") => {
                    JoinError::UnknownVersion(format!("{p}."))
                }
                _ => JoinError::NotAJoinToken,
            });
        };
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| JoinError::NotBase64)?;
        let token: JoinToken =
            serde_json::from_slice(&bytes).map_err(|e| JoinError::NotJson(e.to_string()))?;
        for (what, value) in [
            ("region", &token.region),
            ("cell", &token.cell),
            ("node id", &token.node),
            ("certificate", &token.ca),
        ] {
            if value.trim().is_empty() {
                return Err(JoinError::Missing(what));
            }
        }
        // A credential of *some* kind: the node's, or a pool's. A pool-only
        // token carries no node token on purpose — the machine runs no guests
        // — and a token with neither is a machine with nothing to be.
        let has_pool = token
            .pool
            .as_ref()
            .is_some_and(|p| !p.id.trim().is_empty() && !p.token.trim().is_empty());
        if token.token.trim().is_empty() && !has_pool {
            return Err(JoinError::Missing(
                "credential (neither a node nor a pool token)",
            ));
        }
        if token.urls.iter().all(|u| u.trim().is_empty()) {
            return Err(JoinError::Missing("API address"));
        }
        Ok(token)
    }
}

/// The join file, carried on the install medium itself.
///
/// The cell hands out the installer ISO with this appended after the ISO's
/// last byte: a marker, the join file, a marker. The medium stays exactly the
/// ISO for everything that reads it as one — the filesystem ends where its
/// volume descriptor says — and the installer reads past that end for the
/// trailer. Nothing in the ISO is rewritten and no partition table is touched,
/// which is what makes this safe to do to an image that was verified against a
/// published digest: the bytes the digest covers are the bytes that are there.
/// A medium written with `dd`, Etcher or Ventoy carries it, because they copy
/// every byte; a tool that unpacks the ISO onto a stick does not, and the
/// wizard then asks for the token the way it always did.
pub const TRAILER_BEGIN: &[u8] = b"-----BEGIN VELSTRA JOIN-----\n";
pub const TRAILER_END: &[u8] = b"-----END VELSTRA JOIN-----\n";

/// How far past the ISO's declared end the installer looks. A hybrid ISO's
/// own padding and its backup partition table lie inside this, and so does
/// the trailer; the far end of a large stick does not, and is not read.
pub const TRAILER_WINDOW: u64 = 8 * 1024 * 1024;

/// The bytes appended to the medium for one machine.
pub fn trailer(join_file: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(join_file.len() + 80);
    out.push(b'\n');
    out.extend_from_slice(TRAILER_BEGIN);
    out.extend_from_slice(join_file.as_bytes());
    if !join_file.ends_with('\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(TRAILER_END);
    out
}

/// The join file inside a trailer, if these bytes hold one.
pub fn trailer_in(bytes: &[u8]) -> Option<String> {
    let start = find(bytes, TRAILER_BEGIN)? + TRAILER_BEGIN.len();
    let end = start + find(&bytes[start..], TRAILER_END)?;
    String::from_utf8(bytes[start..end].to_vec()).ok()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> JoinToken {
        JoinToken {
            v: 1,
            region: "eu-central".into(),
            cell: "cell-1".into(),
            node: "peter".into(),
            urls: vec![
                "https://10.10.10.8:8443".into(),
                "https://horst:8443".into(),
            ],
            ca: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".into(),
            token: "ab".repeat(32),
            pool: None,
        }
    }

    /// What goes in comes out, with the prefix a wizard keys on.
    #[test]
    fn a_token_survives_the_round_trip() {
        let t = token();
        let s = t.encode();
        assert!(s.starts_with(PREFIX), "{s}");
        assert!(
            !s.contains('='),
            "padding would be one more thing a paste mangles: {s}"
        );
        assert_eq!(JoinToken::decode(&s), Ok(t));
    }

    /// A pool's credential rides along, and its absence costs no bytes.
    #[test]
    fn a_pool_is_carried_only_when_there_is_one() {
        let mut with = token();
        with.pool = Some(PoolJoin {
            id: "local-2".into(),
            token: "cd".repeat(32),
        });
        assert_eq!(JoinToken::decode(&with.encode()), Ok(with.clone()));
        assert!(!token().encode().contains("pool"));
    }

    /// A console that wrapped the line, or a paste with a newline in it, is
    /// still the same token — the alternative is a machine that registers
    /// nowhere because of a character nobody can see.
    #[test]
    fn whitespace_inside_a_paste_is_not_part_of_the_token() {
        let s = token().encode();
        let (a, b) = s.split_at(s.len() / 2);
        let wrapped = format!("  {a}\n{b} \n");
        assert_eq!(JoinToken::decode(&wrapped), Ok(token()));
    }

    /// Each wrong paste is told apart and named, because "invalid token" sent
    /// to somebody holding three different strings is not an error message.
    #[test]
    fn the_wrong_thing_pasted_is_named() {
        assert_eq!(
            JoinToken::decode(&"ab".repeat(32)),
            Err(JoinError::NotAJoinToken)
        );
        assert_eq!(
            JoinToken::decode("https://horst:8443"),
            Err(JoinError::NotAJoinToken)
        );
        assert_eq!(
            JoinToken::decode("velstra2.abc"),
            Err(JoinError::UnknownVersion("velstra2.".into()))
        );
        assert_eq!(JoinToken::decode("velstra1.***"), Err(JoinError::NotBase64));
        assert!(matches!(
            JoinToken::decode("velstra1.e30"), // {}
            Err(JoinError::NotJson(_))
        ));
        let mut empty = token();
        empty.ca = String::new();
        assert_eq!(
            JoinToken::decode(&empty.encode()),
            Err(JoinError::Missing("certificate"))
        );
        // No credential at all is refused; a pool's alone is enough.
        let mut none = token();
        none.token = String::new();
        assert!(matches!(
            JoinToken::decode(&none.encode()),
            Err(JoinError::Missing(_))
        ));
        none.pool = Some(PoolJoin {
            id: "local-2".into(),
            token: "cd".repeat(32),
        });
        assert!(JoinToken::decode(&none.encode()).is_ok());
    }

    /// Stays under what a serial console takes in one line, with a real-sized
    /// certificate in it.
    #[test]
    fn a_real_certificate_keeps_the_token_pasteable() {
        let mut t = token();
        // A P-256 self-signed certificate PEM is about 700–900 bytes.
        t.ca = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            "M".repeat(900)
        );
        assert!(t.encode().len() < 2048, "{}", t.encode().len());
    }
}

/// What a machine signs to prove it holds the key it announced.
///
/// Here rather than in the model, because both ends need it and only one of
/// them carries the model: the API verifies, and the *installer* signs — and
/// the installer is a small binary on a sealed medium that deliberately
/// depends on this crate and nothing larger. A message spelled twice is a
/// machine that signs something the cell does not check.
///
/// The enrolment's own id and nothing else. A nonce would be better against
/// replay and is not needed: a claim succeeds once, so a replayed signature
/// buys a refusal. Versioned, so a future shape cannot be mistaken for this
/// one by a machine running older code.
pub fn claim_message(id: &str) -> Vec<u8> {
    format!("velstra-enrollment-claim:v1:{id}").into_bytes()
}

#[cfg(test)]
mod claim_message_tests {
    use super::claim_message;

    /// Pinned, because it is a wire format: changing it silently would stop
    /// every machine already in the field from being able to claim.
    #[test]
    fn the_message_is_the_id_under_a_versioned_prefix() {
        assert_eq!(
            claim_message("m-1a2b3c4d5e6f"),
            b"velstra-enrollment-claim:v1:m-1a2b3c4d5e6f".to_vec()
        );
    }

    /// The trailer carries the file exactly, and nothing else in a medium is
    /// mistaken for one.
    #[test]
    fn a_trailer_carries_the_join_file_and_is_found_after_the_iso() {
        use super::{TRAILER_BEGIN, trailer, trailer_in};

        let file = "# velstra join: peter in cell-1\nvelstra1.abc\n";
        let mut medium = vec![0u8; 4096];
        medium.extend_from_slice(&trailer(file));
        medium.extend_from_slice(&[0u8; 100]);
        assert_eq!(trailer_in(&medium).as_deref(), Some(file));
        assert_eq!(trailer_in(&[0u8; 4096]), None);
        // A begin without an end is not a trailer: a write that stopped half
        // way is not a token.
        let mut cut = vec![0u8; 16];
        cut.extend_from_slice(TRAILER_BEGIN);
        cut.extend_from_slice(b"velstra1.abc\n");
        assert_eq!(trailer_in(&cut), None);
        // A file without its own newline gets one, so the end marker starts a line.
        assert_eq!(
            trailer_in(&trailer("velstra1.abc")).as_deref(),
            Some("velstra1.abc\n")
        );
    }
}
