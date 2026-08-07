use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct YamlConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub providers: Vec<YamlProvider>,
    #[serde(default, rename = "models", alias = "routes")]
    pub models: Vec<YamlModel>,
    #[serde(default)]
    pub settings: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_proxy_host")]
    pub proxy_host: String,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            proxy_host: default_proxy_host(),
            proxy_port: default_proxy_port(),
        }
    }
}

fn default_proxy_host() -> String {
    "127.0.0.1".to_string()
}
fn default_proxy_port() -> u16 {
    19530
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "YamlProviderRaw")]
pub struct YamlProvider {
    pub name: String,
    pub vendor: Option<String>,
    pub default_protocol: Option<String>,
    pub endpoints: IndexMap<String, YamlEndpoint>,
    pub api_key: String,
    pub use_proxy: bool,
    pub models_source: Option<String>,
    pub static_models: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct YamlProviderRaw {
    pub name: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub default_protocol: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub endpoints: IndexMap<String, YamlEndpoint>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub apikey: Option<String>,
    #[serde(default)]
    pub use_proxy: bool,
    #[serde(default)]
    pub models_source: Option<String>,
    #[serde(default)]
    pub static_models: Option<Vec<String>>,
    // Deprecated: capabilities_source was removed; captured here only to emit a warning.
    #[serde(default)]
    pub capabilities_source: Option<serde_json::Value>,
}

impl TryFrom<YamlProviderRaw> for YamlProvider {
    type Error = String;

    fn try_from(r: YamlProviderRaw) -> Result<Self, Self::Error> {
        let default_protocol = match (r.default_protocol, r.protocol) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "provider '{}': 'default_protocol' and its alias 'protocol' cannot both be set",
                    r.name
                ));
            }
            (Some(v), None) | (None, Some(v)) => Some(v),
            (None, None) => None,
        };
        let api_key = match (r.api_key, r.apikey) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "provider '{}': 'api_key' and its alias 'apikey' cannot both be set",
                    r.name
                ));
            }
            (Some(v), None) | (None, Some(v)) => v,
            (None, None) => String::new(),
        };
        if r.capabilities_source.is_some() {
            tracing::warn!(
                provider = %r.name,
                "YAML field 'capabilities_source' is no longer supported and will be ignored; \
                 remove it from your config file"
            );
        }
        Ok(YamlProvider {
            name: r.name,
            vendor: r.vendor,
            default_protocol,
            endpoints: r.endpoints,
            api_key,
            use_proxy: r.use_proxy,
            models_source: r.models_source,
            static_models: r.static_models,
        })
    }
}

impl YamlProvider {
    pub fn resolved_protocol(&self) -> Option<&str> {
        if let Some(p) = self.default_protocol.as_deref() {
            return Some(p);
        }
        self.endpoints.keys().next().map(String::as_str)
    }
}

#[derive(Debug, Deserialize)]
pub struct YamlEndpoint {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_endpoint_auth_scheme")]
    pub auth_scheme: String,
    #[serde(default = "default_endpoint_enabled")]
    pub is_enabled: bool,
    #[serde(default)]
    pub priority: i32,
}

fn default_endpoint_auth_scheme() -> String {
    "auto".to_string()
}

fn default_endpoint_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct YamlModel {
    #[serde(alias = "vmodel", alias = "virtual_model")]
    pub name: String,
    #[serde(default = "default_balance", alias = "strategy")]
    pub balance: String,
    #[serde(default, rename = "backends", alias = "targets")]
    pub backends: Vec<YamlModelBackend>,
    #[serde(default, alias = "access_control")]
    pub enable_auth: bool,
    // Deprecated: route_type / type was removed; captured here only to emit a warning.
    #[serde(default, alias = "type")]
    pub route_type: Option<String>,
}

fn default_balance() -> String {
    "weighted".to_string()
}

