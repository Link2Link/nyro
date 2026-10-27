//! Downstream side of nyro→nyro usage forwarding.
//!
//! A Nyro-preset provider reads the remote's `GET /v1/nyro/usage` report (see
//! `admin::usage_forward`) with the same key it routes with, and passes the
//! report through untouched, so a newer remote's fields reach the WebUI without
//! a local upgrade. Only two things are interpreted here:
//!
//! - the transport errors, mapped to actionable messages (an older remote
//!   answers 404);
//! - this provider's own scheduling: it pauses only when **every** upstream
//!   provider in the report is paused by the remote, because then no route
//!   behind this key can be served. Any upstream that is eligible, failed to
//!   report, or has no usage backend keeps the provider eligible.

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::Value;
use std::time::Duration;

use super::nyro_build_url;
use crate::router::quota::QuotaTierObservation;

/// Remote route serving the forwarded usage report.
pub const USAGE_PATH: &str = "/v1/nyro/usage";
/// `object` marker of a forwarded usage report.
pub const REPORT_OBJECT: &str = "nyro.usage";
/// Synthetic scheduling tier set when every upstream provider is paused.
pub const UPSTREAM_TIER: &str = "nyro_upstream";

/// The remote may refresh several vendor quotas before answering.
const USAGE_TIMEOUT: Duration = Duration::from_secs(30);

fn remote_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// Read the remote report. The returned value is the remote's report with
/// nested forwarding data stripped (a report is one hop deep).
pub(crate) async fn fetch_forwarded_usage(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<Value> {
    let url = nyro_build_url(base_url, USAGE_PATH);
    let response = client
        .get(&url)
        .bearer_auth(api_key)
        .timeout(USAGE_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("usage forwarding request failed: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == StatusCode::NOT_FOUND {
        anyhow::bail!(
            "the remote Nyro does not support usage forwarding (HTTP 404 on {USAGE_PATH}); \
             upgrade the remote to a version that serves it"
        );
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        anyhow::bail!(
            "authentication failed (HTTP {status}): {}; use the remote's NYRO_PROXY_AUTH_KEY \
             or an API key created on it",
            remote_error_message(&body)
        );
    }
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {}", remote_error_message(&body));
    }
    let mut report: Value = serde_json::from_str(&body)
        .map_err(|error| anyhow::anyhow!("failed to parse forwarded usage report: {error}"))?;
    if report.get("object").and_then(Value::as_str) != Some(REPORT_OBJECT)
        || !report.get("providers").is_some_and(Value::is_array)
    {
        anyhow::bail!("unexpected usage forwarding response from the remote Nyro");
    }
    if let Some(providers) = report.get_mut("providers").and_then(Value::as_array_mut) {
        for provider in providers {
            if let Some(usage) = provider.get_mut("usage").and_then(Value::as_object_mut) {
                usage.remove("upstream");
                usage.remove("account_pool");
            }
        }
    }
    Ok(report)
}

fn is_paused(provider: &Value) -> bool {
    provider.get("status").and_then(Value::as_str) == Some("ok")
        && provider
            .pointer("/usage/scheduling/status")
            .and_then(Value::as_str)
            == Some("quota_exhausted")
}

/// Scheduling observation for the local provider: one exhausted synthetic tier
/// when every upstream provider is paused (resetting when the first of them
/// does), otherwise none.
pub(crate) fn upstream_scheduling_observation(report: &Value) -> Vec<QuotaTierObservation> {
    let Some(providers) = report.get("providers").and_then(Value::as_array) else {
        return Vec::new();
    };
    if providers.is_empty() || !providers.iter().all(is_paused) {
        return Vec::new();
    }
    let earliest_reset = providers
        .iter()
        .filter_map(|provider| {
            provider
                .pointer("/usage/scheduling/reset_at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
        })
        .min();
    vec![QuotaTierObservation {
        name: UPSTREAM_TIER.to_string(),
        used_percent: 100.0,
        resets_at: earliest_reset.map(|value| value.to_rfc3339()),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(status: &str, scheduling: Option<(&str, Option<&str>)>) -> Value {
        let mut value = json!({"name": "p", "routes": ["r"], "status": status});
        if let Some((state, reset_at)) = scheduling {
            value["usage"] = json!({"scheduling": {"status": state, "reset_at": reset_at}});
        }
        value
    }

    #[test]
    fn pauses_only_when_every_upstream_is_paused() {
        let paused_a = entry(
            "ok",
            Some(("quota_exhausted", Some("2026-10-27T12:00:00Z"))),
        );
        let paused_b = entry(
            "ok",
            Some(("quota_exhausted", Some("2026-10-27T10:00:00+00:00"))),
        );
        let tiers = upstream_scheduling_observation(&json!({"providers": [paused_a, paused_b]}));
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].name, UPSTREAM_TIER);
        assert_eq!(tiers[0].used_percent, 100.0);
        // The provider recovers when the first upstream does.
        assert_eq!(
            tiers[0].resets_at.as_deref(),
            Some("2026-10-27T10:00:00+00:00")
        );

        for other in [
            entry("ok", Some(("eligible", None))),
            entry("error", None),
            entry("unsupported", None),
        ] {
            let report =
                json!({"providers": [entry("ok", Some(("quota_exhausted", None))), other]});
            assert!(upstream_scheduling_observation(&report).is_empty());
        }
        assert!(upstream_scheduling_observation(&json!({"providers": []})).is_empty());
        assert!(upstream_scheduling_observation(&json!({})).is_empty());
    }

    #[test]
    fn unknown_reset_times_leave_the_reset_open() {
        let report = json!({"providers": [entry("ok", Some(("quota_exhausted", None)))]});
        let tiers = upstream_scheduling_observation(&report);
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].resets_at.is_none());
    }

    #[test]
    fn remote_error_message_prefers_the_json_error() {
        assert_eq!(
            remote_error_message(r#"{"error":{"message":"Invalid API key"}}"#),
            "Invalid API key"
        );
        assert_eq!(remote_error_message("plain"), "plain");
    }
}
