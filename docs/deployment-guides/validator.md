# Validator Node Deployment

Comprehensive guide for deploying and managing Stellar validator nodes on Kubernetes.

## Overview

Stellar validators participate in consensus by voting on transaction sets and blocks. Running a validator requires careful consideration of security, availability, and performance.

## Basic Validator Deployment

### Minimal Configuration

```yaml title="validator-basic.yaml"
apiVersion: stellar.k8s.io/v1alpha1
kind: StellarValidator
metadata:
  name: validator-node
  namespace: stellar
spec:
  network: mainnet
  replicas: 1
  
  config:
    nodeIsValidator: true
    publicNetwork: true
    
  storage:
    size: 1Ti
    storageClassName: fast-ssd
    
  resources:
    requests:
      cpu: "8"
      memory: "16Gi"
    limits:
      cpu: "16"
      memory: "32Gi"
```

Apply the configuration:

```bash
kubectl apply -f validator-basic.yaml
```

## Advanced Configuration

### Full Production Configuration

```yaml title="validator-production.yaml"
apiVersion: stellar.k8s.io/v1alpha1
kind: StellarValidator
metadata:
  name: prod-validator
  namespace: stellar-prod
  annotations:
    description: "Production mainnet validator"
spec:
  network: mainnet
  replicas: 3  # High availability
  
  # Node configuration
  config:
    nodeIsValidator: true
    publicNetwork: true
    nodeSeed: "secret://stellar-validator-seed"  # Kubernetes Secret reference
    
    # Consensus configuration
    catchupRecent: 8192
    catchupComplete: false
    maxConcurrentSubprocesses: 16
    
    # Network configuration
    peerPort: 11625
    targetPeerConnections: 20
    maxAdditionalPeerConnections: 50
    maxPendingConnections: 500
    peerReadingCapacity: 20000000
    peerFloodCapacity: 200
    
    # Database configuration
    databaseUrl: "postgresql://stellar:password@postgres:5432/stellar"
    
    # History configuration
    historyArchives:
      - name: "sdf"
        url: "https://history.stellar.org/prd/core-live/core_live_001"
      - name: "backup"
        url: "https://stellar-history.example.com/archive"
    
    # Quorum configuration
    quorumSet:
      threshold: 3
      validators:
        - "GCGB2S2KGYARPVIA37HYZXVRM2YZUEXA6S33ZU5BUDC6THSB62LZSTYH"  # SDF 1
        - "GCM6QMP3DLRPTAZW2UZPCPX2LF3SXWXKPMP3GKFZBDSF3QZGV2G5QSTK"  # SDF 2
        - "GABMKJM6I25XI4K7U6XWMULOUQIQ27BCTMLS6BYYSOWKTBUXVRJSXHYQ"  # SDF 3
        - "$self"  # This validator
      
  # Storage configuration
  storage:
    size: 2Ti
    storageClassName: premium-ssd
    
    # Enable volume snapshots
    volumeSnapshotClassName: volumesnapshot-class
    backupSchedule: "0 2 * * *"  # Daily at 2 AM
    
  # Resource allocation
  resources:
    requests:
      cpu: "16"
      memory: "32Gi"
    limits:
      cpu: "32"
      memory: "64Gi"
      
  # Pod placement
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - labelSelector:
            matchLabels:
              app: prod-validator
          topologyKey: kubernetes.io/hostname
          
  tolerations:
    - key: "node-role.kubernetes.io/validator"
      operator: "Equal"
      value: "true"
      effect: "NoSchedule"
      
  # Node selector
  nodeSelector:
    node-type: high-performance
    
  # Security context
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    fsGroup: 10001
    seccompProfile:
      type: RuntimeDefault
      
  # Monitoring
  monitoring:
    enabled: true
    serviceMonitor: true
    prometheusRule: true
    grafanaDashboard: true
    
  # Networking
  service:
    type: LoadBalancer
    annotations:
      service.beta.kubernetes.io/aws-load-balancer-type: "nlb"
      service.beta.kubernetes.io/aws-load-balancer-cross-zone-load-balancing-enabled: "true"
    loadBalancerSourceRanges:
      - "0.0.0.0/0"  # Adjust for security
```

Apply with validation:

```bash
kubectl apply --dry-run=server -f validator-production.yaml
kubectl apply -f validator-production.yaml
```

## Configuration Options

### Node Identity

Set up validator identity using Kubernetes Secrets:

```bash
# Generate validator keypair
stellar-core gen-seed

# Create secret
kubectl create secret generic stellar-validator-seed \
  --from-literal=seed='SBXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX' \
  -n stellar-prod
```

Reference in validator spec:

```yaml
spec:
  config:
    nodeSeed: "secret://stellar-validator-seed"
```

### Validator Identity Rotation

Long-lived validator seed keys become cryptographic liabilities over time. The operator automates rotation of ed25519 node identities using GitOps principles, pulling dynamically generated identities from an external secret store via the External Secrets Operator (ESO) and rotating them without jeopardizing quorum.

