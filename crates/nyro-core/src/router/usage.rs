//! Provider quota scoring for the `usage` model-balance strategy.
//!
//! Each provider is characterized by its **largest** reported main quota
//! window (monthly, else weekly, else five-hour). The routing value is the
//! required acceleration needed to finish exactly that window at its reset:
//!
//! ```text
//! rate r = remaining_quota% / remaining_time%
//! ```
//!
//! `r > 1` means the window is paced to leave quota unused at reset (waste
//! risk -> push more traffic), `r < 1` means it will exhaust before reset
//! (back off), and `r` grows as a reset nears with quota still unspent.
//! All scored providers compete in a single pool weighted by `r³`.
//!
//! Window lengths mirror the steady-pace marker rendered by the WebUI:
//! fixed 5h / 7d / 30d reconstructed backwards from the upstream reset time.

use chrono::{DateTime, NaiveDate, Utc};

use super::quota::QuotaTierObservation;

pub(crate) const TIER_FIVE_HOUR: &str = "five_hour";
pub(crate) const TIER_WEEKLY_LIMIT: &str = "weekly_limit";
pub(crate) const TIER_MONTHLY: &str = "monthly";

const FIVE_HOURS_MS: f64 = 5.0 * 60.0 * 60.0 * 1000.0;
const SEVEN_DAYS_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const THIRTY_DAYS_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// Upper bound for the required-acceleration ratio. A last-good snapshot
/// whose reset already passed (or a window about to reset with quota left)
/// must not monopolize routing indefinitely.
pub(crate) const MAX_USAGE_RATE: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ProviderUsageScore {
    /// Required acceleration on the provider's largest main window.
    pub rate: f64,
    /// Name of the governing window (`monthly` / `weekly_limit` /
    /// `five_hour`) — surfaced in route-decision snapshots.
    pub window: &'static str,
    /// Remaining quota of the governing window, in percent.
    pub remaining_quota: f64,
    /// Remaining time of the governing window, in percent of the window.
    /// `None` when no usable reset time was reported.
    pub remaining_time: Option<f64>,
}

/// Provider routing weight: the cube of the required acceleration.
pub(crate) fn dynamic_weight(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate.min(MAX_USAGE_RATE).powi(3)
    } else {
        0.0
    }
}

/// Score one provider from its canonical main quota tiers.
///
/// Only the largest reported main window participates (monthly, else
/// weekly, else five-hour); smaller windows are deliberately ignored for
/// routing. Providers without any canonical window stay unscored and fall
/// back to the equal-weight unknown pool.
pub(crate) fn score_provider(
    tiers: &[QuotaTierObservation],
    now: DateTime<Utc>,
) -> Option<ProviderUsageScore> {
    for window in [TIER_MONTHLY, TIER_WEEKLY_LIMIT, TIER_FIVE_HOUR] {
        let rates = tiers
            .iter()
            .filter(|tier| tier.name == window)
            .filter_map(|tier| tier_rate(tier, now))
            .collect::<Vec<_>>();
        if rates.is_empty() {
            continue;
        }
        // Duplicate same-name windows: keep the least urgent estimate so a
        // malformed duplicate cannot inflate the provider's priority.
        return rates
            .into_iter()
            .reduce(min_window_rate)
            .map(|w| ProviderUsageScore {
                rate: w.rate,
                window,
                remaining_quota: w.remaining_quota,
                remaining_time: w.remaining_time,
            });
    }
    None
}

fn min_window_rate(a: WindowRate, b: WindowRate) -> WindowRate {
    if a.rate <= b.rate { a } else { b }
}

