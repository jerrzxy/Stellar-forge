#!/usr/bin/env bash
set -euo pipefail

# etcd-snapshot.sh - Save, inspect, and restore etcd snapshots for the
# Kubernetes control-plane etcd that backs a Stellar-K8s operator host.
#
# This is the final failsafe documented in
# docs/operations/dr-gitops-rollback.md. It is deliberately explicit: every
# restore verifies the snapshot checksum first, refuses to run while an etcd
# process is alive, and moves (never deletes) the previous data directory.
#
# Subcommands:
#   save     Take a consistent snapshot and write a .sha256 manifest.
#   status   Print health/summary information for a snapshot file.
#   restore  Restore a verified snapshot into a data directory (destructive).
#
# Restore requires --yes-i-really-mean-it. There is no default that makes
# this script restore on its own.

usage() {
  cat <<'EOF'
Usage:
  etcd-snapshot.sh save    [--name <file>] [connection options]
  etcd-snapshot.sh status  --snapshot <path>
  etcd-snapshot.sh restore --snapshot <path> --data-dir <dir> --yes-i-really-mean-it
                            [connection options]

Connection options (defaults match the kubeadm static-pod layout):
  --endpoints        etcd client endpoints   (default: https://127.0.0.1:2379)
  --cacert           CA certificate          (default: /etc/kubernetes/pki/etcd/ca.crt)
  --cert             client certificate      (default: /etc/kubernetes/pki/etcd/server.crt)
  --key              client key              (default: /etc/kubernetes/pki/etcd/server.key)
  --snapshot-dir     save destination dir    (default: /var/lib/etcd-snapshots)

Restore-only options:
  --data-dir                 target data directory (required)
  --initial-cluster          etcd initial cluster string (optional)
  --initial-cluster-token    etcd initial cluster token  (optional)
  --member-name              etcd member name           (optional)
  --yes-i-really-mean-it     required confirmation for restore

  --help                     Show this message.
EOF
}

log() {
  echo "[$(date -Is)] [etcd-snapshot] $*"
}

die() {
  echo "[$(date -Is)] [etcd-snapshot] ERROR: $*" >&2
  exit 1
}

ENDPOINTS="https://127.0.0.1:2379"
CACERT="/etc/kubernetes/pki/etcd/ca.crt"
CERT="/etc/kubernetes/pki/etcd/server.crt"
KEY="/etc/kubernetes/pki/etcd/server.key"
SNAPSHOT_DIR="/var/lib/etcd-snapshots"
SNAPSHOT=""
NAME=""
DATA_DIR=""
INITIAL_CLUSTER=""
INITIAL_CLUSTER_TOKEN=""
MEMBER_NAME=""
CONFIRMED=false

[[ $# -ge 1 ]] || { usage; exit 2; }
CMD="$1"
shift

while [[ $# -gt 0 ]]; do
  case "$1" in
    --endpoints)              ENDPOINTS="$2"; shift 2 ;;
    --cacert)                 CACERT="$2"; shift 2 ;;
    --cert)                   CERT="$2"; shift 2 ;;
    --key)                    KEY="$2"; shift 2 ;;
    --snapshot-dir)           SNAPSHOT_DIR="$2"; shift 2 ;;
    --snapshot)               SNAPSHOT="$2"; shift 2 ;;
    --name)                   NAME="$2"; shift 2 ;;
    --data-dir)               DATA_DIR="$2"; shift 2 ;;
    --initial-cluster)        INITIAL_CLUSTER="$2"; shift 2 ;;
    --initial-cluster-token)  INITIAL_CLUSTER_TOKEN="$2"; shift 2 ;;
    --member-name)            MEMBER_NAME="$2"; shift 2 ;;
    --yes-i-really-mean-it)   CONFIRMED=true; shift ;;
    --help|-h)                usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
  esac
done

case "$CMD" in
  save|status|restore) ;;
  *) echo "Unknown subcommand: $CMD" >&2; usage; exit 2 ;;
esac

# Prefer etcdutl (etcd >= 3.5); fall back to etcdctl for older hosts.
if command -v etcdutl >/dev/null 2>&1; then
  ETCDUTL="etcdutl"
elif command -v etcdctl >/dev/null 2>&1; then
  ETCDUTL=""
else
  die "neither etcdutl nor etcdctl found in PATH; install etcd tools first"
fi

if command -v etcdctl >/dev/null 2>&1; then
  ETCDCTL="etcdctl"
else
  ETCDCTL=""
fi

[[ -n "$ETCDUTL" || -n "$ETCDCTL" ]] || die "no etcd snapshot tool available"

