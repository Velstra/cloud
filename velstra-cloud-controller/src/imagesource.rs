//! Keep a family's images current.
//!
//! The whole of the trust argument lives in [`velstra_cloud_model::images`]; the
//! short version is that the *digest* is learned over `https://` with the
//! certificate checked, and the *bytes* are then fetched by the node over
//! whatever scheme the URL names, because content-addressed bytes need no
//! transport security and a wrong byte fails the fetch.
//!
//! What this does on each pass is deliberately small: look, and if the answer is
//! a digest this cell does not have, publish an image. It changes nothing about
//! any guest. A machine keeps the bytes it was built from, and "always the
//! newest" is delivered where it belongs — at creation, by `families/<name>`.

use std::sync::Arc;

use tracing::{info, warn};
use velstra_cloud_model::{
    ConditionStatus,
    images::{ImageSourceSpec, ImageSourceStatus},
    meta::{Condition, Meta, Placement, ResourceName, Timestamp, set_condition},
    resources::{ImageFormat, ImageSpec, ImageState, ImageStatus, Resource},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler, status::StatusWriter};

const WRITER: &str = "images";

/// What the platform's own condition on a source is called.
///
/// One name, written on every pass whatever the outcome, so a source that cannot
/// be reached says so on the object rather than only in a log — which is the
/// difference between an operator seeing a stale family and not.
pub const CHECKED: &str = "Checked";

/// Fetch a URL's body as text.
///
/// A trait so the controller can be tested without a network, and so the one
/// place that talks to the outside is named. The implementation checks
/// certificates; a source's checksums URL is refused unless it is `https://`,
/// which is enforced at the API door as well as here.
pub trait Fetch: Send + Sync + 'static {
    fn text(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = std::result::Result<String, String>> + Send;
}

pub struct ImageSourceController<F: Fetch> {
    images: TypedStore<ImageSpec, ImageStatus>,
    /// Read only to answer one question before a delete: is anything built from
    /// this. The controller writes to the store directly, so the API's
    /// reference guard is not in its path and it has to ask for itself.
    instances: TypedStore<
        velstra_cloud_model::resources::InstanceSpec,
        velstra_cloud_model::resources::InstanceStatus,
    >,
    status: StatusWriter<ImageSourceSpec, ImageSourceStatus>,
    fetch: Arc<F>,
    cell: String,
    region: String,
}

impl<F: Fetch> ImageSourceController<F> {
    pub fn new(
        images: TypedStore<ImageSpec, ImageStatus>,
        instances: TypedStore<
            velstra_cloud_model::resources::InstanceSpec,
            velstra_cloud_model::resources::InstanceStatus,
        >,
        status: StatusWriter<ImageSourceSpec, ImageSourceStatus>,
        fetch: Arc<F>,
        region: &str,
        cell: &str,
    ) -> Self {
        Self {
            images,
            instances,
            status,
            fetch,
            cell: cell.to_string(),
            region: region.to_string(),
        }
    }

    /// Whether enough time has passed to look again.
    ///
    /// A generation the controller has not seen is always due: an operator who
    /// just changed the URL is waiting for an answer about the URL they typed,
    /// not for the interval to elapse.
    fn due(source: &Resource<ImageSourceSpec, ImageSourceStatus>, now: Timestamp) -> bool {
        if source.status.observed_generation != source.meta.generation {
            return true;
        }
        let last = source.status.last_checked.0;
        last == 0 || now.0.saturating_sub(last) >= velstra_cloud_model::images::every(&source.spec)
    }

