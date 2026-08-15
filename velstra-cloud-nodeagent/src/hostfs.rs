//! What both hypervisor backends need from the machine underneath them.
//!
//! Cloud Hypervisor and QEMU differ in how a guest is described and how it is
//! spoken to, and in almost nothing else: both keep a directory per guest, both
//! boot from an image this node hashed itself, both put the guest in a
//! transient systemd unit so it outlives the agent, and both read their capacity
//! out of `/proc` and `/sys`. That shared half lives here so the two backends
//! are readable as what they actually are — two ways of talking to a VMM —
//! rather than two copies of a node.
//!
//! Nothing in this module remembers anything. Every function either changes the
//! machine or asks it a question; the answers to "what is running", "what is
//! listening" and "where" all come from systemd and the filesystem, which is
//! the same rule the rest of the crate follows and the reason a restarted agent
//! needs no catch-up.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use velstra_cloud_model::meta::Timestamp;

use crate::host::{HostError, Result};

/// Where a guest's things live and what it is called on this host.
///
/// Shared by both backends: `binary` and `firmware` mean "the VMM" and "what it
/// boots", and each backend reads them its own way.
#[derive(Clone, Debug)]
pub struct Layout {
    /// One directory per guest: `{run_dir}/{slug}/`.
    pub run_dir: PathBuf,
    /// Verified images, named by their digest. Nothing else is ever in here.
    pub image_dir: PathBuf,
    /// Where an image arrives before it has been verified.
    pub incoming_dir: PathBuf,
    pub slice: String,
    pub binary: String,
    /// Firmware or kernel to boot the guest with.
    ///
    /// Both VMMs require this at the **same path on both machines** for a
    /// migration: the guest's saved configuration names it, and the destination
    /// resolves that name against its own filesystem.
    pub firmware: PathBuf,
    /// What this node offers for guest disks. Not derivable from `std` without
    /// `statvfs`, so it is configured rather than guessed at.
    pub disk_gib: u64,
    /// The address other nodes reach this one at for a migration.
    ///
    /// `None` means this node only accepts a transfer over a unix socket — one
    /// machine, which is what an in-place VMM upgrade is. A node that should
    /// accept guests from elsewhere has to say where it can be reached, because
    /// nothing on the machine can tell which of its addresses its peers route
    /// to.
    pub migration_address: Option<String>,
    /// The ports a receiver may bind. One guest arriving needs one port, and a
    /// node that has run out says so rather than picking one at random and
    /// colliding with something.
    pub migration_ports: std::ops::Range<u16>,
    /// Where the certificates for an encrypted transfer live. TLS is only
    /// available over TCP — over a unix socket both VMMs refuse it — so this is
    /// ignored for a local move rather than silently downgrading it.
    pub migration_tls_dir: Option<PathBuf>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::from("/var/lib/velstra/instances"),
            image_dir: PathBuf::from("/var/lib/velstra/images"),
            incoming_dir: PathBuf::from("/var/lib/velstra/images/incoming"),
            slice: "velstra-vm.slice".to_string(),
            binary: "cloud-hypervisor".to_string(),
            firmware: PathBuf::from("/usr/share/cloud-hypervisor/hypervisor-fw"),
            disk_gib: 0,
            migration_address: None,
            migration_ports: 4900..4950,
            migration_tls_dir: None,
        }
    }
}

impl Layout {
    pub fn dir(&self, instance: &str) -> PathBuf {
        self.run_dir.join(slug(instance))
    }

    pub fn disk(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("root.raw")
    }
}

// ---- names ---------------------------------------------------------------

/// A resource name as one path segment, reversibly.
///
/// Reversible is the point: `observe` maps a directory back to the instance it
/// belongs to, so the mapping cannot be kept in a table somewhere. `~` is safe
/// because a `ResourceName` may only hold `a-z`, `0-9`, `-`, `.` and `/`.
pub fn slug(name: &str) -> String {
    name.replace('/', "~")
}

pub fn unslug(slug: &str) -> String {
    slug.replace('~', "/")
}

// ---- images --------------------------------------------------------------

