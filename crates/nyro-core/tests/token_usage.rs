use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::config::GatewayConfig;
use nyro_core::db::{self, models::*};
use nyro_core::storage::{MemoryStorage, SqliteStorage, Storage};
use sqlx::sqlite::SqlitePoolOptions;

fn millis(date: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(date)
        .unwrap()
        .timestamp_millis()
}

async fn insert(
    pool: &sqlx::SqlitePool,
    at: i64,
    provider: Option<&str>,
    model: Option<&str>,
    usage: (Option<i32>, Option<i32>, Option<i32>),
    status: i32,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    insert_with_id(pool, &id, at, provider, model, usage, status).await
}

async fn insert_with_id(
    pool: &sqlx::SqlitePool,
    id: &str,
    at: i64,
    provider: Option<&str>,
    model: Option<&str>,
    usage: (Option<i32>, Option<i32>, Option<i32>),
    status: i32,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, provider_name, client_model, upstream_model, input_tokens, output_tokens, cache_read_tokens, upstream_status_code, client_status_code, outcome_version, attempt_outcome) VALUES (?, ?, ?, 'Old provider name', 'client-alias', ?, ?, ?, ?, ?, 200, 1, ?)")
        .bind(id).bind(at).bind(provider).bind(model)
        .bind(usage.0).bind(usage.1).bind(usage.2).bind(status)
        .bind(if status >= 400 { "failed" } else { "completed" })
        .execute(pool).await?;
    Ok(())
}

/// Order-preserving projection of every counted field of a usage day.
type UsageKey = (String, String, String, i64, i64, i64, i64, i64, i64);

fn usage_keys(rows: &[TokenUsageDay]) -> Vec<UsageKey> {
    rows.iter()
        .map(|r| {
            (
                r.date.clone(),
                r.provider_id.clone(),
                r.model.clone(),
                r.request_count,
                r.input_tokens,
                r.output_tokens,
                r.cache_read_tokens,
                r.missing_usage_count,
                r.cache_read_reported_count,
            )
        })
        .collect()
}

async fn log_count(pool: &sqlx::SqlitePool) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
        .fetch_one(pool)
        .await?)
}

async fn storage() -> anyhow::Result<SqliteStorage> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    db::migrate(&pool).await?;
    Ok(SqliteStorage::from_pool(pool))
}

#[tokio::test]
async fn token_usage_half_open_utc_year_days_and_provider_model_identity() -> anyhow::Result<()> {
    let storage = storage().await?;
    let start = millis("2024-01-01T00:00:00Z");
    let end = millis("2025-01-01T00:00:00Z");
    let leap_day = millis("2024-02-29T00:00:00Z");
    for (at, provider, model) in [
        (start - 1, "p1", "shared"),
        (start, "p1", "shared"),
        (leap_day - 1, "p1", "shared"),
        (leap_day, "p1", "shared"),
        (leap_day, "p2", "shared"),
        (leap_day, "p2", "Shared"),
        (end - 1, "p1", "shared"),
        (end, "p1", "shared"),
    ] {
        insert(
            storage.pool(),
            at,
            Some(provider),
            Some(model),
            (Some(10), Some(2), Some(3)),
            200,
        )
        .await?;
    }
    let rows = storage.logs().token_usage_days(start, end).await?;
    let identities: Vec<_> = rows
        .iter()
        .map(|r| (r.date.as_str(), r.provider_id.as_str(), r.model.as_str()))
        .collect();
    assert_eq!(
        identities,
        vec![
            ("2024-01-01", "p1", "shared"),
            ("2024-02-28", "p1", "shared"),
            ("2024-02-29", "p1", "shared"),
            ("2024-02-29", "p2", "Shared"),
            ("2024-02-29", "p2", "shared"),
            ("2024-12-31", "p1", "shared"),
        ]
    );
    assert!(rows.iter().all(|r| r.request_count == 1));
    assert!(
        rows.iter().all(|r| r.cost.is_none()),
        "storage does not persist pricing"
    );
    assert!(
        storage
            .logs()
            .token_usage_days(start, start)
            .await?
            .is_empty()
    );
    assert_eq!(
        storage
            .logs()
            .token_usage_days(leap_day - 1, leap_day)
            .await?[0]
            .date,
        "2024-02-28"
    );
    Ok(())
}

