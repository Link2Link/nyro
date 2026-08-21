//! MiniMax vendor (OpenAI-compatible).

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::{
    ProtocolId, ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    OPENAI_RESPONSES_V1,
};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai::{
    openai_bearer_auth_headers, openai_build_url, openai_map_error,
};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const METADATA: VendorMetadata = VendorMetadata {
    id: "minimax",
    label: Label {
        zh: "MiniMax",
        en: "MiniMax",
    },
    icon: "minimax",
    default_protocol: "openai-compatible",
    channels: &[
        ChannelDef {
            id: "default",
            label: Label {
                zh: "默认",
                en: "Default",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://api.minimax.io/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://api.minimax.io/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("ai://models.dev/minimax"),
            capabilities_source: CapabilitiesSource::ModelsDev("minimax"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: false,
            auth_schemes: None,
        },
        ChannelDef {
            id: "china",
            label: Label {
                zh: "中国站",
                en: "China",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://api.minimaxi.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://api.minimaxi.com/anthropic",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://api.minimaxi.com/v1",
                },
            ],
            api_key: None,
            models_source: Some("https://api.minimaxi.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("minimax"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: None,
        },
    ],
};

pub struct MinimaxVendor;

/// MiniMax enables reasoning by default when the request omits a reasoning
/// directive. Make the gateway default explicit on the upstream wire while
/// preserving every client-supplied effort, budget, or thinking declaration.
fn default_reasoning_to_disabled(protocol: ProtocolId, body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    if protocol == OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        // MiniMax's Chat Completions switch is `reasoning_split`; unlike
        // OpenAI's top-level effort enum, it supports an explicit false value.
        if !object.contains_key("reasoning_effort") && !object.contains_key("reasoning_split") {
            object.insert("reasoning_split".to_string(), Value::Bool(false));
        }
    } else if protocol == OPENAI_RESPONSES_V1 {
        match object.entry("reasoning".to_string()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(serde_json::json!({"effort": "none"}));
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                if let Some(reasoning) = entry.get_mut().as_object_mut() {
                    reasoning
                        .entry("effort".to_string())
                        .or_insert_with(|| Value::String("none".to_string()));
                }
            }
        }
    } else if protocol == ANTHROPIC_MESSAGES_2023_06_01
        && !object.contains_key("thinking")
        && object
            .get("output_config")
            .and_then(Value::as_object)
            .is_none_or(|config| !config.contains_key("effort"))
    {
        object.insert(
            "thinking".to_string(),
            serde_json::json!({"type": "disabled"}),
        );
    }
}

#[async_trait]
impl Vendor for MinimaxVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "minimax",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        openai_build_url(base_url, path)
    }
    fn vendor_id(&self) -> &'static str {
        "minimax"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        };
        &[
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        ]
    }
    fn declared_request_mutations(&self) -> bool {
        true
    }

    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        default_reasoning_to_disabled(ctx.protocol_id, body);
        Ok(())
    }
    fn declared_response_mutations(&self) -> bool {
        false
    }
    async fn build_request(
        &self,
        req: &mut AiRequest,
        ctx: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        pipeline::build_request(self, req, ctx).await
    }
    async fn parse_response(
        &self,
        resp: InboundResponse,
        ctx: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        pipeline::parse_response(self, resp, ctx).await
    }
    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        openai_map_error("minimax", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(MinimaxVendor) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_chat_defaults_missing_effort_to_disabled_split() {
        let mut body = serde_json::json!({"model": "MiniMax-M2.7", "messages": []});

        default_reasoning_to_disabled(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_split"], false);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_chat_preserves_explicit_effort() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "reasoning_effort": "high"
        });

        default_reasoning_to_disabled(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning_split").is_none());
    }

    #[test]
    fn openai_chat_preserves_explicit_reasoning_split() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "reasoning_split": true
        });

        default_reasoning_to_disabled(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut body);

        assert_eq!(body["reasoning_split"], true);
    }

    #[test]
    fn openai_responses_defaults_missing_effort_to_none() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "input": [],
            "reasoning": {"summary": "auto"}
        });

        default_reasoning_to_disabled(OPENAI_RESPONSES_V1, &mut body);

        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn openai_responses_preserves_explicit_effort() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "input": [],
            "reasoning": {"effort": "max"}
        });

        default_reasoning_to_disabled(OPENAI_RESPONSES_V1, &mut body);

        assert_eq!(body["reasoning"]["effort"], "max");
    }

    #[test]
    fn anthropic_defaults_missing_thinking_to_disabled() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "max_tokens": 1024
        });

        default_reasoning_to_disabled(ANTHROPIC_MESSAGES_2023_06_01, &mut body);

        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_preserves_explicit_thinking_and_effort() {
        let mut body = serde_json::json!({
            "model": "MiniMax-M2.7",
            "messages": [],
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "output_config": {"effort": "high"}
        });
        let expected = body.clone();

        default_reasoning_to_disabled(ANTHROPIC_MESSAGES_2023_06_01, &mut body);

        assert_eq!(body, expected);
    }
}
