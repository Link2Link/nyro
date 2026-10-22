use std::collections::BTreeMap;

use crate::db::models::AccountModelUsage;

/// Only metadata and usage columns are read; never load request/response payloads.
#[derive(sqlx::FromRow)]
pub(super) struct AccountUsageRow {
    route_decision: Option<String>,
    upstream_model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
}

/// Shared across SQL dialects so malformed historical JSON and account identity
/// have the same semantics without database-specific JSON casts or label joins.
#[derive(Default)]
pub(super) struct AccountUsageAccumulator {
    groups: BTreeMap<(String, String), AccountModelUsage>,
}

impl AccountUsageAccumulator {
    pub(super) fn add(&mut self, row: AccountUsageRow) {
        let Some(decision) = row
            .route_decision
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        else {
            return;
        };
        let Some(account_id) = decision
            .get("account")
            .and_then(|account| account.get("account_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
        else {
            return;
        };
        // Keep the exact ID/model bytes: no label/version grouping or normalization.
        let model = row.upstream_model.unwrap_or_default();
        let usage = self
            .groups
            .entry((account_id.to_owned(), model.clone()))
            .or_insert_with(|| AccountModelUsage {
                account_id: account_id.to_owned(),
                model,
                ..Default::default()
            });
        usage.request_count += 1;
        usage.input_tokens += row.input_tokens.unwrap_or_default();
        usage.output_tokens += row.output_tokens.unwrap_or_default();
        // Cache reads are already included in input_tokens, not extra input.
        usage.cache_read_tokens += row.cache_read_tokens.unwrap_or_default();
        usage.missing_usage_count +=
            i64::from(row.input_tokens.is_none() || row.output_tokens.is_none());
        usage.cache_read_reported_count += i64::from(row.cache_read_tokens.is_some());
    }

    pub(super) fn finish(self) -> Vec<AccountModelUsage> {
        self.groups.into_values().collect()
    }
}
