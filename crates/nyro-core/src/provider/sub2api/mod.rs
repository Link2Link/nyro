//! Sub2API vendor — relay to a Sub2API gateway, one channel per group platform.
//!
//! A Sub2API API key belongs to one group; the group's platform (Anthropic,
//! OpenAI, Gemini, Antigravity, Grok, the Chinese multi-protocol vendors,
//! aggregators or Composite) decides which routes serve it and which
//! protocols are forwarded natively. Each platform is a shared-key adaptive
//! channel here: the user enters the Sub2API address and key, every checked
//! protocol endpoint stores the normalized gateway root, and [`Sub2ApiVendor`]
//! inserts the platform's route prefix (Antigravity's `/antigravity`) in
//! front of the encoder path. Every route accepts `Authorization: Bearer`,
//! while `/v1/*` rejects `?key=`, so all protocols use Bearer.
//!
//! nyro owns the client side only (protocol choice and conversion, routing
//! across providers); Sub2API owns accounts, upstream impersonation,
//! envelopes, signatures and billing. Design: `docs/design/sub2api-provider.md`.

pub mod detect;
pub mod errors;
pub mod platform;
pub mod url;
pub mod usage;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::db::models::{CreateProviderProtocolEndpoint, Provider, UpsertProviderKey};
use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai::{openai_bearer_auth_headers, openai_map_error};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    CapabilitiesSource, ChannelDef, Label, ProtocolAuthScheme, ProtocolBaseUrl,
    ProtocolRoutePrefix, RelayAddressing, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::VendorCtx;

pub use platform::Sub2ApiPlatform;

pub const VENDOR_ID: &str = "sub2api";

/// True for providers created from the Sub2API preset (vendor or preset key).
pub fn is_sub2api(provider: &Provider) -> bool {
    is_sub2api_identity(provider.vendor.as_deref(), provider.preset_key.as_deref())
}

/// [`is_sub2api`] on raw admin input fields.
pub fn is_sub2api_identity(vendor: Option<&str>, preset_key: Option<&str>) -> bool {
    [vendor, preset_key]
        .into_iter()
        .flatten()
        .any(|value| value.trim().eq_ignore_ascii_case(VENDOR_ID))
}

/// Admin-side normalization before a Sub2API provider is stored: the channel
/// must name a supported platform, the provider, every protocol endpoint and
/// every key-pool override keep the gateway root (a pasted `…/v1` or
/// `…/antigravity` is dropped), every endpoint authenticates with Bearer and
/// an empty model discovery URL gets the platform's listing route.
pub(crate) fn normalize_admin_fields(
    channel: Option<&str>,
    auth_mode: &str,
    base_url: &mut String,
    endpoints: &mut [CreateProviderProtocolEndpoint],
    keys: Option<&mut [UpsertProviderKey]>,
    models_source: &mut Option<String>,
) -> anyhow::Result<()> {
    let platform = channel.and_then(Sub2ApiPlatform::from_id).ok_or_else(|| {
        let supported = Sub2ApiPlatform::ALL
            .iter()
            .map(|platform| platform.id())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!(
            "unsupported Sub2API platform {:?}; choose one of: {supported}",
            channel.unwrap_or_default()
        )
    })?;
    anyhow::ensure!(
        !auth_mode.trim().eq_ignore_ascii_case("oauth"),
        "Sub2API providers authenticate with a Sub2API API key"
    );
    for endpoint in endpoints.iter_mut() {
        endpoint.base_url = url::sub2api_root(&endpoint.base_url).to_string();
        endpoint.auth_scheme = "bearer".to_string();
    }
    let root = Some(url::sub2api_root(base_url))
        .filter(|root| !root.is_empty())
        .or_else(|| {
            endpoints
                .iter()
                .map(|endpoint| endpoint.base_url.as_str())
                .find(|root| !root.is_empty())
        })
        .unwrap_or_default()
        .to_string();
    for key in keys.into_iter().flatten() {
        if let Some(override_url) = key.base_url.as_mut() {
            *override_url = url::sub2api_root(override_url).to_string();
        }
    }
    if models_source
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        *models_source = url::models_url(Some(platform), &root);
    }
    *base_url = root;
    Ok(())
}

const CHAT: &str = "openai-compatible";
const RESPONSES: &str = "openai-responses";
const MESSAGES: &str = "anthropic-messages";
const GEMINI: &str = "google-gemini";

/// Protocol keys only: the Sub2API address is user-supplied.
const fn endpoint(protocol: &'static str) -> ProtocolBaseUrl {
    ProtocolBaseUrl {
        protocol,
        base_url: "",
    }
}

