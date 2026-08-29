//! Every model type that crosses the wire, and every field of it.
//!
//! `convert.rs` has one round-trip test, for an instance, and it is honest
//! about what it is for: what a customer's SDK sends is what the platform
//! reasons about. What it cannot do is speak for the other eleven types, and
//! that is exactly the shape a dropped field hides in — the sibling data-plane
//! repository shipped a protobuf message carrying three of a struct's ten
//! fields, and port security, rate limiting, MSS clamping and NAT were built,
//! unit-tested and inert on every node, because the tests exercised the config
//! struct and never the wire.
//!
//! ## Two halves, and neither is enough alone
//!
//! **Equality** catches a conversion that loses a field: give every field a
//! value, send it through the proto and back, and compare.
//!
//! **Destructuring** catches the way that check goes quietly blind. A field
//! added later and left at its default round-trips to itself whether or not the
//! conversion carries it, so the equality assertion stays green while the field
//! is silently dropped on every real request. So each test also destructures
//! the value with no `..` and asserts every field differs from the type's
//! default — which makes an eleventh field a **compile error** here rather than
//! an omission nobody notices.
//!
//! The one deliberate exception is stated where it is made.

use velstra_cloud_model::{meta, migration, resources};
use velstra_cloud_proto::v1;

/// One type, its populated value, and every field of it named.
///
/// The field list is not documentation: it is a `let` pattern with no `..`, so
/// adding a field to the model without adding it here does not compile.
macro_rules! survives_the_wire {
    ($name:ident, $model:path, $proto:ty, $value:expr, { $($field:ident),+ $(,)? }) => {
        #[test]
        fn $name() {
            let original: $model = $value;

            // Nothing is at its default, so the comparison below is actually
            // asking something of every field.
            let default = <$model>::default();
            let $model { $($field),+ } = &original;
            $(
                assert_ne!(
                    $field, &default.$field,
                    concat!(
                        stringify!($model), ".", stringify!($field),
                        " is at its default value in this test, so the round trip would hold ",
                        "even if the conversion dropped it. Give it a distinct value."
                    )
                );
            )+

            let wire = <$proto>::from(&original);
            let back = <$model>::from(&wire);
            assert_eq!(
                back, original,
                concat!(stringify!($model), " did not survive the round trip intact")
            );
        }
    };
}

fn a_condition() -> meta::Condition {
    meta::Condition {
        kind: "Ready".into(),
        status: meta::ConditionStatus::False,
        reason: "VmFailed".into(),
        message: "the virtual machine exited".into(),
        observed_generation: 6,
        last_transition: meta::Timestamp(1_786_732_801_000),
    }
}

// ---- meta -----------------------------------------------------------------

#[test]
fn a_placement_survives_the_wire() {
    // Not the macro: `Placement` has no `Default`, because a cell with an empty
    // region is not a sensible zero value for anything.
    let original = meta::Placement::new("eu-central", "cell-1");
    let meta::Placement { region, cell } = &original;
    assert!(!region.is_empty() && !cell.is_empty());
    let back = meta::Placement::from(&v1::Placement::from(&original));
    assert_eq!(back, original);
}

#[test]
fn a_condition_survives_the_wire() {
    let original = a_condition();
    let meta::Condition {
        kind,
        status,
        reason,
        message,
        observed_generation,
        last_transition,
    } = &original;
    assert!(!kind.is_empty());
    assert_ne!(*status, meta::ConditionStatus::Unknown);
    assert!(!reason.is_empty());
    assert!(!message.is_empty());
    assert_ne!(*observed_generation, 0);
    assert_ne!(*last_transition, meta::Timestamp(0));

    let back = meta::Condition::from(&v1::Condition::from(&original));
    assert_eq!(back, original);
}

#[test]
fn a_meta_survives_the_wire() {
    // `TryFrom` rather than `From`, and no `Default`: a name is parsed, and a
    // malformed one has to be a rejection rather than a plausible-looking
    // empty.
    let original = meta::Meta {
        name: meta::ResourceName::parse("projects/p1/instances/i1").unwrap(),
        uid: "6b1f-0000".into(),
        placement: meta::Placement::new("eu-central", "cell-1"),
        generation: 7,
        created_at: meta::Timestamp(1_786_732_800_000),
        deleted_at: Some(meta::Timestamp(1_786_732_900_000)),
        finalizers: vec!["node.velstra.io/release".into()],
        labels: [("tier".to_string(), "web".to_string())]
            .into_iter()
            .collect(),
        revision: meta::Revision(412),
    };
    let meta::Meta {
        name,
        uid,
        placement,
        generation,
        created_at,
        deleted_at,
        finalizers,
        labels,
        revision,
    } = &original;
    assert!(!name.to_string().is_empty());
    assert!(!uid.is_empty());
    assert!(!placement.region.is_empty());
    assert_ne!(*generation, 0);
    assert_ne!(*created_at, meta::Timestamp(0));
    assert!(deleted_at.is_some());
    assert!(!finalizers.is_empty());
    assert!(!labels.is_empty());
    assert_ne!(*revision, meta::Revision(0));

    let back = meta::Meta::try_from(&v1::Meta::from(&original)).unwrap();
    assert_eq!(back, original);
}

