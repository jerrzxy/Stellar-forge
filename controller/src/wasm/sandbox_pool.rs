// controller/src/wasm/sandbox_pool.rs

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use wasmtime::{Config, Engine, Instance, Memory, Module, Store};

use crate::wasm::memory_scrubber::{MemoryScrubber, ScrubError};

/// Errors that can occur while interacting with the sandbox pool.
#[derive(Debug)]
pub enum PoolError {
    /// The pool has been shut down and can no longer hand out sandboxes.
    Shutdown,
    /// Timed out waiting for a sandbox to become available.
    Timeout,
    /// Failed to instantiate a new sandbox.
    Instantiation(String),
    /// The sandbox failed a health check and could not be reused.
    Unhealthy(String),
    /// Memory scrubbing failed; the sandbox must not be reused.
    Scrub(ScrubError),
    /// The pool configuration was invalid.
    InvalidConfig(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::Shutdown => write!(f, "sandbox pool is shut down"),
            PoolError::Timeout => write!(f, "timed out waiting for a sandbox"),
            PoolError::Instantiation(e) => write!(f, "failed to instantiate sandbox: {e}"),
            PoolError::Unhealthy(e) => write!(f, "sandbox failed health check: {e}"),
            PoolError::Scrub(e) => write!(f, "memory scrub failed: {e}"),
            PoolError::InvalidConfig(e) => write!(f, "invalid pool config: {e}"),
        }
    }
}

impl std::error::Error for PoolError {}

/// Configuration for the warm sandbox pool.
#[derive(Debug, Clone)]
pub struct SandboxPoolConfig {
    /// Maximum number of idle sandboxes kept warm in the pool.
    pub max_idle: usize,
    /// Maximum number of sandboxes that may be checked out concurrently.
    pub max_total: usize,
    /// How long a caller waits for a sandbox before timing out.
    pub checkout_timeout: Duration,
    /// Maximum number of times a sandbox may be reused before being recycled.
    pub max_reuse_count: u64,
    /// Whether to run a health check on return before pooling.
    pub health_check_on_return: bool,
}

impl Default for SandboxPoolConfig {
    fn default() -> Self {
        Self {
            max_idle: 8,
            max_total: 64,
            checkout_timeout: Duration::from_secs(5),
            max_reuse_count: 10_000,
            health_check_on_return: true,
        }
    }
}

impl SandboxPoolConfig {
    fn validate(&self) -> Result<(), PoolError> {
        if self.max_total == 0 {
            return Err(PoolError::InvalidConfig(
                "max_total must be greater than zero".into(),
            ));
        }
        if self.max_idle > self.max_total {
            return Err(PoolError::InvalidConfig(
                "max_idle must not exceed max_total".into(),
            ));
        }
        Ok(())
    }
}

/// A single pre-initialized Wasmtime execution environment.
///
/// The sandbox owns its own `Store`, `Instance` and linear `Memory`. It is
/// created once and reused across contract invocations, with the linear memory
/// scrubbed between uses.
pub struct Sandbox {
    engine: Engine,
    store: Store<()>,
    instance: Instance,
    memory: Memory,
    reuse_count: u64,
    created_at: Instant,
    last_used: Instant,
}

impl Sandbox {
    /// Instantiate a fresh sandbox from the given engine and module.
    fn new(engine: Engine, module: &Module) -> Result<Self, PoolError> {
        let mut store = Store::new(&engine, ());
        let instance = module
            .instantiate(&mut store, &[])
            .map_err(|e| PoolError::Instantiation(e.to_string()))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| PoolError::Instantiation("module does not export 'memory'".into()))?;

        let now = Instant::now();
        Ok(Self {
            engine,
            store,
            instance,
            memory,
            reuse_count: 0,
            created_at: now,
            last_used: now,
        })
    }

    /// Access the underlying store and instance for executing a contract.
    pub fn store_mut(&mut self) -> &mut Store<()> {
        &mut self.store
    }

    /// Access the underlying instance.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Access the underlying linear memory.
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Number of times this sandbox has been checked out.
    pub fn reuse_count(&self) -> u64 {
        self.reuse_count
    }

    /// Perform a lightweight health check to verify the sandbox is still usable.
    fn health_check(&mut self) -> Result<(), PoolError> {
        // Verify the store is still alive and the memory is accessible.
        let size = self.memory.data_size(&self.store);
        if size == 0 {
            return Err(PoolError::Unhealthy("linear memory has zero size".into()));
        }
        // Verify the instance is still valid by touching an export.
        if self.instance.get_memory(&mut self.store, "memory").is_none() {
            return Err(PoolError::Unhealthy("memory export disappeared".into()));
        }
        Ok(())
    }

    /// Scrub the linear memory so no residual state leaks to the next user.
    fn scrub(&mut self) -> Result<(), PoolError> {
        let scrubber = MemoryScrubber::new();
        scrubber
            .scrub(&mut self.store, &self.memory)
            .map_err(PoolError::Scrub)
    }
}

