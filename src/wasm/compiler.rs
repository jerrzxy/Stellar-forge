//! Wasmtime-backed compiler that serves modules through [`WasmCache`].
//!
//! Each simulation must instantiate the returned `Module` in its own `Store`,
//! giving every call a private linear memory (strict isolation).

use std::sync::Arc;

use wasmtime::{Engine, Module};

use super::cache::{CacheKey, WasmCache};

/// Compiles WASM bytecode once per `(contract_id, ledger_sequence)`.
pub struct WasmCompiler {
    engine: Engine,
    cache: Arc<WasmCache<Module>>,
}

impl WasmCompiler {
    pub fn new(capacity: usize) -> wasmtime::Result<Self> {
        Ok(Self {
            engine: Engine::default(),
            cache: Arc::new(WasmCache::new(capacity)),
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn cache(&self) -> &Arc<WasmCache<Module>> {
        &self.cache
    }

    /// Get a warm compiled module, compiling `wasm` only on a cache miss.
    pub fn module_for(
        &self,
        contract_id: &str,
        ledger_sequence: u32,
        wasm: &[u8],
    ) -> wasmtime::Result<Arc<Module>> {
        let key = CacheKey::new(contract_id, ledger_sequence);
        self.cache
            .get_or_compile(&key, || Module::new(&self.engine, wasm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_once_per_ledger() {
        let c = WasmCompiler::new(4).unwrap();
        let wasm = b"(module (func (export \"f\")))".to_vec();
        c.module_for("C1", 1, &wasm).unwrap();
        c.module_for("C1", 1, &wasm).unwrap();
        c.module_for("C1", 2, &wasm).unwrap();
        let s = c.cache().stats();
        assert_eq!((s.hits, s.misses), (1, 2));
    }
}
