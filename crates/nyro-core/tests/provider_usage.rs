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
    name: Option<&str>,
    client_model: Option<&str>,
    protocol: Option<&str>,
    model: Option<&str>,
    status: i32,
    input: i32,
    output: i32,
    cache: i32,
    duration: i64,
    upstream: Option<i64>,
    ttft: Option<i64>,
) {
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, provider_name, client_model, upstream_protocol, upstream_model, client_status_code, input_tokens, output_tokens, cache_read_tokens, latency_total_ms, latency_upstream_ms, stream_first_chunk_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(at).bind(provider_id).bind(name).bind(client_model).bind(protocol).bind(model).bind(status).bind(input).bind(output).bind(cache).bind(duration).bind(upstream).bind(ttft).execute(storage.pool()).await.unwrap();
}

#[tokio::test]
async fn provider_identity_list_and_latest_all_time_name_are_stable() {
    let s = setup().await;
    log(
        &s,
        "a",
        1000,
        Some("p-a"),
        Some("Old"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    log(
        &s,
        "b",
        1100,
        Some("p-a"),
        Some("Same"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    log(
        &s,
        "c",
        1200,
        Some("p-b"),
        Some("Same"),
        None,
        None,
        Some("m"),
        500,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    log(
        &s,
        "d",
        1300,
        Some("p-b"),
        Some("Same"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    log(
        &s,
        "e",
        1400,
        Some("p-a"),
        Some("Newest"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        Some(5),
        None,
    )
    .await;
    log(
        &s,
        "blank",
        1500,
        Some("  "),
        Some("Blank"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        None,
        None,
    )
    .await;
    log(
        &s,
        "null",
        1600,
        None,
        Some("Null"),
        None,
        None,
        Some("m"),
        200,
        1,
        1,
        0,
        10,
        None,
        None,
    )
    .await;
    let rows = s.logs().stats_by_provider(None).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (&r.provider_id, &r.provider, r.request_count))
            .collect::<Vec<_>>(),
        vec![
            (&"p-a".into(), &"Newest".into(), 3),
            (&"p-b".into(), &"Same".into(), 2)
        ]
    );
    assert!(
        rows.iter()
            .all(|r| r.provider_icon.is_none() && r.provider_protocol.is_none())
    );
    assert_eq!(
        s.logs()
            .provider_usage_detail("p-a", 2000, 3000)
            .await
            .unwrap()
            .provider_name,
        "Newest"
    );
    assert_eq!(
        s.logs()
            .provider_usage_detail("missing", 2000, 3000)
            .await
            .unwrap()
            .provider_name,
        "missing"
    );
}

#[tokio::test]
async fn detail_uses_inclusive_bounds_and_groups_only_upstream_model() {
    let s = setup().await;
    let (start, end) = (1000, 2000);
    log(
        &s,
        "a",
        start,
        Some("p"),
        Some("Old"),
        Some("alias-a"),
        Some("proto-a"),
        Some("model-x"),
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
        Some("p"),
        Some("Old"),
        Some("alias-b"),
        Some("proto-b"),
        Some("model-x"),
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
        "c",
        1200,
        Some("p"),
        Some("Old"),
        Some("alias-c"),
        Some("proto-c"),
        Some("model-x"),
        404,
        30,
        6,
        7,
        300,
        None,
        None,
    )
    .await;
    log(
        &s,
        "d",
        end,
        Some("p"),
        Some("Old"),
        None,
        None,
        None,
        500,
        40,
        8,
        9,
        400,
        Some(350),
        Some(40),
    )
    .await;
    log(
        &s,
        "outside",
        end + 1,
        Some("p"),
        Some("Latest"),
        None,
        None,
        Some("outside"),
        201,
        99,
        99,
        99,
        999,
        Some(999),
        Some(999),
    )
    .await;
    let d = s
        .logs()
        .provider_usage_detail("p", start, end)
        .await
        .unwrap();
    assert_eq!((d.request_count, d.success_count, d.error_count), (4, 1, 2));
    assert_eq!(
        (
            d.total_input_tokens,
            d.total_output_tokens,
            d.total_cache_read_tokens
        ),
        (100, 20, 24)
    );
    assert_eq!(d.avg_duration_ms, 250.0);
    assert_eq!(d.avg_first_token_ms, Some(30.0));
    assert_eq!(d.total_upstream_ms, 580.0);
    assert_eq!(d.last_used_at, Some(end));
    assert_eq!(d.provider_name, "Latest");
    assert_eq!(d.models.len(), 2);
    assert_eq!(
        (&d.models[0].upstream_model, d.models[0].request_count),
        (&"model-x".to_string(), 3)
    );
    assert_eq!(d.models[0].error_count, 1);
    assert_eq!(d.models[0].avg_first_token_ms, Some(20.0));
    assert_eq!(d.models[0].last_used_at, Some(1200));
    assert_eq!(
        (&d.models[1].upstream_model, d.models[1].request_count),
        (&String::new(), 1)
    );
}

#[tokio::test]
async fn zero_detail_and_tie_ordering_are_deterministic() {
    let s = setup().await;
    for (id, p) in [("a", "p-b"), ("b", "p-a")] {
        log(
            &s,
            id,
            1000,
            Some(p),
            Some(p),
            None,
            None,
            Some("m"),
            200,
            0,
            0,
            0,
            0,
            None,
            None,
        )
        .await;
    }
    assert_eq!(
        s.logs()
            .stats_by_provider(None)
            .await
            .unwrap()
            .iter()
            .map(|r| r.provider_id.as_str())
            .collect::<Vec<_>>(),
        vec!["p-a", "p-b"]
    );
    let d = s.logs().provider_usage_detail("none", 0, 10).await.unwrap();
    assert_eq!((d.request_count, d.success_count, d.error_count), (0, 0, 0));
    assert!(d.models.is_empty());
    assert_eq!(d.avg_first_token_ms, None);
    assert_eq!(d.last_used_at, None);
    assert_eq!(d.total_upstream_ms, 0.0);
}
