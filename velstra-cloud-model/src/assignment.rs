//! Whether an object is any of a given agent's business.
//!
//! One definition, used on both sides of the wire. An agent asks it of an event
//! to decide whether to wake up; the API asks it of every object to decide what
//! to send that agent at all. Two copies of this rule would disagree exactly
//! once — the moment an object moved — and the symptom would be a guest running
//! on a node that has stopped being told about it.
//!
//! Two kinds of agent ask, and they are the same shape with different nouns. A
//! **node** holds instances, ports, attachments and migrations; a **pool** holds
//! volumes and snapshots. Both are told about what they hold and what they have
//! been given, and neither is told about anything else.
//!
//! ## Why `status.node` **or** `spec.node`
//!
//! The access rule is "the fact wins while it exists; the assignee may claim
//! only when nobody holds it". A node therefore has business with an object in
//! two different situations, and needs to see it in both:
//!
//! * it **holds** the object (`status.node`), whether or not a scheduler still
//!   thinks it should; and
//! * it has been **given** the object (`spec.node`) and nobody holds it yet.
//!
//! Filtering on either one alone breaks a real case. On `spec.node` alone, a
//! node stops being told about a guest it is still running the moment a
//! scheduler re-assigns it — so it never lets go, and two nodes run one guest.
//! On `status.node` alone, a node is never told about work it has been given, so
//! nothing ever starts.
//!
//! ## Why a migration is different
//!
//! A migration names two nodes and neither field is called `node`. Both halves
//! are the business of the node they name: the destination owns the object and
//! reports on it, and the source has to read it to know where to send. So both
//! are matched, and a node that appears in neither is not told about it at all.
//!
//! ## What this deliberately does not answer
//!
//! Subnets, networks and security groups are not per-node facts and are not
//! filtered by this. Nodes read those whole — they are small, and they are
//! shared by construction. A node's *rules* are a different matter: they depend
//! on every port in the project, so they are resolved centrally and delivered
//! with the port rather than being something a node could work out from what it
//! can see.

// `serde_json` in a crate whose whole point is that it does no I/O. The rule
// there is about *I/O*, not about serialisation: `Value` is a data structure,
// this function reads it and nothing else, and it stays as testable without a
// cluster as everything else here.
//
// The alternative was a trait the resources implement, which would have forced
// the API to deserialise every document just to decide whether to send it — more
// code and more work, to avoid a dependency that costs nothing.
use serde_json::Value;

/// Who is asking.
///
/// An enum rather than two optional fields, so "both at once" cannot be written:
/// an agent is one or the other, and a filter carrying both would be asking for
/// an intersection nobody wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assignee {
    /// A hypervisor.
    Node(String),
    /// A storage pool.
    Pool(String),
}

impl Assignee {
    /// The name, for a log line or a query parameter.
    pub fn id(&self) -> &str {
        match self {
            Self::Node(id) | Self::Pool(id) => id,
        }
    }
}

/// Whether `who` has any business with `object`.
///
/// The same rule for both: what it holds (`status`), or what it has been given
/// (`spec`) — plus, for a migration, either end.
pub fn concerns_assignee(object: &Value, who: &Assignee) -> bool {
    match who {
        Assignee::Node(node) => concerns(object, node),
        Assignee::Pool(pool) => concerns_pool(object, pool),
    }
}

/// Whether `pool` has any business with `object`.
///
/// The storage half, and deliberately the same two fields under different names:
/// a volume lives in the pool that holds it (`status.pool`) and is asked for in
/// the pool it names (`spec.pool`), exactly as a guest runs on the node holding
/// it and is asked for on the node it names.
pub fn concerns_pool(object: &Value, pool: &str) -> bool {
    let holder = object
        .get("status")
        .and_then(|s| s.get("pool"))
        .and_then(Value::as_str);
    let assignee = object
        .get("spec")
        .and_then(|s| s.get("pool"))
        .and_then(Value::as_str);
    holder == Some(pool) || assignee == Some(pool)
}

/// Whether `who` needs to be told about a collection at all.
pub fn is_assigned_to(kind: &str, who: &Assignee) -> bool {
    match who {
        Assignee::Node(_) => is_assigned_collection(kind),
        Assignee::Pool(_) => is_pooled_collection(kind),
    }
}

