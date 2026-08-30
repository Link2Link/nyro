use nyro_core::storage::{SqliteStorage, Storage};
use sqlx::sqlite::SqlitePoolOptions;

async fn setup() -> SqliteStorage {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await.unwrap();
    storage
}

#[allow(clippy::too_many_arguments)]
async fn log(
    storage: &SqliteStorage,
    id: &str,
    at: i64,
    provider_id: Option<&str>,
    provider_name: Option<&str>,
    api_key_id: Option<&str>,
    api_key_name: Option<&str>,
    model: &str,
    status: i32,
    input: i32,
    output: i32,
    cache: i32,
    duration: i64,
    upstream: Option<i64>,
    ttft: Option<i64>,
) {
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, provider_name, api_key_id, api_key_name, upstream_model, client_status_code, input_tokens, output_tokens, cache_read_tokens, latency_total_ms, latency_upstream_ms, stream_first_chunk_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(at).bind(provider_id).bind(provider_name).bind(api_key_id).bind(api_key_name).bind(model).bind(status).bind(input).bind(output).bind(cache).bind(duration).bind(upstream).bind(ttft).execute(storage.pool()).await.unwrap();
}

#[tokio::test]
async fn detail_aggregates_summary_and_both_breakdowns() {
    let s = setup().await;
    let (start, end) = (1000, 2000);
    // p-a rows: status 200 / 302 / 500 across two keys.
    log(
        &s,
        "a",
        start,
        Some("p-a"),
        Some("Old"),
        Some("k1"),
        Some("KeyOld"),
        "model-x",
        200,
        10,
        2,
        3,
        100,
        Some(80),
        Some(20),
    )
    .await;
    log(
        &s,
        "b",
        1100,
        Some("p-a"),
        Some("New"),
        Some("k1"),
        None,
        "model-x",
        302,
        20,
        4,
        5,
        200,
        Some(150),
        Some(-1),
    )
    .await;
    log(
        &s,
        "e",
        1500,
        Some("p-a"),
        Some("New"),
        Some("k2"),
        Some("KeyTwoLater"),
        "model-x",
        500,
        1,
        1,
        1,
        10,
        None,
        None,
    )
    .await;
    // p-b row without a provider name: name falls back to the id.
    log(
        &s,
        "c",
        1200,
        Some("p-b"),
        None,
        Some("k2"),
        Some("KeyTwo"),
        "model-x",
        404,
        30,
        6,
        7,
        300,
        None,
        None,
    )
    .await;
    // No provider and no key: still counted in the summary.
    log(
        &s,
        "d",
        end,
        None,
        None,
        None,
        None,
        "model-x",
        200,
        40,
        8,
        9,
        400,
        Some(350),
        Some(40),
    )
    .await;
    // Outside the window and a different model: excluded everywhere.
    log(
        &s,
        "outside",
        end + 1,
        Some("p-a"),
        Some("Latest"),
        Some("k1"),
        Some("Later"),
        "model-x",
        200,
        99,
        99,
        99,
        999,
        Some(999),
        Some(999),
    )
    .await;
    log(
        &s,
        "other",
        1300,
        Some("p-a"),
        Some("Other"),
        Some("k1"),
        Some("Other"),
        "model-y",
        200,
        50,
        50,
        50,
        50,
        Some(50),
        Some(50),
    )
    .await;

    let d = s
        .logs()
        .model_usage_detail("model-x", start, end)
        .await
        .unwrap();

    // Summary across every model-x row in the inclusive window.
    assert_eq!((d.request_count, d.success_count, d.error_count), (5, 2, 2));
    assert_eq!(
        (
            d.total_input_tokens,
            d.total_output_tokens,
            d.total_cache_read_tokens
        ),
        (101, 21, 25)
    );
    assert_eq!(d.avg_duration_ms, 202.0);
    // TTFT averages only non-negative samples (20 and 40); -1 is ignored.
    assert_eq!(d.avg_first_token_ms, Some(30.0));
    assert_eq!(d.total_upstream_ms, 580.0);
    assert_eq!(d.last_used_at, Some(end));
    assert_eq!(d.upstream_model, "model-x");

    // Provider breakdown: p-a leads with 3 requests, p-b has 1.
    // The row without a provider id is summarized but not broken down.
    // Names resolve to the all-time latest non-empty log name (the
    // window-outside row still wins), falling back to the id.
    assert_eq!(d.providers.len(), 2);
    assert_eq!(
        (
            d.providers[0].provider_id.as_str(),
            d.providers[0].provider_name.as_str(),
            d.providers[0].request_count,
            d.providers[0].error_count
        ),
        ("p-a", "Latest", 3, 1)
    );
    assert_eq!(d.providers[0].last_used_at, Some(1500));
    assert_eq!(
        (
            d.providers[1].provider_id.as_str(),
            d.providers[1].provider_name.as_str(),
            d.providers[1].request_count,
            d.providers[1].error_count
        ),
        ("p-b", "p-b", 1, 1)
    );
    assert!(
        d.providers
            .iter()
            .all(|p| p.provider_icon.is_none() && p.provider_protocol.is_none())
    );

    // Key breakdown: k1 and k2 tie at 2 requests, ordered by id.
    // Names also resolve to the all-time latest log name, so the
    // window-outside row decides: k1 -> "Later", k2 -> "KeyTwoLater".
    assert_eq!(d.api_keys.len(), 2);
    assert_eq!(
        (
            d.api_keys[0].api_key_id.as_str(),
            d.api_keys[0].api_key_name.as_str(),
            d.api_keys[0].request_count,
            d.api_keys[0].error_count
        ),
        ("k1", "Later", 2, 0)
    );
    assert_eq!(d.api_keys[0].last_used_at, Some(1100));
    assert_eq!(
        (
            d.api_keys[1].api_key_id.as_str(),
            d.api_keys[1].api_key_name.as_str(),
            d.api_keys[1].request_count,
            d.api_keys[1].error_count
        ),
        ("k2", "KeyTwoLater", 2, 2)
    );
}

#[tokio::test]
async fn blank_provider_and_key_rows_are_summarized_but_not_broken_down() {
    let s = setup().await;
    log(
        &s,
        "blank",
        1000,
        Some("  "),
        Some("Blank"),
        Some(""),
        Some("BlankKey"),
        "m",
        200,
        5,
        5,
        5,
        50,
        Some(20),
        Some(10),
    )
    .await;
    let d = s.logs().model_usage_detail("m", 0, 2000).await.unwrap();
    assert_eq!(d.request_count, 1);
    assert!(d.providers.is_empty());
    assert!(d.api_keys.is_empty());
}

#[tokio::test]
async fn unknown_model_returns_zeroed_detail() {
    let s = setup().await;
    log(
        &s,
        "a",
        1000,
        Some("p"),
        Some("P"),
        Some("k"),
        Some("K"),
        "m",
        200,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    let d = s.logs().model_usage_detail("none", 0, 10).await.unwrap();
    assert_eq!(d.upstream_model, "none");
    assert_eq!((d.request_count, d.success_count, d.error_count), (0, 0, 0));
    assert_eq!(d.avg_first_token_ms, None);
    assert_eq!(d.last_used_at, None);
    assert_eq!(d.total_upstream_ms, 0.0);
    assert!(d.providers.is_empty());
    assert!(d.api_keys.is_empty());
}
