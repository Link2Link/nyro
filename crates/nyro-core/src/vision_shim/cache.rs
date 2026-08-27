//! Process-wide caption cache for the vision shim.
//!
//! Keyed by SHA-256(prompt bytes ++ image content digest), so identical
//! images reuse captions across turns (chat clients resend history images on
//! every request) and prompt/version changes naturally miss.
//!
//! Bounded and best-effort: capacity-triggered eviction keeps the newest half,
//! expired entries are pruned on insert and read. Desktop and single-process
//! server deployments share one cache; multi-replica servers keep per-process
//! caches (acceptable — a miss only costs one helper call).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

static CACHE: OnceLock<RwLock<HashMap<[u8; 32], Entry>>> = OnceLock::new();

const MAX_ENTRIES: usize = 1024;
const EVICT_TO: usize = 512;

#[derive(Clone)]
struct Entry {
    caption: String,
    stored_at: Instant,
    expires_at: Instant,
}

fn cache() -> &'static RwLock<HashMap<[u8; 32], Entry>> {
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Fetch a live caption for `key`, if cached and unexpired.
pub(crate) fn lookup(key: &[u8; 32]) -> Option<String> {
    let guard = cache().read().ok()?;
    let entry = guard.get(key)?;
    (entry.expires_at > Instant::now()).then(|| entry.caption.clone())
}

/// Store a caption with a per-config TTL.
pub(crate) fn store(key: [u8; 32], caption: String, ttl: Duration) {
    let Ok(mut guard) = cache().write() else {
        return;
    };
    if guard.len() >= MAX_ENTRIES {
        evict(&mut guard);
    }
    let now = Instant::now();
    guard.insert(
        key,
        Entry {
            caption,
            stored_at: now,
            expires_at: now + ttl,
        },
    );
}

fn evict(guard: &mut HashMap<[u8; 32], Entry>) {
    let now = Instant::now();
    guard.retain(|_, entry| entry.expires_at > now);
    if guard.len() >= MAX_ENTRIES {
        let mut by_age: Vec<(Instant, [u8; 32])> = guard
            .iter()
            .map(|(key, entry)| (entry.stored_at, *key))
            .collect();
        by_age.sort_unstable();
        let excess = by_age.len().saturating_sub(EVICT_TO);
        for (_, key) in by_age.into_iter().take(excess) {
            guard.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_lookup_roundtrips_until_ttl_expires() {
        let key = [7u8; 32];
        store(key, "a caption".to_string(), Duration::from_millis(50));
        assert_eq!(lookup(&key).as_deref(), Some("a caption"));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(lookup(&key), None);
    }

    #[test]
    fn distinct_keys_do_not_collide() {
        store([1u8; 32], "one".to_string(), Duration::from_secs(60));
        store([2u8; 32], "two".to_string(), Duration::from_secs(60));
        assert_eq!(lookup(&[1u8; 32]).as_deref(), Some("one"));
        assert_eq!(lookup(&[2u8; 32]).as_deref(), Some("two"));
    }
}
