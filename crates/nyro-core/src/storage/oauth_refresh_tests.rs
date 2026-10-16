use super::traits::OAuthCredentialStore;
use super::*;
use crate::db::models::{CreateProvider, UpsertOAuthCredential};
use std::time::Duration;

fn credential(access_token: &str) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: "codex".to_string(),
        scheme: "oauth_auth_code_pkce".to_string(),
        access_token: access_token.to_string(),
        refresh_token: Some("refresh-token".to_string()),
        expires_at: Some("2099-01-01T00:00:00Z".to_string()),
        resource_url: Some("https://chatgpt.com/backend-api/codex".to_string()),
        subject_id: Some("account".to_string()),
        scopes: Some("[]".to_string()),
        meta: Some("{}".to_string()),
    }
}

#[tokio::test]
async fn failed_refresh_remains_retryable() -> anyhow::Result<()> {
    let store = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
    let oauth = store.oauth_credentials();
    let initial = oauth.upsert("provider", credential("old-token")).await?;
    let lock = oauth
        .try_begin_refresh("provider", initial.status_version)
        .await?
        .expect("refresh lock");

    assert!(
        oauth
            .fail_refresh("provider", lock.status_version, "temporary failure")
            .await?
    );
    let failed = oauth.get("provider").await?.expect("credential");
    assert_eq!(failed.status, "connected");
    assert_eq!(failed.last_error.as_deref(), Some("temporary failure"));
    assert!(
        oauth
            .try_begin_refresh("provider", failed.status_version)
            .await?
            .is_some(),
        "a transient failure must not strand future refresh attempts"
    );
    Ok(())
}

#[tokio::test]
async fn stale_refresh_cannot_overwrite_a_new_bind() -> anyhow::Result<()> {
    let store = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
    let oauth = store.oauth_credentials();
    let initial = oauth.upsert("provider", credential("old-token")).await?;
    let lock = oauth
        .try_begin_refresh("provider", initial.status_version)
        .await?
        .expect("refresh lock");

    oauth
        .upsert("provider", credential("new-bind-token"))
        .await?;
    assert!(
        oauth
            .complete_refresh(
                "provider",
                lock.status_version,
                credential("stale-refresh-token"),
            )
            .await?
            .is_none()
    );
    assert_eq!(
        oauth
            .get("provider")
            .await?
            .expect("credential")
            .access_token,
        "new-bind-token"
    );
    Ok(())
}

fn cursor_credential(token: &str, subject: &str) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: "cursor".to_string(),
        subject_id: Some(subject.to_string()),
        ..credential(token)
    }
}

