//! Coding-plan usage (quota) queries against upstream vendors.
//!
//! GLM Coding Plan (Zhipu `open.bigmodel.cn` / Z.ai `api.z.ai`, personal
//! plan): queries the vendor monitor endpoint
//! `GET {site}/api/monitor/usage/quota/limit` with the provider's raw API
//! key (no Bearer prefix). Behavior ported from cc-switch
//! `src-tauri/src/services/coding_plan.rs` (query_zhipu /
//! parse_zhipu_token_tiers), including the `unit`-field window
//! classification — buckets must NOT be sorted by reset time: near the end
//! of a weekly cycle the weekly bucket resets earlier than the 5-hour one
//! and time ordering swaps the two labels (cc-switch issue #3036).
//!
//! MiniMax Coding Plan (`api.minimaxi.com` CN / `api.minimax.io` global):
//! `GET https://{domain}/v1/api/openplatform/coding_plan/remains` with a
//! Bearer key. Also ported from cc-switch (query_minimax /
//! parse_minimax_tiers): only the `general` model entry is a coding quota,
//! percentages are *remaining* (inverted to used), and the weekly bucket is
//! only active when `current_weekly_status == 1` (3 = plan without a weekly
//! limit, remaining pinned at 100 — must not be shown).
//!
//! Kimi For Coding (`api.kimi.com/coding`): `GET
//! https://api.kimi.com/coding/v1/usages` with a Bearer key. Ported from
//! cc-switch query_kimi: `limits[].detail` entries are 5-hour windows with
//! ABSOLUTE limit/remaining values (utilization = (limit - remaining) /
//! limit), and the top-level `usage` object is the weekly window with the
//! same absolute-value shape. `limit`/`remaining` may be numbers or numeric
//! strings; `resetTime` may be an ISO string or a seconds/milliseconds
//! epoch number.
//!
//! OpenCode Go (`opencode.ai/zen/go`): `GET
//! https://opencode.ai/zen/go/v1/usage` with the regular Bearer API key
//! (official but undocumented; no workspace id / auth cookie). The response
//! carries THREE windows — `rolling` (5h), `weekly`, `monthly` — each with
//! `{ status, percent, resetsAt }`; `percent` is already a used percentage
//! and `resetsAt` is an ISO string, both passed through untouched. Windows
//! whose `status` is not `"ok"` are skipped (verified against a live key
//! 2026-08-16 and the community scripts in cc-switch #6433 /
//! dsh-opencode-go-usage).

use reqwest::header::CONTENT_TYPE;

use super::*;

const TIER_FIVE_HOUR: &str = "five_hour";
const TIER_WEEKLY_LIMIT: &str = "weekly_limit";
const TIER_MONTHLY: &str = "monthly";

/// GLM quota endpoint path, shared by both sites.
const GLM_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// MiniMax coding-plan quota endpoint path, shared by both sites.
const MINIMAX_QUOTA_PATH: &str = "/v1/api/openplatform/coding_plan/remains";

/// Kimi For Coding usage endpoint.
const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

/// DeepSeek account balance endpoint.
const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// OpenCode Go subscription usage endpoint.
const OPENCODE_GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderUsageTier {
    /// `five_hour` | `weekly_limit` | `monthly`
    pub name: String,
    /// Used percentage (0-100).
    pub used_percent: f64,
    /// ISO 8601 reset time when the upstream reports one.
    pub resets_at: Option<String>,
}

/// Pay-as-you-go account balance, one entry per currency (DeepSeek shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderUsageBalance {
    /// ISO currency code, e.g. `CNY`.
    pub currency: String,
    /// Total remaining balance.
    pub total: f64,
    /// Granted (free/promo) portion of the balance.
    pub granted: f64,
    /// Topped-up (paid) portion of the balance.
    pub topped_up: f64,
}

/// A spend figure (consumed amount over a period), e.g. DeepSeek's
/// today/month cost read from the platform dashboard API.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderUsageSpend {
    /// `today` | `month`
    pub name: String,
    /// Consumed amount in the account currency.
    pub amount: f64,
    /// ISO currency code, e.g. `CNY`.
    pub currency: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    /// Query backend kind: `glm_coding_plan` | `minimax_coding_plan` |
    /// `kimi_coding_plan` | `deepseek_balance`.
    pub kind: String,
    /// Site the quota was read from: `cn` | `global`.
    pub site: String,
    /// Plan tier reported by the upstream (e.g. GLM `max`), when present.
    pub level: Option<String>,
    /// Time-window usage tiers (coding plans).
    pub tiers: Vec<ProviderUsageTier>,
    /// Account balances (pay-as-you-go vendors).
    pub balances: Vec<ProviderUsageBalance>,
    /// Spend figures over periods (e.g. DeepSeek today/month cost).
    pub spends: Vec<ProviderUsageSpend>,
    /// Account availability flag when the upstream reports one.
    pub is_available: Option<bool>,
    pub queried_at: String,
}

/// The coding-plan usage backend inferred from a provider's base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageBackend {
    /// GLM on open.bigmodel.cn (quota host pinned to the CN site).
    GlmCn,
    /// GLM on api.z.ai (quota host pinned to the global site).
    GlmGlobal,
    /// MiniMax on api.minimaxi.com (China site).
    MinimaxCn,
    /// MiniMax on api.minimax.io (global site).
    MinimaxGlobal,
    /// Kimi For Coding on api.kimi.com (single global endpoint).
    KimiCode,
    /// DeepSeek pay-as-you-go balance on api.deepseek.com.
    DeepSeek,
    /// OpenCode Go subscription on opencode.ai/zen/go.
    OpencodeGo,
    /// Volcengine Ark coding plan on ark.cn-beijing.volces.com. Usage is read
    /// from the control-plane OpenAPI with IAM AK/SK signing.
    ArkCoding,
}

impl UsageBackend {
    fn detect(base_url: &str) -> Option<Self> {
        let url = base_url.to_lowercase();
        if url.contains("bigmodel.cn") {
            Some(UsageBackend::GlmCn)
        } else if url.contains("z.ai") {
            Some(UsageBackend::GlmGlobal)
        } else if url.contains("minimaxi.com") {
            Some(UsageBackend::MinimaxCn)
        } else if url.contains("minimax.io") {
            Some(UsageBackend::MinimaxGlobal)
        } else if url.contains("api.kimi.com") {
            Some(UsageBackend::KimiCode)
        } else if url.contains("api.deepseek.com") {
            Some(UsageBackend::DeepSeek)
        } else if url.contains("opencode.ai/zen") {
            Some(UsageBackend::OpencodeGo)
        } else if url.contains("volces.com") {
            Some(UsageBackend::ArkCoding)
        } else {
            None
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            UsageBackend::GlmCn | UsageBackend::GlmGlobal => "glm_coding_plan",
            UsageBackend::MinimaxCn | UsageBackend::MinimaxGlobal => "minimax_coding_plan",
            UsageBackend::KimiCode => "kimi_coding_plan",
            UsageBackend::DeepSeek => "deepseek_balance",
            UsageBackend::OpencodeGo => "opencode_go",
            UsageBackend::ArkCoding => "ark_coding_plan",
        }
    }

