//! Shared TPS contract fixture (tests/fixtures/tps-contract.json) drives the
//! single-sample and latest-fifty semantics; the WebUI reads the same file.
//!
//! Expected aggregates below are hand-computed constants (17 cases, 10
//! valid: content 760 / output 1100 / elapsed 12500 ms) so the tests never
//! re-implement the helper under test.

use nyro_core::db::models::{ModelUsageStats, ModelUsageTotals, RecentModelPerformance};
use nyro_core::storage::{SqliteStorage, Storage};
use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/tps-contract.json"
);
const AS_OF: i64 = 1_700_000_000_000;

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    log: LogFields,
    content_tokens: i32,
    tps: Option<f64>,
    gross_tps: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LogFields {
    output_tokens: Option<i32>,
    reasoning_tokens: Option<i32>,
    #[serde(default)]
    is_stream: bool,
    #[serde(default)]
    stream_chunks_count: i32,
    latency_upstream_ms: Option<i64>,
    latency_total_ms: Option<i64>,
    stream_first_chunk_ms: Option<i64>,
}

fn load() -> Vec<Case> {
    let raw = std::fs::read_to_string(FIXTURE).expect("shared fixture readable");
    serde_json::from_str(&raw).expect("shared fixture parses")
}

fn to_sample(log: &LogFields) -> RecentModelPerformance {
    RecentModelPerformance {
        output_tokens: log.output_tokens.unwrap_or(0),
        reasoning_tokens: log.reasoning_tokens.unwrap_or(0),
        is_stream: log.is_stream,
        stream_chunks_count: log.stream_chunks_count,
        latency_upstream_ms: log.latency_upstream_ms,
        latency_total_ms: log.latency_total_ms,
        stream_first_chunk_ms: log.stream_first_chunk_ms,
    }
}

#[test]
fn fixture_matches_single_sample_semantics() {
    let cases = load();
    assert_eq!(cases.len(), 17);
    for case in &cases {
        let sample = to_sample(&case.log);
        assert_eq!(
            sample.content_tokens(),
            case.content_tokens,
            "{}: content_tokens",
            case.name
        );
        assert_eq!(sample.tps(), case.tps, "{}: tps", case.name);
        assert_eq!(
            sample.gross_tps(),
            case.gross_tps,
            "{}: gross_tps",
            case.name
        );
        assert_eq!(
            sample.tps().is_some(),
            sample.gross_tps().is_some(),
            "{}: content and gross share one validity pool",
            case.name
        );
    }
}

#[test]
fn fixture_aggregates_are_latency_weighted_not_averaged() {
    let cases = load();
    let samples: Vec<_> = cases.iter().map(|c| to_sample(&c.log)).collect();
    let stats = ModelUsageStats::from_samples(ModelUsageTotals::default(), &samples);

    assert_eq!(stats.recent_sample_count, 17);
    assert_eq!(stats.valid_tps_count, 10);
    assert_eq!(stats.recent_content_tokens, 760);
    assert_eq!(stats.recent_latency_ms, 12_500);
    // 760 content tokens / 12.5 s and 1100 output tokens / 12.5 s; an
    // arithmetic mean of per-request rates would give a different number.
    assert_eq!(stats.average_tps, Some(60.8));
    assert_eq!(stats.average_gross_tps, Some(88.0));
    assert_eq!(stats.overall_tps, stats.average_tps);
    assert_eq!(stats.overall_gross_tps, stats.average_gross_tps);
}

async fn sqlite() -> anyhow::Result<SqliteStorage> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    Ok(storage)
}

