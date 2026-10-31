//! Retained gateway usage for account-pool rows, separate from upstream quotas.
use super::*;
use crate::provider::account_pool::{ClaudeAccountState, DevinAccountState, KiroAccountState};

/// Use the account's main recurring quota, never a pool-wide earliest reset,
/// model-specific limit, or bonus/trial expiry. An older snapshot can still
/// identify the current cycle, but an expired reset cannot identify the next one.
fn account_period(driver: &str, raw: &str, now: i64) -> Option<(i64, i64)> {
    let (reset, monthly) = match driver {
        "claude-code" | "claude" => {
            let usage = ClaudeAccountState::parse(raw).usage?;
            let mut windows = usage.windows.iter().filter(|w| w.name == "weekly_limit");
            let window = windows.next()?;
            if windows.next().is_some() {
                return None;
            }
            let reset = DateTime::parse_from_rfc3339(window.resets_at.as_deref()?)
                .ok()?
                .with_timezone(&Utc);
            (reset, false)
        }
        "devin" => {
            let reset = DevinAccountState::parse(raw)
                .usage?
                .weekly?
                .resets_at_unix?;
            (DateTime::from_timestamp(reset, 0)?, false)
        }
        "kiro" => {
            let usage = KiroAccountState::parse(raw).usage?;
            // Match resource_pools: base reset first, global reset as fallback.
            // All base resources must agree before assigning one account-wide
            // cycle. Bonus/trial expiration is not a monthly reset date.
            let mut base = usage.items.iter().filter(|item| item.kind == "base");
            let reset = match base.next() {
                Some(first) => {
                    let reset = first.resets_at.or(usage.resets_at)?;
                    if !base.all(|item| item.resets_at.or(usage.resets_at) == Some(reset)) {
                        return None;
                    }
                    reset
                }
                None => usage.resets_at?,
            };
            (reset, true)
        }
        _ => return None,
    };
    let end = reset.timestamp_millis();
    let start = if monthly {
        // Calendar month anchored to the upstream reset, including month-end
        // clamping (March 31 -> February 28/29), not a fixed 30-day duration.
        reset.checked_sub_months(chrono::Months::new(1))?
    } else {
        reset.checked_sub_signed(chrono::Duration::days(7))?
    }
    .timestamp_millis();
    (0 <= start && start <= now && now < end).then_some((start, end))
}

