//! Kiro-specific observations behind the shared account administration API.
use super::*;
use crate::provider::account_pool::KiroAccountState;
use crate::provider::kiro::KiroUsage;
use crate::router::quota::QuotaTierObservation;

pub(super) fn kiro_scheduling(
    account: &OAuthCredential,
    config: Option<&DevinPoolConfig>,
) -> &'static str {
    let state = KiroAccountState::parse(&account.account_state);
    if account.status == "disconnected" {
        return "disconnected";
    }
    if !account.is_enabled {
        return "disabled";
    }
    // A routine token refresh is transient, not a reauthorization requirement.
    if !matches!(account.status.as_str(), "connected" | "refreshing")
        || account.access_token.trim().is_empty()
        || state.auth_invalid
    {
        return "auth_required";
    }
    // Singleton Kiro historically honored upstream overage opt-in. Configured
    // pools additionally require the operator's explicit fallback policy.
    let fallback = DevinPoolConfig {
        reserve_policy: DevinReservePolicy::Fallback,
        ..Default::default()
    };
    match state.cost_class(config.unwrap_or(&fallback)) {
        Some(0) => "included",
        Some(1) => "unknown",
        Some(2) => "reserve",
        _ => "quota_exhausted",
    }
}

pub(super) fn safe_kiro_account(
    account: &OAuthCredential,
    config: Option<&DevinPoolConfig>,
) -> ProviderAccountView {
    let state = KiroAccountState::parse(&account.account_state);
    ProviderAccountView {
        account_id: account.account_id.clone(),
        provider_id: account.provider_id.clone(),
        label: account.label.clone(),
        is_enabled: account.is_enabled,
        priority: account.priority,
        status: account.status.clone(),
        status_version: account.status_version,
        verified_plan: None,
        identity_verified: account
            .subject_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty()),
        scheduling: kiro_scheduling(account, config).into(),
        daily: None,
        weekly: None,
        reserve_usd: None,
        // Typed usage contains no credentials or opaque upstream diagnostics.
        kiro_usage: state.usage,
        observed_at: state.observed_at,
        stale: state.usage_stale
            || state
                .observed_at
                .is_none_or(|at| Utc::now().timestamp_millis().saturating_sub(at) > 300_000),
        updated_at: account.updated_at.clone(),
    }
}

pub(super) fn authenticated_kiro_state(
    usage: KiroUsage,
    previous: Option<&KiroAccountState>,
) -> KiroAccountState {
    let mut fresh = KiroAccountState {
        usage: Some(usage),
        observed_at: Some(Utc::now().timestamp_millis()),
        ..Default::default()
    };
    if let Some(previous) = previous {
        let blocked = fresh.included_exhausted()
            || previous.included_exhausted()
            || (previous.hard_quota
                && previous.hard_quota_reason.as_deref() != Some("reauthorization_verification"));
        let pools = fresh.usage.as_ref().unwrap().resource_pools();
        if let Some(old) = &previous.usage
            && (pools.is_empty()
                || old
                    .resource_pools()
                    .iter()
                    .any(|resource| !pools.iter().any(|pool| pool.resource == resource.resource)))
        {
            fresh.usage = Some(old.clone());
            fresh.observed_at = previous.observed_at;
            fresh.usage_stale = true;
        }
        fresh.hard_quota = true;
        fresh.hard_quota_reason = Some(
            if blocked {
                "quota_reauthorization_verification"
            } else {
                "reauthorization_verification"
            }
            .into(),
        );
    }
    fresh
}

