//! QEMU, one process per guest, under systemd, driven over QMP.
//!
//! ## What is true on this machine, and what is not
//!
//! QEMU is **not installed on the machine this was written on**. Everything
//! that needs the binary or a live guest — `start`, `stop`, `delete`, the
//! volume methods, and every migration method — is written from the documented
//! QMP protocol and command line and has **never been run**. Each such method
//! says so. What the tests at the bottom do exercise, because it needs nothing
//! but bytes and a filesystem: the QMP framing and reply parsing, the command
//! lines for a normal boot and for an incoming migration, the run-state
//! mapping, how progress is read out of `query-migrate`, and reading a
//! receiver's own URL back out of the unit that is listening.
//!
//! ## The shape of a migration here
//!
//! It is the mirror image of Cloud Hypervisor's, which is what makes one trait
//! fit both:
//!
//! 1. The destination starts the guest's QEMU with `-incoming tcp:0:PORT` (or
//!    `-incoming unix:/path` on one machine). That process *is* the guest's VMM
//!    — it sits in the `inmigrate` run state until a guest arrives, which is
//!    also how this node observes that a receiver is ready.
//! 2. The source sets `downtime-limit` and then issues `migrate` over QMP.
//!    QMP's `migrate` returns as soon as the transfer has started, which is
//!    what `-d` means on the human monitor; progress and completion come from
//!    `query-migrate`.
//! 3. `migrate_cancel` abandons it, and under pre-copy the guest is still
//!    running here when it does.
//!
//! Two things QEMU shares with the other backend and which the model refuses
//! before anything is copied: the kernel and disk must be reachable at the same
//! paths on both machines, and the two versions must be close enough to
//! deserialise each other's device state.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use velstra_cloud_model::{
    migration::MigrationMode,
    resources::{Capacity, InstanceState},
};

use crate::{
    cephadm::CephAdmin,
    host::{HostError, HostState, Receiver, Result, Transfer, VmObservation, VmRequest, Vmm},
    hostfs::{self, Boot, Layout, unslug},
};

pub struct QemuVmm {
    layout: Layout,
    /// Ceph's tools, held rather than built per observation.
    ///
    /// Held for two reasons. It keeps whatever binaries this node was
    /// configured with instead of silently reverting to the defaults on the
    /// observation path; and [`CephAdmin`] only says a standing failure once
    /// per spell, which a value rebuilt every pass would defeat by making
    /// every pass the first one.
    cephadm: CephAdmin,
}

