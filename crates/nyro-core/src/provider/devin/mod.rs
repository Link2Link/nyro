//! Devin CLI's native Connect/protobuf chat backend.
//! Wire behavior follows CLIProxyAPI's Devin executor; no Devin CLI process,
//! hosted agent session, filesystem access or local tools are started.
mod catalog;
mod request;
mod runtime;
mod stream;
mod usage;
mod wire;

use async_trait::async_trait;
use serde_json::Value;

use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{DEVIN_BASE_URL, DEVIN_CHAT_V1, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::native::{AttemptControl, IrUpstream};
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::{InboundResponse, OutboundRequest};
use crate::proxy::client::ProxyClient;

pub(crate) use runtime::{check_connection, fetch_usage, probe};
pub(crate) use usage::{DevinQuota, DevinUsage};
pub(crate) const BASE_URL: &str = DEVIN_BASE_URL;
const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";

const METADATA: VendorMetadata = VendorMetadata {
    id: "devin",
    label: Label {
        en: "Devin (Experimental)",
        zh: "Devin（实验性）",
    },
    icon: "devin",
    default_protocol: "devin-chat/get-chat-message/v1",
    channels: &[ChannelDef {
        id: "cli",
        label: Label {
            en: "Devin CLI subscription",
            zh: "Devin CLI 订阅",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: "devin-chat/get-chat-message/v1",
            base_url: BASE_URL,
        }],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: catalog::MODELS,
        auth_mode: AuthMode::OAuth,
        oauth: Some(OAuthConfig {
            auth_base_url: "https://app.devin.ai",
            authorize_url: "https://app.devin.ai/auth/cli/continue",
            token_url: "https://api.devin.ai/auth/cli/token",
            client_id: "",
            redirect_uri: "",
            scope: "",
        }),
        runtime: None,
        shared_key_protocols: false,
        auth_schemes: None,
    }],
};

pub struct DevinVendor;
#[async_trait]
impl Vendor for DevinVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "devin" }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn vendor_id(&self) -> &'static str {
        "devin"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[DEVIN_CHAT_V1]
    }
    async fn build_request(
        &self,
        _: &mut AiRequest,
        _: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        Err(unavailable(
            "Devin requires native Connect/protobuf execution",
        ))
    }
    async fn parse_response(
        &self,
        _: InboundResponse,
        _: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        Err(unavailable(
            "Devin requires native Connect/protobuf execution",
        ))
    }
    fn map_error(&self, status: u16, _: Value) -> GatewayError {
        GatewayError::UpstreamStatus {
            provider: "devin".into(),
            status,
            body: Some(format!("Devin returned HTTP {status}")),
        }
    }
    #[allow(private_interfaces)]
    async fn start_ir(
        &self,
        request: AiRequest,
        ctx: &ProviderCtx<'_>,
        transport: ProxyClient,
        control: AttemptControl,
    ) -> Result<IrUpstream, GatewayError> {
        runtime::start(request, ctx, transport, control).await
    }
}
inventory::submit! { VendorRegistration { make: || Box::new(DevinVendor) } }

fn unavailable(message: impl Into<String>) -> GatewayError {
    GatewayError::provider_unavailable("devin", message)
}

pub(crate) fn is_devin(provider: &Provider) -> bool {
    provider
        .vendor
        .as_deref()
        .is_some_and(|vendor| vendor.trim().eq_ignore_ascii_case("devin"))
}

/// Fixed-mode providers have a legacy endpoint mirror in all storage backends.
/// Accept only a same-origin, credential-free mirror; it is never executed as JSON.
pub(crate) fn validate(provider: &Provider) -> Result<(), GatewayError> {
    use crate::protocol::ProviderProtocols;
    let native = |raw: &str| {
        raw.trim()
            .split('/')
            .next()
            .is_some_and(|suite| suite.eq_ignore_ascii_case("devin-chat"))
    };
    if !is_devin(provider) {
        if native(&provider.protocol)
            || provider
                .protocol_endpoints
                .iter()
                .any(|ep| native(&ep.protocol))
            || provider
                .keys
                .iter()
                .any(|key| key.protocol.as_deref().is_some_and(native))
        {
            return Err(unavailable(
                "devin-chat is restricted to vendor=devin, channel=cli",
            ));
        }
        return Ok(());
    }
    let valid = ProviderProtocols::parse_protocol_key(&provider.protocol) == Some(DEVIN_CHAT_V1)
        && provider.channel.as_deref() == Some("cli")
        && provider.auth_mode == "oauth"
        && provider.protocol_mode == crate::db::models::PROVIDER_PROTOCOL_MODE_FIXED
        && provider.base_url.trim().trim_end_matches('/') == BASE_URL
        && provider.api_key.trim().is_empty()
        && provider.keys.is_empty()
        && provider.protocol_endpoints.iter().all(|ep| {
            ProviderProtocols::parse_protocol_key(&ep.protocol) == Some(DEVIN_CHAT_V1)
                && ep.base_url.trim().trim_end_matches('/') == BASE_URL
                && ep.api_key.trim().is_empty()
                && matches!(ep.auth_scheme.as_str(), "auto" | "oauth")
        });
    if !valid {
        return Err(unavailable(
            "Devin requires vendor=devin, channel=cli, OAuth, fixed devin-chat protocol and https://server.codeium.com; API keys and adaptive endpoints are not supported",
        ));
    }
    Ok(())
}

/// Curated suggestions, not an account-specific entitlement catalog. User model
/// overrides allow exact new upstream UIDs without waiting for a Nyro release.
pub(crate) fn models(provider: &Provider) -> Vec<String> {
    let configured: Vec<String> = provider
        .static_models
        .as_deref()
        .unwrap_or_default()
        .split([',', '\n'])
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .collect();
    if configured.is_empty() {
        catalog::MODELS
            .iter()
            .map(|model| model.to_string())
            .collect()
    } else {
        configured
    }
}
