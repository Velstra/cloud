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
///
/// `Default` is `Read`, and it is the least of them on purpose: a grant whose
/// verb did not parse grants the smallest thing there is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verb {
    /// Get, list, watch.
    #[default]
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

/// The prefix a role the cell defined carries in a binding.
pub const CUSTOM_ROLE_PREFIX: &str = "roles/";

/// What a member has been granted.
///
/// One field on the wire, not two. `"editor"` is a rung; `"roles/db-operator"`
/// is a role the cell wrote down. A binding with a `role` *and* a `customRole`
/// would be two fields with one meaning, and every one of those in this platform
/// has eventually been read in the wrong order by something.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
    /// A role the cell defined: what it grants is written down in a `roles/…`
    /// object, per collection.
    ///
    /// The four rungs above answer "what may this person do" with one word for
    /// the whole project. That is the right shape for most people and the wrong
    /// one for the case every operator eventually meets: somebody who may
    /// restart the database machines and must not touch the network. A fifth
    /// rung cannot express that — the difference is not *how much* but *what*.
    Custom(String),
}

impl serde::Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Ok(Role::from(text.as_str()))
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Viewer => f.write_str("viewer"),
            Role::Operator => f.write_str("operator"),
            Role::Editor => f.write_str("editor"),
            Role::Admin => f.write_str("admin"),
            Role::Custom(name) => f.write_str(name),
        }
    }
}

impl From<&str> for Role {
    /// A name that is neither a rung nor a `roles/…` reference reads as
    /// `Viewer`.
    ///
    /// The lenient half of a deliberate pair: a typo lands on the *least*, never
    /// the most, so a mangled binding cannot be an escalation. The strict half is
    /// at the door — the API refuses a binding naming a role that does not exist,
    /// which is where somebody can still be told about their typo.
    fn from(text: &str) -> Self {
        match text {
            "operator" => Role::Operator,
            "editor" => Role::Editor,
            "admin" => Role::Admin,
            "viewer" => Role::Viewer,
            other if other.starts_with(CUSTOM_ROLE_PREFIX) => Role::Custom(other.to_string()),
            _ => Role::Viewer,
        }
    }
}

impl Role {
    /// Whether this role admits `verb`.
    ///
    /// Written as a full match with no wildcard on the pair, deliberately: a new
    /// verb is a compile error here until somebody says what each role does
    /// about it. A wildcard would silently grant it to whichever rung the
    /// catch-all sat on, which is how a permission system gets a hole nobody
    /// wrote down.
    pub fn admits(&self, verb: Verb, kind: &str, defined: &[CustomRole]) -> bool {
        match (self, verb) {
            (Role::Viewer, Verb::Read) => true,
            (Role::Viewer, Verb::Operate | Verb::Write | Verb::Administer) => false,

            (Role::Operator, Verb::Read | Verb::Operate) => true,
            (Role::Operator, Verb::Write | Verb::Administer) => false,

            (Role::Editor, Verb::Read | Verb::Operate | Verb::Write) => true,
            (Role::Editor, Verb::Administer) => false,

            (Role::Admin, _) => true,

            // A role nobody defined grants nothing. Not an error here: this is
            // the *read* path, and a binding naming a role that has since been
            // deleted has to resolve to the least rather than refuse the whole
            // request — the other members of that project still hold theirs.
            (Role::Custom(name), _) => defined
                .iter()
                .find(|r| &r.name == name)
                .is_some_and(|r| r.admits(verb, kind)),
        }
    }
}

/// A role the cell wrote down: what it lets somebody do, collection by
/// collection.
///
/// **Always narrower than a rung, by construction.** Every grant names the
/// collections it applies to and the list may not be empty — there is no
/// wildcard. Somebody who wants "everything" has four of those already, and a
/// custom role that could mean it would be a second spelling of `admin` with no
/// way to tell them apart in a list.
///
/// The cell's, never a project's. A tenant who could define a role could define
/// one granting more than they hold, and the whole point of keeping `Administer`
/// apart is that an editor cannot widen themselves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomRole {
    /// `roles/db-operator` — the same text a binding carries.
    pub name: String,
    pub grants: Vec<Grant>,
}

