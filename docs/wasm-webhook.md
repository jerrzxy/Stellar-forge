# WebAssembly Validation Policies

The Stellar Kubernetes Operator supports custom validation policies written in WebAssembly (Wasm). This allows you to enforce organization-specific requirements and policies without modifying the operator code.

## Overview

The Wasm webhook system provides:

- **Custom Validation Logic**: Write policies in any language that compiles to Wasm (Rust, Go, C++, AssemblyScript, etc.)
- **Sandboxed Execution**: Plugins run in a secure, isolated environment with resource limits
- **Dynamic Loading**: Load and unload plugins at runtime without restarting the operator
- **ConfigMap Integration**: Store plugins in Kubernetes ConfigMaps for easy management
- **Fail-Open Support**: Configure plugins to allow requests if they fail
- **Audit Logging**: Plugins can add annotations to the Kubernetes audit log

## Architecture

```
┌─────────────────┐
│  Kubernetes API │
└────────┬────────┘
         │
         ▼
┌─────────────────────────┐
│  Admission Webhook      │
│  (Validating/Mutating)  │
└────────┬────────────────┘
         │
         ▼
┌─────────────────────────┐
│  Wasm Runtime           │
│  (Wasmtime)             │
├─────────────────────────┤
│  Plugin 1 (Wasm)        │
│  Plugin 2 (Wasm)        │
│  Plugin 3 (Wasm)        │
└─────────────────────────┘
```

## Quick Start

### 1. Build a Plugin

See the [example plugin](../examples/plugins/image-registry-validator/) for a complete example.

```rust
#[no_mangle]
pub extern "C" fn validate() -> i32 {
    let input = read_validation_input()?;
    let output = validate_stellar_node(&input);
    write_validation_output(&output);
    if output.allowed { 0 } else { 1 }
}
```

Build it:

```bash
cd examples/plugins/image-registry-validator
cargo build --target wasm32-unknown-unknown --release
```

### 2. Deploy the Plugin

#### Option A: ConfigMap

```bash
kubectl create configmap my-validator \
    --from-file=plugin.wasm=target/wasm32-unknown-unknown/release/my_validator.wasm \
    -n stellar-operator-system
```

Then configure the operator to load it (see Configuration section).

#### Option B: Direct API

```bash
WASM_BASE64=$(base64 < my_validator.wasm)

curl -X POST http://webhook-service:8443/plugins \
    -H "Content-Type: application/json" \
    -d '{
        "metadata": {
            "name": "my-validator",
            "version": "1.0.0",
            "description": "My custom validator"
        },
        "wasm_binary": "'$WASM_BASE64'",
        "operations": ["CREATE", "UPDATE"],
        "enabled": true
    }'
```

### 3. Test the Plugin

```bash
# List loaded plugins
curl http://webhook-service:8443/plugins

# Create a StellarNode (will be validated by your plugin)
kubectl apply -f my-stellarnode.yaml
```

## Plugin Interface

### Input Structure

Plugins receive a JSON object with this structure:

```json
{
  "operation": "CREATE",
  "object": {
    "apiVersion": "stellar.org/v1alpha1",
    "kind": "StellarNode",
    "metadata": { "name": "my-node" },
    "spec": { /* StellarNode spec */ }
  },
  "oldObject": null,
  "namespace": "default",
  "name": "my-node",
  "userInfo": {
    "username": "admin",
    "uid": "...",
    "groups": ["system:masters"],
    "extra": {}
  },
  "context": {}
}
```

### Output Structure

Plugins must return a JSON object:

```json
{
  "allowed": true,
  "message": "Validation passed",
  "reason": null,
  "errors": [],
  "warnings": ["Consider increasing memory limit"],
  "auditAnnotations": {
    "my-plugin/checked": "true"
  }
}
```

### Host Functions

The runtime provides these functions for plugin I/O:

```rust
extern "C" {
    // Get the length of the input data
    fn get_input_len() -> i32;

    // Read input data into Wasm memory
    fn read_input(ptr: *mut u8, len: i32) -> i32;

    // Write output data from Wasm memory
    fn write_output(ptr: *const u8, len: i32) -> i32;

    // Log a debug message
    fn log_message(ptr: *const u8, len: i32);
}
```

## Configuration

### Operator Configuration

