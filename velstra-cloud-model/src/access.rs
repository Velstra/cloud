//! Who may write which half of a resource — enforced, not documented.
//!
//! Every write carries the identity of the writer, and the store refuses one
//! that touches the other half. This is the whole of invariant 1, and it is
//! deliberately checked here as a pure function so it is unit-testable without
//! a store, a network, or a cluster.
//!
//! The rule in one line: **a controller may change `spec` and metadata; the
//! owning agent may change `status`; neither may change the other's half.**
//!
//! Why enforce rather than trust: every "stuck in PENDING" bug in the systems
//! this replaces is two writers disagreeing about one field. A controller sets
//! `attaching`, the agent sets `attached`, a third party sets `error`, and now
//! the object's state is a function of who wrote last rather than of what is
//! true. If only one party can write a field, that class of bug is unreachable.

use serde::{Deserialize, Serialize};

/// The identity a write is made under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Writer {
    /// A controller or the API on an operator's behalf. Owns `spec` and
    /// metadata (labels, finalizers, deletion).
    Controller(String),
    /// The agent on a node. Owns `status` for the objects assigned to it, and
    /// nothing else — including for objects assigned to a different node.
    Agent { node: String },
}

impl Writer {
    pub fn controller(who: &str) -> Self {
        Self::Controller(who.to_string())
    }

    pub fn agent(node: &str) -> Self {
        Self::Agent {
            node: node.to_string(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Controller(who) => format!("controller {who}"),
            Self::Agent { node } => format!("agent on {node}"),
        }
    }
}

/// Why a write was refused. Each variant names a specific confusion, because
/// "permission denied" on a write path is how an operator ends up guessing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefused {
    #[error("{writer} changed spec; only a controller may")]
    SpecIsNotYours { writer: String },
    #[error("{writer} changed status; only the agent that owns the object may")]
    StatusIsNotYours { writer: String },
    #[error("agent on {writer} changed status of an object assigned to {owner}")]
    NotYourObject { writer: String, owner: String },
    #[error("{writer} changed metadata; only a controller may")]
    MetaIsNotYours { writer: String },
    #[error("generation moved without spec changing")]
    GenerationWithoutChange,
    #[error("spec changed without the generation moving")]
    ChangeWithoutGeneration,
    #[error(
        "{writer} created an object; only a controller may create one — an agent reports on \
         objects a controller brought into being, and never invents one, so it can never assert \
         ownership by creating it either"
    )]
    CreateIsNotYours { writer: String },
    #[error("{writer} deleted an object; only a controller may")]
    DeleteIsNotYours { writer: String },
}

/// What changed between the stored copy and the one being written.
///
/// Computed by the caller (which has the concrete types) and judged here, so
/// this stays free of every resource type in the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub spec: bool,
    pub status: bool,
    pub meta: bool,
    pub generation: bool,
}

/// Who may speak for an object's `status`.
///
/// Two names, because ownership has two sides and using only one deadlocks:
///
/// * `assigned` — the node a **controller** put in `spec`. This is the ask, and
///   it is trustworthy because only a controller can write it.
/// * `owner` — the node the **status** currently names. This is the fact.
///
/// The fact wins while it exists, and the ask is what breaks the tie when it does
/// not:
///
/// * **Nobody owns it yet** — the assignee may claim it. Requiring prior
///   ownership here is a deadlock, because ownership can only come from the
///   report that ownership would be needed to make, and nothing would ever
///   start.
/// * **Somebody owns it** — only that node may write, even when a controller
///   has already re-assigned the object elsewhere. During a migration the old
///   node still has the guest running; letting the new one write status then
///   would put two agents on one field and, worse, invite the new one to act on
///   a claim the old one has not yet given up. The handover completes in the
///   only safe order: the owner reports that it has let go, and the assignee
///   picks it up on the next pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ownership<'a> {
    pub assigned: Option<&'a str>,
    pub owner: Option<&'a str>,
}

impl<'a> Ownership<'a> {
    pub fn of(assigned: Option<&'a str>, owner: Option<&'a str>) -> Self {
        Self { assigned, owner }
    }

    fn admits(&self, node: &str) -> bool {
        match self.owner {
            Some(owner) => owner == node,
            None => self.assigned == Some(node),
        }
    }

