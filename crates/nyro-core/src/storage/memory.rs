use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::db::models::{
    ApiKeyStats, ApiKeyUsageDetail, CreateModel, CreateProvider, LogPage, LogQuery, Model,
    ModelStats, ModelTimeBucket, ModelUsageDetail, ModelUsageStats, OAuthAccountUpdate,
    OAuthCredential, Provider, ProviderKeyProbeResult, ProviderStats, ProviderUsageDetail,
    RequestLog, StatsHourly, StatsOverview, StatsTimeBucket, UpdateModel, UpdateProvider,
    UpsertOAuthCredential,
};
use crate::logging::LogEntry;

use super::traits::{
    ApiKeyStore, AuthAccessStore, LogStore, ModelBackendStore, ModelRatingStore,
    ModelSnapshotStore, ModelStore, OAuthCredentialStore, ProviderEndpointTestResult,
    ProviderStore, ProviderTestResult, SettingsStore, Storage, StorageBackend, StorageBootstrap,
    StorageHealth,
};

use std::sync::Arc;

#[derive(Clone)]
pub struct MemoryStorage {
    providers: Arc<RwLock<Vec<Provider>>>,
    models: Arc<RwLock<Vec<Model>>>,
    settings: Arc<RwLock<Vec<(String, String)>>>,
    oauth_credentials: Arc<MemoryOAuthCredentialStore>,
}

impl MemoryStorage {
    pub fn new(
        providers: Vec<Provider>,
        models: Vec<Model>,
        settings: Vec<(String, String)>,
    ) -> Self {
        let providers = Arc::new(RwLock::new(providers));
        Self {
            providers: providers.clone(),
            models: Arc::new(RwLock::new(models)),
            settings: Arc::new(RwLock::new(settings)),
            oauth_credentials: Arc::new(MemoryOAuthCredentialStore {
                providers,
                credentials: RwLock::new(std::collections::HashMap::new()),
            }),
        }
    }
}

pub struct MemoryOAuthCredentialStore {
    providers: Arc<RwLock<Vec<Provider>>>,
    credentials: RwLock<std::collections::HashMap<String, OAuthCredential>>,
}

impl Storage for MemoryStorage {
    fn providers(&self) -> &dyn ProviderStore {
        self
    }
    fn model_ratings(&self) -> Option<&dyn ModelRatingStore> {
        // YAML configuration is read-only and has no persistent rating store.
        None
    }
    fn models(&self) -> &dyn ModelStore {
        self
    }
    fn snapshots(&self) -> &dyn ModelSnapshotStore {
        self
    }
    fn model_backends(&self) -> Option<&dyn ModelBackendStore> {
        None
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        None
    }
    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        None
    }
    fn logs(&self) -> &dyn LogStore {
        self
    }
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.oauth_credentials.as_ref()
    }
    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self
    }
}

