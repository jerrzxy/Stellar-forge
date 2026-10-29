// controller/src/wasm/memory_scrubber.rs
//
// Absolute linear-memory scrubbing for pooled WASM execution sandboxes.
//
// This module is responsible for guaranteeing that no state written by one
// smart contract invocation can be observed by a subsequent invocation that
// reuses the same pooled sandbox. The scrubbing routine is intentionally
// paranoid: it zeroes every linear memory page, resets the store/instance
// state, and clears any host-side buffers that may have captured data.
//
// SECURITY NOTE: Leaking state between consecutive contract executions within
// a pooled sandbox represents a critical vulnerability. Any change to this
// module must be reviewed by security engineers and must preserve the
// "absolute sanitization" invariant validated by the fuzz tests below.

use std::sync::atomic::{compiler_fence, Ordering};

use wasmtime::{Instance, Memory, Store, StoreContextMut};

/// Errors that can occur while scrubbing a sandbox.
#[derive(Debug, thiserror::Error)]
pub enum ScrubError {
    #[error("failed to access linear memory: {0}")]
    MemoryAccess(String),
    #[error("failed to reset instance state: {0}")]
    InstanceReset(String),
    #[error("scrub verification failed: residual data detected at offset {0}")]
    ResidualData(usize),
}

/// A host-side buffer that may have captured data during an execution and
/// therefore must be cleared before the sandbox is returned to the pool.
pub trait HostBuffer {
    /// Zero out the entire contents of the buffer.
    fn clear_secure(&mut self);
    /// Return the current length of the buffer (used for verification).
    fn len(&self) -> usize;
    /// Return a copy of the buffer contents for verification purposes.
    fn snapshot(&self) -> Vec<u8>;
}

impl HostBuffer for Vec<u8> {
    fn clear_secure(&mut self) {
        // Overwrite with zeros before truncating so that the allocator does
        // not hand back stale bytes to a future allocation.
        for byte in self.iter_mut() {
            // Use a volatile write to prevent the optimizer from eliding the
            // zeroing of memory that is about to be freed/truncated.
            unsafe { std::ptr::write_volatile(byte, 0u8) };
        }
        compiler_fence(Ordering::SeqCst);
        self.clear();
        self.shrink_to_fit();
    }

    fn len(&self) -> usize {
        Vec::len(self)
    }

    fn snapshot(&self) -> Vec<u8> {
        self.clone()
    }
}

/// Configuration controlling how aggressively the scrubber operates.
#[derive(Debug, Clone, Copy)]
pub struct ScrubConfig {
    /// If true, verify after scrubbing that every byte is zero. This is
    /// expensive but recommended for high-assurance deployments.
    pub verify: bool,
    /// If true, also reset the instance's exported globals to their initial
    /// values (best-effort; requires the instance to expose a reset hook).
    pub reset_instance_state: bool,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            verify: true,
            reset_instance_state: true,
        }
    }
}

/// The result of a scrub operation, useful for telemetry and assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrubReport {
    /// Number of linear memory pages that were zeroed.
    pub pages_zeroed: u64,
    /// Number of bytes zeroed across all linear memories.
    pub bytes_zeroed: u64,
    /// Number of host-side buffers that were cleared.
    pub host_buffers_cleared: usize,
    /// Whether the post-scrub verification passed.
    pub verified: bool,
}

/// The core scrubber. Holds no state of its own; all state lives in the
/// sandbox being scrubbed. This makes it trivially `Send + Sync` and safe to
/// share across worker threads.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryScrubber {
    config: ScrubConfig,
}

impl MemoryScrubber {
    pub fn new(config: ScrubConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(ScrubConfig::default())
    }

