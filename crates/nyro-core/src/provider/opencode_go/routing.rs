//! OpenCode Go model → endpoint routing.
//!
//! The Go surface does not serve every model on every endpoint, and an
//! endpoint that does not serve a model answers with an opaque HTTP 500
//! `{"type":"error","error":{"type":"error","message":"Internal server
//! error"}}` — indistinguishable from a transient upstream failure. So the
//! mapping cannot be discovered from error codes: it is hardcoded here from
//! the measured matrix.
//!
//! Rules, in priority order:
//!
//! 1. **The client's protocol wins when the model supports it** — a Claude
//!    Code (anthropic) or Codex (responses) caller keeps its native wire
//!    format, so prompt caching and thinking signatures stay intact.
//! 2. **Otherwise the model's own endpoint wins** — a chat-only caller asking
//!    for `grok-4.6` is transcoded onto `/v1/responses`, and a Claude Code
//!    caller asking for `glm-5.3` is transcoded onto `/v1/chat/completions`.
//!    Without this downgrade an adaptive provider would pick the ingress
//!    protocol and 500.
//! 3. **A model that is not listed is assumed chat-only** — `/v1/chat/
//!    completions` is the default endpoint, so this default keeps unknown
//!    models working for the common case instead of silently sending them to
//!    an endpoint that does not serve them.
//!
//! Matching is exact (`trim` + case-insensitive) against the *upstream* model
//! name, so a future variant of a listed family is never hijacked by a prefix
//! rule; add it explicitly when it appears.

use crate::db::models::Provider;
use crate::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
    ProtocolId,
};

use super::session::is_opencode_go;

const CHAT: ProtocolId = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
const RESPONSES: ProtocolId = OPENAI_RESPONSES_V1;
const MESSAGES: ProtocolId = ANTHROPIC_MESSAGES_2023_06_01;

/// `/v1/chat/completions` and `/v1/messages` both reject these models.
const RESPONSES_ONLY: &[ProtocolId] = &[RESPONSES];
/// `/v1/chat/completions` and `/v1/responses` both reject these models.
const MESSAGES_ONLY: &[ProtocolId] = &[MESSAGES];

/// Models whose endpoint set differs from the chat-only default. Everything
/// else — including models added upstream after this table was written — is
/// treated as chat-only by [`supports`].
const MODEL_ENDPOINTS: &[(&str, &[ProtocolId])] = &[
    // Responses only.
    ("gpt-5.6-luna", RESPONSES_ONLY),
    ("grok-4.6", RESPONSES_ONLY),
    ("muse-spark-1.2-contributor", RESPONSES_ONLY),
    ("muse-spark-1.3-contributor", RESPONSES_ONLY),
    // Anthropic messages only.
    ("minimax-m2.5", MESSAGES_ONLY),
    ("minimax-m2.7", MESSAGES_ONLY),
    ("minimax-m3", MESSAGES_ONLY),
    ("qwen3.6-plus", MESSAGES_ONLY),
    ("qwen3.7-max", MESSAGES_ONLY),
    ("qwen3.7-plus", MESSAGES_ONLY),
    ("qwen3.8-flash", MESSAGES_ONLY),
    ("qwen3.8-max", MESSAGES_ONLY),
];

/// Models the upstream still advertises in `GET /v1/models` but does not serve
/// on this plan: every endpoint answers `Model is unavailable` (HTTP 400) or
/// errors out. They are filtered out of the provider's model list so the route
/// target pickers never offer them; typing the name by hand still reaches the
/// upstream, so a model that comes back to life is not blocked by stale code.
///
/// Revisit when Go re-enables any of them (`glm-5`, `hy3-preview`, `qwen3.5-plus`
/// and `kimi-k2.5` are plausible returnees as newer siblings ship).
const UNAVAILABLE_MODELS: &[&str] = &[
    "glm-5",
    "grok-4.5",
    "hy3-preview",
    "kimi-k2.5",
    "mimo-v2-omni",
    "mimo-v2-pro",
    "qwen3.5-plus",
];

