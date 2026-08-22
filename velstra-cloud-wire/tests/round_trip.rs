//! Every field of every resource survives its own wire.
//!
//! The conversion between the model's `snake_case` and the contract's
//! `camelCase` is a *guess* in one direction: `hugepages1gi` and `l3Vni` are the
//! same shape on the wire and came from different names. The guess is documented
//! and correct for both — and it is a guess, so the only honest defence is to
//! run every name the model actually has through it.
//!
//! This is what caught `l3_vni` coming back as `l_3_vni`, which meant a router
//! could not be written back from the shape it was read in. It had been that way
//! since routers were added, and nothing noticed because no client sends a
//! status.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use velstra_cloud_model::{
    ceph::{BlockDevice, CephClusterSpec, CephClusterStatus, DeviceUse, NodeCeph, OsdSpec},
    identity::{
        CredentialSpec, CredentialStatus, SessionSpec, SessionStatus, UserSpec, UserStatus,
    },
    migration::{MigrationSpec, MigrationStatus},
    resources::*,
};

/// The wire's own promise: what goes out comes back.
fn survives<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(what: &str, value: T) {
    let json = serde_json::to_value(&value).expect("serialises");
    let wire = velstra_cloud_wire::to_wire(json.clone());

    // Every key on the wire is camelCase. A snake_case key that leaked through
    // is a field a client is told about in a spelling the contract does not use.
    assert_no_underscores(what, &wire, "");

    let back = velstra_cloud_wire::from_wire(wire);
    assert_eq!(
        back, json,
        "{what}: the wire did not give back what it was given"
    );
    let parsed: T = serde_json::from_value(back)
        .unwrap_or_else(|e| panic!("{what}: cannot be read back from its own wire: {e}"));
    assert_eq!(parsed, value, "{what}: round-tripped into something else");
}

fn assert_no_underscores(what: &str, value: &Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                // Labels and other free-form maps hold keys a person chose, and
                // `cost_center` is theirs to spell. Only *field* names are the
                // contract's.
                if key == "labels" {
                    continue;
                }
                assert!(
                    !key.contains('_'),
                    "{what}: {path}{key} reaches the wire in snake_case"
                );
                assert_no_underscores(what, child, &format!("{path}{key}."));
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_no_underscores(what, item, path);
            }
        }
        _ => {}
    }
}

