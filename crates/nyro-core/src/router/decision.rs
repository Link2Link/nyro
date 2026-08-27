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
