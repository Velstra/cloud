//! Generate a gRPC client for fabric's orchestrator from the vendored schema.
//!
//! Client only: this agent asks fabric to program a port and never serves
//! anything to it, and generating a server would be code nobody can call.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/vendor/velstra.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[proto], &["proto/vendor"])?;
    Ok(())
}