// ---- project --------------------------------------------------------------

survives_the_wire!(
    a_quota_survives_the_wire,
    resources::Quota,
    v1::Quota,
    resources::Quota {
        devices: 4,
        instances: 40,
        vcpus: 200,
        memory_mib: 512_000,
        volumes: 60,
        volume_gib: 20_000,
        floating_ips: 12,
        load_balancers: 6,
    },
    { instances, vcpus, memory_mib, volumes, volume_gib, floating_ips, load_balancers, devices }
);

#[test]
fn a_project_spec_survives_the_wire_except_its_bindings() {
    // The one deliberate exception, and it is deliberate in both directions:
    // there is no protobuf message for a binding, so a project that arrives
    // over gRPC carries none and one that leaves loses them. Empty is the safe
    // direction — it grants nothing rather than inventing a grant nobody asked
    // for — and REST is where a policy is written.
    //
    // Asserted rather than merely commented, because the thing that makes this
    // safe is *not* the conversion: it is that `ProjectSpec.bindings` is
    // `skip_serializing_if = "Vec::is_empty"`, so the JSON a gRPC update
    // produces has no `bindings` key at all and the API's merge-patch leaves the
    // stored policy alone. Take that attribute off and every `UpdateProject`
    // over gRPC silently revokes every grant in the project. The test that says
    // so lives in `velstra-cloud-api/tests/authz.rs`; this one pins the field
    // list, so that a *second* field cannot join `bindings` by accident.
    let original = resources::ProjectSpec {
        display_name: "Payments".into(),
        parent: "organizations/o1".into(),
        quota: resources::Quota {
            devices: 0,
            instances: 40,
            vcpus: 200,
            memory_mib: 512_000,
            volumes: 60,
            volume_gib: 20_000,
            floating_ips: 12,
            load_balancers: 6,
        },
        bindings: vec![velstra_cloud_model::authz::Binding {
            role: velstra_cloud_model::authz::Role::Admin,
            members: vec!["ada".into()],
        }],
        policy: resources::ProjectPolicy {
            host_bridges: vec!["br0".into()],
            device_passthrough: true,
            floating_ips: true,
        },
        cell: "cell-2".into(),
    };
    let default = resources::ProjectSpec::default();
    let resources::ProjectSpec {
        policy,
        display_name,
        parent,
        quota,
        bindings,
        cell,
    } = &original;
    assert_ne!(display_name, &default.display_name);
    assert_ne!(parent, &default.parent);
    assert_ne!(quota, &default.quota);
    assert_ne!(bindings, &default.bindings);
    assert_ne!(policy, &default.policy);
    assert_ne!(cell, &default.cell);

    let back = resources::ProjectSpec::from(&v1::ProjectSpec::from(&original));
    assert_eq!(back.display_name, original.display_name);
    assert_eq!(back.parent, original.parent);
    assert_eq!(back.quota, original.quota);
    // gRPC carries them now. It used not to, and the note here said so — a
    // project created over gRPC arrived with no bindings at all, which was the
    // safe direction and made the surface useless to anybody automating a
    // tenant's setup.
    assert_eq!(
        back.bindings, original.bindings,
        "a project's bindings did not survive the wire"
    );
    // And what the cell allowed that tenant, which is the other half of setting
    // one up: a policy that did not travel would leave every gRPC-created
    // project on the closed default with no way to say otherwise.
    assert_eq!(
        back.policy, original.policy,
        "a project's policy did not survive the wire"
    );
    // The home cell IS carried, and must be: a project created over gRPC that
    // lost it would silently become a project of whichever cell took the call,
    // and every resource in it would be routed there.
    assert_eq!(
        back.cell, original.cell,
        "the project's home cell did not survive the wire"
    );
}

survives_the_wire!(
    a_project_status_survives_the_wire,
    resources::ProjectStatus,
    v1::ProjectStatus,
    resources::ProjectStatus {
        observed_generation: 6,
        conditions: vec![a_condition()],
        used: resources::Quota {
            devices: 0,
            instances: 3,
            vcpus: 12,
            memory_mib: 24_576,
            volumes: 5,
            volume_gib: 300,
            floating_ips: 2,
            load_balancers: 2,
        },
    },
    { observed_generation, conditions, used }
);

// ---- node -----------------------------------------------------------------

survives_the_wire!(
    a_capacity_survives_the_wire,
    resources::Capacity,
    v1::Capacity,
    resources::Capacity {
        vcpus: 64,
        memory_mib: 262_144,
        disk_gib: 4096,
        numa_free_mib: vec![65_536, 65_536],
        hugepages_1gi: 8,
    },
    { vcpus, memory_mib, disk_gib, numa_free_mib, hugepages_1gi }
);

