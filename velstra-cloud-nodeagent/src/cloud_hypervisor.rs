//! Cloud Hypervisor, one process per guest, under systemd.
//!
//! ## What is true on this machine, and what is not
//!
//! Cloud Hypervisor is **not installed on the machine this was written on**, so
//! everything that requires the binary or a running guest — `start`, `stop`,
//! `delete`, the whole of the migration half, the API calls in
//! [`CloudHypervisorVmm::api`], and `observe` of a live VM — is written from the
//! documented API and has **never been run**. It is marked as such on each
//! method. What *is* exercised by the tests at the bottom of this file, because
//! it needs nothing but a filesystem: the run directory layout and the name
//! encoding, sparse disk creation, image verification, the HTTP framing over the
//! API socket, the shape of the `ch-remote` argument lists, and capacity
//! parsing.
//!
//! ## Why systemd owns the process and not this agent
//!
//! Each guest is started with `systemd-run` into its own transient unit inside
//! a slice. That is not decoration:
//!
//! - **The guest outlives the agent.** Its parent is systemd, so restarting,
//!   upgrading or crashing the agent cannot take a tenant's workload with it.
//!   An agent that forks its VMs as children has an upgrade path that is also
//!   an outage. The same argument applies twice over to a migration, which is
//!   why the receiver and the sender are units too: an agent restarted half way
//!   through a transfer finds it still running.
//! - **The slice is where the limits go.** CPU, memory and IO accounting for
//!   guests belong in one cgroup subtree, separate from the agent's own, so a
//!   busy node throttles guests rather than the thing that manages them.
//! - **systemd is the registry.** After a restart the agent asks systemd which
//!   units exist rather than reading a file it wrote — one source of truth on
//!   the node, and it is the node.
//!
//! ## Why the run directory is not a database
//!
//! `observe` reads the directory, the API socket and systemd. Nothing in it was
//! written to remember a decision: a directory exists because a disk is in it,
//! a socket answers because a VMM is alive, a receiver is ready because its unit
//! is active — and the URL it is listening on is read back out of that unit's
//! own command line. An image file is present *only* after its bytes hashed to
//! the digest in its name, so "cached" cannot be believed into existence.
//!
//! ## The order a migration happens in
//!
//! Not ours, and not negotiable — it is what Cloud Hypervisor documents:
//!
//! 1. The destination starts an empty `cloud-hypervisor --api-socket=…`.
//! 2. The destination runs `ch-remote --api-socket=… receive-migration
//!    receiver_url=<URL>`, which listens until a guest arrives.
//! 3. **Only then** the source runs `ch-remote --api-socket=… send-migration
//!    destination_url=<URL>`.
//!
//! A URL is `unix:/path` (one machine, which is what an in-place VMM upgrade
//! is) or `tcp:host:port` (two machines). TLS is available over TCP and only
//! over TCP. More than one connection is unsupported over a unix socket. The
//! kernel and initramfs must be at the same path on both machines, and the two
//! protocol versions must match or be one apart — which is why the model
//! refuses a version gap before anything is copied.

use std::{collections::BTreeSet, ffi::OsString, path::PathBuf, time::Duration};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use velstra_cloud_model::{
    migration::MigrationMode,
    resources::{Capacity, InstanceState},
};

pub use crate::hostfs::Layout;
use crate::{
    host::{HostError, HostState, Receiver, Result, Transfer, VmObservation, VmRequest, Vmm},
    hostfs::{self, slug, unslug},
};

/// How long to wait for a VMM's API socket to appear before giving up on it.
///
/// A receiver has to exist before `ch-remote` can be pointed at it, and the VMM
/// binds its socket a moment after systemd returns. Waiting is bounded so a VMM
/// that never came up is a failure on the object rather than a pass that never
/// ends.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

pub struct CloudHypervisorVmm {
    layout: Layout,
}

impl CloudHypervisorVmm {
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn dir(&self, instance: &str) -> PathBuf {
        self.layout.dir(instance)
    }

