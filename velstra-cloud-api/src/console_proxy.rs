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

/// Relay a client's websocket to a node's, until either end stops.
pub async fn relay(client: WebSocket, node_url: String) -> Result<(), String> {
    let (node, _) = tokio_tungstenite::connect_async(&node_url)
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
