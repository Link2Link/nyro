//! Performance-page presentation of the same recent-call TPS used by model usage.

use super::*;
use crate::db::model_performance::ModelPerformanceStats;

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceItem {
    pub rating: ProviderModelRating,
    pub mixed: ModelPerformanceStats,
    pub unclassified_count: i64,
    pub untrusted_count: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelPerformanceResponse {
    pub as_of: i64,
    /// No extra time window: use the latest retained calls, like model usage.
    pub window_start: Option<i64>,
    pub models: Vec<ModelPerformanceItem>,
}

impl AdminService {
    pub async fn get_model_performance(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<ModelPerformanceResponse> {
        // Only explicitly rated pairs need statistics. Reuse the model-usage
        // sampling and TPS rules; as_of is response time, not a time filter.
        let ratings = self.list_provider_model_ratings(provider_id).await?;
        let as_of = Utc::now().timestamp_millis();
        let pairs = ratings
            .iter()
            .map(|rating| (rating.provider_id.clone(), rating.upstream_model.clone()))
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
        let models = ratings
            .into_iter()
            .map(|rating| {
                let stats =
                    by_pair.remove(&(rating.provider_id.clone(), rating.upstream_model.clone()));
                match stats {
                    Some(stats) => ModelPerformanceItem {
                        rating,
                        mixed: stats.mixed,
                        unclassified_count: stats.unclassified_count,
                        untrusted_count: stats.untrusted_count,
                        status: "ready".to_string(),
                        error: None,
                    },
                    // Missing rows are a storage contract failure, not a fabricated
                    // empty successful group. Valid peers can still be displayed.
                    None => ModelPerformanceItem {
                        rating,
                        mixed: ModelPerformanceStats::default(),
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
            window_start: None,
            models,
        })
    }
}
