//! Typed, credential-free failures for the native Cursor protocol boundary.
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorError {
    InvalidRequest(String),
    Unsupported(String),
    Protocol(String),
    Decode(String),
    Compression(String),
    LimitExceeded {
        resource: &'static str,
        limit: usize,
    },
    Upstream {
        code: String,
        message: String,
    },
    MissingBlob,
    BlobHashMismatch,
    UnexpectedEof,
    AlreadyTerminated,
}

impl fmt::Display for CursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(s) => write!(f, "invalid Cursor request: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported Cursor operation: {s}"),
            Self::Protocol(s) => write!(f, "Cursor protocol error: {s}"),
            Self::Decode(s) => write!(f, "Cursor protobuf decode error: {s}"),
            Self::Compression(s) => write!(f, "Cursor gzip error: {s}"),
            Self::LimitExceeded { resource, limit } => {
                write!(f, "Cursor {resource} exceeds {limit} bytes")
            }
            Self::Upstream { code, message } => write!(f, "Cursor upstream {code}: {message}"),
            Self::MissingBlob => f.write_str("Cursor requested an unknown blob"),
            Self::BlobHashMismatch => f.write_str("Cursor blob SHA-256 mismatch"),
            Self::UnexpectedEof => f.write_str("Cursor response ended without a verified terminal"),
            Self::AlreadyTerminated => f.write_str("Cursor session is already terminated"),
        }
    }
}
impl std::error::Error for CursorError {}
impl From<prost::DecodeError> for CursorError {
    fn from(error: prost::DecodeError) -> Self {
        Self::Decode(error.to_string())
    }
}
impl From<serde_json::Error> for CursorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(format!("invalid JSON: {error}"))
    }
}
