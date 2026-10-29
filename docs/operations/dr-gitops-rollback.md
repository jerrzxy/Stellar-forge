# Disaster Recovery: GitOps State Rollback & Forced Recovery

**Audience:** SRE / on-call engineers recovering a Tier-1 validator cluster from a
corrupted GitOps deployment.
**Scope:** Forcibly reverting a live `StellarNode` cluster to a known-good
cryptographic state hash via GitOps, recovering orphaned pods and displaced
volumes, and — only as a final failsafe — restoring the operator host's `etcd`
from a snapshot.

**Prerequisites:**
- `kubectl` cluster-admin access and `argocd` CLI access to the affected
  environment (see [gitops-argocd.md](gitops-argocd.md)).
- Write access to the GitOps configuration repository
  (`examples/gitops/` overlays).
- A recent, verified `etcd` snapshot (see
  [../../examples/dr/etcd-snapshot.sh](../../examples/dr/etcd-snapshot.sh)).

> **WARNING:** This runbook is intentionally destructive. A rapid Git revert
> races the Kubernetes control plane: pruning, orphaned pods, and volume
> detach/attach all run concurrently. Execute the steps **in order**, announce
> each phase in the incident channel, and never start `etcd` restoration while
> the API server is still healthy. When in doubt, prefer the GitOps revert —
> `etcd` restoration is the last resort, not a shortcut.

## Table of Contents

