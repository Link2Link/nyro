use nyro_core::db::models::TokenUsageDay;
use nyro_core::storage::{MemoryStorage, SqliteStorage, Storage};
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

async fn attempt(
    storage: &SqliteStorage,
    provider: Option<&str>,
    model: Option<&str>,
    at: i64,
    tokens: (Option<i64>, Option<i64>, Option<i64>),
    index: i32,
) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    // No account IDs. Malformed/NULL historical route decisions are immaterial.
    sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, client_model, created_at, route_decision, input_tokens, output_tokens, cache_read_tokens, client_request_id, attempt_index, outcome_version, attempt_outcome) VALUES (?, ?, ?, 'client-alias', ?, ?, ?, ?, ?, 'same-request', ?, 1, ?)")
        .bind(&id).bind(provider).bind(model).bind(at)
        .bind((index % 2 == 0).then_some("{invalid-json"))
        .bind(tokens.0).bind(tokens.1).bind(tokens.2).bind(index)
        .bind(if index == 0 { "failed" } else { "completed" })
        .execute(storage.pool()).await?;
    Ok(id)
}

fn counts(rows: &[TokenUsageDay]) -> Vec<(&str, i64, i64, i64, i64, i64, i64)> {
    rows.iter()
        .map(|row| {
            (
                row.model.as_str(),
                row.request_count,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.missing_usage_count,
                row.cache_read_reported_count,
            )
        })
        .collect()
}

#[tokio::test]
async fn provider_usage_one_provider_half_open_cycle_and_captured_now() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    for (at, input) in [
        (1, 1),
        (99, 10),
        (100, 100),
        (199, 1000),
        (200, 10000),
        (201, 100000),
        (301, 1000000),
    ] {
        attempt(
            &storage,
            Some("provider"),
            Some("upstream"),
            at,
            (Some(input), Some(2), Some(1)),
            0,
        )
        .await?;
    }
    for provider in [
        Some("Provider"),
        Some("provider "),
        Some("other"),
        Some(""),
        None,
    ] {
        attempt(
            &storage,
            provider,
            Some("upstream"),
            150,
            (Some(1), Some(1), None),
            0,
        )
        .await?;
    }
    for model in [Some("Upstream"), Some("upstream "), None] {
        attempt(
            &storage,
            Some("provider"),
            model,
            150,
            (Some(1), Some(1), None),
            0,
        )
        .await?;
    }
    let (all, period) = storage
        .logs()
        .provider_token_usage_with_period("provider", 300, Some((100, 200)))
        .await?;
    assert_eq!(all.iter().map(|r| r.request_count).sum::<i64>(), 9);
    assert_eq!(period.iter().map(|r| r.request_count).sum::<i64>(), 5);
    assert!(
        all.iter()
            .all(|r| r.provider_id == "provider" && r.date.is_empty() && r.cost.is_none())
    );
    assert_eq!(
        all.iter()
            .find(|r| r.model == "upstream")
            .unwrap()
            .input_tokens,
        111111
    );
    let row = period.iter().find(|r| r.model == "upstream").unwrap();
    assert_eq!(
        (row.request_count, row.input_tokens),
        (2, 1100),
        "[start,end) excludes both old and next-cycle attempts"
    );
    assert_eq!(
        period.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
        ["", "Upstream", "upstream", "upstream "]
    );
    let (capped_all, capped_period) = storage
        .logs()
        .provider_token_usage_with_period("provider", 100, Some((100, 200)))
        .await?;
    assert_eq!(
        (capped_all[0].request_count, capped_all[0].input_tokens),
        (3, 111)
    );
    assert_eq!(
        (
            capped_period[0].request_count,
            capped_period[0].input_tokens
        ),
        (1, 100),
        "now is inclusive and future attempts are excluded"
    );
    for period in [None, Some((100, 100)), Some((200, 100))] {
        let (without_period, current) = storage
            .logs()
            .provider_token_usage_with_period("provider", 300, period)
            .await?;
        assert_eq!(counts(&without_period), counts(&all));
        assert!(current.is_empty());
    }
    let missing = storage
        .logs()
        .provider_token_usage_with_period("missing", 300, Some((100, 200)))
        .await?;
    assert!(missing.0.is_empty() && missing.1.is_empty());
    Ok(())
}

#[tokio::test]
async fn provider_usage_preserves_partial_zero_cache_failed_retry_fields_and_retention()
-> anyhow::Result<()> {
    let storage = sqlite().await?;
    let mut ids = Vec::new();
    for (index, tokens) in [
        (Some(100), Some(10), Some(40)),
        (None, Some(7), Some(0)),
        (None, None, None),
        (Some(5), None, None),
        (Some(0), Some(0), Some(0)),
    ]
    .into_iter()
    .enumerate()
    {
        ids.push(
            attempt(
                &storage,
                Some("provider"),
                Some("upstream"),
                100 + index as i64,
                tokens,
                index as i32,
            )
            .await?,
        );
    }
    sqlx::query("INSERT INTO request_results (client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at) VALUES ('same-request', 'completed', ?, 5, 105)")
        .bind(ids.last().unwrap()).execute(storage.pool()).await?;
    let (all, period) = storage
        .logs()
        .provider_token_usage_with_period("provider", 200, Some((100, 105)))
        .await?;
    assert_eq!(counts(&all), counts(&period));
    assert_eq!(counts(&period), [("upstream", 5, 105, 17, 40, 3, 3)]);
    assert_eq!(
        period[0].input_tokens + period[0].output_tokens,
        122,
        "cache is not extra gross input"
    );
    // Permanent rollups must not be folded back into these retained-only totals.
    storage.logs().delete_by_id(&ids[0]).await?;
    let (all, period) = storage
        .logs()
        .provider_token_usage_with_period("provider", 200, Some((100, 105)))
        .await?;
    assert_eq!(counts(&all), counts(&period));
    assert_eq!(counts(&all), [("upstream", 4, 5, 7, 0, 3, 2)]);
    storage.logs().clear_all().await?;
    let cleared = storage
        .logs()
        .provider_token_usage_with_period("provider", 200, Some((100, 105)))
        .await?;
    assert!(cleared.0.is_empty() && cleared.1.is_empty());
    Ok(())
}

#[tokio::test]
async fn provider_usage_has_no_pagination_cap_and_memory_has_no_logs() -> anyhow::Result<()> {
    let storage = sqlite().await?;
    sqlx::query("WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1205) INSERT INTO request_logs (id, provider_id, upstream_model, created_at, input_tokens, output_tokens, cache_read_tokens) SELECT 'history-' || n, 'provider', 'model', n, 2, 3, 1 FROM seq")
        .execute(storage.pool()).await?;
    let (all, period) = storage
        .logs()
        .provider_token_usage_with_period("provider", 1205, Some((1, 1206)))
        .await?;
    assert_eq!(counts(&all), counts(&period));
    assert_eq!(counts(&all), [("model", 1205, 2410, 3615, 1205, 0, 1205)]);
    let memory = MemoryStorage::new(vec![], vec![], vec![]);
    let empty = memory
        .logs()
        .provider_token_usage_with_period("provider", 1205, Some((1, 1206)))
        .await?;
    assert!(empty.0.is_empty() && empty.1.is_empty());
    Ok(())
}
