//! Public storage contract coverage. External backends are opt-in and may only
//! target disposable test databases: set NYRO_TEST_RATINGS_DATABASES_ONLY=1 plus
//! NYRO_TEST_POSTGRES_RATINGS_URL / NYRO_TEST_MYSQL_RATINGS_URL. Their database
//! names must start with `nyro_test_` or end with `_test`; these tests migrate the
//! schema and drop/recreate the ratings table. No default/deployment URL is used.
//! Set NYRO_TEST_RATINGS_PRECREATE_REFERENCE=1 with NEW EMPTY test databases to
//! additionally load the generated reference SQL before running migration tests.

use nyro_core::db::models::{
    CreateModel, CreateProvider, Provider, ProviderModelRating, UpdateProvider,
};
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MemoryStorage, MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const EARLIER: &str = "2024-02-03T04:05:06.007Z";
const LATER: &str = "2025-06-07T08:09:10.011Z";

fn rating(provider_id: &str, model: &str, score: i32) -> ProviderModelRating {
    ProviderModelRating {
        provider_id: provider_id.to_owned(),
        upstream_model: model.to_owned(),
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
    let first = provider(storage, "first").await?;
    let second = provider(storage, "second").await?;
    storage
        .providers()
        .update(
            &second.id,
            UpdateProvider {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    storage
        .models()
        .create(serde_json::from_value::<CreateModel>(serde_json::json!({
            "name": format!("rating-route-{}", uuid::Uuid::new_v4()),
            "target_provider": first.id,
            "target_model": "model"
        }))?)
        .await?;
    let snapshot = serde_json::to_value(storage.snapshots().load_active_snapshot().await?)?;
    let store = storage
        .provider_model_ratings()
        .expect("SQL rating capability");

    assert!(store.list(Some(&first.id)).await?.is_empty());
    assert_eq!(store.get(&first.id, "model").await?, None);
    let zero = rating(&first.id, "model", 0);
    assert_eq!(store.upsert(zero.clone()).await?, zero);
    assert_eq!(store.get(&first.id, "model").await?, Some(zero));
    assert_eq!(store.get(&second.id, "model").await?, None);
    let hundred = rating(&second.id, "model", 100);
    assert_eq!(store.upsert(hundred.clone()).await?, hundred);

    for (index, model) in [
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
        let row = rating(&first.id, model, (index + 1) as i32);
        assert_eq!(store.upsert(row.clone()).await?, row);
        assert_eq!(store.get(&first.id, model).await?, Some(row));
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
    ];
    expected_keys.sort_unstable();
    assert_eq!(
        store
            .list(Some(&first.id))
            .await?
            .iter()
            .map(|r| r.upstream_model.as_str())
            .collect::<Vec<_>>(),
        expected_keys
    );
    assert_eq!(store.list(Some(&second.id)).await?, vec![hundred.clone()]);
    assert!(store.list(None).await?.contains(&hundred));

    let changed = ProviderModelRating {
        score: 100,
        updated_at: LATER.to_owned(),
        ..rating(&first.id, "model", 0)
    };
    assert_eq!(store.upsert(changed.clone()).await?, changed);
    assert_eq!(
        store.list(Some(&first.id)).await?.len(),
        expected_keys.len()
    );
    for score in [-1, 101, i32::MAX] {
        assert!(
            store
                .upsert(rating(&first.id, "model", score))
                .await
                .is_err()
        );
        assert_eq!(store.get(&first.id, "model").await?, Some(changed.clone()));
    }
    for model in ["x".repeat(1024), "🦀".repeat(256)] {
        let row = rating(&first.id, &model, 50);
        assert_eq!(store.upsert(row.clone()).await?, row);
        assert_eq!(store.get(&first.id, &model).await?, Some(row));
    }
    for model in [String::new(), "x".repeat(1025), "🦀".repeat(257)] {
        assert!(store.upsert(rating(&first.id, &model, 50)).await.is_err());
    }
    let missing = uuid::Uuid::new_v4().to_string();
    assert!(store.upsert(rating(&missing, "model", 50)).await.is_err());
    assert!(
        store
            .restore(&missing, &[rating(&first.id, "model", 50)])
            .await
            .is_err()
    );
    assert_eq!(store.get(&missing, "model").await?, None);
    assert!(store.list(Some(&missing)).await?.is_empty());

    store.delete(&first.id, "Model").await?;
    store.delete(&first.id, "Model").await?;
    assert_eq!(store.get(&first.id, "Model").await?, None);
    assert_eq!(store.get(&first.id, "model").await?, Some(changed.clone()));
    assert!(store.get(&first.id, "model ").await?.is_some());
    assert_eq!(store.get(&second.id, "model").await?, Some(hundred.clone()));

    let restored = [
        rating(&missing, "restored", 0),
        ProviderModelRating {
            updated_at: LATER.to_owned(),
            ..rating(&first.id, "restored ", 100)
        },
    ];
    store.restore(&second.id, &restored).await?;
    let expected: Vec<_> = restored
        .iter()
        .map(|row| ProviderModelRating {
            provider_id: second.id.clone(),
            ..row.clone()
        })
        .collect();
    assert_eq!(store.list(Some(&second.id)).await?, expected);
    assert_eq!(store.get(&second.id, "model").await?, None);
    assert_eq!(store.get(&first.id, "model").await?, Some(changed));

    // Failure after a deletion and valid insertion must roll back the whole replace.
    assert!(
        store
            .restore(
                &second.id,
                &[
                    rating(&first.id, "partial", 25),
                    rating(&first.id, "invalid", 101)
                ]
            )
            .await
            .is_err()
    );
    assert_eq!(store.list(Some(&second.id)).await?, expected);
    assert!(
        store
            .restore(
                &second.id,
                &[
                    rating(&first.id, "partial", 25),
                    rating(&first.id, &"🦀".repeat(257), 25)
                ]
            )
            .await
            .is_err()
    );
    assert_eq!(store.list(Some(&second.id)).await?, expected);
    store.restore(&second.id, &[]).await?;
    assert!(store.list(Some(&second.id)).await?.is_empty());
    store.upsert(hundred.clone()).await?;
    assert_eq!(
        serde_json::to_value(storage.snapshots().load_active_snapshot().await?)?,
        snapshot
    );

    storage.providers().delete(&first.id).await?;
    assert!(store.list(Some(&first.id)).await?.is_empty());
    assert_eq!(store.get(&second.id, "model").await?, Some(hundred));
    storage.providers().delete(&second.id).await?;
    assert!(store.list(Some(&second.id)).await?.is_empty());
    Ok(())
}

async fn assert_rating_errors(storage: &dyn Storage, id: &str) -> anyhow::Result<()> {
    let store = storage.provider_model_ratings().unwrap();
    assert!(store.list(None).await.is_err());
    assert!(store.list(Some(id)).await.is_err());
    assert!(store.get(id, "model").await.is_err());
    assert!(store.upsert(rating(id, "model", 50)).await.is_err());
    assert!(store.delete(id, "model").await.is_err());
    assert!(store.restore(id, &[]).await.is_err());
    assert!(store.restore(id, &[rating(id, "model", 50)]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn sqlite_rating_contract() -> anyhow::Result<()> {
    exercise_sql_contract(&sqlite(true).await?).await
}

#[tokio::test]
async fn sqlite_constraints_and_foreign_key_cascade() -> anyhow::Result<()> {
    let storage = sqlite(true).await?;
    let provider = provider(&storage, "constraints").await?;
    for score in ["NULL", "-1", "101", "0.5", "'not-an-integer'"] {
        let sql = format!(
            "INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, 'raw', {score}, ?)"
        );
        assert!(
            sqlx::query(&sql)
                .bind(&provider.id)
                .bind(EARLIER)
                .execute(storage.pool())
                .await
                .is_err()
        );
    }
    for model in [String::new(), "x".repeat(1025), "🦀".repeat(257)] {
        assert!(sqlx::query("INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, ?, 50, ?)")
            .bind(&provider.id).bind(model).bind(EARLIER).execute(storage.pool()).await.is_err());
    }
    for sql in [
        "INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, NULL, 50, ?)",
        "INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, 'raw', 50, NULL)",
    ] {
        // The second statement only has one placeholder.
        let query = sqlx::query(sql).bind(&provider.id);
        let result = if sql.contains("NULL, 50") {
            query.bind(EARLIER).execute(storage.pool()).await
        } else {
            query.execute(storage.pool()).await
        };
        assert!(result.is_err());
    }
    let store = storage.provider_model_ratings().unwrap();
    store.upsert(rating(&provider.id, "model", 0)).await?;
    assert!(sqlx::query("INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, 'model', 100, ?)")
        .bind(&provider.id).bind(EARLIER).execute(storage.pool()).await.is_err());
    sqlx::query("DELETE FROM providers WHERE id = ?")
        .bind(&provider.id)
        .execute(storage.pool())
        .await?;
    assert!(store.list(Some(&provider.id)).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn sqlite_provider_cleanup_is_atomic_even_with_foreign_keys_off() -> anyhow::Result<()> {
    let storage = sqlite(false).await?;
    let first = provider(&storage, "cleanup").await?;
    let second = provider(&storage, "untouched").await?;
    let store = storage.provider_model_ratings().unwrap();
    let row = rating(&first.id, "model", 0);
    store.upsert(row.clone()).await?;
    store.upsert(rating(&second.id, "model", 100)).await?;
    sqlx::raw_sql("CREATE TRIGGER refuse_provider_delete BEFORE DELETE ON providers BEGIN SELECT RAISE(ABORT, 'test deletion failure'); END;")
        .execute(storage.pool()).await?;
    assert!(storage.providers().delete(&first.id).await.is_err());
    assert_eq!(store.get(&first.id, "model").await?, Some(row));
    assert!(storage.providers().get(&first.id).await?.is_some());
    sqlx::query("DROP TRIGGER refuse_provider_delete")
        .execute(storage.pool())
        .await?;
    storage.providers().delete(&first.id).await?;
    assert!(store.list(Some(&first.id)).await?.is_empty());
    assert_eq!(store.list(Some(&second.id)).await?.len(), 1);
    storage.providers().delete(&second.id).await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_missing_schema_migrates_without_backfill_and_errors_propagate() -> anyhow::Result<()>
{
    let storage = sqlite(true).await?;
    let provider = provider(&storage, "migration").await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    sqlx::query("DROP TABLE provider_model_ratings")
        .execute(storage.pool())
        .await?;
    let health = storage.bootstrap().health().await?;
    assert!(health.can_connect);
    assert!(!health.schema_compatible);
    assert_rating_errors(&storage, &provider.id).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(
        storage
            .provider_model_ratings()
            .unwrap()
            .list(None)
            .await?
            .is_empty()
    );
    assert!(storage.providers().get(&provider.id).await?.is_some());
    let row = rating(&provider.id, "model", 0);
    storage
        .provider_model_ratings()
        .unwrap()
        .upsert(row.clone())
        .await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(
        storage
            .provider_model_ratings()
            .unwrap()
            .list(Some(&provider.id))
            .await?,
        vec![row]
    );
    storage.pool().close().await;
    assert_rating_errors(&storage, &provider.id).await?;
    assert!(!storage.bootstrap().health().await?.can_connect);
    Ok(())
}

#[test]
fn memory_ratings_are_explicitly_unsupported() {
    assert!(
        MemoryStorage::new(vec![], vec![], vec![])
            .provider_model_ratings()
            .is_none()
    );
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
    let provider = provider(&storage, "migration").await?;
    let row = rating(&provider.id, "preserved-on-migrate", 0);
    storage
        .provider_model_ratings()
        .unwrap()
        .upsert(row.clone())
        .await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(
        storage
            .provider_model_ratings()
            .unwrap()
            .list(Some(&provider.id))
            .await?,
        vec![row]
    );
    sqlx::query("DROP TABLE provider_model_ratings")
        .execute(storage.pool())
        .await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    assert_rating_errors(&storage, &provider.id).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(
        storage
            .provider_model_ratings()
            .unwrap()
            .list(None)
            .await?
            .is_empty()
    );
    storage.providers().delete(&provider.id).await?;
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
    let provider = provider(&storage, "migration").await?;
    let store = storage.provider_model_ratings().unwrap();
    let row = rating(&provider.id, "preserved-on-migrate", 0);
    store.upsert(row.clone()).await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(store.list(Some(&provider.id)).await?, vec![row]);
    // Invalid external bytes must surface as an error, not a lossy/absent model key.
    sqlx::query("INSERT INTO provider_model_ratings (provider_id, upstream_model, score, updated_at) VALUES (?, ?, 0, ?)")
        .bind(&provider.id).bind(vec![0xffu8]).bind(EARLIER).execute(storage.pool()).await?;
    assert!(store.list(Some(&provider.id)).await.is_err());
    assert!(store.list(None).await.is_err());
    sqlx::query("DROP TABLE provider_model_ratings")
        .execute(storage.pool())
        .await?;
    assert!(!storage.bootstrap().health().await?.schema_compatible);
    assert_rating_errors(&storage, &provider.id).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.bootstrap().health().await?.schema_compatible);
    assert!(store.list(None).await?.is_empty());
    storage.providers().delete(&provider.id).await?;
    storage.pool().close().await;
    Ok(())
}
