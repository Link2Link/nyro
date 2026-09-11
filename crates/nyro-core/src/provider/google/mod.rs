//! Google vendor (Gemini direct API + Google subscription channels).

pub(crate) mod antigravity;
pub(crate) mod gemini_cli;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::pipeline;
use crate::provider::inbound::InboundResponse;
use crate::provider::metadata::{
    AuthMode, CapabilitiesSource, ChannelDef, Label, OAuthConfig, ProtocolBaseUrl, VendorMetadata,
};
use crate::provider::outbound::OutboundRequest;
use crate::provider::registry::{ExtensionRegistration, VendorRegistration, VendorScope};
use crate::provider::vendor::{ProviderCtx, Vendor};
use crate::provider::vendor_ext::{VendorCtx, VendorExtension};

const METADATA: VendorMetadata = VendorMetadata {
    id: "google",
    label: Label {
        zh: "Google",
        en: "Google",
    },
    icon: "google",
    default_protocol: "google-gemini",
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
                    base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
                },
                ProtocolBaseUrl {
                    protocol: "google-gemini",
                    base_url: "https://generativelanguage.googleapis.com",
                },
            ],
            api_key: None,
            models_source: Some("https://generativelanguage.googleapis.com/v1beta/openai/models"),
            capabilities_source: CapabilitiesSource::ModelsDev("google"),
            static_models: &[],
            auth_mode: AuthMode::ApiKey,
            oauth: None,
            runtime: None,
            shared_key_protocols: false,
            auth_schemes: None,
        },
        ChannelDef {
            id: "antigravity",
            label: Label {
                zh: "Google AI Pro（Antigravity）",
                en: "Google AI Pro (Antigravity)",
            },
            base_urls: &[ProtocolBaseUrl {
                protocol: "google-gemini",
                base_url: "https://cloudcode-pa.googleapis.com",
            }],
            api_key: None,
            // The v1internal surface has no public /models endpoint; models
            // are discovered per-account via `fetchAvailableModels` with the
            // curated static list as fallback (see antigravity module).
            models_source: None,
            capabilities_source: CapabilitiesSource::ModelsDev("google"),
            static_models: antigravity::ANTIGRAVITY_STATIC_MODELS,
            auth_mode: AuthMode::OAuth,
            oauth: Some(OAuthConfig {
                auth_base_url: "https://accounts.google.com",
                authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
                token_url: "https://oauth2.googleapis.com/token",
                client_id: antigravity::ANTIGRAVITY_OAUTH_CLIENT_ID,
                redirect_uri: antigravity::ANTIGRAVITY_REDIRECT_URI,
                scope: antigravity::ANTIGRAVITY_OAUTH_SCOPES,
            }),
            runtime: None,
            shared_key_protocols: false,
            auth_schemes: None,
        },
        ChannelDef {
            id: "gemini-cli",
            label: Label {
                zh: "Gemini CLI（Code Assist）",
                en: "Gemini CLI (Code Assist)",
            },
            base_urls: &[ProtocolBaseUrl {
                protocol: "google-gemini",
                base_url: "https://cloudcode-pa.googleapis.com",
            }],
            api_key: None,
            // Same v1internal surface as antigravity: per-account
            // `fetchAvailableModels` discovery with a curated fallback (see
            // gemini_cli module).
            models_source: None,
            capabilities_source: CapabilitiesSource::ModelsDev("google"),
            static_models: gemini_cli::GEMINI_CLI_STATIC_MODELS,
            auth_mode: AuthMode::OAuth,
            oauth: Some(OAuthConfig {
                auth_base_url: "https://accounts.google.com",
                authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
                token_url: "https://oauth2.googleapis.com/token",
                client_id: gemini_cli::GEMINI_CLI_OAUTH_CLIENT_ID,
                redirect_uri: gemini_cli::GEMINI_CLI_REDIRECT_URI,
                scope: gemini_cli::GEMINI_CLI_OAUTH_SCOPES,
            }),
            runtime: None,
            shared_key_protocols: false,
            auth_schemes: None,
        },
    ],
};

pub struct GoogleVendor;

