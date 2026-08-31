//! The conformance suite against a real etcd, plus the two things only a real
//! one can be asked: does the state outlive the process, and can two of them
//! see it.
//!
//! Every case starts its own etcd on a free port in a temporary directory and
//! kills it afterwards, including when the case fails. When the `etcd` binary
//! is not installed the cases skip rather than fail — a suite that goes red for
//! want of a fixture teaches people to ignore red, and then it stops catching
//! anything at all.

mod conformance;

use std::{
    io,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use velstra_cloud_store::{Expect, Store, etcd::EtcdStore, key_for};

/// How many etcd servers this binary will have alive at once.
fn concurrent_servers() -> std::sync::Arc<tokio::sync::Semaphore> {
    static LIMIT: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    LIMIT
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
}

/// A single-node etcd, owned by one test.
struct Etcd {
    /// Held for as long as this server is alive. See [`Etcd::start`].
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    child: Child,
    dir: PathBuf,
    /// What this fixture's server calls itself, so the fixture can tell its own
    /// server from somebody else's. See [`Etcd::serving`].
    name: String,
    client_port: u16,
    peer_port: u16,
}

impl Etcd {
    /// Start one and wait until it is serving, or return `None` if etcd is not
    /// installed here.
    ///
    /// A free port is only free until something takes it, and with cases
    /// running in parallel two of them can choose the same one. The loser's
    /// server exits, so the retry below is what turns that from a once-a-run
    /// mystery failure into a second attempt nobody notices.
    async fn start() -> Option<Self> {
        // At most this many etcd servers alive at once, across the whole binary.
        //
        // Every case starts its own, and there are twenty-one of them: run flat
        // out they are twenty-one single-node clusters competing for ports and
        // CPU with each other and with the rest of the workspace, and requests
        // that would take milliseconds start timing out. That surfaced as three
        // different cases failing in one run and none of them in the next, which
        // is the kind of red people learn to re-run rather than read.
        //
        // A permit rather than `--test-threads=1`, because the isolation these
        // cases want is one server per case, not one case at a time — and the
        // cost of the bound is a few seconds, once.
        let _permit = concurrent_servers().acquire_owned().await.ok()?;
        // Twenty attempts with a pause, not five in a tight loop. `free_port`
        // binds a port, closes it, and hopes — so under a whole suite starting an
        // etcd per test, two fixtures are handed the same number and one of them
        // loses. The race is in the design and cannot be closed here (etcd has to
        // bind the port itself), so the answer is patience: the loser tries a
        // different number a moment later.
        for attempt in 0..20 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let n = {
                static NEXT: AtomicU32 = AtomicU32::new(0);
                NEXT.fetch_add(1, Ordering::Relaxed)
            };
            let name = format!("velstra-{}-{n}", std::process::id());
            let dir = std::env::temp_dir().join(&name);
            let _ = std::fs::remove_dir_all(&dir);
            let (client_port, peer_port) = (free_port(), free_port());
            let child = spawn(&name, &dir, client_port, peer_port)?;
            let mut etcd = Self {
                permit: None,
                child,
                dir,
                name,
                client_port,
                peer_port,
            };
            if etcd.serving().await {
                etcd.permit = Some(_permit);
                return Some(etcd);
            }
            // Dropping it kills what is left and takes the directory with it.
        }
        panic!(
            "etcd would not stay on a port this test had to itself, after 20 attempts — \
             the machine is either out of ports or very busy"
        );
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.client_port)
    }

    /// Wait until *this* server answers on its port.
    ///
    /// Asking whether *a* server answers would not do: a fixture that lost a
    /// port race would find the winner's etcd there, pass its own tests against
    /// somebody else's data, and fail whichever case happened to notice.
    async fn serving(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            // A server that has exited did not get the port, and nothing on
            // that port is ours.
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false;
            }
            if let Ok(mut client) = etcd_client::Client::connect([self.endpoint()], None).await
                && let Ok(members) = client.member_list().await
                && members.members().iter().any(|m| m.name() == self.name)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// A client of this cluster. A fresh one each time, so a test can hold two
    /// that share nothing but the cluster.
    async fn store(&self) -> EtcdStore {
        EtcdStore::connect([self.endpoint()])
            .await
            .expect("the fixture waited for this server before handing it out")
    }

    /// Kill the server and start it again on the same port and data directory —
    /// the only honest way to ask whether anything was actually written down.
    async fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Retried, because the ordinary reason a replacement exits at once is
        // that the port it wants is not free yet — the old process is gone but
        // its socket has not been reclaimed. Treating that first exit as final
        // is what made a whole-workspace run fail at random while the same test
        // passed every time it was run on its own.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.child = spawn(&self.name, &self.dir, self.client_port, self.peer_port)
                .expect("etcd was here a moment ago");
            if self.serving().await {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "etcd did not come back within 30s"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for Etcd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn spawn(name: &str, dir: &Path, client_port: u16, peer_port: u16) -> Option<Child> {
    let client = format!("http://127.0.0.1:{client_port}");
    let peer = format!("http://127.0.0.1:{peer_port}");
    let result = Command::new("etcd")
        .args([
            &format!("--name={name}"),
            &format!("--data-dir={}", dir.display()),
            &format!("--listen-client-urls={client}"),
            &format!("--advertise-client-urls={client}"),
            &format!("--listen-peer-urls={peer}"),
            &format!("--initial-advertise-peer-urls={peer}"),
            &format!("--initial-cluster={name}={peer}"),
            "--log-level=error",
            // About test speed, not a stance on durability: these cases ask
            // whether etcd wrote the data down, which the page cache answers as
            // well as the disk does when the thing being restarted is a process
            // rather than a machine.
            "--unsafe-no-fsync",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(child) => Some(child),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => panic!("etcd is installed but would not start: {e}"),
    }
}

/// A port nobody is using — as close to a reservation as this gets. The listener
/// is closed before etcd binds it, so this races with anything else on the
/// machine doing the same thing, and every other way of choosing a port races
/// worse.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("no loopback")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

/// Start etcd, or say why the test is not running and leave.
///
/// The skip is deliberate — a suite that goes red because a machine lacks a
/// binary teaches people to ignore red. But a *silent* skip is worse than
/// either: with etcd off the PATH this file reported twenty passing tests in
/// 0.00 seconds, which is indistinguishable from twenty that ran. Nothing in
/// the output said the backend had not been exercised at all.
///
/// So the skip is recorded, and [`the_etcd_backend_was_actually_exercised`]
/// turns a skipped suite into one loud failure rather than twenty quiet
/// successes. Set `VELSTRA_ETCD_OPTIONAL=1` to say, deliberately, that a green
/// run without etcd is acceptable here.
static SKIPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

macro_rules! etcd_or_skip {
    () => {
        match Etcd::start().await {
            Some(etcd) => etcd,
            None => {
                SKIPPED.store(true, Ordering::Relaxed);
                eprintln!("skipped: etcd is not on PATH");
                return;
            }
        }
    };
}

/// Fails when the suite skipped everything, unless somebody said that is fine.
///
/// Named so it reads in the output as what it is checking. It runs last only by
/// luck of ordering, which does not matter: any case that skipped has already
/// set the flag by the time this is asked, and if this one runs first it starts
/// its own server and finds the flag clear — which is also the truth.
#[tokio::test]
async fn the_etcd_backend_was_actually_exercised() {
    if Etcd::start().await.is_some() {
        return;
    }
    SKIPPED.store(true, Ordering::Relaxed);
    assert!(
        std::env::var("VELSTRA_ETCD_OPTIONAL").is_ok(),
        "etcd is not on PATH, so every case in this file skipped and the backend \
         was not exercised at all. Twenty green tests that checked nothing look \
         exactly like twenty that checked everything, which is why this one is \
         red. Install etcd, or set VELSTRA_ETCD_OPTIONAL=1 to accept the gap."
    );
}

/// Each case gets its own cell as well as its own server. The cell is what
/// keeps a case's objects out of every other case's `list` and `watch`, which
/// matters more than it looks: it is also what makes the suite meaningful
/// against a shared etcd somebody points it at later.
macro_rules! cases {
    ($($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                let etcd = etcd_or_skip!();
                let store = etcd.store().await;
                conformance::$case(&store, stringify!($case)).await;
            }
        )*
    };
}

cases! {
    a_revision_moves_forward_and_never_repeats,
    an_object_round_trips_through_create_read_update_delete,
    a_stale_writer_is_refused_rather_than_winning,
    creating_twice_is_refused,
    writing_to_something_that_is_gone_is_a_conflict,
    deleting_something_that_is_not_there_says_so,
    a_stale_delete_is_refused,
    a_list_is_prefix_scoped_and_ordered,
    a_list_returns_the_whole_collection_however_big_it_is,
    reading_what_is_not_there_is_not_an_error,
    a_watch_sees_changes_under_its_prefix_and_no_others,
    a_watch_from_a_past_revision_gets_what_it_missed,
    a_delete_tells_watchers_which_key_went,
    a_watcher_that_stops_reading_is_dropped_rather_than_queued,
    a_dropped_watcher_does_not_wedge_the_store,
    asking_where_the_store_is_does_not_move_it,
    paging_walks_the_whole_collection_exactly_once,
    a_collection_that_ends_on_a_page_boundary_says_it_is_done,
    a_page_resumes_strictly_after_the_key_it_is_given,
    a_page_stays_inside_its_prefix,
    paging_an_empty_collection_is_one_empty_page,
}

#[tokio::test]
async fn state_outlives_the_process_that_held_it() {
    // The reason this backend exists. Everything above it is written as if the
    // store remembers; with the memory store that is true only until the
    // process ends, and nothing else in the suite can tell the difference.
    let mut etcd = etcd_or_skip!();
    let key = key_for("cell-durable", "instances", "projects/p1/instances/i1");

    let written = {
        let store = etcd.store().await;
        store
            .put(&key, b"survivor".to_vec(), Expect::Absent)
            .await
            .unwrap()
    };

    etcd.restart().await;

    let store = etcd.store().await;
    let entry = store
        .get(&key)
        .await
        .unwrap()
        .expect("the object did not survive a restart");
    assert_eq!(entry.value, b"survivor");
    assert_eq!(
        entry.revision, written,
        "the object came back at a different revision than it was written at"
    );
    assert!(
        store.revision().await.unwrap() >= written,
        "the store forgot how far it had got"
    );
}

#[tokio::test]
async fn two_handles_see_each_others_writes() {
    // The whole point of the exercise: separate binaries. Two handles here are
    // two clients with nothing in common but the cluster, which is what an API
    // server and a controller in different processes are.
    let etcd = etcd_or_skip!();
    let api = etcd.store().await;
    let controller = etcd.store().await;
    let cell = "cell-shared";
    let key = key_for(cell, "instances", "projects/p1/instances/i1");

    let created = api
        .put(&key, b"spec".to_vec(), Expect::Absent)
        .await
        .unwrap();
    let seen = controller
        .get(&key)
        .await
        .unwrap()
        .expect("the second handle cannot see what the first wrote");
    assert_eq!(seen.revision, created);

    // And the compare-and-swap is a real one across handles, not a per-process
    // lock: the second writer holding the first revision must lose.
    api.put(&key, b"spec-2".to_vec(), Expect::Revision(created))
        .await
        .unwrap();
    let err = controller
        .put(&key, b"spec-3".to_vec(), Expect::Revision(seen.revision))
        .await
        .unwrap_err();
    assert!(
        matches!(err, velstra_cloud_store::StoreError::Conflict { .. }),
        "two processes both believed they had written: {err:?}"
    );
}

#[tokio::test]
async fn a_watcher_in_one_process_sees_another_process_write() {
    let etcd = etcd_or_skip!();
    let controller = etcd.store().await;
    let api = etcd.store().await;
    let cell = "cell-watched";

    let from = controller.revision().await.unwrap();
    let mut rx = controller.watch(
        &velstra_cloud_store::prefix_for(cell, "instances"),
        Some(from),
    );

    let key = key_for(cell, "instances", "projects/p1/instances/i1");
    api.put(&key, b"spec".to_vec(), Expect::Absent)
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a write in another process never reached the watcher")
        .expect("the watch closed");
    assert_eq!(event.key(), key);
}

#[tokio::test]
async fn a_watch_from_a_revision_that_is_gone_ends_instead_of_pretending() {
    // `StoreError::Compacted` is in the error enum, but `watch` hands back a
    // plain receiver with nowhere to put an error, so this is what a caller
    // actually gets when the history it asked for has been thrown away: the
    // channel ends. That is the same recovery `Compacted` would have asked for
    // — re-list — arrived at without a variant the signature has no room for.
    // Worth pinning, because the alternative failure mode is silence, and
    // silence on a watch is indistinguishable from "nothing has changed".
    let etcd = etcd_or_skip!();
    let store = etcd.store().await;
    let cell = "cell-compacted";
    let prefix = velstra_cloud_store::prefix_for(cell, "instances");

    let listed_at = store.revision().await.unwrap();
    for i in 0..4 {
        store
            .put(
                &key_for(cell, "instances", &format!("i{i}")),
                b"spec".to_vec(),
                Expect::Absent,
            )
            .await
            .unwrap();
    }
    let now = store.revision().await.unwrap();

    // Through the trait, which grew `compact` after a live cell filled its
    // quota with history nobody was throwing away. "Nothing above this crate
    // should compact" was the old rule; the incident is why the API now must.
    store.compact(now).await.unwrap();

    let mut rx = store.watch(&prefix, Some(listed_at));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a watch on a compacted revision hung instead of ending")
            .is_none(),
        "a watch on history that no longer exists delivered something anyway"
    );
}

