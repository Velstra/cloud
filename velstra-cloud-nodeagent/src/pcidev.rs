//! What PCI devices this machine has, read from sysfs.
//!
//! One node's observation of its own hardware, in the pattern
//! [`crate::devices`] and [`crate::hostcpu`] already follow: this states facts
//! and decides nothing. Whether a device may be handed to a guest is
//! [`velstra_cloud_model::pci::offerable`], which needs the whole list because
//! the answer is about the IOMMU group rather than the device.
//!
//! ## Where the numbers come from
//!
//! `/sys/bus/pci/devices/<address>/`, which the kernel keeps current:
//!
//! * `vendor` / `device` — the `0x10de` / `0x2204` pair, joined into the
//!   `10de:2204` form `lspci -n` prints and a device class matches on.
//! * `class` — a 24-bit code whose top byte is the broad kind.
//! * `driver` — a symlink to whatever is bound, absent when nothing is.
//! * `iommu_group` — a symlink whose basename is the group number, absent on a
//!   machine with no IOMMU.
//!
//! Read rather than shelled out to `lspci`: the file layout is stable kernel
//! ABI, and a node that could only inventory its hardware when a particular
//! userspace tool happened to be installed would report an empty machine on
//! the ones where it is not.
//!
//! ## What is tested
//!
//! The parsing and the group logic, against a directory tree a test builds.
//! What *this* machine happens to contain is asserted nowhere: a test that
//! expected a GPU would pass or fail depending on whose laptop ran it.

use std::path::Path;

use velstra_cloud_model::pci::{DeviceKind, DeviceUse, PciDevice};

/// Where sysfs exposes PCI. A parameter on the inner function so a test can
/// point it at a directory it built.
const SYSFS_PCI: &str = "/sys/bus/pci/devices";

/// Every PCI device this machine has.
///
/// `held` maps a PCI address to the instance holding it, from what the VMM
/// reports about its running guests — the node cannot learn that from sysfs,
/// where a device passed to a guest simply looks bound to `vfio-pci`.
///
/// A machine with no PCI bus at all, or a sysfs this process cannot read,
/// yields an empty list rather than an error: most nodes never pass anything
/// through, and failing a whole observation over an absent directory would
/// take those nodes' guests down for a feature they do not use.
pub fn observe(held: &std::collections::BTreeMap<String, String>) -> Vec<PciDevice> {
    read_from(Path::new(SYSFS_PCI), held)
}

fn read_from(root: &Path, held: &std::collections::BTreeMap<String, String>) -> Vec<PciDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices: Vec<PciDevice> = entries
        .filter_map(Result::ok)
        .filter_map(|e| one(&e.path(), held))
        .collect();
    // Sorted by address, so two passes over the same machine produce the same
    // list and a level-triggered reconcile sees no change where there is none.
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    devices
}

