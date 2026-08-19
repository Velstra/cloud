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

/// Connect to the fabric's orchestrator.
///
/// A plain helper rather than a wrapper type: every caller wants the generated
/// client, and a type that only forwarded to it would be one more thing to read
/// before finding out it does nothing.
pub async fn connect(
    endpoint: &str,
) -> Result<Client<tonic::transport::Channel>, tonic::transport::Error> {
    Client::connect(endpoint.to_string()).await
}
