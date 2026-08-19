//! The gRPC surface, over exactly the handlers the REST gateway uses.
//!
//! Nothing here decides anything either. Each method converts a protobuf
//! message into the model shape [`crate::core::Api`] speaks, calls the same
//! function the REST path calls, and converts the answer back. Where the two
//! transports look different — a `PATCH` body carries only what changes, an
//! `Update` request carries the whole object — the *rule* is still the same
//! one, applied to the shape each transport has:
//!
//! * REST refuses a body that carries `status`.
//! * gRPC refuses an object whose `status` differs from what is stored.
//!
//! Both mean "you do not write that half", which is invariant 1, and neither
//! is a rule this file invented.

use std::{pin::Pin, time::Duration};

use futures::{Stream, StreamExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tonic::{Request, Response, Status, metadata::MetadataMap};
use velstra_cloud_model::{
    meta::{ResourceName, Revision},
    migration::{MigrationSpec, MigrationStatus},
    resources::{
        AttachmentSpec, AttachmentStatus, ImageSpec, ImageStatus, InstanceSpec, InstanceStatus,
        NetworkSpec, NetworkStatus, NodeSpec, NodeStatus, Operation, OperationSpec,
        OperationStatus, PortSpec, PortStatus, ProjectSpec, ProjectStatus, Resource, SnapshotSpec,
        SnapshotStatus, SubnetSpec, SubnetStatus, VolumeSpec, VolumeStatus,
    },
};
use velstra_cloud_proto::v1::{
    self, DeleteRequest, GetRequest, ListRequest, WaitOperationRequest, WatchRequest,
    admin_server::{Admin, AdminServer},
    compute_server::{Compute, ComputeServer},
    networking_server::{Networking, NetworkingServer},
    operations_server::{Operations, OperationsServer},
    storage_server::{Storage, StorageServer},
};

use crate::{
    auth::{Identity, identify},
    core::{Api, Filter, WatchEvent},
    error::{ApiError, ApiResult},
    paging::{PageToken, Paging},
};

/// An unset `page_size` and `page_token` is a caller who wants the collection,
/// not a caller who wants zero of it.
///
/// proto3 has no way to tell "absent" from "zero" on a scalar, and here the two
/// genuinely mean different things. `0` is read as *unset* — the whole
/// collection — which matches what the field says and keeps every client written
/// before paging existing working unchanged. A caller who wants a page asks for
/// a size; there is no way to ask for an empty one, and nothing would want to.
fn paging_for(page_size: i32, page_token: &str) -> ApiResult<Paging> {
    if page_size < 0 {
        return Err(ApiError::invalid("page_size cannot be negative"));
    }
    Ok(Paging {
        size: (page_size > 0).then_some(page_size as usize),
        token: match page_token {
            "" => None,
            raw => Some(PageToken::decode(raw)?),
        },
    })
}

/// An empty `node` and `pool` is not an agent called "": it is a caller who
/// wants the whole collection, which is what a console and an operator are
/// asking for.
fn filter_for(node: &str, pool: &str) -> ApiResult<Filter> {
    match (node.is_empty(), pool.is_empty()) {
        (true, true) => Ok(Filter::none()),
        (false, true) => Ok(Filter::for_node(node)),
        (true, false) => Ok(Filter::for_pool(pool)),
        (false, false) => Err(ApiError::invalid(
            "node and pool name two different kinds of agent; ask as one of them",
        )),
    }
}

/// A server-streaming answer. Boxed because every watch has the same shape and
/// naming ten stream types would say nothing.
pub type EventStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Clone)]
pub struct Service {
    api: Api,
}

impl Service {
    pub fn new(api: Api) -> Self {
        Self { api }
    }

    /// Who is calling. The same verifier the REST gateway uses, reading the
    /// same bearer token out of the transport's own header.
    async fn who<T>(&self, request: &Request<T>) -> ApiResult<Identity> {
        let header = authorization(request.metadata());
        identify(self.api.verifier(), header.as_deref()).await
    }