fn one(dir: &Path, held: &std::collections::BTreeMap<String, String>) -> Option<PciDevice> {
    let address = dir.file_name()?.to_str()?.to_string();
    // A PCI address and nothing else. sysfs keeps other things in this
    // directory on some kernels, and a stray name read as a device would be a
    // device nobody can find.
    if !looks_like_an_address(&address) {
        return None;
    }
    let vendor = hex4(dir, "vendor")?;
    let device = hex4(dir, "device")?;

    let driver = std::fs::read_link(dir.join("driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));

    // Held by a guest, bound to a host driver, or free. Checked in that order:
    // a device passed through *is* bound to `vfio-pci`, so asking sysfs alone
    // would report every guest's GPU as "bound to a driver" and never as in
    // use — true and useless.
    let state = match held.get(&address) {
        Some(instance) => DeviceUse::Guest {
            instance: instance.clone(),
        },
        None => match driver.as_deref() {
            // `vfio-pci` is the driver that means "available for passing
            // through", which is the opposite of what every other driver here
            // means.
            None | Some("vfio-pci") => DeviceUse::Free,
            Some(d) => DeviceUse::HostDriver {
                driver: d.to_string(),
            },
        },
    };

    Some(PciDevice {
        address,
        vendor_device: format!("{vendor:04x}:{device:04x}"),
        description: read_trimmed(dir, "label").unwrap_or_default(),
        kind: kind_of(read_hex(dir, "class").unwrap_or(0)),
        iommu_group: std::fs::read_link(dir.join("iommu_group"))
            .ok()
            .and_then(|p| p.file_name()?.to_str()?.parse().ok()),
        state,
    })
}

/// `0000:41:00.0` — domain:bus:device.function.
fn looks_like_an_address(s: &str) -> bool {
    let mut parts = s.split(':');
    let (Some(domain), Some(bus), Some(rest), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Some((slot, function)) = rest.split_once('.') else {
        return false;
    };
    domain.len() == 4
        && bus.len() == 2
        && slot.len() == 2
        && !function.is_empty()
        && [domain, bus, slot, function]
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The broad kind, from the top byte of the 24-bit class code.
///
/// Audio is the one that needs its full code: `0x0403` is a multimedia *audio*
/// device, which is what sits beside a GPU and is the usual reason a GPU's
/// IOMMU group has two members. The rest of class 0x04 is video capture and
/// the like, which nobody is passing through by accident.
fn kind_of(class: u32) -> DeviceKind {
    match (class >> 16, class >> 8) {
        (0x01, _) => DeviceKind::Storage,
        (0x02, _) => DeviceKind::Network,
        (0x03, _) => DeviceKind::Gpu,
        (0x04, 0x0403) => DeviceKind::Audio,
        _ => DeviceKind::Other,
    }
}

fn read_trimmed(dir: &Path, name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(name)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn read_hex(dir: &Path, name: &str) -> Option<u32> {
    let raw = read_trimmed(dir, name)?;
    u32::from_str_radix(raw.trim_start_matches("0x"), 16).ok()
}

fn hex4(dir: &Path, name: &str) -> Option<u16> {
    read_hex(dir, name)?.try_into().ok()
}

/// The kernel command line asked for IOMMU, as far as this node can tell.
///
/// Reported so the console can tell "this machine has no accelerators" from
/// "this machine cannot pass anything through", which look identical in an
/// empty offer list and have completely different remedies.
pub fn iommu_enabled() -> bool {
    Path::new("/sys/kernel/iommu_groups")
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::*;

    /// Build a fake `/sys/bus/pci/devices`.
    ///
    /// A directory under the system temp, named for the process and a counter,
    /// and removed on drop — the idiom the rest of this crate's tests use, so
    /// no dependency is added for a directory.
    struct Sysfs {
        dir: PathBuf,
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Sysfs {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "velstra-pci-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // Fresh every time: a leftover from a killed run would otherwise
            // show up as devices this test never created.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        fn device(&self, address: &str, vendor: &str, device: &str, class: &str) -> PathBuf {
            let d = self.path().join(address);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("vendor"), format!("0x{vendor}\n")).unwrap();
            std::fs::write(d.join("device"), format!("0x{device}\n")).unwrap();
            std::fs::write(d.join("class"), format!("0x{class}\n")).unwrap();
            d
        }

        fn bind(&self, dir: &Path, driver: &str) {
            let target = self.path().join("__drivers").join(driver);
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, dir.join("driver")).unwrap();
        }

        fn group(&self, dir: &Path, group: u32) {
            let target = self.path().join("__groups").join(group.to_string());
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, dir.join("iommu_group")).unwrap();
        }

        fn read(&self, held: &BTreeMap<String, String>) -> Vec<PciDevice> {
            read_from(self.path(), held)
        }
    }

    #[test]
    fn a_device_is_read_with_its_id_kind_and_group() {
        let fs = Sysfs::new();
        let gpu = fs.device("0000:41:00.0", "10de", "2204", "030000");
        fs.group(&gpu, 17);

        let all = fs.read(&BTreeMap::new());
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].address, "0000:41:00.0");
        assert_eq!(all[0].vendor_device, "10de:2204");
        assert_eq!(all[0].kind, DeviceKind::Gpu);
        assert_eq!(all[0].iommu_group, Some(17));
        // Nothing bound: free.
        assert_eq!(all[0].state, DeviceUse::Free);
    }

    /// `vfio-pci` means available, and every other driver means held.
    ///
    /// The inversion that is easy to get backwards: a device bound to
    /// `vfio-pci` looks bound and *is* the one that can be given away.
    #[test]
    fn vfio_pci_reads_as_free_and_any_other_driver_reads_as_held() {
        let fs = Sysfs::new();
        let ready = fs.device("0000:41:00.0", "10de", "2204", "030000");
        fs.bind(&ready, "vfio-pci");
        let busy = fs.device("0000:81:00.0", "10de", "2204", "030000");
        fs.bind(&busy, "nvidia");

        let all = fs.read(&BTreeMap::new());
        assert_eq!(all[0].state, DeviceUse::Free);
        assert_eq!(
            all[1].state,
            DeviceUse::HostDriver {
                driver: "nvidia".into()
            }
        );
    }

    /// A device a guest holds is reported as the guest's, not as `vfio-pci`.
    ///
    /// Sysfs cannot know this: a passed-through device is bound to `vfio-pci`
    /// exactly like a free one. Reporting from sysfs alone would show every
    /// guest's GPU as available and hand it to a second guest.
    #[test]
    fn a_device_a_guest_holds_is_reported_as_the_guests() {
        let fs = Sysfs::new();
        let d = fs.device("0000:41:00.0", "10de", "2204", "030000");
        fs.bind(&d, "vfio-pci");
        fs.group(&d, 17);

        let held = BTreeMap::from([(
            "0000:41:00.0".to_string(),
            "projects/p1/instances/i1".to_string(),
        )]);
        let all = fs.read(&held);
        assert_eq!(
            all[0].state,
            DeviceUse::Guest {
                instance: "projects/p1/instances/i1".into()
            }
        );
    }

    #[test]
    fn a_machine_with_no_iommu_reports_devices_without_a_group() {
        let fs = Sysfs::new();
        fs.device("0000:41:00.0", "10de", "2204", "030000");
        let all = fs.read(&BTreeMap::new());
        assert_eq!(all[0].iommu_group, None);
        // And such a device is never offerable — the rule lives in the model,
        // and this is the report it acts on.
        assert!(velstra_cloud_model::pci::offerable(&all[0], &all).is_err());
    }

    #[test]
    fn the_broad_kind_comes_off_the_class_code() {
        let fs = Sysfs::new();
        fs.device("0000:01:00.0", "8086", "1521", "020000"); // network
        fs.device("0000:02:00.0", "1b4b", "9230", "010601"); // storage (AHCI)
        fs.device("0000:03:00.0", "10de", "2204", "030000"); // display
        fs.device("0000:04:00.0", "10de", "1aef", "040300"); // audio
        fs.device("0000:05:00.0", "1022", "1480", "060000"); // bridge → other

        let all = fs.read(&BTreeMap::new());
        let kinds: Vec<DeviceKind> = all.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [
                DeviceKind::Network,
                DeviceKind::Storage,
                DeviceKind::Gpu,
                DeviceKind::Audio,
                DeviceKind::Other,
            ]
        );
    }

    /// Anything in the directory that is not a PCI address is skipped.
    #[test]
    fn a_stray_name_in_the_directory_is_not_read_as_a_device() {
        let fs = Sysfs::new();
        std::fs::create_dir_all(fs.path().join("__drivers")).unwrap();
        std::fs::write(fs.path().join("power"), "on").unwrap();
        fs.device("0000:41:00.0", "10de", "2204", "030000");

        let all = fs.read(&BTreeMap::new());
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].address, "0000:41:00.0");
    }

    #[test]
    fn an_absent_sysfs_yields_no_devices_rather_than_an_error() {
        let all = read_from(Path::new("/nonexistent/pci"), &BTreeMap::new());
        assert!(all.is_empty());
    }

    /// Two passes over the same machine produce the same list.
    ///
    /// `read_dir` yields in whatever order the filesystem feels like, and an
    /// inventory that reordered itself would look like a change to every
    /// level-triggered reader on every pass.
    #[test]
    fn the_inventory_is_ordered_so_an_unchanged_machine_reads_as_unchanged() {
        let fs = Sysfs::new();
        fs.device("0000:81:00.0", "10de", "2204", "030000");
        fs.device("0000:41:00.0", "10de", "2204", "030000");
        fs.device("0000:41:00.1", "10de", "1aef", "040300");

        let addresses: Vec<String> = fs
            .read(&BTreeMap::new())
            .into_iter()
            .map(|d| d.address)
            .collect();
        assert_eq!(addresses, ["0000:41:00.0", "0000:41:00.1", "0000:81:00.0"]);
        assert_eq!(fs.read(&BTreeMap::new()), fs.read(&BTreeMap::new()));
    }
}
