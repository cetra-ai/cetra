# N6 — The Helm chart

Read [README.md](README.md) first, especially § Design › Helm chart.

**Goal:**
- `deploy/helm/llmobs/`: a minimal chart that runs llmobs as one StatefulSet pod with a
  persistent volume, optional S3, and config through values, environment variables and Secrets
  (D-N-10).
- `just helm-check` lints the chart and validates the rendered manifests with kubeconform, for
  the default values and two test value files, and checks the two template guards.
- `just helm-smoke [tag]` installs the chart on a throwaway kind cluster and smoke-tests it.

**Edit only:**
- `deploy/helm/llmobs/Chart.yaml`, `values.yaml`, `.helmignore`, `README.md`,
  `templates/_helpers.tpl`, `templates/statefulset.yaml`, `templates/service.yaml`,
  `templates/configmap.yaml`, `templates/secret.yaml`, `templates/ingress.yaml`,
  `templates/NOTES.txt`, `ci/s3-values.yaml`, `ci/full-values.yaml` (new);
- `tools/packaging/helm-check.sh`, `tools/packaging/helm-smoke.sh` (new, executable);
- `justfile`.

**Commit:** `Add the Helm chart`

**Tools:** helm v4.3.1, kubeconform v0.8.0 (it downloads the Kubernetes schemas from GitHub, so
it needs network), kind v0.33.0, kubectl, Docker, python3.

## How it works

- **No replica value.** `replicas: 1` is written in the template. One process locks the data
  directory, so the chart offers no way to run two.
- **Rolling update.** `updateStrategy: RollingUpdate` on a one-pod StatefulSet deletes the old
  pod before it creates the new one, so two pods never share the volume.
- **`config`.** It is rendered with Helm's `toToml` into a ConfigMap key `llmobs.toml`, mounted
  at `/etc/llmobs` and named by `LLMOBS_CONFIG`. With `config: {}` there is no ConfigMap, mount
  or variable.
- **Restarts on change.** The pod template carries `checksum/config` and `checksum/secret`
  annotations, so a change to either restarts the pod.
- **Environment order.** The fixed variables come first (`LLMOBS_DATA_DIR`,
  `LLMOBS_PUBLIC_URL`, `LLMOBS_CONFIG`, the S3 ones), then `env`. `envFrom` holds the chart's
  Secret, then `existingSecret`. In Kubernetes, `env` wins over `envFrom` for the same name.
- **No service links.** `enableServiceLinks: false`: otherwise Kubernetes injects
  `LLMOBS_PORT=tcp://…` and similar variables for the Service named `llmobs`, and the config
  loader rejects them as unknown `LLMOBS_*` keys, so the pod crash-loops.
- **Secrets.** The chart generates none: no `randAlphaNum`, no `lookup`.
- **Guards.** `fail` stops the render when `s3.enabled` has no `s3.bucket`, or when
  `ingress.enabled` has no `ingress.host`.
- **Versions.** `Chart.yaml` `version` and `appVersion` are the workspace version, `0.0.0` now.
  N8's `release-prep` rewrites both lines and `versions-check` checks them, so keep them exactly
  as given.
- **`helm-smoke.sh`:**
  1. Creates a kind cluster named `llmobs-smoke-<pid>` and `kind load`s the image.
  2. Installs with `image.pullPolicy=Never`, `persistence.size=1Gi` and the smoke init variables
     in `secret.values`, then waits for the pod to be ready.
  3. Port-forwards the Service's 8080 and 4318 and runs `smoke.sh`.
  4. Always deletes the cluster.

  `helm install --wait --timeout 11m` outlasts the startup probe (120 × 5 s = 10 min).

## Files

### `deploy/helm/llmobs/Chart.yaml`

