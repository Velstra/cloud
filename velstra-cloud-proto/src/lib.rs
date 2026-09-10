//! The wire contract, generated from `proto/velstra/cloud/v1/cloud.proto`.
//!
//! Two things live here and nothing else: the generated messages and services,
//! and the conversions between them and [`velstra_cloud_model`]. Keeping the
//! conversions next to the generated code means a field added to the proto
//! fails to compile here — in one file, next to the message it belongs to —
//! rather than silently arriving on the wire as a default somewhere upstream.

pub mod convert;

pub mod v1 {
    //! The generated messages, clients and servers.
    //!
    //! `result_large_err` is allowed because every generated method returns
    //! `Result<_, tonic::Status>` and that type's size is tonic's to decide,
    //! not ours.
    #![allow(clippy::large_enum_variant, clippy::result_large_err)]
    tonic::include_proto!("velstra.cloud.v1");
}

/// The protobuf well-known types the contract uses, re-exported so a caller
/// building a `FieldMask` builds the same type this crate's messages carry.
///
/// Without this every crate that touches an update request would need its own
/// `prost-types` dependency, pinned to the same version by nothing but luck —
/// and two `FieldMask` types that are structurally identical and nominally
/// different is a compile error nobody enjoys reading.
pub use prost_types;
pub use v1::*;
