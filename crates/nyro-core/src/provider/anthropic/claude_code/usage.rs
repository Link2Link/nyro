//! Claude subscription usage for the Claude Code OAuth channel.
//!
//! Claude Code's `/usage` panel and status line read
//! `GET https://api.anthropic.com/api/oauth/usage` with the OAuth Bearer
//! token and `anthropic-beta: oauth-2025-04-20` (measured on 2.1.312). The
//! body is:
//!
//! ```text
//! { five_hour, seven_day, seven_day_oauth_apps, seven_day_opus,
//!   seven_day_sonnet: {utilization: 0-100 | null, resets_at: ISO | null} | null,
//!   limits: [{kind, group, percent, resets_at, scope:{model:{display_name}}}],
//!   extra_usage: {is_enabled, monthly_limit, used_credits, utilization, currency} }
//! ```
//!
//! `utilization` is already a percentage (the CLI prints
//! `Math.floor(utilization)% used`). The CLI shows "Current session"
//! (`five_hour`), "Current week (all models)" (`seven_day`), the
//! Sonnet-only week on Max/Team plans, and one row per model-scoped weekly
//! limit; nyro maps the same buckets onto its common usage tiers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{profile, usage_throttle};
use crate::auth::types::StoredCredential;

const USAGE_PATH: &str = "/api/oauth/usage";

/// One usage window, in nyro's tier vocabulary (see `admin::usage`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    /// `five_hour`, `weekly_limit`, or `feature:<name>:<window>`.
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    /// Presentation only, never persisted by usage refreshes: `usage_api` for a
    /// full usage query, `inference_header` for a newer same-cycle header value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Presentation only: when this window value was observed (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
}

/// Parsed `/api/oauth/usage`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClaudeUsage {
    pub windows: Vec<UsageWindow>,
    /// Canonical upstream profile plan; unknown stays unknown in stored metadata.
    /// Pool estimates separately use Pro-equivalent capacity for missing/unsupported plans.
    pub plan: Option<String>,
}

fn window(bucket: Option<&Value>, name: String) -> Option<UsageWindow> {
    let bucket = bucket?;
    let used = bucket.get("utilization").and_then(Value::as_f64)?;
    if !used.is_finite() {
        return None;
    }
    let resets_at = bucket
        .get("resets_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some(UsageWindow {
        name,
        used_percent: used.clamp(0.0, 100.0),
        resets_at,
        source: None,
        observed_at: None,
    })
}

/// Deterministic tier segment from an upstream label (`"Fable"` → `fable`).
fn feature_segment(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_separator = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            pending_separator = false;
        } else if !out.is_empty() {
            pending_separator = true;
        }
    }
    if out.is_empty() {
        "model".to_string()
    } else {
        out
    }
}

