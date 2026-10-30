//! Downstream side of nyro→nyro usage forwarding.
//!
//! A Nyro-preset provider reads the remote's `GET /v1/nyro/usage` report (see
//! `admin::usage_forward`) with the same key it routes with, and passes the
//! report through with a privacy fence: account details/nested reports are
//! removed and anonymous pool quota is parsed through a fixed safe DTO. Other
//! newer remote fields still reach the WebUI without a local upgrade. We also interpret:
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

pub mod pool_quota;
use pool_quota::AnonymousPoolQuota;

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
    sanitize_forwarded_usage(&mut report);
    Ok(report)
}

fn sanitize_forwarded_usage(report: &mut Value) {
    if let Some(providers) = report.get_mut("providers").and_then(Value::as_array_mut) {
        for provider in providers {
            if let Some(usage) = provider.get_mut("usage").and_then(Value::as_object_mut) {
                usage.remove("upstream");
                usage.remove("account_pool");
                // Never relay an untyped nested pool payload: drop private or
                // unknown fields at every level, and omit malformed summaries.
                // Old reports without pool_quota remain unchanged.
                if let Some(raw) = usage.remove("pool_quota")
                    && let Ok(quota) = serde_json::from_value::<AnonymousPoolQuota>(raw)
                    && let Ok(safe) = serde_json::to_value(quota)
                {
                    usage.insert("pool_quota".into(), safe);
                }
            }
        }
    }
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

    fn pool_summary() -> Value {
        json!({
            "daily": null, "weekly": null, "estimated": true,
            "claude_windows": [{
                "name": "weekly_limit",
                "quota": {
                    "total_accounts": 2, "capacity_complete": true, "total_units": 2,
                    "known_accounts": 1, "known_remaining_units": 0.7,
                    "used_units": null, "remaining_units": null,
                    "used_percent": null, "remaining_percent": null,
                    "stale": true, "next_reset_at": "2030-01-07T00:00:00Z",
                    "next_reset_accounts": 1, "reset_mode": "staggered"
                }
            }]
        })
    }

    #[test]
    fn forwarding_keeps_pool_metadata_without_accepting_nested_private_fields() {
        for reset_mode in ["unknown", "aligned", "staggered"] {
            let mut expected = pool_summary();
            expected["claude_windows"][0]["quota"]["reset_mode"] = json!(reset_mode);
            if reset_mode == "unknown" {
                expected["claude_windows"][0]["quota"]["next_reset_at"] = Value::Null;
            }
            // Exercise the other wire-compatible windows too, without
            // changing which provider families generate them in this release.
            expected["daily"] = expected["claude_windows"][0]["quota"].clone();
            expected["weekly"] = expected["daily"].clone();
            let mut incoming = expected.clone();
            // Admin-only data hidden anywhere within the new object must not
            // survive, even when a remote accidentally serializes a larger DTO.
            for object in [
                "",
                "/daily",
                "/weekly",
                "/claude_windows/0",
                "/claude_windows/0/quota",
            ] {
                let object = incoming
                    .pointer_mut(object)
                    .unwrap()
                    .as_object_mut()
                    .unwrap();
                for key in [
                    "accounts",
                    "label",
                    "config",
                    "reserve_usd",
                    "account_id",
                    "extra",
                ] {
                    object.insert(key.into(), json!({"secret": "PRIVATE"}));
                }
            }
            let mut unknown = expected["claude_windows"][0].clone();
            unknown["name"] = json!("PRIVATE@example.org");
            incoming["claude_windows"]
                .as_array_mut()
                .unwrap()
                .push(unknown);
            let mut report = json!({"providers": [{
                "status": "ok", "usage": {
                    "tiers": [], "scheduling": {"status": "eligible"},
                    "account_pool": {"accounts": ["PRIVATE"]},
                    "upstream": {"secret": "PRIVATE"}, "pool_quota": incoming
                }
            }]});
            sanitize_forwarded_usage(&mut report);
            let usage = &report["providers"][0]["usage"];
            assert_eq!(usage["pool_quota"], expected);
            assert!(usage.get("account_pool").is_none());
            assert!(usage.get("upstream").is_none());
            assert!(!report.to_string().contains("PRIVATE"));
            assert_eq!(usage["tiers"], json!([]));
            assert!(upstream_scheduling_observation(&report).is_empty());
            // Even a fully spent display-only estimate cannot pause routing.
            report["providers"][0]["usage"]["pool_quota"]["claude_windows"][0]["quota"]["used_percent"] =
                json!(100);
            assert!(upstream_scheduling_observation(&report).is_empty());
        }
    }

    #[test]
    fn forwarding_legacy_reports_stays_compatible_and_malformed_summaries_fail_closed() {
        let old = json!({"object": "nyro.usage", "version": 1, "providers": [{
            "status": "ok", "usage": {"tiers": [{"name": "weekly_limit", "used_percent": 17}]}
        }]});
        let mut report = old.clone();
        sanitize_forwarded_usage(&mut report);
        assert_eq!(report, old);
        for invalid in [
            Value::Null,
            json!({"accounts": ["PRIVATE"]}),
            json!("PRIVATE"),
        ] {
            report["providers"][0]["usage"]["pool_quota"] = invalid;
            sanitize_forwarded_usage(&mut report);
            assert_eq!(report, old);
        }
        for (key, invalid) in [
            ("next_reset_at", json!("PRIVATE@example.org")),
            ("reset_mode", json!("PRIVATE@example.org")),
            ("used_percent", json!({"accounts": ["PRIVATE"]})),
        ] {
            let mut summary = pool_summary();
            summary["claude_windows"][0]["quota"][key] = invalid;
            report["providers"][0]["usage"]["pool_quota"] = summary;
            sanitize_forwarded_usage(&mut report);
            assert_eq!(
                report["providers"][0]["usage"]["pool_quota"]["claude_windows"],
                json!([])
            );
            assert!(!report.to_string().contains("PRIVATE"));
        }
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
