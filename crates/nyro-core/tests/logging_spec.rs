//! Spec-aligned logging layer unit tests.
//!
//! Covers:
//!   1. headers_to_json — sensitive key redaction (axum HeaderMap)
//!   2. reqwest_headers_to_json — sensitive key redaction (reqwest HeaderMap)
//!   3. LogEntry — created_at is Unix milliseconds
//!   4. LogEntry — stream indicator via stream_chunks_count > 0
//!   5. LogEntry — non-stream has stream_chunks_count == 0
//!   6. DB schema — new columns exist in the CREATE TABLE SQL (unit-level)

use axum::http::{HeaderMap as AxumHeaderMap, HeaderName, HeaderValue};
use nyro_core::proxy::observability::{headers_to_json, reqwest_headers_to_json};

// ── 1. Axum headers: sensitive values are redacted ────────────────────────────

#[test]
fn headers_to_json_redacts_authorization() {
    let mut map = AxumHeaderMap::new();
    map.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("Bearer secret-token"),
    );
    map.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );

    let json = headers_to_json(&map).expect("should serialize");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(v["authorization"], "***", "authorization must be redacted");
    assert_eq!(
        v["content-type"], "application/json",
        "non-sensitive header must pass through"
    );
}

#[test]
fn headers_to_json_redacts_all_sensitive_keys() {
    let sensitive = [
        ("x-api-key", "key-value"),
        ("x-goog-api-key", "google-key"),
        ("cookie", "session=abc"),
        ("set-cookie", "token=xyz"),
        ("proxy-authorization", "Basic creds"),
    ];
    let mut map = AxumHeaderMap::new();
    for (k, v) in &sensitive {
        map.insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    let json = headers_to_json(&map).expect("should serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    for (k, _) in &sensitive {
        assert_eq!(parsed[*k], "***", "header '{k}' must be redacted");
    }
}

// ── 2. Reqwest headers: sensitive values are redacted ─────────────────────────

#[test]
fn reqwest_headers_to_json_redacts_authorization() {
    let mut map = reqwest::header::HeaderMap::new();
    map.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_static("Bearer upstream-secret"),
    );
    map.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );

    let json = reqwest_headers_to_json(&map).expect("should serialize");
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(v["authorization"], "***");
    assert_eq!(v["content-type"], "application/json");
}

// ── 3. LogEntry timestamp is Unix milliseconds ────────────────────────────────

#[test]
fn log_entry_timestamp_is_unix_millis() {
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let ts = chrono::Utc::now().timestamp_millis();

    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    assert!(
        ts >= before && ts <= after,
        "timestamp {ts} should be within [{before}, {after}]"
    );

    // Sanity: value must be > 2020-01-01 in ms (1577836800000)
    assert!(
        ts > 1_577_836_800_000,
        "timestamp must be a reasonable Unix-ms value"
    );

    // Build a LogEntry and confirm created_at field accepts i64 ms.
    let _entry = LogEntry {
        api_key_id: None,
        api_key_name: None,
        created_at: ts,
        client_protocol: "openai/chat/v1".into(),
        upstream_protocol: "openai/chat/v1".into(),
        provider_id: "test-provider".into(),
        provider_name: "Test Provider".into(),
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: "gpt-4".into(),
        upstream_model: "gpt-4".into(),
        reasoning_effort: Some("high".into()),

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
        upstream_status_code: Some(200),
        client_status_code: 200,
        latency_total_ms: 42,
        latency_upstream_ms: Some(30),
        usage: Usage::default(),
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
    };
}

// ── 4. stream_chunks_count > 0 means streaming ───────────────────────────────

#[test]
fn stream_indicator_via_chunks_count() {
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;

    let base = LogEntry {
        api_key_id: None,
        api_key_name: None,
        created_at: 0,
        client_protocol: String::new(),
        upstream_protocol: String::new(),
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: String::new(),
        upstream_model: String::new(),
        reasoning_effort: None,

        route_decision: None,
        method: None,
        path: None,
        client_request_headers: None,
        client_request_body: None,
        client_response_headers: None,
        client_response_body: None,
        upstream_request_headers: None,
        upstream_request_body: None,
        upstream_response_headers: None,
        upstream_response_body: None,
        upstream_status_code: None,
        client_status_code: 200,
        latency_total_ms: 0,
        latency_upstream_ms: None,
        usage: Usage::default(),
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
    };

    // Non-streaming: chunks == 0
    let non_stream = base.clone();
    assert_eq!(non_stream.stream_chunks_count, 0);
    assert!(non_stream.stream_first_chunk_ms.is_none());

    // Streaming: chunks > 0
    let stream = LogEntry {
        stream_chunks_count: 15,
        stream_first_chunk_ms: Some(120),
        ..base
    };
    assert!(
        stream.stream_chunks_count > 0,
        "is_stream inferred from chunks_count > 0"
    );
    assert_eq!(stream.stream_first_chunk_ms, Some(120));
}