    fn site(&self) -> &'static str {
        match self {
            UsageBackend::GlmCn | UsageBackend::MinimaxCn => "cn",
            // Kimi, DeepSeek and OpenCode Go serve both sites from a single
            // global endpoint.
            UsageBackend::GlmGlobal
            | UsageBackend::MinimaxGlobal
            | UsageBackend::KimiCode
            | UsageBackend::DeepSeek
            | UsageBackend::OpencodeGo => "global",
            UsageBackend::ArkCoding => "cn",
        }
    }
}

fn millis_to_iso8601(ms: i64) -> Option<String> {
    let secs = ms.div_euclid(1000);
    let nsecs = ms.rem_euclid(1000) as u32 * 1_000_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nsecs).map(|dt| dt.to_rfc3339())
}

enum GlmWindow {
    FiveHour,
    Weekly,
}

/// Classify a `limits[]` entry by its explicit `unit` field. Observed shapes
/// (bigmodel.cn and z.ai share the same backend):
/// - `unit: 3, number: 5` → 5-hour rolling window (old and new plans)
/// - `unit: 6, number: 7` and `unit: 6, number: 1` → weekly window (both
///   observed, so only `unit` is anchored)
///
/// Unknown or missing `unit` returns `None` and the caller falls back to the
/// reset-time heuristic.
fn classify_glm_window(item: &Value) -> Option<GlmWindow> {
    match item.get("unit").and_then(Value::as_i64) {
        Some(3) => Some(GlmWindow::FiveHour),
        Some(6) => Some(GlmWindow::Weekly),
        _ => None,
    }
}

/// Parse the GLM quota `data` object into usage tiers.
///
/// Classification order:
/// 1. Explicit `unit` field (see [`classify_glm_window`]).
/// 2. Fallback heuristic (missing/unrecognized `unit`): entries without a
///    `nextResetTime` fill the five-hour slot first (the 5-hour bucket can
///    lack a reset while idle at 0%), the rest fill remaining slots by reset
///    time ascending.
///
/// Old plans (subscribed before 2026-02-12) return a single TOKENS_LIMIT
/// entry and degrade to a five-hour-only view; at most two entries are kept.
fn parse_glm_quota_tiers(data: &Value) -> Vec<ProviderUsageTier> {
    type Entry = (Option<i64>, f64, Option<String>);
    let mut five_hour: Option<Entry> = None;
    let mut weekly: Option<Entry> = None;
    let mut unclassified: Vec<Entry> = Vec::new();

    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for item in limits {
            let limit_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            // Case-insensitive: survives upstream switching to lowercase or
            // camelCase. TIME_LIMIT entries (per-call quotas) are skipped.
            if !(limit_type.eq_ignore_ascii_case("TOKENS_LIMIT")
                || limit_type.eq_ignore_ascii_case("CREDIT_LIMIT"))
            {
                continue;
            }
            let percentage = item
                .get("percentage")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let reset_ms = item.get("nextResetTime").and_then(Value::as_i64);
            let reset_iso = reset_ms.and_then(millis_to_iso8601);
            let entry = (reset_ms, percentage, reset_iso);
            match classify_glm_window(item) {
                Some(GlmWindow::FiveHour) if five_hour.is_none() => five_hour = Some(entry),
                Some(GlmWindow::Weekly) if weekly.is_none() => weekly = Some(entry),
                _ => unclassified.push(entry),
            }
        }
    }

    unclassified.sort_by_key(|(reset, _, _)| (reset.is_some(), reset.unwrap_or(i64::MIN)));
    for entry in unclassified {
        if five_hour.is_none() {
            five_hour = Some(entry);
        } else if weekly.is_none() {
            weekly = Some(entry);
        }
    }

    let mut tiers = Vec::new();
    for (name, slot) in [(TIER_FIVE_HOUR, five_hour), (TIER_WEEKLY_LIMIT, weekly)] {
        if let Some((_, used_percent, resets_at)) = slot {
            tiers.push(ProviderUsageTier {
                name: name.to_string(),
                used_percent,
                resets_at,
            });
        }
    }
    tiers
}

/// Parse the MiniMax `/coding_plan/remains` response into usage tiers.
///
/// Semantics (cc-switch parse_minimax_tiers):
/// - `model_remains[]` carries entries per model; only `model_name ==
///   "general"` is the coding-plan quota (`video` and others are skipped).
/// - `current_*_remaining_percent` is the REMAINING percentage (0-100),
///   inverted here into a used percentage.
/// - The 5-hour bucket always exists; the weekly bucket is only active when
///   `current_weekly_status == 1` — plans without a weekly limit report 3
///   with remaining pinned at 100 and must not be shown.
fn parse_minimax_tiers(body: &Value) -> Vec<ProviderUsageTier> {
    let mut tiers = Vec::new();

    let Some(model_remains) = body.get("model_remains").and_then(Value::as_array) else {
        return tiers;
    };
    let Some(item) = model_remains.iter().find(|item| {
        item.get("model_name")
            .and_then(Value::as_str)
            .is_some_and(|s| s == "general")
    }) else {
        return tiers;
    };

    // 5-hour bucket: remaining → used.
    if let Some(remain_pct) = item
        .get("current_interval_remaining_percent")
        .and_then(Value::as_f64)
    {
        let resets_at = item
            .get("end_time")
            .and_then(Value::as_i64)
            .and_then(millis_to_iso8601);
        tiers.push(ProviderUsageTier {
            name: TIER_FIVE_HOUR.to_string(),
            used_percent: 100.0 - remain_pct,
            resets_at,
        });
    }

    // Weekly bucket: only active at status == 1.
    if item.get("current_weekly_status").and_then(Value::as_i64) == Some(1)
        && let Some(remain_pct) = item
            .get("current_weekly_remaining_percent")
            .and_then(Value::as_f64)
    {
        let resets_at = item
            .get("weekly_end_time")
            .and_then(Value::as_i64)
            .and_then(millis_to_iso8601);
        tiers.push(ProviderUsageTier {
            name: TIER_WEEKLY_LIMIT.to_string(),
            used_percent: 100.0 - remain_pct,
            resets_at,
        });
    }

    tiers
}

/// Parse a JSON value as f64, accepting numbers and numeric strings
/// (`100` and `"100"`), mirroring cc-switch parse_f64.
fn parse_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// Extract a reset time as ISO 8601 from a string, a seconds epoch, or a
/// milliseconds epoch, mirroring cc-switch extract_reset_time. Zero/negative
/// epochs mean "no reset time".
fn extract_reset_time(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    if let Some(n) = value.as_i64() {
        if n <= 0 {
            return None;
        }
        // Seconds epochs are < 1e12; milliseconds are >= 1e12.
        let ms = if n < 1_000_000_000_000 { n * 1000 } else { n };
        return millis_to_iso8601(ms);
    }
    None
}

/// Build a tier from ABSOLUTE limit/remaining values (Kimi shape).
/// Utilization = (limit - remaining) / limit, floored at zero-used.
fn kimi_tier(name: &str, detail: &Value) -> ProviderUsageTier {
    let limit = detail.get("limit").and_then(parse_f64).unwrap_or(1.0);
    let remaining = detail.get("remaining").and_then(parse_f64).unwrap_or(0.0);
    let resets_at = detail.get("resetTime").and_then(extract_reset_time);
    let used = (limit - remaining).max(0.0);
    let used_percent = if limit > 0.0 {
        used / limit * 100.0
    } else {
        0.0
    };
    ProviderUsageTier {
        name: name.to_string(),
        used_percent,
        resets_at,
    }
}