#[derive(Debug, Deserialize)]
pub struct YamlModelBackend {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_weight")]
    pub weight: i32,
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_weight() -> i32 {
    100
}
fn default_priority() -> i32 {
    1
}

impl YamlConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config file {path}: {e}"))?;
        let content =
            shellexpand::env_with_context_no_errors(&raw, |var: &str| match std::env::var(var) {
                Ok(val) => Some(val),
                Err(_) => {
                    tracing::warn!(
                        "config: env var '{}' is not set, placeholder left as-is",
                        var
                    );
                    None
                }
            })
            .into_owned();
        let config: Self = serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse YAML config: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let provider_names: Vec<&str> = self.providers.iter().map(|p| p.name.as_str()).collect();
        for (i, p) in self.providers.iter().enumerate() {
            if p.name.trim().is_empty() {
                anyhow::bail!("providers[{i}]: name is required");
            }
            if p.endpoints.is_empty() {
                anyhow::bail!(
                    "providers[{i}] ({}): at least one endpoint is required",
                    p.name
                );
            }
            let resolved = p.resolved_protocol().ok_or_else(|| {
                anyhow::anyhow!(
                    "providers[{i}] ({}): unable to determine protocol from endpoints",
                    p.name
                )
            })?;
            let adaptive = p.endpoints.len() > 1;
            let mut canonical_endpoints = std::collections::HashSet::new();
            let mut enabled_endpoints = std::collections::HashSet::new();
            for (protocol, endpoint) in &p.endpoints {
                let canonical = canonical_yaml_endpoint(protocol, adaptive)?;
                if !canonical_endpoints.insert(canonical.clone()) {
                    anyhow::bail!(
                        "providers[{i}] ({}): duplicate protocol endpoint '{}'",
                        p.name,
                        protocol
                    );
                }
                if endpoint.is_enabled {
                    enabled_endpoints.insert(canonical);
                }
                let url = endpoint
                    .base_url
                    .trim()
                    .parse::<axum::http::Uri>()
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "providers[{i}] ({}): invalid Base URL '{}'",
                            p.name,
                            endpoint.base_url
                        )
                    })?;
                if !matches!(url.scheme_str(), Some("http" | "https")) {
                    anyhow::bail!(
                        "providers[{i}] ({}): Base URL must use http or https",
                        p.name
                    );
                }
                let auth_scheme = endpoint.auth_scheme.trim();
                if !matches!(
                    auth_scheme,
                    "" | "auto" | "bearer" | "x-api-key" | "query" | "none"
                ) {
                    anyhow::bail!(
                        "providers[{i}] ({}): unsupported auth_scheme '{}'",
                        p.name,
                        auth_scheme
                    );
                }
                let api_key = endpoint.api_key.as_deref().unwrap_or(&p.api_key).trim();
                if api_key.is_empty() && auth_scheme != "none" {
                    anyhow::bail!(
                        "providers[{i}] ({}): endpoint '{}' requires api_key",
                        p.name,
                        protocol
                    );
                }
            }
            if enabled_endpoints.is_empty() {
                anyhow::bail!(
                    "providers[{i}] ({}): at least one endpoint must be enabled",
                    p.name
                );
            }
            if adaptive
                && [p.vendor.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|vendor| vendor.trim().eq_ignore_ascii_case("vertexai"))
            {
                anyhow::bail!(
                    "providers[{i}] ({}): adaptive protocol mode does not support Vertex AI providers",
                    p.name
                );
            }
            let resolved_endpoint = canonical_yaml_endpoint(resolved, adaptive)?;
            if !enabled_endpoints.contains(&resolved_endpoint) {
                anyhow::bail!(
                    "providers[{i}] ({}): protocol '{}' has no matching endpoint enabled in 'endpoints'",
                    p.name,
                    resolved
                );
            }
            if p.default_protocol.is_none() && p.endpoints.len() > 1 {
                tracing::warn!(
                    "providers[{i}] ({}): 'protocol' not set and 'endpoints' has {} entries; inferring '{}' as default (set 'protocol' explicitly to silence this warning)",
                    p.name,
                    p.endpoints.len(),
                    resolved
                );
            }
        }
        for (i, m) in self.models.iter().enumerate() {
            if m.name.trim().is_empty() {
                anyhow::bail!("models[{i}]: name is required");
            }
            if m.route_type.is_some() {
                tracing::warn!(
                    model = %m.name,
                    "YAML field 'type' (route_type) is no longer supported and will be ignored; \
                     remove it from your config file"
                );
            }
            if m.backends.is_empty() {
                anyhow::bail!("models[{i}] ({}): at least one backend is required", m.name);
            }
            for (j, b) in m.backends.iter().enumerate() {
                if !provider_names.contains(&b.provider.as_str()) {
                    anyhow::bail!(
                        "models[{i}] ({}): backends[{j}].provider '{}' not found in providers",
                        m.name,
                        b.provider
                    );
                }
            }
        }
        Ok(())
    }
}

