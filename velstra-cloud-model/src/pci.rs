//! Passing a PCI device into a guest.
//!
//! ## The constraint that shapes everything
//!
//! **The IOMMU group is the unit of isolation, not the device.** A group is the
//! smallest set of devices the hardware can isolate from each other; a GPU
//! usually shares one with its own audio function, sometimes with a bridge, and
//! occasionally — on a board that routes badly — with a NIC somebody else's
//! guest is using. Handing one member to a guest hands it DMA reach over the
//! whole group.
//!
//! So a device is offerable only when *nothing in its group* is in use. That is
//! the same rule the Ceph disk inventory already follows ("offered only when it
//! is provably empty"), applied to a different piece of hardware, and it is why
//! every function here takes the group rather than the device.
//!
//! ## What an instance asks for
//!
//! A **class**, never an address. `0000:41:00.0` is node-specific: an instance
//! naming one can only ever be scheduled on the one machine that has it, and
//! the scheduler is left with nothing to decide. A [`DeviceClassSpec`] names a
//! set of matching devices across the fleet, and an instance asks for the
//! class.
//!
//! ## What phase one does not model
//!
//! No vGPU, no MIG, no time-slicing, no weights — whole-device passthrough
//! only. Not an oversight: vGPU needs a proprietary host driver tied to a
//! licence server, and a field the console offers that the platform cannot
//! deliver is worse than an absent one. When the driver exists the field
//! arrives in the same change as the code that reads it.
//!
//! ## What is tested
//!
//! All of it, because all of it is pure. Whether `vfio-pci` actually binds is
//! not decided here — the node reports what is bound and this reasons about
//! the report.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// What a PCI device is for, coarsely. The part of the class code that decides
/// whether a person would call it a GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    #[default]
    Other,
    /// Display controller (PCI class 0x03) — what an operator means by "GPU".
    Gpu,
    /// Network controller (class 0x02).
    Network,
    /// Mass storage controller (class 0x01).
    Storage,
    /// Audio (class 0x0403). Listed because it is almost never wanted on its
    /// own and almost always in a GPU's IOMMU group, where it is the reason
    /// the group has two members.
    Audio,
}

/// What is holding a device right now, as the node sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DeviceUse {
    /// Bound to `vfio-pci` and claimed by nobody: available.
    #[default]
    Free,
    /// Bound to a host driver — `nvidia`, `nouveau`, `ixgbe`. Not available,
    /// and the driver is named because that is what an operator has to change.
    HostDriver { driver: String },
    /// Held by a guest on this node.
    Guest { instance: String },
}

/// One PCI device, as the node holding it sees it.
///
/// Observation only, in the pattern the block-device inventory already
/// follows: the node states facts and decides nothing. Which of these may be
/// *offered* is [`offerable`], never this list on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PciDevice {
    /// `0000:41:00.0`. Node-specific, which is why an instance never names one.
    pub address: String,
    /// `10de:2204`, lowercase hex, as `lspci -n` writes it. The stable identity
    /// of *what* this is, and what a class matches on.
    pub vendor_device: String,
    /// The human name, for a console: `NVIDIA GA102 [GeForce RTX 3090]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub kind: DeviceKind,
    /// The IOMMU group number this device is in.
    ///
    /// `None` means the machine has no IOMMU, or it is off in the firmware or
    /// the kernel command line. Not an error to report — plenty of nodes never
    /// pass anything through — but such a device can never be offered, because
    /// nothing can isolate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    #[serde(default)]
    pub state: DeviceUse,
}

/// Why a device is not on offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotOfferable {
    /// The machine cannot isolate it: no IOMMU group.
    NoIommu,
    /// The device itself is held.
    InUse { by: DeviceUse },
    /// Something *else* in the same IOMMU group is held.
    ///
    /// The variant that surprises people, and the reason it names the other
    /// device: "your GPU is busy" is wrong and unactionable when the truth is
    /// "the audio function beside it is bound to snd_hda_intel".
    GroupInUse {
        group: u32,
        other: String,
        by: DeviceUse,
    },
}

impl std::fmt::Display for NotOfferable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotOfferable::NoIommu => f.write_str(
                "this node has no IOMMU group for it — enable IOMMU in the firmware and \
                 on the kernel command line",
            ),
            NotOfferable::InUse {
                by: DeviceUse::HostDriver { driver },
            } => write!(f, "it is bound to the host driver {driver}"),
            NotOfferable::InUse {
                by: DeviceUse::Guest { instance },
            } => write!(f, "{instance} is using it"),
            NotOfferable::InUse { by: DeviceUse::Free } => {
                // Not reachable from `offerable`, which never builds this.
                f.write_str("it is not available")
            }
            NotOfferable::GroupInUse { group, other, by } => {
                let held = match by {
                    DeviceUse::HostDriver { driver } => format!("bound to {driver}"),
                    DeviceUse::Guest { instance } => format!("held by {instance}"),
                    DeviceUse::Free => "in use".to_string(),
                };
                write!(
                    f,
                    "{other} is in the same IOMMU group ({group}) and is {held}; \
                     a group is passed through whole or not at all"
                )
            }
        }
    }
}

