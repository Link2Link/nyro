use super::*;

const DEFAULT_TIME_SERIES_HOURS: i32 = 24;
const MAX_TIME_SERIES_HOURS: i32 = 168;
const MILLIS_PER_MINUTE: i64 = 60_000;
const MILLIS_PER_HOUR: i64 = 60 * MILLIS_PER_MINUTE;

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

impl AdminService {
    // ── Logs ──

    pub async fn query_logs(&self, q: LogQuery) -> anyhow::Result<LogPage> {
        let mut q = q;
        q.limit = Some(q.limit.unwrap_or(50).min(500));
        q.offset = Some(q.offset.unwrap_or(0));
        self.gw.storage.logs().query(q).await
    }

    pub async fn get_log(&self, id: &str) -> anyhow::Result<Option<RequestLog>> {
        self.gw.storage.logs().find_by_id(id).await
    }

    pub async fn clear_logs(&self) -> anyhow::Result<u64> {
        self.gw.storage.logs().clear_all().await
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
            .stats_time_buckets(start_at, end_at, bucket_ms)
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
        self.gw
            .storage
            .logs()
            .stats_by_provider(Self::normalize_hours(hours).map(i64::from))
            .await
    }

    pub async fn get_stats_by_api_key(
        &self,
        hours: Option<i32>,
    ) -> anyhow::Result<Vec<ApiKeyStats>> {
        self.gw
            .storage
            .logs()
            .stats_by_api_key(Self::normalize_hours(hours).map(i64::from))
            .await
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
