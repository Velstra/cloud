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

use velstra_cloud_console::{COLLECTIONS, Kind};
use velstra_cloud_model::{
    ceph::{BlockDevice, CephClusterSpec, DeviceUse, MIN_OSD_GIB, may_consume},
    loadbalancer::LoadBalancerSpec,
    resources::{ImageSpec, InstanceSpec, ProjectSpec, VolumeSpec},
};

/// Spec fields the console deliberately does not offer, and why.
///
/// An entry here is a decision. The list being short is what makes the test
/// worth having: every line is something a person cannot do from the console.
fn exempt(collection: &str, field: &str) -> Option<&'static str> {
    match (collection, field) {
        ("projects", "bindings") => Some(
            "IAM is its own surface and now has one: `iam.js` renders a project's \
             grants as rows — one person, one role, added and removed on their \
             own, saved with the revision the sheet was drawn from. It is \
             deliberately not a schema field, because a text box holding the \
             whole set replaces all of it on every save and loses whatever a \
             colleague did in between",
        ),
        ("images", "signature") => Some(
            "nothing in this platform verifies a signature, so the API refuses \
             one — and a box that records a security claim nothing checks is \
             where the claim comes from. It comes back with verification",
        ),
        ("instances", "node") => Some(
            "where a guest runs is the scheduler's answer, not an operator's \
             request; forcing it is a separate deliberate action",
        ),
        ("backup-targets", "kind") => Some(
            "there is exactly one kind of target, and a control with one \
             option is a question with one answer. It comes back the day a \
             second kind does",
        ),
        ("backups", "pool") => Some(
            "derived: the API reads it off the volume being copied, so a box \
             for it would be a second answer to a question already settled — \
             and a wrong one would assign the copy to a pool that cannot see \
             the source",
        ),
        ("backups", "schedule") => Some(
            "written by the schedule that created the copy, and the reason it \
             exists is that retention must never expire a backup somebody took \
             by hand. A person setting it could hand their own copy to a \
             schedule to delete",
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
            policy: Default::default(),
            display_name: "Payments".into(),
            parent: "organizations/o1".into(),
            quota: Quota {
                devices: 0,
                instances: 1,
                vcpus: 1,
                memory_mib: 1,
                volume_gib: 1,
                ..Quota::default()
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
            start_order: 0,
            start_delay_s: 0,
            on_node_loss: Default::default(),
            console: false,
            devices: Vec::new(),
            vcpus: 1,
            memory_mib: 1,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 1,
            desired_state: DesiredState::Running,
            ports: vec!["projects/p1/ports/pt1".into()],
            networks: Vec::new(),
            ssh_keys: vec!["ssh-ed25519 AAAA".into()],
            user_data: Some("#cloud-config".into()),
            node: Some("node-a".into()),
            placement_policy: PlacementPolicy {
                anti_affinity_group: Some("web".into()),
                required_labels: vec!["gpu".into()],
                min_cpu_level: None,
                affinity_group: Some("cache".into()),
                spread: velstra_cloud_model::resources::Strength::Preferred,
                affinity: velstra_cloud_model::resources::Strength::Preferred,
            },
        }
    }

    pub fn volume() -> VolumeSpec {
        VolumeSpec {
            source_backup: None,
            size_gib: 1,
            pool: "pools/p".into(),
            encryption_key: Some("projects/p1/keys/k".into()),
            source_image: Some("projects/p1/images/sha256-abc".into()),
            source_snapshot: Some("projects/p1/volumes/v/snapshots/s".into()),
        }
    }

    pub fn ceph_cluster() -> CephClusterSpec {
        CephClusterSpec {
            public_network: "10.0.0.0/24".into(),
            cluster_network: "10.1.0.0/24".into(),
            monitors: vec!["node-a".into()],
            osds: vec![velstra_cloud_model::ceph::OsdSpec {
                node: "node-a".into(),
                device: "/dev/disk/by-id/nvme-x".into(),
            }],
            pools: vec![velstra_cloud_model::ceph::CephPoolSpec {
                pool: "volumes".into(),
                size: 3,
                min_size: 2,
            }],
            paused: true,
        }
    }

    pub fn image() -> ImageSpec {
        ImageSpec {
            family: "debian-13".into(),
            version: "20260815".into(),
            source_instance: None,
            digest: "sha256-abc".into(),
            format: ImageFormat::Qcow2,
            size_bytes: 1,
            source_url: "http://images.invalid/x".into(),
            signature: Some("sig".into()),
        }
    }

    pub fn backup_target() -> velstra_cloud_model::backup::BackupTargetSpec {
        velstra_cloud_model::backup::BackupTargetSpec {
            kind: velstra_cloud_model::backup::TargetKind::Directory,
            path: "/srv/archive".into(),
            accepting: true,
            agent: "nvme".into(),

            verify_every_hours: 0,
        }
    }

    pub fn maintenance_window() -> velstra_cloud_model::maintenance::MaintenanceWindowSpec {
        velstra_cloud_model::maintenance::MaintenanceWindowSpec {
            node: "node-a".into(),
            starts_at: velstra_cloud_model::meta::Timestamp(1),
            minutes: 60,
            drain: true,
            note: "swapping the failed DIMM in slot 3".into(),
        }
    }

    pub fn backup() -> velstra_cloud_model::backup::BackupSpec {
        velstra_cloud_model::backup::BackupSpec {
            volume: "projects/p1/volumes/v1".into(),
            target: "backup-targets/archive".into(),
            pool: "nvme".into(),
            schedule: Some("projects/p1/backup-schedules/nightly".into()),
        }
    }

    pub fn load_balancer() -> LoadBalancerSpec {
        LoadBalancerSpec {
            network: "projects/p1/networks/n1".into(),
            subnet: "projects/p1/subnets/s1".into(),
            vip: Some("10.20.0.100".into()),
            listeners: vec![velstra_cloud_model::loadbalancer::Listener {
                protocol: velstra_cloud_model::loadbalancer::Protocol::Tcp,
                port: 443,
                member_port: 8080,
            }],
            members: vec!["projects/p1/ports/pt1".into()],
        }
    }
}

