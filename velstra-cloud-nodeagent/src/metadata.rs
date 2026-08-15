//! The metadata service, on the node that runs the guest.
//!
//! A guest asks `169.254.169.254` who it is. The answer comes from the agent on
//! the same machine, which means its availability is that machine's
//! availability: there is no central metadata service whose outage stops every
//! guest in the region from booting. A node that is up can always answer for
//! the guests it is running, because it is the thing running them.
//!
//! **Identity is the source address, and nothing else.** No token, no header, no
//! query parameter — all of which a guest could forge or a neighbour could
//! replay. The address on the packet is the one thing the node itself assigned,
//! through the port it programmed, so it is the only claim in the request that
//! the answerer already knows to be true. An address the agent has not
//! programmed on this node gets a 404, and one guest asking about another's
//! user-data is not a request this service can express.

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
};

use axum::{
    Router,
    extract::{ConnectInfo, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};

/// What a guest may learn about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstanceMetadata {
    /// The instance resource name.
    pub instance_id: String,
    pub hostname: String,
    pub ssh_keys: Vec<String>,
    pub user_data: Option<String>,
}

/// Which address is which guest, on this node.
///
/// Replaced wholesale by the agent on every pass rather than edited: the map is
/// derived from the instances and ports this node currently holds, so an entry
/// can never outlive the guest it describes — which is how a re-used address
/// ends up serving the previous tenant's keys.
#[derive(Clone, Default)]
pub struct MetadataRegistry {
    by_address: Arc<RwLock<BTreeMap<IpAddr, Arc<InstanceMetadata>>>>,
}

impl MetadataRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&self, entries: BTreeMap<IpAddr, InstanceMetadata>) {
        let next = entries
            .into_iter()
            .map(|(addr, meta)| (addr, Arc::new(meta)))
            .collect();
        *self.by_address.write().unwrap() = next;
    }

    pub fn lookup(&self, addr: IpAddr) -> Option<Arc<InstanceMetadata>> {
        self.by_address.read().unwrap().get(&addr).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_address.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Parse an address as it appears on a port — with or without a prefix length.
pub fn address_of(port_address: &str) -> Option<IpAddr> {
    port_address
        .split('/')
        .next()
        .and_then(|a| a.parse::<IpAddr>().ok())
}

/// Serve until the returned handle is dropped or aborted.
///
/// The listener is bound before returning and its real address handed back, so
/// a caller (a test, or a node whose link-local address is not up yet) knows
/// exactly where it ended up instead of guessing.
pub async fn serve(
    listen: SocketAddr,
    registry: MetadataRegistry,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    let app = router(registry);
    let handle = tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, service).await {
            tracing::error!(error = %e, "the metadata service stopped");
        }
    });
    Ok((bound, handle))
}

fn router(registry: MetadataRegistry) -> Router {
    Router::new()
        .route("/latest/meta-data", get(index))
        .route("/latest/meta-data/", get(index))
        .route("/latest/meta-data/instance-id", get(instance_id))
        .route("/latest/meta-data/hostname", get(hostname))
        .route("/latest/meta-data/local-hostname", get(hostname))
        .route("/latest/meta-data/public-keys", get(public_keys))
        .route("/latest/meta-data/public-keys/", get(public_keys))
        .route(
            "/latest/meta-data/public-keys/:index/openssh-key",
            get(openssh_key),
        )
        .route("/latest/user-data", get(user_data))
        .fallback(unknown_path)
        .with_state(registry)
}

/// The one place a caller becomes a guest. Everything else in this file goes
/// through it, so there is no handler that can accidentally answer for someone.
fn caller(registry: &MetadataRegistry, peer: SocketAddr) -> Option<Arc<InstanceMetadata>> {
    registry.lookup(peer.ip())
}

/// Deliberately the same answer for a stranger and for a path that does not
/// exist: nobody learns from here whether an address is in use on this node.
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

fn text(body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

async fn index(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(_) => text("instance-id\nhostname\nlocal-hostname\npublic-keys\n"),
        None => not_found(),
    }
}

async fn instance_id(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(me) => text(me.instance_id.clone()),
        None => not_found(),
    }
}

async fn hostname(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        Some(me) => text(me.hostname.clone()),
        None => not_found(),
    }
}

async fn public_keys(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    match caller(&registry, peer) {
        // The index format cloud-init expects: one `n=name` line per key.
        Some(me) => text(
            me.ssh_keys
                .iter()
                .enumerate()
                .map(|(i, _)| format!("{i}=velstra\n"))
                .collect::<String>(),
        ),
        None => not_found(),
    }
}

async fn openssh_key(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(index): Path<usize>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    match me.ssh_keys.get(index) {
        Some(key) => text(format!("{key}\n")),
        None => not_found(),
    }
}

async fn user_data(
    State(registry): State<MetadataRegistry>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(me) = caller(&registry, peer) else {
        return not_found();
    };
    match &me.user_data {
        Some(data) => text(data.clone()),
        // An instance with no user-data gets the same 404 cloud-init expects,
        // not an empty 200 it would then try to run.
        None => not_found(),
    }
}

async fn unknown_path() -> Response {
    not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_taken_from_a_port_with_or_without_its_prefix() {
        assert_eq!(
            address_of("10.0.0.5/24"),
            Some("10.0.0.5".parse::<IpAddr>().unwrap())
        );
        assert_eq!(
            address_of("fd00::5"),
            Some("fd00::5".parse::<IpAddr>().unwrap())
        );
        assert_eq!(address_of("not-an-address"), None);
    }

    #[test]
    fn replacing_the_map_forgets_an_address_that_is_no_longer_here() {
        // The reason this is a replace and not an insert: a guest that moved
        // away must stop being answerable for, immediately and without anyone
        // remembering to remove it.
        let registry = MetadataRegistry::new();
        let addr: IpAddr = "10.0.0.5".parse().unwrap();
        registry.replace(BTreeMap::from([(
            addr,
            InstanceMetadata {
                instance_id: "projects/p1/instances/i1".into(),
                ..Default::default()
            },
        )]));
        assert!(registry.lookup(addr).is_some());
        registry.replace(BTreeMap::new());
        assert!(registry.lookup(addr).is_none());
    }
}
