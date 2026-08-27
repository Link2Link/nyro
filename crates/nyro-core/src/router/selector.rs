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
//! | `usage`    | Largest-window quota rate³ weights     |
//!
//! `latency` orders targets by the time-to-first-token EWMA held in
//! [`LatencyRegistry`]. Targets without a graduated fresh estimate (never
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

use super::decision::{DecisionCandidate, RouteDecision, SkipReason};
use super::health::HealthRegistry;
use super::latency::LatencyRegistry;
use super::quota::ProviderQuotaRegistry;
use super::usage::{dynamic_weight, score_provider};
use crate::db::models::{ModelBackend, ModelBalance};

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
                target: target.model.clone(),
                rank: None,
                weight: None,
                share: None,
                score: Some(serde_json::json!({ "static_weight": target.weight })),
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
                target: target.model.clone(),
                rank: Some(index + 1),
                weight: Some(weight),
                share: if total > 0.0 {
                    Some(weight / total)
                } else {
                    None
                },
                score: Some(serde_json::json!({ "static_weight": target.weight })),
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
                    target: target.model.clone(),
                    rank: Some(rank),
                    weight: None,
                    share: None,
                    score: Some(serde_json::json!({
                        "group": group_priority,
                        "in_group_rank": in_group + 1,
                    })),
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
                target: target.model.clone(),
                rank: Some(index + 1),
                weight: None,
                share: None,
                score: Some(score),
                skipped: None,
            });
        }
        ordered.into_iter().map(to_selected).collect()
    }
}

// ── Usage ─────────────────────────────────────────────────────────────────────

/// Orders provider groups by upstream quota headroom. Every scored provider
/// competes in a single pool whose weight is the cube of the required
/// acceleration on its largest main window (monthly, else weekly, else
/// five-hour): rate = remaining quota % / remaining time % of that window.
/// Providers without a canonical window form an equal-weight fallback pool;
/// quota-blocked rows stay last so the dispatcher keeps its exhausted 503.
pub struct UsageStrategy;

struct ProviderTargetGroup<'a> {
    provider_id: &'a str,
    targets: Vec<&'a ModelBackend>,
    score: Option<super::usage::ProviderUsageScore>,
    schedulable: bool,
}

