//! Cross-backend contract for the shared TPS sums on the eight aggregate
//! DTOs (ModelStats, ProviderStats, ProviderUsageDetail,
//! ProviderModelUsageStats, ModelUsageDetail, ModelProviderUsageStats,
//! ModelApiKeyUsageStats, ApiKeyModelRouteStats).
//!
//! Rows come from the shared fixture tests/fixtures/tps-contract.json plus
//! two extreme valid samples outside the detail-query window. Expected
//! numbers are hand-computed constants, independent of the helper under
//! test: fixture window sums content 760 / output 1100 / elapsed 12500;
//! with the out-of-window rows 1_000_759 / 1_013_444 / 14_000.

use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/tps-contract.json");
const START: i64 = 1_700_000_000_000;
const END: i64 = 1_700_000_020_000;

#[derive(Debug, Deserialize)]
struct Case {
    log: LogFields,
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

#[derive(Debug, Clone)]
struct Row {
    id: String,
    created_at: i64,
    has_api_key: bool,
    output_tokens: Option<i32>,
    reasoning_tokens: Option<i32>,
    is_stream: bool,
    stream_chunks_count: i32,
    latency_upstream_ms: Option<i64>,
    latency_total_ms: Option<i64>,
    stream_first_chunk_ms: Option<i64>,
}

fn rows() -> Vec<Row> {
    let cases: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(FIXTURE).expect("shared fixture readable"),
    )
    .expect("shared fixture parses");
    let mut rows: Vec<Row> = cases
        .iter()
        .enumerate()
        .map(|(index, case)| Row {
            id: format!("agg-fixture-{index:02}"),
            created_at: START + index as i64,
            has_api_key: true,
            output_tokens: case.log.output_tokens,
            reasoning_tokens: case.log.reasoning_tokens,
            is_stream: case.log.is_stream,
            stream_chunks_count: case.log.stream_chunks_count,
            latency_upstream_ms: case.log.latency_upstream_ms,
            latency_total_ms: case.log.latency_total_ms,
            stream_first_chunk_ms: case.log.stream_first_chunk_ms,
        })
        .collect();
    // Extreme valid samples outside [START, END]: detail queries must
    // exclude them, while the unfiltered stats_by_* views include them.
    rows.push(Row {
        id: "agg-out-before".into(),
        created_at: START - 100_000,
        has_api_key: false,
        output_tokens: Some(999_999),
        reasoning_tokens: Some(0),
        is_stream: false,
        stream_chunks_count: 0,
        latency_upstream_ms: Some(1_000),
        latency_total_ms: Some(1_001),
        stream_first_chunk_ms: None,
    });
    rows.push(Row {
        id: "agg-out-after".into(),
        created_at: END + 100_000,
        has_api_key: false,
        output_tokens: Some(12_345),
        reasoning_tokens: Some(12_345),
        is_stream: true,
        stream_chunks_count: 3,
        latency_upstream_ms: Some(500),
        latency_total_ms: Some(600),
        stream_first_chunk_ms: Some(50),
    });
    rows
}

/// Distinguishable samples written a few seconds before now under a dedicated
/// pair (provider tpsp-recent / model tps-recent): pooled they give 150
/// content / 300 output / 3000 elapsed, exercising the 24 h stats branches
/// that the historical fixture rows (November 2023) can never reach.
fn recent_rows() -> Vec<Row> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as i64;
    vec![
        Row {
            id: "agg-recent-a".into(),
            created_at: now - 5_000,
            has_api_key: false,
            output_tokens: Some(200),
            reasoning_tokens: Some(50),
            is_stream: true,
            stream_chunks_count: 4,
            latency_upstream_ms: Some(2_000),
            latency_total_ms: Some(2_100),
            stream_first_chunk_ms: Some(200),
        },
        Row {
            id: "agg-recent-b".into(),
            created_at: now - 60_000,
            has_api_key: false,
            output_tokens: Some(100),
            reasoning_tokens: Some(100),
            is_stream: false,
            stream_chunks_count: 0,
            latency_upstream_ms: Some(1_000),
            latency_total_ms: Some(1_100),
            stream_first_chunk_ms: None,
        },
    ]
}

