//! WASM module caching layer for RPC simulation.

pub mod cache;
#[cfg(feature = "wasmtime")]`r`npub mod compiler;

pub use cache::{CacheKey, CacheStats, WasmCache};