#[tokio::test]
async fn token_usage_null_zero_gross_input_and_error_attempts() -> anyhow::Result<()> {
    let storage = storage().await?;
    let at = millis("2024-06-01T00:00:00Z");
    for (usage, status) in [
        ((None, None, None), 200),
        ((Some(0), Some(0), Some(0)), 200),
        ((Some(20), Some(7), Some(12)), 200),
        ((Some(9), None, None), 200),
        ((None, Some(3), None), 200),
        // Failed attempts can incur billable usage, even if a later attempt succeeds.
        ((Some(4), Some(2), Some(1)), 429),
    ] {
        insert(
            storage.pool(),
            at,
            Some("p"),
            Some("upstream"),
            usage,
            status,
        )
        .await?;
    }
    // Final request outcomes are not extra upstream attempts.
    sqlx::query("INSERT INTO request_results (client_request_id, final_outcome, attempt_count, finished_at) VALUES ('request', 'completed', 2, ?)")
        .bind(at).execute(storage.pool()).await?;
    let rows = storage.logs().token_usage_days(at, at + 1).await?;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.request_count, 6);
    assert_eq!(row.input_tokens, 33, "gross input must not add cache reads");
    assert_eq!(row.output_tokens, 12, "known partial values are retained");
    assert_eq!(row.cache_read_tokens, 13);
    assert_eq!(
        row.missing_usage_count, 3,
        "NULL or partial usage is not valid zero"
    );
    assert_eq!(
        row.cache_read_reported_count, 3,
        "reported zero is still non-NULL"
    );
    Ok(())
}

#[tokio::test]
async fn migration_backfills_kiro_cache_reads_at_ninety_percent_only_once() -> anyhow::Result<()> {
    let storage = storage().await?;
    let pool = storage.pool();
    let kiro = "kiro-chat/generate-assistant-response/v1";
    let at = millis("2024-06-01T00:00:00Z");
    // (id, upstream protocol, input, cache read)
    for (id, protocol, input, cached) in [
        ("kiro-zero", kiro, Some(1_000), Some(0)),
        ("kiro-null", kiro, Some(19), None),
        ("kiro-reported", kiro, Some(1_000), Some(123)),
        ("kiro-tiny", kiro, Some(1), Some(0)),
        ("kiro-no-usage", kiro, None, None),
        (
            "other",
            "openai-compatible/chat-completions/v1",
            Some(1_000),
            Some(0),
        ),
    ] {
        sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, upstream_protocol, upstream_model, input_tokens, output_tokens, cache_read_tokens, client_status_code) VALUES (?, ?, 'p', ?, 'm', ?, 1, ?, 200)")
            .bind(id).bind(at).bind(protocol).bind(input).bind(cached)
            .execute(pool).await?;
    }
    let cache = |id: &'static str| async move {
        sqlx::query_scalar::<_, Option<i32>>(
            "SELECT cache_read_tokens FROM request_logs WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
    };
    for _ in 0..2 {
        // Idempotent across restarts.
        db::migrate(pool).await?;
        assert_eq!(cache("kiro-zero").await?, Some(900));
        assert_eq!(
            cache("kiro-null").await?,
            Some(17),
            "90% of 19, rounded down"
        );
        assert_eq!(
            cache("kiro-reported").await?,
            Some(123),
            "existing counts are kept"
        );
        assert_eq!(cache("kiro-tiny").await?, Some(0), "share rounds to zero");
        assert_eq!(cache("kiro-no-usage").await?, None);
        assert_eq!(cache("other").await?, Some(0), "only Kiro rows are assumed");
    }
    Ok(())
}

