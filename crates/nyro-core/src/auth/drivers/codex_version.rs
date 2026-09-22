//! Best-effort probe of the latest `@openai/codex` CLI version from npm.
//!
//! `chatgpt.com/backend-api/codex` content-negotiates on the advertised
//! `client_version`: the models manifest hides models newer than the
//! client and 404s below 0.144.0. Instead of relying solely on a compile-time
//! constant that drifts stale between gateway releases, the process probes
//! npm's unauthenticated dist-tags endpoint for the `latest` tag of
//! `@openai/codex` and caches the result in-process for a few hours.
//!
//! Every failure mode — offline, DNS, rate limit, malformed payload,
//! implausible version — falls back to the compile-time constant so model
//! discovery keeps working exactly as before.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Compile-time fallback used whenever the probe fails or has not answered
/// yet. Keep in sync with `CODEX_USER_AGENT` and the codex channel's
/// `models_client_version` in provider/openai/mod.rs (guarded by a unit
/// test in drivers/openai.rs).
pub(crate) const FALLBACK_CODEX_CLIENT_VERSION: &str = "0.156.0";

/// Unauthenticated, plain-JSON `{tag: version}` map. Preferred over the
/// GitHub releases API, which rate-limits anonymous callers.
const NPM_DIST_TAGS_URL: &str = "https://registry.npmjs.org/-/package/@openai%2fcodex/dist-tags";
/// Upstream 404s below this version; anything older reported by the probe is
/// treated as implausible and discarded in favor of the fallback.
const MIN_CODEX_VERSION: (u64, u64, u64) = (0, 144, 0);
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// Floor between failed probes so an unreachable registry is not hammered.
const FAILURE_RETRY_DELAY: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

struct CacheState {
    version: Option<(String, Instant)>,
    last_failure: Option<Instant>,
}

impl CacheState {
    const fn new() -> Self {
        Self {
            version: None,
            last_failure: None,
        }
    }

    fn fresh_version(&self) -> Option<String> {
        self.version
            .as_ref()
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(version, _)| version.clone())
    }
}

