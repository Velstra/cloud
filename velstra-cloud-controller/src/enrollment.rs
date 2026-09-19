//! Letting an unanswered announcement go.
//!
//! A machine that announced itself and was never looked at is a row in a list
//! an operator reads. Without something to retire them, a rack flashed by
//! mistake leaves forty of those for ever, and the list that exists to show
//! what is waiting for a decision fills with things nobody will ever decide.
//!
//! So: past its hour, an unanswered announcement is marked `Expired`. That is
//! the whole of this controller, and the two things it does *not* do are the
//! interesting half.
//!
//! **It does not delete.** A row is a record that a machine asked, and an
//! operator who wonders what became of the box in slot four should be able to
//! see that it asked and nobody answered. The console does not offer deletion
//! either, for the same reason.
//!
//! **It does not decide anything the claim path relies on.** `claimable`
//! checks expiry against the clock, not against this phase — a sweep that has
//! not run yet is not a reason to let a machine in, and one that has run is
//! not what keeps it out. This controller writes down what the clock already
//! decided, so a person reading the list sees it; the door does its own
//! arithmetic. Two places computing the same thing would be a bug here, and
//! this is the shape where it is not: one of them is the authority and the
//! other is a label.
//!
//! An expired machine announces again on its next pass and lands on its own
//! row — the id comes from its key — so the cost of retiring one early is a
//! row that comes back, not a machine that cannot join.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    enrollment::{EnrollmentPhase, EnrollmentSpec, EnrollmentStatus},
    meta::Timestamp,
    resources::Resource,
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "enrollment";

pub struct EnrollmentController {
    enrollments: TypedStore<EnrollmentSpec, EnrollmentStatus>,
    nodes: TypedStore<
        velstra_cloud_model::resources::NodeSpec,
        velstra_cloud_model::resources::NodeStatus,
    >,
    now: std::sync::Arc<dyn Fn() -> Timestamp + Send + Sync>,
}

impl EnrollmentController {
    pub fn new(
        enrollments: TypedStore<EnrollmentSpec, EnrollmentStatus>,
        nodes: TypedStore<
            velstra_cloud_model::resources::NodeSpec,
            velstra_cloud_model::resources::NodeStatus,
        >,
    ) -> Self {
        Self {
            enrollments,
            nodes,
            now: std::sync::Arc::new(Timestamp::now),
        }
    }

    pub fn with_clock(mut self, now: impl Fn() -> Timestamp + Send + Sync + 'static) -> Self {
        self.now = std::sync::Arc::new(now);
        self
    }
}

/// What this pass should write on a row, if anything.
///
/// Pure, so the rules are tested without a store: which rows are retired,
/// which are left alone, and the one case that looks like it should be retired
/// and must not be.
pub fn expire(
    spec: &EnrollmentSpec,
    status: &EnrollmentStatus,
    now: u64,
) -> Option<EnrollmentPhase> {
    // Over is over. A claimed machine is a node now, and a refused one is an
    // answer somebody gave; neither becomes "expired" later.
    if status.phase.settled() {
        return None;
    }
    if spec.refused {
        return Some(EnrollmentPhase::Refused);
    }
    if status.expires_at.0 != 0 && now >= status.expires_at.0 {
        return Some(EnrollmentPhase::Expired);
    }
    // Approved and not yet collected. It looks stale and it is not: the
    // machine polls, and retiring a row somebody has already said yes to would
    // undo their decision on a timer. The clock still governs the claim — an
    // approval does not extend the hour — so this is a row that will either be
    // claimed or expire like any other.
    if spec.approved && status.phase != EnrollmentPhase::Approved {
        return Some(EnrollmentPhase::Approved);
    }
    None
}

impl Reconciler for EnrollmentController {
    type Spec = EnrollmentSpec;
    type Status = EnrollmentStatus;

    fn name(&self) -> &'static str {
        "enrollment"
    }

    async fn reconcile(
        &self,
        _name: &str,
        object: Option<&Resource<Self::Spec, Self::Status>>,
    ) -> Result<()> {
        let Some(row) = object else {
            return Ok(());
        };
        if row.meta.is_deleting() {
            return Ok(());
        }
        let now = (self.now)().0;
        let Some(phase) = expire(&row.spec, &row.status, now) else {
            return Ok(());
        };
        let mut next = row.clone();
        next.status.phase = phase;
        next.status.observed_generation = row.meta.generation;
        self.enrollments
            .update(&next, &Writer::controller(WHO))
            .await?;
        // The Node the announcement made, if nobody ever said yes to it.
        //
        // An unapproved row is an object with no credential, not schedulable,
        // and now with nobody waiting on it — a machine that asked and was
        // ignored. Leaving it would fill the one list an operator reads with
        // rows for machines that never joined, which is the thing that made
        // this whole feature worth building. Only a row this controller can
        // see was made by an announcement: the label says so, and it is
        // removed when a claim takes the node into service.
        if matches!(phase, EnrollmentPhase::Expired | EnrollmentPhase::Refused) {
            self.let_go_of_the_node(row.meta.name.id()).await;
        }
        info!(
            enrollment = %row.meta.name,
            fingerprint = %row.status.fingerprint,
            phase = ?phase,
            "an announcement moved on",
        );
        Ok(())
    }
}

