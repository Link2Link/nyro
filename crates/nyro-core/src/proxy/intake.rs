//! Request intake layer.
//!
//! Responsibilities (single-concern):
//! - Extract and validate the raw request body.
//! - Extract the `model` field from the body.
//! - Serialize headers for logging.
//! - Provide the `request_id` from `RequestContext`.
//!
//! This module has NO knowledge of auth, routing, or upstream calls.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequest, Request, rejection::JsonRejection};
use axum::http::{HeaderMap, header};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::Value;

use crate::error::GatewayError;
use crate::proxy::context::RequestContext;
use crate::proxy::observability::headers_to_json;

/// A JSON request body together with the exact bytes consumed by Axum.
#[derive(Debug, Clone)]
pub struct JsonIntake {
    pub value: Value,
    pub raw: Bytes,
}

#[async_trait]
impl<S> FromRequest<S> for JsonIntake
where
    S: Send + Sync + 'static,
{
    type Rejection = JsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let started_at = Instant::now();
        let (mut parts, body) = req.into_parts();
        // Wire snapshot taken before `parts` is consumed by the inner `Json`
        // extractor: intake-rejection logging needs the original view (the
        // compressed path strips content headers before re-invoking `Json`).
        let wire = WireMeta {
            started_at: parts
                .extensions
                .get::<RequestContext>()
                .map_or(started_at, |ctx| ctx.started_at),
            method: parts.method.clone(),
            path: parts.uri.path().to_string(),
            headers: parts.headers.clone(),
            context: parts
                .extensions
                .get::<crate::proxy::context::RequestContext>()
                .cloned(),
        };
        let encoding = request_content_encoding(&parts.headers);
        if encoding
            .as_deref()
            .is_some_and(nyro_ccswitch_compat::is_supported_content_encoding)
        {
            let (body, captured) = observe_body(body, false);
            // Unlike to_bytes(usize::MAX), the Bytes extractor enforces the
            // request's DefaultBodyLimit and preserves read/limit rejections.
            let compressed =
                match Bytes::from_request(Request::from_parts(parts.clone(), body), state).await {
                    Ok(bytes) => bytes,
                    Err(rejection) => {
                        let rejection = JsonRejection::BytesRejection(rejection);
                        let capture = captured
                            .lock()
                            .expect("request body capture mutex poisoned");
                        report_intake_rejection(
                            state,
                            &wire,
                            capture.payload(&wire, "wire", None),
                            &rejection,
                            None,
                        );
                        return Err(rejection);
                    }
                };
            // Bounded decode: a compressed body within the transport limit can
            // still expand without bound (decompression bomb), so expansion is
            // capped at the same request-body limit. Oversized payloads fall
            // through to the JSON rejection path instead of exhausting memory.
            let decoded = nyro_ccswitch_compat::decompress_body_with_limit(
                encoding.as_deref().unwrap(),
                &compressed,
                crate::proxy::server::PROXY_JSON_BODY_LIMIT_BYTES,
            );
            match decoded {
                Ok(Some(decompressed)) => {
                    parts.headers.remove(header::CONTENT_ENCODING);
                    parts.headers.remove(header::CONTENT_LENGTH);
                    parts.headers.remove(header::TRANSFER_ENCODING);
                    let decoded = Bytes::from(decompressed);
                    let decoded_for_json = decoded.clone();
                    let req = Request::from_parts(parts, Body::from(decoded_for_json));
                    let Json(value) = match Json::<Value>::from_request(req, state).await {
                        Ok(v) => v,
                        Err(rejection) => {
                            let wire_capture = captured
                                .lock()
                                .expect("request body capture mutex poisoned");
                            let mut decoded_capture = IntakeCapture::default();
                            decoded_capture.observe(&decoded, false);
                            decoded_capture.complete = wire_capture.complete;
                            report_intake_rejection(
                                state,
                                &wire,
                                decoded_capture.payload(
                                    &wire,
                                    "decoded",
                                    Some(wire_capture.total_observed_bytes),
                                ),
                                &rejection,
                                None,
                            );
                            return Err(rejection);
                        }
                    };
                    return Ok(Self {
                        value,
                        raw: decoded,
                    });
                }
                decode_result => {
                    let failure = match decode_result {
                        Err(nyro_ccswitch_compat::DecompressError::TooLarge { .. }) => Some((
                            "intake_body_limit",
                            "intake_decompress",
                            "The decoded client request body exceeded the size limit.",
                        )),
                        Err(_) => Some((
                            "intake_decompress_error",
                            "intake_decompress",
                            "The encoded client request body could not be decompressed.",
                        )),
                        _ => None,
                    };
                    let req = Request::from_parts(parts, Body::from(compressed.clone()));
                    let Json(value) = match Json::<Value>::from_request(req, state).await {
                        Ok(v) => v,
                        Err(rejection) => {
                            let capture = captured
                                .lock()
                                .expect("request body capture mutex poisoned");
                            report_intake_rejection(
                                state,
                                &wire,
                                capture.payload(&wire, "wire", None),
                                &rejection,
                                failure,
                            );
                            return Err(rejection);
                        }
                    };
                    return Ok(Self {
                        value,
                        raw: compressed,
                    });
                }
            }
        }

        // The full raw copy is needed by successful JsonIntake consumers, not
        // diagnostic retention. The separate logging capture only keeps 512 bytes.
        let (body, captured) = observe_body(body, true);
        let req = Request::from_parts(parts, body);

        let Json(value) = match Json::<Value>::from_request(req, state).await {
            Ok(v) => v,
            Err(rejection) => {
                let capture = captured
                    .lock()
                    .expect("request body capture mutex poisoned");
                report_intake_rejection(
                    state,
                    &wire,
                    capture.payload(&wire, "wire", None),
                    &rejection,
                    None,
                );
                return Err(rejection);
            }
        };
        let raw = std::mem::take(
            &mut captured
                .lock()
                .expect("request body capture mutex poisoned")
                .raw,
        )
        .freeze();

        Ok(Self { value, raw })
    }
}

