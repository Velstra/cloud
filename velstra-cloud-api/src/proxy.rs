//! Sending a request to the cell that owns it.
//!
//! A cell is the failure and scaling domain, so growing means adding cells. That
//! only works if a client can reach one address and have its request land in the
//! cell holding the resource. This is that hop.
//!
//! ## What decides
//!
//! [`velstra_cloud_model::routing::Directory`] — project id to cell, read from
//! the projects collection, which is global. Resolution is a parse of the first
//! two segments of a name and a map hit; there is no query in front of a
//! request. The directory is kept by an informer (one watch, in memory), so a
//! project moving is picked up without anybody polling.
//!
//! **No opinion means answer here, never refuse.** A router a few seconds behind
//! would otherwise reject a freshly created project's first request, turning
//! propagation delay into an error a tenant sees. If this cell turns out not to
//! own the resource after all, its own `check_cell` refuses and names the cell
//! that does — a correct answer one hop late, rather than a wrong one now.
//!
//! ## Three things that are easy to leave out and are not optional
//!
//! **A hop marker.** Two routers with directories that disagree will forward a
//! request to each other until something gives out. Every forwarded request
//! carries [`FORWARDED`], and a request that arrives carrying it is answered
//! here whatever the directory says — one hop, always, and a disagreement
//! becomes a wrong answer from a named cell instead of a loop.
//!
//! **Hop-by-hop headers.** `Connection`, `Transfer-Encoding` and the rest of RFC
//! 9110 §7.6.1 describe *this* connection and are meaningless on the next one.
//! Forwarding them produces a request that is malformed in ways that surface far
//! from here. `Host` is dropped for the same reason: it names the wrong server.
//!
//! **The body is streamed, not collected.** A watch is an open response that
//! never ends, and an image upload is measured in gigabytes. Buffering either
//! turns a proxy into the place the cell runs out of memory.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderName, Request, Response, StatusCode, Uri, header},
    middleware::Next,
    response::IntoResponse,
};
use velstra_cloud_model::{
    meta::ResourceName,
    resources::{ProjectSpec, ProjectStatus},
    routing::Directory,
};
use velstra_cloud_store::{Cached, Store, TypedStore};

use crate::error::{ApiError, Code};

/// Marks a request this installation has already routed once.
///
/// Not a general "via" record — its only job is to make a second hop
/// impossible. See the module note.
pub const FORWARDED: HeaderName = HeaderName::from_static("x-velstra-forwarded");

/// Headers that describe one connection and must not be copied onto another
/// (RFC 9110 §7.6.1), plus `Host`, which names the wrong server once forwarded.
const HOP_BY_HOP: [HeaderName; 8] = [
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::HOST,
];

/// Where each cell can be reached. Deployment configuration rather than a
/// resource: a cell's address is a fact about the network, and putting it in the
/// store would mean a cell has to be reachable to learn how to reach it.
#[derive(Clone, Debug, Default)]
pub struct Cells(BTreeMap<String, String>);

impl Cells {
    /// From `cell=http://host:port` pairs, as a flag or an environment variable
    /// would give them.
    pub fn parse(pairs: &[String]) -> Result<Self, String> {
        let mut out = BTreeMap::new();
        for pair in pairs {
            let (cell, endpoint) = pair
                .split_once('=')
                .ok_or_else(|| format!("{pair:?} is not cell=endpoint"))?;
            if cell.is_empty() || endpoint.is_empty() {
                return Err(format!("{pair:?} names an empty cell or endpoint"));
            }
            out.insert(cell.to_string(), endpoint.trim_end_matches('/').to_string());
        }
        Ok(Self(out))
    }