#[tokio::test]
async fn token_usage_admin_year_validation_current_names_and_id_fallback() -> anyhow::Result<()> {
    let storage = storage().await?;
    let provider = storage
        .providers()
        .create(CreateProvider {
            name: "Current name".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test-only".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    let start = millis("2024-01-01T00:00:00Z");
    let end = millis("2025-01-01T00:00:00Z");
    for (at, id) in [
        (start - 1, provider.id.as_str()),
        (start, provider.id.as_str()),
        (end - 1, "deleted"),
        (end, "deleted"),
    ] {
        insert(
            storage.pool(),
            at,
            Some(id),
            Some("upstream"),
            (Some(1), Some(2), None),
            200,
        )
        .await?;
    }
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::from_storage(
        GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        },
        Arc::new(storage),
    )
    .await?;
    let before = chrono::Utc::now().timestamp_millis();
    let stats = gw.admin().get_token_usage_stats(2024).await?;
    assert_eq!(stats.year, 2024);
    assert_eq!(stats.time_zone, "UTC");
    assert!((before..=chrono::Utc::now().timestamp_millis()).contains(&stats.generated_at));
    assert_eq!(stats.rows.len(), 2);
    assert_eq!(stats.rows[0].provider_name, "Current name");
    assert_eq!(stats.rows[1].provider_name, "deleted");
    assert_eq!(stats.rows[0].date, "2024-01-01");
    assert_eq!(stats.rows[1].date, "2024-12-31");
    for year in [i32::MIN, 1969, 9999, i32::MAX] {
        assert_eq!(
            gw.admin()
                .get_token_usage_stats(year)
                .await
                .unwrap_err()
                .to_string(),
            "year must be between 1970 and 9998"
        );
    }
    assert_eq!(
        gw.admin().get_token_usage_stats(2023).await?.rows[0].date,
        "2023-12-31"
    );
    assert_eq!(
        gw.admin().get_token_usage_stats(2025).await?.rows[0].date,
        "2025-01-01"
    );
    for year in [1970, 9998] {
        assert!(
            gw.admin()
                .get_token_usage_stats(year)
                .await?
                .rows
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn token_usage_admin_enriches_exact_and_prefix_costs_without_changing_utc_totals()
-> anyhow::Result<()> {
    let storage = storage().await?;
    let provider = storage
        .providers()
        .create(CreateProvider {
            name: "Current proxy".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test-only".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    let cases = [
        (
            "2024-01-01T00:00:00Z",
            provider.id.as_str(),
            "gpt-price-fixture",
            (Some(1_000_000), Some(500_000), Some(250_000)),
        ),
        (
            "2024-02-29T00:00:00Z",
            provider.id.as_str(),
            "GPT-PRICE-FIXTURE-medium",
            (Some(500_000), None, None),
        ),
        (
            "2024-03-01T00:00:00Z",
            provider.id.as_str(),
            "gpt-cache-fallback-fixture:medium",
            (Some(1_000_000), Some(0), Some(500_000)),
        ),
        (
            "2024-04-01T00:00:00Z",
            provider.id.as_str(),
            "free-fixture/medium",
            (Some(100), Some(200), Some(0)),
        ),
        (
            "2024-05-01T00:00:00Z",
            provider.id.as_str(),
            "missing-fixture-medium",
            (Some(0), Some(0), Some(0)),
        ),
        (
            "2024-06-01T00:00:00Z",
            provider.id.as_str(),
            "invalid-fixture_medium",
            (Some(100), Some(200), Some(0)),
        ),
        (
            "2024-07-01T00:00:00Z",
            provider.id.as_str(),
            "ambiguous-fixture-medium",
            (Some(100), Some(200), Some(0)),
        ),
        (
            "2024-08-01T00:00:00Z",
            provider.id.as_str(),
            "unknown-fixture",
            (Some(100), Some(200), Some(0)),
        ),
        (
            "2024-12-31T23:59:59Z",
            "deleted",
            "gpt-price-fixture",
            (Some(100), Some(200), Some(0)),
        ),
        (
            "2025-01-01T00:00:00Z",
            provider.id.as_str(),
            "gpt-price-fixture",
            (Some(1), Some(2), Some(0)),
        ),
    ];
    for (date, provider, model, usage) in cases {
        insert(
            storage.pool(),
            millis(date),
            Some(provider),
            Some(model),
            usage,
            200,
        )
        .await?;
    }
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::from_storage(
        GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        },
        Arc::new(storage),
    )
    .await?;
    // The stats call reads this local catalogue, not provider HTTP capabilities.
    std::fs::write(dir.path().join("models.dev.json"), serde_json::json!({
        "openai": {"models": {
            "gpt-price-fixture": {"id": "gpt-price-fixture", "cost": {"input": 2, "output": 8, "cache_read": 0.5}},
            "gpt-cache-fallback-fixture": {"id": "gpt-cache-fallback-fixture", "cost": {"input": 2, "output": 8}}
        }},
        "one": {"models": {
            "free-fixture": {"id": "free-fixture", "cost": {"input": 0, "output": 0}},
            "missing-fixture": {"id": "missing-fixture", "cost": {"input": 0}},
            "invalid-fixture": {"id": "invalid-fixture", "cost": {"input": -1, "output": 8}},
            "ambiguous-fixture": {"id": "ambiguous-fixture", "cost": {"input": 1, "output": 8}}
        }},
        "two": {"models": {"ambiguous-fixture": {"id": "ambiguous-fixture", "cost": {"input": 2, "output": 8}}}}
    }).to_string())?;
    let stats = gw.admin().get_token_usage_stats(2024).await?;
    assert_eq!(stats.time_zone, "UTC");
    assert_eq!(stats.rows.len(), 9);
    let first = &stats.rows[0];
    assert_eq!(first.date, "2024-01-01");
    assert_eq!(first.provider_name, "Current proxy");
    assert_eq!(
        (
            first.input_tokens,
            first.cache_read_tokens,
            first.output_tokens
        ),
        (1_000_000, 250_000, 500_000)
    );
    let cost = first.cost.as_ref().unwrap();
    assert_eq!(
        (
            cost.input_cost,
            cost.cache_read_cost,
            cost.output_cost,
            cost.total_cost
        ),
        (1.5, 0.125, 4.0, 5.625)
    );
    assert_eq!(cost.currency, "USD");
    assert_eq!(cost.pricing_provider, "openai");
    assert!(!cost.cache_rate_fallback);
    assert_eq!(stats.rows[1].date, "2024-02-29");
    assert_eq!(stats.rows[1].missing_usage_count, 1);
    assert_eq!(stats.rows[1].cost.as_ref().unwrap().total_cost, 1.0);
    assert_eq!(
        stats.rows[1].cost.as_ref().unwrap().pricing_model,
        "gpt-price-fixture"
    );
    let fallback = stats.rows[2].cost.as_ref().unwrap();
    assert!(fallback.cache_rate_fallback);
    assert_eq!(fallback.total_cost, 2.0);
    assert_eq!(fallback.cache_read_rate, 2.0);
    assert_eq!(stats.rows[3].cost.as_ref().unwrap().total_cost, 0.0);
    assert!(stats.rows[4..].iter().all(|row| row.cost.is_none()));
    assert_eq!(stats.rows[8].provider_name, "deleted");
    assert_eq!(stats.rows[8].date, "2024-12-31");
    let json = serde_json::to_value(stats)?;
    assert!(json["rows"][4]["cost"].is_null());
    assert_eq!(json["rows"][3]["cost"]["total_cost"], 0.0);
    Ok(())
}

#[tokio::test]
async fn token_usage_memory_mode_has_no_retained_logs() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    assert!(storage.logs().token_usage_days(0, 1).await?.is_empty());
    assert!(
        storage
            .logs()
            .token_usage_rollup_days(0, 1)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn token_usage_extreme_supported_dates_and_unknown_model() -> anyhow::Result<()> {
    let storage = storage().await?;
    for (date, expected) in [
        ("1970-01-01T00:00:00Z", "1970-01-01"),
        ("9998-12-31T23:59:59Z", "9998-12-31"),
    ] {
        let at = millis(date);
        // A routed attempt with no recorded upstream model is still real usage.
        insert(storage.pool(), at, Some("p"), None, (None, None, None), 200).await?;
        let rows = storage.logs().token_usage_days(at, at + 1).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date, expected);
        assert_eq!(rows[0].provider_id, "p");
        assert_eq!(rows[0].model, "");
        assert_eq!(rows[0].missing_usage_count, 1);
    }
    Ok(())
}

#[tokio::test]
async fn token_usage_excludes_pre_routing_rejections_without_provider() -> anyhow::Result<()> {
    let storage = storage().await?;
    let at = millis("2024-06-01T00:00:00Z");
    // `no route for model` rejections log NULL (legacy) or '' provider/model.
    insert(
        storage.pool(),
        at,
        None,
        None,
        (Some(0), Some(0), None),
        404,
    )
    .await?;
    insert(
        storage.pool(),
        at,
        Some(""),
        Some(""),
        (Some(0), Some(0), None),
        404,
    )
    .await?;
    insert(
        storage.pool(),
        at,
        Some("p"),
        Some("m"),
        (Some(5), Some(1), None),
        200,
    )
    .await?;
    let rows = storage.logs().token_usage_days(at, at + 1).await?;
    let identities: Vec<_> = rows
        .iter()
        .map(|r| (r.provider_id.as_str(), r.model.as_str(), r.request_count))
        .collect();
    assert_eq!(identities, vec![("p", "m", 1)]);
    Ok(())
}

async fn current_provider(storage: &SqliteStorage, name: &str) -> anyhow::Result<Provider> {
    storage
        .providers()
        .create(CreateProvider {
            name: name.into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test-only".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await
}

#[tokio::test]
async fn token_usage_totals_survive_every_log_deletion_path() -> anyhow::Result<()> {
    let storage = storage().await?;
    let pool = storage.pool().clone();
    let provider = current_provider(&storage, "Current proxy").await?;
    let p = provider.id.as_str();
    let day1 = millis("2024-03-01T08:00:00Z");
    let day2 = millis("2024-03-02T23:59:59Z");
    for (id, at, provider, model, usage, status) in [
        (
            "ok",
            day1,
            Some(p),
            Some("gpt-price-fixture"),
            (Some(1_000_000), Some(500_000), Some(250_000)),
            200,
        ),
        (
            "failed",
            day1,
            Some(p),
            Some("gpt-price-fixture"),
            (Some(4), Some(2), Some(1)),
            429,
        ),
        (
            "no-usage",
            day1,
            Some(p),
            Some("gpt-price-fixture"),
            (None, None, None),
            200,
        ),
        (
            "lower",
            day2,
            Some(p),
            Some("model"),
            (Some(10), Some(1), None),
            200,
        ),
        (
            "upper",
            day2,
            Some(p),
            Some("Model"),
            (Some(20), Some(2), Some(0)),
            200,
        ),
        (
            "no-model",
            day2,
            Some(p),
            None,
            (Some(3), Some(3), None),
            200,
        ),
        (
            "deleted-provider",
            day2,
            Some("gone"),
            Some("model"),
            (Some(5), Some(5), Some(5)),
            200,
        ),
        (
            "pre-routing",
            day2,
            None,
            None,
            (Some(0), Some(0), None),
            404,
        ),
        (
            "pre-routing-empty",
            day2,
            Some(""),
            Some(""),
            (Some(0), Some(0), None),
            404,
        ),
    ] {
        insert_with_id(&pool, id, at, provider, model, usage, status).await?;
    }
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("models.dev.json"),
        serde_json::json!({"openai": {"models": {
            "gpt-price-fixture": {"id": "gpt-price-fixture", "cost": {"input": 2, "output": 8, "cache_read": 0.5}}
        }}})
        .to_string(),
    )?;
    let (gw, _) = Gateway::from_storage(
        GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        },
        Arc::new(storage),
    )
    .await?;
    let stats = || async { gw.admin().get_token_usage_stats(2024).await };
    let baseline = stats().await?;
    assert_eq!(
        baseline.rows.len(),
        5,
        "provider-less attempts are not usage"
    );
    let expected = usage_keys(&baseline.rows);
    let priced = |rows: &[TokenUsageDay]| {
        rows.iter()
            .map(|r| r.cost.as_ref().map(|c| c.total_cost))
            .collect::<Vec<_>>()
    };
    let expected_costs = priced(&baseline.rows);
    // Day-1 attempts merge: 1_000_004 gross input incl. 250_001 cache reads, 500_002 output
    // at 2 / 0.5 / 8 USD per million tokens.
    let day1_cost = expected_costs[0].unwrap();
    assert!((day1_cost - (1.500_006 + 0.125_000_5 + 4.000_016)).abs() < 1e-9);
    assert!(
        expected_costs[1..].iter().all(Option::is_none),
        "unknown models stay unpriced"
    );

    // Single delete, error clear and full clear each fold before deleting.
    assert_eq!(gw.admin().delete_log("ok").await?, 1);
    assert_eq!(usage_keys(&stats().await?.rows), expected);
    assert_eq!(
        gw.admin().clear_error_logs().await?,
        3,
        "429 plus both 404s"
    );
    assert_eq!(usage_keys(&stats().await?.rows), expected);
    // Payload clearing keeps the rows and their token columns.
    gw.admin().clear_log_payloads().await?;
    assert_eq!(usage_keys(&stats().await?.rows), expected);
    assert_eq!(gw.admin().clear_logs().await?, 5);
    assert_eq!(log_count(&pool).await?, 0);
    let after = stats().await?;
    assert_eq!(usage_keys(&after.rows), expected);
    assert_eq!(
        priced(&after.rows),
        expected_costs,
        "costs are recomputed from kept tokens"
    );
    assert_eq!(after.rows[0].provider_name, "Current proxy");
    assert_eq!(
        after
            .rows
            .iter()
            .find(|r| r.provider_id == "gone")
            .unwrap()
            .provider_name,
        "gone",
        "deleted providers keep falling back to their ID"
    );

    // Clearing again cannot double count; new logs add to the kept totals.
    assert_eq!(gw.admin().clear_logs().await?, 0);
    assert_eq!(usage_keys(&stats().await?.rows), expected);
    insert(
        &pool,
        day1,
        Some(p),
        Some("gpt-price-fixture"),
        (Some(1), Some(1), Some(1)),
        200,
    )
    .await?;
    let grown = stats().await?;
    assert_eq!(
        (grown.rows[0].request_count, grown.rows[0].input_tokens),
        (expected[0].3 + 1, expected[0].4 + 1),
        "retained and folded rows of one day merge into one row"
    );
    assert_eq!(grown.rows.len(), expected.len());
    gw.admin().clear_logs().await?;
    assert_eq!(usage_keys(&stats().await?.rows), usage_keys(&grown.rows));
    Ok(())
}

#[tokio::test]
async fn token_usage_rollup_keeps_identity_null_semantics_and_utc_days() -> anyhow::Result<()> {
    let storage = storage().await?;
    let start = millis("2024-01-01T00:00:00Z");
    let end = millis("2025-01-01T00:00:00Z");
    let leap_day = millis("2024-02-29T00:00:00Z");
    for (at, provider, model, usage) in [
        (
            start - 1,
            Some("p1"),
            Some("shared"),
            (Some(10), Some(2), Some(3)),
        ),
        (
            start,
            Some("p1"),
            Some("shared"),
            (Some(10), Some(2), Some(3)),
        ),
        (
            leap_day - 1,
            Some("p1"),
            Some("shared"),
            (Some(10), Some(2), Some(3)),
        ),
        (leap_day, Some("p2"), Some("shared"), (None, Some(2), None)),
        (
            leap_day,
            Some("p2"),
            Some("Shared"),
            (Some(9), None, Some(0)),
        ),
        (leap_day, Some("p2"), None, (Some(1), Some(1), None)),
        (
            end - 1,
            Some("p1"),
            Some("shared"),
            (Some(10), Some(2), Some(3)),
        ),
        (
            end,
            Some("p1"),
            Some("shared"),
            (Some(10), Some(2), Some(3)),
        ),
        (leap_day, None, None, (Some(7), Some(7), None)),
    ] {
        insert(storage.pool(), at, provider, model, usage, 200).await?;
    }
    let all = (
        millis("2023-01-01T00:00:00Z"),
        millis("2026-01-01T00:00:00Z"),
    );
    let retained = usage_keys(&storage.logs().token_usage_days(all.0, all.1).await?);
    assert_eq!(storage.logs().clear_all().await?, 9);
    assert!(
        storage
            .logs()
            .token_usage_days(all.0, all.1)
            .await?
            .is_empty()
    );
    assert_eq!(
        usage_keys(&storage.logs().token_usage_rollup_days(all.0, all.1).await?),
        retained,
        "folded rows match the retained-log view row for row"
    );
    let year = storage.logs().token_usage_rollup_days(start, end).await?;
    let identities: Vec<_> = year
        .iter()
        .map(|r| (r.date.as_str(), r.provider_id.as_str(), r.model.as_str()))
        .collect();
    assert_eq!(
        identities,
        vec![
            ("2024-01-01", "p1", "shared"),
            ("2024-02-28", "p1", "shared"),
            ("2024-02-29", "p2", ""),
            ("2024-02-29", "p2", "Shared"),
            ("2024-02-29", "p2", "shared"),
            ("2024-12-31", "p1", "shared"),
        ]
    );
    let shared = &year[4];
    assert_eq!(
        (shared.missing_usage_count, shared.cache_read_reported_count),
        (1, 0)
    );
    let upper = &year[3];
    assert_eq!(
        (upper.missing_usage_count, upper.cache_read_reported_count),
        (1, 1)
    );
    assert!(
        year.iter()
            .all(|r| r.cost.is_none() && r.provider_name == r.provider_id)
    );
    Ok(())
}

#[tokio::test]
async fn retention_cleanup_folds_only_the_rows_it_deletes() -> anyhow::Result<()> {
    let storage = storage().await?;
    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 30 * 86_400_000;
    insert(
        storage.pool(),
        old,
        Some("p"),
        Some("m"),
        (Some(5), Some(6), Some(1)),
        200,
    )
    .await?;
    insert(
        storage.pool(),
        now,
        Some("p"),
        Some("m"),
        (Some(1), Some(2), None),
        200,
    )
    .await?;
    let all = (0, now + 86_400_000);
    // An unparseable modifier keeps the previous "delete nothing" behavior.
    assert_eq!(storage.logs().cleanup_before("not a modifier").await?, 0);
    assert!(
        storage
            .logs()
            .token_usage_rollup_days(all.0, all.1)
            .await?
            .is_empty()
    );
    assert_eq!(storage.logs().cleanup_before("-7 days").await?, 1);
    let folded = storage.logs().token_usage_rollup_days(all.0, all.1).await?;
    assert_eq!(folded.len(), 1);
    assert_eq!(
        (
            folded[0].request_count,
            folded[0].input_tokens,
            folded[0].output_tokens
        ),
        (1, 5, 6)
    );
    let kept = storage.logs().token_usage_days(all.0, all.1).await?;
    assert_eq!((kept.len(), kept[0].input_tokens), (1, 1));
    assert_eq!(storage.logs().cleanup_before("-7 days").await?, 0);
    assert_eq!(
        storage.logs().token_usage_rollup_days(all.0, all.1).await?[0].request_count,
        1,
        "a repeated cleanup folds nothing twice"
    );
    Ok(())
}

#[tokio::test]
async fn missing_permanent_usage_table_is_schema_incompatible_until_migrated() -> anyhow::Result<()>
{
    let storage = storage().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    sqlx::query("DROP TABLE token_usage_daily")
        .execute(storage.pool())
        .await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    Ok(())
}