#[async_trait]
impl ProviderStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        Ok(self.providers.read().await.clone())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        Ok(self
            .providers
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned())
    }

    async fn create(&self, _input: CreateProvider) -> anyhow::Result<Provider> {
        anyhow::bail!("create not supported in standalone (YAML) mode")
    }

    async fn update(&self, _id: &str, _input: UpdateProvider) -> anyhow::Result<Provider> {
        anyhow::bail!("update not supported in standalone (YAML) mode")
    }

    async fn delete(&self, _id: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete not supported in standalone (YAML) mode")
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let providers = self.providers.read().await;
        Ok(providers
            .iter()
            .any(|p| p.name == name && exclude_id.is_none_or(|eid| p.id != eid)))
    }

    async fn record_test_result(
        &self,
        _provider_id: &str,
        _result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn record_endpoint_test_result(
        &self,
        endpoint_id: &str,
        result: ProviderEndpointTestResult,
    ) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        for endpoint in providers
            .iter_mut()
            .flat_map(|provider| provider.protocol_endpoints.iter_mut())
        {
            if endpoint.id == endpoint_id {
                endpoint.test_status = if result.success { "success" } else { "failed" }.into();
                endpoint.test_error = result.error;
                endpoint.tested_at = Some(result.tested_at);
                break;
            }
        }
        Ok(())
    }

    async fn record_key_probe_result(
        &self,
        key_id: &str,
        result: ProviderKeyProbeResult,
    ) -> anyhow::Result<()> {
        let mut providers = self.providers.write().await;
        for key in providers
            .iter_mut()
            .flat_map(|provider| provider.keys.iter_mut())
        {
            if key.id == key_id {
                if let Some(models) = result.models {
                    key.models_snapshot = Some(crate::db::models::encode_model_list(&models));
                }
                key.probe_error = result.error;
                key.last_probe_at = Some(result.tested_at);
                break;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ModelStore for MemoryStorage {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(self.models.read().await.clone())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        Ok(self
            .models
            .read()
            .await
            .iter()
            .find(|m| m.id == id)
            .cloned())
    }

    async fn create(&self, _input: CreateModel) -> anyhow::Result<Model> {
        anyhow::bail!("create not supported in standalone (YAML) mode")
    }

    async fn update(&self, _id: &str, _input: UpdateModel) -> anyhow::Result<Model> {
        anyhow::bail!("update not supported in standalone (YAML) mode")
    }

    async fn delete(&self, _id: &str) -> anyhow::Result<()> {
        anyhow::bail!("delete not supported in standalone (YAML) mode")
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let models = self.models.read().await;
        Ok(models
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(name) && exclude_id.is_none_or(|eid| m.id != eid)))
    }
}

#[async_trait]
impl ModelSnapshotStore for MemoryStorage {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        let models = self.models.read().await;
        Ok(models.iter().filter(|m| m.is_enabled).cloned().collect())
    }
}

#[async_trait]
impl SettingsStore for MemoryStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let settings = self.settings.read().await;
        Ok(settings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone()))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut settings = self.settings.write().await;
        if let Some(entry) = settings.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value.to_string();
        } else {
            settings.push((key.to_string(), value.to_string()));
        }
        Ok(())
    }

    async fn list_all(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(self.settings.read().await.clone())
    }
}