/// Internal shared state guarded by a mutex.
struct PoolState {
    idle: VecDeque<Sandbox>,
    total: usize,
    shutdown: bool,
}

/// A thread-safe warm pool of pre-initialized Wasmtime sandboxes.
pub struct SandboxPool {
    engine: Engine,
    module: Module,
    config: SandboxPoolConfig,
    state: Mutex<PoolState>,
    available: Condvar,
    /// Number of sandboxes currently checked out.
    checked_out: AtomicUsize,
    /// Total number of sandboxes ever created (for metrics).
    created_total: AtomicU64,
    /// Total number of sandboxes recycled due to scrub/health failures.
    recycled_total: AtomicU64,
    shutdown_flag: AtomicBool,
}

impl SandboxPool {
    /// Create a new pool and pre-warm it up to `max_idle` sandboxes.
    pub fn new(
        engine: Engine,
        module: Module,
        config: SandboxPoolConfig,
    ) -> Result<Arc<Self>, PoolError> {
        config.validate()?;

        let pool = Arc::new(Self {
            engine,
            module,
            config,
            state: Mutex::new(PoolState {
                idle: VecDeque::new(),
                total: 0,
                shutdown: false,
            }),
            available: Condvar::new(),
            checked_out: AtomicUsize::new(0),
            created_total: AtomicU64::new(0),
            recycled_total: AtomicU64::new(0),
            shutdown_flag: AtomicBool::new(false),
        });

        pool.prewarm()?;
        Ok(pool)
    }

    /// Create a pool from a WAT/WASM byte slice using default Wasmtime config.
    pub fn from_bytes(
        wasm: &[u8],
        config: SandboxPoolConfig,
    ) -> Result<Arc<Self>, PoolError> {
        let mut engine_config = Config::new();
        engine_config.wasm_multi_memory(false);
        engine_config.wasm_bulk_memory(true);
        let engine = Engine::new(&engine_config)
            .map_err(|e| PoolError::Instantiation(e.to_string()))?;
        let module = Module::new(&engine, wasm)
            .map_err(|e| PoolError::Instantiation(e.to_string()))?;
        Self::new(engine, module, config)
    }

    /// Fill the idle queue up to `max_idle`.
    fn prewarm(&self) -> Result<(), PoolError> {
        let mut state = self.state.lock().unwrap();
        while state.idle.len() < self.config.max_idle && state.total < self.config.max_total {
            let sandbox = Sandbox::new(self.engine.clone(), &self.module)?;
            self.created_total.fetch_add(1, Ordering::Relaxed);
            state.total += 1;
            state.idle.push_back(sandbox);
        }
        Ok(())
    }

    /// Check out a sandbox from the pool, instantiating a new one if needed.
    pub fn checkout(&self) -> Result<PooledSandbox<'_>, PoolError> {
        if self.shutdown_flag.load(Ordering::Acquire) {
            return Err(PoolError::Shutdown);
        }

        let deadline = Instant::now() + self.config.checkout_timeout;
        let mut state = self.state.lock().unwrap();