/// Bearer for every protocol: keeps the pipeline from rewriting Anthropic
/// egress to `x-api-key` and Gemini egress to `?key=` (rejected on `/v1`).
const AUTH_SCHEMES: &[ProtocolAuthScheme] = &[
    ProtocolAuthScheme {
        protocol: CHAT,
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: RESPONSES,
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: MESSAGES,
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: GEMINI,
        auth_scheme: "bearer",
    },
];

/// Mirrors [`Sub2ApiPlatform::route_prefix`] for the WebUI route preview.
const ANTIGRAVITY_PREFIXES: &[ProtocolRoutePrefix] = &[
    ProtocolRoutePrefix {
        protocol: MESSAGES,
        prefix: "/antigravity",
    },
    ProtocolRoutePrefix {
        protocol: GEMINI,
        prefix: "/antigravity",
    },
];

const fn relay(
    models_path: &'static str,
    route_prefixes: &'static [ProtocolRoutePrefix],
) -> Option<RelayAddressing> {
    Some(RelayAddressing {
        models_path,
        route_prefixes,
        root_suffixes: url::ROOT_SUFFIXES,
    })
}

const PASS_THROUGH_NOTE: Label = Label {
    zh: "Chat、Messages、Responses 同协议直通，由 Sub2API 按账号或模型选择上游端点。",
    en: "Chat, Messages and Responses pass through as-is; Sub2API picks the upstream endpoint per account or model.",
};

