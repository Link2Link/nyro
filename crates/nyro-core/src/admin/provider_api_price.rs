//! Admin-only API-equivalent prices for retained coding-plan provider attempts.
//! These are estimates at current catalogue/manual rates, not upstream invoices.
//! Unlike the permanent token-usage ledger, clearing logs reduces these totals.

use super::*;

/// Retained provider usage at current API-equivalent rates. Current-cycle bounds
/// belong to the captured upstream observation (`ProviderUsage.queried_at`), not
/// a calendar week, browser clock, or the five-hour bar when a weekly bar exists.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderApiPriceStats {
    pub all_time: AccountUsageTotals,
    /// Unknown/invalid/expired cycles are unavailable, not a zero-usage cycle.
    pub period: Option<AccountUsageTotals>,
    /// Upstream-anchored [start, end) in epoch milliseconds.
    pub period_start: Option<i64>,
    pub period_end: Option<i64>,
    /// `weekly_limit` | `five_hour` | `monthly`, only for an identified cycle.
    pub period_tier: Option<String>,
    /// Finite observed utilization (0..=100) of that same identified cycle.
    pub used_percent: Option<f64>,
    /// Period prices scaled by 100 / used_percent, per native currency. Partial
    /// prices stay partial; priced_request_count is the observed priced sample,
    /// not a projected request count. No estimate without usage and a price.
    pub estimated_capacity: Option<Vec<EquivalentCostTotal>>,
}

#[derive(Debug, PartialEq)]
struct PricePeriod<'a> {
    start: i64,
    end: i64,
    tier: &'a ProviderUsageTier,
}

fn supported(kind: &str) -> bool {
    matches!(
        kind,
        "glm_coding_plan" | "minimax_coding_plan" | "grok_plan"
    )
}

fn reset_time(kind: &str, reset: &str) -> Option<DateTime<chrono::FixedOffset>> {
    if let Ok(reset) = DateTime::parse_from_rfc3339(reset) {
        // Preserve the upstream offset for calendar-month subtraction. Moving
        // to UTC first can change the calendar date/month at offset boundaries.
        return Some(reset);
    }
    // Grok currentPeriod.end also uses date-only values. Define these as UTC
    // midnight explicitly; never depend on local/server/browser time zones.
    if kind == "grok_plan" {
        let date = chrono::NaiveDate::parse_from_str(reset, "%Y-%m-%d").ok()?;
        if date.format("%Y-%m-%d").to_string() == reset {
            return Some(date.and_hms_opt(0, 0, 0)?.and_utc().fixed_offset());
        }
    }
    None
}

fn provider_period<'a>(
    kind: &str,
    tiers: &'a [ProviderUsageTier],
    now: i64,
) -> Option<PricePeriod<'a>> {
    if !supported(kind) {
        return None;
    }
    // Presence wins, not validity: an invalid, duplicated or expired weekly
    // observation must never silently fall back to another quota window.
    let name = if tiers.iter().any(|tier| tier.name == "weekly_limit") {
        "weekly_limit"
    } else if kind == "grok_plan" {
        "monthly"
    } else {
        "five_hour"
    };
    let mut matching = tiers.iter().filter(|tier| tier.name == name);
    let tier = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    let reset = reset_time(kind, tier.resets_at.as_deref()?)?;
    let start = match name {
        "weekly_limit" => reset.checked_sub_signed(chrono::Duration::days(7))?,
        "five_hour" => reset.checked_sub_signed(chrono::Duration::hours(5))?,
        // Calendar-month anchor in the upstream's explicit offset, including
        // month-end/leap-year clamping. Date-only Grok resets use UTC.
        "monthly" => reset.checked_sub_months(chrono::Months::new(1))?,
        _ => return None,
    }
    .timestamp_millis();
    let end = reset.timestamp_millis();
    (0 <= start && start <= now && now < end).then_some(PricePeriod { start, end, tier })
}

