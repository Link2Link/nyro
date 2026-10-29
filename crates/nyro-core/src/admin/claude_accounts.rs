//! Claude Code observations behind the shared OAuth account administration API.
use super::*;
use crate::provider::account_pool::ClaudeAccountState;
use crate::router::quota::QuotaTierObservation;

pub(super) fn is_claude_provider(provider: &Provider) -> bool {
    crate::provider::anthropic::claude_code::egress::is_claude_code_channel(provider)
}

pub(super) fn is_claude_account(account: &OAuthCredential) -> bool {
    matches!(account.driver_key.as_str(), "claude-code" | "claude")
}

pub(super) fn claude_scheduling(
    account: &OAuthCredential,
    config: Option<&DevinPoolConfig>,
) -> &'static str {
    let state = ClaudeAccountState::parse(&account.account_state);
    if account.status == "disconnected" {
        return "disconnected";
    }
    if !account.is_enabled {
        return "disabled";
    }
    if !matches!(account.status.as_str(), "connected" | "refreshing")
        || account.access_token.trim().is_empty()
        || state.auth_invalid
    {
        return "auth_required";
    }
    match state.cost_class(config.unwrap_or(&DevinPoolConfig::default())) {
        Some(0) => "included",
        Some(1) => "unknown",
        _ => "quota_exhausted",
    }
}

pub(super) fn safe_claude_account(
    account: &OAuthCredential,
    config: Option<&DevinPoolConfig>,
) -> ProviderAccountView {
    let state = ClaudeAccountState::parse(&account.account_state);
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
        scheduling: claude_scheduling(account, config).into(),
        daily: None,
        weekly: None,
        reserve_usd: None,
        kiro_usage: None,
        claude_usage: state.usage,
        observed_at: state.observed_at,
        stale: state.usage_stale
            || state
                .observed_at
                .is_none_or(|at| Utc::now().timestamp_millis().saturating_sub(at) > 300_000),
        updated_at: account.updated_at.clone(),
    }
}

/// Plan-relative estimates, deliberately separate from scheduling observations.
/// Missing/unsupported plans use Pro-equivalent capacity without changing metadata.
/// Missing, stale, or invalid usage never becomes a zero-percent member.
pub(super) fn aggregate_claude_window(
    members: &[&ProviderAccountView],
    name: &str,
) -> PoolQuotaWindow {
    let capacity = |account: &ProviderAccountView| match (
        account
            .claude_usage
            .as_ref()
            .and_then(|usage| usage.plan.as_deref()),
        name,
    ) {
        (Some("max 5x"), "five_hour" | "weekly_limit") => Some(5),
        (Some("max 20x"), "five_hour") => Some(20),
        (Some("max 20x"), "weekly_limit") => Some(10),
        (_, "five_hour" | "weekly_limit") => Some(1),
        _ => None,
    };
    let total_units: usize = members.iter().filter_map(|a| capacity(a)).sum();
    let capacity_complete = members.iter().all(|a| capacity(a).is_some());
    let now = Utc::now();
    let windows: Vec<_> = members
        .iter()
        .filter_map(|a| {
            let units = capacity(a)?;
            if a.stale || a.scheduling == "auth_required" {
                return None;
            }
            let mut matches = a
                .claude_usage
                .as_ref()?
                .windows
                .iter()
                .filter(|w| w.name == name);
            let window = matches.next()?;
            if matches.next().is_some()
                || !window.used_percent.is_finite()
                || !(0.0..=100.0).contains(&window.used_percent)
            {
                return None;
            }
            // Past reset timestamps invalidate the old observation; they do not
            // imply a fresh zero-use window. Missing/invalid reset timestamps
            // leave reset coverage unknown without inventing a reset instant.
            let reset = window
                .resets_at
                .as_deref()
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                .map(|at| at.with_timezone(&Utc));
            if reset.is_some_and(|at| at <= now) {
                return None;
            }
            Some((window, units, reset))
        })
        .collect();
    let known = windows.len();
    let subtotal: f64 = windows
        .iter()
        .map(|(w, units, _)| (1.0 - w.used_percent / 100.0) * *units as f64)
        .sum();
    let n = members.len();
    let complete = total_units > 0 && capacity_complete && known == n;
    let next = windows.iter().filter_map(|(_, _, reset)| *reset).min();
    let next_count = next.map_or(0, |at| {
        windows
            .iter()
            .filter(|(_, _, reset)| *reset == Some(at))
            .count()
    });
    let all_resets = complete && windows.iter().all(|(_, _, reset)| reset.is_some());
    PoolQuotaWindow {
        total_accounts: n,
        capacity_complete,
        total_units,
        known_accounts: known,
        known_remaining_units: subtotal,
        used_units: complete.then_some(total_units as f64 - subtotal),
        remaining_units: complete.then_some(subtotal),
        used_percent: complete.then(|| 100.0 - subtotal / total_units as f64 * 100.0),
        remaining_percent: complete.then(|| subtotal / total_units as f64 * 100.0),
        stale: !complete,
        next_reset_at: next.map(|at| at.to_rfc3339()),
        next_reset_accounts: next_count,
        reset_mode: if !all_resets {
            "unknown"
        } else if next_count == n {
            "aligned"
        } else {
            "staggered"
        }
        .into(),
    }
}

