//! Whether a caller may act at all.
//!
//! Deliberately **not** [`crate::access`], which answers a different question:
//! that one says which *half* of an object a writer owns — spec or status — and
//! it applies to controllers and agents inside the platform. This one says
//! whether a request from outside should be served, and it applies to people and
//! to whatever they point at the API.
//!
//! Both are pure functions for the same reason: an authorisation rule that can
//! only be exercised against a running cluster is an authorisation rule nobody
//! tests the edges of.
//!
//! ## The shape
//!
//! A **project is the unit of tenancy**, so a project carries the bindings that
//! decide who may do what inside it. A resource under `projects/p1` is governed
//! by `p1`'s bindings, whatever its kind — an instance, a port, a volume and a
//! snapshot are all one tenant's business or none of it.
//!
//! Resources **outside** any project — nodes, pools, the projects collection
//! itself — are the cell operator's, and are governed by a list of subjects the
//! cell is started with. That list is configuration rather than data on purpose:
//! it is what a fresh cell is bootstrapped from, and a permission stored inside
//! the thing it protects has no answer for the first request.
//!
//! ## Why three roles and not a permission matrix
//!
//! Because the matrix is what nobody reads. Three roles cover what a tenant
//! actually distinguishes — look, change, and decide who else may — and a fourth
//! is easy to add the day something needs it. Starting from a per-verb,
//! per-collection grid would be a lot of surface for a platform whose access
//! story has, until now, been that everyone may do everything.

use serde::{Deserialize, Serialize};

use crate::meta::ResourceName;

/// What a request wants to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Get, list, watch.
    Read,
    /// Create, update, delete.
    Write,
    /// Change who else may — kept apart from `Write` so that an editor cannot
    /// grant themselves more than they were given, which is the one escalation
    /// a role system has to be closed against.
    Administer,
}

/// What a member has been granted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    /// Read everything in the project and change nothing.
    #[default]
    Viewer,
    /// Read and change everything in the project, except who may.
    Editor,
    /// Everything, including the bindings themselves.
    Admin,
}

impl Role {
    /// Whether this role admits `verb`.
    pub fn admits(self, verb: Verb) -> bool {
        match (self, verb) {
            (Role::Viewer, Verb::Read) => true,
            (Role::Viewer, _) => false,
            (Role::Editor, Verb::Read | Verb::Write) => true,
            (Role::Editor, Verb::Administer) => false,
            (Role::Admin, _) => true,
        }
    }
}

/// One grant: a role, and who holds it.
///
/// Members are subjects exactly as the token verifier reports them, and nothing
/// here interprets their shape. A cell using OIDC has emails or service-account
/// names in here; a development cell has whatever its static verifier says. A
/// binding that tried to parse them would be a second opinion about identity,
/// and the one place identity is decided is the verifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub role: Role,
    pub members: Vec<String>,
}

/// Why a request was refused, in words the caller gets to see.
///
/// Deliberately the same sentence whether the resource exists or not: an error
/// that distinguishes "you may not read this project" from "there is no such
/// project" is an oracle for enumerating other tenants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied(pub String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether `subject` may `verb` a resource governed by `bindings`.
///
/// `cell_admins` is the operator list the cell was started with; a subject on it
/// may do anything, including inside every project. That is what an operator
/// is, and pretending otherwise would mean a cell nobody can repair.
pub fn may(
    subject: &str,
    cell_admins: &[String],
    bindings: &[Binding],
    verb: Verb,
) -> Result<(), Denied> {
    if cell_admins.iter().any(|a| a == subject) {
        return Ok(());
    }
    let granted = bindings
        .iter()
        .filter(|b| b.members.iter().any(|m| m == subject))
        .any(|b| b.role.admits(verb));
    if granted {
        Ok(())
    } else {
        Err(Denied(
            "no permission on this resource, or it does not exist".into(),
        ))
    }
}

