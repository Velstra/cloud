//! A machine in a process.
//!
//! This is not a mock. It is a hypervisor with the behaviours that matter —
//! guests that keep running when the agent goes away, a start that refuses if
//! the image or disk is missing, a guest that can be made to crash on demand —
//! so the whole platform can be exercised end to end on a laptop, in a CI
//! runner, and in every test in this repository.
//!
//! The one rule it holds to: **the machine outlives the agent**. A [`FakeVmm`]
//! clone is a second handle onto the same host, the way a second process on a
//! real node sees the same VMs; it is never a second machine. That is what
//! makes "drop the agent, build another, watch it re-derive" a real test rather
//! than a rehearsal.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use velstra_cloud_model::{
    meta::Timestamp,
    migration::MigrationMode,
    resources::{Capacity, InstanceState, NetworkSpec, PortSpec},
    security::ResolvedRule,
};

use crate::host::{
    Datapath, HostError, HostState, ProgrammedPort, Receiver, Result, Transfer, VmObservation,
    VmRequest, Vmm,
};

/// How much a single `send` copies before it either lands or fails.
///
/// The number is arbitrary; that it is copied *before* a failure is not. A
/// pre-copy transfer that dies half way has still moved memory, and the
/// destination has to be able to say how much — otherwise a test of a failed
/// migration cannot tell "nothing was attempted" from "it got most of the way".
const PROGRESS_MIB: u64 = 512;

/// Something that can be made to fail, so a test can ask what the agent does
/// about it rather than assume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    Start,
    Stop,
    Delete,
    Disk,
    Pull,
    OpenVolume,
    CloseVolume,
    PrepareReceiver,
    TearDownReceiver,
    Send,
    CancelSend,
}

/// The processor every fake machine reports unless a test says otherwise.
fn ordinary_cpu() -> velstra_cloud_model::cpu::NodeCpu {
    let flags: BTreeSet<String> = [
        "cx16", "lahf_lm", "popcnt", "sse3", "sse4_1", "sse4_2", "ssse3", "avx", "avx2", "bmi1",
        "bmi2", "f16c", "fma", "lzcnt", "movbe",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    velstra_cloud_model::cpu::NodeCpu {
        arch: "x86_64".into(),
        vendor: "GenuineIntel".into(),
        model_name: "Intel(R) Xeon(R) Gold 6248R".into(),
        presents: "host".into(),
        presented_flags: flags.clone(),
        flags,
        can_mask: true,
        ..Default::default()
    }
}

#[derive(Default)]
struct Machine {
    /// What other machines can reach this one at. Part of every URL it hands
    /// out, so a test reading a published URL can see which node it points at.
    id: String,
    vms: BTreeMap<String, VmObservation>,
    /// Guests whose plug was pulled, in order. A test about escalation has to
    /// tell a graceful stop from a kill, and the observation alone cannot:
    /// both end with the guest not running.
    killed: Vec<String>,
    disks: BTreeSet<String>,
    images: BTreeSet<String>,
    fetching: BTreeSet<String>,
    /// Disks this fake machine claims to have, so a test can drive the console's
    /// disk picker without a machine that has spare disks.
    devices: Vec<velstra_cloud_model::ceph::BlockDevice>,
    ceph: Option<velstra_cloud_model::ceph::NodeCeph>,
    /// What this fake machine claims its processor is.
    ///
    /// An ordinary one by default, and the *same* one on every fake machine,
    /// because a homogeneous cell is what a test about something else needs
    /// underneath it. A test that wants the "agent too old to report" case
    /// sets this to `None` with [`FakeVmm::with_cpu`] and gets the refusal
    /// that follows from it.
    cpu: Option<velstra_cloud_model::cpu::NodeCpu>,
    /// PCI devices this fake machine claims to have, so a test can drive the
    /// passthrough paths without hardware.
    pci_devices: Vec<velstra_cloud_model::pci::PciDevice>,
    /// Where each pulled image was told to come from, so a test can assert the
    /// node was handed the *right* source and not merely that it pulled.
    pulled_from: BTreeMap<String, String>,
    volumes: BTreeMap<String, String>,
    receivers: BTreeMap<String, Receiver>,
    /// Transfers this machine has begun and not yet finished, and where each
    /// one is going.
    sending: BTreeMap<String, String>,
    /// Instances whose transfer is to stay in flight until a test says
    /// otherwise, so the agent can be asked what it does *during* a move.
    stalled: BTreeSet<String>,
    /// How often each thing was asked for, per target. A test that wants to
    /// know an action did not happen twice asks here.
    counts: BTreeMap<(Fault, String), usize>,
    faults: BTreeMap<(Fault, String), String>,
    next_pid: u32,
    next_port: u16,
    capacity: Capacity,
    /// Where the URLs this machine publishes lead. Shared with every other
    /// machine on the same network.
    network: FakeNetwork,
}

/// The wire between two machines: a map from a published URL to whatever is
/// listening behind it.
///
/// This exists because a migration is the one thing a node cannot do alone, and
/// a fake that could not connect two of them would leave the interesting half
/// of the feature untested. The reference is weak on purpose — a machine that
/// has been dropped is a node that is gone, and sending to it must fail the way
/// sending to a dead node fails, not resurrect it.
#[derive(Clone, Default)]
pub struct FakeNetwork {
    wires: Arc<Mutex<BTreeMap<String, Weak<Mutex<Machine>>>>>,
}

impl FakeNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// A machine on this network, reachable by the URLs it publishes.
    pub fn host(&self, id: &str) -> FakeVmm {
        let vmm = FakeVmm::new();
        {
            let mut m = vmm.machine.lock().unwrap();
            m.id = id.to_string();
            m.network = self.clone();
        }
        vmm
    }

    fn listen(&self, url: &str, machine: &Arc<Mutex<Machine>>) {
        self.wires
            .lock()
            .unwrap()
            .insert(url.to_string(), Arc::downgrade(machine));
    }

    fn hang_up(&self, url: &str) {
        self.wires.lock().unwrap().remove(url);
    }

    fn dial(&self, url: &str) -> Option<Arc<Mutex<Machine>>> {
        self.wires.lock().unwrap().get(url)?.upgrade()
    }
}

