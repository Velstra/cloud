//! Reading the cell through the API, filtered to this node.
//!
//! The other half of [`crate::cell`]. Where [`crate::cell::StoreCell`] reads the
//! store and gets the whole cell, this asks the API for `?node=<me>` and gets
//! this node's share — and the API serves every node from one watch per
//! collection rather than one per node.
//!
//! ## Why REST and not gRPC
//!
//! Both are the same handlers underneath. REST covers **every** collection,
//! including security groups, which the protobuf surface does not describe at
//! all — and a node needs those, because a group's membership is what lets it
//! expand a rule that names another group without reading every port in the
//! cell. Using the transport that covers the whole contract beat adding six
//! messages to a schema to avoid an HTTP client.
//!
//! ## What a wake-up is
//!
//! Server-sent events, one stream per assigned collection, and the body is
//! thrown away. A pass is level-triggered and re-reads what this node owns, so
//! the only thing a stream has to carry is "look again" — which is what makes it
//! safe for it to be an unreliable channel rather than a protocol, and why a
//! dropped stream costs latency until the next resync and nothing else.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use http_body_util::BodyExt;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;
use velstra_cloud_model::{
    migration::Migration,
    resources::{Attachment, Instance, Network, Port, Subnet},
    security::SecurityGroup,
};

use crate::{
    cell::CellReader,
    host::{HostError, Result},
};

/// How long to wait for one read. Long enough for a busy API, short enough that
/// a wedged one becomes a counted failure and a retried pass rather than an
/// agent that has stopped.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ApiCell {
    /// `http://host:port`, no trailing slash.
    base: String,
    /// `host:port`, for the connection and the `Host` header.
    authority: String,
    token: String,
    /// How this agent names itself to the API, as the query parameter it sends:
    /// `node=node-a` or `pool=pool-a`. One field rather than two, because an
    /// agent is one or the other and the API refuses both at once.
    who: String,
    /// Every object this node reads lives under one project today. Kept as a
    /// field because the day it does not, this is the one place that changes.
    parent: String,
}

impl ApiCell {
    /// `base` is the API's URL; `token` is what goes in `authorization`.
    /// A reader for a node.
    pub fn for_node(base: &str, token: &str, node: &str) -> Result<Self> {
        Self::new(base, token, &format!("node={node}"))
    }

    /// A reader for a storage pool.
    pub fn for_pool(base: &str, token: &str, pool: &str) -> Result<Self> {
        Self::new(base, token, &format!("pool={pool}"))
    }

