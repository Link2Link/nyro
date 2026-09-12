//! Real backend parity. External URLs are opt-in, explicitly disposable and
//! restricted to test database names; no application configuration is loaded.
use nyro_core::db::models::{CreateProvider, LogQuery};
use nyro_core::logging::LogEntry;
use nyro_core::performance::PerformanceMetadata;
use nyro_core::protocol::ir::Usage;
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const AS_OF: i64 = 1_800_000_000_000;
const WEEK: i64 = 604_800_000;

fn entry(provider: &str, model: &str, tier: Option<&str>, at: i64) -> LogEntry {
    LogEntry {
        diagnostic: Default::default(),
        api_key_id: None,
        api_key_name: None,
        created_at: at,
        client_protocol: "openai/chat/completions".into(),
        upstream_protocol: "openai/chat/completions".into(),
        provider_id: provider.into(),
        provider_name: "test".into(),
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: model.into(),
        upstream_model: model.into(),
        reasoning_effort: Some("legacy-unchanged".into()),
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
        upstream_status_code: Some(200),
        client_status_code: 200,
        latency_total_ms: 99_000,
        latency_upstream_ms: Some(10_000),
        usage: Usage {
            completion_tokens: 100,
            ..Default::default()
        },
        is_stream: true,
        stream_chunks_count: 2,
        stream_first_chunk_ms: Some(9_000),
        enable_payload: None,
        performance: PerformanceMetadata {
            version: 1,
            effort_status: if tier.is_some() { "present" } else { "absent" }.into(),
            effort_raw: tier.map(str::to_owned),
            effort_tier: tier.map(str::to_owned),
            completion: "completed".into(),
            completion_reason: Some("stop".into()),
            response_mode: "buffered".into(),
            upstream_duration_ms: Some(1000),
            first_chunk_ms: None,
            completed_at: Some(at),
        },
    }
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

async fn exercise(storage: &dyn Storage) -> anyhow::Result<()> {
    let provider = storage.providers().create(serde_json::from_value::<CreateProvider>(serde_json::json!({
        "name": format!("performance-{}", uuid::Uuid::new_v4()),
        "protocol": "openai/chat/completions", "base_url": "https://example.invalid", "api_key": "test"
    }))?).await?;
    let p = &provider.id;
    let mut rows = Vec::new();
    // All efforts share one latest-ten sample window for the exact model.
    for (index, tier) in ["low", "medium", "high", "xhigh", "max"].iter().enumerate() {
        for n in 0..12 {
            let mut e = entry(
                p,
                "partition",
                Some(tier),
                AS_OF - 1000 + index as i64 * 100 + n,
            );
            if *tier == "low" {
                e.performance.effort_raw = Some("minimal".into());
            }
            if n >= 3 {
                e.usage.completion_tokens = 0;
            }
            rows.push(e);
        }
    }
    // Invalid samples consume latest10, and have null mean/ranges when all invalid.
    for n in 0..11 {
        let mut e = entry(p, "invalid", Some("low"), AS_OF - 100 + n);
        if n > 0 {
            e.usage.completion_tokens = 0;
        }
        rows.push(e);
    }
    // All retained logs count, even outside the old seven-day/as_of window.
    let mut boundary = entry(p, "boundary", Some("high"), AS_OF - WEEK);
    boundary.created_at = AS_OF - WEEK - 50_000;
    rows.push(boundary);
    rows.push(entry(p, "boundary", Some("high"), AS_OF));
    rows.push(entry(p, "boundary", Some("high"), AS_OF - WEEK - 1));
    rows.push(entry(p, "boundary", Some("high"), AS_OF + 1));
    // Known-complete effort outside canonical five belongs to mixed only.
    let mut other = entry(p, "mixed", None, AS_OF - 20);
    other.performance.effort_status = "present".into();
    other.performance.effort_raw = Some("budget:8192".into());
    rows.push(other);
    rows.push(entry(p, "mixed", Some("low"), AS_OF - 19));
    rows.push(entry(p, "mixed", None, AS_OF - 18));
    let mut unknown_effort = entry(p, "mixed", None, AS_OF - 17);
    unknown_effort.performance.effort_status = "unknown".into();
    rows.push(unknown_effort);
    for completion in ["unknown", "failed", "incomplete", "cancelled", "timed_out"] {
        let mut e = entry(p, "mixed", Some("high"), AS_OF - 1);
        e.performance.completion = completion.into();
        rows.push(e);
    }
    let mut http_error = entry(p, "mixed", Some("high"), AS_OF - 1);
    http_error.upstream_status_code = Some(500);
    rows.push(http_error);
    let mut client_error = entry(p, "mixed", Some("high"), AS_OF - 1);
    client_error.client_status_code = 500;
    rows.push(client_error);
    // Legacy stream detection and timing win, regardless of diagnostic metadata.
    for (index, first, upstream, expected_tokens) in
        [(0, 200, 1000, 80), (1, 800, 1000, 100), (2, 30, 70, 7)]
    {
        let mut e = entry(p, "timing", Some("high"), AS_OF - index);
        e.is_stream = false;
        e.performance.response_mode = "stream".into();
        e.performance.first_chunk_ms = Some(0);
        e.performance.upstream_duration_ms = Some(1);
        e.stream_first_chunk_ms = Some(first);
        e.latency_upstream_ms = Some(upstream);
        e.usage.completion_tokens = expected_tokens;
        rows.push(e);
    }
    let mut missing = entry(p, "timing", Some("high"), AS_OF - 4);
    missing.performance.upstream_duration_ms = None;
    rows.push(missing);
    let mut missing_first = entry(p, "timing", Some("high"), AS_OF - 5);
    missing_first.performance.response_mode = "stream".into();
    rows.push(missing_first);
    rows.push(entry(p, "Case", Some("low"), AS_OF));
    rows.push(entry(p, "case", Some("high"), AS_OF));
    rows.push(entry(p, "Case ", Some("max"), AS_OF));
    let mut historical = entry(
        p,
        "historical",
        None,
        chrono::Utc::now().timestamp_millis() - 1000,
    );
    historical.performance = PerformanceMetadata::default();
    historical.upstream_request_body = Some("{\"reasoning_effort\":\"minimal\"}".into());
    rows.push(historical);
    // Reproducer: MiniMax's unknown completion has perfectly usable legacy TPS,
    // including with no saved payload and no diagnostic performance timings.
    let mut minimax = entry(p, "MiniMax-M2.7", None, AS_OF - 10);
    minimax.performance = PerformanceMetadata {
        version: 1,
        ..Default::default()
    };
    minimax.usage.completion_tokens = 2007;
    minimax.latency_upstream_ms = Some(20_617);
    minimax.stream_first_chunk_ms = Some(1798);
    rows.push(minimax);
    let mut old = entry(p, "old-only", None, AS_OF - WEEK - 1234);
    old.performance = PerformanceMetadata::default();
    rows.push(old);
    let other_provider = external_legacy_provider(storage).await?;
    let mut same_model = entry(&other_provider, "MiniMax-M2.7", None, AS_OF);
    same_model.usage.completion_tokens = 500;
    rows.push(same_model);
    storage.logs().append_batch(rows).await?;
    let models = [
        "partition",
        "invalid",
        "boundary",
        "mixed",
        "timing",
        "Case",
        "case",
        "Case ",
        "no-history",
        "MiniMax-M2.7",
        "old-only",
    ];
    let mut pairs: Vec<_> = models.iter().map(|m| (p.clone(), m.to_string())).collect();
    pairs.push((other_provider.clone(), "MiniMax-M2.7".into()));
    let stats = storage
        .logs()
        .model_performance_stats(&pairs, AS_OF)
        .await?;
    assert_eq!(stats.len(), pairs.len());
    for (pair, result) in pairs.iter().zip(&stats) {
        let usage = storage.logs().model_usage_stats(&pair.0, &pair.1).await?;
        assert_eq!(result.mixed.average_tps, usage.average_tps, "{}", pair.1);
        assert_eq!(
            result.mixed.selected_request_count, usage.recent_sample_count,
            "{}",
            pair.1
        );
        assert_eq!(result.unclassified_count, 0);
        assert_eq!(result.untrusted_count, 0);
    }
    assert_eq!(stats[0].mixed.average_tps, Some(10.0));
    let serialized = serde_json::to_value(&stats[0])?;
    assert!(serialized.get("tiers").is_none());
    assert_eq!(stats[0].mixed.selected_request_count, 10);
    assert_eq!(stats[0].mixed.valid_tps_count, 1);
    assert_eq!(stats[1].mixed.selected_request_count, 10);
    assert_eq!(stats[1].mixed.valid_tps_count, 0);
    assert_eq!(stats[1].mixed.average_tps, None);
    assert_eq!(stats[1].mixed.first_sample_at, None);
    assert_eq!(stats[1].mixed.last_sample_at, None);
    assert_eq!(stats[2].mixed.selected_request_count, 4);
    assert_eq!(stats[2].mixed.valid_tps_count, 4);
    assert_eq!(stats[2].mixed.first_sample_at, Some(AS_OF - WEEK - 50_000));
    assert_eq!(stats[2].mixed.last_sample_at, Some(AS_OF + 1));
    assert_eq!(stats[3].mixed.selected_request_count, 10);
    assert_eq!(stats[3].mixed.valid_tps_count, 10);
    assert_eq!(stats[4].mixed.selected_request_count, 5);
    assert_eq!(stats[4].mixed.valid_tps_count, 5);
    assert!((stats[4].mixed.average_tps.unwrap() - 64.0).abs() < 1e-9);
    assert_eq!(stats[5].mixed.selected_request_count, 1);
    assert_eq!(stats[6].mixed.selected_request_count, 1);
    assert_eq!(stats[7].mixed.selected_request_count, 1);
    assert_eq!(stats[8].mixed.selected_request_count, 0);
    assert_eq!(stats[8].mixed.average_tps, None);
    assert_eq!(
        stats[9].mixed.average_tps,
        Some(2007.0 / (18_819.0 / 1000.0))
    );
    assert_eq!(
        stats[9].mixed.overall_tps,
        Some(2007.0 / (20_617.0 / 1000.0))
    );
    assert_eq!(stats[9].mixed.valid_tps_count, 1);
    assert_eq!(stats[10].mixed.selected_request_count, 1);
    assert_eq!(stats[10].mixed.valid_tps_count, 1);
    assert_eq!(stats[10].mixed.first_sample_at, Some(AS_OF - WEEK - 1234));
    assert_eq!(stats[11].mixed.average_tps, Some(50.0));
    assert_eq!(stats[11].mixed.selected_request_count, 1);
    let page = storage
        .logs()
        .query(LogQuery {
            provider: Some(p.clone()),
            upstream_model: Some("partition".into()),
            ..Default::default()
        })
        .await?;
    let low = page
        .items
        .iter()
        .find(|e| e.upstream_effort_tier.as_deref() == Some("low"));
    // Default page may contain only the latest50; find_by_id must preserve every metadata field.
    let log = low.unwrap_or(&page.items[0]);
    let detail = storage.logs().find_by_id(&log.id).await?.unwrap();
    assert_eq!(detail.performance_metadata_version, 1);
    assert_eq!(detail.upstream_response_mode, "buffered");
    assert_eq!(detail.request_completion, "completed");
    assert_eq!(detail.completion_reason.as_deref(), Some("stop"));
    assert_eq!(detail.performance_upstream_ms, Some(1000));
    assert_eq!(detail.reasoning_effort.as_deref(), Some("legacy-unchanged"));
    let legacy_before = serde_json::to_value(storage.logs().model_usage_stats(p, "timing").await?)?;
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    let history = storage
        .logs()
        .query(LogQuery {
            provider: Some(p.clone()),
            upstream_model: Some("historical".into()),
            ..Default::default()
        })
        .await?;
    assert_eq!(history.items[0].performance_metadata_version, 1);
    assert_eq!(
        history.items[0].upstream_effort_tier.as_deref(),
        Some("low")
    );
    assert_eq!(history.items[0].request_completion, "unknown");
    assert_eq!(history.items[0].performance_completed_at, None);
    assert_eq!(
        legacy_before,
        serde_json::to_value(storage.logs().model_usage_stats(p, "timing").await?)?
    );
    storage.providers().delete(p).await?;
    storage.providers().delete(&other_provider).await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_performance_contract() -> anyhow::Result<()> {
    exercise(&sqlite().await?).await
}

#[tokio::test]
async fn sqlite_equal_created_at_uses_id_before_validation() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    for n in 0..11 {
        sqlx::query("INSERT INTO request_logs(id,created_at,provider_id,upstream_model,performance_metadata_version,request_completion,upstream_status_code,client_status_code,performance_completed_at,upstream_response_mode,performance_upstream_ms,latency_upstream_ms,output_tokens) VALUES (?,?,'p','tie',1,'completed',200,200,?,'buffered',1000,1000,?)")
            .bind(format!("tie-{n:02}")).bind(AS_OF).bind(AS_OF).bind(if n == 0 {100} else {0}).execute(storage.pool()).await?;
    }
    let result = storage
        .logs()
        .model_performance_stats(&[("p".into(), "tie".into())], AS_OF)
        .await?;
    assert_eq!(result[0].mixed.selected_request_count, 10);
    assert_eq!(result[0].mixed.valid_tps_count, 0);
    let usage = storage.logs().model_usage_stats("p", "tie").await?;
    assert_eq!(result[0].mixed.average_tps, usage.average_tps);
    assert_eq!(
        result[0].mixed.selected_request_count,
        usage.recent_sample_count
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_raw_timing_and_missing_fields_match_usage() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    // tokens, client streaming, chunks, upstream ms, total ms, first chunk ms, TPS
    let fixtures = [
        (
            Some(80),
            false,
            Some(2),
            Some(1000),
            Some(9000),
            Some(200),
            Some(100.0),
        ),
        (
            Some(80),
            false,
            Some(0),
            Some(1000),
            Some(9000),
            Some(200),
            Some(80.0),
        ),
        (
            Some(100),
            true,
            Some(2),
            Some(1000),
            Some(9000),
            Some(800),
            Some(100.0),
        ),
        (
            Some(7),
            true,
            Some(2),
            Some(70),
            Some(9000),
            Some(30),
            Some(7.0 / 0.07),
        ),
        (
            Some(100),
            true,
            Some(2),
            Some(1000),
            Some(9000),
            Some(1200),
            Some(100.0),
        ),
        (
            Some(50),
            true,
            None,
            None,
            Some(2000),
            Some(200),
            Some(25.0),
        ),
        (
            Some(50),
            true,
            Some(2),
            Some(1000),
            Some(9000),
            None,
            Some(50.0),
        ),
        (
            Some(50),
            true,
            Some(2),
            Some(0),
            Some(2000),
            Some(200),
            None,
        ),
        (Some(50), false, None, Some(-1), Some(2000), None, None),
        (
            Some(0),
            true,
            Some(2),
            Some(1000),
            Some(2000),
            Some(200),
            None,
        ),
        (None, false, None, Some(1000), Some(2000), None, None),
        (Some(50), false, None, None, None, None, None),
        (Some(50), false, None, None, Some(0), None, None),
        (Some(-1), false, None, Some(1000), None, None, None),
    ];
    let mut pairs = Vec::new();
    for (index, (tokens, stream, chunks, upstream, total, first, _)) in fixtures.iter().enumerate()
    {
        let model = format!("raw-{index}");
        sqlx::query("INSERT INTO request_logs(id,created_at,provider_id,upstream_model,output_tokens,is_stream,stream_chunks_count,latency_upstream_ms,latency_total_ms,stream_first_chunk_ms) VALUES (?,?, 'p',?,?,?,?,?,?,?)")
            .bind(&model).bind((AS_OF - WEEK - index as i64).to_string()).bind(&model)
            .bind(tokens).bind(stream).bind(chunks).bind(upstream).bind(total).bind(first)
            .execute(storage.pool()).await?;
        pairs.push(("p".into(), model));
    }
    // An as_of earlier than every row must not exclude retained logs either.
    let results = storage.logs().model_performance_stats(&pairs, 0).await?;
    for (index, ((provider, model), result)) in pairs.iter().zip(results).enumerate() {
        let usage = storage.logs().model_usage_stats(provider, model).await?;
        let expected = fixtures[index].6;
        assert_eq!(result.mixed.average_tps, expected, "{model}");
        assert_eq!(result.mixed.average_tps, usage.average_tps, "{model}");
        assert_eq!(
            result.mixed.selected_request_count,
            usage.recent_sample_count
        );
        assert_eq!(result.mixed.selected_request_count, 1);
        assert_eq!(result.mixed.valid_tps_count, i64::from(expected.is_some()));
        let at = expected.map(|_| AS_OF - WEEK - index as i64);
        assert_eq!(result.mixed.first_sample_at, at);
        assert_eq!(result.mixed.last_sample_at, at);
        assert_eq!(result.unclassified_count, 0);
        assert_eq!(result.untrusted_count, 0);
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
async fn postgres_performance_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_POSTGRES_PERFORMANCE_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    exercise(&storage).await
}

#[tokio::test]
async fn mysql_performance_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_MYSQL_PERFORMANCE_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    exercise(&storage).await
}

async fn external_legacy_provider(storage: &dyn Storage) -> anyhow::Result<String> {
    Ok(storage.providers().create(serde_json::from_value::<CreateProvider>(serde_json::json!({
        "name": format!("legacy-performance-{}", uuid::Uuid::new_v4()),
        "protocol": "openai/chat/completions", "base_url": "https://example.invalid", "api_key": "test"
    }))?).await?.id)
}

#[tokio::test]
async fn sqlite_historical_metadata_recovery() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    let pool = storage.pool();
    sqlx::query("INSERT INTO providers(id,name,protocol,base_url,api_key) VALUES ('p','p','openai/chat/completions','','')").execute(pool).await?;
    let now = chrono::Utc::now().timestamp_millis();
    for (id, at, body) in [
        (
            "recent",
            now - 1000,
            "{\"reasoning_effort\":\"minimal\"}".to_string(),
        ),
        ("missing", now - 1000, String::new()),
        ("oversize", now - 1000, "x".repeat(1_048_577)),
        (
            "old",
            now - WEEK - 1000,
            "{\"reasoning_effort\":\"high\"}".to_string(),
        ),
    ] {
        sqlx::query("INSERT INTO request_logs(id,created_at,provider_id,upstream_model,upstream_request_body,upstream_status_code,client_status_code) VALUES (?,?,'p','Model',?,200,200)")
            .bind(id).bind(at).bind(body).execute(pool).await?;
    }
    // More than one batch at the same timestamp exercises the (created_at,id) cursor.
    for n in 0..120 {
        sqlx::query(
            "INSERT INTO request_logs(id,created_at,upstream_request_body) VALUES (?,?,'{}')",
        )
        .bind(format!("batch-{n:03}"))
        .bind(now - 2000)
        .execute(pool)
        .await?;
    }
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    let recovered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE id LIKE 'batch-%' AND performance_metadata_version=1 AND request_completion='unknown'").fetch_one(pool).await?;
    assert_eq!(recovered, 120);
    for id in ["recent", "missing", "oversize"] {
        let log = storage.logs().find_by_id(id).await?.unwrap();
        assert_eq!(log.performance_metadata_version, 1);
        assert_eq!(log.request_completion, "unknown");
        assert_eq!(log.performance_completed_at, None);
    }
    let log = storage.logs().find_by_id("recent").await?.unwrap();
    assert_eq!(log.upstream_effort_raw.as_deref(), Some("minimal"));
    assert_eq!(log.upstream_effort_tier.as_deref(), Some("low"));
    assert_eq!(
        storage
            .logs()
            .find_by_id("old")
            .await?
            .unwrap()
            .performance_metadata_version,
        0
    );
    let stats = storage
        .logs()
        .model_performance_stats(&[("p".into(), "Model".into())], now)
        .await?;
    assert_eq!(stats[0].mixed.selected_request_count, 4);
    assert_eq!(stats[0].mixed.valid_tps_count, 0);
    assert_eq!(stats[0].untrusted_count, 0);
    let usage = storage.logs().model_usage_stats("p", "Model").await?;
    assert_eq!(stats[0].mixed.average_tps, usage.average_tps);
    assert_eq!(
        stats[0].mixed.selected_request_count,
        usage.recent_sample_count
    );
    Ok(())
}
