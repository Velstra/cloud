//! What disks this machine has, and which of them are safe to give away.
//!
//! The node's own report about its own hardware, in the same shape as its images
//! and for the same reason: nobody else can see it.
//!
//! ## Why this is a parser and not a decision
//!
//! Everything here turns `lsblk --json` into
//! [`BlockDevice`](velstra_cloud_model::ceph::BlockDevice)s and stops. Whether
//! one may be consumed is
//! [`may_consume`](velstra_cloud_model::ceph::may_consume), in the model, where
//! it can be exercised without a disk to lose. This file's whole job is to be
//! *accurate about what is there*, and the one way it can be dangerous is by
//! reporting something as empty when it is not.
//!
//! So the classification is written the pessimistic way round: a device is
//! `Free` only when every branch that could say otherwise has been checked, and
//! anything unrecognised is `Unsuitable` rather than free. A new `lsblk` field,
//! a device type nobody here has seen, a `null` where a string was expected —
//! each of those lands on "do not offer this", which costs an operator a disk
//! they could have used and costs nobody any data.
//!
//! ## Why `lsblk` and not `/sys/block`
//!
//! `/sys` has the devices and not the signatures: it says a disk is 1 TB and
//! spinning, and says nothing about the ext4 on it. Deciding from `/sys` alone
//! would mean offering every disk that has no partition table, which includes
//! every whole-disk filesystem in existence. `lsblk` calls libblkid, which is
//! the same thing every other tool on the machine trusts.

use velstra_cloud_model::ceph::{BlockDevice, DeviceUse};

use crate::host::{HostError, Result};

/// Older `util-linux` — before its `--json` emitter learned typed values —
/// prints every `lsblk` field as a string: `"size":"500107862016"`,
/// `"rota":"1"`. A plain `u64`/`bool` field then fails, and because
/// [`parse_lsblk`] deserialises the whole document at once, one string-typed
/// field makes the node report *no disks at all* — this file's single dangerous
/// outcome. These accept the number, the string, and a `null` (as zero/false)
/// so the parse survives either `lsblk`.
fn de_u64_flex<'de, D>(d: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

fn de_bool_flex<'de, D>(d: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_u64().is_some_and(|v| v != 0),
        serde_json::Value::String(s) => matches!(s.trim(), "1" | "true" | "True"),
        _ => false,
    })
}

/// The `lsblk` columns this needs. Named here so the call and the parse cannot
/// drift apart.
const COLUMNS: &str = "NAME,PATH,SIZE,TYPE,ROTA,RM,MODEL,SERIAL,FSTYPE,MOUNTPOINT,PKNAME";

#[derive(Debug, serde::Deserialize)]
struct Listing {
    #[serde(default)]
    blockdevices: Vec<Node>,
}

#[derive(Debug, serde::Deserialize)]
struct Node {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default, deserialize_with = "de_u64_flex")]
    size: u64,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "de_bool_flex")]
    rota: bool,
    #[serde(default, deserialize_with = "de_bool_flex")]
    rm: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    children: Vec<Node>,
}

impl Node {
    /// Every node under this one, itself included.
    fn flatten(&self) -> Vec<&Node> {
        let mut all = vec![self];
        for child in &self.children {
            all.extend(child.flatten());
        }
        all
    }
}

/// Turn one `lsblk --json` document into the disks a node should report.
///
/// Only whole disks: partitions, device-mapper nodes and loop devices are not
/// things anybody hands to Ceph, and listing them would be a list where the
/// wrong entry looks like the right one.
pub fn parse_lsblk(json: &str) -> Result<Vec<BlockDevice>> {
    let listing: Listing = serde_json::from_str(json)
        .map_err(|e| HostError::failed(format!("`lsblk --json` did not answer with json: {e}")))?;
    Ok(listing
        .blockdevices
        .iter()
        .filter(|node| node.kind == "disk")
        .map(classify)
        .collect())
}

