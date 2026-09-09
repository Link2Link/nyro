//! Public storage contract coverage. External backends are opt-in and may only
//! target disposable test databases: set NYRO_TEST_RATINGS_DATABASES_ONLY=1 plus
//! NYRO_TEST_POSTGRES_RATINGS_URL / NYRO_TEST_MYSQL_RATINGS_URL. Their database
//! names must start with `nyro_test_` or end with `_test`; these tests migrate the
//! schema and drop/recreate the ratings table. No default/deployment URL is used.
//! Set NYRO_TEST_RATINGS_PRECREATE_REFERENCE=1 with NEW EMPTY test databases to
//! additionally load the generated reference SQL before running migration tests.

use nyro_core::db::models::{CreateProvider, ModelRatingEntry, Provider};
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MemoryStorage, MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const EARLIER: &str = "2024-02-03T04:05:06.007Z";
const LATER: &str = "2025-06-07T08:09:10.011Z";

fn entry(prefix: &str, score: i32) -> ModelRatingEntry {
    ModelRatingEntry {
        model_prefix: prefix.to_owned(),
        score,
        updated_at: EARLIER.to_owned(),
    }
}

async fn sqlite(foreign_keys: bool) -> anyhow::Result<SqliteStorage> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(foreign_keys),
        )
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    Ok(storage)
}

async fn provider(storage: &dyn Storage, label: &str) -> anyhow::Result<Provider> {
    storage
        .providers()
        .create(serde_json::from_value::<CreateProvider>(
            serde_json::json!({
                "name": format!("rating-{label}-{}", uuid::Uuid::new_v4()),
                "protocol": "openai/chat/completions",
                "base_url": "https://example.invalid/v1",
                "api_key": "test-key"
            }),
        )?)
        .await
}

async fn exercise_sql_contract(storage: &dyn Storage) -> anyhow::Result<()> {
    let provider = provider(storage, "main").await?;
    let store = storage.model_ratings().expect("SQL rating capability");

    assert!(store.list().await?.is_empty());
    let zero = entry("model", 0);
    assert_eq!(store.upsert(zero.clone()).await?, zero);
    let hundred = entry("deepseek-v4-pro", 100);
    assert_eq!(store.upsert(hundred.clone()).await?, hundred);

    for (index, prefix) in [
        "Model",
        "model ",
        " model",
        "café",
        "cafe",
        "cafe\u{301}",
        "a/b?c#d%你好",
        "🦀",
    ]
    .iter()
    .enumerate()
    {
        let row = entry(prefix, (index + 1) as i32);
        assert_eq!(store.upsert(row.clone()).await?, row);
    }
    let mut expected_keys = vec![
        "model",
        "Model",
        "model ",
        " model",
        "café",
        "cafe",
        "cafe\u{301}",
        "a/b?c#d%你好",
        "🦀",
        "deepseek-v4-pro",
    ];
    expected_keys.sort_unstable();
    assert_eq!(
        store
            .list()
            .await?
            .iter()
            .map(|r| r.model_prefix.as_str())
            .collect::<Vec<_>>(),
        expected_keys
    );

    // Upserting an existing prefix replaces score and timestamp in place.
    let changed = ModelRatingEntry {
        score: 100,
        updated_at: LATER.to_owned(),
        ..entry("model", 0)
    };
    assert_eq!(store.upsert(changed.clone()).await?, changed);
    assert_eq!(
        store
            .list()
            .await?
            .iter()
            .filter(|r| r.model_prefix == "model")
            .count(),
        1
    );

    for score in [-1, 101, i32::MAX] {
        assert!(store.upsert(entry("model", score)).await.is_err());
    }
    for prefix in ["x".repeat(1024), "🦀".repeat(256)] {
        let row = entry(&prefix, 50);
        assert_eq!(store.upsert(row.clone()).await?, row);
    }
    for prefix in [String::new(), "x".repeat(1025), "🦀".repeat(257)] {
        assert!(store.upsert(entry(&prefix, 50)).await.is_err());
    }

    store.delete("Model").await?;
    store.delete("Model").await?;
    assert!(store
        .list()
        .await?
        .iter()
        .all(|r| r.model_prefix != "Model"));
    assert!(store
        .list()
        .await?
        .iter()
        .any(|r| r.model_prefix == "model"));

    // Restore replaces the entire prefix table and preserves timestamps.
    let restored = [
        entry("restored", 0),
        ModelRatingEntry {
            updated_at: LATER.to_owned(),
            ..entry("restored ", 100)
        },
    ];
    store.restore(&restored).await?;
    assert_eq!(store.list().await?, restored);
    // Failure after a deletion and valid insertion must roll back the replace.
    assert!(store
        .restore(&[
            entry("partial", 25),
            entry("invalid", 101),
        ])
        .await
        .is_err());
    assert_eq!(store.list().await?, restored);
    assert!(store
        .restore(&[
            entry("partial", 25),
            entry(&"🦀".repeat(257), 25),
        ])
        .await
        .is_err());
    assert_eq!(store.list().await?, restored);
    store.restore(&[]).await?;
    assert!(store.list().await?.is_empty());

    // Ratings survive provider deletion: prefixes are provider-independent.
    store.upsert(hundred.clone()).await?;
    storage.providers().delete(&provider.id).await?;
    assert_eq!(store.list().await?, vec![hundred]);
    Ok(())
}

