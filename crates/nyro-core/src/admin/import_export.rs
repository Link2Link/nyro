use super::*;

pub(super) fn import_provider_protocol(provider: &ExportProvider) -> String {
    let configured = if provider.default_protocol.trim().is_empty() {
        provider.protocol.clone()
    } else {
        provider.default_protocol.clone()
    };
    crate::db::models::normalize_legacy_provider_protocol_config(
        &provider.protocol_endpoints,
        &configured,
        &provider.api_key,
    )
    .map(|legacy| legacy.default_protocol)
    .unwrap_or(configured)
}

pub(super) fn import_provider_base_url(provider: &ExportProvider) -> String {
    if !provider.base_url.trim().is_empty() {
        return provider.base_url.clone();
    }
    crate::db::models::normalize_legacy_provider_protocol_config(
        &provider.protocol_endpoints,
        &import_provider_protocol(provider),
        &provider.api_key,
    )
    .and_then(|legacy| legacy.default_base_url().map(ToString::to_string))
    .unwrap_or_default()
}

impl AdminService {
    // ── Config Import/Export ──

    pub async fn export_config(&self) -> anyhow::Result<ExportData> {
        let providers = self.list_providers().await?;
        let models = self.list_models().await?;
        let settings = self.gw.storage.settings().list_all().await?;
        let mut ratings_by_provider: HashMap<String, Vec<ExportProviderModelRating>> =
            HashMap::new();
        if let Some(store) = self.gw.storage.provider_model_ratings() {
            for rating in store.list(None).await? {
                ratings_by_provider
                    .entry(rating.provider_id)
                    .or_default()
                    .push(ExportProviderModelRating {
                        upstream_model: rating.upstream_model,
                        effort: rating.effort,
                        score: rating.score,
                        updated_at: rating.updated_at,
                    });
            }
        }
        for ratings in ratings_by_provider.values_mut() {
            ratings.sort_by(|a, b| a.upstream_model.cmp(&b.upstream_model).then_with(|| a.effort.cmp(&b.effort)));
        }

        Ok(ExportData {
            version: 2,
            providers: providers
                .into_iter()
                .map(|p| {
                    let endpoints = p
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
                        .collect();
                    ExportProvider {
                        model_ratings: ratings_by_provider.remove(&p.id).unwrap_or_default(),
                        name: p.name,
                        vendor: p.vendor,
                        protocol: p.protocol,
                        base_url: p.base_url,
                        protocol_mode: p.protocol_mode,
                        endpoints,
                        default_protocol: String::new(),
                        protocol_endpoints: String::new(),
                        preset_key: p.preset_key,
                        channel: p.channel,
                        models_source: p.models_source,
                        static_models: p.static_models,
                        api_key: p.api_key,
                        auth_mode: p.auth_mode,
                        use_proxy: p.use_proxy,
                        fast_mode: p.fast_mode,
                        is_enabled: p.is_enabled,
                    }
                })
                .collect(),
            models: models
                .into_iter()
                .map(|m| ExportModel {
                    name: m.name,
                    target_model: m.target_model,
                    enable_auth: m.enable_auth,
                    enable_payload: m.enable_payload,
                    force_max_reasoning: m.force_max_reasoning,
                    vision_shim: m.vision_shim,
                    is_enabled: m.is_enabled,
                })
                .collect(),
            settings: settings.into_iter().collect(),
        })
    }