    /// Scrub a single linear memory: zero every byte across every page.
    ///
    /// We deliberately operate on the full `memory.data_size(store)` rather
    /// than only the currently "used" region, because a contract may have
    /// grown memory and then shrunk its logical usage while leaving bytes
    /// behind in the tail of the last page.
    pub fn scrub_memory<T>(
        &self,
        store: &mut Store<T>,
        memory: &Memory,
    ) -> Result<u64, ScrubError> {
        let data_size = memory
            .data_size(&*store)
            .try_into()
            .map_err(|_| ScrubError::MemoryAccess("memory size overflow".into()))?;

        if data_size == 0 {
            return Ok(0);
        }

        // Zero the entire linear memory in one shot. `data_mut` gives us a
        // mutable slice over the whole backing allocation.
        {
            let data = memory
                .data_mut(&mut *store)
                .try_into()
                .map_err(|_| ScrubError::MemoryAccess("memory size overflow".into()))?;
            let data: &mut [u8] = data;
            // Volatile writes ensure the compiler cannot elide the zeroing
            // even if it believes the memory is dead.
            for byte in data.iter_mut() {
                unsafe { std::ptr::write_volatile(byte, 0u8) };
            }
            compiler_fence(Ordering::SeqCst);
        }

        if self.config.verify {
            let data = memory
                .data(&*store)
                .try_into()
                .map_err(|_| ScrubError::MemoryAccess("memory size overflow".into()))?;
            let data: &[u8] = data;
            if let Some(offset) = data.iter().position(|&b| b != 0) {
                return Err(ScrubError::ResidualData(offset));
            }
        }

        Ok(data_size)
    }

    /// Scrub every linear memory exported by the given instance.
    pub fn scrub_instance_memories<T>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
    ) -> Result<(u64, u64), ScrubError> {
        let mut pages_zeroed = 0u64;
        let mut bytes_zeroed = 0u64;

        for export in instance.exports(&mut *store) {
            if let Ok(memory) = export.clone().into_memory() {
                let bytes = self.scrub_memory(store, &memory)?;
                bytes_zeroed = bytes_zeroed.saturating_add(bytes);
                pages_zeroed = pages_zeroed.saturating_add(bytes / 65536);
            }
        }

        Ok((pages_zeroed, bytes_zeroed))
    }

    /// Best-effort reset of instance state. If the instance exports a
    /// function named `__reset_state`, we invoke it after memory has been
    /// zeroed so that any host-visible globals are restored to defaults.
    pub fn reset_instance_state<T>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
    ) -> Result<(), ScrubError> {
        if !self.config.reset_instance_state {
            return Ok(());
        }

        let reset_fn = instance
            .get_typed_func::<(), ()>(&mut *store, "__reset_state")
            .ok();

        if let Some(func) = reset_fn {
            func.call(&mut *store, ())
                .map_err(|e| ScrubError::InstanceReset(e.to_string()))?;
        }

        Ok(())
    }

    /// Clear all host-side buffers that may have captured data.
    pub fn scrub_host_buffers<B: HostBuffer>(&self, buffers: &mut [B]) -> usize {
        let mut cleared = 0usize;
        for buf in buffers.iter_mut() {
            if buf.len() > 0 {
                buf.clear_secure();
                cleared += 1;
            }
        }
        cleared
    }

    /// Full scrub of a sandbox before it is returned to the warm pool.
    ///
    /// Order matters:
    ///   1. Zero linear memory (the largest, most likely leak surface).
    ///   2. Reset instance state (globals, tables, etc.).
    ///   3. Clear host-side buffers.
    ///   4. Verify (if enabled).
    pub fn scrub<T, B: HostBuffer>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
        host_buffers: &mut [B],
    ) -> Result<ScrubReport, ScrubError> {
        let (pages_zeroed, bytes_zeroed) = self.scrub_instance_memories(store, instance)?;
        self.reset_instance_state(store, instance)?;
        let host_buffers_cleared = self.scrub_host_buffers(host_buffers);

        // Final verification pass: re-check every memory is still zero after
        // the instance-state reset (the reset hook could, in principle, write
        // to memory; we must catch that).
        let mut verified = true;
        if self.config.verify {
            for export in instance.exports(&mut *store) {
                if let Ok(memory) = export.clone().into_memory() {
                    let data = memory
                        .data(&*store)
                        .try_into()
                        .map_err(|_| ScrubError::MemoryAccess("memory size overflow".into()))?;
                    let data: &[u8] = data;
                    if let Some(offset) = data.iter().position(|&b| b != 0) {
                        return Err(ScrubError::ResidualData(offset));
                    }
                }
            }
        } else {
            verified = false;
        }

        Ok(ScrubReport {
            pages_zeroed,
            bytes_zeroed,
            host_buffers_cleared,
            verified,
        })
    }
}