/// Compaction throws history away and keeps every object.
///
/// The case this exists for was found live: a two-day-old cell held 393 kB of
/// objects under two gigabytes of their history, hit etcd's 2 GiB quota, and
/// answered `mvcc: database space exceeded` to a login. Nothing anywhere
/// compacted, so nothing could recover without an operator and etcdctl.
#[tokio::test]
async fn compacting_keeps_the_objects_and_drops_the_history() {
    let etcd = etcd_or_skip!();
    let store = etcd.store().await;
    let cell = "cell-compact";

    // An object with history: written, then rewritten.
    let before = store.revision().await.unwrap();
    let rev1 = store
        .put(&key_for(cell, "instances", "i1"), b"v1".to_vec(), Expect::Absent)
        .await
        .unwrap();
    store
        .put(
            &key_for(cell, "instances", "i1"),
            b"v2".to_vec(),
            Expect::Revision(store.get(&key_for(cell, "instances", "i1")).await.unwrap().unwrap().revision),
        )
        .await
        .unwrap();
    let now = store.revision().await.unwrap();

    store.compact(now).await.unwrap();

    // The object is whole; only the past is gone.
    let read = store.get(&key_for(cell, "instances", "i1")).await.unwrap().unwrap();
    assert_eq!(read.value, b"v2");

    // Compacting a second time to the same point is not an error — another
    // replica or an operator's etcdctl may always have got there first.
    store.compact(now).await.unwrap();
    // Nor is asking for a revision that is itself compacted away.
    store.compact(rev1).await.unwrap();

    // And a watch from before the compaction ends rather than pretending,
    // which is the recovery the API's watch path already performs: re-list.
    // From *before* the first write: `rev1 + 1` is exactly the compaction
    // point and survives, so a watch from `rev1` is valid and rightly stays
    // open — which is what this test first asserted the opposite of.
    let mut watch =
        store.watch(&velstra_cloud_store::prefix_for(cell, "instances"), Some(before));
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(_event) = watch.recv().await {}
    })
    .await;
    assert!(ended.is_ok(), "a watch across the compaction neither ended nor erred");
}
