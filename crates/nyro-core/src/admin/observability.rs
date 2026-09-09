use super::*;

const DEFAULT_TIME_SERIES_HOURS: i32 = 24;
const MAX_TIME_SERIES_HOURS: i32 = 168;
const MILLIS_PER_MINUTE: i64 = 60_000;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;

fn normalize_detail_hours(hours: Option<i32>) -> anyhow::Result<i32> {
    let hours = hours.unwrap_or(24);
    anyhow::ensure!(
        matches!(hours, 6 | 24 | 72 | 168),
        "hours must be one of 6, 24, 72, or 168"
    );
    Ok(hours)
}

fn normalize_time_series_hours(hours: Option<i32>) -> i32 {
    hours
        .unwrap_or(DEFAULT_TIME_SERIES_HOURS)
        .clamp(1, MAX_TIME_SERIES_HOURS)
}

fn time_series_bucket_minutes(hours: i32) -> i32 {
    match hours {
        ..=6 => 5,
        ..=24 => 15,
        ..=72 => 30,
        _ => 60,
    }
}

fn floor_to_bucket(timestamp_ms: i64, bucket_ms: i64) -> i64 {
    timestamp_ms.div_euclid(bucket_ms) * bucket_ms
}

fn empty_time_bucket(bucket_start: i64) -> StatsTimeBucket {
    StatsTimeBucket {
        bucket_start,
        request_count: 0,
        error_count: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read_tokens: 0,
        avg_duration_ms: None,
    }
}

fn fill_time_buckets(
    buckets: Vec<StatsTimeBucket>,
    start_ms: i64,
    end_ms: i64,
    bucket_ms: i64,
) -> Vec<StatsTimeBucket> {
    let mut buckets_by_start: HashMap<i64, StatsTimeBucket> = buckets
        .into_iter()
        .map(|bucket| (bucket.bucket_start, bucket))
        .collect();
    let first_bucket = floor_to_bucket(start_ms, bucket_ms);
    let last_bucket = floor_to_bucket(end_ms, bucket_ms);
    let mut points = Vec::new();
    let mut bucket_start = first_bucket;

    while bucket_start <= last_bucket {
        points.push(
            buckets_by_start
                .remove(&bucket_start)
                .unwrap_or_else(|| empty_time_bucket(bucket_start)),
        );
        bucket_start += bucket_ms;
    }

    points
}

#[derive(Debug, Serialize)]
pub struct RequestLogAttempts {
    pub client_request_id: String,
    pub result: Option<crate::logging::diagnostics::RequestResult>,
    pub attempts: Vec<RequestLog>,
}

fn decorate_log_result(log: &mut RequestLog) {
    log.is_error = crate::logging::diagnostics::is_error(
        log.client_status_code,
        log.upstream_status_code,
        log.outcome_version,
        &log.attempt_outcome,
    );
    log.effective_outcome = crate::logging::diagnostics::effective_outcome(
        log.client_status_code,
        log.upstream_status_code,
        log.outcome_version,
        &log.attempt_outcome,
    )
    .to_string();
}

impl AdminService {
    // ── Logs ──

    pub async fn query_logs(&self, q: LogQuery) -> anyhow::Result<LogPage> {
        let mut q = q;
        q.limit = Some(q.limit.unwrap_or(50).min(500));
        q.offset = Some(q.offset.unwrap_or(0));
        if let Some(outcome) = q.outcome.as_deref() {
            anyhow::ensure!(
                matches!(
                    outcome,
                    "error" | "completed" | "cancelled" | "output_limited" | "unknown"
                ),
                "unsupported log outcome filter"
            );
        }
        let mut page = self.gw.storage.logs().query(q).await?;
        for log in &mut page.items {
            decorate_log_result(log);
        }
        Ok(page)
    }

    pub async fn get_log(&self, id: &str) -> anyhow::Result<Option<RequestLog>> {
        let Some(mut log) = self.gw.storage.logs().find_by_id(id).await? else {
            return Ok(None);
        };
        decorate_log_result(&mut log);
        if let Some(request_id) = log.client_request_id.as_deref() {
            log.request_result = self.gw.storage.logs().request_result(request_id).await?;
        }
        Ok(Some(log))
    }

