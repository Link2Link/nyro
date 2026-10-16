//! Protocol layer.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (JSON/HTTP families plus egress-only `cursor-agent`).
//! - `name`: wire-format endpoint name (`chat-completions`, `responses`, `messages`, `generate-content`).
//! - `version`: schema version as the vendor labels it (`v1`, `2023-06-01`, `v1beta`).
//!
//! See [`ids`], [`traits`], [`registry`], and [`codec`] for the model.
//!
//! ## Codec layout
//!
//! Each `codec/<vendor>/<protocol>/` directory co-locates the wire codecs **and** the
//! thin `EndpointHandler` registration shell for every endpoint:
//!
//! - `codec/openai/compatible/chat_completions.rs` — `OpenAICompatibleChatCompletionsV1`
//! - `codec/openai/compatible/embeddings.rs` — `OpenAICompatibleEmbeddingsV1`
//! - `codec/openai/responses/responses.rs` — `OpenAIResponsesV1`
//! - `codec/anthropic/messages/messages.rs` — `AnthropicMessages2023`
//! - `codec/google/gemini/generate_content.rs` — `GoogleGeminiGenerateContentV1Beta`
//!
//! Shared semantic utilities live in `codec/reasoning.rs` and
//! `codec/tool_correlation.rs`.
//!
//! ## Alias table
//!
//! See [`registry::ProtocolRegistry`] for three-tier resolution of endpoint aliases
//! and [`registry::ProtocolRegistry::parse_protocol`] for Protocol-level resolution.

pub mod codec;

pub mod ids;
pub mod ir;
pub mod registry;
pub mod traits;

use reqwest::header::HeaderMap;

use crate::db::models::{PROVIDER_PROTOCOL_MODE_FIXED, Provider};
use crate::error::GatewayError;
use crate::protocol::ids::{
    CURSOR_AGENT_BASE_URL, CURSOR_AGENT_RUN_V1, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    ProtocolEndpoint,
};
pub use registry::{EndpointDescriptor, ExecutionKind};

// ── Client → Gateway ──

pub trait RequestDecoder {
    fn decode_request(&self, body: serde_json::Value) -> anyhow::Result<ir::AiRequest>;
}

// ── Gateway → Provider ──

pub trait RequestEncoder {
    fn encode_request(&self, req: &ir::AiRequest)
    -> anyhow::Result<(serde_json::Value, HeaderMap)>;

    fn egress_path(&self, model: &str, stream: bool) -> String;
}

// ── Provider response → internal ──

pub trait ResponseDecoder: Send {
    fn parse_response(&self, resp: serde_json::Value) -> anyhow::Result<ir::AiResponse>;
}

// ── Internal → client response ──

pub trait ResponseEncoder: Send {
    fn format_response(&self, resp: &ir::AiResponse) -> serde_json::Value;
}

// ── Streaming: provider → internal deltas ──

pub trait StreamResponseDecoder: Send {
    fn parse_chunk(&mut self, raw: &str) -> anyhow::Result<Vec<ir::AiStreamDelta>>;
    fn finish(&mut self) -> anyhow::Result<Vec<ir::AiStreamDelta>>;
}

// ── Streaming: internal deltas → client SSE ──

pub trait StreamResponseEncoder: Send {
    fn format_deltas(&mut self, deltas: &[ir::AiStreamDelta]) -> Vec<SseEvent>;
    fn format_done(&mut self) -> Vec<SseEvent>;
    fn usage(&self) -> ir::Usage;
}

// ── SSE helper ──

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn new(event: Option<&str>, data: impl Into<String>) -> Self {
        Self {
            event: event.map(|e| e.to_string()),
            data: data.into(),
        }
    }

    pub fn to_sse_string(&self) -> String {
        let mut s = String::new();
        if let Some(ref event) = self.event {
            s.push_str(&format!("event: {event}\n"));
        }
        s.push_str(&format!("data: {}\n\n", self.data));
        s
    }
}

// ── Provider protocol negotiation ──

