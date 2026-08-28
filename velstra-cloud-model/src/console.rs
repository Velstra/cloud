//! A way into a guest, for when the network is not one.
//!
//! ## The gap this closes
//!
//! Until this existed, a guest's serial line went to a file and nowhere else.
//! That is enough to *watch* a machine fail and nothing at all to reach one: a
//! guest whose network never came up, whose SSH key was never installed, or
//! whose firewall locked the door could be observed failing in perfect detail
//! and not touched. Every hypervisor worth the name has an answer to this and
//! this one did not.
//!
//! ## Why the ticket is an object
//!
//! The browser talks to the API and the API talks to the node — the node is not
//! reachable from a tenant's browser in any deployment worth designing for, and
//! it must not be. So the API opens a connection *to* the node, and the node has
//! to know that the caller is the API and that this particular console was
//! actually granted.
//!
//! A shared secret would need distributing; a signing key would need publishing
//! and rotating. Both are new machinery for a question this platform already
//! answers everywhere else: **put it in an object**. The API mints a ticket and
//! writes a session naming the guest, the node, who asked, and when it stops
//! being valid. The node already reads the cell, so it already has the session
//! by the time the connection arrives, and it checks the ticket against it.
//!
//! Three things fall out of that for free:
//!
//! * **Nothing new is trusted.** No key exchange, no second credential, no
//!   channel that is not already there.
//! * **It is on the record.** Who opened a console into which guest, and when,
//!   is an object an operator can list — which is what anybody investigating
//!   "who was on that machine" actually needs.
//! * **It is level-triggered like everything else.** A node that restarts
//!   mid-session re-reads the sessions and knows exactly as much as before.
//!
//! ## The ticket is stored hashed, and that is not decoration
//!
//! Every node agent in the cell may read the cell. A session carrying the
//! ticket in the clear would hand every node a credential that opens a console
//! into a guest on a *different* node — so the object carries
//! `ticket_sha256` and the ticket itself exists only in the answer to the
//! request that minted it and in the connection that spends it.
//!
//! ## One ticket, one attach
//!
//! A session is spent when it is attached to, and the node says so by writing
//! `status.attached_at`. A second connection presenting the same ticket is
//! refused rather than joined: two people on one serial line type over each
//! other, and a ticket that can be replayed is a ticket worth stealing.

use serde::{Deserialize, Serialize};

use crate::meta::Timestamp;

/// How long a minted ticket may go unused, in milliseconds.
///
/// A minute. It is spent by the browser opening a connection immediately after
/// asking for it, so a minute is generous for the round trip and short enough
/// that a ticket recovered from a log or a proxy is worthless by the time
/// anybody reads it.
pub const TICKET_LIFETIME_MS: u64 = 60_000;

/// "Let this person at that guest's serial line."
// Snake case on the way out, like every other spec here: the contract's
// camelCase is the wire layer's business (`velstra_cloud_wire`), applied in both
// directions. A model type that renamed its own fields would be translated
// twice on the way in and not at all on the way out — which is exactly what
// happened, and read as "missing field `expiresAt`" on a node that could not
// deserialise a session the API had written correctly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsoleSessionSpec {
    /// The guest whose line this opens.
    pub instance: String,
    /// The node holding it, derived by the API rather than asked for.
    ///
    /// It is the assignee: only the machine with the guest has the socket.
    /// Without it the object is assigned to nobody, and the access rule refuses
    /// every agent that tries to report on it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// Who asked. Not used to decide anything — the decision was made when this
    /// object was created — but this is the field somebody investigating a
    /// machine reads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    /// The ticket, hashed. **Never the ticket.** See the module note.
    ///
    /// Always written, even when empty. A `skip_serializing_if` here would keep
    /// the field out of a default document — and out of the guard that walks
    /// every field name in the model, which is what would have caught this
    /// field's name not surviving the wire before a node ever refused a console
    /// over it.
    #[serde(default)]
    pub ticket_sha256: String,
    /// When an unspent ticket stops being one.
    pub expires_at: Timestamp,
    /// Whether the holder may type, or only watch.
    ///
    /// A viewer gets a window; somebody who may change the guest gets a
    /// keyboard. It is on the session rather than decided at the node because
    /// the node has no bindings to read and must not be the place a permission
    /// question is answered.
    #[serde(default)]
    pub read_only: bool,
}

/// What the node says about it. The node owns this status; nobody else writes
/// it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsoleSessionStatus {
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default)]
    pub conditions: Vec<crate::meta::Condition>,
    /// When the ticket was spent. Set once, by the node that accepted the
    /// connection — and the reason a replay is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_at: Option<Timestamp>,
    /// When the connection ended. A session with both timestamps is a finished
    /// visit; one with neither is a ticket nobody used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at: Option<Timestamp>,
    /// The node that claimed it, which is the node the guest is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Whether a presented ticket opens this session, at this moment.