/// Parse the Kimi `/coding/v1/usages` response into usage tiers.
///
/// Shape (cc-switch query_kimi):
/// - `limits[]` entries carry a `detail` object each; every one is a 5-hour
///   window (Kimi returns at most one in practice) and is emitted as a
///   `five_hour` tier.
/// - The top-level `usage` object is the weekly window (`weekly_limit`).
///
/// Both use absolute `limit` / `remaining` values, not percentages.
fn parse_kimi_tiers(body: &Value) -> Vec<ProviderUsageTier> {
    let mut tiers = Vec::new();

    // 5-hour window limits (displayed first).
    if let Some(limits) = body.get("limits").and_then(Value::as_array) {
        for limit_item in limits {
            if let Some(detail) = limit_item.get("detail") {
                tiers.push(kimi_tier(TIER_FIVE_HOUR, detail));
            }
        }
    }

    // Overall usage (weekly limit).
    if let Some(usage) = body.get("usage") {
        tiers.push(kimi_tier(TIER_WEEKLY_LIMIT, usage));
    }

    tiers
}

/// Parse the DeepSeek `/user/balance` response body.
///
/// Shape (cc-switch query_deepseek): `{ is_available: bool,
/// balance_infos: [{ currency, total_balance, granted_balance,
/// topped_up_balance }] }` — one entry per currency, values as numeric
/// STRINGS.
fn parse_deepseek_balances(body: &Value) -> (Vec<ProviderUsageBalance>, Option<bool>) {
    let is_available = body.get("is_available").and_then(Value::as_bool);
    let mut balances = Vec::new();
    if let Some(infos) = body.get("balance_infos").and_then(Value::as_array) {
        for info in infos {
            let currency = info
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("CNY")
                .to_string();
            balances.push(ProviderUsageBalance {
                currency,
                total: info.get("total_balance").and_then(parse_f64).unwrap_or(0.0),
                granted: info
                    .get("granted_balance")
                    .and_then(parse_f64)
                    .unwrap_or(0.0),
                topped_up: info
                    .get("topped_up_balance")
                    .and_then(parse_f64)
                    .unwrap_or(0.0),
            });
        }
    }
    (balances, is_available)
}

/// Parse the OpenCode Go `/v1/usage` response into usage tiers.
///
/// Shape (verified live 2026-08-16): `{ usage: { rolling, weekly, monthly }
/// }`, each window `{ status, percent, resetsAt }`. `percent` is already a
/// used percentage (0-100) and `resetsAt` is an ISO string — both passed
/// through. Windows whose `status` is not `"ok"` are skipped, as are
/// missing windows; emission order is rolling → weekly → monthly.
fn parse_opencode_tiers(body: &Value) -> Vec<ProviderUsageTier> {
    let Some(usage) = body.get("usage") else {
        return Vec::new();
    };
    let windows = [
        ("rolling", TIER_FIVE_HOUR),
        ("weekly", TIER_WEEKLY_LIMIT),
        ("monthly", TIER_MONTHLY),
    ];
    windows
        .into_iter()
        .filter_map(|(key, tier_name)| {
            let window = usage.get(key)?;
            if window.get("status").and_then(Value::as_str) != Some("ok") {
                return None;
            }
            let used_percent = window.get("percent").and_then(Value::as_f64)?;
            let resets_at = window
                .get("resetsAt")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(ProviderUsageTier {
                name: tier_name.to_string(),
                used_percent,
                resets_at,
            })
        })
        .collect()
}

/// Parse the Volcengine `GetCodingPlanUsage` OpenAPI result into tiers.
///
/// Each usage window carries a `Level` label (`5h` / `weekly` / `monthly`),
/// a used `Percent` (0-100), and a `ResetTimestamp` in unix seconds
/// (`<= 0` means "no reset scheduled" and is dropped). Windows with unknown
/// labels are skipped. The parser walks any nesting inside `Result` so minor
/// upstream shape changes do not break it.
fn parse_ark_tiers(result: &Value) -> Vec<ProviderUsageTier> {
    fn tier_name(level: &str) -> Option<&'static str> {
        match level.trim().to_ascii_lowercase().as_str() {
            "5h" | "five_hour" | "fivehour" => Some(TIER_FIVE_HOUR),
            "weekly" | "week" => Some(TIER_WEEKLY_LIMIT),
            "monthly" | "month" => Some(TIER_MONTHLY),
            _ => None,
        }
    }

    fn visit(value: &Value, out: &mut Vec<ProviderUsageTier>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| visit(item, out)),
            Value::Object(map) => {
                let has_percent = map.contains_key("Percent") || map.contains_key("percent");
                if has_percent {
                    let level = map
                        .get("Level")
                        .or_else(|| map.get("level"))
                        .and_then(Value::as_str)
                        .and_then(tier_name);
                    if let Some(name) = level {
                        let used_percent = map
                            .get("Percent")
                            .or_else(|| map.get("percent"))
                            .and_then(parse_f64);
                        if let Some(used_percent) = used_percent {
                            let resets_at = map
                                .get("ResetTimestamp")
                                .or_else(|| map.get("resetTimestamp"))
                                .or_else(|| map.get("reset_timestamp"))
                                .and_then(Value::as_i64)
                                .filter(|ts| *ts > 0)
                                .and_then(|ts| {
                                    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                                        .map(|dt| dt.to_rfc3339())
                                });
                            out.push(ProviderUsageTier {
                                name: name.to_string(),
                                used_percent,
                                resets_at,
                            });
                            return;
                        }
                    }
                }
                map.values().for_each(|child| visit(child, out));
            }
            _ => {}
        }
    }

    let mut tiers = Vec::new();
    visit(result, &mut tiers);
    // Stable emission order: five-hour, weekly, monthly.
    let order = |name: &str| match name {
        TIER_FIVE_HOUR => 0,
        TIER_WEEKLY_LIMIT => 1,
        _ => 2,
    };
    tiers.sort_by_key(|tier| order(&tier.name));
    tiers
}

/// Settings keys for the per-provider usage-query credentials (distinct from
/// the inference API key). Two generic slots: Volcengine Ark stores its IAM
/// AK/SK pair; DeepSeek stores the platform userToken in slot A. Reads fall
/// back to the legacy `usage.ark.*` keys so existing Ark setups keep working.
fn usage_credential_keys(provider_id: &str) -> ((String, String), (String, String)) {
    (
        (
            format!("usage.cred.a.{provider_id}"),
            format!("usage.cred.b.{provider_id}"),
        ),
        (
            format!("usage.ark.ak.{provider_id}"),
            format!("usage.ark.sk.{provider_id}"),
        ),
    )
}