pub(crate) fn parse_usage(body: &Value) -> ClaudeUsage {
    let mut windows = Vec::new();
    windows.extend(window(body.get("five_hour"), "five_hour".to_string()));
    windows.extend(window(body.get("seven_day"), "weekly_limit".to_string()));
    for (key, feature) in [
        ("seven_day_sonnet", "sonnet"),
        ("seven_day_opus", "opus"),
        ("seven_day_oauth_apps", "oauth_apps"),
    ] {
        windows.extend(window(
            body.get(key),
            format!("feature:{feature}:weekly_limit"),
        ));
    }
    // Model-scoped weekly limits (e.g. a per-model Fable week).
    for limit in body
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(model) = limit
            .pointer("/scope/model/display_name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
        else {
            continue;
        };
        if limit.get("group").and_then(Value::as_str) != Some("weekly") {
            continue;
        }
        let Some(percent) = limit
            .get("percent")
            .and_then(Value::as_f64)
            .filter(|p| p.is_finite())
        else {
            continue;
        };
        let name = format!("feature:{}:weekly_limit", feature_segment(model));
        if windows.iter().any(|w: &UsageWindow| w.name == name) {
            continue;
        }
        windows.push(UsageWindow {
            name,
            used_percent: percent.clamp(0.0, 100.0),
            resets_at: limit
                .get("resets_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            source: None,
            observed_at: None,
        });
    }
    ClaudeUsage {
        windows,
        plan: None,
    }
}

const PROFILE_PATH: &str = "/api/oauth/profile";
const PLAN_TTL_MS: i64 = 6 * 60 * 60_000;
const PLAN_NEGATIVE_TTL_MS: i64 = 10 * 60_000;
const PLAN_CACHE_CAPACITY: usize = 256;

#[derive(Default)]
struct PlanEntry {
    expires_at: i64,
    plan: Option<String>,
}

type PlanSlot = std::sync::Arc<tokio::sync::Mutex<PlanEntry>>;

#[derive(Default)]
struct PlanCache {
    entries: std::collections::HashMap<String, PlanSlot>,
}

impl PlanCache {
    fn slot(&mut self, key: String, now: i64) -> Option<PlanSlot> {
        // Active slots (including queued followers) are never evicted, so cache
        // pressure cannot split a singleflight into two upstream requests.
        self.entries.retain(|_, slot| {
            std::sync::Arc::strong_count(slot) > 1
                || slot.try_lock().map_or(true, |entry| entry.expires_at > now)
        });
        if let Some(slot) = self.entries.get(&key) {
            return Some(slot.clone());
        }
        if self.entries.len() >= PLAN_CACHE_CAPACITY {
            // Evict only expired entries (above). Dropping an unexpired negative
            // entry would let cache pressure bypass its upstream Retry-After.
            return None;
        }
        let slot = std::sync::Arc::new(tokio::sync::Mutex::new(PlanEntry::default()));
        self.entries.insert(key, slot.clone());
        Some(slot)
    }
}

fn plan_cache_key(base: &str, token: &str, credential: &StoredCredential) -> Option<String> {
    use sha2::{Digest, Sha256};
    let origin = usage_throttle::origin(base).ok()?;
    // Known accounts survive access-token rotation and duplicate provider IDs.
    // Unknown accounts must NOT share profile/entitlement results with each other.
    let identity = usage_throttle::account_identity(credential);
    let owner = identity
        .map(str::to_owned)
        .unwrap_or_else(|| hex::encode(Sha256::digest(token.as_bytes())));
    let scope = serde_json::to_vec(&(origin, identity.is_some(), owner)).ok()?;
    Some(hex::encode(Sha256::digest(scope)))
}

/// Plan badge from `/api/oauth/profile`, using Claude Code's own mapping
/// (`organization_type`: `claude_max` → max, `claude_pro` → pro, `claude_team`
/// → team, `claude_enterprise` → enterprise) plus the Max tier from
/// `rate_limit_tier` (`default_claude_max_5x` / `_20x`). Short lowercase
/// tokens, like the other subscription backends' `level`.
pub(crate) fn parse_plan(profile: &Value) -> Option<String> {
    let organization = profile.get("organization")?;
    let plan = match organization
        .get("organization_type")
        .and_then(Value::as_str)?
    {
        "claude_max" => "max",
        "claude_pro" => "pro",
        "claude_team" => "team",
        "claude_enterprise" => "enterprise",
        _ => return None,
    };
    if plan != "max" {
        return Some(plan.to_string());
    }
    let tier = organization
        .get("rate_limit_tier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let max = if tier.ends_with("_20x") {
        "max 20x"
    } else if tier.ends_with("_5x") {
        "max 5x"
    } else {
        "max"
    };
    Some(max.to_string())
}

fn plan_cache() -> &'static std::sync::Mutex<PlanCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<PlanCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn plan_expiry(now: i64, success: bool, retry_after: Option<i64>) -> i64 {
    now.saturating_add(if success {
        PLAN_TTL_MS
    } else {
        PLAN_NEGATIVE_TTL_MS
    })
    .max(retry_after.unwrap_or(0))
}