1. [When to Use This Runbook](#when-to-use-this-runbook)
2. [Isolate the Corrupting Commit](#isolate-the-corrupting-commit)
3. [Immediate Rollback via `git revert`](#immediate-rollback-via-git-revert)
4. [Orphaned Pods and Hung Resources](#orphaned-pods-and-hung-resources)
5. [PVC Detach/Reattach After Storage-Class Corruption](#pvc-detachreattach-after-storage-class-corruption)
6. [`etcd` Snapshot Restoration Matrix (Final Failsafe)](#etcd-snapshot-restoration-matrix-final-failsafe)
7. [Validation and the 5-Minute Recovery SLA](#validation-and-the-5-minute-recovery-sla)
8. [Post-Incident Requirements](#post-incident-requirements)

---

## When to Use This Runbook

Use this procedure when a malformed configuration merge (bad resource limits,
a wrong image tag, a corrupted storage-class reference, a broken Kustomize
patch) has corrupted a Tier-1 validator deployment and automated self-healing
can no longer converge:

- The ArgoCD Application is stuck `OutOfSync`/`Progressing` with repeated
  `SyncFailed` errors.
- `StellarNode` resources are crash-looping, stuck in `Pending`, or attached
  to volumes that no longer exist.
- The live cluster state no longer matches **any** commit in Git history
  (someone force-edited resources, or a mutating webhook corrupted objects).

If only the workload is unhealthy but Git state is clean, prefer the ordinary
[Disaster Recovery Runbook](disaster-recovery.md) or
[split-brain recovery](split-brain-recovery.md) instead.

---

## Isolate the Corrupting Commit

Never roll back "a few commits" on intuition. Identify the exact corrupting
merge and confirm the exact known-good commit hash you are reverting to.

```bash
ARGO_APP="stellar-testnet"   # stellar-mainnet / stellar-futurenet as appropriate

# 1. Application history as ArgoCD sees it (newest last).
argocd app history "$ARGO_APP" --output json | jq '.[-3:]'

# 2. What is live right now vs what Git says should be live.
argocd app diff "$ARGO_APP" | head -100

# 3. The last N merges to the GitOps overlay that feeds this Application.
git -C gitops-repo log --oneline -n 10 -- examples/gitops/overlays/testnet

# 4. Verify the candidate commit rendered a healthy overlay in CI at merge time.
git -C gitops-repo show <known-good-sha> --stat
```

Record both hashes in the incident channel before proceeding:

```text
BAD_SHA=<corrupting merge commit>
GOOD_SHA=<last known-good commit>   # must be a full 40-char hash
REVERT_MSG="revert: <original subject> (corrupts tier-1 validator config)"
```

> **Note:** ArgoCD tracks live state as a *rendered manifest*, not a Git hash.
> The state hash you are racing back to is the output of
> `kustomize build examples/gitops/overlays/<env>` at `GOOD_SHA`. Capture it
> now — you will verify against it in step 7:
>
> ```bash
> git -C gitops-repo show "$GOOD_SHA" >/dev/null  # object must exist locally
> git -C gitops-repo worktree add /tmp/good "$GOOD_SHA"
> kustomize build /tmp/good/examples/gitops/overlays/testnet > /tmp/known-good-state.yaml
> sha256sum /tmp/known-good-state.yaml
> ```

---

## Immediate Rollback via `git revert`

A `git revert` creates a *new* commit that inverts the corrupting diff. This
is mandatory: ArgoCD's automated sync (poll every 3 minutes, or webhook) will
treat the revert as a normal change and drive the cluster back to `GOOD_SHA`'s
rendered state without any imperative cluster edits.

```bash
cd gitops-repo
git checkout main
git pull --ff-only origin main

# 1. Revert the corrupting commit (add -m 1 for merge commits).
git revert --no-edit "$BAD_SHA"

# 2. Review the inverted diff — it must be the exact inverse of the bad change.
git diff HEAD~1..HEAD

# 3. Push. ArgoCD picks this up via webhook immediately, else within ~3 min.
git push origin main
```

Force an immediate sync instead of waiting for the poll interval:

```bash
argocd app wait "$ARGO_APP" --health --timeout 120
argocd app history "$ARGO_APP" | tail -3
```

If the Application's automated sync is disabled or wedged, drive it
explicitly, then re-enable self-heal:

```bash
argocd app sync "$ARGO_APP" --prune --timeout 300
argocd app wait "$ARGO_APP" --health --sync --timeout 300

# Self-heal must be on again before you walk away from the incident.
argocd app set "$ARGO_APP" --sync-policy automated.selfHeal=true
```

**ArgoCD-side rollback (break-glass only):** if the Git remote itself is
unreachable (GitHub outage, credential loss) but ArgoCD's repo-server still
has history, roll back to a known-good sync *operation ID* and treat Git as
the follow-up action:

```bash
argocd app history "$ARGO_APP"            # note the ID of the good sync
argocd app rollback "$ARGO_APP" <id>      # re-applies that rendered state
argocd app wait "$ARGO_APP" --health --timeout 300
# REQUIRED follow-up: restore Git as the source of truth, or self-heal will
# re-apply the corrupting state on the next reconciliation.
```

> **WARNING:** `argocd app rollback` without a corresponding Git revert is a
> temporary tourniquet only. Automated self-heal **will** overwrite the
> rollback with the corrupting commit the next time it reconciles. Git is
> always the source of truth in this architecture.

---

## Orphaned Pods and Hung Resources

A rapid revert with `prune: true` and `PrunePropagationPolicy=foreground`
deletes and recreates resources concurrently. During the window between prune
and re-apply, **orphaned pods** — pods whose owner references were pruned but
which are still running — continue serving traffic with the *corrupted*
configuration. They are the primary danger of a fast Git revert: they keep the
old (bad) StatefulSet ordinal alive, they hold PersistentVolume attachments,
and they keep the corrupted configuration in DNS while the new pod for the
same ordinal cannot start.

### Detect orphaned and hung resources

```bash
# Pods whose ownerRef no longer resolves (orphaned by a prune).
kubectl get pods -n stellar-testnet -o json \
  | jq -r '.items[] | select(.metadata.ownerReferences == null) | .metadata.name'

# Resources stuck terminating longer than 90 seconds.
kubectl get pods -n stellar-testnet \
  --field-selector status.phase!=Running,status.phase!=Succeeded \
  -o custom-columns=NAME:.metadata.name,STATUS:.status.phase,AGE:.metadata.creationTimestamp

# StatefulSet ordinals that failed to recreate.
kubectl get statefulset -n stellar-testnet -o wide
kubectl get events -n stellar-testnet --sort-by=.lastTimestamp | tail -30
```

### Force-delete hung resources

> **WARNING:** Force deletion (`--force --grace-period=0`) removes the pod
> object from the API immediately **without** waiting for graceful shutdown.
> For validators this risks a double-process on the same volume if the node
> process is still alive at the kubelet level. Only force-delete after you
> have verified the node is not making progress, and only with the incident
> commander's approval. Never force-delete every pod at once — one ordinal at
> a time, re-checking health between each.

```bash
NS=stellar-testnet
STS=stellar-validator   # StatefulSet name for the affected tier

# 1. Confirm the ordinal is truly hung (not merely reconciling).
kubectl -n "$NS" describe pod "${STS}-2" | tail -20

# 2. Force-delete the single hung pod. kubelet detaches volumes on the old node.
kubectl -n "$NS" delete pod "${STS}-2" --force --grace-period=0

# 3. Watch the StatefulSet recreate the ordinal with the reverted config.
kubectl -n "$NS" rollout status statefulset/"${STS}-2" --timeout=180s

# 4. If the pod object is stuck Terminating on finalizers, remove the
#    Kubernetes-service finalizer ONLY after step 2's volume detach is done.
kubectl -n "$NS" get pod "${STS}-2" -o json \
  | jq '.metadata.finalizers'
# Example output: ["kubernetes.io/pv-protection"]
kubectl -n "$NS" patch pod "${STS}-2" --type=merge \
  -p '{"metadata":{"finalizers":null}}'
```

Repeat per hung ordinal. Between each deletion, confirm the reverted pod for
that ordinal becomes `Ready` before moving on — this keeps quorum loss bounded
to a single validator at a time.

---

## PVC Detach/Reattach After Storage-Class Corruption

A corrupted merge often touches `storageClassName` on a
`volumeClaimTemplate` or on a standalone PVC. Two immutable facts drive the
whole recovery procedure:

1. A PVC's `storageClassName` **cannot be mutated** after creation.
2. StatefulSet `volumeClaimTemplates` are immutable — they cannot be edited in
   place, only deleted and recreated with the StatefulSet.

### Diagnose the displacement

```bash
NS=stellar-testnet

# Which PVCs are bound to the wrong storage class?
kubectl -n "$NS" get pvc -o custom-columns=\
NAME:.metadata.name,SC:.spec.storageClassName,STATUS:.status.phase,CLAIM:.spec.volumeName

# Are pods stuck ContainerCreating with "volume still attached to another node"?
kubectl -n "$NS" describe pod stellar-validator-0 | grep -A5 Events

# Which node currently holds the attachment?
kubectl get pv "$PV_NAME" -o jsonpath='{.spec.claimRef.namespace}{"\n"}'
kubectl get volumeattachments | grep "$PV_NAME"
```

### Detach and reattach dynamically

```bash
NS=stellar-testnet
STS=stellar-validator
GOOD_SC="stellar-data-ssd"   # the storage class from GOOD_SHA

# 1. Snapshot the displaced volume BEFORE touching anything (data-insurance step).
#    (Uses the VolumeSnapshot API; see docs/volume-snapshots.md.)
kubectl -n "$NS" apply -f - <<EOF
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: ${STS}-0-rescue
spec:
  volumeSnapshotClassName: csi-snapclass
  source:
    persistentVolumeClaimName: ${STS}-0
EOF
kubectl -n "$NS" get volumesnapshot "${STS}-0-rescue" -w   # wait until READYTOUSE=true

# 2. Scale the StatefulSet to zero so every consumer releases its volumes.
kubectl -n "$NS" scale statefulset "$STS" --replicas=0
kubectl -n "$NS" wait --for=delete pod -l app.kubernetes.io/name="$STS" --timeout=180s

# 3. Confirm every volume attachment from the old nodes is gone.
kubectl get volumeattachments -o json \
  | jq -r '.items[] | select(.status.attached==true) | .metadata.name'
# Any remaining attachment for our PVs: detach by deleting the VolumeAttachment
# object ONLY if the node is confirmed dead/reattached (break-glass).
# kubectl delete volumeattachment <name>

# 4. Delete the mis-provisioned PVCs (they are unrecoverable in place).
kubectl -n "$NS" delete pvc "${STS}-0" "${STS}-1" "${STS}-2"

# 5. Fix the root cause in Git — NOT with kubectl edit.
#    Correct storageClassName in the overlay patch, commit, push:
#      examples/gitops/overlays/testnet/validator-patch.yaml
git add examples/gitops/overlays/testnet/validator-patch.yaml
git commit -m "fix(storage): restore correct storageClassName for validator PVCs"
git push origin main

# 6. Scale back up; the operator provisions fresh PVCs with the good class,
#    and the StatefulSet reattaches the recovered data.
kubectl -n "$NS" scale statefulset "$STS" --replicas=3
kubectl -n "$NS" rollout status statefulset/"$STS" --timeout=300s
```

If the VolumeSnapshot API is unavailable in the cluster, fall back to
`examples/dr/restore-script.sh` with a backup taken by the scheduled snapshot
job — see [pvc-troubleshooting.md](pvc-troubleshooting.md) and
[dr-failover.md](dr-failover.md).

---

## `etcd` Snapshot Restoration Matrix (Final Failsafe)

Use this **only** when the corruption reached the operator host's control
plane — for example, a runaway reconcile or admission-webhook crash loop has
exhausted `etcd` quota, or the API server itself is serving from a corrupted
store. Restoring `etcd` rewinds **every** object in the cluster, GitOps state
included; the GitOps revert from step 3 must already be pushed so ArgoCD can
re-converge afterwards.

The companion script
[../../examples/dr/etcd-snapshot.sh](../../examples/dr/etcd-snapshot.sh)
automates every row of this matrix with checksum verification and a
confirmation gate. The raw commands are listed here for situations where the
script itself cannot run.

**Defaults used below:** control-plane host layout from the Kubernetes static
pod pattern (`/etc/kubernetes/manifests/etcd.yaml`), data directory
`/var/lib/etcd`, endpoints `https://127.0.0.1:2379`, PKI under
`/etc/kubernetes/pki/etcd/`. Adjust to your bootstrap layout.

| Scenario | Command(s) |
|---|---|
| Take a snapshot (script) | `etcd-snapshot.sh save --name pre-incident-$(date +%s)` |
| Take a snapshot (raw) | `ETCDCTL_API=3 etcdctl --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key snapshot save /var/lib/etcd-snapshots/pre-incident.db` |
| Inspect snapshot health (script) | `etcd-snapshot.sh status --snapshot /var/lib/etcd-snapshots/pre-incident.db` |
| Inspect snapshot health (raw) | `etcdutl snapshot status /var/lib/etcd-snapshots/pre-incident.db -w table` |
| Cluster member map | `etcdctl member list -w table` |
| Member & endpoint health | `etcdctl endpoint health -w table --cluster` |
| Quota / DB size check | `etcdctl endpoint status -w table --cluster` |
| Restore a **single** failed member (in-place) | `etcd-snapshot.sh restore --snapshot <db> --data-dir /var/lib/etcd --yes-i-really-mean-it` |
| Restore a single member (raw, etcdutl) | `etcdutl snapshot restore /var/lib/etcd-snapshots/pre-incident.db --data-dir /var/lib/etcd-restored --skip-hash-check=false` |
| Restore a single member (raw, legacy etcdctl) | `ETCDCTL_API=3 etcdctl snapshot restore /var/lib/etcd-snapshots/pre-incident.db --data-dir /var/lib/etcd-restored` |
| Rebuild a **full cluster** from one snapshot | `etcdutl snapshot restore <db> --data-dir /var/lib/etcd --initial-cluster etcd0=https://cp0:2380,etcd1=https://cp1:2380,etcd2=https://cp2:2380 --initial-cluster-token etcd-cluster-restore --name etcd0` |
| Wipe-and-restore all members | run the full-cluster row **on every** control-plane host with the same `--initial-cluster-token`, then restart the kubelet on each host |
| Verify post-restore member health | `etcdctl endpoint health -w table --cluster` |

### Restore procedure (single member)

```bash
# 1. Stop the etcd static pod by moving the manifest aside (kubelet stops it).
sudo mv /etc/kubernetes/manifests/etcd.yaml /root/etcd.yaml.halted

# 2. Wait for the process to exit, then move (never delete) the corrupt data.
while pgrep -x etcd >/dev/null; do sleep 2; done
sudo mv /var/lib/etcd "/var/lib/etcd.corrupt-$(date +%s)"

# 3. Restore from the verified snapshot.
sudo examples/dr/etcd-snapshot.sh restore \
  --snapshot /var/lib/etcd-snapshots/pre-incident.db \
  --data-dir /var/lib/etcd \
  --yes-i-really-mean-it

# 4. Bring etcd back and verify quorum before touching anything else.
sudo mv /root/etcd.yaml.halted /etc/kubernetes/manifests/etcd.yaml
sleep 10
etcdctl endpoint health -w table --cluster
kubectl get nodes
```

> **WARNING:** Never restore different members from *different* snapshots —
> divergent snapshots permanently split the member set and require a full
> cluster rebuild. Take one snapshot, distribute the same file to every
> member, and record its SHA-256 in the incident log.

---

## Validation and the 5-Minute Recovery SLA

The rollback is complete when the cluster serves the known-good state hash
again. Budget the 5-minute SLA as follows; if any step exceeds its budget,
escalate to the incident commander rather than improvising.

| t+ | Check | Command | Budget |
|---|---|---|---|
| t+0:00 | Revert pushed; ArgoCD sync triggered | `git push` + `argocd app history` | 60 s |
| t+1:00 | Application reports `Synced` | `argocd app wait "$ARGO_APP" --sync --timeout 120` | 60 s |
| t+2:00 | Live state hash equals `GOOD_SHA` hash | `kubectl get appstate -n "$NS" -o yaml \| sha256sum` vs `/tmp/known-good-state.yaml` | 60 s |
| t+3:00 | All `StellarNode` resources `Ready`, no orphans | `kubectl get stellarnode -n "$NS"` + orphan scan from step 4 | 60 s |
| t+4:00 | SCP consensus restored (3+ validators voting) | `kubectl logs -n "$NS" sts/stellar-validator-0 \| grep -i quorum` | 60 s |
| t+5:00 | Horizon/API health endpoint green | `curl -sf https://horizon.stellar-testnet.internal/health` | 60 s |

```bash
# One-shot verification helper (run from the operator bastion).
ARGO_APP="stellar-testnet"; NS="stellar-testnet"
argocd app wait "$ARGO_APP" --health --sync --timeout 300 \
  && kubectl -n "$NS" get stellarnode \
  && kubectl -n "$NS" get pods --field-selector status.phase!=Running \
  && echo "ROLLBACK VERIFIED — network recovered within SLA"
```

Any check that fails at t+5:00 converts the incident from "GitOps rollback"
to "full failover": continue with [dr-failover.md](dr-failover.md) and the
[backup and disaster recovery runbook](../backup-disaster-recovery-runbook.md).

---

## Post-Incident Requirements

1. **Root-cause the guardrail gap.** The corrupting change passed CI. Open an
   issue for the missing schema validation, admission-policy, or Helm test and
   link it from the incident record.
2. **Re-take baseline snapshots.** Every snapshot taken before the incident is
   now stale. Run `etcd-snapshot.sh save --name post-incident-baseline` on
   each control-plane host and refresh the off-box copy.
3. **Verify backup freshness** with the
   [backup verification guide](backup-verification.md).
4. **Record the measured SLA** (times from the table above) in the incident
   post-mortem; update this runbook if the budgets proved unrealistic.
5. **Never merge the fix forward.** If a hotfix was applied to Git during the
   incident, ensure `main` reflects the reverted-and-fixed state before
   re-enabling automated sync on lower environments.

## See Also

- [GitOps Deployment Architecture via ArgoCD](gitops-argocd.md)
- [Disaster Recovery: Failover & Snapshot Restoration](dr-failover.md)
- [Operations Disaster Recovery Overview](disaster-recovery.md)
- [PVC Troubleshooting](pvc-troubleshooting.md)
- [Backup and Disaster Recovery Runbook](../backup-disaster-recovery-runbook.md)
- [examples/dr/etcd-snapshot.sh](../../examples/dr/etcd-snapshot.sh)
- [examples/dr/restore-script.sh](../../examples/dr/restore-script.sh)
