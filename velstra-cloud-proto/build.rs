//! The proto is the source of truth, so it is compiled rather than mirrored by
//! hand. A field added to the `.proto` file appears in Rust on the next build,
//! and a field that only exists in Rust cannot exist at all.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/velstra/cloud/v1/cloud.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &["proto"])?;
    Ok(())
}
