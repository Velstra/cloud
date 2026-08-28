//! A real guest, started by the real backend, on this machine.
//!
//! Everything else about the QEMU backend is tested against bytes: the command
//! line it builds, the QMP framing, the run-state mapping. None of that can
//! answer the only question that matters — does a guest boot — and until this
//! test ran, nothing had. The first attempt found two reasons it could not:
//!
//! * a firmware blob was being passed to `-kernel`, which takes a Linux kernel
//!   and nothing else, so the guest booted nothing;
//! * and no console was attached, so it said nothing about it either. The
//!   evidence that a guest had failed to boot was an empty file that did not
//!   exist.
//!
//! Skipped rather than failed when the machine cannot run it (no QEMU, no
//! `/dev/kvm`, no user systemd, no image). A test that fails for want of a
//! fixture teaches people to ignore red.

use std::{path::PathBuf, time::Duration};

use velstra_cloud_model::resources::InstanceState;
use velstra_cloud_nodeagent::{Boot, Layout, QemuVmm, Scope, VmRequest, Vmm};

/// A raw disk image with a bootable guest on it, if this machine has one.
///
/// Fetched by hand rather than downloaded here: a test that reaches the network
/// fails for reasons that have nothing to do with the code.
fn image() -> Option<PathBuf> {
    let path = PathBuf::from(
        std::env::var("VELSTRA_TEST_IMAGE").unwrap_or_else(|_| "/tmp/vq/alpine.raw".into()),
    );
    path.exists().then_some(path)
}

fn can_run() -> Option<PathBuf> {
    if !PathBuf::from("/dev/kvm").exists() {
        eprintln!("skipping: no /dev/kvm");
        return None;
    }
    if which("qemu-system-x86_64").is_none() {
        eprintln!("skipping: no qemu-system-x86_64");
        return None;
    }
    if which("systemd-run").is_none() {
        eprintln!("skipping: no systemd-run");
        return None;
    }
    let Some(image) = image() else {
        eprintln!("skipping: no guest image (set VELSTRA_TEST_IMAGE)");
        return None;
    };
    Some(image)
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[tokio::test]
async fn a_stock_cloud_image_boots_and_says_so() {
    let Some(image) = can_run() else { return };

    // A short run directory on purpose: a unix socket path is capped at 108
    // bytes, and the scratch directories this repository is usually tested from
    // are longer than that on their own. QEMU's refusal is clear, but only if
    // somebody reads it.
    let run_dir = PathBuf::from(format!("/tmp/vq-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).expect("a run directory");

    let layout = Layout {
        run_dir: run_dir.join("instances"),
        // Under the test's own directory, like everything else it touches: the
        // defaults live in /var/lib, which a rootless node cannot write.
        image_dir: run_dir.join("images"),
        incoming_dir: run_dir.join("images/incoming"),
        binary: "qemu-system-x86_64".into(),
        // The guest's own bootloader. This is what every stock cloud image
        // expects, and what the platform got wrong.
        boot: Boot::Firmware(None),
        // No root on the machine this is written for, and a data path that can
        // only be exercised as root is one almost nobody exercises.
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
                "projects/p1/instances/boot-1".replace('/', "_")
            ),
        ])
        .output()
        .await;

    let vmm = QemuVmm::new(layout.clone());

    let instance = "projects/p1/instances/boot-1";
    let request = VmRequest {
        devices: Vec::new(),
        instance: instance.to_string(),
        vcpus: 1,
        memory_mib: 512,
        image: "projects/p1/images/alpine".into(),
        root_disk_gib: 1,
        nics: vec![],
        cpu_baseline: None,
    };

    // The image, published the way a pulled one is: under its slug in the
    // image directory. From here on the platform does the work — the test does
    // not put an operating system anywhere the guest can reach it.
    std::fs::create_dir_all(&layout.image_dir).expect("an image directory");
    std::fs::copy(
        &image,
        layout.image_dir.join(request.image.replace('/', "~")),
    )
    .expect("the published image");

    // `create_disk` makes the guest's root disk. It used to make an *empty*
    // file: the image was pulled, verified, and then never used, so every
    // instance this platform ever started booted a blank disk. On a direct
    // kernel boot that looks exactly like a guest that is merely slow.
    vmm.create_disk(
        instance,
        request.root_disk_gib,
        &request.image,
        velstra_cloud_model::resources::ImageFormat::Raw,
    )
    .await
    .expect("the root disk is made from the image");
    assert!(
        std::fs::metadata(layout.disk(instance))
            .expect("a disk")
            .len()
            >= std::fs::metadata(&image).expect("the image").len(),
        "the disk is smaller than the image it is supposed to be a copy of"
    );

    vmm.start(&request).await.expect("qemu starts");

    // Running, as this node sees it — asked of the machine, not remembered.
    let mut running = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let host = vmm.observe().await.expect("the machine can be read");
        if host.vms.get(instance).map(|vm| vm.state) == Some(InstanceState::Running) {
            running = true;
            break;
        }
    }
    assert!(running, "the VMM never reported the guest as running");

    // …and the guest is really *in* there, which the run state alone does not
    // say: a QEMU that booted nothing is every bit as "running" as one that
    // booted Linux. This is the assertion the whole file exists for.
    let console = layout.console(instance);
    let mut booted = String::new();
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        booted = std::fs::read_to_string(&console).unwrap_or_default();
        if booted.contains("Linux version") {
            break;
        }
    }
    assert!(
        booted.contains("Linux version"),
        "nothing booted. The console held {} bytes:\n{}",
        booted.len(),
        booted.chars().take(2000).collect::<String>()
    );
    // Printed on success too. A test that only speaks when it fails leaves
    // "passed in two seconds" looking exactly like "skipped".
    eprintln!(
        "the guest said ({} bytes of console):\n  {}",
        booted.len(),
        booted
            .lines()
            .find(|l| l.contains("Linux version"))
            .unwrap_or_default()
    );

    vmm.delete(instance).await.expect("the guest goes away");
    let after = vmm.observe().await.expect("the machine can be read");
    assert!(
        !after.vms.contains_key(instance),
        "the guest is still here after being deleted"
    );
    let _ = std::fs::remove_dir_all(&run_dir);
}