pub(super) fn apply_kiro_usage_snapshot(
    state: &mut KiroAccountState,
    usage: KiroUsage,
    config: Option<&DevinPoolConfig>,
) {
    let previously_exhausted = state.included_exhausted();
    let pools = usage.resource_pools();
    let complete = !pools.is_empty()
        && state.usage.as_ref().is_none_or(|previous| {
            previous
                .resource_pools()
                .iter()
                .all(|old| pools.iter().any(|new| new.resource == old.resource))
        });
    if !complete {
        // Missing resources are not proof that those allowances recovered.
        // Retain the last confirmed snapshot and its timestamp, explicitly stale.
        state.usage_stale = true;
        if previously_exhausted || pools.iter().any(|pool| pool.used >= pool.limit) {
            state.hard_quota = true;
            state
                .hard_quota_reason
                .get_or_insert_with(|| "quota_observation_incomplete".into());
        } else if state.hard_quota_reason.as_deref() == Some("reauthorization_verification") {
            // Authentication was verified; absence of quota remains unknown.
            state.hard_quota = false;
            state.hard_quota_reason = None;
        }
        return;
    }
    state.usage = Some(usage);
    state.observed_at = Some(Utc::now().timestamp_millis());
    state.usage_stale = false;
    let reserve = config.is_none_or(|c| c.reserve_policy == DevinReservePolicy::Fallback)
        && state.usage.as_ref().is_some_and(|u| u.overage_enabled);
    if state.hard_quota_reason.as_deref() == Some("reauthorization_verification")
        || (state.usage_complete() && (!state.included_exhausted() || reserve))
    {
        state.hard_quota = false;
        state.hard_quota_reason = None;
    } else if previously_exhausted && !state.usage_complete() {
        state.hard_quota = true;
        state
            .hard_quota_reason
            .get_or_insert_with(|| "quota_observation_incomplete".into());
    }
    // auth_invalid and rejection_revision are deliberately not reset by a
    // read-only usage observation; exact-state CAS fences concurrent refusals.
}

pub(super) fn kiro_usage_observations(usage: &KiroUsage) -> Vec<QuotaTierObservation> {
    super::usage::kiro_usage_tiers(usage)
        .into_iter()
        .filter(|tier| !tier.name.starts_with("feature:"))
        .map(|tier| QuotaTierObservation {
            name: tier.name,
            used_percent: tier.used_percent,
            resets_at: tier.resets_at,
        })
        .collect()
}

impl AdminService {
    pub(super) async fn refresh_kiro_account_usage_locked(
        &self,
        provider: &Provider,
        account_id: &str,
        config: Option<&DevinPoolConfig>,
    ) -> anyhow::Result<ProviderAccountView> {
        let store = self.gw.storage.oauth_credentials();
        let before = store
            .get_account(&provider.id, account_id)
            .await?
            .context("account not found")?;
        let account = self.refresh_kiro_pool_account(provider, &before).await?;
        let runtime = self
            .resolve_provider_account_runtime(provider, &account)
            .await?;
        let credential = runtime
            .credential
            .as_ref()
            .context("Kiro OAuth credential is missing")?;
        let mut next = KiroAccountState::parse(&account.account_state);
        let permit = super::account_pool::STATUS_CONCURRENCY.acquire().await?;
        let result = crate::provider::kiro::fetch_usage(
            &self.gw,
            provider,
            credential,
            &runtime.access_token,
        )
        .await;
        drop(permit);
        let result_error = match result {
            Ok(usage) => {
                apply_kiro_usage_snapshot(&mut next, usage, config);
                None
            }
            Err(error) => {
                next.usage_stale = true;
                if error
                    .to_string()
                    .starts_with("Kiro authentication failed (HTTP 401)")
                    || error
                        .to_string()
                        .starts_with("Kiro authentication failed (HTTP 403)")
                {
                    next.auth_invalid = true;
                }
                Some(error)
            }
        };
        let raw = serde_json::to_string(&next)?;
        anyhow::ensure!(
            store
                .compare_account_state(
                    &provider.id,
                    account_id,
                    account.status_version,
                    &account.account_state,
                    &raw,
                )
                .await?,
            "ACCOUNT_VERSION_CONFLICT: account changed during usage query"
        );
        let mut updated = account;
        updated.account_state = raw;
        if result_error.is_some() || next.usage_stale {
            self.gw
                .account_quota_registry
                .record_query_failure(account_id);
        } else {
            let class = kiro_scheduling(&updated, config);
            self.gw.account_quota_registry.observe_with_reserve(
                account_id,
                &next
                    .usage
                    .as_ref()
                    .map(kiro_usage_observations)
                    .unwrap_or_default(),
                Some(matches!(class, "included" | "unknown" | "reserve")),
                class == "reserve",
            );
        }
        self.get_provider_account_pool(&provider.id).await?;
        if let Some(error) = result_error {
            return Err(error);
        }
        Ok(safe_kiro_account(&updated, config))
    }