/// Whether this device may be handed to a guest, and if not, why.
///
/// Takes the whole node's inventory because the answer is never about one
/// device: it is about its IOMMU group. A version of this that looked only at
/// `device.state` would offer a GPU whose audio function is bound to the host
/// — and passing that group through takes the host's audio device away
/// mid-flight while giving the guest DMA reach over it.
pub fn offerable(device: &PciDevice, all: &[PciDevice]) -> Result<(), NotOfferable> {
    let Some(group) = device.iommu_group else {
        return Err(NotOfferable::NoIommu);
    };
    if device.state != DeviceUse::Free {
        return Err(NotOfferable::InUse {
            by: device.state.clone(),
        });
    }
    for other in all {
        if other.address == device.address || other.iommu_group != Some(group) {
            continue;
        }
        if other.state != DeviceUse::Free {
            return Err(NotOfferable::GroupInUse {
                group,
                other: other.address.clone(),
                by: other.state.clone(),
            });
        }
    }
    Ok(())
}

/// Every device in one IOMMU group, by address, sorted.
///
/// What a console shows beside a device before anybody claims it: passing one
/// through takes all of these, and an operator who learns that afterwards
/// learns it from an outage.
pub fn group_members(device: &PciDevice, all: &[PciDevice]) -> Vec<String> {
    let Some(group) = device.iommu_group else {
        return vec![device.address.clone()];
    };
    let mut members: Vec<String> = all
        .iter()
        .filter(|d| d.iommu_group == Some(group))
        .map(|d| d.address.clone())
        .collect();
    members.sort();
    members
}

/// A named set of interchangeable devices, across the fleet.
///
/// The thing an instance asks for. Fleet-wide rather than per node, because
/// that is exactly what makes an instance schedulable on more than one machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceClassSpec {
    /// `vendor:device` values a member of this class may have. Several,
    /// because a fleet buys the same accelerator across two board revisions
    /// and an operator should not have to care which node got which.
    #[serde(default)]
    pub matches: Vec<String>,
    /// What to call it in a console: `NVIDIA A100 80GB`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl DeviceClassSpec {
    /// Whether a device belongs to this class.
    ///
    /// Case-insensitive on the id, because `lspci` and the kernel disagree
    /// about hex case and neither is wrong — an operator who pasted one and
    /// gets no match learns nothing useful from the silence.
    pub fn accepts(&self, device: &PciDevice) -> bool {
        self.matches
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&device.vendor_device))
    }
}

/// What a node can offer of one class, right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassAvailability {
    /// Devices of the class that are free and whose whole group is free.
    pub free: Vec<String>,
    /// Devices of the class that exist here and are not on offer, each with
    /// the reason. Carried rather than counted: "3 exist, none free" sends an
    /// operator hunting, and "the audio function beside it is bound to
    /// snd_hda_intel" is a thing they can act on in a minute.
    pub blocked: Vec<(String, NotOfferable)>,
}

impl ClassAvailability {
    pub fn any_free(&self) -> bool {
        !self.free.is_empty()
    }
}

/// What one node can offer of one class.
pub fn availability(class: &DeviceClassSpec, devices: &[PciDevice]) -> ClassAvailability {
    let mut free = Vec::new();
    let mut blocked = Vec::new();
    for device in devices.iter().filter(|d| class.accepts(d)) {
        match offerable(device, devices) {
            Ok(()) => free.push(device.address.clone()),
            Err(why) => blocked.push((device.address.clone(), why)),
        }
    }
    free.sort();
    blocked.sort_by(|a, b| a.0.cmp(&b.0));
    ClassAvailability { free, blocked }
}

