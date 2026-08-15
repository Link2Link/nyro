//! Unified observability sink for the proxy layer.
//!
//! All structured log writes and target-health updates MUST go through this
//! module.  No handler code should call `gw.log_tx.try_send` directly.

use crate::logging::LogEntry;

// ── Sensitive header redaction ─────────────────────────────────────────────────

/// Header names whose values are replaced with `"***"` before logging.
const REDACT_HEADER_KEYS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "openai-api-key",
    "anthropic-api-key",
    "cookie",
    "set-cookie",
    "proxy-authorization",
];

// ── Log extras ─────────────────────────────────────────────────────────────────

/// Optional HTTP-layer fields attached to every log entry. Used as an
/// intermediate carrier inside `LogBuilder`; maps 1-to-1 to `LogEntry` wire
/// fields.
#[derive(Default, Clone)]
pub struct LogExtras {
    pub method: Option<String>,
    pub path: Option<String>,

    pub client_request_headers: Option<String>,
    pub client_request_body: Option<String>,
    pub client_response_headers: Option<String>,
    pub client_response_body: Option<String>,

    pub upstream_request_headers: Option<String>,
    pub upstream_request_body: Option<String>,
    pub upstream_response_headers: Option<String>,
    pub upstream_response_body: Option<String>,

    pub upstream_url: Option<String>,
    pub upstream_status_code: Option<i32>,
    pub latency_upstream_ms: Option<i64>,

    pub stream_chunks_count: i32,
    pub stream_first_chunk_ms: Option<i64>,
}

// ── Direct log send ────────────────────────────────────────────────────────────

/// Enqueue a `LogEntry` directly. The canonical write path — no handler code
/// should call `gw.log_tx.try_send` outside of this function.
pub fn send_log(gw: &crate::Gateway, entry: LogEntry) {
    let _ = gw.log_tx.try_send(entry);
}

// ── headers_to_json ────────────────────────────────────────────────────────────

/// Serialize an axum `HeaderMap` to a flat JSON object string for logging.
/// Sensitive header values are replaced with `"***"`.
pub fn headers_to_json(headers: &axum::http::HeaderMap) -> Option<String> {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let key = name.as_str().to_ascii_lowercase();
        let val = if REDACT_HEADER_KEYS.contains(&key.as_str()) {
            serde_json::Value::String("***".to_string())
        } else {
            value
                .to_str()
                .map(|s| serde_json::Value::String(s.to_string()))
                .unwrap_or_else(|_| {
                    serde_json::Value::String(format!("0x{}", hex_encode(value.as_bytes())))
                })
        };
        map.insert(key, val);
    }
    serde_json::to_string(&serde_json::Value::Object(map)).ok()
}

/// Serialize a reqwest `HeaderMap` to a flat JSON object string for logging.
/// Sensitive header values are replaced with `"***"`.
pub fn reqwest_headers_to_json(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let key = name.as_str().to_ascii_lowercase();
        let val = if REDACT_HEADER_KEYS.contains(&key.as_str()) {
            serde_json::Value::String("***".to_string())
        } else {
            value
                .to_str()
                .map(|s| serde_json::Value::String(s.to_string()))
                .unwrap_or_else(|_| {
                    serde_json::Value::String(format!("0x{}", hex_encode(value.as_bytes())))
                })
        };
        map.insert(key, val);
    }
    serde_json::to_string(&serde_json::Value::Object(map)).ok()
}

pub fn header_map_to_redacted_json(
    headers: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let mut map = serde_json::Map::with_capacity(headers.len());
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        let val = if REDACT_HEADER_KEYS.contains(&key.as_str()) {
            serde_json::Value::String("***".to_string())
        } else {
            serde_json::Value::String(value.to_string())
        };
        map.insert(key, val);
    }
    serde_json::to_string(&serde_json::Value::Object(map)).ok()
}

pub fn redact_url_credentials(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return url.to_string();
    };

    if !parsed.username().is_empty() {
        let _ = parsed.set_username("***");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(Some("***"));
    }

    let mut redacted = false;
    let pairs = parsed
        .query_pairs()
        .map(|(key, value)| {
            let is_sensitive = matches!(
                key.to_ascii_lowercase().as_str(),
                "key" | "api_key" | "apikey" | "access_token" | "token"
            );
            if is_sensitive {
                redacted = true;
                (key.into_owned(), "***".to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect::<Vec<_>>();

    if redacted {
        parsed.set_query(None);
        {
            let mut query = parsed.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(&key, &value);
            }
        }
    }

    parsed.to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Derive the reasoning-effort label that was actually sent to the upstream
/// from the encoded upstream request body.
///
/// Recognizes the wire shapes produced by every egress codec / compat
/// transform, checked from most to least specific:
/// - OpenAI Chat top-level `reasoning_effort`
/// - OpenAI Responses / OpenRouter `reasoning.effort`
/// - Zhipu-style `output_config.effort`
/// - Gemini `generationConfig.thinkingConfig.thinkingLevel`
/// - Anthropic / DeepSeek `thinking` (`budget_tokens` preferred, else the
///   `type` discriminator so adaptive/enabled/disabled stays distinguishable)
pub(crate) fn upstream_reasoning_effort(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let labelled = value
        .get("reasoning_effort")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .pointer("/reasoning/effort")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .pointer("/output_config/effort")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value.get("thinking").and_then(|thinking| {
                thinking
                    .get("budget_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .map(|tokens| format!("budget:{tokens}"))
                    .or_else(|| {
                        thinking
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
            })
        });
    labelled.filter(|label| !label.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::upstream_reasoning_effort;

    #[test]
    fn reads_openai_chat_top_level_effort() {
        let body = r#"{"model":"deepseek-v4-flash","reasoning_effort":"high","messages":[]}"#;
        assert_eq!(upstream_reasoning_effort(body).as_deref(), Some("high"));
    }

    #[test]
    fn reads_responses_and_openrouter_reasoning_effort() {
        let body = r#"{"model":"glm-5.3","reasoning":{"effort":"xhigh"},"input":[]}"#;
        assert_eq!(upstream_reasoning_effort(body).as_deref(), Some("xhigh"));
    }

    #[test]
    fn reads_output_config_effort() {
        let body = r#"{"model":"glm-5.3","output_config":{"effort":"max"}}"#;
        assert_eq!(upstream_reasoning_effort(body).as_deref(), Some("max"));
    }

    #[test]
    fn reads_gemini_thinking_level() {
        let body =
            r#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingLevel":"high"}}}"#;
        assert_eq!(upstream_reasoning_effort(body).as_deref(), Some("high"));
    }

    #[test]
    fn reads_anthropic_thinking_budget_before_type() {
        let body = r#"{"model":"claude","thinking":{"type":"enabled","budget_tokens":16384}}"#;
        assert_eq!(
            upstream_reasoning_effort(body).as_deref(),
            Some("budget:16384")
        );
    }

    #[test]
    fn reads_anthropic_thinking_type_without_budget() {
        let body = r#"{"model":"glm","thinking":{"type":"adaptive"}}"#;
        assert_eq!(upstream_reasoning_effort(body).as_deref(), Some("adaptive"));
    }

    #[test]
    fn no_reasoning_directive_yields_none() {
        let body = r#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(upstream_reasoning_effort(body).is_none());
    }

    #[test]
    fn invalid_json_yields_none() {
        assert!(upstream_reasoning_effort("not json").is_none());
        assert!(upstream_reasoning_effort("").is_none());
    }
}