/// Every top-level key of `spec`, as the wire spells them.
fn spec_keys<T: serde::Serialize>(spec: &T) -> Vec<String> {
    let value = serde_json::to_value(spec).expect("a spec always serialises");
    value
        .as_object()
        .expect("a spec is an object")
        .iter()
        // One level in, so that a nested object is not a place fields can be
        // added without this noticing. `placementPolicy` was exactly that: it
        // counted as covered because *one* of its fields had a control, and
        // three more were added underneath it without a word.
        .flat_map(|(key, value)| match value.as_object() {
            Some(inner) if !inner.is_empty() => inner
                .keys()
                .map(|sub| {
                    format!(
                        "{}.{}",
                        velstra_cloud_wire::to_camel(key),
                        velstra_cloud_wire::to_camel(sub)
                    )
                })
                .collect::<Vec<_>>(),
            _ => vec![velstra_cloud_wire::to_camel(key)],
        })
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

/// The three that were added this year and never checked. `backup-targets` in
/// particular grew a field — which pool agent reports on it — that an operator
/// has to set and had no way to.
#[test]
fn the_console_can_express_every_backup_target_field() {
    assert_covered("backup-targets", &complete::backup_target());
}

#[test]
fn the_console_can_express_every_backup_field() {
    assert_covered("backups", &complete::backup());
}

#[test]
fn the_console_can_express_every_maintenance_window_field() {
    assert_covered("maintenance-windows", &complete::maintenance_window());
}

#[test]
fn the_console_can_express_every_load_balancer_field() {
    assert_covered("load-balancers", &complete::load_balancer());
}

#[test]
fn the_console_can_express_every_ceph_cluster_field() {
    // The one that made this test worth extending. `CEPH_FIELDS` had four
    // entries — two networks, the monitors and the pause switch — so an operator
    // could describe a Ceph cluster in every respect except the two the feature
    // exists for: which disks it is made of and which pools it holds.
    assert_covered("ceph-clusters", &complete::ceph_cluster());
}

// ---- the disks, and the one wording that is written down twice --------------
//
// The console deliberately does not link the model, so the sentences it shows
// beside a disk it will not take are a *copy* of the ones `may_consume`
// produces. A copy is a thing that drifts, and this is the pair of tests that
// does not let it: every refusal the console can print is expanded here and
// compared, character for character, against what the model would have said
// about the same disk.
//
// It is worth the machinery because of what the drift would look like. The
// console would keep offering a considered, specific reason — "it holds an ext4
// filesystem" — for a disk the API now refuses for some other reason entirely,
// and an operator would act on a sentence nothing in the platform believes any
// more. On this screen, acting on it erases a disk.

/// The disk picker's payload, or a failure saying the field is gone.
fn disk_picker() -> (
    &'static [velstra_cloud_console::Refusal],
    u64,
    &'static str,
    &'static str,
) {
    let ceph = COLLECTIONS
        .iter()
        .find(|c| c.id == "ceph-clusters")
        .expect("the console has a Ceph screen");
    for f in ceph.fields {
        if let Kind::DiskList {
            refusals,
            min_gib,
            too_small,
            unknown,
            ..
        } = f.kind
        {
            return (refusals, min_gib, too_small, unknown);
        }
    }
    panic!("the Ceph screen has no disk picker, so nobody can choose an OSD from the console");
}

/// One `BlockDevice` per state the model can report.
///
/// The `match` below reads nothing and exists for what it refuses to compile: a
/// state added to `DeviceUse` without a line in the list above is an error
/// *here*, at the moment the state is added, rather than a device the console
/// silently has no sentence for.
fn every_device_state() -> Vec<DeviceUse> {
    let all = vec![
        DeviceUse::Free,
        DeviceUse::Partitioned { partitions: 3 },
        DeviceUse::Filesystem {
            fstype: "ext4".into(),
        },
        DeviceUse::Mounted { at: "/srv".into() },
        DeviceUse::System,
        DeviceUse::Osd { id: "7".into() },
        DeviceUse::Volume { of: "md0".into() },
        DeviceUse::Unsuitable {
            why: "it is removable, and removable is not a disk to build storage on.".into(),
        },
    ];
    for state in &all {
        match state {
            DeviceUse::Free
            | DeviceUse::Partitioned { .. }
            | DeviceUse::Filesystem { .. }
            | DeviceUse::Mounted { .. }
            | DeviceUse::System
            | DeviceUse::Osd { .. }
            | DeviceUse::Volume { .. }
            | DeviceUse::Unsuitable { .. } => {}
        }
    }
    all
}

/// `{fstype}` from the same JSON the console reads it out of.
fn expand(template: &str, values: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = open
            + rest[open..]
                .find('}')
                .expect("every placeholder in the schema is closed");
        let key = &rest[open + 1..close];
        let value = values
            .get(key)
            .unwrap_or_else(|| panic!("the schema asks for {{{key}}}, which no disk carries"));
        match value {
            serde_json::Value::String(s) => out.push_str(s),
            other => out.push_str(&other.to_string()),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn device(state: DeviceUse, size_gib: u64) -> BlockDevice {
    BlockDevice {
        path: "/dev/disk/by-id/nvme-eui.0001".into(),
        kernel_name: "nvme0n1".into(),
        size_gib,
        rotational: false,
        model: "Samsung SSD 990".into(),
        serial: "S1A2B3".into(),
        state,
    }
}

/// The values the console substitutes into a refusal: the device's state, as the
/// wire spells it, plus the two numbers the size refusal needs.
fn placeholders(d: &BlockDevice, min_gib: u64) -> serde_json::Value {
    let mut v = velstra_cloud_wire::to_wire(
        serde_json::to_value(&d.state).expect("a device state always serialises"),
    );
    let map = v.as_object_mut().expect("a state is an object");
    map.insert("sizeGib".into(), d.size_gib.into());
    map.insert("minGib".into(), min_gib.into());
    v
}

#[test]
fn a_disk_that_is_not_free_is_refused_in_the_models_exact_words() {
    let (refusals, min_gib, _, _) = disk_picker();
    for state in every_device_state() {
        let d = device(state, 931);
        let values = placeholders(&d, min_gib);
        let kind = values["kind"].as_str().expect("a state carries its tag");
        let template = refusals.iter().find(|r| r.kind == kind);
        match may_consume(&d) {
            Ok(()) => assert!(
                template.is_none(),
                "the console refuses a {kind} disk that the model would accept, so an operator \
                 is told they cannot have a disk they can have"
            ),
            Err(model_says) => {
                let t = template.unwrap_or_else(|| {
                    panic!(
                        "the model refuses a {kind} disk and the console has no sentence for it, \
                         so the row would say `{model_says}` nowhere and the disk would simply be \
                         missing from the list"
                    )
                });
                assert_eq!(
                    expand(t.text, &values),
                    model_says,
                    "the console and the model disagree about a {kind} disk"
                );
            }
        }
    }
}

#[test]
fn a_disk_too_small_to_be_an_osd_is_refused_in_the_models_exact_words() {
    let (_, min_gib, too_small, _) = disk_picker();
    assert_eq!(
        min_gib, MIN_OSD_GIB,
        "the console's floor for an OSD is not the model's, so it offers disks the API refuses"
    );
    let d = device(DeviceUse::Free, MIN_OSD_GIB - 1);
    let model_says = may_consume(&d).expect_err("a disk under the floor is refused");
    assert_eq!(expand(too_small, &placeholders(&d, min_gib)), model_says);
}

/// A state this console has never heard of gets a sentence too, and it refuses.
///
/// There is no model wording to pin this one against — it is the case where the
/// model has moved and this page has not. What matters is that it exists and
/// that it names the state, because the alternative is a newer agent reporting
/// something protective and the console reading the silence as "free".
#[test]
fn a_state_the_console_has_never_heard_of_still_says_something() {
    let (_, _, _, unknown) = disk_picker();
    assert!(
        unknown.contains("{kind}"),
        "the fallback refusal does not say which state it could not read"
    );
    assert!(
        unknown.len() > 40,
        "the fallback refusal is too short to be an answer"
    );
}

/// A blank pool row starts where `CephPoolSpec` starts.
///
/// Three copies with a floor of two is the model's answer for a pool named and
/// nothing else, and a form that proposed 2/1 while the API meant 3/2 would be
/// quietly offering a weaker pool than the one it is copying — visible to nobody
/// until a node reboots and writes stop.
#[test]
fn a_blank_pool_starts_where_the_model_starts() {
    let ceph = COLLECTIONS
        .iter()
        .find(|c| c.id == "ceph-clusters")
        .expect("the console has a Ceph screen");
    let kind = ceph
        .fields
        .iter()
        .find_map(|f| match f.kind {
            Kind::PoolList {
                default_size,
                default_min_size,
            } => Some((default_size, default_min_size)),
            _ => None,
        })
        .expect("the Ceph screen has no pool control");
    let bare: velstra_cloud_model::ceph::CephPoolSpec =
        serde_json::from_value(serde_json::json!({ "pool": "volumes" }))
            .expect("a pool needs only a name");
    assert_eq!(
        kind,
        (bare.size, bare.min_size),
        "the console's blank pool is not the pool the API would make from a bare name"
    );
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
        "console-sessions" => Some(
            "a console session is minted and spent within a minute of somebody clicking \
             Console, and there is nothing on it a person would go looking for: the ticket is \
             stored hashed, and the guest, the node and the time are all on the guest's own \
             screen. It is a record, not a screen — an operator asking who opened a console \
             lists them through the API or reads the audit trail",
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

// ---- the other direction ---------------------------------------------------
//
// Everything above asks whether the model reached the console. This asks
// whether the console points at anything the model does not have — the same
// drift running the other way, and the more embarrassing of the two, because
// what it produces is a column of blanks that looks like data nobody has yet.
//
// It has already happened. `ImageStatus.cachedOn` was removed for being a field
// no writer could ever have written, and the column stayed: an operator reading
// the image list saw "Cached on" and a number that could only ever be zero, and
// every test was green because coverage was only ever checked in the direction
// that adds things.

/// Every path a complete resource of this kind actually has, in the wire's
/// spelling, including `meta.*` and every nested key.
fn paths_of(
    spec: serde_json::Value,
    status: serde_json::Value,
) -> std::collections::BTreeSet<String> {
    use velstra_cloud_model::meta::{Meta, Placement, ResourceName};

    let meta = serde_json::to_value(Meta::new(
        ResourceName::parse("projects/p1/instances/i1").expect("a name"),
        Placement::new("eu", "cell-1"),
    ))
    .expect("meta serialises");

    let document = velstra_cloud_wire::to_wire(serde_json::json!({
        "meta": meta,
        "spec": spec,
        "status": status,
    }));

    let mut out = std::collections::BTreeSet::new();
    walk(&document, String::new(), &mut out);
    out
}

fn walk(value: &serde_json::Value, at: String, out: &mut std::collections::BTreeSet<String>) {
    if !at.is_empty() {
        out.insert(at.clone());
    }
    let step = |key: &str| {
        if at.is_empty() {
            key.to_string()
        } else {
            format!("{at}.{key}")
        }
    };
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                walk(child, step(key), out);
            }
        }
        // The console indexes into lists — `status.addresses.0` is how an
        // instance's first address is shown — so a list's positions are paths
        // too. Only the ones that exist: a column naming `.3` of a list the
        // fixture gives one entry is not something this can call wrong, which is
        // why the complete fixtures above give every list at least one member.
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk(child, step(&i.to_string()), out);
            }
        }
        _ => {}
    }
}

