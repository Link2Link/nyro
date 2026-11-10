use super::*;

/// Settings key used to signal config changes to other replicas.
pub const CONFIG_EPOCH_KEY: &str = "config_epoch";

/// Public setting names use ASCII identifiers. MySQL's case/accent-insensitive
/// name collation must not let an alternate spelling overwrite an internal gate.
pub(super) fn valid_setting_identifier(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic())
}

pub(super) fn internal_setting_key(key: &str) -> bool {
    [
        "claude_usage_cooldown.",
        "provider_account_fence:",
        "quota_bucket_owner.",
        "quota_bucket_fence.",
    ]
    .iter()
    .any(|prefix| {
        key.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

impl AdminService {
    // ── Settings ──

    pub async fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.gw.storage.settings().get(key).await
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_setting_identifier(key),
            "Setting identifiers must be printable ASCII without spaces; values may contain Unicode"
        );
        anyhow::ensure!(
            !internal_setting_key(key),
            "Account and quota identity fences are internal settings"
        );
        if let Some(provider_id) = key.strip_prefix("quota_buckets.") {
            let config: crate::quota_buckets::QuotaBucketConfig =
                serde_json::from_str(value).context("Invalid quota bucket configuration")?;
            self.set_provider_quota_buckets_config(provider_id, config)
                .await?;
            return Ok(());
        }
        if let Some(provider_id) = key.strip_prefix(super::account_pool::POOL_SETTING_PREFIX) {
            let config: DevinPoolConfig =
                serde_json::from_str(value).context("Invalid account pool configuration")?;
            self.configure_provider_account_pool(provider_id, config)
                .await?;
            return Ok(());
        }
        super::token_price::validate_setting(key, value)?;
        self.gw.storage.settings().set(key, value).await
    }

    /// Explicit owner/credential replacement invalidates the old entitlement,
    /// unlike routine token refresh or a temporary quota-query failure.
    pub(super) async fn clear_provider_quota_identity(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<()> {
        // The same lock spans quota observation and persistent publication.
        // A late old-owner refresh must finish before its fence is cleared.
        let lock = self.gw.quota_registry.refresh_lock(provider_id);
        let _guard = lock.lock().await;
        let store = self.gw.storage.settings();
        store
            .set(&format!("quota_bucket_owner.{provider_id}"), "")
            .await?;
        store
            .set(
                &crate::quota_buckets::quota_bucket_fence_key(provider_id),
                "[]",
            )
            .await?;
        self.gw.quota_registry.invalidate(provider_id);
        self.gw.usage_snapshots.invalidate(provider_id);
        Ok(())
    }

    /// Increment the shared `config_epoch` counter so other replicas know they
    /// must reload their in-memory `model_cache`.
    pub(super) async fn bump_config_epoch(&self) -> anyhow::Result<()> {
        let store = self.gw.storage.settings();
        let current: i64 = store
            .get(CONFIG_EPOCH_KEY)
            .await?
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        store
            .set(CONFIG_EPOCH_KEY, &(current + 1).to_string())
            .await
    }
}
