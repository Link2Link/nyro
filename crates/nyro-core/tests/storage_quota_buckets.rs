//! Retained quota attribution contract. External dialect checks are explicit
//! opt-in against disposable test databases; never run against an application DB.
use std::collections::HashMap;

use nyro_core::db::models::{LogQuery, OpenAiSpeedMode};
use nyro_core::storage::provider_usage::ProviderQuotaPeriod;
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use serde_json::json;

struct Row {
    id: String,
    at: i64,
    decision: Option<String>,
    input: Option<i32>,
    output: Option<i32>,
}

fn tier_metadata(at: i64) -> Option<String> {
    let (value, source) = match at {
        110 => ("fast", "client_request"), // not upstream evidence
        111 => ("fast", "upstream_request"),
        200 => ("ultrafast", "upstream_response"),
        201 => ("normal", "upstream_request"),
        202 => ("unrecognized", "upstream_response"),
        203 => ("fast", "upstream_response"),
        206 => return Some("{malformed".to_owned()),
        _ => return None,
    };
    Some(json!({"codex_service_tier": { "value": value, "source": source }}).to_string())
}

fn rows() -> Vec<Row> {
    let mut rows = Vec::new();
    for (scope, bucket, dispatch, at, missing) in [
        ("owner-a", "flash", 99, 110, false),
        ("owner-a", "flash", 100, 111, false),
        ("owner-a", "flash", 199, 200, false),
        ("owner-a", "flash", 200, 201, false),
        ("owner-b", "flash", 150, 202, false),
        ("owner-a", "argon", 150, 203, true),
        ("retired-owner", "retired-bucket", 150, 205, false),
        ("owner-a", "flash", 200, 301, false),
    ] {
        rows.push(Row {
            id: uuid::Uuid::new_v4().to_string(),
            at,
            decision: Some(
                json!({"quota_attribution": {
                    "version": 1, "scope_id": scope, "bucket_id": bucket,
                    "backend_model": "backend-alias", "wire_model": format!("wire-{bucket}"),
                    "source": "configured", "revision": 7, "dispatched_at": dispatch
                }})
                .to_string(),
            ),
            // The ordinary API-price group clamps cache only after aggregating:
            // the first tier's cache (40) exceeds its input, but not the group's.
            input: (!missing).then_some(match at {
                111 => 10,
                200 => 190,
                _ => 100,
            }),
            output: (!missing && at != 200).then_some(10),
        });
    }
    for decision in [
        None,
        Some("{malformed".to_owned()),
        Some(json!({"quota_attribution": {"version": 2, "bucket_id": "flash"}}).to_string()),
    ] {
        rows.push(Row {
            id: uuid::Uuid::new_v4().to_string(),
            at: 206,
            decision,
            input: Some(0),
            output: Some(0),
        });
    }
    rows
}