fn estimate_capacity(
    period: &AccountUsageTotals,
    used_percent: f64,
) -> Option<Vec<EquivalentCostTotal>> {
    if !used_percent.is_finite()
        || used_percent <= 0.0
        || used_percent > 100.0
        || period.request_count <= 0
        || !period
            .equivalent_costs
            .iter()
            .any(|cost| cost.priced_request_count > 0)
    {
        return None;
    }
    period
        .equivalent_costs
        .iter()
        .map(|cost| {
            if !cost.total_cost.is_finite() || cost.total_cost < 0.0 {
                return None;
            }
            // Divide first so finite percentages near zero do not themselves
            // overflow a scale factor. A priced zero remains a valid zero.
            let total_cost = cost.total_cost / used_percent * 100.0;
            (total_cost.is_finite() && total_cost >= 0.0).then(|| EquivalentCostTotal {
                currency: cost.currency.clone(),
                total_cost,
                priced_request_count: cost.priced_request_count,
            })
        })
        .collect()
}

impl AdminService {
    /// Best-effort enrichment for the full admin path only. Do not propagate
    /// storage/settings/pricing errors to quota observation or scheduling.
    pub(super) async fn enrich_provider_api_price(&self, usage: &mut ProviderUsage) {
        if !supported(&usage.kind) {
            return;
        }
        usage.api_price = None;
        match self.provider_api_price_stats(usage).await {
            Ok(stats) => usage.api_price = Some(stats),
            Err(error) => tracing::warn!(
                provider_id = %usage.provider_id,
                kind = %usage.kind,
                %error,
                "could not compute provider retained-log API price statistics"
            ),
        }
    }

    async fn provider_api_price_stats(
        &self,
        usage: &ProviderUsage,
    ) -> anyhow::Result<ProviderApiPriceStats> {
        // Capture once at the upstream observation, not after asynchronous log
        // or settings queries; future attempts must not inflate this snapshot.
        let now = DateTime::parse_from_rfc3339(&usage.queried_at)
            .context("invalid provider usage observation time")?
            .timestamp_millis();
        let period = provider_period(&usage.kind, &usage.tiers, now);
        let provider = self.get_provider(&usage.provider_id).await?;
        let (mut rows, current) = self
            .gw
            .storage
            .logs()
            .provider_token_usage_with_period(
                &usage.provider_id,
                now,
                period.as_ref().map(|period| (period.start, period.end)),
            )
            .await?;
        let all_time_len = rows.len();
        rows.extend(current);
        let providers = HashMap::from([(usage.provider_id.clone(), provider)]);
        let overrides = token_price::parse_overrides(self.gw.storage.settings().list_all().await?)?;
        token_cost::enrich_token_costs(&mut rows, &providers, &self.gw.config.data_dir, &overrides);
        let all_time = account_usage::summarize(&rows[..all_time_len]);
        let current = period
            .as_ref()
            .map(|_| account_usage::summarize(&rows[all_time_len..]));
        // Individual prices are finite, but a sum across models may overflow.
        // Withhold the optional enrichment instead of serializing false zeros.
        anyhow::ensure!(
            all_time
                .equivalent_costs
                .iter()
                .chain(current.iter().flat_map(|totals| &totals.equivalent_costs))
                .all(|cost| cost.total_cost.is_finite() && cost.total_cost >= 0.0),
            "provider API price totals overflowed"
        );
        let used_percent = period.as_ref().and_then(|period| {
            let used = period.tier.used_percent;
            (used.is_finite() && (0.0..=100.0).contains(&used)).then_some(used)
        });
        let estimated_capacity = current
            .as_ref()
            .zip(used_percent)
            .and_then(|(totals, used)| estimate_capacity(totals, used));
        Ok(ProviderApiPriceStats {
            all_time,
            period: current,
            period_start: period.as_ref().map(|period| period.start),
            period_end: period.as_ref().map(|period| period.end),
            period_tier: period.as_ref().map(|period| period.tier.name.clone()),
            used_percent,
            estimated_capacity,
        })
    }
}

#[cfg(test)]
mod tests;
