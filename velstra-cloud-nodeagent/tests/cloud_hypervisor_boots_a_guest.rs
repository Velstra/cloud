//! The other backend, actually started, on this machine.
//!
//! Its sibling `qemu_boots_a_guest.rs` proves a stock cloud image boots through
//! QEMU's own firmware. This one cannot use the same image, and that difference
//! is the finding: **the two backends do not accept the same guests.**
//!
//! Cloud Hypervisor has no firmware of its own. `--kernel` takes a Linux kernel
//! or a PVH firmware blob and nothing else — OVMF is refused outright ("Invalid
//! bzImage"), and `rust-hypervisor-firmware` boots UEFI/GPT images only, so a
//! BIOS-partitioned cloud image panics with "Unable to boot from any virtio-blk
//! device". QEMU's SeaBIOS takes either.
//!
//! So this exercises the other arm of [`Boot`]: a kernel and an initramfs the
//! host supplies, with the command line that a directly-booted kernel has no
//! bootloader to write for it. That arm is Cloud Hypervisor's native mode and
//! the one a platform that ships its own kernels would use.
//!
//! Skipped rather than failed when the machine cannot run it. A test that goes
//! red for want of a fixture teaches people to ignore red.

use std::{path::PathBuf, time::Duration};

use velstra_cloud_model::resources::InstanceState;
use velstra_cloud_nodeagent::{Boot, CloudHypervisorVmm, Layout, Scope, VmRequest, Vmm};

/// A kernel and an initramfs that boot to a shell without any root device.
///
/// Alpine's netboot pair, fetched by hand: a test that reaches the network
/// fails for reasons that have nothing to do with the code.
fn kernel_and_initramfs() -> Option<(PathBuf, PathBuf)> {
    let dir =
        PathBuf::from(std::env::var("VELSTRA_TEST_BOOT").unwrap_or_else(|_| "/tmp/vq/boot".into()));
    let kernel = dir.join("vmlinuz-virt");
    let initramfs = dir.join("initramfs-virt");
    (kernel.exists() && initramfs.exists()).then_some((kernel, initramfs))
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

fn can_run() -> Option<(PathBuf, PathBuf)> {
    if !PathBuf::from("/dev/kvm").exists() {
        eprintln!("skipping: no /dev/kvm");
        return None;
    }
    for tool in ["cloud-hypervisor", "systemd-run"] {
        if which(tool).is_none() {
            eprintln!("skipping: no {tool}");
            return None;
        }
    }
    let Some(pair) = kernel_and_initramfs() else {
        eprintln!("skipping: no kernel/initramfs (set VELSTRA_TEST_BOOT)");
        return None;
    };
    Some(pair)
}

#[tokio::test]
async fn a_directly_booted_kernel_runs_and_says_so() {
    let Some((kernel, initramfs)) = can_run() else {
        return;
    };

    // Short, because a unix socket path is capped at 108 bytes and this
    // repository is usually tested from somewhere longer than that on its own.
    let run_dir = PathBuf::from(format!("/tmp/vq-ch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).expect("a run directory");

    let layout = Layout {
        run_dir: run_dir.join("instances"),
        image_dir: run_dir.join("images"),
        incoming_dir: run_dir.join("images/incoming"),
        binary: "cloud-hypervisor".into(),
        // The arm QEMU's test does not cover: the host supplies the kernel, and
        // with it the command line, because there is no bootloader to write one.
        // `console=ttyS0` is not decoration — without it the kernel boots
        // perfectly and says nothing, which is indistinguishable from not
        // booting at all.
        boot: Boot::Kernel {
            kernel,
            cmdline: "console=ttyS0".into(),
            initrd: Some(initramfs),
        },
        scope: Scope::User,
        slice: "velstra-test.slice".into(),
        ..Layout::default()
    };
    // A guest outlives a killed test: an earlier run of this file was stopped
    // by a timeout before it reached `delete`, and its VMM was still running an
    // hour later — holding the unit name, so the next run could not start at
    // all. The reset belongs here and not at the end, because the end is
    // exactly what does not run when a test is interrupted.
    let _ = tokio::process::Command::new("systemctl")
        .args([
            "--user",
            "stop",
            &format!(
                "velstra-vm-{}.service",
                "projects/p1/instances/ch-1".replace('/', "_")
            ),
        ])
        .output()
        .await;

    let vmm = CloudHypervisorVmm::new(layout.clone());

    let instance = "projects/p1/instances/ch-1";
    let request = VmRequest {
        instance: instance.to_string(),
        vcpus: 1,
        memory_mib: 1024,
        image: "projects/p1/images/netboot".into(),
        root_disk_gib: 1,
        nics: vec![],
    };

    // A disk is still made and attached: nothing boots from it here, but the
    // backend's normal path includes one and a test that skipped it would be
    // testing a shape nobody runs.
    vmm.create_disk(instance, request.root_disk_gib, &request.image)
        .await
        .expect("a root disk");
    vmm.start(&request).await.expect("cloud-hypervisor starts");

    let mut running = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let host = vmm.observe().await.expect("the machine can be read");
        if host.vms.get(instance).map(|vm| vm.state) == Some(InstanceState::Running) {
            running = true;
            break;
        }
    }
    assert!(running, "the VMM never reported the guest as running");

    // Running is not booted. A VMM that started and loaded nothing reports
    // exactly the same state, which is why the console is what this asserts on.
    let console = layout.console(instance);
    let mut said = String::new();
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        said = std::fs::read_to_string(&console).unwrap_or_default();
        if said.contains("Linux version") || said.contains("initramfs") {
            break;
        }
    }
    assert!(
        said.contains("Linux version") || said.contains("initramfs"),
        "nothing booted. The console held {} bytes:\n{}",
        said.len(),
        said.chars().take(2000).collect::<String>()
    );
    eprintln!(
        "the guest said ({} bytes of console):\n  {}",
        said.len(),
        said.lines().next().unwrap_or_default()
    );

    vmm.delete(instance).await.expect("the guest goes away");
    let after = vmm.observe().await.expect("the machine can be read");
    assert!(
        !after.vms.contains_key(instance),
        "the guest is still here after being deleted"
    );
    let _ = std::fs::remove_dir_all(&run_dir);
}
