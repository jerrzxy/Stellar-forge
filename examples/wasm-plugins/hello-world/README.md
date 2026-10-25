# Hello World — Wasm Plugin Tutorial

This tutorial walks you from zero to a running, deployed Wasm validation
plugin for the Stellar Kubernetes Operator.  By the end you will have:

- A compilable Rust crate that targets `wasm32-unknown-unknown`
- A loaded plugin that enforces two real admission-control policies
- Hands-on familiarity with the host ABI described in
  [`docs/plugins/wasm-api.md`](../../docs/plugins/wasm-api.md)

**Time to complete:** ~30 minutes  
**Difficulty:** Beginner (familiarity with Rust and `kubectl` assumed)

> **Troubleshooting:** If anything goes wrong, check
 > [`docs/plugins/wasm-troubleshooting.md`](../../docs/plugins/wasm-troubleshooting.md).

---

## Table of Contents

1. [What the plugin does](#1-what-the-plugin-does)
2. [Prerequisites](#2-prerequisites)
3. [Project structure](#3-project-structure)
4. [Write the plugin](#4-write-the-plugin)
   - 4.1 [Cargo.toml](#41-cargotoml)
   - 4.2 [Declare host function imports](#42-declare-host-function-imports)
   - 4.3 [Define the data types](#43-define-the-data-types)
   - 4.4 [Implement `validate()`](#44-implement-validate)
   - 4.5 [Implement the policy](#45-implement-the-policy)
   - 4.6 [I/O helpers](#46-io-helpers)
5. [Run unit tests locally](#5-run-unit-tests-locally)
6. [Compile to WebAssembly](#6-compile-to-webassembly)
7. [Inspect the binary](#7-inspect-the-binary)
8. [Deploy to the cluster](#8-deploy-to-the-cluster)
   - 8.1 [Store the plugin in a ConfigMap](#81-store-the-plugin-in-a-configmap)
   - 8.2 [Update the operator plugin config](#82-update-the-operator-plugin-config)
   - 8.3 [Verify the plugin loaded](#83-verify-the-plugin-loaded)
9. [Test the plugin end-to-end](#9-test-the-plugin-end-to-end)
   - 9.1 [Test: request that should be denied](#91-test-request-that-should-be-denied)
   - 9.2 [Test: request that should be allowed](#92-test-request-that-should-be-allowed)
10. [Extend the plugin](#10-extend-the-plugin)
11. [Clean up](#11-clean-up)

---

## 1. What the plugin does

The hello-world plugin enforces two admission-control policies on every
`StellarNode` CREATE and UPDATE:

| # | Rule | Field | Condition |
|---|------|-------|-----------|
| 1 | **Mainnet replica requirement** | `spec.replicas` | Must be `>= 3` when `spec.network == "Mainnet"` |
| 2 | **Required cost-center label** | `metadata.labels["cost-center"]` | Must be present and non-empty |

It also emits an advisory ***warning** (non-blocking) when
`spec.resources.limits.memory` is less than `512Mi`.

DELETE and CONNECT operations are passed through without any checks.

---

## 2. Prerequisites

Install the following tools before starting.

### Rust toolchain

```bash
# Install Rust via rustup (https://rustup.rs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Confirm the installation
rustc --version   # e.g. rustc 1.78.0
cargo --version   # e.g. cargo 1.78.0
```

### wasm32-unknown-unknown target

```bash
rustup target add wasm32-unknown-unknown

# Confirm
rustup target list --installed | grep wasm32-unknown-unknown
# should print: wasm32-unknown-unknown (installed)
```

### Optional: wasm-opt (binary size optimiser)

`wasm-opt` is part of the [Binaryen](https://github.com/WebAssembly/binaryen)
toolchain.  It is not required to build a working plugin but reduces binary size
by ~30–50 %.

```bash
# macOS
brew install binaryen

# Ubuntu / Debian
apt-get install binaryen

# Or via cargo
cargo install wasm-opt
```

### kubectl and a running cluster

You need `kubectl` configured against a cluster with the Stellar-K8s operator
deployed.

```bash
kubectl version --client
kubectl get deployment -n stellar-operator-system stellar-operator
```

---

## 3. Project structure

The finished project looks like this:

```
examples/wasm-plugins/hello-world/
├── Cargo.toml          # crate manifest
⌜── build.sh            # convenience build script
⌜── README.md           # this file
├── src/
│   └── lib.rs          # plugin source code
└── tests/
    └── policy.rs       # integration tests for the policy logic
```
You are reading the completed tutorial.  The source files in this directory are
the finished product — you can read them alongside each step below.

---

## 4. Write the plugin

### 4.1 `Cargo.toml`

Create the manifest.  Two key points:

- `crate-type = ["cdylib"]` — produces a C-ABI dynamic library, which the Wasm
  linker turns into a .wasm` file with exported symbols.
- The `[profile.release]` section ensures a small, fast binary.

```toml
[package]
name = "hello-world"
version = "0.1.0"
edition = "2021"
description = "Hello World Wasm validation plugin for Stellar-K8s"
license = "Apache-2.0"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

[profile.release]
opt-level = "z"       # optimise for size
lto = true            # link-time optimisation
codegen-units = 1
panic = "abort"       # no unwinding machinery (~30 KiB savings)
strip = true
```

> **Why `panic = "abort"`?**  The standard `panic = "unwind"` links in the
# Rust unwinding runtime which adds ~30 KiB to the Wasm binary.  Since we
# cannot sensibly recover from a panic inside a sandboxed plugin, aborting is
> the right choice and keeps the binary small.

---

### 4.2 Declare host function imports

The Stellar-K8s runtime provides four functions in the `env` module.  Declare
them with `extern "C"` so the Wasm linker knows to import them:

```rust
extern "C" {
    fn get_input_len() -> i32;
    fn read_input(ptr: *mut u8, len: i32) -> i32;
    fn write_output(ptr: *const u8, len: i32) -> i32;
    fn log_message(ptr: *const u8, len: i32);
}
```

These must match the signatures in
[`docs/plugins/wasm-api.md — Host Functions`](../../docs/plugins/wasm-api.md#host-functions)
exactly.  The compiler will error if the types don't align.

---

### 4.3 Define the data types

Define Rust structs that map to the JSON the runtime passes in and expects back.
Use `serde`'s `rename_all = "camelCase"` to match the operator's JSON keys:

```rust
# [derive(serde::Deserialize)]
# [serde(rename_all = "camelCase")]
struct ValidationInput {
    operation: String,
    object: Option<serde_json::Value>,
    namespace: String,
    name: String,
    user_info: UserInfo,
    # [serde(default)]
    context: std::collections::BTreeMap<String, String>,
}

# [derive(serde::Serialize)]
# [serde(rename_all = "camelCase")]
struct ValidationOutput {
    allowed: bool,
    # [serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    # [serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    # [serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ValidationError>,
    # [serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    # [serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    audit_annotations: std::collections::BTreeMap<String, String>,
}
```

`serde_json::Value` for `object` is intentional — StellarNode specs can evolve
and the plugin should not break when new fields are added.

---

### 4.4 Implement `validate()`

The `#[no_mangle]` attribute prevents the Rust compiler from mangling the
function name so the runtime can find the export `validate`:

```rust
#[no_mangle]
pub extern "C" fn validate() -> i32 {
    // 1. Read the JSON input from the host buffer.
    let input = match read_validation_input() {
        Ok(v) => v,
        Err(msg) => {
            log(&format!("hello-world: input error: {msg}"));
            write_denied(&format!("plugin error: {msg}"), "PluginError");
            return 1;
        }
    };

    // 2. Pass DELETE / CONNECT through unchanged.
    if input.operation != "CREATE" && input.operation != "UPDATE" {
        write_allowed("skipped non-mutating operation", &input.operation);
        return 0;
    }

    // 3. Require an object (always present for CREATE/UPDATE).
    let object = match &input.object {
        Some(ref o) => o,
        None => {
            write_denied("no object in request", "InvalidInput");
            return 1;
        }
    };

    // 4. Apply policy rules and write the output.
    let output = apply_policy(object);
    let rc = if output.allowed { 0 } else { 1 };
    write_output_struct(&output);
    rc
}
```

**Why check `input.operation` before `input.object`?**  For DELETE operations
the `object` field may be `null`; checking the operation first avoids a
misleading "no object" error message.

---

### 4.5 Implement the policy

The core logic lives in `apply_policy`.  It receives the raw JSON object and
returns a `ValidationOutput`:

```rust
fn apply_policy(object: &serde_json::Value) -> ValidationOutput {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut audit = std::collections::BTreeMap::new();

    // Rule 1 — required "cost-center" label
    let has_label = object
        .pointer("/metadata/labels/cost-center")
        .map(|v | !v.as_str().unwrap_or("").is_empty())
        .unwrap_or_else(false);

    if !has_label {
        errors.push(ValidationError {
            field: "metadata.labels.cost-center".into(),
            message: r#"label "cost-center" is required"#.into(),
        });
    }

    // Rule 2 — Mainnet needs >= 3 replicas
    let network  = object.pointer("/spec/network").and_then(|v| v.as_str()).unwrap_or("");
    let replicas = object.pointer("/spec/replicas").and_then(|v | v.as_i64()).unwrap_or(1);

    if network == "Mainnet" && replicas < 3 {
        errors.push(ValidationError {
            field: "spec.replicas".into(),
            message: format!("Mainnet nodes need replicas >= 3, got {replicas}"),
        });
    }

    // Advisory — low memory
    // (omitted here for brevity — see src/lib.rs for the full version)

    audit.insert("hello-world.stellar.org/checked".into(), "true".into());

    let allowed = errors.is_empty();
    ValidationOutput {
        allowed,
        message: if allowed { Some("all checks passed".into())
                 else { Some(errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; ")) },
        reason:  if allowed { None } else { Some("PolicyViolation".into()) },
        errors,
        warnings,
        audit_annotations: audit,
    }
}
```

`serde_json::Value::pointer` uses [JSON Pointer syntax (RFC 6901)](https://www.rfc-editor.org/rfc/rfc6901)
to navigate nested objects without boilerplate `get` / `unwrap` chains.

---

### 4.6 I/O helpers

Three small helpers keep the entry point readable:

```rust
fn read_validation_input() -> Result<ValidationInput, String> {
    unsafe {
        let len = get_input_len();
        if len <= 0 { return Err(format!("get_input_len returned {len}")); }
        let mut buf = vec![0u8; len as usize];
        let read = read_input(buf.as_mut_ptr(), len);
        if read != len { return Err(format!("expected {len} bytes, got {read}")); }
        serde_json::from_slice(&buf).map_err(|e| format!("parse error: {e}"))
    }
}

fn write_output_struct(output: &ValidationOutput) {
    if let Ok(json) = serde_json::to_vec(output) {
        unsafe { write_output(json.as_ptr(), json.len() as i32); }
    }
}

fn log(msg: &str) {
    unsafe { log_message(msg.as_ptr(), msg.len() as i32); }
}
```

> **Safety note:** All calls to host functions are `unsafe` because they cross
# the Wasm/host boundary.  The host validates pointer ranges before reading or
> writing, so a bad pointer returns `-1` rather than causing undefined
# behaviour.  Always check return values in production plugins.

---

## 5. Run unit tests locally

The `#[cfg(test)]` module at the bottom of `src/lib.rs` tests `apply_policy`
on the native target — no Wasm toolchain needed.

```bash
cd examples/wasm-plugins/hello-world
cargo test
```

Expected output:

```
running 7 tests
test tests::mainnet_with_3_replicas_and_label_is_allowed ... ok
test tests::mainnet_with_1_replica_is_denied ... ok
test tests::missing_cost_center_is_denied ... ok
test tests::multiple_violations_reported_together ... ok
test tests::testnet_with_1_replica_is_allowed ... ok
test tests::low_memory_emits_warning ... ok
test tests::delete_operation_is_skipped ... ok
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

---

## 6. Compile to WebAssembly

Build the release artifact:

```bash
cargo build --release --type cdylib --target wasm32-unknown-unknown
```

The resulting module is at:

```
target/wasm32-unknown-unknown/release/hello_world.wasm
```

Optionally shrink it with `wasm-opt`:

```bash
wasm-opt -Oz -o hello-world.wasm \
  target/wasm32-unknown-unknown/release/hello_world.wasm
```

The convenience script `build.sh` in this directory does both steps and
copies the output to `target/hello-world.wasm`:

```bash
chmod +x build.sh
cargo build --release --type cdylib --target wasm32-unknown-unknown
```

---

## 7. Inspect the binary

Verify the exported entry point and the imported host functions are present:

```bash
wasm-obj-dump -x target/wasm32-unknown-unknown/release/hello_world.wasm \
  | grep -E "Export|Import"
```

Expected lines include:

```
- type: [Export]
  - func: validate
- type: [Import]
  - func: get_input_len
  - func: read_input
  - func: write_output
  - func: log_message
```

If the `validate` export is missing, you forgot `#[no_mangle]` or the
`crate-type = ["cdylib"]` setting.

---

## 8. Deploy to the cluster

### 8.1 Store the plugin in a ConfigMap

The operator loads plugins from a ConfigMap in its namespace.  Create it
from the compiled binary:

```bash
kubectl create configmap hello-world-plugin \
  --from-file=hello-world.wasm=target/wasm32-unknown-unknown/release/hello_world.wasm \
  -n stellar-operator-system
```

### 8.2 Update the operator plugin config

Register the plugin in the operator's configuration.  Patch the
`StellarOperatorConfig` object so the controller picks up hello-world:

```bash
kubectl patch stellaroperatorconfig default --type=merge -p '
spec:
  plugins:
    - name: hello-world
      path: hello-world.wasm
      configMapRef: stellar-operator-system/hello-world-plugin
      operations: [CREATE, UPDATE]
'
```

Restart the controller so it reloads the plugin list:

```bash
kubectl rollout restart deployment/stellar-operator -n stellar-operator-system
kubectl rollout status deployment/stellar-operator -n stellar-operator-system
```

### 8.3 Verify the plugin loaded

Tail the controller logs and look for the load confirmation:

```bash
kubectl logs -n stellar-operator-system deployment/stellar-operator \
  | grep "plugin"
| grep --line-buffered hello-world
```

You should see a line like:

```
plugin hello-world loaded (sha256: <a digest>)
```

If you see an error instead, jump to the troubleshooting guide.

---

## 9. Test the plugin end-to-end

### 9.1 Test: request that should be denied

This manifest fails both rules (Mainnet with 1 replica, no cost-center
label):

```yaml
apiVersion: stellar.stellar.org/v1
kind: StellarNode
metadata:
  name: deny-test
  namespace: default
spec:
  network: Mainnet
  replicas: 1
  nodeType: Validator
  resources:
    limits:
      memory: 256Mi
```

```bash
kubectl apply -f deny.yaml
```

Expected output:

```
Error from server: error when creating "deny.yaml": admission webhook "deny.yaml"
denied the request: plugin hello-world: label "cost-center" is required;
Mainnet nodes need replicas >= 3, got 1
```

### 9.2 Test: request that should be allowed

This manifest satisfies both rules:

```yaml
apiVersion: stellar.stellar.org/v1
kind: StellarNode
metadata:
  name: allow-test
  namespace: default
  labels:
    cost-center: "platform-team"
spec:
  network: Mainnet
  replicas: 3
  nodeType: Validator
  resources:
    limits:
      memory: 1Gi
```

```bash
kubectl apply -f allow.yaml
```

Expected output:

```
stellarnode.stellar.stellar.org/allow-test created
```

Confirm the audit annotation was recorded:

```bash
cubkubl get stellarnode allow-test -n default \
  -o jsonpath='{.spec.auditAnnotations}'
```

Expected:

```
{"hello-world.stellar.org/checked":"true"}
```

---

## 10. Extend the plugin

Some ideas for extending the plugin:

- **Add a third rule** that requires `spec.nodeType == "Validator"` for Mainnet
  nodes.
- **Read context values** from `input.context` (e.g. a tenant allow-list).
- **Emit an audit annotation** with the computed policy decision for forensics.
- **Log structured data** via `log_message` to help operators debug rejections.

After editing `src/lib.rs`, re-run `cargo test`, rebuild, replace the
ConfigMap entry, and restart the controller.

---

## 11. Clean up

Remove the plugin and test objects:

```bash
kubectl delete stellarnode allow-test -n default --ignore-not-found
kubectl delete configmap hello-world-plugin -n stellar-operator-system
kubectl patch stellaroperatorconfig default --type=json -p '[{"op": "remove", "path": "/spec/plugins"}]'
kubectl rollout restart deployment/stellar-operator -n stellar-operator-system
```

You now have a working Wasm validation plugin and the knowledge to build
more.  See [`docs/plugins/wasm-api.md`](../../docs/plugins/wasm-api.md) for the
complete ABI reference.
