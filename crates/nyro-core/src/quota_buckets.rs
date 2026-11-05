//! Model-aware, owner-scoped quota observations shared by routing and admin views.
//!
//! Bucket identity is independent of utilization, reset timestamps and labels.
//! `models` is an exact binding, never a price/family/prefix matcher. An unresolved
//! observation remains visible but is not an invented routing allowance.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::admin::{ProviderApiPriceStats, ProviderUsageTier};
use crate::router::quota::ProviderScheduling;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaBucketBinding {
    /// `resolved`, `unresolved`, `conflict`, or `shared_constraint`.
    pub status: String,
    /// `official`, `google_exact_model_v1`, `catalog_exact_model_v1`, `configured`.
    pub source: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuotaBucket {
    pub id: String,
    /// Opaque owner/authority/project hash. Never an email or raw project ID.
    pub scope_id: String,
    pub label: String,
    pub models: Vec<String>,
    pub binding: QuotaBucketBinding,
    pub tiers: Vec<ProviderUsageTier>,
    /// Additional shared constraints, represented once as sidecar buckets with
    /// binding.status = `shared_constraint`, not as independent capacity.
    pub constraint_ids: Vec<String>,
    pub source: String,
    pub queried_at: String,
    pub stale: bool,
    pub scheduling: ProviderScheduling,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_price: Option<ProviderApiPriceStats>,
}

impl QuotaBucket {
    pub fn is_primary(&self) -> bool {
        !matches!(
            self.binding.status.as_str(),
            "shared_constraint" | "window_constraint"
        )
    }

    /// Shared does not imply global. Only an explicit upstream all-model
    /// declaration grants this versioned marker; ordinary shared resources
    /// apply solely through a primary bucket's exact constraint references.
    pub fn is_global_constraint(&self) -> bool {
        self.binding.status == "shared_constraint"
            && self.binding.source == "official_all_models_v1"
    }

    pub fn is_resolved(&self) -> bool {
        self.binding.status == "resolved" && !self.models.is_empty()
    }

    pub fn binds_model(&self, model: &str) -> bool {
        self.is_resolved() && self.models.iter().any(|id| id == model)
    }
}

/// Length-delimited hashing prevents component ambiguity and leaking upstream
/// owner/resource identifiers. Callers supply stable identity, never access tokens.
pub fn opaque_quota_id(namespace: &str, components: &[&str]) -> String {
    let mut digest = Sha256::new();
    for component in std::iter::once(namespace).chain(components.iter().copied()) {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    format!("qb_{}", hex::encode(digest.finalize()))
}

/// Google may initially expose identity only in OAuth metadata, then promote
/// the same email into subject_id on refresh. Normalize both paths identically.
/// A truly absent identity uses only the persisted local fallback, not a token.
pub fn google_quota_owner_id(
    credential: &crate::auth::types::StoredCredential,
    legacy_owner: Option<&str>,
) -> Option<String> {
    credential
        .subject_id
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .or_else(|| {
            credential
                .meta
                .get("email")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|owner| !owner.is_empty())
        })
        .map(|owner| {
            if owner.contains('@') {
                owner.to_ascii_lowercase()
            } else {
                owner.to_owned()
            }
        })
        .or_else(|| {
            legacy_owner
                .map(str::trim)
                .filter(|owner| !owner.is_empty())
                .map(str::to_owned)
        })
}

pub fn google_quota_scope_id(
    provider_id: &str,
    authority: &str,
    owner: &str,
    project: &str,
) -> String {
    opaque_quota_id(
        "google_owner_scope_v1",
        &[provider_id, owner, authority, project],
    )
}

/// Server-owned attempt attribution. Never populated from client JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaAttribution {
    pub version: u32,
    pub bucket_id: String,
    pub scope_id: String,
    pub backend_model: String,
    pub wire_model: String,
    pub source: String,
    pub revision: u64,
    pub dispatched_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuotaBucketConfig {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub bindings: Vec<ManualQuotaBucketBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualQuotaBucketBinding {
    /// Operator-chosen immutable local key, not a mutable label.
    pub id: String,
    pub scope_id: String,
    pub label: String,
    pub models: Vec<String>,
    /// Discovered opaque resource IDs, including every applicable window.
    pub constraint_ids: Vec<String>,
}

pub fn quota_bucket_config_key(provider_id: &str) -> String {
    format!("quota_buckets.{provider_id}")
}

/// Group discovered window resources only on explicit operator instruction.
/// Sidecar resources remain visible for the editor but are not capacity rows.
pub fn apply_quota_bucket_config(
    discovered: &[QuotaBucket],
    config: &QuotaBucketConfig,
) -> Result<Vec<QuotaBucket>, String> {
    let mut result = discovered.to_vec();
    let mut ids = std::collections::HashSet::new();
    let mut models = std::collections::HashSet::new();
    let mut consumed = std::collections::HashSet::new();
    for binding in &config.bindings {
        if binding.id.trim().is_empty()
            || binding.label.trim().is_empty()
            || !ids.insert(&binding.id)
        {
            return Err(
                "Quota bindings require unique stable IDs and nonempty English labels".to_owned(),
            );
        }
        if binding.models.is_empty() || binding.constraint_ids.is_empty() {
            return Err(
                "Quota bindings require exact models and discovered constraints".to_owned(),
            );
        }
        for model in &binding.models {
            if model.trim().is_empty() || model.trim() != model || !models.insert(model.clone()) {
                return Err(
                    "Quota bindings contain empty or overlapping model selectors".to_owned(),
                );
            }
        }
        let mut binding_resources = std::collections::HashSet::new();
        let mut tiers = Vec::new();
        let mut names = std::collections::HashSet::new();
        let mut shared_ids = Vec::new();
        let mut stale = false;
        let mut queried_at: Option<String> = None;
        for id in &binding.constraint_ids {
            let resource = discovered
                .iter()
                .find(|bucket| bucket.id == *id && bucket.scope_id == binding.scope_id)
                .ok_or_else(|| {
                    "Quota binding references an unknown or retired owner-scoped resource"
                        .to_owned()
                })?;
            if resource.source != "enforced_summary" {
                return Err("Manual bindings require enforced discovered quota resources; catalog data cannot become authoritative".to_owned());
            }
            if resource.binding.source == "official"
                && !resource.models.is_empty()
                && !binding
                    .models
                    .iter()
                    .all(|model| resource.models.contains(model))
            {
                return Err("Quota binding conflicts with official model membership".to_owned());
            }
            if resource.binding.status == "shared_constraint" {
                shared_ids.push(resource.id.clone());
                continue;
            }
            // Aggregates and their raw child windows are views of the SAME
            // allowance, not independent capacities. Reserve the entire
            // non-shared resource closure, including the selected ancestor.
            let mut pending = vec![resource.id.clone()];
            let mut closure = std::collections::HashSet::new();
            while let Some(resource_id) = pending.pop() {
                let nested = discovered
                    .iter()
                    .find(|bucket| bucket.id == resource_id && bucket.scope_id == binding.scope_id)
                    .ok_or_else(|| {
                        "Quota binding references a missing underlying resource".to_owned()
                    })?;
                if nested.binding.status == "shared_constraint" {
                    continue;
                }
                if nested.source != "enforced_summary" {
                    return Err(
                        "Catalog resources cannot underlie enforced quota bindings".to_owned()
                    );
                }
                if closure.insert(nested.id.clone()) {
                    pending.extend(nested.constraint_ids.iter().cloned());
                }
            }
            if closure.iter().any(|id| consumed.contains(id)) {
                return Err("Independent primary bindings cannot reuse an allowance or its underlying window resources".to_owned());
            }
            binding_resources.extend(closure.iter().cloned());
            consumed.extend(closure);
            for tier in &resource.tiers {
                if !names.insert(tier.name.clone()) {
                    return Err(
                        "Quota binding contains ambiguous duplicate window constraints".to_owned(),
                    );
                }
                tiers.push(tier.clone());
            }
            shared_ids.push(resource.id.clone());
            shared_ids.extend(resource.constraint_ids.iter().cloned());
            stale |= resource.stale;
            if queried_at
                .as_ref()
                .is_none_or(|old| resource.queried_at < *old)
            {
                queried_at = Some(resource.queried_at.clone());
            }
        }
        // An operator may resolve unknown resources, but must not split a
        // proven allowance or drop an enforced weekly/shared sibling window.
        for official in discovered.iter().filter(|bucket| {
            bucket.is_resolved()
                && bucket.binding.source != "configured"
                && bucket.source == "enforced_summary"
                && bucket.scope_id == binding.scope_id
                && bucket
                    .models
                    .iter()
                    .any(|model| binding.models.contains(model))
        }) {
            if official.models.len() != binding.models.len()
                || !official
                    .models
                    .iter()
                    .all(|model| binding.models.contains(model))
            {
                return Err(
                    "A proven quota allowance cannot be split into independent model bindings"
                        .to_owned(),
                );
            }
            if !binding_resources.contains(&official.id) {
                let complete = !official.constraint_ids.is_empty()
                    && official.constraint_ids.iter().all(|id| {
                        discovered
                            .iter()
                            .find(|resource| {
                                resource.id == *id && resource.scope_id == binding.scope_id
                            })
                            .is_some_and(|resource| {
                                if resource.binding.status == "shared_constraint" {
                                    shared_ids.contains(id)
                                } else {
                                    binding_resources.contains(id)
                                }
                            })
                    });
                if !complete {
                    return Err("Quota binding cannot omit a proven allowance's required windows or shared constraints".to_owned());
                }
            }
        }
        if tiers.is_empty() {
            return Err("A primary quota binding needs an observed allowance".to_owned());
        }
        tiers.sort_by_key(|tier| match tier.name.as_str() {
            "five_hour" => 0,
            "daily" => 1,
            "weekly_limit" => 2,
            "monthly" => 3,
            _ => 4,
        });
        shared_ids.sort();
        shared_ids.dedup();
        result.push(QuotaBucket {
            id: opaque_quota_id("configured_bucket_v1", &[&binding.scope_id, &binding.id]),
            scope_id: binding.scope_id.clone(),
            label: binding.label.clone(),
            models: binding.models.clone(),
            binding: QuotaBucketBinding {
                status: "resolved".to_owned(),
                source: "configured".to_owned(),
                revision: config.revision,
            },
            tiers,
            constraint_ids: shared_ids,
            source: "enforced_summary".to_owned(),
            queried_at: queried_at.unwrap_or_default(),
            stale,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        });
    }
    for bucket in &mut result {
        if consumed.contains(&bucket.id) {
            bucket.binding.status = "window_constraint".to_owned();
        } else if bucket.source == "catalog" {
            bucket.models.retain(|model| !models.contains(model));
        }
    }
    result.retain(|bucket| bucket.source != "catalog" || !bucket.models.is_empty());
    let mut primary_models = std::collections::HashSet::new();
    for bucket in result.iter().filter(|bucket| bucket.is_resolved()) {
        for model in &bucket.models {
            if !primary_models.insert((&bucket.scope_id, model)) {
                return Err(format!(
                    "Conflicting primary quota bindings for model {model}"
                ));
            }
        }
    }
    Ok(result)
}

/// Compact last-good enforcement evidence. Internal settings only; it omits
/// account identities, prices and credentials. Exact model selectors are needed
/// to restore the SAME narrow gate before any request can be admitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentQuotaBucketFence {
    pub id: String,
    pub scope_id: String,
    pub label: String,
    pub models: Vec<String>,
    pub binding: QuotaBucketBinding,
    pub tiers: Vec<PersistentQuotaTier>,
    pub constraint_ids: Vec<String>,
    pub source: String,
    pub queried_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentQuotaTier {
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
}

pub fn quota_bucket_fence_key(provider_id: &str) -> String {
    format!("quota_bucket_fence.{provider_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(id: &str, status: &str, refs: &[&str]) -> QuotaBucket {
        QuotaBucket {
            id: id.to_owned(),
            scope_id: "owner".to_owned(),
            label: id.to_owned(),
            models: Vec::new(),
            binding: QuotaBucketBinding {
                status: status.to_owned(),
                source: "official".to_owned(),
                revision: 1,
            },
            tiers: vec![ProviderUsageTier {
                name: "five_hour".to_owned(),
                used_percent: 20.0,
                resets_at: None,
            }],
            constraint_ids: refs.iter().map(|id| (*id).to_owned()).collect(),
            source: "enforced_summary".to_owned(),
            queried_at: "2030-01-01T00:00:00Z".to_owned(),
            stale: false,
            scheduling: ProviderScheduling::default(),
            api_price: None,
        }
    }

    #[test]
    fn manual_binding_cannot_duplicate_aggregate_and_child_allowance() {
        let discovered = vec![
            resource("aggregate", "unresolved", &["child"]),
            resource("child", "window_constraint", &[]),
        ];
        let config = QuotaBucketConfig {
            revision: 1,
            bindings: vec![
                ManualQuotaBucketBinding {
                    id: "high".to_owned(),
                    scope_id: "owner".to_owned(),
                    label: "High".to_owned(),
                    models: vec!["high".to_owned()],
                    constraint_ids: vec!["aggregate".to_owned()],
                },
                ManualQuotaBucketBinding {
                    id: "low".to_owned(),
                    scope_id: "owner".to_owned(),
                    label: "Low".to_owned(),
                    models: vec!["low".to_owned()],
                    constraint_ids: vec!["child".to_owned()],
                },
            ],
        };
        assert!(apply_quota_bucket_config(&discovered, &config).is_err());
        let mut reversed = config.clone();
        reversed.bindings.reverse();
        assert!(apply_quota_bucket_config(&discovered, &reversed).is_err());
    }

    #[test]
    fn manual_binding_cannot_drop_proven_weekly_window_or_split_members() {
        let mut aggregate = resource("aggregate", "resolved", &["five", "week"]);
        aggregate.models = vec!["low".to_owned(), "high".to_owned()];
        aggregate.tiers.push(ProviderUsageTier {
            name: "weekly_limit".to_owned(),
            used_percent: 100.0,
            resets_at: None,
        });
        let mut five = resource("five", "window_constraint", &[]);
        five.models = aggregate.models.clone();
        let mut week = resource("week", "window_constraint", &[]);
        week.models = aggregate.models.clone();
        week.tiers[0].name = "weekly_limit".to_owned();
        week.tiers[0].used_percent = 100.0;
        let discovered = vec![aggregate, five, week];
        let config = QuotaBucketConfig {
            revision: 1,
            bindings: vec![ManualQuotaBucketBinding {
                id: "only-five".to_owned(),
                scope_id: "owner".to_owned(),
                label: "Flash".to_owned(),
                models: vec!["low".to_owned(), "high".to_owned()],
                constraint_ids: vec!["five".to_owned()],
            }],
        };
        assert!(apply_quota_bucket_config(&discovered, &config).is_err());
        let mut split = config.clone();
        split.bindings[0].models = vec!["low".to_owned()];
        split.bindings[0].constraint_ids = vec!["aggregate".to_owned()];
        assert!(apply_quota_bucket_config(&discovered, &split).is_err());
        let mut complete = config;
        complete.bindings[0].constraint_ids = vec!["aggregate".to_owned()];
        assert!(apply_quota_bucket_config(&discovered, &complete).is_ok());
    }

    #[test]
    fn manual_independent_allowances_may_reference_same_shared_constraint() {
        let discovered = vec![
            resource("a", "unresolved", &["shared"]),
            resource("b", "unresolved", &["shared"]),
            resource("shared", "shared_constraint", &[]),
        ];
        let config = QuotaBucketConfig {
            revision: 1,
            bindings: vec![
                ManualQuotaBucketBinding {
                    id: "flash".to_owned(),
                    scope_id: "owner".to_owned(),
                    label: "Flash".to_owned(),
                    models: vec!["flash".to_owned()],
                    constraint_ids: vec!["a".to_owned()],
                },
                ManualQuotaBucketBinding {
                    id: "argon".to_owned(),
                    scope_id: "owner".to_owned(),
                    label: "Argon".to_owned(),
                    models: vec!["argon".to_owned()],
                    constraint_ids: vec!["b".to_owned()],
                },
            ],
        };
        assert!(apply_quota_bucket_config(&discovered, &config).is_ok());
    }

    #[test]
    fn google_metadata_email_and_refreshed_subject_keep_same_owner() {
        let first = crate::auth::types::StoredCredential {
            meta: serde_json::json!({"email": "User@Example.org"}),
            ..Default::default()
        };
        let next = crate::auth::types::StoredCredential {
            subject_id: Some("user@example.org".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            google_quota_owner_id(&first, Some("random-fallback")),
            google_quota_owner_id(&next, None)
        );
        assert_eq!(
            google_quota_owner_id(
                &crate::auth::types::StoredCredential::default(),
                Some("stable-legacy")
            ),
            Some("stable-legacy".to_owned())
        );
    }

    #[test]
    fn identities_are_opaque_stable_and_component_delimited() {
        let scope = opaque_quota_id("google", &["provider", "user@example.org", "project"]);
        assert!(!scope.contains("user"));
        assert_eq!(
            scope,
            opaque_quota_id("google", &["provider", "user@example.org", "project"])
        );
        assert_ne!(
            opaque_quota_id("bucket", &["ab", "c"]),
            opaque_quota_id("bucket", &["a", "bc"])
        );
        assert_ne!(
            scope,
            opaque_quota_id("google", &["provider", "another-owner", "project"])
        );
    }
}