/// Best effort profile lookup: six-hour success cache, ten-minute negative cache
/// (or a longer Retry-After). A bounded, per-account singleflight is shared by all
/// callers; unlike usage admission, unknown account results are token-isolated.
pub(crate) async fn fetch_plan(
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
    credential: &StoredCredential,
) -> Option<String> {
    let key = plan_cache_key(api_base_url, access_token, credential)?;
    let now = chrono::Utc::now().timestamp_millis();
    let slot = plan_cache().lock().ok()?.slot(key, now)?;
    let mut entry = slot.lock().await;
    let now = chrono::Utc::now().timestamp_millis();
    if entry.expires_at > now {
        return entry.plan.clone();
    }
    // Reserve a negative result before HTTP: cancellation also suppresses a
    // request stampede. Followers only hold the per-key async lock, not the map.
    entry.plan = None;
    entry.expires_at = plan_expiry(now, false, None);
    let base = api_base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://api.anthropic.com"
    } else {
        base
    };
    let response = client
        .get(format!("{base}{PROFILE_PATH}"))
        .bearer_auth(access_token)
        .header("content-type", "application/json")
        .header("cache-control", "no-cache")
        .header("user-agent", profile::CLI_USER_AGENT)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let now = chrono::Utc::now().timestamp_millis();
    let retry_after = usage_throttle::retry_after_at(response.headers(), now);
    entry.expires_at = plan_expiry(now, false, retry_after);
    if !response.status().is_success() {
        tracing::debug!(status = %response.status(), "claude-code: oauth profile unavailable");
        return None;
    }
    let body: Value = response.json().await.ok()?;
    entry.plan = parse_plan(&body);
    entry.expires_at = plan_expiry(chrono::Utc::now().timestamp_millis(), true, retry_after);
    entry.plan.clone()
}

/// Account usage refresh and login share the same best-effort profile lookup.
/// Never copy a previous plan when the current profile cannot be resolved.
pub(crate) async fn fetch_usage_with_plan(
    gw: &crate::Gateway,
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
    credential: &crate::auth::types::StoredCredential,
    prior_observed_at: Option<i64>,
    _account_identity: &str,
) -> anyhow::Result<ClaudeUsage> {
    let mut usage = fetch_usage(
        gw,
        client,
        api_base_url,
        access_token,
        credential,
        prior_observed_at,
    )
    .await?;
    usage.plan = fetch_plan(client, api_base_url, access_token, credential).await;
    Ok(usage)
}

/// Sanitized management-facing failure. In particular, retain the historical
/// 401 prefix for callers that still classify authentication by its string.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct UsageError {
    pub status: Option<u16>,
    pub retry_after: Option<i64>,
    message: String,
}

impl UsageError {
    fn http(status: u16, retry_after: Option<i64>) -> Self {
        let message = match status {
            401 => {
                "Claude authentication failed (HTTP 401); re-authorize this provider".to_string()
            }
            403 => "Claude usage access denied (HTTP 403); verify account permissions".to_string(),
            429 => "Claude usage endpoint is rate limited (HTTP 429); retry later".to_string(),
            _ => format!("Claude usage query failed (HTTP {status})"),
        };
        Self {
            status: Some(status),
            retry_after,
            message,
        }
    }

    fn transport(message: &str, retry_after: Option<i64>) -> Self {
        Self {
            status: None,
            retry_after,
            message: message.to_string(),
        }
    }
}

async fn record_failure(
    gw: &crate::Gateway,
    base: &str,
    credential: &StoredCredential,
    attempt: i64,
    error: &UsageError,
) {
    let failure = error
        .status
        .map(usage_throttle::Failure::from_status)
        .unwrap_or(usage_throttle::Failure::Transient);
    if usage_throttle::record(
        gw,
        base,
        credential,
        attempt,
        usage_throttle::Outcome::Failure(failure),
        error.retry_after,
    )
    .await
    .is_err()
    {
        // The mandatory reservation is already durable. Do not replace a typed
        // HTTP 401 with a storage error, or log storage details/credentials.
        tracing::warn!("claude-code: failed to persist usage backoff; reservation remains active");
    }
}