    fn new(base: &str, token: &str, who: &str) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let authority = base
            .strip_prefix("http://")
            .ok_or_else(|| {
                HostError::failed(format!(
                    "{base} is not an http:// URL; this agent speaks plain HTTP to the API and \
                     says so rather than quietly failing to verify a certificate"
                ))
            })?
            .to_string();
        Ok(Self {
            base,
            authority,
            token: token.to_string(),
            who: who.to_string(),
            parent: String::new(),
        })
    }

    /// Read every object of a kind that this node has business with.
    async fn list<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<T>> {
        let path = self.path(kind, false);
        let body = self.get(&path).await?;
        let document: Value = serde_json::from_slice(&body)
            .map_err(|e| HostError::failed(format!("{kind}: unreadable answer: {e}")))?;
        let items = document
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| HostError::failed(format!("{kind}: the answer carried no items")))?;
        items
            .iter()
            .map(|item| {
                // Contract shape in, model shape out — the same conversion the
                // API does on the way out, run backwards. Sharing the code is
                // what stops the two ends of a contract drifting.
                serde_json::from_value(velstra_cloud_wire::from_wire(item.clone()))
                    .map_err(|e| HostError::failed(format!("{kind}: {e}")))
            })
            .collect()
    }

    /// `/api/v1/[parent/]kind?node=…`, plus `&watch=true` when asking to be
    /// woken rather than answered.
    fn path(&self, kind: &str, watch: bool) -> String {
        let parent = if self.parent.is_empty() {
            String::new()
        } else {
            format!("{}/", self.parent)
        };
        let watch = if watch { "&watch=true" } else { "" };
        format!("/api/v1/{parent}{kind}?{}{watch}", self.who)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let (mut sender, connection) = self.connect().await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = self.request(path)?;
        let response = tokio::time::timeout(READ_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| HostError::failed(format!("{path}: the API did not answer in time")))?
            .map_err(|e| HostError::failed(format!("{path}: {e}")))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| HostError::failed(format!("{path}: reading the answer: {e}")))?
            .to_bytes();
        if status != StatusCode::OK {
            return Err(HostError::failed(format!(
                "{path}: the API answered {status}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        Ok(body.to_vec())
    }

    async fn connect(
        &self,
    ) -> Result<(
        hyper::client::conn::http1::SendRequest<String>,
        hyper::client::conn::http1::Connection<TokioIo<tokio::net::TcpStream>, String>,
    )> {
        let stream = tokio::net::TcpStream::connect(&self.authority)
            .await
            .map_err(|e| HostError::failed(format!("connecting to {}: {e}", self.base)))?;
        hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| HostError::failed(format!("speaking HTTP to {}: {e}", self.base)))
    }

    fn request(&self, path: &str) -> Result<Request<String>> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header("host", &self.authority)
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/json")
            .body(String::new())
            .map_err(|e| HostError::failed(format!("building a request for {path}: {e}")))
    }

    /// Stay subscribed to one collection, waking `tx` whenever anything arrives.
    ///
    /// Reconnects for ever, with a pause: an API that is down is a reason to
    /// fall back on the resync timer, never a reason for the agent to stop.
    async fn follow(self: Arc<Self>, kind: &'static str, tx: tokio::sync::mpsc::Sender<()>) {
        let path = self.path(kind, true);
        loop {
            if tx.is_closed() {
                return;
            }
            match self.stream(&path, &tx).await {
                Ok(()) => tracing::debug!(kind, "the API's stream ended; subscribing again"),
                Err(e) => {
                    tracing::warn!(kind, error = %e, "could not follow; the resync carries it")
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// One subscription, until it ends.
    async fn stream(&self, path: &str, tx: &tokio::sync::mpsc::Sender<()>) -> Result<()> {
        let (mut sender, connection) = self.connect().await?;
        let pump = tokio::spawn(async move {
            let _ = connection.await;
        });
        let response = sender
            .send_request(self.request(path)?)
            .await
            .map_err(|e| HostError::failed(format!("{path}: {e}")))?;
        if response.status() != StatusCode::OK {
            pump.abort();
            return Err(HostError::failed(format!(
                "{path}: the API answered {}",
                response.status()
            )));
        }
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| HostError::failed(format!("{path}: {e}")))?;
            // The payload is deliberately not read. What a pass needs is that
            // *something* changed; what changed it will find by looking.
            if frame.data_ref().is_some() && tx.try_send(()).is_err() && tx.is_closed() {
                pump.abort();
                return Ok(());
            }
        }
        pump.abort();
        Ok(())
    }
}

#[async_trait]
impl CellReader for ApiCell {
    async fn instances(&self) -> Result<Vec<Instance>> {
        self.list("instances").await
    }
    async fn attachments(&self) -> Result<Vec<Attachment>> {
        self.list("attachments").await
    }
    async fn ports(&self) -> Result<Vec<Port>> {
        self.list("ports").await
    }
    async fn migrations(&self) -> Result<Vec<Migration>> {
        self.list("migrations").await
    }
    async fn security_groups(&self) -> Result<Vec<SecurityGroup>> {
        self.list("security-groups").await
    }
    async fn subnets(&self) -> Result<Vec<Subnet>> {
        self.list("subnets").await
    }
    async fn networks(&self) -> Result<Vec<Network>> {
        self.list("networks").await
    }

    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let me = Arc::new(Self {
            base: self.base.clone(),
            authority: self.authority.clone(),
            token: self.token.clone(),
            who: self.who.clone(),
            parent: self.parent.clone(),
        });
        // Only the four that are assigned. The shared collections change rarely
        // and the resync is what notices; subscribing to them would put every
        // node back on every event about them.
        for kind in ["instances", "ports", "attachments", "migrations"] {
            tokio::spawn(me.clone().follow(kind, tx.clone()));
        }
        rx
    }

    fn describe(&self) -> String {
        format!(
            "{} with {}: it is handed the objects it holds or has been given, and the API serves \
             every agent from one watch per collection",
            self.base, self.who
        )
    }
}

/// The same client, reading the storage half.
#[async_trait]
impl crate::cell::PoolReader for ApiCell {
    async fn volumes(&self) -> Result<Vec<velstra_cloud_model::resources::Volume>> {
        self.list("volumes").await
    }
    async fn snapshots(&self) -> Result<Vec<velstra_cloud_model::resources::Snapshot>> {
        self.list("snapshots").await
    }
    fn describe(&self) -> String {
        format!(
            "{} with {}: this pool is handed the volumes and snapshots it holds or has been \
             given, and nothing else in the cell",
            self.base, self.who
        )
    }
}
