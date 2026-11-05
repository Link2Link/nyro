//! Target selection strategies for the proxy routing layer.
//!
//! # Architecture
//!
//! Each strategy implements the [`RoutingStrategy`] trait and returns an
//! ordered `Vec<SelectedTarget>` — the dispatcher tries them in order and
//! stops on the first successful upstream response.
//!
//! | Strategy   | Description                           |
//! |------------|---------------------------------------|
//! | `weighted` | Weighted reservoir sampling (default) |
//! | `priority` | Priority groups in ascending order    |
//! | `latency`  | Lowest streaming TTFB EWMA first      |
//! | `usage`    | Largest-window quota rate³ × window-perishability weights |
//!
//! `latency` orders targets by the time-to-first-token EWMA held in
//! [`LatencyRegistry`].
//!
//! Rows flagged `is_fallback` never enter the strategies: the selector
//! appends them after the strategy ordering, so the dispatcher only reaches
//! a fallback when every regular target has been skipped (quota / circuit /
//! disabled) or failed with a retryable error. Targets without a graduated fresh estimate (never
//! probed, still gathering their three-sample probing round, or stale past
//! the registry's freshness window) sort optimistically ahead of the known
//! ones so real traffic keeps probing them; among themselves they keep
//! `weight` descending order. Failures never enter the registry — the health
//! circuit breaker owns availability.
//!
//! # Usage
//!
//! ```rust,ignore
//! // Dispatcher
//! let ordered = TargetSelector::select_ordered(
//!     &route.balance,
//!     &targets,
//!     &gw.latency_registry,
//!     &gw.quota_registry,
//! );
//! ```

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use rand::Rng;

use super::decision::{DecisionCandidate, QuotaDecision, RouteDecision, SkipReason};
use super::health::HealthRegistry;
use super::latency::LatencyRegistry;
use super::quota::{ProviderQuotaRegistry, ProviderSchedulingStatus, QuotaTierObservation};
use super::usage::{ProviderUsageScore, dynamic_weight, score_provider, window_boost};
use crate::db::models::{ModelBackend, ModelBalance};
use crate::quota_buckets::QuotaBucket;

// ── SelectedTarget ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub provider_id: String,
    pub model: String,
}

// ── RoutingStrategy trait ─────────────────────────────────────────────────────

/// Produces an ordered list of targets to try, from most to least preferred.
///
/// The latency registry is threaded through so `LatencyStrategy` can rank
/// targets; the stateless strategies ignore it.
pub trait RoutingStrategy: Send + Sync {
    /// Order the targets and, in parallel, record the decision detail into
    /// `decision` — every configured row as one candidate with its score,
    /// weight, share, final rank, or the reason it was excluded.
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget>;
}

// ── Weighted ──────────────────────────────────────────────────────────────────

pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        _latency: &LatencyRegistry,
        _quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget> {
        for target in targets.iter().filter(|t| t.weight <= 0) {
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                provider_name: None,
                target: target.model.clone(),
                rank: None,
                weight: None,
                share: None,
                score: Some(serde_json::json!({ "static_weight": target.weight })),
                quota: None,
                skipped: Some(SkipReason::weight_zero()),
            });
        }
        let refs: Vec<&ModelBackend> = targets.iter().filter(|t| t.weight > 0).collect();
        let total: f64 = refs.iter().map(|t| t.weight as f64).sum();
        let shuffled = weighted_shuffle(&refs);
        for (index, target) in shuffled.iter().enumerate() {
            let weight = target.weight as f64;
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                provider_name: None,
                target: target.model.clone(),
                rank: Some(index + 1),
                weight: Some(weight),
                share: if total > 0.0 {
                    Some(weight / total)
                } else {
                    None
                },
                score: Some(serde_json::json!({ "static_weight": target.weight })),
                quota: None,
                skipped: None,
            });
        }
        shuffled.into_iter().map(to_selected).collect()
    }
}

// ── Priority ──────────────────────────────────────────────────────────────────

pub struct PriorityStrategy;

impl RoutingStrategy for PriorityStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        _latency: &LatencyRegistry,
        _quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget> {
        let mut groups: BTreeMap<i32, Vec<&ModelBackend>> = BTreeMap::new();
        for t in targets {
            groups.entry(t.priority).or_default().push(t);
        }
        let mut rank = 0_usize;
        let mut ordered = Vec::new();
        for (group_priority, group) in groups {
            for (in_group, target) in group.into_iter().enumerate() {
                rank += 1;
                decision.candidates.push(DecisionCandidate {
                    provider: target.provider_id.clone(),
                    provider_name: None,
                    target: target.model.clone(),
                    rank: Some(rank),
                    weight: None,
                    share: None,
                    score: Some(serde_json::json!({
                        "group": group_priority,
                        "in_group_rank": in_group + 1,
                    })),
                    quota: None,
                    skipped: None,
                });
                ordered.push(to_selected(target));
            }
        }
        ordered
    }
}

// ── Latency ───────────────────────────────────────────────────────────────────

/// Orders targets by streaming TTFB EWMA ascending (winner takes the traffic;
/// the rest stay warm standbys). Targets without a graduated estimate — never
/// probed, still gathering their three-sample probing round, or stale — probe
/// optimistically first, tie-broken by `weight` descending then declaration
/// order; graduated targets tie-break the same way.
pub struct LatencyStrategy;

impl RoutingStrategy for LatencyStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        _quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget> {
        let mut unknown: Vec<&ModelBackend> = Vec::new();
        let mut known: Vec<(&ModelBackend, f64)> = Vec::new();
        for t in targets {
            match latency.rank_ms(&t.provider_id, &t.model) {
                Some(rank) => known.push((t, rank)),
                None => unknown.push(t),
            }
        }
        // Heavier weight first. `sort_by` is stable, so declaration order
        // survives as the final tie-break in both partitions.
        unknown.sort_by_key(|t| std::cmp::Reverse(t.weight));
        known.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.weight.cmp(&a.0.weight))
        });
        let ordered = unknown
            .into_iter()
            .chain(known.iter().map(|(t, _)| *t))
            .collect::<Vec<_>>();
        for (index, target) in ordered.iter().enumerate() {
            let score = match latency.rank_ms(&target.provider_id, &target.model) {
                Some(ttft_ms) => serde_json::json!({
                    "state": "known",
                    "ttft_ms": ttft_ms.round() as i64,
                }),
                None => serde_json::json!({ "state": "probing" }),
            };
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                provider_name: None,
                target: target.model.clone(),
                rank: Some(index + 1),
                weight: None,
                share: None,
                score: Some(score),
                quota: None,
                skipped: None,
            });
        }
        ordered.into_iter().map(to_selected).collect()
    }
}

// ── Usage ─────────────────────────────────────────────────────────────────────

/// Orders only this route's targets by their applicable quota allowances.
/// Included quota precedes unknown fallback, then prepaid reserve, then blocked
/// rows. Within each pool, a provider gets ONE outer mass: the maximum eligible
/// bucket weight, never the sum. Independent bucket groups are then shuffled by
/// their own pacing weights; aliases sharing a bucket use static row weights.
/// This preserves the legacy provider lottery without allowing duplicate rows,
/// unrelated unknown mappings, or blocked sibling buckets to inflate its mass.
/// Blocked rows stay in the retry list so the dispatcher keeps its precise 503.
pub struct UsageStrategy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UsagePool {
    Included,
    Unknown,
    Reserve,
    Blocked,
}

/// A signature is identity, not utilization: equal percentages never merge
/// unrelated allowances. Unknown scoped models stay distinct controlled probes;
/// legacy provider-only observations retain their single shared group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum QuotaChoice {
    Bucket {
        scope: String,
        bucket: String,
        constraints: Vec<String>,
    },
    UnknownModel(String),
    Legacy,
}

struct QuotaTargetGroup<'a> {
    choice: QuotaChoice,
    targets: Vec<&'a ModelBackend>,
    score: Option<ProviderUsageScore>,
    pool: UsagePool,
    weight: f64,
}