survives_the_wire!(
    a_node_spec_survives_the_wire,
    resources::NodeSpec,
    v1::NodeSpec,
    resources::NodeSpec {
        // `NodeSpec::default()` is *not* schedulable — the derived zero value,
        // which is also the safe direction for a node nobody has vouched for.
        schedulable: true,
        labels: vec!["ssd".into(), "gpu".into()],
        cpu_baseline: Some(velstra_cloud_model::cpu::CpuLevel::V3),
        // Not the default: a field that crosses the wire as its zero value is
        // a field this test cannot tell from one that was dropped.
        vcpu_overcommit: 4,
        fence_after_s: 60,
        evacuate: true,
        gateway: true,
    },
    { schedulable, labels, cpu_baseline, fence_after_s, evacuate, vcpu_overcommit, gateway }
);

survives_the_wire!(
    a_node_status_survives_the_wire,
    resources::NodeStatus,
    v1::NodeStatus,
    resources::NodeStatus {
        vmm: "qemu".into(),
        fetching: vec!["sha256-0123456789abcdef".into()],
        pci_devices: vec![velstra_cloud_model::pci::PciDevice {
            address: "0000:41:00.0".into(),
            vendor_device: "10de:2204".into(),
            description: "NVIDIA GA102".into(),
            kind: velstra_cloud_model::pci::DeviceKind::Gpu,
            iommu_group: Some(17),
            // A held device, not a free one: `Free` is the default, and a
            // round trip that only ever carried the default proves nothing
            // about the flattening this conversion does.
            state: velstra_cloud_model::pci::DeviceUse::HostDriver {
                driver: "nvidia".into(),
            },
        }],
        observed_generation: 4,
        conditions: vec![a_condition()],
        console_endpoint: "10.0.0.7:8447".into(),
        capacity: resources::Capacity {
            vcpus: 64,
            memory_mib: 262_144,
            disk_gib: 4096,
            numa_free_mib: vec![65_536],
            hugepages_1gi: 8,
        },
        allocated: resources::Capacity {
            vcpus: 8,
            memory_mib: 16_384,
            disk_gib: 200,
            numa_free_mib: vec![32_768],
            hugepages_1gi: 1,
        },
        agent_version: "0.1.0".into(),
        last_heartbeat: meta::Timestamp(1_786_732_802_000),
        images: vec!["projects/p1/images/sha256-abc".into()],
        // One free disk and one that is not, because the *reason* a disk is
        // unavailable is what the console shows and what an operator acts on —
        // a wire that carried "busy" and dropped "ext4" would be a screen that
        // says no and cannot say why.
        devices: vec![
            velstra_cloud_model::ceph::BlockDevice {
                path: "/dev/disk/by-id/wwn-0x5000".into(),
                kernel_name: "sdb".into(),
                size_gib: 500,
                rotational: true,
                model: "ST500".into(),
                serial: "X1".into(),
                state: velstra_cloud_model::ceph::DeviceUse::Free,
            },
            velstra_cloud_model::ceph::BlockDevice {
                path: "/dev/disk/by-id/wwn-0x6000".into(),
                kernel_name: "sdc".into(),
                size_gib: 1000,
                rotational: false,
                model: String::new(),
                serial: String::new(),
                state: velstra_cloud_model::ceph::DeviceUse::Filesystem {
                    fstype: "ext4".into(),
                },
            },
        ],
        ceph: Some(velstra_cloud_model::ceph::NodeCeph {
            installed: true,
            version: "19.2.0".into(),
            monitor: true,
            manager: false,
            osd_devices: vec!["/dev/disk/by-id/wwn-0x7000".into()],
            pools: vec!["velstra-volumes".into()],
            cluster_hosts: vec!["hv-1".into(), "hv-2".into()],
            address: "10.0.0.5".into(),
            ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
            trusts_key: true,
        }),
        cpu: Some(velstra_cloud_model::cpu::NodeCpu {
            arch: "x86_64".into(),
            vendor: "GenuineIntel".into(),
            model_name: "Intel(R) Xeon(R) Gold 6248R".into(),
            family: 6,
            model: 85,
            stepping: 7,
            flags: ["avx", "avx2", "sse4_2"].iter().map(|s| s.to_string()).collect(),
            // Deliberately different from `flags`: a baselined node presents
            // less than it holds, and a conversion that carried one field into
            // both would pass a test where the two were equal.
            presents: "x86-64-v3".into(),
            presented_flags: ["avx", "sse4_2"].iter().map(|s| s.to_string()).collect(),
            can_mask: true,
        }),
    },
    {
        observed_generation, conditions, capacity, allocated, agent_version,
        console_endpoint, last_heartbeat, images, devices, ceph, cpu, pci_devices,
        vmm, fetching,
    }
);

