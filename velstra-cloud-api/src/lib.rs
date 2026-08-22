//! The API server: one set of handlers, served twice.
//!
//! gRPC is the native surface and REST is the JSON gateway over it, but neither
//! is implemented in terms of the other — they are two renderings of
//! [`core::Api`], which is where every rule lives. That arrangement is the
//! whole design of this crate, because the alternative has been tried
//! everywhere: a REST path that validates one field more than its gRPC twin,
//! discovered by a customer whose SDK writes something a console refuses.
//!
//! Read [`core`] first. [`rest`] and [`grpc`] are deliberately dull.

pub mod auth;
pub mod collection;
pub mod core;
pub mod error;
pub mod grpc;
/// The JSON the contract promises, from the model as it is.
///
/// Moved out of this crate so a node agent can speak the same wire without
/// linking an API server: the shaping is pure, and the two ends of a contract
/// should not have two implementations of it.
pub use velstra_cloud_wire as json;
pub mod paging;
pub mod proxy;
pub mod refs;
pub mod rest;
pub mod served;
pub mod sessions;

pub use core::{Api, COLLECTIONS, Filter, WatchEvent};
use std::sync::Arc;

pub use auth::{Identity, StaticTokenVerifier, TokenVerifier};
use axum::Router;
pub use error::{ApiError, ApiResult, Code};
use velstra_cloud_store::Store;

/// Everything this process serves, on one listener: the JSON gateway under
/// `/api/v1/` and the gRPC services at their own paths.
///
/// One port rather than two because they are one API. A client that has to be
/// told which port speaks which dialect will eventually be told wrong.
pub fn server(api: Api) -> Router {
    grpc::services(api.clone()).merge(rest::router(api))
}

/// The same server, with a routing hop in front of it.
///
/// A request for a resource this cell does not own is forwarded to the cell that
/// does, rather than answered here. Everything else — including every request in
/// a single-cell installation, where the router has no opinion — reaches exactly
/// the handlers [`server`] serves.
///
/// Separate from `server` rather than a parameter on it because the two have
/// different dependencies: this one needs to know where the other cells are, and
/// a single-cell deployment should not have to say "no cells" to get a working
/// API.
pub fn server_routed(api: Api, router: proxy::Router) -> Router {
    server(api).layer(axum::middleware::from_fn_with_state(router, proxy::route))
}

/// The development arrangement: an in-memory store, one cell, one static token.
pub fn in_memory(region: &str, cell: &str, verifier: Arc<dyn TokenVerifier>) -> Api {
    let store: Arc<dyn Store> = Arc::new(velstra_cloud_store::MemoryStore::new());
    Api::new(store, region, cell, verifier)
}
