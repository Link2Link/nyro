//! Mandatory, durable admission for Claude's usage endpoint. No success cache:
//! a denied attempt must never become a new authoritative quota observation.
use anyhow::Context;
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::{Gateway, auth::types::StoredCredential};

pub(crate) const INTERVAL_MS: i64 = 10 * 60 * 1000;
// Allow one monitor tick plus an upstream request, without marking a failed
// refresh fresh. Explicit usage_stale still takes precedence everywhere.
pub(crate) const FRESHNESS_MS: i64 = INTERVAL_MS + 60_000;
pub(crate) const SETTING_PREFIX: &str = "claude_usage_cooldown.";

#[derive(Debug, thiserror::Error)]
#[error(
    "Claude usage refresh is limited to once every 10 minutes; next allowed at {next_refresh_at} (Unix milliseconds). Cached usage is unchanged."
)]
pub(crate) struct Cooldown {
    pub next_refresh_at: i64,
}

pub(crate) fn key(base_url: &str, credential: &StoredCredential) -> anyhow::Result<String> {
    let base = if base_url.trim().is_empty() {
        "https://api.anthropic.com"
    } else {
        base_url.trim()
    };
    let url = reqwest::Url::parse(base).context("Invalid Claude usage origin")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Invalid Claude usage origin"
    );
    let identity = credential
        .meta
        .pointer("/account/uuid")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            credential
                .subject_id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
        });
    // Unknown identities share an origin-wide gate. Token rotation, duplicate
    // provider IDs, or repeated login sessions must not create new allowances.
    let owner = identity.map(str::trim).unwrap_or("unknown-account");
    let scope = serde_json::to_vec(&(url.origin().ascii_serialization(), owner))?;
    Ok(format!(
        "{SETTING_PREFIX}{}",
        hex::encode(Sha256::digest(scope))
    ))
}

fn parse_deadline(raw: &str) -> anyhow::Result<i64> {
    let deadline = raw
        .parse::<i64>()
        .context("Invalid internal Claude usage cooldown")?;
    anyhow::ensure!(deadline >= 0, "Invalid internal Claude usage cooldown");
    Ok(deadline)
}

pub(crate) async fn next_refresh_at(
    gw: &Gateway,
    base_url: &str,
    credential: &StoredCredential,
    prior_observed_at: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let key = key(base_url, credential)?;
    match gw.storage.settings().get(&key).await? {
        Some(raw) => Ok(Some(parse_deadline(&raw)?)),
        None => Ok(prior_observed_at.map(|at| at.saturating_add(INTERVAL_MS))),
    }
}

pub(crate) async fn reserve(
    gw: &Gateway,
    base_url: &str,
    credential: &StoredCredential,
    prior_observed_at: Option<i64>,
) -> anyhow::Result<i64> {
    reserve_at(
        gw,
        base_url,
        credential,
        prior_observed_at,
        Utc::now().timestamp_millis(),
    )
    .await
}