async fn exercise(storage: &dyn Storage, provider: &str, rows: &[Row]) -> anyhow::Result<()> {
    let periods = HashMap::from([
        (
            ("owner-a".to_owned(), "flash".to_owned()),
            ProviderQuotaPeriod {
                start: 100,
                end: 200,
                cutoff: 200,
            },
        ),
        (
            ("owner-b".to_owned(), "flash".to_owned()),
            ProviderQuotaPeriod {
                start: 100,
                end: 200,
                cutoff: 300,
            },
        ),
        (
            ("owner-a".to_owned(), "argon".to_owned()),
            ProviderQuotaPeriod {
                start: 100,
                end: 200,
                cutoff: 300,
            },
        ),
    ]);
    let scan = storage
        .logs()
        .provider_quota_token_usage_with_periods(provider, 300, None, &periods, true)
        .await?;
    assert_eq!(
        scan.all_time
            .iter()
            .map(|row| row.usage.request_count)
            .sum::<i64>(),
        10
    );
    assert_eq!(
        scan.period
            .iter()
            .map(|row| row.usage.request_count)
            .sum::<i64>(),
        4
    );
    assert_eq!(
        scan.all_time
            .iter()
            .map(|row| row.unassigned_request_count)
            .sum::<i64>(),
        3
    );
    assert_eq!(
        scan.all_time
            .iter()
            .map(|row| row.inferred_request_count)
            .sum::<i64>(),
        0
    );
    let raw_scan = storage
        .logs()
        .provider_quota_token_usage_with_periods(provider, 300, None, &periods, false)
        .await?;
    assert!(
        raw_scan
            .all_time
            .iter()
            .chain(&raw_scan.period)
            .all(|row| row.speed_tiers.is_empty())
    );
    assert_eq!(raw_scan.all_time.len(), scan.all_time.len());
    for (raw, with_tiers) in raw_scan.all_time.iter().zip(&scan.all_time) {
        assert_eq!(raw.scope_id, with_tiers.scope_id);
        assert_eq!(raw.bucket_id, with_tiers.bucket_id);
        assert_eq!(
            serde_json::to_value(&raw.usage)?,
            serde_json::to_value(&with_tiers.usage)?
        );
    }
    let flash = scan
        .period
        .iter()
        .find(|row| {
            row.scope_id.as_deref() == Some("owner-a") && row.bucket_id.as_deref() == Some("flash")
        })
        .unwrap();
    assert_eq!(
        (
            flash.usage.model.as_str(),
            flash.usage.request_count,
            flash.usage.input_tokens,
            flash.usage.cache_read_tokens
        ),
        ("wire-flash", 2, 200, 80)
    );
    let tier_usage = |row: &nyro_core::storage::provider_usage::ProviderQuotaModelUsage| {
        row.speed_tiers
            .iter()
            .map(|tier| {
                (
                    tier.speed_tier,
                    tier.usage.request_count,
                    tier.usage.input_tokens,
                    tier.usage.output_tokens,
                    tier.usage.cache_read_tokens,
                    tier.usage.missing_usage_count,
                )
            })
            .collect::<Vec<_>>()
    };
    let expected_tiers = vec![
        (Some(OpenAiSpeedMode::Fast), 1, 10, 10, 40, 0),
        (Some(OpenAiSpeedMode::Ultrafast), 1, 190, 0, 40, 1),
    ];
    assert_eq!(tier_usage(flash), expected_tiers);
    assert_eq!(flash.usage.output_tokens, 10);
    assert_eq!(flash.usage.missing_usage_count, 1);
    assert_eq!(flash.fully_missing_usage_request_count, 0);
    let all_flash = scan
        .all_time
        .iter()
        .find(|row| row.scope_id == flash.scope_id && row.bucket_id == flash.bucket_id)
        .unwrap();
    assert_eq!(
        tier_usage(all_flash),
        vec![
            (None, 1, 100, 10, 40, 0),
            (Some(OpenAiSpeedMode::Normal), 1, 100, 10, 40, 0),
            expected_tiers[0],
            expected_tiers[1],
        ]
    );
    for row in &scan.all_time {
        if row.bucket_id.is_none()
            || matches!(row.scope_id.as_deref(), Some("owner-b" | "retired-owner"))
        {
            // Malformed JSON, unrecognized tier, and absent metadata all fall back to unknown.
            assert_eq!(row.speed_tiers.len(), 1);
            assert_eq!(row.speed_tiers[0].speed_tier, None);
            assert_eq!(
                row.speed_tiers[0].usage.request_count,
                row.usage.request_count
            );
        }
    }
    let argon = scan
        .all_time
        .iter()
        .find(|row| row.bucket_id.as_deref() == Some("argon"))
        .unwrap();
    assert_eq!(
        (
            argon.usage.missing_usage_count,
            argon.fully_missing_usage_request_count
        ),
        (1, 1)
    );
    assert_eq!(
        tier_usage(argon),
        vec![(Some(OpenAiSpeedMode::Fast), 1, 0, 0, 40, 1)]
    );
    assert!(
        scan.all_time
            .iter()
            .any(|row| row.scope_id.as_deref() == Some("retired-owner"))
    );
    for (bucket, scope, expected) in [
        ("flash", Some("owner-a"), 4),
        ("flash", None, 5),
        ("unassigned", None, 3),
        ("flash' OR 1=1 --", None, 0),
    ] {
        let page = storage
            .logs()
            .query(LogQuery {
                provider: Some(provider.to_owned()),
                before: Some(300),
                quota_bucket: Some(bucket.to_owned()),
                quota_scope: scope.map(str::to_owned),
                limit: Some(1),
                ..Default::default()
            })
            .await?;
        assert_eq!(page.total, expected, "bucket={bucket}, scope={scope:?}");
        assert_eq!(page.items.len(), usize::from(expected > 0));
        let streamed = storage
            .logs()
            .provider_quota_usage_detail(provider, 0, 300, bucket, scope)
            .await?;
        assert_eq!(streamed.detail.request_count, expected);
        assert_eq!(
            streamed
                .detail
                .models
                .iter()
                .map(|model| model.request_count)
                .sum::<i64>(),
            expected
        );
        assert_eq!(
            streamed
                .usage
                .iter()
                .map(|row| row.usage.request_count)
                .sum::<i64>(),
            expected
        );
    }
    // Detail ranges use emitted time (inclusive), not the dispatch cycle. All
    // dialects use bound exact provider bytes and the same opaque scope predicate.
    let ranged = storage
        .logs()
        .provider_quota_usage_detail(provider, 111, 200, "flash", Some("owner-a"))
        .await?;
    assert_eq!(ranged.detail.request_count, 2);
    assert!(ranged.usage.iter().all(|row| row.speed_tiers.is_empty()));
    assert_eq!(ranged.detail.total_input_tokens, 200);
    assert_eq!(ranged.detail.total_cache_read_tokens, 80);
    assert_eq!(ranged.detail.models[0].upstream_model, "wire-flash");
    let missing = storage
        .logs()
        .provider_quota_usage_detail(provider, 0, 300, "argon", Some("owner-a"))
        .await?;
    assert_eq!(missing.usage[0].fully_missing_usage_request_count, 1);
    let mismatch = storage
        .logs()
        .provider_quota_usage_detail(&format!("{provider} "), 0, 300, "flash", None)
        .await?;
    assert_eq!(mismatch.detail.request_count, 0);
    // Deletion folds the existing permanent ledger, but never reconstructs a
    // quota cycle from that ledger. Payload erasure retains attribution metadata.
    storage.logs().delete_by_id(&rows[1].id).await?;
    let scan = storage
        .logs()
        .provider_quota_token_usage_with_periods(provider, 300, None, &periods, true)
        .await?;
    assert_eq!(
        scan.all_time
            .iter()
            .map(|row| row.usage.request_count)
            .sum::<i64>(),
        9
    );
    assert_eq!(
        scan.period
            .iter()
            .map(|row| row.usage.request_count)
            .sum::<i64>(),
        3
    );
    for row in rows {
        storage.logs().delete_by_id(&row.id).await?;
    }
    let cleared = storage
        .logs()
        .provider_quota_token_usage_with_periods(provider, 300, None, &periods, true)
        .await?;
    assert!(cleared.all_time.is_empty() && cleared.period.is_empty());
    Ok(())
}

