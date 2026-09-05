//! Folders: the thing above a project.
//!
//! A project is the unit of tenancy, and that has always been the whole of the
//! story here — every binding lives on a project, and a person who administers
//! forty of them administers them forty times. `ProjectSpec.parent` has said
//! since the beginning that it names "`organizations/o1` or `folders/f2`, the
//! parent policies are inherited from, kept as a name so the hierarchy is
//! walked, not guessed", and nothing walked it. The field was a promise the
//! platform did not keep: a customer could set it, the console showed it, and it
//! changed nothing about who could do what.
//!
//! **Roles add up going down.** A binding on a folder governs every project
//! under it, at any depth. Nothing subtracts: there is no deny, and there is no
//! way for a project to shed a role granted above it. That is the same rule the
//! large providers settled on, and it is the one people can hold in their heads
//! — "where can this go wrong" is answered by reading upward, once, and the
//! answer is a union rather than a resolution order.
//!
//! **The walk is bounded twice.** A cycle is refused when it is written, and the
//! walk stops at [`MAX_DEPTH`] regardless. Once is not enough: the refusal
//! protects a cell where every write goes through this API, and the bound
//! protects one where a store was restored, edited by hand, or written by a
//! version that did not have the check.

use serde::{Deserialize, Serialize};

use crate::{Observed, Resource, authz::Binding, meta::Condition, resources::Assigned};

/// How far up a walk goes before it stops looking.
///
/// Eight, which is deeper than any organisation chart anybody has drawn on a
/// whiteboard and shallow enough that the read cost of a permission check stays
/// something you can say out loud. A folder deeper than this is refused when it
/// is created, so the bound is never the thing a person meets — it is what keeps
/// a store somebody edited by hand from turning one permission check into an
/// infinite loop.
pub const MAX_DEPTH: usize = 8;

/// `folders/` — the prefix a parent carries when it names one.
pub const FOLDER_PREFIX: &str = "folders/";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderSpec {
    /// What a person calls it. `Engineering`, `Kunde Nord`, `Staging`.
    pub display_name: String,
    /// The folder above this one, or empty at the top.
    ///
    /// Only another folder. There is no separate organisation object: a cell is
    /// the organisation, and inventing a second kind whose only difference is
    /// that it may not have a parent would be a kind whose whole content is a
    /// restriction.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    /// Who may do what in everything under this folder.
    ///
    /// The same [`Binding`] a project carries, deliberately: a role means the
    /// same thing wherever it is granted, and the only difference between
    /// granting it here and granting it on a project is how much it reaches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
}

pub type Folder = Resource<FolderSpec, FolderStatus>;

/// A role the cell wrote down, as an object.
///
/// Here beside folders rather than in `authz` because it is the same kind of
/// thing: something cell-wide that changes what a binding means. `authz` holds
/// the *decision*; this holds the object the decision reads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleSpec {
    pub display_name: String,
    pub description: String,
    /// What it lets somebody do. At least one grant, each naming at least one
    /// collection — see [`crate::authz::CustomRole`] for why there is no
    /// wildcard.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<crate::authz::Grant>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleStatus {
    pub observed_generation: u64,
    pub conditions: Vec<Condition>,
}

pub type RoleObject = Resource<RoleSpec, RoleStatus>;

impl Assigned for RoleSpec {}

impl Observed for RoleStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
    fn self_owned(&self) -> bool {
        true
    }
    fn written_by_the_platform(&self) -> bool {
        true
    }
}

/// Nobody is assigned a folder. It is a place in a tree, not a thing an agent
/// runs, and the only party that ever writes one is a person.
impl Assigned for FolderSpec {}

impl Observed for FolderStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    fn owner(&self) -> Option<&str> {
        None
    }
    fn self_owned(&self) -> bool {
        true
    }
    fn written_by_the_platform(&self) -> bool {
        true
    }
}

/// One step up from whatever `parent` says, if it names a folder.
///
/// `None` for an empty parent and for anything that is not a folder name. The
/// second case matters: `parent` is a free string on the wire, and a walk that
/// followed `projects/p1` would climb sideways into tenancy.
pub fn folder_above(parent: &str) -> Option<&str> {
    let rest = parent.strip_prefix(FOLDER_PREFIX)?;
    (!rest.is_empty() && !rest.contains('/')).then_some(parent)
}

