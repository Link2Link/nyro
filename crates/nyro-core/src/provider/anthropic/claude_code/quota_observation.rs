//! Bounded, account-scoped observations from inference response headers.
//!
//! These are deliberately not full usage snapshots or scheduling evidence. No
//! HTTP request, task, registry update, or authorization transition belongs here.
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::Gateway;
use crate::db::models::OAuthCredential;
use crate::provider::account_pool::ClaudeAccountState;
use crate::proxy::client::ResponseObserver;

const SOURCE: &str = "inference_header";
const MAX_HEADER_BYTES: usize = 96;
const MAX_DUPLICATES: usize = 4;
const MAX_CAS_ATTEMPTS: usize = 3;
const CLOCK_SKEW_SECONDS: i64 = 300;
static LAST_REQUEST_START_NS: AtomicI64 = AtomicI64::new(0);

/// Partial evidence, not a successful full usage refresh.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClaudeQuotaObservation {
    pub windows: Vec<ClaudeQuotaWindowObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaudeQuotaWindowObservation {
    pub name: String,
    pub used_percent: Option<f64>,
    pub status: Option<String>,
    /// UTC RFC3339; absent means unknown, not an expired or renewed window.
    pub resets_at: Option<String>,
    /// Header receipt time, in UTC Unix milliseconds.
    pub observed_at: i64,
    pub source: String,
}

/// Storage-only watermark. Never nested in the public observation DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ClaudeQuotaObservationOrder {
    five_hour: Option<WindowOrder>,
    weekly_limit: Option<WindowOrder>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WindowOrder {
    request_started_at_ns: i64,
    // Retain the reset high-water mark even when a newer partial observation
    // has no reset. This is ordering evidence, not a synthesized public field.
    reset_at_ms: Option<i64>,
}

impl ClaudeQuotaObservationOrder {
    fn slot(&mut self, name: &str) -> Option<&mut Option<WindowOrder>> {
        match name {
            "five_hour" => Some(&mut self.five_hour),
            "weekly_limit" => Some(&mut self.weekly_limit),
            _ => None,
        }
    }
}

// Read only explicitly recognized fields, never copy arbitrary headers. Repeated
// equal values are harmless; conflicting or excessive duplicates fail closed.
fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut result = None;
    for (index, value) in headers.get_all(name).iter().enumerate() {
        if index >= MAX_DUPLICATES || value.as_bytes().len() > MAX_HEADER_BYTES {
            return Err(());
        }
        let value = value.to_str().map_err(|_| ())?.trim();
        if value.is_empty() || result.is_some_and(|old| old != value) {
            return Err(());
        }
        result = Some(value);
    }
    Ok(result)
}

fn parse_ratio(value: &str) -> Result<f64, ()> {
    let ratio: f64 = value.parse().map_err(|_| ())?;
    if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
        return Err(());
    }
    Ok(ratio * 100.0)
}

fn parse_status(value: &str) -> Result<String, ()> {
    match value {
        "allowed" | "allowed_warning" | "rejected" => Ok(value.to_owned()),
        _ => Err(()),
    }
}