    fn disk(&self, instance: &str) -> PathBuf {
        self.layout.disk(instance)
    }

    fn socket(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("api.sock")
    }

    /// The socket a local transfer arrives over. Local means one machine, and
    /// the only reason to move a guest to the machine it is already on is to
    /// hand it to a newer VMM.
    fn migrate_socket(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("migrate.sock")
    }

    /// The guest's VMM. A receiver runs under this same name on purpose: the
    /// VMM that takes delivery *is* the VMM that then runs the guest, so a
    /// destination has one unit for an instance and not two.
    fn unit(&self, instance: &str) -> String {
        format!("velstra-vm-{}", slug(instance))
    }

    /// The `ch-remote receive-migration` that waits for a guest. Separate,
    /// because it blocks until the transfer completes and its liveness is the
    /// answer to "is a receiver listening".
    fn receive_unit(&self, instance: &str) -> String {
        format!("velstra-recv-{}", slug(instance))
    }

    /// The `ch-remote send-migration` that copies a guest away. Also separate,
    /// and also for its liveness: a transfer under way must not be started a
    /// second time by the next pass.
    fn send_unit(&self, instance: &str) -> String {
        format!("velstra-send-{}", slug(instance))
    }

    /// One request to a guest's API socket.
    ///
    /// **Untested:** needs a live Cloud Hypervisor. The framing it builds and
    /// the response parsing it uses are tested separately against bytes.
    async fn api(&self, instance: &str, method: &str, path: &str, body: &str) -> Result<String> {
        let socket = self.socket(instance);
        let mut stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|e| {
                HostError::failed(format!("{} is not answering: {e}", socket.display()))
            })?;
        stream
            .write_all(request(method, path, body).as_bytes())
            .await?;
        stream.flush().await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        parse_response(&raw)
    }

    /// What is listening here for that guest, if anything.
    ///
    /// **Untested:** needs systemd. The unit is asked whether it is active and
    /// then asked what it was started with — so a receiver that died stops
    /// being ready on the next pass, and one started by a previous agent is
    /// found by this one with its URL intact.
    async fn observe_receiver(&self, instance: &str) -> Option<Receiver> {
        let unit = self.receive_unit(instance);
        if !hostfs::unit_is_active(&unit).await {
            return None;
        }
        let url = hostfs::url_in(&hostfs::unit_command(&unit).await?)?;
        Some(Receiver {
            // Cloud Hypervisor does not tell the receiving side how much has
            // arrived — there is no counter in `vm.info` and none in
            // `ch-remote` — so this node reports nothing rather than inventing
            // progress. An operator watching a large guest move sees the
            // condition change, not a bar.
            url,
            received_mib: 0,
        })
    }

    /// The migration ports this machine is already using, read off the units
    /// that are using them.
    async fn ports_in_use(&self, instances: &BTreeSet<String>) -> BTreeSet<u16> {
        let mut taken = BTreeSet::new();
        for instance in instances {
            if let Some(receiver) = self.observe_receiver(instance).await {
                if let Some(port) = hostfs::port_of(&receiver.url) {
                    taken.insert(port);
                }
            }
        }
        taken
    }

    /// Wait for a VMM to bind its API socket.
    ///
    /// **Untested:** needs a live Cloud Hypervisor.
    async fn await_socket(&self, instance: &str) -> Result<()> {
        let socket = self.socket(instance);
        let deadline = std::time::Instant::now() + SOCKET_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(HostError::failed(format!(
            "{} never appeared; the VMM did not come up",
            socket.display()
        )))
    }
}

