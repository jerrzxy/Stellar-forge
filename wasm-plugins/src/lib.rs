//! Wasm-compatible bounded fail-open cache for Soroban RPC state reads.
//!
//! This library also provides a Wasm-based Quorum Set Validation Engine for
//! Stellar SCP configurations.

pub mod cache;
pub mod quorum_eval;

pub use cache::{
    CacheConfig, CacheError, CacheStats, StateCache, DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES,
    DEFAULT_TTL_SECS, MAX_CACHE_BYTES, MAX_CACHE_ENTRIES,
};
pub use quorum_eval::{
    QuorumSetConfig, QuorumValidator, ValidationError, ValidationMetadata, ValidationPolicy,
    ValidationResult,
};

#[cfg(target_arch = "wasm32")]
#[path = "wasm.rs"]
mod wasm;

#[cfg(target_arch = "wasm32")]
mod quorum_eval_wasm;

#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(target_arch = "wasm32")]
pub use quorum_eval_wasm::*;

/// Fail-open wrapper around [`StateCache`] for Soroban RPC state reads.
///
/// Every cache interaction is guarded so that a cache failure (panic, memory
/// limit, or internal error) transparently bypasses the cache and routes the
/// request to the underlying RPC via the supplied `fetch` closure. Requests are
/// never dropped: on any cache fault the value is fetched directly from the RPC.
pub struct FailOpenCache {
    cache: StateCache,
}

impl FailOpenCache {
    /// Create a new fail-open cache from the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            cache: StateCache::new(config),
        }
    }

    /// Read a state entry, falling back to `fetch` on a cache miss or fault.
    ///
    /// The cache is consulted first; on a hit the cached value is returned. On
    /// a miss, or if the cache panics / errors, `fetch` is invoked to read from
    /// the underlying Soroban RPC and the result is best-effort inserted back
    /// into the cache. Cache write failures are ignored so the request still
    /// succeeds.
    pub fn read<F>(&mut self, key: &[u8], fetch: F) -> Vec<u8>
    where
        F: FnOnce() -> Vec<u8>,
    {
        let cached = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cache.get(key)
        }));

        if let Ok(Some(value)) = cached {
            return value;
        }

        let value = fetch();

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cache.insert(key, value.clone())
        }));

        value
    }

    /// Return a snapshot of cache statistics.
    pub fn stats(&self) -> CacheStats {
        self.cache.stats()
    }
}
