//! Synthetic AWS-shaped responses, not recovered production body fixtures.
//! Exercise HTTP observation -> final LogEntry -> collector -> SQLite RequestLog.
//! Raw wire is privileged admin evidence; public errors, headers and diagnostic
//! metadata must remain sanitized even when an upstream body echoes a credential.
use super::*;
use crate::config::GatewayConfig;
use crate::db::models::RequestLog;
use crate::logging::LogEntry;
use crate::logging::payload::{
    BODY_CAPTURE_LIMIT, BODY_HEAD_LIMIT, BODY_TAIL_LIMIT, capture_headers, capture_json,
};
use crate::protocol::StreamResponseEncoder;
use crate::protocol::codec::anthropic::messages::stream::AnthropicStreamFormatter;
use crate::protocol::ir::{Message, MessageContent, Role};
use crate::storage::{DynStorage, sqlite::SqliteStorage};
use axum::{Router, body::Body, http::Response, routing::post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const TOKEN: &str = "opaque-live-auth-fixture";
const RAW: &str = "private-raw-dialogue-fixture";

fn entry(status: u16) -> LogEntry {
    LogEntry {
        diagnostic: Default::default(),
        api_key_id: None,
        api_key_name: None,
        created_at: chrono::Utc::now().timestamp_millis(),
        client_protocol: "anthropic-messages/messages/2023-06-01".into(),
        upstream_protocol: super::super::PROTOCOL_KEY.into(),
        provider_id: "kiro-diagnostic-fixture".into(),
        provider_name: "Kiro diagnostic fixture".into(),
        provider_key_name: None,
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: "claude-sonnet-4-5".into(),
        upstream_model: "claude-sonnet-4.5".into(),
        reasoning_effort: None,
        route_decision: None,
        method: Some("POST".into()),
        path: Some("/v1/messages".into()),
        client_request_headers: None,
        client_request_body: None,
        client_response_headers: None,
        client_response_body: None,
        upstream_request_headers: None,
        upstream_request_body: None,
        upstream_response_headers: None,
        upstream_response_body: None,
        upstream_status_code: None,
        client_status_code: status as i32,
        performance: Default::default(),
        latency_total_ms: 0,
        latency_upstream_ms: None,
        usage: Default::default(),
        is_stream: true,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: Some(false),
    }
}

fn assert_safe(text: &str) {
    assert!(!text.contains(TOKEN), "credential leaked outside raw body");
    assert!(!text.contains(RAW), "dialogue leaked outside raw body");
}

fn assert_safe_log(log: &LogEntry) {
    assert_safe(&serde_json::to_string(&log.diagnostic).unwrap());
    for headers in [
        &log.client_request_headers,
        &log.client_response_headers,
        &log.upstream_request_headers,
        &log.upstream_response_headers,
    ]
    .into_iter()
    .flatten()
    {
        assert_safe(headers);
    }
}

// Deliberately exclude body columns. An upstream may echo a credential in its
// body, and retaining that byte-exact body for authorized admins is intentional.
fn safe_row_surface(row: &RequestLog) -> String {
    json!({
        "client_request_headers": row.client_request_headers,
        "client_response_headers": row.client_response_headers,
        "upstream_request_headers": row.upstream_request_headers,
        "upstream_response_headers": row.upstream_response_headers,
        "failure_kind": row.failure_kind,
        "failure_stage": row.failure_stage,
        "error_message": row.error_message,
        "error_causes": row.error_causes,
        "payload_metadata": row.payload_metadata,
    })
    .to_string()
}

fn metadata(row: &RequestLog) -> Value {
    serde_json::from_str(row.payload_metadata.as_deref().unwrap()).unwrap()
}

fn restored_body(body: Option<&str>, capture: &Value) -> Vec<u8> {
    let body = body.expect("retained native wire body");
    match capture["encoding"].as_str().unwrap() {
        "utf8" => body.as_bytes().to_vec(),
        "base64" => STANDARD.decode(body).expect("valid binary wire Base64"),
        encoding => panic!("unexpected retained body encoding: {encoding}"),
    }
}

fn assert_bounded_capture(body: Option<&str>, capture: &Value, wire: &[u8], complete: bool) {
    let retained = wire.len().min(BODY_CAPTURE_LIMIT);
    let head = retained.min(BODY_HEAD_LIMIT);
    let tail = retained - head;
    let truncated = wire.len() > BODY_CAPTURE_LIMIT;
    let expected = if truncated {
        [
            &wire[..BODY_HEAD_LIMIT],
            &wire[wire.len() - BODY_TAIL_LIMIT..],
        ]
        .concat()
    } else {
        wire.to_vec()
    };
    assert_eq!(capture["capture_state"], "captured");
    assert_eq!(capture["total_observed_bytes"], wire.len() as u64);
    assert_eq!(capture["retained_bytes"], retained as u64);
    assert_eq!(capture["head_bytes"], head as u64);
    assert_eq!(capture["tail_bytes"], tail as u64);
    assert_eq!(capture["truncated"], truncated);
    assert_eq!(capture["complete"], complete);
    assert_eq!(restored_body(body, capture), expected);
}

fn assert_capture(body: Option<&str>, capture: &Value, wire: &[u8], complete: bool) {
    assert_bounded_capture(body, capture, wire, complete);
    assert_eq!(capture["sha256"], hex::encode(Sha256::digest(wire)));
    // HTTP may coalesce the mock's 137-byte writes; this is observed transport
    // chunk count, not an invented count of AWS frames or IR deltas.
    assert!(capture["chunks"].as_u64().is_some_and(|count| count > 0));
}

fn client_request_fixture() -> Value {
    json!({
        "model": "claude-sonnet-4-5", "stream": true, "max_tokens": 1024,
        "messages": [{"role": "user", "content": RAW}],
    })
}

async fn run_http(
    status: u16,
    wire: Vec<u8>,
    headers: HeaderMap,
) -> (LogEntry, Option<GatewayError>, String) {
    let captured_request = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_request = captured_request.clone();
    let app = Router::new().route(
        "/generateAssistantResponse",
        post(move |request: axum::extract::Request| {
            let wire = wire.clone();
            let headers = headers.clone();
            let observed_request = observed_request.clone();
            async move {
                let request_body = axum::body::to_bytes(request.into_body(), BODY_CAPTURE_LIMIT)
                    .await
                    .unwrap();
                *observed_request.lock().unwrap() = request_body.to_vec();
                let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = wire
                    .chunks(137)
                    .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                    .collect();
                let mut response = Response::builder()
                    .status(status)
                    .body(Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap();
                *response.headers_mut() = headers;
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let deadline = Deadline::from_now(Duration::from_secs(5));
    let cancellation = CancellationToken::new();
    let (tx, mut rx) = mpsc::channel(2);
    let performance = crate::performance::Attempt::new(tx, cancellation.clone(), deadline.clone());
    performance.correlate(&uuid::Uuid::new_v4().to_string(), 0);
    let producer = performance.producer();
    let control = AttemptControl {
        deadline: deadline.clone(),
        cancellation,
        performance: performance.clone(),
        run_submitted: Arc::new(AtomicBool::new(false)),
        local_failure: Arc::new(AtomicBool::new(false)),
        rejected: Arc::new(AtomicBool::new(false)),
    };
    let client = ProxyClient::new(reqwest::Client::builder().no_proxy().build().unwrap())
        .with_deadline(deadline)
        .with_performance(performance.clone());
    let mut request = AiRequest::new(
        "claude-sonnet-4-5",
        vec![Message {
            role: Role::User,
            content: MessageContent::Text(RAW.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    let result = execute(
        request,
        "claude-sonnet-4-5",
        TOKEN,
        &KiroCredential::from_stored(&StoredCredential::default()),
        format!("http://{address}/generateAssistantResponse"),
        client,
        control.clone(),
    )
    .await;
    let mut formatter = AnthropicStreamFormatter::new();
    let mut client_wire = String::new();
    let (error, response_started) = match result {
        Err(error) => (Some(error), false),
        Ok(mut upstream) => {
            let mut failure = None;
            while let Some(batch) = upstream.events.next().await {
                match batch {
                    Ok(events) => {
                        for event in formatter.format_deltas(&events) {
                            client_wire.push_str(&event.to_sse_string());
                        }
                    }
                    Err(error) => {
                        assert!(failure.is_none(), "stream emitted more than one failure");
                        failure = Some(error);
                    }
                }
            }
            if failure.is_none() {
                for event in formatter.format_done() {
                    client_wire.push_str(&event.to_sse_string());
                }
            }
            (failure, true)
        }
    };
    let response = if let Some(error) = &error {
        // A late dispatcher failure must not replace the sticky original HTTP
        // or stream-parser failure recorded by the transport worker.
        performance.record_failure("failed", "upstream_error", "upstream_response", error);
        assert_eq!(
            control.rejected.load(std::sync::atomic::Ordering::Acquire),
            error
                .account_failure()
                .is_some_and(|failure| failure.rejected_before_execution),
        );
        assert_safe(&format!("{error:?}"));
        let mut response = error.render(None);
        if response_started {
            // Streaming headers have already been committed. Keep both statuses
            // at 200 so retention MUST use the versioned abnormal outcome.
            *response.status_mut() = axum::http::StatusCode::OK;
        }
        response
    } else {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(client_wire))
            .unwrap()
    };
    let mut log_entry = entry(response.status().as_u16());
    // The real intake has already snapshotted this client request before native
    // execution. Populate it through the same bounded/redacted capture helpers.
    let client_request = capture_json(&client_request_fixture(), true);
    log_entry.client_request_body = client_request.body;
    log_entry.diagnostic.payload_metadata["client_request_body"] = client_request.metadata;
    let client_headers = capture_headers(&HeaderMap::from_iter([
        (
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(TOKEN).unwrap(),
        ),
        (
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        ),
    ]));
    log_entry.client_request_headers = client_headers.headers;
    log_entry.diagnostic.payload_metadata["client_request_headers"] = client_headers.metadata;
    log_entry.usage = formatter.usage();
    performance.select_final(1);
    performance.log(log_entry);
    let response = performance.wrap(response);
    producer.finish();
    drop(producer);
    let rendered = axum::body::to_bytes(response.into_body(), BODY_CAPTURE_LIMIT)
        .await
        .unwrap();
    let log = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let _ = server.await;
    assert_safe_log(&log);
    let rendered = String::from_utf8(rendered.to_vec()).unwrap();
    if error.is_some() {
        assert_safe(&rendered);
    }
    assert_bounded_capture(
        log.client_request_body.as_deref(),
        &log.diagnostic.payload_metadata["client_request_body"],
        &serde_json::to_vec(&client_request_fixture()).unwrap(),
        true,
    );
    assert_bounded_capture(
        log.client_response_body.as_deref(),
        &log.diagnostic.payload_metadata["client_response_body"],
        rendered.as_bytes(),
        true,
    );
    // Payload policy is applied by the collector, not the observer. Even with
    // the per-model switch off, request and response are provisionally captured.
    let request_wire = captured_request.lock().unwrap();
    assert_capture(
        log.upstream_request_body.as_deref(),
        &log.diagnostic.payload_metadata["upstream_request_body"],
        &request_wire,
        true,
    );
    assert!(log.upstream_request_body.as_deref().unwrap().contains(RAW));
    assert_eq!(
        log.diagnostic.payload_metadata["upstream_request_body"]["encoding"],
        "utf8"
    );
    let request_headers: Value =
        serde_json::from_str(log.upstream_request_headers.as_deref().unwrap()).unwrap();
    assert_eq!(request_headers["authorization"], "***");
    assert!(log.upstream_response_body.is_some());
    (log, error, rendered)
}

async fn persist_with_setting(entries: Vec<LogEntry>, global_enabled: bool) -> Vec<RequestLog> {
    let dir = tempfile::tempdir().unwrap();
    let storage: DynStorage = Arc::new(
        SqliteStorage::from_config(&GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    storage
        .settings()
        .set(
            crate::logging::ENABLE_PAYLOAD_KEY,
            if global_enabled { "true" } else { "false" },
        )
        .await
        .unwrap();
    let ids: Vec<_> = entries
        .iter()
        .map(|entry| entry.diagnostic.log_id.clone())
        .collect();
    let (tx, rx) = mpsc::channel(entries.len().max(1));
    for entry in entries {
        tx.send(entry).await.unwrap();
    }
    drop(tx);
    // Closing the channel deterministically flushes through the real collector,
    // including payload-retention policy; no polling of eventual DB writes.
    crate::logging::run_collector(rx, storage.clone()).await;
    let mut rows = Vec::new();
    for id in ids {
        let row = storage.logs().find_by_id(&id).await.unwrap().unwrap();
        assert_safe(&safe_row_surface(&row));
        rows.push(row);
    }
    rows
}

async fn persist(entries: Vec<LogEntry>) -> Vec<RequestLog> {
    persist_with_setting(entries, false).await
}

fn headers() -> HeaderMap {
    HeaderMap::from_iter([
        (
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        ),
        (
            reqwest::header::HeaderName::from_static("x-amzn-errortype"),
            HeaderValue::from_static("ValidationException"),
        ),
        (
            reqwest::header::HeaderName::from_static("x-amzn-requestid"),
            HeaderValue::from_static("synthetic-request-123"),
        ),
    ])
}

fn success_wire() -> Vec<u8> {
    super::super::eventstream::encode_event(
        "assistantResponseEvent",
        &json!({"content": RAW}).to_string(),
    )
}

#[tokio::test]
async fn kiro_off_http_error_retains_raw_body_with_sanitized_diagnostic() {
    let body = serde_json::to_vec(&json!({
        "__type":"ValidationException", "message":"Improperly formed\u{7} request.",
        "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":"messages[0].content",
        "raw":RAW, "unknown":TOKEN, "conversation":[RAW],
    }))
    .unwrap();
    let (log, error, rendered) = run_http(400, body.clone(), headers()).await;
    let error = error.unwrap();
    assert_eq!(error.http_status().as_u16(), 400);
    assert!(error.account_failure().is_none());
    assert!(!error.retryable());
    assert!(!rendered.contains("Improperly"));
    assert!(!rendered.contains("CONTENT_LENGTH"));
    assert!(rendered.contains("upstream returned an error status: 400"));
    assert_eq!(
        log.diagnostic.failure_kind.as_deref(),
        Some("upstream_http_error")
    );
    assert_eq!(log.diagnostic.error_causes, vec!["upstream HTTP 400"]);
    let expected = json!({
        "source":"http_error", "status":400, "sanitized":true, "body_state":"json",
        "code":"ValidationException", "message":"Improperly formed request.",
        "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":"messages[0].content",
        "error_type":"ValidationException", "request_id":"synthetic-request-123",
    });
    assert_eq!(
        log.diagnostic.payload_metadata["kiro_upstream_error"],
        expected
    );
    let request_body = log.upstream_request_body.clone();
    let rows = persist(vec![log]).await;
    let row = &rows[0];
    assert_eq!(row.upstream_status_code, Some(400));
    assert_eq!(row.client_status_code, Some(400));
    assert_eq!(row.attempt_outcome, "failed");
    assert_eq!(row.upstream_request_body, request_body);
    let metadata = metadata(row);
    assert_eq!(metadata["kiro_upstream_error"], expected);
    assert_capture(
        row.upstream_response_body.as_deref(),
        &metadata["upstream_response_body"],
        &body,
        true,
    );
    assert_eq!(metadata["upstream_response_body"]["encoding"], "utf8");
    // This is authorized raw-body evidence, not a credential leak through a
    // public/sanitized surface. Do not ban TOKEN from the entire RequestLog.
    assert!(
        row.upstream_response_body
            .as_deref()
            .unwrap()
            .contains(TOKEN)
    );
    assert!(row.upstream_response_body.as_deref().unwrap().contains(RAW));
}

#[tokio::test]
async fn kiro_redaction_precedes_truncation_and_covers_final_header_copies() {
    let mut entries = Vec::new();
    let mut bodies = Vec::new();
    for message in [
        format!("{} {TOKEN}", "safe prefix ".repeat(70)),
        "See HTTPS://user:pass@example.test/path?key=opaque-value".into(),
        "Authorization: Bearer other-auth-value".into(),
    ] {
        let body = serde_json::to_vec(&json!({"error": {
            "code":"ValidationException", "message":message,
            "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":TOKEN,
            "details": {"message":RAW}, "raw":RAW,
        }}))
        .unwrap();
        let mut headers = headers();
        headers.insert("x-amzn-requestid", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("x-amzn-errortype", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("authorization", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("set-cookie", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("x-unknown", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("x-raw", HeaderValue::from_str(RAW).unwrap());
        let (log, _, rendered) = run_http(400, body.clone(), headers).await;
        assert!(!rendered.contains(&message));
        let diagnostic = &log.diagnostic.payload_metadata["kiro_upstream_error"];
        for field in ["message", "param", "request_id", "error_type"] {
            assert_eq!(diagnostic[field], "Sensitive diagnostic text omitted");
        }
        assert_eq!(diagnostic["reason"], "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        let stored_headers = log.upstream_response_headers.as_deref().unwrap();
        assert!(!stored_headers.contains("x-unknown"));
        assert!(!stored_headers.contains(TOKEN));
        bodies.push(body);
        entries.push(log);
    }
    for (row, body) in persist(entries).await.iter().zip(bodies) {
        let safe = safe_row_surface(row);
        for forbidden in ["user:pass", "opaque-value", "other-auth-value"] {
            assert!(!safe.contains(forbidden));
        }
        assert_capture(
            row.upstream_response_body.as_deref(),
            &metadata(row)["upstream_response_body"],
            &body,
            true,
        );
    }
}

#[tokio::test]
async fn kiro_auth_types_and_safe_reasons_survive_sqlite_without_token_bearing_messages() {
    let mut entries = Vec::new();
    let mut bodies = Vec::new();
    let cases = [
        (
            "UnauthorizedException",
            "The security token included in the request is expired",
            "token_expired",
        ),
        (
            "InvalidTokenException",
            "The provided token is invalid",
            "token_invalid",
        ),
        (
            "AccessDeniedException",
            "Authorization permission denied",
            "permission_denied",
        ),
        (
            "AccessDeniedException",
            "Your account has been suspended; token cannot be used",
            "account_suspended",
        ),
        (
            "ServiceQuotaExceededException",
            "Monthly quota exceeded for token",
            "quota_exhausted",
        ),
    ];
    for (error_type, message, reason) in cases {
        // Real AWS field shapes with synthetic secrets. Sanitized metadata must
        // omit namespaces, URLs and credentials, while admin wire stays exact.
        let body = serde_json::to_vec(&json!({
            "type": format!("com.amazon.auth#{error_type}:private-type-detail"),
            "message": format!("{message}; Authorization: Bearer {TOKEN}; other-secret-value; https://private.test/?token=other-secret-value"),
            "unknown": TOKEN, "raw": RAW,
        })).unwrap();
        let mut headers = headers();
        headers.insert(
            "x-amzn-errortype",
            HeaderValue::from_str(&format!(
                "{error_type}:https://private.test/?token=other-secret-value"
            ))
            .unwrap(),
        );
        let (log, _, rendered) = run_http(403, body.clone(), headers).await;
        let safe = &log.diagnostic.payload_metadata["kiro_upstream_error"];
        assert_eq!(safe["code"], error_type);
        assert_eq!(safe["error_type"], error_type);
        assert_eq!(safe["reason_class"], reason);
        assert_eq!(safe["message"], "Sensitive diagnostic text omitted");
        assert!(!rendered.contains(error_type));
        assert!(!rendered.contains(reason));
        let safe = serde_json::to_string(&log.diagnostic).unwrap();
        for forbidden in [
            "other-secret-value",
            "private.test",
            "private-type-detail",
            "com.amazon.auth",
        ] {
            assert!(
                !safe.contains(forbidden),
                "{forbidden} leaked to diagnostic"
            );
        }
        bodies.push(body);
        entries.push(log);
    }
    let rows = persist(entries).await;
    assert_eq!(rows.len(), cases.len());
    for ((row, (error_type, _, reason)), body) in rows.iter().zip(cases).zip(bodies) {
        let metadata = metadata(row);
        let safe = &metadata["kiro_upstream_error"];
        assert_eq!(safe["code"], error_type);
        assert_eq!(safe["error_type"], error_type);
        assert_eq!(safe["reason_class"], reason);
        assert_eq!(safe["message"], "Sensitive diagnostic text omitted");
        let safe = safe_row_surface(row);
        for forbidden in [
            "other-secret-value",
            "private.test",
            "private-type-detail",
            "com.amazon.auth",
        ] {
            assert!(
                !safe.contains(forbidden),
                "{forbidden} leaked to safe row fields"
            );
        }
        assert_capture(
            row.upstream_response_body.as_deref(),
            &metadata["upstream_response_body"],
            &body,
            true,
        );
    }
}

#[tokio::test]
async fn kiro_invalid_and_oversized_errors_keep_raw_wire_separate_from_bounded_diagnostics() {
    let mut exact = br#"{"message":"synthetic bounded validation"}"#.to_vec();
    exact.resize(MAX_ERROR_BYTES, b' ');
    let mut oversized = exact.clone();
    oversized.extend_from_slice(RAW.as_bytes());
    let mut entries = Vec::new();
    let mut bodies = Vec::new();
    for (body, state, has_message) in [
        (
            format!("{{not json {TOKEN} {RAW}").into_bytes(),
            "invalid_json",
            false,
        ),
        (vec![0xff, 0x00], "invalid_json", false),
        (
            serde_json::to_vec(&json!({"message":RAW.repeat(1000)})).unwrap(),
            "incomplete_or_oversized",
            false,
        ),
        // A complete JSON prefix at the classification limit must not hide the
        // trailing payload. The independent native capture still reaches EOF.
        (oversized, "incomplete_or_oversized", false),
        (exact, "json", true),
    ] {
        let (log, error, _) = run_http(400, body.clone(), headers()).await;
        assert_eq!(error.unwrap().http_status().as_u16(), 400);
        let safe = &log.diagnostic.payload_metadata["kiro_upstream_error"];
        assert_eq!(safe["body_state"], state);
        assert_eq!(safe.get("message").is_some(), has_message);
        assert_capture(
            log.upstream_response_body.as_deref(),
            &log.diagnostic.payload_metadata["upstream_response_body"],
            &body,
            true,
        );
        bodies.push(body);
        entries.push(log);
    }
    let rows = persist(entries).await;
    assert_eq!(rows.len(), 5);
    for (row, body) in rows.iter().zip(bodies) {
        let metadata = metadata(row);
        assert_capture(
            row.upstream_response_body.as_deref(),
            &metadata["upstream_response_body"],
            &body,
            true,
        );
        assert_eq!(metadata["upstream_response_body"]["truncated"], false);
    }
    assert_eq!(
        metadata(&rows[1])["upstream_response_body"]["encoding"],
        "base64"
    );
}

#[tokio::test]
async fn kiro_402_still_maps_to_monthly_quota_with_safe_replay_and_retry_after() {
    let body = serde_json::to_vec(
        &json!({"message":"Monthly quota exhausted", "reason":"MONTHLY_REQUEST_COUNT", "raw":RAW}),
    )
    .unwrap();
    let mut headers = headers();
    headers.insert("retry-after", HeaderValue::from_static("17"));
    let (log, error, rendered) = run_http(402, body.clone(), headers).await;
    let error = error.unwrap();
    assert_eq!(error.http_status().as_u16(), 429);
    let failure = error.account_failure().unwrap();
    assert_eq!(failure.reason, Reason::MonthlyQuota);
    assert!(failure.rejected_before_execution);
    assert_eq!(failure.retry_after_seconds, Some(17));
    assert!(rendered.contains("upstream returned an error status: 429"));
    let rows = persist(vec![log]).await;
    assert_eq!(rows[0].upstream_status_code, Some(402));
    assert_eq!(rows[0].client_status_code, Some(429));
    let metadata = metadata(&rows[0]);
    assert_eq!(
        metadata["kiro_upstream_error"]["reason"],
        "MONTHLY_REQUEST_COUNT"
    );
    assert_capture(
        rows[0].upstream_response_body.as_deref(),
        &metadata["upstream_response_body"],
        &body,
        true,
    );
}

#[tokio::test]
async fn kiro_success_off_is_captured_temporarily_but_not_retained_in_sqlite() {
    let body = success_wire();
    let (log, error, _) = run_http(200, body.clone(), HeaderMap::new()).await;
    assert!(error.is_none());
    assert_eq!(log.diagnostic.attempt_outcome, "completed");
    assert!(
        log.diagnostic
            .payload_metadata
            .get("kiro_upstream_error")
            .is_none()
    );
    assert_capture(
        log.upstream_response_body.as_deref(),
        &log.diagnostic.payload_metadata["upstream_response_body"],
        &body,
        true,
    );
    let rows = persist(vec![log]).await;
    let row = &rows[0];
    assert_eq!(row.client_status_code, Some(200));
    assert_eq!(row.attempt_outcome, "completed");
    for payload in [
        &row.client_request_headers,
        &row.client_request_body,
        &row.client_response_headers,
        &row.client_response_body,
        &row.upstream_request_headers,
        &row.upstream_request_body,
        &row.upstream_response_headers,
        &row.upstream_response_body,
    ] {
        assert!(payload.is_none(), "successful off request retained payload");
    }
    let metadata = metadata(row);
    assert!(metadata.get("kiro_upstream_error").is_none());
    for side in ["upstream_request_body", "upstream_response_body"] {
        let capture = &metadata[side];
        assert_eq!(capture["capture_state"], "not_retained");
        assert_eq!(capture["retained_bytes"], 0);
        assert_eq!(capture["head_bytes"], 0);
        assert_eq!(capture["tail_bytes"], 0);
        assert_eq!(capture["complete"], true);
        assert_eq!(capture["truncated"], false);
        assert!(
            capture["sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64)
        );
        assert!(capture["chunks"].as_u64().is_some_and(|count| count > 0));
    }
    assert_eq!(
        metadata["upstream_response_body"]["sha256"],
        hex::encode(Sha256::digest(&body))
    );
    assert_eq!(
        metadata["upstream_response_body"]["total_observed_bytes"],
        body.len() as u64
    );
    assert!(!serde_json::to_string(row).unwrap().contains(RAW));
}

#[tokio::test]
async fn kiro_success_retention_obeys_global_and_model_switch_matrix_at_flush() {
    // Existing settings semantics are a global gate AND the optional model gate:
    // model=true cannot override global=false; model=None inherits global.
    for (global, model, retained) in [
        (false, None, false),
        (false, Some(false), false),
        (false, Some(true), false),
        (true, None, true),
        (true, Some(false), false),
        (true, Some(true), true),
    ] {
        let body = success_wire();
        let (mut log, error, _) = run_http(200, body.clone(), HeaderMap::new()).await;
        assert!(error.is_none());
        assert!(log.upstream_request_body.is_some());
        assert!(log.upstream_response_body.is_some());
        // The switch is read by flush after observation, not used to suppress
        // capture before a response's success/failure is known.
        log.enable_payload = model;
        let request_body = log.upstream_request_body.clone();
        let client_request_body = log.client_request_body.clone();
        let client_response_body = log.client_response_body.clone();
        let rows = persist_with_setting(vec![log], global).await;
        let row = &rows[0];
        assert_eq!(row.attempt_outcome, "completed");
        assert_eq!(
            row.upstream_request_body.is_some(),
            retained,
            "global={global}, model={model:?}"
        );
        assert_eq!(
            row.upstream_response_body.is_some(),
            retained,
            "global={global}, model={model:?}"
        );
        assert_eq!(row.upstream_request_headers.is_some(), retained);
        assert_eq!(row.upstream_response_headers.is_some(), retained);
        assert_eq!(row.client_request_headers.is_some(), retained);
        assert_eq!(row.client_response_headers.is_some(), retained);
        assert_eq!(row.client_request_body.is_some(), retained);
        assert_eq!(row.client_response_body.is_some(), retained);
        let metadata = metadata(row);
        if retained {
            assert_eq!(row.upstream_request_body, request_body);
            assert_eq!(row.client_request_body, client_request_body);
            assert_eq!(row.client_response_body, client_response_body);
            assert_bounded_capture(
                row.client_request_body.as_deref(),
                &metadata["client_request_body"],
                &serde_json::to_vec(&client_request_fixture()).unwrap(),
                true,
            );
            assert_bounded_capture(
                row.client_response_body.as_deref(),
                &metadata["client_response_body"],
                client_response_body.as_deref().unwrap().as_bytes(),
                true,
            );
            assert_capture(
                row.upstream_response_body.as_deref(),
                &metadata["upstream_response_body"],
                &body,
                true,
            );
        } else {
            assert_eq!(
                metadata["upstream_response_body"]["capture_state"],
                "not_retained"
            );
            assert_eq!(metadata["upstream_response_body"]["retained_bytes"], 0);
        }
    }
}

#[tokio::test]
async fn kiro_opted_in_success_recovers_synthetic_binary_text_reasoning_and_tool_wire() {
    use super::super::eventstream::{Decoder, encode_event, encode_frame};

    // These are synthetic native fields, not claims about a production response.
    // Logging must preserve bytes regardless of whether IR recognizes a field or
    // considers a particular sentence to be thinking, text, or tool content.
    let text = json!({
        "content": RAW,
        "reasoning": "synthetic opaque reasoning field",
        "providerExtension": {"uninterpreted": RAW},
    });
    let reasoning = json!({
        "text": "synthetic reasoning fixture",
        "signature": "synthetic-signature-not-an-auth-header",
        "unknown": TOKEN,
    });
    let tool = json!({
        "name": "synthetic_lookup", "toolUseId": "synthetic-tu-1",
        "input": json!({"query": RAW, "echo": TOKEN}).to_string(), "stop": true,
    });
    let mut body = encode_event("assistantResponseEvent", &text.to_string());
    body.extend(encode_event(
        "reasoningContentEvent",
        &reasoning.to_string(),
    ));
    body.extend(encode_event("toolUseEvent", &tool.to_string()));
    // Forward-compatible bookkeeping can contain arbitrary binary payloads.
    body.extend(encode_frame(
        &[
            (":message-type", "event"),
            (":event-type", "syntheticOpaqueEvent"),
        ],
        &[0xff, 0x00, 0xfe],
    ));
    assert!(std::str::from_utf8(&body).is_err());
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        "content-type",
        HeaderValue::from_static("application/vnd.amazon.eventstream"),
    );
    response_headers.insert("authorization", HeaderValue::from_str(TOKEN).unwrap());
    response_headers.insert("set-cookie", HeaderValue::from_str(TOKEN).unwrap());
    response_headers.insert("x-provider-token", HeaderValue::from_str(TOKEN).unwrap());
    let (mut log, error, client_sse) = run_http(200, body.clone(), response_headers).await;
    assert!(error.is_none());
    assert_eq!(log.diagnostic.attempt_outcome, "completed");
    let upstream_request = log.upstream_request_body.clone();
    log.enable_payload = Some(true);
    let rows = persist_with_setting(vec![log], true).await;
    let row = &rows[0];
    let metadata = metadata(row);
    let capture = &metadata["upstream_response_body"];
    assert_eq!(capture["encoding"], "base64");
    assert_capture(row.upstream_response_body.as_deref(), capture, &body, true);
    assert_eq!(
        row.stream_chunks_count as u64,
        capture["chunks"].as_u64().unwrap()
    );
    let restored = restored_body(row.upstream_response_body.as_deref(), capture);
    let mut decoder = Decoder::new();
    let frames = decoder.feed(&restored).unwrap();
    decoder.finish().unwrap();
    assert_eq!(frames.len(), 4);
    for (frame, (kind, expected)) in frames.iter().zip([
        ("assistantResponseEvent", text),
        ("reasoningContentEvent", reasoning),
        ("toolUseEvent", tool),
    ]) {
        assert_eq!(frame.event_type(), kind);
        assert_eq!(
            serde_json::from_slice::<Value>(&frame.payload).unwrap(),
            expected
        );
    }
    assert_eq!(frames[3].payload, [0xff, 0x00, 0xfe]);
    assert_eq!(row.upstream_request_body, upstream_request);
    assert_bounded_capture(
        row.client_request_body.as_deref(),
        &metadata["client_request_body"],
        &serde_json::to_vec(&client_request_fixture()).unwrap(),
        true,
    );
    assert_bounded_capture(
        row.client_response_body.as_deref(),
        &metadata["client_response_body"],
        client_sse.as_bytes(),
        true,
    );
    let client_events: Vec<Value> = client_sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(
        client_events
            .iter()
            .any(|event| event["delta"]["type"] == "text_delta" && event["delta"]["text"] == RAW)
    );
    assert!(
        client_events
            .iter()
            .any(|event| event["delta"]["type"] == "thinking_delta"
                && event["delta"]["thinking"] == "synthetic reasoning fixture")
    );
    assert!(
        client_events
            .iter()
            .any(|event| event["content_block"]["type"] == "tool_use"
                && event["content_block"]["name"] == "synthetic_lookup")
    );
    assert!(client_events.iter().any(|event| {
        event["delta"]["type"] == "input_json_delta"
            && event["delta"]["partial_json"]
                .as_str()
                .is_some_and(|input| input.contains(RAW))
    }));
    assert!(
        client_events
            .iter()
            .any(|event| event["type"] == "message_stop")
    );
    // Unknown native fields stay on the privileged original-wire side rather
    // than being invented as downstream reasoning or tool metadata.
    assert!(!client_sse.contains("synthetic opaque reasoning field"));
    assert!(!client_sse.contains("synthetic-signature-not-an-auth-header"));
    let client_headers: Value =
        serde_json::from_str(row.client_request_headers.as_deref().unwrap()).unwrap();
    assert_eq!(client_headers["authorization"], "***");
    let headers: Value =
        serde_json::from_str(row.upstream_response_headers.as_deref().unwrap()).unwrap();
    for name in ["authorization", "set-cookie", "x-provider-token"] {
        assert_eq!(headers[name], "***");
    }
    assert_safe(&safe_row_surface(row));
}

#[tokio::test]
async fn kiro_off_200_stream_failures_force_retention_without_requiring_http_error_status() {
    use super::super::eventstream::{encode_event, encode_frame};

    let mut exception = success_wire();
    exception.extend(encode_frame(
        &[
            (":message-type", "exception"),
            (":exception-type", "throttlingException"),
        ],
        &serde_json::to_vec(&json!({"message": format!("Too many requests; {TOKEN}"), "raw": RAW}))
            .unwrap(),
    ));
    // AWS error headers are untrusted data, not safe diagnostic identifiers.
    // Preserve echoed secrets only in the privileged raw frame, never causes.
    let error_frame = encode_frame(
        &[
            (":message-type", "error"),
            (":error-code", TOKEN),
            (":error-message", RAW),
        ],
        &serde_json::to_vec(&json!({"message": TOKEN, "raw": RAW})).unwrap(),
    );
    let mut incomplete = encode_event(
        "assistantResponseEvent",
        &json!({"content": RAW}).to_string(),
    );
    incomplete.pop();
    let mut invalid_crc = success_wire();
    *invalid_crc.last_mut().unwrap() ^= 0xff;
    for (name, body, complete, error_status) in [
        ("exception", exception, false, 429),
        (
            "error frame with echoed credential",
            error_frame,
            false,
            502,
        ),
        // Protocol truncation is not capture-limit truncation. HTTP reached EOF,
        // so all bytes actually sent are present even though the frame is invalid.
        ("truncated frame", incomplete, true, 502),
        ("invalid CRC", invalid_crc, false, 502),
    ] {
        let (log, error, rendered) = run_http(200, body.clone(), HeaderMap::new()).await;
        assert_eq!(
            error.unwrap().http_status().as_u16(),
            error_status,
            "{name}"
        );
        assert_safe(&rendered);
        assert_eq!(log.upstream_status_code, Some(200), "{name}");
        assert_eq!(log.client_status_code, 200, "{name}");
        assert_eq!(log.diagnostic.attempt_outcome, "failed", "{name}");
        assert_eq!(
            log.diagnostic.failure_kind.as_deref(),
            Some("kiro_protocol_error")
        );
        let request = log.upstream_request_body.clone();
        let rows = persist(vec![log]).await;
        let row = &rows[0];
        assert_eq!(row.upstream_status_code, Some(200));
        assert_eq!(row.client_status_code, Some(200));
        assert_eq!(row.attempt_outcome, "failed");
        assert_eq!(
            row.outcome_version,
            crate::logging::diagnostics::OUTCOME_VERSION
        );
        assert_eq!(row.upstream_request_body, request);
        assert_capture(
            row.upstream_response_body.as_deref(),
            &metadata(row)["upstream_response_body"],
            &body,
            complete,
        );
    }
}

#[tokio::test]
async fn kiro_off_200_output_limited_retains_raw_wire_even_without_stream_error() {
    let body = super::super::eventstream::encode_event(
        "reasoningContentEvent",
        &json!({"text": RAW}).to_string(),
    );
    let (log, error, _) = run_http(200, body.clone(), HeaderMap::new()).await;
    assert!(error.is_none());
    assert_eq!(log.diagnostic.attempt_outcome, "output_limited");
    let rows = persist(vec![log]).await;
    let row = &rows[0];
    assert_eq!(row.client_status_code, Some(200));
    assert_eq!(row.attempt_outcome, "output_limited");
    assert_capture(
        row.upstream_response_body.as_deref(),
        &metadata(row)["upstream_response_body"],
        &body,
        true,
    );
}

#[tokio::test]
async fn kiro_opted_in_success_bounds_raw_wire_without_confusing_truncation_and_eof() {
    use super::super::eventstream::encode_frame;

    for total_bytes in [BODY_CAPTURE_LIMIT, BODY_CAPTURE_LIMIT + 257] {
        let mut body = success_wire();
        let headers = [
            (":message-type", "event"),
            (":event-type", "syntheticPaddingEvent"),
        ];
        let overhead = encode_frame(&headers, &[]).len();
        let padding = vec![0xff; total_bytes - body.len() - overhead];
        body.extend(encode_frame(&headers, &padding));
        assert_eq!(body.len(), total_bytes);
        let (mut log, error, _) = run_http(200, body.clone(), HeaderMap::new()).await;
        assert!(error.is_none());
        assert_eq!(log.diagnostic.attempt_outcome, "completed");
        log.enable_payload = Some(true);
        let rows = persist_with_setting(vec![log], true).await;
        let row = &rows[0];
        let metadata = metadata(row);
        let capture = &metadata["upstream_response_body"];
        assert_eq!(capture["encoding"], "base64");
        assert_capture(row.upstream_response_body.as_deref(), capture, &body, true);
        assert_eq!(capture["retained_bytes"], BODY_CAPTURE_LIMIT as u64);
        assert_eq!(capture["head_bytes"], BODY_HEAD_LIMIT as u64);
        assert_eq!(capture["tail_bytes"], BODY_TAIL_LIMIT as u64);
        // Hash/count describe ALL observed wire, not just the retained sections.
        assert_eq!(capture["truncated"], total_bytes > BODY_CAPTURE_LIMIT);
        assert_eq!(capture["complete"], true);
    }
}
