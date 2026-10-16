use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, Notify};

const NORMAL_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const EXHAUSTED_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Recheck cadence while exhausted included windows are being covered by a
/// prepaid reserve. Real money is being spent and the reserve can run dry
/// within minutes under agent traffic, so the depletion edge must be caught
/// as quickly as a plain exhaustion's recovery edge.
const RESERVE_REFRESH_INTERVAL: Duration = EXHAUSTED_REFRESH_INTERVAL;
const RESET_GRACE_PERIOD: Duration = Duration::from_secs(5);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSchedulingStatus {
    Eligible,
    QuotaExhausted,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderScheduling {
    pub status: ProviderSchedulingStatus,
    pub reason: Option<String>,
    pub blocking_tiers: Vec<String>,
    pub reset_at: Option<String>,
    pub next_check_at: Option<String>,
}

impl Default for ProviderScheduling {
    fn default() -> Self {
        Self {
            status: ProviderSchedulingStatus::Eligible,
            reason: None,
            blocking_tiers: Vec::new(),
            reset_at: None,
            next_check_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaTierObservation {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
}

struct ProviderQuotaState {
    scheduling: ProviderScheduling,
    /// Last authoritative upstream quota tiers. Query failures deliberately
    /// preserve this snapshot so usage-based routing remains stable.
    tiers: Vec<QuotaTierObservation>,
    /// At least one included window is exhausted but a prepaid reserve still
    /// covers traffic, so the provider stays schedulable — as a last resort.
    on_reserve: bool,
    next_check: Instant,
    consecutive_failures: u32,
}

pub struct ProviderQuotaRegistry {
    states: RwLock<HashMap<String, ProviderQuotaState>>,
    refresh_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    refresh_notify: Notify,
}

impl Default for ProviderQuotaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderQuotaRegistry {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            refresh_locks: StdMutex::new(HashMap::new()),
            refresh_notify: Notify::new(),
        }
    }

    pub fn is_schedulable(&self, provider_id: &str) -> bool {
        self.states
            .read()
            .unwrap()
            .get(provider_id)
            .map(|state| state.scheduling.status == ProviderSchedulingStatus::Eligible)
            .unwrap_or(true)
    }

    pub fn snapshot(&self, provider_id: &str) -> ProviderScheduling {
        self.states
            .read()
            .unwrap()
            .get(provider_id)
            .map(|state| state.scheduling.clone())
            .unwrap_or_default()
    }

    /// True while exhausted included windows are being covered by a prepaid
    /// reserve (the provider is schedulable, but only as a last resort).
    pub(crate) fn is_on_reserve(&self, provider_id: &str) -> bool {
        self.states
            .read()
            .unwrap()
            .get(provider_id)
            .is_some_and(|state| state.on_reserve)
    }

    /// Last authoritative canonical quota tiers for hot-path usage routing.
    /// Query failures do not clear this snapshot; provider invalidation does.
    pub(crate) fn tier_snapshot(&self, provider_id: &str) -> Vec<QuotaTierObservation> {
        self.states
            .read()
            .unwrap()
            .get(provider_id)
            .map(|state| state.tiers.clone())
            .unwrap_or_default()
    }

    pub fn observe(
        &self,
        provider_id: &str,
        tiers: &[QuotaTierObservation],
        is_available: Option<bool>,
    ) -> ProviderScheduling {
        self.observe_with_reserve(provider_id, tiers, is_available, false)
    }

    /// [`observe`](Self::observe) for backends whose included windows are
    /// backed by a prepaid pay-as-you-go reserve (Devin's extra-usage
    /// balance): once a window is spent, traffic continues against the
    /// reserve until it runs dry.
    ///
    /// With `reserve_available`, exhausted windows no longer block scheduling
    /// — the real tiers are still retained so the readout stays honest — and
    /// the provider is flagged [`is_on_reserve`](Self::is_on_reserve) so
    /// usage-based routing ranks it last. An unavailable account still blocks:
    /// the reserve only covers exhausted windows. The caller decides when the
    /// reserve is gone by passing `false`, which restores the normal
    /// pause-until-reset behavior.
    pub fn observe_with_reserve(
        &self,
        provider_id: &str,
        tiers: &[QuotaTierObservation],
        is_available: Option<bool>,
        reserve_available: bool,
    ) -> ProviderScheduling {
        let now = Utc::now();
        let blocking = tiers
            .iter()
            .filter(|tier| tier.used_percent >= 100.0)
            .collect::<Vec<_>>();
        let account_unavailable = is_available == Some(false);
        let on_reserve = reserve_available && !account_unavailable && !blocking.is_empty();
        let exhausted = account_unavailable || (!blocking.is_empty() && !on_reserve);
        // A covered window blocks nothing: report the blocking set only when
        // it actually pauses the provider.
        let blocking = if on_reserve { Vec::new() } else { blocking };

        let reset_at = blocking
            .iter()
            .map(|tier| tier.resets_at.as_deref().and_then(parse_reset_at))
            .collect::<Option<Vec<_>>>()
            .and_then(|resets| resets.into_iter().max())
            .map(|reset| reset.to_rfc3339());

        let delay = if on_reserve {
            RESERVE_REFRESH_INTERVAL
        } else if !exhausted {
            NORMAL_REFRESH_INTERVAL
        } else if let Some(reset) = reset_at.as_deref().and_then(parse_reset_at) {
            reset
                .signed_duration_since(now)
                .to_std()
                .unwrap_or(Duration::ZERO)
                .saturating_add(RESET_GRACE_PERIOD)
                .max(RESET_GRACE_PERIOD)
        } else {
            EXHAUSTED_REFRESH_INTERVAL
        };
        let next_check_at = iso_after(now, delay);
        let scheduling = ProviderScheduling {
            status: if exhausted {
                ProviderSchedulingStatus::QuotaExhausted
            } else {
                ProviderSchedulingStatus::Eligible
            },
            reason: exhausted.then(|| {
                if account_unavailable {
                    "account_unavailable".to_string()
                } else {
                    "usage_limit".to_string()
                }
            }),
            blocking_tiers: blocking.iter().map(|tier| tier.name.clone()).collect(),
            reset_at,
            next_check_at: Some(next_check_at),
        };

        let mut states = self.states.write().unwrap();
        let previous = states.get(provider_id).map(|state| state.scheduling.status);
        let previous_reserve = states
            .get(provider_id)
            .is_some_and(|state| state.on_reserve);
        states.insert(
            provider_id.to_string(),
            ProviderQuotaState {
                scheduling: scheduling.clone(),
                tiers: tiers.to_vec(),
                on_reserve,
                next_check: Instant::now() + delay,
                consecutive_failures: 0,
            },
        );
        drop(states);

        if previous != Some(scheduling.status) {
            tracing::info!(
                provider_id,
                status = ?scheduling.status,
                reason = scheduling.reason.as_deref().unwrap_or("none"),
                "provider quota scheduling status changed"
            );
        }
        if previous_reserve != on_reserve {
            tracing::info!(
                provider_id,
                on_reserve,
                "provider prepaid reserve coverage changed"
            );
        }
        scheduling
    }

    pub fn record_query_failure(&self, provider_id: &str) -> ProviderScheduling {
        let mut states = self.states.write().unwrap();
        let state = states
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderQuotaState {
                scheduling: ProviderScheduling::default(),
                tiers: Vec::new(),
                on_reserve: false,
                next_check: Instant::now(),
                consecutive_failures: 0,
            });
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let multiplier = 1_u64 << state.consecutive_failures.saturating_sub(1).min(2);
        let delay = Duration::from_secs(60 * multiplier).min(MAX_FAILURE_BACKOFF);
        state.next_check = Instant::now() + delay;
        state.scheduling.next_check_at = Some(iso_after(Utc::now(), delay));
        state.scheduling.clone()
    }

    pub fn is_due(&self, provider_id: &str) -> bool {
        self.states
            .read()
            .unwrap()
            .get(provider_id)
            .map(|state| Instant::now() >= state.next_check)
            .unwrap_or(true)
    }

    pub fn request_refresh(&self, provider_id: &str) {
        let mut states = self.states.write().unwrap();
        let state = states
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderQuotaState {
                scheduling: ProviderScheduling::default(),
                tiers: Vec::new(),
                on_reserve: false,
                next_check: Instant::now(),
                consecutive_failures: 0,
            });
        state.next_check = Instant::now();
        state.scheduling.next_check_at = Some(Utc::now().to_rfc3339());
        drop(states);
        self.refresh_notify.notify_one();
    }

    pub fn request_refresh_all(&self) {
        let now = Instant::now();
        let next_check_at = Utc::now().to_rfc3339();
        for state in self.states.write().unwrap().values_mut() {
            state.next_check = now;
            state.scheduling.next_check_at = Some(next_check_at.clone());
        }
        self.refresh_notify.notify_one();
    }

    pub fn invalidate(&self, provider_id: &str) {
        self.states.write().unwrap().remove(provider_id);
        self.request_refresh(provider_id);
    }

    pub fn remove(&self, provider_id: &str) {
        self.states.write().unwrap().remove(provider_id);
        self.refresh_locks.lock().unwrap().remove(provider_id);
    }

    pub fn refresh_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.refresh_locks
            .lock()
            .unwrap()
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn wait_for_refresh_request(&self) {
        self.refresh_notify.notified().await;
    }
}

