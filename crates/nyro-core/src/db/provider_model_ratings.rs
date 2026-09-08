use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A provider-local rating for an exact upstream model name.
///
/// No row means unrated; a stored score of zero is an explicit rating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ProviderModelRating {
    pub provider_id: String,
    pub upstream_model: String,
    pub score: i32,
    pub updated_at: String,
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
