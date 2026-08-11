use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::provider::AuthMode;
use crate::provider::VendorRegistry;

pub fn default_provider_auth_mode() -> String {
    "apikey".to_string()
}

pub const PROVIDER_PROTOCOL_MODE_FIXED: &str = "fixed";
pub const PROVIDER_PROTOCOL_MODE_ADAPTIVE: &str = "adaptive";

pub fn default_provider_protocol_mode() -> String {
    PROVIDER_PROTOCOL_MODE_FIXED.to_string()
}

pub fn default_provider_endpoint_auth_scheme() -> String {
    "auto".to_string()
}

pub fn default_provider_endpoint_enabled() -> bool {
    true
}

pub fn is_valid_provider_auth_mode(value: &str) -> bool {
    matches!(value.trim(), "apikey" | "oauth")
}

fn auth_mode_to_legacy(mode: AuthMode) -> &'static str {
    // Legacy DB / WebUI vocabulary only knows "apikey" / "oauth"; the
    // newer `setuptoken` mode degrades to "apikey" for storage purposes
    // (the OAuth driver layer knows the real flow via vendor metadata).
    match mode {
        AuthMode::ApiKey => "apikey",
        AuthMode::OAuth => "oauth",
        AuthMode::SetupToken => "apikey",
    }
}

