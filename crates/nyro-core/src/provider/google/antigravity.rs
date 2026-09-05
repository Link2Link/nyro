//! Antigravity (Google AI Pro) wire adapter for the `google` vendor.
//!
//! The Code Assist internal API (`cloudcode-pa.googleapis.com/v1internal`)
//! wraps the standard Gemini generateContent body:
//!
//! ```json
//! {
//!   "model": "gemini-2.5-pro",
//!   "project": "<cloudaicompanionProject>",
//!   "requestType": "agent",
//!   "userAgent": "antigravity",
//!   "requestId": "agent-<uuid>",
//!   "request": { "...standard Gemini generateContent body...": {} }
//! }
//! ```
//!
//! Responses (and every SSE `data:` line) arrive as
//! `{"response": <gemini chunk>, "responseId": ...}` and must be unwrapped
//! before the google-gemini codec can parse them. Fingerprint fields mirror
//! the Antigravity IDE (validated by CLIProxyAPI / sub2api): a
//! `request.sessionId` derived from the user turn, no
//! `request.safetySettings`, and the agent-style `requestId`.

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::models::Provider;

/// Curated model catalog for the subscription channel. The upstream exposes
/// the authoritative per-account list via `v1internal:fetchAvailableModels`
/// (follow-up); this static list mirrors what the references ship.
pub(crate) const ANTIGRAVITY_STATIC_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-image",
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
];

/// Antigravity IDE public OAuth client (published "installed app" client;
/// the secret lives in the auth driver, metadata stays secret-free).
pub(crate) const ANTIGRAVITY_OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";

pub(crate) const ANTIGRAVITY_REDIRECT_URI: &str = "http://localhost:8085/callback";

pub(crate) const ANTIGRAVITY_OAUTH_SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/cloud-platform ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile ",
    "https://www.googleapis.com/auth/cclog ",
    "https://www.googleapis.com/auth/experimentsandconfigs",
);

/// Antigravity client version reported in the User-Agent. Cloud Code rejects
/// newer models for clients below 2.9.0, so keep this at or above 2.9.1.
/// **Bump this** when Google tightens client gating.
pub(crate) const ANTIGRAVITY_VERSION: &str = "2.9.1";
const ANTIGRAVITY_PLATFORM: &str = "windows/amd64";

/// X-Goog-Api-Client header the Antigravity node client sends on
/// control-plane calls.
pub(crate) const X_GOOG_API_CLIENT: &str = "gl-node/22.21.1";

pub(crate) fn antigravity_user_agent() -> String {
    format!("antigravity/{ANTIGRAVITY_VERSION} {ANTIGRAVITY_PLATFORM}")
}

/// True when the provider row is the google/antigravity OAuth channel.
pub(crate) fn is_antigravity_channel(provider: &Provider) -> bool {
    provider
        .channel
        .as_deref()
        .is_some_and(|channel| channel.trim().eq_ignore_ascii_case("antigravity"))
}

/// Vendor+channel check (channel name alone may collide on custom vendors).
pub(crate) fn is_google_antigravity(provider: &Provider) -> bool {
    provider
        .vendor
        .as_deref()
        .is_some_and(|vendor| vendor.trim().eq_ignore_ascii_case("google"))
        && is_antigravity_channel(provider)
}

/// The Code Assist v1internal surface behaves reliably only in streaming
/// mode: both reference clients (sub2api antigravity gateway, CLIProxyAPI
/// claude-model path) call `:streamGenerateContent` and aggregate, because
/// the non-stream action can return empty bodies. True when this provider
/// must always stream upstream regardless of the client's stream flag.
pub(crate) fn forces_upstream_stream(provider: &Provider) -> bool {
    is_google_antigravity(provider)
}

/// Per-account dynamic model discovery: `v1internal:fetchAvailableModels`
/// returns the authoritative catalog (with per-model quotas) for THIS
/// subscription — newer models appear here before any static list can ship.
pub(crate) async fn fetch_available_models(
    client: &reqwest::Client,
    access_token: &str,
    project_id: &str,
) -> Result<Vec<String>> {
    let endpoint = "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
    let response = client
        .post(endpoint)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "*/*")
        .header("User-Agent", antigravity_user_agent())
        .json(&json!({ "project": project_id }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("antigravity fetchAvailableModels failed: HTTP {status}: {body}");
    }
    let parsed: Value = serde_json::from_str(&body)?;
    Ok(parse_available_models(&parsed))
}

