//! Acceptance tests for `ProviderProtocols`.
//!
//! Covers three guarantees:
//!
//! 1. Alias-aware provider protocol parsing.
//! 2. Single-provider `resolve_egress` — same protocol suite stays native,
//!    different protocol suites fall back to the provider default.
//! 3. `ProtocolId::handler()` — `proxy/handler.rs` calls
//!    `ingress.handler().make_request_decoder()` on every request, so we assert
//!    it returns a registered handler for every canonical id we ship.

use nyro_core::db::models::{Provider, ProviderProtocolEndpoint};
use nyro_core::protocol::ProviderProtocols;
use nyro_core::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1, OPENAI_RESPONSES_V1,
};
use nyro_core::protocol::registry::ProtocolRegistry;

fn provider_with_protocol(protocol: &str, base_url: &str) -> Provider {
    Provider {
        id: "p".to_string(),
        name: "p".to_string(),
        vendor: None,
        protocol: protocol.to_string(),
        base_url: base_url.to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: Vec::new(),
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: String::new(),
        auth_mode: "apikey".to_string(),
        use_proxy: false,
        last_test_success: None,
        last_test_at: None,
        is_enabled: true,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn adaptive_provider() -> Provider {
    let mut provider = provider_with_protocol(
        "anthropic-messages/messages/2023-06-01",
        "https://messages.example",
    );
    provider.protocol_mode = "adaptive".to_string();
    provider.protocol_endpoints = vec![
        ProviderProtocolEndpoint {
            id: "chat-endpoint".to_string(),
            provider_id: provider.id.clone(),
            protocol: "openai-compatible/chat-completions/v1".to_string(),
            base_url: "https://chat.example/v1".to_string(),
            api_key: "sk-chat".to_string(),
            auth_scheme: "bearer".to_string(),
            is_enabled: true,
            priority: 1,
            test_status: "untested".to_string(),
            test_error: None,
            tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        },
        ProviderProtocolEndpoint {
            id: "anthropic-endpoint".to_string(),
            provider_id: provider.id.clone(),
            protocol: "anthropic-messages/messages/2023-06-01".to_string(),
            base_url: "https://messages.example".to_string(),
            api_key: "sk-anthropic".to_string(),
            auth_scheme: "x-api-key".to_string(),
            is_enabled: true,
            priority: 0,
            test_status: "untested".to_string(),
            test_error: None,
            tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        },
    ];
    provider
}

#[test]
fn parses_legacy_protocol_keys() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert!(!pp.supports(ANTHROPIC_MESSAGES_2023_06_01));
    assert!(!pp.supports(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA));
    assert!(!pp.supports(OPENAI_RESPONSES_V1));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(
        pp.get(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
            .unwrap()
            .base_url,
        "https://a.example/v1"
    );
}

#[test]
fn parses_canonical_protocol_id() {
    let provider = provider_with_protocol("openai/chat/v1", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
}

#[test]
fn parses_short_name_aliases() {
    let provider = provider_with_protocol("openai-chat", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);

    assert!(pp.supports(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1));
    assert_eq!(pp.default, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
}

#[test]
fn resolve_egress_exact_match_skips_conversion() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    let r = pp
        .resolve_egress(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .unwrap();

    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(r.base_url, "https://a.example/v1");
    assert!(!r.needs_conversion);
}

#[test]
fn resolve_egress_responses_falls_back_to_provider_default() {
    // OpenAI Responses (`openai-responses`) and OpenAI Compatible (`openai-compatible`) are
    // separate protocols; there is no same-protocol Tier-2 fallback between them.
    // A client speaking Responses API falls through to Tier 3 (provider default).
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    let r = pp.resolve_egress(OPENAI_RESPONSES_V1).unwrap();

    // No exact match, no same-protocol match (OpenAIResponses ≠ OpenAICompatible).
    // Tier 3: provider default = OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.
    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(r.base_url, "https://a.example/v1");
    assert!(r.needs_conversion);
}

#[test]
fn resolve_egress_falls_back_to_global_default_when_family_missing() {
    let provider = provider_with_protocol("openai", "https://a.example/v1");
    let pp = ProviderProtocols::from_provider(&provider);
    // Anthropic ingress, no Anthropic endpoint → fall back to default.
    let r = pp.resolve_egress(ANTHROPIC_MESSAGES_2023_06_01).unwrap();

    assert_eq!(r.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert!(r.needs_conversion);
}

#[test]
fn adaptive_exact_match_uses_endpoint_specific_base_url_and_auth() {
    let protocols = ProviderProtocols::from_provider(&adaptive_provider());
    let resolved = protocols
        .resolve_egress(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        .unwrap();

    assert_eq!(resolved.protocol, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
    assert_eq!(resolved.base_url, "https://chat.example/v1");
    assert_eq!(resolved.endpoint_id.as_deref(), Some("chat-endpoint"));
    assert_eq!(resolved.auth_scheme, "bearer");
    assert!(!resolved.needs_conversion);
}

#[test]
fn adaptive_missing_protocol_converts_only_to_configured_default() {
    let protocols = ProviderProtocols::from_provider(&adaptive_provider());
    let resolved = protocols
        .resolve_egress(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA)
        .unwrap();

    assert_eq!(resolved.protocol, ANTHROPIC_MESSAGES_2023_06_01);
    assert_eq!(resolved.base_url, "https://messages.example");
    assert_eq!(resolved.endpoint_id.as_deref(), Some("anthropic-endpoint"));
    assert_eq!(resolved.auth_scheme, "x-api-key");
    assert!(resolved.needs_conversion);
}

#[test]
fn adaptive_embeddings_never_fall_back_to_chat_or_default() {
    let protocols = ProviderProtocols::from_provider(&adaptive_provider());
    assert!(
        protocols
            .resolve_egress(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
            .is_none()
    );
}

#[test]
fn protocol_handler_resolves_for_every_canonical_id() {
    let reg = ProtocolRegistry::global();

    for id in [
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        OPENAI_RESPONSES_V1,
        ANTHROPIC_MESSAGES_2023_06_01,
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
    ] {
        assert!(reg.get(&id).is_some(), "no handler registered for {id}");
        assert_eq!(id.handler().id(), id);
    }
}