fn parse_reset(value: &str, now_ms: i64, period_seconds: i64) -> Result<String, ()> {
    // Unified inference headers use epoch seconds, not a relative duration or
    // milliseconds. Also accept an explicit RFC3339 instant, normalizing to UTC.
    let reset = if value.bytes().all(|b| b.is_ascii_digit()) {
        DateTime::from_timestamp(value.parse::<i64>().map_err(|_| ())?, 0).ok_or(())?
    } else {
        DateTime::parse_from_rfc3339(value)
            .map_err(|_| ())?
            .with_timezone(&Utc)
    };
    let delta = reset.timestamp_millis().checked_sub(now_ms).ok_or(())?;
    let horizon_ms = (period_seconds + CLOCK_SKEW_SECONDS) * 1000;
    if !(-horizon_ms..=horizon_ms).contains(&delta) {
        return Err(());
    }
    Ok(reset.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn parse_window(
    headers: &HeaderMap,
    prefix: &str,
    name: &str,
    period_seconds: i64,
    observed_at: i64,
) -> Result<Option<ClaudeQuotaWindowObservation>, ()> {
    let used_percent = header_value(headers, &format!("{prefix}-utilization"))?
        .map(parse_ratio)
        .transpose()?;
    let status = header_value(headers, &format!("{prefix}-status"))?
        .map(parse_status)
        .transpose()?;
    let resets_at = header_value(headers, &format!("{prefix}-reset"))?
        .map(|value| parse_reset(value, observed_at, period_seconds))
        .transpose()?;
    if used_percent.is_none() && status.is_none() && resets_at.is_none() {
        return Ok(None);
    }
    Ok(Some(ClaudeQuotaWindowObservation {
        name: name.to_owned(),
        used_percent,
        status,
        resets_at,
        observed_at,
        source: SOURCE.to_owned(),
    }))
}

/// Only 5h and global 7d windows are understood. Invalid evidence invalidates
/// its whole window, not an unrelated valid sibling window.
fn parse_headers(headers: &HeaderMap, observed_at: i64) -> Option<ClaudeQuotaObservation> {
    let windows: Vec<_> = [
        ("anthropic-ratelimit-unified-5h", "five_hour", 5 * 3600),
        ("anthropic-ratelimit-unified-7d", "weekly_limit", 7 * 86400),
    ]
    .into_iter()
    .filter_map(|(prefix, name, period)| {
        parse_window(headers, prefix, name, period, observed_at)
            .ok()
            .flatten()
    })
    .collect();
    (!windows.is_empty()).then_some(ClaudeQuotaObservation { windows })
}

fn origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    (matches!(url.scheme(), "https" | "http")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.origin().ascii_serialization())
}

fn inference_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .is_some_and(|url| url.path().trim_end_matches('/').ends_with("/v1/messages"))
}

fn reset_ms(window: &ClaudeQuotaWindowObservation) -> Option<i64> {
    DateTime::parse_from_rfc3339(window.resets_at.as_deref()?)
        .ok()
        .map(|reset| reset.timestamp_millis())
}

fn apply_observation(
    state: &mut ClaudeAccountState,
    incoming: &ClaudeQuotaObservation,
    request_started_at_ns: i64,
) -> bool {
    let mut observation = state.passive_usage.clone().unwrap_or_default();
    let mut order = state.passive_usage_order.clone().unwrap_or_default();
    let mut changed = false;
    for window in &incoming.windows {
        let Some(slot) = order.slot(&window.name) else {
            continue;
        };
        let previous = observation
            .windows
            .iter()
            .position(|old| old.name == window.name);
        let old_window = previous.map(|index| &observation.windows[index]);
        if slot
            .as_ref()
            .is_some_and(|old| request_started_at_ns <= old.request_started_at_ns)
            || (slot.is_none()
                && old_window
                    .is_some_and(|old| request_started_at_ns / 1_000_000 <= old.observed_at))
        {
            continue;
        }
        let old_reset = slot
            .as_ref()
            .and_then(|old| old.reset_at_ms)
            .or_else(|| old_window.and_then(reset_ms));
        let new_reset = reset_ms(window);
        if matches!((old_reset, new_reset), (Some(old), Some(new)) if new < old) {
            continue;
        }
        // Replace the WHOLE window. In particular, never retain an old percent
        // when the new cycle only tells us a status/reset (or vice versa).
        if let Some(index) = previous {
            observation.windows[index] = window.clone();
        } else {
            observation.windows.push(window.clone());
        }
        *slot = Some(WindowOrder {
            request_started_at_ns,
            reset_at_ms: new_reset.or(old_reset),
        });
        changed = true;
    }
    if changed {
        observation.windows.sort_by(|a, b| a.name.cmp(&b.name));
        state.passive_usage = Some(observation);
        state.passive_usage_order = Some(order);
    }
    changed
}

