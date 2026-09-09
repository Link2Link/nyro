mod model_ratings;

use crate::storage::ModelRatingStore;
use model_ratings::PostgresModelRatingStore;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use sqlx::{Pool, Postgres};
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
pub struct PostgresAdapter {
    pool: Pool<Postgres>,
    config: SqlBackendConfig,
}

#[derive(Debug, Clone)]
pub struct PostgresHealth {
    pub can_connect: bool,
    pub schema_compatible: bool,
}

impl PostgresAdapter {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let pool = RelationalPool::connect(
            crate::storage::sql::config::SqlBackendKind::Postgres,
            &config,
        )
        .await
        .context("connect postgres adapter")?;
        let pool = pool
            .as_postgres()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("relational pool kind mismatch: expected postgres"))?;
        Ok(Self { pool, config })
    }

    pub fn config(&self) -> &SqlBackendConfig {
        &self.config
    }

    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn health(&self) -> PostgresHealth {
        let can_connect = self.ping().await.is_ok();
        // Missing rating storage means this database still needs migration.
        let schema_compatible = if can_connect {
            pg_table_exists(&self.pool, "models").await.unwrap_or(false)
                && pg_table_exists(&self.pool, "model_rating_prefixes")
                    .await
                    .unwrap_or(false)
                && sqlx::query("SELECT performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at FROM request_logs LIMIT 0").execute(&self.pool).await.is_ok()
                && sqlx::query("SELECT client_request_id, attempt_index, outcome_version, attempt_outcome, failure_kind, failure_stage, error_message, error_causes_json, payload_metadata_json, payload_cleared_at FROM request_logs LIMIT 0").execute(&self.pool).await.is_ok()
                && sqlx::query("SELECT client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at FROM request_results LIMIT 0").execute(&self.pool).await.is_ok()
        } else {
            false
        };
        PostgresHealth {
            can_connect,
            schema_compatible,
        }
    }
}

#[derive(Clone)]
pub struct PostgresStorage {
    pool: Pool<Postgres>,
    provider_store: Arc<PostgresProviderStore>,
    model_rating_store: Arc<PostgresModelRatingStore>,
    model_store: Arc<PostgresModelStore>,
    model_backend_store: Arc<PostgresModelBackendStore>,
    settings_store: Arc<PostgresSettingsStore>,
    api_key_store: Arc<PostgresApiKeyStore>,
    auth_store: Arc<PostgresAuthAccessStore>,
    oauth_credential_store: Arc<PostgresOAuthCredentialStore>,
    log_store: Arc<PostgresLogStore>,
    bootstrap: Arc<PostgresBootstrap>,
}

impl PostgresStorage {
    pub async fn connect(config: SqlBackendConfig) -> anyhow::Result<Self> {
        let adapter = PostgresAdapter::connect(config).await?;
        let pool = adapter.pool().clone();
        let provider_store = Arc::new(PostgresProviderStore { pool: pool.clone() });
        let model_rating_store = Arc::new(PostgresModelRatingStore { pool: pool.clone() });
        let model_store = Arc::new(PostgresModelStore { pool: pool.clone() });
        let model_backend_store = Arc::new(PostgresModelBackendStore { pool: pool.clone() });
        let settings_store = Arc::new(PostgresSettingsStore { pool: pool.clone() });
        let api_key_store = Arc::new(PostgresApiKeyStore { pool: pool.clone() });
        let auth_store = Arc::new(PostgresAuthAccessStore { pool: pool.clone() });
        let oauth_credential_store = Arc::new(PostgresOAuthCredentialStore { pool: pool.clone() });
        let log_store = Arc::new(PostgresLogStore { pool: pool.clone() });
        let bootstrap = Arc::new(PostgresBootstrap { adapter });
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

    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }
}