/// What one whole disk is being used for.
///
/// The order of these checks is the safety property. Each one that can say "not
/// free" is asked before the one that can say "free", and the last word belongs
/// to no branch at all — `Free` is only reached by falling off the end with
/// nothing found.
fn classify(disk: &Node) -> BlockDevice {
    let all = disk.flatten();

    // Removable first: a USB stick with nothing on it passes every other check,
    // and building cluster storage on one is not a thing anybody meant.
    // What a disk *is* comes before what the platform thinks of it: a
    // removable device that already carries an OSD, a volume or a filesystem
    // is that thing, and reporting it as "unsuitable" hid a running OSD from
    // the deploy for ever — the step could never be seen as done. The
    // removable opinion now applies only to an empty disk, where it is the
    // only thing left to say.
    let state = if disk.size == 0 {
        DeviceUse::Unsuitable {
            why: "it reports no size, so there is nothing here to use".to_string(),
        }
    } else if all.iter().any(|n| is_system(n)) {
        // The machine's own life comes before every other answer: a disk
        // carrying root or swap is one that takes the node down with it, and
        // that is worth saying instead of "it is mounted at /".
        DeviceUse::System
    } else if let Some(id) = all.iter().find_map(|n| osd_id(n)) {
        DeviceUse::Osd { id }
    } else if let Some(of) = all.iter().find_map(|n| volume_member(n)) {
        DeviceUse::Volume { of }
    } else if let Some(at) = all
        .iter()
        .filter_map(|n| n.mountpoint.as_deref())
        .find(|m| !m.is_empty())
    {
        DeviceUse::Mounted { at: at.to_string() }
    } else if !disk.children.is_empty() {
        DeviceUse::Partitioned {
            partitions: disk.children.len() as u32,
        }
    } else if let Some(fstype) = disk.fstype.as_deref().filter(|f| !f.is_empty()) {
        DeviceUse::Filesystem {
            fstype: fstype.to_string(),
        }
    } else if disk.rm {
        DeviceUse::Unsuitable {
            why: "it is removable, and cluster storage on a removable device is not something \
                  anybody means"
                .to_string(),
        }
    } else {
        DeviceUse::Free
    };

    BlockDevice {
        path: if disk.path.is_empty() {
            format!("/dev/{}", disk.name)
        } else {
            disk.path.clone()
        },
        kernel_name: disk.name.clone(),
        // Rounded **down**: this is how much can be stored, and rounding a
        // 999 GiB disk up to 1000 would have a scheduler place what does not fit.
        size_gib: disk.size / (1024 * 1024 * 1024),
        rotational: disk.rota,
        model: disk.model.clone().unwrap_or_default().trim().to_string(),
        serial: disk.serial.clone().unwrap_or_default().trim().to_string(),
        state,
    }
}

/// Whether this node is the machine's root or its swap.
fn is_system(node: &Node) -> bool {
    let mount = node.mountpoint.as_deref().unwrap_or("");
    mount == "/" || mount.starts_with("[SWAP]") || node.fstype.as_deref() == Some("swap")
}

/// The OSD living on this node, if it is one.
///
/// Three spellings, and the third is the one that actually shows up. A modern
/// OSD made by `ceph-volume` does not put a `ceph_bluestore` signature on the
/// disk at all — it lays an LVM volume group over it, so `lsblk` reports the
/// disk as `LVM2_member` and its child as `ceph--<uuid>-osd--block--<uuid>`.
/// Matching only the signatures would classify a working OSD as an ordinary LVM
/// member, which reads as "not free" (so nothing is destroyed) but never reads
/// as "already done" — and the deployment would ask for that OSD again on every
/// pass, for ever.
///
/// `ceph_bluestore` and `ceph_data` stay because a disk prepared the old way,
/// or by hand, still carries them.
fn osd_id(node: &Node) -> Option<String> {
    if let Some(fstype) = node.fstype.as_deref()
        && fstype.starts_with("ceph")
    {
        return Some(format!("on {}", node.name));
    }
    // LVM names are mangled: a single dash in the volume-group name becomes a
    // double dash in the device-mapper name, so the group `ceph-<uuid>` appears
    // as `ceph--<uuid>`. The common prefix survives both spellings.
    node.name
        .starts_with("ceph-")
        .then(|| format!("on {}", node.name))
}

/// The array, group or pool this node belongs to.
fn volume_member(node: &Node) -> Option<String> {
    match node.fstype.as_deref()? {
        "LVM2_member" => Some("an LVM volume group".to_string()),
        "linux_raid_member" => Some("a Linux MD array".to_string()),
        "zfs_member" => Some("a ZFS pool".to_string()),
        "crypto_LUKS" => Some("a LUKS container".to_string()),
        _ => None,
    }
}

