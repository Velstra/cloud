//! Model in, protobuf out — and back.
//!
//! Two shapes describe the same objects and neither may drift: the model is
//! what the platform reasons about, the proto is what a customer's SDK sees.
//! Everything here is mechanical on purpose. The only judgement calls are
//! stated where they are made, and there are three of them:
//!
//! * A revision is a **string** on the wire. It is opaque, and a number invites
//!   a client to order or increment it — which is exactly the coupling the
//!   store's doc forbids.
//! * A proto message field is always optional on the wire, so a missing `meta`
//!   or `spec` becomes the default rather than an error. The API layer rejects
//!   what it actually needs; this layer never guesses.
//! * A proto enum's zero value means "unset", so it maps to the model's most
//!   honest value — `Unknown` for a condition, `Unknown` for an instance state
//!   — never to a plausible-looking one.

use velstra_cloud_model::{meta, migration, resources};

use crate::v1;

// ---- primitives -----------------------------------------------------------

fn name_of(name: &meta::ResourceName) -> String {
    name.to_string()
}

/// Parse a name, falling back to something unmistakably invalid rather than
/// panicking: a malformed name arrives from the network, and the API layer
/// turns the resulting rejection into an `INVALID_ARGUMENT`.
fn parse_name(s: &str) -> Result<meta::ResourceName, meta::NameError> {
    meta::ResourceName::parse(s)
}

fn revision_string(r: meta::Revision) -> String {
    r.0.to_string()
}

fn revision_from(s: &str) -> meta::Revision {
    meta::Revision(s.parse().unwrap_or(0))
}

fn millis(t: meta::Timestamp) -> i64 {
    t.0 as i64
}

fn timestamp(ms: i64) -> meta::Timestamp {
    meta::Timestamp(ms.max(0) as u64)
}

impl From<&meta::Placement> for v1::Placement {
    fn from(p: &meta::Placement) -> Self {
        Self {
            region: p.region.clone(),
            cell: p.cell.clone(),
        }
    }
}

impl From<&v1::Placement> for meta::Placement {
    fn from(p: &v1::Placement) -> Self {
        Self::new(p.region.clone(), p.cell.clone())
    }
}

impl From<&meta::Meta> for v1::Meta {
    fn from(m: &meta::Meta) -> Self {
        Self {
            name: name_of(&m.name),
            uid: m.uid.clone(),
            placement: Some((&m.placement).into()),
            generation: m.generation,
            created_at: millis(m.created_at),
            deleted_at: m.deleted_at.map(millis),
            finalizers: m.finalizers.clone(),
            labels: m
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            revision: revision_string(m.revision),
        }
    }
}

impl TryFrom<&v1::Meta> for meta::Meta {
    type Error = meta::NameError;

    fn try_from(m: &v1::Meta) -> Result<Self, Self::Error> {
        let placement = m
            .placement
            .as_ref()
            .map(meta::Placement::from)
            .unwrap_or_else(|| meta::Placement::new(String::new(), String::new()));
        Ok(Self {
            name: parse_name(&m.name)?,
            uid: m.uid.clone(),
            placement,
            generation: m.generation,
            created_at: timestamp(m.created_at),
            deleted_at: m.deleted_at.map(timestamp),
            finalizers: m.finalizers.clone(),
            labels: m
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            revision: revision_from(&m.revision),
        })
    }
}

impl From<meta::ConditionStatus> for v1::ConditionStatus {
    fn from(s: meta::ConditionStatus) -> Self {
        match s {
            meta::ConditionStatus::True => Self::True,
            meta::ConditionStatus::False => Self::False,
            meta::ConditionStatus::Unknown => Self::Unknown,
        }
    }
}

impl From<v1::ConditionStatus> for meta::ConditionStatus {
    fn from(s: v1::ConditionStatus) -> Self {
        match s {
            v1::ConditionStatus::True => Self::True,
            v1::ConditionStatus::False => Self::False,
            // Unspecified is not `False`. A message that left the field out has
            // said nothing about the world, and "not yet known" is the only
            // reading of that which cannot be wrong.
            _ => Self::Unknown,
        }
    }
}

impl From<&meta::Condition> for v1::Condition {
    fn from(c: &meta::Condition) -> Self {
        Self {
            kind: c.kind.clone(),
            status: v1::ConditionStatus::from(c.status) as i32,
            reason: c.reason.clone(),
            message: c.message.clone(),
            observed_generation: c.observed_generation,
            last_transition: millis(c.last_transition),
        }
    }
}