async fn assert_rating_errors(storage: &dyn Storage) -> anyhow::Result<()> {
    let store = storage.model_ratings().unwrap();
    assert!(store.list().await.is_err());
    assert!(store.upsert(entry("model", 50)).await.is_err());
    assert!(store.delete("model").await.is_err());
    assert!(store.restore(&[]).await.is_err());
    assert!(store.restore(&[entry("model", 50)]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn sqlite_rating_contract() -> anyhow::Result<()> {
    exercise_sql_contract(&sqlite(true).await?).await
}

#[tokio::test]
async fn sqlite_constraints() -> anyhow::Result<()> {
    let storage = sqlite(true).await?;
    for score in ["NULL", "-1", "101", "0.5", "'not-an-integer'"] {
        let sql = format!(
            "INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES ('raw', {score}, ?)"
        );
        assert!(sqlx::query(&sql).bind(EARLIER).execute(storage.pool()).await.is_err());
    }
    for prefix in [String::new(), "x".repeat(1025), "🦀".repeat(257)] {
        assert!(sqlx::query("INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES (?, 50, ?)")
            .bind(prefix).bind(EARLIER).execute(storage.pool()).await.is_err());
    }
    for sql in [
        "INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES (NULL, 50, ?)",
        "INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES ('raw', 50, NULL)",
    ] {
        // The second statement has no placeholder.
        let query = sqlx::query(sql);
        let result = if sql.contains("NULL, 50") {
            query.bind(EARLIER).execute(storage.pool()).await
        } else {
            query.execute(storage.pool()).await
        };
        assert!(result.is_err());
    }
    let store = storage.model_ratings().unwrap();
    store.upsert(entry("model", 0)).await?;
    assert!(sqlx::query("INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES ('model', 100, ?)")
        .bind(EARLIER).execute(storage.pool()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn sqlite_old_ratings_table_is_dropped_on_migrate() -> anyhow::Result<()> {
    let storage = sqlite(true).await?;
    // Simulate an upgraded database that still carries the legacy table.
    sqlx::query(
        "CREATE TABLE provider_model_ratings (provider_id TEXT, upstream_model TEXT, \
         score INTEGER, updated_at TEXT, PRIMARY KEY (provider_id, upstream_model))",
    )
    .execute(storage.pool())
    .await?;
    storage.bootstrap().migrate().await?;
    let legacy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='provider_model_ratings'",
    )
    .fetch_one(storage.pool())
    .await?;
    assert_eq!(legacy, 0, "legacy ratings table must be dropped");
    assert!(storage.bootstrap().health().await?.schema_compatible);
    Ok(())
}

#[tokio::test]
async fn sqlite_missing_schema_migrates_without_backfill_and_errors_propagate() -> anyhow::Result<()>
{
    let storage = sqlite(true).await?;
    provider(&storage, "migration").await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    sqlx::query("DROP TABLE model_rating_prefixes")
        .execute(storage.pool())
        .await?;
    let health = storage.bootstrap().health().await?;
    assert!(health.can_connect);
    assert!(!health.schema_compatible);
    assert_rating_errors(&storage).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(storage.model_ratings().unwrap().list().await?.is_empty());
    let row = entry("model", 0);
    storage.model_ratings().unwrap().upsert(row.clone()).await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(storage.model_ratings().unwrap().list().await?, vec![row]);
    storage.pool().close().await;
    assert_rating_errors(&storage).await?;
    assert!(!storage.bootstrap().health().await?.can_connect);
    Ok(())
}

#[test]
fn memory_ratings_are_explicitly_unsupported() {
    assert!(MemoryStorage::new(vec![], vec![], vec![]).model_ratings().is_none());
}

fn external_test_config(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        eprintln!("skipping external rating test: {variable} is not set");
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_RATINGS_DATABASES_ONLY").as_deref() == Ok("1"),
        "external rating tests require NYRO_TEST_RATINGS_DATABASES_ONLY=1 for disposable test databases"
    );
    let parsed = reqwest::Url::parse(&url)?;
    let database = parsed.path().trim_start_matches('/');
    anyhow::ensure!(
        database.starts_with("nyro_test_") || database.ends_with("_test"),
        "external rating tests refuse non-test database names"
    );
    Ok(Some(SqlBackendConfig {
        max_connections: 1,
        ..SqlBackendConfig::with_url(url)
    }))
}

fn reference_sql(backend: &str) -> anyhow::Result<Option<String>> {
    if std::env::var("NYRO_TEST_RATINGS_PRECREATE_REFERENCE").as_deref() != Ok("1") {
        return Ok(None);
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/schema")
        .join(format!("{backend}.sql"));
    let sql = std::fs::read_to_string(path)?;
    // psql's security meta-commands are not server SQL; keep all actual DDL.
    Ok(Some(
        sql.lines()
            .filter(|line| !line.starts_with("\\restrict ") && !line.starts_with("\\unrestrict "))
            .collect::<Vec<_>>()
            .join("\n"),
    ))
}

#[tokio::test]
async fn postgres_ratings_optional_test_database() -> anyhow::Result<()> {
    let Some(config) = external_test_config("NYRO_TEST_POSTGRES_RATINGS_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    if let Some(sql) = reference_sql("postgres")? {
        let tables = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public'",
        )
        .fetch_one(storage.pool())
        .await?;
        anyhow::ensure!(
            tables == 0,
            "reference precreate requires a new empty test database"
        );
        sqlx::raw_sql(&sql).execute(storage.pool()).await?;
        // pg_dump clears search_path on its connection; restore application default.
        sqlx::query("SET search_path TO public")
            .execute(storage.pool())
            .await?;
    }
    storage.bootstrap().migrate().await?;
    exercise_sql_contract(&storage).await?;
    let row = entry("preserved-on-migrate", 0);
    storage
        .model_ratings()
        .unwrap()
        .upsert(row.clone())
        .await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(storage.model_ratings().unwrap().list().await?, vec![row]);
    sqlx::query("DROP TABLE model_rating_prefixes")
        .execute(storage.pool())
        .await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    assert_rating_errors(&storage).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(storage.model_ratings().unwrap().list().await?.is_empty());
    storage.pool().close().await;
    Ok(())
}

#[tokio::test]
async fn mysql_ratings_optional_test_database() -> anyhow::Result<()> {
    let Some(config) = external_test_config("NYRO_TEST_MYSQL_RATINGS_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    if let Some(sql) = reference_sql("mysql")? {
        let tables = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE()",
        )
        .fetch_one(storage.pool())
        .await?;
        anyhow::ensure!(
            tables == 0,
            "reference precreate requires a new empty test database"
        );
        sqlx::raw_sql(&sql).execute(storage.pool()).await?;
    }
    storage.bootstrap().migrate().await?;
    exercise_sql_contract(&storage).await?;
    let store = storage.model_ratings().unwrap();
    let row = entry("preserved-on-migrate", 0);
    store.upsert(row.clone()).await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(store.list().await?, vec![row]);
    // Invalid external bytes must surface as an error, not a lossy/absent prefix.
    sqlx::query("INSERT INTO model_rating_prefixes (model_prefix, score, updated_at) VALUES (?, 0, ?)")
        .bind(vec![0xffu8]).bind(EARLIER).execute(storage.pool()).await?;
    assert!(store.list().await.is_err());
    sqlx::query("DROP TABLE model_rating_prefixes")
        .execute(storage.pool())
        .await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    assert_rating_errors(&storage).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(store.list().await?.is_empty());
    storage.pool().close().await;
    Ok(())
}
