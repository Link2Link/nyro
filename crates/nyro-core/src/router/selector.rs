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
//! | `usage`    | Provider quota score² dynamic weights   |
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

use super::latency::LatencyRegistry;
use super::quota::ProviderQuotaRegistry;
use super::usage::{UsageScorePool, dynamic_weight, score_provider};
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
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
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
    ) -> Vec<SelectedTarget> {
        let refs: Vec<&ModelBackend> = targets.iter().filter(|t| t.weight > 0).collect();
        weighted_shuffle(&refs)
            .into_iter()
            .map(to_selected)
            .collect()
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
    ) -> Vec<SelectedTarget> {
        let mut groups: BTreeMap<i32, Vec<&ModelBackend>> = BTreeMap::new();
        for t in targets {
            groups.entry(t.priority).or_default().push(t);
        }
        groups
            .into_values()
            .flat_map(|group| group.into_iter().map(to_selected))
            .collect()
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
        unknown
            .into_iter()
            .chain(known.into_iter().map(|(t, _)| t))
            .map(to_selected)
            .collect()
    }
}

// ── Usage ─────────────────────────────────────────────────────────────────────

/// Orders provider groups from upstream quota headroom. Providers carrying only
/// a five-hour window form the highest-priority pool. Once weekly or monthly
/// quota exists, five-hour usage is ignored and the long-window bottleneck
/// score is squared into the provider's dynamic randomization weight.
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
    ) -> Vec<SelectedTarget> {
        let mut indexes: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<ProviderTargetGroup<'_>> = Vec::new();
        // Match weighted balance semantics: zero-weight rows do not participate.
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

        let mut five_hour: Vec<(&ProviderTargetGroup<'_>, f64)> = Vec::new();
        let mut long_term: Vec<(&ProviderTargetGroup<'_>, f64)> = Vec::new();
        // Unknown and zero-score providers get one equal provider-level fallback
        // mass; static target weights only choose within that provider. Duplicate
        // rows therefore never amplify a provider's cross-provider probability.
        let mut fallback: Vec<(&ProviderTargetGroup<'_>, f64)> = Vec::new();
        let mut inactive: Vec<&ModelBackend> = Vec::new();

        for group in &groups {
            if !group.schedulable {
                inactive.extend(group.targets.iter().copied());
                continue;
            }
            match group.score {
                Some(score) if score.score.is_finite() && score.score > 0.0 => {
                    let weight = dynamic_weight(score.score);
                    match score.pool {
                        UsageScorePool::FiveHourOnly => five_hour.push((group, weight)),
                        UsageScorePool::LongTerm => long_term.push((group, weight)),
                    }
                }
                Some(_) | None => fallback.push((group, 1.0)),
            }
        }

        let mut ordered: Vec<&ModelBackend> = Vec::new();
        append_provider_groups(&mut ordered, weighted_shuffle_by(&five_hour));
        append_provider_groups(&mut ordered, weighted_shuffle_by(&long_term));
        append_provider_groups(&mut ordered, weighted_shuffle_by(&fallback));
        // Keep quota-blocked rows in the retry list so the dispatcher preserves
        // its precise "all providers exhausted" 503 outcome.
        ordered.extend(inactive);

        ordered.into_iter().map(to_selected).collect()
    }
}

fn append_provider_groups<'a>(
    ordered: &mut Vec<&'a ModelBackend>,
    groups: Vec<&ProviderTargetGroup<'a>>,
) {
    for group in groups {
        ordered.extend(weighted_shuffle(&group.targets));
    }
}

// ── TargetSelector (public entry point) ───────────────────────────────────────

pub struct TargetSelector;

impl TargetSelector {
    /// Return targets ordered by the named balance. Unrecognised balance
    /// strings fall back to `weighted`.
    pub fn select_ordered(
        balance: &str,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
        quota: &ProviderQuotaRegistry,
    ) -> Vec<SelectedTarget> {
        match ModelBalance::from_str(balance).unwrap_or_default() {
            ModelBalance::Weighted => WeightedStrategy.select_ordered(targets, latency, quota),
            ModelBalance::Priority => PriorityStrategy.select_ordered(targets, latency, quota),
            ModelBalance::Latency => LatencyStrategy.select_ordered(targets, latency, quota),
            ModelBalance::Usage => UsageStrategy.select_ordered(targets, latency, quota),
        }
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
    fn squared_scores_drive_weighted_first_choice_probability() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let items = [("a", dynamic_weight(70.0)), ("b", dynamic_weight(50.0))];
        let mut a_first = 0_u32;
        let samples = 20_000_u32;
        for _ in 0..samples {
            let ordered = weighted_shuffle_by_rng(&items, &mut rng);
            if ordered.first() == Some(&"a") {
                a_first += 1;
            }
        }
        let observed = f64::from(a_first) / f64::from(samples);
        let expected = 4900.0 / (4900.0 + 2500.0);

        assert!(
            (observed - expected).abs() < 0.02,
            "{observed} vs {expected}"
        );
    }

    #[test]
    fn usage_prioritizes_five_hour_only_then_long_term_then_unknown() {
        let quota = ProviderQuotaRegistry::new();
        quota.observe(
            "five",
            &[QuotaTierObservation {
                name: "five_hour".to_string(),
                used_percent: 30.0,
                resets_at: None,
            }],
            None,
        );
        quota.observe(
            "long",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 10.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "long", "long", 100),
            backend("2", "unknown", "unknown", 100),
            backend("3", "five", "five", 100),
        ];

        let ordered =
            TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);

        assert_eq!(ordered_models(&ordered), vec!["five", "long", "unknown"]);
    }

    #[test]
    fn every_eligible_five_hour_only_provider_stays_ahead_of_long_term_pool() {
        let quota = ProviderQuotaRegistry::new();
        for provider in ["five-a", "five-b"] {
            quota.observe(
                provider,
                &[QuotaTierObservation {
                    name: "five_hour".to_string(),
                    used_percent: 25.0,
                    resets_at: None,
                }],
                None,
            );
        }
        quota.observe(
            "long",
            &[QuotaTierObservation {
                name: "weekly_limit".to_string(),
                used_percent: 1.0,
                resets_at: None,
            }],
            None,
        );
        let targets = vec![
            backend("1", "long", "long", 100),
            backend("2", "five-a", "five-a", 100),
            backend("3", "five-b", "five-b", 100),
        ];

        let ordered =
            TargetSelector::select_ordered("usage", &targets, &LatencyRegistry::new(), &quota);
        let first_pool = ordered[..2]
            .iter()
            .map(|target| target.provider_id.as_str())
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(
            first_pool,
            std::collections::HashSet::from(["five-a", "five-b"])
        );
        assert_eq!(ordered[2].provider_id, "long");
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
}