/// The sha256 an image name commits to, if it carries one.
pub fn digest_of(image: &str) -> Option<String> {
    let last = image.rsplit('/').next()?;
    let hex = last
        .strip_prefix("sha256:")
        .or_else(|| last.strip_prefix("sha256-"))?;
    let hex = hex.to_ascii_lowercase();
    let valid = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then_some(hex)
}

pub async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Verify bytes that arrived in `incoming` and publish them under their digest.
///
/// Fetching them is somebody else's job — this node's job is to refuse to boot
/// anything it has not hashed itself. Identical on both backends because it is
/// about bytes on a disk and not about a hypervisor.
pub async fn publish_image(layout: &Layout, image: &str) -> Result<()> {
    let name = slug(image);
    let published = layout.image_dir.join(&name);
    if published.exists() {
        return Ok(());
    }
    let expected = digest_of(image).ok_or_else(|| {
        HostError::failed(format!(
            "{image} carries no sha256 digest in its name, so this node cannot verify it"
        ))
    })?;
    let incoming = layout.incoming_dir.join(&name);
    if !incoming.exists() {
        return Err(HostError::failed(format!(
            "no copy of {image} has arrived on this node"
        )));
    }
    let actual = sha256_file(&incoming).await?;
    if actual != expected {
        // Leave the bad copy where it is: deleting it destroys the evidence of
        // whatever produced it.
        return Err(HostError::failed(format!(
            "{image} hashed to {actual}, not {expected}"
        )));
    }
    std::fs::create_dir_all(&layout.image_dir)?;
    // The rename is the commit: a reader either sees no image or sees a
    // verified one, never a half-written file under a trusted name.
    std::fs::rename(&incoming, &published)?;
    Ok(())
}

/// A sparse file of the asked-for size, made once.
pub async fn create_disk(layout: &Layout, instance: &str, gib: u64) -> Result<()> {
    let dir = layout.dir(instance);
    std::fs::create_dir_all(&dir)?;
    let path = layout.disk(instance);
    if path.exists() {
        return Ok(());
    }
    // Written under a temporary name and renamed, so an interrupted creation
    // cannot leave a short disk that looks finished.
    let partial = dir.join("root.raw.partial");
    let file = std::fs::File::create(&partial)?;
    file.set_len(gib.saturating_mul(1024 * 1024 * 1024))?;
    file.sync_all()?;
    std::fs::rename(&partial, &path)?;
    Ok(())
}

// ---- the filesystem ------------------------------------------------------

pub fn read_dir_names(dir: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A node that has never run a guest has no directory, and that is not
        // an error — it is an empty machine.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `incoming` lives under the image directory and is not an image.
        if name == "incoming" || name.ends_with(".partial") {
            continue;
        }
        names.insert(name);
    }
    Ok(names)
}

/// When the guest's directory was created — the closest thing to a start time
/// that is a property of the machine rather than of this process's memory.
pub fn started_at(dir: &Path) -> Option<Timestamp> {
    let created = std::fs::metadata(dir).ok()?.modified().ok()?;
    let since = created.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(Timestamp(since.as_millis() as u64))
}

// ---- systemd -------------------------------------------------------------

