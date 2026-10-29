# Helm Upgrades Guide for Stellar-K8s

This guide provides operational procedures, pre-upgrade verification checks, rollback strategies, and troubleshooting steps for upgrading Helm releases across Stellar-K8s clusters.

## Table of Contents

- [Overview](#overview)
- [Prerequisites & Pre-Flight Checks](#prerequisites--pre-flight-checks)
- [Cluster Setup Verification](#cluster-setup-verification)
- [Upgrade Procedures](#upgrade-procedures)
  - [Standard Upgrade](#standard-upgrade)
  - [Dry-Run and Validation](#dry-run-and-validation)
  - [Custom Values Profiles](#custom-values-profiles)
- [Rollback Strategies](#rollback-strategies)
  - [Automated Helm Rollback](#automated-helm-rollback)
  - [Manual Recovery](#manual-recovery)
- [Post-Upgrade Verification](#post-upgrade-verification)
- [Troubleshooting Common Issues](#troubleshooting-common-issues)
  - [Hook Execution Failures](#hook-execution-failures)
  - [CRD Schema Validation Mismatches](#crd-schema-validation-mismatches)
  - [Pod Eviction or Pending State](#pod-eviction-or-pending-state)
- [Related Documentation](#related-documentation)

---

## Overview

The Stellar-K8s operator and managed components are deployed and maintained via Helm charts located under `charts/stellar-operator`. Managing lifecycle upgrades requires careful coordination to ensure zero-downtime ledger processing, preserved quorum consensus, and backward-compatible CRD schemas.

Before applying upgrades in production, always review the [Cluster Setup Verification](#cluster-setup-verification) section and test changes in a staging environment.

---

## Prerequisites & Pre-Flight Checks

Before initiating any Helm release upgrade, verify the environment:

1. **Tooling & Access:**
   ```bash
   helm version --short
   kubectl version --client
   kubectl auth can-i update stellarnodes.stellar.org -n stellar-system
   ```

2. **Backup Release State:**
   Always snapshot current release values and CRD configurations:
   ```bash
   helm get values stellar-operator -n stellar-system > backup-values.yaml
   kubectl get crd stellarnodes.stellar.org -o yaml > backup-stellarnode-crd.yaml
   kubectl get stellarnodes -A -o yaml > backup-stellarnodes.yaml
   ```

3. **Check Node Synchronization:**
   Ensure active nodes are synced and healthy:
   ```bash
   kubectl get stellarnodes -A -o custom-columns=NAME:.metadata.name,NAMESPACE:.metadata.namespace,PHASE:.status.phase,SYNCED:.status.synced
   ```

---

## Cluster Setup Verification

Ensuring correct cluster topology and network configuration is critical before proceeding with any Helm upgrade. Follow these verification steps to ensure cluster components are prepared:

1. **Node and Namespace Readiness:**
   Confirm all Kubernetes worker nodes and system pods are running healthy:
   ```bash
   kubectl get nodes
   kubectl get pods -n stellar-system
   ```

2. **Storage and PVC Health:**
   Verify PersistentVolumeClaims attached to ledger storage are bound with no pending volume operations:
   ```bash
   kubectl get pvc -n stellar-system
   ```

3. **Multi-Cluster & Ingress Connectivity:**
   For multi-cluster or hybrid deployments, ensure cross-cluster trust anchors and service mesh endpoints are reachable. Review the [Troubleshooting Common Issues](#troubleshooting-common-issues) if connectivity check fails.

---

## Upgrade Procedures

### Standard Upgrade

To execute an in-place upgrade using the updated repository charts with pre-upgrade hooks enabled:

```bash
helm repo update
helm upgrade stellar-operator charts/stellar-operator \
  -n stellar-system \
  -f backup-values.yaml \
  --wait \
  --timeout 10m
```

### Dry-Run and Validation

Always perform a dry-run to preview rendered manifests and check for unexpected modifications:

```bash
helm upgrade stellar-operator charts/stellar-operator \
  -n stellar-system \
  -f backup-values.yaml \
  --dry-run \
  --debug
```

### Custom Values Profiles

For production clusters requiring high availability or disaster recovery overrides:

```bash
helm upgrade stellar-operator charts/stellar-operator \
  -n stellar-system \
  -f charts/stellar-operator/examples/values-production.yaml \
  --wait \
  --timeout 15m
```

---

## Rollback Strategies

If an upgrade encounters unexpected failure or ledger synchronization halts, follow these rollback procedures immediately.

### Automated Helm Rollback

Roll back directly to the previous healthy Helm release revision:

```bash
# View release history
helm history stellar-operator -n stellar-system

# Rollback to the previous revision
helm rollback stellar-operator -n stellar-system --wait
```

### Manual Recovery

If Helm rollback is blocked by hook timeouts or schema issues:

1. Restore previous CRD manifests:
   ```bash
   kubectl apply -f backup-stellarnode-crd.yaml
   ```
2. Re-apply saved release values:
   ```bash
   helm upgrade stellar-operator charts/stellar-operator \
     -n stellar-system \
     -f backup-values.yaml \
     --set hooks.preUpgrade.enabled=false \
     --force
   ```

---

## Post-Upgrade Verification

Once the Helm upgrade finishes, perform end-to-end verification:

1. **Run Helm Test Suite:**
   ```bash
   helm test stellar-operator -n stellar-system
   ```

2. **Check Operator Logs:**
   ```bash
   kubectl logs -n stellar-system -l app.kubernetes.io/name=stellar-operator --tail=100 -f
   ```

3. **Verify Ledger Progression:**
   Ensure validator nodes continue closing ledgers without interruption:
   ```bash
   kubectl get stellarnodes -n stellar-system -o custom-columns=NAME:.metadata.name,LEDGER:.status.ledgerSequence,PHASE:.status.phase
   ```

---

## Troubleshooting Common Issues

### Hook Execution Failures

If pre-upgrade jobs fail with timeouts:
- Inspect hook job logs: `kubectl logs -n stellar-system job/stellar-operator-pre-upgrade`
- Ensure the API server allows required RBAC operations during hook execution.

### CRD Schema Validation Mismatches

If resource validation fails:
- Review release notes for deprecated fields or schema breaking changes.
- Refer back to [Prerequisites & Pre-Flight Checks](#prerequisites--pre-flight-checks) to verify backed-up CRD specifications.

### Pod Eviction or Pending State

If operator or validator pods remain pending:
- Check cluster resources: `kubectl describe nodes | grep -A 5 "Allocated resources"`
- Consult [Cluster Setup Verification](#cluster-setup-verification) to ensure storage classes and PVCs remain bound.

---

## Related Documentation

- [Major Version Upgrade Guide](upgrade-guide.md)
- [Helm Release Drift Remediation Guide](helm-drift-remediation.md)
- [Blue-Green Upgrade Runbook](blue-green-upgrade.md)
- [Zero-Downtime Protocol Upgrade Runbook](protocol-upgrades.md)
- [Helm Chart Upgrade Guide](../../charts/stellar-operator/UPGRADE_GUIDE.md)
