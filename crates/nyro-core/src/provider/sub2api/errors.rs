//! Sub2API rejection classification.
//!
//! Sub2API rejects a request *before* it reaches any upstream account when
//! the key's billing state or the group's policy does not allow it. These
//! rejections are scoped to this provider (another route target can still
//! serve the request), say nothing about the provider's health, and some of
//! them are fixable configuration mistakes worth a precise hint. Formats
//! differ per route: Anthropic/OpenAI envelopes carry `error.type` /
//! `error.code`, the auth middleware writes a flat `{code, message}`, and
//! Gemini routes write Google envelopes with only a message, so matching
//! looks at both codes and Sub2API's exact message strings.

use serde_json::Value;

use crate::db::models::Provider;

/// What Sub2API refused, decided from the status and the error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    /// Balance, subscription, key quota/expiry or key window limits.
    Billing,
    /// The group only accepts Claude Code clients.
    ClaudeCodeOnly,
    /// An OpenAI group without `/v1/messages` dispatch.
    MessagesDispatchDisabled,
    /// The key's group platform does not serve the requested route.
    PlatformMismatch,
    /// The model is not enabled for the key's group.
    ModelNotInGroup,
}

/// Response extension marking a pre-execution Sub2API rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sub2ApiRejection {
    pub kind: RejectionKind,
}

impl Sub2ApiRejection {
    /// Billing rejections change the key's quota state: re-read `/v1/usage`.
    pub fn refreshes_usage(self) -> bool {
        self.kind == RejectionKind::Billing
    }

    /// Operator-facing explanation, recorded on the request log and appended
    /// to admin probe/connection-test errors.
    pub fn hint(self) -> &'static str {
        match self.kind {
            RejectionKind::Billing => {
                "Sub2API rejected the key for billing (balance, subscription, or key quota/limit). \
                 Check this key's balance, subscription and quota in Sub2API."
            }
            RejectionKind::ClaudeCodeOnly => {
                "This Sub2API group only accepts Claude Code clients. Route other clients to \
                 another provider, or lift the Claude Code restriction on the group in Sub2API."
            }
            RejectionKind::MessagesDispatchDisabled => {
                "This Sub2API OpenAI group does not allow /v1/messages. Uncheck Anthropic Messages \
                 on this provider so Nyro converts Claude requests to Responses, or enable \
                 /v1/messages dispatch for the group in Sub2API."
            }
            RejectionKind::PlatformMismatch => {
                "The Sub2API key's group platform does not serve this route. Check the platform \
                 selected for this provider (Detect platform can suggest it)."
            }
            RejectionKind::ModelNotInGroup => {
                "This model is not enabled for the Sub2API key's group. Use a model from the \
                 group's list, or update the group's model allowlist in Sub2API."
            }
        }
    }
}

/// Error codes Sub2API uses only for its own billing checks.
const BILLING_CODES: &[&str] = &[
    "billing_error",
    "insufficient_quota",
    "insufficient_balance",
    "api_key_quota_exhausted",
    "api_key_expired",
    "subscription_not_found",
    "subscription_invalid",
    "usage_limit_exceeded",
    "daily_limit_exceeded",
    "weekly_limit_exceeded",
    "monthly_limit_exceeded",
    "api_key_rate_5h_exceeded",
    "api_key_rate_1d_exceeded",
    "api_key_rate_7d_exceeded",
    "user_platform_daily_quota_exhausted",
    "user_platform_weekly_quota_exhausted",
    "user_platform_monthly_quota_exhausted",
    "group_rpm_exceeded",
    "user_rpm_exceeded",
];

/// Billing messages, matched when only a message survives (Gemini routes,
/// and `rate_limit_exceeded`, which upstream accounts also use).
const BILLING_MESSAGES: &[&str] = &[
    "insufficient balance",
    "insufficient account balance",
    "api key 额度已用完",
    "api key 已过期",
    "no active subscription found",
    "subscription is invalid or expired",
    "usage limit exceeded",
    "限额已用完",
    "usage quota exhausted for this platform",
    "requests-per-minute limit exceeded",
];

const CLAUDE_CODE_ONLY_MESSAGES: &[&str] = &[
    "restricted to claude code clients",
    "only allows claude code clients",
];

const DISPATCH_DISABLED_MESSAGES: &[&str] = &["does not allow /v1/messages dispatch"];

const PLATFORM_MISMATCH_MESSAGES: &[&str] = &[
    "group platform is not gemini",
    "is not supported for this platform",
    "not supported by composite groups",
    "not supported by this openai-compatible endpoint for composite groups",
];