    /// Publish an image for a digest this cell does not have.
    ///
    /// The id is the digest, as every image's is, so publishing the same digest
    /// twice is not two images — it is the same object, and the create is
    /// refused as already existing, which is the answer and not an error.
    async fn publish(
        &self,
        spec: &ImageSourceSpec,
        digest: &velstra_cloud_model::images::Digest,
        now: Timestamp,
    ) -> Result<String> {
        let id = digest.stored();
        let name = ResourceName::parse(&format!("images/{id}"))
            .map_err(|e| crate::Error::Refused(e.to_string()))?;
        if self.images.get(&name.to_string()).await?.is_some() {
            return Ok(name.to_string());
        }
        let image = Resource::new(
            Meta::new(name.clone(), Placement::new(&self.region, &self.cell)),
            ImageSpec {
                from: String::new(),
                family: spec.family.clone(),
                // Dated, because a source that publishes the same file name
                // every time gives no version of its own, and "which one is
                // this" has to be answerable by a person reading a list.
                version: iso_day(now),
                digest: digest.value(),
                format: ImageFormat::Qcow2,
                size_bytes: 0,
                source_url: spec.url.clone(),
                source_instance: None,
                signature: None,
                state: ImageState::Active,
                replacement: String::new(),
                shared_with: Default::default(),
            },
            ImageStatus::default(),
        );
        self.images
            .create(&image, &velstra_cloud_model::Writer::controller(WRITER))
            .await?;
        self.retire_what_this_supersedes(spec, &name).await;
        Ok(name.to_string())
    }

    /// Deprecate the images this one has just replaced.
    ///
    /// Publishing a newer image into a family already changes what
    /// `families/<family>` resolves to — but that leaves the old one looking
    /// exactly as current as the new one on every list and every form, and the
    /// only way to tell them apart is to compare dates by eye. Saying it on
    /// the object is what makes "which of these should I be using" answerable
    /// without knowing how the family rule works.
    ///
    /// Only images from **this same source**: a family can hold images
    /// somebody published by hand, and a rotation deprecating those would be
    /// this controller reaching outside what it owns.
    ///
    /// Best effort and never fatal. The new image exists either way, and a
    /// rotation that failed because a *label* could not be written would be a
    /// cell that stops getting security updates over a cosmetic field.
    async fn retire_what_this_supersedes(&self, spec: &ImageSourceSpec, published: &ResourceName) {
        let Ok(images) = self.images.list().await else {
            warn!("could not read the images to deprecate what a new one replaces");
            return;
        };
        for image in images {
            if image.meta.name == *published
                || image.spec.family != spec.family
                || image.spec.source_url != spec.url
                || image.meta.deleted_at.is_some()
                || image.spec.state != ImageState::Active
            {
                continue;
            }
            let mut next = image.clone();
            next.spec.state = ImageState::Deprecated;
            next.spec.replacement = published.to_string();
            next.meta.generation += 1;
            if let Err(e) = self
                .images
                .update(&next, &velstra_cloud_model::Writer::controller(WRITER))
                .await
            {
                warn!(image = %image.meta.name, error = %e, "could not deprecate a superseded image");
            } else {
                info!(image = %image.meta.name, replacement = %published, "deprecated: a newer image of this family was published");
            }
        }
    }

    /// Take away the versions past `keep`, and only ones nobody is using.
    ///
    /// Three rules, each of which is the difference between tidying up and
    /// breaking somebody's estate:
    ///
    /// **Only what this source published.** A family can hold images somebody
    /// made by hand — a patched build, a golden image captured from a guest —
    /// and a source's retention has no business deleting those. Matched by
    /// `source_url`, which is the source's own fingerprint on what it made.
    ///
    /// **Never one an instance names.** A guest keeps the bytes it was built
    /// from, so an image an instance references is needed for as long as that
    /// instance can be moved, restarted, or rebuilt on another node — however
    /// old it is, and whatever `keep` says. The controller writes to the store
    /// directly, which means the API's reference guard is not in its path: it
    /// asks the question itself, and a deleted-but-not-yet-gone instance counts,
    /// because it may still be starting.
    ///
    /// **Newest first, by when this cell learned of them** — the same ordering
    /// `families/<name>` resolves by, so what a new guest would get is never
    /// what retention takes away.
    async fn prune(&self, spec: &ImageSourceSpec) -> Result<(Vec<String>, usize)> {
        let keep = velstra_cloud_model::images::keep(spec) as usize;
        let mut mine: Vec<_> = self
            .images
            .list()
            .await?
            .into_iter()
            .filter(|i: &Resource<ImageSpec, ImageStatus>| {
                i.spec.family == spec.family
                    && i.spec.source_url == spec.url
                    && i.meta.deleted_at.is_none()
            })
            .collect();
        if mine.len() <= keep {
            return Ok((Vec::new(), 0));
        }
        mine.sort_by_key(|i| std::cmp::Reverse(i.meta.created_at.0));

        let in_use: std::collections::BTreeSet<String> = self
            .instances
            .list()
            .await?
            .into_iter()
            .map(|i| i.spec.image.clone())
            .collect();

        let mut removed = Vec::new();
        let mut spared = 0usize;
        for old in mine.into_iter().skip(keep) {
            let name = old.meta.name.to_string();
            if in_use.contains(&name) {
                spared += 1;
                continue;
            }
            self.images
                .delete(
                    &name,
                    old.meta.revision,
                    &velstra_cloud_model::Writer::controller(WRITER),
                )
                .await?;
            removed.push(name);
        }
        Ok((removed, spared))
    }
}