#[async_trait]
impl Vmm for CloudHypervisorVmm {
    /// Re-derive everything from the machine.
    ///
    /// **Partly untested:** the directory and image scan are exercised by the
    /// tests below; asking a live VMM for its state, and asking systemd about
    /// receivers and transfers, are not.
    async fn observe(&self) -> Result<HostState> {
        let mut host = HostState::default();

        for digest in hostfs::read_dir_names(&self.layout.image_dir)? {
            // A file is only ever moved in here after its bytes hashed to this
            // name, so presence is verification. There is no marker file to
            // trust, and none to go stale.
            host.images.insert(unslug(&digest));
        }

        for entry in hostfs::read_dir_names(&self.layout.run_dir)? {
            let instance = unslug(&entry);
            let dir = self.layout.run_dir.join(&entry);
            if dir.join("root.raw").exists() {
                host.disks.insert(instance.clone());
            }
            if hostfs::unit_is_active(&self.send_unit(&instance)).await {
                host.sending.insert(instance.clone());
            }
            if let Some(receiver) = self.observe_receiver(&instance).await {
                // A VMM in receive mode answers on its socket, but there is no
                // guest behind it yet. Reporting one would tell the control
                // plane the instance is here — and the source, which still has
                // it, would be the second copy.
                host.receivers.insert(instance, receiver);
                continue;
            }
            if !self.socket(&instance).exists() {
                // No socket: nothing of this guest is running. The disk stays,
                // which is why a stopped instance keeps its data.
                continue;
            }
            let observation = match self.api(&instance, "GET", "/api/v1/vm.info", "").await {
                Ok(body) => VmObservation {
                    state: state_of(&body),
                    pid: hostfs::main_pid(&self.unit(&instance)).await,
                    started_at: hostfs::started_at(&dir),
                },
                // The socket is there and nobody is behind it: the VMM died and
                // left its socket. That is a failure, and it is reported as one
                // rather than as "stopped", because the two want different
                // things done about them.
                Err(_) => VmObservation {
                    state: InstanceState::Failed,
                    pid: None,
                    started_at: hostfs::started_at(&dir),
                },
            };
            host.vms.insert(instance, observation);
        }

        Ok(host)
    }

    /// Verify bytes that arrived in `incoming` and publish them under their
    /// digest. Fetching them is somebody else's job — this node's job is to
    /// refuse to boot anything it has not hashed itself.
    async fn pull_image(&self, image: &str) -> Result<()> {
        hostfs::publish_image(&self.layout, image).await
    }

    /// A sparse file of the asked-for size. Real, and covered by a test.
    async fn create_disk(&self, instance: &str, gib: u64) -> Result<()> {
        hostfs::create_disk(&self.layout, instance, gib).await
    }

    /// **Untested:** requires `cloud-hypervisor` and `systemd-run`.
    async fn start(&self, request: &VmRequest) -> Result<()> {
        let dir = self.dir(&request.instance);
        std::fs::create_dir_all(&dir)?;
        let socket = self.socket(&request.instance);
        // A socket left by a dead VMM would make the new one fail to bind.
        let _ = std::fs::remove_file(&socket);

        hostfs::systemd_run(
            &self.unit(&request.instance),
            &self.layout.slice,
            &dir,
            &self.layout.binary,
            &vmm_args(&self.layout, request, &socket),
        )
        .await?;
        // Boot is asynchronous, and this method does not wait for it. Whether
        // the guest is running is answered by `observe`, on the next round of
        // the same pass — there is no place here to write "starting".
        Ok(())
    }

    /// **Untested:** ACPI power button, the graceful stop.
    ///
    /// A guest that ignores ACPI stays running, and the agent will ask again on
    /// every pass until its round cap trips and says so on the object. That is
    /// deliberate: a node that escalates to a kill on its own would make an
    /// unclean shutdown a silent event.
    async fn stop(&self, instance: &str) -> Result<()> {
        self.api(instance, "PUT", "/api/v1/vm.power-button", "")
            .await
            .map(|_| ())
    }

