use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, Notify};

const NORMAL_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const EXHAUSTED_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
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

#[derive(Debug, Clone)]
pub struct QuotaTierObservation {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
}

struct ProviderQuotaState {
    scheduling: ProviderScheduling,
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

    pub fn observe(
        &self,
        provider_id: &str,
        tiers: &[QuotaTierObservation],
        is_available: Option<bool>,
    ) -> ProviderScheduling {
        let now = Utc::now();
        let blocking = tiers
            .iter()
            .filter(|tier| tier.used_percent >= 100.0)
            .collect::<Vec<_>>();
        let account_unavailable = is_available == Some(false);
        let exhausted = account_unavailable || !blocking.is_empty();

        let reset_at = blocking
            .iter()
            .map(|tier| tier.resets_at.as_deref().and_then(parse_reset_at))
            .collect::<Option<Vec<_>>>()
            .and_then(|resets| resets.into_iter().max())
            .map(|reset| reset.to_rfc3339());

        let delay = if !exhausted {
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
        states.insert(
            provider_id.to_string(),
            ProviderQuotaState {
                scheduling: scheduling.clone(),
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
        scheduling
    }

    pub fn record_query_failure(&self, provider_id: &str) -> ProviderScheduling {
        let mut states = self.states.write().unwrap();
        let state = states
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderQuotaState {
                scheduling: ProviderScheduling::default(),
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
    }
}
