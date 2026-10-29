# Stellar-K8s

Kubernetes operator and tooling for running Stellar Core and Soroban RPC nodes.

## WebAssembly Fail-Open Caching Layer for Soroban RPC State Reads

High-frequency state reads from Soroban RPC nodes place significant load on the
underlying Stellar Core instances. Stellar-K8s ships a WebAssembly plugin that
intercepts read-only Soroban RPC state reads and serves them from a bounded,
in-sandbox LRU cache, reducing latency and RPC load for dApp builders.

### How it works

* The plugin is loaded through the existing **Custom Validation Plugin** system
  and intercepts state read requests before they reach the Soroban RPC.
* An **LRU cache** is maintained strictly inside the Wasm sandbox. Entries are
  evicted by least-recently-used order once the configured size bound is hit.
* The layer is **fail-open**: if the cache fails, panics, or hits its memory
  limit, the request is transparently bypassed and routed to the underlying RPC
  node. No requests are dropped and there is no user-facing disruption.
* Memory allocation is strictly bounded so the sandbox cannot be crashed by
  out-of-memory conditions, and the compiled Wasm binary stays under 2MB.

### Configuration

The cache respects TTL and size settings supplied via a standard Kubernetes
ConfigMap. Cluster operators can tune the TTL to control how long cached state
reads remain valid before being re-fetched from the RPC node.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: soroban-rpc-cache-config
  namespace: stellar
data:
  # Time-To-Live for cached state reads, in seconds.
  cache_ttl_seconds: "30"
  # Maximum number of entries retained in the LRU cache.
  cache_max_entries: "10000"
  # Upper bound on sandbox memory for the cache, in bytes.
  cache_max_memory_bytes: "16777216"
```

Reference this ConfigMap from the Soroban RPC node configuration to enable the
caching layer. Lower TTL values favor freshness; higher values reduce RPC load
at the cost of serving slightly staler state reads.

### Validation

Unit tests cover the LRU caching logic, and an integration test simulates
10k read requests/second to demonstrate reduced RPC load and correct fail-open
fallback behavior when the cache is forced to fail.
