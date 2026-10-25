//! Permanent token usage (`token_usage_daily`) on PostgreSQL and MySQL: every
//! log deletion path folds exactly the deleted rows, preserving merged totals.
//! SQLite coverage lives in `token_usage.rs`. External tests require dedicated,
//! disposable databases and never read application/production configuration:
//! set NYRO_TEST_POSTGRES_USAGE_URL / NYRO_TEST_MYSQL_USAGE_URL to a database
//! named `nyro_test_*` or `*_test`, plus NYRO_TEST_USAGE_DATABASES_ONLY=1.
use std::collections::BTreeMap;

use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, Storage};

type Key = (String, String, String);
type Totals = (i64, i64, i64, i64, i64, i64);

fn external(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_USAGE_DATABASES_ONLY").as_deref() == Ok("1"),
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

/// Retained plus folded usage over all time, keyed like the admin merge.
async fn merged(storage: &dyn Storage) -> anyhow::Result<BTreeMap<Key, Totals>> {
    let (start, end) = (0, 253_402_300_800_000); // [1970-01-01, 10000-01-01)
    let logs = storage.logs();
    let mut out: BTreeMap<Key, Totals> = BTreeMap::new();
    for row in logs
        .token_usage_rollup_days(start, end)
        .await?
        .into_iter()
        .chain(logs.token_usage_days(start, end).await?)
    {
        let t = out
            .entry((row.date, row.provider_id, row.model))
            .or_default();
        t.0 += row.request_count;
        t.1 += row.input_tokens;
        t.2 += row.output_tokens;
        t.3 += row.cache_read_tokens;
        t.4 += row.missing_usage_count;
        t.5 += row.cache_read_reported_count;
    }
    Ok(out)
}

/// Multi-row literal insert valid on both PostgreSQL and MySQL. 2024-03-01
/// rows are old enough for any retention window; `fresh` is not.
fn seed_sql(now: i64) -> String {
    let day = 1_709_251_200_000_i64; // 2024-03-01T00:00:00Z
    let rows = [
        format!("('ok', {day}, 'p', 'model', 100, 10, 5, 200, 1, 'completed')"),
        format!(
            "('err', {}, 'p', 'model', 7, 3, NULL, 429, 1, 'failed')",
            day + 1
        ),
        format!(
            "('ok2', {}, 'p', 'model', 1, 1, 1, 200, 1, 'completed')",
            day + 2
        ),
        format!("('upper', {day}, 'p', 'Model', 2, 2, 0, 200, 1, 'completed')"),
        format!("('space', {day}, 'p', 'model ', 3, 3, NULL, 200, 1, 'completed')"),
        format!("('uni', {day}, 'p', '模型-α', 4, NULL, 4, 200, 1, 'completed')"),
        format!("('no-model', {day}, 'p', NULL, 5, 5, NULL, 200, 1, 'completed')"),
        format!("('pre-routing', {day}, NULL, NULL, 0, 0, NULL, 404, 1, 'failed')"),
        format!("('fresh', {now}, 'p', 'model', 9, 9, 9, 200, 1, 'completed')"),
    ];
    format!(
        "INSERT INTO request_logs (id, created_at, provider_id, upstream_model, input_tokens, \
         output_tokens, cache_read_tokens, upstream_status_code, outcome_version, attempt_outcome) \
         VALUES {}",
        rows.join(", ")
    )
}

async fn assert_every_deletion_path_preserves_totals(storage: &dyn Storage) -> anyhow::Result<()> {
    let expected = merged(storage).await?;
    assert_eq!(
        expected.len(),
        6,
        "case, trailing space, unicode and NULL model stay distinct"
    );
    assert_eq!(
        expected[&("2024-03-01".into(), "p".into(), "model".into())],
        (3, 108, 14, 6, 0, 2)
    );
    assert_eq!(
        expected[&("2024-03-01".into(), "p".into(), "模型-α".into())],
        (1, 4, 0, 4, 1, 1)
    );
    let logs = storage.logs();
    assert_eq!(logs.delete_by_id("ok").await?, 1);
    assert_eq!(merged(storage).await?, expected);
    // Same key as the single delete: the second fold must accumulate.
    assert_eq!(logs.clear_errors().await?, 2);
    assert_eq!(merged(storage).await?, expected);
    assert_eq!(logs.clear_payloads().await?, 0);
    assert_eq!(logs.cleanup_before("-7 days").await?, 5);
    assert_eq!(merged(storage).await?, expected);
    assert_eq!(logs.clear_all().await?, 1, "only the fresh row is left");
    assert_eq!(merged(storage).await?, expected);
    assert_eq!(logs.clear_all().await?, 0);
    assert_eq!(logs.cleanup_before("-7 days").await?, 0);
    assert_eq!(
        merged(storage).await?,
        expected,
        "repeats fold nothing twice"
    );
    assert!(storage.bootstrap().health().await?.schema_compatible);
    Ok(())
}

#[tokio::test]
async fn postgres_token_usage_rollup_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_POSTGRES_USAGE_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    sqlx::raw_sql(
        "DELETE FROM request_logs; DELETE FROM request_results; DELETE FROM token_usage_daily;",
    )
    .execute(storage.pool())
    .await?;
    sqlx::raw_sql(&seed_sql(chrono::Utc::now().timestamp_millis()))
        .execute(storage.pool())
        .await?;
    assert_every_deletion_path_preserves_totals(&storage).await
}

#[tokio::test]
async fn mysql_token_usage_rollup_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_MYSQL_USAGE_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    sqlx::raw_sql(
        "DELETE FROM request_logs; DELETE FROM request_results; DELETE FROM token_usage_daily;",
    )
    .execute(storage.pool())
    .await?;
    sqlx::raw_sql(&seed_sql(chrono::Utc::now().timestamp_millis()))
        .execute(storage.pool())
        .await?;
    assert_every_deletion_path_preserves_totals(&storage).await
}
