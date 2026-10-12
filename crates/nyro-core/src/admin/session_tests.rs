use super::*;
use crate::auth::AuthExchangeInput;
use crate::config::GatewayConfig;
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

const FAR_FUTURE_RFC3339: &str = "2099-01-01T00:00:00Z";
const PAST_RFC3339: &str = "2000-01-01T00:00:00Z";
const CODEX_RUNTIME_URL: &str = "https://chatgpt.com/backend-api/codex";
const GROK_RUNTIME_URL: &str = "https://cli-chat-proxy.grok.com/v1";

#[tokio::test]
async fn oauth_session_is_shared_across_admin_instances_and_cancel_deletes_it() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;

    let init = gw.admin().init_oauth_session("codex", false).await?;
    let status = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await?;
    assert!(matches!(status, AuthSessionStatusData::Pending { .. }));

    gw.admin().cancel_oauth_session(&init.session_id).await?;
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .is_none()
    );

    let err = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await
        .expect_err("cancelled session should be removed");
    assert!(err.to_string().contains("auth session not found"));

    Ok(())
}

#[tokio::test]
async fn failed_complete_preserves_pending_session_for_retry() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let init = gw.admin().init_oauth_session("codex", false).await?;
    let err = gw
        .admin()
        .complete_oauth_session(
            &init.session_id,
            AuthExchangeInput {
                code: None,
                callback_url: Some(
                    "https://app.example/callback?code=test-code&state=wrong-state".to_string(),
                ),
                metadata: Value::Null,
            },
        )
        .await
        .expect_err("invalid callback state should fail the exchange");

    assert!(
        err.to_string().contains("state"),
        "unexpected complete error: {err:#}"
    );
    let session = gw
        .admin()
        .get_auth_session_record(&init.session_id)
        .await?
        .expect("failed exchange should preserve PKCE state for retry");
    assert_eq!(session.status, AuthSessionStatus::Pending.as_str());

    Ok(())
}

#[tokio::test]
async fn timeout_and_cleanup_remove_expired_sessions() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let timed_out = gw.admin().init_oauth_session("codex", false).await?;
    gw.admin()
        .update_auth_session_record(
            &timed_out.session_id,
            UpdateAuthSession {
                expires_at: Some(PAST_RFC3339.to_string()),
                ..Default::default()
            },
        )
        .await?;

    let status = gw
        .admin()
        .get_oauth_session_status(&timed_out.session_id)
        .await?;
    assert!(matches!(
        status,
        AuthSessionStatusData::Error { ref code, .. } if code == "AUTH_TIMEOUT"
    ));
    assert!(
        gw.admin()
            .get_auth_session_record(&timed_out.session_id)
            .await?
            .is_none()
    );

    let stale_ready = gw.admin().init_oauth_session("codex", false).await?;
    seed_ready_session(
        &gw.admin(),
        &stale_ready.session_id,
        CredentialBundle {
            access_token: Some("stale-access-token".to_string()),
            refresh_token: Some("stale-refresh-token".to_string()),
            expires_at: Some(PAST_RFC3339.to_string()),
            resource_url: None,
            subject_id: None,
            scopes: vec![],
            raw: json!({ "access_token": "stale-access-token" }),
        },
    )
    .await?;

    let removed = gw.admin().cleanup_auth_sessions().await?;
    assert_eq!(removed, 1);
    assert!(
        gw.admin()
            .get_auth_session_record(&stale_ready.session_id)
            .await?
            .is_none()
    );

    Ok(())
}