// ---- image ----------------------------------------------------------------

survives_the_wire!(
    an_image_spec_survives_the_wire,
    resources::ImageSpec,
    v1::ImageSpec,
    resources::ImageSpec {
        from: "projects/p1/images/sha256-abc".to_string(),
        family: "debian-13".into(),
        version: "20260815".into(),
        source_instance: Some("projects/p1/instances/golden".into()),
        digest: "sha256:abc".into(),
        format: resources::ImageFormat::Qcow2,
        size_bytes: 4_294_967_296,
        source_url: "https://example.invalid/img.qcow2".into(),
        signature: Some("base64-signature".into()),
    },
    { digest, format, size_bytes, source_url, signature, source_instance, family, version, from }
);

survives_the_wire!(
    an_image_status_survives_the_wire,
    resources::ImageStatus,
    v1::ImageStatus,
    resources::ImageStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
    },
    { observed_generation, conditions }
);

// ---- instance -------------------------------------------------------------

survives_the_wire!(
    a_placement_policy_survives_the_wire,
    resources::PlacementPolicy,
    v1::PlacementPolicy,
    resources::PlacementPolicy {
        anti_affinity_group: Some("web".into()),
        required_labels: vec!["ssd".into()],
        min_cpu_level: Some(velstra_cloud_model::cpu::CpuLevel::V3),
        affinity_group: Some("cache".into()),
        // Not the default: a field that crosses the wire as its zero value is
        // a field this test cannot tell from one that was dropped.
        spread: resources::Strength::Preferred,
        affinity: resources::Strength::Preferred,
    },
    { anti_affinity_group, required_labels, min_cpu_level, affinity_group, spread, affinity }
);

survives_the_wire!(
    an_instance_spec_survives_the_wire,
    resources::InstanceSpec,
    v1::InstanceSpec,
    resources::InstanceSpec {
        start_order: 2,
        start_delay_s: 30,
        on_node_loss: velstra_cloud_model::ha::OnNodeLoss::Restart,
        console: true,
        devices: vec!["gpu-a100".into()],
        vcpus: 4,
        memory_mib: 8192,
        image: "projects/p1/images/sha256-abc".into(),
        root_disk_gib: 40,
        // `Running` is the default, so `Stopped` is the distinct one.
        desired_state: resources::DesiredState::Stopped,
        ports: vec!["projects/p1/ports/port-a".into()],
        networks: vec!["projects/p1/networks/prod".to_string()],
    volumes: vec!["projects/p1/volumes/data".to_string()],
        ssh_keys: vec!["ssh-ed25519 AAAA".into()],
        user_data: Some("#cloud-config".into()),
        node: Some("node-a".into()),
        placement_policy: resources::PlacementPolicy {
            anti_affinity_group: Some("web".into()),
            required_labels: vec!["ssd".into()],
            min_cpu_level: None,
            affinity_group: None,
            spread: resources::Strength::Required,
            affinity: resources::Strength::Required,
        },
    },
    {
        vcpus, memory_mib, image, root_disk_gib, desired_state, ports, networks, volumes, ssh_keys,
        user_data, node, placement_policy, devices, console, on_node_loss,
        start_order, start_delay_s,
    }
);

survives_the_wire!(
    an_instance_status_survives_the_wire,
    resources::InstanceStatus,
    v1::InstanceStatus,
    resources::InstanceStatus {
        stop_requested_at: Some(velstra_cloud_model::meta::Timestamp(1_700_000_000_123)),
        running_size: Some(velstra_cloud_model::resources::RunningSize {
            vcpus: 4,
            memory_mib: 8192,
            root_disk_gib: 40,
        }),
        console_tail: "[    0.000000] Linux version 6.12.63\n".into(),
        console_bytes: 4096,
        devices: vec!["0000:41:00.0".into()],
        cpu: Some(velstra_cloud_model::cpu::GuestCpu {
            model: "x86-64-v3".into(),
            arch: "x86_64".into(),
            flags: ["avx", "sse4_2"].iter().map(|s| s.to_string()).collect(),
        }),
        observed_generation: 6,
        conditions: vec![a_condition()],
        state: resources::InstanceState::Failed,
        node: Some("node-a".into()),
        addresses: vec!["10.0.0.5".into()],
        vmm_pid: Some(4242),
        started_at: Some(meta::Timestamp(1_786_732_801_000)),
    },
    {
        observed_generation, conditions, state, node, addresses, vmm_pid, started_at,
        cpu, devices, console_tail, console_bytes, running_size, stop_requested_at,
    }
);

// ---- storage --------------------------------------------------------------

