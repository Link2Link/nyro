//! Thin dispatch adapter: selection lives in the provider registry, credentials
//! in storage, and quota observations in the account administrative service.
use super::*;
use crate::db::models::OAuthCredential;
use crate::error::{AccountFailure, AccountFailureReason as Reason};
use crate::provider::account_pool::{DevinAccountState, affinity_key, logical_session};
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct SelectedAccount {
    pub credential: OAuthCredential,
    pub affinity: Option<String>,
    pub config: crate::admin::DevinPoolConfig,
}

pub(super) async fn candidates(
    gw: &Gateway,
    provider: &Provider,
    model: &str,
    request: &AiRequest,
    principal: Option<&str>,
    excluded: &HashSet<String>,
) -> anyhow::Result<Option<Vec<SelectedAccount>>> {
    if !crate::provider::devin::is_devin(provider) {
        return Ok(None);
    }
    let Some(config) = gw.admin().provider_pool_config(&provider.id).await? else {
        return Ok(None);
    };
    let accounts = gw
        .storage
        .oauth_credentials()
        .list_accounts(&provider.id)
        .await?;
    let affinity =
        logical_session(request).map(|session| affinity_key(principal, &provider.id, &session));
    let accounts = gw.account_pool.ordered(
        &provider.id,
        model,
        &config,
        &accounts,
        affinity.as_deref(),
        excluded,
    );
    Ok(Some(
        accounts
            .into_iter()
            .map(|credential| SelectedAccount {
                credential,
                affinity: affinity.clone(),
                config: config.clone(),
            })
            .collect(),
    ))
}

pub(super) async fn still_valid(gw: &Gateway, candidate: &SelectedAccount, model: &str) -> bool {
    let account = &candidate.credential;
    if !gw
        .account_pool
        .is_available(&account.account_id, account.status_version, model)
    {
        return false;
    }
    // Queued siblings cannot use a superseded reserve/priority policy.
    if !matches!(gw.admin().provider_pool_config(&account.provider_id).await,
        Ok(Some(current)) if current == candidate.config)
    {
        return false;
    }
    matches!(gw.storage.oauth_credentials().get_account(&account.provider_id, &account.account_id).await,
        Ok(Some(current)) if current.status_version == account.status_version
            && current.is_enabled && current.status == "connected"
            && DevinAccountState::parse(&current.account_state).cost_class(&candidate.config).is_some())
}

pub(super) async fn feedback(
    gw: &Gateway,
    candidate: &SelectedAccount,
    model: &str,
    failure: AccountFailure,
) {
    let account = &candidate.credential;
    let Ok(Some(current)) = gw
        .storage
        .oauth_credentials()
        .get_account(&account.provider_id, &account.account_id)
        .await
    else {
        return;
    };
    if current.status_version != account.status_version {
        return;
    }
    gw.account_pool
        .release_generation(&account.account_id, account.status_version);
    match failure.reason {
        Reason::AuthInvalid | Reason::DailyQuota | Reason::WeeklyQuota => {
            // CAS the original generation, but merge into the latest same-generation
            // state: an intervening successful usage refresh cannot hide this newer
            // rejection. A concurrent reauthorization always fences this write.
            for _ in 0..3 {
                let Ok(Some(current)) = gw
                    .storage
                    .oauth_credentials()
                    .get_account(&account.provider_id, &account.account_id)
                    .await
                else {
                    return;
                };
                if current.status_version != account.status_version {
                    return;
                }
                let mut state = DevinAccountState::parse(&current.account_state);
                state.rejection_revision = state.rejection_revision.saturating_add(1);
                if failure.reason == Reason::AuthInvalid {
                    state.auth_invalid = true;
                } else {
                    state.hard_quota = true;
                    state.hard_quota_reason = Some(
                        match failure.reason {
                            Reason::DailyQuota => "daily_quota",
                            _ => "weekly_quota",
                        }
                        .into(),
                    );
                }
                let Ok(raw) = serde_json::to_string(&state) else {
                    return;
                };
                match gw
                    .storage
                    .oauth_credentials()
                    .compare_account_state(
                        &account.provider_id,
                        &account.account_id,
                        account.status_version,
                        &current.account_state,
                        &raw,
                    )
                    .await
                {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::warn!(account_id=%account.account_id, error=%error, "account scheduling feedback could not persist");
                        break;
                    }
                }
            }
            gw.account_quota_registry
                .request_refresh(&account.account_id);
            gw.quota_registry.request_refresh(&account.provider_id);
        }
        Reason::RateLimited | Reason::Capacity | Reason::ModelForbidden => {
            gw.account_pool.cooldown(
                &account.account_id,
                account.status_version,
                model,
                failure.retry_after_seconds,
            );
            if failure.reason == Reason::RateLimited {
                gw.account_quota_registry
                    .request_refresh(&account.account_id);
                gw.quota_registry.request_refresh(&account.provider_id);
            }
        }
        _ => {}
    }
}

