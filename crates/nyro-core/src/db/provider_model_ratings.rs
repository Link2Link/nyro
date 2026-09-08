use serde::{Deserialize, Serialize};
use sqlx::FromRow;

pub fn common_effort() -> String {
    "common".to_string()
}

pub const RATING_EFFORTS: [&str; 6] = ["common", "low", "medium", "high", "xhigh", "max"];

/// A provider-local rating for an exact upstream model and explicit scope.
/// No row means unrated; a stored score of zero is an explicit rating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ProviderModelRating {
    pub provider_id: String,
    pub upstream_model: String,
    #[serde(default = "common_effort")]
    pub effort: String,
    pub score: i32,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingValue {
    pub score: i32,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingOverrides {
    pub low: Option<RatingValue>,
    pub medium: Option<RatingValue>,
    pub high: Option<RatingValue>,
    pub xhigh: Option<RatingValue>,
    pub max: Option<RatingValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRating {
    pub status: String,
    pub score: Option<i32>,
    pub source: String,
    pub score_updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveRatings {
    pub low: ResolvedRating,
    pub medium: ResolvedRating,
    pub high: ResolvedRating,
    pub xhigh: ResolvedRating,
    pub max: ResolvedRating,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelRatingProfile {
    pub provider_id: String,
    pub upstream_model: String,
    pub common: Option<RatingValue>,
    pub overrides: RatingOverrides,
    pub effective: EffectiveRatings,
    pub display_mode: String,
}

impl ProviderModelRatingProfile {
    pub fn from_rows(provider_id: &str, model: &str, rows: &[ProviderModelRating]) -> Self {
        let value = |effort: &str| {
            rows.iter()
                .find(|r| {
                    r.provider_id == provider_id && r.upstream_model == model && r.effort == effort
                })
                .map(|r| RatingValue {
                    score: r.score,
                    updated_at: r.updated_at.clone(),
                })
        };
        let common = value("common");
        let overrides = RatingOverrides {
            low: value("low"),
            medium: value("medium"),
            high: value("high"),
            xhigh: value("xhigh"),
            max: value("max"),
        };
        let resolve = |own: &Option<RatingValue>| match own.as_ref().or(common.as_ref()) {
            Some(value) => ResolvedRating {
                status: "rated".to_string(),
                score: Some(value.score),
                source: if own.is_some() { "override" } else { "common" }.to_string(),
                score_updated_at: Some(value.updated_at.clone()),
            },
            None => ResolvedRating {
                status: "unrated".to_string(),
                score: None,
                source: "unrated".to_string(),
                score_updated_at: None,
            },
        };
        let effective = EffectiveRatings {
            low: resolve(&overrides.low),
            medium: resolve(&overrides.medium),
            high: resolve(&overrides.high),
            xhigh: resolve(&overrides.xhigh),
            max: resolve(&overrides.max),
        };
        let per_effort = [
            &overrides.low,
            &overrides.medium,
            &overrides.high,
            &overrides.xhigh,
            &overrides.max,
        ]
        .iter()
        .any(|value| value.is_some());
        Self {
            provider_id: provider_id.to_string(),
            upstream_model: model.to_string(),
            common,
            overrides,
            effective,
            display_mode: if per_effort { "per_effort" } else { "common" }.to_string(),
        }
    }
}

/// Prevent truncation on backends that can run in non-strict SQL modes.
/// The admin service owns the remaining name validation and timestamp creation.
pub(crate) fn ensure_model_byte_limit(model: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !model.is_empty() && model.len() <= 1024,
        "upstream model name must contain between 1 and 1024 UTF-8 bytes"
    );
    Ok(())
}

pub(crate) fn ensure_rating_scope(rating: &ProviderModelRating) -> anyhow::Result<()> {
    ensure_model_byte_limit(&rating.upstream_model)?;
    anyhow::ensure!(
        RATING_EFFORTS.contains(&rating.effort.as_str()),
        "Invalid rating effort"
    );
    anyhow::ensure!(
        (0..=100).contains(&rating.score),
        "Rating score must be between 0 and 100"
    );
    Ok(())
}
