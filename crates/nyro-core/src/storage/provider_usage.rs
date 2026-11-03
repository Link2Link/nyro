//! Provider-scoped retained usage, independent of account-pool attribution.
use std::collections::BTreeMap;

use crate::db::models::TokenUsageDay;

/// Only usage columns are streamed; request/response payloads and account JSON
/// are irrelevant to provider totals and never loaded here.
#[derive(sqlx::FromRow)]
pub(super) struct ProviderUsageRow {
    created_at: i64,
    upstream_model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
}

/// Shared accounting and exact model identity across all SQL dialects. Both
/// aggregates are built during the same provider-filtered retained-log scan.
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
        provider_id: &str,
        row: &ProviderUsageRow,
    ) {
        // Do not normalize model bytes or substitute the client-facing alias.
        let model = row.upstream_model.as_deref().unwrap_or_default();
        let usage = groups
            .entry(model.to_owned())
            .or_insert_with(|| TokenUsageDay {
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
            });
        usage.request_count = usage.request_count.saturating_add(1);
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(row.input_tokens.unwrap_or_default());
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(row.output_tokens.unwrap_or_default());
        // Gross input already contains cache reads; never count them twice.
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(row.cache_read_tokens.unwrap_or_default());
        usage.missing_usage_count = usage.missing_usage_count.saturating_add(i64::from(
            row.input_tokens.is_none() || row.output_tokens.is_none(),
        ));
        usage.cache_read_reported_count = usage
            .cache_read_reported_count
            .saturating_add(i64::from(row.cache_read_tokens.is_some()));
    }

    pub(super) fn finish(self) -> (Vec<TokenUsageDay>, Vec<TokenUsageDay>) {
        (
            self.all_time.into_values().collect(),
            self.current.into_values().collect(),
        )
    }
}