/// One channel per platform. "Checked" protocols (`base_urls` minus
/// `optional_protocols`) are forwarded natively by Sub2API without any group
/// switch; optional ones work after Sub2API converts or need a group switch.
/// Unchecked ingress protocols are converted by nyro to `default_protocol`.
const METADATA: VendorMetadata = VendorMetadata {
    id: VENDOR_ID,
    label: Label {
        zh: "Sub2API",
        en: "Sub2API",
    },
    icon: "sub2api",
    default_protocol: CHAT,
    channels: &[
        ChannelDef {
            id: "anthropic",
            label: Label {
                zh: "Anthropic",
                en: "Anthropic",
            },
            base_urls: &[endpoint(MESSAGES), endpoint(CHAT), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("anthropic"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(MESSAGES),
            relay: relay("/v1/models", &[]),
            optional_protocols: &[CHAT, RESPONSES],
            notes: Some(Label {
                zh: "Messages 原生直通。分组开启「仅限 Claude Code」时只有 Claude Code 客户端可用；其他客户端由 Nyro 转换为 Messages。",
                en: "Messages pass through natively. Groups restricted to Claude Code accept only Claude Code clients; Nyro converts other clients to Messages.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "openai",
            label: Label {
                zh: "OpenAI",
                en: "OpenAI",
            },
            base_urls: &[endpoint(RESPONSES), endpoint(CHAT), endpoint(MESSAGES)],
            capabilities_source: CapabilitiesSource::ModelsDev("openai"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(RESPONSES),
            relay: relay("/v1/models", &[]),
            optional_protocols: &[CHAT, MESSAGES],
            notes: Some(Label {
                zh: "Responses 原生直通（沿用 Codex 契约与 Fast 档）。勾选 Anthropic Messages 需要分组开启 /v1/messages 调度。",
                en: "Responses pass through natively (Codex contract and Fast tier apply). Anthropic Messages requires the group to allow /v1/messages dispatch.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "gemini",
            label: Label {
                zh: "Gemini",
                en: "Gemini",
            },
            base_urls: &[endpoint(GEMINI), endpoint(MESSAGES), endpoint(CHAT)],
            capabilities_source: CapabilitiesSource::ModelsDev("google"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(GEMINI),
            relay: relay("/v1beta/models", &[]),
            optional_protocols: &[MESSAGES, CHAT],
            notes: Some(Label {
                zh: "Gemini /v1beta 原生直通。Sub2API 的 Gemini 分组不提供 Responses，Responses 客户端由 Nyro 转换为 Gemini。",
                en: "Gemini /v1beta passes through natively. Sub2API Gemini groups do not serve Responses; Nyro converts Responses clients to Gemini.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "antigravity",
            label: Label {
                zh: "Antigravity",
                en: "Antigravity",
            },
            base_urls: &[
                endpoint(MESSAGES),
                endpoint(GEMINI),
                endpoint(CHAT),
                endpoint(RESPONSES),
            ],
            capabilities_source: CapabilitiesSource::Auto,
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(MESSAGES),
            relay: relay("/v1/models", ANTIGRAVITY_PREFIXES),
            optional_protocols: &[CHAT, RESPONSES],
            notes: Some(Label {
                zh: "Messages 与 Gemini 走 /antigravity 专用路由（只调度 Antigravity 账号）。请勿与 Anthropic 平台在同一会话中混用。",
                en: "Messages and Gemini use the dedicated /antigravity routes (Antigravity accounts only). Do not mix with the Anthropic platform in one conversation.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "grok",
            label: Label {
                zh: "Grok",
                en: "Grok",
            },
            base_urls: &[endpoint(RESPONSES), endpoint(CHAT), endpoint(MESSAGES)],
            capabilities_source: CapabilitiesSource::ModelsDev("xai"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(RESPONSES),
            relay: relay("/v1/models", &[]),
            optional_protocols: &[MESSAGES],
            notes: Some(Label {
                zh: "Responses 与 Chat 原生直通；Claude Code 请求默认由 Nyro 转换为 Responses。",
                en: "Responses and Chat pass through natively; Nyro converts Claude Code requests to Responses by default.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "kimi",
            label: Label {
                zh: "Kimi",
                en: "Kimi",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("moonshotai"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(PASS_THROUGH_NOTE),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "zhipu",
            label: Label {
                zh: "智谱 GLM",
                en: "Zhipu GLM",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("zhipuai"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            optional_protocols: &[RESPONSES],
            notes: Some(Label {
                zh: "Chat 与 Messages 原生直通；上游没有 Responses，勾选后由 Sub2API 转换。",
                en: "Chat and Messages pass through natively; the upstream has no Responses endpoint, so Sub2API converts it when checked.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "deepseek",
            label: Label {
                zh: "DeepSeek",
                en: "DeepSeek",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("deepseek"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(PASS_THROUGH_NOTE),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "minimax",
            label: Label {
                zh: "MiniMax",
                en: "MiniMax",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("minimax"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(PASS_THROUGH_NOTE),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "opencode_go",
            label: Label {
                zh: "OpenCode",
                en: "OpenCode",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::ModelsDev("opencode-go"),
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(PASS_THROUGH_NOTE),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "command_code",
            label: Label {
                zh: "Command Code",
                en: "Command Code",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::Auto,
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(PASS_THROUGH_NOTE),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "cline",
            label: Label {
                zh: "Cline",
                en: "Cline",
            },
            base_urls: &[endpoint(CHAT), endpoint(MESSAGES), endpoint(RESPONSES)],
            capabilities_source: CapabilitiesSource::Auto,
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            optional_protocols: &[MESSAGES, RESPONSES],
            notes: Some(Label {
                zh: "上游只有 Chat Completions；其他协议默认由 Nyro 转换。",
                en: "The upstream offers Chat Completions only; Nyro converts other protocols by default.",
            }),
            ..ChannelDef::DEFAULT
        },
        ChannelDef {
            id: "composite",
            label: Label {
                zh: "组合分组",
                en: "Composite",
            },
            base_urls: &[
                endpoint(CHAT),
                endpoint(RESPONSES),
                endpoint(MESSAGES),
                endpoint(GEMINI),
            ],
            capabilities_source: CapabilitiesSource::Auto,
            shared_key_protocols: true,
            auth_schemes: Some(AUTH_SCHEMES),
            default_protocol: Some(CHAT),
            relay: relay("/v1/models", &[]),
            notes: Some(Label {
                zh: "四种协议全部直通，由 Sub2API 按模型解析到具体平台。",
                en: "All four protocols pass through; Sub2API resolves each model to its platform.",
            }),
            ..ChannelDef::DEFAULT
        },
    ],
};

pub struct Sub2ApiVendor;

#[async_trait]
impl Vendor for Sub2ApiVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: VENDOR_ID,
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn auth_headers(&self, ctx: &VendorCtx<'_>) -> HeaderMap {
        openai_bearer_auth_headers(ctx)
    }
    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        url::endpoint_url(
            Sub2ApiPlatform::of_preset(ctx.provider),
            ctx.protocol_id.protocol,
            base_url,
            path,
        )
    }
    fn vendor_id(&self) -> &'static str {
        VENDOR_ID
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_COMPATIBLE_EMBEDDINGS_V1,
            OPENAI_RESPONSES_V1,
        };
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_EMBEDDINGS_V1,
        ]
    }
    // Sub2API forwards natively: preserve protocol content. Codex contract
    // passes still run on the passthrough path for the OpenAI platform.
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
        openai_map_error(VENDOR_ID, status, body)
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(Sub2ApiVendor) } }

#[cfg(test)]
mod tests;
