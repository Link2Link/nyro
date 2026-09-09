//! Manual capability ratings are administration data, not routing inputs.
//!
//! Ratings are keyed by a user-chosen model prefix and shared by every provider
//! whose model names match it at a "-" segment boundary. A missing entry is
//! unrated, a stored zero is rated, and a failed lookup is an error. Never
//! consult upstream catalogs or refresh routing state here.

use super::*;
use crate::storage::traits::ModelRatingStore;

pub const MAX_RATING_PREFIX_BYTES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModelRating {
    pub score: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelRatingError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Persistent model ratings are not supported by this storage backend")]
    UnsupportedStorage,
}

pub(super) fn validate_rating_prefix(prefix: &str) -> anyhow::Result<()> {
    let message = if prefix.trim().is_empty() {
        Some("Rating prefix cannot be blank")
    } else if prefix.contains('\0') {
        Some("Rating prefix cannot contain NUL")
    } else if prefix.len() > MAX_RATING_PREFIX_BYTES {
        Some("Rating prefix cannot exceed 1024 UTF-8 bytes")
    } else {
        None
    };
    if let Some(message) = message {
        return Err(ModelRatingError::InvalidInput(message.to_string()).into());
    }
    Ok(())
}

pub(super) fn validate_rating_score(score: i32) -> anyhow::Result<()> {
    if !(0..=100).contains(&score) {
        return Err(ModelRatingError::InvalidInput(
            "Score must be an integer between 0 and 100".to_string(),
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_export_ratings(entries: &[ExportModelRating]) -> anyhow::Result<()> {
    let mut prefixes = std::collections::HashSet::new();
    for entry in entries {
        // Canonical form decides duplicates and bounds; case variants collide.
        let canonical = canonical_model_prefix(&entry.model_prefix);
        validate_rating_prefix(&canonical)?;
        validate_rating_score(entry.score)?;
        if !prefixes.insert(canonical) {
            return Err(ModelRatingError::InvalidInput(format!(
                "Duplicate model rating prefix: {}",
                entry.model_prefix
            ))
            .into());
        }
        DateTime::parse_from_rfc3339(&entry.updated_at).map_err(|_| {
            ModelRatingError::InvalidInput(format!(
                "Rating updated_at must be an RFC3339 timestamp: {}",
                entry.model_prefix
            ))
        })?;
    }
    Ok(())
}

impl AdminService {
    pub(super) fn rating_store(&self) -> anyhow::Result<&dyn ModelRatingStore> {
        self.gw
            .storage
            .model_ratings()
            .ok_or_else(|| ModelRatingError::UnsupportedStorage.into())
    }

    pub async fn list_model_ratings(&self) -> anyhow::Result<Vec<ModelRatingEntry>> {
        let mut entries = self.rating_store()?.list().await?;
        entries.sort_by(|a, b| a.model_prefix.cmp(&b.model_prefix));
        Ok(entries)
    }

    pub async fn set_model_rating(
        &self,
        model_prefix: &str,
        input: SetModelRating,
    ) -> anyhow::Result<ModelRatingEntry> {
        // Prefixes are stored lowercase: the case-insensitive match can never
        // see two same-length variants of one prefix.
        let canonical = canonical_model_prefix(model_prefix);
        validate_rating_prefix(&canonical)?;
        validate_rating_score(input.score)?;
        let store = self.rating_store()?;
        store
            .upsert(ModelRatingEntry {
                model_prefix: canonical,
                score: input.score,
                updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            })
            .await
    }

    pub async fn delete_model_rating(&self, model_prefix: &str) -> anyhow::Result<()> {
        let canonical = canonical_model_prefix(model_prefix);
        validate_rating_prefix(&canonical)?;
        self.rating_store()?.delete(&canonical).await
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
                serde_json::from_str::<SetModelRating>(json).is_err(),
                "{json}"
            );
        }
        assert_eq!(
            serde_json::from_str::<SetModelRating>("{\"score\":0}")
                .unwrap()
                .score,
            0
        );
    }

    #[test]
    fn prefix_validation_preserves_identity_and_bounds_utf8_bytes() {
        for prefix in [" deepseek-v4-pro ", "MODEL-x", "model-x", "模型-é", "a\tb"] {
            validate_rating_prefix(prefix).unwrap();
        }
        for prefix in ["", " \t\n", "a\0b"] {
            assert!(validate_rating_prefix(prefix).is_err());
        }
        validate_rating_prefix(&"é".repeat(512)).unwrap();
        assert!(validate_rating_prefix(&"é".repeat(513)).is_err());
    }

    #[test]
    fn export_validation_rejects_duplicates_and_bad_timestamps() {
        let entry = |prefix: &str, updated_at: &str| ExportModelRating {
            model_prefix: prefix.to_string(),
            score: 50,
            updated_at: updated_at.to_string(),
        };
        validate_export_ratings(&[entry("a", "2026-01-01T00:00:00Z")]).unwrap();
        validate_export_ratings(&[entry("A", "2026-01-01T00:00:00Z")]).unwrap();
        assert!(validate_export_ratings(&[
            entry("a", "2026-01-01T00:00:00Z"),
            entry("a", "2026-01-02T00:00:00Z")
        ])
        .is_err());
        // Case variants of one prefix are duplicates after canonicalization.
        assert!(validate_export_ratings(&[
            entry("Model", "2026-01-01T00:00:00Z"),
            entry("model", "2026-01-02T00:00:00Z")
        ])
        .is_err());
        assert!(validate_export_ratings(&[entry("a", "not-a-timestamp")]).is_err());
    }
}
