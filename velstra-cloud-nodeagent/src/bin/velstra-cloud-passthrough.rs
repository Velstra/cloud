//! Handing a PCI device to `vfio-pci`, so a guest can be given it.
//!
//! ## Why this is not a kernel parameter
//!
//! The usual recipe is `vfio-pci.ids=10de:2204` on the kernel command line.
//! That cannot work on the appliance: its command line is sealed into a signed
//! Unified Kernel Image — it carries `usrhash=` and is what the signature
//! covers — so putting device ids there would make *which cards this machine
//! passes through* a fact about the image. An image is built once for a fleet
//! and installed onto boxes with different cards in them, weeks apart.
//!
//! So it is the seed, like every other per-machine fact, and the binding
//! happens at boot through sysfs. `driver_override` plus `drivers_probe` is
//! the kernel's own supported path for exactly this, and it has the property
//! the parameter does not: it takes a card away from a host driver that has
//! already claimed it — which is what happens on a machine where `nouveau`
//! bound the GPU three seconds before this ran.
//!
//! ## Why a binary of its own
//!
//! It runs once, as root, before the agent — not on a reconcile pass. The
//! agent observes devices and decides nothing about them, and a loop that
//! rebound hardware every thirty seconds would be a loop that can take a card
//! from a running guest. This is the deliberate, one-shot half.
//!
//! The IOMMU group arithmetic is in `velstra_cloud_model::pci::to_bind`, which
//! is pure and tested; everything here writes to sysfs and picks nothing.

use std::path::{Path, PathBuf};

use clap::Parser;

/// What every failure here is, in the convention this crate's other binaries
/// use: no `anyhow`, because the node agent does not depend on it.
type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Parser)]
#[command(
    name = "velstra-cloud-passthrough",
    about = "Bind the PCI devices this node reserves to vfio-pci, so guests can be given them"
)]
struct Args {
    /// What to reserve: PCI addresses (`0000:41:00.0`) or vendor:device pairs
    /// (`10de:2204`), comma separated.
    ///
    /// The pair is the useful spelling — an address is a fact about one slot
    /// in one box, and a fleet is installed from one answer. It matches every
    /// card of that model in the machine.
    #[arg(long, env = "VELSTRA_PASSTHROUGH", value_delimiter = ',')]
    device: Vec<String>,

    /// Where the PCI bus is. A flag so a test can point somewhere else.
    #[arg(long, default_value = "/sys/bus/pci")]
    bus: PathBuf,
}

fn main() -> Fallible<()> {
    let args = Args::parse();
    let wanted: Vec<String> = args
        .device
        .iter()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    if wanted.is_empty() {
        println!("no devices reserved for passthrough");
        return Ok(());
    }

    let devices = velstra_cloud_nodeagent::pcidev::observe(&Default::default());
    let plan = velstra_cloud_model::pci::to_bind(&wanted, &devices);

    // Said out loud, never swallowed: a seed written for one model of box and
    // flashed onto another is the ordinary way a device goes missing, and a
    // silent skip turns it into a guest that will not schedule for a reason
    // nobody can see.
    for unknown in &plan.unknown {
        println!("  {unknown}: not in this machine — nothing to bind");
    }
    for (address, why) in &plan.refused {
        println!("  {address}: {why}");
    }

    // Never fatal. A node whose card is busy should come up and report the
    // device as held — that is something an operator can read in the console —
    // rather than refuse to boot over one slot.
    for address in &plan.bind {
        match bind_one(&args.bus, address) {
            Ok(true) => println!("  {address}: bound to vfio-pci"),
            Ok(false) => println!("  {address}: already on vfio-pci"),
            Err(e) => println!("  {address}: {e:#}"),
        }
    }
    Ok(())
}

/// Move one device onto `vfio-pci`. `Ok(false)` when it was already there.
///
/// The order is the kernel's and not negotiable: override first, then unbind,
/// then probe. Unbinding before the override is set hands the device straight
/// back to the driver that had it, which looks like the write did nothing.
fn bind_one(bus: &Path, address: &str) -> Fallible<bool> {
    let device = bus.join("devices").join(address);
    if current_driver(&device).as_deref() == Some("vfio-pci") {
        return Ok(false);
    }
    std::fs::write(device.join("driver_override"), "vfio-pci\n")
        .map_err(|e| format!("claiming {address} for vfio-pci: {e}"))?;
    if let Some(driver) = current_driver(&device) {
        // Best-effort: a device with no driver has nothing to unbind from, and
        // one whose driver refuses is caught by the check after the probe.
        let _ = std::fs::write(
            bus.join("drivers").join(&driver).join("unbind"),
            format!("{address}\n"),
        );
    }
    std::fs::write(bus.join("drivers_probe"), format!("{address}\n"))
        .map_err(|e| format!("probing {address} after the override: {e}"))?;
    match current_driver(&device).as_deref() {
        Some("vfio-pci") => Ok(true),
        Some(other) => Err(format!("the kernel left it on {other} — is vfio-pci loaded?").into()),
        None => Err("the kernel bound nothing — is vfio-pci loaded?".into()),
    }
}

/// The driver currently bound, by name.
fn current_driver(device: &Path) -> Option<String> {
    std::fs::read_link(device.join("driver"))
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_string)
}
