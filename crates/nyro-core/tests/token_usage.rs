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
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, provider_name, client_model, upstream_model, input_tokens, output_tokens, cache_read_tokens, upstream_status_code, client_status_code, outcome_version, attempt_outcome) VALUES (?, ?, ?, 'Old provider name', 'client-alias', ?, ?, ?, ?, ?, 200, 1, ?)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(at).bind(provider).bind(model)
        .bind(usage.0).bind(usage.1).bind(usage.2).bind(status)
        .bind(if status >= 400 { "failed" } else { "completed" })
        .execute(pool).await?;
    Ok(())
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