impl EnrollmentController {
    /// Remove the Node an unanswered announcement made, and nothing else.
    ///
    /// Matched by the label rather than by name: a node that has been claimed
    /// loses the label, and one an operator made by hand never had it. So a
    /// machine that joined and was later decommissioned under the same name is
    /// not swept away by an old enrolment expiring — which would be the worst
    /// possible version of this, quietly and an hour late.
    ///
    /// Best-effort: an enrolment that expired and left a row behind is untidy,
    /// and an expiry that failed because of it would be worse.
    async fn let_go_of_the_node(&self, enrolment: &str) {
        let Ok(nodes) = self.nodes.list().await else {
            return;
        };
        for node in nodes {
            let holds = node
                .meta
                .labels
                .get(velstra_cloud_model::enrollment::AWAITING_LABEL)
                .is_some_and(|v| v == enrolment);
            if !holds || node.meta.is_deleting() {
                continue;
            }
            let _ = self
                .nodes
                .delete(
                    &node.meta.name.to_string(),
                    // The revision it was read at, so a node an operator was
                    // editing at that moment is not swept out from under them.
                    node.meta.revision,
                    &Writer::controller(WHO),
                )
                .await;
            info!(node = %node.meta.name, "nobody answered, so the machine's row goes too");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(now: u64) -> (EnrollmentSpec, EnrollmentStatus) {
        (
            EnrollmentSpec::default(),
            velstra_cloud_model::enrollment::announced("AAAA", "", Default::default(), now),
        )
    }

    /// Inside the hour, nothing happens. A machine that announced a minute ago
    /// is waiting, which is what the list is for.
    #[test]
    fn a_fresh_announcement_is_left_alone() {
        let now = 1_700_000_000_000;
        let (spec, status) = waiting(now);
        assert_eq!(expire(&spec, &status, now + 60_000), None);
    }

    /// Past it, retired — so the list an operator reads is what is actually
    /// waiting for them.
    #[test]
    fn an_unanswered_announcement_is_retired_after_its_hour() {
        let now = 1_700_000_000_000;
        let (spec, status) = waiting(now);
        assert_eq!(
            expire(&spec, &status, now + 3_600_001),
            Some(EnrollmentPhase::Expired)
        );
    }

    /// Approved and not yet collected is **not** stale. The machine polls, and
    /// a sweep that retired a row somebody said yes to would undo their
    /// decision on a timer.
    #[test]
    fn an_approved_machine_is_not_retired_for_being_slow() {
        let now = 1_700_000_000_000;
        let (mut spec, mut status) = waiting(now);
        spec.approved = true;
        status.phase = EnrollmentPhase::Approved;
        assert_eq!(expire(&spec, &status, now + 60_000), None);
    }

    /// The phase catches up with an approval, so the console shows one.
    #[test]
    fn approving_moves_the_phase() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = waiting(now);
        spec.approved = true;
        assert_eq!(expire(&spec, &status, now), Some(EnrollmentPhase::Approved));
    }

    /// A refusal is written down, so the row says what happened rather than
    /// sitting at Pending with a flag nobody reads.
    #[test]
    fn a_refusal_is_written_on_the_row() {
        let now = 1_700_000_000_000;
        let (mut spec, status) = waiting(now);
        spec.refused = true;
        assert_eq!(expire(&spec, &status, now), Some(EnrollmentPhase::Refused));
    }

    /// Over is over. A machine that collected its credential is a node now,
    /// and does not become "expired" an hour later.
    #[test]
    fn a_claimed_machine_is_never_retired() {
        let now = 1_700_000_000_000;
        let (spec, mut status) = waiting(now);
        status.phase = EnrollmentPhase::Claimed;
        assert_eq!(expire(&spec, &status, now + 86_400_000), None);
        status.phase = EnrollmentPhase::Refused;
        assert_eq!(expire(&spec, &status, now + 86_400_000), None);
        status.phase = EnrollmentPhase::Expired;
        assert_eq!(expire(&spec, &status, now + 86_400_000), None);
    }
}
