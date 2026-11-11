//! Mandatory, durable admission for Claude's usage endpoint. No success cache:
//! a denied attempt must never become a new authoritative quota observation.
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Gateway, auth::types::StoredCredential};

pub(crate) const INTERVAL_MS: i64 = 10 * 60 * 1000;
// Allow one monitor tick plus an upstream request, without marking a failed
// refresh fresh. Explicit usage_stale still takes precedence everywhere.
pub(crate) const FRESHNESS_MS: i64 = INTERVAL_MS + 60_000;
pub(crate) const SETTING_PREFIX: &str = "claude_usage_cooldown.";

#[derive(Debug, thiserror::Error)]
#[error(
    "Claude usage refresh is limited to once every 10 minutes; next allowed at {next_refresh_at} (Unix milliseconds). Upstream Retry-After/backoff may extend this interval. Cached usage is unchanged."
)]
pub(crate) struct Cooldown {
    pub next_refresh_at: i64,
}

pub(super) fn origin(base_url: &str) -> anyhow::Result<String> {
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
    Ok(url.origin().ascii_serialization())
}

pub(super) fn account_identity(credential: &StoredCredential) -> Option<&str> {
    credential
        .meta
        .pointer("/account/uuid")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            credential
                .subject_id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
        })
        .map(str::trim)
}

pub(crate) fn key(base_url: &str, credential: &StoredCredential) -> anyhow::Result<String> {
    // Unknown identities share an origin-wide gate. Token rotation, duplicate
    // provider IDs, or repeated login sessions must not create new allowances.
    let owner = account_identity(credential).unwrap_or("unknown-account");
    let scope = serde_json::to_vec(&(origin(base_url)?, owner))?;
    Ok(format!(
        "{SETTING_PREFIX}{}",
        hex::encode(Sha256::digest(scope))
    ))
}

/// Kept in the existing settings row: no schema changes. The reservation's
/// original deadline is also its monotonically increasing attempt identifier.
#[derive(Debug, Serialize, Deserialize)]
struct Ledger {
    version: u8,
    next_refresh_at: i64,
    attempt: Option<i64>,
    consecutive_failures: u32,
    retry_after: Option<i64>,
    completed: bool,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            next_refresh_at: 0,
            attempt: None,
            consecutive_failures: 0,
            retry_after: None,
            completed: false,
        }
    }
}