impl From<&v1::Condition> for meta::Condition {
    fn from(c: &v1::Condition) -> Self {
        Self {
            kind: c.kind.clone(),
            status: c.status().into(),
            reason: c.reason.clone(),
            message: c.message.clone(),
            observed_generation: c.observed_generation,
            last_transition: timestamp(c.last_transition),
        }
    }
}

fn conditions_out(cs: &[meta::Condition]) -> Vec<v1::Condition> {
    cs.iter().map(Into::into).collect()
}

fn conditions_in(cs: &[v1::Condition]) -> Vec<meta::Condition> {
    cs.iter().map(Into::into).collect()
}

// ---- project --------------------------------------------------------------

impl From<&resources::Quota> for v1::Quota {
    fn from(q: &resources::Quota) -> Self {
        Self {
            instances: q.instances,
            vcpus: q.vcpus,
            memory_mib: q.memory_mib,
            volume_gib: q.volume_gib,
        }
    }
}

impl From<&v1::Quota> for resources::Quota {
    fn from(q: &v1::Quota) -> Self {
        Self {
            instances: q.instances,
            vcpus: q.vcpus,
            memory_mib: q.memory_mib,
            volume_gib: q.volume_gib,
        }
    }
}

impl From<&resources::ProjectSpec> for v1::ProjectSpec {
    fn from(s: &resources::ProjectSpec) -> Self {
        Self {
            display_name: s.display_name.clone(),
            parent: s.parent.clone(),
            quota: Some((&s.quota).into()),
            cell: s.cell.clone(),
        }
    }
}

impl From<&v1::ProjectSpec> for resources::ProjectSpec {
    fn from(s: &v1::ProjectSpec) -> Self {
        Self {
            display_name: s.display_name.clone(),
            parent: s.parent.clone(),
            quota: s.quota.as_ref().map(Into::into).unwrap_or_default(),
            cell: s.cell.clone(),
            // The protobuf surface does not describe bindings yet, so a project
            // that arrives over gRPC carries none. Empty is the safe direction:
            // it grants nothing rather than inventing a grant nobody asked for.
            // Managing them over gRPC needs a message, and until there is one,
            // REST is where a policy is written.
            bindings: Vec::new(),
        }
    }
}

impl From<&resources::ProjectStatus> for v1::ProjectStatus {
    fn from(s: &resources::ProjectStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            used: Some((&s.used).into()),
        }
    }
}

impl From<&v1::ProjectStatus> for resources::ProjectStatus {
    fn from(s: &v1::ProjectStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            used: s.used.as_ref().map(Into::into).unwrap_or_default(),
        }
    }
}

// ---- node -----------------------------------------------------------------

impl From<&resources::Capacity> for v1::Capacity {
    fn from(c: &resources::Capacity) -> Self {
        Self {
            vcpus: c.vcpus,
            memory_mib: c.memory_mib,
            disk_gib: c.disk_gib,
            numa_free_mib: c.numa_free_mib.clone(),
            hugepages_1gi: c.hugepages_1gi,
        }
    }
}

impl From<&v1::Capacity> for resources::Capacity {
    fn from(c: &v1::Capacity) -> Self {
        Self {
            vcpus: c.vcpus,
            memory_mib: c.memory_mib,
            disk_gib: c.disk_gib,
            numa_free_mib: c.numa_free_mib.clone(),
            hugepages_1gi: c.hugepages_1gi,
        }
    }
}

impl From<&resources::NodeSpec> for v1::NodeSpec {
    fn from(s: &resources::NodeSpec) -> Self {
        Self {
            schedulable: s.schedulable,
            labels: s.labels.clone(),
        }
    }
}

impl From<&v1::NodeSpec> for resources::NodeSpec {
    fn from(s: &v1::NodeSpec) -> Self {
        Self {
            schedulable: s.schedulable,
            labels: s.labels.clone(),
        }
    }
}

impl From<&resources::NodeStatus> for v1::NodeStatus {
    fn from(s: &resources::NodeStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            capacity: Some((&s.capacity).into()),
            allocated: Some((&s.allocated).into()),
            agent_version: s.agent_version.clone(),
            last_heartbeat: millis(s.last_heartbeat),
            images: s.images.clone(),
        }
    }
}

