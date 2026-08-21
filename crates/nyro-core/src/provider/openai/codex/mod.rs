//! OpenAI Codex channel (ChatGPT-backed, OAuth).

use reqwest::header::HeaderMap;

use crate::provider::common::openai::{openai_bearer_auth_headers, openai_build_url};
use crate::provider::registry::{ExtensionRegistration, VendorScope};
use crate::provider::vendor_ext::{VendorCtx, VendorExtension};

pub struct OpenAiCodexChannel;

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
    use crate::db::models::Provider;
    use crate::protocol::ids::OPENAI_RESPONSES_V1;

    fn provider() -> Provider {
        Provider {
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
            fast_mode: false,
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