    fn describe(&self) -> String {
        match (self.assigned, self.owner) {
            (None, None) => "nobody".to_string(),
            (Some(a), None) => a.to_string(),
            (None, Some(o)) => o.to_string(),
            (Some(a), Some(o)) if a == o => a.to_string(),
            (Some(a), Some(o)) => format!("{o} (handing over to {a})"),
        }
    }
}

/// Judge a write.
pub fn judge(writer: &Writer, changed: Changed, held: Ownership<'_>) -> Result<(), WriteRefused> {
    match writer {
        Writer::Controller(_) => {
            if changed.status {
                return Err(WriteRefused::StatusIsNotYours {
                    writer: writer.describe(),
                });
            }
            // A spec change without a new generation is invisible to every agent
            // — they compare `observed_generation` and would see nothing to do.
            if changed.spec && !changed.generation {
                return Err(WriteRefused::ChangeWithoutGeneration);
            }
            // …and the reverse makes every agent redo work for nothing, and
            // makes "still converging" mean nothing.
            if changed.generation && !changed.spec {
                return Err(WriteRefused::GenerationWithoutChange);
            }
            Ok(())
        }
        Writer::Agent { node } => {
            if changed.spec {
                return Err(WriteRefused::SpecIsNotYours {
                    writer: writer.describe(),
                });
            }
            if changed.meta || changed.generation {
                return Err(WriteRefused::MetaIsNotYours {
                    writer: writer.describe(),
                });
            }
            // An agent reporting on an object that is not assigned to it is
            // either a stale assignment it has not noticed, or two agents
            // fighting over one object after a failed migration. Both must fail
            // loudly rather than let the last writer win.
            if held.admits(node) {
                Ok(())
            } else {
                Err(WriteRefused::NotYourObject {
                    writer: node.clone(),
                    owner: held.describe(),
                })
            }
        }
    }
}

/// Judge bringing an object into existence.
///
/// A create is a controller's act — it writes spec and metadata, which an agent
/// never does.
///
/// So an agent that creates one is refused, and that single rule is what closes
/// the ownership-smuggle: the way a compromised node would hand itself an object
/// is by *creating* one whose status already names it as owner, bypassing the
/// claim that ownership normally has to be earned through. An agent that cannot
/// create anything cannot create a pre-owned object either — the smuggle has no
/// door, rather than a door with a guard on it. A controller is trusted with
/// spec and metadata and may create; in this platform it always does so with an
/// empty status (the API forces it, and every controller writes
/// `Status::default()`), so nothing is lost by not policing a controller's
/// create for a claim it never makes.
pub fn judge_create(writer: &Writer) -> Result<(), WriteRefused> {
    match writer {
        Writer::Controller(_) => Ok(()),
        Writer::Agent { .. } => Err(WriteRefused::CreateIsNotYours {
            writer: writer.describe(),
        }),
    }
}