/// Paths the API adds to a document on the way out, which no typed status has.
///
/// **This list is the point of the exercise, not an escape from it.** A handful
/// of status fields are computed when a document is read rather than stored —
/// membership over ports, occupancy over a subnet, which nodes hold an image —
/// because they are aggregates that no single writer owns and that change far
/// more often than anybody looks at them. They are real, and a console column
/// for one is correct, but they are absent from the Rust type, so a check built
/// from the type alone would call every one of them a column pointing nowhere.
///
/// Registering them here rather than silently ignoring unknown paths does three
/// things: it says which fields are computed and why, in one place; it makes
/// adding another one a decision made here; and — because the test asserts every
/// entry is still *needed* — it fails if one of them ever becomes a stored field
/// and the note goes stale.
fn computed_on_read(collection: &str, path: &str) -> Option<&'static str> {
    match (collection, path) {
        ("images", "status.cachedOn") => Some(
            "which nodes hold a verified copy is an aggregate over every node's \
             own report; an image cannot write it and no controller owns it",
        ),
        ("images", "status.fetchingOn") => Some(
            "which nodes are downloading it right now is the same aggregate, taken \
             over what each node reports as arriving. It is on the nodes' disks — a \
             partial copy in the incoming directory is what a fetch in progress *is* \
             — so nothing has to be recorded and nothing can go stale",
        ),
        ("instances", "status.pendingChanges.0.field") => Some(
            "what a running guest will only get at its next start is a comparison \
             between spec and status.runningSize; storing it would be a third copy \
             that can disagree with both. Absent when nothing is pending, so a \
             blank here reads correctly as 'running on what was asked for'",
        ),
        _ => None,
    }
}

