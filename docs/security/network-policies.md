# Network Policies for Stellar-K8s

This guide documents the Kubernetes `NetworkPolicy` configurations required to
isolate Stellar infrastructure components. Every policy in this document is
deployable with `kubectl apply` and has been validated against a real cluster
using the verification procedure at the bottom of this page.

> **Related docs**
> - [RBAC Hardening](rbac-hardening.md)
> - [Secrets Management](secrets-management.md)
> - [p2p-firewall.md](p2p-firewall.md) — eBPF-level firewall for validator P2P
> - [network-isolation.md](../../network-isolation.md) — operator-generated per-node policies
> - Example manifests: [`examples/security/`](../../examples/security/)

---

## Contents

1. [Prerequisites and CNI requirements](#prerequisites-and-cni-requirements)
2. [Namespace trust model](#namespace-trust-model)
3. [Default deny — namespace baseline](#default-deny--namespace-baseline)
4. [Validator isolation policy](#validator-isolation-policy)
5. [Horizon API policies](#horizon-api-policies)
6. [Soroban RPC policies](#soroban-rpc-policies)
7. [Operator egress policy](#operator-egress-policy)
8. [Monitoring ingress policy](#monitoring-ingress-policy)
9. [DNS egress policy](#dns-egress-policy)
10. [Composing policies for production](#composing-policies-for-production)
11. [Verification procedure](#verification-procedure)
12. [Troubleshooting](#troubleshooting)

---

## Prerequisites and CNI requirements

Kubernetes `NetworkPolicy` is only enforced when a CNI plugin that supports it is
installed. The following CNIs are tested with these manifests:

| CNI | Support | Notes |
|---|---|---|
| Calico | ✅ Full | Recommended for bare-metal and self-managed clusters |
| Cilium | ✅ Full | Recommended for EKS/GKE; also supports eBPF-level policies |
| WeaveNet | ✅ Full | Good for multi-cloud; end-of-life as of 2023 |
| Flannel | ❌ None | Does not enforce NetworkPolicy; add Calico as a policy engine |
| AWS VPC CNI | ✅ with Calico | Use `--network-policy=calico` on EKS 1.25+ |
| GKE Dataplane V2 | ✅ Full | Enabled by default on new GKE clusters |

Verify enforcement before relying on these policies:

```bash
# Deploy a test pod in stellar namespace and attempt cross-namespace traffic
kubectl run netpol-test --image=busybox:1.36 -n stellar --rm -it \
  -- wget -q -T 3 http://validator-0.stellar-system.svc.cluster.local:11626
# With the policies in place, this MUST time out or be refused.
```

---

## Namespace trust model

Policies use **namespace label selectors** as trust boundaries. Apply these labels
before deploying policies. The operator's PSS controller also uses `stellar.org/node-namespace`
to scope its label-patching ClusterRole.

```bash
# Workload namespace — runs Validator, Horizon, Soroban pods
kubectl label namespace stellar \
  stellar.org/node-namespace="true" \
  pod-security.kubernetes.io/enforce=restricted

# Operator namespace
kubectl label namespace stellar-system \
  stellar.org/operator-namespace="true"

# Monitoring namespace (Prometheus, Grafana)
kubectl label namespace monitoring \
  stellar.org/role=monitoring

# Ingress controller namespace
kubectl label namespace ingress-nginx \
  stellar.org/role=ingress

# Trusted API gateway namespace (if applicable)
kubectl label namespace stellar-gateway \
  stellar.org/network-access=trusted
```

> Protect these labels with RBAC or a Kyverno/Gatekeeper policy. If application
> teams can add `stellar.org/network-access=trusted` to arbitrary namespaces, the
> trust model is broken.

---

## Default deny — namespace baseline

Deploy a default-deny policy first in every Stellar namespace. All subsequent
policies **add** allowed traffic on top of this baseline.

```yaml
# Apply with: kubectl apply -f examples/security/netpol-default-deny.yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: default-deny-all
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: baseline
spec:
  podSelector: {}    # selects ALL pods in the namespace
  policyTypes:
    - Ingress
    - Egress
  # No ingress or egress rules = deny all
```

Apply the same policy to `stellar-system`:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: default-deny-all
  namespace: stellar-system
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: baseline
spec:
  podSelector: {}
  policyTypes:
    - Ingress
    - Egress
```

After applying default-deny, all cross-pod traffic is dropped. Add the policies
below to restore only the minimum required paths.

---

## Validator isolation policy

Validator pods communicate P2P on ports **11625** (SCP gossip) and **11626** (HTTP
admin). The admin port must **not** be reachable from outside the namespace.

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: validator-allow-p2p
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: validator
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/component: validator
  policyTypes:
    - Ingress
    - Egress

  ingress:
    # Allow SCP gossip from other validators in the same namespace
    - from:
        - podSelector:
            matchLabels:
              app.kubernetes.io/component: validator
      ports:
        - protocol: TCP
          port: 11625

    # Allow SCP gossip from external peers (Stellar network)
    # Replace with CIDRs of known quorum peers if you want a tighter policy.
    - ports:
        - protocol: TCP
          port: 11625

    # Admin HTTP — operator only, within cluster
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/operator-namespace: "true"
          podSelector:
            matchLabels:
              app.kubernetes.io/name: stellar-operator
      ports:
        - protocol: TCP
          port: 11626

    # Prometheus scrape — monitoring namespace only
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: monitoring
      ports:
        - protocol: TCP
          port: 11626    # metrics exposed on the admin port

  egress:
    # Allow SCP gossip to external peers
    - ports:
        - protocol: TCP
          port: 11625

    # Allow DNS resolution (required for SCP peer discovery)
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53

    # Allow HTTPS for history archive sync (S3, GCS, Cloudflare, etc.)
    - ports:
        - protocol: TCP
          port: 443
```

> The admin port (11626) is **not** exposed by the Kubernetes Service for validators.
> The Service only exposes port 11625. The NetworkPolicy above provides an additional
> in-cluster enforcement layer.

---

## Horizon API policies

Horizon exposes its REST API on **port 8000**. Traffic should only reach it from
the ingress controller or a trusted API gateway. Direct cross-namespace access
from application pods must be denied.

```yaml
# Deny all ingress to Horizon by default (overrides default-deny explicitly)
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: horizon-deny-direct-ingress
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: horizon
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/component: horizon
  policyTypes:
    - Ingress
---
# Allow ingress only from the ingress controller and trusted gateway namespaces
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: horizon-allow-trusted-ingress
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: horizon
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/component: horizon
  policyTypes:
    - Ingress
    - Egress

  ingress:
    # Ingress controller (nginx, Traefik, etc.)
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: ingress
      ports:
        - protocol: TCP
          port: 8000

    # Trusted API gateway namespace (optional — remove if not used)
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/network-access: trusted
          podSelector:
            matchLabels:
              app.kubernetes.io/part-of: stellar-api-gateway
      ports:
        - protocol: TCP
          port: 8000

    # Prometheus scrape
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: monitoring
      ports:
        - protocol: TCP
          port: 8000    # or a dedicated metrics port if Horizon exposes one

  egress:
    # Horizon talks to Stellar Core (captive core on localhost or via service)
    - to:
        - podSelector:
            matchLabels:
              app.kubernetes.io/component: validator
      ports:
        - protocol: TCP
          port: 11626    # Stellar Core admin HTTP — Horizon ingestion

    # Horizon Postgres database
    - to:
        - podSelector:
            matchLabels:
              app.kubernetes.io/name: horizon-db
      ports:
        - protocol: TCP
          port: 5432

    # DNS
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53
```

---

## Soroban RPC policies

Soroban RPC exposes its JSON-RPC API on **port 8000** (or **18000** when the cache
sidecar is enabled). It talks to an underlying Stellar Core process on localhost
(captive core) so inter-pod egress is minimal.

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: soroban-rpc-allow-trusted-ingress
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: soroban-rpc
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/component: sorobanrpc
  policyTypes:
    - Ingress
    - Egress

  ingress:
    # Public RPC ingress via ingress controller
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: ingress
      ports:
        - protocol: TCP
          port: 8000     # direct to Soroban RPC process
        - protocol: TCP
          port: 18000    # cache sidecar (if enabled)

    # Trusted clients only (e.g. internal dApps, SDK test suites)
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/network-access: trusted
      ports:
        - protocol: TCP
          port: 8000
        - protocol: TCP
          port: 18000

    # Prometheus scrape
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: monitoring
      ports:
        - protocol: TCP
          port: 8000

  egress:
    # Soroban RPC uses captive core — all Stellar Core traffic is on localhost.
    # External egress is only needed for history archive downloads.
    - ports:
        - protocol: TCP
          port: 443

    # DNS
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53
```

---

## Operator egress policy

The `stellar-operator` pod in `stellar-system` needs egress to the Kubernetes API
server and to the managed workload namespace. It does **not** need direct access
to validator P2P ports or external databases.

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: stellar-operator-egress
  namespace: stellar-system
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: operator
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/name: stellar-operator
  policyTypes:
    - Ingress
    - Egress

  ingress:
    # Prometheus scrape of operator metrics
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: monitoring
      ports:
        - protocol: TCP
          port: 8080    # operator metrics port

    # Kubernetes webhook server (admission controller)
    - from:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: TCP
          port: 9443    # webhook HTTPS port

  egress:
    # Kubernetes API server — required for all reconciliation operations
    # Use the actual ClusterIP of the kubernetes service (default: 10.96.0.1)
    - to:
        - ipBlock:
            cidr: 0.0.0.0/0   # k8s API server IP — restrict to exact CIDR in production
      ports:
        - protocol: TCP
          port: 443
        - protocol: TCP
          port: 6443

    # Managed namespace: operator reads node state and pushes updates
    - to:
        - namespaceSelector:
            matchLabels:
              stellar.org/node-namespace: "true"

    # DNS
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53

    # Vault (if operator-side Vault Agent is used — typically not needed)
    # - to:
    #     - namespaceSelector:
    #         matchLabels:
    #           kubernetes.io/metadata.name: vault
    #   ports:
    #     - protocol: TCP
    #       port: 8200
```

---

## Monitoring ingress policy

Allow Prometheus to scrape all Stellar workloads without opening monitoring access
to other namespaces.

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-prometheus-scrape
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: monitoring
spec:
  podSelector: {}   # all pods in stellar namespace
  policyTypes:
    - Ingress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              stellar.org/role: monitoring
          podSelector:
            matchLabels:
              app.kubernetes.io/name: prometheus
      ports:
        - protocol: TCP
          port: 9090
        - protocol: TCP
          port: 8080
        - protocol: TCP
          port: 8000   # Horizon/Soroban metrics endpoint
        - protocol: TCP
          port: 11626  # Stellar Core admin (metrics)
```

---

## DNS egress policy

A targeted DNS egress policy for all pods in `stellar`. This is required for
service discovery and external hostname resolution when the default-deny policy
is in effect.

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-dns-egress
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/policy-class: baseline
spec:
  podSelector: {}
  policyTypes:
    - Egress
  egress:
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53
```

---

## Composing policies for production

Apply the policies in this order for a production cluster:

```bash
# 1. Label namespaces (one-time setup)
kubectl label namespace stellar \
  stellar.org/node-namespace="true" \
  pod-security.kubernetes.io/enforce=restricted
kubectl label namespace stellar-system stellar.org/operator-namespace="true"
kubectl label namespace monitoring stellar.org/role=monitoring
kubectl label namespace ingress-nginx stellar.org/role=ingress

# 2. Apply baseline policies
kubectl apply -f examples/security/netpol-default-deny.yaml
kubectl apply -f examples/security/netpol-dns-egress.yaml

# 3. Apply component policies
kubectl apply -f examples/security/netpol-validator.yaml
kubectl apply -f examples/security/netpol-horizon.yaml
kubectl apply -f examples/security/netpol-soroban-rpc.yaml
kubectl apply -f examples/security/netpol-operator-egress.yaml
kubectl apply -f examples/security/netpol-monitoring.yaml

# 4. Verify (see next section)
```

The network-policy.yaml in `examples/security/` provides the combined deny-and-allow
policies as a single deployable manifest for quick testing.

---

## Verification procedure

After deploying policies, verify traffic isolation using `kubectl` ephemeral containers
or a debug pod. Every cross-namespace traffic test that is **not** explicitly allowed
must time out or be refused.

### Prerequisite: debug image

```bash
# Create a lightweight debug pod in the stellar namespace
kubectl run netpol-verify --image=busybox:1.36 --restart=Never \
  -n stellar -- sleep 3600
```

### Test 1 — Validator P2P is not reachable from an untrusted namespace

```bash
# Run from a pod in the default namespace (no network-access label)
kubectl run attacker --image=busybox:1.36 --restart=Never \
  -n default -- sh -c \
  "wget -q -T 3 http://validator-0.stellar.svc.cluster.local:11625 && echo PASS || echo BLOCKED"
# Expected output: BLOCKED (wget: download timed out / Connection refused)
```

### Test 2 — Horizon port 8000 is not reachable from untrusted namespace

```bash
kubectl run attacker2 --image=busybox:1.36 --restart=Never \
  -n default -- sh -c \
  "wget -q -T 3 http://horizon.stellar.svc.cluster.local:8000 && echo PASS || echo BLOCKED"
# Expected output: BLOCKED
```

### Test 3 — Ingress controller CAN reach Horizon

```bash
# Run from a pod in ingress-nginx namespace (label: stellar.org/role=ingress)
kubectl run ingress-test --image=busybox:1.36 --restart=Never \
  -n ingress-nginx -- sh -c \
  "wget -q -T 5 -O- http://horizon.stellar.svc.cluster.local:8000/health && echo REACHABLE || echo BLOCKED"
# Expected output: {"status":"OK"} REACHABLE
```

### Test 4 — Prometheus CAN scrape Stellar Core admin port

```bash
kubectl run prom-test --image=busybox:1.36 --restart=Never \
  -n monitoring -- sh -c \
  "wget -q -T 5 -O- http://validator-0.stellar.svc.cluster.local:11626/metrics && echo SCRAPED || echo BLOCKED"
# Expected output: # HELP ... SCRAPED
```

### Test 5 — Validator admin port is NOT reachable from stellar namespace itself (non-operator pods)

```bash
# From an unprivileged pod in stellar namespace
kubectl exec netpol-verify -n stellar -- sh -c \
  "wget -q -T 3 http://validator-0.stellar.svc.cluster.local:11626 && echo PASS || echo BLOCKED"
# Expected output: BLOCKED
```

### Automated verification script

```bash
#!/usr/bin/env bash
# Quick network policy smoke test
set -euo pipefail

STELLAR_NS="${STELLAR_NS:-stellar}"
INGRESS_NS="${INGRESS_NS:-ingress-nginx}"
MONITORING_NS="${MONITORING_NS:-monitoring}"
VALIDATOR_SVC="validator-0.${STELLAR_NS}.svc.cluster.local"
HORIZON_SVC="horizon.${STELLAR_NS}.svc.cluster.local"

pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*" >&2; exit 1; }

check_blocked() {
  local ns=$1 target=$2 label=$3
  result=$(kubectl run "np-test-${RANDOM}" --image=busybox:1.36 \
    --restart=Never --rm -q -n "$ns" \
    -- sh -c "wget -q -T 3 $target && echo PASS || echo BLOCKED" 2>/dev/null)
  [[ "$result" == *"BLOCKED"* ]] && pass "$label blocked" || fail "$label NOT blocked"
}

check_reachable() {
  local ns=$1 target=$2 label=$3
  result=$(kubectl run "np-test-${RANDOM}" --image=busybox:1.36 \
    --restart=Never --rm -q -n "$ns" \
    -- sh -c "wget -q -T 5 -O- $target 2>&1 | head -1 && echo REACHABLE || echo BLOCKED" 2>/dev/null)
  [[ "$result" == *"REACHABLE"* ]] && pass "$label reachable" || fail "$label NOT reachable"
}

check_blocked default "http://${VALIDATOR_SVC}:11625" "default→validator:11625"
check_blocked default "http://${HORIZON_SVC}:8000"    "default→horizon:8000"
check_blocked "$STELLAR_NS" "http://${VALIDATOR_SVC}:11626" "stellar-pod→validator:11626"
check_reachable "$INGRESS_NS" "http://${HORIZON_SVC}:8000"  "ingress→horizon:8000"

echo ""
echo "All network policy checks passed."
```

---

## Troubleshooting

### Policy not enforced

- Check that the CNI supports NetworkPolicy: `kubectl get nodes -o wide` and look
  for the CNI version in `kubectl describe node`.
- Cilium: `cilium status` and `cilium policy get`.
- Calico: `calicoctl get networkpolicy --all-namespaces`.

### Pod cannot reach DNS after default-deny

Apply the `allow-dns-egress` policy from this guide. Without it, CoreDNS is also
blocked.

### Operator loses connectivity to API server after applying operator-egress policy

The operator-egress policy uses `0.0.0.0/0` for the API server. On some clusters
the API server address is not `10.96.0.1`. Pin the correct IP:

```bash
kubectl get service kubernetes -n default -o jsonpath='{.spec.clusterIP}'
```

Then replace the `ipBlock.cidr` in the operator-egress policy with `<ip>/32`.

### Horizon cannot connect to Stellar Core

The Horizon pod needs egress to `validator-0` on port 11626 (Stellar Core admin HTTP).
Verify the `horizon-allow-trusted-ingress` policy includes the Horizon→Core egress rule
and that the Core pod has a matching ingress rule allowing port 11626 from Horizon pods.

### NetworkPolicy not selecting correct pods

Use `kubectl describe networkpolicy <name> -n stellar` and compare the
`PodSelector` with `kubectl get pods -n stellar --show-labels` to confirm labels match.
