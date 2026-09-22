//! Xiaomi MiMo vendor (小米 MiMo 开放平台, platform.xiaomimimo.com).
//!
//! OpenAI-compatible core (Chat Completions + Responses API) plus an
//! Anthropic Messages endpoint; one API key is valid for every protocol of a
//! channel. Endpoints follow the official Quick Access / First API Call docs:
//!
//! * `default` — pay-as-you-go `sk-` keys on `api.xiaomimimo.com`.
//! * `token-plan-cn` / `token-plan-sgp` / `token-plan-ams` — Token Plan
//!   subscription keys (`tp-` personal, `ttp-` team) on the China /
//!   Singapore / Europe cluster of `token-plan-*.xiaomimimo.com`.
//!
//! Wire quirks anchored to the official docs:
//! * Authentication accepts `api-key: <key>` or `Authorization: Bearer
//!   <key>` on every endpoint; `x-api-key` is not a documented scheme, so the
//!   Anthropic egress is pinned to `bearer` — the official Claude Code guide
//!   configures `ANTHROPIC_AUTH_TOKEN` for the same reason.
//! * Thinking mode (`thinking.type: enabled|disabled`, default enabled on
//!   `mimo-v2.5-pro` / `mimo-v2.5`) returns `reasoning_content` next to
//!   `tool_calls`; multi-turn tool history MUST pass every previous
//!   `reasoning_content` back or the API answers 400. The conversion profiles
//!   already cover this through the `xiaomimimo` reasoning-vendor hints
//!   (`conversion/resolver.rs`, cc-switch `claude_compat`).
//! * Token Plan keys and pay-as-you-go keys cannot be mixed across the two
//!   Base URL families (HTTP 401 otherwise).

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

/// MiMo accepts `api-key: <key>` or `Authorization: Bearer <key>` — never
/// the Anthropic-standard `x-api-key` — so Anthropic egress is pinned to Bearer.
const MIMO_ANTHROPIC_AUTH: &[ProtocolAuthScheme] = &[ProtocolAuthScheme {
    protocol: "anthropic-messages",
    auth_scheme: "bearer",
}];

/// Chat models from the official Models table. `GET /v1/models`
/// (`models_source`) stays authoritative and merges anything newer.
const MIMO_STATIC_MODELS: &[&str] = &["mimo-v2.5-pro", "mimo-v2.5"];

const METADATA: VendorMetadata = VendorMetadata {
    id: "xiaomimimo",
    label: Label {
        zh: "小米 MiMo",
        en: "Xiaomi MiMo",
    },
    icon: "xiaomimimo",
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
                    base_url: "https://api.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://api.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://api.xiaomimimo.com/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("https://api.xiaomimimo.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("xiaomi"),
            static_models: MIMO_STATIC_MODELS,
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: Some(MIMO_ANTHROPIC_AUTH),
        },
        ChannelDef {
            id: "token-plan-cn",
            label: Label {
                zh: "Token Plan 中国集群",
                en: "Token Plan (China)",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://token-plan-cn.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://token-plan-cn.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://token-plan-cn.xiaomimimo.com/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("https://token-plan-cn.xiaomimimo.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("xiaomi"),
            static_models: MIMO_STATIC_MODELS,
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: Some(MIMO_ANTHROPIC_AUTH),
        },
        ChannelDef {
            id: "token-plan-sgp",
            label: Label {
                zh: "Token Plan 新加坡集群",
                en: "Token Plan (Singapore)",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://token-plan-sgp.xiaomimimo.com/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("https://token-plan-sgp.xiaomimimo.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("xiaomi"),
            static_models: MIMO_STATIC_MODELS,
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: Some(MIMO_ANTHROPIC_AUTH),
        },
        ChannelDef {
            id: "token-plan-ams",
            label: Label {
                zh: "Token Plan 欧洲集群",
                en: "Token Plan (Europe)",
            },
            base_urls: &[
                ProtocolBaseUrl {
                    protocol: "openai-compatible",
                    base_url: "https://token-plan-ams.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "openai-responses",
                    base_url: "https://token-plan-ams.xiaomimimo.com/v1",
                },
                ProtocolBaseUrl {
                    protocol: "anthropic-messages",
                    base_url: "https://token-plan-ams.xiaomimimo.com/anthropic",
                },
            ],
            api_key: None,
            models_source: Some("https://token-plan-ams.xiaomimimo.com/v1/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("xiaomi"),
            static_models: MIMO_STATIC_MODELS,
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: true,
            auth_schemes: Some(MIMO_ANTHROPIC_AUTH),
        },
    ],
};

pub struct XiaomiMimoVendor;

#[async_trait]
impl Vendor for XiaomiMimoVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "xiaomimimo",
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
        "xiaomimimo"
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
        openai_map_error("xiaomimimo", status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(XiaomiMimoVendor) } }
