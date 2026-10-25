# Finalizer Cleanup Recovery Controller

**Component:** `controller/src/finalizers/`  
**Status:** Implemented (Issue #46)  
**Complexity:** High (200 points)

---

## Overview

When the Stellar-K8s controller process is interrupted mid-deletion — e.g. a
pod crash, OOM kill, rolling upgrade, or node failure — a `StellarNode`
resource can become permanently stuck in the `Terminating` state.  The resource
has a non-zero `deletionTimestamp` but the finalizer
`stellarnode.stellar.org/finalizer` is never removed because the controller
that would remove it is no longer running.

This document describes the **Finalizer Cleanup Recovery Controller**: a
background garbage-collection reconciler that safely resolves this condition.

---

## Problem Statement

### Kubernetes Finalizer Lifecycle

1. User issues `kubectl delete stellarnode my-validator`.
2. Kubernetes sets `metadata.deletionTimestamp` on the resource.
3. The controller sees the timestamp, runs cleanup logic (detaches EBS/GCP PD
   volumes, releases cloud resources), and then removes the finalizer.
4. Only after the finalizer list is empty does Kubernetes permanently delete
   the resource.

### Failure Scenario

If the controller process is killed between steps 2 and 4, the resource
remains in `Terminating` indefinitely.  Manual intervention is required to
remove the stuck finalizer, which risks:

- **Data leaks** if the finalizer is stripped before the volume detaches.
- **Cloud cost accumulation** from unreleased volumes.
- **Operational blindness** as the resource stays visible but unusable.

---

## Design

### Architecture Diagram

```
┌───────────────────────────────────────────────────────────┐
│                   Operator Process                         │
│                                                           │
│  ┌──────────────────────────────────────────────────────┐ │
│  │         FinalizerCleanupReconciler (loop/30s)         │ │
│  │                                                      │ │
│  │  1. list_terminating_nodes()  ──► Kubernetes API     │ │
│  │           │                                          │ │
│  │           ▼                                          │ │
│  │  2. is_volume_detached()  ──────► Cloud Provider API │ │
│  │           │                         (AWS EC2 /       │ │
│  │           │                          GCP Compute)    │ │
│  │           ▼ (only if Ok(true))                       │ │
│  │  3. remove_finalizer()  ────────► Kubernetes API     │ │
│  └──────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────┘
```

### Module Structure

```
controller/src/finalizers/
├── mod.rs            # Module declaration and top-level docs
├── cloud_verify.rs   # CloudVolumeVerifier trait + AWS/GCP implementations
└── cleanup.rs        # FinalizerCleanupReconciler + KubeClient trait
```

---

## Safety Verification Steps

The core safety invariant is:

> **A finalizer is never removed unless the cloud provider API explicitly
> confirms that the underlying volume has zero active attachments.**

### Step 1 — Detect terminating resources

`KubeClient::list_terminating_nodes()` queries the Kubernetes API for all
`StellarNode` resources with a non-zero `deletionTimestamp` and extracts:

- `namespace`, `name` (for the Kubernetes patch)
- `cloud_provider` annotation (→ selects the verifier)
- `region_or_zone` annotation (→ passed to the cloud API)
- `volume_id` annotation on the associated PVC (→ the volume to check)

If the annotation is missing or the provider is unrecognised, the resource is
skipped and no finalizer is removed.

### Step 2 — Cloud provider verification

`CloudVolumeVerifier::is_volume_detached(volume_id, region_or_zone)` is called
and **must return `Ok(true)`** before any patch is issued.

| Return value     | Action                                          |
|------------------|-------------------------------------------------|
| `Ok(true)`       | Volume confirmed detached → proceed to step 3  |
| `Ok(false)`      | Volume still attached → log info, retry later  |
| `Err(_)`         | API error → log warning, retry later            |

**AWS EBS verification** (`AwsEbsVerifier`):  
Calls the EC2 `DescribeVolumes` API and parses the XML response.  The volume
is considered detached only when the `<attachmentSet>` is empty or all
attachment entries have `<status>detached</status>`.

**GCP Persistent Disk verification** (`GcpPdVerifier`):  
Calls the Compute Engine `disks.get` REST API and inspects the `users` field.
An empty `users` array (or its absence) confirms no VM instances are using
the disk.  A 404 response means the disk no longer exists and is treated as
fully detached.

### Step 3 — Safe finalizer removal

Only after step 2 returns `Ok(true)`, the reconciler calls
`KubeClient::remove_finalizer()` which issues a Kubernetes strategic-merge
patch:

```json
{
  "metadata": {
    "finalizers": ["...remaining finalizers, without stellarnode.stellar.org/finalizer..."]
  }
}
```

This removes only the Stellar finalizer, preserving any other finalizers set
by third-party operators.

### Retry behaviour

The reconciler runs every **30 seconds** (configurable via
`DEFAULT_GC_INTERVAL`).  On each cycle it re-evaluates all `Terminating`
resources.  A transient cloud API error or still-attached volume causes the
cycle to log and continue; no manual intervention is required.

---

## Crash-Recovery Validation

The issue requires simulating a controller crash mid-deletion of a 500 GiB
validator node and verifying recovery.  The `crash_recovery_simulation` test in
`cleanup.rs` covers this scenario end-to-end:

```rust
// First pass: controller restarted, volume is still detaching
let verifier_attached = MockCloudVerifier::attached();
let results = reconciler.run_once().await;
assert_eq!(results[0].1, ReconcileOutcome::VolumeStillAttached);
// → finalizer NOT removed

// Second pass: volume has fully detached
let verifier_detached = MockCloudVerifier::detached();
let results = reconciler.run_once().await;
assert_eq!(results[0].1, ReconcileOutcome::FinalizerRemoved);
// → finalizer removed, resource can be garbage-collected
```

---

## Mock Cloud API Tests

Both `cloud_verify.rs` and `cleanup.rs` include comprehensive `#[cfg(test)]`
suites covering:

### `cloud_verify.rs`

| Test | Coverage |
|------|----------|
| `aws_xml_parser_detects_attached_volume` | XML with `<status>attached</status>` is blocked |
| `aws_xml_parser_detects_attaching_volume` | XML with `<status>attaching</status>` is blocked |
| `aws_xml_parser_accepts_detached_volume` | Empty `<attachmentSet>` is allowed |
| `aws_xml_parser_accepts_detached_status_label` | `<status>detached</status>` is allowed |
| `mock_detached_returns_true` | `MockCloudVerifier::detached()` returns `true` |
| `mock_attached_returns_false` | `MockCloudVerifier::attached()` returns `false` |
| `mock_error_propagates` | `MockCloudVerifier::error()` propagates error message |
| `aws_verifier_rejects_invalid_volume_id` | Non-`vol-` prefix is rejected |
| `aws_verifier_rejects_short_volume_id` | `vol-abc` (< 12 chars) is rejected |
| `gcp_verifier_rejects_empty_disk_name` | Empty disk name is rejected |
| `unknown_provider_always_blocks` | Unrecognised provider always returns `false` |
| `factory_returns_aws_verifier_for_aws_labels` | Provider factory dispatches correctly |
| `factory_returns_gcp_verifier_for_gcp_labels` | Provider factory dispatches correctly |

### `cleanup.rs`

| Test | Coverage |
|------|----------|
| `finalizer_removed_when_volume_is_detached` | Happy path end-to-end |
| `finalizer_not_removed_when_volume_still_attached` | Safety: no premature removal |
| `finalizer_not_removed_on_cloud_api_error` | Safety: error handling |
| `verification_error_returned_on_kube_patch_failure` | Kube API failure path |
| `empty_list_returns_no_results` | Empty cluster state |
| `handles_multiple_orphaned_resources` | Bulk reconciliation |
| `crash_recovery_simulation` | Full crash-recovery scenario (two-pass) |
| `strip_stellar_finalizer_removes_only_stellar_entry` | Patch helper correctness |
| `strip_stellar_finalizer_noop_when_not_present` | Idempotency |
| `build_finalizer_remove_patch_produces_expected_json` | Patch JSON correctness |

---

## Cloud Provider Annotations

The reconciler reads the following annotations from `StellarNode` resources
and their associated PVCs:

| Annotation | Required | Values | Description |
|-----------|----------|--------|-------------|
| `stellar.org/cloud-provider` | Yes | `aws`, `aws-ebs`, `gcp`, `gcp-pd` | Selects the cloud verifier |
| `stellar.org/cloud-region` | Yes | e.g. `us-east-1`, `us-central1-a` | Region/zone for API calls |
| `stellar.org/volume-id` | Yes | e.g. `vol-0abc1234`, `my-disk` | Cloud volume identifier |

---

## Integration with the Operator

The `FinalizerCleanupReconciler` is designed to be spawned as a background
tokio task during operator startup:

```rust
use std::sync::Arc;
use controller::finalizers::cleanup::{FinalizerCleanupReconciler, DEFAULT_GC_INTERVAL};
use controller::finalizers::cloud_verify::AwsEbsVerifier;

// Spawn the GC reconciler as a background task
let kube = Arc::new(ProductionKubeClient::new(kube_client));
let verifier = Arc::new(AwsEbsVerifier::new());
let reconciler = Arc::new(FinalizerCleanupReconciler::new(kube, verifier));
tokio::spawn(reconciler.run(DEFAULT_GC_INTERVAL));
```

---

## Related Documents

- [`docs/adr/0003-kube-rs-finalizers.md`](../adr/0003-kube-rs-finalizers.md) — ADR documenting the finalizer strategy.
- [`docs/operations/pvc-troubleshooting.md`](../operations/pvc-troubleshooting.md) — Runbook for PVC stuck in Terminating.
- [`docs/storage-repair.md`](../storage-repair.md) — Manual volume repair procedures.
- [GitHub Issue #46](https://github.com/agnesnaomiolim-cloud/Stellar-forge/issues/46) — Original feature request.
