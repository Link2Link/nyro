//! OpenCode Go model → endpoint routing.
//!
//! The Go surface does not serve every model on every endpoint, and an
//! endpoint that does not serve a model answers with an opaque HTTP 500
//! `{"type":"error","error":{"type":"error","message":"Internal server
//! error"}}` — indistinguishable from a transient upstream failure. So the
//! mapping cannot be discovered from error codes: it is hardcoded here from
//! OpenCode's own per-model SDK binding (`@ai-sdk/openai` → `/v1/responses`,
//! `@ai-sdk/openai-compatible` → `/v1/chat/completions`, `@ai-sdk/anthropic`
//! → `/v1/messages`).
//!
//! Rules, in priority order:
//!
//! 1. **The model's pinned protocol always wins** — a Claude Code (anthropic)
//!    or Codex (responses) caller asking for `kimi-k3` is transcoded onto
//!    `/v1/chat/completions`, and a chat caller asking for `grok-4.6` is
//!    transcoded onto `/v1/responses`. Matching ingress stays native inside
//!    `negotiate()`; this module still returns the pin so adaptive providers
//!    never fall through to "use the client's protocol".
//! 2. **A model that is not listed is assumed chat-only** — `/v1/chat/
//!    completions` is the default endpoint, so this default keeps unknown
//!    models working for the common case instead of silently sending them to
//!    an endpoint that does not serve them.
//! 3. **The preference is only honoured when the provider declares the
//!    endpoint** — a provider still configured as fixed chat-only degrades to
//!    today's behaviour. There is no endpoint-level fallback: once chosen,
//!    upstream errors surface as-is.
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