/// Ask the machine what it has.
pub async fn observe_devices() -> Result<Vec<BlockDevice>> {
    let output = tokio::process::Command::new("lsblk")
        .args(["--json", "--bytes", "-o", COLUMNS])
        .output()
        .await
        .map_err(|e| HostError::failed(format!("running `lsblk`: {e}")))?;
    if !output.status.success() {
        return Err(HostError::failed(format!(
            "`lsblk` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_lsblk(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::ceph::may_consume;

    use super::*;

    /// A real machine's `lsblk`, with the three cases that matter on it: swap,
    /// an encrypted root behind partitions, and nothing spare at all.
    ///
    /// Taken verbatim from a laptop rather than written by hand, because a
    /// fixture somebody invented is a fixture that matches the parser rather
    /// than the world.
    const REAL_LAPTOP: &str = r#"{
   "blockdevices": [
      {"name":"zram0","path":"/dev/zram0","size":29169287168,"type":"disk","rota":false,"rm":false,
       "model":null,"serial":null,"fstype":"swap","mountpoint":"[SWAP]","pkname":null},
      {"name":"nvme0n1","path":"/dev/nvme0n1","size":1024209543168,"type":"disk","rota":false,"rm":false,
       "model":"SKHynix_HFS001TEJ9X102N","serial":"SJB8N40801130863F","fstype":null,"mountpoint":null,"pkname":null,
       "children":[
         {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","size":4294967296,"type":"part","rota":false,"rm":false,
          "model":null,"serial":null,"fstype":"vfat","mountpoint":"/boot","pkname":"nvme0n1"},
         {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","size":1019912441856,"type":"part","rota":false,"rm":false,
          "model":null,"serial":null,"fstype":"crypto_LUKS","mountpoint":null,"pkname":"nvme0n1",
          "children":[
            {"name":"luks-c28e","path":"/dev/mapper/luks-c28e","size":1019895664640,"type":"crypt","rota":false,"rm":false,
             "model":null,"serial":null,"fstype":"btrfs","mountpoint":"/var/tmp","pkname":"nvme0n1p2"}]}]}
   ]}"#;

    #[test]
    fn a_real_machine_with_nothing_spare_offers_nothing() {
        let devices = parse_lsblk(REAL_LAPTOP).unwrap();
        // Two whole disks; the partitions and the dm-crypt node are not things
        // anybody hands to Ceph and listing them would put the wrong entry
        // beside the right one.
        assert_eq!(devices.len(), 2, "{devices:#?}");

        let zram = &devices[0];
        assert_eq!(zram.state, DeviceUse::System, "swap was not seen as system");

        let nvme = &devices[1];
        // Root is behind LUKS behind a partition, so the *disk* is not mounted
        // anywhere — and it still must not be offered. Walking the whole tree is
        // what makes that true.
        assert!(
            matches!(
                nvme.state,
                DeviceUse::Volume { .. } | DeviceUse::Partitioned { .. }
            ),
            "the boot disk read as {:?}",
            nvme.state
        );
        assert_eq!(nvme.model, "SKHynix_HFS001TEJ9X102N");
        assert_eq!(nvme.size_gib, 953);

        for device in &devices {
            assert!(
                may_consume(device).is_err(),
                "{} was offered on a machine with no spare disk",
                device.path
            );
        }
    }

    fn one(json: &str) -> BlockDevice {
        parse_lsblk(&format!(r#"{{"blockdevices":[{json}]}}"#))
            .unwrap()
            .pop()
            .expect("one disk")
    }

    #[test]
    fn an_empty_disk_is_the_only_thing_offered() {
        let free = one(
            r#"{"name":"sdb","path":"/dev/sdb","size":536870912000,"type":"disk","rota":true,
                "rm":false,"model":"ST500","serial":"X1","fstype":null,"mountpoint":null}"#,
        );
        assert_eq!(free.state, DeviceUse::Free);
        assert!(may_consume(&free).is_ok());
        assert_eq!(free.size_gib, 500);
        assert!(free.rotational);
    }

    /// Every way a disk can be busy, and each one recognised as itself.
    ///
    /// The point is not that they are refused — `may_consume` does that — it is
    /// that they are refused *for the right reason*, because the reason is what
    /// the console shows and what an operator acts on.
    #[test]
    fn every_kind_of_busy_disk_is_recognised_as_the_kind_it_is() {
        /// One case: the `lsblk` fields, and what they must be recognised as.
        type Case = (&'static str, fn(&DeviceUse) -> bool);
        let cases: &[Case] = &[
            (
                r#""fstype":"ext4","mountpoint":null"#,
                |s| matches!(s, DeviceUse::Filesystem { fstype } if fstype == "ext4"),
            ),
            (
                r#""fstype":"LVM2_member","mountpoint":null"#,
                |s| matches!(s, DeviceUse::Volume { of } if of.contains("LVM")),
            ),
            (
                r#""fstype":"zfs_member","mountpoint":null"#,
                |s| matches!(s, DeviceUse::Volume { of } if of.contains("ZFS")),
            ),
            (
                r#""fstype":"linux_raid_member","mountpoint":null"#,
                |s| matches!(s, DeviceUse::Volume { of } if of.contains("MD")),
            ),
            (r#""fstype":"ceph_bluestore","mountpoint":null"#, |s| {
                matches!(s, DeviceUse::Osd { .. })
            }),
            (
                // The filestore spelling, which a cluster upgraded over the years
                // still has. Missing it would offer a disk that holds data.
                r#""fstype":"ceph_data","mountpoint":null"#,
                |s| matches!(s, DeviceUse::Osd { .. }),
            ),
            (
                r#""fstype":"xfs","mountpoint":"/srv""#,
                |s| matches!(s, DeviceUse::Mounted { at } if at == "/srv"),
            ),
            (r#""fstype":"xfs","mountpoint":"/""#, |s| {
                matches!(s, DeviceUse::System)
            }),
            (r#""fstype":"swap","mountpoint":null"#, |s| {
                matches!(s, DeviceUse::System)
            }),
        ];
        for (fields, expected) in cases {
            let device = one(&format!(
                r#"{{"name":"sdb","path":"/dev/sdb","size":536870912000,"type":"disk",
                     "rota":false,"rm":false,{fields}}}"#
            ));
            assert!(
                expected(&device.state),
                "{fields} was classified as {:?}",
                device.state
            );
            assert!(may_consume(&device).is_err(), "{fields} was offered");
        }
    }

    #[test]
    fn a_removable_disk_is_never_offered_however_empty_it_is() {
        let stick = one(
            r#"{"name":"sdc","path":"/dev/sdc","size":137438953472,"type":"disk","rota":false,
                "rm":true,"model":"Cruzer","serial":"Z","fstype":null,"mountpoint":null}"#,
        );
        // It passes every other check — nothing on it, big enough, not mounted.
        assert!(
            matches!(stick.state, DeviceUse::Unsuitable { .. }),
            "{:?}",
            stick.state
        );
        assert!(may_consume(&stick).is_err());
    }

    #[test]
    fn a_partitioned_disk_says_how_many_rather_than_just_no() {
        let device = one(
            r#"{"name":"sdb","path":"/dev/sdb","size":536870912000,"type":"disk","rota":false,
                "rm":false,"fstype":null,"mountpoint":null,"children":[
                  {"name":"sdb1","path":"/dev/sdb1","size":1,"type":"part","rota":false,"rm":false,
                   "fstype":null,"mountpoint":null},
                  {"name":"sdb2","path":"/dev/sdb2","size":1,"type":"part","rota":false,"rm":false,
                   "fstype":null,"mountpoint":null}]}"#,
        );
        assert_eq!(device.state, DeviceUse::Partitioned { partitions: 2 });
    }

    /// Output that is not the JSON expected is an error, never an empty list.
    ///
    /// An empty list reads as "this node has no disks", which is a perfectly
    /// ordinary state — so a parse failure that returned one would hide itself
    /// behind a node that simply looks diskless.
    #[test]
    fn unparseable_output_is_an_error_rather_than_a_node_with_no_disks() {
        for junk in ["", "lsblk: unrecognized option", "{}garbage"] {
            assert!(parse_lsblk(junk).is_err(), "{junk:?}");
        }
        // A document with the key and nothing in it *is* a node with no disks.
        assert_eq!(parse_lsblk(r#"{"blockdevices":[]}"#).unwrap().len(), 0);
    }

    #[test]
    fn size_is_rounded_down_because_it_says_what_fits() {
        // One byte short of 500 GiB is 499, not 500: this number is what a
        // scheduler places against, and rounding up would have it place what
        // does not fit.
        let device = one(
            r#"{"name":"sdb","path":"/dev/sdb","size":536870911999,"type":"disk","rota":false,
                "rm":false,"fstype":null,"mountpoint":null}"#,
        );
        assert_eq!(device.size_gib, 499);
    }
}
