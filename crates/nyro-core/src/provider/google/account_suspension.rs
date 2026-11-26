//! Google subscription account suspension (`TOS_VIOLATION`) evidence.
//!
//! Once Google suspends an account for a Terms of Service violation, every
//! Code Assist `v1internal` call answers `403 PERMISSION_DENIED` carrying
//! `google.rpc.ErrorInfo{reason: "TOS_VIOLATION"}` plus appeal metadata (shape
//! from CLIProxyAPI 5a351d957, issue #7515). Unlike an ordinary 403 (missing
//! entitlement, disabled API), retrying never succeeds until an appeal is
//! accepted, so the dispatcher persists it as a terminal credential error and
//! stops routing to the account until it is reauthorized.

use serde_json::Value;

/// Longest appeal URL carried into the stored credential error.
const MAX_APPEAL_URL_LEN: usize = 256;

/// Structured suspension evidence from one upstream error body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TosViolation {
    /// Sanitized `https` appeal link (query and fragment removed), if any.
    appeal_url: Option<String>,
}

impl TosViolation {
    /// Operator-facing credential error. Only the recognized reason and the
    /// sanitized appeal link are kept; free-text upstream messages can carry
    /// account identifiers and are never copied.
    pub(crate) fn credential_error(&self) -> String {
        let next = match &self.appeal_url {
            Some(url) => format!("submit an appeal at {url}, then reauthorize the account"),
            None => "appeal with Google, then reauthorize the account".to_string(),
        };
        format!(
            "Google suspended this account for a Terms of Service violation (TOS_VIOLATION); {next}"
        )
    }
}

/// Classify an upstream error body. Only an HTTP 403 whose `ErrorInfo`
/// reason is `TOS_VIOLATION` qualifies; `None` for every other shape.
pub(crate) fn tos_violation(status: u16, body: &Value) -> Option<TosViolation> {
    if status != 403 {
        return None;
    }
    // Code Assist stream actions may wrap the error object in a JSON array.
    let error = body
        .get("error")
        .or_else(|| body.as_array()?.first()?.get("error"))?;
    error
        .get("details")?
        .as_array()?
        .iter()
        .filter(|detail| {
            detail
                .get("@type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with("google.rpc.ErrorInfo"))
        })
        .find(|detail| {
            detail
                .get("reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.eq_ignore_ascii_case("TOS_VIOLATION"))
        })
        .map(|detail| TosViolation {
            appeal_url: detail
                .pointer("/metadata/appeal_url")
                .and_then(Value::as_str)
                .and_then(sanitize_appeal_url),
        })
}

fn sanitize_appeal_url(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw.trim()).ok()?;
    if url.scheme() != "https" || url.host_str().is_none_or(str::is_empty) {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let url = url.to_string();
    (url.len() <= MAX_APPEAL_URL_LEN).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tos_body(appeal_url: &str) -> Value {
        json!({"error": {
            "code": 403,
            "message": "This service has been disabled in this account for violation of Terms of Service. Please submit an appeal to continue using this product.",
            "status": "PERMISSION_DENIED",
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "TOS_VIOLATION",
                "domain": "cloudcode-pa.googleapis.com",
                "metadata": {
                    "appeal_url": appeal_url,
                    "appeal_url_link_text": "Submit Appeal"
                }
            }]
        }})
    }

    #[test]
    fn recognizes_verbatim_signature_and_sanitizes_appeal_url() {
        let violation =
            tos_violation(403, &tos_body("https://forms.gle/hGzM9MEUv2azZsrb9")).unwrap();
        assert_eq!(
            violation.credential_error(),
            "Google suspended this account for a Terms of Service violation (TOS_VIOLATION); submit an appeal at https://forms.gle/hGzM9MEUv2azZsrb9, then reauthorize the account"
        );
        // Array-wrapped stream errors carry the same evidence.
        let wrapped = json!([tos_body("https://forms.gle/x")]);
        assert!(tos_violation(403, &wrapped).is_some());
        // Query strings and fragments may identify the account; drop them.
        let tracked = tos_violation(
            403,
            &tos_body("https://support.google.com/appeal?email=a@b.c#f"),
        )
        .unwrap();
        assert!(
            tracked
                .credential_error()
                .contains("https://support.google.com/appeal,")
        );
        assert!(!tracked.credential_error().contains("email"));
    }

    #[test]
    fn unsafe_or_missing_appeal_urls_fall_back_to_generic_guidance() {
        let long = format!("https://forms.gle/{}", "a".repeat(MAX_APPEAL_URL_LEN));
        for url in [
            "http://forms.gle/x",
            "javascript:alert(1)",
            "https://user:pw@forms.gle/x",
            "",
            long.as_str(),
        ] {
            let violation = tos_violation(403, &tos_body(url)).unwrap();
            assert_eq!(violation, TosViolation { appeal_url: None }, "{url:?}");
            assert!(
                violation
                    .credential_error()
                    .ends_with("appeal with Google, then reauthorize the account")
            );
        }
    }

    #[test]
    fn only_a_403_with_tos_error_info_qualifies() {
        assert_eq!(tos_violation(401, &tos_body("https://forms.gle/x")), None);
        assert_eq!(tos_violation(429, &tos_body("https://forms.gle/x")), None);
        for body in [
            json!({"error": {"code": 403, "status": "PERMISSION_DENIED"}}),
            json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED"}
            ]}}),
            // The reason must come from ErrorInfo, not from another detail type.
            json!({"error": {"code": 403, "details": [
                {"@type": "type.googleapis.com/google.rpc.Help", "reason": "TOS_VIOLATION"}
            ]}}),
            json!({"error": "TOS_VIOLATION"}),
            json!("TOS_VIOLATION"),
        ] {
            assert_eq!(tos_violation(403, &body), None, "{body}");
        }
    }
}
