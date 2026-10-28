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

use serde_json::Value;

use super::profile;

const USAGE_PATH: &str = "/api/oauth/usage";

/// One usage window, in nyro's tier vocabulary (see `admin::usage`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageWindow {
    /// `five_hour`, `weekly_limit`, or `feature:<name>:<window>`.
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
}

/// Parsed `/api/oauth/usage`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ClaudeUsage {
    pub windows: Vec<UsageWindow>,
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
        });
    }
    ClaudeUsage { windows }
}

/// Fetch and parse the subscription usage for one OAuth access token.
pub(crate) async fn fetch_usage(
    client: &reqwest::Client,
    api_base_url: &str,
    access_token: &str,
) -> anyhow::Result<ClaudeUsage> {
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
        .await
        .map_err(|error| anyhow::anyhow!("Claude usage request failed: {error}"))?;
    let status = response.status();
    let raw = response
        .bytes()
        .await
        .map_err(|error| anyhow::anyhow!("failed to read Claude usage response: {error}"))?;
    if !status.is_success() {
        let preview: String = String::from_utf8_lossy(&raw).chars().take(240).collect();
        match status {
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => anyhow::bail!(
                "Claude authentication failed (HTTP {status}); re-authorize this provider"
            ),
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                anyhow::bail!("Claude usage endpoint is rate limited (HTTP 429); retry later")
            }
            _ => anyhow::bail!("Claude usage query failed (HTTP {status}): {preview}"),
        }
    }
    let body: Value = serde_json::from_slice(&raw)
        .map_err(|error| anyhow::anyhow!("failed to parse Claude usage response: {error}"))?;
    Ok(parse_usage(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn clamps_and_tolerates_empty_bodies() {
        let usage = parse_usage(&json!({"five_hour": {"utilization": 140, "resets_at": null}}));
        assert_eq!(usage.windows[0].used_percent, 100.0);
        assert_eq!(usage.windows[0].resets_at, None);
        assert!(parse_usage(&json!({})).windows.is_empty());
    }
}
