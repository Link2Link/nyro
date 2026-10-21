//! Account-scoped admission and affinity for Devin and Kiro subscriptions.
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

pub(crate) fn supports_pool(provider: &crate::db::models::Provider) -> bool {
    provider.effective_auth_mode() == "oauth"
        && (super::devin::is_devin(provider) || super::kiro::is_kiro(provider))
}

pub(crate) fn account_cost_class(
    account: &OAuthCredential,
    config: &DevinPoolConfig,
) -> Option<u8> {
    if account.driver_key == "kiro" {
        KiroAccountState::parse(&account.account_state).cost_class(config)
    } else {
        DevinAccountState::parse(&account.account_state).cost_class(config)
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
                let joinable_refresh =
                    account.driver_key == "kiro" && account.status == "refreshing";
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
                let class = account_cost_class(account, config)?;
                Some((class, account))
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|(a_class, a), (b_class, b)| {
            (a_class, a.priority, &a.created_at, &a.account_id).cmp(&(
                b_class,
                b.priority,
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
            .take_while(|(class, account)| {
                *class == best_class && account.priority == best_priority
            })
            .count();
        let affinity = config.session_affinity.then_some(affinity).flatten();
        let sticky_index = affinity
            .and_then(|key| state.bindings.get(key))
            .and_then(|binding| {
                eligible[..group_end].iter().position(|(_, account)| {
                    account.account_id == binding.account_id
                        && account.status_version == binding.version
                })
            });
        let cursor_key = format!("{provider_id}:{best_class}:{best_priority}");
        let index = sticky_index.unwrap_or_else(|| {
            if config.strategy == DevinPoolStrategy::FillFirst {
                return 0;
            }
            state
                .cursors
                .get(&cursor_key)
                .and_then(|previous| {
                    eligible[..group_end]
                        .iter()
                        .position(|(_, account)| &account.account_id == previous)
                })
                .map(|i| (i + 1) % group_end)
                .unwrap_or(0)
        });
        eligible[..group_end].rotate_left(index);
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
            .map(|(_, account)| account.clone())
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
    let id = [
        "session_id",
        "sessionId",
        "conversation_id",
        "previous_interaction_id",
    ]
    .iter()
    .find_map(|key| raw.body.as_ref()?.get(*key)?.as_str())
    .or_else(|| {
        [
            "x-session-id",
            "session-id",
            "x-conversation-id",
            "x-claude-code-session-id",
        ]
        .iter()
        .find_map(|key| raw.headers.get(*key).map(String::as_str))
    })?;
    let id = id.trim();
    (!id.is_empty() && id.len() <= 4096).then(|| id.to_owned())
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