#[tokio::test]
async fn sqlite_readback_matches_fixture_contract() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    let cases = load();
    for (index, case) in cases.iter().enumerate() {
        sqlx::query(
            "INSERT INTO request_logs(id, created_at, provider_id, upstream_model, output_tokens, reasoning_tokens, is_stream, stream_chunks_count, latency_upstream_ms, latency_total_ms, stream_first_chunk_ms) VALUES (?, ?, 'tpsp', 'tps-model', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format!("contract-{index:02}"))
        .bind(AS_OF + index as i64)
        .bind(case.log.output_tokens)
        .bind(case.log.reasoning_tokens)
        .bind(case.log.is_stream)
        .bind(case.log.stream_chunks_count)
        .bind(case.log.latency_upstream_ms)
        .bind(case.log.latency_total_ms)
        .bind(case.log.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }

    let usage = storage
        .logs()
        .model_usage_stats("tpsp", "tps-model")
        .await?;
    assert_eq!(usage.recent_sample_count, 17);
    assert_eq!(usage.valid_tps_count, 10);
    assert_eq!(usage.recent_content_tokens, 760);
    assert_eq!(usage.recent_latency_ms, 12_500);
    assert_eq!(usage.average_tps, Some(60.8));
    assert_eq!(usage.average_gross_tps, Some(88.0));
    assert_eq!(usage.overall_tps, usage.average_tps);
    assert_eq!(usage.overall_gross_tps, usage.average_gross_tps);

    let stats = storage
        .logs()
        .model_performance_stats(&[("tpsp".into(), "tps-model".into())], AS_OF + 100)
        .await?;
    let mixed = &stats[0].mixed;
    assert_eq!(mixed.selected_request_count, 17);
    assert_eq!(mixed.valid_tps_count, 10);
    assert_eq!(mixed.total_content_tokens, 760);
    assert_eq!(mixed.total_output_tokens, 1_100);
    assert_eq!(mixed.total_latency_ms, 12_500);
    assert_eq!(mixed.average_tps, Some(60.8));
    assert_eq!(mixed.average_gross_tps, Some(88.0));
    assert_eq!(mixed.overall_tps, mixed.average_tps);
    assert_eq!(mixed.overall_gross_tps, mixed.average_gross_tps);
    // first/last cover valid samples only (indexes 0..=9 of the fixture).
    assert_eq!(mixed.first_sample_at, Some(AS_OF));
    assert_eq!(mixed.last_sample_at, Some(AS_OF + 9));
    Ok(())
}

#[tokio::test]
async fn sqlite_latest_fifty_window_selects_before_validation() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    // 50 invalid recent rows + 5 older valid rows: the latest-fifty window
    // contains only the invalid ones, so nothing is backfilled and the
    // aggregates stay empty (select first, validate after, no refill).
    for index in 0..50i64 {
        sqlx::query(
            "INSERT INTO request_logs(id, created_at, provider_id, upstream_model, output_tokens, latency_upstream_ms) VALUES (?, ?, 'tpsp', 'window', 0, 1000)",
        )
        .bind(format!("win-recent-{index:02}"))
        .bind(AS_OF + index)
        .execute(storage.pool())
        .await?;
    }
    for index in 0..5i64 {
        sqlx::query(
            "INSERT INTO request_logs(id, created_at, provider_id, upstream_model, output_tokens, latency_upstream_ms) VALUES (?, ?, 'tpsp', 'window', 600, 6000)",
        )
        .bind(format!("win-old-{index:02}"))
        .bind(AS_OF - 1000 + index)
        .execute(storage.pool())
        .await?;
    }
    let usage = storage.logs().model_usage_stats("tpsp", "window").await?;
    assert_eq!(usage.recent_sample_count, 50);
    assert_eq!(usage.valid_tps_count, 0);
    assert_eq!(usage.average_tps, None);
    assert_eq!(usage.recent_latency_ms, 0);

    let stats = storage
        .logs()
        .model_performance_stats(&[("tpsp".into(), "window".into())], AS_OF)
        .await?;
    assert_eq!(stats[0].mixed.selected_request_count, 50);
    assert_eq!(stats[0].mixed.valid_tps_count, 0);
    assert_eq!(stats[0].mixed.average_tps, None);
    assert_eq!(stats[0].mixed.first_sample_at, None);
    Ok(())
}
