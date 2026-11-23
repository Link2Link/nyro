//! Artificial Analysis Intelligence Index automatic daily sync and alias management.
//!
//! Fetches the paginated Artificial Analysis v2 free language model list,
//! consolidates each base model to its highest reasoning effort tier, and caches
//! the snapshot in both the shared settings store (for multi-replica consistency)
//! and a local runtime cache file (fallback when settings payload is large).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Gateway;
use crate::db::model_rating_match::{
    AaModelAlias, AaModelRatingEntry, AaRatingIndex, AaRawModel, consolidate_aa_models,
};
use crate::db::model_rating_prefixes::canonical_model_prefix;

use super::AdminService;
use super::model_ratings::{ModelRatingError, validate_rating_prefix};

pub const AA_DEFAULT_BASE_URL: &str = "https://artificialanalysis.ai/api/v2/language/models/free";
pub const AA_SYNC_CONFIG_KEY: &str = "aa_rating_sync.config";
pub const AA_SYNC_STATUS_KEY: &str = "aa_rating_sync.status";
pub const AA_SYNC_LEASE_KEY: &str = "aa_rating_sync.lease";
pub const AA_ALIASES_KEY: &str = "aa_rating_sync.aliases";
pub const AA_SNAPSHOT_KEY: &str = "aa_rating_sync.snapshot";
const AA_SNAPSHOT_FILE: &str = "aa-intelligence-index.json";

pub const AA_DEFAULT_SYNC_INTERVAL_HOURS: u32 = 24;
pub const AA_MIN_SYNC_INTERVAL_HOURS: u32 = 1;
pub const AA_MAX_SYNC_INTERVAL_HOURS: u32 = 168;
const AA_MAX_PAGES: usize = 25;
const AA_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const AA_PAGE_TIMEOUT: Duration = Duration::from_secs(25);
const AA_WORKER_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const AA_LEASE_TTL_SECS: i64 = 180;
const AA_FAILURE_RETRY_SECS: i64 = 30 * 60;
/// Cap settings DB snapshot writes below MySQL TEXT's 64 KiB limit when needed;
/// larger snapshots are stored in the local data_dir cache file.
const AA_DB_SNAPSHOT_MAX_BYTES: usize = 56 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AaSyncConfigStored {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_interval_hours")]
    pub sync_interval_hours: u32,
    /// Optional custom endpoint override (defaults to official AA v2 free endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
}

fn default_interval_hours() -> u32 {
    AA_DEFAULT_SYNC_INTERVAL_HOURS
}

impl Default for AaSyncConfigStored {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            sync_interval_hours: AA_DEFAULT_SYNC_INTERVAL_HOURS,
            api_base_url: None,
        }
    }
}

/// Public view of the AA sync configuration (never returns the raw API key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AaSyncConfigView {
    pub enabled: bool,
    pub has_api_key: bool,
    pub api_key_preview: Option<String>,
    pub env_api_key_set: bool,
    pub sync_interval_hours: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
}

