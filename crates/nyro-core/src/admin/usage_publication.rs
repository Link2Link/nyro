//! Read-only external usage entries. Configured providers remain the routing and
//! CRUD identity; publication is a projection of one already-fetched snapshot.

use std::collections::BTreeSet;

use serde::Serialize;

use super::{
    AdminService, ProviderScheduling, ProviderSchedulingStatus, ProviderUsage,
    ProviderUsageListItem, ProviderUsageQueryStatus, ProviderUsageTier,
};
use crate::quota_buckets::{QuotaBucket, opaque_quota_id};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PublishedUsageEntry {
    pub kind: &'static str,
    pub source_provider_ref: String,
    pub bucket_ref: String,
    pub binding_status: String,
    pub stale: bool,
}

/// Same flat envelope consumed by existing usage clients, with optional typed
/// provenance. Published IDs are never accepted as provider configuration IDs.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PublishedProviderUsageListItem {
    pub provider_id: String,
    pub provider_name: String,
    pub is_enabled: bool,
    pub status: ProviderUsageQueryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_entry: Option<PublishedUsageEntry>,
}

impl From<ProviderUsageListItem> for PublishedProviderUsageListItem {
    fn from(item: ProviderUsageListItem) -> Self {
        Self {
            provider_id: item.provider_id,
            provider_name: item.provider_name,
            is_enabled: item.is_enabled,
            status: item.status,
            error: item.error,
            usage: item.usage,
            usage_entry: None,
        }
    }
}

pub(crate) fn published_bucket_id(provider_id: &str, bucket: &QuotaBucket) -> String {
    opaque_quota_id(
        "published_usage_v1",
        &[provider_id, &bucket.scope_id, &bucket.id],
    )
}

/// Canonical windows, in display order. Conflicting duplicates are withheld,
/// rather than selecting an optimistic/max observation or emitting duplicate UI
/// keys. Unknown-duration observations never acquire a fabricated window name.
pub(crate) fn canonical_bucket_tiers(tiers: &[ProviderUsageTier]) -> Vec<ProviderUsageTier> {
    ["five_hour", "daily", "weekly_limit", "monthly"]
        .into_iter()
        .filter_map(|name| {
            let mut matching = tiers.iter().filter(|tier| tier.name == name);
            let first = matching.next()?;
            (first.used_percent.is_finite() && matching.all(|tier| tier == first))
                .then(|| first.clone())
        })
        .collect()
}

pub(crate) fn bucket_tiers_conflict(tiers: &[ProviderUsageTier]) -> bool {
    ["five_hour", "daily", "weekly_limit", "monthly"]
        .into_iter()
        .any(|name| {
            let mut matching = tiers.iter().filter(|tier| tier.name == name);
            matching.next().is_some_and(|first| {
                !first.used_percent.is_finite() || matching.any(|tier| tier != first)
            })
        })
}

/// AND admission constraints. Alternative providers/buckets must not use this
/// helper: those are OR choices and are aggregated separately for each route.
pub(crate) fn effective_bucket_scheduling(
    usage: &ProviderUsage,
    bucket: &QuotaBucket,
) -> ProviderScheduling {
    let mut scheduling = bucket.scheduling.clone();
    for constraint in usage.quota_buckets.iter().filter(|constraint| {
        !constraint.is_primary() && bucket.constraint_ids.contains(&constraint.id)
    }) {
        if constraint.scheduling.status == ProviderSchedulingStatus::QuotaExhausted {
            and_blocker(&mut scheduling, &constraint.scheduling);
        }
    }
    // The provider summary can mean "some model exhausted". It is NOT a global
    // blocker. Only explicit owner/auth unavailability applies to every bucket.
    if usage.is_available == Some(false)
        || matches!(
            usage.scheduling.reason.as_deref(),
            Some(
                "account_unavailable"
                    | "no_available_account"
                    | "auth_error"
                    | "authentication_failed"
            )
        )
    {
        let mut owner = usage.scheduling.clone();
        owner.status = ProviderSchedulingStatus::QuotaExhausted;
        owner.reason = Some("account_unavailable".into());
        and_blocker(&mut scheduling, &owner);
    }
    scheduling
}