#[async_trait]
impl LogStore for MemoryStorage {
    async fn append_batch(&self, _entries: Vec<LogEntry>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn query(&self, _query: LogQuery) -> anyhow::Result<LogPage> {
        Ok(LogPage {
            items: vec![],
            total: 0,
        })
    }

    async fn find_by_id(&self, _id: &str) -> anyhow::Result<Option<RequestLog>> {
        Ok(None)
    }

    async fn request_result(
        &self,
        _client_request_id: &str,
    ) -> anyhow::Result<Option<crate::db::models::RequestResult>> {
        Ok(None)
    }

    async fn cleanup_before(&self, _cutoff: &str) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn clear_all(&self) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn clear_payloads(&self) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn delete_by_id(&self, _id: &str) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn clear_errors(&self) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn stats_overview_range(
        &self,
        _start_at: Option<i64>,
        _end_at: Option<i64>,
    ) -> anyhow::Result<StatsOverview> {
        Ok(StatsOverview::default())
    }

    async fn stats_hourly(&self, _hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
        Ok(vec![])
    }

    async fn stats_time_buckets(
        &self,
        _start_ms: i64,
        _end_ms: i64,
        _bucket_ms: i64,
        _upstream_model: Option<&str>,
    ) -> anyhow::Result<Vec<StatsTimeBucket>> {
        Ok(vec![])
    }

    async fn api_key_model_time_buckets(
        &self,
        _api_key_id: &str,
        _start_ms: i64,
        _end_ms: i64,
        _bucket_ms: i64,
    ) -> anyhow::Result<Vec<ModelTimeBucket>> {
        Ok(vec![])
    }

    async fn token_usage_days(
        &self,
        _start_at: i64,
        _end_at: i64,
    ) -> anyhow::Result<Vec<crate::db::models::TokenUsageDay>> {
        // YAML-only memory storage does not retain request logs.
        Ok(vec![])
    }

    async fn account_token_usage(
        &self,
        _provider_id: &str,
        _start_at: Option<i64>,
        _end_at: Option<i64>,
    ) -> anyhow::Result<Vec<crate::db::models::AccountModelUsage>> {
        // YAML-only memory storage does not retain request logs.
        Ok(vec![])
    }

    async fn stats_by_model_range(
        &self,
        _start_at: Option<i64>,
        _end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ModelStats>> {
        Ok(vec![])
    }

    async fn model_usage_stats(
        &self,
        _provider_id: &str,
        _upstream_model: &str,
    ) -> anyhow::Result<ModelUsageStats> {
        Ok(ModelUsageStats::default())
    }

    async fn stats_by_provider_range(
        &self,
        _start_at: Option<i64>,
        _end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        Ok(vec![])
    }

    async fn provider_usage_detail(
        &self,
        provider_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ProviderUsageDetail> {
        Ok(ProviderUsageDetail {
            start_at,
            end_at,
            provider_id: provider_id.to_string(),
            provider_name: provider_id.to_string(),
            provider_icon: None,
            provider_protocol: None,
            request_count: 0,
            success_count: 0,
            error_count: 0,
            unknown_count: 0,
            cancelled_count: 0,
            output_limited_count: 0,
            outcome_stats_version: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            avg_duration_ms: 0.0,
            avg_first_token_ms: None,
            total_upstream_ms: 0.0,
            last_used_at: None,
            tps_totals: Default::default(),
            models: Vec::new(),
            equivalent_costs: Vec::new(),
            unpriced_request_count: 0,
        })
    }

    async fn stats_by_api_key_range(
        &self,
        _start_at: Option<i64>,
        _end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        Ok(vec![])
    }

    async fn api_key_usage_detail(
        &self,
        api_key_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ApiKeyUsageDetail> {
        Ok(ApiKeyUsageDetail {
            start_at,
            end_at,
            api_key_id: api_key_id.to_string(),
            api_key_name: String::new(),
            request_count: 0,
            success_count: 0,
            error_count: 0,
            unknown_count: 0,
            cancelled_count: 0,
            output_limited_count: 0,
            outcome_stats_version: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            avg_duration_ms: 0.0,
            avg_first_token_ms: None,
            last_used_at: None,
            model_routes: Vec::new(),
            model_time_series: Vec::new(),
        })
    }

    async fn model_usage_detail(
        &self,
        upstream_model: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ModelUsageDetail> {
        Ok(ModelUsageDetail {
            start_at,
            end_at,
            upstream_model: upstream_model.to_string(),
            request_count: 0,
            success_count: 0,
            error_count: 0,
            unknown_count: 0,
            cancelled_count: 0,
            output_limited_count: 0,
            outcome_stats_version: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            avg_duration_ms: 0.0,
            avg_first_token_ms: None,
            total_upstream_ms: 0.0,
            last_used_at: None,
            providers: Vec::new(),
            api_keys: Vec::new(),
            time_series: None,
            tps_totals: Default::default(),
        })
    }
}

#[async_trait]
impl StorageBootstrap for MemoryStorage {
    async fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn health(&self) -> anyhow::Result<StorageHealth> {
        Ok(StorageHealth {
            backend: StorageBackend::Sqlite,
            can_connect: true,
            schema_compatible: true,
            writable: false,
        })
    }
}

impl MemoryOAuthCredentialStore {
    async fn mutate(
        &self,
        provider_id: &str,
        action: crate::storage::oauth::Mutation<'_>,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let providers = self.providers.read().await;
        let parent = providers.iter().find(|p| p.id == provider_id);
        let mut map = self.credentials.write().await;
        let rows: Vec<_> = map
            .values()
            .filter(|r| r.provider_id == provider_id)
            .cloned()
            .collect();
        let row = crate::storage::oauth::mutate(
            provider_id,
            parent.map(|p| {
                (
                    p.auth_mode.as_str(),
                    p.vendor.as_deref().unwrap_or(""),
                    p.preset_key.as_deref().unwrap_or(""),
                    p.channel.as_deref().unwrap_or(""),
                )
            }),
            &rows,
            &action,
        )?;
        if let Some(row) = &row {
            map.insert(row.account_id.clone(), row.clone());
        }
        Ok(row)
    }
}

#[async_trait]
impl OAuthCredentialStore for MemoryOAuthCredentialStore {
    async fn list_accounts(&self, provider_id: &str) -> anyhow::Result<Vec<OAuthCredential>> {
        let mut rows: Vec<_> = self
            .credentials
            .read()
            .await
            .values()
            .filter(|r| r.provider_id == provider_id && r.status != "disconnected")
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            (&a.priority, &a.created_at, &a.account_id).cmp(&(
                &b.priority,
                &b.created_at,
                &b.account_id,
            ))
        });
        Ok(rows)
    }
    async fn get_account(
        &self,
        provider_id: &str,
        account_id: &str,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(self
            .credentials
            .read()
            .await
            .get(account_id)
            .filter(|r| r.provider_id == provider_id && r.status != "disconnected")
            .cloned())
    }

    async fn create_account(
        &self,
        provider_id: &str,
        label: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::Create(label, &input),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("Account was not created"))
    }
    async fn replace_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::Replace(account_id, expected_version, &input),
        )
        .await
    }
    async fn update_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        update: OAuthAccountUpdate,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::Update(account_id, expected_version, &update),
        )
        .await
    }
    async fn disconnect_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<bool> {
        Ok(self
            .mutate(
                provider_id,
                crate::storage::oauth::Mutation::Disconnect(account_id, expected_version),
            )
            .await?
            .is_some())
    }
    async fn compare_account_state(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        expected_state: &str,
        new_state: &str,
    ) -> anyhow::Result<bool> {
        Ok(self
            .mutate(
                provider_id,
                crate::storage::oauth::Mutation::CompareState(
                    account_id,
                    expected_version,
                    expected_state,
                    new_state,
                ),
            )
            .await?
            .is_some())
    }
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        crate::storage::oauth::singleton(&self.list_accounts(provider_id).await?)
    }
    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        self.mutate(provider_id, crate::storage::oauth::Mutation::Upsert(&input))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Credential was not bound"))
    }
    async fn restore_if_version(
        &self,
        provider_id: &str,
        expected_version: i32,
        previous: Option<UpsertOAuthCredential>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .mutate(
                provider_id,
                crate::storage::oauth::Mutation::Restore(expected_version, previous.as_ref()),
            )
            .await?
            .is_some())
    }
    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        self.mutate(provider_id, crate::storage::oauth::Mutation::Delete)
            .await?;
        Ok(())
    }
    async fn try_begin_account_refresh(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::BeginAccountRefresh(account_id, expected_version),
        )
        .await
    }
    async fn complete_account_refresh(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::CompleteAccountRefresh(
                account_id,
                expected_version,
                &input,
            ),
        )
        .await
    }
    async fn fail_account_refresh_with_policy(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        error_message: &str,
        terminal: bool,
        meta: Option<String>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .mutate(
                provider_id,
                crate::storage::oauth::Mutation::FailAccountRefresh(
                    account_id,
                    expected_version,
                    error_message,
                    terminal,
                    meta.as_deref(),
                ),
            )
            .await?
            .is_some())
    }
    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::BeginRefresh(expected_version),
        )
        .await
    }
    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        self.mutate(
            provider_id,
            crate::storage::oauth::Mutation::CompleteRefresh(expected_version, &input),
        )
        .await
    }
    async fn fail_refresh_with_policy(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
        terminal: bool,
        meta: Option<String>,
    ) -> anyhow::Result<bool> {
        Ok(self
            .mutate(
                provider_id,
                crate::storage::oauth::Mutation::FailRefresh(
                    expected_version,
                    error_message,
                    terminal,
                    meta.as_deref(),
                ),
            )
            .await?
            .is_some())
    }
    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let cutoff = i128::from(chrono::Utc::now().timestamp()) + i128::from(before.as_secs());
        let map = self.credentials.read().await;
        Ok(map
            .values()
            .filter(|credential| {
                credential.status == "connected"
                    && credential.driver_key != "devin"
                    && credential.is_enabled
                    && credential
                        .expires_at
                        .as_deref()
                        .and_then(super::oauth::parse_timestamp)
                        .is_some_and(|expiry| i128::from(expiry.timestamp()) <= cutoff)
            })
            .cloned()
            .collect())
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let mut map = self.credentials.write().await;
        let now = chrono::Utc::now();
        let cutoff = i128::from(now.timestamp()) - i128::from(timeout.as_secs());
        let mut count = 0u64;
        for cred in map.values_mut() {
            if cred.status == "refreshing"
                && super::oauth::parse_timestamp(&cred.updated_at)
                    .is_some_and(|updated| i128::from(updated.timestamp()) < cutoff)
            {
                cred.status = "connected".to_string();
                cred.last_error = Some(super::oauth::STALE_REFRESH_ERROR.to_string());
                cred.status_version += 1;
                cred.updated_at = now.format("%Y-%m-%d %H:%M:%S").to_string();
                count += 1;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod oauth_time_tests {
    use super::*;

    #[tokio::test]
    async fn stale_recovery_respects_age() -> anyhow::Result<()> {
        let store = MemoryOAuthCredentialStore {
            providers: Arc::new(RwLock::new(Vec::new())),
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        for (provider, driver) in [
            ("old-a", "codex"),
            ("old-codex", "codex"),
            ("fresh", "codex"),
        ] {
            let initial = store
                .upsert(
                    provider,
                    UpsertOAuthCredential {
                        driver_key: driver.to_string(),
                        access_token: "secret".to_string(),
                        subject_id: Some(provider.to_string()),
                        ..Default::default()
                    },
                )
                .await?;
            store
                .try_begin_refresh(provider, initial.status_version)
                .await?
                .unwrap();
        }
        {
            let mut rows = store.credentials.write().await;
            rows.get_mut("legacy:old-a").unwrap().updated_at =
                "2000-01-01T01:00:00+01:00".to_string();
            rows.get_mut("legacy:old-codex").unwrap().updated_at =
                "2000-01-01 00:00:00".to_string();
        }
        assert_eq!(
            store
                .recover_stale_refreshing(Duration::from_secs(60))
                .await?,
            2
        );
        assert_eq!(
            store
                .recover_stale_refreshing(Duration::from_secs(60))
                .await?,
            0
        );
        let recovered = store.get("old-a").await?.unwrap();
        assert_eq!(recovered.status, "connected");
        assert_eq!(
            recovered.last_error.as_deref(),
            Some(super::super::oauth::STALE_REFRESH_ERROR)
        );
        assert_eq!(recovered.status_version, 2);
        assert_eq!(store.get("old-codex").await?.unwrap().status, "connected");
        assert_eq!(store.get("fresh").await?.unwrap().status, "refreshing");
        assert!(!store.fail_refresh("old-a", 1, "late").await?);
        assert!(
            store
                .complete_refresh("old-a", 1, UpsertOAuthCredential::default())
                .await?
                .is_none()
        );
        store.delete("old-a").await?;
        assert!(store.get("old-a").await?.is_none());
        let rows = store.credentials.read().await;
        let tombstone = rows.get("legacy:old-a").unwrap();
        assert_eq!(tombstone.status, "disconnected");
        assert_eq!(tombstone.access_token, "");
        assert_eq!(tombstone.meta, "{}");
        assert_eq!(tombstone.subject_id, None);
        assert_eq!(tombstone.refresh_token, None);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_or_absent_expiry_is_not_scheduled() -> anyhow::Result<()> {
        let store = MemoryOAuthCredentialStore {
            providers: Arc::new(RwLock::new(Vec::new())),
            credentials: RwLock::new(std::collections::HashMap::new()),
        };
        for expiry in [None, Some("not-a-time".to_string())] {
            store
                .upsert(
                    "provider",
                    UpsertOAuthCredential {
                        expires_at: expiry,
                        ..Default::default()
                    },
                )
                .await?;
            assert!(
                store
                    .list_expiring(Duration::from_secs(60))
                    .await?
                    .is_empty()
            );
        }
        Ok(())
    }
}