/// Input payload for updating AA sync configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAaSyncConfig {
    pub enabled: Option<bool>,
    /// Omit to keep existing key; pass empty string to clear stored key.
    pub api_key: Option<String>,
    pub sync_interval_hours: Option<u32>,
    /// Omit to keep existing override; pass empty string to reset to default.
    pub api_base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AaSyncStatus {
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub index_version: Option<String>,
    pub previous_index_version: Option<String>,
    pub model_count: usize,
    pub raw_model_count: usize,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AaSnapshot {
    pub synced_at: String,
    pub index_version: Option<String>,
    pub models: Vec<AaModelRatingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AaSyncOverview {
    pub config: AaSyncConfigView,
    pub status: AaSyncStatus,
    pub synced_at: Option<String>,
    pub models: Vec<AaModelRatingEntry>,
    pub aliases: Vec<AaModelAlias>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAaModelAlias {
    pub aa_canonical_key: String,
}

#[derive(Debug, Deserialize)]
struct AaApiPagination {
    #[serde(default)]
    total_pages: Option<usize>,
    has_more: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AaApiPageResponse {
    #[serde(default)]
    intelligence_index_version: Option<serde_json::Value>,
    #[serde(default)]
    pagination: Option<AaApiPagination>,
    data: Vec<AaRawModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AaSyncLease {
    expires_at_ms: i64,
}

fn mask_api_key(key: &str) -> Option<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return Some("****".to_string());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("{head}...{tail}"))
}

fn env_aa_api_key() -> Option<String> {
    std::env::var("NYRO_AA_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn effective_api_key(config: &AaSyncConfigStored) -> Option<String> {
    let stored = config.api_key.trim();
    if !stored.is_empty() {
        return Some(stored.to_string());
    }
    env_aa_api_key()
}

fn effective_endpoint(config: &AaSyncConfigStored) -> String {
    config
        .api_base_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(AA_DEFAULT_BASE_URL)
        .to_string()
}

fn build_config_view(config: &AaSyncConfigStored) -> AaSyncConfigView {
    let env_set = env_aa_api_key().is_some();
    let preview = mask_api_key(&config.api_key);
    AaSyncConfigView {
        enabled: config.enabled,
        has_api_key: preview.is_some() || env_set,
        api_key_preview: preview,
        env_api_key_set: env_set,
        sync_interval_hours: config
            .sync_interval_hours
            .clamp(AA_MIN_SYNC_INTERVAL_HOURS, AA_MAX_SYNC_INTERVAL_HOURS),
        api_base_url: config.api_base_url.clone(),
    }
}

fn format_index_version(val: Option<&serde_json::Value>) -> Option<String> {
    match val? {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    }
}

fn snapshot_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(AA_SNAPSHOT_FILE)
}

fn read_snapshot_file(data_dir: &Path) -> Option<AaSnapshot> {
    let content = std::fs::read_to_string(snapshot_file_path(data_dir)).ok()?;
    serde_json::from_str::<AaSnapshot>(&content).ok()
}

fn write_snapshot_file(data_dir: &Path, snapshot: &AaSnapshot) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let json = serde_json::to_string(snapshot)?;
    let tmp = data_dir.join(format!("{AA_SNAPSHOT_FILE}.tmp"));
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, snapshot_file_path(data_dir))?;
    Ok(())
}

fn page_url(base_url: &str, page: usize) -> anyhow::Result<String> {
    let mut parsed =
        reqwest::Url::parse(base_url).map_err(|e| anyhow::anyhow!("Invalid AA API URL: {e}"))?;
    parsed
        .query_pairs_mut()
        .append_pair("page", &page.to_string());
    Ok(parsed.to_string())
}

/// Fetch all pages from Artificial Analysis language models endpoint with strict
/// page/size bounds and completeness verification.
async fn fetch_aa_raw_models(
    http_client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<(Option<String>, Vec<AaRawModel>)> {
    let mut all_models = Vec::new();
    let mut total_bytes: usize = 0;
    let mut index_version: Option<String> = None;

    for page in 1..=AA_MAX_PAGES {
        let url = page_url(base_url, page)?;
        let resp = http_client
            .get(&url)
            .header("x-api-key", api_key)
            .header("accept", "application/json")
            .timeout(AA_PAGE_TIMEOUT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("AA API request failed (page {page}): {e}"))?;

        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("AA API read body failed (page {page}): {e}"))?;

        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
            anyhow::bail!(
                "AA API returned HTTP {} (page {}): {}",
                status.as_u16(),
                page,
                snippet.trim()
            );
        }

        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > AA_MAX_TOTAL_BYTES {
            anyhow::bail!(
                "AA API response exceeded {} byte safety cap",
                AA_MAX_TOTAL_BYTES
            );
        }

        let parsed: AaApiPageResponse = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("Invalid AA API JSON (page {page}): {e}"))?;

        if index_version.is_none() {
            index_version = format_index_version(parsed.intelligence_index_version.as_ref());
        }

        all_models.extend(parsed.data);

        let Some(pagination) = parsed.pagination else {
            // Single unpaginated page response (e.g. legacy/custom mirror).
            break;
        };
        let Some(has_more) = pagination.has_more else {
            anyhow::bail!("AA API response missing pagination.has_more on page {page}");
        };
        if !has_more {
            break;
        }
        if let Some(total) = pagination.total_pages
            && page >= total
        {
            break;
        }
        if page == AA_MAX_PAGES {
            anyhow::bail!("AA API pagination exceeded {AA_MAX_PAGES}-page safety cap");
        }
    }

    if all_models.is_empty() {
        anyhow::bail!("AA API returned 0 models");
    }

    Ok((index_version, all_models))
}

