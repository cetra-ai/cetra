# 08: packaging and docs for restore mode, `/metrics` and `/readyz`

Read [00-design.md](00-design.md) first, especially § Decisions (D-N-25 to D-N-36) and D-N-32.

**Goal:** the image, the chart and compose know about the metrics port. The self-hosting docs explain restore mode, `/metrics` and readiness. Spec 12 § Self-monitoring describes what is built. The decisions, the CHANGELOG and the roadmap row are written.

**Edit only:**
- `deploy/docker/Dockerfile`;
- `deploy/helm/llmobs/` (`values.yaml`, `templates/statefulset.yaml`, `README.md`, a new `ci/metrics-values.yaml`);
- `tools/packaging/helm-check.sh`;
- `deploy/compose/compose.yaml`;
- `docs/self-hosting/` (`configuration.md`, `backup.md`, `kubernetes.md`, `docker-compose.md`, `s3.md`, a new `monitoring.md`, `README.md`);
- `docs/spec/12-operations.md`;
- `docs/mvp-decisions.md`;
- `docs/roadmap.md`;
- `docs/plans/n-packaging/README.md` (§ Requests for Q, if it lists R11 or R18);
- `CHANGELOG.md`.

**Commit:** `Document restore mode, /metrics and /readyz`

**Needs:** 01–07 merged.

## Changes

### Image

`deploy/docker/Dockerfile`: `EXPOSE 8080 4317 4318 9464`. The port is only used when `metrics_addr` is set; say so in a comment above the line. Update spec 12's Docker image row (`EXPOSE 8080 4317 4318 9464`).

### Helm chart (D-N-32)

**`values.yaml`:**

```yaml
# Prometheus metrics on port 9464 (`metrics_addr`, D-N-28). Off by default.
metrics:
  enabled: false
  # Added to the pod when metrics are enabled, for annotation-based scraping.
  podAnnotations:
    prometheus.io/scrape: "true"
    prometheus.io/port: "9464"
    prometheus.io/path: "/metrics"
```

