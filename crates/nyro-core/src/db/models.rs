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
    /// Channel-specific fast-mode switch (e.g. sub2api): when enabled,
    /// outbound OpenAI Responses requests get `service_tier: "priority"`
    /// injected unless the client already set the field.
    #[serde(default)]
    #[sqlx(default)]
    pub fast_mode: bool,
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
    #[serde(default)]
    pub fast_mode: bool,
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
    pub fast_mode: Option<bool>,
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
pub struct StatsTimeBucket {
    pub bucket_start: i64,
    pub request_count: i64,
    pub error_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub avg_duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsTimeSeries {
    pub start_at: i64,
    pub end_at: i64,
    pub bucket_minutes: i32,
    pub has_data: bool,
    pub points: Vec<StatsTimeBucket>,
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

#[derive(Debug, Clone, Default, FromRow)]
pub struct ModelUsageTotals {
    pub request_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub last_called_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct RecentModelPerformance {
    pub output_tokens: i32,
    pub is_stream: bool,
    pub stream_chunks_count: i32,
    pub latency_upstream_ms: Option<i64>,
    pub latency_total_ms: Option<i64>,
    pub stream_first_chunk_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsageStats {
    pub request_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cache_read_tokens: i64,
    pub last_called_at: Option<i64>,
    pub recent_sample_count: i64,
    pub average_tps: Option<f64>,
    pub average_first_token_ms: Option<f64>,
}

impl ModelUsageStats {
    pub fn from_samples(totals: ModelUsageTotals, samples: &[RecentModelPerformance]) -> Self {
        let mut tps_total = 0.0;
        let mut tps_count = 0;
        let mut first_token_total = 0.0;
        let mut first_token_count = 0;

        for sample in samples {
            let is_stream = sample.is_stream || sample.stream_chunks_count > 0;
            let generation_ms = match (
                is_stream,
                sample.latency_upstream_ms,
                sample.stream_first_chunk_ms,
            ) {
                (true, Some(upstream), Some(first_token)) if upstream > 0 => {
                    let generation = upstream - first_token;
                    let looks_non_incremental = generation < 50
                        || generation <= 0
                        || first_token as f64 / upstream as f64 >= 0.8;
                    Some(if looks_non_incremental {
                        upstream
                    } else {
                        generation
                    })
                }
                _ => sample.latency_upstream_ms.or(sample.latency_total_ms),
            };

            if sample.output_tokens > 0 {
                if let Some(generation_ms) = generation_ms.filter(|value| *value > 0) {
                    tps_total += sample.output_tokens as f64 / (generation_ms as f64 / 1000.0);
                    tps_count += 1;
                }
            }
            if let Some(first_token_ms) = sample.stream_first_chunk_ms.filter(|value| *value >= 0) {
                first_token_total += first_token_ms as f64;
                first_token_count += 1;
            }
        }

        Self {
            request_count: totals.request_count,
            total_input_tokens: totals.total_input_tokens,
            total_output_tokens: totals.total_output_tokens,
            total_cache_read_tokens: totals.total_cache_read_tokens,
            last_called_at: totals.last_called_at,
            recent_sample_count: samples.len() as i64,
            average_tps: (tps_count > 0).then_some(tps_total / tps_count as f64),
            average_first_token_ms: (first_token_count > 0)
                .then_some(first_token_total / first_token_count as f64),
        }
    }
}

#[cfg(test)]
mod model_usage_stats_tests {
    use super::*;

    #[test]
    fn averages_recent_request_performance() {
        let totals = ModelUsageTotals {
            request_count: 12,
            total_input_tokens: 1_200,
            total_output_tokens: 600,
            total_cache_read_tokens: 300,
            last_called_at: Some(1_700_000_000_000),
        };
        let samples = vec![
            RecentModelPerformance {
                output_tokens: 100,
                is_stream: true,
                stream_chunks_count: 10,
                latency_upstream_ms: Some(2_000),
                latency_total_ms: Some(2_100),
                stream_first_chunk_ms: Some(500),
            },
            RecentModelPerformance {
                output_tokens: 50,
                is_stream: false,
                stream_chunks_count: 0,
                latency_upstream_ms: Some(1_000),
                latency_total_ms: Some(1_100),
                stream_first_chunk_ms: None,
            },
        ];

        let stats = ModelUsageStats::from_samples(totals, &samples);

        assert_eq!(stats.request_count, 12);
        assert_eq!(stats.recent_sample_count, 2);
        assert!((stats.average_tps.unwrap() - 58.333).abs() < 0.01);
        assert_eq!(stats.average_first_token_ms, Some(500.0));
    }
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
    #[serde(default)]
    pub fast_mode: bool,
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