/// One inference attempt, bound to its selected credential generation and origin.
/// Deliberately not Debug: the Gateway and credential association are internal.
pub(crate) struct ClaudeQuotaObserver {
    gw: Gateway,
    provider_id: String,
    account_id: String,
    status_version: i32,
    rejection_revision: u64,
    request_started_at_ns: i64,
    origin: String,
}

impl ClaudeQuotaObserver {
    pub(crate) fn new(gw: Gateway, account: &OAuthCredential, upstream_url: &str) -> Option<Self> {
        if !matches!(account.driver_key.as_str(), "claude-code" | "claude")
            || !account.is_enabled
            || account.status != "connected"
            || account.access_token.trim().is_empty()
        {
            return None;
        }
        // Exclude count_tokens, OAuth, usage, and catalog requests even if a
        // caller accidentally installs this hook on a non-inference operation.
        if !inference_url(upstream_url) {
            return None;
        }
        // Unlike the display/scheduling parser, a failed state parse must never
        // fall back to defaults and erase a refusal while recording a header.
        let state: ClaudeAccountState = serde_json::from_str(&account.account_state).ok()?;
        let origin = origin(upstream_url)?;
        let now = Utc::now().timestamp_nanos_opt()?;
        let previous = LAST_REQUEST_START_NS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(now.max(old.saturating_add(1)))
            })
            .ok()?;
        Some(Self {
            gw,
            provider_id: account.provider_id.clone(),
            account_id: account.account_id.clone(),
            status_version: account.status_version,
            rejection_revision: state.rejection_revision,
            request_started_at_ns: now.max(previous.saturating_add(1)),
            origin,
        })
    }
}