/// Required acceleration for one canonical quota window.
///
/// - reset in the future -> `remaining_quota% / remaining_time%` of the
///   window (a reset farther than one full window counts as a fresh window);
/// - reset already passed (stale last-good) -> capped at [`MAX_USAGE_RATE`];
/// - reset missing or unparseable -> raw headroom fraction
///   `remaining_quota% / 100%`, mirroring the raw-remaining fallback.
pub(crate) fn tier_rate(tier: &QuotaTierObservation, now: DateTime<Utc>) -> Option<WindowRate> {
    let window_ms = tier_window_ms(&tier.name)?;
    if !tier.used_percent.is_finite() {
        return None;
    }
    let used = tier.used_percent.clamp(0.0, 100.0);
    let remaining_quota = 100.0 - used;
    let rate = match tier.resets_at.as_deref().and_then(parse_reset_at) {
        Some(reset) => {
            let remaining_ms = reset.signed_duration_since(now).num_milliseconds() as f64;
            if remaining_ms <= 0.0 {
                MAX_USAGE_RATE
            } else {
                let remaining_time = if remaining_ms >= window_ms {
                    100.0
                } else {
                    remaining_ms / window_ms * 100.0
                };
                remaining_quota / remaining_time
            }
        }
        None => remaining_quota / 100.0,
    };
    Some(WindowRate {
        rate: rate.clamp(0.0, MAX_USAGE_RATE),
        remaining_quota,
        remaining_time: tier
            .resets_at
            .as_deref()
            .and_then(parse_reset_at)
            .map(|reset| {
                let remaining_ms = reset.signed_duration_since(now).num_milliseconds() as f64;
                if remaining_ms <= 0.0 {
                    0.0
                } else if remaining_ms >= window_ms {
                    100.0
                } else {
                    remaining_ms / window_ms * 100.0
                }
            }),
    })
}