async fn exercise(storage: &dyn Storage) -> anyhow::Result<()> {
    // ---- detail queries: fixture window only ----
    let provider_detail = storage
        .logs()
        .provider_usage_detail("tpsp", START, END)
        .await?;
    assert_eq!(provider_detail.request_count, 17);
    assert_eq!(provider_detail.tps_totals.tps_content_tokens, 760);
    assert_eq!(provider_detail.tps_totals.tps_output_tokens, 1_100);
    assert_eq!(provider_detail.tps_totals.tps_elapsed_ms, 12_500);
    assert_eq!(provider_detail.tps_totals.content_tps(), Some(60.8));
    assert_eq!(provider_detail.tps_totals.gross_tps(), Some(88.0));
    // Legacy fields keep their old semantics over the same window.
    assert_eq!(provider_detail.total_output_tokens, 1_495);
    assert!((provider_detail.total_upstream_ms - 13_400.0).abs() < 1e-6);
    assert_eq!(provider_detail.models.len(), 1);
    assert_eq!(provider_detail.models[0].upstream_model, "tps-model");
    assert_eq!(provider_detail.models[0].tps_totals.tps_content_tokens, 760);
    assert_eq!(provider_detail.models[0].tps_totals.tps_output_tokens, 1_100);
    assert_eq!(provider_detail.models[0].tps_totals.tps_elapsed_ms, 12_500);

    let model_detail = storage
        .logs()
        .model_usage_detail("tps-model", START, END)
        .await?;
    assert_eq!(model_detail.request_count, 17);
    assert_eq!(model_detail.tps_totals.tps_content_tokens, 760);
    assert_eq!(model_detail.tps_totals.tps_output_tokens, 1_100);
    assert_eq!(model_detail.tps_totals.tps_elapsed_ms, 12_500);
    assert_eq!(model_detail.providers.len(), 1);
    assert_eq!(model_detail.providers[0].tps_totals.tps_elapsed_ms, 12_500);
    assert_eq!(model_detail.providers[0].tps_totals.tps_content_tokens, 760);
    assert_eq!(model_detail.api_keys.len(), 1);
    assert_eq!(model_detail.api_keys[0].tps_totals.tps_content_tokens, 760);
    assert_eq!(model_detail.api_keys[0].tps_totals.tps_output_tokens, 1_100);

    let key_detail = storage
        .logs()
        .api_key_usage_detail("tps-key", START, END)
        .await?;
    assert_eq!(key_detail.request_count, 17);
    assert_eq!(key_detail.model_routes.len(), 1);
    assert_eq!(key_detail.model_routes[0].tps_totals.tps_content_tokens, 760);
    assert_eq!(key_detail.model_routes[0].tps_totals.tps_output_tokens, 1_100);
    assert_eq!(key_detail.model_routes[0].tps_totals.tps_elapsed_ms, 12_500);

    // ---- unfiltered stats_by_* views: fixture + out-of-window rows ----
    let by_model = storage.logs().stats_by_model(None).await?;
    let model = by_model
        .iter()
        .find(|m| m.model == "tps-model")
        .expect("tps-model group");
    assert_eq!(model.request_count, 19);
    assert_eq!(model.tps_totals.tps_content_tokens, 1_000_759);
    assert_eq!(model.tps_totals.tps_output_tokens, 1_013_444);
    assert_eq!(model.tps_totals.tps_elapsed_ms, 14_000);
    assert_eq!(model.total_output_tokens, 1_013_839);
    assert!((model.total_upstream_ms - 14_900.0).abs() < 1e-6);

    let by_provider = storage.logs().stats_by_provider(None).await?;
    let provider = by_provider
        .iter()
        .find(|p| p.provider_id == "tpsp")
        .expect("tpsp group");
    assert_eq!(provider.request_count, 19);
    assert_eq!(provider.tps_totals.tps_content_tokens, 1_000_759);
    assert_eq!(provider.tps_totals.tps_output_tokens, 1_013_444);
    assert_eq!(provider.tps_totals.tps_elapsed_ms, 14_000);
    assert_eq!(provider.total_output_tokens, 1_013_839);

    // ---- hours-filtered stats branches: only the recent pair qualifies ----
    // The historical fixture pair and both out-of-window extremes are years
    // old, so they must not pass the 24 h filter (group lookups, not global
    // lengths: external databases may retain rows from other test runs).
    let day_by_model = storage.logs().stats_by_model(Some(24)).await?;
    let recent_model = day_by_model
        .iter()
        .find(|m| m.model == "tps-recent")
        .expect("recent model group inside the 24 h window");
    assert_eq!(recent_model.request_count, 2);
    assert_eq!(recent_model.tps_totals.tps_content_tokens, 150);
    assert_eq!(recent_model.tps_totals.tps_output_tokens, 300);
    assert_eq!(recent_model.tps_totals.tps_elapsed_ms, 3_000);
    assert_eq!(recent_model.tps_totals.content_tps(), Some(50.0));
    assert_eq!(recent_model.tps_totals.gross_tps(), Some(100.0));
    assert!(
        day_by_model.iter().all(|m| m.model != "tps-model"),
        "historical fixture rows must not pass the 24 h filter"
    );

    let day_by_provider = storage.logs().stats_by_provider(Some(24)).await?;
    let recent_provider = day_by_provider
        .iter()
        .find(|p| p.provider_id == "tpsp-recent")
        .expect("recent provider group inside the 24 h window");
    assert_eq!(recent_provider.tps_totals.tps_content_tokens, 150);
    assert_eq!(recent_provider.tps_totals.tps_output_tokens, 300);
    assert_eq!(recent_provider.tps_totals.tps_elapsed_ms, 3_000);
    assert!(
        day_by_provider.iter().all(|p| p.provider_id != "tpsp"),
        "historical fixture provider must not pass the 24 h filter"
    );
    Ok(())
}

