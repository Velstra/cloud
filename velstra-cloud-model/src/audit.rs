//! What was refused, and who was told no.
//!
//! ## What this is not
//!
//! It is **not** a log of everything that happened. Every successful write in
//! this platform already creates an `operation` carrying its target, its verb,
//! who asked for it and when it finished — that is the record of what was done,
//! it is a first-class object, and duplicating it here would give an operator
//! two logs that will eventually disagree about one afternoon.
//!
//! What no operation exists for is a request that was **refused**. Nothing is
//! created, nothing is changed, and the only trace is an HTTP status somebody
//! else received. That is precisely the event a multi-tenant platform is asked
//! about afterwards: who tried to read another tenant's guests, and when did
//! they start.
//!
//! Sign-ins are here for the same reason — a session that begins leaves no
//! object behind either.
//!
//! ## Why it cannot be flooded
//!
//! A refusal is a thing an attacker can cause on purpose, so a record per
//! refusal is a way to fill somebody's store from the outside. Instead the
//! name is **derived** from who, what, which verb and *which minute* — so a
//! thousand attempts in one minute collide on create and leave one record. The
//! exact count is lost and the fact is not, which is the right way round: an
//! operator asked "did this happen, and from when" needs the second one.
//!
//! ## Why nothing expires it
//!
//! Refusals are rare in a working cell — they are mistakes and attacks — and
//! sign-ins are bounded by how many people there are. So the volume is small,
//! and nothing here deletes anything: an audit record that quietly expired
//! before somebody came looking is worse than a disk somebody can see filling.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// What kind of thing is being recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditKind {
    /// Somebody was told no.
    #[default]
    Refused,
    /// A session began.
    SignedIn,
    /// A session ended, by the person or by having its user's password changed.
    SignedOut,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditSpec {
    pub kind: AuditKind,
    /// Who. The subject from the token, as the API knows it.
    pub subject: String,
    /// What they were reaching for. Empty for a sign-in, which is about no
    /// particular object.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// `read`, `write`, `administer` — what they were trying to do with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verb: String,
    /// The sentence they were given.
    ///
    /// The *same* sentence, deliberately. An audit line that paraphrases the
    /// refusal is one an operator has to correlate by hand against what the
    /// person actually saw.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// When. Stamped by the API rather than derived from `meta.createdAt`,
    /// because the minute in the name is a coarse bucket and this is not.
    pub at: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditStatus {
    pub observed_generation: u64,
    pub conditions: Vec<crate::meta::Condition>,
}

/// The id for one record.
///
/// Derived, and that is what bounds the collection: who, what, which verb and
/// which minute. A burst of refusals in one minute is one record, because the
/// second create collides with the first.
///
/// Hashed rather than spelled out, because a subject can be an email and a
/// target is a resource name — both carry characters a resource id may not, and
/// a name built by mangling them would collide in ways nobody could predict.
/// The fields are all on the object; the id only has to be stable and unique.
pub fn record_id(
    kind: AuditKind,
    subject: &str,
    verb: &str,
    target: &str,
    at: Timestamp,
) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let minute = at.0 / 60_000;
    let mut h = DefaultHasher::new();
    subject.hash(&mut h);
    verb.hash(&mut h);
    target.hash(&mut h);
    minute.hash(&mut h);
    let kind = match kind {
        AuditKind::Refused => "refused",
        AuditKind::SignedIn => "signin",
        AuditKind::SignedOut => "signout",
    };
    format!("{kind}-{minute}-{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    /// A burst in one minute is one record; the next minute is another.
    ///
    /// This is the whole flood defence. A refusal is something an attacker can
    /// cause at will, so one record per refusal would be a way to fill a store
    /// from the outside.
    #[test]
    fn repeats_within_a_minute_collapse_and_the_next_minute_does_not() {
        let a = record_id(
            AuditKind::Refused,
            "alice@example.com",
            "read",
            "projects/p2/instances/i1",
            Timestamp(10 * MIN + 1),
        );
        let same_minute = record_id(
            AuditKind::Refused,
            "alice@example.com",
            "read",
            "projects/p2/instances/i1",
            Timestamp(10 * MIN + 59_000),
        );
        assert_eq!(a, same_minute, "a burst in one minute made two records");

        let next = record_id(
            AuditKind::Refused,
            "alice@example.com",
            "read",
            "projects/p2/instances/i1",
            Timestamp(11 * MIN),
        );
        assert_ne!(a, next, "a refusal a minute later was lost");
    }

    /// Different people, verbs and targets are different records.
    ///
    /// Collapsing any of these would hide the thing an operator is looking
    /// for: one person sweeping a project is not the same event as everybody
    /// hitting one object.
    #[test]
    fn who_what_and_which_verb_each_make_a_record_of_their_own() {
        let base = |subject, verb, target| {
            record_id(
                AuditKind::Refused,
                subject,
                verb,
                target,
                Timestamp(10 * MIN),
            )
        };
        let one = base("alice", "read", "projects/p2/instances/i1");
        assert_ne!(one, base("bob", "read", "projects/p2/instances/i1"));
        assert_ne!(one, base("alice", "write", "projects/p2/instances/i1"));
        assert_ne!(one, base("alice", "read", "projects/p2/instances/i2"));
    }

    /// A sign-in and a refusal by the same person in the same minute are two
    /// records.
    #[test]
    fn a_sign_in_is_not_confused_with_a_refusal() {
        let at = Timestamp(10 * MIN);
        assert_ne!(
            record_id(AuditKind::Refused, "alice", "read", "", at),
            record_id(AuditKind::SignedIn, "alice", "read", "", at)
        );
    }

    /// The id is a resource id: no slashes, no `@`, nothing a name would
    /// refuse.
    ///
    /// Subjects are email addresses and targets are resource names, and an id
    /// built by mangling those is one that collides in ways nobody predicts.
    #[test]
    fn an_id_survives_being_a_resource_name() {
        let id = record_id(
            AuditKind::Refused,
            "alice@example.com",
            "read",
            "projects/p2/instances/i1",
            Timestamp(10 * MIN),
        );
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id}"
        );
    }
}