    /// **Untested:** stops the unit and removes everything of the guest.
    async fn delete(&self, instance: &str) -> Result<()> {
        if self.socket(instance).exists() {
            let _ = self.api(instance, "PUT", "/api/v1/vm.shutdown", "").await;
        }
        // Anything this instance had in flight goes with it. A transfer of a
        // guest that is being deleted has nowhere to land.
        hostfs::stop_unit(&self.send_unit(instance)).await;
        hostfs::stop_unit(&self.receive_unit(instance)).await;
        hostfs::stop_unit(&self.unit(instance)).await;
        let dir = self.dir(instance);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// **Untested:** hot-plugs a volume into a running guest.
    async fn open_volume(&self, instance: &str, volume: &str, read_only: bool) -> Result<String> {
        let path = self.layout.run_dir.join(slug(instance)).join(slug(volume));
        let body = serde_json::json!({
            "path": path.to_string_lossy(),
            "readonly": read_only,
        })
        .to_string();
        let response = self
            .api(instance, "PUT", "/api/v1/vm.add-disk", &body)
            .await?;
        // Cloud Hypervisor answers with the PCI address it plugged it into;
        // the device name the guest sees is derived from the order, and the
        // honest thing to report is what the hypervisor said.
        Ok(response.trim().to_string())
    }

    /// **Untested:** removes the disk from the running guest.
    async fn close_volume(&self, instance: &str, volume: &str) -> Result<()> {
        let body = serde_json::json!({ "id": slug(volume) }).to_string();
        self.api(instance, "PUT", "/api/v1/vm.remove-device", &body)
            .await
            .map(|_| ())
    }

    /// From `/proc` and `/sys`, which is where the truth about a Linux host is.
    async fn capacity(&self) -> Result<Capacity> {
        Ok(hostfs::capacity(&self.layout))
    }

    /// **Untested:** requires `cloud-hypervisor`, `ch-remote` and `systemd-run`.
    ///
    /// The two documented steps, in the documented order: an empty VMM, then a
    /// `receive-migration` pointed at it. Both are units, so a receiver started
    /// by one agent is found by its successor.
    async fn prepare_receiver(&self, request: &VmRequest, _mode: MigrationMode) -> Result<String> {
        if let Some(receiver) = self.observe_receiver(&request.instance).await {
            // Already listening. Asking twice must not bind a second port and
            // must not answer with a URL nothing is behind.
            return Ok(receiver.url);
        }
        let dir = self.dir(&request.instance);
        std::fs::create_dir_all(&dir)?;
        if !self.disk(&request.instance).exists() {
            // The guest resumes into its own root disk, by the path its saved
            // configuration names. A receiver without one takes delivery of a
            // guest that cannot run.
            return Err(HostError::failed(format!(
                "{} has no root disk on this node",
                request.instance
            )));
        }

        let url = match &self.layout.migration_address {
            Some(address) => {
                let instances: BTreeSet<String> = hostfs::read_dir_names(&self.layout.run_dir)?
                    .iter()
                    .map(|entry| unslug(entry))
                    .collect();
                let port = hostfs::free_port(&self.layout, &self.ports_in_use(&instances).await)?;
                format!("tcp:{address}:{port}")
            }
            None => {
                let socket = self.migrate_socket(&request.instance);
                // A socket left behind by an abandoned receiver would stop the
                // new one from binding.
                let _ = std::fs::remove_file(&socket);
                format!("unix:{}", socket.display())
            }
        };

        // The VMM first, empty. It is started under the guest's own unit name
        // because in a moment it will be the guest's VMM.
        let socket = self.socket(&request.instance);
        let _ = std::fs::remove_file(&socket);
        hostfs::systemd_run(
            &self.unit(&request.instance),
            &self.layout.slice,
            &dir,
            &self.layout.binary,
            &[OsString::from("--api-socket"), socket.clone().into()],
        )
        .await?;
        self.await_socket(&request.instance).await?;

        // Then the receiver, which listens until the guest arrives.
        hostfs::systemd_run(
            &self.receive_unit(&request.instance),
            &self.layout.slice,
            &dir,
            "ch-remote",
            &receive_args(&socket, &url),
        )
        .await?;
        Ok(url)
    }

    /// **Untested:** needs systemd.
    async fn tear_down_receiver(&self, instance: &str) -> Result<()> {
        hostfs::stop_unit(&self.receive_unit(instance)).await;
        // The order of these two checks is the whole method. Once a transfer
        // has landed, the receiver *is* the guest's VMM: stopping that unit
        // would kill the guest this migration just moved here. So the VMM is
        // only stopped when the machine says there is no guest behind it.
        let holds_a_guest = self.socket(instance).exists()
            && matches!(
                self.api(instance, "GET", "/api/v1/vm.info", "").await,
                Ok(body) if state_of(&body) != InstanceState::Failed
            );
        if !holds_a_guest {
            hostfs::stop_unit(&self.unit(instance)).await;
            let _ = std::fs::remove_file(self.socket(instance));
        }
        let _ = std::fs::remove_file(self.migrate_socket(instance));
        Ok(())
    }

    /// **Untested:** requires `ch-remote` and `systemd-run`.
    ///
    /// Returns as soon as the transfer is running, not when it has finished:
    /// `ch-remote send-migration` blocks for as long as the copy takes, so it
    /// goes into a unit of its own and the pass moves on. `observe` is what
    /// says whether it is still going, and the guest leaving this machine is
    /// what says it worked.
    async fn send(&self, transfer: &Transfer) -> Result<()> {
        let args = send_args(&self.layout, transfer, &self.socket(&transfer.instance))?;
        hostfs::systemd_run(
            &self.send_unit(&transfer.instance),
            &self.layout.slice,
            &self.dir(&transfer.instance),
            "ch-remote",
            &args,
        )
        .await
    }

    /// **Untested:** needs systemd.
    ///
    /// Cloud Hypervisor's own way to abandon a transfer is `timeout_strategy`,
    /// which is a decision made when the send starts. What is left for a
    /// cancellation somebody asked for afterwards is to stop the transfer: the
    /// guest is still running here, because under pre-copy it never stopped.
    async fn cancel_send(&self, instance: &str) -> Result<()> {
        hostfs::stop_unit(&self.send_unit(instance)).await;
        Ok(())
    }
}

/// The command line for a guest's VMM.
///
/// Pure, so the argument list can be argued about without a hypervisor.
fn vmm_args(layout: &Layout, request: &VmRequest, socket: &std::path::Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--api-socket".into(),
        socket.into(),
        "--cpus".into(),
        format!("boot={}", request.vcpus).into(),
        "--memory".into(),
        format!("size={}M", request.memory_mib).into(),
        "--kernel".into(),
        layout.firmware.clone().into(),
        "--disk".into(),
        format!("path={}", layout.disk(&request.instance).display()).into(),
    ];
    for tap in &request.taps {
        args.push("--net".into());
        args.push(format!("tap={tap}").into());
    }
    args
}