struct ProviderTargetGroup<'a> {
    groups: Vec<QuotaTargetGroup<'a>>,
}

impl ProviderTargetGroup<'_> {
    fn mass(&self, pool: UsagePool) -> Option<f64> {
        self.groups
            .iter()
            .filter(|group| group.pool == pool)
            .map(|group| group.weight)
            .reduce(f64::max)
    }
}

impl RoutingStrategy for UsageStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        _latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget> {
        for target in targets.iter().filter(|target| target.weight <= 0) {
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                provider_name: None,
                target: target.model.clone(),
                rank: None,
                weight: None,
                share: None,
                score: None,
                quota: None,
                skipped: Some(SkipReason::weight_zero()),
            });
        }

        let now = chrono::Utc::now();
        let mut indexes: HashMap<&str, usize> = HashMap::new();
        let mut providers: Vec<ProviderTargetGroup<'_>> = Vec::new();
        for target in targets.iter().filter(|target| target.weight > 0) {
            let index = *indexes
                .entry(target.provider_id.as_str())
                .or_insert_with(|| {
                    providers.push(ProviderTargetGroup { groups: Vec::new() });
                    providers.len() - 1
                });
            let buckets = quota.bucket_snapshot(&target.provider_id);
            let bucket = quota.model_snapshot(&target.provider_id, &target.model);
            let choice = match &bucket {
                Some(bucket) => {
                    let mut constraints = bucket.constraint_ids.clone();
                    constraints.sort();
                    constraints.dedup();
                    QuotaChoice::Bucket {
                        scope: bucket.scope_id.clone(),
                        bucket: bucket.id.clone(),
                        constraints,
                    }
                }
                None if !buckets.is_empty() => QuotaChoice::UnknownModel(target.model.clone()),
                None => QuotaChoice::Legacy,
            };
            let score = match &bucket {
                Some(bucket) => score_bucket_choice(bucket, &buckets, now),
                None if !buckets.is_empty() => None,
                None => score_provider(&quota.model_tiers(&target.provider_id, &target.model), now),
            };
            let pool = if !quota.is_model_schedulable(&target.provider_id, &target.model) {
                UsagePool::Blocked
            } else if buckets.is_empty() && quota.is_on_reserve(&target.provider_id) {
                UsagePool::Reserve
            } else if score.is_some_and(|score| score.rate.is_finite() && score.rate > 0.0) {
                UsagePool::Included
            } else {
                UsagePool::Unknown
            };
            let weight = match pool {
                UsagePool::Included => score.map(usage_weight).unwrap_or(0.0),
                UsagePool::Unknown | UsagePool::Reserve => 1.0,
                UsagePool::Blocked => 0.0,
            };
            let groups = &mut providers[index].groups;
            // Pool is part of grouping: an exact-model cooldown must not gate
            // a healthy alias even when both consume the same allowance.
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.choice == choice && group.pool == pool)
            {
                group.targets.push(target);
            } else {
                groups.push(QuotaTargetGroup {
                    choice,
                    targets: vec![target],
                    score,
                    pool,
                    weight,
                });
            }
        }

        let mut ordered = Vec::new();
        let mut rank = 0;
        // Separate global passes prevent a provider's unknown/reserve siblings
        // jumping ahead of another provider's still-included quota.
        for pool in [
            UsagePool::Included,
            UsagePool::Unknown,
            UsagePool::Reserve,
            UsagePool::Blocked,
        ] {
            if pool == UsagePool::Blocked {
                // No lottery mass at all for blocked groups. Retain them only
                // for dispatcher's precise exhaustion response and diagnostics.
                for provider in &providers {
                    for group in provider.groups.iter().filter(|group| group.pool == pool) {
                        for target in weighted_shuffle(&group.targets) {
                            rank += 1;
                            let scheduling =
                                quota.model_scheduling(&target.provider_id, &target.model);
                            let mut candidate = usage_candidate(
                                target,
                                group.score,
                                None,
                                None,
                                Some(rank),
                                Some(SkipReason::quota(
                                    &scheduling.blocking_tiers,
                                    scheduling.reason.as_deref(),
                                )),
                            );
                            candidate.score.get_or_insert_with(|| serde_json::json!({}))["state"] =
                                serde_json::json!("blocked");
                            decision.candidates.push(candidate);
                            ordered.push(to_selected(target));
                        }
                    }
                }
                continue;
            }
            let provider_weights = providers
                .iter()
                .filter_map(|provider| provider.mass(pool).map(|mass| (provider, mass)))
                .collect::<Vec<_>>();
            let provider_total: f64 = provider_weights.iter().map(|(_, weight)| weight).sum();
            for (provider, outer_weight) in weighted_shuffle_pairs_by(&provider_weights) {
                let group_weights = provider
                    .groups
                    .iter()
                    .filter(|group| group.pool == pool)
                    .map(|group| (group, group.weight))
                    .collect::<Vec<_>>();
                let group_total: f64 = group_weights.iter().map(|(_, weight)| weight).sum();
                for (group, group_weight) in weighted_shuffle_pairs_by(&group_weights) {
                    let static_total: f64 = group
                        .targets
                        .iter()
                        .map(|target| target.weight as f64)
                        .sum();
                    for target in weighted_shuffle(&group.targets) {
                        rank += 1;
                        let mut candidate = usage_candidate(
                            target,
                            group.score,
                            Some(outer_weight),
                            share_of(outer_weight, provider_total),
                            Some(rank),
                            None,
                        );
                        let score = candidate.score.get_or_insert_with(|| serde_json::json!({}));
                        score["state"] = serde_json::json!(match pool {
                            UsagePool::Included => "included",
                            UsagePool::Unknown => "unknown_quota",
                            UsagePool::Reserve => "reserve",
                            UsagePool::Blocked => "blocked",
                        });
                        score["bucket_weight"] = serde_json::json!(group_weight);
                        score["bucket_share"] =
                            serde_json::json!(share_of(group_weight, group_total));
                        score["static_weight"] = serde_json::json!(target.weight);
                        score["target_share"] =
                            serde_json::json!(share_of(target.weight as f64, static_total));
                        decision.candidates.push(candidate);
                        ordered.push(to_selected(target));
                    }
                }
            }
        }
        ordered
    }
}

fn usage_weight(score: ProviderUsageScore) -> f64 {
    dynamic_weight(score.rate) * window_boost(score.window)
}

