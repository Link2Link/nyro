use super::*;

#[derive(Debug)]
struct NormalizedProtocolConfig {
    mode: String,
    default_protocol: String,
    base_url: String,
    api_key: String,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
}

/// Per-model result of the "send hi" probe over a provider's model list.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeResult {
    pub model: String,
    pub success: bool,
    pub error: Option<String>,
    pub latency_ms: u64,
    /// Canonical protocol endpoint id used for the probe (e.g.
    /// `openai-compatible/chat-completions/v1`).
    pub protocol: String,
    /// Assistant text received for the "hi" probe (success only).
    pub reply: Option<String>,
}

/// Which protocol/base_url the probe ran through (reported once per run).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeMeta {
    pub protocol: String,
    pub base_url: String,
}

/// Full probe response: shared run metadata plus per-model results.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeOutcome {
    pub meta: ProviderModelProbeMeta,
    pub results: Vec<ProviderModelProbeResult>,
}

fn build_model_probe_request(
    suite: crate::protocol::ids::Protocol,
    base_url: &str,
    api_key: &str,
    auth_scheme: &str,
    runtime_headers: &HeaderMap,
    model: &str,
    is_codex_oauth: bool,
) -> anyhow::Result<(String, HeaderMap, Value)> {
    // Reasoning models (e.g. glm-5.3) burn the whole completion budget on
    // thinking before emitting visible text. 1024 leaves room for both.
    let (path, body) = match suite {
        crate::protocol::ids::Protocol::OpenAICompatible => (
            "/v1/chat/completions",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1024,
                "stream": false,
            }),
        ),
        crate::protocol::ids::Protocol::OpenAIResponses => (
            "/v1/responses",
            if is_codex_oauth {
                // ChatGPT's internal Codex endpoint requires canonical
                // Responses input items and returns SSE even for admin probes.
                serde_json::json!({
                    "model": model,
                    "input": [{
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}],
                    }],
                    "instructions": "You are a helpful assistant.",
                    "store": false,
                    "stream": true,
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "input": "hi",
                    "max_output_tokens": 1024,
                    "stream": false,
                })
            },
        ),
        crate::protocol::ids::Protocol::AnthropicMessages => (
            "/v1/messages",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1024,
            }),
        ),
        crate::protocol::ids::Protocol::GoogleGemini => {
            anyhow::bail!("google-gemini probe is not supported");
        }
    };

    let path = if is_codex_oauth && path == "/v1/responses" {
        "/responses"
    } else {
        path
    };
    let mut url = crate::provider::common::openai::openai_build_url(base_url, path);
    let mut headers = HeaderMap::new();
    match auth_scheme {
        "bearer" => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))?,
            );
        }
        "x-api-key" => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
            if suite == crate::protocol::ids::Protocol::AnthropicMessages {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        "query" => {
            let separator = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{separator}key={api_key}");
        }
        "none" => {}
        other => anyhow::bail!("unsupported auth scheme: {other}"),
    }
    // OAuth runtime identity is provider-owned and authoritative, matching
    // the dispatcher precedence (default auth < RuntimeBinding headers).
    headers.extend(runtime_headers.clone());
    Ok((url, headers, body))
}

/// Probe one model with a minimal non-streaming "hi" request (30s timeout).
async fn probe_single_model(
    client: reqwest::Client,
    suite: crate::protocol::ids::Protocol,
    base_url: &str,
    api_key: &str,
    auth_scheme: &str,
    runtime_headers: &HeaderMap,
    model: &str,
    protocol_id: &str,
    is_codex_oauth: bool,
) -> ProviderModelProbeResult {
    let start = Instant::now();

    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let (url, headers, body) = build_model_probe_request(
            suite,
            base_url,
            api_key,
            auth_scheme,
            runtime_headers,
            model,
            is_codex_oauth,
        )?;

        let response = client
            .post(url)
            .headers(headers)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let preview: String = body_text.chars().take(120).collect();
            anyhow::bail!("HTTP {status}: {preview}");
        }
        let body_text = response.text().await.unwrap_or_default();
        let reply = extract_probe_reply(&body_text)
            .ok_or_else(|| anyhow::anyhow!("response did not contain a readable reply"))?;
        Ok::<String, anyhow::Error>(reply)
    })
    .await;

    let (success, error, reply) = match outcome {
        Ok(Ok(reply)) => (true, None, Some(reply)),
        Ok(Err(error)) => (false, Some(error.to_string()), None),
        Err(_) => (false, Some("timeout after 30s".to_string()), None),
    };
    ProviderModelProbeResult {
        model: model.to_string(),
        success,
        error,
        latency_ms: start.elapsed().as_millis() as u64,
        protocol: protocol_id.to_string(),
        reply,
    }
}

/// Extract the assistant's text reply from a probe response body for any of
/// the three supported wire formats. Returns `None` when no text is present
/// (e.g. content filter, empty choices) — the probe then reports failure.
///
/// Reasoning models may spend the whole budget on thinking; when `content`
/// is empty the reasoning text is used as a fallback (prefixed with a marker
/// so the log makes clear it is a thought, not the final answer).
fn extract_probe_reply(body: &str) -> Option<String> {
    if let Some(reply) = extract_probe_sse_reply(body) {
        return Some(reply);
    }
    let json: Value = serde_json::from_str(body.trim_start()).ok()?;
    extract_probe_reply_json(&json)
}

fn extract_probe_sse_reply(body: &str) -> Option<String> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut completed = false;
    for line in body.lines() {
        let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    reasoning.push_str(delta);
                }
            }
            Some("response.completed" | "response.done") => {
                completed = true;
                if text.trim().is_empty()
                    && let Some(response) = event.get("response")
                    && let Some(reply) = extract_probe_reply_json(response)
                {
                    text.push_str(&reply);
                }
            }
            _ => {}
        }
    }
    if !completed {
        return None;
    }
    if !text.trim().is_empty() {
        Some(text.trim().to_string())
    } else if !reasoning.trim().is_empty() {
        Some(format!("[thinking] {}", reasoning.trim()))
    } else {
        // A completed response proves that the model is callable even when it
        // produced no displayable text for the tiny probe prompt.
        Some("[completed]".to_string())
    }
}