#[tokio::test]
async fn ready_session_is_single_use_and_provider_status_exposes_runtime_url() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;

    let init = gw.admin().init_oauth_session("codex", false).await?;
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("test-access-token".to_string()),
            refresh_token: Some("test-refresh-token".to_string()),
            expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
            resource_url: None,
            subject_id: Some("acct_test".to_string()),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            raw: json!({ "access_token": "test-access-token" }),
        },
    )
    .await?;

    let mut input = oauth_provider_input();
    input.vendor = None;
    input.preset_key = None;
    input.channel = None;
    input.protocol = "openai".to_string();
    input.base_url = "https://placeholder.invalid".to_string();
    let provider = gw
        .admin()
        .create_provider_with_oauth_session(&init.session_id, input)
        .await?;

    assert_eq!(provider.effective_auth_mode(), "oauth");
    assert_eq!(provider.vendor.as_deref(), Some("openai"));
    assert_eq!(provider.preset_key.as_deref(), Some("openai"));
    assert_eq!(provider.channel.as_deref(), Some("codex"));
    assert_eq!(provider.protocol, "openai-responses");
    assert_eq!(provider.base_url, CODEX_RUNTIME_URL);
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .is_none()
    );

    let err = gw
        .admin()
        .create_provider_with_oauth_session(&init.session_id, oauth_provider_input())
        .await
        .expect_err("consumed ready session should not be reusable");
    assert!(err.to_string().contains("auth session not found"));

    let status = gw.admin().get_provider_oauth_status(&provider.id).await?;
    assert_eq!(status.status, AuthBindingStatus::Connected.as_str());
    assert_eq!(status.resource_url.as_deref(), Some(CODEX_RUNTIME_URL));

    Ok(())
}

#[tokio::test]
async fn incompatible_bind_restores_ready_session() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = gw.admin().init_oauth_session("codex", false).await?;
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("test-access-token".to_string()),
            refresh_token: Some("test-refresh-token".to_string()),
            expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
            resource_url: Some(CODEX_RUNTIME_URL.to_string()),
            subject_id: Some("acct_test".to_string()),
            scopes: vec!["openid".to_string()],
            raw: json!({ "chatgpt_account_id": "acct_test" }),
        },
    )
    .await?;

    let mut incompatible = oauth_provider_input();
    incompatible.name = format!("api-key-provider-{}", Uuid::new_v4());
    incompatible.preset_key = None;
    incompatible.channel = None;
    incompatible.vendor = Some("custom".to_string());
    incompatible.auth_mode = "apikey".to_string();
    incompatible.api_key = "api-key".to_string();
    let provider = gw.admin().create_provider(incompatible).await?;

    let error = gw
        .admin()
        .bind_provider_with_oauth_session(&provider.id, &init.session_id)
        .await
        .expect_err("Codex session must not bind to an API-key provider");
    assert!(error.to_string().contains("not configured for OAuth"));
    let restored = gw
        .admin()
        .get_auth_session_record(&init.session_id)
        .await?
        .expect("failed bind must restore the consumed session");
    assert_eq!(restored.status, AuthSessionStatus::Ready.as_str());
    assert!(
        gw.storage
            .oauth_credentials()
            .get(&provider.id)
            .await?
            .is_none()
    );

    Ok(())
}

#[tokio::test]
async fn grok_oauth_session_creates_xai_grok_provider() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let init = gw.admin().init_oauth_session("xai", false).await?;
    assert!(
        init.auth_url.contains("auth.x.ai"),
        "unexpected grok auth url: {}",
        init.auth_url
    );
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("grok-access-token".to_string()),
            refresh_token: Some("grok-refresh-token".to_string()),
            expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
            resource_url: None,
            subject_id: Some("grok_user".to_string()),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            raw: json!({ "email": "user@example.com" }),
        },
    )
    .await?;

    let mut input = oauth_provider_input();
    input.vendor = None;
    input.preset_key = None;
    input.channel = None;
    input.protocol = "openai".to_string();
    input.base_url = "https://placeholder.invalid".to_string();
    let provider = gw
        .admin()
        .create_provider_with_oauth_session(&init.session_id, input)
        .await?;

    assert_eq!(provider.effective_auth_mode(), "oauth");
    assert_eq!(provider.vendor.as_deref(), Some("xai"));
    assert_eq!(provider.preset_key.as_deref(), Some("xai"));
    assert_eq!(provider.channel.as_deref(), Some("grok"));
    assert_eq!(provider.protocol, "openai-responses");
    assert_eq!(provider.base_url, GROK_RUNTIME_URL);

    let status = gw.admin().get_provider_oauth_status(&provider.id).await?;
    assert_eq!(status.status, AuthBindingStatus::Connected.as_str());
    assert_eq!(status.resource_url.as_deref(), Some(GROK_RUNTIME_URL));

    Ok(())
}

