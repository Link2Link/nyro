//! Explicit account-scoped generation probes. Never advance pool scheduling,
//! fall back to a sibling, or resolve a provider's ambiguous singleton token.
use super::*;
use crate::provider::account_pool::{ClaudeAccountState, DevinAccountState, KiroAccountState};

#[derive(Debug, thiserror::Error)]
pub enum ModelProbeAccountError {
    #[error("Probe account ID must be a non-empty string of at most 256 bytes")]
    InvalidId,
    #[error("Account selection is only supported for Devin, Kiro or Claude Code OAuth providers")]
    Unsupported,
    #[error("Selected probe account was not found in this provider")]
    NotFound,
    #[error("Selected probe account is disabled, disconnected or not authorized")]
    Unavailable,
    #[error("Selected probe account changed during the test; select it again and retry")]
    Changed,
    #[error(
        "Selected probe account credential refresh failed; refresh or reauthorize this account"
    )]
    RefreshFailed,
    #[error("Unable to read selected probe account state; retry later")]
    StateUnavailable,
}

pub fn normalize_probe_account_id(id: Option<String>) -> anyhow::Result<Option<String>> {
    id.map(|id| {
        let id = id.trim();
        anyhow::ensure!(
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
            ModelProbeAccountError::InvalidId
        );
        Ok(id.to_owned())
    })
    .transpose()
}

fn driver_for(provider: &Provider) -> &'static str {
    if crate::provider::kiro::is_kiro(provider) {
        "kiro"
    } else if crate::provider::devin::is_devin(provider) {
        "devin"
    } else {
        "claude-code"
    }
}

fn validate_account(
    provider: &Provider,
    account: &OAuthCredential,
    joining: bool,
) -> anyhow::Result<()> {
    let driver = driver_for(provider);
    let status_ok = account.status == "connected"
        || (joining && driver != "devin" && account.status == "refreshing");
    anyhow::ensure!(
        account.provider_id == provider.id
            && account.is_enabled
            && status_ok
            && (account.driver_key.is_empty()
                || auth::normalize_driver_key(&account.driver_key) == driver),
        ModelProbeAccountError::Unavailable
    );
    let blocked = match driver {
        "kiro" => {
            let state = KiroAccountState::parse(&account.account_state);
            state.auth_invalid || state.hard_quota
        }
        "claude-code" => {
            let state = ClaudeAccountState::parse(&account.account_state);
            state.auth_invalid || state.hard_quota
        }
        _ => {
            let state = DevinAccountState::parse(&account.account_state);
            state.auth_invalid || state.hard_quota
        }
    };
    anyhow::ensure!(!blocked, ModelProbeAccountError::Unavailable);
    Ok(())
}

fn eligible(
    provider: &Provider,
    account: &OAuthCredential,
    config: &DevinPoolConfig,
    model: &str,
) -> bool {
    match driver_for(provider) {
        "kiro" => KiroAccountState::parse(&account.account_state)
            .cost_class(config)
            .is_some(),
        "claude-code" => ClaudeAccountState::parse(&account.account_state)
            .cost_class_for_model(config, Some(model))
            .is_some(),
        _ => DevinAccountState::parse(&account.account_state)
            .cost_class(config)
            .is_some(),
    }
}

fn result_error(model: &str, protocol: &str, message: &str) -> ProviderModelProbeResult {
    ProviderModelProbeResult {
        model: model.into(),
        success: false,
        error: Some(message.into()),
        latency_ms: 0,
        protocol: protocol.into(),
        reply: None,
        quota_remaining: None,
        quota_resets_at: None,
    }
}

