//! Persisted, secret-free refresh retry barriers shared by every refresh entry point.

use chrono::{DateTime, Utc};
use serde_json::Value;

pub(crate) const REFRESH_RETRY_AT_KEY: &str = "nyro_refresh_retry_at";
pub(crate) const REFRESH_PUBLICATION_ID_KEY: &str = "nyro_refresh_publication_id";
pub(crate) const OUTCOME_UNKNOWN_MESSAGE: &str =
    "OAuth refresh outcome is unknown; sign in again before refreshing";

pub(crate) fn retry_at(meta: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(meta.get(REFRESH_RETRY_AT_KEY)?.as_str()?)
        .ok()
        .map(|deadline| deadline.with_timezone(&Utc))
}

pub(crate) fn ensure_refresh_allowed(meta: &Value, now: DateTime<Utc>) -> anyhow::Result<()> {
    anyhow::ensure!(
        !retry_at(meta).is_some_and(|deadline| deadline > now),
        "OAuth refresh is temporarily unavailable; try again later"
    );
    Ok(())
}

pub(crate) fn with_retry_at(meta: &Value, deadline: DateTime<Utc>) -> Value {
    let mut meta = meta.clone();
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    meta[REFRESH_RETRY_AT_KEY] = Value::String(deadline.to_rfc3339());
    meta
}

pub(crate) fn clear_retry_at(meta: &mut Value) {
    if let Some(meta) = meta.as_object_mut() {
        meta.remove(REFRESH_RETRY_AT_KEY);
    }
}

/// A new authorization cannot impersonate an earlier refresh publication receipt.
pub(crate) fn clear_publication_id(meta: &mut Value) {
    if let Some(meta) = meta.as_object_mut() {
        meta.remove(REFRESH_PUBLICATION_ID_KEY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_barrier_preserves_metadata_and_uses_strict_future_deadlines() {
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let original = serde_json::json!({"project_id": "project", "nested": {"keep": true}});
        let mut meta = with_retry_at(&original, now + chrono::Duration::seconds(30));
        assert_eq!(retry_at(&meta), Some(now + chrono::Duration::seconds(30)));
        assert_eq!(meta["project_id"], original["project_id"]);
        assert_eq!(meta["nested"], original["nested"]);
        assert_eq!(
            ensure_refresh_allowed(&meta, now).unwrap_err().to_string(),
            "OAuth refresh is temporarily unavailable; try again later"
        );
        assert!(ensure_refresh_allowed(&meta, now + chrono::Duration::seconds(30)).is_ok());
        clear_retry_at(&mut meta);
        assert_eq!(meta, original);
        assert!(ensure_refresh_allowed(&meta, now).is_ok());
    }

    #[test]
    fn invalid_or_missing_retry_deadlines_are_ignored() {
        for meta in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({"nyro_refresh_retry_at": "invalid"}),
            serde_json::json!({"nyro_refresh_retry_at": 7}),
        ] {
            assert!(retry_at(&meta).is_none());
            assert!(ensure_refresh_allowed(&meta, Utc::now()).is_ok());
        }
    }
}
