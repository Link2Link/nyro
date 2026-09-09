//! Bounded, byte-exact copies for diagnostics. These helpers never alter wire data.
//!
//! Each body retains at most 1 MiB of raw bytes (512 KiB head + 512 KiB tail).
//! `body` joins those sections without a synthetic separator; metadata describes
//! the split. Base64 is used for non-UTF-8 data or a split inside a UTF-8 character,
//! so its stored string can be larger than the raw-byte limit (at most 1,398,104
//! bytes). Freeze completed captures in `Arc<CapturedPayload>` when sharing them
//! between request attempts/builders; active captures deliberately are not Clone.

use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

pub const BODY_CAPTURE_LIMIT: usize = 1024 * 1024;
pub const BODY_HEAD_LIMIT: usize = BODY_CAPTURE_LIMIT / 2;
pub const BODY_TAIL_LIMIT: usize = BODY_CAPTURE_LIMIT / 2;
/// One byte below 64 KiB so serialized headers also fit MySQL TEXT (65,535 bytes).
pub const HEADER_CAPTURE_LIMIT: usize = 64 * 1024 - 1;

/// A finished logging copy. Store this behind an Arc for cheap builder clones.
#[derive(Debug)]
pub struct CapturedPayload {
    pub body: Option<String>,
    pub metadata: Value,
}

impl Default for CapturedPayload {
    fn default() -> Self {
        BoundedPayloadCapture::new().finish(false)
    }
}

/// One independently bounded body capture. `push` observes even an empty slice,
/// distinguishing an explicitly empty body from a capture that never saw data.
#[derive(Debug, Default)]
pub struct BoundedPayloadCapture {
    bytes: Vec<u8>,
    tail_start: usize,
    total_observed_bytes: u64,
    observed: bool,
    utf8: Utf8Validation,
}

impl BoundedPayloadCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, mut bytes: &[u8]) {
        self.observed = true;
        self.total_observed_bytes = self.total_observed_bytes.saturating_add(bytes.len() as u64);
        self.utf8.push(bytes);

        let capacity_needed = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .min(BODY_CAPTURE_LIMIT);
        if capacity_needed > self.bytes.capacity() {
            // Geometric growth for small streaming writes, never above 1 MiB.
            let capacity = capacity_needed
                .next_power_of_two()
                .max(1024)
                .min(BODY_CAPTURE_LIMIT);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        let head_count = bytes
            .len()
            .min(BODY_HEAD_LIMIT.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..head_count]);
        bytes = &bytes[head_count..];
        if bytes.is_empty() {
            return;
        }
        // Large writes discard their middle before copying anything. The head
        // and tail live in one allocation so finish can move it into a String.
        if bytes.len() >= BODY_TAIL_LIMIT {
            self.bytes.truncate(BODY_HEAD_LIMIT);
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - BODY_TAIL_LIMIT..]);
            self.tail_start = 0;
            return;
        }
        let append_count = bytes.len().min(BODY_CAPTURE_LIMIT - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..append_count]);
        bytes = &bytes[append_count..];
        if !bytes.is_empty() {
            let first_count = bytes.len().min(BODY_TAIL_LIMIT - self.tail_start);
            let tail = &mut self.bytes[BODY_HEAD_LIMIT..];
            tail[self.tail_start..self.tail_start + first_count]
                .copy_from_slice(&bytes[..first_count]);
            let remainder = bytes.len() - first_count;
            tail[..remainder].copy_from_slice(&bytes[first_count..]);
            self.tail_start = (self.tail_start + bytes.len()) % BODY_TAIL_LIMIT;
        }
    }

    pub fn total_observed_bytes(&self) -> u64 {
        self.total_observed_bytes
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// `complete` reports transport completeness, independently of truncation.
    pub fn finish(mut self, complete: bool) -> CapturedPayload {
        let retained = self.retained_bytes();
        let truncated = self.total_observed_bytes > retained as u64;
        if self.tail_start != 0 {
            self.bytes[BODY_HEAD_LIMIT..].rotate_left(self.tail_start);
        }
        let head_bytes = retained.min(BODY_HEAD_LIMIT);
        let tail_bytes = retained - head_bytes;
        // Do not allow two broken UTF-8 edges to accidentally form a valid
        // character when the missing middle is removed.
        let split_utf8 = !truncated
            || (std::str::from_utf8(&self.bytes[..head_bytes]).is_ok()
                && std::str::from_utf8(&self.bytes[head_bytes..]).is_ok());
        let state = if !self.observed {
            "absent"
        } else if retained == 0 {
            "empty"
        } else {
            "captured"
        };
        let encoding = if !self.observed {
            "none"
        } else if self.utf8.is_complete() && split_utf8 {
            "utf8"
        } else {
            "base64"
        };
        let body = if !self.observed {
            None
        } else {
            // UTF-8 finish reuses the capture allocation (no additional body
            // copy). Base64 allocates only its bounded encoded representation.
            Some(if encoding == "utf8" {
                String::from_utf8(self.bytes).expect("incrementally validated UTF-8")
            } else {
                STANDARD.encode(self.bytes)
            })
        };
        CapturedPayload {
            body,
            metadata: json!({
                "total_observed_bytes": self.total_observed_bytes,
                "retained_bytes": retained,
                "head_bytes": head_bytes,
                "tail_bytes": tail_bytes,
                "truncated": truncated,
                "complete": complete,
                "encoding": encoding,
                "capture_state": state,
            }),
        }
    }
}