/// Column paths the console names that the model does not have.
///
/// `status.*` is checked against a **default** status rather than a complete
/// one, and that is deliberate in one direction only: a status field carrying
/// `skip_serializing_if` is absent from a default value, so this would report it
/// as missing. Every such field is therefore listed in the complete status
/// below — which makes adding one a decision here, exactly like the spec side.
fn assert_no_column_points_nowhere(
    collection: &str,
    spec: serde_json::Value,
    status: serde_json::Value,
) {
    let Some(c) = COLLECTIONS.iter().find(|c| c.id == collection) else {
        panic!("there is no {collection} collection in the console");
    };
    let have = paths_of(spec, status);
    let mut nowhere = Vec::new();
    let mut stale = Vec::new();
    for column in c.columns {
        let computed = computed_on_read(collection, column.path);
        match (have.contains(column.path), computed.is_some()) {
            // Ordinary: a column over a field the model has.
            (true, false) => {}
            // Registered above, and genuinely not on the type.
            (false, true) => {}
            // A column over nothing at all.
            (false, false) => nowhere.push(column.path),
            // Registered as computed and now present on the type. The note is
            // no longer true, and a list of reasons that have stopped being
            // reasons is how the next person is misled.
            (true, true) => stale.push(column.path),
        }
    }
    assert!(
        nowhere.is_empty(),
        "the {collection} list has columns for {nowhere:?}, which nothing in the model produces \
         and nothing computes on read. A column pointing at a path that does not exist is not \
         empty data — it is a heading over a column of blanks, which reads as \"none yet\".\n\
         the model has: {have:?}"
    );
    assert!(
        stale.is_empty(),
        "{stale:?} is registered as computed-on-read for {collection} and is now a stored field. \
         Take the entry out: a reason that has stopped being true is worse than none."
    );
}

