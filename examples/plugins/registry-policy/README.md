# registry-policy — WASM Admission Plugin

Enforces that every `StellarNode` resource's `spec.version` field only references
images from approved Docker registries.

## Approved registries

| Prefix | Description |
|---|---|
| `docker.io/stellar/` | Official Docker Hub Stellar images |
| `ghcr.io/stellar/` | GitHub Container Registry |
| `gcr.io/stellar-project/` | Google Container Registry |
| `registry.k8s.io/stellar/` | Kubernetes registry mirror |

## Behaviour

| Scenario | Decision |
|---|---|
| `spec.version` has no `/` (plain tag) | ✅ Allow + warning |
| `spec.version` starts with an approved prefix | ✅ Allow |
| `spec.version` starts with an unapproved registry | ❌ Deny |
| `spec.version` is empty | ✅ Allow + warning |
| Operation is DELETE or CONNECT | ✅ Allow (skip) |

## Build

```bash
# Install the wasm32-unknown-unknown target
rustup target add wasm32-unknown-unknown

# Build the plugin
cargo build --target wasm32-unknown-unknown --release

# The .wasm binary is at:
# target/wasm32-unknown-unknown/release/registry_policy.wasm
```

## Deploy

### Via kubectl (ConfigMap)

```bash
kubectl create configmap registry-policy \
  --from-file=plugin.wasm=target/wasm32-unknown-unknown/release/registry_policy.wasm \
  -n stellar-system
```

### Via the webhook API

```bash
WASM_B64=$(base64 -w0 < target/wasm32-unknown-unknown/release/registry_policy.wasm)

curl -sk -X POST https://webhook:8443/plugins \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"registry-policy\",\"wasm_b64\":\"${WASM_B64}\",\"fail_open\":false}"
```

### Via environment variable (startup-time loading)

```bash
export WASM_POLICY_PLUGIN_PATH=/etc/plugins/registry_policy.wasm
export WASM_POLICY_FAIL_OPEN=false
stellar-operator webhook --bind 0.0.0.0:8443 --cert-path /tls/tls.crt --key-path /tls/tls.key
```

## Test

Apply a non-compliant manifest:

```yaml
# non-compliant-node.yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: bad-validator
  namespace: stellar
spec:
  nodeType: Validator
  network: testnet
  version: "private.registry.internal/stellar/stellar-core:v21"
```

```bash
kubectl apply -f non-compliant-node.yaml
# Expected output:
# Error from server: admission webhook "stellar-admission-webhook.stellar-system.svc" denied the request:
#   image 'private.registry.internal/stellar/stellar-core:v21' is not from an approved registry.
```

Apply a compliant manifest:

```yaml
# compliant-node.yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: good-validator
  namespace: stellar
spec:
  nodeType: Validator
  network: testnet
  version: "ghcr.io/stellar/stellar-core:v21.0.0"
```

```bash
kubectl apply -f compliant-node.yaml
# stellarnode.stellar.org/good-validator created
```
