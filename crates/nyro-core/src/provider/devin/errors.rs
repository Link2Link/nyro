//! Keep scheduling facts separate from bounded, admin-only upstream diagnostics.
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
        provider: "Devin",
        status,
        message: None,
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

/// A diagnostic copy, never a wire body or a client-facing error. Unknown fields
/// (including Connect details/metadata) are deliberately excluded. The actual
/// credential is checked before truncation, even when echoed without a label.
pub(super) fn diagnostic(
    value: &serde_json::Value,
    source: &str,
    token: &str,
) -> Option<serde_json::Value> {
    let error = value.get("error").unwrap_or(value);
    let code = error.get("code").and_then(serde_json::Value::as_str);
    let message = error.get("message").and_then(serde_json::Value::as_str);
    if code.is_none() && message.is_none() {
        return None;
    }
    let sanitize = |text: &str| {
        // Refuse oversized opaque diagnostics before allocating normalization
        // buffers. Connect permits much larger frames than our logging budget.
        if text.len() > 16 * 1024 {
            return "Oversized diagnostic text omitted".to_string();
        }
        let lower = text.to_ascii_lowercase();
        if (!token.is_empty() && text.contains(token))
            || [
                "basic ",
                "api-key",
                "api key",
                "private key",
                "://",
                "cookie",
                "secret",
                "token",
                "credential",
                "signature",
                "session",
                "sk-",
                "eyj",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
        {
            "Sensitive diagnostic text omitted".to_string()
        } else {
            crate::logging::diagnostics::sanitize_cause(text)
        }
    };
    // Codes are expected to be a short machine identifier. Do not permit an
    // opaque upstream string to bypass message redaction through this field.
    let code = code.map(|code| {
        if code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            sanitize(code)
        } else {
            "Unrecognized upstream code omitted".into()
        }
    });
    Some(serde_json::json!({
        "source": source,
        "code": code,
        "message": message.map(sanitize),
        "sanitized": true,
    }))
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
    fn diagnostics_keep_only_bounded_sanitized_code_and_message() {
        let value = serde_json::json!({"error": {
            "code": "permission_denied",
            "message": "Tool web_search is not available for this request",
            "details": [{"token": "private-value"}], "metadata": {"secret": "private-value"}
        }});
        let safe = diagnostic(&value, "connect_trailer", "private-value").unwrap();
        assert_eq!(safe["code"], "permission_denied");
        assert_eq!(
            safe["message"],
            "Tool web_search is not available for this request"
        );
        assert!(!safe.to_string().contains("private-value"));
        for message in [
            "Bearer abc",
            "Basic abc",
            "password: abc",
            "session_cookie=abc",
            "signature abc",
            "x-api-key: abc123",
            "See HTTPS://user:pass@host/?key=abc123",
            "unlabeled private-value",
        ] {
            let safe = diagnostic(
                &serde_json::json!({"message":message}),
                "http_error",
                "private-value",
            )
            .unwrap();
            assert_eq!(
                safe["message"], "Sensitive diagnostic text omitted",
                "{message}"
            );
        }
        let long = "错误".repeat(1000);
        let safe = diagnostic(
            &serde_json::json!({"code":"permission_denied", "message":long}),
            "http_error",
            "private-value",
        )
        .unwrap();
        assert!(safe["message"].as_str().unwrap().len() <= 512);
        let safe = diagnostic(
            &serde_json::json!({"message": "a ".repeat(10000)}),
            "http_error",
            "",
        )
        .unwrap();
        assert_eq!(safe["message"], "Oversized diagnostic text omitted");
        let safe = diagnostic(
            &serde_json::json!({"message":"See https://host/?key=value"}),
            "http_error",
            "",
        )
        .unwrap();
        assert!(!safe["message"].as_str().unwrap().contains("key=value"));
        assert!(diagnostic(&serde_json::json!({"other":"opaque"}), "http_error", "").is_none());
        let safe = diagnostic(
            &serde_json::json!({"code":"private-value", "message":"ok"}),
            "http_error",
            "private-value",
        )
        .unwrap();
        assert!(!safe.to_string().contains("private-value"));
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
