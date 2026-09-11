//! Replaying a create without making a second one.
//!
//! **The problem is the generated name.** A create that carries its own id is
//! already idempotent: send it twice and the second answer is
//! `AlreadyExists`, which a client can read as "mine". A create that lets the
//! platform pick the name has no such handle — the request that timed out may
//! have been accepted, and the only way to find out is to look, which is
//! exactly the race a retry is trying to avoid. So a client that retries makes
//! two instances, and a client that does not retry loses the one it asked for.
//!
//! A key closes it. The caller invents one, sends it on every attempt of the
//! same create, and the API answers the second attempt with the *first
//! attempt's* answer: same operation, same target, nothing new made. This is
//! `Idempotency-Key` as the IETF draft spells it, and it is what AWS calls a
//! client token and Google a request id.
//!
//! **What the record has to hold.** The answer, so a replay can be answered
//! without doing the work again — and a fingerprint of the request, so a key
//! reused for a *different* create is refused rather than silently answered
//! with somebody else's object. That refusal is the important half: a client
//! that reuses one key for a loop of creates has a bug, and the failure mode
//! without the check is one object where there should have been fifty, with
//! forty-nine successful-looking answers.
//!
//! **Why it is scoped by caller — and by what was being made.** The key is the
//! caller's own invention, so two tenants will eventually pick the same string,
//! and so will one tenant across two scripts. The record is named by the
//! subject *and* by the parent and collection the create was aimed at, so both
//! collisions are non-events rather than one create answering another's.
//!
//! The narrower scope is the honest one: a key stands for "this create", and
//! one volume and one instance asked for under the same string are two creates.
//! Refusing the second would be the API inventing a conflict out of a client's
//! naming habit.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::meta::Timestamp;

/// The store segment the records live under — a kind of its own, like the
/// readiness probe's, so a sweep is one range read rather than a walk.
pub const IDEMPOTENCY_KIND: &str = "idempotency";

/// How long a key is remembered.
///
/// A day, the same window AWS holds a client token for. Long enough that a
/// retry after an outage still lands, short enough that the store is not an
/// archive of every create ever made.
pub const KEY_LIFETIME_MS: u64 = 24 * 60 * 60 * 1000;

/// How long an unanswered **claim** is believed.
///
/// The claim is written before the create is attempted, so that two attempts
/// arriving together cannot both do the work — and a process that dies in
/// between leaves one behind with nobody to finish it. Believed for the day a
/// finished record is believed, that claim is a key its owner can never spend:
/// every retry is told the first attempt is still in flight, forever, and the
/// object they asked for is never made.
///
/// Two minutes: longer than any create this platform accepts takes to answer
/// (it answers with an operation and converges afterwards, so the synchronous
/// half is a handful of store writes), and short enough that a crash costs a
/// retry rather than a day. Past it the claim is taken over, which is the
/// compare-and-set the claim path already does.
pub const CLAIM_LIFETIME_MS: u64 = 2 * 60 * 1000;

/// The longest key that will be accepted.
///
/// A UUID is 36 characters; this leaves room for a caller who prefixes theirs
/// with something meaningful, and refuses one that is trying to use the header
/// as storage.
pub const LONGEST_KEY: usize = 255;

/// What was answered the first time, and what was asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replay {
    /// When the original create was accepted. The record expires from here.
    pub at: Timestamp,
    /// The request this key was spent on. A second request under the same key
    /// is refused unless it hashes to this.
    pub fingerprint: String,
    /// The operation the first attempt was given, so the retry can wait on the
    /// same one it would have waited on.
    ///
    /// `None` is a **claim**: written before the create is attempted, so that
    /// two attempts arriving together cannot both do the work. The second one
    /// finds this and is told the first is still in flight, which is a truthful
    /// answer it can retry on — rather than a duplicate object, which is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<serde_json::Value>,
    /// The object the first attempt made. Empty while the claim is open.
    #[serde(default)]
    pub target: String,
}

impl Replay {
    /// Claimed, but not yet answered: a create under this key is in flight.
    pub fn in_flight(&self) -> bool {
        self.operation.is_none()
    }

    /// A claim nobody came back for. See [`CLAIM_LIFETIME_MS`]: the first
    /// attempt died between claiming the key and answering, and the claim is
    /// taken over rather than believed until the key expires.
    pub fn abandoned(&self, now: Timestamp) -> bool {
        self.in_flight() && now.0.saturating_sub(self.at.0) >= CLAIM_LIFETIME_MS
    }

    /// Past its day. An expired record is treated as absent rather than
    /// deleted on the read path: the create that follows overwrites it, which
    /// is one write instead of two.
    pub fn expired(&self, now: Timestamp) -> bool {
        now.0.saturating_sub(self.at.0) >= KEY_LIFETIME_MS
    }
}

