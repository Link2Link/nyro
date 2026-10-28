//! Anthropic entitlement refusals.
//!
//! Anthropic answers a request the account is not entitled to run — a model
//! or mode that needs usage credits (extra usage) the account has not enabled
//! — with HTTP 429 `rate_limit_error`, e.g.:
//!
//! ```text
//! {"type":"error","error":{"type":"rate_limit_error",
//!   "message":"Usage credits are required for this model.",
//!   "details":{"error_code":"credits_required","disabled_reason":"org_level_disabled",
//!              "exhausted_included_allowance":false, …}}}
//! ```
//!
//! Despite the status this is not a quota window or a throttled credential:
//! the same request fails the same way on every retry, while the credential
//! stays healthy for everything else. CLIProxyAPI classifies it as
//! request-scoped (`claudeEntitlementError`): no cooldown, no credential
//! penalty. Claude Code calls it `model_requires_usage_credits`.

use serde_json::Value;

/// Why the account cannot run this request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EntitlementRefusal {
    /// Upstream `details.disabled_reason` (e.g. `org_level_disabled`).
    pub disabled_reason: Option<String>,
    /// The included allowance is used up (vs. never covering the model).
    pub exhausted_included_allowance: bool,
}

/// Classify an upstream error body. Only 429s qualify; a genuine rate limit
/// never carries `credits_required` nor mentions usage credits.
pub(crate) fn entitlement_refusal(status: u16, body: &Value) -> Option<EntitlementRefusal> {
    if status != 429 {
        return None;
    }
    let error = body.get("error")?;
    let details = error.get("details");
    let code = details
        .and_then(|d| d.get("error_code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let matches = code == "credits_required"
        || message.contains("usage credits are required")
        || message.contains("credits are required for")
        || message.contains("fast request rejected");
    if !matches {
        return None;
    }
    Some(EntitlementRefusal {
        disabled_reason: details
            .and_then(|d| d.get("disabled_reason"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string),
        exhausted_included_allowance: details
            .and_then(|d| d.get("exhausted_included_allowance"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// [`entitlement_refusal`] over raw bytes.
pub(crate) fn entitlement_refusal_bytes(status: u16, body: &[u8]) -> Option<EntitlementRefusal> {
    if status != 429 {
        return None;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| entitlement_refusal(status, &value))
}

impl EntitlementRefusal {
    /// Operator-facing explanation appended to a probe failure.
    pub(crate) fn hint(&self) -> String {
        let cause = if self.exhausted_included_allowance {
            "the plan's included allowance for it is used up"
        } else {
            "this Claude plan does not include it without usage credits"
        };
        let state = match self.disabled_reason.as_deref() {
            Some("org_level_disabled") => {
                " and usage credits (extra usage) are disabled for the organization"
            }
            Some(_) => " and usage credits (extra usage) are disabled for this account",
            None => " and no usage credits are available",
        };
        format!(
            " — not a quota window: {cause}{state}. Enable extra usage / usage credits in \
             claude.ai settings (an admin must allow it for an organization), or route this \
             model to another provider."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn credits_required() -> Value {
        json!({"type": "error", "error": {
            "type": "rate_limit_error",
            "message": "Usage credits are required for this model.",
            "details": {
                "error_code": "credits_required",
                "can_user_purchase_credits": true,
                "has_chargeable_saved_payment_method": false,
                "disabled_reason": "org_level_disabled",
                "exhausted_included_allowance": false
            }
        }})
    }

    #[test]
    fn recognises_model_and_fast_mode_credit_refusals() {
        let refusal = entitlement_refusal(429, &credits_required()).unwrap();
        assert_eq!(
            refusal.disabled_reason.as_deref(),
            Some("org_level_disabled")
        );
        assert!(!refusal.exhausted_included_allowance);
        assert!(
            refusal
                .hint()
                .contains("does not include it without usage credits")
        );
        assert!(refusal.hint().contains("disabled for the organization"));

        let fast = json!({"type": "error", "error": {"type": "rate_limit_error",
            "message": "Usage credits are required for fast mode."}});
        assert!(entitlement_refusal(429, &fast).is_some());
    }

    #[test]
    fn genuine_rate_limits_and_other_statuses_are_not_refusals() {
        let throttled = json!({"type": "error", "error": {"type": "rate_limit_error",
            "message": "This request would exceed the rate limit for your organization."}});
        assert!(entitlement_refusal(429, &throttled).is_none());
        assert!(entitlement_refusal(400, &credits_required()).is_none());
        assert!(entitlement_refusal_bytes(429, b"not json").is_none());
        let bytes = serde_json::to_vec(&credits_required()).unwrap();
        assert!(entitlement_refusal_bytes(429, &bytes).is_some());
    }
}