async fn reserve_at(
    gw: &Gateway,
    base_url: &str,
    credential: &StoredCredential,
    prior_observed_at: Option<i64>,
    now: i64,
) -> anyhow::Result<i64> {
    let key = key(base_url, credential)?;
    let store = gw.storage.settings();
    // CAS is shared-storage atomic, not just an in-process mutex. Persist before
    // HTTP so errors, cancellation, restarts and failed publication consume a slot.
    for _ in 0..8 {
        let raw = store.get(&key).await?;
        let existing = raw.as_deref().map(parse_deadline).transpose()?;
        if let Some(next_refresh_at) = existing.filter(|at| *at > now) {
            return Err(Cooldown { next_refresh_at }.into());
        }
        if existing.is_none()
            && let Some(next_refresh_at) = prior_observed_at
                .map(|at| at.saturating_add(INTERVAL_MS))
                .filter(|at| *at > now)
        {
            // Seed existing successful snapshots on upgrade. This cannot recover
            // attempts made by older versions that did not store a reservation.
            if store
                .compare_and_set(&key, None, &next_refresh_at.to_string())
                .await?
            {
                return Err(Cooldown { next_refresh_at }.into());
            }
            continue;
        }
        let next_refresh_at = now.saturating_add(INTERVAL_MS);
        if store
            .compare_and_set(&key, raw.as_deref(), &next_refresh_at.to_string())
            .await?
        {
            return Ok(next_refresh_at);
        }
    }
    anyhow::bail!("Claude usage refresh admission changed concurrently; retry later")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use std::sync::Arc;

    async fn gateway(storage: Arc<MemoryStorage>) -> Gateway {
        Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
            .await
            .unwrap()
            .0
    }

    fn credential(id: &str) -> StoredCredential {
        StoredCredential {
            meta: serde_json::json!({"account": {"uuid": id}}),
            ..Default::default()
        }
    }

    #[test]
    fn identity_survives_rotation_and_unknown_accounts_cannot_evade_admission() {
        let a = credential("a");
        let mut rotated = a.clone();
        rotated.access_token = Some("SECRET-NEW-TOKEN".into());
        rotated.refresh_token = Some("SECRET-REFRESH".into());
        assert_eq!(
            key("https://API.ANTHROPIC.COM:443/", &a).unwrap(),
            key("https://api.anthropic.com/v1", &rotated).unwrap()
        );
        assert_ne!(
            key("https://api.anthropic.com", &a).unwrap(),
            key("https://api.anthropic.com", &credential("b")).unwrap()
        );
        assert_ne!(
            key("http://127.0.0.1:1", &a).unwrap(),
            key("http://127.0.0.1:2", &a).unwrap()
        );
        let mut unknown = StoredCredential::default();
        let original = key("", &unknown).unwrap();
        unknown.access_token = Some("SECRET-RELOGIN".into());
        assert_eq!(
            original,
            key("https://api.anthropic.com/", &unknown).unwrap()
        );
        assert!(!original.contains("SECRET"));
    }

    #[tokio::test]
    async fn exact_deadline_restart_and_blocked_attempts_do_not_extend_cooldown() {
        let storage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let gw = gateway(storage.clone()).await;
        let owner = credential("one");
        let at = 1_800_000_000_000;
        let end = reserve_at(&gw, "", &owner, None, at).await.unwrap();
        assert_eq!(end, at + INTERVAL_MS);
        // A second Gateway models process restart / another worker on the same store.
        let restarted = gateway(storage).await;
        for now in [at, at + 1, end - 1] {
            let error = reserve_at(&restarted, "", &owner, None, now)
                .await
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<Cooldown>().unwrap().next_refresh_at,
                end
            );
        }
        assert_eq!(
            next_refresh_at(&gw, "", &owner, None).await.unwrap(),
            Some(end)
        );
        assert_eq!(
            reserve_at(&gw, "", &owner, None, end).await.unwrap(),
            end + INTERVAL_MS
        );
    }

    #[tokio::test]
    async fn concurrent_workers_share_one_reservation_and_accounts_are_independent() {
        let storage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let a = gateway(storage.clone()).await;
        let b = gateway(storage).await;
        let at = 1_800_000_000_000;
        let owner = credential("one");
        let (left, right) = tokio::join!(
            reserve_at(&a, "", &owner, None, at),
            reserve_at(&b, "", &owner, None, at),
        );
        assert_ne!(left.is_ok(), right.is_ok());
        assert!(
            reserve_at(&b, "", &credential("two"), None, at)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn old_observation_seeds_gate_and_corrupt_ledger_fails_closed() {
        let gw = gateway(Arc::new(MemoryStorage::new(vec![], vec![], vec![]))).await;
        let owner = credential("one");
        let at = 1_800_000_000_000;
        let error = reserve_at(&gw, "", &owner, Some(at - 60_000), at)
            .await
            .unwrap_err();
        let end = error.downcast_ref::<Cooldown>().unwrap().next_refresh_at;
        assert_eq!(end, at + INTERVAL_MS - 60_000);
        assert_eq!(
            reserve_at(&gw, "", &owner, Some(at - INTERVAL_MS), end - 1)
                .await
                .unwrap_err()
                .downcast_ref::<Cooldown>()
                .unwrap()
                .next_refresh_at,
            end
        );
        assert!(reserve_at(&gw, "", &owner, None, end).await.is_ok());
        gw.storage
            .settings()
            .set(&key("", &owner).unwrap(), "broken")
            .await
            .unwrap();
        assert!(
            reserve_at(&gw, "", &owner, None, end + INTERVAL_MS)
                .await
                .is_err()
        );
    }
}