/// Known token totals and current API-equivalent estimates, not an upstream bill.
#[derive(Debug, Clone, Serialize)]
pub struct AccountUsageTotals {
    /// Gross input plus output; cache reads are already included in input.
    pub total_tokens: i64,
    /// Upstream attempts, including retries/errors (not unique client requests).
    pub request_count: i64,
    pub equivalent_costs: Vec<EquivalentCostTotal>,
    pub unpriced_request_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountUsageStats {
    pub account_id: String,
    pub all_time: AccountUsageTotals,
    /// Unknown/expired reset dates are unavailable, not a zero-usage cycle.
    pub period: Option<AccountUsageTotals>,
    /// Upstream-anchored [start, end) in epoch milliseconds, specific to this account.
    pub period_start: Option<i64>,
    pub period_end: Option<i64>,
}

fn summarize(rows: &[TokenUsageDay]) -> AccountUsageTotals {
    let (equivalent_costs, unpriced_request_count) = super::token_cost::summarize_costs(rows);
    AccountUsageTotals {
        total_tokens: rows.iter().fold(0_i64, |total, row| {
            total
                .saturating_add(row.input_tokens)
                .saturating_add(row.output_tokens)
        }),
        request_count: rows
            .iter()
            .fold(0_i64, |total, row| total.saturating_add(row.request_count)),
        equivalent_costs,
        unpriced_request_count,
    }
}

impl AdminService {
    /// All retained logs and each account's current upstream reset cycle, both
    /// capped at the same captured `now`. Boundaries come from saved observations,
    /// independent of browser/server time zones. No upstream calls, credential
    /// mutations, or guesses based on account labels are made.
    pub async fn get_provider_account_usage_stats(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<AccountUsageStats>> {
        let now = Utc::now().timestamp_millis();
        let provider = self.account_pool_provider(provider_id).await?;
        let accounts = self
            .gw
            .storage
            .oauth_credentials()
            .list_accounts(provider_id)
            .await?;
        if accounts.is_empty() {
            return Ok(Vec::new());
        }
        let account_ids: std::collections::HashSet<_> = accounts
            .iter()
            .map(|account| account.account_id.as_str())
            .collect();
        let periods: HashMap<_, _> = accounts
            .iter()
            .filter_map(|account| {
                account_period(&account.driver_key, &account.account_state, now)
                    .map(|period| (account.account_id.clone(), period))
            })
            .collect();
        // One scan, with per-account filtering before model aggregation/pricing.
        let (all_time, period) = self
            .gw
            .storage
            .logs()
            .account_token_usage_with_periods(provider_id, None, Some(now), &periods)
            .await?;
        let mut owners = Vec::new();
        let mut rows = Vec::new();
        for (is_period, usage) in [(false, all_time), (true, period)] {
            for row in usage {
                if !account_ids.contains(row.account_id.as_str()) {
                    continue;
                }
                owners.push((row.account_id, is_period));
                // Pricing only depends on provider/model/token counts; the
                // shared engine does not require a day for these totals.
                rows.push(TokenUsageDay {
                    date: String::new(),
                    provider_id: provider_id.to_string(),
                    provider_name: provider.name.clone(),
                    model: row.model,
                    request_count: row.request_count,
                    input_tokens: row.input_tokens,
                    output_tokens: row.output_tokens,
                    cache_read_tokens: row.cache_read_tokens,
                    missing_usage_count: row.missing_usage_count,
                    cache_read_reported_count: row.cache_read_reported_count,
                    cost: None,
                });
            }
        }
        let providers = HashMap::from([(provider_id.to_string(), provider)]);
        let overrides =
            super::token_price::parse_overrides(self.gw.storage.settings().list_all().await?)?;
        super::token_cost::enrich_token_costs(
            &mut rows,
            &providers,
            &self.gw.config.data_dir,
            &overrides,
        );
        let mut by_account: HashMap<(String, bool), Vec<TokenUsageDay>> = HashMap::new();
        for (owner, row) in owners.into_iter().zip(rows) {
            by_account.entry(owner).or_default().push(row);
        }
        Ok(accounts
            .into_iter()
            .map(|account| AccountUsageStats {
                all_time: summarize(
                    &by_account
                        .remove(&(account.account_id.clone(), false))
                        .unwrap_or_default(),
                ),
                period: periods.get(&account.account_id).map(|_| {
                    summarize(
                        &by_account
                            .remove(&(account.account_id.clone(), true))
                            .unwrap_or_default(),
                    )
                }),
                period_start: periods.get(&account.account_id).map(|&(start, _)| start),
                period_end: periods.get(&account.account_id).map(|&(_, end)| end),
                account_id: account.account_id,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ms(value: &str) -> i64 {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn account_usage_cycle_uses_weekly_reset_not_calendar_or_short_window() {
        let reset = "2026-11-06T17:59:59+08:00";
        let now = ms("2026-10-31T13:48:02+08:00");
        let expected = Some((ms("2026-10-30T17:59:59+08:00"), ms(reset)));
        let raw = json!({"usage": {"windows": [
            {"name": "five_hour", "used_percent": 0, "resets_at": "2026-10-31T16:09:59+08:00"},
            {"name": "feature:sonnet:weekly_limit", "used_percent": 0, "resets_at": "2026-11-01T00:00:00Z"},
            {"name": "weekly_limit", "used_percent": 4, "resets_at": reset}
        ]}, "usage_stale": true, "observed_at": 1}).to_string();
        for driver in ["claude-code", "claude"] {
            assert_eq!(account_period(driver, &raw, now), expected);
        }
        let devin = json!({"usage": {"weekly": {
            "remaining_percent": 96, "resets_at_unix": ms(reset) / 1000
        }}})
        .to_string();
        assert_eq!(account_period("devin", &devin, now), expected);
        let (start, end) = expected.unwrap();
        assert_eq!(account_period("claude", &raw, start), expected);
        assert_eq!(account_period("claude", &raw, start - 1), None);
        assert_eq!(account_period("claude", &raw, end), None);
        assert_eq!(account_period("claude", &raw, end + 7 * 86_400_000), None);
    }

    #[test]
    fn account_usage_cycle_months_follow_reset_day_and_clamp_month_ends() {
        for (reset, start, now) in [
            (
                "2026-11-06T09:59:59Z",
                "2026-10-06T09:59:59Z",
                "2026-10-31T05:48:02Z",
            ),
            (
                "2026-03-31T09:59:59Z",
                "2026-02-28T09:59:59Z",
                "2026-03-10T00:00:00Z",
            ),
            (
                "2024-03-31T09:59:59Z",
                "2024-02-29T09:59:59Z",
                "2024-03-10T00:00:00Z",
            ),
            (
                "2027-01-06T09:59:59Z",
                "2026-12-06T09:59:59Z",
                "2027-01-01T00:00:00Z",
            ),
        ] {
            let raw = json!({"usage": {"resets_at": reset, "overage_enabled": false, "items": []}})
                .to_string();
            assert_eq!(
                account_period("kiro", &raw, ms(now)),
                Some((ms(start), ms(reset)))
            );
        }
    }

    #[test]
    fn account_usage_cycle_kiro_base_resets_match_resources_with_global_fallback() {
        let now = ms("2026-10-31T05:48:02Z");
        let base_reset = "2026-11-06T00:00:00Z";
        let global_reset = "2026-11-01T00:00:00Z";
        let base = |reset| json!({"resource": "credits", "kind": "base", "name": "Credits", "used": 0, "limit": 100, "resets_at": reset});
        let state = |items| {
            json!({"usage": {"resets_at": global_reset, "overage_enabled": false, "items": items}})
                .to_string()
        };
        let raw = state(vec![base(Some(base_reset)), base(Some(base_reset))]);
        assert_eq!(
            account_period("kiro", &raw, now),
            Some((ms("2026-10-06T00:00:00Z"), ms(base_reset)))
        );
        assert_eq!(account_period("kiro", &raw, ms(base_reset)), None);
        assert_eq!(
            account_period("kiro", &state(vec![base(None)]), now),
            Some((ms("2026-10-01T00:00:00Z"), ms(global_reset)))
        );
        assert_eq!(
            account_period(
                "kiro",
                &state(vec![base(Some(base_reset)), base(None)]),
                now
            ),
            None
        );
        for reset in [0, -1, i64::MAX] {
            let raw =
                json!({"usage": {"weekly": {"remaining_percent": 50, "resets_at_unix": reset}}})
                    .to_string();
            assert_eq!(account_period("devin", &raw, now), None);
        }
    }

    #[test]
    fn account_usage_cycle_missing_invalid_or_ambiguous_resets_stay_unknown() {
        let now = ms("2026-10-31T05:48:02Z");
        for driver in ["devin", "claude", "kiro", "unknown"] {
            for raw in ["{}", "not-json"] {
                assert_eq!(account_period(driver, raw, now), None);
            }
        }
        for reset in [
            None,
            Some("invalid"),
            Some("2026-10-01T00:00:00Z"),
            Some("2027-11-01T00:00:00Z"),
        ] {
            let raw = json!({"usage": {"windows": [{"name": "weekly_limit", "used_percent": 0, "resets_at": reset}]}}).to_string();
            assert_eq!(account_period("claude", &raw, now), None);
        }
        for name in ["five_hour", "feature:sonnet:weekly_limit"] {
            let raw = json!({"usage": {"windows": [{"name": name, "used_percent": 0, "resets_at": "2026-11-06T00:00:00Z"}]}}).to_string();
            assert_eq!(account_period("claude", &raw, now), None);
        }
        let window =
            json!({"name": "weekly_limit", "used_percent": 0, "resets_at": "2026-11-06T00:00:00Z"});
        assert_eq!(
            account_period(
                "claude",
                &json!({"usage": {"windows": [window.clone(), window]}}).to_string(),
                now
            ),
            None
        );
        let item = |kind, reset| json!({"resource": "credits", "name": "Credits", "kind": kind, "used": 0, "limit": 100, "resets_at": reset});
        for kind in ["bonus", "free_trial"] {
            let raw = json!({"usage": {"overage_enabled": false, "items": [item(kind, "2026-11-06T00:00:00Z")]}}).to_string();
            assert_eq!(account_period("kiro", &raw, now), None);
        }
        let base = item("base", "2026-11-06T00:00:00Z");
        let raw = json!({"usage": {"overage_enabled": false, "items": [base.clone(), item("bonus", "2026-11-01T00:00:00Z")]}}).to_string();
        assert_eq!(
            account_period("kiro", &raw, now),
            Some((ms("2026-10-06T00:00:00Z"), ms("2026-11-06T00:00:00Z")))
        );
        let raw = json!({"usage": {"overage_enabled": false, "items": [base, item("base", "2026-11-05T00:00:00Z")]}}).to_string();
        assert_eq!(account_period("kiro", &raw, now), None);
    }
}