/// Original wire metadata kept for intake-rejection logging.
struct WireMeta {
    started_at: Instant,
    method: axum::http::Method,
    path: String,
    headers: axum::http::HeaderMap,
    context: Option<crate::proxy::context::RequestContext>,
}

/// Downcast the router state to [`crate::Gateway`] and emit an
/// intake-rejection log entry through the dispatcher. No-op when the
/// state is not a `Gateway` (extractor unit tests, foreign routers); the
/// rejection itself propagates unchanged either way.
fn report_intake_rejection<S>(
    state: &S,
    wire: &WireMeta,
    body: crate::logging::payload::CapturedPayload,
    rejection: &JsonRejection,
    failure: Option<(&'static str, &'static str, &'static str)>,
) where
    S: 'static,
{
    let any_state: &dyn std::any::Any = state;
    if let Some(gw) = any_state.downcast_ref::<crate::Gateway>() {
        let (kind, stage, cause) = failure.unwrap_or_else(|| match rejection {
            JsonRejection::BytesRejection(_)
                if rejection.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE =>
            {
                (
                    "intake_body_limit",
                    "intake_read",
                    "The client request body exceeded the size limit.",
                )
            }
            JsonRejection::BytesRejection(_) => (
                "intake_read_error",
                "intake_read",
                "The client request body could not be read to completion.",
            ),
            JsonRejection::MissingJsonContentType(_) => (
                "intake_content_type",
                "intake_headers",
                "The client request did not specify a JSON content type.",
            ),
            _ => (
                "intake_parse_error",
                "intake_parse",
                "The client request body could not be parsed as JSON.",
            ),
        });
        crate::proxy::dispatcher::log_intake_rejection(
            gw,
            crate::proxy::dispatcher::IntakeRejectionLog {
                method: &wire.method,
                path: &wire.path,
                headers: &wire.headers,
                status: rejection.status(),
                rejection_text: &rejection.body_text(),
                context: wire.context.as_ref(),
                body,
                started_at: wire.started_at,
                failure_kind: kind,
                failure_stage: stage,
                safe_cause: cause,
            },
        );
    }
}

/// Intake diagnostics retain a prefix only, independently of the raw bytes
/// needed by successful requests. Observed counts never come from Content-Length.
#[derive(Default)]
struct IntakeCapture {
    prefix: Vec<u8>,
    total_observed_bytes: u64,
    observed: bool,
    complete: bool,
    raw: BytesMut,
}

impl IntakeCapture {
    fn observe(&mut self, bytes: &[u8], retain_raw: bool) {
        self.observed = true;
        self.total_observed_bytes = self.total_observed_bytes.saturating_add(bytes.len() as u64);
        let remaining = crate::proxy::dispatcher::INTAKE_REJECTION_BODY_PREFIX_BYTES
            .saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if retain_raw {
            self.raw.extend_from_slice(bytes);
        }
    }

    fn payload(
        &self,
        wire: &WireMeta,
        byte_domain: &str,
        wire_observed_bytes: Option<u64>,
    ) -> crate::logging::payload::CapturedPayload {
        // capture_bytes preserves the retained bytes exactly, using base64 for
        // binary data or a UTF-8 character split at the prefix boundary.
        let mut capture = if self.observed || self.complete {
            crate::logging::payload::capture_bytes(&self.prefix, self.complete)
        } else {
            crate::logging::payload::CapturedPayload::default()
        };
        capture.metadata["total_observed_bytes"] = self.total_observed_bytes.into();
        capture.metadata["truncated"] =
            (self.total_observed_bytes > self.prefix.len() as u64).into();
        capture.metadata["byte_domain"] = byte_domain.into();
        capture.metadata["encoding_scope"] = "retained_prefix".into();
        if let Some(declared) = wire
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            // Content-Length always describes the wire representation, even
            // when the prefix and observed count describe decoded JSON bytes.
            capture.metadata["declared_bytes"] = declared.into();
            capture.metadata["declared_byte_domain"] = "wire".into();
        }
        if let Some(observed) = wire_observed_bytes {
            capture.metadata["wire_observed_bytes"] = observed.into();
        }
        capture
    }
}

