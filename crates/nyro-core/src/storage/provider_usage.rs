//! Provider-scoped retained usage. Never reconstruct quota history from today's mapping.
use std::collections::{BTreeMap, HashMap};

use crate::db::models::{
    LogPage, LogQuery, ProviderModelUsageStats, ProviderUsageDetail, TokenUsageDay,
};

/// Narrow streaming detail projection: no request/response payload columns.
#[derive(sqlx::FromRow)]
pub(super) struct QuotaDetailRow {
    #[sqlx(flatten)]
    usage: QuotaUsageRow,
    reasoning_tokens: Option<i64>,
    latency_total_ms: Option<i64>,
    latency_upstream_ms: Option<i64>,
    stream_first_chunk_ms: Option<i64>,
    client_status_code: Option<i32>,
    upstream_status_code: Option<i32>,
    outcome_version: i32,
    attempt_outcome: String,
}

/// Summary and priceable usage from the SAME single provider/range scan.
pub struct ProviderQuotaUsageDetail {
    pub detail: ProviderUsageDetail,
    pub usage: Vec<ProviderQuotaModelUsage>,
}

#[derive(Default)]
struct DetailMetrics {
    requests: i64,
    errors: i64,
    completed: i64,
    unknown: i64,
    cancelled: i64,
    output_limited: i64,
    input: i64,
    output: i64,
    cached: i64,
    duration_sum: i64,
    duration_count: i64,
    first_sum: i64,
    first_count: i64,
    upstream_sum: i64,
    last: Option<i64>,
    tps: crate::db::tps::TpsTotals,
}

impl DetailMetrics {
    fn add(&mut self, row: &QuotaDetailRow) {
        let usage = &row.usage;
        self.requests += 1;
        match crate::logging::diagnostics::effective_outcome(
            row.client_status_code,
            row.upstream_status_code,
            row.outcome_version,
            &row.attempt_outcome,
        ) {
            "error" => self.errors += 1,
            "completed" => self.completed += 1,
            "cancelled" => self.cancelled += 1,
            "output_limited" => self.output_limited += 1,
            _ => self.unknown += 1,
        }
        self.input = self
            .input
            .saturating_add(usage.input_tokens.unwrap_or_default());
        self.output = self
            .output
            .saturating_add(usage.output_tokens.unwrap_or_default());
        self.cached = self
            .cached
            .saturating_add(usage.cache_read_tokens.unwrap_or_default());
        if let Some(ms) = row.latency_total_ms {
            self.duration_sum = self.duration_sum.saturating_add(ms);
            self.duration_count += 1;
        }
        if let Some(ms) = row.stream_first_chunk_ms.filter(|ms| *ms >= 0) {
            self.first_sum = self.first_sum.saturating_add(ms);
            self.first_count += 1;
        }
        self.upstream_sum = self
            .upstream_sum
            .saturating_add(row.latency_upstream_ms.unwrap_or_default());
        self.last = Some(
            self.last
                .map_or(usage.created_at, |last| last.max(usage.created_at)),
        );
        if let (Some(output), Some(elapsed)) = (
            usage.output_tokens.filter(|tokens| *tokens > 0),
            row.latency_upstream_ms
                .or(row.latency_total_ms)
                .filter(|ms| *ms > 0),
        ) {
            self.tps.tps_output_tokens = self.tps.tps_output_tokens.saturating_add(output);
            self.tps.tps_content_tokens = self
                .tps
                .tps_content_tokens
                .saturating_add(output - row.reasoning_tokens.unwrap_or_default().clamp(0, output));
            self.tps.tps_elapsed_ms = self.tps.tps_elapsed_ms.saturating_add(elapsed);
        }
    }
    fn model(&self, model: String) -> ProviderModelUsageStats {
        ProviderModelUsageStats {
            upstream_model: model,
            request_count: self.requests,
            error_count: self.errors,
            total_input_tokens: self.input,
            total_output_tokens: self.output,
            total_cache_read_tokens: self.cached,
            avg_duration_ms: if self.duration_count > 0 {
                self.duration_sum as f64 / self.duration_count as f64
            } else {
                0.0
            },
            avg_first_token_ms: (self.first_count > 0)
                .then(|| self.first_sum as f64 / self.first_count as f64),
            total_upstream_ms: self.upstream_sum as f64,
            last_used_at: self.last,
            tps_totals: self.tps,
            equivalent_costs: Vec::new(),
            unpriced_request_count: 0,
        }
    }
}

/// O(distinct exact models/scopes) retained memory, independent of attempt count.
pub(super) struct QuotaDetailAccumulator<'a> {
    provider: &'a str,
    start: i64,
    end: i64,
    bucket: &'a str,
    scope: Option<&'a str>,
    summary: DetailMetrics,
    models: BTreeMap<String, DetailMetrics>,
    usage: QuotaUsageAccumulator<'a>,
}