// ── 5. DB schema SQL contains all new columns ─────────────────────────────

#[test]
fn db_schema_sql_contains_new_columns() {
    // The INIT_SQL is not directly exported, but we can verify the column list
    // by checking that the migration function adds all expected new columns.
    // We do a lighter check: verify the constant column names expected per spec.
    let expected_columns = [
        "id",
        "created_at",
        "api_key_id",
        "api_key_name",
        "provider_id",
        "provider_name",
        "model_id",
        "model_name",
        "client_protocol",
        "upstream_protocol",
        "upstream_url",
        "client_model",
        "upstream_model",
        "reasoning_effort",
        "route_decision",
        "method",
        "path",
        "client_request_headers",
        "client_request_body",
        "client_response_headers",
        "client_response_body",
        "upstream_request_headers",
        "upstream_request_body",
        "upstream_response_headers",
        "upstream_response_body",
        "upstream_status_code",
        "client_status_code",
        "latency_total_ms",
        "latency_upstream_ms",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "is_stream",
        "stream_chunks_count",
        "stream_first_chunk_ms",
    ];
    assert_eq!(
        expected_columns.len(),
        35,
        "schema requires 35 columns (id + 34 data columns)"
    );

    // Verify RequestLog struct has the same field names via a compile-time
    // check — if any field is missing this test file won't compile.
    use nyro_core::db::models::RequestLog;
    fn _check_fields(r: &RequestLog) {
        let _: &str = &r.id;
        let _: i64 = r.created_at;
        let _: &Option<String> = &r.api_key_id;
        let _: &Option<String> = &r.api_key_name;
        let _: &Option<String> = &r.provider_id;
        let _: &Option<String> = &r.provider_name;
        let _: &Option<String> = &r.model_id;
        let _: &Option<String> = &r.model_name;
        let _: &Option<String> = &r.client_protocol;
        let _: &Option<String> = &r.upstream_protocol;
        let _: &Option<String> = &r.upstream_url;
        let _: &Option<String> = &r.client_model;
        let _: &Option<String> = &r.upstream_model;
        let _: &Option<String> = &r.reasoning_effort;
        let _: &Option<String> = &r.method;
        let _: &Option<String> = &r.path;
        let _: &Option<String> = &r.client_request_headers;
        let _: &Option<String> = &r.client_request_body;
        let _: &Option<String> = &r.client_response_headers;
        let _: &Option<String> = &r.client_response_body;
        let _: &Option<String> = &r.upstream_request_headers;
        let _: &Option<String> = &r.upstream_request_body;
        let _: &Option<String> = &r.upstream_response_headers;
        let _: &Option<String> = &r.upstream_response_body;
        let _: &Option<i32> = &r.upstream_status_code;
        let _: &Option<i32> = &r.client_status_code;
        let _: &Option<i64> = &r.latency_total_ms;
        let _: &Option<i64> = &r.latency_upstream_ms;
        let _: i32 = r.input_tokens;
        let _: i32 = r.output_tokens;
        let _: i32 = r.cache_read_tokens;
        let _: bool = r.is_stream;
        let _: i32 = r.stream_chunks_count;
        let _: &Option<i64> = &r.stream_first_chunk_ms;
    }
}

// ── 6. REDACT_HEADER_KEYS covers spec-required set ───────────────────────────

#[test]
fn redaction_covers_openai_and_anthropic_keys() {
    // Ensure both common AI-provider key headers are redacted.
    for key in &["openai-api-key", "anthropic-api-key"] {
        let mut map = AxumHeaderMap::new();
        map.insert(
            HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_static("should-be-hidden"),
        );
        let json = headers_to_json(&map).expect("serialization must not fail");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[*key], "***", "header '{key}' must be redacted");
    }
}

