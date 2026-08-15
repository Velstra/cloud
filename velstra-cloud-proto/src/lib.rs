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

pub use v1::*;
