//! Devin account management. Credentials never cross this module's DTO boundary.
use super::*;
use crate::provider::account_pool::DevinAccountState;
use crate::provider::devin::{DevinQuota, DevinUsage};
use crate::router::quota::QuotaTierObservation;
use crate::router::usage::{dynamic_weight, score_provider, window_boost};
use futures::{StreamExt, stream};

pub(crate) const POOL_SETTING_PREFIX: &str = "provider_account_pool:";
const SESSION_INTENT: &str = "provider_account_intent";
static STATUS_CONCURRENCY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DevinPlan {
    Pro,
    Max,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DevinPoolStrategy {
    #[default]
    RoundRobin,
    FillFirst,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DevinReservePolicy {
    #[default]
    Disabled,
    Fallback,
}

fn affinity_default() -> bool {
    true
}
fn affinity_ttl_default() -> u64 {
    3600
}
fn attempts_default() -> usize {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevinPoolConfig {
    pub plan: DevinPlan,
    #[serde(default)]
    pub strategy: DevinPoolStrategy,
    #[serde(default = "affinity_default")]
    pub session_affinity: bool,
    #[serde(default = "affinity_ttl_default")]
    pub affinity_ttl_seconds: u64,
    #[serde(default = "attempts_default")]
    pub max_account_attempts: usize,
    #[serde(default)]
    pub reserve_policy: DevinReservePolicy,
}
impl DevinPoolConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=8).contains(&self.max_account_attempts),
            "max_account_attempts must be between 1 and 8"
        );
        anyhow::ensure!(
            (1..=86400).contains(&self.affinity_ttl_seconds),
            "affinity_ttl_seconds must be between 1 and 86400"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AccountQuotaWindow {
    pub remaining_percent: u8,
    pub resets_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderAccountView {
    pub account_id: String,
    pub provider_id: String,
    pub label: String,
    pub is_enabled: bool,
    pub priority: i32,
    pub status: String,
    pub status_version: i32,
    pub verified_plan: Option<DevinPlan>,
    pub identity_verified: bool,
    pub scheduling: String,
    pub daily: Option<AccountQuotaWindow>,
    pub weekly: Option<AccountQuotaWindow>,
    pub reserve_usd: Option<f64>,
    pub observed_at: Option<i64>,
    pub stale: bool,
    pub updated_at: String,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PoolQuotaWindow {
    pub total_units: usize,
    pub known_accounts: usize,
    pub known_remaining_units: f64,
    pub used_units: Option<f64>,
    pub remaining_units: Option<f64>,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub stale: bool,
    pub next_reset_at: Option<String>,
    pub next_reset_accounts: usize,
    pub reset_mode: String,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AccountPoolQuota {
    pub daily: Option<PoolQuotaWindow>,
    pub weekly: Option<PoolQuotaWindow>,
    /// The upstream does not expose independently audited entitlement capacity.
    pub estimated: bool,
}
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AccountPoolCounts {
    pub total: usize,
    pub accounting: usize,
    pub available: usize,
    pub included: usize,
    pub reserve: usize,
    pub unknown: usize,
    pub disabled: usize,
    pub disconnected: usize,
    pub auth_required: usize,
    pub quota_exhausted: usize,
    pub plan_mismatch: usize,
    pub plan_unverified: usize,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AccountPoolView {
    pub provider_id: String,
    pub config: Option<DevinPoolConfig>,
    pub accounts: Vec<ProviderAccountView>,
    pub quota: AccountPoolQuota,
    pub counts: AccountPoolCounts,
    pub readiness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountSessionIntent {
    provider_id: String,
    account_id: Option<String>,
    expected_version: Option<i32>,
    plan: DevinPlan,
    #[serde(default)]
    fence: u64,
}
fn intent(session: &AuthSession) -> anyhow::Result<AccountSessionIntent> {
    let context: Value = serde_json::from_str(session.context_json.as_deref().unwrap_or("{}"))?;
    serde_json::from_value(
        context
            .get(SESSION_INTENT)
            .cloned()
            .context("account login intent is missing")?,
    )
    .context("invalid account login intent")
}
pub(super) fn is_account_session(session: &AuthSession) -> bool {
    serde_json::from_str::<Value>(session.context_json.as_deref().unwrap_or("{}"))
        .ok()
        .is_some_and(|context| context.get(SESSION_INTENT).is_some())
}
fn conflict() -> anyhow::Error {
    coded_error(
        "ACCOUNT_VERSION_CONFLICT",
        "Account changed; reload before retrying",
        serde_json::json!({}),
    )
}
fn state(account: &OAuthCredential) -> DevinAccountState {
    serde_json::from_str(&account.account_state).unwrap_or_default()
}
fn reset_at(unix: Option<i64>) -> Option<String> {
    unix.and_then(|time| DateTime::<Utc>::from_timestamp(time, 0))
        .map(|time| time.to_rfc3339())
}
fn is_stale(state: &DevinAccountState) -> bool {
    state.usage_stale
        || state
            .observed_at
            .is_none_or(|at| Utc::now().timestamp_millis().saturating_sub(at) > 300_000)
}
pub(crate) fn validate_devin_plan(plan: DevinPlan, usage: &DevinUsage) -> anyhow::Result<()> {
    let actual = match usage
        .plan_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "pro" | "devin pro" => Some(DevinPlan::Pro),
        "max" | "devin max" => Some(DevinPlan::Max),
        _ => None,
    };
    anyhow::ensure!(actual.is_some(), "PLAN_UNVERIFIED: unrecognized Devin plan");
    anyhow::ensure!(
        actual == Some(plan),
        "PLAN_MISMATCH: account does not match the pool plan"
    );
    anyhow::ensure!(
        usage.weekly.is_some() && (usage.daily.is_some() == (plan == DevinPlan::Pro)),
        "PLAN_MISMATCH: incompatible quota window shape"
    );
    Ok(())
}
fn scheduling(
    account: &OAuthCredential,
    state: &DevinAccountState,
    config: Option<&DevinPoolConfig>,
) -> &'static str {
    if account.status == "disconnected" {
        return "disconnected";
    }
    if !account.is_enabled {
        return "disabled";
    }
    if state.plan_mismatch {
        return "plan_mismatch";
    }
    if config.is_some_and(|c| state.verified_plan != Some(c.plan)) {
        return "plan_unverified";
    }
    if account.status != "connected" || account.access_token.is_empty() || state.auth_invalid {
        return "auth_required";
    }
    let exhausted = state.hard_quota
        || state.usage.as_ref().is_some_and(|usage| {
            [&usage.daily, &usage.weekly]
                .into_iter()
                .flatten()
                .any(|window| window.remaining_percent == 0)
        });
    if exhausted {
        if !state.hard_quota
            && config.is_none_or(|c| c.reserve_policy == DevinReservePolicy::Fallback)
            && state
                .usage
                .as_ref()
                .and_then(|u| u.overage_usd)
                .is_some_and(|usd| usd > 0.0)
        {
            return "reserve";
        }
        return "quota_exhausted";
    }
    if state.usage.is_none() {
        "unknown"
    } else {
        "included"
    }
}
fn safe_account(
    account: &OAuthCredential,
    config: Option<&DevinPoolConfig>,
) -> ProviderAccountView {
    let state = state(account);
    let window = |quota: &DevinQuota| AccountQuotaWindow {
        remaining_percent: quota.remaining_percent,
        resets_at: reset_at(quota.resets_at_unix),
    };
    ProviderAccountView {
        account_id: account.account_id.clone(),
        provider_id: account.provider_id.clone(),
        label: account.label.clone(),
        is_enabled: account.is_enabled,
        priority: account.priority,
        status: account.status.clone(),
        status_version: account.status_version,
        verified_plan: state.verified_plan,
        identity_verified: account
            .subject_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty()),
        scheduling: scheduling(account, &state, config).into(),
        daily: state
            .usage
            .as_ref()
            .and_then(|u| u.daily.as_ref())
            .map(window),
        weekly: state
            .usage
            .as_ref()
            .and_then(|u| u.weekly.as_ref())
            .map(window),
        reserve_usd: state.usage.as_ref().and_then(|u| u.overage_usd),
        observed_at: state.observed_at,
        stale: is_stale(&state),
        updated_at: account.updated_at.clone(),
    }
}
fn aggregate_window(members: &[&ProviderAccountView], daily: bool) -> PoolQuotaWindow {
    let windows: Vec<_> = members
        .iter()
        .filter_map(|a| {
            if daily {
                a.daily.as_ref()
            } else {
                a.weekly.as_ref()
            }
        })
        .collect();
    let n = members.len();
    let known = windows.len();
    let subtotal: f64 = windows
        .iter()
        .map(|w| f64::from(w.remaining_percent) / 100.0)
        .sum();
    let complete = n > 0 && n == known;
    let next = windows
        .iter()
        .filter_map(|w| w.resets_at.as_ref())
        .min()
        .cloned();
    let next_count = next.as_ref().map_or(0, |next| {
        windows
            .iter()
            .filter(|w| w.resets_at.as_ref() == Some(next))
            .count()
    });
    let all_resets = known == n && n > 0 && windows.iter().all(|w| w.resets_at.is_some());
    PoolQuotaWindow {
        total_units: n,
        known_accounts: known,
        known_remaining_units: subtotal,
        used_units: complete.then_some(n as f64 - subtotal),
        remaining_units: complete.then_some(subtotal),
        used_percent: complete.then(|| 100.0 - subtotal / n as f64 * 100.0),
        remaining_percent: complete.then(|| subtotal / n as f64 * 100.0),
        stale: !complete || members.iter().any(|a| a.stale),
        next_reset_at: next,
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
fn pool_view(
    id: &str,
    config: Option<DevinPoolConfig>,
    records: &[OAuthCredential],
) -> AccountPoolView {
    let accounts: Vec<_> = records
        .iter()
        .map(|a| safe_account(a, config.as_ref()))
        .collect();
    let mut counts = AccountPoolCounts {
        total: accounts.len(),
        ..Default::default()
    };
    for account in &accounts {
        match account.scheduling.as_str() {
            "included" => counts.included += 1,
            "reserve" => counts.reserve += 1,
            "unknown" => counts.unknown += 1,
            "disabled" => counts.disabled += 1,
            "disconnected" => counts.disconnected += 1,
            "auth_required" => counts.auth_required += 1,
            "quota_exhausted" => counts.quota_exhausted += 1,
            "plan_mismatch" => counts.plan_mismatch += 1,
            _ => counts.plan_unverified += 1,
        }
    }
    counts.available = counts.included + counts.reserve + counts.unknown;
    let members: Vec<_> = accounts
        .iter()
        .filter(|a| {
            a.is_enabled
                && a.status != "disconnected"
                && config
                    .as_ref()
                    .is_some_and(|c| a.verified_plan == Some(c.plan))
                && a.scheduling != "plan_mismatch"
        })
        .collect();
    counts.accounting = members.len();
    let quota = AccountPoolQuota {
        daily: config
            .as_ref()
            .filter(|c| c.plan == DevinPlan::Pro)
            .map(|_| aggregate_window(&members, true)),
        weekly: config.as_ref().map(|_| aggregate_window(&members, false)),
        estimated: true,
    };
    let readiness = if counts.included > 0 {
        "eligible"
    } else if counts.unknown > 0 {
        "unknown_usage"
    } else if counts.reserve > 0 {
        "reserve_only"
    } else if counts.total == counts.disconnected {
        "no_account"
    } else if counts.disabled + counts.disconnected == counts.total {
        "disabled"
    } else if counts.auth_required > 0 {
        "auth_required"
    } else if counts.quota_exhausted > 0 {
        "quota_exhausted"
    } else {
        "plan_verification_required"
    }
    .into();
    AccountPoolView {
        provider_id: id.into(),
        config,
        accounts,
        quota,
        counts,
        readiness,
    }
}

impl AdminService {
    /// New-provider login publishes no credential until its selected plan has
    /// been authenticated. Existing provider-only login remains a legacy facade.
    pub async fn create_devin_provider_with_pool(
        &self,
        session_id: &str,
        mut input: CreateProvider,
        config: DevinPoolConfig,
    ) -> anyhow::Result<Provider> {
        config.validate()?;
        let preview = self
            .get_auth_session_record(session_id)
            .await?
            .context("auth session not found")?;
        anyhow::ensure!(
            preview.driver_key == "devin"
                && !is_account_session(&preview)
                && preview.provider_id.is_none(),
            "A fresh Devin login session is required"
        );
        let mut session = self.take_ready_auth_session_record(session_id).await?;
        if is_expired_at(session.expires_at.as_deref()) {
            self.restore_auth_session_record(session).await?;
            anyhow::bail!("auth session expired");
        }
        input.vendor = Some("devin".into());
        input.preset_key = Some("devin".into());
        input.channel = Some("cli".into());
        input.protocol = "devin-chat/get-chat-message/v1".into();
        input.protocol_mode = "fixed".into();
        input.protocol_endpoints.clear();
        input.keys.clear();
        input.base_url = "https://server.codeium.com".into();
        input.auth_mode = "oauth".into();
        input.api_key.clear();
        let provider = match self.create_provider(input).await {
            Ok(provider) => provider,
            Err(error) => {
                self.restore_auth_session_record(session).await?;
                return Err(error);
            }
        };
        let target = AccountSessionIntent {
            provider_id: provider.id.clone(),
            account_id: None,
            expected_version: None,
            plan: config.plan,
            fence: 0,
        };
        session.provider_id = Some(provider.id.clone());
        let result = async {
            self.configure_provider_account_pool(&provider.id, config.clone())
                .await?;
            self.bind_account_session(&provider, &session, &target, &config, "Default account")
                .await?;
            Ok(provider.clone())
        }
        .await;
        if result.is_err() {
            self.delete_provider(&provider.id)
                .await
                .context("Rollback failed account pool creation")?;
            session.provider_id = None;
            self.restore_auth_session_record(session).await?;
        }
        result
    }
    pub(super) async fn account_login_fence(&self, id: &str) -> anyhow::Result<u64> {
        Ok(self
            .get_setting(&format!("provider_account_fence:{id}"))
            .await?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }
    // Caller holds auth_session_controls, serializing this fence with publication.
    pub(super) async fn fence_provider_account_logins(&self, id: &str) -> anyhow::Result<()> {
        let fence = self.account_login_fence(id).await?.saturating_add(1);
        self.gw
            .storage
            .settings()
            .set(&format!("provider_account_fence:{id}"), &fence.to_string())
            .await
    }
    async fn devin_pool_provider(&self, id: &str) -> anyhow::Result<Provider> {
        let provider = self.get_provider(id).await?;
        anyhow::ensure!(
            crate::provider::devin::is_devin(&provider)
                && provider.effective_auth_mode() == "oauth",
            "Account pools require a Devin OAuth provider"
        );
        Ok(provider)
    }
    pub(crate) async fn provider_pool_config(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<DevinPoolConfig>> {
        self.get_setting(&format!("{POOL_SETTING_PREFIX}{id}"))
            .await?
            .map(|raw| {
                let config: DevinPoolConfig = serde_json::from_str(&raw)?;
                config.validate()?;
                Ok(config)
            })
            .transpose()
    }
    pub async fn get_provider_account_pool(&self, id: &str) -> anyhow::Result<AccountPoolView> {
        self.devin_pool_provider(id).await?;
        let config = self.provider_pool_config(id).await?;
        let accounts = self
            .gw
            .storage
            .oauth_credentials()
            .list_accounts(id)
            .await?;
        anyhow::ensure!(
            config.is_some() || accounts.len() <= 1,
            "AMBIGUOUS_ACCOUNT: an unconfigured provider cannot select among multiple accounts"
        );
        let view = pool_view(id, config, &accounts);
        self.publish_pool_scheduling(&view, &accounts);
        Ok(view)
    }
    fn publish_pool_scheduling(&self, view: &AccountPoolView, accounts: &[OAuthCredential]) {
        let mut representative = Vec::new();
        let mut best = -1.0;
        for account in accounts {
            let state = state(account);
            let class = scheduling(account, &state, view.config.as_ref());
            if class != "included" {
                continue;
            }
            let tiers = state.usage.as_ref().map(usage_tiers).unwrap_or_default();
            let score = score_provider(&tiers, Utc::now())
                .map(|s| dynamic_weight(s.rate) * window_boost(s.window))
                .unwrap_or(0.0);
            if score > best {
                best = score;
                representative = tiers;
            }
        }
        self.gw.quota_registry.publish_pool(
            &view.provider_id,
            view.counts.available > 0,
            view.counts.included == 0 && view.counts.unknown == 0 && view.counts.reserve > 0,
            &representative,
            (view.counts.available == 0).then_some(view.readiness.as_str()),
        );
    }
    pub async fn configure_provider_account_pool(
        &self,
        id: &str,
        config: DevinPoolConfig,
    ) -> anyhow::Result<AccountPoolView> {
        self.devin_pool_provider(id).await?;
        config.validate()?;
        // Serialize with all session publications, including first-account adds.
        let controls = self.gw.auth_session_controls.lock().await;
        let old = self.provider_pool_config(id).await?;
        let accounts = self
            .gw
            .storage
            .oauth_credentials()
            .list_accounts(id)
            .await?;
        if old.as_ref().is_some_and(|old| old.plan != config.plan)
            && accounts.iter().any(|a| a.status != "disconnected")
        {
            anyhow::bail!("POOL_PLAN_LOCKED: disconnect all accounts before changing the plan");
        }
        self.gw
            .storage
            .settings()
            .set(
                &format!("{POOL_SETTING_PREFIX}{id}"),
                &serde_json::to_string(&config)?,
            )
            .await?;
        drop(controls);
        self.bump_config_epoch().await?;
        self.gw.quota_registry.invalidate(id);
        for account in accounts {
            self.gw
                .account_quota_registry
                .request_refresh(&account.account_id);
        }
        self.gw.quota_registry.request_refresh(id);
        self.get_provider_account_pool(id).await
    }
    pub async fn init_provider_account_session(
        &self,
        id: &str,
        account_id: Option<&str>,
        expected_version: Option<i32>,
    ) -> anyhow::Result<AuthSessionInitData> {
        let provider = self.devin_pool_provider(id).await?;
        let config = self
            .provider_pool_config(id)
            .await?
            .context("POOL_NOT_CONFIGURED: select a plan before adding accounts")?;
        anyhow::ensure!(
            account_id.is_some() == expected_version.is_some(),
            "Reauthorization requires account_id and expected_version together"
        );
        if let Some(account_id) = account_id {
            let account = self
                .gw
                .storage
                .oauth_credentials()
                .get_account(id, account_id)
                .await?
                .context("account not found")?;
            anyhow::ensure!(
                Some(account.status_version) == expected_version
                    && account.status != "disconnected",
                "ACCOUNT_VERSION_CONFLICT: account changed"
            );
        }
        let fence = self.account_login_fence(id).await?;
        let init = self.init_oauth_session("devin", provider.use_proxy).await?;
        let mut sessions = self.gw.auth_sessions.write().await;
        let session = sessions
            .get_mut(&init.session_id)
            .context("auth session disappeared")?;
        let mut context: Value =
            serde_json::from_str(session.context_json.as_deref().unwrap_or("{}"))?;
        context[SESSION_INTENT] = serde_json::to_value(AccountSessionIntent {
            provider_id: id.into(),
            account_id: account_id.map(str::to_owned),
            expected_version,
            plan: config.plan,
            fence,
        })?;
        session.provider_id = Some(id.into());
        session.context_json = Some(context.to_string());
        Ok(init)
    }
    pub async fn bind_provider_account(
        &self,
        id: &str,
        session_id: &str,
        label: Option<&str>,
    ) -> anyhow::Result<ProviderAccountView> {
        let provider = self.devin_pool_provider(id).await?;
        let label = normalize_name(label.unwrap_or("Default account"), "account label")?;
        anyhow::ensure!(label.len() <= 128, "Account label is too long");
        let preview = self
            .get_auth_session_record(session_id)
            .await?
            .context("auth session not found")?;
        let target = intent(&preview)?;
        anyhow::ensure!(
            target.provider_id == id && preview.provider_id.as_deref() == Some(id),
            "ACCOUNT_SESSION_TARGET_MISMATCH"
        );
        super::oauth::validate_oauth_session_for_provider(&preview, &provider)?;
        let config = self
            .provider_pool_config(id)
            .await?
            .context("POOL_NOT_CONFIGURED")?;
        anyhow::ensure!(config.plan == target.plan, "Pool plan changed during login");
        let session = self.take_ready_auth_session_record(session_id).await?;
        let result = self
            .bind_account_session(&provider, &session, &target, &config, &label)
            .await;
        if result.is_err() {
            self.restore_auth_session_record(session).await?;
        }
        result
    }
    async fn bind_account_session(
        &self,
        provider: &Provider,
        session: &AuthSession,
        target: &AccountSessionIntent,
        config: &DevinPoolConfig,
        label: &str,
    ) -> anyhow::Result<ProviderAccountView> {
        anyhow::ensure!(
            !is_expired_at(session.expires_at.as_deref()),
            "auth session expired"
        );
        let bundle = parse_auth_session_bundle(session)?;
        let token = bundle
            .access_token
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .context("auth session missing access token")?;
        let previous = if let Some(id) = &target.account_id {
            let account = self
                .gw
                .storage
                .oauth_credentials()
                .get_account(&provider.id, id)
                .await?
                .context("account not found")?;
            anyhow::ensure!(
                Some(account.status_version) == target.expected_version
                    && account.status != "disconnected",
                "ACCOUNT_VERSION_CONFLICT: account changed during login"
            );
            if let Some(subject) = account.subject_id.as_deref().filter(|s| !s.is_empty()) {
                anyhow::ensure!(
                    bundle.subject_id.as_deref() == Some(subject),
                    "ACCOUNT_IDENTITY_MISMATCH: reauthorization must verify the same identity"
                );
            }
            Some(account)
        } else {
            None
        };
        let permit = STATUS_CONCURRENCY.acquire().await?;
        let usage = crate::provider::devin::fetch_usage(&self.gw, provider, token.trim()).await?;
        drop(permit);
        validate_devin_plan(config.plan, &usage)?;
        let mut controls = self.gw.auth_session_controls.lock().await;
        anyhow::ensure!(
            !is_expired_at(session.expires_at.as_deref()),
            "auth session expired during verification"
        );
        anyhow::ensure!(
            controls
                .get(&session.id)
                .is_some_and(|c| !c.cancelled && c.generation == session.generation),
            "auth session was cancelled or superseded"
        );
        anyhow::ensure!(
            self.provider_pool_config(&provider.id)
                .await?
                .as_ref()
                .is_some_and(|c| c.plan == config.plan),
            "Pool plan changed during login"
        );
        anyhow::ensure!(
            self.account_login_fence(&provider.id).await? == target.fence,
            "Account login was superseded by provider logout"
        );
        let store = self.gw.storage.oauth_credentials();
        let input = upsert_credential_from_bundle(&session.driver_key, &session.scheme, &bundle);
        let account = if let Some(previous) = previous {
            store
                .replace_account(
                    &provider.id,
                    &previous.account_id,
                    previous.status_version,
                    input,
                )
                .await?
                .ok_or_else(conflict)?
        } else {
            store.create_account(&provider.id, label, input).await?
        };
        let mut fresh = DevinAccountState::default();
        fresh.verified_plan = Some(config.plan);
        fresh.usage = Some(usage);
        fresh.observed_at = Some(Utc::now().timestamp_millis());
        // The preflight predates the credential replacement. A rejection from
        // an old in-flight request may have landed in that interval. Require a
        // post-replacement authoritative observation before releasing reauth.
        if target.account_id.is_some() {
            fresh.hard_quota = true;
            fresh.hard_quota_reason = Some("reauthorization_verification".into());
        }
        // Publish only after successful authenticated probe. A failed CAS cannot
        // overwrite a concurrent logout, disable, or newer request outcome.
        let saved = store
            .compare_account_state(
                &provider.id,
                &account.account_id,
                account.status_version,
                &account.account_state,
                &serde_json::to_string(&fresh)?,
            )
            .await?;
        // Credential publication consumes the result even if state publication
        // loses a CAS or a later cache invalidation fails.
        if let Some(control) = controls.get_mut(&session.id) {
            control.generation += 1;
        }
        drop(controls);
        if !saved {
            return Err(conflict());
        }
        self.gw.account_pool.release_binding(&account.account_id);
        if target.account_id.is_some() {
            let _ = self
                .refresh_provider_account_usage(&provider.id, &account.account_id)
                .await;
        }
        self.bump_config_epoch().await?;
        let view = self.get_provider_account_pool(&provider.id).await?;
        view.accounts
            .into_iter()
            .find(|a| a.account_id == account.account_id)
            .context("account disappeared")
    }
    pub async fn update_provider_account(
        &self,
        id: &str,
        account_id: &str,
        expected_version: i32,
        mut input: OAuthAccountUpdate,
    ) -> anyhow::Result<ProviderAccountView> {
        self.devin_pool_provider(id).await?;
        if let Some(label) = &input.label {
            let label = normalize_name(label, "account label")?;
            anyhow::ensure!(label.len() <= 128, "Account label is too long");
            input.label = Some(label);
        }
        let account = self
            .gw
            .storage
            .oauth_credentials()
            .update_account(id, account_id, expected_version, input)
            .await?
            .ok_or_else(conflict)?;
        self.bump_config_epoch().await?;
        self.gw.account_quota_registry.invalidate(account_id);
        self.gw.account_pool.release_binding(account_id);
        self.get_provider_account_pool(id).await?;
        Ok(safe_account(
            &account,
            self.provider_pool_config(id).await?.as_ref(),
        ))
    }
    pub async fn disconnect_provider_account(
        &self,
        id: &str,
        account_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<()> {
        self.devin_pool_provider(id).await?;
        anyhow::ensure!(
            self.gw
                .storage
                .oauth_credentials()
                .disconnect_account(id, account_id, expected_version)
                .await?,
            "ACCOUNT_VERSION_CONFLICT: account changed"
        );
        self.gw.account_quota_registry.remove(account_id);
        self.gw.account_pool.release_binding(account_id);
        self.bump_config_epoch().await?;
        self.get_provider_account_pool(id).await?;
        Ok(())
    }
    pub(crate) async fn resolve_provider_account_runtime(
        &self,
        provider: &Provider,
        account: &OAuthCredential,
    ) -> anyhow::Result<ResolvedProviderRuntime> {
        anyhow::ensure!(
            account.provider_id == provider.id
                && account.is_enabled
                && account.status == "connected",
            "account is unavailable"
        );
        anyhow::ensure!(
            crate::provider::devin::is_devin(provider) && provider.effective_auth_mode() == "oauth",
            "Account runtime currently supports Devin OAuth only"
        );
        crate::provider::devin::validate(provider)?;
        // Historical singleton backfills left driver_key empty. Only the
        // validated Devin parent may supply that missing identity.
        anyhow::ensure!(
            account.driver_key.trim().is_empty() || account.driver_key == "devin",
            "Account runtime currently supports Devin only"
        );
        let credential = stored_credential_from_oauth(account, "devin");
        let access_token = credential
            .access_token
            .clone()
            .context("account credential is missing")?;
        let driver = auth::build_driver("devin").context("Devin auth driver missing")?;
        Ok(ResolvedProviderRuntime {
            access_token,
            binding: driver.bind_runtime(provider, &credential)?,
            credential: Some(credential),
        })
    }
    pub async fn refresh_provider_account_usage(
        &self,
        id: &str,
        account_id: &str,
    ) -> anyhow::Result<ProviderAccountView> {
        let before = self
            .gw
            .storage
            .oauth_credentials()
            .get_account(id, account_id)
            .await?
            .context("account not found")?;
        let lock = self.gw.account_quota_registry.refresh_lock(account_id);
        let _guard = lock.lock().await;
        let current = self
            .gw
            .storage
            .oauth_credentials()
            .get_account(id, account_id)
            .await?
            .context("account not found")?;
        if current.status_version != before.status_version {
            return Err(conflict());
        }
        if current.account_state != before.account_state {
            // Join a completed concurrent observation rather than immediately
            // issuing the same expensive status RPC again.
            return Ok(safe_account(
                &current,
                self.provider_pool_config(id).await?.as_ref(),
            ));
        }
        self.refresh_account_usage_locked(id, account_id).await
    }
    async fn refresh_account_usage_locked(
        &self,
        id: &str,
        account_id: &str,
    ) -> anyhow::Result<ProviderAccountView> {
        let provider = self.devin_pool_provider(id).await?;
        let config = self.provider_pool_config(id).await?;
        let store = self.gw.storage.oauth_credentials();
        let account = store
            .get_account(id, account_id)
            .await?
            .context("account not found")?;
        let runtime = self
            .resolve_provider_account_runtime(&provider, &account)
            .await?;
        let mut next = state(&account);
        let permit = STATUS_CONCURRENCY.acquire().await?;
        let result =
            crate::provider::devin::fetch_usage(&self.gw, &provider, &runtime.access_token).await;
        drop(permit);
        let result_error = match result {
            Ok(usage) => {
                next.plan_mismatch = config
                    .as_ref()
                    .is_some_and(|c| validate_devin_plan(c.plan, &usage).is_err());
                if !next.plan_mismatch {
                    if let Some(config) = &config {
                        next.verified_plan = Some(config.plan);
                    }
                }
                next.usage = Some(usage);
                next.observed_at = Some(Utc::now().timestamp_millis());
                next.usage_stale = false;
                // A usage observation never revives an auth-invalid account.
                next.hard_quota = false;
                next.hard_quota_reason = None;
                None
            }
            Err(error) => {
                next.usage_stale = true;
                if error
                    .to_string()
                    .starts_with("Devin authentication failed (HTTP 401)")
                {
                    next.auth_invalid = true;
                }
                Some(error)
            }
        };
        if !store
            .compare_account_state(
                id,
                account_id,
                account.status_version,
                &account.account_state,
                &serde_json::to_string(&next)?,
            )
            .await?
        {
            return Err(conflict());
        }
        let mut updated = account;
        updated.account_state = serde_json::to_string(&next)?;
        if result_error.is_some() {
            self.gw
                .account_quota_registry
                .record_query_failure(account_id);
        } else {
            let class = scheduling(&updated, &next, config.as_ref());
            self.gw.account_quota_registry.observe_with_reserve(
                account_id,
                &next.usage.as_ref().map(usage_tiers).unwrap_or_default(),
                Some(matches!(class, "included" | "unknown" | "reserve")),
                class == "reserve",
            );
        }
        self.get_provider_account_pool(id).await?;
        if let Some(error) = result_error {
            return Err(error);
        }
        Ok(safe_account(&updated, config.as_ref()))
    }
    pub(super) fn account_pool_provider_usage(&self, view: AccountPoolView) -> ProviderUsage {
        let mut tiers = Vec::new();
        let mut balances = Vec::new();
        if view.config.is_some() {
            for (name, window) in [
                ("daily", &view.quota.daily),
                ("weekly_limit", &view.quota.weekly),
            ] {
                if let Some(window) = window
                    && let Some(used_percent) = window.used_percent
                {
                    tiers.push(ProviderUsageTier {
                        name: name.into(),
                        used_percent,
                        resets_at: (window.reset_mode == "aligned")
                            .then(|| window.next_reset_at.clone())
                            .flatten(),
                    });
                }
            }
        } else if let Some(account) = view.accounts.first().filter(|_| view.accounts.len() == 1) {
            for (name, window) in [("daily", &account.daily), ("weekly_limit", &account.weekly)] {
                if let Some(window) = window {
                    tiers.push(ProviderUsageTier {
                        name: name.into(),
                        used_percent: 100.0 - f64::from(window.remaining_percent),
                        resets_at: window.resets_at.clone(),
                    });
                }
            }
        }
        let reserve: Vec<_> = view
            .accounts
            .iter()
            .filter(|a| {
                a.is_enabled && a.status != "disconnected" && a.scheduling != "plan_mismatch"
            })
            .collect();
        if !reserve.is_empty() && reserve.iter().all(|a| a.reserve_usd.is_some()) {
            let total = reserve.iter().filter_map(|a| a.reserve_usd).sum();
            balances.push(ProviderUsageBalance {
                currency: "USD".into(),
                total,
                granted: 0.0,
                topped_up: total,
            });
        }
        ProviderUsage {
            provider_id: view.provider_id.clone(),
            kind: "devin_plan".into(),
            site: "global".into(),
            level: view.config.as_ref().map(|c| {
                match c.plan {
                    DevinPlan::Pro => "Pro",
                    DevinPlan::Max => "Max",
                }
                .into()
            }),
            tiers,
            balances,
            spends: Vec::new(),
            is_available: Some(view.counts.available > 0),
            scheduling: self.gw.quota_registry.snapshot(&view.provider_id),
            queried_at: Utc::now().to_rfc3339(),
            account_pool: Some(view),
        }
    }
    pub async fn refresh_provider_account_pool_usage(
        &self,
        id: &str,
    ) -> anyhow::Result<AccountPoolView> {
        self.refresh_account_pool_usage(id, false).await
    }
    pub(super) async fn refresh_account_pool_usage(
        &self,
        id: &str,
        due_only: bool,
    ) -> anyhow::Result<AccountPoolView> {
        self.devin_pool_provider(id).await?;
        let accounts = self
            .gw
            .storage
            .oauth_credentials()
            .list_accounts(id)
            .await?;
        if self.provider_pool_config(id).await?.is_none() && accounts.len() > 1 {
            anyhow::bail!(
                "AMBIGUOUS_ACCOUNT: configure the account pool before refreshing multiple accounts"
            );
        }
        let jobs = accounts.into_iter().filter(|a| {
            a.is_enabled
                && a.status == "connected"
                && !state(a).auth_invalid
                && (!due_only || self.gw.account_quota_registry.is_due(&a.account_id))
        });
        stream::iter(jobs).for_each_concurrent(4, |account| async move {
            if self.refresh_provider_account_usage(id, &account.account_id).await.is_err() {
                tracing::debug!(provider_id = id, account_id = %account.account_id, "Account usage refresh failed; preserving last confirmed observation");
            }
        }).await;
        self.get_provider_account_pool(id).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config(plan: DevinPlan) -> DevinPoolConfig {
        DevinPoolConfig {
            plan,
            strategy: DevinPoolStrategy::RoundRobin,
            session_affinity: true,
            affinity_ttl_seconds: 3600,
            max_account_attempts: 3,
            reserve_policy: DevinReservePolicy::Disabled,
        }
    }
    fn usage(plan: DevinPlan, daily: u8, weekly: u8) -> DevinUsage {
        DevinUsage {
            plan_name: Some(
                match plan {
                    DevinPlan::Pro => "Pro",
                    DevinPlan::Max => "Max",
                }
                .into(),
            ),
            daily: (plan == DevinPlan::Pro).then_some(DevinQuota {
                remaining_percent: daily,
                resets_at_unix: Some(2_000_000_000),
            }),
            weekly: Some(DevinQuota {
                remaining_percent: weekly,
                resets_at_unix: Some(2_000_600_000),
            }),
            overage_usd: Some(0.0),
        }
    }
    fn account(id: &str, plan: DevinPlan, daily: u8, weekly: u8) -> OAuthCredential {
        let state = DevinAccountState {
            verified_plan: Some(plan),
            usage: Some(usage(plan, daily, weekly)),
            observed_at: Some(Utc::now().timestamp_millis()),
            ..Default::default()
        };
        OAuthCredential {
            account_id: id.into(),
            provider_id: "pool".into(),
            label: id.into(),
            is_enabled: true,
            priority: 0,
            account_state: serde_json::to_string(&state).unwrap(),
            driver_key: "devin".into(),
            scheme: "oauth_auth_code_pkce".into(),
            access_token: format!("SECRET-{id}"),
            refresh_token: None,
            expires_at: None,
            resource_url: None,
            subject_id: None,
            scopes: "[]".into(),
            meta: "{\"token\":\"SECRET-META\"}".into(),
            status: "connected".into(),
            status_version: 1,
            last_error: Some("SECRET-ERROR".into()),
            last_refresh_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
    fn mutate(account: &mut OAuthCredential, update: impl FnOnce(&mut DevinAccountState)) {
        let mut value = state(account);
        update(&mut value);
        account.account_state = serde_json::to_string(&value).unwrap();
    }
    fn provider() -> Provider {
        Provider {
            id: "pool".into(),
            name: "Devin pool".into(),
            vendor: Some("devin".into()),
            protocol: "devin-chat/get-chat-message/v1".into(),
            base_url: "https://server.codeium.com".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: Some("devin".into()),
            channel: Some("cli".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
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
                "pool",
                label,
                UpsertOAuthCredential {
                    driver_key: "devin".into(),
                    scheme: "oauth_auth_code_pkce".into(),
                    access_token: format!("SECRET-{label}"),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
    #[test]
    fn homogeneous_units_and_max_daily_absence() {
        let records = vec![
            account("a", DevinPlan::Pro, 60, 20),
            account("b", DevinPlan::Pro, 30, 50),
            account("c", DevinPlan::Pro, 90, 80),
        ];
        let view = pool_view("pool", Some(config(DevinPlan::Pro)), &records);
        let daily = view.quota.daily.unwrap();
        let weekly = view.quota.weekly.unwrap();
        assert_eq!(daily.total_units, 3);
        assert!((daily.remaining_units.unwrap() - 1.8).abs() < 1e-9);
        assert!((daily.remaining_percent.unwrap() - 60.0).abs() < 1e-9);
        assert_eq!(weekly.remaining_percent, Some(50.0));
        assert_eq!(view.counts.available, 3);
        let max = pool_view(
            "pool",
            Some(config(DevinPlan::Max)),
            &[account("max", DevinPlan::Max, 0, 50)],
        );
        assert!(max.quota.daily.is_none());
        assert_eq!(max.quota.weekly.unwrap().remaining_percent, Some(50.0));
    }
    #[test]
    fn cross_window_exhaustion_never_becomes_provider_availability() {
        let view = pool_view(
            "pool",
            Some(config(DevinPlan::Pro)),
            &[
                account("a", DevinPlan::Pro, 100, 0),
                account("b", DevinPlan::Pro, 0, 100),
            ],
        );
        assert_eq!(view.quota.daily.unwrap().remaining_percent, Some(50.0));
        assert_eq!(view.quota.weekly.unwrap().remaining_percent, Some(50.0));
        assert_eq!(view.counts.accounting, 2);
        assert_eq!(view.counts.available, 0);
        assert_eq!(view.readiness, "quota_exhausted");
    }
    #[test]
    fn auth_failure_keeps_capacity_but_unknown_does_not_fabricate_totals() {
        let mut failed = account("failed", DevinPlan::Pro, 0, 20);
        mutate(&mut failed, |state| {
            state.auth_invalid = true;
            state.usage_stale = true;
        });
        let mut unknown = account("unknown", DevinPlan::Pro, 50, 50);
        mutate(&mut unknown, |state| state.usage = None);
        let mut disabled = account("disabled", DevinPlan::Pro, 100, 100);
        disabled.is_enabled = false;
        let mut disconnected = account("gone", DevinPlan::Pro, 100, 100);
        disconnected.status = "disconnected".into();
        let view = pool_view(
            "pool",
            Some(config(DevinPlan::Pro)),
            &[failed, unknown, disabled, disconnected],
        );
        assert_eq!(view.counts.accounting, 2);
        assert_eq!(view.counts.auth_required, 1);
        let daily = view.quota.daily.unwrap();
        assert_eq!(daily.total_units, 2);
        assert_eq!(daily.known_accounts, 1);
        assert!(daily.remaining_units.is_none() && daily.used_percent.is_none() && daily.stale);
    }
    #[test]
    fn reset_modes_drift_and_redaction() {
        let a = account("a", DevinPlan::Pro, 60, 60);
        let mut b = account("b", DevinPlan::Pro, 40, 40);
        mutate(&mut b, |s| {
            s.usage
                .as_mut()
                .unwrap()
                .daily
                .as_mut()
                .unwrap()
                .resets_at_unix = Some(2_000_000_100)
        });
        let view = pool_view(
            "pool",
            Some(config(DevinPlan::Pro)),
            &[a.clone(), b.clone()],
        );
        let daily = view.quota.daily.as_ref().unwrap();
        assert_eq!(daily.reset_mode, "staggered");
        assert_eq!(daily.next_reset_accounts, 1);
        let serialized = serde_json::to_string(&view).unwrap();
        for secret in [
            "SECRET",
            "access_token",
            "refresh_token",
            "subject_id",
            "meta",
            "last_error",
        ] {
            assert!(!serialized.contains(secret), "leaked {secret}");
        }
        mutate(&mut b, |s| s.plan_mismatch = true);
        assert_eq!(
            pool_view("pool", Some(config(DevinPlan::Pro)), &[a, b])
                .counts
                .accounting,
            1
        );
        let empty = pool_view("pool", Some(config(DevinPlan::Pro)), &[]);
        assert_eq!(empty.quota.daily.unwrap().remaining_percent, None);
    }
    #[test]
    fn plan_validation_and_policy_defaults_fail_closed() {
        assert!(validate_devin_plan(DevinPlan::Pro, &usage(DevinPlan::Pro, 0, 0)).is_ok());
        assert!(validate_devin_plan(DevinPlan::Max, &usage(DevinPlan::Pro, 100, 100)).is_err());
        let mut snapshot = usage(DevinPlan::Max, 0, 50);
        snapshot.plan_name = Some("Teams".into());
        assert!(validate_devin_plan(DevinPlan::Max, &snapshot).is_err());
        snapshot.plan_name = Some("Max".into());
        snapshot.daily = Some(DevinQuota {
            remaining_percent: 0,
            resets_at_unix: None,
        });
        assert!(validate_devin_plan(DevinPlan::Max, &snapshot).is_err());
        let defaults: DevinPoolConfig = serde_json::from_str("{\"plan\":\"pro\"}").unwrap();
        assert_eq!(defaults.max_account_attempts, 3);
        assert_eq!(defaults.affinity_ttl_seconds, 3600);
        assert_eq!(defaults.reserve_policy, DevinReservePolicy::Disabled);
        assert!(serde_json::from_str::<DevinPoolConfig>("{}").is_err());
        let mut invalid = defaults;
        invalid.max_account_attempts = 0;
        assert!(invalid.validate().is_err());
    }
    #[tokio::test]
    async fn settings_cannot_change_populated_plan_and_sessions_have_fixed_targets() {
        let gw = gateway().await;
        let admin = gw.admin();
        assert!(
            admin
                .init_provider_account_session("pool", None, None)
                .await
                .is_err()
        );
        admin
            .configure_provider_account_pool("pool", config(DevinPlan::Pro))
            .await
            .unwrap();
        let account = add(&gw, "A").await;
        let raw = serde_json::to_string(&config(DevinPlan::Max)).unwrap();
        assert!(
            admin
                .set_setting("provider_account_pool:pool", &raw)
                .await
                .unwrap_err()
                .to_string()
                .contains("POOL_PLAN_LOCKED")
        );
        let init = admin
            .init_provider_account_session(
                "pool",
                Some(&account.account_id),
                Some(account.status_version),
            )
            .await
            .unwrap();
        let session = admin
            .get_auth_session_record(&init.session_id)
            .await
            .unwrap()
            .unwrap();
        let target = intent(&session).unwrap();
        assert_eq!(target.provider_id, "pool");
        assert_eq!(target.account_id, Some(account.account_id.clone()));
        admin
            .disconnect_provider_account("pool", &account.account_id, account.status_version)
            .await
            .unwrap();
        assert!(
            admin
                .init_provider_account_session(
                    "pool",
                    Some(&account.account_id),
                    Some(account.status_version)
                )
                .await
                .is_err()
        );
        admin.cancel_oauth_session(&init.session_id).await.unwrap();
        assert!(
            admin
                .get_auth_session_record(&init.session_id)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn legacy_ambiguity_is_rejected_before_session_consumption() {
        let gw = gateway().await;
        let admin = gw.admin();
        add(&gw, "A").await;
        add(&gw, "B").await;
        let init = admin.init_oauth_session("devin", false).await.unwrap();
        let error = admin
            .bind_provider_with_oauth_session("pool", &init.session_id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("AMBIGUOUS_ACCOUNT"));
        assert!(
            admin
                .get_auth_session_record(&init.session_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            admin
                .logout_provider_oauth("pool")
                .await
                .unwrap_err()
                .to_string()
                .contains("AMBIGUOUS_ACCOUNT")
        );
        assert_eq!(
            gw.storage
                .oauth_credentials()
                .list_accounts("pool")
                .await
                .unwrap()
                .len(),
            2
        );
    }
    #[tokio::test]
    async fn provider_only_management_rejects_ambiguity_but_static_catalog_does_not() {
        let gw = gateway().await;
        let admin = gw.admin();
        add(&gw, "A").await;
        add(&gw, "B").await;
        admin
            .configure_provider_account_pool("pool", config(DevinPlan::Pro))
            .await
            .unwrap();
        assert!(!admin.get_provider_models("pool").await.unwrap().is_empty());
        let error = admin.test_provider("pool").await.unwrap_err();
        assert!(error.to_string().contains("AMBIGUOUS_ACCOUNT"));
        let error = admin
            .probe_provider_models("pool", Some(vec!["claude-sonnet-4".into()]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("AMBIGUOUS_ACCOUNT"));
        let error = admin.copy_provider("pool").await.unwrap_err();
        assert!(error.to_string().contains("AMBIGUOUS_ACCOUNT"));
        assert_eq!(admin.list_providers().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn legacy_runtime_is_compatible_but_disable_and_hard_blocks_are_enforced() {
        let gw = gateway().await;
        let admin = gw.admin();
        let account = add(&gw, "A").await;
        assert!(admin.resolve_provider_runtime(&provider()).await.is_ok());
        let updated = admin
            .update_provider_account(
                "pool",
                &account.account_id,
                account.status_version,
                OAuthAccountUpdate {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(admin.resolve_provider_runtime(&provider()).await.is_err());
        let updated = admin
            .update_provider_account(
                "pool",
                &account.account_id,
                updated.status_version,
                OAuthAccountUpdate {
                    is_enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let current = gw
            .storage
            .oauth_credentials()
            .get_account("pool", &account.account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            gw.storage
                .oauth_credentials()
                .compare_account_state(
                    "pool",
                    &account.account_id,
                    updated.status_version,
                    &current.account_state,
                    "{\"hard_quota\":true}"
                )
                .await
                .unwrap()
        );
        assert!(admin.resolve_provider_runtime(&provider()).await.is_err());
    }
    #[tokio::test]
    async fn migrated_empty_driver_uses_only_the_validated_devin_parent() {
        let gw = gateway().await;
        let admin = gw.admin();
        let mut migrated = account("legacy", DevinPlan::Pro, 50, 50);
        migrated.driver_key.clear();
        assert!(
            admin
                .resolve_provider_account_runtime(&provider(), &migrated)
                .await
                .is_ok()
        );
        migrated.driver_key = "codex".into();
        assert!(
            admin
                .resolve_provider_account_runtime(&provider(), &migrated)
                .await
                .is_err()
        );
        migrated.driver_key.clear();
        let mut foreign = provider();
        foreign.vendor = Some("codex".into());
        assert!(
            admin
                .resolve_provider_account_runtime(&foreign, &migrated)
                .await
                .is_err()
        );
        let mut foreign = provider();
        foreign.base_url = "https://other.invalid".into();
        assert!(
            admin
                .resolve_provider_account_runtime(&foreign, &migrated)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn old_generation_and_state_cannot_overwrite_new_outcomes() {
        let gw = gateway().await;
        let admin = gw.admin();
        admin
            .configure_provider_account_pool("pool", config(DevinPlan::Pro))
            .await
            .unwrap();
        let account = add(&gw, "A").await;
        let store = gw.storage.oauth_credentials();
        let newer = "{\"hard_quota\":true}";
        assert!(
            store
                .compare_account_state(
                    "pool",
                    &account.account_id,
                    account.status_version,
                    &account.account_state,
                    newer
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .compare_account_state(
                    "pool",
                    &account.account_id,
                    account.status_version,
                    &account.account_state,
                    "{}"
                )
                .await
                .unwrap()
        );
        admin
            .disconnect_provider_account("pool", &account.account_id, account.status_version)
            .await
            .unwrap();
        assert!(
            !store
                .compare_account_state(
                    "pool",
                    &account.account_id,
                    account.status_version,
                    newer,
                    "{}"
                )
                .await
                .unwrap()
        );
    }
}

fn usage_tiers(usage: &DevinUsage) -> Vec<QuotaTierObservation> {
    [
        ("daily", usage.daily.as_ref()),
        ("weekly_limit", usage.weekly.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, quota)| {
        quota.map(|quota| QuotaTierObservation {
            name: name.into(),
            used_percent: 100.0 - f64::from(quota.remaining_percent),
            resets_at: reset_at(quota.resets_at_unix),
        })
    })
    .collect()
}
