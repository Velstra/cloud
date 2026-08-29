//! Turning a finished capture into an image.
//!
//! The node copies the bytes and reports what they hashed to. This is the half
//! that follows: an image object, named for that digest, pointing at where the
//! bytes landed.
//!
//! It is split that way because of who may write what. Only the node holding
//! the disk can copy it, and only a controller may create an object — so the
//! node reports a digest and this makes the image. The same division as a
//! migration, where the destination reports a receiver and a controller moves
//! the assignment.
//!
//! ## Why the image cannot be made first
//!
//! An image's name carries its digest, and the agent refuses to fetch one whose
//! name does not — that is what makes a pull verifiable. So there is no name to
//! reserve before the bytes exist, and "capture" cannot be a call that hands
//! back an image.
//!
//! ## Idempotent by construction, and write-free once done
//!
//! The image's name is derived from the label and the digest, so a pass over a
//! capture that already produced one finds it with a read and stops. Nothing is
//! recorded on the capture saying so — the node owns that status, and asking a
//! controller to write it would be two writers on one object. The link is
//! computed from the two fields that are already there, which also means it
//! cannot go stale: a capture whose image was deleted afterwards stops naming
//! one, instead of pointing at something that is gone.

use tracing::info;
use velstra_cloud_model::{
    access::Writer,
    capture::{self, CaptureSpec, CaptureStatus},
    meta::{Meta, ResourceName},
    resources::{Capture, ImageSpec, ImageStatus, Resource},
};
use velstra_cloud_store::TypedStore;

use crate::{Result, runner::Reconciler};

const WHO: &str = "capture";

pub struct CaptureController {
    images: TypedStore<ImageSpec, ImageStatus>,
    targets: TypedStore<
        velstra_cloud_model::backup::BackupTargetSpec,
        velstra_cloud_model::backup::BackupTargetStatus,
    >,
}

impl CaptureController {
    pub fn new(
        images: TypedStore<ImageSpec, ImageStatus>,
        targets: TypedStore<
            velstra_cloud_model::backup::BackupTargetSpec,
            velstra_cloud_model::backup::BackupTargetStatus,
        >,
    ) -> Self {
        Self { images, targets }
    }
}

impl Reconciler for CaptureController {
    type Spec = CaptureSpec;
    type Status = CaptureStatus;

