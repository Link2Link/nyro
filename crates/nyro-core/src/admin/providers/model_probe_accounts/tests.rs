use super::*;
use serde_json::json;

fn provider(kind: &str, id: &str) -> Provider {
    let (vendor, channel, protocol, base) = match kind {
        "kiro" => (
            "kiro",
            "ide",
            "kiro-chat",
            "https://q.us-east-1.amazonaws.com",
        ),
        "devin" => ("devin", "cli", "devin-chat", "https://server.codeium.com"),
        _ => (
            "anthropic",
            "claude-code",
            "anthropic-messages",
            "https://api.anthropic.com",
        ),
    };
    serde_json::from_value(
        json!({"id":id,"name":kind,"vendor":vendor,"channel":channel,
        "protocol":protocol,"base_url":base,"protocol_mode":"fixed","auth_mode":"oauth",
        "api_key":"","is_enabled":true,"created_at":"","updated_at":""}),
    )
    .unwrap()
}

async fn fixture(kind: &str) -> (Gateway, Provider, OAuthCredential, OAuthCredential) {
    let p = provider(kind, "pool");
    let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
        vec![p.clone(), provider(kind, "other")],
        vec![],
        vec![],
    ));
    let (gw, _) = Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
        .await
        .unwrap();
    let store = gw.storage.oauth_credentials();
    let mut accounts = Vec::new();
    for (parent, label) in [("pool", "A"), ("other", "Foreign")] {
        accounts.push(
            store
                .create_account(
                    parent,
                    label,
                    UpsertOAuthCredential {
                        driver_key: driver_for(&p).into(),
                        scheme: "oauth_auth_code_pkce".into(),
                        access_token: format!("SECRET-{label}"),
                        expires_at: Some("2099-01-01T00:00:00Z".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    (gw, p, accounts.remove(0), accounts.remove(0))
}

#[test]
fn account_id_validation_never_converts_invalid_selection_to_auto() {
    assert_eq!(normalize_probe_account_id(None).unwrap(), None);
    assert_eq!(
        normalize_probe_account_id(Some(" A ".into()))
            .unwrap()
            .as_deref(),
        Some("A")
    );
    for id in ["".into(), " \n".into(), "A\0B".into(), "x".repeat(257)] {
        assert!(
            normalize_probe_account_id(Some(id))
                .unwrap_err()
                .downcast_ref::<ModelProbeAccountError>()
                .is_some()
        );
    }
}

#[tokio::test]
async fn explicit_probe_rejects_missing_foreign_disabled_and_disconnected_accounts() {
    for kind in ["devin", "kiro", "claude-code"] {
        let (gw, p, a, foreign) = fixture(kind).await;
        let admin = gw.admin();
        for id in ["missing", foreign.account_id.as_str()] {
            let error = admin
                .probe_provider_models_with_account(
                    &p.id,
                    Some(vec!["model".into()]),
                    None,
                    Some(id.into()),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.downcast_ref::<ModelProbeAccountError>(),
                    Some(ModelProbeAccountError::NotFound)
                ),
                "{kind}: {error}"
            );
        }
        let a = gw
            .storage
            .oauth_credentials()
            .update_account(
                &p.id,
                &a.account_id,
                a.status_version,
                OAuthAccountUpdate {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        let error = admin
            .probe_provider_models_with_account(
                &p.id,
                Some(vec!["model".into()]),
                None,
                Some(a.account_id.clone()),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ModelProbeAccountError>(),
            Some(ModelProbeAccountError::Unavailable)
        ));
        gw.storage
            .oauth_credentials()
            .disconnect_account(&p.id, &a.account_id, a.status_version)
            .await
            .unwrap();
        assert!(
            admin
                .probe_provider_models_with_account(
                    &p.id,
                    Some(vec!["model".into()]),
                    None,
                    Some(a.account_id)
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn selected_account_preflight_fences_generation_state_and_policy_changes() {
    for kind in ["devin", "kiro", "claude-code"] {
        let (gw, p, a, _) = fixture(kind).await;
        let admin = gw.admin();
        let config = DevinPoolConfig::default();
        let store = gw.storage.oauth_credentials();
        let state = if kind == "devin" {
            r#"{"verified_plan":"pro"}"#
        } else {
            "{}"
        };
        assert!(
            store
                .compare_account_state(
                    &p.id,
                    &a.account_id,
                    a.status_version,
                    &a.account_state,
                    state
                )
                .await
                .unwrap()
        );
        let a = store
            .get_account(&p.id, &a.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            admin
                .check_probe_account(&p, &a, &config, "claude-sonnet-5-5")
                .await
                .is_ok()
        );
        // Quota state updates intentionally do not advance credential generation.
        assert!(
            store
                .compare_account_state(
                    &p.id,
                    &a.account_id,
                    a.status_version,
                    &a.account_state,
                    r#"{"hard_quota":true}"#
                )
                .await
                .unwrap()
        );
        assert!(
            admin
                .check_probe_account(&p, &a, &config, "claude-sonnet-5-5")
                .await
                .is_err()
        );
        assert!(
            store
                .compare_account_state(
                    &p.id,
                    &a.account_id,
                    a.status_version,
                    r#"{"hard_quota":true}"#,
                    state
                )
                .await
                .unwrap()
        );
        let current = store
            .update_account(
                &p.id,
                &a.account_id,
                a.status_version,
                OAuthAccountUpdate {
                    label: Some("New generation".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            admin
                .check_probe_account(&p, &a, &config, "model")
                .await
                .unwrap_err()
                .downcast_ref::<ModelProbeAccountError>(),
            Some(ModelProbeAccountError::Changed)
        ));
        admin
            .configure_provider_account_pool(
                &p.id,
                DevinPoolConfig {
                    reserve_policy: DevinReservePolicy::Fallback,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            admin
                .check_probe_account(&p, &current, &config, "model")
                .await
                .unwrap_err()
                .downcast_ref::<ModelProbeAccountError>(),
            Some(ModelProbeAccountError::Changed)
        ));
    }
}

#[tokio::test]
async fn selected_probe_preserves_hard_blocks_and_unknown_usage_rules() {
    for kind in ["devin", "kiro", "claude-code"] {
        let (_gw, p, mut a, _) = fixture(kind).await;
        let config = DevinPoolConfig::default();
        for flag in ["auth_invalid", "hard_quota"] {
            a.account_state = json!({flag:true,"verified_plan":"pro"}).to_string();
            assert!(!eligible(&p, &a, &config, "claude-sonnet-5-5"));
        }
        a.account_state = "{}".into();
        assert_eq!(eligible(&p, &a, &config, "model"), kind != "devin");
        a.account_state = r#"{"verified_plan":"pro"}"#.into();
        assert!(eligible(&p, &a, &config, "model"));
        a.driver_key = "foreign".into();
        assert!(validate_account(&p, &a, true).is_err());
        a.driver_key.clear();
        assert!(validate_account(&p, &a, true).is_ok());
        a.status = "refreshing".into();
        assert_eq!(validate_account(&p, &a, true).is_ok(), kind != "devin");
        assert!(validate_account(&p, &a, false).is_err());
    }
}

#[tokio::test]
async fn explicit_probe_rejects_unsupported_provider_and_hard_blocks_before_refresh() {
    let mut unsupported = provider("claude-code", "api");
    unsupported.auth_mode = "apikey".into();
    unsupported.api_key = "SECRET-api".into();
    let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
        vec![unsupported],
        vec![],
        vec![],
    ));
    let (gw, _) = Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
        .await
        .unwrap();
    let error = gw
        .admin()
        .probe_provider_models_with_account(
            "api",
            Some(vec!["model".into()]),
            None,
            Some("account".into()),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ModelProbeAccountError>(),
        Some(ModelProbeAccountError::Unsupported)
    ));
    for kind in ["devin", "kiro", "claude-code"] {
        let (gw, p, account, _) = fixture(kind).await;
        let store = gw.storage.oauth_credentials();
        assert!(
            store
                .compare_account_state(
                    &p.id,
                    &account.account_id,
                    account.status_version,
                    &account.account_state,
                    r#"{"auth_invalid":true,"hard_quota":true}"#
                )
                .await
                .unwrap()
        );
        let error = gw
            .admin()
            .probe_provider_models_with_account(
                &p.id,
                Some(vec!["model".into()]),
                None,
                Some(account.account_id),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ModelProbeAccountError>(),
            Some(ModelProbeAccountError::Unavailable)
        ));
        assert!(!error.to_string().contains("SECRET"));
    }
}

#[test]
fn account_probe_errors_never_echo_upstream_secrets() {
    for message in [
        "HTTP 401 SECRET-token",
        "HTTP 429 SECRET-refresh",
        "timeout after 30s SECRET",
        "https://user:SECRET@upstream.invalid",
        "SECRET raw RPC failure",
    ] {
        assert!(!safe_account_probe_error(message).contains("SECRET"));
    }
}