    pub async fn get_request_log_attempts(
        &self,
        request_id: &str,
    ) -> anyhow::Result<RequestLogAttempts> {
        anyhow::ensure!(
            !request_id.trim().is_empty() && request_id.len() <= 128,
            "invalid client request ID"
        );
        let result = self.gw.storage.logs().request_result(request_id).await?;
        // The correlated view is scalar-only; fetch individual payloads on demand.
        let mut attempts = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .query_logs(LogQuery {
                    client_request_id: Some(request_id.to_string()),
                    limit: Some(500),
                    offset: Some(offset),
                    ..Default::default()
                })
                .await?;
            let count = page.items.len();
            attempts.extend(page.items);
            offset += count as i64;
            if count == 0 || offset >= page.total {
                break;
            }
        }
        attempts.sort_by(|a, b| {
            a.attempt_index
                .cmp(&b.attempt_index)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(RequestLogAttempts {
            client_request_id: request_id.to_string(),
            result,
            attempts,
        })
    }

    pub async fn clear_logs(&self) -> anyhow::Result<u64> {
        self.gw.storage.logs().clear_all().await
    }

    /// Clear all recorded payloads, including failed attempts, without deleting metadata.
    pub async fn clear_log_payloads(&self) -> anyhow::Result<u64> {
        self.gw.storage.logs().clear_payloads().await
    }

    /// Delete a single request log row; 0 when the id does not exist.
    pub async fn delete_log(&self, id: &str) -> anyhow::Result<u64> {
        self.gw.storage.logs().delete_by_id(id).await
    }

    /// Delete actual HTTP errors or authoritative live failures/timeouts, once per attempt.
    pub async fn clear_error_logs(&self) -> anyhow::Result<u64> {
        self.gw.storage.logs().clear_errors().await
    }
    // ── Stats ──

    fn normalize_hours(hours: Option<i32>) -> Option<i32> {
        hours.and_then(|value| (value > 0).then_some(value))
    }

    pub async fn get_stats_overview(&self, hours: Option<i32>) -> anyhow::Result<StatsOverview> {
        self.gw
            .storage
            .logs()
            .stats_overview(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_stats_hourly(&self, hours: i32) -> anyhow::Result<Vec<StatsHourly>> {
        self.gw
            .storage
            .logs()
            .stats_hourly(i64::from(hours.max(1)))
            .await
    }

    pub async fn get_stats_timeseries(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<StatsTimeSeries> {
        let hours = normalize_time_series_hours(hours);
        let bucket_minutes = time_series_bucket_minutes(hours);
        let bucket_ms = i64::from(bucket_minutes) * MILLIS_PER_MINUTE;
        let end_at = Utc::now().timestamp_millis();
        let start_at = end_at - i64::from(hours) * MILLIS_PER_HOUR;
        let buckets = self
            .gw
            .storage
            .logs()
            .stats_time_buckets(start_at, end_at, bucket_ms, None)
            .await?;
        let has_data = !buckets.is_empty();

        Ok(StatsTimeSeries {
            start_at,
            end_at,
            bucket_minutes,
            has_data,
            points: fill_time_buckets(buckets, start_at, end_at, bucket_ms),
        })
    }

    pub async fn get_stats_by_model(&self, hours: Option<i32>) -> anyhow::Result<Vec<ModelStats>> {
        self.gw
            .storage
            .logs()
            .stats_by_model(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_model_usage_stats(
        &self,
        provider_id: &str,
        upstream_model: &str,
    ) -> anyhow::Result<ModelUsageStats> {
        self.gw
            .storage
            .logs()
            .model_usage_stats(provider_id, upstream_model)
            .await
    }

    pub async fn get_stats_by_provider(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<Vec<ProviderStats>> {
        let mut stats = self
            .gw
            .storage
            .logs()
            .stats_by_provider(Self::normalize_hours(hours).map(i64::from))
            .await?;
        let providers: HashMap<_, _> = self
            .gw
            .storage
            .providers()
            .list()
            .await?
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        for item in &mut stats {
            if let Some(provider) = providers.get(&item.provider_id) {
                item.provider.clone_from(&provider.name);
                item.provider_icon = provider.preset_key.clone().or(provider.vendor.clone());
                item.provider_protocol = Some(provider.protocol.clone());
            }
        }
        Ok(stats)
    }

    pub async fn get_provider_usage_detail(
        &self,
        provider_id: &str,
        hours: Option<i32>,
    ) -> anyhow::Result<ProviderUsageDetail> {
        let hours = normalize_detail_hours(hours)?;
        let end_at = Utc::now().timestamp_millis();
        let start_at = end_at - i64::from(hours) * MILLIS_PER_HOUR;
        let mut detail = self
            .gw
            .storage
            .logs()
            .provider_usage_detail(provider_id, start_at, end_at)
            .await?;
        if let Some(provider) = self.gw.storage.providers().get(provider_id).await? {
            detail.provider_name = provider.name;
            detail.provider_icon = provider.preset_key.or(provider.vendor);
            detail.provider_protocol = Some(provider.protocol);
        }
        Ok(detail)
    }

    pub async fn get_stats_by_api_key(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        let mut stats = self
            .gw
            .storage
            .logs()
            .stats_by_api_key(Self::normalize_hours(hours).map(i64::from))
            .await?;
        if let Some(store) = self.gw.storage.api_keys() {
            let current_names: HashMap<_, _> = store
                .list()
                .await?
                .into_iter()
                .map(|key| (key.id, key.name))
                .collect();
            for item in &mut stats {
                if let Some(name) = current_names.get(&item.api_key_id) {
                    item.api_key_name.clone_from(name);
                }
            }
        }
        Ok(stats)
    }

    pub async fn get_api_key_usage_detail(
        &self,
        api_key_id: &str,
        hours: Option<i32>,
    ) -> anyhow::Result<ApiKeyUsageDetail> {
        let hours = normalize_detail_hours(hours)?;
        let end_at = Utc::now().timestamp_millis();
        let start_at = end_at - i64::from(hours) * MILLIS_PER_HOUR;
        let mut detail = self
            .gw
            .storage
            .logs()
            .api_key_usage_detail(api_key_id, start_at, end_at)
            .await?;

        if let Some(store) = self.gw.storage.api_keys() {
            if let Some(key) = store.get(api_key_id).await? {
                detail.api_key_name = key.name;
            }
        }
        let provider_names: HashMap<_, _> = self
            .gw
            .storage
            .providers()
            .list()
            .await?
            .into_iter()
            .map(|provider| (provider.id, provider.name))
            .collect();
        for route in &mut detail.model_routes {
            if let Some(name) = provider_names.get(&route.provider_id) {
                route.provider_name.clone_from(name);
            }
        }

        // Per-model token time series over the exact same window as the
        // summary cards, so trends and totals above can never disagree.
        let bucket_minutes = time_series_bucket_minutes(hours);
        let bucket_ms = i64::from(bucket_minutes) * MILLIS_PER_MINUTE;
        let rows = self
            .gw
            .storage
            .logs()
            .api_key_model_time_buckets(api_key_id, start_at, end_at, bucket_ms)
            .await?;
        let mut by_model: HashMap<String, Vec<StatsTimeBucket>> = HashMap::new();
        for row in rows {
            by_model
                .entry(row.upstream_model.clone())
                .or_default()
                .push(StatsTimeBucket {
                    bucket_start: row.bucket_start,
                    request_count: row.request_count,
                    error_count: row.error_count,
                    total_input_tokens: row.total_input_tokens,
                    total_output_tokens: row.total_output_tokens,
                    total_cache_read_tokens: row.total_cache_read_tokens,
                    avg_duration_ms: row.avg_duration_ms,
                });
        }
        let model_tokens = |item: &ApiKeyModelTimeSeries| {
            item.series
                .points
                .iter()
                .map(|point| point.total_input_tokens + point.total_output_tokens)
                .sum::<i64>()
        };
        let mut model_time_series: Vec<ApiKeyModelTimeSeries> = by_model
            .into_iter()
            .map(|(upstream_model, buckets)| ApiKeyModelTimeSeries {
                upstream_model,
                series: StatsTimeSeries {
                    start_at,
                    end_at,
                    bucket_minutes,
                    has_data: !buckets.is_empty(),
                    points: fill_time_buckets(buckets, start_at, end_at, bucket_ms),
                },
            })
            .collect();
        model_time_series.sort_by(|a, b| {
            model_tokens(b)
                .cmp(&model_tokens(a))
                .then_with(|| a.upstream_model.cmp(&b.upstream_model))
        });
        detail.model_time_series = model_time_series;

        Ok(detail)
    }

    pub async fn get_model_usage_detail(
        &self,
        upstream_model: &str,
        hours: Option<i32>,
    ) -> anyhow::Result<ModelUsageDetail> {
        let hours = normalize_detail_hours(hours)?;
        let end_at = Utc::now().timestamp_millis();
        let start_at = end_at - i64::from(hours) * MILLIS_PER_HOUR;
        let mut detail = self
            .gw
            .storage
            .logs()
            .model_usage_detail(upstream_model, start_at, end_at)
            .await?;

        let providers: HashMap<_, _> = self
            .gw
            .storage
            .providers()
            .list()
            .await?
            .into_iter()
            .map(|provider| (provider.id.clone(), provider))
            .collect();
        for item in &mut detail.providers {
            if let Some(provider) = providers.get(&item.provider_id) {
                item.provider_name.clone_from(&provider.name);
                item.provider_icon = provider.preset_key.clone().or(provider.vendor.clone());
                item.provider_protocol = Some(provider.protocol.clone());
            }
        }

        if let Some(store) = self.gw.storage.api_keys() {
            let current_names: HashMap<_, _> = store
                .list()
                .await?
                .into_iter()
                .map(|key| (key.id, key.name))
                .collect();
            for item in &mut detail.api_keys {
                if let Some(name) = current_names.get(&item.api_key_id) {
                    item.api_key_name.clone_from(name);
                }
            }
        }

        // Token time series over the exact same window as the summary cards so
        // the chart and the totals above it can never disagree.
        let bucket_minutes = time_series_bucket_minutes(hours);
        let bucket_ms = i64::from(bucket_minutes) * MILLIS_PER_MINUTE;
        let buckets = self
            .gw
            .storage
            .logs()
            .stats_time_buckets(start_at, end_at, bucket_ms, Some(upstream_model))
            .await?;
        detail.time_series = Some(StatsTimeSeries {
            start_at,
            end_at,
            bucket_minutes,
            has_data: !buckets.is_empty(),
            points: fill_time_buckets(buckets, start_at, end_at, bucket_ms),
        });

        Ok(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_bucket(bucket_start: i64, request_count: i64) -> StatsTimeBucket {
        StatsTimeBucket {
            bucket_start,
            request_count,
            error_count: 1,
            total_input_tokens: 100,
            total_output_tokens: 25,
            total_cache_read_tokens: 40,
            avg_duration_ms: Some(250.0),
        }
    }

    #[test]
    fn detail_hours_use_default_and_reject_unsupported_values() {
        assert_eq!(normalize_detail_hours(None).unwrap(), 24);
        for hours in [6, 24, 72, 168] {
            assert_eq!(normalize_detail_hours(Some(hours)).unwrap(), hours);
        }
        for hours in [-1, 0, 1, 12, 169] {
            assert!(normalize_detail_hours(Some(hours)).is_err());
        }
    }

    #[test]
    fn adaptive_bucket_policy_matches_supported_ranges() {
        assert_eq!(time_series_bucket_minutes(1), 5);
        assert_eq!(time_series_bucket_minutes(6), 5);
        assert_eq!(time_series_bucket_minutes(7), 15);
        assert_eq!(time_series_bucket_minutes(24), 15);
        assert_eq!(time_series_bucket_minutes(25), 30);
        assert_eq!(time_series_bucket_minutes(72), 30);
        assert_eq!(time_series_bucket_minutes(73), 60);
        assert_eq!(time_series_bucket_minutes(168), 60);
    }

    #[test]
    fn time_series_range_uses_defaults_and_supported_bounds() {
        assert_eq!(normalize_time_series_hours(None), 24);
        assert_eq!(normalize_time_series_hours(Some(0)), 1);
        assert_eq!(normalize_time_series_hours(Some(6)), 6);
        assert_eq!(normalize_time_series_hours(Some(999)), 168);
    }

    #[test]
    fn fills_missing_and_partial_boundary_buckets() {
        let bucket_ms = 5 * MILLIS_PER_MINUTE;
        let points = fill_time_buckets(
            vec![populated_bucket(0, 2), populated_bucket(2 * bucket_ms, 3)],
            MILLIS_PER_MINUTE,
            2 * bucket_ms + MILLIS_PER_MINUTE,
            bucket_ms,
        );

        assert_eq!(points.len(), 3);
        assert_eq!(points[0].bucket_start, 0);
        assert_eq!(points[0].request_count, 2);
        assert_eq!(points[1].bucket_start, bucket_ms);
        assert_eq!(points[1].request_count, 0);
        assert_eq!(points[1].avg_duration_ms, None);
        assert_eq!(points[2].bucket_start, 2 * bucket_ms);
        assert_eq!(points[2].request_count, 3);
    }

    #[test]
    fn empty_series_still_builds_a_stable_axis() {
        let bucket_ms = 15 * MILLIS_PER_MINUTE;
        let points = fill_time_buckets(Vec::new(), 0, bucket_ms, bucket_ms);

        assert_eq!(points.len(), 2);
        assert!(points.iter().all(|point| point.request_count == 0));
    }
}