impl RoutingStrategy for UsageStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        _latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
        decision: &mut RouteDecision,
    ) -> Vec<SelectedTarget> {
        // Match weighted balance semantics: zero-weight rows do not participate.
        for target in targets.iter().filter(|target| target.weight <= 0) {
            decision.candidates.push(DecisionCandidate {
                provider: target.provider_id.clone(),
                target: target.model.clone(),
                rank: None,
                weight: None,
                share: None,
                score: None,
                skipped: Some(SkipReason::weight_zero()),
            });
        }

        let mut indexes: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<ProviderTargetGroup<'_>> = Vec::new();
        for target in targets.iter().filter(|target| target.weight > 0) {
            if let Some(index) = indexes.get(target.provider_id.as_str()).copied() {
                groups[index].targets.push(target);
                continue;
            }
            let index = groups.len();
            indexes.insert(target.provider_id.as_str(), index);
            groups.push(ProviderTargetGroup {
                provider_id: target.provider_id.as_str(),
                targets: vec![target],
                score: None,
                schedulable: false,
            });
        }

        let now = chrono::Utc::now();
        for group in &mut groups {
            group.schedulable = quota.is_schedulable(group.provider_id);
            group.score = score_provider(&quota.tier_snapshot(group.provider_id), now);
        }

        let mut scored: Vec<(&ProviderTargetGroup<'_>, f64)> = Vec::new();
        // Unknown and zero-rate providers get one equal provider-level fallback
        // mass; static target weights only choose within that provider. Duplicate
        // rows therefore never amplify a provider's cross-provider probability.
        let mut fallback: Vec<(&ProviderTargetGroup<'_>, f64)> = Vec::new();
        let mut inactive: Vec<(
            &ProviderTargetGroup<'_>,
            Option<super::usage::ProviderUsageScore>,
        )> = Vec::new();

        for group in &groups {
            if !group.schedulable {
                inactive.push((group, group.score));
                continue;
            }
            match group.score {
                Some(score) if score.rate.is_finite() && score.rate > 0.0 => {
                    scored.push((group, dynamic_weight(score.rate)))
                }
                Some(_) | None => fallback.push((group, 1.0)),
            }
        }

        let scored_total: f64 = scored.iter().map(|(_, w)| *w).sum();
        let fallback_total: f64 = fallback.iter().map(|(_, w)| *w).sum();

        let mut ordered: Vec<&ModelBackend> = Vec::new();
        let mut rank = 0_usize;
        // Scored pool first, shuffled by rate³.
        for (group, weight) in weighted_shuffle_pairs_by(&scored) {
            if let Some(score) = group.score {
                for target in weighted_shuffle(&group.targets) {
                    rank += 1;
                    decision.candidates.push(usage_candidate(
                        target,
                        Some(score),
                        Some(weight),
                        share_of(weight, scored_total),
                        Some(rank),
                        None,
                    ));
                    ordered.push(target);
                }
            }
        }
        // Equal-weight fallback pool (no usable quota window).
        for (group, weight) in weighted_shuffle_pairs_by(&fallback) {
            for target in weighted_shuffle(&group.targets) {
                rank += 1;
                decision.candidates.push(DecisionCandidate {
                    provider: target.provider_id.clone(),
                    target: target.model.clone(),
                    rank: Some(rank),
                    weight: Some(weight),
                    share: share_of(weight, fallback_total),
                    score: Some(serde_json::json!({ "state": "unknown_quota" })),
                    skipped: None,
                });
                ordered.push(target);
            }
        }
        // Keep quota-blocked rows in the retry list so the dispatcher preserves
        // its precise "all providers exhausted" 503 outcome.
        for (group, score) in inactive {
            let scheduling = quota.snapshot(group.provider_id);
            let skipped =
                SkipReason::quota(&scheduling.blocking_tiers, scheduling.reason.as_deref());
            for target in weighted_shuffle(&group.targets) {
                rank += 1;
                decision.candidates.push(usage_candidate(
                    target,
                    score,
                    None,
                    None,
                    Some(rank),
                    Some(skipped.clone()),
                ));
                ordered.push(target);
            }
        }

        ordered.into_iter().map(to_selected).collect()
    }
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
            "remaining_quota_pct": (s.remaining_quota * 10.0).round() / 10.0,
        });
        if let Some(remaining_time) = s.remaining_time {
            value["remaining_time_pct"] = serde_json::json!((remaining_time * 10.0).round() / 10.0);
        }
        value
    });
    DecisionCandidate {
        provider: target.provider_id.clone(),
        target: target.model.clone(),
        rank,
        weight,
        share,
        score: score_json,
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
        let ordered = match resolved {
            ModelBalance::Weighted => {
                WeightedStrategy.select_ordered(targets, latency, quota, &mut decision)
            }
            ModelBalance::Priority => {
                PriorityStrategy.select_ordered(targets, latency, quota, &mut decision)
            }
            ModelBalance::Latency => {
                LatencyStrategy.select_ordered(targets, latency, quota, &mut decision)
            }
            ModelBalance::Usage => {
                UsageStrategy.select_ordered(targets, latency, quota, &mut decision)
            }
        };
        for candidate in &mut decision.candidates {
            if candidate.skipped.is_some() || candidate.rank.is_none() {
                continue;
            }
            if !quota.is_schedulable(&candidate.provider) {
                let scheduling = quota.snapshot(&candidate.provider);
                candidate.skipped = Some(SkipReason::quota(
                    &scheduling.blocking_tiers,
                    scheduling.reason.as_deref(),
                ));
            } else if let Some(secs) = health.open_provider_secs(&candidate.provider) {
                candidate.skipped = Some(SkipReason::circuit(secs));
            }
        }
        (ordered, decision)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
    use crate::router::quota::QuotaTierObservation;
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
            created_at: String::new(),
        }
    }

    fn ordered_models(ordered: &[SelectedTarget]) -> Vec<String> {
        ordered.iter().map(|t| t.model.clone()).collect()
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

    #[test]
    fn traced_selection_annotates_circuit_open_on_other_strategies() {
        let health = std::sync::Arc::new(HealthRegistry::new());
        for _ in 0..3 {
            health
                .try_acquire("b:openai-compatible/chat-completions/v1:m")
                .unwrap()
                .failure();
        }
        let targets = vec![backend("1", "a", "a1", 50), backend("2", "b", "b1", 50)];

        let (ordered, decision) = TargetSelector::select_ordered_traced(
            "weighted",
            &targets,
            &LatencyRegistry::new(),
            &ProviderQuotaRegistry::new(),
            &health,
        );

        assert_eq!(ordered.len(), 2);
        let b = decision
            .candidates
            .iter()
            .find(|c| c.provider == "b")
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
