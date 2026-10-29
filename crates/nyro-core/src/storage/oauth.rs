//! Shared OAuth storage invariants. SQL stores apply mutations while holding the
//! parent-provider lock; memory holds its credential-map write lock.

use crate::db::models::{OAuthAccountUpdate, OAuthCredential, UpsertOAuthCredential};

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
        return matches!(mysql.number(), 1213 | 1205);
    }
    matches!(
        error.code().as_deref(),
        Some("40001" | "40P01" | "5" | "6" | "261" | "262" | "517")
    )
}

pub(super) fn singleton(rows: &[OAuthCredential]) -> anyhow::Result<Option<OAuthCredential>> {
    let mut active = rows.iter().filter(|row| row.status != "disconnected");
    let first = active.next().cloned();
    anyhow::ensure!(active.next().is_none(), "AMBIGUOUS_ACCOUNT");
    Ok(first)
}

pub(super) enum Mutation<'a> {
    Upsert(&'a UpsertOAuthCredential),
    Restore(i32, Option<&'a UpsertOAuthCredential>),
    Delete,
    BeginRefresh(i32),
    CompleteRefresh(i32, &'a UpsertOAuthCredential),
    FailRefresh(i32, &'a str, bool, Option<&'a str>),
    BeginAccountRefresh(&'a str, i32),
    CompleteAccountRefresh(&'a str, i32, &'a UpsertOAuthCredential),
    FailAccountRefresh(&'a str, i32, &'a str, bool, Option<&'a str>),
    Create(&'a str, &'a UpsertOAuthCredential),
    Replace(&'a str, i32, &'a UpsertOAuthCredential),
    Update(&'a str, i32, &'a OAuthAccountUpdate),
    Disconnect(&'a str, i32),
    CompareState(&'a str, i32, &'a str, &'a str),
}

fn normalized_token(driver: &str, token: &str) -> String {
    let token = token.trim();
    if crate::auth::normalize_driver_key(driver) == "devin" && token.starts_with("eyJ") {
        format!("devin-session-token${token}")
    } else {
        token.to_string()
    }
}

// A subject alone is not a verified global identity. Only deduplicate when the
// caller supplied an explicit authenticated organization/tenant scope as well.
fn scoped_identity(subject: Option<&str>, meta: &str) -> Option<(String, String, String)> {
    let subject = subject?.trim();
    if subject.is_empty() {
        return None;
    }
    let meta: serde_json::Value = serde_json::from_str(meta).ok()?;
    for key in ["organization_id", "org_id", "tenant_id", "team_id"] {
        if let Some(scope) = meta
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Some((subject.to_string(), key.to_string(), scope.to_string()));
        }
    }
    None
}

fn validate_input(
    parent: Option<(&str, &str, &str, &str)>,
    input: &UpsertOAuthCredential,
) -> anyhow::Result<()> {
    let (mode, vendor, preset, channel) =
        parent.ok_or_else(|| anyhow::anyhow!("OAuth provider not found"))?;
    anyhow::ensure!(mode == "oauth", "OAuth account requires an OAuth provider");
    // Match the management binding contract: vendor takes precedence, preset
    // only supplies a missing vendor, and Google's OAuth client is channel-specific.
    let vendor = crate::auth::normalize_driver_key(if vendor.trim().is_empty() {
        preset
    } else {
        vendor
    });
    let expected_driver = if vendor == "google" && channel.trim().eq_ignore_ascii_case("gemini-cli")
    {
        "google-gemini-cli"
    } else {
        vendor.as_str()
    };
    anyhow::ensure!(
        !expected_driver.is_empty()
            && expected_driver == crate::auth::normalize_driver_key(&input.driver_key),
        "OAuth account vendor mismatch"
    );
    let token = normalized_token(&input.driver_key, &input.access_token);
    anyhow::ensure!(
        !token.is_empty()
            && token.len() <= 32768
            && !token
                .bytes()
                .any(|c| c.is_ascii_whitespace() || c.is_ascii_control()),
        "Invalid OAuth access token"
    );
    Ok(())
}

fn check_duplicates(
    rows: &[OAuthCredential],
    except: Option<&str>,
    input: &UpsertOAuthCredential,
) -> anyhow::Result<()> {
    let driver = crate::auth::normalize_driver_key(&input.driver_key);
    let token = normalized_token(&driver, &input.access_token);
    let identity = scoped_identity(
        input.subject_id.as_deref(),
        input.meta.as_deref().unwrap_or("{}"),
    );
    for row in rows
        .iter()
        .filter(|r| r.status != "disconnected" && Some(r.account_id.as_str()) != except)
    {
        // Earliest Provider-field backfills did not populate driver_key; their
        // token still belongs to this parent's driver and must not be duplicated.
        if !row.driver_key.is_empty()
            && crate::auth::normalize_driver_key(&row.driver_key) != driver
        {
            continue;
        }
        if normalized_token(&driver, &row.access_token) == token
            || identity.is_some()
                && identity == scoped_identity(row.subject_id.as_deref(), &row.meta)
        {
            anyhow::bail!("DUPLICATE_ACCOUNT: {}", row.account_id);
        }
    }
    Ok(())
}

fn bind(row: &mut OAuthCredential, input: &UpsertOAuthCredential, refresh: bool) {
    row.driver_key = input.driver_key.clone();
    row.scheme = input.scheme.clone();
    row.access_token = normalized_token(&input.driver_key, &input.access_token);
    row.refresh_token = input.refresh_token.clone();
    row.expires_at = input.expires_at.clone();
    row.resource_url = input.resource_url.clone();
    row.subject_id = input.subject_id.clone();
    if !refresh || input.scopes.is_some() {
        row.scopes = input.scopes.clone().unwrap_or_else(|| "[]".into());
    }
    if !refresh || input.meta.is_some() {
        row.meta = input.meta.clone().unwrap_or_else(|| "{}".into());
    }
    row.status = "connected".into();
    row.last_error = None;
    if refresh {
        row.last_refresh_at = Some(row.updated_at.clone());
    } else {
        row.account_state = "{}".into();
    }
}

fn scrub(row: &mut OAuthCredential) {
    row.driver_key.clear();
    row.scheme.clear();
    row.access_token.clear();
    row.refresh_token = None;
    row.expires_at = None;
    row.resource_url = None;
    row.subject_id = None;
    row.scopes = "[]".into();
    row.meta = "{}".into();
    row.account_state = "{}".into();
    row.label.clear();
    row.is_enabled = false;
    row.priority = 0;
    row.status = "disconnected".into();
    row.last_error = None;
    row.last_refresh_at = None;
}

fn new_row(provider: &str, account_id: String, label: &str, now: &str) -> OAuthCredential {
    OAuthCredential {
        account_id,
        provider_id: provider.into(),
        label: label.into(),
        is_enabled: true,
        priority: 0,
        account_state: "{}".into(),
        driver_key: String::new(),
        scheme: String::new(),
        access_token: String::new(),
        refresh_token: None,
        expires_at: None,
        resource_url: None,
        subject_id: None,
        scopes: "[]".into(),
        meta: "{}".into(),
        status: "connected".into(),
        status_version: -1,
        last_error: None,
        last_refresh_at: None,
        created_at: now.into(),
        updated_at: now.into(),
    }
}

/// Returns the exact row to publish, or None for a stale/missing target. No
/// caller may write a result without retaining the lock used to load `rows`.
pub(super) fn mutate(
    provider: &str,
    parent: Option<(&str, &str, &str, &str)>,
    rows: &[OAuthCredential],
    action: &Mutation<'_>,
) -> anyhow::Result<Option<OAuthCredential>> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let explicit = match action {
        Mutation::Replace(id, version, _)
        | Mutation::Update(id, version, _)
        | Mutation::Disconnect(id, version)
        | Mutation::CompareState(id, version, _, _)
        | Mutation::BeginAccountRefresh(id, version)
        | Mutation::CompleteAccountRefresh(id, version, _)
        | Mutation::FailAccountRefresh(id, version, _, _, _) => Some((*id, *version)),
        _ => None,
    };
    let mut row = if let Mutation::Create(label, input) = action {
        validate_input(parent, input)?;
        anyhow::ensure!(!label.trim().is_empty(), "Account label is required");
        check_duplicates(rows, None, input)?;
        anyhow::ensure!(
            matches!(
                crate::auth::normalize_driver_key(&input.driver_key).as_str(),
                "devin" | "kiro" | "claude-code"
            ) || rows.iter().all(|r| r.status == "disconnected"),
            "MAX_ACCOUNTS: this OAuth driver permits one account"
        );
        let mut row = new_row(
            provider,
            uuid::Uuid::new_v4().to_string(),
            label.trim(),
            &now,
        );
        row.status_version = rows.iter().map(|r| r.status_version).max().unwrap_or(-1);
        row
    } else if let Some((id, version)) = explicit {
        let Some(row) = rows.iter().find(|r| {
            r.account_id == id
                && r.provider_id == provider
                && r.status != "disconnected"
                && r.status_version == version
        }) else {
            return Ok(None);
        };
        row.clone()
    } else {
        match singleton(rows)? {
            Some(row) => {
                // Legacy CAS has no account ID. After pool contraction, an old
                // callback must not accidentally match a sibling's generation.
                if matches!(
                    action,
                    Mutation::Restore(..)
                        | Mutation::BeginRefresh(..)
                        | Mutation::CompleteRefresh(..)
                        | Mutation::FailRefresh(..)
                ) && (rows.iter().any(|other| {
                    other.account_id != row.account_id && other.status_version >= row.status_version
                }) || rows.len() > 1
                    && (matches!(action, Mutation::Restore(..))
                        || matches!(
                            crate::auth::normalize_driver_key(&row.driver_key).as_str(),
                            "devin" | "kiro" | "claude-code"
                        )))
                {
                    return Ok(None);
                }
                row
            }
            None if matches!(action, Mutation::Upsert(_)) => rows
                .iter()
                .filter(|r| r.status == "disconnected")
                .max_by_key(|r| (r.status_version, r.account_id.clone()))
                .cloned()
                .unwrap_or_else(|| {
                    new_row(
                        provider,
                        format!("legacy:{provider}"),
                        "Default account",
                        &now,
                    )
                }),
            None => return Ok(None),
        }
    };
    let old_version = row.status_version;
    // Observation writes do not renew the age of an in-flight refresh lease.
    if !matches!(action, Mutation::CompareState(..)) {
        row.updated_at = now;
    }
    match action {
        Mutation::Create(_, input) => bind(&mut row, input, false),
        Mutation::Upsert(input) => {
            if row.status == "disconnected" {
                row.label = "Default account".into();
                row.is_enabled = true;
            }
            bind(&mut row, input, false);
            // Provider-only callbacks have no account ID. Fence every generation
            // previously published by this facade, even after pool contraction.
            row.status_version = rows.iter().map(|r| r.status_version).max().unwrap_or(-1);
        }
        Mutation::Replace(_, _, input) => {
            validate_input(parent, input)?;
            anyhow::ensure!(
                matches!(
                    crate::auth::normalize_driver_key(&input.driver_key).as_str(),
                    "devin" | "kiro" | "claude-code"
                ) || rows.iter().filter(|r| r.status != "disconnected").count() <= 1,
                "MAX_ACCOUNTS: this OAuth driver permits one account"
            );
            check_duplicates(rows, Some(&row.account_id), input)?;
            bind(&mut row, input, false);
        }
        Mutation::Restore(version, previous) => {
            if row.status != "connected" || old_version != *version {
                return Ok(None);
            }
            if let Some(input) = previous {
                bind(&mut row, input, false);
            } else {
                scrub(&mut row);
            }
        }
        Mutation::Delete | Mutation::Disconnect(_, _) => scrub(&mut row),
        Mutation::BeginRefresh(version) | Mutation::BeginAccountRefresh(_, version) => {
            if row.status != "connected" || old_version != *version {
                return Ok(None);
            }
            row.status = "refreshing".into();
        }
        Mutation::CompleteRefresh(version, input)
        | Mutation::CompleteAccountRefresh(_, version, input) => {
            if row.status != "refreshing" || old_version != *version {
                return Ok(None);
            }
            bind(&mut row, input, true);
        }
        Mutation::FailRefresh(version, error, terminal, meta)
        | Mutation::FailAccountRefresh(_, version, error, terminal, meta) => {
            if row.status != "refreshing" || old_version != *version {
                return Ok(None);
            }
            row.status = if *terminal { "error" } else { "connected" }.into();
            row.last_error = Some((*error).into());
            if let Some(meta) = meta {
                row.meta = (*meta).into();
            }
        }
        Mutation::Update(_, _, update) => {
            if let Some(label) = &update.label {
                anyhow::ensure!(!label.trim().is_empty(), "Account label is required");
                row.label = label.trim().into();
            }
            if let Some(enabled) = update.is_enabled {
                row.is_enabled = enabled;
            }
            if let Some(priority) = update.priority {
                row.priority = priority;
            }
        }
        Mutation::CompareState(_, _, expected, new_state) => {
            if row.account_state != *expected {
                return Ok(None);
            }
            anyhow::ensure!(new_state.len() <= 65536, "Account state is too large");
            let value: serde_json::Value = serde_json::from_str(new_state)
                .map_err(|_| anyhow::anyhow!("Account state must be a JSON object"))?;
            anyhow::ensure!(value.is_object(), "Account state must be a JSON object");
            row.account_state = (*new_state).into();
            return Ok(Some(row));
        }
    }
    row.status_version = row
        .status_version
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("OAuth generation overflow"))?;
    Ok(Some(row))
}
