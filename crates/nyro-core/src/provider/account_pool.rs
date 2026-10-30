//! Account-scoped admission and affinity for Devin, Kiro and Claude subscriptions.
//! Credentials stay in storage; this registry contains only bounded scheduling
//! hints. A request always carries an immutable credential generation.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::admin::{DevinPlan, DevinPoolConfig, DevinPoolStrategy, DevinReservePolicy};
use crate::db::models::OAuthCredential;
use crate::protocol::ir::AiRequest;
use crate::provider::devin::DevinUsage;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct DevinAccountState {
    pub verified_plan: Option<DevinPlan>,
    pub usage: Option<DevinUsage>,
    /// UTC Unix milliseconds. Never interpreted as a token expiry.
    pub observed_at: Option<i64>,
    pub usage_stale: bool,
    pub auth_invalid: bool,
    pub hard_quota: bool,
    pub hard_quota_reason: Option<String>,
    /// Every rejection advances this even if the semantic block is unchanged.
    /// Otherwise an overlapping status probe could CAS across a repeated quota
    /// refusal and accidentally clear newer evidence.
    pub rejection_revision: u64,
    pub plan_mismatch: bool,
}

impl DevinAccountState {
    pub fn parse(raw: &str) -> Self {
        let mut state: Self = serde_json::from_str(raw).unwrap_or_default();
        if let Some(usage) = &mut state.usage {
            state.verified_plan = Some(usage.normalize_for_pool());
        }
        // Older homogeneous pools quarantined otherwise valid accounts when
        // their plan differed from the Provider. That restriction no longer
        // exists; authentication and quota rejections remain independent.
        state.plan_mismatch = false;
        state
    }

    pub fn plan(&self) -> Option<DevinPlan> {
        self.usage
            .as_ref()
            .map(|usage| super::devin::classify_plan_name(usage.plan_name.as_deref()))
            .or(self.verified_plan)
    }

    pub fn included_exhausted(&self) -> bool {
        self.usage.as_ref().is_some_and(|usage| {
            (self.plan() != Some(DevinPlan::Max)
                && usage
                    .daily
                    .as_ref()
                    .is_some_and(|quota| quota.remaining_percent == 0))
                || usage
                    .weekly
                    .as_ref()
                    .is_some_and(|quota| quota.remaining_percent == 0)
        })
    }

    pub fn usage_complete(&self) -> bool {
        self.usage.as_ref().is_some_and(|usage| {
            usage.weekly.is_some() && (self.plan() == Some(DevinPlan::Max) || usage.daily.is_some())
        })
    }

    /// Lower class wins. Unknown usage never overrides an explicit hard block.
    pub fn cost_class(&self, config: &DevinPoolConfig) -> Option<u8> {
        if self.auth_invalid || self.plan().is_none() || self.hard_quota {
            return None;
        }
        if self.included_exhausted() {
            return (config.reserve_policy == DevinReservePolicy::Fallback
                && self
                    .usage
                    .as_ref()
                    .and_then(|usage| usage.overage_usd)
                    .is_some_and(|balance| balance > 0.0))
            .then_some(2);
        }
        Some(if self.usage_complete() { 0 } else { 1 })
    }
}

/// Kiro keeps real resource allowances instead of Devin plan-equivalent units.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct KiroAccountState {
    pub usage: Option<super::kiro::KiroUsage>,
    pub observed_at: Option<i64>,
    pub usage_stale: bool,
    pub auth_invalid: bool,
    pub hard_quota: bool,
    pub hard_quota_reason: Option<String>,
    pub rejection_revision: u64,
}

impl KiroAccountState {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn included_exhausted(&self) -> bool {
        self.usage.as_ref().is_some_and(|usage| {
            usage
                .resource_pools()
                .iter()
                .any(|pool| pool.used >= pool.limit)
        })
    }

    pub fn usage_complete(&self) -> bool {
        self.usage
            .as_ref()
            .is_some_and(|usage| !usage.resource_pools().is_empty())
    }

    pub fn cost_class(&self, config: &DevinPoolConfig) -> Option<u8> {
        if self.auth_invalid || self.hard_quota {
            return None;
        }
        if self.included_exhausted() {
            return (config.reserve_policy == DevinReservePolicy::Fallback
                && self
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.overage_enabled))
            .then_some(2);
        }
        Some(if self.usage_complete() { 0 } else { 1 })
    }
}

/// Claude quotas are percentages of real windows, never plan-equivalent units.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ClaudeAccountState {
    pub usage: Option<super::anthropic::claude_code::usage::ClaudeUsage>,
    pub observed_at: Option<i64>,
    pub usage_stale: bool,
    pub auth_invalid: bool,
    pub hard_quota: bool,
    pub hard_quota_reason: Option<String>,
    pub rejection_revision: u64,
}

impl ClaudeAccountState {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn included_exhausted(&self) -> bool {
        self.exhausted_for_model(None)
    }

    fn exhausted_for_model(&self, model: Option<&str>) -> bool {
        self.usage.as_ref().is_some_and(|usage| {
            usage.windows.iter().any(|window| {
                claude_window_applies(&window.name, model) && window.used_percent >= 100.0
            })
        })
    }

    pub fn usage_complete(&self) -> bool {
        self.usage.as_ref().is_some_and(|usage| {
            ["five_hour", "weekly_limit"].iter().all(|name| {
                usage
                    .windows
                    .iter()
                    .any(|w| w.name == *name && w.used_percent.is_finite())
            })
        })
    }

