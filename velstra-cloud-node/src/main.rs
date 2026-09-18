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

mod announce;
mod cell;
mod disks;
mod install;
mod joinfile;
mod product;
mod quickstart;
mod roles;
mod seed;
mod setup;
mod tls;
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
    /// Bring an existing seed up to what this package needs.
    ///
    /// Run by the package's `postinst` on every upgrade, and safe to run by
    /// hand. It changes only what is provably safe and says what it changed.
    MigrateSeed {
        #[arg(long, default_value = "/var/lib/velstra")]
        dir: std::path::PathBuf,
        /// Where this machine keeps who it is. Separate from the state
        /// directory because that one may be shared with other machines.
        #[arg(long, default_value = "/etc/velstra")]
        identity: std::path::PathBuf,
    },

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
        /// Join a cell with the token its console showed when the node was
        /// created. One string, everything in it: cell, address, certificate,
        /// credential. Nothing else is asked. See docs/joining.md.
        #[arg(long, conflicts_with = "config")]
        join: Option<String>,
        /// Read the join token from this file instead of the command line.
        ///
        /// A token on a command line is in `ps` for every user on the machine
        /// and in the shell's history afterwards. This is also the shape
        /// configuration management wants: write the file, run the command.
        /// The file may hold a comment above the token.
        #[arg(long, conflicts_with = "join")]
        join_file: Option<PathBuf>,
    },
    /// One box, one command: seed, units, and the two objects a cell needs.
    ///
    /// `setup` answers "what is this machine". This answers that and then does
    /// the rest — create the node, move its one-time token, create the pool,
    /// bring the agents up — because none of that is hard and all of it is
    /// where somebody trying this for the first time gives up.
    ///
    /// Safe to run again: nothing here is created twice.
    Quickstart {
        /// Where the seed goes.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// What the API binds, e.g. `0.0.0.0:8443`. Asked when left out.
        #[arg(long)]
        listen: Option<String>,
        /// A name for this machine. Asked when left out.
        #[arg(long)]
        node: Option<String>,
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
    /// First boot of a new cell's first machine, part one: make this
    /// machine's certificate with the addresses it actually has, and tell the
    /// seed where it is and what to advertise. Runs before the API; a no-op
    /// once the certificate exists.
    EnsureTls {
        #[arg(long, default_value = "/var/lib/velstra")]
        dir: PathBuf,
    },
    /// First boot of a new cell's first machine, part two: the Node and Pool
    /// objects this machine is, and their credentials. Runs after the API
    /// answers; every step is idempotent. What `quickstart` does after it has
    /// written the seed, for a machine that was flashed rather than set up.
    BootstrapCell {
        #[arg(long, default_value = "/var/lib/velstra")]
        dir: PathBuf,
    },
    /// Apply what the seed says about logging in to this machine: the root
    /// password, the SSH key, and closing both when it says nothing. Runs
    /// every boot, because `/etc` here does not survive one.
    ApplyAccess {
        #[arg(long, default_value = "/var/lib/velstra")]
        dir: PathBuf,
    },
    /// Write the console banner: this machine's name, its addresses, what it
    /// runs, and where its console is.
    Banner {
        #[arg(long, default_value = "/var/lib/velstra")]
        dir: PathBuf,
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
        Cmd::MigrateSeed { dir, identity } => {
            for said in setup::migrate_seed(&dir, &identity)? {
                println!("  · {said}");
            }
            Ok(())
        }
        Cmd::Install { source } => install::run_install(source),
        Cmd::Setup {
            dir,
            nixos,
            config,
            join,
            join_file,
        } => setup::run_with(dir, nixos, config, join, join_file),
        Cmd::Quickstart { dir, listen, node } => quickstart::run(dir, listen, node),
        Cmd::HasRole { role } => roles::has_role_or_exit(&role),
        Cmd::EnsureTls { dir } => cell::ensure_tls(&dir),
        Cmd::BootstrapCell { dir } => cell::bootstrap(&dir),
        Cmd::ApplyAccess { dir } => cell::apply_access(&dir),
        Cmd::Banner { dir } => cell::banner(&dir),
        Cmd::Unlock => unlock::run(),
        Cmd::Update { image } => update::run_update(&image),
    }
}