static CACHE: OnceLock<Mutex<CacheState>> = OnceLock::new();
/// Dedups detached refresh tasks spawned from synchronous bind paths.
static REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Serializes actual HTTP probes so concurrent resolvers share one request.
static FETCH_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn lock_cache() -> std::sync::MutexGuard<'static, CacheState> {
    CACHE
        .get_or_init(|| Mutex::new(CacheState::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Latest probed version if the cache is fresh, else the fallback. Never
/// performs I/O — safe to call from synchronous bind paths.
pub(crate) fn cached_or_fallback() -> String {
    lock_cache()
        .fresh_version()
        .unwrap_or_else(|| FALLBACK_CODEX_CLIENT_VERSION.to_string())
}

/// Resolve the codex client version, probing npm when the cache is stale.
/// Bound by `REQUEST_TIMEOUT`; returns the fallback on any failure.
pub(crate) async fn resolve(client: Option<&reqwest::Client>) -> String {
    if let Some(version) = fresh_or_throttled() {
        return version;
    }

    let _gate = FETCH_GATE.lock().await;
    // Another resolver may have refreshed (or just failed) while we waited
    // on the gate; re-check before issuing our own request.
    if let Some(version) = fresh_or_throttled() {
        return version;
    }

    match fetch_latest_tag(client).await {
        Ok(version) => {
            lock_cache().version = Some((version.clone(), Instant::now()));
            if version != FALLBACK_CODEX_CLIENT_VERSION {
                tracing::info!(
                    version,
                    fallback = FALLBACK_CODEX_CLIENT_VERSION,
                    "probed codex client version from npm dist-tags"
                );
            } else {
                tracing::debug!(
                    version,
                    "probed codex client version matches the compile-time fallback"
                );
            }
            version
        }
        Err(error) => {
            lock_cache().last_failure = Some(Instant::now());
            tracing::debug!(
                error = %error,
                fallback = FALLBACK_CODEX_CLIENT_VERSION,
                "codex dist-tags probe failed; using fallback client version"
            );
            FALLBACK_CODEX_CLIENT_VERSION.to_string()
        }
    }
}

/// Fire-and-forget variant for synchronous/hot paths: spawns a detached
/// refresh task when the cache is stale and returns immediately. No-op
/// outside a tokio runtime (e.g. plain unit tests).
pub(crate) fn spawn_refresh_if_stale(client: Option<reqwest::Client>) {
    if lock_cache().fresh_version().is_some() {
        return;
    }
    if REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return; // a detached refresh is already running
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        return;
    };
    // Reset the flag no matter how the task ends (including panic).
    struct ResetRefreshInFlight;
    impl Drop for ResetRefreshInFlight {
        fn drop(&mut self) {
            REFRESH_IN_FLIGHT.store(false, Ordering::Release);
        }
    }
    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.spawn(async move {
            let _reset = ResetRefreshInFlight;
            resolve(client.as_ref()).await;
        })
    }));
    if spawned.is_err() {
        // Runtime was shutting down; allow a later retry.
        REFRESH_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Fresh cached version, or a throttled-failure fallback: when the last
/// probe failed recently, serve the fallback without re-probing.
fn fresh_or_throttled() -> Option<String> {
    let state = lock_cache();
    if let Some(version) = state.fresh_version() {
        return Some(version);
    }
    match state.last_failure {
        Some(at) if at.elapsed() < FAILURE_RETRY_DELAY => {
            Some(FALLBACK_CODEX_CLIENT_VERSION.to_string())
        }
        _ => None,
    }
}

async fn fetch_latest_tag(client: Option<&reqwest::Client>) -> anyhow::Result<String> {
    let owned;
    let client = match client {
        Some(client) => client,
        None => {
            owned = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?;
            &owned
        }
    };
    let payload: Value = client
        .get(NPM_DIST_TAGS_URL)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    parse_dist_tags_latest(&payload)
        .ok_or_else(|| anyhow::anyhow!("dist-tags payload missing a plausible `latest` tag"))
}

/// Extract a plausible stable `latest` version from a dist-tags payload.
/// Prerelease strings (`0.157.0-alpha.9`), platform-tagged versions
/// (`0.156.0-linux-x64`), and versions older than the upstream minimum
/// are rejected so a corrupt payload cannot degrade the advertised client
/// version below the fallback.
fn parse_dist_tags_latest(payload: &Value) -> Option<String> {
    let version = payload.get("latest")?.as_str()?.trim();
    if !is_plausible_version(version) {
        return None;
    }
    Some(version.to_string())
}

fn is_plausible_version(version: &str) -> bool {
    parse_semver_triple(version).is_some_and(|triple| triple >= MIN_CODEX_VERSION)
}

fn parse_semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_latest_from_dist_tags_payload() {
        let payload = json!({
            "beta": "0.1.2505172116",
            "alpha": "0.157.0-alpha.9",
            "latest": "0.156.0",
            "linux-x64": "0.156.0-linux-x64"
        });
        assert_eq!(parse_dist_tags_latest(&payload).as_deref(), Some("0.156.0"));
    }

    #[test]
    fn falls_back_when_latest_is_missing_or_malformed() {
        assert_eq!(
            parse_dist_tags_latest(&json!({ "alpha": "0.157.0-alpha.9" })),
            None
        );
        assert_eq!(parse_dist_tags_latest(&json!({ "latest": "oops" })), None);
        assert_eq!(
            parse_dist_tags_latest(&json!({ "latest": "0.157.0-alpha.9" })),
            None,
            "prerelease tags must not be advertised"
        );
        assert_eq!(
            parse_dist_tags_latest(&json!({ "latest": "0.156.0-linux-x64" })),
            None,
            "platform-tagged versions must not be advertised"
        );
        assert_eq!(parse_dist_tags_latest(&json!({ "latest": "" })), None);
        assert_eq!(parse_dist_tags_latest(&json!({ "latest": 156 })), None);
        assert_eq!(parse_dist_tags_latest(&Value::Null), None);
    }

    #[test]
    fn rejects_versions_below_upstream_minimum() {
        assert_eq!(
            parse_dist_tags_latest(&json!({ "latest": "0.143.9" })),
            None,
            "upstream 404s below 0.144.0"
        );
        assert_eq!(
            parse_dist_tags_latest(&json!({ "latest": "0.144.0" })).as_deref(),
            Some("0.144.0")
        );
    }

    #[test]
    fn cached_or_fallback_serves_fallback_without_a_probe() {
        // Unit tests never populate the process-wide cache (no test calls
        // `resolve`), so the fallback is served deterministically.
        assert_eq!(cached_or_fallback(), FALLBACK_CODEX_CLIENT_VERSION);
    }
}
