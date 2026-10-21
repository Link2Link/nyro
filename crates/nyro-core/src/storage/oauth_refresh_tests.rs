use super::traits::OAuthCredentialStore;
use super::*;
use crate::db::models::{CreateProvider, OAuthAccountUpdate, Provider, UpsertOAuthCredential};
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
    postgres_account_migration(&storage).await?;
    let first = create_test_provider(&storage).await?;
    let second = create_test_provider(&storage).await?;
    let result = async {
        exercise_contract(storage.oauth_credentials(), &first, &second).await?;
        exercise_pool_storage(&storage).await?;
        exercise_account_refresh_storage(&storage).await?;
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
    mysql_account_migration(&storage).await?;
    let first = create_test_provider(&storage).await?;
    let second = create_test_provider(&storage).await?;
    let result = async {
        exercise_contract(storage.oauth_credentials(), &first, &second).await?;
        exercise_pool_storage(&storage).await?;
        exercise_account_refresh_storage(&storage).await?;
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

fn account_provider(id: &str, vendor: &str) -> Provider {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": id, "vendor": vendor, "protocol": "openai/chat", "base_url": "https://example.invalid",
        "api_key": "", "auth_mode": "oauth", "is_enabled": true, "created_at": "2026-01-01 00:00:00", "updated_at": "2026-01-01 00:00:00"
    })).unwrap()
}

fn devin_credential(token: &str) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: "devin".into(),
        scheme: "session_token".into(),
        access_token: token.into(),
        ..Default::default()
    }
}

