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

use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use velstra_cloud_model::{
    migration::MigrationMode,
    resources::{Capacity, InstanceState},
};

pub use crate::hostfs::Layout;
use crate::{
    host::{HostError, HostState, Receiver, Result, Transfer, VmObservation, VmRequest, Vmm},
    hostfs::{self, Boot, slug, unslug},
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

    /// The guest's VMM, as started here.
    fn unit(&self, instance: &str) -> String {
        format!("velstra-vm-{}", hostfs::unit_slug(instance))
    }

    /// The VMM an **arriving** guest resumes into, and the socket it answers on.
    ///
    /// Distinct from [`Self::unit`], and it has to be: an in-place VMM upgrade
    /// migrates a guest to the node it is already on, so the outgoing VMM still
    /// holds that unit name and that socket path while the incoming one is being
    /// started. `systemd-run` refuses the name outright — "already loaded or has
    /// a fragment file" — which is how this was found, by running the whole
    /// chain rather than a fake.
    ///
    /// A guest that arrived this way keeps the incoming pair for the rest of its
    /// life on this node. A transient unit cannot be renamed, and pretending
    /// otherwise would mean asking a socket nobody is holding.
    fn incoming_unit(&self, instance: &str) -> String {
        format!("velstra-in-{}", hostfs::unit_slug(instance))
    }

    fn incoming_socket(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("incoming.sock")
    }

    /// Where this node remembers the URL a receiver is listening on.
    fn receiver_url_path(&self, instance: &str) -> PathBuf {
        self.dir(instance).join("receiver.url")
    }

    /// Whether the incoming VMM is holding the guest **yet**.
    ///
    /// A receiver is an *empty* VMM with a `receive-migration` pointed at it, and
    /// an empty VMM answers `vm.info` like a guest that has failed. Counting its
    /// socket as the guest therefore reads as "the guest here is broken" — and a
    /// node that believes that restarts a guest which is still running on the
    /// source. That is the two-copies-one-disk failure reached from the other
    /// direction, and it is what this predicate exists to stop.
    ///
    /// The signal is the VMM's own state, and it has to be. `ch-remote
    /// receive-migration` **returns as soon as the VMM is listening** — exit 0,
    /// no output — so its liveness says nothing about whether anything arrived;
    /// reading it that way meant "arrived" the instant the receiver was set up.
    /// A VMM that is receiving answers `vm.info` in a state that is not
    /// `Running`; Cloud Hypervisor resumes the guest when the transfer lands,
    /// and `Running` is that moment.
    async fn incoming_holds_guest(&self, instance: &str) -> bool {
        if !self.incoming_socket(instance).exists() {
            return false;
        }
        matches!(
            self.api_at(&self.incoming_socket(instance), "GET", "/api/v1/vm.info", "")
                .await,
            Ok(body) if state_of(&body) == InstanceState::Running
        )
    }

    /// Which of the two VMMs is this guest's, right now.
    ///
    /// The ordinary pair wins when both are present, which is the state during a
    /// transfer: the guest is still the outgoing one until it is not.
    async fn live(&self, instance: &str) -> (String, PathBuf) {
        if !self.socket(instance).exists() && self.incoming_holds_guest(instance).await {
            (self.incoming_unit(instance), self.incoming_socket(instance))
        } else {
            (self.unit(instance), self.socket(instance))
        }
    }

    /// The `ch-remote receive-migration` that waits for a guest. Separate,
    /// because it blocks until the transfer completes and its liveness is the
    /// answer to "is a receiver listening".
    fn receive_unit(&self, instance: &str) -> String {
        format!("velstra-recv-{}", hostfs::unit_slug(instance))
    }

    /// The `ch-remote send-migration` that copies a guest away. Also separate,
    /// and also for its liveness: a transfer under way must not be started a
    /// second time by the next pass.
    fn send_unit(&self, instance: &str) -> String {
        format!("velstra-send-{}", hostfs::unit_slug(instance))
    }

    /// One request to a guest's API socket.
    ///
    /// **Untested:** needs a live Cloud Hypervisor. The framing it builds and
    /// the response parsing it uses are tested separately against bytes.
    async fn api(&self, instance: &str, method: &str, path: &str, body: &str) -> Result<String> {
        let (_, socket) = self.live(instance).await;
        self.api_at(&socket, method, path, body).await
    }

    /// The same, against a socket the caller names.
    ///
    /// Needed because one caller must not go through [`Self::live`]: tearing down
    /// a receiver asks the *incoming* VMM whether it is holding a guest, and
    /// `live` deliberately answers "the ordinary one" while a transfer is still
    /// arriving. Routing that question through it made the teardown decide the
    /// receiver held nothing and kill it a moment after it was started — which
    /// looked exactly like a receiver that never came up.
    async fn api_at(&self, socket: &Path, method: &str, path: &str, body: &str) -> Result<String> {
        let mut stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|e| {
                HostError::failed(format!("{} is not answering: {e}", socket.display()))
            })?;
        stream
            .write_all(request(method, path, body).as_bytes())
            .await?;
        stream.flush().await?;
        // Read exactly one response and stop — never to end-of-file.
        //
        // Cloud Hypervisor sends a complete answer and then **keeps the
        // connection open**, `Connection: close` in the request
        // notwithstanding. A `read_to_end` therefore never returns, and because
        // this is called from `observe`, one running guest was enough to wedge
        // the agent's entire pass: the node stops reporting, stops reconciling,
        // stops everything, and says nothing about why. Measured, not guessed:
        // 1272 bytes arrive in milliseconds and the socket then sits there.
        //
        // The timeout is the second line of defence, for a VMM that stops
        // mid-header rather than after it. A local unix socket that has not
        // answered in this long is not going to.
        let raw = tokio::time::timeout(API_TIMEOUT, read_one_response(&mut stream))
            .await
            .map_err(|_| {
                HostError::failed(format!(
                    "{} accepted the connection and did not answer within {:?}",
                    socket.display(),
                    API_TIMEOUT
                ))
            })??;
        parse_response(&raw)
    }

    /// What is listening here for that guest, if anything.
    ///
    /// **Untested:** needs systemd. The unit is asked whether it is active and
    /// then asked what it was started with — so a receiver that died stops
    /// being ready on the next pass, and one started by a previous agent is
    /// found by this one with its URL intact.
    async fn observe_receiver(&self, instance: &str) -> Option<Receiver> {
        // Read from a file this node wrote, not from the unit that set the
        // receiver up: `ch-remote receive-migration` exits immediately, so by the
        // next pass there is no unit left to ask and its command line — where the
        // URL used to be read from — is gone with it. A receiver's URL is state on
        // this machine for as long as the VMM holding it is up, so that is where
        // it lives.
        if !self.incoming_socket(instance).exists() {
            return None;
        }
        // Already holding a guest: that is an arrival, not a receiver waiting.
        if self.incoming_holds_guest(instance).await {
            return None;
        }
        let url = std::fs::read_to_string(self.receiver_url_path(instance))
            .ok()?
            .trim()
            .to_string();
        if url.is_empty() {
            return None;
        }
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

    /// Wait for a VMM to bind the API socket it was told to.
    ///
    /// Takes the path rather than the instance, because an arriving guest's VMM
    /// binds a different one — waiting for the wrong socket here would mean
    /// handing `ch-remote` a receiver that is not listening yet.
    ///
    /// **Untested:** needs a live Cloud Hypervisor.
    async fn await_socket(&self, socket: &Path) -> Result<()> {
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
    /// tests below, and `tests/cloud_hypervisor_boots_a_guest.rs` reads a running guest's state back
    /// through it; asking systemd about receivers and transfers is not.
    fn vmm_name(&self) -> &'static str {
        "cloud-hypervisor"
    }

    async fn observe(&self) -> Result<HostState> {
        // `can_mask: false`, on every architecture, and not a stopgap.
        //
        // Cloud Hypervisor derives the guest CPUID from the host's
        // `KVM_GET_SUPPORTED_CPUID` and has no CPU model to name — `--cpus`
        // takes boot/max/topology/kvm_hyperv/max_phys_bits and a small feature
        // allowlist, none of which can present a different processor. So two
        // Cloud Hypervisor nodes exchange guests only if they are the same
        // machine, and the platform reports that rather than implying a
        // baseline could fix it. If upstream gains CPU models, this flag is
        // the only line that changes.
        let mut host = HostState {
            cpu: Some(crate::hostcpu::observe(false, None)),
            ..HostState::default()
        };

        // The machine's hardware as sysfs has it. Which guest holds which
        // device is *not* known here and is deliberately left blank: a device
        // passed to a guest is bound to `vfio-pci` exactly like a free one, so
        // sysfs cannot tell them apart. The agent overlays that from the
        // instances it holds — see `Agent::mark_held_devices`.
        host.pci_devices = crate::pcidev::observe(&Default::default());

        for digest in hostfs::read_dir_names(&self.layout.image_dir)? {
            // A file is only ever moved in here after its bytes hashed to this
            // name, so presence is verification. There is no marker file to
            // trust, and none to go stale.
            host.images.insert(unslug(&digest));
        }

        // What is on its way in. The incoming directory holds a copy that has
        // not been verified and moved across yet — which is what a fetch in
        // progress *is*, so there is nothing to record and nothing to go stale.
        for name in hostfs::read_dir_names(&self.layout.incoming_dir).unwrap_or_default() {
            let name = name.strip_suffix(".partial").unwrap_or(&name).to_string();
            if !host.images.contains(&name) {
                host.fetching.insert(name);
            }
        }

        for entry in hostfs::read_dir_names(&self.layout.run_dir)? {
            let instance = unslug(&entry);
            let dir = self.layout.run_dir.join(&entry);
            if dir.join("root.raw").exists() {
                host.disks.insert(instance.clone());
            }
            if hostfs::unit_is_active(self.layout.scope, &self.send_unit(&instance)).await {
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
            if !self.socket(&instance).exists() && !self.incoming_holds_guest(&instance).await {
                // No socket: nothing of this guest is running. The disk stays,
                // which is why a stopped instance keeps its data.
                continue;
            }
            let (unit, _) = self.live(&instance).await;
            // Cloud Hypervisor writes its serial output to the same file QEMU
            // does, so the capture and everything above it is one feature for
            // both backends rather than two.
            let (console_tail, console_bytes) = hostfs::console_tail(
                &self.layout.console(&instance),
                velstra_cloud_model::resources::CONSOLE_TAIL_BYTES,
            );
            let observation = match self.api(&instance, "GET", "/api/v1/vm.info", "").await {
                Ok(body) => VmObservation {
                    // Out of the VMM's own view of itself, which `vm.info`
                    // carries beside the state — so this costs no extra call
                    // and needs no memory of what was asked for.
                    size: size_of(&body, hostfs::disk_gib(&self.layout.disk(&instance))),
                    console_tail: console_tail.clone(),
                    console_bytes,
                    // Cloud Hypervisor passthrough is not in this phase: the
                    // backend never passes a device, so a guest here holds none.
                    devices: Vec::new(),
                    state: state_of(&body),
                    pid: hostfs::main_pid(self.layout.scope, &unit).await,
                    started_at: hostfs::started_at(&dir),
                },
                // Nobody answered *and* no VMM is running: the process is gone
                // and only its socket file is left. That is **absent**, not
                // failed, and the difference decides whether a migration can
                // finish: Cloud Hypervisor's source VMM exits when a transfer
                // completes, leaving exactly this, and reported as a failure it
                // kept the source from ever saying it had let go.
                //
                // The stale file goes with it, so the next pass does not have to
                // work this out again.
                Err(_) if !hostfs::unit_is_active(self.layout.scope, &unit).await => {
                    let _ = std::fs::remove_file(self.socket(&instance));
                    let _ = std::fs::remove_file(self.incoming_socket(&instance));
                    continue;
                }
                // A VMM that is running and not answering. That is a failure, and
                // it is reported as one rather than as "stopped", because the two
                // want different things done about them.
                Err(_) => VmObservation {
                    // A VMM that will not answer cannot say what it is running,
                    // and guessing would report a size that may not be true.
                    // Unreported reads as "nothing pending", which is the safe
                    // direction for a machine that is already failing.
                    size: None,
                    console_tail: console_tail.clone(),
                    console_bytes,
                    // Cloud Hypervisor passthrough is not in this phase: the
                    // backend never passes a device, so a guest here holds none.
                    devices: Vec::new(),
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
    async fn pull_image(&self, image: &str, digest: &str, source: &str) -> Result<()> {
        hostfs::fetch_image(&self.layout, image, digest, source).await
    }

    /// A sparse file of the asked-for size. Real, and covered by a test.
    async fn create_disk(
        &self,
        instance: &str,
        gib: u64,
        image: &str,
        format: velstra_cloud_model::resources::ImageFormat,
    ) -> Result<()> {
        let source = hostfs::image_path(&self.layout, image);
        hostfs::create_disk(&self.layout, instance, gib, source.as_deref(), format).await
    }

    /// Covered by `tests/cloud_hypervisor_boots_a_guest.rs`, which starts a real guest and
    /// reads its console: "running" is what a VMM reports for a machine that
    /// loaded nothing, so the console is the only proof that it booted.
    fn disk_path(&self, instance: &str) -> Option<std::path::PathBuf> {
        let path = self.layout.disk(instance);
        path.exists().then_some(path)
    }

    async fn start(&self, request: &VmRequest) -> Result<()> {
        let dir = self.dir(&request.instance);
        std::fs::create_dir_all(&dir)?;
        let socket = self.socket(&request.instance);
        // A socket left by a dead VMM would make the new one fail to bind.
        let _ = std::fs::remove_file(&socket);

        hostfs::systemd_run(
            self.layout.scope,
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

    /// The teardown half of `tests/cloud_hypervisor_boots_a_guest.rs`: the guest is deleted and
    /// the machine is asked again, so "gone" is observed rather than assumed.
    async fn kill(&self, instance: &str) -> Result<()> {
        // `quit` ends the VMM process where `system_powerdown` only asks the
        // guest to. The unit goes with it; the disk and the directory stay,
        // because this is a stop and not a delete.
        // `vm.shutdown` here is the hard one: Cloud Hypervisor's `vm.power-button`
        // is the ACPI press that `stop` already made and this guest ignored.
        let _ = self.api(instance, "PUT", "/api/v1/vm.shutdown", "").await;
        hostfs::stop_unit(self.layout.scope, &self.unit(instance)).await;
        Ok(())
    }

    async fn delete(&self, instance: &str) -> Result<()> {
        // Either socket: a guest that arrived by migration answers on the
        // incoming one, and asking the absent one would skip the shutdown.
        if self.socket(instance).exists() || self.incoming_socket(instance).exists() {
            let _ = self.api(instance, "PUT", "/api/v1/vm.shutdown", "").await;
        }
        // Anything this instance had in flight goes with it. A transfer of a
        // guest that is being deleted has nowhere to land.
        hostfs::stop_unit(self.layout.scope, &self.send_unit(instance)).await;
        hostfs::stop_unit(self.layout.scope, &self.receive_unit(instance)).await;
        let _ = std::fs::remove_file(self.incoming_socket(instance));
        // Both, because a guest that arrived by migration runs under the
        // incoming unit and stopping only the ordinary one would leave it up.
        hostfs::stop_unit(self.layout.scope, &self.unit(instance)).await;
        hostfs::stop_unit(self.layout.scope, &self.incoming_unit(instance)).await;
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

        // The VMM first, empty, under its own name and socket — see
        // `incoming_unit` for why it cannot be the guest's.
        let socket = self.incoming_socket(&request.instance);
        let _ = std::fs::remove_file(&socket);
        hostfs::systemd_run(
            self.layout.scope,
            &self.incoming_unit(&request.instance),
            &self.layout.slice,
            &dir,
            &self.layout.binary,
            &[OsString::from("--api-socket"), socket.clone().into()],
        )
        .await?;
        self.await_socket(&socket).await?;

        // Then the receiver, which listens until the guest arrives.
        hostfs::systemd_run(
            self.layout.scope,
            &self.receive_unit(&request.instance),
            &self.layout.slice,
            &dir,
            "ch-remote",
            &receive_args(&socket, &url),
        )
        .await?;
        // Written after the VMM is listening, so a file that exists means a
        // receiver that exists — and never the other way round.
        std::fs::write(self.receiver_url_path(&request.instance), &url)?;
        Ok(url)
    }

    /// **Untested:** needs systemd.
    async fn tear_down_receiver(&self, instance: &str) -> Result<()> {
        hostfs::stop_unit(self.layout.scope, &self.receive_unit(instance)).await;
        // The order of these two checks is the whole method. Once a transfer
        // has landed, the receiver *is* the guest's VMM: stopping that unit
        // would kill the guest this migration just moved here. So the VMM is
        // only stopped when the machine says there is no guest behind it.
        // The receiver's VMM is the incoming one, so that is the socket to ask —
        // by name, not through `live`.
        let holds_a_guest = self.incoming_socket(instance).exists()
            && matches!(
                self.api_at(&self.incoming_socket(instance), "GET", "/api/v1/vm.info", "")
                    .await,
                Ok(body) if state_of(&body) != InstanceState::Failed
            );
        if !holds_a_guest {
            hostfs::stop_unit(self.layout.scope, &self.incoming_unit(instance)).await;
            let _ = std::fs::remove_file(self.incoming_socket(instance));
            let _ = std::fs::remove_file(self.receiver_url_path(instance));
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
            self.layout.scope,
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
        hostfs::stop_unit(self.layout.scope, &self.send_unit(instance)).await;
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
        "--disk".into(),
        format!("path={}", layout.disk(&request.instance).display()).into(),
        // To a file, always: a guest that will not boot is the one with the most
        // to say and the least chance of being heard.
        "--serial".into(),
        format!("file={}", layout.console(&request.instance).display()).into(),
    ];
    match &layout.boot {
        // Cloud Hypervisor takes either a Linux kernel or a firmware blob here,
        // which is why one field served both backends for so long — and why the
        // same field quietly meant something else to QEMU.
        Boot::Firmware(Some(path)) => {
            args.push("--kernel".into());
            args.push(path.clone().into());
        }
        // Unlike QEMU, Cloud Hypervisor has no firmware of its own to fall back
        // on. Starting it with neither a kernel nor a firmware fails at the VMM
        // rather than here, and its message is better than any this could
        // invent.
        Boot::Firmware(None) => {}
        Boot::Kernel {
            kernel,
            cmdline,
            initrd,
        } => {
            args.push("--kernel".into());
            args.push(kernel.clone().into());
            if let Some(initrd) = initrd {
                args.push("--initramfs".into());
                args.push(initrd.clone().into());
            }
            if !cmdline.is_empty() {
                args.push("--cmdline".into());
                args.push(cmdline.clone().into());
            }
        }
    }
    for nic in &request.nics {
        args.push("--net".into());
        // The MAC is stated so that the guest's NIC is the one the platform
        // recorded: DHCP, the metadata service and the fabric all key off it,
        // and a VMM-invented address is an identity nothing else has heard of.
        let mut net = format!("tap={}", nic.tap);
        if let Some(mac) = &nic.mac {
            net.push_str(&format!(",mac={mac}"));
        }
        args.push(net.into());
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
    // **One** comma-joined argument, not one per setting. `ch-remote
    // send-migration` takes a single positional value —
    // `destination_url=…[,downtime_ms=…,timeout_s=…,…]` — and refuses anything
    // after it with "unexpected argument", which is how the transfer failed
    // silently as far as the platform could see: the source reported its action
    // done, the destination sat listening, and the migration stayed at 0 MiB.
    let mut settings = vec![
        format!("destination_url={}", transfer.url),
        format!("downtime_ms={}", transfer.downtime_ms),
        format!("timeout_s={}", transfer.timeout_s),
        // Give the guest back rather than leaving it paused with half its
        // memory somewhere else. `ignore` is the setting that loses guests.
        "timeout_strategy=cancel".to_string(),
        format!("connections={}", transfer.connections),
        format!("memory_mode={memory_mode}"),
    ];
    if local {
        // The flag the tool wants for a unix-socket transfer, and it has to be
        // said: without it the URL is taken as a network destination.
        settings.push("local=on".to_string());
    }
    // TLS is only available over TCP. Over a unix socket it is not a downgrade
    // worth being quiet about — there is no network between the two ends.
    if let Some(dir) = &layout.migration_tls_dir {
        if !local {
            settings.push(format!("tls_dir={}", dir.display()));
        }
    }
    Ok(vec![
        "--api-socket".into(),
        socket.into(),
        "send-migration".into(),
        settings.join(",").into(),
    ])
}

/// The size a Cloud Hypervisor guest is actually running with.
///
/// Read out of `vm.info`, which carries the whole configuration beside the
/// state — so this needs no extra call and no memory of what was asked for.
///
/// `None` when the shape is not the one this build knows. Deliberately: a
/// wrong number here reads as a pending change nobody asked for, and an
/// operator chasing a resize that never happened is worse off than one who
/// sees nothing.
///
/// The disk is not in `vm.info` as a size — it is a file — so it comes from
/// the caller, exactly as it does for QEMU.
fn size_of(vm_info: &str, disk_gib: u64) -> Option<velstra_cloud_model::resources::RunningSize> {
    let value: serde_json::Value = serde_json::from_str(vm_info).ok()?;
    let config = value.get("config")?;
    let vcpus = config.get("cpus")?.get("boot_vcpus")?.as_u64()?;
    // Bytes on the wire; mebibytes everywhere a person reads them.
    let bytes = config.get("memory")?.get("size")?.as_u64()?;
    Some(velstra_cloud_model::resources::RunningSize {
        vcpus: u32::try_from(vcpus).ok()?,
        memory_mib: bytes / (1024 * 1024),
        root_disk_gib: disk_gib,
    })
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
/// How long a VMM on a local socket gets to answer.
///
/// Generous for something on the same machine, and the point is not the number:
/// it is that there *is* one, so a wedged VMM costs one pass rather than every
/// pass from now on.
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Read one HTTP response: the headers, then exactly `Content-Length` bytes.
///
/// Framed by the message rather than by the socket closing, because this
/// server does not close it. Anything without a `Content-Length` is read as a
/// bodyless response — the VMM sends one for every call that has nothing to
/// say, and waiting for a body it will never send is the bug this exists to
/// avoid.
async fn read_one_response(stream: &mut tokio::net::UnixStream) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(HostError::failed(
                "the VMM closed the connection before finishing its headers",
            ));
        }
        raw.extend_from_slice(&chunk[..n]);
    };
    let length = String::from_utf8_lossy(&raw[..head_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while raw.len() < head_end + length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(HostError::failed(
                "the VMM closed the connection part way through its answer",
            ));
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    raw.truncate(head_end + length);
    Ok(raw)
}

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

    /// A response is framed by its own length, not by the socket closing.
    ///
    /// This is the shape of the worst bug found in this crate. Cloud Hypervisor
    /// answers in milliseconds and then **keeps the connection open**, whatever
    /// `Connection: close` said. The old code read to end-of-file, so it never
    /// returned — and since `observe` calls this, one running guest wedged the
    /// agent's whole pass for ever: no reports, no reconciliation, no error,
    /// nothing in a log to say why.
    ///
    /// The server here does exactly that, because a server that closes politely
    /// cannot show the difference.
    #[tokio::test]
    async fn a_server_that_never_closes_does_not_wedge_the_reader() {
        let dir = std::env::temp_dir().join(format!("vq-frame-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("a socket");

        let held = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("a connection");
            let body = r#"{"state":"Running"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("written");
            stream.flush().await.expect("flushed");
            // …and now hold it open, which is the whole point. Kept until the
            // task is dropped, so the reader has no end-of-file to wait for.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });

        let mut client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("connects");
        client
            .write_all(request("GET", "/api/v1/vm.info", "").as_bytes())
            .await
            .expect("written");
        client.flush().await.expect("flushed");

        // Two seconds is a hundred times what a local socket needs, and far
        // under the API timeout — so this fails by hanging if the framing is
        // wrong, rather than by quietly taking the timeout path.
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_one_response(&mut client),
        )
        .await
        .expect("the reader waited for an end-of-file that was never coming")
        .expect("a response");

        assert_eq!(
            parse_response(&raw).expect("parsed"),
            r#"{"state":"Running"}"#
        );
        assert_eq!(
            state_of(&parse_response(&raw).unwrap()),
            InstanceState::Running
        );

        held.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
    use sha2::Digest;

    use super::*;
    use crate::host::Nic;

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

    /// The settings out of the one positional value `ch-remote` takes.
    ///
    /// Split here rather than asserted as separate argv entries, because that is
    /// the shape the tool documents — and asserting the other shape is exactly
    /// how these tests passed while every real transfer was refused with
    /// "unexpected argument".
    fn settings(args: &[OsString]) -> Vec<String> {
        words(args)
            .last()
            .map(|value| value.split(',').map(str::to_string).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn a_disk_is_the_size_that_was_asked_for_and_making_it_twice_is_fine() {
        let scratch = Scratch::new("disk");
        let vmm = vmm(&scratch);
        vmm.create_disk(
            "projects/p1/instances/i1",
            2,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        let path = vmm.disk("projects/p1/instances/i1");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            2 * 1024 * 1024 * 1024
        );
        // Idempotence, on a method that would otherwise truncate a guest's
        // root disk on the second pass over the same instance.
        vmm.create_disk(
            "projects/p1/instances/i1",
            2,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_disk_that_exists_is_what_observe_reports() {
        let scratch = Scratch::new("observe");
        let vmm = vmm(&scratch);
        vmm.create_disk(
            "projects/p1/instances/i1",
            1,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
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
        // The object may be called anything; the bytes are filed under their
        // digest, which is what makes one node's copy serve every name.
        let image = "projects/p1/images/debian-13";
        let value = format!("sha256:{digest}");
        let stored = format!("sha256-{digest}");
        std::fs::write(vmm.layout.incoming_dir.join(&stored), bytes).unwrap();

        // The source names a file that does not exist: the copy already in
        // `incoming` must be what is verified and published, and if the fetch
        // were attempted anyway this would fail rather than pass quietly.
        vmm.pull_image(image, &value, "file:///nonexistent")
            .await
            .unwrap();
        assert!(vmm.observe().await.unwrap().images.contains(&stored));
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
        let image = "projects/p1/images/debian-13";
        let value = format!("sha256:{digest}");
        let arrived = vmm.layout.incoming_dir.join(format!("sha256-{digest}"));
        std::fs::write(&arrived, b"something else entirely").unwrap();

        let err = vmm
            .pull_image(image, &value, "file:///nonexistent")
            .await
            .unwrap_err();
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
            .pull_image(
                "projects/p1/images/ubuntu-latest",
                // No digest at all: a name is not one, and this is the case the
                // refusal is for — a node that downloads bytes it cannot check
                // is a node whose tenant isolation rests on the network alone.
                "",
                "http://images.invalid/x",
            )
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
            devices: Vec::new(),
            instance: "projects/p1/instances/i1".into(),
            vcpus: 2,
            memory_mib: 2048,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 20,
            nics: vec![],
            cpu_baseline: None,
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
        assert!(words(&args).contains(&"send-migration".to_string()));
        let args = settings(&args);
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
        let remote = settings(
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

        let local = settings(
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
            devices: Vec::new(),
            instance: "projects/p1/instances/i1".into(),
            vcpus: 4,
            memory_mib: 8192,
            image: "projects/p1/images/sha256-abc".into(),
            root_disk_gib: 20,
            nics: vec![
                Nic {
                    tap: "vt-a".into(),
                    mac: Some("52:54:00:00:00:0a".into()),
                },
                Nic {
                    tap: "vt-b".into(),
                    mac: None,
                },
            ],
            cpu_baseline: None,
        };
        let args = words(&vmm_args(
            &Layout::default(),
            &request,
            std::path::Path::new("/s"),
        ));
        let nets: Vec<&String> = args.iter().filter(|a| a.starts_with("tap=")).collect();
        // The MAC travels with the tap: a guest whose NIC comes up with an
        // address the platform never recorded is one DHCP cannot recognise and
        // the metadata service cannot describe.
        assert_eq!(nets, vec!["tap=vt-a,mac=52:54:00:00:00:0a", "tap=vt-b"]);
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
    /// The size comes out of `vm.info`, which the VMM already answers.
    ///
    /// Without this, a resize of a Cloud Hypervisor guest showed nothing
    /// pending — the spec said one thing, the machine ran another, and the
    /// object read as settled. That is the silent divergence the pending-change
    /// work exists to remove, and it was still there on this backend.
    #[test]
    fn the_running_size_is_read_out_of_what_the_vmm_already_reports() {
        let info = r#"{
            "state": "Running",
            "config": {
                "cpus": { "boot_vcpus": 4, "max_vcpus": 8 },
                "memory": { "size": 8589934592, "shared": false }
            }
        }"#;
        let size = size_of(info, 40).expect("vm.info carries a size");
        assert_eq!(size.vcpus, 4);
        // Bytes on the wire, mebibytes where a person reads them.
        assert_eq!(size.memory_mib, 8192);
        assert_eq!(size.root_disk_gib, 40);
    }

    /// A shape this build does not know reports nothing, not a guess.
    ///
    /// A wrong number here reads as a pending change nobody asked for, and an
    /// operator chasing a resize that never happened is worse off than one who
    /// sees nothing.
    #[test]
    fn an_unfamiliar_shape_reports_no_size_rather_than_a_wrong_one() {
        assert!(size_of("{}", 40).is_none());
        assert!(size_of(r#"{"config":{"cpus":{}}}"#, 40).is_none());
        assert!(size_of(r#"{"config":{"cpus":{"boot_vcpus":4}}}"#, 40).is_none());
        assert!(size_of("not json at all", 40).is_none());
    }
}