#### How Rotation Works

The rotation controller (`controller/src/security/rotation.rs`) coordinates with the ESO integration (`controller/src/security/eso_integration.rs`) to perform the following sequence for each node:

1. Fetch the next ed25519 identity from the external secret store (AWS Secrets Manager or HashiCorp Vault).
2. Gracefully un-peer the node from the consensus network.
3. Update the ConfigMap with the new seed and restart the pod.
4. Rejoin the consensus network and verify ledger advancement resumes.
5. Propagate the rotated public key to any attached Horizon ingestion workers.

#### External Secrets Operator Setup

Install ESO and configure a `SecretStore` pointing at your secret backend:

```yaml title="eso-secretstore.yaml"
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: validator-secrets
  namespace: stellar-prod
spec:
  provider:
    aws:
      service: SecretsManager
      region: us-east-1
      auth:
        jwt:
          serviceAccountRef:
            name: stellar-validator
```

For HashiCorp Vault, use the `vault` provider with Kubernetes auth instead:

```yaml title="eso-vault-secretstore.yaml"
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: validator-secrets
  namespace: stellar-prod
spec:
  provider:
    vault:
      server: "https://vault.example.com"
      path: "stellar"
      version: "v2"
      auth:
        kubernetes:
          mountPath: "kubernetes"
          role: "stellar-validator"
          serviceAccountRef:
            name: stellar-validator
```

Define an `ExternalSecret` that maps the dynamically generated node identity into a Kubernetes Secret consumed by the validator:

```yaml title="validator-identity-externalsecret.yaml"
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: stellar-validator-seed
  namespace: stellar-prod
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: validator-secrets
    kind: SecretStore
  target:
    name: stellar-validator-seed
    creationPolicy: Owner
  data:
    - secretKey: seed
      remoteRef:
        key: stellar/validators/prod-validator
        property: seed
```

#### Rotation Policy

Enable automated rotation in the validator spec:

```yaml
spec:
  config:
    nodeSeed: "secret://stellar-validator-seed"

  identityRotation:
    enabled: true
    interval: "720h"          # 30-day rotation policy
    secretStoreRef:
      name: validator-secrets
      kind: SecretStore
    remoteKey: "stellar/validators/prod-validator"
    # Stagger rotation across the cluster
    staggerInterval: "10m"
    maxUnavailable: 1
    # Propagate rotated keys to Horizon ingestion workers
    horizonPropagation:
      enabled: true
      databaseSecretRef: "horizon-db-credentials"
```

The `interval` field defines the rotation cadence (30 days in the example). The controller schedules rotations so that no more than `maxUnavailable` nodes are rotating at any time, waiting `staggerInterval` between each node.

#### Staggered Multi-Node Behavior

Key rotation must be staggered across a multi-node cluster to ensure quorum thresholds are never jeopardized during the restart sequence. The controller:

- Rotates one node at a time, waiting `staggerInterval` between nodes.
- Never rotates a node if doing so would drop the number of healthy validators below the quorum threshold.
- Verifies ledger advancement after each node rejoins before proceeding to the next.
- Aborts the rotation and rolls back if ledger advancement halts.

For a 5-node testnet with a quorum threshold of 3, the controller rotates nodes sequentially, keeping at least 4 nodes online at all times.

#### Quorum Safety Constraints

The controller enforces the following constraints before and during rotation:

- The number of simultaneously rotating nodes must never exceed `maxUnavailable`.
- The remaining healthy validators must always satisfy the configured quorum threshold.
- A node is only un-peered after its replacement identity is confirmed available in the external store.
- If ledger advancement stalls for longer than the configured timeout, the rotation is rolled back to the previous identity.

#### Horizon Database Key Propagation

When a validator has attached Horizon ingestion workers, the rotated public key must be reflected in the Horizon database. With `horizonPropagation.enabled`, the controller updates the ingestion worker records automatically after a successful rotation, ensuring ingestion continues against the correct validator identity.

#### Validation

Trigger an automated 30-day rotation policy across a 5-node testnet and confirm all nodes successfully migrate identities without halting ledger advancement:

```bash
# Simulate a rotation cycle
kubectl annotate stellarvalidator prod-validator -n stellar-prod \
  stellar.k8s.io/rotate-now="$(date +%s)" --overwrite

# Watch rotation progress
kubectl get stellarvalidators -n stellar-prod -w
kubectl logs -n stellar-prod -l app=stellar-operator -f | grep rotation
```

Confirm ledger advancement continues throughout:

```promql
stellar_core_ledger_age_seconds
```

#### Security Review Checklist

A PR introducing or modifying rotation must include a comprehensive security review of the transit pathways between the external secret store and pod memory. Verify:

- [ ] Secret transit uses TLS/mTLS between the external store, ESO, and the API server.
- [ ] Seeds are never written to logs, events, or ConfigMaps in plaintext.
- [ ] Seeds are mounted as in-memory volumes (`emptyDir: { medium: Memory }`) and never persisted to disk.
- [ ] The validator service account has least-privilege access to the secret store path only.
- [ ] ESO `SecretStore` credentials are short-lived and rotated independently.
- [ ] Pod memory containing the seed is protected by `readOnlyRootFilesystem` and dropped capabilities.
- [ ] Rotation audit events are recorded without exposing key material.
- [ ] Rollback paths do not retain stale seeds beyond the rotation window.

### Quorum Configuration

Configure trusted validators for consensus:

```yaml
spec:
  config:
    quorumSet:
      threshold: 3
      validators:
        - "VALIDATOR_PUBLIC_KEY_1"
        - "VALIDATOR_PUBLIC_KEY_2"
        - "VALIDATOR_PUBLIC_KEY_3"
        - "$self"
      innerQuorumSets:
        - threshold: 2
          validators:
            - "VALIDATOR_PUBLIC_KEY_4"
            - "VALIDATOR_PUBLIC_KEY_5"
```

### Database Configuration

#### External PostgreSQL

```yaml
spec:
  config:
    databaseUrl: "postgresql://user:password@postgres.example.com:5432/stellar"
```

#### Embedded SQLite (Not Recommended for Production)

```yaml
spec:
  config:
    databaseUrl: "sqlite3:///data/stellar.db"
```

## Monitoring

### Prometheus Metrics

Validator exposes metrics on port 11626:

```yaml
spec:
  monitoring:
    enabled: true
    serviceMonitor: true
```

Key metrics to monitor:

- `stellar_core_ledger_age_seconds` - Time since last ledger close
- `stellar_core_peer_connections` - Number of connected peers
- `stellar_core_sync_state` - Synchronization status
- `stellar_core_database_size_bytes` - Database size

### Grafana Dashboard

Import the Stellar validator dashboard:

```bash
kubectl apply -f https://raw.githubusercontent.com/OtowoOrg/Stellar-K8s/main/monitoring/grafana-dashboards/validator.json
```

Access Grafana:

```bash
kubectl port-forward -n monitoring svc/grafana 3000:80
```

Navigate to http://localhost:3000 and import the dashboard.

### Alerts

Configure PrometheusRule for critical alerts:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: stellar-validator-alerts
  namespace: stellar-prod
spec:
  groups:
    - name: stellar-validator
      interval: 30s
      rules:
        - alert: ValidatorNotSynced
          expr: stellar_core_ledger_age_seconds > 60
          for: 5m
          labels:
            severity: critical
          annotations:
            summary: "Validator {{ $labels.pod }} is not synced"
            description: "Ledger age is {{ $value }} seconds"
            
        - alert: LowPeerCount
          expr: stellar_core_peer_connections < 5
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Low peer connections on {{ $labels.pod }}"
            description: "Only {{ $value }} peers connected"
```

## High Availability

### Multi-Replica Deployment

```yaml
spec:
  replicas: 3
  
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - labelSelector:
            matchLabels:
              app: validator
          topologyKey: failure-domain.beta.kubernetes.io/zone
```

### Pod Disruption Budget

```yaml
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: validator-pdb
  namespace: stellar-prod
spec:
  minAvailable: 2
  selector:
    matchLabels:
      app: prod-validator
```

## Security Hardening

### Network Policies

Restrict network access:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: validator-network-policy
  namespace: stellar-prod
spec:
  podSelector:
    matchLabels:
      app: prod-validator
  policyTypes:
    - Ingress
    - Egress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              name: monitoring
      ports:
        - protocol: TCP
          port: 11626  # Metrics
    - from:
        - ipBlock:
            cidr: 0.0.0.0/0
      ports:
        - protocol: TCP
          port: 11625  # Peer connections
  egress:
    - to:
        - ipBlock:
            cidr: 0.0.0.0/0
      ports:
        - protocol: TCP
          port: 11625  # Peer connections
        - protocol: TCP
          port: 443  # HTTPS (history archives)
```

### Pod Security Standards

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: validator-pod
spec:
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    fsGroup: 10001
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: stellar-core
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop:
            - ALL
        readOnlyRootFilesystem: true
```

## Troubleshooting

### Check Validator Status

```bash
kubectl get stellarvalidators -n stellar-prod
kubectl describe stellarvalidator prod-validator -n stellar-prod
```

### View Logs

```bash
kubectl logs -n stellar-prod prod-validator-0 -f
```

### Common Issues

See the [Troubleshooting Guide](../troubleshooting/common-issues.md) for solutions to common validator problems.

## Next Steps

- [Configure Horizon API](horizon.md) to query validator data
- Set up backup and restore
- Optimize performance

!!! info "Production Checklist"
    - [x] Quorum set configured with trusted validators
    - [x] High availability with multiple replicas
    - [x] Monitoring and alerting enabled
    - [x] Security hardening applied
    - [x] Backup strategy implemented
    - [x] Network policies configured
    - [x] Resource limits appropriate for load
