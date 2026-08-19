//! What the API accepts, the console can express.
//!
//! The console is schema-driven and deliberately does not link the model: it
//! speaks REST, and coupling it to the types behind the API would couple it to
//! something it never sees. That decoupling is right, and it has one cost —
//! nothing makes the schema follow the model, so a field can be added to a spec,
//! carried across the wire, enforced by the API, and remain unreachable from the
//! only interface a person uses.
//!
//! That happened on 2026-08-19 with `ProjectSpec.cell`, the field cell routing
//! turns on. It reached the model, the proto (both directions), the destructuring
//! round-trip guard and `Api::check_cell` — and an operator could not set it,
//! because `PROJECT_FIELDS` had never heard of it. Nothing was wrong; something
//! was simply missing, and no test was in a position to notice.
//!
//! This test is in the API crate because it is the one that knows both halves:
//! it serves the model and it ships the console. Neither crate could hold it
//! without gaining a dependency it has no other use for.

use velstra_cloud_console::COLLECTIONS;
use velstra_cloud_model::resources::{ImageSpec, InstanceSpec, ProjectSpec, VolumeSpec};

/// Spec fields the console deliberately does not offer, and why.
///
/// An entry here is a decision. The list being short is what makes the test
/// worth having: every line is something a person cannot do from the console.
fn exempt(collection: &str, field: &str) -> Option<&'static str> {
    match (collection, field) {
        ("projects", "bindings") => Some(
            "IAM is its own surface: a role grant is not a form field, and a \
             text box that silently replaced a project's whole policy is worse \
             than no box at all",
        ),
        ("instances", "node") => Some(
            "where a guest runs is the scheduler's answer, not an operator's \
             request; forcing it is a separate deliberate action",
        ),
        _ => None,
    }
}

/// A spec with **every field set to something non-default**, written out field
/// by field with no `..Default::default()`.
///
/// Both halves matter and the first is the one that took a second attempt. A
/// field carrying `skip_serializing_if` — `ProjectSpec.cell` and `bindings` both
/// do — simply is not in the JSON of a default value, so a test that serialised
/// `Default::default()` could not see it at all. This test passed for the
/// missing field it was written to catch.
///
/// Spelling every field out also makes adding one to the model a **compile
/// error here**, which is the same guard the wire round-trips carry: the
/// question "did anybody teach the console about this" is asked at the moment
/// the field is added, not whenever somebody next looks.
mod complete {
    use velstra_cloud_model::{
        authz::{Binding, Role},
        resources::{DesiredState, ImageFormat, PlacementPolicy, Quota},
    };

    use super::*;

    pub fn project() -> ProjectSpec {
        ProjectSpec {
            display_name: "Payments".into(),
            parent: "organizations/o1".into(),
            quota: Quota {
                instances: 1,
                vcpus: 1,
                memory_mib: 1,
                volume_gib: 1,
            },
            bindings: vec![Binding {
                role: Role::Admin,
                members: vec!["ada".into()],
            }],
            cell: "cell-1".into(),
        }
    }

    pub fn instance() -> InstanceSpec {
        InstanceSpec {
            vcpus: 1,
            memory_mib: 1,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 1,
            desired_state: DesiredState::Running,
            ports: vec!["projects/p1/ports/pt1".into()],
            ssh_keys: vec!["ssh-ed25519 AAAA".into()],
            user_data: Some("#cloud-config".into()),
            node: Some("node-a".into()),
            placement_policy: PlacementPolicy {
                anti_affinity_group: Some("web".into()),
                required_labels: vec!["gpu".into()],
            },
        }
    }

    pub fn volume() -> VolumeSpec {
        VolumeSpec {
            size_gib: 1,
            pool: "pools/p".into(),
            encryption_key: Some("projects/p1/keys/k".into()),
            source_image: Some("projects/p1/images/sha256-abc".into()),
            source_snapshot: Some("projects/p1/volumes/v/snapshots/s".into()),
        }
    }

    pub fn image() -> ImageSpec {
        ImageSpec {
            digest: "sha256-abc".into(),
            format: ImageFormat::Qcow2,
            size_bytes: 1,
            source_url: "http://images.invalid/x".into(),
            signature: Some("sig".into()),
        }
    }
}