const MODEL_NOT_IN_GROUP_MESSAGES: &[&str] = &[
    "not in whitelist",
    "is not available for this group",
    "is not supported by any configured account in this group",
];

/// Lower-cased codes and messages found in the error envelopes Sub2API writes.
fn error_texts(body: &Value) -> (Vec<String>, Vec<String>) {
    let lower = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(|text| text.trim().to_ascii_lowercase())
            .filter(|text| !text.is_empty())
    };
    let error = body.get("error");
    let codes = [
        error.and_then(|error| error.get("type")),
        error.and_then(|error| error.get("code")),
        error.and_then(|error| error.get("status")),
        body.get("code"),
    ]
    .into_iter()
    .filter_map(lower)
    .collect();
    // `to_ascii_lowercase` keeps non-ASCII (Chinese) messages intact.
    let messages = [
        error.and_then(|error| error.get("message")),
        body.get("message"),
        error.filter(|error| error.is_string()),
    ]
    .into_iter()
    .filter_map(lower)
    .collect();
    (codes, messages)
}

fn mentions(messages: &[String], needles: &[&str]) -> bool {
    messages
        .iter()
        .any(|message| needles.iter().any(|needle| message.contains(needle)))
}

/// Classify a Sub2API error response; `None` for anything else (upstream
/// account errors, transient outages, malformed requests).
pub fn classify(status: u16, body: &Value) -> Option<Sub2ApiRejection> {
    let (codes, messages) = error_texts(body);
    let kind = if matches!(status, 400 | 402 | 403 | 429)
        && (codes
            .iter()
            .any(|code| BILLING_CODES.contains(&code.as_str()))
            || mentions(&messages, BILLING_MESSAGES))
    {
        RejectionKind::Billing
    } else if status == 403 && mentions(&messages, CLAUDE_CODE_ONLY_MESSAGES) {
        RejectionKind::ClaudeCodeOnly
    } else if status == 403 && mentions(&messages, DISPATCH_DISABLED_MESSAGES) {
        RejectionKind::MessagesDispatchDisabled
    } else if matches!(status, 400 | 403 | 404) && mentions(&messages, PLATFORM_MISMATCH_MESSAGES) {
        RejectionKind::PlatformMismatch
    } else if matches!(status, 400 | 403 | 404) && mentions(&messages, MODEL_NOT_IN_GROUP_MESSAGES)
    {
        RejectionKind::ModelNotInGroup
    } else {
        return None;
    };
    Some(Sub2ApiRejection { kind })
}

/// [`classify`] on a raw body (admin probes keep the text).
pub fn classify_bytes(status: u16, body: &[u8]) -> Option<Sub2ApiRejection> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| classify(status, &body))
}

/// Identity-gated classification: only providers relaying to Sub2API (the
/// preset or the legacy OpenAI-preset channel) are interpreted this way.
pub fn rejection(provider: &Provider, status: u16, body: &Value) -> Option<Sub2ApiRejection> {
    relays_sub2api(provider)
        .then(|| classify(status, body))
        .flatten()
}