/// The collections a pool agent is told about, and the ones that grow with the
/// cell for it.
pub fn is_pooled_collection(kind: &str) -> bool {
    matches!(kind, "volumes" | "snapshots")
}

/// A collection nobody is assigned any of, which every agent therefore reads
/// whole.
///
/// The distinction that matters is **not** "assigned to me or not". Three
/// answers are possible and conflating two of them is a hole:
///
/// * *shared* — a subnet, a network, a security group. Nobody owns one, and an
///   agent that could not read them would have no gateway to hand its guests.
/// * *somebody else's kind* — a volume, to a node. Not shared and not this
///   agent's, so the honest answer is **nothing**, not everything.
/// * *this agent's kind* — filtered by [`concerns_assignee`].
///
/// Reading the second as the first is how a node asking for volumes was handed
/// every volume in the cell: harmless today only because no node reads them, and
/// exactly the shape that puts the whole cell back on the wire the first time
/// one does.
pub fn is_shared_collection(kind: &str) -> bool {
    !is_assigned_collection(kind) && !is_pooled_collection(kind)
}

/// Whether `node` has any business with `object`.
///
/// Works on the document rather than on a typed resource on purpose: the API
/// applies it to whatever a collection hands back, without knowing which kind it
/// is holding, and a filter that had to be taught about every type would be one
/// more place to forget a new one.
///
/// An object that carries none of these fields concerns **nobody**, which is the
/// safe direction: a filter that let unknown shapes through would quietly put
/// the whole cell back on the wire.
pub fn concerns(object: &Value, node: &str) -> bool {
    let holder = object
        .get("status")
        .and_then(|s| s.get("node"))
        .and_then(Value::as_str);
    let spec = object.get("spec");
    let assignee = spec.and_then(|s| s.get("node")).and_then(Value::as_str);
    // Both spellings, because the same field is `from_node` in the model and
    // `fromNode` on the wire, and this is applied to documents from both sides.
    let moving = ["to_node", "from_node", "toNode", "fromNode"]
        .iter()
        .any(|field| spec.and_then(|s| s.get(field)).and_then(Value::as_str) == Some(node));

    holder == Some(node) || assignee == Some(node) || moving
}

