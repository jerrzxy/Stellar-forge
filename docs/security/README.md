# Stellar-K8s Security Hardening Manual

This manual documents the defence-in-depth security posture for production
Stellar-K8s deployments. It covers the three pillars of Kubernetes security
hardening: **identity and access control (RBAC)**, **secrets management**, and
**network isolation (NetworkPolicy)**.

> Every configuration in this manual is backed by a deployable YAML example in
> [`examples/security/`](../../examples/security/). There is no theoretical advice
> without a concrete, testable artifact.

---

## Manual structure

| Document | What it covers | Example manifests |
|---|---|---|
| [RBAC Hardening](rbac-hardening.md) | Least-privilege operator RBAC, workload ServiceAccounts, Pod Security Standards | [`strict-rbac.yaml`](../../examples/security/strict-rbac.yaml), [`operator-rbac.yaml`](../../examples/security/operator-rbac.yaml), [`audit-rbac.sh`](../../examples/security/audit-rbac.sh) |
| [Secrets Management](secrets-management.md) | HashiCorp Vault, AWS Secrets Manager, GCP Secret Manager, ESO integration, rotation | [`externalsecret-vault.yaml`](../../examples/security/externalsecret-vault.yaml), [`externalsecret-aws.yaml`](../../examples/security/externalsecret-aws.yaml) |
| [Network Policies](network-policies.md) | Default-deny, validator isolation, Horizon/Soroban ingress control, operator egress | [`network-policy.yaml`](../../examples/security/network-policy.yaml) |
| [Pod Security Standards](pss.md) | Namespace PSS labels, seccomp, capabilities | Included in `strict-rbac.yaml` |
| [P2P Firewall](p2p-firewall.md) | eBPF-level validator peer allowlist | `security/p2p-firewall/` |
| [Compliance Guide](compliance-guide.md) | SOC 2, PCI-DSS, GDPR control mapping | — |
| [Credentials and Secrets](credentials-and-secrets.md) | Quick-reference index for all credential types | — |
| [Secrets Access Control](secrets-access-control.md) | Rotation runbooks, cert-manager, RBAC templates | — |
| [Key Sharding](key-sharding.md) | Shamir sharding, Vault Transit, air-gapped seed ceremonies | [`examples/scripts/shamir-split.py`](../../examples/scripts/shamir-split.py) |
| [Incident Response Playbook](incident-response-playbook.md) | Fast containment, forensic continuity | — |
| [RPC DoS Mitigation](rpc-dos-mitigation.md) | L7 ingress rate limiting, Soroban RPC hardening | — |
| [Compliance Mapping](compliance-mapping.md) | Audit-ready control traceability | — |
| [CIS Hardening](cis-kubernetes-hardening.md) | CIS Kubernetes Benchmark compliance | — |
| [Dependency Updates](dependency-updates.md) | Dependabot, CI gates, merge process | — |
| [mTLS Cert Rotation](mtls-cert-rotation.md) | cert-manager based mTLS rotation | — |
| [E2E Encryption Architecture](e2e-encryption-architecture.md) | End-to-end encryption design | — |

---

## Security principles

1. **Validator signing material is Tier-0.** `STELLAR_SEED` is never stored in Git,
   never logged, never written by the operator, and is always sourced from an
   external secret manager or Vault Agent injection.

2. **No RBAC wildcards.** The operator ServiceAccount uses explicit `resources` and
   `verbs`. Wildcard grants exist in no production profile.

3. **Network default-deny.** Every Stellar namespace applies a default-deny
   NetworkPolicy. Allowed traffic is explicitly enumerated.

4. **Workload pods have no API access.** Validator, Horizon, and Soroban RPC pods
   run with an empty-rules Role. They do not talk to the Kubernetes API server.

5. **Operator is read-only to Secrets.** The operator ServiceAccount can `get`,
   `list`, and `watch` Secrets but cannot `create`, `update`, or `delete` them.
   External Secrets Operator writes Secrets under its own ServiceAccount.

6. **PSS restricted for workload namespaces.** The `stellar` namespace enforces
   `pod-security.kubernetes.io/enforce: restricted`.

---

## Quick-start hardening checklist

### Step 1 — Apply strict RBAC

```bash
# Removes all default RBAC and applies the strict least-privilege profile
kubectl apply -f examples/security/strict-rbac.yaml

# Verify
bash examples/security/audit-rbac.sh
```

See [RBAC Hardening](rbac-hardening.md) for the full permission map and rejection criteria.

### Step 2 — Configure external secret management

Choose one provider:

**HashiCorp Vault (recommended for on-premises and multi-cloud):**

```bash
# 1. Write policy and Kubernetes auth role in Vault
# 2. Store the validator seed
vault kv put secret/stellar/validators/validator-0 seed=SXXX...

# 3. Deploy ESO SecretStore + ExternalSecret
kubectl apply -f examples/security/externalsecret-vault.yaml

# 4. Verify sync
kubectl get externalsecret validator-0-seed -n stellar
# STATUS: SecretSynced
```

