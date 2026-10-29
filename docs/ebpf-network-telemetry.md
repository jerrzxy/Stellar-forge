# eBPF network telemetry

The operator telemetry crate (`stellar-telemetry`) can load an Aya-rs probe against
the host networking stack to observe ingress/egress flows to Stellar Core pods
without a sidecar proxy.

## Privileges

Kernel load requires Linux 5.8+ with BTF and these capabilities on the collector
container (host network namespace):

| Capability | Why |
|---|---|
| `CAP_BPF` | `bpf()` syscall (Linux 5.8+) |
| `CAP_PERFMON` | perf event / kprobe attach |
| `CAP_NET_ADMIN` | sockops / cgroup net programs |

Example (opt-in; default operator Deployment stays unprivileged):

```yaml
securityContext:
  capabilities:
    add: ["BPF", "PERFMON", "NET_ADMIN"]
    drop: ["ALL"]
  privileged: false
```

Set `STELLAR_SIMULATE_EBPF=1` only in tests. Production should not run privileged
unless eBPF telemetry is explicitly enabled.

## Fallback

If `bpf()` is denied (seccomp, AppArmor, GKE COS without extra privileges, or
unsupported kernel), `EbpfCollector::try_load()` returns `CollectorMode::Fallback`
and continues exporting standard Prometheus metrics. The process does not crash.

## Metrics

- `stellar_ebpf_packet_drops_total`
- `stellar_ebpf_connection_resets_total`
- `stellar_ebpf_latency_us_avg`
- `stellar_ebpf_pod_mapped_total`
- `stellar_ebpf_fallback_active`
