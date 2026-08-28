//! The node's end of a console: a socket, and a check before anybody reaches it.
//!
//! ## What is on either side
//!
//! On one side, a unix socket QEMU is listening on — the guest's serial line,
//! set up in [`crate::qemu`]. On the other, a websocket the **API** opens; never
//! a browser, because a tenant's browser has no business on the machines a cell
//! is made of. The API has already decided who may be here and whether they may
//! type; this service decides only whether the connection in front of it is
//! carrying a grant that is real, unspent, unexpired and about a guest on *this*
//! machine.
//!
//! ## Why the check reads an object
//!
//! The alternative was a shared secret or a signing key, and both are new
//! machinery for a question this platform already answers with objects: the API
//! writes a session, the node already reads the cell, and the ticket is checked
//! against what it finds. Nothing new is trusted, and who opened a console into
//! which guest is on the record — see [`velstra_cloud_model::console`], where
//! the rule itself lives as a pure function.
//!
//! ## Read-only is enforced here and decided elsewhere
//!
//! A viewer's session says `read_only`, and what this service does with it is
//! **drop what they type**. Refusing the connection would be worse: somebody who
//! may read a machine should be able to watch it fail to boot, which is what a
//! console is for. And the decision is not re-made here — a node has no
//! bindings to read and must never be the place a permission question is
//! answered.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use velstra_cloud_model::{
    console::{Refused, admits},
    meta::Timestamp,
};

use crate::cell::CellReader;

/// What this node needs to answer an attach.
#[derive(Clone)]
pub struct Consoles {
    /// This node's own name, which is what a session has to name for this
    /// machine to serve it.
    pub node: String,
    /// Where the cell's sessions are read from — the same reader the rest of
    /// the agent uses.
    pub cell: Arc<dyn CellReader>,
    /// Where a guest's serial socket lives on disk.
    pub layout: crate::hostfs::Layout,
    /// Somewhere to say that a ticket was spent, so a second connection
    /// presenting it is refused.
    pub sink: Arc<dyn Attachments>,
}

/// Saying that a session has been attached to — and later, released.
///
/// A trait so the check can be exercised without a store: the interesting part
/// is that it is *said*, and a test that could not observe the saying would be
/// testing nothing.
#[async_trait::async_trait]
pub trait Attachments: Send + Sync + 'static {
    async fn attached(&self, session: &str, at: Timestamp) -> crate::host::Result<()>;
    async fn detached(&self, session: &str, at: Timestamp) -> crate::host::Result<()>;
}

#[derive(Debug, Deserialize)]
pub struct Attach {
    /// The session's resource name, so this service knows which grant to check
    /// rather than searching every one it can see for a matching hash.
    pub session: String,
    pub ticket: String,
}

pub fn router(consoles: Consoles) -> Router {
    Router::new()
        .route("/console", get(attach))
        .with_state(consoles)
}

/// Bind and serve, answering with the address actually bound.
///
/// The bound address is what gets reported on the node's status: a node that
/// asked for port 0, or that could not have the port it wanted, must advertise
/// what it got rather than what it asked for.
pub async fn serve(
    listen: SocketAddr,
    consoles: Consoles,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    let app = router(consoles);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "the console service stopped");
        }
    });
    Ok((bound, task))
}

