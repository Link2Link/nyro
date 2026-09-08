//! Completion-aware performance samples, separate from the legacy usage metric.
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
pub struct ModelPerformanceTiers {
    pub low: ModelPerformanceStats,
    pub medium: ModelPerformanceStats,
    pub high: ModelPerformanceStats,
    pub xhigh: ModelPerformanceStats,
    pub max: ModelPerformanceStats,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PairPerformanceStats {
    pub provider_id: String,
    pub upstream_model: String,
    pub mixed: ModelPerformanceStats,
    pub tiers: ModelPerformanceTiers,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct PerformanceSample {
    pub group_name: Option<String>,
    pub output_tokens: Option<i64>,
    pub upstream_response_mode: Option<String>,
    pub performance_upstream_ms: Option<i64>,
    pub performance_first_chunk_ms: Option<i64>,
    pub performance_completed_at: Option<i64>,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
}

impl PairPerformanceStats {
    pub(crate) fn add_sample(&mut self, row: PerformanceSample) {
        self.unclassified_count = row.unclassified_count;
        self.untrusted_count = row.untrusted_count;
        let stats = match row.group_name.as_deref() {
            Some("mixed") => &mut self.mixed,
            Some("low") => &mut self.tiers.low,
            Some("medium") => &mut self.tiers.medium,
            Some("high") => &mut self.tiers.high,
            Some("xhigh") => &mut self.tiers.xhigh,
            Some("max") => &mut self.tiers.max,
            _ => return,
        };
        stats.selected_request_count += 1;
        let Some(tokens) = row.output_tokens.filter(|n| *n > 0) else {
            return;
        };
        let Some(upstream) = row.performance_upstream_ms.filter(|n| *n > 0) else {
            return;
        };
        let duration = match row.upstream_response_mode.as_deref() {
            Some("buffered") => upstream,
            Some("stream") => {
                let Some(first) = row
                    .performance_first_chunk_ms
                    .filter(|n| *n >= 0 && *n <= upstream)
                else {
                    return;
                };
                let generation = upstream - first;
                if generation < 50 || first as f64 / upstream as f64 >= 0.8 {
                    upstream
                } else {
                    generation
                }
            }
            _ => return,
        };
        let tps = tokens as f64 * 1000.0 / duration as f64;
        if !tps.is_finite() || tps <= 0.0 {
            return;
        }
        let Some(at) = row.performance_completed_at else {
            return;
        };
        stats.valid_tps_count += 1;
        let n = stats.valid_tps_count as f64;
        stats.average_tps = Some(stats.average_tps.unwrap_or(0.0) * ((n - 1.0) / n) + tps / n);
        stats.first_sample_at = Some(stats.first_sample_at.map_or(at, |old| old.min(at)));
        stats.last_sample_at = Some(stats.last_sample_at.map_or(at, |old| old.max(at)));
    }
}

// Shared SQL and reduction guarantee identical backend semantics. Each pair uses
// one narrow scalar-only window query (never request/response payload columns).
macro_rules! model_performance_method {
    ($this:expr, $pairs:expr, $as_of:expr, $database:ty, $model_column:expr, $token_cast:expr) => {{
            let pairs = $pairs;
            let as_of = $as_of;
            let start = as_of.saturating_sub(604_800_000);
            let mut result = Vec::with_capacity(pairs.len());
            for (provider, model) in pairs {
                let mut sql = sqlx::QueryBuilder::<$database>::new("WITH eligible AS (SELECT id, upstream_effort_status, upstream_effort_tier, ");
                sql.push($token_cast).push(" AS output_tokens, upstream_response_mode, performance_upstream_ms, performance_first_chunk_ms, performance_completed_at FROM request_logs WHERE provider_id = ");
                sql.push_bind(provider).push(" AND ").push($model_column).push(" = ").push_bind(model);
                sql.push(" AND performance_metadata_version > 0 AND request_completion = 'completed' AND upstream_status_code BETWEEN 200 AND 299 AND client_status_code BETWEEN 200 AND 299 AND performance_completed_at BETWEEN ");
                sql.push_bind(start).push(" AND ").push_bind(as_of);
                sql.push("), grouped AS (SELECT 'mixed' AS group_name, eligible.* FROM eligible UNION ALL SELECT upstream_effort_tier AS group_name, eligible.* FROM eligible WHERE upstream_effort_status = 'present' AND upstream_effort_tier IN ('low','medium','high','xhigh','max')), ranked AS (SELECT grouped.*, ROW_NUMBER() OVER (PARTITION BY group_name ORDER BY performance_completed_at DESC, id DESC) AS rn FROM grouped) SELECT r.group_name, r.output_tokens, r.upstream_response_mode, r.performance_upstream_ms, r.performance_first_chunk_ms, r.performance_completed_at, (SELECT COUNT(*) FROM eligible WHERE upstream_effort_status <> 'present' OR upstream_effort_tier IS NULL OR upstream_effort_tier NOT IN ('low','medium','high','xhigh','max')) AS unclassified_count, (SELECT COUNT(*) FROM request_logs WHERE provider_id = ");
                sql.push_bind(provider).push(" AND ").push($model_column).push(" = ").push_bind(model);
                sql.push(" AND request_completion = 'unknown' AND created_at BETWEEN ").push_bind(start).push(" AND ").push_bind(as_of);
                sql.push(") AS untrusted_count FROM (SELECT 1 AS anchor) a LEFT JOIN ranked r ON r.rn <= 10");
                let rows = sql.build_query_as::<crate::db::model_performance::PerformanceSample>().fetch_all(&$this.pool).await?;
                let mut pair = crate::db::PairPerformanceStats { provider_id: provider.clone(), upstream_model: model.clone(), ..Default::default() };
                for row in rows { pair.add_sample(row); }
                result.push(pair);
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