/// Start something in its own transient unit inside this node's slice.
///
/// **Untested:** needs `systemd-run`.
///
/// Everything a node starts goes through here, and the reason is the same every
/// time: the unit's parent is systemd, so restarting or upgrading the agent
/// cannot take a tenant's workload — or a transfer half way through — with it.
pub async fn systemd_run(
    unit: &str,
    slice: &str,
    dir: &Path,
    program: &str,
    args: &[OsString],
) -> Result<()> {
    let mut command = tokio::process::Command::new("systemd-run");
    command
        .arg("--collect")
        .arg(format!("--unit={unit}"))
        .arg(format!("--slice={slice}"))
        // The unit gets its own cgroup subtree, which is where a node's limits
        // for guests go — separate from the agent's own, so a busy node
        // throttles guests rather than the thing that manages them.
        .arg("--property=Delegate=yes")
        .arg(format!("--property=WorkingDirectory={}", dir.display()))
        .arg("--")
        .arg(program)
        .args(args);

    let output = command.output().await?;
    if !output.status.success() {
        return Err(HostError::failed(format!(
            "systemd-run refused {unit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Whether a unit is running right now.
///
/// **Untested:** needs systemd. This is how "is a receiver listening" is
/// answered, and it must be a question rather than a memory: a receiver whose
/// process died has to stop being ready on the very next pass.
pub async fn unit_is_active(unit: &str) -> bool {
    let Ok(output) = tokio::process::Command::new("systemctl")
        .arg("is-active")
        .arg(format!("{unit}.service"))
        .output()
        .await
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).trim() == "active"
}

/// One property of a unit, as systemd prints it.
///
/// **Untested:** needs systemd.
pub async fn unit_property(unit: &str, property: &str) -> Option<String> {
    let output = tokio::process::Command::new("systemctl")
        .arg("show")
        .arg(format!("--property={property}"))
        .arg("--value")
        .arg(format!("{unit}.service"))
        .output()
        .await
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Ask systemd for the process, rather than remembering it.
///
/// **Untested:** needs a systemd unit.
pub async fn main_pid(unit: &str) -> Option<u32> {
    let pid: u32 = unit_property(unit, "MainPID").await?.parse().ok()?;
    (pid != 0).then_some(pid)
}

/// The command line a unit is running, which is where a node reads back what it
/// asked for the last time it started something.
///
/// **Untested:** needs systemd. What it is *for* is tested: see [`url_in`].
pub async fn unit_command(unit: &str) -> Option<String> {
    unit_property(unit, "ExecStart").await
}

/// **Untested:** needs systemd. A unit that is already gone is the state that
/// was wanted, so this reports nothing rather than failing.
pub async fn stop_unit(unit: &str) {
    let result = tokio::process::Command::new("systemctl")
        .arg("stop")
        .arg(format!("{unit}.service"))
        .output()
        .await;
    if let Ok(output) = result {
        if !output.status.success() {
            tracing::debug!(
                unit,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "systemctl stop said no"
            );
        }
    }
}

/// The migration URL inside a unit's command line.
///
/// A receiver publishes its URL by *being started with it*, and this reads it
/// back out. That is the difference between an agent that knows where its own
/// receiver is listening and one that remembers where it once put one: after a
/// restart, an upgrade, or a takeover by a different binary, the command line
/// is still there and the memory is not.
pub fn url_in(command: &str) -> Option<String> {
    command
        .split(|c: char| c.is_whitespace() || c == '"' || c == ';')
        .find_map(|token| {
            // The URL may be the whole argument (`-incoming tcp:0:4900`) or the
            // value half of one (`receiver_url=tcp:10.0.0.2:4901`), so this
            // looks inside the token rather than at its start.
            let scheme = token.find("unix:").or_else(|| token.find("tcp:"))?;
            Some(token[scheme..].trim_end_matches([',', '}']).to_string())
        })
}

/// A port in this node's migration range that nothing is already using.
///
/// `taken` is what the machine says is in use right now, not a list this
/// process has been keeping. Handing out a port twice would mean two receivers
/// racing for one bind, and the loser would be a guest that never arrives.
pub fn free_port(layout: &Layout, taken: &BTreeSet<u16>) -> Result<u16> {
    layout
        .migration_ports
        .clone()
        .find(|port| !taken.contains(port))
        .ok_or_else(|| {
            HostError::failed(format!(
                "every migration port between {} and {} on this node is in use",
                layout.migration_ports.start, layout.migration_ports.end
            ))
        })
}

/// The port out of a `tcp:host:port` URL, for counting what is in use.
pub fn port_of(url: &str) -> Option<u16> {
    url.strip_prefix("tcp:")?.rsplit(':').next()?.parse().ok()
}

// ---- what this machine is ------------------------------------------------

pub fn mem_total_mib(meminfo: &str) -> u64 {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib / 1024)
        .unwrap_or(0)
}

/// Free memory per NUMA node, so placement can refuse a host that has the total
/// but not on one node.
pub fn numa_free_mib() -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/node") else {
        return Vec::new();
    };
    let mut nodes: BTreeMap<u32, u64> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(index) = name
            .strip_prefix("node")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let info = std::fs::read_to_string(entry.path().join("meminfo")).unwrap_or_default();
        let free = info
            .lines()
            .find(|line| line.contains("MemFree:"))
            .and_then(|line| {
                let mut fields = line.split_whitespace().rev();
                fields.next(); // "kB"
                fields.next()?.parse::<u64>().ok()
            })
            .unwrap_or(0);
        nodes.insert(index, free / 1024);
    }
    nodes.into_values().collect()
}

/// What this machine offers, read off it.
pub fn capacity(layout: &Layout) -> velstra_cloud_model::resources::Capacity {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    velstra_cloud_model::resources::Capacity {
        vcpus: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0),
        memory_mib: mem_total_mib(&meminfo),
        disk_gib: layout.disk_gib,
        numa_free_mib: numa_free_mib(),
        hugepages_1gi: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_survives_the_trip_through_the_filesystem() {
        // `observe` maps a directory back to the object it belongs to. If this
        // is not reversible, a restarted agent cannot say which guest it found.
        let name = "projects/p1/instances/i1";
        assert_eq!(unslug(&slug(name)), name);
        assert!(!slug(name).contains('/'));
    }

    #[test]
    fn capacity_is_read_off_the_machine() {
        assert_eq!(
            mem_total_mib("MemTotal:       16316200 kB\nMemFree: 100 kB\n"),
            15933
        );
        assert_eq!(mem_total_mib("nothing useful"), 0);
    }

    #[test]
    fn a_receivers_url_is_read_back_out_of_the_unit_that_is_listening() {
        // This is how a node answers "where is my own receiver listening" after
        // the process that started it is gone. systemd prints ExecStart as a
        // record with the argv in it; the URL is the one token that can only be
        // a URL.
        let exec_start = "{ path=/usr/bin/ch-remote ; argv[]=/usr/bin/ch-remote \
                          --api-socket /var/lib/velstra/instances/p~i1/api.sock \
                          receive-migration receiver_url=tcp:10.0.0.2:4901 ; ignore_errors=no }";
        assert_eq!(url_in(exec_start).as_deref(), Some("tcp:10.0.0.2:4901"));

        let local = "{ path=/usr/bin/ch-remote ; argv[]=/usr/bin/ch-remote receive-migration \
                     unix:/var/lib/velstra/instances/p~i1/migrate.sock }";
        assert_eq!(
            url_in(local).as_deref(),
            Some("unix:/var/lib/velstra/instances/p~i1/migrate.sock")
        );

        // QEMU says where it is listening in the same place, in its own words.
        let qemu = "{ path=/usr/bin/qemu-system-x86_64 ; argv[]=-m 2048 -incoming tcp:0:4900 }";
        assert_eq!(url_in(qemu).as_deref(), Some("tcp:0:4900"));
        assert_eq!(port_of("tcp:0:4900"), Some(4900));

        // A unit that is not a receiver says nothing, rather than something
        // that would be sent to.
        assert_eq!(
            url_in("{ path=/usr/bin/qemu-system-x86_64 ; argv[]=-m 2048 }"),
            None
        );
    }

    #[test]
    fn a_port_is_only_handed_out_if_the_machine_is_not_already_using_it() {
        let layout = Layout {
            migration_ports: 4900..4903,
            ..Default::default()
        };
        assert_eq!(free_port(&layout, &BTreeSet::new()).unwrap(), 4900);
        assert_eq!(
            free_port(&layout, &BTreeSet::from([4900, 4901])).unwrap(),
            4902
        );
        // Out of ports is a sentence an operator can act on, not a collision
        // that shows up as a guest that never arrives.
        let err = free_port(&layout, &BTreeSet::from([4900, 4901, 4902])).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
    }

    #[test]
    fn a_url_gives_up_its_port() {
        assert_eq!(port_of("tcp:10.0.0.2:4901"), Some(4901));
        assert_eq!(port_of("unix:/tmp/sock"), None);
    }
}
