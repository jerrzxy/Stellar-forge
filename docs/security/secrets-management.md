# Secrets Management for Stellar-K8s

This guide covers how to secure **validator seed material** (`seedSecretRef`) and
other sensitive credentials using external secret managers. It provides concrete,
deployable configurations for HashiCorp Vault (via the Vault Agent Injector and via
External Secrets Operator) and AWS Secrets Manager (via ESO with IRSA).

> **Related docs**
> - [RBAC Hardening](rbac-hardening.md) — least-privilege operator RBAC
> - [Network Policies](network-policies.md) — traffic isolation
> - [credentials-and-secrets.md](credentials-and-secrets.md) — quick-reference index
> - [key-sharding.md](key-sharding.md) — Shamir splitting for air-gapped ceremonies

---

## Contents

1. [Threat model](#threat-model)
2. [Architecture overview](#architecture-overview)
3. [Kubernetes native Secrets (baseline)](#kubernetes-native-secrets-baseline)
4. [HashiCorp Vault — Vault Agent Injector](#hashicorp-vault--vault-agent-injector)
5. [HashiCorp Vault — External Secrets Operator](#hashicorp-vault--external-secrets-operator)
6. [AWS Secrets Manager — External Secrets Operator with IRSA](#aws-secrets-manager--external-secrets-operator-with-irsa)
7. [GCP Secret Manager — External Secrets Operator with Workload Identity](#gcp-secret-manager--external-secrets-operator-with-workload-identity)
8. [Secret rotation](#secret-rotation)
9. [Encryption at rest](#encryption-at-rest)
10. [Audit and compliance](#audit-and-compliance)
11. [Validation checklist](#validation-checklist)

---

## Threat model

The `STELLAR_SEED` (validator signing key) is a Tier-0 asset. Compromise leads to
validator impersonation, double-signing, and loss of network trust. The design goals
for every integration in this document are:

| Goal | Mechanism |
|---|---|
| Seed never stored in Git | External secret manager as the single source of truth |
| Seed never written by operator | `stellar-operator` is read-only to Kubernetes Secrets in the strict RBAC profile |
| Seed access is auditable | Provider audit logs + Kubernetes audit log |
| Seed rotation is zero-downtime | ESO refresh interval / Vault lease renewal without pod restart |
| Blast radius of key compromise is limited | One seed per secret; no wildcard paths |

---

## Architecture overview

```
┌──────────────────────────────────────────────────────────────────────┐
│  External Secret Store (Vault / AWS Secrets Manager / GCP SM)        │
│  ● Authoritative seed store                                           │
│  ● Audit log for every access                                         │
│  ● Access scoped to one path per validator                            │
└─────────────────────┬────────────────────────────────────────────────┘
                      │  pull (mTLS or IRSA JWT)
                      ▼
┌─────────────────────────────────────────────────────────────────────┐
│  External Secrets Operator (ESO)                        namespace:   │
│  ● SecretStore / ClusterSecretStore                     stellar      │
│  ● ExternalSecret → Kubernetes Secret (named seedSecretRef)          │
│  ● Refresh interval enforces automatic rotation                      │
└──────────────────┬──────────────────────────────────────────────────┘
                   │  Kubernetes Secret (in-cluster only, etcd-encrypted)
                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│  StellarNode CR                            namespace: stellar         │
│  spec.validatorConfig.seedSecretRef: "my-validator-seed"             │
│                                                                       │
│  Operator reads Secret (GET only) → mounts as env/volume into pod    │
└──────────────────────────────────────────────────────────────────────┘
```

The operator (`stellar-operator` SA) **never writes Secrets**. It only reads the
reference name from the CRD and mounts the resulting Kubernetes Secret into the
validator pod.

---

## Kubernetes native Secrets (baseline)

For development clusters or air-gapped environments where an external store is
unavailable, store the seed in a Kubernetes Secret directly. Always enable
**envelope encryption** at the etcd level before doing this in production.

```yaml
# 1. Create the seed secret (substitute real seed value)
kubectl create secret generic my-validator-seed \
  --namespace stellar \
  --from-literal=seed=SXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX \
  --dry-run=client -o yaml | kubectl apply -f -
```

Reference from the StellarNode CR:

```yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: my-validator
  namespace: stellar
spec:
  nodeType: Validator
  network: mainnet
  version: "v21.0.0"
  validatorConfig:
    seedSecretRef: my-validator-seed   # must exist before the CR is applied
    enableHistoryArchive: true
  storage:
    storageClass: standard
    size: 500Gi
```

**Hardening checklist for native Secrets:**

- [ ] etcd encryption at rest enabled (`--encryption-provider-config`)
- [ ] `kubectl get secret -n stellar my-validator-seed -o yaml` returns `data` as
      base64 only — never plaintext in API server response
- [ ] Network policy prevents pods other than the operator and the validator from
      reading the Secret
- [ ] RBAC: only `stellar-operator` SA and the validator pod SA can `get` the Secret

---

## HashiCorp Vault — Vault Agent Injector

When `spec.validatorConfig.seedSecretSource.vaultRef` is set, Stellar-K8s adds
Vault Agent Injector annotations to the validator pod. The operator never retrieves
the seed value itself; Vault Agent injects it at pod start and renews the lease
continuously.

### Step 1 — Vault KV policy (HCL)

```hcl
# stellar-validator.hcl — grant read-only access to one validator's path
path "secret/data/stellar/validators/validator-0" {
  capabilities = ["read"]
}

# Deny list so the agent cannot enumerate paths
path "secret/metadata/stellar/validators/" {
  capabilities = ["deny"]
}
```

```bash
# Apply the policy
vault policy write stellar-validator stellar-validator.hcl
```

### Step 2 — Kubernetes auth role

```bash
vault write auth/kubernetes/role/stellar-validator \
  bound_service_account_names=stellar-node \
  bound_service_account_namespaces=stellar \
  policies=stellar-validator \
  ttl=1h \
  max_ttl=4h
```

> The role is bound to **`stellar-node`**, the workload ServiceAccount, not
> `stellar-operator`. See `examples/security/strict-rbac.yaml` for the SA definition.

### Step 3 — Write the seed (once, in a ceremony)

```bash
# Never store the seed in shell history; use stdin or a temporary file with
# permissions restricted to the operator performing the ceremony.
vault kv put secret/stellar/validators/validator-0 seed=@/secure/path/seed.txt
```

### Step 4 — StellarNode CR

```yaml
apiVersion: stellar.org/v1alpha1
kind: StellarNode
metadata:
  name: validator-0
  namespace: stellar
spec:
  nodeType: Validator
  network: mainnet
  version: "v21.0.0"
  validatorConfig:
    seedSecretSource:
      vaultRef:
        role: stellar-validator
        secretPath: secret/data/stellar/validators/validator-0
        secretKey: seed
        secretFileName: stellar-seed   # written to /vault/secrets/stellar-seed inside pod
    enableHistoryArchive: true
  storage:
    storageClass: standard
    size: 500Gi
```

### What the operator injects (informational)

The controller appends these annotations to the pod spec; the Vault Agent sidecar
then handles authentication and file injection:

```yaml
vault.hashicorp.com/agent-inject: "true"
vault.hashicorp.com/role: "stellar-validator"
vault.hashicorp.com/agent-inject-secret-stellar-seed: "secret/data/stellar/validators/validator-0"
vault.hashicorp.com/agent-inject-template-stellar-seed: |
  {{- with secret "secret/data/stellar/validators/validator-0" -}}
  {{ .Data.data.seed }}
  {{- end -}}
```

### Vault audit log verification

```bash
# Confirm the seed path is accessed only by the workload SA JWT
vault audit list
# Then look for auth.metadata.service_account_name=stellar-node
# and request.path=secret/data/stellar/validators/validator-0
```

---

## HashiCorp Vault — External Secrets Operator

ESO provides a declarative `ExternalSecret` CRD that reconciles the Vault path
into a Kubernetes Secret on a configurable interval, enabling zero-downtime rotation
without pod restarts.

### Prerequisites

```bash
# Install ESO (use a pinned version in production)
helm repo add external-secrets https://charts.external-secrets.io
helm install external-secrets external-secrets/external-secrets \
  --namespace external-secrets \
  --create-namespace \
  --version 0.9.20 \
  --set installCRDs=true
```

### SecretStore (namespace-scoped)

```yaml
# examples/security/externalsecret-vault.yaml (extends the existing stub)
apiVersion: v1
kind: ServiceAccount
metadata:
  name: external-secrets
  namespace: stellar
---
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: vault-secrets
  namespace: stellar
spec:
  provider:
    vault:
      server: "https://vault.vault.svc.cluster.local:8200"
      path: "secret"
      version: "v2"
      caBundle: ""  # base64 CA cert, or use caProvider for cert-manager integration
      auth:
        kubernetes:
          mountPath: "kubernetes"
          role: "stellar-mainnet-validator"
          serviceAccountRef:
            name: external-secrets
          # audiences: ["vault"]   # set if your cluster uses a non-default audience
```

### Vault Kubernetes auth role for ESO

```bash
vault write auth/kubernetes/role/stellar-mainnet-validator \
  bound_service_account_names=external-secrets \
  bound_service_account_namespaces=stellar \
  policies=stellar-validator \
  ttl=1h
```

### ExternalSecret — validator seed

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: validator-0-seed
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/node: validator-0
spec:
  refreshInterval: 1h            # re-fetch every hour; Vault policy TTL must cover this
  secretStoreRef:
    name: vault-secrets
    kind: SecretStore
  target:
    name: my-validator-seed       # this is the value of seedSecretRef in the CRD
    creationPolicy: Owner
    deletionPolicy: Retain        # keep the Secret on ExternalSecret deletion for safety
    template:
      type: Opaque
      metadata:
        labels:
          stellar.org/secret-type: validator-seed
  data:
    - secretKey: seed             # key inside the resulting Kubernetes Secret
      remoteRef:
        key: stellar/validators/validator-0
        property: seed
```

Reference from StellarNode:

```yaml
spec:
  validatorConfig:
    seedSecretRef: my-validator-seed   # matches ExternalSecret target.name
```

---

## AWS Secrets Manager — External Secrets Operator with IRSA

### IAM policy (narrowly scoped)

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "StellarValidatorSeedRead",
      "Effect": "Allow",
      "Action": [
        "secretsmanager:GetSecretValue",
        "secretsmanager:DescribeSecret"
      ],
      "Resource": "arn:aws:secretsmanager:us-east-1:000000000000:secret:stellar/prod/validator-*"
    }
  ]
}
```

> Do **not** use `"Resource": "*"`. Pin the ARN pattern to the exact secret prefix.

### IAM role trust policy (IRSA)

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::000000000000:oidc-provider/oidc.eks.us-east-1.amazonaws.com/id/EXAMPLED539D4633E53DE1B716D3041E"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "oidc.eks.us-east-1.amazonaws.com/id/EXAMPLED539D4633E53DE1B716D3041E:sub":
            "system:serviceaccount:stellar:external-secrets",
          "oidc.eks.us-east-1.amazonaws.com/id/EXAMPLED539D4633E53DE1B716D3041E:aud":
            "sts.amazonaws.com"
        }
      }
    }
  ]
}
```

### ServiceAccount with IRSA annotation

```yaml
# examples/security/externalsecret-aws.yaml (extends the existing stub)
apiVersion: v1
kind: ServiceAccount
metadata:
  name: external-secrets
  namespace: stellar
  annotations:
    eks.amazonaws.com/role-arn: "arn:aws:iam::000000000000:role/stellar-seed-read"
    eks.amazonaws.com/sts-regional-endpoints: "true"
```

### SecretStore — AWS Secrets Manager

```yaml
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: aws-secrets-manager
  namespace: stellar
spec:
  provider:
    aws:
      service: SecretsManager
      region: us-east-1
      auth:
        jwt:
          serviceAccountRef:
            name: external-secrets
            audiences:
              - sts.amazonaws.com
```

### ExternalSecret — validator seed

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: validator-0-seed
  namespace: stellar
  labels:
    app.kubernetes.io/managed-by: stellar-operator
    stellar.org/node: validator-0
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: aws-secrets-manager
    kind: SecretStore
  target:
    name: my-validator-seed        # seedSecretRef value in the CRD
    creationPolicy: Owner
    deletionPolicy: Retain
    template:
      type: Opaque
  data:
    - secretKey: seed
      remoteRef:
        key: stellar/prod/validator-0    # AWS Secrets Manager secret name
        property: seed                   # JSON key if the secret is a JSON object
```

### Create the secret in AWS (once, in a ceremony)

```bash
# Store as a JSON object so multiple fields can be added later
aws secretsmanager create-secret \
  --name stellar/prod/validator-0 \
  --description "Stellar mainnet validator-0 seed" \
  --secret-string '{"seed":"SXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"}' \
  --region us-east-1

# Restrict access with a resource-based policy (defence-in-depth)
aws secretsmanager put-resource-policy \
  --secret-id stellar/prod/validator-0 \
  --resource-policy file://secret-resource-policy.json \
  --region us-east-1
```

---

## GCP Secret Manager — External Secrets Operator with Workload Identity

### GCP IAM binding

```bash
# Create a dedicated GSA for ESO
gcloud iam service-accounts create stellar-eso \
  --project=my-project \
  --display-name="Stellar ESO secret reader"

# Grant access to the specific secret only
gcloud secrets add-iam-policy-binding stellar-prod-validator-0 \
  --project=my-project \
  --member="serviceAccount:stellar-eso@my-project.iam.gserviceaccount.com" \
  --role="roles/secretmanager.secretAccessor"

# Bind to the Kubernetes SA via Workload Identity
gcloud iam service-accounts add-iam-policy-binding stellar-eso@my-project.iam.gserviceaccount.com \
  --project=my-project \
  --role=roles/iam.workloadIdentityUser \
  --member="serviceAccount:my-project.svc.id.goog[stellar/external-secrets]"
```

### ServiceAccount with Workload Identity annotation

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: external-secrets
  namespace: stellar
  annotations:
    iam.gke.io/gcp-service-account: "stellar-eso@my-project.iam.gserviceaccount.com"
```

### SecretStore

```yaml
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: gcp-secrets
  namespace: stellar
spec:
  provider:
    gcpsm:
      projectID: my-project
      auth:
        workloadIdentity:
          clusterLocation: us-central1
          clusterName: my-cluster
          clusterProjectID: my-project
          serviceAccountRef:
            name: external-secrets
```

### ExternalSecret

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: validator-0-seed
  namespace: stellar
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: gcp-secrets
    kind: SecretStore
  target:
    name: my-validator-seed
    creationPolicy: Owner
    deletionPolicy: Retain
  data:
    - secretKey: seed
      remoteRef:
        key: stellar-prod-validator-0
        version: latest
```

---

## Secret rotation

### ESO-managed rotation (recommended)

ESO refreshes the Kubernetes Secret at the configured `refreshInterval`. The
validator pod does **not** need to restart unless the seed itself changes:

- For Horizon/Soroban database passwords, the controller automatically reconciles
  when the Secret changes. Set `refreshInterval: 24h` as a starting point.
- For validator seeds, rotation requires a coordinated SCP quorum update. Do **not**
  set a short refresh interval for seeds unless you have an automated rotation workflow.

### Manual rotation procedure

```bash
# 1. Update the value in the external store
vault kv put secret/stellar/validators/validator-0 seed=SNEWSEED...
# or
aws secretsmanager put-secret-value \
  --secret-id stellar/prod/validator-0 \
  --secret-string '{"seed":"SNEWSEED..."}'

# 2. Force ESO to sync immediately (instead of waiting for refreshInterval)
kubectl annotate externalsecret validator-0-seed \
  -n stellar \
  force-sync=$(date +%s) --overwrite

# 3. Verify the Kubernetes Secret was updated
kubectl get secret my-validator-seed -n stellar \
  -o jsonpath='{.metadata.resourceVersion}'

# 4. The operator will pick up the new seed on the next reconciliation loop.
# For zero-downtime, the validator pod continues with the old seed until the
# operator triggers a rolling update (StatefulSet updateStrategy: RollingUpdate).
```

---

## Encryption at rest

Kubernetes Secrets are stored as base64 in etcd by default. Enable envelope
encryption to protect them with a KMS key:

```yaml
# /etc/kubernetes/encryption-config.yaml (kube-apiserver)
apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
    providers:
      - aescbc:
          keys:
            - name: key1
              secret: <base64-encoded-32-byte-key>
      - identity: {}   # fallback for old secrets not yet re-encrypted
```

For managed Kubernetes services:

- **EKS**: enable Secrets encryption via `--encryption-config` or the EKS console
  (KMS key required). See [AWS docs](https://docs.aws.amazon.com/eks/latest/userguide/enable-kms.html).
- **GKE**: application-layer Secrets encryption is enabled per-cluster at creation.
  See [GKE docs](https://cloud.google.com/kubernetes-engine/docs/how-to/encrypting-secrets).
- **AKS**: Secrets are encrypted by default with platform-managed keys. Customer-managed
  keys are available via Azure Disk Encryption.

After enabling encryption, re-encrypt all existing Secrets:

```bash
kubectl get secrets --all-namespaces -o json | kubectl replace -f -
```

---

## Audit and compliance

### Kubernetes audit log — Secret access events

Configure the API server audit policy to log all access to Secrets in the `stellar`
namespace:

```yaml
# /etc/kubernetes/audit-policy.yaml
apiVersion: audit.k8s.io/v1
kind: Policy
rules:
  # Log all Secret access in stellar namespace at RequestResponse level
  - level: RequestResponse
    resources:
      - group: ""
        resources: ["secrets"]
    namespaces: ["stellar", "stellar-system"]
  # Log other requests at Metadata level
  - level: Metadata
```

### Vault audit log verification

```bash
# Enable file audit device (use syslog or socket in production)
vault audit enable file file_path=/var/log/vault/audit.log

# Verify validator seed path access is recorded
vault audit list -detailed
grep '"path":"secret/data/stellar/validators/validator-0"' /var/log/vault/audit.log \
  | jq '.auth.metadata.service_account_name'
```

### AWS CloudTrail verification

```bash
# Query CloudTrail for GetSecretValue calls on the validator secret
aws cloudtrail lookup-events \
  --lookup-attributes AttributeKey=EventName,AttributeValue=GetSecretValue \
  --region us-east-1 \
  | jq '.Events[] | select(.CloudTrailEvent | fromjson | .requestParameters.secretId
        | startswith("stellar/prod/validator"))'
```

---

## Validation checklist

Run these checks after deploying an external secrets integration:

```bash
# 1. Verify ExternalSecret status is Ready
kubectl get externalsecret -n stellar
# Expected: READY=True, STATUS=SecretSynced

# 2. Verify the Kubernetes Secret was created
kubectl get secret my-validator-seed -n stellar

# 3. Verify the operator can read but NOT write the Secret
kubectl auth can-i get secrets -n stellar \
  --as system:serviceaccount:stellar-system:stellar-operator
# Expected: yes

kubectl auth can-i create secrets -n stellar \
  --as system:serviceaccount:stellar-system:stellar-operator
# Expected: no

kubectl auth can-i update secrets -n stellar \
  --as system:serviceaccount:stellar-system:stellar-operator
# Expected: no

# 4. Verify no other ServiceAccount can read the seed Secret
kubectl auth can-i get secrets/my-validator-seed -n stellar \
  --as system:serviceaccount:stellar:default
# Expected: no

# 5. Verify the StellarNode is reconciling without RBAC errors
kubectl logs -n stellar-system deploy/stellar-operator --since=5m \
  | grep -Ei 'forbidden|permission denied|cannot (get|list|watch).*secret'
# Expected: no output

# 6. Verify etcd encryption is active (requires cluster admin access)
kubectl get secret my-validator-seed -n stellar -o yaml \
  | grep -q "k8s:enc:aescbc" && echo "encrypted" || echo "NOT encrypted"
```