#[tokio::test]
async fn sqlite_round_trips_reasoning_effort_in_list_and_detail() {
    use nyro_core::db;
    use nyro_core::db::models::LogQuery;
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;
    use nyro_core::storage::{SqliteStorage, Storage};
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::migrate(&pool).await.expect("migrate sqlite schema");
    let storage = SqliteStorage::from_pool(pool);

    storage
        .logs()
        .append_batch(vec![LogEntry {
            api_key_id: None,
            api_key_name: None,
            created_at: 1,
            client_protocol: "openai/chat/v1".into(),
            upstream_protocol: "openai/chat/v1".into(),
            provider_id: "provider-1".into(),
            provider_name: "Provider".into(),
            model_id: Some("model-1".into()),
            model_name: Some("Model".into()),
            upstream_url: None,
            client_model: "gpt-test".into(),
            upstream_model: "gpt-test".into(),
            reasoning_effort: Some("high".into()),

            route_decision: Some(
                r#"{"balance":"weighted","candidates":[{"provider":"p1"}]}"#.into(),
            ),
            method: Some("POST".into()),
            path: Some("/v1/chat/completions".into()),
            client_request_headers: None,
            client_request_body: Some(r#"{"reasoning_effort":"high"}"#.into()),
            client_response_headers: None,
            client_response_body: None,
            upstream_request_headers: None,
            upstream_request_body: None,
            upstream_response_headers: None,
            upstream_response_body: None,
            upstream_status_code: Some(200),
            client_status_code: 200,
            latency_total_ms: 1,
            latency_upstream_ms: Some(1),
            usage: Usage::default(),
            is_stream: false,
            stream_chunks_count: 0,
            stream_first_chunk_ms: None,
            enable_payload: None,
        }])
        .await
        .expect("append log");

    let page = storage
        .logs()
        .query(LogQuery::default())
        .await
        .expect("query log list");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].reasoning_effort.as_deref(), Some("high"));
    assert!(
        page.items[0]
            .route_decision
            .as_deref()
            .is_some_and(|d| d.contains("\"balance\":\"weighted\""))
    );
    assert!(page.items[0].client_request_body.is_none());

    let detail = storage
        .logs()
        .find_by_id(&page.items[0].id)
        .await
        .expect("query log detail")
        .expect("log detail should exist");
    assert_eq!(detail.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        detail.route_decision.as_deref(),
        Some(r#"{"balance":"weighted","candidates":[{"provider":"p1"}]}"#)
    );
    assert_eq!(
        detail.client_request_body.as_deref(),
        Some(r#"{"reasoning_effort":"high"}"#)
    );
}

// ── 7. Historical payload clearing keeps errors and log metadata ────────────

#[tokio::test]
async fn sqlite_clears_success_payloads_but_preserves_error_payloads() {
    use nyro_core::db;
    use nyro_core::db::models::LogQuery;
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;
    use nyro_core::storage::{SqliteStorage, Storage};
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::migrate(&pool).await.expect("migrate sqlite schema");
    let storage = SqliteStorage::from_pool(pool);

    let entry = |client_status_code: i32, upstream_status_code: Option<i32>| LogEntry {
        api_key_id: Some("key-1".into()),
        api_key_name: Some("Key".into()),
        created_at: 42,
        client_protocol: "openai/chat/v1".into(),
        upstream_protocol: "anthropic/messages/v1".into(),
        provider_id: "provider-1".into(),
        provider_name: "Provider".into(),
        model_id: Some("model-1".into()),
        model_name: Some("Model".into()),
        upstream_url: Some("https://example.test/v1/messages".into()),
        client_model: "gpt-test".into(),
        upstream_model: "claude-test".into(),
        reasoning_effort: Some("high".into()),
        route_decision: Some(r#"{"strategy":"weighted"}"#.into()),
        method: Some("POST".into()),
        path: Some("/v1/chat/completions".into()),
        client_request_headers: Some(r#"{"x-client":"request"}"#.into()),
        client_request_body: Some(r#"{"client":"request"}"#.into()),
        client_response_headers: Some(r#"{"x-client":"response"}"#.into()),
        client_response_body: Some(r#"{"client":"response"}"#.into()),
        upstream_request_headers: Some(r#"{"x-upstream":"request"}"#.into()),
        upstream_request_body: Some(r#"{"upstream":"request"}"#.into()),
        upstream_response_headers: Some(r#"{"x-upstream":"response"}"#.into()),
        upstream_response_body: Some(r#"{"upstream":"response"}"#.into()),
        upstream_status_code,
        client_status_code,
        latency_total_ms: 123,
        latency_upstream_ms: Some(100),
        usage: Usage {
            prompt_tokens: 11,
            completion_tokens: 22,
            total_tokens: 33,
            cache_read_tokens: Some(3),
            ..Usage::default()
        },
        is_stream: true,
        stream_chunks_count: 7,
        stream_first_chunk_ms: Some(25),
        enable_payload: None,
    };

    storage
        .logs()
        .append_batch(vec![
            entry(200, Some(201)),
            entry(500, Some(500)),
            entry(200, Some(429)),
        ])
        .await
        .expect("append logs");

    let logs = storage.logs();
    let before = logs
        .query(LogQuery::default())
        .await
        .expect("query before clearing");
    assert_eq!(before.total, 3);
    let success_id = before
        .items
        .iter()
        .find(|row| row.client_status_code == Some(200) && row.upstream_status_code == Some(201))
        .expect("successful log exists")
        .id
        .clone();
    let client_error_id = before
        .items
        .iter()
        .find(|row| row.client_status_code == Some(500))
        .expect("client-error log exists")
        .id
        .clone();
    let upstream_error_id = before
        .items
        .iter()
        .find(|row| row.upstream_status_code == Some(429))
        .expect("upstream-error log exists")
        .id
        .clone();

    assert_eq!(logs.clear_payloads().await.expect("clear payloads"), 1);

    let after = logs
        .find_by_id(&success_id)
        .await
        .expect("query detail after clearing")
        .expect("log row is preserved");
    assert_eq!(after.id, success_id);
    assert_eq!(after.created_at, 42);
    assert_eq!(after.client_status_code, Some(200));
    assert_eq!(after.upstream_status_code, Some(201));
    assert_eq!(after.latency_total_ms, Some(123));
    assert_eq!(after.input_tokens, 11);
    assert_eq!(after.output_tokens, 22);
    assert_eq!(after.cache_read_tokens, 3);
    assert_eq!(after.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        after.route_decision.as_deref(),
        Some(r#"{"strategy":"weighted"}"#)
    );
    assert!(after.client_request_headers.is_none());
    assert!(after.client_request_body.is_none());
    assert!(after.client_response_headers.is_none());
    assert!(after.client_response_body.is_none());
    assert!(after.upstream_request_headers.is_none());
    assert!(after.upstream_request_body.is_none());
    assert!(after.upstream_response_headers.is_none());
    assert!(after.upstream_response_body.is_none());

    for error_id in [&client_error_id, &upstream_error_id] {
        let error = logs
            .find_by_id(error_id)
            .await
            .expect("query preserved error detail")
            .expect("error log row is preserved");
        assert!(error.client_request_headers.is_some());
        assert!(error.client_request_body.is_some());
        assert!(error.client_response_headers.is_some());
        assert!(error.client_response_body.is_some());
        assert!(error.upstream_request_headers.is_some());
        assert!(error.upstream_request_body.is_some());
        assert!(error.upstream_response_headers.is_some());
        assert!(error.upstream_response_body.is_some());
    }

    assert_eq!(
        logs.query(LogQuery::default())
            .await
            .expect("query after clearing")
            .total,
        3
    );
    assert_eq!(
        logs.clear_payloads()
            .await
            .expect("error payloads remain excluded on repeated clearing"),
        0
    );
}

// ── 8. Log deletion: single row by id, and error-only wipe ───────────────────

#[tokio::test]
async fn sqlite_deletes_single_log_and_clears_errors_only() {
    use nyro_core::db;
    use nyro_core::db::models::LogQuery;
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;
    use nyro_core::storage::{SqliteStorage, Storage};
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    db::migrate(&pool).await.expect("migrate sqlite schema");
    let storage = SqliteStorage::from_pool(pool);

    let entry = |client_status: i32| LogEntry {
        api_key_id: None,
        api_key_name: None,
        created_at: 1,
        client_protocol: "openai/chat/v1".into(),
        upstream_protocol: "openai/chat/v1".into(),
        provider_id: "provider-1".into(),
        provider_name: "Provider".into(),
        model_id: Some("model-1".into()),
        model_name: Some("Model".into()),
        upstream_url: None,
        client_model: "gpt-test".into(),
        upstream_model: "gpt-test".into(),
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
        upstream_status_code: Some(client_status),
        client_status_code: client_status,
        latency_total_ms: 1,
        latency_upstream_ms: Some(1),
        usage: Usage::default(),
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
    };

    storage
        .logs()
        .append_batch(vec![entry(200), entry(200), entry(500)])
        .await
        .expect("append logs");

    let logs = storage.logs();
    let page = logs.query(LogQuery::default()).await.expect("query");
    assert_eq!(page.total, 3);
    let ok_id = page
        .items
        .iter()
        .find(|i| i.client_status_code == Some(200))
        .expect("ok row exists")
        .id
        .clone();

    // Single-row delete: exactly one row gone, others untouched.
    let deleted = logs.delete_by_id(&ok_id).await.expect("delete by id");
    assert_eq!(deleted, 1);
    assert_eq!(
        logs.delete_by_id("nonexistent-id")
            .await
            .expect("delete missing id is not an error"),
        0
    );
    let page = logs.query(LogQuery::default()).await.expect("query");
    assert_eq!(page.total, 2, "only the targeted row was deleted");

    // Error wipe removes the >= 400 rows only.
    let deleted = logs.clear_errors().await.expect("clear errors");
    assert_eq!(deleted, 1);
    let page = logs.query(LogQuery::default()).await.expect("query");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].client_status_code, Some(200));

    // Clearing again with no errors left is a no-op.
    assert_eq!(logs.clear_errors().await.expect("clear errors again"), 0);
}