/// Whether a node needs to be told about a collection at all.
///
/// The four that are assigned are the four that grow with the cell; the rest are
/// small and shared. Naming them here rather than at each call site means a new
/// per-node collection is one edit, and a new *shared* one needs no edit at all.
pub fn is_assigned_collection(kind: &str) -> bool {
    matches!(kind, "instances" | "ports" | "attachments" | "migrations")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_node_is_told_about_what_it_holds() {
        let object = json!({"spec": {}, "status": {"node": "node-a"}});
        assert!(concerns(&object, "node-a"));
        assert!(!concerns(&object, "node-b"));
    }

    #[test]
    fn a_node_is_told_about_what_it_has_been_given() {
        let object = json!({"spec": {"node": "node-a"}, "status": {}});
        assert!(concerns(&object, "node-a"));
    }

    #[test]
    fn a_node_still_running_a_guest_it_was_reassigned_from_is_told_about_it() {
        // The case that makes "or" necessary rather than tidy. Filtering on the
        // assignment alone would stop telling node-a about a guest it is still
        // running, so it would never let go — and both nodes would run it.
        let object = json!({"spec": {"node": "node-b"}, "status": {"node": "node-a"}});
        assert!(concerns(&object, "node-a"), "the holder was not told");
        assert!(concerns(&object, "node-b"), "the assignee was not told");
    }

    #[test]
    fn both_ends_of_a_migration_are_told_about_it() {
        let object = json!({"spec": {"from_node": "node-a", "to_node": "node-b"}, "status": {}});
        assert!(concerns(&object, "node-a"));
        assert!(concerns(&object, "node-b"));
        assert!(!concerns(&object, "node-c"));
    }

    #[test]
    fn a_migration_in_wire_shape_is_read_the_same_way() {
        // The API hands out camelCase. A filter that only knew the model's
        // spelling would send every node every migration in the cell.
        let object = json!({"spec": {"fromNode": "node-a", "toNode": "node-b"}, "status": {}});
        assert!(concerns(&object, "node-a"));
        assert!(concerns(&object, "node-b"));
        assert!(!concerns(&object, "node-c"));
    }

    #[test]
    fn an_object_naming_nobody_concerns_nobody() {
        let object = json!({"spec": {}, "status": {}});
        assert!(!concerns(&object, "node-a"));
    }

    #[test]
    fn a_shape_with_no_node_in_it_is_not_let_through() {
        // A subnet, say. Letting unknown shapes past would put the whole cell
        // back on the wire while looking like a working filter.
        let object = json!({"spec": {"cidr": "10.0.0.0/8"}, "status": {}});
        assert!(!concerns(&object, "node-a"));
    }

    #[test]
    fn a_pool_is_told_about_what_it_holds_and_what_it_has_been_given() {
        let asked = json!({"spec": {"pool": "pool-a"}, "status": {}});
        assert!(concerns_pool(&asked, "pool-a"));
        assert!(!concerns_pool(&asked, "pool-b"));

        let held = json!({"spec": {"pool": "pool-b"}, "status": {"pool": "pool-a"}});
        // Both, for the same reason a node is: a pool that stopped being told
        // about a volume it is still holding could never let go of it, and a
        // pool never told about one it was given would never make it.
        assert!(concerns_pool(&held, "pool-a"), "the holder was not told");
        assert!(concerns_pool(&held, "pool-b"), "the assignee was not told");
    }

    #[test]
    fn a_node_and_a_pool_are_told_about_different_things() {
        // The two halves must not leak into each other: a node has no business
        // with a volume's bytes, and a pool has none with a guest.
        let volume = json!({"spec": {"pool": "pool-a", "sizeGib": 1}, "status": {}});
        assert!(!concerns(&volume, "pool-a"), "a node matched a volume");
        assert!(concerns_assignee(&volume, &Assignee::Pool("pool-a".into())));
        assert!(!concerns_assignee(
            &volume,
            &Assignee::Node("pool-a".into())
        ));

        let instance = json!({"spec": {"node": "node-a"}, "status": {}});
        assert!(
            !concerns_pool(&instance, "node-a"),
            "a pool matched a guest"
        );
    }

    #[test]
    fn each_agent_is_told_about_its_own_collections_and_no_others() {
        let node = Assignee::Node("node-a".into());
        let pool = Assignee::Pool("pool-a".into());
        for kind in ["instances", "ports", "attachments", "migrations"] {
            assert!(is_assigned_to(kind, &node), "{kind}");
            assert!(!is_assigned_to(kind, &pool), "{kind} reached a pool");
        }
        for kind in ["volumes", "snapshots"] {
            assert!(is_assigned_to(kind, &pool), "{kind}");
            assert!(!is_assigned_to(kind, &node), "{kind} reached a node");
        }
    }

    #[test]
    fn somebody_elses_collection_is_nothing_rather_than_everything() {
        // The hole this closes: "not assigned to me" read as "shared" handed a
        // node every volume in the cell. Harmless only until a node reads them.
        let node = Assignee::Node("node-a".into());
        let pool = Assignee::Pool("pool-a".into());
        assert!(!is_shared_collection("volumes"));
        assert!(!is_shared_collection("instances"));
        assert!(
            !is_assigned_to("volumes", &node),
            "a node was offered volumes"
        );
        assert!(
            !is_assigned_to("instances", &pool),
            "a pool was offered guests"
        );

        for kind in [
            "subnets",
            "networks",
            "security-groups",
            // A load balancer is a cell-wide fact like the network it fronts:
            // balancing happens wherever traffic arrives, so no node owns one.
            "load-balancers",
            "nodes",
            "images",
            "projects",
        ] {
            assert!(
                is_shared_collection(kind),
                "{kind} is not shared but should be"
            );
        }
    }

    #[test]
    fn the_collections_that_grow_with_the_cell_are_the_filtered_ones() {
        for kind in ["instances", "ports", "attachments", "migrations"] {
            assert!(is_assigned_collection(kind), "{kind}");
        }
        for kind in ["subnets", "networks", "security-groups", "nodes", "images"] {
            assert!(!is_assigned_collection(kind), "{kind}");
        }
    }
}
