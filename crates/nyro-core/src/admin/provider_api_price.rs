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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_time_coverage: Option<ProviderApiPriceCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_coverage: Option<ProviderApiPriceCoverage>,
    /// True when a projected priced sample excludes unknown usage/prices/history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_capacity_partial: Option<bool>,
    /// Structured-provider view only: never copied onto published bucket rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unassigned: Option<AccountUsageTotals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unassigned_coverage: Option<ProviderApiPriceCoverage>,
    /// Immutable retired/unknown scoped buckets, separate from active allowances.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_buckets: Vec<RetainedQuotaBucketPriceStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<QuotaModelPriceStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderApiPriceCoverage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub missing_usage_request_count: i64,
    pub cache_read_reported_request_count: i64,
    pub priced_request_count: i64,
    pub unpriced_request_count: i64,
    pub attributed_request_count: i64,
    pub inferred_request_count: i64,
    pub unassigned_request_count: i64,
    pub legacy_timestamp_request_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaModelPriceStats {
    pub model: String,
    pub all_time: AccountUsageTotals,
    pub coverage: ProviderApiPriceCoverage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetainedQuotaBucketPriceStats {
    pub scope_id: String,
    pub bucket_id: String,
    pub all_time: AccountUsageTotals,
    pub coverage: ProviderApiPriceCoverage,
    pub models: Vec<QuotaModelPriceStats>,
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
        "glm_coding_plan"
            | "minimax_coding_plan"
            | "grok_plan"
            | "openai_codex"
            | "google_subscription"
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

use crate::storage::provider_usage::ProviderQuotaModelUsage;

type QuotaPriceKey = (String, String);

fn coverage(rows: &[ProviderQuotaModelUsage]) -> ProviderApiPriceCoverage {
    let mut result = ProviderApiPriceCoverage::default();
    for row in rows {
        let usage = &row.usage;
        result.input_tokens = result.input_tokens.saturating_add(usage.input_tokens);
        result.output_tokens = result.output_tokens.saturating_add(usage.output_tokens);
        result.cache_read_tokens = result
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        result.missing_usage_request_count = result
            .missing_usage_request_count
            .saturating_add(usage.missing_usage_count);
        result.cache_read_reported_request_count = result
            .cache_read_reported_request_count
            .saturating_add(usage.cache_read_reported_count);
        if usage.cost.is_some() {
            result.priced_request_count = result
                .priced_request_count
                .saturating_add(usage.request_count);
        } else {
            result.unpriced_request_count = result
                .unpriced_request_count
                .saturating_add(usage.request_count);
        }
        result.attributed_request_count = result
            .attributed_request_count
            .saturating_add(row.attributed_request_count);
        result.inferred_request_count = result
            .inferred_request_count
            .saturating_add(row.inferred_request_count);
        result.unassigned_request_count = result
            .unassigned_request_count
            .saturating_add(row.unassigned_request_count);
        result.legacy_timestamp_request_count = result
            .legacy_timestamp_request_count
            .saturating_add(row.legacy_timestamp_request_count);
    }
    result
}

fn totals_for(rows: &[ProviderQuotaModelUsage]) -> AccountUsageTotals {
    account_usage::summarize(&rows.iter().map(|row| row.usage.clone()).collect::<Vec<_>>())
}

fn model_prices(rows: &[ProviderQuotaModelUsage]) -> Vec<QuotaModelPriceStats> {
    let mut groups: std::collections::BTreeMap<String, Vec<ProviderQuotaModelUsage>> =
        Default::default();
    for row in rows {
        groups
            .entry(row.usage.model.clone())
            .or_default()
            .push(row.clone());
    }
    groups
        .into_iter()
        .map(|(model, rows)| QuotaModelPriceStats {
            model,
            all_time: totals_for(&rows),
            coverage: coverage(&rows),
        })
        .collect()
}

fn partition_key(row: &ProviderQuotaModelUsage) -> Option<QuotaPriceKey> {
    Some((row.scope_id.clone()?, row.bucket_id.clone()?))
}

fn price_stats(
    all: &[ProviderQuotaModelUsage],
    current: &[ProviderQuotaModelUsage],
    period: Option<&PricePeriod<'_>>,
    allow_capacity: bool,
    attribution_partial: bool,
) -> anyhow::Result<ProviderApiPriceStats> {
    let all_time = totals_for(all);
    let current_totals = period.map(|_| totals_for(current));
    anyhow::ensure!(
        all_time
            .equivalent_costs
            .iter()
            .chain(
                current_totals
                    .iter()
                    .flat_map(|totals| &totals.equivalent_costs)
            )
            .all(|cost| cost.total_cost.is_finite() && cost.total_cost >= 0.0),
        "provider API price totals overflowed"
    );
    let used_percent = period.and_then(|period| {
        let used = period.tier.used_percent;
        (used.is_finite() && (0.0..=100.0).contains(&used)).then_some(used)
    });
    let period_coverage = period.map(|_| coverage(current));
    let estimated_capacity = allow_capacity
        .then(|| {
            current_totals
                .as_ref()
                .zip(used_percent)
                .and_then(|(totals, used)| estimate_capacity(totals, used))
        })
        .flatten();
    let estimated_capacity_partial = estimated_capacity.as_ref().map(|_| {
        attribution_partial
            || period_coverage.as_ref().is_some_and(|coverage| {
                coverage.missing_usage_request_count > 0
                    || coverage.unpriced_request_count > 0
                    || coverage.inferred_request_count > 0
                    || coverage.unassigned_request_count > 0
            })
    });
    Ok(ProviderApiPriceStats {
        all_time,
        period: current_totals,
        period_start: period.map(|period| period.start),
        period_end: period.map(|period| period.end),
        period_tier: period.map(|period| period.tier.name.clone()),
        used_percent,
        estimated_capacity,
        all_time_coverage: Some(coverage(all)),
        period_coverage,
        estimated_capacity_partial,
        unassigned: None,
        unassigned_coverage: None,
        retained_buckets: Vec::new(),
        models: model_prices(all),
    })
}

/// Only adapters that identify fixed canonical windows can establish a period.
/// Catalog percentages/reset timestamps alone never establish a denominator.
fn bucket_period<'a>(
    kind: &str,
    bucket: &'a crate::quota_buckets::QuotaBucket,
    now: i64,
) -> Option<PricePeriod<'a>> {
    if !bucket.is_primary()
        || bucket.stale
        || !matches!(
            bucket.source.as_str(),
            "enforced_summary" | "quota_summary" | "official" | "validated_adapter" | "configured"
        )
    {
        return None;
    }
    provider_period(kind, &bucket.tiers, now)
}

