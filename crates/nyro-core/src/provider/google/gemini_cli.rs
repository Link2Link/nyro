//! Gemini CLI (Code Assist) subscription adapter for the `google` vendor.
//!
//! The same `cloudcode-pa.googleapis.com/v1internal` surface also serves
//! subscriptions authenticated through the public **Gemini CLI** OAuth
//! client (Code Assist Standard/Enterprise and legacy free tier). Compared
//! to the Antigravity channel the wire dialect is slimmer:
//!
//! * envelope: `{"model", "project", "request"}` only — no
//!   `requestType`/`userAgent`/`requestId` fingerprint fields and no
//!   `sessionId` injection (validated by sub2api's gemini-cli gateway).
//! * `safetySettings` pass through untouched (only the Antigravity path
//!   rejects them).
//! * control-plane UA is the Gemini CLI's own (`GeminiCLI/…`), not the
//!   Antigravity IDE's.
//! * model ids are clean (`gemini-3.6-flash`), without the antigravity
//!   thinking-tier suffixes (`gemini-3.6-flash-high`).
//!
//! Response envelopes (`{"response": …}` on both full bodies and SSE
//! `data:` lines) are identical to the Antigravity channel, so the unwrap
//! helpers in [`super::antigravity`] are reused as-is.

use serde_json::{Value, json};

use crate::db::models::Provider;

/// Curated fallback catalog for the channel. The per-account catalog is
/// discovered at runtime via `v1internal:fetchAvailableModels` (shared with
/// the antigravity channel); this list mirrors the union of what the
/// references currently ship (CLIProxyAPI `gemini` registry + sub2api
/// `geminicli.DefaultModels`) and only backs discovery when that call is
/// unavailable.
pub(crate) const GEMINI_CLI_STATIC_MODELS: &[&str] = &[
    "gemini-2.0-flash",
    "gemini-2.5-flash",
    "gemini-2.5-flash-image",
    "gemini-2.5-flash-lite",
    "gemini-2.5-pro",
    "gemini-3-flash-preview",
    "gemini-3-pro-image-preview",
    "gemini-3-pro-preview",
    "gemini-3.1-flash-image",
    "gemini-3.1-flash-image-preview",
    "gemini-3.1-flash-lite-preview",
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.6-flash",
    "gemini-3.7-flash",
];

/// Gemini CLI public OAuth client ("login without creating your own OAuth
/// client"); the secret lives in the auth driver, metadata stays
/// secret-free. Identical to the value shipped by sub2api.
pub(crate) const GEMINI_CLI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// Google shows the authorization code on this page (copy/paste flow); the
/// client has no loopback redirect registered.
pub(crate) const GEMINI_CLI_REDIRECT_URI: &str = "https://codeassist.google.com/authcode";

/// Code Assist scopes for the built-in client (the client rejects
/// generative-language / drive scopes).
pub(crate) const GEMINI_CLI_OAUTH_SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/cloud-platform ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile",
);

/// User-Agent the Gemini CLI sends. Keep aligned with sub2api
/// (`geminicli.GeminiCLIUserAgent`); bump when Google tightens gating.
pub(crate) const GEMINI_CLI_USER_AGENT: &str = "GeminiCLI/0.1.5 (Windows; AMD64)";

/// True when the provider row is the google/gemini-cli OAuth channel.
pub(crate) fn is_gemini_cli_channel(provider: &Provider) -> bool {
    provider
        .channel
        .as_deref()
        .is_some_and(|channel| channel.trim().eq_ignore_ascii_case("gemini-cli"))
}

/// Vendor+channel check (channel name alone may collide on custom vendors).
pub(crate) fn is_google_gemini_cli(provider: &Provider) -> bool {
    provider
        .vendor
        .as_deref()
        .is_some_and(|vendor| vendor.trim().eq_ignore_ascii_case("google"))
        && is_gemini_cli_channel(provider)
}

/// Wrap a standard Gemini generateContent body into the slim Code Assist
/// envelope. Unlike [`super::antigravity::wrap_request`] this adds no
/// fingerprint fields and leaves the inner body untouched.
pub(crate) fn wrap_request(body: Value, model: &str, project_id: &str) -> Value {
    json!({
        "model": model,
        "project": project_id,
        "request": body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(vendor: &str, channel: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some(vendor.into()),
            protocol: "google-gemini".into(),
            base_url: String::new(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some(channel.into()),
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
        }
    }

    #[test]
    fn channel_detection_requires_google_vendor() {
        assert!(is_google_gemini_cli(&provider("google", "gemini-cli")));
        assert!(!is_google_gemini_cli(&provider("google", "antigravity")));
        assert!(!is_google_gemini_cli(&provider("custom", "gemini-cli")));
    }

    #[test]
    fn wrap_request_builds_slim_envelope() {
        let inner = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT"}],
        });
        let wrapped = wrap_request(inner.clone(), "gemini-3.6-flash", "proj-1");
        assert_eq!(wrapped["model"], "gemini-3.6-flash");
        assert_eq!(wrapped["project"], "proj-1");
        assert_eq!(wrapped["request"], inner);
        // No antigravity fingerprint fields on this dialect.
        for key in ["requestType", "userAgent", "requestId"] {
            assert!(wrapped.get(key).is_none(), "unexpected {key} in envelope");
        }
    }

    #[test]
    fn static_models_carry_current_flash_line() {
        for model in ["gemini-3.5-flash", "gemini-3.6-flash", "gemini-3.7-flash"] {
            assert!(GEMINI_CLI_STATIC_MODELS.contains(&model), "missing {model}");
        }
    }
}
