// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! WASM Admission Policy Runner
//!
//! This module provides a secure, sandboxed [`WasmRunner`] that executes
//! compiled WebAssembly admission-policy plugins loaded at runtime from
//! Kubernetes ConfigMaps.
//!
//! # Security model
//!
//! Each execution runs inside an isolated Wasmtime [`Store`] with:
//!
//! - **Fuel metering** — each plugin is limited to [`DEFAULT_FUEL`] Wasm
//!   instructions, preventing infinite loops.
//! - **Memory cap** — the linear memory is capped at [`MAX_MEMORY_BYTES`]
//!   (4 MiB by default) to prevent unconstrained heap growth in the operator
//!   thread.
//! - **Execution timeout** — the caller passes a `deadline` using
//!   [`std::time::Instant`]; if the deadline is exceeded before Wasmtime
//!   returns, the runner surfaces a [`WasmRunnerError::Timeout`].
//! - **No host access** — plugins cannot open files, sockets, or spawn
//!   processes.  The only interface is the two-function host ABI described
//!   below.
//!
//! # Host ABI
//!
//! The Wasmtime engine exposes two host functions imported from the `env`
//! module by every plugin:
//!
//! ```text
//! env::get_input_len() -> i32    Returns byte-length of the JSON input.
//! env::read_input(ptr, len) -> i32  Copies input into guest memory.
//! env::write_output(ptr, len) -> i32  Copies output out of guest memory.
//! env::log_message(ptr, len)         Emits a log line in the operator logs.
//! ```
//!
//! The plugin is expected to export a single `validate() -> i32` function.
//! A return value of `0` means **allow**; any non-zero value means **deny**.
//!
//! # Memory safety
//!
//! The [`WasmRunner`] is intentionally **not** `Clone` or `Send` across
//! long-lived tasks — each `run_plugin` call borrows `&self` and creates a
//! fresh Wasmtime `Store` + `Linker` stack, so there is **no shared mutable
//! state** between invocations and no risk of cross-request contamination.
//!
//! [`Store`]: wasmtime::Store

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument, warn};
use wasmtime::{Config, Engine, Extern, Linker, Module, Store};
use wasmtime_wasi::preview1::{add_to_linker_sync as add_wasi_to_linker, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Fuel budget per plugin execution (≈ 5 million Wasm instructions).
/// Prevents policies from executing runaway loops that block the webhook thread.
const DEFAULT_FUEL: u64 = 5_000_000;

/// Maximum linear-memory the guest may allocate, in bytes (4 MiB).
const MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// Safety margin before the Kubernetes 10-second deadline.
/// Plugins must finish within this window so the webhook can still form a
/// response and send it before the hard API-server timeout.
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(9);

/// Name of the Wasm export entry-point expected in every plugin module.
const VALIDATE_FUNC: &str = "validate";

// ─────────────────────────────────────────────────────────────────────────────
// Policy types  (serialised to/from the guest memory buffer)
// ─────────────────────────────────────────────────────────────────────────────

/// Serialisable representation of a Kubernetes AdmissionRequest for WASM plugins.
///
/// Only the fields relevant to policy evaluation are included.  The full
/// `AdmissionReview` body is never passed to the guest to keep the attack
/// surface small.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyInput {
    /// `CREATE`, `UPDATE`, `DELETE`, or `CONNECT`
    pub operation: String,
    /// Kubernetes namespace of the object being admitted.
    pub namespace: String,
    /// Name of the object being admitted.
    pub name: String,
    /// `stellar.org/v1alpha1/StellarNode` or similar.
    pub resource_kind: String,
    /// The `spec` field of the admitted object as a raw JSON value.
    pub object_spec: serde_json::Value,
    /// Annotations on the submitted object.
    #[serde(default)]
    pub annotations: std::collections::HashMap<String, String>,
    /// Labels on the submitted object.
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

/// Result emitted by a WASM plugin back to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyOutput {
    /// Whether the admission request should be allowed.
    pub allowed: bool,
    /// Human-readable reason returned to `kubectl` / the API server when
    /// `allowed == false`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Non-fatal warnings surfaced in the API-server response.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl PolicyOutput {
    /// Convenience: allow with no message.
    pub fn allow() -> Self {
        Self { allowed: true, reason: None, warnings: vec![] }
    }

    /// Convenience: deny with a human-readable reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self { allowed: false, reason: Some(reason.into()), warnings: vec![] }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during WASM plugin execution.
#[derive(Debug, thiserror::Error)]
pub enum WasmRunnerError {
    /// The plugin ran out of fuel before returning.
    #[error("WASM plugin exceeded instruction budget (fuel = {DEFAULT_FUEL})")]
    FuelExhausted,

    /// The wall-clock timeout fired before the plugin returned.
    #[error("WASM plugin exceeded execution timeout ({timeout:?})")]
    Timeout { timeout: Duration },

    /// The plugin tried to allocate more memory than [`MAX_MEMORY_BYTES`].
    #[error("WASM plugin exceeded memory limit ({MAX_MEMORY_BYTES} bytes)")]
    MemoryExceeded,

    /// The plugin module does not export the required `validate` function.
    #[error("WASM plugin missing export: '{VALIDATE_FUNC}'")]
    MissingExport,

    /// The compiled WASM bytes were invalid or the module failed to link.
    #[error("WASM plugin compile/link error: {0}")]
    Compile(#[from] anyhow::Error),

    /// The plugin produced output that could not be deserialised.
    #[error("WASM plugin output serialisation error: {0}")]
    BadOutput(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Compiled-module cache entry
// ─────────────────────────────────────────────────────────────────────────────

/// A compiled (AOT-ready) Wasmtime module, ready for cheap re-instantiation.
///
/// Store this in a `HashMap<String, CompiledPlugin>` indexed by ConfigMap key
/// to avoid re-parsing the WASM binary on every admission request.
#[derive(Clone)]
pub struct CompiledPlugin {
    /// The Wasmtime engine this module was compiled with.  Must be the same
    /// engine that instantiates the module.
    engine: Arc<Engine>,
    /// The compiled module (heap-allocated, `Send + Sync`).
    module: Arc<Module>,
    /// Human-readable name, used in log lines.
    pub name: String,
    /// Whether the plugin runs in fail-open mode.  If `true`, execution errors
    /// are logged but the admission request is still allowed to proceed.
    pub fail_open: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// WasmRunner
// ─────────────────────────────────────────────────────────────────────────────

/// Sandboxed Wasmtime-based execution engine for WASM admission policies.
///
/// Create one `WasmRunner` for the lifetime of the webhook server.  Individual
/// `run_plugin` calls are cheap because the Wasmtime `Engine` (and
/// `CompiledPlugin` cache) are reused; only the `Store` is per-invocation.
pub struct WasmRunner {
    engine: Arc<Engine>,
}

impl WasmRunner {
    /// Construct a new `WasmRunner` with safe defaults.
    ///
    /// Fuel metering is enabled; epoch-based interruption is **not** used
    /// because it requires an OS thread to tick the epoch counter, which
    /// introduces unnecessary complexity.  Instead the caller is expected to
    /// honour `deadline` and abort on [`WasmRunnerError::Timeout`].
    pub fn new() -> anyhow::Result<Self> {
        let mut config = Config::new();
        // Enable the deterministic fuel mechanism so we can cap CPU.
        config.consume_fuel(true);
        // Disable WASI capabilities by default; each invocation opts in explicitly.
        // Compile with optimisations for fast execution.
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);
        // Allow modules to be shared across threads (required for Arc<Module>).
        config.parallel_compilation(true);

        let engine = Engine::new(&config).context("Failed to create Wasmtime engine")?;
        Ok(Self { engine: Arc::new(engine) })
    }

    /// Compile WASM bytes into a [`CompiledPlugin`] ready for instantiation.
    ///
    /// This is the expensive step (Cranelift JIT compilation).  Cache the
    /// returned [`CompiledPlugin`] and reuse it across requests.
    ///
    /// # Arguments
    ///
    /// * `name`       – A human-readable name used in logs.
    /// * `wasm_bytes` – Raw `.wasm` binary (not WAT text format).
    /// * `fail_open`  – If `true`, runtime errors allow the request through.
    #[instrument(skip(self, wasm_bytes), fields(plugin = %name))]
    pub fn compile(
        &self,
        name: impl Into<String>,
        wasm_bytes: &[u8],
        fail_open: bool,
    ) -> anyhow::Result<CompiledPlugin> {
        let name = name.into();
        let module =
            Module::from_binary(&self.engine, wasm_bytes).context("Failed to compile WASM module")?;

        info!(%name, "compiled WASM plugin ({} bytes)", wasm_bytes.len());

        Ok(CompiledPlugin {
            engine: Arc::clone(&self.engine),
            module: Arc::new(module),
            name,
            fail_open,
        })
    }

    /// Execute a compiled plugin against the given `input`, returning the
    /// plugin's [`PolicyOutput`].
    ///
    /// This method:
    ///
    /// 1. Creates a fresh Wasmtime `Store` bounded by `fuel` and `memory_limit`.
    /// 2. Links the host ABI (`get_input_len`, `read_input`, `write_output`, `log_message`).
    /// 3. Instantiates the module.
    /// 4. Calls the `validate()` export.
    /// 5. Reads the output written by the plugin into the host output buffer.
    /// 6. Returns [`WasmRunnerError::Timeout`] if the deadline has already
    ///    passed when the call returns (Wasmtime is synchronous; we cannot
    ///    interrupt mid-execution without epoch ticking).
    ///
    /// # Memory-safety note
    ///
    /// The `Store` and all guest memory references are dropped at the end of
    /// this call.  There is no shared mutable state between invocations;
    /// separate calls may be made concurrently on different OS threads.
    #[instrument(skip(self, plugin, input), fields(plugin = %plugin.name))]
    pub fn run_plugin(
        &self,
        plugin: &CompiledPlugin,
        input: &PolicyInput,
        deadline: Instant,
    ) -> Result<PolicyOutput, WasmRunnerError> {
        // ── 1. Serialise the input to JSON ────────────────────────────────────
        let input_json =
            serde_json::to_vec(input).map_err(|e| WasmRunnerError::Compile(e.into()))?;

        // ── 2. Shared host buffers (wrapped in Arc<Mutex<…>>) ─────────────────
        // We use `Arc<std::sync::Mutex<…>>` rather than `RefCell` because the
        // Wasmtime linker requires `Fn` (not `FnMut`) closures.
        let input_buf = Arc::new(std::sync::Mutex::new(input_json));
        let output_buf: Arc<std::sync::Mutex<Vec<u8>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        // ── 3. Build the Wasmtime store ───────────────────────────────────────
        let wasi_ctx = WasiCtxBuilder::new().build_p1();

        struct HostState {
            wasi: WasiP1Ctx,
            input: Arc<std::sync::Mutex<Vec<u8>>>,
            output: Arc<std::sync::Mutex<Vec<u8>>>,
        }

        let state = HostState {
            wasi: wasi_ctx,
            input: Arc::clone(&input_buf),
            output: Arc::clone(&output_buf),
        };

        let mut store: Store<HostState> = Store::new(&plugin.engine, state);

        // Cap instructions (fuel).
        store
            .add_fuel(DEFAULT_FUEL)
            .map_err(|e| WasmRunnerError::Compile(e))?;

        // Cap linear memory growth.
        store.limiter(|_state| &mut StoreLimiter);

        // ── 4. Build the linker with WASI and the host ABI ────────────────────
        let mut linker: Linker<HostState> = Linker::new(&plugin.engine);

        // Register WASI-preview1 system calls (no-op stubs when WASI isn't used).
        add_wasi_to_linker(&mut linker, |s: &mut HostState| &mut s.wasi)
            .map_err(|e| WasmRunnerError::Compile(e))?;

        // -- get_input_len() → i32
        {
            let buf = Arc::clone(&input_buf);
            linker
                .func_wrap("env", "get_input_len", move |_caller: wasmtime::Caller<'_, HostState>| -> i32 {
                    buf.lock().map(|b| b.len() as i32).unwrap_or(-1)
                })
                .map_err(|e| WasmRunnerError::Compile(e))?;
        }

        // -- read_input(ptr: i32, len: i32) → i32
        {
            let buf = Arc::clone(&input_buf);
            linker
                .func_wrap(
                    "env",
                    "read_input",
                    move |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                        let src = match buf.lock() {
                            Ok(b) => b.clone(),
                            Err(_) => return -1,
                        };
                        let to_copy = (len as usize).min(src.len());
                        if to_copy == 0 {
                            return 0;
                        }
                        let mem = match caller.get_export("memory") {
                            Some(Extern::Memory(m)) => m,
                            _ => return -1,
                        };
                        let mem_data = mem.data_mut(&mut caller);
                        let start = ptr as usize;
                        if start + to_copy > mem_data.len() {
                            return -1;
                        }
                        mem_data[start..start + to_copy].copy_from_slice(&src[..to_copy]);
                        to_copy as i32
                    },
                )
                .map_err(|e| WasmRunnerError::Compile(e))?;
        }

        // -- write_output(ptr: i32, len: i32) → i32
        {
            let buf = Arc::clone(&output_buf);
            linker
                .func_wrap(
                    "env",
                    "write_output",
                    move |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
                        let mem = match caller.get_export("memory") {
                            Some(Extern::Memory(m)) => m,
                            _ => return -1,
                        };
                        let mem_data = mem.data(&caller);
                        let start = ptr as usize;
                        let end = start + len as usize;
                        if end > mem_data.len() {
                            return -1;
                        }
                        match buf.lock() {
                            Ok(mut b) => {
                                b.clear();
                                b.extend_from_slice(&mem_data[start..end]);
                                0
                            }
                            Err(_) => -1,
                        }
                    },
                )
                .map_err(|e| WasmRunnerError::Compile(e))?;
        }

        // -- log_message(ptr: i32, len: i32)
        {
            let plugin_name = plugin.name.clone();
            linker
                .func_wrap(
                    "env",
                    "log_message",
                    move |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| {
                        let mem = match caller.get_export("memory") {
                            Some(Extern::Memory(m)) => m,
                            _ => return,
                        };
                        let mem_data = mem.data(&caller);
                        let start = ptr as usize;
                        let end = (start + len as usize).min(mem_data.len());
                        if let Ok(msg) = std::str::from_utf8(&mem_data[start..end]) {
                            debug!(plugin = %plugin_name, wasm = msg, "wasm_plugin log");
                        }
                    },
                )
                .map_err(|e| WasmRunnerError::Compile(e))?;
        }

        // ── 5. Instantiate and call `validate` ────────────────────────────────
        let instance = linker
            .instantiate(&mut store, &plugin.module)
            .map_err(|e| WasmRunnerError::Compile(e))?;

        let validate_fn = instance
            .get_typed_func::<(), i32>(&mut store, VALIDATE_FUNC)
            .map_err(|_| WasmRunnerError::MissingExport)?;

        let ret = validate_fn.call(&mut store, ()).map_err(|e| {
            // Distinguish fuel exhaustion from other trap types.
            if e.to_string().contains("fuel") || e.to_string().contains("out of fuel") {
                WasmRunnerError::FuelExhausted
            } else {
                WasmRunnerError::Compile(e)
            }
        })?;

        // ── 6. Check wall-clock deadline ──────────────────────────────────────
        if Instant::now() > deadline {
            warn!(plugin = %plugin.name, "WASM plugin returned after deadline");
            return Err(WasmRunnerError::Timeout { timeout: WEBHOOK_TIMEOUT });
        }

        // ── 7. Parse the output written by the guest ──────────────────────────
        let raw_output = output_buf.lock().map_err(|_| {
            WasmRunnerError::BadOutput("output buffer lock poisoned".into())
        })?.clone();

        if raw_output.is_empty() {
            // Plugin returned a non-zero i32 without calling write_output — treat
            // non-zero return as deny with no message.
            debug!(plugin = %plugin.name, ret, "plugin returned without calling write_output");
            if ret == 0 {
                return Ok(PolicyOutput::allow());
            } else {
                return Ok(PolicyOutput::deny(format!(
                    "Policy '{}' denied the request (exit code {ret})",
                    plugin.name
                )));
            }
        }

        let output: PolicyOutput = serde_json::from_slice(&raw_output).map_err(|e| {
            WasmRunnerError::BadOutput(format!("failed to deserialise plugin output: {e}"))
        })?;

        info!(
            plugin = %plugin.name,
            allowed = output.allowed,
            reason = ?output.reason,
            "WASM policy evaluation complete"
        );

        Ok(output)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Wasmtime ResourceLimiter (memory cap)
// ─────────────────────────────────────────────────────────────────────────────

struct StoreLimiter;

impl wasmtime::ResourceLimiter for StoreLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        if desired > MAX_MEMORY_BYTES {
            error!(
                current_bytes = current,
                desired_bytes = desired,
                limit = MAX_MEMORY_BYTES,
                "WASM plugin exceeded memory limit"
            );
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: u32,
        _desired: u32,
        _maximum: Option<u32>,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WAT-format plugin that reads its input and always returns allow.
    ///
    /// The plugin does NOT call `write_output`, so the runner falls back to
    /// interpreting the i32 return value (0 = allow).
    const ALLOW_WAT: &str = r#"
    (module
      (import "env" "get_input_len" (func $get_input_len (result i32)))
      (import "env" "read_input"    (func $read_input    (param i32 i32) (result i32)))
      (import "env" "write_output"  (func $write_output  (param i32 i32) (result i32)))
      (import "env" "log_message"   (func $log_message   (param i32 i32)))
      (memory (export "memory") 1)
      (func (export "validate") (result i32)
        i32.const 0
      )
    )
    "#;

    /// Plugin that always denies (returns 1 without writing output).
    const DENY_WAT: &str = r#"
    (module
      (import "env" "get_input_len" (func $get_input_len (result i32)))
      (import "env" "read_input"    (func $read_input    (param i32 i32) (result i32)))
      (import "env" "write_output"  (func $write_output  (param i32 i32) (result i32)))
      (import "env" "log_message"   (func $log_message   (param i32 i32)))
      (memory (export "memory") 1)
      (func (export "validate") (result i32)
        i32.const 1
      )
    )
    "#;

    fn make_input() -> PolicyInput {
        PolicyInput {
            operation: "CREATE".into(),
            namespace: "stellar".into(),
            name: "my-validator".into(),
            resource_kind: "StellarNode".into(),
            object_spec: serde_json::json!({ "version": "v21.0.0" }),
            annotations: Default::default(),
            labels: Default::default(),
        }
    }

    #[test]
    fn allow_plugin_returns_allowed() {
        let runner = WasmRunner::new().unwrap();
        let wasm = wat::parse_str(ALLOW_WAT).expect("valid WAT");
        let plugin = runner.compile("allow-plugin", &wasm, false).unwrap();
        let deadline = Instant::now() + WEBHOOK_TIMEOUT;
        let result = runner.run_plugin(&plugin, &make_input(), deadline).unwrap();
        assert!(result.allowed, "expected allow, got deny: {:?}", result.reason);
    }

    #[test]
    fn deny_plugin_returns_denied() {
        let runner = WasmRunner::new().unwrap();
        let wasm = wat::parse_str(DENY_WAT).expect("valid WAT");
        let plugin = runner.compile("deny-plugin", &wasm, false).unwrap();
        let deadline = Instant::now() + WEBHOOK_TIMEOUT;
        let result = runner.run_plugin(&plugin, &make_input(), deadline).unwrap();
        assert!(!result.allowed, "expected deny");
    }

    #[test]
    fn fail_open_config_preserved() {
        let runner = WasmRunner::new().unwrap();
        let wasm = wat::parse_str(ALLOW_WAT).unwrap();
        let plugin = runner.compile("test", &wasm, true).unwrap();
        assert!(plugin.fail_open);
    }

    #[test]
    fn missing_export_returns_error() {
        let runner = WasmRunner::new().unwrap();
        // A module with NO exports.
        let wasm = wat::parse_str(
            r#"(module
                 (import "env" "get_input_len" (func (result i32)))
                 (import "env" "read_input"    (func (param i32 i32) (result i32)))
                 (import "env" "write_output"  (func (param i32 i32) (result i32)))
                 (import "env" "log_message"   (func (param i32 i32)))
                 (memory 1)
               )"#,
        )
        .unwrap();
        let plugin = runner.compile("no-export", &wasm, false).unwrap();
        let deadline = Instant::now() + WEBHOOK_TIMEOUT;
        let err = runner.run_plugin(&plugin, &make_input(), deadline).unwrap_err();
        assert!(
            matches!(err, WasmRunnerError::MissingExport),
            "unexpected error: {err}"
        );
    }
}
