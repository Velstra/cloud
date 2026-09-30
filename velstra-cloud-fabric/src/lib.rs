//! How this control plane talks to the Velstra fabric.
//!
//! One crate rather than a copy in each caller, because there are now two: a
//! node agent programs its own ports, and a controller mirrors the cell-wide
//! facts — networks — that no single machine owns. Two vendored copies of one
//! contract would drift, and the drift would show up as a field silently
//! meaning something different on one side.
//!
//! Client only. Nothing here serves anything to fabric; generating a server
//! would be code nobody can call.

/// The generated client for fabric's orchestrator.
///
/// Lints are relaxed for generated code only: it is not this repository's to
/// write, and pinning its style would mean editing it by hand every time the
/// contract moves — which is exactly what the vendored copy exists to avoid.
#[allow(clippy::result_large_err, clippy::doc_overindented_list_items)]
pub mod pb {
    tonic::include_proto!("velstra.v1");
}

pub use pb::velstra_orchestrator_client::VelstraOrchestratorClient as Client;

/// The client as [`connect`] hands it back.
///
/// Re-exported as a name of its own because a caller that keeps one — a
/// controller that makes several calls in a pass — otherwise has to spell
/// `tonic::transport::Channel`, and would need tonic as a dependency to say a
/// type it never constructs.
pub type Connected = Client<tonic::transport::Channel>;

/// Re-exported so a caller can name a refusal without depending on tonic
/// directly: every method on [`Connected`] fails with one.
pub use tonic::Status;

/// Connect to the fabric's orchestrator.
///
/// A single endpoint retains the original wire contract. With a comma-separated
/// list, each controller is asked whether it currently leads before returning
/// a client for mutations. The next reconcile re-discovers leadership after a
/// failover; a follower must never make an otherwise healthy tenant port look
/// permanently refused just because it was first in the configured list.
pub type ConnectError = Box<dyn std::error::Error + Send + Sync>;

type LeaderCache = std::sync::Mutex<std::collections::HashMap<String, String>>;
static LAST_LEADER: std::sync::OnceLock<LeaderCache> = std::sync::OnceLock::new();

fn endpoints(configured: &str) -> Result<Vec<&str>, ConnectError> {
    let entries: Vec<_> = configured.split(',').map(str::trim).collect();
    if entries.iter().any(|e| e.is_empty()) {
        return Err("fabric endpoint list contains an empty URL".into());
    }
    if entries.iter().any(|e| e.contains('@')) {
        return Err("fabric endpoints must not contain embedded credentials".into());
    }
    let scheme = if entries[0].starts_with("https://") {
        "https://"
    } else if entries[0].starts_with("http://") {
        "http://"
    } else {
        return Err("fabric endpoint must use HTTP or HTTPS".into());
    };
    if entries.iter().any(|e| !e.starts_with(scheme)) {
        return Err("all fabric endpoints must use the same HTTP or HTTPS scheme".into());
    }
    Ok(entries)
}

pub async fn connect(configured: &str) -> Result<Connected, ConnectError> {
    let mut entries = endpoints(configured)?;
    if entries.len() == 1 {
        return connect_one(entries[0], 5).await;
    }
    // A controller or node agent can reconcile many ports per pass. Probe the
    // last successful leader first instead of contacting the same follower for
    // every port. The probe still checks leadership, so failover is discovered
    // rather than trusting a stale cache entry.
    let cache = LAST_LEADER.get_or_init(|| std::sync::Mutex::new(Default::default()));
    if let Some(last) = cache
        .lock()
        .ok()
        .and_then(|map| map.get(configured).cloned())
        && let Some(position) = entries.iter().position(|endpoint| *endpoint == last)
    {
        entries.swap(0, position);
    }
    let mut last = String::from("no fabric controller reported a leader");
    for endpoint in entries {
        match connect_one(endpoint, 3).await {
            Ok(mut client) => {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    client.get_leader(pb::LeaderRequest {}),
                )
                .await
                {
                    Ok(Ok(response)) if response.get_ref().leader => {
                        if let Ok(mut map) = cache.lock() {
                            map.insert(configured.to_owned(), endpoint.to_owned());
                        }
                        return Ok(client);
                    }
                    Ok(Ok(_)) => last = format!("{endpoint} is a follower"),
                    Ok(Err(error)) => last = format!("{endpoint}: leader probe: {error}"),
                    Err(_) => last = format!("{endpoint}: leader probe timed out"),
                }
            }
            Err(error) => last = format!("{endpoint}: {error}"),
        }
    }
    Err(format!("no writable fabric controller: {last}").into())
}