```yaml
apiVersion: v2
name: llmobs
description: LLM observability and evaluation in one binary (one StatefulSet, one volume).
type: application
# Chart version = app version = the workspace version (D-N-5); `just release-prep` sets both.
version: 0.0.0
appVersion: "0.0.0"
home: https://github.com/sebastiencs/llmobs
sources:
  - https://github.com/sebastiencs/llmobs
```

### `deploy/helm/llmobs/.helmignore`

```text
# Not packaged into the chart archive.
ci/
.helmignore
```

### `deploy/helm/llmobs/values.yaml`

```yaml
# Values of the llmobs chart (D-N-10). Reference: deploy/helm/llmobs/README.md.
# llmobs runs as exactly one pod: its data directory is a single volume that one process locks
# (spec 12 § Packaging). There is no replica count.

image:
  repository: ghcr.io/sebastiencs/llmobs
  # Empty: the chart's appVersion.
  tag: ""
  pullPolicy: IfNotPresent

imagePullSecrets: []

# The URL users open (LLMOBS_PUBLIC_URL): links in emails and OAuth redirects use it.
publicUrl: http://localhost:8080

# Settings written to llmobs.toml (spec 12 § Configuration), as nested keys. Empty: no file.
# Example:
#   config:
#     storage:
#       flush:
#         max_age: 60s
#     auth:
#       signup: invite_only
config: {}

# Extra environment variables of the llmobs container (Kubernetes EnvVar list).
env: []

secret:
  # Name of an existing Secret in the release namespace. Each of its keys becomes an
  # environment variable (envFrom), e.g. LLMOBS_ENCRYPTION_KEY, LLMOBS_SMTP_URL,
  # LLMOBS_LICENSE_KEY, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY.
  existingSecret: ""
  # Variables for a Secret this chart creates, as NAME: value. Prefer existingSecret: values
  # given here are stored in the Helm release.
  values: {}

# S3-compatible object storage for Parquet files and media. Off: they stay on the volume.
# Credentials: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY in secret.existingSecret or
# secret.values.
s3:
  enabled: false
  bucket: ""
  # Optional key prefix inside the bucket.
  prefix: ""
  # AWS_REGION; required by most providers.
  region: ""
  # Custom endpoint (Garage, R2, Hetzner, ...). Empty: AWS.
  endpoint: ""
  forcePathStyle: false
  allowHttp: false

persistence:
  # The volume holding /data: WAL, catalog.db, meta.db, secret.key and, without S3, the
  # Parquet files and media.
  size: 20Gi
  # Empty: the cluster's default StorageClass.
  storageClassName: ""

resources: {}

service:
  type: ClusterIP
  annotations: {}

# Routes the HTTP port (UI, API, OTLP/HTTP at /v1/traces, MCP). OTLP/gRPC (4317) is not
# routed by the Ingress; use the Service.
ingress:
  enabled: false
  className: ""
  annotations: {}
  host: ""
  # Example: [{secretName: llmobs-tls, hosts: [llmobs.example.com]}]
  tls: []

podAnnotations: {}
podLabels: {}
nodeSelector: {}
tolerations: []
affinity: {}
```

### `deploy/helm/llmobs/templates/_helpers.tpl`

```gotemplate
{{/* Names and labels of the llmobs chart (D-N-10). */}}

{{- define "llmobs.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "llmobs.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "llmobs.selectorLabels" -}}
app.kubernetes.io/name: {{ include "llmobs.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "llmobs.labels" -}}
{{ include "llmobs.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "llmobs.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "llmobs.objectStore" -}}
{{- if .Values.s3.prefix -}}
{{- printf "s3://%s/%s" .Values.s3.bucket (trimAll "/" .Values.s3.prefix) -}}
{{- else -}}
{{- printf "s3://%s" .Values.s3.bucket -}}
{{- end -}}
{{- end -}}
```

### `deploy/helm/llmobs/templates/statefulset.yaml`