impl From<&v1::NodeStatus> for resources::NodeStatus {
    fn from(s: &v1::NodeStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            capacity: s.capacity.as_ref().map(Into::into).unwrap_or_default(),
            allocated: s.allocated.as_ref().map(Into::into).unwrap_or_default(),
            agent_version: s.agent_version.clone(),
            last_heartbeat: timestamp(s.last_heartbeat),
            images: s.images.clone(),
        }
    }
}

// ---- image ----------------------------------------------------------------

impl From<resources::ImageFormat> for v1::ImageFormat {
    fn from(f: resources::ImageFormat) -> Self {
        match f {
            resources::ImageFormat::Raw => Self::Raw,
            resources::ImageFormat::Qcow2 => Self::Qcow2,
        }
    }
}

impl From<v1::ImageFormat> for resources::ImageFormat {
    fn from(f: v1::ImageFormat) -> Self {
        match f {
            v1::ImageFormat::Qcow2 => Self::Qcow2,
            _ => Self::Raw,
        }
    }
}

impl From<&resources::ImageSpec> for v1::ImageSpec {
    fn from(s: &resources::ImageSpec) -> Self {
        Self {
            digest: s.digest.clone(),
            format: v1::ImageFormat::from(s.format) as i32,
            size_bytes: s.size_bytes,
            source_url: s.source_url.clone(),
            signature: s.signature.clone(),
        }
    }
}

impl From<&v1::ImageSpec> for resources::ImageSpec {
    fn from(s: &v1::ImageSpec) -> Self {
        Self {
            digest: s.digest.clone(),
            format: s.format().into(),
            size_bytes: s.size_bytes,
            source_url: s.source_url.clone(),
            signature: s.signature.clone(),
        }
    }
}

impl From<&resources::ImageStatus> for v1::ImageStatus {
    fn from(s: &resources::ImageStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
        }
    }
}

impl From<&v1::ImageStatus> for resources::ImageStatus {
    fn from(s: &v1::ImageStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
        }
    }
}

// ---- instance -------------------------------------------------------------

impl From<resources::DesiredState> for v1::DesiredState {
    fn from(d: resources::DesiredState) -> Self {
        match d {
            resources::DesiredState::Running => Self::Running,
            resources::DesiredState::Stopped => Self::Stopped,
        }
    }
}

impl From<v1::DesiredState> for resources::DesiredState {
    fn from(d: v1::DesiredState) -> Self {
        match d {
            v1::DesiredState::Stopped => Self::Stopped,
            // Unspecified means the caller did not say, and the model's default
            // for a machine somebody asked to exist is that it runs.
            _ => Self::Running,
        }
    }
}

impl From<resources::InstanceState> for v1::InstanceState {
    fn from(s: resources::InstanceState) -> Self {
        match s {
            resources::InstanceState::Unknown => Self::Unspecified,
            resources::InstanceState::Stopped => Self::Stopped,
            resources::InstanceState::Running => Self::Running,
            resources::InstanceState::Failed => Self::Failed,
        }
    }
}

impl From<v1::InstanceState> for resources::InstanceState {
    fn from(s: v1::InstanceState) -> Self {
        match s {
            v1::InstanceState::Stopped => Self::Stopped,
            v1::InstanceState::Running => Self::Running,
            v1::InstanceState::Failed => Self::Failed,
            v1::InstanceState::Unspecified => Self::Unknown,
        }
    }
}

impl From<&resources::PlacementPolicy> for v1::PlacementPolicy {
    fn from(p: &resources::PlacementPolicy) -> Self {
        Self {
            anti_affinity_group: p.anti_affinity_group.clone(),
            required_labels: p.required_labels.clone(),
        }
    }
}

impl From<&v1::PlacementPolicy> for resources::PlacementPolicy {
    fn from(p: &v1::PlacementPolicy) -> Self {
        Self {
            anti_affinity_group: p.anti_affinity_group.clone(),
            required_labels: p.required_labels.clone(),
        }
    }
}

impl From<&resources::InstanceSpec> for v1::InstanceSpec {
    fn from(s: &resources::InstanceSpec) -> Self {
        Self {
            vcpus: s.vcpus,
            memory_mib: s.memory_mib,
            image: s.image.clone(),
            root_disk_gib: s.root_disk_gib,
            desired_state: v1::DesiredState::from(s.desired_state) as i32,
            ports: s.ports.clone(),
            ssh_keys: s.ssh_keys.clone(),
            user_data: s.user_data.clone(),
            node: s.node.clone(),
            placement_policy: Some((&s.placement_policy).into()),
        }
    }
}

