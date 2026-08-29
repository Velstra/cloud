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

/// Whose systemd runs the guests.
///
/// A production node runs them in the system manager, where a node's cgroup
/// limits and its slice live. A development node has no root and must still be
/// able to start something — and a platform whose data path can only be
/// exercised as root is a platform almost nobody exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    System,
    /// `systemd-run --user`, so a guest can be started by whoever is logged in.
    User,
}

impl Scope {
    /// The flag this scope adds to every systemd call, if any. One place, so
    /// the four call sites cannot drift into asking two different managers
    /// about one unit — which reads as "the guest vanished".
    fn flag(self) -> Option<&'static str> {
        match self {
            Scope::System => None,
            Scope::User => Some("--user"),
        }
    }
}

/// How a guest gets from powered-off to running its own code.
///
/// This used to be one `firmware: PathBuf` handed to both backends, and the two
/// of them read it differently in a way nobody could see until a guest was
/// actually started. Cloud Hypervisor's `--kernel` accepts either a Linux kernel
/// or a firmware blob; QEMU's `-kernel` accepts **only** a kernel, and a
/// firmware belongs on `-bios`. So the default — Cloud Hypervisor's
/// `hypervisor-fw` — was being passed to QEMU as if it were a Linux kernel.
///
/// The observable result, on this machine, with a stock Alpine cloud image:
/// nothing at all. No boot, and — because a directly-booted kernel gets no
/// `console=` unless somebody supplies one — not one byte on the serial either.
/// The same image booted in seconds through its own bootloader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// The guest boots itself: its own bootloader runs off its disk, and it
    /// chooses its kernel, its root and its console. This is what every stock
    /// cloud image expects, so it is the sane default for a platform that runs
    /// images it did not build.
    ///
    /// `None` means the VMM's built-in firmware, which is the right answer for
    /// QEMU (SeaBIOS) and not an answer at all for Cloud Hypervisor, which has
    /// none and must be given one.
    Firmware(Option<PathBuf>),
    /// Direct kernel boot: the host supplies the kernel, and with it the command
    /// line, because a kernel started this way has no bootloader to write one.
    ///
    /// The `cmdline` is not optional in practice even though the type allows an
    /// empty one: without `root=` the kernel cannot mount anything, and without
    /// `console=` it cannot say so.
    Kernel {
        kernel: PathBuf,
        cmdline: String,
        initrd: Option<PathBuf>,
    },
}

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
    /// Which systemd starts the guests. See [`Scope`].
    pub scope: Scope,
    /// How a guest is started.
    ///
    /// Whatever paths this names must exist at the **same place on both
    /// machines** for a migration: the guest's saved configuration names them,
    /// and the destination resolves those names against its own filesystem.
    pub boot: Boot,
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
            scope: Scope::System,
            boot: Boot::Firmware(Some(PathBuf::from(
                "/usr/share/cloud-hypervisor/hypervisor-fw",
            ))),
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

    /// Where the guest's serial output is kept.
    ///
    /// Every guest gets one, always. Without it a guest that fails to boot says
    /// nothing at all to anybody — the first time this platform started a real
    /// image, the evidence that it had not booted was an empty file that did not
    /// exist. A console is also the only thing that answers "why is it not
    /// coming up" for an image the operator did not build.
    pub fn console(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("console.log")
    }

    /// The guest's serial line, as something that can be attached to.
    ///
    /// Beside the log rather than instead of it. The log is what a dead guest
    /// leaves behind and it must not depend on anybody having been attached;
    /// this is what somebody who wants to *type* needs, and there is nothing to
    /// type at in a file.
    pub fn console_socket(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("console.sock")
    }
}

/// How large a guest's disk file is, in whole gibibytes.
///
/// The apparent size, not what it occupies: a sparse or backing-file disk
/// takes far less room than it presents, and what a guest sees is the number
/// its spec is about.
pub fn disk_gib(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|m| m.len() / (1024 * 1024 * 1024))
        .unwrap_or(0)
}

