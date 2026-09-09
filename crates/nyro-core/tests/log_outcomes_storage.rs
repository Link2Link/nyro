//! Outcome persistence and shared predicate parity. External tests require dedicated,
//! disposable databases and never read application/production configuration.
use nyro_core::db::models::{LogQuery, RequestResult};
use nyro_core::logging::{LogEntry, diagnostics::LogDiagnostic};
use nyro_core::performance::PerformanceMetadata;
use nyro_core::protocol::ir::Usage;
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, SqliteStorage, Storage, UsageWindow};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn entry() -> LogEntry {
    LogEntry {
        diagnostic: LogDiagnostic::default(),
        api_key_id: Some("key".into()),
        api_key_name: Some("test key".into()),
        created_at: chrono::Utc::now().timestamp_millis(),
        client_protocol: "openai/chat/completions".into(),
        upstream_protocol: "openai/chat/completions".into(),
        provider_id: "provider".into(),
        provider_name: "test provider".into(),
        model_id: None,
        model_name: None,
        upstream_url: None,
        client_model: "client-model".into(),
        upstream_model: "upstream-model".into(),
        reasoning_effort: None,
        route_decision: None,
        method: Some("POST".into()),
        path: Some("/v1/chat/completions".into()),
        client_request_headers: Some("client request headers".into()),
        client_request_body: Some("client request body".into()),
        client_response_headers: Some("client response headers".into()),
        client_response_body: Some("client response body".into()),
        upstream_request_headers: Some("upstream request headers".into()),
        upstream_request_body: Some("upstream request body".into()),
        upstream_response_headers: Some("upstream response headers".into()),
        upstream_response_body: Some("upstream response body".into()),
        upstream_status_code: Some(200),
        client_status_code: 200,
        latency_total_ms: 1000,
        latency_upstream_ms: Some(1000),
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            ..Default::default()
        },
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
        // A completed performance marker must NOT promote an unknown diagnostic.
        performance: PerformanceMetadata {
            version: 1,
            completion: "completed".into(),
            ..Default::default()
        },
    }
}

struct Case {
    client: Option<i32>,
    upstream: Option<i32>,
    version: i32,
    outcome: &'static str,
    effective: &'static str,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for code in [400, 429, 500, 599] {
        cases.push(Case {
            client: Some(code),
            upstream: None,
            version: 0,
            outcome: "unknown",
            effective: "error",
        });
        cases.push(Case {
            client: Some(200),
            upstream: Some(code),
            version: 0,
            outcome: "unknown",
            effective: "error",
        });
    }
    for (client, upstream, version, outcome, effective) in [
        (None, None, 0, "unknown", "unknown"),
        (Some(600), Some(600), 0, "unknown", "unknown"),
        (Some(399), Some(399), 0, "unknown", "unknown"),
        (Some(200), Some(200), 0, "failed", "unknown"),
        (Some(200), Some(200), 0, "completed", "unknown"),
        (Some(200), Some(200), 2, "failed", "unknown"),
        (Some(200), Some(200), 2, "completed", "unknown"),
        (Some(200), Some(200), 1, "failed", "error"),
        (Some(200), Some(200), 1, "timed_out", "error"),
        (None, None, 1, "failed", "error"),
        (Some(200), Some(200), 1, "completed", "completed"),
        (None, None, 1, "completed", "completed"),
        (Some(200), Some(200), 1, "cancelled", "cancelled"),
        (None, None, 1, "cancelled", "cancelled"),
        (Some(200), Some(200), 1, "output_limited", "output_limited"),
        (None, None, 1, "output_limited", "output_limited"),
        (Some(200), Some(200), 1, "unknown", "unknown"),
        (Some(200), Some(200), 1, "future_outcome", "unknown"),
        (Some(500), Some(200), 1, "completed", "error"),
        (Some(200), Some(500), 1, "cancelled", "error"),
        (Some(500), Some(200), 1, "output_limited", "error"),
        (Some(200), Some(500), 2, "completed", "error"),
    ] {
        cases.push(Case {
            client,
            upstream,
            version,
            outcome,
            effective,
        });
    }
    cases
}

fn case_id(index: usize) -> String {
    uuid::Uuid::from_u128(index as u128 + 1).to_string()
}