/// An in-process hypervisor. Cloning shares the machine.
#[derive(Clone)]
pub struct FakeVmm {
    machine: Arc<Mutex<Machine>>,
    /// Where this fake pretends a guest's disk is, when a test has given it
    /// one. A real file, because the one caller reads the bytes and hashes
    /// them — a fake that handed out a path to nothing could not be used to
    /// prove that a capture copies anything.
    disks_on_disk: Arc<Mutex<BTreeMap<String, std::path::PathBuf>>>,
}

impl Default for FakeVmm {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeVmm {
    pub fn new() -> Self {
        Self {
            machine: Arc::new(Mutex::new(Machine {
                id: "fake".to_string(),
                // The same processor on every fake machine, so a cell built
                // out of them is homogeneous — which is what a test about
                // something other than CPUs needs underneath it.
                cpu: Some(ordinary_cpu()),
                next_pid: 1000,
                next_port: 4900,
                capacity: Capacity {
                    vcpus: 32,
                    memory_mib: 131_072,
                    disk_gib: 4096,
                    numa_free_mib: vec![65_536, 65_536],
                    hugepages_1gi: 0,
                },
                ..Default::default()
            })),
            disks_on_disk: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Give a guest a real file to be its disk.
    ///
    /// A real one, because the one caller that asks — making an image out of a
    /// guest — reads the bytes and hashes them. A fake that handed out a path
    /// to nothing could not be used to prove that a capture copies anything.
    pub fn with_disk_file(&self, instance: &str, path: std::path::PathBuf) {
        self.disks_on_disk
            .lock()
            .unwrap()
            .insert(instance.to_string(), path);
    }

    pub fn with_capacity(capacity: Capacity) -> Self {
        let vmm = Self::new();
        vmm.machine.lock().unwrap().capacity = capacity;
        vmm
    }

    /// Give this machine some disks.
    ///
    /// A real VMM reports what `lsblk` says; this is how a test says the same
    /// thing. It matters for the Ceph pass, which decides whether a disk may be
    /// consumed from this list and from nothing else.
    pub fn with_devices(self, devices: Vec<velstra_cloud_model::ceph::BlockDevice>) -> Self {
        self.machine.lock().unwrap().devices = devices;
        self
    }

    /// Make the next attempt at `what` on `target` fail, until healed.
    pub fn fail(&self, what: Fault, target: &str, why: &str) {
        self.machine
            .lock()
            .unwrap()
            .faults
            .insert((what, target.to_string()), why.to_string());
    }

    pub fn heal(&self, what: Fault, target: &str) {
        self.machine
            .lock()
            .unwrap()
            .faults
            .remove(&(what, target.to_string()));
    }

    /// The guest died in a way the machine could not repair — the VMM exited
    /// and the object it left behind says so.
    pub fn crash(&self, instance: &str) {
        if let Some(vm) = self.machine.lock().unwrap().vms.get_mut(instance) {
            vm.state = InstanceState::Failed;
            vm.pid = None;
        }
    }

    /// The whole machine forgot: a host reboot, or a VMM that vanished without
    /// leaving anything behind.
    pub fn vanish(&self, instance: &str) {
        self.machine.lock().unwrap().vms.remove(instance);
    }

    pub fn is_running(&self, instance: &str) -> bool {
        self.machine
            .lock()
            .unwrap()
            .vms
            .get(instance)
            .map(|v| v.state == InstanceState::Running)
            .unwrap_or(false)
    }

    /// How often something was asked of this machine. The counter a test uses
    /// to say "and not twice".
    pub fn count(&self, what: Fault, target: &str) -> usize {
        self.machine
            .lock()
            .unwrap()
            .counts
            .get(&(what, target.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Put an image in the cache without pulling it, for a test that is about
    /// something else.
    /// Say this machine already holds an image, by **digest**.
    ///
    /// The digest and not the name, because that is what a node files bytes
    /// under: a fixture that seeded a name described a machine no real node
    /// resembles, and the check it was meant to satisfy compared the other
    /// spelling.
    pub fn cache_image(&self, digest: &str) {
        self.machine.lock().unwrap().images.insert(
            crate::hostfs::stored_as(digest).unwrap_or_else(|| digest.to_string()),
        );
    }

    /// Where this machine was told to fetch `image` from, if it pulled it.
    ///
    /// The question a test has to be able to ask: not "did it pull" but "was it
    /// handed the source the operator registered". Pulling from the wrong place
    /// and pulling from nowhere look identical from the outside otherwise.
    pub fn pulled_from(&self, image: &str) -> Option<String> {
        self.machine.lock().unwrap().pulled_from.get(image).cloned()
    }

    // ---- migration, from a test's side -----------------------------------

    /// Whether something is listening here for that guest right now.
    pub fn is_receiving(&self, instance: &str) -> bool {
        self.machine
            .lock()
            .unwrap()
            .receivers
            .contains_key(instance)
    }

    pub fn received_mib(&self, instance: &str) -> u64 {
        self.machine
            .lock()
            .unwrap()
            .receivers
            .get(instance)
            .map(|r| r.received_mib)
            .unwrap_or(0)
    }

    /// When the guest was started, which a migration must not change: it is
    /// the same guest afterwards.
    pub fn started_at(&self, instance: &str) -> Option<Timestamp> {
        self.machine
            .lock()
            .unwrap()
            .vms
            .get(instance)
            .and_then(|vm| vm.started_at)
    }

    pub fn is_sending(&self, instance: &str) -> bool {
        self.machine.lock().unwrap().sending.contains_key(instance)
    }

    /// Hold this guest's next transfer open instead of letting it land, so a
    /// test can ask what the source does while a move is in flight.
    pub fn stall(&self, instance: &str) {
        self.machine
            .lock()
            .unwrap()
            .stalled
            .insert(instance.to_string());
    }

    /// Let a stalled transfer land.
    pub fn finish_transfer(&self, instance: &str) -> Result<()> {
        let (url, guest) = {
            let mut m = self.machine.lock().unwrap();
            m.stalled.remove(instance);
            let Some(url) = m.sending.get(instance).cloned() else {
                return Err(HostError::failed(format!("no transfer of {instance} here")));
            };
            let Some(guest) = m.vms.get(instance).cloned() else {
                return Err(HostError::failed(format!("no guest {instance} here")));
            };
            (url, guest)
        };
        self.hand_over(instance, &url, guest)
    }

    /// Move the guest to whatever is listening at `url`, then let go of it
    /// here. The two halves are done under separate locks and in this order:
    /// a guest that exists on neither machine for an instant is an outage, and
    /// one that exists on both is worse.
    fn hand_over(&self, instance: &str, url: &str, guest: VmObservation) -> Result<()> {
        let far = self
            .far_end(url)
            .ok_or_else(|| HostError::failed(format!("nothing is listening at {url}")))?;
        {
            let mut d = far.lock().unwrap();
            if d.receivers.remove(instance).is_none() {
                return Err(HostError::failed(format!(
                    "{url} is not waiting for {instance}"
                )));
            }
            let pid = {
                d.next_pid += 1;
                d.next_pid
            };
            // The guest keeps the uptime it had. It is the same guest: a
            // migration a tenant can see in their own `uptime` is a migration
            // that was not live.
            d.vms.insert(
                instance.to_string(),
                VmObservation {
                    size: None,
                    console_tail: String::new(),
                    console_bytes: 0,
                    devices: Vec::new(),
                    state: InstanceState::Running,
                    pid: Some(pid),
                    started_at: guest.started_at,
                },
            );
            d.network.hang_up(url);
        }
        let mut m = self.machine.lock().unwrap();
        m.vms.remove(instance);
        m.sending.remove(instance);
        Ok(())
    }

    fn far_end(&self, url: &str) -> Option<Arc<Mutex<Machine>>> {
        let network = self.machine.lock().unwrap().network.clone();
        network.dial(url)
    }
}

/// Count the request, and fail it if a test asked for that.
fn check(machine: &mut Machine, what: Fault, target: &str) -> Result<()> {
    count(machine, what, target);
    match machine.faults.get(&(what, target.to_string())) {
        Some(why) => Err(HostError::failed(why)),
        None => Ok(()),
    }
}

fn count(machine: &mut Machine, what: Fault, target: &str) {
    *machine
        .counts
        .entry((what, target.to_string()))
        .or_default() += 1;
}

#[async_trait]
impl Vmm for FakeVmm {
    fn disk_path(&self, instance: &str) -> Option<std::path::PathBuf> {
        self.disks_on_disk.lock().unwrap().get(instance).cloned()
    }

    fn vmm_name(&self) -> &'static str {
        "fake"
    }

    async fn observe(&self) -> Result<HostState> {
        let m = self.machine.lock().unwrap();
        Ok(HostState {
            vms: m.vms.clone(),
            disks: m.disks.clone(),
            images: m.images.clone(),
            fetching: m.fetching.clone(),
            volumes: m.volumes.clone(),
            receivers: m.receivers.clone(),
            sending: m.sending.keys().cloned().collect(),
            devices: m.devices.clone(),
            ceph: m.ceph.clone(),
            cpu: m.cpu.clone(),
            pci_devices: m.pci_devices.clone(),
        })
    }

    async fn pull_image(&self, image: &str, digest: &str, source: &str) -> Result<()> {
        // Recorded, so a test can assert the node was told *where* to fetch
        // from and not merely that it tried: passing the wrong source is the
        // failure this whole path exists to make impossible.
        let mut m = self.machine.lock().unwrap();
        m.pulled_from.insert(image.to_string(), source.to_string());
        check(&mut m, Fault::Pull, image)?;
        // Filed the way a real backend files it — under the digest — so a test
        // cell answers the same question the same way as a machine does.
        m.images.insert(
            crate::hostfs::stored_as(digest).unwrap_or_else(|| digest.to_string()),
        );
        Ok(())
    }

    async fn create_disk(
        &self,
        instance: &str,
        _gib: u64,
        _image: &str,
        _format: velstra_cloud_model::resources::ImageFormat,
    ) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::Disk, instance)?;
        m.disks.insert(instance.to_string());
        Ok(())
    }

    async fn start(&self, request: &VmRequest) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::Start, &request.instance)?;
        // A real VMM refuses these too, and it matters that this one does: an
        // agent that got the order wrong must fail loudly here rather than
        // produce a guest with no disk and no network for somebody to find
        // later.
        // By the name the bytes are filed under, which is what `request.image`
        // now carries: an image object may be called anything.
        let want = crate::hostfs::stored_as(&request.image)
            .unwrap_or_else(|| request.image.clone());
        if !m.images.contains(&want) {
            return Err(HostError::failed(format!(
                "no verified copy of {} on this node",
                request.image
            )));
        }
        if !m.disks.contains(&request.instance) {
            return Err(HostError::failed("no root disk"));
        }
        if m.vms
            .get(&request.instance)
            .map(|v| v.state == InstanceState::Running)
            .unwrap_or(false)
        {
            return Err(HostError::failed("already running"));
        }
        m.next_pid += 1;
        let pid = m.next_pid;
        m.vms.insert(
            request.instance.clone(),
            VmObservation {
                size: None,
                console_tail: String::new(),
                console_bytes: 0,
                devices: Vec::new(),
                state: InstanceState::Running,
                pid: Some(pid),
                started_at: Some(Timestamp::now()),
            },
        );
        Ok(())
    }

