//! Objects written before today's fields existed still read — and read safely.
//!
//! ## Why this is not covered by anything else
//!
//! Every other test in this workspace builds its fixtures out of *today's*
//! types, so every one of them is a test of a fresh object. The store, though,
//! holds JSON written by whichever version of this code was running when the
//! object was created. A field added with `#[serde(default)]` reads back —
//! that is the whole point of the attribute — but nothing checked it, and
//! "nothing checked it" is how a `deny_unknown_fields` added for a good reason
//! somewhere makes yesterday's cell unreadable on a Tuesday morning.
//!
//! ## The half that actually matters
//!
//! Not that it parses: **what it parses to**. Every default here is a decision
//! about a machine somebody is running right now, and each one has to be the
//! conservative reading:
//!
//!  * a node that never mentioned fencing does not fence, so its guests are
//!    never recovered elsewhere — because "unreachable" is not "stopped";
//!  * a node that never mentioned an overcommit hands out one vCPU per core;
//!  * a guest that never mentioned `onNodeLoss` is left where it is;
//!  * an anti-affinity group that never mentioned a strength is a **rule**,
//!    not a wish.
//!
//! Every one of those, read the other way, is a fleet doing something nobody
//! asked it to do the moment it is upgraded.
//!
//! ## How to add to it
//!
//! Paste the object as it was actually stored — snake_case, because the store
//! holds the model's own spelling and the wire conversion happens above it —
//! and assert the reading, not the parse.

use velstra_cloud_model::{
    ha::OnNodeLoss,
    resources::{
        Capacity, ImageSpec, Instance, InstanceSpec, InstanceStatus, Node, NodeSpec, Project,
        Strength, VolumeSpec,
    },
};

/// A node as it was stored before fencing, baselines, overcommit, PCI or
/// evacuation existed.
#[test]
fn a_node_from_before_half_of_this_platform_reads_conservatively() {
    let stored = serde_json::json!({
        "meta": {
            "name": { "segments": ["nodes", "node-a"] },
            "uid": "11111111-1111-4111-8111-111111111111",
            "generation": 1,
            "revision": 7,
            "placement": { "region": "eu-central", "cell": "cell-1" },
            "created_at": 1_700_000_000_000u64,
            "deleted_at": null,
            "finalizers": [],
            "labels": {}
        },
        "spec": { "schedulable": true, "labels": ["ssd"] },
        "status": {
            "observed_generation": 1,
            "conditions": [],
            "capacity": {
                "vcpus": 16, "memory_mib": 65536, "disk_gib": 1000,
                "numa_free_mib": [32768, 32768], "hugepages_1gi": 0
            },
            "allocated": {
                "vcpus": 4, "memory_mib": 8192, "disk_gib": 40,
                "numa_free_mib": [], "hugepages_1gi": 0
            },
            "agent_version": "0.1.0",
            "last_heartbeat": 1_700_000_000_000u64,
            "images": []
        }
    });

    let node: Node = serde_json::from_value(stored).expect("a stored node still reads");

    // It does not stop its own guests, so nothing recovers them elsewhere.
    // Read the other way, an upgrade would start guests on a second machine
    // while the first one is merely unreachable — which is how two guests come
    // to write to one volume.
    assert_eq!(node.spec.fence_after_s, 0);
    // One vCPU per core. A default of anything else would quietly oversubscribe
    // every machine in the cell on the day of the upgrade.
    assert_eq!(node.spec.vcpu_overcommit, 0);
    assert!(!node.spec.evacuate, "an upgrade started emptying a node");
    assert_eq!(node.spec.cpu_baseline, None);
    // Nothing is claimed about hardware nobody reported.
    assert!(node.status.pci_devices.is_empty());
    assert!(
        node.status.cpu.is_none(),
        "a cpu was invented for a node that never sent one"
    );
    // And what it did say is still there.
    assert_eq!(node.spec.labels, vec!["ssd".to_string()]);
    assert_eq!(node.status.capacity.vcpus, 16);
}