/// One verb, over the collections it applies to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub verb: Verb,
    /// At least one, and no wildcard. See [`CustomRole`].
    pub collections: Vec<String>,
}

impl CustomRole {
    /// Whether this role admits `verb` on `kind`.
    ///
    /// `Read` is implied by every other verb on the same collection, and that is
    /// not convenience: a person who may start a guest and may not read it is
    /// looking at a console that shows nothing and a button that works, which is
    /// worse than either half alone.
    pub fn admits(&self, verb: Verb, kind: &str) -> bool {
        self.grants.iter().any(|g| {
            g.collections.iter().any(|c| c == kind)
                && (g.verb == verb || (verb == Verb::Read && g.verb != Verb::Administer))
        })
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
    kind: &str,
    defined: &[CustomRole],
) -> Result<(), Denied> {
    if cell_admins.iter().any(|a| a == subject) {
        return Ok(());
    }
    let granted = bindings
        .iter()
        .filter(|b| b.members.iter().any(|m| m == subject))
        .any(|b| b.role.admits(verb, kind, defined));
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
        assert!(may("ada", &[], &b, Verb::Read, "instances", &[]).is_ok());
        assert!(may("ada", &[], &b, Verb::Write, "instances", &[]).is_err());
        assert!(may("ada", &[], &b, Verb::Administer, "instances", &[]).is_err());
    }

    #[test]
    fn an_editor_may_not_grant_themselves_more() {
        // The one escalation a role system has to be closed against: an editor
        // who could write the bindings would be an admin one request later.
        let b = bindings(Role::Editor, "ada");
        assert!(may("ada", &[], &b, Verb::Write, "instances", &[]).is_ok());
        assert!(
            may("ada", &[], &b, Verb::Administer, "instances", &[]).is_err(),
            "an editor could change who may"
        );
    }

    #[test]
    fn somebody_with_no_binding_may_do_nothing() {
        let b = bindings(Role::Admin, "ada");
        for verb in [Verb::Read, Verb::Write, Verb::Administer] {
            assert!(
                may("bob", &[], &b, verb, "instances", &[]).is_err(),
                "{verb:?}"
            );
        }
    }

    #[test]
    fn the_refusal_does_not_say_whether_the_thing_exists() {
        // An error that distinguishes "not yours" from "not there" is an oracle
        // for enumerating other tenants' resources.
        let denied = may("bob", &[], &[], Verb::Read, "instances", &[]).unwrap_err();
        assert!(denied.0.contains("does not exist"), "{denied}");
        let also = may(
            "bob",
            &[],
            &bindings(Role::Admin, "ada"),
            Verb::Read,
            "instances",
            &[],
        )
        .unwrap_err();
        assert_eq!(denied, also, "the two refusals can be told apart");
    }

