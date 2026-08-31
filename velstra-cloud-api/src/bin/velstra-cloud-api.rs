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

    /// Where hourly snapshots of the store go; empty takes none.
    ///
    /// The guests survive their control plane dying — a cell whose store is
    /// gone is a cell nobody will ever manage again, which is why this exists.
    /// Point it at storage that is not this machine's own disk, and restore
    /// with `etcdctl snapshot restore` (see docs/operations.md).
    #[arg(long, env = "VELSTRA_STORE_BACKUP_DIR", default_value = "")]
    store_backup_dir: String,

    /// The certificate and key to serve TLS with, as PEM files.
    ///
    /// Both or neither. Without them this port is plaintext and says so in the
    /// log at startup — which used to be the only option, with "put a reverse
    /// proxy in front" as the documented answer. That is right for a cluster and
    /// wrong for the box that is the whole cell: it has nothing in front of it,
    /// and an administrator's password crossed the wire in the clear while the
    /// URL said 8443 and looked like it would not.
    ///
    /// `quickstart` makes a self-signed pair and points these at it. Replacing
    /// it with a real certificate is two file copies and a restart.
    #[arg(long, env = "VELSTRA_TLS_CERT")]
    tls_cert: Option<String>,

    #[arg(long, env = "VELSTRA_TLS_KEY")]
    tls_key: Option<String>,

    /// Bearer tokens, one per line, optionally `token subject`.
    ///
    /// Service accounts and automation. People sign in with a password and get a
    /// session; a daemon holds one of these, because issuing a session to a
    /// daemon means something has to renew it. Optional: a cell where only
    /// people sign in needs no token file at all.
    #[arg(long, env = "VELSTRA_TOKEN_FILE")]
    token_file: Option<PathBuf>,

    /// Create this administrator if — and only if — the cell has no users yet.
    ///
    /// A cell with no users is a cell nobody can sign into, so an installation
    /// has to be able to make the first one. The guard is "no users at all", not
    /// "this user is missing": re-running against a populated cell must never
    /// resurrect a deleted administrator or reset a live one's password, which
    /// would be an unauthenticated way back in for anyone who can restart this
    /// process.
    #[arg(long, env = "VELSTRA_BOOTSTRAP_ADMIN")]
    bootstrap_admin: Option<String>,

    /// The password for `--bootstrap-admin`.
    ///
    /// Read from the environment rather than typed on a command line wherever
    /// possible: an argument is visible in `ps` to every user on the machine,
    /// and this one is the cell's first administrator.
    #[arg(long, env = "VELSTRA_BOOTSTRAP_PASSWORD", hide = true)]
    bootstrap_password: Option<String>,

    /// A subject that may do anything anywhere in this cell. Repeatable.
    ///
    /// Configuration rather than data, and deliberately: it is what a fresh cell
    /// is bootstrapped from, and a permission stored inside the thing it
    /// protects has no answer for the first request. Everything else is decided
    /// by the bindings on the project a request touches.
    #[arg(long)]
    cell_admin: Vec<String>,

    /// Another cell this installation can reach: `cell-2=https://cell-2:8443`.
    /// Repeatable, and `VELSTRA_CELLS` takes the same pairs comma-separated.
    ///
    /// A cell is the failure and scaling domain, so growing means adding cells —
    /// and that only works if a client can reach **one** address and have the
    /// request land in the cell holding the resource. With no pairs this
    /// process answers everything itself, which is what every single-cell
    /// installation wants and costs nothing.
    ///
    /// Which cell owns what is read from the projects collection, not from
    /// this list: the list says only where each cell *is*. A project this
    /// installation has not heard of yet is answered here rather than refused —
    /// a router a few seconds behind must not turn propagation delay into an
    /// error a tenant sees.
    #[arg(long = "cell-endpoint", env = "VELSTRA_CELLS", value_delimiter = ',')]
    cell_endpoint: Vec<String>,

    /// How many **writes** a second one caller may sustain. `0` — the default —
    /// is no limit.
    ///
    /// What it stops is the ordinary accident: a script in a loop, a controller
    /// somebody wrote with no backoff, taking the cell's write path from
    /// everybody else. It is not a security boundary and does not pretend to be
    /// one. Reads are never counted, and node agents are never limited — an
    /// agent reports when something changed, and something changing is not
    /// something it can defer.
    #[arg(long, default_value_t = 0, env = "VELSTRA_WRITES_PER_SECOND")]
    writes_per_second: u32,

    /// How many writes one caller may spend at once after being quiet.
    ///
    /// Defaults to ten seconds' worth. Creating twenty guests in a moment is a
    /// normal Tuesday, and a limiter that refuses it is one people route
    /// around.
    #[arg(long, env = "VELSTRA_WRITE_BURST")]
    write_burst: Option<u32>,

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

    let static_tokens: Option<Arc<dyn TokenVerifier>> = match &args.token_file {
        Some(path) => {
            let contents = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!("the token file {} could not be read: {e}", path.display())
            })?;
            Some(Arc::new(
                StaticTokenVerifier::from_file_contents(&contents)
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            ))
        }
        None => None,
    };

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
    // Kept for the cross-cell router below, which watches the projects to learn
    // which cell owns what. Cloned rather than re-connected: two connections to
    // one etcd for one process is one too many.
    let routing_store = store.clone();
    if args.cell_admin.is_empty() && args.bootstrap_admin.is_none() {
        // Said out loud rather than discovered: a cell with no operator can
        // still serve every tenant whose project grants them, but nobody can
        // register a node, create a project, or repair either. A stored
        // administrator counts too, which is why this is not fatal — the cell may
        // already have one from an earlier start.
        tracing::warn!(
            "no --cell-admin and no --bootstrap-admin: unless this cell already holds an \
             administrator, nothing in it may register a node or create a project"
        );
    }
    // Sessions first, static tokens second. A person signs in and holds a
    // session; a daemon holds a token; the API cannot tell them apart afterwards
    // and does not need to.
    let identity =
        velstra_cloud_api::sessions::IdentityStore::new(store.clone(), &args.region, &args.cell);
    let mut sessions = velstra_cloud_api::sessions::StoreTokenVerifier::new(identity.clone());
    if let Some(tokens) = static_tokens {
        sessions = sessions.with_fallback(tokens);
    }
    let verifier: Arc<dyn TokenVerifier> = Arc::new(sessions);

    match (&args.bootstrap_admin, &args.bootstrap_password) {
        (Some(user), Some(password)) => {
            if identity.bootstrap_admin(user, password).await? {
                tracing::info!(user, "created the first administrator");
            } else {
                tracing::info!(
                    "this cell already has users; --bootstrap-admin did nothing, which is \
                     what keeps it from being a way back in"
                );
            }
        }
        (Some(_), None) => {
            anyhow::bail!(
                "--bootstrap-admin needs --bootstrap-password (or VELSTRA_BOOTSTRAP_PASSWORD)"
            )
        }
        (None, Some(_)) => {
            anyhow::bail!("--bootstrap-password was given with no --bootstrap-admin to use it")
        }
        (None, None) => {}
    }

    let mut api = velstra_cloud_api::Api::new(store, &args.region, &args.cell, verifier)
        .with_cell_admins(args.cell_admin.clone());
    if !args.store_backup_dir.is_empty() {
        api = api.with_store_backups(std::path::PathBuf::from(&args.store_backup_dir));
    }
    if args.writes_per_second > 0 {
        let rate = velstra_cloud_model::limit::Rate {
            per_second: args.writes_per_second,
            burst: args.write_burst.unwrap_or(args.writes_per_second * 10),
        };
        tracing::info!(
            per_second = rate.per_second,
            burst = rate.burst,
            "capping how fast one caller may write"
        );
        api = api.with_write_rate(rate);
    }
    let api = api;
    // One watch on the store per assigned collection, from here on, however many
    // node agents ask. Started by the server and not by `Api::new`, so a test or
    // a one-shot tool that builds an Api pays for none of it.
    api.serve_agents();
    // Delete expired sessions on a timer. The request path only reaches a token
    // that is presented again; this reaps the ones that were issued and never
    // used once more, which otherwise sit in the store until it is compacted by
    // hand.
    api.spawn_session_sweeper();
    let tls = match (args.tls_cert.as_deref(), args.tls_key.as_deref()) {
        (Some(cert), Some(key)) => Some((cert.to_string(), key.to_string())),
        // One without the other is a mistake worth naming rather than silently
        // serving plaintext on a port somebody believes is encrypted.
        (Some(_), None) => return Err(anyhow::anyhow!("--tls-cert needs --tls-key")),
        (None, Some(_)) => return Err(anyhow::anyhow!("--tls-key needs --tls-cert")),
        (None, None) => None,
    };
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!(
        listen = %args.listen,
        region = %args.region,
        cell = %args.cell,
        "serving REST under /api/v1/ and gRPC on the same port"
    );
    // One address for several cells, when this installation has been told
    // where the others are. `server_routed` existed, was tested, and was
    // reachable from nothing — so a multi-cell installation had no hop and
    // every client had to know which cell held what.
    let served = if args.cell_endpoint.is_empty() {
        velstra_cloud_api::server(api)
    } else {
        let cells = velstra_cloud_api::proxy::Cells::parse(&args.cell_endpoint)
            .map_err(|e| anyhow::anyhow!("--cell-endpoint: {e}"))?;
        tracing::info!(
            cells = args.cell_endpoint.join(","),
            "this address answers for several cells"
        );
        let router = velstra_cloud_api::proxy::Router::new(routing_store, &args.cell, cells);
        velstra_cloud_api::server_routed(api, router)
    };
    match tls {
        None => {
            tracing::warn!(
                "serving plaintext: no --tls-cert. A password crosses this connection in \
                 the clear, so put TLS in front of it before it leaves a network you trust"
            );
            axum::serve(listener, served).await?;
        }
        Some((cert, key)) => {
            // Chosen here rather than left to rustls, which will not choose: with
            // more than one provider compiled in it panics at the first
            // handshake instead of picking, and the panic is at *use* — so an
            // API starts, says "serving https", and dies on the first
            // connection.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let address = listener.local_addr()?;
            drop(listener);
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .map_err(|e| anyhow::anyhow!("reading {cert} and {key}: {e}"))?;
            tracing::info!(cert = %cert, "serving https");
            axum_server::bind_rustls(address, config)
                .serve(served.into_make_service())
                .await?;
        }
    }
    Ok(())
}
