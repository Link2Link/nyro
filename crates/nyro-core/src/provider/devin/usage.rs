//! Plan, quota-window and extra-usage readout decoded from `GetUserStatus`.
//!
//! The same unary RPC backs [`super::check_connection`]. Field numbers come
//! from the Devin CLI's own response as observed in oh-my-pi, openusage and a
//! live Pro account; only the subset needed here is declared. Unknown fields
//! are skipped by prost, so upstream additions are harmless — but a *declared*
//! field arriving with a different wire type fails the whole decode, which is
//! deliberate: a schema drift must surface as a query error rather than be
//! misread as "quota exhausted" and pause routing.
//!
//! Wire facts that shape the decoding:
//!
//! * Quota percentages are **remaining**, not used.
//! * proto3 omits zero values, so "0% remaining" is indistinguishable from
//!   "no data" on the wire. A window therefore *applies* only when it is not
//!   hidden by the plan and is either server-dated (reset > 0) or the plan is
//!   quota-billed; an applying window whose percentage is absent is exhausted.
//! * Negative percentages are sentinels (credit buckets use `-1` for
//!   "unlimited / no bucket"): such a window is skipped, never read as spent.
//! * Reset instants are Unix seconds; `0` means unknown.
//! * The extra-usage (overage) balance is in micro-dollars and is what keeps
//!   an account usable after a window is spent.
use prost::Message;

/// `plan_info.billing_strategy` value for quota-billed (daily/weekly) plans.
const BILLING_STRATEGY_QUOTA: i32 = 2;
const MICROS_PER_DOLLAR: f64 = 1_000_000.0;

#[derive(Clone, PartialEq, Message)]
pub(super) struct StatusResponse {
    #[prost(message, optional, tag = "1")]
    pub user: Option<UserStatus>,
    /// Authoritative plan description (a copy also nests under `plan_status`).
    #[prost(message, optional, tag = "2")]
    pub plan: Option<PlanInfo>,
}

/// Personal fields (name, e-mail, user and org ids) are deliberately not
/// declared: they are never needed and prost simply skips them.
#[derive(Clone, PartialEq, Message)]
pub(super) struct UserStatus {
    #[prost(message, optional, tag = "13")]
    pub plan_status: Option<PlanStatus>,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct PlanInfo {
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int32, tag = "35")]
    pub billing_strategy: i32,
    #[prost(bool, tag = "36")]
    pub hide_daily_quota: bool,
    #[prost(bool, tag = "37")]
    pub hide_weekly_quota: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct PlanStatus {
    /// Fallback copy of the plan description.
    #[prost(message, optional, tag = "1")]
    pub plan: Option<PlanInfo>,
    #[prost(int32, optional, tag = "14")]
    pub daily_remaining_percent: Option<i32>,
    #[prost(int32, optional, tag = "15")]
    pub weekly_remaining_percent: Option<i32>,
    /// Extra-usage balance in micro-dollars.
    #[prost(int64, tag = "16")]
    pub overage_balance_micros: i64,
    #[prost(int64, tag = "17")]
    pub daily_reset_unix: i64,
    #[prost(int64, tag = "18")]
    pub weekly_reset_unix: i64,
}

/// One included quota window.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DevinQuota {
    /// Percentage still available, `0..=100`.
    pub remaining_percent: u8,
    /// Reset instant as Unix seconds, when the upstream dated it.
    pub resets_at_unix: Option<i64>,
}

/// Everything the usage panel and the scheduler need from one status call.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DevinUsage {
    /// Upstream plan name (e.g. `Pro`).
    pub plan_name: Option<String>,
    pub daily: Option<DevinQuota>,
    pub weekly: Option<DevinQuota>,
    /// Extra-usage balance in USD, present when the upstream reports one or
    /// the plan is quota-billed (where `$0.00` is itself meaningful).
    pub overage_usd: Option<f64>,
}

