//! OpenCode Go routing identity (`x-opencode-session`).
//!
//! The Go surface requires every request to say which conversation it belongs
//! to: <https://opencode.ai/docs/go/#where-can-i-use-it> asks clients to "send
//! a stable session ID in `x-opencode-session` for each conversation so we can
//! optimize routing and prompt caching", and a request without one is rejected
//! outright:
//!
//! ```text
//! HTTP 400 {"type":"error","error":{"type":"MissingSessionID","message":
//!   "Error from provider (Console Go): Request is missing x-opencode-session
//!    and cannot be routed efficiently."}}
//! ```
//!
//! The id must be **stable across the turns of one conversation** (that is what
//! buys routing and prompt-cache affinity) and **distinct between
//! conversations**, so nyro fingerprints the conversation's opening turn — the
//! same technique `google/antigravity` uses for its `sessionId`. A
//! client-supplied header always wins: [`apply_egress_headers`] never
//! overwrites one, so native OpenCode (and Claude Code / Codex, whose own
//! session headers Go recognizes) keep their own identity while passing
//! through the gateway.

use reqwest::header::HeaderMap;
use sha2::{Digest, Sha256};

use crate::db::models::Provider;
use crate::protocol::ir::{AiRequest, Role};

/// Header the Go surface routes and prompt-caches on.
pub(crate) const SESSION_HEADER: &str = "x-opencode-session";

/// Domain separation prefix for every derived id, so a fingerprint can never
/// collide with an id derived for another purpose from the same bytes.
const SESSION_DOMAIN: &[u8] = b"nyro-opencode-session-v1";

/// True when the target is the OpenCode Go surface.
///
/// `vendor` is optional because callers that sit below provider identity (the
/// admin model probe builds its requests from a resolved base URL) can only
/// match on the URL; the base URL is the most stable cross-configuration
/// signal either way, mirroring the Volcengine Ark detection in the shared
/// pipeline.
pub(crate) fn is_opencode_go_target(vendor: Option<&str>, base_url: &str) -> bool {
    let vendor = vendor.map(str::trim).unwrap_or_default();
    vendor.eq_ignore_ascii_case("opencode-go") || base_url.contains("opencode.ai/zen/go")
}

/// True when the provider row targets the OpenCode Go surface.
pub(crate) fn is_opencode_go(provider: &Provider) -> bool {
    is_opencode_go_target(provider.vendor.as_deref(), &provider.base_url)
}

/// Stable per-conversation routing identity, derived from the conversation's
/// opening turn (system prompt + first user message).
///
/// Growing the conversation appends messages *after* that opening turn, so the
/// id survives from the first request of a session to its last. A request with
/// no user text yet (a tool-result-only continuation, say) has nothing stable
/// to fingerprint and falls back to a fresh id — the request still needs *an*
/// identity to be routable at all.
pub(crate) fn conversation_session_id(req: &AiRequest) -> String {
    let opening = req
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(|message| message.content.to_text())
        .filter(|text| !text.trim().is_empty());

    let Some(opening) = opening else {
        return format!("nyro-{}", uuid::Uuid::new_v4().simple());
    };

    let mut hasher = Sha256::new();
    hasher.update(SESSION_DOMAIN);
    if let Some(system) = req.system.as_deref() {
        hasher.update(b"\x00system\x00");
        hasher.update(system.as_bytes());
    }
    hasher.update(b"\x00user\x00");
    hasher.update(opening.as_bytes());
    format!("nyro-{}", hex::encode(&hasher.finalize()[..16]))
}

/// Deterministic identity for a caller-chosen seed.
///
/// Used by the admin model probe, which synthesizes its own "hi" body: seeding
/// on the model keeps repeated runs on the same id without letting probes
/// collide with each other or with a real conversation.
pub(crate) fn seeded_session_id(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SESSION_DOMAIN);
    hasher.update(b"\x00seed\x00");
    hasher.update(seed.as_bytes());
    format!("nyro-{}", hex::encode(&hasher.finalize()[..16]))
}

/// Add the routing identity to outbound `headers` unless one is already there.
///
/// Applied at the single egress choke point in the dispatcher, after adapter,
/// runtime-binding and forwarded client headers have been merged, so every
/// request-build path (native passthrough, IR encode, raw-wire compat) carries
/// it and a client-supplied value is preserved.
pub(crate) fn apply_egress_headers(headers: &mut HeaderMap, provider: &Provider, req: &AiRequest) {
    if !is_opencode_go(provider) || headers.contains_key(SESSION_HEADER) {
        return;
    }
    insert_session_header(headers, conversation_session_id(req));
}

/// Probe variant: match on the resolved base URL and seed on `model`.
pub(crate) fn apply_probe_session_header(headers: &mut HeaderMap, base_url: &str, model: &str) {
    if !is_opencode_go_target(None, base_url) || headers.contains_key(SESSION_HEADER) {
        return;
    }
    insert_session_header(headers, seeded_session_id(&format!("admin-probe:{model}")));
}

fn insert_session_header(headers: &mut HeaderMap, session_id: String) {
    // Derived ids are hex, so this only guards a caller-supplied seed.
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&session_id) {
        headers.insert(SESSION_HEADER, value);
    }
}