impl Write for BoundedPayloadCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.push(bytes);
        // The serializer/wire producer must see ALL input as consumed, not only
        // the retained bytes. Capture limits never become transport limits.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn capture_bytes(bytes: &[u8], complete: bool) -> CapturedPayload {
    let mut capture = BoundedPayloadCapture::new();
    capture.push(bytes);
    capture.finish(complete)
}

/// Serialize directly into bounded storage, not via an unbounded String/Vec.
/// The caller's existing JSON value remains the authoritative wire/parser data.
pub fn capture_json(value: &Value, complete: bool) -> CapturedPayload {
    let mut capture = BoundedPayloadCapture::new();
    let serialized = serde_json::to_writer(&mut capture, value).is_ok();
    capture.finish(complete && serialized)
}

/// A redacted, valid JSON header object capped at 64 KiB including JSON escaping.
#[derive(Debug)]
pub struct CapturedHeaders {
    pub headers: Option<String>,
    pub metadata: Value,
}

impl Default for CapturedHeaders {
    fn default() -> Self {
        Self {
            headers: None,
            metadata: json!({
                "total_observed_bytes": 0,
                "retained_bytes": 0,
                "truncated": false,
                "complete": false,
                "encoding": "none",
                "capture_state": "absent",
                "total_headers": 0,
                "retained_headers": 0,
                "omitted_headers": 0,
                "redacted_headers": 0,
            }),
        }
    }
}

/// Header credentials are matched case-insensitively, including common custom
/// token/password names rather than only a fixed list of vendor API keys.
pub fn is_sensitive_header(name: &str) -> bool {
    let exact = [
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "x-goog-api-key",
        "openai-api-key",
        "anthropic-api-key",
    ];
    exact.iter().any(|key| name.eq_ignore_ascii_case(key))
        || [
            "token",
            "password",
            "passwd",
            "secret",
            "api-key",
            "api_key",
            "apikey",
            "credential",
        ]
        .iter()
        .any(|part| contains_ascii_case_insensitive(name, part))
}

pub fn is_sensitive_url_key(name: &str) -> bool {
    name.eq_ignore_ascii_case("key")
        || name.eq_ignore_ascii_case("auth")
        || is_sensitive_header(name)
}

fn contains_ascii_case_insensitive(value: &str, part: &str) -> bool {
    value
        .as_bytes()
        .windows(part.len())
        .any(|window| window.eq_ignore_ascii_case(part.as_bytes()))
}

fn header_priority(name: &str) -> u8 {
    if [
        "content-type",
        "content-length",
        "content-encoding",
        "transfer-encoding",
        "accept",
        "user-agent",
        "host",
        "retry-after",
        "request-id",
        "x-request-id",
        "x-correlation-id",
        "traceparent",
        "tracestate",
        "anthropic-version",
        "anthropic-beta",
        "openai-version",
        "openai-processing-ms",
    ]
    .iter()
    .any(|key| name.eq_ignore_ascii_case(key))
        || contains_ascii_case_insensitive(name, "ratelimit")
    {
        0
    } else if is_sensitive_header(name) {
        1
    } else {
        2
    }
}