/// Per-window scoring detail used by both routing and decision snapshots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowRate {
    pub rate: f64,
    pub remaining_quota: f64,
    pub remaining_time: Option<f64>,
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

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn on_pace_window_requires_no_acceleration() {
        // Half the week remains and half the quota is spent -> r = 50/50.
        let rate = tier_rate(
            &tier(TIER_WEEKLY_LIMIT, 50.0, Some("2026-01-04T12:00:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, 1.0));
    }

    #[test]
    fn behind_pace_window_accelerates() {
        // 40% spent with only 30% of the window left -> r = 60/30 = 2.
        let rate = tier_rate(
            &tier(TIER_WEEKLY_LIMIT, 40.0, Some("2026-01-03T02:24:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, 2.0));
    }

    #[test]
    fn over_paced_window_backs_off() {
        // 90% spent with half the window left -> r = 10/50 = 0.2.
        let rate = tier_rate(
            &tier(TIER_WEEKLY_LIMIT, 90.0, Some("2026-01-04T12:00:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, 0.2));
    }

    #[test]
    fn reset_beyond_full_window_counts_as_fresh() {
        // Reset 10 days out is farther than one weekly window -> full time.
        let rate = tier_rate(
            &tier(TIER_WEEKLY_LIMIT, 30.0, Some("2026-01-11T00:00:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, 0.7));
    }

    #[test]
    fn missing_reset_uses_raw_headroom_fraction() {
        let rate = tier_rate(&tier(TIER_WEEKLY_LIMIT, 25.0, None), now()).unwrap();

        assert!(approx(rate.rate, 0.75));
    }

    #[test]
    fn expired_reset_caps_rate() {
        // Stale last-good snapshot: nominally past the window end.
        let rate = tier_rate(
            &tier(TIER_MONTHLY, 40.0, Some("2025-12-31T00:00:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, MAX_USAGE_RATE));
    }

    #[test]
    fn imminent_reset_is_capped_not_infinite() {
        // 0% spent with one hour left of a weekly window -> huge but capped.
        let rate = tier_rate(
            &tier(TIER_WEEKLY_LIMIT, 0.0, Some("2026-01-01T01:00:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, MAX_USAGE_RATE));
    }

    #[test]
    fn five_hour_window_uses_five_hour_length() {
        // 40% spent with a quarter of the 5h window left -> r = 60/25 = 2.4.
        let rate = tier_rate(
            &tier(TIER_FIVE_HOUR, 40.0, Some("2026-01-01T01:15:00Z")),
            now(),
        )
        .unwrap();

        assert!(approx(rate.rate, 2.4));
    }

    #[test]
    fn monthly_governs_when_reported() {
        // Largest window wins even when smaller windows are far more urgent.
        let tiers = vec![
            tier(TIER_FIVE_HOUR, 0.0, Some("2026-01-01T02:30:00Z")), // r = 2.0
            tier(TIER_WEEKLY_LIMIT, 90.0, Some("2026-01-04T12:00:00Z")), // r = 0.2
            tier(TIER_MONTHLY, 10.0, Some("2026-01-16T00:00:00Z")),  // r = 1.8
        ];

        assert_eq!(
            score_provider(&tiers, now()),
            Some(ProviderUsageScore {
                rate: 1.8,
                window: TIER_MONTHLY,
                remaining_quota: 90.0,
                remaining_time: Some(50.0),
            })
        );
    }

    #[test]
    fn weekly_governs_when_no_monthly() {
        let tiers = vec![
            tier(TIER_FIVE_HOUR, 0.0, Some("2026-01-01T02:30:00Z")), // r = 2.0
            tier(TIER_WEEKLY_LIMIT, 50.0, Some("2026-01-04T12:00:00Z")), // r = 1.0
        ];

        assert_eq!(
            score_provider(&tiers, now()),
            Some(ProviderUsageScore {
                rate: 1.0,
                window: TIER_WEEKLY_LIMIT,
                remaining_quota: 50.0,
                remaining_time: Some(50.0),
            })
        );
    }

    #[test]
    fn five_hour_only_provider_uses_five_hour_rate() {
        assert_eq!(
            score_provider(
                &[tier(TIER_FIVE_HOUR, 40.0, Some("2026-01-01T01:15:00Z"))],
                now(),
            ),
            Some(ProviderUsageScore {
                rate: 2.4,
                window: TIER_FIVE_HOUR,
                remaining_quota: 60.0,
                remaining_time: Some(25.0),
            })
        );
    }

    #[test]
    fn duplicate_same_name_windows_keep_least_urgent() {
        let tiers = vec![
            tier(TIER_WEEKLY_LIMIT, 20.0, Some("2026-01-04T12:00:00Z")), // r = 1.6
            tier(TIER_WEEKLY_LIMIT, 60.0, Some("2026-01-04T12:00:00Z")), // r = 0.8
        ];

        assert_eq!(
            score_provider(&tiers, now()),
            Some(ProviderUsageScore {
                rate: 0.8,
                window: TIER_WEEKLY_LIMIT,
                remaining_quota: 40.0,
                remaining_time: Some(50.0),
            })
        );
    }

    #[test]
    fn malformed_largest_window_falls_back_to_next_largest() {
        let tiers = vec![
            tier(TIER_FIVE_HOUR, 10.0, None), // r = 0.9
            tier(TIER_WEEKLY_LIMIT, f64::NAN, None),
        ];

        assert_eq!(
            score_provider(&tiers, now()),
            Some(ProviderUsageScore {
                rate: 0.9,
                window: TIER_FIVE_HOUR,
                remaining_quota: 90.0,
                remaining_time: None,
            })
        );
    }

    #[test]
    fn non_finite_usage_is_not_scored() {
        assert_eq!(
            tier_rate(&tier(TIER_WEEKLY_LIMIT, f64::NAN, None), now()),
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

    #[test]
    fn provider_weight_is_the_cube_of_its_rate() {
        assert!(approx(dynamic_weight(1.2), 1.728));
        assert!(approx(dynamic_weight(MAX_USAGE_RATE), 1000.0));
        assert!(approx(dynamic_weight(50.0), 1000.0), "capped at the bound");
        assert_eq!(dynamic_weight(0.0), 0.0);
        assert_eq!(dynamic_weight(f64::NAN), 0.0);
        assert_eq!(dynamic_weight(-1.0), 0.0);
    }
}
