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
    /// Somebody changed something: created it, edited it, or deleted it.
    ///
    /// The other three kinds are about *access*; this one is about the estate.
    /// It exists because an operation is minted only when an object is
    /// created, so "who deleted that instance" and "who changed the project's
    /// bindings" had no answer anywhere in the platform — the two questions
    /// asked first after an incident.
    Changed,
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
    /// What they were doing with it: `read`, `write` or `administer` for a
    /// refusal, and `create`, `update` or `delete` for a change.
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
/// The id prefixes an audit record can start with, in the order the store
/// holds them.
///
/// Public because a range scan needs them: an id is `{kind}-{minute}-{hash}`,
/// so the keys sort by kind first and by minute inside each kind. A caller
/// asking for a time range has to seek once per kind, and this is the list to
/// seek over.
pub const KINDS: &[&str] = &["changed", "refused", "signin", "signout"];

/// The exclusive cursor a range scan for `kind` from `at` starts after.
///
/// `{kind}-{minute}` with no hash, which is a **prefix** of every id written in
/// that minute — and a prefix sorts below everything that extends it. The
/// store's cursor is exclusive, so starting after this key includes the whole
/// minute rather than skipping it. That is why nothing is subtracted here: an
/// off-by-one in the other direction would drop a minute of records, silently.
///
/// The minute is decimal and unpadded, so this ordering holds only while every
/// minute has the same number of digits. That is eight digits from March 1989
/// to April 2160, which covers every record this platform will ever hold.
/// Stated rather than hidden: the alternative is a padded id nobody can read,
/// for a problem nobody here will have.
pub fn seek_from(kind: &str, at: Timestamp) -> String {
    format!("{kind}-{}", at.0 / 60_000)
}

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
        AuditKind::Changed => "changed",
    };
    format!("{kind}-{minute}-{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    /// The seek key sorts below every record of that minute and above every
    /// record of the minute before — which is the whole of what a range scan
    /// needs from it.
    ///
    /// The minutes here are eight digits, as every minute between 1989 and
    /// 2160 is: the ordering is lexicographic and only holds at a fixed width,
    /// which `seek_from` says out loud.
    #[test]
    fn the_seek_key_lands_just_below_the_minute_asked_for() {
        let minute = 29_814_793u64;
        let at = Timestamp(minute * MIN + 30_000);
        let seek = seek_from("refused", at);
        let inside = record_id(AuditKind::Refused, "a", "read", "t", at);
        let before = record_id(
            AuditKind::Refused,
            "a",
            "read",
            "t",
            Timestamp((minute - 2) * MIN),
        );
        assert!(seek.as_str() < inside.as_str(), "{seek} !< {inside}");
        assert!(before.as_str() < seek.as_str(), "{before} !< {seek}");
    }

    /// Every kind a record can be written as is one this list names — a kind
    /// missing here is a range scan that silently skips a quarter of the log.
    #[test]
    fn every_kind_is_in_the_list_a_scan_seeks_over() {
        for kind in [
            AuditKind::Refused,
            AuditKind::SignedIn,
            AuditKind::SignedOut,
            AuditKind::Changed,
        ] {
            let id = record_id(kind, "a", "read", "t", Timestamp(0));
            assert!(
                KINDS.iter().any(|k| id.starts_with(&format!("{k}-"))),
                "{id} starts with no known kind"
            );
        }
    }

    /// The store holds them in this order, so a scan that walks the list in
    /// order walks the store in order.
    #[test]
    fn the_kinds_are_listed_in_the_order_the_store_holds_them() {
        let mut sorted = KINDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KINDS);
    }

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