survives_the_wire!(
    a_volume_spec_survives_the_wire,
    resources::VolumeSpec,
    v1::VolumeSpec,
    resources::VolumeSpec {
        source_backup: Some("projects/p1/backups/nightly-1787687886".into()),
        size_gib: 100,
        pool: "rbd-standard".into(),
        encryption_key: Some("projects/p1/keys/k1".into()),
        source_image: Some("projects/p1/images/sha256-abc".into()),
        source_snapshot: Some("projects/p1/volumes/v1/snapshots/nightly".into()),
    },
    { size_gib, pool, encryption_key, source_image, source_snapshot, source_backup }
);

survives_the_wire!(
    a_volume_status_survives_the_wire,
    resources::VolumeStatus,
    v1::VolumeStatus,
    resources::VolumeStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        provisioned: true,
        actual_size_gib: 100,
        pool: Some("rbd-standard".into()),
        at: Some("rbd:rbd-standard/projects~p1~volumes~data".into()),
    },
    { observed_generation, conditions, provisioned, actual_size_gib, pool, at }
);

survives_the_wire!(
    a_snapshot_spec_survives_the_wire,
    resources::SnapshotSpec,
    v1::SnapshotSpec,
    resources::SnapshotSpec {
        pool: "rbd-standard".into(),
    },
    { pool }
);

survives_the_wire!(
    a_snapshot_status_survives_the_wire,
    resources::SnapshotStatus,
    v1::SnapshotStatus,
    resources::SnapshotStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        pool: Some("rbd-standard".into()),
        taken: true,
        size_gib: 100,
        taken_at: Some(meta::Timestamp(1_786_732_803_000)),
    },
    { observed_generation, conditions, pool, taken, size_gib, taken_at }
);

survives_the_wire!(
    an_attachment_spec_survives_the_wire,
    resources::AttachmentSpec,
    v1::AttachmentSpec,
    resources::AttachmentSpec {
        volume: "projects/p1/volumes/v1".into(),
        instance: "projects/p1/instances/i1".into(),
        node: "node-a".into(),
        at: "/srv/velstra/pool/projects~p1~volumes~data.qcow2".into(),
        read_only: true,
    },
    { volume, instance, node, at, read_only }
);

survives_the_wire!(
    an_attachment_status_survives_the_wire,
    resources::AttachmentStatus,
    v1::AttachmentStatus,
    resources::AttachmentStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
        attached: true,
        device: Some("/dev/vdb".into()),
        node: Some("node-a".into()),
    },
    { observed_generation, conditions, attached, device, node }
);

// ---- networking -----------------------------------------------------------

survives_the_wire!(
    a_network_spec_survives_the_wire,
    resources::NetworkSpec,
    v1::NetworkSpec,
    resources::NetworkSpec {
            host_bridge: "br0".into(),
        vni: 5001,
        mtu: 1450,
        external: true,
        // Not the default, so a field crossing as its zero value cannot be told
        // from one that was dropped.
        announce: velstra_cloud_model::public::Announce::FromHost,
    },
    { vni, mtu, external, announce, host_bridge }
);

survives_the_wire!(
    a_network_status_survives_the_wire,
    resources::NetworkStatus,
    v1::NetworkStatus,
    resources::NetworkStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
    },
    { observed_generation, conditions }
);

survives_the_wire!(
    a_subnet_spec_survives_the_wire,
    resources::SubnetSpec,
    v1::SubnetSpec,
    resources::SubnetSpec {
        network: "projects/p1/networks/n1".into(),
        cidr: "10.20.0.0/24".into(),
        gateway: "10.20.0.1".into(),
        dns: vec!["10.20.0.2".into()],
        reserved: vec!["10.20.0.3".into()],
    },
    { network, cidr, gateway, dns, reserved }
);

survives_the_wire!(
    a_subnet_status_survives_the_wire,
    resources::SubnetStatus,
    v1::SubnetStatus,
    resources::SubnetStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
        allocated: 12,
        available: 240,
    },
    { observed_generation, conditions, allocated, available }
);

survives_the_wire!(
    a_port_spec_survives_the_wire,
    resources::PortSpec,
    v1::PortSpec,
    resources::PortSpec {
        network: "projects/p1/networks/n1".into(),
        subnet: "projects/p1/subnets/s1".into(),
        node: Some("node-a".into()),
        address: Some("10.20.0.7".into()),
        mac: Some("02:ab:cd:ef:00:07".into()),
        security_groups: vec!["projects/p1/security-groups/web".into()],
        rate_limit_mbit: Some(1000),
    },
    { network, subnet, node, address, mac, security_groups, rate_limit_mbit }
);

survives_the_wire!(
    a_port_status_survives_the_wire,
    resources::PortStatus,
    v1::PortStatus,
    resources::PortStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
        node: Some("node-a".into()),
        programmed: true,
        tap_device: Some("vtweb1a2b".into()),
    },
    { observed_generation, conditions, node, programmed, tap_device }
);

// ---- operation ------------------------------------------------------------

