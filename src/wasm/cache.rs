//! In-memory LRU cache of compiled WASM modules, keyed by
//! `(contract_id, ledger_sequence)`, with a background idle-entry collector.
//!
//! Isolation: only the immutable compiled artifact is shared (`Arc<M>`).
//! Linear memory belongs to a per-call instance/store created by the caller,
//! so memory spaces of different contract instances can never overlap.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;

/// Cache key: a new ledger sequence forces recompilation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub contract_id: String,
    pub ledger_sequence: u32,
}

impl CacheKey {
    pub fn new(contract_id: impl Into<String>, ledger_sequence: u32) -> Self {
        Self {
            contract_id: contract_id.into(),
            ledger_sequence,
        }
    }
}

/// Snapshot of cache counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
}

struct Entry<M> {
    module: Arc<M>,
    last_used: Instant,
}

/// Thread-safe LRU cache of compiled modules.
pub struct WasmCache<M> {
    inner: Mutex<LruCache<CacheKey, Entry<M>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl<M> WasmCache<M> {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Return the cached module, or compile and insert it on a miss.
    pub fn get_or_compile<E>(
        &self,
        key: &CacheKey,
        compile: impl FnOnce() -> Result<M, E>,
    ) -> Result<Arc<M>, E> {
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(e) = guard.get_mut(key) {
                e.last_used = Instant::now();
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(e.module.clone());
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        // Compile outside the lock so other contracts are not blocked.
        let module = Arc::new(compile()?);
        let mut guard = self.inner.lock().unwrap();
        if guard
            .push(
                key.clone(),
                Entry {
                    module: module.clone(),
                    last_used: Instant::now(),
                },
            )
            .is_some_and(|(k, _)| &k != key)
        {
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(module)
    }

    /// Evict entries idle longer than `max_idle`. Returns the number evicted.
    pub fn evict_idle(&self, max_idle: Duration) -> usize {
        let mut guard = self.inner.lock().unwrap();
        let stale: Vec<CacheKey> = guard
            .iter()
            .filter(|(_, e)| e.last_used.elapsed() > max_idle)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            guard.pop(k);
        }
        self.evictions
            .fetch_add(stale.len() as u64, Ordering::Relaxed);
        stale.len()
    }

    /// Evict entries for `contract_id` compiled at ledgers older than `current_ledger`.
    pub fn evict_older_ledgers(&self, contract_id: &str, current_ledger: u32) -> usize {
        let mut guard = self.inner.lock().unwrap();
        let stale: Vec<CacheKey> = guard
            .iter()
            .filter(|(k, _)| k.contract_id == contract_id && k.ledger_sequence < current_ledger)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            guard.pop(k);
        }
        self.evictions
            .fetch_add(stale.len() as u64, Ordering::Relaxed);
        stale.len()
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            entries: self.inner.lock().unwrap().len(),
        }
    }
}

impl<M: Send + Sync + 'static> WasmCache<M> {
    /// Spawn an aggressive background GC that evicts idle entries every `interval`.
    /// The task stops automatically once the cache is dropped.
    pub fn spawn_gc(
        cache: &Arc<Self>,
        interval: Duration,
        max_idle: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(cache);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                match weak.upgrade() {
                    Some(c) => {
                        c.evict_idle(max_idle);
                    }
                    None => break,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_miss_and_ledger_invalidation() {
        let c: WasmCache<u32> = WasmCache::new(4);
        let k1 = CacheKey::new("C1", 1);
        let k2 = CacheKey::new("C1", 2);
        c.get_or_compile::<()>(&k1, || Ok(1)).unwrap();
        c.get_or_compile::<()>(&k1, || panic!("must hit")).unwrap();
        c.get_or_compile::<()>(&k2, || Ok(2)).unwrap();
        let s = c.stats();
        assert_eq!((s.hits, s.misses, s.entries), (1, 2, 2));
        assert_eq!(c.evict_older_ledgers("C1", 2), 1);
    }

    #[test]
    fn lru_eviction() {
        let c: WasmCache<u32> = WasmCache::new(1);
        c.get_or_compile::<()>(&CacheKey::new("A", 1), || Ok(1)).unwrap();
        c.get_or_compile::<()>(&CacheKey::new("B", 1), || Ok(2)).unwrap();
        assert_eq!(c.stats().entries, 1);
        assert_eq!(c.stats().evictions, 1);
    }

    #[test]
    fn idle_eviction() {
        let c: WasmCache<u32> = WasmCache::new(4);
        c.get_or_compile::<()>(&CacheKey::new("A", 1), || Ok(1)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.evict_idle(Duration::from_millis(5)), 1);
        assert_eq!(c.stats().entries, 0);
    }

    #[tokio::test]
    async fn background_gc_evicts() {
        let c = Arc::new(WasmCache::<u32>::new(4));
        c.get_or_compile::<()>(&CacheKey::new("A", 1), || Ok(1)).unwrap();
        let h = WasmCache::spawn_gc(&c, Duration::from_millis(10), Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(c.stats().entries, 0);
        drop(c);
        h.await.unwrap();
    }

    /// 10,000 sequential "simulations": one miss with costly compile, then hits.
    #[test]
    fn benchmark_latency_reduction() {
        let c: WasmCache<u64> = WasmCache::new(8);
        let key = CacheKey::new("COMPLEX", 100);
        let compile = || -> Result<u64, ()> {
            std::thread::sleep(Duration::from_millis(50)); // simulated JIT cost
            Ok(42)
        };
        let t = Instant::now();
        c.get_or_compile(&key, compile).unwrap();
        let miss = t.elapsed();
        let t = Instant::now();
        for _ in 0..10_000 {
            c.get_or_compile(&key, compile).unwrap();
        }
        let avg_hit = t.elapsed() / 10_000;
        assert!(avg_hit.as_secs_f64() <= miss.as_secs_f64() * 0.2);
        assert_eq!(c.stats().hits, 10_000);
    }
}
