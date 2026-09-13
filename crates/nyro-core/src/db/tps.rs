//! Shared end-to-end TPS sums for every aggregation surface.
//!
//! One validity pool everywhere (single log row, latest-fifty window, and the
//! windowed SQL aggregates): `output_tokens > 0` and an end-to-end latency
//! `COALESCE(latency_upstream_ms, latency_total_ms) > 0`. Upstream latency
//! only falls back to the total when it is NULL; a present-but-nonpositive
//! upstream value makes the row invalid. Content tokens clamp reasoning into
//! `[0, output_tokens]` (negative reasoning -> 0, reasoning above output ->
//! zero content). TTFT is never subtracted.

use serde::{Deserialize, Serialize};

use super::models::RecentModelPerformance;

/// Flat sum totals over the shared valid-sample pool. Field names double as
/// the SQL aliases and JSON keys so aggregates stay directly readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TpsTotals {
    #[serde(default)]
    pub tps_content_tokens: i64,
    #[serde(default)]
    pub tps_output_tokens: i64,
    #[serde(default)]
    pub tps_elapsed_ms: i64,
}

impl TpsTotals {
    /// Content TPS = sum(content) / sum(elapsed). `None` when the valid pool
    /// is empty; `Some(0.0)` when every valid sample is pure reasoning.
    pub fn content_tps(&self) -> Option<f64> {
        self.ratio(self.tps_content_tokens)
    }

    /// Gross TPS = sum(output) / sum(elapsed) over the same pool.
    pub fn gross_tps(&self) -> Option<f64> {
        self.ratio(self.tps_output_tokens)
    }

    fn ratio(&self, tokens: i64) -> Option<f64> {
        (self.tps_elapsed_ms > 0)
            .then(|| tokens as f64 / (self.tps_elapsed_ms as f64 / 1000.0))
    }

    /// Fold one retained-log sample into the sums. Returns whether the sample
    /// joined the valid pool.
    pub(crate) fn push(&mut self, sample: &RecentModelPerformance) -> bool {
        if !sample.has_tps_usage() {
            return false;
        }
        let Some(elapsed) = sample.e2e_latency_ms() else {
            return false;
        };
        self.tps_content_tokens += sample.content_tokens() as i64;
        self.tps_output_tokens += sample.output_tokens as i64;
        self.tps_elapsed_ms += elapsed;
        true
    }

    /// Merge another totals set (used when folding per-variant aggregates).
    pub(crate) fn absorb(&mut self, other: &TpsTotals) {
        self.tps_content_tokens += other.tps_content_tokens;
        self.tps_output_tokens += other.tps_output_tokens;
        self.tps_elapsed_ms += other.tps_elapsed_ms;
    }

    /// Whether the valid pool is non-empty (sum(elapsed) > 0 implies at least
    /// one sample, because every valid sample contributes elapsed > 0).
    pub fn has_valid_samples(&self) -> bool {
        self.tps_elapsed_ms > 0
    }
}

/// Backend-specific SQL spelling for the shared aggregate columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TpsSqlDialect {
    Sqlite,
    Postgres,
    Mysql,
}