async fn exercise_account_pool(
    oauth: &dyn OAuthCredentialStore,
    devin: &str,
    other: &str,
) -> anyhow::Result<()> {
    let a = oauth
        .create_account(devin, "A", devin_credential(" eyJ-account-a "))
        .await?;
    assert!(uuid::Uuid::parse_str(&a.account_id).is_ok());
    assert_eq!(a.access_token, "devin-session-token$eyJ-account-a");
    let duplicate = oauth
        .create_account(
            devin,
            "duplicate",
            devin_credential("devin-session-token$eyJ-account-a"),
        )
        .await
        .unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains(&format!("DUPLICATE_ACCOUNT: {}", a.account_id))
    );
    // Authorization error is still an active account for ambiguity checks.
    let lease = oauth
        .try_begin_refresh(devin, a.status_version)
        .await?
        .unwrap();
    assert!(
        oauth
            .fail_refresh_with_policy(devin, lease.status_version, "safe auth failure", true, None)
            .await?
    );
    let a = oauth.get_account(devin, &a.account_id).await?.unwrap();
    assert_eq!(a.status, "error");
    let b = oauth
        .create_account(devin, "B", devin_credential("token-b"))
        .await?;
    assert_eq!(oauth.list_accounts(devin).await?.len(), 2);
    assert!(oauth.get_account(other, &a.account_id).await?.is_none());
    assert!(
        !oauth
            .disconnect_account(other, &a.account_id, a.status_version)
            .await?
    );
    assert!(
        oauth
            .replace_account(
                other,
                &a.account_id,
                a.status_version,
                devin_credential("cross-provider")
            )
            .await?
            .is_none()
    );

    let b = oauth
        .update_account(
            devin,
            &b.account_id,
            b.status_version,
            OAuthAccountUpdate {
                is_enabled: Some(false),
                priority: Some(-2),
                ..Default::default()
            },
        )
        .await?
        .unwrap();
    assert!(!b.is_enabled);
    assert_eq!(b.priority, -2);
    assert!(
        oauth
            .update_account(
                devin,
                &b.account_id,
                b.status_version - 1,
                OAuthAccountUpdate {
                    is_enabled: Some(true),
                    ..Default::default()
                }
            )
            .await?
            .is_none()
    );
    assert!(
        oauth
            .compare_account_state(
                devin,
                &b.account_id,
                b.status_version,
                "{}",
                r#"{"blocked":"Weekly"}"#
            )
            .await?
    );
    assert!(
        !oauth
            .compare_account_state(
                devin,
                &b.account_id,
                b.status_version,
                r#"{"blocked":"weekly"}"#,
                "{}"
            )
            .await?
    );
    let metadata = oauth
        .update_account(
            devin,
            &b.account_id,
            b.status_version,
            OAuthAccountUpdate {
                label: Some("Renamed".into()),
                ..Default::default()
            },
        )
        .await?
        .unwrap();
    assert_eq!(metadata.account_state, r#"{"blocked":"Weekly"}"#);
    assert_eq!(metadata.status_version, b.status_version + 1);
    let b = metadata;
    assert_eq!(
        oauth.list_accounts(devin).await?[0].account_id,
        b.account_id
    );
    for result in [
        oauth.get(devin).await.map(|_| ()),
        oauth
            .upsert(devin, devin_credential("do-not-replace"))
            .await
            .map(|_| ()),
        oauth.delete(devin).await,
        oauth
            .restore_if_version(devin, a.status_version, None)
            .await
            .map(|_| ()),
        oauth
            .try_begin_refresh(devin, a.status_version)
            .await
            .map(|_| ()),
        oauth
            .complete_refresh(devin, a.status_version, devin_credential("late"))
            .await
            .map(|_| ()),
        oauth
            .fail_refresh(devin, a.status_version, "late")
            .await
            .map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().to_string(), "AMBIGUOUS_ACCOUNT");
    }
    assert_eq!(oauth.list_accounts(devin).await?.len(), 2);
    assert_eq!(
        oauth
            .get_account(devin, &a.account_id)
            .await?
            .unwrap()
            .access_token,
        a.access_token
    );

    assert!(
        oauth
            .compare_account_state(
                devin,
                &a.account_id,
                a.status_version,
                "{}",
                r#"{"blocked":"weekly"}"#
            )
            .await?
    );
    assert!(
        !oauth
            .compare_account_state(
                devin,
                &a.account_id,
                a.status_version,
                "{}",
                r#"{"remaining":100}"#
            )
            .await?
    );
    assert_eq!(
        oauth
            .get_account(devin, &a.account_id)
            .await?
            .unwrap()
            .status_version,
        a.status_version
    );
    assert!(
        oauth
            .compare_account_state(
                devin,
                &a.account_id,
                a.status_version,
                r#"{"blocked":"weekly"}"#,
                "[]"
            )
            .await
            .is_err()
    );
    let duplicate = oauth
        .replace_account(
            devin,
            &a.account_id,
            a.status_version,
            devin_credential("token-b"),
        )
        .await
        .unwrap_err();
    assert!(duplicate.to_string().contains("DUPLICATE_ACCOUNT"));
    let (left, right) = tokio::join!(
        oauth.replace_account(
            devin,
            &a.account_id,
            a.status_version,
            devin_credential("replacement-left")
        ),
        oauth.replace_account(
            devin,
            &a.account_id,
            a.status_version,
            devin_credential("replacement-right")
        ),
    );
    let (left, right) = (left?, right?);
    assert_ne!(left.is_some(), right.is_some());
    let replaced = left.or(right).unwrap();
    assert_eq!(replaced.status_version, a.status_version + 1);
    assert_eq!(replaced.account_state, "{}");
    assert!(
        !oauth
            .compare_account_state(
                devin,
                &a.account_id,
                a.status_version,
                "{}",
                r#"{"remaining":100}"#
            )
            .await?
    );
    assert!(
        oauth
            .disconnect_account(devin, &a.account_id, replaced.status_version)
            .await?
    );
    assert!(
        !oauth
            .disconnect_account(devin, &a.account_id, replaced.status_version)
            .await?
    );
    assert!(
        oauth
            .replace_account(
                devin,
                &a.account_id,
                replaced.status_version + 1,
                devin_credential("revive")
            )
            .await?
            .is_none()
    );
    assert_eq!(oauth.get(devin).await?.unwrap().account_id, b.account_id);
    let rebound = oauth
        .upsert(devin, devin_credential("singleton-rebind"))
        .await?;
    assert_eq!(rebound.account_id, b.account_id);
    assert!(
        !oauth
            .restore_if_version(
                devin,
                rebound.status_version,
                Some(devin_credential("unsafe accountless rollback"))
            )
            .await?
    );
    assert!(
        oauth
            .try_begin_refresh(devin, rebound.status_version)
            .await?
            .is_none()
    );
    oauth.delete(devin).await?;
    assert!(oauth.list_accounts(devin).await?.is_empty());

    // Both duplicate checks and the singleton limit are inside the write lock.
    let (first, second) = tokio::join!(
        oauth.create_account(other, "one", credential("one")),
        oauth.create_account(other, "two", credential("two"))
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let rejected = if let Err(error) = first {
        error
    } else {
        second.unwrap_err()
    };
    assert!(rejected.to_string().contains("MAX_ACCOUNTS"));
    assert_eq!(oauth.list_accounts(other).await?.len(), 1);
    let first_generation = oauth.get(other).await?.unwrap();
    let lease = oauth
        .try_begin_refresh(other, first_generation.status_version)
        .await?
        .unwrap();
    oauth.delete(other).await?;
    let recreated = oauth
        .create_account(other, "new identity", credential("recreated"))
        .await?;
    assert!(recreated.status_version > lease.status_version);
    assert!(
        oauth
            .complete_refresh(
                other,
                lease.status_version,
                credential("stale old identity")
            )
            .await?
            .is_none()
    );
    oauth.delete(other).await?;
    assert!(
        oauth
            .create_account(other, "wrong vendor", devin_credential("wrong"))
            .await
            .is_err()
    );
    assert!(
        oauth
            .create_account("missing", "missing", credential("missing"))
            .await
            .is_err()
    );

    let (first, second) = tokio::join!(
        oauth.create_account(devin, "one", devin_credential("same-token")),
        oauth.create_account(devin, "two", devin_credential(" same-token "))
    );
    assert_ne!(first.is_ok(), second.is_ok());
    oauth.delete(devin).await?;
    // Authenticated identity dedup needs both subject and explicit org scope.
    let scoped = |token: &str, org: &str| UpsertOAuthCredential {
        subject_id: Some("subject".into()),
        meta: Some(serde_json::json!({"organization_id":org}).to_string()),
        ..devin_credential(token)
    };
    let scoped_a = oauth
        .create_account(devin, "scope-a", scoped("scope-a", "org-a"))
        .await?;
    assert!(
        oauth
            .create_account(devin, "duplicate identity", scoped("scope-b", "org-a"))
            .await
            .unwrap_err()
            .to_string()
            .contains("DUPLICATE_ACCOUNT")
    );
    let scoped_b = oauth
        .create_account(devin, "scope-b", scoped("scope-c", "org-b"))
        .await?;
    assert!(
        oauth
            .disconnect_account(devin, &scoped_a.account_id, scoped_a.status_version)
            .await?
    );
    assert!(
        oauth
            .disconnect_account(devin, &scoped_b.account_id, scoped_b.status_version)
            .await?
    );
    Ok(())
}

