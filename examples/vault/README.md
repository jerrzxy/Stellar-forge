# Vault examples for Stellar-K8s

Use together with [`docs/vault-stellar-tutorial.md`](../../docs/vault-stellar-tutorial.md).

Files to add in your cluster (after Vault + injector are running):

- `ClusterRoleBinding` or `RoleBinding` linking the validator `ServiceAccount` to the Vault Kubernetes auth role.
- Optional `MutatingWebhookConfiguration` check: ensure the injector is not blocked by network policies on the `vault-agent` sidecar.

### Built-in Vault seed injector (issue #224)

Rather than the generic Vault Agent Injector, you can use the Stellar-K8s
built-in injector (`/mutate/vault` webhook): it intercepts pods annotated
`stellar.io/vault-injection: "enabled"`, authenticates via the Kubernetes
service-account auth method at pod start-up, and renders `stellar-core.cfg`
into a memory-only tmpfs volume — the plaintext seed never reaches etcd,
a Kubernetes Secret, or persistent disk.

See [`injected-validator-pod.yaml`](injected-validator-pod.yaml) and
[docs/security/vault-webhook-injection.md](../../docs/security/vault-webhook-injection.md).

## Policy (illustrative)

Allow read on `secret/data/stellar/*` for the validator role. Tune to least privilege in production.

```hcl
path "secret/data/stellar/*" {
  capabilities = ["read"]
}
```

Store policies and roles via your GitOps / Vault admin workflow — not committed as static secrets here.
