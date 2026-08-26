use nyro_core::db::models::LogQuery;
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
async fn insert_log(
    storage: &SqliteStorage,
    id: &str,
    created_at: i64,
    key_id: &str,
    key_name: &str,
    client_model: Option<&str>,
    provider_id: Option<&str>,
    provider_name: Option<&str>,
    upstream_model: Option<&str>,
    status: i32,
    input: i32,
    output: i32,
    cache: i32,
    duration: i64,
    upstream: Option<i64>,
    ttft: Option<i64>,
) {
    sqlx::query("INSERT INTO request_logs (id, created_at, api_key_id, api_key_name, client_model, provider_id, provider_name, upstream_model, client_status_code, input_tokens, output_tokens, cache_read_tokens, latency_total_ms, latency_upstream_ms, stream_first_chunk_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(id).bind(created_at).bind(key_id).bind(key_name).bind(client_model)
        .bind(provider_id).bind(provider_name).bind(upstream_model).bind(status)
        .bind(input).bind(output).bind(cache).bind(duration).bind(upstream).bind(ttft)
        .execute(storage.pool()).await.unwrap();
}

#[tokio::test]
async fn aggregates_by_key_id_and_stable_route_identity() {
    let storage = setup().await;
    let (start, end) = (1_000_i64, 2_000_i64);
    insert_log(
        &storage,
        "a",
        start,
        "key-a",
        "Old",
        Some("client-a"),
        Some("provider-a"),
        Some("Provider Old"),
        Some("up-a"),
        200,
        10,
        2,
        3,
        100,
        Some(80),
        Some(20),
    )
    .await;
    insert_log(
        &storage,
        "b",
        1_100,
        "key-a",
        "Current",
        Some("client-a"),
        Some("provider-a"),
        Some("Provider New"),
        Some("up-a"),
        302,
        20,
        4,
        5,
        200,
        Some(150),
        None,
    )
    .await;
    insert_log(
        &storage,
        "c",
        1_200,
        "key-a",
        "Current",
        Some("client-a"),
        Some("provider-a"),
        Some("Provider New"),
        Some("up-a"),
        404,
        30,
        6,
        7,
        300,
        Some(250),
        Some(40),
    )
    .await;
    insert_log(
        &storage, "d", end, "key-a", "Current", None, None, None, None, 500, 40, 8, 9, 400, None,
        None,
    )
    .await;
    insert_log(
        &storage,
        "outside",
        end + 1,
        "key-a",
        "Newest Outside",
        Some("outside"),
        Some("provider-a"),
        Some("Latest Provider"),
        Some("outside"),
        201,
        99,
        99,
        99,
        999,
        None,
        None,
    )
    .await;
    insert_log(
        &storage,
        "same-name",
        1_300,
        "key-b",
        "Current",
        Some("client-b"),
        Some("provider-b"),
        Some("Other"),
        Some("up-b"),
        201,
        1,
        1,
        1,
        10,
        Some(5),
        Some(5),
    )
    .await;

    let detail = storage
        .logs()
        .api_key_usage_detail("key-a", start, end)
        .await
        .unwrap();
    assert_eq!(
        (
            detail.request_count,
            detail.success_count,
            detail.error_count
        ),
        (4, 1, 2)
    );
    assert_eq!(
        (
            detail.total_input_tokens,
            detail.total_output_tokens,
            detail.total_cache_read_tokens
        ),
        (100, 20, 24)
    );
    assert_eq!(detail.avg_duration_ms, 250.0);
    assert_eq!(detail.avg_first_token_ms, Some(30.0));
    assert_eq!(detail.last_used_at, Some(end));
    assert_eq!(detail.api_key_name, "Newest Outside");
    assert_eq!(detail.model_routes.len(), 2);
    let route = &detail.model_routes[0];
    assert_eq!(
        (
            &route.client_model,
            &route.provider_id,
            &route.upstream_model
        ),
        (
            &"client-a".to_string(),
            &"provider-a".to_string(),
            &"up-a".to_string()
        )
    );
    assert_eq!((route.request_count, route.error_count), (3, 1));
    assert_eq!(route.provider_name, "Latest Provider");
    assert_eq!(route.total_upstream_ms, 480.0);
    assert_eq!(
        (
            &detail.model_routes[1].client_model,
            &detail.model_routes[1].provider_id,
            &detail.model_routes[1].upstream_model
        ),
        (&String::new(), &String::new(), &String::new())
    );

    let stats = storage.logs().stats_by_api_key(None).await.unwrap();
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].api_key_id, "key-a");
    assert_eq!(stats[0].api_key_name, "Newest Outside");
    assert_eq!(stats[1].api_key_id, "key-b");

    let empty = storage
        .logs()
        .api_key_usage_detail("key-a", 10_000, 20_000)
        .await
        .unwrap();
    assert_eq!(empty.api_key_name, "Newest Outside");
    assert_eq!(empty.request_count, 0);
    assert_eq!(empty.success_count, 0);
    assert_eq!(empty.error_count, 0);
    assert_eq!(empty.total_input_tokens, 0);
    assert_eq!(empty.total_output_tokens, 0);
    assert_eq!(empty.total_cache_read_tokens, 0);
    assert_eq!(empty.avg_duration_ms, 0.0);
    assert_eq!(empty.avg_first_token_ms, None);
    assert_eq!(empty.last_used_at, None);
    assert!(empty.model_routes.is_empty());

    let cutoff = chrono::Utc::now().timestamp_millis() - 1;
    sqlx::query("UPDATE request_logs SET created_at = ? WHERE id = 'outside'")
        .bind(cutoff)
        .execute(storage.pool())
        .await
        .unwrap();
    let filtered = storage.logs().stats_by_api_key(Some(1)).await.unwrap();
    let key_a = filtered
        .iter()
        .find(|item| item.api_key_id == "key-a")
        .unwrap();
    assert_eq!(key_a.api_key_name, "Newest Outside");
}

#[tokio::test]
async fn log_filters_compose_and_list_omits_payloads() {
    let storage = setup().await;
    insert_log(
        &storage,
        "one",
        10,
        "key",
        "Name",
        Some("client"),
        Some("provider"),
        Some("Provider"),
        Some("upstream"),
        201,
        1,
        2,
        3,
        10,
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE request_logs SET client_request_body = 'secret-body' WHERE id = 'one'")
        .execute(storage.pool())
        .await
        .unwrap();
    insert_log(
        &storage,
        "two",
        11,
        "key",
        "Name",
        Some("other"),
        Some("provider"),
        Some("Provider"),
        Some("legacy"),
        500,
        1,
        2,
        3,
        10,
        None,
        None,
    )
    .await;

    let page = storage
        .logs()
        .query(LogQuery {
            limit: Some(1),
            offset: Some(0),
            provider: Some("provider".into()),
            client_model: Some("client".into()),
            model: Some("wrong-legacy".into()),
            upstream_model: Some("upstream".into()),
            status_min: Some(200),
            status_max: Some(299),
            api_key: Some("key".into()),
            after: Some(10),
            before: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].client_request_body, None);
    let full = storage.logs().find_by_id("one").await.unwrap().unwrap();
    assert_eq!(full.client_request_body.as_deref(), Some("secret-body"));

    let legacy = storage
        .logs()
        .query(LogQuery {
            model: Some("legacy".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(legacy.total, 1);
}
