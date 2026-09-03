//! Route-decision snapshot persisted on every request log row.
//!
//! Captured at target-selection time so the recorded candidates, scores, and
//! skip reasons are exactly what the strategy saw when it ordered the
//! attempts. All four balances share one JSON skeleton; the `score` object
//! carries strategy-specific keys (`rate`/`window` for usage, `ttft_ms`
//! for latency, `static_weight` for weighted, `group` for priority).

use serde::Serialize;

/// Why a configured candidate did not (or would not) receive traffic.
pub(crate) const SKIP_WEIGHT_ZERO: &str = "weight_zero";
pub(crate) const SKIP_QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub(crate) const SKIP_PROVIDER_DISABLED: &str = "provider_disabled";
pub(crate) const SKIP_CIRCUIT_OPEN: &str = "circuit_open";

#[derive(Debug, Clone, Serialize)]
pub struct RouteDecision {
    /// Balance strategy that produced this ordering (`weighted` /
    /// `priority` / `latency` / `usage`).
    pub balance: String,
    /// Every configured candidate row, including skipped ones.
    pub candidates: Vec<DecisionCandidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionCandidate {
    pub provider: String,
    /// Human-readable provider display name, resolved by the dispatcher right
    /// after selection so the persisted snapshot reads without a provider
    /// join. Absent when the provider row could not be loaded (the WebUI then
    /// maps the id back to a name as a fallback for such older rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Declared backend model of the target row.
    pub target: String,
    /// Position in the attempt order (1 = tried first). `None` when the
    /// strategy excluded the row entirely (see `skipped`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    /// Randomization weight the strategy assigned (provider-level for
    /// `usage`). `None` when the strategy does not randomize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// First-choice probability share derived from the weights.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
    /// Strategy-specific scoring detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<SkipReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkipReason {
    /// One of the stable skip codes (`weight_zero`, `quota_exhausted`,
    /// `provider_disabled`, `circuit_open`).
    pub reason: String,
    /// `quota_exhausted`: the blocking window(s), or `account_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// `circuit_open`: seconds until the circuit probes for recovery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in_secs: Option<i64>,
}

impl RouteDecision {
    pub fn new(balance: &str) -> Self {
        Self {
            balance: balance.to_string(),
            candidates: Vec::new(),
        }
    }

    /// Compact JSON serialization for the `request_logs.route_decision`
    /// column. Never fails: the snapshot is plain data.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| format!("{{\"balance\":\"{}\",\"candidates\":[]}}", self.balance))
    }

    /// Stamp the display name onto every candidate of one provider.
    pub fn set_provider_name(&mut self, provider_id: &str, name: &str) {
        for candidate in &mut self.candidates {
            if candidate.provider == provider_id {
                candidate.provider_name = Some(name.to_string());
            }
        }
    }

    /// Distinct provider ids whose candidates still lack a display name, in
    /// snapshot order. Owned strings so callers can mutate the decision while
    /// resolving each id.
    pub fn unnamed_provider_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.candidates
            .iter()
            .filter(|candidate| candidate.provider_name.is_none())
            .map(|candidate| candidate.provider.clone())
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    /// Mark every not-yet-annotated candidate of one provider as disabled.
    /// Used by the dispatcher when provider resolution fails mid-failover.
    pub fn mark_provider_disabled(&mut self, provider_id: &str) {
        for candidate in &mut self.candidates {
            if candidate.provider == provider_id && candidate.skipped.is_none() {
                candidate.skipped = Some(SkipReason {
                    reason: SKIP_PROVIDER_DISABLED.to_string(),
                    window: None,
                    retry_in_secs: None,
                });
            }
        }
    }
}

impl SkipReason {
    pub(crate) fn weight_zero() -> Self {
        Self {
            reason: SKIP_WEIGHT_ZERO.to_string(),
            window: None,
            retry_in_secs: None,
        }
    }

    pub(crate) fn circuit(retry_in_secs: i64) -> Self {
        Self {
            reason: SKIP_CIRCUIT_OPEN.to_string(),
            window: None,
            retry_in_secs: Some(retry_in_secs),
        }
    }

    /// Derive the quota skip detail from a scheduling snapshot: the blocking
    /// windows, or the account-unavailable marker when that is the cause.
    pub(crate) fn quota(blocking_tiers: &[String], reason: Option<&str>) -> Self {
        let window = if reason == Some("account_unavailable") {
            Some("account_unavailable".to_string())
        } else if blocking_tiers.is_empty() {
            None
        } else {
            Some(blocking_tiers.join(","))
        };
        Self {
            reason: SKIP_QUOTA_EXHAUSTED.to_string(),
            window,
            retry_in_secs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(provider: &str, target: &str, rank: Option<usize>) -> DecisionCandidate {
        DecisionCandidate {
            provider: provider.to_string(),
            provider_name: None,
            target: target.to_string(),
            rank,
            weight: None,
            share: None,
            score: None,
            skipped: None,
        }
    }

    #[test]
    fn set_provider_name_stamps_every_candidate_of_that_provider() {
        let mut decision = RouteDecision::new("weighted");
        decision.candidates.push(candidate("p-1", "m-a", Some(1)));
        decision.candidates.push(candidate("p-1", "m-b", Some(2)));
        decision.candidates.push(candidate("p-2", "m-a", None));

        assert_eq!(decision.unnamed_provider_ids(), vec!["p-1", "p-2"]);
        decision.set_provider_name("p-1", "GLM Pro");
        assert_eq!(decision.unnamed_provider_ids(), vec!["p-2"]);

        let json = decision.to_json();
        // Both p-1 rows carry the name; p-2 omits the field entirely.
        assert_eq!(json.matches("GLM Pro").count(), 2);
        assert!(!json.contains("\"provider_name\":null"));
    }
}
