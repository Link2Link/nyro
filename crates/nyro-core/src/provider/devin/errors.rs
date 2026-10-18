//! Extract allowlisted scheduling facts before throwing away opaque diagnostics.
use crate::error::{AccountFailure, AccountFailureReason as Reason, GatewayError};

pub(super) fn failure(
    status: u16,
    message: &str,
    explicit_refusal: bool,
    retry_after_seconds: Option<u64>,
) -> GatewayError {
    let text = message.to_ascii_lowercase();
    // Narrow quota matching: capacity, ordinary throttling and monthly credits
    // must not put a whole subscriber into a daily/weekly hard block.
    let quota_exhausted =
        text.contains("quota") && (text.contains("exhausted") || text.contains("exceeded"));
    let reason = if status == 401 {
        Reason::AuthInvalid
    } else if status == 429 && quota_exhausted && text.contains("weekly") {
        Reason::WeeklyQuota
    } else if status == 429 && quota_exhausted && text.contains("daily") {
        Reason::DailyQuota
    } else if (status == 429 || status == 503)
        && (text.contains("high demand") || text.contains("capacity"))
    {
        Reason::Capacity
    } else if status == 429 {
        Reason::RateLimited
    } else if status == 403 {
        Reason::ModelForbidden
    } else if status == 400 {
        Reason::RequestInvalid
    } else {
        Reason::Other
    };
    GatewayError::AccountUpstream {
        status,
        failure: AccountFailure {
            reason,
            rejected_before_execution: explicit_refusal
                && matches!(
                    reason,
                    Reason::AuthInvalid
                        | Reason::DailyQuota
                        | Reason::WeeklyQuota
                        | Reason::RateLimited
                        | Reason::ModelForbidden
                        | Reason::Capacity
                ),
            retry_after_seconds,
        },
    }
}

/// A status alone is not replay evidence for capacity/permission failures.
/// Accept the protocol's structured refusal envelope, not arbitrary proxy text.
pub(super) fn http_refusal(status: u16, body: &[u8]) -> bool {
    if status == 401 {
        return true;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let error = value.get("error").unwrap_or(&value);
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        (status, code),
        (403, "permission_denied") | (429, "resource_exhausted")
    ) || status == 429
        && code == "failed_precondition"
        && message.contains("quota")
        && (message.contains("exhausted") || message.contains("exceeded"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_scope_is_narrow_and_raw_secrets_are_not_retained() {
        let err = failure(429, "Weekly quota exhausted secret-fixture", true, None);
        assert_eq!(err.account_failure().unwrap().reason, Reason::WeeklyQuota);
        assert!(!err.to_string().contains("secret-fixture"));
        assert_eq!(
            failure(429, "high demand", true, None)
                .account_failure()
                .unwrap()
                .reason,
            Reason::Capacity
        );
        assert_eq!(
            failure(429, "monthly ACU exhausted", true, None)
                .account_failure()
                .unwrap()
                .reason,
            Reason::RateLimited
        );
    }
    #[test]
    fn bare_permission_and_rate_status_are_not_replay_evidence() {
        assert!(!http_refusal(429, b""));
        assert!(!http_refusal(403, b"proxy error"));
        assert!(!http_refusal(429, br#"{"message":"rate limit"}"#));
        assert!(http_refusal(
            429,
            br#"{"code":"resource_exhausted","message":"daily quota exhausted"}"#
        ));
        assert!(http_refusal(
            403,
            br#"{"error":{"code":"permission_denied"}}"#
        ));
    }
    #[test]
    fn post_execution_error_never_proves_safe_replay() {
        assert!(
            !failure(401, "unauthenticated", false, None)
                .account_failure()
                .unwrap()
                .rejected_before_execution
        );
        assert!(
            !failure(503, "server failure", true, None)
                .account_failure()
                .unwrap()
                .rejected_before_execution
        );
    }
}
