//! One process, one cell, both surfaces.
//!
//! The store is in memory for now, which is a real store with real MVCC
//! semantics rather than a mock — the same one a development cell runs on. The
//! day this points at etcd instead, it is this file that changes and nothing
//! above it.

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use velstra_cloud_api::{StaticTokenVerifier, TokenVerifier};
use velstra_cloud_store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "velstra-cloud-api",
    about = "The Velstra Cloud control plane API"
)]
struct Args {
    /// Where the state lives: `memory`, or one or more etcd endpoints.
    ///
    /// `memory` serves an API that nothing else can see into. That is a demo,
    /// not a cell: an instance created against it exists only here, and the
    /// controller that would place it is looking at a different world.
    #[arg(long, env = "VELSTRA_STORE", default_value = "memory")]
    store: String,

    /// Where to listen. REST under /api/v1/ and gRPC share it.
    #[arg(long, default_value = "127.0.0.1:8443", env = "VELSTRA_LISTEN")]
    listen: String,

    /// Bearer tokens, one per line, optionally `token subject`.
    ///
    /// This is the development verifier. In production the token is an
    /// OIDC-issued JWT and the only thing that changes is which
    /// implementation of the trait is constructed here.
    #[arg(long, env = "VELSTRA_TOKEN_FILE")]
    token_file: PathBuf,

    /// A subject that may do anything anywhere in this cell. Repeatable.
    ///
    /// Configuration rather than data, and deliberately: it is what a fresh cell
    /// is bootstrapped from, and a permission stored inside the thing it
    /// protects has no answer for the first request. Everything else is decided
    /// by the bindings on the project a request touches.
    #[arg(long)]
    cell_admin: Vec<String>,

    /// The cell this API serves. Every object it writes is stamped with it, and
    /// a cell is the failure domain — one going down must not take a region
    /// with it.
    #[arg(long, default_value = "cell-1", env = "VELSTRA_CELL")]
    cell: String,

    #[arg(long, default_value = "eu-central", env = "VELSTRA_REGION")]
    region: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // With no fallback this server logged *nothing* unless RUST_LOG was
        // set — not even which store it had opened or that it was listening.
        // A process that starts silently is one you debug by guessing.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let contents = std::fs::read_to_string(&args.token_file).map_err(|e| {
        anyhow::anyhow!(
            "the token file {} could not be read: {e}",
            args.token_file.display()
        )
    })?;
    let verifier: Arc<dyn TokenVerifier> = Arc::new(
        StaticTokenVerifier::from_file_contents(&contents).map_err(|e| anyhow::anyhow!("{e}"))?,
    );

    let store: Arc<dyn Store> = if args.store == "memory" {
        tracing::warn!(
            "using the in-memory store: objects created here are visible to no \
             other process, and go when this one does"
        );
        Arc::new(velstra_cloud_store::MemoryStore::new())
    } else {
        let endpoints: Vec<&str> = args.store.split(',').map(str::trim).collect();
        let store = velstra_cloud_store::EtcdStore::connect(&endpoints).await?;
        tracing::info!(endpoints = %args.store, "state store connected");
        Arc::new(store)
    };
    if args.cell_admin.is_empty() {
        // Said out loud rather than discovered: a cell with no operator can
        // still serve every tenant whose project grants them, but nobody can
        // register a node, create a project, or repair either.
        tracing::warn!(
            "no --cell-admin given: nothing in this cell may register a node, create a project, \
             or read anything outside one"
        );
    }
    let api = velstra_cloud_api::Api::new(store, &args.region, &args.cell, verifier)
        .with_cell_admins(args.cell_admin.clone());
    // One watch on the store per assigned collection, from here on, however many
    // node agents ask. Started by the server and not by `Api::new`, so a test or
    // a one-shot tool that builds an Api pays for none of it.
    api.serve_agents();
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!(
        listen = %args.listen,
        region = %args.region,
        cell = %args.cell,
        "serving REST under /api/v1/ and gRPC on the same port"
    );
    axum::serve(listener, velstra_cloud_api::server(api)).await?;
    Ok(())
}