**AWS Secrets Manager (EKS with IRSA):**

```bash
# 1. Create IAM role, write secret in AWS
aws secretsmanager create-secret \
  --name stellar/prod/validator-0 \
  --secret-string '{"seed":"SXXX..."}'

# 2. Deploy ESO SecretStore + ExternalSecret
kubectl apply -f examples/security/externalsecret-aws.yaml

# 3. Verify sync
kubectl get externalsecret validator-0-seed -n stellar
# STATUS: SecretSynced
```

See [Secrets Management](secrets-management.md) for GCP Secret Manager, Vault Agent
Injector, rotation procedures, and etcd encryption.

### Step 3 — Apply NetworkPolicies

```bash
# Label namespaces (one-time)
kubectl label namespace stellar \
  stellar.org/node-namespace="true" \
  pod-security.kubernetes.io/enforce=restricted
kubectl label namespace stellar-system stellar.org/operator-namespace="true"
kubectl label namespace monitoring stellar.org/role=monitoring
kubectl label namespace ingress-nginx stellar.org/role=ingress

# Apply all policies
kubectl apply -f examples/security/network-policy.yaml

# Verify cross-namespace traffic is blocked
kubectl run netpol-test --image=busybox:1.36 --restart=Never -n default -- \
  sh -c "wget -q -T 3 http://horizon.stellar.svc.cluster.local:8000 && echo OPEN || echo BLOCKED"
# Expected: BLOCKED
```

See [Network Policies](network-policies.md) for per-component policy breakdowns and
the full automated verification script.

### Step 4 — Enable etcd encryption at rest

```bash
# EKS
aws eks update-cluster-config \
  --name my-cluster \
  --resources-vpc-config '...' \
  --encryption-config '[{"provider":{"keyArn":"arn:aws:kms:..."},"resources":["secrets"]}]'

# Re-encrypt all existing Secrets
kubectl get secrets --all-namespaces -o json | kubectl replace -f -
```

### Step 5 — Enable Kubernetes audit logging

Configure the API server with an audit policy that captures `RequestResponse` level
events for all Secret operations in the `stellar` namespace. See
[Secrets Management — Audit and compliance](secrets-management.md#audit-and-compliance).

---

## Example manifests reference

| File | Description |
|---|---|
| [`strict-rbac.yaml`](../../examples/security/strict-rbac.yaml) | Full strict RBAC profile: SA, Roles, ClusterRoles, PSS namespace labels, workload SA |
| [`operator-rbac.yaml`](../../examples/security/operator-rbac.yaml) | Alternative per-namespace RBAC for multi-namespace deployments |
| [`network-policy.yaml`](../../examples/security/network-policy.yaml) | All NetworkPolicies: default-deny, validator, Horizon, Soroban, operator, monitoring, DNS |
| [`externalsecret-vault.yaml`](../../examples/security/externalsecret-vault.yaml) | ESO SecretStore + ExternalSecret for HashiCorp Vault |
| [`externalsecret-aws.yaml`](../../examples/security/externalsecret-aws.yaml) | ESO SecretStore + ExternalSecret for AWS Secrets Manager with IRSA |
| [`audit-rbac.sh`](../../examples/security/audit-rbac.sh) | `auth can-i` validation script — verifies allowed and denied operations |

---

## Threat model summary

| Asset | Threat | Mitigation |
|---|---|---|
| Validator seed (`STELLAR_SEED`) | Git/log exposure | External secret manager, no operator Secret writes |
| Validator seed | Kubernetes API exfiltration | RBAC read-only for operator, network policy blocks pod-to-pod |
| Soroban RPC admin | Unauthorised smart contract invocation | NetworkPolicy allows only ingress controller + trusted namespace |
| Horizon REST API | Direct cross-namespace access from compromised pod | NetworkPolicy denies untrusted namespaces |
| Operator ServiceAccount token | Lateral movement if compromised | automountServiceAccountToken=false, no RBAC wildcards |
| etcd data | Node compromise → secret extraction | Envelope encryption with KMS |
| P2P validator port 11625 | DDoS from non-quorum peers | NetworkPolicy + eBPF p2p-firewall |

---

## Related production documentation

- [Production Security Hardening](../production-security-hardening.md) — comprehensive
  full-cluster hardening guide
- [Gatekeeper Policies](../gatekeeper-policies.md) — OPA policy enforcement
- [mTLS Guide](../mtls-guide.md) — mutual TLS for inter-component communication
- [Service Mesh](../service-mesh.md) — Istio/Linkerd integration for zero-trust networking
- [Network Isolation](../network-isolation.md) — operator-generated per-node policies
- [Secret Rotation](../secret-rotation.md) — zero-downtime database credential rotation