    pub async fn import_config(&self, data: ExportData) -> anyhow::Result<ImportResult> {
        // Validate every nested rating before any configuration is changed.
        for provider in &data.providers {
            super::provider_model_ratings::validate_export_ratings(&provider.model_ratings)?;
        }
        if data
            .providers
            .iter()
            .any(|provider| !provider.model_ratings.is_empty())
        {
            self.rating_store()?;
        }
        let mut providers_imported = 0u32;
        let mut ratings_imported = 0u32;
        let mut models_imported = 0u32;
        let mut settings_imported = 0u32;

        for p in &data.providers {
            let exists = self
                .gw
                .storage
                .providers()
                .exists_by_name(&p.name, None)
                .await?;
            if exists {
                // Existing-name conflicts skip the entire provider, including
                // its ratings. Never overwrite current manual scores.
                continue;
            }

            let legacy = if p.endpoints.is_empty() {
                crate::db::models::normalize_legacy_provider_protocol_config(
                    &p.protocol_endpoints,
                    &import_provider_protocol(p),
                    &p.api_key,
                )
            } else {
                None
            };
            let protocol_mode = if p.protocol_mode.trim() == PROVIDER_PROTOCOL_MODE_ADAPTIVE
                || legacy.as_ref().is_some_and(|config| config.adaptive)
            {
                PROVIDER_PROTOCOL_MODE_ADAPTIVE.to_string()
            } else {
                PROVIDER_PROTOCOL_MODE_FIXED.to_string()
            };
            let protocol_endpoints = if !p.endpoints.is_empty() {
                p.endpoints.clone()
            } else if protocol_mode == PROVIDER_PROTOCOL_MODE_ADAPTIVE {
                legacy
                    .as_ref()
                    .map(|config| config.endpoints.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            let created = self
                .create_provider(CreateProvider {
                    name: p.name.clone(),
                    vendor: p.vendor.clone(),
                    protocol: import_provider_protocol(p),
                    base_url: import_provider_base_url(p),
                    protocol_mode,
                    protocol_endpoints,
                    preset_key: p.preset_key.clone(),
                    channel: p.channel.clone(),
                    models_source: p.models_source.clone(),
                    static_models: p.static_models.clone(),
                    api_key: p.api_key.clone(),
                    auth_mode: p.auth_mode.clone(),
                    use_proxy: p.use_proxy,
                    fast_mode: p.fast_mode,
                })
                .await;
            let created = match created {
                Ok(provider) => provider,
                // Preserve legacy handling of providers with no new metadata,
                // but never silently discard requested rating restoration.
                Err(error) if p.model_ratings.is_empty() => {
                    tracing::warn!(provider = %p.name, error = %error, "provider import skipped");
                    continue;
                }
                Err(error) => return Err(error.context(format!(
                    "Provider import failed after {providers_imported} providers and {ratings_imported} ratings were imported"
                ))),
            };
            if !p.model_ratings.is_empty() {
                let restored: Vec<ProviderModelRating> = p
                    .model_ratings
                    .iter()
                    .map(|rating| ProviderModelRating {
                        provider_id: created.id.clone(),
                        upstream_model: rating.upstream_model.clone(),
                        effort: rating.effort.clone(),
                        score: rating.score,
                        // Already validated before any mutations; retain the
                        // original instant in the canonical UTC millisecond form.
                        updated_at: DateTime::parse_from_rfc3339(&rating.updated_at)
                            .expect("rating timestamp prevalidated")
                            .with_timezone(&Utc)
                            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    })
                    .collect();
                if let Err(error) = self.rating_store()?.restore(&created.id, &restored).await {
                    let rollback = self.delete_provider(&created.id).await;
                    let rollback_detail = match rollback {
                        Ok(()) => "new provider rolled back".to_string(),
                        Err(cleanup_error) => format!(
                            "rollback of new provider {} also failed: {cleanup_error}",
                            created.id
                        ),
                    };
                    return Err(error.context(format!(
                        "Rating import failed ({rollback_detail}); {providers_imported} providers and {ratings_imported} ratings were imported earlier"
                    )));
                }
                ratings_imported += restored.len() as u32;
            }
            providers_imported += 1;
        }

        let fallback_provider_id = self
            .list_providers()
            .await?
            .into_iter()
            .next()
            .map(|provider| provider.id);

        for m in &data.models {
            let exists = self
                .gw
                .storage
                .models()
                .exists_by_name(&m.name, None)
                .await
                .unwrap_or(false);

            if !exists
                && let Some(pid) = fallback_provider_id.clone()
                && self
                    .create_model(CreateModel {
                        name: m.name.clone(),
                        balance: Some("weighted".to_string()),
                        target_provider: pid,
                        target_model: m.target_model.clone(),
                        targets: vec![],
                        enable_auth: Some(m.enable_auth),
                        enable_payload: m.enable_payload,
                        force_max_reasoning: Some(m.force_max_reasoning),
                        vision_shim: m
                            .vision_shim
                            .as_deref()
                            .and_then(|raw| serde_json::from_str(raw).ok()),
                    })
                    .await
                    .is_ok()
            {
                models_imported += 1;
            }
        }

        for (key, value) in &data.settings {
            self.set_setting(key, value).await?;
            settings_imported += 1;
        }

        if providers_imported > 0 || models_imported > 0 {
            self.bump_config_epoch().await?;
        }

        Ok(ImportResult {
            providers_imported,
            ratings_imported,
            models_imported,
            settings_imported,
        })
    }
}