/// `2026-08-28`, from a timestamp, without pulling in a date library.
///
/// Civil-from-days, the standard algorithm. A version string is read by people
/// and never compared by this platform, so the only thing that matters is that
/// it is right and stable.
fn iso_day(now: Timestamp) -> String {
    let days = (now.0 / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl<F: Fetch> Reconciler for ImageSourceController<F> {
    type Spec = ImageSourceSpec;
    type Status = ImageSourceStatus;

    fn name(&self) -> &'static str {
        "image-sources"
    }

    async fn reconcile(
        &self,
        _name: &str,
        object: Option<&Resource<ImageSourceSpec, ImageSourceStatus>>,
    ) -> Result<()> {
        let Some(source) = object else {
            return Ok(());
        };
        if source.spec.paused {
            let mut next = source.clone();
            next.status.observed_generation = source.meta.generation;
            set_condition(
                &mut next.status.conditions,
                Condition::new(
                    CHECKED,
                    ConditionStatus::True,
                    "Paused",
                    "not looking; this source is paused",
                    source.meta.generation,
                ),
            );
            return self.status.write(source, &next).await.map(|_| ());
        }
        let now = Timestamp::now();
        if !Self::due(source, now) {
            return Ok(());
        }

        let mut next = source.clone();
        next.status.observed_generation = source.meta.generation;
        next.status.last_checked = now;

        // Refused at the API door too, and asked again here: an object stored
        // before that check existed is still an object this must not act on.
        if let Err(why) = velstra_cloud_model::images::refuse_an_unusable_source(&source.spec) {
            set_condition(
                &mut next.status.conditions,
                Condition::new(
                    CHECKED,
                    ConditionStatus::False,
                    "Unusable",
                    &why.to_string(),
                    source.meta.generation,
                ),
            );
            return self.status.write(source, &next).await.map(|_| ());
        }

        let body = match self.fetch.text(&source.spec.checksums).await {
            Ok(body) => body,
            Err(why) => {
                set_condition(
                    &mut next.status.conditions,
                    Condition::new(
                        CHECKED,
                        ConditionStatus::False,
                        "Unreachable",
                        &format!("could not read {}: {why}", source.spec.checksums),
                        source.meta.generation,
                    ),
                );
                return self.status.write(source, &next).await.map(|_| ());
            }
        };

        let filename = velstra_cloud_model::images::filename_of(&source.spec.url);
        let Some(digest) = velstra_cloud_model::images::digest_for(&body, filename)
            .and_then(|d| velstra_cloud_model::images::Digest::parse(&d))
        else {
            set_condition(
                &mut next.status.conditions,
                Condition::new(
                    CHECKED,
                    ConditionStatus::False,
                    "NotListed",
                    &format!(
                        "{} names no digest for `{filename}` this platform can read. A \
                         checksums file says which bytes a file is as a sha256 or a sha512, \
                         one per line, in either the coreutils or the BSD-tag layout; a file \
                         that mentions this image under none of those says nothing about it.",
                        source.spec.checksums
                    ),
                    source.meta.generation,
                ),
            );
            return self.status.write(source, &next).await.map(|_| ());
        };

        let published = self.publish(&source.spec, &digest, now).await?;
        // Compared and stored as the tagged value, so a source that moved from
        // one hash function to the other reads as a change rather than as the
        // same image under a name nothing files it under.
        let digest = digest.value();
        let is_new = source.status.last_digest != digest;
        next.status.last_digest = digest;
        next.status.published = published.clone();

        // Only after a publish. Retention that ran on every look would be a loop
        // reading every image and every instance in the cell six times a day to
        // find nothing to do; nothing can fall out of `keep` unless something new
        // came in.
        let (removed, spared) = if is_new {
            self.prune(&source.spec).await?
        } else {
            (Vec::new(), 0)
        };
        if !removed.is_empty() {
            info!(
                family = source.spec.family,
                removed = removed.len(),
                "older versions taken away"
            );
        }

        let mut message = if is_new {
            format!("{published} is the newest {}", source.spec.family)
        } else {
            format!("{} is still current", source.spec.family)
        };
        if !removed.is_empty() {
            message.push_str(&format!("; {} older ones taken away", removed.len()));
        }
        // Said out loud, because "why is this family still holding eleven
        // versions when keep is three" has exactly one answer worth reading, and
        // an operator should not have to work it out from a list of guests.
        if spared > 0 {
            message.push_str(&format!(
                "; {spared} past `keep` kept because instances are built from them"
            ));
        }
        set_condition(
            &mut next.status.conditions,
            Condition::new(
                CHECKED,
                ConditionStatus::True,
                if is_new { "Published" } else { "Unchanged" },
                &message,
                source.meta.generation,
            ),
        );
        self.status.write(source, &next).await.map(|_| ())
    }
}

/// The real one: https, certificate checked, with a deadline.
///
/// A checksums file is a few kilobytes. The cap is there because a controller
/// that blocks for ever on one unreachable source stops looking at all the
/// others, and a source pointed at something that streams is not a source.
pub struct OverHttps {
    client: reqwest::Client,
}

impl OverHttps {
    pub fn new() -> std::result::Result<Self, String> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("velstra-cloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(|client| Self { client })
            .map_err(|e| e.to_string())
    }
}

impl Fetch for OverHttps {
    fn text(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = std::result::Result<String, String>> + Send {
        let request = self.client.get(url).send();
        async move {
            let response = request.await.map_err(|e| e.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("the server answered {status}"));
            }
            // Bounded, because the answer is a checksums file and anything that
            // does not end is not one.
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            if body.len() > 4 * 1024 * 1024 {
                return Err("that is not a checksums file: over 4 MiB".into());
            }
            String::from_utf8(body.to_vec()).map_err(|_| "not text".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use velstra_cloud_model::meta::condition;
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    /// A checksums file, handed over without a network.
    struct Says {
        body: Mutex<std::result::Result<String, String>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fetch for Says {
        fn text(
            &self,
            url: &str,
        ) -> impl std::future::Future<Output = std::result::Result<String, String>> + Send {
            self.asked.lock().unwrap().push(url.to_string());
            let answer = self.body.lock().unwrap().clone();
            async move { answer }
        }
    }

    const DIGEST: &str = "cbf3e1f588f02f8d738dbecb32652d07568cc1d56cd60f72dbed54400ba3ae8d";

    fn sums(digest: &str) -> String {
        format!("{digest}  debian-13-genericcloud-amd64.qcow2\n")
    }

    fn a_source() -> Resource<ImageSourceSpec, ImageSourceStatus> {
        let mut s = Resource::new(
            Meta::new(
                "image-sources/debian".parse().unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            ImageSourceSpec {
                family: "debian-13".into(),
                url: "http://cloud.example/debian-13-genericcloud-amd64.qcow2".into(),
                checksums: "https://cloud.example/SHA256SUMS".into(),
                ..Default::default()
            },
            ImageSourceStatus::default(),
        );
        s.meta.generation = 1;
        s
    }

    async fn fixture(
        body: std::result::Result<String, String>,
    ) -> (
        Arc<MemoryStore>,
        Arc<Says>,
        ImageSourceController<Says>,
        TypedStore<ImageSourceSpec, ImageSourceStatus>,
    ) {
        let raw = Arc::new(MemoryStore::new());
        let store: Arc<dyn Store> = raw.clone();
        let sources = TypedStore::new(store.clone(), "cell-1", "image-sources");
        let images = TypedStore::new(store.clone(), "cell-1", "images");
        let says = Arc::new(Says {
            body: Mutex::new(body),
            asked: Mutex::new(Vec::new()),
        });
        let instances = TypedStore::new(store.clone(), "cell-1", "instances");
        let c = ImageSourceController::new(
            images,
            instances,
            StatusWriter::new(store, "cell-1", "image-sources", WRITER),
            says.clone(),
            "eu-central",
            "cell-1",
        );
        (raw, says, c, sources)
    }

    #[tokio::test]
    async fn a_source_publishes_the_digest_its_checksums_file_names() {
        let (_raw, says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();

        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        // It asked the checksums file, not the image: the digest is the thing
        // being learned, and the bytes are the node's business.
        assert_eq!(
            says.asked.lock().unwrap().as_slice(),
            &["https://cloud.example/SHA256SUMS".to_string()]
        );

        let stored = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("the source is still there");
        // Tagged, not bare: which function it is travels with the hex, so a
        // source that moves between them reads as a change.
        assert_eq!(stored.status.last_digest, format!("sha256:{DIGEST}"));
        assert_eq!(stored.status.published, format!("images/sha256-{DIGEST}"));
        let checked = condition(&stored.status.conditions, CHECKED).expect("it says it looked");
        assert_eq!(checked.status, ConditionStatus::True);
        assert_eq!(checked.reason, "Published");
    }

    #[tokio::test]
    async fn a_second_look_at_the_same_digest_publishes_nothing_new() {
        // The id *is* the digest, so re-publishing is not two images. What must
        // not happen is the condition claiming a new one arrived every six hours.
        let (_raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();
        // Wound back, because within the interval the second pass does not look
        // at all — which is the point of the interval and not what this checks.
        let mut once = sources.get("image-sources/debian").await.unwrap().unwrap();
        once.status.last_checked = Timestamp(1);
        sources
            .update(&once, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let once = sources.get("image-sources/debian").await.unwrap().unwrap();

        c.reconcile("image-sources/debian", Some(&once))
            .await
            .unwrap();
        let twice = sources.get("image-sources/debian").await.unwrap().unwrap();
        let checked = condition(&twice.status.conditions, CHECKED).unwrap();
        assert_eq!(checked.reason, "Unchanged", "{}", checked.message);
    }

    #[tokio::test]
    async fn a_checksums_file_that_cannot_be_read_says_so_on_the_object() {
        // Not only in a log. A family that quietly stopped being updated is the
        // failure this whole thing exists to prevent, and it is invisible unless
        // the source says it could not look.
        let (_raw, _says, c, sources) = fixture(Err("connection refused".into())).await;
        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();
        let stored = sources.get("image-sources/debian").await.unwrap().unwrap();
        let checked = condition(&stored.status.conditions, CHECKED).unwrap();
        assert_eq!(checked.status, ConditionStatus::False);
        assert_eq!(checked.reason, "Unreachable");
        assert!(
            checked.message.contains("connection refused"),
            "{}",
            checked.message
        );
    }

    /// Debian publishes `SHA512SUMS` for its cloud images and no `SHA256SUMS`
    /// at all, so a source pointed at the distribution most people start with
    /// used to be a source that could never publish anything. It publishes
    /// now, under the hash the file actually gives — and it is *filed* under
    /// that hash too, because a sha512 recorded as a sha256 is a digest no
    /// bytes will ever match.
    #[tokio::test]
    async fn a_sha512sums_file_publishes_under_sha512() {
        let hex = "b".repeat(128);
        let (_raw, _says, c, sources) =
            fixture(Ok(format!("{hex}  debian-13-genericcloud-amd64.qcow2\n"))).await;
        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();
        let stored = sources.get("image-sources/debian").await.unwrap().unwrap();
        let checked = condition(&stored.status.conditions, CHECKED).unwrap();
        assert_eq!(checked.status, ConditionStatus::True, "{}", checked.message);
        assert_eq!(stored.status.last_digest, format!("sha512:{hex}"));
        assert_eq!(stored.status.published, format!("images/sha512-{hex}"));
    }

    /// A checksums file that mentions the image under neither function still
    /// says nothing about it, and the source says so rather than publishing
    /// something it invented.
    #[tokio::test]
    async fn a_length_that_is_neither_names_nothing() {
        let (_raw, _says, c, sources) = fixture(Ok(format!(
            "{}  debian-13-genericcloud-amd64.qcow2\n",
            "a".repeat(40)
        )))
        .await;
        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();
        let stored = sources.get("image-sources/debian").await.unwrap().unwrap();
        let checked = condition(&stored.status.conditions, CHECKED).unwrap();
        assert_eq!(checked.reason, "NotListed");
    }

    #[tokio::test]
    async fn a_paused_source_is_not_looked_at() {
        let (_raw, says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let mut source = a_source();
        source.spec.paused = true;
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources
            .get("image-sources/debian")
            .await
            .unwrap()
            .expect("stored");
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();
        assert!(
            says.asked.lock().unwrap().is_empty(),
            "a paused source was fetched"
        );
        let stored = sources.get("image-sources/debian").await.unwrap().unwrap();
        assert_eq!(
            condition(&stored.status.conditions, CHECKED)
                .unwrap()
                .reason,
            "Paused"
        );
    }

    /// Publish `n` versions of the family, oldest first, as the source would.
    async fn versions(
        images: &TypedStore<ImageSpec, ImageStatus>,
        url: &str,
        family: &str,
        digests: &[&str],
    ) {
        for (n, d) in digests.iter().enumerate() {
            let mut image = Resource::new(
                Meta::new(
                    format!("images/sha256-{d}").parse().unwrap(),
                    Placement::new("eu-central", "cell-1"),
                ),
                ImageSpec {
                    from: String::new(),
                    family: family.into(),
                    version: format!("v{n}"),
                    digest: format!("sha256:{d}"),
                    format: velstra_cloud_model::resources::ImageFormat::Qcow2,
                    size_bytes: 0,
                    source_url: url.into(),
                    source_instance: None,
                    signature: None,
                    state: ImageState::Active,
                    replacement: String::new(),
                    shared_with: Default::default(),
                },
                ImageStatus::default(),
            );
            // Age is the ordering, and it is the only one: oldest goes first.
            image.meta.created_at = Timestamp(1_000 + n as u64);
            images
                .create(&image, &velstra_cloud_model::Writer::controller("test"))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn the_versions_past_keep_are_taken_away_newest_first() {
        let (raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let store: Arc<dyn Store> = raw.clone();
        let images: TypedStore<ImageSpec, ImageStatus> = TypedStore::new(store, "cell-1", "images");
        let url = "http://cloud.example/debian-13-genericcloud-amd64.qcow2";
        versions(
            &images,
            url,
            "debian-13",
            &[
                "a".repeat(64).as_str(),
                &"b".repeat(64),
                &"c".repeat(64),
                &"d".repeat(64),
            ],
        )
        .await;

        let mut source = a_source();
        source.spec.keep = 2;
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources.get("image-sources/debian").await.unwrap().unwrap();
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        let left: Vec<String> = images
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.meta.name.id().to_string())
            .collect();
        // The one just published and the newest of the four: `keep` counts
        // versions, and the newest is what `families/…` would hand out.
        assert!(
            left.contains(&format!("sha256-{DIGEST}")),
            "the one just published was taken away: {left:?}"
        );
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(
            !left.iter().any(|n| n.ends_with(&"a".repeat(64))),
            "the oldest survived: {left:?}"
        );
    }

    #[tokio::test]
    async fn an_image_a_guest_was_built_from_is_never_taken_away() {
        // Whatever `keep` says. A guest keeps the bytes it was built from for as
        // long as it exists — take the image and the machine cannot be restarted
        // on another node, which is a cleanup that breaks somebody's estate.
        let (raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let store: Arc<dyn Store> = raw.clone();
        let images: TypedStore<ImageSpec, ImageStatus> =
            TypedStore::new(store.clone(), "cell-1", "images");
        let instances: TypedStore<
            velstra_cloud_model::resources::InstanceSpec,
            velstra_cloud_model::resources::InstanceStatus,
        > = TypedStore::new(store, "cell-1", "instances");
        let url = "http://cloud.example/debian-13-genericcloud-amd64.qcow2";
        let old = "a".repeat(64);
        versions(
            &images,
            url,
            "debian-13",
            &[&old, &"b".repeat(64), &"c".repeat(64)],
        )
        .await;

        let mut guest = Resource::new(
            Meta::new(
                "projects/p1/instances/i1".parse().unwrap(),
                Placement::new("eu-central", "cell-1"),
            ),
            velstra_cloud_model::resources::InstanceSpec {
                image: format!("images/sha256-{old}"),
                ..Default::default()
            },
            velstra_cloud_model::resources::InstanceStatus::default(),
        );
        guest.meta.generation = 1;
        instances
            .create(&guest, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();

        let mut source = a_source();
        source.spec.keep = 1;
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources.get("image-sources/debian").await.unwrap().unwrap();
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        let left: Vec<String> = images
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.meta.name.id().to_string())
            .collect();
        assert!(
            left.contains(&format!("sha256-{old}")),
            "an image a guest was built from was taken away: {left:?}"
        );
        // And it says so rather than leaving somebody to wonder why `keep` looks
        // wrong.
        let stored = sources.get("image-sources/debian").await.unwrap().unwrap();
        let checked = condition(&stored.status.conditions, CHECKED).unwrap();
        assert!(
            checked.message.contains("instances are built from them"),
            "{}",
            checked.message
        );
    }

    #[tokio::test]
    async fn an_image_this_source_did_not_publish_is_left_alone() {
        // A family can hold a hand-made image — a patched build, a golden image
        // captured from a guest. A source's retention has no business deleting
        // somebody else's work just because it shares a name.
        let (raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let store: Arc<dyn Store> = raw.clone();
        let images: TypedStore<ImageSpec, ImageStatus> = TypedStore::new(store, "cell-1", "images");
        let url = "http://cloud.example/debian-13-genericcloud-amd64.qcow2";
        versions(
            &images,
            url,
            "debian-13",
            &[&"b".repeat(64), &"c".repeat(64)],
        )
        .await;
        versions(
            &images,
            "http://elsewhere.example/patched.qcow2",
            "debian-13",
            &[&"e".repeat(64)],
        )
        .await;

        let mut source = a_source();
        source.spec.keep = 1;
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources.get("image-sources/debian").await.unwrap().unwrap();
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        let left: Vec<String> = images
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.meta.name.id().to_string())
            .collect();
        assert!(
            left.contains(&format!("sha256-{}", "e".repeat(64))),
            "a hand-made image was taken away by a source's retention: {left:?}"
        );
    }

    // ---- superseding: publishing a newer image deprecates what it replaced

    /// The case this exists for: yesterday's image stops looking as current as
    /// today's, and says where to go.
    #[tokio::test]
    async fn a_new_image_deprecates_the_one_it_replaces() {
        let (raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let store: Arc<dyn Store> = raw.clone();
        let images: TypedStore<ImageSpec, ImageStatus> = TypedStore::new(store, "cell-1", "images");
        let url = "http://cloud.example/debian-13-genericcloud-amd64.qcow2";
        let old = "b".repeat(64);
        versions(&images, url, "debian-13", &[&old]).await;

        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources.get("image-sources/debian").await.unwrap().unwrap();
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        let was = images
            .get(&format!("images/sha256-{old}"))
            .await
            .unwrap()
            .expect("the older image is still there");
        assert_eq!(
            was.spec.state,
            ImageState::Deprecated,
            "the superseded image still looks as current as its replacement"
        );
        assert_eq!(was.spec.replacement, format!("images/sha256-{DIGEST}"));

        let now = images
            .get(&format!("images/sha256-{DIGEST}"))
            .await
            .unwrap()
            .expect("the new image");
        assert_eq!(now.spec.state, ImageState::Active);
    }

    /// And it does not reach outside what it publishes: a hand-made image in
    /// the same family is somebody else's, deprecating included.
    #[tokio::test]
    async fn an_image_from_elsewhere_is_not_deprecated() {
        let (raw, _says, c, sources) = fixture(Ok(sums(DIGEST))).await;
        let store: Arc<dyn Store> = raw.clone();
        let images: TypedStore<ImageSpec, ImageStatus> = TypedStore::new(store, "cell-1", "images");
        let mine = "e".repeat(64);
        versions(
            &images,
            "http://elsewhere.example/patched.qcow2",
            "debian-13",
            &[&mine],
        )
        .await;

        let source = a_source();
        sources
            .create(&source, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let source = sources.get("image-sources/debian").await.unwrap().unwrap();
        c.reconcile("image-sources/debian", Some(&source))
            .await
            .unwrap();

        let untouched = images
            .get(&format!("images/sha256-{mine}"))
            .await
            .unwrap()
            .expect("the hand-made image");
        assert_eq!(
            untouched.spec.state,
            ImageState::Active,
            "a rotation deprecated an image it did not publish"
        );
    }
}
