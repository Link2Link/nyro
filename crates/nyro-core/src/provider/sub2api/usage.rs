//! Sub2API key usage: `GET {root}/v1/usage` with the provider's own key.
//!
//! The endpoint answers in one of three shapes (see Sub2API
//! `GatewayHandler.Usage`):
//!
//! - `mode = "quota_limited"`: the key has a total USD quota and/or rolling
//!   5h / 1d / 7d USD limits (`rate_limits[].reset_at` once a window started);
//! - `mode = "unrestricted"` with a subscription group: daily / weekly /
//!   monthly USD limits of the subscription (`planName` = group name);
//! - `mode = "unrestricted"` with a wallet: `balance` in USD.
//!
//! `usage.today` / `usage.total` carry the key's actual billed cost. The
//! parser is pure; [`fetch_usage`] separates "this deployment has no usage
//! endpoint" ([`UsageUnsupported`]) from real query failures.

use std::time::Duration;

use serde_json::Value;

use super::url::sub2api_root;

pub const TIER_FIVE_HOUR: &str = "five_hour";
pub const TIER_DAILY: &str = "daily";
pub const TIER_WEEKLY: &str = "weekly_limit";
pub const TIER_MONTHLY: &str = "monthly";
/// The key's total USD quota (no reset).
pub const TIER_KEY_QUOTA: &str = "key_quota";

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// One usage window, in percent of its USD limit.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    pub name: &'static str,
    pub used_percent: f64,
    pub amount: WindowAmount,
    pub resets_at: Option<String>,
}

/// Monetary detail for the local admin card.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WindowAmount {
    pub used_usd: f64,
    pub limit_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct QuotaDetails {
    pub amounts: std::collections::BTreeMap<&'static str, WindowAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Parsed `/v1/usage` report.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sub2ApiUsage {
    /// Subscription plan (group) name.
    pub level: Option<String>,
    pub expires_at: Option<String>,
    pub windows: Vec<UsageWindow>,
    /// Wallet balance in USD.
    pub balance_usd: Option<f64>,
    /// `(today | total, actual USD cost)`.
    pub spends: Vec<(&'static str, f64)>,
    pub is_available: Option<bool>,
}

/// `/v1/usage` is not served by this address (older Sub2API, a proxy that
/// blocks it, or not a Sub2API at all). Callers fall back to local accounting.
#[derive(Debug)]
pub struct UsageUnsupported(pub String);

impl std::fmt::Display for UsageUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the Sub2API usage endpoint is unavailable: {}", self.0)
    }
}

impl std::error::Error for UsageUnsupported {}

/// Fetch the usage report. `today` (`YYYY-MM-DD`) narrows the optional
/// daily/model statistics Sub2API computes alongside, keeping polls cheap.
pub async fn fetch_usage(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    today: &str,
) -> anyhow::Result<Value> {
    let root = sub2api_root(base_url);
    anyhow::ensure!(!root.is_empty(), "the Sub2API provider has no address");
    let url = format!("{root}/v1/usage?days=1&start_date={today}&end_date={today}");
    let response = client
        .get(&url)
        .bearer_auth(api_key.trim())
        .timeout(USAGE_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("usage query request failed: {error}"))?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 405 | 501) {
        return Err(UsageUnsupported(format!("HTTP {status}")).into());
    }
    let body = response.text().await.unwrap_or_default();
    if status.as_u16() == 401 {
        anyhow::bail!(
            "authentication failed (HTTP 401): the Sub2API API key is invalid or disabled"
        );
    }
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("HTTP {status}: {preview}");
    }
    serde_json::from_str(&body)
        .map_err(|_| UsageUnsupported("the response is not JSON".into()).into())
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|value: &f64| value.is_finite())
}

fn window_amount(used: Option<&Value>, limit: Option<&Value>) -> Option<WindowAmount> {
    Some(WindowAmount {
        used_usd: number(used)?.max(0.0),
        limit_usd: number(limit).filter(|limit| *limit > 0.0)?,
    })
}

impl WindowAmount {
    fn used_percent(&self) -> f64 {
        (self.used_usd / self.limit_usd * 100.0).min(100.0)
    }
}