    async fn read<S, T>(&self, name: &str, who: &Identity) -> ApiResult<Resource<S, T>>
    where
        S: DeserializeOwned,
        T: DeserializeOwned,
    {
        let name = ResourceName::parse(name)?;
        let document = self.api.get(&name, who).await?;
        Ok(serde_json::from_value(document)?)
    }

    /// Refuse an update that would move `status`.
    ///
    /// A client that read an object and is sending it back carries the status
    /// it read, and that is not an attempt to write it — so only a *difference*
    /// is refused, and a message with no status at all says nothing and is
    /// left alone.
    async fn refuse_status_change<S, T>(&self, name: &ResourceName, sent: T) -> ApiResult<()>
    where
        S: DeserializeOwned,
        T: DeserializeOwned + Default + PartialEq + Serialize,
    {
        if sent == T::default() {
            return Ok(());
        }
        let stored: Resource<S, T> = self.api.typed(name).await?;
        if sent != stored.status {
            return Err(ApiError::invalid(
                "status is written by the agent that owns the object, and this update would change it",
            )
            .at("status"));
        }
        Ok(())
    }
}

fn authorization(metadata: &MetadataMap) -> Option<String> {
    metadata
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// An empty revision means the caller did not say which version it is
/// replacing, which is last-writer-wins — the same as an absent `If-Match`.
fn expect(revision: &str) -> ApiResult<Option<Revision>> {
    if revision.is_empty() {
        return Ok(None);
    }
    revision
        .parse::<u64>()
        .map(|r| Some(Revision(r)))
        .map_err(|_| ApiError::invalid("a revision is one this API handed out").at("revision"))
}

fn typed<S, T>(document: Value) -> ApiResult<Resource<S, T>>
where
    S: DeserializeOwned,
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(document)?)
}

