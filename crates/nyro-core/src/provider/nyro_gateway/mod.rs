//! Nyro vendor — chain this gateway to another Nyro instance.
//!
//! A remote Nyro proxy serves every ingress protocol from one origin with one
//! credential (`NYRO_PROXY_AUTH_KEY` or an admin-managed API key):
//!
//! | protocol             | remote route                          |
//! |----------------------|---------------------------------------|
//! | `openai-compatible`  | `/v1/chat/completions`                |
//! | `openai-responses`   | `/v1/responses`                       |
//! | `anthropic-messages` | `/v1/messages`                        |
//! | `google-gemini`      | `/v1beta/models/{model}:{action}`     |
//! | model discovery      | `/v1/models`                          |
//!
//! The preset is therefore a shared-key adaptive channel declaring all four
//! protocols: every client protocol lands on the same-protocol remote route
//! and is forwarded natively (no IR round-trip, no lossy conversion).
//!
//! The vendor id is `nyro-gateway`, not `nyro`: `nyro` is the retired id of
//! the Custom preset — every storage backend still migrates
//! `vendor`/`preset_key = 'nyro'` to `custom` on startup, and the WebUI uses
//! `nyro` as its offline fallback preset id.
//!
//! The preset ships no base URL: there is no sensible default remote, and
//! pointing at this instance's own proxy port would loop requests back into
//! itself. The user enters the remote address; [`gateway_root`] normalizes it
//! so a pasted `…/v1` or `…/v1beta` suffix cannot double-version any route.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::GatewayError;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::{AiRequest, AiResponse};
use crate::provider::common::openai::{openai_bearer_auth_headers, openai_map_error};
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

pub mod usage;

pub const VENDOR_ID: &str = "nyro-gateway";

/// True for providers created from the Nyro preset (vendor or preset key).
pub fn is_nyro_gateway(provider: &crate::db::models::Provider) -> bool {
    [provider.vendor.as_deref(), provider.preset_key.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| value.trim().eq_ignore_ascii_case(VENDOR_ID))
}

/// Every remote route accepts `Authorization: Bearer`, including on remotes
/// that predate `x-api-key` / `x-goog-api-key` support in the global
/// `NYRO_PROXY_AUTH_KEY` middleware. Declaring Bearer for every protocol keeps
/// the pipeline from rewriting Anthropic egress to `x-api-key` and Gemini
/// egress to `?key=`, so chains work against older remotes too.
const AUTH_SCHEMES: &[ProtocolAuthScheme] = &[
    ProtocolAuthScheme {
        protocol: "openai-compatible",
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: "openai-responses",
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: "anthropic-messages",
        auth_scheme: "bearer",
    },
    ProtocolAuthScheme {
        protocol: "google-gemini",
        auth_scheme: "bearer",
    },
];

const METADATA: VendorMetadata = VendorMetadata {
    id: VENDOR_ID,
    label: Label {
        zh: "Nyro",
        en: "Nyro",
    },
    icon: "nyro",
    default_protocol: "openai-compatible",
    channels: &[ChannelDef {
        id: "default",
        label: Label {
            zh: "默认",
            en: "Default",
        },
        // Protocol keys only: the remote address is user-supplied.
        base_urls: &[
            ProtocolBaseUrl {
                protocol: "openai-compatible",
                base_url: "",
            },
            ProtocolBaseUrl {
                protocol: "openai-responses",
                base_url: "",
            },
            ProtocolBaseUrl {
                protocol: "anthropic-messages",
                base_url: "",
            },
            ProtocolBaseUrl {
                protocol: "google-gemini",
                base_url: "",
            },
        ],
        api_key: None,
        models_source: None,
        // Remote model names are the remote's route aliases; fuzzy-match them.
        capabilities_source: CapabilitiesSource::Auto,
        static_models: &[],
        auth_mode: AuthMode::ApiKey,
        oauth: None,
        runtime: None,
        shared_key_protocols: true,
        auth_schemes: Some(AUTH_SCHEMES),
    }],
};

/// True when `segment` is one of the remote's own version prefixes.
fn is_nyro_version_segment(segment: &str) -> bool {
    segment.eq_ignore_ascii_case("v1") || segment.eq_ignore_ascii_case("v1beta")
}