    /// Only the global weekly window drives this preference. Observation time,
    /// five-hour and model-specific resets are unrelated to the weekly deadline.
    fn next_weekly_reset_at(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let window = self
            .usage
            .as_ref()?
            .windows
            .iter()
            .find(|w| w.name == "weekly_limit")?;
        let reset = chrono::DateTime::parse_from_rfc3339(window.resets_at.as_deref()?.trim())
            .ok()?
            .with_timezone(&chrono::Utc);
        // A passed deadline does not prove that the upstream renewed the quota.
        (reset > now).then_some(reset)
    }

    pub fn cost_class(&self, config: &DevinPoolConfig) -> Option<u8> {
        self.cost_class_for_model(config, None)
    }

    pub fn cost_class_for_model(
        &self,
        _config: &DevinPoolConfig,
        model: Option<&str>,
    ) -> Option<u8> {
        // extra_usage.is_enabled alone is not evidence of an available paid
        // balance. Until payment capacity is verifiable, never select overage,
        // even when the operator enables the generic reserve fallback policy.
        if self.auth_invalid || self.hard_quota || self.exhausted_for_model(model) {
            return None;
        }
        Some(if self.usage_complete() { 0 } else { 1 })
    }
}

/// Feature windows are model scoped; oauth_apps applies to this OAuth channel.
/// With no model, report provider-wide windows only, not an Opus-only refusal.
fn claude_window_applies(name: &str, model: Option<&str>) -> bool {
    let Some(feature) = name.strip_prefix("feature:") else {
        return true;
    };
    let feature = feature.split(':').next().unwrap_or_default();
    if feature == "oauth_apps" {
        return true;
    }
    model.is_some_and(|model| {
        let model = model.to_ascii_lowercase().replace('-', "_");
        !feature.is_empty() && model.contains(feature)
    })
}

pub(crate) fn supports_pool(provider: &crate::db::models::Provider) -> bool {
    provider.effective_auth_mode() == "oauth"
        && (super::devin::is_devin(provider)
            || super::kiro::is_kiro(provider)
            || super::anthropic::claude_code::egress::is_claude_code_channel(provider))
}

pub(crate) fn account_cost_class(
    account: &OAuthCredential,
    config: &DevinPoolConfig,
) -> Option<u8> {
    if matches!(account.driver_key.as_str(), "claude-code" | "claude") {
        ClaudeAccountState::parse(&account.account_state).cost_class(config)
    } else if account.driver_key == "kiro" {
        KiroAccountState::parse(&account.account_state).cost_class(config)
    } else {
        DevinAccountState::parse(&account.account_state).cost_class(config)
    }
}

pub(crate) fn account_cost_class_for_model(
    account: &OAuthCredential,
    config: &DevinPoolConfig,
    model: &str,
) -> Option<u8> {
    if matches!(account.driver_key.as_str(), "claude-code" | "claude") {
        ClaudeAccountState::parse(&account.account_state).cost_class_for_model(config, Some(model))
    } else {
        account_cost_class(account, config)
    }
}

struct Binding {
    account_id: String,
    version: i32,
    expires_at: Instant,
}
struct Cooldown {
    until: Instant,
    forget_at: Instant,
    failures: u32,
}
#[derive(Default)]
struct SchedulingState {
    cursors: HashMap<String, String>,
    bindings: HashMap<String, Binding>,
    cooldowns: HashMap<(String, i32, String), Cooldown>,
}

#[derive(Default)]
pub(crate) struct AccountPoolRegistry {
    inner: Mutex<SchedulingState>,
}

const MAX_AFFINITIES: usize = 4096;
const MAX_SCHEDULING_KEYS: usize = 8192;