/// Every status field spelled out, for the same reason the specs are: one
/// carrying `skip_serializing_if` is absent from a default value, so a default
/// status would report a perfectly good column as pointing nowhere. Writing
/// them out makes adding a status field a decision here.
mod settled {
    use velstra_cloud_model::{
        ceph::{CephClusterStatus, CephPhase, OsdSpec},
        meta::Timestamp,
        resources::{
            ImageStatus, InstanceState, InstanceStatus, ProjectStatus, Quota, VolumeStatus,
        },
    };

    pub fn project() -> ProjectStatus {
        ProjectStatus {
            observed_generation: 1,
            conditions: vec![],
            used: Quota {
                devices: 0,
                instances: 1,
                vcpus: 2,
                memory_mib: 3,
                volume_gib: 4,
                ..Quota::default()
            },
        }
    }

    pub fn instance() -> InstanceStatus {
        InstanceStatus {
            running_size: None,
            stop_requested_at: None,
            console_tail: String::new(),
            console_bytes: 0,
            devices: Vec::new(),
            cpu: None,
            observed_generation: 1,
            conditions: vec![],
            state: InstanceState::Running,
            node: Some("hv-1".into()),
            addresses: vec!["10.0.0.5".into()],
            vmm_pid: Some(4242),
            started_at: Some(Timestamp(1)),
        }
    }