/// The three shared SUM(CASE ...) columns, comma separated without a leading
/// comma:
///
/// - `tps_content_tokens`: clamped content tokens of valid samples
/// - `tps_output_tokens`: raw output tokens of valid samples
/// - `tps_elapsed_ms`: end-to-end latency of valid samples
///
/// Validity matches [`RecentModelPerformance::tps`]: `output_tokens > 0` and
/// `COALESCE(latency_upstream_ms, latency_total_ms) > 0`. Casts keep sqlx
/// decoding to i64 stable (MySQL SUM yields DECIMAL, Postgres SUM can yield
/// numeric); SQLite spells GREATEST/LEAST as the scalar MAX/MIN.
pub(crate) fn tps_totals_columns(dialect: TpsSqlDialect) -> String {
    let (greatest, least, cast) = match dialect {
        TpsSqlDialect::Sqlite => ("MAX", "MIN", "CAST({sum} AS INTEGER)"),
        TpsSqlDialect::Postgres => ("GREATEST", "LEAST", "{sum}::BIGINT"),
        TpsSqlDialect::Mysql => ("GREATEST", "LEAST", "CAST({sum} AS SIGNED)"),
    };
    let cond = "output_tokens > 0 AND COALESCE(latency_upstream_ms, latency_total_ms) > 0";
    let content_expr = format!(
        "{greatest}(output_tokens - {least}({greatest}(COALESCE(reasoning_tokens, 0), 0), output_tokens), 0)"
    );
    let sum = |then: String| {
        cast.replace(
            "{sum}",
            &format!("COALESCE(SUM(CASE WHEN {cond} THEN {then} ELSE 0 END), 0)"),
        )
    };
    format!(
        "{content} AS tps_content_tokens, {output} AS tps_output_tokens, {elapsed} AS tps_elapsed_ms",
        content = sum(content_expr),
        output = sum("output_tokens".to_string()),
        elapsed = sum("COALESCE(latency_upstream_ms, latency_total_ms)".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        output: i32,
        reasoning: i32,
        upstream: Option<i64>,
        total: Option<i64>,
    ) -> RecentModelPerformance {
        RecentModelPerformance {
            output_tokens: output,
            reasoning_tokens: reasoning,
            is_stream: true,
            stream_chunks_count: 2,
            latency_upstream_ms: upstream,
            latency_total_ms: total,
            stream_first_chunk_ms: Some(100),
        }
    }

    #[test]
    fn push_accumulates_only_valid_samples() {
        let mut totals = TpsTotals::default();
        assert!(totals.push(&sample(100, 80, Some(2000), Some(2100))));
        assert!(totals.push(&sample(100, 100, None, Some(1000)))); // pure reasoning
        assert!(!totals.push(&sample(0, 0, Some(1000), None)));
        assert!(!totals.push(&sample(100, 0, Some(0), Some(5000))));
        assert!(!totals.push(&sample(100, 0, None, None)));
        assert_eq!(
            totals,
            TpsTotals {
                tps_content_tokens: 20,
                tps_output_tokens: 200,
                tps_elapsed_ms: 3000,
            }
        );
        assert!((totals.content_tps().unwrap() - (20.0 / 3.0)).abs() < 1e-9);
        assert!((totals.gross_tps().unwrap() - (200.0 / 3.0)).abs() < 1e-9);
        assert!(totals.has_valid_samples());
    }

    #[test]
    fn pure_reasoning_pool_reports_zero_content_tps() {
        let mut totals = TpsTotals::default();
        assert!(totals.push(&sample(100, 100, Some(1000), None)));
        assert_eq!(totals.content_tps(), Some(0.0));
        assert_eq!(totals.gross_tps(), Some(100.0));
    }

    #[test]
    fn empty_pool_has_no_rates() {
        let totals = TpsTotals::default();
        assert_eq!(totals.content_tps(), None);
        assert_eq!(totals.gross_tps(), None);
        assert!(!totals.has_valid_samples());
    }

    #[test]
    fn absorb_merges_totals() {
        let mut a = TpsTotals::default();
        assert!(a.push(&sample(100, 0, Some(1000), None)));
        let mut b = TpsTotals::default();
        assert!(b.push(&sample(300, 100, Some(2000), None)));
        a.absorb(&b);
        assert_eq!(
            a,
            TpsTotals {
                tps_content_tokens: 300,
                tps_output_tokens: 400,
                tps_elapsed_ms: 3000,
            }
        );
    }

    #[test]
    fn sql_columns_match_dialect_spelling() {
        let sqlite = tps_totals_columns(TpsSqlDialect::Sqlite);
        assert!(sqlite.contains(
            "MAX(output_tokens - MIN(MAX(COALESCE(reasoning_tokens, 0), 0), output_tokens), 0)"
        ));
        assert!(sqlite.contains("CAST(COALESCE(SUM(CASE"));
        assert_eq!(sqlite.matches(" AS tps_").count(), 3);

        let postgres = tps_totals_columns(TpsSqlDialect::Postgres);
        assert!(postgres.contains(
            "GREATEST(output_tokens - LEAST(GREATEST(COALESCE(reasoning_tokens, 0), 0), output_tokens), 0)"
        ));
        assert!(postgres.contains("::BIGINT AS tps_content_tokens"));

        let mysql = tps_totals_columns(TpsSqlDialect::Mysql);
        assert!(mysql.contains("CAST(COALESCE(SUM(CASE"));
        assert!(mysql.contains("AS SIGNED) AS tps_elapsed_ms"));
        for sql in [sqlite, postgres, mysql] {
            assert!(sql.contains(
                "output_tokens > 0 AND COALESCE(latency_upstream_ms, latency_total_ms) > 0"
            ));
        }
    }
}
