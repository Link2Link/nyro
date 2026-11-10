//! Settings CAS contract and concurrency coverage. External SQL tests are opt-in:
//! set NYRO_TEST_SETTINGS_DATABASES_ONLY=1 and NYRO_TEST_POSTGRES_SETTINGS_URL or
//! NYRO_TEST_MYSQL_SETTINGS_URL to a disposable database whose name starts with
//! nyro_test_ or ends with _test. They run migrations; no deployment URL is used.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{
    MemoryStorage, MysqlStorage, PostgresStorage, SettingsStore, SqliteStorage, Storage,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tokio::sync::Barrier;
use tokio::task::JoinSet;

async fn exercise_contract(settings: &dyn SettingsStore, key: &str) -> anyhow::Result<()> {
    assert_eq!(settings.get(key).await?, None);
    assert!(
        !settings
            .compare_and_set(key, Some("missing"), "new")
            .await?
    );
    assert_eq!(settings.get(key).await?, None);
    assert!(settings.compare_and_set(key, None, "Lease").await?);
    assert!(!settings.compare_and_set(key, None, "replacement").await?);
    assert!(!settings.compare_and_set(key, None, "Lease").await?);

    // Case and trailing whitespace are significant, even on MySQL's default collation.
    for mismatch in ["wrong", "lease", "Lease ", " Lease", "Léase"] {
        assert!(!settings.compare_and_set(key, Some(mismatch), "new").await?);
        assert_eq!(settings.get(key).await?.as_deref(), Some("Lease"));
    }
    assert!(settings.compare_and_set(key, Some("Lease"), "next").await?);
    assert_eq!(settings.get(key).await?.as_deref(), Some("next"));
    assert!(
        !settings
            .compare_and_set(key, Some("Lease"), "stale")
            .await?
    );
    assert!(settings.compare_and_set(key, Some("next"), "next").await?);

    // An empty string is an existing value, not an absent key.
    assert!(settings.compare_and_set(key, Some("next"), "").await?);
    assert!(!settings.compare_and_set(key, None, "replacement").await?);
    assert!(settings.compare_and_set(key, Some(""), "restored").await?);
    assert_eq!(settings.get(key).await?.as_deref(), Some("restored"));
    Ok(())
}

async fn race(
    replicas: &[Arc<dyn Storage>],
    key: &str,
    expected: Option<&str>,
) -> anyhow::Result<String> {
    const WORKERS: usize = 16;
    let barrier = Arc::new(Barrier::new(WORKERS));
    let mut tasks = JoinSet::new();
    // Each round must change the value, even if the same worker wins both rounds.
    let round = uuid::Uuid::new_v4();
    for worker in 0..WORKERS {
        let storage = replicas[worker % replicas.len()].clone();
        let barrier = barrier.clone();
        let key = key.to_owned();
        let expected = expected.map(str::to_owned);
        let value = format!("{round}-{worker}");
        tasks.spawn(async move {
            barrier.wait().await;
            let won = storage
                .settings()
                .compare_and_set(&key, expected.as_deref(), &value)
                .await?;
            Ok::<_, anyhow::Error>((won, value))
        });
    }
    let mut winners = Vec::new();
    while let Some(result) = tasks.join_next().await {
        let (won, value) = result??;
        if won {
            winners.push(value);
        }
    }
    assert_eq!(winners.len(), 1, "exactly one CAS contender must win");
    let winner = winners.pop().unwrap();
    for storage in replicas {
        assert_eq!(
            storage.settings().get(key).await?.as_deref(),
            Some(winner.as_str())
        );
    }
    Ok(winner)
}

struct UnsupportedSettings;

#[async_trait]
impl SettingsStore for UnsupportedSettings {
    async fn get(&self, _key: &str) -> anyhow::Result<Option<String>> {
        panic!("default CAS must not emulate a read-modify-write")
    }

    async fn set(&self, _key: &str, _value: &str) -> anyhow::Result<()> {
        panic!("default CAS must not emulate a read-modify-write")
    }