/// Each required allowance scores its own largest main window. Shared groups
/// cap effective pacing instead of being treated as alternative capacity, even
/// when their governing window differs from the primary bucket's window.
fn score_bucket_choice(
    bucket: &QuotaBucket,
    buckets: &[QuotaBucket],
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ProviderUsageScore> {
    let mut effective = score_bucket(bucket, now)?;
    for id in &bucket.constraint_ids {
        let constraint = buckets
            .iter()
            .find(|constraint| constraint.id == *id && constraint.scope_id == bucket.scope_id)?;
        // Copied raw window references belong to the primary allowance;
        // scoring them again would replace its largest-window policy with an
        // unintended minimum across windows. Only an additional shared budget
        // is an independent required allowance that caps effective pacing.
        if constraint.binding.status != "shared_constraint" {
            continue;
        }
        let score = score_bucket(constraint, now)?;
        if usage_weight(score) < usage_weight(effective) {
            effective = score;
        }
    }
    Some(effective)
}

fn score_bucket(
    bucket: &QuotaBucket,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ProviderUsageScore> {
    // Catalog observations do not establish canonical window duration, even
    // when an upstream display label happens to resemble a canonical name.
    if bucket.source == "catalog" {
        return None;
    }
    let tiers = bucket
        .tiers
        .iter()
        .map(|tier| QuotaTierObservation {
            name: tier.name.clone(),
            used_percent: tier.used_percent,
            resets_at: tier.resets_at.clone(),
        })
        .collect::<Vec<_>>();
    score_provider(&tiers, now)
}

fn usage_candidate(
    target: &ModelBackend,
    score: Option<super::usage::ProviderUsageScore>,
    weight: Option<f64>,
    share: Option<f64>,
    rank: Option<usize>,
    skipped: Option<SkipReason>,
) -> DecisionCandidate {
    let score_json = score.map(|s| {
        let mut value = serde_json::json!({
            "rate": (s.rate * 1000.0).round() / 1000.0,
            "window": s.window,
            "window_boost": (window_boost(s.window) * 1000.0).round() / 1000.0,
            "remaining_quota_pct": (s.remaining_quota * 10.0).round() / 10.0,
        });
        if let Some(remaining_time) = s.remaining_time {
            value["remaining_time_pct"] = serde_json::json!((remaining_time * 10.0).round() / 10.0);
        }
        value
    });
    DecisionCandidate {
        provider: target.provider_id.clone(),
        provider_name: None,
        target: target.model.clone(),
        rank,
        weight,
        share,
        score: score_json,
        quota: None,
        skipped,
    }
}

fn share_of(weight: f64, total: f64) -> Option<f64> {
    if total > 0.0 {
        Some((weight / total * 10000.0).round() / 10000.0)
    } else {
        None
    }
}

// ── TargetSelector (public entry point) ───────────────────────────────────────

pub struct TargetSelector;

impl TargetSelector {
    /// Return targets ordered by the named balance. Unrecognised balance
    /// strings fall back to `weighted`. Discards the decision snapshot.
    pub fn select_ordered(
        balance: &str,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
    ) -> Vec<SelectedTarget> {
        Self::select_ordered_traced(balance, targets, latency, quota, &HealthRegistry::new()).0
    }

    /// Same ordering as [`select_ordered`], plus the decision snapshot the
    /// strategy produced at selection time. After the strategy fills its own
    /// strategy-specific detail, ordered-but-unannotated candidates get the
    /// dispatcher-level skip facts (quota exhausted / circuit open) so the
    /// snapshot answers "why did this configured row receive no traffic".
    pub fn select_ordered_traced(
        balance: &str,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
        health: &HealthRegistry,
    ) -> (Vec<SelectedTarget>, RouteDecision) {
        let resolved = ModelBalance::from_str(balance).unwrap_or_default();
        let balance_name = resolved.as_str();
        let mut decision = RouteDecision::new(balance_name);
        // Fallback rows are invisible to the balance strategies; they are
        // appended after the strategy ordering as last-resort targets.
        let regular: Vec<ModelBackend> = targets
            .iter()
            .filter(|target| !target.is_fallback)
            .cloned()
            .collect();
        let fallback: Vec<ModelBackend> = targets
            .iter()
            .filter(|target| target.is_fallback)
            .cloned()
            .collect();
        let mut ordered = match resolved {
            ModelBalance::Weighted => {
                WeightedStrategy.select_ordered(&regular, latency, quota, &mut decision)
            }
            ModelBalance::Priority => {
                PriorityStrategy.select_ordered(&regular, latency, quota, &mut decision)
            }
            ModelBalance::Latency => {
                LatencyStrategy.select_ordered(&regular, latency, quota, &mut decision)
            }
            ModelBalance::Usage => {
                UsageStrategy.select_ordered(&regular, latency, quota, &mut decision)
            }
        };
        let base_rank = decision
            .candidates
            .iter()
            .filter_map(|candidate| candidate.rank)
            .max()
            .unwrap_or(0);
        for (offset, target) in fallback.iter().enumerate() {
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                provider_name: None,
                target: target.model.clone(),
                rank: Some(base_rank + offset + 1),
                weight: None,
                share: None,
                score: Some(serde_json::json!({ "state": "fallback" })),
                quota: None,
                skipped: None,
            });
            ordered.push(to_selected(target));
        }
        for candidate in &mut decision.candidates {
            let evidence = quota_decision(quota, &candidate.provider, &candidate.target);
            if candidate.skipped.is_none() && candidate.rank.is_some() {
                if evidence.scheduling.status == ProviderSchedulingStatus::QuotaExhausted {
                    candidate.skipped = Some(SkipReason::quota(
                        &evidence.scheduling.blocking_tiers,
                        evidence.scheduling.reason.as_deref(),
                    ));
                } else if let Some(secs) =
                    health.open_model_secs(&candidate.provider, &candidate.target)
                {
                    candidate.skipped = Some(SkipReason::circuit(secs));
                }
            }
            candidate.quota = Some(evidence);
        }
        (ordered, decision)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn quota_decision(quota: &ProviderQuotaRegistry, provider: &str, model: &str) -> QuotaDecision {
    let buckets = quota.bucket_snapshot(provider);
    let bucket = quota.model_snapshot(provider, model);
    let scheduling = quota.model_scheduling(provider, model);
    let blocked = scheduling.status == ProviderSchedulingStatus::QuotaExhausted;
    let mut blocking_constraint_ids = Vec::new();
    // Include conflicting bindings too: a known blocked conflict must not be
    // presented as a harmless unknown or as a provider-wide sibling block.
    for primary in buckets
        .iter()
        .filter(|bucket| bucket.is_primary() && bucket.models.iter().any(|id| id == model))
    {
        if primary.tiers.iter().any(|tier| tier.used_percent >= 100.0) {
            blocking_constraint_ids.push(primary.id.clone());
        }
        for constraint in buckets.iter().filter(|constraint| {
            constraint.scope_id == primary.scope_id
                && primary.constraint_ids.contains(&constraint.id)
        }) {
            if constraint.scheduling.status == ProviderSchedulingStatus::QuotaExhausted {
                blocking_constraint_ids.push(constraint.id.clone());
            }
        }
    }
    blocking_constraint_ids.sort();
    blocking_constraint_ids.dedup();
    let blocker_scope = blocked.then(|| {
        match scheduling.reason.as_deref() {
            Some("account_unavailable") => "provider",
            Some("model_cooldown") => "model",
            Some("shared_constraint") => "shared_constraint",
            _ if bucket.is_some() => "bucket",
            _ if buckets.is_empty() => "provider",
            _ => "model",
        }
        .to_string()
    });
    let mut constraint_ids = bucket
        .as_ref()
        .map(|bucket| bucket.constraint_ids.clone())
        .unwrap_or_default();
    constraint_ids.sort();
    constraint_ids.dedup();
    QuotaDecision {
        bucket_id: bucket.as_ref().map(|bucket| bucket.id.clone()),
        scope_id: bucket.as_ref().map(|bucket| bucket.scope_id.clone()),
        constraint_ids,
        binding_status: bucket
            .as_ref()
            .map(|bucket| bucket.binding.status.clone())
            .unwrap_or_else(|| {
                if buckets.is_empty() {
                    "legacy"
                } else if buckets
                    .iter()
                    .any(|bucket| bucket.is_primary() && bucket.models.iter().any(|id| id == model))
                {
                    "conflict"
                } else {
                    "unresolved"
                }
                .to_string()
            }),
        binding_source: bucket.as_ref().map(|bucket| bucket.binding.source.clone()),
        revision: bucket.as_ref().map(|bucket| bucket.binding.revision),
        source: bucket.as_ref().map(|bucket| bucket.source.clone()),
        queried_at: bucket.as_ref().map(|bucket| bucket.queried_at.clone()),
        stale: bucket.as_ref().map(|bucket| bucket.stale),
        scheduling,
        blocker_scope,
        blocking_constraint_ids,
    }
}

#[inline]
fn to_selected(t: &ModelBackend) -> SelectedTarget {
    SelectedTarget {
        provider_id: t.provider_id.clone(),
        model: t.model.clone(),
    }
}

fn weighted_shuffle<'a>(targets: &[&'a ModelBackend]) -> Vec<&'a ModelBackend> {
    let weighted = targets
        .iter()
        .filter(|target| target.weight > 0)
        .map(|target| (*target, target.weight as f64))
        .collect::<Vec<_>>();
    weighted_shuffle_by(&weighted)
}