```yaml
{{- if and .Values.s3.enabled (not .Values.s3.bucket) }}
{{- fail "s3.bucket is required when s3.enabled is true" }}
{{- end }}
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: {{ include "llmobs.fullname" . }}
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
spec:
  # Exactly one: one process locks the data directory (spec 12 § Packaging, D-N-10).
  replicas: 1
  serviceName: {{ include "llmobs.fullname" . }}-headless
  podManagementPolicy: OrderedReady
  updateStrategy:
    type: RollingUpdate
  persistentVolumeClaimRetentionPolicy:
    whenDeleted: Retain
    whenScaled: Retain
  selector:
    matchLabels:
      {{- include "llmobs.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "llmobs.selectorLabels" . | nindent 8 }}
        {{- with .Values.podLabels }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      annotations:
        checksum/config: {{ include (print $.Template.BasePath "/configmap.yaml") . | sha256sum }}
        checksum/secret: {{ include (print $.Template.BasePath "/secret.yaml") . | sha256sum }}
        {{- with .Values.podAnnotations }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
    spec:
      automountServiceAccountToken: false
      # Service links would inject LLMOBS_PORT=tcp://... and friends, which the config loader
      # rejects as unknown LLMOBS_* keys.
      enableServiceLinks: false
      # Shutdown drains requests (20 s) and flushes memtables (30 s) (spec 12).
      terminationGracePeriodSeconds: 60
      {{- with .Values.imagePullSecrets }}
      imagePullSecrets:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        fsGroup: 65532
        fsGroupChangePolicy: OnRootMismatch
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: llmobs
          image: {{ include "llmobs.image" . | quote }}
          imagePullPolicy: {{ .Values.image.pullPolicy }}
          args: ["serve"]
          ports:
            - name: http
              containerPort: 8080
              protocol: TCP
            - name: otlp-grpc
              containerPort: 4317
              protocol: TCP
            - name: otlp-http
              containerPort: 4318
              protocol: TCP
          env:
            - name: LLMOBS_DATA_DIR
              value: /data
            - name: LLMOBS_PUBLIC_URL
              value: {{ .Values.publicUrl | quote }}
            {{- if .Values.config }}
            - name: LLMOBS_CONFIG
              value: /etc/llmobs/llmobs.toml
            {{- end }}
            {{- if .Values.s3.enabled }}
            - name: LLMOBS_OBJECT_STORE
              value: {{ include "llmobs.objectStore" . | quote }}
            {{- with .Values.s3.region }}
            - name: AWS_REGION
              value: {{ . | quote }}
            {{- end }}
            {{- with .Values.s3.endpoint }}
            - name: LLMOBS_STORAGE__S3__ENDPOINT
              value: {{ . | quote }}
            {{- end }}
            - name: LLMOBS_STORAGE__S3__FORCE_PATH_STYLE
              value: {{ .Values.s3.forcePathStyle | quote }}
            - name: LLMOBS_STORAGE__S3__ALLOW_HTTP
              value: {{ .Values.s3.allowHttp | quote }}
            {{- end }}
            {{- with .Values.env }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
          {{- if or .Values.secret.existingSecret .Values.secret.values }}
          envFrom:
            {{- if .Values.secret.values }}
            - secretRef:
                name: {{ include "llmobs.fullname" . }}
            {{- end }}
            {{- with .Values.secret.existingSecret }}
            - secretRef:
                name: {{ . }}
            {{- end }}
          {{- end }}
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
          # Opening storage replays the WAL before the ports open; allow 10 minutes.
          startupProbe:
            httpGet:
              path: /healthz
              port: http
            periodSeconds: 5
            failureThreshold: 120
          livenessProbe:
            httpGet:
              path: /healthz
              port: http
            periodSeconds: 10
            timeoutSeconds: 5
            failureThreshold: 6
          readinessProbe:
            httpGet:
              path: /readyz
              port: http
            periodSeconds: 10
            timeoutSeconds: 5
            failureThreshold: 3
          {{- with .Values.resources }}
          resources:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          volumeMounts:
            - name: data
              mountPath: /data
            {{- if .Values.config }}
            - name: config
              mountPath: /etc/llmobs
              readOnly: true
            {{- end }}
      {{- if .Values.config }}
      volumes:
        - name: config
          configMap:
            name: {{ include "llmobs.fullname" . }}
      {{- end }}
      {{- with .Values.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
  volumeClaimTemplates:
    - metadata:
        name: data
        labels:
          {{- include "llmobs.selectorLabels" . | nindent 10 }}
      spec:
        accessModes: ["ReadWriteOnce"]
        {{- with .Values.persistence.storageClassName }}
        storageClassName: {{ . | quote }}
        {{- end }}
        resources:
          requests:
            storage: {{ .Values.persistence.size | quote }}
```