    #[test]
    fn a_cell_operator_may_act_inside_every_project() {
        // Anything else is a cell nobody can repair.
        let admins = vec!["ops".to_string()];
        assert!(may("ops", &admins, &[], Verb::Administer, "instances", &[]).is_ok());
        assert!(
            may(
                "ops",
                &admins,
                &bindings(Role::Viewer, "ada"),
                Verb::Write,
                "instances",
                &[]
            )
            .is_ok()
        );
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
        assert!(may("ada", &[], &b, Verb::Write, "instances", &[]).is_ok());
        assert!(may("bob", &[], &b, Verb::Write, "instances", &[]).is_ok());
        assert!(may("bob", &[], &b, Verb::Administer, "instances", &[]).is_err());
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
                    role.admits(verb, "instances", &[]),
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
            assert!(!role.admits(Verb::Administer, "instances", &[]), "{role:?}");
        }
    }

    /// The rung that exists for a platform with customers.
    ///
    /// Somebody who keeps an estate running reboots machines and does not
    /// delete them. Before this rung there was no way to say that, so anybody
    /// who needed to restart a guest was given the ability to destroy one.
    #[test]
    fn an_operator_may_run_the_estate_and_not_change_what_it_consists_of() {
        assert!(Role::Operator.admits(Verb::Operate, "instances", &[]));
        assert!(!Role::Operator.admits(Verb::Write, "instances", &[]));
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

/// Whether a collection is the **cell's own** rather than any tenant's.
///
/// The distinction decides what a tenant is told when they ask for one. A
/// cell-wide collection with tenant-visible objects — `images`, whose catalogue
/// everybody may boot; `projects`, where they see theirs — is filtered, and an
/// empty answer means "none for you". A collection where no object will *ever*
/// pass a tenant's read is a different thing, and filtering it produces a lie:
/// a customer asking for `/nodes` was told the cell has **zero machines**, when
/// it has one they simply may not see.
///
/// So those are refused instead. It is what every large provider does, and the
/// reason is not tidiness: "you may not look" and "there is nothing there" lead
/// somebody to entirely different next steps, and only one of them is true.
///
/// Read by the API, which refuses, and checked against the console's own list,
/// which hides them — two answers to one question that must not drift apart.
pub fn belongs_to_the_cell(kind: &str) -> bool {
    matches!(
        kind,
        "nodes"
            | "pools"
            | "ceph-clusters"
            | "device-classes"
            | "maintenance-windows"
            | "image-sources"
            | "backup-targets"
            | "users"
            // A session to the router in front of the cell names machines on
            // both ends; nothing about it is any tenant's.
            | "bgp-peers"
    )
}

/// Which of the cell's own collections a node agent reads whole.
///
/// Two, and each for a stated reason rather than because a node is trusted in
/// general. A node agent is inside the machine room, not above it: it has no
/// business reading the cell's accounts, and `users` is on the list above.
///
/// - `nodes`, because a node has to find itself, and because the Ceph pass is
///   built on every node computing the same answer over the same facts. Nothing
///   hands a node its step; it works out whether the step is its own. A node
///   that can see only itself computes a different answer from everybody else
///   and takes a step nobody else expects — or, more likely, none at all. On a
///   single-node cell that is invisible, which is why it survived.
/// - `ceph-clusters`, for the same pass, which is meaningless without them.
pub fn a_node_reads_the_cells(kind: &str) -> bool {
    matches!(
        kind,
        "nodes" | "pools" | "backup-targets" | "ceph-clusters"
            // A gateway machine programs its routing daemon from these.
            | "bgp-peers"
    )
}

/// Whether a machine agent may read one of the cell's own objects at all.
///
/// The same four, and it is the same question — which is the point. Until this
/// existed the two halves disagreed: listing `users` was refused and
/// `users/admin` was served, to a pool agent, with 200. A curtain over a door
/// that is open is worse than either, because it reads as a rule.
///
/// Found by pointing a real pool agent at a real cell and asking it for things
/// it has no business with. What a machine needs is the machine room: the nodes
/// and pools it computes over, the backup targets it reports on, the Ceph
/// clusters those two are meaningless without. Not the cell's accounts.
pub fn a_machine_may_read(kind: &str) -> bool {
    !belongs_to_the_cell(kind) || a_node_reads_the_cells(kind)
}

#[cfg(test)]
mod what_the_cell_keeps {
    use super::*;

    #[test]
    fn a_tenants_own_collections_are_not_on_it() {
        // Filtering is right for these: an empty answer means "none of yours",
        // which is true and useful.
        for kind in [
            "projects",
            "images",
            "instances",
            "volumes",
            "audit",
            "usage",
        ] {
            assert!(!belongs_to_the_cell(kind), "{kind}");
        }
    }

    #[test]
    fn a_node_reads_what_its_own_passes_need_and_no_more() {
        // The machine room: the two the Ceph pass computes over, plus the pool
        // an agent *is* and the backup targets it reports on. That last pair
        // was added when a real pool agent, pointed at a real cell, could not
        // list the targets it exists to answer for.
        for kind in ["nodes", "pools", "backup-targets", "ceph-clusters"] {
            assert!(a_node_reads_the_cells(kind), "{kind}");
        }
        // A node agent is inside the machine room, not above it.
        for kind in ["users", "image-sources", "device-classes"] {
            assert!(!a_node_reads_the_cells(kind), "{kind}");
        }
    }

    /// The two halves answer the same question.
    ///
    /// They did not: listing `users` was refused to a pool agent's token and
    /// `users/admin` was served to it, 200, on a live cell. A curtain over an
    /// open door is worse than either, because it reads as a rule.
    #[test]
    fn what_a_machine_may_list_is_what_a_machine_may_get() {
        for kind in ["nodes", "pools", "backup-targets", "ceph-clusters"] {
            assert!(a_machine_may_read(kind), "{kind}");
        }
        for kind in [
            "users",
            "image-sources",
            "device-classes",
            "maintenance-windows",
        ] {
            assert!(!a_machine_may_read(kind), "{kind}");
        }
        // A tenant's own collections are not the cell's, and an agent reads
        // those the way it always has — narrowed to what concerns it.
        for kind in ["instances", "volumes", "ports", "images"] {
            assert!(a_machine_may_read(kind), "{kind}");
        }
    }

    #[test]
    fn the_machine_room_is() {
        for kind in ["nodes", "pools", "device-classes", "image-sources"] {
            assert!(belongs_to_the_cell(kind), "{kind}");
        }
    }
}

#[cfg(test)]
mod a_role_the_cell_wrote_down {
    use super::*;

    fn role(name: &str, grants: &[(Verb, &[&str])]) -> CustomRole {
        CustomRole {
            name: name.to_string(),
            grants: grants
                .iter()
                .map(|(verb, collections)| Grant {
                    verb: *verb,
                    collections: collections.iter().map(|c| c.to_string()).collect(),
                })
                .collect(),
        }
    }

    fn held(role: Role, who: &str) -> Vec<Binding> {
        vec![Binding {
            role,
            members: vec![who.to_string()],
        }]
    }

    #[test]
    fn the_case_a_rung_cannot_express() {
        // The four rungs answer "what may this person do" with one word for the
        // whole project. That is right for most people and wrong for the case
        // every operator eventually meets: somebody who may restart the database
        // machines and must not touch the network. A fifth rung cannot say it —
        // the difference is not *how much* but *what*.
        let defined = [role(
            "roles/db-operator",
            &[(Verb::Operate, &["instances"])],
        )];
        let bindings = held(Role::Custom("roles/db-operator".into()), "ada");

        assert!(may("ada", &[], &bindings, Verb::Operate, "instances", &defined).is_ok());
        assert!(may("ada", &[], &bindings, Verb::Operate, "networks", &defined).is_err());
        // And not a rung by another name: no creating, no deleting, anywhere.
        assert!(may("ada", &[], &bindings, Verb::Write, "instances", &defined).is_err());
    }

    #[test]
    fn being_able_to_act_on_something_means_being_able_to_see_it() {
        // Not convenience. A person who may start a guest and may not read it is
        // looking at a console that shows nothing and a button that works, which
        // is worse than either half on its own.
        let defined = [role(
            "roles/db-operator",
            &[(Verb::Operate, &["instances"])],
        )];
        let bindings = held(Role::Custom("roles/db-operator".into()), "ada");
        assert!(may("ada", &[], &bindings, Verb::Read, "instances", &defined).is_ok());
        // Only where it may act, though. Read on everything is `viewer`.
        assert!(may("ada", &[], &bindings, Verb::Read, "volumes", &defined).is_err());
    }

    #[test]
    fn administering_does_not_carry_read_with_it() {
        // The one verb kept apart from the ladder: it is about *who may*, not
        // about the objects. Letting it imply Read would make a role granted to
        // change bindings quietly a role that can also read the estate.
        let defined = [role("roles/granter", &[(Verb::Administer, &["projects"])])];
        let bindings = held(Role::Custom("roles/granter".into()), "ada");
        assert!(
            may(
                "ada",
                &[],
                &bindings,
                Verb::Administer,
                "projects",
                &defined
            )
            .is_ok()
        );
        assert!(may("ada", &[], &bindings, Verb::Read, "projects", &defined).is_err());
    }

    #[test]
    fn a_role_nobody_defined_grants_nothing() {
        // The read path, where a binding naming a role that has since been
        // deleted must resolve to the least rather than refuse the whole
        // request: the other members of that project still hold theirs. Being
        // *told* about it happens at the door, where the API refuses a binding
        // naming a role that is not there.
        let bindings = held(Role::Custom("roles/weg".into()), "ada");
        for verb in [Verb::Read, Verb::Operate, Verb::Write, Verb::Administer] {
            assert!(
                may("ada", &[], &bindings, verb, "instances", &[]).is_err(),
                "{verb:?}"
            );
        }
    }

    #[test]
    fn a_typo_lands_on_the_least_and_never_on_the_most() {
        // The lenient half of a deliberate pair. `admiin` is a viewer, not an
        // admin; `roles/typo` grants nothing at all.
        assert_eq!(Role::from("admiin"), Role::Viewer);
        assert_eq!(Role::from(""), Role::Viewer);
        assert_eq!(Role::from("admin"), Role::Admin);
        assert_eq!(
            Role::from("roles/db-operator"),
            Role::Custom("roles/db-operator".into())
        );
    }

    #[test]
    fn a_role_goes_over_the_wire_as_the_one_string_it_is() {
        // One field, not two. A binding with a `role` *and* a `customRole` would
        // be two fields with one meaning, and every one of those in this
        // platform has eventually been read in the wrong order by something.
        for role in [
            Role::Viewer,
            Role::Operator,
            Role::Editor,
            Role::Admin,
            Role::Custom("roles/db-operator".into()),
        ] {
            let text = serde_json::to_string(&role).unwrap();
            assert!(text.starts_with('"'), "{text}");
            assert_eq!(serde_json::from_str::<Role>(&text).unwrap(), role);
        }
        assert_eq!(
            serde_json::to_string(&Role::Custom("roles/x".into())).unwrap(),
            "\"roles/x\""
        );
    }

    #[test]
    fn several_roles_add_up_like_bindings_always_have() {
        // Nothing new: two bindings for one person have always been a union.
        // Said out loud because it is the answer to "how do I give somebody two
        // of these" — and because a system that resolved them in an order would
        // be one nobody can predict.
        let defined = [
            role("roles/db", &[(Verb::Operate, &["instances"])]),
            role("roles/store", &[(Verb::Write, &["volumes"])]),
        ];
        let bindings = vec![
            Binding {
                role: Role::Custom("roles/db".into()),
                members: vec!["ada".into()],
            },
            Binding {
                role: Role::Custom("roles/store".into()),
                members: vec!["ada".into()],
            },
        ];
        assert!(may("ada", &[], &bindings, Verb::Operate, "instances", &defined).is_ok());
        assert!(may("ada", &[], &bindings, Verb::Write, "volumes", &defined).is_ok());
        assert!(may("ada", &[], &bindings, Verb::Write, "instances", &defined).is_err());
    }

    #[test]
    fn a_cell_operator_is_still_a_cell_operator() {
        let defined = [role("roles/nichts", &[(Verb::Read, &["instances"])])];
        let admins = vec!["ops".to_string()];
        assert!(may("ops", &admins, &[], Verb::Administer, "nodes", &defined).is_ok());
    }
}