impl Storage for PostgresStorage {
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

#[derive(Clone)]
struct PostgresOAuthCredentialStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl OAuthCredentialStore for PostgresOAuthCredentialStore {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>> {
        Ok(sqlx::query_as::<_, OAuthCredential>(
            "SELECT provider_id, driver_key, scheme, access_token, refresh_token, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, to_char(last_refresh_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_refresh_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM provider_oauth_credentials WHERE provider_id = $1",
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
            "INSERT INTO provider_oauth_credentials (provider_id, driver_key, scheme, access_token, refresh_token, expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'connected', 0, NULL) ON CONFLICT(provider_id) DO UPDATE SET driver_key=EXCLUDED.driver_key, scheme=EXCLUDED.scheme, access_token=EXCLUDED.access_token, refresh_token=EXCLUDED.refresh_token, expires_at=EXCLUDED.expires_at, resource_url=EXCLUDED.resource_url, subject_id=EXCLUDED.subject_id, scopes=EXCLUDED.scopes, meta=EXCLUDED.meta, status='connected', status_version=provider_oauth_credentials.status_version+1, last_error=NULL, updated_at=CURRENT_TIMESTAMP",
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
        sqlx::query("DELETE FROM provider_oauth_credentials WHERE provider_id = $1")
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
            "UPDATE provider_oauth_credentials SET status='refreshing', status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$1 AND status='connected' AND status_version=$2",
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
            "UPDATE provider_oauth_credentials SET driver_key=$1, scheme=$2, access_token=$3, refresh_token=$4, expires_at=$5, resource_url=$6, subject_id=$7, scopes=$8, meta=$9, status='connected', status_version=status_version+1, last_error=NULL, last_refresh_at=CURRENT_TIMESTAMP, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$10 AND status='refreshing' AND status_version=$11",
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
            "UPDATE provider_oauth_credentials SET status='connected', last_error=$1, status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE provider_id=$2 AND status='refreshing' AND status_version=$3",
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
            "SELECT provider_id, driver_key, scheme, access_token, refresh_token, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, resource_url, subject_id, scopes, meta, status, status_version, last_error, to_char(last_refresh_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_refresh_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM provider_oauth_credentials WHERE status='connected' AND expires_at IS NOT NULL AND expires_at <= CURRENT_TIMESTAMP + ($1 * INTERVAL '1 second')",
        )
        .bind(seconds)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64> {
        let seconds = timeout.as_secs() as i64;
        let result = sqlx::query(
            "UPDATE provider_oauth_credentials SET status='connected', last_error='refresh timeout: process did not complete within timeout', status_version=status_version+1, updated_at=CURRENT_TIMESTAMP WHERE status='refreshing' AND updated_at + ($1 * INTERVAL '1 second') < CURRENT_TIMESTAMP",
        )
        .bind(seconds)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[derive(Clone)]
struct PostgresProviderStore {
    pool: Pool<Postgres>,
}

impl PostgresProviderStore {
    async fn load_endpoints(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<Vec<ProviderProtocolEndpoint>> {
        let base = "SELECT id, provider_id, protocol, base_url, api_key, COALESCE(auth_scheme, 'auto') AS auth_scheme, COALESCE(is_enabled, TRUE) AS is_enabled, COALESCE(priority, 0) AS priority, COALESCE(test_status, 'untested') AS test_status, test_error, to_char(tested_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS tested_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM provider_protocol_endpoints";
        let endpoints = if let Some(provider_id) = provider_id {
            sqlx::query_as::<_, ProviderProtocolEndpoint>(&format!(
                "{base} WHERE provider_id = $1 ORDER BY priority, created_at, id"
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

fn postgres_endpoint_inputs_or_legacy(
    input: &CreateProvider,
) -> Vec<CreateProviderProtocolEndpoint> {
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
impl ProviderStore for PostgresProviderStore {
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
        let mut provider = sqlx::query_as::<_, Provider>(&provider_select(Some("WHERE id = $1")))
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
        let endpoint_inputs = postgres_endpoint_inputs_or_legacy(&input);
        if !is_valid_provider_auth_mode(&input.auth_mode) {
            anyhow::bail!("unsupported provider auth_mode: {}", input.auth_mode);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO providers (id, name, vendor, protocol, base_url, protocol_mode, preset_key, channel, models_source, static_models, api_key, auth_mode, use_proxy, fast_mode) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
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
                "INSERT INTO provider_protocol_endpoints (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
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
            "UPDATE providers SET name=$1, vendor=$2, protocol=$3, base_url=$4, protocol_mode=$5, preset_key=$6, channel=$7, models_source=$8, static_models=$9, api_key=$10, auth_mode=$11, use_proxy=$12, fast_mode=$13, is_enabled=$14, updated_at=CURRENT_TIMESTAMP WHERE id=$15",
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
            sqlx::query("DELETE FROM provider_protocol_endpoints WHERE provider_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            for endpoint in endpoint_inputs.unwrap_or_default() {
                sqlx::query(
                    "INSERT INTO provider_protocol_endpoints (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
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
             WHERE provider_id = $1
                OR model_id IN (SELECT id FROM models WHERE target_provider = $1)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM models WHERE target_provider = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM provider_protocol_endpoints WHERE provider_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM providers WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM providers WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
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
            "UPDATE providers SET last_test_success = $1, last_test_at = CURRENT_TIMESTAMP WHERE id = $2",
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
            "UPDATE provider_protocol_endpoints SET test_status = $1, test_error = $2, tested_at = $3::timestamptz, updated_at = CURRENT_TIMESTAMP WHERE id = $4",
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

#[derive(Clone)]
struct PostgresModelStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl ModelStore for PostgresModelStore {
    async fn list(&self) -> anyhow::Result<Vec<Model>> {
        Ok(
            sqlx::query_as::<_, Model>(&model_select(Some("ORDER BY created_at DESC")))
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Model>> {
        let sql = format!("{} WHERE id = $1", model_select(None));
        Ok(sqlx::query_as::<_, Model>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn create(&self, input: CreateModel) -> anyhow::Result<Model> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO models (id, name, balance, target_provider, target_model, enable_auth, enable_payload, vision_shim, force_max_reasoning) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
            "UPDATE models SET name=$1, balance=$2, target_provider=$3, target_model=$4, enable_auth=$5, enable_payload=$6, vision_shim=$7, force_max_reasoning=$8, is_enabled=$9 WHERE id=$10",
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
        sqlx::query("DELETE FROM models WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM models WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }
}

#[async_trait]
impl ModelSnapshotStore for PostgresModelStore {
    async fn load_active_snapshot(&self) -> anyhow::Result<Vec<Model>> {
        let sql = format!(
            "{} WHERE COALESCE(is_enabled, TRUE) = true",
            model_select(None)
        );
        Ok(sqlx::query_as::<_, Model>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }
}

#[derive(Clone)]
struct PostgresModelBackendStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl ModelBackendStore for PostgresModelBackendStore {
    async fn list_backends_by_model(&self, model_id: &str) -> anyhow::Result<Vec<ModelBackend>> {
        Ok(sqlx::query_as::<_, ModelBackend>(
            "SELECT id, model_id, provider_id, model, weight, priority, is_fallback, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at FROM model_backends WHERE model_id = $1 ORDER BY priority ASC, created_at ASC",
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
        sqlx::query("DELETE FROM model_backends WHERE model_id = $1")
            .bind(model_id)
            .execute(&mut *tx)
            .await?;

        for backend in backends {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority, is_fallback) VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
        sqlx::query("DELETE FROM model_backends WHERE model_id = $1")
            .bind(model_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
struct PostgresSettingsStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl SettingsStore for PostgresSettingsStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (name, value, updated_at) VALUES ($1, $2, CURRENT_TIMESTAMP) ON CONFLICT(name) DO UPDATE SET value=EXCLUDED.value, updated_at=EXCLUDED.updated_at",
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

#[derive(Clone)]
struct PostgresApiKeyStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl ApiKeyStore for PostgresApiKeyStore {
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
        let row = sqlx::query_as::<_, ApiKey>(&api_key_select(Some("WHERE id = $1")))
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
            "INSERT INTO api_keys (id, token, name, rpm, rpd, tpm, tpd, is_privileged, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULLIF($9, '')::timestamptz)",
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
        let current = sqlx::query_as::<_, ApiKey>(&api_key_select(Some("WHERE id = $1")))
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
            "UPDATE api_keys SET name=$1, rpm=$2, rpd=$3, tpm=$4, tpd=$5, is_enabled=$6, is_privileged=$7, expires_at=NULLIF($8, '')::timestamptz, updated_at=CURRENT_TIMESTAMP WHERE id=$9",
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
        sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        let row = if let Some(exclude_id) = exclude_id {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim($1)) AND id != $2 LIMIT 1",
            )
            .bind(name)
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM api_keys WHERE lower(trim(name)) = lower(trim($1)) LIMIT 1",
            )
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
        };
        Ok(row.is_some())
    }
}

#[derive(Clone)]
struct PostgresAuthAccessStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl AuthAccessStore for PostgresAuthAccessStore {
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
            "SELECT id, COALESCE(name, '') AS name, COALESCE(is_enabled, TRUE) AS is_enabled, COALESCE(is_privileged, FALSE) AS is_privileged, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, rpm, rpd, tpm, tpd FROM api_keys WHERE token = $1",
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
            "SELECT COUNT(*) FROM api_key_models WHERE api_key_id = $1 AND model_id = $2",
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
        let interval = interval_expr(window);
        let sql = format!(
            "SELECT COUNT(*) FROM request_logs WHERE api_key_id = $1 AND created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{interval}') * 1000"
        );
        Ok(sqlx::query_scalar::<_, i64>(&sql)
            .bind(api_key_id)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn token_count_since(
        &self,
        api_key_id: &str,
        window: UsageWindow,
    ) -> anyhow::Result<i64> {
        let interval = interval_expr(window);
        let sql = format!(
            "SELECT COALESCE(SUM(input_tokens + output_tokens), 0) FROM request_logs WHERE api_key_id = $1 AND created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{interval}') * 1000"
        );
        Ok(sqlx::query_scalar::<_, i64>(&sql)
            .bind(api_key_id)
            .fetch_one(&self.pool)
            .await?)
    }
}

#[derive(Clone)]
struct PostgresLogStore {
    pool: Pool<Postgres>,
}

#[async_trait]
impl LogStore for PostgresLogStore {
    async fn model_performance_stats(
        &self,
        pairs: &[(String, String)],
        as_of: i64,
    ) -> anyhow::Result<Vec<crate::db::PairPerformanceStats>> {
        crate::db::model_performance::model_performance_method!(
            self,
            pairs,
            as_of,
            sqlx::Postgres,
            "upstream_model COLLATE \"C\"",
            "CAST(created_at AS BIGINT)",
            "FALSE"
        )
    }
    async fn distinct_logged_pairs(&self) -> anyhow::Result<Vec<(String, String)>> {
        Ok(sqlx::query_as::<_, (String, String)>(
            "SELECT DISTINCT provider_id, upstream_model FROM request_logs",
        )
        .fetch_all(&self.pool)
        .await?)
    }
    async fn append_batch(&self, entries: Vec<LogEntry>) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for entry in entries {
            let diagnostic = &entry.diagnostic;
            let error_causes_json = serde_json::to_string(&diagnostic.error_causes)?;
            let payload_metadata_json = serde_json::to_string(&diagnostic.payload_metadata)?;
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
                     client_request_id, attempt_index, outcome_version, attempt_outcome, failure_kind, failure_stage, error_message, error_causes_json, payload_metadata_json, payload_cleared_at)
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$31,$32,$33,$34,$35,$36,$37,$38,$39,$40,$41,$42,$43,$44,$45,$46,$47,$48,$49,$50,$51,$52,$53,$54,NULL)"#,
            )
            .bind(&diagnostic.log_id)
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
            .bind(&diagnostic.client_request_id)
            .bind(diagnostic.attempt_index)
            .bind(diagnostic.outcome_version)
            .bind(&diagnostic.attempt_outcome)
            .bind(&diagnostic.failure_kind)
            .bind(&diagnostic.failure_stage)
            .bind(&diagnostic.error_message)
            .bind(error_causes_json)
            .bind(payload_metadata_json)
            .execute(&mut *tx)
            .await?;

            if let Some(result) = &diagnostic.final_result {
                sqlx::query(
                    "INSERT INTO request_results (client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (client_request_id) DO UPDATE SET final_outcome = EXCLUDED.final_outcome, final_attempt_id = EXCLUDED.final_attempt_id, attempt_count = EXCLUDED.attempt_count, finished_at = EXCLUDED.finished_at",
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
            "SELECT id, COALESCE(created_at::BIGINT, 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, reasoning_effort, route_decision, method, path, \
             NULL::text AS client_request_headers, NULL::text AS client_request_body, \
             NULL::text AS client_response_headers, NULL::text AS client_response_body, \
             NULL::text AS upstream_request_headers, NULL::text AS upstream_request_body, \
             NULL::text AS upstream_response_headers, NULL::text AS upstream_response_body, \
             upstream_status_code, client_status_code, \
             latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(is_stream, FALSE) AS is_stream, stream_chunks_count, stream_first_chunk_ms, \
             performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at, \
             client_request_id, attempt_index, outcome_version, attempt_outcome, failure_kind, failure_stage, error_message, \
             error_causes_json AS error_causes, payload_metadata_json AS payload_metadata, payload_cleared_at \
             FROM request_logs WHERE 1=1",
        );
        let mut idx = 1;
        let mut bind_values: Vec<String> = Vec::new();

        if let Some(provider) = query.provider.filter(|v| !v.is_empty()) {
            count_sql.push_str(&format!(" AND provider_id = ${idx}"));
            data_sql.push_str(&format!(" AND provider_id = ${idx}"));
            bind_values.push(provider);
            idx += 1;
        }
        if let Some(client_model) = query.client_model.filter(|v| !v.is_empty()) {
            count_sql.push_str(&format!(" AND client_model = ${idx}"));
            data_sql.push_str(&format!(" AND client_model = ${idx}"));
            bind_values.push(client_model);
            idx += 1;
        }
        let upstream_model = query
            .upstream_model
            .filter(|v| !v.is_empty())
            .or_else(|| query.model.filter(|v| !v.is_empty()));
        if let Some(upstream_model) = upstream_model {
            count_sql.push_str(&format!(" AND upstream_model = ${idx}"));
            data_sql.push_str(&format!(" AND upstream_model = ${idx}"));
            bind_values.push(upstream_model);
            idx += 1;
        }
        if let Some(status_min) = query.status_min {
            count_sql.push_str(&format!(" AND client_status_code >= ${idx}::TEXT::INTEGER"));
            data_sql.push_str(&format!(" AND client_status_code >= ${idx}::TEXT::INTEGER"));
            bind_values.push(status_min.to_string());
            idx += 1;
        }
        if let Some(status_max) = query.status_max {
            count_sql.push_str(&format!(" AND client_status_code <= ${idx}::TEXT::INTEGER"));
            data_sql.push_str(&format!(" AND client_status_code <= ${idx}::TEXT::INTEGER"));
            bind_values.push(status_max.to_string());
            idx += 1;
        }
        if let Some(api_key) = query.api_key.filter(|v| !v.is_empty()) {
            count_sql.push_str(&format!(" AND api_key_id = ${idx}"));
            data_sql.push_str(&format!(" AND api_key_id = ${idx}"));
            bind_values.push(api_key);
            idx += 1;
        }
        if let Some(after) = query.after {
            count_sql.push_str(&format!(" AND created_at >= ${idx}::TEXT::BIGINT"));
            data_sql.push_str(&format!(" AND created_at >= ${idx}::TEXT::BIGINT"));
            bind_values.push(after.to_string());
            idx += 1;
        }
        if let Some(before) = query.before {
            count_sql.push_str(&format!(" AND created_at <= ${idx}::TEXT::BIGINT"));
            data_sql.push_str(&format!(" AND created_at <= ${idx}::TEXT::BIGINT"));
            bind_values.push(before.to_string());
            idx += 1;
        }

        if let Some(client_request_id) = query.client_request_id {
            count_sql.push_str(&format!(" AND client_request_id = ${idx}"));
            data_sql.push_str(&format!(" AND client_request_id = ${idx}"));
            bind_values.push(client_request_id);
            idx += 1;
        }
        if let Some(is_error) = query.is_error {
            let predicate = error_sql("");
            let filter = if is_error {
                format!(" AND ({predicate})")
            } else {
                format!(" AND NOT ({predicate})")
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
            let filter = format!(" AND ({})", outcome_sql("", &outcome));
            count_sql.push_str(&filter);
            data_sql.push_str(&filter);
        }

        data_sql.push_str(&format!(
            " ORDER BY created_at DESC LIMIT ${idx} OFFSET ${}",
            idx + 1
        ));

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
            "SELECT id, COALESCE(created_at::BIGINT, 0) AS created_at, api_key_id, api_key_name, \
             client_protocol, upstream_protocol, provider_id, provider_name, model_id, model_name, upstream_url, \
             client_model, upstream_model, reasoning_effort, route_decision, method, path, \
             client_request_headers, client_request_body, \
             client_response_headers, client_response_body, \
             upstream_request_headers, upstream_request_body, \
             upstream_response_headers, upstream_response_body, \
             upstream_status_code, client_status_code, \
             latency_total_ms, latency_upstream_ms, \
             input_tokens, output_tokens, COALESCE(cache_read_tokens, 0) AS cache_read_tokens, \
             COALESCE(is_stream, FALSE) AS is_stream, stream_chunks_count, stream_first_chunk_ms, \
             performance_metadata_version, upstream_effort_status, upstream_effort_raw, upstream_effort_tier, request_completion, completion_reason, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at, \
             client_request_id, attempt_index, outcome_version, attempt_outcome, failure_kind, failure_stage, error_message, \
             error_causes_json AS error_causes, payload_metadata_json AS payload_metadata, payload_cleared_at \
             FROM request_logs WHERE id = $1",
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
            "SELECT client_request_id, final_outcome, final_attempt_id, attempt_count, finished_at FROM request_results WHERE client_request_id = $1",
        )
        .bind(client_request_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn cleanup_before(&self, cutoff_expression: &str) -> anyhow::Result<u64> {
        let interval = cutoff_expression.trim().trim_start_matches('-').trim();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "DELETE FROM request_logs WHERE created_at < EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - $1::TEXT::INTERVAL) * 1000",
        )
        .bind(interval)
        .execute(&mut *tx)
        .await?;
        cleanup_orphan_request_results_pg(&mut tx).await?;
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
             upstream_response_headers = NULL, upstream_response_body = NULL, \
             payload_cleared_at = $1 \
             WHERE client_request_headers IS NOT NULL OR client_request_body IS NOT NULL \
                OR client_response_headers IS NOT NULL OR client_response_body IS NOT NULL \
                OR upstream_request_headers IS NOT NULL OR upstream_request_body IS NOT NULL \
                OR upstream_response_headers IS NOT NULL OR upstream_response_body IS NOT NULL",
        )
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn delete_by_id(&self, id: &str) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("DELETE FROM request_logs WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        cleanup_orphan_request_results_pg(&mut tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn clear_errors(&self) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(&format!("DELETE FROM request_logs WHERE {}", error_sql("")))
            .execute(&mut *tx)
            .await?;
        cleanup_orphan_request_results_pg(&mut tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview> {
        let error = error_sql("");
        let time_filter = hours
            .map(|hours| format!(" WHERE created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{hours} hours') * 1000"))
            .unwrap_or_default();
        let sql = format!(
            "SELECT COUNT(*) AS total_requests, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count FROM request_logs{time_filter}"
        );
        Ok(sqlx::query_as::<_, StatsOverview>(&sql)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn stats_hourly(&self, hours: i64) -> anyhow::Result<Vec<StatsHourly>> {
        let error = error_sql("");
        let sql = format!(
            "SELECT to_char(date_trunc('hour', to_timestamp(created_at/1000) AT TIME ZONE 'UTC'), 'YYYY-MM-DD HH24:00:00') AS hour, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms FROM request_logs WHERE created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{hours} hours') * 1000 GROUP BY 1 ORDER BY 1 ASC"
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
            &format!("SELECT (created_at / $1) * $1 AS bucket_start, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, AVG(latency_total_ms)::FLOAT8 AS avg_duration_ms FROM request_logs WHERE created_at >= $2 AND created_at <= $3 AND ($4::TEXT IS NULL OR upstream_model = $4) GROUP BY 1 ORDER BY 1 ASC"),
        )
        .bind(bucket_ms)
        .bind(start_ms)
        .bind(end_ms)
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
            &format!("SELECT COALESCE(upstream_model, '') AS upstream_model, (created_at / $1) * $1 AS bucket_start, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, AVG(latency_total_ms)::FLOAT8 AS avg_duration_ms FROM request_logs WHERE api_key_id = $2 AND created_at >= $3 AND created_at <= $4 GROUP BY 1, 2 ORDER BY 1 ASC, 2 ASC"),
        )
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
                "SELECT upstream_model AS model, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms FROM request_logs WHERE created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{hours} hours') * 1000 GROUP BY upstream_model ORDER BY request_count DESC"
            )
        } else {
            "SELECT upstream_model AS model, COUNT(*) AS request_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms FROM request_logs GROUP BY upstream_model ORDER BY request_count DESC".to_string()
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
             COALESCE(SUM(input_tokens), 0)::BIGINT AS total_input_tokens, \
             COALESCE(SUM(output_tokens), 0)::BIGINT AS total_output_tokens, \
             COALESCE(SUM(cache_read_tokens), 0)::BIGINT AS total_cache_read_tokens, \
             MAX(created_at)::BIGINT AS last_called_at \
             FROM request_logs WHERE provider_id = $1 AND upstream_model = $2",
        )
        .bind(provider_id)
        .bind(upstream_model)
        .fetch_one(&self.pool)
        .await?;
        let samples = sqlx::query_as::<_, RecentModelPerformance>(
            "SELECT COALESCE(output_tokens, 0) AS output_tokens, COALESCE(is_stream, FALSE) AS is_stream, \
             COALESCE(stream_chunks_count, 0) AS stream_chunks_count, latency_upstream_ms, latency_total_ms, stream_first_chunk_ms \
             FROM request_logs WHERE provider_id = $1 AND upstream_model = $2 \
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
        let time_filter = hours.map(|hours| format!(" AND created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{hours} hours') * 1000")).unwrap_or_default();
        let sql = format!(
            "WITH aggregated AS (SELECT provider_id, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms FROM request_logs WHERE provider_id IS NOT NULL AND btrim(provider_id) <> ''{time_filter} GROUP BY provider_id) SELECT a.provider_id, COALESCE((SELECT NULLIF(btrim(r.provider_name), '') FROM request_logs r WHERE r.provider_id = a.provider_id AND NULLIF(btrim(r.provider_name), '') IS NOT NULL ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.provider_id) AS provider, NULL::TEXT AS provider_icon, NULL::TEXT AS provider_protocol, a.request_count, a.error_count, a.avg_duration_ms, a.total_output_tokens, a.total_upstream_ms FROM aggregated a ORDER BY a.request_count DESC, a.provider_id ASC"
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
        let error = error_sql("");
        let success = outcome_sql("", "completed");
        let unknown = outcome_sql("", "unknown");
        let cancelled = outcome_sql("", "cancelled");
        let output_limited = outcome_sql("", "output_limited");
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
        let summary = sqlx::query_as::<_, SummaryRow>(&format!("SELECT COALESCE((SELECT NULLIF(btrim(r.provider_name), '') FROM request_logs r WHERE r.provider_id = $1 AND NULLIF(btrim(r.provider_name), '') IS NOT NULL ORDER BY r.created_at DESC, r.id DESC LIMIT 1), $1) AS provider_name, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {success} THEN 1 ELSE 0 END), 0) AS success_count, COALESCE(SUM(CASE WHEN {unknown} THEN 1 ELSE 0 END), 0) AS unknown_count, COALESCE(SUM(CASE WHEN {cancelled} THEN 1 ELSE 0 END), 0) AS cancelled_count, COALESCE(SUM(CASE WHEN {output_limited} THEN 1 ELSE 0 END), 0) AS output_limited_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE provider_id = $1 AND created_at >= $2 AND created_at <= $3")).bind(provider_id).bind(start_at).bind(end_at).fetch_one(&self.pool).await?;
        let models = sqlx::query_as::<_, ProviderModelUsageStats>(&format!("SELECT COALESCE(upstream_model, '') AS upstream_model, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE provider_id = $1 AND created_at >= $2 AND created_at <= $3 GROUP BY COALESCE(upstream_model, '') ORDER BY request_count DESC, upstream_model ASC")).bind(provider_id).bind(start_at).bind(end_at).fetch_all(&self.pool).await?;
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
        let error = error_sql("f");
        let time_filter = hours
            .map(|hours| format!(" AND created_at >= EXTRACT(EPOCH FROM CURRENT_TIMESTAMP - INTERVAL '{hours} hours') * 1000"))
            .unwrap_or_default();
        let sql = format!(
            "WITH aggregated AS (SELECT f.api_key_id, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(f.input_tokens), 0) AS total_input_tokens, COALESCE(SUM(f.output_tokens), 0) AS total_output_tokens, COALESCE(SUM(f.cache_read_tokens), 0) AS cache_read_tokens, MAX(f.created_at) AS last_used_at FROM request_logs f WHERE f.api_key_id IS NOT NULL AND f.api_key_id <> ''{time_filter} GROUP BY f.api_key_id) SELECT a.api_key_id, COALESCE((SELECT COALESCE(NULLIF(r.api_key_name, ''), r.api_key_id) FROM request_logs r WHERE r.api_key_id = a.api_key_id ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.api_key_id) AS api_key_name, a.request_count, a.error_count, a.total_input_tokens, a.total_output_tokens, a.cache_read_tokens, a.last_used_at FROM aggregated a ORDER BY a.request_count DESC, a.api_key_id ASC"
        );
        Ok(sqlx::query_as::<_, ApiKeyStats>(&sql)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn api_key_usage_detail(
        &self,
        api_key_id: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> anyhow::Result<ApiKeyUsageDetail> {
        let error = error_sql("");
        let success = outcome_sql("", "completed");
        let unknown = outcome_sql("", "unknown");
        let cancelled = outcome_sql("", "cancelled");
        let output_limited = outcome_sql("", "output_limited");
        #[derive(sqlx::FromRow)]
        struct ApiKeyUsageSummaryRow {
            start_at: i64,
            end_at: i64,
            api_key_id: String,
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

        let summary = sqlx::query_as::<_, ApiKeyUsageSummaryRow>(
            &format!("SELECT $2::BIGINT AS start_at, $3::BIGINT AS end_at, $1::TEXT AS api_key_id, COALESCE((SELECT COALESCE(NULLIF(name_log.api_key_name, ''), name_log.api_key_id) FROM request_logs name_log WHERE name_log.api_key_id = $1 ORDER BY name_log.created_at DESC, name_log.id DESC LIMIT 1), $1) AS api_key_name, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {success} THEN 1 ELSE 0 END), 0) AS success_count, COALESCE(SUM(CASE WHEN {unknown} THEN 1 ELSE 0 END), 0) AS unknown_count, COALESCE(SUM(CASE WHEN {cancelled} THEN 1 ELSE 0 END), 0) AS cancelled_count, COALESCE(SUM(CASE WHEN {output_limited} THEN 1 ELSE 0 END), 0) AS output_limited_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE api_key_id = $1 AND created_at >= $2 AND created_at <= $3"),
        )
        .bind(api_key_id)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_one(&self.pool)
        .await?;

        let model_routes = sqlx::query_as::<_, ApiKeyModelRouteStats>(
            &format!("WITH grouped AS (SELECT COALESCE(client_model, '') AS client_model, COALESCE(provider_id, '') AS provider_id, COALESCE(upstream_model, '') AS upstream_model, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms FROM request_logs WHERE api_key_id = $1 AND created_at >= $2 AND created_at <= $3 GROUP BY COALESCE(client_model, ''), COALESCE(provider_id, ''), COALESCE(upstream_model, '')), latest_provider AS (SELECT COALESCE(provider_id, '') AS provider_id, COALESCE(NULLIF(provider_name, ''), provider_id, '') AS provider_name, ROW_NUMBER() OVER (PARTITION BY COALESCE(provider_id, '') ORDER BY created_at DESC, id DESC) AS row_num FROM request_logs WHERE COALESCE(provider_id, '') IN (SELECT provider_id FROM grouped)) SELECT g.client_model, g.provider_id, COALESCE(p.provider_name, g.provider_id, '') AS provider_name, g.upstream_model, g.request_count, g.error_count, g.total_input_tokens, g.total_output_tokens, g.total_cache_read_tokens, g.avg_duration_ms, g.avg_first_token_ms, g.total_upstream_ms FROM grouped g LEFT JOIN latest_provider p ON p.provider_id = g.provider_id AND p.row_num = 1 ORDER BY g.request_count DESC, g.client_model ASC, g.provider_id ASC, g.upstream_model ASC"),
        )
        .bind(api_key_id)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(ApiKeyUsageDetail {
            start_at: summary.start_at,
            end_at: summary.end_at,
            api_key_id: summary.api_key_id,
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
        let error = error_sql("");
        let success = outcome_sql("", "completed");
        let unknown = outcome_sql("", "unknown");
        let cancelled = outcome_sql("", "cancelled");
        let output_limited = outcome_sql("", "output_limited");
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
        let summary = sqlx::query_as::<_, SummaryRow>(
            &format!("SELECT COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {success} THEN 1 ELSE 0 END), 0) AS success_count, COALESCE(SUM(CASE WHEN {unknown} THEN 1 ELSE 0 END), 0) AS unknown_count, COALESCE(SUM(CASE WHEN {cancelled} THEN 1 ELSE 0 END), 0) AS cancelled_count, COALESCE(SUM(CASE WHEN {output_limited} THEN 1 ELSE 0 END), 0) AS output_limited_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE upstream_model = $1 AND created_at >= $2 AND created_at <= $3"),
        )
        .bind(upstream_model)
        .bind(start_at)
        .bind(end_at)
        .fetch_one(&self.pool)
        .await?;
        let providers = sqlx::query_as::<_, ModelProviderUsageStats>(
            &format!("WITH aggregated AS (SELECT provider_id, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE upstream_model = $1 AND provider_id IS NOT NULL AND btrim(provider_id) <> '' AND created_at >= $2 AND created_at <= $3 GROUP BY provider_id) SELECT a.provider_id, COALESCE((SELECT NULLIF(btrim(r.provider_name), '') FROM request_logs r WHERE r.provider_id = a.provider_id AND NULLIF(btrim(r.provider_name), '') IS NOT NULL ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.provider_id) AS provider_name, NULL::TEXT AS provider_icon, NULL::TEXT AS provider_protocol, a.request_count, a.error_count, a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens, a.avg_duration_ms, a.avg_first_token_ms, a.total_upstream_ms, a.last_used_at FROM aggregated a ORDER BY a.request_count DESC, a.provider_id ASC"),
        )
        .bind(upstream_model)
        .bind(start_at)
        .bind(end_at)
        .fetch_all(&self.pool)
        .await?;
        let api_keys = sqlx::query_as::<_, ModelApiKeyUsageStats>(
            &format!("WITH aggregated AS (SELECT api_key_id, COUNT(*) AS request_count, COALESCE(SUM(CASE WHEN {error} THEN 1 ELSE 0 END), 0) AS error_count, COALESCE(SUM(input_tokens), 0) AS total_input_tokens, COALESCE(SUM(output_tokens), 0) AS total_output_tokens, COALESCE(SUM(cache_read_tokens), 0) AS total_cache_read_tokens, COALESCE(AVG(latency_total_ms)::FLOAT8, 0) AS avg_duration_ms, AVG(CASE WHEN stream_first_chunk_ms >= 0 THEN stream_first_chunk_ms END)::FLOAT8 AS avg_first_token_ms, COALESCE(SUM(latency_upstream_ms), 0)::FLOAT8 AS total_upstream_ms, MAX(created_at) AS last_used_at FROM request_logs WHERE upstream_model = $1 AND api_key_id IS NOT NULL AND api_key_id <> '' AND created_at >= $2 AND created_at <= $3 GROUP BY api_key_id) SELECT a.api_key_id, COALESCE((SELECT COALESCE(NULLIF(r.api_key_name, ''), r.api_key_id) FROM request_logs r WHERE r.api_key_id = a.api_key_id ORDER BY r.created_at DESC, r.id DESC LIMIT 1), a.api_key_id) AS api_key_name, a.request_count, a.error_count, a.total_input_tokens, a.total_output_tokens, a.total_cache_read_tokens, a.avg_duration_ms, a.avg_first_token_ms, a.total_upstream_ms, a.last_used_at FROM aggregated a ORDER BY a.request_count DESC, a.api_key_id ASC"),
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

async fn cleanup_orphan_request_results_pg(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM request_results WHERE NOT EXISTS (SELECT 1 FROM request_logs WHERE request_logs.client_request_id = request_results.client_request_id)",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Clone)]
struct PostgresBootstrap {
    adapter: PostgresAdapter,
}

#[async_trait]
impl StorageBootstrap for PostgresBootstrap {
    async fn init(&self) -> anyhow::Result<()> {
        self.adapter.ping().await
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::raw_sql(POSTGRES_INIT_SQL)
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE routes ADD COLUMN IF NOT EXISTS balance TEXT DEFAULT 'weighted'")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query(
            "UPDATE routes SET balance = 'weighted' WHERE balance IS NULL OR btrim(balance) = ''",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS use_proxy BOOLEAN NOT NULL DEFAULT FALSE")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS fast_mode BOOLEAN NOT NULL DEFAULT FALSE")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS protocol_mode TEXT NOT NULL DEFAULT 'fixed'")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS auth_mode TEXT NOT NULL DEFAULT 'apikey'")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS access_token TEXT")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS refresh_token TEXT")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE providers DROP CONSTRAINT IF EXISTS providers_auth_mode_check")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("UPDATE providers SET auth_mode = 'apikey' WHERE auth_mode = 'api_key'")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query(
            r#"DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'providers_auth_mode_check'
    ) THEN
        ALTER TABLE providers
        ADD CONSTRAINT providers_auth_mode_check
        CHECK (auth_mode IN ('apikey', 'oauth'));
    END IF;
END $$;"#,
        )
        .execute(self.adapter.pool())
        .await?;
        migrate_collapse_provider_protocol_columns_pg(self.adapter.pool()).await?;
        sqlx::query(
            "INSERT INTO provider_protocol_endpoints \
             (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) \
             SELECT p.id || '-default-endpoint', p.id, p.protocol, p.base_url, p.api_key, 'auto', TRUE, 0 \
             FROM providers p \
             WHERE NOT EXISTS (\
                 SELECT 1 FROM provider_protocol_endpoints e WHERE e.provider_id = p.id\
             )",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query(
            r#"
            INSERT INTO route_targets (id, route_id, provider_id, model, weight, priority)
            SELECT md5(random()::text || clock_timestamp()::text), r.id, r.target_provider, r.target_model, 100, 1
            FROM routes r
            WHERE r.target_provider IS NOT NULL
              AND btrim(r.target_provider) != ''
              AND NOT EXISTS (SELECT 1 FROM route_targets rt WHERE rt.route_id = r.id)
            "#,
        )
        .execute(self.adapter.pool())
        .await?;
        // Migrate: providers/routes is_active -> is_enabled
        sqlx::query(
            "ALTER TABLE providers ADD COLUMN IF NOT EXISTS is_enabled BOOLEAN DEFAULT TRUE",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query("UPDATE providers SET is_enabled = is_active WHERE is_active IS NOT NULL AND is_enabled IS DISTINCT FROM is_active")
            .execute(self.adapter.pool())
            .await
            .ok();
        sqlx::query("ALTER TABLE routes ADD COLUMN IF NOT EXISTS is_enabled BOOLEAN DEFAULT TRUE")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("UPDATE routes SET is_enabled = is_active WHERE is_active IS NOT NULL AND is_enabled IS DISTINCT FROM is_active")
            .execute(self.adapter.pool())
            .await
            .ok();
        // Migrate: api_keys status -> is_enabled
        sqlx::query(
            "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS is_enabled BOOLEAN DEFAULT TRUE",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query(
            "UPDATE api_keys SET is_enabled = CASE WHEN status = 'active' THEN TRUE ELSE FALSE END \
             WHERE status IS NOT NULL AND is_enabled IS DISTINCT FROM (status = 'active')",
        )
        .execute(self.adapter.pool())
        .await
        .ok();
        // Migrate OAuth credentials from providers table to new dedicated table
        sqlx::query(
            r#"
            INSERT INTO provider_oauth_credentials
                (provider_id, access_token, refresh_token, expires_at, status)
            SELECT id, COALESCE(access_token, ''), refresh_token, expires_at, 'connected'
            FROM providers
            WHERE auth_mode = 'oauth'
              AND (
                (access_token IS NOT NULL AND btrim(access_token) != '')
                OR (refresh_token IS NOT NULL AND btrim(refresh_token) != '')
              )
            ON CONFLICT DO NOTHING
            "#,
        )
        .execute(self.adapter.pool())
        .await?;
        // PR2B → PR13: vendor name migrations. Idempotent.
        // `nyro → custom` (PR13 reversal), `zhipu → zhipuai` (PR2B).
        for (from, to) in [("nyro", "custom"), ("zhipu", "zhipuai")] {
            sqlx::query("UPDATE providers SET vendor = $1 WHERE lower(btrim(vendor)) = $2")
                .bind(to)
                .bind(from)
                .execute(self.adapter.pool())
                .await?;
            sqlx::query("UPDATE providers SET preset_key = $1 WHERE lower(btrim(preset_key)) = $2")
                .bind(to)
                .bind(from)
                .execute(self.adapter.pool())
                .await?;
        }
        // PR4: rewrite provider protocol identifiers into canonical
        // `family/dialect/version` form. Idempotent.
        normalize_provider_protocols_pg(self.adapter.pool()).await?;
        // Q2: drop route_type column (idempotent via IF EXISTS).
        sqlx::query("ALTER TABLE routes DROP COLUMN IF EXISTS route_type")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query(
            "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS cache_read_tokens INTEGER DEFAULT 0",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query("ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS reasoning_effort TEXT")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS route_decision TEXT")
            .execute(self.adapter.pool())
            .await?;

        // Rename tables: routes → models, route_targets → model_backends, api_key_routes → api_key_models
        pg_rename_table_if_needed(self.adapter.pool(), "routes", "models").await?;
        pg_rename_table_if_needed(self.adapter.pool(), "route_targets", "model_backends").await?;
        pg_rename_table_if_needed(self.adapter.pool(), "api_key_routes", "api_key_models").await?;

        // Rename columns within renamed tables
        pg_rename_column_if_needed(
            self.adapter.pool(),
            "model_backends",
            "route_id",
            "model_id",
        )
        .await?;
        pg_rename_column_if_needed(
            self.adapter.pool(),
            "api_key_models",
            "route_id",
            "model_id",
        )
        .await?;

        // Rename columns in request_logs: route_id → model_id, route_name → model_name
        pg_rename_column_if_needed(self.adapter.pool(), "request_logs", "route_id", "model_id")
            .await?;
        pg_rename_column_if_needed(
            self.adapter.pool(),
            "request_logs",
            "route_name",
            "model_name",
        )
        .await?;

        // Rename column: models strategy → balance
        pg_rename_column_if_needed(self.adapter.pool(), "models", "strategy", "balance").await?;

        // Merge virtual_model into name and drop the column
        if pg_column_exists(self.adapter.pool(), "models", "virtual_model").await? {
            tracing::info!("merging virtual_model into name on models table (postgres)");
            sqlx::query(
                "UPDATE models SET name = BTRIM(virtual_model)
                 WHERE virtual_model IS NOT NULL AND BTRIM(virtual_model) != ''",
            )
            .execute(self.adapter.pool())
            .await?;
            sqlx::query("ALTER TABLE models DROP COLUMN virtual_model")
                .execute(self.adapter.pool())
                .await?;
        }

        // Rename access_control → enable_auth on models table
        pg_rename_column_if_needed(
            self.adapter.pool(),
            "models",
            "access_control",
            "enable_auth",
        )
        .await?;
        sqlx::query("ALTER TABLE models ADD COLUMN IF NOT EXISTS enable_payload BOOLEAN")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query("ALTER TABLE models ADD COLUMN IF NOT EXISTS vision_shim TEXT")
            .execute(self.adapter.pool())
            .await?;
        sqlx::query(
            "ALTER TABLE models ADD COLUMN IF NOT EXISTS force_max_reasoning BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(self.adapter.pool())
        .await?;
        sqlx::query(
            "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS is_privileged BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(self.adapter.pool())
        .await?;
        // Add is_fallback column to model_backends (last-resort degraded fallback)
        sqlx::query(
            "ALTER TABLE model_backends ADD COLUMN IF NOT EXISTS is_fallback BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(self.adapter.pool())
        .await?;
        // Rename settings key log_record_payloads → enable_payload
        sqlx::query(
            "UPDATE settings SET name = 'enable_payload' WHERE name = 'log_record_payloads'",
        )
        .execute(self.adapter.pool())
        .await
        .ok();

        // Rename columns for MySQL compat: settings.key → settings.name, api_keys.key → api_keys.token
        pg_rename_column_if_needed(self.adapter.pool(), "settings", "key", "name").await?;
        pg_rename_column_if_needed(self.adapter.pool(), "api_keys", "key", "token").await?;
        migrate_performance_pg(self.adapter.pool()).await?;
        migrate_diagnostics_pg(self.adapter.pool()).await?;
        // Provider-scoped ratings were replaced by prefix ratings; old rows are
        // deliberately dropped instead of migrated.
        sqlx::query("DROP TABLE IF EXISTS provider_model_ratings")
            .execute(self.adapter.pool())
            .await?;
        crate::db::model_performance::recover_historical_metadata!(
            self.adapter.pool(),
            sqlx::Postgres,
            "octet_length(upstream_request_body)"
        );

        Ok(())
    }

    async fn health(&self) -> anyhow::Result<StorageHealth> {
        let health = self.adapter.health().await;
        Ok(StorageHealth {
            backend: StorageBackend::Postgres,
            can_connect: health.can_connect,
            schema_compatible: health.schema_compatible,
            writable: health.can_connect,
        })
    }
}

async fn migrate_diagnostics_pg(pool: &Pool<Postgres>) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    // Legacy rows stay unknown: performance observations are not diagnostic proof.
    for (column, definition) in [
        ("client_request_id", "TEXT"),
        ("attempt_index", "INTEGER"),
        ("outcome_version", "INTEGER NOT NULL DEFAULT 0"),
        ("attempt_outcome", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("failure_kind", "TEXT"),
        ("failure_stage", "TEXT"),
        ("error_message", "TEXT"),
        ("error_causes_json", "TEXT"),
        ("payload_metadata_json", "TEXT"),
        ("payload_cleared_at", "BIGINT"),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS {column} {definition}"
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS request_results (client_request_id TEXT PRIMARY KEY, final_outcome TEXT NOT NULL, final_attempt_id TEXT, attempt_count INTEGER NOT NULL, finished_at BIGINT NOT NULL)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_client_request_attempt ON request_logs(client_request_id, attempt_index)")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_performance_pg(pool: &Pool<Postgres>) -> anyhow::Result<()> {
    for (column, definition) in [
        ("performance_metadata_version", "INTEGER NOT NULL DEFAULT 0"),
        ("upstream_effort_status", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("upstream_effort_raw", "TEXT"),
        ("upstream_effort_tier", "TEXT"),
        ("request_completion", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("completion_reason", "TEXT"),
        ("upstream_response_mode", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("performance_upstream_ms", "BIGINT"),
        ("performance_first_chunk_ms", "BIGINT"),
        ("performance_completed_at", "BIGINT"),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS {column} {definition}"
        ))
        .execute(pool)
        .await?;
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_performance_pair ON request_logs(provider_id, upstream_model COLLATE \"C\", request_completion, performance_completed_at, id)").execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_performance_recovery ON request_logs(performance_metadata_version, created_at, id)").execute(pool).await?;
    Ok(())
}

/// Collapse removed provider protocol columns into `protocol` / `base_url`,
/// then drop `default_protocol` and `protocol_endpoints`.
async fn migrate_collapse_provider_protocol_columns_pg(
    pool: &Pool<Postgres>,
) -> anyhow::Result<()> {
    let has_default_protocol = pg_column_exists(pool, "providers", "default_protocol").await?;
    let has_protocol_endpoints = pg_column_exists(pool, "providers", "protocol_endpoints").await?;
    if !has_default_protocol && !has_protocol_endpoints {
        return Ok(());
    }

    if has_default_protocol {
        sqlx::query(
            "UPDATE providers \
             SET protocol = btrim(default_protocol) \
             WHERE default_protocol IS NOT NULL AND btrim(default_protocol) != ''",
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
                    sqlx::query("UPDATE providers SET base_url = $1 WHERE id = $2")
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
                    "INSERT INTO provider_protocol_endpoints \
                     (id, provider_id, protocol, base_url, api_key, auth_scheme, is_enabled, priority) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                     ON CONFLICT (provider_id, protocol) DO NOTHING",
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
                "UPDATE providers SET protocol = $1, base_url = $2, protocol_mode = 'adaptive' WHERE id = $3",
            )
            .bind(legacy.default_protocol)
            .bind(effective_base_url)
            .bind(id)
            .execute(pool)
            .await?;
        }
    }

    sqlx::query(
        "ALTER TABLE providers \
         DROP COLUMN IF EXISTS protocol_endpoints, \
         DROP COLUMN IF EXISTS default_protocol",
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn pg_column_exists(
    pool: &Pool<Postgres>,
    table_name: &str,
    column_name: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
            FROM information_schema.columns
            WHERE table_schema = current_schema()
              AND table_name = $1
              AND column_name = $2
        )",
    )
    .bind(table_name)
    .bind(column_name)
    .fetch_one(pool)
    .await?)
}

async fn pg_table_exists(pool: &Pool<Postgres>, table_name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
            FROM information_schema.tables
            WHERE table_schema = current_schema()
              AND table_name = $1
        )",
    )
    .bind(table_name)
    .fetch_one(pool)
    .await?)
}

async fn pg_rename_table_if_needed(
    pool: &Pool<Postgres>,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    if pg_table_exists(pool, old).await? && !pg_table_exists(pool, new).await? {
        tracing::info!("renaming table {old} -> {new}");
        sqlx::query(&format!("ALTER TABLE {old} RENAME TO {new}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn pg_rename_column_if_needed(
    pool: &Pool<Postgres>,
    table: &str,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    if pg_column_exists(pool, table, old).await? && !pg_column_exists(pool, table, new).await? {
        tracing::info!("renaming column {table}.{old} -> {table}.{new}");
        sqlx::query(&format!("ALTER TABLE {table} RENAME COLUMN {old} TO {new}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Postgres counterpart of `crate::db::normalize_provider_protocols` —
/// rewrites fixed providers to suites while retaining exact defaults for
/// adaptive providers.
async fn normalize_provider_protocols_pg(pool: &Pool<Postgres>) -> anyhow::Result<()> {
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
            "normalizing provider protocol identifier (postgres)"
        );

        sqlx::query("UPDATE providers SET protocol = $1 WHERE id = $2")
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
                    "leaving unrecognized adaptive provider endpoint unchanged (postgres)"
                );
                trimmed.to_string()
            });
    }
    match reg.parse_protocol(trimmed) {
        Some(protocol) => protocol.as_str().to_string(),
        None => {
            tracing::warn!(
                value = trimmed,
                "leaving unrecognized provider protocol identifier unchanged (postgres)"
            );
            trimmed.to_string()
        }
    }
}

fn provider_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, name, vendor, protocol, base_url, COALESCE(protocol_mode, 'fixed') AS protocol_mode, preset_key, channel, models_source, static_models, api_key, COALESCE(auth_mode, 'apikey') AS auth_mode, COALESCE(use_proxy, FALSE) AS use_proxy, COALESCE(fast_mode, FALSE) AS fast_mode, last_test_success, to_char(last_test_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS last_test_at, COALESCE(is_enabled, TRUE) AS is_enabled, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM providers",
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
        "SELECT id, name, COALESCE(balance, 'weighted') AS balance, target_provider, target_model, COALESCE(enable_auth, false) AS enable_auth, enable_payload, vision_shim, COALESCE(force_max_reasoning, FALSE) AS force_max_reasoning, COALESCE(is_enabled, TRUE) AS is_enabled, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at FROM models",
    );
    if let Some(suffix) = suffix {
        sql.push(' ');
        sql.push_str(suffix);
    }
    sql
}

fn api_key_select(suffix: Option<&str>) -> String {
    let mut sql = String::from(
        "SELECT id, token, name, rpm, rpd, tpm, tpd, COALESCE(is_enabled, TRUE) AS is_enabled, COALESCE(is_privileged, FALSE) AS is_privileged, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS expires_at, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS created_at, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS updated_at FROM api_keys",
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

fn interval_expr(window: UsageWindow) -> &'static str {
    match window {
        UsageWindow::Minute => "1 minute",
        UsageWindow::Day => "1 day",
    }
}

async fn list_api_key_model_ids(
    pool: &Pool<Postgres>,
    api_key_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT model_id FROM api_key_models WHERE api_key_id = $1 ORDER BY model_id ASC",
    )
    .bind(api_key_id)
    .fetch_all(pool)
    .await?)
}

async fn replace_api_key_models(
    pool: &Pool<Postgres>,
    api_key_id: &str,
    model_ids: &[String],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM api_key_models WHERE api_key_id = $1")
        .bind(api_key_id)
        .execute(&mut *tx)
        .await?;

    for model_id in model_ids.iter().filter(|id| !id.trim().is_empty()) {
        sqlx::query(
            "INSERT INTO api_key_models (api_key_id, model_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(api_key_id)
        .bind(model_id.trim())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

const POSTGRES_INIT_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    vendor TEXT,
    protocol TEXT NOT NULL,
    base_url TEXT NOT NULL,
    protocol_mode TEXT NOT NULL DEFAULT 'fixed',
    preset_key TEXT,
    channel TEXT,
    models_source TEXT,
    static_models TEXT,
    api_key TEXT NOT NULL,
    auth_mode TEXT NOT NULL DEFAULT 'apikey' CHECK (auth_mode IN ('apikey', 'oauth')),
    access_token TEXT,
    refresh_token TEXT,
    expires_at TIMESTAMPTZ,
    use_proxy BOOLEAN NOT NULL DEFAULT FALSE,
    fast_mode BOOLEAN NOT NULL DEFAULT FALSE,
    last_test_success BOOLEAN,
    last_test_at TIMESTAMPTZ,
    is_enabled BOOLEAN DEFAULT TRUE,
    priority INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS model_rating_prefixes (
    model_prefix TEXT COLLATE "C" NOT NULL
        CHECK (octet_length(model_prefix) BETWEEN 1 AND 1024),
    score INTEGER NOT NULL CHECK (score BETWEEN 0 AND 100),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (model_prefix)
);

CREATE TABLE IF NOT EXISTS provider_protocol_endpoints (
    id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    protocol TEXT NOT NULL,
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    auth_scheme TEXT NOT NULL DEFAULT 'auto',
    is_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    priority INTEGER NOT NULL DEFAULT 0,
    test_status TEXT NOT NULL DEFAULT 'untested',
    test_error TEXT,
    tested_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(provider_id, protocol)
);

CREATE INDEX IF NOT EXISTS idx_provider_protocol_endpoints_provider
    ON provider_protocol_endpoints(provider_id, is_enabled, priority);

CREATE TABLE IF NOT EXISTS routes (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    balance TEXT DEFAULT 'weighted',
    target_provider TEXT NOT NULL REFERENCES providers(id),
    target_model TEXT NOT NULL,
    enable_auth BOOLEAN DEFAULT FALSE,
    enable_payload BOOLEAN,
    force_max_reasoning BOOLEAN NOT NULL DEFAULT FALSE,
    vision_shim TEXT,
    is_enabled BOOLEAN DEFAULT TRUE,
    priority INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS route_targets (
    id TEXT PRIMARY KEY,
    route_id TEXT NOT NULL REFERENCES routes(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id),
    model TEXT NOT NULL,
    weight INTEGER DEFAULT 100,
    priority INTEGER DEFAULT 1,
    is_fallback BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_route_targets_route_id ON route_targets(route_id);

CREATE TABLE IF NOT EXISTS request_logs (
    id                        TEXT PRIMARY KEY,
    created_at                BIGINT NOT NULL DEFAULT 0,
    api_key_id                TEXT,
    api_key_name              TEXT,
    client_protocol           TEXT,
    upstream_protocol         TEXT,
    provider_id               TEXT,
    provider_name             TEXT,
    model_id                  TEXT,
    model_name                TEXT,
    upstream_url              TEXT,
    client_model              TEXT,
    upstream_model            TEXT,
    reasoning_effort          TEXT,
    route_decision            TEXT,
    method                    TEXT,
    path                      TEXT,
    client_request_headers    TEXT,
    client_request_body       TEXT,
    client_response_headers   TEXT,
    client_response_body      TEXT,
    upstream_request_headers  TEXT,
    upstream_request_body     TEXT,
    upstream_response_headers TEXT,
    upstream_response_body    TEXT,
    upstream_status_code      INTEGER,
    client_status_code        INTEGER,
    latency_total_ms          BIGINT,
    latency_upstream_ms       BIGINT,
    input_tokens              INTEGER DEFAULT 0,
    output_tokens             INTEGER DEFAULT 0,
    cache_read_tokens         INTEGER DEFAULT 0,
    is_stream                 BOOLEAN DEFAULT FALSE,
    stream_chunks_count       INTEGER DEFAULT 0,
    stream_first_chunk_ms     BIGINT,
    performance_metadata_version INTEGER NOT NULL DEFAULT 0,
    upstream_effort_status    TEXT NOT NULL DEFAULT 'unknown',
    upstream_effort_raw       TEXT,
    upstream_effort_tier      TEXT,
    request_completion        TEXT NOT NULL DEFAULT 'unknown',
    completion_reason         TEXT,
    upstream_response_mode    TEXT NOT NULL DEFAULT 'unknown',
    performance_upstream_ms   BIGINT,
    performance_first_chunk_ms BIGINT,
    performance_completed_at  BIGINT,
    client_request_id         TEXT,
    attempt_index             INTEGER,
    outcome_version           INTEGER NOT NULL DEFAULT 0,
    attempt_outcome           TEXT NOT NULL DEFAULT 'unknown',
    failure_kind              TEXT,
    failure_stage             TEXT,
    error_message             TEXT,
    error_causes_json         TEXT,
    payload_metadata_json     TEXT,
    payload_cleared_at        BIGINT
);

-- No foreign key: the final attempt may be deleted while other attempts remain.
CREATE TABLE IF NOT EXISTS request_results (
    client_request_id TEXT PRIMARY KEY,
    final_outcome TEXT NOT NULL,
    final_attempt_id TEXT,
    attempt_count INTEGER NOT NULL,
    finished_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_logs_created_at ON request_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_logs_provider_id ON request_logs(provider_id);
CREATE INDEX IF NOT EXISTS idx_logs_client_status ON request_logs(client_status_code);
CREATE INDEX IF NOT EXISTS idx_logs_upstream_model ON request_logs(upstream_model);
CREATE INDEX IF NOT EXISTS idx_logs_api_key ON request_logs(api_key_id);

CREATE TABLE IF NOT EXISTS settings (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY,
    token TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    rpm INTEGER,
    rpd INTEGER,
    tpm INTEGER,
    tpd INTEGER,
    is_enabled BOOLEAN DEFAULT TRUE,
    is_privileged BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS api_key_routes (
    api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    route_id TEXT NOT NULL REFERENCES routes(id) ON DELETE CASCADE,
    PRIMARY KEY (api_key_id, route_id)
);

CREATE INDEX IF NOT EXISTS idx_api_keys_token ON api_keys(token);
CREATE INDEX IF NOT EXISTS idx_api_key_routes_route_id ON api_key_routes(route_id);

CREATE TABLE IF NOT EXISTS provider_oauth_credentials (
    provider_id       TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    driver_key        TEXT NOT NULL DEFAULT '',
    scheme            TEXT NOT NULL DEFAULT '',
    access_token      TEXT NOT NULL DEFAULT '',
    refresh_token     TEXT,
    expires_at        TIMESTAMPTZ,
    resource_url      TEXT,
    subject_id        TEXT,
    scopes            TEXT NOT NULL DEFAULT '[]',
    meta              TEXT NOT NULL DEFAULT '{}',
    status            TEXT NOT NULL DEFAULT 'connected',
    status_version    INTEGER NOT NULL DEFAULT 0,
    last_error        TEXT,
    last_refresh_at   TIMESTAMPTZ,
    created_at        TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at        TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_oauth_creds_status ON provider_oauth_credentials(status);
CREATE INDEX IF NOT EXISTS idx_oauth_creds_expires ON provider_oauth_credentials(expires_at);
"#;