### `deploy/helm/llmobs/templates/service.yaml`

```yaml
apiVersion: v1
kind: Service
metadata:
  name: {{ include "llmobs.fullname" . }}
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
  {{- with .Values.service.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  type: {{ .Values.service.type }}
  selector:
    {{- include "llmobs.selectorLabels" . | nindent 4 }}
  ports:
    - name: http
      port: 8080
      targetPort: http
      protocol: TCP
    - name: otlp-grpc
      port: 4317
      targetPort: otlp-grpc
      protocol: TCP
      appProtocol: grpc
    - name: otlp-http
      port: 4318
      targetPort: otlp-http
      protocol: TCP
---
# Governs the StatefulSet's pod identity (spec.serviceName).
apiVersion: v1
kind: Service
metadata:
  name: {{ include "llmobs.fullname" . }}-headless
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
spec:
  clusterIP: None
  selector:
    {{- include "llmobs.selectorLabels" . | nindent 4 }}
  ports:
    - name: http
      port: 8080
      targetPort: http
      protocol: TCP
```

### `deploy/helm/llmobs/templates/configmap.yaml`

```yaml
{{- if .Values.config }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "llmobs.fullname" . }}
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
data:
  llmobs.toml: |
    {{- toToml .Values.config | nindent 4 }}
{{- end }}
```

### `deploy/helm/llmobs/templates/secret.yaml`

```yaml
{{- if .Values.secret.values }}
apiVersion: v1
kind: Secret
metadata:
  name: {{ include "llmobs.fullname" . }}
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
type: Opaque
stringData:
  {{- range $name, $value := .Values.secret.values }}
  {{ $name }}: {{ $value | toString | quote }}
  {{- end }}
{{- end }}
```

### `deploy/helm/llmobs/templates/ingress.yaml`

```yaml
{{- if .Values.ingress.enabled }}
{{- if not .Values.ingress.host }}
{{- fail "ingress.host is required when ingress.enabled is true" }}
{{- end }}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "llmobs.fullname" . }}
  labels:
    {{- include "llmobs.labels" . | nindent 4 }}
  {{- with .Values.ingress.annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
spec:
  {{- with .Values.ingress.className }}
  ingressClassName: {{ . }}
  {{- end }}
  {{- with .Values.ingress.tls }}
  tls:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  rules:
    - host: {{ .Values.ingress.host | quote }}
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: {{ include "llmobs.fullname" . }}
                port:
                  name: http
{{- end }}
```

### `deploy/helm/llmobs/templates/NOTES.txt`

```text
llmobs {{ .Chart.AppVersion }} is installed as the StatefulSet {{ include "llmobs.fullname" . }} (one pod).

Open the UI:
  kubectl --namespace {{ .Release.Namespace }} port-forward svc/{{ include "llmobs.fullname" . }} 8080:8080
  then http://localhost:8080 (the setup page creates the first admin, unless LLMOBS_INIT_* is set).

Send OTLP to the Service {{ include "llmobs.fullname" . }}: port 4318 (HTTP) or 4317 (gRPC), with an
sk-llmobs- key of the project as "Authorization: Bearer <key>".

Back up the volume, and the bucket when s3.enabled, as in docs/self-hosting/backup.md.
```