/// Declared protocol capabilities of a single provider, built from the DB row.
#[derive(Debug, Clone)]
pub struct ProviderProtocols {
    pub default: ProtocolEndpoint,
    pub endpoints: Vec<ProviderProtocolTarget>,
    pub adaptive: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderProtocolTarget {
    pub record_id: Option<String>,
    pub protocol: ProtocolEndpoint,
    pub base_url: String,
    pub auth_scheme: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedEgress {
    pub protocol: ProtocolEndpoint,
    pub base_url: String,
    pub endpoint_id: Option<String>,
    pub auth_scheme: String,
    pub needs_conversion: bool,
}

impl ProviderProtocols {
    /// Best-effort string → [`ProtocolEndpoint`] resolver.
    pub fn parse_protocol_key(s: &str) -> Option<ProtocolEndpoint> {
        let reg = registry::ProtocolRegistry::global();
        reg.resolve_alias(s).or_else(|| {
            let protocol = reg.parse_protocol(s)?;
            reg.list_endpoints()
                .into_iter()
                .find(|endpoint| endpoint.id.protocol == protocol)
                .map(|endpoint| endpoint.id)
        })
    }

    /// Validate vendor-only protocol invariants before dispatch, probing or
    /// persisting provider configuration. Legacy JSON/HTTP provider semantics
    /// are unchanged; Cursor never inherits a generic provider/key fallback.
    pub fn validate_provider(provider: &Provider) -> Result<(), GatewayError> {
        crate::provider::devin::validate(provider)?;
        let is_cursor = provider
            .vendor
            .as_deref()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("cursor");
        let declares_cursor = |raw: &str| {
            raw.trim()
                .split('/')
                .next()
                .is_some_and(|suite| suite.eq_ignore_ascii_case("cursor-agent"))
        };
        if !is_cursor {
            if declares_cursor(&provider.protocol)
                || provider
                    .protocol_endpoints
                    .iter()
                    .any(|ep| declares_cursor(&ep.protocol))
                || provider
                    .keys
                    .iter()
                    .any(|key| key.protocol.as_deref().is_some_and(declares_cursor))
            {
                return Err(GatewayError::provider_unavailable(
                    &provider.id,
                    "cursor-agent is restricted to vendor=cursor, channel=agent",
                ));
            }
            return Ok(());
        }

        let invalid = if Self::parse_protocol_key(&provider.protocol) != Some(CURSOR_AGENT_RUN_V1) {
            Some("Cursor requires the cursor-agent/run/v1 protocol")
        } else if !provider
            .channel
            .as_deref()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("agent")
        {
            Some("Cursor requires channel=agent")
        } else if provider.auth_mode.trim() != "oauth" {
            Some("Cursor requires OAuth authentication")
        } else if provider.protocol_mode.trim() != PROVIDER_PROTOCOL_MODE_FIXED
            || !provider.protocol_endpoints.is_empty()
        {
            Some("Cursor requires fixed protocol mode without adaptive endpoints")
        } else if provider.base_url.trim().trim_end_matches('/') != CURSOR_AGENT_BASE_URL {
            Some("Cursor requires the fixed https://api2.cursor.sh service origin")
        } else if !provider.keys.is_empty() {
            Some("Cursor does not support API-key pools or per-key overrides")
        } else {
            None
        };
        match invalid {
            Some(message) => Err(GatewayError::provider_unavailable(&provider.id, message)),
            None => Ok(()),
        }
    }

    /// Checked construction for dispatch/admin callers that need an explicit
    /// configuration error rather than the fail-closed compatibility result.
    pub fn try_from_provider(provider: &Provider) -> Result<Self, GatewayError> {
        Self::validate_provider(provider)?;
        Ok(Self::from_provider(provider))
    }

    /// Build from a provider DB row. Invalid Cursor declarations fail closed
    /// with no usable endpoints, never silently falling back to OpenAI.
    /// Call `try_from_provider` when the configuration error should be surfaced.
    pub fn from_provider(provider: &Provider) -> Self {
        if Self::validate_provider(provider).is_err() {
            return Self {
                default: CURSOR_AGENT_RUN_V1,
                endpoints: Vec::new(),
                adaptive: false,
            };
        }
        if provider
            .vendor
            .as_deref()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("cursor")
        {
            return Self {
                default: CURSOR_AGENT_RUN_V1,
                endpoints: vec![ProviderProtocolTarget {
                    record_id: None,
                    protocol: CURSOR_AGENT_RUN_V1,
                    base_url: CURSOR_AGENT_BASE_URL.to_string(),
                    auth_scheme: "oauth".to_string(),
                }],
                adaptive: false,
            };
        }
        if crate::provider::devin::is_devin(provider) {
            return Self {
                default: ids::DEVIN_CHAT_V1,
                endpoints: vec![ProviderProtocolTarget {
                    record_id: None,
                    protocol: ids::DEVIN_CHAT_V1,
                    base_url: ids::DEVIN_BASE_URL.to_string(),
                    auth_scheme: "oauth".to_string(),
                }],
                adaptive: false,
            };
        }
        let registry = registry::ProtocolRegistry::global();
        if provider.is_adaptive() {
            let mut configured = provider
                .protocol_endpoints
                .iter()
                .filter(|endpoint| endpoint.is_enabled)
                .filter_map(|endpoint| {
                    let protocol = registry.resolve_alias(&endpoint.protocol)?;
                    Some((
                        endpoint.priority,
                        ProviderProtocolTarget {
                            record_id: Some(endpoint.id.clone()),
                            protocol,
                            base_url: endpoint.base_url.trim().to_string(),
                            auth_scheme: endpoint.auth_scheme.trim().to_string(),
                        },
                    ))
                })
                .collect::<Vec<_>>();
            configured.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.protocol.cmp(&right.1.protocol))
            });
            let endpoints = configured
                .into_iter()
                .map(|(_, endpoint)| endpoint)
                .collect::<Vec<_>>();

            let declared_default = registry.resolve_alias(provider.protocol.trim());
            let declared_suite = registry.parse_protocol(provider.protocol.trim());
            let default = declared_default
                .filter(|candidate| endpoints.iter().any(|target| target.protocol == *candidate))
                .or_else(|| {
                    declared_suite.and_then(|suite| {
                        endpoints
                            .iter()
                            .find(|target| target.protocol.protocol == suite)
                            .map(|target| target.protocol)
                    })
                })
                .or_else(|| endpoints.first().map(|target| target.protocol))
                .unwrap_or(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);

            return Self {
                default,
                endpoints,
                adaptive: true,
            };
        }