Configure the webhook in the operator deployment:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: stellar-operator
spec:
  template:
    spec:
      containers:
      - name: operator
        args:
        - webhook
        - --webhook-port=8443
        - --webhook-cert=/certs/tls.crt
        - --webhook-key=/certs/tls.key
        - --plugin-config=/config/plugins.yaml
```

### Plugin Configuration File

Create a `plugins.yaml` file:

```yaml
plugins:
  - metadata:
      name: image-registry-validator
      version: "1.0.0"
      description: "Validates image registries"
      limits:
        timeoutMs: 1000
        maxMemoryBytes: 16777216  # 16MB
        maxFuel: 1000000
    configMapRef:
      name: image-registry-validator
      key: plugin.wasm
      namespace: stellar-operator-system
    operations:
      - CREATE
      - UPDATE
    enabled: true
    failOpen: false

  - metadata:
      name: resource-limits-validator
      version: "1.0.0"
    secretRef:
      name: resource-limits-validator
      key: plugin.wasm
    operations:
      - CREATE
      - UPDATE
    enabled: true
    failOpen: true  # Allow if plugin fails
```

### Kubernetes Resources

#### ValidatingWebhookConfiguration

```yaml
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: stellar-node-validator
webhooks:
  - name: validate.stellarnode.stellar.org
    clientConfig:
      service:
        name: stellar-operator-webhook
        namespace: stellar-operator-system
        path: /validate
      caBundle: <base64-encoded-ca-cert>
    rules:
      - operations: ["CREATE", "UPDATE"]
        apiGroups: ["stellar.org"]
        apiVersions: ["v1alpha1"]
        resources: ["stellarnodes"]
    admissionReviewVersions: ["v1"]
    sideEffects: None
    timeoutSeconds: 10
```

## Security

### Sandboxing

Plugins run in a secure sandbox with:

- **No filesystem access**: Plugins cannot read or write files
- **No network access**: Plugins cannot make network requests
- **No system calls**: Only approved host functions are available
- **Memory limits**: Configurable maximum memory usage
- **CPU limits**: Fuel metering prevents infinite loops
- **Timeout**: Execution time limits prevent hanging

### Resource Limits

Configure limits per plugin:

```yaml
limits:
  timeoutMs: 1000          # Maximum execution time
  maxMemoryBytes: 16777216 # Maximum memory (16MB)
  maxFuel: 1000000         # Maximum instructions
```

### Integrity Verification

Verify plugin integrity with SHA256 hashes:

```yaml
metadata:
  name: my-plugin
  version: "1.0.0"
  sha256: "abc123..."  # SHA256 hash of the Wasm binary
```

The runtime will verify the hash before loading the plugin.

## Use Cases

### 1. Image Registry Enforcement

Ensure all images come from approved registries:

```rust
const APPROVED_REGISTRIES: &[&str] = &[
    "docker.io/stellar/",
    "ghcr.io/myorg/",
];

fn validate(input: &ValidationInput) -> ValidationOutput {
    let version = input.object.spec.version;
    if !APPROVED_REGISTRIES.iter().any(|r| version.starts_with(r)) {
        return ValidationOutput::denied("Unapproved registry");
    }
    ValidationOutput::allowed()
}
```

### 2. Resource Limit Enforcement

Enforce minimum/maximum resource limits:

```rust
fn validate(input: &ValidationInput) -> ValidationOutput {
    let memory = input.object.spec.resources.limits.memory;
    let memory_bytes = parse_memory(memory);

    if memory_bytes < 512 * 1024 * 1024 {
        return ValidationOutput::denied("Memory must be at least 512Mi");
    }

    ValidationOutput::allowed()
}
```

### 3. Network Policy Enforcement

Ensure nodes on mainnet have specific configurations:

```rust
fn validate(input: &ValidationInput) -> ValidationOutput {
    if input.object.spec.network == "Mainnet" {
        if input.object.spec.replicas < 3 {
            return ValidationOutput::denied(
                "Mainnet nodes must have at least 3 replicas"
            );
        }
    }
    ValidationOutput::allowed()
}
```

### 4. Compliance Checks

Enforce organizational compliance requirements:

```rust
fn validate(input: &ValidationInput) -> ValidationOutput {
    let mut errors = Vec::new();

    // Check labels
    if !input.object.metadata.labels.contains_key("cost-center") {
        errors.push(ValidationError::new(
            "metadata.labels.cost-center",
            "Cost center label is required"
        ));
    }

    // Check annotations
    if !input.object.metadata.annotations.contains_key("owner") {
        errors.push(ValidationError::new(
            "metadata.annotations.owner",
            "Owner annotation is required"
        ));
    }

    if errors.is_empty() {
        ValidationOutput::allowed()
    } else {
        ValidationOutput::denied_with_errors(errors)
    }
}
```

## Development

### Prerequisites

- Rust toolchain
- `wasm32-unknown-unknown` target
- `wasm-opt` (optional, for optimization)

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-opt
```