/// `{"models": {"<model-id>": {…quotaInfo…}}, "deprecatedModelIds": …}`
/// → sorted model ids.
pub(crate) fn parse_available_models(payload: &Value) -> Vec<String> {
    let Some(models) = payload.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = models
        .keys()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    ids.sort();
    ids
}

/// Read the companion project id from the OAuth credential metadata.
pub(crate) fn antigravity_project_id(credential: Option<&crate::auth::types::StoredCredential>) -> Result<String> {
    credential
        .and_then(|cred| cred.meta.get("project_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "google/antigravity credential is missing project_id; \
re-login the provider to re-run onboarding"
            )
        })
}

/// Wrap a standard Gemini generateContent body into the v1internal envelope.
pub(crate) fn wrap_request(body: Value, model: &str, project_id: &str) -> Value {
    let mut request = match body {
        Value::Object(map) => serde_json::Map::from_iter(map),
        other => {
            // Degenerate input: keep it inspectable rather than dropping it.
            let mut map = serde_json::Map::new();
            map.insert("contents".to_string(), other);
            map
        }
    };

    // The internal API rejects safetySettings on this path.
    request.remove("safetySettings");

    // Stable per-conversation session id, derived from the latest user turn
    // (mirrors the Antigravity IDE's own derivation).
    let has_session_id = request
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if !has_session_id {
        let session_id = derive_session_id(request.get("contents"));
        request.insert("sessionId".to_string(), Value::String(session_id));
    }

    json!({
        "model": model,
        "project": project_id,
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": format!("agent-{}", Uuid::new_v4()),
        "request": Value::Object(request),
    })
}

/// Unwrap a v1internal response envelope in place. Non-wrapped bodies (error
/// payloads, plain Gemini responses) pass through untouched.
pub(crate) fn unwrap_response(body: &mut Value) {
    if let Some(inner) = body.get("response").filter(|inner| inner.is_object()) {
        let inner = inner.clone();
        *body = inner;
    }
}

/// Unwrap every SSE `data:` line of a v1internal stream chunk. Lines that
/// are not `data:` payloads or do not carry a `response` object are kept
/// verbatim.
pub(crate) fn unwrap_stream_chunk(chunk: &str) -> String {
    let mut out = String::with_capacity(chunk.len());
    for line in chunk.split_inclusive('\n') {
        out.push_str(&unwrap_stream_line(line));
    }
    out
}

fn unwrap_stream_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let (prefix, rest) = match trimmed.split_once(':') {
        Some((prefix, rest)) if prefix.eq_ignore_ascii_case("data") => (prefix, rest),
        _ => return line.to_string(),
    };
    let data = rest.strip_prefix(' ').unwrap_or(rest).trim();
    if data.is_empty() {
        return line.to_string();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(data) else {
        return line.to_string();
    };
    let Some(inner) = parsed.get("response").filter(|inner| inner.is_object()) else {
        return line.to_string();
    };
    let mut rewritten = String::with_capacity(line.len());
    rewritten.push_str(&line[..indent_len]);
    rewritten.push_str(prefix);
    rewritten.push_str(": ");
    rewritten.push_str(&inner.to_string());
    // Preserve the original line terminator.
    let newline_len = line.chars().rev().take_while(|c| *c == '\n').count();
    for _ in 0..newline_len {
        rewritten.push('\n');
    }
    rewritten
}

/// Antigravity session ids look like Google's own: a negative decimal string.
fn derive_session_id(contents: Option<&Value>) -> String {
    let mut hasher = Sha256::new();
    let mut saw_user_text = false;
    if let Some(contents) = contents.and_then(Value::as_array) {
        for content in contents {
            if content.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if text.trim().is_empty() {
                            continue;
                        }
                        saw_user_text = true;
                        hasher.update(text.as_bytes());
                    }
                }
            }
        }
    }
    if !saw_user_text {
        return format!("-{}", random_session_digits());
    }
    let digest = hasher.finalize();
    let numeric = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
    format!("-{numeric}")
}

fn random_session_digits() -> u64 {
    let mut bytes = [0u8; 8];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut bytes);
    u64::from_be_bytes(bytes) % 9_000_000_000_000_000_000
}