impl<'a> QuotaDetailAccumulator<'a> {
    pub(super) fn new(
        provider: &'a str,
        start: i64,
        end: i64,
        bucket: &'a str,
        scope: Option<&'a str>,
        periods: &'a HashMap<(String, String), ProviderQuotaPeriod>,
    ) -> Self {
        Self {
            provider,
            start,
            end,
            bucket,
            scope,
            summary: Default::default(),
            models: Default::default(),
            usage: QuotaUsageAccumulator::new(provider, end, None, periods),
        }
    }
    pub(super) fn add(&mut self, mut row: QuotaDetailRow) {
        if row.usage.created_at < self.start
            || row.usage.created_at > self.end
            || !matches_quota_filter(row.usage.route_decision.as_deref(), self.bucket, self.scope)
        {
            return;
        }
        let model = quota_priced_model(
            row.usage.route_decision.as_deref(),
            row.usage.upstream_model.as_deref(),
        );
        self.summary.add(&row);
        self.models.entry(model).or_default().add(&row);
        // Detail is an emitted-time range, exactly like log filtering, not an
        // upstream observation cutoff. Completion time cannot change its set.
        row.usage.performance_completed_at = None;
        self.usage.add(row.usage);
    }
    pub(super) fn finish(self) -> ProviderQuotaUsageDetail {
        let summary = self.summary.model(String::new());
        let mut models: Vec<_> = self
            .models
            .into_iter()
            .map(|(model, metrics)| metrics.model(model))
            .collect();
        models.sort_by(|a, b| {
            b.request_count
                .cmp(&a.request_count)
                .then_with(|| a.upstream_model.cmp(&b.upstream_model))
        });
        ProviderQuotaUsageDetail {
            detail: ProviderUsageDetail {
                start_at: self.start,
                end_at: self.end,
                provider_id: self.provider.to_owned(),
                provider_name: self.provider.to_owned(),
                provider_icon: None,
                provider_protocol: None,
                request_count: summary.request_count,
                success_count: self.summary.completed,
                error_count: summary.error_count,
                unknown_count: self.summary.unknown,
                cancelled_count: self.summary.cancelled,
                output_limited_count: self.summary.output_limited,
                outcome_stats_version: 1,
                total_input_tokens: summary.total_input_tokens,
                total_output_tokens: summary.total_output_tokens,
                total_cache_read_tokens: summary.total_cache_read_tokens,
                avg_duration_ms: summary.avg_duration_ms,
                avg_first_token_ms: summary.avg_first_token_ms,
                total_upstream_ms: summary.total_upstream_ms,
                last_used_at: summary.last_used_at,
                tps_totals: summary.tps_totals,
                models,
                equivalent_costs: Vec::new(),
                unpriced_request_count: 0,
            },
            usage: self.usage.finish().all_time,
        }
    }
}

/// Only server-owned metadata and usage columns are read; never incoming payloads.
#[derive(sqlx::FromRow)]
pub(super) struct ProviderUsageRow {
    pub(super) created_at: i64,
    pub(super) upstream_model: Option<String>,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) cache_read_tokens: Option<i64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct QuotaUsageRow {
    pub(super) created_at: i64,
    pub(super) performance_completed_at: Option<i64>,
    pub(super) route_decision: Option<String>,
    pub(super) upstream_model: Option<String>,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) cache_read_tokens: Option<i64>,
}

