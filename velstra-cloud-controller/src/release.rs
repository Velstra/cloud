//! Reading a channel, and bringing a release onto this cell.
//!
//! A release names a channel — a directory somewhere holding the artefacts
//! CI publishes and the `SHA256SUMS` that names them — and this controller
//! turns that into bytes on the control plane's disk: it reads the sums,
//! writes what the channel offers onto the status, fetches each artefact into
//! the releases directory, and verifies it against its digest before it is
//! ever marked `fetched`. Only then is the release `Ready`, and only then does
//! a rollout hand a node a file or an install medium get cut from one.
//!
//! **One artefact per pass.** An image is a gigabyte; fetching all three in
//! one reconcile would be a status that said "reading the channel" for ten
//! minutes. Each fetch is announced on the status before it starts and marked
//! when it verifies, and the write of that mark is what wakes the next pass —
//! so the console shows *fetching the image (2 of 3)* while it happens, and a
//! controller restart in the middle costs one file, not the release.
//!
//! **Fail closed.** A digest that does not match is a refusal on the status
//! with both digests in it, and the file is removed — a file that hashed wrong
//! is not kept for a later pass to trust. A channel that cannot be read says
//! so; one that carries two builds is refused rather than guessed at. Nothing
//! here decides what a build *is*: that is `velstra_cloud_model::release`,
//! tested without a network.
//!
//! **`file://` is verified in place.** A directory an operator carried onto the
//! control plane is not copied — the hash is taken where it lies and the
//! releases directory gets a link. Copying two gigabytes to prove they are the
//! same two gigabytes would be the one step of this that could fill the disk.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};

use tracing::{info, warn};
use velstra_cloud_model::{
    meta::{Condition, ConditionStatus, Timestamp, set_condition},
    release::{self, Artefact, READY, ReleaseSpec, ReleaseStatus},
    resources::Resource,
};

use crate::{Result, runner::Reconciler, status::StatusWriter};

/// How the controller reaches a channel. Mocked in tests; [`OverHttp`] in
/// production, which also reads `file://`.
pub trait Fetch: Send + Sync + 'static {
    /// A small text file, whole.
    fn text(&self, url: &str) -> impl Future<Output = std::result::Result<String, String>> + Send;
    /// A large file, streamed to `dest`. `dest` is a private name; the caller
    /// renames it into place once the bytes have been verified.
    fn download(
        &self,
        url: &str,
        dest: &Path,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send;
}

pub struct ReleaseController<F: Fetch> {
    status: StatusWriter<ReleaseSpec, ReleaseStatus>,
    fetch: Arc<F>,
    /// Where releases live: `<dir>/<release id>/<file>`.
    dir: PathBuf,
}

impl<F: Fetch> ReleaseController<F> {
    pub fn new(
        status: StatusWriter<ReleaseSpec, ReleaseStatus>,
        fetch: Arc<F>,
        dir: PathBuf,
    ) -> Self {
        Self { status, fetch, dir }
    }

    /// The channel's sums, read and written onto the status: which build, and
    /// which files. A file whose name and digest did not change keeps its
    /// `fetched` mark — a re-read of the same channel is not a re-fetch.
    async fn read_channel(
        &self,
        release: &Resource<ReleaseSpec, ReleaseStatus>,
        next: &mut Resource<ReleaseSpec, ReleaseStatus>,
    ) -> bool {
        let url = release::file_url(&release.spec.url, release::SUMS);
        let text = match self.fetch.text(&url).await {
            Ok(text) => text,
            Err(why) => {
                not_ready(next, "Unreachable", &format!("could not read {url}: {why}"));
                return false;
            }
        };
        let offered = match release::offered(&release::sums(&text)) {
            Ok(offered) => offered,
            Err(why) => {
                not_ready(next, "Unusable", &format!("{url}: {why}"));
                return false;
            }
        };
        let keep = |old: &Option<Artefact>, new: Option<Artefact>| {
            new.map(|mut a| {
                if let Some(o) = old
                    && o.file == a.file
                    && o.sha256 == a.sha256
                {
                    a.fetched = o.fetched;
                }
                a
            })
        };
        next.status.version = offered.version;
        next.status.image = keep(&release.status.image, offered.image);
        next.status.package = keep(&release.status.package, offered.package);
        next.status.installer = keep(&release.status.installer, offered.installer);
        next.status.checked_at = Timestamp::now();
        true
    }

