//! Kiro (Amazon Q Developer / AWS CodeWhisperer) native chat backend.
//!
//! Wire behavior follows AIClient2API's Kiro provider; no Kiro IDE or CLI
//! process is started. Requests are plain JSON over HTTPS, responses are AWS
//! event-stream frames decoded into protocol-neutral IR deltas.
mod catalog;
pub(crate) mod credential;
mod estimate;
mod eventstream;
mod request;
mod runtime;
mod stream;
pub(crate) mod usage;

use async_trait::async_trait;
use serde_json::Value;

use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{KIRO_CHAT_V1, KIRO_DEFAULT_BASE_URL, ProtocolId};
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::native::{AttemptControl, IrUpstream};
use crate::provider::registry::{VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::{InboundResponse, OutboundRequest};
use crate::proxy::client::ProxyClient;

pub(crate) use estimate::ASSUMED_CACHE_READ_PERCENT;
pub(crate) use runtime::{check_connection, fetch_usage, probe};
pub(crate) use usage::KiroUsage;

pub(crate) const PROTOCOL_KEY: &str = "kiro-chat/generate-assistant-response/v1";

const METADATA: VendorMetadata = VendorMetadata {
    id: "kiro",
    label: Label {
        en: "Kiro",
        zh: "Kiro",
    },
    icon: "kiro",
    default_protocol: PROTOCOL_KEY,
    channels: &[ChannelDef {
        id: "ide",
        label: Label {
            en: "Kiro IDE / CLI subscription",
            zh: "Kiro IDE / CLI 订阅",
        },
        base_urls: &[ProtocolBaseUrl {
            protocol: PROTOCOL_KEY,
            base_url: KIRO_DEFAULT_BASE_URL,
        }],
        api_key: None,
        models_source: None,
        capabilities_source: CapabilitiesSource::Auto,
        static_models: catalog::MODELS,
        auth_mode: AuthMode::OAuth,
        oauth: Some(OAuthConfig {
            auth_base_url: "https://app.kiro.dev",
            authorize_url: "https://app.kiro.dev",
            token_url: "https://oidc.us-east-1.amazonaws.com/token",
            client_id: "",
            redirect_uri: "",
            scope: "",
        }),
        runtime: None,
        shared_key_protocols: false,
        auth_schemes: None,
    }],
};

pub struct KiroVendor;
#[async_trait]
impl Vendor for KiroVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor { vendor_id: "kiro" }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn vendor_id(&self) -> &'static str {
        "kiro"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        &[KIRO_CHAT_V1]
    }
    async fn build_request(
        &self,
        _: &mut AiRequest,
        _: &ProviderCtx<'_>,
    ) -> Result<OutboundRequest, GatewayError> {
        Err(unavailable("Kiro requires native event-stream execution"))
    }
    async fn parse_response(
        &self,
        _: InboundResponse,
        _: &ProviderCtx<'_>,
    ) -> Result<AiResponse, GatewayError> {
        Err(unavailable("Kiro requires native event-stream execution"))
    }
    fn map_error(&self, status: u16, _: Value) -> GatewayError {
        GatewayError::UpstreamStatus {
            provider: "kiro".into(),
            status,
            body: Some(format!("Kiro returned HTTP {status}")),
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
inventory::submit! { VendorRegistration { make: || Box::new(KiroVendor) } }

fn unavailable(message: impl Into<String>) -> GatewayError {
    GatewayError::provider_unavailable("kiro", message)
}

pub(crate) fn is_kiro(provider: &Provider) -> bool {
    provider
        .vendor
        .as_deref()
        .is_some_and(|vendor| vendor.trim().eq_ignore_ascii_case("kiro"))
}

/// Native providers have a legacy endpoint mirror in every storage backend.
/// Accept only a credential-free mirror of the provider's own regional origin.
pub(crate) fn validate(provider: &Provider) -> Result<(), GatewayError> {
    use crate::protocol::ProviderProtocols;
    let native = |raw: &str| {
        raw.trim()
            .split('/')
            .next()
            .is_some_and(|suite| suite.eq_ignore_ascii_case("kiro-chat"))
    };
    if !is_kiro(provider) {
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
                "kiro-chat is restricted to vendor=kiro, channel=ide",
            ));
        }
        return Ok(());
    }
    let origin = provider.base_url.trim().trim_end_matches('/');
    let valid = ProviderProtocols::parse_protocol_key(&provider.protocol) == Some(KIRO_CHAT_V1)
        && provider.channel.as_deref() == Some("ide")
        && provider.auth_mode == "oauth"
        && provider.protocol_mode == crate::db::models::PROVIDER_PROTOCOL_MODE_FIXED
        && credential::region_from_origin(origin).is_some()
        && provider.api_key.trim().is_empty()
        && provider.keys.is_empty()
        && provider.protocol_endpoints.iter().all(|ep| {
            ProviderProtocols::parse_protocol_key(&ep.protocol) == Some(KIRO_CHAT_V1)
                && ep.base_url.trim().trim_end_matches('/') == origin
                && ep.api_key.trim().is_empty()
                && matches!(ep.auth_scheme.as_str(), "auto" | "oauth")
        });
    if !valid {
        return Err(unavailable(
            "Kiro requires vendor=kiro, channel=ide, OAuth, fixed kiro-chat protocol and an https://q.<region>.amazonaws.com origin; API keys and adaptive endpoints are not supported",
        ));
    }
    Ok(())
}