    async fn stop(&self, instance: &str) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::Stop, instance)?;
        if let Some(vm) = m.vms.get_mut(instance) {
            vm.state = InstanceState::Stopped;
            vm.pid = None;
        }
        Ok(())
    }

    async fn kill(&self, instance: &str) -> Result<()> {
        // The plug, not the button: a fake that asked politely would let a
        // test about escalation pass while the escalation did nothing.
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::Stop, instance)?;
        if let Some(vm) = m.vms.get_mut(instance) {
            vm.state = InstanceState::Stopped;
            vm.pid = None;
        }
        m.killed.push(instance.to_string());
        Ok(())
    }

    async fn delete(&self, instance: &str) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::Delete, instance)?;
        m.vms.remove(instance);
        m.disks.remove(instance);
        Ok(())
    }

    async fn open_volume(&self, _instance: &str, volume: &str, _read_only: bool) -> Result<String> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::OpenVolume, volume)?;
        // Device names are handed out in open order, which is what a guest
        // sees; re-opening one already open returns the same device rather
        // than a second one.
        if let Some(device) = m.volumes.get(volume) {
            return Ok(device.clone());
        }
        let device = format!("/dev/vd{}", (b'b' + m.volumes.len() as u8) as char);
        m.volumes.insert(volume.to_string(), device.clone());
        Ok(device)
    }

    async fn close_volume(&self, _instance: &str, volume: &str) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::CloseVolume, volume)?;
        m.volumes.remove(volume);
        Ok(())
    }

    async fn capacity(&self) -> Result<Capacity> {
        Ok(self.machine.lock().unwrap().capacity.clone())
    }

    async fn prepare_receiver(&self, request: &VmRequest, _mode: MigrationMode) -> Result<String> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::PrepareReceiver, &request.instance)?;
        if let Some(receiver) = m.receivers.get(&request.instance) {
            // Asking twice is asking once, and the answer is the URL that is
            // already listening rather than a second one.
            return Ok(receiver.url.clone());
        }
        // A real receiver refuses these too. The guest resumes into its own
        // disk and its own image, and a destination without them takes delivery
        // of a guest it cannot run.
        // By the name the bytes are filed under, which is what `request.image`
        // now carries: an image object may be called anything.
        let want = crate::hostfs::stored_as(&request.image)
            .unwrap_or_else(|| request.image.clone());
        if !m.images.contains(&want) {
            return Err(HostError::failed(format!(
                "no verified copy of {} on this node",
                request.image
            )));
        }
        if !m.disks.contains(&request.instance) {
            return Err(HostError::failed("no root disk"));
        }
        if m.vms.contains_key(&request.instance) {
            return Err(HostError::failed(
                "this node already has that guest; receiving it would be the second copy",
            ));
        }
        m.next_port += 1;
        let url = format!("tcp:{}:{}", m.id, m.next_port);
        m.receivers.insert(
            request.instance.clone(),
            Receiver {
                url: url.clone(),
                received_mib: 0,
            },
        );
        m.network.listen(&url, &self.machine);
        Ok(url)
    }

    async fn tear_down_receiver(&self, instance: &str) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::TearDownReceiver, instance)?;
        if let Some(receiver) = m.receivers.remove(instance) {
            m.network.hang_up(&receiver.url);
        }
        // Note what this does *not* do: a guest that has arrived is in `vms`
        // and is left alone. The receiver became the guest's VMM the moment the
        // transfer landed, and tearing that down is the same command as killing
        // the guest.
        Ok(())
    }

    async fn send(&self, transfer: &Transfer) -> Result<()> {
        let (fault, guest, stall) = {
            let mut m = self.machine.lock().unwrap();
            count(&mut m, Fault::Send, &transfer.instance);
            if m.sending.contains_key(&transfer.instance) {
                return Err(HostError::failed(format!(
                    "{} is already being sent from this node",
                    transfer.instance
                )));
            }
            let Some(guest) = m.vms.get(&transfer.instance).cloned() else {
                return Err(HostError::failed(format!(
                    "{} is not on this node",
                    transfer.instance
                )));
            };
            if transfer.connections > 1 && transfer.url.starts_with("unix:") {
                return Err(HostError::failed(
                    "more than one connection is unsupported over a unix socket",
                ));
            }
            m.sending
                .insert(transfer.instance.clone(), transfer.url.clone());
            (
                m.faults
                    .get(&(Fault::Send, transfer.instance.clone()))
                    .cloned(),
                guest,
                m.stalled.contains(&transfer.instance),
            )
        };

        // Copy something before anything can go wrong, because that is what a
        // pre-copy transfer does and what makes a failure legible afterwards.
        if let Some(far) = self.far_end(&transfer.url) {
            let mut d = far.lock().unwrap();
            if let Some(receiver) = d.receivers.get_mut(&transfer.instance) {
                receiver.received_mib += PROGRESS_MIB;
            }
        }

        if let Some(why) = fault {
            // The guest is untouched and still running here. That is pre-copy's
            // whole point, and the thing this fake exists to let a test assert.
            let mut m = self.machine.lock().unwrap();
            m.sending.remove(&transfer.instance);
            return Err(HostError::failed(why));
        }
        if stall {
            // Under way and not finished. The agent must be able to tell.
            return Ok(());
        }
        self.hand_over(&transfer.instance, &transfer.url, guest)
            .inspect_err(|_| {
                self.machine
                    .lock()
                    .unwrap()
                    .sending
                    .remove(&transfer.instance);
            })
    }

    async fn cancel_send(&self, instance: &str) -> Result<()> {
        let mut m = self.machine.lock().unwrap();
        check(&mut m, Fault::CancelSend, instance)?;
        m.sending.remove(instance);
        m.stalled.remove(instance);
        // The guest stays. Cancelling a pre-copy transfer costs the memory that
        // was copied and nothing else.
        Ok(())
    }
}