    pub fn volume() -> VolumeStatus {
        VolumeStatus {
            observed_generation: 1,
            conditions: vec![],
            provisioned: true,
            actual_size_gib: 10,
            pool: Some("pool-a".into()),
        }
    }

    pub fn image() -> ImageStatus {
        ImageStatus {
            observed_generation: 1,
            conditions: vec![],
        }
    }

    pub fn load_balancer() -> velstra_cloud_model::loadbalancer::LoadBalancerStatus {
        velstra_cloud_model::loadbalancer::LoadBalancerStatus {
            observed_generation: 1,
            conditions: vec![],
            vip: "10.20.0.100".into(),
            listeners: vec![velstra_cloud_model::loadbalancer::ObservedListener {
                protocol: velstra_cloud_model::loadbalancer::Protocol::Tcp,
                port: 443,
                members: 1,
            }],
        }
    }

    pub fn ceph_cluster() -> CephClusterStatus {
        CephClusterStatus {
            ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
            observed_generation: 1,
            conditions: vec![],
            phase: CephPhase::Ready,
            monitors_up: vec!["hv-1".into()],
            managers_up: vec!["hv-1".into()],
            osds_up: vec![OsdSpec {
                node: "hv-1".into(),
                device: "/dev/sdb".into(),
            }],
            pools_present: vec!["velstra-volumes".into()],
        }
    }
}

