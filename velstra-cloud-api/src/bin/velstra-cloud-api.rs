//! One process, one cell, both surfaces.
//!
//! The store is in memory for now, which is a real store with real MVCC
//! semantics rather than a mock — the same one a development cell runs on. The
//! day this points at etcd instead, it is this file that changes and nothing
//! above it.

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use velstra_cloud_api::{StaticTokenVerifier, TokenVerifier};

#[derive(Parser, Debug)]
#[command(
    name = "velstra-cloud-api",
    about = "The Velstra Cloud control plane API"
)]
struct Args {
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
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
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

    let api = velstra_cloud_api::in_memory(&args.region, &args.cell, verifier);
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