impl From<&v1::InstanceSpec> for resources::InstanceSpec {
    fn from(s: &v1::InstanceSpec) -> Self {
        Self {
            vcpus: s.vcpus,
            memory_mib: s.memory_mib,
            image: s.image.clone(),
            root_disk_gib: s.root_disk_gib,
            desired_state: s.desired_state().into(),
            ports: s.ports.clone(),
            ssh_keys: s.ssh_keys.clone(),
            user_data: s.user_data.clone(),
            node: s.node.clone(),
            placement_policy: s
                .placement_policy
                .as_ref()
                .map(Into::into)
                .unwrap_or_default(),
        }
    }
}

impl From<&resources::InstanceStatus> for v1::InstanceStatus {
    fn from(s: &resources::InstanceStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            state: v1::InstanceState::from(s.state) as i32,
            node: s.node.clone(),
            addresses: s.addresses.clone(),
            vmm_pid: s.vmm_pid,
            started_at: s.started_at.map(millis),
        }
    }
}

impl From<&v1::InstanceStatus> for resources::InstanceStatus {
    fn from(s: &v1::InstanceStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            state: s.state().into(),
            node: s.node.clone(),
            addresses: s.addresses.clone(),
            vmm_pid: s.vmm_pid,
            started_at: s.started_at.map(timestamp),
        }
    }
}

// ---- volume and attachment ------------------------------------------------

impl From<&resources::VolumeSpec> for v1::VolumeSpec {
    fn from(s: &resources::VolumeSpec) -> Self {
        Self {
            size_gib: s.size_gib,
            pool: s.pool.clone(),
            encryption_key: s.encryption_key.clone(),
            source_image: s.source_image.clone(),
            source_snapshot: s.source_snapshot.clone(),
        }
    }
}

impl From<&v1::VolumeSpec> for resources::VolumeSpec {
    fn from(s: &v1::VolumeSpec) -> Self {
        Self {
            size_gib: s.size_gib,
            pool: s.pool.clone(),
            encryption_key: s.encryption_key.clone(),
            source_image: s.source_image.clone(),
            source_snapshot: s.source_snapshot.clone(),
        }
    }
}

impl From<&resources::SnapshotSpec> for v1::SnapshotSpec {
    fn from(s: &resources::SnapshotSpec) -> Self {
        Self {
            pool: s.pool.clone(),
        }
    }
}

impl From<&v1::SnapshotSpec> for resources::SnapshotSpec {
    fn from(s: &v1::SnapshotSpec) -> Self {
        Self {
            pool: s.pool.clone(),
        }
    }
}

impl From<&resources::SnapshotStatus> for v1::SnapshotStatus {
    fn from(s: &resources::SnapshotStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            pool: s.pool.clone(),
            taken: s.taken,
            size_gib: s.size_gib,
            taken_at: s.taken_at.map(millis),
        }
    }
}

impl From<&v1::SnapshotStatus> for resources::SnapshotStatus {
    fn from(s: &v1::SnapshotStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            pool: s.pool.clone(),
            taken: s.taken,
            size_gib: s.size_gib,
            taken_at: s.taken_at.map(timestamp),
        }
    }
}

impl From<&resources::VolumeStatus> for v1::VolumeStatus {
    fn from(s: &resources::VolumeStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            provisioned: s.provisioned,
            actual_size_gib: s.actual_size_gib,
            pool: s.pool.clone(),
        }
    }
}

impl From<&v1::VolumeStatus> for resources::VolumeStatus {
    fn from(s: &v1::VolumeStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            provisioned: s.provisioned,
            actual_size_gib: s.actual_size_gib,
            pool: s.pool.clone(),
        }
    }
}

impl From<&resources::AttachmentSpec> for v1::AttachmentSpec {
    fn from(s: &resources::AttachmentSpec) -> Self {
        Self {
            volume: s.volume.clone(),
            instance: s.instance.clone(),
            node: s.node.clone(),
            read_only: s.read_only,
        }
    }
}

impl From<&v1::AttachmentSpec> for resources::AttachmentSpec {
    fn from(s: &v1::AttachmentSpec) -> Self {
        Self {
            volume: s.volume.clone(),
            instance: s.instance.clone(),
            node: s.node.clone(),
            read_only: s.read_only,
        }
    }
}