    /// One artefact, brought here and verified. `Ok(())` means the file under
    /// `<dir>/<id>/<file>` hashes to the digest the channel named.
    async fn bring(
        &self,
        channel: &str,
        id: &str,
        artefact: &Artefact,
    ) -> std::result::Result<(), String> {
        let here = self.dir.join(id);
        tokio::fs::create_dir_all(&here)
            .await
            .map_err(|e| format!("making {}: {e}", here.display()))?;
        let dest = here.join(&artefact.file);
        if let Some(root) = channel.strip_prefix("file://") {
            // Verified where it lies, and linked rather than copied.
            let source = Path::new(root).join(&artefact.file);
            let got = sha256_of(&source).await?;
            if got != artefact.sha256 {
                return Err(mismatch(&artefact.file, &artefact.sha256, &got));
            }
            let _ = tokio::fs::remove_file(&dest).await;
            tokio::fs::symlink(&source, &dest)
                .await
                .map_err(|e| format!("linking {} to {}: {e}", dest.display(), source.display()))?;
            return Ok(());
        }
        let partial = here.join(format!("{}.partial", artefact.file));
        let _ = tokio::fs::remove_file(&partial).await;
        let url = release::file_url(channel, &artefact.file);
        if let Err(why) = self.fetch.download(&url, &partial).await {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(format!("fetching {url}: {why}"));
        }
        let got = sha256_of(&partial).await?;
        if got != artefact.sha256 {
            // Not kept: a file that hashed wrong is not a file a later pass
            // may find and trust.
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(mismatch(&artefact.file, &artefact.sha256, &got));
        }
        tokio::fs::rename(&partial, &dest)
            .await
            .map_err(|e| format!("moving {} into place: {e}", artefact.file))
    }
}

fn mismatch(file: &str, wanted: &str, got: &str) -> String {
    format!(
        "{file} did not hash to what the channel's {} names: expected {wanted}, got {got}. The \
         channel is inconsistent or the download was corrupted; nothing from it is kept.",
        release::SUMS
    )
}

fn not_ready(next: &mut Resource<ReleaseSpec, ReleaseStatus>, reason: &str, message: &str) {
    set_condition(
        &mut next.status.conditions,
        Condition::new(
            READY,
            ConditionStatus::False,
            reason,
            message,
            next.meta.generation,
        ),
    );
}

