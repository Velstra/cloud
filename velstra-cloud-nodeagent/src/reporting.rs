//! Writing status, and what the store's answer means.
//!
//! Shared by every agent in this crate — the node agent and the pool agent —
//! because "report what I see" and "say this is mine now" are the only two
//! writes any agent ever makes, and they must mean the same thing in both. Two
//! copies would drift on the interesting case: what a *refusal* implies.

use serde::{Serialize, de::DeserializeOwned};
use velstra_cloud_model::{
    access::Writer,
    resources::{Assigned, Observed, Resource},
};
use velstra_cloud_store::{StoreError, TypedStore, typed::TypedError};

use crate::agent::Pass;

/// Report the observed status, and say what the store thought of it.
///
/// A status that has not changed is not written. That is what makes a converged
/// agent quiet, and a quiet agent is what makes the resync interval a matter of
/// taste rather than a load knob.
pub(crate) async fn report<S, T>(
    store: &TypedStore<S, T>,
    stored: &Resource<S, T>,
    next: Resource<S, T>,
    writer: &Writer,
    pass: &mut Pass,
) where
    S: Serialize + DeserializeOwned + PartialEq + Assigned + Send + Sync,
    T: Serialize + DeserializeOwned + PartialEq + Observed + Send + Sync,
{
    if next.status == stored.status {
        return;
    }
    match store.update(&next, writer).await {
        Ok(_) => pass.reports += 1,
        Err(TypedError::Store(StoreError::Conflict { .. })) => {
            // Somebody changed the object while this pass was acting. The next
            // pass reads the new one; there is nothing to resume.
            tracing::debug!(name = %next.meta.name, "status write lost a race; the next pass redoes it");
            pass.conflicts += 1;
        }
        Err(TypedError::Refused(why)) => {
            tracing::warn!(name = %next.meta.name, %why, "the store refused this agent's report");
            pass.refused += 1;
        }
        Err(e) => {
            tracing::warn!(name = %next.meta.name, error = %e, "could not report status");
            pass.failures += 1;
        }
    }
}

/// Say "this is mine now" and let the store answer.
///
/// Nothing on the machine happens first. An object whose status somebody else
/// still owns is an object they have not let go of, and acting on it in the
/// meantime is how two parties end up holding one thing.
pub(crate) async fn claim<S, T>(
    store: &TypedStore<S, T>,
    stored: &Resource<S, T>,
    take_ownership: impl FnOnce(&mut T),
    writer: &Writer,
    pass: &mut Pass,
) where
    S: Serialize + DeserializeOwned + PartialEq + Clone + Assigned + Send + Sync,
    T: Serialize + DeserializeOwned + PartialEq + Observed + Clone + Send + Sync,
{
    let mut next = stored.clone();
    take_ownership(&mut next.status);
    match store.update(&next, writer).await {
        Ok(_) => pass.reports += 1,
        Err(TypedError::Refused(why)) => {
            tracing::warn!(
                name = %stored.meta.name, %why,
                "assigned here, but its status belongs to somebody else; \
                 doing nothing until that is resolved"
            );
            pass.refused += 1;
        }
        Err(TypedError::Store(StoreError::Conflict { .. })) => pass.conflicts += 1,
        Err(e) => {
            tracing::warn!(name = %stored.meta.name, error = %e, "could not claim");
            pass.failures += 1;
        }
    }
}
