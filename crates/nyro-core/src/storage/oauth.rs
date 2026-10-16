//! Shared OAuth storage invariants; no provider protocol or token handling here.

pub(super) const STALE_REFRESH_ERROR: &str =
    "refresh timeout: process did not complete within timeout";
pub(super) const BIND_ATTEMPTS: usize = 4;

pub(super) fn parse_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|time| time.and_utc())
        })
}

// MySQL DATETIME does not accept RFC3339 on all supported server versions.
pub(super) fn mysql_expiry(value: Option<&str>) -> anyhow::Result<Option<String>> {
    value
        .map(|value| {
            parse_timestamp(value)
                .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                .ok_or_else(|| anyhow::anyhow!("invalid OAuth expiry timestamp"))
        })
        .transpose()
}

pub(super) fn retryable_bind_error(error: &anyhow::Error) -> bool {
    let Some(sqlx::Error::Database(error)) = error.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    if let Some(mysql) = error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        // DatabaseError::code is SQLSTATE, not the MySQL numeric error code.
        return matches!(mysql.number(), 1213 | 1205);
    }
    matches!(
        error.code().as_deref(),
        // PostgreSQL serialization failure/deadlock; SQLite BUSY, LOCKED and
        // their extended codes. Each retry opens a new transaction/snapshot.
        Some("40001" | "40P01" | "5" | "6" | "261" | "262" | "517")
    )
}
