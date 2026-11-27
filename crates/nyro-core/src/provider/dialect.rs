//! Upstream identity facts shared by pipeline-level wire policies.
//!
//! Wire policies (effort normalization, Codex consumer sanitizers, raw-wire
//! compat flavors) depend on which vendor's request contract the final hop
//! speaks. For relays that is not `provider.vendor`: a Sub2API provider's
//! platform names the real upstream (`sub2api/grok` speaks xAI). Identity
//! decisions — OAuth drivers, usage backends, account pools, subscription
//! envelopes — must keep reading `provider.vendor` and never use these.

use crate::db::models::Provider;
use crate::provider::sub2api::{self, Sub2ApiPlatform};

/// Vendor id whose wire dialect the final upstream hop speaks. Ordinary
/// providers return their own (trimmed) vendor id; a Sub2API provider
/// returns the vendor matching its group platform, or `sub2api` when the
/// upstream varies per model.
pub(crate) fn wire_dialect_vendor(provider: &Provider) -> &str {
    if let Some(platform) = Sub2ApiPlatform::of_preset(provider) {
        return platform.dialect_vendor().unwrap_or(sub2api::VENDOR_ID);
    }
    provider
        .vendor
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
}

/// Channel ids whose upstream is the ChatGPT Codex consumer backend on any
/// vendor: codex OAuth direct and the legacy `sub2api` relay channel.
fn is_codex_consumer_channel(channel: &str) -> bool {
    let channel = channel.trim();
    channel.eq_ignore_ascii_case("codex") || channel.eq_ignore_ascii_case("sub2api")
}

/// Codex consumer request contract (sampling-parameter stripping, reasoning
/// replay defense, tool-call ID normalization, Fast tier). Historically keyed
/// on the channel alone; Sub2API's OpenAI platform joins it.
pub(crate) fn is_codex_consumer(provider: &Provider) -> bool {
    provider
        .channel
        .as_deref()
        .is_some_and(is_codex_consumer_channel)
        || Sub2ApiPlatform::of_preset(provider).is_some_and(Sub2ApiPlatform::codex_consumer)
}

/// Vendor-qualified Codex consumer check used for raw-wire flavor selection:
/// the OpenAI preset's codex / sub2api channels or Sub2API's OpenAI platform.
pub(crate) fn is_openai_codex_consumer(provider: &Provider) -> bool {
    let openai_vendor = provider
        .vendor
        .as_deref()
        .is_some_and(|vendor| vendor.trim().eq_ignore_ascii_case("openai"));
    (openai_vendor
        && provider
            .channel
            .as_deref()
            .is_some_and(is_codex_consumer_channel))
        || Sub2ApiPlatform::of_preset(provider).is_some_and(Sub2ApiPlatform::codex_consumer)
}

/// OpenAI-native Responses upstream: third-party Responses rewrites must not
/// run (OpenAI vendor, Codex consumer channels, Sub2API's OpenAI platform).
pub(crate) fn is_openai_native_responses(provider: &Provider) -> bool {
    wire_dialect_vendor(provider).eq_ignore_ascii_case("openai")
        || provider
            .channel
            .as_deref()
            .is_some_and(is_codex_consumer_channel)
}

/// Relays whose remote picks the final vendor (a chained Nyro, Sub2API
/// Composite groups): protocol content passes untouched, no raw-wire
/// normalization, because the remote still has to choose a vendor.
pub(crate) fn is_transparent_relay(provider: &Provider) -> bool {
    crate::provider::nyro_gateway::is_nyro_gateway(provider)
        || Sub2ApiPlatform::of_preset(provider).is_some_and(Sub2ApiPlatform::transparent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(vendor: &str, channel: &str) -> Provider {
        Provider {
            keys: Vec::new(),
            id: "dialect-test".into(),
            name: vendor.into(),
            vendor: Some(vendor.into()),
            protocol: "openai-responses".into(),
            base_url: "https://example.com/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some(vendor.into()),
            channel: Some(channel.into()),
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: Default::default(),
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn sub2api_platforms_report_their_upstream_dialect() {
        assert_eq!(wire_dialect_vendor(&provider("sub2api", "grok")), "xai");
        assert_eq!(
            wire_dialect_vendor(&provider("sub2api", "openai")),
            "openai"
        );
        assert_eq!(
            wire_dialect_vendor(&provider("sub2api", "composite")),
            "sub2api"
        );
        assert_eq!(
            wire_dialect_vendor(&provider("sub2api", "antigravity")),
            "sub2api"
        );
        assert_eq!(wire_dialect_vendor(&provider(" xai ", "default")), "xai");
    }

    #[test]
    fn codex_contract_covers_codex_legacy_and_sub2api_openai() {
        for (vendor, channel, consumer, openai_consumer) in [
            ("openai", "codex", true, true),
            ("openai", "sub2api", true, true),
            ("custom", "sub2api", true, false),
            ("custom", "codex", true, false),
            ("sub2api", "openai", true, true),
            ("sub2api", "grok", false, false),
            ("openai", "default", false, false),
            ("vertexai", "openai", false, false),
        ] {
            let p = provider(vendor, channel);
            assert_eq!(is_codex_consumer(&p), consumer, "{vendor}/{channel}");
            assert_eq!(
                is_openai_codex_consumer(&p),
                openai_consumer,
                "{vendor}/{channel}"
            );
        }
        assert!(is_openai_native_responses(&provider("sub2api", "openai")));
        assert!(is_openai_native_responses(&provider("custom", "sub2api")));
        assert!(!is_openai_native_responses(&provider("sub2api", "kimi")));
    }

    #[test]
    fn only_composite_and_nyro_are_transparent() {
        assert!(is_transparent_relay(&provider("sub2api", "composite")));
        assert!(is_transparent_relay(&provider("nyro-gateway", "default")));
        assert!(!is_transparent_relay(&provider("sub2api", "openai")));
        assert!(!is_transparent_relay(&provider("custom", "composite")));
    }
}