async fn exercise_refresh_fences(
    oauth: &dyn OAuthCredentialStore,
    provider: &str,
) -> anyhow::Result<()> {
    for driver in ["codex", "cursor"] {
        let input = |token: &str| UpsertOAuthCredential {
            driver_key: driver.to_string(),
            ..credential(token)
        };
        let initial = oauth.upsert(provider, input("old")).await?;
        let lock = oauth
            .try_begin_refresh(provider, initial.status_version)
            .await?
            .expect("refresh lock");
        assert_eq!(lock.status, "refreshing");
        assert_eq!(lock.access_token, "old");
        assert_eq!(lock.status_version, initial.status_version + 1);
        assert!(
            oauth
                .try_begin_refresh(provider, initial.status_version)
                .await?
                .is_none()
        );

        let rebound = oauth.upsert(provider, input("new-bind")).await?;
        // The new bind has exactly expected_version+1. Merely rereading that
        // version after a no-op UPDATE must not report a refresh success.
        assert_eq!(rebound.status_version, lock.status_version + 1);
        assert!(
            oauth
                .complete_refresh(provider, lock.status_version, input("late"))
                .await?
                .is_none()
        );
        assert!(
            !oauth
                .fail_refresh_with_policy(
                    provider,
                    lock.status_version,
                    "late",
                    true,
                    Some("{}".to_string())
                )
                .await?
        );
        assert_eq!(oauth.get(provider).await?.unwrap().access_token, "new-bind");

        let old_lock = oauth
            .try_begin_refresh(provider, rebound.status_version)
            .await?
            .unwrap();
        oauth.delete(provider).await?;
        assert!(oauth.get(provider).await?.is_none());
        assert!(
            oauth
                .complete_refresh(provider, old_lock.status_version, input("late"))
                .await?
                .is_none()
        );
        assert!(
            !oauth
                .fail_refresh(provider, old_lock.status_version, "late")
                .await?
        );

        let rebound = oauth.upsert(provider, input("post-logout")).await?;
        assert!(rebound.status_version > old_lock.status_version);
        assert!(
            oauth
                .try_begin_refresh(provider, initial.status_version)
                .await?
                .is_none()
        );
        let new_lock = oauth
            .try_begin_refresh(provider, rebound.status_version)
            .await?
            .unwrap();
        assert!(new_lock.status_version > old_lock.status_version);
        // A physical delete would reset the new lock to version 1, colliding
        // with the first refresh attempt (even though a later lock was 3).
        assert!(
            oauth
                .complete_refresh(provider, lock.status_version, input("first-generation-ABA"))
                .await?
                .is_none()
        );
        assert!(
            !oauth
                .fail_refresh(provider, lock.status_version, "first-generation-ABA")
                .await?
        );
        assert!(
            oauth
                .complete_refresh(provider, old_lock.status_version, input("ABA"))
                .await?
                .is_none()
        );
        assert!(
            !oauth
                .fail_refresh(provider, old_lock.status_version, "ABA")
                .await?
        );
        let completed = oauth
            .complete_refresh(provider, new_lock.status_version, input("fresh"))
            .await?
            .unwrap();
        assert_eq!(completed.access_token, "fresh");
        assert_eq!(completed.status, "connected");
        assert_eq!(completed.status_version, new_lock.status_version + 1);
        assert!(
            oauth
                .complete_refresh(
                    provider,
                    new_lock.status_version,
                    input("duplicate-completion")
                )
                .await?
                .is_none()
        );
        oauth.delete(provider).await?;
    }
    Ok(())
}

async fn exercise_failure_policy(
    oauth: &dyn OAuthCredentialStore,
    provider: &str,
) -> anyhow::Result<()> {
    let initial = oauth
        .upsert(provider, cursor_credential("old", "policy-account"))
        .await?;
    let lock = oauth
        .try_begin_refresh(provider, initial.status_version)
        .await?
        .unwrap();
    let retry_meta = r#"{"cursor_refresh_retry_at":2000000000,"cursor_refresh_failures":1}"#;
    assert!(
        oauth
            .fail_refresh_with_policy(
                provider,
                lock.status_version,
                "temporarily unavailable",
                false,
                Some(retry_meta.to_string())
            )
            .await?
    );
    let retry = oauth.get(provider).await?.unwrap();
    assert_eq!(retry.status, "connected");
    assert_eq!(retry.meta, retry_meta);
    assert_eq!(retry.access_token, "old");
    assert!(
        !oauth
            .fail_refresh_with_policy(
                provider,
                lock.status_version,
                "late",
                true,
                Some("{}".to_string())
            )
            .await?
    );
    let lock = oauth
        .try_begin_refresh(provider, retry.status_version)
        .await?
        .unwrap();
    // Existing API stays transient for all old providers; None retains metadata.
    assert!(
        oauth
            .fail_refresh(provider, lock.status_version, "legacy failure")
            .await?
    );
    let retry = oauth.get(provider).await?.unwrap();
    assert_eq!(retry.status, "connected");
    assert_eq!(retry.meta, retry_meta);
    let lock = oauth
        .try_begin_refresh(provider, retry.status_version)
        .await?
        .unwrap();
    let mut refreshed = cursor_credential("fresh", "policy-account");
    refreshed.meta = None;
    refreshed.scopes = None;
    let refreshed = oauth
        .complete_refresh(provider, lock.status_version, refreshed)
        .await?
        .unwrap();
    assert_eq!(refreshed.meta, retry_meta);
    let lock = oauth
        .try_begin_refresh(provider, refreshed.status_version)
        .await?
        .unwrap();
    assert!(
        oauth
            .fail_refresh_with_policy(provider, lock.status_version, "reauth_required", true, None)
            .await?
    );
    let terminal = oauth.get(provider).await?.unwrap();
    assert_eq!(terminal.status, "error");
    assert_eq!(terminal.meta, retry_meta);
    assert!(
        oauth
            .try_begin_refresh(provider, terminal.status_version)
            .await?
            .is_none()
    );
    oauth.delete(provider).await?;
    Ok(())
}