/// The last `cap` bytes a guest wrote to its console, and how much it wrote.
///
/// Seeks rather than reads: a guest that has been up for a month has a console
/// log measured in megabytes, and loading it to keep the last eight kibibytes
/// would make every observation pass proportional to guest uptime.
///
/// The cut lands on a byte boundary, not a character one, so the first line can
/// begin mid-UTF-8. `from_utf8_lossy` handles that — one replacement character
/// at the start of a tail is a better outcome than refusing to show a panic
/// because its first byte was inconvenient.
pub fn console_tail(path: &Path, cap: usize) -> (String, u64) {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), 0);
    };
    let Ok(total) = file.metadata().map(|m| m.len()) else {
        return (String::new(), 0);
    };
    let want = total.min(cap as u64);
    if file.seek(SeekFrom::End(-(want as i64))).is_err() {
        return (String::new(), total);
    }
    let mut buf = vec![0u8; want as usize];
    if file.read_exact(&mut buf).is_err() {
        return (String::new(), total);
    }
    (String::from_utf8_lossy(&buf).into_owned(), total)
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

/// A resource name as a **systemd unit** name.
///
/// Not [`slug`], because systemd does not accept `~` in a unit name: it escapes
/// it to `\x7e` and hands back
/// `velstra-vm-projects\x7ep1\x7einstances\x7ei1.service`, which is what an
/// operator then has to type, quote and read in `systemctl` output while trying
/// to work out why a guest will not start. It works — systemd normalises both
/// spellings — but every one of those is a place to get it wrong by hand.
///
/// `_` is in systemd's own allowed set, so nothing is escaped and the name says
/// what it is. The path layout keeps `~`; only the unit changes, and the two
/// have no reason to match.
pub fn unit_slug(name: &str) -> String {
    name.replace('/', "_")
}

/// The resource name a unit was made for.
pub fn from_unit_slug(slug: &str) -> String {
    slug.replace('_', "/")
}

pub fn unslug(slug: &str) -> String {
    slug.replace('~', "/")
}

/// A resource name as a **QMP node-name**.
///
/// A third spelling, for the same reason there is a second one: QEMU's block
/// layer accepts only `[A-Za-z0-9\-._]` in a `node-name`, and [`slug`] produces
/// `~`. Hot-plugging a disk answered
///
/// ```text
/// Invalid node-name: 'projects~p1~volumes~data'
/// ```
///
/// and the attachment sat at `attached: false` with that sentence on it. Found
/// live, on the first attach that got far enough to be refused for a reason
/// other than a path nobody wrote.
///
/// `_` like the unit spelling, and for the identical reason: it is in the
/// allowed set, so nothing is escaped and the name still says what it is.
pub fn qmp_id(name: &str) -> String {
    name.replace(['/', '~', ':'], "_")
}

/// The resource name a QMP id was made for, if it is one of ours.
///
/// `None` for anything that is not shaped like a resource name — the root disk,
/// a CD, whatever else a guest has attached. Guessing there would have the node
/// report a volume the cell has never heard of as open.
///
/// Lossy in principle: `_` in the original name is indistinguishable from a
/// separator. Resource ids do not carry one — see `ResourceName`'s own rules —
/// so in practice the round trip holds, and a name that broke it would be
/// reported as a volume nobody named rather than as the wrong volume.
pub fn from_qmp_id(id: &str) -> Option<String> {
    let name = id.replace('_', "/");
    let parsed = velstra_cloud_model::meta::ResourceName::parse(&name).ok()?;
    (parsed.collection() == "volumes").then_some(name)
}

// ---- images --------------------------------------------------------------

/// The sha256 an image name commits to, if it carries one.
/// The hex of a `sha256:…` value, whatever spelling it arrives in.
///
/// Read from the image's **spec**, not from its name. It used to be parsed out
/// of the name, which forced every image to be called `sha256-<64 hex>` — a
/// name no operator wants to see in a list where they are choosing an operating
/// system. The bytes are still addressed by their digest on disk; what changed
/// is that the object above them may be called `debian-13`.
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