        loop {
            if state.shutdown {
                return Err(PoolError::Shutdown);
            }

            if let Some(mut sandbox) = state.idle.pop_front() {
                // Health check before handing out.
                if let Err(e) = sandbox.health_check() {
                    state.total -= 1;
                    self.recycled_total.fetch_add(1, Ordering::Relaxed);
                    drop(state);
                    // Try to replace the unhealthy sandbox and retry.
                    let _ = self.replenish_one();
                    if matches!(e, PoolError::Unhealthy(_)) {
                        // Continue loop to try another sandbox.
                        state = self.state.lock().unwrap();
                        continue;
                    }
                    return Err(e);
                }

                sandbox.reuse_count += 1;
                sandbox.last_used = Instant::now();
                self.checked_out.fetch_add(1, Ordering::Relaxed);
                return Ok(PooledSandbox {
                    pool: self,
                    sandbox: Some(sandbox),
                });
            }

            // No idle sandbox available; create one if under capacity.
            if state.total < self.config.max_total {
                state.total += 1;
                drop(state);
                match Sandbox::new(self.engine.clone(), &self.module) {
                    Ok(mut sandbox) => {
                        self.created_total.fetch_add(1, Ordering::Relaxed);
                        sandbox.reuse_count = 1;
                        self.checked_out.fetch_add(1, Ordering::Relaxed);
                        return Ok(PooledSandbox {
                            pool: self,
                            sandbox: Some(sandbox),
                        });
                    }
                    Err(e) => {
                        let mut state = self.state.lock().unwrap();
                        state.total -= 1;
                        return Err(e);
                    }
                }
            }

            // At capacity; wait for a sandbox to be returned.
            let now = Instant::now();
            if now >= deadline {
                return Err(PoolError::Timeout);
            }
            let (new_state, timeout_result) = self
                .available
                .wait_timeout(state, deadline - now)
                .unwrap();
            state = new_state;
            if timeout_result.timed_out() && state.idle.is_empty() {
                return Err(PoolError::Timeout);
            }
        }
    }

    /// Return a sandbox to the pool after scrubbing and health checks.
    fn return_sandbox(&self, mut sandbox: Sandbox) {
        self.checked_out.fetch_sub(1, Ordering::Relaxed);

        if self.shutdown_flag.load(Ordering::Acquire) {
            let mut state = self.state.lock().unwrap();
            state.total = state.total.saturating_sub(1);
            return;
        }

        // Recycle if the sandbox has been reused too many times.
        if sandbox.reuse_count >= self.config.max_reuse_count {
            let mut state = self.state.lock().unwrap();
            state.total = state.total.saturating_sub(1);
            self.recycled_total.fetch_add(1, Ordering::Relaxed);
            self.available.notify_one();
            return;
        }

        // Scrub memory absolutely before pooling.
        if let Err(_e) = sandbox.scrub() {
            let mut state = self.state.lock().unwrap();
            state.total = state.total.saturating_sub(1);
            self.recycled_total.fetch_add(1, Ordering::Relaxed);
            self.available.notify_one();
            return;
        }

        // Optional health check on return.
        if self.config.health_check_on_return && sandbox.health_check().is_err() {
            let mut state = self.state.lock().unwrap();
            state.total = state.total.saturating_sub(1);
            self.recycled_total.fetch_add(1, Ordering::Relaxed);
            self.available.notify_one();
            return;
        }

        sandbox.last_used = Instant::now();

        let mut state = self.state.lock().unwrap();
        if state.shutdown || state.idle.len() >= self.config.max_idle {
            // Pool is full or shutting down; drop the sandbox.
            state.total = state.total.saturating_sub(1);
        } else {
            state.idle.push_back(sandbox);
        }
        drop(state);
        self.available.notify_one();
    }

    /// Attempt to create one replacement sandbox to keep the pool warm.
    fn replenish_one(&self) -> Result<(), PoolError> {
        let mut state = self.state.lock().unwrap();
        if state.shutdown
            || state.idle.len() >= self.config.max_idle
            || state.total >= self.config.max_total
        {
            return Ok(());
        }
        state.total += 1;
        drop(state);

        match Sandbox::new(self.engine.clone(), &self.module) {
            Ok(sandbox) => {
                self.created_total.fetch_add(1, Ordering::Relaxed);
                let mut state = self.state.lock().unwrap();
                if state.shutdown || state.idle.len() >= self.config.max_idle {
                    state.total = state.total.saturating_sub(1);
                } else {
                    state.idle.push_back(sandbox);
                }
                drop(state);
                self.available.notify_one();
                Ok(())
            }
            Err(e) => {
                let mut state = self.state.lock().unwrap();
                state.total = state.total.saturating_sub(1);
                Err(e)
            }
        }
    }

    /// Number of idle sandboxes currently in the pool.
    pub fn idle_count(&self) -> usize {
        self.state.lock().unwrap().idle.len()
    }

    /// Number of sandboxes currently checked out.
    pub fn checked_out_count(&self) -> usize {
        self.checked_out.load(Ordering::Relaxed)
    }

    /// Total number of sandboxes created over the pool's lifetime.
    pub fn created_total(&self) -> u64 {
        self.created_total.load(Ordering::Relaxed)
    }

    /// Total number of sandboxes recycled due to failures.
    pub fn recycled_total(&self) -> u64 {
        self.recycled_total.load(Ordering::Relaxed)
    }

    /// Shut down the pool, dropping all idle sandboxes and waking waiters.
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        state.shutdown = true;
        state.idle.clear();
        state.total = 0;
        drop(state);
        self.available.notify_all();
    }
}