/// Pool classification is deliberately binary: only an explicit Max label is
/// Max; Pro, Team full seats and every other successful status label use Pro.
pub(crate) fn classify_plan_name(name: Option<&str>) -> crate::admin::DevinPlan {
    match name.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "max" | "devin max" => crate::admin::DevinPlan::Max,
        _ => crate::admin::DevinPlan::Pro,
    }
}

impl DevinUsage {
    /// Max has no daily allowance. Remove proto default/legacy daily artifacts
    /// from the pool view without inventing any weekly remaining percentage.
    pub(crate) fn normalize_for_pool(&mut self) -> crate::admin::DevinPlan {
        let plan = classify_plan_name(self.plan_name.as_deref());
        if plan == crate::admin::DevinPlan::Max {
            self.daily = None;
        }
        plan
    }
}

fn window(
    remaining: Option<i32>,
    reset_unix: i64,
    hidden: bool,
    quota_billed: bool,
) -> Option<DevinQuota> {
    if hidden || !(reset_unix > 0 || quota_billed) {
        return None;
    }
    // Absent == proto3 zero == nothing remaining; a negative value is a
    // "no limit" sentinel and carries no quota window at all.
    let remaining = remaining.unwrap_or(0);
    if remaining < 0 {
        return None;
    }
    Some(DevinQuota {
        remaining_percent: remaining.min(100) as u8,
        resets_at_unix: (reset_unix > 0).then_some(reset_unix),
    })
}