fn extract_probe_reply_json(json: &Value) -> Option<String> {
    let raw = match json {
        // OpenAI chat.completions: choices[0].message.content, falling back
        // to reasoning_content for thinking-only replies.
        Value::Object(map) if map.contains_key("choices") => {
            let message = json.pointer("/choices/0/message")?;
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string);
            match content.filter(|text| !text.trim().is_empty()) {
                Some(text) => Some(text),
                None => message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| format!("[thinking] {}", text)),
            }
        }
        // OpenAI responses: output[] text parts. Reasoning models put their
        // thought in a `reasoning` output item — used as a fallback when no
        // text part was produced.
        Value::Object(map) if map.contains_key("output") => {
            let output = json.pointer("/output")?.as_array()?;
            let text_parts = output
                .iter()
                .filter_map(|item| {
                    let text = item.pointer("/content/0/text").and_then(Value::as_str)?;
                    Some(text.to_string())
                })
                .collect::<Vec<_>>()
                .join("");
            if !text_parts.trim().is_empty() {
                Some(text_parts)
            } else {
                let reasoning = output
                    .iter()
                    .filter_map(|item| {
                        let text = item
                            .pointer("/summary/0/text")
                            .or_else(|| item.get("text"))
                            .and_then(Value::as_str)?;
                        Some(text.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!reasoning.trim().is_empty()).then(|| format!("[thinking] {reasoning}"))
            }
        }
        // Anthropic messages: content[] text blocks. Thinking models emit
        // `thinking` blocks first — used as a fallback when no text block
        // was produced (e.g. budget exhausted mid-thought).
        Value::Object(map) if map.contains_key("content") => {
            let content = json.pointer("/content")?.as_array()?;
            let text_parts = content
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text") {
                        item.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            if !text_parts.trim().is_empty() {
                Some(text_parts)
            } else {
                let thinking = content
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(Value::as_str) == Some("thinking") {
                            item.get("thinking")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (!thinking.trim().is_empty()).then(|| format!("[thinking] {thinking}"))
            }
        }
        _ => None,
    }?;
    let trimmed = raw.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn normalize_protocol_config(
    mode: &str,
    default_protocol: &str,
    base_url: &str,
    api_key: &str,
    auth_mode: &str,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
) -> anyhow::Result<NormalizedProtocolConfig> {
    let mode = match mode.trim() {
        "" | PROVIDER_PROTOCOL_MODE_FIXED => PROVIDER_PROTOCOL_MODE_FIXED,
        PROVIDER_PROTOCOL_MODE_ADAPTIVE => PROVIDER_PROTOCOL_MODE_ADAPTIVE,
        other => anyhow::bail!("unsupported provider protocol_mode: {other}"),
    };
    let registry = crate::protocol::registry::ProtocolRegistry::global();

    if mode == PROVIDER_PROTOCOL_MODE_FIXED {
        let protocol = registry
            .parse_protocol(default_protocol)
            .ok_or_else(|| anyhow::anyhow!("unsupported provider protocol: {default_protocol}"))?
            .as_str()
            .to_string();
        let base_url = normalize_endpoint_url(base_url)?;
        return Ok(NormalizedProtocolConfig {
            mode: mode.to_string(),
            default_protocol: protocol.clone(),
            base_url: base_url.clone(),
            api_key: api_key.to_string(),
            endpoints: vec![CreateProviderProtocolEndpoint {
                protocol,
                base_url,
                api_key: api_key.to_string(),
                auth_scheme: "auto".to_string(),
                is_enabled: true,
                priority: 0,
            }],
        });
    }

    if auth_mode.trim() != "apikey" {
        anyhow::bail!("adaptive protocol mode currently supports API key providers only");
    }
    if endpoints.is_empty() {
        anyhow::bail!("adaptive protocol mode requires at least one protocol endpoint");
    }

    let mut normalized = Vec::with_capacity(endpoints.len());
    let mut seen = std::collections::HashSet::new();
    for (index, endpoint) in endpoints.into_iter().enumerate() {
        let protocol = normalize_adaptive_endpoint_protocol(&endpoint.protocol)?;
        if !seen.insert(protocol.clone()) {
            anyhow::bail!("duplicate adaptive protocol endpoint: {protocol}");
        }
        let auth_scheme = match endpoint.auth_scheme.trim() {
            "" | "auto" => "auto",
            "bearer" => "bearer",
            "x-api-key" => "x-api-key",
            "query" => "query",
            "none" => "none",
            other => anyhow::bail!("unsupported endpoint auth_scheme: {other}"),
        };
        if endpoint.api_key.trim().is_empty() && auth_scheme != "none" {
            anyhow::bail!("API key is required for adaptive endpoint {protocol}");
        }
        normalized.push(CreateProviderProtocolEndpoint {
            protocol,
            base_url: normalize_endpoint_url(&endpoint.base_url)?,
            api_key: endpoint.api_key,
            auth_scheme: auth_scheme.to_string(),
            is_enabled: endpoint.is_enabled,
            priority: if endpoint.priority == 0 {
                index as i32
            } else {
                endpoint.priority
            },
        });
    }

    if !normalized.iter().any(|endpoint| endpoint.is_enabled) {
        anyhow::bail!("adaptive protocol mode requires at least one enabled endpoint");
    }

    let default_endpoint = resolve_default_adaptive_endpoint(default_protocol, &normalized)?;
    Ok(NormalizedProtocolConfig {
        mode: mode.to_string(),
        default_protocol: default_endpoint.protocol.clone(),
        base_url: default_endpoint.base_url.clone(),
        api_key: default_endpoint.api_key.clone(),
        endpoints: normalized,
    })
}

fn normalize_adaptive_endpoint_protocol(raw: &str) -> anyhow::Result<String> {
    let registry = crate::protocol::registry::ProtocolRegistry::global();
    if let Some(endpoint) = registry.resolve_alias(raw) {
        return Ok(endpoint.to_string());
    }
    let protocol = registry
        .parse_protocol(raw)
        .ok_or_else(|| anyhow::anyhow!("unsupported protocol endpoint: {raw}"))?;
    let endpoints = registry.list_by_protocol(protocol);
    if endpoints.len() != 1 {
        anyhow::bail!(
            "protocol '{raw}' has multiple endpoints; select a concrete protocol endpoint"
        );
    }
    Ok(endpoints[0].id().to_string())
}

fn resolve_default_adaptive_endpoint<'a>(
    raw: &str,
    endpoints: &'a [CreateProviderProtocolEndpoint],
) -> anyhow::Result<&'a CreateProviderProtocolEndpoint> {
    let registry = crate::protocol::registry::ProtocolRegistry::global();
    if let Some(default) = registry.resolve_alias(raw) {
        let canonical = default.to_string();
        return endpoints
            .iter()
            .find(|endpoint| endpoint.is_enabled && endpoint.protocol == canonical)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "default protocol endpoint is not configured or enabled: {canonical}"
                )
            });
    }

    let suite = registry
        .parse_protocol(raw)
        .ok_or_else(|| anyhow::anyhow!("unsupported default protocol: {raw}"))?;
    let matches = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.is_enabled
                && registry
                    .resolve_alias(&endpoint.protocol)
                    .is_some_and(|candidate| candidate.protocol == suite)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [endpoint] => Ok(*endpoint),
        [] => anyhow::bail!("default protocol has no configured endpoint: {raw}"),
        _ => anyhow::bail!(
            "default protocol '{raw}' is ambiguous; select a concrete protocol endpoint"
        ),
    }
}