/// Where a verified image lives on this node, if it is here.
///
/// `None` means it has not been published, which the caller must treat as "not
/// yet" rather than "boot without it": a disk made from no image is a disk with
/// no operating system on it.
pub fn image_path(layout: &Layout, digest: &str) -> Option<PathBuf> {
    let path = layout.image_dir.join(stored_as(digest)?);
    path.exists().then_some(path)
}

/// What a verified image is called on disk: `sha256-<hex>`, from its digest.
///
/// The digest and not the name, so two objects carrying the same bytes — a
/// project's copy of a catalogue image, the same image published twice under
/// different names — are one file on every node that has it. Names are for
/// people and there can be several; the bytes have exactly one identity.
pub fn stored_as(digest: &str) -> Option<String> {
    digest_of(digest).map(|hex| format!("sha256-{hex}"))
}

/// Fetch an image from `source` into the incoming directory, then verify and
/// publish it.
///
/// **The digest is the integrity control, not the transport.** The image's own
/// resource name carries its `sha256:`, and [`publish_image`] below refuses
/// anything that does not hash to it — so bytes that arrive over a plain,
/// unauthenticated connection still cannot make this node boot something other
/// than what the operator registered. That is the same property a container
/// layer or a Nix fixed-output derivation relies on, and it is why fetching over
/// `http://` is a defensible thing for this to do rather than a corner cut.
/// What a transport would add is confidentiality and knowing *who* served the
/// bytes; neither changes what gets booted.
///
/// Nothing is fetched when a verified copy is already published — an image is
/// content-addressed, so "already here" is a complete answer.
pub async fn fetch_image(layout: &Layout, image: &str, digest: &str, source: &str) -> Result<()> {
    // Refuse an image with no usable digest before spending a download on it:
    // publish would refuse it afterwards anyway, and saying so first costs the
    // operator a wait rather than a gigabyte.
    let name = stored_as(digest).ok_or_else(|| {
        HostError::failed(format!(
            "{image} carries no sha256 digest, so this node cannot verify what it downloads"
        ))
    })?;
    if layout.image_dir.join(&name).exists() {
        return Ok(());
    }

    let incoming = layout.incoming_dir.join(&name);
    if incoming.exists() {
        // A copy is already here — from a previous attempt, or placed by hand.
        // Verification below is what decides whether it is usable.
        return publish_image(layout, image, digest).await;
    }
    std::fs::create_dir_all(&layout.incoming_dir)?;

    // Downloaded to a private name and renamed into place, so a second agent
    // pass never reads a half-written file and calls it "already arrived".
    let partial = layout.incoming_dir.join(format!("{name}.partial"));
    let _ = std::fs::remove_file(&partial);
    match source.split_once("://") {
        Some(("file", path)) => {
            // A pre-seeded or shared-storage deployment: the bytes are already
            // on a filesystem this node can see. Copied rather than linked —
            // publish renames, and renaming a hard link would move the
            // operator's own file out from under them.
            tokio::fs::copy(path, &partial)
                .await
                .map_err(|e| HostError::failed(format!("copying {image} from {path}: {e}")))?;
        }
        Some(("http", _)) => fetch_http(source, &partial).await?,
        Some(("https", _)) => {
            // The bytes are verified against the digest either way — that is
            // what makes plain http safe here. What https buys is that the
            // image can be fetched from where images actually *are*: refusing
            // it meant telling an operator to go and find an http mirror of
            // Debian, and it is the reason every guest built from a normal
            // `https://cloud.debian.org/…` URL failed to start.
            fetch_https(source, &partial).await?;
        }
        _ => {
            let _ = std::fs::remove_file(&partial);
            return Err(HostError::failed(format!(
                "{image} has source {source:?}, which names no scheme this agent                  can fetch (http:// or file://)"
            )));
        }
    }
    std::fs::rename(&partial, &incoming)?;
    publish_image(layout, image, digest).await
}

