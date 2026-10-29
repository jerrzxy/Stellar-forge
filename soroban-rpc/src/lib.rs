//! Soroban RPC integration layer.
//!
//! Provides a fail-open, bounded LRU caching layer for read-only Soroban RPC
//! state reads. The cache is designed to be driven from a WebAssembly plugin
//! (see `wasm-plugins/src/cache.rs`) but the core logic lives here so it can be
//! unit tested and reused by the RPC server directly.
//!
//! Fail-open guarantees: any cache error, panic, or memory-limit breach must
//! transparently bypass the cache and route the request to the underlying RPC
//! node without dropping or disrupting the request.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Configuration for the fail-open cache. Mirrors the values exposed via a
/// Kubernetes ConfigMap so operators can tune TTL and size without rebuilding.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries retained in the LRU cache.
    pub max_entries: usize,
    /// Time-to-live for a cached entry.
    pub ttl: Duration,
    /// Hard upper bound on approximate bytes retained by the cache. When the
    /// bound would be exceeded the cache evicts entries; if it still cannot fit
    /// the value it fails open (does not cache).
    pub max_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            ttl: Duration::from_secs(5),
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

struct Entry {
    value: Vec<u8>,
    inserted: Instant,
    last_used: u64,
}

/// Bounded LRU cache with TTL, safe to share across threads.
pub struct FailOpenCache {
    config: CacheConfig,
    inner: Mutex<Inner>,
}

struct Inner {
    entries: HashMap<Vec<u8>, Entry>,
    clock: u64,
    bytes: usize,
}

impl FailOpenCache {
    pub fn new(config: CacheConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                clock: 0,
                bytes: 0,
            }),
        }
    }

    /// Attempt to read a cached value. Returns `None` on miss, expiry, or any
    /// internal failure (fail-open).
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let config = self.config.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut inner = self.inner.lock().ok()?;
            inner.clock = inner.clock.wrapping_add(1);
            let now = Instant::now();
            let clock = inner.clock;
            let entry = inner.entries.get_mut(key)?;
            if now.duration_since(entry.inserted) > config.ttl {
                let removed = inner.entries.remove(key)?;
                inner.bytes = inner.bytes.saturating_sub(removed.value.len());
                return None;
            }
            entry.last_used = clock;
            Some(entry.value.clone())
        }));
        result.ok().flatten()
    }

    /// Attempt to store a value. Any failure (lock poisoning, panic, memory
    /// bound) is swallowed so the caller always proceeds to the RPC node.
    pub fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        let config = self.config.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if value.len() > config.max_bytes {
                return;
            }
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            inner.clock = inner.clock.wrapping_add(1);
            let clock = inner.clock;
            let now = Instant::now();

            if let Some(prev) = inner.entries.remove(&key) {
                inner.bytes = inner.bytes.saturating_sub(prev.value.len());
            }

            while inner.entries.len() >= config.max_entries
                || inner.bytes.saturating_add(value.len()) > config.max_bytes
            {
                let Some(victim) = inner
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(removed) = inner.entries.remove(&victim) {
                    inner.bytes = inner.bytes.saturating_sub(removed.value.len());
                }
            }

            inner.bytes = inner.bytes.saturating_add(value.len());
            inner.entries.insert(
                key,
                Entry {
                    value,
                    inserted: now,
                    last_used: clock,
                },
            );
        }));
    }

    /// Read-through helper: returns the cached value on hit, otherwise invokes
    /// `fetch` and caches the result. `fetch` errors are propagated unchanged
    /// (the cache never masks RPC failures).
    pub fn get_or_fetch<F, E>(&self, key: &[u8], fetch: F) -> Result<Vec<u8>, E>
    where
        F: FnOnce() -> Result<Vec<u8>, E>,
    {
        if let Some(hit) = self.get(key) {
            return Ok(hit);
        }
        let value = fetch()?;
        self.put(key.to_vec(), value.clone());
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max_entries: usize, ttl: Duration) -> FailOpenCache {
        FailOpenCache::new(CacheConfig {
            max_entries,
            ttl,
            max_bytes: 1024 * 1024,
        })
    }

    #[test]
    fn hit_after_put() {
        let c = cache(4, Duration::from_secs(60));
        c.put(b"k".to_vec(), b"v".to_vec());
        assert_eq!(c.get(b"k"), Some(b"v".to_vec()));
    }

    #[test]
    fn miss_returns_none() {
        let c = cache(4, Duration::from_secs(60));
        assert_eq!(c.get(b"absent"), None);
    }

    #[test]
    fn ttl_expiry() {
        let c = cache(4, Duration::from_millis(1));
        c.put(b"k".to_vec(), b"v".to_vec());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(c.get(b"k"), None);
    }

    #[test]
    fn lru_eviction() {
        let c = cache(2, Duration::from_secs(60));
        c.put(b"a".to_vec(), b"1".to_vec());
        c.put(b"b".to_vec(), b"2".to_vec());
        // Touch `a` so `b` becomes the least recently used.
        assert_eq!(c.get(b"a"), Some(b"1".to_vec()));
        c.put(b"c".to_vec(), b"3".to_vec());
        assert_eq!(c.get(b"b"), None);
        assert_eq!(c.get(b"a"), Some(b"1".to_vec()));
        assert_eq!(c.get(b"c"), Some(b"3".to_vec()));
    }

    #[test]
    fn oversized_value_is_not_cached() {
        let c = FailOpenCache::new(CacheConfig {
            max_entries: 4,
            ttl: Duration::from_secs(60),
            max_bytes: 4,
        });
        c.put(b"k".to_vec(), vec![0u8; 8]);
        assert_eq!(c.get(b"k"), None);
    }

    #[test]
    fn get_or_fetch_caches_and_fails_open() {
        let c = cache(4, Duration::from_secs(60));
        let mut calls = 0;
        let v = c
            .get_or_fetch(b"k", || {
                calls += 1;
                Ok::<_, ()>(b"v".to_vec())
            })
            .unwrap();
        assert_eq!(v, b"v".to_vec());
        let v2 = c
            .get_or_fetch(b"k", || {
                calls += 1;
                Ok::<_, ()>(b"v".to_vec())
            })
            .unwrap();
        assert_eq!(v2, b"v".to_vec());
        assert_eq!(calls, 1);
    }

    #[test]
    fn fetch_error_propagates() {
        let c = cache(4, Duration::from_secs(60));
        let res: Result<Vec<u8>, &str> = c.get_or_fetch(b"k", || Err("rpc down"));
        assert_eq!(res, Err("rpc down"));
    }
}