/// OpenCode Go catalog: each listed model is served on exactly one endpoint.
/// Everything else — including models added upstream after this table was
/// written — is treated as chat-only by [`primary_protocol`].
const MODEL_PROTOCOL: &[(&str, ProtocolId)] = &[
    // Responses only (`@ai-sdk/openai` → `/v1/responses`).
    ("gpt-5.6-luna", RESPONSES),
    ("grok-4.6", RESPONSES),
    ("muse-spark-1.2-contributor", RESPONSES),
    ("muse-spark-1.3-contributor", RESPONSES),
    // Anthropic messages only (`@ai-sdk/anthropic` → `/v1/messages`).
    ("minimax-m2.5", MESSAGES),
    ("minimax-m2.7", MESSAGES),
    ("minimax-m3", MESSAGES),
    ("qwen3.6-plus", MESSAGES),
    ("qwen3.7-max", MESSAGES),
    ("qwen3.7-plus", MESSAGES),
    ("qwen3.8-flash", MESSAGES),
    ("qwen3.8-max", MESSAGES),
    // Chat only (`@ai-sdk/openai-compatible` → `/v1/chat/completions`).
    ("deepseek-v4-flash", CHAT),
    ("deepseek-v4-flash-vision-exp", CHAT),
    ("deepseek-v4-pro", CHAT),
    ("deepseek-v4.1-flash", CHAT),
    ("glm-5.1", CHAT),
    ("glm-5.2", CHAT),
    ("glm-5.3", CHAT),
    ("glm-5.3-flash", CHAT),
    ("hy3", CHAT),
    ("hy4-preview", CHAT),
    ("kimi-k2.6", CHAT),
    ("kimi-k2.7-code", CHAT),
    ("kimi-k3", CHAT),
    ("longcat-2.0", CHAT),
    ("mimo-v2.5", CHAT),
    ("mimo-v2.5-pro", CHAT),
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

fn listed_protocol(model: &str) -> Option<ProtocolId> {
    let model = normalize(model);
    MODEL_PROTOCOL
        .iter()
        .find(|(listed, _)| *listed == model)
        .map(|(_, protocol)| *protocol)
}

/// True when the Go plan serves `model` on `protocol`.
///
/// Unlisted models are chat-only (see the module docs).
#[cfg(test)]
pub(crate) fn supports(model: &str, protocol: ProtocolId) -> bool {
    if normalize(model).is_empty() {
        return false;
    }
    listed_protocol(model).unwrap_or(CHAT) == protocol
}

/// The endpoint a request for `model` must be transcoded onto: the catalog
/// pin when listed, otherwise chat.
pub(crate) fn primary_protocol(model: &str) -> ProtocolId {
    listed_protocol(model).unwrap_or(CHAT)
}

/// Egress-protocol preference for an OpenCode Go request.
///
/// `None` means "no opinion": the caller is not an OpenCode Go provider.
/// `Some(protocol)` asks `negotiate()` for that endpoint even when it already
/// matches the client's protocol, so adaptive providers never fall through to
/// ingress-driven resolution. The preference is only honoured when the
/// provider actually declares the endpoint, so a provider still configured as
/// fixed chat-only degrades to today's behaviour.
pub(crate) fn preferred_egress(
    provider: &Provider,
    actual_model: &str,
    _ingress: ProtocolId,
) -> Option<ProtocolId> {
    if !is_opencode_go(provider) {
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
    fn catalog_pins_each_listed_model_to_exactly_one_protocol() {
        for (model, pinned) in MODEL_PROTOCOL {
            assert_eq!(
                primary_protocol(model),
                *pinned,
                "{model} must pin to {pinned}"
            );
            assert!(
                supports(model, *pinned),
                "{model} must support its pinned protocol {pinned}"
            );
            for other in [CHAT, RESPONSES, MESSAGES] {
                if other == *pinned {
                    continue;
                }
                assert!(
                    !supports(model, other),
                    "{model} must not also support {other}"
                );
            }
        }
        assert_eq!(
            MODEL_PROTOCOL.len(),
            28,
            "keep the OpenCode Go catalog exhaustive"
        );
    }

    #[test]
    fn listed_models_expose_only_their_measured_endpoints() {
        // DeepSeek is chat-only
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
        assert!(supports("glm-6-turbo", CHAT));
        assert!(!supports("glm-6-turbo", RESPONSES));
        assert!(!supports("glm-6-turbo", MESSAGES));
        assert!(supports("grok-4.6-preview", CHAT));
        assert!(!supports("grok-4.6-preview", RESPONSES));
        assert!(!supports("", CHAT));
    }

    #[test]
    fn matching_is_trimmed_case_insensitive_and_exact() {
        assert!(supports("  Grok-4.6 ", RESPONSES));
        assert!(!supports("grok-4.6-preview", RESPONSES));
        assert!(supports("grok-4.6-preview", CHAT));
        assert_eq!(primary_protocol("MINIMAX-M2.7"), MESSAGES);
        assert_eq!(primary_protocol("QWEN3.8-MAX"), MESSAGES);
        assert_eq!(primary_protocol("  Kimi-K3  "), CHAT);
    }

    #[test]
    fn primary_endpoint_follows_the_catalog_pin() {
        assert_eq!(primary_protocol("glm-5.3"), CHAT);
        assert_eq!(primary_protocol("glm-5.3-flash"), CHAT);
        assert_eq!(primary_protocol("kimi-k3"), CHAT);
        assert_eq!(primary_protocol("deepseek-v4-pro"), CHAT);
        assert_eq!(primary_protocol("minimax-m2.7"), MESSAGES);
        assert_eq!(primary_protocol("minimax-m3"), MESSAGES);
        assert_eq!(primary_protocol("qwen3.8-max"), MESSAGES);
        assert_eq!(primary_protocol("grok-4.6"), RESPONSES);
        assert_eq!(primary_protocol("gpt-5.6-luna"), RESPONSES);
        assert_eq!(primary_protocol("muse-spark-1.3-contributor"), RESPONSES);
    }

    #[test]
    fn preferred_egress_always_returns_the_pin_on_opencode_go() {
        let go = go_provider();
        // Matching ingress still returns the pin (negotiate stays Native).
        assert_eq!(preferred_egress(&go, "kimi-k3", CHAT), Some(CHAT));
        assert_eq!(
            preferred_egress(&go, "grok-4.6", RESPONSES),
            Some(RESPONSES)
        );
        assert_eq!(
            preferred_egress(&go, "minimax-m3", MESSAGES),
            Some(MESSAGES)
        );
        assert_eq!(
            preferred_egress(&go, "qwen3.8-max", MESSAGES),
            Some(MESSAGES)
        );
        // Cross-protocol callers are forced onto the pin, not the client wire.
        assert_eq!(preferred_egress(&go, "kimi-k3", MESSAGES), Some(CHAT));
        assert_eq!(
            preferred_egress(&go, "deepseek-v4-pro", RESPONSES),
            Some(CHAT)
        );
        assert_eq!(preferred_egress(&go, "glm-5.3", MESSAGES), Some(CHAT));
        assert_eq!(preferred_egress(&go, "longcat-2.0", RESPONSES), Some(CHAT));
        assert_eq!(preferred_egress(&go, "grok-4.6", CHAT), Some(RESPONSES));
        assert_eq!(
            preferred_egress(&go, "gpt-5.6-luna", MESSAGES),
            Some(RESPONSES)
        );
        assert_eq!(preferred_egress(&go, "minimax-m3", CHAT), Some(MESSAGES));
        assert_eq!(preferred_egress(&go, "qwen3.8-max", CHAT), Some(MESSAGES));
        // Unlisted still default to chat.
        assert_eq!(preferred_egress(&go, "glm-6-turbo", MESSAGES), Some(CHAT));
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