async fn exercise_concurrent_snapshots(
    oauth: &dyn OAuthCredentialStore,
    provider: &str,
) -> anyhow::Result<()> {
    for _ in 0..16 {
        let initial = oauth.upsert(provider, credential("initial")).await?;
        let (begun, bound) = tokio::join!(
            oauth.try_begin_refresh(provider, initial.status_version),
            oauth.upsert(provider, credential("concurrent-bind")),
        );
        bound?;
        if let Some(snapshot) = begun? {
            assert_eq!(snapshot.status, "refreshing");
            assert_eq!(snapshot.status_version, initial.status_version + 1);
            assert_eq!(snapshot.access_token, "initial");
        }
        let current = oauth
            .upsert(provider, credential("before-completion"))
            .await?;
        let lock = oauth
            .try_begin_refresh(provider, current.status_version)
            .await?
            .unwrap();
        let (completed, bound) = tokio::join!(
            oauth.complete_refresh(provider, lock.status_version, credential("refreshed")),
            oauth.upsert(provider, credential("new-bind")),
        );
        bound?;
        if let Some(snapshot) = completed? {
            assert_eq!(snapshot.access_token, "refreshed");
            assert_eq!(snapshot.status_version, lock.status_version + 1);
        }
        assert_eq!(oauth.get(provider).await?.unwrap().access_token, "new-bind");
        let current = oauth.get(provider).await?.unwrap();
        let lock = oauth
            .try_begin_refresh(provider, current.status_version)
            .await?
            .unwrap();
        let (completed, deleted) = tokio::join!(
            oauth.complete_refresh(provider, lock.status_version, credential("before-logout")),
            oauth.delete(provider),
        );
        completed?;
        deleted?;
        assert!(oauth.get(provider).await?.is_none());
    }
    Ok(())
}

async fn exercise_expiry_filter(
    oauth: &dyn OAuthCredentialStore,
    provider: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    for (expiry, expected) in [
        (None, false),
        (Some((now - chrono::Duration::hours(1)).to_rfc3339()), true),
        (
            Some((now + chrono::Duration::seconds(90)).to_rfc3339()),
            true,
        ),
        (
            Some(
                (now + chrono::Duration::hours(1))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
            false,
        ),
    ] {
        let initial = oauth
            .upsert(
                provider,
                UpsertOAuthCredential {
                    expires_at: expiry,
                    ..credential("expiry-test")
                },
            )
            .await?;
        assert_eq!(
            oauth
                .list_expiring(Duration::from_secs(120))
                .await?
                .iter()
                .any(|row| row.provider_id == provider),
            expected
        );
        let lock = oauth
            .try_begin_refresh(provider, initial.status_version)
            .await?
            .unwrap();
        assert!(
            !oauth
                .list_expiring(Duration::from_secs(7200))
                .await?
                .iter()
                .any(|row| row.provider_id == provider)
        );
        // A fresh in-flight refresh must never be stolen by the recovery sweep.
        assert_eq!(
            oauth
                .recover_stale_refreshing(Duration::from_secs(60))
                .await?,
            0
        );
        assert_eq!(
            oauth.get(provider).await?.unwrap().status_version,
            lock.status_version
        );
        oauth.delete(provider).await?;
    }
    Ok(())
}

async fn exercise_bind_compensation(
    oauth: &dyn OAuthCredentialStore,
    first: &str,
    second: &str,
) -> anyhow::Result<()> {
    let previous = cursor_credential("previous", "rollback-account");
    let published_input = cursor_credential("published", "published-account");
    let published = oauth.upsert(first, published_input.clone()).await?;
    oauth.delete(first).await?;
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, None)
            .await?
    );
    assert!(
        oauth.get(first).await?.is_none(),
        "rollback must not resurrect logout"
    );

    let published = oauth.upsert(first, published_input.clone()).await?;
    let newer = oauth
        .upsert(first, cursor_credential("new-bind", "new-account"))
        .await?;
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, None)
            .await?
    );
    assert_eq!(oauth.get(first).await?.unwrap().access_token, "new-bind");
    assert_eq!(
        oauth.get(first).await?.unwrap().status_version,
        newer.status_version
    );

    let published = oauth.upsert(first, published_input.clone()).await?;
    assert!(
        oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    let restored = oauth.get(first).await?.unwrap();
    assert_eq!(restored.access_token, "previous");
    assert_eq!(restored.status_version, published.status_version + 1);
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    assert!(
        oauth
            .restore_if_version(first, restored.status_version, None)
            .await?
    );
    assert!(oauth.get(first).await?.is_none());
    assert!(
        !oauth
            .restore_if_version(first, restored.status_version, None)
            .await?
    );

    // Subjects are not globally unique. Compensation and a second bind may
    // both succeed; version fencing is what prevents a stale rollback.
    let published = oauth.upsert(first, published_input.clone()).await?;
    oauth.upsert(second, previous.clone()).await?;
    assert!(
        oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    oauth.delete(first).await?;
    oauth.delete(second).await?;

    // Refresh ownership changes also fence bind compensation.
    let published = oauth.upsert(first, published_input).await?;
    let lock = oauth
        .try_begin_refresh(first, published.status_version)
        .await?
        .unwrap();
    assert!(
        !oauth
            .restore_if_version(first, published.status_version, Some(previous.clone()))
            .await?
    );
    assert!(
        !oauth
            .restore_if_version(first, lock.status_version, Some(previous))
            .await?
    );
    assert!(
        !oauth
            .restore_if_version(first, lock.status_version, None)
            .await?
    );
    oauth.delete(first).await?;
    Ok(())
}

async fn exercise_contract(
    oauth: &dyn OAuthCredentialStore,
    first: &str,
    second: &str,
) -> anyhow::Result<()> {
    exercise_refresh_fences(oauth, first).await?;
    exercise_bind_compensation(oauth, first, second).await?;
    exercise_failure_policy(oauth, first).await?;
    exercise_concurrent_snapshots(oauth, first).await?;
    exercise_expiry_filter(oauth, first).await?;
    Ok(())
}

#[tokio::test]
async fn memory_oauth_contract() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(vec![], vec![], vec![]);
    exercise_contract(storage.oauth_credentials(), "first", "second").await
}