macro_rules! nothing_points_nowhere {
    ($name:ident, $collection:literal, $spec:expr, $status:expr) => {
        #[test]
        fn $name() {
            assert_no_column_points_nowhere(
                $collection,
                serde_json::to_value($spec).expect("a spec serialises"),
                serde_json::to_value($status).expect("a status serialises"),
            );
        }
    };
}

nothing_points_nowhere!(
    no_image_column_points_at_something_the_model_does_not_have,
    "images",
    complete::image(),
    settled::image()
);
nothing_points_nowhere!(
    no_project_column_points_at_something_the_model_does_not_have,
    "projects",
    complete::project(),
    settled::project()
);
nothing_points_nowhere!(
    no_instance_column_points_at_something_the_model_does_not_have,
    "instances",
    complete::instance(),
    settled::instance()
);
nothing_points_nowhere!(
    no_volume_column_points_at_something_the_model_does_not_have,
    "volumes",
    complete::volume(),
    settled::volume()
);
nothing_points_nowhere!(
    no_ceph_column_points_at_something_the_model_does_not_have,
    "ceph-clusters",
    complete::ceph_cluster(),
    settled::ceph_cluster()
);
nothing_points_nowhere!(
    no_load_balancer_column_points_at_something_the_model_does_not_have,
    "load-balancers",
    complete::load_balancer(),
    settled::load_balancer()
);

/// A field the API will not let anybody change must say so in the schema.
///
/// The two live apart on purpose — the refusal is a rule about the API, the
/// flag is what a form does about it — and apart is exactly how they drift. A
/// field refused by the API and not marked here is an edit control whose only
/// possible outcome is a refusal; a field marked here and not refused is a
/// control withheld for no reason.
///
/// Held as a list rather than derived, because the refusals are hand-written
/// prose in `core.rs` and there is nothing to read them off. What the list buys
/// is that adding a refusal without marking the field fails here rather than in
/// somebody's face.
#[test]
fn a_field_the_api_refuses_to_change_is_not_offered_for_editing() {
    // (collection, field) pairs `core.rs` refuses to change after creation.
    const FIXED: &[(&str, &str)] = &[
        ("volumes", "pool"),
        ("volumes", "sourceImage"),
        ("volumes", "sourceSnapshot"),
    ];

    for (collection, key) in FIXED {
        let coll = velstra_cloud_console::COLLECTIONS
            .iter()
            .find(|c| c.id == *collection)
            .unwrap_or_else(|| panic!("there is no {collection} collection"));
        let field = coll
            .fields
            .iter()
            .find(|f| f.key == *key)
            .unwrap_or_else(|| panic!("{collection} has no {key} field"));
        assert!(
            field.at_creation || field.derived,
            "the API refuses to change {collection}.{key} after creation, and the console \
             still offers it in the edit form. Set at_creation: true, or the only thing an \
             operator can do with that control is be refused by it."
        );
    }
}

/// Every rung the model has is one the console can hand out.
///
/// The roles are a contract of their own — the model decides them, the wire
/// carries their names, the contract document lists them, and the console has
/// to offer them. A rung that existed in one place and not the others would be
/// a role somebody is granted and nothing honours, or one nobody can grant.
///
/// Checked against the console's actual source rather than a copy of it, so a
/// rung added to the model and forgotten in the picker fails here.
#[test]
fn every_role_the_model_has_is_offered_by_the_console() {
    use velstra_cloud_model::authz::Role;

    let page = velstra_cloud_console::page();
    for role in [Role::Viewer, Role::Operator, Role::Editor, Role::Admin] {
        let name = serde_json::to_value(role).unwrap();
        let name = name.as_str().expect("a role serialises as its name");
        assert!(
            page.contains(&format!("id: \"{name}\"")),
            "the model has a `{name}` role and the console does not offer it — a rung nobody \
             can grant is a rung that does not exist"
        );
    }

    // And the contract says the same four, in the same spelling.
    let contract = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("docs/rest-contract.md"),
    )
    .expect("the contract is beside the code");
    for name in ["viewer", "operator", "editor", "admin"] {
        assert!(
            contract.contains(&format!("`{name}`")),
            "the contract does not mention the `{name}` role"
        );
    }
}