/// Parse the DeepSeek platform dashboard `usage/cost` response into
/// today's and the month's total spend.
///
/// Envelope (private API, verified against the web console):
/// `{ code: 0, data: { biz_code: 0, biz_data: { days: [ { date:
/// "YYYY-MM-DD", data: [ { usage: [ { cost|amount } ] } ] } ] } } } }`
/// (`biz_data` may also be a one-element array). Days are keyed by the
/// account's local calendar date; every model's usage cost is summed per
/// day, the month total across all days. Returns an empty list on any
/// envelope/shape mismatch (the caller then just omits spend rows).
fn parse_deepseek_spends(body: &Value, currency: &str) -> Vec<ProviderUsageSpend> {
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        return Vec::new();
    }
    let Some(data) = body.get("data") else {
        return Vec::new();
    };
    if data.get("biz_code").and_then(Value::as_i64) != Some(0) {
        return Vec::new();
    }
    let Some(biz_data) = data.get("biz_data") else {
        return Vec::new();
    };
    let container = biz_data
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(biz_data);
    let Some(days) = container.get("days").and_then(Value::as_array) else {
        return Vec::new();
    };

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut today_amount = None;
    let mut month_amount = 0.0;
    for day in days {
        let date = day.get("date").and_then(Value::as_str).unwrap_or("");
        let mut day_total = 0.0;
        for model_entry in day
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for usage in model_entry
                .get("usage")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(cost) = usage
                    .get("cost")
                    .or_else(|| usage.get("amount"))
                    .and_then(parse_f64)
                {
                    day_total += cost;
                }
            }
        }
        month_amount += day_total;
        if date == today {
            today_amount = Some(day_total);
        }
    }

    let mut spends = Vec::new();
    if let Some(amount) = today_amount {
        spends.push(ProviderUsageSpend {
            name: "today".to_string(),
            amount: (amount * 100.0).round() / 100.0,
            currency: currency.to_string(),
        });
    }
    spends.push(ProviderUsageSpend {
        name: "month".to_string(),
        amount: (month_amount * 100.0).round() / 100.0,
        currency: currency.to_string(),
    });
    spends
}

/// Fetch the current month's cost from the DeepSeek platform dashboard API.
/// Auth is the web session `userToken` (platform.deepseek.com localStorage),
/// NOT the inference API key. Envelope errors (expired token: 40002/40003)
/// surface as errors the caller silently ignores.
async fn fetch_deepseek_platform_cost(
    client: &reqwest::Client,
    token: &str,
) -> anyhow::Result<Value> {
    use chrono::Datelike;
    let now = chrono::Local::now();
    let url = format!(
        "https://platform.deepseek.com/api/v0/usage/cost?month={}&year={}",
        now.month(),
        now.year()
    );
    let resp = client
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("x-app-version", "1.0.0")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("platform usage request failed: {e}"))?;
    let status = resp.status();
    let raw = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read platform usage response: {e}"))?;
    let body: Value = serde_json::from_slice(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse platform usage response: {e}"))?;
    if !status.is_success() {
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
        anyhow::bail!("HTTP {status} (code {code})");
    }
    if body.get("code").and_then(Value::as_i64) == Some(40002)
        || body.get("code").and_then(Value::as_i64) == Some(40003)
    {
        anyhow::bail!(
            "platform token expired: re-login platform.deepseek.com and update the token"
        );
    }
    if body.get("code").and_then(Value::as_i64) != Some(0) {
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
        anyhow::bail!("platform usage error (code {code})");
    }
    Ok(body)
}

/// Resolve the API key for a usage query. GLM coding plans are plain
/// API-key providers; adaptive providers fall back to the first enabled
/// protocol endpoint's key (GLM coding keys are shared across protocols).
fn usage_api_key(provider: &Provider) -> Option<String> {
    let trimmed = provider.api_key.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    provider
        .protocol_endpoints
        .iter()
        .find(|endpoint| endpoint.is_enabled && !endpoint.api_key.trim().is_empty())
        .map(|endpoint| endpoint.api_key.trim().to_string())
}

/// Send a usage GET with Bearer auth (MiniMax) and hand back the parsed
/// JSON body, after the shared status/business-error checks.
async fn fetch_usage_json(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> anyhow::Result<Value> {
    let resp = client
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("usage query request failed: {e}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("authentication failed (HTTP {status}): check the provider API key");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("HTTP {status}: {preview}");
    }
    let raw = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read usage response: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("failed to parse usage response: {e}"))
}

