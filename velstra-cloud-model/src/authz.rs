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
//! ## Roles, and why these
//!
//! A ladder, not a matrix, because the matrix is what nobody reads. Each rung
//! is a distinction somebody running a fleet actually makes:
//!
//! | Role | May | Cannot |
//! |---|---|---|
//! | `viewer` | look at everything in the project | change anything |
//! | `operator` | run what is already there — start, stop, resize, attach, open a console | bring anything into existence or take it away |
//! | `editor` | that, and create and delete | change who may |
//! | `admin` | everything, including the bindings | leave the project's own limits, which are the cell's |
//!
//! `operator` is the rung a platform serving customers needs and a single-tenant
//! one does not: the people who keep an estate running are usually not the
//! people who decide what it consists of, and a role that could only do both was
//! a role handed out too widely. It is the difference between rebooting a
//! machine and deleting it.
//!
//! **A project `admin` is not a cell operator.** The cell's operators are named
//! in the cell's own configuration; they may do anything anywhere, and they are
//! the provider. A project admin is a customer's own administrator, and there
//! are things they deliberately cannot do to their own project — raise its
//! quota, or step outside the policy the cell set for it. Everything below says
//! "cell operator" in full whenever it means the former.

use serde::{Deserialize, Serialize};

use crate::meta::ResourceName;

/// What a request wants to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Get, list, watch.
    Read,
    /// Change something that already exists: start a guest, resize it, attach a
    /// volume, open its console.
    ///
    /// Kept apart from `Write` because "may run the estate" and "may decide what
    /// the estate consists of" are different jobs in every organisation large
    /// enough to have both — and a platform that could not express the
    /// difference forced the second on anybody who needed the first.
    Operate,
    /// Bring something into existence, or take it away.
    Write,
    /// Change who else may — kept apart from the rest so that an editor cannot
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
    /// Run what is already there, and bring nothing new into existence.
    ///
    /// Start a guest, stop it, resize it, attach a volume, open a console. Not
    /// create one, and not delete one — which is the whole point: the people
    /// who keep an estate running are usually not the people who decide what it
    /// consists of.
    Operator,
    /// Read and change everything in the project, except who may.
    Editor,
    /// Everything, including the bindings themselves.
    Admin,
}

impl Role {
    /// Whether this role admits `verb`.
    ///
    /// Written as a full match with no wildcard on the pair, deliberately: a new
    /// verb is a compile error here until somebody says what each role does
    /// about it. A wildcard would silently grant it to whichever rung the
    /// catch-all sat on, which is how a permission system gets a hole nobody
    /// wrote down.
    pub fn admits(self, verb: Verb) -> bool {
        match (self, verb) {
            (Role::Viewer, Verb::Read) => true,
            (Role::Viewer, Verb::Operate | Verb::Write | Verb::Administer) => false,

            (Role::Operator, Verb::Read | Verb::Operate) => true,
            (Role::Operator, Verb::Write | Verb::Administer) => false,

            (Role::Editor, Verb::Read | Verb::Operate | Verb::Write) => true,
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
    }

    #[test]
    fn a_node_belongs_to_the_cell_and_not_to_a_tenant() {
        // A tenant with Admin on their project must not be able to drain a
        // hypervisor, and a node is not inside anybody's project.
        for name in ["nodes/node-a", "pools/pool-a"] {
            let n = ResourceName::parse(name).unwrap();
            // Outside every project, and therefore the cell operator's.
            assert_eq!(governing_project(&n), None, "{name}");
        }
    }
}

#[cfg(test)]
mod the_ladder {
    use super::*;

    /// The whole role table, written out once, so what each rung means is a
    /// fact this file asserts rather than a sentence in a doc comment.
    ///
    /// A change to any cell of it is a change somebody has to make here on
    /// purpose — which is the point of writing it out rather than checking a
    /// few interesting cases.
    #[test]
    fn each_rung_admits_exactly_what_its_name_says() {
        use Role::*;
        use Verb::*;
        let table = [
            //          Read  Operate Write  Administer
            (Viewer, [true, false, false, false]),
            (Operator, [true, true, false, false]),
            (Editor, [true, true, true, false]),
            (Admin, [true, true, true, true]),
        ];
        for (role, expected) in table {
            for (verb, want) in [Read, Operate, Write, Administer].into_iter().zip(expected) {
                assert_eq!(
                    role.admits(verb),
                    want,
                    "{role:?} and {verb:?}: the table says {want}"
                );
            }
        }
    }

    /// The one escalation a role system has to be closed against.
    ///
    /// An editor who could change the bindings would be an admin one request
    /// later, and an operator who could would be one two requests later.
    #[test]
    fn nobody_below_admin_can_grant_themselves_more() {
        for role in [Role::Viewer, Role::Operator, Role::Editor] {
            assert!(!role.admits(Verb::Administer), "{role:?}");
        }
    }

    /// The rung that exists for a platform with customers.
    ///
    /// Somebody who keeps an estate running reboots machines and does not
    /// delete them. Before this rung there was no way to say that, so anybody
    /// who needed to restart a guest was given the ability to destroy one.
    #[test]
    fn an_operator_may_run_the_estate_and_not_change_what_it_consists_of() {
        assert!(Role::Operator.admits(Verb::Operate));
        assert!(!Role::Operator.admits(Verb::Write));
    }

    /// A role name that arrives misspelled must land on the least, not the
    /// most. Serde's default is `Viewer`, and this pins it: a binding whose
    /// role failed to parse grants looking and nothing else.
    #[test]
    fn a_role_nobody_recognises_is_a_viewer() {
        let binding: Binding =
            serde_json::from_str(r#"{"role":"viewer","members":["ada"]}"#).expect("a binding");
        assert_eq!(binding.role, Role::Viewer);
        assert_eq!(Role::default(), Role::Viewer);
    }

    /// Every role spells the same on the wire as in the console and the
    /// contract. A rename here is a rename everywhere, and this is where it
    /// gets noticed.
    #[test]
    fn the_role_names_are_the_ones_written_down() {
        for (role, name) in [
            (Role::Viewer, "viewer"),
            (Role::Operator, "operator"),
            (Role::Editor, "editor"),
            (Role::Admin, "admin"),
        ] {
            assert_eq!(serde_json::to_string(&role).unwrap(), format!("\"{name}\""));
        }
    }
}