impl From<&resources::AttachmentStatus> for v1::AttachmentStatus {
    fn from(s: &resources::AttachmentStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            attached: s.attached,
            device: s.device.clone(),
            node: s.node.clone(),
        }
    }
}

impl From<&v1::AttachmentStatus> for resources::AttachmentStatus {
    fn from(s: &v1::AttachmentStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            attached: s.attached,
            device: s.device.clone(),
            node: s.node.clone(),
        }
    }
}

// ---- network, subnet, port ------------------------------------------------

impl From<&resources::NetworkSpec> for v1::NetworkSpec {
    fn from(s: &resources::NetworkSpec) -> Self {
        Self {
            vni: s.vni,
            mtu: s.mtu,
        }
    }
}

impl From<&v1::NetworkSpec> for resources::NetworkSpec {
    fn from(s: &v1::NetworkSpec) -> Self {
        Self {
            vni: s.vni,
            mtu: s.mtu,
        }
    }
}

impl From<&resources::NetworkStatus> for v1::NetworkStatus {
    fn from(s: &resources::NetworkStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
        }
    }
}

impl From<&v1::NetworkStatus> for resources::NetworkStatus {
    fn from(s: &v1::NetworkStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
        }
    }
}

impl From<&resources::RouterSpec> for v1::RouterSpec {
    fn from(s: &resources::RouterSpec) -> Self {
        Self {
            networks: s.networks.clone(),
        }
    }
}

impl From<&v1::RouterSpec> for resources::RouterSpec {
    fn from(s: &v1::RouterSpec) -> Self {
        Self {
            networks: s.networks.clone(),
        }
    }
}

impl From<&resources::RouterStatus> for v1::RouterStatus {
    fn from(s: &resources::RouterStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            l3_vni: s.l3_vni,
            gateway_mac: s.gateway_mac.clone(),
        }
    }
}

impl From<&v1::RouterStatus> for resources::RouterStatus {
    fn from(s: &v1::RouterStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            l3_vni: s.l3_vni,
            gateway_mac: s.gateway_mac.clone(),
        }
    }
}

impl From<&resources::FloatingIpSpec> for v1::FloatingIpSpec {
    fn from(s: &resources::FloatingIpSpec) -> Self {
        Self {
            subnet: s.subnet.clone(),
            // Absent and empty are the same thing on the wire — "nobody has
            // decided yet" — because proto3 has no third state and inventing one
            // would mean two ways to say it.
            address: s.address.clone().unwrap_or_default(),
            port: s.port.clone(),
        }
    }
}

impl From<&v1::FloatingIpSpec> for resources::FloatingIpSpec {
    fn from(s: &v1::FloatingIpSpec) -> Self {
        Self {
            subnet: s.subnet.clone(),
            address: (!s.address.is_empty()).then(|| s.address.clone()),
            port: s.port.clone(),
        }
    }
}

impl From<&resources::FloatingIpStatus> for v1::FloatingIpStatus {
    fn from(s: &resources::FloatingIpStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            fabric_id: s.fabric_id.clone(),
            associated: s.associated.clone(),
        }
    }
}

impl From<&v1::FloatingIpStatus> for resources::FloatingIpStatus {
    fn from(s: &v1::FloatingIpStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            fabric_id: s.fabric_id.clone(),
            associated: s.associated.clone(),
        }
    }
}

impl From<&resources::SubnetSpec> for v1::SubnetSpec {
    fn from(s: &resources::SubnetSpec) -> Self {
        Self {
            network: s.network.clone(),
            cidr: s.cidr.clone(),
            gateway: s.gateway.clone(),
            dns: s.dns.clone(),
            reserved: s.reserved.clone(),
        }
    }
}

impl From<&v1::SubnetSpec> for resources::SubnetSpec {
    fn from(s: &v1::SubnetSpec) -> Self {
        Self {
            network: s.network.clone(),
            cidr: s.cidr.clone(),
            gateway: s.gateway.clone(),
            dns: s.dns.clone(),
            reserved: s.reserved.clone(),
        }
    }
}

impl From<&resources::SubnetStatus> for v1::SubnetStatus {
    fn from(s: &resources::SubnetStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            allocated: s.allocated,
            available: s.available,
        }
    }
}

impl From<&v1::SubnetStatus> for resources::SubnetStatus {
    fn from(s: &v1::SubnetStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            allocated: s.allocated,
            available: s.available,
        }
    }
}

