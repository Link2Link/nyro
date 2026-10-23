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
            let value = sanitize(value, token, identifier);
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
                    ("code", &["code", "__type"][..], true),
                    ("message", &["message"][..], false),
                    ("reason", &["reason"][..], false),
                    ("param", &["param"][..], true),
                ] {
                    if let Some(value) =
                        text_field(error, aliases).or_else(|| text_field(&root, aliases))
                    {
                        diagnostic[field] = json!(sanitize(value, token, identifier));
                    }
                }
            }
            _ => diagnostic["body_state"] = json!("invalid_json"),
        }
    }
    // Header values have already passed the same whole-field sanitizer.
    if let Some(error_type) = headers
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
    {
        diagnostic["error_type"] = json!(error_type);
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
    fn fields_are_bounded_and_unknown_or_structured_content_is_excluded() {
        let body = serde_json::to_vec(&json!({"error": {
            "__type":"com.amazon#ValidationException", "message":"错".repeat(1000),
            "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD", "param":"messages[0].content",
            "raw":"private-conversation", "details":{"message":"private-conversation"},
            "messages":[{"content":"private-conversation"}],
        }}))
        .unwrap();
        let safe = diagnostic(&body, true, &HeaderMap::new(), 400, "opaque-auth");
        assert_eq!(safe["code"], "com.amazon#ValidationException");
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
