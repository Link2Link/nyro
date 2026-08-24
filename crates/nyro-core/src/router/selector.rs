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
//!
//! # Usage
//!
//! ```rust,ignore
//! // Dispatcher
//! let ordered = TargetSelector::select_ordered(&route.balance, &targets);
//! ```

use std::collections::BTreeMap;
use std::str::FromStr;

use rand::Rng;

use crate::db::models::{ModelBackend, ModelBalance};

// ── SelectedTarget ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub provider_id: String,
    pub model: String,
}

// ── RoutingStrategy trait ─────────────────────────────────────────────────────

/// Produces an ordered list of targets to try, from most to least preferred.
pub trait RoutingStrategy: Send + Sync {
    fn select_ordered(&self, targets: &[ModelBackend]) -> Vec<SelectedTarget>;
}

// ── Weighted ──────────────────────────────────────────────────────────────────

pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select_ordered(&self, targets: &[ModelBackend]) -> Vec<SelectedTarget> {
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
    fn select_ordered(&self, targets: &[ModelBackend]) -> Vec<SelectedTarget> {
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

// ── TargetSelector (public entry point) ───────────────────────────────────────

pub struct TargetSelector;

impl TargetSelector {
    /// Return targets ordered by the named balance. Unrecognised balance
    /// strings fall back to `weighted`.
    pub fn select_ordered(balance: &str, targets: &[ModelBackend]) -> Vec<SelectedTarget> {
        match ModelBalance::from_str(balance).unwrap_or_default() {
            ModelBalance::Weighted => WeightedStrategy.select_ordered(targets),
            ModelBalance::Priority => PriorityStrategy.select_ordered(targets),
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