/// Fetch and parse the subscription usage for one OAuth access token.
pub(crate) async fn fetch_usage(
    gw: &crate::Gateway,
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
    credential: &crate::auth::types::StoredCredential,
    prior_observed_at: Option<i64>,
) -> anyhow::Result<ClaudeUsage> {
    let attempt = usage_throttle::reserve(gw, api_base_url, credential, prior_observed_at).await?;
    let base = api_base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://api.anthropic.com"
    } else {
        base
    };
    let response = client
        .get(format!("{base}{USAGE_PATH}"))
        .bearer_auth(access_token)
        .header("anthropic-beta", profile::OAUTH_BETA)
        .header("content-type", "application/json")
        .header("user-agent", profile::CLI_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            let error = UsageError::transport(
                "Claude usage request failed; verify proxy/network settings",
                None,
            );
            record_failure(gw, api_base_url, credential, attempt, &error).await;
            return Err(error.into());
        }
    };
    // Status and Retry-After are authoritative even if the body is truncated,
    // stalls indefinitely, or contains private data. Never read an error body.
    let status = response.status();
    let retry_after =
        usage_throttle::retry_after_at(response.headers(), chrono::Utc::now().timestamp_millis());
    if !status.is_success() {
        let error = UsageError::http(status.as_u16(), retry_after);
        record_failure(gw, api_base_url, credential, attempt, &error).await;
        return Err(error.into());
    }
    if retry_after.is_some() {
        usage_throttle::record(
            gw,
            api_base_url,
            credential,
            attempt,
            usage_throttle::Outcome::Headers,
            retry_after,
        )
        .await?;
    }
    let raw = match response.bytes().await {
        Ok(raw) => raw,
        Err(_) => {
            let error = UsageError::transport("Failed to read Claude usage response", retry_after);
            record_failure(gw, api_base_url, credential, attempt, &error).await;
            return Err(error.into());
        }
    };
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(_) => {
            let error = UsageError::transport("Invalid Claude usage response", retry_after);
            record_failure(gw, api_base_url, credential, attempt, &error).await;
            return Err(error.into());
        }
    };
    let recorded = usage_throttle::record(
        gw,
        api_base_url,
        credential,
        attempt,
        usage_throttle::Outcome::Success,
        retry_after,
    )
    .await?;
    anyhow::ensure!(
        recorded.is_some(),
        "Claude usage attempt was superseded; cached usage is unchanged"
    );
    Ok(parse_usage(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn gateway() -> crate::Gateway {
        crate::Gateway::from_storage(
            crate::config::GatewayConfig::default(),
            std::sync::Arc::new(crate::storage::MemoryStorage::new(vec![], vec![], vec![])),
        )
        .await
        .unwrap()
        .0
    }

    fn credential(id: &str) -> StoredCredential {
        StoredCredential {
            meta: json!({"account": {"uuid": id}}),
            ..Default::default()
        }
    }

    #[test]
    fn plan_cache_scopes_stable_accounts_origins_and_unknown_tokens() {
        let a = credential("a");
        assert_eq!(
            plan_cache_key("https://API.ANTHROPIC.COM:443/v1", "old", &a),
            plan_cache_key("https://api.anthropic.com", "rotated", &a)
        );
        assert_ne!(
            plan_cache_key("", "same", &a),
            plan_cache_key("", "same", &credential("b"))
        );
        assert_ne!(
            plan_cache_key("http://127.0.0.1:1", "same", &a),
            plan_cache_key("http://127.0.0.1:2", "same", &a)
        );
        assert_ne!(
            plan_cache_key("", "first", &Default::default()),
            plan_cache_key("", "second", &Default::default())
        );
        let subject = StoredCredential {
            subject_id: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_cache_key("", "rotated", &subject),
            plan_cache_key("", "original", &a)
        );
        assert!(
            !plan_cache_key("", "SECRET", &Default::default())
                .unwrap()
                .contains("SECRET")
        );
    }

    #[test]
    fn plan_cache_expiration_capacity_and_active_singleflight_are_bounded() {
        let now = 1_800_000_000_000;
        assert_eq!(plan_expiry(now, true, None), now + 6 * 60 * 60_000);
        assert_eq!(plan_expiry(now, false, None), now + 10 * 60_000);
        assert_eq!(
            plan_expiry(now, false, Some(now + 60_000)),
            now + 10 * 60_000
        );
        let hint = now + 24 * 60 * 60_000;
        assert_eq!(plan_expiry(now, false, Some(hint)), hint);
        let mut cache = PlanCache::default();
        for index in 0..PLAN_CACHE_CAPACITY {
            let slot = cache.slot(index.to_string(), now).unwrap();
            slot.try_lock().unwrap().expires_at = hint;
        }
        assert_eq!(cache.entries.len(), PLAN_CACHE_CAPACITY);
        assert!(cache.slot("overflow".into(), now).is_none());
        let active = cache.slot("0".into(), now).unwrap();
        active.try_lock().unwrap().expires_at = now;
        assert!(cache.slot("overflow".into(), now).is_none());
        assert!(std::sync::Arc::ptr_eq(
            &active,
            &cache.slot("0".into(), now).unwrap()
        ));
        drop(active);
        assert!(cache.slot("overflow".into(), now).is_some());
        assert_eq!(cache.entries.len(), PLAN_CACHE_CAPACITY);
        drop(cache.slot("at-expiry".into(), hint).unwrap());
        assert!(cache.entries.len() < PLAN_CACHE_CAPACITY);
    }

    #[tokio::test]
    async fn profile_singleflight_caches_success_failure_and_retry_after() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [200, 401, 403, 429, 503] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let count = std::sync::Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                tokio::pin!(stopped);
                loop {
                    let socket = tokio::select! {
                        socket = listener.accept() => socket.unwrap().0,
                        _ = &mut stopped => break,
                    };
                    let mut socket = socket;
                    let mut request = [0u8; 4096];
                    socket.read(&mut request).await.unwrap();
                    seen.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"organization":{"organization_type":"claude_pro"}}"#;
                    let response = format!(
                        "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nRetry-After: 86400\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
            let client = reqwest::Client::new();
            let owner = credential(&format!("singleflight-{status}"));
            let before = chrono::Utc::now().timestamp_millis();
            let expected = if status == 200 { Some("pro") } else { None };
            let (a, b, c) = tokio::join!(
                fetch_plan(&client, &base, "token", &owner),
                fetch_plan(&client, &base, "rotated", &owner),
                fetch_plan(&client, &base, "duplicate-provider", &owner),
            );
            assert_eq!(a.as_deref(), expected);
            assert_eq!(b.as_deref(), expected);
            assert_eq!(c.as_deref(), expected);
            assert_eq!(
                fetch_plan(&client, &base, "rotated-again", &owner)
                    .await
                    .as_deref(),
                expected
            );
            let key = plan_cache_key(&base, "ignored-known-token", &owner).unwrap();
            let slot = plan_cache()
                .lock()
                .unwrap()
                .entries
                .get(&key)
                .unwrap()
                .clone();
            assert!(slot.lock().await.expires_at >= before + 86_400_000);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            stop.send(()).unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn usage_headers_persist_retry_after_without_waiting_for_error_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [401, 403, 429, 503] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 9999\r\nRetry-After: 86400\r\n\r\n").as_bytes()).await.unwrap();
                std::future::pending::<()>().await;
            });
            let gw = gateway().await;
            let owner = credential("header-first");
            let before = chrono::Utc::now().timestamp_millis();
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                fetch_usage(&gw, &reqwest::Client::new(), &base, "SECRET", &owner, None),
            )
            .await
            .expect("must not wait for an error body")
            .unwrap_err();
            let error = error.downcast_ref::<UsageError>().unwrap();
            assert_eq!(error.status, Some(status));
            assert!(error.retry_after.unwrap() >= before + 86_400_000);
            assert!(
                usage_throttle::next_refresh_at(&gw, &base, &owner, None)
                    .await
                    .unwrap()
                    .unwrap()
                    >= error.retry_after.unwrap()
            );
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
        }
    }

    #[tokio::test]
    async fn successful_or_truncated_body_never_loses_retry_after_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for truncated in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                socket.read(&mut request).await.unwrap();
                let length = if truncated { 9999 } else { 2 };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nRetry-After: 86400\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
            });
            let gw = gateway().await;
            let owner = credential("success-hint");
            let before = chrono::Utc::now().timestamp_millis();
            let result =
                fetch_usage(&gw, &reqwest::Client::new(), &base, "SECRET", &owner, None).await;
            assert_eq!(result.is_err(), truncated);
            assert!(
                usage_throttle::next_refresh_at(&gw, &base, &owner, None)
                    .await
                    .unwrap()
                    .unwrap()
                    >= before + 86_400_000
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn usage_http_errors_are_sanitized_and_only_401_means_auth_rejection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [401, 403, 429, 500] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let body = "PRIVATE-UPSTREAM-ACCESS-TOKEN";
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let gw = gateway().await;
            let error = fetch_usage(
                &gw,
                &reqwest::Client::new(),
                &format!("http://{address}"),
                "SECRET-TOKEN",
                &Default::default(),
                None,
            )
            .await
            .unwrap_err()
            .to_string();
            task.await.unwrap();
            // The listener is gone: a failed query still consumes the interval.
            let repeat = fetch_usage(
                &gw,
                &reqwest::Client::new(),
                &format!("http://{address}"),
                "SECRET-TOKEN",
                &Default::default(),
                None,
            )
            .await
            .unwrap_err();
            assert!(repeat.is::<super::super::usage_throttle::Cooldown>());
            assert!(
                !error.contains("PRIVATE")
                    && !error.contains("SECRET")
                    && !error.contains(&address.to_string())
            );
            assert_eq!(
                error.starts_with("Claude authentication failed"),
                status == 401
            );
        }
    }

    #[tokio::test]
    async fn cancelled_usage_query_keeps_its_reservation() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (started, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 4096];
            socket.read(&mut bytes).await.unwrap();
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let gw = gateway().await;
        let cloned = gw.clone();
        let url = base.clone();
        let caller = tokio::spawn(async move {
            fetch_usage(
                &cloned,
                &reqwest::Client::new(),
                &url,
                "SECRET",
                &Default::default(),
                None,
            )
            .await
        });
        received.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let error = fetch_usage(
            &gw,
            &reqwest::Client::new(),
            &base,
            "SECRET",
            &Default::default(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.is::<super::super::usage_throttle::Cooldown>());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn old_serialized_usage_has_unknown_plan() {
        let usage: ClaudeUsage = serde_json::from_value(json!({"windows":[{
            "name":"five_hour", "used_percent":25.0, "resets_at":null
        }]}))
        .unwrap();
        assert_eq!(usage.plan, None);
        assert_eq!(usage.windows[0].used_percent, 25.0);
        assert_eq!(
            serde_json::from_str::<ClaudeUsage>("{}").unwrap(),
            ClaudeUsage::default()
        );
    }

    #[tokio::test]
    async fn account_usage_profile_is_credential_scoped_and_failure_is_unknown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            // Three credentials sharing one legacy provider cache key. The last
            // profile fails and must not inherit either previous plan.
            for _ in 0..6 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                let (status, body) = if request.starts_with("GET /api/oauth/usage ") {
                    (
                        200,
                        json!({"five_hour":{"utilization":20},"seven_day":{"utilization":30}}),
                    )
                } else if request.contains("Bearer token-pro") {
                    (
                        200,
                        json!({"organization":{"organization_type":"claude_pro"}}),
                    )
                } else if request.contains("Bearer token-max") {
                    (
                        200,
                        json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}}),
                    )
                } else {
                    (500, json!({"error":"private"}))
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = reqwest::Client::new();
        for (token, expected) in [
            ("token-pro", Some("pro")),
            ("token-max", Some("max 20x")),
            ("token-failed", None),
        ] {
            let gw = gateway().await;
            let usage = fetch_usage_with_plan(
                &gw,
                &client,
                &base,
                token,
                &Default::default(),
                None,
                "same-provider",
            )
            .await
            .unwrap();
            assert_eq!(usage.plan.as_deref(), expected);
            assert_eq!(usage.windows.len(), 2);
        }
        task.await.unwrap();
        // The server is gone: a same-credential hit still resolves from cache.
        assert_eq!(
            fetch_plan(&client, &base, "token-pro", &Default::default())
                .await
                .as_deref(),
            Some("pro")
        );
    }

    #[test]
    fn maps_session_week_and_model_buckets() {
        let body = json!({
            "five_hour": {"utilization": 37.5, "resets_at": "2026-10-28T18:00:00Z"},
            "seven_day": {"utilization": 12, "resets_at": "2026-11-02T00:00:00Z"},
            "seven_day_sonnet": {"utilization": 4, "resets_at": "2026-11-02T00:00:00Z"},
            "seven_day_opus": null,
            "seven_day_oauth_apps": {"utilization": null, "resets_at": null},
            "limits": [
                {"kind": "weekly_scoped", "group": "weekly", "percent": 61, "resets_at": "2026-11-02T00:00:00Z",
                 "severity": "normal", "is_active": true, "scope": {"model": {"display_name": "Fable"}}},
                {"kind": "spend", "group": "monthly", "percent": 54, "resets_at": "2026-11-01T00:00:00Z",
                 "severity": "normal", "is_active": true}
            ],
            "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null}
        });
        let usage = parse_usage(&body);
        let names: Vec<&str> = usage.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "five_hour",
                "weekly_limit",
                "feature:sonnet:weekly_limit",
                "feature:fable:weekly_limit"
            ]
        );
        assert_eq!(usage.windows[0].used_percent, 37.5);
        assert_eq!(
            usage.windows[0].resets_at.as_deref(),
            Some("2026-10-28T18:00:00Z")
        );
        assert_eq!(usage.windows[3].used_percent, 61.0);
    }

    #[test]
    fn public_usage_round_trips_only_typed_windows() {
        let usage = parse_usage(&json!({
            "five_hour":{"utilization":23.5,"resets_at":"2030-01-01T00:00:00Z"},
            "seven_day":{"utilization":40},
            "secret":"PRIVATE", "extra_usage":{"is_enabled":true,"opaque":"PRIVATE"}
        }));
        let serialized = serde_json::to_string(&usage).unwrap();
        assert!(!serialized.contains("PRIVATE") && !serialized.contains("extra_usage"));
        let restored: ClaudeUsage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored, usage);
        assert_eq!(
            serde_json::to_value(restored).unwrap()["windows"][1]["name"],
            "weekly_limit"
        );
    }

    #[test]
    fn plan_badge_follows_claude_code_mapping() {
        let plan = |organization_type: &str, tier: &str| {
            parse_plan(&json!({"organization": {
                "organization_type": organization_type, "rate_limit_tier": tier
            }}))
        };
        assert_eq!(
            plan("claude_max", "default_claude_max_20x").as_deref(),
            Some("max 20x")
        );
        assert_eq!(
            plan("claude_max", "default_claude_max_5x").as_deref(),
            Some("max 5x")
        );
        assert_eq!(plan("claude_max", "").as_deref(), Some("max"));
        assert_eq!(
            plan("claude_pro", "default_claude_zero").as_deref(),
            Some("pro")
        );
        assert_eq!(plan("claude_team", "").as_deref(), Some("team"));
        assert_eq!(plan("claude_enterprise", "").as_deref(), Some("enterprise"));
        assert_eq!(plan("api_org", ""), None);
        assert_eq!(parse_plan(&json!({})), None);
    }

    #[test]
    fn clamps_and_tolerates_empty_bodies() {
        let usage = parse_usage(&json!({"five_hour": {"utilization": 140, "resets_at": null}}));
        assert_eq!(usage.windows[0].used_percent, 100.0);
        assert_eq!(usage.windows[0].resets_at, None);
        assert!(parse_usage(&json!({})).windows.is_empty());
    }
}
