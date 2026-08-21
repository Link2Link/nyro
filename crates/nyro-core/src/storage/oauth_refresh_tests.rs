use super::*;
use crate::db::models::UpsertOAuthCredential;

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
