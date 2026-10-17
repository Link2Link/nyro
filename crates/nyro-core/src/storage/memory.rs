use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::db::models::{
    ApiKeyStats, ApiKeyUsageDetail, CreateModel, CreateProvider, LogPage, LogQuery, Model,
    ModelStats, ModelTimeBucket, ModelUsageDetail, ModelUsageStats, OAuthCredential, Provider,
    ProviderKeyProbeResult, ProviderStats, ProviderUsageDetail, RequestLog, StatsHourly,
    StatsOverview, StatsTimeBucket, UpdateModel, UpdateProvider, UpsertOAuthCredential,
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
        Self {
            providers: Arc::new(RwLock::new(providers)),
            models: Arc::new(RwLock::new(models)),
            settings: Arc::new(RwLock::new(settings)),
            oauth_credentials: Arc::new(MemoryOAuthCredentialStore {
                credentials: RwLock::new(std::collections::HashMap::new()),
            }),
        }
    }
}

pub struct MemoryOAuthCredentialStore {
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

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn write_memory_credential(
    map: &mut std::collections::HashMap<String, OAuthCredential>,
    provider_id: &str,
    input: UpsertOAuthCredential,
) -> anyhow::Result<OAuthCredential> {
    let now = now_rfc3339();
    let version = map
        .get(provider_id)
        .map(|c| c.status_version + 1)
        .unwrap_or(0);
    let cred = OAuthCredential {
        provider_id: provider_id.to_string(),
        driver_key: input.driver_key,
        scheme: input.scheme,
        access_token: input.access_token,
        refresh_token: input.refresh_token,
        expires_at: input.expires_at,
        resource_url: input.resource_url,
        subject_id: input.subject_id,
        scopes: input.scopes.unwrap_or_else(|| "[]".to_string()),
        meta: input.meta.unwrap_or_else(|| "{}".to_string()),
        status: "connected".to_string(),
        status_version: version,
        last_error: None,
        last_refresh_at: map.get(provider_id).and_then(|c| c.last_refresh_at.clone()),
        created_at: map
            .get(provider_id)
            .map(|c| c.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
    };
    map.insert(provider_id.to_string(), cred.clone());
    Ok(cred)
}

fn scrub_memory_credential(credential: &mut OAuthCredential) {
    // Preserve only the generation fence, not the logged-out account.
    credential.driver_key.clear();
    credential.scheme.clear();
    credential.access_token.clear();
    credential.refresh_token = None;
    credential.expires_at = None;
    credential.resource_url = None;
    credential.subject_id = None;
    credential.scopes = "[]".to_string();
    credential.meta = "{}".to_string();
    credential.status = "disconnected".to_string();
    credential.status_version += 1;
    credential.last_error = None;
    credential.last_refresh_at = None;
    credential.updated_at = now_rfc3339();
}

#[async_trait]
impl OAuthCredentialStore for MemoryOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(self
            .credentials
            .read()
            .await
            .get(provider_id)
            .filter(|credential| credential.status != "disconnected")
            .cloned())
    }

    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        let mut map = self.credentials.write().await;
        write_memory_credential(&mut map, provider_id, input)
    }

    async fn restore_if_version(
        &self,
        provider_id: &str,
        expected_version: i32,
        previous: Option<UpsertOAuthCredential>,
    ) -> anyhow::Result<bool> {
        let mut map = self.credentials.write().await;
        if map.get(provider_id).is_none_or(|credential| {
            credential.status != "connected" || credential.status_version != expected_version
        }) {
            return Ok(false);
        }
        if let Some(previous) = previous {
            write_memory_credential(&mut map, provider_id, previous)?;
        } else {
            scrub_memory_credential(map.get_mut(provider_id).expect("checked under write lock"));
        }
        Ok(true)
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        let mut map = self.credentials.write().await;
        if let Some(credential) = map.get_mut(provider_id) {
            scrub_memory_credential(credential);
        }
        Ok(())
    }

    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let mut map = self.credentials.write().await;
        let Some(cred) = map.get_mut(provider_id) else {
            return Ok(None);
        };
        if cred.status != "connected" || cred.status_version != expected_version {
            return Ok(None);
        }
        cred.status = "refreshing".to_string();
        cred.status_version += 1;
        cred.updated_at = now_rfc3339();
        Ok(Some(cred.clone()))
    }

    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let mut map = self.credentials.write().await;
        let Some(cred) = map.get_mut(provider_id) else {
            return Ok(None);
        };
        if cred.status != "refreshing" || cred.status_version != expected_version {
            return Ok(None);
        }
        let now = now_rfc3339();
        cred.driver_key = input.driver_key;
        cred.scheme = input.scheme;
        cred.access_token = input.access_token;
        cred.refresh_token = input.refresh_token;
        cred.expires_at = input.expires_at;
        cred.resource_url = input.resource_url;
        cred.subject_id = input.subject_id;
        if let Some(scopes) = input.scopes {
            cred.scopes = scopes;
        }
        if let Some(meta) = input.meta {
            cred.meta = meta;
        }
        cred.status = "connected".to_string();
        cred.status_version += 1;
        cred.last_error = None;
        cred.last_refresh_at = Some(now.clone());
        cred.updated_at = now;
        Ok(Some(cred.clone()))
    }

    async fn fail_refresh_with_policy(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
        terminal: bool,
        meta: Option<String>,
    ) -> anyhow::Result<bool> {
        let mut map = self.credentials.write().await;
        let Some(cred) = map.get_mut(provider_id) else {
            return Ok(false);
        };
        if cred.status != "refreshing" || cred.status_version != expected_version {
            return Ok(false);
        }
        cred.status = if terminal { "error" } else { "connected" }.to_string();
        if let Some(meta) = meta {
            cred.meta = meta;
        }
        cred.last_error = Some(error_message.to_string());
        cred.status_version += 1;
        cred.updated_at = now_rfc3339();
        Ok(true)
    }

    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let cutoff = i128::from(chrono::Utc::now().timestamp()) + i128::from(before.as_secs());
        let map = self.credentials.read().await;
        Ok(map
            .values()
            .filter(|credential| {
                credential.status == "connected"
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
            rows.get_mut("old-a").unwrap().updated_at = "2000-01-01T01:00:00+01:00".to_string();
            rows.get_mut("old-codex").unwrap().updated_at = "2000-01-01 00:00:00".to_string();
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
        let tombstone = rows.get("old-a").unwrap();
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
