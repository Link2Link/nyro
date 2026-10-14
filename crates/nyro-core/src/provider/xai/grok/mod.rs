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

// Shared by inference/model discovery, provider metadata, and billing requests.
// The CLI proxy rejects versions below 1.0.13 with HTTP 426.
pub(crate) const GROK_CLI_VERSION: &str = "1.0.13";
pub(crate) const GROK_CLI_TOKEN_AUTH: &str = "xai-grok-cli";
pub(crate) const GROK_CLI_IDENTIFIER: &str = "grok-shell";

pub(crate) fn grok_cli_user_agent() -> String {
    format!("xai-grok-workspace/{GROK_CLI_VERSION}")
}

/// How many extra connect attempts the Grok subscription channel may make.
///
/// `cli-chat-proxy.grok.com` is reached through whatever DNS the host uses.
/// On a fake-ip proxy the handshake is occasionally reset (`tls handshake eof`)
/// before the HTTP request is written. The route usually has a single
/// candidate, so the dispatcher's "try the next target" retry never runs.
/// Replaying this channel once is safe: a connect failure means the body was
/// not sent. Other vendors, and the xAI API-key channel, stay single-attempt.
pub(crate) fn transient_connect_retries(provider: &crate::db::models::Provider) -> u8 {
    match (provider.vendor.as_deref(), provider.channel.as_deref()) {
        (Some(vendor), Some(channel))
            if vendor.eq_ignore_ascii_case("xai") && channel.eq_ignore_ascii_case("grok") =>
        {
            1
        }
        _ => 0,
    }
}

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
            keys: Vec::new(),
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

    #[test]
    fn only_the_grok_channel_replays_a_dropped_connect() {
        let mut provider = provider();
        assert_eq!(transient_connect_retries(&provider), 1);

        provider.channel = Some("default".to_string());
        assert_eq!(transient_connect_retries(&provider), 0);

        provider.channel = Some("grok".to_string());
        provider.vendor = Some("openai".to_string());
        assert_eq!(transient_connect_retries(&provider), 0);

        provider.vendor = None;
        provider.channel = None;
        assert_eq!(transient_connect_retries(&provider), 0);
    }
}
