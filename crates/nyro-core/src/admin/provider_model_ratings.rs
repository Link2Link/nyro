//! Manual capability ratings are administration data, not routing inputs.
//!
//! A missing row is unrated, a stored zero is rated, and a failed lookup is an
//! error. Never consult upstream catalogs or refresh routing state here.

use super::*;
use crate::storage::traits::ProviderModelRatingStore;

pub const MAX_RATING_MODEL_BYTES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetProviderModelRating {
    pub score: i32,
}

/// Explicit wire-level absence, with impossible rated/null combinations excluded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderModelRatingState {
    Rated(ProviderModelRating),
    Unrated {
        provider_id: String,
        upstream_model: String,
        score: (),
        updated_at: (),
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderModelRatingError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Provider not found")]
    ProviderNotFound,
    #[error("Persistent model ratings are not supported by this storage backend")]
    UnsupportedStorage,
}

pub(super) fn validate_rating_model(model: &str) -> anyhow::Result<()> {
    let message = if model.trim().is_empty() {
        Some("Model name cannot be blank")
    } else if model.contains('\0') {
        Some("Model name cannot contain NUL")
    } else if model.len() > MAX_RATING_MODEL_BYTES {
        Some("Model name cannot exceed 1024 UTF-8 bytes")
    } else {
        None
    };
    if let Some(message) = message {
        return Err(ProviderModelRatingError::InvalidInput(message.to_string()).into());
    }
    Ok(())
}

pub(super) fn validate_rating_score(score: i32) -> anyhow::Result<()> {
    if !(0..=100).contains(&score) {
        return Err(ProviderModelRatingError::InvalidInput(
            "Score must be an integer between 0 and 100".to_string(),
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_export_ratings(ratings: &[ExportProviderModelRating]) -> anyhow::Result<()> {
    let mut models = std::collections::HashSet::new();
    for rating in ratings {
        validate_rating_model(&rating.upstream_model)?;
        validate_rating_score(rating.score)?;
        if !models.insert(&rating.upstream_model) {
            return Err(ProviderModelRatingError::InvalidInput(format!(
                "Duplicate model rating: {}",
                rating.upstream_model
            ))
            .into());
        }
        DateTime::parse_from_rfc3339(&rating.updated_at).map_err(|_| {
            ProviderModelRatingError::InvalidInput(format!(
                "Rating updated_at must be an RFC3339 timestamp: {}",
                rating.upstream_model
            ))
        })?;
    }
    Ok(())
}

impl AdminService {
    pub(super) fn rating_store(&self) -> anyhow::Result<&dyn ProviderModelRatingStore> {
        self.gw
            .storage
            .provider_model_ratings()
            .ok_or_else(|| ProviderModelRatingError::UnsupportedStorage.into())
    }

    async fn ensure_rating_provider(&self, provider_id: &str) -> anyhow::Result<()> {
        match self.gw.storage.providers().get(provider_id).await? {
            // MySQL's provider FK uses its parent's case-insensitive collation.
            // Never let a differently cased UUID create a rating whose returned
            // identity cannot join the actual provider in other consumers.
            Some(provider) if provider.id == provider_id => Ok(()),
            _ => Err(ProviderModelRatingError::ProviderNotFound.into()),
        }
    }

    pub async fn list_provider_model_ratings(
        &self,
        provider_id: Option<&str>,
    ) -> anyhow::Result<Vec<ProviderModelRating>> {
        let store = self.rating_store()?;
        if let Some(id) = provider_id {
            self.ensure_rating_provider(id).await?;
        }
        let mut ratings = store.list(provider_id).await?;
        ratings.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then_with(|| a.upstream_model.cmp(&b.upstream_model))
        });
        Ok(ratings)
    }

    pub async fn get_provider_model_rating(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<ProviderModelRatingState> {
        validate_rating_model(model)?;
        let store = self.rating_store()?;
        self.ensure_rating_provider(provider_id).await?;
        Ok(match store.get(provider_id, model).await? {
            Some(rating) => ProviderModelRatingState::Rated(rating),
            None => ProviderModelRatingState::Unrated {
                provider_id: provider_id.to_string(),
                upstream_model: model.to_string(),
                score: (),
                updated_at: (),
            },
        })
    }

    pub async fn set_provider_model_rating(
        &self,
        provider_id: &str,
        model: &str,
        input: SetProviderModelRating,
    ) -> anyhow::Result<ProviderModelRating> {
        validate_rating_model(model)?;
        validate_rating_score(input.score)?;
        let store = self.rating_store()?;
        self.ensure_rating_provider(provider_id).await?;
        store
            .upsert(ProviderModelRating {
                provider_id: provider_id.to_string(),
                upstream_model: model.to_string(),
                score: input.score,
                updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            })
            .await
    }

    pub async fn delete_provider_model_rating(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        validate_rating_model(model)?;
        let store = self.rating_store()?;
        self.ensure_rating_provider(provider_id).await?;
        store.delete(provider_id, model).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_does_not_coerce_or_accept_client_timestamps() {
        for json in [
            "{}",
            "{\"score\":null}",
            "{\"score\":1.5}",
            "{\"score\":\"85\"}",
            "{\"score\":true}",
            "{\"score\":85,\"updated_at\":\"2020-01-01T00:00:00Z\"}",
        ] {
            assert!(
                serde_json::from_str::<SetProviderModelRating>(json).is_err(),
                "{json}"
            );
        }
        assert_eq!(
            serde_json::from_str::<SetProviderModelRating>("{\"score\":0}")
                .unwrap()
                .score,
            0
        );
    }

    #[test]
    fn model_validation_preserves_identity_and_bounds_utf8_bytes() {
        for model in [" model/X ", "MODEL-x", "model-x", "模型-é", "a\tb"] {
            validate_rating_model(model).unwrap();
        }
        for model in ["", " \t\n", "a\0b"] {
            assert!(validate_rating_model(model).is_err());
        }
        validate_rating_model(&"é".repeat(512)).unwrap();
        assert!(validate_rating_model(&"é".repeat(513)).is_err());
    }
}
