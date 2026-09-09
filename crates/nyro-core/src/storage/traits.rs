use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::db::models::{
    ApiKeyStats, ApiKeyUsageDetail, ApiKeyWithBindings, CreateApiKey, CreateModel,
    CreateModelBackend, CreateProvider, LogPage, LogQuery, Model, ModelBackend, ModelRatingEntry,
    ModelStats, ModelTimeBucket, ModelUsageDetail, ModelUsageStats, OAuthCredential, Provider,
    ProviderStats, ProviderUsageDetail, RequestLog, RequestResult, StatsHourly, StatsOverview,
    StatsTimeBucket, UpdateApiKey, UpdateModel, UpdateProvider, UpsertOAuthCredential,
};
use crate::logging::LogEntry;

#[derive(Debug, Clone)]
pub struct ProviderTestResult {
    pub success: bool,
    pub tested_at: String,
}

#[derive(Debug, Clone)]
pub struct ProviderEndpointTestResult {
    pub success: bool,
    pub error: Option<String>,
    pub tested_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    Minute,
    Day,
}

#[derive(Debug, Clone)]
pub struct ApiKeyAccessRecord {
    pub id: String,
    pub name: String,
    pub is_enabled: bool,
    /// When true the per-model binding check is skipped (enable, expiry and
    /// quota gates still apply).
    pub is_privileged: bool,
    pub expires_at: Option<String>,
    pub rpm: Option<i32>,
    pub rpd: Option<i32>,
    pub tpm: Option<i32>,
    pub tpd: Option<i32>,
}

#[derive(Debug, Clone)]
pub enum StorageBackend {
    Sqlite,
    Postgres,
    Mysql,
}

#[derive(Debug, Clone)]
pub struct StorageHealth {
    pub backend: StorageBackend,
    pub can_connect: bool,
    pub schema_compatible: bool,
    pub writable: bool,
}

#[async_trait]
pub trait ProviderStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<Provider>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>>;
    async fn create(&self, input: CreateProvider) -> anyhow::Result<Provider>;
    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()>;
    async fn record_endpoint_test_result(
        &self,
        endpoint_id: &str,
        result: ProviderEndpointTestResult,
    ) -> anyhow::Result<()>;
}

/// Persistence for prefix-keyed ratings, independent of routing, providers,
/// and model catalogs.
#[async_trait]
pub trait ModelRatingStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<ModelRatingEntry>>;
    async fn upsert(&self, entry: ModelRatingEntry) -> anyhow::Result<ModelRatingEntry>;
    async fn delete(&self, model_prefix: &str) -> anyhow::Result<()>;
    /// Atomically replace all entries; row timestamps are preserved.
    async fn restore(&self, entries: &[ModelRatingEntry]) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ModelStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<Model>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>>;
    async fn create(&self, input: CreateModel) -> anyhow::Result<Model>;
    async fn update(&self, id: &str, input: UpdateModel) -> anyhow::Result<Model>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait ModelSnapshotStore: Send + Sync {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>>;
}

#[async_trait]
pub trait ModelBackendStore: Send + Sync {
    async fn list_backends_by_model(&self, model_id: &str) -> anyhow::Result<Vec<ModelBackend>>;
    async fn set_backends(
        &self,
        model_id: &str,
        backends: &[CreateModelBackend],
    ) -> anyhow::Result<Vec<ModelBackend>>;
    async fn delete_backends_by_model(&self, model_id: &str) -> anyhow::Result<()>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
    async fn list_all(&self) -> anyhow::Result<Vec<(String, String)>>;
}