/// `ch-remote … receive-migration receiver_url=<URL>`, as documented.
fn receive_args(socket: &std::path::Path, url: &str) -> Vec<OsString> {
    vec![
        "--api-socket".into(),
        socket.into(),
        "receive-migration".into(),
        format!("receiver_url={url}").into(),
    ]
}

/// `ch-remote … send-migration destination_url=<URL> …`, as documented.
///
/// The one thing this refuses outright is the combination Cloud Hypervisor
/// documents as unsupported: more than one connection over a unix socket. A
/// backend that passed it through would fail after the receiver was up, which
/// is a more expensive way to learn the same thing.
fn send_args(
    layout: &Layout,
    transfer: &Transfer,
    socket: &std::path::Path,
) -> Result<Vec<OsString>> {
    let local = transfer.url.starts_with("unix:");
    if local && transfer.connections > 1 {
        return Err(HostError::failed(
            "more than one connection is unsupported over a unix socket",
        ));
    }
    let memory_mode = match transfer.mode {
        MigrationMode::Live => "precopy",
        MigrationMode::PostCopy => "postcopy",
        MigrationMode::Reboot => {
            return Err(HostError::failed(
                "a reboot migration is not a transfer; it is a stop here and a start there",
            ));
        }
    };
    let mut args: Vec<OsString> = vec![
        "--api-socket".into(),
        socket.into(),
        "send-migration".into(),
        format!("destination_url={}", transfer.url).into(),
        format!("downtime_ms={}", transfer.downtime_ms).into(),
        format!("timeout_s={}", transfer.timeout_s).into(),
        // Give the guest back rather than leaving it paused with half its
        // memory somewhere else. `ignore` is the setting that loses guests.
        "timeout_strategy=cancel".into(),
        format!("connections={}", transfer.connections).into(),
        format!("memory_mode={memory_mode}").into(),
    ];
    // TLS is only available over TCP. Over a unix socket it is not a downgrade
    // worth being quiet about — there is no network between the two ends.
    if let Some(dir) = &layout.migration_tls_dir {
        if !local {
            args.push(format!("tls_dir={}", dir.display()).into());
        }
    }
    Ok(args)
}