#[async_trait]
impl Vendor for GoogleVendor {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "google",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        Some(&METADATA)
    }
    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        if is_subscription_channel(ctx.provider) {
            // Canonicalized again by the channel-scoped v1internal exts;
            // kept here so direct calls produce the right path too.
            return antigravity::build_v1internal_url(base_url, path);
        }
        let url = format!("{}{path}", base_url.trim_end_matches('/'));
        if url.contains('?') {
            format!("{url}&key={}", ctx.api_key)
        } else {
            format!("{url}?key={}", ctx.api_key)
        }
    }
    fn vendor_id(&self) -> &'static str {
        "google"
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        &[GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA]
    }
    fn declared_request_mutations(&self) -> bool {
        false
    }
    fn declared_request_mutations_for(&self, provider: &crate::db::models::Provider) -> bool {
        is_subscription_channel(provider)
    }
    fn declared_response_mutations(&self) -> bool {
        false
    }
    fn declared_response_mutations_for(&self, provider: &crate::db::models::Provider) -> bool {
        is_subscription_channel(provider)
    }
    async fn post_encode(
        &self,
        ctx: &VendorCtx<'_>,
        body: &mut Value,
        _headers: &mut HeaderMap,
    ) -> anyhow::Result<()> {
        // Wrap the standard Gemini body into the v1internal envelope the
        // subscription surface expects. The antigravity channel uses the
        // full IDE fingerprint envelope; gemini-cli uses the slim Code
        // Assist dialect. (Effort-aware tier model selection lives in the
        // pipeline, see apply_tier_model_rewrite.)
        if antigravity::is_google_antigravity(ctx.provider) {
            let project_id = antigravity::code_assist_project_id(ctx.credential)?;
            let model = ctx.actual_model.to_string();
            *body = antigravity::wrap_request(std::mem::take(body), &model, &project_id);
        } else if gemini_cli::is_google_gemini_cli(ctx.provider) {
            let project_id = antigravity::code_assist_project_id(ctx.credential)?;
            let model = ctx.actual_model.to_string();
            *body = gemini_cli::wrap_request(std::mem::take(body), &model, &project_id);
        }
        Ok(())
    }
    async fn pre_parse(&self, ctx: &VendorCtx<'_>, resp: &mut Value) -> anyhow::Result<()> {
        if !is_subscription_channel(ctx.provider) {
            return Ok(());
        }
        antigravity::unwrap_response(resp);
        Ok(())
    }
    async fn on_stream_raw_chunk(
        &self,
        ctx: &VendorCtx<'_>,
        chunk: &mut String,
    ) -> anyhow::Result<()> {
        if !is_subscription_channel(ctx.provider) {
            return Ok(());
        }
        // Every SSE data line carries the v1internal {"response": …} envelope;
        // unwrap before the google-gemini stream decoder parses the chunk.
        *chunk = antigravity::unwrap_stream_chunk(chunk);
        Ok(())
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
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("upstream HTTP {status}"));
        GatewayError::upstream_status("google", status, Some(msg))
    }
}

inventory::submit! { VendorRegistration { make: || Box::new(GoogleVendor) } }

/// Family-level fallback for providers with blank/unknown vendor on Google-family protocols.
pub struct GoogleFamilyExt;

impl VendorExtension for GoogleFamilyExt {
    fn scope(&self) -> VendorScope {
        VendorScope::Vendor {
            vendor_id: "google",
        }
    }
    fn metadata(&self) -> Option<&'static VendorMetadata> {
        None
    }
    fn build_url(&self, ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        let url = format!("{}{path}", base_url.trim_end_matches('/'));
        if url.contains('?') {
            format!("{url}&key={}", ctx.api_key)
        } else {
            format!("{url}?key={}", ctx.api_key)
        }
    }
}

inventory::submit! { ExtensionRegistration { make: || Box::new(GoogleFamilyExt) } }

/// Channel-scoped URL canonicalizer for google/antigravity: rewrites the
/// codec's `/v1beta/models/{model}:…` egress path to the v1internal action
/// path (`:generateContent` / `:streamGenerateContent?alt=sse`) on the
/// Code Assist host.
pub struct GoogleAntigravityExt;

impl VendorExtension for GoogleAntigravityExt {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "google",
            channel_id: "antigravity",
        }
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        antigravity::build_v1internal_url(base_url, path)
    }
}

inventory::submit! { ExtensionRegistration { make: || Box::new(GoogleAntigravityExt) } }

/// Channel-scoped URL canonicalizer for google/gemini-cli: same v1internal
/// rewrite as the antigravity channel — both subscription channels share the
/// Code Assist host and action paths.
pub struct GoogleGeminiCliExt;

impl VendorExtension for GoogleGeminiCliExt {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "google",
            channel_id: "gemini-cli",
        }
    }
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        antigravity::build_v1internal_url(base_url, path)
    }
}

inventory::submit! { ExtensionRegistration { make: || Box::new(GoogleGeminiCliExt) } }

/// True when the provider rides a Google subscription channel (antigravity
/// or gemini-cli) on the Code Assist v1internal surface.
fn is_subscription_channel(provider: &crate::db::models::Provider) -> bool {
    antigravity::is_google_antigravity(provider) || gemini_cli::is_google_gemini_cli(provider)
}
