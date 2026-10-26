//! Bounded admin-only diagnostics, separate from client and retry semantics.
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Map, Value, json};

pub(super) const MAX_ERROR_BYTES: usize = 16 * 1024;
const MAX_TEXT_BYTES: usize = 512;
const MAX_IDENTIFIER_BYTES: usize = 128;
const SENSITIVE: &str = "Sensitive diagnostic text omitted";

/// Check the entire field BEFORE truncation, including the actual credential
/// (which need not have an identifying prefix). Omit suspicious fields whole;
/// retaining a prefix of a credential-bearing field is not safe redaction.
fn sanitize(text: &str, token: &str, identifier: bool) -> String {
    if text.len() > MAX_ERROR_BYTES {
        return "Oversized diagnostic text omitted".into();
    }
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let lower = clean.to_ascii_lowercase();
    let token = token.trim();
    if (!token.is_empty() && (text.contains(token) || clean.contains(token)))
        || [
            "bearer",
            "basic ",
            "authorization",
            "api-key",
            "api_key",
            "api key",
            "apikey",
            "private key",
            "password",
            "passwd",
            "cookie",
            "secret",
            "token",
            "credential",
            "signature",
            "session",
            "ksk_",
            "sk-",
            "eyj",
            "://",
            "//",
            "www.",
            "mailto:",
            "key=",
            "auth=",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return SENSITIVE.into();
    }
    if identifier {
        // Codes may include an AWS namespace; param may be a field/index path.
        // Do not let arbitrary prose or conversation bypass the text budget.
        if clean.is_empty()
            || clean.len() > MAX_IDENTIFIER_BYTES
            || !clean.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'_' | b'-' | b'.' | b'#' | b':' | b'[' | b']')
            })
        {
            return "Unrecognized diagnostic identifier omitted".into();
        }
        return clean;
    }
    let mut safe = crate::logging::diagnostics::sanitize_cause(&clean);
    if safe.len() > MAX_TEXT_BYTES {
        let mut end = MAX_TEXT_BYTES - 3;
        while !safe.is_char_boundary(end) {
            end -= 1;
        }
        safe.truncate(end);
        safe.push_str("...");
    }
    safe
}

/// Only these static names may bypass the conservative credential-word filter.
/// AWS JSON uses namespace#Type; x-amzn-errortype may append :detail. Neither
/// namespace nor suffix is retained, even if it contains a URL or a secret.
fn known_error_type(text: &str) -> Option<&'static str> {
    if text.len() > MAX_ERROR_BYTES || text.chars().any(char::is_control) {
        return None;
    }
    let name = text.trim().split(':').next()?.rsplit('#').next()?;
    [
        "UnauthorizedException",
        "InvalidTokenException",
        "ExpiredTokenException",
        "ExpiredToken",
        "InvalidClientTokenId",
        "UnrecognizedClientException",
        "AccessDeniedException",
        "AccessDenied",
        "ForbiddenException",
        "AuthorizationException",
        "AccountSuspendedException",
        "ServiceQuotaExceededException",
        "QuotaExceededException",
        "ThrottlingException",
        "TooManyRequestsException",
        "ValidationException",
        "ResourceNotFoundException",
        "InternalServerException",
        "ServiceUnavailableException",
    ]
    .into_iter()
    .find(|known| name.eq_ignore_ascii_case(known))
}

fn sanitize_error_type(text: &str, token: &str) -> String {
    if text.len() > MAX_ERROR_BYTES {
        return "Oversized diagnostic text omitted".into();
    }
    // A credential could itself equal a known type, or occur in its namespace.
    // In that case the normal whole-field credential check still wins.
    let token = token.trim();
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if !token.is_empty() && (text.contains(token) || clean.contains(token)) {
        return SENSITIVE.into();
    }
    if matches!(
        text,
        SENSITIVE
            | "Oversized diagnostic text omitted"
            | "Unrecognized diagnostic identifier omitted"
    ) {
        return text.into();
    }
    if let Some(known) = known_error_type(text) {
        if !token.is_empty() && known.contains(token) {
            return SENSITIVE.into();
        }
        return known.into();
    }
    let safe = sanitize(text, token, true);
    if safe == SENSITIVE || safe == "Oversized diagnostic text omitted" {
        safe
    } else {
        "Unrecognized diagnostic identifier omitted".into()
    }
}

/// Safe evidence, not an account/retry decision. In particular, an ambiguous
/// UnauthorizedException alone does not prove that a token is expired/invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiagnosticReason {
    TokenExpired,
    TokenInvalid,
    PermissionDenied,
    AccountSuspended,
    QuotaExhausted,
}