/// Normalize a user-supplied remote address to the gateway root that every
/// remote route hangs off: trailing slashes and one trailing `/v1` or
/// `/v1beta` segment are dropped. A reverse-proxy path prefix
/// (`https://host/nyro`) is preserved.
pub fn gateway_root(base_url: &str) -> &str {
    let base = base_url.trim().trim_end_matches('/');
    match base.rsplit_once('/') {
        // `rest` must still contain more than the scheme separator, so a host
        // literally named `v1` (`http://v1`) is never mistaken for a path.
        Some((rest, last)) if is_nyro_version_segment(last) && !rest.ends_with('/') => {
            rest.trim_end_matches('/')
        }
        _ => base,
    }
}

/// Build a remote URL: `gateway_root(base_url)` + the encoder's full route
/// path (`/v1/...` or `/v1beta/...`, query string included).
pub fn nyro_build_url(base_url: &str, path: &str) -> String {
    format!("{}{}", gateway_root(base_url), path)
}

pub struct NyroGatewayVendor;

#[async_trait]
impl Vendor for NyroGatewayVendor {
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
    fn build_url(&self, _ctx: &VendorCtx<'_>, base_url: &str, path: &str) -> String {
        nyro_build_url(base_url, path)
    }
    fn vendor_id(&self) -> &'static str {
        VENDOR_ID
    }
    fn supported_protocols(&self) -> &'static [ProtocolId] {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
        };
        &[
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        ]
    }
    // The remote speaks every protocol natively: forward byte-for-byte.
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

inventory::submit! { VendorRegistration { make: || Box::new(NyroGatewayVendor) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_root_strips_trailing_version_segment_and_slashes() {
        for (input, expected) in [
            ("http://10.0.0.2:19530", "http://10.0.0.2:19530"),
            ("http://10.0.0.2:19530/", "http://10.0.0.2:19530"),
            ("http://10.0.0.2:19530/v1", "http://10.0.0.2:19530"),
            ("http://10.0.0.2:19530/v1/", "http://10.0.0.2:19530"),
            ("http://10.0.0.2:19530/V1BETA", "http://10.0.0.2:19530"),
            (
                "  https://nyro.example.com/v1  ",
                "https://nyro.example.com",
            ),
            // Reverse-proxy path prefixes survive.
            (
                "https://edge.example.com/nyro",
                "https://edge.example.com/nyro",
            ),
            (
                "https://edge.example.com/nyro/v1",
                "https://edge.example.com/nyro",
            ),
            // Only the remote's own version prefixes are stripped.
            ("https://edge.example.com/v2", "https://edge.example.com/v2"),
            // A host named like a version segment is not a path.
            ("http://v1", "http://v1"),
        ] {
            assert_eq!(gateway_root(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn build_url_maps_every_protocol_route_onto_one_root() {
        for base in [
            "http://nyro:19530",
            "http://nyro:19530/v1",
            "http://nyro:19530/v1beta/",
        ] {
            assert_eq!(
                nyro_build_url(base, "/v1/chat/completions"),
                "http://nyro:19530/v1/chat/completions"
            );
            assert_eq!(
                nyro_build_url(base, "/v1/responses"),
                "http://nyro:19530/v1/responses"
            );
            assert_eq!(
                nyro_build_url(base, "/v1/messages"),
                "http://nyro:19530/v1/messages"
            );
            assert_eq!(
                nyro_build_url(
                    base,
                    "/v1beta/models/gemini-pro:streamGenerateContent?alt=sse"
                ),
                "http://nyro:19530/v1beta/models/gemini-pro:streamGenerateContent?alt=sse"
            );
        }
    }

    #[test]
    fn metadata_declares_shared_key_bearer_channel_for_all_protocols() {
        let channel = &METADATA.channels[0];
        assert!(channel.shared_key_protocols);
        assert_eq!(channel.auth_mode, AuthMode::ApiKey);
        let protocols: Vec<&str> = channel.base_urls.iter().map(|e| e.protocol).collect();
        assert_eq!(
            protocols,
            [
                "openai-compatible",
                "openai-responses",
                "anthropic-messages",
                "google-gemini"
            ]
        );
        assert!(channel.base_urls.iter().all(|e| e.base_url.is_empty()));
        let schemes = channel.auth_schemes.expect("auth schemes");
        assert_eq!(schemes.len(), protocols.len());
        assert!(schemes.iter().all(|s| s.auth_scheme == "bearer"));
    }
}
