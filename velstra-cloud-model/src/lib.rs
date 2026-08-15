//! The resource model, the access rules, and the decisions — all pure.
//!
//! Nothing in this crate touches a store, a socket or a clock beyond reading
//! the time for a timestamp. That is deliberate: the parts of a cloud platform
//! that are hard to get right are decisions, and decisions that need a cluster
//! to test are decisions that get tested once and then trusted forever.
//!
//! Start with [`meta`] for the three invariants everything else follows from.

pub mod access;
pub mod meta;
pub mod migration;
pub mod reconcile;
pub mod resources;

pub use access::{WriteRefused, Writer};
pub use meta::{Condition, ConditionStatus, Meta, Placement, ResourceName, Revision, Timestamp};
pub use resources::{Observed, Resource};