/// Generate one service implementation.
///
/// The whole `impl` block is generated in one expansion rather than method by
/// method, because `#[tonic::async_trait]` has to see the finished methods to
/// desugar them. The point of the macro is not brevity — it is that all ten
/// collections get the *same* six methods, so a fix to how a create mints its
/// operation cannot land on nine of them.
macro_rules! service {
    (
        trait: $trait:path,
        kinds: [ $( {
            kind: $kind:literal,
            spec: $spec:ty,
            status: $status:ty,
            message: $message:ident,
            event: $event:ident,
            resource_field: $field:ident,
            get: $get:ident,
            list: $list:ident,
            list_response: $list_response:ident,
            list_field: $list_field:ident,
            create: $create:ident,
            create_request: $create_request:ident,
            id_field: $id_field:ident,
            update: $update:ident,
            update_request: $update_request:ident,
            delete: $delete:ident,
            watch: $watch:ident,
            stream: $stream:ident,
        } ),* $(,)? ],
        extra: { $($extra:tt)* }
    ) => {
        #[tonic::async_trait]
        impl $trait for Service {
            $(
                type $stream = EventStream<v1::$event>;

                async fn $get(&self, request: Request<GetRequest>) -> Result<Response<v1::$message>, Status> {
                    let who = self.who(&request).await?;
                    let resource: Resource<$spec, $status> = self.read(&request.into_inner().name, &who).await?;
                    Ok(Response::new(v1::$message::from(&resource)))
                }

                async fn $list(&self, request: Request<ListRequest>) -> Result<Response<v1::$list_response>, Status> {
                    let who = self.who(&request).await?;
                    let request = request.into_inner();
                    // A node agent asks for its own share; everything else asks
                    // for the collection. See `ListRequest.node` for why that
                    // distinction is what bounds a cell.
                    let listing = self
                        .api
                        .list_page_for(
                            &request.parent,
                            $kind,
                            &filter_for(&request.node, &request.pool)?,
                            &paging_for(request.page_size, &request.page_token)?,
                            &who,
                        )
                        .await?;
                    let items = listing
                        .items
                        .into_iter()
                        .map(|document| {
                            typed::<$spec, $status>(document).map(|r| v1::$message::from(&r))
                        })
                        .collect::<ApiResult<Vec<_>>>()?;
                    Ok(Response::new(v1::$list_response {
                        $list_field: items,
                        revision: listing.revision.to_string(),
                        next_page_token: listing.next_page_token.unwrap_or_default(),
                    }))
                }

                async fn $create(
                    &self,
                    request: Request<v1::$create_request>,
                ) -> Result<Response<v1::Operation>, Status> {
                    let who = self.who(&request).await?;
                    let request = request.into_inner();
                    let sent = request.$field.unwrap_or_default();
                    let spec = <$spec>::from(&sent.spec.unwrap_or_default());
                    let meta = sent.meta.unwrap_or_default();
                    let mut body = json!({ "id": request.$id_field, "spec": spec, "meta": { "labels": meta.labels } });
                    if !meta.name.is_empty() {
                        body["meta"]["name"] = json!(meta.name);
                    }
                    let created = self.api.create(&request.parent, $kind, &body, &who).await?;
                    let operation: Operation = typed(created.operation)?;
                    Ok(Response::new(v1::Operation::from(&operation)))
                }

                async fn $update(
                    &self,
                    request: Request<v1::$update_request>,
                ) -> Result<Response<v1::$message>, Status> {
                    let who = self.who(&request).await?;
                    let request = request.into_inner();
                    let sent = request
                        .$field
                        .ok_or_else(|| ApiError::invalid("an update carries the object to change"))?;
                    let meta = sent.meta.unwrap_or_default();
                    let name = ResourceName::parse(&meta.name).map_err(ApiError::from)?;
                    if let Some(status) = sent.status.as_ref() {
                        self.refuse_status_change::<$spec, $status>(&name, <$status>::from(status))
                            .await?;
                    }
                    let spec = <$spec>::from(&sent.spec.unwrap_or_default());
                    let body = json!({ "spec": spec, "meta": { "labels": meta.labels } });
                    let updated = self.api.patch(&name, &body, expect(&request.revision)?, &who).await?;
                    let resource: Resource<$spec, $status> = typed(updated)?;
                    Ok(Response::new(v1::$message::from(&resource)))
                }

                async fn $delete(&self, request: Request<DeleteRequest>) -> Result<Response<v1::$message>, Status> {
                    let who = self.who(&request).await?;
                    let request = request.into_inner();
                    let name = ResourceName::parse(&request.name).map_err(ApiError::from)?;
                    let deleted = self.api.delete(&name, expect(&request.revision)?, &who).await?;
                    let resource: Resource<$spec, $status> = typed(deleted.resource)?;
                    Ok(Response::new(v1::$message::from(&resource)))
                }

                async fn $watch(&self, request: Request<WatchRequest>) -> Result<Response<Self::$stream>, Status> {
                    // The answer was thrown away here, which authenticated the
                    // caller and authorised nothing: any accepted token could
                    // watch any project's objects, or the whole cell.
                    let who = self.who(&request).await?;
                    let request = request.into_inner();
                    let from = if request.from_revision.is_empty() {
                        None
                    } else {
                        expect(&request.from_revision)?
                    };
                    let stream = self
                        .api
                        .watch_for(&request.parent, $kind, from, filter_for(&request.node, &request.pool)?, &who)
                        .await?
                        .filter_map(|event| async move {
                        match event {
                            WatchEvent::Put(document) => {
                                let resource: Resource<$spec, $status> = typed(document).ok()?;
                                Some(Ok(v1::$event {
                                    r#type: v1::EventType::Put as i32,
                                    name: resource.meta.name.to_string(),
                                    revision: resource.meta.revision.to_string(),
                                    resource: Some(v1::$message::from(&resource)),
                                }))
                            }
                            WatchEvent::Delete { name, revision } => Some(Ok(v1::$event {
                                r#type: v1::EventType::Delete as i32,
                                name,
                                revision: revision.to_string(),
                                resource: None,
                            })),
                        }
                    });
                    Ok(Response::new(Box::pin(stream) as Self::$stream))
                }
            )*

            $($extra)*
        }
    };
}

