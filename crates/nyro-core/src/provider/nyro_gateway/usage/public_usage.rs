//! Fixed anonymous forwarding DTO and receive-side allowlist. Adding a field to
//! admin usage must never implicitly expand the public proxy surface.

use serde::Serialize;
use serde_json::{Map, Value};

use super::pool_quota::AnonymousPoolQuota;
use crate::admin::{
    ProviderScheduling, ProviderUsage, ProviderUsageBalance, ProviderUsageSpend, ProviderUsageTier,
};

#[derive(Debug, Clone, Serialize)]
pub struct PublicProviderUsage {
    pub provider_id: String,
    pub kind: String,
    pub site: String,
    pub level: Option<String>,
    pub tiers: Vec<ProviderUsageTier>,
    pub balances: Vec<ProviderUsageBalance>,
    pub spends: Vec<ProviderUsageSpend>,
    pub is_available: Option<bool>,
    pub scheduling: ProviderScheduling,
    pub queried_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_quota: Option<AnonymousPoolQuota>,
    pub stale: bool,
}

impl PublicProviderUsage {
    pub fn from_usage(usage: &ProviderUsage, include_money: bool, stale: bool) -> Self {
        Self {
            provider_id: usage.provider_id.clone(),
            kind: usage.kind.clone(),
            site: usage.site.clone(),
            level: usage.level.clone(),
            tiers: usage.tiers.clone(),
            balances: if include_money {
                usage.balances.clone()
            } else {
                Vec::new()
            },
            spends: if include_money {
                usage.spends.clone()
            } else {
                Vec::new()
            },
            is_available: usage.is_available,
            scheduling: usage.scheduling.clone(),
            queried_at: usage.queried_at.clone(),
            pool_quota: usage.pool_quota.clone(),
            stale,
        }
    }
}

fn strings(raw: &Value) -> Value {
    Value::Array(
        raw.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|text| Value::String(text.to_owned()))
            .collect(),
    )
}

fn string_fields(raw: &Value, safe: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = raw.get(*field) {
            if let Some(text) = value.as_str() {
                safe.insert((*field).into(), Value::String(text.to_owned()));
            } else if value.is_null() {
                safe.insert((*field).into(), Value::Null);
            }
        }
    }
}

fn bool_fields(raw: &Value, safe: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = raw.get(*field) {
            if let Some(value) = value.as_bool() {
                safe.insert((*field).into(), Value::Bool(value));
            } else if value.is_null() {
                safe.insert((*field).into(), Value::Null);
            }
        }
    }
}

fn number_fields(raw: &Value, safe: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = raw.get(*field).and_then(Value::as_number) {
            safe.insert((*field).into(), Value::Number(value.clone()));
        }
    }
}

fn object_array(raw: Option<&Value>, convert: fn(&Value) -> Value) -> Option<Value> {
    raw.and_then(Value::as_array).map(|rows| {
        Value::Array(
            rows.iter()
                .filter(|row| row.is_object())
                .map(convert)
                .collect(),
        )
    })
}

fn tier(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(raw, &mut safe, &["name", "resets_at"]);
    number_fields(raw, &mut safe, &["used_percent"]);
    Value::Object(safe)
}

fn balance(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(raw, &mut safe, &["currency"]);
    number_fields(raw, &mut safe, &["total", "granted", "topped_up"]);
    Value::Object(safe)
}

fn spend(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(raw, &mut safe, &["name", "currency"]);
    number_fields(raw, &mut safe, &["amount"]);
    Value::Object(safe)
}

pub(super) fn safe_scheduling(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(
        raw,
        &mut safe,
        &["status", "reason", "reset_at", "next_check_at"],
    );
    if let Some(tiers) = raw.get("blocking_tiers") {
        safe.insert("blocking_tiers".into(), strings(tiers));
    }
    Value::Object(safe)
}

fn usage(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(
        raw,
        &mut safe,
        &["provider_id", "kind", "site", "level", "queried_at"],
    );
    bool_fields(raw, &mut safe, &["is_available", "stale"]);
    for (name, convert) in [
        ("tiers", tier as fn(&Value) -> Value),
        ("balances", balance),
        ("spends", spend),
    ] {
        if let Some(rows) = object_array(raw.get(name), convert) {
            safe.insert(name.into(), rows);
        }
    }
    if let Some(raw) = raw.get("scheduling").filter(|raw| raw.is_object()) {
        safe.insert("scheduling".into(), safe_scheduling(raw));
    }
    if let Some(raw) = raw.get("pool_quota")
        && let Ok(quota) = serde_json::from_value::<AnonymousPoolQuota>(raw.clone())
        && let Ok(quota) = serde_json::to_value(quota)
    {
        safe.insert("pool_quota".into(), quota);
    }
    // No quota_buckets, api_price, upstream, owner/account/project or arbitrary
    // nested extensions. This is an allowlist, not a recursively growing denylist.
    Value::Object(safe)
}

fn provenance(raw: &Value) -> Value {
    let mut safe = Map::new();
    if raw.get("kind").and_then(Value::as_str) == Some("quota_bucket") {
        safe.insert("kind".into(), Value::String("quota_bucket".into()));
        string_fields(
            raw,
            &mut safe,
            &["source_provider_ref", "bucket_ref", "binding_status"],
        );
        bool_fields(raw, &mut safe, &["stale"]);
    }
    Value::Object(safe)
}

fn provider(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(raw, &mut safe, &["id", "name", "vendor", "status", "error"]);
    if let Some(routes) = raw.get("routes") {
        safe.insert("routes".into(), strings(routes));
    }
    if let Some(raw_usage) = raw.get("usage").filter(|raw| raw.is_object()) {
        let mut projected = usage(raw_usage);
        if let Some(id) = raw.get("id").and_then(Value::as_str) {
            projected["provider_id"] = Value::String(id.to_owned());
        }
        safe.insert("usage".into(), projected);
    }
    if let Some(raw) = raw.get("usage_entry").filter(|raw| raw.is_object()) {
        safe.insert("usage_entry".into(), provenance(raw));
    }
    Value::Object(safe)
}

fn route(raw: &Value) -> Value {
    let mut safe = Map::new();
    string_fields(raw, &mut safe, &["route"]);
    if let Some(raw) = raw.get("scheduling").filter(|raw| raw.is_object()) {
        safe.insert("scheduling".into(), safe_scheduling(raw));
    }
    Value::Object(safe)
}

pub(super) fn sanitize(report: &mut Value) {
    let mut safe = Map::new();
    string_fields(report, &mut safe, &["object", "generated_at"]);
    number_fields(report, &mut safe, &["version"]);
    for name in ["redacted", "capabilities"] {
        if let Some(raw) = report.get(name) {
            safe.insert(name.into(), strings(raw));
        }
    }
    if let Some(mut rows) = object_array(report.get("providers"), provider) {
        // Honor monetary withholding even if an older/buggy peer attached a
        // value inside a redacted field. The receiver cannot widen permission.
        for field in ["balances", "spends"] {
            let withheld = report
                .get("redacted")
                .and_then(Value::as_array)
                .is_some_and(|fields| fields.iter().any(|value| value.as_str() == Some(field)));
            if withheld {
                for row in rows.as_array_mut().into_iter().flatten() {
                    if let Some(usage) = row.get_mut("usage").and_then(Value::as_object_mut) {
                        usage.insert(field.into(), Value::Array(Vec::new()));
                    }
                }
            }
        }
        safe.insert("providers".into(), rows);
    }
    if let Some(rows) = object_array(report.get("route_scheduling"), route) {
        safe.insert("route_scheduling".into(), rows);
    }
    *report = Value::Object(safe);
}
