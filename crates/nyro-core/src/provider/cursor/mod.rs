//! Experimental native Cursor AgentService provider.
//! No Cursor CLI, local filesystem tools, or secondary gateway is started.
pub(crate) mod catalog;
pub(crate) mod discovery;
pub(crate) mod errors;
pub(crate) mod framing;
pub(crate) mod history;
mod runtime;
pub(crate) mod session;
pub(crate) mod wire;

use crate::error::GatewayError;
use crate::protocol::ids::{CURSOR_AGENT_RUN_V1, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::native::{AttemptControl, IrUpstream};
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::{InboundResponse, OutboundRequest};
use crate::proxy::client::ProxyClient;
use async_trait::async_trait;
use serde_json::Value;

pub(crate) use runtime::{discover_models, probe};

pub(crate) const BASE_URL: &str = wire::BASE_URL;
pub(crate) const CLIENT_VERSION: &str = wire::CLIENT_VERSION;
pub(crate) const RUN_PATH: &str = wire::RUN_PATH;
const METADATA: VendorMetadata = VendorMetadata {
    id: "cursor",
    label: Label {
        en: "Cursor (Experimental)",
        zh: "Cursor（实验性）",
    },
    icon: "cursor",
    default_protocol: "cursor-agent/run/v1",
    channels: &[ChannelDef {
        id: "agent",
        label: Label {
            en: "AgentService subscription",
            zh: "AgentService 订阅",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: "cursor-agent/run/v1",
            base_url: BASE_URL,
        }],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: &[],
        auth_mode: AuthMode::OAuth,
        oauth: Some(OAuthConfig {
            auth_base_url: "https://cursor.com",
            authorize_url: "https://cursor.com/loginDeepControl",
            token_url: "https://api2.cursor.sh/auth/exchange_user_api_key",
            client_id: "",
            redirect_uri: "",
            scope: "",
        }),
        runtime: None,
        shared_key_protocols: false,
        auth_schemes: None,
    }],
};

pub struct CursorVendor;
#[async_trait]
impl Vendor for CursorVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "cursor",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn vendor_id(&self) -> &'static str {
        "cursor"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[CURSOR_AGENT_RUN_V1]
    }
    async fn build_request(
        &self,
        _: &mut AiRequest,
        _: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        Err(unsupported_json())
    }
    async fn parse_response(
        &self,
        _: InboundResponse,
        _: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        Err(unsupported_json())
    }
    fn map_error(&self, status: u16, body: Value) -> GatewayError {
        GatewayError::UpstreamStatus {
            provider: "cursor".into(),
            status,
            body: Some(body.to_string()),
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
fn unsupported_json() -> GatewayError {
    GatewayError::ProviderUnavailable {
        provider: "cursor".into(),
        reason: "Cursor requires native HTTP/2 AgentService execution".into(),
    }
}
inventory::submit! { VendorRegistration { make: || Box::new(CursorVendor) } }

pub(crate) fn gateway_response_error(error: errors::CursorError) -> GatewayError {
    if matches!(
        error,
        errors::CursorError::LimitExceeded { .. } | errors::CursorError::InvalidRequest(_)
    ) {
        return GatewayError::UpstreamStatus {
            provider: "cursor".into(),
            status: 502,
            body: Some(error.to_string()),
        };
    }
    gateway_error(error)
}

pub(crate) fn gateway_error(error: errors::CursorError) -> GatewayError {
    use errors::CursorError;
    match error {
        CursorError::InvalidRequest(message) | CursorError::Unsupported(message) => {
            GatewayError::BadRequest {
                code: "cursor_unsupported_request",
                msg: message,
            }
        }
        CursorError::LimitExceeded { resource, limit } => GatewayError::UpstreamStatus {
            provider: "cursor".into(),
            status: 413,
            body: Some(format!("{resource} exceeds {limit} bytes")),
        },
        CursorError::Upstream { code, message } => {
            let status = match code.as_str() {
                "unauthenticated" => 401,
                "permission_denied" => 403,
                "not_found" => 404,
                "resource_exhausted" => 429,
                "invalid_argument" => 400,
                "unavailable" => 503,
                "deadline_exceeded" => 504,
                _ => 502,
            };
            GatewayError::UpstreamStatus {
                provider: "cursor".into(),
                status,
                body: Some(format!("{code}: {message}")),
            }
        }
        _ => GatewayError::StreamParseError {
            provider: "cursor".into(),
            raw_chunk: None,
        },
    }
}