#[derive(Default)]
struct Fabric {
    /// Port resource name to tap device — the same thing a real datapath is
    /// asked for, and the only state a datapath has.
    taps: BTreeMap<String, String>,
    /// What each port was last programmed with. Kept so a test can assert on
    /// the allowances that actually reached the datapath: whether a security
    /// group *resolved* correctly is provable in the model, but whether the
    /// result ever arrives here is not, and that is exactly the gap this
    /// feature existed in for as long as it did.
    rules: BTreeMap<String, Vec<ResolvedRule>>,
    faults: BTreeMap<String, String>,
    /// Make tearing this port down fail — *after* the tap has gone, which is
    /// what a real half-finished teardown looks like. The fabric datapath
    /// removes the device first and then talks to the orchestrator, so the
    /// interesting failure is exactly the one where the machine looks clean and
    /// the fabric is still holding an address.
    teardown_faults: BTreeMap<String, String>,
    /// How many times each port was asked to be torn down, so a test can say
    /// "and it asked again" rather than only "it failed".
    unprograms: BTreeMap<String, usize>,
    /// Behave like the fabric datapath: report no rules from `observe`, and
    /// answer `agrees` from what the datapath is actually holding.
    ///
    /// Not a quirk to be tolerated but the shape of the real thing. The fabric
    /// keeps the rules, in its own vocabulary, and the tap layer this observes
    /// through is deliberately programmed with none — so a datapath that could
    /// only be asked "what are your rules" would answer "none" for ever, every
    /// port would read as out of date on every pass, and an instance whose
    /// start is gated on its ports being current would never start.
    holds_rules_elsewhere: bool,
}

