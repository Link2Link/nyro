//! Opencode Go vendor (opencode.ai/zen/go — Opencode Go plan).
//!
//! Adaptively routed channel: the Go plan serves different models on
//! `/v1/chat/completions`, `/v1/responses` and `/v1/messages`, so the preset
//! declares all three endpoints (one shared API key) and
//! [`routing`] hardcodes which model is served where. Every request also
//! carries the per-conversation routing identity [`session`] derives.

pub(crate) mod routing;
pub(crate) mod session;

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

/// The Go plan serves all three protocol families from the same host; the
/// path differs per protocol (`/v1/chat/completions`, `/v1/responses`,
/// `/v1/messages`).
const GO_BASE_URL: &str = "https://opencode.ai/zen/go";

/// `/v1/messages` authenticates with `x-api-key`, unlike the OpenAI-family
/// endpoints which take `Authorization: Bearer`. Declared here so both the
/// connectivity probe and the proxy egress send the header the upstream
/// expects.
const AUTH_SCHEMES: &[ProtocolAuthScheme] = &[ProtocolAuthScheme {
    protocol: "anthropic-messages",
    auth_scheme: "x-api-key",
}];

const METADATA: VendorMetadata = VendorMetadata {
    id: "opencode-go",
    label: Label {
        zh: "Opencode Go",
        en: "Opencode Go",
    },
    icon: "opencode",
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
                base_url: GO_BASE_URL,
            },
            ProtocolBaseUrl {
                protocol: "openai-responses",
                base_url: GO_BASE_URL,
            },
            ProtocolBaseUrl {
                protocol: "anthropic-messages",
                base_url: GO_BASE_URL,
            },
        ],
        api_key: None,
        models_source: Some("https://opencode.ai/zen/go/v1/models"),
        capabilities_source: CapabilitiesSource::ModelsDev("opencode-go"),
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
        // One subscription key covers all three endpoints, so the WebUI seeds
        // them from a single key field (and keeps the channel adaptive).
        shared_key_protocols: true,
        auth_schemes: Some(AUTH_SCHEMES),
    }],
};

pub struct OpencodeGoVendor;

#[async_trait]
impl Vendor for OpencodeGoVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "opencode-go",
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
        "opencode-go"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
        };
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
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
        openai_map_error("opencode-go", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(OpencodeGoVendor) } }