# Build the shared connection arguments once, in the current shell.
COMMON_ARGS=(--endpoints "$ENDPOINTS")
if [[ "$ENDPOINTS" == https://* ]]; then
  [[ -f "$CACERT" ]] || die "CA certificate not found: $CACERT"
  [[ -f "$CERT" ]] || die "client certificate not found: $CERT"
  [[ -f "$KEY" ]] || die "client key not found: $KEY"
  COMMON_ARGS+=(--cacert "$CACERT" --cert "$CERT" --key "$KEY")
fi

# ---------------------------------------------------------------------------
# save
# ---------------------------------------------------------------------------
cmd_save() {
  [[ -n "$NAME" ]] || NAME="etcd-$(date +%Y%m%d%H%M%S).db"
  [[ "$NAME" == *.db ]] || NAME="${NAME}.db"
  local target="${SNAPSHOT_DIR}/${NAME}"

  mkdir -p "$SNAPSHOT_DIR"
  [[ -n "$ETCDCTL" ]] || die "etcdctl is required for 'save'"

  log "Saving snapshot to $target"
  ETCDCTL_API=3 "$ETCDCTL" "${COMMON_ARGS[@]}" snapshot save "$target"

  log "Writing checksum manifest"
  (cd "$SNAPSHOT_DIR" && sha256sum "$NAME" > "${NAME}.sha256")

  log "Snapshot complete"
  ETCDCTL_API=3 "$ETCDCTL" "${COMMON_ARGS[@]}" snapshot status "$target" -w table \
    || log "WARNING: snapshot status returned non-zero; verify with the 'status' subcommand"

  echo "OK: snapshot saved to $target"
}

# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------
cmd_status() {
  [[ -n "$SNAPSHOT" ]] || die "--snapshot is required for 'status'"
  [[ -f "$SNAPSHOT" ]] || die "snapshot not found: $SNAPSHOT"

  if [[ -f "${SNAPSHOT}.sha256" ]]; then
    log "Verifying checksum"
    (cd "$(dirname "$SNAPSHOT")" && sha256sum -c "$(basename "${SNAPSHOT}.sha256")") \
      || die "checksum verification failed for $SNAPSHOT"
  else
    log "WARNING: no checksum manifest at ${SNAPSHOT}.sha256; skipping verification"
  fi

  if [[ -n "$ETCDUTL" ]]; then
    "$ETCDUTL" snapshot status "$SNAPSHOT" -w table
  else
    [[ -n "$ETCDCTL" ]] || die "etcdctl is required for legacy status"
    ETCDCTL_API=3 "$ETCDCTL" snapshot status "$SNAPSHOT" -w table
  fi
}

# ---------------------------------------------------------------------------
# restore (destructive)
# ---------------------------------------------------------------------------
cmd_restore() {
  [[ -n "$SNAPSHOT" ]] || die "--snapshot is required for 'restore'"
  [[ -n "$DATA_DIR" ]] || die "--data-dir is required for 'restore'"
  [[ -f "$SNAPSHOT" ]] || die "snapshot not found: $SNAPSHOT"
  [[ "$CONFIRMED" == true ]] || die "restore refuses to run without --yes-i-really-mean-it"

  # Restoring while etcd is alive corrupts the member set.
  if pgrep -x etcd >/dev/null 2>&1; then
    die "an etcd process is still running; stop the static pod first (see the runbook)"
  fi

  if [[ -f "${SNAPSHOT}.sha256" ]]; then
    log "Verifying checksum"
    (cd "$(dirname "$SNAPSHOT")" && sha256sum -c "$(basename "${SNAPSHOT}.sha256")") \
      || die "checksum verification failed for $SNAPSHOT"
  else
    log "WARNING: no checksum manifest at ${SNAPSHOT}.sha256; refusing to restore"
    die "generate a checksum (save subcommand) or add one manually before restoring"
  fi

  # Move (never delete) any existing data directory aside for forensics.
  if [[ -d "$DATA_DIR" ]]; then
    local retired="${DATA_DIR}.corrupt-$(date +%Y%m%d%H%M%S)"
    log "Moving existing data dir aside to $retired"
    mv "$DATA_DIR" "$retired"
  fi

  local restore_args=(snapshot restore "$SNAPSHOT" --data-dir "$DATA_DIR")
  [[ -n "$INITIAL_CLUSTER" ]] && restore_args+=(--initial-cluster "$INITIAL_CLUSTER")
  [[ -n "$INITIAL_CLUSTER_TOKEN" ]] && restore_args+=(--initial-cluster-token "$INITIAL_CLUSTER_TOKEN")
  [[ -n "$MEMBER_NAME" ]] && restore_args+=(--name "$MEMBER_NAME")

  log "Restoring $SNAPSHOT into $DATA_DIR"
  if [[ -n "$ETCDUTL" ]]; then
    "$ETCDUTL" "${restore_args[@]}"
  else
    [[ -n "$ETCDCTL" ]] || die "etcdctl is required for legacy restore"
    ETCDCTL_API=3 "$ETCDCTL" "${restore_args[@]}"
  fi

  # Restore the ownership the kubelet/static pod expects.
  if id etcd >/dev/null 2>&1; then
    chown -R etcd:etcd "$DATA_DIR"
  else
    log "WARNING: no 'etcd' user on this host; verify ownership of $DATA_DIR"
  fi

  local manifest="${SNAPSHOT%.db}-restore-manifest.json"
  cat > "$manifest" <<EOF
{
  "snapshot": "$SNAPSHOT",
  "snapshot_sha256": "$(cut -d' ' -f1 "${SNAPSHOT}.sha256")",
  "data_dir": "$DATA_DIR",
  "restored_at": "$(date -Is)",
  "initial_cluster": "$INITIAL_CLUSTER",
  "member_name": "$MEMBER_NAME"
}
EOF

  log "Restore manifest written to $manifest"
  log "Restart etcd (restore the static-pod manifest) and verify with: etcdctl endpoint health --cluster"
  echo "OK: $SNAPSHOT restored into $DATA_DIR"
}

case "$CMD" in
  save)    cmd_save ;;
  status)  cmd_status ;;
  restore) cmd_restore ;;
esac