async fn prepare_stale_rows(
    oauth: &dyn OAuthCredentialStore,
    cursor: &str,
    legacy: &str,
) -> anyhow::Result<()> {
    let initial = oauth
        .upsert(cursor, cursor_credential("stale", "stale-account"))
        .await?;
    oauth
        .try_begin_refresh(cursor, initial.status_version)
        .await?
        .unwrap();
    let initial = oauth.upsert(legacy, credential("stale")).await?;
    oauth
        .try_begin_refresh(legacy, initial.status_version)
        .await?
        .unwrap();
    Ok(())
}

async fn assert_stale_recovery(
    oauth: &dyn OAuthCredentialStore,
    cursor: &str,
    legacy: &str,
) -> anyhow::Result<()> {
    let lock = oauth.get(cursor).await?.unwrap();
    assert_eq!(
        oauth
            .recover_stale_refreshing(Duration::from_secs(60))
            .await?,
        2
    );
    assert_eq!(
        oauth
            .recover_stale_refreshing(Duration::from_secs(60))
            .await?,
        0
    );
    let cursor_row = oauth.get(cursor).await?.unwrap();
    assert_eq!(cursor_row.status, "connected");
    assert_eq!(
        cursor_row.last_error.as_deref(),
        Some(oauth::STALE_REFRESH_ERROR)
    );
    assert_eq!(oauth.get(legacy).await?.unwrap().status, "connected");
    assert!(
        oauth
            .complete_refresh(
                cursor,
                lock.status_version,
                cursor_credential("late", "stale-account")
            )
            .await?
            .is_none()
    );
    assert!(
        !oauth
            .fail_refresh(cursor, lock.status_version, "late")
            .await?
    );
    oauth.delete(cursor).await?;
    oauth.delete(legacy).await?;
    Ok(())
}

async fn create_test_provider(storage: &dyn Storage) -> anyhow::Result<String> {
    let input: CreateProvider = serde_json::from_value(serde_json::json!({
        "name": format!("oauth-storage-test-{}", uuid::Uuid::new_v4()),
        "protocol": "openai", "base_url": "https://example.invalid", "api_key": ""
    }))?;
    Ok(storage.providers().create(input).await?.id)
}