/// The fabric, in a process. Same rule: cloning shares the datapath.
#[derive(Clone, Default)]
pub struct FakeDatapath {
    fabric: Arc<Mutex<Fabric>>,
}

impl FakeDatapath {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make programming this port fail, until healed.
    pub fn fail(&self, port: &str, why: &str) {
        self.fabric
            .lock()
            .unwrap()
            .faults
            .insert(port.to_string(), why.to_string());
    }

    pub fn heal(&self, port: &str) {
        self.fabric.lock().unwrap().faults.remove(port);
    }

    pub fn is_programmed(&self, port: &str) -> bool {
        self.fabric.lock().unwrap().taps.contains_key(port)
    }

    /// Make tearing this port down fail after the tap is already gone. See
    /// [`Fabric::teardown_faults`].
    pub fn fail_teardown(&self, port: &str, why: &str) {
        self.fabric
            .lock()
            .unwrap()
            .teardown_faults
            .insert(port.to_string(), why.to_string());
    }

    pub fn heal_teardown(&self, port: &str) {
        self.fabric.lock().unwrap().teardown_faults.remove(port);
    }

    /// How many times this port was asked to be torn down.
    pub fn unprograms(&self, port: &str) -> usize {
        self.fabric
            .lock()
            .unwrap()
            .unprograms
            .get(port)
            .copied()
            .unwrap_or(0)
    }