pub(super) fn authenticated_claude_state(
    usage: ClaudeUsage,
    previous: Option<&ClaudeAccountState>,
) -> ClaudeAccountState {
    let mut fresh = ClaudeAccountState {
        usage: Some(usage),
        observed_at: Some(Utc::now().timestamp_millis()),
        ..Default::default()
    };
    if let Some(previous) = previous {
        let blocked = previous.included_exhausted()
            || fresh.included_exhausted()
            || (previous.hard_quota
                && previous.hard_quota_reason.as_deref() != Some("reauthorization_verification"));
        // A re-login repairs auth only. Keep every missing old window, including
        // model-specific refusals, until a post-publication authoritative probe.
        if let Some(old) = &previous.usage {
            let current = fresh.usage.as_mut().unwrap();
            for window in &old.windows {
                if !current.windows.iter().any(|new| new.name == window.name) {
                    current.windows.push(window.clone());
                    fresh.usage_stale = true;
                    fresh.observed_at = previous.observed_at;
                }
            }
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

pub(super) fn apply_claude_usage_snapshot(state: &mut ClaudeAccountState, mut usage: ClaudeUsage) {
    let primary_complete = ["five_hour", "weekly_limit"]
        .iter()
        .all(|name| usage.windows.iter().any(|w| w.name == *name));
    let mut complete = primary_complete;
    if let Some(previous) = &state.usage {
        for old in &previous.windows {
            if !usage.windows.iter().any(|new| new.name == old.name) {
                // Absence is unknown, never proof that a spent window recovered.
                usage.windows.push(old.clone());
                complete = false;
            }
        }
    }
    state.usage = Some(usage);
    state.usage_stale = !complete;
    if complete {
        state.observed_at = Some(Utc::now().timestamp_millis());
    }
    if (complete && !state.included_exhausted())
        || (state.hard_quota_reason.as_deref() == Some("reauthorization_verification")
            && !state.included_exhausted())
    {
        state.hard_quota = false;
        state.hard_quota_reason = None;
    }
    // Never reset auth_invalid/rejection_revision. Exact raw-state CAS fences
    // a concurrent refusal, including semantically identical repeated refusals.
}

pub(super) fn claude_usage_observations(usage: &ClaudeUsage) -> Vec<QuotaTierObservation> {
    usage
        .windows
        .iter()
        .filter(|w| !w.name.starts_with("feature:") || w.name.starts_with("feature:oauth_apps:"))
        .map(|w| QuotaTierObservation {
            name: w.name.clone(),
            used_percent: w.used_percent,
            resets_at: w.resets_at.clone(),
        })
        .collect()
}

impl AdminService {
    pub(super) async fn refresh_claude_account_usage_locked(
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
        let account = self.refresh_claude_pool_account(provider, &before).await?;
        let runtime = self
            .resolve_provider_account_runtime(provider, &account)
            .await?;
        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let mut next = ClaudeAccountState::parse(&account.account_state);
        let permit = super::account_pool::STATUS_CONCURRENCY.acquire().await?;
        let result = crate::provider::anthropic::claude_code::usage::fetch_usage_with_plan(
            &client,
            runtime
                .binding
                .base_url_override
                .as_deref()
                .unwrap_or(&provider.base_url),
            runtime.access_token.trim(),
            &account.account_id,
        )
        .await;
        drop(permit);
        let result_error = match result {
            Ok(usage) => {
                apply_claude_usage_snapshot(&mut next, usage);
                None
            }
            Err(error) => {
                next.usage_stale = true;
                if error
                    .to_string()
                    .starts_with("Claude authentication failed (HTTP 401")
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
                    &raw
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
            let class = claude_scheduling(&updated, config);
            self.gw.account_quota_registry.observe_with_reserve(
                account_id,
                &next
                    .usage
                    .as_ref()
                    .map(claude_usage_observations)
                    .unwrap_or_default(),
                Some(matches!(class, "included" | "unknown")),
                false,
            );
        }
        self.get_provider_account_pool(&provider.id).await?;
        if let Some(error) = result_error {
            return Err(error);
        }
        Ok(safe_claude_account(&updated, config))
    }

    pub(super) fn claude_account_pool_provider_usage(
        &self,
        view: AccountPoolView,
    ) -> ProviderUsage {
        let members: Vec<_> = view
            .accounts
            .iter()
            .filter(|a| a.is_enabled && a.status != "disconnected")
            .collect();
        // Preserve exact singleton tiers. Multi-account weighted estimates live
        // only in account_pool.quota.claude_windows, never provider usage tiers
        // or routing observations; model-specific windows remain per account.
        let tiers = if members.len() == 1 {
            members[0]
                .claude_usage
                .as_ref()
                .map(|usage| {
                    usage
                        .windows
                        .iter()
                        .map(|w| ProviderUsageTier {
                            name: w.name.clone(),
                            used_percent: w.used_percent,
                            resets_at: w.resets_at.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        ProviderUsage {
            provider_id: view.provider_id.clone(),
            kind: "claude_code_plan".into(),
            site: "global".into(),
            level: None,
            tiers,
            balances: Vec::new(),
            spends: Vec::new(),
            is_available: Some(view.counts.available > 0),
            scheduling: self.gw.quota_registry.snapshot(&view.provider_id),
            queried_at: Utc::now().to_rfc3339(),
            account_pool: Some(view),
            upstream: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Provider {
        serde_json::from_value(serde_json::json!({
            "id":"claude-pool", "name":"Claude pool", "vendor":"anthropic",
            "protocol":"anthropic-messages/messages/2023-06-01", "base_url":"https://api.anthropic.com",
            "protocol_mode":"fixed", "protocol_endpoints":[], "keys":[],
            "preset_key":"anthropic", "channel":"claude-code", "api_key":"", "auth_mode":"oauth",
            "use_proxy":false, "fast_mode":false, "is_enabled":true, "created_at":"", "updated_at":""
        })).unwrap()
    }
    fn usage(session: f64, weekly: f64) -> ClaudeUsage {
        crate::provider::anthropic::claude_code::usage::parse_usage(&serde_json::json!({
            "five_hour":{"utilization":session,"resets_at":"2030-01-01T05:00:00Z"},
            "seven_day":{"utilization":weekly,"resets_at":"2030-01-07T00:00:00Z"},
            "seven_day_opus":{"utilization":100,"resets_at":"2030-01-07T00:00:00Z"}
        }))
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
    async fn add(gw: &Gateway, label: &str) -> OAuthCredential {
        gw.storage
            .oauth_credentials()
            .create_account(
                "claude-pool",
                label,
                UpsertOAuthCredential {
                    driver_key: "claude-code".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: format!("SECRET-{label}"),
                    refresh_token: Some("PRIVATE-REFRESH".into()),
                    subject_id: Some(label.into()),
                    meta: Some("{\"secret\":\"PRIVATE\"}".into()),
                    expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn claude_pool_dto_has_real_windows_without_devin_units_or_credentials() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("claude-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        for (label, used) in [("a", 10.0), ("b", 40.0)] {
            let account = add(&gw, label).await;
            let state = authenticated_claude_state(usage(used, 20.0), None);
            assert!(
                gw.storage
                    .oauth_credentials()
                    .compare_account_state(
                        "claude-pool",
                        &account.account_id,
                        account.status_version,
                        &account.account_state,
                        &serde_json::to_string(&state).unwrap()
                    )
                    .await
                    .unwrap()
            );
        }
        let view = admin
            .get_provider_account_pool("claude-pool")
            .await
            .unwrap();
        assert_eq!(view.kind.as_deref(), Some("claude_code"));
        assert_eq!(view.counts.included, 2); // Opus-only exhaustion does not stop Sonnet.
        assert_eq!(
            (view.counts.pro, view.counts.max, view.counts.unclassified),
            (0, 0, 0)
        );
        assert!(view.quota.daily.is_none() && view.quota.weekly.is_none() && view.quota.estimated);
        assert_eq!(view.quota.claude_windows.len(), 2);
        assert!(
            view.quota
                .claude_windows
                .iter()
                .all(|w| w.quota.capacity_complete && w.quota.total_units == 2)
        );
        assert_eq!(view.quota.claude_windows[0].quota.used_percent, Some(25.0));
        assert_eq!(view.quota.claude_windows[1].quota.used_percent, Some(20.0));
        for account in &view.accounts {
            assert_eq!(account.claude_usage.as_ref().unwrap().plan, None);
            assert!(
                account.daily.is_none()
                    && account.weekly.is_none()
                    && account.verified_plan.is_none()
            );
            assert_eq!(
                account.claude_usage.as_ref().unwrap().windows[0].name,
                "five_hour"
            );
            assert!(account.kiro_usage.is_none());
        }
        let serialized = serde_json::to_string(&view).unwrap();
        for secret in [
            "SECRET-",
            "PRIVATE",
            "access_token",
            "refresh_token",
            "subject_id",
        ] {
            assert!(!serialized.contains(secret), "leaked {secret}");
        }
        let multi = admin.account_pool_provider_usage(view.clone());
        assert!(multi.tiers.is_empty());
        let mut single = view;
        single.accounts.truncate(1);
        assert_eq!(
            admin.account_pool_provider_usage(single).tiers[1].name,
            "weekly_limit"
        );
    }

    #[tokio::test]
    async fn claude_single_and_bulk_usage_expose_pool_without_reviving_auth_rejections() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("claude-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        for label in ["a", "b"] {
            let account = add(&gw, label).await;
            let state = ClaudeAccountState {
                usage: Some(usage(10.0, 20.0)),
                auth_invalid: true,
                usage_stale: true,
                observed_at: Some(Utc::now().timestamp_millis()),
                ..Default::default()
            };
            assert!(
                gw.storage
                    .oauth_credentials()
                    .compare_account_state(
                        "claude-pool",
                        &account.account_id,
                        account.status_version,
                        &account.account_state,
                        &serde_json::to_string(&state).unwrap()
                    )
                    .await
                    .unwrap()
            );
        }
        // Auth-invalid accounts do not issue upstream queries; retain their
        // last observation without advertising them as usable capacity.
        let single = admin.get_provider_usage("claude-pool").await.unwrap();
        let bulk = admin.list_provider_usage().await.unwrap();
        assert_eq!(single.kind, "claude_code_plan");
        assert_eq!(single.is_available, Some(false));
        assert!(single.tiers.is_empty());
        assert_eq!(single.account_pool.as_ref().unwrap().accounts.len(), 2);
        assert_eq!(bulk.len(), 1);
        assert_eq!(bulk[0].status, ProviderUsageQueryStatus::Ok);
        let public = serde_json::to_value(&bulk).unwrap();
        assert_eq!(public[0]["usage"]["account_pool"]["kind"], "claude_code");
        assert_eq!(
            public[0]["usage"]["account_pool"]["accounts"][0]["claude_usage"]["windows"][0]["name"],
            "five_hour"
        );
        assert!(!gw.quota_registry.is_schedulable("claude-pool"));
    }

    #[tokio::test]
    async fn claude_aggregate_rejects_nonfinite_duplicate_windows_but_defaults_unspecified_max() {
        // Direct DTOs exercise IEEE values which JSON cannot represent.
        let gw = gateway().await;
        let record = add(&gw, "nonfinite").await;
        let mut account = safe_claude_account(&record, None);
        account.stale = false;
        account.scheduling = "included".into();
        let mut known = usage(25.0, 40.0); // Unknown plan still has fallback capacity.
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut snapshot = known.clone();
            snapshot.windows[0].used_percent = invalid;
            account.claude_usage = Some(snapshot);
            let aggregate = aggregate_claude_window(&[&account], "five_hour");
            assert!(aggregate.capacity_complete);
            assert_eq!(aggregate.total_units, 1);
            assert_eq!(aggregate.known_accounts, 0);
            assert_eq!(aggregate.used_percent, None);
            assert_eq!(aggregate.known_remaining_units, 0.0);
        }
        let mut duplicate = known.clone();
        duplicate.windows.push(duplicate.windows[0].clone());
        account.claude_usage = Some(duplicate);
        assert_eq!(
            aggregate_claude_window(&[&account], "five_hour").known_accounts,
            0
        );
        known.plan = Some("max".into());
        account.claude_usage = Some(known);
        let aggregate = aggregate_claude_window(&[&account], "five_hour");
        assert!(aggregate.capacity_complete);
        assert_eq!(aggregate.total_units, 1);
        assert_eq!(aggregate.used_percent, Some(25.0));
        assert_eq!(
            account.claude_usage.as_ref().unwrap().plan.as_deref(),
            Some("max")
        );
    }

    #[tokio::test]
    async fn claude_account_refresh_persists_profile_plan_and_clears_failed_profile() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                let (status, body) = if request.starts_with("GET /api/oauth/usage ") {
                    (
                        200,
                        serde_json::json!({"five_hour":{"utilization":10}, "seven_day":{"utilization":20}}),
                    )
                } else if request.contains("Bearer success") {
                    (
                        200,
                        serde_json::json!({"organization":{"organization_type":"claude_pro"}}),
                    )
                } else {
                    (500, serde_json::json!({}))
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("claude-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        for (token, expected) in [("success", Some("pro")), ("failed", None)] {
            let store = gw.storage.oauth_credentials();
            let account = store
                .create_account(
                    "claude-pool",
                    token,
                    UpsertOAuthCredential {
                        driver_key: "claude-code".into(),
                        scheme: "oauth_auth_code_pkce".into(),
                        access_token: token.into(),
                        resource_url: Some(base.clone()),
                        expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut old = usage(30.0, 40.0);
            old.plan = Some("max 20x".into());
            let state = authenticated_claude_state(old, None);
            assert!(
                store
                    .compare_account_state(
                        "claude-pool",
                        &account.account_id,
                        account.status_version,
                        &account.account_state,
                        &serde_json::to_string(&state).unwrap()
                    )
                    .await
                    .unwrap()
            );
            let refreshed = admin
                .refresh_provider_account_usage("claude-pool", &account.account_id)
                .await
                .unwrap();
            assert_eq!(refreshed.claude_usage.unwrap().plan.as_deref(), expected);
            let saved = store
                .get_account("claude-pool", &account.account_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                ClaudeAccountState::parse(&saved.account_state)
                    .usage
                    .unwrap()
                    .plan
                    .as_deref(),
                expected
            );
        }
        server.await.unwrap();
    }

    #[test]
    fn claude_refresh_and_relogin_do_not_retain_previous_plan_on_profile_failure() {
        let mut old_usage = usage(20.0, 30.0);
        old_usage.plan = Some("max 20x".into());
        let mut state = authenticated_claude_state(old_usage, None);
        let fresh = usage(10.0, 20.0); // Best-effort profile fetch returned None.
        let login = authenticated_claude_state(fresh.clone(), Some(&state));
        assert_eq!(login.usage.unwrap().plan, None);
        apply_claude_usage_snapshot(&mut state, fresh);
        assert_eq!(state.usage.unwrap().plan, None);
    }

    #[test]
    fn partial_claude_observations_preserve_spent_windows_and_rejection_fences() {
        let mut state = ClaudeAccountState {
            usage: Some(usage(100.0, 10.0)),
            observed_at: Some(42),
            auth_invalid: true,
            rejection_revision: 7,
            hard_quota: true,
            hard_quota_reason: Some("quota_exhausted".into()),
            ..Default::default()
        };
        let partial =
            crate::provider::anthropic::claude_code::usage::parse_usage(&serde_json::json!({
                "seven_day":{"utilization":0}
            }));
        apply_claude_usage_snapshot(&mut state, partial.clone());
        assert!(state.hard_quota && state.auth_invalid && state.usage_stale);
        assert!(state.included_exhausted());
        assert_eq!(state.observed_at, Some(42));
        assert_eq!(state.rejection_revision, 7);
        let mut reauth = authenticated_claude_state(partial, Some(&state));
        assert!(!reauth.auth_invalid && reauth.hard_quota);
        apply_claude_usage_snapshot(&mut reauth, ClaudeUsage::default());
        assert!(reauth.hard_quota);
        apply_claude_usage_snapshot(&mut state, usage(1.0, 2.0));
        assert!(!state.hard_quota && state.auth_invalid);
        assert_eq!(state.rejection_revision, 7);
    }

    #[tokio::test]
    async fn claude_account_runtime_is_exact_and_stale_mutations_cannot_publish() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("claude-pool", DevinPoolConfig::default())
            .await
            .unwrap();
        let a = add(&gw, "a").await;
        let b = add(&gw, "b").await;
        let runtime = admin
            .resolve_provider_account_runtime(&provider(), &a)
            .await
            .unwrap();
        assert_eq!(runtime.access_token, "SECRET-a");
        assert_eq!(
            runtime
                .binding
                .extra_headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer SECRET-a")
        );
        assert!(runtime.binding.disable_default_auth);
        let updated = admin
            .update_provider_account(
                "claude-pool",
                &a.account_id,
                a.status_version,
                OAuthAccountUpdate {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(updated.status_version > a.status_version);
        assert!(
            !gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "claude-pool",
                    &a.account_id,
                    a.status_version,
                    &a.account_state,
                    "{}"
                )
                .await
                .unwrap()
        );
        assert!(
            admin
                .disconnect_provider_account("claude-pool", &a.account_id, a.status_version)
                .await
                .is_err()
        );
        admin
            .disconnect_provider_account("claude-pool", &b.account_id, b.status_version)
            .await
            .unwrap();
        assert!(
            !admin
                .get_provider_models("claude-pool")
                .await
                .unwrap()
                .is_empty()
        );
        let result = admin.test_provider("claude-pool").await.unwrap();
        assert!(!result.success);
        // Disconnected accounts are tombstones excluded by list_accounts.
        // The remaining disabled account is reported, but never probed.
        assert_eq!(result.accounts.len(), 1);
        assert_eq!(result.accounts[0].account_id, a.account_id);
        assert_eq!(
            result.accounts[0].skipped_reason.as_deref(),
            Some("disabled")
        );
        assert_eq!(
            result.error.as_deref(),
            Some("No enabled Claude Code accounts")
        );
    }
}