#[tokio::test]
async fn sqlite_oauth_contract() -> anyhow::Result<()> {
    // Dedicated temporary WAL database permits independent concurrent connections.
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::db::migrate(&pool).await?;
    let storage = SqliteStorage::from_pool(pool.clone());
    let first = create_test_provider(&storage).await?;
    let second = create_test_provider(&storage).await?;
    exercise_contract(storage.oauth_credentials(), &first, &second).await?;
    prepare_stale_rows(storage.oauth_credentials(), &first, &second).await?;
    sqlx::query("UPDATE provider_oauth_credentials SET updated_at = '2000-01-01 00:00:00' WHERE provider_id IN (?, ?)")
        .bind(&first).bind(&second).execute(&pool).await?;
    assert_stale_recovery(storage.oauth_credentials(), &first, &second).await?;
    let tombstone: (String, Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT access_token, refresh_token, subject_id, meta FROM provider_oauth_credentials WHERE provider_id = ?",
    ).bind(&first).fetch_one(&pool).await?;
    assert_eq!(tombstone, (String::new(), None, None, "{}".to_string()));
    storage.providers().delete(&first).await?;
    storage.providers().delete(&second).await?;
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_oauth_credentials")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        remaining, 0,
        "provider deletion removes version fences via FK cascade"
    );
    pool.close().await;
    Ok(())
}

// External tests are opt-in only: explicit URLs, acknowledgement, and a test
// database name. No fallback to the application's configured/production URL.
fn external_oauth_config(variable: &str) -> anyhow::Result<Option<sql::config::SqlBackendConfig>> {
    let Ok(url) = std::env::var(variable) else {
        eprintln!("skipping optional OAuth database test: {variable} is not set");
        return Ok(None);
    };
    anyhow::ensure!(
        std::env::var("NYRO_TEST_OAUTH_DATABASES_ONLY").as_deref() == Ok("1"),
        "OAuth SQL tests require NYRO_TEST_OAUTH_DATABASES_ONLY=1 and disposable databases"
    );
    let parsed = reqwest::Url::parse(&url)?;
    let database = parsed.path().trim_start_matches('/');
    anyhow::ensure!(
        database.starts_with("nyro_test_") || database.ends_with("_test"),
        "OAuth SQL tests refuse non-test database names"
    );
    Ok(Some(sql::config::SqlBackendConfig {
        max_connections: 5,
        ..sql::config::SqlBackendConfig::with_url(url)
    }))
}

#[tokio::test]
async fn postgres_oauth_optional_disposable_database() -> anyhow::Result<()> {
    let Some(config) = external_oauth_config("NYRO_TEST_POSTGRES_OAUTH_URL")? else {
        return Ok(());
    };
    let storage = PostgresStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    let first = create_test_provider(&storage).await?;
    let second = create_test_provider(&storage).await?;
    let result = async {
        exercise_contract(storage.oauth_credentials(), &first, &second).await?;
        prepare_stale_rows(storage.oauth_credentials(), &first, &second).await?;
        sqlx::query("UPDATE provider_oauth_credentials SET updated_at = '2000-01-01 00:00:00' WHERE provider_id IN ($1, $2)")
            .bind(&first).bind(&second).execute(storage.pool()).await?;
        assert_stale_recovery(storage.oauth_credentials(), &first, &second).await
    }.await;
    storage.providers().delete(&first).await?;
    storage.providers().delete(&second).await?;
    storage.pool().close().await;
    result
}

#[tokio::test]
async fn mysql_oauth_optional_disposable_database() -> anyhow::Result<()> {
    let Some(config) = external_oauth_config("NYRO_TEST_MYSQL_OAUTH_URL")? else {
        return Ok(());
    };
    let storage = MysqlStorage::connect(config).await?;
    storage.bootstrap().migrate().await?;
    let first = create_test_provider(&storage).await?;
    let second = create_test_provider(&storage).await?;
    let result = async {
        exercise_contract(storage.oauth_credentials(), &first, &second).await?;
        prepare_stale_rows(storage.oauth_credentials(), &first, &second).await?;
        sqlx::query("UPDATE provider_oauth_credentials SET updated_at = '2000-01-01 00:00:00' WHERE provider_id IN (?, ?)")
            .bind(&first).bind(&second).execute(storage.pool()).await?;
        assert_stale_recovery(storage.oauth_credentials(), &first, &second).await
    }.await;
    storage.providers().delete(&first).await?;
    storage.providers().delete(&second).await?;
    storage.pool().close().await;
    result
}