impl DiagnosticReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::TokenExpired => "token_expired",
            Self::TokenInvalid => "token_invalid",
            Self::PermissionDenied => "permission_denied",
            Self::AccountSuspended => "account_suspended",
            Self::QuotaExhausted => "quota_exhausted",
        }
    }
}

fn type_reason(text: &str) -> Option<DiagnosticReason> {
    use DiagnosticReason::*;
    match known_error_type(text)? {
        "ExpiredTokenException" | "ExpiredToken" => Some(TokenExpired),
        "InvalidTokenException" | "InvalidClientTokenId" | "UnrecognizedClientException" => {
            Some(TokenInvalid)
        }
        "AccessDeniedException" | "AccessDenied" | "ForbiddenException" => Some(PermissionDenied),
        "AccountSuspendedException" => Some(AccountSuspended),
        "ServiceQuotaExceededException" | "QuotaExceededException" => Some(QuotaExhausted),
        _ => None,
    }
}

fn object_reason(
    root: Option<&Map<String, Value>>,
    headers: &HeaderMap,
) -> Option<DiagnosticReason> {
    use DiagnosticReason::*;
    let mut reasons = Vec::new();
    if let Some(reason) = headers
        .get("x-amzn-errortype")
        .and_then(|value| value.to_str().ok())
        .and_then(type_reason)
    {
        reasons.push(reason);
    }
    if let Some(root) = root {
        for object in std::iter::once(root).chain(root.get("error").and_then(Value::as_object)) {
            for key in ["code", "__type", "type", "errorType"] {
                if let Some(reason) = object
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(type_reason)
                {
                    reasons.push(reason);
                }
            }
            for key in ["message", "Message", "reason"] {
                let Some(text) = object.get(key).and_then(Value::as_str) else {
                    continue;
                };
                // Inspect only selected bounded fields; emit constants, never
                // substrings. Raw messages still go through strict sanitization.
                let lower = text.to_ascii_lowercase();
                for (reason, phrases) in [
                    (
                        TokenExpired,
                        &[
                            "token expired",
                            "token has expired",
                            "token is expired",
                            "token included in the request is expired",
                            "token_expired",
                        ][..],
                    ),
                    (
                        TokenInvalid,
                        &[
                            "invalid token",
                            "token is invalid",
                            "token is not valid",
                            "token included in the request is invalid",
                            "invalid_token",
                        ][..],
                    ),
                    (
                        PermissionDenied,
                        &[
                            "access denied",
                            "permission denied",
                            "not authorized to",
                            "access_denied",
                        ][..],
                    ),
                    (
                        AccountSuspended,
                        &[
                            "account suspended",
                            "account is suspended",
                            "account has been suspended",
                            "account_suspended",
                        ][..],
                    ),
                    (
                        QuotaExhausted,
                        &[
                            "quota exhausted",
                            "quota exceeded",
                            "monthly quota",
                            "usage limit exceeded",
                            "monthly_request_count",
                            "quota_exceeded",
                        ][..],
                    ),
                ] {
                    if phrases.iter().any(|phrase| lower.contains(phrase)) {
                        reasons.push(reason);
                    }
                }
            }
        }
    }
    // Prefer specific account/quota/token evidence over a generic denial type.
    [
        AccountSuspended,
        QuotaExhausted,
        TokenExpired,
        TokenInvalid,
        PermissionDenied,
    ]
    .into_iter()
    .find(|reason| reasons.contains(reason))
}

/// Optional runtime integration point. Call before discarding the bounded body;
/// callers own status/retry policy. Incomplete/non-object bodies are never read
/// for evidence, but a known response-header type can still be classified.
pub(super) fn reason_class(
    body: &[u8],
    complete: bool,
    headers: &HeaderMap,
) -> Option<DiagnosticReason> {
    let parsed = (complete && body.len() <= MAX_ERROR_BYTES)
        .then(|| serde_json::from_slice::<Value>(body).ok())
        .flatten();
    object_reason(parsed.as_ref().and_then(Value::as_object), headers)
}

/// A logging-only copy. The transport's original headers (including retry-after)
/// remain untouched. This also prevents a token echoed in an innocently named
/// response header from bypassing the stricter diagnostic field redaction.
pub(super) fn safe_headers(headers: &HeaderMap, token: &str) -> HeaderMap {
    let mut safe = HeaderMap::new();
    for name in [
        "content-type",
        "content-length",
        "retry-after",
        "x-amzn-errortype",
        "x-amzn-requestid",
        "x-amz-request-id",
        "x-request-id",
        "request-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            let identifier = !matches!(name, "content-type" | "retry-after");
            let value = if name == "x-amzn-errortype" {
                sanitize_error_type(value, token)
            } else {
                sanitize(value, token, identifier)
            };
            if let Ok(value) = HeaderValue::from_str(&value) {
                safe.insert(name, value);
            }
        }
    }
    safe
}