survives_the_wire!(
    an_operation_spec_survives_the_wire,
    resources::OperationSpec,
    v1::OperationSpec,
    resources::OperationSpec {
        target: "projects/p1/instances/i1".into(),
        target_generation: 3,
        verb: "create".into(),
        requested_by: "ada".into(),
    },
    { target, target_generation, verb, requested_by }
);

survives_the_wire!(
    an_operation_status_survives_the_wire,
    resources::OperationStatus,
    v1::OperationStatus,
    resources::OperationStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        done: true,
        error: Some("the node refused".into()),
        finished_at: Some(meta::Timestamp(1_786_732_804_000)),
    },
    { observed_generation, conditions, done, error, finished_at }
);

// ---- migration ------------------------------------------------------------

survives_the_wire!(
    a_migration_spec_survives_the_wire,
    migration::MigrationSpec,
    v1::MigrationSpec,
    migration::MigrationSpec {
        instance: "projects/p1/instances/i1".into(),
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        // `Live` is the default, so `PostCopy` is the distinct one.
        mode: migration::MigrationMode::PostCopy,
        downtime_ms: 500,
        timeout_s: 1800,
        connections: 4,
    },
    { instance, from_node, to_node, mode, downtime_ms, timeout_s, connections }
);

survives_the_wire!(
    a_migration_status_survives_the_wire,
    migration::MigrationStatus,
    v1::MigrationStatus,
    migration::MigrationStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
        node: Some("node-b".into()),
        receiver_url: Some("tcp:10.0.0.2:4900".into()),
        receiver_ready: true,
        transferred_mib: 2048,
    },
    { observed_generation, conditions, node, receiver_url, receiver_ready, transferred_mib }
);

// ---- the whole objects ----------------------------------------------------

/// The object level, where the macro in `convert.rs` wires `meta` + `spec` +
/// `status` together for twelve types.
///
/// What is worth asserting here is that the *macro* carries all three halves —
/// a resource that lost its `meta` on the way out would still round-trip its
/// spec and status perfectly — and a type the macro was never applied to does
/// not compile here at all.
macro_rules! whole_object_survives {
    ($name:ident, $model:ty, $proto:ty, $spec:expr, $status:expr) => {
        #[test]
        fn $name() {
            let mut meta = meta::Meta::new(
                meta::ResourceName::parse("projects/p1/instances/i1").unwrap(),
                meta::Placement::new("eu-central", "cell-1"),
            );
            meta.generation = 7;
            meta.revision = meta::Revision(412);
            meta.finalizers = vec!["node.velstra.io/release".into()];
            meta.labels.insert("tier".into(), "web".into());
            meta.deleted_at = Some(meta::Timestamp(1_786_732_800_000));

            let original = <$model>::new(meta, $spec, $status);
            let back = <$model>::try_from(&<$proto>::from(&original)).unwrap();
            assert_eq!(back, original);
        }
    };
}

whole_object_survives!(
    a_whole_project_survives,
    resources::Project,
    v1::Project,
    resources::ProjectSpec {
        policy: Default::default(),
        display_name: "Payments".into(),
        parent: "organizations/o1".into(),
        quota: resources::Quota {
            devices: 0,
            instances: 40,
            vcpus: 200,
            memory_mib: 512_000,
            volumes: 60,
            volume_gib: 20_000,
            floating_ips: 12,
            load_balancers: 6,
        },
        // Not carried on the wire; see the spec test above.
        bindings: Vec::new(),
        cell: "cell-2".into(),
    },
    resources::ProjectStatus {
        observed_generation: 6,
        conditions: vec![a_condition()],
        used: resources::Quota {
            devices: 0,
            instances: 3,
            vcpus: 12,
            memory_mib: 24_576,
            volumes: 5,
            volume_gib: 300,
            floating_ips: 2,
            load_balancers: 2,
        },
    }
);

whole_object_survives!(
    a_whole_node_survives,
    resources::Node,
    v1::Node,
    resources::NodeSpec {
        evacuate: false,
        vcpu_overcommit: 0,
        fence_after_s: 0,
        schedulable: false,
        labels: vec!["ssd".into()],
        cpu_baseline: None,
        gateway: false,
    },
    resources::NodeStatus {
        vmm: "qemu".into(),
            fetching: Vec::new(),
        pci_devices: Vec::new(),
        cpu: None,
        console_endpoint: "10.0.0.7:8447".into(),
        observed_generation: 4,
        conditions: vec![a_condition()],
        capacity: resources::Capacity {
            vcpus: 64,
            memory_mib: 262_144,
            disk_gib: 4096,
            numa_free_mib: vec![65_536],
            hugepages_1gi: 8,
        },
        allocated: resources::Capacity::default(),
        agent_version: "0.1.0".into(),
        last_heartbeat: meta::Timestamp(1_786_732_802_000),
        images: vec!["projects/p1/images/sha256-abc".into()],
        devices: vec![velstra_cloud_model::ceph::BlockDevice {
            path: "/dev/disk/by-id/wwn-0x5000".into(),
            kernel_name: "sdb".into(),
            size_gib: 500,
            rotational: true,
            model: "ST500".into(),
            serial: "X1".into(),
            state: velstra_cloud_model::ceph::DeviceUse::Free,
        }],
        ceph: None,
    }
);