/// True when the Go plan serves `model` on `protocol`.
///
/// Unlisted models are chat-only (see the module docs).
pub(crate) fn supports(model: &str, protocol: ProtocolId) -> bool {
    let model = normalize(model);
    if model.is_empty() {
        return false;
    }
    match MODEL_ENDPOINTS.iter().find(|(listed, _)| *listed == model) {
        Some((_, protocols)) => protocols.contains(&protocol),
        None => protocol == CHAT,
    }
}

/// The endpoint a request for `model` should be transcoded onto when the
/// client's own protocol is not served: chat when the model serves it,
/// otherwise the first of its declared endpoints in table order.
pub(crate) fn primary_protocol(model: &str) -> ProtocolId {
    MODEL_ENDPOINTS
        .iter()
        .find(|(listed, _)| *listed == normalize(model))
        .and_then(|(_, protocols)| protocols.first().copied())
        .unwrap_or(CHAT)
}

/// Egress-protocol preference for an OpenCode Go request.
///
/// `None` means "no opinion": the caller is not an OpenCode Go provider, or
/// the model already serves the client's protocol and must stay native.
/// `Some(protocol)` asks `negotiate()` for that endpoint; the preference is
/// only honoured when the provider actually declares the endpoint, so a
/// provider still configured as fixed chat-only degrades to today's behaviour.
pub(crate) fn preferred_egress(
    provider: &Provider,
    actual_model: &str,
    ingress: ProtocolId,
) -> Option<ProtocolId> {
    if !is_opencode_go(provider) || supports(actual_model, ingress) {
        return None;
    }
    Some(primary_protocol(actual_model))
}

/// Drop models the Go plan does not serve from a discovered model list.
pub(crate) fn filter_models(models: Vec<String>) -> Vec<String> {
    models
        .into_iter()
        .filter(|model| !is_unavailable(model))
        .collect()
}

/// Vendor-scoped wrapper for the model-list surfaces: strips models the plan
/// does not serve, for OpenCode Go providers only.
pub(crate) fn visible_models(provider: &Provider, models: Vec<String>) -> Vec<String> {
    if !is_opencode_go(provider) {
        return models;
    }
    filter_models(models)
}

/// True when the upstream advertises `model` but does not serve it on this plan.
pub(crate) fn is_unavailable(model: &str) -> bool {
    let model = normalize(model);
    !model.is_empty() && UNAVAILABLE_MODELS.contains(&model.as_str())
}

