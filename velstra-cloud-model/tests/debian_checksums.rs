//! Debian's own `SHA512SUMS`, read as the platform reads it.
//!
//! Not a fixture somebody typed: the file this test carries was fetched from
//! `cloud.debian.org` and is checked in beside it, because the reason the
//! reader was wrong for months is that every test used a hand-written line.
use velstra_cloud_model::images::{Algorithm, Digest, digest_for};

const DEBIAN: &str = include_str!("debian-SHA512SUMS.txt");

#[test]
fn debians_own_checksums_file_names_a_sha512_for_its_cloud_image() {
    let name = "debian-13-genericcloud-amd64.qcow2";
    let value = digest_for(DEBIAN, name).expect("Debian's file names this image");
    let digest = Digest::parse(&value).expect("and it parses as a digest");
    assert_eq!(digest.algorithm, Algorithm::Sha512);
    assert_eq!(digest.hex.len(), 128);
    assert!(value.starts_with("sha512:"), "{value}");
    // What a node would file it under, and what an image object would be
    // called. Both come off the same digest so they cannot disagree.
    assert_eq!(digest.stored(), format!("sha512-{}", digest.hex));

    // A name the file does not cover still says nothing.
    assert_eq!(digest_for(DEBIAN, "debian-13-nonesuch.qcow2"), None);
}