/// Build the v1internal inference path for the given codec egress path.
pub(crate) fn build_v1internal_url(base_url: &str, codec_path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if codec_path.contains(":streamGenerateContent") {
        format!("{base}/v1internal:streamGenerateContent?alt=sse")
    } else {
        format!("{base}/v1internal:generateContent")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_request_builds_v1internal_envelope() {
        let body = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hello"}]},
                {"role": "user", "parts": [{"text": "again"}]},
            ],
            "generationConfig": {"temperature": 0.5},
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT"}],
        });
        let wrapped = wrap_request(body, "gemini-2.5-pro", "cloudaicompanion-1");
        assert_eq!(wrapped["model"], "gemini-2.5-pro");
        assert_eq!(wrapped["project"], "cloudaicompanion-1");
        assert_eq!(wrapped["requestType"], "agent");
        assert_eq!(wrapped["userAgent"], "antigravity");
        assert!(wrapped["requestId"]
            .as_str()
            .unwrap()
            .starts_with("agent-"));
        assert_eq!(wrapped["request"]["generationConfig"]["temperature"], 0.5);
        // safetySettings stripped, sessionId injected.
        assert!(wrapped["request"].get("safetySettings").is_none());
        let session_id = wrapped["request"]["sessionId"].as_str().unwrap();
        assert!(session_id.starts_with('-'));
        // Deterministic on identical content.
        let body2 = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hello"}]},
                {"role": "user", "parts": [{"text": "again"}]},
            ],
        });
        let wrapped2 = wrap_request(body2, "gemini-2.5-pro", "cloudaicompanion-1");
        assert_eq!(wrapped2["request"]["sessionId"].as_str(), Some(session_id));
    }

    #[test]
    fn unwrap_response_replaces_envelope_with_inner() {
        let mut body = json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "hi"}]}}]},
            "responseId": "abc",
            "modelVersion": "gemini-2.5-pro",
        });
        unwrap_response(&mut body);
        assert!(body.get("candidates").is_some());
        assert!(body.get("response").is_none());
        // Non-wrapped bodies pass through.
        let mut plain = json!({"error": {"message": "nope"}});
        unwrap_response(&mut plain);
        assert!(plain.get("error").is_some());
    }

    #[test]
    fn unwrap_stream_chunk_rewrites_data_lines_only() {
        let chunk = concat!(
            "event: message\n",
            "data: {\"response\":{\"candidates\":[]},\"responseId\":\"r1\"}\n\n",
            ": keep-alive comment\n",
            "data: [DONE]\n"
        );
        let out = unwrap_stream_chunk(chunk);
        assert!(out.contains("event: message\n"), "non-data lines preserved");
        assert!(
            out.contains("data: {\"candidates\":[]}\n\n"),
            "data line unwrapped: {out}"
        );
        assert!(!out.contains("responseId"), "envelope keys dropped");
        assert!(out.contains(": keep-alive comment\n"));
        assert!(out.contains("data: [DONE]\n"));
    }

    #[test]
    fn v1internal_url_matches_stream_and_non_stream() {
        assert_eq!(
            build_v1internal_url(
                "https://cloudcode-pa.googleapis.com",
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
            ),
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            build_v1internal_url(
                "https://cloudcode-pa.googleapis.com/",
                "/v1beta/models/gemini-2.5-pro:generateContent"
            ),
            "https://cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
    }

    #[test]
    fn forces_upstream_stream_requires_google_vendor_and_channel() {
        let mut provider = crate::db::models::Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url: String::new(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some("antigravity".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(forces_upstream_stream(&provider));
        provider.vendor = Some("custom".into());
        assert!(!forces_upstream_stream(&provider), "channel alone must not flip a custom vendor");
        provider.vendor = Some("google".into());
        provider.channel = Some("default".into());
        assert!(!forces_upstream_stream(&provider), "default channel keeps non-stream upstream");
    }

    #[test]
    fn parse_available_models_extracts_sorted_catalog() {
        let payload = json!({
            "models": {
                "gemini-3.6-flash": {"quotaInfo": {"remainingFraction": 0.9}},
                "gemini-2.5-pro": {"quotaInfo": {"remainingFraction": 1.0}},
                "gemini-3.1-pro-preview": {},
            },
            "deprecatedModelIds": {"old-model": {"newModelId": "gemini-2.5-pro"}},
        });
        assert_eq!(
            parse_available_models(&payload),
            vec![
                "gemini-2.5-pro".to_string(),
                "gemini-3.1-pro-preview".to_string(),
                "gemini-3.6-flash".to_string(),
            ]
        );
        assert!(parse_available_models(&json!({})).is_empty());
        assert!(parse_available_models(&json!({"models": {}})).is_empty());
    }

    #[test]
    fn project_id_requires_credential_meta() {
        assert!(antigravity_project_id(None).is_err());
        let cred = crate::auth::types::StoredCredential {
            meta: json!({"project_id": "proj-x"}),
            ..Default::default()
        };
        assert_eq!(
            antigravity_project_id(Some(&cred)).unwrap(),
            "proj-x".to_string()
        );
    }
}