impl QemuVmm {
    /// `layout.binary` is the QEMU to run — `qemu-system-x86_64` on a normal
    /// node — and `layout.firmware` is what it boots.
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            cephadm: CephAdmin::default(),
        }
    }

    /// Point this VMM's Ceph observation at particular binaries.
    ///
    /// The counterpart to the agent's own `with_ceph_tools`: the deployment
    /// path and the observation path are two different call sites, and a test
    /// that redirected only one of them would be testing the redirection
    /// rather than the node.
    pub fn with_ceph_tools(mut self, cephadm: CephAdmin) -> Self {
        self.cephadm = cephadm;
        self
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn monitor(&self, instance: &str) -> PathBuf {
        self.layout.dir(instance).join("qmp.sock")
    }

    fn migrate_socket(&self, instance: &str) -> PathBuf {
        self.layout.dir(instance).join("migrate.sock")
    }

    fn unit(&self, instance: &str) -> String {
        format!("velstra-vm-{}", hostfs::unit_slug(instance))
    }

    /// The VMM an **arriving** guest resumes into, and the monitor it answers on.
    ///
    /// Distinct from [`Self::unit`], and it has to be: an in-place VMM upgrade
    /// migrates a guest to the node it is already on, so the outgoing QEMU still
    /// holds that unit name and that monitor path while the incoming one is
    /// started. `systemd-run` refuses the name outright — "already loaded or has
    /// a fragment file" — which is how this was found, by running the whole chain
    /// against a real hypervisor.
    ///
    /// A guest that arrived this way keeps the incoming pair for the rest of its
    /// life on this node: a transient unit cannot be renamed.
    fn incoming_unit(&self, instance: &str) -> String {
        format!("velstra-in-{}", hostfs::unit_slug(instance))
    }

    fn incoming_monitor(&self, instance: &str) -> PathBuf {
        self.layout.dir(instance).join("incoming-qmp.sock")
    }

    /// Which of the two VMMs is this guest's, right now. The ordinary pair wins
    /// when both are there, which is the state during a transfer: the guest is
    /// still the outgoing one until it is not.
    fn live(&self, instance: &str) -> (String, PathBuf) {
        if !self.monitor(instance).exists() && self.incoming_monitor(instance).exists() {
            (
                self.incoming_unit(instance),
                self.incoming_monitor(instance),
            )
        } else {
            (self.unit(instance), self.monitor(instance))
        }
    }

    /// One QMP command.
    ///
    /// **Untested:** needs a live QEMU. The handshake it performs and the reply
    /// parsing it uses are tested separately against bytes.
    ///
    /// A fresh connection per command, deliberately: a long-lived monitor
    /// connection is a piece of state this process would have to keep correct
    /// across a VMM restart, and the whole design of this crate is that it keeps
    /// none.
    async fn qmp(&self, instance: &str, command: &str, arguments: Value) -> Result<Value> {
        let (_, socket) = self.live(instance);
        self.qmp_at(&socket, command, arguments).await
    }

    /// The same, against a monitor the caller names — needed by the receiver
    /// paths, which must ask the *incoming* VMM even while the outgoing one is
    /// still answering on its own socket.
    async fn qmp_at(&self, socket: &Path, command: &str, arguments: Value) -> Result<Value> {
        let socket = socket.to_path_buf();
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .map_err(|e| {
                HostError::failed(format!("{} is not answering: {e}", socket.display()))
            })?;
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();

        // The greeting comes first, unasked. Then capabilities negotiation,
        // which QEMU requires before it will accept anything else.
        let _greeting = lines.next_line().await?;
        ask(&mut write, &json!({ "execute": "qmp_capabilities" })).await?;
        answer(&mut lines, "qmp_capabilities").await?;
        ask(
            &mut write,
            &json!({ "execute": command, "arguments": arguments }),
        )
        .await?;
        answer(&mut lines, command).await
    }

    /// **Untested:** needs a live QEMU. Whether a transfer this node started is
    /// still running — the thing that stops a pass from issuing a second
    /// `migrate` on top of the first.
    async fn is_sending(&self, instance: &str) -> bool {
        match self.qmp(instance, "query-migrate", json!({})).await {
            Ok(answer) => answer
                .get("status")
                .and_then(|s| s.as_str())
                .map(still_going)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// **Untested:** needs a live QEMU.
    async fn run_state(&self, instance: &str) -> Option<String> {
        let (_, socket) = self.live(instance);
        self.run_state_at(&socket).await
    }

    async fn run_state_at(&self, socket: &Path) -> Option<String> {
        let answer = self.qmp_at(socket, "query-status", json!({})).await.ok()?;
        answer.get("status")?.as_str().map(str::to_string)
    }

    /// What is listening here for that guest, if anything.
    ///
    /// **Untested:** needs QEMU and systemd. Two questions, both asked of the
    /// machine: is the VMM in the `inmigrate` run state — QEMU's own word for
    /// "waiting for a guest" — and what was it started with. A receiver whose
    /// process died answers neither, and so stops being ready.
    async fn observe_receiver(&self, instance: &str) -> Option<Receiver> {
        // Asked of the incoming VMM by name: during a same-machine transfer the
        // outgoing one is still answering on its own monitor, and it is not the
        // one in `inmigrate`.
        if self
            .run_state_at(&self.incoming_monitor(instance))
            .await
            .as_deref()
            != Some("inmigrate")
        {
            return None;
        }
        let incoming = hostfs::url_in(
            &hostfs::unit_command(self.layout.scope, &self.incoming_unit(instance)).await?,
        )?;
        Some(Receiver {
            url: self.published_url(&incoming)?,
            // The destination's `query-migrate` carries the counters only once
            // pages start arriving, and often not at all; it reports what it
            // has and nothing when it has nothing.
            received_mib: self.received_mib(instance).await,
        })
    }

    /// The volumes this guest has open, as volume name to the node-name QEMU
    /// knows it by.
    ///
    /// From `query-block`, and specifically from `inserted.node-name`, which is
    /// exactly what `blockdev-add` was given. The neighbouring `qdev` is *not*
    /// it — a real answer, recorded from a live guest holding a disk:
    ///
    /// ```json
    /// { "device": "",
    ///   "qdev": "/machine/peripheral/projects_p1_volumes_data/virtio-backend",
    ///   "inserted": { "node-name": "projects_p1_volumes_data" } }
    /// ```
    ///
    /// `device` is empty for anything added this way, and `qdev` is a QOM path
    /// with the id buried in the middle of it. Reading `qdev` first dropped every
    /// entry on the floor, which left `attached` false and had the agent plug the
    /// same disk in once a pass for ever — `Duplicate nodes with node-name='…'`,
    /// while the guest ran with the disk perfectly well attached.
    ///
    /// Best-effort: a VMM that will not answer is a VMM whose guest is in
    /// trouble for larger reasons, and the pass has plenty else to report.
    async fn open_volumes(&self, instance: &str) -> Vec<(String, String)> {
        let Ok(answer) = self.qmp(instance, "query-block", json!({})).await else {
            return Vec::new();
        };
        let Some(devices) = answer.as_array() else {
            return Vec::new();
        };
        devices
            .iter()
            .filter_map(|d| {
                let id = d
                    .get("inserted")
                    .and_then(|i| i.get("node-name"))
                    .and_then(|v| v.as_str())?;
                // The root disk and anything else QEMU named for itself
                // (`#block156`) are not ours, and `from_qmp_id` says so.
                let volume = crate::hostfs::from_qmp_id(id)?;
                Some((volume, id.to_string()))
            })
            .collect()
    }

    /// **Untested:** needs a live QEMU.
    async fn received_mib(&self, instance: &str) -> u64 {
        match self.qmp(instance, "query-migrate", json!({})).await {
            Ok(answer) => transferred_mib(&answer),
            Err(_) => 0,
        }
    }

    /// What `-incoming` says, turned into what a source can send to.
    ///
    /// `-incoming tcp:0:4900` means "every address of this machine", which is
    /// not a thing another node can be told to connect to. The address this
    /// node's peers reach it at is configuration, because nothing on the machine
    /// knows which of its addresses that is.
    fn published_url(&self, incoming: &str) -> Option<String> {
        if incoming.starts_with("unix:") {
            return Some(incoming.to_string());
        }
        let port = hostfs::port_of(incoming)?;
        let address = self.layout.migration_address.as_ref()?;
        Some(format!("tcp:{address}:{port}"))
    }

    /// The migration ports this machine is already using, read off the units
    /// using them.
    async fn ports_in_use(&self) -> Result<BTreeSet<u16>> {
        let mut taken = BTreeSet::new();
        for entry in hostfs::read_dir_names(&self.layout.run_dir)? {
            let instance = unslug(&entry);
            let Some(command) =
                // The incoming unit is the one started with `-incoming <url>`, so
                // that is where a port in use is named.
                hostfs::unit_command(self.layout.scope, &self.incoming_unit(&instance)).await
            else {
                continue;
            };
            if let Some(port) = hostfs::url_in(&command)
                .as_deref()
                .and_then(hostfs::port_of)
            {
                taken.insert(port);
            }
        }
        Ok(taken)
    }
}

#[async_trait]
impl Vmm for QemuVmm {
    /// **Partly untested:** the directory and image scan need only a
    /// filesystem and are exercised below; the run state of a live guest is
    /// not.
    fn vmm_name(&self) -> &'static str {
        "qemu"
    }

    async fn observe(&self) -> Result<HostState> {
        let mut host = HostState::default();

        for digest in hostfs::read_dir_names(&self.layout.image_dir)? {
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

        // The machine's disks, and what it runs of Ceph. Best-effort on both:
        // a node without `lsblk` or without `cephadm` is an ordinary node that
        // simply cannot be a Ceph host, and failing the whole observation over
        // it would take that node's guests down for a feature it does not use.
        match crate::devices::observe_devices().await {
            Ok(devices) => host.devices = devices,
            Err(e) => tracing::debug!("could not read this machine's disks: {e}"),
        }
        // `can_mask: true` on x86: QEMU has a CPU model system, which is what
        // lets a mixed fleet be baselined into one migration domain. Not on
        // aarch64, where QEMU has no models either — the capability belongs to
        // the pair, not to the VMM's name.
        // Silicon only. What this node *presents* is the agent's to apply,
        // because it is declared on the node object and this backend cannot
        // see it — see `Agent::present_baseline`.
        // Asked of this QEMU, not inferred from the architecture. A machine
        // whose QEMU has no `x86-64-vN` models cannot be baselined, and saying
        // it can is how an operator ends up with guests that will not start.
        host.cpu = Some(crate::hostcpu::observe(
            std::env::consts::ARCH == "x86_64" && presents_levels(&self.layout.binary).await,
            None,
        ));

        // The machine's hardware as sysfs has it. Which guest holds which
        // device is *not* known here and is deliberately left blank: a device
        // passed to a guest is bound to `vfio-pci` exactly like a free one, so
        // sysfs cannot tell them apart. The agent overlays that from the
        // instances it holds — see `Agent::mark_held_devices`.
        host.pci_devices = crate::pcidev::observe(&Default::default());

        let ceph = self.cephadm.installed().await;
        if ceph.installed {
            host.ceph = Some(ceph);
        }

        for entry in hostfs::read_dir_names(&self.layout.run_dir)? {
            let instance = unslug(&entry);
            let dir = self.layout.run_dir.join(&entry);
            if dir.join("root.raw").exists() {
                host.disks.insert(instance.clone());
            }
            if !self.monitor(&instance).exists() && !self.incoming_monitor(&instance).exists() {
                // No monitor. Usually that means no VMM was ever asked for here
                // — a directory left from a guest that has gone — and skipping
                // is right.
                //
                // But it is also what a VMM that died *before* opening its
                // monitor looks like, and those two must not be confused. A
                // QEMU that exits at argument-parsing creates no socket, so the
                // guest simply vanished from this observation: the control plane
                // was told `Stopped`, the console was empty, and the reason
                // existed only in the unit's journal. Found live, from a CPU
                // baseline this platform advised.
                if let Some(why) =
                    hostfs::unit_failure(self.layout.scope, &self.unit(&instance)).await
                {
                    host.vms.insert(
                        instance,
                        VmObservation {
                            size: None,
                            // The hypervisor's own words, put where a person is
                            // already looking. It never reached the console log,
                            // because it never opened one.
                            console_tail: why,
                            console_bytes: 0,
                            state: InstanceState::Failed,
                            pid: None,
                            started_at: hostfs::started_at(&dir),
                            devices: Vec::new(),
                        },
                    );
                }
                continue;
            }
            // Read once per guest per pass, before either branch: a VMM that
            // has just died still has its log on disk, and this is the moment
            // to pick it up.
            let (console_tail, console_bytes) = hostfs::console_tail(
                &self.layout.console(&instance),
                velstra_cloud_model::resources::CONSOLE_TAIL_BYTES,
            );
            let Some(state) = self.run_state(&instance).await else {
                // The socket is there and nobody is behind it: the VMM died and
                // left it. A failure, not a stop — the two want different
                // things done about them.
                host.vms.insert(
                    instance,
                    VmObservation {
                        // A dead VMM is running nothing, so there is no size
                        // to report — and a stale one would make a stopped
                        // guest look as though it disagreed with its spec.
                        size: None,
                        // Especially here. A VMM that died and left its socket
                        // behind is the case this whole capture exists for.
                        console_tail,
                        console_bytes,
                        state: InstanceState::Failed,
                        pid: None,
                        started_at: hostfs::started_at(&dir),
                        // A dead VMM holds nothing: whatever it had is back
                        // with the host, and reporting otherwise would keep a
                        // device out of everyone's reach until somebody
                        // noticed.
                        devices: Vec::new(),
                    },
                );
                continue;
            };
            if state == "inmigrate" {
                // Waiting for a guest, not holding one. Reporting a VM here
                // would tell the control plane the instance has arrived while
                // the source still has it.
                if let Some(receiver) = self.observe_receiver(&instance).await {
                    host.receivers.insert(instance, receiver);
                }
                continue;
            }
            if self.is_sending(&instance).await {
                host.sending.insert(instance.clone());
            }
            // Which volumes this guest has open, asked of the VMM itself.
            //
            // Nothing filled this in before, on either real backend, and the
            // consequence was invisible until an attach worked: `attached` is
            // computed from this map, so it was always false, so every pass
            // asked the VMM to plug the disk in again — and QEMU answered
            // `Duplicate nodes with node-name='…'` for ever while the guest sat
            // there with the disk perfectly well attached. The fake filled it in,
            // which is why the tests were happy.
            for (volume, device) in self.open_volumes(&instance).await {
                host.volumes.insert(volume, device);
            }

            host.vms.insert(
                instance.clone(),
                VmObservation {
                    size: running_size(
                        &hostfs::unit_command(self.layout.scope, &self.unit(&instance))
                            .await
                            .unwrap_or_default(),
                        hostfs::disk_gib(&self.layout.disk(&instance)),
                    ),
                    console_tail,
                    console_bytes,
                    state: state_of(&state),
                    pid: hostfs::main_pid(self.layout.scope, &self.unit(&instance)).await,
                    started_at: hostfs::started_at(&dir),
                    // Read back off the running VMM's own command line, the
                    // same way a receiver's URL is. The agent therefore
                    // remembers nothing: a restarted agent recovers which
                    // guest holds which device from the machine, which is the
                    // rule this whole crate is built on.
                    devices: passed_devices(
                        &hostfs::unit_command(self.layout.scope, &self.unit(&instance))
                            .await
                            .unwrap_or_default(),
                    ),
                },
            );
        }

        Ok(host)
    }

    async fn pull_image(&self, image: &str, digest: &str, source: &str) -> Result<()> {
        hostfs::fetch_image(&self.layout, image, digest, source).await
    }

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

    /// Covered by `tests/qemu_boots_a_guest.rs`, which starts a real guest and
    /// reads its console: "running" is what a VMM reports for a machine that
    /// loaded nothing, so the console is the only proof that it booted.
    fn disk_path(&self, instance: &str) -> Option<std::path::PathBuf> {
        let path = self.layout().disk(instance);
        path.exists().then_some(path)
    }

    async fn start(&self, request: &VmRequest) -> Result<()> {
        let dir = self.layout.dir(&request.instance);
        std::fs::create_dir_all(&dir)?;
        let monitor = self.monitor(&request.instance);
        // A socket left by a dead VMM would stop the new one binding.
        let _ = std::fs::remove_file(&monitor);
        hostfs::systemd_run(
            self.layout.scope,
            &self.unit(&request.instance),
            &self.layout.slice,
            &dir,
            &self.layout.binary,
            &qemu_args(&self.layout, request, &monitor, None),
        )
        .await
    }

    /// **Untested:** ACPI power button, the graceful stop. A guest that ignores
    /// it stays running and the object says so, rather than this node
    /// escalating to a kill on its own.
    async fn stop(&self, instance: &str) -> Result<()> {
        self.qmp(instance, "system_powerdown", json!({}))
            .await
            .map(|_| ())
    }

    /// The teardown half of `tests/qemu_boots_a_guest.rs`: the guest is deleted and
    /// the machine is asked again, so "gone" is observed rather than assumed.
    async fn kill(&self, instance: &str) -> Result<()> {
        // `quit` ends the VMM process where `system_powerdown` only asks the
        // guest to. The unit goes with it; the disk and the directory stay,
        // because this is a stop and not a delete.
        let _ = self.qmp(instance, "quit", json!({})).await;
        hostfs::stop_unit(self.layout.scope, &self.unit(instance)).await;
        Ok(())
    }

    async fn delete(&self, instance: &str) -> Result<()> {
        if self.monitor(instance).exists() || self.incoming_monitor(instance).exists() {
            let _ = self.qmp(instance, "quit", json!({})).await;
        }
        hostfs::stop_unit(self.layout.scope, &self.unit(instance)).await;
        let dir = self.layout.dir(instance);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// **Untested:** hot-plugs a volume into a running guest.
    async fn open_volume(
        &self,
        instance: &str,
        volume: &str,
        at: &str,
        read_only: bool,
    ) -> Result<String> {
        // Not `slug`: QEMU refuses `~` in a node-name. See `hostfs::qmp_id`.
        let id = crate::hostfs::qmp_id(volume);
        // `qcow2` for a file, because that is what the directory pool writes and
        // opening a qcow2 as `raw` hands the guest the image header as its first
        // sector. Ceph names itself.
        let file = if let Some(image) = at.strip_prefix("rbd:") {
            json!({ "driver": "rbd", "image": image })
        } else {
            json!({ "driver": "file", "filename": at })
        };
        let driver = if at.ends_with(".qcow2") {
            "qcow2"
        } else {
            "raw"
        };
        self.qmp(
            instance,
            "blockdev-add",
            json!({
                "node-name": id,
                "driver": driver,
                "read-only": read_only,
                "file": file,
            }),
        )
        .await?;
        self.qmp(
            instance,
            "device_add",
            json!({ "driver": "virtio-blk-pci", "drive": id, "id": id }),
        )
        .await?;
        // The name the guest's kernel gives it depends on the guest; what this
        // node can honestly report is the device it plugged in. The same string
        // comes back from `query-block`, which is what makes the next pass see
        // the disk as attached rather than plug it in again.
        Ok(id)
    }

    /// **Untested:** unplugs the device and drops the block node behind it.
    async fn close_volume(&self, instance: &str, volume: &str) -> Result<()> {
        let id = crate::hostfs::qmp_id(volume);
        self.qmp(instance, "device_del", json!({ "id": id }))
            .await?;
        self.qmp(instance, "blockdev-del", json!({ "node-name": id }))
            .await
            .map(|_| ())
    }

    async fn capacity(&self) -> Result<Capacity> {
        Ok(hostfs::capacity(&self.layout))
    }

    /// **Untested:** requires `qemu-system-*` and `systemd-run`.
    ///
    /// One process and no second one: QEMU's receiver is the guest's own VMM,
    /// started with `-incoming` and sitting in `inmigrate` until the transfer
    /// lands.
    async fn prepare_receiver(&self, request: &VmRequest, _mode: MigrationMode) -> Result<String> {
        if let Some(receiver) = self.observe_receiver(&request.instance).await {
            return Ok(receiver.url);
        }
        let dir = self.layout.dir(&request.instance);
        std::fs::create_dir_all(&dir)?;
        if !self.layout.disk(&request.instance).exists() {
            // The guest resumes into its own root disk by the path its command
            // line names, and QEMU opens that disk at start — before anything
            // arrives. A receiver without one fails immediately.
            return Err(HostError::failed(format!(
                "{} has no root disk on this node",
                request.instance
            )));
        }

        let incoming = match &self.layout.migration_address {
            Some(_) => format!(
                "tcp:0:{}",
                hostfs::free_port(&self.layout, &self.ports_in_use().await?)?
            ),
            None => {
                let socket = self.migrate_socket(&request.instance);
                let _ = std::fs::remove_file(&socket);
                format!("unix:{}", socket.display())
            }
        };
        // Its own name and monitor — see `incoming_unit` for why it cannot be the
        // guest's.
        let monitor = self.incoming_monitor(&request.instance);
        let _ = std::fs::remove_file(&monitor);
        hostfs::systemd_run(
            self.layout.scope,
            &self.incoming_unit(&request.instance),
            &self.layout.slice,
            &dir,
            &self.layout.binary,
            &qemu_args(&self.layout, request, &monitor, Some(&incoming)),
        )
        .await?;
        self.published_url(&incoming)
            .ok_or_else(|| HostError::failed("this node has no migration address to publish"))
    }

    /// **Untested:** needs systemd.
    async fn tear_down_receiver(&self, instance: &str) -> Result<()> {
        // Only a VMM that is still waiting may be stopped. Once the guest has
        // arrived this same unit *is* the guest, and stopping it would be the
        // migration killing what it just moved.
        // Asked of the incoming VMM by name: on one machine the outgoing one is
        // still answering, and it is not the one that may be stopped.
        if self
            .run_state_at(&self.incoming_monitor(instance))
            .await
            .as_deref()
            == Some("inmigrate")
        {
            hostfs::stop_unit(self.layout.scope, &self.incoming_unit(instance)).await;
            let _ = std::fs::remove_file(self.incoming_monitor(instance));
        }
        let _ = std::fs::remove_file(self.migrate_socket(instance));
        Ok(())
    }

    /// **Untested:** needs a live QEMU.
    ///
    /// Parameters first, then the transfer. `migrate` over QMP returns as soon
    /// as the copy is under way — the same thing `-d` means on the human
    /// monitor — so this returns then too, and `observe` answers what happened.
    async fn send(&self, transfer: &Transfer) -> Result<()> {
        for (parameter, value) in migrate_parameters(transfer)? {
            self.qmp(
                &transfer.instance,
                "migrate-set-parameters",
                json!({ parameter: value }),
            )
            .await?;
        }
        if transfer.connections > 1 {
            self.qmp(
                &transfer.instance,
                "migrate-set-capabilities",
                json!({ "capabilities": [{ "capability": "multifd", "state": true }] }),
            )
            .await?;
        }
        if transfer.mode == MigrationMode::PostCopy {
            self.qmp(
                &transfer.instance,
                "migrate-set-capabilities",
                json!({ "capabilities": [{ "capability": "postcopy-ram", "state": true }] }),
            )
            .await?;
        }
        self.qmp(
            &transfer.instance,
            "migrate",
            json!({ "uri": transfer.url }),
        )
        .await?;
        if transfer.mode == MigrationMode::PostCopy {
            // Post-copy resumes the destination first and faults pages in, so
            // from here on a failure loses the guest. That is why it is never a
            // default and why the model makes it a stated mode.
            self.qmp(&transfer.instance, "migrate-start-postcopy", json!({}))
                .await?;
        }
        Ok(())
    }

    /// **Untested:** needs a live QEMU. Under pre-copy the guest never stopped
    /// running here, so cancelling costs the memory that was copied and nothing
    /// else.
    async fn cancel_send(&self, instance: &str) -> Result<()> {
        self.qmp(instance, "migrate_cancel", json!({}))
            .await
            .map(|_| ())
    }
}

/// Write one QMP command.
async fn ask(write: &mut tokio::net::unix::OwnedWriteHalf, request: &Value) -> std::io::Result<()> {
    write.write_all(request.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.flush().await
}

/// Read until the monitor says something that is an answer.
///
/// Events arrive whenever QEMU feels like it, including between a command and
/// its reply, so this reads past them rather than mistaking the first line for
/// the answer.
async fn answer(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    command: &str,
) -> Result<Value> {
    while let Some(line) = lines.next_line().await? {
        if let Some(answer) = qmp_reply(&line)? {
            return Ok(answer);
        }
    }
    Err(HostError::failed(format!(
        "the monitor closed before answering {command}"
    )))
}

/// The command line for a guest, with or without an incoming transfer.
///
/// Pure, so what a guest is started with can be argued about without QEMU. The
/// receiving command line is the same one plus `-incoming`, which is what makes
/// a migrated guest come back as the machine it was: a destination that boots a
/// different shape of machine is a guest that resumes into missing devices.
/// The value for `-cpu`.
///
/// No baseline declared: `host`, the machine the guest was actually placed on.
/// A baseline: the model, with `enforce` appended so QEMU refuses to start
/// rather than quietly presenting the guest something smaller than promised.
///
/// `enforce` is what makes a baseline a promise instead of a hope. Without it
/// a node that cannot provide the model starts the guest anyway with whatever
/// subset it has, the platform reports the baseline as met, and the first
/// symptom is a guest faulting on the destination of a migration the platform
/// had every reason to believe was safe.
/// The size a QEMU command line describes.
///
/// Parsed back out for the same reason the devices are: the agent remembers
/// nothing, so what a running guest *is* has to be recoverable from the
/// machine. `-smp 4 -m 8192`, in the form `qemu_args` writes them.
///
/// The disk is not on the command line as a size — it is a file — so it is
/// read from the file instead, by the caller.
fn running_size(
    command: &str,
    disk_gib: u64,
) -> Option<velstra_cloud_model::resources::RunningSize> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let after = |flag: &str| -> Option<&str> {
        words
            .iter()
            .position(|w| *w == flag)
            .and_then(|at| words.get(at + 1))
            .copied()
    };
    Some(velstra_cloud_model::resources::RunningSize {
        vcpus: after("-smp")?.split(',').next()?.parse().ok()?,
        memory_mib: after("-m")?.parse().ok()?,
        root_disk_gib: disk_gib,
    })
}

/// The PCI addresses on a QEMU command line.
///
/// Parsed back out rather than remembered, so a restarted agent recovers what
/// each guest holds from the machine. The form is the one `qemu_args` writes:
/// `-device vfio-pci,host=0000:41:00.0`.
fn passed_devices(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("vfio-pci,host="))
        .map(|rest| rest.split(',').next().unwrap_or(rest).to_string())
        .collect()
}

fn cpu_arg(request: &VmRequest) -> String {
    match request.cpu_baseline {
        None => "host".to_string(),
        // `CpuLevel` prints as `x86-64-v3`, which is QEMU's name for that model
        // **where QEMU has it**. Not every build does — see `presents_levels`,
        // and the outage that taught us.
        Some(level) => format!("{level},enforce"),
    }
}

/// Whether this QEMU knows the `x86-64-vN` models a baseline is expressed in.
///
/// Asked of the binary, once, rather than assumed from the architecture. It was
/// assumed, and on Debian 13's QEMU 10 the assumption is false: it has sixty
/// versioned models and not one `x86-64-vN` among them.
///
/// What that cost, on two real machines: the console advised a baseline — "one
/// migration domain, horst loses ibrs_enhanced" — the API accepted it, the agent
/// reported presenting it, and every guest started afterwards died instantly
/// with
///
/// ```text
/// qemu-system-x86_64: unable to find CPU model 'x86-64-v1'
/// ```
///
/// An operator following the platform's own advice ended with machines that
/// would not start. A capability nobody checked is a capability the platform
/// claims on somebody else's behalf.
///
/// Missing `qemu-system-*` answers `false`, which is the closed direction: a
/// node that cannot be asked cannot be shown able.
async fn presents_levels(binary: &str) -> bool {
        let Ok(out) = tokio::process::Command::new(binary)
        .args(["-cpu", "help"])
        .output()
        .await
    else {
        return false;
    };
    levels_listed(&String::from_utf8_lossy(&out.stdout))
}

/// The reading half of [`presents_levels`], separated so it can be tested
/// against what QEMU really prints.
///
/// Every level, not merely one: a fleet baselined to a level half of it cannot
/// present is the same outage with extra steps.
fn levels_listed(listing: &str) -> bool {
    use velstra_cloud_model::cpu::CpuLevel;
    [CpuLevel::V1, CpuLevel::V2, CpuLevel::V3, CpuLevel::V4]
        .iter()
        .all(|l| listing.contains(&l.to_string()))
}

fn qemu_args(
    layout: &Layout,
    request: &VmRequest,
    monitor: &Path,
    incoming: Option<&str>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-machine".into(),
        "accel=kvm".into(),
        // Never omitted. QEMU's x86_64 default is `qemu64`, an Opteron-G1-era
        // model without SSSE3, SSE4.1, SSE4.2 or POPCNT — below x86-64-v2,
        // which RHEL 9, CentOS Stream 9 and a growing list of distributions
        // require in order to boot at all. Leaving `-cpu` off does not mean
        // "whatever the host has"; it means a 2006 processor.
        "-cpu".into(),
        cpu_arg(request).into(),
        "-nodefaults".into(),
        "-display".into(),
        "none".into(),
        // A display adapter, even though nobody is looking at it.
        //
        // `-nodefaults` takes away every device QEMU would have added, which is
        // the right starting point — and then this one has to come back. A
        // stock cloud image's kernel command line is `console=tty0
        // console=ttyS0`: it initialises the *first* console before it can
        // write to the second, and on a machine with no display adapter it dies
        // there. What that looks like from outside is a guest that resets about
        // once a second, for ever, having printed GRUB's "Booting …" and not
        // one character of kernel output. Seven hundred and seventy-nine times,
        // in the run that found this.
        //
        // `VGA` specifically, not `virtio-gpu-pci`: the legacy console is what
        // is missing, and a virtio adapter is not one — measured, and it does
        // not boot either.
        //
        // `-display none` stays: this gives the guest an adapter, not the host
        // a window.
        "-device".into(),
        "VGA".into(),
        // Where the guest's cloud-init is told to look.
        //
        // This platform serves NoCloud (`/meta-data`, `/user-data`) and EC2
        // IMDS on 169.254.169.254 — and a stock cloud image asks for neither,
        // because cloud-init does not probe. Its `ds-identify` looks the machine
        // over first, and a plain QEMU machine matches no datasource it knows,
        // so it **disables cloud-init entirely** and says nothing about it on
        // the console.
        //
        // The consequence is a guest that boots perfectly and is configured by
        // nobody: no user, no SSH key, no network, and `ssh.service` failing for
        // want of host keys cloud-init would have generated. Every guest this
        // platform ever started from a public image was that guest.
        //
        // `ds=nocloud;s=<url>` in the SMBIOS system serial is the documented way
        // to say it (cloud-init 21.3 and later), and it is preferred over the
        // alternative — claiming to be Amazon EC2 or OpenStack in DMI so that
        // ds-identify guesses right — because this platform is neither, and a
        // guest that believes it is on EC2 makes decisions on that basis.
        //
        // The trailing slash is load-bearing: cloud-init appends `meta-data` and
        // `user-data` to it.
        "-smbios".into(),
        format!(
            "type=1,serial=ds=nocloud;s=http://{}/",
            crate::metadata::ADDRESS
        )
        .into(),
        "-smp".into(),
        request.vcpus.to_string().into(),
        "-m".into(),
        request.memory_mib.to_string().into(),
        "-drive".into(),
        format!(
            "file={},format=raw,if=virtio",
            layout.disk(&request.instance).display()
        )
        .into(),
        "-qmp".into(),
        format!("unix:{},server=on,wait=off", monitor.display()).into(),
        // A socket **and** a file, which is one chardev doing both.
        //
        // The file half is not negotiable and predates the socket: a guest that
        // cannot boot is the one that most needs to be heard and the one that
        // says the least, and with no console at all the first real image
        // started here produced not a single byte to explain itself. So
        // `logfile=` — QEMU writes everything the line carries to it whether or
        // not anybody is attached, which is what keeps the tail on the guest's
        // status honest.
        //
        // The socket half is what a console can attach to. A file cannot be
        // typed at, so until now there was no way into a guest at all: a machine
        // whose network did not come up, or whose key was not installed, could
        // be watched failing and not reached. `wait=off` because a guest must
        // never wait for somebody to look at it.
        "-chardev".into(),
        format!(
            "socket,id=serial0,path={},server=on,wait=off,logfile={},logappend=on",
            layout.console_socket(&request.instance).display(),
            layout.console(&request.instance).display(),
        )
        .into(),
        "-serial".into(),
        "chardev:serial0".into(),
        // The display, reachable. `-display none` above keeps the *host*
        // windowless; this makes what the VGA adapter is showing attachable
        // over VNC's own protocol, on a unix socket beside the serial one.
        // Without it, a guest whose kernel stopped writing to ttyS0 — a panic
        // with `console=tty0` first, a graphical installer, a locked-up
        // getty — was invisible: the serial line showed the past and the
        // screen showed the truth, and nothing could reach the screen.
        "-vnc".into(),
        format!("unix:{}", layout.vnc_socket(&request.instance).display()).into(),
    ];
    // Passed-through hardware. `vfio-pci` is the only device model here:
    // whole-device passthrough is all this platform offers, and a mediated
    // device would need a `sysfsdev` this build has nothing to fill in with.
    for address in &request.devices {
        args.push("-device".into());
        args.push(format!("vfio-pci,host={address}").into());
    }
    match &layout.boot {
        // `-bios`, not `-kernel`. QEMU's `-kernel` takes a Linux kernel and
        // nothing else; a firmware blob given to it boots nothing and says
        // nothing. `None` leaves QEMU its own SeaBIOS, which is what makes a
        // stock cloud image just work.
        Boot::Firmware(None) => {}
        Boot::Firmware(Some(path)) => {
            args.push("-bios".into());
            args.push(path.clone().into());
        }
        Boot::Kernel {
            kernel,
            cmdline,
            initrd,
        } => {
            args.push("-kernel".into());
            args.push(kernel.clone().into());
            if let Some(initrd) = initrd {
                args.push("-initrd".into());
                args.push(initrd.clone().into());
            }
            // Passed even when empty would be wrong, but an empty one is a
            // kernel that cannot find a root filesystem — the caller's problem
            // to notice, not this function's to paper over.
            if !cmdline.is_empty() {
                args.push("-append".into());
                args.push(cmdline.clone().into());
            }
        }
    }
    for (index, nic) in request.nics.iter().enumerate() {
        // The guest's NIC order is the order of this list, and a guest that
        // finds its addresses on the wrong NIC after a move is an outage with
        // no error message.
        args.push("-netdev".into());
        args.push(format!("tap,id=n{index},ifname={},script=no,downscript=no", nic.tap).into());
        args.push("-device".into());
        // Stated rather than left to QEMU, which would otherwise hand every
        // guest the same default address and give the second one on a link a
        // duplicate.
        let mut device = format!("virtio-net-pci,netdev=n{index}");
        if let Some(mac) = &nic.mac {
            device.push_str(&format!(",mac={mac}"));
        }
        args.push(device.into());
    }
    if let Some(incoming) = incoming {
        args.push("-incoming".into());
        args.push(incoming.into());
    }
    args
}

/// What to set before a transfer starts.
///
/// `downtime-limit` is the pause the guest may take at the end, in
/// milliseconds — the same number Cloud Hypervisor calls `downtime_ms`, which
/// is why the model carries one field and not two.
fn migrate_parameters(transfer: &Transfer) -> Result<Vec<(&'static str, u64)>> {
    if transfer.mode == MigrationMode::Reboot {
        return Err(HostError::failed(
            "a reboot migration is not a transfer; it is a stop here and a start there",
        ));
    }
    if transfer.connections > 1 && transfer.url.starts_with("unix:") {
        return Err(HostError::failed(
            "more than one connection is unsupported over a unix socket",
        ));
    }
    let mut parameters = vec![("downtime-limit", u64::from(transfer.downtime_ms))];
    if transfer.connections > 1 {
        parameters.push(("multifd-channels", u64::from(transfer.connections)));
    }
    Ok(parameters)
}

/// One line of QMP: the answer, or nothing if it was an event.
fn qmp_reply(line: &str) -> Result<Option<Value>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(|e| {
        HostError::failed(format!("the monitor sent something that is not QMP: {e}"))
    })?;
    if value.get("event").is_some() || value.get("QMP").is_some() {
        return Ok(None);
    }
    if let Some(error) = value.get("error") {
        let description = error
            .get("desc")
            .and_then(|d| d.as_str())
            .unwrap_or("the monitor refused");
        return Err(HostError::failed(description));
    }
    Ok(value.get("return").cloned().or(Some(Value::Null)))
}