/// The chain from `start` upwards, nearest first, stopping at [`MAX_DEPTH`].
///
/// `lookup` answers with a folder's own parent. Pure so that the walk — which is
/// the part with the cycle in it — can be tested without a store.
///
/// A name already seen ends the walk. That is not the same as refusing a cycle:
/// this is the *read* path, and its job is to answer the permission question
/// with whatever the store actually holds rather than to hang.
pub fn ancestors(start: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> Vec<String> {
    let mut chain = Vec::new();
    let mut here = folder_above(start).map(str::to_string);
    while let Some(name) = here {
        if chain.contains(&name) || chain.len() >= MAX_DEPTH {
            break;
        }
        chain.push(name.clone());
        here = lookup(&name).and_then(|p| folder_above(&p).map(str::to_string));
    }
    chain
}

/// Whether making `folder`'s parent `parent` would close a loop.
///
/// Asked before the write. A loop written down is a permission check that never
/// terminates on any cell whose walk is not bounded — and every cell's walk is
/// bounded, which is exactly why this refusal has to exist too: without it the
/// loop is not an error, it is a folder whose grandparent is quietly itself and
/// whose bindings therefore stop being read at some arbitrary depth.
pub fn would_loop(folder: &str, parent: &str, lookup: impl FnMut(&str) -> Option<String>) -> bool {
    let Some(above) = folder_above(parent) else {
        return false;
    };
    above == folder || ancestors(above, lookup).iter().any(|a| a == folder)
}

/// Every binding that governs something under `parent`, nearest first.
///
/// Nearest first is for the reader, not for the decision: roles add up, so the
/// order changes nothing about the answer. It changes what an explanation looks
/// like, and an explanation of a permission is worth as much as the permission.
pub fn inherited(
    parent: &str,
    mut bindings_of: impl FnMut(&str) -> Option<(Vec<Binding>, String)>,
) -> Vec<Binding> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut here = folder_above(parent).map(str::to_string);
    while let Some(name) = here {
        if seen.contains(&name) || seen.len() >= MAX_DEPTH {
            break;
        }
        seen.push(name.clone());
        match bindings_of(&name) {
            Some((mut bindings, above)) => {
                out.append(&mut bindings);
                here = folder_above(&above).map(str::to_string);
            }
            // A folder that is not there grants nothing and ends the walk. It
            // does not refuse: a project whose folder was deleted is a project
            // whose own bindings still govern it, and taking a tenant's access
            // away because somebody tidied up above them would be an outage
            // caused by housekeeping.
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod walking_upward {
    use super::*;
    use crate::authz::Role;

    fn tree<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, p)| p.to_string())
        }
    }

    #[test]
    fn a_parent_that_is_not_a_folder_is_not_climbed() {
        // `parent` is a free string on the wire. A walk that followed
        // `projects/p1` would climb sideways out of the hierarchy and into
        // somebody's tenancy.
        assert_eq!(folder_above(""), None);
        assert_eq!(folder_above("projects/p1"), None);
        assert_eq!(folder_above("folders/"), None);
        assert_eq!(folder_above("folders/a/b"), None);
        assert_eq!(folder_above("folders/eng"), Some("folders/eng"));
    }

    #[test]
    fn the_chain_is_nearest_first() {
        let up = tree(&[
            ("folders/team", "folders/eng"),
            ("folders/eng", "folders/all"),
            ("folders/all", ""),
        ]);
        assert_eq!(
            ancestors("folders/team", up),
            vec!["folders/team", "folders/eng", "folders/all"]
        );
    }

    #[test]
    fn a_loop_ends_the_walk_instead_of_hanging() {
        // The read path's job is to answer with whatever the store holds, not to
        // be the place a bad store is discovered.
        let up = tree(&[("folders/a", "folders/b"), ("folders/b", "folders/a")]);
        assert_eq!(ancestors("folders/a", up), vec!["folders/a", "folders/b"]);
    }

    #[test]
    fn a_chain_longer_than_anybody_should_have_stops() {
        let pairs: Vec<(String, String)> = (0..40)
            .map(|i| (format!("folders/f{i}"), format!("folders/f{}", i + 1)))
            .collect();
        let up = |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
        };
        assert_eq!(ancestors("folders/f0", up).len(), MAX_DEPTH);
    }

    #[test]
    fn a_loop_is_refused_before_it_is_written() {
        let up = tree(&[("folders/b", "folders/a"), ("folders/a", "")]);
        // b is under a; making a's parent b closes the loop.
        assert!(would_loop("folders/a", "folders/b", up));
        let up = tree(&[("folders/b", "folders/a"), ("folders/a", "")]);
        assert!(!would_loop("folders/c", "folders/b", up));
    }

    #[test]
    fn a_folder_cannot_be_its_own_parent() {
        assert!(would_loop("folders/a", "folders/a", |_| None));
    }

    #[test]
    fn roles_add_up_going_down() {
        let bind = |role: Role, member: &str| Binding {
            role,
            members: vec![member.to_string()],
        };
        let tree = |name: &str| match name {
            "folders/team" => Some((vec![bind(Role::Viewer, "ada")], "folders/eng".to_string())),
            "folders/eng" => Some((vec![bind(Role::Editor, "bob")], String::new())),
            _ => None,
        };
        let all = inherited("folders/team", tree);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|b| b.members.contains(&"ada".to_string())));
        assert!(all.iter().any(|b| b.members.contains(&"bob".to_string())));
    }

    #[test]
    fn a_folder_that_is_gone_does_not_take_a_tenant_down_with_it() {
        // Somebody tidies up above a project. Its own bindings still govern it,
        // and an outage caused by housekeeping is the wrong answer.
        let all = inherited("folders/weg", |_| None);
        assert!(all.is_empty());
    }
}
