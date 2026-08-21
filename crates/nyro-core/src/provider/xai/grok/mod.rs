//! xAI Grok OAuth channel (Grok subscription via cli-chat-proxy).
//!
//! Auth-specific headers are injected by `GrokOAuthDriver` through
//! `RuntimeBinding.extra_headers`; this channel extension just gives the
//! resolver a concrete `(vendor=xai, channel=grok)` target and returns no
//! fallback auth headers so flipping `disable_default_auth` cannot leak an
//! empty Bearer from the API-key path.

use reqwest::header::HeaderMap;

use crate::provider::common::openai::openai_build_url;
use crate::provider::registry::{ExtensionRegistration, VendorScope};
use crate::provider::vendor_ext::{VendorCtx, VendorExtension};

pub struct XaiGrokChannel;

impl VendorExtension for XaiGrokChannel {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "xai",
            channel_id: "grok",
        }
    }

    fn auth_headers(&self, _ctx: &VendorCtx<'_>) -> HeaderMap {
        HeaderMap::new()
    }

    // The base URL already carries `/v1` (`https://cli-chat-proxy.grok.com/v1`);
    // reuse the OpenAI version-segment dedup so the encoder-emitted
    // `/v1/responses` does not double up to `/v1/v1/responses` (upstream 404).
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
}

inventory::submit! {
    ExtensionRegistration { make: || Box::new(XaiGrokChannel) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Provider;
    use crate::protocol::ids::OPENAI_RESPONSES_V1;

    fn provider() -> Provider {
        Provider {
            id: "provider-1".to_string(),
            name: "Grok".to_string(),
            vendor: Some("xai".to_string()),
            protocol: "openai-responses".to_string(),
            base_url: "https://cli-chat-proxy.grok.com/v1".to_string(),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: Some("xai".to_string()),
            channel: Some("grok".to_string()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".to_string(),
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
    fn grok_channel_keeps_openai_compatible_paths() {
        let provider = provider();
        let ctx = VendorCtx {
            provider: &provider,
            protocol_id: OPENAI_RESPONSES_V1,
            api_key: "",
            actual_model: "grok-4.5",
            credential: None,
        };
        // Base already ends with `/v1`; the channel must dedup so the
        // encoder-emitted `/v1/responses` does not double up (upstream 404).
        assert_eq!(
            XaiGrokChannel.build_url(&ctx, "https://cli-chat-proxy.grok.com/v1", "/v1/responses"),
            "https://cli-chat-proxy.grok.com/v1/responses"
        );
        assert_eq!(
            XaiGrokChannel.build_url(
                &ctx,
                "https://cli-chat-proxy.grok.com/v1",
                "/v1/chat/completions"
            ),
            "https://cli-chat-proxy.grok.com/v1/chat/completions"
        );
        assert!(XaiGrokChannel.auth_headers(&ctx).is_empty());
    }
}