impl From<&resources::PortSpec> for v1::PortSpec {
    fn from(s: &resources::PortSpec) -> Self {
        Self {
            network: s.network.clone(),
            subnet: s.subnet.clone(),
            address: s.address.clone(),
            mac: s.mac.clone(),
            security_groups: s.security_groups.clone(),
            rate_limit_mbit: s.rate_limit_mbit,
            node: s.node.clone(),
        }
    }
}

impl From<&v1::PortSpec> for resources::PortSpec {
    fn from(s: &v1::PortSpec) -> Self {
        Self {
            network: s.network.clone(),
            subnet: s.subnet.clone(),
            address: s.address.clone(),
            mac: s.mac.clone(),
            security_groups: s.security_groups.clone(),
            rate_limit_mbit: s.rate_limit_mbit,
            node: s.node.clone(),
        }
    }
}

impl From<&resources::PortStatus> for v1::PortStatus {
    fn from(s: &resources::PortStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            node: s.node.clone(),
            programmed: s.programmed,
            tap_device: s.tap_device.clone(),
        }
    }
}

impl From<&v1::PortStatus> for resources::PortStatus {
    fn from(s: &v1::PortStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            node: s.node.clone(),
            programmed: s.programmed,
            tap_device: s.tap_device.clone(),
        }
    }
}

// ---- operation ------------------------------------------------------------

impl From<&resources::OperationSpec> for v1::OperationSpec {
    fn from(s: &resources::OperationSpec) -> Self {
        Self {
            target: s.target.clone(),
            target_generation: s.target_generation,
            verb: s.verb.clone(),
            requested_by: s.requested_by.clone(),
        }
    }
}

impl From<&v1::OperationSpec> for resources::OperationSpec {
    fn from(s: &v1::OperationSpec) -> Self {
        Self {
            target: s.target.clone(),
            target_generation: s.target_generation,
            verb: s.verb.clone(),
            requested_by: s.requested_by.clone(),
        }
    }
}

impl From<&resources::OperationStatus> for v1::OperationStatus {
    fn from(s: &resources::OperationStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            done: s.done,
            error: s.error.clone(),
            finished_at: s.finished_at.map(millis),
        }
    }
}

impl From<&v1::OperationStatus> for resources::OperationStatus {
    fn from(s: &v1::OperationStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            done: s.done,
            error: s.error.clone(),
            finished_at: s.finished_at.map(timestamp),
        }
    }
}

// ---- migration ------------------------------------------------------------

impl From<migration::MigrationMode> for v1::MigrationMode {
    fn from(m: migration::MigrationMode) -> Self {
        match m {
            migration::MigrationMode::Live => Self::Live,
            migration::MigrationMode::PostCopy => Self::PostCopy,
            migration::MigrationMode::Reboot => Self::Reboot,
        }
    }
}

impl From<v1::MigrationMode> for migration::MigrationMode {
    fn from(m: v1::MigrationMode) -> Self {
        match m {
            v1::MigrationMode::PostCopy => Self::PostCopy,
            v1::MigrationMode::Reboot => Self::Reboot,
            // Unspecified is pre-copy, which is the mode where a failure costs
            // nothing. A caller who did not choose must not be given the one
            // that can lose the guest.
            _ => Self::Live,
        }
    }
}

impl From<&migration::MigrationSpec> for v1::MigrationSpec {
    fn from(s: &migration::MigrationSpec) -> Self {
        Self {
            instance: s.instance.clone(),
            from_node: s.from_node.clone(),
            to_node: s.to_node.clone(),
            mode: v1::MigrationMode::from(s.mode) as i32,
            downtime_ms: s.downtime_ms,
            timeout_s: s.timeout_s,
            connections: s.connections as u32,
        }
    }
}

impl From<&v1::MigrationSpec> for migration::MigrationSpec {
    fn from(s: &v1::MigrationSpec) -> Self {
        let defaults = migration::MigrationSpec::default();
        Self {
            instance: s.instance.clone(),
            from_node: s.from_node.clone(),
            to_node: s.to_node.clone(),
            mode: s.mode().into(),
            // A zero here is a field the caller left out, not a request for a
            // guest that may never pause and a transfer that may never end.
            downtime_ms: if s.downtime_ms == 0 {
                defaults.downtime_ms
            } else {
                s.downtime_ms
            },
            timeout_s: if s.timeout_s == 0 {
                defaults.timeout_s
            } else {
                s.timeout_s
            },
            connections: if s.connections == 0 {
                defaults.connections
            } else {
                s.connections.min(u8::MAX as u32) as u8
            },
        }
    }
}

