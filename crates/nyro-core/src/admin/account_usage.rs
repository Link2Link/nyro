//! Retained gateway usage for account-pool rows, separate from upstream quotas.
use super::*;

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
    pub period: AccountUsageTotals,
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
    /// All retained logs and a caller-local calendar period, ending at the same
    /// captured `now`. The UI supplies its Monday/month-start as epoch ms so the
    /// result is independent of the server/database time zone. No upstream calls,
    /// credential mutations, or guesses based on account labels are made.
    pub async fn get_provider_account_usage_stats(
        &self,
        provider_id: &str,
        period_start: i64,
    ) -> anyhow::Result<Vec<AccountUsageStats>> {
        let now = Utc::now().timestamp_millis();
        anyhow::ensure!(
            (0..=now).contains(&period_start),
            "{INVALID_RANGE_PREFIX}: period_start must be between 0 and now"
        );
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
        let (all_time, period) = tokio::try_join!(
            self.gw
                .storage
                .logs()
                .account_token_usage(provider_id, None, Some(now)),
            self.gw
                .storage
                .logs()
                .account_token_usage(provider_id, Some(period_start), Some(now)),
        )?;
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
                period: summarize(
                    &by_account
                        .remove(&(account.account_id.clone(), true))
                        .unwrap_or_default(),
                ),
                account_id: account.account_id,
            })
            .collect())
    }
}