/// Choose devices for an instance's request, or say what is short.
///
/// Devices of the same class are interchangeable by definition, so this takes
/// the lowest free addresses — a stable choice, which matters because an
/// unstable one would move a guest's device between passes of a level-triggered
/// reconcile and restart it for no reason.
///
/// Two devices of the same class in one request are two *different* devices:
/// asking for two A100s and being handed the same one twice is the bug this
/// exists to make impossible.
pub fn assign(
    wanted: &[String],
    classes: &BTreeMap<String, DeviceClassSpec>,
    devices: &[PciDevice],
) -> Result<Vec<String>, Shortfall> {
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for name in wanted {
        let Some(class) = classes.get(name) else {
            return Err(Shortfall::UnknownClass { class: name.clone() });
        };
        let here = availability(class, devices);
        let Some(pick) = here.free.iter().find(|a| !taken.contains(*a)) else {
            return Err(Shortfall::None {
                class: name.clone(),
                blocked: here.blocked,
                // How many the request had already consumed, so the message can
                // distinguish "there are none" from "there was one and you
                // asked for two".
                already_taken: taken.len(),
            });
        };
        taken.insert(pick.clone());
        out.push(pick.clone());
    }
    Ok(out)
}

/// Why a request could not be satisfied on a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shortfall {
    UnknownClass {
        class: String,
    },
    None {
        class: String,
        blocked: Vec<(String, NotOfferable)>,
        already_taken: usize,
    },
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Shortfall::UnknownClass { class } => {
                write!(f, "there is no device class {class}")
            }
            Shortfall::None {
                class,
                blocked,
                already_taken,
            } if blocked.is_empty() => {
                if *already_taken > 0 {
                    write!(f, "no further {class} here")
                } else {
                    write!(f, "no {class} here")
                }
            }
            Shortfall::None { class, blocked, .. } => {
                let reasons: Vec<String> = blocked
                    .iter()
                    .map(|(address, why)| format!("{address}: {why}"))
                    .collect();
                write!(f, "no free {class} here — {}", reasons.join("; "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(address: &str, group: u32, state: DeviceUse) -> PciDevice {
        PciDevice {
            address: address.into(),
            vendor_device: "10de:2204".into(),
            description: "NVIDIA GA102".into(),
            kind: DeviceKind::Gpu,
            iommu_group: Some(group),
            state,
        }
    }

    fn audio(address: &str, group: u32, state: DeviceUse) -> PciDevice {
        PciDevice {
            address: address.into(),
            vendor_device: "10de:1aef".into(),
            kind: DeviceKind::Audio,
            iommu_group: Some(group),
            state,
            ..PciDevice::default()
        }
    }

    fn a100() -> DeviceClassSpec {
        DeviceClassSpec {
            matches: vec!["10de:2204".into()],
            description: "NVIDIA GA102".into(),
        }
    }

    #[test]
    fn a_lone_free_device_in_a_free_group_is_offerable() {
        let all = vec![gpu("0000:41:00.0", 17, DeviceUse::Free)];
        assert_eq!(offerable(&all[0], &all), Ok(()));
    }

    /// The rule the whole module exists for: a busy neighbour blocks the
    /// device, and the refusal names the neighbour.
    ///
    /// This is the failure that would otherwise be discovered by passing the
    /// group through and taking the host's audio device away mid-flight.
    #[test]
    fn a_busy_neighbour_in_the_same_group_blocks_the_device_and_is_named() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            audio(
                "0000:41:00.1",
                17,
                DeviceUse::HostDriver {
                    driver: "snd_hda_intel".into(),
                },
            ),
        ];
        let Err(NotOfferable::GroupInUse { group, other, by }) = offerable(&all[0], &all) else {
            panic!("a GPU was offered while its group-mate was bound to the host");
        };
        assert_eq!(group, 17);
        assert_eq!(other, "0000:41:00.1");
        assert_eq!(
            by,
            DeviceUse::HostDriver {
                driver: "snd_hda_intel".into()
            }
        );

        // And the sentence says what to look at, rather than blaming the GPU.
        let said = NotOfferable::GroupInUse { group, other, by }.to_string();
        assert!(said.contains("0000:41:00.1"), "{said}");
        assert!(said.contains("snd_hda_intel"), "{said}");
    }

    /// A neighbour in a *different* group is irrelevant, however busy.
    #[test]
    fn a_busy_device_in_another_group_does_not_block_anything() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            audio(
                "0000:42:00.1",
                18,
                DeviceUse::HostDriver {
                    driver: "snd_hda_intel".into(),
                },
            ),
        ];
        assert_eq!(offerable(&all[0], &all), Ok(()));
    }

    /// A machine with no IOMMU can never offer anything, and says why.
    #[test]
    fn a_device_with_no_iommu_group_is_never_offerable() {
        let mut d = gpu("0000:41:00.0", 17, DeviceUse::Free);
        d.iommu_group = None;
        let all = vec![d.clone()];
        assert_eq!(offerable(&d, &all), Err(NotOfferable::NoIommu));
        assert!(
            NotOfferable::NoIommu.to_string().contains("kernel command line"),
            "the refusal does not say what to change"
        );
        // Its "group" is itself: a console must still be able to say what
        // would be taken, and inventing group-mates would be worse than
        // saying the one thing that is true.
        assert_eq!(group_members(&d, &all), ["0000:41:00.0"]);
    }

    #[test]
    fn a_group_lists_every_member_so_nobody_learns_it_from_an_outage() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            audio("0000:41:00.1", 17, DeviceUse::Free),
            gpu("0000:81:00.0", 25, DeviceUse::Free),
        ];
        assert_eq!(
            group_members(&all[0], &all),
            ["0000:41:00.0", "0000:41:00.1"]
        );
        assert_eq!(group_members(&all[2], &all), ["0000:81:00.0"]);
    }

    #[test]
    fn a_class_matches_on_the_id_whatever_case_it_was_written_in() {
        let class = DeviceClassSpec {
            matches: vec!["10DE:2204".into()],
            ..DeviceClassSpec::default()
        };
        assert!(class.accepts(&gpu("0000:41:00.0", 17, DeviceUse::Free)));
        assert!(!class.accepts(&audio("0000:41:00.1", 17, DeviceUse::Free)));
    }

    /// Two of a class means two devices, not the same one twice.
    #[test]
    fn a_request_for_two_is_given_two_different_devices() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            gpu("0000:81:00.0", 25, DeviceUse::Free),
        ];
        let classes = BTreeMap::from([("gpu-a100".to_string(), a100())]);
        let picked = assign(
            &["gpu-a100".to_string(), "gpu-a100".to_string()],
            &classes,
            &all,
        )
        .expect("two free GPUs satisfy a request for two");
        assert_eq!(picked, ["0000:41:00.0", "0000:81:00.0"]);
    }

    /// Asking for more than exist is refused, and the message distinguishes
    /// "none at all" from "not enough".
    #[test]
    fn asking_for_more_than_exist_says_which_kind_of_shortfall_it_is() {
        let all = vec![gpu("0000:41:00.0", 17, DeviceUse::Free)];
        let classes = BTreeMap::from([("gpu-a100".to_string(), a100())]);

        let Err(short) = assign(
            &["gpu-a100".to_string(), "gpu-a100".to_string()],
            &classes,
            &all,
        ) else {
            panic!("one GPU satisfied a request for two");
        };
        assert!(
            short.to_string().contains("no further"),
            "a partial shortfall reads as an absence: {short}"
        );

        let Err(none) = assign(&["gpu-a100".to_string()], &classes, &[]) else {
            panic!("a node with no devices satisfied a request");
        };
        assert_eq!(none.to_string(), "no gpu-a100 here");
    }

    /// A shortfall caused by a busy group carries the reason all the way out.
    ///
    /// The chain that matters: an operator asks why their guest will not
    /// start and is told which neighbouring device to unbind, not that the
    /// class is unavailable.
    #[test]
    fn a_shortfall_carries_the_group_reason_rather_than_flattening_it() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            audio(
                "0000:41:00.1",
                17,
                DeviceUse::HostDriver {
                    driver: "snd_hda_intel".into(),
                },
            ),
        ];
        let classes = BTreeMap::from([("gpu-a100".to_string(), a100())]);
        let Err(short) = assign(&["gpu-a100".to_string()], &classes, &all) else {
            panic!("a GPU in a busy group was assigned");
        };
        let said = short.to_string();
        assert!(said.contains("snd_hda_intel"), "{said}");
        assert!(said.contains("0000:41:00.1"), "{said}");
    }

    #[test]
    fn an_unknown_class_is_named_rather_than_read_as_an_empty_one() {
        let Err(e) = assign(&["gpu-h100".to_string()], &BTreeMap::new(), &[]) else {
            panic!("an unknown class was satisfied");
        };
        assert_eq!(
            e,
            Shortfall::UnknownClass {
                class: "gpu-h100".into()
            }
        );
    }

    #[test]
    fn availability_separates_what_is_free_from_what_is_merely_present() {
        let all = vec![
            gpu("0000:41:00.0", 17, DeviceUse::Free),
            gpu(
                "0000:81:00.0",
                25,
                DeviceUse::Guest {
                    instance: "projects/p1/instances/i1".into(),
                },
            ),
        ];
        let here = availability(&a100(), &all);
        assert_eq!(here.free, ["0000:41:00.0"]);
        assert_eq!(here.blocked.len(), 1);
        assert_eq!(here.blocked[0].0, "0000:81:00.0");
        assert!(here.any_free());
    }
}