fn text_field<'a>(object: &'a Map<String, Value>, aliases: &[&str]) -> Option<&'a str> {
    aliases.iter().find_map(|key| object.get(*key)?.as_str())
}

/// Only complete, bounded JSON objects are eligible for field extraction. Never
/// interpret a truncated prefix as a complete error or fall back to raw text.
/// Fixtures model possible AWS shapes, not a recovered production error body.
pub(super) fn diagnostic(
    body: &[u8],
    complete: bool,
    headers: &HeaderMap,
    status: u16,
    token: &str,
) -> Value {
    let mut diagnostic = json!({
        "source": "http_error",
        "status": status,
        "sanitized": true,
        "body_state": "incomplete_or_oversized",
    });
    if complete && body.len() <= MAX_ERROR_BYTES {
        match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(root)) => {
                diagnostic["body_state"] = json!("json");
                let error = root
                    .get("error")
                    .and_then(Value::as_object)
                    .unwrap_or(&root);
                for (field, aliases, identifier) in [
                    ("code", &["code", "__type", "type", "errorType"][..], true),
                    ("message", &["message", "Message"][..], false),
                    ("reason", &["reason"][..], false),
                    ("param", &["param"][..], true),
                ] {
                    if let Some(value) =
                        text_field(error, aliases).or_else(|| text_field(&root, aliases))
                    {
                        diagnostic[field] = json!(if field == "code" {
                            sanitize_error_type(value, token)
                        } else {
                            sanitize(value, token, identifier)
                        });
                    }
                }
            }
            _ => diagnostic["body_state"] = json!("invalid_json"),
        }
    }
    if let Some(reason) = reason_class(body, complete, headers) {
        diagnostic["reason_class"] = json!(reason.as_str());
    }
    // Runtime supplies sanitized headers; normalize the type again so this
    // diagnostic helper cannot accidentally expose a raw type from other callers.
    if let Some(error_type) = headers
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
    {
        diagnostic["error_type"] = json!(sanitize_error_type(error_type, token));
    }
    if let Some(request_id) = [
        "x-amzn-requestid",
        "x-amz-request-id",
        "x-request-id",
        "request-id",
    ]
    .iter()
    .find_map(|name| headers.get(*name)?.to_str().ok())
    {
        diagnostic["request_id"] = json!(request_id);
    }
    diagnostic
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_independent_and_sanitized_before_truncation() {
        let token = "actual-opaque-value";
        for field in ["code", "message", "reason", "param"] {
            for dangerous in [
                format!("{} {token}", "safe prefix ".repeat(70)),
                "Bearer abc".into(),
                "Basic abc".into(),
                "password: abc".into(),
                "session_cookie=abc".into(),
                "x-api-key: abc".into(),
                "ksk_opaque".into(),
                "See HTTPS://user:pass@host/path?key=abc".into(),
                "See //host/path?key=abc".into(),
            ] {
                let mut value = json!({"message":"Improperly formed request.", "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"});
                value[field] = json!(dangerous);
                let diagnostic = diagnostic(
                    &serde_json::to_vec(&value).unwrap(),
                    true,
                    &HeaderMap::new(),
                    400,
                    token,
                );
                assert_eq!(diagnostic[field], SENSITIVE, "{field}");
                if field != "reason" {
                    assert_eq!(diagnostic["reason"], "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
                }
                assert!(!diagnostic.to_string().contains(token));
            }
        }
    }

    #[test]
    fn known_types_are_canonical_but_never_copy_namespaces_suffixes_or_unknown_ids() {
        for (input, expected) in [
            ("UnauthorizedException", "UnauthorizedException"),
            ("invalidtokenexception", "InvalidTokenException"),
            (
                "com.amazon.auth#AccessDeniedException",
                "AccessDeniedException",
            ),
            (
                "AuthorizationException:https://private.test/?secret=other-key",
                "AuthorizationException",
            ),
            (
                "private-namespace#ExpiredTokenException:private-suffix",
                "ExpiredTokenException",
            ),
        ] {
            for alias in ["code", "__type", "type", "errorType"] {
                let mut body =
                    json!({"message": "Authorization token is invalid: Bearer other-secret"});
                body[alias] = json!(input);
                let mut headers = HeaderMap::new();
                headers.insert("x-amzn-errortype", HeaderValue::from_str(input).unwrap());
                let safe = diagnostic(
                    &serde_json::to_vec(&body).unwrap(),
                    true,
                    &safe_headers(&headers, "live-secret"),
                    403,
                    "live-secret",
                );
                assert_eq!(safe["code"], expected);
                assert_eq!(safe["error_type"], expected);
                assert_eq!(safe["message"], SENSITIVE);
                for forbidden in [
                    "private.test",
                    "other-key",
                    "private-namespace",
                    "private-suffix",
                    "other-secret",
                ] {
                    assert!(!safe.to_string().contains(forbidden), "{safe}");
                }
            }
        }
        for unknown in [
            "UnknownException",
            "PrivateOpaqueIdentifier",
            "UnauthorizedExceptionExtra",
            "https://host/InvalidTokenException",
        ] {
            let safe = sanitize_error_type(unknown, "live-secret");
            assert!(!safe.contains(unknown));
            assert!(known_error_type(unknown).is_none());
        }
        for (value, token) in [
            ("InvalidTokenException", "InvalidTokenException"),
            ("live-secret#InvalidTokenException", "live-secret"),
            ("InvalidTokenException:live-secret", "live-secret"),
            ("invalidtokenexception", "InvalidTokenException"),
        ] {
            assert_eq!(sanitize_error_type(value, token), SENSITIVE);
        }
        assert!(known_error_type("Invalid\nTokenException").is_none());
    }

    #[test]
    fn reason_evidence_is_structured_bounded_and_never_assumes_generic_403_is_auth_failure() {
        use DiagnosticReason::*;
        for (body, expected) in [
            (
                json!({"type":"UnauthorizedException", "message":"The security token included in the request is expired"}),
                Some(TokenExpired),
            ),
            (
                json!({"error":{"type":"UnauthorizedException", "Message":"The security token included in the request is invalid"}}),
                Some(TokenInvalid),
            ),
            (json!({"type":"InvalidTokenException"}), Some(TokenInvalid)),
            (
                json!({"type":"AccessDeniedException"}),
                Some(PermissionDenied),
            ),
            (
                json!({"type":"AccessDeniedException", "message":"Your account has been suspended. Contact support."}),
                Some(AccountSuspended),
            ),
            (
                json!({"message":"Monthly quota exhausted", "reason":"MONTHLY_REQUEST_COUNT"}),
                Some(QuotaExhausted),
            ),
            (json!({"type":"UnauthorizedException"}), None),
            (json!({"type":"AuthorizationException"}), None),
            (json!({"type":"ThrottlingException"}), None),
            (
                json!({"message":"Request failed", "details":{"message":"token expired"}, "unknown":"invalid token"}),
                None,
            ),
            (
                json!({"message":{"message":"token expired"}, "reason":["invalid token"]}),
                None,
            ),
        ] {
            let body = serde_json::to_vec(&body).unwrap();
            assert_eq!(reason_class(&body, true, &HeaderMap::new()), expected);
            assert_eq!(reason_class(&body, false, &HeaderMap::new()), None);
        }
        for body in [
            b"token expired".to_vec(),
            br#"{"message":"token expired""#.to_vec(),
            vec![b' '; MAX_ERROR_BYTES + 1],
        ] {
            assert_eq!(reason_class(&body, true, &HeaderMap::new()), None);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amzn-errortype",
            HeaderValue::from_static("com.amazon#ExpiredTokenException:private-detail"),
        );
        assert_eq!(
            reason_class(b"invalid JSON", false, &headers),
            Some(TokenExpired)
        );
        headers.insert(
            "x-amzn-errortype",
            HeaderValue::from_static("UnknownExpiredTokenException"),
        );
        assert_eq!(reason_class(b"", true, &headers), None);
    }

    #[test]
    fn fields_are_bounded_and_unknown_or_structured_content_is_excluded() {
        let body = serde_json::to_vec(&json!({"error": {
            "__type":"com.amazon#ValidationException", "message":"错".repeat(1000),
            "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":"messages[0].content",
            "raw":"private-conversation", "details":{"message":"private-conversation"},
            "messages":[{"content":"private-conversation"}],
        }}))
        .unwrap();
        let safe = diagnostic(&body, true, &HeaderMap::new(), 400, "opaque-auth");
        assert_eq!(safe["code"], "ValidationException");
        assert_eq!(safe["reason"], "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        assert_eq!(safe["param"], "messages[0].content");
        assert!(safe["message"].as_str().unwrap().len() <= MAX_TEXT_BYTES);
        assert!(!safe.to_string().contains("private-conversation"));
        let safe = diagnostic(
            br#"{"message":{"message":"private-conversation"},"reason":["private-conversation"]}"#,
            true,
            &HeaderMap::new(),
            400,
            "",
        );
        assert!(safe.get("message").is_none());
        assert!(safe.get("reason").is_none());
    }
}