fn weighted_shuffle_by<T: Copy>(items: &[(T, f64)]) -> Vec<T> {
    let mut rng = rand::thread_rng();
    weighted_shuffle_by_rng(items, &mut rng)
}

/// Weighted shuffle that keeps each item's weight alongside it, for callers
/// that report the weight (usage route-decision snapshots).
fn weighted_shuffle_pairs_by<T: Copy>(items: &[(T, f64)]) -> Vec<(T, f64)> {
    let mut rng = rand::thread_rng();
    let mut keyed = items
        .iter()
        .filter(|(_, weight)| weight.is_finite() && *weight > 0.0)
        .map(|(item, weight)| {
            let key = rng.r#gen::<f64>().powf(1.0 / *weight);
            (*item, *weight, key)
        })
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    keyed
        .into_iter()
        .map(|(item, weight, _)| (item, weight))
        .collect()
}

fn weighted_shuffle_by_rng<T: Copy, R: Rng + ?Sized>(items: &[(T, f64)], rng: &mut R) -> Vec<T> {
    let mut keyed = items
        .iter()
        .filter(|(_, weight)| weight.is_finite() && *weight > 0.0)
        .map(|(item, weight)| {
            let key = rng.r#gen::<f64>().powf(1.0 / *weight);
            (*item, key)
        })
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    keyed.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::ProviderUsageTier;
    use crate::quota_buckets::QuotaBucketBinding;
    use crate::router::quota::ProviderScheduling;
    use rand::SeedableRng;
    use std::time::Duration;

    fn backend(id: &str, provider_id: &str, model: &str, weight: i32) -> ModelBackend {
        ModelBackend {
            id: id.to_string(),
            model_id: "m".to_string(),
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            weight,
            priority: 1,
            is_fallback: false,
            created_at: String::new(),
        }
    }

    fn fallback_backend(id: &str, provider_id: &str, model: &str) -> ModelBackend {
        ModelBackend {
            is_fallback: true,
            ..backend(id, provider_id, model, 100)
        }
    }

    fn ordered_models(ordered: &[SelectedTarget]) -> Vec<String> {
        ordered.iter().map(|t| t.model.clone()).collect()
    }

    fn bucket(id: &str, models: &[&str], window: &str, used: f64) -> QuotaBucket {
        QuotaBucket {
            id: id.to_string(),
            scope_id: "owner-scope".to_string(),
            label: id.to_string(),
            models: models.iter().map(|model| (*model).to_string()).collect(),
            binding: QuotaBucketBinding {
                status: "resolved".to_string(),
                source: "configured".to_string(),
                revision: 7,
            },
            tiers: vec![ProviderUsageTier {
                name: window.to_string(),
                used_percent: used,
                resets_at: None,
            }],
            constraint_ids: Vec::new(),
            source: "enforced_summary".to_string(),
            queried_at: chrono::Utc::now().to_rfc3339(),
            stale: false,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        }
    }

    fn candidate<'a>(decision: &'a RouteDecision, model: &str) -> &'a DecisionCandidate {
        decision
            .candidates
            .iter()
            .find(|candidate| candidate.target == model)
            .unwrap()
    }

    #[test]
    fn latency_orders_known_targets_by_ttft_ascending() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);
        for _ in 0..3 {
            registry.record("p2", "slow", 900);
            registry.record("p1", "fast", 100);
            registry.record("p3", "mid", 500);
        }
        let targets = vec![
            backend("1", "p1", "fast", 10),
            backend("2", "p2", "slow", 90),
            backend("3", "p3", "mid", 50),
        ];

        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        assert_eq!(
            ordered_models(&ordered),
            vec!["fast", "mid", "slow"],
            "weight must not override TTFT for known targets"
        );
    }

    #[test]
    fn latency_probes_unknown_targets_optimistically_first() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);
        for _ in 0..3 {
            registry.record("p-known", "known", 100);
        }
        let targets = vec![
            backend("1", "p-known", "known", 99),
            backend("2", "p-unknown-a", "ua", 10),
            backend("3", "p-unknown-b", "ub", 80),
        ];

        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        assert_eq!(ordered_models(&ordered), vec!["ub", "ua", "known"]);
    }

    #[test]
    fn unknown_targets_tie_break_by_weight_desc_then_declaration() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);
        let targets = vec![
            backend("1", "p", "light", 10),
            backend("2", "p", "heavy", 80),
            backend("3", "p", "equal-heavy", 80),
        ];

        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        assert_eq!(
            ordered_models(&ordered),
            vec!["heavy", "equal-heavy", "light"]
        );
    }

    #[test]
    fn stale_samples_rejoin_the_optimistic_probe_group() {
        let registry = LatencyRegistry::with_config(Duration::from_millis(40), 0.4);
        for _ in 0..3 {
            registry.record("p-fast", "fast", 900);
        }
        std::thread::sleep(Duration::from_millis(60));
        for _ in 0..3 {
            registry.record("p-fresh", "fresh", 800);
        }
        let targets = vec![
            backend("1", "p-fast", "fast", 5),
            backend("2", "p-fresh", "fresh", 1),
        ];

        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        // `fast` went stale → unknown → probes first even though its last
        // graduated mean (900) was slower than the fresh `fresh` (800).
        assert_eq!(ordered_models(&ordered), vec!["fast", "fresh"]);
    }

    #[test]
    fn collecting_targets_probe_ahead_until_the_round_completes() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);
        // p-fast has two fast samples (still collecting), p-slow graduated slow.
        registry.record("p-fast", "fast", 100);
        registry.record("p-fast", "fast", 100);
        for _ in 0..3 {
            registry.record("p-slow", "slow", 900);
        }
        let targets = vec![
            backend("1", "p-fast", "fast", 1),
            backend("2", "p-slow", "slow", 99),
        ];

        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        // 2/3 samples: still unknown → probes first despite the lower weight.
        assert_eq!(
            ordered_models(&ordered),
            vec!["fast", "slow"],
            "collecting targets stay in the probe group"
        );

        // Third sample graduates the round; its mean wins on merit.
        registry.record("p-fast", "fast", 100);
        let ordered = TargetSelector::select_ordered(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );
        assert_eq!(
            ordered_models(&ordered),
            vec!["fast", "slow"],
            "graduated mean 100 beats 900"
        );
    }

    #[test]
    fn cubed_rates_drive_weighted_first_choice_probability() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let items = [("a", dynamic_weight(1.2)), ("b", dynamic_weight(0.9))];
        let mut a_first = 0_u32;
        let samples = 20_000_u32;
        for _ in 0..samples {
            let ordered = weighted_shuffle_by_rng(&items, &mut rng);
            if ordered.first() == Some(&"a") {
                a_first += 1;
            }
        }
        let observed = f64::from(a_first) / f64::from(samples);
        let expected = 1.728 / (1.728 + 0.729);

        assert!(
            (observed - expected).abs() < 0.02,
            "{observed} vs {expected}"
        );
    }

    #[test]
    fn usage_ranks_by_required_acceleration_with_unknown_fallback_last() {
        let quota = ProviderQuotaRegistry::new();
        // Stale last-good reset: rate capped at the bound -> weight 1000.
        quota.observe(
            "urgent",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 40.0,
                resets_at: Some("2020-01-01T00:00:00Z".to_string()),
            }],
            None,
        );
        // 90% spent with half the window left -> rate 0.2 -> weight 0.008.
        let half_window_ahead = (chrono::Utc::now() + chrono::Duration::hours(84)).to_rfc3339();
        quota.observe(
            "relaxed",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 90.0,
                resets_at: Some(half_window_ahead),
            }],
            None,
        );
        let targets = vec![
            backend("1", "relaxed", "relaxed", 100),
            backend("2", "unknown", "unknown", 100),
            backend("3", "urgent", "urgent", 100),
        ];

        // The urgent provider's weight dominates by five orders of magnitude,
        // so the ordering is stable across the sampled runs.
        for _ in 0..500 {
            let ordered =
                TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

            assert_eq!(ordered[0].provider_id, "urgent");
            assert_eq!(ordered[1].provider_id, "relaxed");
            assert_eq!(ordered[2].provider_id, "unknown");
        }
    }

    #[test]
    fn exhausted_five_hour_provider_does_not_block_long_term_pool() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe(
            "five",
            &[QuotaTierObservation {
                name: "five_hour".to_string(),
                used_percent: 100.0,
                resets_at: None,
            }],
            None,
        );
        quota.observe(
            "long",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 20.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "five", "five", 100),
            backend("2", "long", "long", 100),
        ];

        let ordered =
            TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

        assert_eq!(ordered_models(&ordered), vec!["long", "five"]);
    }

    #[test]
    fn long_term_provider_still_honors_five_hour_hard_quota_guard() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe(
            "blocked",
            &[
                QuotaTierObservation {
                    name: "five_hour".to_string(),
                    used_percent: 100.0,
                    resets_at: None,
                },
                QuotaTierObservation {
                    name: "weekly_limit".to_string(),
                    used_percent: 10.0,
                    resets_at: None,
                },
            ],
            None,
        );
        quota.observe(
            "eligible",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 60.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "blocked", "blocked", 100),
            backend("2", "eligible", "eligible", 100),
        ];

        let ordered =
            TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

        assert_eq!(ordered_models(&ordered), vec!["eligible", "blocked"]);
    }

    #[test]
    fn duplicate_provider_targets_stay_contiguous_and_do_not_form_extra_groups() {
        let quota = ProviderQuotaRegistry::new();
        for provider in ["a", "b"] {
            quota.observe(
                provider,
                &[QuotaTierObservation {
                    name: "weekly_limit".to_string(),
                    used_percent: 40.0,
                    resets_at: None,
                }],
                None,
            );
        }
        let targets = vec![
            backend("1", "a", "a1", 90),
            backend("2", "b", "b1", 100),
            backend("3", "a", "a2", 10),
        ];

        let samples = 10_000_u32;
        let mut a_first = 0_u32;
        let mut a1_first = 0_u32;
        for _ in 0..samples {
            let ordered =
                TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);
            if ordered
                .first()
                .is_some_and(|target| target.provider_id == "a")
            {
                a_first += 1;
                if ordered.first().is_some_and(|target| target.model == "a1") {
                    a1_first += 1;
                }
            }
            let providers = ordered
                .iter()
                .map(|target| target.provider_id.as_str())
                .collect::<Vec<_>>();
            let a_positions = providers
                .iter()
                .enumerate()
                .filter_map(|(index, provider)| (*provider == "a").then_some(index))
                .collect::<Vec<_>>();
            assert_eq!(a_positions.len(), 2);
            assert_eq!(a_positions[1], a_positions[0] + 1);
        }
        let provider_share = f64::from(a_first) / f64::from(samples);
        let internal_share = f64::from(a1_first) / f64::from(a_first);

        assert!((provider_share - 0.5).abs() < 0.03, "{provider_share}");
        assert!((internal_share - 0.9).abs() < 0.03, "{internal_share}");
    }

    #[test]
    fn unknown_duplicate_rows_do_not_amplify_provider_probability() {
        let quota = ProviderQuotaRegistry::new();
        let targets = vec![
            backend("1", "a", "a1", 100),
            backend("2", "a", "a2", 100),
            backend("3", "b", "b1", 100),
        ];
        let samples = 10_000_u32;
        let mut a_first = 0_u32;
        for _ in 0..samples {
            let ordered =
                TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);
            if ordered
                .first()
                .is_some_and(|target| target.provider_id == "a")
            {
                a_first += 1;
            }
            let providers = ordered
                .iter()
                .map(|target| target.provider_id.as_str())
                .collect::<Vec<_>>();
            let a_positions = providers
                .iter()
                .enumerate()
                .filter_map(|(index, provider)| (*provider == "a").then_some(index))
                .collect::<Vec<_>>();
            assert_eq!(a_positions[1], a_positions[0] + 1);
        }
        let observed = f64::from(a_first) / f64::from(samples);

        assert!((observed - 0.5).abs() < 0.03, "observed={observed}");
    }

    #[test]
    fn usage_excludes_zero_static_weight_rows() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe(
            "p",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 10.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "p", "active", 100),
            backend("2", "p", "disabled", 0),
        ];

        let ordered =
            TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

        assert_eq!(ordered_models(&ordered), vec!["active"]);
    }

    #[test]
    fn unknown_balance_falls_back_to_weighted() {
        let registry = LatencyRegistry::new();
        let targets = vec![backend("1", "p", "only", 100)];

        let ordered = TargetSelector::select_ordered(
            "nonsense",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
        );

        assert_eq!(ordered_models(&ordered), vec!["only"]);
    }

    // ── Route-decision snapshot ─────────────────────────────────────────────

    #[test]
    fn weighted_decision_records_all_candidates_with_zero_weight_skip() {
        let targets = vec![
            backend("1", "a", "a1", 80),
            backend("2", "b", "b1", 0),
            backend("3", "c", "c1", 20),
        ];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "weighted",
            &targets,
            &LatencyRegistry::new(),
            &ProviderQuotaRegistry::new(),
            &HealthRegistry::new(),
        );

        assert_eq!(decision.balance, "weighted");
        assert_eq!(decision.candidates.len(), 3);
        assert_eq!(ordered.len(), 2);
        let zero = decision
            .candidates
            .iter()
            .find(|c| c.target == "b1")
            .expect("zero-weight row stays in snapshot");
        assert_eq!(zero.rank, None);
        assert_eq!(zero.skipped.as_ref().unwrap().reason, "weight_zero");
        let shares: f64 = decision.candidates.iter().filter_map(|c| c.share).sum();
        assert!((shares - 1.0).abs() < 1e-6, "shares sum to 1: {shares}");
        let ranked: Vec<usize> = decision.candidates.iter().filter_map(|c| c.rank).collect();
        assert_eq!(ranked, vec![1, 2]);
        let json = decision.to_json();
        assert!(json.contains("static_weight"));
    }

    #[test]
    fn fallback_row_is_appended_last_for_every_balance() {
        let fallback = fallback_backend("9", "z", "last-resort");
        for balance in ["weighted", "priority", "latency", "usage"] {
            let targets = vec![
                backend("1", "a", "a1", 80),
                backend("2", "b", "b1", 20),
                fallback.clone(),
            ];
            let ordered = TargetSelector::select_ordered(
                balance,
                &targets,
                &LatencyRegistry::new(),
                &ProviderQuotaRegistry::new(),
            );
            assert_eq!(
                ordered.last().map(|t| t.model.as_str()),
                Some("last-resort"),
                "{balance}: fallback stays last"
            );
            assert_eq!(ordered.len(), 3, "{balance}: fallback still dispatched");
        }
    }

    #[test]
    fn fallback_decision_records_state_and_contiguous_rank() {
        let targets = vec![
            backend("1", "a", "a1", 80),
            backend("2", "b", "b1", 20),
            fallback_backend("9", "z", "last-resort"),
        ];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "weighted",
            &targets,
            &LatencyRegistry::new(),
            &ProviderQuotaRegistry::new(),
            &HealthRegistry::new(),
        );

        let candidate = decision
            .candidates
            .iter()
            .find(|c| c.target == "last-resort")
            .expect("fallback row stays in snapshot");
        assert_eq!(candidate.rank, Some(3));
        assert_eq!(candidate.score.as_ref().unwrap()["state"], "fallback");
        assert!(candidate.skipped.is_none());
        let mut ranked: Vec<usize> = decision.candidates.iter().filter_map(|c| c.rank).collect();
        ranked.sort_unstable();
        assert_eq!(ranked, vec![1, 2, 3]);
        assert_eq!(ordered.last().unwrap().model, "last-resort");
    }

    #[test]
    fn fallback_row_keeps_quota_skip_annotation() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe(
            "z",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 100.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "a", "a1", 100),
            fallback_backend("9", "z", "last-resort"),
        ];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "weighted",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        let candidate = decision
            .candidates
            .iter()
            .find(|c| c.target == "last-resort")
            .unwrap();
        let skipped = candidate.skipped.as_ref().unwrap();
        assert_eq!(skipped.reason, "quota_exhausted");
    }

    #[test]
    fn priority_decision_records_groups_and_in_group_rank() {
        let mut second = backend("2", "b", "b1", 100);
        second.priority = 1;
        let mut first = backend("1", "a", "a1", 100);
        first.priority = 2;
        let targets = vec![second, first];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "priority",
            &targets,
            &LatencyRegistry::new(),
            &ProviderQuotaRegistry::new(),
            &HealthRegistry::new(),
        );

        let a1 = decision
            .candidates
            .iter()
            .find(|c| c.target == "a1")
            .unwrap();
        // priority 1 (b1) is tried before priority 2 (a1).
        assert_eq!(a1.rank, Some(2));
        assert_eq!(a1.score.as_ref().unwrap()["group"], 2);
        let b1 = decision
            .candidates
            .iter()
            .find(|c| c.target == "b1")
            .unwrap();
        assert_eq!(b1.rank, Some(1));
        assert_eq!(b1.score.as_ref().unwrap()["group"], 1);
    }

    #[test]
    fn latency_decision_records_ttft_and_probing_state() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(600), 0.4);
        for _ in 0..3 {
            registry.record("p-known", "known", 250);
        }
        let targets = vec![
            backend("1", "p-known", "known", 50),
            backend("2", "p-fresh", "fresh", 50),
        ];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "latency",
            &targets,
            &registry,
            &ProviderQuotaRegistry::new(),
            &HealthRegistry::new(),
        );

        let fresh = decision
            .candidates
            .iter()
            .find(|c| c.target == "fresh")
            .unwrap();
        assert_eq!(fresh.rank, Some(1));
        assert_eq!(fresh.score.as_ref().unwrap()["state"], "probing");
        let known = decision
            .candidates
            .iter()
            .find(|c| c.target == "known")
            .unwrap();
        assert_eq!(known.score.as_ref().unwrap()["state"], "known");
        assert_eq!(known.score.as_ref().unwrap()["ttft_ms"], 250);
    }

    #[test]
    fn usage_decision_records_rate_window_and_quota_skip() {
        let quota = ProviderQuotaRegistry::new();
        let half_week_ahead = (chrono::Utc::now() + chrono::Duration::hours(84)).to_rfc3339();
        quota.observe(
            "hot",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 40.0,
                resets_at: Some(half_week_ahead),
            }],
            None,
        );
        quota.observe(
            "blocked",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 100.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "hot", "hot-model", 100),
            backend("2", "blocked", "blocked-model", 100),
            backend("3", "mystery", "mystery-model", 100),
        ];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        assert_eq!(ordered[0].provider_id, "hot");
        let hot = decision
            .candidates
            .iter()
            .find(|c| c.provider == "hot")
            .unwrap();
        assert_eq!(hot.rank, Some(1));
        assert_eq!(hot.score.as_ref().unwrap()["window"], "weekly_limit");
        assert_eq!(hot.score.as_ref().unwrap()["remaining_quota_pct"], 60.0);
        assert!(hot.share.is_some_and(|s| (s - 1.0).abs() < 1e-6));
        let blocked = decision
            .candidates
            .iter()
            .find(|c| c.provider == "blocked")
            .unwrap();
        let skip = blocked.skipped.as_ref().unwrap();
        assert_eq!(skip.reason, "quota_exhausted");
        assert_eq!(skip.window.as_deref(), Some("monthly"));
        let mystery = decision
            .candidates
            .iter()
            .find(|c| c.provider == "mystery")
            .unwrap();
        assert_eq!(mystery.score.as_ref().unwrap()["state"], "unknown_quota");
    }

    /// An exhausted `daily` window covered by a prepaid reserve, while the
    /// weekly window (the one scoring would normally govern by) is nearly
    /// untouched — the shape that would otherwise rank the provider FIRST.
    fn observe_reserve(quota: &ProviderQuotaRegistry, provider: &str) {
        let half_week_ahead = (chrono::Utc::now() + chrono::Duration::hours(84)).to_rfc3339();
        quota.observe_with_reserve(
            provider,
            &[
                QuotaTierObservation {
                    name: "daily".to_string(),
                    used_percent: 100.0,
                    resets_at: None,
                },
                QuotaTierObservation {
                    name: "weekly_limit".to_string(),
                    used_percent: 9.0,
                    resets_at: Some(half_week_ahead),
                },
            ],
            Some(true),
            true,
        );
    }

    #[test]
    fn usage_ranks_prepaid_reserve_after_included_quota_and_unknown_pools() {
        let quota = ProviderQuotaRegistry::new();
        let half_week_ahead = (chrono::Utc::now() + chrono::Duration::hours(84)).to_rfc3339();
        quota.observe(
            "included",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 95.0,
                resets_at: Some(half_week_ahead),
            }],
            None,
        );
        observe_reserve(&quota, "reserve");
        quota.observe(
            "blocked",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 100.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "blocked", "blocked", 100),
            backend("2", "reserve", "reserve", 100),
            backend("3", "unknown", "unknown", 100),
            backend("4", "included", "included", 100),
        ];

        // Even an almost-spent included provider and an unknown-quota one beat
        // the reserve provider, whatever the shuffle draws; blocked stays last.
        for _ in 0..200 {
            let ordered =
                TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

            assert_eq!(ordered[0].provider_id, "included");
            assert_eq!(ordered[1].provider_id, "unknown");
            assert_eq!(ordered[2].provider_id, "reserve");
            assert_eq!(ordered[3].provider_id, "blocked");
        }
    }

    #[test]
    fn usage_decision_labels_the_reserve_pool_and_keeps_the_provider_schedulable() {
        let quota = ProviderQuotaRegistry::new();
        observe_reserve(&quota, "reserve");
        let targets = vec![backend("1", "reserve", "reserve-model", 100)];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        // Reserve coverage never removes the provider from the candidate list.
        assert_eq!(ordered.len(), 1);
        let candidate = decision
            .candidates
            .iter()
            .find(|c| c.provider == "reserve")
            .unwrap();
        assert_eq!(candidate.rank, Some(1));
        assert!(candidate.skipped.is_none());
        assert_eq!(candidate.score.as_ref().unwrap()["state"], "reserve");
    }

    #[test]
    fn reserve_does_not_change_non_usage_strategies() {
        let quota = ProviderQuotaRegistry::new();
        observe_reserve(&quota, "reserve");
        let targets = vec![
            backend("1", "reserve", "reserve", 100),
            backend("2", "other", "other", 100),
        ];

        // Priority/weighted/latency ignore quota scores: a schedulable reserve
        // provider keeps its configured position.
        let ordered =
            TargetSelector::select_ordered("priority", &targets, &LatencyRegistry::new(), &quota);

        assert_eq!(ordered.len(), 2);
        assert!(ordered.iter().any(|t| t.provider_id == "reserve"));
    }

    #[test]
    fn weekly_perishability_beats_monthly_at_equal_rate() {
        // Two on-pace providers (rate 1.0) used to tie 50/50; the weekly one
        // now wins by exactly the window boost: 2.0702 / 3.0702 = 0.6743.
        let quota = ProviderQuotaRegistry::new();
        let now = chrono::Utc::now();
        let half_week_ahead = (now + chrono::Duration::hours(84)).to_rfc3339();
        let half_month_ahead = (now + chrono::Duration::hours(15 * 24)).to_rfc3339();
        quota.observe(
            "weekly",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 50.0,
                resets_at: Some(half_week_ahead),
            }],
            None,
        );
        quota.observe(
            "monthly",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 50.0,
                resets_at: Some(half_month_ahead),
            }],
            None,
        );
        let targets = vec![
            backend("1", "weekly", "m", 100),
            backend("2", "monthly", "m", 100),
        ];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        let weekly = decision
            .candidates
            .iter()
            .find(|c| c.provider == "weekly")
            .unwrap();
        let monthly = decision
            .candidates
            .iter()
            .find(|c| c.provider == "monthly")
            .unwrap();
        assert_eq!(weekly.score.as_ref().unwrap()["window"], "weekly_limit");
        assert_eq!(weekly.score.as_ref().unwrap()["window_boost"], 2.07);
        assert_eq!(monthly.score.as_ref().unwrap()["window_boost"], 1.0);
        assert!((weekly.share.unwrap() - 0.6743).abs() < 0.005);
        assert!((monthly.share.unwrap() - 0.3257).abs() < 0.005);
    }

    #[test]
    fn same_window_providers_keep_pure_cubed_shares() {
        // The boost cancels within one window class: weekly rates 1.2 vs 0.9
        // keep the pure-cube split 1.728 : 0.729 = 0.7033 : 0.2967.
        let quota = ProviderQuotaRegistry::new();
        let half_week_ahead = (chrono::Utc::now() + chrono::Duration::hours(84)).to_rfc3339();
        for (provider, used) in [("fastish", 40.0), ("slowish", 55.0)] {
            quota.observe(
                provider,
                &[QuotaTierObservation {
                    name: "weekly_limit".to_string(),
                    used_percent: used,
                    resets_at: Some(half_week_ahead.clone()),
                }],
                None,
            );
        }
        let targets = vec![
            backend("1", "fastish", "m", 100),
            backend("2", "slowish", "m", 100),
        ];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        let fastish = decision
            .candidates
            .iter()
            .find(|c| c.provider == "fastish")
            .unwrap();
        let slowish = decision
            .candidates
            .iter()
            .find(|c| c.provider == "slowish")
            .unwrap();
        // Same boost on both sides: shares equal the pure rate³ shares.
        assert!((fastish.share.unwrap() - 0.7033).abs() < 0.002);
        assert!((slowish.share.unwrap() - 0.2967).abs() < 0.002);
    }

    #[test]
    fn production_fixture_weekly_surplus_takes_four_fifths() {
        // Real route_decision from request 44262e73 (2026-08-30): GLM
        // weekly 99.0% quota / 81.3% of the window left vs Volcano monthly
        // 76.8% / 78.9%. Pure rate³ split was 66/34; the perishability
        // boost moves it to ~80/20 (weights 1.8051×2.0702 vs 0.9223).
        let quota = ProviderQuotaRegistry::new();
        let now = chrono::Utc::now();
        // 81.3% of the 7d window and 78.9% of the 30d window remain.
        let weekly_reset = (now + chrono::Duration::milliseconds(491_702_400)).to_rfc3339();
        let monthly_reset = (now + chrono::Duration::milliseconds(2_045_088_000)).to_rfc3339();
        quota.observe(
            "glm",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 1.0,
                resets_at: Some(weekly_reset),
            }],
            None,
        );
        quota.observe(
            "volcano",
            &[QuotaTierObservation {
                name: "monthly".to_string(),
                used_percent: 23.2,
                resets_at: Some(monthly_reset),
            }],
            None,
        );
        let targets = vec![
            backend("1", "glm", "glm-5.3", 100),
            backend("2", "volcano", "glm-5.3", 100),
        ];

        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );

        let glm = decision
            .candidates
            .iter()
            .find(|c| c.provider == "glm")
            .unwrap();
        let volcano = decision
            .candidates
            .iter()
            .find(|c| c.provider == "volcano")
            .unwrap();
        assert_eq!(glm.score.as_ref().unwrap()["window"], "weekly_limit");
        assert_eq!(glm.score.as_ref().unwrap()["window_boost"], 2.07);
        assert_eq!(volcano.score.as_ref().unwrap()["window_boost"], 1.0);
        assert!((glm.share.unwrap() - 0.80).abs() < 0.01);
        assert!((volcano.share.unwrap() - 0.20).abs() < 0.01);
        let ratio = glm.weight.unwrap() / volcano.weight.unwrap();
        assert!((ratio - 4.05).abs() < 0.05, "weight ratio = {ratio}");
    }

    #[test]
    fn scoped_bucket_admission_and_trace_do_not_block_sibling_model() {
        let quota = ProviderQuotaRegistry::new();
        let mut flash = bucket("flash-bucket", &["flash"], "weekly_limit", 20.0);
        flash.tiers.push(ProviderUsageTier {
            name: "five_hour".to_string(),
            used_percent: 100.0,
            resets_at: None,
        });
        quota.observe_buckets(
            "google",
            &[
                flash,
                bucket("argon-bucket", &["argon"], "weekly_limit", 30.0),
            ],
            None,
        );
        let targets = vec![
            backend("1", "google", "flash", 100),
            backend("2", "google", "argon", 100),
        ];

        for balance in ["usage", "weighted", "priority", "latency"] {
            let (ordered, decision) = TargetSelector::select_ordered_traced(
                balance,
                &targets,
                &LatencyRegistry::new(),
                &quota,
                &HealthRegistry::new(),
            );
            if balance == "usage" {
                assert_eq!(ordered_models(&ordered), vec!["argon", "flash"]);
            }
            assert_eq!(
                ordered.len(),
                2,
                "blocked rows remain for dispatcher diagnostics"
            );
            let flash = candidate(&decision, "flash");
            assert_eq!(
                flash.skipped.as_ref().unwrap().window.as_deref(),
                Some("five_hour")
            );
            let evidence = flash.quota.as_ref().unwrap();
            assert_eq!(evidence.bucket_id.as_deref(), Some("flash-bucket"));
            assert_eq!(evidence.blocking_constraint_ids, vec!["flash-bucket"]);
            assert_eq!(evidence.blocker_scope.as_deref(), Some("bucket"));
            assert_eq!(evidence.revision, Some(7));
            assert!(candidate(&decision, "argon").skipped.is_none());
            assert_eq!(
                candidate(&decision, "argon")
                    .quota
                    .as_ref()
                    .unwrap()
                    .bucket_id
                    .as_deref(),
                Some("argon-bucket")
            );
        }
    }

    #[test]
    fn scoped_usage_uses_own_bucket_weights_and_one_max_provider_mass() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe_buckets(
            "a",
            &[
                bucket("fast-bucket", &["fast", "fast-alias"], "weekly_limit", 20.0),
                bucket("slow-bucket", &["slow"], "weekly_limit", 60.0),
            ],
            None,
        );
        quota.observe_buckets(
            "b",
            &[bucket("peer-bucket", &["peer"], "weekly_limit", 20.0)],
            None,
        );
        let targets = vec![
            backend("1", "a", "fast", 90),
            backend("2", "a", "fast-alias", 10),
            backend("3", "a", "slow", 100),
            backend("4", "b", "peer", 100),
        ];
        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        let fast = candidate(&decision, "fast");
        let alias = candidate(&decision, "fast-alias");
        let slow = candidate(&decision, "slow");
        assert_eq!(fast.score.as_ref().unwrap()["remaining_quota_pct"], 80.0);
        assert_eq!(slow.score.as_ref().unwrap()["remaining_quota_pct"], 40.0);
        assert_eq!(
            fast.share,
            Some(0.5),
            "aliases and second bucket add no outer tickets"
        );
        assert_eq!(slow.share, Some(0.5));
        assert_eq!(
            fast.weight, slow.weight,
            "reported outer mass is max, not own or summed mass"
        );
        assert_eq!(fast.score.as_ref().unwrap()["bucket_share"], 0.8889);
        assert_eq!(slow.score.as_ref().unwrap()["bucket_share"], 0.1111);
        assert_eq!(fast.score.as_ref().unwrap()["target_share"], 0.9);
        assert_eq!(alias.score.as_ref().unwrap()["target_share"], 0.1);

        let mut a_first = 0;
        let mut fast_bucket_first = 0;
        let mut fast_row_first = 0;
        let samples = 4_000;
        for _ in 0..samples {
            let ordered =
                TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);
            if ordered[0].provider_id == "a" {
                a_first += 1;
                if ordered[0].model != "slow" {
                    fast_bucket_first += 1;
                    if ordered[0].model == "fast" {
                        fast_row_first += 1;
                    }
                }
            }
        }
        assert!((a_first as f64 / samples as f64 - 0.5).abs() < 0.04);
        assert!((fast_bucket_first as f64 / a_first as f64 - 8.0 / 9.0).abs() < 0.04);
        assert!((fast_row_first as f64 / fast_bucket_first as f64 - 0.9).abs() < 0.04);
    }

    #[test]
    fn unrelated_unknown_and_unrouted_buckets_never_boost_known_mass() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe_buckets(
            "a",
            &[
                bucket("low", &["known"], "weekly_limit", 90.0),
                bucket("unused-healthy", &["not-in-route"], "weekly_limit", 0.0),
            ],
            None,
        );
        quota.observe_buckets(
            "b",
            &[bucket("peer", &["peer"], "weekly_limit", 90.0)],
            None,
        );
        let targets = vec![
            backend("1", "a", "known", 100),
            backend("2", "a", "unmapped", 100),
            backend("3", "b", "peer", 100),
        ];
        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        assert_eq!(ordered.last().unwrap().model, "unmapped");
        assert_eq!(candidate(&decision, "known").share, Some(0.5));
        assert!(candidate(&decision, "known").weight.unwrap() < 0.01);
        assert_eq!(
            candidate(&decision, "unmapped").score.as_ref().unwrap()["state"],
            "unknown_quota"
        );
    }

    #[test]
    fn shared_allowance_caps_pacing_across_different_governing_windows() {
        let quota = ProviderQuotaRegistry::new();
        let mut primary = bucket("primary", &["model"], "weekly_limit", 0.0);
        primary.constraint_ids.push("shared".to_string());
        let mut shared = bucket("shared", &[], "five_hour", 90.0);
        shared.binding.status = "shared_constraint".to_string();
        quota.observe_buckets("a", &[primary, shared], None);
        let targets = vec![backend("1", "a", "model", 100)];
        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        let score = candidate(&decision, "model").score.as_ref().unwrap();
        assert_eq!(score["window"], "five_hour");
        assert_eq!(score["remaining_quota_pct"], 10.0);
        assert!((score["bucket_weight"].as_f64().unwrap() - 0.004).abs() < 1e-9);
    }

    #[test]
    fn raw_window_references_preserve_largest_primary_scoring_window() {
        let quota = ProviderQuotaRegistry::new();
        let mut primary = bucket("primary", &["model"], "weekly_limit", 20.0);
        primary.tiers.push(ProviderUsageTier {
            name: "five_hour".to_string(),
            used_percent: 99.0,
            resets_at: None,
        });
        primary.constraint_ids.push("raw-five-hour".to_string());
        let mut raw = bucket("raw-five-hour", &[], "five_hour", 99.0);
        raw.binding.status = "window_constraint".to_string();
        quota.observe_buckets("a", &[primary, raw], None);
        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &[backend("1", "a", "model", 100)],
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        assert_eq!(
            candidate(&decision, "model").score.as_ref().unwrap()["window"],
            "weekly_limit"
        );
    }

    #[test]
    fn shared_blocker_trace_identifies_constraint_without_blocking_unbound_sibling() {
        let quota = ProviderQuotaRegistry::new();
        let mut primary = bucket("primary", &["model"], "weekly_limit", 20.0);
        primary.constraint_ids.push("shared".to_string());
        let mut shared = bucket("shared", &[], "five_hour", 100.0);
        shared.binding.status = "shared_constraint".to_string();
        quota.observe_buckets(
            "a",
            &[
                primary,
                shared,
                bucket("sibling", &["sibling"], "weekly_limit", 20.0),
            ],
            None,
        );
        let (_, decision) = TargetSelector::select_ordered_traced(
            "priority",
            &[
                backend("1", "a", "model", 100),
                backend("2", "a", "sibling", 100),
            ],
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        let evidence = candidate(&decision, "model").quota.as_ref().unwrap();
        assert_eq!(evidence.blocker_scope.as_deref(), Some("shared_constraint"));
        assert_eq!(evidence.blocking_constraint_ids, vec!["shared"]);
        assert!(candidate(&decision, "sibling").skipped.is_none());
    }

    #[test]
    fn catalog_percentage_does_not_invent_canonical_duration() {
        let quota = ProviderQuotaRegistry::new();
        let mut catalog = bucket("catalog", &["model"], "weekly_limit", 0.0);
        catalog.source = "catalog".to_string();
        quota.observe_buckets("a", &[catalog], None);
        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &[backend("1", "a", "model", 100)],
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        let candidate = candidate(&decision, "model");
        assert_eq!(candidate.score.as_ref().unwrap()["state"], "unknown_quota");
        assert!(candidate.score.as_ref().unwrap().get("rate").is_none());
    }

    #[test]
    fn unknown_required_allowance_does_not_borrow_primary_score() {
        let quota = ProviderQuotaRegistry::new();
        let mut primary = bucket("primary", &["model"], "weekly_limit", 20.0);
        primary.constraint_ids.push("shared".to_string());
        let mut shared = bucket("shared", &[], "catalog_remaining", 20.0);
        shared.binding.status = "shared_constraint".to_string();
        quota.observe_buckets("a", &[primary, shared], None);
        let (_, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &[backend("1", "a", "model", 100)],
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        let candidate = candidate(&decision, "model");
        assert_eq!(candidate.score.as_ref().unwrap()["state"], "unknown_quota");
        assert!(candidate.score.as_ref().unwrap().get("rate").is_none());
        assert!(candidate.skipped.is_none());
    }

    #[test]
    fn exact_model_cooldown_splits_aliases_without_blocking_shared_bucket() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe_buckets(
            "a",
            &[bucket(
                "shared",
                &["flash", "flash-alias"],
                "weekly_limit",
                20.0,
            )],
            None,
        );
        quota.record_model_rejection("a", "flash", Some(30));
        let targets = vec![
            backend("1", "a", "flash", 100),
            backend("2", "a", "flash-alias", 100),
        ];
        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "usage",
            &targets,
            &LatencyRegistry::new(),
            &quota,
            &HealthRegistry::new(),
        );
        assert_eq!(ordered_models(&ordered), vec!["flash-alias", "flash"]);
        assert!(candidate(&decision, "flash-alias").skipped.is_none());
        let flash = candidate(&decision, "flash").quota.as_ref().unwrap();
        assert_eq!(flash.scheduling.reason.as_deref(), Some("model_cooldown"));
        assert_eq!(flash.blocker_scope.as_deref(), Some("model"));
    }

    #[test]
    fn traced_selection_annotates_circuit_open_on_other_strategies() {
        let health = std::sync::Arc::new(HealthRegistry::new());
        for _ in 0..3 {
            health
                .try_acquire("b:openai-compatible/chat-completions/v1:b1")
                .unwrap()
                .failure();
        }
        let targets = vec![
            backend("1", "a", "a1", 50),
            backend("2", "b", "b1", 50),
            backend("3", "b", "healthy-sibling", 50),
        ];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "weighted",
            &targets,
            &LatencyRegistry::new(),
            &ProviderQuotaRegistry::new(),
            &health,
        );

        assert_eq!(ordered.len(), 3);
        assert!(candidate(&decision, "healthy-sibling").skipped.is_none());
        let b = decision
            .candidates
            .iter()
            .find(|c| c.target == "b1")
            .unwrap();
        let skip = b.skipped.as_ref().unwrap();
        assert_eq!(skip.reason, "circuit_open");
        assert!(skip.retry_in_secs.unwrap_or(0) <= 30);
        let a = decision
            .candidates
            .iter()
            .find(|c| c.provider == "a")
            .unwrap();
        assert!(a.skipped.is_none());
    }
}