/// Build the usage snapshot from a decoded status response. Errors when the
/// response carries no user status, so an empty or unrecognised reply is never
/// mistaken for an unlimited account.
pub(super) fn snapshot(response: StatusResponse) -> anyhow::Result<DevinUsage> {
    let user = response
        .user
        .ok_or_else(|| anyhow::anyhow!("Devin status response contains no user status"))?;
    let status = user.plan_status.unwrap_or_default();
    let plan_name = response
        .plan
        .as_ref()
        .into_iter()
        .chain(status.plan.as_ref())
        .map(|plan| plan.name.trim())
        .find(|name| !name.is_empty())
        .map(str::to_owned);
    let plan = response.plan.or(status.plan.clone()).unwrap_or_default();
    let quota_billed = plan.billing_strategy == BILLING_STRATEGY_QUOTA;

    let daily = window(
        status.daily_remaining_percent,
        status.daily_reset_unix,
        plan.hide_daily_quota,
        quota_billed,
    );
    let weekly = window(
        status.weekly_remaining_percent,
        status.weekly_reset_unix,
        plan.hide_weekly_quota,
        quota_billed,
    );
    // A negative balance is a debt, not spendable headroom.
    let overage_usd = (status.overage_balance_micros > 0 || quota_billed)
        .then(|| status.overage_balance_micros.max(0) as f64 / MICROS_PER_DOLLAR);

    Ok(DevinUsage {
        plan_name,
        daily,
        weekly,
        overage_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode raw bytes exactly as the transport does, then snapshot.
    fn parse(bytes: &[u8]) -> anyhow::Result<DevinUsage> {
        snapshot(
            StatusResponse::decode(bytes)
                .map_err(|_| anyhow::anyhow!("Invalid Devin status response"))?,
        )
    }

    fn exhausted_window_exists(usage: &DevinUsage) -> bool {
        [&usage.daily, &usage.weekly]
            .into_iter()
            .flatten()
            .any(|quota| quota.remaining_percent == 0)
    }

    const DAILY_RESET: i64 = 1_792_137_600;
    const WEEKLY_RESET: i64 = 1_792_310_400;

    fn plan(name: &str, billing: i32) -> PlanInfo {
        PlanInfo {
            name: name.into(),
            billing_strategy: billing,
            ..Default::default()
        }
    }

    fn response(plan: PlanInfo, status: PlanStatus) -> StatusResponse {
        StatusResponse {
            user: Some(UserStatus {
                plan_status: Some(status),
            }),
            plan: Some(plan),
        }
    }

    /// The shape observed from a live quota-billed Pro account.
    fn live_pro_status() -> PlanStatus {
        PlanStatus {
            daily_remaining_percent: Some(83),
            weekly_remaining_percent: Some(91),
            overage_balance_micros: 10_000_000,
            daily_reset_unix: DAILY_RESET,
            weekly_reset_unix: WEEKLY_RESET,
            ..Default::default()
        }
    }

    #[test]
    fn decodes_the_live_pro_shape_through_the_wire() {
        let bytes =
            response(plan("Pro", BILLING_STRATEGY_QUOTA), live_pro_status()).encode_to_vec();

        let usage = parse(&bytes).unwrap();

        assert_eq!(usage.plan_name.as_deref(), Some("Pro"));
        assert_eq!(
            usage.daily,
            Some(DevinQuota {
                remaining_percent: 83,
                resets_at_unix: Some(DAILY_RESET)
            })
        );
        assert_eq!(
            usage.weekly,
            Some(DevinQuota {
                remaining_percent: 91,
                resets_at_unix: Some(WEEKLY_RESET)
            })
        );
        assert_eq!(usage.overage_usd, Some(10.0));
        assert!(!exhausted_window_exists(&usage));
        assert_eq!(usage.overage_usd, Some(10.0));
    }

    #[test]
    fn plan_name_falls_back_to_the_plan_status_copy() {
        let mut status = live_pro_status();
        status.plan = Some(plan("Pro", BILLING_STRATEGY_QUOTA));
        let usage = snapshot(StatusResponse {
            user: Some(UserStatus {
                plan_status: Some(status),
            }),
            plan: None,
        })
        .unwrap();

        assert_eq!(usage.plan_name.as_deref(), Some("Pro"));
        assert!(usage.daily.is_some());
    }

    #[test]
    fn blank_outer_plan_name_uses_nested_max_without_discarding_quota_fields() {
        let mut status = live_pro_status();
        status.plan = Some(plan("Max", BILLING_STRATEGY_QUOTA));
        let mut usage = snapshot(response(plan("  ", BILLING_STRATEGY_QUOTA), status)).unwrap();
        assert_eq!(usage.plan_name.as_deref(), Some("Max"));
        assert_eq!(usage.normalize_for_pool(), crate::admin::DevinPlan::Max);
        assert!(usage.daily.is_none());
        assert_eq!(usage.weekly.as_ref().unwrap().remaining_percent, 91);
    }

    #[test]
    fn only_max_removes_daily_and_non_max_names_keep_pro_observations() {
        for name in [
            None,
            Some(""),
            Some("Pro"),
            Some("Team Full Seat"),
            Some("unknown"),
        ] {
            assert_eq!(classify_plan_name(name), crate::admin::DevinPlan::Pro);
        }
        for name in ["Max", " max ", " DEVIN MAX "] {
            assert_eq!(classify_plan_name(Some(name)), crate::admin::DevinPlan::Max);
        }
        let mut pro = snapshot(response(
            plan("Team Full Seat", BILLING_STRATEGY_QUOTA),
            live_pro_status(),
        ))
        .unwrap();
        assert_eq!(pro.normalize_for_pool(), crate::admin::DevinPlan::Pro);
        assert!(pro.daily.is_some());
        pro.plan_name = Some("Max".into());
        pro.weekly = None;
        pro.normalize_for_pool();
        assert!(pro.daily.is_none() && pro.weekly.is_none());
    }

    #[test]
    fn missing_percentage_on_a_dated_window_means_exhausted() {
        // proto3 drops a zero: "0% remaining" arrives with no percentage field.
        let mut status = live_pro_status();
        status.daily_remaining_percent = None;
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.daily.as_ref().unwrap().remaining_percent, 0);
        assert!(exhausted_window_exists(&usage));
        assert_eq!(usage.weekly.as_ref().unwrap().remaining_percent, 91);
    }

    #[test]
    fn explicit_zero_percentage_is_also_exhausted() {
        let mut status = live_pro_status();
        status.weekly_remaining_percent = Some(0);
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.weekly.as_ref().unwrap().remaining_percent, 0);
        assert!(exhausted_window_exists(&usage));
    }

    #[test]
    fn hidden_windows_are_dropped_even_when_dated() {
        let mut info = plan("Pro", BILLING_STRATEGY_QUOTA);
        info.hide_daily_quota = true;
        info.hide_weekly_quota = true;
        let usage = snapshot(response(info, live_pro_status())).unwrap();

        assert_eq!(usage.daily, None);
        assert_eq!(usage.weekly, None);
        assert!(!exhausted_window_exists(&usage));
    }

    #[test]
    fn credit_plans_do_not_invent_quota_windows_from_proto_defaults() {
        // Credit-billed: percentages sit at proto defaults, nothing is dated.
        let usage = snapshot(response(plan("Teams", 1), PlanStatus::default())).unwrap();

        assert_eq!(usage.daily, None);
        assert_eq!(usage.weekly, None);
        assert_eq!(usage.overage_usd, None);
        assert!(!exhausted_window_exists(&usage));
    }

    #[test]
    fn negative_percentage_is_an_unlimited_sentinel_not_an_exhausted_window() {
        let mut status = live_pro_status();
        status.daily_remaining_percent = Some(-1);
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.daily, None);
        assert!(!exhausted_window_exists(&usage));
    }

    #[test]
    fn out_of_range_percentage_is_clamped() {
        let mut status = live_pro_status();
        status.daily_remaining_percent = Some(250);
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.daily.as_ref().unwrap().remaining_percent, 100);
    }

    #[test]
    fn undated_quota_window_keeps_an_unknown_reset() {
        let mut status = live_pro_status();
        status.weekly_reset_unix = 0;
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.weekly.as_ref().unwrap().resets_at_unix, None);
    }

    #[test]
    fn quota_plan_without_extra_balance_reports_an_explicit_zero() {
        let mut status = live_pro_status();
        status.overage_balance_micros = 0;
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.overage_usd, Some(0.0));
    }

    #[test]
    fn negative_extra_balance_is_a_debt_and_never_available() {
        let mut status = live_pro_status();
        status.overage_balance_micros = -2_500_000;
        let usage = snapshot(response(plan("Pro", BILLING_STRATEGY_QUOTA), status)).unwrap();

        assert_eq!(usage.overage_usd, Some(0.0));
    }

    #[test]
    fn blank_plan_name_is_omitted() {
        let usage = snapshot(response(
            plan("   ", BILLING_STRATEGY_QUOTA),
            live_pro_status(),
        ))
        .unwrap();

        assert_eq!(usage.plan_name, None);
    }

    #[test]
    fn missing_user_status_is_an_error_not_an_unlimited_account() {
        let bytes = StatusResponse {
            user: None,
            plan: Some(plan("Pro", BILLING_STRATEGY_QUOTA)),
        }
        .encode_to_vec();

        assert!(parse(&bytes).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn malformed_bytes_never_leak_into_the_error() {
        let error = parse(b"\xff\xff\xffsecret-token").unwrap_err().to_string();

        assert_eq!(error, "Invalid Devin status response");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let mut bytes =
            response(plan("Pro", BILLING_STRATEGY_QUOTA), live_pro_status()).encode_to_vec();
        // Append an undeclared varint field 99 and a string field 98.
        bytes.extend_from_slice(&[0x98, 0x06, 0x01, 0x92, 0x06, 0x02, b'h', b'i']);

        assert_eq!(parse(&bytes).unwrap().plan_name.as_deref(), Some("Pro"));
    }

    #[test]
    fn a_wire_type_mismatch_on_a_declared_field_fails_closed() {
        // Field 14 (daily remaining %) arriving as a string must not be read
        // as "exhausted"; the whole decode errors instead.
        let mut status = Vec::new();
        status.extend_from_slice(&[0x72, 0x02, b'4', b'2']); // tag 14, wire type 2
        let mut user = Vec::new();
        user.extend_from_slice(&[0x6a, status.len() as u8]); // user_status.f13
        user.extend_from_slice(&status);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x0a, user.len() as u8]); // response.f1
        bytes.extend_from_slice(&user);

        assert!(parse(&bytes).is_err());
    }
}
