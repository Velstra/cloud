//! One error type, two transports.
//!
//! A caller must be able to tell the same three things apart on either
//! transport: what kind of failure it was (`code`), what a person should read
//! (`message`), and which field caused it (`field`). Anything else — an HTTP
//! status, a gRPC code — is derived from those, in one place, so a REST client
//! and a gRPC client never disagree about what happened.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use velstra_cloud_model::{access::WriteRefused, meta::Revision};
use velstra_cloud_store::{StoreError, typed::TypedError};

/// The codes in `docs/rest-contract.md`, and no others. They are the gRPC
/// canonical codes, which is why the two transports can share them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    NotFound,
    AlreadyExists,
    FailedPrecondition,
    Aborted,
    ResourceExhausted,
    PermissionDenied,
    Unauthenticated,
    Internal,
}

impl Code {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::NotFound => "NOT_FOUND",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::FailedPrecondition => "FAILED_PRECONDITION",
            Self::Aborted => "ABORTED",
            Self::ResourceExhausted => "RESOURCE_EXHAUSTED",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Internal => "INTERNAL",
        }
    }

    fn http(&self) -> StatusCode {
        match self {
            Self::InvalidArgument | Self::FailedPrecondition => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            // A revision conflict and a name collision are both "somebody else
            // got there first", and 409 is the answer a client retries on.
            Self::AlreadyExists | Self::Aborted => StatusCode::CONFLICT,
            Self::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn grpc(&self) -> tonic::Code {
        match self {
            Self::InvalidArgument => tonic::Code::InvalidArgument,
            Self::NotFound => tonic::Code::NotFound,
            Self::AlreadyExists => tonic::Code::AlreadyExists,
            Self::FailedPrecondition => tonic::Code::FailedPrecondition,
            Self::Aborted => tonic::Code::Aborted,
            Self::ResourceExhausted => tonic::Code::ResourceExhausted,
            Self::PermissionDenied => tonic::Code::PermissionDenied,
            Self::Unauthenticated => tonic::Code::Unauthenticated,
            Self::Internal => tonic::Code::Internal,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub code: Code,
    pub message: String,
    /// The offending path — `spec.vcpus`, `status` — when there is one. A
    /// client that has to find the bad field by reading a sentence will get it
    /// wrong.
    pub field: Option<String>,
    /// Set on a conflict: what the object is actually at now, so the client can
    /// re-read from the answer rather than guess.
    pub revision: Option<Revision>,
}

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            revision: None,
        }
    }

    pub fn at(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidArgument, message)
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(Code::NotFound, format!("{what} does not exist"))
    }

    /// The caller is who they say they are and may not do this.
    ///
    /// Deliberately says the same thing whether the resource is theirs and
    /// forbidden or somebody else's and invisible: an error that tells the two
    /// apart is an oracle for enumerating other tenants.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(Code::PermissionDenied, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Code::Internal, message)
    }

    pub fn conflict(current: Revision) -> Self {
        Self {
            code: Code::Aborted,
            message: format!(
                "the object has moved on: it is at revision {current}, and the write named a different one"
            ),
            field: Some("meta.revision".into()),
            revision: Some(current),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Serialize)]
struct Body {
    error: Envelope,
}

#[derive(Serialize)]
struct Envelope {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.code.http();
        let body = Body {
            error: Envelope {
                code: self.code.as_str(),
                message: self.message,
                field: self.field,
                revision: self.revision.map(|r| r.to_string()),
            },
        };
        (status, Json(body)).into_response()
    }
}

impl From<ApiError> for tonic::Status {
    fn from(e: ApiError) -> Self {
        // The field goes into the message rather than into trailers: a gRPC
        // client that has to install an interceptor to learn which field was
        // wrong will not, and the sentence is what ends up in its logs.
        let message = match &e.field {
            Some(field) => format!("{} ({field})", e.message),
            None => e.message.clone(),
        };
        tonic::Status::new(e.code.grpc(), message)
    }
}

/// The resource name inside a store key.
///
/// A key is `/{cell}/{kind}/{name}`, and the first two segments are the store's
/// business. An operator reading `/cell-1/volumes/projects/p1/volumes/v1` has
/// been shown the key layout instead of an answer — and a message that leaks it
/// is one clients will start parsing.
fn named(key: &str) -> String {
    match velstra_cloud_store::parse_key(key) {
        Some((_, _, name)) => name.to_string(),
        None => key.to_string(),
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            // A create is the only thing that can hit this, and it names the
            // collection and the id far better than this can — see
            // `Api::create`. This is the sentence for when something else does.
            StoreError::Exists { key } => ApiError::new(
                Code::AlreadyExists,
                format!("{} already exists", named(&key)),
            )
            .at("id"),
            StoreError::Conflict { actual, .. } => ApiError::conflict(actual),
            StoreError::Missing { key } => ApiError::not_found(named(&key)),
            // The watcher fell behind: the events it missed are gone and the
            // only useful answer is "re-list", which is what the code says.
            StoreError::Compacted { from } => ApiError::new(
                Code::Aborted,
                format!(
                    "the watch fell behind at revision {from}; list again and watch from there"
                ),
            ),
            StoreError::Backend(m) => ApiError::internal(m),
        }
    }
}

impl From<TypedError> for ApiError {
    fn from(e: TypedError) -> Self {
        match e {
            TypedError::Store(s) => s.into(),
            // The store is the backstop for invariant 1, not the first line of
            // defence — the request layer refuses a `status` write before it
            // gets here. If one arrives anyway, it is still the client's
            // mistake and it is still named after the field it touched.
            TypedError::Refused(r) => {
                let field = match &r {
                    WriteRefused::SpecIsNotYours { .. } => Some("spec"),
                    WriteRefused::StatusIsNotYours { .. } => Some("status"),
                    WriteRefused::MetaIsNotYours { .. } => Some("meta"),
                    _ => None,
                };
                let mut err = ApiError::new(Code::InvalidArgument, r.to_string());
                err.field = field.map(str::to_string);
                err
            }
            TypedError::Corrupt { kind, source } => {
                ApiError::internal(format!("a stored {kind} could not be read: {source}"))
            }
            TypedError::Missing(name) => ApiError::not_found(name),
            // Internal, and deliberately not a 404. The object exists; this
            // installation is wired wrong. Answering "does not exist" would send
            // an operator looking for a deleted machine instead of at the two
            // cell names the message hands them.
            misplaced @ TypedError::Misplaced { .. } => ApiError::internal(misplaced.to_string()),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::invalid(format!(
            "the body is not the shape this collection takes: {e}"
        ))
    }
}

impl From<velstra_cloud_model::meta::NameError> for ApiError {
    fn from(e: velstra_cloud_model::meta::NameError) -> Self {
        ApiError::invalid(e.to_string()).at("meta.name")
    }
}