fn kiro_credential(token: &str) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: "kiro".into(),
        scheme: "oauth_device_code".into(),
        scopes: Some(r#"["kiro:read"]"#.into()),
        meta: Some(r#"{"region":"us-east-1"}"#.into()),
        ..credential(token)
    }
}

async fn assert_rejected_account_refresh(
    oauth: &dyn OAuthCredentialStore,
    provider: &str,
    account_id: &str,
    version: i32,
) -> anyhow::Result<()> {
    let before = serde_json::to_value(oauth.list_accounts(provider).await?)?;
    assert!(
        oauth
            .complete_account_refresh(provider, account_id, version, kiro_credential("stale"))
            .await?
            .is_none()
    );
    assert!(
        !oauth
            .fail_account_refresh_with_policy(
                provider,
                account_id,
                version,
                "stale failure",
                true,
                Some("{}".into()),
            )
            .await?
    );
    assert_eq!(
        serde_json::to_value(oauth.list_accounts(provider).await?)?,
        before,
        "rejected callbacks must not mutate any account"
    );
    Ok(())
}

async fn exercise_account_refresh(
    oauth: &dyn OAuthCredentialStore,
    kiro: &str,
    other: &str,
) -> anyhow::Result<()> {
    // Kiro supports multiple accounts, but other drivers keep their singleton limit.
    let a = oauth
        .create_account(kiro, "A", kiro_credential("a"))
        .await?;
    let b = oauth
        .create_account(kiro, "B", kiro_credential("b"))
        .await?;
    assert_eq!(oauth.list_accounts(kiro).await?.len(), 2);
    let sibling = serde_json::to_value(&b)?;
    let single = oauth
        .create_account(other, "Only", credential("only"))
        .await?;
    assert!(
        oauth
            .create_account(other, "Second", credential("second"))
            .await
            .unwrap_err()
            .to_string()
            .contains("MAX_ACCOUNTS")
    );
    assert_eq!(
        oauth.get(kiro).await.unwrap_err().to_string(),
        "AMBIGUOUS_ACCOUNT"
    );

    // Explicit refresh mirrors the legacy status policy, including preserving a
    // disabled account's enablement. Callers decide whether to start its refresh.
    let a = oauth
        .update_account(
            kiro,
            &a.account_id,
            a.status_version,
            OAuthAccountUpdate {
                label: Some("Primary".into()),
                priority: Some(-7),
                is_enabled: Some(false),
            },
        )
        .await?
        .unwrap();
    assert_eq!(
        a.status_version, b.status_version,
        "identical versions need exact identity"
    );
    let state = r#"{"quota":{"remaining":12},"cooldown_until":2000000000}"#;
    assert!(
        oauth
            .compare_account_state(kiro, &a.account_id, a.status_version, "{}", state)
            .await?
    );
    for (provider, id) in [(other, a.account_id.as_str()), (kiro, "missing")] {
        assert!(
            oauth
                .try_begin_account_refresh(provider, id, a.status_version)
                .await?
                .is_none()
        );
        assert_rejected_account_refresh(oauth, provider, id, a.status_version).await?;
    }
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, a.status_version).await?;

    let (left, right) = tokio::join!(
        oauth.try_begin_account_refresh(kiro, &a.account_id, a.status_version),
        oauth.try_begin_account_refresh(kiro, &a.account_id, a.status_version),
    );
    let (left, right) = (left?, right?);
    assert_ne!(
        left.is_some(),
        right.is_some(),
        "one generation has one refresh owner"
    );
    let lease = left.or(right).unwrap();
    assert_eq!(lease.account_id, a.account_id);
    assert_eq!(lease.status, "refreshing");
    assert_eq!(lease.status_version, a.status_version + 1);
    assert_eq!(lease.account_state, state);
    assert_eq!(
        serde_json::to_value(oauth.get_account(kiro, &b.account_id).await?.unwrap())?,
        sibling
    );
    assert!(
        oauth
            .try_begin_account_refresh(kiro, &a.account_id, lease.status_version)
            .await?
            .is_none()
    );
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, a.status_version).await?;

    // Observations do not advance the generation, and completion preserves the
    // latest state rather than a snapshot from when the refresh began.
    let observed = r#"{"quota":{"remaining":9},"cooldown_until":2000000000}"#;
    assert!(
        oauth
            .compare_account_state(kiro, &a.account_id, lease.status_version, state, observed)
            .await?
    );
    let mut input = kiro_credential("refreshed");
    input.meta = None;
    input.scopes = None;
    input.refresh_token = Some("rotated-refresh-token".into());
    let refreshed = oauth
        .complete_account_refresh(kiro, &a.account_id, lease.status_version, input)
        .await?
        .unwrap();
    assert_eq!(refreshed.status, "connected");
    assert_eq!(refreshed.status_version, lease.status_version + 1);
    assert_eq!(refreshed.access_token, "refreshed");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("rotated-refresh-token")
    );
    assert_eq!(refreshed.meta, a.meta);
    assert_eq!(refreshed.scopes, a.scopes);
    assert_eq!(refreshed.account_state, observed);
    assert_eq!(refreshed.label, "Primary");
    assert_eq!(refreshed.priority, -7);
    assert!(!refreshed.is_enabled);
    assert!(refreshed.last_refresh_at.is_some());
    assert!(refreshed.last_error.is_none());
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, lease.status_version).await?;
    assert_eq!(
        serde_json::to_value(oauth.get_account(kiro, &b.account_id).await?.unwrap())?,
        sibling
    );

    let retry_meta = r#"{"region":"us-east-1","refresh_failures":1}"#;
    let mut current = refreshed;
    for (terminal, meta) in [
        (false, Some(retry_meta.to_string())),
        (false, None),
        (true, None),
    ] {
        let lease = oauth
            .try_begin_account_refresh(kiro, &a.account_id, current.status_version)
            .await?
            .unwrap();
        assert!(
            oauth
                .fail_account_refresh_with_policy(
                    kiro,
                    &a.account_id,
                    lease.status_version,
                    "safe failure",
                    terminal,
                    meta
                )
                .await?
        );
        current = oauth.get_account(kiro, &a.account_id).await?.unwrap();
        assert_eq!(current.status, if terminal { "error" } else { "connected" });
        assert_eq!(current.status_version, lease.status_version + 1);
        assert_eq!(current.last_error.as_deref(), Some("safe failure"));
        assert_eq!(current.meta, retry_meta);
        assert_eq!(current.access_token, "refreshed");
        assert_eq!(current.account_state, observed);
        assert_eq!(current.label, "Primary");
        assert_eq!(current.priority, -7);
        assert!(!current.is_enabled);
        assert_rejected_account_refresh(oauth, kiro, &a.account_id, lease.status_version).await?;
        assert_eq!(
            serde_json::to_value(oauth.get_account(kiro, &b.account_id).await?.unwrap())?,
            sibling
        );
    }
    assert!(
        oauth
            .try_begin_account_refresh(kiro, &a.account_id, current.status_version)
            .await?
            .is_none()
    );

    // Reauthorization remains separate from refresh: it resets account state and
    // fences callbacks, even with sibling accounts present.
    current = oauth
        .replace_account(
            kiro,
            &a.account_id,
            current.status_version,
            kiro_credential("reauthorized"),
        )
        .await?
        .unwrap();
    assert_eq!(current.account_state, "{}");
    current = oauth
        .update_account(
            kiro,
            &a.account_id,
            current.status_version,
            OAuthAccountUpdate {
                is_enabled: Some(true),
                ..Default::default()
            },
        )
        .await?
        .unwrap();
    let lease = oauth
        .try_begin_account_refresh(kiro, &a.account_id, current.status_version)
        .await?
        .unwrap();
    let rebound = oauth
        .replace_account(
            kiro,
            &a.account_id,
            lease.status_version,
            kiro_credential("new-reauth"),
        )
        .await?
        .unwrap();
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, lease.status_version).await?;
    assert_eq!(
        oauth
            .get_account(kiro, &a.account_id)
            .await?
            .unwrap()
            .access_token,
        "new-reauth"
    );

    let lease = oauth
        .try_begin_account_refresh(kiro, &a.account_id, rebound.status_version)
        .await?
        .unwrap();
    assert!(
        oauth
            .compare_account_state(kiro, &a.account_id, lease.status_version, "{}", state)
            .await?
    );
    let disabled = oauth
        .update_account(
            kiro,
            &a.account_id,
            lease.status_version,
            OAuthAccountUpdate {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?
        .unwrap();
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, lease.status_version).await?;
    assert!(!disabled.is_enabled);
    assert_eq!(disabled.account_state, state);
    assert_eq!(
        oauth
            .get_account(kiro, &a.account_id)
            .await?
            .unwrap()
            .status_version,
        disabled.status_version
    );

    let rebound = oauth
        .replace_account(
            kiro,
            &a.account_id,
            disabled.status_version,
            kiro_credential("before-disconnect"),
        )
        .await?
        .unwrap();
    let lease = oauth
        .try_begin_account_refresh(kiro, &a.account_id, rebound.status_version)
        .await?
        .unwrap();
    assert!(
        oauth
            .disconnect_account(kiro, &a.account_id, lease.status_version)
            .await?
    );
    for version in [lease.status_version, lease.status_version + 1] {
        assert!(
            oauth
                .try_begin_account_refresh(kiro, &a.account_id, version)
                .await?
                .is_none()
        );
        assert_rejected_account_refresh(oauth, kiro, &a.account_id, version).await?;
    }
    assert!(oauth.get_account(kiro, &a.account_id).await?.is_none());
    assert_eq!(
        serde_json::to_value(oauth.get_account(kiro, &b.account_id).await?.unwrap())?,
        sibling
    );

    // A pool reduced to one account still rejects account-less refresh callbacks,
    // while exact account refresh continues to work beside disconnected fences.
    let rebound = oauth.upsert(kiro, kiro_credential("b-rebound")).await?;
    assert_eq!(rebound.account_id, b.account_id);
    assert!(rebound.status_version > lease.status_version + 1);
    assert!(
        oauth
            .try_begin_refresh(kiro, rebound.status_version)
            .await?
            .is_none()
    );
    let lease = oauth
        .try_begin_account_refresh(kiro, &b.account_id, rebound.status_version)
        .await?
        .unwrap();
    assert_rejected_account_refresh(oauth, kiro, &a.account_id, lease.status_version).await?;
    let completed = oauth
        .complete_account_refresh(
            kiro,
            &b.account_id,
            lease.status_version,
            kiro_credential("b-refreshed"),
        )
        .await?
        .unwrap();
    assert_eq!(completed.account_id, b.account_id);
    assert_eq!(completed.access_token, "b-refreshed");
    assert!(
        oauth
            .disconnect_account(kiro, &b.account_id, completed.status_version)
            .await?
    );
    assert!(
        oauth
            .disconnect_account(other, &single.account_id, single.status_version)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn memory_oauth_account_refresh() -> anyhow::Result<()> {
    let storage = MemoryStorage::new(
        vec![
            account_provider("kiro", "kiro"),
            account_provider("codex", "openai"),
        ],
        vec![],
        vec![],
    );
    exercise_account_refresh(storage.oauth_credentials(), "kiro", "codex").await
}

#[tokio::test]
async fn memory_oauth_account_pool() -> anyhow::Result<()> {
    let mut apikey = account_provider("apikey", "devin");
    apikey.auth_mode = "apikey".into();
    let storage = MemoryStorage::new(
        vec![
            account_provider("devin", "devin"),
            account_provider("codex", "openai"),
            apikey,
        ],
        vec![],
        vec![],
    );
    assert!(
        storage
            .oauth_credentials()
            .create_account("apikey", "invalid parent", devin_credential("invalid"))
            .await
            .is_err()
    );
    exercise_account_pool(storage.oauth_credentials(), "devin", "codex").await
}

async fn create_pool_test_provider(storage: &dyn Storage, vendor: &str) -> anyhow::Result<String> {
    let input: CreateProvider = serde_json::from_value(serde_json::json!({
        "name": format!("oauth-account-test-{}", uuid::Uuid::new_v4()), "vendor": vendor,
        "protocol": "openai", "base_url": "https://example.invalid", "api_key": "", "auth_mode": "oauth"
    }))?;
    Ok(storage.providers().create(input).await?.id)
}

async fn exercise_pool_storage(storage: &dyn Storage) -> anyhow::Result<()> {
    let devin = create_pool_test_provider(storage, "devin").await?;
    let other = create_pool_test_provider(storage, "openai").await?;
    let result = exercise_account_pool(storage.oauth_credentials(), &devin, &other).await;
    storage.providers().delete(&devin).await?;
    storage.providers().delete(&other).await?;
    result?;
    exercise_google_channel_storage(storage).await
}

async fn exercise_account_refresh_storage(storage: &dyn Storage) -> anyhow::Result<()> {
    let kiro = create_pool_test_provider(storage, "kiro").await?;
    let other = create_pool_test_provider(storage, "openai").await?;
    let result = exercise_account_refresh(storage.oauth_credentials(), &kiro, &other).await;
    storage.providers().delete(&kiro).await?;
    storage.providers().delete(&other).await?;
    result
}

#[tokio::test]
async fn sqlite_oauth_account_refresh() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = crate::db::init_pool(dir.path()).await?;
    crate::db::migrate(&pool).await?;
    let storage = SqliteStorage::from_pool(pool.clone());
    exercise_account_refresh_storage(&storage).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_oauth_account_pool() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = crate::db::init_pool(dir.path()).await?;
    crate::db::migrate(&pool).await?;
    let storage = SqliteStorage::from_pool(pool.clone());
    exercise_pool_storage(&storage).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_oauth_legacy_migration_preserves_fences() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = crate::db::init_pool(dir.path()).await?;
    crate::db::migrate(&pool).await?;
    let storage = SqliteStorage::from_pool(pool.clone());
    let live = create_pool_test_provider(&storage, "devin").await?;
    let dead = create_pool_test_provider(&storage, "devin").await?;
    // Reconstruct only the old OAuth table in this dedicated temporary DB.
    sqlx::query("DROP TABLE provider_oauth_credentials")
        .execute(&pool)
        .await?;
    sqlx::query("CREATE TABLE provider_oauth_credentials (provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE, driver_key TEXT NOT NULL DEFAULT '', scheme TEXT NOT NULL DEFAULT '', access_token TEXT NOT NULL DEFAULT '', refresh_token TEXT, expires_at TEXT, resource_url TEXT, subject_id TEXT, scopes TEXT NOT NULL DEFAULT '[]', meta TEXT NOT NULL DEFAULT '{}', status TEXT NOT NULL DEFAULT 'connected', status_version INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_refresh_at TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')))").execute(&pool).await?;
    for (id, token, status, version) in [
        (&live, "retained-secret", "connected", 17),
        (&dead, "", "disconnected", 29),
    ] {
        sqlx::query("INSERT INTO provider_oauth_credentials(provider_id, access_token, status, status_version) VALUES (?, ?, ?, ?)").bind(id).bind(token).bind(status).bind(version).execute(&pool).await?;
        sqlx::query("UPDATE providers SET access_token='obsolete-provider-secret' WHERE id=?")
            .bind(id)
            .execute(&pool)
            .await?;
    }
    crate::db::migrate(&pool).await?;
    crate::db::migrate(&pool).await?;
    let migrated = storage.oauth_credentials().get(&live).await?.unwrap();
    assert_eq!(migrated.account_id, format!("legacy:{live}"));
    assert_eq!(migrated.status_version, 17);
    assert_eq!(migrated.access_token, "retained-secret");
    assert_eq!(migrated.label, "Default account");
    assert!(storage.oauth_credentials().get(&dead).await?.is_none());
    let row: (String, String, i32) = sqlx::query_as("SELECT account_id, access_token, status_version FROM provider_oauth_credentials WHERE provider_id=?").bind(&dead).fetch_one(&pool).await?;
    assert_eq!(row, (format!("legacy:{dead}"), String::new(), 29));
    storage.oauth_credentials().delete(&live).await?;
    crate::db::migrate(&pool).await?;
    assert!(storage.oauth_credentials().get(&live).await?.is_none());
    let rebound = storage
        .oauth_credentials()
        .upsert(&live, devin_credential("new-secret"))
        .await?;
    assert_eq!(rebound.account_id, migrated.account_id);
    assert_eq!(rebound.status_version, 19);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_oauth_credentials")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2);
    // A new UUID-only pool must not get a legacy account from obsolete fields,
    // even once its final member has become a disconnected tombstone.
    let uuid_provider = create_pool_test_provider(&storage, "devin").await?;
    let uuid_row = storage
        .oauth_credentials()
        .create_account(
            &uuid_provider,
            "UUID account",
            devin_credential("uuid-token"),
        )
        .await?;
    sqlx::query("UPDATE providers SET access_token='obsolete-uuid-provider-secret' WHERE id=?")
        .bind(&uuid_provider)
        .execute(&pool)
        .await?;
    crate::db::migrate(&pool).await?;
    assert_eq!(
        storage
            .oauth_credentials()
            .list_accounts(&uuid_provider)
            .await?
            .len(),
        1
    );
    assert!(
        storage
            .oauth_credentials()
            .disconnect_account(
                &uuid_provider,
                &uuid_row.account_id,
                uuid_row.status_version
            )
            .await?
    );
    crate::db::migrate(&pool).await?;
    assert!(
        storage
            .oauth_credentials()
            .list_accounts(&uuid_provider)
            .await?
            .is_empty()
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM provider_oauth_credentials WHERE provider_id=?")
            .bind(&uuid_provider)
            .fetch_one(&pool)
            .await?;
    assert_eq!(rows, 1);
    pool.close().await;
    Ok(())
}

// These migration checks are called only by the explicitly opted-in disposable
// backend tests above, and refuse to downgrade any populated credential table.
async fn postgres_account_migration(storage: &PostgresStorage) -> anyhow::Result<()> {
    let pool = storage.pool();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_oauth_credentials")
        .fetch_one(pool)
        .await?;
    anyhow::ensure!(
        count == 0,
        "OAuth migration regression requires an empty disposable credential table"
    );
    let live = create_pool_test_provider(storage, "devin").await?;
    let dead = create_pool_test_provider(storage, "devin").await?;
    sqlx::query("ALTER TABLE provider_oauth_credentials DROP CONSTRAINT provider_oauth_credentials_pkey, DROP COLUMN account_id, DROP COLUMN label, DROP COLUMN is_enabled, DROP COLUMN priority, DROP COLUMN account_state, ADD PRIMARY KEY(provider_id)").execute(pool).await?;
    for (id, token, status, version) in [
        (&live, "preserved", "connected", 17),
        (&dead, "", "disconnected", 29),
    ] {
        sqlx::query("INSERT INTO provider_oauth_credentials(provider_id, access_token, status, status_version) VALUES ($1,$2,$3,$4)").bind(id).bind(token).bind(status).bind(version).execute(pool).await?;
        sqlx::query("UPDATE providers SET access_token='obsolete' WHERE id=$1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(
        storage
            .oauth_credentials()
            .get(&live)
            .await?
            .unwrap()
            .status_version,
        17
    );
    let tombstone: (String, String, i32) = sqlx::query_as("SELECT account_id, access_token, status_version FROM provider_oauth_credentials WHERE provider_id=$1").bind(&dead).fetch_one(pool).await?;
    assert_eq!(tombstone, (format!("legacy:{dead}"), String::new(), 29));
    storage.oauth_credentials().delete(&live).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.oauth_credentials().get(&live).await?.is_none());
    assert!(storage.oauth_credentials().get(&dead).await?.is_none());
    storage.providers().delete(&live).await?;
    storage.providers().delete(&dead).await?;
    Ok(())
}

async fn mysql_account_migration(storage: &MysqlStorage) -> anyhow::Result<()> {
    let pool = storage.pool();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_oauth_credentials")
        .fetch_one(pool)
        .await?;
    anyhow::ensure!(
        count == 0,
        "OAuth migration regression requires an empty disposable credential table"
    );
    let live = create_pool_test_provider(storage, "devin").await?;
    let dead = create_pool_test_provider(storage, "devin").await?;
    sqlx::query("ALTER TABLE provider_oauth_credentials DROP PRIMARY KEY, DROP COLUMN account_id, DROP COLUMN label, DROP COLUMN is_enabled, DROP COLUMN priority, DROP COLUMN account_state, ADD PRIMARY KEY(provider_id)").execute(pool).await?;
    for (id, token, status, version) in [
        (&live, "preserved", "connected", 17),
        (&dead, "", "disconnected", 29),
    ] {
        sqlx::query("INSERT INTO provider_oauth_credentials(provider_id, driver_key, scheme, scopes, meta, access_token, status, status_version) VALUES (?, '', '', '[]', '{}', ?, ?, ?)").bind(id).bind(token).bind(status).bind(version).execute(pool).await?;
        sqlx::query("UPDATE providers SET access_token='obsolete' WHERE id=?")
            .bind(id)
            .execute(pool)
            .await?;
    }
    storage.bootstrap().migrate().await?;
    storage.bootstrap().migrate().await?;
    assert_eq!(
        storage
            .oauth_credentials()
            .get(&live)
            .await?
            .unwrap()
            .status_version,
        17
    );
    let tombstone: (String, String, i32) = sqlx::query_as("SELECT account_id, access_token, status_version FROM provider_oauth_credentials WHERE provider_id=?").bind(&dead).fetch_one(pool).await?;
    assert_eq!(tombstone, (format!("legacy:{dead}"), String::new(), 29));
    storage.oauth_credentials().delete(&live).await?;
    storage.bootstrap().migrate().await?;
    assert!(storage.oauth_credentials().get(&live).await?.is_none());
    assert!(storage.oauth_credentials().get(&dead).await?.is_none());
    storage.providers().delete(&live).await?;
    storage.providers().delete(&dead).await?;
    Ok(())
}

async fn exercise_google_channel_accounts(
    oauth: &dyn OAuthCredentialStore,
    cli: &str,
    antigravity: &str,
) -> anyhow::Result<()> {
    let cli_input = |token: &str| UpsertOAuthCredential {
        driver_key: "google-gemini-cli".into(),
        ..credential(token)
    };
    let ide_input = |token: &str| UpsertOAuthCredential {
        driver_key: "google".into(),
        ..credential(token)
    };
    assert!(
        oauth
            .create_account(cli, "Wrong OAuth client", ide_input("wrong-client"))
            .await
            .unwrap_err()
            .to_string()
            .contains("vendor mismatch")
    );
    assert!(
        oauth
            .create_account(antigravity, "Wrong channel", cli_input("wrong-channel"))
            .await
            .unwrap_err()
            .to_string()
            .contains("vendor mismatch")
    );
    let created = oauth
        .create_account(cli, "Gemini CLI", cli_input("first-token"))
        .await?;
    let replaced = oauth
        .replace_account(
            cli,
            &created.account_id,
            created.status_version,
            cli_input("reauthorized-token"),
        )
        .await?
        .unwrap();
    assert_eq!(replaced.account_id, created.account_id);
    assert_eq!(replaced.status_version, created.status_version + 1);
    assert_eq!(replaced.access_token, "reauthorized-token");
    assert!(
        oauth
            .replace_account(
                cli,
                &replaced.account_id,
                replaced.status_version,
                ide_input("wrong-client")
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("vendor mismatch")
    );
    assert_eq!(
        oauth.get(cli).await?.unwrap().access_token,
        "reauthorized-token"
    );
    oauth.delete(cli).await?;
    // Legacy provider-only imports remain compatible, and their first explicit
    // reauthorization must resolve the same channel-specific client as create.
    let legacy = oauth.upsert(cli, cli_input("legacy-token")).await?;
    assert!(
        oauth
            .replace_account(
                cli,
                &legacy.account_id,
                legacy.status_version,
                cli_input("legacy-reauthorized")
            )
            .await?
            .is_some()
    );
    let ide = oauth
        .create_account(antigravity, "Antigravity", ide_input("ide-token"))
        .await?;
    assert!(
        oauth
            .replace_account(
                antigravity,
                &ide.account_id,
                ide.status_version,
                cli_input("wrong-channel")
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("vendor mismatch")
    );
    assert!(
        oauth
            .replace_account(
                antigravity,
                &ide.account_id,
                ide.status_version,
                ide_input("ide-reauthorized")
            )
            .await?
            .is_some()
    );
    oauth.delete(cli).await?;
    oauth.delete(antigravity).await?;
    Ok(())
}

async fn exercise_google_channel_storage(storage: &dyn Storage) -> anyhow::Result<()> {
    let mut ids = Vec::new();
    for channel in ["gemini-cli", "antigravity"] {
        let input: CreateProvider = serde_json::from_value(serde_json::json!({
            "name": format!("oauth-google-channel-test-{}", uuid::Uuid::new_v4()),
            "vendor": "google", "preset_key": "google", "channel": channel,
            "protocol": "google-gemini", "base_url": "https://example.invalid", "api_key": "", "auth_mode": "oauth"
        }))?;
        ids.push(storage.providers().create(input).await?.id);
    }
    let result =
        exercise_google_channel_accounts(storage.oauth_credentials(), &ids[0], &ids[1]).await;
    for id in &ids {
        storage.providers().delete(id).await?;
    }
    result
}

#[tokio::test]
async fn memory_oauth_google_channel_reauthorization() -> anyhow::Result<()> {
    let mut cli = account_provider("cli", "google");
    cli.preset_key = Some("google".into());
    cli.channel = Some("gemini-cli".into());
    let mut ide = cli.clone();
    ide.id = "ide".into();
    ide.channel = Some("antigravity".into());
    let mut preset_only = cli.clone();
    preset_only.id = "preset-only".into();
    preset_only.vendor = None;
    let mut wrong_vendor = cli.clone();
    wrong_vendor.id = "wrong-vendor".into();
    wrong_vendor.vendor = Some("custom".into());
    let storage = MemoryStorage::new(vec![cli, ide, preset_only, wrong_vendor], vec![], vec![]);
    exercise_google_channel_accounts(storage.oauth_credentials(), "cli", "ide").await?;
    let input = UpsertOAuthCredential {
        driver_key: "google-gemini-cli".into(),
        ..credential("preset-fallback")
    };
    assert!(
        storage
            .oauth_credentials()
            .create_account("preset-only", "Preset fallback", input.clone())
            .await
            .is_ok()
    );
    assert!(
        storage
            .oauth_credentials()
            .create_account("wrong-vendor", "Vendor takes precedence", input)
            .await
            .unwrap_err()
            .to_string()
            .contains("vendor mismatch")
    );
    Ok(())
}