/// One immutable primary accounting partition and exact priced model. Absent
/// identities mean unassigned, not the provider's currently configured bucket.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderQuotaModelUsage {
    pub scope_id: Option<String>,
    pub bucket_id: Option<String>,
    pub usage: TokenUsageDay,
    pub attributed_request_count: i64,
    pub inferred_request_count: i64,
    pub unassigned_request_count: i64,
    pub legacy_timestamp_request_count: i64,
    /// No input OR output evidence: must not become a priced free zero.
    pub fully_missing_usage_request_count: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderQuotaPeriod {
    pub start: i64,
    pub end: i64,
    /// Captured bucket observation cutoff, at most the provider snapshot cutoff.
    pub cutoff: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderQuotaUsage {
    pub all_time: Vec<ProviderQuotaModelUsage>,
    pub period: Vec<ProviderQuotaModelUsage>,
}

use crate::quota_buckets::QuotaAttribution as Attribution;

/// Guarded parser shared by pricing and all bucket-filtered admin queries.
/// Deliberately no historical inference: current bindings are not time-valid evidence.
fn attribution(raw: Option<&str>) -> Option<Attribution> {
    let decision: serde_json::Value = serde_json::from_str(raw?).ok()?;
    let item: Attribution =
        serde_json::from_value(decision.get("quota_attribution")?.clone()).ok()?;
    let nonempty = |value: &str| !value.trim().is_empty();
    (item.version == 1
        && nonempty(&item.bucket_id)
        && item.bucket_id != "unassigned"
        && nonempty(&item.scope_id)
        && nonempty(&item.backend_model)
        && nonempty(&item.wire_model)
        && nonempty(&item.source)
        && item.dispatched_at >= 0)
        .then_some(item)
}

pub(crate) fn quota_priced_model(
    route_decision: Option<&str>,
    upstream_model: Option<&str>,
) -> String {
    attribution(route_decision)
        .map(|item| item.wire_model)
        .unwrap_or_else(|| upstream_model.unwrap_or_default().to_owned())
}

pub(crate) fn matches_quota_filter(
    route_decision: Option<&str>,
    bucket: &str,
    scope: Option<&str>,
) -> bool {
    let item = attribution(route_decision);
    if bucket == "unassigned" {
        return item.is_none() && scope.is_none();
    }
    item.is_some_and(|item| {
        item.bucket_id == bucket && scope.is_none_or(|scope| item.scope_id == scope)
    })
}

/// Dialect-independent fallback over existing scalar-only, bound-parameter log
/// queries. Pagination happens AFTER the exact immutable attribution predicate;
/// malformed historical JSON cannot throw PostgreSQL/MySQL cast errors.
pub(super) async fn query_quota_logs(
    store: &dyn super::traits::LogStore,
    mut query: LogQuery,
) -> anyhow::Result<LogPage> {
    let bucket = query.quota_bucket.take().unwrap_or_default();
    let scope = query.quota_scope.take();
    let offset = query.offset.unwrap_or(0).max(0) as usize;
    let limit = query.limit.unwrap_or(50).clamp(0, 500) as usize;
    let mut scanned = 0;
    let mut total = 0;
    let mut items = Vec::new();
    loop {
        query.limit = Some(500);
        query.offset = Some(scanned);
        let page = store.query(query.clone()).await?;
        let count = page.items.len();
        for log in page.items {
            if matches_quota_filter(log.route_decision.as_deref(), &bucket, scope.as_deref()) {
                if total >= offset && items.len() < limit {
                    items.push(log);
                }
                total += 1;
            }
        }
        scanned += count as i64;
        if count == 0 || scanned >= page.total {
            break;
        }
    }
    Ok(LogPage {
        items,
        total: total as i64,
    })
}

fn empty_usage(provider_id: &str, model: &str) -> TokenUsageDay {
    TokenUsageDay {
        date: String::new(),
        provider_id: provider_id.to_owned(),
        provider_name: provider_id.to_owned(),
        model: model.to_owned(),
        request_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        missing_usage_count: 0,
        cache_read_reported_count: 0,
        cost: None,
    }
}

fn add_tokens(
    usage: &mut TokenUsageDay,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
) {
    usage.request_count = usage.request_count.saturating_add(1);
    usage.input_tokens = usage.input_tokens.saturating_add(input.unwrap_or_default());
    usage.output_tokens = usage
        .output_tokens
        .saturating_add(output.unwrap_or_default());
    // Gross input already contains cache reads; never count them twice.
    usage.cache_read_tokens = usage
        .cache_read_tokens
        .saturating_add(cached.unwrap_or_default());
    usage.missing_usage_count = usage
        .missing_usage_count
        .saturating_add(i64::from(input.is_none() || output.is_none()));
    usage.cache_read_reported_count = usage
        .cache_read_reported_count
        .saturating_add(i64::from(cached.is_some()));
}

pub(super) struct ProviderUsageAccumulator<'a> {
    provider_id: &'a str,
    period: Option<(i64, i64)>,
    all_time: BTreeMap<String, TokenUsageDay>,
    current: BTreeMap<String, TokenUsageDay>,
}

impl<'a> ProviderUsageAccumulator<'a> {
    pub(super) fn new(provider_id: &'a str, period: Option<(i64, i64)>) -> Self {
        Self {
            provider_id,
            period: period.filter(|(start, end)| start < end),
            all_time: BTreeMap::new(),
            current: BTreeMap::new(),
        }
    }
    pub(super) fn add(&mut self, row: ProviderUsageRow) {
        Self::accumulate(&mut self.all_time, self.provider_id, &row);
        if self
            .period
            .is_some_and(|(start, end)| start <= row.created_at && row.created_at < end)
        {
            Self::accumulate(&mut self.current, self.provider_id, &row);
        }
    }
    fn accumulate(
        groups: &mut BTreeMap<String, TokenUsageDay>,
        provider: &str,
        row: &ProviderUsageRow,
    ) {
        let model = row.upstream_model.as_deref().unwrap_or_default();
        let usage = groups
            .entry(model.to_owned())
            .or_insert_with(|| empty_usage(provider, model));
        add_tokens(
            usage,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
        );
    }
    pub(super) fn finish(self) -> (Vec<TokenUsageDay>, Vec<TokenUsageDay>) {
        (
            self.all_time.into_values().collect(),
            self.current.into_values().collect(),
        )
    }
}

