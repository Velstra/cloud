//! Verify image bytes as soon as an image is published.

use std::sync::Arc;

use futures::StreamExt;
use sha2::{Digest as _, Sha256, Sha512};
use velstra_cloud_model::{
    ConditionStatus,
    images::{Algorithm, Digest},
    meta::{Condition, set_condition},
    resources::{ImageSpec, ImageStatus, Resource},
};

use crate::{Result, runner::Reconciler, status::StatusWriter};

pub const READY: &str = "Ready";

pub trait Verify: Send + Sync + 'static {
    fn verify(
        &self,
        url: &str,
        digest: &Digest,
        size_bytes: u64,
    ) -> impl std::future::Future<Output = std::result::Result<u64, String>> + Send;
}

pub struct ImageController<V: Verify> {
    status: StatusWriter<ImageSpec, ImageStatus>,
    verifier: Arc<V>,
}

impl<V: Verify> ImageController<V> {
    pub fn new(status: StatusWriter<ImageSpec, ImageStatus>, verifier: Arc<V>) -> Self {
        Self { status, verifier }
    }
}

impl<V: Verify> Reconciler for ImageController<V> {
    type Spec = ImageSpec;
    type Status = ImageStatus;

    fn name(&self) -> &'static str {
        "images"
    }

    async fn reconcile(
        &self,
        _name: &str,
        object: Option<&Resource<ImageSpec, ImageStatus>>,
    ) -> Result<()> {
        let Some(image) = object else { return Ok(()) };
        let already_verified = image.status.verified_digest == image.spec.digest
            && image.status.verified_source_url == image.spec.source_url
            && (image.spec.size_bytes == 0
                || image.status.verified_size_bytes == image.spec.size_bytes);
        if image.status.observed_generation == image.meta.generation
            && already_verified
            && image.status.conditions.iter().any(|c| c.kind == READY)
        {
            return Ok(());
        }

        let mut next = image.clone();
        next.status.observed_generation = image.meta.generation;
        let (status, reason, message) = if already_verified {
            (
                ConditionStatus::True,
                "Verified",
                format!(
                    "verified {} bytes against {}",
                    image.status.verified_size_bytes, image.spec.digest
                ),
            )
        } else if image.spec.source_instance.is_some() && image.spec.source_url.is_empty() {
            next.status.verified_digest = image.spec.digest.clone();
            next.status.verified_source_url = image.spec.source_url.clone();
            next.status.verified_size_bytes = image.spec.size_bytes;
            (
                ConditionStatus::True,
                "Captured",
                "the capture path produced and addressed these bytes".to_string(),
            )
        } else if image.spec.source_url.is_empty() {
            (
                ConditionStatus::False,
                "NoSource",
                "the image has no source URL whose bytes can be checked".to_string(),
            )
        } else if let Some(digest) = Digest::parse(&image.spec.digest) {
            match self
                .verifier
                .verify(&image.spec.source_url, &digest, image.spec.size_bytes)
                .await
            {
                Ok(bytes) => {
                    next.status.verified_digest = image.spec.digest.clone();
                    next.status.verified_source_url = image.spec.source_url.clone();
                    next.status.verified_size_bytes = bytes;
                    (
                        ConditionStatus::True,
                        "Verified",
                        format!("verified {} bytes against {}", bytes, digest.value()),
                    )
                }
                Err(why) => (ConditionStatus::False, "VerificationFailed", why),
            }
        } else {
            (
                ConditionStatus::False,
                "InvalidDigest",
                format!("{} is not a supported content digest", image.spec.digest),
            )
        };
        set_condition(
            &mut next.status.conditions,
            Condition::new(READY, status, reason, &message, image.meta.generation),
        );
        self.status.write(image, &next).await.map(|_| ())
    }
}

pub struct HttpVerifier;

impl HttpVerifier {
    pub fn new() -> std::result::Result<Self, String> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(30 * 60))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("velstra-cloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(|_| Self)
            .map_err(|e| e.to_string())
    }
}