/// Judge removing an object.
///
/// Deletion is a metadata decision — it sets `deleted_at` and then, once the
/// finalizers are gone, takes the object away — and metadata is the controller's
/// half. An agent reports on objects; it never asks for one to be gone, so an
/// agent delete is refused for the same reason an agent's metadata write is. The
/// stored ownership is not consulted: even the node that runs an instance may
/// not delete it, because a delete is a decision about the object rather than a
/// report about the machine.
pub fn judge_delete(writer: &Writer) -> Result<(), WriteRefused> {
    match writer {
        Writer::Controller(_) => Ok(()),
        Writer::Agent { node } => Err(WriteRefused::DeleteIsNotYours {
            writer: format!("agent on {node}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: Changed = Changed {
        spec: true,
        status: false,
        meta: false,
        generation: true,
    };
    const STATUS: Changed = Changed {
        spec: false,
        status: true,
        meta: false,
        generation: false,
    };

    fn held(owner: Option<&str>) -> Ownership<'_> {
        Ownership::of(owner, owner)
    }

    #[test]
    fn a_controller_owns_spec_and_an_agent_owns_status() {
        assert!(judge(&Writer::controller("scheduler"), SPEC, held(Some("node-a"))).is_ok());
        assert!(judge(&Writer::agent("node-a"), STATUS, held(Some("node-a"))).is_ok());
    }

    #[test]
    fn a_node_may_claim_an_object_a_controller_assigned_to_it() {
        // The first report. Requiring existing ownership here is a deadlock:
        // the fact can only come from the report, and the report would need the
        // fact. The assignment in `spec` is the trustworthy half, because only a
        // controller can have written it.
        let fresh = Ownership::of(Some("node-a"), None);
        assert!(judge(&Writer::agent("node-a"), STATUS, fresh).is_ok());
        assert!(
            judge(&Writer::agent("node-b"), STATUS, fresh).is_err(),
            "a node claimed an object assigned to another"
        );
    }

    #[test]
    fn a_handover_waits_for_the_old_owner_to_let_go() {
        // Re-assigned to node-b while node-a still holds it. node-a keeps the
        // pen — it is the one that can still see the guest — and node-b is
        // refused until the owner is cleared. Allowing both would put two
        // agents on one field, which is the whole failure this design removes.
        let moving = Ownership::of(Some("node-b"), Some("node-a"));
        assert!(judge(&Writer::agent("node-a"), STATUS, moving).is_ok());
        assert!(
            judge(&Writer::agent("node-b"), STATUS, moving).is_err(),
            "the new node wrote status while the old one still had the guest"
        );

        // Once node-a has reported that it let go, node-b may take over.
        let released = Ownership::of(Some("node-b"), None);
        assert!(judge(&Writer::agent("node-b"), STATUS, released).is_ok());
        assert!(judge(&Writer::agent("node-a"), STATUS, released).is_err());
    }

    #[test]
    fn neither_may_write_the_other_half() {
        assert_eq!(
            judge(
                &Writer::controller("scheduler"),
                STATUS,
                held(Some("node-a"))
            ),
            Err(WriteRefused::StatusIsNotYours {
                writer: "controller scheduler".into()
            })
        );
        assert_eq!(
            judge(&Writer::agent("node-a"), SPEC, held(Some("node-a"))),
            Err(WriteRefused::SpecIsNotYours {
                writer: "agent on node-a".into()
            })
        );
    }

    #[test]
    fn an_agent_may_not_report_on_somebody_elses_object() {
        // The shape of a half-finished migration: the old node still believes
        // the instance is his. Letting the last writer win is how an instance
        // ends up running in two places with one status.
        assert_eq!(
            judge(&Writer::agent("node-a"), STATUS, held(Some("node-b"))),
            Err(WriteRefused::NotYourObject {
                writer: "node-a".into(),
                owner: "node-b".into()
            })
        );
        assert!(judge(&Writer::agent("node-a"), STATUS, held(None)).is_err());
    }

    #[test]
    fn only_a_controller_may_create_and_never_with_a_status() {
        // A controller brings objects into being — the ordinary case.
        assert!(judge_create(&Writer::controller("api")).is_ok());

        // An agent may not create at all: it reports on objects a controller
        // brought into being, and inventing one is exactly the escalation the
        // per-node boundary exists to stop. This is also what makes an ownership
        // smuggle impossible — a node cannot create a pre-owned object because it
        // cannot create an object.
        assert_eq!(
            judge_create(&Writer::agent("node-a")),
            Err(WriteRefused::CreateIsNotYours {
                writer: "agent on node-a".into()
            })
        );
    }

    #[test]
    fn only_a_controller_may_delete() {
        assert!(judge_delete(&Writer::controller("api")).is_ok());
        // Even the node that runs the object may not delete it: a delete is a
        // decision about the object, not a report about the machine.
        assert_eq!(
            judge_delete(&Writer::agent("node-a")),
            Err(WriteRefused::DeleteIsNotYours {
                writer: "agent on node-a".into()
            })
        );
    }

    #[test]
    fn a_spec_change_and_its_generation_travel_together() {
        let spec_only = Changed {
            spec: true,
            generation: false,
            ..Default::default()
        };
        assert_eq!(
            judge(&Writer::controller("api"), spec_only, held(None)),
            Err(WriteRefused::ChangeWithoutGeneration),
            "a spec change no agent can notice is worse than no change"
        );
        let gen_only = Changed {
            generation: true,
            ..Default::default()
        };
        assert_eq!(
            judge(&Writer::controller("api"), gen_only, held(None)),
            Err(WriteRefused::GenerationWithoutChange),
            "a bumped generation with nothing behind it makes every agent redo its work"
        );
    }
}
