//! Approved Registry Policy — Stellar-K8s WASM Admission Plugin
//!
//! Enforces that every `StellarNode` `spec.version` which contains an explicit
//! registry prefix originates from an approved registry.
//!
//! # Host ABI
//!
//! Uses the four-function ABI defined in `controller/src/webhook/wasm_runner.rs`:
//!
//! ```text
//! env::get_input_len()  -> i32
//! env::read_input(ptr: i32, len: i32) -> i32
//! env::write_output(ptr: i32, len: i32) -> i32
//! env::log_message(ptr: i32, len: i32)
//! ```
//!
//! Input is a JSON-serialised `PolicyInput`; output is a `PolicyOutput`.
//!
//! # Build
//!
//! ```bash
//! cargo build --target wasm32-unknown-unknown --release -p registry-policy
//! ```

// ─────────────────────────────────────────────────────────────────────────────
// Host function imports
// ─────────────────────────────────────────────────────────────────────────────

extern "C" {
    fn get_input_len() -> i32;
    fn read_input(ptr: *mut u8, len: i32) -> i32;
    fn write_output(ptr: *const u8, len: i32) -> i32;
    fn log_message(ptr: *const u8, len: i32);
}

// ─────────────────────────────────────────────────────────────────────────────
// Approved registry prefixes (extend as needed)
// ─────────────────────────────────────────────────────────────────────────────

const APPROVED_REGISTRIES: &[&str] = &[
    "docker.io/stellar/",
    "ghcr.io/stellar/",
    "gcr.io/stellar-project/",
    "registry.k8s.io/stellar/",
];

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Kubernetes AdmissionWebhook entry point.
///
/// Returns `0` on allow, `1` on deny.
#[no_mangle]
pub extern "C" fn validate() -> i32 {
    match run_policy() {
        Ok((allowed, reason, warnings)) => {
            emit_output(allowed, reason.as_deref(), &warnings);
            if allowed { 0 } else { 1 }
        }
        Err(msg) => {
            log(&format!("registry-policy error: {msg}"));
            emit_output(false, Some(&format!("plugin error: {msg}")), &[]);
            1
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Policy logic
// ─────────────────────────────────────────────────────────────────────────────

fn run_policy() -> Result<(bool, Option<String>, Vec<String>), String> {
    // 1. Read the input buffer from the host.
    let raw = read_host_input()?;

    // 2. Parse as a generic JSON value (PolicyInput schema).
    let input: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("json parse: {e}"))?;

    // 3. Extract fields — camelCase matches PolicyInput in wasm_runner.rs.
    let operation = input["operation"].as_str().unwrap_or("").to_string();
    let spec = input.get("objectSpec");

    // 4. Skip non-write operations.
    if operation != "CREATE" && operation != "UPDATE" {
        return Ok((true, None, vec![]));
    }

    // 5. Extract spec.version.
    let version = spec
        .and_then(|s| s.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    log(&format!("registry-policy: op={operation} version='{version}'"));

    if version.is_empty() {
        return Ok((
            true,
            None,
            vec!["spec.version is empty — skipping registry check".into()],
        ));
    }

    // 6. If the version contains '/', check against the allow-list.
    if version.contains('/') {
        let approved = APPROVED_REGISTRIES
            .iter()
            .any(|prefix| version.starts_with(prefix));

        if !approved {
            let allowed_list = APPROVED_REGISTRIES.join(", ");
            return Ok((
                false,
                Some(format!(
                    "image '{version}' is not from an approved registry. \
                     Allowed prefixes: {allowed_list}"
                )),
                vec![],
            ));
        }
        log(&format!("registry-policy: '{version}' approved ✓"));
    } else {
        // Plain tag (no registry) — allow with warning.
        return Ok((
            true,
            None,
            vec![format!(
                "spec.version '{version}' has no registry prefix — ensure the \
                 cluster image pull policy restricts to approved registries"
            )],
        ));
    }

    Ok((true, None, vec![]))
}

// ─────────────────────────────────────────────────────────────────────────────
// Host ABI helpers
// ─────────────────────────────────────────────────────────────────────────────

fn read_host_input() -> Result<Vec<u8>, String> {
    let len = unsafe { get_input_len() };
    if len < 0 {
        return Err(format!("get_input_len returned {len}"));
    }
    let mut buf = vec![0u8; len as usize];
    let copied = unsafe { read_input(buf.as_mut_ptr(), len) };
    if copied != len {
        return Err(format!("read_input: expected {len} bytes, got {copied}"));
    }
    Ok(buf)
}

fn emit_output(allowed: bool, reason: Option<&str>, warnings: &[String]) {
    // Serialise a PolicyOutput-compatible JSON object.
    let mut obj = serde_json::json!({ "allowed": allowed });
    if let Some(r) = reason {
        obj["reason"] = serde_json::Value::String(r.to_string());
    }
    if !warnings.is_empty() {
        obj["warnings"] = serde_json::Value::Array(
            warnings
                .iter()
                .map(|w| serde_json::Value::String(w.clone()))
                .collect(),
        );
    }
    let bytes = serde_json::to_vec(&obj).unwrap_or_default();
    unsafe { write_output(bytes.as_ptr(), bytes.len() as i32) };
}

fn log(msg: &str) {
    unsafe { log_message(msg.as_ptr(), msg.len() as i32) };
}