whole_object_survives!(
    a_whole_image_survives,
    resources::Image,
    v1::Image,
    resources::ImageSpec {
        from: String::new(),
        family: "debian-13".into(),
        version: "20260815".into(),
        source_instance: None,
        digest: "sha256:abc".into(),
        format: resources::ImageFormat::Qcow2,
        size_bytes: 4_294_967_296,
        source_url: "https://example.invalid/img.qcow2".into(),
        signature: Some("base64-signature".into()),
    },
    resources::ImageStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
    }
);

whole_object_survives!(
    a_whole_volume_survives,
    resources::Volume,
    v1::Volume,
    resources::VolumeSpec {
        source_backup: None,
        size_gib: 100,
        pool: "rbd-standard".into(),
        encryption_key: Some("projects/p1/keys/k1".into()),
        source_image: None,
        source_snapshot: None,
    },
    resources::VolumeStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        provisioned: true,
        actual_size_gib: 100,
        pool: Some("rbd-standard".into()),
        at: Some("rbd:rbd-standard/projects~p1~volumes~data".into()),
    }
);

whole_object_survives!(
    a_whole_snapshot_survives,
    resources::Snapshot,
    v1::Snapshot,
    resources::SnapshotSpec {
        pool: "rbd-standard".into(),
    },
    resources::SnapshotStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        pool: Some("rbd-standard".into()),
        taken: true,
        size_gib: 100,
        taken_at: Some(meta::Timestamp(1_786_732_803_000)),
    }
);

whole_object_survives!(
    a_whole_attachment_survives,
    resources::Attachment,
    v1::Attachment,
    resources::AttachmentSpec {
        volume: "projects/p1/volumes/v1".into(),
        instance: "projects/p1/instances/i1".into(),
        node: "node-a".into(),
        at: String::new(),
        read_only: true,
    },
    resources::AttachmentStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
        attached: true,
        device: Some("/dev/vdb".into()),
        node: Some("node-a".into()),
    }
);

whole_object_survives!(
    a_whole_network_survives,
    resources::Network,
    v1::Network,
    resources::NetworkSpec {
        host_bridge: "br0".into(),
        vni: 5001,
        mtu: 1450,
        external: false,
        announce: Default::default(),
    },
    resources::NetworkStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
    }
);

whole_object_survives!(
    a_whole_router_survives,
    resources::Router,
    v1::Router,
    resources::RouterSpec {
        networks: vec![
            "projects/p1/networks/front".into(),
            "projects/p1/networks/back".into(),
        ],
    },
    resources::RouterStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        l3_vni: 900_001,
        // Recorded rather than derived on read: a person reading an ARP table
        // has to recognise it, and it must not change under them.
        gateway_mac: "02:00:5e:00:53:01".into(),
    }
);

whole_object_survives!(
    a_whole_floating_ip_survives,
    resources::FloatingIp,
    v1::FloatingIp,
    resources::FloatingIpSpec {
        subnet: "projects/p1/subnets/s1".into(),
        // Present, not `None`: an absent address is the *default*, and a
        // round-trip of the default proves only that nothing was written.
        address: Some("203.0.113.7".into()),
        port: "projects/p1/ports/web".into(),
        delivery: Default::default(),
        announce: None,
    },
    resources::FloatingIpStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
        fabric_id: "fip-9f3c".into(),
        associated: "10.20.0.7".into(),
    }
);

whole_object_survives!(
    a_whole_subnet_survives,
    resources::Subnet,
    v1::Subnet,
    resources::SubnetSpec {
        network: "projects/p1/networks/n1".into(),
        cidr: "10.20.0.0/24".into(),
        gateway: "10.20.0.1".into(),
        dns: vec!["10.20.0.2".into()],
        reserved: vec!["10.20.0.3".into()],
    },
    resources::SubnetStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
        allocated: 12,
        available: 240,
    }
);

whole_object_survives!(
    a_whole_port_survives,
    resources::Port,
    v1::Port,
    resources::PortSpec {
        network: "projects/p1/networks/n1".into(),
        subnet: "projects/p1/subnets/s1".into(),
        node: Some("node-a".into()),
        address: Some("10.20.0.7".into()),
        mac: Some("02:ab:cd:ef:00:07".into()),
        security_groups: vec!["projects/p1/security-groups/web".into()],
        rate_limit_mbit: Some(1000),
    },
    resources::PortStatus {
        observed_generation: 2,
        conditions: vec![a_condition()],
        node: Some("node-a".into()),
        programmed: true,
        tap_device: Some("vtweb1a2b".into()),
    }
);