impl AdminService {
    /// Query the coding-plan usage (quota) of a configured provider.
    ///
    /// Supported backends:
    /// - GLM Coding Plan (Zhipu / Z.ai personal plan):
    ///   `GET {site}/api/monitor/usage/quota/limit`, raw API key without the
    ///   Bearer prefix.
    /// - MiniMax Coding Plan (CN / global): `GET
    ///   https://{domain}/v1/api/openplatform/coding_plan/remains`, Bearer
    ///   key.
    /// - Kimi For Coding: `GET https://api.kimi.com/coding/v1/usages`,
    ///   Bearer key.
    pub async fn get_provider_usage(&self, id: &str) -> anyhow::Result<ProviderUsage> {
        let provider = self.get_provider(id).await?;

        let backend = UsageBackend::detect(&provider.base_url).ok_or_else(|| {
            anyhow::anyhow!(
                "usage query is not supported for this provider: only GLM \
                 (bigmodel.cn / api.z.ai), MiniMax (api.minimaxi.com / api.minimax.io), \
                 Kimi (api.kimi.com), OpenCode Go (opencode.ai/zen), Ark \
                 (volces.com) and DeepSeek (api.deepseek.com) are supported"
            )
        })?;

        let Some(api_key) = usage_api_key(&provider) else {
            anyhow::bail!("provider api key is empty");
        };

        let (tiers, balances, level, is_available, spends) = match backend {
            UsageBackend::GlmCn | UsageBackend::GlmGlobal => {
                let site_base = match backend {
                    UsageBackend::GlmCn => "https://open.bigmodel.cn",
                    _ => "https://api.z.ai",
                };
                let url = format!("{site_base}{GLM_QUOTA_PATH}");
                let resp = self
                    .gw
                    .http_client
                    .get(&url)
                    // Zhipu expects the raw key without the Bearer prefix.
                    .header(AUTHORIZATION, &api_key)
                    .header(CONTENT_TYPE, "application/json")
                    .header("Accept-Language", "en-US,en")
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("usage query request failed: {e}"))?;

                let status = resp.status();
                if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    anyhow::bail!(
                        "authentication failed (HTTP {status}): check the provider API key"
                    );
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    let preview: String = body.chars().take(200).collect();
                    anyhow::bail!("HTTP {status}: {preview}");
                }
                let raw = resp
                    .bytes()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read usage response: {e}"))?;
                let body: Value = serde_json::from_slice(&raw)
                    .map_err(|e| anyhow::anyhow!("failed to parse usage response: {e}"))?;

                // Business-level error: HTTP 200 with success:false + msg.
                if body.get("success").and_then(Value::as_bool) == Some(false) {
                    let msg = body
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    anyhow::bail!("API error: {msg}");
                }
                let data = body.get("data").ok_or_else(|| {
                    anyhow::anyhow!("unexpected usage response: missing 'data' field")
                })?;
                let level = data
                    .get("level")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (
                    parse_glm_quota_tiers(data),
                    Vec::new(),
                    level,
                    None,
                    Vec::new(),
                )
            }
            UsageBackend::MinimaxCn | UsageBackend::MinimaxGlobal => {
                let domain = match backend {
                    UsageBackend::MinimaxCn => "api.minimaxi.com",
                    _ => "api.minimax.io",
                };
                let url = format!("https://{domain}{MINIMAX_QUOTA_PATH}");
                let body = fetch_usage_json(&self.gw.http_client, &url, &api_key).await?;

                // MiniMax reports business errors inside a 200 body via
                // base_resp.status_code (0 = success).
                if let Some(base_resp) = body.get("base_resp") {
                    let status_code = base_resp
                        .get("status_code")
                        .and_then(Value::as_i64)
                        .unwrap_or(-1);
                    if status_code != 0 {
                        let msg = base_resp
                            .get("status_msg")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        anyhow::bail!("API error (code {status_code}): {msg}");
                    }
                }
                (
                    parse_minimax_tiers(&body),
                    Vec::new(),
                    None,
                    None,
                    Vec::new(),
                )
            }
            UsageBackend::KimiCode => {
                let body = fetch_usage_json(&self.gw.http_client, KIMI_USAGE_URL, &api_key).await?;
                // Kimi reports failures via plain HTTP statuses; a 200 body
                // with tiers (or an empty one) is the success shape.
                (parse_kimi_tiers(&body), Vec::new(), None, None, Vec::new())
            }
            UsageBackend::DeepSeek => {
                // DeepSeek reports failures via plain HTTP statuses; the
                // 200 body carries per-currency balance_infos with numeric
                // STRING values.
                let body =
                    fetch_usage_json(&self.gw.http_client, DEEPSEEK_BALANCE_URL, &api_key).await?;
                let (balances, is_available) = parse_deepseek_balances(&body);

                // Optional spend detail (today/month) from the platform
                // dashboard API — needs the web-session userToken, not the
                // API key. Without a token (or on any failure) the spend
                // rows are silently omitted; the balance still shows.
                let mut spends = Vec::new();
                if let Ok((Some(platform_token), _)) =
                    self.get_provider_usage_credentials(&provider.id).await
                {
                    if let Ok(cost_body) =
                        fetch_deepseek_platform_cost(&self.gw.http_client, &platform_token).await
                    {
                        let currency = balances
                            .first()
                            .map(|b| b.currency.clone())
                            .unwrap_or_else(|| "CNY".to_string());
                        spends = parse_deepseek_spends(&cost_body, &currency);
                    }
                }
                (Vec::new(), balances, None, is_available, spends)
            }
            UsageBackend::OpencodeGo => {
                // OpenCode Go reports failures via plain HTTP statuses; the
                // 200 body carries three ready-made percentage windows.
                let body =
                    fetch_usage_json(&self.gw.http_client, OPENCODE_GO_USAGE_URL, &api_key).await?;
                (
                    parse_opencode_tiers(&body),
                    Vec::new(),
                    None,
                    None,
                    Vec::new(),
                )
            }
            UsageBackend::ArkCoding => {
                // The Ark control-plane OpenAPI needs IAM AK/SK signing; the
                // inference API key is not accepted there.
                let (ak, sk) = self.get_provider_usage_credentials(&provider.id).await?;
                let (ak, sk) = match (ak, sk) {
                    (Some(ak), Some(sk)) if !ak.trim().is_empty() && !sk.trim().is_empty() => {
                        (ak.trim().to_string(), sk.trim().to_string())
                    }
                    _ => anyhow::bail!(
                        "Ark usage query requires Volcengine IAM AK/SK (not the inference \
                         API key): configure them in the provider edit form. Create a key \
                         at https://console.volcengine.com/iam/keymanage"
                    ),
                };

                let signed = super::volcengine_sign::sign_get(
                    &ak,
                    &sk,
                    "open.volcengineapi.com",
                    "/",
                    &[
                        ("Action".to_string(), "GetCodingPlanUsage".to_string()),
                        ("Version".to_string(), "2024-01-01".to_string()),
                    ],
                    "cn-beijing",
                    "ark",
                    chrono::Utc::now(),
                );
                let resp = self
                    .gw
                    .http_client
                    .get(&signed.url)
                    .header("Host", "open.volcengineapi.com")
                    .header("X-Date", &signed.x_date)
                    .header("X-Content-Sha256", &signed.x_content_sha256)
                    .header("Authorization", &signed.authorization)
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("usage query request failed: {e}"))?;