impl Ledger {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        let ledger = if let Ok(deadline) = raw.parse::<i64>() {
            // Pre-backoff versions stored just a deadline. Never reopen its gate.
            Self {
                next_refresh_at: deadline,
                ..Default::default()
            }
        } else {
            serde_json::from_str(raw).context("Invalid internal Claude usage cooldown")?
        };
        anyhow::ensure!(
            ledger.version == 1
                && ledger.next_refresh_at >= 0
                && ledger
                    .attempt
                    .is_none_or(|at| at >= 0 && at <= ledger.next_refresh_at)
                && ledger
                    .retry_after
                    .is_none_or(|at| at >= 0 && at <= ledger.next_refresh_at),
            "Invalid internal Claude usage cooldown"
        );
        Ok(ledger)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Failure {
    RateLimited,
    Forbidden,
    Transient,
    Other,
}

impl Failure {
    pub(super) fn from_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimited,
            403 => Self::Forbidden,
            500..=599 => Self::Transient,
            _ => Self::Other,
        }
    }

    fn bounds(self) -> (i64, i64) {
        match self {
            Self::RateLimited => (20 * 60_000, 6 * 60 * 60_000),
            Self::Transient => (10 * 60_000, 60 * 60_000),
            // A 403 is not evidence of revoked authentication. Avoid hammering
            // a policy/scope denial while preserving the credential for inference.
            Self::Forbidden => (6 * 60 * 60_000, 6 * 60 * 60_000),
            Self::Other => (INTERVAL_MS, INTERVAL_MS),
        }
    }

    fn delay_ms(self, failures: u32) -> i64 {
        let (base, cap) = self.bounds();
        (base * (1_i64 << failures.saturating_sub(1).min(10))).min(cap)
    }

    fn delay_with_jitter(self, failures: u32, key: &str, attempt: i64) -> i64 {
        let mut hash = Sha256::new();
        hash.update(key.as_bytes());
        hash.update(attempt.to_be_bytes());
        hash.update(failures.to_be_bytes());
        let hash = hash.finalize();
        let jitter = 1000 + i64::from(u16::from_be_bytes([hash[0], hash[1]])) % 29_001;
        (self.delay_ms(failures) + jitter).min(self.bounds().1)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Outcome {
    Headers,
    Success,
    Failure(Failure),
}

/// Parse delta-seconds and HTTP dates before consuming the response body.
/// Multiple valid hints are combined conservatively; upstream deadlines are
/// never capped by the local exponential-backoff maximum.
pub(super) fn retry_after_at(headers: &reqwest::header::HeaderMap, now: i64) -> Option<i64> {
    headers
        .get_all(reqwest::header::RETRY_AFTER)
        .iter()
        .filter_map(|header| {
            let value = header.to_str().ok()?.trim();
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                // Even an overflowing integer is a valid, very long upstream delay.
                let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
                let millis = seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
                return Some(now.saturating_add(millis));
            }
            let parsed = chrono::DateTime::parse_from_rfc2822(value)
                .map(|date| date.timestamp_millis())
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
                        .map(|date| date.and_utc().timestamp_millis())
                })
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
                        .map(|date| date.and_utc().timestamp_millis())
                })
                .ok()?;
            Some(parsed.max(now))
        })
        .max()
}

