//! Speed-mode storage contract. Optional external tests require a NEW EMPTY
//! disposable database named nyro_test_*; never pass an application URL.
use nyro_core::db::models::{OpenAiSpeedMode, UpdateProvider};
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, SqliteStorage, Storage};
use sqlx::sqlite::SqlitePoolOptions;

async fn exercise(storage: &dyn Storage) -> anyhow::Result<()> {
    use OpenAiSpeedMode::*;
    let store = storage.providers();
    for (id, mode) in [("legacy-off", Normal), ("legacy-on", Fast)] {
        assert_eq!(store.get(id).await?.unwrap().fast_mode, mode);
    }
    let p = store
        .create(serde_json::from_value(serde_json::json!({
            "name":"new", "base_url":"", "api_key":"", "protocol":"openai-compatible",
            "fast_mode":"ultrafast"
        }))?)
        .await?;
    assert_eq!(p.fast_mode, Ultrafast);
    // Restarting and PATCHing an unrelated field must not reset the third state.
    storage.bootstrap().migrate().await?;
    assert_eq!(
        store
            .update(&p.id, UpdateProvider::default())
            .await?
            .fast_mode,
        Ultrafast
    );
    assert!(
        store
            .list()
            .await?
            .iter()
            .any(|row| row.id == p.id && row.fast_mode == Ultrafast)
    );
    for mode in [Fast, Ultrafast, Normal] {
        let update = UpdateProvider {
            fast_mode: Some(mode),
            ..Default::default()
        };
        assert_eq!(store.update(&p.id, update).await?.fast_mode, mode);
        assert_eq!(store.get(&p.id).await?.unwrap().fast_mode, mode);
    }
    Ok(())
}

// These literals are compatible with the original SQLite INTEGER, MySQL
// TINYINT and PostgreSQL BOOLEAN column types.
const SEED: &str = "INSERT INTO providers (id, name, base_url, api_key, protocol, fast_mode) VALUES
    ('legacy-off', 'old off', '', '', 'openai-compatible', FALSE),
    ('legacy-on', 'old on', '', '', 'openai-compatible', TRUE)";

#[tokio::test]
async fn sqlite_speed_mode_keeps_legacy_booleans_and_null() -> anyhow::Result<()> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    let storage = SqliteStorage::from_pool(pool);
    storage.bootstrap().migrate().await?;
    sqlx::query(SEED).execute(storage.pool()).await?;
    sqlx::query(
        "INSERT INTO providers (id, name, base_url, api_key, protocol, fast_mode) VALUES
        ('legacy-null', 'old null', '', '', 'openai-compatible', NULL)",
    )
    .execute(storage.pool())
    .await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(
        storage
            .providers()
            .get("legacy-null")
            .await?
            .unwrap()
            .fast_mode,
        OpenAiSpeedMode::Normal
    );
    exercise(&storage).await
}

fn scratch_config(var: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(var) else {
        return Ok(None);
    };
    let parsed = reqwest::Url::parse(&url)?;
    anyhow::ensure!(
        parsed.path().starts_with("/nyro_test_") && parsed.query().is_none(),
        "speed-mode tests require a dedicated nyro_test_* database URL without overrides"
    );
    Ok(Some(SqlBackendConfig::with_url(url)))
}

#[tokio::test]
async fn postgres_speed_mode_migrates_old_booleans() -> anyhow::Result<()> {
    let Some(config) = scratch_config("NYRO_TEST_POSTGRES_SPEED_MODE_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = current_schema()",
    )
    .fetch_one(storage.pool())
    .await?;
    anyhow::ensure!(tables == 0, "refusing non-empty speed-mode test database");
    storage.bootstrap().migrate().await?;
    sqlx::query(
        "ALTER TABLE providers ALTER COLUMN fast_mode DROP DEFAULT,
        ALTER COLUMN fast_mode TYPE BOOLEAN USING fast_mode::boolean,
        ALTER COLUMN fast_mode SET DEFAULT FALSE",
    )
    .execute(storage.pool())
    .await?;
    sqlx::query(SEED).execute(storage.pool()).await?;
    storage.bootstrap().migrate().await?;
    exercise(&storage).await
}

#[tokio::test]
async fn mysql_speed_mode_migrates_old_tinyints() -> anyhow::Result<()> {
    let Some(config) = scratch_config("NYRO_TEST_MYSQL_SPEED_MODE_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE()",
    )
    .fetch_one(storage.pool())
    .await?;
    anyhow::ensure!(tables == 0, "refusing non-empty speed-mode test database");
    storage.bootstrap().migrate().await?;
    sqlx::query("ALTER TABLE providers MODIFY COLUMN fast_mode TINYINT(1) NOT NULL DEFAULT 0")
        .execute(storage.pool())
        .await?;
    sqlx::query(SEED).execute(storage.pool()).await?;
    storage.bootstrap().migrate().await?;
    exercise(&storage).await
}
