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
/// A plain helper rather than a wrapper type: every caller wants the generated
/// client, and a type that only forwarded to it would be one more thing to read
/// before finding out it does nothing.
pub type ConnectError = Box<dyn std::error::Error + Send + Sync>;

pub async fn connect(endpoint: &str) -> Result<Connected, ConnectError> {
    // Fabric is part of a reconciliation pass, so an unreachable daemon must
    // become a reported failure instead of stopping heartbeats, migrations and
    // every unrelated guest action on the node.  The channel timeout applies
    // to each RPC made through the returned client as well as bounding setup.
    let mut channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())?
        .connect_timeout(std::time::Duration::from_secs(5))
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
}