async fn seed_matrix(storage: &dyn Storage) -> anyhow::Result<()> {
    storage.logs().clear_all().await?;
    let rows = cases()
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut row = entry();
            row.diagnostic.log_id = case_id(index);
            row.diagnostic.outcome_version = case.version;
            row.diagnostic.attempt_outcome = case.outcome.into();
            row.client_status_code = case.client.unwrap_or(0);
            row.upstream_status_code = case.upstream;
            row.created_at -= index as i64;
            row
        })
        .collect();
    storage.logs().append_batch(rows).await
}

async fn assert_matrix(storage: &dyn Storage) -> anyhow::Result<()> {
    let logs = storage.logs();
    let cases = cases();
    let total = cases.len() as i64;
    let count = |outcome| {
        cases
            .iter()
            .filter(|case| case.effective == outcome)
            .count() as i64
    };
    let errors = count("error");
    for outcome in [
        "error",
        "completed",
        "cancelled",
        "output_limited",
        "unknown",
    ] {
        let page = logs
            .query(LogQuery {
                outcome: Some(outcome.into()),
                limit: Some(100),
                ..Default::default()
            })
            .await?;
        assert_eq!(page.total, count(outcome), "{outcome}");
        let mut ids: Vec<_> = page.items.iter().map(|row| row.id.clone()).collect();
        ids.sort();
        let mut expected: Vec<_> = cases
            .iter()
            .enumerate()
            .filter(|(_, case)| case.effective == outcome)
            .map(|(i, _)| case_id(i))
            .collect();
        expected.sort();
        assert_eq!(ids, expected, "{outcome}");
        assert!(page.items.iter().all(
            |row| row.client_request_body.is_none() && row.upstream_response_headers.is_none()
        ));
    }
    for is_error in [true, false] {
        let page = logs
            .query(LogQuery {
                is_error: Some(is_error),
                limit: Some(100),
                ..Default::default()
            })
            .await?;
        assert_eq!(page.total, if is_error { errors } else { total - errors });
    }
    let all_filters = logs
        .query(LogQuery {
            is_error: Some(true),
            outcome: Some("error".into()),
            provider: Some("provider".into()),
            client_model: Some("client-model".into()),
            upstream_model: Some("upstream-model".into()),
            api_key: Some("key".into()),
            status_min: Some(400),
            status_max: Some(599),
            after: Some(0),
            before: Some(chrono::Utc::now().timestamp_millis() + 1000),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        all_filters.total,
        cases
            .iter()
            .filter(|c| c.client.is_some_and(|s| (400..=599).contains(&s)))
            .count() as i64
    );
    let filtered = logs
        .query(LogQuery {
            is_error: Some(true),
            status_max: Some(200),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        filtered.total,
        cases
            .iter()
            .filter(|c| c.effective == "error" && c.client.is_some_and(|s| s <= 200))
            .count() as i64
    );
    assert_eq!(
        logs.query(LogQuery {
            is_error: Some(true),
            outcome: Some("cancelled".into()),
            ..Default::default()
        })
        .await?
        .total,
        0
    );
    assert!(
        logs.query(LogQuery {
            outcome: Some("invalid".into()),
            ..Default::default()
        })
        .await
        .is_err()
    );
    for hours in [None, Some(24)] {
        let summary = logs.stats_overview(hours).await?;
        assert_eq!(
            (summary.total_requests, summary.error_count),
            (total, errors)
        );
        let providers = logs.stats_by_provider(hours).await?;
        assert_eq!(
            (providers[0].request_count, providers[0].error_count),
            (total, errors)
        );
        let keys = logs.stats_by_api_key(hours).await?;
        assert_eq!(
            (keys[0].request_count, keys[0].error_count),
            (total, errors)
        );
    }
    let hourly = logs.stats_hourly(24).await?;
    assert_eq!(
        hourly.iter().map(|row| row.error_count).sum::<i64>(),
        errors
    );
    let end = chrono::Utc::now().timestamp_millis() + 1000;
    let buckets = logs.stats_time_buckets(0, end, 60_000, None).await?;
    assert_eq!(
        buckets.iter().map(|row| row.request_count).sum::<i64>(),
        total
    );
    assert_eq!(
        buckets.iter().map(|row| row.error_count).sum::<i64>(),
        errors
    );
    let key_buckets = logs
        .api_key_model_time_buckets("key", 0, end, 60_000)
        .await?;
    assert_eq!(
        key_buckets.iter().map(|row| row.error_count).sum::<i64>(),
        errors
    );
    let provider = logs.provider_usage_detail("provider", 0, end).await?;
    let key = logs.api_key_usage_detail("key", 0, end).await?;
    let model = logs.model_usage_detail("upstream-model", 0, end).await?;
    for (requests, success, failed, unknown, cancelled, limited, version) in [
        (
            provider.request_count,
            provider.success_count,
            provider.error_count,
            provider.unknown_count,
            provider.cancelled_count,
            provider.output_limited_count,
            provider.outcome_stats_version,
        ),
        (
            key.request_count,
            key.success_count,
            key.error_count,
            key.unknown_count,
            key.cancelled_count,
            key.output_limited_count,
            key.outcome_stats_version,
        ),
        (
            model.request_count,
            model.success_count,
            model.error_count,
            model.unknown_count,
            model.cancelled_count,
            model.output_limited_count,
            model.outcome_stats_version,
        ),
    ] {
        assert_eq!(
            (
                requests, success, failed, unknown, cancelled, limited, version
            ),
            (
                total,
                count("completed"),
                errors,
                count("unknown"),
                count("cancelled"),
                count("output_limited"),
                1
            )
        );
        assert_eq!(requests, success + failed + unknown + cancelled + limited);
    }
    assert_eq!(
        provider.models.iter().map(|r| r.error_count).sum::<i64>(),
        errors
    );
    assert_eq!(
        key.model_routes.iter().map(|r| r.error_count).sum::<i64>(),
        errors
    );
    assert_eq!(
        model.providers.iter().map(|r| r.error_count).sum::<i64>(),
        errors
    );
    assert_eq!(
        model.api_keys.iter().map(|r| r.error_count).sum::<i64>(),
        errors
    );
    assert_eq!(logs.clear_payloads().await?, total as u64);
    for (index, _) in cases.iter().enumerate() {
        let row = logs.find_by_id(&case_id(index)).await?.unwrap();
        assert!(row.payload_cleared_at.is_some());
        assert!(row.client_request_headers.is_none() && row.upstream_response_body.is_none());
    }
    assert_eq!(logs.clear_errors().await?, errors as u64);
    assert_eq!(logs.stats_overview(None).await?.error_count, 0);
    assert_eq!(
        logs.stats_overview(None).await?.total_requests,
        total - errors
    );
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(
            logs.find_by_id(&case_id(i)).await?.is_some(),
            case.effective != "error"
        );
    }
    Ok(())
}

async fn assert_persistence(storage: &dyn Storage) -> anyhow::Result<()> {
    let logs = storage.logs();
    logs.clear_all().await?;
    let mut first = entry();
    first.diagnostic.client_request_id = Some("client-request".into());
    first.diagnostic.attempt_index = Some(0);
    first.diagnostic.outcome_version = 1;
    first.diagnostic.attempt_outcome = "failed".into();
    first.diagnostic.failure_kind = Some("upstream_read".into());
    first.diagnostic.failure_stage = Some("read".into());
    first.diagnostic.error_message = Some("Read failed".into());
    first.diagnostic.error_causes = vec!["outer cause".into(), "inner cause".into()];
    first.diagnostic.payload_metadata =
        serde_json::json!({"client_request_body": {"captured_bytes": 19, "truncated": false}});
    let first_id = first.diagnostic.log_id.clone();
    let mut final_entry = entry();
    final_entry.provider_id = "provider-b".into();
    final_entry.provider_name = "test provider B".into();
    final_entry.diagnostic.client_request_id = Some("client-request".into());
    final_entry.diagnostic.attempt_index = Some(1);
    final_entry.diagnostic.outcome_version = 1;
    final_entry.diagnostic.attempt_outcome = "completed".into();
    let final_id = final_entry.diagnostic.log_id.clone();
    let result = RequestResult {
        client_request_id: "client-request".into(),
        final_outcome: "completed".into(),
        final_attempt_id: Some(final_id.clone()),
        attempt_count: 2,
        finished_at: final_entry.created_at,
    };
    final_entry.diagnostic.final_result = Some(result.clone());
    logs.append_batch(vec![first, final_entry]).await?;
    let row = logs.find_by_id(&first_id).await?.unwrap();
    assert_eq!(row.attempt_index, Some(0));
    assert_eq!(row.failure_kind.as_deref(), Some("upstream_read"));
    assert_eq!(row.failure_stage.as_deref(), Some("read"));
    assert_eq!(row.error_message.as_deref(), Some("Read failed"));
    assert_eq!(
        serde_json::from_str::<Vec<String>>(row.error_causes.as_deref().unwrap())?,
        ["outer cause", "inner cause"]
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(row.payload_metadata.as_deref().unwrap())?["client_request_body"]
            ["captured_bytes"],
        19
    );
    assert_eq!(
        logs.request_result("client-request").await?,
        Some(result.clone())
    );
    assert_eq!(
        logs.query(LogQuery {
            client_request_id: Some("client-request".into()),
            ..Default::default()
        })
        .await?
        .total,
        2
    );
    assert_eq!(logs.stats_overview(None).await?.total_requests, 2);
    assert_eq!(
        storage
            .auth()
            .unwrap()
            .request_count_since("key", UsageWindow::Day)
            .await?,
        2
    );
    assert_eq!(
        storage
            .auth()
            .unwrap()
            .token_count_since("key", UsageWindow::Day)
            .await?,
        60
    );
    // Legacy TPS is untouched by diagnostic classification and separate result rows.
    let usage = logs.model_usage_stats("provider", "upstream-model").await?;
    assert_eq!(usage.recent_sample_count, 1);
    assert_eq!(usage.average_tps, Some(20.0));
    let final_usage = logs
        .model_usage_stats("provider-b", "upstream-model")
        .await?;
    assert_eq!(final_usage.recent_sample_count, 1);
    assert_eq!(final_usage.average_tps, Some(20.0));
    assert_eq!(logs.clear_payloads().await?, 2);
    for id in [&first_id, &final_id] {
        let row = logs.find_by_id(id).await?.unwrap();
        assert!(row.payload_cleared_at.is_some());
        assert!(
            [
                row.client_request_headers,
                row.client_request_body,
                row.client_response_headers,
                row.client_response_body,
                row.upstream_request_headers,
                row.upstream_request_body,
                row.upstream_response_headers,
                row.upstream_response_body
            ]
            .iter()
            .all(Option::is_none)
        );
    }
    assert_eq!(logs.clear_payloads().await?, 0);
    // Re-running migrations cannot resurrect cleared payloads or promote metadata.
    storage.bootstrap().migrate().await?;
    assert!(
        logs.find_by_id(&first_id)
            .await?
            .unwrap()
            .client_request_body
            .is_none()
    );
    assert!(
        logs.find_by_id(&first_id)
            .await?
            .unwrap()
            .payload_metadata
            .is_some()
    );
    assert_eq!(logs.clear_errors().await?, 1);
    assert_eq!(
        logs.request_result("client-request").await?,
        Some(result.clone())
    );
    assert_eq!(logs.delete_by_id(&final_id).await?, 1);
    assert!(logs.request_result("client-request").await?.is_none());
    // Deleting the final attempt first must retain the result while an earlier
    // attempt remains; the informational final_attempt_id is intentionally not an FK.
    let mut survivor = entry();
    survivor.diagnostic.client_request_id = Some("client-request".into());
    let survivor_id = survivor.diagnostic.log_id.clone();
    let mut final_again = entry();
    final_again.diagnostic.log_id = final_id.clone();
    final_again.diagnostic.client_request_id = Some("client-request".into());
    final_again.diagnostic.final_result = Some(result.clone());
    logs.append_batch(vec![survivor, final_again]).await?;
    assert_eq!(logs.delete_by_id(&final_id).await?, 1);
    assert_eq!(logs.request_result("client-request").await?, Some(result));
    assert_eq!(logs.delete_by_id(&survivor_id).await?, 1);
    assert!(logs.request_result("client-request").await?.is_none());
    // A duplicate stable ID must reject the entire batch, including its final result.
    let mut duplicate = entry();
    duplicate.diagnostic.client_request_id = Some("rolled-back".into());
    duplicate.diagnostic.final_result = Some(RequestResult {
        client_request_id: "rolled-back".into(),
        final_outcome: "completed".into(),
        final_attempt_id: Some(duplicate.diagnostic.log_id.clone()),
        attempt_count: 1,
        finished_at: duplicate.created_at,
    });
    assert!(
        logs.append_batch(vec![duplicate.clone(), duplicate])
            .await
            .is_err()
    );
    assert_eq!(logs.stats_overview(None).await?.total_requests, 0);
    assert!(logs.request_result("rolled-back").await?.is_none());
    // Retention deletes orphan summaries without introducing extra counted rows.
    let mut old = entry();
    old.created_at = 1;
    old.diagnostic.client_request_id = Some("old-request".into());
    old.diagnostic.final_result = Some(RequestResult {
        client_request_id: "old-request".into(),
        final_outcome: "unknown".into(),
        final_attempt_id: Some(old.diagnostic.log_id.clone()),
        attempt_count: 1,
        finished_at: 1,
    });
    logs.append_batch(vec![old.clone()]).await?;
    assert_eq!(logs.cleanup_before("-7 days").await?, 1);
    assert!(logs.request_result("old-request").await?.is_none());
    logs.append_batch(vec![old]).await?;
    assert_eq!(logs.clear_all().await?, 1);
    assert!(logs.request_result("old-request").await?.is_none());
    Ok(())
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
async fn sqlite_outcome_predicates_and_persistence() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    seed_matrix(&storage).await?;
    sqlx::query("UPDATE request_logs SET client_status_code = NULL WHERE client_status_code = 0")
        .execute(storage.pool())
        .await?;
    assert_matrix(&storage).await?;
    assert_persistence(&storage).await
}

#[tokio::test]
async fn sqlite_batch_trigger_failure_is_atomic() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    sqlx::raw_sql("CREATE TRIGGER reject_second BEFORE INSERT ON request_logs WHEN NEW.attempt_index = 1 BEGIN SELECT RAISE(ABORT, 'test failure'); END;").execute(storage.pool()).await?;
    let mut first = entry();
    first.diagnostic.client_request_id = Some("trigger-request".into());
    first.diagnostic.final_result = Some(RequestResult {
        client_request_id: "trigger-request".into(),
        final_outcome: "unknown".into(),
        final_attempt_id: None,
        attempt_count: 1,
        finished_at: first.created_at,
    });
    let mut second = entry();
    second.diagnostic.attempt_index = Some(1);
    assert!(
        storage
            .logs()
            .append_batch(vec![first, second])
            .await
            .is_err()
    );
    assert_eq!(storage.logs().stats_overview(None).await?.total_requests, 0);
    assert!(
        storage
            .logs()
            .request_result("trigger-request")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_legacy_upgrade_defaults_and_health() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    sqlx::query("DROP INDEX idx_logs_client_request_attempt")
        .execute(storage.pool())
        .await?;
    for column in [
        "client_request_id",
        "attempt_index",
        "outcome_version",
        "attempt_outcome",
        "failure_kind",
        "failure_stage",
        "error_message",
        "error_causes_json",
        "payload_metadata_json",
        "payload_cleared_at",
    ] {
        sqlx::query(&format!("ALTER TABLE request_logs DROP COLUMN {column}"))
            .execute(storage.pool())
            .await?;
    }
    sqlx::query("DROP TABLE request_results")
        .execute(storage.pool())
        .await?;
    sqlx::query("INSERT INTO request_logs(id, created_at, client_status_code, performance_metadata_version, request_completion) VALUES ('legacy', 1, 200, 1, 'failed')").execute(storage.pool()).await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    let legacy = storage.logs().find_by_id("legacy").await?.unwrap();
    assert_eq!(
        (legacy.outcome_version, legacy.attempt_outcome.as_str()),
        (0, "unknown")
    );
    assert!(legacy.client_request_id.is_none());
    assert!(legacy.error_causes.is_none());
    assert_eq!(storage.logs().clear_errors().await?, 0);
    assert_eq!(
        storage
            .logs()
            .query(LogQuery {
                outcome: Some("unknown".into()),
                ..Default::default()
            })
            .await?
            .total,
        1
    );
    Ok(())
}

fn external(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_OUTCOME_DATABASES_ONLY").as_deref() == Ok("1"),
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
async fn postgres_outcomes_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_POSTGRES_OUTCOMES_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    seed_matrix(&storage).await?;
    sqlx::query("UPDATE request_logs SET client_status_code = NULL WHERE client_status_code = 0")
        .execute(storage.pool())
        .await?;
    assert_matrix(&storage).await?;
    assert_persistence(&storage).await?;
    // Roll back even a final-result upsert performed before a trigger failure.
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION nyro_test_reject_attempt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.attempt_index = 1 THEN RAISE EXCEPTION 'test failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_second BEFORE INSERT ON request_logs FOR EACH ROW EXECUTE FUNCTION nyro_test_reject_attempt();").execute(storage.pool()).await?;
    assert_trigger_rollback(&storage).await?;
    sqlx::raw_sql(
        "DROP TRIGGER reject_second ON request_logs; DROP FUNCTION nyro_test_reject_attempt();",
    )
    .execute(storage.pool())
    .await?;
    for column in DIAGNOSTIC_COLUMNS {
        sqlx::query(&format!("ALTER TABLE request_logs DROP COLUMN {column}"))
            .execute(storage.pool())
            .await?;
    }
    sqlx::query("DROP TABLE request_results")
        .execute(storage.pool())
        .await?;
    sqlx::query("INSERT INTO request_logs(id, created_at, client_status_code, performance_metadata_version, request_completion) VALUES ('legacy', 1, 200, 1, 'failed')").execute(storage.pool()).await?;
    assert_legacy_upgrade(&storage).await
}

#[tokio::test]
async fn mysql_outcomes_optional() -> anyhow::Result<()> {
    let Some(config) = external("NYRO_TEST_MYSQL_OUTCOMES_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    sqlx::raw_sql("DROP TRIGGER IF EXISTS reject_second")
        .execute(storage.pool())
        .await?;
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    seed_matrix(&storage).await?;
    sqlx::query("UPDATE request_logs SET client_status_code = NULL WHERE client_status_code = 0")
        .execute(storage.pool())
        .await?;
    assert_matrix(&storage).await?;
    assert_persistence(&storage).await?;
    sqlx::raw_sql("CREATE TRIGGER reject_second BEFORE INSERT ON request_logs FOR EACH ROW BEGIN IF NEW.attempt_index = 1 THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'test failure'; END IF; END").execute(storage.pool()).await?;
    assert_trigger_rollback(&storage).await?;
    sqlx::raw_sql("DROP TRIGGER reject_second")
        .execute(storage.pool())
        .await?;
    sqlx::query("DROP INDEX idx_logs_client_request_attempt ON request_logs")
        .execute(storage.pool())
        .await?;
    for column in DIAGNOSTIC_COLUMNS {
        sqlx::query(&format!("ALTER TABLE request_logs DROP COLUMN {column}"))
            .execute(storage.pool())
            .await?;
    }
    sqlx::query("DROP TABLE request_results")
        .execute(storage.pool())
        .await?;
    sqlx::query("INSERT INTO request_logs(id, created_at, client_status_code, performance_metadata_version, request_completion) VALUES ('legacy', 1, 200, 1, 'failed')").execute(storage.pool()).await?;
    assert_legacy_upgrade(&storage).await
}

const DIAGNOSTIC_COLUMNS: &[&str] = &[
    "client_request_id",
    "attempt_index",
    "outcome_version",
    "attempt_outcome",
    "failure_kind",
    "failure_stage",
    "error_message",
    "error_causes_json",
    "payload_metadata_json",
    "payload_cleared_at",
];

async fn assert_legacy_upgrade(storage: &dyn Storage) -> anyhow::Result<()> {
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    let legacy = storage.logs().find_by_id("legacy").await?.unwrap();
    assert_eq!(
        (legacy.outcome_version, legacy.attempt_outcome.as_str()),
        (0, "unknown")
    );
    assert!(legacy.client_request_id.is_none());
    assert!(legacy.error_causes.is_none());
    assert_eq!(storage.logs().clear_errors().await?, 0);
    assert_eq!(
        storage
            .logs()
            .query(LogQuery {
                outcome: Some("unknown".into()),
                ..Default::default()
            })
            .await?
            .total,
        1
    );
    storage.logs().clear_all().await?;
    Ok(())
}

async fn assert_trigger_rollback(storage: &dyn Storage) -> anyhow::Result<()> {
    let mut first = entry();
    first.diagnostic.client_request_id = Some("trigger-request".into());
    first.diagnostic.final_result = Some(RequestResult {
        client_request_id: "trigger-request".into(),
        final_outcome: "unknown".into(),
        final_attempt_id: None,
        attempt_count: 1,
        finished_at: first.created_at,
    });
    let mut second = entry();
    second.diagnostic.attempt_index = Some(1);
    assert!(
        storage
            .logs()
            .append_batch(vec![first, second])
            .await
            .is_err()
    );
    assert_eq!(storage.logs().stats_overview(None).await?.total_requests, 0);
    assert!(
        storage
            .logs()
            .request_result("trigger-request")
            .await?
            .is_none()
    );
    Ok(())
}