    pub fn endpoint(&self, cell: &str) -> Option<&str> {
        self.0.get(cell).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What the middleware needs to decide and to forward.
#[derive(Clone)]
pub struct Router {
    /// This cell's name — what "answer here" means.
    cell: String,
    cells: Cells,
    /// The projects, kept current by one watch. `None` when this process routes
    /// nothing, which is every single-cell deployment.
    projects: Option<Cached<ProjectSpec, ProjectStatus>>,
}

impl Router {
    /// A router that always answers locally. What a single-cell installation
    /// gets, and what every test that is not about routing wants.
    pub fn local(cell: &str) -> Self {
        Self {
            cell: cell.to_string(),
            cells: Cells::default(),
            projects: None,
        }
    }

    /// A router that resolves against the projects in `store` and forwards to
    /// the cells in `cells`.
    pub fn new(store: Arc<dyn Store>, cell: &str, cells: Cells) -> Self {
        let projects = Cached::start(
            TypedStore::new(store.clone(), cell, "projects"),
            store,
            velstra_cloud_store::prefix_for(cell, "projects"),
        );
        Self {
            cell: cell.to_string(),
            cells,
            projects: Some(projects),
        }
    }

    /// The directory as it currently stands.
    async fn directory(&self) -> Directory {
        let Some(projects) = &self.projects else {
            return Directory::default();
        };
        let (held, _) = projects.all().await;
        Directory::new(held.iter().filter_map(|p| {
            let id = p.meta.name.to_string();
            let id = id.strip_prefix("projects/")?.to_string();
            Some((id, p.spec.cell.clone()))
        }))
    }

    /// Which cell should answer for this path, if it is not this one.
    async fn elsewhere(&self, path: &str) -> Option<String> {
        let name = resource_name(path)?;
        let directory = self.directory().await;
        let home = directory.cell_of(&name)?;
        (home != self.cell).then(|| home.to_string())
    }
}

/// The resource name a request path addresses, if it addresses one.
///
/// Only `/api/v1/...` is routed. The console and its assets are served by
/// whichever cell was asked — they are markup, identical everywhere, and a
/// person who typed one cell's address should get a page from it.
fn resource_name(path: &str) -> Option<ResourceName> {
    let rest = path.strip_prefix("/api/v1/")?;
    // A verb (`:explainPlacement`) and a query are not part of the name.
    let rest = rest.split('?').next()?.split(':').next()?;
    ResourceName::parse(rest.trim_end_matches('/')).ok()
}

/// Forward a request the directory says belongs to another cell, or hand it on.
pub async fn route(
    State(router): State<Router>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    // Already routed once. Answering here — even if this cell turns out to be
    // wrong — is what makes two disagreeing routers produce a refusal naming a
    // cell instead of a loop between them.
    if request.headers().contains_key(&FORWARDED) {
        return next.run(request).await;
    }
    let path = request.uri().path().to_string();
    let Some(home) = router.elsewhere(&path).await else {
        return next.run(request).await;
    };
    let Some(endpoint) = router.cells.endpoint(&home) else {
        // Known to belong elsewhere and no way to get there. Said plainly: the
        // alternative is answering from the wrong cell, which is the failure
        // this whole path exists to prevent.
        return ApiError::new(
            Code::FailedPrecondition,
            format!(
                "this resource lives in cell {home}, and this router has no address for it; \
                 send the request to {home} directly or configure its endpoint here"
            ),
        )
        .into_response();
    };
    forward(request, endpoint).await
}

/// Send `request` to `endpoint` and stream the answer back.
async fn forward(request: Request<Body>, endpoint: &str) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let target: Uri = match format!("{endpoint}{path_and_query}").parse() {
        Ok(uri) => uri,
        Err(e) => return gateway_error(format!("{endpoint} is not a usable address: {e}")),
    };
    let (Some(host), port) = (target.host(), target.port_u16().unwrap_or(80)) else {
        return gateway_error(format!("{endpoint} names no host"));
    };

    let stream = match tokio::net::TcpStream::connect((host, port)).await {
        Ok(s) => s,
        Err(e) => return gateway_error(format!("connecting to {host}:{port}: {e}")),
    };
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) = match hyper::client::conn::http1::handshake(io).await {
        Ok(pair) => pair,
        Err(e) => return gateway_error(format!("handshake with {host}: {e}")),
    };
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut outgoing = Request::builder().method(parts.method.clone()).uri(
        target
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string(),
    );
    if let Some(headers) = outgoing.headers_mut() {
        for (name, value) in &parts.headers {
            if HOP_BY_HOP.contains(name) {
                continue;
            }
            headers.insert(name.clone(), value.clone());
        }
        headers.insert(
            header::HOST,
            match host.parse() {
                Ok(v) => v,
                Err(_) => return gateway_error(format!("{host} is not a usable Host header")),
            },
        );
        // One hop, always.
        headers.insert(FORWARDED, header::HeaderValue::from_static("1"));
    }
    let outgoing = match outgoing.body(body) {
        Ok(r) => r,
        Err(e) => return gateway_error(format!("building the forwarded request: {e}")),
    };

    match sender.send_request(outgoing).await {
        Ok(response) => {
            let (parts, body) = response.into_parts();
            let mut out = Response::builder().status(parts.status);
            if let Some(headers) = out.headers_mut() {
                for (name, value) in &parts.headers {
                    if HOP_BY_HOP.contains(name) {
                        continue;
                    }
                    headers.insert(name.clone(), value.clone());
                }
            }
            // Streamed, never collected: a watch is a response that does not
            // end, and an image is measured in gigabytes.
            out.body(Body::new(body))
                .unwrap_or_else(|e| gateway_error(format!("relaying the answer: {e}")))
        }
        Err(e) => gateway_error(format!("forwarding to {host}: {e}")),
    }
}

/// A failure of the hop itself, distinct from anything the far cell said.
///
/// `502`, because the request was well-formed and this router could not deliver
/// it — reporting it as the tenant's mistake would send somebody debugging their
/// own payload.
fn gateway_error(why: String) -> Response<Body> {
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(serde_json::json!({
            "error": { "code": "BAD_GATEWAY", "message": why }
        })),
    )
        .into_response()
}