#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>>;
    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings>;
    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait AuthAccessStore: Send + Sync {
    async fn find_api_key(&self, raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>>;
    async fn model_binding_exists(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool>;
    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>>;
    async fn request_count_since(
        &self,
        api_key_id: &str,
        window: UsageWindow,
    ) -> anyhow::Result<i64>;
    async fn token_count_since(&self, api_key_id: &str, window: UsageWindow)
    -> anyhow::Result<i64>;
}

#[async_trait]
pub trait LogStore: Send + Sync {
    /// Completion-aware, seven-day mixed performance samples per provider/model pair.
    async fn model_performance_stats(
        &self,
        _pairs: &[(String, String)],
        _as_of: i64,
    ) -> anyhow::Result<Vec<crate::db::PairPerformanceStats>> {
        anyhow::bail!("model performance statistics are unsupported by this storage")
    }
    /// Distinct (provider_id, upstream_model) pairs across all retained logs;
    /// the candidate set for prefix-matched performance statistics. Same
    /// retention scope as model_performance_stats.
    async fn distinct_logged_pairs(&self) -> anyhow::Result<Vec<(String, String)>> {
        anyhow::bail!("distinct logged pairs are unsupported by this storage")
    }
    /// Persist all attempts and attached final results atomically. SQL errors,
    /// including duplicate stable log IDs, reject the entire batch without retry.
    async fn append_batch(&self, entries: Vec<LogEntry>) -> anyhow::Result<()>;
    async fn query(&self, query: LogQuery) -> anyhow::Result<LogPage>;
    async fn find_by_id(&self, id: &str) -> anyhow::Result<Option<RequestLog>>;
    /// Final client result; never counted as an extra attempt in usage statistics.
    async fn request_result(
        &self,
        _client_request_id: &str,
    ) -> anyhow::Result<Option<RequestResult>> {
        Ok(None)
    }
    async fn cleanup_before(&self, cutoff_expression: &str) -> anyhow::Result<u64>;
    async fn clear_all(&self) -> anyhow::Result<u64>;
    /// Clear all recorded request/response headers and bodies, including errors.
    /// Retain safe metadata and record the payload-clearing timestamp.
    /// Returns the number of rows whose recorded payload fields were cleared.
    async fn clear_payloads(&self) -> anyhow::Result<u64>;
    /// Delete one log row by id; 0 when the id does not exist.
    async fn delete_by_id(&self, id: &str) -> anyhow::Result<u64>;
    /// Delete attempts matching the shared authoritative error predicate.
    async fn clear_errors(&self) -> anyhow::Result<u64>;
    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview>;
    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>>;
    async fn stats_time_buckets(
        &self,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
        upstream_model: Option<&str>,
    ) -> anyhow::Result<Vec<StatsTimeBucket>>;
    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>>;
    async fn model_usage_stats(
        &self,
        provider_id: &str,
        upstream_model: &str,
    ) -> anyhow::Result<ModelUsageStats>;
    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>>;
    async fn provider_usage_detail(
        &self,
        provider_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ProviderUsageDetail>;
    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>>;
    async fn api_key_usage_detail(
        &self,
        api_key_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ApiKeyUsageDetail>;
    /// Time buckets grouped by upstream model for one API key; the admin
    /// service splits the rows into per-model series.
    async fn api_key_model_time_buckets(
        &self,
        api_key_id: &str,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
    ) -> anyhow::Result<Vec<ModelTimeBucket>>;
    async fn model_usage_detail(
        &self,
        upstream_model: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ModelUsageDetail>;
}

#[async_trait]
pub trait OAuthCredentialStore: Send + Sync {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>>;
    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential>;
    async fn delete(&self, provider_id: &str) -> anyhow::Result<()>;
    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool>;
    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>>;
    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64>;
}

#[async_trait]
pub trait StorageBootstrap: Send + Sync {
    async fn init(&self) -> anyhow::Result<()>;
    async fn migrate(&self) -> anyhow::Result<()>;
    async fn health(&self) -> anyhow::Result<StorageHealth>;
}

pub trait Storage: Send + Sync {
    fn providers(&self) -> &dyn ProviderStore;
    /// SQL-only capability; YAML-backed memory storage and custom stores may not support ratings.
    fn model_ratings(&self) -> Option<&dyn ModelRatingStore> {
        None
    }
    fn models(&self) -> &dyn ModelStore;
    fn snapshots(&self) -> &dyn ModelSnapshotStore;
    fn model_backends(&self) -> Option<&dyn ModelBackendStore> {
        None
    }
    fn settings(&self) -> &dyn SettingsStore;
    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        None
    }
    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        None
    }
    fn logs(&self) -> &dyn LogStore;
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore;
    fn bootstrap(&self) -> &dyn StorageBootstrap;
}

pub type DynStorage = Arc<dyn Storage>;
