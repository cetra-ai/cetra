# llmobs Helm chart

Runs llmobs as a StatefulSet with one pod and one PersistentVolumeClaim (`/data`). Guide:
[docs/self-hosting/kubernetes.md](../../../docs/self-hosting/kubernetes.md).

```bash
helm install llmobs oci://ghcr.io/sebastiencs/charts/llmobs --version <version> \
  --namespace llmobs --create-namespace --set publicUrl=https://llmobs.example.com
```

The chart version is the llmobs version.

## Values

| Value | Default | Description |
|---|---|---|
| `image.repository` | `ghcr.io/sebastiencs/llmobs` | image |
| `image.tag` | `""` | empty: the chart's `appVersion` |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | e.g. `[{name: ghcr}]` for a private registry |
| `publicUrl` | `http://localhost:8080` | `LLMOBS_PUBLIC_URL`: the URL users open |
| `config` | `{}` | nested settings rendered to `/etc/llmobs/llmobs.toml` (`LLMOBS_CONFIG`); empty: no file |
| `env` | `[]` | extra container environment variables (`EnvVar` list) |
| `secret.existingSecret` | `""` | Secret whose keys become environment variables |
| `secret.values` | `{}` | `NAME: value` pairs of a Secret the chart creates (stored in the release; prefer `existingSecret`) |
| `s3.enabled` | `false` | Parquet files and media in a bucket |
| `s3.bucket` | `""` | required when `s3.enabled` |
| `s3.prefix` | `""` | key prefix in the bucket |
| `s3.region` | `""` | `AWS_REGION` |
| `s3.endpoint` | `""` | custom endpoint; empty: AWS |
| `s3.forcePathStyle` | `false` | path-style bucket addressing |
| `s3.allowHttp` | `false` | allow an `http://` endpoint |
| `persistence.size` | `20Gi` | size of the data volume |
| `persistence.storageClassName` | `""` | empty: the default StorageClass |
| `resources` | `{}` | container resources |
| `extraVolumes`, `extraVolumeMounts` | `[]`, `[]` | more pod volumes and llmobs container mounts, e.g. a backup volume |
| `metrics.enabled` | `false` | Prometheus `/metrics` on port 9464 (`LLMOBS_METRICS_ADDR`), a `metrics` container port; no authentication, no Service port ([monitoring.md](../../../docs/self-hosting/monitoring.md)) |
| `metrics.podAnnotations` | `prometheus.io/scrape`, `port` (9464), `path` (`/metrics`) | added to the pod when `metrics.enabled`; `podAnnotations` wins on a shared key |
| `service.type` | `ClusterIP` | Service type (ports 8080, 4317, 4318) |
| `service.annotations` | `{}` | |
| `ingress.enabled` | `false` | Ingress for port 8080 (UI, API, OTLP/HTTP, MCP) |
| `ingress.className` | `""` | |
| `ingress.annotations` | `{}` | |
| `ingress.host` | `""` | required when `ingress.enabled` |
| `ingress.tls` | `[]` | Ingress `tls` entries |
| `podAnnotations`, `podLabels` | `{}` | |
| `nodeSelector`, `tolerations`, `affinity` | `{}`, `[]`, `{}` | scheduling |

S3 credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) and other secrets go in
`secret.existingSecret` or `secret.values`. The chart never generates secrets: without
`LLMOBS_ENCRYPTION_KEY`, llmobs generates `/data/secret.key` on the volume on its first start.

Fixed by the chart: one replica, `terminationGracePeriodSeconds: 60`, uid/gid 65532, read-only
root filesystem, no capabilities, `automountServiceAccountToken: false`, the PVC kept when the
release is deleted.
