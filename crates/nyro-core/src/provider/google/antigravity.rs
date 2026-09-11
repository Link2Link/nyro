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

/// Curated fallback catalog for the subscription channel. The authoritative
/// per-account list is fetched at runtime via
/// `v1internal:fetchAvailableModels` (see `fetch_available_models`); this
/// static list only backs discovery when that call is unavailable and
/// mirrors the catalog the references currently ship (CLIProxyAPI
/// antigravity registry). New upstream models land through dynamic
/// discovery long before this list needs a bump.
pub(crate) const ANTIGRAVITY_STATIC_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-image",
    "gemini-3-pro-preview",
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
    // Current subscription catalog: reasoning-tier suffixes are part of the
    // upstream model id (`-high` / `-low` select the thinking level).
    "gemini-3-flash",
    "gemini-3.1-flash-lite",
    "gemini-3.1-flash-image",
    "gemini-3.1-pro-low",
    "gemini-pro-agent",
    "gemini-3.6-flash-high",
    "gemini-3.7-flash-high",
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

/// Inference base URL for the antigravity channel. Consumer (Google AI
/// Pro/Ultra) credentials must send `generateContent`/`streamGenerateContent`
/// to the **daily** host — the prod host parks them in the free consumer
/// pool (`aicode-consumers` project) where every mainline model is
/// tier-gated to 429 RESOURCE_EXHAUSTED. Control-plane calls
/// (`loadCodeAssist`) stay on prod. Validated by CLIProxyAPI
/// (`resolveAntigravityRequestBaseURL` defaults consumer credentials to
/// daily) and sub2api (paid plan_type → daily endpoint).
pub(crate) const ANTIGRAVITY_INFERENCE_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

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
/// mode: both reference clients (sub2api gateways, CLIProxyAPI) call
/// `:streamGenerateContent` and aggregate, because the non-stream action can
/// return empty bodies. True when this provider must always stream upstream
/// regardless of the client's stream flag. Applies to both Google
/// subscription channels (antigravity + gemini-cli).
pub(crate) fn forces_upstream_stream(provider: &Provider) -> bool {
    is_google_antigravity(provider) || super::gemini_cli::is_google_gemini_cli(provider)
}

/// A catalog model with its per-model quota state (subscription channels).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AvailableModel {
    pub id: String,
    /// Remaining quota fraction (0.0–1.0), when the account reports it.
    pub quota_remaining: Option<f64>,
    /// ISO-8601 reset timestamp of the quota window, when reported.
    pub quota_resets_at: Option<String>,
}

/// Per-account dynamic model discovery: `v1internal:fetchAvailableModels`
/// returns the authoritative catalog (with per-model quotas) for THIS
/// subscription — newer models appear here before any static list can ship.
/// Shared by both Google subscription channels; `user_agent` selects the
/// client fingerprint (Antigravity IDE vs Gemini CLI).
pub(crate) async fn fetch_available_models(
    client: &reqwest::Client,
    access_token: &str,
    project_id: &str,
    user_agent: &str,
) -> Result<Vec<AvailableModel>> {
    let endpoint = "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
    let response = client
        .post(endpoint)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "*/*")
        .header("User-Agent", user_agent)
        .json(&json!({ "project": project_id }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("antigravity fetchAvailableModels failed: HTTP {status}: {body}");
    }
    let parsed: Value = serde_json::from_str(&body)?;
    Ok(parse_available_models_detailed(&parsed))
}

