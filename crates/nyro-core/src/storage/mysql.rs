mod model_ratings;

use crate::storage::ModelRatingStore;
use model_ratings::MysqlModelRatingStore;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use sqlx::{MySql, Pool};
use std::time::Duration;

use crate::db::models::{
    ApiKey, ApiKeyModelRouteStats, ApiKeyStats, ApiKeyUsageDetail, ApiKeyWithBindings,
    CreateApiKey, CreateModel, CreateModelBackend, CreateProvider, CreateProviderProtocolEndpoint,
    LogPage, LogQuery, Model, ModelApiKeyUsageStats, ModelBackend, ModelProviderUsageStats,
    ModelStats, ModelTimeBucket, ModelUsageDetail, ModelUsageStats, ModelUsageTotals,
    OAuthCredential, Provider, ProviderModelUsageStats, ProviderProtocolEndpoint, ProviderStats,
    ProviderUsageDetail, RecentModelPerformance, RequestLog, RequestResult, StatsHourly,
    StatsOverview, StatsTimeBucket, UpdateApiKey, UpdateModel, UpdateProvider,
    UpsertOAuthCredential, is_valid_provider_auth_mode,
};
use crate::logging::LogEntry;
use crate::logging::diagnostics::{error_sql, outcome_sql};
use crate::storage::sql::config::SqlBackendConfig;
use crate::storage::sql::pool::RelationalPool;
use crate::storage::traits::{
    ApiKeyAccessRecord, ApiKeyStore, AuthAccessStore, LogStore, ModelBackendStore,
    ModelSnapshotStore, ModelStore, OAuthCredentialStore, ProviderEndpointTestResult,
    ProviderStore, ProviderTestResult, SettingsStore, Storage, StorageBackend, StorageBootstrap,
    StorageHealth, UsageWindow,
};

#[derive(Clone)]
pub struct MysqlAdapter {
    pool: Pool<MySql>,
    config: SqlBackendConfig,
}

#[derive(Debug, Clone)]
pub struct MysqlHealth {
    pub can_connect: bool,
    pub schema_compatible: bool,
}

impl MysqlAdapter {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let pool =
            RelationalPool::connect(crate::storage::sql::config::SqlBackendKind::Mysql, &config)
                .await
                .context("connect mysql adapter")?;
        let pool = pool
            .as_mysql()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("relational pool kind mismatch: expected mysql"))?;
        Ok(Self { pool, config })
    }

    pub fn config(&self) -> &SqlBackendConfig {
        &self.config
    }

    pub fn pool(&self) -> &Pool<MySql> {
        &self.pool
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn health(&self) -> MysqlHealth {
        let can_connect = self.ping().await.is_ok();
        // Missing rating storage means this database still needs migration.
        let schema_compatible = if can_connect {
            mysql_table_exists(&self.pool, "models")
                .await
                .unwrap_or(false)
                && mysql_table_exists(&self.pool, "model_rating_prefixes")
                    .await
                    .unwrap_or(false)
                && sqlx::query("SELECT performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at, client_request_id, attempt_index, outcome_version, attempt_outcome, failure_kind, failure_stage, error_message, error_causes_json, payload_metadata_json, payload_cleared_at FROM request_logs LIMIT 0").execute(&self.pool).await.is_ok()
                && sqlx::query("SELECT client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at FROM request_results LIMIT 0").execute(&self.pool).await.is_ok()
        } else {
            false
        };
        MysqlHealth {
            can_connect,
            schema_compatible,
        }
    }
}

#[derive(Clone)]
pub struct MysqlStorage {
    pool: Pool<MySql>,
    provider_store: Arc<MysqlProviderStore>,
    model_rating_store: Arc<MysqlModelRatingStore>,
    model_store: Arc<MysqlModelStore>,
    model_backend_store: Arc<MysqlModelBackendStore>,
    settings_store: Arc<MysqlSettingsStore>,
    api_key_store: Arc<MysqlApiKeyStore>,
    auth_store: Arc<MysqlAuthAccessStore>,
    oauth_credential_store: Arc<MysqlOAuthCredentialStore>,
    log_store: Arc<MysqlLogStore>,
    bootstrap: Arc<MysqlBootstrap>,
}

impl MysqlStorage {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let adapter = MysqlAdapter::connect(config).await?;
        let pool = adapter.pool().clone();
        let provider_store = Arc::new(MysqlProviderStore { pool: pool.clone() });
        let model_rating_store = Arc::new(MysqlModelRatingStore { pool: pool.clone() });
        let model_store = Arc::new(MysqlModelStore { pool: pool.clone() });
        let model_backend_store = Arc::new(MysqlModelBackendStore { pool: pool.clone() });
        let settings_store = Arc::new(MysqlSettingsStore { pool: pool.clone() });
        let api_key_store = Arc::new(MysqlApiKeyStore { pool: pool.clone() });
        let auth_store = Arc::new(MysqlAuthAccessStore { pool: pool.clone() });
        let oauth_credential_store = Arc::new(MysqlOAuthCredentialStore { pool: pool.clone() });
        let log_store = Arc::new(MysqlLogStore { pool: pool.clone() });
        let bootstrap = Arc::new(MysqlBootstrap { adapter });
        Ok(Self {
            pool,
            provider_store,
            model_rating_store,
            model_store,
            model_backend_store,
            settings_store,
            api_key_store,
            auth_store,
            oauth_credential_store,
            log_store,
            bootstrap,
        })
    }

    pub fn pool(&self) -> &Pool<MySql> {
        &self.pool
    }
}

impl Storage for MysqlStorage {
    fn providers(&self) -> &dyn ProviderStore {
        self.provider_store.as_ref()
    }

    fn model_ratings(&self) -> Option<&dyn ModelRatingStore> {
        Some(self.model_rating_store.as_ref())
    }

    fn models(&self) -> &dyn ModelStore {
        self.model_store.as_ref()
    }

    fn snapshots(&self) -> &dyn ModelSnapshotStore {
        self.model_store.as_ref()
    }

    fn settings(&self) -> &dyn SettingsStore {
        self.settings_store.as_ref()
    }

    fn model_backends(&self) -> Option<&dyn ModelBackendStore> {
        Some(self.model_backend_store.as_ref())
    }

    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        Some(self.api_key_store.as_ref())
    }

    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        Some(self.auth_store.as_ref())
    }

    fn logs(&self) -> &dyn LogStore {
        self.log_store.as_ref()
    }

    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.oauth_credential_store.as_ref()
    }

    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self.bootstrap.as_ref()
    }
}