fn normalize_endpoint_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed)
        .map_err(|_| anyhow::anyhow!("invalid provider Base URL: {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("provider Base URL must use http or https: {raw}");
    }
    Ok(trimmed.to_string())
}

fn ensure_adaptive_provider_supported(
    mode: &str,
    vendor: Option<&str>,
    preset_key: Option<&str>,
) -> anyhow::Result<()> {
    if mode.trim() != PROVIDER_PROTOCOL_MODE_ADAPTIVE {
        return Ok(());
    }
    let is_vertex = [vendor, preset_key]
        .into_iter()
        .flatten()
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case("vertexai"));
    if is_vertex {
        anyhow::bail!("adaptive protocol mode does not support Vertex AI providers");
    }
    Ok(())
}

impl AdminService {
    // ── Providers ──

    pub async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
        self.gw.storage.providers().list().await
    }

    pub async fn list_provider_presets(&self) -> anyhow::Result<Vec<Value>> {
        parse_provider_presets_snapshot()
    }

    pub async fn get_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.gw
            .storage
            .providers()
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider not found: {id}"))
    }
    pub async fn create_provider(&self, input: CreateProvider) -> anyhow::Result<Provider> {
        let name = normalize_name(&input.name, "provider name")?;
        self.ensure_provider_name_unique(None, &name).await?;
        let vendor = normalize_vendor(input.vendor.as_deref());
        ensure_adaptive_provider_supported(
            &input.protocol_mode,
            vendor.as_deref(),
            input.preset_key.as_deref(),
        )?;
        let auth_mode = resolve_admin_preset_channel_auth_mode(
            input.preset_key.as_deref(),
            input.channel.as_deref(),
        )
        .unwrap_or(input.auth_mode);
        let api_key = if auth_mode == "oauth" {
            String::new()
        } else {
            input.api_key
        };
        let protocol = normalize_protocol_config(
            &input.protocol_mode,
            &input.protocol,
            &input.base_url,
            &api_key,
            &auth_mode,
            input.protocol_endpoints,
        )?;
        let provider = self
            .gw
            .storage
            .providers()
            .create(CreateProvider {
                name,
                vendor,
                protocol: protocol.default_protocol,
                base_url: protocol.base_url,
                protocol_mode: protocol.mode,
                protocol_endpoints: protocol.endpoints,
                preset_key: input.preset_key,
                channel: input.channel,
                models_source: input.models_source,
                static_models: input.static_models,
                api_key: protocol.api_key,
                auth_mode,
                use_proxy: input.use_proxy,
                fast_mode: input.fast_mode,
            })
            .await?;

        let provider = if provider.is_adaptive() {
            let _ = self.test_provider(&provider.id).await?;
            self.get_provider(&provider.id).await?
        } else {
            provider
        };
        self.gw.quota_registry.request_refresh(&provider.id);
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn copy_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.copy_provider_with_options(id, CopyProviderOptions::default())
            .await
    }

    pub async fn copy_provider_with_options(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        let original = self.get_provider(id).await?;
        let name = self.next_provider_copy_name(&original.name).await?;
        let copied = self
            .create_provider(CreateProvider {
                name,
                vendor: original.vendor.clone(),
                protocol: original.protocol.clone(),
                base_url: original.base_url.clone(),
                protocol_mode: original.protocol_mode.clone(),
                protocol_endpoints: original
                    .protocol_endpoints
                    .iter()
                    .map(|endpoint| CreateProviderProtocolEndpoint {
                        protocol: endpoint.protocol.clone(),
                        base_url: endpoint.base_url.clone(),
                        api_key: endpoint.api_key.clone(),
                        auth_scheme: endpoint.auth_scheme.clone(),
                        is_enabled: endpoint.is_enabled,
                        priority: endpoint.priority,
                    })
                    .collect(),
                preset_key: original.preset_key.clone(),
                channel: original.channel.clone(),
                models_source: original.models_source.clone(),
                static_models: original.static_models.clone(),
                api_key: original.api_key.clone(),
                auth_mode: original.auth_mode.clone(),
                use_proxy: original.use_proxy,
                fast_mode: original.fast_mode,
            })
            .await?;
        let copied = self
            .update_provider(
                &copied.id,
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await?;

        let copied = if original.effective_auth_mode() == "oauth" {
            match self
                .gw
                .storage
                .oauth_credentials()
                .get(&original.id)
                .await?
            {
                Some(credential) => {
                    let credential_input = upsert_credential_from_oauth(&credential);
                    let provisioned = async {
                        self.gw
                            .storage
                            .oauth_credentials()
                            .upsert(&copied.id, credential_input)
                            .await?;
                        let driver_key = credential.driver_key.clone();
                        let stored = stored_credential_from_oauth(&credential, &driver_key);
                        self.sync_provider_runtime_fields(&copied, &stored).await
                    }
                    .await;

                    match provisioned {
                        Ok(provider) => provider,
                        Err(error) => {
                            if let Err(cleanup_error) = self.delete_provider(&copied.id).await {
                                tracing::warn!(
                                    "failed to rollback copied oauth provider {} after provisioning error: {}",
                                    copied.id,
                                    cleanup_error
                                );
                            }
                            return Err(error.context("copy oauth provider"));
                        }
                    }
                }
                None => copied,
            }
        } else {
            copied
        };

        if options.append_targets {
            self.append_provider_targets(&original.id, &copied.id)
                .await?;
        }

        Ok(copied)
    }

    pub async fn update_provider(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        let current = self.get_provider(id).await?;
        let current_base_url = current.base_url.clone();
        let protocol_config_changed = input.protocol_mode.is_some()
            || input.protocol.is_some()
            || input.base_url.is_some()
            || input.api_key.is_some()
            || input.auth_mode.is_some()
            || input.protocol_endpoints.is_some();
        let quota_config_changed = protocol_config_changed
            || input.vendor.is_some()
            || input.preset_key.is_some()
            || input.channel.is_some()
            || input.is_enabled.is_some();
        let models_source_input = input
            .models_source
            .clone()
            .map(|value| value.trim().to_string());

        let name = normalize_name(
            &input.name.clone().unwrap_or_else(|| current.name.clone()),
            "provider name",
        )?;
        self.ensure_provider_name_unique(Some(id), &name).await?;
        let vendor = if input.vendor.is_some() {
            normalize_vendor(input.vendor.as_deref())
        } else {
            normalize_vendor(current.vendor.as_deref())
        };
        let models_source = models_source_input
            .or_else(|| current.models_source.as_deref().map(ToString::to_string));
        let raw_protocol = input
            .protocol
            .clone()
            .unwrap_or_else(|| current.protocol.clone());
        let raw_base_url = input
            .base_url
            .clone()
            .unwrap_or_else(|| current.base_url.clone());
        let preset_key = input.preset_key.clone().or(current.preset_key.clone());
        let channel = input.channel.clone().or(current.channel.clone());
        let static_models = input
            .static_models
            .clone()
            .or(current.static_models.clone());
        let raw_api_key = input
            .api_key
            .clone()
            .unwrap_or_else(|| current.api_key.clone());
        let auth_mode =
            resolve_admin_preset_channel_auth_mode(preset_key.as_deref(), channel.as_deref())
                .or(input.auth_mode.clone())
                .unwrap_or_else(|| current.auth_mode.clone());
        let raw_api_key = if auth_mode == "oauth" {
            String::new()
        } else {
            raw_api_key
        };
        let protocol_mode = input
            .protocol_mode
            .as_deref()
            .unwrap_or(&current.protocol_mode);
        ensure_adaptive_provider_supported(
            protocol_mode,
            vendor.as_deref(),
            preset_key.as_deref(),
        )?;
        let raw_endpoints = input.protocol_endpoints.clone().unwrap_or_else(|| {
            current
                .protocol_endpoints
                .iter()
                .map(|endpoint| CreateProviderProtocolEndpoint {
                    protocol: endpoint.protocol.clone(),
                    base_url: endpoint.base_url.clone(),
                    api_key: endpoint.api_key.clone(),
                    auth_scheme: endpoint.auth_scheme.clone(),
                    is_enabled: endpoint.is_enabled,
                    priority: endpoint.priority,
                })
                .collect()
        });
        let protocol = normalize_protocol_config(
            protocol_mode,
            &raw_protocol,
            &raw_base_url,
            &raw_api_key,
            &auth_mode,
            raw_endpoints,
        )?;
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let fast_mode = input.fast_mode.unwrap_or(current.fast_mode);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let base_url_changed = protocol.base_url != current_base_url;

        let mut provider = self
            .gw
            .storage
            .providers()
            .update(
                id,
                UpdateProvider {
                    name: Some(name),
                    vendor,
                    protocol: Some(protocol.default_protocol),
                    base_url: Some(protocol.base_url),
                    protocol_mode: Some(protocol.mode),
                    protocol_endpoints: protocol_config_changed.then_some(protocol.endpoints),
                    preset_key,
                    channel,
                    models_source,
                    static_models,
                    api_key: Some(protocol.api_key),
                    auth_mode: Some(auth_mode),
                    use_proxy: Some(use_proxy),
                    fast_mode: Some(fast_mode),
                    is_enabled: Some(is_enabled),
                },
            )
            .await?;

        if base_url_changed {
            self.gw.clear_ollama_capability_cache_for_provider(id).await;
        }

        if protocol_config_changed && provider.is_adaptive() {
            let _ = self.test_provider(id).await?;
            provider = self.get_provider(id).await?;
        }

        if quota_config_changed {
            if provider.is_enabled {
                self.gw.quota_registry.invalidate(id);
            } else {
                self.gw.quota_registry.remove(id);
            }
        }
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.gw.storage.providers().delete(id).await?;
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        self.gw.clear_ollama_capability_cache_for_provider(id).await;
        self.gw.quota_registry.remove(id);
        Ok(())
    }

    async fn ensure_provider_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .gw
            .storage
            .providers()
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "PROVIDER_NAME_CONFLICT",
                &format!("provider name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }

    async fn next_provider_copy_name(&self, original_name: &str) -> anyhow::Result<String> {
        let base = format!("{}_Copy", normalize_name(original_name, "provider name")?);
        if !self
            .gw
            .storage
            .providers()
            .exists_by_name(&base, None)
            .await?
        {
            return Ok(base);
        }

        for index in 2.. {
            let candidate = format!("{base}{index}");
            if !self
                .gw
                .storage
                .providers()
                .exists_by_name(&candidate, None)
                .await?
            {
                return Ok(candidate);
            }
        }

        unreachable!("unbounded provider copy name search must return");
    }

    async fn append_provider_targets(
        &self,
        original_provider_id: &str,
        copied_provider_id: &str,
    ) -> anyhow::Result<()> {
        let models = self.list_models().await?;
        for model in models.into_iter().filter(|model| {
            model
                .targets
                .iter()
                .any(|target| target.provider_id == original_provider_id)
        }) {
            let mut targets = model
                .targets
                .iter()
                .map(|target| CreateModelBackend {
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                })
                .collect::<Vec<_>>();

            let copied_targets = model
                .targets
                .iter()
                .filter(|target| target.provider_id == original_provider_id)
                .map(|target| CreateModelBackend {
                    provider_id: copied_provider_id.to_string(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                });
            targets.extend(copied_targets);

            self.update_model(
                &model.id,
                UpdateModel {
                    targets: Some(
                        targets
                            .into_iter()
                            .map(|target| UpsertModelBackend {
                                id: None,
                                provider_id: target.provider_id,
                                model: target.model,
                                weight: target.weight,
                                priority: target.priority,
                            })
                            .collect(),
                    ),
                    ..UpdateModel::default()
                },
            )
            .await?;
        }
        Ok(())
    }
    pub async fn test_provider(&self, id: &str) -> anyhow::Result<TestResult> {
        let provider = self.get_provider(id).await?;
        self.gw
            .clear_ollama_capability_cache_for_provider(&provider.id)
            .await;
        if provider.is_adaptive() {
            let result = self.test_adaptive_provider_endpoints(&provider).await?;
            self.record_provider_test_result(&provider.id, &result)
                .await?;
            return Ok(result);
        }
        let start = Instant::now();
        let protocol = provider.protocol.trim();
        let vertex_runtime = if vertexai::is_vertex_vendor(&provider) {
            Some(self.resolve_provider_runtime(&provider).await?)
        } else {
            None
        };
        let base_url_owned = vertex_runtime
            .as_ref()
            .and_then(|runtime| runtime.binding.base_url_override.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| provider.base_url.clone());
        let base_url = base_url_owned.trim();

        let result = if base_url.is_empty() {
            TestResult {
                success: false,
                latency_ms: 0,
                model: None,
                error: Some("Base URL is empty".to_string()),
                endpoints: Vec::new(),
            }
        } else {
            let mut failures: Vec<String> = Vec::new();
            if reqwest::Url::parse(base_url).is_err() {
                failures.push(format!("{protocol}: Base URL format is invalid"));
            } else {
                let mut request = self
                    .gw
                    .http_client
                    .get(base_url)
                    .timeout(Duration::from_secs(10));
                if let Some(runtime) = &vertex_runtime {
                    let mut headers = runtime_binding_headers(&runtime.binding)?;
                    if !runtime.binding.disable_default_auth {
                        headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {}", runtime.access_token))?,
                        );
                    }
                    request = request.headers(headers);
                }
                if let Err(e) = request.send().await {
                    failures.push(format!("{protocol}: {}", format_connectivity_error(&e)));
                }
            }

            if failures.is_empty() {
                TestResult {
                    success: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: None,
                    endpoints: Vec::new(),
                }
            } else {
                TestResult {
                    success: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: Some(format!(
                        "Connectivity check failed for provider endpoint: {}",
                        failures.join("; ")
                    )),
                    endpoints: Vec::new(),
                }
            }
        };
        self.record_provider_test_result(&provider.id, &result)
            .await?;
        Ok(result)
    }

    async fn test_adaptive_provider_endpoints(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<TestResult> {
        let start = Instant::now();
        let endpoints = provider
            .protocol_endpoints
            .iter()
            .filter(|endpoint| endpoint.is_enabled)
            .cloned()
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Ok(TestResult {
                success: false,
                latency_ms: 0,
                model: None,
                error: Some("Adaptive provider has no enabled protocol endpoints".to_string()),
                endpoints: Vec::new(),
            });
        }

        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let results = futures::future::join_all(
            endpoints
                .into_iter()
                .map(|endpoint| self.test_adaptive_endpoint(client.clone(), endpoint)),
        )
        .await;

        let mut endpoint_results = Vec::with_capacity(results.len());
        for result in results {
            self.gw
                .storage
                .providers()
                .record_endpoint_test_result(
                    &result.endpoint_id,
                    ProviderEndpointTestResult {
                        success: result.success,
                        error: result.error.clone(),
                        tested_at: result.tested_at.clone(),
                    },
                )
                .await?;
            endpoint_results.push(result);
        }

        let failures = endpoint_results
            .iter()
            .filter(|result| !result.success)
            .map(|result| {
                format!(
                    "{}: {}",
                    result.protocol,
                    result.error.as_deref().unwrap_or("connection failed")
                )
            })
            .collect::<Vec<_>>();
        Ok(TestResult {
            success: failures.is_empty(),
            latency_ms: start.elapsed().as_millis() as u64,
            model: None,
            error: (!failures.is_empty()).then(|| {
                format!(
                    "Connectivity check failed for adaptive endpoint(s): {}",
                    failures.join("; ")
                )
            }),
            endpoints: endpoint_results,
        })
    }

    async fn test_adaptive_endpoint(
        &self,
        client: reqwest::Client,
        endpoint: ProviderProtocolEndpoint,
    ) -> EndpointTestResult {
        let start = Instant::now();
        let tested_at = Utc::now().to_rfc3339();
        let outcome = async {
            let mut url = reqwest::Url::parse(endpoint.base_url.trim())?;
            let protocol = crate::protocol::registry::ProtocolRegistry::global()
                .resolve_alias(&endpoint.protocol)
                .ok_or_else(|| anyhow::anyhow!("unsupported protocol endpoint"))?;
            let auth_scheme = match endpoint.auth_scheme.trim() {
                "" | "auto" => match protocol.protocol {
                    crate::protocol::ids::Protocol::AnthropicMessages => "x-api-key",
                    crate::protocol::ids::Protocol::GoogleGemini => "query",
                    _ => "bearer",
                },
                explicit => explicit,
            };
            let mut headers = HeaderMap::new();
            match auth_scheme {
                "bearer" => {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {}", endpoint.api_key))?,
                    );
                }
                "x-api-key" => {
                    headers.insert("x-api-key", HeaderValue::from_str(&endpoint.api_key)?);
                    if protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages {
                        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                    }
                }
                "query" => {
                    url.query_pairs_mut().append_pair("key", &endpoint.api_key);
                }
                "none" => {}
                other => anyhow::bail!("unsupported endpoint auth scheme: {other}"),
            }

            let response = client
                .get(url)
                .headers(headers)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|error| anyhow::anyhow!(format_connectivity_error(&error)))?;
            let status = response.status();
            if matches!(status.as_u16(), 401 | 403) || status.is_server_error() {
                anyhow::bail!("HTTP {status}");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        EndpointTestResult {
            endpoint_id: endpoint.id,
            protocol: endpoint.protocol,
            base_url: endpoint.base_url,
            success: outcome.is_ok(),
            latency_ms: start.elapsed().as_millis() as u64,
            error: outcome.err().map(|error| error.to_string()),
            tested_at,
        }
    }

    /// Send a minimal "hi" chat request to every model in the provider's
    /// discovered list and report which ones actually answer. The WebUI uses
    /// the results to hide non-callable models from route target pickers.
    pub async fn probe_provider_models(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderModelProbeOutcome> {
        use futures::StreamExt;

        let provider = self.get_provider(id).await?;
        let models = self.get_provider_models(id).await?;
        if models.is_empty() {
            anyhow::bail!("provider model list is empty");
        }

        // Pick the probe endpoint: adaptive providers probe through the
        // endpoint matching their configured default protocol
        // (`provider.protocol`); if the default is disabled or missing, fall
        // back to the first enabled endpoint. Fixed providers probe through
        // their single configuration. Fixed OAuth providers resolve the same
        // refreshed token, base URL, and identity headers used by dispatch.
        let registry = crate::protocol::registry::ProtocolRegistry::global();
        let runtime = if provider.is_adaptive() {
            None
        } else {
            Some(self.resolve_provider_runtime(&provider).await?)
        };
        let (suite_raw, base_url, api_key, auth_scheme, runtime_headers) = if provider.is_adaptive()
        {
            let enabled: Vec<&ProviderProtocolEndpoint> = provider
                .protocol_endpoints
                .iter()
                .filter(|endpoint| endpoint.is_enabled)
                .collect();
            if enabled.is_empty() {
                anyhow::bail!("provider has no enabled protocol endpoints");
            }
            let preferred = enabled
                .iter()
                .find(|endpoint| endpoint.protocol == provider.protocol)
                .or_else(|| enabled.first())
                .expect("enabled endpoints is non-empty");
            (
                preferred.protocol.clone(),
                preferred.base_url.clone(),
                preferred.api_key.clone(),
                preferred.auth_scheme.clone(),
                HeaderMap::new(),
            )
        } else {
            let runtime = runtime.expect("fixed provider runtime was resolved above");
            let base_url = runtime
                .binding
                .base_url_override
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| provider.base_url.clone());
            let auth_scheme = if runtime.binding.disable_default_auth {
                "none".to_string()
            } else {
                "auto".to_string()
            };
            (
                provider.protocol.clone(),
                base_url,
                runtime.access_token,
                auth_scheme,
                runtime_binding_headers(&runtime.binding)?,
            )
        };

        let suite = registry
            .parse_protocol(&suite_raw)
            .ok_or_else(|| anyhow::anyhow!("unsupported provider protocol: {suite_raw}"))?;
        if suite == crate::protocol::ids::Protocol::GoogleGemini {
            anyhow::bail!("model probe does not support google-gemini providers");
        }
        if base_url.trim().is_empty() {
            anyhow::bail!("provider base URL is empty");
        }
        if api_key.trim().is_empty()
            && auth_scheme.trim() != "none"
            && !runtime_headers.contains_key(AUTHORIZATION)
        {
            anyhow::bail!("provider api key is empty");
        }

        let effective_scheme = match auth_scheme.trim() {
            "" | "auto" => match suite {
                crate::protocol::ids::Protocol::AnthropicMessages => "x-api-key",
                _ => "bearer",
            },
            explicit => explicit,
        };

        let is_codex_oauth = provider.effective_auth_mode().trim() == "oauth"
            && provider
                .vendor
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("openai"))
            && provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("codex"));
        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let base_url = base_url.trim().to_string();
        let api_key = api_key.trim().to_string();
        let protocol_id = registry
            .resolve_alias(&suite_raw)
            .map(|endpoint| endpoint.to_string())
            .unwrap_or_else(|| suite_raw.clone());

        let mut results: Vec<ProviderModelProbeResult> = futures::stream::iter(models)
            .map(|model| {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = api_key.clone();
                let scheme = effective_scheme.to_string();
                let runtime_headers = runtime_headers.clone();
                let protocol_id = protocol_id.clone();
                async move {
                    probe_single_model(
                        client,
                        suite,
                        &base_url,
                        &api_key,
                        &scheme,
                        &runtime_headers,
                        &model,
                        &protocol_id,
                        is_codex_oauth,
                    )
                    .await
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;
        results.sort_by(|a, b| a.model.cmp(&b.model));
        Ok(ProviderModelProbeOutcome {
            meta: ProviderModelProbeMeta {
                protocol: protocol_id,
                base_url,
            },
            results,
        })
    }

    async fn record_provider_test_result(
        &self,
        provider_id: &str,
        result: &TestResult,
    ) -> anyhow::Result<()> {
        self.gw
            .storage
            .providers()
            .record_test_result(
                provider_id,
                ProviderTestResult {
                    success: result.success,
                    tested_at: String::new(),
                },
            )
            .await
    }

    pub async fn test_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        // Adaptive providers: the discovery endpoint is OpenAI-style even when
        // the default protocol is not — authenticate with an enabled
        // OpenAI-family endpoint's Bearer key instead of the default
        // protocol's scheme (e.g. Anthropic `x-api-key`).
        let (auth_protocol, auth_credential) = match adaptive_model_fetch_auth(&provider) {
            Some((protocol, api_key)) => (protocol, api_key),
            None => (provider.protocol.clone(), credential),
        };
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models: Vec<String> = static_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }
        let endpoint = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| provider.effective_models_source().map(ToString::to_string))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Model Discovery URL is empty"))?;

        if let Some(models) = lookup_models_dev_models(&self.gw.config.data_dir, &endpoint)? {
            if models.is_empty() {
                anyhow::bail!("Model list format is invalid or empty");
            }
            return Ok(models);
        }

        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(&endpoint)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if is_google_protocol(&auth_protocol) && !runtime.binding.disable_default_auth {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{endpoint}{separator}key={}", auth_credential))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("HTTP {status}: {preview}");
        }

        let json: Value = resp.json().await.unwrap_or_default();
        let models =
            extract_models_from_response(&provider.protocol, provider.vendor.as_deref(), &json);
        if models.is_empty() {
            anyhow::bail!("Model list format is invalid or empty");
        }

        Ok(merge_model_lists(models, preset_extra_models(&provider)))
    }
    pub async fn get_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        // Same adaptive-auth rationale as `test_provider_models` above.
        let (auth_protocol, auth_credential) = match adaptive_model_fetch_auth(&provider) {
            Some((protocol, api_key)) => (protocol, api_key),
            None => (provider.protocol.clone(), credential),
        };
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models: Vec<String> = static_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }

        if let Some(endpoint) = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| resolve_models_endpoint(&provider))
        {
            if let Some(models) = lookup_models_dev_models(&self.gw.config.data_dir, &endpoint)?
                && !models.is_empty()
            {
                return Ok(models);
            }

            let mut headers = if runtime.binding.disable_default_auth {
                HeaderMap::new()
            } else {
                build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?
            };
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            let mut request = self.gw.http_client.get(&endpoint).headers(headers);

            if is_google_protocol(&auth_protocol) && !runtime.binding.disable_default_auth {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let mut headers = build_model_headers(
                    &auth_protocol,
                    provider.vendor.as_deref(),
                    &auth_credential,
                )?;
                headers.extend(runtime_binding_headers(&runtime.binding)?);
                request = self
                    .gw
                    .http_client
                    .get(format!("{endpoint}{separator}key={}", auth_credential))
                    .headers(headers);
            }

            if let Ok(resp) = request.send().await
                && resp.status().is_success()
            {
                let json: Value = resp.json().await.unwrap_or_default();
                let models = extract_models_from_response(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &json,
                );
                if !models.is_empty() {
                    return Ok(merge_model_lists(models, preset_extra_models(&provider)));
                }
            }
        }

        let static_list = parse_static_models(provider.static_models.as_deref());
        let extra = preset_extra_models(&provider);
        if !extra.is_empty() {
            return Ok(merge_model_lists(static_list, extra));
        }
        Ok(static_list)
    }

    pub async fn get_model_capabilities(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let provider = self.get_provider(provider_id).await?;
        let trimmed_model = model.trim();
        if trimmed_model.is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        self.resolve_provider_model_capabilities(&provider, trimmed_model)
            .await
    }

    async fn resolve_provider_model_capabilities(
        &self,
        provider: &Provider,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        match preset_capabilities_source(provider) {
            CapabilitiesSource::ModelsDev(vendor_key) => {
                let matched =
                    lookup_models_dev_capability(&self.gw.config.data_dir, vendor_key, model);
                matched.ok_or_else(|| {
                    anyhow::anyhow!("no matched model capabilities found in models.dev")
                })
            }
            CapabilitiesSource::Http(url) => {
                if is_ollama_show_endpoint(url) {
                    self.query_ollama_show_capability(url, model).await
                } else {
                    self.query_http_capability(provider, url, model).await
                }
            }
            CapabilitiesSource::Auto => Ok(fuzzy_match_models_dev(&self.gw.config.data_dir, model)
                .ok_or_else(|| {
                    anyhow::anyhow!("no matched model capabilities found in auto mode")
                })?),
        }
    }

    async fn query_http_capability(
        &self,
        provider: &Provider,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let runtime = self.resolve_provider_runtime(provider).await?;
        let credential = runtime.access_token;
        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(url)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if is_google_protocol(&provider.protocol) && !runtime.binding.disable_default_auth {
            let separator = if url.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{url}{separator}key={}", credential))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("capability source returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        if let Some(cap) = parse_http_capability(&json, model) {
            return Ok(cap);
        }
        anyhow::bail!("no matched model capabilities found from capability source")
    }

    async fn query_ollama_show_capability(
        &self,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let resp = self
            .gw
            .http_client
            .post(url)
            .json(&serde_json::json!({ "name": model }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("ollama /api/show returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        Ok(parse_ollama_capability(&json, model))
    }
}

#[cfg(test)]
mod probe_reply_tests {
    use super::*;

    #[test]
    fn openai_chat_reply_prefers_content() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello","reasoning_content":"thought"}}]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello"));
    }

    #[test]
    fn openai_chat_falls_back_to_reasoning_when_content_empty() {
        // Reasoning model whose visible text was cut by the token budget.
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"","reasoning_content":"Let me think"}}]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] Let me think")
        );
    }

    #[test]
    fn openai_responses_text_parts_win() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"hmm"}]},
            {"type":"message","content":[{"type":"output_text","text":"hi there"}]}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hi there"));
    }

    #[test]
    fn openai_responses_falls_back_to_reasoning_summary() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"pondering"}]}
        ]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] pondering")
        );
    }

    #[test]
    fn anthropic_text_blocks_win() {
        let body = r#"{"content":[
            {"type":"thinking","thinking":"internal"},
            {"type":"text","text":"answer"}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn anthropic_falls_back_to_thinking_block() {
        let body = r#"{"content":[
            {"type":"thinking","thinking":"budget exhausted mid-thought"}
        ]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] budget exhausted mid-thought")
        );
    }

    #[test]
    fn no_text_anywhere_is_none() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":""}}]}"#;
        assert_eq!(extract_probe_reply(body), None);
        let body = r#"{"content":[]}"#;
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn oauth_runtime_headers_authoritatively_authenticate_model_probe() {
        let runtime_headers = HeaderMap::from_iter([
            (
                AUTHORIZATION,
                HeaderValue::from_static("Bearer oauth-access-token"),
            ),
            (
                reqwest::header::HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_static("account-1"),
            ),
            (
                reqwest::header::HeaderName::from_static("originator"),
                HeaderValue::from_static("Codex Desktop"),
            ),
        ]);
        let (url, headers, body) = build_model_probe_request(
            crate::protocol::ids::Protocol::OpenAIResponses,
            "https://chatgpt.com/backend-api/codex",
            "",
            "none",
            &runtime_headers,
            "gpt-5-codex",
            true,
        )
        .unwrap();

        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer oauth-access-token"
        );
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-1");
        assert_eq!(headers.get("originator").unwrap(), "Codex Desktop");
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("gpt-5-codex")
        );
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
        assert_eq!(body.get("store").and_then(Value::as_bool), Some(false));
        assert_eq!(
            body.pointer("/input/0/content/0/type")
                .and_then(Value::as_str),
            Some("input_text")
        );
        assert_eq!(
            body.pointer("/input/0/content/0/text")
                .and_then(Value::as_str),
            Some("hi")
        );
    }

    #[test]
    fn codex_sse_probe_extracts_text_and_requires_completion() {
        let completed = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"he\"}\n\n",
            "data:{\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
        );
        assert_eq!(extract_probe_reply(completed).as_deref(), Some("hello"));

        let truncated = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        assert_eq!(extract_probe_reply(truncated), None);
    }

    #[test]
    fn codex_sse_completed_without_text_still_proves_model_is_callable() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn runtime_authorization_overrides_default_probe_api_key() {
        let runtime_headers = HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth-access-token"),
        )]);
        let (_, headers, _) = build_model_probe_request(
            crate::protocol::ids::Protocol::OpenAIResponses,
            "https://example.com",
            "legacy-api-key",
            "bearer",
            &runtime_headers,
            "gpt-test",
            false,
        )
        .unwrap();

        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer oauth-access-token"
        );
    }
}