async fn attach(
    State(consoles): State<Consoles>,
    Query(query): Query<Attach>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let sessions = match consoles.cell.console_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            // Said out loud: a node that cannot read the cell refuses every
            // console, and "the button does nothing" is the worst way to learn
            // that a node lost its credentials.
            tracing::error!(error = %e, "could not read this cell's console sessions");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "this node cannot read the cell just now",
            )
                .into_response();
        }
    };
    let Some(session) = sessions
        .iter()
        .find(|s| s.meta.name.to_string() == query.session)
    else {
        // The same answer as a wrong ticket, deliberately: a caller must not be
        // able to learn which sessions exist by asking.
        tracing::debug!(session = %query.session, "no such console session");
        return refusal();
    };

    if let Err(why) = admits(
        &session.spec,
        &session.status,
        &query.ticket,
        &consoles.node,
        Timestamp::now(),
    ) {
        // Logged with the reason, because a journal full of these is how an
        // operator tells "somebody's browser was slow" from "somebody is
        // guessing tickets" — and answered with the same sentence either way.
        tracing::info!(session = %query.session, why = %why, "console attach refused");
        let _: Refused = why;
        return refusal();
    }

    let socket = consoles.layout.console_socket(&session.spec.instance);
    let stream = match tokio::net::UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(e) => {
            // Two different failures wearing one error, and telling them apart
            // is what somebody staring at a console that will not open needs.
            // `EAGAIN` from a chardev socket means the guest is running and
            // somebody else is already attached — QEMU accepts one peer — and
            // saying "this guest has no console" there sends the reader to look
            // at the guest, which is fine.
            let busy = e.kind() == std::io::ErrorKind::WouldBlock;
            tracing::warn!(
                instance = %session.spec.instance,
                error = %e,
                busy,
                "could not attach to this guest's serial line"
            );
            return (
                StatusCode::CONFLICT,
                if busy {
                    "another console is already attached to this guest; a serial line carries one \
                     at a time"
                } else {
                    "this guest is not running, so there is no console to attach to"
                },
            )
                .into_response();
        }
    };

    // Spent *before* the connection is upgraded, so two connections racing the
    // same ticket cannot both be told yes.
    let name = session.meta.name.to_string();
    if let Err(e) = consoles.sink.attached(&name, Timestamp::now()).await {
        tracing::error!(session = %name, error = %e, "could not record the attach");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this node could not record the attach, so it will not serve it",
        )
            .into_response();
    }

    let read_only = session.spec.read_only;
    let sink = consoles.sink.clone();
    upgrade.on_upgrade(move |socket| async move {
        relay(socket, stream, read_only).await;
        if let Err(e) = sink.detached(&name, Timestamp::now()).await {
            tracing::warn!(session = %name, error = %e, "could not record the detach");
        }
    })
}

/// The same answer for every reason, so asking is not a way to find out.
fn refusal() -> Response {
    (
        StatusCode::FORBIDDEN,
        "that is not a console this node will open",
    )
        .into_response()
}