/// Convenience wrapper that scrubs a store/instance pair using default config.
pub fn scrub_sandbox<T, B: HostBuffer>(
    store: &mut Store<T>,
    instance: &Instance,
    host_buffers: &mut [B],
) -> Result<ScrubReport, ScrubError> {
    MemoryScrubber::with_defaults().scrub(store, instance, host_buffers)
}

/// Helper used by the pool to obtain a mutable store context. Kept here so
/// the pool module does not need to know about scrubber internals.
pub fn scrub_with_context<T, B: HostBuffer>(
    ctx: &mut StoreContextMut<'_, T>,
    instance: &Instance,
    host_buffers: &mut [B],
) -> Result<ScrubReport, ScrubError> {
    MemoryScrubber::with_defaults().scrub(&mut *ctx, instance, host_buffers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Module, Store};

    /// Minimal WASM module that exports a linear memory and a function to
    /// write a byte at a given offset, plus a function to read a byte.
    const WRITER_WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "write") (param i32 i32)
                local.get 0
                local.get 1
                i32.store8)
            (func (export "read") (param i32) (result i32)
                local.get 0
                i32.load8_u))
    "#;

    fn instantiate(engine: &Engine) -> (Store<()>, Instance) {
        let module = Module::new(engine, WRITER_WAT).expect("compile module");
        let mut store = Store::new(engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
        (store, instance)
    }

    fn call_write(store: &mut Store<()>, instance: &Instance, offset: i32, value: i32) {
        let f = instance
            .get_typed_func::<(i32, i32), ()>(&mut *store, "write")
            .expect("write export");
        f.call(&mut *store, (offset, value)).expect("write call");
    }

    fn call_read(store: &mut Store<()>, instance: &Instance, offset: i32) -> i32 {
        let f = instance
            .get_typed_func::<i32, i32>(&mut *store, "read")
            .expect("read export");
        f.call(&mut *store, offset).expect("read call")
    }

    #[test]
    fn scrub_zeroes_entire_memory() {
        let engine = Engine::default();
        let (mut store, instance) = instantiate(&engine);

        // Contract A writes a recognizable pattern across the first page.
        for i in 0..1024 {
            call_write(&mut store, &instance, i, 0xAB);
        }

        let scrubber = MemoryScrubber::with_defaults();
        let report = scrubber
            .scrub(&mut store, &instance, &mut [] as &mut [Vec<u8>])
            .expect("scrub");

        assert!(report.verified);
        assert_eq!(report.bytes_zeroed, 65536);

        // Contract B must not observe any residual data.
        for i in 0..1024 {
            assert_eq!(call_read(&mut store, &instance, i), 0, "residual at {i}");
        }
    }

    #[test]
    fn scrub_clears_host_buffers() {
        let engine = Engine::default();
        let (mut store, instance) = instantiate(&engine);

        let mut buffers: Vec<Vec<u8>> = vec![vec![0xDE; 128], vec![0xAD; 64], vec![]];
        let scrubber = MemoryScrubber::with_defaults();
        let report = scrubber
            .scrub(&mut store, &instance, &mut buffers)
            .expect("scrub");

        assert_eq!(report.host_buffers_cleared, 2);
        assert!(buffers.iter().all(|b| b.is_empty()));
    }

    #[test]
    fn contract_a_then_b_cannot_read_residual() {
        // This is the canonical validation described in the issue: run
        // contract A, scrub, run contract B in the same sandbox, and assert
        // B cannot read any of A's data.
        let engine = Engine::default();
        let (mut store, instance) = instantiate(&engine);

        // Contract A: write a secret at a known offset.
        const SECRET_OFFSET: i32 = 4096;
        const SECRET: i32 = 0x5A;
        call_write(&mut store, &instance, SECRET_OFFSET, SECRET);
        assert_eq!(call_read(&mut store, &instance, SECRET_OFFSET), SECRET);

        // Scrub before returning to pool.
        let scrubber = MemoryScrubber::with_defaults();
        scrubber
            .scrub(&mut store, &instance, &mut [] as &mut [Vec<u8>])
            .expect("scrub");

        // Contract B: read the same offset; must be zero.
        assert_eq!(call_read(&mut store, &instance, SECRET_OFFSET), 0);
    }

    /// Fuzz-style test: write random patterns across random offsets, scrub,
    /// and assert every byte in the entire linear memory is zero.
    #[test]
    fn fuzz_scrub_absolute() {
        let engine = Engine::default();
        let (mut store, instance) = instantiate(&engine);
        let scrubber = MemoryScrubber::with_defaults();

        // Deterministic pseudo-random generator so failures are reproducible.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for iteration in 0..64 {
            // Write a random number of random bytes at random offsets.
            let writes = (next() % 256) as usize + 1;
            for _ in 0..writes {
                let offset = (next() % 65536) as i32;
                let value = (next() & 0xFF) as i32;
                call_write(&mut store, &instance, offset, value);
            }

            let report = scrubber
                .scrub(&mut store, &instance, &mut [] as &mut [Vec<u8>])
                .expect("scrub");
            assert!(report.verified, "iteration {iteration} not verified");

            // Exhaustively verify the entire page is zero.
            let memory = instance
                .get_memory(&mut store, "memory")
                .expect("memory export");
            let data = memory.data(&store);
            if let Some(offset) = data.iter().position(|&b| b != 0) {
                panic!("iteration {iteration}: residual byte at offset {offset}");
            }
        }
    }

    /// Fuzz test that also exercises host-side buffers.
    #[test]
    fn fuzz_scrub_with_host_buffers() {
        let engine = Engine::default();
        let (mut store, instance) = instantiate(&engine);
        let scrubber = MemoryScrubber::with_defaults();

        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..32 {
            let mut buffers: Vec<Vec<u8>> = (0..(next() % 8) as usize)
                .map(|_| {
                    let len = (next() % 512) as usize;
                    (0..len).map(|_| (next() & 0xFF) as u8).collect()
                })
                .collect();

            let report = scrubber
                .scrub(&mut store, &instance, &mut buffers)
                .expect("scrub");
            assert!(report.verified);
            assert!(buffers.iter().all(|b| b.is_empty()));
        }
    }

    /// Verify that scrubbing a grown memory zeroes the grown region too.
    #[test]
    fn scrub_handles_grown_memory() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "grow") (param i32) (result i32)
                    local.get 0
                    memory.grow)
                (func (export "write") (param i32 i32)
                    local.get 0
                    local.get 1
                    i32.store8)
                (func (export "read") (param i32) (result i32)
                    local.get 0
                    i32.load8_u))
        "#;
        let engine = Engine::default();
        let module = Module::new(&engine, wat).expect("compile");
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");

        let grow = instance
            .get_typed_func::<i32, i32>(&mut store, "grow")
            .expect("grow");
        grow.call(&mut store, 3).expect("grow call");

        let write = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "write")
            .expect("write");
        // Write into the grown region (page 3).
        write.call(&mut store, (3 * 65536 + 100, 0x77)).expect("write");

        let scrubber = MemoryScrubber::with_defaults();
        let report = scrubber
            .scrub(&mut store, &instance, &mut [] as &mut [Vec<u8>])
            .expect("scrub");
        assert!(report.verified);
        assert_eq!(report.bytes_zeroed, 4 * 65536);

        let read = instance
            .get_typed_func::<i32, i32>(&mut store, "read")
            .expect("read");
        let v = read.call(&mut store, 3 * 65536 + 100).expect("read");
        assert_eq!(v, 0);
    }
}