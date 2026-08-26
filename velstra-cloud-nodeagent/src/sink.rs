//! Where a status report goes — the write half of [`crate::cell`].
//!
//! An agent makes exactly one kind of write: it reports the status of an object
//! it owns. Reads have two destinations already ([`crate::cell::StoreCell`] and
//! [`crate::cell::ApiCell`]); this is the matching seam for the write, so `--api`
//! mode is a real trust boundary and not a reader bolted onto a writer that
//! still holds the operator's own store.
//!
//! Two destinations, and the difference is *who enforces the ownership rule*:
//!
//! * The store, directly. The agent writes as `Writer::agent(node)` and the
//!   store's own [`velstra_cloud_model::access::judge`] refuses a write that is
//!   not this node's. This is the single-operator default: the token that
//!   reaches the store is the operator's, so the writer identity is trusted, not
//!   verified.
//!
//! * The API, over HTTP, presenting this node's own token. The API authenticates
//!   the token as *this node* and runs the same judgement server-side — so a
//!   compromised node holds a credential that can write only its own objects'
//!   status, which the operator token never was.
//!
//! The agent does not know which it is talking to, and does not need to: it
//! hands a full object to `write_status`, only the status lands, and the verdict
//! comes back as a [`SinkOutcome`] the pass counts.

use async_trait::async_trait;
use serde_json::Value;
use velstra_cloud_model::access::Writer;

/// What the far end made of a report, in the four shapes a pass distinguishes.
///
/// The same four a direct-store write already produces (see
/// [`crate::reporting`]): a write accepted, a lost compare-and-swap that the
/// next pass redoes, an ownership refusal that means two parties disagree about
/// who runs the object, and everything else.
#[derive(Debug)]
pub enum SinkOutcome {
    /// The status was written.
    Wrote,
    /// A compare-and-swap was lost — somebody changed the object while this pass
    /// acted. Not an error; the next pass reads the new one.
    Conflict,
    /// Refused on ownership grounds, with the sentence saying why. Always worth
    /// an operator's attention: two parties believe they run one object.
    Refused(String),
    /// Anything else — the store or the API could not be reached, or answered in
    /// a way this could not read.
    Failed(String),
}

/// Where a node agent sends the status it observed.
#[async_trait]
pub trait StatusSink: Send + Sync + 'static {
    /// Report `object` — a full `meta`/`spec`/`status` document — as `writer`.
    /// Only the status is written; the destination keeps the stored spec and
    /// metadata and enforces that the writer may speak for the object.
    async fn write_status(&self, kind: &str, object: &Value, writer: &Writer) -> SinkOutcome;

    /// What this writes to, for a log line at startup.
    fn describe(&self) -> String;
}