async fn connect_one(endpoint: &str, connect_timeout_s: u64) -> Result<Connected, ConnectError> {
    // Fabric is part of a reconciliation pass, so an unreachable daemon must
    // become a reported failure instead of stopping heartbeats, migrations and
    // every unrelated guest action on the node.  The channel timeout applies
    // to each RPC made through the returned client as well as bounding setup.
    let mut channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())?
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_s))
        .timeout(std::time::Duration::from_secs(10));
    if let Some(tls) = tls_config(
        endpoint,
        std::env::var_os("VELSTRA_FABRIC_CA").map(std::path::PathBuf::from),
        std::env::var_os("VELSTRA_FABRIC_CERT").map(std::path::PathBuf::from),
        std::env::var_os("VELSTRA_FABRIC_KEY").map(std::path::PathBuf::from),
    )? {
        channel = channel.tls_config(tls)?;
    }
    Ok(Client::new(channel.connect().await?))
}

/// A configured identity must never silently fall back to plaintext or to an
/// unauthenticated TLS connection. Separate files allow per-node identities.
fn tls_config(
    endpoint: &str,
    ca: Option<std::path::PathBuf>,
    cert: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
) -> Result<Option<tonic::transport::ClientTlsConfig>, ConnectError> {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    if cert.is_some() != key.is_some() {
        return Err("VELSTRA_FABRIC_CERT and VELSTRA_FABRIC_KEY must be set together".into());
    }
    if ca.is_none() && cert.is_some() {
        return Err("the fabric client identity requires VELSTRA_FABRIC_CA".into());
    }
    if ca.is_some() && !endpoint.starts_with("https://") {
        return Err("fabric TLS credentials require an https:// endpoint".into());
    }
    if endpoint.starts_with("https://") {
        // Tonic can bring both rustls crypto backends into a final binary.
        // Without an explicit process default, constructing its TLS channel
        // panics. The Cloud API and node agent already choose ring; do it here
        // as well so a controller's first Fabric reconcile cannot exit its
        // whole process when HTTPS is enabled.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let Some(ca) = ca else {
        return Ok(endpoint.starts_with("https://").then(ClientTlsConfig::new));
    };
    let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(std::fs::read(ca)?));
    if let (Some(cert), Some(key)) = (cert, key) {
        tls = tls.identity(Identity::from_pem(
            std::fs::read(cert)?,
            std::fs::read(key)?,
        ));
    }
    Ok(Some(tls))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn path() -> Option<std::path::PathBuf> {
        Some("missing-identity.pem".into())
    }
    #[test]
    fn endpoint_list_rejects_empty_members() {
        assert_eq!(
            endpoints("https://a:50052,https://b:50052").unwrap(),
            ["https://a:50052", "https://b:50052"]
        );
        assert!(endpoints("").is_err());
        assert!(endpoints("https://a:50052,,https://b:50052").is_err());
        assert!(endpoints("https://a:50052,").is_err());
        assert!(endpoints("https://a:50052,http://b:50052").is_err());
        assert!(endpoints("https://user:password@a:50052").is_err());
    }
    #[test]
    fn partial_identity_is_rejected() {
        assert!(
            tls_config("https://fabric:50052", path(), path(), None)
                .unwrap_err()
                .to_string()
                .contains("set together")
        );
    }
    #[test]
    fn identity_without_trust_is_rejected() {
        assert!(
            tls_config("https://fabric:50052", None, path(), path())
                .unwrap_err()
                .to_string()
                .contains("requires")
        );
    }
    #[test]
    fn credentials_on_plaintext_are_rejected() {
        assert!(
            tls_config("http://fabric:50052", path(), path(), path())
                .unwrap_err()
                .to_string()
                .contains("https://")
        );
    }
    #[test]
    fn missing_ca_is_rejected() {
        assert!(tls_config("https://fabric:50052", path(), None, None).is_err());
    }
    #[test]
    fn local_plaintext_remains_explicitly_supported() {
        assert!(
            tls_config("http://127.0.0.1:50052", None, None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn https_channel_construction_has_a_crypto_provider() {
        let tls = tls_config("https://127.0.0.1:50052", None, None, None)
            .unwrap()
            .unwrap();
        tonic::transport::Endpoint::from_static("https://127.0.0.1:50052")
            .tls_config(tls)
            .expect("HTTPS Fabric client construction must not panic");
    }
}