                let status = resp.status();
                let raw = resp
                    .bytes()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to read usage response: {e}"))?;
                let body: Value = serde_json::from_slice(&raw)
                    .map_err(|e| anyhow::anyhow!("failed to parse usage response: {e}"))?;
                if !status.is_success() {
                    // Volcengine errors carry ResponseMetadata.Error.
                    if let Some(err) = body.pointer("/ResponseMetadata/Error") {
                        let code = err.get("Code").and_then(Value::as_str).unwrap_or("");
                        let message = err.get("Message").and_then(Value::as_str).unwrap_or("");
                        anyhow::bail!("HTTP {status} (code {code}): {message}");
                    }
                    let preview: String = String::from_utf8_lossy(&raw).chars().take(200).collect();
                    anyhow::bail!("HTTP {status}: {preview}");
                }
                if let Some(err) = body.pointer("/ResponseMetadata/Error") {
                    let code = err.get("Code").and_then(Value::as_str).unwrap_or("");
                    let message = err.get("Message").and_then(Value::as_str).unwrap_or("");
                    anyhow::bail!("API error (code {code}): {message}");
                }
                let result = body.get("Result").cloned().unwrap_or(Value::Null);
                (parse_ark_tiers(&result), Vec::new(), None, None, Vec::new())
            }
        };

        Ok(ProviderUsage {
            provider_id: provider.id,
            kind: backend.kind().to_string(),
            site: backend.site().to_string(),
            level,
            tiers,
            balances,
            spends,
            is_available,
            queried_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Get the stored usage-query credentials for a provider. Two generic
    /// slots: Ark stores its IAM AK/SK pair, DeepSeek its platform userToken
    /// in slot A. Legacy `usage.ark.*` keys are read as a fallback so
    /// existing Ark setups keep working. Empty strings are normalized to
    /// `None`.
    pub async fn get_provider_usage_credentials(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let ((a_key, b_key), (legacy_a, legacy_b)) = usage_credential_keys(provider_id);
        let store = self.gw.storage.settings();
        let normalize = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let a = match normalize(store.get(&a_key).await?) {
            Some(value) => Some(value),
            None => normalize(store.get(&legacy_a).await?),
        };
        let b = match normalize(store.get(&b_key).await?) {
            Some(value) => Some(value),
            None => normalize(store.get(&legacy_b).await?),
        };
        Ok((a, b))
    }

    /// Store (or clear, when empty) the usage-query credentials for a
    /// provider.
    pub async fn set_provider_usage_credentials(
        &self,
        provider_id: &str,
        access_key: &str,
        secret_key: &str,
    ) -> anyhow::Result<()> {
        let ((a_key, b_key), _) = usage_credential_keys(provider_id);
        let store = self.gw.storage.settings();
        store.set(&a_key, access_key.trim()).await?;
        store.set(&b_key, secret_key.trim()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(name: &str, used: f64, resets_at: Option<&str>) -> ProviderUsageTier {
        ProviderUsageTier {
            name: name.to_string(),
            used_percent: used,
            resets_at: resets_at.map(str::to_string),
        }
    }

    fn data_with_limits(limits: Value) -> Value {
        serde_json::json!({ "limits": limits, "level": "max" })
    }

    #[test]
    fn glm_unit_field_overrides_reset_order_when_weekly_resets_sooner() {
        // End-of-cycle shape: the weekly bucket resets before the 5-hour one.
        // Reset-time sorting would swap the labels (cc-switch issue #3036).
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 66,
              "nextResetTime": 1787330866999_i64 },
            { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 12,
              "nextResetTime": 1786872853881_i64 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 12.0, Some("2026-08-16T09:34:13.881+00:00")),
                tier("weekly_limit", 66.0, Some("2026-08-21T16:47:46.999+00:00")),
            ]
        );
    }

    #[test]
    fn glm_old_plan_single_tier_falls_back_to_five_hour() {
        // Old plans return one entry without `unit` → heuristic fills 5h.
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "percentage": 40, "nextResetTime": 1786872853881_i64 }
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(
            tiers,
            vec![tier(
                "five_hour",
                40.0,
                Some("2026-08-16T09:34:13.881+00:00")
            )]
        );
    }

    #[test]
    fn glm_weekly_unit_six_number_one_variant() {
        // z.ai has also been observed reporting the weekly window as
        // (unit:6, number:1) — only `unit` is anchored.
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 5 },
            { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 80 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 5.0, None),
                tier("weekly_limit", 80.0, None)
            ]
        );
    }

    #[test]
    fn glm_time_limit_entries_are_skipped() {
        // Real CN-site response also carries a TIME_LIMIT call quota entry.
        let data = data_with_limits(serde_json::json!([
            { "type": "TIME_LIMIT", "unit": 5, "number": 1, "usage": 4000,
              "currentValue": 0, "remaining": 4000, "percentage": 0,
              "nextResetTime": 1789404466998_i64 },
            { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 12,
              "nextResetTime": 1786872853881_i64 },
            { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 66,
              "nextResetTime": 1787330866999_i64 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[1].name, "weekly_limit");
    }

    #[test]
    fn glm_type_matching_is_case_insensitive() {
        let data = data_with_limits(serde_json::json!([
            { "type": "tokens_limit", "unit": 3, "percentage": 10 },
            { "type": "Credit_Limit", "unit": 6, "percentage": 50 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 10.0, None),
                tier("weekly_limit", 50.0, None)
            ]
        );
    }

    #[test]
    fn glm_missing_reset_time_fills_five_hour_when_weekly_has_reset() {
        // Heuristic: no-reset entries prefer the five-hour slot.
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "percentage": 0 },
            { "type": "TOKENS_LIMIT", "percentage": 30, "nextResetTime": 1787330866999_i64 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[0].used_percent, 0.0);
        assert_eq!(tiers[1].name, "weekly_limit");
        assert!(tiers[1].resets_at.is_some());
    }

    #[test]
    fn glm_unknown_unit_falls_back_to_reset_order() {
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "unit": 9, "percentage": 20,
              "nextResetTime": 1786872853881_i64 },
            { "type": "TOKENS_LIMIT", "unit": 9, "percentage": 70,
              "nextResetTime": 1787330866999_i64 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[0].used_percent, 20.0);
        assert_eq!(tiers[1].name, "weekly_limit");
        assert_eq!(tiers[1].used_percent, 70.0);
    }

    #[test]
    fn glm_more_than_two_token_limits_keeps_first_two() {
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "unit": 3, "percentage": 1 },
            { "type": "TOKENS_LIMIT", "unit": 6, "percentage": 2 },
            { "type": "TOKENS_LIMIT", "unit": 3, "percentage": 3 },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 1.0, None),
                tier("weekly_limit", 2.0, None)
            ]
        );
    }

    #[test]
    fn glm_no_token_limits_returns_empty() {
        let data = data_with_limits(serde_json::json!([]));
        assert!(parse_glm_quota_tiers(&data).is_empty());
        assert!(parse_glm_quota_tiers(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn glm_invalid_percentage_falls_back_to_zero() {
        let data = data_with_limits(serde_json::json!([
            { "type": "TOKENS_LIMIT", "unit": 3, "percentage": "not-a-number" },
        ]));
        let tiers = parse_glm_quota_tiers(&data);
        assert_eq!(tiers, vec![tier("five_hour", 0.0, None)]);
    }

    #[test]
    fn usage_backend_detect_routes_by_base_url() {
        use UsageBackend::*;
        let detect = |url: &str| UsageBackend::detect(url);
        assert_eq!(
            detect("https://open.bigmodel.cn/api/coding/paas/v4"),
            Some(GlmCn)
        );
        assert_eq!(
            detect("https://OPEN.BigModel.cn/api/anthropic"),
            Some(GlmCn)
        );
        assert_eq!(
            detect("https://api.z.ai/api/coding/paas/v4"),
            Some(GlmGlobal)
        );
        assert_eq!(
            detect("https://api.minimaxi.com/anthropic"),
            Some(MinimaxCn)
        );
        assert_eq!(detect("https://api.minimax.io/v1"), Some(MinimaxGlobal));
        assert_eq!(detect("https://api.kimi.com/coding"), Some(KimiCode));
        assert_eq!(detect("https://api.kimi.com/coding/v1"), Some(KimiCode));
        assert_eq!(detect("https://api.deepseek.com"), Some(DeepSeek));
        assert_eq!(detect("https://api.deepseek.com/anthropic"), Some(DeepSeek));
        assert_eq!(detect("https://opencode.ai/zen/go"), Some(OpencodeGo));
        assert_eq!(detect("https://opencode.ai/zen/go/v1"), Some(OpencodeGo));
        assert_eq!(detect("https://api.example.com/v1"), None);

        // Bigmodel must win over z.ai when both substrings appear (CN URLs
        // can carry z.ai in a path); minimaxi.com (CN) before minimax.io.
        assert_eq!(detect("https://open.bigmodel.cn/r/z.ai"), Some(GlmCn));

        assert_eq!(GlmCn.kind(), "glm_coding_plan");
        assert_eq!(GlmGlobal.kind(), "glm_coding_plan");
        assert_eq!(MinimaxCn.kind(), "minimax_coding_plan");
        assert_eq!(MinimaxGlobal.kind(), "minimax_coding_plan");
        assert_eq!(KimiCode.kind(), "kimi_coding_plan");
        assert_eq!(DeepSeek.kind(), "deepseek_balance");
        assert_eq!(OpencodeGo.kind(), "opencode_go");
        assert_eq!(KimiCode.site(), "global");
        assert_eq!(DeepSeek.site(), "global");
        assert_eq!(MinimaxCn.site(), "cn");
        assert_eq!(MinimaxGlobal.site(), "global");
    }

    fn minimax_body(
        general: Value,
        weekly_status: i64,
        weekly_remaining: Option<f64>,
        weekly_end: Option<i64>,
    ) -> Value {
        let mut item = serde_json::Map::new();
        item.insert("model_name".into(), Value::String("general".into()));
        for (k, v) in general.as_object().into_iter().flatten() {
            item.insert(k.clone(), v.clone());
        }
        item.insert(
            "current_weekly_status".into(),
            Value::Number(weekly_status.into()),
        );
        if let Some(remain) = weekly_remaining {
            item.insert(
                "current_weekly_remaining_percent".into(),
                serde_json::json!(remain),
            );
        }
        if let Some(end) = weekly_end {
            item.insert("weekly_end_time".into(), serde_json::json!(end));
        }
        serde_json::json!({
            "model_remains": [
                Value::Object(item),
                { "model_name": "video",
                  "current_interval_remaining_percent": 55.0,
                  "end_time": 1786872853881_i64 },
            ],
            "base_resp": { "status_code": 0, "status_msg": "success" },
        })
    }

    #[test]
    fn minimax_general_only_and_remaining_inverted_to_used() {
        // The `video` entry must be ignored; remaining 87.5 → used 12.5.
        let body = minimax_body(
            serde_json::json!({
                "current_interval_remaining_percent": 87.5,
                "end_time": 1786872853881_i64,
            }),
            1,
            Some(34.0),
            Some(1787330866999_i64),
        );
        let tiers = parse_minimax_tiers(&body);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 12.5, Some("2026-08-16T09:34:13.881+00:00")),
                tier("weekly_limit", 66.0, Some("2026-08-21T16:47:46.999+00:00")),
            ]
        );
    }

    #[test]
    fn minimax_weekly_status_three_hides_weekly_bucket() {
        // Plans without a weekly limit report status 3 with remaining pinned
        // at 100 — the weekly tier must not be shown.
        let body = minimax_body(
            serde_json::json!({
                "current_interval_remaining_percent": 90.0,
                "end_time": 1786872853881_i64,
            }),
            3,
            Some(100.0),
            Some(1787330866999_i64),
        );
        let tiers = parse_minimax_tiers(&body);
        assert_eq!(
            tiers,
            vec![tier(
                "five_hour",
                10.0,
                Some("2026-08-16T09:34:13.881+00:00")
            )]
        );
    }

    #[test]
    fn minimax_no_weekly_fields_returns_five_hour_only() {
        let body = minimax_body(
            serde_json::json!({
                "current_interval_remaining_percent": 100.0,
                "end_time": 1786872853881_i64,
            }),
            1,
            None,
            None,
        );
        let tiers = parse_minimax_tiers(&body);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[0].used_percent, 0.0);
    }

    #[test]
    fn minimax_missing_general_entry_returns_empty() {
        let body = serde_json::json!({
            "model_remains": [
                { "model_name": "video", "current_interval_remaining_percent": 55.0 },
            ],
        });
        assert!(parse_minimax_tiers(&body).is_empty());
        // Missing model_remains entirely.
        assert!(parse_minimax_tiers(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn minimax_missing_interval_fields_returns_empty() {
        // No 5-hour remaining percent → nothing to report (weekly still
        // gated behind the always-present interval bucket semantics in
        // practice; cc-switch likewise only emits tiers per present field).
        let body = minimax_body(
            serde_json::json!({ "end_time": 1786872853881_i64 }),
            1,
            Some(30.0),
            Some(1787330866999_i64),
        );
        let tiers = parse_minimax_tiers(&body);
        assert_eq!(
            tiers,
            vec![tier(
                "weekly_limit",
                70.0,
                Some("2026-08-21T16:47:46.999+00:00")
            )]
        );
    }

    #[test]
    fn kimi_absolute_values_become_percentages() {
        // limits[].detail → five_hour; usage → weekly_limit. Absolute
        // limit/remaining values: (120-30)/120 = 75%, (2000-500)/2000 = 75%.
        let body = serde_json::json!({
            "limits": [
                { "detail": { "limit": 120.0, "remaining": 30.0,
                              "resetTime": 1786872853881_i64 } },
            ],
            "usage": { "limit": 2000.0, "remaining": 500.0,
                       "resetTime": 1787330866999_i64 },
        });
        let tiers = parse_kimi_tiers(&body);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 75.0, Some("2026-08-16T09:34:13.881+00:00")),
                tier("weekly_limit", 75.0, Some("2026-08-21T16:47:46.999+00:00")),
            ]
        );
    }

    #[test]
    fn kimi_numeric_strings_and_second_epochs() {
        // limit/remaining may be numeric strings; resetTime may be a
        // seconds epoch or an ISO string.
        let body = serde_json::json!({
            "limits": [
                { "detail": { "limit": "100", "remaining": "25", "resetTime": 1786872853 } },
            ],
            "usage": { "limit": 400.0, "remaining": 100.0,
                       "resetTime": "2026-08-21T16:47:46.999Z" },
        });
        let tiers = parse_kimi_tiers(&body);
        assert_eq!(tiers[0].used_percent, 75.0);
        assert_eq!(
            tiers[0].resets_at.as_deref(),
            Some("2026-08-16T09:34:13+00:00")
        );
        assert_eq!(tiers[1].used_percent, 75.0);
        assert_eq!(
            tiers[1].resets_at.as_deref(),
            Some("2026-08-21T16:47:46.999Z")
        );
    }

    #[test]
    fn kimi_zero_limit_reports_zero_used() {
        let body = serde_json::json!({
            "limits": [ { "detail": { "limit": 0, "remaining": 0 } } ],
            "usage": { "limit": 0.0, "remaining": 0.0 },
        });
        let tiers = parse_kimi_tiers(&body);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 0.0, None),
                tier("weekly_limit", 0.0, None)
            ]
        );
    }

    #[test]
    fn kimi_overconsumed_remaining_clamps_to_full() {
        // remaining > limit must not go negative.
        let body = serde_json::json!({
            "usage": { "limit": 100.0, "remaining": 150.0 },
        });
        let tiers = parse_kimi_tiers(&body);
        assert_eq!(tiers, vec![tier("weekly_limit", 0.0, None)]);
    }

    #[test]
    fn kimi_empty_body_returns_empty() {
        assert!(parse_kimi_tiers(&serde_json::json!({})).is_empty());
        // A limits entry without detail contributes nothing.
        assert!(parse_kimi_tiers(&serde_json::json!({ "limits": [ { "other": 1 } ] })).is_empty());
    }

    #[test]
    fn deepseek_balances_parse_numeric_strings_per_currency() {
        // Real response shape (observed 2026-08-16): values are numeric
        // strings, one entry per currency.
        let body = serde_json::json!({
            "is_available": true,
            "balance_infos": [
                { "currency": "CNY", "total_balance": "93.72",
                   "granted_balance": "0.00", "topped_up_balance": "93.72" },
                { "currency": "USD", "total_balance": "12.5",
                   "granted_balance": "2.5", "topped_up_balance": "10.0" },
            ],
        });
        let (balances, is_available) = parse_deepseek_balances(&body);
        assert_eq!(is_available, Some(true));
        assert_eq!(
            balances,
            vec![
                ProviderUsageBalance {
                    currency: "CNY".into(),
                    total: 93.72,
                    granted: 0.0,
                    topped_up: 93.72,
                },
                ProviderUsageBalance {
                    currency: "USD".into(),
                    total: 12.5,
                    granted: 2.5,
                    topped_up: 10.0,
                },
            ]
        );
    }

    #[test]
    fn deepseek_balances_defaults_and_empty() {
        // Missing is_available defaults to None; missing currency falls
        // back to CNY; missing numeric fields fall back to 0.
        let (balances, is_available) =
            parse_deepseek_balances(&serde_json::json!({ "balance_infos": [{}] }));
        assert_eq!(is_available, None);
        assert_eq!(
            balances,
            vec![ProviderUsageBalance {
                currency: "CNY".into(),
                total: 0.0,
                granted: 0.0,
                topped_up: 0.0,
            }]
        );
        let (empty, avail) = parse_deepseek_balances(&serde_json::json!({}));
        assert!(empty.is_empty());
        assert_eq!(avail, None);
    }

    #[test]
    fn deepseek_balances_unavailable_flag_passes_through() {
        let (balances, is_available) = parse_deepseek_balances(&serde_json::json!({
            "is_available": false,
            "balance_infos": [
                { "currency": "CNY", "total_balance": "0", "granted_balance": "0",
                  "topped_up_balance": "0" },
            ],
        }));
        assert_eq!(is_available, Some(false));
        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].total, 0.0);
    }

    #[test]
    fn deepseek_spends_parse_today_and_month() {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let body = serde_json::json!({
            "code": 0,
            "data": { "biz_code": 0, "biz_data": { "days": [
                { "date": "2026-08-01", "data": [
                    { "usage": [ { "cost": "1.25" } ] },
                ]},
                { "date": today, "data": [
                    { "usage": [ { "cost": "0.5" }, { "amount": "0.25" } ] },
                    { "usage": [ { "cost": "0.1" } ] },
                ]},
            ]}},
        });
        let spends = parse_deepseek_spends(&body, "CNY");
        assert_eq!(spends.len(), 2);
        assert_eq!(spends[0].name, "today");
        assert_eq!(spends[0].amount, 0.85); // 0.5 + 0.25 + 0.1
        assert_eq!(spends[0].currency, "CNY");
        assert_eq!(spends[1].name, "month");
        assert_eq!(spends[1].amount, 2.10); // 1.25 + 0.85
    }

    #[test]
    fn deepseek_spends_without_today_row_only_month() {
        let body = serde_json::json!({
            "code": 0,
            "data": { "biz_code": 0, "biz_data": { "days": [
                { "date": "2026-08-01", "data": [
                    { "usage": [ { "cost": 3 } ] },
                ]},
            ]}},
        });
        let spends = parse_deepseek_spends(&body, "CNY");
        assert_eq!(spends.len(), 1);
        assert_eq!(spends[0].name, "month");
        assert_eq!(spends[0].amount, 3.0);
    }

    #[test]
    fn deepseek_spends_bad_envelope_is_empty() {
        // Non-zero envelope codes (e.g. expired token 40002) yield no rows —
        // the caller silently omits spend detail.
        for body in [
            serde_json::json!({ "code": 40002 }),
            serde_json::json!({ "code": 0, "data": { "biz_code": 1 } }),
            serde_json::json!({ "code": 0, "data": { "biz_code": 0, "biz_data": {} } }),
        ] {
            assert!(parse_deepseek_spends(&body, "CNY").is_empty());
        }
    }

    #[test]
    fn deepseek_spends_accepts_biz_data_array() {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let body = serde_json::json!({
            "code": 0,
            "data": { "biz_code": 0, "biz_data": [
                { "days": [ { "date": today, "data": [ { "usage": [ { "cost": 2 } ] } ] } ] },
            ]},
        });
        let spends = parse_deepseek_spends(&body, "CNY");
        assert_eq!(spends.len(), 2);
        assert_eq!(spends[0].amount, 2.0);
        assert_eq!(spends[1].amount, 2.0);
    }

    #[test]
    fn opencode_three_windows_pass_through_percentages() {
        // Real response shape (observed 2026-08-16): percent is already a
        // used percentage, resetsAt already ISO — both passed through, in
        // rolling → weekly → monthly order.
        let body = serde_json::json!({
            "usage": {
                "rolling": { "status": "ok", "percent": 0,
                             "resetsAt": "2026-08-16T14:04:53.827Z" },
                "weekly": { "status": "ok", "percent": 63,
                            "resetsAt": "2026-08-17T00:00:00.827Z" },
                "monthly": { "status": "ok", "percent": 44,
                             "resetsAt": "2026-09-08T15:13:38.827Z" },
            },
        });
        let tiers = parse_opencode_tiers(&body);
        assert_eq!(
            tiers,
            vec![
                tier("five_hour", 0.0, Some("2026-08-16T14:04:53.827Z")),
                tier("weekly_limit", 63.0, Some("2026-08-17T00:00:00.827Z")),
                tier("monthly", 44.0, Some("2026-09-08T15:13:38.827Z")),
            ]
        );
    }

    #[test]
    fn ark_tiers_parse_level_percent_and_reset() {
        // GetCodingPlanUsage Result shape (per ark-cli / cc-switch): windows
        // carry Level / Percent / ResetTimestamp (unix seconds, <=0 dropped).
        let result = serde_json::json!({
            "Usage": [
                { "Level": "weekly",  "Percent": 42.5, "ResetTimestamp": 1784848000 },
                { "Level": "5h",      "Percent": 10,   "ResetTimestamp": 1784784000 },
                { "Level": "monthly", "Percent": 7,    "ResetTimestamp": 0 },
                { "Level": "quarter", "Percent": 3,    "ResetTimestamp": 1784848000 },
            ]
        });
        let tiers = parse_ark_tiers(&result);
        assert_eq!(tiers.len(), 3);
        // Stable order five_hour → weekly_limit → monthly regardless of input.
        assert_eq!(tiers[0].name, "five_hour");
        assert_eq!(tiers[0].used_percent, 10.0);
        assert_eq!(tiers[1].name, "weekly_limit");
        assert_eq!(tiers[1].used_percent, 42.5);
        assert_eq!(tiers[2].name, "monthly");
        // ResetTimestamp <= 0 → no resets_at.
        assert_eq!(tiers[2].resets_at, None);
        assert!(
            tiers[0]
                .resets_at
                .as_deref()
                .unwrap_or("")
                .starts_with("2026-")
        );
    }

    #[test]
    fn ark_detect_and_kind() {
        use UsageBackend::ArkCoding;
        let detect = |url: &str| UsageBackend::detect(url);
        assert_eq!(
            detect("https://ark.cn-beijing.volces.com/api/coding"),
            Some(ArkCoding)
        );
        assert_eq!(ArkCoding.kind(), "ark_coding_plan");
        assert_eq!(ArkCoding.site(), "cn");
    }

    #[test]
    fn opencode_non_ok_windows_are_skipped() {
        // Windows whose status is not "ok" (e.g. exhausted / unknown) are
        // not emitted; missing windows are simply absent.
        let body = serde_json::json!({
            "usage": {
                "rolling": { "status": "blocked", "percent": 100,
                             "resetsAt": "2026-08-16T14:04:53.827Z" },
                "weekly": { "status": "ok", "percent": 63,
                            "resetsAt": "2026-08-17T00:00:00.827Z" },
            },
        });
        let tiers = parse_opencode_tiers(&body);
        assert_eq!(
            tiers,
            vec![tier("weekly_limit", 63.0, Some("2026-08-17T00:00:00.827Z"))]
        );
    }

    #[test]
    fn opencode_missing_percent_or_usage_returns_empty() {
        // status ok but no numeric percent → window skipped (filter_map).
        let no_percent = serde_json::json!({
            "usage": { "rolling": { "status": "ok", "resetsAt": "x" } },
        });
        assert!(parse_opencode_tiers(&no_percent).is_empty());
        // No usage object at all.
        assert!(parse_opencode_tiers(&serde_json::json!({})).is_empty());
    }
}