///
/// Everything the node needs to decide, in one place and as a pure function, so
/// that the decision is testable without a socket and cannot drift between the
/// place that checks and the place that explains.
///
/// The comparison is over the **hash**, and it is deliberately not
/// short-circuiting on length: a ticket is compared with a constant-time
/// equality so that the time taken to refuse says nothing about how much of it
/// was right.
pub fn admits(
    session: &ConsoleSessionSpec,
    status: &ConsoleSessionStatus,
    presented: &str,
    node: &str,
    now: Timestamp,
) -> Result<(), Refused> {
    if session.node != node {
        return Err(Refused::AnotherNode);
    }
    if status.attached_at.is_some() {
        return Err(Refused::Spent);
    }
    if now.0 >= session.expires_at.0 {
        return Err(Refused::Expired);
    }
    if !constant_time_eq(&sha256_hex(presented), &session.ticket_sha256) {
        return Err(Refused::WrongTicket);
    }
    Ok(())
}

/// Why a console was not opened.
///
/// Named cases rather than a string, because the node logs one of these on
/// every refusal and an operator reading a journal full of them needs to tell
/// "somebody's browser was slow" from "somebody is guessing tickets".
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("this session is for a guest on another node")]
    AnotherNode,
    #[error("this ticket has already been used; ask for another")]
    Spent,
    #[error("this ticket has expired; ask for another")]
    Expired,
    #[error("that is not this session's ticket")]
    WrongTicket,
}

/// The hex sha256 of a ticket, which is the only form of it that is ever
/// stored.
pub fn sha256_hex(ticket: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(ticket.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Equality that takes the same time whichever way it comes out.
///
/// Over two hex digests, so the lengths are equal in every real case and a
/// mismatch in length is simply a no. The point is the loop: an early return on
/// the first differing byte would make the time taken a measurement of how many
/// leading characters were right, which is all somebody guessing needs.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut differences = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        differences |= x ^ y;
    }
    differences == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(node: &str, ticket: &str, expires: u64) -> ConsoleSessionSpec {
        ConsoleSessionSpec {
            instance: "projects/p/instances/i1".into(),
            node: node.into(),
            subject: "ada".into(),
            ticket_sha256: sha256_hex(ticket),
            expires_at: Timestamp(expires),
            read_only: false,
        }
    }

    #[test]
    fn the_right_ticket_on_the_right_node_in_time_opens_it() {
        let spec = session("node-a", "s3cret", 1_000);
        let status = ConsoleSessionStatus::default();
        assert_eq!(
            admits(&spec, &status, "s3cret", "node-a", Timestamp(500)),
            Ok(())
        );
    }

    /// The stored object never carries the ticket, so a node that reads the
    /// cell — every node does — learns nothing it could spend.
    #[test]
    fn the_object_does_not_carry_the_ticket() {
        let spec = session("node-a", "s3cret", 1_000);
        let json = serde_json::to_string(&spec).unwrap();
        assert!(!json.contains("s3cret"), "{json}");
        assert!(json.contains(&sha256_hex("s3cret")), "{json}");
    }

    #[test]
    fn a_ticket_is_spent_once() {
        let spec = session("node-a", "s3cret", 1_000);
        let status = ConsoleSessionStatus {
            attached_at: Some(Timestamp(400)),
            ..ConsoleSessionStatus::default()
        };
        assert_eq!(
            admits(&spec, &status, "s3cret", "node-a", Timestamp(500)),
            Err(Refused::Spent)
        );
    }

    #[test]
    fn an_expired_ticket_is_not_a_ticket() {
        let spec = session("node-a", "s3cret", 1_000);
        let status = ConsoleSessionStatus::default();
        assert_eq!(
            admits(&spec, &status, "s3cret", "node-a", Timestamp(1_000)),
            Err(Refused::Expired)
        );
    }

    /// The check every node in the cell relies on. Sessions are readable by all
    /// of them; a node that served one belonging to another node's guest would
    /// be opening a console it has no business opening — and would find no
    /// socket anyway, which is exactly the kind of "it does not work" that
    /// hides a permission mistake.
    #[test]
    fn a_session_for_another_nodes_guest_is_refused_here() {
        let spec = session("node-b", "s3cret", 1_000);
        let status = ConsoleSessionStatus::default();
        assert_eq!(
            admits(&spec, &status, "s3cret", "node-a", Timestamp(500)),
            Err(Refused::AnotherNode)
        );
    }

    #[test]
    fn a_wrong_ticket_is_refused_however_nearly_right_it_is() {
        let spec = session("node-a", "s3cret", 1_000);
        let status = ConsoleSessionStatus::default();
        for guess in ["", "s3cret ", "S3cret", "s3crf t", &"a".repeat(64)] {
            assert_eq!(
                admits(&spec, &status, guess, "node-a", Timestamp(500)),
                Err(Refused::WrongTicket),
                "{guess:?}"
            );
        }
    }

    #[test]
    fn the_comparison_does_not_stop_at_the_first_wrong_character() {
        // Not a timing measurement — those are unreliable in a test suite. What
        // is checked is the property the implementation must have: every byte is
        // read whatever the answer, so the two calls do the same work.
        let right = sha256_hex("s3cret");
        // Flip the last character to something it certainly is not, rather
        // than to a fixed digit that it might already be.
        let last = right.chars().last().unwrap();
        let other = if last == 'f' { '0' } else { 'f' };
        let nearly = format!("{}{other}", &right[..right.len() - 1]);
        assert!(!constant_time_eq(&right, &nearly));
        assert!(constant_time_eq(&right, &right));
        assert!(!constant_time_eq(&right, ""));
    }
}