#[async_trait]
impl ResponseObserver for ClaudeQuotaObserver {
    async fn observe_response_headers(&self, status: u16, headers: &HeaderMap, response_url: &str) {
        if !((200..300).contains(&status) || status == 429)
            || !inference_url(response_url)
            || origin(response_url).as_deref() != Some(self.origin.as_str())
        {
            return;
        }
        let Some(observation) = parse_headers(headers, Utc::now().timestamp_millis()) else {
            return;
        };
        let store = self.gw.storage.oauth_credentials();
        for _ in 0..MAX_CAS_ATTEMPTS {
            let Ok(Some(current)) = store.get_account(&self.provider_id, &self.account_id).await
            else {
                return;
            };
            if current.status_version != self.status_version
                || !current.is_enabled
                || current.status != "connected"
                || current.access_token.trim().is_empty()
                || !matches!(current.driver_key.as_str(), "claude-code" | "claude")
            {
                return;
            }
            let Ok(mut state) = serde_json::from_str::<ClaudeAccountState>(&current.account_state)
            else {
                return;
            };
            if state.rejection_revision != self.rejection_revision
                || state
                    .observed_at
                    .is_some_and(|at| self.request_started_at_ns / 1_000_000 < at)
                || !apply_observation(&mut state, &observation, self.request_started_at_ns)
            {
                return;
            }
            // Touch only our two keys, preserving full usage, refusal state,
            // and even future/unknown account-state fields exactly as JSON.
            let Ok(serde_json::Value::Object(mut raw_state)) =
                serde_json::from_str::<serde_json::Value>(&current.account_state)
            else {
                return;
            };
            let (Ok(passive), Ok(order)) = (
                serde_json::to_value(&state.passive_usage),
                serde_json::to_value(&state.passive_usage_order),
            ) else {
                return;
            };
            raw_state.insert("passive_usage".into(), passive);
            raw_state.insert("passive_usage_order".into(), order);
            let Ok(raw) = serde_json::to_string(&raw_state) else {
                return;
            };
            match store
                .compare_account_state(
                    &self.provider_id,
                    &self.account_id,
                    self.status_version,
                    &current.account_state,
                    &raw,
                )
                .await
            {
                Ok(false) => continue,
                Ok(true) | Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{OAuthAccountUpdate, Provider, UpsertOAuthCredential};
    use reqwest::header::{HeaderName, HeaderValue};
    use serde_json::{Value, json};

    const URL: &str = "https://api.anthropic.com/v1/messages?beta=true";
    const NOW: i64 = 1_800_000_000_000;

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    fn five_hour(ratio: &str, reset: i64) -> HeaderMap {
        headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", ratio),
            ("anthropic-ratelimit-unified-5h-reset", &reset.to_string()),
        ])
    }

    #[test]
    fn parser_maps_only_known_windows_and_converts_ratios() {
        let mut headers = five_hour("0.25", NOW / 1000 + 3600);
        headers.extend(self::headers(&[
            ("anthropic-ratelimit-unified-7d-utilization", "1"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-opus-utilization", "0.9"),
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-requests-remaining", "0"),
            ("authorization", "Bearer PRIVATE"),
            ("x-request-id", "PRIVATE-REQUEST"),
        ]));
        let observation = parse_headers(&headers, NOW).unwrap();
        assert_eq!(observation.windows.len(), 2);
        let five = &observation.windows[0];
        assert_eq!(five.name, "five_hour");
        assert_eq!(five.used_percent, Some(25.0));
        assert_eq!(five.status, None);
        assert_eq!(reset_ms(five), Some(NOW + 3_600_000));
        assert_eq!(five.observed_at, NOW);
        assert_eq!(five.source, "inference_header");
        let seven = &observation.windows[1];
        assert_eq!(seven.name, "weekly_limit");
        assert_eq!(seven.used_percent, Some(100.0));
        assert_eq!(seven.status.as_deref(), Some("rejected"));
        assert_eq!(seven.resets_at, None);
        let value = serde_json::to_value(&observation).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_eq!(value["windows"][0].as_object().unwrap().len(), 6);
        assert!(!value.to_string().contains("PRIVATE"));
        assert!(!value.to_string().contains("request_started"));
    }

    #[test]
    fn missing_status_and_reset_never_invent_percent() {
        assert!(parse_headers(&HeaderMap::new(), NOW).is_none());
        assert!(
            parse_headers(
                &headers(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-7d-sonnet-utilization", "0.2"),
                ]),
                NOW
            )
            .is_none()
        );
        for status in ["allowed", "allowed_warning", "rejected"] {
            let parsed = parse_headers(
                &headers(&[("anthropic-ratelimit-unified-5h-status", status)]),
                NOW,
            )
            .unwrap();
            assert_eq!(parsed.windows.len(), 1);
            assert_eq!(parsed.windows[0].used_percent, None);
            assert_eq!(parsed.windows[0].resets_at, None);
        }
        let reset = (NOW / 1000 + 60).to_string();
        let parsed = parse_headers(
            &headers(&[("anthropic-ratelimit-unified-7d-reset", &reset)]),
            NOW,
        )
        .unwrap();
        assert_eq!(parsed.windows[0].used_percent, None);
        assert_eq!(parsed.windows[0].status, None);
        assert_eq!(
            parse_headers(&five_hour("0", NOW / 1000), NOW)
                .unwrap()
                .windows[0]
                .used_percent,
            Some(0.0)
        );
    }

    #[test]
    fn invalid_percent_status_or_reset_rejects_whole_window() {
        for ratio in ["NaN", "inf", "-inf", "-0.01", "1.01", "25", "", "0.2,0.3"] {
            assert!(
                parse_headers(&five_hour(ratio, NOW / 1000), NOW).is_none(),
                "{ratio}"
            );
        }
        for status in ["unknown", "blocked", "PRIVATE", "", "allowed,rejected"] {
            assert!(
                parse_headers(
                    &headers(&[
                        ("anthropic-ratelimit-unified-5h-utilization", "0.5"),
                        ("anthropic-ratelimit-unified-5h-status", status),
                    ]),
                    NOW
                )
                .is_none(),
                "{status}"
            );
        }
        for reset in [
            "0",
            "-1",
            "NaN",
            "1800000000000",
            "1800000000.5",
            "9999999999999999999",
            "2039-01-01T00:00:00Z",
        ] {
            assert!(
                parse_headers(
                    &headers(&[
                        ("anthropic-ratelimit-unified-5h-utilization", "0.5"),
                        ("anthropic-ratelimit-unified-5h-reset", reset),
                    ]),
                    NOW
                )
                .is_none(),
                "{reset}"
            );
        }
        let reset = DateTime::from_timestamp(NOW / 1000 + 60, 0)
            .unwrap()
            .to_rfc3339();
        assert!(
            parse_headers(
                &headers(&[("anthropic-ratelimit-unified-5h-reset", &reset),]),
                NOW
            )
            .unwrap()
            .windows[0]
                .resets_at
                .as_ref()
                .unwrap()
                .ends_with('Z')
        );
    }

    #[test]
    fn conflicts_duplicates_and_oversized_headers_fail_closed() {
        for suffix in ["utilization", "status", "reset"] {
            let name = format!("anthropic-ratelimit-unified-5h-{suffix}");
            let (a, b) = match suffix {
                "utilization" => ("0.2", "0.3"),
                "status" => ("allowed", "rejected"),
                _ => ("1800000000", "1800000001"),
            };
            assert!(parse_headers(&headers(&[(&name, a), (&name, b)]), NOW).is_none());
            assert!(parse_headers(&headers(&[(&name, a), (&name, a)]), NOW).is_some());
            let repeated = vec![(name.as_str(), a); MAX_DUPLICATES + 1];
            assert!(parse_headers(&headers(&repeated), NOW).is_none());
        }
        let long = "0".repeat(MAX_HEADER_BYTES + 1);
        assert!(
            parse_headers(
                &headers(&[("anthropic-ratelimit-unified-5h-utilization", &long),]),
                NOW
            )
            .is_none()
        );
        let mut malformed = HeaderMap::new();
        malformed.insert(
            "anthropic-ratelimit-unified-5h-status",
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(parse_headers(&malformed, NOW).is_none());
        malformed.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("0.4"),
        );
        let parsed = parse_headers(&malformed, NOW).unwrap();
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(parsed.windows[0].name, "weekly_limit");
    }

    fn provider() -> Provider {
        serde_json::from_value(json!({
            "id":"quota-observer", "name":"Claude", "vendor":"anthropic",
            "protocol":"anthropic-messages/messages/2023-06-01", "base_url":"https://api.anthropic.com",
            "protocol_mode":"fixed", "protocol_endpoints":[], "keys":[],
            "preset_key":"anthropic", "channel":"claude-code", "api_key":"", "auth_mode":"oauth",
            "use_proxy":false, "fast_mode":false, "is_enabled":true, "created_at":"", "updated_at":""
        })).unwrap()
    }

    fn credential(label: &str) -> UpsertOAuthCredential {
        UpsertOAuthCredential {
            driver_key: "claude-code".into(),
            scheme: "oauth_auth_code_pkce".into(),
            access_token: format!("PRIVATE-{label}"),
            refresh_token: Some(format!("PRIVATE-REFRESH-{label}")),
            subject_id: Some(label.into()),
            ..Default::default()
        }
    }

    async fn fixture() -> (Gateway, OAuthCredential, OAuthCredential) {
        let storage = std::sync::Arc::new(crate::storage::MemoryStorage::new(
            vec![provider()],
            vec![],
            vec![],
        ));
        let gw = Gateway::from_storage(crate::config::GatewayConfig::default(), storage)
            .await
            .unwrap()
            .0;
        let store = gw.storage.oauth_credentials();
        let a = store
            .create_account("quota-observer", "a", credential("a"))
            .await
            .unwrap();
        let b = store
            .create_account("quota-observer", "b", credential("b"))
            .await
            .unwrap();
        (gw, a, b)
    }

    async fn current(gw: &Gateway, account: &OAuthCredential) -> OAuthCredential {
        gw.storage
            .oauth_credentials()
            .get_account(&account.provider_id, &account.account_id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn set_state(gw: &Gateway, account: &OAuthCredential, state: Value) -> OAuthCredential {
        let raw = state.to_string();
        assert!(
            gw.storage
                .oauth_credentials()
                .compare_account_state(
                    &account.provider_id,
                    &account.account_id,
                    account.status_version,
                    &account.account_state,
                    &raw,
                )
                .await
                .unwrap()
        );
        current(gw, account).await
    }

    fn observer(gw: &Gateway, account: &OAuthCredential) -> ClaudeQuotaObserver {
        ClaudeQuotaObserver::new(gw.clone(), account, URL).unwrap()
    }

    fn read_state(account: &OAuthCredential) -> ClaudeAccountState {
        serde_json::from_str(&account.account_state).unwrap()
    }

    #[tokio::test]
    async fn observation_preserves_full_usage_auth_unknown_fields_and_sibling() {
        let (gw, a, b) = fixture().await;
        let baseline = json!({
            "usage":{"windows":[{"name":"five_hour", "used_percent":100.0, "resets_at":null}], "plan":"max 5x"},
            "observed_at":123, "usage_stale":true, "auth_invalid":true,
            "hard_quota":true, "hard_quota_reason":"verified_refusal", "rejection_revision":9,
            "future_authoritative_field":{"keep":[1,2,3]}
        });
        let a = set_state(&gw, &a, baseline.clone()).await;
        let before_provider =
            serde_json::to_value(gw.quota_registry.snapshot(&a.provider_id)).unwrap();
        let before_account =
            serde_json::to_value(gw.account_quota_registry.snapshot(&a.account_id)).unwrap();
        let hook = observer(&gw, &a);
        hook.observe_response_headers(
            429,
            &headers(&[("anthropic-ratelimit-unified-5h-status", "rejected")]),
            URL,
        )
        .await;
        let updated = current(&gw, &a).await;
        let mut raw: Value = serde_json::from_str(&updated.account_state).unwrap();
        let passive = raw
            .as_object_mut()
            .unwrap()
            .remove("passive_usage")
            .unwrap();
        assert!(
            raw.as_object_mut()
                .unwrap()
                .remove("passive_usage_order")
                .is_some()
        );
        assert_eq!(raw, baseline);
        assert_eq!(passive["windows"][0]["used_percent"], Value::Null);
        assert_eq!(passive["windows"][0]["status"], "rejected");
        assert_eq!(updated.status_version, a.status_version);
        assert_eq!(updated.status, a.status);
        assert_eq!(updated.updated_at, a.updated_at);
        observer(&gw, &updated)
            .observe_response_headers(200, &HeaderMap::new(), URL)
            .await;
        assert_eq!(current(&gw, &a).await.account_state, updated.account_state);
        assert_eq!(current(&gw, &b).await.account_state, b.account_state);
        assert_eq!(
            serde_json::to_value(gw.quota_registry.snapshot(&a.provider_id)).unwrap(),
            before_provider
        );
        assert_eq!(
            serde_json::to_value(gw.account_quota_registry.snapshot(&a.account_id)).unwrap(),
            before_account
        );
        assert_eq!(read_state(&updated).passive_usage.unwrap().windows.len(), 1);
    }

    #[tokio::test]
    async fn absent_headers_and_rejected_origins_or_statuses_are_noops() {
        let (gw, a, _) = fixture().await;
        let evidence = five_hour("0.5", Utc::now().timestamp() + 60);
        let hook = observer(&gw, &a);
        hook.observe_response_headers(200, &HeaderMap::new(), URL)
            .await;
        for status in [100, 301, 400, 401, 403, 500, 503] {
            hook.observe_response_headers(status, &evidence, URL).await;
        }
        for url in [
            "https://api.anthropic.com.evil.test/v1/messages",
            "https://api.anthropic.com:444/v1/messages",
            "http://api.anthropic.com/v1/messages",
            "https://other.test/v1/messages",
            "https://user@api.anthropic.com/v1/messages",
            "https://api.anthropic.com/api/oauth/usage",
            "https://api.anthropic.com/v1/messages/count_tokens",
            "not a URL",
        ] {
            hook.observe_response_headers(200, &evidence, url).await;
        }
        assert_eq!(current(&gw, &a).await.account_state, a.account_state);
        // Explicit default port is the same origin, not a string-prefix match.
        hook.observe_response_headers(201, &evidence, "https://api.anthropic.com:443/v1/messages")
            .await;
        assert_eq!(
            read_state(&current(&gw, &a).await)
                .passive_usage
                .unwrap()
                .windows[0]
                .used_percent,
            Some(50.0)
        );
    }

    #[tokio::test]
    async fn constructor_rejects_non_inference_or_ineligible_accounts() {
        let (gw, a, _) = fixture().await;
        for url in [
            "https://api.anthropic.com/v1/messages/count_tokens",
            "https://api.anthropic.com/api/oauth/usage",
            "https://api.anthropic.com/v1/models",
            "file:///v1/messages",
        ] {
            assert!(ClaudeQuotaObserver::new(gw.clone(), &a, url).is_none());
        }
        for status in ["disconnected", "refreshing", "error"] {
            let mut changed = a.clone();
            changed.status = status.into();
            assert!(ClaudeQuotaObserver::new(gw.clone(), &changed, URL).is_none());
        }
        let mut changed = a.clone();
        changed.is_enabled = false;
        assert!(ClaudeQuotaObserver::new(gw.clone(), &changed, URL).is_none());
        changed = a.clone();
        changed.driver_key = "openai".into();
        assert!(ClaudeQuotaObserver::new(gw.clone(), &changed, URL).is_none());
        changed = a.clone();
        changed.account_state = "malformed".into();
        assert!(ClaudeQuotaObserver::new(gw.clone(), &changed, URL).is_none());
    }

    #[tokio::test]
    async fn newer_rejection_revision_blocks_late_observation_even_if_flags_unchanged() {
        let (gw, a, _) = fixture().await;
        let a = set_state(
            &gw,
            &a,
            json!({"hard_quota":true, "auth_invalid":true, "rejection_revision":9}),
        )
        .await;
        let hook = observer(&gw, &a);
        let refused = set_state(
            &gw,
            &a,
            json!({"hard_quota":true, "auth_invalid":true, "rejection_revision":10}),
        )
        .await;
        hook.observe_response_headers(200, &five_hour("0.1", Utc::now().timestamp() + 60), URL)
            .await;
        assert_eq!(current(&gw, &a).await.account_state, refused.account_state);
    }

    #[tokio::test]
    async fn disable_logout_refresh_and_credential_replacement_fence_attempt() {
        for action in ["disable", "logout", "refresh", "replace"] {
            let (gw, a, b) = fixture().await;
            let hook = observer(&gw, &a);
            let store = gw.storage.oauth_credentials();
            match action {
                "disable" => {
                    store
                        .update_account(
                            &a.provider_id,
                            &a.account_id,
                            a.status_version,
                            OAuthAccountUpdate {
                                is_enabled: Some(false),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap()
                        .unwrap();
                }
                "logout" => {
                    assert!(
                        store
                            .disconnect_account(&a.provider_id, &a.account_id, a.status_version)
                            .await
                            .unwrap()
                    );
                }
                "refresh" => {
                    store
                        .try_begin_account_refresh(&a.provider_id, &a.account_id, a.status_version)
                        .await
                        .unwrap()
                        .unwrap();
                }
                _ => {
                    store
                        .replace_account(
                            &a.provider_id,
                            &a.account_id,
                            a.status_version,
                            credential("replacement"),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                }
            }
            let before = store
                .get_account(&a.provider_id, &a.account_id)
                .await
                .unwrap();
            hook.observe_response_headers(200, &five_hour("0.1", Utc::now().timestamp() + 60), URL)
                .await;
            let after = store
                .get_account(&a.provider_id, &a.account_id)
                .await
                .unwrap();
            assert_eq!(
                after.as_ref().map(|account| &account.account_state),
                before.as_ref().map(|account| &account.account_state),
                "{action}"
            );
            if action == "logout" {
                assert!(after.is_none());
            }
            assert_eq!(current(&gw, &b).await.account_state, b.account_state);
        }
    }

    #[tokio::test]
    async fn late_requests_and_old_resets_cannot_overwrite_newer_windows() {
        let (gw, a, _) = fixture().await;
        let reset = Utc::now().timestamp() + 3600;
        let old = observer(&gw, &a);
        let new = observer(&gw, &a);
        assert!(new.request_started_at_ns > old.request_started_at_ns);
        new.observe_response_headers(200, &five_hour("0.2", reset), URL)
            .await;
        let latest = current(&gw, &a).await;
        // Even a later reset in a late, older request cannot replace newer data.
        old.observe_response_headers(200, &five_hour("0.9", reset + 60), URL)
            .await;
        assert_eq!(current(&gw, &a).await.account_state, latest.account_state);
        let next = observer(&gw, &latest);
        next.observe_response_headers(200, &five_hour("0.8", reset - 60), URL)
            .await;
        assert_eq!(current(&gw, &a).await.account_state, latest.account_state);
        // Whole-window replacement drops old utilization/status across cycles.
        let next = observer(&gw, &latest);
        next.observe_response_headers(
            200,
            &headers(&[
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    &(reset + 120).to_string(),
                ),
            ]),
            URL,
        )
        .await;
        let latest = current(&gw, &a).await;
        let window = read_state(&latest).passive_usage.unwrap().windows.remove(0);
        assert_eq!(window.used_percent, None);
        assert_eq!(window.status.as_deref(), Some("allowed"));
        assert_eq!(reset_ms(&window), Some((reset + 120) * 1000));
        // Missing reset stays publicly missing, but does not discard the private
        // reset watermark and allow an older cycle to be reintroduced later.
        observer(&gw, &latest)
            .observe_response_headers(
                200,
                &headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.3")]),
                URL,
            )
            .await;
        let latest = current(&gw, &a).await;
        let window = read_state(&latest).passive_usage.unwrap().windows.remove(0);
        assert_eq!(window.used_percent, Some(30.0));
        assert_eq!(window.status, None);
        assert_eq!(window.resets_at, None);
        observer(&gw, &latest)
            .observe_response_headers(200, &five_hour("0.7", reset), URL)
            .await;
        assert_eq!(current(&gw, &a).await.account_state, latest.account_state);
    }

    #[tokio::test]
    async fn windows_order_independently_and_full_refresh_fences_late_attempt() {
        let (gw, a, _) = fixture().await;
        let old = observer(&gw, &a);
        let new = observer(&gw, &a);
        new.observe_response_headers(
            200,
            &headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.4")]),
            URL,
        )
        .await;
        old.observe_response_headers(
            200,
            &headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.8"),
                ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ]),
            URL,
        )
        .await;
        let latest = current(&gw, &a).await;
        let windows = read_state(&latest).passive_usage.unwrap().windows;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, Some(40.0));
        assert_eq!(windows[1].used_percent, None);
        assert_eq!(windows[1].status.as_deref(), Some("allowed_warning"));
        let hook = observer(&gw, &latest);
        let mut raw: Value = serde_json::from_str(&latest.account_state).unwrap();
        raw["observed_at"] = (hook.request_started_at_ns / 1_000_000 + 1).into();
        let refreshed = set_state(&gw, &latest, raw).await;
        hook.observe_response_headers(
            200,
            &headers(&[("anthropic-ratelimit-unified-5h-utilization", "0.6")]),
            URL,
        )
        .await;
        assert_eq!(
            current(&gw, &a).await.account_state,
            refreshed.account_state
        );
    }
}
