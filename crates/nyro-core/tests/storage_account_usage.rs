use nyro_core::db::models::{AccountModelUsage, LogQuery};
use nyro_core::storage::{MemoryStorage, SqliteStorage, Storage};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

async fn sqlite() -> anyhow::Result<SqliteStorage> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    Ok(storage)
}

struct Attempt<'a> {
    provider: Option<&'a str>,
    model: Option<&'a str>,
    route_decision: Option<String>,
    tokens: (Option<i64>, Option<i64>, Option<i64>),
    created_at: i64,
    outcome: &'a str,
    attempt_index: i32,
}

impl Attempt<'_> {
    fn account(account_id: &str) -> Self {
        Self {
            provider: Some("provider"),
            model: Some("model"),
            route_decision: Some(json!({"account": {"account_id": account_id}}).to_string()),
            tokens: (Some(100), Some(10), Some(40)),
            created_at: 1_700_000_000_000,
            outcome: "completed",
            attempt_index: 0,
        }
    }
}

async fn insert(storage: &SqliteStorage, attempt: Attempt<'_>) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO request_logs (id, provider_id, upstream_model, route_decision, \
         input_tokens, output_tokens, cache_read_tokens, created_at, client_model, \
         client_request_id, attempt_index, outcome_version, attempt_outcome, upstream_status_code) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'client-alias', 'same-client-request', ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(attempt.provider)
    .bind(attempt.model)
    .bind(attempt.route_decision)
    .bind(attempt.tokens.0)
    .bind(attempt.tokens.1)
    .bind(attempt.tokens.2)
    .bind(attempt.created_at)
    .bind(attempt.attempt_index)
    .bind(attempt.outcome)
    .bind(if attempt.outcome == "error" { 502 } else { 200 })
    .execute(storage.pool())
    .await?;
    Ok(id)
}

