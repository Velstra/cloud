//! `velstra-cloud-node`: the compute node's installer, unlocker and updater.
//!
//! One binary, three jobs, all on the same disk layout the Nix image factory
//! produces (identical to the Sentinel appliance's — the logic here is a port
//! of that installer's proven flow, with the product names swapped via
//! [`product`]):
//!
//!   * `install` — the interactive text wizard that writes the verified image
//!     onto internal storage and seeds the node's identity;
//!   * `unlock` — the boot-time LUKS unlock, a no-op on plaintext installs;
//!   * `update` — the A/B slot update from a local image file.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod disks;
mod install;
mod product;
mod roles;
mod seed;
mod setup;
mod unlock;
mod update;
mod wizard;

#[derive(Debug, Parser)]
#[command(
    name = "velstra-cloud-node",
    about = "Install, unlock and update a Velstra compute node"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Install the node image onto internal storage (interactive wizard).
    Install {
        /// A raw node image to clone from, overriding
        /// $VELSTRA_NODE_INSTALL_SOURCE; left out entirely, the booted medium
        /// itself is the source.
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Set up a machine that already has an operating system: ask which cell
    /// and as what, and write the seed. Touches no disks and no packages.
    Setup {
        /// Where the seed goes. The default is the one path every kind of
        /// machine uses, appliance included.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Assume NixOS (print the module snippet) or not (print the units to
        /// enable). Detected from `/etc/NIXOS` when left out.
        #[arg(long)]
        nixos: Option<bool>,
        /// Take the answers from this file instead of asking.
        ///
        /// The file **is a seed**: the same `KEY=value` lines this writes, so
        /// an operator can take one off a working machine, change two lines,
        /// and install the next one with it. A missing answer is an error
        /// naming the key — an unattended install that guessed a cell name
        /// would make a machine that registers nowhere and is found weeks
        /// later.
        ///
        /// `VELSTRA_TOKEN` in the environment fills in the one answer a file
        /// should not have to carry.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Exit 0 when this machine's seed names `role`, non-zero otherwise.
    ///
    /// What every unit's `ExecCondition` runs. A non-zero answer means "not
    /// for this machine", which systemd shows as a unit that was skipped
    /// rather than one that failed — the difference between "this box is not a
    /// pool" and "the pool agent is broken".
    HasRole {
        /// `control-plane`, `hypervisor` or `pool`.
        role: String,
    },
    /// Open the encrypted data volume at boot (a no-op on a plaintext
    /// install).
    Unlock,
    /// Write a new image into the inactive A/B slot and make it the boot
    /// default.
    Update {
        /// The raw node image (or block device) to install into the inactive
        /// slot.
        #[arg(long)]
        image: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Install { source } => install::run_install(source),
        Cmd::Setup { dir, nixos, config } => setup::run_with(dir, nixos, config),
        Cmd::HasRole { role } => roles::has_role_or_exit(&role),
        Cmd::Unlock => unlock::run(),
        Cmd::Update { image } => update::run_update(&image),
    }
}
