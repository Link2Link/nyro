//! Thin dispatch adapter: selection lives in the provider registry, credentials
//! in storage, and quota observations in the account administrative service.
use super::*;
use crate::db::models::OAuthCredential;
use crate::error::{AccountFailure, AccountFailureReason as Reason};
use crate::provider::account_pool::{
    ClaudeAccountState, DevinAccountState, KiroAccountState, account_cost_class_for_model,
    affinity_key, logical_session, supports_pool,
};
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct SelectedAccount {
    pub credential: OAuthCredential,
    pub affinity: Option<String>,
    pub config: crate::admin::DevinPoolConfig,
    /// Cost class at admission. Submission must not silently degrade from
    /// included allowance to paid overage while a better sibling exists.
    pub class: u8,
}

pub(super) async fn candidates(
    gw: &Gateway,
    provider: &Provider,
    model: &str,
    request: &AiRequest,
    principal: Option<&str>,
    excluded: &HashSet<String>,
) -> anyhow::Result<Option<Vec<SelectedAccount>>> {
    if !supports_pool(provider) {
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
    let mut ordered = gw.account_pool.ordered(
        &provider.id,
        model,
        &config,
        &accounts,
        affinity.as_deref(),
        excluded,
    );
    // Refresh only the admitted head account, never a provider-global token.
    // The returned snapshot pins the new generation before health/admission
    // checks. A failed refresh cools down that account and lets a healthy
    // sibling take its place. If the refreshed head's cost class is worse than
    // its ranking assumed (for example a concurrent usage observation moved it
    // from included allowance to paid overage), rank again from current state
    // so a better sibling is not skipped. Each pass consumes one account, so
    // this terminates within the pool size.
    if crate::provider::kiro::is_kiro(provider)
        || crate::provider::anthropic::claude_code::egress::is_claude_code_channel(provider)
    {
        let mut remaining = ordered.len();
        while remaining > 0 && !ordered.is_empty() {
            remaining -= 1;
            let head = ordered.remove(0);
            let expected = account_cost_class_for_model(&head, &config, model);
            let refreshed = if crate::provider::kiro::is_kiro(provider) {
                gw.admin().refresh_kiro_pool_account(provider, &head).await
            } else {
                gw.admin()
                    .refresh_claude_pool_account(provider, &head)
                    .await
            };
            match refreshed {
                Ok(current)
                    if account_cost_class_for_model(&current, &config, model) == expected =>
                {
                    ordered.insert(0, current);
                    break;
                }
                Ok(current) => {
                    let mut snapshot = ordered;
                    snapshot.push(current);
                    ordered = gw.account_pool.ordered(
                        &provider.id,
                        model,
                        &config,
                        &snapshot,
                        affinity.as_deref(),
                        excluded,
                    );
                }
                Err(_) => {
                    gw.account_pool.cooldown(
                        &head.account_id,
                        head.status_version,
                        model,
                        Some(30),
                    );
                }
            }
        }
    }
    Ok(Some(
        ordered
            .into_iter()
            .filter_map(|credential| {
                let class = account_cost_class_for_model(&credential, &config, model)?;
                Some(SelectedAccount {
                    credential,
                    affinity: affinity.clone(),
                    config: config.clone(),
                    class,
                })
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
    // A worse current class (e.g. included -> paid overage) is not silently
    // accepted: the dispatcher re-selects so a better sibling wins first.
    matches!(gw.storage.oauth_credentials().get_account(&account.provider_id, &account.account_id).await,
        Ok(Some(current)) if current.status_version == account.status_version
            && current.is_enabled && current.status == "connected"
            && account_cost_class_for_model(&current, &candidate.config, model)
                .is_some_and(|class| class <= candidate.class))
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
        Reason::AuthInvalid | Reason::DailyQuota | Reason::WeeklyQuota | Reason::MonthlyQuota => {
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
                let reason = match failure.reason {
                    Reason::DailyQuota => "daily_quota",
                    Reason::MonthlyQuota => "monthly_quota",
                    _ => "weekly_quota",
                };
                let raw = if account.driver_key == "kiro" {
                    let mut state = KiroAccountState::parse(&current.account_state);
                    state.rejection_revision = state.rejection_revision.saturating_add(1);
                    if failure.reason == Reason::AuthInvalid {
                        state.auth_invalid = true;
                    } else {
                        state.hard_quota = true;
                        state.hard_quota_reason = Some(reason.into());
                    }
                    serde_json::to_string(&state)
                } else if matches!(account.driver_key.as_str(), "claude-code" | "claude") {
                    let mut state = ClaudeAccountState::parse(&current.account_state);
                    state.rejection_revision = state.rejection_revision.saturating_add(1);
                    if failure.reason == Reason::AuthInvalid {
                        state.auth_invalid = true;
                    } else {
                        state.hard_quota = true;
                        state.hard_quota_reason = Some(reason.into());
                    }
                    serde_json::to_string(&state)
                } else {
                    let mut state = DevinAccountState::parse(&current.account_state);
                    state.rejection_revision = state.rejection_revision.saturating_add(1);
                    if failure.reason == Reason::AuthInvalid {
                        state.auth_invalid = true;
                    } else {
                        state.hard_quota = true;
                        state.hard_quota_reason = Some(reason.into());
                    }
                    serde_json::to_string(&state)
                };
                let Ok(raw) = raw else {
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
    async fn legacy_pro_pool_admits_max_and_rechecks_dynamic_weekly_exhaustion() {
        let (gw, provider, accounts) = fixture().await;
        let max = &accounts[1];
        let mut state = DevinAccountState::parse(&max.account_state);
        state.usage.as_mut().unwrap().plan_name = Some("Max".into());
        state
            .usage
            .as_mut()
            .unwrap()
            .daily
            .as_mut()
            .unwrap()
            .remaining_percent = 0;
        state.plan_mismatch = true;
        let raw = serde_json::to_string(&state).unwrap();
        assert!(
            gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "pool",
                    &max.account_id,
                    max.status_version,
                    &max.account_state,
                    &raw
                )
                .await
                .unwrap()
        );
        let request = AiRequest::new("swe-2", vec![]);
        let all = candidates(&gw, &provider, "swe-2", &request, None, &HashSet::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(all.len(), 2);
        let candidate = all
            .iter()
            .find(|a| a.credential.account_id == max.account_id)
            .unwrap();
        assert!(candidate.config.plan.is_none());
        assert!(still_valid(&gw, candidate, "swe-2").await);
        let mut exhausted = DevinAccountState::parse(&raw);
        exhausted
            .usage
            .as_mut()
            .unwrap()
            .weekly
            .as_mut()
            .unwrap()
            .remaining_percent = 0;
        assert!(
            gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "pool",
                    &max.account_id,
                    max.status_version,
                    &raw,
                    &serde_json::to_string(&exhausted).unwrap()
                )
                .await
                .unwrap()
        );
        assert!(!still_valid(&gw, candidate, "swe-2").await);
        let remaining = candidates(&gw, &provider, "swe-2", &request, None, &HashSet::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].credential.account_id, accounts[0].account_id);
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
    async fn kiro_fixture() -> (Gateway, Provider, Vec<OAuthCredential>) {
        let provider: Provider = serde_json::from_value(serde_json::json!({
            "id":"kiro-pool","name":"Kiro pool","vendor":"kiro","preset_key":"kiro",
            "channel":"ide","protocol":"kiro-chat/generate-assistant-response/v1",
            "base_url":"https://q.us-east-1.amazonaws.com","api_key":"","auth_mode":"oauth",
            "last_test_success":null,"last_test_at":null,"is_enabled":false,
            "created_at":"","updated_at":""
        }))
        .unwrap();
        let mut provider = provider;
        provider.protocol = crate::provider::kiro::PROTOCOL_KEY.into();
        let storage = Arc::new(crate::storage::MemoryStorage::new(
            vec![provider.clone()],
            vec![],
            vec![],
        ));
        let (gw, _) = Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
            .await
            .unwrap();
        gw.admin()
            .configure_provider_account_pool(&provider.id, Default::default())
            .await
            .unwrap();
        let mut accounts = Vec::new();
        for name in ["a", "b"] {
            accounts.push(
                gw.storage
                    .oauth_credentials()
                    .create_account(
                        &provider.id,
                        name,
                        UpsertOAuthCredential {
                            driver_key: "kiro".into(),
                            scheme: "manual".into(),
                            access_token: format!("ksk_fixture_{name}"),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap(),
            );
        }
        (gw, provider, accounts)
    }

    #[tokio::test]
    async fn kiro_pool_feedback_isolates_accounts_and_old_credential_generations() {
        let (gw, provider, _) = kiro_fixture().await;
        let request = AiRequest::new("claude-sonnet-4.5", vec![]);
        let selected = candidates(
            &gw,
            &provider,
            &request.model,
            &request,
            None,
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            selected.len(),
            2,
            "unknown usage is not a Devin plan requirement"
        );
        let first = &selected[0];
        let second = &selected[1];
        let runtime = gw
            .admin()
            .resolve_provider_account_runtime(&provider, &first.credential)
            .await
            .unwrap();
        assert_eq!(runtime.access_token, first.credential.access_token);
        feedback(
            &gw,
            first,
            &request.model,
            AccountFailure {
                reason: Reason::MonthlyQuota,
                rejected_before_execution: true,
                retry_after_seconds: None,
            },
        )
        .await;
        assert!(!still_valid(&gw, first, &request.model).await);
        assert!(still_valid(&gw, second, &request.model).await);
        let remaining = candidates(
            &gw,
            &provider,
            &request.model,
            &request,
            None,
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].credential.account_id,
            second.credential.account_id
        );
        let replaced = gw
            .storage
            .oauth_credentials()
            .replace_account(
                &provider.id,
                &first.credential.account_id,
                first.credential.status_version,
                UpsertOAuthCredential {
                    driver_key: "kiro".into(),
                    scheme: "manual".into(),
                    access_token: "ksk_reauthorized".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        feedback(
            &gw,
            first,
            &request.model,
            AccountFailure {
                reason: Reason::AuthInvalid,
                rejected_before_execution: false,
                retry_after_seconds: None,
            },
        )
        .await;
        let current = gw
            .storage
            .oauth_credentials()
            .get_account(&provider.id, &replaced.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!KiroAccountState::parse(&current.account_state).auth_invalid);
        assert_eq!(current.access_token, "ksk_reauthorized");
    }

    #[tokio::test]
    async fn kiro_failed_expired_credential_does_not_hide_healthy_sibling() {
        let (gw, provider, accounts) = kiro_fixture().await;
        let first = &accounts[0];
        let expired = gw
            .storage
            .oauth_credentials()
            .replace_account(
                &provider.id,
                &first.account_id,
                first.status_version,
                UpsertOAuthCredential {
                    driver_key: "kiro".into(),
                    scheme: "manual".into(),
                    access_token: "expired".into(),
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        gw.storage
            .oauth_credentials()
            .update_account(
                &provider.id,
                &expired.account_id,
                expired.status_version,
                crate::db::models::OAuthAccountUpdate {
                    priority: Some(-1),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        let selected = candidates(
            &gw,
            &provider,
            "m",
            &AiRequest::new("m", vec![]),
            None,
            &HashSet::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].credential.account_id, accounts[1].account_id);
    }

    fn kiro_usage_state(used: f64, limit: f64, overage: bool) -> String {
        let usage = crate::provider::kiro::usage::parse(&serde_json::json!({
            "overageConfiguration":{"overageEnabled":overage},
            "usageBreakdownList":[{"resourceType":"CREDITS","currentUsage":used,"usageLimit":limit}]
        }));
        serde_json::to_string(&KiroAccountState {
            usage: Some(usage),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn kiro_paid_overage_never_jumps_ahead_of_an_included_sibling() {
        let (gw, provider, accounts) = kiro_fixture().await;
        let config = crate::admin::DevinPoolConfig {
            reserve_policy: crate::admin::DevinReservePolicy::Fallback,
            ..Default::default()
        };
        gw.admin()
            .configure_provider_account_pool(&provider.id, config)
            .await
            .unwrap();
        let store = gw.storage.oauth_credentials();
        for account in &accounts {
            let current = store
                .get_account(&provider.id, &account.account_id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                store
                    .compare_account_state(
                        &provider.id,
                        &account.account_id,
                        current.status_version,
                        &current.account_state,
                        &kiro_usage_state(10.0, 100.0, true)
                    )
                    .await
                    .unwrap()
            );
        }
        let request = AiRequest::new("m", vec![]);
        let selected = candidates(&gw, &provider, "m", &request, None, &HashSet::new())
            .await
            .unwrap()
            .unwrap();
        let head = &selected[0];
        assert_eq!(head.class, 0);
        assert!(still_valid(&gw, head, "m").await);
        // A concurrent usage observation exhausts the head's included
        // allowance after admission: it is now paid overage only.
        let current = store
            .get_account(&provider.id, &head.credential.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .compare_account_state(
                    &provider.id,
                    &head.credential.account_id,
                    current.status_version,
                    &current.account_state,
                    &kiro_usage_state(100.0, 100.0, true)
                )
                .await
                .unwrap()
        );
        assert!(
            !still_valid(&gw, head, "m").await,
            "degraded head must not be submitted"
        );
        let reselected = candidates(&gw, &provider, "m", &request, None, &HashSet::new())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            reselected[0].credential.account_id,
            head.credential.account_id
        );
        assert_eq!(reselected[0].class, 0, "included sibling wins");
        assert_eq!(
            reselected.last().unwrap().class,
            2,
            "overage stays a last resort"
        );
    }

    #[tokio::test]
    async fn kiro_refreshing_account_is_still_a_pool_member() {
        let (gw, provider, accounts) = kiro_fixture().await;
        let store = gw.storage.oauth_credentials();
        let lease = store
            .try_begin_account_refresh(
                &provider.id,
                &accounts[0].account_id,
                accounts[0].status_version,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lease.status, "refreshing");
        let listed = store.list_accounts(&provider.id).await.unwrap();
        let ordered = gw.account_pool.ordered(
            &provider.id,
            "m",
            &Default::default(),
            &listed,
            None,
            &HashSet::new(),
        );
        assert!(
            ordered
                .iter()
                .any(|a| a.account_id == accounts[0].account_id),
            "a routine token refresh must not empty a single-account pool"
        );
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