#[tokio::test]
async fn account_usage_strictly_isolates_providers_accounts_and_upstream_models()
-> anyhow::Result<()> {
    let storage = sqlite().await?;
    for account in ["account", "ACCOUNT", "account ", "other-account"] {
        insert(&storage, Attempt::account(account)).await?;
    }
    for model in [Some("Model"), Some("model "), None] {
        insert(
            &storage,
            Attempt {
                model,
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    for provider in [
        Some("Provider"),
        Some("provider "),
        Some("other-provider"),
        None,
    ] {
        insert(
            &storage,
            Attempt {
                provider,
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    let usage = storage
        .logs()
        .account_token_usage("provider", None, None)
        .await?;
    assert_eq!(usage.len(), 7);
    assert!(
        usage
            .iter()
            .all(|row| row.request_count == 1 && row.input_tokens == 100)
    );
    assert_eq!(
        usage
            .iter()
            .map(|row| (row.account_id.as_str(), row.model.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("ACCOUNT", "model"),
            ("account", ""),
            ("account", "Model"),
            ("account", "model"),
            ("account", "model "),
            ("account ", "model"),
            ("other-account", "model"),
        ]
    );
    assert_eq!(
        storage
            .logs()
            .account_token_usage("Provider", None, None)
            .await?
            .len(),
        1
    );
    assert!(
        storage
            .logs()
            .account_token_usage("unknown-provider", None, None)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_skips_unattributable_history_and_ignores_label_or_version_changes()
-> anyhow::Result<()> {
    let storage = sqlite().await?;
    for raw in [
        None,
        Some(""),
        Some("{not json"),
        Some("null"),
        Some("[]"),
        Some("{}"),
        Some(r#"{"account_id":"account"}"#),
        Some(r#"{"account":{"label":"account"}}"#),
        Some(r#"{"account":{"account_id":null}}"#),
        Some(r#"{"account":{"account_id":42}}"#),
        Some(r#"{"account":{"account_id":true}}"#),
        Some(r#"{"account":{"account_id":{}}}"#),
        Some(r#"{"account":{"account_id":[]}}"#),
        Some(r#"{"account":{"account_id":""}}"#),
        Some(r#"{"account":{"account_id":" \t "}}"#),
    ] {
        insert(
            &storage,
            Attempt {
                route_decision: raw.map(str::to_owned),
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    for (label, version, tokens) in [
        ("old-label", 1, (Some(100), Some(10), Some(40))),
        ("renamed-label", 2, (None, Some(7), Some(0))),
        ("replaced-credentials", 9, (None, None, None)),
        ("replaced-credentials", 10, (Some(5), None, None)),
    ] {
        insert(
            &storage,
            Attempt {
                route_decision: Some(
                    json!({"account": {
                        "account_id": "account", "label": label, "status_version": version
                    }})
                    .to_string(),
                ),
                tokens,
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    // There is no current provider/account catalog entry to infer historical identity from.
    assert_eq!(
        storage
            .logs()
            .account_token_usage("provider", None, None)
            .await?,
        vec![AccountModelUsage {
            account_id: "account".into(),
            model: "model".into(),
            request_count: 4,
            input_tokens: 105,
            output_tokens: 17,
            cache_read_tokens: 40,
            missing_usage_count: 3,
            cache_read_reported_count: 2,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_counts_failed_and_retry_attempts_without_adding_cache_or_final_results()
-> anyhow::Result<()> {
    let storage = sqlite().await?;
    let first = insert(
        &storage,
        Attempt {
            outcome: "error",
            ..Attempt::account("account")
        },
    )
    .await?;
    let final_id = insert(
        &storage,
        Attempt {
            tokens: (Some(200), Some(20), Some(60)),
            attempt_index: 1,
            ..Attempt::account("account")
        },
    )
    .await?;
    sqlx::query("INSERT INTO request_results (client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at) VALUES ('same-client-request', 'completed', ?, 2, 1700000000001)")
        .bind(final_id)
        .execute(storage.pool())
        .await?;
    let usage = storage
        .logs()
        .account_token_usage("provider", None, None)
        .await?;
    assert_eq!(
        usage,
        vec![AccountModelUsage {
            account_id: "account".into(),
            model: "model".into(),
            request_count: 2,
            input_tokens: 300,
            output_tokens: 30,
            cache_read_tokens: 100,
            missing_usage_count: 0,
            cache_read_reported_count: 2,
        }]
    );
    assert_eq!(usage[0].input_tokens + usage[0].output_tokens, 330);
    // The aggregate is always derived from retained rows, not a separate lifetime counter.
    storage.logs().delete_by_id(&first).await?;
    assert_eq!(
        storage
            .logs()
            .account_token_usage("provider", None, None)
            .await?[0]
            .input_tokens,
        200
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_supports_inclusive_period_bounds_and_open_ranges() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    let start = 1_700_000_000_000;
    let end = start + 1_000;
    for (created_at, input) in [(start - 1, 1), (start, 10), (end, 100), (end + 1, 1000)] {
        insert(
            &storage,
            Attempt {
                created_at,
                tokens: (Some(input), Some(0), None),
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    for (from, to, count, input) in [
        (None, None, 4, 1111),
        (Some(start), Some(end), 2, 110),
        (None, Some(end), 3, 111),
        (Some(start), None, 3, 1110),
        (Some(start), Some(start), 1, 10),
    ] {
        let rows = storage
            .logs()
            .account_token_usage("provider", from, to)
            .await?;
        assert_eq!(rows[0].request_count, count);
        assert_eq!(rows[0].input_tokens, input);
    }
    assert!(
        storage
            .logs()
            .account_token_usage("provider", Some(end), Some(start))
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_uses_each_exact_accounts_half_open_period() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    for account in [
        "account",
        "other-account",
        "ACCOUNT",
        "account ",
        "empty",
        "reversed",
    ] {
        for (at, input) in [
            (999, 1),
            (1000, 10),
            (2000, 100),
            (3000, 1000),
            (3001, 10000),
        ] {
            insert(
                &storage,
                Attempt {
                    created_at: at,
                    tokens: (Some(input), Some(0), None),
                    ..Attempt::account(account)
                },
            )
            .await?;
        }
    }
    let periods = std::collections::HashMap::from([
        ("account".into(), (1000, 2000)),
        ("other-account".into(), (2000, 3001)),
        ("empty".into(), (1000, 1000)),
        ("reversed".into(), (3000, 1000)),
        ("missing-account".into(), (1000, 3001)),
    ]);
    // Outer bounds still include both endpoints; account periods exclude their end.
    let (all_time, period) = storage
        .logs()
        .account_token_usage_with_periods("provider", Some(1000), Some(3000), &periods)
        .await?;
    assert_eq!(
        all_time,
        storage
            .logs()
            .account_token_usage("provider", Some(1000), Some(3000))
            .await?
    );
    assert_eq!(all_time.len(), 6);
    assert!(
        all_time
            .iter()
            .all(|row| row.request_count == 3 && row.input_tokens == 1110)
    );
    assert_eq!(
        period,
        vec![
            AccountModelUsage {
                account_id: "account".into(),
                model: "model".into(),
                request_count: 1,
                input_tokens: 10,
                ..Default::default()
            },
            AccountModelUsage {
                account_id: "other-account".into(),
                model: "model".into(),
                request_count: 2,
                input_tokens: 1100,
                ..Default::default()
            },
        ]
    );
    let (empty_map_all, empty_map_period) = storage
        .logs()
        .account_token_usage_with_periods("provider", None, None, &std::collections::HashMap::new())
        .await?;
    assert!(empty_map_period.is_empty());
    assert!(empty_map_all.iter().all(|row| row.request_count == 5));
    let (reversed_all, reversed_period) = storage
        .logs()
        .account_token_usage_with_periods("provider", Some(3000), Some(1000), &periods)
        .await?;
    assert!(reversed_all.is_empty() && reversed_period.is_empty());
    Ok(())
}

#[tokio::test]
async fn account_usage_period_preserves_every_pricing_and_missing_usage_field() -> anyhow::Result<()>
{
    let storage = sqlite().await?;
    for (at, tokens) in [
        (99, (Some(1000), Some(100), Some(400))),
        (100, (Some(100), Some(10), Some(40))),
        (101, (None, Some(7), Some(0))),
        (102, (None, None, None)),
        (103, (Some(5), None, None)),
        (104, (Some(1000), Some(100), Some(400))),
    ] {
        insert(
            &storage,
            Attempt {
                created_at: at,
                tokens,
                outcome: "error",
                attempt_index: 1,
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    for model in [Some("Model"), Some("model "), None] {
        insert(
            &storage,
            Attempt {
                created_at: 101,
                model,
                ..Attempt::account("account")
            },
        )
        .await?;
    }
    insert(
        &storage,
        Attempt {
            created_at: 101,
            route_decision: Some("{invalid".into()),
            ..Attempt::account("account")
        },
    )
    .await?;
    let periods = std::collections::HashMap::from([("account".into(), (100, 104))]);
    let (all_time, period) = storage
        .logs()
        .account_token_usage_with_periods("provider", None, None, &periods)
        .await?;
    assert_eq!(
        period,
        storage
            .logs()
            .account_token_usage("provider", Some(100), Some(103))
            .await?
    );
    assert_eq!(period.len(), 4);
    assert_eq!(
        period.iter().find(|row| row.model == "model").unwrap(),
        &AccountModelUsage {
            account_id: "account".into(),
            model: "model".into(),
            request_count: 4,
            input_tokens: 105,
            output_tokens: 17,
            cache_read_tokens: 40,
            missing_usage_count: 3,
            cache_read_reported_count: 2,
        }
    );
    let all_model = all_time.iter().find(|row| row.model == "model").unwrap();
    assert_eq!(all_model.request_count, 6);
    assert_eq!(all_model.input_tokens, 2105);
    assert_eq!(all_model.output_tokens, 217);
    assert_eq!(all_model.cache_read_tokens, 840);
    assert_eq!(all_model.missing_usage_count, 3);
    assert_eq!(all_model.cache_read_reported_count, 4);
    Ok(())
}

#[tokio::test]
async fn account_usage_reads_all_retained_history_without_log_pagination_limits()
-> anyhow::Result<()> {
    let storage = sqlite().await?;
    let row_count = 1_205;
    sqlx::query(
        "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ?) \
         INSERT INTO request_logs (id, created_at, provider_id, upstream_model, route_decision, \
         input_tokens, output_tokens, cache_read_tokens) \
         SELECT 'history-' || n, n, 'provider', 'model', \
         '{\"account\":{\"account_id\":\"historical-account\"}}', 2, 3, 1 FROM seq",
    )
    .bind(row_count)
    .execute(storage.pool())
    .await?;
    let page = storage.logs().query(LogQuery::default()).await?;
    assert!(page.items.len() < row_count as usize);
    let usage = storage
        .logs()
        .account_token_usage("provider", None, None)
        .await?;
    assert_eq!(usage[0].request_count, row_count);
    assert_eq!(usage[0].input_tokens, 2 * row_count);
    assert_eq!(usage[0].output_tokens, 3 * row_count);
    assert_eq!(usage[0].cache_read_tokens, row_count);
    Ok(())
}

#[tokio::test]
async fn account_usage_memory_has_no_retained_logs() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    let periods = std::collections::HashMap::from([("account".into(), (0, 1000))]);
    let (all_time, period) = storage
        .logs()
        .account_token_usage_with_periods("provider", None, Some(1000), &periods)
        .await?;
    assert!(all_time.is_empty() && period.is_empty());
    assert!(
        storage
            .logs()
            .account_token_usage("provider", None, None)
            .await?
            .is_empty()
    );
    assert!(
        storage
            .logs()
            .account_token_usage("provider", Some(0), Some(1))
            .await?
            .is_empty()
    );
    Ok(())
}