fn canonical_yaml_endpoint(raw: &str, require_exact: bool) -> anyhow::Result<String> {
    let registry = nyro_core::protocol::registry::ProtocolRegistry::global();
    if let Some(endpoint) = registry.resolve_alias(raw) {
        return Ok(endpoint.to_string());
    }
    let protocol = registry
        .parse_protocol(raw)
        .ok_or_else(|| anyhow::anyhow!("unsupported protocol endpoint: {raw}"))?;
    let endpoints = registry.list_by_protocol(protocol);
    if require_exact && endpoints.len() != 1 {
        anyhow::bail!(
            "protocol '{raw}' has multiple endpoints; select a concrete protocol endpoint"
        );
    }
    endpoints
        .first()
        .map(|endpoint| endpoint.id().to_string())
        .ok_or_else(|| anyhow::anyhow!("protocol has no registered endpoint: {raw}"))
}

use nyro_core::db::models::{Model, ModelBackend, Provider, ProviderProtocolEndpoint};

pub fn build_providers(yaml: &YamlConfig) -> Vec<Provider> {
    use nyro_core::protocol::registry::ProtocolRegistry;
    let reg = ProtocolRegistry::global();

    yaml.providers
        .iter()
        .enumerate()
        .map(|(i, yp)| {
            let id = format!("yaml-provider-{i}");
            let raw_protocol = yp.resolved_protocol().unwrap_or_default().to_string();
            let adaptive = yp.endpoints.len() > 1;
            let resolved_endpoint = canonical_yaml_endpoint(&raw_protocol, adaptive)
                .expect("validated YAML protocol endpoint");
            let resolved_protocol = if adaptive {
                resolved_endpoint.clone()
            } else {
                reg.parse_protocol(&raw_protocol)
                    .map(|protocol| protocol.as_str().to_string())
                    .unwrap_or_else(|| resolved_endpoint.clone())
            };
            let now = chrono::Utc::now().to_rfc3339();
            let protocol_endpoints = yp
                .endpoints
                .iter()
                .enumerate()
                .map(|(j, (protocol, endpoint))| ProviderProtocolEndpoint {
                    id: format!("{id}-endpoint-{j}"),
                    provider_id: id.clone(),
                    protocol: canonical_yaml_endpoint(protocol, adaptive)
                        .expect("validated YAML endpoint"),
                    base_url: endpoint.base_url.trim().trim_end_matches('/').to_string(),
                    api_key: endpoint
                        .api_key
                        .clone()
                        .unwrap_or_else(|| yp.api_key.clone()),
                    auth_scheme: endpoint.auth_scheme.clone(),
                    is_enabled: endpoint.is_enabled,
                    priority: if endpoint.priority == 0 {
                        j as i32
                    } else {
                        endpoint.priority
                    },
                    test_status: "untested".to_string(),
                    test_error: None,
                    tested_at: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                })
                .collect::<Vec<_>>();
            let default_ep = protocol_endpoints
                .iter()
                .find(|endpoint| endpoint.protocol == resolved_endpoint)
                .expect("validated default endpoint");
            let base_url = default_ep.base_url.clone();
            let api_key = default_ep.api_key.clone();
            Provider {
                id,
                name: yp.name.clone(),
                vendor: yp.vendor.clone(),
                protocol: resolved_protocol,
                base_url,
                protocol_mode: if adaptive { "adaptive" } else { "fixed" }.to_string(),
                protocol_endpoints,
                preset_key: None,
                channel: None,
                models_source: yp.models_source.clone(),
                static_models: yp.static_models.as_ref().map(|v| v.join("\n")),
                api_key,
                auth_mode: "apikey".to_string(),
                use_proxy: yp.use_proxy,
                last_test_success: None,
                last_test_at: None,
                is_enabled: true,
                created_at: now.clone(),
                updated_at: now,
            }
        })
        .collect()
}

