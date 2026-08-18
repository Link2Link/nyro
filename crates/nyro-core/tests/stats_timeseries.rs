use nyro_core::db;
use nyro_core::logging::LogEntry;
use nyro_core::protocol::ir::Usage;
use nyro_core::storage::{SqliteStorage, Storage};
use sqlx::sqlite::SqlitePoolOptions;

const MINUTE_MS: i64 = 60_000;

fn log_entry(
    created_at: i64,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    status: i32,
    latency_ms: i64,
) -> LogEntry {
    LogEntry {
        api_key_id: None,
        api_key_name: None,
        created_at,
        client_protocol: "openai/chat/v1".into(),
        upstream_protocol: "openai/chat/v1".into(),
        provider_id: "provider-1".into(),
        provider_name: "Provider".into(),
        model_id: Some("model-1".into()),
        model_name: Some("Model".into()),
        upstream_url: None,
        client_model: "test-model".into(),
        upstream_model: "test-model".into(),
        reasoning_effort: None,
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
        upstream_status_code: Some(status),
        client_status_code: status,
        latency_total_ms: latency_ms,
        latency_upstream_ms: Some(latency_ms),
        usage: Usage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            total_tokens: input_tokens + output_tokens,
            cache_read_tokens: Some(cache_read_tokens),
            ..Default::default()
        },
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
    }
}

#[tokio::test]
async fn sqlite_aggregates_epoch_buckets_and_boundaries() -> anyhow::Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    db::migrate(&pool).await?;
    let storage = SqliteStorage::from_pool(pool);
    let bucket_ms = 5 * MINUTE_MS;

    storage
        .logs()
        .append_batch(vec![
            log_entry(MINUTE_MS, 10, 5, 2, 200, 100),
            log_entry(4 * MINUTE_MS, 20, 7, 3, 500, 300),
            log_entry(bucket_ms, 7, 3, 2, 200, 500),
        ])
        .await?;

    let buckets = storage
        .logs()
        .stats_time_buckets(0, 10 * MINUTE_MS, bucket_ms)
        .await?;

    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].bucket_start, 0);
    assert_eq!(buckets[0].request_count, 2);
    assert_eq!(buckets[0].error_count, 1);
    assert_eq!(buckets[0].total_input_tokens, 30);
    assert_eq!(buckets[0].total_output_tokens, 12);
    assert_eq!(buckets[0].total_cache_read_tokens, 5);
    assert_eq!(buckets[0].avg_duration_ms, Some(200.0));

    assert_eq!(buckets[1].bucket_start, bucket_ms);
    assert_eq!(buckets[1].request_count, 1);
    assert_eq!(buckets[1].error_count, 0);
    assert_eq!(buckets[1].total_input_tokens, 7);
    assert_eq!(buckets[1].total_output_tokens, 3);
    assert_eq!(buckets[1].total_cache_read_tokens, 2);
    assert_eq!(buckets[1].avg_duration_ms, Some(500.0));

    assert!(
        storage
            .logs()
            .stats_time_buckets(0, 10 * MINUTE_MS, 0)
            .await
            .is_err()
    );

    Ok(())
}
