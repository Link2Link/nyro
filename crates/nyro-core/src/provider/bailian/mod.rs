//! Bailian vendor (阿里百炼 / Alibaba Cloud Model Studio).
//! OpenAI-compatible chat-completions core plus an Anthropic Messages
//! endpoint; one API key is valid for every protocol of a channel.
//!
//! * `default` — pay-as-you-go endpoints on `dashscope.aliyuncs.com`.
//! * `coding` — Bailian coding-plan subscription endpoints on
//!   `token-plan.cn-beijing.maas.aliyuncs.com`.

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
    AuthMode, CapabilitiesSource, ChannelDef, Label, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

const METADATA: VendorMetadata = VendorMetadata {
    id: "bailian",
    label: Label {
        zh: "阿里百炼",
        en: "Alibaba Bailian",
    },
    icon: "bailian",
    default_protocol: "openai-compatible",
    channels: &[
        ChannelDef {
            id: "default",
            label: Label {
                zh: "按量付费",
                en: "Pay-as-you-go",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://dashscope.aliyuncs.com/apps/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("https://dashscope.aliyuncs.com/compatible-mode/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("alibaba"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: None,
        },
        ChannelDef {
            id: "coding",
            label: Label {
                zh: "百炼套餐",
                en: "Coding Plan",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic",
                },
            ],
            api_key: None,
            models_source: Some(
                "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1/models",
            ),
            capabilities_source: CapabilitiesSource::ModelsDev("alibaba-coding-plan"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: None,
        },
    ],
};

pub struct BailianVendor;

#[async_trait]
impl Vendor for BailianVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "bailian" }
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
        "bailian"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        };
        &[
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
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
        openai_map_error("bailian", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(BailianVendor) } }
