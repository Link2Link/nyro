//! Synthetic AWS-shaped responses, not recovered production body fixtures.
//! Exercise HTTP observation -> final LogEntry -> collector -> SQLite RequestLog.
use super::*;
use crate::config::GatewayConfig;
use crate::db::models::RequestLog;
use crate::logging::LogEntry;
use crate::protocol::ir::{Message, MessageContent, Role};
use crate::storage::{DynStorage, sqlite::SqliteStorage};
use axum::{Router, body::Body, http::Response, routing::post};
use serde_json::json;
use tokio::sync::mpsc;

const TOKEN: &str = "opaque-live-auth-fixture";
const RAW: &str = "private-raw-dialogue-fixture";

fn entry(status: u16) -> LogEntry {
    LogEntry {
        diagnostic: Default::default(),
        api_key_id: None,
        api_key_name: None,
        created_at: chrono::Utc::now().timestamp_millis(),
        client_protocol: "openai/chat-completions/v1".into(),
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
        path: Some("/v1/chat/completions".into()),
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

async fn run_http(
    status: u16,
    wire: Vec<u8>,
    headers: HeaderMap,
) -> (LogEntry, Option<GatewayError>, String) {
    let app = Router::new().route(
        "/generateAssistantResponse",
        post(move || {
            let wire = wire.clone();
            let headers = headers.clone();
            async move {
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
    let error = match result {
        Err(error) => Some(error),
        Ok(mut upstream) => {
            while let Some(batch) = upstream.events.next().await {
                batch.unwrap();
            }
            None
        }
    };
    let response = if let Some(error) = &error {
        // The same late failure registration that used to lose the reason must
        // not be required to replace the already-sticky HTTP failure.
        performance.record_failure("failed", "upstream_error", "upstream_response", error);
        assert_eq!(
            control.rejected.load(std::sync::atomic::Ordering::Acquire),
            error
                .account_failure()
                .is_some_and(|failure| failure.rejected_before_execution),
        );
        assert!(!format!("{error:?}").contains(TOKEN));
        error.render(None)
    } else {
        Response::new(Body::empty())
    };
    performance.select_final(1);
    performance.log(entry(response.status().as_u16()));
    let response = performance.wrap(response);
    producer.finish();
    drop(producer);
    let rendered = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let log = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let _ = server.await;
    assert!(!format!("{log:?}").contains(TOKEN));
    assert!(!format!("{log:?}").contains(RAW));
    assert!(log.upstream_request_body.is_none());
    assert!(log.upstream_response_body.is_none());
    (log, error, String::from_utf8(rendered.to_vec()).unwrap())
}

async fn persist(entries: Vec<LogEntry>) -> Vec<RequestLog> {
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
        .set(crate::logging::ENABLE_PAYLOAD_KEY, "false")
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
        let encoded = serde_json::to_string(&row).unwrap();
        assert!(!encoded.contains(TOKEN), "credential leaked to RequestLog");
        assert!(!encoded.contains(RAW), "raw dialogue leaked to RequestLog");
        assert!(row.upstream_request_body.is_none());
        assert!(row.upstream_response_body.is_none());
        rows.push(row);
    }
    rows
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

#[tokio::test]
async fn kiro_validation_diagnostic_survives_final_log_and_sqlite_without_raw_payloads() {
    let body = serde_json::to_vec(&json!({
        "__type":"ValidationException", "message":"Improperly formed\u{7} request.",
        "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":"messages[0].content",
        "raw":RAW, "unknown":TOKEN, "conversation":[RAW],
    }))
    .unwrap();
    let (log, error, rendered) = run_http(400, body, headers()).await;
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
    let rows = persist(vec![log]).await;
    let row = &rows[0];
    assert_eq!(row.upstream_status_code, Some(400));
    assert_eq!(row.client_status_code, Some(400));
    assert_eq!(row.attempt_outcome, "failed");
    let metadata: Value = serde_json::from_str(row.payload_metadata.as_deref().unwrap()).unwrap();
    assert_eq!(metadata["kiro_upstream_error"], expected);
    assert_eq!(
        metadata["upstream_response_body"]["capture_state"],
        "digest_only"
    );
    assert_eq!(metadata["upstream_response_body"]["retained_bytes"], 0);
}

#[tokio::test]
async fn kiro_redaction_precedes_truncation_and_covers_final_header_copies() {
    let mut entries = Vec::new();
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
        headers.insert("x-unknown", HeaderValue::from_str(TOKEN).unwrap());
        headers.insert("x-raw", HeaderValue::from_str(RAW).unwrap());
        let (log, _, rendered) = run_http(400, body, headers).await;
        assert!(!rendered.contains(&message));
        let diagnostic = &log.diagnostic.payload_metadata["kiro_upstream_error"];
        for field in ["message", "param", "request_id", "error_type"] {
            assert_eq!(diagnostic[field], "Sensitive diagnostic text omitted");
        }
        assert_eq!(diagnostic["reason"], "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        let stored_headers = log.upstream_response_headers.as_deref().unwrap();
        assert!(!stored_headers.contains("x-unknown"));
        assert!(!stored_headers.contains(TOKEN));
        entries.push(log);
    }
    for row in persist(entries).await {
        let encoded = serde_json::to_string(&row).unwrap();
        for forbidden in ["user:pass", "opaque-value", "other-auth-value"] {
            assert!(!encoded.contains(forbidden));
        }
    }
}

#[tokio::test]
async fn kiro_incomplete_invalid_and_oversized_errors_never_persist_body_prefixes() {
    let mut exact = br#"{"message":"synthetic bounded validation"}"#.to_vec();
    exact.resize(MAX_ERROR_BYTES, b' ');
    let mut oversized = exact.clone();
    oversized.extend_from_slice(RAW.as_bytes());
    let mut entries = Vec::new();
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
        // A complete JSON prefix at the limit must not hide a trailing payload.
        (oversized, "incomplete_or_oversized", false),
        (exact, "json", true),
    ] {
        let (log, error, _) = run_http(400, body, headers()).await;
        assert_eq!(error.unwrap().http_status().as_u16(), 400);
        let safe = &log.diagnostic.payload_metadata["kiro_upstream_error"];
        assert_eq!(safe["body_state"], state);
        assert_eq!(safe.get("message").is_some(), has_message);
        entries.push(log);
    }
    assert_eq!(persist(entries).await.len(), 5);
}

#[tokio::test]
async fn kiro_402_still_maps_to_monthly_quota_with_safe_replay_and_retry_after() {
    let body = serde_json::to_vec(
        &json!({"message":"Monthly quota exhausted", "reason":"MONTHLY_REQUEST_COUNT", "raw":RAW}),
    )
    .unwrap();
    let mut headers = headers();
    headers.insert("retry-after", HeaderValue::from_static("17"));
    let (log, error, rendered) = run_http(402, body, headers).await;
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
    let metadata: Value =
        serde_json::from_str(rows[0].payload_metadata.as_deref().unwrap()).unwrap();
    assert_eq!(
        metadata["kiro_upstream_error"]["reason"],
        "MONTHLY_REQUEST_COUNT"
    );
}

#[tokio::test]
async fn kiro_success_stays_digest_only_without_error_diagnostic() {
    let body = super::super::eventstream::encode_event(
        "assistantResponseEvent",
        &json!({"content":RAW}).to_string(),
    );
    let (log, error, _) = run_http(200, body, HeaderMap::new()).await;
    assert!(error.is_none());
    assert!(
        log.diagnostic
            .payload_metadata
            .get("kiro_upstream_error")
            .is_none()
    );
    assert_eq!(
        log.diagnostic.payload_metadata["upstream_response_body"]["capture_state"],
        "digest_only"
    );
    assert_eq!(
        log.diagnostic.payload_metadata["upstream_response_body"]["retained_bytes"],
        0
    );
    let rows = persist(vec![log]).await;
    assert_eq!(rows[0].client_status_code, Some(200));
    let metadata: Value =
        serde_json::from_str(rows[0].payload_metadata.as_deref().unwrap()).unwrap();
    assert!(metadata.get("kiro_upstream_error").is_none());
    assert!(
        metadata["upstream_response_body"]["sha256"]
            .as_str()
            .is_some()
    );
}