    fn name(&self) -> &'static str {
        "capture"
    }

    async fn reconcile(&self, name: &str, object: Option<&Capture>) -> Result<()> {
        let Some(capture) = object else {
            return Ok(());
        };
        if capture.meta.is_deleting() {
            return Ok(());
        }
        // Still being copied, or never started. Nothing to make an image from,
        // and this is every pass until the node finishes.
        let Some(digest) = capture.status.digest.clone() else {
            return Ok(());
        };
        let Some(project) = capture.meta.name.parent() else {
            return Ok(());
        };
        let id = capture::image_id(&capture.spec.label, &digest);
        let full = format!("{project}/images/{id}");
        let Ok(image_name) = ResourceName::parse(&full) else {
            return Ok(());
        };

        // Already made. This is every pass after the first, so it is the path
        // that has to be cheap: one read, no writes, no create to be refused.
        if self.images.get(&full).await?.is_some() {
            return Ok(());
        }

        // Where the bytes landed. Read from the target rather than remembered
        // on the capture: a target whose path was corrected between the copy
        // and now should produce an image that points at the new one.
        let target = self.targets.get(&capture.spec.target).await?;
        let Some(target) = target else {
            return Ok(());
        };

        let image: velstra_cloud_model::resources::Image = Resource::new(
            Meta::new(image_name, capture.meta.placement.clone()),
            ImageSpec {
                // A capture is one machine's bytes at one moment, not a series
                // somebody rotates — so it joins no family, and asking for
                // `families/…` will never hand somebody a snapshot of a guest.
                from: String::new(),
                family: String::new(),
                version: String::new(),
                digest: digest.clone(),
                source_instance: Some(capture.spec.instance.clone()),
                format: velstra_cloud_model::resources::ImageFormat::Raw,
                size_bytes: capture.status.size_bytes,
                source_url: capture::image_url(&target.spec.path, &digest),
                // None, and the API would refuse it anyway: nothing in this
                // platform verifies an image signature, and a field that is
                // unused while something reports a security property from it
                // is worse than not having one.
                signature: None,
            },
            ImageStatus::default(),
        );
        match self.images.create(&image, &Writer::controller(WHO)).await {
            Ok(_) => info!(capture = %name, image = %full, "made an image from a captured guest"),
            // The same name, so the same image: an earlier pass got there, or
            // somebody captured the identical disk under the same label. Both
            // mean the object this pass wanted exists.
            Err(e) if is_taken(&e) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

fn is_taken(e: &velstra_cloud_store::typed::TypedError) -> bool {
    matches!(
        e,
        velstra_cloud_store::typed::TypedError::Store(
            velstra_cloud_store::StoreError::Exists { .. }
        )
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use velstra_cloud_model::{
        backup::{BackupTargetSpec, BackupTargetStatus, TargetKind},
        meta::{Placement, Timestamp},
    };
    use velstra_cloud_store::{MemoryStore, Store};

    use super::*;

    const CAPTURE: &str = "projects/p1/captures/golden";

    struct Fixture {
        captures: TypedStore<CaptureSpec, CaptureStatus>,
        images: TypedStore<ImageSpec, ImageStatus>,
        controller: CaptureController,
    }

    async fn fixture(digest: Option<&str>) -> Fixture {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let captures: TypedStore<CaptureSpec, CaptureStatus> =
            TypedStore::new(store.clone(), "cell-1", "captures");
        let images: TypedStore<ImageSpec, ImageStatus> =
            TypedStore::new(store.clone(), "cell-1", "images");
        let targets: TypedStore<BackupTargetSpec, BackupTargetStatus> =
            TypedStore::new(store.clone(), "cell-1", "backup-targets");

        let t: velstra_cloud_model::resources::BackupTarget = Resource::new(
            Meta::new(
                ResourceName::parse("backup-targets/shared").unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            BackupTargetSpec {
                kind: TargetKind::Directory,
                path: "/srv/images".into(),
                accepting: true,
                agent: String::new(),
                verify_every_hours: 0,
            },
            BackupTargetStatus {
                writable: Some(true),
                ..Default::default()
            },
        );
        targets
            .create(&t, &Writer::controller("test"))
            .await
            .unwrap();

        let c: Capture = Resource::new(
            Meta::new(
                ResourceName::parse(CAPTURE).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            CaptureSpec {
                instance: "projects/p1/instances/golden".into(),
                target: "backup-targets/shared".into(),
                node: "node-a".into(),
                label: "debian-13-golden".into(),
            },
            CaptureStatus {
                node: Some("node-a".into()),
                digest: digest.map(str::to_string),
                size_bytes: 1_181_116_006,
                finished_at: digest.map(|_| Timestamp::now()),
                ..Default::default()
            },
        );
        captures
            .create(&c, &Writer::controller("test"))
            .await
            .unwrap();

        let controller = CaptureController::new(images.clone(), targets);
        Fixture {
            captures,
            images,
            controller,
        }
    }

    impl Fixture {
        async fn pass(&self) {
            let c = self.captures.get(CAPTURE).await.unwrap().unwrap();
            self.controller.reconcile(CAPTURE, Some(&c)).await.unwrap();
        }

        async fn image_names(&self) -> Vec<String> {
            let mut out: Vec<String> = self
                .images
                .list()
                .await
                .unwrap()
                .into_iter()
                .map(|i| i.meta.name.id().to_string())
                .collect();
            out.sort();
            out
        }
    }

    /// A finished capture becomes an image whose name carries its digest, and a
    /// second pass makes nothing more.
    #[tokio::test]
    async fn a_finished_capture_becomes_one_image_however_often_it_is_reconciled() {
        let f = fixture(Some("sha256:abc123")).await;

        f.pass().await;
        let made = f.image_names().await;
        assert_eq!(made.len(), 1, "{made:?}");
        // Both halves: the label a person chose, and the digest that makes a
        // pull verifiable.
        assert!(made[0].starts_with("debian-13-golden-"), "{made:?}");
        assert!(made[0].contains("sha256-abc123"), "{made:?}");

        f.pass().await;
        assert_eq!(
            f.image_names().await.len(),
            1,
            "a second pass made a second image"
        );
    }

    /// The image points into the target, so any node that can reach the path
    /// can fetch it.
    #[tokio::test]
    async fn the_image_points_at_where_the_bytes_actually_landed() {
        let f = fixture(Some("sha256:abc123")).await;
        f.pass().await;
        let image = &f.images.list().await.unwrap()[0];
        assert_eq!(image.spec.source_url, "file:///srv/images/sha256-abc123");
        assert_eq!(
            image.spec.source_instance.as_deref(),
            Some("projects/p1/instances/golden"),
            "the image does not say which guest it came from"
        );
    }

    /// A capture still being made produces nothing, and writes nothing.
    ///
    /// This is every pass until the node finishes, so it is the path that has
    /// to be free.
    #[tokio::test]
    async fn a_capture_still_being_made_produces_nothing() {
        let f = fixture(None).await;
        let before = f
            .captures
            .get(CAPTURE)
            .await
            .unwrap()
            .unwrap()
            .meta
            .revision;

        f.pass().await;

        assert!(f.image_names().await.is_empty());
        let after = f
            .captures
            .get(CAPTURE)
            .await
            .unwrap()
            .unwrap()
            .meta
            .revision;
        assert_eq!(
            before, after,
            "a pass over an unfinished capture wrote something"
        );
    }

    /// Which image a capture became is *computed*, and the capture itself is
    /// never written to.
    ///
    /// The node owns that status. A controller writing "the image is over
    /// there" would be a second writer on one object — and the link would then
    /// be able to go stale, which the derived one cannot.
    #[tokio::test]
    async fn the_link_to_the_image_is_derived_and_the_capture_is_never_written() {
        let f = fixture(Some("sha256:abc123")).await;
        let before = f
            .captures
            .get(CAPTURE)
            .await
            .unwrap()
            .unwrap()
            .meta
            .revision;

        f.pass().await;

        let c = f.captures.get(CAPTURE).await.unwrap().unwrap();
        assert_eq!(c.meta.revision, before, "the controller wrote the capture");
        assert_eq!(
            velstra_cloud_model::capture::image_id(
                &c.spec.label,
                c.status.digest.as_deref().unwrap()
            ),
            "debian-13-golden-sha256-abc123",
        );
        assert!(
            f.image_names()
                .await
                .contains(&"debian-13-golden-sha256-abc123".to_string())
        );
    }
}