#[cfg(test)]
mod adaptive_protocol_tests {
    use super::*;

    fn endpoint(protocol: &str, priority: i32) -> CreateProviderProtocolEndpoint {
        CreateProviderProtocolEndpoint {
            protocol: protocol.to_string(),
            base_url: format!("https://{}.example", protocol.split('/').nth(1).unwrap()),
            api_key: format!("key-{priority}"),
            auth_scheme: "auto".to_string(),
            is_enabled: true,
            priority,
        }
    }

    #[test]
    fn adaptive_config_keeps_explicit_priorities_and_fills_zero_values() {
        let normalized = normalize_protocol_config(
            "adaptive",
            "openai-compatible/chat-completions/v1",
            "",
            "",
            "apikey",
            vec![
                endpoint("openai-compatible/chat-completions/v1", 0),
                endpoint("anthropic-messages/messages/2023-06-01", 7),
                endpoint("openai-responses/responses/v1", 0),
            ],
        )
        .unwrap();

        assert_eq!(
            normalized.default_protocol,
            "openai-compatible/chat-completions/v1"
        );
        assert_eq!(
            normalized
                .endpoints
                .iter()
                .map(|endpoint| endpoint.priority)
                .collect::<Vec<_>>(),
            vec![0, 7, 2]
        );
    }

    #[test]
    fn adaptive_config_rejects_disabled_default_endpoint() {
        let mut disabled = endpoint("openai-compatible/chat-completions/v1", 0);
        disabled.is_enabled = false;
        let default_protocol = disabled.protocol.clone();
        let error = normalize_protocol_config(
            "adaptive",
            &default_protocol,
            "",
            "",
            "apikey",
            vec![
                disabled,
                endpoint("anthropic-messages/messages/2023-06-01", 1),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("not configured or enabled"));
    }

    #[test]
    fn adaptive_config_rejects_vertex_provider_selection() {
        let error =
            ensure_adaptive_provider_supported("adaptive", Some("vertexai"), None).unwrap_err();
        assert!(error.to_string().contains("does not support Vertex AI"));
    }
}
