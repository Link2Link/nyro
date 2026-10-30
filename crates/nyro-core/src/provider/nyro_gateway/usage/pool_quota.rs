//! Anonymous, display-only account-pool quota wire contract.
//!
//! Keep this DTO independent of the admin account-pool schema: adding account
//! details or money to that schema must never expand proxy-visible data. Unknown
//! fields are discarded by typed deserialization and reserialization, including
//! inside windows. No field here is an input to routing or quota observations.

use chrono::DateTime;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The safe subset of `AccountPoolQuota`, shared by both forwarding ends.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnonymousPoolQuota {
    pub daily: Option<AnonymousPoolQuotaWindow>,
    pub weekly: Option<AnonymousPoolQuotaWindow>,
    #[serde(default, deserialize_with = "deserialize_claude_windows")]
    pub claude_windows: Vec<AnonymousClaudePoolQuotaWindow>,
    pub estimated: bool,
}

/// Numbers, coverage and reset metadata only; never per-account identities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnonymousPoolQuotaWindow {
    pub total_accounts: usize,
    pub capacity_complete: bool,
    pub total_units: usize,
    pub known_accounts: usize,
    pub known_remaining_units: f64,
    pub used_units: Option<f64>,
    pub remaining_units: Option<f64>,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub stale: bool,
    #[serde(default, deserialize_with = "deserialize_reset_time")]
    pub next_reset_at: Option<String>,
    pub next_reset_accounts: usize,
    pub reset_mode: PoolQuotaResetMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnonymousClaudePoolQuotaWindow {
    pub name: ClaudePoolWindowName,
    pub quota: AnonymousPoolQuotaWindow,
}

/// Never pass arbitrary window names through: they could contain identities.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudePoolWindowName {
    FiveHour,
    WeeklyLimit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolQuotaResetMode {
    Unknown,
    Aligned,
    Staggered,
}

fn deserialize_claude_windows<'de, D>(
    deserializer: D,
) -> Result<Vec<AnonymousClaudePoolQuotaWindow>, D::Error>
where
    D: Deserializer<'de>,
{
    // A newer upstream may add windows. Ignore unknown names/invalid windows
    // independently so the known windows remain usable, without string leakage.
    Ok(Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .filter_map(|window| serde_json::from_value(window).ok())
        .collect())
}

fn deserialize_reset_time<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .map(|_| raw)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

impl From<&crate::admin::AccountPoolQuota> for AnonymousPoolQuota {
    fn from(quota: &crate::admin::AccountPoolQuota) -> Self {
        Self {
            daily: quota.daily.as_ref().map(Into::into),
            weekly: quota.weekly.as_ref().map(Into::into),
            claude_windows: quota
                .claude_windows
                .iter()
                .filter_map(|window| {
                    let name = match window.name.as_str() {
                        "five_hour" => ClaudePoolWindowName::FiveHour,
                        "weekly_limit" => ClaudePoolWindowName::WeeklyLimit,
                        _ => return None,
                    };
                    Some(AnonymousClaudePoolQuotaWindow {
                        name,
                        quota: (&window.quota).into(),
                    })
                })
                .collect(),
            estimated: quota.estimated,
        }
    }
}

impl From<&crate::admin::PoolQuotaWindow> for AnonymousPoolQuotaWindow {
    fn from(window: &crate::admin::PoolQuotaWindow) -> Self {
        Self {
            total_accounts: window.total_accounts,
            capacity_complete: window.capacity_complete,
            total_units: window.total_units,
            known_accounts: window.known_accounts,
            known_remaining_units: window.known_remaining_units,
            used_units: window.used_units,
            remaining_units: window.remaining_units,
            used_percent: window.used_percent,
            remaining_percent: window.remaining_percent,
            stale: window.stale,
            next_reset_at: window
                .next_reset_at
                .as_ref()
                .filter(|raw| DateTime::parse_from_rfc3339(raw).is_ok())
                .cloned(),
            next_reset_accounts: window.next_reset_accounts,
            reset_mode: match window.reset_mode.as_str() {
                "aligned" => PoolQuotaResetMode::Aligned,
                "staggered" => PoolQuotaResetMode::Staggered,
                _ => PoolQuotaResetMode::Unknown,
            },
        }
    }
}
