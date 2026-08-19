//! Which cell a resource lives in, and how a request finds it.
//!
//! A cell is the failure and the scaling domain: one store, one API, a few
//! thousand machines. Growing means **adding cells**, never growing one — Borg
//! bounds a cell at roughly ten thousand machines with one replicated master for
//! exactly this reason. That only works if a request naming a resource can find
//! the cell holding it without asking every cell, because "ask everybody" is a
//! design whose cost grows with the number of cells and whose availability is
//! the product of theirs.
//!
//! **A project is the unit of placement.** Everything under a project lives in
//! the project's cell, so routing is a lookup on the first two segments of a
//! name — `projects/p1` — and nothing deeper is ever consulted. That makes the
//! projects collection global, which is affordable precisely because it is
//! small: thousands of projects, changing rarely, cheap for every router to hold
//! entirely in memory. It is the same split GCP makes — projects are global, the
//! resources inside them are zonal — and it is what keeps routing a *parse*
//! rather than a *query*.
//!
//! **The limit this accepts, stated rather than discovered later:** a project
//! bounded to one cell is bounded to one cell's size. When one outgrows that,
//! the answer is a location segment in the name —
//! `projects/p1/zones/z1/instances/i1`, which [`crate::meta::ResourceName`]
//! already parses and whose `ancestor("zones")` already works — and routing by
//! zone rather than by project. Nothing here forecloses that. It is simply not
//! the problem yet, and building the general case first would mean carrying a
//! zone in every name for a system that has one cell.
//!
//! **Why any of this exists now.** `meta.placement` has been written on every
//! object since the first commit and read by nothing. A field that records where
//! an object lives and is never consulted is not routing information — it is a
//! claim, and the system behaves identically whether it is right or wrong. The
//! functions here are what turn it into an answer.

use crate::meta::{Placement, ResourceName};

/// Collections that exist once for the whole installation rather than once per
/// cell.
///
/// Only projects, and the shortness of the list is the point: every global
/// collection is one more thing that must be replicated everywhere and agreed
/// on before a request can be routed. Projects earn it by being the thing
/// routing is *keyed on* — a router that had to ask a cell where a project lives
/// would need to know which cell to ask.
///
/// Everything else is a cell's own. `nodes` and `pools` are the cell's hardware
/// and never leave it; `instances`, `volumes` and the rest belong to a project
/// and live where it does.
pub fn is_global_collection(kind: &str) -> bool {
    kind == "projects"
}

/// The project a name belongs to, if it belongs to one.
///
/// `None` for a cell-scoped root collection — `nodes/node-a`, `pools/p` — which
/// is not homeless but *this cell's*: hardware does not move between cells, so
/// the cell answering is the cell that owns it.
pub fn project_of(name: &ResourceName) -> Option<&str> {
    name.ancestor("projects")
}

/// Whether an object placed at `placement` is this cell's to answer for.
///
/// The region is deliberately not compared. A cell name is unique across the
/// installation — it has to be, since it is what a router resolves to — so
/// comparing the region as well could only ever turn a correct answer into a
/// refusal on a configuration typo, and never catch a case the cell alone
/// misses.
pub fn is_held_by(placement: &Placement, cell: &str) -> bool {
    placement.cell == cell
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_but_projects_belongs_to_a_cell() {
        assert!(is_global_collection("projects"));
        for kind in [
            "instances",
            "volumes",
            "snapshots",
            "attachments",
            "networks",
            "subnets",
            "ports",
            "security-groups",
            "images",
            "nodes",
            "pools",
            "operations",
            "migrations",
        ] {
            assert!(
                !is_global_collection(kind),
                "{kind} was made global, which means replicating it to every cell \
                 and agreeing on it before any request can be routed"
            );
        }
    }

    #[test]
    fn a_name_under_a_project_routes_by_that_project() {
        let name = ResourceName::parse("projects/p1/instances/i1").unwrap();
        assert_eq!(project_of(&name), Some("p1"));
        // Deeper nesting must not change the answer: routing reads the first two
        // segments and stops, which is what makes it a parse.
        let deep = ResourceName::parse("projects/p1/zones/z1/instances/i1").unwrap();
        assert_eq!(project_of(&deep), Some("p1"));
    }

    #[test]
    fn a_cells_own_hardware_has_no_project() {
        let name = ResourceName::parse("nodes/node-a").unwrap();
        assert_eq!(project_of(&name), None);
    }

    #[test]
    fn a_cell_holds_what_names_it_and_nothing_else() {
        let here = Placement::new("eu-central", "cell-1");
        assert!(is_held_by(&here, "cell-1"));
        assert!(!is_held_by(&here, "cell-2"));
        // Same cell, different region recorded: still this cell's. A cell name
        // is unique installation-wide, so the region can only add false
        // refusals.
        let odd = Placement::new("us-east", "cell-1");
        assert!(is_held_by(&odd, "cell-1"));
    }
}