/// Stream an `http://` URL into `dest`.
///
/// Streamed rather than buffered: an image is measured in gigabytes and this
/// runs on a hypervisor whose memory belongs to the guests.
/// Stream an `https://` URL into `dest`, with the certificate checked.
///
/// A separate function from [`fetch_http`] rather than a flag on it, because
/// they are different mechanisms: the plain one is a hand-rolled hyper request
/// with no trust in it at all, and this one is a TLS client that verifies the
/// server. Keeping them apart means neither can quietly become the other.
///
/// Streamed to disk, never held in memory: a cloud image is a gigabyte and a
/// node has better uses for that.
async fn fetch_https(url: &str, dest: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let response = reqwest::Client::builder()
        .user_agent(concat!("velstra-cloud/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| HostError::failed(format!("no https client: {e}")))?
        .get(url)
        .send()
        .await
        .map_err(|e| HostError::failed(format!("fetching {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(HostError::failed(format!(
            "fetching {url}: the server answered {}",
            response.status()
        )));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| HostError::failed(format!("{}: {e}", dest.display())))?;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| HostError::failed(format!("reading {url}: {e}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| HostError::failed(format!("writing {}: {e}", dest.display())))?;
    }
    file.flush()
        .await
        .map_err(|e| HostError::failed(format!("writing {}: {e}", dest.display())))?;
    Ok(())
}

async fn fetch_http(url: &str, dest: &Path) -> Result<()> {
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| HostError::failed(format!("{url} is not a URL: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| HostError::failed(format!("{url} names no host")))?;
    let port = uri.port_u16().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| HostError::failed(format!("connecting to {host}:{port}: {e}")))?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| HostError::failed(format!("http handshake with {host}: {e}")))?;
    // The connection task ends when the response body is done; a failure there
    // surfaces as a short read below rather than being swallowed here.
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let path = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let request = hyper::Request::builder()
        .uri(path)
        .header(hyper::header::HOST, host)
        .body(String::new())
        .map_err(|e| HostError::failed(format!("building request for {url}: {e}")))?;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|e| HostError::failed(format!("fetching {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(HostError::failed(format!(
            "fetching {url}: the server answered {}",
            response.status()
        )));
    }

    let mut file = tokio::fs::File::create(dest).await?;
    while let Some(frame) = response.frame().await {
        let frame = frame.map_err(|e| HostError::failed(format!("reading {url}: {e}")))?;
        if let Some(chunk) = frame.data_ref() {
            file.write_all(chunk).await?;
        }
    }
    file.flush().await?;
    Ok(())
}

/// Verify bytes that arrived in `incoming` and publish them under their digest.
///
/// Fetching them is [`fetch_image`]'s job; this one's is to refuse to boot
/// anything it has not hashed itself. Identical on both backends because it is
/// about bytes on a disk and not about a hypervisor.
pub async fn publish_image(layout: &Layout, image: &str, digest: &str) -> Result<()> {
    let expected = digest_of(digest).ok_or_else(|| {
        HostError::failed(format!(
            "{image} carries no sha256 digest, so this node cannot verify what it downloads"
        ))
    })?;
    let name = format!("sha256-{expected}");
    let published = layout.image_dir.join(&name);
    if published.exists() {
        return Ok(());
    }
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
/// The magic a qcow2 file begins with: `QFI\xfb`.
const QCOW2_MAGIC: [u8; 4] = [0x51, 0x46, 0x49, 0xfb];

/// What a file's first bytes say it is, which is not always what somebody
/// declared it to be.
fn looks_like_qcow2(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4];
    matches!(
        std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)),
        Ok(())
    ) && head == QCOW2_MAGIC
}

