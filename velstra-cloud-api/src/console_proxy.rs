//! The hop between a browser and a node.
//!
//! A tenant's browser talks to the API and to nothing else — it has no business
//! on the machines a cell is made of, and a node it could reach directly would
//! be a node it could reach without asking the API first. So the API is the one
//! that connects, to the address the node reported on its own status.
//!
//! What travels is bytes. The websocket in front carries them as binary frames
//! and so does the one behind, because a serial line carries whatever the guest
//! writes — including bytes that are not UTF-8, which is exactly the output
//! somebody attaches a console to read.
//!
//! ## The ticket goes past, not through
//!
//! This relay does not check the ticket. It cannot usefully: the check is
//! against a session object, and the node reads the same one. Checking here as
//! well would be a second copy of a rule, which is how two copies come to
//! disagree. What the API decided — who may attach, and whether they may type —
//! is already written on the session; this only carries the connection.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use rustls_pki_types::pem::PemObject;

/// Who to believe when the node's console speaks TLS.
///
/// A cell's nodes carry certificates its own authority signed, not ones a
/// public root vouches for — so the default trust store answers "unknown
/// issuer" to every one of them. This is the file that CA lives in, and it is
/// read once per attach rather than held, because an operator who replaces it
/// should not have to restart the API to be believed.
///
/// `None` falls back to the public roots. That is the right default rather than
/// a refusal: it is what a node with a publicly-signed name needs, and a cell
/// with neither simply never gets a `wss://` URL to connect to.
pub fn trust(ca: Option<&std::path::Path>) -> Option<tokio_tungstenite::Connector> {
    let ca = ca?;
    let pem = std::fs::read(ca)
        .map_err(|e| tracing::warn!(path = %ca.display(), error = %e, "the console CA could not be read"))
        .ok()?;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_pki_types::CertificateDer::pem_slice_iter(&pem).flatten() {
        let _ = roots.add(certificate);
    }
    if roots.is_empty() {
        tracing::warn!(path = %ca.display(), "the console CA file holds no certificate");
        return None;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
        config,
    )))
}

/// Relay a client's websocket to a node's, until either end stops.
pub async fn relay(
    client: WebSocket,
    node_url: String,
    connector: Option<tokio_tungstenite::Connector>,
) -> Result<(), String> {
    let (node, _) =
        tokio_tungstenite::connect_async_tls_with_config(&node_url, None, false, connector)
            .await
            .map_err(|e| format!("connecting to {node_url}: {e}"))?;

    let (mut to_client, mut from_client) = client.split();
    let (mut to_node, mut from_node) = node.split();

    let mut downstream = tokio::spawn(async move {
        while let Some(Ok(message)) = from_node.next().await {
            let bytes = match message {
                tokio_tungstenite::tungstenite::Message::Binary(bytes) => bytes,
                tokio_tungstenite::tungstenite::Message::Text(text) => text.into_bytes(),
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            if to_client.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    let mut upstream = tokio::spawn(async move {
        while let Some(Ok(message)) = from_client.next().await {
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Text(text) => text.into_bytes(),
                Message::Close(_) => break,
                _ => continue,
            };
            if to_node
                .send(tokio_tungstenite::tungstenite::Message::Binary(bytes))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Either direction ending ends the visit, and the other task is **aborted**
    // rather than left running: it holds a connection to the node, which holds
    // the guest's serial line, which QEMU hands to one peer at a time. See the
    // same note in the node's own relay, where leaving it running made the
    // second console onto any guest fail.
    tokio::select! {
        _ = &mut downstream => upstream.abort(),
        _ = &mut upstream => downstream.abort(),
    }
    Ok(())
}