// Keep wholly missing usage separate from observed/partial usage so the cost
// engine cannot turn absent token evidence into a priced zero request.
type GroupKey = (Option<(String, String)>, String, bool);

pub(super) struct QuotaUsageAccumulator<'a> {
    provider_id: &'a str,
    now: i64,
    provider_period: Option<(i64, i64)>,
    periods: &'a HashMap<(String, String), crate::storage::provider_usage::ProviderQuotaPeriod>,
    all_time: BTreeMap<GroupKey, ProviderQuotaModelUsage>,
    current: BTreeMap<GroupKey, ProviderQuotaModelUsage>,
}

impl<'a> QuotaUsageAccumulator<'a> {
    pub(super) fn new(
        provider_id: &'a str,
        now: i64,
        provider_period: Option<(i64, i64)>,
        periods: &'a HashMap<(String, String), crate::storage::provider_usage::ProviderQuotaPeriod>,
    ) -> Self {
        Self {
            provider_id,
            now,
            provider_period,
            periods,
            all_time: BTreeMap::new(),
            current: BTreeMap::new(),
        }
    }
    pub(super) fn add(&mut self, row: QuotaUsageRow) {
        // Rows emitted/finished after the captured cutoff are not visible usage.
        if row.created_at > self.now
            || row
                .performance_completed_at
                .is_some_and(|time| time > self.now)
        {
            return;
        }
        let item = attribution(row.route_decision.as_deref());
        let anchor = item
            .as_ref()
            .map(|item| item.dispatched_at)
            .unwrap_or(row.created_at);
        let identity = item
            .as_ref()
            .map(|item| (item.scope_id.clone(), item.bucket_id.clone()));
        let model = item
            .as_ref()
            .map(|item| item.wire_model.clone())
            .unwrap_or_else(|| row.upstream_model.clone().unwrap_or_default());
        let key = (
            identity.clone(),
            model,
            row.input_tokens.is_none() && row.output_tokens.is_none(),
        );
        Self::accumulate(&mut self.all_time, self.provider_id, &key, &row);
        // Legacy/provider-detail ranges filter emitted rows, exactly like logs.
        // Only per-bucket quota cycles use immutable dispatch-time membership.
        let (period, timestamp) = match self.provider_period {
            Some(period) => (Some(period), row.created_at),
            None => (
                identity
                    .as_ref()
                    .and_then(|key| self.periods.get(key))
                    .filter(|period| {
                        row.created_at <= period.cutoff
                            && anchor <= period.cutoff
                            && row
                                .performance_completed_at
                                .is_none_or(|time| time <= period.cutoff)
                    })
                    .map(|period| (period.start, period.end)),
                anchor,
            ),
        };
        if period.is_some_and(|(start, end)| start < end && start <= timestamp && timestamp < end) {
            Self::accumulate(&mut self.current, self.provider_id, &key, &row);
        }
    }
    fn accumulate(
        groups: &mut BTreeMap<GroupKey, ProviderQuotaModelUsage>,
        provider: &str,
        key: &GroupKey,
        row: &QuotaUsageRow,
    ) {
        let usage = groups
            .entry(key.clone())
            .or_insert_with(|| ProviderQuotaModelUsage {
                scope_id: key.0.as_ref().map(|key| key.0.clone()),
                bucket_id: key.0.as_ref().map(|key| key.1.clone()),
                usage: empty_usage(provider, &key.1),
                attributed_request_count: 0,
                inferred_request_count: 0,
                unassigned_request_count: 0,
                legacy_timestamp_request_count: 0,
                fully_missing_usage_request_count: 0,
            });
        add_tokens(
            &mut usage.usage,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
        );
        if key.2 {
            usage.fully_missing_usage_request_count =
                usage.fully_missing_usage_request_count.saturating_add(1);
        }
        if key.0.is_some() {
            usage.attributed_request_count = usage.attributed_request_count.saturating_add(1);
        } else {
            usage.unassigned_request_count = usage.unassigned_request_count.saturating_add(1);
            usage.legacy_timestamp_request_count =
                usage.legacy_timestamp_request_count.saturating_add(1);
        }
    }
    pub(super) fn finish(self) -> ProviderQuotaUsage {
        ProviderQuotaUsage {
            all_time: self.all_time.into_values().collect(),
            period: self.current.into_values().collect(),
        }
    }
}