/// The project whose bindings govern a resource, if any.
///
/// `None` means the resource is outside every project — a node, a pool, the
/// projects collection itself — and is therefore the cell operator's.
pub fn governing_project(name: &ResourceName) -> Option<String> {
    // A project governs itself, so that granting somebody Admin on `projects/p1`
    // is what lets them manage `p1` rather than needing a grant somewhere above.
    if name.collection() == "projects" {
        return Some(name.to_string());
    }
    name.project().map(|p| format!("projects/{p}"))
}

/// Whether a resource is outside every project and therefore the operator's.
pub fn is_cell_scoped(name: &ResourceName) -> bool {
    governing_project(name).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(role: Role, member: &str) -> Vec<Binding> {
        vec![Binding {
            role,
            members: vec![member.to_string()],
        }]
    }

    #[test]
    fn a_viewer_may_look_and_change_nothing() {
        let b = bindings(Role::Viewer, "ada");
        assert!(may("ada", &[], &b, Verb::Read).is_ok());
        assert!(may("ada", &[], &b, Verb::Write).is_err());
        assert!(may("ada", &[], &b, Verb::Administer).is_err());
    }

    #[test]
    fn an_editor_may_not_grant_themselves_more() {
        // The one escalation a role system has to be closed against: an editor
        // who could write the bindings would be an admin one request later.
        let b = bindings(Role::Editor, "ada");
        assert!(may("ada", &[], &b, Verb::Write).is_ok());
        assert!(
            may("ada", &[], &b, Verb::Administer).is_err(),
            "an editor could change who may"
        );
    }

    #[test]
    fn somebody_with_no_binding_may_do_nothing() {
        let b = bindings(Role::Admin, "ada");
        for verb in [Verb::Read, Verb::Write, Verb::Administer] {
            assert!(may("bob", &[], &b, verb).is_err(), "{verb:?}");
        }
    }

    #[test]
    fn the_refusal_does_not_say_whether_the_thing_exists() {
        // An error that distinguishes "not yours" from "not there" is an oracle
        // for enumerating other tenants' resources.
        let denied = may("bob", &[], &[], Verb::Read).unwrap_err();
        assert!(denied.0.contains("does not exist"), "{denied}");
        let also = may("bob", &[], &bindings(Role::Admin, "ada"), Verb::Read).unwrap_err();
        assert_eq!(denied, also, "the two refusals can be told apart");
    }

    #[test]
    fn a_cell_operator_may_act_inside_every_project() {
        // Anything else is a cell nobody can repair.
        let admins = vec!["ops".to_string()];
        assert!(may("ops", &admins, &[], Verb::Administer).is_ok());
        assert!(may("ops", &admins, &bindings(Role::Viewer, "ada"), Verb::Write).is_ok());
    }

    #[test]
    fn two_grants_to_one_subject_are_the_wider_of_them() {
        let b = vec![
            Binding {
                role: Role::Viewer,
                members: vec!["ada".into()],
            },
            Binding {
                role: Role::Editor,
                members: vec!["ada".into(), "bob".into()],
            },
        ];
        assert!(may("ada", &[], &b, Verb::Write).is_ok());
        assert!(may("bob", &[], &b, Verb::Write).is_ok());
        assert!(may("bob", &[], &b, Verb::Administer).is_err());
    }

    #[test]
    fn a_project_governs_itself_and_everything_under_it() {
        let under = ResourceName::parse("projects/p1/instances/i1").unwrap();
        assert_eq!(governing_project(&under).as_deref(), Some("projects/p1"));
        let itself = ResourceName::parse("projects/p1").unwrap();
        assert_eq!(governing_project(&itself).as_deref(), Some("projects/p1"));
        assert!(!is_cell_scoped(&under));
    }

    #[test]
    fn a_node_belongs_to_the_cell_and_not_to_a_tenant() {
        // A tenant with Admin on their project must not be able to drain a
        // hypervisor, and a node is not inside anybody's project.
        for name in ["nodes/node-a", "pools/pool-a"] {
            let n = ResourceName::parse(name).unwrap();
            assert!(is_cell_scoped(&n), "{name}");
            assert_eq!(governing_project(&n), None, "{name}");
        }
    }
}
