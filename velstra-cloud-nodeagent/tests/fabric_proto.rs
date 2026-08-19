//! The vendored copy of fabric's service contract, checked rather than hoped for.
//!
//! This agent speaks to fabric's orchestrator over gRPC, so it needs the schema.
//! It is a copy (see `proto/vendor/README.md` for why a dependency would not do),
//! and the one honest cost of a copy is that it can fall behind the original
//! without anybody noticing until a field means something else.

use std::path::{Path, PathBuf};

fn vendored() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("proto/vendor/velstra.proto")
}

fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

#[test]
fn the_vendored_copy_is_the_file_its_digest_says_it_is() {
    let recorded = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("proto/vendor/velstra.proto.sha256"),
    )
    .expect("the recorded digest");
    let actual = digest(&std::fs::read(vendored()).expect("the vendored proto"));
    assert_eq!(
        actual,
        recorded.trim(),
        "the vendored proto was edited in place. It is somebody else's contract: change it in \
         fabric, copy it here, and move the digest with it"
    );
}

#[test]
fn the_vendored_copy_matches_fabric_when_fabric_is_here() {
    // Skips loudly rather than failing. A red test on a machine that simply does
    // not have the other repository checked out is a test people learn to
    // scroll past, and this one has real work to do on the machines that do.
    let candidates = [
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fabric/velstra-proto/proto/velstra.proto"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fabric/velstra-proto/proto/velstra.proto"),
    ];
    let Some(theirs) = candidates.iter().find(|p| p.exists()) else {
        eprintln!("skipped: the fabric repository is not checked out beside this one");
        return;
    };
    let theirs = std::fs::read(theirs).expect("fabric's proto");
    let ours = std::fs::read(vendored()).expect("the vendored proto");
    assert_eq!(
        digest(&ours),
        digest(&theirs),
        "the vendored copy has fallen behind fabric's. Refresh it:\n    \
         cp ../fabric/velstra-proto/proto/velstra.proto \
         velstra-cloud-nodeagent/proto/vendor/velstra.proto"
    );
}