/// `qemu-img convert -O raw`, which is the only sane way to read qcow2 —
/// doing it here would be reimplementing a disk format.
async fn convert_to_raw(from: &Path, to: &Path) -> Result<()> {
    let out = tokio::process::Command::new("qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(from)
        .arg(to)
        .output()
        .await
        .map_err(|e| HostError::failed(format!("running qemu-img convert: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    Err(HostError::failed(format!(
        "qemu-img could not convert {} to raw: {}",
        from.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

pub async fn create_disk(
    layout: &Layout,
    instance: &str,
    gib: u64,
    image: Option<&Path>,
    format: velstra_cloud_model::resources::ImageFormat,
) -> Result<()> {
    let dir = layout.dir(instance);
    std::fs::create_dir_all(&dir)?;
    let path = layout.disk(instance);
    if path.exists() {
        return Ok(());
    }
    // Written under a temporary name and renamed, so an interrupted creation
    // cannot leave a short disk that looks finished — and, now that the image
    // is copied in, cannot leave a *half-copied* one that looks bootable.
    let partial = dir.join("root.raw.partial");
    match image {
        // The disk starts life as the image. Nothing between "created" and
        // "has an operating system on it": a guest started off an empty disk
        // fails in the least legible way there is, and before this the image
        // was pulled, verified, and then never used for anything.
        Some(image) => {
            // A qcow2 image is *converted*, never copied.
            //
            // This used to be `fs::copy` whatever the image was, while the disk
            // is handed to the VMM as `format=raw` — so a qcow2 image produced a
            // root disk whose first four bytes were `QFI\xfb` and whose boot
            // sector was a qcow2 header. The guest started, found no
            // bootloader, and sat with an empty console. Nothing failed: the
            // disk was made, the VMM ran, and the platform reported a guest
            // that could never boot. Practically every public cloud image is
            // qcow2, so this was the first thing a new operator hit.
            //
            // `ImageFormat` existed for exactly this and was read by nobody.
            let declared_qcow2 = format == velstra_cloud_model::resources::ImageFormat::Qcow2;
            let really_qcow2 = looks_like_qcow2(image);
            if declared_qcow2 != really_qcow2 {
                // The declaration and the bytes disagree, and picking one to
                // believe is how the wrong one wins quietly. The digest proves
                // *which* bytes these are and says nothing about what they are.
                return Err(HostError::failed(format!(
                    "{} is registered as {:?} and its first bytes say otherwise; \
                     correct spec.format on the image rather than have this node \
                     decide which of the two to believe",
                    image.display(),
                    format
                )));
            }
            if really_qcow2 {
                convert_to_raw(image, &partial).await?;
            } else {
                std::fs::copy(image, &partial)?;
            }
        }
        None => {
            std::fs::File::create(&partial)?;
        }
    }
    let file = std::fs::OpenOptions::new().write(true).open(&partial)?;
    // Grow, never shrink: the image may already be larger than the size asked
    // for, and truncating it would cut the filesystem off at the knees.
    let wanted = gib.saturating_mul(1024 * 1024 * 1024);
    if file.metadata()?.len() < wanted {
        file.set_len(wanted)?;
    }
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
    scope: Scope,
    unit: &str,
    slice: &str,
    dir: &Path,
    program: &str,
    args: &[OsString],
) -> Result<()> {
    let mut command = tokio::process::Command::new("systemd-run");
    if let Some(flag) = scope.flag() {
        command.arg(flag);
    }
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
pub async fn unit_is_active(scope: Scope, unit: &str) -> bool {
    let mut command = tokio::process::Command::new("systemctl");
    if let Some(flag) = scope.flag() {
        command.arg(flag);
    }
    let Ok(output) = command
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
pub async fn unit_property(scope: Scope, unit: &str, property: &str) -> Option<String> {
    let mut command = tokio::process::Command::new("systemctl");
    if let Some(flag) = scope.flag() {
        command.arg(flag);
    }
    let output = command
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
pub async fn main_pid(scope: Scope, unit: &str) -> Option<u32> {
    let pid: u32 = unit_property(scope, unit, "MainPID").await?.parse().ok()?;
    (pid != 0).then_some(pid)
}

/// The command line a unit is running, which is where a node reads back what it
/// asked for the last time it started something.
///
/// **Untested:** needs systemd. What it is *for* is tested: see [`url_in`].
pub async fn unit_command(scope: Scope, unit: &str) -> Option<String> {
    unit_property(scope, unit, "ExecStart").await
}

/// Why a guest's unit is not running, when systemd knows and nothing else does.
///
/// **The case this exists for is a VMM that died before it opened its monitor.**
/// A guest is observed by connecting to that socket, and a QEMU that exits at
/// argument-parsing never creates one — so the whole instance directory is
/// skipped, `host.vms` has no entry, and the control plane is told the guest is
/// stopped with no reason attached. Found live: a CPU baseline the platform
/// itself advised produced
///
/// ```text
/// qemu-system-x86_64: unable to find CPU model 'x86-64-v1'
/// ```
///
/// and every screen said `Stopped`, `HostActions: Done`, empty console. The only
/// place the reason existed was the guest unit's journal, which is not somewhere
/// a tenant can look and not somewhere an operator thinks to look first.
///
/// `None` when the unit is fine, absent, or systemd cannot be asked — all three
/// mean "nothing to say here", and inventing a failure would be worse than
/// saying nothing.
pub async fn unit_failure(scope: Scope, unit: &str) -> Option<String> {
    if unit_property(scope, unit, "Result").await? == "success" {
        return None;
    }
    let status = unit_property(scope, unit, "ExecMainStatus")
        .await
        .unwrap_or_default();
    let result = unit_property(scope, unit, "Result")
        .await
        .unwrap_or_default();
    // The last thing it said, which is the part somebody actually needs. A
    // status code alone sends people to the journal; this saves the trip.
    let said = last_words(scope, unit).await;
    Some(match said {
        Some(words) if !words.is_empty() => words,
        _ => format!("the hypervisor exited ({result}, status {status}) without saying why"),
    })
}

/// The last line a failed unit logged.
async fn last_words(scope: Scope, unit: &str) -> Option<String> {
    let mut command = tokio::process::Command::new("journalctl");
    if scope.flag() == Some("--user") {
        command.arg("--user");
    }
    let out = command
        .args([
            "-u",
            &format!("{unit}.service"),
            "-n",
            "20",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // systemd's own lines say a unit failed; what is wanted is what the *program*
    // said before it did.
    text.lines()
        .rev()
        .find(|l| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with("Started ") && !l.starts_with("Stopped ")
        })
        .map(|l| l.trim().to_string())
}

/// **Untested:** needs systemd. A unit that is already gone is the state that
/// was wanted, so this reports nothing rather than failing.
pub async fn stop_unit(scope: Scope, unit: &str) {
    let mut command = tokio::process::Command::new("systemctl");
    if let Some(flag) = scope.flag() {
        command.arg(flag);
    }
    let result = command
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

/// Memory a guest could actually get on each NUMA node, so placement can refuse
/// a host that has the total but not on one node.
///
/// **Not `MemFree`**, and that distinction is the difference between a node that
/// keeps working and one that stops accepting guests after a day.
///
/// `MemFree` is memory the kernel is holding *nothing* in, and on a healthy
/// Linux that number trends to almost zero: everything else is page cache,
/// which the kernel gives back the moment somebody asks. Reading it as capacity
/// was measured on a real node with 15 GiB of memory and one 2 GiB guest —
/// which reported **245 MiB** free, and the scheduler refused every placement
/// with "no single NUMA node holds 1024 MiB". Nothing was wrong with the
/// machine; it had simply been up long enough to read some files.
///
/// So: free, plus what the kernel would reclaim without hesitating — the file
/// pages and the reclaimable slab. That is the same reasoning `MemAvailable`
/// uses in `/proc/meminfo`, which has no per-node equivalent, so it is computed
/// here from the fields a node's own `meminfo` does carry.
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
        nodes.insert(index, available_mib(&info));
    }
    nodes.into_values().collect()
}

/// What a guest could get on one node, from that node's own `meminfo`.
///
/// Free plus the two things the kernel hands back without being asked twice:
/// file-backed pages and the reclaimable slab. Pure, so the arithmetic is
/// testable against a real file rather than against a machine.
pub fn available_mib(meminfo: &str) -> u64 {
    let field = |name: &str| -> u64 {
        meminfo
            .lines()
            .find(|line| line.contains(name))
            .and_then(|line| {
                let mut fields = line.split_whitespace().rev();
                fields.next(); // "kB"
                fields.next()?.parse::<u64>().ok()
            })
            .unwrap_or(0)
    };
    // `FilePages` is the whole file cache on a node; the active/inactive pair
    // is what a node's meminfo carries on kernels that do not publish it, and
    // taking the larger of the two readings is how this stays right on both
    // rather than silently reading zero on one.
    let file = field("FilePages:").max(field("Active(file):") + field("Inactive(file):"));
    let kib = field("MemFree:") + file + field("SReclaimable:");
    kib / 1024
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

    /// The tail is the *last* bytes, and the total is the whole file.
    ///
    /// Both halves matter: a reader that saw eight kibibytes with no total
    /// would take it for everything the guest ever said, and go looking for a
    /// panic that scrolled off an hour ago.
    #[test]
    fn a_console_tail_is_the_end_of_the_log_and_says_how_big_the_log_is() {
        let dir = std::env::temp_dir().join(format!("velstra-console-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");

        // Shorter than the cap: the whole thing, and the total agrees.
        std::fs::write(&path, b"boot\nok\n").unwrap();
        let (tail, total) = console_tail(&path, 8192);
        assert_eq!(tail, "boot\nok\n");
        assert_eq!(total, 8);

        // Longer than the cap: the end, not the beginning.
        let long: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, long.as_bytes()).unwrap();
        let (tail, total) = console_tail(&path, 64);
        assert_eq!(total, long.len() as u64);
        assert_eq!(tail.len(), 64);
        assert!(tail.ends_with("line 499\n"), "{tail:?}");
        assert!(!tail.contains("line 0\n"), "the tail carried the start");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cut that lands mid-character shows the tail anyway.
    ///
    /// The alternative is refusing to show a panic because its first byte was
    /// inconvenient, which is the wrong trade at exactly the moment somebody
    /// needs to read it.
    #[test]
    fn a_tail_that_starts_mid_character_is_still_shown() {
        let dir = std::env::temp_dir().join(format!("velstra-console-utf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("console.log");

        // A three-byte character, then plain ASCII. A four-byte tail cuts it.
        std::fs::write(&path, "€abcd".as_bytes()).unwrap();
        let (tail, _) = console_tail(&path, 4);
        assert!(tail.ends_with("abcd"), "{tail:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A guest with no log yet says nothing, rather than failing.
    ///
    /// Every guest is in this state for the first moments of its life, and an
    /// observation pass that errored on it would take the whole node down for
    /// a file that is about to exist.
    #[test]
    fn a_missing_console_log_reads_as_silence() {
        let (tail, total) = console_tail(Path::new("/nonexistent/console.log"), 8192);
        assert!(tail.is_empty());
        assert_eq!(total, 0);
    }

    /// A unit name systemd takes as written.
    ///
    /// Asserted against systemd's own allowed set rather than against `_`,
    /// because the point is not the character — it is that nothing gets
    /// escaped. With `~` the name came back as
    /// `velstra-vm-projects\x7ep1\x7einstances\x7ei1.service`, which is what an
    /// operator then has to read and quote while working out why a guest will
    /// not start.
    #[test]
    fn a_unit_name_needs_no_escaping_and_survives_the_round_trip() {
        let name = "projects/p1/instances/i1";
        let unit = unit_slug(name);
        assert_eq!(from_unit_slug(&unit), name);
        assert!(
            unit.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@')),
            "systemd would escape {unit}"
        );
        // The path layout is a different question and keeps its own separator:
        // nothing requires the two to agree, and `~` reads better in a path.
        assert!(slug(name).contains('~'));
    }
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

    /// A node stops accepting guests once it has read some files.
    ///
    /// `MemFree` on a healthy Linux trends to almost nothing: the rest is page
    /// cache, which the kernel hands back the moment somebody asks for it.
    /// Reading it as capacity was measured on a real machine with 15 GiB and
    /// one 2 GiB guest — it reported 245 MiB, and the scheduler refused every
    /// placement with "no single NUMA node holds 1024 MiB". Nothing was wrong
    /// with the machine.
    #[test]
    fn what_a_guest_could_get_is_not_what_is_untouched() {
        // A node that has been up a while: almost nothing free, almost
        // everything cache.
        let busy = "Node 0 MemTotal:       16316200 kB\n                    Node 0 MemFree:          251000 kB\n                    Node 0 FilePages:      11000000 kB\n                    Node 0 SReclaimable:     400000 kB\n";
        let available = available_mib(busy);
        assert!(
            available > 10_000,
            "a node with 11 GiB of reclaimable cache reported {available} MiB"
        );

        // A node that is genuinely full says so, or this would have turned the
        // check into a rubber stamp.
        let full = "Node 0 MemTotal:       16316200 kB\n                    Node 0 MemFree:          120000 kB\n                    Node 0 FilePages:         30000 kB\n                    Node 0 SReclaimable:      10000 kB\n";
        assert!(available_mib(full) < 200, "{}", available_mib(full));

        // A kernel that publishes the active/inactive pair instead of the total
        // is read just as well.
        let split = "Node 0 MemFree:          251000 kB\n                     Node 0 Active(file):    6000000 kB\n                     Node 0 Inactive(file):  5000000 kB\n                     Node 0 SReclaimable:     400000 kB\n";
        assert!(available_mib(split) > 10_000, "{}", available_mib(split));

        // And a file with none of it is zero rather than a panic.
        assert_eq!(available_mib(""), 0);
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

#[cfg(test)]
mod names_a_hypervisor_will_take {
    use super::*;

    #[test]
    fn a_qmp_node_name_carries_nothing_qemu_refuses() {
        // QEMU's block layer takes `[A-Za-z0-9\-._]` and nothing else. Handed a
        // slug it answered `Invalid node-name: 'projects~p1~volumes~data'`, and
        // the attachment carried that sentence instead of a disk.
        let id = qmp_id("projects/p1/volumes/data");
        assert_eq!(id, "projects_p1_volumes_data");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')),
            "{id} carries something QEMU will refuse"
        );
    }

    #[test]
    fn a_slug_that_has_been_through_the_path_layout_is_taken_too() {
        assert_eq!(
            qmp_id("projects~p1~volumes~data"),
            "projects_p1_volumes_data"
        );
    }

    #[test]
    fn the_round_trip_holds_for_the_names_this_platform_makes() {
        // It has to: the node learns which volumes a guest has open by reading
        // these back off `query-block`. A round trip that did not hold would
        // leave `attached` false for ever, which is how the platform ended up
        // asking QEMU to plug in a disk it already had — `Duplicate nodes with
        // node-name='…'`, once a pass, while the guest ran perfectly.
        for name in [
            "projects/p1/volumes/data",
            "projects/kunde-3/volumes/db-1",
            "projects/p1/volumes/volume-e0fc2ec6",
        ] {
            assert_eq!(from_qmp_id(&qmp_id(name)).as_deref(), Some(name));
        }
    }

    #[test]
    fn a_device_that_is_not_ours_is_not_claimed_as_a_volume() {
        // A guest has a root disk and may have a CD. Reading either back as a
        // volume would have the node report one the cell never made.
        for id in ["root", "virtio-disk0", "ide0-cd0", ""] {
            assert_eq!(from_qmp_id(id), None, "{id} was read as a volume");
        }
    }
}