impl AccountPoolRegistry {
    /// Selection and first-turn affinity are serialized. A failed admission
    /// releases its provisional binding; subsequent turns can never use a
    /// disabled/revoked/changed-plan account merely because it was sticky.
    pub fn ordered(
        &self,
        provider_id: &str,
        model: &str,
        config: &DevinPoolConfig,
        accounts: &[OAuthCredential],
        affinity: Option<&str>,
        excluded: &HashSet<String>,
    ) -> Vec<OAuthCredential> {
        let now = Instant::now();
        let wall_now = chrono::Utc::now();
        let weekly_reset_first = config.strategy == DevinPoolStrategy::WeeklyResetFirst;
        let mut state = self.inner.lock().unwrap();
        state.bindings.retain(|_, binding| binding.expires_at > now);
        state
            .cooldowns
            .retain(|_, cooldown| cooldown.forget_at > now);
        let mut eligible = accounts
            .iter()
            .filter_map(|account| {
                // A Kiro account mid-refresh is still a member: admission joins
                // the in-flight account lease instead of reporting an empty pool.
                let joinable_refresh = matches!(
                    account.driver_key.as_str(),
                    "kiro" | "claude-code" | "claude"
                ) && account.status == "refreshing";
                if account.provider_id != provider_id
                    || !account.is_enabled
                    || (account.status != "connected" && !joinable_refresh)
                    || account.access_token.trim().is_empty()
                    || excluded.contains(&account.account_id)
                    || state
                        .cooldowns
                        .get(&(
                            account.account_id.clone(),
                            account.status_version,
                            model.to_owned(),
                        ))
                        .is_some_and(|cooldown| cooldown.until > now)
                {
                    return None;
                }
                let class = account_cost_class_for_model(account, config, model)?;
                let weekly_reset = if weekly_reset_first
                    && matches!(account.driver_key.as_str(), "claude-code" | "claude")
                {
                    ClaudeAccountState::parse(&account.account_state).next_weekly_reset_at(wall_now)
                } else {
                    None
                };
                Some((class, account, weekly_reset))
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|(a_class, a, a_reset), (b_class, b, b_reset)| {
            // Preserve eligibility classes and explicit priorities. Unknown,
            // invalid or elapsed weekly deadlines sort after known future ones.
            (
                a_class,
                a.priority,
                a_reset.is_none(),
                a_reset,
                &a.created_at,
                &a.account_id,
            )
                .cmp(&(
                    b_class,
                    b.priority,
                    b_reset.is_none(),
                    b_reset,
                    &b.created_at,
                    &b.account_id,
                ))
        });
        if eligible.is_empty() {
            return Vec::new();
        }
        let best_class = eligible[0].0;
        let best_priority = eligible[0].1.priority;
        let group_end = eligible
            .iter()
            .take_while(|(class, account, _)| {
                *class == best_class && account.priority == best_priority
            })
            .count();
        let affinity = config.session_affinity.then_some(affinity).flatten();
        let sticky_index = affinity
            .and_then(|key| state.bindings.get(key))
            .and_then(|binding| {
                eligible[..group_end].iter().position(|(_, account, _)| {
                    account.account_id == binding.account_id
                        && account.status_version == binding.version
                })
            });
        let cursor_key = format!("{provider_id}:{best_class}:{best_priority}");
        let index = sticky_index.unwrap_or_else(|| {
            if config.strategy != DevinPoolStrategy::RoundRobin {
                return 0;
            }
            state
                .cursors
                .get(&cursor_key)
                .and_then(|previous| {
                    eligible[..group_end]
                        .iter()
                        .position(|(_, account, _)| &account.account_id == previous)
                })
                .map(|i| (i + 1) % group_end)
                .unwrap_or(0)
        });
        if weekly_reset_first {
            // Move a sticky choice to the front without disturbing deadline order
            // for fallback candidates (rotating the group would wrap earlier ones).
            eligible[..=index].rotate_right(1);
        } else {
            eligible[..group_end].rotate_left(index);
        }
        let chosen = &eligible[0].1.account_id;
        if sticky_index.is_none() {
            if state.cursors.len() >= MAX_SCHEDULING_KEYS {
                state.cursors.clear();
            }
            state.cursors.insert(cursor_key, chosen.clone());
        }
        if let Some(key) = affinity {
            if state.bindings.len() >= MAX_AFFINITIES && !state.bindings.contains_key(key) {
                if let Some(oldest) = state
                    .bindings
                    .iter()
                    .min_by_key(|(_, b)| b.expires_at)
                    .map(|(k, _)| k.clone())
                {
                    state.bindings.remove(&oldest);
                }
            }
            state.bindings.insert(
                key.to_owned(),
                Binding {
                    account_id: chosen.clone(),
                    version: eligible[0].1.status_version,
                    expires_at: now
                        + Duration::from_secs(config.affinity_ttl_seconds.clamp(1, 86_400)),
                },
            );
        }
        // Do not truncate before exact model health admission. Open circuits
        // consume no outbound attempt and must not hide a later healthy member.
        eligible
            .into_iter()
            .map(|(_, account, _)| account.clone())
            .collect()
    }

    pub fn is_available(&self, account_id: &str, version: i32, model: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .cooldowns
            .get(&(account_id.to_owned(), version, model.to_owned()))
            .is_none_or(|cooldown| cooldown.until <= Instant::now())
    }

    pub fn bind(&self, key: &str, account_id: &str, version: i32, ttl_seconds: u64) {
        let mut state = self.inner.lock().unwrap();
        if state
            .bindings
            .get(key)
            .is_some_and(|binding| binding.account_id == account_id && binding.version > version)
        {
            return;
        }
        if state.bindings.len() >= MAX_AFFINITIES && !state.bindings.contains_key(key) {
            if let Some(oldest) = state
                .bindings
                .iter()
                .min_by_key(|(_, b)| b.expires_at)
                .map(|(k, _)| k.clone())
            {
                state.bindings.remove(&oldest);
            }
        }
        state.bindings.insert(
            key.to_owned(),
            Binding {
                account_id: account_id.to_owned(),
                version,
                expires_at: Instant::now() + Duration::from_secs(ttl_seconds.clamp(1, 86_400)),
            },
        );
    }

    pub fn release_binding(&self, account_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .bindings
            .retain(|_, binding| binding.account_id != account_id);
    }

    pub fn release_generation(&self, account_id: &str, version: i32) {
        self.inner
            .lock()
            .unwrap()
            .bindings
            .retain(|_, binding| binding.account_id != account_id || binding.version != version);
    }

    pub fn cooldown(&self, account_id: &str, version: i32, model: &str, retry_after: Option<u64>) {
        let now = Instant::now();
        let mut state = self.inner.lock().unwrap();
        let key = (account_id.to_owned(), version, model.to_owned());
        let failures = state
            .cooldowns
            .get(&key)
            .map_or(1, |c| c.failures.saturating_add(1));
        let jitter = rand::random::<u8>() as u64 % 3;
        let delay = retry_after
            .unwrap_or(5 * (1_u64 << failures.saturating_sub(1).min(6)) + jitter)
            .clamp(1, 300);
        if state.cooldowns.len() >= MAX_SCHEDULING_KEYS {
            state.cooldowns.retain(|_, c| c.until > now);
            if state.cooldowns.len() >= MAX_SCHEDULING_KEYS {
                state.cooldowns.clear();
            }
        }
        state.cooldowns.insert(
            key,
            Cooldown {
                until: now + Duration::from_secs(delay),
                forget_at: now + Duration::from_secs(600),
                failures,
            },
        );
        state
            .bindings
            .retain(|_, binding| binding.account_id != account_id || binding.version != version);
    }
}

/// Only explicit session identity is sticky; no prompt hashing, token keys or
/// shared anonymous pseudo-session. Bound untrusted identifiers before hashing.
pub(crate) fn logical_session(request: &AiRequest) -> Option<String> {
    let raw = request.meta.raw.as_ref()?;
    let bounded = |id: &str| {
        let id = id.trim();
        (!id.is_empty() && id.len() <= 4096).then(|| id.to_owned())
    };
    [
        "session_id",
        "sessionId",
        "conversation_id",
        "previous_interaction_id",
    ]
    .iter()
    .find_map(|key| bounded(raw.body.as_ref()?.get(*key)?.as_str()?))
    .or_else(|| {
        raw.headers
            .get("x-claude-code-session-id")
            .and_then(|id| bounded(id))
    })
    .or_else(|| {
        // Native Claude Code can carry its sole explicit session identifier in
        // a JSON-encoded metadata.user_id. Own the string before dropping the
        // parsed value; never use account_uuid/device_id as conversation keys.
        let encoded = raw.body.as_ref()?.pointer("/metadata/user_id")?.as_str()?;
        if encoded.len() > 16_384 {
            return None;
        }
        let identity: serde_json::Value = serde_json::from_str(encoded).ok()?;
        bounded(identity.get("session_id")?.as_str()?)
    })
    .or_else(|| {
        // Match Claude identity's generic-header precedence.
        [
            "session-id",
            "x-session-id",
            "x-session-affinity",
            "x-conversation-id",
        ]
        .iter()
        .find_map(|key| bounded(raw.headers.get(*key)?))
    })
}

pub(crate) fn affinity_key(principal: Option<&str>, provider: &str, session: &str) -> String {
    let mut digest = Sha256::new();
    for part in [principal.unwrap_or("anonymous"), provider, session] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

/// Scope the upstream ID to the caller/provider/account without changing the
/// caller's original body. Devin's existing UUID encoder consumes this override.
pub(crate) fn scope_session(request: &mut AiRequest, key: &str, account_id: &str) {
    let scoped = affinity_key(Some(key), account_id, "devin-upstream-session");
    if let Some(raw) = &mut request.meta.raw {
        let body = raw.body.get_or_insert_with(|| serde_json::json!({}));
        if let Some(object) = body.as_object_mut() {
            object.insert("session_id".into(), serde_json::Value::String(scoped));
        }
    }
}

/// Preserve the native rule: opaque continuation alone cannot be moved between
/// account namespaces even before any bytes are delivered to the client.
pub(crate) fn can_reconstruct(request: &AiRequest) -> bool {
    let opaque = request
        .meta
        .raw
        .as_ref()
        .and_then(|raw| raw.body.as_ref())
        .is_some_and(|body| {
            ["previous_response_id", "previous_interaction_id"]
                .iter()
                .any(|key| {
                    body.get(*key)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|v| !v.is_empty())
                })
        });
    !opaque
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, daily: u8, weekly: u8) -> OAuthCredential {
        serde_json::from_value(serde_json::json!({
            "account_id":id,"provider_id":"p","label":id,"is_enabled":true,"priority":0,
            "account_state":serde_json::to_string(&DevinAccountState {
                verified_plan:Some(DevinPlan::Pro),
                usage:Some(DevinUsage {
                    plan_name:Some("Pro".into()),
                    daily:Some(super::super::devin::DevinQuota {remaining_percent:daily,resets_at_unix:None}),
                    weekly:Some(super::super::devin::DevinQuota {remaining_percent:weekly,resets_at_unix:None}),
                    overage_usd:None,
                }), ..Default::default()
            }).unwrap(),
            "driver_key":"devin","scheme":"oauth_auth_code_pkce","access_token":"fixture",
            "refresh_token":null,"expires_at":null,"resource_url":null,"subject_id":null,
            "scopes":"[]","meta":"{}","status":"connected","status_version":0,
            "last_error":null,"last_refresh_at":null,"created_at":"0","updated_at":"0"
        })).unwrap()
    }
    fn config() -> DevinPoolConfig {
        DevinPoolConfig::default()
    }
    fn kiro_account(id: &str, used: f64, limit: f64, overage: bool) -> OAuthCredential {
        let mut account = account(id, 100, 100);
        account.driver_key = "kiro".into();
        let usage = super::super::kiro::usage::parse(&serde_json::json!({
            "subscriptionInfo":{"subscriptionTitle":"KIRO PRO"},
            "overageConfiguration":{"overageEnabled":overage},
            "usageBreakdownList":[{"resourceType":"CREDITS","currentUsage":used,"usageLimit":limit}]
        }));
        account.account_state = serde_json::to_string(&KiroAccountState {
            usage: Some(usage),
            ..Default::default()
        })
        .unwrap();
        account
    }