fn observe_body(body: Body, retain_raw: bool) -> (Body, Arc<Mutex<IntakeCapture>>) {
    let captured = Arc::new(Mutex::new(IntakeCapture::default()));
    let capture = Arc::clone(&captured);
    let mut stream = body.into_data_stream();
    let mut failed = false;
    let body = Body::from_stream(futures::stream::poll_fn(move |cx| {
        let polled = stream.poll_next_unpin(cx);
        let mut capture = capture.lock().expect("request body capture mutex poisoned");
        match &polled {
            std::task::Poll::Ready(Some(Ok(bytes))) => capture.observe(bytes, retain_raw),
            std::task::Poll::Ready(Some(Err(_))) => failed = true,
            std::task::Poll::Ready(None) if !failed => capture.complete = true,
            _ => {}
        }
        polled
    }));
    (body, captured)
}

fn request_content_encoding(headers: &HeaderMap) -> Option<String> {
    let combined = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    (!combined.is_empty()).then_some(combined.to_ascii_lowercase())
}

/// Result of the intake phase.
pub struct IntakeResult {
    /// The parsed request body.
    pub body: Value,
    /// The `model` field extracted from the body (trimmed, non-empty).
    pub model: String,
    /// Serialized headers for logging.
    pub request_headers_str: Option<String>,
    /// Serialized body for logging.
    pub request_body_str: Option<String>,
}

/// Parse and validate a standard chat / responses / messages / generate
/// ingress body.
///
/// Returns `Err(GatewayError)` if the body is missing the `model` field.
pub fn intake_body(headers: &HeaderMap, body: Value) -> Result<IntakeResult, GatewayError> {
    let request_headers_str = headers_to_json(headers);
    let request_body_str = serde_json::to_string(&body).ok();

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| GatewayError::bad_request("model_required", "model is required"))?;

    Ok(IntakeResult {
        body,
        model,
        request_headers_str,
        request_body_str,
    })
}

/// Extract the `model` from a body, returning an error if absent.
///
/// Used by ingress paths that perform their own decoding before calling the
/// pipeline (e.g. embeddings and Gemini which need a pre-decode step).
pub fn extract_model(body: &Value) -> Result<String, GatewayError> {
    body.get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| GatewayError::bad_request("model_required", "model is required"))
}