#[tokio::test]
async fn sqlite_retained_quota_contract() -> anyhow::Result<()> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    let provider = uuid::Uuid::new_v4().to_string();
    let rows = rows();
    for row in &rows {
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, route_decision, input_tokens, output_tokens, cache_read_tokens, payload_metadata_json) VALUES (?, ?, 'backend-alias', ?, ?, ?, ?, 40, ?)")
            .bind(&row.id).bind(&provider).bind(row.at).bind(&row.decision).bind(row.input).bind(row.output).bind(tier_metadata(row.at)).execute(storage.pool()).await?;
    }
    exercise(&storage, &provider, &rows).await
}

fn external(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_QUOTA_DATABASES_ONLY").as_deref() == Ok("1"),
        "explicit disposable quota database consent required"
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
async fn postgres_retained_quota_contract_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_POSTGRES_QUOTA_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    let provider = uuid::Uuid::new_v4().to_string();
    let rows = rows();
    for row in &rows {
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, route_decision, input_tokens, output_tokens, cache_read_tokens, payload_metadata_json) VALUES ($1, $2, 'backend-alias', $3, $4, $5, $6, 40, $7)")
            .bind(&row.id).bind(&provider).bind(row.at).bind(&row.decision).bind(row.input).bind(row.output).bind(tier_metadata(row.at)).execute(storage.pool()).await?;
    }
    exercise(&storage, &provider, &rows).await
}

#[tokio::test]
async fn mysql_retained_quota_contract_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_MYSQL_QUOTA_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    let provider = uuid::Uuid::new_v4().to_string();
    let rows = rows();
    for row in &rows {
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, route_decision, input_tokens, output_tokens, cache_read_tokens, payload_metadata_json) VALUES (?, ?, 'backend-alias', ?, ?, ?, ?, 40, ?)")
            .bind(&row.id).bind(&provider).bind(row.at).bind(&row.decision).bind(row.input).bind(row.output).bind(tier_metadata(row.at)).execute(storage.pool()).await?;
    }
    exercise(&storage, &provider, &rows).await
}