/// Feedback must also run for errors after the client stream has committed.
/// Returning the same event preserves the original conversion/terminal contract.
pub(super) fn observe_stream(
    mut upstream: crate::provider::native::IrUpstream,
    gw: &Gateway,
    candidate: &SelectedAccount,
    model: &str,
) -> crate::provider::native::IrUpstream {
    let gw = gw.clone();
    let candidate = candidate.clone();
    let model = model.to_owned();
    upstream.events = Box::pin(upstream.events.then(move |event| {
        let gw = gw.clone();
        let candidate = candidate.clone();
        let model = model.clone();
        async move {
            if let Err(error) = &event {
                if let Some(failure) = error.account_failure() {
                    feedback(&gw, &candidate, &model, failure).await;
                }
            }
            event
        }
    }));
    upstream
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{DevinPlan, DevinPoolConfig};
    use crate::db::models::UpsertOAuthCredential;
    use crate::provider::devin::{DevinQuota, DevinUsage};

    async fn fixture() -> (Gateway, Provider, Vec<OAuthCredential>) {
        let provider: Provider = serde_json::from_value(serde_json::json!({
            "id":"pool","name":"Devin pool","vendor":"devin","preset_key":"devin",
            "channel":"cli","protocol":"devin-chat/get-chat-message/v1",
            "base_url":"https://server.codeium.com","api_key":"","auth_mode":"oauth",
            "last_test_success":null,"last_test_at":null,"is_enabled":false,
            "created_at":"","updated_at":""
        }))
        .unwrap();
        // Disabled Provider keeps the background usage monitor off the network;
        // these tests exercise account admission and feedback directly.
        let storage = Arc::new(crate::storage::MemoryStorage::new(
            vec![provider.clone()],
            vec![],
            vec![],
        ));
        let (gw, _logs) = Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
            .await
            .unwrap();
        let config: DevinPoolConfig = serde_json::from_str(r#"{"plan":"pro"}"#).unwrap();
        gw.admin()
            .configure_provider_account_pool("pool", config)
            .await
            .unwrap();
        let mut accounts = Vec::new();
        for name in ["a", "b"] {
            let mut account = gw
                .storage
                .oauth_credentials()
                .create_account(
                    "pool",
                    name,
                    UpsertOAuthCredential {
                        driver_key: "devin".into(),
                        scheme: "oauth_auth_code_pkce".into(),
                        access_token: format!("fixture-{name}"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let state = DevinAccountState {
                verified_plan: Some(DevinPlan::Pro),
                usage: Some(DevinUsage {
                    plan_name: Some("Pro".into()),
                    daily: Some(DevinQuota {
                        remaining_percent: 80,
                        resets_at_unix: None,
                    }),
                    weekly: Some(DevinQuota {
                        remaining_percent: 80,
                        resets_at_unix: None,
                    }),
                    overage_usd: None,
                }),
                ..Default::default()
            };
            let raw = serde_json::to_string(&state).unwrap();
            assert!(
                gw.storage
                    .oauth_credentials()
                    .compare_account_state(
                        "pool",
                        &account.account_id,
                        account.status_version,
                        &account.account_state,
                        &raw
                    )
                    .await
                    .unwrap()
            );
            account.account_state = raw;
            accounts.push(account);
        }
        (gw, provider, accounts)
    }
    #[tokio::test]
    async fn quota_feedback_is_account_scoped_and_stale_generation_cannot_revoke_reauth() {
        let (gw, provider, accounts) = fixture().await;
        let request = AiRequest::new("swe-2", vec![]);
        let selected = candidates(
            &gw,
            &provider,
            "swe-2",
            &request,
            Some("caller"),
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        let a = &selected[0];
        let b = &selected[1];
        feedback(
            &gw,
            a,
            "swe-2",
            AccountFailure {
                reason: Reason::WeeklyQuota,
                rejected_before_execution: true,
                retry_after_seconds: None,
            },
        )
        .await;
        let remaining = candidates(
            &gw,
            &provider,
            "swe-2",
            &request,
            Some("caller"),
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].credential.account_id, b.credential.account_id);
        let current = gw
            .storage
            .oauth_credentials()
            .get_account("pool", &a.credential.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(DevinAccountState::parse(&current.account_state).hard_quota);
        feedback(
            &gw,
            a,
            "swe-2",
            AccountFailure {
                reason: Reason::WeeklyQuota,
                rejected_before_execution: true,
                retry_after_seconds: None,
            },
        )
        .await;
        let repeated = gw
            .storage
            .oauth_credentials()
            .get_account("pool", &a.credential.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            DevinAccountState::parse(&repeated.account_state).rejection_revision
                > DevinAccountState::parse(&current.account_state).rejection_revision
        );
        assert!(
            !gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "pool",
                    &a.credential.account_id,
                    a.credential.status_version,
                    &current.account_state,
                    &a.credential.account_state
                )
                .await
                .unwrap()
        );
        assert!(
            !gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "pool",
                    &a.credential.account_id,
                    a.credential.status_version,
                    &a.credential.account_state,
                    &a.credential.account_state
                )
                .await
                .unwrap()
        );
        let fresh = gw
            .storage
            .oauth_credentials()
            .replace_account(
                "pool",
                &a.credential.account_id,
                a.credential.status_version,
                UpsertOAuthCredential {
                    driver_key: "devin".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: "new-fixture-token".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        feedback(
            &gw,
            a,
            "swe-2",
            AccountFailure {
                reason: Reason::AuthInvalid,
                rejected_before_execution: true,
                retry_after_seconds: None,
            },
        )
        .await;
        let final_row = gw
            .storage
            .oauth_credentials()
            .get_account("pool", &fresh.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!DevinAccountState::parse(&final_row.account_state).auth_invalid);
        feedback(
            &gw,
            a,
            "swe-2",
            AccountFailure {
                reason: Reason::RateLimited,
                rejected_before_execution: true,
                retry_after_seconds: Some(300),
            },
        )
        .await;
        assert!(
            gw.account_pool
                .is_available(&fresh.account_id, fresh.status_version, "swe-2")
        );
        assert_eq!(accounts.len(), 2);
    }
    #[tokio::test]
    async fn queued_candidates_observe_new_policy_and_model_cooldown() {
        let (gw, provider, _) = fixture().await;
        let selected = candidates(
            &gw,
            &provider,
            "swe-2",
            &AiRequest::new("swe-2", vec![]),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        let a = &selected[0];
        assert!(still_valid(&gw, a, "swe-2").await);
        gw.account_pool.cooldown(
            &a.credential.account_id,
            a.credential.status_version,
            "swe-2",
            Some(60),
        );
        assert!(!still_valid(&gw, a, "swe-2").await);
        assert!(still_valid(&gw, a, "other-model").await);
        let mut config = a.config.clone();
        config.session_affinity = false;
        gw.admin()
            .configure_provider_account_pool("pool", config)
            .await
            .unwrap();
        assert!(!still_valid(&gw, a, "other-model").await);
    }
}