**`templates/statefulset.yaml`,** when `.Values.metrics.enabled`:
- the env var `LLMOBS_METRICS_ADDR` = `0.0.0.0:9464`;
- a container port `metrics` (9464/TCP);
- `.Values.metrics.podAnnotations` merged into the pod annotations. Build **one** dict, so a key set in both does not render twice (duplicate YAML keys):

  ```yaml
  {{- $annotations := deepCopy (.Values.podAnnotations | default dict) }}
  {{- if .Values.metrics.enabled }}
  {{- $annotations = merge $annotations .Values.metrics.podAnnotations }}
  {{- end }}
  ```

  `.Values.podAnnotations` wins over the metrics defaults (`merge` keeps the first dict's keys). Then render `$annotations` where `.Values.podAnnotations` is rendered today.

No Service port and no `ServiceMonitor`: Prometheus scrapes the pod. Keep the probes as they are; the liveness and startup probes stay on `/healthz`.

**Checks:**
- `ci/metrics-values.yaml` sets `metrics.enabled: true`.
- `tools/packaging/helm-check.sh` adds that file to its loop.
- `README.md` (the chart's) gets the `metrics.*` rows in its values table.

### Compose

`deploy/compose/compose.yaml`, in the `llmobs` service's environment block, add commented lines:

```yaml
      # Prometheus metrics (docs/self-hosting/monitoring.md); also publish the port.
      # LLMOBS_METRICS_ADDR: "0.0.0.0:9464"
```

### `docs/self-hosting/`

**`monitoring.md` (new):**
- **Health endpoints:**
  - `/healthz`: the process is alive and has no storage failure that only a restart clears (D-N-35); use it for liveness. Under Kubernetes a failing `/healthz` restarts the pod.
  - `/readyz`: the six checks of spec 12; use it for readiness. A full disk or an unreachable bucket makes it fail. Under Kubernetes the pod leaves the Service and is not restarted; with one replica the UI is then unreachable through the Service. Free space with `kubectl exec` or a bigger volume, and alert on `llmobs_storage_disk_free_bytes` before it happens. Under Docker the container shows `unhealthy` and keeps running.
- **Metrics:** how to enable `metrics_addr` (TOML, env, compose, Helm), that it has no authentication (bind it privately), and a table of the metrics (copy the 00-design table's name, type, labels and a one-line meaning).
- **The label rule** in one sentence (D-N-29).
- **Five example alert expressions:**
  - `llmobs_storage_failed == 1`;
  - `llmobs_storage_disk_free_bytes < 2 * llmobs_storage_disk_min_free_bytes`;
  - `rate(llmobs_storage_flushes_total{outcome="failed"}[10m]) > 0`;
  - `llmobs_eval_jobs{status="queued"} > 1000`;
  - `sum(rate(llmobs_http_requests_total{status="5xx"}[5m])) / sum(rate(llmobs_http_requests_total[5m])) > 0.05`.

**`backup.md`:** a new section, `## Restore mode (restore drills)`:
- `restore_mode = true` / `LLMOBS_RESTORE_MODE=true` starts a server that makes no outbound call:
  - no alert evaluation or delivery;
  - no judge, webhook-evaluator or backfill run;
  - no email (logged and dropped);
  - no identity provider (no SSO buttons; password login and API keys only);
  - no org-SSO refresh or domain check;
  - no usage notices.
- User-triggered outbound calls answer 503 `restore_mode`.
- Reads, ingestion, deletions, retention, compaction and backups work.
- **How to check it is on:** the startup line `restore mode: outbound work is off …`, `/readyz` returns `"restore_mode": true`, and the metric `llmobs_restore_mode 1`.
- **What it does not do:**
  - it does not make the copy read-only;
  - it does not stop a copy from reaching the live bucket if it is configured to; point it at a copy of the bucket, as the restore procedure says.
- It deletes no object (D-N-34): GC, media collection and project drops remove nothing until the server runs without restore mode, so a copy pointed at the live bucket by mistake can add objects but never delete one. Still point it at a bucket copy. `gc-orphans` still deletes.
- Use an API key when `auth.disable_password = true`, and for members of orgs that enforce SSO: external sign-in is off.
- Link it from the restore procedure's "verify the restore" step: verifying a restore in place uses the same mode.

**`configuration.md`:** the two keys, `restore_mode` and `metrics_addr`, in its table.

**`kubernetes.md` / `docker-compose.md`:** one line each pointing to `monitoring.md` for metrics and probes.

**`s3.md`:** one sentence: "`/readyz` checks the bucket by reading the marker object `_probe/readyz` (written on the first check), so it needs no permission beyond the server's own (D-N-36)." 

**`README.md`:** link `monitoring.md`.

### Spec 12 § Self-monitoring

Replace the "Deferred to just before release (D38). Placeholder decisions: …" paragraph with:

> **Built in N M3 (D-N-28 to D-N-30, D-N-33):**
> - Prometheus `/metrics` on `metrics_addr`, a separate listener, off by default, with no authentication. The metrics are listed in `docs/self-hosting/monitoring.md`. Labels come from fixed sets only, never a tenant, user, key, model or URL (D-N-29).
> - `/readyz` as § Process lifecycle describes it.
> - Restore mode (`restore_mode`, D-N-25) for restore drills: no outbound work.
>
> OTel self-traces stay deferred (D38).

### Decisions

- `docs/mvp-decisions.md` § Track N: D-N-25 to D-N-36 are already there (from the plan's PR). Make them match what was built, and change "(planner)" to "(user)" for each one the coordinator says the user confirmed.
- D38's row: append ` /metrics and /readyz: N M3 (D-N-28–D-N-30).`

### Roadmap

Replace the N row's M3 sentence with: `M3 (restore mode for Q's drills, Prometheus /metrics on metrics_addr, /readyz with object store, free disk and storage flags) implemented.` Update its decision cell to `D-N-1–D-N-36, D180, D182`.

### Requests for Q

If `docs/plans/n-packaging/README.md` lists R11 or R18 as open, mark them answered: R18 by D-N-25 (`restore_mode`, `docs/self-hosting/backup.md` § Restore mode), R11 by D-N-28 (`metrics_addr`).

### CHANGELOG

Under `[Unreleased]`:
- `### Added`:
  - "Restore mode (`restore_mode`): the server serves reads and makes no outbound call, for restore drills."
  - "Prometheus metrics on a separate listener (`metrics_addr`); the Helm chart's `metrics.enabled`."
- `### Changed`: "`/readyz` also checks the object store, free disk and storage failures; a full disk or an unreachable bucket now makes the server not ready."
- `### Upgrade notes`: "`/readyz` can now fail on a full disk or an unreachable object store. Keep liveness probes on `/healthz`; the Helm chart already does."

## Verify

```bash
just helm-check
just check
```

Then run 00-design § How to verify (whole milestone) with a release build, and report each step's output. `just helm-smoke` takes only the image tag, so it cannot enable metrics: skip that, and say so.

## Done when

- [ ] The image, chart and compose changes are in, and `helm-check` covers `metrics.enabled`.
- [ ] `monitoring.md` and the restore-mode section exist, and spec 12, the decisions, the roadmap and the CHANGELOG are updated.
- [ ] The whole-milestone verification ran.
- [ ] `just check` passes. One commit.