/// The Sub2API preset (any channel) or the legacy `sub2api` channel.
pub fn relays_sub2api(provider: &Provider) -> bool {
    super::is_sub2api(provider) || super::Sub2ApiPlatform::of(provider).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(status: u16, body: Value) -> Option<RejectionKind> {
        classify(status, &body).map(|rejection| rejection.kind)
    }

    #[test]
    fn billing_rejections_in_every_envelope() {
        use RejectionKind::Billing;
        for (status, body) in [
            // Anthropic envelope (billingErrorDetails).
            (
                403,
                json!({"type":"error","error":{"type":"billing_error","message":"insufficient balance"}}),
            ),
            // OpenAI chat / responses envelopes.
            (
                403,
                json!({"error":{"type":"billing_error","message":"Billing error"}}),
            ),
            (
                403,
                json!({"error":{"code":"billing_error","message":"Billing error"}}),
            ),
            // Middleware flat envelope.
            (
                403,
                json!({"code":"INSUFFICIENT_BALANCE","message":"Insufficient account balance"}),
            ),
            (
                429,
                json!({"code":"API_KEY_QUOTA_EXHAUSTED","message":"API key 额度已用完"}),
            ),
            (
                403,
                json!({"code":"API_KEY_EXPIRED","message":"API key 已过期"}),
            ),
            (
                403,
                json!({"code":"SUBSCRIPTION_NOT_FOUND","message":"No active subscription found for this group"}),
            ),
            (
                429,
                json!({"code":"USAGE_LIMIT_EXCEEDED","message":"daily usage limit exceeded"}),
            ),
            // OpenAI-compatible key quota.
            (
                429,
                json!({"error":{"message":"API key 额度已用完","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}),
            ),
            // Key windows / platform quota via the generic code: matched by message.
            (
                429,
                json!({"type":"error","error":{"type":"rate_limit_exceeded","message":"api key 5小时限额已用完"}}),
            ),
            (
                429,
                json!({"error":{"type":"rate_limit_exceeded","message":"Daily usage quota exhausted for this platform."}}),
            ),
            // Google envelope on /v1beta: message only.
            (
                403,
                json!({"error":{"code":403,"message":"Insufficient account balance","status":"PERMISSION_DENIED"}}),
            ),
            (
                429,
                json!({"error":{"code":429,"message":"API key 额度已用完","status":"RESOURCE_EXHAUSTED"}}),
            ),
        ] {
            assert_eq!(kind(status, body.clone()), Some(Billing), "{status} {body}");
        }
    }

    #[test]
    fn upstream_and_transient_errors_are_not_rejections() {
        for (status, body) in [
            // An upstream OpenAI account throttle passed through.
            (
                429,
                json!({"error":{"type":"requests","code":"rate_limit_exceeded","message":"Rate limit reached for gpt-5"}}),
            ),
            (
                429,
                json!({"type":"error","error":{"type":"rate_limit_error","message":"Upstream rate limit exceeded, please retry later"}}),
            ),
            (
                503,
                json!({"type":"error","error":{"type":"billing_service_error","message":"Billing service temporarily unavailable. Please retry later."}}),
            ),
            (
                503,
                json!({"error":{"type":"api_error","message":"No available accounts"}}),
            ),
            (
                401,
                json!({"code":"INVALID_API_KEY","message":"Invalid API key"}),
            ),
            (
                400,
                json!({"error":{"type":"invalid_request_error","message":"messages: field required"}}),
            ),
            (500, json!("plain text")),
        ] {
            assert_eq!(kind(status, body.clone()), None, "{status} {body}");
        }
    }

    #[test]
    fn configuration_rejections_get_specific_kinds() {
        use RejectionKind::*;
        for (status, body, expected) in [
            (
                403,
                json!({"error":{"type":"permission_error","message":"This group is restricted to Claude Code clients (/v1/messages only)"}}),
                ClaudeCodeOnly,
            ),
            (
                403,
                json!({"type":"error","error":{"type":"permission_error","message":"this group only allows Claude Code clients"}}),
                ClaudeCodeOnly,
            ),
            (
                403,
                json!({"type":"error","error":{"type":"permission_error","message":"This group does not allow /v1/messages dispatch"}}),
                MessagesDispatchDisabled,
            ),
            (
                400,
                json!({"error":{"code":400,"message":"API key group platform is not gemini","status":"INVALID_ARGUMENT"}}),
                PlatformMismatch,
            ),
            (
                404,
                json!({"error":{"type":"not_found_error","message":"Embeddings API is not supported for this platform"}}),
                PlatformMismatch,
            ),
            (
                404,
                json!({"error":{"type":"not_found_error","message":"Model \"claude-x\" is not available for this group"}}),
                ModelNotInGroup,
            ),
            (
                403,
                json!({"type":"error","error":{"type":"permission_error","message":"model claude-x not in whitelist"}}),
                ModelNotInGroup,
            ),
        ] {
            assert_eq!(kind(status, body.clone()), Some(expected), "{body}");
        }
        assert_eq!(
            classify_bytes(
                403,
                br#"{"error":{"message":"this group only allows Claude Code clients"}}"#
            )
            .map(|rejection| rejection.kind),
            Some(ClaudeCodeOnly)
        );
        assert!(classify_bytes(403, b"not json").is_none());
    }

    #[test]
    fn only_billing_refreshes_usage_and_every_kind_has_a_hint() {
        use RejectionKind::*;
        for kind in [
            Billing,
            ClaudeCodeOnly,
            MessagesDispatchDisabled,
            PlatformMismatch,
            ModelNotInGroup,
        ] {
            let rejection = Sub2ApiRejection { kind };
            assert_eq!(rejection.refreshes_usage(), kind == Billing);
            assert!(
                rejection.hint().starts_with("This")
                    || rejection.hint().starts_with("Sub2API")
                    || rejection.hint().starts_with("The")
            );
        }
    }
}