fn and_blocker(scheduling: &mut ProviderScheduling, blocker: &ProviderScheduling) {
    if scheduling.status != ProviderSchedulingStatus::QuotaExhausted {
        *scheduling = blocker.clone();
        return;
    }
    let tiers = scheduling
        .blocking_tiers
        .iter()
        .chain(&blocker.blocking_tiers)
        .cloned()
        .collect::<BTreeSet<_>>();
    scheduling.blocking_tiers = tiers.into_iter().collect();
    scheduling.reset_at = match (&scheduling.reset_at, &blocker.reset_at) {
        (Some(left), Some(right)) => chrono::DateTime::parse_from_rfc3339(left)
            .ok()
            .zip(chrono::DateTime::parse_from_rfc3339(right).ok())
            .map(|(left, right)| left.max(right).to_rfc3339()),
        _ => None,
    };
}

pub(crate) fn bucket_usage(usage: &ProviderUsage, bucket: &QuotaBucket) -> ProviderUsage {
    ProviderUsage {
        provider_id: published_bucket_id(&usage.provider_id, bucket),
        kind: usage.kind.clone(),
        site: usage.site.clone(),
        level: usage.level.clone(),
        tiers: canonical_bucket_tiers(&bucket.tiers),
        balances: Vec::new(),
        spends: Vec::new(),
        is_available: usage.is_available,
        scheduling: effective_bucket_scheduling(usage, bucket),
        queried_at: bucket.queried_at.clone(),
        api_price: bucket.api_price.clone(),
        account_pool: None,
        pool_quota: None,
        upstream: None,
        quota_buckets: Vec::new(),
    }
}

/// Expand primary buckets only, never shared sidecars or a duplicate aggregate
/// parent. Even a single remaining primary bucket retains its published ID.
pub fn published_usage_items(
    items: Vec<ProviderUsageListItem>,
) -> Vec<PublishedProviderUsageListItem> {
    items
        .into_iter()
        .flat_map(|item| {
            let Some(usage) = item.usage.as_ref() else {
                return vec![item.into()];
            };
            if usage.quota_buckets.is_empty()
                || usage
                    .quota_buckets
                    .iter()
                    .all(|bucket| bucket.source == "forwarded_route")
            {
                // Route-admission buckets have no local accounting quota. Preserve
                // the relay row: DSH expands its safe upstream.providers[] itself,
                // using the relay ID plus remote bucket ID as the composite key.
                return vec![item.into()];
            }
            let primary = usage
                .quota_buckets
                .iter()
                .filter(|bucket| {
                    bucket.is_primary()
                        && bucket.is_resolved()
                        && bucket.source != "catalog"
                        && !bucket_tiers_conflict(&bucket.tiers)
                })
                .collect::<Vec<_>>();
            let unresolved = primary.is_empty()
                || usage.quota_buckets.iter().any(|bucket| {
                    bucket.is_primary()
                        && (!bucket.is_resolved()
                            || bucket.source == "catalog"
                            || bucket_tiers_conflict(&bucket.tiers))
                });
            let mut rows = primary
                .into_iter()
                .map(|bucket| {
                    let projected = bucket_usage(usage, bucket);
                    PublishedProviderUsageListItem {
                        provider_id: projected.provider_id.clone(),
                        provider_name: format!("{} · {}", item.provider_name, bucket.label),
                        is_enabled: item.is_enabled,
                        status: item.status,
                        error: item.error.clone(),
                        usage: Some(projected),
                        usage_entry: Some(PublishedUsageEntry {
                            kind: "quota_bucket",
                            source_provider_ref: item.provider_id.clone(),
                            bucket_ref: bucket.id.clone(),
                            binding_status: bucket.binding.status.clone(),
                            stale: bucket.stale,
                        }),
                    }
                })
                .collect::<Vec<_>>();
            if unresolved {
                rows.push(PublishedProviderUsageListItem {
                    provider_id: opaque_quota_id("published_unresolved_v1", &[&item.provider_id]),
                    provider_name: format!("{} · Unresolved quota", item.provider_name),
                    is_enabled: item.is_enabled,
                    status: ProviderUsageQueryStatus::Error,
                    error: Some(
                        "Quota associations are unresolved; inspect the structured provider view"
                            .into(),
                    ),
                    usage: None,
                    usage_entry: None,
                });
            }
            rows
        })
        .collect()
}