impl From<&migration::MigrationStatus> for v1::MigrationStatus {
    fn from(s: &migration::MigrationStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_out(&s.conditions),
            node: s.node.clone(),
            receiver_url: s.receiver_url.clone(),
            receiver_ready: s.receiver_ready,
            transferred_mib: s.transferred_mib,
        }
    }
}

impl From<&v1::MigrationStatus> for migration::MigrationStatus {
    fn from(s: &v1::MigrationStatus) -> Self {
        Self {
            observed_generation: s.observed_generation,
            conditions: conditions_in(&s.conditions),
            node: s.node.clone(),
            receiver_url: s.receiver_url.clone(),
            receiver_ready: s.receiver_ready,
            transferred_mib: s.transferred_mib,
        }
    }
}

/// A refusal, as a stable token plus the numbers behind it — the same split as
/// a placement rejection, because a console branches on one and shows the other.
pub fn destination_of(node: &str, refusal: Option<&migration::Refusal>) -> v1::Destination {
    use migration::Refusal;
    let Some(refusal) = refusal else {
        return v1::Destination {
            node: node.to_string(),
            allowed: true,
            why: String::new(),
            detail: String::new(),
        };
    };
    let why = match refusal {
        Refusal::AlreadyThere { .. } => "AlreadyThere",
        Refusal::NotRunning { .. } => "NotRunning",
        Refusal::NotFromThere { .. } => "NotFromThere",
        Refusal::DestinationDraining { .. } => "DestinationDraining",
        Refusal::DestinationTooSmall { .. } => "DestinationTooSmall",
        Refusal::VersionsTooFarApart { .. } => "VersionsTooFarApart",
        Refusal::DestinationLacksImage { .. } => "DestinationLacksImage",
    };
    v1::Destination {
        node: node.to_string(),
        allowed: false,
        why: why.to_string(),
        // The model already writes each of these as a sentence for a person.
        detail: refusal.to_string(),
    }
}

// ---- the whole objects ----------------------------------------------------

/// Wire a resource type to its three protobuf halves, and generate the
/// object-level conversions from them.
///
/// One macro rather than ten hand-written pairs, because an object is only ever
/// `meta` + `spec` + `status`: writing it out ten times would be ten chances to
/// forget the `meta`.
macro_rules! resource_conversions {
    ($model:path, $proto:ident) => {
        impl From<&$model> for v1::$proto {
            fn from(r: &$model) -> Self {
                Self {
                    meta: Some((&r.meta).into()),
                    spec: Some((&r.spec).into()),
                    status: Some((&r.status).into()),
                }
            }
        }

        impl TryFrom<&v1::$proto> for $model {
            type Error = meta::NameError;

            fn try_from(r: &v1::$proto) -> Result<Self, Self::Error> {
                let m = r.meta.as_ref().ok_or_else(|| {
                    meta::NameError::NotPairs("a resource arrived without meta".into())
                })?;
                Ok(Self {
                    meta: meta::Meta::try_from(m)?,
                    spec: r.spec.as_ref().map(Into::into).unwrap_or_default(),
                    status: r.status.as_ref().map(Into::into).unwrap_or_default(),
                })
            }
        }
    };
}

resource_conversions!(resources::Project, Project);
resource_conversions!(resources::Node, Node);
resource_conversions!(resources::Image, Image);
resource_conversions!(resources::Instance, Instance);
resource_conversions!(resources::Volume, Volume);
resource_conversions!(resources::Snapshot, Snapshot);
resource_conversions!(resources::Attachment, Attachment);
resource_conversions!(resources::Network, Network);
resource_conversions!(resources::Router, Router);
resource_conversions!(resources::FloatingIp, FloatingIp);
resource_conversions!(resources::Subnet, Subnet);
resource_conversions!(resources::Port, Port);
resource_conversions!(resources::Operation, Operation);
resource_conversions!(migration::Migration, Migration);

// ---- placement ------------------------------------------------------------