// ---------------------------------------------------------------------------
// OAuth Credential Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlOAuthCredentialStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl OAuthCredentialStore for MysqlOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(sqlx::query_as::<_, OAuthCredential>(
            "SELECT provider_id, driver_key, scheme, access_token, refresh_token, DATE_FORMAT(expires_at, '%Y-%m-%d %H:%i:%S') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, DATE_FORMAT(last_refresh_at, '%Y-%m-%d %H:%i:%S') AS last_refresh_at, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at, DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%S') AS updated_at FROM provider_oauth_credentials WHERE provider_id = ?",
        )
        .bind(provider_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential> {
        sqlx::query(
            "INSERT INTO provider_oauth_credentials (provider_id, driver_key, scheme, access_token, refresh_token, expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'connected', 0, NULL) ON DUPLICATE KEY UPDATE driver_key=VALUES(driver_key), scheme=VALUES(scheme), access_token=VALUES(access_token), refresh_token=VALUES(refresh_token), expires_at=VALUES(expires_at), resource_url=VALUES(resource_url), subject_id=VALUES(subject_id), scopes=VALUES(scopes), meta=VALUES(meta), status='connected', status_version=status_version+1, last_error=NULL, updated_at=NOW()",
        )
        .bind(provider_id)
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(&input.expires_at)
        .bind(&input.resource_url)
        .bind(&input.subject_id)
        .bind(input.scopes.as_deref().unwrap_or("[]"))
        .bind(input.meta.as_deref().unwrap_or("{}"))
        .execute(&self.pool)
        .await?;
        self.get(provider_id)
            .await?
            .context("credential not found after upsert")
    }

    async fn delete(&self, provider_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM provider_oauth_credentials WHERE provider_id = ?")
            .bind(provider_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='refreshing', status_version=status_version+1, updated_at=NOW() WHERE provider_id=? AND status='connected' AND status_version=?",
        )
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() > 0 {
            Ok(self.get(provider_id).await?)
        } else {
            Ok(None)
        }
    }

    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<Option<OAuthCredential>> {
        sqlx::query(
            "UPDATE provider_oauth_credentials SET driver_key=?, scheme=?, access_token=?, refresh_token=?, expires_at=?, resource_url=?, subject_id=?, scopes=?, meta=?, status='connected', status_version=status_version+1, last_error=NULL, last_refresh_at=NOW(), updated_at=NOW() WHERE provider_id=? AND status='refreshing' AND status_version=?",
        )
        .bind(&input.driver_key)
        .bind(&input.scheme)
        .bind(&input.access_token)
        .bind(&input.refresh_token)
        .bind(&input.expires_at)
        .bind(&input.resource_url)
        .bind(&input.subject_id)
        .bind(input.scopes.as_deref().unwrap_or("[]"))
        .bind(input.meta.as_deref().unwrap_or("{}"))
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        let current = self.get(provider_id).await?;
        Ok(current.filter(|credential| credential.status_version == expected_version + 1))
    }

    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='connected', last_error=?, status_version=status_version+1, updated_at=NOW() WHERE provider_id=? AND status='refreshing' AND status_version=?",
        )
        .bind(error_message)
        .bind(provider_id)
        .bind(expected_version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>> {
        let seconds = before.as_secs() as i64;
        Ok(sqlx::query_as::<_, OAuthCredential>(
            "SELECT provider_id, driver_key, scheme, access_token, refresh_token, DATE_FORMAT(expires_at, '%Y-%m-%d %H:%i:%S') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, DATE_FORMAT(last_refresh_at, '%Y-%m-%d %H:%i:%S') AS last_refresh_at, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at, DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%S') AS updated_at FROM provider_oauth_credentials WHERE status='connected' AND expires_at IS NOT NULL AND expires_at <= NOW() + INTERVAL ? SECOND",
        )
        .bind(seconds)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let seconds = timeout.as_secs() as i64;
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='connected', last_error='refresh timeout: process did not complete within timeout', status_version=status_version+1, updated_at=NOW() WHERE status='refreshing' AND updated_at + INTERVAL ? SECOND < NOW()",
        )
        .bind(seconds)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// Provider Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlProviderStore {
    pool: Pool<MySql>,
}

impl MysqlProviderStore {
    async fn load_endpoints(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<Vec<ProviderProtocolEndpoint>> {
        let base = "SELECT id, provider_id, protocol, base_url, api_key, COALESCE(auth_scheme, 'auto') AS auth_scheme, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(priority, 0) AS priority, COALESCE(test_status, 'untested') AS test_status, test_error, DATE_FORMAT(tested_at, '%Y-%m-%d %H:%i:%S') AS tested_at, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at, DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%S') AS updated_at FROM provider_protocol_endpoints";
        let endpoints = if let Some(provider_id) = provider_id {
            sqlx::query_as::<_, ProviderProtocolEndpoint>(&format!(
                "{base} WHERE provider_id = ? ORDER BY priority, created_at, id"
            ))
            .bind(provider_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, ProviderProtocolEndpoint>(&format!(
                "{base} ORDER BY provider_id, priority, created_at, id"
            ))
            .fetch_all(&self.pool)
            .await?
        };
        Ok(endpoints)
    }
}

fn mysql_endpoint_inputs_or_legacy(input: &CreateProvider) -> Vec<CreateProviderProtocolEndpoint> {
    if !input.protocol_endpoints.is_empty() {
        return input.protocol_endpoints.clone();
    }
    vec![CreateProviderProtocolEndpoint {
        protocol: input.protocol.clone(),
        base_url: input.base_url.clone(),
        api_key: input.api_key.clone(),
        auth_scheme: "auto".to_string(),
        is_enabled: true,
        priority: 0,
    }]
}

#[async_trait]
impl ProviderStore for MysqlProviderStore {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        let mut providers = sqlx::query_as::<_, Provider>(&provider_select(None))
            .fetch_all(&self.pool)
            .await?;
        let mut by_provider: HashMap<String, Vec<ProviderProtocolEndpoint>> = HashMap::new();
        for endpoint in self.load_endpoints(None).await? {
            by_provider
                .entry(endpoint.provider_id.clone())
                .or_default()
                .push(endpoint);
        }
        for provider in &mut providers {
            provider.protocol_endpoints = by_provider.remove(&provider.id).unwrap_or_default();
        }
        Ok(providers)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        let mut provider = sqlx::query_as::<_, Provider>(&provider_select(Some("WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(provider) = &mut provider {
            provider.protocol_endpoints = self.load_endpoints(Some(id)).await?;
        }
        Ok(provider)
    }

    async fn create(&self, input: CreateProvider) -> anyhow::Result<Provider> {
        let id = uuid::Uuid::new_v4().to_string();
        let vendor = normalize_provider_vendor(input.vendor.as_deref());
        let models_source = input.effective_models_source().map(ToString::to_string);
        let endpoint_inputs = mysql_endpoint_inputs_or_legacy(&input);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO providers (id, name, vendor, protocol, base_url, protocol_mode, preset_key, channel, models_source, static_models, api_key, auth_mode, use_proxy, fast_mode) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(vendor)
        .bind(input.protocol.trim())
        .bind(input.base_url.trim())
        .bind(input.protocol_mode.trim())
        .bind(input.preset_key)
        .bind(input.channel)
        .bind(models_source)
        .bind(input.static_models)
        .bind(input.api_key)
        .bind(input.auth_mode)
        .bind(input.use_proxy)
        .bind(input.fast_mode)
        .execute(&mut *tx)
        .await?;
        for endpoint in endpoint_inputs {
            sqlx::query(
                "INSERT INTO provider_protocol_endpoints (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&id)
            .bind(endpoint.protocol.trim())
            .bind(endpoint.base_url.trim())
            .bind(endpoint.api_key)
            .bind(endpoint.auth_scheme.trim())
            .bind(endpoint.is_enabled)
            .bind(endpoint.priority)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.get(&id)
            .await?
            .context("provider missing after create")
    }

    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider> {
        let current = self
            .get(id)
            .await?
            .context("provider not found for update")?;
        let replace_endpoints = input.protocol_endpoints.is_some();
        let endpoint_inputs = input.protocol_endpoints.clone();
        let models_source_input = input.models_source.map(|value| value.trim().to_string());
        let name = input.name.unwrap_or(current.name);
        let vendor = if input.vendor.is_some() {
            normalize_provider_vendor(input.vendor.as_deref())
        } else {
            normalize_provider_vendor(current.vendor.as_deref())
        };
        let models_source = models_source_input.or_else(|| current.models_source.clone());
        let protocol = input.protocol.unwrap_or(current.protocol.clone());
        let base_url = input.base_url.unwrap_or(current.base_url);
        let protocol_mode = input.protocol_mode.unwrap_or(current.protocol_mode);
        let preset_key = input.preset_key.or(current.preset_key);
        let channel = input.channel.or(current.channel);
        let static_models = input.static_models.or(current.static_models);
        let api_key = input.api_key.unwrap_or(current.api_key);
        let auth_mode = input.auth_mode.unwrap_or(current.auth_mode);
        if !is_valid_provider_auth_mode(&auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", auth_mode);
        }
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let fast_mode = input.fast_mode.unwrap_or(current.fast_mode);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE providers SET name=?, vendor=?, protocol=?, base_url=?, protocol_mode=?, preset_key=?, channel=?, models_source=?, static_models=?, api_key=?, auth_mode=?, use_proxy=?, fast_mode=?, is_enabled=?, updated_at=NOW() WHERE id=?",
        )
        .bind(name.trim())
        .bind(vendor)
        .bind(protocol.trim())
        .bind(base_url.trim())
        .bind(protocol_mode.trim())
        .bind(preset_key)
        .bind(channel)
        .bind(models_source)
        .bind(static_models)
        .bind(api_key)
        .bind(auth_mode)
        .bind(use_proxy)
        .bind(fast_mode)
        .bind(is_enabled)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if replace_endpoints {
            sqlx::query("DELETE FROM provider_protocol_endpoints WHERE provider_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            for endpoint in endpoint_inputs.unwrap_or_default() {
                sqlx::query(
                    "INSERT INTO provider_protocol_endpoints (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(uuid::Uuid::new_v4().to_string())
                .bind(id)
                .bind(endpoint.protocol.trim())
                .bind(endpoint.base_url.trim())
                .bind(endpoint.api_key)
                .bind(endpoint.auth_scheme.trim())
                .bind(endpoint.is_enabled)
                .bind(endpoint.priority)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        self.get(id).await?.context("provider missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "DELETE FROM model_backends
             WHERE provider_id = ?
                OR model_id IN (SELECT id FROM models WHERE target_provider = ?)",
        )
        .bind(id)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM models WHERE target_provider = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM provider_protocol_endpoints WHERE provider_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) AND id != ? LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }

    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()> {
        let _ = result.tested_at;
        sqlx::query(
            "UPDATE providers SET last_test_success = ?, last_test_at = NOW() WHERE id = ?",
        )
        .bind(result.success)
        .bind(provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_endpoint_test_result(
        &self,
        endpoint_id: &str,
        result: ProviderEndpointTestResult,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE provider_protocol_endpoints SET test_status = ?, test_error = ?, tested_at = ?, updated_at = NOW() WHERE id = ?",
        )
        .bind(if result.success { "success" } else { "failed" })
        .bind(result.error)
        .bind(result.tested_at)
        .bind(endpoint_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Model Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlModelStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl ModelStore for MysqlModelStore {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(
            sqlx::query_as::<_, Model>(&model_select(Some("ORDER BY created_at DESC")))
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        let sql = format!("{} WHERE id = ?", model_select(None));
        Ok(sqlx::query_as::<_, Model>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create(&self, input: CreateModel) -> anyhow::Result<Model> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO models (id, name, balance, target_provider, target_model, enable_auth, enable_payload, vision_shim, force_max_reasoning) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(input.name.trim())
        .bind(input.balance.unwrap_or_else(|| "weighted".to_string()))
        .bind(input.target_provider.trim())
        .bind(input.target_model.trim())
        .bind(input.enable_auth.unwrap_or(false))
        .bind(input.enable_payload)
        .bind(crate::db::models::vision_shim_value_to_raw(
            input.vision_shim.as_ref().unwrap_or(&serde_json::Value::Null),
        )?)
        .bind(input.force_max_reasoning.unwrap_or(false))
        .execute(&self.pool)
        .await?;
        self.get(&id).await?.context("model missing after create")
    }

    async fn update(&self, id: &str, input: UpdateModel) -> anyhow::Result<Model> {
        let current = self.get(id).await?.context("model not found for update")?;
        let name = input.name.unwrap_or(current.name);
        let balance = input.balance.unwrap_or(current.balance);
        let target_provider = input.target_provider.unwrap_or(current.target_provider);
        let target_model = input.target_model.unwrap_or(current.target_model);
        let enable_auth = input.enable_auth.unwrap_or(current.enable_auth);
        let enable_payload = input.enable_payload.unwrap_or(current.enable_payload);
        let force_max_reasoning = input
            .force_max_reasoning
            .unwrap_or(current.force_max_reasoning);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let vision_shim = match input.vision_shim.as_ref() {
            Some(value) => crate::db::models::vision_shim_value_to_raw(value)?,
            None => current.vision_shim.clone(),
        };

        sqlx::query(
            "UPDATE models SET name=?, balance=?, target_provider=?, target_model=?, enable_auth=?, enable_payload=?, vision_shim=?, force_max_reasoning=?, is_enabled=? WHERE id=?",
        )
        .bind(name.trim())
        .bind(balance.trim().to_lowercase())
        .bind(target_provider.trim())
        .bind(target_model.trim())
        .bind(enable_auth)
        .bind(enable_payload)
        .bind(vision_shim)
        .bind(force_max_reasoning)
        .bind(is_enabled)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get(id).await?.context("model missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) AND id != ? LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }
}

#[async_trait]
impl ModelSnapshotStore for MysqlModelStore {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        let sql = format!("{} WHERE COALESCE(is_enabled, 1) = 1", model_select(None));
        Ok(sqlx::query_as::<_, Model>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }
}

// ---------------------------------------------------------------------------
// Model Backend Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlModelBackendStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl ModelBackendStore for MysqlModelBackendStore {
    async fn list_backends_by_model(&self, model_id: &str) -> anyhow::Result<Vec<ModelBackend>> {
        Ok(sqlx::query_as::<_, ModelBackend>(
            "SELECT id, model_id, provider_id, model, weight, priority, is_fallback, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at FROM model_backends WHERE model_id = ? ORDER BY priority ASC, created_at ASC",
        )
        .bind(model_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn set_backends(
        &self,
        model_id: &str,
        backends: &[CreateModelBackend],
    ) -> anyhow::Result<Vec<ModelBackend>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM model_backends WHERE model_id = ?")
            .bind(model_id)
            .execute(&mut *tx)
            .await?;

        for backend in backends {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, is_fallback) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(model_id)
            .bind(backend.provider_id.trim())
            .bind(backend.model.trim())
            .bind(backend.weight.unwrap_or(100).max(0))
            .bind(backend.priority.unwrap_or(1).max(1))
            .bind(backend.is_fallback.unwrap_or(false))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        self.list_backends_by_model(model_id).await
    }

    async fn delete_backends_by_model(&self, model_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM model_backends WHERE model_id = ?")
            .bind(model_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Settings Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlSettingsStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl SettingsStore for MysqlSettingsStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (name, value, updated_at) VALUES (?, ?, NOW()) ON DUPLICATE KEY UPDATE value=VALUES(value), updated_at=NOW()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_all(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(
            sqlx::query_as::<_, (String, String)>("SELECT name, value FROM settings")
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

// ---------------------------------------------------------------------------
// API Key Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlApiKeyStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl ApiKeyStore for MysqlApiKeyStore {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>> {
        let rows = sqlx::query_as::<_, ApiKey>(&api_key_select(None))
            .fetch_all(&self.pool)
            .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let model_ids = list_api_key_model_ids(&self.pool, &row.id).await?;
            items.push(api_key_with_bindings(row, model_ids));
        }
        Ok(items)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>> {
        let row = sqlx::query_as::<_, ApiKey>(&api_key_select(Some("WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let model_ids = list_api_key_model_ids(&self.pool, id).await?;
        Ok(Some(api_key_with_bindings(row, model_ids)))
    }

    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("sk-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO api_keys (id, token, name, rpm, rpd, tpm, tpd, is_privileged, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULLIF(?, ''))",
        )
        .bind(&id)
        .bind(&key)
        .bind(input.name.trim())
        .bind(input.rpm)
        .bind(input.rpd)
        .bind(input.tpm)
        .bind(input.tpd)
        .bind(input.is_privileged)
        .bind(input.expires_at.as_deref().map(str::trim).unwrap_or(""))
        .execute(&self.pool)
        .await?;
        replace_api_key_models(&self.pool, &id, &input.model_ids).await?;
        self.get(&id).await?.context("api key missing after create")
    }

    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings> {
        let current = sqlx::query_as::<_, ApiKey>(&api_key_select(Some("WHERE id = ?")))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .context("api key not found for update")?;
        let name = input.name.unwrap_or(current.name);
        let rpm = input.rpm.or(current.rpm);
        let rpd = input.rpd.or(current.rpd);
        let tpm = input.tpm.or(current.tpm);
        let tpd = input.tpd.or(current.tpd);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let is_privileged = input.is_privileged.unwrap_or(current.is_privileged);
        let expires_at = input.expires_at.or(current.expires_at);

        sqlx::query(
            "UPDATE api_keys SET name=?, rpm=?, rpd=?, tpm=?, tpd=?, is_enabled=?, is_privileged=?, expires_at=NULLIF(?, ''), updated_at=NOW() WHERE id=?",
        )
        .bind(name.trim())
        .bind(rpm)
        .bind(rpd)
        .bind(tpm)
        .bind(tpd)
        .bind(is_enabled)
        .bind(is_privileged)
        .bind(expires_at.as_deref().map(str::trim).unwrap_or(""))
        .bind(id)
        .execute(&self.pool)
        .await?;

        if let Some(model_ids) = input.model_ids {
            replace_api_key_models(&self.pool, id, &model_ids).await?;
        }
        self.get(id).await?.context("api key missing after update")
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) AND id != ? LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE LOWER(TRIM(name)) = LOWER(TRIM(?)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }
}

// ---------------------------------------------------------------------------
// Auth Access Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlAuthAccessStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl AuthAccessStore for MysqlAuthAccessStore {
    async fn find_api_key(&self, raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                bool,
                bool,
                Option<String>,
                Option<i32>,
                Option<i32>,
                Option<i32>,
                Option<i32>,
            ),
        >(
            "SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(is_privileged, 0) AS is_privileged, DATE_FORMAT(expires_at, '%Y-%m-%d %H:%i:%S') AS expires_at, rpm, rpd, tpm, tpd FROM api_keys WHERE token = ?",
        )
        .bind(raw_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(id, name, is_enabled, is_privileged, expires_at, rpm, rpd, tpm, tpd)| {
                ApiKeyAccessRecord {
                    id,
                    name,
                    is_enabled,
                    is_privileged,
                    expires_at,
                    rpm,
                    rpd,
                    tpm,
                    tpd,
                }
            },
        ))
    }

    async fn model_binding_exists(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM api_key_models WHERE api_key_id = ? AND model_id = ?",
        )
        .bind(api_key_id)
        .bind(model_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>> {
        list_api_key_model_ids(&self.pool, api_key_id).await
    }

    async fn request_count_since(
        &self,
        api_key_id: &str,
        window: UsageWindow,
    ) -> anyhow::Result<i64> {
        let seconds: i64 = match window {
            UsageWindow::Minute => 60,
            UsageWindow::Day => 86400,
        };
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM request_logs WHERE api_key_id = ? AND created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL ? SECOND) * 1000"
        )
        .bind(api_key_id)
        .bind(seconds)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn token_count_since(
        &self,
        api_key_id: &str,
        window: UsageWindow,
    ) -> anyhow::Result<i64> {
        let seconds: i64 = match window {
            UsageWindow::Minute => 60,
            UsageWindow::Day => 86400,
        };
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT CAST(COALESCE(SUM(input_tokens + output_tokens), 0) AS SIGNED) FROM request_logs WHERE api_key_id = ? AND created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL ? SECOND) * 1000"
        )
        .bind(api_key_id)
        .bind(seconds)
        .fetch_one(&self.pool)
        .await?)
    }
}

// ---------------------------------------------------------------------------
// Log Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlLogStore {
    pool: Pool<MySql>,
}

#[async_trait]
impl LogStore for MysqlLogStore {
    async fn model_performance_stats(
        &self,
        pairs: &[(String, String)],
        as_of: i64,
    ) -> anyhow::Result<Vec<crate::db::PairPerformanceStats>> {
        crate::db::model_performance::model_performance_method!(
            self,
            pairs,
            as_of,
            sqlx::MySql,
            "BINARY upstream_model",
            "CAST(created_at AS SIGNED)",
            "0"
        )
    }
    async fn distinct_logged_pairs(&self) -> anyhow::Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT DISTINCT provider_id, upstream_model FROM request_logs")
            .fetch_all(&self.pool)
            .await?;
        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows {
            use sqlx::Row;
            pairs.push((
                row.try_get::<Option<String>, _>(0)?.unwrap_or_default(),
                row.try_get::<Option<String>, _>(1)?.unwrap_or_default(),
            ));
        }
        Ok(pairs)
    }
    async fn append_batch(&self, entries: Vec<LogEntry>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            let error_causes = serde_json::to_string(&entry.diagnostic.error_causes)?;
            let payload_metadata = serde_json::to_string(&entry.diagnostic.payload_metadata)?;
            sqlx::query(
                r#"INSERT INTO request_logs
                    (id, created_at, api_key_id, api_key_name,
                     client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url,
                     client_model, upstream_model, reasoning_effort, route_decision,
                     method, path,
                     client_request_headers, client_request_body,
                     client_response_headers, client_response_body,
                     upstream_request_headers, upstream_request_body,
                     upstream_response_headers, upstream_response_body,
                     upstream_status_code, client_status_code,
                     latency_total_ms, latency_upstream_ms,
                     input_tokens, output_tokens, cache_read_tokens,
                     is_stream, stream_chunks_count, stream_first_chunk_ms,
                     performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at,
                     client_request_id, attempt_index, outcome_version, attempt_outcome,
                     failure_kind, failure_stage, error_message, error_causes_json, payload_metadata_json)
                VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?,?)"#,
            )
            .bind(&entry.diagnostic.log_id)
            .bind(entry.created_at)
            .bind(&entry.api_key_id)
            .bind(&entry.api_key_name)
            .bind(&entry.client_protocol)
            .bind(&entry.upstream_protocol)
            .bind(&entry.provider_id)
            .bind(&entry.provider_name)
            .bind(&entry.model_id)
            .bind(&entry.model_name)
            .bind(&entry.upstream_url)
            .bind(&entry.client_model)
            .bind(&entry.upstream_model)
            .bind(&entry.reasoning_effort)
            .bind(&entry.route_decision)
            .bind(&entry.method)
            .bind(&entry.path)
            .bind(&entry.client_request_headers)
            .bind(&entry.client_request_body)
            .bind(&entry.client_response_headers)
            .bind(&entry.client_response_body)
            .bind(&entry.upstream_request_headers)
            .bind(&entry.upstream_request_body)
            .bind(&entry.upstream_response_headers)
            .bind(&entry.upstream_response_body)
            .bind(entry.upstream_status_code)
            .bind(entry.client_status_code)
            .bind(entry.latency_total_ms)
            .bind(entry.latency_upstream_ms)
            .bind(entry.input_tokens())
            .bind(entry.output_tokens())
            .bind(entry.cache_read_tokens())
            .bind(entry.is_stream)
            .bind(entry.stream_chunks_count)
            .bind(entry.stream_first_chunk_ms)
            .bind(entry.performance.version)
            .bind(&entry.performance.effort_status)
            .bind(&entry.performance.effort_raw)
            .bind(&entry.performance.effort_tier)
            .bind(&entry.performance.completion)
            .bind(&entry.performance.completion_reason)
            .bind(&entry.performance.response_mode)
            .bind(entry.performance.upstream_duration_ms)
            .bind(entry.performance.first_chunk_ms)
            .bind(entry.performance.completed_at)
            .bind(&entry.diagnostic.client_request_id)
            .bind(entry.diagnostic.attempt_index)
            .bind(entry.diagnostic.outcome_version)
            .bind(&entry.diagnostic.attempt_outcome)
            .bind(&entry.diagnostic.failure_kind)
            .bind(&entry.diagnostic.failure_stage)
            .bind(&entry.diagnostic.error_message)
            .bind(error_causes)
            .bind(payload_metadata)
            .execute(&mut *tx)
            .await?;
            if let Some(result) = &entry.diagnostic.final_result {
                sqlx::query(
                    "INSERT INTO request_results (client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE final_outcome = VALUES(final_outcome), final_attempt_id = VALUES(final_attempt_id), attempt_count = VALUES(attempt_count), finished_at = VALUES(finished_at)",
                )
                .bind(&result.client_request_id)
                .bind(&result.final_outcome)
                .bind(&result.final_attempt_id)
                .bind(result.attempt_count)
                .bind(result.finished_at)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn query(&self, query: LogQuery) -> anyhow::Result<LogPage> {
        let mut count_sql = String::from("SELECT COUNT(*) AS total FROM request_logs WHERE 1=1");
        // List query skips heavy body/header columns (NULL placeholders preserve struct layout).
        let mut data_sql = String::from(
            "SELECT id, COALESCE(created_at, 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, reasoning_effort, route_decision, method, path, \
             CAST(NULL AS CHAR) AS client_request_headers, CAST(NULL AS CHAR) AS client_request_body, \
             CAST(NULL AS CHAR) AS client_response_headers, CAST(NULL AS CHAR) AS client_response_body, \
             CAST(NULL AS CHAR) AS upstream_request_headers, CAST(NULL AS CHAR) AS upstream_request_body, \
             CAST(NULL AS CHAR) AS upstream_response_headers, CAST(NULL AS CHAR) AS upstream_response_body, \
             upstream_status_code, client_status_code, \
             latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(is_stream, 0) AS is_stream, stream_chunks_count, stream_first_chunk_ms, \
             performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at, \
             CAST(client_request_id AS CHAR CHARACTER SET utf8mb4) AS client_request_id, attempt_index, outcome_version, \
             CAST(attempt_outcome AS CHAR CHARACTER SET utf8mb4) AS attempt_outcome, failure_kind, failure_stage, error_message, \
             error_causes_json AS error_causes, payload_metadata_json AS payload_metadata, payload_cleared_at \
             FROM request_logs WHERE 1=1",
        );
        let mut bind_values: Vec<String> = Vec::new();

        if let Some(provider) = query.provider.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND provider_id = ?");
            data_sql.push_str(" AND provider_id = ?");
            bind_values.push(provider);
        }
        if let Some(client_model) = query.client_model.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND client_model = ?");
            data_sql.push_str(" AND client_model = ?");
            bind_values.push(client_model);
        }
        if let Some(upstream_model) = query
            .upstream_model
            .filter(|v| !v.is_empty())
            .or_else(|| query.model.filter(|v| !v.is_empty()))
        {
            count_sql.push_str(" AND upstream_model = ?");
            data_sql.push_str(" AND upstream_model = ?");
            bind_values.push(upstream_model);
        }
        if let Some(status_min) = query.status_min {
            count_sql.push_str(" AND client_status_code >= ?");
            data_sql.push_str(" AND client_status_code >= ?");
            bind_values.push(status_min.to_string());
        }
        if let Some(status_max) = query.status_max {
            count_sql.push_str(" AND client_status_code <= ?");
            data_sql.push_str(" AND client_status_code <= ?");
            bind_values.push(status_max.to_string());
        }
        if let Some(is_error) = query.is_error {
            let predicate = error_sql("");
            let filter = if is_error {
                format!(" AND {predicate}")
            } else {
                format!(" AND NOT {predicate}")
            };
            count_sql.push_str(&filter);
            data_sql.push_str(&filter);
        }
        if let Some(outcome) = query.outcome {
            anyhow::ensure!(
                matches!(
                    outcome.as_str(),
                    "error" | "completed" | "cancelled" | "output_limited" | "unknown"
                ),
                "unsupported log outcome: {outcome}"
            );
            let filter = format!(" AND {}", outcome_sql("", &outcome));
            count_sql.push_str(&filter);
            data_sql.push_str(&filter);
        }
        if let Some(client_request_id) = query.client_request_id {
            count_sql.push_str(" AND client_request_id = ?");
            data_sql.push_str(" AND client_request_id = ?");
            bind_values.push(client_request_id);
        }
        if let Some(api_key) = query.api_key.filter(|v| !v.is_empty()) {
            count_sql.push_str(" AND api_key_id = ?");
            data_sql.push_str(" AND api_key_id = ?");
            bind_values.push(api_key);
        }
        if let Some(after) = query.after {
            count_sql.push_str(" AND created_at >= ?");
            data_sql.push_str(" AND created_at >= ?");
            bind_values.push(after.to_string());
        }
        if let Some(before) = query.before {
            count_sql.push_str(" AND created_at <= ?");
            data_sql.push_str(" AND created_at <= ?");
            bind_values.push(before.to_string());
        }

        data_sql.push_str(" ORDER BY created_at DESC LIMIT ? OFFSET ?");

        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        let mut data_query = sqlx::query_as::<_, RequestLog>(&data_sql);
        for value in &bind_values {
            count_query = count_query.bind(value);
            data_query = data_query.bind(value);
        }

        let total = count_query.fetch_one(&self.pool).await?;
        let items = data_query
            .bind(query.limit.unwrap_or(50))
            .bind(query.offset.unwrap_or(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(LogPage { items, total })
    }

    async fn find_by_id(&self, id: &str) -> anyhow::Result<Option<RequestLog>> {
        let row = sqlx::query_as::<_, RequestLog>(
            "SELECT id, COALESCE(created_at, 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, reasoning_effort, route_decision, method, path, \
             client_request_headers, client_request_body, \
             client_response_headers, client_response_body, \
             upstream_request_headers, upstream_request_body, \
             upstream_response_headers, upstream_response_body, \
             upstream_status_code, client_status_code, \
             latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(is_stream, 0) AS is_stream, stream_chunks_count, stream_first_chunk_ms, \
             performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at, \
             CAST(client_request_id AS CHAR CHARACTER SET utf8mb4) AS client_request_id, attempt_index, outcome_version, \
             CAST(attempt_outcome AS CHAR CHARACTER SET utf8mb4) AS attempt_outcome, failure_kind, failure_stage, error_message, \
             error_causes_json AS error_causes, payload_metadata_json AS payload_metadata, payload_cleared_at \
             FROM request_logs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn request_result(
        &self,
        client_request_id: &str,
    ) -> anyhow::Result<Option<RequestResult>> {
        Ok(sqlx::query_as::<_, RequestResult>(
            "SELECT CAST(client_request_id AS CHAR CHARACTER SET utf8mb4) AS client_request_id, \
             CAST(final_outcome AS CHAR CHARACTER SET utf8mb4) AS final_outcome, \
             final_attempt_id, attempt_count, finished_at FROM request_results WHERE client_request_id = ?",
        )
        .bind(client_request_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn cleanup_before(&self, cutoff_expression: &str) -> anyhow::Result<u64> {
        let interval = cutoff_expression.trim().trim_start_matches('-').trim();
        let mut parts = interval.split_whitespace();
        let amount: i64 = parts
            .next()
            .context("missing log retention interval")?
            .parse()?;
        let unit = parts
            .next()
            .context("missing log retention interval unit")?
            .to_ascii_lowercase();
        let unit = match unit.trim_end_matches('s') {
            "second" => "SECOND",
            "minute" => "MINUTE",
            "hour" => "HOUR",
            "day" => "DAY",
            "week" => "WEEK",
            "month" => "MONTH",
            "year" => "YEAR",
            _ => anyhow::bail!("unsupported log retention interval unit"),
        };
        anyhow::ensure!(
            amount >= 0 && parts.next().is_none(),
            "invalid log retention interval"
        );
        let sql = format!(
            "DELETE FROM request_logs WHERE created_at < UNIX_TIMESTAMP(NOW() - INTERVAL ? {unit}) * 1000"
        );
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(&sql).bind(amount).execute(&mut *tx).await?;
        mysql_cleanup_orphan_request_results(&mut tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn clear_all(&self) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("DELETE FROM request_logs")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM request_results")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn clear_payloads(&self) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE request_logs SET \
             client_request_headers = NULL, client_request_body = NULL, \
             client_response_headers = NULL, client_response_body = NULL, \
             upstream_request_headers = NULL, upstream_request_body = NULL, \
             upstream_response_headers = NULL, upstream_response_body = NULL, payload_cleared_at = ? \
             WHERE (client_request_headers IS NOT NULL OR client_request_body IS NOT NULL \
                OR client_response_headers IS NOT NULL OR client_response_body IS NOT NULL \
                OR upstream_request_headers IS NOT NULL OR upstream_request_body IS NOT NULL \
                OR upstream_response_headers IS NOT NULL OR upstream_response_body IS NOT NULL)",
        )
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn delete_by_id(&self, id: &str) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("DELETE FROM request_logs WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        mysql_cleanup_orphan_request_results(&mut tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn clear_errors(&self) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(&format!("DELETE FROM request_logs WHERE {}", error_sql("")))
            .execute(&mut *tx)
            .await?;
        mysql_cleanup_orphan_request_results(&mut tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        let error = error_sql("");
        let sql = if let Some(hours) = hours {
            format!(
                "SELECT COUNT(*) AS total_requests, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count FROM request_logs WHERE created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL {hours} HOUR) * 1000"
            )
        } else {
            format!(
                "SELECT COUNT(*) AS total_requests, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count FROM request_logs"
            )
        };
        Ok(sqlx::query_as::<_, StatsOverview>(&sql)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
        let error = error_sql("");
        let sql = format!(
            "SELECT DATE_FORMAT(FROM_UNIXTIME(created_at/1000), '%Y-%m-%d %H:00:00') AS hour, COUNT(*) AS request_count, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms FROM request_logs WHERE created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL {hours} HOUR) * 1000 GROUP BY hour ORDER BY hour ASC"
        );
        Ok(sqlx::query_as::<_, StatsHourly>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn stats_time_buckets(
        &self,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
        upstream_model: Option<&str>,
    ) -> anyhow::Result<Vec<StatsTimeBucket>> {
        anyhow::ensure!(bucket_ms > 0, "stats bucket must be positive");
        let error = error_sql("");
        Ok(sqlx::query_as::<_, StatsTimeBucket>(
            &format!("SELECT CAST(FLOOR(created_at / ?) * ? AS SIGNED) AS bucket_start, COUNT(*) AS request_count, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(AVG(latency_total_ms) AS DOUBLE) AS avg_duration_ms FROM request_logs WHERE created_at >= ? AND created_at <= ? AND (? IS NULL OR upstream_model = ?) GROUP BY bucket_start ORDER BY bucket_start ASC"),
        )
        .bind(bucket_ms)
        .bind(bucket_ms)
        .bind(start_ms)
        .bind(end_ms)
        .bind(upstream_model)
        .bind(upstream_model)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn api_key_model_time_buckets(
        &self,
        api_key_id: &str,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
    ) -> anyhow::Result<Vec<ModelTimeBucket>> {
        anyhow::ensure!(bucket_ms > 0, "stats bucket must be positive");
        let error = error_sql("");
        Ok(sqlx::query_as::<_, ModelTimeBucket>(
            &format!("SELECT COALESCE(upstream_model, '') AS upstream_model, CAST(FLOOR(created_at / ?) * ? AS SIGNED) AS bucket_start, COUNT(*) AS request_count, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(AVG(latency_total_ms) AS DOUBLE) AS avg_duration_ms FROM request_logs WHERE api_key_id = ? AND created_at >= ? AND created_at <= ? GROUP BY upstream_model, bucket_start ORDER BY upstream_model ASC, bucket_start ASC"),
        )
        .bind(bucket_ms)
        .bind(bucket_ms)
        .bind(api_key_id)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>> {
        let sql = if let Some(hours) = hours {
            format!(
                "SELECT upstream_model AS model, COUNT(*) AS request_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms FROM request_logs WHERE created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL {hours} HOUR) * 1000 GROUP BY upstream_model ORDER BY request_count DESC"
            )
        } else {
            "SELECT upstream_model AS model, COUNT(*) AS request_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms FROM request_logs GROUP BY upstream_model ORDER BY request_count DESC".to_string()
        };
        Ok(sqlx::query_as::<_, ModelStats>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn model_usage_stats(
        &self,
        provider_id: &str,
        upstream_model: &str,
    ) -> anyhow::Result<ModelUsageStats> {
        let totals = sqlx::query_as::<_, ModelUsageTotals>(
            "SELECT COUNT(*) AS request_count, \
             CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, \
             CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, \
             CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, \
             CAST(MAX(created_at) AS SIGNED) AS last_called_at \
             FROM request_logs WHERE provider_id = ? AND BINARY upstream_model = ?",
        )
        .bind(provider_id)
        .bind(upstream_model)
        .fetch_one(&self.pool)
        .await?;
        let samples = sqlx::query_as::<_, RecentModelPerformance>(
            "SELECT COALESCE(output_tokens, 0) AS output_tokens, COALESCE(is_stream, 0) AS is_stream, \
             COALESCE(stream_chunks_count, 0) AS stream_chunks_count, latency_upstream_ms, latency_total_ms, stream_first_chunk_ms \
             FROM request_logs WHERE provider_id = ? AND BINARY upstream_model = ? \
             ORDER BY created_at DESC, id DESC LIMIT 10",
        )
        .bind(provider_id)
        .bind(upstream_model)
        .fetch_all(&self.pool)
        .await?;
        Ok(ModelUsageStats::from_samples(totals, &samples))
    }
    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>> {
        let error = error_sql("");
        let time_filter = hours
            .map(|hours| {
                format!(" AND created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL {hours} HOUR) * 1000")
            })
            .unwrap_or_default();
        let sql = format!(
            "WITH aggregated AS (SELECT provider_id, COUNT(*) AS request_count, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms FROM request_logs WHERE provider_id IS NOT NULL AND TRIM(provider_id) <> ''{time_filter} GROUP BY provider_id) SELECT a.provider_id, COALESCE((SELECT NULLIF(TRIM(r.provider_name), '') FROM request_logs r WHERE r.provider_id = a.provider_id AND NULLIF(TRIM(r.provider_name), '') IS NOT NULL ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.provider_id) AS provider, CAST(NULL AS CHAR) AS provider_icon, CAST(NULL AS CHAR) AS provider_protocol, a.request_count, a.error_count, a.avg_duration_ms, a.total_output_tokens, a.total_upstream_ms FROM aggregated a ORDER BY a.request_count DESC, a.provider_id ASC"
        );
        Ok(sqlx::query_as::<_, ProviderStats>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn provider_usage_detail(
        &self,
        provider_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ProviderUsageDetail> {
        #[derive(sqlx::FromRow)]
        struct SummaryRow {
            provider_name: String,
            request_count: i64,
            success_count: i64,
            error_count: i64,
            unknown_count: i64,
            cancelled_count: i64,
            output_limited_count: i64,
            total_input_tokens: i64,
            total_output_tokens: i64,
            total_cache_read_tokens: i64,
            avg_duration_ms: f64,
            avg_first_token_ms: Option<f64>,
            total_upstream_ms: f64,
            last_used_at: Option<i64>,
        }
        let outcome_counts = mysql_outcome_counts("");
        let error = error_sql("");
        let summary = sqlx::query_as::<_, SummaryRow>(&format!("SELECT COALESCE((SELECT NULLIF(TRIM(r.provider_name), '') FROM request_logs r WHERE r.provider_id = ? AND NULLIF(TRIM(r.provider_name), '') IS NOT NULL ORDER BY r.created_at DESC, r.id DESC LIMIT 1), ?) AS provider_name, COUNT(*) AS request_count, {outcome_counts}, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms, CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms, CAST(MAX(created_at) AS SIGNED) AS last_used_at FROM request_logs WHERE provider_id = ? AND created_at >= ? AND created_at <= ?")).bind(provider_id).bind(provider_id).bind(provider_id).bind(start_at).bind(end_at).fetch_one(&self.pool).await?;
        let models = sqlx::query_as::<_, ProviderModelUsageStats>(&format!("SELECT COALESCE(upstream_model, '') AS upstream_model, COUNT(*) AS request_count, CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms, CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms, CAST(MAX(created_at) AS SIGNED) AS last_used_at FROM request_logs WHERE provider_id = ? AND created_at >= ? AND created_at <= ? GROUP BY COALESCE(upstream_model, '') ORDER BY request_count DESC, upstream_model ASC")).bind(provider_id).bind(start_at).bind(end_at).fetch_all(&self.pool).await?;
        Ok(ProviderUsageDetail {
            start_at,
            end_at,
            provider_id: provider_id.to_string(),
            provider_name: summary.provider_name,
            provider_icon: None,
            provider_protocol: None,
            request_count: summary.request_count,
            success_count: summary.success_count,
            error_count: summary.error_count,
            unknown_count: summary.unknown_count,
            cancelled_count: summary.cancelled_count,
            output_limited_count: summary.output_limited_count,
            outcome_stats_version: 1,
            total_input_tokens: summary.total_input_tokens,
            total_output_tokens: summary.total_output_tokens,
            total_cache_read_tokens: summary.total_cache_read_tokens,
            avg_duration_ms: summary.avg_duration_ms,
            avg_first_token_ms: summary.avg_first_token_ms,
            total_upstream_ms: summary.total_upstream_ms,
            last_used_at: summary.last_used_at,
            models,
        })
    }

    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>> {
        let error = error_sql("l");
        let time_filter = hours
            .map(|hours| {
                format!(" AND created_at >= UNIX_TIMESTAMP(NOW() - INTERVAL {hours} HOUR) * 1000")
            })
            .unwrap_or_default();
        let sql = format!(
            "SELECT COALESCE(l.api_key_id, '') AS api_key_id, \
             COALESCE((SELECT NULLIF(r.api_key_name, '') FROM request_logs r \
                       WHERE r.api_key_id = l.api_key_id \
                       ORDER BY r.created_at DESC, r.id DESC LIMIT 1), l.api_key_id, '') AS api_key_name, \
             COUNT(*) AS request_count, \
             CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, \
             CAST(COALESCE(SUM(l.input_tokens), 0) AS SIGNED) AS total_input_tokens, \
             CAST(COALESCE(SUM(l.output_tokens), 0) AS SIGNED) AS total_output_tokens, \
             CAST(COALESCE(SUM(l.cache_read_tokens), 0) AS SIGNED) AS cache_read_tokens, \
             MAX(l.created_at) AS last_used_at \
             FROM request_logs l WHERE l.api_key_id IS NOT NULL AND l.api_key_id <> ''{time_filter} \
             GROUP BY l.api_key_id ORDER BY request_count DESC, api_key_id ASC"
        );
        Ok(sqlx::query_as::<_, ApiKeyStats>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn api_key_usage_detail(
        &self,
        api_key_id: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ApiKeyUsageDetail> {
        #[derive(sqlx::FromRow)]
        struct SummaryRow {
            api_key_name: String,
            request_count: i64,
            success_count: i64,
            error_count: i64,
            unknown_count: i64,
            cancelled_count: i64,
            output_limited_count: i64,
            total_input_tokens: i64,
            total_output_tokens: i64,
            total_cache_read_tokens: i64,
            avg_duration_ms: f64,
            avg_first_token_ms: Option<f64>,
            last_used_at: Option<i64>,
        }

        let outcome_counts = mysql_outcome_counts("");
        let error = error_sql("");
        let summary = sqlx::query_as::<_, SummaryRow>(
            &format!("SELECT COALESCE((SELECT NULLIF(r.api_key_name, '') FROM request_logs r \
                              WHERE r.api_key_id = ? \
                              ORDER BY r.created_at DESC, r.id DESC LIMIT 1), ?) AS api_key_name, \
             COUNT(*) AS request_count, \
             {outcome_counts}, \
             CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, \
             CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, \
             CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, \
             CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, \
             CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms, \
             CAST(MAX(created_at) AS SIGNED) AS last_used_at \
             FROM request_logs WHERE api_key_id = ? AND created_at >= ? AND created_at <= ?"),
        )
        .bind(api_key_id)
        .bind(api_key_id)
        .bind(api_key_id)
        .bind(start_at)
        .bind(end_at)
        .fetch_one(&self.pool)
        .await?;

        let model_routes = sqlx::query_as::<_, ApiKeyModelRouteStats>(
            &format!("WITH grouped AS (SELECT COALESCE(client_model, '') AS client_model, \
             COALESCE(provider_id, '') AS provider_id, \
             COALESCE(upstream_model, '') AS upstream_model, \
             COUNT(*) AS request_count, \
             CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, \
             CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens, \
             CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens, \
             CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens, \
             CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms, \
             CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms, \
             CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms \
             FROM request_logs WHERE api_key_id = ? AND created_at >= ? AND created_at <= ? \
             GROUP BY COALESCE(client_model, ''), COALESCE(provider_id, ''), COALESCE(upstream_model, '')), \
             latest_provider AS (SELECT COALESCE(provider_id, '') AS provider_id, \
             COALESCE(NULLIF(provider_name, ''), provider_id, '') AS provider_name, \
             ROW_NUMBER() OVER (PARTITION BY COALESCE(provider_id, '') ORDER BY created_at DESC, id DESC) AS row_num \
              FROM request_logs WHERE COALESCE(provider_id, '') IN (SELECT provider_id FROM grouped)) \
             SELECT g.client_model, g.provider_id, COALESCE(p.provider_name, g.provider_id, '') AS provider_name, \
             g.upstream_model, g.request_count, g.error_count, g.total_input_tokens, g.total_output_tokens, \
             g.total_cache_read_tokens, g.avg_duration_ms, g.avg_first_token_ms, g.total_upstream_ms \
             FROM grouped g LEFT JOIN latest_provider p ON p.provider_id = g.provider_id AND p.row_num = 1 \
             ORDER BY g.request_count DESC, g.client_model ASC, g.provider_id ASC, g.upstream_model ASC"),
        )
        .bind(api_key_id)
        .bind(start_at)
        .bind(end_at)
        .fetch_all(&self.pool)
        .await?;

        Ok(ApiKeyUsageDetail {
            start_at,
            end_at,
            api_key_id: api_key_id.to_string(),
            api_key_name: summary.api_key_name,
            request_count: summary.request_count,
            success_count: summary.success_count,
            error_count: summary.error_count,
            unknown_count: summary.unknown_count,
            cancelled_count: summary.cancelled_count,
            output_limited_count: summary.output_limited_count,
            outcome_stats_version: 1,
            total_input_tokens: summary.total_input_tokens,
            total_output_tokens: summary.total_output_tokens,
            total_cache_read_tokens: summary.total_cache_read_tokens,
            avg_duration_ms: summary.avg_duration_ms,
            avg_first_token_ms: summary.avg_first_token_ms,
            last_used_at: summary.last_used_at,
            model_routes,
            model_time_series: Vec::new(),
        })
    }

    async fn model_usage_detail(
        &self,
        upstream_model: &str,
        start_at: i64,
        end_at: i64,
    ) -> anyhow::Result<ModelUsageDetail> {
        #[derive(sqlx::FromRow)]
        struct SummaryRow {
            request_count: i64,
            success_count: i64,
            error_count: i64,
            unknown_count: i64,
            cancelled_count: i64,
            output_limited_count: i64,
            total_input_tokens: i64,
            total_output_tokens: i64,
            total_cache_read_tokens: i64,
            avg_duration_ms: f64,
            avg_first_token_ms: Option<f64>,
            total_upstream_ms: f64,
            last_used_at: Option<i64>,
        }
        let outcome_counts = mysql_outcome_counts("");
        let error = error_sql("");
        let summary = sqlx::query_as::<_, SummaryRow>(
            &format!("SELECT COUNT(*) AS request_count, {outcome_counts},              CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens,              CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens,              CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens,              CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms,              CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms,              CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms,              CAST(MAX(created_at) AS SIGNED) AS last_used_at              FROM request_logs WHERE upstream_model = ? AND created_at >= ? AND created_at <= ?"),
        )
        .bind(upstream_model)
        .bind(start_at)
        .bind(end_at)
        .fetch_one(&self.pool)
        .await?;
        let providers = sqlx::query_as::<_, ModelProviderUsageStats>(
            &format!("WITH aggregated AS (SELECT provider_id, COUNT(*) AS request_count,              CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count,              CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens,              CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens,              CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens,              CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms,              CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms,              CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms,              CAST(MAX(created_at) AS SIGNED) AS last_used_at              FROM request_logs WHERE upstream_model = ? AND provider_id IS NOT NULL AND TRIM(provider_id) <> ''              AND created_at >= ? AND created_at <= ? GROUP BY provider_id)              SELECT a.provider_id, COALESCE((SELECT NULLIF(TRIM(r.provider_name), '') FROM request_logs r              WHERE r.provider_id = a.provider_id AND NULLIF(TRIM(r.provider_name), '') IS NOT NULL              ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.provider_id) AS provider_name,              CAST(NULL AS CHAR) AS provider_icon, CAST(NULL AS CHAR) AS provider_protocol,              a.request_count, a.error_count, a.total_input_tokens, a.total_output_tokens,              a.total_cache_read_tokens, a.avg_duration_ms, a.avg_first_token_ms, a.total_upstream_ms,              a.last_used_at FROM aggregated a ORDER BY a.request_count DESC, a.provider_id ASC"),
        )
        .bind(upstream_model)
        .bind(start_at)
        .bind(end_at)
        .fetch_all(&self.pool)
        .await?;
        let api_keys = sqlx::query_as::<_, ModelApiKeyUsageStats>(
            &format!("WITH aggregated AS (SELECT api_key_id, COUNT(*) AS request_count,              CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count,              CAST(COALESCE(SUM(input_tokens), 0) AS SIGNED) AS total_input_tokens,              CAST(COALESCE(SUM(output_tokens), 0) AS SIGNED) AS total_output_tokens,              CAST(COALESCE(SUM(cache_read_tokens), 0) AS SIGNED) AS total_cache_read_tokens,              CAST(COALESCE(AVG(latency_total_ms), 0) AS DOUBLE) AS avg_duration_ms,              CAST(AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END) AS DOUBLE) AS avg_first_token_ms,              CAST(COALESCE(SUM(latency_upstream_ms), 0) AS DOUBLE) AS total_upstream_ms,              CAST(MAX(created_at) AS SIGNED) AS last_used_at              FROM request_logs WHERE upstream_model = ? AND api_key_id IS NOT NULL AND api_key_id <> ''              AND created_at >= ? AND created_at <= ? GROUP BY api_key_id)              SELECT a.api_key_id, COALESCE((SELECT NULLIF(r.api_key_name, '') FROM request_logs r              WHERE r.api_key_id = a.api_key_id ORDER BY r.created_at DESC, r.id DESC LIMIT 1),              a.api_key_id) AS api_key_name,              a.request_count, a.error_count, a.total_input_tokens, a.total_output_tokens,              a.total_cache_read_tokens, a.avg_duration_ms, a.avg_first_token_ms, a.total_upstream_ms,              a.last_used_at FROM aggregated a ORDER BY a.request_count DESC, a.api_key_id ASC"),
        )
        .bind(upstream_model)
        .bind(start_at)
        .bind(end_at)
        .fetch_all(&self.pool)
        .await?;
        Ok(ModelUsageDetail {
            start_at,
            end_at,
            upstream_model: upstream_model.to_string(),
            request_count: summary.request_count,
            success_count: summary.success_count,
            error_count: summary.error_count,
            unknown_count: summary.unknown_count,
            cancelled_count: summary.cancelled_count,
            output_limited_count: summary.output_limited_count,
            outcome_stats_version: 1,
            total_input_tokens: summary.total_input_tokens,
            total_output_tokens: summary.total_output_tokens,
            total_cache_read_tokens: summary.total_cache_read_tokens,
            avg_duration_ms: summary.avg_duration_ms,
            avg_first_token_ms: summary.avg_first_token_ms,
            total_upstream_ms: summary.total_upstream_ms,
            last_used_at: summary.last_used_at,
            providers,
            api_keys,
            time_series: None,
        })
    }
}

fn mysql_outcome_counts(alias: &str) -> String {
    let success = outcome_sql(alias, "completed");
    let error = error_sql(alias);
    let unknown = outcome_sql(alias, "unknown");
    let cancelled = outcome_sql(alias, "cancelled");
    let output_limited = outcome_sql(alias, "output_limited");
    format!(
        "CAST(COALESCE(SUM(CASE WHEN {success} THEN 1 ELSE 0 END), 0) AS SIGNED) AS success_count, \
         CAST(COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS SIGNED) AS error_count, \
         CAST(COALESCE(SUM(CASE WHEN {unknown} THEN 1 ELSE 0 END), 0) AS SIGNED) AS unknown_count, \
         CAST(COALESCE(SUM(CASE WHEN {cancelled} THEN 1 ELSE 0 END), 0) AS SIGNED) AS cancelled_count, \
         CAST(COALESCE(SUM(CASE WHEN {output_limited} THEN 1 ELSE 0 END), 0) AS SIGNED) AS output_limited_count"
    )
}

async fn mysql_cleanup_orphan_request_results(
    tx: &mut sqlx::Transaction<'_, MySql>,
) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM request_results WHERE NOT EXISTS (SELECT 1 FROM request_logs WHERE request_logs.client_request_id = request_results.client_request_id)",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MysqlBootstrap {
    adapter: MysqlAdapter,
}

#[async_trait]
impl StorageBootstrap for MysqlBootstrap {
    async fn init(&self) -> anyhow::Result<()> {
        self.adapter.ping().await
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        let pool = self.adapter.pool();

        sqlx::raw_sql(MYSQL_INIT_SQL).execute(pool).await?;
        // MySQL has no CREATE INDEX IF NOT EXISTS. Guard existing indexes so
        // upgrades (including creation of the ratings table) can finish.
        for (table, column, index) in [
            ("route_targets", "route_id", "idx_route_targets_route_id"),
            ("request_logs", "created_at", "idx_logs_created_at"),
            ("request_logs", "provider_id", "idx_logs_provider_id"),
            (
                "request_logs",
                "client_status_code",
                "idx_logs_client_status",
            ),
            ("request_logs", "upstream_model", "idx_logs_upstream_model"),
            ("request_logs", "api_key_id", "idx_logs_api_key"),
            ("api_keys", "token", "idx_api_keys_token"),
            ("api_key_routes", "route_id", "idx_api_key_routes_route_id"),
            (
                "provider_oauth_credentials",
                "status",
                "idx_oauth_creds_status",
            ),
            (
                "provider_oauth_credentials",
                "expires_at",
                "idx_oauth_creds_expires",
            ),
        ] {
            mysql_ensure_index(pool, table, column, index).await?;
        }

        // Add balance column to routes
        mysql_add_column_if_not_exists(
            pool,
            "routes",
            "balance",
            "VARCHAR(255) DEFAULT 'weighted'",
        )
        .await?;
        sqlx::query(
            "UPDATE routes SET balance = 'weighted' WHERE balance IS NULL OR TRIM(balance) = ''",
        )
        .execute(pool)
        .await?;

        // Add use_proxy to providers
        mysql_add_column_if_not_exists(
            pool,
            "providers",
            "use_proxy",
            "TINYINT(1) NOT NULL DEFAULT 0",
        )
        .await?;
        mysql_add_column_if_not_exists(
            pool,
            "providers",
            "fast_mode",
            "TINYINT(1) NOT NULL DEFAULT 0",
        )
        .await?;
        mysql_add_column_if_not_exists(
            pool,
            "providers",
            "protocol_mode",
            "VARCHAR(32) NOT NULL DEFAULT 'fixed'",
        )
        .await?;

        // Add auth_mode to providers
        mysql_add_column_if_not_exists(
            pool,
            "providers",
            "auth_mode",
            "VARCHAR(255) NOT NULL DEFAULT 'apikey'",
        )
        .await?;

        // Add OAuth columns to providers
        mysql_add_column_if_not_exists(pool, "providers", "access_token", "TEXT").await?;
        mysql_add_column_if_not_exists(pool, "providers", "refresh_token", "TEXT").await?;
        mysql_add_column_if_not_exists(pool, "providers", "expires_at", "DATETIME").await?;

        // Auth mode constraint: update api_key → apikey
        sqlx::query("UPDATE providers SET auth_mode = 'apikey' WHERE auth_mode = 'api_key'")
            .execute(pool)
            .await?;

        // Collapse provider protocol columns (MySQL variant)
        migrate_collapse_provider_protocol_columns_mysql(pool).await?;
        sqlx::query(
            "INSERT INTO provider_protocol_endpoints \
             (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) \
             SELECT UUID(), p.id, p.protocol, p.base_url, p.api_key, 'auto', 1, 0 \
             FROM providers p \
             WHERE NOT EXISTS (\
                 SELECT 1 FROM provider_protocol_endpoints e WHERE e.provider_id = p.id\
             )",
        )
        .execute(pool)
        .await?;

        // Backfill route_targets
        sqlx::query(
            r#"
            INSERT IGNORE INTO route_targets (id, route_id, provider_id, model, weight, priority)
            SELECT UUID(), r.id, r.target_provider, r.target_model, 100, 1
            FROM routes r
            WHERE r.target_provider IS NOT NULL
              AND TRIM(r.target_provider) != ''
              AND NOT EXISTS (SELECT 1 FROM route_targets rt WHERE rt.route_id = r.id)
            "#,
        )
        .execute(pool)
        .await?;

        // Migrate: providers/routes is_active -> is_enabled
        mysql_add_column_if_not_exists(pool, "providers", "is_enabled", "TINYINT(1) DEFAULT 1")
            .await?;
        sqlx::query("UPDATE providers SET is_enabled = is_active WHERE is_active IS NOT NULL AND is_enabled <> is_active")
            .execute(pool)
            .await
            .ok();

        mysql_add_column_if_not_exists(pool, "routes", "is_enabled", "TINYINT(1) DEFAULT 1")
            .await?;
        sqlx::query("UPDATE routes SET is_enabled = is_active WHERE is_active IS NOT NULL AND is_enabled <> is_active")
            .execute(pool)
            .await
            .ok();

        // Migrate: api_keys status -> is_enabled
        mysql_add_column_if_not_exists(pool, "api_keys", "is_enabled", "TINYINT(1) DEFAULT 1")
            .await?;
        sqlx::query(
            "UPDATE api_keys SET is_enabled = CASE WHEN status = 'active' THEN 1 ELSE 0 END \
             WHERE status IS NOT NULL AND is_enabled <> (status = 'active')",
        )
        .execute(pool)
        .await
        .ok();

        // Migrate OAuth credentials from providers table to new dedicated table
        sqlx::query(
            r#"
            INSERT IGNORE INTO provider_oauth_credentials
                (provider_id, access_token, refresh_token, expires_at, status)
            SELECT id, COALESCE(access_token, ''), refresh_token, expires_at, 'connected'
            FROM providers
            WHERE auth_mode = 'oauth'
              AND (
                (access_token IS NOT NULL AND TRIM(access_token) != '')
                OR (refresh_token IS NOT NULL AND TRIM(refresh_token) != '')
              )
            "#,
        )
        .execute(pool)
        .await?;

        // Vendor name migrations
        for (from, to) in [("nyro", "custom"), ("zhipu", "zhipuai")] {
            sqlx::query("UPDATE providers SET vendor = ? WHERE LOWER(TRIM(vendor)) = ?")
                .bind(to)
                .bind(from)
                .execute(pool)
                .await?;
            sqlx::query("UPDATE providers SET preset_key = ? WHERE LOWER(TRIM(preset_key)) = ?")
                .bind(to)
                .bind(from)
                .execute(pool)
                .await?;
        }

        // Protocol normalization (MySQL variant)
        normalize_provider_protocols_mysql(pool).await?;

        // Drop route_type column
        if mysql_column_exists(pool, "routes", "route_type").await? {
            sqlx::query("ALTER TABLE routes DROP COLUMN route_type")
                .execute(pool)
                .await?;
        }

        // Add cache_read_tokens
        mysql_add_column_if_not_exists(
            pool,
            "request_logs",
            "cache_read_tokens",
            "INTEGER DEFAULT 0",
        )
        .await?;
        mysql_add_column_if_not_exists(pool, "request_logs", "reasoning_effort", "VARCHAR(64)")
            .await?;
        mysql_add_column_if_not_exists(pool, "request_logs", "route_decision", "LONGTEXT").await?;

        // Rename tables: routes → models, route_targets → model_backends, api_key_routes → api_key_models
        mysql_rename_table_if_needed(pool, "routes", "models").await?;
        mysql_rename_table_if_needed(pool, "route_targets", "model_backends").await?;
        mysql_rename_table_if_needed(pool, "api_key_routes", "api_key_models").await?;

        // Rename columns within renamed tables
        mysql_rename_column_if_needed(pool, "model_backends", "route_id", "model_id").await?;
        mysql_rename_column_if_needed(pool, "api_key_models", "route_id", "model_id").await?;

        // Rename columns in request_logs: route_id → model_id, route_name → model_name
        mysql_rename_column_if_needed(pool, "request_logs", "route_id", "model_id").await?;
        mysql_rename_column_if_needed(pool, "request_logs", "route_name", "model_name").await?;

        // Rename column: models strategy → balance
        mysql_rename_column_if_needed(pool, "models", "strategy", "balance").await?;

        // Add vision_shim column to models table (multimodal facade config JSON)
        mysql_add_column_if_not_exists(pool, "models", "vision_shim", "TEXT").await?;

        // Add force_max_reasoning column to models table (max-reasoning override)
        mysql_add_column_if_not_exists(
            pool,
            "models",
            "force_max_reasoning",
            "BOOLEAN NOT NULL DEFAULT 0",
        )
        .await?;

        // Add is_privileged column to api_keys (binding-check bypass flag)
        mysql_add_column_if_not_exists(
            pool,
            "api_keys",
            "is_privileged",
            "TINYINT(1) NOT NULL DEFAULT 0",
        )
        .await?;

        // Add is_fallback column to model_backends (last-resort degraded fallback)
        mysql_add_column_if_not_exists(
            pool,
            "model_backends",
            "is_fallback",
            "TINYINT(1) NOT NULL DEFAULT 0",
        )
        .await?;

        // Merge virtual_model into name and drop the column
        if mysql_column_exists(pool, "models", "virtual_model").await? {
            tracing::info!("merging virtual_model into name on models table (mysql)");
            sqlx::query(
                "UPDATE models SET name = TRIM(virtual_model)
                 WHERE virtual_model IS NOT NULL AND TRIM(virtual_model) != ''",
            )
            .execute(pool)
            .await?;
            sqlx::query("ALTER TABLE models DROP COLUMN virtual_model")
                .execute(pool)
                .await?;
        }

        // Rename access_control → enable_auth on models table
        mysql_rename_column_if_needed(pool, "models", "access_control", "enable_auth").await?;

        mysql_add_column_if_not_exists(pool, "models", "enable_payload", "TINYINT(1) DEFAULT NULL")
            .await?;

        // Rename settings key log_record_payloads → enable_payload
        sqlx::query(
            "UPDATE settings SET name = 'enable_payload' WHERE name = 'log_record_payloads'",
        )
        .execute(pool)
        .await
        .ok();

        // Rename columns for compat: settings.key → settings.name, api_keys.key → api_keys.token
        mysql_rename_column_if_needed(pool, "settings", "key", "name").await?;
        mysql_rename_column_if_needed(pool, "api_keys", "key", "token").await?;
        migrate_performance_mysql(pool).await?;
        migrate_diagnostics_mysql(pool).await?;
        // Provider-scoped ratings were replaced by prefix ratings; old rows are
        // deliberately dropped instead of migrated.
        sqlx::query("DROP TABLE IF EXISTS provider_model_ratings")
            .execute(pool)
            .await?;
        crate::db::model_performance::recover_historical_metadata!(
            pool,
            sqlx::MySql,
            "OCTET_LENGTH(upstream_request_body)"
        );

        Ok(())
    }

    async fn health(&self) -> anyhow::Result<StorageHealth> {
        let health = self.adapter.health().await;
        Ok(StorageHealth {
            backend: StorageBackend::Mysql,
            can_connect: health.can_connect,
            schema_compatible: health.schema_compatible,
            writable: health.can_connect,
        })
    }
}

// ---------------------------------------------------------------------------
// Migration helpers
// ---------------------------------------------------------------------------

async fn migrate_diagnostics_mysql(pool: &Pool<MySql>) -> anyhow::Result<()> {
    // Historical rows remain version 0/unknown: performance metadata is not an outcome.
    for (column, definition) in [
        (
            "client_request_id",
            "VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin",
        ),
        ("attempt_index", "INTEGER"),
        ("outcome_version", "INTEGER NOT NULL DEFAULT 0"),
        (
            "attempt_outcome",
            "VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT 'unknown'",
        ),
        ("failure_kind", "VARCHAR(64)"),
        ("failure_stage", "VARCHAR(64)"),
        ("error_message", "TEXT"),
        ("error_causes_json", "TEXT"),
        ("payload_metadata_json", "TEXT"),
        ("payload_cleared_at", "BIGINT"),
    ] {
        mysql_add_column_if_not_exists(pool, "request_logs", column, definition).await?;
    }
    // request_results is created by MYSQL_INIT_SQL for both fresh and existing databases.
    mysql_create_index_if_not_exists(
        pool,
        "request_logs",
        "idx_logs_client_request_attempt",
        "client_request_id, attempt_index",
    )
    .await?;
    Ok(())
}

async fn migrate_performance_mysql(pool: &Pool<MySql>) -> anyhow::Result<()> {
    for (column, definition) in [
        ("performance_metadata_version", "INTEGER NOT NULL DEFAULT 0"),
        (
            "upstream_effort_status",
            "VARCHAR(16) NOT NULL DEFAULT 'unknown'",
        ),
        ("upstream_effort_raw", "TEXT"),
        ("upstream_effort_tier", "VARCHAR(16)"),
        (
            "request_completion",
            "VARCHAR(16) NOT NULL DEFAULT 'unknown'",
        ),
        ("completion_reason", "TEXT"),
        (
            "upstream_response_mode",
            "VARCHAR(16) NOT NULL DEFAULT 'unknown'",
        ),
        ("performance_upstream_ms", "BIGINT"),
        ("performance_first_chunk_ms", "BIGINT"),
        ("performance_completed_at", "BIGINT"),
    ] {
        mysql_add_column_if_not_exists(pool, "request_logs", column, definition).await?;
    }
    mysql_create_index_if_not_exists(
        pool,
        "request_logs",
        "idx_logs_performance_pair",
        "provider_id, upstream_model, request_completion, performance_completed_at, id",
    )
    .await?;
    mysql_create_index_if_not_exists(
        pool,
        "request_logs",
        "idx_logs_performance_recovery",
        "performance_metadata_version, created_at, id",
    )
    .await?;
    Ok(())
}

async fn mysql_column_exists(
    pool: &Pool<MySql>,
    table_name: &str,
    column_name: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
    )
    .bind(table_name)
    .bind(column_name)
    .fetch_one(pool)
    .await?
    > 0)
}

async fn mysql_table_exists(pool: &Pool<MySql>, table_name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?",
    )
    .bind(table_name)
    .fetch_one(pool)
    .await?
    > 0)
}

async fn mysql_create_index_if_not_exists(
    pool: &Pool<MySql>,
    table: &str,
    index: &str,
    columns: &str,
) -> anyhow::Result<()> {
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.statistics WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ?")
        .bind(table).bind(index).fetch_one(pool).await?;
    if exists == 0 {
        sqlx::query(&format!("CREATE INDEX `{index}` ON `{table}` ({columns})"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn mysql_ensure_index(
    pool: &Pool<MySql>,
    table: &str,
    column: &str,
    index: &str,
) -> anyhow::Result<()> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ?",
    )
    .bind(table)
    .bind(index)
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        sqlx::query(&format!("CREATE INDEX `{index}` ON `{table}` (`{column}`)"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn mysql_rename_table_if_needed(
    pool: &Pool<MySql>,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    if mysql_table_exists(pool, old).await? && !mysql_table_exists(pool, new).await? {
        tracing::info!("renaming table {old} -> {new}");
        sqlx::query(&format!("RENAME TABLE `{old}` TO `{new}`"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn mysql_rename_column_if_needed(
    pool: &Pool<MySql>,
    table: &str,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    if mysql_column_exists(pool, table, old).await?
        && !mysql_column_exists(pool, table, new).await?
    {
        tracing::info!("renaming column {table}.{old} -> {table}.{new}");
        sqlx::query(&format!(
            "ALTER TABLE `{table}` RENAME COLUMN `{old}` TO `{new}`"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn mysql_add_column_if_not_exists(
    pool: &Pool<MySql>,
    table: &str,
    column: &str,
    definition: &str,
) -> anyhow::Result<()> {
    if !mysql_column_exists(pool, table, column).await? {
        sqlx::query(&format!(
            "ALTER TABLE `{table}` ADD COLUMN `{column}` {definition}"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider protocol collapse (MySQL variant)
// ---------------------------------------------------------------------------

async fn migrate_collapse_provider_protocol_columns_mysql(
    pool: &Pool<MySql>,
) -> anyhow::Result<()> {
    let has_default_protocol = mysql_column_exists(pool, "providers", "default_protocol").await?;
    let has_protocol_endpoints =
        mysql_column_exists(pool, "providers", "protocol_endpoints").await?;
    if !has_default_protocol && !has_protocol_endpoints {
        return Ok(());
    }

    if has_default_protocol {
        sqlx::query(
            "UPDATE providers \
             SET protocol = TRIM(default_protocol) \
             WHERE default_protocol IS NOT NULL AND TRIM(default_protocol) != ''",
        )
        .execute(pool)
        .await?;
    }

    if has_protocol_endpoints {
        let rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, protocol, base_url, api_key, protocol_endpoints FROM providers",
        )
        .fetch_all(pool)
        .await?;
        for (id, protocol, base_url, api_key, raw_endpoints) in rows {
            let Some(mut legacy) = crate::db::models::normalize_legacy_provider_protocol_config(
                raw_endpoints.as_deref().unwrap_or(""),
                &protocol,
                &api_key,
            ) else {
                continue;
            };
            let effective_base_url = if base_url.trim().is_empty() {
                legacy.default_base_url().unwrap_or_default().to_string()
            } else {
                base_url.trim().to_string()
            };

            if !legacy.adaptive {
                if base_url.trim().is_empty() && !effective_base_url.is_empty() {
                    sqlx::query("UPDATE providers SET base_url = ? WHERE id = ?")
                        .bind(effective_base_url)
                        .bind(id)
                        .execute(pool)
                        .await?;
                }
                continue;
            }

            if let Some(default) = legacy
                .endpoints
                .iter_mut()
                .find(|endpoint| endpoint.protocol == legacy.default_protocol)
                && !effective_base_url.is_empty()
            {
                default.base_url = effective_base_url.clone();
            }
            for endpoint in legacy.endpoints {
                sqlx::query(
                    "INSERT IGNORE INTO provider_protocol_endpoints \
                     (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(uuid::Uuid::new_v4().to_string())
                .bind(&id)
                .bind(endpoint.protocol)
                .bind(endpoint.base_url)
                .bind(endpoint.api_key)
                .bind(endpoint.auth_scheme)
                .bind(endpoint.is_enabled)
                .bind(endpoint.priority)
                .execute(pool)
                .await?;
            }
            sqlx::query(
                "UPDATE providers SET protocol = ?, base_url = ?, protocol_mode = 'adaptive' WHERE id = ?",
            )
            .bind(legacy.default_protocol)
            .bind(effective_base_url)
            .bind(id)
            .execute(pool)
            .await?;
        }
    }

    if mysql_column_exists(pool, "providers", "protocol_endpoints").await? {
        sqlx::query("ALTER TABLE providers DROP COLUMN protocol_endpoints")
            .execute(pool)
            .await?;
    }
    if mysql_column_exists(pool, "providers", "default_protocol").await? {
        sqlx::query("ALTER TABLE providers DROP COLUMN default_protocol")
            .execute(pool)
            .await?;
    }

    Ok(())
}

/// MySQL counterpart of provider protocol normalization.
async fn normalize_provider_protocols_mysql(pool: &Pool<MySql>) -> anyhow::Result<()> {
    use crate::protocol::registry::ProtocolRegistry;

    let reg = ProtocolRegistry::global();
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, protocol, COALESCE(protocol_mode, 'fixed') FROM providers")
            .fetch_all(pool)
            .await?;

    for (id, raw_protocol, protocol_mode) in rows {
        let new_protocol = normalize_provider_protocol_value(
            reg,
            &raw_protocol,
            protocol_mode.trim() == crate::db::models::PROVIDER_PROTOCOL_MODE_ADAPTIVE,
        );
        if new_protocol == raw_protocol {
            continue;
        }

        tracing::info!(
            provider_id = %id,
            old_protocol = %raw_protocol,
            new_protocol = %new_protocol,
            "normalizing provider protocol identifier (mysql)"
        );

        sqlx::query("UPDATE providers SET protocol = ? WHERE id = ?")
            .bind(&new_protocol)
            .bind(&id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

fn normalize_provider_protocol_value(
    reg: &crate::protocol::registry::ProtocolRegistry,
    raw: &str,
    adaptive: bool,
) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if adaptive {
        return reg
            .resolve_alias(trimmed)
            .map(|endpoint| endpoint.to_string())
            .unwrap_or_else(|| {
                tracing::warn!(
                    value = trimmed,
                    "leaving unrecognized adaptive provider endpoint unchanged (mysql)"
                );
                trimmed.to_string()
            });
    }
    match reg.parse_protocol(trimmed) {
        Some(protocol) => protocol.as_str().to_string(),
        None => {
            tracing::warn!(
                value = trimmed,
                "leaving unrecognized provider protocol identifier unchanged (mysql)"
            );
            trimmed.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Select helpers
// ---------------------------------------------------------------------------

fn provider_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, name, vendor, protocol, base_url, COALESCE(protocol_mode, 'fixed') AS protocol_mode, preset_key, channel, models_source, static_models, api_key, COALESCE(auth_mode, 'apikey') AS auth_mode, COALESCE(use_proxy, 0) AS use_proxy, COALESCE(fast_mode, 0) AS fast_mode, last_test_success, DATE_FORMAT(last_test_at, '%Y-%m-%d %H:%i:%S') AS last_test_at, COALESCE(is_enabled, 1) AS is_enabled, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at, DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%S') AS updated_at FROM providers",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    } else {
        sql.push_str(" ORDER BY created_at DESC");
    }
    sql
}

fn model_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, name, COALESCE(balance, 'weighted') AS balance, target_provider, target_model, COALESCE(enable_auth, 0) AS enable_auth, enable_payload, vision_shim, COALESCE(force_max_reasoning, 0) AS force_max_reasoning, COALESCE(is_enabled, 1) AS is_enabled, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at FROM models",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    }
    sql
}

fn api_key_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, token, name, rpm, rpd, tpm, tpd, COALESCE(is_enabled, 1) AS is_enabled, COALESCE(is_privileged, 0) AS is_privileged, DATE_FORMAT(expires_at, '%Y-%m-%d %H:%i:%S') AS expires_at, DATE_FORMAT(created_at, '%Y-%m-%d %H:%i:%S') AS created_at, DATE_FORMAT(updated_at, '%Y-%m-%d %H:%i:%S') AS updated_at FROM api_keys",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    } else {
        sql.push_str(" ORDER BY created_at DESC");
    }
    sql
}

fn api_key_with_bindings(row: ApiKey, model_ids: Vec<String>) -> ApiKeyWithBindings {
    ApiKeyWithBindings {
        id: row.id,
        token: row.token,
        name: row.name,
        rpm: row.rpm,
        rpd: row.rpd,
        tpm: row.tpm,
        tpd: row.tpd,
        is_enabled: row.is_enabled,
        is_privileged: row.is_privileged,
        expires_at: row.expires_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
        model_ids,
    }
}

fn normalize_provider_vendor(vendor: Option<&str>) -> Option<String> {
    vendor
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "custom")
        .map(|v| v.to_lowercase())
}

async fn list_api_key_model_ids(
    pool: &Pool<MySql>,
    api_key_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT model_id FROM api_key_models WHERE api_key_id = ? ORDER BY model_id ASC",
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await?)
}

async fn replace_api_key_models(
    pool: &Pool<MySql>,
    api_key_id: &str,
    model_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = ?")
        .bind(api_key_id)
        .execute(&mut *tx)
        .await?;

    for model_id in model_ids.iter().filter(|id| !id.trim().is_empty()) {
        sqlx::query("INSERT IGNORE INTO api_key_models (api_key_id, model_id) VALUES (?, ?)")
            .bind(api_key_id)
            .bind(model_id.trim())
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

const MYSQL_INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS providers (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    vendor VARCHAR(255),
    protocol VARCHAR(255) NOT NULL,
    base_url TEXT NOT NULL,
    protocol_mode VARCHAR(32) NOT NULL DEFAULT 'fixed',
    preset_key VARCHAR(255),
    channel VARCHAR(255),
    models_source TEXT,
    static_models TEXT,
    api_key TEXT NOT NULL,
    auth_mode VARCHAR(255) NOT NULL DEFAULT 'apikey',
    access_token TEXT,
    refresh_token TEXT,
    expires_at DATETIME,
    use_proxy TINYINT(1) NOT NULL DEFAULT 0,
    fast_mode TINYINT(1) NOT NULL DEFAULT 0,
    last_test_success TINYINT(1),
    last_test_at DATETIME,
    is_enabled TINYINT(1) DEFAULT 1,
    priority INTEGER DEFAULT 0,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS model_rating_prefixes (
    model_prefix VARBINARY(1024) NOT NULL
        CHECK (OCTET_LENGTH(model_prefix) BETWEEN 1 AND 1024),
    score INTEGER NOT NULL CHECK (score BETWEEN 0 AND 100),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (model_prefix)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS provider_protocol_endpoints (
    id VARCHAR(36) PRIMARY KEY,
    provider_id VARCHAR(36) NOT NULL,
    protocol VARCHAR(255) NOT NULL,
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    auth_scheme VARCHAR(32) NOT NULL DEFAULT 'auto',
    is_enabled TINYINT(1) NOT NULL DEFAULT 1,
    priority INTEGER NOT NULL DEFAULT 0,
    test_status VARCHAR(32) NOT NULL DEFAULT 'untested',
    test_error TEXT,
    tested_at DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uq_provider_protocol_endpoints_provider_protocol (provider_id, protocol),
    KEY idx_provider_protocol_endpoints_provider (provider_id, is_enabled, priority),
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS routes (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    balance VARCHAR(255) DEFAULT 'weighted',
    target_provider VARCHAR(36) NOT NULL,
    target_model VARCHAR(255) NOT NULL,
    enable_auth TINYINT(1) DEFAULT 0,
    enable_payload TINYINT(1) DEFAULT NULL,
    force_max_reasoning TINYINT(1) NOT NULL DEFAULT 0,
    vision_shim TEXT,
    is_enabled TINYINT(1) DEFAULT 1,
    priority INTEGER DEFAULT 0,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (target_provider) REFERENCES providers(id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS route_targets (
    id VARCHAR(36) PRIMARY KEY,
    route_id VARCHAR(36) NOT NULL,
    provider_id VARCHAR(36) NOT NULL,
    model VARCHAR(255) NOT NULL,
    weight INTEGER DEFAULT 100,
    priority INTEGER DEFAULT 1,
    is_fallback TINYINT(1) NOT NULL DEFAULT 0,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (route_id) REFERENCES routes(id) ON DELETE CASCADE,
    FOREIGN KEY (provider_id) REFERENCES providers(id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS request_logs (
    id                        VARCHAR(36) PRIMARY KEY,
    created_at                BIGINT NOT NULL DEFAULT 0,
    api_key_id                VARCHAR(36),
    api_key_name              VARCHAR(255),
    client_protocol           VARCHAR(255),
    upstream_protocol         VARCHAR(255),
    provider_id               VARCHAR(36),
    provider_name             VARCHAR(255),
    model_id                  VARCHAR(36),
    model_name                VARCHAR(255),
    upstream_url              TEXT,
    client_model              VARCHAR(255),
    upstream_model            VARCHAR(255),
    reasoning_effort          VARCHAR(64),
    route_decision            LONGTEXT,
    method                    VARCHAR(255),
    path                      TEXT,
    client_request_headers    TEXT,
    client_request_body       LONGTEXT,
    client_response_headers   TEXT,
    client_response_body      LONGTEXT,
    upstream_request_headers  TEXT,
    upstream_request_body     LONGTEXT,
    upstream_response_headers TEXT,
    upstream_response_body    LONGTEXT,
    upstream_status_code      INTEGER,
    client_status_code        INTEGER,
    latency_total_ms          BIGINT,
    latency_upstream_ms       BIGINT,
    input_tokens              INTEGER DEFAULT 0,
    output_tokens             INTEGER DEFAULT 0,
    cache_read_tokens         INTEGER DEFAULT 0,
    is_stream                 TINYINT(1) DEFAULT 0,
    stream_chunks_count       INTEGER DEFAULT 0,
    stream_first_chunk_ms     BIGINT,
    performance_metadata_version INTEGER NOT NULL DEFAULT 0,
    upstream_effort_status    VARCHAR(16) NOT NULL DEFAULT 'unknown',
    upstream_effort_raw       TEXT,
    upstream_effort_tier      VARCHAR(16),
    request_completion        VARCHAR(16) NOT NULL DEFAULT 'unknown',
    completion_reason         TEXT,
    upstream_response_mode    VARCHAR(16) NOT NULL DEFAULT 'unknown',
    performance_upstream_ms   BIGINT,
    performance_first_chunk_ms BIGINT,
    performance_completed_at  BIGINT,
    client_request_id         VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin,
    attempt_index             INTEGER,
    outcome_version           INTEGER NOT NULL DEFAULT 0,
    attempt_outcome           VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT 'unknown',
    failure_kind              VARCHAR(64),
    failure_stage             VARCHAR(64),
    error_message             TEXT,
    error_causes_json          TEXT,
    payload_metadata_json     TEXT,
    payload_cleared_at        BIGINT,
    KEY idx_logs_client_request_attempt (client_request_id, attempt_index)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS request_results (
    client_request_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin PRIMARY KEY,
    final_outcome VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    final_attempt_id VARCHAR(36),
    attempt_count INTEGER NOT NULL,
    finished_at BIGINT NOT NULL
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS settings (
    name VARCHAR(255) PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS api_keys (
    id VARCHAR(36) PRIMARY KEY,
    token VARCHAR(255) NOT NULL UNIQUE,
    name VARCHAR(255) NOT NULL,
    rpm INTEGER,
    rpd INTEGER,
    tpm INTEGER,
    tpd INTEGER,
    is_enabled TINYINT(1) DEFAULT 1,
    is_privileged TINYINT(1) NOT NULL DEFAULT 0,
    expires_at DATETIME,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS api_key_routes (
    api_key_id VARCHAR(36) NOT NULL,
    route_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (api_key_id, route_id),
    FOREIGN KEY (api_key_id) REFERENCES api_keys(id) ON DELETE CASCADE,
    FOREIGN KEY (route_id) REFERENCES routes(id) ON DELETE CASCADE
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS provider_oauth_credentials (
    provider_id       VARCHAR(36) PRIMARY KEY,
    driver_key        TEXT NOT NULL,
    scheme            VARCHAR(255) NOT NULL DEFAULT '',
    access_token      TEXT NOT NULL,
    refresh_token     TEXT,
    expires_at        DATETIME,
    resource_url      TEXT,
    subject_id        VARCHAR(255),
    scopes            TEXT NOT NULL,
    meta              TEXT NOT NULL,
    status            VARCHAR(255) NOT NULL DEFAULT 'connected',
    status_version    INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    last_refresh_at   DATETIME,
    created_at        DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at        DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
"#;