        let default = Self::parse_protocol_key(provider.protocol.trim())
            .unwrap_or(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        let suite = registry
            .parse_protocol(provider.protocol.trim())
            .unwrap_or(default.protocol);
        let endpoints = registry
            .list_by_protocol(suite)
            .into_iter()
            .map(|handler| ProviderProtocolTarget {
                record_id: None,
                protocol: handler.id(),
                base_url: provider.base_url.trim().to_string(),
                auth_scheme: "auto".to_string(),
            })
            .collect();

        Self {
            default,
            endpoints,
            adaptive: false,
        }
    }

    /// Returns `true` if the provider declares support for `protocol`.
    pub fn supports(&self, protocol: ProtocolEndpoint) -> bool {
        self.get(protocol).is_some()
    }

    pub fn get(&self, protocol: ProtocolEndpoint) -> Option<&ProviderProtocolTarget> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.protocol == protocol)
    }

    /// Deterministic exact-or-default egress resolution:
    ///
    /// 1. **Exact endpoint** — use the endpoint configured for the ingress.
    /// 2. **Provider default** — last resort with conversion.
    pub fn resolve_egress(&self, ingress: ProtocolEndpoint) -> Option<ResolvedEgress> {
        let registry = registry::ProtocolRegistry::global();
        let ingress_descriptor = registry.describe(&ingress)?;
        if ingress_descriptor.execution != ExecutionKind::JsonHttp {
            return None;
        }
        if let Some(endpoint) = self.get(ingress) {
            return Some(ResolvedEgress {
                protocol: ingress,
                base_url: endpoint.base_url.clone(),
                endpoint_id: endpoint.record_id.clone(),
                auth_scheme: endpoint.auth_scheme.clone(),
                needs_conversion: false,
            });
        }

        if ingress_descriptor.capabilities.embeddings
            || registry.describe(&self.default)?.capabilities.embeddings
        {
            return None;
        }

        let endpoint = self.get(self.default)?;
        Some(ResolvedEgress {
            protocol: self.default,
            base_url: endpoint.base_url.clone(),
            endpoint_id: endpoint.record_id.clone(),
            auth_scheme: endpoint.auth_scheme.clone(),
            needs_conversion: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ProviderKey, ProviderProtocolEndpoint};
    use crate::protocol::ids::OPENAI_COMPATIBLE_EMBEDDINGS_V1;

    fn cursor_provider() -> Provider {
        Provider {
            id: "cursor-test".into(),
            name: "Cursor".into(),
            vendor: Some("cursor".into()),
            protocol: "cursor-agent/run/v1".into(),
            base_url: CURSOR_AGENT_BASE_URL.into(),
            protocol_mode: PROVIDER_PROTOCOL_MODE_FIXED.into(),
            protocol_endpoints: Vec::new(),
            keys: Vec::new(),
            preset_key: None,
            channel: Some("agent".into()),
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

    fn endpoint(protocol: &str) -> ProviderProtocolEndpoint {
        ProviderProtocolEndpoint {
            id: "ep".into(),
            provider_id: "cursor-test".into(),
            protocol: protocol.into(),
            base_url: CURSOR_AGENT_BASE_URL.into(),
            api_key: String::new(),
            auth_scheme: "oauth".into(),
            is_enabled: true,
            priority: 0,
            test_status: "unknown".into(),
            test_error: None,
            tested_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn key(protocol: Option<&str>) -> ProviderKey {
        ProviderKey {
            id: "key".into(),
            provider_id: "cursor-test".into(),
            name: "key".into(),
            api_key: "secret".into(),
            protocol: protocol.map(str::to_string),
            base_url: None,
            is_enabled: true,
            priority: 0,
            models_snapshot: None,
            manual_models: None,
            last_probe_at: None,
            probe_error: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn assert_rejected(provider: &Provider) {
        assert!(ProviderProtocols::validate_provider(provider).is_err());
        assert!(ProviderProtocols::try_from_provider(provider).is_err());
        let declaration = ProviderProtocols::from_provider(provider);
        assert_eq!(declaration.default, CURSOR_AGENT_RUN_V1);
        assert!(declaration.endpoints.is_empty());
        assert!(
            declaration
                .resolve_egress(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
                .is_none()
        );
    }

    #[test]
    fn cursor_declaration_is_exact_fixed_and_oauth() {
        let mut provider = cursor_provider();
        for protocol in ["cursor-agent", "cursor-agent/run/v1"] {
            provider.protocol = protocol.into();
            provider.base_url = format!("{CURSOR_AGENT_BASE_URL}/");
            let declaration = ProviderProtocols::try_from_provider(&provider).unwrap();
            assert_eq!(declaration.default, CURSOR_AGENT_RUN_V1);
            assert_eq!(declaration.endpoints.len(), 1);
            assert!(!declaration.adaptive);
            let target = declaration.get(CURSOR_AGENT_RUN_V1).unwrap();
            assert_eq!(target.base_url, CURSOR_AGENT_BASE_URL);
            assert_eq!(target.auth_scheme, "oauth");
            assert!(target.record_id.is_none());
            let resolved = declaration
                .resolve_egress(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
                .unwrap();
            assert_eq!(resolved.protocol, CURSOR_AGENT_RUN_V1);
            assert!(resolved.needs_conversion);
            assert!(declaration.resolve_egress(CURSOR_AGENT_RUN_V1).is_none());
            assert!(
                declaration
                    .resolve_egress(OPENAI_COMPATIBLE_EMBEDDINGS_V1)
                    .is_none()
            );
        }
    }

    #[test]
    fn invalid_cursor_configuration_never_falls_back_to_openai() {
        for mutate in [
            |p: &mut Provider| p.protocol = "openai".into(),
            |p: &mut Provider| p.protocol = "cursor-agent/run/v2".into(),
            |p: &mut Provider| p.protocol = String::new(),
            |p: &mut Provider| p.channel = Some("default".into()),
            |p: &mut Provider| p.channel = None,
            |p: &mut Provider| p.auth_mode = "apikey".into(),
            |p: &mut Provider| p.protocol_mode = "adaptive".into(),
            |p: &mut Provider| p.protocol_mode = String::new(),
            |p: &mut Provider| p.base_url = "https://relay.example".into(),
            |p: &mut Provider| p.base_url = "https://api2.cursor.sh/evil".into(),
            |p: &mut Provider| p.base_url = String::new(),
            |p: &mut Provider| p.protocol_endpoints.push(endpoint("cursor-agent")),
            |p: &mut Provider| p.keys.push(key(None)),
            |p: &mut Provider| p.keys.push(key(Some("openai"))),
        ] {
            let mut provider = cursor_provider();
            mutate(&mut provider);
            assert_rejected(&provider);
        }
    }

    #[test]
    fn non_cursor_providers_cannot_declare_or_pin_cursor() {
        let mut provider = cursor_provider();
        provider.vendor = Some("custom".into());
        assert_rejected(&provider);
        provider.vendor = None;
        assert_rejected(&provider);
        provider.protocol = "openai".into();
        provider
            .protocol_endpoints
            .push(endpoint("cursor-agent/run/v1"));
        assert_rejected(&provider);
        provider.protocol_endpoints[0].is_enabled = false;
        assert_rejected(&provider);
        provider.protocol_endpoints.clear();
        provider.keys.push(key(Some("cursor-agent")));
        assert_rejected(&provider);
    }
}