pub(crate) async fn next_refresh_at(
    gw: &Gateway,
    base_url: &str,
    credential: &StoredCredential,
    prior_observed_at: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let key = key(base_url, credential)?;
    match gw.storage.settings().get(&key).await? {
        Some(raw) => Ok(Some(Ledger::parse(&raw)?.next_refresh_at)),
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
        let existing = raw.as_deref().map(Ledger::parse).transpose()?;
        if let Some(next_refresh_at) = existing
            .as_ref()
            .map(|state| state.next_refresh_at)
            .filter(|at| *at > now)
        {
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
        anyhow::ensure!(
            now >= 0 && next_refresh_at > now,
            "Invalid Claude usage admission time"
        );
        let mut state = existing.unwrap_or_default();
        state.next_refresh_at = next_refresh_at;
        state.attempt = Some(next_refresh_at);
        state.retry_after = None;
        state.completed = false;
        let updated = serde_json::to_string(&state)?;
        if store
            .compare_and_set(&key, raw.as_deref(), &updated)
            .await?
        {
            return Ok(next_refresh_at);
        }
    }
    anyhow::bail!("Claude usage refresh admission changed concurrently; retry later")
}

/// Extend the gate using CAS and the reservation identifier, never an account's
/// latest state alone. Late responses and duplicate completions cannot overwrite
/// a newer attempt or reset its failure streak.
pub(super) async fn record(
    gw: &Gateway,
    base_url: &str,
    credential: &StoredCredential,
    attempt: i64,
    outcome: Outcome,
    retry_after: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    record_at(
        gw,
        &key(base_url, credential)?,
        attempt,
        outcome,
        retry_after,
        Utc::now().timestamp_millis(),
    )
    .await
}

async fn record_at(
    gw: &Gateway,
    key: &str,
    attempt: i64,
    outcome: Outcome,
    retry_after: Option<i64>,
    now: i64,
) -> anyhow::Result<Option<i64>> {
    let store = gw.storage.settings();
    for _ in 0..8 {
        let Some(raw) = store.get(key).await? else {
            return Ok(None);
        };
        let mut state = Ledger::parse(&raw)?;
        if state.attempt != Some(attempt) || state.completed {
            return Ok(None);
        }
        if let Some(hint) = retry_after {
            state.retry_after = Some(state.retry_after.unwrap_or(0).max(hint));
            state.next_refresh_at = state.next_refresh_at.max(hint);
        }
        match outcome {
            Outcome::Headers => {}
            Outcome::Success => {
                state.consecutive_failures = 0;
                state.completed = true;
                // Success must not undo either the reservation or Retry-After.
            }
            Outcome::Failure(failure) => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.next_refresh_at =
                    state
                        .next_refresh_at
                        .max(now.saturating_add(failure.delay_with_jitter(
                            state.consecutive_failures,
                            key,
                            attempt,
                        )));
                state.completed = true;
            }
        }
        let updated = serde_json::to_string(&state)?;
        if store.compare_and_set(key, Some(&raw), &updated).await? {
            return Ok(Some(state.next_refresh_at));
        }
    }
    anyhow::bail!("Claude usage outcome changed concurrently; retry later")
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
    fn retry_after_accepts_seconds_dates_and_never_caps_upstream_hints() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-11T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let parse = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            retry_after_at(&headers, now)
        };
        assert_eq!(parse("120"), Some(now + 120_000));
        assert_eq!(parse("86400"), Some(now + 86_400_000));
        assert_eq!(
            parse("Wed, 11 Nov 2026 01:00:00 GMT"),
            Some(now + 3_600_000)
        );
        assert_eq!(
            parse("Wednesday, 11-Nov-26 01:00:00 GMT"),
            Some(now + 3_600_000)
        );
        assert_eq!(parse("Wed Nov 11 01:00:00 2026"), Some(now + 3_600_000));
        assert_eq!(parse("Tue, 10 Nov 2026 01:00:00 GMT"), Some(now));
        assert_eq!(parse("999999999999999999999999999999"), Some(i64::MAX));
        for invalid in ["", "-1", "NaN", "1.5", "bad-date"] {
            assert_eq!(parse(invalid), None);
        }
        let mut headers = HeaderMap::new();
        headers.append(RETRY_AFTER, HeaderValue::from_static("3600"));
        headers.append(RETRY_AFTER, HeaderValue::from_static("60"));
        assert_eq!(retry_after_at(&headers, now), Some(now + 3_600_000));
    }

    #[test]
    fn exponential_failure_schedules_and_positive_jitter_are_bounded() {
        for (failure, minutes) in [
            (Failure::RateLimited, [20, 40, 80, 160, 320, 360, 360]),
            (Failure::Transient, [10, 20, 40, 60, 60, 60, 60]),
            (Failure::Forbidden, [360; 7]),
        ] {
            for (index, minutes) in minutes.into_iter().enumerate() {
                let failures = index as u32 + 1;
                let base = failure.delay_ms(failures);
                assert_eq!(base, minutes * 60_000);
                let delay = failure.delay_with_jitter(failures, "account", 1_800_000_000_000);
                assert!(delay >= base && delay <= (base + 30_000).min(failure.bounds().1));
                if base < failure.bounds().1 {
                    assert!(delay >= base + 1000);
                }
                assert_eq!(
                    delay,
                    failure.delay_with_jitter(failures, "account", 1_800_000_000_000)
                );
            }
        }
    }

    #[tokio::test]
    async fn failed_attempts_persist_streaks_across_restarts_and_success_keeps_gate() {
        let storage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let gw = gateway(storage.clone()).await;
        let owner = credential("streak");
        let key = key("", &owner).unwrap();
        let mut now = 1_800_000_000_000;
        for count in 1..=7 {
            let attempt = reserve_at(&gw, "", &owner, None, now).await.unwrap();
            let expected = now + Failure::RateLimited.delay_with_jitter(count, &key, attempt);
            assert_eq!(
                record_at(
                    &gw,
                    &key,
                    attempt,
                    Outcome::Failure(Failure::RateLimited),
                    None,
                    now
                )
                .await
                .unwrap(),
                Some(expected)
            );
            let restarted = gateway(storage.clone()).await;
            assert_eq!(
                next_refresh_at(&restarted, "", &owner, None).await.unwrap(),
                Some(expected)
            );
            assert!(
                reserve_at(&restarted, "", &owner, None, expected - 1)
                    .await
                    .is_err()
            );
            now = expected;
        }
        let attempt = reserve_at(&gw, "", &owner, None, now).await.unwrap();
        let hint = now + 24 * 60 * 60_000;
        record_at(&gw, &key, attempt, Outcome::Headers, Some(hint), now)
            .await
            .unwrap();
        assert_eq!(
            record_at(&gw, &key, attempt, Outcome::Success, None, now)
                .await
                .unwrap(),
            Some(hint)
        );
        let raw = gw.storage.settings().get(&key).await.unwrap().unwrap();
        let state = Ledger::parse(&raw).unwrap();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.retry_after, Some(hint));
        let attempt = reserve_at(&gw, "", &owner, None, hint).await.unwrap();
        assert_eq!(
            record_at(
                &gw,
                &key,
                attempt,
                Outcome::Failure(Failure::RateLimited),
                None,
                hint
            )
            .await
            .unwrap(),
            Some(hint + Failure::RateLimited.delay_with_jitter(1, &key, attempt))
        );
    }

    #[tokio::test]
    async fn old_and_duplicate_completions_cannot_replace_newer_state() {
        let gw = gateway(Arc::new(MemoryStorage::new(vec![], vec![], vec![]))).await;
        let owner = credential("superseded");
        let key = key("", &owner).unwrap();
        let now = 1_800_000_000_000;
        let old = reserve_at(&gw, "", &owner, None, now).await.unwrap();
        let current = reserve_at(&gw, "", &owner, None, old).await.unwrap();
        let baseline = gw.storage.settings().get(&key).await.unwrap();
        for outcome in [
            Outcome::Success,
            Outcome::Failure(Failure::Forbidden),
            Outcome::Headers,
        ] {
            assert_eq!(
                record_at(&gw, &key, old, outcome, Some(i64::MAX), old)
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(gw.storage.settings().get(&key).await.unwrap(), baseline);
        }
        let hint = old + 24 * 60 * 60_000;
        assert_eq!(
            record_at(
                &gw,
                &key,
                current,
                Outcome::Failure(Failure::Transient),
                Some(hint),
                old
            )
            .await
            .unwrap(),
            Some(hint)
        );
        let completed = gw.storage.settings().get(&key).await.unwrap();
        assert_eq!(
            record_at(&gw, &key, current, Outcome::Success, None, old)
                .await
                .unwrap(),
            None
        );
        assert_eq!(gw.storage.settings().get(&key).await.unwrap(), completed);
    }

    #[tokio::test]
    async fn legacy_integer_migrates_at_deadline_and_future_versions_fail_closed() {
        let gw = gateway(Arc::new(MemoryStorage::new(vec![], vec![], vec![]))).await;
        let owner = credential("legacy");
        let key = key("", &owner).unwrap();
        let end = 1_800_000_000_000;
        gw.storage
            .settings()
            .set(&key, &end.to_string())
            .await
            .unwrap();
        assert_eq!(
            next_refresh_at(&gw, "", &owner, None).await.unwrap(),
            Some(end)
        );
        assert!(reserve_at(&gw, "", &owner, None, end - 1).await.is_err());
        assert_eq!(
            reserve_at(&gw, "", &owner, None, end).await.unwrap(),
            end + INTERVAL_MS
        );
        let raw = gw.storage.settings().get(&key).await.unwrap().unwrap();
        assert_eq!(Ledger::parse(&raw).unwrap().version, 1);
        let mut unsupported: serde_json::Value = serde_json::from_str(&raw).unwrap();
        unsupported["version"] = serde_json::json!(2);
        assert!(Ledger::parse(&unsupported.to_string()).is_err());
        assert!(Ledger::parse("-1").is_err());
        assert!(Ledger::parse("{}").is_err());
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