/// The lowercase hex SHA-256 of a file, a megabyte at a time.
async fn sha256_of(path: &Path) -> std::result::Result<String, String> {
    use sha2::Digest as _;
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
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

impl<F: Fetch> Reconciler for ReleaseController<F> {
    type Spec = ReleaseSpec;
    type Status = ReleaseStatus;

    fn name(&self) -> &'static str {
        "release"
    }

    async fn reconcile(
        &self,
        name: &str,
        object: Option<&Resource<Self::Spec, Self::Status>>,
    ) -> Result<()> {
        let Some(release) = object else {
            // Gone: so are its bytes. Only under this controller's own
            // directory, by the id the name carries.
            if let Some(id) = name.rsplit('/').next()
                && release::safe_file_name(id)
            {
                let here = self.dir.join(id);
                if tokio::fs::remove_dir_all(&here).await.is_ok() {
                    info!(release = name, "removed the release's files with it");
                }
            }
            return Ok(());
        };
        if release.meta.is_deleting() {
            return Ok(());
        }
        let mut next = release.clone();
        // The channel, read when it has not been or the spec changed.
        let unread = release.status.version.is_empty()
            || release.status.observed_generation != release.meta.generation;
        if unread && !self.read_channel(release, &mut next).await {
            return self.status.write(release, &next).await.map(|_| ());
        }

        next.status.observed_generation = release.meta.generation;
        // `fetched` is an observation about this host, not durable evidence
        // that another replica still has the file after a failover or restore.
        for slot in [
            &mut next.status.image,
            &mut next.status.package,
            &mut next.status.installer,
        ] {
            if let Some(artefact) = slot
                && artefact.fetched
                && !tokio::fs::try_exists(
                    self.dir.join(release.meta.name.id()).join(&artefact.file),
                )
                .await
                .unwrap_or(false)
            {
                artefact.fetched = false;
            }
        }
        // One file that is not here yet.
        let pending: Vec<Artefact> = next
            .status
            .artefacts()
            .into_iter()
            .filter(|(_, a)| !a.fetched)
            .map(|(_, a)| a.clone())
            .collect();
        let total = next.status.artefacts().len();
        if let Some(artefact) = pending.first() {
            let kind = release::classify(&artefact.file)
                .map(|(k, _)| k.describe())
                .unwrap_or("file");
            let nth = total - pending.len() + 1;
            not_ready(
                &mut next,
                "Fetching",
                &format!("fetching the {kind} ({nth} of {total}): {}", artefact.file),
            );
            // Said before it starts, so the console says what is happening for
            // the minutes it takes.
            let mut before = release.clone();
            if let Some(revision) = self.status.write(release, &next).await? {
                before = next.clone();
                before.meta.revision = revision;
            }
            let id = release.meta.name.id().to_string();
            match self.bring(&release.spec.url, &id, artefact).await {
                Ok(()) => {
                    for slot in [
                        &mut next.status.image,
                        &mut next.status.package,
                        &mut next.status.installer,
                    ] {
                        if let Some(a) = slot
                            && a.file == artefact.file
                        {
                            a.fetched = true;
                        }
                    }
                    info!(release = %release.meta.name, file = %artefact.file, "fetched and verified");
                    if next.status.artefacts().iter().all(|(_, a)| a.fetched) {
                        ready(&mut next);
                    } else {
                        not_ready(
                            &mut next,
                            "Fetching",
                            &format!("{} of {total} files are here; fetching the rest", nth),
                        );
                    }
                }
                Err(why) => {
                    warn!(release = %release.meta.name, file = %artefact.file, %why, "could not bring a file here");
                    not_ready(&mut next, "FetchFailed", &why);
                }
            }
            return self.status.write(&before, &next).await.map(|_| ());
        }

        if !next.status.is_ready() {
            ready(&mut next);
        }
        if next.status == release.status {
            return Ok(());
        }
        self.status.write(release, &next).await.map(|_| ())
    }
}

fn ready(next: &mut Resource<ReleaseSpec, ReleaseStatus>) {
    let files = next.status.artefacts().len();
    set_condition(
        &mut next.status.conditions,
        Condition::new(
            "Ready",
            ConditionStatus::True,
            "Ready",
            &format!("every file is on this cell and verified ({files})"),
            next.meta.generation,
        ),
    );
}

/// The channel over `https://`, `http://` or `file://`.
pub struct OverHttp {
    client: reqwest::Client,
}

impl OverHttp {
    pub fn new() -> std::result::Result<Self, String> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(60))
            .user_agent(concat!("velstra-cloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(|client| Self { client })
            .map_err(|e| e.to_string())
    }
}

