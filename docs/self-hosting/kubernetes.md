# Kubernetes (Helm)

The chart runs llmobs as a StatefulSet with exactly one pod and one PersistentVolumeClaim mounted at
`/data`. llmobs is a single process that locks its data directory, so there is no replica setting
and no horizontal scaling. The volume must be `ReadWriteOnce` block or local storage: the lock
(`flock`) is not reliable on NFS-like volumes.

```bash
helm install llmobs oci://ghcr.io/sebastiencs/charts/llmobs --version 0.1.0 \
  --namespace llmobs --create-namespace \
  --set publicUrl=https://llmobs.example.com
kubectl -n llmobs logs llmobs-0 | grep 'setup required'
```

The chart version is the llmobs version. The values are listed in the chart's
[README](../../deploy/helm/llmobs/README.md); the main ones:

| Value | Default | Purpose |
|---|---|---|
| `publicUrl` | `http://localhost:8080` | `LLMOBS_PUBLIC_URL` |
| `persistence.size`, `persistence.storageClassName` | `20Gi`, cluster default | the data volume |
| `config` | `{}` | nested settings written to `llmobs.toml` |
| `env` | `[]` | extra environment variables |
| `secret.existingSecret` | `""` | a Secret whose keys become environment variables |
| `secret.values` | `{}` | variables of a Secret the chart creates |
| `s3.*` | off | Parquet files and media in a bucket ([s3.md](s3.md)) |
| `ingress.*` | off | an Ingress for the HTTP port |

Secrets (SMTP URL, OAuth client secrets, the licence key, `AWS_*` credentials,
`LLMOBS_ENCRYPTION_KEY`) belong in a Secret you create, named by `secret.existingSecret`:

```bash
kubectl -n llmobs create secret generic llmobs-env \
  --from-literal=LLMOBS_SMTP_URL='smtps://user:password@smtp.example.com:465' \
  --from-literal=LLMOBS_EMAIL_FROM='llmobs <llmobs@example.com>'
helm upgrade llmobs oci://ghcr.io/sebastiencs/charts/llmobs --version 0.1.0 -n llmobs \
  --reuse-values --set secret.existingSecret=llmobs-env
```

Without `LLMOBS_ENCRYPTION_KEY`, llmobs generates `/data/secret.key` on its first start; it lives
on the volume and must be backed up with it ([backup.md](backup.md)).

OTLP: the Service exposes 8080 (HTTP, including `/v1/traces`), 4317 (gRPC) and 4318 (HTTP). The
Ingress routes 8080 only; expose gRPC through a Service of type `LoadBalancer` or a gRPC-aware
ingress if you need it from outside the cluster.

The pod runs as uid/gid 65532 with a read-only root filesystem and no capabilities. Probes:
startup and liveness on `/healthz` (the startup probe allows 10 minutes for WAL replay), readiness
on `/readyz`. `terminationGracePeriodSeconds` is 60 so a stop can drain and flush.

Upgrades: [upgrade.md](upgrade.md#kubernetes). Backups: [backup.md](backup.md#kubernetes).