fn parse_reset_at(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn iso_after(now: DateTime<Utc>, delay: Duration) -> String {
    now.checked_add_signed(chrono::Duration::from_std(delay).unwrap_or_default())
        .unwrap_or(now)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(name: &str, used_percent: f64, resets_at: Option<&str>) -> QuotaTierObservation {
        QuotaTierObservation {
            name: name.to_string(),
            used_percent,
            resets_at: resets_at.map(str::to_string),
        }
    }

    #[test]
    fn usage_below_limit_remains_schedulable() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe("provider", &[tier("five_hour", 99.99, None)], None);

        assert_eq!(scheduling.status, ProviderSchedulingStatus::Eligible);
        assert!(registry.is_schedulable("provider"));
    }

    #[test]
    fn any_full_window_blocks_the_whole_provider() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe(
            "provider",
            &[
                tier("five_hour", 100.0, Some("2030-01-01T01:00:00Z")),
                tier("weekly_limit", 42.0, Some("2030-01-07T00:00:00Z")),
            ],
            None,
        );

        assert_eq!(scheduling.status, ProviderSchedulingStatus::QuotaExhausted);
        assert_eq!(scheduling.blocking_tiers, vec!["five_hour"]);
        assert!(!registry.is_schedulable("provider"));
    }

    #[test]
    fn explicit_account_unavailability_blocks_without_tiers() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe("provider", &[], Some(false));

        assert_eq!(scheduling.reason.as_deref(), Some("account_unavailable"));
        assert!(!registry.is_schedulable("provider"));
    }

    #[test]
    fn latest_blocking_reset_is_reported() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe(
            "provider",
            &[
                tier("five_hour", 100.0, Some("2030-01-01T01:00:00Z")),
                tier("weekly_limit", 101.0, Some("2030-01-07T00:00:00Z")),
            ],
            None,
        );

        assert_eq!(
            scheduling.reset_at.as_deref(),
            Some("2030-01-07T00:00:00+00:00")
        );
    }

    #[test]
    fn missing_blocking_reset_uses_periodic_recheck() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe(
            "provider",
            &[
                tier("five_hour", 100.0, Some("2030-01-01T01:00:00Z")),
                tier("weekly_limit", 100.0, None),
            ],
            None,
        );

        assert_eq!(scheduling.reset_at, None);
        assert!(scheduling.next_check_at.is_some());
    }

    #[test]
    fn invalid_blocking_reset_uses_periodic_recheck() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe(
            "provider",
            &[
                tier("five_hour", 100.0, Some("not-a-timestamp")),
                tier("weekly_limit", 100.0, Some("2030-01-07T00:00:00Z")),
            ],
            None,
        );

        assert_eq!(scheduling.reset_at, None);
        assert!(scheduling.next_check_at.is_some());
    }

    #[test]
    fn failed_refresh_preserves_exhausted_state_until_confirmed_recovery() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("provider", &[tier("five_hour", 100.0, None)], None);
        registry.record_query_failure("provider");

        assert!(!registry.is_schedulable("provider"));
        registry.observe("provider", &[tier("five_hour", 12.0, None)], None);
        assert!(registry.is_schedulable("provider"));
    }

    #[test]
    fn last_authoritative_tiers_survive_failures_and_refresh_requests() {
        let registry = ProviderQuotaRegistry::new();
        let first = vec![tier("weekly_limit", 35.0, Some("2030-01-07T00:00:00Z"))];
        registry.observe("provider", &first, None);

        registry.record_query_failure("provider");
        registry.request_refresh("provider");
        registry.request_refresh_all();

        assert_eq!(registry.tier_snapshot("provider"), first);

        let second = vec![tier("weekly_limit", 52.0, None)];
        registry.observe("provider", &second, None);
        assert_eq!(registry.tier_snapshot("provider"), second);
    }

    #[test]
    fn authoritative_unavailability_clears_tiers_and_recovery_replaces_them() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("provider", &[tier("weekly_limit", 35.0, None)], Some(true));

        registry.observe("provider", &[], Some(false));
        assert!(!registry.is_schedulable("provider"));
        assert!(registry.tier_snapshot("provider").is_empty());

        let recovered = vec![tier("weekly_limit", 5.0, None)];
        registry.observe("provider", &recovered, Some(true));
        assert!(registry.is_schedulable("provider"));
        assert_eq!(registry.tier_snapshot("provider"), recovered);
    }

    #[test]
    fn refresh_all_preserves_blocking_decisions() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("provider", &[tier("five_hour", 100.0, None)], None);

        registry.request_refresh_all();

        assert!(!registry.is_schedulable("provider"));
        assert!(registry.is_due("provider"));
    }

    #[test]
    fn invalidation_fails_open_and_requests_a_refresh() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("provider", &[tier("five_hour", 100.0, None)], None);
        registry.invalidate("provider");

        assert!(registry.is_schedulable("provider"));
        assert!(registry.is_due("provider"));
        assert!(registry.tier_snapshot("provider").is_empty());
    }

    #[test]
    fn remove_clears_retained_tiers() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("provider", &[tier("monthly", 10.0, None)], None);

        registry.remove("provider");

        assert!(registry.tier_snapshot("provider").is_empty());
    }

    fn exhausted_daily_with_healthy_weekly() -> Vec<QuotaTierObservation> {
        vec![
            tier("daily", 100.0, Some("2030-01-01T08:00:00Z")),
            tier("weekly_limit", 9.0, Some("2030-01-05T08:00:00Z")),
        ]
    }

    #[test]
    fn reserve_keeps_an_exhausted_window_schedulable_and_flags_it() {
        let registry = ProviderQuotaRegistry::new();
        let tiers = exhausted_daily_with_healthy_weekly();
        let scheduling = registry.observe_with_reserve("provider", &tiers, Some(true), true);

        assert_eq!(scheduling.status, ProviderSchedulingStatus::Eligible);
        assert_eq!(scheduling.reason, None);
        assert!(scheduling.blocking_tiers.is_empty());
        assert_eq!(scheduling.reset_at, None);
        assert!(registry.is_schedulable("provider"));
        assert!(registry.is_on_reserve("provider"));
        // The honest readout survives: the exhausted window is still recorded.
        assert_eq!(registry.tier_snapshot("provider"), tiers);
    }

    #[test]
    fn reserve_is_rechecked_at_the_fast_cadence() {
        let registry = ProviderQuotaRegistry::new();
        let before = Utc::now();
        let scheduling = registry.observe_with_reserve(
            "provider",
            &exhausted_daily_with_healthy_weekly(),
            Some(true),
            true,
        );
        let next = parse_reset_at(scheduling.next_check_at.as_deref().unwrap()).unwrap();
        let wait = next.signed_duration_since(before).num_seconds();
        let expected = RESERVE_REFRESH_INTERVAL.as_secs() as i64;

        assert!(
            (expected - 2..=expected + 2).contains(&wait),
            "reserve recheck must use the fast cadence, got {wait}s"
        );
    }

    #[test]
    fn exhausted_reserve_restores_the_normal_pause() {
        let registry = ProviderQuotaRegistry::new();
        let tiers = exhausted_daily_with_healthy_weekly();
        registry.observe_with_reserve("provider", &tiers, Some(true), true);
        assert!(registry.is_schedulable("provider"));

        let scheduling = registry.observe_with_reserve("provider", &tiers, Some(true), false);

        assert_eq!(scheduling.status, ProviderSchedulingStatus::QuotaExhausted);
        assert_eq!(scheduling.reason.as_deref(), Some("usage_limit"));
        assert_eq!(scheduling.blocking_tiers, vec!["daily"]);
        assert_eq!(
            scheduling.reset_at.as_deref(),
            Some("2030-01-01T08:00:00+00:00")
        );
        assert!(!registry.is_schedulable("provider"));
        assert!(!registry.is_on_reserve("provider"));
    }

    #[test]
    fn reserve_never_overrides_an_unavailable_account() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe_with_reserve(
            "provider",
            &exhausted_daily_with_healthy_weekly(),
            Some(false),
            true,
        );

        assert_eq!(scheduling.reason.as_deref(), Some("account_unavailable"));
        assert!(!registry.is_schedulable("provider"));
        assert!(!registry.is_on_reserve("provider"));
    }

    #[test]
    fn reserve_without_an_exhausted_window_is_not_reserve_traffic() {
        let registry = ProviderQuotaRegistry::new();
        // A positive reserve balance alone must not demote a healthy provider.
        let scheduling = registry.observe_with_reserve(
            "provider",
            &[tier("daily", 17.0, None), tier("weekly_limit", 9.0, None)],
            Some(true),
            true,
        );

        assert_eq!(scheduling.status, ProviderSchedulingStatus::Eligible);
        assert!(!registry.is_on_reserve("provider"));
    }

    #[test]
    fn plain_observe_never_enters_reserve_and_keeps_its_blocking_behavior() {
        let registry = ProviderQuotaRegistry::new();
        let scheduling = registry.observe("provider", &exhausted_daily_with_healthy_weekly(), None);

        assert_eq!(scheduling.status, ProviderSchedulingStatus::QuotaExhausted);
        assert!(!registry.is_on_reserve("provider"));
    }

    #[test]
    fn reserve_state_survives_failures_and_clears_on_invalidation() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_with_reserve(
            "provider",
            &exhausted_daily_with_healthy_weekly(),
            Some(true),
            true,
        );

        registry.record_query_failure("provider");
        registry.request_refresh("provider");
        assert!(registry.is_on_reserve("provider"));
        assert!(registry.is_schedulable("provider"));

        registry.invalidate("provider");
        assert!(!registry.is_on_reserve("provider"));
        assert!(registry.is_schedulable("provider"));
    }
}