/// Stamp the request_id header on an outbound response for client correlation.
pub fn stamp_request_id(response: &mut axum::response::Response, ctx: &RequestContext) {
    if let Ok(value) = axum::http::HeaderValue::from_str(&ctx.request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request as HttpRequest, StatusCode, header};
    use axum::response::IntoResponse;
    use serde_json::json;

    use super::*;

    fn request(body: &'static [u8], content_type: Option<&str>) -> Request {
        let mut builder = HttpRequest::builder().method("POST").uri("/");
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        builder.body(Body::from(body)).expect("valid test request")
    }

    fn encoded_request(body: Vec<u8>, content_encoding: &'static str) -> Request {
        HttpRequest::builder()
            .method("POST")
            .uri("/")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, content_encoding)
            .body(Body::from(body))
            .expect("valid encoded test request")
    }

    async fn assert_rejection_matches_axum(
        body: &'static [u8],
        content_type: Option<&str>,
        expected_status: StatusCode,
    ) {
        let intake = JsonIntake::from_request(request(body, content_type), &())
            .await
            .expect_err("intake request should be rejected")
            .into_response();
        let axum = Json::<Value>::from_request(request(body, content_type), &())
            .await
            .expect_err("Axum JSON request should be rejected")
            .into_response();

        assert_eq!(intake.status(), expected_status);
        assert_eq!(intake.status(), axum.status());
        assert_eq!(
            intake.headers().get(header::CONTENT_TYPE),
            axum.headers().get(header::CONTENT_TYPE)
        );
        let intake_body = to_bytes(intake.into_body(), usize::MAX)
            .await
            .expect("read intake rejection body");
        let axum_body = to_bytes(axum.into_body(), usize::MAX)
            .await
            .expect("read Axum rejection body");
        assert_eq!(intake_body, axum_body);
    }

    #[tokio::test]
    async fn preserves_valid_json_bytes_and_parsed_value() {
        let raw = br#"{
  "z": 1,
  "a": [ true, null ]
}
"#;

        let intake = JsonIntake::from_request(request(raw, Some("application/json")), &())
            .await
            .expect("valid JSON intake");

        assert_eq!(intake.raw.as_ref(), raw);
        assert_eq!(intake.value, json!({"z": 1, "a": [true, null]}));
    }

    #[tokio::test]
    async fn decompresses_supported_request_content_encoding_before_json_parse() {
        let raw = br#"{ "z": 1, "model": "gpt-test", "unknown": true }"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, raw).unwrap();
        let compressed = encoder.finish().unwrap();

        let intake = JsonIntake::from_request(encoded_request(compressed, "gzip"), &())
            .await
            .expect("compressed JSON intake");

        assert_eq!(intake.raw.as_ref(), raw);
        assert_eq!(intake.value["model"], "gpt-test");
    }

    #[tokio::test]
    async fn request_decompression_bomb_is_rejected_without_expansion() {
        // A gzip bomb expanding past the request-body limit must be rejected by
        // the bounded decoder instead of being fully expanded into memory; the
        // compressed bytes then fail JSON parsing exactly like any other
        // malformed body.
        let payload = vec![0u8; crate::proxy::server::PROXY_JSON_BODY_LIMIT_BYTES + 1];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &payload).unwrap();
        let compressed = encoder.finish().unwrap();
        drop(payload);
        assert!(compressed.len() < 1024 * 1024);

        let compressed_len = compressed.len();
        let (status, entry) =
            finalized_rejection_log(encoded_request(compressed, "gzip"), Some(Instant::now()))
                .await;
        // Keep the established JSON-rejection response, but diagnose the
        // bounded decode failure rather than falsely claiming a parser error.
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_body_limit")
        );
        assert_eq!(
            entry.diagnostic.failure_stage.as_deref(),
            Some("intake_decompress")
        );
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["byte_domain"], "wire");
        assert_eq!(meta["total_observed_bytes"], compressed_len);
        assert_eq!(meta["retained_bytes"], 512);
        assert_eq!(meta["complete"], true);
        assert_eq!(meta["truncated"], true);
        // The decoder returned no decoded body: do not invent an output count.
        assert!(meta.get("wire_observed_bytes").is_none());
    }

    #[tokio::test]
    async fn unsupported_request_content_encoding_keeps_axum_json_rejection() {
        let compressed = b"not-json".to_vec();
        let intake = JsonIntake::from_request(encoded_request(compressed.clone(), "snappy"), &())
            .await
            .expect_err("unsupported encoded JSON intake");
        let axum = Json::<Value>::from_request(encoded_request(compressed, "snappy"), &())
            .await
            .expect_err("Axum JSON intake");

        let intake = intake.into_response();
        let axum = axum.into_response();
        assert_eq!(intake.status(), axum.status());
        assert_eq!(
            intake.headers().get(header::CONTENT_TYPE),
            axum.headers().get(header::CONTENT_TYPE)
        );
        let intake_body = to_bytes(intake.into_body(), usize::MAX).await.unwrap();
        let axum_body = to_bytes(axum.into_body(), usize::MAX).await.unwrap();
        assert_eq!(intake_body, axum_body);
    }

    #[tokio::test]
    async fn invalid_json_rejection_matches_axum_json() {
        assert_rejection_matches_axum(
            br#"{"model": }"#,
            Some("application/json"),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    #[tokio::test]
    async fn missing_content_type_rejection_matches_axum_json() {
        assert_rejection_matches_axum(
            br#"{"model":"gpt-test"}"#,
            None,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
    }

    #[tokio::test]
    async fn wrong_content_type_rejection_matches_axum_json() {
        assert_rejection_matches_axum(
            br#"{"model":"gpt-test"}"#,
            Some("text/plain"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
    }

    #[tokio::test]
    async fn empty_body_rejection_matches_axum_json() {
        assert_rejection_matches_axum(b"", Some("application/json"), StatusCode::BAD_REQUEST).await;
    }
    async fn log_test_gateway() -> (
        crate::Gateway,
        tokio::sync::mpsc::Receiver<crate::logging::LogEntry>,
    ) {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-intake-log-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        crate::Gateway::new(config)
            .await
            .expect("gateway init for intake log test")
    }

    fn uri_request(body: &'static [u8], uri: &str) -> Request {
        HttpRequest::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("valid test request")
    }

    async fn next_log_entry(
        log_rx: &mut tokio::sync::mpsc::Receiver<crate::logging::LogEntry>,
    ) -> crate::logging::LogEntry {
        tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("intake rejection should be logged")
            .expect("log channel should remain open")
    }

    async fn finalized_rejection_log(
        mut req: Request,
        started_at: Option<Instant>,
    ) -> (StatusCode, crate::logging::LogEntry) {
        let (gw, mut log_rx) = log_test_gateway().await;
        let context = started_at.map(|started_at| {
            let mut ctx = RequestContext::without_total_timeout(
                crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            );
            ctx.started_at = started_at;
            req.extensions_mut().insert(ctx.clone());
            ctx
        });
        let rejection = JsonIntake::from_request(req, &gw)
            .await
            .expect_err("invalid intake");
        let status = rejection.status();
        if let Some(ctx) = context {
            let response = crate::proxy::dispatcher::finish_preflight_response(
                rejection.into_response(),
                &ctx,
            );
            to_bytes(response.into_body(), usize::MAX).await.unwrap();
        }
        let entry = next_log_entry(&mut log_rx).await;
        assert!(entry.upstream_status_code.is_none());
        assert!(entry.upstream_url.is_none());
        assert!(entry.provider_id.is_empty());
        assert_eq!(entry.diagnostic.attempt_outcome, "failed");
        (status, entry)
    }

    fn streaming_request(body: Body, encoding: Option<&str>, declared: Option<u64>) -> Request {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(encoding) = encoding {
            builder = builder.header(header::CONTENT_ENCODING, encoding);
        }
        if let Some(declared) = declared {
            builder = builder.header(header::CONTENT_LENGTH, declared.to_string());
        }
        builder.body(body).unwrap()
    }

    #[tokio::test]
    async fn intake_complete_invalid_json_logs_real_counts_and_entry_latency() {
        let mut raw = vec![b' '; 2048];
        raw[0] = b'!';
        let req = streaming_request(Body::from(raw), None, Some(2048));
        let (status, entry) = finalized_rejection_log(
            req,
            Some(Instant::now() - std::time::Duration::from_millis(75)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(entry.latency_total_ms >= 75);
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_parse_error")
        );
        assert_eq!(
            entry.diagnostic.failure_stage.as_deref(),
            Some("intake_parse")
        );
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["total_observed_bytes"], 2048);
        assert_eq!(meta["retained_bytes"], 512);
        assert_eq!(meta["head_bytes"], 512);
        assert_eq!(meta["tail_bytes"], 0);
        assert_eq!(meta["complete"], true);
        assert_eq!(meta["truncated"], true);
        assert_eq!(meta["declared_bytes"], 2048);
        assert_eq!(entry.client_request_body.as_ref().unwrap().len(), 512);
    }

    #[tokio::test]
    async fn intake_partial_io_error_keeps_counts_and_safe_failure_after_finalization() {
        for observed in [128, 768] {
            let chunks = vec![
                Ok(Bytes::from(vec![b'x'; observed])),
                Err(std::io::Error::other(
                    "Bearer secret=do-not-copy-source-body",
                )),
            ];
            let req = streaming_request(
                Body::from_stream(futures::stream::iter(chunks)),
                None,
                Some(21_950_000),
            );
            let (status, entry) = finalized_rejection_log(req, Some(Instant::now())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                entry.diagnostic.failure_kind.as_deref(),
                Some("intake_read_error")
            );
            assert_eq!(
                entry.diagnostic.failure_stage.as_deref(),
                Some("intake_read")
            );
            assert_eq!(
                entry.diagnostic.error_causes,
                vec!["The client request body could not be read to completion."]
            );
            let meta = &entry.diagnostic.payload_metadata["client_request_body"];
            assert_eq!(meta["total_observed_bytes"], observed);
            assert_eq!(meta["retained_bytes"], observed.min(512));
            assert_eq!(meta["complete"], false);
            assert_eq!(meta["truncated"], observed > 512);
            assert_eq!(meta["declared_bytes"], 21_950_000);
            assert_eq!(meta["byte_domain"], "wire");
            assert_eq!(
                entry.client_request_body.as_ref().unwrap().len(),
                observed.min(512)
            );
        }
    }

    #[tokio::test]
    async fn intake_empty_body_logs_complete_empty_capture_without_declared_length() {
        let req = streaming_request(Body::empty(), None, None);
        let (status, entry) = finalized_rejection_log(req, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["total_observed_bytes"], 0);
        assert_eq!(meta["retained_bytes"], 0);
        assert_eq!(meta["capture_state"], "empty");
        assert_eq!(meta["complete"], true);
        assert_eq!(meta["truncated"], false);
        assert!(meta.get("declared_bytes").is_none());
        assert_eq!(entry.client_request_body.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn intake_fallback_latency_includes_body_read_wait() {
        // A short async wait exercises the no-RequestContext timestamp fallback.
        let chunks = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Err::<Bytes, _>(std::io::Error::other("read failed"))
        });
        let req = streaming_request(Body::from_stream(chunks), None, None);
        let (_, entry) = finalized_rejection_log(req, None).await;
        assert!(entry.latency_total_ms >= 20);
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["total_observed_bytes"], 0);
        assert_eq!(meta["capture_state"], "absent");
        assert_eq!(meta["complete"], false);
        assert_eq!(meta["truncated"], false);
        assert!(meta.get("declared_bytes").is_none());
    }

    #[tokio::test]
    async fn intake_body_limit_records_only_observed_bytes_for_plain_and_compressed() {
        // No router override: Axum's default body limit is 2 MiB. A second
        // successful data frame crosses it; the third frame is never polled.
        let limit = 2 * 1024 * 1024;
        for encoding in [None, Some("gzip")] {
            let chunks = vec![
                Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 700])),
                Ok(Bytes::from(vec![b'y'; limit])),
                Ok(Bytes::from_static(b"unread")),
            ];
            let req = streaming_request(
                Body::from_stream(futures::stream::iter(chunks)),
                encoding,
                Some(30_000_000),
            );
            let (status, entry) = finalized_rejection_log(req, Some(Instant::now())).await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(
                entry.diagnostic.failure_kind.as_deref(),
                Some("intake_body_limit")
            );
            assert_eq!(
                entry.diagnostic.failure_stage.as_deref(),
                Some("intake_read")
            );
            let meta = &entry.diagnostic.payload_metadata["client_request_body"];
            assert_eq!(meta["total_observed_bytes"], limit + 700);
            assert_eq!(meta["retained_bytes"], 512);
            assert_eq!(meta["complete"], false);
            assert_eq!(meta["truncated"], true);
        }
    }

    #[tokio::test]
    async fn intake_non_utf8_and_split_utf8_prefix_are_byte_exact() {
        use base64::Engine;
        let binary = vec![0xff; 700];
        let mut split = vec![b'x'; 511];
        split.extend_from_slice("é".as_bytes());
        split.extend_from_slice(b"not-json");
        for raw in [binary, split] {
            let expected = raw[..512].to_vec();
            let observed = raw.len();
            let req = streaming_request(Body::from(raw), None, None);
            let (_, entry) = finalized_rejection_log(req, None).await;
            let meta = &entry.diagnostic.payload_metadata["client_request_body"];
            assert_eq!(meta["encoding"], "base64");
            assert_eq!(meta["encoding_scope"], "retained_prefix");
            assert_eq!(meta["total_observed_bytes"], observed);
            assert_eq!(meta["retained_bytes"], 512);
            assert_eq!(meta["complete"], true);
            assert_eq!(meta["truncated"], true);
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(entry.client_request_body.unwrap())
                    .unwrap(),
                expected,
            );
        }
    }

    #[tokio::test]
    async fn intake_compressed_io_failure_remains_read_error_with_wire_prefix() {
        let chunks = vec![
            Ok(Bytes::from(vec![0x1f; 800])),
            Err(std::io::Error::other("connection closed")),
        ];
        let req = streaming_request(
            Body::from_stream(futures::stream::iter(chunks)),
            Some("gzip"),
            Some(1000),
        );
        let (status, entry) = finalized_rejection_log(req, Some(Instant::now())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_read_error")
        );
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["byte_domain"], "wire");
        assert_eq!(meta["total_observed_bytes"], 800);
        assert_eq!(meta["retained_bytes"], 512);
        assert_eq!(meta["complete"], false);
        assert_eq!(meta["truncated"], true);
    }

    #[tokio::test]
    async fn intake_corrupt_compression_logs_wire_transport_as_complete() {
        let raw = vec![0xff; 800];
        let req = streaming_request(Body::from(raw), Some("gzip"), None);
        let (_, entry) = finalized_rejection_log(req, Some(Instant::now())).await;
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_decompress_error")
        );
        assert_eq!(
            entry.diagnostic.failure_stage.as_deref(),
            Some("intake_decompress")
        );
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["byte_domain"], "wire");
        assert_eq!(meta["total_observed_bytes"], 800);
        assert_eq!(meta["complete"], true);
        assert_eq!(meta["truncated"], true);
        assert_eq!(meta["encoding"], "base64");
        assert!(meta.get("wire_observed_bytes").is_none());
    }

    #[tokio::test]
    async fn intake_rejection_is_logged_with_wire_evidence() {
        let (gw, mut log_rx) = log_test_gateway().await;

        // The client stream-corruption class: a complete HTTP request line
        // smuggled into the body slot of another request.
        let body: &'static [u8] =
            b"POST http://192.168.31.2:19530/v1/chat/completions HTTP/1.1\r\nHost: x\r\n\r\n";
        let rejection = JsonIntake::from_request(uri_request(body, "/v1/chat/completions"), &gw)
            .await
            .expect_err("smuggled body must be rejected");
        assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);

        let entry = next_log_entry(&mut log_rx).await;
        assert_eq!(entry.client_status_code, 400);
        assert_eq!(entry.method.as_deref(), Some("POST"));
        assert_eq!(entry.path.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(
            entry.client_protocol,
            "openai-compatible/chat-completions/v1"
        );
        let body_str = entry.client_request_body.expect("body prefix recorded");
        assert!(body_str.starts_with("POST http://192.168.31.2:19530"));
        let resp = entry.client_response_body.expect("rejection text recorded");
        assert!(resp.contains("expected value"), "unexpected body: {resp}");
    }

    #[tokio::test]
    async fn intake_rejection_redacts_sensitive_headers() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/messages")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, "Bearer sk-intake-secret")
            .body(Body::from(b"not-json".as_slice()))
            .expect("valid test request");
        assert!(JsonIntake::from_request(req, &gw).await.is_err());

        let entry = next_log_entry(&mut log_rx).await;
        let headers = entry.client_request_headers.expect("headers recorded");
        assert!(
            headers.contains("\"***\""),
            "redacted marker missing: {headers}"
        );
        assert!(!headers.contains("sk-intake-secret"));
        assert_eq!(
            entry.client_protocol,
            "anthropic-messages/messages/2023-06-01"
        );
    }

    #[tokio::test]
    async fn valid_intake_with_gateway_state_emits_no_log() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let intake = JsonIntake::from_request(
            uri_request(br#"{"model":"gpt-test"}"#, "/v1/chat/completions"),
            &gw,
        )
        .await
        .expect("valid intake");
        assert_eq!(intake.value["model"], "gpt-test");
        assert!(matches!(
            log_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn missing_content_type_rejection_is_logged_with_415() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/responses")
            .body(Body::from(br#"{"model":"gpt-test"}"#.as_slice()))
            .expect("valid test request");
        let rejection = JsonIntake::from_request(req, &gw)
            .await
            .expect_err("missing content type must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let entry = next_log_entry(&mut log_rx).await;
        assert_eq!(entry.client_status_code, 415);
        assert_eq!(entry.client_protocol, "openai-responses/responses/v1");
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_content_type")
        );
        assert_eq!(
            entry.diagnostic.failure_stage.as_deref(),
            Some("intake_headers")
        );
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(meta["total_observed_bytes"], 0);
        assert_eq!(meta["retained_bytes"], 0);
        assert_eq!(meta["capture_state"], "absent");
        assert_eq!(meta["complete"], false);
        assert_eq!(meta["truncated"], false);
        assert!(entry.client_request_body.is_none());
        let resp = entry.client_response_body.expect("rejection text recorded");
        assert!(resp.contains("Expected request with"), "unexpected: {resp}");
    }

    #[tokio::test]
    async fn compressed_rejection_logs_parsed_view_and_original_headers() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let raw = format!("still not json after decompression{}", " ".repeat(900));
        std::io::Write::write_all(&mut encoder, raw.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let compressed_len = compressed.len();

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, "gzip")
            .header(header::CONTENT_LENGTH, compressed_len.to_string())
            .body(Body::from(compressed))
            .expect("valid test request");
        assert!(JsonIntake::from_request(req, &gw).await.is_err());

        let entry = next_log_entry(&mut log_rx).await;
        let meta = &entry.diagnostic.payload_metadata["client_request_body"];
        assert_eq!(
            entry.diagnostic.failure_kind.as_deref(),
            Some("intake_parse_error")
        );
        assert_eq!(meta["byte_domain"], "decoded");
        assert_eq!(meta["total_observed_bytes"], raw.len());
        assert_eq!(meta["wire_observed_bytes"], compressed_len);
        assert_eq!(meta["declared_bytes"], compressed_len);
        assert_eq!(meta["declared_byte_domain"], "wire");
        assert_eq!(meta["retained_bytes"], 512);
        assert_eq!(meta["complete"], true);
        assert_eq!(meta["truncated"], true);
        // The decompression succeeded, so the logged prefix is the
        // decompressed bytes the JSON parser actually rejected.
        let body_str = entry.client_request_body.expect("body prefix recorded");
        assert!(body_str.starts_with("still not json"), "got: {body_str:?}");
        // The wire snapshot preserves the original request headers,
        // including the content-encoding stripped before parsing.
        let headers = entry.client_request_headers.expect("headers recorded");
        assert!(
            headers.contains("\"gzip\""),
            "original content-encoding missing: {headers}"
        );
    }

    #[tokio::test]
    async fn unsupported_encoding_rejection_logs_wire_bytes() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, "snappy")
            .body(Body::from(b"not-json-wire-bytes".as_slice()))
            .expect("valid test request");
        assert!(JsonIntake::from_request(req, &gw).await.is_err());

        let entry = next_log_entry(&mut log_rx).await;
        let body_str = entry.client_request_body.expect("body prefix recorded");
        assert!(body_str.starts_with("not-json-wire-bytes"));
    }

    #[tokio::test]
    async fn non_gateway_state_rejection_does_not_panic() {
        // Extractor unit tests and foreign routers use non-Gateway states:
        // rejection logging must be a silent no-op there.
        let rejection =
            JsonIntake::from_request(request(b"not json", Some("application/json")), &())
                .await
                .expect_err("invalid JSON must be rejected");
        assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
    }
}