    #[test]
    fn claude_windows_are_model_scoped_and_paid_usage_is_never_assumed() {
        let mut account = account("claude", 100, 100);
        account.driver_key = "claude-code".into();
        let usage = super::super::anthropic::claude_code::usage::parse_usage(&serde_json::json!({
            "five_hour":{"utilization":30}, "seven_day":{"utilization":20},
            "seven_day_opus":{"utilization":100},
            "extra_usage":{"is_enabled":true,"monthly_limit":10000,"used_credits":0}
        }));
        let mut state = ClaudeAccountState {
            usage: Some(usage),
            ..Default::default()
        };
        account.account_state = serde_json::to_string(&state).unwrap();
        let mut config = config();
        assert_eq!(
            account_cost_class_for_model(&account, &config, "claude-sonnet-4-6"),
            Some(0)
        );
        assert_eq!(
            account_cost_class_for_model(&account, &config, "claude-opus-4-6"),
            None
        );
        assert_eq!(account_cost_class(&account, &config), Some(0));
        let registry = AccountPoolRegistry::default();
        assert!(
            registry
                .ordered(
                    "p",
                    "claude-opus-4-6",
                    &config,
                    &[account.clone()],
                    None,
                    &HashSet::new()
                )
                .is_empty()
        );
        assert_eq!(
            registry
                .ordered(
                    "p",
                    "claude-sonnet-4-6",
                    &config,
                    &[account.clone()],
                    None,
                    &HashSet::new()
                )
                .len(),
            1
        );
        state.usage.as_mut().unwrap().windows[0].used_percent = 100.0;
        config.reserve_policy = DevinReservePolicy::Fallback;
        assert_eq!(state.cost_class(&config), None);
        state.usage = None;
        assert_eq!(state.cost_class(&config), Some(1));
        state.hard_quota = true;
        assert_eq!(state.cost_class(&config), None);
        state.hard_quota = false;
        state.auth_invalid = true;
        assert_eq!(state.cost_class(&config), None);
    }