async fn build_gateway() -> anyhow::Result<Gateway> {
    let config = GatewayConfig {
        data_dir: test_data_dir(),
        ..Default::default()
    };
    let (gw, _log_rx) = Gateway::new(config).await?;
    Ok(gw)
}

fn test_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("nyro-oauth-admin-tests-{}", Uuid::new_v4()))
}

fn oauth_provider_input() -> CreateProvider {
    CreateProvider {
        keys: Vec::new(),
        name: format!("oauth-provider-{}", Uuid::new_v4()),
        vendor: None,
        protocol: "openai".to_string(),
        base_url: "https://placeholder.invalid".to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: Vec::new(),
        preset_key: Some("openai".to_string()),
        channel: Some("codex".to_string()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        auth_mode: "oauth".to_string(),
        use_proxy: false,
        fast_mode: false,
    }
}

async fn seed_ready_session(
    admin: &AdminService,
    session_id: &str,
    bundle: CredentialBundle,
) -> anyhow::Result<()> {
    admin
        .update_auth_session_record(
            session_id,
            UpdateAuthSession {
                status: Some(AuthSessionStatus::Ready.as_str().to_string()),
                result_json: Some(serde_json::to_string(&bundle)?),
                expires_at: bundle.expires_at.clone(),
                last_error: Some(String::new()),
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn cursor_completion_mode_is_poll_and_never_exposes_verifier() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = gw.admin().init_oauth_session("cursor", false).await?;
    assert_eq!(init.completion_mode, Some(auth::AuthCompletionMode::Poll));
    assert!(!init.requires_manual_code);
    assert!((295..=300).contains(&init.expires_in));
    let session = gw
        .admin()
        .get_auth_session_record(&init.session_id)
        .await?
        .unwrap();
    let state: Value = serde_json::from_str(session.state_json.as_deref().unwrap())?;
    let verifier = state["verifier"].as_str().unwrap();
    assert!(!serde_json::to_string(&init)?.contains(verifier));
    // Make the offline test independent of slow CI clock time: force the
    // server-side throttle so this test can never contact the real upstream.
    gw.auth_session_controls
        .lock()
        .await
        .get_mut(&init.session_id)
        .unwrap()
        .next_poll_at = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
    let pending = gw
        .admin()
        .get_oauth_session_status(&init.session_id)
        .await?;
    assert!(!serde_json::to_string(&pending)?.contains(verifier));
    assert!(matches!(
        pending,
        AuthSessionStatusData::Pending {
            completion_mode: Some(auth::AuthCompletionMode::Poll),
            requires_manual_code: false,
            ..
        }
    ));
    assert!(
        gw.admin()
            .complete_oauth_session(&init.session_id, AuthExchangeInput::default())
            .await
            .is_err()
    );
    // Removing mode mimics a legacy PKCE session; scheme fallback remains manual.
    gw.admin()
        .update_auth_session_record(
            &init.session_id,
            UpdateAuthSession {
                context_json: Some("{}".into()),
                ..Default::default()
            },
        )
        .await?;
    assert!(matches!(
        gw.admin()
            .get_oauth_session_status(&init.session_id)
            .await?,
        AuthSessionStatusData::Pending {
            completion_mode: Some(auth::AuthCompletionMode::ManualExchange),
            requires_manual_code: true,
            ..
        }
    ));
    Ok(())
}

#[tokio::test]
async fn cancelled_taken_session_cannot_be_restored() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let init = gw.admin().init_oauth_session("codex", false).await?;
    seed_ready_session(
        &gw.admin(),
        &init.session_id,
        CredentialBundle {
            access_token: Some("fake".into()),
            expires_at: Some(FAR_FUTURE_RFC3339.into()),
            ..Default::default()
        },
    )
    .await?;
    let taken = gw
        .admin()
        .take_ready_auth_session_record(&init.session_id)
        .await?;
    gw.admin().cancel_oauth_session(&init.session_id).await?;
    gw.admin().restore_auth_session_record(taken).await?;
    assert!(
        gw.admin()
            .get_auth_session_record(&init.session_id)
            .await?
            .is_none()
    );
    Ok(())
}

fn cursor_test_bundle(refresh: bool) -> CredentialBundle {
    use base64::Engine;
    let expires = Utc::now().timestamp() + 3600;
    let access = format!(
        "e30.{}.test-signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"sub":"auth0|test-cursor-user","exp":expires})).unwrap()
        )
    );
    CredentialBundle {
        access_token: Some(access),
        refresh_token: refresh.then(|| "fixture-refresh".into()),
        expires_at: Some(
            DateTime::<Utc>::from_timestamp(expires, 0)
                .unwrap()
                .to_rfc3339(),
        ),
        subject_id: Some("test-cursor-user".into()),
        resource_url: Some("https://api2.cursor.sh".into()),
        raw: json!({"label":"fixture"}),
        ..Default::default()
    }
}

#[tokio::test]
async fn cursor_session_canonical_provider_and_terminal_token_policy() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let admin = gw.admin();
    let init = admin.init_oauth_session("cursor", false).await?;
    seed_ready_session(&admin, &init.session_id, cursor_test_bundle(true)).await?;
    let provider = admin
        .create_provider_with_oauth_session(&init.session_id, oauth_provider_input())
        .await?;
    assert_eq!(provider.vendor.as_deref(), Some("cursor"));
    assert_eq!(provider.channel.as_deref(), Some("agent"));
    assert_eq!(provider.protocol, "cursor-agent/run/v1");
    assert_eq!(provider.base_url, "https://api2.cursor.sh");
    let store = gw.storage.oauth_credentials();
    let credential = store.get(&provider.id).await?.unwrap();
    let lease = store
        .try_begin_refresh(&provider.id, credential.status_version)
        .await?
        .unwrap();
    store
        .fail_refresh_with_policy(
            &provider.id,
            lease.status_version,
            "transient",
            false,
            Some(json!({"cursor_refresh_retry_at":Utc::now().timestamp()+300}).to_string()),
        )
        .await?;
    // Only typed transient failures leave connected state and permit old token.
    assert!(admin.resolve_provider_runtime(&provider).await.is_ok());
    let current = store.get(&provider.id).await?.unwrap();
    let lease = store
        .try_begin_refresh(&provider.id, current.status_version)
        .await?
        .unwrap();
    store
        .fail_refresh_with_policy(
            &provider.id,
            lease.status_version,
            "reauthentication required",
            true,
            None,
        )
        .await?;
    let error = admin
        .resolve_provider_runtime(&provider)
        .await
        .err()
        .expect("terminal Cursor credential must fail");
    assert!(error.to_string().contains("reauthentication"));
    Ok(())
}

#[tokio::test]
async fn cursor_missing_refresh_terminalizes_without_contacting_upstream() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let admin = gw.admin();
    let init = admin.init_oauth_session("cursor", false).await?;
    seed_ready_session(&admin, &init.session_id, cursor_test_bundle(false)).await?;
    let provider = admin
        .create_provider_with_oauth_session(&init.session_id, oauth_provider_input())
        .await?;
    let error = admin
        .reconnect_provider_oauth(&provider.id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reauthentication"));
    let status = admin.get_provider_oauth_status(&provider.id).await?;
    assert_eq!(status.status, "error");
    assert!(!status.has_refresh_token);
    Ok(())
}