/// Copy headers only after redaction and an escaped-JSON size check. Oversized
/// fields are omitted whole, not serialized into a temporary first. Essential
/// protocol/request-id/rate-limit fields are considered before other fields.
/// Observed bytes count original header names + values (without HTTP framing);
/// retained bytes count the redacted JSON, including braces/quotes/escaping.
pub fn capture_header_pairs<'a, I, F>(pairs: F) -> CapturedHeaders
where
    I: Iterator<Item = (&'a str, &'a [u8])>,
    F: Fn() -> I,
{
    let mut output = Vec::with_capacity(HEADER_CAPTURE_LIMIT);
    output.push(b'{');
    let mut total_observed_bytes = 0u64;
    let mut total_headers = 0usize;
    let mut retained_headers = 0usize;
    let mut redacted_headers = 0usize;
    for (name, value) in pairs() {
        total_observed_bytes = total_observed_bytes
            .saturating_add(name.len() as u64)
            .saturating_add(value.len() as u64);
        total_headers += 1;
    }
    for priority in 0..=2 {
        for (name, bytes) in pairs() {
            if header_priority(name) != priority {
                continue;
            }
            let redacted = is_sensitive_header(name);
            let value = if redacted {
                Ok("***")
            } else {
                std::str::from_utf8(bytes)
            };
            let key_len = json_string_len(name);
            let value_len = match value {
                Ok(value) => json_string_len(value),
                Err(_) => bytes.len().saturating_mul(2).saturating_add(4), // "0x..."
            };
            let needed = key_len
                .saturating_add(value_len)
                .saturating_add(1)
                .saturating_add(usize::from(retained_headers != 0));
            if needed > HEADER_CAPTURE_LIMIT - output.len() - 1 {
                continue;
            }
            if retained_headers != 0 {
                output.push(b',');
            }
            // These copies only occur after their escaped size fits the budget.
            serde_json::to_writer(&mut output, &name.to_ascii_lowercase())
                .expect("header name serialization into Vec");
            output.push(b':');
            match value {
                Ok(value) => serde_json::to_writer(&mut output, value)
                    .expect("header value serialization into Vec"),
                Err(_) => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    output.extend_from_slice(b"\"0x");
                    for byte in bytes {
                        output.push(HEX[(byte >> 4) as usize]);
                        output.push(HEX[(byte & 15) as usize]);
                    }
                    output.push(b'"');
                }
            }
            retained_headers += 1;
            redacted_headers += usize::from(redacted);
        }
    }
    output.push(b'}');
    let retained_bytes = output.len();
    CapturedHeaders {
        headers: Some(String::from_utf8(output).expect("JSON header object is UTF-8")),
        metadata: json!({
            "total_observed_bytes": total_observed_bytes,
            "retained_bytes": retained_bytes,
            "truncated": total_headers != retained_headers,
            "complete": true,
            "encoding": "utf8",
            "capture_state": if total_headers == 0 { "empty" } else { "captured" },
            "total_headers": total_headers,
            "retained_headers": retained_headers,
            "omitted_headers": total_headers - retained_headers,
            "redacted_headers": redacted_headers,
        }),
    }
}

pub fn capture_headers(headers: &axum::http::HeaderMap) -> CapturedHeaders {
    capture_header_pairs(|| {
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
    })
}

pub fn capture_header_map(headers: &std::collections::HashMap<String, String>) -> CapturedHeaders {
    capture_header_pairs(|| {
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
    })
}

fn json_string_len(value: &str) -> usize {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).expect("infallible JSON length counter");
    counter.0
}

/// Validate all observed bytes (including omitted middle bytes), while keeping
/// at most one incomplete UTF-8 code point across arbitrary chunk boundaries.
#[derive(Debug, Default)]
struct Utf8Validation {
    invalid: bool,
    pending: [u8; 4],
    pending_len: usize,
}

impl Utf8Validation {
    fn push(&mut self, mut bytes: &[u8]) {
        if self.invalid {
            return;
        }
        if self.pending_len != 0 {
            let width = match self.pending[0] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => unreachable!("from_utf8 only permits valid incomplete prefixes"),
            };
            let copied = bytes.len().min(width - self.pending_len);
            self.pending[self.pending_len..self.pending_len + copied]
                .copy_from_slice(&bytes[..copied]);
            self.pending_len += copied;
            bytes = &bytes[copied..];
            if self.pending_len < width {
                return;
            }
            if std::str::from_utf8(&self.pending[..width]).is_err() {
                self.invalid = true;
                return;
            }
            self.pending_len = 0;
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                self.invalid = true;
            } else {
                let remainder = &bytes[error.valid_up_to()..];
                self.pending[..remainder.len()].copy_from_slice(remainder);
                self.pending_len = remainder.len();
            }
        }
    }

    fn is_complete(&self) -> bool {
        !self.invalid && self.pending_len == 0
    }
}