    pub(super) fn kiro_account_pool_provider_usage(&self, view: AccountPoolView) -> ProviderUsage {
        let members: Vec<_> = view
            .accounts
            .iter()
            .filter(|a| a.is_enabled && a.status != "disconnected")
            .collect();
        // No guessed cross-account capacity: actual used/limit is available on
        // every account. Only a genuine singleton can expose provider tiers.
        let tiers = if members.len() == 1 {
            members[0]
                .kiro_usage
                .as_ref()
                .map(super::usage::kiro_usage_tiers)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut plans: Vec<_> = members
            .iter()
            .filter_map(|a| a.kiro_usage.as_ref()?.plan.clone())
            .collect();
        plans.sort();
        plans.dedup();
        let level = match plans.len() {
            0 => None,
            1 => plans.pop(),
            _ => Some("Mixed".into()),
        };
        ProviderUsage {
            provider_id: view.provider_id.clone(),
            kind: "kiro_plan".into(),
            site: "global".into(),
            level,
            tiers,
            balances: Vec::new(),
            spends: Vec::new(),
            is_available: Some(view.counts.available > 0),
            scheduling: self.gw.quota_registry.snapshot(&view.provider_id),
            queried_at: Utc::now().to_rfc3339(),
            account_pool: Some(view),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn provider() -> Provider {
        serde_json::from_value(serde_json::json!({
            "id":"kiro-pool", "name":"Kiro pool", "vendor":"kiro",
            "protocol":"kiro-chat/generate-assistant-response/v1",
            "base_url":"https://q.us-east-1.amazonaws.com", "protocol_mode":"fixed",
            "protocol_endpoints":[], "keys":[], "preset_key":"kiro", "channel":"ide",
            "api_key":"", "auth_mode":"oauth", "use_proxy":false, "fast_mode":false,
            "is_enabled":true, "created_at":"", "updated_at":""
        }))
        .unwrap()
    }
    async fn gateway() -> Gateway {
        let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
            vec![provider()],
            vec![],
            vec![],
        ));
        Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
            .await
            .unwrap()
            .0
    }
    fn usage(used: f64, limit: f64) -> KiroUsage {
        crate::provider::kiro::usage::parse(&serde_json::json!({
            "subscriptionInfo":{"subscriptionTitle":"KIRO PRO"},
            "usageBreakdownList":[{"resourceType":"CREDITS", "currentUsage":used,"usageLimit":limit}]
        }))
    }
    async fn add(gw: &Gateway, label: &str, region: &str) -> OAuthCredential {
        gw.storage
            .oauth_credentials()
            .create_account(
                "kiro-pool",
                label,
                UpsertOAuthCredential {
                    driver_key: "kiro".into(),
                    scheme: "oauth_device_code".into(),
                    access_token: format!("SECRET-{label}"),
                    refresh_token: Some(format!("REFRESH-{label}")),
                    expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                    resource_url: Some(format!("https://q.{region}.amazonaws.com")),
                    meta: Some(
                        serde_json::json!({"region":region,"client_secret":"PRIVATE"}).to_string(),
                    ),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn kiro_pool_views_show_actual_account_quota_not_plan_weights_or_secrets() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("kiro-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        let a = add(&gw, "A", "eu-central-1").await;
        let b = add(&gw, "B", "us-east-1").await;
        for (record, used, limit) in [(&a, 10.0, 100.0), (&b, 125.0, 250.0)] {
            let state = authenticated_kiro_state(usage(used, limit), None);
            assert!(
                gw.storage
                    .oauth_credentials()
                    .compare_account_state(
                        "kiro-pool",
                        &record.account_id,
                        record.status_version,
                        &record.account_state,
                        &serde_json::to_string(&state).unwrap()
                    )
                    .await
                    .unwrap()
            );
        }
        let view = admin.get_provider_account_pool("kiro-pool").await.unwrap();
        assert_eq!(view.kind.as_deref(), Some("kiro"));
        assert_eq!(view.counts.available, 2);
        assert_eq!(
            (
                view.counts.pro,
                view.counts.max,
                view.counts.plan_unverified
            ),
            (0, 0, 0)
        );
        assert!(view.quota.daily.is_none() && view.quota.weekly.is_none() && !view.quota.estimated);
        assert!(
            view.accounts
                .iter()
                .all(|a| a.daily.is_none() && a.weekly.is_none() && a.verified_plan.is_none())
        );
        let actual = view
            .accounts
            .iter()
            .find(|v| v.account_id == b.account_id)
            .unwrap()
            .kiro_usage
            .as_ref()
            .unwrap();
        assert_eq!(
            (actual.items[0].used, actual.items[0].limit),
            (125.0, 250.0)
        );
        let public = serde_json::to_string(&view).unwrap();
        for secret in [
            "SECRET",
            "REFRESH",
            "PRIVATE",
            "client_secret",
            "access_token",
            "subject_id",
        ] {
            assert!(!public.contains(secret), "leaked {secret}");
        }
        let combined = admin.account_pool_provider_usage(view);
        assert_eq!(combined.kind, "kiro_plan");
        assert!(
            combined.tiers.is_empty(),
            "Do not invent a combined quota from unrelated accounts"
        );
    }
    #[test]
    fn kiro_partial_usage_does_not_clear_hard_quota_or_newer_rejections() {
        let mut state = KiroAccountState {
            usage: Some(usage(100.0, 100.0)),
            auth_invalid: true,
            rejection_revision: 9,
            ..Default::default()
        };
        apply_kiro_usage_snapshot(&mut state, KiroUsage::default(), None);
        assert!(state.hard_quota && state.auth_invalid);
        assert_eq!(state.rejection_revision, 9);
        apply_kiro_usage_snapshot(&mut state, usage(5.0, 100.0), None);
        assert!(!state.hard_quota && state.auth_invalid);
        let mut fresh = authenticated_kiro_state(
            KiroUsage::default(),
            Some(&KiroAccountState {
                usage: Some(usage(100.0, 100.0)),
                ..Default::default()
            }),
        );
        assert!(!fresh.auth_invalid && fresh.hard_quota);
        apply_kiro_usage_snapshot(&mut fresh, KiroUsage::default(), None);
        assert!(fresh.hard_quota);
    }
    #[test]
    fn kiro_missing_resource_keeps_last_good_snapshot_and_reauthorization_quota_fence() {
        let old = crate::provider::kiro::usage::parse(&serde_json::json!({
            "usageBreakdownList":[
                {"resourceType":"A", "currentUsage":100,"usageLimit":100},
                {"resourceType":"B", "currentUsage":2,"usageLimit":10}
            ]
        }));
        let partial = crate::provider::kiro::usage::parse(&serde_json::json!({
            "usageBreakdownList":[{"resourceType":"B","currentUsage":1,"usageLimit":10}]
        }));
        let previous = KiroAccountState {
            usage: Some(old.clone()),
            observed_at: Some(42),
            hard_quota: true,
            hard_quota_reason: Some("monthly_quota".into()),
            rejection_revision: 3,
            ..Default::default()
        };
        let mut state = previous.clone();
        apply_kiro_usage_snapshot(&mut state, partial.clone(), None);
        assert_eq!(state.usage, Some(old.clone()));
        assert_eq!(state.observed_at, Some(42));
        assert!(state.hard_quota && state.usage_stale);
        assert_eq!(state.rejection_revision, 3);
        let mut fresh = authenticated_kiro_state(partial.clone(), Some(&previous));
        assert_eq!(fresh.usage, Some(old));
        apply_kiro_usage_snapshot(&mut fresh, partial, None);
        assert!(fresh.hard_quota && fresh.usage_stale);
    }
    #[tokio::test]
    async fn kiro_account_import_sessions_keep_target_and_legacy_apis_reject_ambiguity() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("kiro-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        let a = add(&gw, "A", "eu-central-1").await;
        add(&gw, "B", "us-east-1").await;
        let init = admin
            .init_provider_account_session_with_options(
                "kiro-pool",
                Some(&a.account_id),
                Some(a.status_version),
                Some(auth::KiroLoginOptions::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            init.completion_mode,
            Some(auth::AuthCompletionMode::ManualExchange)
        );
        let session = admin
            .get_auth_session_record(&init.session_id)
            .await
            .unwrap()
            .unwrap();
        let context: Value =
            serde_json::from_str(session.context_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            context["provider_account_intent"]["account_id"],
            a.account_id
        );
        assert_eq!(session.driver_key, "kiro");
        // Actual import exchange stays available without making a live API call.
        let import = serde_json::json!({"accessToken":"IMPORTED", "refreshToken":"REFRESH-IMPORTED",
            "expiresAt":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339(),
            "authMethod":"social", "region":"eu-central-1",
            "profileArn":"arn:aws:codewhisperer:eu-central-1:1:profile/X"});
        admin
            .complete_oauth_session(
                &init.session_id,
                auth::AuthExchangeInput {
                    code: Some(import.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            admin
                .bind_provider_with_oauth_session("kiro-pool", &init.session_id)
                .await
                .is_err()
        );
        assert!(
            admin
                .get_auth_session_record(&init.session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(admin.logout_provider_oauth("kiro-pool").await.is_err());
        assert_eq!(
            gw.storage
                .oauth_credentials()
                .list_accounts("kiro-pool")
                .await
                .unwrap()
                .len(),
            2
        );
        admin.cancel_oauth_session(&init.session_id).await.unwrap();
    }
    #[tokio::test]
    async fn kiro_runtime_is_exact_account_and_models_do_not_use_singleton_credentials() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("kiro-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        let a = add(&gw, "A", "eu-central-1").await;
        let b = add(&gw, "B", "us-east-1").await;
        let runtime = admin
            .resolve_provider_account_runtime(&provider(), &a)
            .await
            .unwrap();
        assert_eq!(runtime.access_token, "SECRET-A");
        assert_eq!(
            runtime.binding.base_url_override.as_deref(),
            Some("https://q.eu-central-1.amazonaws.com")
        );
        let refreshed = admin
            .refresh_kiro_pool_account(&provider(), &a)
            .await
            .unwrap();
        assert_eq!(refreshed.status_version, a.status_version);
        assert_eq!(refreshed.account_id, a.account_id);
        assert_eq!(
            gw.storage
                .oauth_credentials()
                .get_account("kiro-pool", &b.account_id)
                .await
                .unwrap()
                .unwrap()
                .access_token,
            "SECRET-B"
        );
        for account in [a, b] {
            admin
                .update_provider_account(
                    "kiro-pool",
                    &account.account_id,
                    account.status_version,
                    OAuthAccountUpdate {
                        is_enabled: Some(false),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert!(
            !admin
                .get_provider_models("kiro-pool")
                .await
                .unwrap()
                .is_empty()
        );
        let result = admin.test_provider("kiro-pool").await.unwrap();
        assert!(!result.success);
        assert_eq!(result.accounts.len(), 2);
        assert_eq!(result.error.as_deref(), Some("No enabled Kiro accounts"));
    }
}
