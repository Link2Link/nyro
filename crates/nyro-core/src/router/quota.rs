use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, Notify};

use crate::quota_buckets::QuotaBucket;

/// Captured before network I/O; invalidation/removal fences late responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketRefreshFence(u64);

#[derive(Default)]
struct ScopedBuckets {
    generation: u64,
    scope_id: Option<String>,
    buckets: Vec<QuotaBucket>,
    unavailable: bool,
    /// Generic 429 is a temporary exact-model throttle, never quota recovery.
    cooldowns: HashMap<String, Instant>,
}

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
    scoped: RwLock<HashMap<String, ScopedBuckets>>,
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
            scoped: RwLock::new(HashMap::new()),
            refresh_locks: StdMutex::new(HashMap::new()),
            refresh_notify: Notify::new(),
        }
    }

    /// Capture before starting discovery/summary I/O. An invalidation or owner
    /// replacement makes publication with this fence a no-op.
    pub fn bucket_refresh_fence(&self, provider_id: &str) -> BucketRefreshFence {
        let mut scoped = self.scoped.write().unwrap();
        BucketRefreshFence(scoped.entry(provider_id.to_owned()).or_default().generation)
    }

    /// Revalidate an attempt's captured owner/mapping generation immediately
    /// before dispatch. A cold provider still has a generation tombstone.
    pub fn bucket_fence_is_current(&self, provider_id: &str, fence: &BucketRefreshFence) -> bool {
        self.scoped
            .read()
            .unwrap()
            .get(provider_id)
            .is_some_and(|state| state.generation == fence.0)
    }

    pub fn observe_buckets_fenced(
        &self,
        provider_id: &str,
        buckets: &[QuotaBucket],
        is_available: Option<bool>,
        fence: BucketRefreshFence,
    ) -> (Vec<QuotaBucket>, ProviderScheduling) {
        self.observe_buckets_inner(provider_id, buckets, is_available, Some(fence), false)
    }

    pub fn observe_buckets(
        &self,
        provider_id: &str,
        buckets: &[QuotaBucket],
        is_available: Option<bool>,
    ) -> (Vec<QuotaBucket>, ProviderScheduling) {
        self.observe_buckets_inner(provider_id, buckets, is_available, None, false)
    }

    fn observe_buckets_inner(
        &self,
        provider_id: &str,
        buckets: &[QuotaBucket],
        is_available: Option<bool>,
        fence: Option<BucketRefreshFence>,
        replace_mapping: bool,
    ) -> (Vec<QuotaBucket>, ProviderScheduling) {
        let mut scoped = self.scoped.write().unwrap();
        let state = scoped.entry(provider_id.to_owned()).or_default();
        if replace_mapping {
            state
                .buckets
                .retain(|bucket| bucket.binding.source != "configured");
            let refs: Vec<String> = state
                .buckets
                .iter()
                .filter(|bucket| bucket.is_resolved())
                .flat_map(|bucket| bucket.constraint_ids.iter().cloned())
                .collect();
            for bucket in &mut state.buckets {
                if bucket.binding.status == "window_constraint" && !refs.contains(&bucket.id) {
                    bucket.binding.status = "unresolved".to_owned();
                }
            }
            state.generation = state.generation.wrapping_add(1);
        }
        if fence.is_some_and(|fence| fence.0 != state.generation) {
            return (effective_buckets(state), self.snapshot(provider_id));
        }
        if let Some(scope) = buckets.first().map(|bucket| &bucket.scope_id) {
            // Mixed owners are not a meaningful observation of this provider.
            if buckets.iter().any(|bucket| &bucket.scope_id != scope) {
                return (effective_buckets(state), self.snapshot(provider_id));
            }
            if state.scope_id.as_ref() != Some(scope) {
                if state.scope_id.is_some() {
                    state.buckets.clear();
                    state.cooldowns.clear();
                    state.unavailable = false;
                }
                // First discovery also establishes an owner where requests may
                // have captured an earlier unknown generation.
                state.generation = state.generation.wrapping_add(1);
            }
            state.scope_id = Some(scope.clone());
        }
        if let Some(available) = is_available {
            state.unavailable = !available;
        }
        if buckets.iter().any(|bucket| bucket.source != "unknown") {
            state.buckets.retain(|bucket| bucket.source != "unknown");
        }
        // Absence/partial failure is not deletion. Preserve independently timed
        // last-good constraints and mark them stale before merging successes.
        for bucket in &mut state.buckets {
            normalize_google_window_aliases(bucket);
            bucket.stale = true;
        }
        for incoming in buckets {
            let mut incoming = incoming.clone();
            normalize_google_window_aliases(&mut incoming);
            if incoming.source == "enforced_summary"
                && (parse_reset_at(&incoming.queried_at).is_none()
                    || incoming.tiers.iter().any(|tier| {
                        !tier.used_percent.is_finite()
                            || !(0.0..=100.0).contains(&tier.used_percent)
                    }))
            {
                incoming.stale = true;
            }
            incoming.models.sort();
            incoming.models.dedup();
            // A catalog may discover models but cannot replace an enforced
            // primary or clear its exhausted fence through a new resource ID.
            if incoming.source == "catalog" {
                incoming.models.retain(|model| {
                    !state
                        .buckets
                        .iter()
                        .any(|old| old.source != "catalog" && old.binds_model(model))
                        && !buckets
                            .iter()
                            .any(|new| new.source != "catalog" && new.binds_model(model))
                });
                if incoming.models.is_empty() && incoming.binding.status == "resolved" {
                    continue;
                }
            } else if incoming.is_resolved() {
                for old in &mut state.buckets {
                    if old.source == "catalog" {
                        old.models.retain(|model| !incoming.models.contains(model));
                    }
                }
            }
            if let Some(old) = state.buckets.iter_mut().find(|old| old.id == incoming.id) {
                let older = match (
                    parse_reset_at(&incoming.queried_at),
                    parse_reset_at(&old.queried_at),
                ) {
                    (Some(new), Some(old)) => new < old,
                    _ => false,
                };
                if older || source_rank(&incoming.source) < source_rank(&old.source) {
                    continue;
                }
                // Discovery can succeed partially. Omitting a known member of
                // this SAME owner/resource is not authoritative retirement.
                // Explicit configuration replacement has its own atomic path.
                if incoming.binding.source != "configured" && old.binding.source != "configured" {
                    for model in &old.models {
                        if !incoming.models.contains(model) {
                            incoming.models.push(model.clone());
                            incoming.stale = true;
                        }
                    }
                    incoming.models.sort();
                }
                if old.binding.status == "window_constraint"
                    && incoming.binding.status == "unresolved"
                {
                    incoming.binding = old.binding.clone();
                    incoming.models = old.models.clone();
                }
                // A missing/invalid tier does not prove recovery. Fresh values
                // for the SAME constraint/window may clear that window only.
                let mut tiers = old.tiers.clone();
                if old
                    .tiers
                    .iter()
                    .any(|old| !incoming.tiers.iter().any(|new| new.name == old.name))
                {
                    incoming.stale = true;
                }
                // A partial refresh cannot detach an older applicable block.
                for id in &old.constraint_ids {
                    if !incoming.constraint_ids.contains(id) {
                        incoming.constraint_ids.push(id.clone());
                    }
                }
                for tier in incoming
                    .tiers
                    .iter()
                    .filter(|tier| tier.used_percent.is_finite())
                {
                    if let Some(old) = tiers.iter_mut().find(|old| old.name == tier.name) {
                        if (incoming.stale || incoming.binding.source == "conflicting_official_v1")
                            && old.used_percent >= 100.0
                            && tier.used_percent < 100.0
                        {
                            continue;
                        }
                        *old = tier.clone();
                    } else {
                        tiers.push(tier.clone());
                    }
                }
                incoming.tiers = tiers;
                *old = incoming;
            } else {
                state.buckets.push(incoming);
            }
        }
        state.buckets.retain(|bucket| {
            bucket.source != "catalog" || !bucket.models.is_empty() || !bucket.is_resolved()
        });
        // Conflicting primary bindings are visible as conflicts, not selected
        // by vector order. A conflict with exhaustion must still fail closed.
        let mut counts = HashMap::<String, usize>::new();
        for bucket in state.buckets.iter().filter(|bucket| {
            bucket.is_primary() && matches!(bucket.binding.status.as_str(), "resolved" | "conflict")
        }) {
            for model in &bucket.models {
                *counts.entry(model.clone()).or_default() += 1;
            }
        }
        for bucket in &mut state.buckets {
            if bucket.is_primary()
                && matches!(bucket.binding.status.as_str(), "resolved" | "conflict")
                && bucket
                    .models
                    .iter()
                    .any(|model| counts.get(model).copied().unwrap_or(0) > 1)
            {
                bucket.binding.status = "conflict".to_owned();
            } else if bucket.binding.status == "conflict"
                && !bucket.models.is_empty()
                && bucket.binding.source != "conflicting_official_v1"
            {
                bucket.binding.status = "resolved".to_owned();
            }
        }
        let effective = effective_buckets(state);
        // Generic throttles are not provider-wide quota exhaustion, even if
        // every currently mapped model happens to be cooling down.
        let quota_only: Vec<_> = effective
            .iter()
            .cloned()
            .map(|mut bucket| {
                if bucket.scheduling.reason.as_deref() == Some("model_cooldown") {
                    bucket.scheduling = schedule_bucket(&bucket, state.unavailable);
                }
                bucket
            })
            .collect();
        let summary = bucket_summary(&quota_only, state.unavailable);
        let fast_recheck = effective.iter().any(|bucket| {
            bucket.stale || bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
        });
        let delay = if fast_recheck {
            EXHAUSTED_REFRESH_INTERVAL
        } else {
            NORMAL_REFRESH_INTERVAL
        };
        let mut summary = summary;
        summary.next_check_at = Some(iso_after(Utc::now(), delay));
        self.states.write().unwrap().insert(
            provider_id.to_owned(),
            ProviderQuotaState {
                scheduling: summary.clone(),
                // Alternatives have no additive/provider-wide percentage.
                tiers: Vec::new(),
                on_reserve: false,
                next_check: Instant::now() + delay,
                consecutive_failures: 0,
            },
        );
        (effective, summary)
    }

    pub fn persistent_bucket_fences(
        &self,
        provider_id: &str,
    ) -> Vec<crate::quota_buckets::PersistentQuotaBucketFence> {
        self.bucket_snapshot(provider_id)
            .into_iter()
            .filter(|bucket| bucket.source == "enforced_summary")
            .map(|bucket| crate::quota_buckets::PersistentQuotaBucketFence {
                id: bucket.id,
                scope_id: bucket.scope_id,
                label: bucket.label,
                models: bucket.models,
                binding: bucket.binding,
                tiers: bucket
                    .tiers
                    .into_iter()
                    .map(|tier| crate::quota_buckets::PersistentQuotaTier {
                        name: tier.name,
                        used_percent: tier.used_percent,
                        resets_at: tier.resets_at,
                    })
                    .collect(),
                constraint_ids: bucket.constraint_ids,
                source: bucket.source,
                queried_at: bucket.queried_at,
            })
            .collect()
    }

    /// Startup-only restore. Expired reset timestamps do not erase a known
    /// exhaustion; the first same-owner authoritative refresh proves recovery.
    pub fn restore_bucket_fences(
        &self,
        provider_id: &str,
        fences: &[crate::quota_buckets::PersistentQuotaBucketFence],
    ) {
        let buckets: Vec<_> = fences
            .iter()
            .filter(|fence| {
                fence.source == "enforced_summary"
                    && !fence.id.is_empty()
                    && !fence.scope_id.is_empty()
                    && fence.tiers.iter().all(|tier| tier.used_percent.is_finite())
            })
            .map(|fence| QuotaBucket {
                id: fence.id.clone(),
                scope_id: fence.scope_id.clone(),
                label: fence.label.clone(),
                models: fence.models.clone(),
                binding: fence.binding.clone(),
                tiers: fence
                    .tiers
                    .iter()
                    .map(|tier| crate::admin::ProviderUsageTier {
                        name: tier.name.clone(),
                        used_percent: tier.used_percent,
                        resets_at: tier.resets_at.clone(),
                    })
                    .collect(),
                constraint_ids: fence.constraint_ids.clone(),
                source: fence.source.clone(),
                queried_at: fence.queried_at.clone(),
                stale: true,
                scheduling: ProviderScheduling::default(),
                api_price: None,
            })
            .collect();
        self.observe_buckets(provider_id, &buckets, None);
        self.request_refresh(provider_id);
    }

    /// Discovered resources suitable for validating a replacement operator
    /// mapping, without feeding old configured aggregates back into itself.
    pub fn raw_bucket_snapshot(&self, provider_id: &str) -> Vec<QuotaBucket> {
        let mut buckets = self.bucket_snapshot(provider_id);
        buckets.retain(|bucket| bucket.binding.source != "configured");
        let official_refs: Vec<String> = buckets
            .iter()
            .filter(|bucket| bucket.is_resolved())
            .flat_map(|bucket| bucket.constraint_ids.iter().cloned())
            .collect();
        for bucket in &mut buckets {
            if bucket.binding.status == "window_constraint" && !official_refs.contains(&bucket.id) {
                bucket.binding.status = "unresolved".to_owned();
            }
        }
        buckets
    }

    /// Replace mapping only, not owner evidence or temporary rejection fences.
    pub fn replace_bucket_mapping(
        &self,
        provider_id: &str,
        configured: &[QuotaBucket],
    ) -> (Vec<QuotaBucket>, ProviderScheduling) {
        self.observe_buckets_inner(provider_id, configured, None, None, true)
    }

    pub fn bucket_snapshot(&self, provider_id: &str) -> Vec<QuotaBucket> {
        self.scoped
            .read()
            .unwrap()
            .get(provider_id)
            .map(effective_buckets)
            .unwrap_or_default()
    }

    /// Unique exact primary binding only. Shared constraints are folded into
    /// effective scheduling, never appended as duplicate display windows.
    pub fn model_snapshot(&self, provider_id: &str, model: &str) -> Option<QuotaBucket> {
        let buckets = self.bucket_snapshot(provider_id);
        let mut matches = buckets
            .into_iter()
            .filter(|bucket| bucket.binds_model(model));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn is_model_schedulable(&self, provider_id: &str, model: &str) -> bool {
        let scoped = self.scoped.read().unwrap();
        let Some(state) = scoped.get(provider_id) else {
            return self.is_schedulable(provider_id);
        };
        if state.unavailable
            || state
                .cooldowns
                .get(model)
                .is_some_and(|until| *until > Instant::now())
        {
            return false;
        }
        if state.buckets.is_empty() {
            return self.is_schedulable(provider_id);
        }
        let buckets = effective_buckets(state);
        if buckets.iter().any(|bucket| {
            bucket.is_global_constraint()
                && bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
        }) {
            return false;
        }
        let matches: Vec<_> = buckets
            .iter()
            .filter(|bucket| bucket.is_primary() && bucket.models.iter().any(|id| id == model))
            .collect();
        // Unknown exact bindings remain controlled probes; a known blocked
        // conflicting binding must not be bypassed as "unknown".
        !matches
            .iter()
            .any(|bucket| bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted)
    }

    pub fn model_scheduling(&self, provider_id: &str, model: &str) -> ProviderScheduling {
        let scoped = self.scoped.read().unwrap();
        let Some(state) = scoped.get(provider_id) else {
            return self.snapshot(provider_id);
        };
        if state.unavailable {
            return ProviderScheduling {
                status: ProviderSchedulingStatus::QuotaExhausted,
                reason: Some("account_unavailable".to_owned()),
                ..Default::default()
            };
        }
        if let Some(until) = state
            .cooldowns
            .get(model)
            .filter(|until| **until > Instant::now())
        {
            return ProviderScheduling {
                status: ProviderSchedulingStatus::QuotaExhausted,
                reason: Some("model_cooldown".to_owned()),
                next_check_at: Some(iso_after(
                    Utc::now(),
                    until.saturating_duration_since(Instant::now()),
                )),
                ..Default::default()
            };
        }
        if state.buckets.is_empty() {
            return self.snapshot(provider_id);
        }
        let effective = effective_buckets(state);
        if let Some(global) = effective.iter().find(|bucket| {
            bucket.is_global_constraint()
                && bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
        }) {
            let mut scheduling = global.scheduling.clone();
            scheduling.reason = Some("shared_constraint".to_owned());
            return scheduling;
        }
        if let Some(blocked) = effective.iter().find(|bucket| {
            bucket.is_primary()
                && bucket.models.iter().any(|id| id == model)
                && bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted
        }) {
            return blocked.scheduling.clone();
        }
        let matches: Vec<_> = effective
            .iter()
            .filter(|bucket| bucket.binds_model(model))
            .collect();
        if matches.len() == 1 {
            return matches[0].scheduling.clone();
        }
        ProviderScheduling {
            reason: Some(
                if effective
                    .iter()
                    .any(|bucket| bucket.models.iter().any(|id| id == model))
                {
                    "conflicting_binding"
                } else {
                    "unknown_mapping"
                }
                .to_owned(),
            ),
            ..Default::default()
        }
    }

    pub fn model_tiers(&self, provider_id: &str, model: &str) -> Vec<QuotaTierObservation> {
        if let Some(bucket) = self.model_snapshot(provider_id, model) {
            let mut tiers: Vec<_> = bucket
                .tiers
                .iter()
                .map(|tier| QuotaTierObservation {
                    name: tier.name.clone(),
                    used_percent: tier.used_percent,
                    resets_at: tier.resets_at.clone(),
                })
                .collect();
            // Shared scoring windows cap, not add capacity. Keep the tightest
            // same-name constraint only in this internal scoring projection.
            for shared in self
                .bucket_snapshot(provider_id)
                .iter()
                .filter(|shared| bucket.constraint_ids.contains(&shared.id))
            {
                for tier in &shared.tiers {
                    if let Some(old) = tiers.iter_mut().find(|old| old.name == tier.name) {
                        if tier.used_percent > old.used_percent {
                            old.used_percent = tier.used_percent;
                            old.resets_at = tier.resets_at.clone();
                        }
                    } else {
                        tiers.push(QuotaTierObservation {
                            name: tier.name.clone(),
                            used_percent: tier.used_percent,
                            resets_at: tier.resets_at.clone(),
                        });
                    }
                }
            }
            return tiers;
        }
        self.tier_snapshot(provider_id)
    }

    /// Trusted adapter-only feedback for an already-known opaque enforced
    /// resource. No vendor error text guessing is performed here. Persist via
    /// persistent_bucket_fences after a successful mutation. Bumping generation
    /// prevents any refresh started before this rejection from clearing it.
    pub fn record_constraint_rejection_fenced(
        &self,
        provider_id: &str,
        constraint_id: &str,
        fence: &BucketRefreshFence,
    ) -> Option<ProviderScheduling> {
        let mut scoped = self.scoped.write().unwrap();
        let state = scoped.get_mut(provider_id)?;
        if state.generation != fence.0 {
            return None;
        }
        let constraint = state.buckets.iter_mut().find(|bucket| {
            bucket.id == constraint_id
                && bucket.source == "enforced_summary"
                && matches!(
                    bucket.binding.status.as_str(),
                    "window_constraint" | "shared_constraint"
                )
                && !bucket.tiers.is_empty()
        })?;
        for tier in &mut constraint.tiers {
            tier.used_percent = 100.0;
        }
        constraint.queried_at = Utc::now().to_rfc3339();
        constraint.stale = false;
        state.generation = state.generation.wrapping_add(1);
        let effective = effective_buckets(state);
        let mut summary = bucket_summary(&effective, state.unavailable);
        summary.next_check_at = Some(Utc::now().to_rfc3339());
        self.states.write().unwrap().insert(
            provider_id.to_owned(),
            ProviderQuotaState {
                scheduling: summary.clone(),
                tiers: Vec::new(),
                on_reserve: false,
                next_check: Instant::now(),
                consecutive_failures: 0,
            },
        );
        drop(scoped);
        self.refresh_notify.notify_one();
        Some(summary)
    }

    /// Unknown 429s are narrow, bounded throttles. Refreshes cannot clear an
    /// unexpired cooldown, and a cooldown never exhausts sibling buckets.
    pub fn record_model_rejection(
        &self,
        provider_id: &str,
        model: &str,
        retry_after_secs: Option<u64>,
    ) -> ProviderScheduling {
        self.record_model_rejection_inner(provider_id, model, retry_after_secs, None)
            .expect("unfenced rejection is always current")
    }

    /// Late responses from a replaced owner/mapping must not cool down its
    /// successor. Validation and mutation share one lock; None means discarded.
    pub fn record_model_rejection_fenced(
        &self,
        provider_id: &str,
        model: &str,
        retry_after_secs: Option<u64>,
        fence: &BucketRefreshFence,
    ) -> Option<ProviderScheduling> {
        self.record_model_rejection_inner(provider_id, model, retry_after_secs, Some(fence))
    }

    fn record_model_rejection_inner(
        &self,
        provider_id: &str,
        model: &str,
        retry_after_secs: Option<u64>,
        fence: Option<&BucketRefreshFence>,
    ) -> Option<ProviderScheduling> {
        let delay = Duration::from_secs(retry_after_secs.unwrap_or(30).clamp(1, 300));
        let mut scoped = self.scoped.write().unwrap();
        let state = scoped.entry(provider_id.to_owned()).or_default();
        if fence.is_some_and(|fence| fence.0 != state.generation) {
            return None;
        }
        let until = Instant::now() + delay;
        state
            .cooldowns
            .entry(model.to_owned())
            .and_modify(|old| *old = (*old).max(until))
            .or_insert(until);
        drop(scoped);
        self.request_refresh(provider_id);
        Some(ProviderScheduling {
            status: ProviderSchedulingStatus::QuotaExhausted,
            reason: Some("model_cooldown".to_owned()),
            blocking_tiers: Vec::new(),
            reset_at: None,
            next_check_at: Some(iso_after(Utc::now(), delay)),
        })
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

    /// Publish account-pool readiness without interpreting aggregate display
    /// percentages as one subscriber's quota. `tiers` belong to one eligible
    /// representative account and are used only for the outer usage score.
    pub(crate) fn publish_pool(
        &self,
        provider_id: &str,
        schedulable: bool,
        on_reserve: bool,
        tiers: &[QuotaTierObservation],
        reason: Option<&str>,
    ) -> ProviderScheduling {
        let now = Utc::now();
        let scheduling = ProviderScheduling {
            status: if schedulable {
                ProviderSchedulingStatus::Eligible
            } else {
                ProviderSchedulingStatus::QuotaExhausted
            },
            reason: (!schedulable).then(|| reason.unwrap_or("no_available_account").to_owned()),
            blocking_tiers: Vec::new(),
            reset_at: None,
            next_check_at: Some(iso_after(now, EXHAUSTED_REFRESH_INTERVAL)),
        };
        let mut states = self.states.write().unwrap();
        let next_check = states
            .get(provider_id)
            .map(|state| state.next_check)
            .unwrap_or_else(Instant::now);
        states.insert(
            provider_id.to_owned(),
            ProviderQuotaState {
                scheduling: scheduling.clone(),
                tiers: tiers.to_vec(),
                on_reserve: schedulable && on_reserve,
                next_check,
                consecutive_failures: 0,
            },
        );
        scheduling
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
                .min(NORMAL_REFRESH_INTERVAL)
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
        if let Some(scoped) = self.scoped.write().unwrap().get_mut(provider_id) {
            for bucket in &mut scoped.buckets {
                bucket.stale = true;
            }
        }
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
        self.reset_bucket_scope(provider_id);
        self.states.write().unwrap().remove(provider_id);
        self.request_refresh(provider_id);
    }

    fn reset_bucket_scope(&self, provider_id: &str) {
        let mut scoped = self.scoped.write().unwrap();
        let state = scoped.entry(provider_id.to_owned()).or_default();
        let generation = state.generation.wrapping_add(1);
        *state = ScopedBuckets {
            generation,
            ..Default::default()
        };
    }

    pub fn remove(&self, provider_id: &str) {
        // Keep a generation tombstone so old in-flight results cannot resurrect
        // removed owner observations, even if the provider ID is later reused.
        self.reset_bucket_scope(provider_id);
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

/// Only the Google enforced-summary surface emits this observed alias. Preserve
/// opaque resource IDs and references while upgrading restored/display tier names.
/// Duplicate aliases remain conservative evidence, never an accidental recovery.
fn normalize_google_window_aliases(bucket: &mut QuotaBucket) {
    if bucket.source != "enforced_summary" || !bucket.tiers.iter().any(|tier| tier.name == "5h") {
        return;
    }
    let mut tiers: Vec<crate::admin::ProviderUsageTier> = Vec::new();
    for mut tier in std::mem::take(&mut bucket.tiers) {
        if tier.name == "5h" {
            tier.name = "five_hour".to_owned();
        }
        if let Some(old) = tiers.iter_mut().find(|old| old.name == tier.name) {
            bucket.stale = true;
            if tier.used_percent > old.used_percent {
                *old = tier;
            }
        } else {
            tiers.push(tier);
        }
    }
    bucket.tiers = tiers;
}

fn source_rank(source: &str) -> u8 {
    match source {
        "enforced_summary" | "configured" | "forwarded_route" => 2,
        "catalog" => 1,
        _ => 0,
    }
}

fn schedule_bucket(bucket: &QuotaBucket, unavailable: bool) -> ProviderScheduling {
    let blocking: Vec<_> = bucket
        .tiers
        .iter()
        .filter(|tier| tier.used_percent >= 100.0)
        .collect();
    // Forwarded buckets already include account/global admission evidence.
    let upstream_block = bucket.source == "forwarded_route"
        && bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted;
    let source_conflict = bucket.binding.source == "conflicting_official_v1";
    let exhausted = unavailable || !blocking.is_empty() || upstream_block || source_conflict;
    ProviderScheduling {
        status: if exhausted {
            ProviderSchedulingStatus::QuotaExhausted
        } else {
            ProviderSchedulingStatus::Eligible
        },
        reason: if unavailable {
            Some("account_unavailable".to_owned())
        } else if upstream_block {
            bucket.scheduling.reason.clone()
        } else if source_conflict {
            Some("conflicting_binding".to_owned())
        } else if exhausted {
            Some("usage_limit".to_owned())
        } else if bucket.source == "catalog" {
            Some("unknown_mapping".to_owned())
        } else if bucket.stale {
            Some("stale_observation".to_owned())
        } else if !bucket.is_resolved() {
            Some("unresolved_binding".to_owned())
        } else {
            None
        },
        blocking_tiers: if upstream_block {
            bucket.scheduling.blocking_tiers.clone()
        } else {
            blocking.iter().map(|tier| tier.name.clone()).collect()
        },
        reset_at: blocking
            .iter()
            .map(|tier| tier.resets_at.as_deref().and_then(parse_reset_at))
            .collect::<Option<Vec<_>>>()
            .and_then(|resets| resets.into_iter().max())
            .map(|reset| reset.to_rfc3339()),
        next_check_at: None,
    }
}

fn effective_buckets(state: &ScopedBuckets) -> Vec<QuotaBucket> {
    let mut buckets = state.buckets.clone();
    let globals: Vec<_> = buckets
        .iter()
        .filter(|bucket| bucket.is_global_constraint())
        .map(|bucket| bucket.id.clone())
        .collect();
    for bucket in buckets.iter_mut().filter(|bucket| bucket.is_primary()) {
        for id in &globals {
            if !bucket.constraint_ids.contains(id) {
                bucket.constraint_ids.push(id.clone());
            }
        }
    }
    // A discovery outage may still refresh identified enforced resources.
    // Recompose retained mappings from their exact window resource refs, never
    // from matching labels or percentages. Global refs are admission-only.
    for bucket in &mut buckets {
        if bucket.is_primary() {
            let ids = bucket.constraint_ids.clone();
            for resource in state.buckets.iter().filter(|resource| {
                ids.contains(&resource.id) && resource.binding.status != "shared_constraint"
            }) {
                for tier in &resource.tiers {
                    if let Some(old) = bucket.tiers.iter_mut().find(|old| old.name == tier.name) {
                        *old = tier.clone();
                    }
                }
            }
        }
        bucket.scheduling = schedule_bucket(bucket, state.unavailable);
    }
    let shared = buckets.clone();
    for bucket in &mut buckets {
        let ids = bucket.constraint_ids.clone();
        for constraint in shared
            .iter()
            .filter(|constraint| ids.contains(&constraint.id))
        {
            if constraint.binding.status == "window_constraint"
                && bucket.binding.status == "window_constraint"
            {
                continue;
            }
            if constraint.scheduling.status == ProviderSchedulingStatus::QuotaExhausted {
                bucket.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
                bucket.scheduling.reason = Some("shared_constraint".to_owned());
                for name in &constraint.scheduling.blocking_tiers {
                    if !bucket.scheduling.blocking_tiers.contains(name) {
                        bucket.scheduling.blocking_tiers.push(name.clone());
                    }
                }
                bucket.scheduling.reset_at = None;
            }
        }
        if bucket.scheduling.status == ProviderSchedulingStatus::Eligible
            && !bucket.models.is_empty()
            && bucket.models.iter().all(|model| {
                state
                    .cooldowns
                    .get(model)
                    .is_some_and(|until| *until > Instant::now())
            })
        {
            bucket.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
            bucket.scheduling.reason = Some("model_cooldown".to_owned());
        }
    }
    buckets
}

fn bucket_summary(buckets: &[QuotaBucket], unavailable: bool) -> ProviderScheduling {
    let primary: Vec<_> = buckets
        .iter()
        .filter(|bucket| bucket.is_primary())
        .collect();
    let unknown = primary.iter().any(|bucket| !bucket.is_resolved());
    let all_blocked = !primary.is_empty()
        && !unknown
        && primary
            .iter()
            .all(|bucket| bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted);
    let any_blocked = primary
        .iter()
        .any(|bucket| bucket.scheduling.status == ProviderSchedulingStatus::QuotaExhausted);
    ProviderScheduling {
        status: if unavailable || all_blocked {
            ProviderSchedulingStatus::QuotaExhausted
        } else {
            ProviderSchedulingStatus::Eligible
        },
        reason: if unavailable {
            Some("account_unavailable".to_owned())
        } else if all_blocked {
            Some("usage_limit".to_owned())
        } else if any_blocked {
            Some("partially_available".to_owned())
        } else if unknown || primary.is_empty() {
            Some("unknown_mapping".to_owned())
        } else {
            None
        },
        blocking_tiers: if all_blocked {
            primary
                .iter()
                .flat_map(|bucket| bucket.scheduling.blocking_tiers.iter().cloned())
                .collect()
        } else {
            Vec::new()
        },
        reset_at: None,
        next_check_at: None,
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

    fn bucket(id: &str, model: &str, five_hour: f64, weekly: f64) -> QuotaBucket {
        QuotaBucket {
            id: id.to_owned(),
            scope_id: "owner".to_owned(),
            label: model.to_owned(),
            models: vec![model.to_owned()],
            binding: crate::quota_buckets::QuotaBucketBinding {
                status: "resolved".to_owned(),
                source: "official".to_owned(),
                revision: 1,
            },
            tiers: vec![
                crate::admin::ProviderUsageTier {
                    name: "five_hour".to_owned(),
                    used_percent: five_hour,
                    resets_at: None,
                },
                crate::admin::ProviderUsageTier {
                    name: "weekly_limit".to_owned(),
                    used_percent: weekly,
                    resets_at: None,
                },
            ],
            constraint_ids: Vec::new(),
            source: "enforced_summary".to_owned(),
            queried_at: "2030-01-01T00:00:00Z".to_owned(),
            stale: false,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        }
    }

    #[test]
    fn scoped_windows_are_and_sibling_buckets_are_independent() {
        let registry = ProviderQuotaRegistry::new();
        let (buckets, summary) = registry.observe_buckets(
            "p",
            &[
                bucket("flash", "gemini-3.8-flash", 100.0, 20.0),
                bucket("argon", "gemini-4-argon", 5.0, 30.0),
            ],
            Some(true),
        );
        assert_eq!(buckets.len(), 2);
        assert!(!registry.is_model_schedulable("p", "gemini-3.8-flash"));
        assert!(registry.is_model_schedulable("p", "gemini-4-argon"));
        assert_eq!(summary.reason.as_deref(), Some("partially_available"));
        assert!(registry.is_schedulable("p"));
        registry.observe_buckets("p", &[bucket("argon", "gemini-4-argon", 5.0, 100.0)], None);
        assert!(!registry.is_model_schedulable("p", "gemini-4-argon"));
        assert!(!registry.is_schedulable("p"));
    }

    #[test]
    fn shared_global_blocks_every_bound_bucket_without_duplicate_display_tiers() {
        let registry = ProviderQuotaRegistry::new();
        let mut flash = bucket("flash", "flash", 10.0, 20.0);
        let mut argon = bucket("argon", "argon", 10.0, 20.0);
        let mut global = bucket("global", "", 100.0, 0.0);
        global.binding.status = "shared_constraint".to_owned();
        global.models.clear();
        flash.constraint_ids.push(global.id.clone());
        argon.constraint_ids.push(global.id.clone());
        registry.observe_buckets("p", &[flash, argon, global], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(!registry.is_model_schedulable("p", "argon"));
        assert_eq!(
            registry.model_snapshot("p", "flash").unwrap().tiers.len(),
            2
        );
        assert_eq!(
            registry.model_scheduling("p", "argon").reason.as_deref(),
            Some("shared_constraint")
        );
    }

    #[test]
    fn shared_constraint_only_gates_explicitly_referenced_models() {
        let registry = ProviderQuotaRegistry::new();
        let mut flash = bucket("flash", "flash", 10.0, 20.0);
        let argon = bucket("argon", "argon", 10.0, 20.0);
        let mut shared = bucket("shared", "", 100.0, 0.0);
        shared.binding.status = "shared_constraint".to_owned();
        shared.models.clear();
        flash.constraint_ids.push(shared.id.clone());
        registry.observe_buckets("p", &[flash, argon, shared], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(registry.is_model_schedulable("p", "argon"));
        assert!(registry.is_model_schedulable("p", "unknown-model"));
        assert_eq!(
            registry.model_scheduling("p", "flash").reason.as_deref(),
            Some("shared_constraint")
        );
        assert_eq!(
            registry.model_scheduling("p", "argon").status,
            ProviderSchedulingStatus::Eligible
        );
    }

    #[test]
    fn catalog_and_partial_refresh_cannot_clear_enforced_exhaustion() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets(
            "p",
            &[
                bucket("flash", "flash", 100.0, 20.0),
                bucket("argon", "argon", 10.0, 20.0),
            ],
            None,
        );
        let mut catalog = bucket("catalog-flash", "flash", 0.0, 0.0);
        catalog.source = "catalog".to_owned();
        registry.observe_buckets("p", &[catalog, bucket("argon", "argon", 20.0, 30.0)], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(registry.model_snapshot("p", "flash").unwrap().stale);
        assert!(registry.is_model_schedulable("p", "argon"));
        let mut partial = bucket("flash", "flash", 0.0, 10.0);
        partial.tiers.retain(|tier| tier.name == "weekly_limit");
        registry.observe_buckets("p", &[partial], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        registry.observe_buckets("p", &[bucket("flash", "flash", 10.0, 10.0)], None);
        assert!(registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn restored_google_five_hour_alias_recovers_only_from_same_resource_fresh_evidence() {
        let before = ProviderQuotaRegistry::new();
        let mut primary = bucket("flash", "flash", 100.0, 20.0);
        primary.tiers[0].name = "5h".to_owned();
        primary.constraint_ids = vec!["flash-five".to_owned()];
        let mut window = bucket("flash-five", "flash", 100.0, 20.0);
        window.tiers.truncate(1);
        window.tiers[0].name = "5h".to_owned();
        window.binding.status = "window_constraint".to_owned();
        before.observe_buckets("p", &[primary.clone(), window.clone()], None);
        // Persist a genuine pre-fix spelling rather than the normalized runtime view.
        let mut fences = before.persistent_bucket_fences("p");
        for fence in &mut fences {
            for tier in &mut fence.tiers {
                if tier.name == "five_hour" {
                    tier.name = "5h".to_owned();
                }
            }
        }
        let registry = ProviderQuotaRegistry::new();
        registry.restore_bucket_fences("p", &fences);
        assert!(!registry.is_model_schedulable("p", "flash"));
        let snapshot = registry.model_snapshot("p", "flash").unwrap();
        assert_eq!(snapshot.constraint_ids, vec!["flash-five"]);
        assert_eq!(snapshot.tiers.len(), 2);
        assert_eq!(snapshot.tiers[0].name, "five_hour");
        for (at, used, stale) in [
            ("2029-12-31T23:59:00Z", 20.0, false),
            ("2030-01-01T00:01:00Z", 20.0, true),
            ("invalid-time", 20.0, false),
            ("2030-01-01T00:01:00Z", f64::NAN, false),
            ("2030-01-01T00:01:00Z", -1.0, false),
        ] {
            let mut invalid = window.clone();
            invalid.tiers[0].name = "five_hour".to_owned();
            invalid.tiers[0].used_percent = used;
            invalid.queried_at = at.to_owned();
            invalid.stale = stale;
            registry.observe_buckets("p", &[invalid], None);
            assert!(
                !registry.is_model_schedulable("p", "flash"),
                "{at} {used} {stale}"
            );
        }
        let mut catalog = window.clone();
        catalog.source = "catalog".to_owned();
        catalog.tiers[0].used_percent = 0.0;
        registry.observe_buckets("p", &[catalog], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        let mut recovered = window.clone();
        recovered.tiers[0].name = "five_hour".to_owned();
        recovered.tiers[0].used_percent = 20.0;
        recovered.queried_at = "2030-01-01T00:02:00Z".to_owned();
        registry.observe_buckets("p", &[recovered], None);
        assert!(registry.is_model_schedulable("p", "flash"));
        let snapshot = registry.model_snapshot("p", "flash").unwrap();
        assert_eq!(snapshot.constraint_ids, vec!["flash-five"]);
        assert_eq!(snapshot.tiers.len(), 2);
        assert_eq!(snapshot.tiers[0].used_percent, 20.0);
        assert!(snapshot.tiers.iter().all(|tier| tier.name != "5h"));
    }

    #[test]
    fn google_alias_upgrade_cannot_cross_owner_generation_or_erase_other_windows() {
        let registry = ProviderQuotaRegistry::new();
        let mut old = bucket("same-resource", "flash", 100.0, 100.0);
        old.tiers[0].name = "5h".to_owned();
        registry.observe_buckets("p", &[old.clone()], None);
        let old_fence = registry.bucket_refresh_fence("p");
        let mut new_owner = old.clone();
        new_owner.scope_id = "new-owner".to_owned();
        registry.observe_buckets("p", &[new_owner.clone()], None);
        old.tiers[0].name = "five_hour".to_owned();
        old.tiers[0].used_percent = 0.0;
        old.tiers[1].used_percent = 0.0;
        registry.observe_buckets_fenced("p", &[old], None, old_fence);
        assert!(!registry.is_model_schedulable("p", "flash"));
        new_owner.queried_at = "2030-01-01T00:01:00Z".to_owned();
        new_owner.tiers.truncate(1);
        new_owner.tiers[0].name = "five_hour".to_owned();
        new_owner.tiers[0].used_percent = 20.0;
        registry.observe_buckets("p", &[new_owner], None);
        let snapshot = registry.model_snapshot("p", "flash").unwrap();
        assert_eq!(snapshot.scope_id, "new-owner");
        assert!(
            snapshot
                .tiers
                .iter()
                .any(|tier| tier.name == "weekly_limit" && tier.used_percent == 100.0)
        );
        assert!(!registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn unknown_mapping_does_not_hide_known_block_or_exhaust_provider() {
        let registry = ProviderQuotaRegistry::new();
        let mut unknown = bucket("unknown", "", 100.0, 100.0);
        unknown.models.clear();
        unknown.binding.status = "unresolved".to_owned();
        registry.observe_buckets("p", &[bucket("flash", "flash", 100.0, 20.0), unknown], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(registry.is_model_schedulable("p", "new-model"));
        assert!(registry.is_schedulable("p"));
    }

    #[test]
    fn model_rejection_is_narrow_temporary_and_survives_refresh() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets(
            "p",
            &[
                bucket("flash", "flash", 1.0, 2.0),
                bucket("argon", "argon", 1.0, 2.0),
            ],
            None,
        );
        registry.record_model_rejection("p", "flash", Some(30));
        registry.observe_buckets("p", &[bucket("flash", "flash", 0.0, 0.0)], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(registry.is_model_schedulable("p", "argon"));
        assert!(registry.is_schedulable("p"));
        registry
            .scoped
            .write()
            .unwrap()
            .get_mut("p")
            .unwrap()
            .cooldowns
            .insert("flash".to_owned(), Instant::now() - Duration::from_secs(1));
        assert!(registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn invalidate_and_owner_generation_fence_late_refreshes() {
        let registry = ProviderQuotaRegistry::new();
        let old = registry.bucket_refresh_fence("p");
        registry.invalidate("p");
        registry.observe_buckets_fenced("p", &[bucket("old", "flash", 100.0, 20.0)], None, old);
        assert!(registry.bucket_snapshot("p").is_empty());
        let old = registry.bucket_refresh_fence("p");
        registry.observe_buckets("p", &[bucket("first", "flash", 100.0, 20.0)], None);
        let mut new_owner = bucket("new", "argon", 5.0, 10.0);
        new_owner.scope_id = "different-owner".to_owned();
        registry.observe_buckets("p", &[new_owner], None);
        registry.observe_buckets_fenced("p", &[bucket("old", "flash", 100.0, 20.0)], None, old);
        assert!(registry.model_snapshot("p", "flash").is_none());
        assert!(registry.model_snapshot("p", "argon").is_some());
    }

    #[test]
    fn partial_member_discovery_preserves_same_resource_exhausted_binding() {
        let registry = ProviderQuotaRegistry::new();
        let mut first = bucket("group-g", "flash-low", 100.0, 20.0);
        first.models.push("flash-high".to_owned());
        registry.observe_buckets("p", &[first], None);
        let partial = bucket("group-g", "flash-low", 100.0, 20.0);
        registry.observe_buckets("p", &[partial], None);
        assert!(!registry.is_model_schedulable("p", "flash-high"));
        assert!(registry.model_snapshot("p", "flash-high").is_some());
        assert!(registry.model_snapshot("p", "flash-high").unwrap().stale);
    }

    #[test]
    fn trusted_constraint_rejection_fences_old_refresh_and_only_known_scope() {
        let registry = ProviderQuotaRegistry::new();
        let mut flash = bucket("flash", "flash", 10.0, 20.0);
        flash.constraint_ids = vec!["flash-five".to_owned(), "flash-week".to_owned()];
        let mut five = bucket("flash-five", "", 10.0, 20.0);
        five.binding.status = "window_constraint".to_owned();
        five.models.clear();
        five.tiers.retain(|tier| tier.name == "five_hour");
        let mut week = bucket("flash-week", "", 10.0, 20.0);
        week.binding.status = "window_constraint".to_owned();
        week.models.clear();
        week.tiers.retain(|tier| tier.name == "weekly_limit");
        registry.observe_buckets(
            "p",
            &[flash, five, week, bucket("argon", "argon", 10.0, 20.0)],
            None,
        );
        let fence = registry.bucket_refresh_fence("p");
        assert!(
            registry
                .record_constraint_rejection_fenced("p", "unknown-raw-id", &fence)
                .is_none()
        );
        assert!(
            registry
                .record_constraint_rejection_fenced("p", "flash", &fence)
                .is_none()
        );
        assert!(registry.is_model_schedulable("p", "flash"));
        assert!(
            registry
                .record_constraint_rejection_fenced("p", "flash-five", &fence)
                .is_some()
        );
        let snapshot = registry.model_snapshot("p", "flash").unwrap();
        assert_eq!(
            snapshot
                .tiers
                .iter()
                .find(|tier| tier.name == "five_hour")
                .unwrap()
                .used_percent,
            100.0
        );
        assert_eq!(
            snapshot
                .tiers
                .iter()
                .find(|tier| tier.name == "weekly_limit")
                .unwrap()
                .used_percent,
            20.0
        );
        assert!(!registry.is_model_schedulable("p", "flash"));
        assert!(registry.is_model_schedulable("p", "argon"));
        registry.observe_buckets_fenced("p", &[bucket("flash", "flash", 0.0, 0.0)], None, fence);
        assert!(!registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn captured_attempt_fence_rejects_late_owner_feedback() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets("p", &[bucket("a", "flash", 10.0, 20.0)], None);
        let owner_a = registry.bucket_refresh_fence("p");
        assert!(registry.bucket_fence_is_current("p", &owner_a));
        registry.invalidate("p");
        let mut replacement = bucket("b", "flash", 5.0, 10.0);
        replacement.scope_id = "owner-b".to_owned();
        registry.observe_buckets("p", &[replacement], None);
        assert!(!registry.bucket_fence_is_current("p", &owner_a));
        assert!(
            registry
                .record_model_rejection_fenced("p", "flash", Some(30), &owner_a)
                .is_none()
        );
        assert!(registry.is_model_schedulable("p", "flash"));
        let owner_b = registry.bucket_refresh_fence("p");
        assert!(
            registry
                .record_model_rejection_fenced("p", "flash", Some(30), &owner_b)
                .is_some()
        );
        assert!(!registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn cold_attempt_fence_is_invalid_after_first_owner_discovery() {
        let registry = ProviderQuotaRegistry::new();
        let unknown = registry.bucket_refresh_fence("p");
        assert!(registry.bucket_fence_is_current("p", &unknown));
        registry.observe_buckets("p", &[bucket("first", "flash", 10.0, 20.0)], None);
        assert!(!registry.bucket_fence_is_current("p", &unknown));
        assert!(
            registry
                .record_model_rejection_fenced("p", "flash", Some(30), &unknown)
                .is_none()
        );
        assert!(registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn mapping_generation_and_removal_fence_existing_attempts() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets("p", &[bucket("first", "flash", 10.0, 20.0)], None);
        let before_mapping = registry.bucket_refresh_fence("p");
        registry.replace_bucket_mapping("p", &registry.raw_bucket_snapshot("p"));
        assert!(!registry.bucket_fence_is_current("p", &before_mapping));
        let before_remove = registry.bucket_refresh_fence("p");
        registry.remove("p");
        assert!(!registry.bucket_fence_is_current("p", &before_remove));
        assert!(
            registry
                .record_model_rejection_fenced("p", "flash", None, &before_remove)
                .is_none()
        );
    }

    #[test]
    fn persisted_enforcement_survives_restart_and_expired_reset() {
        let before = ProviderQuotaRegistry::new();
        let mut flash = bucket("flash", "flash", 100.0, 20.0);
        flash.tiers[0].resets_at = Some("2020-01-01T00:00:00Z".to_owned());
        before.observe_buckets("p", &[flash], None);
        let serialized = serde_json::to_string(&before.persistent_bucket_fences("p")).unwrap();
        let fences: Vec<crate::quota_buckets::PersistentQuotaBucketFence> =
            serde_json::from_str(&serialized).unwrap();
        let after = ProviderQuotaRegistry::new();
        after.restore_bucket_fences("p", &fences);
        assert!(!after.is_model_schedulable("p", "flash"));
        assert!(after.bucket_snapshot("p")[0].stale);
        assert!(after.is_due("p"));
        after.observe_buckets("p", &[bucket("flash", "flash", 10.0, 20.0)], None);
        assert!(after.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn global_evidence_gates_unknown_model_without_guessing_membership() {
        let registry = ProviderQuotaRegistry::new();
        let mut global = bucket("global", "", 100.0, 0.0);
        global.binding.status = "shared_constraint".to_owned();
        global.models.clear();
        global.binding.source = "official_all_models_v1".to_owned();
        registry.observe_buckets("p", &[global], None);
        assert!(!registry.is_model_schedulable("p", "new-model"));
        assert_eq!(
            registry
                .model_scheduling("p", "new-model")
                .reason
                .as_deref(),
            Some("shared_constraint")
        );
    }

    #[test]
    fn forwarded_route_keeps_authoritative_gate_without_fake_quota_tiers() {
        let registry = ProviderQuotaRegistry::new();
        let mut flash = bucket("remote-flash", "remote-route-flash", 0.0, 0.0);
        flash.source = "forwarded_route".to_owned();
        flash.tiers.clear();
        flash.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
        flash.scheduling.reason = Some("usage_limit".to_owned());
        registry.observe_buckets("relay", &[flash], None);
        assert!(!registry.is_model_schedulable("relay", "remote-route-flash"));
        let mut recovered = bucket("remote-flash", "remote-route-flash", 0.0, 0.0);
        recovered.source = "forwarded_route".to_owned();
        recovered.tiers.clear();
        registry.observe_buckets("relay", &[recovered], None);
        assert!(registry.is_model_schedulable("relay", "remote-route-flash"));
    }

    #[test]
    fn stale_summary_cannot_claim_authoritative_recovery() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe_buckets("p", &[bucket("flash", "flash", 100.0, 20.0)], None);
        let mut stale = bucket("flash", "flash", 10.0, 10.0);
        stale.stale = true;
        registry.observe_buckets("p", &[stale], None);
        assert!(!registry.is_model_schedulable("p", "flash"));
    }

    #[test]
    fn model_methods_fall_back_to_unchanged_legacy_provider_state() {
        let registry = ProviderQuotaRegistry::new();
        registry.observe("legacy", &[tier("five_hour", 100.0, None)], None);
        assert!(!registry.is_model_schedulable("legacy", "anything"));
        assert_eq!(registry.model_tiers("legacy", "anything").len(), 1);
    }

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
