//! In-memory streaming-TTFB registry for latency-first routing.
//!
//! # Architecture
//!
//! The registry holds one EWMA of time-to-first-token (milliseconds) per
//! routing target, keyed by (provider_id, declared backend model). Both the
//! recording side (dispatcher log settlement) and the ranking side
//! (LatencyStrategy) go through the same record/rank_ms API, so the key can
//! never drift between writer and reader — the defect that silently disabled
//! the previous latency strategy removed in v2.0.5.
//!
//! Only streaming requests contribute samples (stream_first_chunk_ms);
//! non-streaming requests never touch this registry.
//!
//! # Freshness
//!
//! Samples older than the freshness window are treated as unknown
//! (rank_ms returns None), which makes the selector probe those targets
//! optimistically with real traffic. A stale gap starts a fresh probing
//! round: [`PROBE_SAMPLES`] consecutive samples whose mean seeds the EWMA;
//! until the round completes the target stays in the optimistic-probe group.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// How long a TTFB sample stays trustworthy for ranking.
const FRESHNESS_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Weight of a new sample in the EWMA (0.4 = 40% new, 60% history).
const EWMA_ALPHA: f64 = 0.4;
/// Consecutive streaming samples a target must contribute before leaving the
/// optimistic-probe group; its first rank is the mean of that probing round.
const PROBE_SAMPLES: u32 = 3;

/// Separator for the composite state key. NUL cannot appear in provider ids
/// or model names from configuration, unlike ':'.
const KEY_SEP: char = '\u{0}';

#[derive(Clone, Copy)]
struct TargetLatency {
    /// Samples gathered in the current probing round (0..=PROBE_SAMPLES).
    /// At PROBE_SAMPLES the round graduates into `ewma_ms`.
    probe_count: u32,
    /// Running sum of the current probing round's samples.
    probe_sum: f64,
    /// Mean of the graduated probing round; blended with alpha afterwards.
    /// Not ranked while `probe_count < PROBE_SAMPLES`.
    ewma_ms: f64,
    last_sample_at: Instant,
}

/// Shared TTFB state for latency-first target ordering.
///
/// Lives in Gateway next to HealthRegistry; resets on restart by design,
/// then re-converges through optimistic probing.
pub struct LatencyRegistry {
    states: RwLock<HashMap<String, TargetLatency>>,
    freshness: Duration,
    alpha: f64,
}

impl Default for LatencyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyRegistry {
    pub fn new() -> Self {
        Self::with_config(FRESHNESS_WINDOW, EWMA_ALPHA)
    }

