//! One loop, written once, and the controllers that sit in it.
//!
//! Every controller here is the same three lines of thinking: read the object,
//! ask a pure function in [`velstra_cloud_model::reconcile`] what should be
//! true of it, write the difference with a compare-and-swap. The decisions are
//! not here — they are there, unit-tested without a cluster. What is here is
//! the machinery that runs them: a work queue, a watch that recovers, a resync
//! that makes a missed event a latency problem rather than a correctness one.
//!
//! The properties that machinery has to have, and what each one prevents:
//!
//! * **Level-triggered.** A reconcile is a function of what is stored now, not
//!   of the event that woke it. A missed event costs latency until the next
//!   resync; it never costs correctness. This is also why a resync over a
//!   settled cluster writes nothing at all — every controller compares before
//!   it writes, and there is nothing to compare unequal.
//! * **Idempotent.** Running a reconcile twice is running it once. There is
//!   nothing to resume after a crash, because nothing was ever half-written:
//!   each pass is one compare-and-swap on one object, which either happened or
//!   did not.
//! * **Backed off per object.** One object that can never reconcile costs one
//!   slot in a rate-limited queue, not a core and not its neighbours' latency.

pub mod address;
pub mod alerts;
pub mod attachment;
pub mod backoff;
pub mod backup_schedule;
pub mod capture;
pub mod ceph;
pub mod disk;
pub mod drift;
pub mod election;
pub mod evacuation;
pub mod floating_ip;
pub mod instance;
pub mod load_balancer;
pub mod metrics;
pub mod migration;
pub mod network;
pub mod operations;
pub mod port;
pub mod queue;
pub mod imagesource;
pub mod quota;
pub mod recovery;
pub mod router;
pub mod runner;
pub mod scheduler;
pub mod snapshot;
pub mod snapshot_schedule;
pub mod status;
pub mod volume;
pub mod wiring;

pub use metrics::Metrics;
pub use runner::{LoopConfig, Reconciler, Related, run, run_when_leading, sweep};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] velstra_cloud_store::StoreError),
    #[error(transparent)]
    Typed(#[from] velstra_cloud_store::typed::TypedError),
    /// A write this crate refused before the store saw it — see
    /// [`status::StatusWriter`].
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether this is somebody else having written first.
    ///
    /// Not a failure, and deliberately not counted as one: a conflict means the
    /// object moved under us, so the answer is to read it again and decide
    /// again — immediately, subject only to the queue's rate limit. Backing off
    /// on a conflict would make a contended object the slowest one in the
    /// cluster, which is the opposite of what it needs.
    pub fn is_conflict(&self) -> bool {
        use velstra_cloud_store::StoreError;
        matches!(
            self,
            Error::Store(StoreError::Conflict { .. })
                | Error::Typed(velstra_cloud_store::typed::TypedError::Store(
                    StoreError::Conflict { .. }
                ))
        )
    }

    /// Whether the object went away between the read and the write. Nothing to
    /// do and nothing to repair — somebody deleted it, which is allowed.
    pub fn is_missing(&self) -> bool {
        use velstra_cloud_store::StoreError;
        matches!(
            self,
            Error::Store(StoreError::Missing { .. })
                | Error::Typed(velstra_cloud_store::typed::TypedError::Missing(_))
        )
    }
}