    async fn list_all(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn unsupported_settings_cas_fails_closed() {
    assert!(
        UnsupportedSettings
            .compare_and_set("key", None, "new")
            .await
            .is_err()
    );
    assert!(
        UnsupportedSettings
            .compare_and_set("key", Some("old"), "new")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_settings_cas_contract_and_single_winner() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
    exercise_contract(storage.settings(), "contract").await?;
    let replicas: Vec<Arc<dyn Storage>> = vec![Arc::new(storage.clone()), Arc::new(storage)];
    let inserted = race(&replicas, "race", None).await?;
    race(&replicas, "race", Some(&inserted)).await?;
    Ok(())
}

async fn sqlite(options: SqliteConnectOptions) -> anyhow::Result<SqliteStorage> {
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    Ok(SqliteStorage::from_pool(pool))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_settings_cas_across_pools_and_reopen() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let options = SqliteConnectOptions::new()
        .filename(directory.path().join("settings.sqlite"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10));
    let first = sqlite(options.clone()).await?;
    first.bootstrap().migrate().await?;
    exercise_contract(first.settings(), "contract").await?;
    // Separate pools model independent replicas, not an application-local mutex.
    let second = sqlite(options.clone()).await?;
    let replicas: Vec<Arc<dyn Storage>> = vec![Arc::new(first.clone()), Arc::new(second.clone())];
    let inserted = race(&replicas, "race", None).await?;
    let updated = race(&replicas, "race", Some(&inserted)).await?;
    first.pool().close().await;
    second.pool().close().await;

    // Storage failures must remain errors, not look like ordinary CAS misses.
    assert!(
        first
            .settings()
            .compare_and_set("closed", None, "new")
            .await
            .is_err()
    );
    assert!(
        first
            .settings()
            .compare_and_set("race", Some(&updated), "new")
            .await
            .is_err()
    );

    let reopened = sqlite(options).await?;
    assert_eq!(
        reopened.settings().get("race").await?,
        Some(updated.clone())
    );
    assert!(
        !reopened
            .settings()
            .compare_and_set("race", None, "new")
            .await?
    );
    assert!(
        !reopened
            .settings()
            .compare_and_set("race", Some(&inserted), "new")
            .await?
    );
    assert!(
        reopened
            .settings()
            .compare_and_set("race", Some(&updated), "new")
            .await?
    );
    reopened.pool().close().await;
    Ok(())
}

fn external_config(variable: &str) -> anyhow::Result<Option<SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_SETTINGS_DATABASES_ONLY").as_deref() == Ok("1"),
        "explicit disposable settings database consent required"
    );
    let parsed = reqwest::Url::parse(&url)?;
    let database = parsed.path().trim_start_matches('/');
    anyhow::ensure!(
        database.starts_with("nyro_test_") || database.ends_with("_test"),
        "refusing non-test database"
    );
    Ok(Some(SqlBackendConfig {
        max_connections: 4,
        ..SqlBackendConfig::with_url(url)
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_settings_cas_optional() -> anyhow::Result<()> {
    let Some(config) = external_config("NYRO_TEST_POSTGRES_SETTINGS_URL")? else {
        return Ok(());
    };
    let first = PostgresStorage::connect(config.clone()).await?;
    first.bootstrap().migrate().await?;
    let second = PostgresStorage::connect(config).await?;
    let contract_key = format!("cas-contract-{}", uuid::Uuid::new_v4());
    let race_key = format!("cas-race-{}", uuid::Uuid::new_v4());
    exercise_contract(first.settings(), &contract_key).await?;
    let replicas: Vec<Arc<dyn Storage>> = vec![Arc::new(first.clone()), Arc::new(second.clone())];
    let inserted = race(&replicas, &race_key, None).await?;
    race(&replicas, &race_key, Some(&inserted)).await?;
    sqlx::query("DELETE FROM settings WHERE name IN ($1, $2)")
        .bind(&contract_key)
        .bind(&race_key)
        .execute(first.pool())
        .await?;
    first.pool().close().await;
    second.pool().close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_settings_cas_optional() -> anyhow::Result<()> {
    let Some(config) = external_config("NYRO_TEST_MYSQL_SETTINGS_URL")? else {
        return Ok(());
    };
    let first = MysqlStorage::connect(config.clone()).await?;
    first.bootstrap().migrate().await?;
    let second = MysqlStorage::connect(config).await?;
    let contract_key = format!("cas-contract-{}", uuid::Uuid::new_v4());
    let race_key = format!("cas-race-{}", uuid::Uuid::new_v4());
    exercise_contract(first.settings(), &contract_key).await?;
    let replicas: Vec<Arc<dyn Storage>> = vec![Arc::new(first.clone()), Arc::new(second.clone())];
    let inserted = race(&replicas, &race_key, None).await?;
    race(&replicas, &race_key, Some(&inserted)).await?;
    sqlx::query("DELETE FROM settings WHERE name IN (?, ?)")
        .bind(&contract_key)
        .bind(&race_key)
        .execute(first.pool())
        .await?;
    first.pool().close().await;
    second.pool().close().await;
    Ok(())
}
