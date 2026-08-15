use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const DEFAULT_RECOVERY_SECS: u64 = 30;

pub struct HealthRegistry {
    states: RwLock<HashMap<String, TargetHealth>>,
    failure_threshold: u32,
    recovery_after: Duration,
}

#[derive(Clone)]
pub struct HealthPermit {
    inner: Arc<HealthPermitInner>,
}

struct HealthPermitInner {
    registry: Arc<HealthRegistry>,
    target_key: String,
    epoch: u64,
    kind: PermitKind,
    completed: AtomicBool,
}

#[derive(Clone, Copy)]
enum PermitKind {
    Closed,
    HalfOpenProbe,
}

impl HealthPermit {
    pub fn success(self) {
        if !self.inner.completed.swap(true, Ordering::AcqRel) {
            self.inner.registry.record_success(
                &self.inner.target_key,
                self.inner.epoch,
                self.inner.kind,
            );
        }
    }

    pub fn failure(self) {
        if !self.inner.completed.swap(true, Ordering::AcqRel) {
            self.inner.registry.record_failure(
                &self.inner.target_key,
                self.inner.epoch,
                self.inner.kind,
            );
        }
    }

    pub fn neutral(self) {
        if !self.inner.completed.swap(true, Ordering::AcqRel) {
            self.inner.registry.record_neutral(
                &self.inner.target_key,
                self.inner.epoch,
                self.inner.kind,
            );
        }
    }
}

impl Drop for HealthPermitInner {
    fn drop(&mut self) {
        if !self.completed.swap(true, Ordering::AcqRel) {
            self.registry
                .record_neutral(&self.target_key, self.epoch, self.kind);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

struct TargetHealth {
    state: CircuitState,
    epoch: u64,
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
}

impl Default for TargetHealth {
    fn default() -> Self {
        Self {
            state: CircuitState::Closed,
            epoch: 0,
            consecutive_failures: 0,
            last_failure_at: None,
        }
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_FAILURE_THRESHOLD,
            Duration::from_secs(DEFAULT_RECOVERY_SECS),
        )
    }

    fn with_config(failure_threshold: u32, recovery_after: Duration) -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            failure_threshold,
            recovery_after,
        }
    }

    pub fn is_healthy(&self, target_key: &str) -> bool {
        let states = self.states.read().unwrap();
        match states.get(target_key) {
            None => true,
            Some(state) => match state.state {
                CircuitState::Closed => true,
                CircuitState::HalfOpen => false,
                CircuitState::Open => state
                    .last_failure_at
                    .map(|failed_at| failed_at.elapsed() >= self.recovery_after)
                    .unwrap_or(true),
            },
        }
    }

    pub fn try_acquire(self: &Arc<Self>, target_key: &str) -> Option<HealthPermit> {
        let mut states = self.states.write().unwrap();
        let (epoch, kind) = match states.get_mut(target_key) {
            None => (0, PermitKind::Closed),
            Some(state) => match state.state {
                CircuitState::Closed => (state.epoch, PermitKind::Closed),
                CircuitState::HalfOpen => return None,
                CircuitState::Open => {
                    let recovery_elapsed = state
                        .last_failure_at
                        .map(|failed_at| failed_at.elapsed() >= self.recovery_after)
                        .unwrap_or(true);
                    if !recovery_elapsed {
                        return None;
                    }
                    state.state = CircuitState::HalfOpen;
                    (state.epoch, PermitKind::HalfOpenProbe)
                }
            },
        };
        drop(states);
        Some(HealthPermit {
            inner: Arc::new(HealthPermitInner {
                registry: self.clone(),
                target_key: target_key.to_string(),
                epoch,
                kind,
                completed: AtomicBool::new(false),
            }),
        })
    }

    fn record_success(&self, target_key: &str, epoch: u64, kind: PermitKind) {
        let mut states = self.states.write().unwrap();
        let Some(state) = states.get_mut(target_key) else {
            return;
        };
        if state.epoch != epoch {
            return;
        }
        if matches!(kind, PermitKind::HalfOpenProbe) && state.state == CircuitState::HalfOpen {
            state.state = CircuitState::Closed;
            state.consecutive_failures = 0;
            state.last_failure_at = None;
        } else if matches!(kind, PermitKind::Closed) && state.state == CircuitState::Closed {
            state.consecutive_failures = 0;
            state.last_failure_at = None;
        }
    }

