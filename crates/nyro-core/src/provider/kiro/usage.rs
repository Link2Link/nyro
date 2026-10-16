//! Kiro `getUsageLimits` response parsing (plan, quota windows, bonuses).
//!
//! Field names follow AIClient2API's usage service. Everything is optional on
//! the wire, so parsing never fails: unknown shapes yield an empty readout.
//!
//! Shared by connection checks and admin usage queries. Allowances belonging
//! to one resource are additive (base + active bonuses + trial), not independent
//! rate-limit windows: scheduling must observe the pooled allowance.
use chrono::{DateTime, Utc};
use serde_json::Value;

/// One quota line: the base allowance, a bonus, or the free trial.
#[derive(Debug, Clone, PartialEq)]
pub struct KiroUsageItem {
    /// Base, bonus and trial entries with this key share one credit pool.
    pub resource: String,
    pub kind: &'static str,
    pub name: String,
    pub used: f64,
    pub limit: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl KiroUsageItem {
    pub fn remaining_percent(&self) -> Option<f64> {
        (self.limit.is_finite() && self.used.is_finite() && self.limit > 0.0 && self.used >= 0.0)
            .then(|| ((self.limit - self.used) / self.limit * 100.0).clamp(0.0, 100.0))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct KiroUsage {
    /// Subscription plan, e.g. `PRO`; `KIRO ` prefix stripped.
    pub plan: Option<String>,
    pub email: Option<String>,
    pub resets_at: Option<DateTime<Utc>>,
    /// Only an explicit upstream opt-in allows paid usage beyond the pool.
    pub overage_enabled: bool,
    pub items: Vec<KiroUsageItem>,
}

/// Timestamps arrive as epoch seconds or milliseconds.
fn epoch(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())?;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let millis = if raw < 1e10 { raw * 1000.0 } else { raw };
    DateTime::from_timestamp_millis(millis as i64)
}

/// Prefer the `*WithPrecision` variant, fall back to the integer field.
fn number(node: &Value, key: &str) -> Option<f64> {
    node.get(format!("{key}WithPrecision"))
        .and_then(Value::as_f64)
        .or_else(|| node.get(key).and_then(Value::as_f64))
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn expired(node: &Value, status_key: &str) -> bool {
    node.get(status_key)
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("EXPIRED"))
}

fn name_of(node: &Value, keys: &[&str], fallback: &str) -> String {
    keys.iter()
        .find_map(|key| node.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

pub fn parse(raw: &Value) -> KiroUsage {
    parse_at(raw, Utc::now())
}

fn parse_at(raw: &Value, now: DateTime<Utc>) -> KiroUsage {
    let plan = raw
        .pointer("/subscriptionInfo/subscriptionTitle")
        .or_else(|| raw.pointer("/subscription/title"))
        .and_then(Value::as_str)
        .map(|title| {
            let title = title.trim();
            title
                .strip_prefix("KIRO ")
                .unwrap_or(title)
                .trim()
                .to_string()
        })
        .filter(|title| !title.is_empty());
    let email = raw
        .pointer("/userInfo/email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_string);
    let mut items = Vec::new();
    for (index, entry) in raw
        .get("usageBreakdownList")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let resource = name_of(entry, &["resourceType"], &format!("resource-{index}"));
        if let (Some(used), Some(limit)) =
            (number(entry, "currentUsage"), number(entry, "usageLimit"))
        {
            items.push(KiroUsageItem {
                resource: resource.clone(),
                kind: "base",
                name: name_of(entry, &["displayName", "resourceType"], "Usage"),
                used,
                limit,
                resets_at: entry.get("nextDateReset").and_then(epoch),
            });
        }
        for bonus in entry
            .get("bonuses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if expired(bonus, "status")
                || bonus
                    .get("expiresAt")
                    .and_then(epoch)
                    .is_some_and(|at| at <= now)
            {
                continue;
            }
            if let (Some(used), Some(limit)) =
                (number(bonus, "currentUsage"), number(bonus, "usageLimit"))
            {
                items.push(KiroUsageItem {
                    resource: resource.clone(),
                    kind: "bonus",
                    name: name_of(bonus, &["displayName", "bonusCode"], "bonus"),
                    used,
                    limit,
                    resets_at: bonus.get("expiresAt").and_then(epoch),
                });
            }
        }
        if let Some(trial) = entry.get("freeTrialInfo")
            && !expired(trial, "freeTrialStatus")
            && !trial
                .get("freeTrialExpiry")
                .and_then(epoch)
                .is_some_and(|at| at <= now)
            && let (Some(used), Some(limit)) =
                (number(trial, "currentUsage"), number(trial, "usageLimit"))
        {
            items.push(KiroUsageItem {
                resource: resource.clone(),
                kind: "free_trial",
                name: "Free trial".into(),
                used,
                limit,
                resets_at: trial.get("freeTrialExpiry").and_then(epoch),
            });
        }
    }
    KiroUsage {
        plan,
        email,
        resets_at: raw.get("nextDateReset").and_then(epoch),
        overage_enabled: raw
            .pointer("/overageConfiguration/overageEnabled")
            .and_then(Value::as_bool)
            == Some(true),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_plan_email_windows_bonuses_and_trial() {
        let usage = parse(&json!({
            "subscriptionInfo": {"subscriptionTitle": "KIRO PRO"},
            "userInfo": {"email": "a@b.c"},
            "nextDateReset": 1_900_000_000,
            "usageBreakdownList": [{
                "resourceType": "AGENTIC_REQUEST",
                "displayName": "Credits",
                "currentUsage": 10,
                "currentUsageWithPrecision": 10.5,
                "usageLimit": 100,
                "usageLimitWithPrecision": 100.0,
                "nextDateReset": 1_900_000_000_000_i64,
                "bonuses": [
                    {"bonusCode": "B1", "currentUsage": 1, "usageLimit": 5, "expiresAt": 1_900_000_000},
                    {"bonusCode": "OLD", "currentUsage": 5, "usageLimit": 5, "status": "EXPIRED"}
                ],
                "freeTrialInfo": {"currentUsage": 2, "usageLimit": 50, "freeTrialStatus": "ACTIVE", "freeTrialExpiry": 1_900_000_000}
            }]
        }));
        assert_eq!(usage.plan.as_deref(), Some("PRO"));
        assert_eq!(usage.email.as_deref(), Some("a@b.c"));
        assert_eq!(usage.items.len(), 3);
        assert_eq!(usage.items[0].kind, "base");
        assert_eq!(usage.items[0].used, 10.5, "precision variant wins");
        assert_eq!(
            usage.items[0].remaining_percent().map(|p| p.round() as i32),
            Some(90)
        );
        assert_eq!(usage.items[1].name, "B1");
        assert_eq!(usage.items[2].kind, "free_trial");
        // Seconds and milliseconds resolve to the same instant.
        assert_eq!(usage.items[0].resets_at, usage.resets_at);
    }

    #[test]
    fn expired_trials_and_malformed_shapes_yield_a_safe_readout() {
        let usage = parse(&json!({
            "usageBreakdownList": [
                {"currentUsage": 1, "usageLimit": 2, "freeTrialInfo": {"currentUsage": 1, "usageLimit": 2, "freeTrialStatus": "EXPIRED"}},
                {"displayName": "no numbers"},
                "garbage"
            ]
        }));
        assert_eq!(usage.items.len(), 1);
        assert_eq!(usage.plan, None);
        assert_eq!(parse(&json!(null)), KiroUsage::default());
        assert_eq!(parse(&json!([])), KiroUsage::default());
    }

    #[test]
    fn zero_limits_have_no_percentage_and_full_use_is_clamped() {
        let zero = KiroUsageItem {
            resource: "AGENTIC_REQUEST".into(),
            kind: "base",
            name: "x".into(),
            used: 0.0,
            limit: 0.0,
            resets_at: None,
        };
        assert_eq!(zero.remaining_percent(), None);
        let over = KiroUsageItem {
            resource: "AGENTIC_REQUEST".into(),
            kind: "base",
            name: "x".into(),
            used: 150.0,
            limit: 100.0,
            resets_at: None,
        };
        assert_eq!(over.remaining_percent(), Some(0.0));
    }

    #[test]
    fn usage_ignores_expired_or_negative_allowances_and_preserves_trial_only_pools() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let usage = parse_at(
            &json!({
                "overageConfiguration": {"overageEnabled": true},
                "usageBreakdownList": [{
                    "resourceType": "AGENTIC_REQUEST", "currentUsage": -1, "usageLimit": 100,
                    "bonuses": [
                        {"currentUsage":0,"usageLimit":50,"expiresAt":1_799_999_999},
                        {"currentUsage":0,"usageLimit":-50}
                    ],
                    "freeTrialInfo": {"currentUsage":1,"usageLimit":10,"freeTrialExpiry":1_800_000_001}
                }]
            }),
            now,
        );
        assert!(usage.overage_enabled);
        assert_eq!(usage.items.len(), 1);
        assert_eq!(usage.items[0].kind, "free_trial");
        assert_eq!(usage.items[0].resource, "AGENTIC_REQUEST");
        assert!(!parse(&json!({"overageConfiguration":{"overageEnabled":"true"}})).overage_enabled);
        let expired = parse_at(
            &json!({"usageBreakdownList":[{"freeTrialInfo":{
                "currentUsage":0,"usageLimit":10,"freeTrialExpiry":1_799_999_999
            }}]}),
            now,
        );
        assert!(expired.items.is_empty());
    }

    #[test]
    fn invalid_epochs_are_ignored() {
        assert_eq!(epoch(&json!(0)), None);
        assert_eq!(epoch(&json!(-5)), None);
        assert_eq!(epoch(&json!("nope")), None);
        assert!(epoch(&json!("1900000000")).is_some());
    }
}