/// Bytes both ways, until either end stops.
///
/// Binary frames, not text: a serial line carries whatever the guest writes,
/// including bytes that are not UTF-8, and a relay that insisted otherwise
/// would drop exactly the output somebody attaches a console to read.
async fn relay(socket: WebSocket, stream: tokio::net::UnixStream, read_only: bool) {
    use futures::{SinkExt, StreamExt};

    let (mut to_client, mut from_client) = socket.split();
    let (mut from_guest, mut to_guest) = stream.into_split();

    let mut downstream = tokio::spawn(async move {
        let mut buffer = vec![0u8; 4096];
        loop {
            match from_guest.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_client
                        .send(Message::Binary(buffer[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let mut upstream = tokio::spawn(async move {
        while let Some(Ok(message)) = from_client.next().await {
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Text(text) => text.into_bytes(),
                Message::Close(_) => break,
                // Ping and pong are the transport's business; axum answers them.
                _ => continue,
            };
            // A viewer's keystrokes are dropped here rather than refused at the
            // door. Watching a guest fail to boot is reading it, and somebody
            // who may read it should be able to.
            if read_only {
                continue;
            }
            if to_guest.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // Either direction ending ends the visit, and the **other task has to be
    // aborted** rather than left to finish on its own.
    //
    // That last part is the whole of it. `select!` on two join handles returns
    // when the first finishes and leaves the second running — still holding its
    // half of the unix socket. QEMU's chardev accepts one peer at a time, so
    // the next attach to that guest gets `EAGAIN` and the node reports, in
    // perfectly good faith, that the guest has no serial socket. The first
    // console worked and every one after it failed until the guest was
    // restarted.
    tokio::select! {
        _ = &mut downstream => upstream.abort(),
        _ = &mut upstream => downstream.abort(),
    }
}

/// Recording an attach the way this node records everything else: as a status
/// on the object, through the sink the agent already has.
///
/// The node owns a session's status because the session names it — the same
/// assignment rule that lets a node report on the guests it was given. Nobody
/// else writes it, which is what makes "already attached" a fact rather than an
/// opinion.
pub struct SessionStatus {
    pub sink: Arc<dyn crate::sink::StatusSink>,
    pub node: String,
}

#[async_trait::async_trait]
impl Attachments for SessionStatus {
    async fn attached(&self, session: &str, at: Timestamp) -> crate::host::Result<()> {
        self.report(
            session,
            serde_json::json!({ "attachedAt": at.0, "node": self.node }),
        )
        .await
    }

    async fn detached(&self, session: &str, at: Timestamp) -> crate::host::Result<()> {
        self.report(session, serde_json::json!({ "detachedAt": at.0 }))
            .await
    }
}

impl SessionStatus {
    async fn report(&self, session: &str, status: serde_json::Value) -> crate::host::Result<()> {
        let document = serde_json::json!({
            "meta": { "name": session },
            "status": status,
        });
        let writer = velstra_cloud_model::access::Writer::agent(&self.node);
        match self
            .sink
            .write_status("console-sessions", &document, &writer)
            .await
        {
            crate::sink::SinkOutcome::Wrote => Ok(()),
            outcome => Err(crate::host::HostError::failed(format!(
                "reporting on {session}: {outcome:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        console::{ConsoleSessionSpec, ConsoleSessionStatus, TICKET_LIFETIME_MS, sha256_hex},
        meta::{Meta, Placement},
        resources::ConsoleSession,
    };

    use super::*;

    fn session(node: &str, ticket: &str, read_only: bool) -> ConsoleSession {
        ConsoleSession::new(
            Meta::new(
                "projects/p/console-sessions/c1".parse().unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            ConsoleSessionSpec {
                instance: "projects/p/instances/i1".into(),
                node: node.into(),
                subject: "ada".into(),
                ticket_sha256: sha256_hex(ticket),
                expires_at: Timestamp(Timestamp::now().0 + TICKET_LIFETIME_MS),
                read_only,
            },
            ConsoleSessionStatus::default(),
        )
    }

    /// A ticket is a fact about one machine. A node serving a session for a
    /// guest on another would be opening a console it has no business opening —
    /// and would find no socket anyway, which is the kind of "it does not work"
    /// that hides a permission mistake instead of showing one.
    #[test]
    fn the_gate_is_the_models_rule_and_not_a_second_copy_of_it() {
        let ticket = "s3cret";
        let session = session("node-b", ticket, false);
        assert_eq!(
            velstra_cloud_model::console::admits(
                &session.spec,
                &session.status,
                ticket,
                "node-a",
                Timestamp::now(),
            ),
            Err(velstra_cloud_model::console::Refused::AnotherNode)
        );
    }

    /// The whole reason this service exists on the node rather than in the API:
    /// what it opens is a path on **this** disk, derived from the guest the
    /// session names and from nothing the caller sent.
    #[test]
    fn the_socket_opened_comes_from_the_session_and_not_from_the_request() {
        let layout = crate::hostfs::Layout {
            run_dir: "/var/lib/velstra/instances".into(),
            image_dir: "/var/lib/velstra/images".into(),
            incoming_dir: "/var/lib/velstra/images/incoming".into(),
            slice: "velstra".into(),
            binary: "/usr/bin/qemu-system-x86_64".into(),
            ..Default::default()
        };
        let session = session("node-a", "s3cret", false);
        let socket = layout.console_socket(&session.spec.instance);
        assert!(
            socket.starts_with("/var/lib/velstra/instances"),
            "{socket:?}"
        );
        assert!(socket.ends_with("console.sock"), "{socket:?}");
        // Nothing a caller sends names a path: a session's instance does, and
        // the instance came from the API.
        assert!(
            !socket.to_string_lossy().contains(".."),
            "a path escaped its directory: {socket:?}"
        );
    }
}
