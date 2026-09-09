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
        let (mut parts, body) = req.into_parts();
        // Wire snapshot taken before `parts` is consumed by the inner `Json`
        // extractor: intake-rejection logging needs the original view (the
        // compressed path strips content headers before re-invoking `Json`).
        let wire = WireMeta {
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
            let compressed = collect_body(body).await;
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
                            report_intake_rejection(state, &wire, &decoded, &rejection);
                            return Err(rejection);
                        }
                    };
                    return Ok(Self {
                        value,
                        raw: decoded,
                    });
                }
                _ => {
                    let req = Request::from_parts(parts, Body::from(compressed.clone()));
                    let Json(value) = match Json::<Value>::from_request(req, state).await {
                        Ok(v) => v,
                        Err(rejection) => {
                            report_intake_rejection(state, &wire, &compressed, &rejection);
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

        let captured = Arc::new(Mutex::new(BytesMut::new()));
        let capture = Arc::clone(&captured);
        let body = Body::from_stream(body.into_data_stream().map(move |chunk| {
            if let Ok(bytes) = &chunk {
                capture
                    .lock()
                    .expect("request body capture mutex poisoned")
                    .extend_from_slice(bytes);
            }
            chunk
        }));
        let req = Request::from_parts(parts, body);

        let Json(value) = match Json::<Value>::from_request(req, state).await {
            Ok(v) => v,
            Err(rejection) => {
                let prefix = captured
                    .lock()
                    .expect("request body capture mutex poisoned")
                    .iter()
                    .take(crate::proxy::dispatcher::INTAKE_REJECTION_BODY_PREFIX_BYTES)
                    .copied()
                    .collect::<Vec<u8>>();
                report_intake_rejection(state, &wire, &prefix, &rejection);
                return Err(rejection);
            }
        };
        let raw = captured
            .lock()
            .expect("request body capture mutex poisoned")
            .clone()
            .freeze();

        Ok(Self { value, raw })
    }
}

/// Original wire metadata kept for intake-rejection logging.
struct WireMeta {
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
    body_prefix: &[u8],
    rejection: &JsonRejection,
) where
    S: 'static,
{
    let any_state: &dyn std::any::Any = state;
    if let Some(gw) = any_state.downcast_ref::<crate::Gateway>() {
        crate::proxy::dispatcher::log_intake_rejection(
            gw,
            &wire.method,
            &wire.path,
            &wire.headers,
            body_prefix,
            rejection.status(),
            &rejection.body_text(),
            wire.context.as_ref(),
        );
    }
}

async fn collect_body(body: Body) -> Bytes {
    // The ambient request-body-limit layer has already run by this point. A
    // transport failure is surfaced as the same syntax rejection an empty body
    // receives rather than bypassing JsonRejection's public constructors.
    axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default()
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

        let intake = JsonIntake::from_request(encoded_request(compressed, "gzip"), &()).await;
        assert!(intake.is_err(), "compressed bomb must not decode to a body");
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
        let resp = entry.client_response_body.expect("rejection text recorded");
        assert!(resp.contains("Expected request with"), "unexpected: {resp}");
    }

    #[tokio::test]
    async fn compressed_rejection_logs_parsed_view_and_original_headers() {
        let (gw, mut log_rx) = log_test_gateway().await;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, b"still not json after decompression").unwrap();
        let compressed = encoder.finish().unwrap();

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(compressed))
            .expect("valid test request");
        assert!(JsonIntake::from_request(req, &gw).await.is_err());

        let entry = next_log_entry(&mut log_rx).await;
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