const COLS: &str = "(id, created_at, provider_id, provider_name, api_key_id, api_key_name, client_model, upstream_model, output_tokens, reasoning_tokens, is_stream, stream_chunks_count, latency_upstream_ms, latency_total_ms, stream_first_chunk_ms)";

#[tokio::test]
async fn sqlite_aggregate_tps_totals() -> anyhow::Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().in_memory(true).foreign_keys(true))
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    sqlx::query("DELETE FROM request_logs WHERE provider_id IN ('tpsp', 'tpsp-recent')")
        .execute(storage.pool())
        .await?;
    for row in rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES (?, ?, 'tpsp', 'TPS Provider', ?, 'tps-key-name', 'tps-client', 'tps-model', ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.has_api_key.then_some("tps-key"))
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    insert_recent_sqlite(&storage).await?;
    exercise(&storage).await
}

async fn insert_recent_sqlite(storage: &SqliteStorage) -> anyhow::Result<()> {
    for row in recent_rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES (?, ?, 'tpsp-recent', 'TPS Recent Provider', NULL, NULL, 'tps-client', 'tps-recent', ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    Ok(())
}

fn external(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_PERFORMANCE_DATABASES_ONLY").as_deref() == Ok("1"),
        "explicit disposable database consent required"
    );
    let parsed = reqwest::Url::parse(&url)?;
    let database = parsed.path().trim_start_matches('/');
    anyhow::ensure!(
        database.starts_with("nyro_test_") || database.ends_with("_test"),
        "refusing non-test database"
    );
    Ok(Some(SqlBackendConfig {
        max_connections: 1,
        ..SqlBackendConfig::with_url(url)
    }))
}

#[tokio::test]
async fn postgres_aggregate_tps_totals_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_POSTGRES_PERFORMANCE_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    sqlx::query("DELETE FROM request_logs WHERE provider_id IN ($1, $2)")
        .bind("tpsp")
        .bind("tpsp-recent")
        .execute(storage.pool())
        .await?;
    for row in recent_rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES ($1, $2, 'tpsp-recent', 'TPS Recent Provider', NULL, NULL, 'tps-client', 'tps-recent', $3, $4, $5, $6, $7, $8, $9)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    for row in rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES ($1, $2, 'tpsp', 'TPS Provider', $3, 'tps-key-name', 'tps-client', 'tps-model', $4, $5, $6, $7, $8, $9, $10)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.has_api_key.then_some("tps-key"))
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    exercise(&storage).await
}

#[tokio::test]
async fn mysql_aggregate_tps_totals_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_MYSQL_PERFORMANCE_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    sqlx::query("DELETE FROM request_logs WHERE provider_id IN (?, ?)")
        .bind("tpsp")
        .bind("tpsp-recent")
        .execute(storage.pool())
        .await?;
    for row in recent_rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES (?, ?, 'tpsp-recent', 'TPS Recent Provider', NULL, NULL, 'tps-client', 'tps-recent', ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    for row in rows() {
        sqlx::query(&format!(
            "INSERT INTO request_logs{COLS} VALUES (?, ?, 'tpsp', 'TPS Provider', ?, 'tps-key-name', 'tps-client', 'tps-model', ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&row.id)
        .bind(row.created_at)
        .bind(row.has_api_key.then_some("tps-key"))
        .bind(row.output_tokens)
        .bind(row.reasoning_tokens)
        .bind(row.is_stream)
        .bind(row.stream_chunks_count)
        .bind(row.latency_upstream_ms)
        .bind(row.latency_total_ms)
        .bind(row.stream_first_chunk_ms)
        .execute(storage.pool())
        .await?;
    }
    exercise(&storage).await
}