impl AdminService {
    pub(crate) async fn load_aa_sync_config(&self) -> anyhow::Result<AaSyncConfigStored> {
        let raw = self.gw.storage.settings().get(AA_SYNC_CONFIG_KEY).await?;
        match raw {
            Some(json) if !json.trim().is_empty() => {
                Ok(serde_json::from_str(&json).unwrap_or_default())
            }
            _ => Ok(AaSyncConfigStored::default()),
        }
    }

    pub(crate) async fn load_aa_sync_status(&self) -> anyhow::Result<AaSyncStatus> {
        let raw = self.gw.storage.settings().get(AA_SYNC_STATUS_KEY).await?;
        match raw {
            Some(json) if !json.trim().is_empty() => {
                Ok(serde_json::from_str(&json).unwrap_or_default())
            }
            _ => Ok(AaSyncStatus::default()),
        }
    }

    pub(crate) async fn load_aa_snapshot(&self) -> anyhow::Result<Option<AaSnapshot>> {
        // Prefer local file when newer or when DB snapshot is omitted due to size cap,
        // otherwise use DB snapshot so replicas stay synchronized.
        let db_snap = self
            .gw
            .storage
            .settings()
            .get(AA_SNAPSHOT_KEY)
            .await?
            .and_then(|json| serde_json::from_str::<AaSnapshot>(&json).ok());
        let file_snap = read_snapshot_file(&self.gw.config.data_dir);
        Ok(match (db_snap, file_snap) {
            (Some(db), Some(file)) => {
                if file.synced_at > db.synced_at {
                    Some(file)
                } else {
                    Some(db)
                }
            }
            (Some(db), None) => Some(db),
            (None, Some(file)) => Some(file),
            (None, None) => None,
        })
    }

    pub async fn list_aa_aliases(&self) -> anyhow::Result<Vec<AaModelAlias>> {
        let raw = self.gw.storage.settings().get(AA_ALIASES_KEY).await?;
        let mut list: Vec<AaModelAlias> = match raw {
            Some(json) if !json.trim().is_empty() => {
                serde_json::from_str(&json).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        list.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));
        Ok(list)
    }