    fn claude_account(id: &str, reset: Option<&str>) -> OAuthCredential {
        let mut account = account(id, 100, 100);
        account.driver_key = "claude-code".into();
        account.account_state = serde_json::to_string(&ClaudeAccountState {
            usage: Some(super::super::anthropic::claude_code::usage::parse_usage(
                &serde_json::json!({
                    "five_hour": {"utilization": 30},
                    "seven_day": {"utilization": 20, "resets_at": reset},
                }),
            )),
            ..Default::default()
        })
        .unwrap();
        account
    }

    fn mutate_claude(account: &mut OAuthCredential, update: impl FnOnce(&mut ClaudeAccountState)) {
        let mut state = ClaudeAccountState::parse(&account.account_state);
        update(&mut state);
        account.account_state = serde_json::to_string(&state).unwrap();
    }

    fn weekly_config() -> DevinPoolConfig {
        DevinPoolConfig {
            strategy: DevinPoolStrategy::WeeklyResetFirst,
            ..config()
        }
    }

    fn future_reset(hours: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339()
    }

    fn account_ids(accounts: &[OAuthCredential]) -> Vec<&str> {
        accounts.iter().map(|a| a.account_id.as_str()).collect()
    }

    #[test]
    fn claude_weekly_reset_first_uses_global_deadline_not_observation_or_other_windows() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![
            claude_account("later", Some("2099-01-01T02:00:00Z")),
            // Lexically later, but earlier once the timezone is normalized.
            claude_account("sooner", Some("2099-01-01T08:00:00+08:00")),
        ];
        mutate_claude(&mut accounts[0], |state| {
            state.observed_at = Some(i64::MAX);
            let usage = state.usage.as_mut().unwrap();
            usage.windows[0].resets_at = Some(future_reset(1));
            usage
                .windows
                .push(super::super::anthropic::claude_code::usage::UsageWindow {
                    name: "feature:sonnet:weekly_limit".into(),
                    used_percent: 10.0,
                    resets_at: Some(future_reset(1)),
                });
        });
        mutate_claude(&mut accounts[1], |state| state.observed_at = Some(1));
        accounts[1].driver_key = "claude".into(); // Legacy driver alias.
        for session in [None, Some("one"), Some("two")] {
            assert_eq!(
                account_ids(&registry.ordered(
                    "p",
                    "claude-sonnet",
                    &weekly_config(),
                    &accounts,
                    session,
                    &HashSet::new()
                )),
                ["sooner", "later"]
            );
        }
        // Existing strategies still use creation/ID order and round-robin cursors.
        let registry = AccountPoolRegistry::default();
        for strategy in [DevinPoolStrategy::RoundRobin, DevinPoolStrategy::FillFirst] {
            let config = DevinPoolConfig {
                strategy,
                ..config()
            };
            assert_eq!(
                registry.ordered("p", "m", &config, &accounts, None, &HashSet::new())[0].account_id,
                "later"
            );
        }
    }

    #[test]
    fn claude_weekly_reset_first_unknown_deadlines_and_ties_are_deterministic() {
        let reset = future_reset(1);
        let mut accounts = vec![
            claude_account("unknown", None),
            claude_account("invalid", Some("not-a-date")),
            claude_account("elapsed", Some(&future_reset(-1))),
            claude_account("b", Some(&reset)),
            claude_account("a", Some(&reset)),
            claude_account("older", Some(&reset)),
            claude_account("missing", None),
        ];
        accounts[5].created_at = "-1".into();
        mutate_claude(&mut accounts[6], |state| {
            state
                .usage
                .as_mut()
                .unwrap()
                .windows
                .retain(|w| w.name != "weekly_limit")
        });
        let registry = AccountPoolRegistry::default();
        let ordered =
            registry.ordered("p", "m", &weekly_config(), &accounts, None, &HashSet::new());
        assert_eq!(
            account_ids(&ordered),
            [
                "older", "a", "b", "elapsed", "invalid", "unknown", "missing"
            ]
        );
        let now = chrono::Utc::now();
        let at_reset = claude_account("boundary", Some(&now.to_rfc3339()));
        assert_eq!(
            ClaudeAccountState::parse(&at_reset.account_state).next_weekly_reset_at(now),
            None
        );
        // No global window means no deadline, even when feature windows exist.
        mutate_claude(&mut accounts[6], |state| {
            state.usage.as_mut().unwrap().windows[0].name = "feature:opus:weekly_limit".into()
        });
        assert_eq!(
            ClaudeAccountState::parse(&accounts[6].account_state).next_weekly_reset_at(now),
            None
        );
    }

    #[test]
    fn claude_weekly_reset_first_preserves_affinity_priority_and_fallback_order() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![
            claude_account("soon", Some(&future_reset(1))),
            claude_account("sticky", Some(&future_reset(2))),
            claude_account("later", Some(&future_reset(3))),
        ];
        let mut config = weekly_config();
        let empty = HashSet::new();
        registry.bind("session", "sticky", 0, 3600);
        assert_eq!(
            account_ids(&registry.ordered("p", "m", &config, &accounts, Some("session"), &empty)),
            ["sticky", "soon", "later"]
        );
        assert_eq!(
            account_ids(&registry.ordered("p", "m", &config, &accounts, Some("new"), &empty)),
            ["soon", "sticky", "later"]
        );
        config.session_affinity = false;
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("session"), &empty)[0].account_id,
            "soon"
        );
        config.session_affinity = true;
        accounts[2].priority = -1;
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("session"), &empty)[0].account_id,
            "later"
        );
        // Known complete usage still beats a higher-priority incomplete snapshot.
        mutate_claude(&mut accounts[2], |state| {
            state
                .usage
                .as_mut()
                .unwrap()
                .windows
                .retain(|w| w.name != "five_hour")
        });
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("session"), &empty)[0].account_id,
            "soon"
        );
    }

    #[test]
    fn claude_weekly_reset_first_never_bypasses_admission_blocks() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![
            claude_account("good", Some(&future_reset(2))),
            claude_account("disabled", Some(&future_reset(1))),
            claude_account("disconnected", Some(&future_reset(1))),
            claude_account("auth", Some(&future_reset(1))),
            claude_account("hard-quota", Some(&future_reset(1))),
            claude_account("weekly-full", Some(&future_reset(-1))),
            claude_account("five-hour-full", Some(&future_reset(1))),
            claude_account("model-full", Some(&future_reset(1))),
            claude_account("cooldown", Some(&future_reset(1))),
            claude_account("excluded", Some(&future_reset(1))),
        ];
        accounts[1].is_enabled = false;
        accounts[2].status = "disconnected".into();
        mutate_claude(&mut accounts[3], |state| state.auth_invalid = true);
        mutate_claude(&mut accounts[4], |state| state.hard_quota = true);
        mutate_claude(&mut accounts[5], |state| {
            state.usage.as_mut().unwrap().windows[1].used_percent = 100.0
        });
        mutate_claude(&mut accounts[6], |state| {
            state.usage.as_mut().unwrap().windows[0].used_percent = 100.0
        });
        mutate_claude(&mut accounts[7], |state| {
            state.usage.as_mut().unwrap().windows.push(
                super::super::anthropic::claude_code::usage::UsageWindow {
                    name: "feature:opus:weekly_limit".into(),
                    used_percent: 100.0,
                    resets_at: Some(future_reset(1)),
                },
            )
        });
        registry.cooldown("cooldown", 0, "claude-opus", Some(60));
        registry.bind("session", "disabled", 0, 3600);
        let excluded = HashSet::from(["excluded".into()]);
        assert_eq!(
            account_ids(&registry.ordered(
                "p",
                "claude-opus",
                &weekly_config(),
                &accounts,
                Some("session"),
                &excluded
            )),
            ["good"]
        );
        accounts[0].is_enabled = false;
        assert!(
            registry
                .ordered(
                    "p",
                    "claude-opus",
                    &weekly_config(),
                    &accounts,
                    Some("session"),
                    &excluded
                )
                .is_empty()
        );
    }

    #[test]
    fn kiro_pool_round_robin_affinity_priority_and_real_allowances() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![
            kiro_account("a", 10.0, 100.0, false),
            kiro_account("b", 100.0, 1000.0, false),
        ];
        let config = config();
        let empty = HashSet::new();
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &empty)[0].account_id,
            "a"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &empty)[0].account_id,
            "a"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, None, &empty)[0].account_id,
            "b"
        );
        accounts[1].priority = -1;
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &empty)[0].account_id,
            "b"
        );
        registry.cooldown("b", 0, "m", Some(30));
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &empty)[0].account_id,
            "a"
        );
    }

    #[test]
    fn kiro_unknown_usage_does_not_override_blocks_or_enable_paid_usage() {
        let mut config = config();
        let exhausted = kiro_account("a", 100.0, 100.0, true);
        assert_eq!(account_cost_class(&exhausted, &config), None);
        config.reserve_policy = DevinReservePolicy::Fallback;
        assert_eq!(account_cost_class(&exhausted, &config), Some(2));
        assert_eq!(
            account_cost_class(&kiro_account("b", 100.0, 100.0, false), &config),
            None
        );
        let mut state = KiroAccountState::default();
        assert_eq!(state.cost_class(&config), Some(1));
        state.hard_quota = true;
        assert_eq!(state.cost_class(&config), None);
        state.hard_quota = false;
        state.auth_invalid = true;
        assert_eq!(state.cost_class(&config), None);
    }

    #[test]
    fn round_robin_new_sessions_and_stable_affinity() {
        let registry = AccountPoolRegistry::default();
        let accounts = vec![account("a", 90, 90), account("b", 90, 90)];
        let excluded = HashSet::new();
        let config = config();
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s1"), &excluded)[0].account_id,
            "a"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s1"), &excluded)[0].account_id,
            "a"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s2"), &excluded)[0].account_id,
            "b"
        );
    }
    #[test]
    fn cross_window_exhaustion_is_not_pool_availability() {
        let registry = AccountPoolRegistry::default();
        let accounts = vec![account("a", 100, 0), account("b", 0, 100)];
        assert!(
            registry
                .ordered("p", "m", &config(), &accounts, None, &HashSet::new())
                .is_empty()
        );
    }
    #[test]
    fn cooldown_and_enabled_state_override_sticky_account() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![account("a", 90, 90), account("b", 90, 90)];
        let config = config();
        let excluded = HashSet::new();
        registry.ordered("p", "m", &config, &accounts, Some("s"), &excluded);
        registry.cooldown("a", 0, "m", Some(30));
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &excluded)[0].account_id,
            "b"
        );
        accounts[1].is_enabled = false;
        assert!(
            registry
                .ordered("p", "m", &config, &accounts, Some("s"), &excluded)
                .is_empty()
        );
    }
    #[test]
    fn included_capacity_wins_over_sticky_paid_reserve() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![account("a", 90, 90), account("b", 90, 90)];
        let mut config = config();
        config.reserve_policy = DevinReservePolicy::Fallback;
        let excluded = HashSet::new();
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &excluded)[0].account_id,
            "a"
        );
        let mut state = DevinAccountState::parse(&accounts[0].account_state);
        state
            .usage
            .as_mut()
            .unwrap()
            .daily
            .as_mut()
            .unwrap()
            .remaining_percent = 0;
        state.usage.as_mut().unwrap().overage_usd = Some(20.0);
        accounts[0].account_state = serde_json::to_string(&state).unwrap();
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("s"), &excluded)[0].account_id,
            "b"
        );
    }
    #[test]
    fn max_is_weekly_only_and_hard_quota_never_uses_old_reserve() {
        let mut config = config();
        config.reserve_policy = DevinReservePolicy::Fallback;
        let mut state = DevinAccountState::parse(&account("a", 0, 80).account_state);
        state.verified_plan = Some(DevinPlan::Max);
        state.usage.as_mut().unwrap().plan_name = Some("Max".into());
        // A legacy/proto artifact must not make a Max account daily-blocked.
        assert!(state.usage.as_ref().unwrap().daily.is_some());
        assert_eq!(state.cost_class(&config), Some(0));
        state.hard_quota = true;
        state.usage.as_mut().unwrap().overage_usd = Some(20.0);
        assert_eq!(state.cost_class(&config), None);
    }
    #[test]
    fn mixed_plan_accounts_share_round_robin_without_provider_plan_gate() {
        let registry = AccountPoolRegistry::default();
        let mut accounts = vec![account("a", 80, 80), account("b", 0, 80)];
        let mut max = DevinAccountState::parse(&accounts[1].account_state);
        max.usage.as_mut().unwrap().plan_name = Some("Max".into());
        max.plan_mismatch = true; // old Pro-only pool quarantine
        accounts[1].account_state = serde_json::to_string(&max).unwrap();
        let config: DevinPoolConfig = serde_json::from_str(r#"{"plan":"pro"}"#).unwrap();
        let excluded = HashSet::new();
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("one"), &excluded)[0].account_id,
            "a"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("two"), &excluded)[0].account_id,
            "b"
        );
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, Some("two"), &excluded)[0].account_id,
            "b"
        );
    }

    #[test]
    fn old_plan_mismatch_is_cleared_but_hard_failures_survive_normalization() {
        let mut state = DevinAccountState::parse(&account("a", 0, 80).account_state);
        state.plan_mismatch = true;
        state.usage.as_mut().unwrap().plan_name = Some("Max".into());
        state.auth_invalid = true;
        state.hard_quota = true;
        state.rejection_revision = 5;
        let parsed = DevinAccountState::parse(&serde_json::to_string(&state).unwrap());
        assert_eq!(parsed.plan(), Some(DevinPlan::Max));
        assert!(parsed.usage.as_ref().unwrap().daily.is_none());
        assert!(!parsed.plan_mismatch);
        assert!(parsed.auth_invalid && parsed.hard_quota);
        assert_eq!(parsed.rejection_revision, 5);
        assert_eq!(parsed.cost_class(&config()), None);
    }

    #[test]
    fn partial_usage_stays_unknown_and_never_overrides_known_exhaustion() {
        let mut state = DevinAccountState::parse(&account("a", 90, 90).account_state);
        state.usage.as_mut().unwrap().weekly = None;
        assert!(!state.usage_complete());
        assert_eq!(state.cost_class(&config()), Some(1));
        state
            .usage
            .as_mut()
            .unwrap()
            .daily
            .as_mut()
            .unwrap()
            .remaining_percent = 0;
        assert_eq!(state.cost_class(&config()), None);
        assert_eq!(DevinAccountState::default().cost_class(&config()), None);
    }

    #[test]
    fn priority_and_fill_first_are_stable_but_exclusions_win() {
        let registry = AccountPoolRegistry::default();
        let mut config = config();
        config.strategy = DevinPoolStrategy::FillFirst;
        let mut accounts = vec![account("a", 90, 90), account("b", 90, 90)];
        accounts[1].priority = -1;
        for _ in 0..2 {
            assert_eq!(
                registry.ordered("p", "m", &config, &accounts, None, &HashSet::new())[0].account_id,
                "b"
            );
        }
        let excluded = HashSet::from(["b".into()]);
        assert_eq!(
            registry.ordered("p", "m", &config, &accounts, None, &excluded)[0].account_id,
            "a"
        );
    }
    #[test]
    fn candidates_are_not_cut_off_before_health_admission() {
        let registry = AccountPoolRegistry::default();
        let accounts = vec![
            account("a", 90, 90),
            account("b", 90, 90),
            account("c", 90, 90),
            account("d", 90, 90),
        ];
        assert_eq!(
            registry
                .ordered("p", "m", &config(), &accounts, None, &HashSet::new())
                .len(),
            4
        );
    }
    #[test]
    fn old_generation_cooldown_never_blocks_new_credentials() {
        let registry = AccountPoolRegistry::default();
        registry.cooldown("a", 0, "m", Some(60));
        assert!(!registry.is_available("a", 0, "m"));
        assert!(registry.is_available("a", 1, "m"));
        registry.bind("session", "a", 1, 3600);
        registry.release_generation("a", 0);
        assert!(
            registry
                .inner
                .lock()
                .unwrap()
                .bindings
                .contains_key("session")
        );
    }
    #[test]
    fn cache_key_does_not_merge_explicit_session_namespaces() {
        let make = |id: &str| {
            let mut request = AiRequest::new("swe-2", vec![]);
            request.meta.raw = Some(crate::protocol::ir::RawEnvelope {
                body: Some(serde_json::json!({"prompt_cache_key":"shared-prefix"})),
                headers: HashMap::from([("x-session-id".into(), id.into())]),
                ..Default::default()
            });
            request
        };
        let mut a = make("conversation-a");
        let mut b = make("conversation-b");
        let sa = logical_session(&a).unwrap();
        let sb = logical_session(&b).unwrap();
        assert_ne!(sa, sb);
        scope_session(&mut a, &affinity_key(Some("caller"), "p", &sa), "account");
        scope_session(&mut b, &affinity_key(Some("caller"), "p", &sb), "account");
        assert_ne!(
            a.meta.raw.unwrap().body.unwrap()["session_id"],
            b.meta.raw.unwrap().body.unwrap()["session_id"]
        );
    }
    #[test]
    fn claude_native_metadata_and_affinity_headers_supply_explicit_sessions() {
        let mut request = AiRequest::new("claude-sonnet-4-6", vec![]);
        request.meta.raw = Some(crate::protocol::ir::RawEnvelope {
            body: Some(serde_json::json!({"metadata":{"user_id":serde_json::json!({
                "session_id":" native-conversation ", "account_uuid":"never-use-account", "device_id":"never-use-device"
            }).to_string()}})),
            headers: HashMap::from([("x-session-affinity".into(), "generic-affinity".into())]),
            ..Default::default()
        });
        assert_eq!(
            logical_session(&request).as_deref(),
            Some("native-conversation")
        );
        request
            .meta
            .raw
            .as_mut()
            .unwrap()
            .headers
            .insert("x-claude-code-session-id".into(), "native-header".into());
        assert_eq!(logical_session(&request).as_deref(), Some("native-header"));
        let raw = request.meta.raw.as_mut().unwrap();
        raw.headers.remove("x-claude-code-session-id");
        raw.body = Some(serde_json::json!({"metadata":{"user_id":"not-json"}}));
        assert_eq!(
            logical_session(&request).as_deref(),
            Some("generic-affinity")
        );
        request.meta.raw.as_mut().unwrap().headers.clear();
        assert!(logical_session(&request).is_none());
        request.meta.raw.as_mut().unwrap().body =
            Some(serde_json::json!({"metadata":{"user_id":serde_json::json!({
            "session_id":"x".repeat(4097), "account_uuid":"never-use-account"
        }).to_string()}}));
        assert!(logical_session(&request).is_none());
        request
            .meta
            .raw
            .as_mut()
            .unwrap()
            .headers
            .insert("x-session-affinity".into(), " ".into());
        assert!(logical_session(&request).is_none());
    }

    #[test]
    fn caller_identity_is_part_of_affinity_namespace() {
        assert_ne!(
            affinity_key(Some("a"), "p", "s"),
            affinity_key(Some("b"), "p", "s")
        );
        assert_ne!(
            affinity_key(Some("a"), "p", "s"),
            affinity_key(Some("a"), "q", "s")
        );
    }
}
