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

    fn name(s: &str) -> ResourceName {
        ResourceName::parse(s).unwrap()
    }

    fn directory() -> Directory {
        Directory::new([
            ("p1".to_string(), "cell-1".to_string()),
            ("p2".to_string(), "cell-2".to_string()),
            // A project with no recorded home: written before routing existed,
            // or living in a one-cell installation.
            ("p3".to_string(), String::new()),
        ])
    }

    #[test]
    fn a_request_is_routed_by_the_project_in_its_name() {
        let d = directory();
        assert_eq!(d.cell_of(&name("projects/p1/instances/i1")), Some("cell-1"));
        assert_eq!(d.cell_of(&name("projects/p2/volumes/v1")), Some("cell-2"));
        // Deeper nesting must not change the answer — routing reads the first
        // two segments and stops, which is what makes it a parse.
        assert_eq!(
            d.cell_of(&name("projects/p1/zones/z1/instances/i1")),
            Some("cell-1")
        );
    }

    /// The three shapes of "this router has no opinion", which must all mean
    /// *answer locally* and never *refuse*.
    ///
    /// A router whose directory is a few seconds behind would otherwise reject a
    /// freshly created project's very first request, turning a propagation delay
    /// into an error the tenant sees. Answering locally is right whenever there
    /// is one cell, and when there are several the local cell refuses with the
    /// name of the right one — a correct answer one hop late beats a wrong one
    /// now.
    #[test]
    fn an_unknown_or_homeless_name_is_answered_locally() {
        let d = directory();
        // A cell's own hardware belongs to no project.
        assert_eq!(d.cell_of(&name("nodes/node-a")), None);
        assert!(d.is_local(&name("nodes/node-a"), "cell-1"));
        assert!(d.is_local(&name("nodes/node-a"), "cell-9"));

        // A project with no recorded home.
        assert_eq!(d.cell_of(&name("projects/p3/instances/i1")), None);
        assert!(d.is_local(&name("projects/p3/instances/i1"), "cell-7"));

        // A project this router has never heard of.
        assert_eq!(d.cell_of(&name("projects/brand-new/instances/i1")), None);
        assert!(d.is_local(&name("projects/brand-new/instances/i1"), "cell-1"));
    }

    #[test]
    fn a_name_is_local_only_to_the_cell_that_owns_its_project() {
        let d = directory();
        let i1 = name("projects/p1/instances/i1");
        assert!(d.is_local(&i1, "cell-1"));
        assert!(
            !d.is_local(&i1, "cell-2"),
            "cell-2 claimed cell-1's project"
        );
    }

    /// An empty home is not a cell called "". A project that records no home
    /// must not become routable to a cell whose name is the empty string.
    #[test]
    fn an_empty_home_is_absence_not_a_cell() {
        let d = Directory::new([("p1".to_string(), String::new())]);
        assert!(d.is_empty(), "an empty home was stored as a route");
        assert_eq!(d.cell_of(&name("projects/p1/instances/i1")), None);
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

/// Where a request goes, answered from the projects a cell already holds.
///
/// Routing is a **lookup on the first two segments of a name** and nothing
/// deeper: `projects/p1/instances/i1` is routed by finding `p1`. That is what
/// makes it affordable to do in front of every request — a parse and a map hit,
/// never a query — and it is why the projects collection is the one that is
/// global. A router that had to ask a cell where a project lives would first
/// have to know which cell to ask.
///
/// Built from the projects a process can already see, so it needs no separate
/// registry to keep in step with reality. Projects are few and change rarely,
/// which is what makes holding all of them in memory the right shape; the day
/// that stops being true is the day this needs a different answer, and it will
/// announce itself as memory rather than as a wrong route.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    /// Project id → the cell its resources live in. Only projects with a home
    /// recorded appear; see [`Directory::cell_of`] for what absence means.
    homes: std::collections::BTreeMap<String, String>,
}

impl Directory {
    /// Build from the projects a cell holds.
    ///
    /// Takes id and home rather than the objects, so this stays in the model
    /// crate and a router does not have to link the API to use it.
    pub fn new(projects: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            homes: projects
                .into_iter()
                .filter(|(_, cell)| !cell.is_empty())
                .collect(),
        }
    }

    /// Which cell should answer for `name`, if this directory knows.
    ///
    /// `None` has exactly one meaning: **this router has no opinion**, and the
    /// caller should answer locally. It covers a name under no project (a cell's
    /// own hardware), a project with no recorded home (every project written
    /// before routing existed, and every project in a one-cell installation),
    /// and a project this router has not heard of yet.
    ///
    /// That last one is why `None` must not mean "refuse". A router whose
    /// directory is a few seconds stale would otherwise reject a freshly created
    /// project's first request — turning a propagation delay into an error the
    /// tenant sees. Answering locally is right whenever there is one cell, and
    /// when there are several the local cell refuses with the name of the right
    /// one (see the placement check in the store), which is a correct answer
    /// arriving one hop late rather than a wrong one arriving now.
    pub fn cell_of(&self, name: &ResourceName) -> Option<&str> {
        let project = project_of(name)?;
        self.homes.get(project).map(String::as_str)
    }

    /// Whether `cell` is the one that should answer for `name`.
    ///
    /// True when the directory has no opinion, for the reason above.
    pub fn is_local(&self, name: &ResourceName, cell: &str) -> bool {
        match self.cell_of(name) {
            Some(home) => home == cell,
            None => true,
        }
    }

    pub fn len(&self) -> usize {
        self.homes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.homes.is_empty()
    }
}