impl AdminService {
    /// `Some(account_id)` binds the complete run to one account. Absent IDs retain
    /// the legacy API behavior; malformed or unavailable IDs never fall back.
    pub async fn probe_provider_models_with_account(
        &self,
        id: &str,
        models: Option<Vec<String>>,
        prompt: Option<String>,
        account_id: Option<String>,
    ) -> anyhow::Result<ProviderModelProbeOutcome> {
        let account_id = normalize_probe_account_id(account_id)?;
        let Some(account_id) = account_id else {
            return self
                .probe_provider_models_with_prompt(id, models, prompt)
                .await;
        };
        let selection = normalize_probe_selection(models)?;
        let prompt = normalize_probe_prompt(prompt)?;
        let provider = self
            .get_provider(id)
            .await
            .map_err(|_| ModelProbeAccountError::StateUnavailable)?;
        anyhow::ensure!(
            crate::provider::account_pool::supports_pool(&provider),
            ModelProbeAccountError::Unsupported
        );
        validate_admin_provider(&provider)?;
        // Load the exact pair before doing any upstream work or catalog lookup.
        let account = self
            .gw
            .storage
            .oauth_credentials()
            .get_account(id, &account_id)
            .await
            .map_err(|_| ModelProbeAccountError::StateUnavailable)?
            .ok_or(ModelProbeAccountError::NotFound)?;
        validate_account(&provider, &account, true)?;
        let models = match selection {
            Some(models) => models,
            None => self
                .get_provider_models(id)
                .await
                .map_err(|_| ModelProbeAccountError::StateUnavailable)?,
        };
        anyhow::ensure!(!models.is_empty(), "provider model list is empty");
        if driver_for(&provider) != "claude-code" {
            anyhow::ensure!(
                models.len() <= 16,
                "Native account model probes are limited to 16 models per request"
            );
        }
        // Use the same account-scoped refresh fence as inference. Never refresh
        // all pool members merely because one account was explicitly selected.
        let account = match driver_for(&provider) {
            "kiro" => self
                .refresh_kiro_pool_account(&provider, &account)
                .await
                .map_err(|_| ModelProbeAccountError::RefreshFailed)?,
            "claude-code" => self
                .refresh_claude_pool_account(&provider, &account)
                .await
                .map_err(|_| ModelProbeAccountError::RefreshFailed)?,
            _ => account,
        };
        validate_account(&provider, &account, false)?;
        let runtime = self
            .resolve_provider_account_runtime(&provider, &account)
            .await
            .map_err(|_| ModelProbeAccountError::Unavailable)?;
        anyhow::ensure!(
            !runtime.access_token.trim().is_empty(),
            ModelProbeAccountError::Unavailable
        );
        let base_url = runtime
            .binding
            .base_url_override
            .clone()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| provider.base_url.clone());
        let protocol = match driver_for(&provider) {
            "devin" => crate::protocol::ids::DEVIN_CHAT_V1,
            "kiro" => crate::protocol::ids::KIRO_CHAT_V1,
            _ => crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01,
        }
        .to_string();
        let config = self
            .provider_pool_config(id)
            .await
            .map_err(|_| ModelProbeAccountError::StateUnavailable)?
            .unwrap_or_default();
        let credential = runtime
            .credential
            .as_ref()
            .ok_or(ModelProbeAccountError::Unavailable)?;
        let http = if driver_for(&provider) == "claude-code" {
            Some((
                self.gw
                    .http_client_for_provider(provider.use_proxy)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Unable to prepare model probe transport; check proxy/network settings"
                        )
                    })?,
                runtime_binding_headers(&runtime.binding)
                    .map_err(|_| ModelProbeAccountError::Unavailable)?,
            ))
        } else {
            None
        };
        let mut results = Vec::with_capacity(models.len());
        // Sequential, bounded native probes and a single immutable credential
        // generation. Changes invalidate later models instead of switching tokens.
        for model in models {
            let started = Instant::now();
            let preflight = self
                .check_probe_account(&provider, &account, &config, &model)
                .await;
            let mut result = if let Err(error) = preflight {
                result_error(&model, &protocol, &error.to_string())
            } else if let Some((client, headers)) = &http {
                let mut result = probe_single_model(
                    client.clone(),
                    crate::protocol::ids::Protocol::AnthropicMessages,
                    &base_url,
                    &runtime.access_token,
                    "none",
                    headers,
                    &model,
                    prompt.as_deref().unwrap_or(DEFAULT_MODEL_PROBE_PROMPT),
                    &protocol,
                    provider.fast_mode,
                    provider.channel.as_deref(),
                    false,
                    None,
                    Some(&ClaudeCodeProbe {
                        provider: provider.clone(),
                        credential: Some(credential.clone()),
                    }),
                )
                .await;
                // The legacy HTTP probe can return upstream body previews. The
                // account surface must never expose credential echoes or URLs.
                if let Some(error) = &result.error {
                    result.error = Some(safe_account_probe_error(error));
                }
                result
            } else {
                let outcome = if driver_for(&provider) == "kiro" {
                    crate::provider::kiro::probe(
                        &self.gw,
                        &provider,
                        credential,
                        &model,
                        prompt.as_deref(),
                    )
                    .await
                } else {
                    crate::provider::devin::probe(
                        &self.gw,
                        &provider,
                        credential,
                        &model,
                        prompt.as_deref(),
                    )
                    .await
                };
                match outcome {
                    Ok(reply) => ProviderModelProbeResult {
                        model: model.clone(),
                        success: true,
                        error: None,
                        latency_ms: 0,
                        protocol: protocol.clone(),
                        reply: Some(reply),
                        quota_remaining: None,
                        quota_resets_at: None,
                    },
                    Err(error) => result_error(
                        &model,
                        &protocol,
                        &safe_account_probe_error(&error.to_string()),
                    ),
                }
            };
            // A disable, removal, reauthorization or new quota block while the
            // upstream ran invalidates its result. In-flight bytes cannot be
            // recalled; never present the old generation as a current success.
            if let Err(error) = self
                .check_probe_account(&provider, &account, &config, &model)
                .await
            {
                result.success = false;
                result.reply = None;
                result.error = Some(error.to_string());
            }
            result.latency_ms = started.elapsed().as_millis() as u64;
            results.push(result);
        }
        results.sort_by(|a, b| a.model.cmp(&b.model));
        Ok(ProviderModelProbeOutcome {
            meta: ProviderModelProbeMeta {
                protocol,
                base_url,
                account_id: Some(account.account_id),
                account_label: Some(account.label),
            },
            results,
        })
    }

    async fn check_probe_account(
        &self,
        provider: &Provider,
        tested: &OAuthCredential,
        config: &DevinPoolConfig,
        model: &str,
    ) -> anyhow::Result<()> {
        let current_config = self
            .provider_pool_config(&provider.id)
            .await
            .map_err(|_| ModelProbeAccountError::StateUnavailable)?
            .unwrap_or_default();
        anyhow::ensure!(&current_config == config, ModelProbeAccountError::Changed);
        let current = self
            .gw
            .storage
            .oauth_credentials()
            .get_account(&provider.id, &tested.account_id)
            .await
            .map_err(|_| ModelProbeAccountError::StateUnavailable)?
            .ok_or(ModelProbeAccountError::Changed)?;
        anyhow::ensure!(
            current.status_version == tested.status_version,
            ModelProbeAccountError::Changed
        );
        validate_account(provider, &current, false)?;
        anyhow::ensure!(
            eligible(provider, &current, config, model),
            "Selected account is blocked by authentication, quota or pool policy; refresh its usage or reauthorize it"
        );
        Ok(())
    }
}

fn safe_account_probe_error(message: &str) -> String {
    // Only locally generated classifications survive, never arbitrary chains.
    if message.starts_with("HTTP 401") || message.starts_with("HTTP 403") {
        "Selected account authentication or model access was rejected; reauthorize it or verify model access".into()
    } else if message.starts_with("HTTP 429") {
        "Selected account is rate limited or lacks model quota; check its plan and usage".into()
    } else if message.starts_with("timeout after ") {
        "Selected account model probe timed out".into()
    } else {
        "Selected account model probe failed; check model access, account status and network settings".into()
    }
}

#[cfg(test)]
mod tests;
