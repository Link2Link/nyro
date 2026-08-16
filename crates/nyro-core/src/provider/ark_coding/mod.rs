//! Ark Coding vendor (ark.cn-beijing.volces.com/api/coding — Volcengine Ark
//! coding plan). OpenAI-compatible core (chat completions + Responses API)
//! with an Anthropic Messages endpoint; one API key is valid for all.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai::{
    openai_bearer_auth_headers, openai_build_url, openai_map_error,
};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, ProtocolAuthScheme, ProtocolBaseUrl,
    VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const METADATA: VendorMetadata = VendorMetadata {
    id: "ark-coding",
    label: Label {
        zh: "Ark Coding",
        en: "Ark Coding",
    },
    icon: "doubao",
    default_protocol: "openai-compatible",
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        base_urls: &[
            ProtocolBaseUrl {
                protocol: "openai-compatible",
                base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            },
            ProtocolBaseUrl {
                protocol: "openai-responses",
                base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            },
            ProtocolBaseUrl {
                protocol: "anthropic-messages",
                base_url: "https://ark.cn-beijing.volces.com/api/coding",
            },
        ],
        api_key: None,
        models_source: Some("https://ark.cn-beijing.volces.com/api/coding/v3/models"),
        capabilities_source: CapabilitiesSource::Auto,
        // Volcengine's /models endpoint returns an incomplete subset; these
        // coding-plan models are merged into the fetched list (deduped).
        static_models: &[
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "doubao-seed-2.0-lite",
            "doubao-seed-2.1-turbo",
            "glm-5.2",
            "glm-5.3",
            "kimi-k2.7-code",
            "minimax-m3",
        ],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
        shared_key_protocols: true,
        // Ark's Anthropic-compatible endpoint authenticates with
        // `Authorization: Bearer` (Volcengine's Claude Code integration uses
        // ANTHROPIC_AUTH_TOKEN), not the Anthropic-standard `x-api-key`.
        auth_schemes: Some(&[ProtocolAuthScheme {
            protocol: "anthropic-messages",
            auth_scheme: "bearer",
        }]),
    }],
};

pub struct ArkCodingVendor;

#[async_trait]
impl Vendor for ArkCodingVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "ark-coding",
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
        "ark-coding"
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
        false
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
        openai_map_error("ark-coding", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(ArkCodingVendor) } }