    /// Make this datapath behave like the fabric one: rules kept somewhere this
    /// process cannot read back, and the "is it current" question answered by
    /// the datapath itself. See [`Fabric::holds_rules_elsewhere`].
    pub fn holding_rules_elsewhere(self) -> Self {
        self.fabric.lock().unwrap().holds_rules_elsewhere = true;
        self
    }

    /// The allowances this port was last programmed with, or `None` if it was
    /// never programmed at all — which is a different thing from being
    /// programmed with none, and a test that could not tell them apart would
    /// pass on a datapath that was never called.
    pub fn rules_programmed(&self, port: &str) -> Option<Vec<ResolvedRule>> {
        self.fabric.lock().unwrap().rules.get(port).cloned()
    }
}

#[async_trait]
impl Datapath for FakeDatapath {
    async fn observe(&self) -> Result<BTreeMap<String, ProgrammedPort>> {
        let f = self.fabric.lock().unwrap();
        let hidden = f.holds_rules_elsewhere;
        Ok(f.taps
            .iter()
            .map(|(port, tap)| {
                (
                    port.clone(),
                    ProgrammedPort {
                        tap: tap.clone(),
                        rules: if hidden {
                            Vec::new()
                        } else {
                            f.rules.get(port).cloned().unwrap_or_default()
                        },
                    },
                )
            })
            .collect())
    }

