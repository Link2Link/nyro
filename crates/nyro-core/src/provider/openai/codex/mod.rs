//! OpenAI Codex channel (ChatGPT-backed, OAuth).

use reqwest::header::HeaderMap;

use crate::db::models::Provider;
use crate::protocol::ids::{OPENAI_RESPONSES_V1, ProtocolId};
use crate::provider::common::openai::{openai_bearer_auth_headers, openai_build_url};
use crate::provider::common::pipeline::openai_service_tier_override;
use crate::provider::registry::{ExtensionRegistration, VendorScope};
use crate::provider::vendor_ext::{VendorCtx, VendorExtension};

pub struct OpenAiCodexChannel;

/// Keep the Codex client's optional routing hint consistent with a forced
/// service tier and the resolved upstream model. Do not add internal headers
/// to requests from other clients, channels or protocols.
pub(crate) fn sync_speed_routing_hint(
    headers: &mut HeaderMap,
    provider: &Provider,
    protocol: ProtocolId,
    model: &str,
) {
    const HINT: &str = "x-codex-routing-hint";
    if protocol != OPENAI_RESPONSES_V1 || !headers.contains_key(HINT) {
        return;
    }
    let Some(tier) = openai_service_tier_override(
        provider.fast_mode,
        crate::provider::dialect::is_codex_consumer(provider),
        protocol.protocol,
    ) else {
        return;
    };
    let hint = format!("model={model};tier={tier}");
    if let Ok(value) = hint.parse() {
        headers.insert(HINT, value);
    } else {
        headers.remove(HINT);
    }
}

impl VendorExtension for OpenAiCodexChannel {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "openai",
            channel_id: "codex",
        }
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        // ChatGPT's Codex backend exposes `/responses` directly under
        // `/backend-api/codex`, not the public-platform `/v1/responses` path.
        let path = path.strip_prefix("/v1/").map_or(path, |rest| {
            // Retain the leading slash expected by `openai_build_url`.
            if rest == "responses" {
                "/responses"
            } else {
                path
            }
        });
        openai_build_url(base_url, path)
    }
}

inventory::submit! {
    ExtensionRegistration { make: || Box::new(OpenAiCodexChannel) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Provider {
        Provider {
            keys: Vec::new(),
            id: "provider-1".to_string(),
            name: "Codex".to_string(),
            vendor: Some("openai".to_string()),
            protocol: "openai-responses".to_string(),
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: Some("openai".to_string()),
            channel: Some("codex".to_string()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".to_string(),
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
    fn responses_url_uses_chatgpt_codex_path() {
        let provider = provider();
        let ctx = VendorCtx {
            provider: &provider,
            protocol_id: OPENAI_RESPONSES_V1,
            api_key: "",
            actual_model: "gpt-5-codex",
            credential: None,
        };
        assert_eq!(
            OpenAiCodexChannel.build_url(
                &ctx,
                "https://chatgpt.com/backend-api/codex",
                "/v1/responses"
            ),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }
}