/// A rejection, as a stable token plus the numbers behind it.
///
/// The token is what a console branches on and the detail is what a person
/// reads; keeping them apart here means neither is ever parsed out of the
/// other.
impl From<&velstra_cloud_model::reconcile::Explanation> for v1::Rejection {
    fn from(e: &velstra_cloud_model::reconcile::Explanation) -> Self {
        use velstra_cloud_model::reconcile::Rejected;
        let (why, detail) = match &e.why {
            Rejected::Unschedulable => ("Unschedulable", "the node is draining".to_string()),
            Rejected::NotReady => (
                "NotReady",
                "the node has not reported itself ready".to_string(),
            ),
            Rejected::InsufficientVcpus { free, want } => {
                ("InsufficientVcpus", format!("{free} free, {want} wanted"))
            }
            Rejected::InsufficientMemory { free_mib, want_mib } => (
                "InsufficientMemory",
                format!("{free_mib} free, {want_mib} wanted"),
            ),
            Rejected::NoNumaNodeFits { want_mib } => {
                ("NoNumaNodeFits", format!("{want_mib} wanted"))
            }
            Rejected::MissingLabel { label } => ("MissingLabel", format!("{label} wanted")),
            Rejected::AntiAffinity { group } => (
                "AntiAffinity",
                format!("another member of {group} is already here"),
            ),
        };
        Self {
            node: e.node.clone(),
            why: why.to_string(),
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Condition, ConditionStatus, Meta, Placement, ResourceName},
        resources::{InstanceSpec, InstanceState, InstanceStatus, Resource},
    };

    use super::*;

    #[test]
    fn an_instance_survives_the_round_trip_unchanged() {
        // The whole point of one proto: what a customer's SDK sends is what the
        // platform reasons about, field for field. A conversion that drops a
        // field is a field that silently resets on every update.
        let mut meta = Meta::new(
            ResourceName::parse("projects/p1/instances/i1").unwrap(),
            Placement::new("eu-central", "cell-1"),
        );
        meta.generation = 7;
        meta.revision = velstra_cloud_model::meta::Revision(412);
        meta.finalizers = vec!["node.velstra.io/release".into()];
        meta.labels.insert("tier".into(), "web".into());
        meta.deleted_at = Some(velstra_cloud_model::meta::Timestamp(1786732800000));

        let original = Resource::new(
            meta,
            InstanceSpec {
                vcpus: 4,
                memory_mib: 8192,
                image: "projects/p1/images/sha256-abc".into(),
                root_disk_gib: 40,
                desired_state: resources::DesiredState::Stopped,
                ports: vec!["projects/p1/ports/port-a".into()],
                ssh_keys: vec!["ssh-ed25519 AAAA".into()],
                user_data: Some("#cloud-config".into()),
                node: Some("node-a".into()),
                placement_policy: resources::PlacementPolicy {
                    anti_affinity_group: Some("web".into()),
                    required_labels: vec!["ssd".into()],
                },
            },
            InstanceStatus {
                observed_generation: 6,
                conditions: vec![Condition::new(
                    "Ready",
                    ConditionStatus::False,
                    "VmFailed",
                    "the virtual machine exited",
                    6,
                )],
                state: InstanceState::Failed,
                node: Some("node-a".into()),
                addresses: vec!["10.0.0.5".into()],
                vmm_pid: Some(4242),
                started_at: Some(velstra_cloud_model::meta::Timestamp(1786732801000)),
            },
        );

        let wire = v1::Instance::from(&original);
        let back = resources::Instance::try_from(&wire).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn a_revision_crosses_the_wire_as_an_opaque_string() {
        // A number invites a client to order it or add one to it, and the store
        // documents that as the thing that makes the backend unswappable.
        let meta = {
            let mut m = Meta::new(
                ResourceName::parse("projects/p1/instances/i1").unwrap(),
                Placement::new("eu", "cell-1"),
            );
            m.revision = velstra_cloud_model::meta::Revision(412);
            m
        };
        let wire = v1::Meta::from(&meta);
        assert_eq!(wire.revision, "412");
        assert_eq!(Meta::try_from(&wire).unwrap().revision.0, 412);
    }

    #[test]
    fn an_unset_state_is_unknown_rather_than_a_plausible_guess() {
        // A client that left `state` out has said nothing about the world. The
        // one reading of that which cannot be wrong is "nobody has looked".
        let model = resources::InstanceStatus::from(&v1::InstanceStatus::default());
        assert_eq!(model.state, InstanceState::Unknown);
        let condition = meta::Condition::from(&v1::Condition::default());
        assert_eq!(condition.status, ConditionStatus::Unknown);
    }
}
