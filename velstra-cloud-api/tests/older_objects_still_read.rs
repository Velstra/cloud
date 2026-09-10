//! A stored object written by an earlier version must still read.
//!
//! This is the whole of what "schema migration" means here. There is no
//! migration step and no version stamp on a stored document: a spec is written
//! as JSON and read back by whatever binary is running. The only thing between
//! "we added a field" and "every object of that kind is unreadable" is that the
//! field has a default — and nothing checks that at the moment it is added.
//!
//! So this keeps a corpus. `tests/stored/<kind>.{spec,status}.json` is what
//! this platform wrote at the moment the corpus was taken, one document per
//! stored type, and the test is that today's types still read those documents.
//! A field added with a default reads fine — the corpus has no key for it and
//! the default fills in, which is exactly the property being asserted. A field
//! added *without* one fails here, on the commit that adds it, rather than on
//! somebody's cell at the next restart.
//!
//! Regenerate with:
//!
//!     VELSTRA_WRITE_STORED=1 cargo test -p velstra-cloud-api --test older_objects_still_read
//!
//! **Regenerating is not how you make this test pass.** It rewrites the corpus
//! to include the new field, which retires the check for every object written
//! before it. Do it when the field has been made defaultable and you want the
//! corpus to move forward, and never as a way past a red build.
//!
//! The other direction — an *older* binary reading a *newer* object — is
//! survivable and deliberately unguarded: serde ignores keys it does not know,
//! which is why a cell part-way through an upgrade works at all. What it costs
//! is that an old writer round-tripping a whole object would erase the new
//! field, and the platform's answer to that is that agents write `status` only,
//! through `report_status`, which overlays onto the stored object rather than
//! replacing it.
//!
//! The list is exhaustive by construction: it is checked against
//! `velstra_cloud_api::COLLECTIONS`, so a collection served by the API and not
//! listed here is a failing test rather than a type nobody proved anything
//! about.

/// `(kind, what this version writes, whether a stored document still reads)`.
type Case = (
    &'static str,
    fn() -> (serde_json::Value, serde_json::Value),
    fn(&serde_json::Value, &serde_json::Value) -> Result<(), String>,
);

macro_rules! cases {
    ($(($kind:literal, $spec:ty, $status:ty)),* $(,)?) => {
        &[$((
            $kind,
            (|| (
                serde_json::to_value(<$spec>::default()).expect("a spec serialises"),
                serde_json::to_value(<$status>::default()).expect("a status serialises"),
            )) as fn() -> (serde_json::Value, serde_json::Value),
            (|spec: &serde_json::Value, status: &serde_json::Value| {
                serde_json::from_value::<$spec>(spec.clone())
                    .map_err(|e| format!("spec: {e}"))?;
                serde_json::from_value::<$status>(status.clone())
                    .map_err(|e| format!("status: {e}"))?;
                Ok(())
            }) as fn(&serde_json::Value, &serde_json::Value) -> Result<(), String>,
        )),*] as &[Case]
    };
}

