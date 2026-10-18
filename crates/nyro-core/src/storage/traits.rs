use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::db::models::{
    ApiKeyStats, ApiKeyUsageDetail, ApiKeyWithBindings, CreateApiKey, CreateModel,
    CreateModelBackend, CreateProvider, LogPage, LogQuery, Model, ModelBackend, ModelRatingEntry,
    ModelStats, ModelTimeBucket, ModelUsageDetail, ModelUsageStats, OAuthAccountUpdate,
    OAuthCredential, Provider, ProviderKeyProbeResult, ProviderStats, ProviderUsageDetail,
    RequestLog, RequestResult, StatsHourly, StatsOverview, StatsTimeBucket, UpdateApiKey,
    UpdateModel, UpdateProvider, UpsertOAuthCredential,
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
    /// Persist a per-key probe outcome: on success the discovered model
    /// snapshot replaces `models_snapshot` and `probe_error` clears; on
    /// failure the snapshot is left untouched and the error is recorded.
    async fn record_key_probe_result(
        &self,
        key_id: &str,
        result: ProviderKeyProbeResult,
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
    /// Mixed end-to-end TPS samples over the latest retained calls per
    /// provider/model pair (latest-fifty window, no time filter; see db::tps
    /// for the shared validity pool).
    async fn model_performance_stats(
        &self,
        _pairs: &[(String, String)],
        _as_of: i64,
    ) -> anyhow::Result<Vec<crate::db::PairPerformanceStats>> {
        anyhow::bail!("model performance statistics are unsupported by this storage")
    }
    /// Same statistics, but the latest-fifty window is selected only from calls
    /// created at or after `since` (ms epoch). Used by the Performance page to
    /// keep the chart to recent traffic; `model_performance_stats` stays unfiltered.
    async fn model_performance_stats_since(
        &self,
        _pairs: &[(String, String)],
        _as_of: i64,
        _since: i64,
    ) -> anyhow::Result<Vec<crate::db::PairPerformanceStats>> {
        anyhow::bail!("windowed model performance statistics are unsupported by this storage")
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
    /// Rolling "last N hours" window; `None` covers every retained log.
    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        self.stats_overview_range(hours_window_start(hours), None)
            .await
    }
    /// Inclusive `created_at` range in epoch ms; a `None` bound is open.
    async fn stats_overview_range(
        &self,
        start_at: Option<i64>,
        end_at: Option<i64>,
    ) -> anyhow::Result<StatsOverview>;
    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>>;
    async fn stats_time_buckets(
        &self,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
        upstream_model: Option<&str>,
    ) -> anyhow::Result<Vec<StatsTimeBucket>>;
    /// Sparse UTC days grouped by provider ID and upstream model over the half-open
    /// millisecond range [start_at, end_at). Includes all retained attempts, not final results.
    /// Excludes rows with no provider (NULL or empty `provider_id`): those are
    /// pre-routing rejections (e.g. `no route for model`) that never reached an
    /// upstream, so they carry no billable usage and must not surface as an
    /// "unknown model" cost entry. They remain visible in the request logs.
    async fn token_usage_days(
        &self,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<Vec<crate::db::models::TokenUsageDay>>;
    /// Rolling "last N hours" window; `None` covers every retained log.
    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        self.stats_by_model_range(hours_window_start(hours), None)
            .await
    }
    /// Inclusive `created_at` range in epoch ms; a `None` bound is open.
    async fn stats_by_model_range(
        &self,
        start_at: Option<i64>,
        end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ModelStats>>;
    async fn model_usage_stats(
        &self,
        provider_id: &str,
        upstream_model: &str,
    ) -> anyhow::Result<ModelUsageStats>;
    /// Rolling "last N hours" window; `None` covers every retained log.
    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        self.stats_by_provider_range(hours_window_start(hours), None)
            .await
    }
    /// Inclusive `created_at` range in epoch ms; a `None` bound is open.
    async fn stats_by_provider_range(
        &self,
        start_at: Option<i64>,
        end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ProviderStats>>;
    async fn provider_usage_detail(
        &self,
        provider_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ProviderUsageDetail>;
    /// Rolling "last N hours" window; `None` covers every retained log.
    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        self.stats_by_api_key_range(hours_window_start(hours), None)
            .await
    }
    /// Inclusive `created_at` range in epoch ms; a `None` bound is open.
    async fn stats_by_api_key_range(
        &self,
        start_at: Option<i64>,
        end_at: Option<i64>,
    ) -> anyhow::Result<Vec<ApiKeyStats>>;
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
    /// Active accounts, including disabled/error rows; excludes disconnected tombstones.
    async fn list_accounts(&self, provider_id: &str) -> anyhow::Result<Vec<OAuthCredential>>;
    async fn get_account(
        &self,
        provider_id: &str,
        account_id: &str,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    /// Creates a new UUID; validates OAuth parent/vendor, deduplicates normalized
    /// credentials and enforces non-Devin max_accounts=1 under the same lock.
    async fn create_account(
        &self,
        provider_id: &str,
        label: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential>;
    /// CAS replacement preserves account identity/operator metadata, clears state,
    /// and advances credential generation. Disconnected accounts cannot be revived.
    async fn replace_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    /// Advances status_version (including label-only/no-op updates) to fence attempts;
    /// preserves quota observations and authorization status.
    async fn update_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        update: OAuthAccountUpdate,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    async fn disconnect_account(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<bool>;
    /// Exact old-state + generation CAS; does not advance generation. Callers must
    /// provide sanitized/allowlisted JSON, never upstream errors or secret data.
    async fn compare_account_state(
        &self,
        provider_id: &str,
        account_id: &str,
        expected_version: i32,
        expected_state: &str,
        new_state: &str,
    ) -> anyhow::Result<bool>;
    /// All provider-only methods fail with AMBIGUOUS_ACCOUNT for >1 active row,
    /// counting disabled/error accounts too; writes never affect multiple rows.
    /// Disconnected version fences are never returned as credentials.
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>>;
    /// Atomically bind and advance the generation.
    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential>;
    /// Compensate a failed bind only while its published connected generation
    /// still owns the row. `Some` restores prior credentials with a new generation;
    /// `None` scrubs a tombstone.
    /// A newer bind, refresh, or logout returns false without mutation. Once
    /// multiple account identities have existed, this account-less compensation
    /// conservatively returns false; callers must use account-addressed CAS.
    async fn restore_if_version(
        &self,
        provider_id: &str,
        expected_version: i32,
        previous: Option<UpsertOAuthCredential>,
    ) -> anyhow::Result<bool>;
    /// Remove credentials from the public view and scrub all secret/account data.
    /// Stores retain a disconnected version fence until the provider is deleted,
    /// so logout followed by rebind cannot reuse a previous refresh generation.
    async fn delete(&self, provider_id: &str) -> anyhow::Result<()>;
    /// Atomically acquire the expected connected generation and return that exact
    /// snapshot, never a later bind or another refresher's row.
    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    /// Return only the snapshot actually updated by this refreshing-generation
    /// CAS.
    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    /// Compatibility path: existing drivers' failures remain retryable.
    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        self.fail_refresh_with_policy(provider_id, expected_version, error_message, false, None)
            .await
    }
    /// CAS the refreshing generation to error (terminal) or connected (retryable).
    /// `meta`, when supplied, replaces metadata atomically with the state change;
    /// the caller must pass sanitized, allowlisted metadata (never raw errors).
    /// None preserves metadata. A stale version returns false without mutation.
    async fn fail_refresh_with_policy(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
        terminal: bool,
        meta: Option<String>,
    ) -> anyhow::Result<bool>;
    /// Connected credentials with a known expiry at or before now + before.
    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>>;
    /// Fence refreshes older than timeout and return them to retryable connected.
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

/// Start of a rolling "last N hours" window as epoch ms; `None` stays unbounded.
pub fn hours_window_start(hours: Option<i64>) -> Option<i64> {
    hours.map(|hours| {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(hours.saturating_mul(3_600_000))
    })
}

/// Inclusive `created_at` predicate for the stats queries. Only integer
/// literals are inlined, so it is injection-safe; no bound yields `1=1`.
pub fn range_sql(start_at: Option<i64>, end_at: Option<i64>) -> String {
    match (start_at, end_at) {
        (Some(start), Some(end)) => format!("created_at >= {start} AND created_at <= {end}"),
        (Some(start), None) => format!("created_at >= {start}"),
        (None, Some(end)) => format!("created_at <= {end}"),
        (None, None) => "1=1".to_string(),
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn range_sql_covers_every_bound_combination() {
        assert_eq!(range_sql(None, None), "1=1");
        assert_eq!(range_sql(Some(5), None), "created_at >= 5");
        assert_eq!(range_sql(None, Some(9)), "created_at <= 9");
        assert_eq!(
            range_sql(Some(5), Some(9)),
            "created_at >= 5 AND created_at <= 9"
        );
    }

    #[test]
    fn hours_window_start_is_none_without_hours_and_in_the_past_otherwise() {
        assert_eq!(hours_window_start(None), None);
        let now = chrono::Utc::now().timestamp_millis();
        let start = hours_window_start(Some(2)).unwrap();
        assert!((now - start - 7_200_000).abs() < 5_000);
    }
}