fn normalize(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(vendor: &str, base_url: &str, mode: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some(vendor.into()),
            protocol: "openai-compatible".into(),
            base_url: base_url.into(),
            protocol_mode: mode.into(),
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

    fn go_provider() -> Provider {
        provider("opencode-go", "https://opencode.ai/zen/go", "adaptive")
    }

    #[test]
    fn listed_models_expose_only_their_measured_endpoints() {
        // DeepSeek is chat-only under current user specs
        assert!(supports("deepseek-v4-pro", CHAT));
        assert!(!supports("deepseek-v4-pro", RESPONSES));
        assert!(!supports("deepseek-v4-pro", MESSAGES));

        // Kimi K3 is chat-only
        assert!(supports("kimi-k3", CHAT));
        assert!(!supports("kimi-k3", MESSAGES));
        assert!(!supports("kimi-k3", RESPONSES));

        // Grok is responses-only
        assert!(supports("grok-4.6", RESPONSES));
        assert!(!supports("grok-4.6", CHAT));
        assert!(!supports("grok-4.6", MESSAGES));

        // MiniMax & Qwen are messages-only
        assert!(supports("minimax-m2.7", MESSAGES));
        assert!(!supports("minimax-m2.7", CHAT));
        assert!(supports("minimax-m3", MESSAGES));
        assert!(!supports("minimax-m3", CHAT));
        assert!(supports("qwen3.8-max", MESSAGES));
        assert!(!supports("qwen3.8-max", CHAT));
    }

    #[test]
    fn unlisted_models_are_chat_only() {
        assert!(supports("glm-5.3", CHAT));
        assert!(!supports("glm-5.3", RESPONSES));
        assert!(!supports("glm-5.3", MESSAGES));
        assert!(supports("hy3", CHAT));
        assert!(supports("hy4-preview", CHAT));
        // Unknown / future models default to chat.
        assert!(supports("glm-6-turbo", CHAT));
        assert!(!supports("glm-6-turbo", RESPONSES));
        assert!(!supports("", CHAT));
    }

    #[test]
    fn matching_is_trimmed_case_insensitive_and_exact() {
        assert!(supports("  Grok-4.6 ", RESPONSES));
        assert!(!supports("grok-4.6-preview", RESPONSES));
        assert!(supports("grok-4.6-preview", CHAT));
        assert_eq!(primary_protocol("MINIMAX-M2.7"), MESSAGES);
        assert_eq!(primary_protocol("QWEN3.8-MAX"), MESSAGES);
    }

    #[test]
    fn primary_endpoint_prefers_chat_then_table_order() {
        assert_eq!(primary_protocol("glm-5.3"), CHAT);
        assert_eq!(primary_protocol("kimi-k3"), CHAT);
        assert_eq!(primary_protocol("minimax-m2.7"), MESSAGES);
        assert_eq!(primary_protocol("minimax-m3"), MESSAGES);
        assert_eq!(primary_protocol("qwen3.8-max"), MESSAGES);
        assert_eq!(primary_protocol("grok-4.6"), RESPONSES);
    }

    #[test]
    fn client_protocol_wins_when_the_model_serves_it() {
        // Claude Code asking a messages-capable model stays native.
        assert_eq!(
            preferred_egress(&go_provider(), "minimax-m3", MESSAGES),
            None
        );
        assert_eq!(
            preferred_egress(&go_provider(), "qwen3.8-max", MESSAGES),
            None
        );
        // Codex asking a responses-capable model stays native.
        assert_eq!(
            preferred_egress(&go_provider(), "grok-4.6", RESPONSES),
            None
        );
        // Chat asking a chat model stays native.
        assert_eq!(preferred_egress(&go_provider(), "glm-5.3", CHAT), None);
        assert_eq!(preferred_egress(&go_provider(), "kimi-k3", CHAT), None);
    }

    #[test]
    fn table_endpoint_takes_over_when_the_ingress_protocol_is_not_served() {
        // Chat client asking a responses-only model.
        assert_eq!(
            preferred_egress(&go_provider(), "grok-4.6", CHAT),
            Some(RESPONSES)
        );
        // Claude Code asking a responses-only model.
        assert_eq!(
            preferred_egress(&go_provider(), "gpt-5.6-luna", MESSAGES),
            Some(RESPONSES)
        );
        // Claude Code asking a chat-only model must not go native.
        assert_eq!(
            preferred_egress(&go_provider(), "glm-5.3", MESSAGES),
            Some(CHAT)
        );
        assert_eq!(
            preferred_egress(&go_provider(), "kimi-k3", MESSAGES),
            Some(CHAT)
        );
        // Codex asking a chat-only model.
        assert_eq!(
            preferred_egress(&go_provider(), "longcat-2.0", RESPONSES),
            Some(CHAT)
        );
        // Chat asking a messages-only model.
        assert_eq!(
            preferred_egress(&go_provider(), "minimax-m3", CHAT),
            Some(MESSAGES)
        );
        assert_eq!(
            preferred_egress(&go_provider(), "qwen3.8-max", CHAT),
            Some(MESSAGES)
        );
    }

    #[test]
    fn non_opencode_provider_has_no_opinion() {
        let other = provider("openai", "https://api.openai.com", "fixed");
        assert_eq!(preferred_egress(&other, "grok-4.6", CHAT), None);
    }

    #[test]
    fn filter_models_strips_only_the_declared_unavailable_set() {
        let models = vec![
            "glm-5.3".into(),
            "glm-5".into(),
            "grok-4.6".into(),
            "grok-4.5".into(),
            "qwen3.5-plus".into(),
            "qwen3.8-max".into(),
            "hy3".into(),
            "hy4-preview".into(),
        ];
        let visible = filter_models(models);
        assert_eq!(
            visible,
            vec!["glm-5.3", "grok-4.6", "qwen3.8-max", "hy3", "hy4-preview"]
        );
    }
}
