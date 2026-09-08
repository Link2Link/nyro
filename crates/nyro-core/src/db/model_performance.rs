//! Performance samples from the same retained request logs as model usage.
use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelPerformanceStats {
    pub selected_request_count: i64,
    pub valid_tps_count: i64,
    pub average_tps: Option<f64>,
    pub first_sample_at: Option<i64>,
    pub last_sample_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PairPerformanceStats {
    pub provider_id: String,
    pub upstream_model: String,
    pub mixed: ModelPerformanceStats,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct PerformanceSample {
    pub created_at: i64,
    #[sqlx(flatten)]
    pub performance: super::models::RecentModelPerformance,
}

impl ModelPerformanceStats {
    pub(crate) fn from_samples(rows: &[PerformanceSample]) -> Self {
        let mut stats = Self {
            selected_request_count: rows.len() as i64,
            ..Default::default()
        };
        let mut tps_total = 0.0;
        for row in rows {
            let Some(tps) = row.performance.tps() else {
                continue;
            };
            tps_total += tps;
            stats.valid_tps_count += 1;
            let at = row.created_at;
            stats.first_sample_at = Some(stats.first_sample_at.map_or(at, |old| old.min(at)));
            stats.last_sample_at = Some(stats.last_sample_at.map_or(at, |old| old.max(at)));
        }
        stats.average_tps =
            (stats.valid_tps_count > 0).then_some(tps_total / stats.valid_tps_count as f64);
        stats
    }
}

// Keep ordering and raw scalar selection aligned with model_usage_stats. Invalid
// rows consume the latest-ten window; completion metadata remains diagnostic only.
macro_rules! model_performance_method {
    ($this:expr, $pairs:expr, $as_of:expr, $database:ty, $model_column:expr, $timestamp_cast:expr, $stream_default:expr) => {{
            let pairs = $pairs;
            let _ = $as_of; // Compatibility: usage statistics cover all retained logs.
            let mut result = Vec::with_capacity(pairs.len());
            for (provider, model) in pairs {
                let mut sql = sqlx::QueryBuilder::<$database>::new("SELECT ");
                sql.push($timestamp_cast).push(" AS created_at, COALESCE(output_tokens, 0) AS output_tokens, COALESCE(is_stream, ");
                sql.push($stream_default).push(") AS is_stream, COALESCE(stream_chunks_count, 0) AS stream_chunks_count, latency_upstream_ms, latency_total_ms, stream_first_chunk_ms FROM request_logs WHERE provider_id = ");
                sql.push_bind(provider).push(" AND ").push($model_column).push(" = ").push_bind(model);
                sql.push(" ORDER BY request_logs.created_at DESC, id DESC LIMIT 10");
                let rows = sql.build_query_as::<crate::db::model_performance::PerformanceSample>().fetch_all(&$this.pool).await?;
                result.push(crate::db::PairPerformanceStats {
                    provider_id: provider.clone(),
                    upstream_model: model.clone(),
                    mixed: crate::db::ModelPerformanceStats::from_samples(&rows),
                    ..Default::default()
                });
            }
            Ok(result)
    }};
}
pub(crate) use model_performance_method;

#[derive(sqlx::FromRow)]
pub(crate) struct HistoricalEffortRow {
    pub id: String,
    pub created_at: i64,
    pub upstream_protocol: Option<String>,
    pub bounded_body: Option<String>,
}

/// Migration-only recovery, bounded by both payload size and wall time. Version
/// guards make each row resumable on subsequent starts and never overwrite live evidence.
macro_rules! recover_historical_metadata {
    ($pool:expr, $database:ty, $byte_length:expr) => {{
        let pool = $pool;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let end = chrono::Utc::now().timestamp_millis();
        let start = end.saturating_sub(604_800_000);
        let mut cursor_at = start;
        let mut cursor_id = String::new();
        loop {
            if tokio::time::Instant::now() >= deadline { break; }
            let mut sql = sqlx::QueryBuilder::<$database>::new("SELECT id, created_at, upstream_protocol, CASE WHEN ");
            sql.push($byte_length).push(" <= 1048576 THEN upstream_request_body ELSE NULL END AS bounded_body FROM request_logs WHERE performance_metadata_version = 0 AND created_at BETWEEN ");
            sql.push_bind(start).push(" AND ").push_bind(end);
            sql.push(" AND (created_at > ").push_bind(cursor_at).push(" OR (created_at = ").push_bind(cursor_at).push(" AND id > ").push_bind(&cursor_id).push(")) ORDER BY created_at, id LIMIT 100");
            let rows = match tokio::time::timeout_at(deadline, sql.build_query_as::<crate::db::model_performance::HistoricalEffortRow>().fetch_all(pool)).await {
                Ok(result) => result?, Err(_) => break,
            };
            if rows.is_empty() { break; }
            for row in rows {
                if tokio::time::Instant::now() >= deadline { break; }
                let metadata = crate::performance::recover_historical_effort(row.bounded_body.as_deref().unwrap_or(""), row.upstream_protocol.as_deref());
                let mut update = sqlx::QueryBuilder::<$database>::new("UPDATE request_logs SET performance_metadata_version = 1, upstream_effort_status = ");
                update.push_bind(&metadata.effort_status).push(", upstream_effort_raw = ").push_bind(&metadata.effort_raw).push(", upstream_effort_tier = ").push_bind(&metadata.effort_tier);
                update.push(" WHERE performance_metadata_version = 0 AND id = ").push_bind(&row.id);
                match tokio::time::timeout_at(deadline, update.build().execute(pool)).await { Ok(result) => { result?; }, Err(_) => break }
                cursor_at = row.created_at;
                cursor_id = row.id;
            }
        }
    }};
}
pub(crate) use recover_historical_metadata;