/// The console must not read a condition nothing writes.
///
/// A collection judged by a name no agent or controller ever sets reads as
/// "not reported" for ever — for every object in it, in every cell. Measured
/// on a real one: a hundred and nine objects on the attention list, three of
/// which were actually wrong, and the three unfindable among them. Networks
/// were on that list because the console read `Ready` while the controller
/// wrote `Mirrored`.
///
/// Checked against the source of everything that writes a condition, so a
/// controller renaming one and forgetting the console fails here rather than in
/// somebody's overview.
#[test]
fn every_condition_the_console_reads_is_one_something_writes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace is above this crate");

    // Everything that could write one: the controllers, the node agent, the
    // pool agent, and the model's own computed conditions.
    let mut written = String::new();
    for crate_dir in [
        "velstra-cloud-controller/src",
        "velstra-cloud-nodeagent/src",
        "velstra-cloud-model/src",
        "velstra-cloud-api/src",
    ] {
        let dir = root.join(crate_dir);
        let mut stack = vec![dir];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        written.push_str(&text);
                    }
                }
            }
        }
    }

    for collection in COLLECTIONS {
        // An empty name is the collection saying nothing reports on it, which
        // its own guard checks.
        if collection.condition.is_empty() {
            continue;
        }
        assert!(
            written.contains(&format!("\"{}\"", collection.condition)),
            "the console judges {} by a `{}` condition, and nothing in this platform writes one — \
             every object in that collection reads as \"not reported\" for ever",
            collection.id,
            collection.condition
        );
    }
}

/// The console and the model agree on what nobody reports on.
///
/// Two copies of that list drift, and the drift is expensive in both
/// directions: a kind the console thinks is reported fills the attention list
/// for ever, and a kind the model thinks is reported leaves every operation for
/// it unfinished — which is what a client polls after a create.
///
/// The model's list is the one; this is what stops the console keeping a
/// second.
#[test]
fn the_console_and_the_model_agree_on_what_nobody_reports_on() {
    use velstra_cloud_model::reconcile::nobody_reports_on;

    for collection in COLLECTIONS {
        let console_says = collection.condition.is_empty();
        let model_says = nobody_reports_on(collection.id);
        assert_eq!(
            console_says, model_says,
            "the console says nothing reports on {} is {console_says}, the model says \
             {model_says} — one of them fills an attention list for ever and the other leaves \
             every operation for it unfinished",
            collection.id
        );
    }
}

/// The console hides what the API refuses.
///
/// Two answers to one question — "is this the cell's own?" — written in two
/// languages. The API refuses a tenant's list of them; the console leaves them
/// out of the rail. Drift either way is bad in a specific direction: an entry
/// the console shows and the API refuses is a menu item that answers 403, and
/// one the API serves and the console hides is a capability nobody can reach.
#[test]
fn the_console_hides_exactly_what_the_api_keeps_for_the_cell() {
    let page = velstra_cloud_console::page_ref();
    let start = page
        .find("const CELL_ONLY = [")
        .expect("the console names what it hides");
    let end = page[start..].find("];").expect("a closing bracket") + start;
    let listed: std::collections::BTreeSet<&str> = page[start..end]
        .split('"')
        .filter(|s| s.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !s.is_empty())
        .collect();

    for kind in velstra_cloud_api::COLLECTIONS {
        let api_keeps = velstra_cloud_model::authz::belongs_to_the_cell(kind);
        if api_keeps {
            assert!(
                listed.contains(kind),
                "the API refuses a tenant's list of {kind} and the console still offers it —                  a menu item that answers 403"
            );
        }
    }
}

