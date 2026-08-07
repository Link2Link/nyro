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
                    is_enabled: m.is_enabled,
                })
                .collect(),
            settings: settings.into_iter().collect(),
        })
    }

    pub async fn import_config(&self, data: ExportData) -> anyhow::Result<ImportResult> {
        let mut providers_imported = 0u32;
        let mut models_imported = 0u32;
        let mut settings_imported = 0u32;

        for p in &data.providers {
            let exists = self
                .gw
                .storage
                .providers()
                .exists_by_name(&p.name, None)
                .await
                .unwrap_or(false);

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

            if !exists
                && self
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
                    })
                    .await
                    .is_ok()
            {
                providers_imported += 1;
            }
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
            models_imported,
            settings_imported,
        })
    }
}