impl Drop for SandboxPool {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.idle.clear();
        }
        self.available.notify_all();
    }
}

/// RAII guard returned by [`SandboxPool::checkout`].
///
/// When dropped, the sandbox is scrubbed and returned to the pool.
pub struct PooledSandbox<'a> {
    pool: &'a SandboxPool,
    sandbox: Option<Sandbox>,
}

impl<'a> PooledSandbox<'a> {
    /// Access the underlying sandbox.
    pub fn sandbox(&mut self) -> &mut Sandbox {
        self.sandbox.as_mut().expect("sandbox already returned")
    }

    /// Access the underlying store.
    pub fn store_mut(&mut self) -> &mut Store<()> {
        self.sandbox().store_mut()
    }

    /// Access the underlying instance.
    pub fn instance(&self) -> &Instance {
        self.sandbox
            .as_ref()
            .expect("sandbox already returned")
            .instance()
    }

    /// Access the underlying linear memory.
    pub fn memory(&self) -> &Memory {
        self.sandbox
            .as_ref()
            .expect("sandbox already returned")
            .memory()
    }
}

impl<'a> Drop for PooledSandbox<'a> {
    fn drop(&mut self) {
        if let Some(sandbox) = self.sandbox.take() {
            self.pool.return_sandbox(sandbox);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT_A_WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "write") (param i32 i32)
                local.get 0
                local.get 1
                i32.store)
            (func (export "read") (param i32) (result i32)
                local.get 0
                i32.load))
    "#;

    fn make_pool() -> Arc<SandboxPool> {
        let config = SandboxPoolConfig {
            max_idle: 2,
            max_total: 4,
            checkout_timeout: Duration::from_secs(2),
            max_reuse_count: 100,
            health_check_on_return: true,
        };
        SandboxPool::from_bytes(CONTRACT_A_WAT.as_bytes(), config).unwrap()
    }

    #[test]
    fn test_checkout_and_return() {
        let pool = make_pool();
        assert_eq!(pool.idle_count(), 2);

        {
            let mut sb = pool.checkout().unwrap();
            assert_eq!(pool.checked_out_count(), 1);
            let _ = sb.store_mut();
        }
        assert_eq!(pool.checked_out_count(), 0);
        assert_eq!(pool.idle_count(), 2);
    }

    #[test]
    fn test_memory_is_scrubbed_between_uses() {
        let pool = make_pool();

        // Contract A writes a sentinel value.
        {
            let mut sb = pool.checkout().unwrap();
            let write = sb
                .instance()
                .get_typed_func::<(i32, i32), ()>(sb.store_mut(), "write")
                .unwrap();
            write.call(sb.store_mut(), (0, 0xDEAD_BEEF_u32 as i32)).unwrap();
        }

        // Contract B reads the same address; it must observe zero.
        {
            let mut sb = pool.checkout().unwrap();
            let read = sb
                .instance()
                .get_typed_func::<i32, i32>(sb.store_mut(), "read")
                .unwrap();
            let value = read.call(sb.store_mut(), 0).unwrap();
            assert_eq!(value, 0, "residual memory leaked between sandboxes");
        }
    }

    #[test]
    fn test_pool_respects_max_total() {
        let config = SandboxPoolConfig {
            max_idle: 1,
            max_total: 1,
            checkout_timeout: Duration::from_millis(50),
            max_reuse_count: 100,
            health_check_on_return: true,
        };
        let pool = SandboxPool::from_bytes(CONTRACT_A_WAT.as_bytes(), config).unwrap();

        let _held = pool.checkout().unwrap();
        let err = pool.checkout().unwrap_err();
        assert!(matches!(err, PoolError::Timeout));
    }

    #[test]
    fn test_shutdown_rejects_checkout() {
        let pool = make_pool();
        pool.shutdown();
        let err = pool.checkout().unwrap_err();
        assert!(matches!(err, PoolError::Shutdown));
    }

    #[test]
    fn test_invalid_config() {
        let config = SandboxPoolConfig {
            max_idle: 10,
            max_total: 5,
            ..Default::default()
        };
        let err = SandboxPool::from_bytes(CONTRACT_A_WAT.as_bytes(), config).unwrap_err();
        assert!(matches!(err, PoolError::InvalidConfig(_)));
    }
}