### `deploy/helm/llmobs/ci/s3-values.yaml`

```yaml
# `just helm-check` renders the chart with this file (S3 mode, existing secret).
s3:
  enabled: true
  bucket: llmobs
  prefix: cell-1
  region: garage
  endpoint: http://garage:3900
  forcePathStyle: true
  allowHttp: true
secret:
  existingSecret: llmobs-s3
```

### `deploy/helm/llmobs/ci/full-values.yaml`

```yaml
# `just helm-check` renders the chart with this file (every optional block on).
publicUrl: https://llmobs.example.com
config:
  storage:
    flush:
      max_age: 60s
  auth:
    signup: invite_only
env:
  - name: LLMOBS_LOG
    value: debug
secret:
  values:
    LLMOBS_SMTP_URL: smtps://user:password@smtp.example.com:465
    LLMOBS_EMAIL_FROM: llmobs <llmobs@example.com>
ingress:
  enabled: true
  className: nginx
  host: llmobs.example.com
  tls:
    - secretName: llmobs-tls
      hosts: [llmobs.example.com]
persistence:
  size: 50Gi
  storageClassName: fast
resources:
  requests:
    cpu: "1"
    memory: 2Gi
  limits:
    memory: 8Gi
podAnnotations:
  example.com/team: observability
nodeSelector:
  kubernetes.io/os: linux
```

### `deploy/helm/llmobs/README.md`

````markdown
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
````

The link to `docs/self-hosting/kubernetes.md` resolves once N11 adds that file, and so does the
`docs/self-hosting/backup.md` path in `NOTES.txt`; leave both as they are.

With the release name `llmobs`, the chart's fullname is `llmobs` (the `contains` branch of
`llmobs.fullname`), so the StatefulSet, Service, ConfigMap and Secret are all named `llmobs`
(different kinds, which is legal) and the pod is `llmobs-0`.

### `tools/packaging/helm-check.sh` (executable)

```bash
#!/usr/bin/env bash
# Lints the chart and validates its rendered manifests against the Kubernetes schemas, with the
# default values and each file in deploy/helm/llmobs/ci/, and checks the two template guards
# (D-N-10). Needs helm and kubeconform; kubeconform downloads the schemas.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
chart=deploy/helm/llmobs

for values in "" "$chart/ci/s3-values.yaml" "$chart/ci/full-values.yaml"; do
  args=()
  if [[ -n "$values" ]]; then
    args=(-f "$values")
  fi
  echo "helm-check: ${values:-default values}"
  helm lint --strict "$chart" "${args[@]}"
  helm template llmobs "$chart" "${args[@]}" | kubeconform -strict -summary
done

# The guards fail the render.
if helm template llmobs "$chart" --set s3.enabled=true >/dev/null 2>&1; then
  echo "helm-check: s3.enabled without s3.bucket rendered" >&2
  exit 1
fi
if helm template llmobs "$chart" --set ingress.enabled=true >/dev/null 2>&1; then
  echo "helm-check: ingress.enabled without ingress.host rendered" >&2
  exit 1
fi
echo "helm-check: passed"
```

### `tools/packaging/helm-smoke.sh` (executable)

