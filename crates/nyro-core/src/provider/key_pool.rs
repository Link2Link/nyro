//! Provider key-pool selection for multi-key relay vendors.
//!
//! A provider with a non-empty `keys` pool routes each request through one
//! key at a time. Eligibility is driven by the key's effective model set
//! (manual override first, then the discovery snapshot); a key with no
//! information at all is treated as eligible for every model so an unprobed
//! pool never silently blocks traffic. Selection is priority-ordered with
//! transparent failover to the next eligible key (see `eligible_keys`).

use crate::db::models::ProviderKey;

/// Keys eligible for `model`, in dispatch order (input must already be
/// ordered by priority — the storage layer guarantees this).
///
/// Eligibility:
/// - disabled keys are never eligible;
/// - a key with `Some(model)` in its effective model set is eligible;
/// - a key whose effective set is `None` (never probed, no manual list) is
///   eligible for any model;
/// - a key whose effective set is `Some(empty)` is eligible for nothing.
pub fn eligible_keys<'a>(keys: &[&'a ProviderKey], model: &str) -> Vec<&'a ProviderKey> {
    let mut eligible: Vec<&'a ProviderKey> = keys
        .iter()
        .copied()
        .filter(|key| {
            if !key.is_enabled {
                return false;
            }
            key.effective_models()
                .map(|models| models.iter().any(|m| m == model))
                .unwrap_or(true)
        })
        .collect();
    // Priority order, stable for equal priorities (input order breaks ties —
    // the storage layer returns insertion order).
    eligible.sort_by_key(|key| key.priority);
    eligible
}

/// Health-registry key component for a pool key. `None` when the provider has
/// no pool — the legacy `provider_id:egress:model` key is used unchanged.
pub fn health_key(provider_id: &str, key_id: Option<&str>, egress: &str, model: &str) -> String {
    match key_id {
        Some(key_id) => format!("{provider_id}:{key_id}:{egress}:{model}"),
        None => format!("{provider_id}:{egress}:{model}"),
    }
}

/// Whether an upstream status should trigger failover to the next eligible
/// key of the same provider. Deliberately narrower than `is_retryable`:
/// 5xx-class upstream errors usually belong to the model, not the key, and
/// are handled by the regular target-level retry path.
pub fn key_failover_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 429)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, priority: i32, enabled: bool, models: Option<&[&str]>) -> ProviderKey {
        let (models_snapshot, manual_models): (Option<String>, Option<String>) = match models {
            Some(list) => (
                Some(crate::db::models::encode_model_list(
                    &list.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                )),
                None,
            ),
            None => (None, None),
        };
        ProviderKey {
            id: id.to_string(),
            provider_id: "p".to_string(),
            name: format!("key-{id}"),
            api_key: "secret".to_string(),
            is_enabled: enabled,
            priority,
            models_snapshot,
            manual_models: None,
            last_probe_at: None,
            probe_error: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn selects_by_priority_when_multiple_keys_hold_model() {
        let keys = vec![
            key("a", 10, true, Some(&["gpt-4o"])),
            key("b", 1, true, Some(&["gpt-4o"])),
        ];
        let refs: Vec<&ProviderKey> = keys.iter().collect();
        let eligible = eligible_keys(&refs, "gpt-4o");
        assert_eq!(eligible.len(), 2);
        assert_eq!(eligible[0].id, "b");
        assert_eq!(eligible[1].id, "a");
    }

    #[test]
    fn unprobed_key_is_eligible_for_any_model() {
        let keys = vec![key("a", 0, true, None)];
        let refs: Vec<&ProviderKey> = keys.iter().collect();
        assert_eq!(eligible_keys(&refs, "anything").len(), 1);
    }

    #[test]
    fn probed_empty_snapshot_is_never_eligible() {
        let keys = vec![key("a", 0, true, Some(&[]))];
        let refs: Vec<&ProviderKey> = keys.iter().collect();
        assert!(eligible_keys(&refs, "gpt-4o").is_empty());
    }

    #[test]
    fn disabled_key_is_skipped() {
        let keys = vec![key("a", 0, false, None), key("b", 1, true, None)];
        let refs: Vec<&ProviderKey> = keys.iter().collect();
        let eligible = eligible_keys(&refs, "m");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].id, "b");
    }

    #[test]
    fn manual_override_wins_over_snapshot() {
        let mut k = key("a", 0, true, Some(&["gpt-4o"]));
        k.manual_models = Some(crate::db::models::encode_model_list(&["claude-4".into()]));
        let refs: Vec<&ProviderKey> = vec![&k];
        assert_eq!(eligible_keys(&refs, "claude-4").len(), 1);
        assert!(eligible_keys(&refs, "gpt-4o").is_empty());
    }

    #[test]
    fn health_key_degrades_without_pool() {
        assert_eq!(health_key("p", None, "chat", "m"), "p:chat:m");
        assert_eq!(health_key("p", Some("k"), "chat", "m"), "p:k:chat:m");
    }

    #[test]
    fn failover_status_covers_key_level_errors() {
        for status in [401, 403, 404, 429] {
            assert!(key_failover_status(status), "status {status}");
        }
        assert!(!key_failover_status(500));
        assert!(!key_failover_status(200));
    }
}