pub fn build_models(yaml: &YamlConfig, providers: &[Provider]) -> Vec<Model> {
    let name_to_id: HashMap<&str, &str> = providers
        .iter()
        .map(|p| (p.name.as_str(), p.id.as_str()))
        .collect();

    yaml.models
        .iter()
        .enumerate()
        .map(|(i, ym)| {
            let model_id = format!("yaml-model-{i}");
            let now = chrono::Utc::now().to_rfc3339();

            let backends: Vec<ModelBackend> = ym
                .backends
                .iter()
                .enumerate()
                .map(|(j, yb)| {
                    let provider_id = name_to_id
                        .get(yb.provider.as_str())
                        .unwrap_or(&"")
                        .to_string();
                    ModelBackend {
                        id: format!("{model_id}-backend-{j}"),
                        model_id: model_id.clone(),
                        provider_id,
                        model: yb.model.clone(),
                        weight: yb.weight,
                        priority: yb.priority,
                        created_at: now.clone(),
                    }
                })
                .collect();

            let primary = backends.first();
            Model {
                id: model_id,
                name: ym.name.clone(),
                balance: ym.balance.clone(),
                target_provider: primary.map(|b| b.provider_id.clone()).unwrap_or_default(),
                target_model: primary.map(|b| b.model.clone()).unwrap_or_default(),
                enable_auth: ym.enable_auth,
                enable_payload: None,
                is_enabled: true,
                created_at: now,
                targets: backends,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_provider(yaml: &str) -> Result<YamlProvider, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn canonical_names_work() {
        let yaml = r#"
name: openai
default_protocol: openai
endpoints:
  openai:
    base_url: https://api.openai.com/v1
api_key: sk-canonical
"#;
        let p = parse_provider(yaml).expect("should parse");
        assert_eq!(p.default_protocol.as_deref(), Some("openai"));
        assert_eq!(p.api_key, "sk-canonical");
        assert_eq!(p.resolved_protocol(), Some("openai"));
    }

    #[test]
    fn alias_protocol_and_apikey_work() {
        let yaml = r#"
name: openai
protocol: openai
endpoints:
  openai:
    base_url: https://api.openai.com/v1
apikey: sk-alias
"#;
        let p = parse_provider(yaml).expect("should parse");
        assert_eq!(p.default_protocol.as_deref(), Some("openai"));
        assert_eq!(p.api_key, "sk-alias");
    }

    #[test]
    fn omitted_protocol_single_endpoint_is_inferred() {
        let yaml = r#"
name: openai
endpoints:
  openai:
    base_url: https://api.openai.com/v1
api_key: sk-x
"#;
        let p = parse_provider(yaml).expect("should parse");
        assert!(p.default_protocol.is_none());
        assert_eq!(p.resolved_protocol(), Some("openai"));
    }

    #[test]
    fn omitted_protocol_multi_endpoint_uses_first_declared() {
        let yaml = r#"
name: deepseek
endpoints:
  anthropic:
    base_url: https://api.deepseek.com/anthropic
  openai:
    base_url: https://api.deepseek.com/v1
apikey: sk-x
"#;
        let p = parse_provider(yaml).expect("should parse");
        assert!(p.default_protocol.is_none());
        assert_eq!(p.resolved_protocol(), Some("anthropic"));
    }

    #[test]
    fn conflict_default_protocol_and_protocol_rejects() {
        let yaml = r#"
name: openai
default_protocol: openai
protocol: anthropic
endpoints:
  openai:
    base_url: https://api.openai.com/v1
api_key: sk-x
"#;
        let err = parse_provider(yaml).expect_err("should reject").to_string();
        assert!(
            err.contains("default_protocol") && err.contains("protocol"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn conflict_api_key_and_apikey_rejects() {
        let yaml = r#"
name: openai
protocol: openai
endpoints:
  openai:
    base_url: https://api.openai.com/v1
api_key: sk-a
apikey: sk-b
"#;
        let err = parse_provider(yaml).expect_err("should reject").to_string();
        assert!(
            err.contains("api_key") && err.contains("apikey"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn missing_api_key_rejects_during_validation() {
        let yaml = r#"
providers:
  - name: openai
    protocol: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        let err = cfg.validate().expect_err("should reject").to_string();
        assert!(err.contains("api_key"), "unexpected error: {err}");
    }

    #[test]
    fn adaptive_provider_accepts_endpoint_specific_credentials() {
        let yaml = r#"
providers:
  - name: multi-api
    default_protocol: openai-compatible/chat-completions/v1
    endpoints:
      openai-compatible/chat-completions/v1:
        base_url: https://chat.example/v1
        api_key: sk-chat
        auth_scheme: bearer
      anthropic-messages/messages/2023-06-01:
        base_url: https://messages.example
        api_key: sk-anthropic
        auth_scheme: x-api-key
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");

        let providers = build_providers(&cfg);
        let provider = &providers[0];
        assert_eq!(provider.protocol_mode, "adaptive");
        assert_eq!(provider.protocol, "openai-compatible/chat-completions/v1");
        assert_eq!(provider.base_url, "https://chat.example/v1");
        assert_eq!(provider.api_key, "sk-chat");
        assert_eq!(provider.protocol_endpoints.len(), 2);
        assert_eq!(provider.protocol_endpoints[0].api_key, "sk-chat");
        assert_eq!(provider.protocol_endpoints[1].api_key, "sk-anthropic");
    }

    #[test]
    fn validate_accepts_inferred_protocol() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
models:
  - name: gpt-4o
    backends:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
    }

    #[test]
    fn validate_accepts_legacy_routes_key() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
routes:
  - name: gpt-4o
    targets:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
    }

    #[test]
    fn vmodel_alias_maps_to_name() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
models:
  - vmodel: gpt-4o
    backends:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.models[0].name, "gpt-4o");
    }

    #[test]
    fn virtual_model_alias_maps_to_name() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
models:
  - virtual_model: gpt-4o
    backends:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.models[0].name, "gpt-4o");
    }

    #[test]
    fn build_providers_normalizes_adaptive_default_to_canonical_endpoint() {
        let yaml = r#"
providers:
  - name: vendor1
    protocol: openai
    endpoints:
      openai:
        base_url: https://a.example/v1
      anthropic:
        base_url: https://b.example/v1
    api_key: sk-x
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
        let providers = build_providers(&cfg);
        assert_eq!(providers.len(), 1);
        let p = &providers[0];
        assert_eq!(p.protocol, "openai-compatible/chat-completions/v1");
        assert_eq!(p.base_url, "https://a.example/v1");
        assert_eq!(p.protocol_mode, "adaptive");
    }

    #[test]
    fn validate_rejects_unknown_protocol_without_matching_endpoint() {
        let yaml = r#"
providers:
  - name: openai
    protocol: gemini
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    api_key: sk-x
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("protocol 'gemini'") && err.contains("no matching endpoint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn deprecated_route_type_field_is_silently_parsed() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
models:
  - name: embeddings
    type: embedding
    backends:
      - provider: openai
        model: text-embedding-3-small
  - name: chat
    route_type: chat
    backends:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate()
            .expect("validate must succeed for deprecated type field");
        assert_eq!(cfg.models.len(), 2);

        let providers = build_providers(&cfg);
        let models = build_models(&cfg, &providers);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "embeddings");
        assert_eq!(models[1].name, "chat");
    }

    #[test]
    fn deprecated_capabilities_source_field_is_silently_parsed() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
    capabilities_source: models.dev
models:
  - name: chat
    backends:
      - provider: openai
        model: gpt-4o
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate()
            .expect("validate must succeed for deprecated capabilities_source");

        let providers = build_providers(&cfg);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "openai");
    }

    #[test]
    fn both_deprecated_fields_together_are_accepted() {
        let yaml = r#"
providers:
  - name: openai
    endpoints:
      openai:
        base_url: https://api.openai.com/v1
    apikey: sk-x
    capabilities_source: http
models:
  - name: embeddings
    type: embedding
    backends:
      - provider: openai
        model: text-embedding-3-small
"#;
        let cfg: YamlConfig = serde_yaml::from_str(yaml).expect("parse");
        cfg.validate().expect("validate");
        let providers = build_providers(&cfg);
        let models = build_models(&cfg, &providers);
        assert_eq!(providers.len(), 1);
        assert_eq!(models.len(), 1);
    }
}