/// Normalize Go's RFC 3339 (nanosecond) timestamps to whole UTC seconds.
fn rfc3339(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = value?.as_str()?.trim();
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

fn iso(time: chrono::DateTime<chrono::Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn window_name(window: &str) -> Option<&'static str> {
    match window.trim().to_ascii_lowercase().as_str() {
        "5h" => Some(TIER_FIVE_HOUR),
        "1d" | "24h" => Some(TIER_DAILY),
        "7d" => Some(TIER_WEEKLY),
        "30d" => Some(TIER_MONTHLY),
        _ => None,
    }
}

/// Parse a `/v1/usage` body. A JSON body without Sub2API's `mode` field is
/// [`UsageUnsupported`] (another relay's usage endpoint).
pub fn parse_usage(body: &Value) -> anyhow::Result<Sub2ApiUsage> {
    let mode = body
        .get("mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| UsageUnsupported("the response is not a Sub2API usage report".into()))?;
    let mut usage = Sub2ApiUsage::default();

    match mode {
        "quota_limited" => {
            if let Some(quota) = body.get("quota")
                && let Some(amount) = window_amount(quota.get("used"), quota.get("limit"))
            {
                usage.windows.push(UsageWindow {
                    name: TIER_KEY_QUOTA,
                    used_percent: amount.used_percent(),
                    amount,
                    resets_at: None,
                });
            }
            for limit in body
                .get("rate_limits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = limit
                    .get("window")
                    .and_then(Value::as_str)
                    .and_then(window_name)
                else {
                    continue;
                };
                let Some(amount) = window_amount(limit.get("used"), limit.get("limit")) else {
                    continue;
                };
                usage.windows.push(UsageWindow {
                    name,
                    used_percent: amount.used_percent(),
                    amount,
                    resets_at: rfc3339(limit.get("reset_at")).map(iso),
                });
            }
            usage.is_available = match body.get("status").and_then(Value::as_str).map(str::trim) {
                Some("active") => Some(true),
                Some("quota_exhausted" | "expired" | "disabled") => Some(false),
                _ => None,
            };
        }
        "unrestricted" => {
            if let Some(balance) = number(body.get("balance")) {
                usage.balance_usd = Some(balance);
                usage.is_available = Some(balance > 0.0);
            } else {
                usage.level = body
                    .get("planName")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                if let Some(subscription) = body.get("subscription").filter(|s| s.is_object()) {
                    let weekly_reset = rfc3339(subscription.get("weekly_window_start"))
                        .map(|start| iso(start + chrono::Duration::days(7)));
                    for (name, used, limit, resets_at) in [
                        (TIER_DAILY, "daily_usage_usd", "daily_limit_usd", None),
                        (
                            TIER_WEEKLY,
                            "weekly_usage_usd",
                            "weekly_limit_usd",
                            weekly_reset,
                        ),
                        (TIER_MONTHLY, "monthly_usage_usd", "monthly_limit_usd", None),
                    ] {
                        if let Some(amount) =
                            window_amount(subscription.get(used), subscription.get(limit))
                        {
                            usage.windows.push(UsageWindow {
                                name,
                                used_percent: amount.used_percent(),
                                amount,
                                resets_at,
                            });
                        }
                    }
                    // The subscription was found and is active; its windows
                    // decide the rest.
                    usage.is_available = Some(true);
                }
            }
        }
        other => {
            return Err(UsageUnsupported(format!("unknown Sub2API usage mode {other:?}")).into());
        }
    }

    usage.expires_at = rfc3339(body.get("expires_at"))
        .or_else(|| rfc3339(body.pointer("/subscription/expires_at")))
        .map(iso);
    if body.get("isValid").and_then(Value::as_bool) == Some(false) {
        usage.is_available = Some(false);
    }
    for (name, path) in [
        ("today", "/usage/today/actual_cost"),
        ("total", "/usage/total/actual_cost"),
    ] {
        if let Some(amount) = number(body.pointer(path)) {
            usage.spends.push((name, amount));
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window(name: &'static str, used: f64, limit: f64, resets_at: Option<&str>) -> UsageWindow {
        UsageWindow {
            name,
            used_percent: used / limit * 100.0,
            amount: WindowAmount {
                used_usd: used,
                limit_usd: limit,
            },
            resets_at: resets_at.map(str::to_string),
        }
    }

    #[test]
    fn quota_limited_key_maps_total_quota_and_rolling_windows() {
        let body = json!({
            "mode": "quota_limited", "isValid": true, "status": "active",
            "quota": {"limit": 50.0, "used": 12.5, "remaining": 37.5, "unit": "USD"},
            "rate_limits": [
                {"window": "5h", "limit": 10.0, "used": 10.0, "remaining": 0.0,
                 "window_start": "2026-11-27T01:00:00.123456789+08:00",
                 "reset_at": "2026-11-27T06:00:00.123456789+08:00"},
                {"window": "1d", "limit": 20.0, "used": 5.0, "remaining": 15.0, "window_start": null},
                {"window": "7d", "limit": 0.0, "used": 1.0},
                {"window": "bogus", "limit": 1.0, "used": 1.0}
            ],
            "expires_at": "2026-12-31T00:00:00Z", "days_until_expiry": 34,
            "usage": {"today": {"requests": 3, "actual_cost": 0.42}, "total": {"actual_cost": 12.5}}
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(
            usage.windows,
            [
                window(TIER_KEY_QUOTA, 12.5, 50.0, None),
                window(TIER_FIVE_HOUR, 10.0, 10.0, Some("2026-11-26T22:00:00Z")),
                window(TIER_DAILY, 5.0, 20.0, None),
            ]
        );
        assert_eq!(usage.is_available, Some(true));
        assert_eq!(usage.spends, [("today", 0.42), ("total", 12.5)]);
        assert_eq!(usage.balance_usd, None);
        assert_eq!(usage.level, None);
    }

    #[test]
    fn exhausted_or_expired_keys_are_unavailable() {
        for (status, valid, expected) in [
            ("quota_exhausted", true, Some(false)),
            ("expired", true, Some(false)),
            ("active", false, Some(false)),
            ("something_new", true, None),
        ] {
            let body = json!({"mode": "quota_limited", "isValid": valid, "status": status,
                "quota": {"limit": 10.0, "used": 10.0}});
            let usage = parse_usage(&body).unwrap();
            assert_eq!(usage.is_available, expected, "{status}");
            assert_eq!(usage.windows, [window(TIER_KEY_QUOTA, 10.0, 10.0, None)]);
        }
    }

    #[test]
    fn subscription_maps_configured_limits_and_the_weekly_reset() {
        let body = json!({
            "mode": "unrestricted", "isValid": true, "planName": "Claude Max", "unit": "USD",
            "remaining": 3.0,
            "subscription": {
                "daily_usage_usd": 7.0, "weekly_usage_usd": 30.0, "monthly_usage_usd": 30.0,
                "daily_limit_usd": 10.0, "weekly_limit_usd": 120.0, "monthly_limit_usd": null,
                "weekly_window_start": "2026-11-24T00:00:00+08:00",
                "expires_at": "2026-12-24T00:00:00+08:00"
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.level.as_deref(), Some("Claude Max"));
        assert_eq!(
            usage.windows,
            [
                window(TIER_DAILY, 7.0, 10.0, None),
                window(TIER_WEEKLY, 30.0, 120.0, Some("2026-11-30T16:00:00Z")),
            ]
        );
        assert_eq!(usage.is_available, Some(true));
        // A subscription group without readable subscription data is unknown.
        let missing =
            parse_usage(&json!({"mode": "unrestricted", "isValid": true, "planName": "Pro"}))
                .unwrap();
        assert!(missing.windows.is_empty());
        assert_eq!(missing.is_available, None);
    }

    #[test]
    fn wallet_mode_reports_the_balance() {
        for (balance, available) in [
            (json!(12.34), Some(true)),
            (json!(0), Some(false)),
            (json!("-1.5"), Some(false)),
        ] {
            let body = json!({"mode": "unrestricted", "isValid": true, "planName": "钱包余额",
                "remaining": balance, "balance": balance, "unit": "USD"});
            let usage = parse_usage(&body).unwrap();
            assert!(usage.balance_usd.is_some());
            assert_eq!(usage.is_available, available);
            // The wallet label is a placeholder, not a plan.
            assert_eq!(usage.level, None);
        }
    }

    #[test]
    fn non_sub2api_bodies_are_unsupported() {
        for body in [
            json!({"object": "usage"}),
            json!({"mode": "metered"}),
            json!([]),
        ] {
            let error = parse_usage(&body).unwrap_err();
            assert!(error.downcast_ref::<UsageUnsupported>().is_some(), "{body}");
        }
    }
}
