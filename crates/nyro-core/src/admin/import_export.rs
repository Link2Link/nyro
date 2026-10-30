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
        let provider_ids: std::collections::HashSet<_> =
            providers.iter().map(|p| p.id.clone()).collect();
        let models = self.list_models().await?;
        let settings = self.gw.storage.settings().list_all().await?;
        let mut model_ratings = self
            .list_model_ratings()
            .await?
            .into_iter()
            .map(|entry| ExportModelRating {
                model_prefix: entry.model_prefix,
                score: entry.score,
                updated_at: entry.updated_at,
            })
            .collect::<Vec<_>>();
        model_ratings.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));

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
                    let keys = p
                        .keys
                        .iter()
                        .map(|key| {
                            let effective = key.effective_models();
                            UpsertProviderKey {
                                id: None,
                                name: key.name.clone(),
                                api_key: key.api_key.clone(),
                                protocol: key.protocol.clone(),
                                base_url: key.base_url.clone(),
                                is_enabled: key.is_enabled,
                                priority: key.priority,
                                manual_models: Some(match effective {
                                    Some(models) => crate::db::models::encode_model_list(&models),
                                    None => key.manual_models.clone().unwrap_or_else(|| {
                                        crate::db::models::encode_model_list(&[])
                                    }),
                                }),
                            }
                        })
                        .collect();
                    ExportProvider {
                        source_id: Some(p.id),
                        name: p.name,
                        vendor: p.vendor,
                        protocol: p.protocol,
                        base_url: p.base_url,
                        protocol_mode: p.protocol_mode,
                        endpoints,
                        keys,
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
            model_ratings,
            settings: settings
                .into_iter()
                .filter(|(key, _)| {
                    !key.starts_with("provider_account_fence:")
                        && key
                            .strip_prefix(super::account_pool::POOL_SETTING_PREFIX)
                            .is_none_or(|id| provider_ids.contains(id))
                })
                .map(|(key, value)| {
                    let value = if key.starts_with(super::account_pool::POOL_SETTING_PREFIX) {
                        // Legacy selected plans are read compatibility only, not exported policy.
                        let config: DevinPoolConfig = serde_json::from_str(&value)?;
                        serde_json::to_string(&config)?
                    } else {
                        value
                    };
                    Ok((key, value))
                })
                .collect::<anyhow::Result<_>>()?,
        })
    }

    pub async fn import_config(&self, data: ExportData) -> anyhow::Result<ImportResult> {
        anyhow::ensure!(
            matches!(data.version, 1 | 2),
            "Unsupported configuration format version"
        );
        // A None destination means the source provider was skipped (name conflict
        // or create failure). Unknown historical identities are not guessed.
        let mut price_provider_ids: HashMap<String, Option<String>> = HashMap::new();
        for provider in &data.providers {
            if let Some(source_id) = &provider.source_id {
                if source_id.trim().is_empty() || price_provider_ids.contains_key(source_id) {
                    return Err(TokenPriceError::InvalidInput(
                        "Provider source_id must be nonblank and unique in the import".into(),
                    )
                    .into());
                }
                price_provider_ids.insert(source_id.clone(), None);
            }
        }
        // Validate reserved price settings and ratings before any configuration changes.
        for (key, value) in &data.settings {
            super::token_price::validate_setting(key, value)?;
            if let Some(source_id) = key.strip_prefix(super::account_pool::POOL_SETTING_PREFIX) {
                let config: DevinPoolConfig =
                    serde_json::from_str(value).context("Invalid account pool configuration")?;
                config.validate()?;
                let provider = data
                    .providers
                    .iter()
                    .find(|p| p.source_id.as_deref() == Some(source_id))
                    .context("Account pool setting has no source provider")?;
                anyhow::ensure!(
                    provider.vendor.as_deref() == Some("devin") && provider.auth_mode == "oauth",
                    "Account pool setting requires a Devin OAuth provider"
                );
                // Reject Claude-only policies during preflight, not after the
                // currently supported Devin import has already created providers.
                anyhow::ensure!(
                    config.strategy != DevinPoolStrategy::WeeklyResetFirst,
                    "weekly_reset_first strategy requires a Claude Code OAuth provider"
                );
            }
        }
        super::model_ratings::validate_export_ratings(&data.model_ratings)?;
        if !data.model_ratings.is_empty() {
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
                // Existing-name conflicts skip the entire provider. Never
                // overwrite current configuration.
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
                    keys: p
                        .keys
                        .iter()
                        .map(|key| UpsertProviderKey {
                            id: None,
                            name: key.name.clone(),
                            api_key: key.api_key.clone(),
                            protocol: key.protocol.clone(),
                            base_url: key.base_url.clone(),
                            is_enabled: key.is_enabled,
                            priority: key.priority,
                            manual_models: key.manual_models.clone(),
                        })
                        .collect(),
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
            match created {
                Ok(provider) => {
                    if let Some(source_id) = &p.source_id {
                        price_provider_ids.insert(source_id.clone(), Some(provider.id));
                    }
                    providers_imported += 1;
                }
                // Preserve legacy handling of providers that cannot be created.
                Err(error) => {
                    tracing::warn!(provider = %p.name, error = %error, "provider import skipped");
                    continue;
                }
            }
        }

        // Ratings replace the whole prefix table after providers land; old
        // backups' nested per-provider ratings were already dropped by serde.
        if !data.model_ratings.is_empty() {
            let restored: Vec<ModelRatingEntry> = data
                .model_ratings
                .iter()
                .map(|entry| ModelRatingEntry {
                    // Canonical lowercase key; already validated pre-mutation.
                    model_prefix: canonical_model_prefix(&entry.model_prefix),
                    score: entry.score,
                    // Retain the original instant in the canonical UTC millisecond form.
                    updated_at: DateTime::parse_from_rfc3339(&entry.updated_at)
                        .expect("rating timestamp prevalidated")
                        .with_timezone(&Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                })
                .collect();
            self.rating_store()?.restore(&restored).await?;
            ratings_imported = restored.len() as u32;
        }

        let current_providers = self.list_providers().await?;
        let fallback_provider_id = current_providers
            .first()
            .map(|provider| provider.id.clone());
        let current_provider_ids: std::collections::HashSet<_> = current_providers
            .into_iter()
            .map(|provider| provider.id)
            .collect();

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
            if key.starts_with("provider_account_fence:") {
                continue;
            }
            if let Some(source_id) = key.strip_prefix(super::account_pool::POOL_SETTING_PREFIX) {
                // Never attach imported policy to a pre-existing/conflicting provider.
                if let Some(Some(target_id)) = price_provider_ids.get(source_id) {
                    self.configure_provider_account_pool(target_id, serde_json::from_str(value)?)
                        .await?;
                    settings_imported += 1;
                }
                continue;
            }
            let Some((key, value)) = super::token_price::remap_import_setting(
                key,
                value,
                &price_provider_ids,
                &current_provider_ids,
            )?
            else {
                continue;
            };
            self.set_setting(&key, &value).await?;
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