fn cases() -> &'static [Case] {
    cases![
        (
            "attachments",
            velstra_cloud_model::resources::AttachmentSpec,
            velstra_cloud_model::resources::AttachmentStatus
        ),
        (
            "audit",
            velstra_cloud_model::audit::AuditSpec,
            velstra_cloud_model::audit::AuditStatus
        ),
        (
            "backup-schedules",
            velstra_cloud_model::backup::BackupScheduleSpec,
            velstra_cloud_model::backup::BackupScheduleStatus
        ),
        (
            "backup-targets",
            velstra_cloud_model::backup::BackupTargetSpec,
            velstra_cloud_model::backup::BackupTargetStatus
        ),
        (
            "backups",
            velstra_cloud_model::backup::BackupSpec,
            velstra_cloud_model::backup::BackupStatus
        ),
        (
            "bgp-peers",
            velstra_cloud_model::resources::BgpPeerSpec,
            velstra_cloud_model::resources::BgpPeerStatus
        ),
        (
            "captures",
            velstra_cloud_model::capture::CaptureSpec,
            velstra_cloud_model::capture::CaptureStatus
        ),
        (
            "ceph-clusters",
            velstra_cloud_model::ceph::CephClusterSpec,
            velstra_cloud_model::ceph::CephClusterStatus
        ),
        (
            "console-sessions",
            velstra_cloud_model::console::ConsoleSessionSpec,
            velstra_cloud_model::console::ConsoleSessionStatus
        ),
        (
            "device-classes",
            velstra_cloud_model::pci::DeviceClassSpec,
            velstra_cloud_model::resources::DeviceClassStatus
        ),
        (
            "flavors",
            velstra_cloud_model::resources::FlavorSpec,
            velstra_cloud_model::resources::FlavorStatus
        ),
        (
            "floatingips",
            velstra_cloud_model::resources::FloatingIpSpec,
            velstra_cloud_model::resources::FloatingIpStatus
        ),
        (
            "folders",
            velstra_cloud_model::hierarchy::FolderSpec,
            velstra_cloud_model::hierarchy::FolderStatus
        ),
        (
            "image-sources",
            velstra_cloud_model::images::ImageSourceSpec,
            velstra_cloud_model::images::ImageSourceStatus
        ),
        (
            "images",
            velstra_cloud_model::resources::ImageSpec,
            velstra_cloud_model::resources::ImageStatus
        ),
        (
            "instances",
            velstra_cloud_model::resources::InstanceSpec,
            velstra_cloud_model::resources::InstanceStatus
        ),
        (
            "load-balancers",
            velstra_cloud_model::loadbalancer::LoadBalancerSpec,
            velstra_cloud_model::loadbalancer::LoadBalancerStatus
        ),
        (
            "maintenance-windows",
            velstra_cloud_model::maintenance::MaintenanceWindowSpec,
            velstra_cloud_model::maintenance::MaintenanceWindowStatus
        ),
        (
            "migrations",
            velstra_cloud_model::migration::MigrationSpec,
            velstra_cloud_model::migration::MigrationStatus
        ),
        (
            "networks",
            velstra_cloud_model::resources::NetworkSpec,
            velstra_cloud_model::resources::NetworkStatus
        ),
        (
            "nodes",
            velstra_cloud_model::resources::NodeSpec,
            velstra_cloud_model::resources::NodeStatus
        ),
        (
            "operations",
            velstra_cloud_model::resources::OperationSpec,
            velstra_cloud_model::resources::OperationStatus
        ),
        (
            "pools",
            velstra_cloud_model::resources::PoolSpec,
            velstra_cloud_model::resources::PoolStatus
        ),
        (
            "ports",
            velstra_cloud_model::resources::PortSpec,
            velstra_cloud_model::resources::PortStatus
        ),
        (
            "projects",
            velstra_cloud_model::resources::ProjectSpec,
            velstra_cloud_model::resources::ProjectStatus
        ),
        (
            "roles",
            velstra_cloud_model::hierarchy::RoleSpec,
            velstra_cloud_model::hierarchy::RoleStatus
        ),
        (
            "routers",
            velstra_cloud_model::resources::RouterSpec,
            velstra_cloud_model::resources::RouterStatus
        ),
        (
            "security-groups",
            velstra_cloud_model::security::SecurityGroupSpec,
            velstra_cloud_model::security::SecurityGroupStatus
        ),
        (
            "snapshot-schedules",
            velstra_cloud_model::storage::SnapshotScheduleSpec,
            velstra_cloud_model::storage::SnapshotScheduleStatus
        ),
        (
            "snapshots",
            velstra_cloud_model::resources::SnapshotSpec,
            velstra_cloud_model::resources::SnapshotStatus
        ),
        (
            "subnets",
            velstra_cloud_model::resources::SubnetSpec,
            velstra_cloud_model::resources::SubnetStatus
        ),
        (
            "usage",
            velstra_cloud_model::usage::UsageRecordSpec,
            velstra_cloud_model::usage::UsageRecordStatus
        ),
        (
            "users",
            velstra_cloud_model::identity::UserSpec,
            velstra_cloud_model::identity::UserStatus
        ),
        (
            "volumes",
            velstra_cloud_model::resources::VolumeSpec,
            velstra_cloud_model::resources::VolumeStatus
        ),
    ]
}

/// The list covers exactly the collections the API serves.
#[test]
fn every_collection_the_api_serves_is_covered() {
    let covered: std::collections::BTreeSet<&str> = cases().iter().map(|(k, _, _)| *k).collect();
    let served: std::collections::BTreeSet<&str> =
        velstra_cloud_api::COLLECTIONS.iter().copied().collect();
    let missing: Vec<&&str> = served.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "these collections are served and nothing here proves their objects survive a \
         version change: {missing:?}"
    );
    let extra: Vec<&&str> = covered.difference(&served).collect();
    assert!(
        extra.is_empty(),
        "these are listed here and not served: {extra:?}"
    );
}

fn corpus() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/stored")
}

/// Every document in the corpus still reads into today's type.
#[test]
fn an_object_from_an_older_version_still_reads() {
    let dir = corpus();
    let writing = std::env::var_os("VELSTRA_WRITE_STORED").is_some();
    if writing {
        std::fs::create_dir_all(&dir).expect("making the corpus directory");
    }
    let mut broken = Vec::new();
    for (kind, writes, reads) in cases() {
        let (spec_path, status_path) = (
            dir.join(format!("{kind}.spec.json")),
            dir.join(format!("{kind}.status.json")),
        );
        if writing {
            let (spec, status) = writes();
            for (path, value) in [(&spec_path, &spec), (&status_path, &status)] {
                let text = serde_json::to_string_pretty(value).expect("plain data");
                std::fs::write(path, format!("{text}\n")).expect("writing the corpus");
            }
            continue;
        }
        let read = |path: &std::path::Path| -> Result<serde_json::Value, String> {
            let text = std::fs::read_to_string(path).map_err(|e| {
                format!(
                    "{}: {e}. Take the corpus with \
                     VELSTRA_WRITE_STORED=1 cargo test -p velstra-cloud-api \
                     --test older_objects_still_read",
                    path.display()
                )
            })?;
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
        };
        match (read(&spec_path), read(&status_path)) {
            (Ok(spec), Ok(status)) => {
                if let Err(why) = reads(&spec, &status) {
                    broken.push(format!("{kind}.{why}"));
                }
            }
            (Err(why), _) | (_, Err(why)) => broken.push(format!("{kind}: {why}")),
        }
    }
    assert!(
        broken.is_empty(),
        "a field was added without a default, so every stored object of that kind written \
         before it is now unreadable — not at the next release, at the next restart. \
         Give the field `#[serde(default)]`; do not regenerate the corpus to make this \
         pass:\n  {}",
        broken.join("\n  ")
    );
}