impl Verify for HttpVerifier {
    fn verify(
        &self,
        url: &str,
        digest: &Digest,
        size_bytes: u64,
    ) -> impl std::future::Future<Output = std::result::Result<u64, String>> + Send {
        let url = url.to_string();
        let expected = digest.clone();
        async move {
            let mut current = reqwest::Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
            let mut response = None;
            for redirects in 0..=5 {
                if !matches!(current.scheme(), "http" | "https") {
                    return Err("image sources must use http or https".into());
                }
                let host = current
                    .host_str()
                    .ok_or_else(|| "the image URL has no host".to_string())?;
                let port = current
                    .port_or_known_default()
                    .ok_or_else(|| "the image URL has no usable port".to_string())?;
                let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|e| format!("could not resolve {host}: {e}"))?
                    .collect();
                if addresses.is_empty() || addresses.iter().any(|address| !public(address.ip())) {
                    return Err(format!(
                        "{host} resolves to a private, local, or otherwise non-public address; image verification cannot reach control-plane networks"
                    ));
                }
                let client = reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .timeout(std::time::Duration::from_secs(30 * 60))
                    .redirect(reqwest::redirect::Policy::none())
                    .user_agent(concat!("velstra-cloud/", env!("CARGO_PKG_VERSION")))
                    .resolve(host, addresses[0])
                    .build()
                    .map_err(|e| e.to_string())?;
                let answer = client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| format!("could not fetch {current}: {e}"))?;
                if answer.status().is_redirection() {
                    if redirects == 5 {
                        return Err("the image URL redirects more than five times".into());
                    }
                    let location = answer
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| format!("{} redirects without a Location", current))?;
                    current = current
                        .join(location)
                        .map_err(|e| format!("invalid redirect from {current}: {e}"))?;
                    continue;
                }
                response = Some(answer);
                break;
            }
            let response = response.ok_or_else(|| "the image URL did not answer".to_string())?;
            if !response.status().is_success() {
                return Err(format!("{current} answered {}", response.status()));
            }
            let mut stream = response.bytes_stream();
            let mut bytes = 0u64;
            let mut sha256 = Sha256::new();
            let mut sha512 = Sha512::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| format!("could not read {current}: {e}"))?;
                bytes = bytes.saturating_add(chunk.len() as u64);
                match expected.algorithm {
                    Algorithm::Sha256 => sha256.update(&chunk),
                    Algorithm::Sha512 => sha512.update(&chunk),
                }
            }
            if size_bytes != 0 && bytes != size_bytes {
                return Err(format!(
                    "{url} contains {bytes} bytes, but the image declares {size_bytes}"
                ));
            }
            let actual = match expected.algorithm {
                Algorithm::Sha256 => format!("{:x}", sha256.finalize()),
                Algorithm::Sha512 => format!("{:x}", sha512.finalize()),
            };
            if actual != expected.hex {
                return Err(format!(
                    "{url} does not match {}: computed {}:{}",
                    expected.value(),
                    expected.algorithm.as_str(),
                    actual
                ));
            }
            Ok(bytes)
        }
    }
}

fn public(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && octets[0] != 0
                && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
                && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                && !(octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                && octets[0] < 240
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && (segments[0] & 0xfe00) != 0xfc00
                && (segments[0] & 0xffc0) != 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use velstra_cloud_model::{
        meta::{Meta, Placement, ResourceName},
        resources::{ImageFormat, ImageState},
    };
    use velstra_cloud_store::{MemoryStore, Store, TypedStore};

    use super::*;

    struct Answer(std::result::Result<u64, String>);

    impl Verify for Answer {
        fn verify(
            &self,
            _url: &str,
            _digest: &Digest,
            _size_bytes: u64,
        ) -> impl std::future::Future<Output = std::result::Result<u64, String>> + Send {
            std::future::ready(self.0.clone())
        }
    }

    async fn checked(answer: std::result::Result<u64, String>) -> Resource<ImageSpec, ImageStatus> {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let images = TypedStore::new(store.clone(), "cell-1", "images");
        let image = Resource::new(
            Meta::new(
                ResourceName::parse(&format!("images/sha256-{}", "a".repeat(64))).unwrap(),
                Placement::new("eu", "cell-1"),
            ),
            ImageSpec {
                family: "debian-13".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                format: ImageFormat::Qcow2,
                size_bytes: 12,
                source_url: "https://example.invalid/debian.qcow2".into(),
                state: ImageState::Active,
                ..ImageSpec::default()
            },
            ImageStatus::default(),
        );
        images
            .create(&image, &velstra_cloud_model::Writer::controller("test"))
            .await
            .unwrap();
        let image = images
            .get(&image.meta.name.to_string())
            .await
            .unwrap()
            .unwrap();
        ImageController::new(
            StatusWriter::new(store, "cell-1", "images", "image-verifier"),
            Arc::new(Answer(answer)),
        )
        .reconcile("image", Some(&image))
        .await
        .unwrap();
        images
            .get(&image.meta.name.to_string())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn an_image_is_ready_only_after_its_bytes_match() {
        let image = checked(Ok(12)).await;
        let ready = image
            .status
            .conditions
            .iter()
            .find(|condition| condition.kind == READY)
            .unwrap();
        assert_eq!(ready.status, ConditionStatus::True);
        assert_eq!(ready.reason, "Verified");
    }

    #[tokio::test]
    async fn a_bad_digest_is_reported_on_the_image() {
        let image = checked(Err("computed a different digest".into())).await;
        let ready = image
            .status
            .conditions
            .iter()
            .find(|condition| condition.kind == READY)
            .unwrap();
        assert_eq!(ready.status, ConditionStatus::False);
        assert_eq!(ready.reason, "VerificationFailed");
        assert!(ready.message.contains("different digest"));
    }
}
