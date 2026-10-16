//! Protocol layer.
//!
//! # Three-layer identity
//!
//! Canonical form: `{protocol}/{name}/{version}`.
//!
//! - `protocol`: closed `Protocol` enum (JSON/HTTP families plus egress-only `devin-chat`).
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

use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolEndpoint};
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
    /// are unchanged.
    pub fn validate_provider(provider: &Provider) -> Result<(), GatewayError> {
        crate::provider::devin::validate(provider)?;
        crate::provider::kiro::validate(provider)
    }

    /// Checked construction for dispatch/admin callers that need an explicit
    /// configuration error rather than the fail-closed compatibility result.
    pub fn try_from_provider(provider: &Provider) -> Result<Self, GatewayError> {
        Self::validate_provider(provider)?;
        Ok(Self::from_provider(provider))
    }

    /// Build from a provider DB row. Invalid native declarations fail closed
    /// with no usable endpoints, never silently falling back to OpenAI.
    /// Call `try_from_provider` when the configuration error should be surfaced.
    pub fn from_provider(provider: &Provider) -> Self {
        if Self::validate_provider(provider).is_err() {
            return Self {
                default: ids::DEVIN_CHAT_V1,
                endpoints: Vec::new(),
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
        if crate::provider::kiro::is_kiro(provider) {
            return Self {
                default: ids::KIRO_CHAT_V1,
                endpoints: vec![ProviderProtocolTarget {
                    record_id: None,
                    protocol: ids::KIRO_CHAT_V1,
                    base_url: provider.base_url.trim().trim_end_matches('/').to_string(),
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