/// QEMU's run states, mapped to the three words a status may use.
fn state_of(run_state: &str) -> InstanceState {
    match run_state {
        "running" => InstanceState::Running,
        // Everything here is a machine that exists and is not executing. None
        // of them is a transition, because a status that means "in progress"
        // outlives whatever wrote it.
        "paused" | "prelaunch" | "suspended" | "shutdown" | "postmigrate" | "finish-migrate"
        | "save-vm" | "restore-vm" | "watchdog" | "debug" => InstanceState::Stopped,
        // `inmigrate` never reaches here — it is a receiver, not a guest.
        _ => InstanceState::Failed,
    }
}

/// Whether a `query-migrate` status means a transfer is still under way.
fn still_going(status: &str) -> bool {
    matches!(
        status,
        "setup" | "active" | "postcopy-active" | "postcopy-paused" | "device" | "cancelling"
    )
}

/// What `query-migrate` says has moved, in MiB. Zero when it says nothing,
/// which is what a destination usually says.
fn transferred_mib(answer: &Value) -> u64 {
    answer
        .get("ram")
        .and_then(|ram| ram.get("transferred"))
        .and_then(|bytes| bytes.as_u64())
        .unwrap_or(0)
        / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Nic;

    fn layout() -> Layout {
        Layout {
            run_dir: PathBuf::from("/var/lib/velstra/instances"),
            binary: "qemu-system-x86_64".to_string(),
            migration_address: Some("10.0.0.2".to_string()),
            ..Default::default()
        }
    }

    fn request() -> VmRequest {
        VmRequest {
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
        }
    }

    fn words(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
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

    /// Every guest gets a display adapter, and it is not decoration.
    ///
    /// A stock cloud image boots with `console=tty0 console=ttyS0` and
    /// initialises the first before it can write to the second. On a machine
    /// with no display adapter the kernel dies there — and because it dies
    /// before the serial console exists, the only evidence is GRUB's "Booting
    /// …" repeating about once a second with no kernel output ever. Seven
    /// hundred and seventy-nine times, in the run that found this. Every guest
    /// this platform started from a public image did that.
    #[test]
    fn a_guest_is_given_a_display_adapter_even_though_nobody_looks_at_it() {
        let args = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            None,
        ));
        assert!(
            args.windows(2).any(|w| w[0] == "-device" && w[1] == "VGA"),
            "no display adapter: every stock cloud image resets for ever\n{args:?}"
        );
        // And the host still opens no window: the adapter is for the guest's
        // kernel, not for anybody to look at.
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-display" && w[1] == "none"),
            "{args:?}"
        );
    }

    /// The guest is told where its configuration is, or nothing configures it.
    ///
    /// cloud-init does not probe: `ds-identify` looks the machine over, finds
    /// nothing it recognises on a plain QEMU machine, and switches cloud-init
    /// off — silently, with not one line on the console. The guest then boots to
    /// a login prompt with no user, no SSH key, no network configuration and a
    /// failed `ssh.service`, and this platform's metadata service, sitting
    /// there answering, is asked nothing by anybody.
    #[test]
    fn a_guest_is_told_where_its_metadata_is_or_cloud_init_never_runs() {
        let args = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            None,
        ));
        let hint = args
            .windows(2)
            .find(|w| w[0] == "-smbios")
            .map(|w| w[1].clone())
            .unwrap_or_else(|| panic!("no datasource hint at all: {args:?}"));
        // The serial, because that is the field ds-identify reads.
        assert!(hint.starts_with("type=1,serial=ds=nocloud;"), "{hint}");
        // The address the DHCP responder hands out, from the same constant, so
        // the two cannot drift apart.
        assert!(
            hint.contains(&crate::metadata::ADDRESS.to_string()),
            "{hint}"
        );
        // The trailing slash is load-bearing: cloud-init appends `meta-data`
        // and `user-data` to it, and without it they land a directory up.
        assert!(hint.ends_with('/'), "{hint}");
    }

    /// The console is a socket now, and the log did not stop being a log.
    ///
    /// Both halves are load-bearing and they pull in opposite directions. A
    /// socket is the only thing that can be typed at — with a file, a guest
    /// whose network never came up or whose key was never installed could be
    /// watched failing and not reached. But a socket nobody has attached to
    /// carries nothing, and the guest that most needs to be heard is exactly the
    /// one nobody is watching yet. `logfile=` is what makes it both.
    #[test]
    fn the_guests_serial_line_can_be_attached_to_and_is_recorded_regardless() {
        let layout = layout();
        let request = request();
        let args = words(&qemu_args(
            &layout,
            &request,
            Path::new("/run/qmp.sock"),
            None,
        ));
        let chardev = args
            .windows(2)
            .find(|w| w[0] == "-chardev")
            .map(|w| w[1].clone())
            .unwrap_or_else(|| panic!("no serial chardev: {args:?}"));

        let socket = layout.console_socket(&request.instance);
        assert!(
            chardev.contains(&format!("path={}", socket.display())),
            "{chardev}"
        );
        // Never waiting: a guest must not be held at the door because nobody is
        // looking at it yet.
        assert!(chardev.contains("wait=off"), "{chardev}");

        let log = layout.console(&request.instance);
        assert!(
            chardev.contains(&format!("logfile={}", log.display())),
            "a socket alone loses everything said before anybody attached: {chardev}"
        );
        assert!(
            chardev.contains("logappend=on"),
            "a restart must not truncate what the last boot said: {chardev}"
        );

        // And the line is that chardev, not a second one.
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-serial" && w[1] == "chardev:serial0"),
            "{args:?}"
        );
    }

    #[test]
    fn a_receiving_guest_is_the_same_machine_as_a_booting_one_plus_incoming() {
        // The property that makes a migration work at all: the destination's
        // command line has to describe the same machine, or the guest resumes
        // into devices that are not there.
        let booting = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            None,
        ));
        let receiving = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            Some("tcp:0:4900"),
        ));
        assert_eq!(receiving[..booting.len()], booting[..]);
        assert_eq!(&receiving[booting.len()..], &["-incoming", "tcp:0:4900"]);

        // Two taps, in the order the instance declares its ports.
        let nets: Vec<&String> = booting.iter().filter(|a| a.starts_with("tap,")).collect();
        assert_eq!(nets.len(), 2);
        assert!(nets[0].contains("id=n0,ifname=vt-a"), "{nets:?}");
        assert!(nets[1].contains("id=n1,ifname=vt-b"), "{nets:?}");
        assert!(booting.contains(&"8192".to_string()));
    }

    /// A guest is never started on QEMU's default CPU.
    ///
    /// The regression this pins is a real one this code shipped with: no
    /// `-cpu` at all, which is not "whatever the host has" but `qemu64` — an
    /// Opteron-G1-era model below x86-64-v2. RHEL 9 and CentOS Stream 9 need
    /// v2 to boot, so those images simply did not run here.
    #[test]
    fn a_guest_is_never_left_on_qemus_2006_default_cpu() {
        let args = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            None,
        ));
        let at = args
            .iter()
            .position(|a| a == "-cpu")
            .expect("no -cpu given");
        assert_eq!(args[at + 1], "host");
    }

    /// A declared baseline reaches the command line, with `enforce`.
    ///
    /// `enforce` is the whole difference between a baseline and a hope:
    /// without it QEMU silently drops features the host cannot provide, the
    /// guest runs with less than was promised, and the platform goes on
    /// believing a migration to a matching node is safe.
    #[test]
    fn a_baseline_is_passed_to_qemu_and_enforced() {
        let mut r = request();
        r.cpu_baseline = Some(velstra_cloud_model::cpu::CpuLevel::V3);
        let args = words(&qemu_args(&layout(), &r, Path::new("/run/qmp.sock"), None));
        let at = args
            .iter()
            .position(|a| a == "-cpu")
            .expect("no -cpu given");
        assert_eq!(args[at + 1], "x86-64-v3,enforce");
    }

    /// A passed-through device reaches the command line, and reads back off it.
    ///
    /// The round trip is the point: the agent remembers nothing, so which
    /// guest holds which device has to be recoverable from the machine. If
    /// these two ever disagree, a restarted agent offers a device a running
    /// guest is already using — and two guests with the same GPU is a host
    /// that does not survive it.
    #[test]
    fn a_passed_device_reaches_qemu_and_reads_back_off_its_command_line() {
        let mut r = request();
        r.devices = vec!["0000:41:00.0".into(), "0000:81:00.0".into()];
        let args = words(&qemu_args(&layout(), &r, Path::new("/run/qmp.sock"), None));

        let passed: Vec<&String> = args.iter().filter(|a| a.starts_with("vfio-pci,")).collect();
        assert_eq!(passed.len(), 2, "{args:?}");
        assert_eq!(passed[0], "vfio-pci,host=0000:41:00.0");
        assert_eq!(passed[1], "vfio-pci,host=0000:81:00.0");

        // And back out of a command line shaped like the one systemd reports.
        assert_eq!(passed_devices(&args.join(" ")), r.devices);
    }

    /// A guest with no devices produces no `-device` at all.
    ///
    /// Not cosmetic: an empty `-device` argument is one QEMU refuses, and a
    /// guest that will not start for a feature it never asked for is the worst
    /// kind of regression.
    #[test]
    fn a_guest_without_devices_gets_no_vfio_arguments() {
        let args = words(&qemu_args(
            &layout(),
            &request(),
            Path::new("/run/qmp.sock"),
            None,
        ));
        assert!(!args.iter().any(|a| a.contains("vfio-pci")), "{args:?}");
        assert!(passed_devices(&args.join(" ")).is_empty());
    }

    /// Reading devices off a command line ignores everything that is not one.
    #[test]
    fn reading_devices_back_ignores_the_rest_of_the_command_line() {
        let command = "/usr/bin/qemu-system-x86_64 -cpu host -device vfio-pci,host=0000:41:00.0 \
                       -device virtio-net-pci,netdev=n0 -m 8192";
        assert_eq!(passed_devices(command), ["0000:41:00.0"]);
        assert!(passed_devices("").is_empty());
    }

    #[test]
    fn a_receiver_publishes_an_address_a_peer_can_actually_reach() {
        // `-incoming tcp:0:4900` means every address of this machine, which is
        // not something another node can be told to connect to.
        let vmm = QemuVmm::new(layout());
        assert_eq!(
            vmm.published_url("tcp:0:4900").as_deref(),
            Some("tcp:10.0.0.2:4900")
        );
        // A local move needs no address and publishes the socket itself.
        assert_eq!(
            vmm.published_url("unix:/var/lib/velstra/instances/p~i1/migrate.sock")
                .as_deref(),
            Some("unix:/var/lib/velstra/instances/p~i1/migrate.sock")
        );
        // And a node that was never told how it is reached cannot publish a TCP
        // receiver at all, rather than publishing one nobody can connect to.
        let anonymous = QemuVmm::new(Layout {
            migration_address: None,
            ..layout()
        });
        assert_eq!(anonymous.published_url("tcp:0:4900"), None);
    }

    #[test]
    fn a_reply_is_told_apart_from_the_events_that_arrive_around_it() {
        assert_eq!(
            qmp_reply(r#"{"QMP":{"version":{"qemu":{"major":8}}}}"#).unwrap(),
            None,
            "the greeting was read as an answer"
        );
        assert_eq!(
            qmp_reply(r#"{"event":"STOP","timestamp":{"seconds":1}}"#).unwrap(),
            None,
            "an event was read as an answer"
        );
        assert_eq!(
            qmp_reply(r#"{"return":{"status":"running"}}"#)
                .unwrap()
                .unwrap()["status"],
            "running"
        );
        // An error carries the sentence an operator will read on the object.
        let err = qmp_reply(r#"{"error":{"class":"GenericError","desc":"Migration is disabled"}}"#)
            .unwrap_err();
        assert!(err.to_string().contains("Migration is disabled"), "{err}");
        assert!(qmp_reply("not json").is_err());
    }

    #[test]
    fn what_the_guest_is_doing_is_one_of_three_words_and_never_a_transition() {
        assert_eq!(state_of("running"), InstanceState::Running);
        assert_eq!(state_of("paused"), InstanceState::Stopped);
        assert_eq!(state_of("finish-migrate"), InstanceState::Stopped);
        assert_eq!(state_of("guest-panicked"), InstanceState::Failed);
        assert_eq!(state_of("io-error"), InstanceState::Failed);
    }

    #[test]
    fn a_transfer_that_is_still_going_is_not_started_again() {
        // This is what stops a pass from issuing a second `migrate` on top of
        // the first every time it comes round while a large guest copies.
        assert!(still_going("active"));
        assert!(still_going("postcopy-active"));
        assert!(!still_going("completed"));
        assert!(!still_going("failed"));
        assert!(!still_going("cancelled"));
    }

    #[test]
    fn progress_is_whatever_the_monitor_says_and_zero_when_it_says_nothing() {
        let answer: Value =
            serde_json::from_str(r#"{"status":"active","ram":{"transferred":1073741824}}"#)
                .unwrap();
        assert_eq!(transferred_mib(&answer), 1024);
        let quiet: Value = serde_json::from_str(r#"{"status":"setup"}"#).unwrap();
        assert_eq!(transferred_mib(&quiet), 0, "progress was invented");
    }

    #[test]
    fn the_pause_the_guest_may_take_is_passed_through_and_the_rest_is_refused() {
        assert_eq!(
            migrate_parameters(&transfer("tcp:10.0.0.2:4900")).unwrap(),
            vec![("downtime-limit", 300)]
        );

        let mut parallel = transfer("tcp:10.0.0.2:4900");
        parallel.connections = 4;
        assert_eq!(
            migrate_parameters(&parallel).unwrap(),
            vec![("downtime-limit", 300), ("multifd-channels", 4)]
        );

        // The combination both VMMs document as unsupported.
        let mut local = transfer("unix:/tmp/migrate.sock");
        local.connections = 4;
        assert!(
            migrate_parameters(&local)
                .unwrap_err()
                .to_string()
                .contains("unix socket")
        );

        let mut reboot = transfer("tcp:10.0.0.2:4900");
        reboot.mode = MigrationMode::Reboot;
        assert!(
            migrate_parameters(&reboot)
                .unwrap_err()
                .to_string()
                .contains("stop here and a start there")
        );
    }
}

#[cfg(test)]
mod what_query_block_really_answers {
    /// Recorded from a live guest on 2026-08-29, holding one attached volume
    /// beside its root disk. Kept verbatim: the shape is the whole lesson.
    const ANSWER: &str = r##"[
      { "device": "virtio0",
        "qdev": "/machine/peripheral-anon/device[2]/virtio-backend",
        "type": "unknown",
        "inserted": { "node-name": "#block156",
                      "file": "/var/lib/velstra/instances/projects~p1~instances~m/root.raw" } },
      { "device": "",
        "qdev": "/machine/peripheral/projects_p1_volumes_data/virtio-backend",
        "type": "unknown",
        "inserted": { "node-name": "projects_p1_volumes_data",
                      "file": "/var/lib/velstra/pool/projects~p1~volumes~data.qcow2" } }
    ]"##;

    /// The reading `open_volumes` does, on the answer above.
    ///
    /// A copy of the filter rather than a call, because `open_volumes` needs a
    /// live QMP socket. What it protects is the part that was wrong: which field
    /// carries the id.
    fn volumes(answer: &serde_json::Value) -> Vec<(String, String)> {
        answer
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| {
                let id = d
                    .get("inserted")
                    .and_then(|i| i.get("node-name"))
                    .and_then(|v| v.as_str())?;
                let volume = crate::hostfs::from_qmp_id(id)?;
                Some((volume, id.to_string()))
            })
            .collect()
    }

    #[test]
    fn the_id_is_in_inserted_node_name_and_not_in_qdev() {
        let answer: serde_json::Value = serde_json::from_str(ANSWER).unwrap();
        let open = volumes(&answer);
        assert_eq!(
            open,
            vec![(
                "projects/p1/volumes/data".to_string(),
                "projects_p1_volumes_data".to_string()
            )],
            "the attached volume was not recognised"
        );
    }

    #[test]
    fn the_root_disk_is_not_reported_as_a_volume() {
        // `#block156` is QEMU's own name for a node nobody named. Read as a
        // volume it would have the node report one the cell never made.
        let answer: serde_json::Value = serde_json::from_str(ANSWER).unwrap();
        assert!(
            !volumes(&answer)
                .iter()
                .any(|(v, _)| v.contains("block") || v.contains("root")),
            "the root disk was claimed as a volume"
        );
    }

    #[test]
    fn reading_qdev_instead_finds_nothing_which_is_the_bug_this_replaced() {
        let answer: serde_json::Value = serde_json::from_str(ANSWER).unwrap();
        let by_qdev: Vec<_> = answer
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| {
                let id = d.get("qdev").and_then(|v| v.as_str())?;
                crate::hostfs::from_qmp_id(id)
            })
            .collect();
        assert!(
            by_qdev.is_empty(),
            "if qdev parsed, this test is asserting the wrong thing"
        );
    }
}

#[cfg(test)]
mod what_this_qemu_can_actually_present {
    use super::*;

    /// What Debian 13's QEMU 10 really prints, trimmed. Recorded from the
    /// machine the outage happened on.
    const DEBIAN_13: &str = "\
Available CPUs:
  486                   (alias configured by machine type)
  486-v1
  Broadwell-IBRS        (alias of Broadwell-v3)
  Broadwell-noTSX       (alias of Broadwell-v2)
  Broadwell-v2          Intel Core Processor (Broadwell, no TSX)
  base                  base CPU model type with no features enabled
  qemu64                (alias configured by machine type)
  qemu64-v1             QEMU Virtual CPU version 2.5+
";

    /// A build that does carry them.
    const WITH_LEVELS: &str = "\
Available CPUs:
  qemu64                (alias configured by machine type)
  x86-64-v1             x86-64 baseline
  x86-64-v2             x86-64 level 2
  x86-64-v3             x86-64 level 3
  x86-64-v4             x86-64 level 4
";

    #[test]
    fn a_qemu_without_the_level_models_says_so() {
        // The whole of the outage in one assertion. The platform assumed that
        // x86_64 meant "can be baselined", the console advised one, the API took
        // it, the agent reported presenting it — and every guest started
        // afterwards died with `unable to find CPU model 'x86-64-v1'`.
        //
        // Sixty versioned models in that listing and not one `x86-64-vN`.
        assert!(!levels_listed(DEBIAN_13));
    }

    #[test]
    fn a_qemu_with_them_says_so_too() {
        assert!(levels_listed(WITH_LEVELS));
    }

    #[test]
    fn half_of_them_is_not_enough() {
        // A fleet baselined to a level half of it cannot present is the same
        // outage with extra steps — and harder to see, because the machines that
        // can present it work.
        let partial = WITH_LEVELS.replace("  x86-64-v4             x86-64 level 4\n", "");
        assert!(!levels_listed(&partial));
    }

    #[test]
    fn nothing_at_all_is_not_a_capability() {
        // A binary that could not be asked cannot be shown able. The closed
        // direction, like everywhere else a capability is claimed.
        assert!(!levels_listed(""));
    }
}