/// Resolve the authentication mode for a `(preset_key, channel_id)`
/// pair by consulting the in-process `VendorRegistry`. Falls back to
/// the preset's `default` channel, then to `None` when the vendor is
/// unknown to the registry.
pub fn resolve_preset_channel_auth_mode(
    preset_key: Option<&str>,
    channel_id: Option<&str>,
) -> Option<String> {
    let preset_key = preset_key?.trim();
    if preset_key.is_empty() {
        return None;
    }
    let requested_channel = channel_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    let metadata = VendorRegistry::global().metadata(preset_key)?;
    let channel = metadata
        .channels
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(requested_channel))
        .or_else(|| metadata.channels.iter().find(|c| c.id == "default"))?;
    Some(auth_mode_to_legacy(channel.auth_mode).to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub protocol: String,
    pub base_url: String,
    #[serde(default = "default_provider_protocol_mode")]
    pub protocol_mode: String,
    #[serde(default)]
    #[sqlx(skip)]
    pub protocol_endpoints: Vec<ProviderProtocolEndpoint>,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsEndpoint")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: String,
    #[serde(default = "default_provider_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub use_proxy: bool,
    pub last_test_success: Option<bool>,
    pub last_test_at: Option<String>,
    pub is_enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct ProviderProtocolEndpoint {
    pub id: String,
    pub provider_id: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_provider_endpoint_auth_scheme")]
    pub auth_scheme: String,
    pub is_enabled: bool,
    pub priority: i32,
    pub test_status: String,
    pub test_error: Option<String>,
    pub tested_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateProviderProtocolEndpoint {
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_provider_endpoint_auth_scheme")]
    pub auth_scheme: String,
    #[serde(default = "default_provider_endpoint_enabled")]
    pub is_enabled: bool,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyProviderProtocolConfig {
    pub default_protocol: String,
    pub endpoints: Vec<CreateProviderProtocolEndpoint>,
    pub adaptive: bool,
}

impl LegacyProviderProtocolConfig {
    pub fn default_base_url(&self) -> Option<&str> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.protocol == self.default_protocol)
            .map(|endpoint| endpoint.base_url.as_str())
    }
}

/// Convert the removed JSON protocol map into normalized endpoint rows.
/// Legacy protocol keys represented suites, so each key expands to every
/// registered endpoint in that suite, matching the pre-collapse runtime.
pub(crate) fn normalize_legacy_provider_protocol_config(
    raw: &str,
    default_protocol: &str,
    api_key: &str,
) -> Option<LegacyProviderProtocolConfig> {
    let registry = crate::protocol::registry::ProtocolRegistry::global();
    let value = serde_json::from_str::<serde_json::Value>(raw.trim()).ok()?;
    let entries = value.as_object()?;
    let mut endpoints = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut valid_sources = 0usize;

    for (raw_protocol, entry) in entries {
        let Some(protocol) = registry.parse_protocol(raw_protocol) else {
            continue;
        };
        let Some(base_url) = entry
            .as_object()
            .and_then(|object| object.get("base_url"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        valid_sources += 1;

        for handler in registry.list_by_protocol(protocol) {
            let endpoint = handler.id().to_string();
            if !seen.insert(endpoint.clone()) {
                continue;
            }
            endpoints.push(CreateProviderProtocolEndpoint {
                protocol: endpoint,
                base_url: base_url.to_string(),
                api_key: api_key.to_string(),
                auth_scheme: "auto".to_string(),
                is_enabled: true,
                priority: endpoints.len() as i32,
            });
        }
    }

    let default = registry
        .resolve_alias(default_protocol)
        .filter(|candidate| {
            endpoints
                .iter()
                .any(|endpoint| endpoint.protocol == candidate.to_string())
        })
        .or_else(|| {
            let protocol = registry.parse_protocol(default_protocol)?;
            registry
                .list_by_protocol(protocol)
                .into_iter()
                .map(|handler| handler.id())
                .find(|candidate| {
                    endpoints
                        .iter()
                        .any(|endpoint| endpoint.protocol == candidate.to_string())
                })
        })
        .or_else(|| {
            endpoints
                .first()
                .and_then(|endpoint| registry.resolve_alias(&endpoint.protocol))
        })?;

    Some(LegacyProviderProtocolConfig {
        default_protocol: default.to_string(),
        endpoints,
        adaptive: valid_sources > 1,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct OAuthCredential {
    pub provider_id: String,
    pub driver_key: String,
    pub scheme: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: String,
    pub meta: String,
    pub status: String,
    pub status_version: i32,
    pub last_error: Option<String>,
    pub last_refresh_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpsertOAuthCredential {
    pub driver_key: String,
    pub scheme: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub resource_url: Option<String>,
    pub subject_id: Option<String>,
    pub scopes: Option<String>,
    pub meta: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub balance: String,
    pub target_provider: String,
    pub target_model: String,
    #[serde(alias = "access_control")]
    pub enable_auth: bool,
    pub enable_payload: Option<bool>,
    pub is_enabled: bool,
    pub created_at: String,
    #[serde(default)]
    #[sqlx(skip)]
    pub targets: Vec<ModelBackend>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ModelBackend {
    pub id: String,
    pub model_id: String,
    pub provider_id: String,
    pub model: String,
    pub weight: i32,
    pub priority: i32,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ModelBalance {
    /// Weighted reservoir sampling — targets with higher weight are preferred.
    #[default]
    Weighted,
    /// Priority groups — lower priority number tried first; random within group.
    Priority,
    /// Cooldown-aware round-robin — deprioritises recently-used targets.
    Cooldown,
    /// Latency-ordered — targets sorted by ascending EMA response latency.
    Latency,
}

impl ModelBalance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Priority => "priority",
            Self::Cooldown => "cooldown",
            Self::Latency => "latency",
        }
    }
}

impl std::str::FromStr for ModelBalance {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "weighted" => Ok(Self::Weighted),
            "priority" => Ok(Self::Priority),
            "cooldown" => Ok(Self::Cooldown),
            "latency" => Ok(Self::Latency),
            other => anyhow::bail!("unsupported model balance: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKey {
    pub id: String,
    #[serde(rename = "key")]
    pub token: String,
    pub name: String,
    pub rpm: Option<i32>,
    pub rpd: Option<i32>,
    pub tpm: Option<i32>,
    pub tpd: Option<i32>,
    pub is_enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyWithBindings {
    pub id: String,
    #[serde(rename = "key")]
    pub token: String,
    pub name: String,
    pub rpm: Option<i32>,
    pub rpd: Option<i32>,
    pub tpm: Option<i32>,
    pub tpd: Option<i32>,
    pub is_enabled: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(alias = "route_ids")]
    pub model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RequestLog {
    pub id: String,
    /// Unix 毫秒时间戳
    pub created_at: i64,
    pub api_key_id: Option<String>,
    pub api_key_name: Option<String>,

    pub client_protocol: Option<String>,
    pub upstream_protocol: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    #[serde(alias = "route_id")]
    pub model_id: Option<String>,
    #[serde(alias = "route_name")]
    pub model_name: Option<String>,
    pub upstream_url: Option<String>,
    pub client_model: Option<String>,
    pub upstream_model: Option<String>,

    /// 客户端请求的归一化推理强度快照（不受载荷记录开关影响）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    pub method: Option<String>,
    pub path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_response_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_response_body: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_response_headers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_response_body: Option<String>,

    pub upstream_status_code: Option<i32>,
    pub client_status_code: Option<i32>,

    pub latency_total_ms: Option<i64>,
    pub latency_upstream_ms: Option<i64>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    #[serde(default)]
    pub cache_read_tokens: i32,

    pub is_stream: bool,
    pub stream_chunks_count: i32,
    pub stream_first_chunk_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProvider {
    pub name: String,
    pub vendor: Option<String>,
    pub protocol: String,
    pub base_url: String,
    #[serde(default = "default_provider_protocol_mode")]
    pub protocol_mode: String,
    #[serde(default)]
    pub protocol_endpoints: Vec<CreateProviderProtocolEndpoint>,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsSource")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: String,
    #[serde(default = "default_provider_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub use_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateProvider {
    pub name: Option<String>,
    pub vendor: Option<String>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub protocol_mode: Option<String>,
    pub protocol_endpoints: Option<Vec<CreateProviderProtocolEndpoint>>,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsSource")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: Option<String>,
    pub auth_mode: Option<String>,
    pub use_proxy: Option<bool>,
    pub is_enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateModel {
    #[serde(alias = "virtual_model", alias = "vmodel")]
    pub name: Option<String>,
    #[serde(rename = "balance", alias = "strategy")]
    pub balance: Option<String>,
    pub target_provider: Option<String>,
    pub target_model: Option<String>,
    #[serde(default)]
    pub targets: Option<Vec<UpsertModelBackend>>,
    #[serde(alias = "access_control")]
    pub enable_auth: Option<bool>,
    pub enable_payload: Option<Option<bool>>,
    pub is_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateModel {
    #[serde(alias = "virtual_model", alias = "vmodel")]
    pub name: String,
    #[serde(rename = "balance", alias = "strategy")]
    pub balance: Option<String>,
    pub target_provider: String,
    pub target_model: String,
    #[serde(default)]
    pub targets: Vec<CreateModelBackend>,
    #[serde(alias = "access_control")]
    pub enable_auth: Option<bool>,
    pub enable_payload: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateModelBackend {
    pub provider_id: String,
    pub model: String,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertModelBackend {
    pub id: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKey {
    pub name: String,
    pub rpm: Option<i32>,
    pub rpd: Option<i32>,
    pub tpm: Option<i32>,
    pub tpd: Option<i32>,
    pub expires_at: Option<String>,
    #[serde(default, alias = "route_ids")]
    pub model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateApiKey {
    pub name: Option<String>,
    pub rpm: Option<i32>,
    pub rpd: Option<i32>,
    pub tpm: Option<i32>,
    pub tpd: Option<i32>,
    pub is_enabled: Option<bool>,
    pub expires_at: Option<String>,
    #[serde(alias = "route_ids")]
    pub model_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LogQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status_min: Option<i32>,
    pub status_max: Option<i32>,
    pub api_key: Option<String>,
    /// Unix 毫秒时间戳，筛选 created_at >= after
    pub after: Option<i64>,
    /// Unix 毫秒时间戳，筛选 created_at <= before
    pub before: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogPage {
    pub items: Vec<RequestLog>,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, FromRow)]
pub struct StatsOverview {
    pub total_requests: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub avg_duration_ms: f64,
    pub error_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct StatsHourly {
    pub hour: String,
    pub request_count: i64,
    pub error_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub avg_duration_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ModelStats {
    pub model: String,
    pub request_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub avg_duration_ms: f64,
    pub total_upstream_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProviderStats {
    pub provider: String,
    pub request_count: i64,
    pub error_count: i64,
    pub avg_duration_ms: f64,
    pub total_output_tokens: i64,
    pub total_upstream_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKeyStats {
    pub api_key_id: String,
    pub api_key_name: String,
    pub request_count: i64,
    pub error_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub cache_read_tokens: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub success: bool,
    pub latency_ms: u64,
    pub model: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointTestResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointTestResult {
    pub endpoint_id: String,
    pub protocol: String,
    pub base_url: String,
    pub success: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub tested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub provider: String,
    pub model_id: String,
    pub context_window: u64,
    pub embedding_length: Option<u64>,
    pub output_max_tokens: Option<u64>,
    pub tool_call: bool,
    pub reasoning: bool,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportData {
    pub version: u32,
    pub providers: Vec<ExportProvider>,
    #[serde(alias = "routes")]
    pub models: Vec<ExportModel>,
    pub settings: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportProvider {
    pub name: String,
    pub vendor: Option<String>,
    pub protocol: String,
    pub base_url: String,
    #[serde(default = "default_provider_protocol_mode")]
    pub protocol_mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<CreateProviderProtocolEndpoint>,
    #[serde(default, skip_serializing)]
    pub default_protocol: String,
    #[serde(default, skip_serializing)]
    pub protocol_endpoints: String,
    pub preset_key: Option<String>,
    pub channel: Option<String>,
    #[serde(alias = "modelsEndpoint")]
    pub models_source: Option<String>,
    pub static_models: Option<String>,
    pub api_key: String,
    #[serde(default = "default_provider_auth_mode")]
    pub auth_mode: String,
    #[serde(default)]
    pub use_proxy: bool,
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportModel {
    #[serde(alias = "virtual_model")]
    pub name: String,
    pub target_model: String,
    #[serde(alias = "access_control")]
    pub enable_auth: bool,
    #[serde(default)]
    pub enable_payload: Option<bool>,
    pub is_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub providers_imported: u32,
    #[serde(alias = "routes_imported")]
    pub models_imported: u32,
    pub settings_imported: u32,
}

impl Provider {
    pub fn is_adaptive(&self) -> bool {
        self.protocol_mode.trim() == PROVIDER_PROTOCOL_MODE_ADAPTIVE
    }

    pub fn effective_auth_mode(&self) -> String {
        resolve_preset_channel_auth_mode(self.preset_key.as_deref(), self.channel.as_deref())
            .unwrap_or_else(|| {
                let mode = self.auth_mode.trim();
                if mode.is_empty() {
                    default_provider_auth_mode()
                } else {
                    mode.to_string()
                }
            })
    }

    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}

impl CreateProvider {
    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}

impl UpdateProvider {
    pub fn effective_models_source(&self) -> Option<&str> {
        self.models_source
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }
}