/// A qcow2 image becomes a raw disk, rather than a raw-shaped qcow2.
///
/// The disk is handed to the VMM as `format=raw`, and this used to be a byte
/// copy whatever the image was — so a qcow2 image produced a root disk starting
/// with `QFI\xfb` where a boot sector belongs. The guest started, found no
/// bootloader, and sat with an empty console. Nothing failed anywhere: the disk
/// was made, the VMM ran, and the platform reported a guest that could never
/// boot. `ImageFormat` existed for this and was read by nobody, which is why
/// every public cloud image — they are all qcow2 — was unbootable.
#[tokio::test]
async fn a_qcow2_image_is_converted_and_not_copied() {
    use velstra_cloud_model::resources::ImageFormat;

    let Some(qemu_img) = which("qemu-img") else {
        println!("skipping: qemu-img is not on PATH");
        return;
    };
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("qcow2-convert");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let src = base.join("src.qcow2");
    assert!(
        std::process::Command::new(&qemu_img)
            .args(["create", "-f", "qcow2"])
            .arg(&src)
            .arg("64M")
            .status()
            .unwrap()
            .success()
    );
    // What the bug looked like from the outside: the source really is qcow2.
    assert_eq!(&std::fs::read(&src).unwrap()[..4], b"QFI\xfb");

    let layout = Layout {
        run_dir: base.join("instances"),
        image_dir: base.join("images"),
        incoming_dir: base.join("images/incoming"),
        ..Layout::default()
    };
    velstra_cloud_nodeagent::hostfs::create_disk(&layout, "g1", 1, Some(&src), ImageFormat::Qcow2)
        .await
        .expect("the disk was not made");

    let disk = std::fs::read(layout.disk("g1")).unwrap();
    assert_ne!(
        &disk[..4],
        b"QFI\xfb",
        "the root disk is still a qcow2 wearing a .raw name"
    );

    // And a declaration that disagrees with the bytes is refused rather than
    // guessed at: the digest proves which bytes these are, never what they are.
    let wrong = velstra_cloud_nodeagent::hostfs::create_disk(
        &layout,
        "g2",
        1,
        Some(&src),
        ImageFormat::Raw,
    )
    .await;
    let why = wrong
        .expect_err("a mis-declared image was accepted")
        .to_string();
    assert!(why.contains("first bytes say otherwise"), "{why}");
}