```bash
#!/usr/bin/env bash
# Creates a kind cluster, loads an image, installs the chart with it and runs smoke.sh through
# `kubectl port-forward`, then deletes the cluster (D-N-10, D-N-13).
#
# Usage: helm-smoke.sh <image>, e.g. helm-smoke.sh llmobs:dev. Needs docker, kind, kubectl, helm,
# python3. The release is named llmobs, so the chart's fullname, the Service and the Secret are
# all `llmobs` and the pod is `llmobs-0`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
if [[ $# -ne 1 ]]; then
  echo "usage: helm-smoke.sh <image>" >&2
  exit 2
fi
image="$1"
repository="${image%:*}"
tag="${image##*:}"
cluster="llmobs-smoke-$$"
context="kind-$cluster"
key="sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ"
forward_pid=""
log="$(mktemp)"

cleanup() {
  if [[ -n "$forward_pid" ]]; then
    kill "$forward_pid" 2>/dev/null || true
  fi
  kind delete cluster --name "$cluster" >/dev/null 2>&1 || true
  rm -f "$log"
}
trap cleanup EXIT

kind create cluster --name "$cluster" --wait 180s
kind load docker-image "$image" --name "$cluster"

if ! helm install llmobs deploy/helm/llmobs --kube-context "$context" \
  --set image.repository="$repository" \
  --set image.tag="$tag" \
  --set image.pullPolicy=Never \
  --set persistence.size=1Gi \
  --set-string secret.values.LLMOBS_INIT_ADMIN_EMAIL=smoke@example.com \
  --set-string secret.values.LLMOBS_INIT_ADMIN_PASSWORD=llmobs-smoke-1 \
  --set-string secret.values.LLMOBS_INIT_ORG=Smoke \
  --set-string secret.values.LLMOBS_INIT_PROJECT=Smoke \
  --set-string secret.values.LLMOBS_INIT_SECRET_KEY="$key" \
  --wait --timeout 11m; then
  kubectl --context "$context" describe pod llmobs-0 >&2 || true
  kubectl --context "$context" logs llmobs-0 >&2 || true
  exit 1
fi

read -r http_port otlp_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(2)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')
if [[ -z "${http_port:-}" || -z "${otlp_port:-}" ]]; then
  echo "helm-smoke: could not pick free ports (is python3 installed?)" >&2
  exit 1
fi
kubectl --context "$context" port-forward svc/llmobs "$http_port:8080" "$otlp_port:4318" >"$log" 2>&1 &
forward_pid=$!

if ! tools/packaging/smoke.sh "http://127.0.0.1:$http_port" "http://127.0.0.1:$otlp_port" "$key"; then
  cat "$log" >&2
  kubectl --context "$context" logs llmobs-0 >&2 || true
  exit 1
fi
echo "helm-smoke: $image passed on kind"
```

### `justfile`

Append after the `compose-smoke` recipe:

```just
# Lint the Helm chart and validate its manifests with kubeconform (D-N-10)
helm-check:
    tools/packaging/helm-check.sh

# Install the chart with an image on a throwaway kind cluster and smoke-test it (D-N-10)
helm-smoke tag="llmobs:dev":
    tools/packaging/helm-smoke.sh {{tag}}
```

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
just helm-check
helm template llmobs deploy/helm/llmobs -f deploy/helm/llmobs/ci/full-values.yaml | grep -A5 'llmobs.toml: |'
just image llmobs:dev            # after just dist, as in N4
just helm-smoke llmobs:dev
just check
just assert-clean
```

Expected:
- `helm-check` runs `helm lint --strict` and `kubeconform -strict -summary` three times, and every
  summary reports `Invalid: 0, Errors: 0`. It ends with `helm-check: passed`.
- The rendered TOML holds `[auth]` with `signup = "invite_only"` and `[storage.flush]` with
  `max_age = "60s"`. Helm's `toToml` decides the table order; any order is fine.
- `helm-smoke` ends with `helm-smoke: llmobs:dev passed on kind`, and `kind get clusters` no
  longer lists `llmobs-smoke-*`.

If `helm lint --strict` warns about a missing `icon` in `Chart.yaml`, that is information only and
does not fail. Any other warning fails `--strict`: fix the template so it matches this file
exactly, and report if it already does.

## Done when

- [ ] The chart's 13 files and the two scripts exist (scripts mode `100755`); the two recipes are
      in the `justfile`.
- [ ] `just helm-check` and `just helm-smoke llmobs:dev` pass.
- [ ] `just check` and `just assert-clean` pass. One commit: `Add the Helm chart`.