impl AdminService {
    pub async fn list_published_provider_usage(
        &self,
    ) -> anyhow::Result<Vec<PublishedProviderUsageListItem>> {
        Ok(published_usage_items(self.list_provider_usage().await?))
    }

    pub async fn get_provider_quota_buckets_config(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<crate::quota_buckets::QuotaBucketConfig> {
        self.get_provider(provider_id).await?;
        match self
            .gw
            .storage
            .settings()
            .get(&crate::quota_buckets::quota_bucket_config_key(provider_id))
            .await?
        {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Default::default()),
        }
    }

    /// The supplied revision is the last-read revision, not the desired next
    /// revision. Serialize with refreshes so a late observation cannot publish a
    /// half-applied mapping. No vendor/network requests are made by this edit.
    pub async fn set_provider_quota_buckets_config(
        &self,
        provider_id: &str,
        mut config: crate::quota_buckets::QuotaBucketConfig,
    ) -> anyhow::Result<crate::quota_buckets::QuotaBucketConfig> {
        use crate::quota_buckets::{apply_quota_bucket_config, quota_bucket_config_key};
        let lock = self.gw.quota_registry.refresh_lock(provider_id);
        let _guard = lock.lock().await;
        let previous = self.get_provider_quota_buckets_config(provider_id).await?;
        anyhow::ensure!(
            previous.revision == config.revision,
            "QUOTA_BUCKET_REVISION_CONFLICT: reload the quota bucket configuration before saving"
        );
        config.revision = previous
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Quota bucket configuration revision overflow"))?;
        // A configured aggregate is an output, not another discovered resource.
        // Keep the original editor windows and true shared constraints only.
        let discovered = self.gw.quota_registry.raw_bucket_snapshot(provider_id);
        let configured = apply_quota_bucket_config(&discovered, &config)
            .map_err(|message| anyhow::anyhow!(message))?;
        self.gw
            .storage
            .settings()
            .set(
                &quota_bucket_config_key(provider_id),
                &serde_json::to_string(&config)?,
            )
            .await?;
        // Refresh is queued, never performed per published row. Invalidate the
        // forwarding representation so clients cannot observe an old mapping.
        self.gw.usage_snapshots.invalidate(provider_id);
        self.gw
            .quota_registry
            .replace_bucket_mapping(provider_id, &configured);
        // Persist the replacement before returning or queueing discovery. A
        // restart before the next refresh must not resurrect retired aggregate
        // mappings from a fence written under the previous configuration.
        self.gw
            .storage
            .settings()
            .set(
                &crate::quota_buckets::quota_bucket_fence_key(provider_id),
                &serde_json::to_string(
                    &self.gw.quota_registry.persistent_bucket_fences(provider_id),
                )?,
            )
            .await?;
        self.gw.quota_registry.request_refresh(provider_id);
        self.bump_config_epoch().await?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota_buckets::QuotaBucketBinding;

    fn bucket(id: &str, label: &str) -> QuotaBucket {
        QuotaBucket {
            id: id.into(),
            scope_id: "opaque-owner".into(),
            label: label.into(),
            models: vec![id.into()],
            binding: QuotaBucketBinding {
                status: "resolved".into(),
                source: "official".into(),
                revision: 1,
            },
            tiers: vec![
                ProviderUsageTier {
                    name: "weekly_limit".into(),
                    used_percent: 30.0,
                    resets_at: None,
                },
                ProviderUsageTier {
                    name: "five_hour".into(),
                    used_percent: 10.0,
                    resets_at: None,
                },
            ],
            constraint_ids: Vec::new(),
            source: "enforced_summary".into(),
            queried_at: "2026-11-05T08:00:00Z".into(),
            stale: false,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        }
    }

    fn item() -> ProviderUsageListItem {
        ProviderUsageListItem {
            provider_id: "real-provider".into(),
            provider_name: "Google Pro".into(),
            is_enabled: true,
            status: ProviderUsageQueryStatus::Ok,
            error: None,
            usage: Some(ProviderUsage {
                provider_id: "real-provider".into(),
                kind: "google_subscription".into(),
                site: "global".into(),
                level: Some("pro".into()),
                tiers: Vec::new(),
                balances: vec![super::super::ProviderUsageBalance {
                    currency: "USD".into(),
                    total: 123.0,
                    granted: 0.0,
                    topped_up: 123.0,
                }],
                spends: Vec::new(),
                is_available: Some(true),
                scheduling: ProviderScheduling::default(),
                queried_at: "2026-11-05T08:00:00Z".into(),
                api_price: None,
                account_pool: None,
                pool_quota: None,
                upstream: None,
                quota_buckets: vec![bucket("flash", "Flash"), bucket("argon", "Argon")],
            }),
        }
    }

    #[test]
    fn published_rows_are_flat_stable_unique_and_not_parent_money() {
        let original = item();
        let rows = published_usage_items(vec![original.clone()]);
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].provider_id, rows[1].provider_id);
        assert_ne!(rows[0].provider_id, original.provider_id);
        assert_eq!(rows[0].provider_name, "Google Pro · Flash");
        for row in &rows {
            let usage = row.usage.as_ref().unwrap();
            assert_eq!(row.provider_id, usage.provider_id);
            assert_eq!(row.status, ProviderUsageQueryStatus::Ok);
            assert!(usage.balances.is_empty() && usage.spends.is_empty());
            assert!(usage.quota_buckets.is_empty() && usage.account_pool.is_none());
            assert_eq!(
                usage
                    .tiers
                    .iter()
                    .map(|tier| tier.name.as_str())
                    .collect::<Vec<_>>(),
                ["five_hour", "weekly_limit"]
            );
        }
        let mut changed = original.clone();
        let usage = changed.usage.as_mut().unwrap();
        usage.quota_buckets.truncate(1);
        usage.quota_buckets[0].label = "Renamed".into();
        usage.quota_buckets[0].tiers[0].used_percent = 99.0;
        usage.quota_buckets[0].queried_at = "2030-01-01T00:00:00Z".into();
        assert_eq!(
            published_usage_items(vec![changed.clone()])[0].provider_id,
            rows[0].provider_id
        );
        changed.usage.as_mut().unwrap().quota_buckets[0].scope_id = "new-owner".into();
        assert_ne!(
            published_usage_items(vec![changed])[0].provider_id,
            rows[0].provider_id
        );
        let wire = serde_json::to_value(&rows).unwrap();
        assert_eq!(wire[0]["usage_entry"]["kind"], "quota_bucket");
    }

    #[test]
    fn shared_and_owner_blocks_affect_every_primary_not_extra_rows() {
        let mut source = item();
        let usage = source.usage.as_mut().unwrap();
        let mut shared = bucket("global", "Global");
        shared.binding.status = "shared_constraint".into();
        shared.scheduling.status = ProviderSchedulingStatus::QuotaExhausted;
        shared.scheduling.reason = Some("shared_constraint".into());
        for bucket in &mut usage.quota_buckets {
            bucket.constraint_ids.push(shared.id.clone());
        }
        usage.quota_buckets.push(shared);
        let rows = published_usage_items(vec![source.clone()]);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.usage.as_ref().unwrap().scheduling.status
                    == ProviderSchedulingStatus::QuotaExhausted)
        );
        source.usage.as_mut().unwrap().quota_buckets.pop();
        source.usage.as_mut().unwrap().is_available = Some(false);
        assert!(
            published_usage_items(vec![source]).iter().all(|row| row
                .usage
                .as_ref()
                .unwrap()
                .scheduling
                .status
                == ProviderSchedulingStatus::QuotaExhausted)
        );
    }

    #[test]
    fn unresolved_is_one_diagnostic_and_relay_keeps_composite_upstream_cards() {
        let mut source = item();
        for bucket in &mut source.usage.as_mut().unwrap().quota_buckets {
            bucket.binding.status = "unresolved".into();
            bucket.models.clear();
        }
        let rows = published_usage_items(vec![source]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ProviderUsageQueryStatus::Error);
        assert!(rows[0].usage.is_none());
        let mut relay = item();
        let usage = relay.usage.as_mut().unwrap();
        usage.kind = "nyro_gateway".into();
        usage.upstream =
            Some(serde_json::json!({"object":"nyro.usage","providers":[{"id":"remote-bucket"}]}));
        for bucket in &mut usage.quota_buckets {
            bucket.source = "forwarded_route".into();
        }
        let rows = published_usage_items(vec![relay.clone()]);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            serde_json::to_value(&rows[0]).unwrap(),
            serde_json::to_value(&relay).unwrap()
        );
    }

    #[test]
    fn catalog_only_observations_are_one_diagnostic_not_schedulable_cards() {
        let mut source = item();
        for bucket in &mut source.usage.as_mut().unwrap().quota_buckets {
            bucket.source = "catalog".into();
            bucket.binding.source = "catalog_exact_model_v1".into();
            bucket.scheduling.reason = Some("unknown_mapping".into());
        }
        let rows = published_usage_items(vec![source.clone()]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ProviderUsageQueryStatus::Error);
        assert!(rows[0].usage.is_none());
        source.usage.as_mut().unwrap().quota_buckets[0].source = "enforced_summary".into();
        let rows = published_usage_items(vec![source]);
        assert_eq!(
            rows.len(),
            2,
            "one bound card and one diagnostic, not several catalog variants"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.status == ProviderUsageQueryStatus::Ok)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn config_writes_validate_resources_serialize_revision_and_preserve_atomicity() {
        use super::super::CreateProvider;
        use crate::quota_buckets::{ManualQuotaBucketBinding, QuotaBucketConfig};
        let dir = tempfile::tempdir().unwrap();
        let (gw, _) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: dir.path().into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let admin = gw.admin();
        let id = admin
            .create_provider(CreateProvider {
                keys: Vec::new(),
                name: "Offline Google".into(),
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: "https://example.invalid".into(),
                protocol_mode: "fixed".into(),
                protocol_endpoints: Vec::new(),
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "offline".into(),
                auth_mode: "apikey".into(),
                use_proxy: false,
                fast_mode: false,
            })
            .await
            .unwrap()
            .id;
        assert_eq!(
            admin
                .get_provider_quota_buckets_config(&id)
                .await
                .unwrap()
                .revision,
            0
        );
        let mut discovered = bucket("resource", "Resource");
        discovered.models.clear();
        discovered.binding.status = "unresolved".into();
        gw.quota_registry
            .observe_buckets(&id, &[discovered], Some(true));
        let config = QuotaBucketConfig {
            revision: 0,
            bindings: vec![ManualQuotaBucketBinding {
                id: "flash-primary".into(),
                scope_id: "opaque-owner".into(),
                label: "Flash".into(),
                models: vec!["flash-wire".into()],
                constraint_ids: vec!["resource".into()],
            }],
        };
        let saved = admin
            .set_provider_quota_buckets_config(&id, config.clone())
            .await
            .unwrap();
        assert_eq!(saved.revision, 1);
        let saved_fences = serde_json::from_str::<serde_json::Value>(
            &admin
                .get_setting(&crate::quota_buckets::quota_bucket_fence_key(&id))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved_fences,
            serde_json::to_value(gw.quota_registry.persistent_bucket_fences(&id)).unwrap()
        );
        let aggregate_id = gw
            .quota_registry
            .model_snapshot(&id, "flash-wire")
            .unwrap()
            .id;
        assert!(
            saved_fences
                .as_array()
                .unwrap()
                .iter()
                .any(|fence| fence["id"] == aggregate_id)
        );
        assert_eq!(
            gw.quota_registry
                .model_snapshot(&id, "flash-wire")
                .unwrap()
                .label,
            "Flash"
        );
        let stale = admin
            .set_provider_quota_buckets_config(&id, config)
            .await
            .unwrap_err();
        assert!(
            stale
                .to_string()
                .starts_with("QUOTA_BUCKET_REVISION_CONFLICT:")
        );
        let mut invalid = saved.clone();
        invalid.bindings[0].constraint_ids = vec!["unknown-or-hidden".into()];
        assert!(
            admin
                .set_provider_quota_buckets_config(&id, invalid)
                .await
                .is_err()
        );
        assert_eq!(
            admin.get_provider_quota_buckets_config(&id).await.unwrap(),
            saved
        );
        let mut renamed = saved;
        renamed.bindings[0].label = "Renamed Flash".into();
        let renamed = admin
            .set_provider_quota_buckets_config(&id, renamed)
            .await
            .unwrap();
        assert_eq!(renamed.revision, 2);
        let renamed_fences = serde_json::from_str::<serde_json::Value>(
            &admin
                .get_setting(&crate::quota_buckets::quota_bucket_fence_key(&id))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            renamed_fences,
            serde_json::to_value(gw.quota_registry.persistent_bucket_fences(&id)).unwrap()
        );
        assert_eq!(
            renamed_fences
                .as_array()
                .unwrap()
                .iter()
                .find(|fence| fence["id"] == aggregate_id)
                .unwrap()["label"],
            "Renamed Flash"
        );
        assert_eq!(
            gw.quota_registry
                .model_snapshot(&id, "flash-wire")
                .unwrap()
                .label,
            "Renamed Flash"
        );
        let cleared = admin
            .set_provider_quota_buckets_config(
                &id,
                QuotaBucketConfig {
                    revision: 2,
                    bindings: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(cleared.revision, 3);
        assert!(
            gw.quota_registry
                .model_snapshot(&id, "flash-wire")
                .is_none()
        );
        let cleared_raw = admin
            .get_setting(&crate::quota_buckets::quota_bucket_fence_key(&id))
            .await
            .unwrap()
            .unwrap();
        let cleared_fences: serde_json::Value = serde_json::from_str(&cleared_raw).unwrap();
        assert_eq!(
            cleared_fences,
            serde_json::to_value(gw.quota_registry.persistent_bucket_fences(&id)).unwrap()
        );
        assert!(
            !cleared_fences
                .as_array()
                .unwrap()
                .iter()
                .any(|fence| fence["id"] == aggregate_id)
        );
        let restored = crate::router::quota::ProviderQuotaRegistry::new();
        restored.restore_bucket_fences(
            &id,
            &serde_json::from_str::<Vec<crate::quota_buckets::PersistentQuotaBucketFence>>(
                &cleared_raw,
            )
            .unwrap(),
        );
        assert!(
            restored.model_snapshot(&id, "flash-wire").is_none(),
            "restart must not resurrect the cleared mapping"
        );
    }

    #[test]
    fn conflicting_window_duplicates_are_withheld_and_legacy_rows_unchanged() {
        let mut tiers = bucket("flash", "Flash").tiers;
        tiers.push(tiers[0].clone());
        assert_eq!(canonical_bucket_tiers(&tiers).len(), 2);
        tiers.last_mut().unwrap().used_percent = 90.0;
        assert_eq!(canonical_bucket_tiers(&tiers).len(), 1);
        let mut source = item();
        source.usage.as_mut().unwrap().quota_buckets.clear();
        let expected = serde_json::to_value(&source).unwrap();
        assert_eq!(
            serde_json::to_value(&published_usage_items(vec![source])[0]).unwrap(),
            expected
        );
    }
}
