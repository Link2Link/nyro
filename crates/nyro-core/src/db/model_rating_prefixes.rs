use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// One comprehensive rating shared by every upstream model whose name equals the
/// prefix or continues after a "-" segment boundary. No row means unrated; a
/// stored score of zero is an explicit rating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
pub struct ModelRatingEntry {
    pub model_prefix: String,
    pub score: i32,
    pub updated_at: String,
}

/// Case-insensitive segment-boundary prefix match: the model either equals the
/// prefix or the prefix ends exactly at a "-" boundary, comparing both sides
/// lowercased. Segment identity is still exact, so "gpt-4" does not match
/// "gpt-4o" and "deepseek-v4-pro" does not match "deepseek-v4-pro2".
pub fn model_matches_prefix(model: &str, prefix: &str) -> bool {
    let model = model.to_lowercase();
    let prefix = prefix.to_lowercase();
    if model.len() <= prefix.len() {
        return model == prefix;
    }
    // `model` starts with the whole UTF-8 `prefix`, so `prefix.len()` is always
    // a char boundary of `model` and indexing that byte cannot panic.
    model.starts_with(&prefix) && model.as_bytes()[prefix.len()] == b'-'
}

/// Longest matching entry wins when several prefixes hit one model; lengths are
/// compared on the lowercased form so case variants never tie.
pub fn longest_matching_entry<'a>(
    model: &str,
    entries: &'a [ModelRatingEntry],
) -> Option<&'a ModelRatingEntry> {
    entries
        .iter()
        .filter(|entry| model_matches_prefix(model, &entry.model_prefix))
        .max_by_key(|entry| entry.model_prefix.to_lowercase().len())
}

/// Canonical storage key: prefixes are stored lowercased so the case-insensitive
/// match can never see two same-length variants of one prefix.
pub fn canonical_model_prefix(prefix: &str) -> String {
    prefix.to_lowercase()
}

/// Prevent truncation on backends that can run in non-strict SQL modes.
/// The admin service owns the remaining prefix validation and timestamps.
pub(crate) fn ensure_prefix_byte_limit(model_prefix: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !model_prefix.is_empty() && model_prefix.len() <= 1024,
        "model rating prefix must contain between 1 and 1024 UTF-8 bytes"
    );
    Ok(())
}

pub(crate) fn ensure_rating_entry(entry: &ModelRatingEntry) -> anyhow::Result<()> {
    ensure_prefix_byte_limit(&entry.model_prefix)?;
    anyhow::ensure!(
        (0..=100).contains(&entry.score),
        "Rating score must be between 0 and 100"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prefix: &str) -> ModelRatingEntry {
        ModelRatingEntry {
            model_prefix: prefix.to_string(),
            score: 50,
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn matches_only_at_segment_boundaries() {
        assert!(model_matches_prefix("deepseek-v4-pro", "deepseek-v4-pro"));
        assert!(model_matches_prefix(
            "deepseek-v4-pro-0813",
            "deepseek-v4-pro"
        ));
        assert!(!model_matches_prefix("deepseek-v4-pro2", "deepseek-v4-pro"));
        assert!(!model_matches_prefix("gpt-4o", "gpt-4"));
        assert!(model_matches_prefix("gpt-4-turbo", "gpt-4"));
        assert!(!model_matches_prefix("deepseek-v4", "deepseek-v4-pro"));
        assert!(!model_matches_prefix("", "deepseek"));
        // Case differences are ignored in both directions.
        assert!(model_matches_prefix("DeepSeek-V4-Pro", "deepseek-v4-pro"));
        assert!(model_matches_prefix(
            "deepseek-v4-pro-0813",
            "DEEPSEEK-V4-PRO"
        ));
        assert!(model_matches_prefix("GPT-4-TURBO", "gpt-4"));
        assert!(!model_matches_prefix("GPT-4O", "gpt-4"));
        // Separator identity is still exact.
        assert!(!model_matches_prefix("deepseek v4 pro", "deepseek-v4"));
        assert!(model_matches_prefix("a--b", "a-"));
        assert!(!model_matches_prefix("a-b", "a-"));
    }

    #[test]
    fn canonical_prefix_is_lowercase() {
        assert_eq!(canonical_model_prefix("DeepSeek-V4-Pro"), "deepseek-v4-pro");
        assert_eq!(canonical_model_prefix("模型-É"), "模型-é");
        assert_eq!(canonical_model_prefix("already-lower"), "already-lower");
    }

    #[test]
    fn longest_prefix_wins_overlaps() {
        let entries = vec![entry("deepseek-v4"), entry("deepseek-v4-pro")];
        assert_eq!(
            longest_matching_entry("deepseek-v4-pro-0813", &entries)
                .map(|found| found.model_prefix.as_str()),
            Some("deepseek-v4-pro")
        );
        assert_eq!(
            longest_matching_entry("deepseek-v4-chat", &entries)
                .map(|found| found.model_prefix.as_str()),
            Some("deepseek-v4")
        );
        // Case variants of the model name resolve to the same entry.
        assert_eq!(
            longest_matching_entry("DeepSeek-V4-PRO-0813", &entries)
                .map(|found| found.model_prefix.as_str()),
            Some("deepseek-v4-pro")
        );
        assert_eq!(longest_matching_entry("glm-4.5", &entries), None);
        assert_eq!(longest_matching_entry("", &entries), None);
    }

    #[test]
    fn unicode_boundaries_are_safe() {
        assert!(model_matches_prefix("模型-专业-0813", "模型-专业"));
        assert!(!model_matches_prefix("模型专业", "模型-专"));
        // Multi-byte prefix boundary: byte at prefix.len() is the '-' ASCII byte.
        assert!(model_matches_prefix("模型-x", "模型"));
    }

    #[test]
    fn entry_validation_bounds_bytes_and_score() {
        ensure_rating_entry(&entry("模型")).unwrap();
        assert!(ensure_rating_entry(&entry("")).is_err());
        assert!(ensure_rating_entry(&entry(&"é".repeat(513))).is_err());
        let mut zero = entry("m");
        zero.score = 0;
        ensure_rating_entry(&zero).unwrap();
        zero.score = 101;
        assert!(ensure_rating_entry(&zero).is_err());
        zero.score = -1;
        assert!(ensure_rating_entry(&zero).is_err());
    }
}
