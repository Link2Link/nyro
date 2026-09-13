//! Performance-page presentation of the same retained-call TPS used by model usage.

use super::*;
use crate::db::model_performance::ModelPerformanceStats;
use crate::db::model_rating_prefixes::longest_matching_entry;

/// One concrete upstream model variant inside a prefix×provider group.
#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceVariant {
    pub upstream_model: String,
    pub mixed: ModelPerformanceStats,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceItem {
    pub model_prefix: String,
    pub provider_id: String,
    pub score: i32,
    pub score_updated_at: String,
    /// Sample-weighted aggregate across this provider's matching variants.
    pub mixed: ModelPerformanceStats,
    pub variants: Vec<ModelPerformanceVariant>,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceResponse {
    pub as_of: i64,
    /// No extra time window: use the latest retained calls, like model usage.
    pub window_start: Option<i64>,
    pub models: Vec<ModelPerformanceItem>,
}

/// Merge variants through the shared valid-sample sums: content/gross rates
/// are sum(tokens)/sum(elapsed) across the whole group, never an average of
/// per-variant rates. Counts are plain sums and sample timestamps span the
/// merged window. A group without any valid sample keeps a null rate instead
/// of a fabricated zero; a group of only pure-reasoning samples keeps 0.
fn aggregate_stats(variants: &[ModelPerformanceVariant]) -> (ModelPerformanceStats, i64, i64) {
    let mut stats = ModelPerformanceStats::default();
    let mut unclassified_count = 0i64;
    let mut untrusted_count = 0i64;
    let mut totals = crate::db::tps::TpsTotals::default();
    for variant in variants {
        stats.selected_request_count += variant.mixed.selected_request_count;
        stats.valid_tps_count += variant.mixed.valid_tps_count;
        totals.absorb(&variant.mixed.tps_totals());
        unclassified_count += variant.unclassified_count;
        untrusted_count += variant.untrusted_count;
    }
    // Independent min/max passes keep first/last semantics regardless of order.
    stats.first_sample_at = variants
        .iter()
        .filter_map(|v| v.mixed.first_sample_at)
        .min();
    stats.last_sample_at = variants.iter().filter_map(|v| v.mixed.last_sample_at).max();
    stats.apply_totals(&totals);
    (stats, unclassified_count, untrusted_count)
}

impl AdminService {
    pub async fn get_model_performance(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<ModelPerformanceResponse> {
        // Only prefix-matched logged pairs need statistics. Reuse the model-usage
        // sampling and TPS rules; as_of is response time, not a time filter.
        let entries = self.list_model_ratings().await?;
        let as_of = Utc::now().timestamp_millis();
        let mut models = Vec::new();
        if !entries.is_empty() {
            // Group logged pairs by (matched prefix, provider); one point each.
            let mut grouped: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
            for (logged_provider, logged_model) in
                self.gw.storage.logs().distinct_logged_pairs().await?
            {
                if let Some(provider_id) = provider_id
                    && logged_provider != provider_id
                {
                    continue;
                }
                let Some(entry) = longest_matching_entry(&logged_model, &entries) else {
                    continue;
                };
                grouped
                    .entry((entry.model_prefix.clone(), logged_provider.clone()))
                    .or_default()
                    .push((logged_provider, logged_model));
            }
            let pairs = grouped
                .values()
                .flatten()
                .map(|(provider, model)| (provider.clone(), model.clone()))
                .collect::<Vec<_>>();
            let mut by_pair = HashMap::new();
            if !pairs.is_empty() {
                for stats in self
                    .gw
                    .storage
                    .logs()
                    .model_performance_stats(&pairs, as_of)
                    .await?
                {
                    by_pair.insert(
                        (stats.provider_id.clone(), stats.upstream_model.clone()),
                        stats,
                    );
                }
            }
            for ((model_prefix, provider), group_pairs) in grouped {
                let entry = entries
                    .iter()
                    .find(|entry| entry.model_prefix == model_prefix)
                    .expect("group key comes from an entry");
                let mut variants = Vec::with_capacity(group_pairs.len());
                for (provider, model) in &group_pairs {
                    // Pairs come from the same retained logs the statistics read,
                    // so a missing row is a storage contract failure.
                    if let Some(stats) = by_pair.remove(&(provider.clone(), model.clone())) {
                        variants.push(ModelPerformanceVariant {
                            upstream_model: model.clone(),
                            mixed: stats.mixed,
                            unclassified_count: stats.unclassified_count,
                            untrusted_count: stats.untrusted_count,
                        });
                    }
                }
                variants.sort_by(|a, b| a.upstream_model.cmp(&b.upstream_model));
                let (mixed, unclassified_count, untrusted_count) = aggregate_stats(&variants);
                models.push(ModelPerformanceItem {
                    model_prefix,
                    provider_id: provider,
                    score: entry.score,
                    score_updated_at: entry.updated_at.clone(),
                    mixed,
                    unclassified_count,
                    untrusted_count,
                    variants,
                });
            }
            models.sort_by(|a, b| {
                a.model_prefix
                    .cmp(&b.model_prefix)
                    .then_with(|| a.provider_id.cmp(&b.provider_id))
            });
        }
        Ok(ModelPerformanceResponse {
            as_of,
            window_start: None,
            models,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a variant from explicit shared-pool sums so merged expectations
    /// stay exact.
    fn variant_from_sums(
        model: &str,
        selected: i64,
        valid: i64,
        totals: crate::db::tps::TpsTotals,
        first: Option<i64>,
        last: Option<i64>,
    ) -> ModelPerformanceVariant {
        let mut mixed = ModelPerformanceStats {
            selected_request_count: selected,
            valid_tps_count: valid,
            ..Default::default()
        };
        mixed.apply_totals(&totals);
        mixed.first_sample_at = first;
        mixed.last_sample_at = last;
        ModelPerformanceVariant {
            upstream_model: model.to_string(),
            mixed,
            unclassified_count: 0,
            untrusted_count: 0,
        }
    }

    #[test]
    fn merges_variants_through_shared_sums() {
        // variant A: 2000 content / 3000 output over 20 s; B: 1000 / 1000
        // over 10 s. Merged: 3000 / 4000 over 30 s — sum ratios, not a mean
        // of per-variant rates (which would weight 100 and 100 equally).
        let variants = [
            variant_from_sums(
                "m",
                30,
                30,
                crate::db::tps::TpsTotals {
                    tps_content_tokens: 2_000,
                    tps_output_tokens: 3_000,
                    tps_elapsed_ms: 20_000,
                },
                Some(10),
                Some(90),
            ),
            variant_from_sums(
                "m-0813",
                10,
                10,
                crate::db::tps::TpsTotals {
                    tps_content_tokens: 1_000,
                    tps_output_tokens: 1_000,
                    tps_elapsed_ms: 10_000,
                },
                Some(20),
                Some(80),
            ),
        ];
        let (stats, _, _) = aggregate_stats(&variants);
        assert_eq!(stats.selected_request_count, 40);
        assert_eq!(stats.valid_tps_count, 40);
        assert_eq!(stats.total_content_tokens, 3_000);
        assert_eq!(stats.total_output_tokens, 4_000);
        assert_eq!(stats.total_latency_ms, 30_000);
        assert_eq!(stats.average_tps, Some(100.0));
        assert_eq!(stats.overall_tps, stats.average_tps);
        assert!((stats.average_gross_tps.unwrap() - (4_000.0 / 30.0)).abs() < 1e-9);
        assert_eq!(stats.overall_gross_tps, stats.average_gross_tps);
        assert_eq!(stats.first_sample_at, Some(10));
        assert_eq!(stats.last_sample_at, Some(90));
    }

    #[test]
    fn latency_weighting_beats_rate_averaging() {
        // One fast variant (100 tok / 0.1 s) and one slow variant (1000 tok /
        // 10 s): merged content TPS is 1100 / 10.1 s, not (1000 + 100) / 2.
        let variants = [
            variant_from_sums(
                "fast",
                1,
                1,
                crate::db::tps::TpsTotals {
                    tps_content_tokens: 100,
                    tps_output_tokens: 100,
                    tps_elapsed_ms: 100,
                },
                None,
                None,
            ),
            variant_from_sums(
                "slow",
                1,
                1,
                crate::db::tps::TpsTotals {
                    tps_content_tokens: 1_000,
                    tps_output_tokens: 1_000,
                    tps_elapsed_ms: 10_000,
                },
                None,
                None,
            ),
        ];
        let (stats, _, _) = aggregate_stats(&variants);
        assert!((stats.average_tps.unwrap() - (1_100.0 / 10.1)).abs() < 1e-9);
    }

    #[test]
    fn pure_reasoning_group_keeps_zero_content_rate() {
        let variants = [variant_from_sums(
            "thinker",
            5,
            5,
            crate::db::tps::TpsTotals {
                tps_content_tokens: 0,
                tps_output_tokens: 500,
                tps_elapsed_ms: 5_000,
            },
            Some(1),
            Some(9),
        )];
        let (stats, _, _) = aggregate_stats(&variants);
        assert_eq!(stats.valid_tps_count, 5);
        assert_eq!(stats.average_tps, Some(0.0));
        assert_eq!(stats.overall_tps, Some(0.0));
        assert_eq!(stats.average_gross_tps, Some(100.0));
        assert_eq!(stats.first_sample_at, Some(1));
        assert_eq!(stats.last_sample_at, Some(9));
    }

    #[test]
    fn no_valid_samples_keeps_null_average() {
        let variants = [variant_from_sums(
            "m",
            5,
            0,
            crate::db::tps::TpsTotals::default(),
            None,
            None,
        )];
        let (stats, _, _) = aggregate_stats(&variants);
        assert_eq!(stats.selected_request_count, 5);
        assert_eq!(stats.valid_tps_count, 0);
        assert_eq!(stats.average_tps, None);
        assert_eq!(stats.average_gross_tps, None);
        assert_eq!(stats.overall_tps, None);
        assert_eq!(stats.overall_gross_tps, None);
        assert_eq!(stats.first_sample_at, None);
        assert_eq!(stats.last_sample_at, None);
    }
}