fn state_of(vm_info: &str) -> InstanceState {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(vm_info) else {
        return InstanceState::Failed;
    };
    match value.get("state").and_then(|s| s.as_str()) {
        Some("Running") => InstanceState::Running,
        // `Created` is a VMM holding a machine that is not executing, and
        // `Paused` is one that stopped executing. Both are "not running", which
        // is what a status is for; neither is a transition.
        Some("Created") | Some("Shutdown") | Some("Paused") => InstanceState::Stopped,
        _ => InstanceState::Failed,
    }
}

fn request(method: &str, path: &str, body: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Split an HTTP/1.1 response into its status and body.
///
/// Written by hand rather than pulled in as a dependency because the whole
/// conversation is one request to a socket owned by this node, and a client
/// library would bring a connection pool, TLS and a retry policy for a Unix
/// socket that is either there or not.
fn parse_response(raw: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| HostError::failed("the VMM sent an answer with no end of headers"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HostError::failed("the VMM sent an answer with no status line"))?;
    if !(200..300).contains(&status) {
        return Err(HostError::failed(format!(
            "the VMM answered {status}: {}",
            body.trim()
        )));
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use sha2::Digest;

    use super::*;

    /// A directory of our own, cleaned up by the test that made it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(what: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "velstra-nodeagent-{}-{what}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn vmm(scratch: &Scratch) -> CloudHypervisorVmm {
        CloudHypervisorVmm::new(Layout {
            run_dir: scratch.0.join("instances"),
            image_dir: scratch.0.join("images"),
            incoming_dir: scratch.0.join("images/incoming"),
            disk_gib: 100,
            ..Default::default()
        })
    }

    fn transfer(url: &str) -> Transfer {
        Transfer {
            instance: "projects/p1/instances/i1".into(),
            url: url.into(),
            mode: MigrationMode::Live,
            downtime_ms: 300,
            timeout_s: 3600,
            connections: 1,
        }
    }

    fn words(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_disk_is_the_size_that_was_asked_for_and_making_it_twice_is_fine() {
        let scratch = Scratch::new("disk");
        let vmm = vmm(&scratch);
        vmm.create_disk("projects/p1/instances/i1", 2)
            .await
            .unwrap();
        let path = vmm.disk("projects/p1/instances/i1");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            2 * 1024 * 1024 * 1024
        );
        // Idempotence, on a method that would otherwise truncate a guest's
        // root disk on the second pass over the same instance.
        vmm.create_disk("projects/p1/instances/i1", 2)
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_disk_that_exists_is_what_observe_reports() {
        let scratch = Scratch::new("observe");
        let vmm = vmm(&scratch);
        vmm.create_disk("projects/p1/instances/i1", 1)
            .await
            .unwrap();
        let host = vmm.observe().await.unwrap();
        assert!(host.disks.contains("projects/p1/instances/i1"));
        // No socket, so nothing is running — and that is a fact read off the
        // machine, not a memory of never having started it.
        assert!(host.vms.is_empty());
        assert!(host.receivers.is_empty(), "a receiver was invented");
        assert!(host.sending.is_empty());
    }

    #[tokio::test]
    async fn an_image_is_only_cached_once_its_bytes_hash_to_its_name() {
        let scratch = Scratch::new("image");
        let vmm = vmm(&scratch);
        std::fs::create_dir_all(&vmm.layout.incoming_dir).unwrap();

        let bytes = b"a small disk image";
        let digest: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let image = format!("projects/p1/images/sha256-{digest}");
        std::fs::write(vmm.layout.incoming_dir.join(slug(&image)), bytes).unwrap();

        vmm.pull_image(&image).await.unwrap();
        assert!(vmm.observe().await.unwrap().images.contains(&image));
    }

    #[tokio::test]
    async fn bytes_that_are_not_what_they_claim_are_refused_and_kept() {
        let scratch = Scratch::new("badimage");
        let vmm = vmm(&scratch);
        std::fs::create_dir_all(&vmm.layout.incoming_dir).unwrap();

        let digest: String = sha2::Sha256::digest(b"the real thing")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let image = format!("projects/p1/images/sha256-{digest}");
        let arrived = vmm.layout.incoming_dir.join(slug(&image));
        std::fs::write(&arrived, b"something else entirely").unwrap();

        let err = vmm.pull_image(&image).await.unwrap_err();
        assert!(err.to_string().contains("hashed to"), "{err}");
        assert!(
            vmm.observe().await.unwrap().images.is_empty(),
            "a node cached bytes it had not verified"
        );
        assert!(arrived.exists(), "the evidence was deleted");
    }

    #[tokio::test]
    async fn an_image_without_a_digest_in_its_name_is_refused() {
        // Nothing can be verified about it, and a node that boots unverified
        // bytes is a node whose tenant isolation rests on the network alone.
        let scratch = Scratch::new("nodigest");
        let err = vmm(&scratch)
            .pull_image("projects/p1/images/ubuntu-latest")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verify"), "{err}");
    }

    #[tokio::test]
    async fn a_receiver_that_has_no_disk_to_resume_into_is_refused_before_it_listens() {
        // Cheap to check here and expensive to find out later: the guest's
        // configuration names a disk path, and the destination resolves it
        // against its own filesystem after the memory has been copied.
        let scratch = Scratch::new("nodisk");
        let vmm = vmm(&scratch);
        let request = VmRequest {
            instance: "projects/p1/instances/i1".into(),
            vcpus: 2,
            memory_mib: 2048,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 20,
            taps: vec![],
        };
        let err = vmm
            .prepare_receiver(&request, MigrationMode::Live)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("root disk"), "{err}");
    }

    #[test]
    fn a_transfer_says_exactly_what_the_documentation_says_it_should() {
        let layout = Layout::default();
        let args = send_args(
            &layout,
            &transfer("tcp:10.0.0.2:4901"),
            std::path::Path::new("/s"),
        )
        .unwrap();
        let args = words(&args);
        assert!(args.contains(&"send-migration".to_string()));
        assert!(args.contains(&"destination_url=tcp:10.0.0.2:4901".to_string()));
        assert!(args.contains(&"downtime_ms=300".to_string()));
        assert!(args.contains(&"timeout_s=3600".to_string()));
        assert!(args.contains(&"memory_mode=precopy".to_string()));
        // Giving the guest back is the only safe answer to a transfer that will
        // not converge.
        assert!(
            args.contains(&"timeout_strategy=cancel".to_string()),
            "{args:?}"
        );

        let receiving = words(&receive_args(
            std::path::Path::new("/s"),
            "tcp:10.0.0.2:4901",
        ));
        assert!(receiving.contains(&"receive-migration".to_string()));
        assert!(receiving.contains(&"receiver_url=tcp:10.0.0.2:4901".to_string()));
        // And what a receiver was started with is what a later pass reads back
        // to answer "where is it listening".
        assert_eq!(
            hostfs::url_in(&receiving.join(" ")).as_deref(),
            Some("tcp:10.0.0.2:4901")
        );
    }

    #[test]
    fn the_combination_the_documentation_calls_unsupported_is_refused_here() {
        // More than one connection over a unix socket. Passing it through would
        // fail at the far end, after the receiver was up and the operator had
        // been told the migration had started.
        let layout = Layout::default();
        let mut local = transfer("unix:/var/lib/velstra/instances/p~i1/migrate.sock");
        local.connections = 4;
        let err = send_args(&layout, &local, std::path::Path::new("/s")).unwrap_err();
        assert!(err.to_string().contains("unix socket"), "{err}");

        local.connections = 1;
        assert!(send_args(&layout, &local, std::path::Path::new("/s")).is_ok());
    }

    #[test]
    fn tls_is_only_ever_asked_for_over_tcp() {
        // Both ends refuse it over a unix socket, and asking anyway would turn
        // a local VMM upgrade into a failed migration.
        let layout = Layout {
            migration_tls_dir: Some(PathBuf::from("/etc/velstra/migration")),
            ..Default::default()
        };
        let remote = words(
            &send_args(
                &layout,
                &transfer("tcp:10.0.0.2:4901"),
                std::path::Path::new("/s"),
            )
            .unwrap(),
        );
        assert!(
            remote.contains(&"tls_dir=/etc/velstra/migration".to_string()),
            "{remote:?}"
        );

        let local = words(
            &send_args(
                &layout,
                &transfer("unix:/tmp/s"),
                std::path::Path::new("/s"),
            )
            .unwrap(),
        );
        assert!(
            !local.iter().any(|a| a.starts_with("tls_dir=")),
            "{local:?}"
        );
    }

    #[test]
    fn a_reboot_migration_is_not_a_transfer_and_says_so() {
        let mut reboot = transfer("tcp:10.0.0.2:4901");
        reboot.mode = MigrationMode::Reboot;
        let err = send_args(&Layout::default(), &reboot, std::path::Path::new("/s")).unwrap_err();
        assert!(
            err.to_string().contains("stop here and a start there"),
            "{err}"
        );
    }

    #[test]
    fn a_guest_is_started_with_the_taps_in_the_order_its_ports_are_declared() {
        // The guest's NIC order is the order of this list, and a guest that
        // finds its addresses on the wrong NIC after a move is an outage with
        // no error message.
        let request = VmRequest {
            instance: "projects/p1/instances/i1".into(),
            vcpus: 4,
            memory_mib: 8192,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 20,
            taps: vec!["vt-a".into(), "vt-b".into()],
        };
        let args = words(&vmm_args(
            &Layout::default(),
            &request,
            std::path::Path::new("/s"),
        ));
        let nets: Vec<&String> = args.iter().filter(|a| a.starts_with("tap=")).collect();
        assert_eq!(nets, vec!["tap=vt-a", "tap=vt-b"]);
        assert!(args.contains(&"boot=4".to_string()));
        assert!(args.contains(&"size=8192M".to_string()));
    }

    #[test]
    fn an_answer_from_the_vmm_is_read_as_a_status_and_a_body() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"state\":\"Running\"}";
        assert_eq!(
            state_of(&parse_response(ok).unwrap()),
            InstanceState::Running
        );

        let refused = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        assert!(parse_response(refused).is_err());

        let truncated = b"HTTP/1.1 200 OK\r\nContent-Len";
        assert!(parse_response(truncated).is_err());
    }

    #[test]
    fn a_vm_the_hypervisor_cannot_describe_is_failed_not_stopped() {
        // The two want different things done about them: one is restarted, the
        // other is left alone.
        assert_eq!(state_of("{\"state\":\"Paused\"}"), InstanceState::Stopped);
        assert_eq!(state_of("not json at all"), InstanceState::Failed);
    }

    #[test]
    fn a_request_says_how_long_its_body_is() {
        let r = request("PUT", "/api/v1/vm.shutdown", "{}");
        assert!(r.contains("Content-Length: 2"));
        assert!(r.ends_with("\r\n\r\n{}"));
    }
}