/// A status with every field set, because a defaulted one skips the fields that
/// are `skip_serializing_if` — which are exactly the ones a round trip can lose
/// without anybody noticing.
#[test]
fn every_resource_survives_its_own_wire() {
    survives(
        "NodeStatus",
        NodeStatus {
            observed_generation: 7,
            conditions: vec![],
            capacity: Capacity {
                vcpus: 64,
                memory_mib: 1,
                disk_gib: 2,
                numa_free_mib: vec![3],
                // The field the digit rule was written for.
                hugepages_1gi: 16,
            },
            allocated: Capacity::default(),
            agent_version: "0.1.0".into(),
            last_heartbeat: velstra_cloud_model::meta::Timestamp(1),
            images: vec!["projects/p1/images/sha256-abc".into()],
            devices: vec![BlockDevice {
                path: "/dev/sdb".into(),
                kernel_name: "sdb".into(),
                size_gib: 500,
                rotational: true,
                model: "ST500".into(),
                serial: "X1".into(),
                state: DeviceUse::Filesystem {
                    fstype: "ext4".into(),
                },
            }],
            ceph: Some(NodeCeph {
                installed: true,
                version: "19.2.0".into(),
                monitor: true,
                manager: true,
                osd_devices: vec!["/dev/sdc".into()],
                pools: vec!["velstra-volumes".into()],
                cluster_hosts: vec!["hv-1".into()],
                address: "10.0.0.5".into(),
                ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
                trusts_key: true,
            }),
        },
    );

    // The one that was broken: `l3_vni` is the field whose digit sits inside the
    // word rather than starting one.
    survives(
        "RouterStatus",
        RouterStatus {
            observed_generation: 3,
            conditions: vec![],
            gateway_mac: "02:00:5e:00:00:01".into(),
            l3_vni: 50100,
        },
    );
    survives(
        "RouterSpec",
        RouterSpec {
            networks: vec!["projects/p1/networks/n1".into()],
        },
    );

    survives(
        "FloatingIpStatus",
        FloatingIpStatus {
            observed_generation: 1,
            conditions: vec![],
            fabric_id: "fip-1".into(),
            associated: "projects/p1/ports/p1".into(),
        },
    );

    survives(
        "CephClusterSpec",
        CephClusterSpec {
            public_network: "10.0.0.0/24".into(),
            cluster_network: "10.1.0.0/24".into(),
            monitors: vec!["hv-1".into()],
            osds: vec![OsdSpec {
                node: "hv-1".into(),
                device: "/dev/sdb".into(),
            }],
            pools: vec![velstra_cloud_model::ceph::CephPoolSpec {
                pool: "velstra-volumes".into(),
                size: 3,
                min_size: 2,
            }],
            paused: true,
        },
    );
    survives(
        "CephClusterStatus",
        CephClusterStatus {
            ssh_pubkey: "ssh-ed25519 AAAA cluster".into(),
            observed_generation: 2,
            conditions: vec![],
            phase: velstra_cloud_model::ceph::CephPhase::Expanding,
            monitors_up: vec!["hv-1".into()],
            managers_up: vec!["hv-1".into()],
            osds_up: vec![OsdSpec {
                node: "hv-1".into(),
                device: "/dev/sdb".into(),
            }],
            pools_present: vec!["velstra-volumes".into()],
        },
    );

    survives(
        "UserSpec",
        UserSpec {
            display_name: "Ada".into(),
            email: "ada@example.org".into(),
            disabled: true,
            cell_admin: true,
        },
    );
    survives(
        "UserStatus",
        UserStatus {
            observed_generation: 1,
            conditions: vec![],
            last_login: velstra_cloud_model::meta::Timestamp(9),
        },
    );
    survives(
        "CredentialSpec",
        CredentialSpec {
            password_hash: "$argon2id$…".into(),
            updated_at: velstra_cloud_model::meta::Timestamp(1),
        },
    );
    survives("CredentialStatus", CredentialStatus::default());
    survives(
        "SessionSpec",
        SessionSpec {
            subject: "ada".into(),
            expires_at: velstra_cloud_model::meta::Timestamp(2),
            issued_at: velstra_cloud_model::meta::Timestamp(1),
        },
    );
    survives("SessionStatus", SessionStatus::default());

    survives(
        "InstanceSpec",
        InstanceSpec {
            vcpus: 2,
            memory_mib: 4096,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 20,
            desired_state: DesiredState::Running,
            ports: vec!["projects/p1/ports/p1".into()],
            ssh_keys: vec!["ssh-ed25519 …".into()],
            user_data: Some("#cloud-config".into()),
            node: Some("hv-1".into()),
            placement_policy: PlacementPolicy::default(),
        },
    );
    survives(
        "MigrationSpec",
        MigrationSpec {
            instance: "projects/p1/instances/i1".into(),
            from_node: "hv-1".into(),
            to_node: "hv-2".into(),
            mode: velstra_cloud_model::migration::MigrationMode::Live,
            downtime_ms: 300,
            connections: 2,
            timeout_s: 600,
        },
    );
    survives("MigrationStatus", MigrationStatus::default());
    survives(
        "VolumeSpec",
        VolumeSpec {
            size_gib: 10,
            pool: "pools/p".into(),
            source_image: Some("projects/p1/images/sha256-abc".into()),
            source_snapshot: None,
            encryption_key: Some("keys/k".into()),
        },
    );
    survives("PoolStatus", PoolStatus::default());
    survives("SubnetSpec", SubnetSpec::default());
    survives("PortSpec", PortSpec::default());
    survives(
        "SecurityGroupSpec",
        velstra_cloud_model::security::SecurityGroupSpec::default(),
    );
    survives("ImageSpec", ImageSpec::default());
    survives("ProjectSpec", ProjectSpec::default());
    survives("OperationStatus", OperationStatus::default());
    survives("AttachmentSpec", AttachmentSpec::default());
    survives("SnapshotSpec", SnapshotSpec::default());
    survives("NetworkSpec", NetworkSpec::default());
}

/// The two shapes the digit rule has to tell apart, spelled out.
#[test]
fn a_digit_in_a_field_name_survives_whichever_word_it_belongs_to() {
    for name in [
        // The digit starts a word: `hugepages` + `1gi`.
        "hugepages_1gi",
        // The digit is inside one: `l3` + `vni`.
        "l3_vni",
        "l3_vni_extra",
        "ipv6_address",
        "sha256_digest",
        "size_gib",
        "observed_generation",
    ] {
        let camel = velstra_cloud_wire::to_camel(name);
        let back = velstra_cloud_wire::to_wire(serde_json::json!({ camel.clone(): 1 }));
        let snake = velstra_cloud_wire::from_wire(back);
        let got = snake.as_object().unwrap().keys().next().unwrap().clone();
        assert_eq!(got, name, "{name} became {camel} and came back as {got}");
    }
}