### Project Structure

```
my-validator/
├── Cargo.toml
├── src/
│   └── lib.rs
├── build.sh
└── README.md
```

### Cargo.toml

```toml
[package]
name = "my-validator"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

### Testing

Test plugins locally before deploying:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approved_registry() {
        let input = ValidationInput {
            operation: "CREATE".to_string(),
            object: Some(json!({
                "spec": {
                    "version": "docker.io/stellar/stellar-core:v21.3.0"
                }
            })),
            // ... other fields
        };

        let output = validate_stellar_node(&input);
        assert!(output.allowed);
    }

    #[test]
    fn test_unapproved_registry() {
        let input = ValidationInput {
            operation: "CREATE".to_string(),
            object: Some(json!({
                "spec": {
                    "version": "quay.io/myorg/stellar-core:v21.3.0"
                }
            })),
            // ... other fields
        };

        let output = validate_stellar_node(&input);
        assert!(!output.allowed);
    }
}
```

## Troubleshooting

### Plugin Not Loading

Check the operator logs:

```bash
kubectl logs -n stellar-operator-system deployment/stellar-operator
```

Common issues:
- Invalid Wasm binary
- Missing required exports (`validate`, `memory`)
- SHA256 mismatch
- ConfigMap not found

### Plugin Execution Failures

Check for:
- Timeout (increase `timeoutMs`)
- Out of memory (increase `maxMemoryBytes`)
- Out of fuel (increase `maxFuel`)
- Invalid JSON output

### Debugging

Enable debug logging in plugins:

```rust
log(&format!("Checking version: {}", version));
```

View logs:

```bash
kubectl logs -n stellar-operator-system deployment/stellar-operator | grep wasm_plugin
```

## Performance

### Benchmarks

Typical plugin performance:

- **Load time**: <100ms (one-time, cached)
- **Execution time**: <5ms per validation
- **Memory usage**: <1MB per plugin
- **Binary size**: 20-100KB (optimized)

### Optimization Tips

1. **Use `wasm-opt`**: Reduces binary size by 50-70%
2. **Minimize dependencies**: Each dependency adds to binary size
3. **Avoid allocations**: Reuse buffers where possible
4. **Profile with fuel**: Monitor `fuel_consumed` in results
5. **Cache compiled modules**: The runtime caches compiled plugins

## Best Practices

1. **Keep plugins focused**: One policy per plugin
2. **Fail gracefully**: Always return valid JSON
3. **Use fail-open for non-critical checks**: Prevent outages
4. **Add audit annotations**: Track what was validated
5. **Version your plugins**: Use semantic versioning
6. **Test thoroughly**: Write unit tests for all cases
7. **Monitor performance**: Track execution time and fuel
8. **Document policies**: Explain what each plugin validates

## API Reference

### REST API

#### List Plugins

```
GET /plugins
```

Response:

```json
{
  "plugins": [
    {
      "name": "image-registry-validator",
      "version": "1.0.0",
      "description": "Validates image registries",
      "operations": ["CREATE", "UPDATE"],
      "enabled": true
    }
  ]
}
```

#### Load Plugin

```
POST /plugins
Content-Type: application/json

{
  "metadata": {
    "name": "my-plugin",
    "version": "1.0.0"
  },
  "wasm_binary": "<base64-encoded-wasm>",
  "operations": ["CREATE", "UPDATE"],
  "enabled": true
}
```

#### Remove Plugin

```
DELETE /plugins/:name
```

## Examples

See the [examples/plugins](../examples/plugins/) directory for complete examples:

- [image-registry-validator](../examples/plugins/image-registry-validator/) - Validates image registries
- [example-validator](../examples/plugins/example-validator/) - Basic validation template

