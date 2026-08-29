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
    resources::{Attachment, Image, Instance, Network, Port, Subnet},
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
    /// The TLS client config, when the API is https. `None` is plain HTTP.
    tls: Option<std::sync::Arc<tokio_rustls::rustls::ClientConfig>>,
}

/// A client config whose **only** root is the cell's own certificate.
///
/// Pinning, not PKI: a self-signed API certificate verifies against nothing a
/// system store holds, and shipping `danger_accept_invalid_certs` instead would
/// mean any machine on the path can be the API. The file the agent trusts is
/// the file the operator was shown the fingerprint of.
fn tls_config(ca_path: &str) -> Result<std::sync::Arc<tokio_rustls::rustls::ClientConfig>> {
    let pem =
        std::fs::read(ca_path).map_err(|e| HostError::failed(format!("reading {ca_path}: {e}")))?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        let cert = cert.map_err(|e| HostError::failed(format!("{ca_path}: {e}")))?;
        roots
            .add(cert)
            .map_err(|e| HostError::failed(format!("{ca_path} is not usable as a root: {e}")))?;
    }
    if roots.is_empty() {
        return Err(HostError::failed(format!(
            "{ca_path} holds no certificate to trust"
        )));
    }
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
}

/// Either side of the line, behind one type, so the HTTP client above does not
/// care which it got.
type IoStream = TokioIo<Box<dyn Stream>>;
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Stream for T {}

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
        // Two schemes, two trust stories, both explicit. `http://` is what a
        // developer cell speaks. `https://` is what a cell speaks the moment
        // `quickstart` makes it a certificate — and the agent broke the day
        // that landed: this client refused anything but http, so the whole
        // node was cut off from its cell, every guest on it stuck at
        // `Unknown`, and the log filled with "invalid HTTP version parsed",
        // which is what a TLS greeting looks like to an HTTP parser.
        let (tls, authority) = if let Some(rest) = base.strip_prefix("http://") {
            (None, rest.to_string())
        } else if let Some(rest) = base.strip_prefix("https://") {
            let ca = std::env::var("VELSTRA_API_CA")
                .ok()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    HostError::failed(format!(
                        "{base} is https and this agent has no root to verify it against. \
                         Set VELSTRA_API_CA to the API's certificate (quickstart writes it \
                         to /var/lib/velstra/tls/cert.pem) — the alternative would be to \
                         accept any certificate at all, which is not an alternative."
                    ))
                })?;
            (Some(tls_config(&ca)?), rest.to_string())
        } else {
            return Err(HostError::failed(format!(
                "{base} names no scheme this agent speaks (http:// or https://)"
            )));
        };
        Ok(Self {
            base,
            authority,
            tls,
            token: token.to_string(),
            who: who.to_string(),
            parent: String::new(),
        })
    }

    /// Read every object of a kind that this node has business with.
    async fn list<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<T>> {
        self.list_at(kind, &self.path(kind, false)).await
    }

    /// Read a cell-wide collection whole: no project in the path, no filter.
    async fn list_cell<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<T>> {
        self.list_at(kind, &format!("/api/v1/{kind}")).await
    }

    /// One object by name, or `None` if it is not there.
    ///
    /// A 404 is an answer, not a failure: an agent starting before its node is
    /// registered is a normal order of events in a level-triggered system.
    async fn get_one<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let body = match self.get(&format!("/api/v1/{name}")).await {
            Ok(body) => body,
            Err(e) if e.to_string().contains("404") => return Ok(None),
            Err(e) => return Err(e),
        };
        let document: Value = serde_json::from_slice(&body)
            .map_err(|e| HostError::failed(format!("{name}: unreadable answer: {e}")))?;
        serde_json::from_value(velstra_cloud_wire::from_wire(document))
            .map(Some)
            .map_err(|e| HostError::failed(format!("{name}: {e}")))
    }

    async fn list_at<T: DeserializeOwned>(&self, kind: &str, path: &str) -> Result<Vec<T>> {
        let body = self.get(path).await?;
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
        hyper::client::conn::http1::Connection<IoStream, String>,
    )> {
        let stream = tokio::net::TcpStream::connect(&self.authority)
            .await
            .map_err(|e| HostError::failed(format!("connecting to {}: {e}", self.base)))?;
        let io: Box<dyn Stream> = match &self.tls {
            None => Box::new(stream),
            Some(config) => {
                let host = self
                    .authority
                    .rsplit_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or(&self.authority)
                    .to_string();
                let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host)
                    .map_err(|e| HostError::failed(format!("{}: {e}", self.base)))?;
                let connected = tokio_rustls::TlsConnector::from(config.clone())
                    .connect(name, stream)
                    .await
                    .map_err(|e| {
                        HostError::failed(format!(
                            "the API at {} did not verify against VELSTRA_API_CA: {e}",
                            self.base
                        ))
                    })?;
                Box::new(connected)
            }
        };
        hyper::client::conn::http1::handshake(TokioIo::new(io))
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

    /// POST a body, optionally conditional on `If-Match`, and return the status
    /// and body. One connection per call, like [`Self::get`] — a report is rare
    /// enough per object that pooling would buy little and add a failure mode.
    async fn post(
        &self,
        path: &str,
        body: String,
        if_match: Option<u64>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("host", &self.authority)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .header("accept", "application/json");
        if let Some(revision) = if_match {
            builder = builder.header("if-match", format!("\"{revision}\""));
        }
        let request = builder
            .body(body)
            .map_err(|e| HostError::failed(format!("building a request for {path}: {e}")))?;

        let (mut sender, connection) = self.connect().await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let response = tokio::time::timeout(READ_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| HostError::failed(format!("{path}: the API did not answer in time")))?
            .map_err(|e| HostError::failed(format!("{path}: {e}")))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| HostError::failed(format!("{path}: reading the answer: {e}")))?
            .to_bytes();
        Ok((status, bytes.to_vec()))
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

    async fn console_sessions(
        &self,
    ) -> Result<Vec<velstra_cloud_model::resources::ConsoleSession>> {
        self.list("console-sessions").await
    }

    async fn images(&self) -> Result<Vec<Image>> {
        self.list("images").await
    }

    async fn captures(&self) -> Result<Vec<velstra_cloud_model::resources::Capture>> {
        // `list`, like the instances: the API narrows every agent's read to
        // what concerns it, so a capture assigned elsewhere never arrives here.
        self.list("captures").await
    }

    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>> {
        self.list("backup-targets").await
    }

    async fn floating_ips(&self) -> Result<Vec<velstra_cloud_model::resources::FloatingIp>> {
        // Not narrowed by node, and it cannot be: an address is bound to a
        // port, and which node holds that port is a fact this list is being
        // read to find out. The API's per-agent filter already hands a node
        // only what concerns it.
        self.list("floatingips").await
    }

    /// Cell-wide, so no project in the path and no `?node=` filter: these are
    /// the two collections a node reads whole. Filtering the node list to this
    /// node would defeat the point of reading it — the Ceph pass decides
    /// whether a step is this node's by computing it over *everybody's*
    /// reports.
    async fn nodes(&self) -> Result<Vec<velstra_cloud_model::resources::Node>> {
        self.list_cell("nodes").await
    }
    async fn node(&self, id: &str) -> Result<Option<velstra_cloud_model::resources::Node>> {
        self.get_one(&format!("nodes/{id}")).await
    }
    async fn ceph_clusters(&self) -> Result<Vec<velstra_cloud_model::ceph::CephCluster>> {
        self.list_cell("ceph-clusters").await
    }

    async fn wake(&self) -> tokio::sync::mpsc::Receiver<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let me = Arc::new(Self {
            base: self.base.clone(),
            authority: self.authority.clone(),
            token: self.token.clone(),
            who: self.who.clone(),
            parent: self.parent.clone(),
            tls: self.tls.clone(),
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

/// The write half: a status report, over HTTP, as this node's own token.
///
/// The API authenticates the token as this node and runs the same ownership
/// judgement the store does, so this presents its own credential and lets the
/// server refuse anything that is not this node's. That is the whole difference
/// between `--api` mode and the direct-store default: here the writer identity
/// is *verified*, not trusted.
#[async_trait]
impl crate::sink::StatusSink for ApiCell {
    async fn write_status(
        &self,
        _kind: &str,
        object: &serde_json::Value,
        _writer: &velstra_cloud_model::access::Writer,
    ) -> crate::sink::SinkOutcome {
        use crate::sink::SinkOutcome;

        // The revision comes off the model object, where it is a number — the
        // same value a direct-store report would compare-and-swap against.
        let revision = object
            .get("meta")
            .and_then(|m| m.get("revision"))
            .and_then(serde_json::Value::as_u64);

        // Wire shape out, exactly as the API expects and as the read path
        // produces coming back — the same conversion, run the other way, so the
        // two ends of the contract cannot spell a field differently. The name is
        // one string on the wire (segments in the model), so it is read from the
        // wire form the request body carries. The writer is not sent: the API
        // derives it from the token, so a node cannot name itself as somebody
        // else.
        let wire = velstra_cloud_wire::to_wire(object.clone());
        let Some(name) = wire
            .get("meta")
            .and_then(|m| m.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            return SinkOutcome::Failed("a report carried no resource name".into());
        };
        let body = match serde_json::to_string(&wire) {
            Ok(body) => body,
            Err(e) => return SinkOutcome::Failed(format!("{name}: could not serialise: {e}")),
        };
        let path = format!("/api/v1/{name}:reportStatus");

        match self.post(&path, body, revision).await {
            Ok((status, _)) if status.is_success() => SinkOutcome::Wrote,
            // The API answers a lost compare-and-swap with 409, the same signal
            // the store gives directly.
            Ok((status, _)) if status == StatusCode::CONFLICT => SinkOutcome::Conflict,
            // 403 is the ownership refusal — this node wrote something that is
            // not its own. Named so the pass reports it rather than swallowing it.
            Ok((status, body)) if status == StatusCode::FORBIDDEN => {
                SinkOutcome::Refused(message_of(&body).unwrap_or_else(|| status.to_string()))
            }
            Ok((status, body)) => SinkOutcome::Failed(format!(
                "the API answered {status}: {}",
                message_of(&body)
                    .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(200).collect())
            )),
            Err(e) => SinkOutcome::Failed(e.to_string()),
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} with {}: status reports go through the API as this node's own token",
            self.base, self.who
        )
    }
}

/// The `error.message` an API error body carries, if it is one.
fn message_of(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
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
    async fn backups(&self) -> Result<Vec<velstra_cloud_model::resources::Backup>> {
        self.list("backups").await
    }
    async fn backup_targets(&self) -> Result<Vec<velstra_cloud_model::resources::BackupTarget>> {
        // Not filtered by pool, and it cannot be: a target belongs to the cell
        // and is named by backups from any pool. It is a short list — one row
        // per place copies are kept — so reading it whole costs nothing that
        // grows.
        self.list("backup-targets").await
    }
    fn describe(&self) -> String {
        format!(
            "{} with {}: this pool is handed the volumes, snapshots and backups it holds or has \
             been given, and nothing else in the cell",
            self.base, self.who
        )
    }
}