whole_object_survives!(
    a_whole_operation_survives,
    resources::Operation,
    v1::Operation,
    resources::OperationSpec {
        target: "projects/p1/instances/i1".into(),
        target_generation: 3,
        verb: "create".into(),
        requested_by: "ada".into(),
    },
    resources::OperationStatus {
        observed_generation: 3,
        conditions: vec![a_condition()],
        done: true,
        error: Some("the node refused".into()),
        finished_at: Some(meta::Timestamp(1_786_732_804_000)),
    }
);

whole_object_survives!(
    a_whole_instance_survives,
    resources::Instance,
    v1::Instance,
    resources::InstanceSpec {
        start_order: 0,
        start_delay_s: 0,
        on_node_loss: Default::default(),
        console: false,
        devices: Vec::new(),
        vcpus: 4,
        memory_mib: 8192,
        image: "projects/p1/images/sha256-abc".into(),
        root_disk_gib: 40,
        desired_state: resources::DesiredState::Stopped,
        ports: vec!["projects/p1/ports/port-a".into()],
        networks: vec!["projects/p1/networks/prod".to_string()],
    volumes: vec!["projects/p1/volumes/data".to_string()],
        ssh_keys: vec!["ssh-ed25519 AAAA".into()],
        user_data: Some("#cloud-config".into()),
        node: Some("node-a".into()),
        placement_policy: resources::PlacementPolicy {
            anti_affinity_group: Some("web".into()),
            required_labels: vec!["ssd".into()],
            min_cpu_level: None,
            affinity_group: None,
            spread: resources::Strength::Required,
            affinity: resources::Strength::Required,
        },
    },
    resources::InstanceStatus {
        stop_requested_at: None,
        running_size: None,
        console_tail: String::new(),
        console_bytes: 0,
        devices: Vec::new(),
        cpu: None,
        observed_generation: 6,
        conditions: vec![a_condition()],
        state: resources::InstanceState::Failed,
        node: Some("node-a".into()),
        addresses: vec!["10.0.0.5".into()],
        vmm_pid: Some(4242),
        started_at: Some(meta::Timestamp(1_786_732_801_000)),
    }
);

whole_object_survives!(
    a_whole_migration_survives,
    migration::Migration,
    v1::Migration,
    migration::MigrationSpec {
        instance: "projects/p1/instances/i1".into(),
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        mode: migration::MigrationMode::PostCopy,
        downtime_ms: 500,
        timeout_s: 1800,
        connections: 4,
    },
    migration::MigrationStatus {
        observed_generation: 1,
        conditions: vec![a_condition()],
        node: Some("node-b".into()),
        receiver_url: Some("tcp:10.0.0.2:4900".into()),
        receiver_ready: true,
        transferred_mib: 2048,
    }
);

/// A device state this build does not recognise must never read as "free".
///
/// The wire carries the state as a tag plus a detail, which is what keeps the
/// detail readable — and it means a peer running a newer build can send a tag
/// this one has never seen. Defaulting that to `Free` would put an unknown disk
/// in the list an operator picks OSDs from, and picking it erases it.
///
/// So the unknown case lands on `Unsuitable`, and this is the test that says so.
/// It is the one conversion in this file where being wrong destroys data rather
/// than dropping a field.
#[test]
fn a_device_state_from_the_future_is_never_read_as_empty() {
    use velstra_cloud_model::ceph::{BlockDevice, DeviceUse};

    let from_a_newer_peer = v1::BlockDevice {
        path: "/dev/sdb".into(),
        kernel_name: "sdb".into(),
        size_gib: 500,
        rotational: false,
        model: String::new(),
        serial: String::new(),
        use_kind: "held-by-something-invented-next-year".into(),
        use_detail: "it is part of a thing this build has not heard of".into(),
    };
    let device: BlockDevice = (&from_a_newer_peer).into();
    assert!(
        !device.state.is_free(),
        "an unknown state was read as an empty disk: {:?}",
        device.state
    );
    assert!(velstra_cloud_model::ceph::may_consume(&device).is_err());
    // And the detail survives, so the console can say what it was told rather
    // than "unknown".
    match &device.state {
        DeviceUse::Unsuitable { why } => assert!(why.contains("not heard of"), "{why}"),
        other => panic!("{other:?}"),
    }

    // An unknown tag with no detail still refuses, and says which tag.
    let bare: BlockDevice = (&v1::BlockDevice {
        use_kind: "mystery".into(),
        use_detail: String::new(),
        size_gib: 500,
        ..from_a_newer_peer.clone()
    })
        .into();
    assert!(!bare.state.is_free());
    match &bare.state {
        DeviceUse::Unsuitable { why } => assert!(why.contains("mystery"), "{why}"),
        other => panic!("{other:?}"),
    }
}