    fn record_failure(&self, target_key: &str, epoch: u64, kind: PermitKind) {
        let mut states = self.states.write().unwrap();
        let state = states.entry(target_key.to_string()).or_default();
        if state.epoch != epoch {
            return;
        }
        match kind {
            PermitKind::HalfOpenProbe if state.state == CircuitState::HalfOpen => {
                Self::open(state);
            }
            PermitKind::Closed if state.state == CircuitState::Closed => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.last_failure_at = Some(Instant::now());
                if state.consecutive_failures >= self.failure_threshold {
                    Self::open(state);
                }
            }
            PermitKind::Closed | PermitKind::HalfOpenProbe => {}
        }
    }

    fn record_neutral(&self, target_key: &str, epoch: u64, kind: PermitKind) {
        let mut states = self.states.write().unwrap();
        let Some(state) = states.get_mut(target_key) else {
            return;
        };
        if state.epoch == epoch
            && matches!(kind, PermitKind::HalfOpenProbe)
            && state.state == CircuitState::HalfOpen
        {
            state.state = CircuitState::Open;
            state.last_failure_at = Some(Instant::now());
        }
    }

    fn open(state: &mut TargetHealth) {
        state.state = CircuitState::Open;
        state.epoch = state.epoch.wrapping_add(1);
        state.last_failure_at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_consecutive_failures() {
        let registry = Arc::new(HealthRegistry::with_config(3, Duration::from_secs(30)));

        registry.try_acquire("target").unwrap().failure();
        registry.try_acquire("target").unwrap().failure();
        registry.try_acquire("target").unwrap().failure();

        assert!(!registry.is_healthy("target"));
        assert!(registry.try_acquire("target").is_none());
    }

    #[test]
    fn recovery_allows_only_one_half_open_probe() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();

        assert!(registry.is_healthy("target"));
        let permit = registry.try_acquire("target").unwrap();
        assert!(!registry.is_healthy("target"));
        assert!(registry.try_acquire("target").is_none());
        permit.neutral();
    }

    #[test]
    fn successful_half_open_probe_closes_the_circuit() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();

        registry.try_acquire("target").unwrap().success();

        assert!(registry.try_acquire("target").is_some());
        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn failed_half_open_probe_reopens_the_circuit() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();

        registry.try_acquire("target").unwrap().failure();

        assert!(registry.is_healthy("target"));
        let permit = registry.try_acquire("target").unwrap();
        assert!(registry.try_acquire("target").is_none());
        permit.neutral();
    }

    #[test]
    fn dropped_half_open_permit_releases_the_probe() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();
        drop(registry.try_acquire("target").unwrap());

        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn dropping_one_half_open_clone_keeps_the_probe_exclusive() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();
        let probe = registry.try_acquire("target").unwrap();
        let worker = probe.clone();

        drop(probe);

        assert!(registry.try_acquire("target").is_none());
        worker.neutral();
        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn concurrent_clones_settle_only_once() {
        let registry = Arc::new(HealthRegistry::with_config(2, Duration::from_secs(30)));
        let permit = registry.try_acquire("target").unwrap();
        let clone = permit.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other_barrier = barrier.clone();

        let first = std::thread::spawn(move || {
            barrier.wait();
            permit.failure();
        });
        let second = std::thread::spawn(move || {
            other_barrier.wait();
            clone.failure();
        });
        first.join().unwrap();
        second.join().unwrap();

        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn neutral_half_open_probe_observes_recovery_delay() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::from_millis(20)));
        registry.try_acquire("target").unwrap().failure();
        std::thread::sleep(Duration::from_millis(25));
        registry.try_acquire("target").unwrap().neutral();

        assert!(registry.try_acquire("target").is_none());
    }

    #[test]
    fn success_resets_failures_without_a_restart() {
        let registry = Arc::new(HealthRegistry::with_config(2, Duration::ZERO));
        registry.try_acquire("target").unwrap().failure();
        registry.try_acquire("target").unwrap().failure();
        registry.try_acquire("target").unwrap().success();
        registry.try_acquire("target").unwrap().failure();

        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn stale_closed_success_cannot_close_a_half_open_circuit() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        let stale = registry.try_acquire("target").unwrap();
        registry.try_acquire("target").unwrap().failure();
        let probe = registry.try_acquire("target").unwrap();

        stale.success();

        assert!(registry.try_acquire("target").is_none());
        probe.neutral();
    }

    #[test]
    fn stale_closed_failure_cannot_fail_a_half_open_probe() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        let stale = registry.try_acquire("target").unwrap();
        registry.try_acquire("target").unwrap().failure();
        let probe = registry.try_acquire("target").unwrap();

        stale.failure();

        assert!(registry.try_acquire("target").is_none());
        probe.success();
        assert!(registry.try_acquire("target").is_some());
    }

    #[test]
    fn stale_closed_drop_cannot_release_a_half_open_probe() {
        let registry = Arc::new(HealthRegistry::with_config(1, Duration::ZERO));
        let stale = registry.try_acquire("target").unwrap();
        registry.try_acquire("target").unwrap().failure();
        let probe = registry.try_acquire("target").unwrap();

        drop(stale);

        assert!(registry.try_acquire("target").is_none());
        probe.success();
    }
}
