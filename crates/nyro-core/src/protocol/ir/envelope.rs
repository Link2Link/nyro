//! Raw request envelope — a snapshot of the original bytes / headers.
//!
//! Preserved for:
//! - Pass-through mode (body forwarded verbatim).
//! - Audit logging (what did the client actually send?).
//! - Debug round-trip verification.

use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;

/// A snapshot of the original inbound request, captured before any codec
/// transformation.
#[derive(Debug, Clone, Default)]
pub struct RawEnvelope {
    /// The parsed JSON body as received from the client.
    pub body: Option<Value>,
    /// The exact request body bytes received from the client.
    pub raw_body: Option<Bytes>,
    /// Flattened request headers (lowercase keys).
    pub headers: HashMap<String, String>,
    /// The HTTP method (e.g. `"POST"`).
    pub method: String,
    /// The request path (e.g. `"/v1/chat/completions"`).
    pub path: String,
}

impl RawEnvelope {
    pub fn new(
        body: Option<Value>,
        headers: HashMap<String, String>,
        method: &str,
        path: &str,
    ) -> Self {
        Self {
            body,
            raw_body: None,
            headers,
            method: method.to_string(),
            path: path.to_string(),
        }
    }

    /// Attach the exact request body bytes captured at the ingress boundary.
    pub fn with_raw_body(mut self, raw_body: Bytes) -> Self {
        self.raw_body = Some(raw_body);
        self
    }
}