/// A guest from before consoles, devices, recovery, start order and placement
/// strengths.
#[test]
fn a_guest_from_before_all_of_that_reads_conservatively_too() {
    let stored = serde_json::json!({
        "meta": {
            "name": { "segments": ["projects", "p1", "instances", "web-1"] },
            "uid": "22222222-2222-4222-8222-222222222222",
            "generation": 2,
            "revision": 9,
            "placement": { "region": "eu-central", "cell": "cell-1" },
            "created_at": 1_700_000_000_000u64,
            "deleted_at": null,
            "finalizers": [],
            "labels": { "env": "prod" }
        },
        "spec": {
            "vcpus": 2,
            "memory_mib": 4096,
            "image": "projects/p1/images/sha256-abc",
            "root_disk_gib": 20,
            "desired_state": "Running",
            "ports": ["projects/p1/ports/web-1-eth0"],
            "ssh_keys": [],
            "user_data": null,
            "node": "node-a",
            "placement_policy": {
                "anti_affinity_group": "web",
                "required_labels": []
            }
        },
        "status": {
            "observed_generation": 2,
            "conditions": [],
            "state": "Running",
            "node": "node-a",
            "addresses": ["10.20.0.11"],
            "vmm_pid": 4711,
            "started_at": 1_700_000_000_000u64
        }
    });

    let guest: Instance = serde_json::from_value(stored).expect("a stored guest still reads");

    // Left where it is when its node goes quiet. The other reading restarts
    // somebody's database on a second machine because they upgraded the
    // control plane.
    assert_eq!(guest.spec.on_node_loss, OnNodeLoss::Leave);
    // Keeping the group apart stays a **rule**. Read as a wish, an upgrade
    // would allow two replicas onto one machine at the next placement — with
    // nothing on screen having changed.
    assert_eq!(guest.spec.placement_policy.spread, Strength::Required);
    assert_eq!(guest.spec.placement_policy.affinity, Strength::Required);
    assert_eq!(guest.spec.placement_policy.affinity_group, None);
    assert!(
        !guest.spec.console,
        "a console was opened on a guest that never asked for one"
    );
    assert!(guest.spec.devices.is_empty());
    assert_eq!(guest.spec.start_order, 0);
    assert_eq!(guest.spec.start_delay_s, 0);
    // Nothing is claimed about what it is running with.
    assert!(guest.status.cpu.is_none());
    assert!(guest.status.running_size.is_none());
    assert_eq!(guest.status.console_bytes, 0);
    // And what it said is intact, labels and all.
    assert_eq!(guest.spec.vcpus, 2);
    assert_eq!(
        guest.meta.labels.get("env").map(String::as_str),
        Some("prod")
    );
}

/// A project from before three of its quota dimensions existed.
#[test]
fn a_project_from_before_three_of_its_limits_existed_is_not_suddenly_capped() {
    let stored = serde_json::json!({
        "meta": {
            "name": { "segments": ["projects", "p1"] },
            "uid": "33333333-3333-4333-8333-333333333333",
            "generation": 1,
            "revision": 3,
            "placement": { "region": "eu-central", "cell": "cell-1" },
            "created_at": 1_700_000_000_000u64,
            "deleted_at": null,
            "finalizers": [],
            "labels": {}
        },
        "spec": {
            "display_name": "Platform",
            "parent": "organizations/o1",
            "quota": {
                "instances": 20, "vcpus": 200, "memory_mib": 524288,
                "volumes": 40, "volume_gib": 4096, "floating_ips": 8
            },
            "bindings": []
        },
        "status": { "observed_generation": 1, "conditions": [], "used": {} }
    });

    let project: Project = serde_json::from_value(stored).expect("a stored project still reads");

    // Zero is "nobody set one", which the quota checker skips — so a project
    // that predates these two dimensions is unlimited in them rather than
    // unable to create a single one.
    assert_eq!(project.spec.quota.load_balancers, 0);
    assert_eq!(project.spec.quota.devices, 0);
    assert_eq!(project.spec.quota.instances, 20);
    // A status written before anything counted reads as nothing counted, not
    // as a refusal.
    assert_eq!(project.status.used.vcpus, 0);

    // And a project stored before there *was* a policy keeps what its tenants
    // could already do. Reading it as the closed policy would take passthrough
    // and public addresses away from every existing project on the next
    // upgrade, with no operator involved and nothing said — the exact failure
    // this file exists to stop.
    assert!(
        project.spec.policy.device_passthrough,
        "an upgrade took hardware passthrough away from a project that had it"
    );
    assert!(
        project.spec.policy.floating_ips,
        "an upgrade took public addresses away from a project that had them"
    );
    // Host bridges are the exception, and honestly so: there was no such thing
    // before, so nobody loses one.
    assert!(project.spec.policy.host_bridges.is_empty());

    // A project made **today** is closed. Two different questions, and the
    // difference is deliberate: `Default` answers "what should a new tenant
    // get", serde answers "what did this object mean when it was written".
    let fresh = velstra_cloud_model::resources::ProjectSpec::default();
    assert!(!fresh.policy.device_passthrough);
    assert!(!fresh.policy.floating_ips);
}