    pub async fn set_aa_alias(
        &self,
        model_prefix: &str,
        input: SetAaModelAlias,
    ) -> anyhow::Result<AaModelAlias> {
        let canonical = canonical_model_prefix(model_prefix);
        validate_rating_prefix(&canonical)?;
        let target = input.aa_canonical_key.trim().to_lowercase();
        if target.is_empty() {
            return Err(
                ModelRatingError::InvalidInput("AA model key cannot be blank".into()).into(),
            );
        }
        let snapshot = self.load_aa_snapshot().await?;
        let target_exists = snapshot
            .as_ref()
            .is_some_and(|s| s.models.iter().any(|m| m.canonical_key == target));
        if !target_exists {
            return Err(
                ModelRatingError::InvalidInput(format!("Unknown AA model key: {target}")).into(),
            );
        }
        let mut aliases = self.list_aa_aliases().await?;
        aliases.retain(|a| canonical_model_prefix(&a.model_prefix) != canonical);
        let saved = AaModelAlias {
            model_prefix: canonical,
            aa_canonical_key: target,
            updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        aliases.push(saved.clone());
        aliases.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));
        self.gw
            .storage
            .settings()
            .set(AA_ALIASES_KEY, &serde_json::to_string(&aliases)?)
            .await?;
        Ok(saved)
    }

    pub async fn delete_aa_alias(&self, model_prefix: &str) -> anyhow::Result<()> {
        let canonical = canonical_model_prefix(model_prefix);
        validate_rating_prefix(&canonical)?;
        let mut aliases = self.list_aa_aliases().await?;
        aliases.retain(|a| canonical_model_prefix(&a.model_prefix) != canonical);
        self.gw
            .storage
            .settings()
            .set(AA_ALIASES_KEY, &serde_json::to_string(&aliases)?)
            .await?;
        Ok(())
    }

    pub async fn get_aa_rating_sync(&self) -> anyhow::Result<AaSyncOverview> {
        let config = self.load_aa_sync_config().await?;
        let status = self.load_aa_sync_status().await?;
        let snapshot = self.load_aa_snapshot().await?;
        let aliases = self.list_aa_aliases().await?;
        let (synced_at, models) = match snapshot {
            Some(snap) => (Some(snap.synced_at), snap.models),
            None => (None, Vec::new()),
        };
        Ok(AaSyncOverview {
            config: build_config_view(&config),
            status,
            synced_at,
            models,
            aliases,
        })
    }

    pub async fn update_aa_rating_sync_config(
        &self,
        input: UpdateAaSyncConfig,
    ) -> anyhow::Result<AaSyncConfigView> {
        let mut config = self.load_aa_sync_config().await?;
        if let Some(enabled) = input.enabled {
            config.enabled = enabled;
        }
        if let Some(api_key) = input.api_key {
            config.api_key = api_key.trim().to_string();
        }
        if let Some(hours) = input.sync_interval_hours {
            if !(AA_MIN_SYNC_INTERVAL_HOURS..=AA_MAX_SYNC_INTERVAL_HOURS).contains(&hours) {
                return Err(ModelRatingError::InvalidInput(format!(
                    "sync_interval_hours must be between {AA_MIN_SYNC_INTERVAL_HOURS} and {AA_MAX_SYNC_INTERVAL_HOURS}"
                ))
                .into());
            }
            config.sync_interval_hours = hours;
        }
        if let Some(base_url) = input.api_base_url {
            let trimmed = base_url.trim();
            if trimmed.is_empty() {
                config.api_base_url = None;
            } else {
                let parsed = reqwest::Url::parse(trimmed).map_err(|_| {
                    ModelRatingError::InvalidInput(
                        "api_base_url must be a valid HTTP(S) URL".into(),
                    )
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(ModelRatingError::InvalidInput(
                        "api_base_url must use http or https".into(),
                    )
                    .into());
                }
                config.api_base_url = Some(trimmed.to_string());
            }
        }
        self.gw
            .storage
            .settings()
            .set(AA_SYNC_CONFIG_KEY, &serde_json::to_string(&config)?)
            .await?;
        Ok(build_config_view(&config))
    }

    /// Trigger an immediate sync from Artificial Analysis.
    pub async fn trigger_aa_rating_sync(&self) -> anyhow::Result<AaSyncOverview> {
        self.sync_aa_ratings_inner(true).await?;
        self.get_aa_rating_sync().await
    }

    /// Build an in-memory AA lookup index plus synced_at timestamp from current snapshot.
    pub(crate) async fn load_aa_rating_index(
        &self,
    ) -> anyhow::Result<(AaRatingIndex, Option<String>)> {
        let Some(snapshot) = self.load_aa_snapshot().await? else {
            return Ok((AaRatingIndex::default(), None));
        };
        Ok((
            AaRatingIndex::build(snapshot.models),
            Some(snapshot.synced_at),
        ))
    }

    async fn try_acquire_sync_lease(&self, now_ms: i64) -> anyhow::Result<bool> {
        let store = self.gw.storage.settings();
        let existing = store.get(AA_SYNC_LEASE_KEY).await?;
        if let Some(raw) = existing.as_deref()
            && let Ok(lease) = serde_json::from_str::<AaSyncLease>(raw)
            && lease.expires_at_ms > now_ms
        {
            return Ok(false);
        }
        let next = serde_json::to_string(&AaSyncLease {
            expires_at_ms: now_ms + AA_LEASE_TTL_SECS * 1000,
        })?;
        match store
            .compare_and_set(AA_SYNC_LEASE_KEY, existing.as_deref(), &next)
            .await
        {
            Ok(acquired) => Ok(acquired),
            Err(_) => Ok(true), // Storage backends without CAS still allow single-node sync.
        }
    }

    async fn release_sync_lease(&self) {
        let _ = self
            .gw
            .storage
            .settings()
            .set(AA_SYNC_LEASE_KEY, "{\"expires_at_ms\":0}")
            .await;
    }

    pub(crate) async fn sync_aa_ratings_inner(&self, manual: bool) -> anyhow::Result<bool> {
        let config = self.load_aa_sync_config().await?;
        if !manual && !config.enabled {
            return Ok(false);
        }
        let Some(api_key) = effective_api_key(&config) else {
            if manual {
                return Err(ModelRatingError::InvalidInput(
                    "Artificial Analysis API key is not configured".into(),
                )
                .into());
            }
            return Ok(false);
        };

        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let mut status = self.load_aa_sync_status().await?;

        if !manual {
            let interval_secs = i64::from(
                config
                    .sync_interval_hours
                    .clamp(AA_MIN_SYNC_INTERVAL_HOURS, AA_MAX_SYNC_INTERVAL_HOURS),
            ) * 3600;
            if let Some(last_ok) = status
                .last_success_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                && now
                    .signed_duration_since(last_ok.with_timezone(&Utc))
                    .num_seconds()
                    < interval_secs
            {
                return Ok(false);
            }
            let backoff_secs =
                AA_FAILURE_RETRY_SECS.saturating_mul(1i64 << status.consecutive_failures.min(3));
            if let Some(last_try) = status
                .last_attempt_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                && now
                    .signed_duration_since(last_try.with_timezone(&Utc))
                    .num_seconds()
                    < backoff_secs
            {
                return Ok(false);
            }
        }

        if !self.try_acquire_sync_lease(now_ms).await? {
            if manual {
                return Err(ModelRatingError::InvalidInput(
                    "An Artificial Analysis sync is already in progress".into(),
                )
                .into());
            }
            return Ok(false);
        }

        let attempt_at = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        status.last_attempt_at = Some(attempt_at.clone());
        let _ = self
            .gw
            .storage
            .settings()
            .set(AA_SYNC_STATUS_KEY, &serde_json::to_string(&status)?)
            .await;

        let endpoint = effective_endpoint(&config);
        let fetch_result = fetch_aa_raw_models(&self.gw.http_client, &endpoint, &api_key).await;

        let outcome = match fetch_result {
            Ok((index_version, raw_models)) => {
                let raw_count = raw_models.len();
                let consolidated = consolidate_aa_models(&raw_models);
                if consolidated.is_empty() {
                    Err(anyhow::anyhow!(
                        "AA API returned no models with valid Intelligence Index scores"
                    ))
                } else {
                    let synced_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    let snapshot = AaSnapshot {
                        synced_at: synced_at.clone(),
                        index_version: index_version.clone(),
                        models: consolidated,
                    };
                    let serialized = serde_json::to_string(&snapshot)?;
                    if let Err(err) = write_snapshot_file(&self.gw.config.data_dir, &snapshot) {
                        tracing::warn!("failed to write AA snapshot file: {err}");
                    }
                    if serialized.len() <= AA_DB_SNAPSHOT_MAX_BYTES {
                        self.gw
                            .storage
                            .settings()
                            .set(AA_SNAPSHOT_KEY, &serialized)
                            .await?;
                    } else {
                        tracing::info!(
                            bytes = serialized.len(),
                            "AA snapshot kept in local data_dir cache"
                        );
                    }
                    if status.index_version.is_some() && status.index_version != index_version {
                        status.previous_index_version = status.index_version.clone();
                    }
                    status.index_version = index_version;
                    status.last_success_at = Some(synced_at);
                    status.last_error = None;
                    status.model_count = snapshot.models.len();
                    status.raw_model_count = raw_count;
                    status.consecutive_failures = 0;
                    self.gw
                        .storage
                        .settings()
                        .set(AA_SYNC_STATUS_KEY, &serde_json::to_string(&status)?)
                        .await?;
                    Ok(true)
                }
            }
            Err(err) => Err(err),
        };

        if let Err(ref err) = outcome {
            status.last_error = Some(err.to_string());
            status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            let _ = self
                .gw
                .storage
                .settings()
                .set(AA_SYNC_STATUS_KEY, &serde_json::to_string(&status)?)
                .await;
        }

        self.release_sync_lease().await;
        outcome
    }
}

/// Background worker that checks periodically whether an AA Intelligence Index sync is due.
pub(crate) async fn run_aa_rating_sync_worker(gw: Gateway) {
    // Short initial delay so startup stays fast.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let mut interval = tokio::time::interval(AA_WORKER_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        match gw.admin().sync_aa_ratings_inner(false).await {
            Ok(true) => tracing::info!("AA Intelligence Index ratings synced"),
            Ok(false) => {}
            Err(err) => tracing::warn!("AA Intelligence Index sync failed: {err}"),
        }
    }
}