/// Same payload → per-model entries with quota state
/// (`quotaInfo.remainingFraction` / `quotaInfo.resetTime`, best-effort).
/// Ids listed in `deprecatedModelIds` are dropped — upstream itself marks
/// them dead (they 404 on the inference surface).
pub(crate) fn parse_available_models_detailed(payload: &Value) -> Vec<AvailableModel> {
    let Some(models) = payload.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let deprecated = payload
        .get("deprecatedModelIds")
        .and_then(Value::as_object)
        .map(|map| {
            map.keys()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut entries: Vec<AvailableModel> = models
        .iter()
        .filter(|(id, _)| !deprecated.iter().any(|dead| dead == *id))
        .map(|(id, info)| {
            let quota = info.get("quotaInfo").filter(|q| q.is_object());
            AvailableModel {
                id: id.trim().to_string(),
                quota_remaining: quota
                    .and_then(|q| q.get("remainingFraction"))
                    .and_then(Value::as_f64),
                quota_resets_at: quota
                    .and_then(|q| q.get("resetTime"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string),
            }
        })
        .filter(|model| !model.id.is_empty())
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries
}

/// Tier suffixes the antigravity surface bakes into model ids.
const TIER_SUFFIXES: &[&str] = &["-extra-low", "-tiered", "-medium", "-high", "-low"];

/// True when the model id already carries an antigravity thinking tier.
pub(crate) fn is_tier_suffixed(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && TIER_SUFFIXES.iter().any(|suffix| model.ends_with(suffix))
}

/// Preference ladders from the client effort vocabulary onto the surface's
/// tier ids, nearest-first so missing tiers degrade gracefully instead of
/// 404-ing the whole family:
/// * `none` / `disable` (effort None or reasoning disabled) → `-extra-low`
/// * `minimal` → `-extra-low`
/// * `low` → `-low`
/// * `medium` → `-medium`
/// * `high` / `xhigh` / `max` → `-high`
/// * token budgets map by magnitude; no effort at all → `-tiered` (adaptive)
fn tier_ladder(reasoning: &crate::protocol::ir::ReasoningConfig) -> &'static [&'static str] {
    use crate::protocol::ir::ReasoningEffort::{Budget, High, Low, Max, Medium, Minimal, Xhigh};
    const EXTRA_LOW: &[&str] = &["-extra-low", "-low"];
    const LOW: &[&str] = &["-low", "-extra-low"];
    const MEDIUM: &[&str] = &["-medium", "-low", "-high"];
    const HIGH: &[&str] = &["-high", "-medium", "-low"];
    const TIERED: &[&str] = &["-tiered", "-medium"];
    // `none` / disabled thinking → lowest tier available.
    if !reasoning.enabled
        || matches!(
            reasoning.effort.as_ref(),
            Some(crate::protocol::ir::ReasoningEffort::None)
        )
    {
        return EXTRA_LOW;
    }
    match reasoning.effort.as_ref() {
        Some(crate::protocol::ir::ReasoningEffort::None) | Some(Minimal) => EXTRA_LOW,
        Some(Low) => LOW,
        Some(Medium) => MEDIUM,
        Some(High | Xhigh | Max) => HIGH,
        Some(Budget(tokens)) => match tokens {
            0..=2048 => LOW,
            2049..=8192 => MEDIUM,
            _ => HIGH,
        },
        None => TIERED,
    }
}

/// Resolve the tier-suffixed model id for a base-id request on the
/// antigravity surface, driven by the client's reasoning effort.
/// Catalog-gated: the suffixed id must exist in the account's model snapshot
/// AND the bare base id must NOT be served itself (e.g. `gemini-2.5-flash`
/// is a real id — never rewritten), so families without tier variants fail
/// safe (no rewrite, upstream answers honestly).
pub(crate) fn resolve_tiered_model(
    model: &str,
    reasoning: &crate::protocol::ir::ReasoningConfig,
    catalog: &[String],
) -> Option<String> {
    let model = model.trim();
    if !model.starts_with("gemini-") {
        return None;
    }
    // Explicit tier id — the caller already chose; never rewrite.
    if is_tier_suffixed(model) {
        return None;
    }
    // Served as-is → no rewrite regardless of effort.
    if catalog.iter().any(|id| id == model) {
        return None;
    }
    tier_ladder(reasoning)
        .iter()
        .filter_map(|suffix| {
            let candidate = format!("{model}{suffix}");
            catalog
                .iter()
                .any(|id| id.eq_ignore_ascii_case(&candidate))
                .then_some(candidate)
        })
        .next()
}

/// Account model-catalog snapshot stashed in the credential meta by the auth
/// driver (exchange/refresh); drives the catalog-gated tier rewrite.
pub(crate) fn credential_catalog_snapshot(
    credential: Option<&crate::auth::types::StoredCredential>,
) -> Vec<String> {
    credential
        .and_then(|cred| cred.meta.get("subscription_models"))
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Effort-aware tier rewrite for the antigravity v1internal envelope
/// (pipeline step, runs after `post_encode`): a base-id gemini request
/// carrying a reasoning effort maps onto the tier variant the account
/// actually serves, and the inner `thinkingConfig` is dropped whenever the
/// model id carries the tier (the id is the effort on this surface).
pub(crate) fn apply_tier_model_rewrite(
    provider: &Provider,
    credential: Option<&crate::auth::types::StoredCredential>,
    reasoning: &crate::protocol::ir::ReasoningConfig,
    body: &mut Value,
) {
    if !is_google_antigravity(provider) {
        return;
    }
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .map(|model| model.trim().to_string())
    else {
        return;
    };
    let catalog = credential_catalog_snapshot(credential);
    if catalog.is_empty() {
        return;
    }
    let effective =
        resolve_tiered_model(&model, reasoning, &catalog).unwrap_or_else(|| model.clone());
    let tier_carried_by_id = is_tier_suffixed(&effective);
    if let Some(envelope) = body.as_object_mut() {
        if effective != model {
            envelope.insert("model".to_string(), Value::String(effective.clone()));
            tracing::debug!(
                client_model = %model,
                upstream_model = %effective,
                "antigravity tier model rewrite"
            );
        }
    }
    if tier_carried_by_id {
        // The id carries the effort now; avoid double-specifying it.
        if let Some(config) = body
            .pointer_mut("/request/generationConfig")
            .and_then(Value::as_object_mut)
        {
            config.remove("thinkingConfig");
        }
    }
}

/// Models the antigravity catalog advertises but the inference surface
/// cannot serve — probed 404 (retired id) or 400 (defunct id). Scoped to the
/// antigravity channel: the same preview ids may still live on other
/// surfaces (API-key / GCP Code Assist). Mirrors the curated-unavailable
/// pattern of the opencode-go channel — filtered from discovery/probe lists
/// only; manual model entries still route, so upstream revivals are never
/// blocked.
pub(crate) const GEMINI_SUBSCRIPTION_UNAVAILABLE_MODELS: &[&str] = &[
    // Retired preview/image ids — 404 NOT_FOUND on the v1internal surface.
    "gemini-2.5-flash-image",
    "gemini-3-flash-preview",
    "gemini-3-pro-preview",
    "gemini-3.1-pro-preview",
    // Defunct antigravity ids (CLIProxyAPI removed gemini-3-flash-agent from
    // its registry for the same reason; the 3.1 Pro high tier is served as
    // `gemini-pro-agent`, not as this id).
    "gemini-3-flash-agent",
    "gemini-3.1-pro-high",
];

/// Internal `chat_<digits>` experiment entries only accept their proprietary
/// completion dialect (400 on generateContent) — noise on any surface.
fn is_chat_noise(model: &str) -> bool {
    let model = model.trim();
    model.len() > 5
        && model[..5].eq_ignore_ascii_case("chat_")
        && model[5..].chars().all(|c| c.is_ascii_digit())
}

/// True when the id is known-unavailable on the Google subscription surface:
/// the curated list above or internal `chat_<digits>` noise.
pub(crate) fn is_unavailable_subscription_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty()
        && (GEMINI_SUBSCRIPTION_UNAVAILABLE_MODELS
            .iter()
            .any(|unavailable| unavailable.eq_ignore_ascii_case(model))
            || is_chat_noise(model))
}

/// Internal `tab_*_preview` entries: they sit in the subscription catalog and
/// report quota, but they serve the IDE's inline-completion dialect instead of
/// agent `generateContent`. Deliberately kept out of the curated unavailable
/// list — discovery and probing stay unchanged; only the usage view treats
/// them as noise.
fn is_tab_preview_noise(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.starts_with("tab_") && model.ends_with("_preview") && model.len() > "tab__preview".len()
}

/// True for catalog entries that carry quota yet cannot serve agent requests,
/// so a usage row for them says nothing about routable capacity: internal
/// `chat_<digits>` experiments and `tab_*_preview` completion models.
pub(crate) fn is_subscription_usage_noise(model: &str) -> bool {
    is_chat_noise(model) || is_tab_preview_noise(model)
}

/// Gemini's documented synthetic-history sentinel: tells the API to skip
/// thought-signature validation for a `functionCall` part. Needed whenever the
/// client protocol cannot carry the provider-issued signature back to us
/// (OpenAI/Anthropic history has no signature field).
pub(crate) const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

/// True when the model needs Gemini thought-signature handling (Gemini 3 and
/// later). Gemini 2.x, Claude and gpt-oss share the subscription surface but
/// have no thought signatures.
fn needs_thought_signatures(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("gemini-") && !model.starts_with("gemini-2")
}

/// Thought-signature replay policy for the Google subscription channels,
/// mirroring CLIProxyAPI's `SanitizeGeminiRequestThoughtSignatures`:
///
/// * the **first** `functionCall` of each model turn without a signature gets
///   the bypass sentinel (upstream rejects unsigned function calls otherwise:
///   `Function call is missing a thought_signature in functionCall parts`);
/// * sibling calls keep the native unsigned parallel-call shape (the sentinel
///   is only legal on the first call);
/// * `functionResponse` parts never carry a signature;
/// * a real provider signature already on the part is preserved untouched.
pub(crate) fn apply_thought_signature_policy(provider: &Provider, body: &mut Value) {
    if !is_google_antigravity(provider) && !super::gemini_cli::is_google_gemini_cli(provider) {
        return;
    }
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(ToString::to_string)
    else {
        return;
    };
    if !needs_thought_signatures(&model) {
        return;
    }
    let Some(contents) = body
        .pointer_mut("/request/contents")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for content in contents.iter_mut() {
        let is_model_turn = content
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| role.eq_ignore_ascii_case("model"));
        let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut first_call_seen = false;
        for part in parts.iter_mut() {
            let Some(object) = part.as_object_mut() else {
                continue;
            };
            if object.contains_key("functionResponse") {
                if object.remove("thoughtSignature").is_some() {
                    tracing::debug!("dropped thoughtSignature from a functionResponse part");
                }
                continue;
            }
            if !is_model_turn || !object.contains_key("functionCall") {
                continue;
            }
            let signature = object
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            let is_first = !first_call_seen;
            first_call_seen = true;
            if is_first {
                if signature.is_none() {
                    // The client protocol cannot replay the real signature;
                    // use Gemini's bypass sentinel.
                    object.insert(
                        "thoughtSignature".to_string(),
                        Value::String(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()),
                    );
                    tracing::debug!(
                        model = %model,
                        "injected skip-thought-signature sentinel on first functionCall"
                    );
                }
                continue;
            }
            // Sibling calls stay unsigned; a sentinel copied onto them by an
            // earlier hop is invalid upstream.
            if signature.as_deref() == Some(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR) {
                object.remove("thoughtSignature");
            }
        }
    }
}

/// Filter known-unavailable models from a discovered list — no-op for
/// providers that are not a Google subscription channel (mirrors the
/// opencode-go `visible_models` semantics). The curated exact list applies
/// to the antigravity channel only; `chat_` noise is dropped everywhere.
pub(crate) fn filter_subscription_unavailable(
    provider: &Provider,
    models: Vec<String>,
) -> Vec<String> {
    let is_antigravity = is_google_antigravity(provider);
    if !is_antigravity && !super::gemini_cli::is_google_gemini_cli(provider) {
        return models;
    }
    models
        .into_iter()
        .filter(|model| {
            if is_antigravity {
                // Curated list + chat noise.
                !is_unavailable_subscription_model(model)
            } else {
                // gemini-cli: only cross-surface chat noise.
                !is_chat_noise(model)
            }
        })
        .collect()
}

/// Read the companion project id from the OAuth credential metadata.
/// Shared by both Google subscription channels — the gemini-cli channel
/// stores its onboarded project under the same key.
pub(crate) fn code_assist_project_id(
    credential: Option<&crate::auth::types::StoredCredential>,
) -> Result<String> {
    credential
        .and_then(|cred| cred.meta.get("project_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "google subscription credential is missing project_id; \
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
        assert!(wrapped["requestId"].as_str().unwrap().starts_with("agent-"));
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
        assert!(
            !forces_upstream_stream(&provider),
            "channel alone must not flip a custom vendor"
        );
        provider.vendor = Some("google".into());
        provider.channel = Some("default".into());
        assert!(
            !forces_upstream_stream(&provider),
            "default channel keeps non-stream upstream"
        );
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
        let ids: Vec<String> = parse_available_models_detailed(&payload)
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "gemini-2.5-pro".to_string(),
                "gemini-3.1-pro-preview".to_string(),
                "gemini-3.6-flash".to_string(),
            ]
        );
        assert!(parse_available_models_detailed(&json!({})).is_empty());
        assert!(parse_available_models_detailed(&json!({"models": {}})).is_empty());
    }

    #[test]
    fn parse_available_models_detailed_carries_quota_state() {
        let payload = json!({
            "models": {
                "gemini-3.8-flash-high": {
                    "quotaInfo": {"remainingFraction": 0.0, "resetTime": "2026-05-22T07:00:00Z"}
                },
                "tab_flash_lite_preview": {
                    "quotaInfo": {"remainingFraction": 0.87}
                },
                "chat_20706": {},
            }
        });
        let detailed = parse_available_models_detailed(&payload);
        assert_eq!(detailed.len(), 3);
        assert_eq!(detailed[0].id, "chat_20706");
        assert!(detailed[0].quota_remaining.is_none());
        assert!(detailed[0].quota_resets_at.is_none());
        assert_eq!(detailed[1].id, "gemini-3.8-flash-high");
        assert_eq!(detailed[1].quota_remaining, Some(0.0));
        assert_eq!(
            detailed[1].quota_resets_at.as_deref(),
            Some("2026-05-22T07:00:00Z")
        );
        assert_eq!(detailed[2].id, "tab_flash_lite_preview");
        assert_eq!(detailed[2].quota_remaining, Some(0.87));
        assert!(detailed[2].quota_resets_at.is_none());
    }

    #[test]
    fn project_id_requires_credential_meta() {
        assert!(code_assist_project_id(None).is_err());
        let cred = crate::auth::types::StoredCredential {
            meta: json!({"project_id": "proj-x"}),
            ..Default::default()
        };
        assert_eq!(
            code_assist_project_id(Some(&cred)).unwrap(),
            "proj-x".to_string()
        );
    }

    #[test]
    fn parse_available_models_detailed_drops_deprecated_ids() {
        let payload = json!({
            "models": {
                "gemini-3-pro-preview": {},
                "gemini-3.1-pro-preview": {},
                "gemini-3.8-flash-high": {},
            },
            "deprecatedModelIds": {
                "gemini-3-pro-preview": {"newModelId": "gemini-3.1-pro-preview"},
                "gemini-3.1-pro-preview": {"newModelId": "gemini-pro-agent"},
            }
        });
        let ids: Vec<String> = parse_available_models_detailed(&payload)
            .into_iter()
            .map(|model| model.id)
            .collect();
        // Deprecated entries are dropped even though upstream still lists
        // them in `models` (they 404 on the inference surface).
        assert_eq!(ids, vec!["gemini-3.8-flash-high".to_string()]);
    }

    #[test]
    fn unavailable_models_match_curated_and_chat_noise() {
        for model in [
            "gemini-2.5-flash-image",
            "gemini-3-flash-preview",
            "gemini-3-pro-preview",
            "gemini-3.1-pro-preview",
            "gemini-3-flash-agent",
            "gemini-3.1-pro-high",
            "chat_20706",
            "chat_23310",
            "CHAT_9",
        ] {
            assert!(is_unavailable_subscription_model(model), "{model}");
        }
        // Live models — including transient failures like capacity-bound
        // gemini-2.5-pro — stay visible.
        for model in [
            "gemini-2.5-pro",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-tiered",
            "gemini-pro-agent",
            "tab_flash_lite_preview",
            "claude-sonnet-4-6",
            "gpt-oss-120b-medium",
            "chatgpt-4o",
        ] {
            assert!(!is_unavailable_subscription_model(model), "{model}");
        }
    }

    #[test]
    fn subscription_usage_noise_covers_chat_and_tab_preview_entries() {
        // Quota-bearing catalog entries that cannot serve agent requests.
        for model in [
            "chat_20706",
            "chat_23310",
            "tab_flash_lite_preview",
            "tab_jump_flash_lite_preview",
            "TAB_FLASH_LITE_PREVIEW",
        ] {
            assert!(is_subscription_usage_noise(model), "{model}");
        }
        // Callable models — and ids that merely look similar — stay visible.
        for model in [
            "gemini-3.8-flash",
            "gemini-2.5-pro",
            "gemini-3.1-flash-image",
            "gemini-pro-agent",
            "claude-sonnet-4-6",
            "gpt-oss-120b-medium",
            "chatgpt-4o",
            "tab_preview",
        ] {
            assert!(!is_subscription_usage_noise(model), "{model}");
        }
    }

    fn reasoning(
        effort: Option<crate::protocol::ir::ReasoningEffort>,
    ) -> crate::protocol::ir::ReasoningConfig {
        crate::protocol::ir::ReasoningConfig {
            enabled: true,
            budget_tokens: None,
            effort,
            display: None,
        }
    }

    #[test]
    fn tier_ladder_maps_effort_vocabulary_onto_catalog_variants() {
        use crate::protocol::ir::ReasoningEffort::{High, Low, Max, Medium, Minimal, Xhigh};
        let catalog: Vec<String> = [
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-tiered",
            "gemini-2.5-flash", // served as-is, no tier family
        ]
        .iter()
        .map(|id| id.to_string())
        .collect();

        let pick =
            |effort| resolve_tiered_model("gemini-3.8-flash", &reasoning(Some(effort)), &catalog);
        assert_eq!(pick(Low).as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!(pick(Medium).as_deref(), Some("gemini-3.8-flash-medium"));
        assert_eq!(pick(High).as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(pick(Xhigh).as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(pick(Max).as_deref(), Some("gemini-3.8-flash-high"));
        // No effort → adaptive tier.
        assert_eq!(
            resolve_tiered_model("gemini-3.8-flash", &reasoning(None), &catalog).as_deref(),
            Some("gemini-3.8-flash-tiered")
        );
        // minimal / none / disabled → lowest tier.
        assert_eq!(pick(Minimal).as_deref(), Some("gemini-3.8-flash-low"));
        let mut disabled = reasoning(Some(High));
        disabled.enabled = false;
        assert_eq!(
            resolve_tiered_model("gemini-3.8-flash", &disabled, &catalog).as_deref(),
            Some("gemini-3.8-flash-low")
        );
        assert_eq!(
            resolve_tiered_model(
                "gemini-3.8-flash",
                &reasoning(Some(crate::protocol::ir::ReasoningEffort::None)),
                &catalog
            )
            .as_deref(),
            Some("gemini-3.8-flash-low")
        );
        // Served base ids and explicit tier ids are never rewritten.
        assert!(
            resolve_tiered_model("gemini-2.5-flash", &reasoning(Some(High)), &catalog).is_none()
        );
        assert!(
            resolve_tiered_model("gemini-3.8-flash-high", &reasoning(Some(Low)), &catalog)
                .is_none()
        );
        // Non-gemini families never rewrite.
        assert!(
            resolve_tiered_model("claude-sonnet-4-6", &reasoning(Some(High)), &catalog).is_none()
        );
    }

    #[test]
    fn tier_ladder_falls_back_to_nearest_available_variant() {
        use crate::protocol::ir::ReasoningEffort::{High, Medium};
        // gemini-3.5-flash family only serves -low / -extra-low.
        let catalog: Vec<String> = ["gemini-3.5-flash-low", "gemini-3.5-flash-extra-low"]
            .iter()
            .map(|id| id.to_string())
            .collect();
        assert_eq!(
            resolve_tiered_model("gemini-3.5-flash", &reasoning(Some(Medium)), &catalog).as_deref(),
            Some("gemini-3.5-flash-low")
        );
        assert_eq!(
            resolve_tiered_model("gemini-3.5-flash", &reasoning(Some(High)), &catalog).as_deref(),
            Some("gemini-3.5-flash-low")
        );
        // Families with no tier variants at all fail safe: no rewrite.
        let thin: Vec<String> = vec![];
        assert!(resolve_tiered_model("gemini-2.5-pro", &reasoning(Some(High)), &thin).is_none());
        // Budget mapping: 1024→low, 4096→medium, 32000→high.
        let full: Vec<String> = [
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-high",
        ]
        .iter()
        .map(|id| id.to_string())
        .collect();
        for (tokens, expected) in [
            (1024u32, "gemini-3.8-flash-low"),
            (4096, "gemini-3.8-flash-medium"),
            (32000, "gemini-3.8-flash-high"),
        ] {
            let cfg = crate::protocol::ir::ReasoningConfig {
                enabled: true,
                budget_tokens: Some(tokens),
                effort: Some(crate::protocol::ir::ReasoningEffort::Budget(tokens)),
                display: None,
            };
            assert_eq!(
                resolve_tiered_model("gemini-3.8-flash", &cfg, &full).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn apply_tier_model_rewrite_patches_envelope_and_strips_thinking_config() {
        let provider = crate::db::models::Provider {
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
        let credential = crate::auth::types::StoredCredential {
            access_token: Some("ya29".into()),
            meta: json!({
                "project_id": "proj",
                "subscription_models": [
                    "gemini-3.8-flash-low", "gemini-3.8-flash-medium",
                    "gemini-3.8-flash-high", "gemini-3.8-flash-tiered",
                ],
            }),
            ..Default::default()
        };
        let mut body = json!({
            "model": "gemini-3.8-flash",
            "project": "proj",
            "request": {
                "contents": [],
                "generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}},
            },
        });
        apply_tier_model_rewrite(
            &provider,
            Some(&credential),
            &reasoning(Some(crate::protocol::ir::ReasoningEffort::High)),
            &mut body,
        );
        assert_eq!(body["model"], "gemini-3.8-flash-high");
        assert!(
            body["request"]["generationConfig"]
                .get("thinkingConfig")
                .is_none()
        );

        // Explicit tier id: model untouched, inner thinkingConfig still stripped.
        let mut explicit = json!({
            "model": "gemini-3.8-flash-tiered",
            "request": {"generationConfig": {"thinkingConfig": {"thinkingLevel": "low"}}},
        });
        apply_tier_model_rewrite(
            &provider,
            Some(&credential),
            &reasoning(Some(crate::protocol::ir::ReasoningEffort::High)),
            &mut explicit,
        );
        assert_eq!(explicit["model"], "gemini-3.8-flash-tiered");
        assert!(
            explicit["request"]["generationConfig"]
                .get("thinkingConfig")
                .is_none()
        );
    }

    fn google_provider(channel: &str, model: &str) -> (crate::db::models::Provider, Value) {
        let provider = crate::db::models::Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some("google".into()),
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
        };
        let body = json!({
            "model": model,
            "project": "proj",
            "request": {
                "contents": [
                    {"role": "user", "parts": [{"text": "list files"}]},
                    {"role": "model", "parts": [{"functionCall": {"name": "bash", "args": {"cmd": "ls"}}}]},
                    {"role": "user", "parts": [{"functionResponse": {"name": "bash", "response": {"out": "a.txt"}}, "thoughtSignature": "leaked"}]}
                ]
            }
        });
        (provider, body)
    }

    #[test]
    fn thought_signature_policy_fills_first_function_call_only() {
        let (provider, mut body) = google_provider("antigravity", "gemini-3.8-flash-tiered");
        apply_thought_signature_policy(&provider, &mut body);
        let parts = body["request"]["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(
            parts[0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        // functionResponse parts must never carry a signature.
        assert!(
            body["request"]["contents"][2]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );
    }

    #[test]
    fn thought_signature_policy_keeps_real_signatures_and_clears_sibling_sentinels() {
        let (provider, mut body) = google_provider("antigravity", "gemini-3.8-flash-tiered");
        body["request"]["contents"][1]["parts"] = json!([
            {"functionCall": {"name": "bash", "args": {"cmd": "ls"}}, "thoughtSignature": "REAL-SIG"},
            {"functionCall": {"name": "read", "args": {"p": "a"}}, "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR},
        ]);
        apply_thought_signature_policy(&provider, &mut body);
        let parts = body["request"]["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["thoughtSignature"], "REAL-SIG");
        // Sibling calls keep the native unsigned shape.
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn thought_signature_policy_scopes_to_gemini3_subscription_requests() {
        // Gemini 2.x history on the same surface stays untouched.
        let (provider, mut legacy) = google_provider("antigravity", "gemini-2.5-pro");
        apply_thought_signature_policy(&provider, &mut legacy);
        assert!(
            legacy["request"]["contents"][1]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );

        // Non-Gemini models on the subscription surface stay untouched.
        let (provider, mut claude) = google_provider("antigravity", "claude-sonnet-4-6");
        apply_thought_signature_policy(&provider, &mut claude);
        assert!(
            claude["request"]["contents"][1]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );

        // Non-subscription providers are never rewritten.
        let mut other = provider.clone();
        other.vendor = Some("custom".into());
        other.channel = None;
        let (_, mut untouched) = google_provider("antigravity", "gemini-3.8-flash-tiered");
        apply_thought_signature_policy(&other, &mut untouched);
        assert!(
            untouched["request"]["contents"][1]["parts"][0]
                .get("thoughtSignature")
                .is_none()
        );

        // The gemini-cli channel shares the policy.
        let (provider, mut gemini_cli) = google_provider("gemini-cli", "gemini-3.6-flash");
        apply_thought_signature_policy(&provider, &mut gemini_cli);
        assert_eq!(
            gemini_cli["request"]["contents"][1]["parts"][0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
    }

    #[test]
    fn filter_subscription_unavailable_scopes_to_google_channels() {
        let models = vec![
            "chat_20706".to_string(),
            "gemini-3-pro-preview".to_string(),
            "gemini-3.8-flash-high".to_string(),
        ];
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
        // Antigravity channel: curated 404/defunct ids + chat noise dropped.
        assert_eq!(
            filter_subscription_unavailable(&provider, models.clone()),
            vec!["gemini-3.8-flash-high".to_string()]
        );
        // Gemini-cli channel: the preview ids may still live there — only
        // chat noise is dropped.
        provider.channel = Some("gemini-cli".into());
        assert_eq!(
            filter_subscription_unavailable(&provider, models.clone()),
            vec![
                "gemini-3-pro-preview".to_string(),
                "gemini-3.8-flash-high".to_string()
            ]
        );
        // Non-google providers keep their lists untouched.
        provider.vendor = Some("custom".into());
        provider.channel = None;
        assert_eq!(
            filter_subscription_unavailable(&provider, models.clone()),
            models
        );
    }
}