/// The key as it will be honoured, or why it will not be.
///
/// Whitespace either side is trimmed, because a header copied out of a shell
/// variable often carries a newline and refusing that teaches nothing. What is
/// refused is a key that is empty, too long, or carries control characters —
/// the last because the key becomes part of a store key, and a caller must not
/// be able to reach into that namespace with one.
pub fn check_key(raw: &str) -> Result<&str, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("an idempotency key cannot be empty".into());
    }
    if key.len() > LONGEST_KEY {
        return Err(format!(
            "an idempotency key is at most {LONGEST_KEY} characters, and this one is {}",
            key.len()
        ));
    }
    if let Some(bad) = key.chars().find(|c| c.is_control()) {
        return Err(format!(
            "an idempotency key holds no control characters, and this one holds {bad:?}"
        ));
    }
    Ok(key)
}

/// The name one caller's key is recorded under.
///
/// Hashed rather than spelled out: the key is the caller's own string, and a
/// store key built by concatenation is one a caller can escape from with a
/// slash. The hash also bounds the length, which the key itself does not.
pub fn record_id(subject: &str, parent: &str, kind: &str, key: &str) -> String {
    let mut h = Sha256::new();
    // Length-prefixed, so that no two different tuples can hash the same by
    // moving a slash from one field to the next.
    for part in [subject, parent, kind, key] {
        h.update(part.len().to_be_bytes());
        h.update(part.as_bytes());
    }
    format!("{:x}", h.finalize())
}

/// A hash of the request, stable across two encodings of the same document.
///
/// Written out by hand rather than serialised, because two clients that send
/// the same object with the fields in a different order are making the same
/// request and must not be told otherwise. Maps are walked in key order;
/// everything else in the order it is.
pub fn fingerprint(body: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    absorb(&mut h, body);
    format!("{:x}", h.finalize())
}

fn absorb(h: &mut Sha256, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => h.update([0u8]),
        Value::Bool(b) => {
            h.update([1u8]);
            h.update([u8::from(*b)]);
        }
        Value::Number(n) => {
            h.update([2u8]);
            h.update(n.to_string().as_bytes());
        }
        Value::String(s) => {
            h.update([3u8]);
            h.update(s.len().to_be_bytes());
            h.update(s.as_bytes());
        }
        Value::Array(items) => {
            h.update([4u8]);
            h.update(items.len().to_be_bytes());
            for item in items {
                absorb(h, item);
            }
        }
        Value::Object(fields) => {
            h.update([5u8]);
            h.update(fields.len().to_be_bytes());
            // `serde_json`'s map is ordered by key unless the crate is built
            // with `preserve_order`; sorting here does not depend on that.
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            for key in keys {
                h.update(key.len().to_be_bytes());
                h.update(key.as_bytes());
                absorb(h, &fields[key]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_key_is_trimmed_and_kept() {
        assert_eq!(check_key("  abc\n").unwrap(), "abc");
    }

    #[test]
    fn an_empty_or_oversized_or_binary_key_is_refused() {
        assert!(check_key("   ").is_err());
        assert!(check_key(&"x".repeat(LONGEST_KEY + 1)).is_err());
        assert!(check_key("a\u{7}b").is_err());
    }

    /// The same request written two ways is the same request.
    #[test]
    fn field_order_does_not_change_the_fingerprint() {
        let one = json!({"spec": {"vcpus": 2, "memoryMib": 1024}, "meta": {"name": "a"}});
        let other = json!({"meta": {"name": "a"}, "spec": {"memoryMib": 1024, "vcpus": 2}});
        assert_eq!(fingerprint(&one), fingerprint(&other));
    }

    /// And a different one is not — including the difference that matters
    /// most, a number that moved.
    #[test]
    fn a_changed_field_changes_the_fingerprint() {
        let one = json!({"spec": {"vcpus": 2}});
        let other = json!({"spec": {"vcpus": 4}});
        assert_ne!(fingerprint(&one), fingerprint(&other));
    }

    /// A string and a one-element list holding it are not the same request,
    /// which a hash that just concatenated bytes would have said they were.
    #[test]
    fn shape_is_part_of_the_fingerprint() {
        assert_ne!(fingerprint(&json!("a")), fingerprint(&json!(["a"])));
        assert_ne!(
            fingerprint(&json!({"a": "b"})),
            fingerprint(&json!({"ab": ""}))
        );
    }

    /// Two callers who picked the same string do not share a record.
    #[test]
    fn a_record_is_one_callers() {
        let mine = record_id("alice", "projects/p", "instances", "k");
        let theirs = record_id("bob", "projects/p", "instances", "k");
        assert_ne!(mine, theirs);
        assert_eq!(mine.len(), 64, "a record name is a whole digest");
    }

    /// And a key cannot be moved from one field into the next to collide.
    #[test]
    fn the_fields_cannot_be_slid_into_each_other() {
        assert_ne!(
            record_id("a", "b", "instances", "k"),
            record_id("ab", "", "instances", "k"),
        );
    }

    #[test]
    fn a_record_expires_after_its_day() {
        let made = Replay {
            at: Timestamp(1_000_000),
            fingerprint: String::new(),
            operation: None,
            target: String::new(),
        };
        assert!(made.in_flight(), "a record with no answer is a claim");
        assert!(!made.expired(Timestamp(1_000_000 + KEY_LIFETIME_MS - 1)));
        assert!(made.expired(Timestamp(1_000_000 + KEY_LIFETIME_MS)));
    }
}