/// The smaller specs that grew a field each.
#[test]
fn a_volume_and_an_image_from_before_their_new_sources_still_read() {
    let volume: VolumeSpec = serde_json::from_value(serde_json::json!({
        "size_gib": 40,
        "pool": "nvme",
        "source_snapshot": null
    }))
    .expect("a stored volume still reads");
    assert_eq!(volume.size_gib, 40);
    assert_eq!(
        volume.source_backup, None,
        "a volume was given a backup it never had"
    );

    let image: ImageSpec = serde_json::from_value(serde_json::json!({
        "digest": "sha256:abc",
        "format": "Raw",
        "size_bytes": 1_181_116_006u64,
        "source_url": "file:///srv/images/sha256-abc"
    }))
    .expect("a stored image still reads");
    assert_eq!(image.source_instance, None);
    assert_eq!(image.signature, None);
}

/// The two shapes that are read on every pass, so a break here is a cell that
/// stops reconciling rather than a screen that looks wrong.
#[test]
fn the_shapes_a_controller_reads_on_every_pass_still_read() {
    let capacity: Capacity = serde_json::from_value(serde_json::json!({
        "vcpus": 8, "memory_mib": 16384, "disk_gib": 500
    }))
    .expect("a capacity written before numa and hugepages still reads");
    assert_eq!(capacity.vcpus, 8);
    assert!(capacity.numa_free_mib.is_empty());
    assert_eq!(capacity.hugepages_1gi, 0);

    let spec: NodeSpec = serde_json::from_value(serde_json::json!({ "schedulable": false }))
        .expect("the smallest node spec there ever was still reads");
    assert!(!spec.schedulable);
    assert!(spec.labels.is_empty());

    let status: InstanceStatus = serde_json::from_value(serde_json::json!({
        "observed_generation": 1,
        "conditions": [],
        "state": "Stopped"
    }))
    .expect("the smallest instance status there ever was still reads");
    assert!(status.node.is_none());
    assert!(status.devices.is_empty());

    // Deliberately *not* asserted here: an instance spec without
    // `desired_state`, `ports` or `image`. Those have been on the type since
    // before any of this session's work, so no stored object lacks them, and a
    // test demanding they be optional would be asking for a shape that never
    // existed — which ends with every field defaulted and a malformed object
    // reading as a valid one full of zeroes. What the guest test above covers
    // is the real case: a whole object from before *these* fields.
    let spec: InstanceSpec = serde_json::from_value(serde_json::json!({
        "vcpus": 1,
        "memory_mib": 512,
        "image": "projects/p1/images/sha256-abc",
        "root_disk_gib": 10,
        "desired_state": "Stopped",
        "ports": [],
        "ssh_keys": [],
        "user_data": null,
        "node": null,
        "placement_policy": { "anti_affinity_group": null, "required_labels": [] }
    }))
    .expect("an instance spec from before this session still reads");
    assert_eq!(
        spec.desired_state,
        velstra_cloud_model::resources::DesiredState::Stopped
    );
    assert_eq!(spec.placement_policy.spread, Strength::Required);
}
