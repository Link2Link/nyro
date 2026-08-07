use super::*;

#[derive(Debug)]
struct NormalizedProtocolConfig {
    mode: String,
    default_protocol: String,
    base_url: String,
    api_key: String,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
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
            })
            .await?;

        if provider.is_adaptive() {
            let _ = self.test_provider(&provider.id).await?;
            return self.get_provider(&provider.id).await;
        }
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

        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.gw.storage.providers().delete(id).await?;
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        self.gw.clear_ollama_capability_cache_for_provider(id).await;
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
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(&endpoint)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if is_google_protocol(&provider.protocol) && !runtime.binding.disable_default_auth {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{endpoint}{separator}key={}", credential))
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

        Ok(models)
    }
    pub async fn get_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
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
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
            };
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            let mut request = self.gw.http_client.get(&endpoint).headers(headers);

            if is_google_protocol(&provider.protocol) && !runtime.binding.disable_default_auth {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let mut headers = build_model_headers(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &credential,
                )?;
                headers.extend(runtime_binding_headers(&runtime.binding)?);
                request = self
                    .gw
                    .http_client
                    .get(format!("{endpoint}{separator}key={}", credential))
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
                    return Ok(models);
                }
            }
        }

        Ok(parse_static_models(provider.static_models.as_deref()))
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
