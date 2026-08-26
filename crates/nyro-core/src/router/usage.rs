//! Provider quota scoring for the `usage` model-balance strategy.
//!
//! The formulas intentionally mirror the steady-pace marker already rendered by
//! the WebUI: fixed 5h / 7d / 30d windows are reconstructed backwards from the
//! upstream reset time. A provider with only a five-hour tier belongs to the
//! short-cycle priority pool; once a weekly or monthly tier exists, five-hour
//! usage is ignored for soft scoring and the long-window bottleneck wins.

use chrono::{DateTime, NaiveDate, Utc};

use super::quota::QuotaTierObservation;

pub(crate) const TIER_FIVE_HOUR: &str = "five_hour";
pub(crate) const TIER_WEEKLY_LIMIT: &str = "weekly_limit";
pub(crate) const TIER_MONTHLY: &str = "monthly";

const FIVE_HOURS_MS: f64 = 5.0 * 60.0 * 60.0 * 1000.0;
const SEVEN_DAYS_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const THIRTY_DAYS_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageScorePool {
    FiveHourOnly,
    LongTerm,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ProviderUsageScore {
    pub pool: UsageScorePool,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TierUsageScore {
    pub score: f64,
    pub steady_pace_used_percent: Option<f64>,
}

pub(crate) fn dynamic_weight(score: f64) -> f64 {
    if score.is_finite() && score > 0.0 {
        score * score
    } else {
        0.0
    }
}

/// Produce one provider score from the canonical main quota tiers.
///
/// - weekly/monthly present: ignore five-hour and take the long-window minimum;
/// - only five-hour present: use the five-hour minimum;
/// - no canonical percentage window: unscored (unknown fallback).
pub(crate) fn score_provider(
    tiers: &[QuotaTierObservation],
    now: DateTime<Utc>,
) -> Option<ProviderUsageScore> {
    let has_long_window = tiers
        .iter()
        .any(|tier| matches!(tier.name.as_str(), TIER_WEEKLY_LIMIT | TIER_MONTHLY));
    if has_long_window {
        return tiers
            .iter()
            .filter(|tier| matches!(tier.name.as_str(), TIER_WEEKLY_LIMIT | TIER_MONTHLY))
            .filter_map(|tier| score_tier(tier, now).map(|score| score.score))
            .reduce(f64::min)
            .map(|score| ProviderUsageScore {
                pool: UsageScorePool::LongTerm,
                score,
            });
    }

    tiers
        .iter()
        .filter(|tier| tier.name == TIER_FIVE_HOUR)
        .filter_map(|tier| score_tier(tier, now).map(|score| score.score))
        .reduce(f64::min)
        .map(|score| ProviderUsageScore {
            pool: UsageScorePool::FiveHourOnly,
            score,
        })
}

/// Score a canonical main quota window on a common 0..=100 scale.
///
/// With a reset time, 50 means exactly on the ideal steady-consumption line;
/// without one, the raw remaining percentage is used. Reset times in the past
/// deliberately clamp the ideal line to 100 because last-good snapshots remain
/// authoritative until a later successful refresh replaces them.
pub(crate) fn score_tier(
    tier: &QuotaTierObservation,
    now: DateTime<Utc>,
) -> Option<TierUsageScore> {
    let window_ms = tier_window_ms(&tier.name)?;
    if !tier.used_percent.is_finite() {
        return None;
    }
    let used = tier.used_percent.clamp(0.0, 100.0);
    let steady_pace = tier
        .resets_at
        .as_deref()
        .and_then(parse_reset_at)
        .map(|reset| {
            let remaining_ms = reset.signed_duration_since(now).num_milliseconds() as f64;
            if remaining_ms <= 0.0 {
                100.0
            } else if remaining_ms >= window_ms {
                0.0
            } else {
                ((window_ms - remaining_ms) / window_ms) * 100.0
            }
        });
    let score = steady_pace
        .map(|pace| 50.0 + (pace - used) / 2.0)
        .unwrap_or(100.0 - used)
        .clamp(0.0, 100.0);
    Some(TierUsageScore {
        score,
        steady_pace_used_percent: steady_pace,
    })
}

fn tier_window_ms(name: &str) -> Option<f64> {
    match name {
        TIER_FIVE_HOUR => Some(FIVE_HOURS_MS),
        TIER_WEEKLY_LIMIT => Some(SEVEN_DAYS_MS),
        TIER_MONTHLY => Some(THIRTY_DAYS_MS),
        _ => None,
    }
}

fn parse_reset_at(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn tier(name: &str, used: f64, reset: Option<&str>) -> QuotaTierObservation {
        QuotaTierObservation {
            name: name.to_string(),
            used_percent: used,
            resets_at: reset.map(str::to_string),
        }
    }

    #[test]
    fn steady_pace_score_matches_existing_webui_marker() {
        let scored = score_tier(
            &tier(TIER_FIVE_HOUR, 30.0, Some("2026-01-01T02:30:00Z")),
            now(),
        )
        .unwrap();

        assert_eq!(scored.steady_pace_used_percent, Some(50.0));
        assert_eq!(scored.score, 60.0);
    }

    #[test]
    fn missing_reset_uses_raw_remaining_percentage() {
        let scored = score_tier(&tier(TIER_WEEKLY_LIMIT, 25.0, None), now()).unwrap();

        assert_eq!(scored.steady_pace_used_percent, None);
        assert_eq!(scored.score, 75.0);
    }

    #[test]
    fn expired_reset_keeps_last_good_snapshot_on_end_of_window_pace() {
        let scored = score_tier(
            &tier(TIER_MONTHLY, 40.0, Some("2025-12-31T00:00:00Z")),
            now(),
        )
        .unwrap();

        assert_eq!(scored.steady_pace_used_percent, Some(100.0));
        assert_eq!(scored.score, 80.0);
    }

    #[test]
    fn long_term_pool_ignores_five_hour_and_uses_bottleneck() {
        let tiers = vec![
            tier(TIER_FIVE_HOUR, 100.0, None),
            tier(TIER_WEEKLY_LIMIT, 40.0, None),
            tier(TIER_MONTHLY, 60.0, None),
        ];

        assert_eq!(
            score_provider(&tiers, now()),
            Some(ProviderUsageScore {
                pool: UsageScorePool::LongTerm,
                score: 40.0,
            })
        );
    }

    #[test]
    fn five_hour_only_provider_enters_short_cycle_pool() {
        assert_eq!(
            score_provider(&[tier(TIER_FIVE_HOUR, 30.0, None)], now()),
            Some(ProviderUsageScore {
                pool: UsageScorePool::FiveHourOnly,
                score: 70.0,
            })
        );
    }

    #[test]
    fn malformed_long_window_never_promotes_provider_to_five_hour_only() {
        let tiers = vec![
            tier(TIER_FIVE_HOUR, 10.0, None),
            tier(TIER_WEEKLY_LIMIT, f64::NAN, None),
        ];

        assert_eq!(score_provider(&tiers, now()), None);
    }

    #[test]
    fn provider_weight_is_the_square_of_its_score() {
        assert_eq!(dynamic_weight(70.0), 4900.0);
        assert_eq!(dynamic_weight(50.0), 2500.0);
        assert_eq!(dynamic_weight(0.0), 0.0);
        assert_eq!(dynamic_weight(f64::NAN), 0.0);
    }

    #[test]
    fn non_finite_usage_is_not_scored() {
        assert_eq!(
            score_tier(&tier(TIER_WEEKLY_LIMIT, f64::NAN, None), now()),
            None
        );
    }

    #[test]
    fn feature_and_unknown_windows_do_not_score() {
        assert_eq!(
            score_provider(
                &[
                    tier("feature:spark:five_hour", 10.0, None),
                    tier("primary_window", 10.0, None),
                ],
                now(),
            ),
            None
        );
    }
}