/// Curated suggestions, not an account-specific entitlement catalog. User model
/// overrides allow exact new upstream ids without waiting for a Nyro release.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Provider {
        Provider {
            id: "p".into(),
            name: "Kiro".into(),
            vendor: Some("kiro".into()),
            protocol: PROTOCOL_KEY.into(),
            base_url: KIRO_DEFAULT_BASE_URL.into(),
            protocol_mode: crate::db::models::PROVIDER_PROTOCOL_MODE_FIXED.into(),
            protocol_endpoints: Vec::new(),
            keys: Vec::new(),
            preset_key: Some("kiro".into()),
            channel: Some("ide".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
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
    fn a_well_formed_kiro_provider_validates_and_other_regions_are_allowed() {
        assert!(validate(&provider()).is_ok());
        let mut eu = provider();
        eu.base_url = "https://q.eu-central-1.amazonaws.com/".into();
        assert!(validate(&eu).is_ok());
    }

    #[test]
    fn invalid_kiro_declarations_are_rejected() {
        for mutate in [
            (|p: &mut Provider| p.base_url = "https://evil.example.com".into())
                as fn(&mut Provider),
            |p| p.base_url = "https://q.us-east-1.amazonaws.com.evil.com".into(),
            |p| p.base_url = "http://q.us-east-1.amazonaws.com".into(),
            |p| p.api_key = "sk-secret".into(),
            |p| p.auth_mode = "apikey".into(),
            |p| p.channel = Some("cli".into()),
            |p| p.protocol_mode = crate::db::models::PROVIDER_PROTOCOL_MODE_ADAPTIVE.into(),
            |p| p.protocol = "openai-compatible".into(),
        ] {
            let mut candidate = provider();
            mutate(&mut candidate);
            assert!(validate(&candidate).is_err());
        }
    }

    #[test]
    fn non_kiro_providers_cannot_claim_the_native_protocol() {
        let mut other = provider();
        other.vendor = Some("custom".into());
        assert!(validate(&other).is_err());
        other.protocol = "openai-compatible".into();
        assert!(validate(&other).is_ok());
    }

    #[test]
    fn model_suggestions_prefer_the_operators_list() {
        let mut candidate = provider();
        assert!(models(&candidate).len() > 5);
        candidate.static_models = Some("claude-opus-5-5, \n claude-haiku-4-5".into());
        assert_eq!(models(&candidate), ["claude-opus-5-5", "claude-haiku-4-5"]);
    }

    #[test]
    fn protocol_key_resolves_to_the_registered_endpoint() {
        use crate::protocol::ProviderProtocols;
        assert_eq!(
            ProviderProtocols::parse_protocol_key(PROTOCOL_KEY),
            Some(KIRO_CHAT_V1)
        );
        assert_eq!(
            ProviderProtocols::parse_protocol_key("kiro-chat"),
            Some(KIRO_CHAT_V1)
        );
        let protocols = ProviderProtocols::from_provider(&provider());
        assert_eq!(protocols.default, KIRO_CHAT_V1);
        assert_eq!(protocols.endpoints.len(), 1);
        assert_eq!(protocols.endpoints[0].base_url, KIRO_DEFAULT_BASE_URL);
        // Invalid declarations fail closed with no usable endpoint.
        let mut broken = provider();
        broken.base_url = "https://evil.example.com".into();
        assert!(
            ProviderProtocols::from_provider(&broken)
                .endpoints
                .is_empty()
        );
    }
}