/// Seeded variant for egress callers that synthesize their own body and so
/// cannot fingerprint a conversation (the vision shim's helper calls). Returns
/// `None` for every other provider, so callers can attach the value directly
/// to a request builder.
pub(crate) fn seeded_egress_session_id(provider: &Provider, seed: &str) -> Option<String> {
    is_opencode_go(provider).then(|| seeded_session_id(seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{Message, MessageContent};

    fn provider(vendor: &str, base_url: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some(vendor.into()),
            protocol: "openai-compatible".into(),
            base_url: base_url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "sk-test".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn request(system: Option<&str>, turns: &[&str]) -> AiRequest {
        let messages = turns
            .iter()
            .map(|text| Message {
                role: Role::User,
                content: MessageContent::Text((*text).to_string()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            })
            .collect();
        let mut req = AiRequest::new("glm-5.3", messages);
        req.system = system.map(str::to_string);
        req
    }

    fn session_of(headers: &HeaderMap) -> String {
        headers
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("session header present")
            .to_string()
    }

    #[test]
    fn matches_vendor_id_or_go_base_url() {
        assert!(is_opencode_go(&provider(
            "opencode-go",
            "https://example.com"
        )));
        assert!(is_opencode_go(&provider(
            "custom",
            "https://opencode.ai/zen/go"
        )));
        assert!(!is_opencode_go(&provider(
            "zhipuai",
            "https://open.bigmodel.cn"
        )));
    }

    #[test]
    fn session_survives_conversation_growth() {
        let first = conversation_session_id(&request(Some("be terse"), &["hello"]));
        // Same opening turn, more history appended -> same routing identity.
        let grown = conversation_session_id(&request(Some("be terse"), &["hello", "more"]));
        assert_eq!(first, grown);
        assert!(first.starts_with("nyro-"));
    }

    #[test]
    fn session_differs_between_conversations_and_systems() {
        let a = conversation_session_id(&request(Some("be terse"), &["hello"]));
        let b = conversation_session_id(&request(Some("be terse"), &["goodbye"]));
        let c = conversation_session_id(&request(Some("be verbose"), &["hello"]));
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn session_falls_back_when_no_user_turn_exists() {
        let req = request(None, &[]);
        let id = conversation_session_id(&req);
        assert!(id.starts_with("nyro-"), "id: {id}");
        assert_ne!(id, conversation_session_id(&req));
    }

    #[test]
    fn egress_headers_are_scoped_to_opencode_go() {
        let req = request(Some("sys"), &["hi"]);

        let mut other = HeaderMap::new();
        apply_egress_headers(
            &mut other,
            &provider("zhipuai", "https://open.bigmodel.cn"),
            &req,
        );
        assert!(other.is_empty());

        let mut headers = HeaderMap::new();
        apply_egress_headers(
            &mut headers,
            &provider("opencode-go", "https://opencode.ai/zen/go"),
            &req,
        );
        assert_eq!(session_of(&headers), conversation_session_id(&req));
    }

    #[test]
    fn client_supplied_session_wins() {
        let req = request(Some("sys"), &["hi"]);
        let mut headers = HeaderMap::new();
        headers.insert(SESSION_HEADER, "client-session-1".parse().unwrap());

        apply_egress_headers(
            &mut headers,
            &provider("opencode-go", "https://opencode.ai/zen/go"),
            &req,
        );

        assert_eq!(session_of(&headers), "client-session-1");
    }

    #[test]
    fn probe_session_is_per_model_and_stable() {
        let mut first = HeaderMap::new();
        apply_probe_session_header(&mut first, "https://opencode.ai/zen/go", "glm-5.3");
        let mut again = HeaderMap::new();
        apply_probe_session_header(&mut again, "https://opencode.ai/zen/go", "glm-5.3");
        let mut other_model = HeaderMap::new();
        apply_probe_session_header(&mut other_model, "https://opencode.ai/zen/go", "kimi-k3");

        assert_eq!(session_of(&first), session_of(&again));
        assert_ne!(session_of(&first), session_of(&other_model));

        let mut unrelated = HeaderMap::new();
        apply_probe_session_header(&mut unrelated, "https://api.openai.com/v1", "gpt-5");
        assert!(unrelated.is_empty());
    }

    #[test]
    fn probe_session_does_not_shadow_a_supplied_value() {
        let mut headers = HeaderMap::new();
        headers.insert(SESSION_HEADER, "runtime-owned".parse().unwrap());
        apply_probe_session_header(&mut headers, "https://opencode.ai/zen/go", "glm-5.3");
        assert_eq!(session_of(&headers), "runtime-owned");
    }

    #[test]
    fn seeded_egress_id_is_scoped_and_deterministic() {
        let go = provider("opencode-go", "https://opencode.ai/zen/go");
        let other = provider("minimax", "https://api.minimax.io");

        assert_eq!(
            seeded_egress_session_id(&go, "vision-shim:minimax-m3"),
            seeded_egress_session_id(&go, "vision-shim:minimax-m3"),
        );
        assert_ne!(
            seeded_egress_session_id(&go, "vision-shim:minimax-m3"),
            seeded_egress_session_id(&go, "vision-shim:glm-5.3"),
        );
        assert_eq!(seeded_egress_session_id(&other, "seed"), None);
    }
}
