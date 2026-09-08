//! Performance-page-only aggregation. Legacy usage endpoints keep their metrics.

use super::*;
use crate::db::model_performance::{ModelPerformanceStats, ModelPerformanceTiers};

const PERFORMANCE_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceItem {
    pub profile: ProviderModelRatingProfile,
    pub mixed: ModelPerformanceStats,
    pub tiers: ModelPerformanceTiers,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceResponse {
    pub as_of: i64,
    pub window_start: i64,
    pub models: Vec<ModelPerformanceItem>,
}

impl AdminService {
    pub async fn get_model_performance(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<ModelPerformanceResponse> {
        // One frozen profile/time snapshot drives both score resolution and the
        // grouping query. Empty saved profiles produce no performance requests.
        let profiles = self
            .list_provider_model_rating_profiles(provider_id)
            .await?;
        let as_of = Utc::now().timestamp_millis();
        let pairs = profiles
            .iter()
            .map(|profile| (profile.provider_id.clone(), profile.upstream_model.clone()))
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
        let models = profiles
            .into_iter()
            .map(|profile| {
                let stats =
                    by_pair.remove(&(profile.provider_id.clone(), profile.upstream_model.clone()));
                match stats {
                    Some(stats) => ModelPerformanceItem {
                        profile,
                        mixed: stats.mixed,
                        tiers: stats.tiers,
                        unclassified_count: stats.unclassified_count,
                        untrusted_count: stats.untrusted_count,
                        status: "ready".to_string(),
                        error: None,
                    },
                    // Missing rows are a storage contract failure, not a fabricated
                    // empty successful group. Valid peers can still be displayed.
                    None => ModelPerformanceItem {
                        profile,
                        mixed: ModelPerformanceStats::default(),
                        tiers: ModelPerformanceTiers::default(),
                        unclassified_count: 0,
                        untrusted_count: 0,
                        status: "error".to_string(),
                        error: Some(
                            "Performance statistics are unavailable for this model".to_string(),
                        ),
                    },
                }
            })
            .collect();
        Ok(ModelPerformanceResponse {
            as_of,
            window_start: as_of - PERFORMANCE_WINDOW_MS,
            models,
        })
    }
}