    /// Configurable constructor (test seam mirroring HealthRegistry::with_config).
    pub fn with_config(freshness: Duration, alpha: f64) -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            freshness,
            alpha: alpha.clamp(0.0, 1.0),
        }
    }

    /// Single source of truth for the state key (read and write side).
    fn key(provider_id: &str, model: &str) -> String {
        format!("{provider_id}{KEY_SEP}{model}")
    }

    /// Record an observed time-to-first-token for a target.
    ///
    /// A target first gathers a probing round of [`PROBE_SAMPLES`] consecutive
    /// fresh samples; the round's mean becomes its first ranked estimate.
    /// Subsequent fresh samples blend into that EWMA. A stale gap (past the
    /// freshness window) discards the old round and starts a new one — the
    /// old estimate no longer describes the upstream.
    pub fn record(&self, provider_id: &str, model: &str, ttft_ms: i64) {
        if ttft_ms < 0 {
            return;
        }
        let sample = ttft_ms as f64;
        let now = Instant::now();
        let mut states = match self.states.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = match states.get(&Self::key(provider_id, model)) {
            Some(prev) if now.duration_since(prev.last_sample_at) <= self.freshness => {
                if prev.probe_count < PROBE_SAMPLES {
                    // Probing round continues; graduates at PROBE_SAMPLES.
                    let probe_count = prev.probe_count + 1;
                    let probe_sum = prev.probe_sum + sample;
                    let ewma_ms = if probe_count >= PROBE_SAMPLES {
                        probe_sum / f64::from(PROBE_SAMPLES)
                    } else {
                        prev.ewma_ms
                    };
                    TargetLatency {
                        probe_count,
                        probe_sum,
                        ewma_ms,
                        last_sample_at: now,
                    }
                } else {
                    // Graduated: blend the sample into the EWMA.
                    TargetLatency {
                        probe_count: prev.probe_count,
                        probe_sum: prev.probe_sum,
                        ewma_ms: self.alpha * sample + (1.0 - self.alpha) * prev.ewma_ms,
                        last_sample_at: now,
                    }
                }
            }
            // New or stale: start a fresh probing round with this sample.
            _ => TargetLatency {
                probe_count: 1,
                probe_sum: sample,
                ewma_ms: sample,
                last_sample_at: now,
            },
        };
        states.insert(Self::key(provider_id, model), next);
    }

    /// Current EWMA estimate for ranking, or None when the target has no
    /// graduated fresh estimate (never probed, still gathering its probing
    /// round, or stale beyond the freshness window).
    pub fn rank_ms(&self, provider_id: &str, model: &str) -> Option<f64> {
        let states = match self.states.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let state = states.get(&Self::key(provider_id, model))?;
        let fresh = Instant::now().duration_since(state.last_sample_at) <= self.freshness;
        let graduated = state.probe_count >= PROBE_SAMPLES;
        (fresh && graduated).then_some(state.ewma_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn probe_round_requires_three_samples_to_rank() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);
        assert_eq!(registry.rank_ms("prov", "model-a"), None);

        registry.record("prov", "model-a", 500);
        registry.record("prov", "model-a", 600);
        assert_eq!(
            registry.rank_ms("prov", "model-a"),
            None,
            "still gathering the probing round at 2 samples"
        );

        registry.record("prov", "model-a", 700);
        let rank = registry
            .rank_ms("prov", "model-a")
            .expect("graduated at 3 samples");
        assert!(
            approx(rank, 600.0),
            "first rank is the probing-round mean, not the last sample"
        );
    }

    #[test]
    fn distinct_targets_do_not_bleed_into_each_other() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);

        for _ in 0..PROBE_SAMPLES {
            registry.record("prov", "model-a", 100);
            registry.record("prov", "model-b", 900);
            registry.record("other", "model-a", 700);
        }

        assert!(approx(registry.rank_ms("prov", "model-a").unwrap(), 100.0));
        assert!(approx(registry.rank_ms("prov", "model-b").unwrap(), 900.0));
        assert!(approx(registry.rank_ms("other", "model-a").unwrap(), 700.0));
    }

    #[test]
    fn ewma_blends_samples_after_graduation() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);

        // Probing round mean = 1000.
        for _ in 0..PROBE_SAMPLES {
            registry.record("p", "m", 1000);
        }
        // Post-graduation blend: 0.4 * 200 + 0.6 * 1000.
        registry.record("p", "m", 200);

        assert!(approx(registry.rank_ms("p", "m").unwrap(), 680.0));
    }

    #[test]
    fn stale_samples_restart_the_probe_round() {
        let registry = LatencyRegistry::with_config(Duration::from_millis(50), 0.4);

        for _ in 0..PROBE_SAMPLES {
            registry.record("p", "m", 1000);
        }
        assert_eq!(
            registry.rank_ms("p", "m"),
            Some(1000.0),
            "graduated round ranks"
        );

        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(registry.rank_ms("p", "m"), None, "stale round is unknown");

        // New round: two samples are not enough, three graduate with the
        // fresh mean - the stale 1000ms history must not blend in.
        registry.record("p", "m", 200);
        registry.record("p", "m", 200);
        assert_eq!(
            registry.rank_ms("p", "m"),
            None,
            "restarted round still gathering"
        );
        registry.record("p", "m", 200);
        assert!(
            approx(registry.rank_ms("p", "m").unwrap_or(f64::MAX), 200.0),
            "new round's mean replaces the stale estimate"
        );
    }

    #[test]
    fn negative_samples_are_ignored() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);

        registry.record("p", "m", -5);

        assert_eq!(registry.rank_ms("p", "m"), None);
    }

    #[test]
    fn wildcard_model_key_is_distinct_from_concrete_models() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);

        for _ in 0..PROBE_SAMPLES {
            registry.record("p", "*", 300);
            registry.record("p", "gpt-4", 800);
        }

        assert!(approx(registry.rank_ms("p", "*").unwrap(), 300.0));
        assert!(approx(registry.rank_ms("p", "gpt-4").unwrap(), 800.0));
    }

    #[test]
    fn one_outlier_cannot_wreck_the_graduated_mean() {
        let registry = LatencyRegistry::with_config(Duration::from_secs(60), 0.4);

        // One 5s outlier among three probes moves the mean to ~2.1s instead
        // of the single-sample estimate of 5s.
        registry.record("p", "m", 300);
        registry.record("p", "m", 320);
        registry.record("p", "m", 5000);

        let rank = registry.rank_ms("p", "m").expect("graduated");
        assert!(approx(rank, (300.0 + 320.0 + 5000.0) / 3.0));
    }
}
