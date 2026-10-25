# Vault Seed Injection via Mutating Admission Webhook

> **See also:** [Credentials & Secrets (Central Reference)](credentials-and-secrets.md) and the [Vault + Stellar tutorial](../vault-stellar-tutorial.md).

Validators traditionally receive their seed key through a Kubernetes Secret
mounted or copied by the operator.  Anyone with cluster RBAC — or anyone able
to read etcd snapshots — can enumerate that Secret and recover the plaintext
seed.  This feature (issue #224) removes that attack surface: the seed no
longer exists in any Kubernetes resource at all.

## How it works

A `MutatingAdmissionWebhookConfiguration` (Helm:
`vaultInjector.enabled=true`, requires `webhook.enabled=true`) intercepts
Pod **CREATE** operations for `stellar.io/vault-injection: "enabled"` pods and
routes them to the stellar-webhook server at `POST /mutate/vault`
(`controller/src/webhook/vault_injector.rs`).

The webhook returns a JSON Patch that injects:

1. **`vault-seed-init`** — an init container (official Vault image) that runs
   a hardened script:
   - reads the projected Kubernetes **service-account JWT**
     (`/var/run/secrets/kubernetes.io/serviceaccount/token`) from the
     kubelet-mounted token file — never from an environment variable;
   - performs the Vault **Kubernetes auth** login (`POST /v1/kubernetes/login`)
     and exchanges the JWT for a short-lived client token, which lives only in
     shell memory and a tmpfs scratch file deleted immediately after use;
   - reads the seed from the configured KV-v2 path (`X-Vault-Token` header);
   - renders the full `stellar-core.cfg` containing the plaintext seed onto
     the tmpfs volume with `0400` permissions;
   - scrubs all intermediate secret values from its own memory.
2. **`stellar-seed-tmpfs`** — an `emptyDir` volume with `medium: Memory`
   (tmpfs, size-capped at 16 MiB by default): backed by RAM, destroyed with
   the pod, never a persistent disk.
3. A **read-only mount** of that volume into the `stellar-core` container.

```
JWT → Vault login → token → KV read → stellar-core.cfg (tmpfs) → stellar-core
```

## What never happens

| Surface                             | Guaranteed by                                                          |
| ----------------------------------- | --------------------------------------------------------------------- |
| Kubernetes Secret                   | No Secret is created or referenced in the patch                        |
| Persistent disk                     | Only destination of the seed is the `Memory`-medium emptyDir           |
| Container env vars                  | Seed/session tokens never appear in any env var (config keys are non-secret identifiers) |
| Webhook logs / admission payloads   | The patch contains zero secret material; `verify_patch_safety()` rejects any write carrying a seed-shaped value or a disallowed JSON pointer |
| etcd                                | The patch (which becomes the pod spec) is audited to carry no seed-shaped material |

The webhook is **fail-open**: a degraded Vault or injector never blocks node
startup; pods come up unmodified and operators can remediate.  Idempotency is
carried by the `stellar.io/vault-injected` annotation.

The Vault client used server-side (`controller/src/webhook/auth.rs`) issues
tokens through the same Kubernetes auth method, caches them in process memory
only (zeroized on drop, refreshed at 80 % of lease) and is exercised by the
integration tests.

## Configuration

| Helm value                                    | Default                     | Purpose                                          |
| --------------------------------------------- | --------------------------- | ------------------------------------------------ |
| `vaultInjector.enabled`                       | `false`                     | Install the MutatingWebhookConfiguration         |
| `vaultInjector.failurePolicy`                 | `Ignore`                    | Fail-open admission                              |
| `vaultInjector.webhookTimeoutSeconds`         | `10`                        | Admission timeout (K8s hard max)                 |
| `vaultInjector.skipNamespaces`                | `[]`                        | Extra namespaces excluded from interception      |

Per-pod overrides (pod annotations, all non-secret):

| Annotation                       | Example                              |
| -------------------------------- | ------------------------------------ |
| `stellar.io/vault-injection`     | `enabled` (required to opt in)       |
| `stellar.io/vault-addr`          | `https://vault.vault.svc:8200`       |
| `stellar.io/vault-role`          | `stellar-validator`                  |
| `stellar.io/vault-secret-path`   | `secret/data/stellar/validator`      |
| `stellar.io/vault-secret-key`    | `seed`                               |

Server-level defaults come from the `VAULT_ADDR`, `VAULT_ROLE`,
`VAULT_SECRET_PATH`, `VAULT_SECRET_KEY`, `VAULT_INIT_IMAGE` and
`VAULT_TMPFS_SIZE` environment variables — again non-secret parameters only.

## Rollout

```bash
# 1. Vault with Kubernetes auth bound to the validator service account
helm repo add hashicorp https://helm.releases.hashicorp.com
helm install vault hashicorp/vault --namespace vault --create-namespace
kubectl exec -n vault vault-0 -- \
  vault auth enable kubernetes 2>/dev/null || true
# … configure the auth config/policy/role bound to stellar-validator.

# 2. Store the seed in KV v2
kubectl exec -n vault vault-0 -- \
  vault kv put secret/stellar/validator seed="S…"

# 3. Enable the injector
helm upgrade stellar-operator charts/stellar-operator \
  --set webhook.enabled=true --set vaultInjector.enabled=true

# 4. Track a validator pod
kubectl apply -f examples/vault/injected-validator-pod.yaml
```

## Verification (Definition of Done)

```bash
# The seed init container and tmpfs volume are present …
kubectl get pod validator-0 -o json \
  | jq '.spec.initContainers[].name, .spec.volumes[].emptyDir.medium'
# → "vault-seed-init", "Memory"

# … but no Kubernetes Secret carries the validator identity …
kubectl get secrets -o name | grep validator-0   # → empty

# … and the seed exists only on tmpfs, not on disk.
kubectl exec validator-0 -c stellar-core -- \
  sh -c 'mount | grep /etc/stellar-seed; grep -c SEED /etc/stellar-seed/stellar-core.cfg'
# mount → tmpfs; cfg contains the seed key only in RAM-backed storage.
```

## Integration tests

`controller/tests/vault_injection_integration.rs` spins up an in-process mock
Vault (Kubernetes-auth login + KV-v2 read) and validates the **full
lifecycle**: JWT rejection, login, in-memory token caching and renewal,
secret read and extraction, patch round-trip through the admission handler,
idempotency, fail-open behaviour, and the security invariants above (no seed,
token or `Secret` reference inside the patch that lands in etcd).

```bash
cd controller && cargo test --test vault_injection_integration
```