    fn agrees(&self, port: &str, have: &ProgrammedPort, want: &[ResolvedRule]) -> bool {
        let f = self.fabric.lock().unwrap();
        if !f.holds_rules_elsewhere {
            return have.rules == want;
        }
        f.rules.get(port).map(Vec::as_slice).unwrap_or_default() == want
    }

    async fn program(
        &self,
        port: &str,
        _spec: &PortSpec,
        _network: &NetworkSpec,
        rules: &[ResolvedRule],
    ) -> Result<String> {
        let mut f = self.fabric.lock().unwrap();
        if let Some(why) = f.faults.get(port) {
            return Err(HostError::failed(why));
        }
        let tap = format!("vt-{}", port.rsplit('/').next().unwrap_or(port));
        f.taps.insert(port.to_string(), tap.clone());
        f.rules.insert(port.to_string(), rules.to_vec());
        Ok(tap)
    }

    async fn unprogram(&self, port: &str) -> Result<()> {
        let mut f = self.fabric.lock().unwrap();
        *f.unprograms.entry(port.to_string()).or_default() += 1;
        f.taps.remove(port);
        f.rules.remove(port);
        // After the tap, deliberately: a real teardown removes the device first
        // and then the fabric's port, so the failure worth modelling is the one
        // that leaves nothing on the machine and everything on the fabric.
        if let Some(why) = f.teardown_faults.get(port) {
            return Err(HostError::failed(why));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> VmRequest {
        VmRequest {
            devices: Vec::new(),
            instance: "projects/p1/instances/i1".into(),
            vcpus: 2,
            memory_mib: 2048,
            image: "sha256:abc".into(),
            root_disk_gib: 10,
            nics: vec![],
            cpu_baseline: None,
        }
    }

    #[tokio::test]
    async fn a_guest_will_not_start_without_its_image_and_disk() {
        let vmm = FakeVmm::new();
        assert!(vmm.start(&request()).await.is_err());
        vmm.pull_image("sha256:abc", "sha256:abc", "http://images.invalid/x")
            .await
            .unwrap();
        assert!(
            vmm.start(&request()).await.is_err(),
            "started without a disk"
        );
        vmm.create_disk(
            "projects/p1/instances/i1",
            10,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        vmm.start(&request()).await.unwrap();
        assert!(vmm.is_running("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn a_second_handle_is_the_same_machine() {
        // The property the restart tests rest on: another handle sees the same
        // guests, because a node has one machine and not one per process.
        let vmm = FakeVmm::new();
        vmm.pull_image("sha256:abc", "sha256:abc", "http://images.invalid/x")
            .await
            .unwrap();
        vmm.create_disk(
            "projects/p1/instances/i1",
            10,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        vmm.start(&request()).await.unwrap();

        let other = vmm.clone();
        drop(vmm);
        assert!(other.is_running("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn a_crashed_guest_is_visible_as_failed_rather_than_absent() {
        let vmm = FakeVmm::new();
        vmm.pull_image("sha256:abc", "sha256:abc", "http://images.invalid/x")
            .await
            .unwrap();
        vmm.create_disk(
            "projects/p1/instances/i1",
            10,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        vmm.start(&request()).await.unwrap();
        vmm.crash("projects/p1/instances/i1");

        let host = vmm.observe().await.unwrap();
        let vm = &host.vms["projects/p1/instances/i1"];
        assert_eq!(vm.state, InstanceState::Failed);
        assert!(vm.pid.is_none(), "a crashed VMM still had a process");
    }

    #[tokio::test]
    async fn an_injected_fault_is_what_the_operator_will_read() {
        let vmm = FakeVmm::new();
        vmm.cache_image("sha256:abc");
        vmm.create_disk(
            "projects/p1/instances/i1",
            10,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        vmm.fail(
            Fault::Start,
            "projects/p1/instances/i1",
            "no hugepages left",
        );
        let err = vmm.start(&request()).await.unwrap_err();
        assert!(err.to_string().contains("hugepages"));
        vmm.heal(Fault::Start, "projects/p1/instances/i1");
        vmm.start(&request()).await.unwrap();
    }

    /// A machine with the guest's image and disk already on it, ready to
    /// receive.
    async fn ready(network: &FakeNetwork, id: &str) -> FakeVmm {
        let vmm = network.host(id);
        vmm.cache_image("sha256:abc");
        vmm.create_disk(
            "projects/p1/instances/i1",
            10,
            "projects/p1/images/sha256-abc",
            velstra_cloud_model::resources::ImageFormat::Raw,
        )
        .await
        .unwrap();
        vmm
    }

    #[tokio::test]
    async fn a_guest_moves_to_whatever_is_listening_at_the_url_and_stays_the_same_guest() {
        let wire = FakeNetwork::new();
        let source = ready(&wire, "node-a").await;
        let destination = ready(&wire, "node-b").await;
        source.start(&request()).await.unwrap();
        let uptime = source.started_at("projects/p1/instances/i1");

        let url = destination
            .prepare_receiver(&request(), MigrationMode::Live)
            .await
            .unwrap();
        assert!(url.contains("node-b"), "{url}");
        source
            .send(&Transfer {
                instance: "projects/p1/instances/i1".into(),
                url: url.clone(),
                mode: MigrationMode::Live,
                downtime_ms: 300,
                timeout_s: 3600,
                connections: 1,
            })
            .await
            .unwrap();

        assert!(!source.is_running("projects/p1/instances/i1"));
        assert!(destination.is_running("projects/p1/instances/i1"));
        // The same guest, not a new one: a migration a tenant can see in their
        // own `uptime` is a migration that was not live.
        assert_eq!(destination.started_at("projects/p1/instances/i1"), uptime);
        // And nothing is listening at that URL any more — the receiver was what
        // took delivery, and having taken it, it is a running VMM.
        assert!(!destination.is_receiving("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn a_transfer_that_fails_leaves_the_guest_where_it_was_and_says_what_it_copied() {
        // Pre-copy's whole point, in the one place every test of it rests on.
        let wire = FakeNetwork::new();
        let source = ready(&wire, "node-a").await;
        let destination = ready(&wire, "node-b").await;
        source.start(&request()).await.unwrap();
        let url = destination
            .prepare_receiver(&request(), MigrationMode::Live)
            .await
            .unwrap();
        source.fail(
            Fault::Send,
            "projects/p1/instances/i1",
            "the far end stopped answering",
        );

        let err = source
            .send(&Transfer {
                instance: "projects/p1/instances/i1".into(),
                url,
                mode: MigrationMode::Live,
                downtime_ms: 300,
                timeout_s: 3600,
                connections: 1,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("stopped answering"));
        assert!(
            source.is_running("projects/p1/instances/i1"),
            "a failed transfer took the guest with it"
        );
        assert!(!source.is_sending("projects/p1/instances/i1"));
        assert!(!destination.is_running("projects/p1/instances/i1"));
        // It got some of the way, and the destination can say how far.
        assert!(destination.received_mib("projects/p1/instances/i1") > 0);
        assert!(destination.is_receiving("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn a_receiver_is_asked_for_twice_and_answers_with_the_one_it_has() {
        let wire = FakeNetwork::new();
        let destination = ready(&wire, "node-b").await;
        let first = destination
            .prepare_receiver(&request(), MigrationMode::Live)
            .await
            .unwrap();
        let again = destination
            .prepare_receiver(&request(), MigrationMode::Live)
            .await
            .unwrap();
        assert_eq!(first, again, "a second receiver bound a second port");

        // A receiver that cannot bind is a failure with a sentence on it, and
        // the machine is left with nothing listening rather than with something
        // half-started.
        let other = wire.host("node-c");
        other.fail(
            Fault::PrepareReceiver,
            "projects/p1/instances/i1",
            "no hugepages left",
        );
        assert!(
            other
                .prepare_receiver(&request(), MigrationMode::Live)
                .await
                .is_err()
        );
        assert!(!other.is_receiving("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn sending_to_a_node_that_is_gone_fails_and_keeps_the_guest() {
        let wire = FakeNetwork::new();
        let source = ready(&wire, "node-a").await;
        let url = {
            let destination = ready(&wire, "node-b").await;
            destination
                .prepare_receiver(&request(), MigrationMode::Live)
                .await
                .unwrap()
            // …and the whole machine goes away, the way a node does.
        };
        source.start(&request()).await.unwrap();

        let err = source
            .send(&Transfer {
                instance: "projects/p1/instances/i1".into(),
                url,
                mode: MigrationMode::Live,
                downtime_ms: 300,
                timeout_s: 3600,
                connections: 1,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("nothing is listening"), "{err}");
        assert!(source.is_running("projects/p1/instances/i1"));
        assert!(!source.is_sending("projects/p1/instances/i1"));
    }

    #[tokio::test]
    async fn reopening_a_volume_returns_the_device_it_already_has() {
        // Idempotence where it is dangerous to get wrong: a second device for
        // one volume is a guest with two views of the same bytes.
        let vmm = FakeVmm::new();
        let first = vmm
            .open_volume("i1", "projects/p1/volumes/v1", false)
            .await
            .unwrap();
        let again = vmm
            .open_volume("i1", "projects/p1/volumes/v1", false)
            .await
            .unwrap();
        assert_eq!(first, again);
    }
}
