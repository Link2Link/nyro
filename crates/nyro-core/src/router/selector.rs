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
//! let ordered =
//!     TargetSelector::select_ordered(&route.balance, &targets, &gw.latency_registry);
//! ```

use std::collections::BTreeMap;
use std::str::FromStr;

use rand::Rng;

use super::latency::LatencyRegistry;
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
    ) -> Vec<SelectedTarget>;
}

// ── Weighted ──────────────────────────────────────────────────────────────────

pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select_ordered(
        &self,
        targets: &[ModelBackend],
        _latency: &LatencyRegistry,
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

// ── TargetSelector (public entry point) ───────────────────────────────────────

pub struct TargetSelector;

impl TargetSelector {
    /// Return targets ordered by the named balance. Unrecognised balance
    /// strings fall back to `weighted`.
    pub fn select_ordered(
        balance: &str,
        targets: &[ModelBackend],
        latency: &LatencyRegistry,
    ) -> Vec<SelectedTarget> {
        match ModelBalance::from_str(balance).unwrap_or_default() {
            ModelBalance::Weighted => WeightedStrategy.select_ordered(targets, latency),
            ModelBalance::Priority => PriorityStrategy.select_ordered(targets, latency),
            ModelBalance::Latency => LatencyStrategy.select_ordered(targets, latency),
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
    if targets.is_empty() {
        return vec![];
    }
    let mut rng = rand::thread_rng();
    let mut items: Vec<(&ModelBackend, f64)> = targets
        .iter()
        .map(|t| {
            let weight = t.weight.max(1) as f64;
            let key = rng.r#gen::<f64>().powf(1.0 / weight);
            (*t, key)
        })
        .collect();
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    items.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);

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

        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);

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

        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);

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

        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);

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

        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);

        // 2/3 samples: still unknown → probes first despite the lower weight.
        assert_eq!(
            ordered_models(&ordered),
            vec!["fast", "slow"],
            "collecting targets stay in the probe group"
        );

        // Third sample graduates the round; its mean wins on merit.
        registry.record("p-fast", "fast", 100);
        let ordered = TargetSelector::select_ordered("latency", &targets, &registry);
        assert_eq!(
            ordered_models(&ordered),
            vec!["fast", "slow"],
            "graduated mean 100 beats 900"
        );
    }

    #[test]
    fn unknown_balance_falls_back_to_weighted() {
        let registry = LatencyRegistry::new();
        let targets = vec![backend("1", "p", "only", 100)];

        let ordered = TargetSelector::select_ordered("nonsense", &targets, &registry);

        assert_eq!(ordered_models(&ordered), vec!["only"]);
    }
}
