//! [`DirectoryPool`] against real `qemu-img`.
//!
//! The pool agent's own tests use the fake, and a fake cannot be wrong about
//! qcow2. What is asked here is only what the fake cannot answer: whether the
//! commands are the right commands, whether a size written by one goes back in
//! through the other, and whether a copy is a copy.
//!
//! Skips loudly without `qemu-img` rather than failing. A red test on a machine
//! that simply lacks a tool is a test people learn to scroll past.

use velstra_cloud_nodeagent::{
    directory_pool::DirectoryPool,
    pool::{Origin, Storage},
};

const VOLUME: &str = "projects/p1/volumes/data-1";
const SNAPSHOT: &str = "projects/p1/volumes/data-1/snapshots/s1";

fn have_qemu_img() -> bool {
    std::process::Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! needs_qemu_img {
    () => {
        if !have_qemu_img() {
            eprintln!("skipped: no qemu-img on this machine");
            return;
        }
    };
}

/// A directory that removes itself, named after the test using it so two can
/// run at once.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(what: &str) -> Self {
        let path = std::env::temp_dir().join(format!("velstra-pool-{}-{what}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("images")).unwrap();
        std::fs::create_dir_all(path.join("pool")).unwrap();
        Self(path)
    }
    fn pool(&self) -> DirectoryPool {
        DirectoryPool::new(self.0.join("pool"), self.0.join("images"))
    }
    fn images(&self) -> std::path::PathBuf {
        self.0.join("images")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn a_blank_volume_is_made_and_read_back_at_the_size_that_was_asked_for() {
    needs_qemu_img!();
    let dir = Dir::new("blank");
    let pool = dir.pool();

    pool.provision(VOLUME, 2, Origin::Blank, None)
        .await
        .expect("provisioning a blank volume");

    let seen = pool.observe().await.unwrap();
    assert_eq!(
        seen.volumes.get(VOLUME),
        Some(&2),
        "the pool does not see the volume it just made: {seen:?}"
    );
    assert_eq!(seen.backend, "directory");
    assert!(
        seen.capacity_gib > 0,
        "a pool with no capacity is one the scheduler will never place on"
    );

    pool.destroy(VOLUME).await.unwrap();
    assert!(pool.observe().await.unwrap().volumes.is_empty());
    // Twice, because the pass is level-triggered and asks again every round.
    pool.destroy(VOLUME).await.expect("destroying twice");
}

#[tokio::test]
async fn a_volume_grows_and_never_shrinks() {
    needs_qemu_img!();
    let dir = Dir::new("grow");
    let pool = dir.pool();
    pool.provision(VOLUME, 1, Origin::Blank, None)
        .await
        .unwrap();

    pool.grow(VOLUME, 3).await.unwrap();
    assert_eq!(pool.observe().await.unwrap().volumes.get(VOLUME), Some(&3));

    // Asked to shrink, it does nothing rather than doing it. `qemu-img resize`
    // is one `--shrink` away from destroying a filesystem, so the refusal lives
    // here as well as in the model.
    pool.grow(VOLUME, 1).await.unwrap();
    assert_eq!(
        pool.observe().await.unwrap().volumes.get(VOLUME),
        Some(&3),
        "the volume was shrunk"
    );
}

#[tokio::test]
async fn a_volume_from_an_image_has_the_image_in_it_before_it_exists() {
    needs_qemu_img!();
    let dir = Dir::new("image");
    let pool = dir.pool();

    // An image with something recognisable in it, published the way the node
    // agent publishes one: the resource name, slugged.
    let image = "projects/p1/images/sha256-abc";
    let raw = dir.images().join(image.replace('/', "~"));
    let mut bytes = vec![0u8; 1024 * 1024];
    bytes[..7].copy_from_slice(b"VELSTRA");
    std::fs::write(&raw, &bytes).unwrap();

    pool.provision(VOLUME, 1, Origin::Image(image), None)
        .await
        .expect("provisioning from an image");

    let seen = pool.observe().await.unwrap();
    assert_eq!(seen.volumes.get(VOLUME), Some(&1), "{seen:?}");

    // And the bytes really came across, rather than a blank volume of the right
    // size — the failure this would otherwise hide is a guest that boots nothing.
    let out = std::process::Command::new("qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(
            dir.0
                .join("pool")
                .join(format!("{}.qcow2", VOLUME.replace('/', "~"))),
        )
        .arg(dir.0.join("out.raw"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read(dir.0.join("out.raw")).unwrap();
    assert_eq!(&written[..7], b"VELSTRA", "the image was not copied in");
}

#[tokio::test]
async fn an_image_that_is_not_here_is_refused_rather_than_left_blank() {
    needs_qemu_img!();
    let dir = Dir::new("noimage");
    let pool = dir.pool();
    let err = pool
        .provision(
            VOLUME,
            1,
            Origin::Image("projects/p1/images/sha256-nope"),
            None,
        )
        .await
        .expect_err("a volume was made from an image that is not on this machine");
    assert!(err.to_string().contains("not on this machine"), "{err}");
    assert!(
        pool.observe().await.unwrap().volumes.is_empty(),
        "a volume that could not be filled was left behind empty"
    );
}

#[tokio::test]
async fn a_copy_is_a_copy_and_a_volume_can_be_made_from_it() {
    needs_qemu_img!();
    let dir = Dir::new("snap");
    let pool = dir.pool();
    pool.provision(VOLUME, 2, Origin::Blank, None)
        .await
        .unwrap();

    pool.take_snapshot(SNAPSHOT, VOLUME)
        .await
        .expect("taking a copy");
    let seen = pool.observe().await.unwrap();
    let copy = seen
        .snapshots
        .get(SNAPSHOT)
        .expect("the pool does not see the copy it just made");
    // The source is read back out of the copy's own name, which is where a
    // snapshot's source lives.
    assert_eq!(copy.volume, VOLUME);
    assert_eq!(copy.gib, 2);
    // And it counts against destroying the volume it came from.
    assert_eq!(seen.of(VOLUME).snapshots, 1);

    let restored = "projects/p1/volumes/data-2";
    pool.provision(restored, 2, Origin::Snapshot(SNAPSHOT), None)
        .await
        .expect("provisioning from the copy");
    assert_eq!(
        pool.observe().await.unwrap().volumes.get(restored),
        Some(&2)
    );

    pool.destroy_snapshot(SNAPSHOT).await.unwrap();
    assert!(pool.observe().await.unwrap().snapshots.is_empty());
    // The volume made from it outlives it, because it was copied and not chained.
    assert_eq!(
        pool.observe().await.unwrap().volumes.get(restored),
        Some(&2),
        "deleting the copy took the volume made from it with it"
    );
}

#[tokio::test]
async fn a_copy_of_a_volume_that_is_not_here_is_refused() {
    needs_qemu_img!();
    let dir = Dir::new("nosource");
    let pool = dir.pool();
    let err = pool
        .take_snapshot(SNAPSHOT, VOLUME)
        .await
        .expect_err("a copy was made of a volume that does not exist");
    assert!(err.to_string().contains("nothing to copy"), "{err}");
    assert!(pool.observe().await.unwrap().snapshots.is_empty());
}

#[tokio::test]
async fn a_half_written_file_is_not_reported_as_anything() {
    needs_qemu_img!();
    // What an agent killed mid-copy leaves behind. Reported as a volume it
    // would be one a guest boots and finds truncated; reported as a *snapshot*
    // it would be permanent, because the model never takes one twice.
    let dir = Dir::new("partial");
    let pool = dir.pool();
    pool.observe().await.unwrap(); // makes the snapshots directory
    std::fs::write(
        dir.0
            .join("pool")
            .join(format!("{}.partial", VOLUME.replace('/', "~"))),
        b"half a volume",
    )
    .unwrap();
    std::fs::write(
        dir.0
            .join("pool/snapshots")
            .join(format!("{}.partial", SNAPSHOT.replace('/', "~"))),
        b"half a copy",
    )
    .unwrap();

    let seen = pool.observe().await.unwrap();
    assert!(seen.volumes.is_empty(), "{seen:?}");
    assert!(seen.snapshots.is_empty(), "{seen:?}");
}

#[tokio::test]
async fn a_volume_that_asks_to_be_encrypted_is_refused_rather_than_made_in_plaintext() {
    needs_qemu_img!();
    let dir = Dir::new("crypt");
    let pool = dir.pool();
    let err = pool
        .provision(VOLUME, 1, Origin::Blank, Some("projects/p1/keys/k1"))
        .await
        .expect_err("an encrypted volume was made by a pool with no key");
    assert!(err.to_string().contains("no KMS"), "{err}");
    assert!(
        pool.observe().await.unwrap().volumes.is_empty(),
        "a plaintext volume was left behind under the name of an encrypted one"
    );
}

#[tokio::test]
async fn one_unreadable_file_does_not_take_the_whole_pool_down() {
    // Found live, within minutes of the first disk attach that worked. A guest
    // holding a volume open made `qemu-img info` on that file answer
    //
    //   Failed to get shared "write" lock
    //   Is another process using the image […]?
    //
    // and `observe` returned `Err` for the pool as a whole: "could not read this
    // pool; doing nothing this pass", every thirty seconds, for ever. Every
    // other volume in the pool stopped being provisioned — so attaching a disk
    // was a way to take storage down for the whole cell.
    //
    // Two things were wrong and each alone was enough. The lock is fixed at the
    // call (`qemu-img info -U`); this is the other half: a file this pool cannot
    // measure is one volume's size unknown, not a pool that cannot be read.
    needs_qemu_img!();
    let dir = Dir::new("unreadable");
    let pool = dir.pool();

    pool.provision(VOLUME, 2, Origin::Blank, None)
        .await
        .expect("provisioning a blank volume");
    // A file this process cannot open at all — the same shape, from `observe`'s
    // seat, as one it is refused a lock on. Junk contents would not do: to
    // `qemu-img info` nineteen bytes of text are a perfectly good raw image.
    let kaputt = dir.0.join("pool").join("projects~p1~volumes~kaputt.qcow2");
    std::fs::write(&kaputt, b"this is not a qcow2").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&kaputt, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    if std::fs::File::open(&kaputt).is_ok() {
        // Running as root, where no file is unreadable. The half this test is
        // about cannot be staged, and passing it silently would be a lie.
        eprintln!("skipped: everything is readable as root");
        return;
    }

    let seen = pool
        .observe()
        .await
        .expect("one bad file made the whole pool unreadable");
    assert_eq!(
        seen.volumes.get(VOLUME),
        Some(&2),
        "the volumes this pool can measure were not reported"
    );
    assert!(
        !seen.volumes.contains_key("projects/p1/volumes/kaputt"),
        "a file that could not be measured was reported with a made-up size"
    );
}

#[tokio::test]
async fn a_volume_someone_has_open_is_still_measurable() {
    // The lock half, with a real lock: `qemu-img` itself takes an exclusive one
    // while it writes, and `info` without `-U` asks for a write lock even though
    // it only reads. Two `info` calls at once is enough to show it.
    needs_qemu_img!();
    let dir = Dir::new("inuse");
    let pool = dir.pool();
    pool.provision(VOLUME, 1, Origin::Blank, None)
        .await
        .unwrap();

    let path = dir.0.join("pool").join("projects~p1~volumes~data-1.qcow2");
    // Hold an exclusive lock the way a running guest does.
    let held = std::process::Command::new("qemu-img")
        .args(["bench", "-c", "1000000", "-w", path.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut held) = held else {
        eprintln!("skipped: qemu-img bench unavailable");
        return;
    };
    std::thread::sleep(std::time::Duration::from_millis(400));

    let seen = pool.observe().await;
    let _ = held.kill();
    let _ = held.wait();

    let seen = seen.expect("a volume in use made the whole pool unreadable");
    assert_eq!(
        seen.volumes.get(VOLUME),
        Some(&1),
        "a volume somebody has open was not measured"
    );
}