impl Fetch for OverHttp {
    fn text(&self, url: &str) -> impl Future<Output = std::result::Result<String, String>> + Send {
        let url = url.to_string();
        let request = url
            .strip_prefix("file://")
            .map(|path| path.to_string())
            .ok_or_else(|| self.client.get(&url).send());
        async move {
            let bytes = match request {
                Ok(path) => tokio::fs::read(&path).await.map_err(|e| e.to_string())?,
                Err(request) => {
                    let response = request.await.map_err(|e| e.to_string())?;
                    let status = response.status();
                    if !status.is_success() {
                        return Err(format!("the server answered {status}"));
                    }
                    response.bytes().await.map_err(|e| e.to_string())?.to_vec()
                }
            };
            if bytes.len() > 4 * 1024 * 1024 {
                return Err("that is not a checksums file: over 4 MiB".into());
            }
            String::from_utf8(bytes).map_err(|_| "not text".to_string())
        }
    }

    fn download(
        &self,
        url: &str,
        dest: &Path,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        use tokio::io::AsyncWriteExt;

        let request = self.client.get(url).send();
        let dest = dest.to_path_buf();
        async move {
            let mut response = request.await.map_err(|e| e.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("the server answered {status}"));
            }
            let mut file = tokio::fs::File::create(&dest)
                .await
                .map_err(|e| format!("{}: {e}", dest.display()))?;
            while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| format!("writing {}: {e}", dest.display()))?;
            }
            file.flush()
                .await
                .map_err(|e| format!("writing {}: {e}", dest.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use velstra_cloud_model::{
        access::Writer,
        meta::{Meta, Placement, ResourceName},
    };
    use velstra_cloud_store::{MemoryStore, TypedStore};

    use super::*;

    const DEB: &str = "velstra-cloud_0.2.0+20260918.c571d71_amd64.deb";
    const ISO: &str = "velstra-cloud-installer_0.2.0+20260918.c571d71_amd64.iso";

    fn sha256(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A channel in memory: URL to bytes.
    struct Fake {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
        downloads: Mutex<Vec<String>>,
    }

    impl Fetch for Fake {
        fn text(
            &self,
            url: &str,
        ) -> impl Future<Output = std::result::Result<String, String>> + Send {
            let got = self.files.lock().unwrap().get(url).cloned();
            async move {
                got.map(|b| String::from_utf8_lossy(&b).to_string())
                    .ok_or_else(|| "404".to_string())
            }
        }
        fn download(
            &self,
            url: &str,
            dest: &Path,
        ) -> impl Future<Output = std::result::Result<(), String>> + Send {
            let got = self.files.lock().unwrap().get(url).cloned();
            self.downloads.lock().unwrap().push(url.to_string());
            let dest = dest.to_path_buf();
            async move {
                let bytes = got.ok_or_else(|| "404".to_string())?;
                tokio::fs::write(&dest, bytes)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    struct Bench {
        store: TypedStore<ReleaseSpec, ReleaseStatus>,
        controller: ReleaseController<Fake>,
        dir: PathBuf,
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn bench(tag: &str, files: BTreeMap<String, Vec<u8>>) -> Bench {
        let raw = Arc::new(MemoryStore::new());
        let store = TypedStore::new(raw.clone(), "cell-1", "releases");
        let dir =
            std::env::temp_dir().join(format!("velstra-release-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fake = Arc::new(Fake {
            files: Mutex::new(files),
            downloads: Mutex::new(Vec::new()),
        });
        let controller = ReleaseController::new(
            StatusWriter::new(raw, "cell-1", "releases", "release"),
            fake,
            dir.clone(),
        );
        let release = Resource::new(
            Meta::new(
                ResourceName::parse("releases/v0.2.0").unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            ReleaseSpec {
                url: "https://x.example/v0.2.0".into(),
            },
            ReleaseStatus::default(),
        );
        store
            .create(&release, &Writer::controller("test"))
            .await
            .unwrap();
        Bench {
            store,
            controller,
            dir,
        }
    }

    async fn pass(b: &Bench) -> Resource<ReleaseSpec, ReleaseStatus> {
        let current = b.store.get("releases/v0.2.0").await.unwrap().unwrap();
        b.controller
            .reconcile("releases/v0.2.0", Some(&current))
            .await
            .unwrap();
        b.store.get("releases/v0.2.0").await.unwrap().unwrap()
    }

    fn channel(deb: &[u8], iso: &[u8], sums: &str) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "https://x.example/v0.2.0/SHA256SUMS".to_string(),
                sums.as_bytes().to_vec(),
            ),
            (format!("https://x.example/v0.2.0/{DEB}"), deb.to_vec()),
            (format!("https://x.example/v0.2.0/{ISO}"), iso.to_vec()),
        ])
    }

    /// The channel is read, then one file per pass is brought here and
    /// verified, and the release is ready when the last one is.
    #[tokio::test]
    async fn a_channel_is_read_and_its_files_brought_here_one_pass_at_a_time() {
        let deb = b"the package".to_vec();
        let iso = vec![7u8; 9000];
        let sums = format!("{}  ./{DEB}\n{}  ./{ISO}\n", sha256(&deb), sha256(&iso));
        let b = bench("bring", channel(&deb, &iso, &sums)).await;

        let after_one = pass(&b).await;
        assert_eq!(after_one.status.version, "0.2.0+20260918.c571d71");
        assert!(
            after_one.status.image.is_none(),
            "the channel named no image"
        );
        assert!(
            after_one.status.package.as_ref().unwrap().fetched,
            "{:?}",
            after_one.status
        );
        assert!(!after_one.status.installer.as_ref().unwrap().fetched);
        assert!(!after_one.status.is_ready());
        assert!(
            after_one
                .status
                .not_ready_because()
                .contains("fetching the rest"),
            "{}",
            after_one.status.not_ready_because()
        );
        assert_eq!(std::fs::read(b.dir.join("v0.2.0").join(DEB)).unwrap(), deb);

        let after_two = pass(&b).await;
        assert!(after_two.status.installer.as_ref().unwrap().fetched);
        assert!(
            after_two.status.is_ready(),
            "{:?}",
            after_two.status.conditions
        );
        assert_eq!(std::fs::read(b.dir.join("v0.2.0").join(ISO)).unwrap(), iso);

        // Settled: another pass reads nothing and writes nothing.
        let downloads = b.controller.fetch.downloads.lock().unwrap().len();
        let after_three = pass(&b).await;
        assert_eq!(after_three.meta.revision, after_two.meta.revision);
        assert_eq!(
            b.controller.fetch.downloads.lock().unwrap().len(),
            downloads
        );
    }

    /// A file that hashes wrong is refused with both digests, and not kept.
    #[tokio::test]
    async fn a_file_that_hashes_wrong_is_refused_and_not_kept() {
        let deb = b"the package".to_vec();
        let iso = vec![7u8; 9000];
        let sums = format!("{}  {DEB}\n{}  {ISO}\n", "0".repeat(64), sha256(&iso));
        let b = bench("wrong", channel(&deb, &iso, &sums)).await;
        let after = pass(&b).await;
        assert!(!after.status.is_ready());
        let why = after.status.not_ready_because();
        assert!(
            why.contains(&"0".repeat(64)) && why.contains(&sha256(&deb)),
            "{why}"
        );
        assert!(!after.status.package.as_ref().unwrap().fetched);
        assert!(
            !b.dir.join("v0.2.0").join(DEB).exists(),
            "the wrong bytes are not kept"
        );
        assert!(!b.dir.join("v0.2.0").join(format!("{DEB}.partial")).exists());
    }

    /// A channel that cannot be read, or that is not a release, says so.
    #[tokio::test]
    async fn an_unreadable_or_unusable_channel_says_so() {
        let b = bench("unreachable", BTreeMap::new()).await;
        let after = pass(&b).await;
        assert!(
            after.status.not_ready_because().contains("could not read"),
            "{}",
            after.status.not_ready_because()
        );
        assert!(after.status.version.is_empty());

        let notes = BTreeMap::from([(
            "https://x.example/v0.2.0/SHA256SUMS".to_string(),
            format!("{}  notes.txt\n", "1".repeat(64)).into_bytes(),
        )]);
        let b = bench("unusable", notes).await;
        let after = pass(&b).await;
        assert!(
            after
                .status
                .not_ready_because()
                .contains("names nothing this cell recognises"),
            "{}",
            after.status.not_ready_because()
        );
    }

    /// A directory on this disk is verified where it lies and linked, not
    /// copied — through the real fetcher, which needs no network for it.
    #[tokio::test]
    async fn a_file_channel_is_verified_in_place_and_linked() {
        let channel_dir =
            std::env::temp_dir().join(format!("velstra-channel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&channel_dir);
        std::fs::create_dir_all(&channel_dir).unwrap();
        let deb = b"a local package".to_vec();
        std::fs::write(channel_dir.join(DEB), &deb).unwrap();
        std::fs::write(
            channel_dir.join("SHA256SUMS"),
            format!("{}  {DEB}\n", sha256(&deb)),
        )
        .unwrap();

        let raw = Arc::new(MemoryStore::new());
        let store: TypedStore<ReleaseSpec, ReleaseStatus> =
            TypedStore::new(raw.clone(), "cell-1", "releases");
        let dir =
            std::env::temp_dir().join(format!("velstra-release-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let controller = ReleaseController::new(
            StatusWriter::new(raw, "cell-1", "releases", "release"),
            Arc::new(OverHttp::new().unwrap()),
            dir.clone(),
        );
        let release = Resource::new(
            Meta::new(
                ResourceName::parse("releases/local").unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            ReleaseSpec {
                url: format!("file://{}", channel_dir.display()),
            },
            ReleaseStatus::default(),
        );
        store
            .create(&release, &Writer::controller("test"))
            .await
            .unwrap();
        let current = store.get("releases/local").await.unwrap().unwrap();
        controller
            .reconcile("releases/local", Some(&current))
            .await
            .unwrap();
        let after = store.get("releases/local").await.unwrap().unwrap();
        assert!(after.status.is_ready(), "{:?}", after.status.conditions);
        let linked = dir.join("local").join(DEB);
        assert!(
            std::fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink(),
            "linked, not copied"
        );
        assert_eq!(std::fs::read(&linked).unwrap(), deb);

        // Gone with the object.
        controller.reconcile("releases/local", None).await.unwrap();
        assert!(!dir.join("local").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&channel_dir);
    }
    #[tokio::test]
    async fn a_failed_changed_channel_never_reuses_the_previous_ready_generation() {
        let deb = b"package";
        let iso = b"installer";
        let sums = format!("{}  {DEB}\n{}  {ISO}\n", sha256(deb), sha256(iso));
        let b = bench("changed-channel", channel(deb, iso, &sums)).await;
        pass(&b).await;
        let mut ready = pass(&b).await;
        assert!(ready.status.is_ready());
        ready.spec.url = "https://missing.example".into();
        ready.meta.generation += 1;
        b.store
            .update(&ready, &Writer::controller("test"))
            .await
            .unwrap();
        for _ in 0..3 {
            let failed = pass(&b).await;
            assert!(!failed.status.is_ready());
            assert_ne!(failed.status.observed_generation, failed.meta.generation);
        }
    }

    #[tokio::test]
    async fn missing_release_files_are_downloaded_again_after_a_failover() {
        let deb = b"package";
        let iso = b"installer";
        let sums = format!("{}  {DEB}\n{}  {ISO}\n", sha256(deb), sha256(iso));
        let b = bench("lost-file", channel(deb, iso, &sums)).await;
        pass(&b).await;
        assert!(pass(&b).await.status.is_ready());
        let file = b.dir.join("v0.2.0").join(DEB);
        std::fs::remove_file(&file).unwrap();
        assert!(pass(&b).await.status.is_ready());
        assert_eq!(std::fs::read(&file).unwrap(), deb);
    }
}