service! {
    trait: Compute,
    kinds: [
        {
            kind: "instances", spec: InstanceSpec, status: InstanceStatus,
            message: Instance, event: InstanceEvent, resource_field: instance,
            get: get_instance, list: list_instances, list_response: ListInstancesResponse, list_field: instances,
            create: create_instance, create_request: CreateInstanceRequest, id_field: instance_id,
            update: update_instance, update_request: UpdateInstanceRequest,
            delete: delete_instance, watch: watch_instances, stream: WatchInstancesStream,
        },
        {
            kind: "migrations", spec: MigrationSpec, status: MigrationStatus,
            message: Migration, event: MigrationEvent, resource_field: migration,
            get: get_migration, list: list_migrations, list_response: ListMigrationsResponse,
            list_field: migrations,
            create: create_migration, create_request: CreateMigrationRequest, id_field: migration_id,
            update: update_migration, update_request: UpdateMigrationRequest,
            delete: delete_migration, watch: watch_migrations, stream: WatchMigrationsStream,
        },
        {
            kind: "images", spec: ImageSpec, status: ImageStatus,
            message: Image, event: ImageEvent, resource_field: image,
            get: get_image, list: list_images, list_response: ListImagesResponse, list_field: images,
            create: create_image, create_request: CreateImageRequest, id_field: image_id,
            update: update_image, update_request: UpdateImageRequest,
            delete: delete_image, watch: watch_images, stream: WatchImagesStream,
        },
    ],
    extra: {
        async fn explain_migration(
            &self,
            request: Request<GetRequest>,
        ) -> Result<Response<v1::MigrationExplanation>, Status> {
            let who = self.who(&request).await?;
            let name = ResourceName::parse(&request.into_inner().name).map_err(ApiError::from)?;
            let answer = self.api.explain_migration(&name, &who).await?;
            Ok(Response::new(v1::MigrationExplanation {
                from: answer["from"].as_str().unwrap_or_default().to_string(),
                destinations: answer["destinations"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|d| v1::Destination {
                                node: d["node"].as_str().unwrap_or_default().to_string(),
                                allowed: d["allowed"].as_bool().unwrap_or(false),
                                why: d["why"].as_str().unwrap_or_default().to_string(),
                                detail: d["detail"].as_str().unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }))
        }

        async fn explain_placement(
            &self,
            request: Request<GetRequest>,
        ) -> Result<Response<v1::PlacementExplanation>, Status> {
            let who = self.who(&request).await?;
            let name = ResourceName::parse(&request.into_inner().name).map_err(ApiError::from)?;
            let answer = self.api.explain_placement(&name, &who).await?;
            Ok(Response::new(v1::PlacementExplanation {
                placed: answer["placed"].as_str().map(str::to_string),
                rejected: answer["rejected"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|r| v1::Rejection {
                                node: r["node"].as_str().unwrap_or_default().to_string(),
                                why: r["why"].as_str().unwrap_or_default().to_string(),
                                detail: r["detail"].as_str().unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }))
        }
    }
}

service! {
    trait: Storage,
    kinds: [
        {
            kind: "volumes", spec: VolumeSpec, status: VolumeStatus,
            message: Volume, event: VolumeEvent, resource_field: volume,
            get: get_volume, list: list_volumes, list_response: ListVolumesResponse, list_field: volumes,
            create: create_volume, create_request: CreateVolumeRequest, id_field: volume_id,
            update: update_volume, update_request: UpdateVolumeRequest,
            delete: delete_volume, watch: watch_volumes, stream: WatchVolumesStream,
        },
        {
            kind: "snapshots", spec: SnapshotSpec, status: SnapshotStatus,
            message: Snapshot, event: SnapshotEvent, resource_field: snapshot,
            get: get_snapshot, list: list_snapshots, list_response: ListSnapshotsResponse,
            list_field: snapshots,
            create: create_snapshot, create_request: CreateSnapshotRequest, id_field: snapshot_id,
            update: update_snapshot, update_request: UpdateSnapshotRequest,
            delete: delete_snapshot, watch: watch_snapshots, stream: WatchSnapshotsStream,
        },
        {
            kind: "attachments", spec: AttachmentSpec, status: AttachmentStatus,
            message: Attachment, event: AttachmentEvent, resource_field: attachment,
            get: get_attachment, list: list_attachments, list_response: ListAttachmentsResponse,
            list_field: attachments,
            create: create_attachment, create_request: CreateAttachmentRequest, id_field: attachment_id,
            update: update_attachment, update_request: UpdateAttachmentRequest,
            delete: delete_attachment, watch: watch_attachments, stream: WatchAttachmentsStream,
        },
    ],
    extra: {}
}

service! {
    trait: Networking,
    kinds: [
        {
            kind: "networks", spec: NetworkSpec, status: NetworkStatus,
            message: Network, event: NetworkEvent, resource_field: network,
            get: get_network, list: list_networks, list_response: ListNetworksResponse, list_field: networks,
            create: create_network, create_request: CreateNetworkRequest, id_field: network_id,
            update: update_network, update_request: UpdateNetworkRequest,
            delete: delete_network, watch: watch_networks, stream: WatchNetworksStream,
        },
        {
            kind: "subnets", spec: SubnetSpec, status: SubnetStatus,
            message: Subnet, event: SubnetEvent, resource_field: subnet,
            get: get_subnet, list: list_subnets, list_response: ListSubnetsResponse, list_field: subnets,
            create: create_subnet, create_request: CreateSubnetRequest, id_field: subnet_id,
            update: update_subnet, update_request: UpdateSubnetRequest,
            delete: delete_subnet, watch: watch_subnets, stream: WatchSubnetsStream,
        },
        {
            kind: "ports", spec: PortSpec, status: PortStatus,
            message: Port, event: PortEvent, resource_field: port,
            get: get_port, list: list_ports, list_response: ListPortsResponse, list_field: ports,
            create: create_port, create_request: CreatePortRequest, id_field: port_id,
            update: update_port, update_request: UpdatePortRequest,
            delete: delete_port, watch: watch_ports, stream: WatchPortsStream,
        },
    ],
    extra: {}
}

service! {
    trait: Admin,
    kinds: [
        {
            kind: "projects", spec: ProjectSpec, status: ProjectStatus,
            message: Project, event: ProjectEvent, resource_field: project,
            get: get_project, list: list_projects, list_response: ListProjectsResponse, list_field: projects,
            create: create_project, create_request: CreateProjectRequest, id_field: project_id,
            update: update_project, update_request: UpdateProjectRequest,
            delete: delete_project, watch: watch_projects, stream: WatchProjectsStream,
        },
        {
            kind: "nodes", spec: NodeSpec, status: NodeStatus,
            message: Node, event: NodeEvent, resource_field: node,
            get: get_node, list: list_nodes, list_response: ListNodesResponse, list_field: nodes,
            create: create_node, create_request: CreateNodeRequest, id_field: node_id,
            update: update_node, update_request: UpdateNodeRequest,
            delete: delete_node, watch: watch_nodes, stream: WatchNodesStream,
        },
    ],
    extra: {}
}

/// Operations are read, not written: there is no create and no update, because
/// an operation says what is true of another object and has nothing of its own
/// to change.
#[tonic::async_trait]
impl Operations for Service {
    type WatchOperationsStream = EventStream<v1::OperationEvent>;

    async fn get_operation(
        &self,
        request: Request<GetRequest>,
    ) -> Result<Response<v1::Operation>, Status> {
        let who = self.who(&request).await?;
        let operation: Operation = self.read(&request.into_inner().name, &who).await?;
        Ok(Response::new(v1::Operation::from(&operation)))
    }

    async fn list_operations(
        &self,
        request: Request<ListRequest>,
    ) -> Result<Response<v1::ListOperationsResponse>, Status> {
        // Authorised per object, like every other collection. This used to take
        // the caller's identity, drop it on the floor and call the unfiltered
        // read — so any authenticated caller was handed every operation in the
        // cell, and an operation names the object it is about. That is a list of
        // every other tenant's resource names and what was recently done to
        // them. The REST surface has always gone through the authorised path;
        // only gRPC had this hole.
        let who = self.who(&request).await?;
        let request = request.into_inner();
        let listing = self
            .api
            .list_page_for(
                &request.parent,
                "operations",
                &Filter::none(),
                &paging_for(request.page_size, &request.page_token)?,
                &who,
            )
            .await?;
        let operations = listing
            .items
            .into_iter()
            .map(|document| {
                typed::<OperationSpec, OperationStatus>(document).map(|o| v1::Operation::from(&o))
            })
            .collect::<ApiResult<Vec<_>>>()?;
        Ok(Response::new(v1::ListOperationsResponse {
            operations,
            revision: listing.revision.to_string(),
            next_page_token: listing.next_page_token.unwrap_or_default(),
        }))
    }

    async fn delete_operation(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<v1::Operation>, Status> {
        let who = self.who(&request).await?;
        let request = request.into_inner();
        let name = ResourceName::parse(&request.name).map_err(ApiError::from)?;
        let deleted = self
            .api
            .delete(&name, expect(&request.revision)?, &who)
            .await?;
        let operation: Operation = typed(deleted.resource)?;
        Ok(Response::new(v1::Operation::from(&operation)))
    }

    async fn watch_operations(
        &self,
        request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchOperationsStream>, Status> {
        let who = self.who(&request).await?;
        let request = request.into_inner();
        let from = if request.from_revision.is_empty() {
            None
        } else {
            expect(&request.from_revision)?
        };
        let stream = self
            .api
            .watch_for(
                &request.parent,
                "operations",
                from,
                crate::core::Filter::none(),
                &who,
            )
            .await?
            .filter_map(|event| async move {
                match event {
                    WatchEvent::Put(document) => {
                        let operation: Operation = typed(document).ok()?;
                        Some(Ok(v1::OperationEvent {
                            r#type: v1::EventType::Put as i32,
                            name: operation.meta.name.to_string(),
                            revision: operation.meta.revision.to_string(),
                            resource: Some(v1::Operation::from(&operation)),
                        }))
                    }
                    WatchEvent::Delete { name, revision } => Some(Ok(v1::OperationEvent {
                        r#type: v1::EventType::Delete as i32,
                        name,
                        revision: revision.to_string(),
                        resource: None,
                    })),
                }
            });
        Ok(Response::new(
            Box::pin(stream) as Self::WatchOperationsStream
        ))
    }

    async fn wait_operation(
        &self,
        request: Request<WaitOperationRequest>,
    ) -> Result<Response<v1::Operation>, Status> {
        let who = self.who(&request).await?;
        let request = request.into_inner();
        let name = ResourceName::parse(&request.name).map_err(ApiError::from)?;
        let document = self
            .api
            .wait_operation(&name, Duration::from_millis(request.timeout_millis), &who)
            .await?;
        let operation: Operation = typed(document)?;
        Ok(Response::new(v1::Operation::from(&operation)))
    }
}

/// The five services, as an axum router, so both surfaces can share one port.
pub fn services(api: Api) -> axum::Router {
    let service = Service::new(api);
    tonic::service::Routes::new(ComputeServer::new(service.clone()))
        .add_service(StorageServer::new(service.clone()))
        .add_service(NetworkingServer::new(service.clone()))
        .add_service(AdminServer::new(service.clone()))
        .add_service(OperationsServer::new(service))
        .into_axum_router()
}

/// The gRPC clients, re-exported so a test — or an operator's script — can
/// talk to this server without depending on the proto crate directly.
pub mod client {
    pub use velstra_cloud_proto::v1::{
        admin_client::AdminClient, compute_client::ComputeClient,
        networking_client::NetworkingClient, operations_client::OperationsClient,
        storage_client::StorageClient,
    };
}