impl AdminService {
    /// Best-effort enrichment for the full admin path only. Do not propagate
    /// storage/settings/pricing errors to quota observation or scheduling.
    pub(super) async fn enrich_provider_api_price(&self, usage: &mut ProviderUsage) {
        if !supported(&usage.kind) {
            return;
        }
        usage.api_price = None;
        for bucket in &mut usage.quota_buckets {
            bucket.api_price = None;
        }
        match self.provider_api_price_stats(usage).await {
            Ok((stats, mut buckets)) => {
                usage.api_price = Some(stats);
                for bucket in &mut usage.quota_buckets {
                    bucket.api_price =
                        buckets.remove(&(bucket.scope_id.clone(), bucket.id.clone()));
                }
            }
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
    ) -> anyhow::Result<(
        ProviderApiPriceStats,
        HashMap<QuotaPriceKey, ProviderApiPriceStats>,
    )> {
        // One captured cutoff, and one provider-filtered retained-log scan. No
        // lookup through today's model binding is ever applied to old attempts.
        let now = DateTime::parse_from_rfc3339(&usage.queried_at)
            .context("invalid provider usage observation time")?
            .timestamp_millis();
        let period = usage
            .quota_buckets
            .is_empty()
            .then(|| provider_period(&usage.kind, &usage.tiers, now))
            .flatten();
        let mut periods = HashMap::new();
        let mut identified = HashMap::new();
        for bucket in usage
            .quota_buckets
            .iter()
            .filter(|bucket| bucket.is_primary())
        {
            let key = (bucket.scope_id.clone(), bucket.id.clone());
            // A duplicated scoped bucket is ambiguous rather than last-wins.
            anyhow::ensure!(
                !identified.contains_key(&key),
                "duplicate scoped quota bucket"
            );
            let observed_at = DateTime::parse_from_rfc3339(&bucket.queried_at)
                .ok()
                .map(|time| time.timestamp_millis());
            let selected = observed_at
                .filter(|cutoff| *cutoff <= now && *cutoff >= 0)
                .and_then(|cutoff| {
                    bucket_period(&usage.kind, bucket, now)
                        .filter(|period| period.start <= cutoff && cutoff < period.end)
                });
            if let Some(period) = &selected {
                periods.insert(
                    key.clone(),
                    crate::storage::provider_usage::ProviderQuotaPeriod {
                        start: period.start,
                        end: period.end,
                        cutoff: observed_at.unwrap_or(now),
                    },
                );
            }
            identified.insert(key, selected);
        }
        let provider = self.get_provider(&usage.provider_id).await?;
        let mut scanned = self
            .gw
            .storage
            .logs()
            .provider_quota_token_usage_with_periods(
                &usage.provider_id,
                now,
                period.as_ref().map(|period| (period.start, period.end)),
                &periods,
            )
            .await?;
        let mut priced: Vec<_> = scanned
            .all_time
            .iter()
            .chain(&scanned.period)
            .map(|row| row.usage.clone())
            .collect();
        let providers = HashMap::from([(usage.provider_id.clone(), provider)]);
        let overrides = token_price::parse_overrides(self.gw.storage.settings().list_all().await?)?;
        token_cost::enrich_token_costs(
            &mut priced,
            &providers,
            &self.gw.config.data_dir,
            &overrides,
        );
        for (row, mut priced) in scanned
            .all_time
            .iter_mut()
            .chain(&mut scanned.period)
            .zip(priced)
        {
            if row.fully_missing_usage_request_count > 0 {
                priced.cost = None;
            }
            row.usage = priced;
        }
        let unassigned: Vec<_> = scanned
            .all_time
            .iter()
            .filter(|row| partition_key(row).is_none())
            .cloned()
            .collect();
        let attribution_partial = !unassigned.is_empty();
        // A refresh losing today's buckets must not revive a mixed provider
        // denominator over immutable history from multiple retired identities.
        let has_bucket_history = scanned
            .all_time
            .iter()
            .any(|row| partition_key(row).is_some());
        let provider_period = if usage.quota_buckets.is_empty() && !has_bucket_history {
            period.as_ref()
        } else {
            None
        };
        let mut stats = price_stats(
            &scanned.all_time,
            &scanned.period,
            provider_period,
            true,
            false,
        )?;
        if !usage.quota_buckets.is_empty() || has_bucket_history {
            stats.unassigned = Some(totals_for(&unassigned));
            stats.unassigned_coverage = Some(coverage(&unassigned));
        }
        let mut buckets = HashMap::new();
        for bucket in usage
            .quota_buckets
            .iter()
            .filter(|bucket| bucket.is_primary())
        {
            let key = (bucket.scope_id.clone(), bucket.id.clone());
            let all: Vec<_> = scanned
                .all_time
                .iter()
                .filter(|row| partition_key(row).as_ref() == Some(&key))
                .cloned()
                .collect();
            let current: Vec<_> = scanned
                .period
                .iter()
                .filter(|row| partition_key(row).as_ref() == Some(&key))
                .cloned()
                .collect();
            let selected = identified.get(&key).and_then(Option::as_ref);
            // A valid immutable attributed sample is required; unresolved bindings
            // can show retained history, never manufacture a capacity estimate.
            let allow_capacity = !bucket.stale && bucket.binding.status == "resolved";
            buckets.insert(
                key,
                price_stats(
                    &all,
                    &current,
                    selected,
                    allow_capacity,
                    attribution_partial,
                )?,
            );
        }
        let mut retired: std::collections::BTreeMap<QuotaPriceKey, Vec<ProviderQuotaModelUsage>> =
            Default::default();
        for row in &scanned.all_time {
            if let Some(key) = partition_key(row) {
                if !identified.contains_key(&key) {
                    retired.entry(key).or_default().push(row.clone());
                }
            }
        }
        for ((scope_id, bucket_id), rows) in retired {
            stats.retained_buckets.push(RetainedQuotaBucketPriceStats {
                scope_id,
                bucket_id,
                all_time: totals_for(&rows),
                coverage: coverage(&rows),
                models: model_prices(&rows),
            });
        }
        Ok((stats, buckets))
    }
}

#[cfg(test)]
mod tests;