## References

- [WebAssembly](https://webassembly.org/)
- [Wasmtime](https://wasmtime.dev/)
- [Kubernetes Admission Webhooks](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/)
- [Rust Wasm Book](https://rustwasm.github.io/docs/book/)

---

## Payload contract tests

Malformed and boundary AdmissionReview payloads are covered by hermetic HTTP
contract tests in `tests/webhook_payload_contract.rs` (issue #1152). These
exercise empty/non-JSON/truncated bodies, missing requests, oversized payloads,
and mutate/validate parity without a live Kubernetes API server.

---

*Last verified: 2026-07-29 (webhook payload contract tests #1152; TODO exemption hygiene).*


---

## Controller-Crate Implementation (Issue #303)

The admission webhook engine is implemented in the `controller` crate under
`controller/src/webhook/`.  This section documents the internal design for
operators and contributors.

### Modules

| Module | Purpose |
|--------|---------|
| `controller::webhook::wasm_runner` | Sandboxed Wasmtime execution engine |
| `controller::webhook::admission`   | AdmissionReview HTTP server (validate + mutate) |
| `controller::tls`                  | Self-signed cert generation for dev/CI |

### Security Constraints

Every plugin execution is bounded by three independent limits:

| Constraint | Default | Purpose |
|------------|---------|---------|
| Fuel | 5 000 000 instructions | Prevents CPU-exhausting loops |
| Memory | 4 MiB | Bounds guest heap growth |
| Wall-clock | 9 seconds | Guarantees response before K8s 10s timeout |

### Fail-Open vs Fail-Closed

Each plugin carries an independent `fail_open` flag:

```bash
# Register a fail-closed policy (deny on plugin error):
curl -sk -X POST https://webhook:8443/plugins \
  -H 'Content-Type: application/json' \
  -d '{"name":"registry-policy","wasm_b64":"<BASE64>","fail_open":false}'

# Register a fail-open policy (allow on plugin error):
curl -sk -X POST https://webhook:8443/plugins \
  -H 'Content-Type: application/json' \
  -d '{"name":"audit-logger","wasm_b64":"<BASE64>","fail_open":true}'
```

### Environment-variable plugin loading

Plugins can be pre-loaded at operator startup:

```bash
export WASM_POLICY_PLUGIN_PATH=/etc/plugins/registry_policy.wasm:/etc/plugins/quota_check.wasm
export WASM_POLICY_FAIL_OPEN=false
stellar-operator webhook --bind 0.0.0.0:8443 --cert-path /tls/tls.crt --key-path /tls/tls.key
```

### Example: Registry Policy Plugin

The `examples/plugins/registry-policy/` directory contains a Rust WASM plugin
that enforces approved Docker registry prefixes.  Build and deploy it:

```bash
# Build
cargo build --target wasm32-unknown-unknown --release -p registry-policy

# Deploy via kubectl
kubectl create configmap registry-policy \
  --from-file=plugin.wasm=target/wasm32-unknown-unknown/release/registry_policy.wasm \
  -n stellar-system

# Verify rejection of a non-compliant node
cat <<EOF | kubectl apply -f -
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: bad-validator
  namespace: stellar
spec:
  nodeType: Validator
  network: testnet
  version: "private.corp.internal/stellar/stellar-core:v21"
EOF
# Expected:
# Error from server: admission webhook denied the request:
#   image 'private.corp.internal/...' is not from an approved registry.
```

### Memory Safety Profile

The `WasmRunner` is designed with the following properties to prevent memory
leaks in the primary operator thread:

- **No shared mutable state** — each `run_plugin` call creates a fresh
  Wasmtime `Store` that is dropped at the end of the call.
- **Arc-wrapped engine** — the Cranelift JIT `Engine` is shared read-only
  across all calls; compiled modules (`CompiledPlugin`) hold an `Arc<Module>`
  and can be safely cloned.
- **Input/output buffers** — per-call `Arc<Mutex<Vec<u8>>>` buffers are
  created fresh each time and dropped when the host-ABI closures go out of
  scope.
- **ResourceLimiter** — the `StoreLimiter` refuses memory allocations beyond
  4 MiB, ensuring a misbehaving plugin cannot grow the host process address
  space unboundedly.