/// Every top-level key of `spec`, as the wire spells them.
fn spec_keys<T: serde::Serialize>(spec: &T) -> Vec<String> {
    let value = serde_json::to_value(spec).expect("a spec always serialises");
    value
        .as_object()
        .expect("a spec is an object")
        .keys()
        .map(|k| velstra_cloud_wire::to_camel(k))
        .collect()
}

/// Whether the console has a **form field** for `key`, counting a nested one
/// (`quota.instances` covers `quota`).
///
/// A column does not count, and that is the point. `spec` is the half a client
/// writes; showing a value an operator cannot set is not coverage of it. The
/// first version of this test accepted either, and it passed with the field
/// that prompted the whole exercise — `projects.cell` had a column and no way
/// to set it, which is exactly the state being tested for.
fn console_covers(collection: &str, key: &str) -> bool {
    let Some(c) = COLLECTIONS.iter().find(|c| c.id == collection) else {
        return false;
    };
    c.fields
        .iter()
        .any(|f| f.key == key || f.key.starts_with(&format!("{key}.")))
}

/// The check itself, run over the collections whose spec an operator writes.
fn assert_covered<T: serde::Serialize>(collection: &str, spec: &T) {
    let mut missing = Vec::new();
    for key in spec_keys(spec) {
        if exempt(collection, &key).is_some() {
            continue;
        }
        if !console_covers(collection, &key) {
            missing.push(key);
        }
    }
    assert!(
        missing.is_empty(),
        "the API accepts {missing:?} on a {collection} spec and the console offers no way to \
         see or set them. Add a field (or a column, for something read-only), or add an \
         `exempt` entry saying why a person should not be able to."
    );
}

#[test]
fn the_console_can_express_every_project_field() {
    assert_covered("projects", &complete::project());
}

#[test]
fn the_console_can_express_every_instance_field() {
    assert_covered("instances", &complete::instance());
}

#[test]
fn the_console_can_express_every_volume_field() {
    assert_covered("volumes", &complete::volume());
}

#[test]
fn the_console_can_express_every_image_field() {
    assert_covered("images", &complete::image());
}

/// An exemption is a claim, and a claim needs a reason attached.
#[test]
fn every_exemption_says_why() {
    for (collection, field) in [("projects", "bindings"), ("instances", "node")] {
        let why = exempt(collection, field).expect("listed above but not exempt");
        assert!(
            why.len() > 40,
            "{collection}.{field} is exempt without saying why"
        );
    }
}

/// Every collection the API serves has a screen, checked against the API's own
/// list rather than one written down beside it.
///
/// The console has its own version of this test, and on 2026-08-19 it passed
/// while `pools` and `snapshots` had no screen at all — because it iterates a
/// hand-written list, and the list was two short. A list somebody maintains is
/// exactly as complete as their memory; this one is `Api::COLLECTIONS`, which is
/// what the server actually routes.
#[test]
fn every_collection_the_api_serves_has_a_screen() {
    let mut missing = Vec::new();
    for kind in velstra_cloud_api::COLLECTIONS {
        if unscreened(kind).is_some() {
            continue;
        }
        if !COLLECTIONS.iter().any(|c| c.id == kind) {
            missing.push(kind);
        }
    }
    assert!(
        missing.is_empty(),
        "the API serves {missing:?} and the console has no screen for them — an operator \
         cannot see or touch those objects at all. Add a Collection, or add an `unscreened` \
         entry saying why the console deliberately does not show it."
    );
}

/// Collections the console deliberately has no screen for, and why.
fn unscreened(kind: &str) -> Option<&'static str> {
    match kind {
        "snapshots" => Some(
            "a snapshot's name hangs under a volume \
             (projects/p1/volumes/data-1/snapshots/nightly) and the console's Scope knows only \
             Project and Global. Showing them needs a scope for a collection nested under an \
             object, which is machinery rather than a schema entry — until then a volume is \
             restored by typing the snapshot's name",
        ),
        _ => None,
    }
}

/// The unscreened list is a claim about what a person cannot do, so it says why.
#[test]
fn every_unscreened_collection_says_why() {
    let why = unscreened("snapshots").expect("listed above but not exempt");
    assert!(why.len() > 60, "snapshots is unscreened without saying why");
}
